//! Render and depth targets copied where each pass ends, inside the application's own command lists: the copy runs
//! right after the pass's last draw on the GPU, before anything later in the list can overwrite the targets.
//!
//! Lists reset or created while a capture is armed are instrumented. A pass ends when its targets are rebound or
//! cleared from the state, a barrier moves a bound target out of its state, the list closes, or a native render pass
//! ends. Its targets are copied just before the command that ends it (after EndRenderPass itself, its ending
//! access done): a transition from the state the target is in to COPY_SOURCE, a copy of the view's subresource into
//! readback memory, the transition back. The application's own barrier names that state; otherwise a bound target
//! is in RENDER_TARGET or DEPTH_WRITE (the layouts of those names for resources made in a layout), or what this
//! list's own barriers left it in. Predication is suspended around the copies.
//!
//! The readback memory lives as long as the GPU may run the list: until the list is reset or destroyed and a fence
//! locum signals after its last execution has completed.
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

use locum_capture::d3d12 as model;
use locum_capture::layout::Sub;
use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

use super::states::{Access, COPY_SOURCE};

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
const RENDER_TARGET: i32 = 0x4;
const DEPTH_WRITE: i32 = 0x10;
const ALL_SUBRESOURCES: u64 = 0xFFFF_FFFF;

/// One target's copy, alive while the GPU may still write it.
struct Copy {
    readback: ID3D12Resource,
    footprints: Vec<(D3D12_PLACED_SUBRESOURCE_FOOTPRINT, u32, u64)>,
    layout: Vec<Sub>,
    joined: Option<u32>,
    bytes: u64,
    list: u64,
    /// The fence value locum signalled after the list's latest execution.
    last: Option<(ID3D12Fence, u64)>,
    /// The list was reset or destroyed: no later execution can come.
    retired: bool,
}
// Free-threaded D3D12 objects.
unsafe impl Send for Copy {}

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
}

/// Frees the copies of retired lists whose last execution has completed.
pub fn sweep() {
    copies().retain(|_, c| {
        let done = c.retired
            && c.last
                .as_ref()
                .is_none_or(|(f, v)| unsafe { f.GetCompletedValue() } >= *v);
        if done {
            BYTES.fetch_sub(c.bytes, Ordering::AcqRel);
        }
        !done
    });
}

/// A submission's executions of instrumented lists: every checkpoint they copy into, and those whose bytes the
/// armed capture reads.
#[derive(Default)]
pub struct Submitted {
    pub all: Vec<u64>,
    pub read: Vec<u64>,
    pub results: Option<super::readback::Results>,
    pub submission: u64,
}

