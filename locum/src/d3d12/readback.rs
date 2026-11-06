//! GPU copies of what an armed frame reads, made on the submitting queue just before the application's lists, into
//! readback memory a collector thread reads once the GPU is done: upload-buffer ranges (constant buffers), and the
//! whole of each resource the submission is the first to use, as the frame found it.
//!
//! The CPU cannot read upload heaps quickly: they are write-combined, and with resizable BAR they can be video memory
//! across PCIe. Elden Ring's armed frame read 742 MiB of them at 6.6 MB/s, 112 s. A copy on the queue reads the bytes
//! when the GPU reaches the application's work, which is when that work reads them. Upload resources stay in
//! GENERIC_READ for their whole life and buffers are COMMON whenever a submission begins, so their copies need no
//! barriers; a texture is transitioned from its tracked state (see [`super::states`]) to COPY_SOURCE and back.
//!
//! Copies go in chunks of at most [`CHUNK`] bytes, each its own submission and fence, and at most [`BUDGET`] bytes of
//! readback memory wait for the collector at once: a frame's first-use contents can be gigabytes.
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use locum_capture::d3d12::FirstUse;
use locum_capture::layout::Sub;
use serde_json::{Value, json};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

use super::states::{Access, COMMON, COPY_SOURCE, UNDEFINED};
use crate::log::log;

/// One range of an upload buffer to copy: `whole` marks the copy of a whole upload buffer at the frame's first
/// submission.
pub struct Copy {
    pub resource: u64,
    pub offset: u64,
    pub len: u64,
    pub whole: bool,
}

/// A resource to copy whole, as the submission about to use it first will find it.
pub struct Snapshot {
    pub resource: u64,
    /// Each D3D12 subresource's state (planes included); empty for a resource that is COMMON whenever a submission
    /// begins (buffers, simultaneous-access textures).
    pub states: Vec<Access>,
    /// D3D12 subresources, planes included.
    pub subresources: u32,
    /// The canonical layout's subresources (see [`locum_capture::layout`]).
    pub layout: Vec<Sub>,
    /// For a depth-stencil format D3D12 keeps in two planes, the bytes of its joined canonical texel.
    pub joined: Option<u32>,
}

/// What one chunk of a submission's copies gave.
#[derive(Default)]
pub struct Batch {
    pub submission: u64,
    pub uploads: Vec<(u64, Value)>,
    pub contents: Vec<(u64, FirstUse)>,
    /// Pass-end checkpoints read back (see [`super::passes`]).
    pub checkpoints: Vec<(u64, Result<String, String>)>,
    /// Checkpoints whose pass the next list continued.
    pub continued: Vec<u64>,
}

/// A capture's batches, filled by the collector.
pub type Results = Arc<Mutex<Vec<Batch>>>;

/// Readback memory a chunk may use, and all chunks waiting for the collector.
const CHUNK: u64 = 256 << 20;
const BUDGET: u64 = 1 << 30;
/// How long a submission waits for the budget before giving up its remaining copies: a queue can wait on a fence the
/// application signals later, and the application cannot signal it while locum holds its submitting thread.
const PATIENCE: Duration = Duration::from_secs(10);
static IN_FLIGHT: AtomicU64 = AtomicU64::new(0);

/// Offsets in the readback buffer are aligned like placed footprints, which keeps each copy's start aligned.
const ALIGN: u64 = D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT as u64;

enum Item {
    Upload {
        copy: Copy,
        resource: super::Real<ID3D12Resource>,
    },
    Contents {
        snapshot: Snapshot,
        resource: super::Real<ID3D12Resource>,
        /// Per D3D12 subresource: its footprint (offset relative to the item), rows and row bytes; empty for a
        /// buffer. A multisampled texture's are its samples' images'.
        footprints: Vec<(D3D12_PLACED_SUBRESOURCE_FOOTPRINT, u32, u64)>,
        /// A multisampled texture's passes, which draw its samples into images to copy.
        samples: Option<locum_capture::d3d12::samples::Reader>,
    },
}

struct Placed {
    item: Item,
    at: u64,
    bytes: u64,
}

