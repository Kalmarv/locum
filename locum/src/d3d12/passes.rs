//! Render and depth targets copied where each pass ends, inside the application's own command lists: the copy runs
//! right after the pass's last draw on the GPU, before anything later in the list can overwrite the targets.
//!
//! Lists reset or created while a capture is armed are instrumented. A pass ends when its targets are rebound or
//! cleared from the state, a barrier moves a bound target that was drawn to out of its state, the list closes, or a
//! native render pass ends. A barrier leaves the target bound: games bind a target and only then move it into
//! RENDER_TARGET or DEPTH_WRITE (Elden Ring's shadow and G-buffer passes), and draw to it again after moving it back. Its targets are copied just before the command that ends it (after EndRenderPass itself, its ending
//! access done): a transition from the state the target is in to COPY_SOURCE, a copy of the view's subresource into
//! readback memory, the transition back (through COPY_DEST to a state that allows no write, so that the list's later
//! writes wait for the copy: see [`barriers`]). The application's own barrier names that state; otherwise a bound target
//! is in RENDER_TARGET or DEPTH_WRITE (the layouts of those names for resources made in a layout), or what this
//! list's own barriers left it in. Predication is suspended around the copies.
//!
//! The readback memory lives as long as the GPU may run the list: until the list is reset or destroyed and a fence
//! locum signals after its last execution has completed.
//!
//! A pass's copy waits for the next command that names its target (binds, clears, moves, copies or resolves it):
//! until one does, nothing in the list can change what the target holds, and the copy, made then from the state
//! the target is in, holds what the pass left. A target moved into a state that allows unordered-access writes, or
//! made for simultaneous access, is copied where its pass ends.
//!
//! A pass still open, or whose copy still waits, when its list closes is decided where the list is submitted. Games split one pass over several
//! lists submitted together (Elden Ring's G-buffer goes over 13 to 15, each list moving it into RENDER_TARGET and
//! back): if the next list of the same submission that draws to the target or writes it otherwise draws to it, with
//! the same views bound, before writing it any other way, the pass continues there and nothing is copied. With other
//! views bound it is another pass by the capture's own reckoning (runs of draws with the same targets).
//! Otherwise locum records a list of its own that copies the target, and submits it right after the closed list in
//! the same ExecuteCommandLists call. Its memory is freed once the copy is read, not held for the list's life.
//!
//! What a draw, dispatch or UAV clear writes through UAVs (see [`super::outputs`]) is copied the same way right after
//! it, before the list's next command: each resource from the state or layout it was written in (UNORDERED_ACCESS,
//! or what this list's own barriers left it in) to COPY_SOURCE and back. A buffer is copied whole, a texture the
//! subresources its view covers.
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use locum_capture::d3d12 as model;
use locum_capture::layout::Sub;
use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

use super::states::{Access, COPY_DEST, COPY_SOURCE};

/// Whether lists reset now are instrumented: from the Present that arms a capture to the one that closes it.
pub static ARMED: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicU64 = AtomicU64::new(1);

/// Checkpoints recorded since the process started: the log compares it at arming and closing.
pub fn recorded() -> u64 {
    NEXT.load(Ordering::Relaxed) - 1
}
/// Readback memory the live copies hold, and the most they may.
static BYTES: AtomicU64 = AtomicU64::new(0);
const BUDGET: u64 = 3 << 30;
/// What outputs may hold of it: the rest is kept for pass-end targets.
const OUTPUT_BUDGET: u64 = 2 << 30;
/// The most one output's copy may be.
const OUTPUT_BYTES: u64 = 64 << 20;
const RENDER_TARGET: i32 = 0x4;
const DEPTH_WRITE: i32 = 0x10;
const ALL_SUBRESOURCES: u64 = 0xFFFF_FFFF;
/// The legacy states that allow a write: RENDER_TARGET, UNORDERED_ACCESS, DEPTH_WRITE, STREAM_OUT, COPY_DEST,
/// RESOLVE_DEST and the video writes.
const WRITES: i32 = 0x4 | 0x8 | 0x10 | 0x100 | COPY_DEST | 0x1000 | 0x20000 | 0x80000 | 0x800000;

/// The readback memory of one target's copy; dropping the last reference gives its bytes back to the budget.
struct Readback {
    readback: ID3D12Resource,
    footprints: Vec<(D3D12_PLACED_SUBRESOURCE_FOOTPRINT, u32, u64)>,
    layout: Vec<Sub>,
    joined: Option<u32>,
    bytes: u64,
}
// Free-threaded D3D12 objects.
unsafe impl Send for Readback {}
unsafe impl Sync for Readback {}

impl Drop for Readback {
    fn drop(&mut self) {
        BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl Readback {
    /// The target's bytes in canonical layout, once the GPU has made the copy.
    unsafe fn read(&self) -> Result<Vec<u8>, String> {
        let mut p = core::ptr::null_mut();
        let whole = D3D12_RANGE {
            Begin: 0,
            End: self.bytes as usize,
        };
        unsafe { self.readback.Map(0, Some(&whole), Some(&mut p)) }
            .map_err(|e| format!("checkpoint Map: {e}"))?;
        let out = unsafe {
            super::readback::canonical(&self.layout, self.joined, &self.footprints, p.cast(), self.bytes)
        };
        unsafe { self.readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 })) };
        out
    }
}

/// One target's copy inside an application list, alive while the GPU may still write it.
struct Copy {
    data: Arc<Readback>,
    list: u64,
    /// The fence value locum signalled after the list's latest execution.
    last: Option<(ID3D12Fence, u64)>,
    /// The list was reset or destroyed: no later execution can come.
    retired: bool,
}
// Free-threaded D3D12 objects.
unsafe impl Send for Copy {}

/// A pass its list's Close left open: the target, and the state each plane is in where the list ends.
struct Pending {
    list: u64,
    device: ID3D12Device,
    target: Bound,
    states: Vec<Access>,
    /// The views bound with it in its pass: a later list continues the pass only with the same.
    set: Vec<u64>,
}
// Free-threaded D3D12 objects.
unsafe impl Send for Pending {}

/// The pending ends of closed lists not yet submitted, by checkpoint.
fn pending() -> MutexGuard<'static, HashMap<u64, Pending>> {
    static PENDING: OnceLock<Mutex<HashMap<u64, Pending>>> = OnceLock::new();
    PENDING
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn copies() -> MutexGuard<'static, HashMap<u64, Copy>> {
    static COPIES: OnceLock<Mutex<HashMap<u64, Copy>>> = OnceLock::new();
    COPIES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Resources made in an enhanced-barrier layout, whose bound targets are in a layout rather than a state.
fn layouts() -> MutexGuard<'static, HashSet<u64>> {
    static LAYOUTS: OnceLock<Mutex<HashSet<u64>>> = OnceLock::new();
    LAYOUTS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn created_in_layout(resource: u64) {
    layouts().insert(resource);
}

/// A list's copies are retired when it is reset or destroyed; resources are forgotten when destroyed.
pub fn destroyed(id: u64) {
    retire(id);
    layouts().remove(&id);
}

pub fn retire(list: u64) {
    for c in copies().values_mut().filter(|c| c.list == list) {
        c.retired = true;
    }
    pending().retain(|_, p| p.list != list);
}

/// Frees the copies of retired lists whose last execution has completed.
pub fn sweep() {
    copies().retain(|_, c| {
        !(c.retired
            && c.last
                .as_ref()
                .is_none_or(|(f, v)| unsafe { f.GetCompletedValue() } >= *v))
    });
}

/// A submission's executions of instrumented lists: every checkpoint they copy into, those whose bytes the armed
/// capture reads, and the lists locum adds after closed lists to copy the passes they left open.
#[derive(Default)]
pub struct Submitted {
    pub all: Vec<u64>,
    pub read: Vec<u64>,
    pub results: Option<super::readback::Results>,
    pub submission: u64,
    /// Locum's copy lists, each with the position in the application's array of the list it follows.
    inserted: Vec<(usize, ID3D12GraphicsCommandList)>,
    allocator: Option<ID3D12CommandAllocator>,
    /// The copies those lists make, and the pending ends they could not copy, by checkpoint.
    reads: Vec<(u64, Arc<Readback>)>,
    gaps: Vec<(u64, String)>,
    /// The array submitted when locum's lists are added to the application's.
    array: Vec<*mut c_void>,
}