/// After the application's lists were submitted to the real queue `queue`: locum's fence marks their completion,
/// and the copies the capture reads are read once it does.
///
/// # Safety
/// `queue` is the real queue the lists were executed on.
pub unsafe fn submitted(queue_id: u64, queue: *mut c_void, s: Submitted) {
    if s.all.is_empty() {
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
                return;
            };
            e.insert((fence, 0))
        }
    };
    *value += 1;
    let (fence, value) = (fence.clone(), *value);
    drop(fences);
    if unsafe { queue.Signal(&fence, value) }.is_err() {
        return;
    }
    let mut read = Vec::new();
    {
        let mut c = copies();
        for id in &s.all {
            if let Some(copy) = c.get_mut(id) {
                copy.last = Some((fence.clone(), value));
            }
        }
        for id in &s.read {
            if let Some(copy) = c.get(id) {
                read.push((
                    *id,
                    copy.readback.clone(),
                    copy.footprints.clone(),
                    copy.layout.clone(),
                    copy.joined,
                    copy.bytes,
                ));
            }
        }
    }
    let (Some(results), false) = (s.results, read.is_empty()) else {
        return;
    };
    let submission = s.submission;
    super::readback::collector(Box::new(move || {
        let wait =
            unsafe { fence.SetEventOnCompletion(value, windows::Win32::Foundation::HANDLE::default()) };
        let checkpoints = read
            .into_iter()
            .map(|(id, readback, footprints, layout, joined, bytes)| {
                let content = wait
                    .as_ref()
                    .map_err(|e| format!("checkpoint wait: {e}"))
                    .and_then(|()| unsafe { bytes_of(&readback, &footprints, &layout, joined, bytes) })
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

unsafe fn bytes_of(
    readback: &ID3D12Resource,
    footprints: &[(D3D12_PLACED_SUBRESOURCE_FOOTPRINT, u32, u64)],
    layout: &[Sub],
    joined: Option<u32>,
    bytes: u64,
) -> Result<Vec<u8>, String> {
    let mut p = core::ptr::null_mut();
    unsafe {
        readback.Map(
            0,
            Some(&D3D12_RANGE {
                Begin: 0,
                End: bytes as usize,
            }),
            Some(&mut p),
        )
    }
    .map_err(|e| format!("checkpoint Map: {e}"))?;
    let out = unsafe { super::readback::canonical(layout, joined, footprints, p.cast(), bytes) };
    unsafe { readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 })) };
    out
}

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

/// What an instrumented list generation knows while it is recorded.
pub struct Pass {
    device: ID3D12Device,
    targets: Vec<Bound>,
    /// EndRenderPass was recorded: its targets are copied before the next command. A suspending render pass
    /// resumes in another list, and nothing may come between: its targets are not copied.
    ended: Option<bool>,
    /// The render pass being recorded suspends.
    suspending: bool,
    /// The list's predication: buffer, offset, operation.
    predicate: Option<(u64, u64, i32)>,
    /// The states and layouts this list's own barriers left resources in, by resource, latest last.
    known: HashMap<u64, Vec<(Range, Access)>>,
}
// Free-threaded D3D12 objects; a list's pass is used by whoever records the list, one at a time.
unsafe impl Send for Pass {}

const DEPTH_READ: i32 = 0x20;
const LAYOUT_RENDER_TARGET: i32 = 2;
const LAYOUT_DEPTH_WRITE: i32 = 4;
const LAYOUT_DEPTH_READ: i32 = 5;

impl Pass {
    /// A pass state for the real graphics list `real` of the direct type, or None for other lists.
    ///
    /// # Safety
    /// `real` is the application's real command list.
    pub unsafe fn new(real: *mut c_void) -> Option<Pass> {
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
        })
    }

    /// Called before the command `call` with recorded arguments `a` is forwarded to the real list, `at` commands
    /// into it: the copies a pass ending here needs are recorded first.
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
        if let Some(suspended) = self.ended.take() {
            unsafe { self.end_pass(list, at, suspended, &mut out) };
        }
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
                for t in &mut self.targets {
                    t.drawn = true;
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
                    unsafe { self.leaving(list, at, r, &range, Access::State(before as i32), &mut out) };
                    self.learn(r, range, Access::State(after as i32));
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
                        unsafe { self.leaving(list, at, r, &range, Access::Layout(before as i32), &mut out) };
                        self.learn(r, range, Access::Layout(after as i32));
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

    /// Before Close: a pass still open ends here.
    ///
    /// # Safety
    /// As [`Pass::before`].
    pub unsafe fn closing(&mut self, real: *mut c_void, at: usize) -> Vec<model::Checkpoint> {
        let mut out = Vec::new();
        if let Some(list) = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&real) } {
            let suspended = self.ended.take().unwrap_or(self.suspending);
            unsafe { self.end_pass(list, at, suspended, &mut out) };
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

    /// A barrier moves subresources `range` of `resource` out of `state`: the pass ends for the bound targets it
    /// touches, which are no longer bound for drawing.
    unsafe fn leaving(
        &mut self,
        list: &ID3D12GraphicsCommandList,
        at: usize,
        resource: u64,
        range: &Range,
        state: Access,
        out: &mut Vec<model::Checkpoint>,
    ) {
        let touches =
            |t: &Bound| t.resource == resource && t.subs.iter().any(|&s| range.holds(s, shape(&t.real)));
        let (leaving, staying): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.targets).into_iter().partition(touches);
        self.targets = staying;
        for t in leaving.into_iter().filter(|t| t.drawn) {
            let states: Vec<Access> = (0..t.subs.len())
                .map(|p| {
                    if range.holds(t.subs[p], shape(&t.real)) {
                        state
                    } else {
                        self.state(&t, p)
                    }
                })
                .collect();
            unsafe { self.copy(list, at, &t, &states, out) };
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
                unsafe { self.copy(list, at, &t, &states, out) };
                self.targets[i].drawn = false;
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
            let states: Vec<Access> = (0..t.subs.len()).map(|p| self.state(&t, p)).collect();
            unsafe { self.copy(list, at, &t, &states, out) };
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
        c.gap = unsafe { self.record(list, t, states, c.id) }.err();
        out.push(c);
    }

    unsafe fn record(
        &self,
        list: &ID3D12GraphicsCommandList,
        t: &Bound,
        states: &[Access],
        id: u64,
    ) -> Result<(), String> {
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
        let (to_copy, back) = (barriers(&t.real, &subs, true)?, barriers(&t.real, &subs, false)?);
        if !to_copy.enhanced.is_empty() && list.cast::<ID3D12GraphicsCommandList7>().is_err() {
            return Err("layout barriers need ID3D12GraphicsCommandList7".into());
        }
        let mut footprints = Vec::new();
        let mut bytes = 0u64;
        for &sub in &t.subs {
            let (mut f, mut rows, mut row_bytes) =
                (D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(), 0u32, 0u64);
            unsafe {
                self.device.GetCopyableFootprints(
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
        if BYTES.fetch_add(bytes, Ordering::AcqRel) + bytes > BUDGET {
            BYTES.fetch_sub(bytes, Ordering::AcqRel);
            return Err(format!("checkpoint budget of {} GiB spent", BUDGET >> 30));
        }
        let mut readback: Option<ID3D12Resource> = None;
        let created = unsafe {
            self.device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_READBACK,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: bytes,
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
        let readback = match (created, readback) {
            (Ok(()), Some(r)) => r,
            (e, _) => {
                BYTES.fetch_sub(bytes, Ordering::AcqRel);
                return Err(format!("readback buffer of {bytes} bytes: {e:?}"));
            }
        };
        let predicate = self.predicate.and_then(|(r, offset, op)| {
            super::real(r)
                .and_then(|r| r.cast::<ID3D12Resource>().ok())
                .map(|r| (r, offset, op))
        });
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
        copies().insert(
            id,
            Copy {
                readback,
                footprints,
                layout,
                joined,
                bytes,
                list: 0,
                last: None,
                retired: false,
            },
        );
        Ok(())
    }
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

/// Assigns the copies made in a list generation to that list, which retires them when it is reset or destroyed.
pub fn belong(list: u64, checkpoints: &[model::Checkpoint]) {
    let mut c = copies();
    for k in checkpoints {
        if let Some(copy) = c.get_mut(&k.id) {
            copy.list = list;
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
    })
}

fn borrowed(r: &ID3D12Resource) -> ManuallyDrop<Option<ID3D12Resource>> {
    unsafe { core::mem::transmute_copy(r) }
}

/// The barriers moving subresources between where they are and COPY_SOURCE.
struct Barriers {
    legacy: Vec<D3D12_RESOURCE_BARRIER>,
    enhanced: Vec<D3D12_TEXTURE_BARRIER>,
}

impl Barriers {
    unsafe fn issue(&self, list: &ID3D12GraphicsCommandList) {
        if !self.legacy.is_empty() {
            unsafe { list.ResourceBarrier(&self.legacy) };
        }
        if !self.enhanced.is_empty() {
            let group = D3D12_BARRIER_GROUP {
                Type: D3D12_BARRIER_TYPE_TEXTURE,
                NumBarriers: self.enhanced.len() as u32,
                Anonymous: D3D12_BARRIER_GROUP_0 {
                    pTextureBarriers: self.enhanced.as_ptr(),
                },
            };
            // Checked when the barriers were made.
            let list7: ID3D12GraphicsCommandList7 = list.cast().expect("ID3D12GraphicsCommandList7");
            unsafe { list7.Barrier(&[group]) };
        }
    }
}

/// The transitions of a target's subresources, each from its state or layout, to COPY_SOURCE (`to_copy`) or back.
fn barriers(r: &ID3D12Resource, subs: &[(u32, Access)], to_copy: bool) -> Result<Barriers, String> {
    let mut out = Barriers {
        legacy: Vec::new(),
        enhanced: Vec::new(),
    };
    for &(sub, access) in subs {
        match access {
            Access::State(s) if s & COPY_SOURCE != 0 => {}
            Access::State(s) => {
                let (before, after) = if to_copy {
                    (s, COPY_SOURCE)
                } else {
                    (COPY_SOURCE, s)
                };
                out.legacy.push(D3D12_RESOURCE_BARRIER {
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
                });
            }
            Access::Layout(l) => {
                let access = match l {
                    LAYOUT_RENDER_TARGET => D3D12_BARRIER_ACCESS_RENDER_TARGET,
                    LAYOUT_DEPTH_WRITE => D3D12_BARRIER_ACCESS_DEPTH_STENCIL_WRITE,
                    LAYOUT_DEPTH_READ => D3D12_BARRIER_ACCESS_DEPTH_STENCIL_READ,
                    _ => return Err(format!("layout {l} where the pass ended")),
                };
                let copy = (
                    D3D12_BARRIER_SYNC_COPY,
                    D3D12_BARRIER_ACCESS_COPY_SOURCE,
                    D3D12_BARRIER_LAYOUT_COPY_SOURCE,
                );
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