/// One chunk on the GPU.
struct Pending {
    submission: u64,
    placed: Vec<Placed>,
    readback: ID3D12Resource,
    fence: ID3D12Fence,
    bytes: u64,
    // The list and allocator live until the copies are done.
    _list: ID3D12GraphicsCommandList,
    _allocator: ID3D12CommandAllocator,
}

/// Records `copies` and `snapshots` for `submission` on lists of the queue's type and executes them on the real
/// queue `queue`, each chunk then signalling a fence of our own; the collector reads them into `results`. Copies that
/// cannot be made are in `results` at once, with the reason.
///
/// # Safety
/// `queue` is the application's real queue (number `queue_id`), called from its ExecuteCommandLists before the
/// application's lists.
pub unsafe fn submit(
    queue_id: u64,
    queue: *mut c_void,
    submission: u64,
    copies: Vec<Copy>,
    snapshots: Vec<Snapshot>,
    results: &Results,
) {
    let mut failed = Batch {
        submission,
        ..Batch::default()
    };
    let items = copies
        .into_iter()
        .map(Ok)
        .chain(snapshots.into_iter().map(Err))
        .collect::<Vec<_>>();
    let context = unsafe { ID3D12CommandQueue::from_raw_borrowed(&queue) }.ok_or("no queue".to_string());
    let context = context.and_then(|queue| {
        let mut device: Option<ID3D12Device> = None;
        unsafe { queue.GetDevice(&mut device) }.map_err(|e| format!("GetDevice: {e}"))?;
        Ok((queue, device.expect("GetDevice succeeded")))
    });
    let (queue, device) = match context {
        Ok(c) => c,
        Err(reason) => {
            for item in items {
                fail(&mut failed, item, &reason);
            }
            results.lock().unwrap_or_else(|e| e.into_inner()).push(failed);
            return;
        }
    };
    let kind = unsafe { queue.GetDesc() }.Type;
    let (here, direct): (Vec<_>, Vec<_>) = items.into_iter().partition(|item| match item {
        Ok(_) => true,
        Err(snapshot) => !needs_direct(snapshot, kind),
    });
    let start = Instant::now();
    let chunks = unsafe { place(&device, kind, here, submission, &mut failed) };
    unsafe {
        run(
            queue,
            &device,
            kind,
            submission,
            chunks,
            start,
            results,
            &mut failed,
        )
    };
    if !direct.is_empty() {
        let chunks = unsafe {
            place(
                &device,
                D3D12_COMMAND_LIST_TYPE_DIRECT,
                direct,
                submission,
                &mut failed,
            )
        };
        if !chunks.is_empty() {
            unsafe {
                beside(
                    queue_id,
                    queue,
                    &device,
                    submission,
                    chunks,
                    start,
                    results,
                    &mut failed,
                )
            };
        }
    }
    if !failed.uploads.is_empty() || !failed.contents.is_empty() {
        results.lock().unwrap_or_else(|e| e.into_inner()).push(failed);
    }
}

/// Whether a snapshot can be made only on a direct queue, not on a queue of `kind`: a multisampled texture's samples
/// are drawn out, and its states, or any snapshot's, may be ones only a direct list can transition (a compute list
/// may not name PIXEL_SHADER_RESOURCE, nor a copy list any read state but COPY_SOURCE).
fn needs_direct(snapshot: &Snapshot, kind: D3D12_COMMAND_LIST_TYPE) -> bool {
    let drawn = snapshot.layout.iter().any(|s| s.sample > 0);
    kind != D3D12_COMMAND_LIST_TYPE_DIRECT
        && (drawn || snapshot.states.iter().any(|s| unreadable(*s, kind).is_some()))
        && snapshot
            .states
            .iter()
            .all(|s| unreadable(*s, D3D12_COMMAND_LIST_TYPE_DIRECT).is_none())
}