impl Submitted {
    pub fn new(submission: u64) -> Submitted {
        Submitted {
            submission,
            ..Default::default()
        }
    }

    /// The lists to submit for the application's `count` lists at `lists`: its own, with locum's copy lists after
    /// the lists they follow. The array lives as long as this submission.
    ///
    /// # Safety
    /// `lists` points at `count` list pointers.
    pub unsafe fn arrange(&mut self, count: u32, lists: *const *mut c_void) -> (u32, *const *mut c_void) {
        if self.inserted.is_empty() || lists.is_null() {
            return (count, lists);
        }
        let mut inserted = self.inserted.iter().peekable();
        for (i, list) in unsafe { core::slice::from_raw_parts(lists, count as usize) }
            .iter()
            .enumerate()
        {
            self.array.push(*list);
            while let Some((_, l)) = inserted.next_if(|(at, _)| *at == i) {
                self.array.push(l.as_raw());
            }
        }
        (self.array.len() as u32, self.array.as_ptr())
    }

    /// Whether the capture has anything to read from this submission's copies.
    pub fn reads(&self) -> bool {
        !self.read.is_empty() || !self.reads.is_empty() || !self.gaps.is_empty()
    }
}

/// One list of a submission, at `at` in the application's array, and whether this is its generation's first
/// execution while the capture is armed.
pub struct Executed<'a> {
    pub at: usize,
    pub list: &'a model::List,
    pub first: bool,
}

/// Decides the pending ends of a submission's lists (see the module's documentation), recording locum's copy lists
/// into `s`. Returns the checkpoints whose pass the next list continues. Ends of lists not executed for the first
/// time while armed are dropped: their copies would not be read.
///
/// # Safety
/// Called from ExecuteCommandLists on the application's direct queue, before the lists are submitted.
pub unsafe fn ends(s: &mut Submitted, lists: &[Executed]) -> Vec<u64> {
    let mut continued = Vec::new();
    for (i, e) in lists.iter().enumerate() {
        let ends: Vec<(u64, Pending)> = {
            let mut p = pending();
            e.list
                .checkpoints
                .iter()
                .filter_map(|c| p.remove(&c.id).map(|end| (c.id, end)))
                .collect()
        };
        if !e.first {
            continue;
        }
        let mut list: Option<ID3D12GraphicsCommandList> = None;
        let mut reads = Vec::new();
        for (id, end) in ends {
            let key = (end.target.resource, end.target.subs[0]);
            // The first later list that draws to the target or writes it otherwise.
            let next = lists[i + 1..]
                .iter()
                .find(|n| n.list.continues.iter().any(|(k, _)| *k == key) || n.list.written.contains(&key.0));
            let continues = |n: &&Executed| {
                n.list
                    .continues
                    .iter()
                    .any(|(k, set)| *k == key && *set == end.set)
            };
            if next.is_some_and(|n| n.first && continues(&n)) {
                continued.push(id);
                continue;
            }
            let recorded = unsafe { s.recorder(&end.device, &mut list) }
                .and_then(|l| unsafe { copy_into(&end.device, l, &end.target, &end.states, None) });
            match recorded {
                Ok(readback) => reads.push((id, Arc::new(readback))),
                Err(why) => s.gaps.push((id, why)),
            }
        }
        let Some(list) = list else {
            continue;
        };
        match unsafe { list.Close() } {
            Ok(()) if !reads.is_empty() => {
                s.inserted.push((e.at, list));
                s.reads.extend(reads);
            }
            Ok(()) => {}
            Err(why) => s.gaps.extend(
                reads
                    .into_iter()
                    .map(|(id, _)| (id, format!("copy list Close: {why}"))),
            ),
        }
    }
    continued
}

impl Submitted {
    /// A new direct list of locum's for copies after one application list, on the submission's own allocator.
    unsafe fn recorder<'a>(
        &mut self,
        device: &ID3D12Device,
        list: &'a mut Option<ID3D12GraphicsCommandList>,
    ) -> Result<&'a ID3D12GraphicsCommandList, String> {
        if list.is_none() {
            if self.allocator.is_none() {
                self.allocator = Some(
                    unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
                        .map_err(|e| format!("copy allocator: {e}"))?,
                );
            }
            let allocator = self.allocator.as_ref().expect("made above");
            *list = Some(
                unsafe { device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, allocator, None) }
                    .map_err(|e| format!("copy list: {e}"))?,
            );
        }
        Ok(list.as_ref().expect("made above"))
    }
}

/// After the application's lists were submitted to the real queue `queue`: locum's fence marks their completion,
/// and the copies the capture reads are read once it does.
///
/// # Safety
/// `queue` is the real queue the lists were executed on.
pub unsafe fn submitted(queue_id: u64, queue: *mut c_void, s: Submitted) {
    if s.all.is_empty() && s.inserted.is_empty() {
        return;
    }
    static FENCES: OnceLock<Mutex<HashMap<u64, (ID3D12Fence, u64)>>> = OnceLock::new();
    let Some(queue) = (unsafe { ID3D12CommandQueue::from_raw_borrowed(&queue) }) else {
        return;
    };
    let mut fences = FENCES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (fence, value) = match fences.entry(queue_id) {
        std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
        std::collections::hash_map::Entry::Vacant(e) => {
            let mut device: Option<ID3D12Device> = None;
            let fence = unsafe { queue.GetDevice(&mut device) }
                .ok()
                .and_then(|()| unsafe { device?.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) }.ok());
            let Some(fence) = fence else {
                crate::log::log!("D3D12 checkpoints: no fence for queue {queue_id}; their copies are kept");
                // The GPU may still run locum's copy lists.
                std::mem::forget(s);
                return;
            };
            e.insert((fence, 0))
        }
    };
    *value += 1;
    let (fence, value) = (fence.clone(), *value);
    drop(fences);
    if unsafe { queue.Signal(&fence, value) }.is_err() {
        crate::log::log!("D3D12 checkpoints: Signal failed on queue {queue_id}; their copies are kept");
        std::mem::forget(s);
        return;
    }
    let Submitted {
        all,
        read: wanted,
        results,
        submission,
        inserted,
        allocator,
        reads,
        gaps,
        ..
    } = s;
    let mut read = Vec::new();
    {
        let mut c = copies();
        for id in &all {
            if let Some(copy) = c.get_mut(id) {
                copy.last = Some((fence.clone(), value));
            }
        }
        for id in &wanted {
            if let Some(copy) = c.get(id) {
                read.push((*id, copy.data.clone()));
            }
        }
    }
    read.extend(reads);
    let idle = inserted.is_empty();
    // Locum's lists and allocator live until the GPU has run them.
    let lists = Lists {
        _lists: inserted,
        _allocator: allocator,
    };
    let Some(results) = results.filter(|_| !read.is_empty() || !gaps.is_empty()) else {
        if !idle {
            super::readback::collector(Box::new(move || {
                let _ = unsafe {
                    fence.SetEventOnCompletion(value, windows::Win32::Foundation::HANDLE::default())
                };
                drop(lists);
            }));
        }
        return;
    };
    super::readback::collector(Box::new(move || {
        let wait =
            unsafe { fence.SetEventOnCompletion(value, windows::Win32::Foundation::HANDLE::default()) };
        drop(lists);
        let checkpoints = read
            .into_iter()
            .map(|(id, readback)| {
                let content = wait
                    .as_ref()
                    .map_err(|e| format!("checkpoint wait: {e}"))
                    .and_then(|()| unsafe { readback.read() })
                    .map(|data| {
                        let key = locum_capture::hash::key(&data);
                        locum_capture::writer::blob(
                            &crate::settings::settings().blobs,
                            key.clone(),
                            data,
                            true,
                        );
                        key
                    });
                (id, content)
            })
            .chain(gaps.into_iter().map(|(id, why)| (id, Err(why))))
            .collect();
        results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(super::readback::Batch {
                submission,
                checkpoints,
                ..Default::default()
            });
    }));
}

