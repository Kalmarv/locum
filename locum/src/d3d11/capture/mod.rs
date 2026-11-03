//! One-frame capture: everything a frame did on the GPU, with the state of every draw and the contents of every
//! resource it used, written to `captures/<name>/capture.db` with the contents in the blob store.
//!
//! Requested by creating the file `capture` in locum's folder; it is removed when seen. Everything is captured by
//! default; its lines can change that:
//! - `label NAME`: the capture's folder name (default `frame-<n>`);
//! - `skip targets`: no target readback at pass ends; `skip contents`: no resource contents;
//! - `max_mb N`: the most memory target readback may use (default 4096);
//! - `skip memory`: no memory dump of the process (see [`super::dump`]).
//!
//! A capture runs over three presents:
//! - **before** (from the present that sees the file): deferred contexts record their work, so command lists
//!   recorded now and executed in the captured frame are complete; every context's writes are noted;
//! - **start** (that frame's present): the resources written during it are copied: their contents when the captured
//!   frame begins;
//! - **recording** (the next frame): every draw, dispatch and resource-changing call on every context, in the order
//!   the GPU runs them (a command list's work where it is executed). A resource the frame writes that was not copied
//!   at the start is copied just before its first write, on that context;
//! - **finish** (the present ending that frame): the frame's other resources (never written by it) are read back or
//!   taken from their upload, copies are read, and the capture is handed to the writer thread.
//!
//! The state of each draw is read from the real context with its getters. Constant buffers are copied by the GPU on
//! the draw's own context, so each draw's constants are the ones the GPU read. A pass ends where the targets in
//! effect change on the GPU's timeline; its targets are read back there (see [`targets`]).

pub mod contents;
pub mod ids;
pub mod ops;
mod snapshot;
mod targets;
mod write;

use core::ffi::c_void;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use locum_capture::db;
use serde_json::Value;
use windows::Win32::Graphics::Direct3D11::*;

use crate::d3d11::registry::Dims;
use crate::log::{folder, log};

use ids::buffer_id;
pub use ids::input_layout_created;

/// True from the request to the end of the captured frame: the only check on the hot paths otherwise.
static ARMED: AtomicBool = AtomicBool::new(false);

pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Before,
    Recording,
}

/// What a capture request asked for.
#[derive(Clone, Debug)]
struct Request {
    label: Option<String>,
    targets: bool,
    contents: bool,
    memory: bool,
    max_target_bytes: u64,
}

impl Request {
    fn parse(text: &str) -> Request {
        let mut r = Request {
            label: None,
            targets: true,
            contents: true,
            memory: true,
            max_target_bytes: 4096 << 20,
        };
        for line in text.lines() {
            let mut words = line.split_whitespace();
            match (words.next(), words.next()) {
                (Some("label"), Some(name)) => {
                    let clean: String = name
                        .chars()
                        .filter(|c| c.is_ascii_alphanumeric() || "-_.".contains(*c))
                        .collect();
                    r.label = (!clean.is_empty()).then_some(clean);
                }
                (Some("skip"), Some("targets")) => r.targets = false,
                (Some("skip"), Some("contents")) => r.contents = false,
                (Some("skip"), Some("memory")) => r.memory = false,
                (Some("max_mb"), Some(n)) => {
                    if let Ok(n) = n.parse::<u64>() {
                        r.max_target_bytes = n << 20;
                    }
                }
                // Older requests asked for targets; they are on by default now.
                (Some("targets"), None) => {}
                (Some(other), _) => log!("capture request: unknown option {other:?} ignored"),
                (None, _) => {}
            }
        }
        r
    }
}

/// The context a call is made on: the real context, locum's number for it, and whether it is deferred.
#[derive(Clone, Copy)]
pub struct Ctx<'a> {
    pub c: &'a ID3D11DeviceContext4,
    pub id: u32,
    pub deferred: bool,
}

/// A constant buffer bound at a draw and where its GPU copy went.
#[derive(Clone)]
pub(super) struct CbCopy {
    /// Its row in the draw's bindings.
    binding: usize,
    source: Option<ID3D11Buffer>,
    first: u32,
    count: u32,
    /// Staging chunk, offset, bytes.
    copy: Option<(u32, u32, u32)>,
}