/// Finds each item's real resource and footprints for lists of `kind`, and lays the items out in chunks of at most
/// [`CHUNK`] bytes; items that cannot be copied go to `failed`.
unsafe fn place(
    device: &ID3D12Device,
    kind: D3D12_COMMAND_LIST_TYPE,
    items: Vec<Result<Copy, Snapshot>>,
    submission: u64,
    failed: &mut Batch,
) -> Vec<Vec<Placed>> {
    let mut chunks: Vec<Vec<Placed>> = vec![Vec::new()];
    let mut chunk_bytes = 0;
    for item in items {
        let placed = match item {
            Ok(copy) => match super::real::<ID3D12Resource>(copy.resource) {
                Some(resource) => {
                    let len = copy.len;
                    Ok((Item::Upload { copy, resource }, len))
                }
                None => Err(Ok(copy)),
            },
            Err(snapshot) => unsafe { contents(device, kind, snapshot) }.map_err(Err),
        };
        let (item, bytes) = match placed {
            Ok(placed) => placed,
            Err(Ok(copy)) => {
                failed.uploads.push(upload_failed(copy, "no live upload buffer"));
                continue;
            }
            Err(Err((snapshot, reason))) => {
                failed
                    .contents
                    .push(first_use(submission, &snapshot, Err(reason)));
                continue;
            }
        };
        if chunk_bytes > 0 && chunk_bytes + bytes > CHUNK {
            chunks.push(Vec::new());
            chunk_bytes = 0;
        }
        chunks.last_mut().unwrap().push(Placed {
            item,
            at: chunk_bytes,
            bytes,
        });
        chunk_bytes = (chunk_bytes + bytes).next_multiple_of(ALIGN);
    }
    chunks.retain(|c| !c.is_empty());
    chunks
}

/// A direct queue of locum's beside an application queue of another type, and the fences that order the two: the
/// application's queue signals `ready` where its next work begins and waits for `done`, which the helper signals once
/// its copies are made.
struct Helper {
    queue: ID3D12CommandQueue,
    ready: ID3D12Fence,
    done: ID3D12Fence,
    next: u64,
}

// Free-threaded D3D12 objects; the map's lock serializes their use.
unsafe impl Send for Helper {}

/// Makes `chunks` on a direct queue of locum's, between the application queue's work submitted so far and the
/// submission about to follow: the copies see what that submission will, and it waits for them. The helper queue and
/// its fences live as long as the application's queue (number `queue_id`), so no wait can outlive a fence.
#[allow(clippy::too_many_arguments)]
unsafe fn beside(
    queue_id: u64,
    queue: &ID3D12CommandQueue,
    device: &ID3D12Device,
    submission: u64,
    chunks: Vec<Vec<Placed>>,
    start: Instant,
    results: &Results,
    failed: &mut Batch,
) {
    let mut helpers = helpers();
    let helper = match helpers.entry(queue_id) {
        std::collections::hash_map::Entry::Occupied(e) => Ok(e.into_mut()),
        std::collections::hash_map::Entry::Vacant(e) => unsafe { helper(device) }.map(|h| e.insert(h)),
    };
    let helper = match helper {
        Ok(h) => h,
        Err(reason) => {
            for p in chunks.into_iter().flatten() {
                fail_item(failed, p.item, &reason);
            }
            return;
        }
    };
    helper.next += 1;
    let n = helper.next;
    // Nothing is queued on either side unless the whole handshake can be: a wait left without its signal would hang a
    // queue for good.
    if let Err(e) = unsafe { queue.Signal(&helper.ready, n) } {
        for p in chunks.into_iter().flatten() {
            fail_item(failed, p.item, &format!("helper queue: Signal: {e}"));
        }
        return;
    }
    if let Err(e) = unsafe { helper.queue.Wait(&helper.ready, n) } {
        for p in chunks.into_iter().flatten() {
            fail_item(failed, p.item, &format!("helper queue: Wait: {e}"));
        }
        return;
    }
    unsafe {
        run(
            &helper.queue,
            device,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            submission,
            chunks,
            start,
            results,
            failed,
        )
    };
    // The application's queue waits only for a signal already queued.
    if unsafe { helper.queue.Signal(&helper.done, n) }.is_ok() {
        if let Err(e) = unsafe { queue.Wait(&helper.done, n) } {
            log!("D3D12 helper queue: the application's queue could not wait for its copies: {e}");
        }
    } else {
        log!(
            "D3D12 helper queue: Signal after the copies failed; the application's queue does not wait for them"
        );
    }
}