/// Locum's copy lists of a submission and their allocator, moved to the collector.
struct Lists {
    _lists: Vec<(usize, ID3D12GraphicsCommandList)>,
    _allocator: Option<ID3D12CommandAllocator>,
}
// Free-threaded D3D12 objects, dropped once the GPU is done with them.
unsafe impl Send for Lists {}

/// A target bound in a pass.
#[derive(Clone)]
struct Bound {
    /// `rt0` to `rt7`, or `depth`.
    target: String,
    view: u64,
    resource: u64,
    real: ID3D12Resource,
    /// The view's subresource in each plane the resource has: two for a depth-stencil format.
    subs: Vec<u32>,
    z: u32,
    format: u32,
    width: u32,
    height: u32,
    depth: bool,
    /// The planes the view only reads: a depth view's READ_ONLY_DEPTH and READ_ONLY_STENCIL flags.
    read_only: [bool; 2],
    /// A render pass attachment its ending access discards.
    discard: bool,
    /// A draw may have written it since it was bound or last copied.
    drawn: bool,
    /// Made with ALLOW_SIMULTANEOUS_ACCESS: written through views in COMMON with no barrier, so no copy can wait.
    simultaneous: bool,
}

impl Bound {
    /// Whether the view writes nothing: no pass of it to copy.
    fn reads_only(&self) -> bool {
        self.read_only[..self.subs.len()].iter().all(|r| *r)
    }
}

/// Subresources a barrier names, in the resource's indexing.
#[derive(Clone)]
enum Range {
    All,
    One(u32),
    /// An enhanced barrier's mips, slices and planes.
    Span {
        mips: (u32, u32),
        slices: (u32, u32),
        planes: (u32, u32),
    },
}

impl Range {
    fn legacy(sub: u64) -> Range {
        if sub == ALL_SUBRESOURCES {
            Range::All
        } else {
            Range::One(sub as u32)
        }
    }

    fn enhanced(r: &Value) -> Range {
        let n = |key: &str| r[key].as_u64().unwrap_or(0) as u32;
        let first = r["index_or_first_mip"].as_u64().unwrap_or(0);
        if n("mips") == 0 {
            return Range::legacy(first);
        }
        let span = |first: u32, count: u32| (first, first + count.max(1));
        Range::Span {
            mips: span(first as u32, n("mips")),
            slices: span(n("first_slice"), n("slices")),
            planes: span(n("first_plane"), n("planes")),
        }
    }

    /// Whether it includes subresource `sub` of a resource with `mips` mips and `slices` array slices.
    fn holds(&self, sub: u32, (mips, slices): (u32, u32)) -> bool {
        match *self {
            Range::All => true,
            Range::One(s) => s == sub,
            Range::Span {
                mips: m,
                slices: s,
                planes: p,
            } => {
                let (mip, slice, plane) = (sub % mips, sub / mips % slices, sub / (mips * slices));
                (m.0..m.1).contains(&mip) && (s.0..s.1).contains(&slice) && (p.0..p.1).contains(&plane)
            }
        }
    }
}

/// A pass that has ended, its copy waiting for the next command that names the target: until one does, nothing in
/// the list can change the target's contents.
struct Ended {
    target: Bound,
    /// The command before which the pass ended.
    at: usize,
    /// The views bound in the pass.
    set: Vec<u64>,
}

/// What an instrumented list generation knows while it is recorded.
pub struct Pass {
    device: ID3D12Device,
    targets: Vec<Bound>,
    /// EndRenderPass was recorded: its targets' pass ends before the next command. A suspending render pass resumes
    /// in another list, and nothing may come between: its targets are not copied.
    ended: Option<bool>,
    /// The render pass being recorded suspends.
    suspending: bool,
    /// The list's predication: buffer, offset, operation.
    predicate: Option<(u64, u64, i32)>,
    /// The states and layouts this list's own barriers left resources in, by resource, latest last.
    known: HashMap<u64, Vec<(Range, Access)>>,
    /// Passes ended whose copies wait for a command naming their targets, or for Close.
    waiting: Vec<Ended>,
    /// The targets (resource, first plane's subresource) drawn to before the list wrote them any other way, with the
    /// views bound at the first such draw: a pass an earlier list left waiting with the same views continues here.
    continues: HashMap<(u64, u32), Vec<u64>>,
    /// The resources the list writes other than by drawing to them (see [`writes`]).
    written: HashSet<u64>,
    /// What the list has bound, which names what its draws and dispatches write through UAVs.
    bound: super::outputs::Bound,
    /// What the command just recorded wrote through UAVs, or why some cannot be named, copied before the next: by
    /// the index of the command.
    due: Vec<(usize, Result<super::outputs::Written, String>)>,
    /// The list has recorded enhanced barriers: its buffers are moved by them too.
    enhanced: bool,
    /// The outputs copied, taken at each command (see [`Pass::outputs`]).
    outputs: Vec<model::Output>,
}
// Free-threaded D3D12 objects; a list's pass is used by whoever records the list, one at a time.
unsafe impl Send for Pass {}

const DEPTH_READ: i32 = 0x20;
const UNORDERED_ACCESS: i32 = 0x8;
const LAYOUT_COMMON: i32 = 0;
const LAYOUT_GENERIC_READ: i32 = 1;
const LAYOUT_RENDER_TARGET: i32 = 2;
const LAYOUT_UNORDERED_ACCESS: i32 = 3;
const LAYOUT_DEPTH_WRITE: i32 = 4;
const LAYOUT_DEPTH_READ: i32 = 5;
const LAYOUT_SHADER_RESOURCE: i32 = 6;
const LAYOUT_COPY_SOURCE: i32 = 7;

/// Whether a target a barrier moved into `access` can be written only by a command that names it (a barrier, a
/// clear, a copy, a binding as a target), so its copy can wait for one: not a state or layout that allows
/// unordered-access writes through a view.
fn waits_in(access: Access) -> bool {
    match access {
        Access::State(s) => s & UNORDERED_ACCESS == 0,
        Access::Layout(l) => matches!(
            l,
            LAYOUT_GENERIC_READ
                | LAYOUT_RENDER_TARGET
                | LAYOUT_DEPTH_WRITE
                | LAYOUT_DEPTH_READ
                | LAYOUT_SHADER_RESOURCE
                | LAYOUT_COPY_SOURCE
        ),
    }
}

