//! One-frame capture: every draw and dispatch of a frame with the full state it ran with, and optionally the render
//! targets at the end of each pass.
//!
//! Requested by creating the file `capture` in locum's folder; it is removed when seen. Its lines are options:
//! - `targets`: read back the render and depth targets at the end of each pass;
//! - `max_mb N`: the most memory target readback may use (default 4096).
//!
//! A pass ends where the targets in effect change on the GPU's timeline: an `OMSetRenderTargets` (or `ClearState`)
//! that binds other targets, a command list whose first draw uses other targets than the draw before it, or the end
//! of the frame. Its targets are copied then, kept with its last draw: inside a command list on its deferred context,
//! between command lists on the immediate context.
//!
//! The capture then runs over three presents:
//! - **pre-recording** (from the present that sees the file): deferred contexts record their draws, so command lists
//!   recorded now and executed in the captured frame are complete;
//! - **recording** (the next frame): draws on the immediate context, and command lists as they are executed, form the
//!   frame's timeline;
//! - **finish** (the present ending that frame): the copies are read back and everything is written to
//!   `captures/frame-<n>/` (see [`write`]).
//!
//! The state of each draw is read from the real context with its getters (the runtime tracks it; locum tracks nothing
//! while not capturing). Constant buffers and targets are copied by the GPU on the draw's own context, into staging
//! resources: a deferred context's copies run when its command list runs, so they see exactly what the draw saw.

mod ids;
mod snapshot;
mod targets;
mod write;

use core::ffi::c_void;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use serde::Serialize;
use windows::Win32::Graphics::Direct3D11::*;

use crate::log::{folder, log};

pub use ids::buffer_id;

/// True from pre-recording to the end of the captured frame: the only check on the draw path otherwise.
static ARMED: AtomicBool = AtomicBool::new(false);

pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    PreRecording,
    Recording,
}

/// What a capture request asked for.
#[derive(Clone, Copy)]
struct Request {
    targets: bool,
    max_target_bytes: u64,
}

impl Request {
    fn parse(text: &str) -> Request {
        let mut r = Request {
            targets: false,
            max_target_bytes: 4096 << 20,
        };
        for line in text.lines() {
            let mut words = line.split_whitespace();
            match (words.next(), words.next().and_then(|n| n.parse::<u64>().ok())) {
                (Some("targets"), _) => r.targets = true,
                (Some("max_mb"), Some(n)) => r.max_target_bytes = n << 20,
                (Some(other), _) => log!("capture request: unknown option {other:?} ignored"),
                (None, _) => {}
            }
        }
        r
    }
}

/// A constant buffer bound at a draw, and where its copy went.
#[derive(Clone, Serialize)]
struct CbBind {
    stage: &'static str,
    slot: u32,
    buffer: u64,
    first: u32,
    count: u32,
    /// The hash of the contents the draw saw (filled in when the frame is written).
    content: Option<String>,
    /// The bound buffer, until its copy is recorded.
    #[serde(skip)]
    source: Option<ID3D11Buffer>,
    /// Where the copy went: staging chunk, offset, bytes.
    #[serde(skip)]
    copy: Option<(u32, u32, u32)>,
}