/// The helper of each application queue that has one, by the queue's number.
fn helpers() -> std::sync::MutexGuard<'static, std::collections::HashMap<u64, Helper>> {
    static HELPERS: OnceLock<Mutex<std::collections::HashMap<u64, Helper>>> = OnceLock::new();
    HELPERS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Object `id` was destroyed: if it was a queue with a helper, the helper goes (it holds the device).
pub fn destroyed(id: u64) {
    let helper = helpers().remove(&id);
    drop(helper);
}

unsafe fn helper(device: &ID3D12Device) -> Result<Helper, String> {
    let queue = unsafe {
        device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })
    }
    .map_err(|e| format!("helper queue: CreateCommandQueue: {e}"))?;
    let fence = || unsafe { device.CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE) };
    Ok(Helper {
        queue,
        ready: fence().map_err(|e| format!("helper queue: CreateFence: {e}"))?,
        done: fence().map_err(|e| format!("helper queue: CreateFence: {e}"))?,
        next: 0,
    })
}

/// Records and executes `chunks` on `queue` (lists of `kind`), each once the readback budget has room; the collector
/// reads them into `results`.
#[allow(clippy::too_many_arguments)]
unsafe fn run(
    queue: &ID3D12CommandQueue,
    device: &ID3D12Device,
    kind: D3D12_COMMAND_LIST_TYPE,
    submission: u64,
    chunks: Vec<Vec<Placed>>,
    start: Instant,
    results: &Results,
    failed: &mut Batch,
) {
    for placed in chunks {
        let bytes = placed.last().map_or(0, |p| p.at + p.bytes);
        // Wait for the collector to make room; past our patience, the chunk is given up.
        loop {
            let in_flight = IN_FLIGHT.load(Ordering::Acquire);
            if in_flight == 0 || in_flight + bytes <= BUDGET {
                break;
            }
            if start.elapsed() > PATIENCE {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let in_flight = IN_FLIGHT.load(Ordering::Acquire);
        if in_flight != 0 && in_flight + bytes > BUDGET {
            let reason = format!(
                "readback budget: {in_flight} bytes of earlier copies still waited for the GPU after {} s",
                PATIENCE.as_secs()
            );
            for p in placed {
                fail_item(failed, p.item, &reason);
            }
            continue;
        }
        match unsafe { record(queue, device, kind, submission, placed, bytes) } {
            Ok(pending) => {
                IN_FLIGHT.fetch_add(bytes, Ordering::AcqRel);
                let results = results.clone();
                collector(Box::new(move || {
                    let batch = collect(pending);
                    IN_FLIGHT.fetch_sub(bytes, Ordering::AcqRel);
                    results.lock().unwrap_or_else(|e| e.into_inner()).push(batch);
                }));
            }
            Err((placed, reason)) => {
                for p in placed {
                    fail_item(failed, p.item, &reason);
                }
            }
        }
    }
}

/// Queues `f` on the collector after everything queued before it: a capture queues its writing this way, after its
/// copies are read.
pub fn then(f: impl FnOnce() + Send + 'static) {
    collector(Box::new(f));
}

type Job = Box<dyn FnOnce() + Send>;

pub(super) fn collector(job: Job) {
    static QUEUE: OnceLock<Mutex<mpsc::Sender<Job>>> = OnceLock::new();
    QUEUE
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("locum-readback".into())
                .spawn(move || {
                    for job in rx {
                        job();
                    }
                })
                .expect("readback collector thread");
            Mutex::new(tx)
        })
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(job)
        .expect("readback collector alive");
}