impl Pass {
    /// A pass state for the real graphics list `real` of the direct type, opened with pipeline `pso`, or None for
    /// other lists.
    ///
    /// # Safety
    /// `real` is the application's real command list.
    pub unsafe fn new(real: *mut c_void, pso: u64) -> Option<Pass> {
        let list = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&real) }?;
        if unsafe { list.GetType() } != D3D12_COMMAND_LIST_TYPE_DIRECT {
            return None;
        }
        let mut device: Option<ID3D12Device> = None;
        unsafe { list.GetDevice(&mut device) }.ok()?;
        Some(Pass {
            device: device?,
            targets: Vec::new(),
            ended: None,
            suspending: false,
            predicate: None,
            known: HashMap::new(),
            waiting: Vec::new(),
            continues: HashMap::new(),
            written: HashSet::new(),
            bound: super::outputs::Bound::reset(pso),
            due: Vec::new(),
            enhanced: false,
            outputs: Vec::new(),
        })
    }

    /// The outputs copied since last asked.
    pub fn outputs(&mut self) -> Vec<model::Output> {
        std::mem::take(&mut self.outputs)
    }

    /// What the list's continuing draws and other writes were, for the submissions that decide pending ends.
    pub fn touches(&mut self) -> (model::Continues, Vec<u64>) {
        (
            std::mem::take(&mut self.continues).into_iter().collect(),
            std::mem::take(&mut self.written).into_iter().collect(),
        )
    }

    /// Called before the command `call` with recorded arguments `a` is forwarded to the real list, `at` commands
    /// into it: the copies of ended passes whose targets it names are recorded first, from the states they are in.
    ///
    /// # Safety
    /// `real` is the application's real list this pass instruments.
    pub unsafe fn before(
        &mut self,
        real: *mut c_void,
        call: &str,
        a: &Value,
        at: usize,
    ) -> Vec<model::Checkpoint> {
        let mut out = Vec::new();
        let Some(list) = (unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&real) }) else {
            return out;
        };
        unsafe { self.copy_outputs(list, at) };
        self.bound.apply(call, a);
        let (written, gaps) = self.bound.written(call, a);
        self.due.extend(written.into_iter().map(|w| (at, Ok(w))));
        self.due.extend(gaps.into_iter().map(|why| (at, Err(why))));
        self.enhanced |= call == "Barrier";
        if let Some(suspended) = self.ended.take() {
            unsafe { self.end_pass(list, at, suspended, &mut out) };
        }
        let named = mentions(call, a);
        if !named.is_empty() {
            unsafe { self.flush(list, |r| named.contains(&r), &mut out) };
        }
        self.written.extend(writes(call, a));
        match call {
            "OMSetRenderTargets" => {
                let mut targets: Vec<Bound> = a[0]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter_map(|(i, v)| bound(v, format!("rt{i}")))
                    .collect();
                targets.extend(bound(&a[1], "depth".into()));
                let same = targets.len() == self.targets.len()
                    && targets
                        .iter()
                        .zip(&self.targets)
                        .all(|(x, y)| x.target == y.target && x.view == y.view);
                if !same {
                    unsafe { self.end_pass(list, at, false, &mut out) };
                    // A target bound again is drawn to again: what it held is copied first.
                    unsafe { self.flush(list, |r| targets.iter().any(|t| t.resource == r), &mut out) };
                    self.targets = targets;
                }
            }
            "BeginRenderPass" => {
                unsafe { self.end_pass(list, at, false, &mut out) };
                let mut targets: Vec<Bound> = a[0]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .filter_map(|(i, t)| attachment(&t["view"], format!("rt{i}"), [&t["end"]; 2]))
                    .collect();
                let depth = &a[1];
                targets.extend(attachment(
                    &depth["view"],
                    "depth".into(),
                    [&depth["depth_end"], &depth["stencil_end"]],
                ));
                unsafe { self.flush(list, |r| targets.iter().any(|t| t.resource == r), &mut out) };
                self.targets = targets;
                // D3D12_RENDER_PASS_FLAG_SUSPENDING_PASS.
                self.suspending = a[2].as_u64().unwrap_or(0) & 2 != 0;
            }
            "EndRenderPass" => self.ended = Some(std::mem::take(&mut self.suspending)),
            "ClearState" => {
                unsafe { self.end_pass(list, at, false, &mut out) };
                self.predicate = None;
            }
            "DrawInstanced"
            | "DrawIndexedInstanced"
            | "DispatchMesh"
            | "ExecuteIndirect"
            | "ExecuteBundle" => {
                let set = views(&self.targets);
                for t in &mut self.targets {
                    t.drawn = true;
                    if !self.written.contains(&t.resource) {
                        self.continues
                            .entry((t.resource, t.subs[0]))
                            .or_insert_with(|| set.clone());
                    }
                }
            }
            // What a clear or discard overwrites ends its pass first.
            "ClearRenderTargetView" | "ClearDepthStencilView" => {
                if let Some(c) = bound(&a[0], String::new()) {
                    unsafe {
                        self.overwritten(
                            list,
                            at,
                            |t| t.resource == c.resource && t.subs[0] == c.subs[0],
                            &mut out,
                        )
                    };
                }
            }
            "DiscardResource" => {
                let r = a[0].as_u64().unwrap_or(0);
                unsafe { self.overwritten(list, at, |t| t.resource == r, &mut out) };
            }
            "ResourceBarrier" => {
                // A split barrier's end finds its resource already in transition.
                let transitions = a[0]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|b| b["type"] == 0 && b["flags"] != 2);
                for b in transitions {
                    let p = &b["payload"];
                    let (Some(r), Some(sub), Some(before), Some(after)) = (
                        p["resource"].as_u64(),
                        p["subresource"].as_u64(),
                        p["before"].as_i64(),
                        p["after"].as_i64(),
                    ) else {
                        continue;
                    };
                    let range = Range::legacy(sub);
                    let (before, after) = (Access::State(before as i32), Access::State(after as i32));
                    // A split barrier's beginning leaves the resource in transition until its end: no copy can wait.
                    let wait = b["flags"] != 1 && waits_in(after);
                    unsafe { self.leaving(list, at, r, &range, before, wait, &mut out) };
                    self.learn(r, range, after);
                }
            }
            "Barrier" => {
                for g in a[0].as_array().into_iter().flatten().filter(|g| g["type"] == 1) {
                    for b in g["barriers"].as_array().into_iter().flatten() {
                        let (Some(r), Some(before), Some(after)) = (
                            b["resource"].as_u64(),
                            b["layout"][0].as_i64(),
                            b["layout"][1].as_i64(),
                        ) else {
                            continue;
                        };
                        let range = Range::enhanced(&b["subresources"]);
                        let (before, after) = (Access::Layout(before as i32), Access::Layout(after as i32));
                        unsafe { self.leaving(list, at, r, &range, before, waits_in(after), &mut out) };
                        self.learn(r, range, after);
                    }
                }
            }
            "SetPredication" => {
                self.predicate = a[0]
                    .as_u64()
                    .filter(|r| *r != 0)
                    .map(|r| (r, a[1].as_u64().unwrap_or(0), a[2].as_i64().unwrap_or(0) as i32));
            }
            _ => {}
        }
        out
    }

    /// Before Close of the real list `real`, `at` commands into it: what its last command wrote is copied; passes
    /// still open, and those whose copies still wait, are pending, decided where the list is submitted.
    ///
    /// # Safety
    /// `real` is the application's real list this pass instruments.
    pub unsafe fn closing(&mut self, real: *mut c_void, at: usize) -> Vec<model::Checkpoint> {
        if let Some(list) = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&real) } {
            unsafe { self.copy_outputs(list, at) };
        }
        let suspended = self.ended.take().unwrap_or(self.suspending);
        let mut out = Vec::new();
        let mut ends: Vec<(Bound, usize, Vec<u64>)> = std::mem::take(&mut self.waiting)
            .into_iter()
            .map(|w| (w.target, w.at, w.set))
            .collect();
        let set = views(&self.targets);
        for t in std::mem::take(&mut self.targets)
            .into_iter()
            .filter(|t| t.drawn && !t.reads_only())
        {
            if suspended {
                out.push(checkpoint(
                    &t,
                    at,
                    Some("its render pass suspends into another list: not copied yet"),
                ));
            } else {
                ends.push((t, at, set.clone()));
            }
        }
        for (t, at, set) in ends {
            let states = (0..t.subs.len()).map(|p| self.state(&t, p)).collect();
            let c = checkpoint(&t, at, None);
            pending().insert(
                c.id,
                Pending {
                    list: 0,
                    device: self.device.clone(),
                    target: t,
                    states,
                    set,
                },
            );
            out.push(c);
        }
        out
    }

    fn learn(&mut self, resource: u64, range: Range, access: Access) {
        let known = self.known.entry(resource).or_default();
        if matches!(range, Range::All) {
            known.clear();
        }
        known.push((range, access));
    }

    /// The state or layout subresource `sub` of a bound target is in: what this list's barriers left it in, or
    /// what binding it requires.
    fn state(&self, t: &Bound, plane: usize) -> Access {
        let sub = t.subs[plane];
        let shape = shape(&t.real);
        if let Some((_, access)) = self
            .known
            .get(&t.resource)
            .and_then(|k| k.iter().rev().find(|(r, _)| r.holds(sub, shape)))
        {
            return *access;
        }
        let layout = layouts().contains(&t.resource);
        match (layout, t.depth, t.read_only[plane]) {
            (false, false, _) => Access::State(RENDER_TARGET),
            (false, true, false) => Access::State(DEPTH_WRITE),
            (false, true, true) => Access::State(DEPTH_READ),
            (true, false, _) => Access::Layout(LAYOUT_RENDER_TARGET),
            (true, true, false) => Access::Layout(LAYOUT_DEPTH_WRITE),
            (true, true, true) => Access::Layout(LAYOUT_DEPTH_READ),
        }
    }

    /// A target's pass, with views `set` bound, has ended before command `at`: its copy waits, unless `now` (or the
    /// target can be written through a view with no command naming it), when it is copied here from the states it
    /// is in.
    #[allow(clippy::too_many_arguments)]
    unsafe fn ended(
        &mut self,
        list: &ID3D12GraphicsCommandList,
        at: usize,
        t: Bound,
        set: Vec<u64>,
        now: Option<Vec<Access>>,
        out: &mut Vec<model::Checkpoint>,
    ) {
        if t.reads_only() {
            return;
        }
        match now {
            None if !t.simultaneous => self.waiting.push(Ended { target: t, at, set }),
            now => {
                let states = now.unwrap_or_else(|| (0..t.subs.len()).map(|p| self.state(&t, p)).collect());
                unsafe { self.copy(list, at, &t, &states, out) };
            }
        }
    }

    /// The waiting copies of the targets `hit` matches, made here from the states they are in.
    unsafe fn flush(
        &mut self,
        list: &ID3D12GraphicsCommandList,
        hit: impl Fn(u64) -> bool,
        out: &mut Vec<model::Checkpoint>,
    ) {
        if self.waiting.is_empty() {
            return;
        }
        let (now, keep): (Vec<Ended>, Vec<Ended>) = std::mem::take(&mut self.waiting)
            .into_iter()
            .partition(|w| hit(w.target.resource));
        self.waiting = keep;
        for w in now {
            let states: Vec<Access> = (0..w.target.subs.len())
                .map(|p| self.state(&w.target, p))
                .collect();
            unsafe { self.copy(list, w.at, &w.target, &states, out) };
        }
    }

    /// A barrier moves subresources `range` of `resource` out of `state`: the pass ends for the bound targets it
    /// touches that were drawn to. Their copies wait if `wait` (the state it moves them into allows no write but by
    /// a command naming them), or are made here from `state`. They stay bound: the draws after a barrier that moves
    /// a target back into a writable state write it again.
    #[allow(clippy::too_many_arguments)]
    unsafe fn leaving(
        &mut self,
        list: &ID3D12GraphicsCommandList,
        at: usize,
        resource: u64,
        range: &Range,
        state: Access,
        wait: bool,
        out: &mut Vec<model::Checkpoint>,
    ) {
        for i in 0..self.targets.len() {
            let t = &self.targets[i];
            let shape = shape(&t.real);
            if !t.drawn || t.resource != resource || !t.subs.iter().any(|&s| range.holds(s, shape)) {
                continue;
            }
            let t = t.clone();
            let states: Vec<Access> = (0..t.subs.len())
                .map(|p| {
                    if range.holds(t.subs[p], shape) {
                        state
                    } else {
                        self.state(&t, p)
                    }
                })
                .collect();
            self.targets[i].drawn = false;
            let set = views(&self.targets);
            unsafe { self.ended(list, at, t, set, (!wait).then_some(states), out) };
        }
    }

    /// A clear or discard overwrites the bound targets `hit` matches: their pass so far is copied first.
    unsafe fn overwritten(
        &mut self,
        list: &ID3D12GraphicsCommandList,
        at: usize,
        hit: impl Fn(&Bound) -> bool,
        out: &mut Vec<model::Checkpoint>,
    ) {
        for i in 0..self.targets.len() {
            let t = &self.targets[i];
            if t.drawn && hit(t) {
                let t = t.clone();
                let states: Vec<Access> = (0..t.subs.len()).map(|p| self.state(&t, p)).collect();
                self.targets[i].drawn = false;
                unsafe { self.ended(list, at, t, Vec::new(), Some(states), out) };
            }
        }
    }

    /// The pass ends for every bound target; none is bound afterwards.
    unsafe fn end_pass(
        &mut self,
        list: &ID3D12GraphicsCommandList,
        at: usize,
        suspended: bool,
        out: &mut Vec<model::Checkpoint>,
    ) {
        let set = views(&self.targets);
        for t in std::mem::take(&mut self.targets).into_iter().filter(|t| t.drawn) {
            if suspended {
                if !t.reads_only() {
                    out.push(checkpoint(
                        &t,
                        at,
                        Some("its render pass suspends into another list: not copied yet"),
                    ));
                }
                continue;
            }
            unsafe { self.ended(list, at, t, set.clone(), None, out) };
        }
    }

    /// Copies one target from `states` (one per plane), recording the checkpoint, or why it was not copied.
    unsafe fn copy(
        &self,
        list: &ID3D12GraphicsCommandList,
        at: usize,
        t: &Bound,
        states: &[Access],
        out: &mut Vec<model::Checkpoint>,
    ) {
        if t.reads_only() {
            return;
        }
        let mut c = checkpoint(t, at, None);
        match unsafe { copy_into(&self.device, list, t, states, self.predication()) } {
            Ok(readback) => keep(c.id, readback),
            Err(why) => c.gap = Some(why),
        }
        out.push(c);
    }

    /// The list's predication, with its buffer.
    fn predication(&self) -> Option<(ID3D12Resource, u64, i32)> {
        self.predicate.and_then(|(r, offset, op)| {
            super::real(r)
                .and_then(|r| r.cast::<ID3D12Resource>().ok())
                .map(|r| (r, offset, op))
        })
    }

    /// Copies what the command before `at` wrote through UAVs, recording each output, or why it was not copied.
    unsafe fn copy_outputs(&mut self, list: &ID3D12GraphicsCommandList, at: usize) {
        for (command, written) in std::mem::take(&mut self.due) {
            let id = NEXT.fetch_add(1, Ordering::Relaxed);
            let (resource, sub, gap) = match written {
                Ok(w) => {
                    let copied = unsafe { self.copy_output(list, &w) }.map(|readback| keep(id, readback));
                    (w.resource, w.sub, copied.err())
                }
                Err(why) => (0, None, Some(why)),
            };
            self.outputs.push(model::Output {
                id,
                at,
                command,
                resource,
                sub,
                gap,
            });
        }
    }

    /// Copies one output from the state or layout it was written in.
    unsafe fn copy_output(
        &self,
        list: &ID3D12GraphicsCommandList,
        w: &super::outputs::Written,
    ) -> Result<Readback, String> {
        let real = super::real(w.resource)
            .and_then(|r| r.cast::<ID3D12Resource>().ok())
            .ok_or("destroyed before it was copied")?;
        let desc = unsafe { real.GetDesc() };
        let buffer = desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER;
        let sub = w.sub.unwrap_or(0);
        let known = self
            .known
            .get(&w.resource)
            .and_then(|k| k.iter().rev().find(|(r, _)| r.holds(sub, shape(&real))))
            .map(|(_, access)| *access);
        let simultaneous = desc.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS);
        // A buffer has no layout: one an enhanced barrier moves is written with no state to leave.
        let access = known.unwrap_or(
            if (buffer && self.enhanced) || (!buffer && layouts().contains(&w.resource)) {
                Access::Layout(if simultaneous && !buffer {
                    LAYOUT_COMMON
                } else {
                    LAYOUT_UNORDERED_ACCESS
                })
            } else {
                Access::State(UNORDERED_ACCESS)
            },
        );
        unsafe {
            output_into(
                &self.device,
                list,
                &real,
                &desc,
                w.sub,
                access,
                self.predication(),
            )
        }
    }
}