#[derive(Clone, Serialize)]
struct Draw {
    #[serde(rename = "ctx")]
    context: u32,
    deferred: bool,
    call: &'static str,
    args: Vec<i64>,
    /// Stage to shader hash (null for a shader locum did not see created).
    shaders: BTreeMap<&'static str, Option<String>>,
    cbs: Vec<CbBind>,
    /// stage, slot, view id, resource id
    srvs: Vec<(&'static str, u32, u64, u64)>,
    /// slot, view id, resource id
    rtvs: Vec<(u32, u64, u64)>,
    /// view id, resource id
    dsv: Option<(u64, u64)>,
    topology: i32,
    layout: u64,
    /// slot, buffer id, stride, offset
    vbs: Vec<(u32, u64, u32, u32)>,
    /// buffer id, format, offset
    ib: Option<(u64, i32, u32)>,
    raster: u64,
    blend: u64,
    depth: u64,
    /// The targets as they were after this draw, the last of its pass (with `targets` only).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    after: Vec<targets::Target>,
    /// The targets bound at the draw (with `targets` only).
    #[serde(skip)]
    bound: Vec<targets::Bound>,
    /// Whether the pass this draw ends has been dealt with (its targets copied, or tried).
    #[serde(skip)]
    closed: bool,
}

/// A finished command list's draws, and whether it was finished before the captured frame.
struct List {
    draws: Vec<Draw>,
    prerecorded: bool,
}

struct State {
    phase: Phase,
    frame: u64,
    request: Request,
    /// Draws recorded on a deferred context since its last FinishCommandList, by context id.
    pending: BTreeMap<u32, Vec<Draw>>,
    /// Finished command lists, by address.
    lists: BTreeMap<usize, List>,
    timeline: Vec<Draw>,
    lists_spliced: u32,
    /// Command lists spliced in that were finished before the captured frame began.
    lists_prerecorded: u32,
    lists_unrecorded: u32,
    /// Constant-buffer staging chunks and the bytes used in the last one.
    staging: Vec<ID3D11Buffer>,
    used: u32,
    copy_failures: u32,
    target_bytes: u64,
    targets_skipped: u32,
    /// The bytes the skipped targets would have taken.
    targets_skipped_bytes: u64,
    next_id: u64,
    /// Captures requested so far: the current capture's number.
    captures: u64,
    /// JSON lines describing each object the first time the capture records it.
    objects: Vec<String>,
}

static STATE: Mutex<State> = Mutex::new(State {
    phase: Phase::Idle,
    frame: 0,
    request: Request {
        targets: false,
        max_target_bytes: 0,
    },
    pending: BTreeMap::new(),
    lists: BTreeMap::new(),
    timeline: Vec::new(),
    lists_spliced: 0,
    lists_prerecorded: 0,
    lists_unrecorded: 0,
    staging: Vec::new(),
    used: 0,
    copy_failures: 0,
    target_bytes: 0,
    targets_skipped: 0,
    targets_skipped_bytes: 0,
    next_id: 1,
    captures: 0,
    objects: Vec::new(),
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

impl State {
    /// Whether work on a context is recorded now: deferred contexts from pre-recording on, the immediate context only
    /// in the captured frame.
    fn records(&self, deferred: bool) -> bool {
        match self.phase {
            Phase::Idle => false,
            Phase::PreRecording => deferred,
            Phase::Recording => true,
        }
    }

    /// Clears everything one capture gathered (ids stay: they are on the objects).
    fn reset(&mut self) {
        self.pending.clear();
        self.lists.clear();
        self.timeline.clear();
        self.lists_spliced = 0;
        self.lists_prerecorded = 0;
        self.lists_unrecorded = 0;
        self.staging.clear();
        self.used = 0;
        self.copy_failures = 0;
        self.target_bytes = 0;
        self.targets_skipped = 0;
        self.targets_skipped_bytes = 0;
        self.objects.clear();
    }
}

/// Ids for contexts, handed out as they are wrapped.
pub fn next_context_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// At each present, before the real one (and after [`end_frame`]). Ends a frame being recorded.
pub fn before_present() -> bool {
    let mut s = state();
    s.frame += 1;
    if s.phase == Phase::Recording {
        ARMED.store(false, Ordering::Relaxed);
        s.phase = Phase::Idle;
        return true;
    }
    false
}

/// At each present, after the real one. `finished` is what [`before_present`] returned; `immediate` is the real
/// immediate context (for reading the copies back).
pub fn after_present(finished: bool, immediate: &ID3D11DeviceContext4) {
    if finished {
        write::finish(immediate);
        return;
    }
    let mut s = state();
    match s.phase {
        Phase::Idle => {
            // Look for a request every 30 frames.
            if !s.frame.is_multiple_of(30) {
                return;
            }
            let Some(path) = folder().map(|d| d.join("capture")) else {
                return;
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                return;
            };
            if std::fs::remove_file(&path).is_err() {
                return;
            }
            s.reset();
            s.request = Request::parse(&text);
            s.phase = Phase::PreRecording;
            s.captures += 1;
            ARMED.store(true, Ordering::Relaxed);
            log!(
                "capture requested at frame {} (targets {}, up to {} MiB): pre-recording",
                s.frame,
                if s.request.targets { "on" } else { "off" },
                s.request.max_target_bytes >> 20
            );
        }
        Phase::PreRecording => {
            log!("capture: recording frame {}", s.frame + 1);
            s.phase = Phase::Recording;
        }
        Phase::Recording => {}
    }
}

/// A draw or dispatch on a context, while armed. `context` is the real context it runs on.
pub fn on_draw(context: &ID3D11DeviceContext4, id: u32, deferred: bool, call: &'static str, args: &[i64]) {
    let targets = {
        let s = state();
        if !s.records(deferred) {
            return;
        }
        s.request.targets
    };
    let compute = call.starts_with("Dispatch");
    let mut draw = Draw {
        context: id,
        deferred,
        call,
        args: args.to_vec(),
        shaders: BTreeMap::new(),
        cbs: Vec::new(),
        srvs: Vec::new(),
        rtvs: Vec::new(),
        dsv: None,
        topology: 0,
        layout: 0,
        vbs: Vec::new(),
        ib: None,
        raster: 0,
        blend: 0,
        depth: 0,
        after: Vec::new(),
        bound: Vec::new(),
        closed: false,
    };
    unsafe { snapshot::snapshot(context, compute, &mut draw) };
    if targets && !compute {
        draw.bound = unsafe { targets::bound(context) };
    }
    // Constant-buffer copies, recorded on this context right now.
    for cb in &mut draw.cbs {
        cb.copy = unsafe { snapshot::copy_cb(context, cb) };
    }
    let mut s = state();
    if deferred {
        s.pending.entry(id).or_default().push(draw);
    } else {
        s.timeline.push(draw);
    }
}

/// `OMSetRenderTargets` (or `ClearState`, with no targets) on a context, while armed: if it binds other targets than
/// the context's last draw used, that draw ended a pass and its targets are copied now, on this context.
///
/// # Safety
/// `rtvs` must hold null or live render target views, `dsv` null or a live depth stencil view.
pub unsafe fn targets_changing(
    context: &ID3D11DeviceContext4,
    id: u32,
    deferred: bool,
    rtvs: &[*mut c_void],
    dsv: *mut c_void,
) {
    if !state().request.targets {
        return;
    }
    let next = unsafe { targets::from_raw(rtvs, dsv) };
    if !deferred {
        close_timeline_pass(context, Some(&next));
        return;
    }
    let bound = {
        let s = state();
        if !s.records(true) {
            return;
        }
        match s.pending.get(&id).and_then(|d| d.last()) {
            Some(last) if !last.closed && !last.bound.is_empty() && !targets::same(&last.bound, &next) => {
                last.bound.clone()
            }
            _ => return,
        }
    };
    let copied = unsafe { targets::copy(context, &bound) };
    if let Some(last) = state().pending.get_mut(&id).and_then(|d| d.last_mut()) {
        last.after = copied;
        last.closed = true;
    }
}

/// Copies the targets of the timeline's last draw, on the immediate context `immediate`, unless that pass is closed
/// already or `next` (the targets about to be used) are the same ones.
fn close_timeline_pass(immediate: &ID3D11DeviceContext4, next: Option<&[targets::Bound]>) {
    let (index, bound) = {
        let s = state();
        if !s.request.targets || s.phase != Phase::Recording {
            return;
        }
        let Some(last) = s.timeline.last() else { return };
        if last.closed || last.bound.is_empty() || next.is_some_and(|n| targets::same(&last.bound, n)) {
            return;
        }
        (s.timeline.len() - 1, last.bound.clone())
    };
    let copied = unsafe { targets::copy(immediate, &bound) };
    let mut s = state();
    s.timeline[index].after = copied;
    s.timeline[index].closed = true;
}

/// At each present, before [`before_present`]: the frame's last pass ends.
pub fn end_frame(immediate: &ID3D11DeviceContext4) {
    if armed() {
        close_timeline_pass(immediate, None);
    }
}

/// A deferred context's FinishCommandList: its pending draws (possibly none) belong to the list.
pub fn on_finish_list(id: u32, list: *mut c_void) {
    let mut s = state();
    let draws = s.pending.remove(&id).unwrap_or_default();
    let prerecorded = s.phase == Phase::PreRecording;
    if !list.is_null() {
        s.lists.insert(list as usize, List { draws, prerecorded });
    }
}

/// ExecuteCommandList on `context`. On the immediate context the list's draws join the timeline (after the pass
/// before them is closed if the list starts with other targets); on a deferred context (a list run from inside
/// another) they join that context's pending draws. A list can be executed more than once, so its recording is kept
/// until the capture ends.
pub fn on_execute_list(context: &ID3D11DeviceContext4, id: u32, deferred: bool, list: *mut c_void) {
    let first = {
        let mut s = state();
        if !s.records(deferred) {
            return;
        }
        match s.lists.get(&(list as usize)) {
            Some(l) => l.draws.first().map(|d| d.bound.clone()),
            None => {
                if !deferred {
                    s.lists_unrecorded += 1;
                }
                return;
            }
        }
    };
    if !deferred && let Some(first) = first {
        close_timeline_pass(context, Some(&first));
    }
    let mut s = state();
    let Some(l) = s.lists.get(&(list as usize)) else {
        return;
    };
    let (draws, prerecorded) = (l.draws.clone(), l.prerecorded);
    if deferred {
        s.pending.entry(id).or_default().extend(draws);
    } else {
        s.lists_spliced += 1;
        s.lists_prerecorded += u32::from(prerecorded);
        s.timeline.extend(draws);
    }
}