/// A snapshot's real resource and footprints, and the bytes its copy takes; or why it cannot be copied on a queue of
/// `kind`.
unsafe fn contents(
    device: &ID3D12Device,
    kind: D3D12_COMMAND_LIST_TYPE,
    snapshot: Snapshot,
) -> Result<(Item, u64), (Snapshot, String)> {
    let Some(resource) = super::real::<ID3D12Resource>(snapshot.resource) else {
        return Err((snapshot, "no live resource".into()));
    };
    if let Some(reason) = snapshot.states.iter().find_map(|s| unreadable(*s, kind)) {
        return Err((snapshot, reason));
    }
    let desc = unsafe { resource.GetDesc() };
    if desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
        return Ok((
            Item::Contents {
                snapshot,
                resource,
                footprints: Vec::new(),
                samples: None,
            },
            desc.Width,
        ));
    }
    // A multisampled texture is copied as its samples' images.
    let (samples, desc) = if desc.SampleDesc.Count > 1 {
        if kind != D3D12_COMMAND_LIST_TYPE_DIRECT {
            return Err((
                snapshot,
                "multisampled: its samples are drawn out, on a direct queue only".into(),
            ));
        }
        match unsafe { locum_capture::d3d12::samples::Reader::new(device, &desc, None) } {
            Ok(reader) => {
                let images = reader.images_desc();
                (Some(reader), images)
            }
            Err(why) => return Err((snapshot, format!("multisampled: {why}"))),
        }
    } else {
        (None, desc)
    };
    let n = if samples.is_some() {
        u32::from(desc.DepthOrArraySize) as usize
    } else {
        snapshot.subresources as usize
    };
    let mut layouts = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
    let mut rows = vec![0u32; n];
    let mut row_bytes = vec![0u64; n];
    let mut total = 0u64;
    unsafe {
        device.GetCopyableFootprints(
            &desc,
            0,
            n as u32,
            0,
            Some(layouts.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            Some(row_bytes.as_mut_ptr()),
            Some(&mut total),
        )
    };
    if total == u64::MAX || row_bytes.contains(&u64::MAX) {
        return Err((
            snapshot,
            format!("GetCopyableFootprints refused the description ({n} subresources)"),
        ));
    }
    Ok((
        Item::Contents {
            snapshot,
            resource,
            footprints: layouts
                .into_iter()
                .zip(rows)
                .zip(row_bytes)
                .map(|((f, r), b)| (f, r, b))
                .collect(),
            samples,
        },
        total,
    ))
}

/// Why a subresource in `state` cannot be copied on a queue of `kind`, if it cannot.
///
/// A legacy state a compute list may not name (PIXEL_SHADER_RESOURCE, say) cannot be got around with an enhanced
/// barrier either: legacy states map to internal LEGACY_* layouts, and an enhanced Barrier may name a subresource in
/// a legacy state only once it is in COMMON (the Enhanced Barriers spec, "Legacy layouts"). Such a copy has to be
/// made on a direct queue.
fn unreadable(state: Access, kind: D3D12_COMMAND_LIST_TYPE) -> Option<String> {
    match state {
        Access::State(s) => {
            // The states each queue type's barriers may name.
            let legal = match kind {
                D3D12_COMMAND_LIST_TYPE_COMPUTE => 0x1 | 0x8 | 0x40 | 0x200 | 0x400 | 0x800 | 0x40_0000,
                D3D12_COMMAND_LIST_TYPE_COPY => 0x400 | 0x800,
                _ => !0,
            };
            (s & !legal != 0).then(|| {
                format!(
                    "state {s:#x} cannot be transitioned on a queue of type {}",
                    kind.0
                )
            })
        }
        Access::Layout(UNDEFINED) => Some("layout UNDEFINED: no defined contents".into()),
        Access::Layout(l) => {
            let legal = match kind {
                D3D12_COMMAND_LIST_TYPE_COMPUTE => matches!(l, 0 | 1 | 3 | 6 | 7 | 8 | 24..=29),
                D3D12_COMMAND_LIST_TYPE_COPY => matches!(l, 0 | 7 | 8),
                _ => !matches!(l, 24..=29),
            };
            (!legal).then(|| format!("layout {l} cannot be transitioned on a queue of type {}", kind.0))
        }
    }
}

/// Whether a subresource in `state` must be transitioned before a copy reads it, and so after.
fn transition(state: Access) -> bool {
    match state {
        // COMMON is promoted implicitly; GENERIC_READ includes COPY_SOURCE.
        Access::State(s) => s != COMMON && s & COPY_SOURCE == 0,
        Access::Layout(l) => !matches!(l, 0 | 1 | 7 | 18 | 19 | 22 | 24 | 25 | 28),
    }
}