/// Keeps a copy's readback memory until its list is retired and the GPU is done with it.
fn keep(id: u64, readback: Readback) {
    copies().insert(
        id,
        Copy {
            data: Arc::new(readback),
            list: 0,
            last: None,
            retired: false,
        },
    );
}

/// The views of bound targets, in slot order with the depth last.
fn views(targets: &[Bound]) -> Vec<u64> {
    targets.iter().map(|t| t.view).collect()
}

/// The resource an argument names: a view's definition, a copy location (`resource`), or an id.
fn resource_of(v: &Value) -> Option<u64> {
    v["definition"]["resource"]
        .as_u64()
        .or_else(|| v["resource"].as_u64())
        .or_else(|| v.as_u64())
        .filter(|r| *r != 0)
}

/// Every texture a command names: as a target bound, cleared, discarded, moved by a barrier, copied or resolved
/// from or into. A copy waiting for its target is made before such a command.
fn mentions(call: &str, a: &Value) -> Vec<u64> {
    let all = |v: &Value| {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(resource_of)
            .collect::<Vec<_>>()
    };
    match call {
        "OMSetRenderTargets" => [all(&a[0]), resource_of(&a[1]).into_iter().collect()].concat(),
        "BeginRenderPass" => {
            let mut out: Vec<u64> = a[0]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|t| [resource_of(&t["view"]), resource_of(&t["end"]["resolve"]["dst"])])
                .flatten()
                .collect();
            out.extend(resource_of(&a[1]["view"]));
            out
        }
        "ClearRenderTargetView" | "ClearDepthStencilView" | "DiscardResource" => {
            resource_of(&a[0]).into_iter().collect()
        }
        "ClearUnorderedAccessViewUint" | "ClearUnorderedAccessViewFloat" => {
            resource_of(&a[2]).into_iter().collect()
        }
        "ResourceBarrier" => a[0]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|b| {
                let p = &b["payload"];
                [
                    resource_of(p),
                    resource_of(&p["before"]),
                    resource_of(&p["after"]),
                ]
            })
            .flatten()
            .collect(),
        "Barrier" => a[0]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|g| {
                g["barriers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(resource_of)
            })
            .collect(),
        "CopyResource" => [&a[0], &a[1]].into_iter().filter_map(resource_of).collect(),
        "CopyTextureRegion" | "ResolveSubresourceRegion" => {
            [&a[0], &a[4]].into_iter().filter_map(resource_of).collect()
        }
        "ResolveSubresource" => [&a[0], &a[2]].into_iter().filter_map(resource_of).collect(),
        "CopyTiles" => resource_of(&a[0]).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// The textures a command writes other than by drawing to them as bound targets: clears, discards, copies and