/// A draw's or dispatch's state.
#[derive(Clone, Default)]
pub(super) struct Draw {
    shaders: [Option<String>; 6],
    bindings: Vec<db::Binding>,
    cb_copies: Vec<CbCopy>,
    viewports: Vec<db::Viewport>,
    scissors: Vec<db::Scissor>,
    topology: i32,
    layout: Option<u64>,
    rasterizer: Option<u64>,
    blend: Option<u64>,
    depth_stencil: Option<u64>,
    blend_factor: Option<[f32; 4]>,
    sample_mask: Option<u32>,
    stencil_ref: Option<u32>,
    predicate: Option<u64>,
    predicate_value: Option<bool>,
    /// The targets as they were after this draw, the last of its pass.
    after: Vec<targets::Target>,
    /// The targets bound at the draw.
    bound: Vec<targets::Bound>,
    /// Whether the pass this draw ends has been dealt with.
    closed: bool,
}

/// One thing a context did, as recorded on it.
#[derive(Clone)]
pub(super) struct Event {
    kind: &'static str,
    call: &'static str,
    context: u32,
    deferred: bool,
    args: Value,
    /// Key of the bytes an update or map wrote.
    data: Option<String>,
    stack: Vec<u64>,
    draw: Option<Box<Draw>>,
    reads: Vec<u64>,
    writes: Vec<u64>,
    /// Resources copied just before this event, on its context, because it writes them first.
    first_writes: Vec<u64>,
    /// The number of the command list execution it came in with (set when spliced).
    list: Option<u64>,
}

/// A finished command list's events, whether it was finished before the captured frame, and how often the captured
/// frame executed it.
struct List {
    events: Vec<Event>,
    before: bool,
    executions: u32,
}

/// A resource the capture uses, held until it ends.
pub(super) struct Tracked {
    resource: ID3D11Resource,
    dims: Dims,
    /// Written while the capture was getting ready: its contents are copied when the frame starts.
    written_before: bool,
    /// Copied on a context just before its first write in the captured frame.
    first_write: Option<contents::Readback>,
    /// Whether an event carries that copy.
    claimed: bool,
}

struct State {
    phase: Phase,
    frame: u64,
    request: Request,
    /// Events recorded on a deferred context since its last FinishCommandList, by context id.
    pending: BTreeMap<u32, Vec<Event>>,
    /// Finished command lists, by address.
    lists: BTreeMap<usize, List>,
    timeline: Vec<Event>,
    lists_spliced: u32,
    lists_before: u32,
    lists_unrecorded: u32,
    /// Constant-buffer staging chunks and the bytes used in the last one.
    staging: Vec<ID3D11Buffer>,
    used: u32,
    copy_failures: u32,
    target_bytes: u64,
    targets_skipped: u32,
    targets_skipped_bytes: u64,
    /// Captures requested so far: the current capture's number.
    captures: u64,
    views: Vec<db::View>,
    states: Vec<db::State>,
    resources: BTreeMap<u64, Tracked>,
    /// Whether the start copies were taken, and of which resources (key and subresource keys of their contents).
    started: bool,
    start: BTreeMap<u64, Result<(String, Vec<String>), String>>,
    gaps: BTreeMap<(String, String), u64>,
}