/// A borrowed resource as the API's struct fields hold one, without a reference of its own.
fn borrowed(r: &ID3D12Resource) -> ManuallyDrop<Option<ID3D12Resource>> {
    unsafe { core::mem::transmute_copy(r) }
}

/// The barriers to COPY_SOURCE (`to_copy`) or back for every subresource a chunk's snapshots must transition: legacy
/// transitions on `list`, layout barriers on `list7`.
unsafe fn barriers(list: &ID3D12GraphicsCommandList, placed: &[Placed], to_copy: bool) -> Result<(), String> {
    let mut legacy = Vec::new();
    let mut layouts = Vec::new();
    for p in placed {
        let Item::Contents {
            snapshot, resource, ..
        } = &p.item
        else {
            continue;
        };
        for (i, state) in snapshot.states.iter().enumerate() {
            if !transition(*state) {
                continue;
            }
            match *state {
                Access::State(s) => {
                    let (before, after) = if to_copy {
                        (s, COPY_SOURCE)
                    } else {
                        (COPY_SOURCE, s)
                    };
                    legacy.push(D3D12_RESOURCE_BARRIER {
                        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                        Anonymous: D3D12_RESOURCE_BARRIER_0 {
                            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                                pResource: borrowed(resource),
                                Subresource: i as u32,
                                StateBefore: D3D12_RESOURCE_STATES(before),
                                StateAfter: D3D12_RESOURCE_STATES(after),
                            }),
                        },
                    });
                }
                Access::Layout(l) => {
                    let copy = (
                        D3D12_BARRIER_SYNC_COPY,
                        D3D12_BARRIER_ACCESS_COPY_SOURCE,
                        D3D12_BARRIER_LAYOUT_COPY_SOURCE,
                    );
                    // Nothing in our fresh list touched it before, and nothing after needs it.
                    let none = (
                        D3D12_BARRIER_SYNC_NONE,
                        D3D12_BARRIER_ACCESS_NO_ACCESS,
                        D3D12_BARRIER_LAYOUT(l),
                    );
                    let (before, after) = if to_copy { (none, copy) } else { (copy, none) };
                    layouts.push(D3D12_TEXTURE_BARRIER {
                        SyncBefore: before.0,
                        SyncAfter: after.0,
                        AccessBefore: before.1,
                        AccessAfter: after.1,
                        LayoutBefore: before.2,
                        LayoutAfter: after.2,
                        pResource: borrowed(resource),
                        Subresources: D3D12_BARRIER_SUBRESOURCE_RANGE {
                            IndexOrFirstMipLevel: i as u32,
                            NumMipLevels: 0,
                            ..Default::default()
                        },
                        Flags: D3D12_TEXTURE_BARRIER_FLAG_NONE,
                    });
                }
            }
        }
    }
    if !legacy.is_empty() {
        unsafe { list.ResourceBarrier(&legacy) };
    }
    if !layouts.is_empty() {
        let list7: ID3D12GraphicsCommandList7 = list
            .cast()
            .map_err(|e| format!("layout barriers need ID3D12GraphicsCommandList7: {e}"))?;
        let group = D3D12_BARRIER_GROUP {
            Type: D3D12_BARRIER_TYPE_TEXTURE,
            NumBarriers: layouts.len() as u32,
            Anonymous: D3D12_BARRIER_GROUP_0 {
                pTextureBarriers: layouts.as_ptr(),
            },
        };
        unsafe { list7.Barrier(&[group]) };
    }
    Ok(())
}