/// resolves into them, aliasing, barriers into a state or layout that allows unordered-access writes or that discard,
/// render passes that begin by clearing or discarding. A list that draws to a target before any of these continues
/// the pass an earlier list left open; one that writes it so first does not.
fn writes(call: &str, a: &Value) -> Vec<u64> {
    let one = |v: &Value| resource_of(v).into_iter().collect::<Vec<_>>();
    match call {
        "ClearRenderTargetView" | "ClearDepthStencilView" | "DiscardResource" => one(&a[0]),
        "ClearUnorderedAccessViewUint" | "ClearUnorderedAccessViewFloat" => one(&a[2]),
        "CopyResource"
        | "CopyTextureRegion"
        | "ResolveSubresource"
        | "ResolveSubresourceRegion"
        | "CopyTiles" => one(&a[0]),
        "ResourceBarrier" => a[0]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|b| {
                let p = &b["payload"];
                match b["type"].as_u64() {
                    Some(0) => p["after"]
                        .as_i64()
                        .filter(|s| !waits_in(Access::State(*s as i32)))
                        .and_then(|_| resource_of(p)),
                    Some(1) => resource_of(&p["after"]),
                    _ => None,
                }
            })
            .collect(),
        "Barrier" => a[0]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|g| g["type"] == 1)
            .flat_map(|g| g["barriers"].as_array().into_iter().flatten())
            .filter(|b| {
                // D3D12_TEXTURE_BARRIER_FLAG_DISCARD (1); a layout before of UNDEFINED discards too.
                let after = b["layout"][1].as_i64().unwrap_or(0) as i32;
                b["flags"].as_u64().unwrap_or(0) & 1 != 0
                    || b["layout"][0].as_i64().is_some_and(|l| l as i32 == -1)
                    || !waits_in(Access::Layout(after))
            })
            .filter_map(resource_of)
            .collect(),
        "BeginRenderPass" => {
            // A beginning access that discards (0) or clears (2) overwrites the attachment.
            let overwrites = |b: &Value| matches!(b["type"].as_u64(), Some(0 | 2));
            let mut out: Vec<u64> = a[0]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|t| overwrites(&t["begin"]))
                .filter_map(|t| resource_of(&t["view"]))
                .collect();
            let d = &a[1];
            if overwrites(&d["depth_begin"]) || overwrites(&d["stencil_begin"]) {
                out.extend(resource_of(&d["view"]));
            }
            out
        }
        _ => Vec::new(),
    }
}

/// Records into `list` the copy of target `t` from `states` (one per plane) into new readback memory, suspending
/// the list's predication `predicate` around it.
unsafe fn copy_into(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    t: &Bound,
    states: &[Access],
    predicate: Option<(ID3D12Resource, u64, i32)>,
) -> Result<Readback, String> {
    if t.discard {
        return Err("discarded where its render pass ended: undefined".into());
    }
    let desc = unsafe { t.real.GetDesc() };
    if desc.SampleDesc.Count > 1 {
        return Err("multisampled target: copying samples out is not supported yet".into());
    }
    let format = desc.Format.0 as u32;
    let joined =
        (t.subs.len() == 2).then(|| locum_capture::format::layout(format).map_or(4, |f| f.block_bytes));
    let layout = locum_capture::layout::texture(format, t.width, t.height, 1, 1, 1)
        .ok_or_else(|| format!("format {format} has no canonical layout"))?;
    let volume = desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
    let subs: Vec<(u32, Access)> = t.subs.iter().copied().zip(states.iter().copied()).collect();
    // A target made for simultaneous access is drawn to in COMMON.
    let common = if t.depth {
        D3D12_BARRIER_ACCESS_DEPTH_STENCIL_WRITE
    } else {
        D3D12_BARRIER_ACCESS_RENDER_TARGET
    };
    let (to_copy, back) = (
        barriers(&t.real, &subs, true, common)?,
        barriers(&t.real, &subs, false, common)?,
    );
    to_copy.possible(list)?;
    let mut footprints = Vec::new();
    let mut bytes = 0u64;
    for &sub in &t.subs {
        let (mut f, mut rows, mut row_bytes) = (D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(), 0u32, 0u64);
        unsafe {
            device.GetCopyableFootprints(
                &desc,
                sub,
                1,
                bytes,
                Some(&mut f),
                Some(&mut rows),
                Some(&mut row_bytes),
                None,
            )
        };
        if row_bytes == u64::MAX {
            return Err(format!("GetCopyableFootprints refused subresource {sub}"));
        }
        // One depth slice of a volume.
        f.Footprint.Depth = 1;
        bytes = (f.Offset + u64::from(f.Footprint.RowPitch) * u64::from(rows))
            .next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT));
        footprints.push((f, rows, row_bytes));
    }
    let readback = unsafe { readback_buffer(device, bytes, BUDGET) }?;
    unsafe {
        if predicate.is_some() {
            list.SetPredication(None, 0, D3D12_PREDICATION_OP_EQUAL_ZERO);
        }
        to_copy.issue(list);
        for (&sub, (f, _, _)) in t.subs.iter().zip(&footprints) {
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrowed(&readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *f },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrowed(&t.real),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: sub,
                },
            };
            let slice = D3D12_BOX {
                left: 0,
                top: 0,
                front: t.z,
                right: t.width,
                bottom: t.height,
                back: t.z + 1,
            };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, volume.then_some(&slice as *const _));
        }
        back.issue(list);
        if let Some((r, offset, op)) = &predicate {
            list.SetPredication(r, *offset, D3D12_PREDICATION_OP(*op));
        }
    }
    Ok(Readback {
        readback,
        footprints,
        layout,
        joined,
        bytes,
    })
}