static STATE: Mutex<State> = Mutex::new(State {
    phase: Phase::Idle,
    frame: 0,
    request: Request {
        label: None,
        targets: false,
        contents: false,
        memory: false,
        max_target_bytes: 0,
    },
    pending: BTreeMap::new(),
    lists: BTreeMap::new(),
    timeline: Vec::new(),
    lists_spliced: 0,
    lists_before: 0,
    lists_unrecorded: 0,
    staging: Vec::new(),
    used: 0,
    copy_failures: 0,
    target_bytes: 0,
    targets_skipped: 0,
    targets_skipped_bytes: 0,
    captures: 0,
    views: Vec::new(),
    states: Vec::new(),
    resources: BTreeMap::new(),
    started: false,
    start: BTreeMap::new(),
    gaps: BTreeMap::new(),
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts something the capture could not record, for the `gaps` table.
pub(super) fn gap(what: &str, detail: impl Into<String>) {
    let mut s = state();
    *s.gaps.entry((what.to_string(), detail.into())).or_insert(0) += 1;
}

impl State {
    /// Whether work on a context is recorded now: deferred contexts from the request on, the immediate context only
    /// in the captured frame.
    fn records(&self, deferred: bool) -> bool {
        match self.phase {
            Phase::Idle => false,
            Phase::Before => deferred,
            Phase::Recording => true,
        }
    }

    fn reset(&mut self) {
        self.pending.clear();
        self.lists.clear();
        self.timeline.clear();
        self.lists_spliced = 0;
        self.lists_before = 0;
        self.lists_unrecorded = 0;
        self.staging.clear();
        self.used = 0;
        self.copy_failures = 0;
        self.target_bytes = 0;
        self.targets_skipped = 0;
        self.targets_skipped_bytes = 0;
        self.views.clear();
        self.states.clear();
        self.resources.clear();
        self.started = false;
        self.start.clear();
        self.gaps.clear();
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
/// immediate context.
pub fn after_present(finished: bool, immediate: &ID3D11DeviceContext4) {
    if finished {
        write::finish(immediate);
        return;
    }
    let phase = state().phase;
    match phase {
        Phase::Idle => {
            let mut s = state();
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
            s.phase = Phase::Before;
            s.captures += 1;
            ARMED.store(true, Ordering::Relaxed);
            log!(
                "capture requested at frame {}: {:?}; recording frame {}",
                s.frame,
                s.request,
                s.frame + 2
            );
        }
        Phase::Before => start(immediate),
        Phase::Recording => {}
    }
}

/// The present before the captured frame: copies what was written while getting ready, then records the frame.
fn start(immediate: &ID3D11DeviceContext4) {
    let (todo, contents) = {
        let mut s = state();
        s.started = true;
        s.phase = Phase::Recording;
        let todo: Vec<(u64, ID3D11Resource, Dims)> = s
            .resources
            .iter()
            .filter(|(_, t)| t.written_before)
            .map(|(&id, t)| (id, t.resource.clone(), t.dims.clone()))
            .collect();
        (todo, s.request.contents)
    };
    let count = todo.len();
    let start = if contents {
        unsafe { contents::read_now(immediate, todo) }
    } else {
        BTreeMap::new()
    };
    let bytes: u64 = start.len() as u64;
    log!("capture: start copies of {count} resources written before the frame ({bytes} read); recording");
    state().start = start;
}

/// Notes what an event writes: while getting ready, that it was written (so it is copied at the start); in the
/// captured frame, a resource written for the first time that was not copied at the start is copied now, on this
/// context, before the caller runs the real call. Returns whether the event is to be recorded.
fn writes(c: &ID3D11DeviceContext4, event: &mut Event) -> bool {
    let (records, phase, started, contents) = {
        let s = state();
        (s.records(event.deferred), s.phase, s.started, s.request.contents)
    };
    if phase == Phase::Idle {
        return false;
    }
    if !started {
        let mut s = state();
        for id in &event.writes {
            if let Some(t) = s.resources.get_mut(id) {
                t.written_before = true;
            }
        }
    }
    if !records || !started || !contents {
        return records;
    }
    let todo: Vec<(u64, ID3D11Resource, Dims)> = {
        let s = state();
        event
            .writes
            .iter()
            .filter(|id| !s.start.contains_key(id))
            .filter_map(|id| {
                let t = s.resources.get(id)?;
                t.first_write
                    .is_none()
                    .then(|| (*id, t.resource.clone(), t.dims.clone()))
            })
            .collect()
    };
    for (id, resource, dims) in todo {
        match unsafe { contents::issue(c, &resource, &dims) } {
            Ok(rb) => {
                if let Some(t) = state().resources.get_mut(&id) {
                    t.first_write = Some(rb);
                }
            }
            Err(why) => gap("contents", format!("not copied before its first write: {why}")),
        }
    }
    // The event that carries a copy is the first write the copy is before (a map's copy is made at the map and
    // carried by its unmap, which is the event recorded).
    if event.kind != "map_begin" {
        let mut s = state();
        for id in &event.writes {
            if let Some(t) = s.resources.get_mut(id)
                && t.first_write.is_some()
                && !t.claimed
            {
                t.claimed = true;
                event.first_writes.push(*id);
            }
        }
    }
    true
}

/// Records an event on a context, while armed (see [`writes`]).
pub(super) fn record(c: &ID3D11DeviceContext4, mut event: Event) {
    if !writes(c, &mut event) {
        return;
    }
    let mut s = state();
    if event.deferred {
        s.pending.entry(event.context).or_default().push(event);
    } else {
        s.timeline.push(event);
    }
}

/// Does what [`record`] does for an event's writes without recording it.
pub(super) fn first_writes_only(c: &ID3D11DeviceContext4, mut event: Event) {
    writes(c, &mut event);
}

fn new_event(kind: &'static str, call: &'static str, context: u32, deferred: bool, args: Value) -> Event {
    Event {
        kind,
        call,
        context,
        deferred,
        args,
        data: None,
        stack: Vec::new(),
        draw: None,
        reads: Vec::new(),
        writes: Vec::new(),
        first_writes: Vec::new(),
        list: None,
    }
}

/// A draw or dispatch on a context, while armed. `context` is the real context it runs on.
pub fn on_draw(x: Ctx, call: &'static str, args: &[i64]) {
    draw_event(x, call, args.iter().map(|&a| Value::from(a)).collect(), &[]);
}

/// An indirect draw or dispatch, while armed: its arguments are the buffer's number and offset, and it reads the
/// buffer.
///
/// # Safety
/// `buffer` must be null or a live `ID3D11Buffer`.
pub unsafe fn on_indirect(x: Ctx, call: &'static str, buffer: *mut c_void, offset: u32) {
    let id = unsafe { buffer_id(buffer) };
    let reads: Vec<u64> = (id != 0).then_some(id).into_iter().collect();
    draw_event(x, call, vec![Value::from(id), Value::from(offset)], &reads);
}

fn draw_event(
    Ctx {
        c: context,
        id,
        deferred,
    }: Ctx,
    call: &'static str,
    args: Vec<Value>,
    extra_reads: &[u64],
) {
    let (records, targets) = {
        let s = state();
        (s.records(deferred), s.request.targets)
    };
    let compute = call.starts_with("Dispatch");
    let mut draw = Box::<Draw>::default();
    let touches = unsafe { snapshot::snapshot(context, compute, &mut draw) };
    if records {
        if targets && !compute {
            draw.bound = unsafe { targets::bound(context) };
        }
        // Constant-buffer copies, recorded on this context right now.
        for cb in &mut draw.cb_copies {
            cb.copy = unsafe { snapshot::copy_cb(context, cb) };
        }
    }
    let kind = if compute { "dispatch" } else { "draw" };
    let mut event = new_event(kind, call, id, deferred, Value::Array(args));
    event.reads = touches.reads;
    event.reads.extend_from_slice(extra_reads);
    event.writes = touches.writes;
    event.draw = Some(draw);
    record(context, event);
}

/// `OMSetRenderTargets` (or `ClearState`, with no targets) on a context, while armed: if it binds other targets than
/// the context's last draw used, that draw ended a pass and its targets are copied now, on this context.
///
/// # Safety
/// `rtvs` must hold null or live render target views, `dsv` null or a live depth stencil view.
pub unsafe fn targets_changing(
    Ctx {
        c: context,
        id,
        deferred,
    }: Ctx,
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
        let last = s.pending.get(&id).and_then(|d| {
            d.iter()
                .rev()
                .find_map(|e| e.draw.as_deref().filter(|_| e.kind == "draw"))
        });
        match last {
            Some(last) if !last.closed && !last.bound.is_empty() && !targets::same(&last.bound, &next) => {
                last.bound.clone()
            }
            _ => return,
        }
    };
    let copied = unsafe { targets::copy(context, &bound) };
    let mut s = state();
    let last = s.pending.get_mut(&id).and_then(|d| {
        d.iter_mut()
            .rev()
            .find_map(|e| e.draw.as_deref_mut().filter(|_| e.kind == "draw"))
    });
    if let Some(last) = last {
        last.after = copied;
        last.closed = true;
    }
}

/// The timeline's last draw, if it is the last event that is a draw.
fn last_timeline_draw(s: &mut State) -> Option<(usize, &mut Draw)> {
    let index = s.timeline.iter().rposition(|e| e.kind == "draw")?;
    let draw = s.timeline[index].draw.as_deref_mut()?;
    Some((index, draw))
}

/// Copies the targets of the timeline's last draw, on the immediate context `immediate`, unless that pass is closed
/// already or `next` (the targets about to be used) are the same ones.
fn close_timeline_pass(immediate: &ID3D11DeviceContext4, next: Option<&[targets::Bound]>) {
    let (index, bound) = {
        let mut s = state();
        if !s.request.targets || s.phase != Phase::Recording {
            return;
        }
        let Some((index, last)) = last_timeline_draw(&mut s) else {
            return;
        };
        if last.closed || last.bound.is_empty() || next.is_some_and(|n| targets::same(&last.bound, n)) {
            return;
        }
        (index, last.bound.clone())
    };
    let copied = unsafe { targets::copy(immediate, &bound) };
    let mut s = state();
    if let Some(d) = s.timeline[index].draw.as_deref_mut() {
        d.after = copied;
        d.closed = true;
    }
}

/// At each present, before [`before_present`]: the frame's last pass ends.
pub fn end_frame(immediate: &ID3D11DeviceContext4) {
    if armed() {
        close_timeline_pass(immediate, None);
    }
}

/// A deferred context's FinishCommandList: its pending events (possibly none) belong to the list.
pub fn on_finish_list(id: u32, list: *mut c_void) {
    let mut s = state();
    let events = s.pending.remove(&id).unwrap_or_default();
    let before = s.phase == Phase::Before;
    if !list.is_null() {
        s.lists.insert(
            list as usize,
            List {
                events,
                before,
                executions: 0,
            },
        );
    }
}

/// ExecuteCommandList on `context`. On the immediate context the list's events join the timeline (after the pass
/// before them is closed if the list starts with other targets); on a deferred context (a list run from inside
/// another) they join that context's pending events. A list can be executed more than once, so its recording is kept
/// until the capture ends.
pub fn on_execute_list(
    Ctx {
        c: context,
        id,
        deferred,
    }: Ctx,
    list: *mut c_void,
) {
    let first = {
        let mut s = state();
        if !s.records(deferred) {
            return;
        }
        match s.lists.get(&(list as usize)) {
            Some(l) => l
                .events
                .iter()
                .find_map(|e| e.draw.as_deref().filter(|_| e.kind == "draw"))
                .map(|d| d.bound.clone()),
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
    let number = u64::from(s.lists_spliced);
    let Some(l) = s.lists.get_mut(&(list as usize)) else {
        return;
    };
    l.executions += 1;
    let (mut events, before) = (l.events.clone(), l.before);
    if deferred {
        s.pending.entry(id).or_default().extend(events);
    } else {
        for e in &mut events {
            e.list = Some(number);
        }
        s.lists_spliced += 1;
        s.lists_before += u32::from(before);
        s.timeline.extend(events);
    }
}

/// The resources the timeline touches, in the order it first touches them, with whether it writes them.
pub(super) fn touched(timeline: &[Event]) -> (BTreeSet<u64>, BTreeSet<u64>) {
    let mut used = BTreeSet::new();
    let mut written = BTreeSet::new();
    for e in timeline {
        used.extend(e.reads.iter().copied());
        used.extend(e.writes.iter().copied());
        written.extend(e.writes.iter().copied());
    }
    (used, written)
}