/// Records and executes one chunk.
unsafe fn record(
    queue: &ID3D12CommandQueue,
    device: &ID3D12Device,
    kind: D3D12_COMMAND_LIST_TYPE,
    submission: u64,
    placed: Vec<Placed>,
    bytes: u64,
) -> Result<Pending, (Vec<Placed>, String)> {
    let fail = |placed, step: &str, e: windows::core::Error| Err((placed, format!("readback {step}: {e}")));
    let mut readback: Option<ID3D12Resource> = None;
    if let Err(e) = unsafe {
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
    } {
        return fail(placed, &format!("buffer of {bytes} bytes"), e);
    }
    let readback = readback.expect("created");
    let allocator: ID3D12CommandAllocator = match unsafe { device.CreateCommandAllocator(kind) } {
        Ok(a) => a,
        Err(e) => return fail(placed, "CreateCommandAllocator", e),
    };
    let list: ID3D12GraphicsCommandList = match unsafe { device.CreateCommandList(0, kind, &allocator, None) }
    {
        Ok(l) => l,
        Err(e) => return fail(placed, "CreateCommandList", e),
    };
    if let Err(reason) = unsafe { barriers(&list, &placed, true) } {
        return Err((placed, reason));
    }
    for p in &placed {
        match &p.item {
            Item::Upload { copy, resource } => unsafe {
                list.CopyBufferRegion(&readback, p.at, &**resource, copy.offset, copy.len)
            },
            Item::Contents {
                resource, footprints, ..
            } if footprints.is_empty() => unsafe {
                list.CopyBufferRegion(&readback, p.at, &**resource, 0, p.bytes)
            },
            Item::Contents {
                resource,
                footprints,
                samples,
                ..
            } => {
                // A multisampled texture's samples are drawn into images, which are copied instead.
                let source: &ID3D12Resource = match samples {
                    Some(reader) => {
                        unsafe { reader.record(&list, resource) };
                        reader.images()
                    }
                    None => resource,
                };
                for (i, (f, _, _)) in footprints.iter().enumerate() {
                    let mut f = *f;
                    f.Offset += p.at;
                    let dst = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: borrowed(&readback),
                        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
                    };
                    let src = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: borrowed(source),
                        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                            SubresourceIndex: i as u32,
                        },
                    };
                    unsafe { list.CopyTextureRegion(&dst, 0, 0, 0, &src, None) };
                }
            }
        }
    }
    if let Err(reason) = unsafe { barriers(&list, &placed, false) } {
        return Err((placed, reason));
    }
    if let Err(e) = unsafe { list.Close() } {
        return fail(placed, "Close", e);
    }
    let fence: ID3D12Fence = match unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) } {
        Ok(f) => f,
        Err(e) => return fail(placed, "CreateFence", e),
    };
    unsafe { queue.ExecuteCommandLists(&[Some(list.cast().expect("a graphics list is a command list"))]) };
    if let Err(e) = unsafe { queue.Signal(&fence, 1) } {
        return fail(placed, "Signal", e);
    }
    Ok(Pending {
        submission,
        placed,
        readback,
        fence,
        bytes,
        _list: list,
        _allocator: allocator,
    })
}

/// Waits for a chunk's copies, then stores each in the blob store: upload ranges as the capture records upload
/// versions, contents in the canonical layout with a key per subresource.
fn collect(pending: Pending) -> Batch {
    let Pending {
        submission,
        placed,
        readback,
        fence,
        bytes,
        ..
    } = pending;
    let mut batch = Batch {
        submission,
        ..Batch::default()
    };
    // A null event makes SetEventOnCompletion wait until the fence reaches the value.
    let mapped = unsafe { fence.SetEventOnCompletion(1, HANDLE::default()) }
        .map_err(|e| format!("readback wait: {e}"))
        .and_then(|()| {
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
            .map_err(|e| format!("readback Map: {e}"))
            .map(|()| p.cast::<u8>())
        });
    let base = match mapped {
        Ok(base) => base,
        Err(reason) => {
            for p in placed {
                fail_item(&mut batch, p.item, &reason);
            }
            return batch;
        }
    };
    for p in placed {
        let data = unsafe { base.add(p.at as usize) };
        match p.item {
            Item::Upload { copy: c, .. } => {
                let start = Instant::now();
                let mut content = unsafe { super::pipelines::bytes(data.cast(), c.len as usize) };
                super::journal::UPLOAD_COST.add(start, c.len);
                content["offset"] = json!(c.offset);
                if c.whole {
                    content["whole"] = json!(true);
                }
                batch.uploads.push((c.resource, content));
            }
            Item::Contents {
                snapshot, footprints, ..
            } => {
                let content =
                    unsafe { canonical(&snapshot.layout, snapshot.joined, &footprints, data, p.bytes) }.map(
                        |bytes| {
                            let subs = locum_capture::layout::subresources(&snapshot.layout)
                                .iter()
                                .map(|s| {
                                    locum_capture::hash::key(
                                        &bytes[s.offset as usize..(s.offset + s.size) as usize],
                                    )
                                })
                                .collect();
                            let key = locum_capture::hash::key(&bytes);
                            locum_capture::writer::blob(
                                &crate::settings::settings().blobs,
                                key.clone(),
                                bytes,
                                true,
                            );
                            (key, subs)
                        },
                    );
                batch.contents.push(first_use(submission, &snapshot, content));
            }
        }
    }
    unsafe { readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 })) };
    batch
}