/// New readback memory of `bytes` bytes, while the live copies hold no more than `limit` with it; a [`Readback`]
/// made of it gives the bytes back.
unsafe fn readback_buffer(device: &ID3D12Device, bytes: u64, limit: u64) -> Result<ID3D12Resource, String> {
    if BYTES.fetch_add(bytes, Ordering::AcqRel) + bytes > limit {
        BYTES.fetch_sub(bytes, Ordering::AcqRel);
        return Err(format!("readback budget of {} GiB spent", limit >> 30));
    }
    let mut readback: Option<ID3D12Resource> = None;
    let created = unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: bytes.max(1),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut readback,
        )
    };
    match (created, readback) {
        (Ok(()), Some(r)) => Ok(r),
        (e, _) => {
            BYTES.fetch_sub(bytes, Ordering::AcqRel);
            Err(format!("readback buffer of {bytes} bytes: {e:?}"))
        }
    }
}

/// Records into `list` the copy of what a command wrote through a UAV into `real`: subresource `sub` of a texture,
/// or the whole of a buffer (`sub` None), from `access`, suspending the list's predication `predicate` around it.
unsafe fn output_into(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    real: &ID3D12Resource,
    desc: &D3D12_RESOURCE_DESC,
    sub: Option<u32>,
    access: Access,
    predicate: Option<(ID3D12Resource, u64, i32)>,
) -> Result<Readback, String> {
    let buffer = desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER;
    let (layout, footprints, bytes) = if buffer {
        let bytes = desc.Width;
        if bytes > OUTPUT_BYTES {
            return Err(format!(
                "a buffer of {} MiB, more than one output's {} MiB",
                bytes >> 20,
                OUTPUT_BYTES >> 20
            ));
        }
        (locum_capture::layout::buffer(bytes as u32), Vec::new(), bytes)
    } else {
        let sub = sub.unwrap_or(0);
        let (mips, _) = shape(real);
        let mip = sub % mips;
        let depth = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
            (u32::from(desc.DepthOrArraySize) >> mip).max(1)
        } else {
            1
        };
        let format = desc.Format.0 as u32;
        let layout = locum_capture::layout::texture(
            format,
            ((desc.Width >> mip) as u32).max(1),
            (desc.Height >> mip).max(1),
            depth,
            1,
            1,
        )
        .ok_or_else(|| format!("format {format} has no canonical layout"))?;
        let size = locum_capture::layout::total(&layout);
        if size > OUTPUT_BYTES {
            return Err(format!(
                "a subresource of {} MiB, more than one output's {} MiB",
                size >> 20,
                OUTPUT_BYTES >> 20
            ));
        }
        let (mut f, mut rows, mut row_bytes) = (D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(), 0u32, 0u64);
        let mut total = 0u64;
        unsafe {
            device.GetCopyableFootprints(
                desc,
                sub,
                1,
                0,
                Some(&mut f),
                Some(&mut rows),
                Some(&mut row_bytes),
                Some(&mut total),
            )
        };
        if row_bytes == u64::MAX || total == u64::MAX {
            return Err(format!("GetCopyableFootprints refused subresource {sub}"));
        }
        (layout, vec![(f, rows, row_bytes)], total)
    };
    // Buffers move as a whole.
    let index = if buffer {
        ALL_SUBRESOURCES as u32
    } else {
        sub.unwrap_or(0)
    };
    let subs = [(index, access)];
    let written = D3D12_BARRIER_ACCESS_UNORDERED_ACCESS;
    let (to_copy, back) = (
        barriers(real, &subs, true, written)?,
        barriers(real, &subs, false, written)?,
    );
    to_copy.possible(list)?;
    let readback = unsafe { readback_buffer(device, bytes, OUTPUT_BUDGET) }?;
    unsafe {
        if predicate.is_some() {
            list.SetPredication(None, 0, D3D12_PREDICATION_OP_EQUAL_ZERO);
        }
        to_copy.issue(list);
        if buffer {
            list.CopyBufferRegion(&readback, 0, real, 0, bytes);
        } else {
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrowed(&readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: footprints[0].0,
                },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrowed(real),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: index,
                },
            };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }
        back.issue(list);
        if let Some((r, offset, op)) = &predicate {
            list.SetPredication(r, *offset, D3D12_PREDICATION_OP(*op));
        }
    }
    Ok(Readback {
        readback,
        footprints,
        layout,
        joined: None,
        bytes,
    })
}

/// A checkpoint of a bound target before command `at`, with a new id.
fn checkpoint(t: &Bound, at: usize, gap: Option<&str>) -> model::Checkpoint {
    model::Checkpoint {
        id: NEXT.fetch_add(1, Ordering::Relaxed),
        at,
        target: t.target.clone(),
        resource: t.resource,
        view: t.view,
        width: t.width,
        height: t.height,
        format: t.format,
        sub: t.subs[0],
        z: t.z,
        gap: gap.map(str::to_string),
    }
}

/// Assigns the copies made in a list generation (checkpoints' and outputs' ids) to that list, which retires them
/// when it is reset or destroyed.
pub fn belong(list: u64, ids: impl IntoIterator<Item = u64>) {
    let (mut c, mut p) = (copies(), pending());
    for id in ids {
        if let Some(copy) = c.get_mut(&id) {
            copy.list = list;
        }
        if let Some(end) = p.get_mut(&id) {
            end.list = list;
        }
    }
}

/// A resource's mips and array slices, as subresource indices count them.
fn shape(r: &ID3D12Resource) -> (u32, u32) {
    let desc = unsafe { r.GetDesc() };
    let slices = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        1
    } else {
        desc.DepthOrArraySize
    };
    (u32::from(desc.MipLevels.max(1)), u32::from(slices.max(1)))
}

/// A render pass attachment, with the ending accesses of its planes: DISCARD (0) leaves a plane undefined, and
/// NO_ACCESS (3) means the pass does not write it.
fn attachment(view: &Value, target: String, ends: [&Value; 2]) -> Option<Bound> {
    let mut t = bound(view, target)?;
    for (plane, end) in ends.iter().enumerate() {
        t.read_only[plane] |= end["type"] == 3;
    }
    let planes = t.subs.len();
    t.discard = (0..planes).all(|p| t.read_only[p] || ends[p]["type"] == 0) && !t.reads_only();
    Some(t)
}

/// The target a recorded view (with its definition) binds, or None for an empty or non-texture view.
fn bound(view: &Value, target: String) -> Option<Bound> {
    let d = &view["definition"];
    let resource = d["resource"].as_u64().filter(|r| *r != 0)?;
    let real = super::real(resource)?.cast::<ID3D12Resource>().ok()?;
    let desc = unsafe { real.GetDesc() };
    if desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER || desc.Dimension == D3D12_RESOURCE_DIMENSION_UNKNOWN
    {
        return None;
    }
    let v = &d["description"];
    let payload = &v["payload"];
    let n = |key: &str| payload[key].as_u64().unwrap_or(0) as u32;
    let (mips, slices) = shape(&real);
    let volume = desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D;
    let mip = n("MipSlice");
    let slice = if volume { 0 } else { n("FirstArraySlice") };
    let format = v["format"]
        .as_u64()
        .map(|f| f as u32)
        .filter(|f| *f != 0)
        .unwrap_or(desc.Format.0 as u32);
    let depth = d["kind"] == "dsv";
    let sub = mip + slice * mips;
    let planes = if locum_capture::format::is_depth_stencil(desc.Format.0 as u32) {
        2
    } else {
        1
    };
    // D3D12_DSV_FLAG_READ_ONLY_DEPTH (1) and D3D12_DSV_FLAG_READ_ONLY_STENCIL (2).
    let flags = if depth {
        v["flags"].as_u64().unwrap_or(0)
    } else {
        0
    };
    Some(Bound {
        target,
        view: d["id"].as_u64().unwrap_or(0),
        resource,
        real,
        subs: (0..planes).map(|p| sub + p * mips * slices).collect(),
        z: if volume { n("FirstWSlice") } else { 0 },
        format,
        width: ((desc.Width >> mip) as u32).max(1),
        height: (desc.Height >> mip).max(1),
        depth,
        read_only: [flags & 1 != 0, flags & 2 != 0],
        discard: false,
        drawn: false,
        simultaneous: desc.Flags.contains(D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS),
    })
}

fn borrowed(r: &ID3D12Resource) -> ManuallyDrop<Option<ID3D12Resource>> {
    unsafe { core::mem::transmute_copy(r) }
}

/// The barriers moving subresources between where they are and COPY_SOURCE.
struct Barriers {
    legacy: Vec<D3D12_RESOURCE_BARRIER>,
    /// Legacy transitions recorded in a second call, after `legacy`: from COPY_DEST to the read-only states copied
    /// subresources go back to (see [`barriers`]).
    settle: Vec<D3D12_RESOURCE_BARRIER>,
    enhanced: Vec<D3D12_TEXTURE_BARRIER>,
    buffers: Vec<D3D12_BUFFER_BARRIER>,
}

impl Barriers {
    /// Whether `list` can record them: enhanced barriers need ID3D12GraphicsCommandList7.
    fn possible(&self, list: &ID3D12GraphicsCommandList) -> Result<(), String> {
        if (!self.enhanced.is_empty() || !self.buffers.is_empty())
            && list.cast::<ID3D12GraphicsCommandList7>().is_err()
        {
            return Err("layout barriers need ID3D12GraphicsCommandList7".into());
        }
        Ok(())
    }

    unsafe fn issue(&self, list: &ID3D12GraphicsCommandList) {
        for batch in [&self.legacy, &self.settle] {
            if !batch.is_empty() {
                unsafe { list.ResourceBarrier(batch) };
            }
        }
        let mut groups = Vec::new();
        if !self.enhanced.is_empty() {
            groups.push(D3D12_BARRIER_GROUP {
                Type: D3D12_BARRIER_TYPE_TEXTURE,
                NumBarriers: self.enhanced.len() as u32,
                Anonymous: D3D12_BARRIER_GROUP_0 {
                    pTextureBarriers: self.enhanced.as_ptr(),
                },
            });
        }
        if !self.buffers.is_empty() {
            groups.push(D3D12_BARRIER_GROUP {
                Type: D3D12_BARRIER_TYPE_BUFFER,
                NumBarriers: self.buffers.len() as u32,
                Anonymous: D3D12_BARRIER_GROUP_0 {
                    pBufferBarriers: self.buffers.as_ptr(),
                },
            });
        }
        if !groups.is_empty() {
            // Checked by `possible` before anything was recorded.
            let list7: ID3D12GraphicsCommandList7 = list.cast().expect("ID3D12GraphicsCommandList7");
            unsafe { list7.Barrier(&groups) };
        }
    }
}

/// The transitions of a resource's subresources, each from its state or layout, to COPY_SOURCE (`to_copy`) or back.
/// A buffer has one, `ALL_SUBRESOURCES`; where it is moved by enhanced barriers (a layout given), it has no layout
/// and only its access changes. `common` is how a subresource in the COMMON layout was used.
///
/// A legacy transition from COPY_SOURCE straight back to a state that allows no write does not hold back the list's
/// later writes: on an NVIDIA driver the game's next barrier into a write state and the draw after it overlap the
/// copy, which reads what they write (Elden Ring's split-plane depth-stencil, each copy of its stencil wrong from row
/// 32 on; WARP keeps the order). A subresource going back to such a state goes through COPY_DEST: the transition into
/// a write state waits for the copy's reads.
fn barriers(
    r: &ID3D12Resource,
    subs: &[(u32, Access)],
    to_copy: bool,
    common: D3D12_BARRIER_ACCESS,
) -> Result<Barriers, String> {
    let mut out = Barriers {
        legacy: Vec::new(),
        settle: Vec::new(),
        enhanced: Vec::new(),
        buffers: Vec::new(),
    };
    let buffer = unsafe { r.GetDesc() }.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER;
    let copy = (
        D3D12_BARRIER_SYNC_COPY,
        D3D12_BARRIER_ACCESS_COPY_SOURCE,
        D3D12_BARRIER_LAYOUT_COPY_SOURCE,
    );
    let transition = |sub: u32, before: i32, after: i32| D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: borrowed(r),
                Subresource: sub,
                StateBefore: D3D12_RESOURCE_STATES(before),
                StateAfter: D3D12_RESOURCE_STATES(after),
            }),
        },
    };
    for &(sub, access) in subs {
        match access {
            Access::State(s) if s & COPY_SOURCE != 0 => {}
            Access::State(s) if to_copy => out.legacy.push(transition(sub, s, COPY_SOURCE)),
            Access::State(s) if s & WRITES == 0 => {
                out.legacy.push(transition(sub, COPY_SOURCE, COPY_DEST));
                out.settle.push(transition(sub, COPY_DEST, s));
            }
            Access::State(s) => out.legacy.push(transition(sub, COPY_SOURCE, s)),
            Access::Layout(_) if buffer => {
                let written = (D3D12_BARRIER_SYNC_ALL, D3D12_BARRIER_ACCESS_UNORDERED_ACCESS);
                let (before, after) = if to_copy {
                    (written, (copy.0, copy.1))
                } else {
                    ((copy.0, copy.1), written)
                };
                out.buffers.push(D3D12_BUFFER_BARRIER {
                    SyncBefore: before.0,
                    SyncAfter: after.0,
                    AccessBefore: before.1,
                    AccessAfter: after.1,
                    pResource: borrowed(r),
                    Offset: 0,
                    Size: u64::MAX,
                });
            }
            Access::Layout(l) => {
                let access = match l {
                    LAYOUT_COPY_SOURCE => continue,
                    LAYOUT_RENDER_TARGET => D3D12_BARRIER_ACCESS_RENDER_TARGET,
                    LAYOUT_DEPTH_WRITE => D3D12_BARRIER_ACCESS_DEPTH_STENCIL_WRITE,
                    LAYOUT_DEPTH_READ => D3D12_BARRIER_ACCESS_DEPTH_STENCIL_READ,
                    LAYOUT_SHADER_RESOURCE | LAYOUT_GENERIC_READ => D3D12_BARRIER_ACCESS_SHADER_RESOURCE,
                    LAYOUT_UNORDERED_ACCESS => D3D12_BARRIER_ACCESS_UNORDERED_ACCESS,
                    LAYOUT_COMMON => common,
                    _ => return Err(format!("layout {l} where the copy is made")),
                };
                // COMMON allows copying from it, and a texture made for simultaneous access has no other layout.
                let copy_layout = if l == LAYOUT_COMMON {
                    D3D12_BARRIER_LAYOUT(l)
                } else {
                    copy.2
                };
                let copy = (copy.0, copy.1, copy_layout);
                let target = (D3D12_BARRIER_SYNC_ALL, access, D3D12_BARRIER_LAYOUT(l));
                let (before, after) = if to_copy { (target, copy) } else { (copy, target) };
                out.enhanced.push(D3D12_TEXTURE_BARRIER {
                    SyncBefore: before.0,
                    SyncAfter: after.0,
                    AccessBefore: before.1,
                    AccessAfter: after.1,
                    LayoutBefore: before.2,
                    LayoutAfter: after.2,
                    pResource: borrowed(r),
                    Subresources: D3D12_BARRIER_SUBRESOURCE_RANGE {
                        IndexOrFirstMipLevel: sub,
                        NumMipLevels: 0,
                        ..Default::default()
                    },
                    Flags: D3D12_TEXTURE_BARRIER_FLAG_NONE,
                });
            }
        }
    }
    Ok(out)
}