/// Subresources' bytes in the canonical layout `layout`, from the footprints the GPU copied them into at `data`
/// (see [`locum_capture::layout::from_footprints`]); no footprints for a buffer.
///
/// # Safety
/// `data` must point to `bytes` readable bytes.
pub(super) unsafe fn canonical(
    layout: &[Sub],
    joined: Option<u32>,
    footprints: &[(D3D12_PLACED_SUBRESOURCE_FOOTPRINT, u32, u64)],
    data: *const u8,
    bytes: u64,
) -> Result<Vec<u8>, String> {
    let footprints: Vec<locum_capture::layout::Footprint> = footprints
        .iter()
        .map(|(f, rows, row_bytes)| locum_capture::layout::Footprint {
            offset: f.Offset,
            row_pitch: f.Footprint.RowPitch,
            depth: f.Footprint.Depth,
            rows: *rows,
            row_bytes: *row_bytes,
        })
        .collect();
    let data = unsafe { core::slice::from_raw_parts(data, bytes as usize) };
    locum_capture::layout::from_footprints(layout, joined, &footprints, data)
}

fn first_use(
    submission: u64,
    snapshot: &Snapshot,
    content: Result<(String, Vec<String>), String>,
) -> (u64, FirstUse) {
    (
        snapshot.resource,
        FirstUse {
            submission,
            states: Value::Array(snapshot.states.iter().map(|s| s.value()).collect()),
            content,
        },
    )
}

fn fail(batch: &mut Batch, item: Result<Copy, Snapshot>, reason: &str) {
    match item {
        Ok(c) => batch.uploads.push(upload_failed(c, reason)),
        Err(s) => batch
            .contents
            .push(first_use(batch.submission, &s, Err(reason.into()))),
    }
}

fn fail_item(batch: &mut Batch, item: Item, reason: &str) {
    match item {
        Item::Upload { copy, .. } => batch.uploads.push(upload_failed(copy, reason)),
        Item::Contents { snapshot, .. } => {
            batch
                .contents
                .push(first_use(batch.submission, &snapshot, Err(reason.into())))
        }
    }
}

fn upload_failed(c: Copy, reason: &str) -> (u64, Value) {
    let mut content = json!({"diagnostic":reason,"offset":c.offset,"size":c.len});
    if c.whole {
        content["whole"] = json!(true);
    }
    (c.resource, content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_transition_only_from_states_that_cannot_be_read() {
        assert!(!transition(Access::State(COMMON)));
        // GENERIC_READ includes COPY_SOURCE.
        assert!(!transition(Access::State(0xAC3)));
        assert!(transition(Access::State(0x4)));
        assert!(!transition(Access::Layout(7)));
        assert!(transition(Access::Layout(2)));
        assert!(unreadable(Access::State(0x4), D3D12_COMMAND_LIST_TYPE_COMPUTE).is_some());
        assert!(unreadable(Access::State(0x8), D3D12_COMMAND_LIST_TYPE_COMPUTE).is_none());
        assert!(unreadable(Access::State(0x80), D3D12_COMMAND_LIST_TYPE_DIRECT).is_none());
        assert!(unreadable(Access::Layout(UNDEFINED), D3D12_COMMAND_LIST_TYPE_DIRECT).is_some());
        assert!(unreadable(Access::Layout(24), D3D12_COMMAND_LIST_TYPE_DIRECT).is_some());
    }
}
