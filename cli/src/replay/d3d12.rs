//! Replay on Direct3D 12: resources, descriptor heaps, root signatures and pipeline states made from the capture's
//! descriptions, each event run as recorded in a command list of its own, waited for before the next.
//!
//! - A resource is made committed, on the kind of heap it was made on, or as the game made it in memory it shares:
//!   placed at its offset in the game's heap made again, or reserved with its tiles mapped as the frame found them,
//!   the frame's changes to tile mappings run where the CPU made them (see [`memory`]).
//! - Barriers: a buffer or a texture made for simultaneous access is promoted from COMMON by each use and decays to
//!   COMMON when its list completes, so only the textures that cannot be made so (depth-stencil and multisampled
//!   ones) are moved between states, by replay's own barriers around each event. The game's barriers are not run.
//! - Descriptors: each shader-visible heap of the game's is mirrored by one of the same size, and as each submission's
//!   events are reached the descriptors the capture recorded for it are written there, so a root argument's table
//!   is the game's slot in the mirror. Upload-heap buffers get the bytes the CPU had written by that submission the
//!   same way.
//! - A draw or dispatch is run from the state the capture took at it (`d12_draw_state`), with the depth bounds and
//!   shading rate its command list had set.

use std::collections::{BTreeSet, HashMap};
use std::mem::ManuallyDrop;

use locum_capture::environment;
use locum_capture::layout::{self, Footprint, Sub};
use locum_capture::rules::{self, Action, Rule};
use serde_json::Value;
use windows::Win32::Foundation::{HANDLE, RECT};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::Interface;

use crate::describe::shape;
use crate::open::Capture;

use super::Backend;

mod indexed;
mod indirect;
mod memory;
mod pipelines;
mod queries;
mod rays;
mod staging;
mod timing;
pub use timing::time_to;
mod views;

use views::Extent;

/// Resource flags kept when a resource is made again: render target, depth-stencil, unordered access, deny shader
/// resource and simultaneous access. Cross-adapter, video and acceleration-structure flags are dropped.
const KEPT_FLAGS: i32 = 0x1 | 0x2 | 0x4 | 0x8 | 0x20;
const SIMULTANEOUS: i32 = 0x20;
const ALL: u32 = u32::MAX;

/// Resource states replay moves tracked textures between.
const COMMON: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COMMON;
const SHADER_READ: i32 = 0x40 | 0x80;
const DEPTH_READ: i32 = 0x20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Heap {
    Default,
    Upload,
    Readback,
    /// A custom heap, by its CPU page property and memory pool.
    Custom {
        page: i32,
        pool: i32,
    },
}

impl Heap {
    /// Whether the CPU reaches it through a mapping.
    fn mapped(self) -> bool {
        match self {
            Heap::Default => false,
            Heap::Upload | Heap::Readback => true,
            // D3D12_CPU_PAGE_PROPERTY_NOT_AVAILABLE.
            Heap::Custom { page, .. } => page != 1,
        }
    }
}

/// A DRED breadcrumb's operation, as d3d12.h names it.
fn operation(op: D3D12_AUTO_BREADCRUMB_OP) -> String {
    const NAMES: [&str; 27] = [
        "SetMarker",
        "BeginEvent",
        "EndEvent",
        "DrawInstanced",
        "DrawIndexedInstanced",
        "ExecuteIndirect",
        "Dispatch",
        "CopyBufferRegion",
        "CopyTextureRegion",
        "CopyResource",
        "CopyTiles",
        "ResolveSubresource",
        "ClearRenderTargetView",
        "ClearUnorderedAccessView",
        "ClearDepthStencilView",
        "ResourceBarrier",
        "ExecuteBundle",
        "Present",
        "ResolveQueryData",
        "BeginSubmission",
        "EndSubmission",
        "DecodeFrame",
        "ProcessFrames",
        "AtomicCopyBufferUint",
        "AtomicCopyBufferUint64",
        "ResolveSubresourceRegion",
        "WriteBufferImmediate",
    ];
    NAMES
        .get(op.0 as usize)
        .map_or_else(|| format!("operation {}", op.0), |n| n.to_string())
}

/// Names a replay object after what it stands for in the capture, so the debug layer's messages say which it is.
fn name(object: &impl Interface, name: String) {
    if let Ok(o) = object.cast::<ID3D12Object>() {
        let _ = unsafe { o.SetName(&windows::core::HSTRING::from(name)) };
    }
}

/// A call a command list made, by its event.
struct ListCall {
    seq: i64,
    call: String,
    args: Value,
}

struct Made {
    resource: ID3D12Resource,
    desc: D3D12_RESOURCE_DESC,
    /// Original buffer width recorded in the capture (may exceed `desc.Width` when clamped for WARP's 4 GiB - 64 KiB limit).
    recorded_width: u64,
    heap: Heap,
    /// A texture replay moves between states with barriers (see the module's note).
    tracked: bool,
    extent: Extent,
    /// The canonical layout of its contents, and for a depth-stencil format the bytes of its joined texel.
    layout: Vec<Sub>,
    joined: Option<u32>,
    /// Where a copy of each D3D12 subresource (planes included) lies in a buffer, as the API gives it and as the
    /// canonical layout reads it, and that buffer's size; for a multisampled texture, of each one-sample image its
    /// samples are copied through.
    placed: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
    footprints: Vec<Footprint>,
    copy_bytes: u64,
    va: u64,
    /// The blob its bytes last came from, for an upload-heap buffer.
    from: Option<String>,
}

impl Made {
    fn buffer(&self) -> bool {
        self.desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER
    }
}

/// A descriptor heap of replay's: a mirror of one of the game's, or scratch room for views a call names by
/// definition.
struct Mirror {
    heap: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    count: u32,
}

impl Mirror {
    fn cpu(&self, slot: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: self.cpu + slot as usize * self.step as usize,
        }
    }

    fn gpu(&self, slot: u32) -> D3D12_GPU_DESCRIPTOR_HANDLE {
        D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: self.gpu + u64::from(slot) * u64::from(self.step),
        }
    }
}

/// One recorded descriptor: `d12_descriptor_versions`.
struct Version {
    id: i64,
    heap: i64,
    slot: u32,
    kind: String,
    resource: i64,
    counter: i64,
    description: Value,
}

const VERSION_COLUMNS: &str = "v.id, v.heap, v.slot, v.kind, v.resource, v.counter_resource, v.description";

fn version(r: &rusqlite::Row) -> rusqlite::Result<Version> {
    Ok(Version {
        id: r.get(0)?,
        heap: r.get(1)?,
        slot: r.get::<_, i64>(2)? as u32,
        kind: r.get(3)?,
        resource: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
        counter: r.get::<_, Option<i64>>(5)?.unwrap_or(0),
        description: r
            .get::<_, Option<String>>(6)?
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or(Value::Null),
    })
}

/// A root signature, with what the capture decoded of it.
struct Root {
    signature: ID3D12RootSignature,
    decoded: Value,
}

/// What a draw or dispatch runs with.
enum Pipeline {
    State(ID3D12PipelineState),
    /// A raytracing state object (SetPipelineState1).
    Rays(ID3D12StateObject),
}

/// A transition of a tracked texture around one event: its subresources and the state they take.
struct Need {
    resource: i64,
    subs: Vec<u32>,
    state: i32,
}

/// Runtime GPU/WARP bindless descriptor-slot tracer: instruments SM 6.6+ DXIL `createHandleFromHeap` calls
/// to atomically set bits in a 2-region `RWByteAddressBuffer` bound at a reserved slot of each mirrored CBV/SRV/UAV
/// heap, reading back the exact `(draw, event, heap, slot, is_write)` accesses executed on the GPU.
struct BindlessHeapLog {
    log_slot: u32,
    max_slots: u32,
    gpu_buf: ID3D12Resource,
    zero_buf: ID3D12Resource,
    readback_buf: ID3D12Resource,
    readback_ptr: *const u8,
    bytes: u64,
}

#[derive(Default)]
struct BindlessTracer {
    heaps: HashMap<i64, BindlessHeapLog>,
    patched_shaders: HashMap<(String, u32, u32), Option<Vec<u8>>>,
    hits: Vec<(i64, i64, i64, u32, bool)>,
    pending: Option<(i64, i64, i64, u32)>,
}

pub struct D3d12 {
    warp: bool,
    device: ID3D12Device2,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    fenced: u64,
    /// Buffers the open list copies from or into, kept until it completes.
    kept: Vec<ID3D12Resource>,
    /// The most bytes one of them holds (see [`staging`]).
    limit: u64,
    resources: HashMap<i64, Made>,
    /// Mirrors of the game's shader-visible heaps (None for a heap that is not, or could not be made).
    heaps: HashMap<i64, Option<Mirror>>,
    /// Shader-visible descriptor heap ids in this capture (cached once for fast descriptor loading).
    visible_heaps: Option<std::collections::HashSet<i64>>,
    rtvs: Mirror,
    dsvs: Mirror,
    /// Room for the CPU handle a UAV clear names.
    cpu_views: Mirror,
    /// The capture's submissions in the order the CPU made them, with that order's numbers, and how many have had
    /// their descriptors and upload bytes put in place.
    submissions: Option<Vec<(i64, i64)>>,
    applied: usize,
    /// Mirror slots a rule changed for the event being run, put back after it.
    changed: Vec<(i64, u32)>,
    /// Each mirrored slot's descriptor version.
    versions: HashMap<(i64, u32), i64>,
    roots: HashMap<i64, Option<Root>>,
    psos: HashMap<(i64, Vec<String>), Option<ID3D12PipelineState>>,
    signatures: HashMap<i64, Option<ID3D12CommandSignature>>,
    /// The game's buffers by GPU address, for the addresses in ExecuteIndirect's argument records and shader tables.
    addresses: Option<std::rc::Rc<indirect::Addresses>>,
    /// Acceleration structures and raytracing state objects.
    rays: rays::Rays,
    /// By command list execution, its depth bounds and shading rate calls, which `d12_draw_state` does not hold.
    list_state: Option<HashMap<i64, Vec<ListCall>>>,
    replaced: HashMap<String, Option<Vec<u8>>>,
    /// Heaps, and the resources made in them.
    memory: memory::Memory,
    queries: queries::Queries,
    /// Resources the frame uses that the capture holds no description of.
    undescribed: Vec<i64>,
    warnings: BTreeSet<String>,
    rule_notes: BTreeSet<String>,
    applied_initial_rules: bool,
    debug: Option<ID3D12InfoQueue>,
    /// Each draw's and dispatch's GPU time, with `--times` (see [`timing`]).
    timing: Option<timing::Timing>,
    /// Optional runtime DXIL `createHandleFromHeap` bindless slot tracer.
    bindless_tracer: Option<BindlessTracer>,
    /// True when GPU-based validation (`Validation::Gpu`) is enabled on this device.
    gbv: bool,
    /// Large directly-indexed shader (`(blob_key, bytes)`) in the open command list on WARP, for removal hints.
    large_bindless_shader: Option<(String, u64)>,
    /// Resources that have received initial contents, CPU upload bytes, a rule write, or an in-plan GPU write.
    populated: std::collections::HashSet<i64>,
}

/// The resources each resource of a Direct3D 12 capture shares memory with (see [`memory`]).
pub fn aliases(
    c: &Capture,
    relevant: Option<&std::collections::HashSet<i64>>,
) -> HashMap<i64, BTreeSet<i64>> {
    memory::aliases(c, relevant)
}

/// The resources of a Direct3D 12 capture that hold acceleration structures.
pub fn acceleration_structures(c: &Capture) -> BTreeSet<i64> {
    let mut out: BTreeSet<i64> = c
        .rows(
            "SELECT id, description FROM d12_objects WHERE kind LIKE 'ID3D12Resource%'",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .into_iter()
        .filter(|(_, d)| {
            serde_json::from_str::<Value>(d).is_ok_and(|d| locum_capture::d3d12::acceleration_structures(&d))
        })
        .map(|(id, _)| id)
        .collect();
    if c.has_table("d12_acceleration_structures") {
        out.extend(
            c.rows("SELECT resource FROM d12_acceleration_structures", [], |r| {
                r.get::<_, i64>(0)
            }),
        );
    }
    out
}

/// For planning: the events that begin, end, count into or resolve the frame's queries, with the queries (as
/// resources of negative id) each reads and writes.
pub fn query_touches(c: &Capture) -> Vec<(i64, Vec<i64>, Vec<i64>)> {
    queries::touches(c)
}

/// For planning: what the heaps `events` index directly name (see [`indexed`]).
pub fn heap_named(
    c: &Capture,
    events: &BTreeSet<i64>,
    existing: &BTreeSet<i64>,
    written_in_plan: &BTreeSet<i64>,
) -> indexed::HeapNamed {
    indexed::named(c, events, existing, written_in_plan)
}

fn u(v: &Value, i: usize) -> u64 {
    v[i].as_u64().unwrap_or(0)
}

fn rects(v: &Value) -> Vec<RECT> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|r| {
            let n = |i: usize| r[i].as_i64().unwrap_or(0) as i32;
            RECT {
                left: n(0),
                top: n(1),
                right: n(2),
                bottom: n(3),
            }
        })
        .collect()
}

fn floats(v: &Value) -> [f32; 4] {
    [0, 1, 2, 3].map(|i| v[i].as_f64().unwrap_or(0.0) as f32)
}

/// Reads a 4-float array using raw u32 IEEE-754 bits when present (preserving NaN / Inf / -0.0 payloads).
fn floats_exact(v: &Value, bits: Option<&Value>) -> [f32; 4] {
    if let Some(arr) = bits.and_then(Value::as_array)
        && arr.len() >= 4
        && arr.iter().take(4).all(Value::is_u64)
    {
        return [0, 1, 2, 3].map(|i| f32::from_bits(arr[i].as_u64().unwrap_or(0) as u32));
    }
    floats(v)
}

/// A resource as a field of a structure the API takes, without the reference the field would own.
fn borrowed(r: &ID3D12Resource) -> ManuallyDrop<Option<ID3D12Resource>> {
    unsafe { std::mem::transmute_copy(r) }
}

fn transition(r: &ID3D12Resource, sub: u32, before: i32, after: i32) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
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
    }
}

impl D3d12 {
    pub fn new(warp: bool, validation: super::Validation) -> Result<D3d12, String> {
        let debug = validation != super::Validation::Off;
        unsafe {
            if debug {
                let mut d: Option<ID3D12Debug> = None;
                D3D12GetDebugInterface(&mut d).map_err(|e| format!("D3D12GetDebugInterface: {e}"))?;
                let d = d.ok_or("no debug interface")?;
                d.EnableDebugLayer();
                // The layer stays in the process once enabled: say each time whether shaders are patched too.
                if let Ok(d1) = d.cast::<ID3D12Debug1>() {
                    d1.SetEnableGPUBasedValidation(validation == super::Validation::Gpu);
                } else if validation == super::Validation::Gpu {
                    return Err("this runtime has no GPU-based validation (ID3D12Debug1)".into());
                }
            }
            // If the device is removed, DRED says which address faulted and which resources were around it.
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
            }
            let mut device: Option<ID3D12Device2> = None;
            if warp {
                let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))
                    .map_err(|e| format!("CreateDXGIFactory2: {e}"))?;
                let adapter: IDXGIAdapter = factory
                    .EnumWarpAdapter()
                    .map_err(|e| format!("EnumWarpAdapter: {e}"))?;
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device)
            } else {
                D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut device)
            }
            .map_err(|e| format!("D3D12CreateDevice: {e}"))?;
            let device = device.ok_or("no device")?;
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .map_err(|e| format!("CreateCommandQueue: {e}"))?;
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .map_err(|e| format!("CreateCommandAllocator: {e}"))?;
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .map_err(|e| format!("CreateCommandList: {e}"))?;
            let fence: ID3D12Fence = device
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
                .map_err(|e| format!("CreateFence: {e}"))?;
            let scratch = |kind, count| mirror(&device, kind, count, false);
            let rtvs = scratch(D3D12_DESCRIPTOR_HEAP_TYPE_RTV, 8)?;
            let dsvs = scratch(D3D12_DESCRIPTOR_HEAP_TYPE_DSV, 1)?;
            let cpu_views = scratch(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1)?;
            let debug = if debug {
                let queue = device
                    .cast::<ID3D12InfoQueue>()
                    .map_err(|e| format!("ID3D12InfoQueue: {e}"))?;
                // Only errors are reported: keep them all, and nothing else.
                let mut severities = [D3D12_MESSAGE_SEVERITY_CORRUPTION, D3D12_MESSAGE_SEVERITY_ERROR];
                let filter = D3D12_INFO_QUEUE_FILTER {
                    AllowList: D3D12_INFO_QUEUE_FILTER_DESC {
                        NumSeverities: severities.len() as u32,
                        pSeverityList: severities.as_mut_ptr(),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                queue
                    .PushStorageFilter(&filter)
                    .map_err(|e| format!("PushStorageFilter: {e}"))?;
                queue
                    .SetMessageCountLimit(1 << 20)
                    .map_err(|e| format!("SetMessageCountLimit: {e}"))?;
                Some(queue)
            } else {
                None
            };
            let timing = timing::Timing::new(&device, &queue)?;
            Ok(D3d12 {
                warp,
                device,
                queue,
                allocator,
                list,
                fence,
                fenced: 0,
                kept: Vec::new(),
                limit: if warp { 16 << 20 } else { staging::LIMIT },
                resources: HashMap::new(),
                heaps: HashMap::new(),
                visible_heaps: None,
                rtvs,
                dsvs,
                cpu_views,
                submissions: None,
                applied: 0,
                changed: Vec::new(),
                versions: HashMap::new(),
                roots: HashMap::new(),
                psos: HashMap::new(),
                signatures: HashMap::new(),
                addresses: None,
                rays: rays::Rays::default(),
                list_state: None,
                replaced: HashMap::new(),
                memory: memory::Memory::default(),
                queries: queries::Queries::default(),
                undescribed: Vec::new(),
                warnings: BTreeSet::new(),
                rule_notes: BTreeSet::new(),
                applied_initial_rules: false,
                debug,
                timing,
                bindless_tracer: None,
                gbv: validation == super::Validation::Gpu,
                large_bindless_shader: None,
                populated: std::collections::HashSet::new(),
            })
        }
    }

    fn warn(&mut self, w: impl Into<String>) {
        self.warnings.insert(w.into());
    }

    fn made(&self, id: i64) -> Result<&Made, String> {
        self.resources
            .get(&id)
            .ok_or_else(|| format!("resource #{id} was not made"))
    }

    fn resource(&self, id: i64) -> Result<ID3D12Resource, String> {
        Ok(self.made(id)?.resource.clone())
    }

    /// A resource's GPU address plus `offset`, or 0 (a null view) for one not made.
    fn address(&self, id: i64, offset: u64) -> u64 {
        self.resources.get(&id).map_or(0, |m| m.va + offset)
    }

    /// Runs what the open list holds and waits for it; the list is open again, empty, after.
    fn flush(&mut self) -> Result<(), String> {
        if let Some(t) = &mut self.timing {
            t.resolve(&self.list);
        }
        let closed = unsafe { self.list.Close() };
        let ran = closed.and_then(|()| unsafe {
            self.queue
                .ExecuteCommandLists(&[Some(self.list.cast::<ID3D12CommandList>()?)]);
            self.fenced += 1;
            self.queue.Signal(&self.fence, self.fenced)?;
            // Without an event the call returns once the fence has the value.
            self.fence.SetEventOnCompletion(self.fenced, HANDLE::default())
        });
        let removed = unsafe { self.device.GetDeviceRemovedReason() };
        if let Some(t) = &mut self.timing {
            t.write(ran.is_ok() && removed.is_ok())?;
        }
        self.kept.clear();
        unsafe {
            self.allocator
                .Reset()
                .map_err(|e| format!("command allocator Reset: {e}"))?;
            self.list
                .Reset(&self.allocator, None)
                .map_err(|e| format!("command list Reset: {e}"))?;
        }
        let large_shader = self.large_bindless_shader.take();
        removed.map_err(|e| {
            let hint = if self.warp && e.code().0 as u32 == 0x887A_0005 {
                if let Some((key, size)) = &large_shader {
                    format!(
                        " [WARP removed the device on large directly-indexed shader {key} ({size} bytes); \
                         WARP+GBV (--debug) also crashes d3d10warp.dll on shaders this size — retry without --warp \
                         on the hardware GPU]"
                    )
                } else if self.debug.is_none() {
                    " [WARP removed the device without GBV; retry with --debug or without --warp]".into()
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            format!(
                "the device was removed: {e}{}{}{hint}",
                self.removal(),
                self.validation()
            )
        })?;
        ran.map_err(|e| format!("the command list did not run: {e}{}", self.validation()))
    }

    /// What DRED recorded of a device removal: the GPU's last operation, and for a page fault, its address and the
    /// resources (named `#id`) at it or freed from it.
    fn removal(&self) -> String {
        let Ok(dred) = self.device.cast::<ID3D12DeviceRemovedExtendedData>() else {
            return String::new();
        };
        let wide = |p: windows::core::PCWSTR| {
            if p.is_null() {
                String::new()
            } else {
                unsafe { p.to_string() }.unwrap_or_default()
            }
        };
        let mut out = Vec::new();
        unsafe {
            if let Ok(crumbs) = dred.GetAutoBreadcrumbsOutput() {
                let mut node = crumbs.pHeadAutoBreadcrumbNode;
                while let Some(n) = node.as_ref() {
                    let count = n.BreadcrumbCount as usize;
                    let done = n.pLastBreadcrumbValue.as_ref().map_or(0, |v| *v as usize);
                    // A list the GPU started and did not finish.
                    if done < count && !n.pCommandHistory.is_null() {
                        let history = std::slice::from_raw_parts(n.pCommandHistory, count);
                        out.push(format!(
                            "DRED: the GPU stopped at operation {done} of {count} ({}) of its list",
                            operation(history[done])
                        ));
                    }
                    node = n.pNext;
                }
            }
            if let Ok(fault) = dred.GetPageFaultAllocationOutput()
                && fault.PageFaultVA != 0
            {
                let names = |mut node: *const D3D12_DRED_ALLOCATION_NODE| {
                    let mut names = Vec::new();
                    while let Some(n) = node.as_ref() {
                        names.push(wide(n.ObjectNameW));
                        node = n.pNext;
                    }
                    names.join(" ")
                };
                out.push(format!(
                    "DRED: page fault at {:#x}, in [{}], freed from [{}]",
                    fault.PageFaultVA,
                    names(fault.pHeadExistingAllocationNode),
                    names(fault.pHeadRecentFreedAllocationNode)
                ));
            }
        }
        if out.is_empty() {
            String::new()
        } else {
            format!(" ({})", out.join("; "))
        }
    }

    /// The validation layer's errors so far, for an error message (empty without `--debug`).
    fn validation(&self) -> String {
        let messages = self.messages();
        if messages.is_empty() {
            String::new()
        } else {
            format!(" ({})", messages.join("; "))
        }
    }

    fn messages(&self) -> Vec<String> {
        let Some(queue) = &self.debug else {
            return Vec::new();
        };
        let mut out = Vec::new();
        unsafe {
            if queue.GetNumMessagesDiscardedByMessageCountLimit() != 0 {
                out.push("D3D12 validation messages were lost to the queue limit".into());
            }
            for i in 0..queue.GetNumStoredMessagesAllowedByRetrievalFilter() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() {
                    continue;
                }
                let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
                let message = storage.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_err() {
                    continue;
                }
                let message = &*message;
                if matches!(
                    message.Severity,
                    D3D12_MESSAGE_SEVERITY_ERROR | D3D12_MESSAGE_SEVERITY_CORRUPTION
                ) {
                    let text =
                        std::slice::from_raw_parts(message.pDescription, message.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    if !self.allowed_in_common(message.ID, text) {
                        out.push(format!("D3D12 validation: {text}"));
                    }
                }
            }
        }
        out
    }

    /// Whether a message is GPU-based validation's about a texture made for simultaneous access: it takes such a
    /// texture as in the COMMON layout, where only copies and shader reads are allowed. Simultaneous-access textures
    /// are always in COMMON, and there they can be used as UAVs and render targets without barriers (the enhanced
    /// barriers spec, "COMMON layout"). The message comes for any of them a shader writes as a UAV, after a barrier
    /// to UNORDERED_ACCESS or not, on the GPU and on WARP alike; it is not an error.
    fn allowed_in_common(&self, id: D3D12_MESSAGE_ID, text: &str) -> bool {
        // Replay names each resource `#id`: "Resource: 0x...:'#2895', ...".
        let named = text
            .split_once("Resource: ")
            .and_then(|(_, rest)| rest.split_once(":'#"))
            .and_then(|(_, rest)| rest.split_once('\''))
            .and_then(|(n, _)| n.parse::<i64>().ok());
        id == D3D12_MESSAGE_ID_GPU_BASED_VALIDATION_INCOMPATIBLE_TEXTURE_LAYOUT
            && text.contains("Layout: D3D12_BARRIER_LAYOUT_COMMON")
            && named
                .and_then(|n| self.resources.get(&n))
                .is_some_and(|m| m.desc.Flags.0 & SIMULTANEOUS != 0)
    }

    /// A committed buffer of `size` bytes on an upload or readback heap, mapped; kept until the open list completes.
    fn staging(&mut self, heap: D3D12_HEAP_TYPE, size: u64) -> Result<(ID3D12Resource, *mut u8), String> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let state = if heap == D3D12_HEAP_TYPE_UPLOAD {
            D3D12_RESOURCE_STATE_GENERIC_READ
        } else {
            D3D12_RESOURCE_STATE_COPY_DEST
        };
        let mut r: Option<ID3D12Resource> = None;
        unsafe {
            self.device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: heap,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                state,
                None,
                &mut r,
            )
        }
        .map_err(|e| format!("a {size}-byte staging buffer: {e}"))?;
        let r = r.ok_or("no staging buffer")?;
        let mut p = core::ptr::null_mut();
        unsafe { r.Map(0, None, Some(&mut p)) }.map_err(|e| format!("staging Map: {e}"))?;
        self.kept.push(r.clone());
        Ok((r, p.cast()))
    }

    /// Barriers for the tracked textures among `needs`, from COMMON to what each needs (`back`: the reverse).
    fn barriers(&self, needs: &[Need], back: bool) {
        // Each subresource's state, merged: read states add up, a write state wins.
        let mut by: HashMap<i64, HashMap<u32, i32>> = HashMap::new();
        for n in needs {
            let Some(m) = self.resources.get(&n.resource) else {
                continue;
            };
            if !m.tracked {
                continue;
            }
            let subs = by.entry(n.resource).or_default();
            for &s in &n.subs {
                let e = subs.entry(s).or_insert(0);
                let write = |x: i32| x & (0x4 | 0x8 | 0x10 | 0x400 | 0x1000 | 0x100) != 0;
                *e = if write(n.state) || write(*e) {
                    if write(n.state) { n.state } else { *e }
                } else {
                    *e | n.state
                };
            }
        }
        let mut out = Vec::new();
        for (id, subs) in &by {
            let m = &self.resources[id];
            let states: BTreeSet<i32> = subs.values().copied().collect();
            let mut push = |sub: u32, state: i32| {
                let (before, after) = if back { (state, 0) } else { (0, state) };
                out.push(transition(&m.resource, sub, before, after));
            };
            if states.len() == 1 && subs.len() as u32 == m.extent.count() {
                push(ALL, *states.first().expect("one state"));
            } else {
                for (&sub, &state) in subs {
                    push(sub, state);
                }
            }
        }
        if !out.is_empty() {
            unsafe { self.list.ResourceBarrier(&out) };
        }
    }

    fn need_all(&self, id: i64, state: i32) -> Need {
        let count = self.resources.get(&id).map_or(0, |m| m.extent.count());
        Need {
            resource: id,
            subs: (0..count).collect(),
            state,
        }
    }

    /// The submission an event's command list ran in.
    fn submission_of(c: &Capture, seq: i64) -> Option<i64> {
        c.row(
            "SELECT x.submission FROM d12_event_locations l JOIN d12_executions x ON x.id = l.execution \
             WHERE l.event = ?1",
            [seq],
            |r| r.get(0),
        )
    }

    /// Shader-visible descriptor heap ids in this capture (cached once).
    fn visible_heap_ids(&mut self, c: &Capture) -> std::collections::HashSet<i64> {
        self.visible_heaps
            .get_or_insert_with(|| {
                c.rows(
                    "SELECT id, description FROM d12_objects WHERE kind = 'ID3D12DescriptorHeap'",
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .into_iter()
                .filter(|(_, d)| {
                    serde_json::from_str::<Value>(d)
                        .ok()
                        .and_then(|v| v["flags"].as_u64())
                        .is_some_and(|f| f & 1 != 0)
                })
                .map(|(id, _)| id)
                .collect()
            })
            .clone()
    }

    /// Puts in place the descriptors and upload bytes of every submission up to `submission`.
    fn enter(
        &mut self,
        c: &Capture,
        submission: i64,
        seq: Option<i64>,
        rules: &[Rule],
    ) -> Result<(), String> {
        let order = self.submissions.get_or_insert_with(|| {
            c.rows(
                "SELECT id, cpu_seq FROM d12_submissions ORDER BY cpu_seq, id",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        });
        let Some(at) = order.iter().position(|&(s, _)| s == submission) else {
            return Ok(());
        };
        if self.applied > at {
            return Ok(());
        }
        let visible = self.visible_heap_ids(c);
        // Coalesce descriptor updates across skipped submissions (self.applied..=at) so each (heap, slot) is written
        // once with its latest version at `submission`, and skip JSON parsing for unmade resources.
        type RawDesc = (i64, String, i64, i64, Option<String>);
        let mut latest_desc: HashMap<(i64, u32), RawDesc> = HashMap::new();
        while self.applied <= at {
            let (s, cpu_seq) = self.submissions.as_ref().expect("ordered")[self.applied];
            self.applied += 1;
            // The changes to tile mappings made before it, then the first-use contents copied at it, as the frame had
            // them when it ran.
            self.remap_before(c, cpu_seq, seq, rules)?;
            self.first_uses(s)?;
            if !visible.is_empty() {
                for (id, heap, slot, kind, resource, counter, desc) in c.rows(
                    "SELECT v.id, v.heap, v.slot, v.kind, v.resource, v.counter_resource, v.description \
                     FROM d12_submission_descriptors s JOIN d12_descriptor_versions v \
                     ON v.id = s.version WHERE s.submission = ?1 ORDER BY v.id",
                    [s],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)? as u32,
                            r.get::<_, String>(3)?,
                            r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                            r.get::<_, Option<i64>>(5)?.unwrap_or(0),
                            r.get::<_, Option<String>>(6)?,
                        ))
                    },
                ) {
                    if !visible.contains(&heap) {
                        continue;
                    }
                    latest_desc.insert((heap, slot), (id, kind, resource, counter, desc));
                }
            }
            let uploads: Vec<(i64, String)> = c.rows(
                "SELECT resource, content_json FROM d12_upload_versions WHERE submission = ?1 \
                 ORDER BY whole DESC, json_extract(content_json, '$.size') DESC",
                [s],
                |r| Ok((r.get(0)?, r.get(1)?)),
            );
            for (id, json) in uploads {
                let Some(m) = self.resources.get(&id) else {
                    continue;
                };
                if !m.heap.mapped() || !m.buffer() {
                    continue;
                }
                let j: Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;
                let Some(key) = j["blob"].as_str() else {
                    continue;
                };
                if m.from.as_deref() == Some(key) {
                    continue;
                }
                let bytes = c.blob(key)?;
                let offset = j["offset"].as_u64().unwrap_or(0);
                self.map_write(id, offset, &bytes)?;
                self.populated.insert(id);
                if j["whole"] == true {
                    self.resources.get_mut(&id).expect("made").from = Some(key.to_string());
                }
            }
        }
        for ((heap, slot), (id, kind, resource, counter, desc_str)) in latest_desc {
            // Fast path: an SRV/UAV/CBV pointing to a resource replay did not make becomes a null view without
            // allocating a full serde_json::Value tree (crucial on 1M-slot bindless heaps).
            let needs_parse = kind == "sampler"
                || kind == "empty"
                || resource == 0
                || self.resources.contains_key(&resource)
                || (kind == "srv"
                    && desc_str
                        .as_deref()
                        .is_some_and(|s| s.contains("\"dimension\":11")));
            if !needs_parse {
                self.null_descriptor(c, id, heap, slot, &kind, desc_str.as_deref());
                continue;
            }
            let v = Version {
                id,
                heap,
                slot,
                kind,
                resource,
                counter,
                description: desc_str
                    .and_then(|d| serde_json::from_str(&d).ok())
                    .unwrap_or(Value::Null),
            };
            self.descriptor(c, &v);
        }
        Ok(())
    }

    /// Writes a null descriptor into `(heap, slot)` for a resource replay did not make, avoiding full JSON parsing.
    fn null_descriptor(
        &mut self,
        c: &Capture,
        id: i64,
        heap: i64,
        slot: u32,
        kind: &str,
        desc_str: Option<&str>,
    ) {
        let Some((count, handle)) = self.heap(c, heap).map(|m| (m.count, m.cpu(slot))) else {
            return;
        };
        if slot >= count {
            self.warn(format!(
                "descriptor heap #{heap}: slot {slot} is past its {count} descriptors"
            ));
            return;
        }
        if self
            .bindless_tracer
            .as_ref()
            .and_then(|t| t.heaps.get(&heap))
            .is_some_and(|l| l.log_slot == slot)
        {
            return;
        }
        let dim = desc_str.and_then(|s| {
            let rest = s.split_once("\"dimension\":")?.1;
            let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
            digits.parse::<i32>().ok()
        });
        unsafe {
            match kind {
                "cb" => {
                    let d = D3D12_CONSTANT_BUFFER_VIEW_DESC::default();
                    self.device.CreateConstantBufferView(Some(&d), handle);
                }
                "srv" => {
                    let dim = match dim {
                        Some(d @ 1..=10) => D3D12_SRV_DIMENSION(d),
                        _ => D3D12_SRV_DIMENSION_TEXTURE2D,
                    };
                    let d = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        ViewDimension: dim,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        ..Default::default()
                    };
                    self.device.CreateShaderResourceView(None, Some(&d), handle);
                }
                "uav" => {
                    let dim = match dim {
                        Some(d @ 1..=8) if d != 6 && d != 7 => D3D12_UAV_DIMENSION(d),
                        _ => D3D12_UAV_DIMENSION_BUFFER,
                    };
                    let d = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                        Format: DXGI_FORMAT_R32_UINT,
                        ViewDimension: dim,
                        ..Default::default()
                    };
                    self.device
                        .CreateUnorderedAccessView(None, None, Some(&d), handle);
                }
                _ => {}
            }
        }
        self.versions.insert((heap, slot), id);
    }

    /// Writes bytes into a buffer on an upload or readback heap through its mapping.
    fn map_write(&mut self, id: i64, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let m = self.made(id)?;
        if offset + bytes.len() as u64 > m.desc.Width {
            return Err(format!(
                "#{id}: {} bytes at {offset} run past its {} bytes",
                bytes.len(),
                m.desc.Width
            ));
        }
        let mut p = core::ptr::null_mut();
        unsafe {
            m.resource
                .Map(0, Some(&D3D12_RANGE::default()), Some(&mut p))
                .map_err(|e| format!("#{id} Map: {e}"))?;
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>().add(offset as usize), bytes.len());
            m.resource.Unmap(0, None);
        }
        self.resources.get_mut(&id).expect("made").from = None;
        Ok(())
    }

    /// The mirror of the game's heap `id`, made on first use; None for a heap shaders do not see.
    fn heap(&mut self, c: &Capture, id: i64) -> Option<&Mirror> {
        if !self.heaps.contains_key(&id) {
            let desc: Option<Value> = c
                .row(
                    "SELECT description FROM d12_objects WHERE id = ?1 AND kind = 'ID3D12DescriptorHeap'",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .and_then(|d| serde_json::from_str(&d).ok());
            let made = match desc {
                Some(d) if d["flags"].as_u64().unwrap_or(0) & 1 != 0 => {
                    let kind = D3D12_DESCRIPTOR_HEAP_TYPE(d["type"].as_i64().unwrap_or(0) as i32);
                    let count = d["count"].as_u64().unwrap_or(0) as u32;
                    let tracing_cbv =
                        self.bindless_tracer.is_some() && kind == D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
                    let (mirror_count, trace_slot) = if tracing_cbv {
                        const D3D12_MAX_SHADER_VISIBLE_CBV_SRV_UAV: u32 = 1_000_000;
                        if count < D3D12_MAX_SHADER_VISIBLE_CBV_SRV_UAV {
                            (count + 1, Some(count))
                        } else {
                            (count, Some(count - 1))
                        }
                    } else {
                        (count, None)
                    };
                    match mirror(&self.device, kind, mirror_count, true) {
                        Ok(m) => {
                            if let Some(log_slot) = trace_slot {
                                match self.make_bindless_log(log_slot, m.cpu(log_slot)) {
                                    Ok(log) => {
                                        if let Some(t) = &mut self.bindless_tracer {
                                            t.heaps.insert(id, log);
                                        }
                                    }
                                    Err(e) => {
                                        self.warn(format!("bindless trace buffer for heap #{id}: {e}"));
                                    }
                                }
                            }
                            Some(m)
                        }
                        Err(e) => {
                            self.warn(format!(
                                "descriptor heap #{id} ({count} descriptors) could not be made: {e}"
                            ));
                            None
                        }
                    }
                }
                Some(_) => None,
                None => {
                    self.warn(format!("descriptor heap #{id} is not described in the capture"));
                    None
                }
            };
            self.heaps.insert(id, made);
        }
        self.heaps[&id].as_ref()
    }

    /// Allocates the 2-region bitmask `RWByteAddressBuffer` (plus reusable zero-upload and readback buffers) for
    /// runtime bindless slot tracing and binds its raw UAV descriptor at `uav_handle`.
    fn make_bindless_log(
        &self,
        log_slot: u32,
        uav_handle: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) -> Result<BindlessHeapLog, String> {
        let bytes = crate::dxil::bindless_log_bytes(log_slot);
        let mk = |heap_type: D3D12_HEAP_TYPE,
                  flags: D3D12_RESOURCE_FLAGS,
                  state: D3D12_RESOURCE_STATES|
         -> Result<ID3D12Resource, String> {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: bytes,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: flags,
                ..Default::default()
            };
            let mut r: Option<ID3D12Resource> = None;
            unsafe {
                self.device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: heap_type,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    state,
                    None,
                    &mut r,
                )
            }
            .map_err(|e| format!("bindless log buffer ({bytes} bytes): {e}"))?;
            r.ok_or_else(|| "bindless log buffer: no resource".into())
        };
        let gpu_buf = mk(
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            COMMON,
        )?;
        let zero_buf = mk(
            D3D12_HEAP_TYPE_UPLOAD,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_GENERIC_READ,
        )?;
        let readback_buf = mk(
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )?;
        unsafe {
            let mut zp = core::ptr::null_mut();
            zero_buf
                .Map(0, Some(&D3D12_RANGE::default()), Some(&mut zp))
                .map_err(|e| format!("zero_buf Map: {e}"))?;
            core::ptr::write_bytes(zp.cast::<u8>(), 0, bytes as usize);
            zero_buf.Unmap(0, None);
        }
        let mut rp = core::ptr::null_mut();
        unsafe {
            readback_buf
                .Map(0, Some(&D3D12_RANGE::default()), Some(&mut rp))
                .map_err(|e| format!("readback_buf Map: {e}"))?;
        }
        let uav_desc = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: (bytes / 4) as u32,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        unsafe {
            self.device
                .CreateUnorderedAccessView(&gpu_buf, None, Some(&uav_desc), uav_handle);
        }
        Ok(BindlessHeapLog {
            log_slot,
            max_slots: log_slot,
            gpu_buf,
            zero_buf,
            readback_buf,
            readback_ptr: rp.cast_const().cast(),
            bytes,
        })
    }

    /// Writes a recorded descriptor into its slot of the mirror of its heap.
    fn descriptor(&mut self, c: &Capture, v: &Version) {
        let Some((count, handle)) = self.heap(c, v.heap).map(|m| (m.count, m.cpu(v.slot))) else {
            return;
        };
        if v.slot >= count {
            self.warn(format!(
                "descriptor heap #{}: slot {} is past its {count} descriptors",
                v.heap, v.slot
            ));
            return;
        }
        if self
            .bindless_tracer
            .as_ref()
            .and_then(|t| t.heaps.get(&v.heap))
            .is_some_and(|l| l.log_slot == v.slot)
        {
            return;
        }
        if v.kind == "srv" && v.description["dimension"] == 11 {
            self.structure_view(c, &v.description, handle);
        } else {
            self.view(&v.kind, v.resource, v.counter, &v.description, handle);
        }
        self.versions.insert((v.heap, v.slot), v.id);
    }

    /// Makes a view at `handle` from its recorded definition. A view of a resource replay did not make (the frame,
    /// or the part replayed, does not use it) is made null.
    fn view(
        &mut self,
        kind: &str,
        resource: i64,
        counter: i64,
        desc: &Value,
        handle: D3D12_CPU_DESCRIPTOR_HANDLE,
    ) {
        let r = self.resources.get(&resource).map(|m| m.resource.clone());
        let default = views::is_default(desc);
        unsafe {
            match kind {
                // Made where they are written (`descriptor`), naming replay's storage.
                "srv" if desc["dimension"] == 11 => {
                    self.warn("acceleration structure views a call names by definition are not replayed");
                }
                "srv" if default => {
                    if let Some(r) = &r {
                        self.device.CreateShaderResourceView(r, None, handle)
                    }
                }
                "srv" => {
                    let mut d = views::srv(desc);
                    if let Some(m) = self.resources.get(&resource)
                        && m.buffer()
                        && m.desc.Width < m.recorded_width
                        && d.ViewDimension == D3D12_SRV_DIMENSION_BUFFER
                    {
                        let b = &mut d.Anonymous.Buffer;
                        let stride = if b.StructureByteStride > 0 {
                            u64::from(b.StructureByteStride)
                        } else if b.Flags.contains(D3D12_BUFFER_SRV_FLAG_RAW) {
                            4
                        } else {
                            u64::from(
                                locum_capture::format::layout(d.Format.0 as u32).map_or(4, |l| l.block_bytes),
                            )
                        };
                        let avail = m.desc.Width / stride.max(1);
                        let max_elems = avail.saturating_sub(b.FirstElement).min(u64::from(u32::MAX)) as u32;
                        b.NumElements = b.NumElements.min(max_elems);
                    }
                    self.device.CreateShaderResourceView(r.as_ref(), Some(&d), handle)
                }
                "uav" => {
                    let counter = self.resources.get(&counter).map(|m| m.resource.clone());
                    if default {
                        if let Some(r) = &r {
                            self.device
                                .CreateUnorderedAccessView(r, counter.as_ref(), None, handle)
                        }
                    } else {
                        let mut d = views::uav(desc);
                        if let Some(m) = self.resources.get(&resource)
                            && m.buffer()
                            && m.desc.Width < m.recorded_width
                            && d.ViewDimension == D3D12_UAV_DIMENSION_BUFFER
                        {
                            let b = &mut d.Anonymous.Buffer;
                            let stride = if b.StructureByteStride > 0 {
                                u64::from(b.StructureByteStride)
                            } else if b.Flags.contains(D3D12_BUFFER_UAV_FLAG_RAW) {
                                4
                            } else {
                                u64::from(
                                    locum_capture::format::layout(d.Format.0 as u32)
                                        .map_or(4, |l| l.block_bytes),
                                )
                            };
                            let avail = m.desc.Width / stride.max(1);
                            let max_elems =
                                avail.saturating_sub(b.FirstElement).min(u64::from(u32::MAX)) as u32;
                            b.NumElements = b.NumElements.min(max_elems);
                        }
                        self.device
                            .CreateUnorderedAccessView(r.as_ref(), counter.as_ref(), Some(&d), handle)
                    }
                }
                "cb" => {
                    let d = match views::cbv(desc) {
                        Some((buffer, offset, bytes)) => D3D12_CONSTANT_BUFFER_VIEW_DESC {
                            BufferLocation: self.address(buffer, offset),
                            SizeInBytes: match self.resources.get(&buffer) {
                                Some(m) => {
                                    let avail = m.desc.Width.saturating_sub(offset);
                                    (u64::from(bytes)).min(avail) as u32
                                }
                                None => 0,
                            },
                        },
                        None => D3D12_CONSTANT_BUFFER_VIEW_DESC::default(),
                    };
                    self.device.CreateConstantBufferView(Some(&d), handle)
                }
                "sampler" => self.device.CreateSampler(&views::sampler(desc), handle),
                "rtv" if default => self.device.CreateRenderTargetView(r.as_ref(), None, handle),
                "rtv" => self
                    .device
                    .CreateRenderTargetView(r.as_ref(), Some(&views::rtv(desc)), handle),
                "dsv" if default => self.device.CreateDepthStencilView(r.as_ref(), None, handle),
                "dsv" => self
                    .device
                    .CreateDepthStencilView(r.as_ref(), Some(&views::dsv(desc)), handle),
                // A slot the game left without a descriptor: whatever it held stays.
                "empty" => {}
                other => self.warn(format!("descriptors of kind {other} are not replayed")),
            }
        }
    }

    /// The subresources a view definition covers in its resource (none for one replay did not make or does not
    /// track).
    fn covered(&self, kind: &str, resource: i64, desc: &Value) -> Vec<u32> {
        match self.resources.get(&resource) {
            Some(m) if m.tracked => views::subresources(kind, desc, m.extent),
            _ => Vec::new(),
        }
    }

    fn root(&mut self, c: &Capture, id: i64) -> Option<&Root> {
        if !self.roots.contains_key(&id) {
            let row: Option<(String, Option<String>)> = c.row(
                "SELECT blob, decoded_json FROM d12_roots WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            );
            let made = match row {
                Some((key, decoded)) => match c.blob(&key).and_then(|blob| {
                    unsafe { self.device.CreateRootSignature::<ID3D12RootSignature>(0, &blob) }
                        .map_err(|e| e.to_string())
                }) {
                    Ok(signature) => Some(Root {
                        signature,
                        decoded: decoded
                            .and_then(|d| serde_json::from_str(&d).ok())
                            .unwrap_or_default(),
                    }),
                    Err(e) => {
                        self.warn(format!("root signature #{id} could not be made: {e}"));
                        None
                    }
                },
                None => {
                    self.warn(format!("root signature #{id} is not in the capture"));
                    None
                }
            };
            self.roots.insert(id, made);
        }
        self.roots[&id].as_ref()
    }

    /// Pipeline state `id`, with the shaders `replaced` (subobject type, code) the rules named by `texts` put in.
    fn pso(
        &mut self,
        c: &Capture,
        id: i64,
        texts: Vec<String>,
        replaced: &[(u32, Vec<u8>)],
    ) -> Option<ID3D12PipelineState> {
        let k = (id, texts);
        if !self.psos.contains_key(&k) {
            let desc: Option<Value> = c
                .row("SELECT description FROM d12_psos WHERE id = ?1", [id], |r| {
                    r.get::<_, String>(0)
                })
                .and_then(|d| serde_json::from_str(&d).ok());
            let made = match desc {
                Some(desc) => {
                    let root = pipelines::subobjects(&desc)
                        .into_iter()
                        .find(|(t, _)| *t == 0)
                        .and_then(|(_, v)| v.as_i64());
                    match root.and_then(|r| self.root(c, r).map(|r| r.signature.clone())) {
                        Some(root) => match pipelines::make(c, &self.device, &desc, &root, replaced) {
                            Ok(p) => {
                                name(&p, format!("pipeline #{id}"));
                                Some(p)
                            }
                            Err(e) => {
                                self.warn(format!(
                                    "pipeline state #{id} could not be made: {e}{}",
                                    self.validation()
                                ));
                                None
                            }
                        },
                        None => {
                            self.warn(format!(
                                "pipeline state #{id} has no root signature replay could make"
                            ));
                            None
                        }
                    }
                }
                None => {
                    self.warn(format!("pipeline state #{id} is not described in the capture"));
                    None
                }
            };
            self.psos.insert(k.clone(), made);
        }
        self.psos[&k].clone()
    }

    fn signature(&mut self, c: &Capture, id: i64) -> Option<ID3D12CommandSignature> {
        if !self.signatures.contains_key(&id) {
            let desc: Option<Value> = c
                .row(
                    "SELECT description FROM d12_objects WHERE id = ?1 AND kind = 'ID3D12CommandSignature'",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .and_then(|d| serde_json::from_str(&d).ok());
            let made = match desc {
                Some(d) => {
                    let args: Vec<D3D12_INDIRECT_ARGUMENT_DESC> = d["arguments"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|a| {
                            let n = |k: &str| a[k].as_u64().unwrap_or(0) as u32;
                            let root = n("root_parameter");
                            let t = D3D12_INDIRECT_ARGUMENT_TYPE(a["type"].as_i64().unwrap_or(-1) as i32);
                            D3D12_INDIRECT_ARGUMENT_DESC {
                                Type: t,
                                Anonymous: match t {
                                    D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW => {
                                        D3D12_INDIRECT_ARGUMENT_DESC_0 {
                                            VertexBuffer: D3D12_INDIRECT_ARGUMENT_DESC_0_0 {
                                                Slot: n("slot"),
                                            },
                                        }
                                    }
                                    D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT => D3D12_INDIRECT_ARGUMENT_DESC_0 {
                                        Constant: D3D12_INDIRECT_ARGUMENT_DESC_0_1 {
                                            RootParameterIndex: root,
                                            DestOffsetIn32BitValues: n("offset"),
                                            Num32BitValuesToSet: n("count"),
                                        },
                                    },
                                    D3D12_INDIRECT_ARGUMENT_TYPE_INCREMENTING_CONSTANT => {
                                        D3D12_INDIRECT_ARGUMENT_DESC_0 {
                                            IncrementingConstant: D3D12_INDIRECT_ARGUMENT_DESC_0_5 {
                                                RootParameterIndex: root,
                                                DestOffsetIn32BitValues: n("offset"),
                                            },
                                        }
                                    }
                                    D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW
                                    | D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW
                                    | D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW => {
                                        D3D12_INDIRECT_ARGUMENT_DESC_0 {
                                            ConstantBufferView: D3D12_INDIRECT_ARGUMENT_DESC_0_2 {
                                                RootParameterIndex: root,
                                            },
                                        }
                                    }
                                    _ => Default::default(),
                                },
                            }
                        })
                        .collect();
                    // A signature that sets root arguments is made against the root signature it was made with.
                    let sets_root = args.iter().any(|a| (5..=8).contains(&a.Type.0) || a.Type.0 == 11);
                    let root_id = d["root_signature"].as_i64().unwrap_or(0);
                    let root = if sets_root {
                        self.root(c, root_id).map(|r| r.signature.clone())
                    } else {
                        None
                    };
                    if args.is_empty() {
                        self.warn(format!("command signature #{id} has no arguments"));
                        None
                    } else if sets_root && root.is_none() {
                        self.warn(format!(
                            "command signature #{id} sets root arguments of root signature #{root_id}, which replay \
                             could not make"
                        ));
                        None
                    } else {
                        let desc = D3D12_COMMAND_SIGNATURE_DESC {
                            ByteStride: d["stride"].as_u64().unwrap_or(0) as u32,
                            NumArgumentDescs: args.len() as u32,
                            pArgumentDescs: args.as_ptr(),
                            NodeMask: 0,
                        };
                        let mut s: Option<ID3D12CommandSignature> = None;
                        match unsafe { self.device.CreateCommandSignature(&desc, root.as_ref(), &mut s) } {
                            Ok(()) => s,
                            Err(e) => {
                                let tier = if args
                                    .iter()
                                    .any(|a| a.Type == D3D12_INDIRECT_ARGUMENT_TYPE_INCREMENTING_CONSTANT)
                                {
                                    let mut o = D3D12_FEATURE_DATA_D3D12_OPTIONS21::default();
                                    let known = unsafe {
                                        self.device.CheckFeatureSupport(
                                            D3D12_FEATURE_D3D12_OPTIONS21,
                                            (&mut o as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS21).cast(),
                                            size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS21>() as u32,
                                        )
                                    }
                                    .is_ok();
                                    match known {
                                        true if o.ExecuteIndirectTier.0 < D3D12_EXECUTE_INDIRECT_TIER_1_1.0 => format!(
                                            " (it has an incrementing constant, which needs ExecuteIndirect tier 1.1; \
                                             the replay device's is {}.{})",
                                            o.ExecuteIndirectTier.0 / 10,
                                            o.ExecuteIndirectTier.0 % 10
                                        ),
                                        true => String::new(),
                                        false => " (it has an incrementing constant, which needs ExecuteIndirect tier \
                                                  1.1; the replay device's runtime knows no tiers)"
                                            .into(),
                                    }
                                } else {
                                    String::new()
                                };
                                self.warn(format!("command signature #{id} could not be made: {e}{tier}"));
                                None
                            }
                        }
                    }
                }
                None => {
                    self.warn(format!("command signature #{id} is not described in the capture"));
                    None
                }
            };
            self.signatures.insert(id, made);
        }
        self.signatures[&id].clone()
    }

    /// The code a shader rule puts in place, for a pipeline whose shaders are DXIL (`dxil`) or not.
    fn replacement(&mut self, rule: &Rule, dxil: bool) -> Option<Vec<u8>> {
        let key = format!("{dxil} {}", rule.text);
        if !self.replaced.contains_key(&key) {
            let code = match &rule.action {
                Action::Solid(colour) if dxil => crate::dxil::compile(&rules::solid_source(colour), "ps_6_0"),
                Action::Solid(colour) => crate::shaders::compile(&rules::solid_source(colour), c"ps_5_0"),
                Action::Shader(code) => Ok(code.clone()),
                _ => Err("not a shader rule".into()),
            };
            let made = match code {
                Ok(c) => Some(c),
                Err(e) => {
                    self.warn(format!("rule {:?}: {e}", rule.text));
                    None
                }
            };
            self.replaced.insert(key.clone(), made);
        }
        self.replaced[&key].clone()
    }

    /// The depth bounds and shading rate calls of the command list execution event `seq` ran in, before it.
    fn list_calls(&mut self, c: &Capture, seq: i64) -> Vec<(String, Value)> {
        let state = self.list_state.get_or_insert_with(|| {
            let mut by: HashMap<i64, Vec<ListCall>> = HashMap::new();
            for (execution, seq, call, args) in c.rows(
                "SELECT l.execution, e.seq, e.call, coalesce(e.args, 'null') FROM events e \
                 JOIN d12_event_locations l ON l.event = e.seq \
                 WHERE e.call IN ('OMSetDepthBounds', 'RSSetShadingRate', 'RSSetShadingRateImage') ORDER BY e.seq",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)),
            ) {
                by.entry(execution).or_default().push(ListCall {
                    seq,
                    call,
                    args: serde_json::from_str(&args).unwrap_or(Value::Null),
                });
            }
            by
        });
        let execution: Option<i64> = c.row(
            "SELECT execution FROM d12_event_locations WHERE event = ?1",
            [seq],
            |r| r.get(0),
        );
        let Some(calls) = execution.and_then(|x| state.get(&x)) else {
            return Vec::new();
        };
        // The last of each call before the event.
        let mut last: Vec<(String, Value)> = Vec::new();
        for l in calls.iter().filter(|l| l.seq < seq) {
            last.retain(|(c, _)| *c != l.call);
            last.push((l.call.clone(), l.args.clone()));
        }
        last
    }

    /// Runs a draw, dispatch or indirect execution from the state the capture took at it.
    #[allow(clippy::too_many_arguments)]
    fn draw(
        &mut self,
        c: &Capture,
        seq: i64,
        kind: &str,
        call: &str,
        args: &Value,
        rules: &[Rule],
        arguments: indirect::Arguments,
    ) -> Result<(), String> {
        type Head = (i64, [Option<String>; 6]);
        let (n, hashes): Head = c
            .row(
                "SELECT n, vs, hs, ds, gs, ps, cs FROM draws WHERE event = ?1",
                [seq],
                |r| {
                    Ok((
                        r.get(0)?,
                        [r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?],
                    ))
                },
            )
            .ok_or("no draw row")?;
        let (pso_id, graphics_root, compute_root, state): (i64, i64, i64, String) = c
            .row(
                "SELECT pso, graphics_root, compute_root, root_args_json FROM d12_draw_state WHERE draw = ?1",
                [n],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok_or("no draw state recorded")?;
        let state: Value = serde_json::from_str(&state).map_err(|e| e.to_string())?;
        let compute = kind == "dispatch";
        let bound = |stage: usize| {
            hashes[stage]
                .as_deref()
                .and_then(|h| u128::from_str_radix(h, 16).ok())
        };
        let applying: Vec<&Rule> = rules
            .iter()
            .filter(|r| r.applies(compute) && r.holds(bound))
            .collect();
        if applying.iter().any(|r| r.action == Action::Skip) || matches!(arguments, indirect::Arguments::Skip)
        {
            return Ok(());
        }
        // Shader rules, as code for the pipeline's own kind of bytecode.
        let dxil = c
            .row(
                "SELECT blob FROM shaders WHERE hash = ?1",
                [hashes.iter().flatten().next()],
                |r| r.get::<_, Option<String>>(0),
            )
            .flatten()
            .and_then(|k| c.blob(&k).ok())
            .is_some_and(|code| crate::dxil::is_dxil(&code));
        let (mut texts, mut replaced) = (Vec::new(), Vec::new());
        for r in &applying {
            if matches!(r.action, Action::Solid(_) | Action::Shader(_))
                && let Some(code) = self.replacement(r, dxil)
            {
                texts.push(r.text.clone());
                replaced.push((pipelines::shader_type(r.stage), code));
            }
        }
        let root_id = if compute { compute_root } else { graphics_root };
        let Some((root, decoded)) = self
            .root(c, root_id)
            .map(|r| (r.signature.clone(), r.decoded.clone()))
        else {
            self.warn(format!("draws with root signature #{root_id} were not run"));
            return Ok(());
        };
        let directly_indexed = decoded["flags"].as_u64().is_some_and(|f| f & 0x400 != 0);
        let pso_desc: Option<Value> =
            if pso_id != 0 && directly_indexed && (self.warp || self.bindless_tracer.is_some()) {
                c.row("SELECT description FROM d12_psos WHERE id = ?1", [pso_id], |r| {
                    r.get::<_, String>(0)
                })
                .and_then(|d| serde_json::from_str::<Value>(&d).ok())
            } else {
                None
            };
        // Guard against a native d3d10warp.dll access violation / heap corruption (0xc0000005 / 0xc0000374 in
        // d3d10warp.dll) when WARP executes large SM 6.6 directly-indexed shaders under GPU-based validation.
        if self.warp
            && let Some(pso_desc) = &pso_desc
        {
            const WARP_GBV_MAX_BINDLESS_SHADER_BYTES: u64 = 65_536;
            let large = pipelines::subobjects(pso_desc)
                .into_iter()
                .find_map(|(sub_ty, val)| {
                    if !matches!(sub_ty, 1 | 2 | 3 | 4 | 5 | 6 | 24 | 25) {
                        return None;
                    }
                    if replaced.iter().any(|&(t, _)| t == sub_ty) {
                        return None;
                    }
                    let key = val["blob"].as_str()?;
                    let size = val["size"].as_u64().unwrap_or(0);
                    (size >= WARP_GBV_MAX_BINDLESS_SHADER_BYTES).then(|| (key.to_string(), size))
                });
            if let Some((key, size)) = large {
                if self.gbv {
                    self.warn(format!(
                        "draw {n} (event {seq}): skipped on WARP with --debug (GBV): directly-indexed shader \
                         {key} ({size} bytes) triggers an access violation / heap corruption inside d3d10warp.dll \
                         under GPU-based validation; run without --warp on the hardware GPU"
                    ));
                    return Ok(());
                }
                self.large_bindless_shader = Some((key, size));
            }
        }
        // Runtime GPU/WARP bindless slot tracing: if enabled and this draw uses a directly-indexed root signature,
        // instrument its DXIL shaders' `createHandleFromHeap` calls to log into the active CBV/SRV/UAV heap's log slot.
        let mut trace_clear: Option<(i64, u32)> = None;
        if self.bindless_tracer.is_some() && pso_id != 0 && directly_indexed {
            let heap_ids: Vec<i64> = state["descriptor_heaps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_i64)
                .collect();
            let mut active_trace_heap = None;
            for hid in heap_ids {
                let _ = self.heap(c, hid);
                if let Some(log) = self.bindless_tracer.as_ref().and_then(|t| t.heaps.get(&hid)) {
                    active_trace_heap = Some((hid, log.log_slot, log.max_slots));
                    break;
                }
            }
            if let Some((trace_heap, log_slot, max_slots)) = active_trace_heap
                && let Some(pso_desc) = &pso_desc
            {
                let mut patched_any = false;
                for (sub_ty, val) in pipelines::subobjects(pso_desc) {
                    if !matches!(sub_ty, 1 | 2 | 3 | 4 | 5 | 6 | 24 | 25) {
                        continue;
                    }
                    if replaced.iter().any(|&(t, _)| t == sub_ty) {
                        continue;
                    }
                    let Some(key) = val["blob"].as_str() else {
                        continue;
                    };
                    let cache_key = (key.to_string(), log_slot, max_slots);
                    let cached = self
                        .bindless_tracer
                        .as_ref()
                        .and_then(|t| t.patched_shaders.get(&cache_key))
                        .cloned();
                    let patched = match cached {
                        Some(v) => v,
                        None => {
                            let res = c.blob(key).and_then(|code| {
                                crate::dxil::instrument_bindless(&code, log_slot, max_slots)
                            });
                            let entry = match res {
                                Ok(opt) => opt,
                                Err(e) => {
                                    self.warn(format!(
                                        "bindless trace instrumentation skipped for shader {key}: {e}"
                                    ));
                                    None
                                }
                            };
                            if let Some(t) = &mut self.bindless_tracer {
                                t.patched_shaders.insert(cache_key, entry.clone());
                            }
                            entry
                        }
                    };
                    if let Some(bytes) = patched {
                        texts.push(format!("__bindless_trace_{sub_ty}_{log_slot}"));
                        replaced.push((sub_ty, bytes));
                        patched_any = true;
                    }
                }
                if patched_any {
                    trace_clear = Some((trace_heap, max_slots));
                }
            }
        }
        // A raytracing state object (SetPipelineState1) runs a DispatchRays.
        let state_object = state["state_object"].as_i64().unwrap_or(0);
        let pipeline = if compute && pso_id == 0 && state_object != 0 {
            if !texts.is_empty() {
                self.warn("rules that put in shaders do not apply to DispatchRays");
            }
            let Some(object) = self.state_object(c, state_object) else {
                self.warn(format!("dispatches of state object #{state_object} were not run"));
                return Ok(());
            };
            Pipeline::Rays(object)
        } else {
            let Some(pso) = self.pso(c, pso_id, texts, &replaced) else {
                self.warn(format!("draws of pipeline state #{pso_id} were not run"));
                return Ok(());
            };
            Pipeline::State(pso)
        };
        if let Some((trace_heap, max_slots)) = trace_clear
            && let Some(t) = &mut self.bindless_tracer
            && let Some(log) = t.heaps.get(&trace_heap)
        {
            unsafe {
                self.list
                    .CopyBufferRegion(&log.gpu_buf, 0, &log.zero_buf, 0, log.bytes);
                let b = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: borrowed(&log.gpu_buf),
                            Subresource: ALL,
                            StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                            StateAfter: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        }),
                    },
                };
                self.list.ResourceBarrier(&[b]);
            }
            t.pending = Some((n, seq, trace_heap, max_slots));
        }
        // Each root argument as recorded: a view's `value`, or a constants parameter's `words` (null where unset).
        let mut root_args: Vec<(u32, String, Value)> = state
            [if compute { "compute_args" } else { "graphics_args" }]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(k, a)| {
            let kind = a["kind"].as_str()?.to_string();
            let v = if kind == "constants" {
                a["words"].clone()
            } else {
                a["value"].clone()
            };
            Some((k.parse().ok()?, kind, v))
        })
        .collect();
        root_args.sort_by_key(|a| a.0);

        // Constant rules, before anything is recorded: reading a buffer runs a list of its own.
        let mut root_cbvs: HashMap<u32, u64> = HashMap::new();
        for r in &applying {
            let Action::Constant { slot, index, bits } = r.action else {
                continue;
            };
            let Some(at) = pipelines::cb_at(&decoded, r.stage, slot) else {
                self.warn(format!(
                    "rule {:?}: the root signature has no b{slot} for that stage",
                    r.text
                ));
                continue;
            };
            // The buffer and range the register reads, and where to point it instead.
            let (resource, offset, size, table) = match at {
                // Root constants are patched where they are recorded.
                pipelines::CbAt::Constants { parameter, words } => {
                    let at = index as usize * 4;
                    if at + 4 > words as usize {
                        self.warn(format!("rule {:?}: c{index} is past the root constants", r.text));
                        continue;
                    }
                    if !root_args.iter().any(|a| a.0 == parameter) {
                        root_args.push((parameter, "constants".into(), Value::Array(Vec::new())));
                    }
                    let (_, _, v) = root_args
                        .iter_mut()
                        .find(|a| a.0 == parameter)
                        .expect("pushed above");
                    let mut recorded = v.as_array().cloned().unwrap_or_default();
                    recorded.resize(recorded.len().max(at + 4), Value::Null);
                    for (k, b) in bits.iter().enumerate() {
                        recorded[at + k] = Value::from(*b);
                    }
                    *v = Value::Array(recorded);
                    continue;
                }
                pipelines::CbAt::Root(p) => {
                    let Some((_, _, v)) = root_args.iter().find(|a| a.0 == p) else {
                        continue;
                    };
                    let resource = v["resource"].as_i64().unwrap_or(0);
                    let offset = v["offset"].as_u64().unwrap_or(0);
                    let width = self.resources.get(&resource).map_or(0, |m| m.desc.Width);
                    (
                        resource,
                        offset,
                        width.saturating_sub(offset).min(65536) as u32,
                        Err(p),
                    )
                }
                pipelines::CbAt::Table { parameter, offset } => {
                    let Some((_, _, v)) = root_args.iter().find(|a| a.0 == parameter) else {
                        continue;
                    };
                    let heap = v["heap"].as_i64().unwrap_or(0);
                    let slot = v["slot"].as_u64().unwrap_or(0) as u32 + offset;
                    let desc: Option<Value> = self.versions.get(&(heap, slot)).and_then(|id| {
                        c.row(
                            "SELECT description FROM d12_descriptor_versions WHERE id = ?1",
                            [id],
                            |r| r.get::<_, String>(0),
                        )
                        .and_then(|d| serde_json::from_str(&d).ok())
                    });
                    let Some((resource, offset, bytes)) = desc.as_ref().and_then(views::cbv) else {
                        self.warn(format!("rule {:?}: b{slot} has no constant buffer bound", r.text));
                        continue;
                    };
                    (resource, offset, bytes, Ok((heap, slot)))
                }
            };
            let all = self.read(resource)?;
            let start = (offset as usize).min(all.len());
            let mut bytes = all[start..(start + size as usize).min(all.len())].to_vec();
            let at = index as usize * 16;
            if at + 16 > bytes.len() {
                self.warn(format!("rule {:?}: c{index} is past the bound range", r.text));
                continue;
            }
            for (k, b) in bits.iter().enumerate() {
                bytes[at + k * 4..at + k * 4 + 4].copy_from_slice(&b.to_le_bytes());
            }
            let room = (bytes.len() as u64).next_multiple_of(256);
            let (patched, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, room)?;
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len()) };
            let va = unsafe { patched.GetGPUVirtualAddress() };
            match table {
                Err(p) => {
                    root_cbvs.insert(p, va);
                }
                Ok((heap, slot)) => {
                    if let Some(handle) = self.heap(c, heap).map(|m| m.cpu(slot)) {
                        let d = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                            BufferLocation: va,
                            SizeInBytes: room as u32,
                        };
                        unsafe { self.device.CreateConstantBufferView(Some(&d), handle) };
                        self.changed.push((heap, slot));
                    }
                }
            }
        }

        // Apply replay root-view and root-constant overrides (`root <param> cbv|srv|uav|const ...`).
        for r in rules {
            match r.action {
                Action::RootView {
                    parameter,
                    kind,
                    resource,
                    offset,
                    when: rules::Timing::Before(ev),
                } if ev.is_none_or(|e| e == seq) => {
                    let kind_str = match kind {
                        rules::RootKind::Cbv => "ConstantBufferView",
                        rules::RootKind::Srv => "ShaderResourceView",
                        rules::RootKind::Uav => "UnorderedAccessView",
                    };
                    let val = serde_json::json!({"resource": resource, "offset": offset});
                    if let Some(entry) = root_args.iter_mut().find(|a| a.0 == parameter) {
                        entry.1 = kind_str.to_string();
                        entry.2 = val;
                    } else {
                        root_args.push((parameter, kind_str.to_string(), val));
                    }
                }
                Action::RootConst {
                    parameter,
                    index,
                    bits,
                    when: rules::Timing::Before(ev),
                } if ev.is_none_or(|e| e == seq) => {
                    if !root_args.iter().any(|a| a.0 == parameter) {
                        root_args.push((parameter, "constants".into(), Value::Array(Vec::new())));
                    }
                    let (_, kind_s, v) = root_args
                        .iter_mut()
                        .find(|a| a.0 == parameter)
                        .expect("pushed above");
                    *kind_s = "constants".into();
                    let mut words = v.as_array().cloned().unwrap_or_default();
                    let idx = index as usize;
                    if words.len() <= idx {
                        words.resize(idx + 1, Value::Null);
                    }
                    words[idx] = Value::from(bits);
                    *v = Value::Array(words);
                }
                _ => {}
            }
        }
        root_args.sort_by_key(|a| a.0);

        // A root SRV may name an acceleration structure: replay's, built before anything of the event is recorded.
        let mut root_srvs: HashMap<u32, u64> = HashMap::new();
        for (i, kind, v) in &root_args {
            if kind == "ShaderResourceView" {
                let (resource, offset) = (
                    v["resource"].as_i64().unwrap_or(0),
                    v["offset"].as_u64().unwrap_or(0),
                );
                let address = self
                    .view_address(c, resource, offset, true)
                    .unwrap_or_else(|why| {
                        self.warn(format!("a root SRV: {why}"));
                        0
                    });
                root_srvs.insert(*i, address);
            }
        }

        let list = self.list.clone();
        let list4 = list.cast::<ID3D12GraphicsCommandList4>().ok();
        unsafe {
            match (&pipeline, &list4) {
                (Pipeline::State(pso), _) => list.SetPipelineState(pso),
                (Pipeline::Rays(object), Some(list4)) => list4.SetPipelineState1(object),
                (Pipeline::Rays(_), None) => {
                    self.warn("this device's command lists do no raytracing (ID3D12GraphicsCommandList4)");
                    return Ok(());
                }
            }
            let mut heaps = Vec::new();
            for id in state["descriptor_heaps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_i64)
            {
                if let Some(m) = self.heap(c, id) {
                    heaps.push(Some(m.heap.clone()));
                }
            }
            if !heaps.is_empty() {
                list.SetDescriptorHeaps(&heaps);
            }
            if compute {
                list.SetComputeRootSignature(&root);
            } else {
                list.SetGraphicsRootSignature(&root);
            }
            for (i, kind, v) in &root_args {
                let address = self.address(
                    v["resource"].as_i64().unwrap_or(0),
                    v["offset"].as_u64().unwrap_or(0),
                );
                match kind.as_str() {
                    "DescriptorTable" => {
                        let heap = v["heap"].as_i64().unwrap_or(0);
                        let Some(m) = self.heap(c, heap) else {
                            self.warn(format!("root tables of heap #{heap} are not bound"));
                            continue;
                        };
                        let handle = m.gpu(v["slot"].as_u64().unwrap_or(0) as u32);
                        if compute {
                            list.SetComputeRootDescriptorTable(*i, handle);
                        } else {
                            list.SetGraphicsRootDescriptorTable(*i, handle);
                        }
                    }
                    "ConstantBufferView" => {
                        let va = root_cbvs.get(i).copied().unwrap_or(address);
                        if compute {
                            list.SetComputeRootConstantBufferView(*i, va);
                        } else {
                            list.SetGraphicsRootConstantBufferView(*i, va);
                        }
                    }
                    "ShaderResourceView" => {
                        let address = root_srvs.get(i).copied().unwrap_or(address);
                        if compute {
                            list.SetComputeRootShaderResourceView(*i, address);
                        } else {
                            list.SetGraphicsRootShaderResourceView(*i, address);
                        }
                    }
                    "UnorderedAccessView" => {
                        if compute {
                            list.SetComputeRootUnorderedAccessView(*i, address);
                        } else {
                            list.SetGraphicsRootUnorderedAccessView(*i, address);
                        }
                    }
                    // Each run of words the list set; words it never set are undefined, as they were for the game.
                    "constants" => {
                        let words: Vec<Option<u32>> = v
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|w| w.as_u64().map(|w| w as u32))
                            .collect();
                        let mut at = 0;
                        while at < words.len() {
                            let run: Vec<u32> = words[at..].iter().map_while(|w| *w).collect();
                            if !run.is_empty() {
                                let p = run.as_ptr().cast();
                                if compute {
                                    list.SetComputeRoot32BitConstants(*i, run.len() as u32, p, at as u32);
                                } else {
                                    list.SetGraphicsRoot32BitConstants(*i, run.len() as u32, p, at as u32);
                                }
                            }
                            at += run.len().max(1);
                        }
                    }
                    other => self.warn(format!("root arguments of kind {other} are not replayed")),
                }
            }
        }
        let mut needs = Vec::new();
        // What the draw's registers read and write, of the tracked textures.
        for (kind, resource, json) in c.rows(
            "SELECT b.kind, b.resource, v.json FROM bindings b LEFT JOIN views v ON v.id = b.view \
             WHERE b.draw = ?1 AND b.resource IS NOT NULL AND b.kind IN ('srv', 'uav')",
            [n],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        ) {
            let native = json
                .and_then(|j| serde_json::from_str::<Value>(&j).ok())
                .map(|j| {
                    if j["default"] == true {
                        Value::Null
                    } else {
                        j["native"].clone()
                    }
                })
                .unwrap_or(Value::Null);
            let subs = self.covered(&kind, resource, &native);
            if !subs.is_empty() {
                needs.push(Need {
                    resource,
                    subs,
                    state: if kind == "srv" { SHADER_READ } else { 0x8 },
                });
            }
        }
        if !compute {
            self.graphics(c, seq, &state["graphics"], &mut needs)?;
        }
        self.barriers(&needs, false);
        let a = |i: usize| u(args, i);
        if let Some(t) = &mut self.timing {
            let program = hashes[5].as_ref().or(hashes[4].as_ref()).or(hashes[0].as_ref());
            t.begin(
                &self.list,
                format!("{seq}	{n}	{call}	{args}	{}", program.map_or("-", |h| h.as_str())),
            );
        }
        unsafe {
            match call {
                "DrawInstanced" => {
                    self.list
                        .DrawInstanced(a(0) as u32, a(1) as u32, a(2) as u32, a(3) as u32)
                }
                "DrawIndexedInstanced" => self.list.DrawIndexedInstanced(
                    a(0) as u32,
                    a(1) as u32,
                    a(2) as u32,
                    args[3].as_i64().unwrap_or(0) as i32,
                    a(4) as u32,
                ),
                "Dispatch" => self.list.Dispatch(a(0) as u32, a(1) as u32, a(2) as u32),
                // One not replayed has a warning saying why.
                "DispatchRays" => {
                    if let (indirect::Arguments::Rays(desc), Some(list4)) = (&arguments, &list4) {
                        list4.DispatchRays(desc)
                    }
                }
                "ExecuteIndirect" => {
                    let signature = self.signature(c, a(0) as i64);
                    let (Some(signature), false) =
                        (signature, matches!(arguments, indirect::Arguments::Skip))
                    else {
                        if let Some(t) = &mut self.timing {
                            t.end(&self.list);
                        }
                        self.barriers(&needs, true);
                        return Ok(());
                    };
                    // Patched records begin at the copy's start.
                    let (buffer, offset) = match arguments {
                        indirect::Arguments::Patched(copy) => (copy, 0),
                        _ => (self.resource(a(2) as i64)?, a(3)),
                    };
                    let count = match a(4) {
                        0 => None,
                        id => Some(self.resource(id as i64)?),
                    };
                    self.list
                        .ExecuteIndirect(&signature, a(1) as u32, &buffer, offset, count.as_ref(), a(5));
                }
                other => self.warn(format!("{other} is not replayed")),
            }
        }
        if let Some(t) = &mut self.timing {
            t.end(&self.list);
        }
        self.barriers(&needs, true);
        Ok(())
    }

    /// A draw's input assembly, targets, viewports and output merger state, and the transitions its targets need.
    fn graphics(&mut self, c: &Capture, seq: i64, g: &Value, needs: &mut Vec<Need>) -> Result<(), String> {
        let list = self.list.clone();
        unsafe {
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(g["topology"].as_i64().unwrap_or(0) as i32));
            let slots: Vec<(u32, &Value)> = g["vertex"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(k, v)| Some((k.parse().ok()?, v)))
                .collect();
            if let Some(count) = slots.iter().map(|s| s.0 + 1).max() {
                let mut vbs = vec![D3D12_VERTEX_BUFFER_VIEW::default(); count as usize];
                for (slot, v) in slots {
                    vbs[slot as usize] = D3D12_VERTEX_BUFFER_VIEW {
                        BufferLocation: self.address(
                            v["buffer"]["resource"].as_i64().unwrap_or(0),
                            v["buffer"]["offset"].as_u64().unwrap_or(0),
                        ),
                        SizeInBytes: v["bytes"].as_u64().unwrap_or(0) as u32,
                        StrideInBytes: v["stride"].as_u64().unwrap_or(0) as u32,
                    };
                }
                list.IASetVertexBuffers(0, Some(&vbs));
            }
            let ib = &g["index"];
            if ib.is_object() {
                list.IASetIndexBuffer(Some(&D3D12_INDEX_BUFFER_VIEW {
                    BufferLocation: self.address(
                        ib["buffer"]["resource"].as_i64().unwrap_or(0),
                        ib["buffer"]["offset"].as_u64().unwrap_or(0),
                    ),
                    SizeInBytes: ib["bytes"].as_u64().unwrap_or(0) as u32,
                    Format: DXGI_FORMAT(ib["format"].as_i64().unwrap_or(0) as i32),
                }));
            }
            if g["stream_output"].as_object().is_some_and(|o| !o.is_empty()) {
                self.warn("stream output targets are not replayed");
            }
            // Targets, made from their definitions in replay's own heaps.
            let mut handles = Vec::new();
            for (i, t) in g["rtvs"].as_array().into_iter().flatten().take(8).enumerate() {
                let handle = self.rtvs.cpu(i as u32);
                let d = &t["definition"];
                let resource = d["resource"].as_i64().unwrap_or(0);
                if d.is_object() && self.resources.contains_key(&resource) {
                    self.view("rtv", resource, 0, &d["description"], handle);
                    needs.push(Need {
                        resource,
                        subs: self.covered("rtv", resource, &d["description"]),
                        state: 0x4,
                    });
                } else {
                    // A null target: writes to it are dropped.
                    let null = D3D12_RENDER_TARGET_VIEW_DESC {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2D,
                        ..Default::default()
                    };
                    self.device.CreateRenderTargetView(None, Some(&null), handle);
                }
                handles.push(handle);
            }
            let dsv = &g["dsv"]["definition"];
            let depth = if dsv.is_object() {
                let resource = dsv["resource"].as_i64().unwrap_or(0);
                let handle = self.dsvs.cpu(0);
                self.view("dsv", resource, 0, &dsv["description"], handle);
                let flags = dsv["description"]["flags"].as_u64().unwrap_or(0);
                let subs = self.covered("dsv", resource, &dsv["description"]);
                if let Some(m) = self.resources.get(&resource) {
                    // Plane 0 is depth, plane 1 stencil; a read-only flag leaves its plane readable.
                    let per_plane = m.extent.mips * m.extent.slices;
                    for (plane, read_only) in [(0, flags & 1 != 0), (1, flags & 2 != 0)] {
                        let subs: Vec<u32> =
                            subs.iter().copied().filter(|s| s / per_plane == plane).collect();
                        if !subs.is_empty() {
                            needs.push(Need {
                                resource,
                                subs,
                                state: if read_only { DEPTH_READ } else { 0x10 },
                            });
                        }
                    }
                }
                Some(handle)
            } else {
                None
            };
            list.OMSetRenderTargets(
                handles.len() as u32,
                (!handles.is_empty()).then_some(handles.as_ptr()),
                false,
                depth.as_ref().map(|h| h as *const _),
            );
            let viewports: Vec<D3D12_VIEWPORT> = g["viewports"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|v| {
                    let f = |i: usize| v[i].as_f64().unwrap_or(0.0) as f32;
                    D3D12_VIEWPORT {
                        TopLeftX: f(0),
                        TopLeftY: f(1),
                        Width: f(2),
                        Height: f(3),
                        MinDepth: f(4),
                        MaxDepth: f(5),
                    }
                })
                .collect();
            if !viewports.is_empty() {
                list.RSSetViewports(&viewports);
            }
            let scissors = rects(&g["scissors"]);
            if !scissors.is_empty() {
                list.RSSetScissorRects(&scissors);
            }
            if g["blend_factor"].is_array() {
                list.OMSetBlendFactor(Some(&floats(&g["blend_factor"])));
            }
            list.OMSetStencilRef(g["stencil_ref"].as_u64().unwrap_or(0) as u32);
            if !g["predicate"].is_null() {
                self.warn("predicated draws are run unpredicated");
            }
        }
        for (call, a) in self.list_calls(c, seq) {
            unsafe {
                match call.as_str() {
                    "OMSetDepthBounds" => match list.cast::<ID3D12GraphicsCommandList1>() {
                        Ok(l) => {
                            let min_d = a
                                .get(2)
                                .and_then(Value::as_u64)
                                .map(|b| f32::from_bits(b as u32))
                                .unwrap_or_else(|| a[0].as_f64().unwrap_or(0.0) as f32);
                            let max_d = a
                                .get(3)
                                .and_then(Value::as_u64)
                                .map(|b| f32::from_bits(b as u32))
                                .unwrap_or_else(|| a[1].as_f64().unwrap_or(1.0) as f32);
                            l.OMSetDepthBounds(min_d, max_d);
                        }
                        Err(_) => self
                            .warn("the device has no depth bounds test; draws run without the game's bounds"),
                    },
                    "RSSetShadingRate" => match list.cast::<ID3D12GraphicsCommandList5>() {
                        Ok(l) => {
                            let combiners: Vec<D3D12_SHADING_RATE_COMBINER> = a[1]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .map(|x| D3D12_SHADING_RATE_COMBINER(x.as_i64().unwrap_or(0) as i32))
                                .collect();
                            l.RSSetShadingRate(
                                D3D12_SHADING_RATE(a[0].as_i64().unwrap_or(0) as i32),
                                (combiners.len() == 2).then_some(combiners.as_ptr()),
                            );
                        }
                        Err(_) => {
                            self.warn("the device has no variable rate shading; draws run at full rate")
                        }
                    },
                    "RSSetShadingRateImage" => match list.cast::<ID3D12GraphicsCommandList5>() {
                        Ok(l) => {
                            let id = a[0].as_i64().unwrap_or(0);
                            let image = self.resources.get(&id).map(|m| m.resource.clone());
                            l.RSSetShadingRateImage(image.as_ref());
                            if image.is_some() {
                                needs.push(self.need_all(id, 0x1000000));
                            }
                        }
                        Err(_) => {
                            self.warn("the device has no variable rate shading; draws run at full rate")
                        }
                    },
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn location(&self, v: &Value) -> Result<D3D12_TEXTURE_COPY_LOCATION, String> {
        let r = self.made(v["resource"].as_i64().unwrap_or(0))?;
        let f = &v["footprint"];
        Ok(if f.is_object() {
            D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrowed(&r.resource),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: f["offset"].as_u64().unwrap_or(0),
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT(f["format"].as_i64().unwrap_or(0) as i32),
                            Width: f["width"].as_u64().unwrap_or(0) as u32,
                            Height: f["height"].as_u64().unwrap_or(0) as u32,
                            Depth: f["depth"].as_u64().unwrap_or(0) as u32,
                            RowPitch: f["row_pitch"].as_u64().unwrap_or(0) as u32,
                        },
                    },
                },
            }
        } else {
            D3D12_TEXTURE_COPY_LOCATION {
                pResource: borrowed(&r.resource),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: v["subresource"].as_u64().unwrap_or(0) as u32,
                },
            }
        })
    }

    /// A transition of one subresource (or all) of a texture a copy reads or writes.
    fn need(&self, id: i64, sub: Option<u32>, state: i32) -> Need {
        match sub {
            Some(s) => Need {
                resource: id,
                subs: vec![s],
                state,
            },
            None => self.need_all(id, state),
        }
    }

    fn copy(&mut self, call: &str, a: &Value) -> Result<(), String> {
        let list = self.list.clone();
        unsafe {
            match call {
                "CopyBufferRegion" => {
                    let (dst_id, dst_off) = (u(a, 0) as i64, u(a, 1));
                    let (src_id, src_off) = (u(a, 2) as i64, u(a, 3));
                    let len = u(a, 4);
                    self.copy_buffer_to_buffer(dst_id, dst_off, src_id, src_off, len)?;
                }
                "CopyResource" => {
                    let (d, s) = (u(a, 0) as i64, u(a, 1) as i64);
                    let (dst, src) = (self.resource(d)?, self.resource(s)?);
                    let needs = [self.need(d, None, 0x400), self.need(s, None, 0x800)];
                    self.barriers(&needs, false);
                    list.CopyResource(&dst, &src);
                    self.barriers(&needs, true);
                }
                "CopyTextureRegion" => {
                    let (dst, src) = (self.location(&a[0])?, self.location(&a[4])?);
                    let side = |v: &Value, state| {
                        let id = v["resource"].as_i64().unwrap_or(0);
                        v["footprint"]
                            .is_null()
                            .then(|| self.need(id, v["subresource"].as_u64().map(|s| s as u32), state))
                    };
                    let needs: Vec<Need> = [side(&a[0], 0x400), side(&a[4], 0x800)]
                        .into_iter()
                        .flatten()
                        .collect();
                    let region = a[5].as_array().map(|b| {
                        let n = |i: usize| b[i].as_u64().unwrap_or(0) as u32;
                        D3D12_BOX {
                            left: n(0),
                            top: n(1),
                            front: n(2),
                            right: n(3),
                            bottom: n(4),
                            back: n(5),
                        }
                    });
                    self.barriers(&needs, false);
                    list.CopyTextureRegion(
                        &dst,
                        u(a, 1) as u32,
                        u(a, 2) as u32,
                        u(a, 3) as u32,
                        &src,
                        region.as_ref().map(|b| b as *const _),
                    );
                    self.barriers(&needs, true);
                }
                "ResolveSubresource" => {
                    let (d, s) = (u(a, 0) as i64, u(a, 2) as i64);
                    let (dst, src) = (self.resource(d)?, self.resource(s)?);
                    let needs = [
                        self.need(d, Some(u(a, 1) as u32), 0x1000),
                        self.need(s, Some(u(a, 3) as u32), 0x2000),
                    ];
                    self.barriers(&needs, false);
                    list.ResolveSubresource(
                        &dst,
                        u(a, 1) as u32,
                        &src,
                        u(a, 3) as u32,
                        DXGI_FORMAT(u(a, 4) as i32),
                    );
                    self.barriers(&needs, true);
                }
                other => self.warn(format!("{other} is not replayed")),
            }
        }
        Ok(())
    }

    fn clear(&mut self, c: &Capture, seq: i64, call: &str, a: &Value, rules: &[Rule]) -> Result<(), String> {
        let list = self.list.clone();
        let skip_for = |resource: i64| {
            rules.iter().any(|r| {
                matches!(
                    r.action,
                    Action::SkipClear {
                        resource: target,
                        event,
                    } if target.is_none_or(|id| id == resource) && event.is_none_or(|ev| ev == seq)
                )
            })
        };
        match call {
            "ClearRenderTargetView" => {
                let d = &a[0]["definition"];
                let resource = d["resource"].as_i64().unwrap_or(0);
                if !self.resources.contains_key(&resource) || skip_for(resource) {
                    return Ok(());
                }
                let mut color = floats_exact(&a[1], a.get(3));
                for r in rules {
                    match r.action {
                        Action::ClearColor {
                            resource: id,
                            color: cval,
                            when,
                        } if id == resource
                            && matches!(when, rules::Timing::Before(ev) if ev.is_none_or(|e| e == seq)) =>
                        {
                            color = cval;
                        }
                        Action::ClearBits {
                            resource: target,
                            bits,
                            count,
                            when,
                        } if target.is_none_or(|id| id == resource)
                            && matches!(when, rules::Timing::Before(ev) if ev.is_none_or(|e| e == seq)) =>
                        {
                            for i in 0..4 {
                                let b = if (i as u8) < count { bits[i] } else { bits[0] };
                                color[i] = f32::from_bits(b);
                            }
                        }
                        _ => {}
                    }
                }
                let handle = self.rtvs.cpu(0);
                self.view("rtv", resource, 0, &d["description"], handle);
                let needs = [Need {
                    resource,
                    subs: self.covered("rtv", resource, &d["description"]),
                    state: 0x4,
                }];
                let r = rects(&a[2]);
                self.barriers(&needs, false);
                unsafe { list.ClearRenderTargetView(handle, &color, (!r.is_empty()).then_some(&r[..])) };
                self.barriers(&needs, true);
            }
            "ClearDepthStencilView" => {
                let d = &a[0]["definition"];
                let resource = d["resource"].as_i64().unwrap_or(0);
                if !self.resources.contains_key(&resource) || skip_for(resource) {
                    return Ok(());
                }
                let mut depth = a
                    .get(5)
                    .and_then(Value::as_u64)
                    .map(|b| f32::from_bits(b as u32))
                    .unwrap_or_else(|| a[2].as_f64().unwrap_or(0.0) as f32);
                let mut stencil = u(a, 3) as u8;
                for r in rules {
                    match r.action {
                        Action::ClearDepth {
                            resource: id,
                            depth: dval,
                            stencil: sval,
                            when,
                        } if id == resource
                            && matches!(when, rules::Timing::Before(ev) if ev.is_none_or(|e| e == seq)) =>
                        {
                            depth = dval;
                            if let Some(s) = sval {
                                stencil = s;
                            }
                        }
                        Action::ClearBits {
                            resource: target,
                            bits,
                            count,
                            when,
                        } if target.is_none_or(|id| id == resource)
                            && matches!(when, rules::Timing::Before(ev) if ev.is_none_or(|e| e == seq)) =>
                        {
                            depth = f32::from_bits(bits[0]);
                            if count >= 2 {
                                stencil = bits[1] as u8;
                            }
                        }
                        _ => {}
                    }
                }
                let handle = self.dsvs.cpu(0);
                self.view("dsv", resource, 0, &d["description"], handle);
                let needs = [Need {
                    resource,
                    subs: self.covered("dsv", resource, &d["description"]),
                    state: 0x10,
                }];
                let r = rects(&a[4]);
                self.barriers(&needs, false);
                unsafe {
                    list.ClearDepthStencilView(
                        handle,
                        D3D12_CLEAR_FLAGS(u(a, 1) as i32),
                        depth,
                        stencil,
                        (!r.is_empty()).then_some(&r[..]),
                    )
                };
                self.barriers(&needs, true);
            }
            "ClearUnorderedAccessViewUint" | "ClearUnorderedAccessViewFloat" => {
                let resource = u(a, 2) as i64;
                if skip_for(resource) {
                    return Ok(());
                }
                let Ok(target) = self.resource(resource) else {
                    return Ok(());
                };
                let (heap, slot) = (
                    a[0]["heap"].as_i64().unwrap_or(0),
                    a[0]["slot"].as_u64().unwrap_or(0) as u32,
                );
                let Some((visible, gpu)) = self.heap(c, heap).map(|m| (m.heap.clone(), m.gpu(slot))) else {
                    self.warn(format!(
                        "a UAV clear names heap #{heap}, which replay has no mirror of"
                    ));
                    return Ok(());
                };
                // The CPU handle's view: its definition, or the shader-visible slot's.
                let d = &a[1]["definition"];
                let (kind, view_resource, counter, desc) = if d.is_object() {
                    (
                        d["kind"].as_str().unwrap_or("uav").to_string(),
                        d["resource"].as_i64().unwrap_or(0),
                        d["counter"].as_i64().unwrap_or(0),
                        d["description"].clone(),
                    )
                } else {
                    let v = self.versions.get(&(heap, slot)).and_then(|id| {
                        c.row(
                            &format!(
                                "SELECT {VERSION_COLUMNS} FROM d12_descriptor_versions v WHERE v.id = ?1"
                            ),
                            [id],
                            version,
                        )
                    });
                    let Some(v) = v else {
                        self.warn("a UAV clear's view was not recorded");
                        return Ok(());
                    };
                    (v.kind, v.resource, v.counter, v.description)
                };
                let handle = self.cpu_views.cpu(0);
                self.view(&kind, view_resource, counter, &desc, handle);
                let needs = [Need {
                    resource,
                    subs: self.covered("uav", resource, &desc),
                    state: 0x8,
                }];
                let r = rects(&a[4]);
                self.barriers(&needs, false);
                unsafe {
                    list.SetDescriptorHeaps(&[Some(visible)]);
                    if call.ends_with("Uint") {
                        let mut v = [0, 1, 2, 3].map(|i| a[3][i].as_u64().unwrap_or(0) as u32);
                        for r in rules {
                            if let Action::ClearBits {
                                resource: target_id,
                                bits,
                                count,
                                when,
                            } = r.action
                                && target_id.is_none_or(|id| id == resource)
                                && matches!(when, rules::Timing::Before(ev) if ev.is_none_or(|e| e == seq))
                            {
                                for i in 0..4 {
                                    v[i] = if (i as u8) < count { bits[i] } else { bits[0] };
                                }
                            }
                        }
                        list.ClearUnorderedAccessViewUint(gpu, handle, &target, &v, &r);
                    } else {
                        let mut v = floats_exact(&a[3], a.get(5));
                        for r in rules {
                            if let Action::ClearBits {
                                resource: target_id,
                                bits,
                                count,
                                when,
                            } = r.action
                                && target_id.is_none_or(|id| id == resource)
                                && matches!(when, rules::Timing::Before(ev) if ev.is_none_or(|e| e == seq))
                            {
                                for i in 0..4 {
                                    let b = if (i as u8) < count { bits[i] } else { bits[0] };
                                    v[i] = f32::from_bits(b);
                                }
                            }
                        }
                        list.ClearUnorderedAccessViewFloat(gpu, handle, &target, &v, &r);
                    }
                }
                self.barriers(&needs, true);
            }
            other => self.warn(format!("{other} is not replayed")),
        }
        Ok(())
    }

    /// Emits a D3D12 UAV barrier on `resource` (or a global UAV barrier when `None`).
    fn emit_uav_barrier(&self, resource: Option<i64>) -> Result<(), String> {
        let target = match resource {
            Some(id) => Some(self.resource(id)?),
            None => None,
        };
        let barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: ManuallyDrop::new(target),
                }),
            },
        };
        unsafe { self.list.ResourceBarrier(&[barrier]) };
        Ok(())
    }

    /// Emits an explicit D3D12 transition barrier sequence on `resource`, flushing first so any prior implicit
    /// promotion in the open list decays back to `COMMON`, then returning the resource to `COMMON` so replay's
    /// per-event state invariant remains valid under D3D12 validation.
    fn emit_transition_barrier(&mut self, resource: i64, before: i32, after: i32) -> Result<(), String> {
        self.flush()?;
        let r = self.resource(resource)?;
        let emit = |from: i32, to: i32| {
            if from == to {
                return;
            }
            let b = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                        pResource: borrowed(&r),
                        Subresource: ALL,
                        StateBefore: D3D12_RESOURCE_STATES(from),
                        StateAfter: D3D12_RESOURCE_STATES(to),
                    }),
                },
            };
            unsafe { self.list.ResourceBarrier(&[b]) };
        };
        emit(0, before);
        emit(before, after);
        emit(after, 0);
        Ok(())
    }

    /// Injects a `ClearRenderTargetView` on texture `resource` (typeless formats mapped to a valid typed RTV format).
    fn inject_clear_color(&mut self, resource: i64, color: [f32; 4]) -> Result<(), String> {
        self.first_use_now(resource)?;
        let m = self.made(resource)?;
        if m.desc.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0 == 0 {
            return Err(format!(
                "clear-color #{resource}: resource has no ALLOW_RENDER_TARGET flag"
            ));
        }
        let r = m.resource.clone();
        let desc = m.desc;
        let handle = self.rtvs.cpu(0);
        let typed = DXGI_FORMAT(locum_capture::format::readable(desc.Format.0 as u32) as i32);
        let rtv_desc = if typed != desc.Format {
            let dim = if desc.SampleDesc.Count > 1 {
                if desc.DepthOrArraySize > 1 {
                    D3D12_RTV_DIMENSION_TEXTURE2DMSARRAY
                } else {
                    D3D12_RTV_DIMENSION_TEXTURE2DMS
                }
            } else if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
                D3D12_RTV_DIMENSION_TEXTURE3D
            } else if desc.DepthOrArraySize > 1 {
                D3D12_RTV_DIMENSION_TEXTURE2DARRAY
            } else {
                D3D12_RTV_DIMENSION_TEXTURE2D
            };
            Some(D3D12_RENDER_TARGET_VIEW_DESC {
                Format: typed,
                ViewDimension: dim,
                ..Default::default()
            })
        } else {
            None
        };
        unsafe {
            self.device
                .CreateRenderTargetView(&r, rtv_desc.as_ref().map(|d| d as *const _), handle);
        }
        let needs = [self.need_all(resource, 0x4)];
        self.barriers(&needs, false);
        unsafe { self.list.ClearRenderTargetView(handle, &color, None) };
        self.barriers(&needs, true);
        Ok(())
    }

    /// Injects a `ClearDepthStencilView` on texture `resource` (typeless depth formats mapped to typed DSV format).
    fn inject_clear_depth(&mut self, resource: i64, depth: f32, stencil: Option<u8>) -> Result<(), String> {
        self.first_use_now(resource)?;
        let m = self.made(resource)?;
        if m.desc.Flags.0 & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 == 0 {
            return Err(format!(
                "clear-depth #{resource}: resource has no ALLOW_DEPTH_STENCIL flag"
            ));
        }
        let r = m.resource.clone();
        let desc = m.desc;
        let handle = self.dsvs.cpu(0);
        let dsv_fmt = match desc.Format {
            DXGI_FORMAT_R32_TYPELESS => Some(DXGI_FORMAT_D32_FLOAT),
            DXGI_FORMAT_R16_TYPELESS => Some(DXGI_FORMAT_D16_UNORM),
            DXGI_FORMAT_R24G8_TYPELESS => Some(DXGI_FORMAT_D24_UNORM_S8_UINT),
            DXGI_FORMAT_R32G8X24_TYPELESS => Some(DXGI_FORMAT_D32_FLOAT_S8X24_UINT),
            _ => None,
        };
        let dsv_desc = dsv_fmt.map(|fmt| {
            let dim = if desc.SampleDesc.Count > 1 {
                if desc.DepthOrArraySize > 1 {
                    D3D12_DSV_DIMENSION_TEXTURE2DMSARRAY
                } else {
                    D3D12_DSV_DIMENSION_TEXTURE2DMS
                }
            } else if desc.DepthOrArraySize > 1 {
                D3D12_DSV_DIMENSION_TEXTURE2DARRAY
            } else {
                D3D12_DSV_DIMENSION_TEXTURE2D
            };
            D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: fmt,
                ViewDimension: dim,
                ..Default::default()
            }
        });
        unsafe {
            self.device
                .CreateDepthStencilView(&r, dsv_desc.as_ref().map(|d| d as *const _), handle);
        }
        let flags = if stencil.is_some() {
            D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL
        } else {
            D3D12_CLEAR_FLAG_DEPTH
        };
        let needs = [self.need_all(resource, 0x10)];
        self.barriers(&needs, false);
        unsafe {
            self.list
                .ClearDepthStencilView(handle, flags, depth, stencil.unwrap_or(0), None);
        }
        self.barriers(&needs, true);
        Ok(())
    }

    /// Replay state rule: seeds `resource` (or a specific subresource or depth/stencil plane) from another resource
    /// or from a raw file on disk.
    fn rule_seed(
        &mut self,
        resource: i64,
        sub: Option<u32>,
        plane: Option<rules::SeedPlane>,
        source: &rules::SeedSource,
    ) -> Result<(), String> {
        self.first_use_now(resource)?;
        let src_bytes = match source {
            rules::SeedSource::Resource(src_id) => {
                self.first_use_now(*src_id)?;
                self.read(*src_id)?
            }
            rules::SeedSource::File(path) => {
                std::fs::read(path).map_err(|e| format!("seed from {}: {e}", path.display()))?
            }
        };
        let m = self.made(resource)?;
        let layout = m.layout.clone();
        let joined = m.joined;
        let total = layout::total(&layout) as usize;
        let mut dst_bytes = if sub.is_none() && plane.is_none() && src_bytes.len() == total {
            src_bytes
        } else {
            let mut cur = self.read(resource)?;
            if let Some(sub_idx) = sub {
                let s = layout
                    .get(sub_idx as usize)
                    .ok_or_else(|| format!("seed #{resource}: subresource {sub_idx} out of range"))?;
                let start = s.offset as usize;
                let len = (s.size as usize)
                    .min(src_bytes.len())
                    .min(cur.len().saturating_sub(start));
                cur[start..start + len].copy_from_slice(&src_bytes[..len]);
            } else if let Some(p) = plane {
                // Depth/stencil joined layout: `joined` is depth bytes per texel (e.g. 4 for D24S8 / D32S8),
                // followed by 1 byte stencil (or 4-byte stride for R24G8/D24S8). If single-plane depth (joined=None),
                // write directly into the resource.
                match (p, joined) {
                    (rules::SeedPlane::Depth, Some(depth_bytes)) => {
                        let stride = depth_bytes as usize + 1;
                        let db = depth_bytes as usize;
                        let texels = (cur.len() / stride).min(src_bytes.len() / db.max(1));
                        for i in 0..texels {
                            cur[i * stride..i * stride + db].copy_from_slice(&src_bytes[i * db..i * db + db]);
                        }
                    }
                    (rules::SeedPlane::Stencil, Some(depth_bytes)) => {
                        let stride = depth_bytes as usize + 1;
                        let db = depth_bytes as usize;
                        let texels = (cur.len() / stride).min(src_bytes.len());
                        for i in 0..texels {
                            cur[i * stride + db] = src_bytes[i];
                        }
                    }
                    _ => {
                        let len = cur.len().min(src_bytes.len());
                        cur[..len].copy_from_slice(&src_bytes[..len]);
                    }
                }
            } else {
                let len = cur.len().min(src_bytes.len());
                cur[..len].copy_from_slice(&src_bytes[..len]);
            }
            cur
        };
        if dst_bytes.len() < total {
            dst_bytes.resize(total, 0);
        }
        self.put(resource, &dst_bytes)
    }

    /// Replay state rule: overrides a shader-visible descriptor heap slot with a null, SRV, UAV, or CBV view.
    #[allow(clippy::too_many_arguments)]
    fn rule_descriptor_override(
        &mut self,
        c: Option<&Capture>,
        heap: i64,
        slot: u32,
        kind: rules::DescKind,
        resource: i64,
        offset: u64,
        size: u64,
        restore_after: bool,
    ) -> Result<(), String> {
        let handle = if let Some(Some(m)) = self.heaps.get(&heap) {
            if slot >= m.count {
                return Err(format!("descriptor #{heap}[{slot}]: past heap count {}", m.count));
            }
            m.cpu(slot)
        } else if let Some(cap) = c {
            let m = self
                .heap(cap, heap)
                .ok_or_else(|| format!("descriptor #{heap}: heap is not shader-visible or not described"))?;
            if slot >= m.count {
                return Err(format!("descriptor #{heap}[{slot}]: past heap count {}", m.count));
            }
            m.cpu(slot)
        } else {
            return Err(format!("descriptor #{heap}: heap not loaded"));
        };
        unsafe {
            match kind {
                rules::DescKind::Null => {
                    let d = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        ..Default::default()
                    };
                    self.device.CreateShaderResourceView(None, Some(&d), handle);
                }
                rules::DescKind::Cbv => {
                    let m = self.made(resource)?;
                    let avail = m.desc.Width.saturating_sub(offset);
                    let bytes = if size > 0 {
                        size.min(avail)
                    } else {
                        avail.min(65536)
                    };
                    let d = D3D12_CONSTANT_BUFFER_VIEW_DESC {
                        BufferLocation: self.address(resource, offset),
                        SizeInBytes: bytes.next_multiple_of(256) as u32,
                    };
                    self.device.CreateConstantBufferView(Some(&d), handle);
                }
                rules::DescKind::Srv => {
                    let m = self.made(resource)?;
                    if m.buffer() {
                        let avail = m.desc.Width.saturating_sub(offset);
                        let bytes = if size > 0 { size.min(avail) } else { avail };
                        let d = D3D12_SHADER_RESOURCE_VIEW_DESC {
                            Format: DXGI_FORMAT_R32_TYPELESS,
                            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                                Buffer: D3D12_BUFFER_SRV {
                                    FirstElement: offset / 4,
                                    NumElements: (bytes / 4).min(u64::from(u32::MAX)) as u32,
                                    StructureByteStride: 0,
                                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                                },
                            },
                        };
                        self.device
                            .CreateShaderResourceView(&m.resource, Some(&d), handle);
                    } else {
                        self.device.CreateShaderResourceView(&m.resource, None, handle);
                    }
                }
                rules::DescKind::Uav => {
                    let m = self.made(resource)?;
                    if m.buffer() {
                        let avail = m.desc.Width.saturating_sub(offset);
                        let bytes = if size > 0 { size.min(avail) } else { avail };
                        let d = D3D12_UNORDERED_ACCESS_VIEW_DESC {
                            Format: DXGI_FORMAT_R32_TYPELESS,
                            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                                Buffer: D3D12_BUFFER_UAV {
                                    FirstElement: offset / 4,
                                    NumElements: (bytes / 4).min(u64::from(u32::MAX)) as u32,
                                    StructureByteStride: 0,
                                    CounterOffsetInBytes: 0,
                                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                                },
                            },
                        };
                        self.device
                            .CreateUnorderedAccessView(&m.resource, None, Some(&d), handle);
                    } else {
                        self.device
                            .CreateUnorderedAccessView(&m.resource, None, None, handle);
                    }
                }
            }
        }
        if restore_after {
            self.changed.push((heap, slot));
        }
        Ok(())
    }

    /// Executes replay-only state/API mutation rules scheduled before (`after == false`) or after (`after == true`)
    /// event `seq`. `clear_target` suppresses duplicate pre-clear injection when a recorded clear on that resource is
    /// already being overridden inside [`Self::clear`].
    fn apply_state_rules(
        &mut self,
        c: Option<&Capture>,
        seq: i64,
        clear_target: Option<i64>,
        rules: &[Rule],
        after: bool,
    ) -> Result<(), String> {
        let initial = !self.applied_initial_rules;
        let matches = |when: rules::Timing| match (after, when) {
            (false, rules::Timing::Before(Some(ev))) => ev == seq,
            (false, rules::Timing::Before(None)) => initial,
            (true, rules::Timing::After(ev)) => ev == seq,
            _ => false,
        };
        let mut ran_any = false;
        for rule in rules {
            match &rule.action {
                Action::TileMap {
                    resource,
                    tile,
                    count,
                    heap,
                    heap_tile,
                    when,
                } if matches(*when) => {
                    if after || ran_any {
                        self.flush()?;
                    }
                    self.rule_tile_map(c, *resource, *tile, *count, *heap, *heap_tile)?;
                    ran_any = true;
                }
                Action::TileUnmap {
                    resource,
                    tile,
                    count,
                    when,
                } if matches(*when) => {
                    if after || ran_any {
                        self.flush()?;
                    }
                    self.rule_tile_unmap(*resource, *tile, *count)?;
                    ran_any = true;
                }
                Action::MapUnmapped {
                    resource,
                    tile,
                    count,
                    when,
                } if matches(*when) => {
                    if after || ran_any {
                        self.flush()?;
                    }
                    self.rule_map_unmapped(*resource, *tile, *count)?;
                    ran_any = true;
                }
                Action::AliasTiles {
                    dst,
                    src,
                    dst_tile,
                    src_tile,
                    count,
                    when,
                } if matches(*when) => {
                    if after || ran_any {
                        self.flush()?;
                    }
                    self.rule_alias_tiles(*dst, *src, *dst_tile, *src_tile, *count)?;
                    ran_any = true;
                }
                Action::ClearColor {
                    resource,
                    color,
                    when,
                } if matches(*when) => {
                    if !after && clear_target == Some(*resource) {
                        continue;
                    }
                    self.inject_clear_color(*resource, *color)?;
                    ran_any = true;
                }
                Action::ClearDepth {
                    resource,
                    depth,
                    stencil,
                    when,
                } if matches(*when) => {
                    if !after && clear_target == Some(*resource) {
                        continue;
                    }
                    self.inject_clear_depth(*resource, *depth, *stencil)?;
                    ran_any = true;
                }
                Action::ClearBits {
                    resource: Some(resource),
                    bits,
                    count,
                    when,
                } if matches(*when) => {
                    if !after && clear_target == Some(*resource) {
                        continue;
                    }
                    let flags = self.made(*resource)?.desc.Flags.0;
                    if flags & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 != 0 {
                        let depth = f32::from_bits(bits[0]);
                        let stencil = (*count >= 2).then_some(bits[1] as u8);
                        self.inject_clear_depth(*resource, depth, stencil)?;
                    } else {
                        let color = [0, 1, 2, 3].map(|i| {
                            let b = if (i as u8) < *count { bits[i] } else { bits[0] };
                            f32::from_bits(b)
                        });
                        self.inject_clear_color(*resource, color)?;
                    }
                    ran_any = true;
                }
                Action::Seed {
                    resource,
                    sub,
                    plane,
                    source,
                    when,
                } if matches(*when) => {
                    if after || ran_any {
                        self.flush()?;
                    }
                    self.rule_seed(*resource, *sub, *plane, source)?;
                    self.populated.insert(*resource);
                    ran_any = true;
                }
                Action::DescriptorOverride {
                    heap,
                    slot,
                    kind,
                    resource,
                    offset,
                    size,
                    when,
                } if matches(*when) => {
                    let restore = matches!(when, rules::Timing::Before(Some(_)));
                    self.rule_descriptor_override(
                        c, *heap, *slot, *kind, *resource, *offset, *size, restore,
                    )?;
                }
                Action::Barrier {
                    resource,
                    before,
                    after: to_state,
                    when,
                } if matches(*when) => {
                    self.emit_transition_barrier(*resource, *before, *to_state)?;
                    ran_any = true;
                }
                Action::UavBarrier { resource, when } if matches(*when) => {
                    self.emit_uav_barrier(*resource)?;
                    ran_any = true;
                }
                Action::SyncFlush { when } if matches(*when) => {
                    self.emit_uav_barrier(None)?;
                    self.flush()?;
                    ran_any = true;
                }
                Action::SyncWait {
                    queue,
                    fence,
                    value,
                    when,
                } if matches(*when) => {
                    self.emit_uav_barrier(None)?;
                    self.flush()?;
                    self.fenced += 1;
                    let v = self.fenced;
                    unsafe {
                        self.queue
                            .Signal(&self.fence, v)
                            .map_err(|e| format!("sync-wait Signal: {e}"))?;
                        self.queue
                            .Wait(&self.fence, v)
                            .map_err(|e| format!("sync-wait Wait: {e}"))?;
                    }
                    self.emit_uav_barrier(None)?;
                    self.flush()?;
                    if let (Some(q), Some(f), Some(val)) = (queue, fence, value) {
                        self.rule_notes.insert(format!(
                            "sync-wait (queue #{q}, fence #{f}, value {val}) enforced at event {seq}"
                        ));
                    }
                    ran_any = true;
                }
                Action::SkipWait { event } if !after && event.is_none_or(|ev| ev == seq) => {
                    self.rule_notes
                        .insert(format!("skip-wait recorded at event {seq}"));
                }
                Action::CopyBuffer {
                    dst,
                    dst_offset,
                    src,
                    src_offset,
                    bytes,
                    when,
                } if matches(*when) => {
                    if after || ran_any {
                        self.flush()?;
                    }
                    self.first_use_now(*dst)?;
                    self.first_use_now(*src)?;
                    self.copy_buffer_to_buffer(*dst, *dst_offset, *src, *src_offset, *bytes)?;
                    ran_any = true;
                }
                Action::CopyResource { dst, src, when } if matches(*when) => {
                    if after || ran_any {
                        self.flush()?;
                    }
                    self.first_use_now(*dst)?;
                    self.first_use_now(*src)?;
                    let (dst_r, src_r) = (self.resource(*dst)?, self.resource(*src)?);
                    let needs = [self.need(*dst, None, 0x400), self.need(*src, None, 0x800)];
                    self.barriers(&needs, false);
                    unsafe { self.list.CopyResource(&dst_r, &src_r) };
                    self.barriers(&needs, true);
                    ran_any = true;
                }
                _ => {}
            }
        }
        if ran_any {
            self.flush()?;
        }
        if !after {
            self.applied_initial_rules = true;
        }
        Ok(())
    }
}

fn mirror(
    device: &ID3D12Device2,
    kind: D3D12_DESCRIPTOR_HEAP_TYPE,
    count: u32,
    visible: bool,
) -> Result<Mirror, String> {
    unsafe {
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: kind,
                NumDescriptors: count.max(1),
                Flags: if visible {
                    D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE
                } else {
                    D3D12_DESCRIPTOR_HEAP_FLAG_NONE
                },
                NodeMask: 0,
            })
            .map_err(|e| e.to_string())?;
        Ok(Mirror {
            cpu: heap.GetCPUDescriptorHandleForHeapStart().ptr,
            gpu: if visible {
                heap.GetGPUDescriptorHandleForHeapStart().ptr
            } else {
                0
            },
            step: device.GetDescriptorHandleIncrementSize(kind),
            count: count.max(1),
            heap,
        })
    }
}

/// A resource's description as the game made it, from the capture's: its flags those replay keeps, a placed one with
/// the alignment the game placed it with (small resources go at 4 KiB, not the 64 KiB asked of 0), and a reserved
/// texture with the game's tile layout (64KB_UNDEFINED_SWIZZLE or a standard swizzle).
pub(super) fn resource_desc(d: &Value) -> D3D12_RESOURCE_DESC {
    let n = |k: &str| d[k].as_u64().unwrap_or(0);
    let buffer = n("dimension") as i32 == D3D12_RESOURCE_DIMENSION_BUFFER.0;
    let placement = memory::Placement::of(d);
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION(n("dimension") as i32),
        Alignment: match placement {
            memory::Placement::Placed { .. } => n("alignment"),
            _ => 0,
        },
        Width: n("width"),
        Height: n("height") as u32,
        DepthOrArraySize: n("depth_or_array_size") as u16,
        MipLevels: n("mips") as u16,
        Format: DXGI_FORMAT(n("format") as i32),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: n("sample_count").max(1) as u32,
            Quality: n("sample_quality") as u32,
        },
        Layout: match placement {
            memory::Placement::Reserved if !buffer => D3D12_TEXTURE_LAYOUT(n("layout") as i32),
            _ if buffer => D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            _ => D3D12_TEXTURE_LAYOUT_UNKNOWN,
        },
        Flags: D3D12_RESOURCE_FLAGS(n("flags") as i32 & KEPT_FLAGS),
    }
}

impl Backend for D3d12 {
    fn prepare(&mut self, c: &Capture, plan: &super::Plan) {
        self.memory.prepare_plan(c, plan);
    }

    fn enable_bindless_trace(&mut self) {
        self.bindless_tracer = Some(BindlessTracer::default());
    }

    fn take_bindless_trace(&mut self) -> Vec<(i64, i64, i64, u32, bool)> {
        self.bindless_tracer
            .as_mut()
            .map(|t| std::mem::take(&mut t.hits))
            .unwrap_or_default()
    }

    fn create(&mut self, c: &Capture, id: i64, contents: Option<&[u8]>) -> Result<(), String> {
        self.create_inner(c, id, contents.map(Ok), None)
    }

    fn create_blob(&mut self, c: &Capture, id: i64, key: Option<&str>) -> Result<(), String> {
        self.create_inner(c, id, key.map(Err), key)
    }

    fn run(&mut self, c: &Capture, seq: i64, rules: &[Rule]) -> Result<(), String> {
        let (kind, call, args, list): (String, String, Option<String>, Option<i64>) = c
            .row(
                "SELECT kind, call, args, list FROM events WHERE seq = ?1",
                [seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok_or("no such event")?;
        let a: Value = args
            .as_deref()
            .and_then(|a| serde_json::from_str(a).ok())
            .unwrap_or_default();
        if let Some(s) = Self::submission_of(c, seq) {
            self.enter(c, s, Some(seq), rules)?;
        }
        let clear_target = if kind == "clear" {
            match call.as_str() {
                "ClearRenderTargetView" | "ClearDepthStencilView" => a[0]["definition"]["resource"].as_i64(),
                "ClearUnorderedAccessViewUint" | "ClearUnorderedAccessViewFloat" => Some(u(&a, 2) as i64),
                _ => None,
            }
        } else {
            None
        };
        if !rules.is_empty() {
            self.apply_state_rules(Some(c), seq, clear_target, rules, false)?;
        }
        match kind.as_str() {
            "draw" | "dispatch" => {
                // Before the event records anything: reading its argument records runs the open list.
                let arguments = match call.as_str() {
                    "ExecuteIndirect" => self.indirect_arguments(c, seq, &a)?,
                    "DispatchRays" => self.dispatch_rays(c, seq, &a)?,
                    _ => indirect::Arguments::AsRecorded,
                };
                if !matches!(arguments, indirect::Arguments::Skip) {
                    self.count_begin(list)?;
                    self.draw(c, seq, &kind, &call, &a, rules, arguments)?;
                    self.count_end()?;
                }
            }
            "copy" if call == "ResolveQueryData" => self.resolve_queries(&a)?,
            "copy" if call == "BuildRaytracingAccelerationStructure" => self.build(c, &a)?,
            "copy" if call == "CopyRaytracingAccelerationStructure" => self.copy_structure(c, &a)?,
            "copy" if call == "EmitRaytracingAccelerationStructurePostbuildInfo" => self.postbuild(c, &a)?,
            "copy" => self.copy(&call, &a)?,
            "clear" => self.clear(c, seq, &call, &a, rules)?,
            "update" if call == "WriteBufferImmediate" => {
                let writes: Vec<(
                    D3D12_WRITEBUFFERIMMEDIATE_PARAMETER,
                    D3D12_WRITEBUFFERIMMEDIATE_MODE,
                )> = a[0]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|p| {
                        (
                            D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
                                Dest: self.address(
                                    p["address"]["resource"].as_i64().unwrap_or(0),
                                    p["address"]["offset"].as_u64().unwrap_or(0),
                                ),
                                Value: p["value"].as_u64().unwrap_or(0) as u32,
                            },
                            D3D12_WRITEBUFFERIMMEDIATE_MODE(p["mode"].as_i64().unwrap_or(0) as i32),
                        )
                    })
                    .filter(|(p, _)| p.Dest != 0)
                    .collect();
                let list = self
                    .list
                    .cast::<ID3D12GraphicsCommandList2>()
                    .map_err(|e| e.to_string())?;
                let (params, modes): (Vec<_>, Vec<_>) = writes.into_iter().unzip();
                if !params.is_empty() {
                    unsafe {
                        list.WriteBufferImmediate(params.len() as u32, params.as_ptr(), Some(modes.as_ptr()))
                    };
                }
            }
            // Barriers are replay's own, state is taken at each draw, and a discard leaves contents undefined:
            // keeping them is one valid outcome. A bundle's commands are recorded after it, as its list's events.
            // The game's aliasing barriers and discards of targets in the memory resources share begin their use
            // there; other barriers are replay's own.
            "barrier" if call == "ResourceBarrier" => self.aliasing(seq, &a, rules),
            "discard" => self.discard(&a),
            "state" if call == "BeginQuery" => self.query_begin(&a, list),
            "state" if call == "EndQuery" => self.query_end(&a)?,
            "state" | "barrier" | "submit" | "bundle" => {}
            other => self.warn(format!("events of kind {other} ({call}) are not replayed")),
        }
        if !rules.is_empty() {
            self.apply_state_rules(Some(c), seq, None, rules, true)?;
        }
        let pending_trace = self.bindless_tracer.as_mut().and_then(|t| t.pending.take());
        if let Some((_, _, trace_heap, _)) = pending_trace
            && let Some(log) = self
                .bindless_tracer
                .as_ref()
                .and_then(|t| t.heaps.get(&trace_heap))
        {
            unsafe {
                let to_copy = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: borrowed(&log.gpu_buf),
                            Subresource: ALL,
                            StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                            StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                        }),
                    },
                };
                self.list.ResourceBarrier(&[to_copy]);
                self.list
                    .CopyBufferRegion(&log.readback_buf, 0, &log.gpu_buf, 0, log.bytes);
                let to_common = D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: borrowed(&log.gpu_buf),
                            Subresource: ALL,
                            StateBefore: D3D12_RESOURCE_STATE_COPY_SOURCE,
                            StateAfter: COMMON,
                        }),
                    },
                };
                self.list.ResourceBarrier(&[to_common]);
            }
        }
        let ran = self.flush().and_then(|()| self.collect_queries());
        if ran.is_ok() {
            for w in c.rows(
                "SELECT resource FROM event_resources WHERE event = ?1 AND access = 'write'",
                [seq],
                |r| r.get::<_, i64>(0),
            ) {
                self.populated.insert(w);
            }
        }
        if ran.is_ok()
            && let Some((draw_n, ev_seq, trace_heap, max_slots)) = pending_trace
            && let Some(t) = &mut self.bindless_tracer
            && let Some(log) = t.heaps.get(&trace_heap)
        {
            let slice = unsafe { core::slice::from_raw_parts(log.readback_ptr, log.bytes as usize) };
            for (slot, is_write) in crate::dxil::decode_bindless_log(slice, max_slots) {
                t.hits.push((draw_n, ev_seq, trace_heap, slot, is_write));
            }
        }
        // Put back the descriptors a rule changed for this event.
        for (heap, slot) in std::mem::take(&mut self.changed) {
            if let Some(v) = self.versions.get(&(heap, slot)).and_then(|id| {
                c.row(
                    &format!("SELECT {VERSION_COLUMNS} FROM d12_descriptor_versions v WHERE v.id = ?1"),
                    [id],
                    version,
                )
            }) {
                self.descriptor(c, &v);
            }
        }
        ran
    }

    fn reach(&mut self, c: &Capture, seq: i64) -> Result<(), String> {
        match Self::submission_of(c, seq) {
            Some(s) => self.enter(c, s, Some(seq), &[]),
            None => Ok(()),
        }
    }

    fn read(&mut self, id: i64) -> Result<Vec<u8>, String> {
        self.first_use_now(id)?;
        let m = self.made(id)?;
        if m.buffer() && m.heap.mapped() {
            let mut p = core::ptr::null_mut();
            let width = m.desc.Width as usize;
            unsafe {
                m.resource
                    .Map(0, Some(&D3D12_RANGE { Begin: 0, End: width }), Some(&mut p))
                    .map_err(|e| format!("#{id} Map: {e}"))?;
                let out = core::slice::from_raw_parts(p.cast::<u8>(), width).to_vec();
                m.resource.Unmap(0, Some(&D3D12_RANGE::default()));
                return Ok(out);
            }
        }
        let (r, buffer, bytes, tracked) = (m.resource.clone(), m.buffer(), m.copy_bytes, m.tracked);
        // A multisampled texture's samples are drawn into images first, copied from there; it lives until the
        // flush below has run them.
        let samples = if m.desc.SampleDesc.Count > 1 {
            Some(
                unsafe { locum_capture::d3d12::samples::Reader::new(&self.device, &m.desc, None) }
                    .map_err(|e| format!("#{id}, its samples: {e}"))?,
            )
        } else {
            None
        };
        let pieces = self.pieces(id);
        // What a reserved resource's unmapped tiles hold: zeros.
        let mut data = vec![0u8; bytes as usize];
        let needs = [self.need_all(id, 0x800)];
        let source = match &samples {
            Some(reader) => {
                unsafe { reader.record(&self.list, &r) };
                reader.images().clone()
            }
            None => r.clone(),
        };
        if tracked {
            self.barriers(&needs, false);
        }
        let last = pieces.len().saturating_sub(1);
        for (k, piece) in pieces.iter().enumerate() {
            let (readback, p) = self.staging(D3D12_HEAP_TYPE_READBACK, piece.len)?;
            for copy in &piece.copies {
                match copy {
                    staging::Copy::Buffer { at } => {
                        self.copy_from_buffer(&readback, 0, id, *at, piece.len)?;
                    }
                    staging::Copy::Texture {
                        sub,
                        footprint,
                        y,
                        z,
                        whole,
                    } => {
                        let dst = D3D12_TEXTURE_COPY_LOCATION {
                            pResource: borrowed(&readback),
                            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                                PlacedFootprint: *footprint,
                            },
                        };
                        let src = D3D12_TEXTURE_COPY_LOCATION {
                            pResource: borrowed(&source),
                            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                                SubresourceIndex: *sub,
                            },
                        };
                        let f = &footprint.Footprint;
                        let band = D3D12_BOX {
                            left: 0,
                            top: *y,
                            front: *z,
                            right: f.Width,
                            bottom: y + f.Height,
                            back: z + f.Depth,
                        };
                        let band = (!whole).then_some(&raw const band);
                        unsafe { self.list.CopyTextureRegion(&dst, 0, 0, 0, &src, band) };
                    }
                }
            }
            if k == last && tracked {
                self.barriers(&needs, true);
            }
            self.flush()?;
            let staged = unsafe { core::slice::from_raw_parts(p.cast_const(), piece.len as usize) };
            data[piece.from as usize..(piece.from + piece.len) as usize].copy_from_slice(staged);
            drop(readback);
        }
        if pieces.is_empty() {
            if tracked {
                self.barriers(&needs, true);
            }
            self.flush()?;
        }
        drop(samples);
        let m = &self.resources[&id];
        if buffer {
            return Ok(data);
        }
        layout::from_footprints(&m.layout, m.joined, &m.footprints, &data)
    }

    fn write(&mut self, _c: &Capture, id: i64, contents: &[u8]) -> Result<(), String> {
        self.put(id, contents)
    }

    fn read_bytes(&mut self, id: i64, offset: u64, len: u64) -> Result<Vec<u8>, String> {
        self.first_use_now(id)?;
        let (r, mapped, actual_width) = self.bytes_of(id, offset, len)?;
        let mut data = vec![0u8; len as usize];
        let avail = actual_width.saturating_sub(offset).min(len);
        if avail == 0 {
            return Ok(data);
        }
        if mapped {
            let mut p = core::ptr::null_mut();
            let range = D3D12_RANGE {
                Begin: offset as usize,
                End: (offset + avail) as usize,
            };
            unsafe {
                r.Map(0, Some(&range), Some(&mut p))
                    .map_err(|e| format!("#{id} Map: {e}"))?;
                core::ptr::copy_nonoverlapping(
                    p.cast::<u8>().add(offset as usize),
                    data.as_mut_ptr(),
                    avail as usize,
                );
                r.Unmap(0, Some(&D3D12_RANGE::default()));
            }
            return Ok(data);
        }
        // What a reserved buffer's unmapped tiles (or clamped tail) hold: zeros.
        for piece in staging::within(self.pieces(id), offset, offset + avail) {
            let (readback, p) = self.staging(D3D12_HEAP_TYPE_READBACK, piece.len)?;
            self.copy_from_buffer(&readback, 0, id, piece.from, piece.len)?;
            self.flush()?;
            let staged = unsafe { core::slice::from_raw_parts(p.cast_const(), piece.len as usize) };
            let at = (piece.from - offset) as usize;
            data[at..at + staged.len()].copy_from_slice(staged);
            drop(readback);
        }
        Ok(data)
    }

    fn write_bytes(&mut self, _c: &Capture, id: i64, offset: u64, bytes: &[u8]) -> Result<(), String> {
        // The rest of it as the frame began, first.
        self.first_use_now(id)?;
        let (_, mapped, actual_width) = self.bytes_of(id, offset, bytes.len() as u64)?;
        let avail = actual_width.saturating_sub(offset).min(bytes.len() as u64);
        if avail == 0 {
            return Ok(());
        }
        if mapped {
            return self.map_write(id, offset, &bytes[..avail as usize]);
        }
        for piece in staging::within(self.pieces(id), offset, offset + avail) {
            let (upload, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, piece.len)?;
            let at = (piece.from - offset) as usize;
            unsafe { core::slice::from_raw_parts_mut(p, piece.len as usize) }
                .copy_from_slice(&bytes[at..at + piece.len as usize]);
            self.copy_to_buffer(id, piece.from, &upload, 0, piece.len)?;
            self.flush()?;
        }
        Ok(())
    }

    fn environment(&self) -> environment::Environment {
        let luid = unsafe { self.device.GetAdapterLuid() };
        let adapter = unsafe { CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)) }
            .and_then(|f| unsafe { f.EnumAdapterByLuid::<IDXGIAdapter>(luid) })
            .map_err(|e| {
                format!(
                    "EnumAdapterByLuid({:08x}:{:08x}): {e}",
                    luid.HighPart, luid.LowPart
                )
            })
            .and_then(|a| environment::adapter(&a, environment::Api::D3d12));
        let mut e = environment::Environment::probe(adapter, environment::Api::D3d12, false);
        e.layers = environment::layers();
        e
    }

    fn warnings(&self) -> Vec<String> {
        let mut warnings = self.warnings.clone();
        warnings.extend(self.memory.warnings());
        warnings.extend(self.queries.warnings());
        if !self.undescribed.is_empty() {
            let mut ids: Vec<String> = self
                .undescribed
                .iter()
                .take(8)
                .map(|id| format!("#{id}"))
                .collect();
            if self.undescribed.len() > ids.len() {
                ids.push("...".into());
            }
            warnings.insert(format!(
                "{} resources the frame uses are not described in the capture, so views of them are null: {}",
                self.undescribed.len(),
                ids.join(" ")
            ));
        }
        warnings.into_iter().collect()
    }

    fn validation(&self) -> Vec<String> {
        self.messages()
    }

    fn notes(&self) -> Vec<String> {
        let mut notes = self.memory.notes();
        notes.extend(self.queries.notes());
        notes.extend(self.rule_notes.iter().cloned());
        notes
    }
}

impl D3d12 {
    /// Creates resource `id` from either in-memory `Ok(bytes)` or a streaming blob key `Err(key)`.
    fn create_inner(
        &mut self,
        c: &Capture,
        id: i64,
        source: Option<Result<&[u8], &str>>,
        key_hint: Option<&str>,
    ) -> Result<(), String> {
        let d: Option<Value> = c
            .row(
                "SELECT description FROM d12_objects WHERE id = ?1 AND kind LIKE 'ID3D12Resource%'",
                [id],
                |r| r.get::<_, String>(0),
            )
            .and_then(|d| serde_json::from_str(&d).ok());
        let (Some(s), Some(d)) = (shape(c, id), d) else {
            self.undescribed.push(id);
            return Ok(());
        };
        let n = |k: &str| d[k].as_u64().unwrap_or(0);
        let dimension = n("dimension") as i32;
        let buffer = dimension == D3D12_RESOURCE_DIMENSION_BUFFER.0;
        let props = &d["heap_properties"];
        let mut heap = match props["type"].as_u64() {
            Some(2) => Heap::Upload,
            Some(3) => Heap::Readback,
            Some(4) => Heap::Custom {
                page: props["cpu_page"].as_i64().unwrap_or(0) as i32,
                pool: props["memory_pool"].as_i64().unwrap_or(0) as i32,
            },
            _ => Heap::Default,
        };
        if !buffer && heap.mapped() {
            self.warn("textures on heaps the CPU maps are made on a default heap");
            heap = Heap::Default;
        }
        let placement = memory::Placement::of(&d);
        let format = n("format") as u32;
        let samples = n("sample_count").max(1) as u32;
        let mut desc = resource_desc(&d);
        let (properties, state) = match heap {
            Heap::Default => (
                D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                COMMON,
            ),
            Heap::Upload => (
                D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_UPLOAD,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_GENERIC_READ,
            ),
            Heap::Readback => (
                D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_READBACK,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_COPY_DEST,
            ),
            Heap::Custom { page, pool } => (
                D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_CUSTOM,
                    CPUPageProperty: D3D12_CPU_PAGE_PROPERTY(page),
                    MemoryPoolPreference: D3D12_MEMORY_POOL(pool),
                    ..Default::default()
                },
                COMMON,
            ),
        };
        let device = self.device.clone();
        let make = |desc: &D3D12_RESOURCE_DESC| -> windows::core::Result<ID3D12Resource> {
            let mut r: Option<ID3D12Resource> = None;
            unsafe {
                device.CreateCommittedResource(
                    &properties,
                    D3D12_HEAP_FLAG_NONE,
                    desc,
                    state,
                    None,
                    &mut r,
                )?
            };
            Ok(r.expect("made"))
        };
        // Helper: if `source` is a pieced blob, compute the set of 64 KiB tiles covering its non-zero pieces.
        let non_zero_blob_tiles = |src: Option<Result<&[u8], &str>>| -> Option<BTreeSet<u32>> {
            let Some(Err(key)) = src else {
                return None;
            };
            let list = c.blobs.pieces(key).ok()??;
            let tile = locum_capture::tiles::TILE_BYTES;
            let mut out = BTreeSet::new();
            let mut at = 0u64;
            for (k, len) in list {
                if k != locum_capture::blobs::ZEROS && len > 0 {
                    let first = (at / tile) as u32;
                    let last = (at.saturating_add(len).saturating_sub(1) / tile) as u32;
                    for t in first..=last {
                        out.insert(t);
                    }
                }
                at = at.saturating_add(len);
            }
            Some(out)
        };
        // Large committed or unshared-placed buffers with uncaptured (or mostly-zero pieced) contents are backed
        // sparsely as reserved buffers (only touched/non-zero 64 KiB tiles committed), on both WARP and GPU.
        let can_sparse_non_reserved = buffer
            && !matches!(placement, memory::Placement::Reserved)
            && desc.Width >= memory::SPARSE_COMMITTED_BUFFER_BYTES
            && !self.memory.upload_versioned.contains(&id)
            && match placement {
                memory::Placement::Placed { heap: gh, .. } => !self.memory.shared_content_heaps.contains(&gh),
                _ => true,
            };
        let sparse_extra_tiles = if can_sparse_non_reserved && source.is_some() {
            let nz = non_zero_blob_tiles(source);
            let total_tiles = desc.Width.div_ceil(locum_capture::tiles::TILE_BYTES);
            nz.filter(|tiles| (tiles.len() as u64) * 2 < total_tiles)
        } else {
            None
        };
        let prefer_sparse = can_sparse_non_reserved && (source.is_none() || sparse_extra_tiles.is_some());
        let sparse_made = if prefer_sparse {
            match self.make_sparse_buffer(id, &desc, COMMON, sparse_extra_tiles.as_ref()) {
                Ok(r) => {
                    heap = Heap::Default;
                    Some(r)
                }
                Err(why) => {
                    self.warn(format!(
                        "sparse backing for large buffer #{id} failed ({why}); falling back to normal creation"
                    ));
                    None
                }
            }
        } else {
            None
        };
        // In the memory the game shared, with the game's own flags, so it takes what it took there.
        let shared = match (sparse_made, placement) {
            (Some(r), _) => Some(r),
            (None, memory::Placement::Committed) => None,
            (None, p) => match self.make_shared(c, id, p, &desc, state, source.is_some()) {
                Ok(r) => Some(r),
                Err(why) => {
                    self.memory.fell_back(why);
                    None
                }
            },
        };
        let resource = match shared {
            Some(r) => r,
            None => {
                desc.Alignment = 0;
                desc.Layout = if buffer {
                    D3D12_TEXTURE_LAYOUT_ROW_MAJOR
                } else {
                    D3D12_TEXTURE_LAYOUT_UNKNOWN
                };
                // Textures that can be are made for simultaneous access, so their uses need no barriers.
                if !buffer && desc.Flags.0 & 0x2 == 0 && samples == 1 {
                    desc.Flags.0 |= SIMULTANEOUS;
                }
                match make(&desc) {
                    Ok(r) => r,
                    Err(e) if buffer && desc.Width >= memory::SPARSE_RESERVED_BUFFER_BYTES => {
                        // First try sparse reserved backing (touched/non-zero tiles only) before a doomed full-size
                        // clamped committed retry.
                        let nz = non_zero_blob_tiles(source);
                        if let Ok(r) = self.make_sparse_buffer(id, &desc, COMMON, nz.as_ref()) {
                            heap = Heap::Default;
                            self.warn(format!(
                                "buffer #{id} ({} bytes) failed full allocation ({e}) and was backed sparsely as a \
                                 reserved buffer",
                                desc.Width
                            ));
                            r
                        } else if desc.Width > memory::MAX_WARP_BUFFER_BYTES {
                            let clamped = D3D12_RESOURCE_DESC {
                                Width: memory::MAX_WARP_BUFFER_BYTES,
                                ..desc
                            };
                            self.warn(format!(
                                "committed buffer #{id} ({} bytes) exceeded this device's buffer width limit and was \
                                 clamped to {} bytes",
                                desc.Width, clamped.Width
                            ));
                            make(&clamped).map_err(|e2| format!("{e} (clamped retry: {e2})"))?
                        } else {
                            return Err(e.to_string());
                        }
                    }
                    Err(e) => return Err(e.to_string()),
                }
            }
        };
        let recorded_width = n("width");
        self.adopt_with_recorded(id, resource, heap, format, s.subs(), recorded_width)?;
        let subs = s.subs();
        let expected_total = layout::total(&subs);
        match source {
            Some(Ok(bytes)) => {
                // Captures before samples were read (2026-11-06) hold a multisampled texture's resolve.
                if samples > 1 && bytes.len() as u64 == layout::total(&s.resolved_subs()) {
                    self.warn(
                        "multisampled textures start empty (the capture holds their resolve, not their samples)",
                    );
                }
                if bytes.len() as u64 == expected_total {
                    if self.memory.shared.contains(&id) && self.defer(c, id) {
                        // Deferred until its first-use submission.
                    } else {
                        self.write(c, id, bytes)?;
                    }
                }
            }
            Some(Err(key)) => {
                let blob_size = c
                    .blobs
                    .size(key)
                    .ok_or_else(|| format!("blob {key} is not in {}", c.blobs.root().display()))?;
                if samples > 1 && blob_size == layout::total(&s.resolved_subs()) {
                    self.warn(
                        "multisampled textures start empty (the capture holds their resolve, not their samples)",
                    );
                }
                if blob_size == expected_total {
                    if self.memory.shared.contains(&id) && self.defer(c, id) {
                        // Deferred until its first-use submission: zero bytes read now!
                    } else {
                        self.put_blob_from(&c.blobs, id, key)?;
                    }
                }
            }
            None => {}
        }
        if source.is_some() {
            self.populated.insert(id);
        }
        if heap.mapped() {
            self.resources.get_mut(&id).expect("made").from = key_hint.map(str::to_string).or_else(|| {
                c.row("SELECT content FROM resources WHERE id = ?1", [id], |r| r.get(0))
                    .flatten()
            });
        }
        Ok(())
    }

    #[cfg(test)]
    fn adopt(
        &mut self,
        id: i64,
        resource: ID3D12Resource,
        heap: Heap,
        format: u32,
        subs: Vec<Sub>,
    ) -> Result<(), String> {
        let width = unsafe { resource.GetDesc() }.Width;
        self.adopt_with_recorded(id, resource, heap, format, subs, width)
    }

    /// Takes resource `id`, made as `resource` on `heap`, of `format`, whose contents lie in `subs` in the canonical
    /// layout: where a copy of each subresource lies in a buffer, and its address for a buffer.
    fn adopt_with_recorded(
        &mut self,
        id: i64,
        resource: ID3D12Resource,
        heap: Heap,
        format: u32,
        subs: Vec<Sub>,
        recorded_width: u64,
    ) -> Result<(), String> {
        let desc = unsafe { resource.GetDesc() };
        let buffer = desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER;
        let dimension = desc.Dimension.0;
        let samples = desc.SampleDesc.Count.max(1);
        name(&resource, format!("#{id}"));
        let planes = if locum_capture::format::is_depth_stencil(format) {
            2
        } else {
            1
        };
        let extent = Extent {
            mips: u32::from(desc.MipLevels).max(1),
            slices: if dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D.0 {
                1
            } else {
                u32::from(desc.DepthOrArraySize).max(1)
            },
            planes,
        };
        // A multisampled texture's contents are copied through one-sample images of its samples (see
        // `locum_capture::d3d12::samples`), whose footprints these are.
        let copied = if samples > 1 {
            locum_capture::d3d12::samples::images_desc(&desc)
                .ok_or_else(|| format!("format {format}: its samples cannot be written or read"))?
        } else {
            desc
        };
        let (placed, footprints, copy_bytes) = if buffer {
            (Vec::new(), Vec::new(), desc.Width)
        } else {
            let count = if samples > 1 {
                u32::from(copied.DepthOrArraySize) as usize
            } else {
                extent.count() as usize
            };
            let mut placed = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count];
            let mut rows = vec![0u32; count];
            let mut row_bytes = vec![0u64; count];
            let mut total = 0u64;
            unsafe {
                self.device.GetCopyableFootprints(
                    &copied,
                    0,
                    count as u32,
                    0,
                    Some(placed.as_mut_ptr()),
                    Some(rows.as_mut_ptr()),
                    Some(row_bytes.as_mut_ptr()),
                    Some(&mut total),
                )
            };
            if total == u64::MAX {
                return Err("GetCopyableFootprints refused its description".into());
            }
            let footprints = placed
                .iter()
                .zip(&rows)
                .zip(&row_bytes)
                .map(|((p, &rows), &row_bytes)| Footprint {
                    offset: p.Offset,
                    row_pitch: p.Footprint.RowPitch,
                    depth: p.Footprint.Depth,
                    rows,
                    row_bytes,
                })
                .collect();
            (placed, footprints, total)
        };
        let va = if buffer {
            unsafe { resource.GetGPUVirtualAddress() }
        } else {
            0
        };
        // Sample images hold both planes of a texel.
        let joined = (planes == 2 && samples == 1)
            .then(|| locum_capture::format::layout(format).map_or(4, |f| f.block_bytes));
        self.resources.insert(
            id,
            Made {
                resource,
                desc,
                recorded_width: if buffer {
                    recorded_width.max(desc.Width)
                } else {
                    desc.Width
                },
                heap,
                tracked: !buffer && desc.Flags.0 & SIMULTANEOUS == 0,
                extent,
                layout: subs,
                joined,
                placed,
                footprints,
                copy_bytes,
                va,
                from: None,
            },
        );
        Ok(())
    }

    /// Buffer `id`, whether the CPU reaches it through a mapping, and its actual width on this device
    /// (which may be clamped below `recorded_width` on WARP for 4 GiB buffers).
    fn bytes_of(&self, id: i64, offset: u64, len: u64) -> Result<(ID3D12Resource, bool, u64), String> {
        let m = self.made(id)?;
        if !m.buffer() {
            return Err(format!("#{id} is not a buffer"));
        }
        if offset.checked_add(len).is_none_or(|end| end > m.recorded_width) {
            return Err(format!(
                "#{id}: bytes {offset}..{} run past its {} bytes",
                offset.saturating_add(len),
                m.recorded_width
            ));
        }
        Ok((m.resource.clone(), m.heap.mapped(), m.desc.Width))
    }

    /// The pieces resource `id`'s contents move in (see [`staging`]): a reserved one's mapped tiles only.
    fn pieces(&self, id: i64) -> Vec<staging::Piece> {
        let m = &self.resources[&id];
        let mapped = self.memory.mapped.get(&id);
        if m.buffer() {
            staging::buffer(m.copy_bytes, mapped, self.limit)
        } else {
            staging::texture(&m.placed, &m.footprints, mapped, self.limit)
        }
    }

    /// Creates a temporary placed buffer alias in `run.heap` at `run.heap_offset` for WARP high-offset copies,
    /// keeping it alive until the open command list finishes.
    fn placed_alias(&mut self, run: &memory::PlacedRun) -> Result<ID3D12Resource, String> {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: run.width,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut r: Option<ID3D12Resource> = None;
        unsafe {
            self.device
                .CreatePlacedResource(&run.heap, run.heap_offset, &desc, COMMON, None, &mut r)
        }
        .map_err(|e| {
            format!(
                "placed alias at heap offset {} ({} bytes): {e}",
                run.heap_offset, run.width
            )
        })?;
        let r = r.ok_or("CreatePlacedResource gave no alias")?;
        self.kept.push(r.clone());
        Ok(r)
    }

    /// Records a copy into buffer `dst_id` at `dst_offset`, using placed-heap aliases on WARP whenever
    /// `dst_offset + len >= 2 GiB` on a reserved buffer.
    fn copy_to_buffer(
        &mut self,
        dst_id: i64,
        dst_offset: u64,
        src: &ID3D12Resource,
        src_offset: u64,
        len: u64,
    ) -> Result<(), String> {
        if self.warp
            && dst_offset.saturating_add(len) >= memory::WARP_COPY_LIMIT
            && let Some(runs) = self.placed_alias_runs(dst_id, dst_offset, len)
        {
            for run in runs {
                let alias = self.placed_alias(&run)?;
                unsafe {
                    self.list
                        .CopyBufferRegion(&alias, run.at, src, src_offset + run.slice_offset, run.len)
                };
            }
            return Ok(());
        }
        let dst = self.resource(dst_id)?;
        unsafe { self.list.CopyBufferRegion(&dst, dst_offset, src, src_offset, len) };
        Ok(())
    }

    /// Records a copy from buffer `src_id` at `src_offset`, using placed-heap aliases on WARP whenever
    /// `src_offset + len >= 2 GiB` on a reserved buffer.
    fn copy_from_buffer(
        &mut self,
        dst: &ID3D12Resource,
        dst_offset: u64,
        src_id: i64,
        src_offset: u64,
        len: u64,
    ) -> Result<(), String> {
        if self.warp
            && src_offset.saturating_add(len) >= memory::WARP_COPY_LIMIT
            && let Some(runs) = self.placed_alias_runs(src_id, src_offset, len)
        {
            for run in runs {
                let alias = self.placed_alias(&run)?;
                unsafe {
                    self.list
                        .CopyBufferRegion(dst, dst_offset + run.slice_offset, &alias, run.at, run.len)
                };
            }
            return Ok(());
        }
        let src = self.resource(src_id)?;
        unsafe { self.list.CopyBufferRegion(dst, dst_offset, &src, src_offset, len) };
        Ok(())
    }

    /// Records an event's `CopyBufferRegion` between two replay buffers, handling WARP's 2 GiB limit on reserved
    /// buffers on either or both sides.
    fn copy_buffer_to_buffer(
        &mut self,
        dst_id: i64,
        dst_offset: u64,
        src_id: i64,
        src_offset: u64,
        len: u64,
    ) -> Result<(), String> {
        let dst_high = self.warp
            && dst_offset.saturating_add(len) >= memory::WARP_COPY_LIMIT
            && self.memory.tile_map.contains_key(&dst_id);
        let src_high = self.warp
            && src_offset.saturating_add(len) >= memory::WARP_COPY_LIMIT
            && self.memory.tile_map.contains_key(&src_id);
        if !dst_high && !src_high {
            let (dst, src) = (self.resource(dst_id)?, self.resource(src_id)?);
            unsafe {
                self.list
                    .CopyBufferRegion(&dst, dst_offset, &src, src_offset, len)
            };
            return Ok(());
        }
        if dst_high && !src_high {
            let src = self.resource(src_id)?;
            return self.copy_to_buffer(dst_id, dst_offset, &src, src_offset, len);
        }
        if src_high && !dst_high {
            let dst = self.resource(dst_id)?;
            return self.copy_from_buffer(&dst, dst_offset, src_id, src_offset, len);
        }
        // Both sides are high reserved buffers: relay through a temporary default buffer in chunks.
        let mut done = 0u64;
        while done < len {
            let step = (len - done).min(self.limit);
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: step,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut tmp: Option<ID3D12Resource> = None;
            unsafe {
                self.device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    COMMON,
                    None,
                    &mut tmp,
                )
            }
            .map_err(|e| format!("relay buffer ({step} bytes): {e}"))?;
            let tmp = tmp.ok_or("no relay buffer")?;
            self.kept.push(tmp.clone());
            self.copy_from_buffer(&tmp, 0, src_id, src_offset + done, step)?;
            self.copy_to_buffer(dst_id, dst_offset + done, &tmp, 0, step)?;
            done += step;
        }
        Ok(())
    }

    /// Writes resource `id`'s contents directly from blob `key` in `blobs`, streaming buffers piece-by-piece
    /// (never materializing a 4 GiB Vec in RAM, and skipping `ZEROS` pieces for fresh/unmapped regions).
    pub(super) fn put_blob_from(
        &mut self,
        blobs: &locum_capture::blobs::Blobs,
        id: i64,
        key: &str,
    ) -> Result<(), String> {
        let m = self.made(id)?;
        let expected = layout::total(&m.layout);
        let blob_size = blobs
            .size(key)
            .ok_or_else(|| format!("blob {key} is not in {}", blobs.root().display()))?;
        if blob_size != expected {
            return Err(format!(
                "#{id}: {blob_size} bytes do not fit its {expected} bytes of subresources"
            ));
        }
        if !m.buffer() {
            let bytes = blobs
                .get(key)
                .map_err(|e| format!("blob {key} is not in {} ({e})", blobs.root().display()))?;
            return self.put(id, &bytes);
        }
        let pieces = self.pieces(id);
        if pieces.is_empty() {
            return Ok(());
        }
        // For pieced blobs (e.g. 4 GiB reserved buffers), identify non-zero byte intervals so ZEROS segments never
        // allocate RAM or staging copies unless a shared heap might have been written before.
        let non_zero: Option<Vec<(u64, u64)>> = blobs
            .pieces(key)
            .map_err(|e| format!("blob {key} pieces: {e}"))?
            .map(|list| {
                let mut out = Vec::new();
                let mut at = 0u64;
                for (k, len) in list {
                    if k != locum_capture::blobs::ZEROS && len > 0 {
                        out.push((at, at + len));
                    }
                    at += len;
                }
                out
            });
        // Split/filter pieces to only non-zero runs when `non_zero` is known.
        let to_upload: Vec<staging::Piece> = match &non_zero {
            Some(runs) if !runs.is_empty() => {
                let mut out = Vec::new();
                for p in pieces {
                    let (p0, p1) = (p.from, p.from + p.len);
                    for &(r0, r1) in runs {
                        let (lo, hi) = (p0.max(r0), p1.min(r1));
                        if lo < hi {
                            out.push(staging::Piece {
                                from: lo,
                                len: hi - lo,
                                copies: vec![staging::Copy::Buffer { at: lo }],
                            });
                        }
                    }
                }
                out
            }
            Some(_) => Vec::new(), // Entirely ZEROS!
            None => pieces,
        };
        if to_upload.is_empty() {
            return Ok(());
        }
        let mapped_heap = self.made(id)?.heap.mapped();
        if mapped_heap {
            for piece in &to_upload {
                let chunk = blobs
                    .read(key, piece.from, Some(piece.len))
                    .map_err(|e| format!("blob {key} at {}..{}: {e}", piece.from, piece.from + piece.len))?;
                self.map_write(id, piece.from, &chunk)?;
            }
            return Ok(());
        }
        let last = to_upload.len().saturating_sub(1);
        for (k, piece) in to_upload.iter().enumerate() {
            let chunk = blobs
                .read(key, piece.from, Some(piece.len))
                .map_err(|e| format!("blob {key} at {}..{}: {e}", piece.from, piece.from + piece.len))?;
            let (upload, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, piece.len)?;
            let staged = unsafe { core::slice::from_raw_parts_mut(p, piece.len as usize) };
            staged.copy_from_slice(&chunk);
            self.copy_to_buffer(id, piece.from, &upload, 0, piece.len)?;
            if k < last {
                self.flush()
                    .map_err(|e| format!("upload #{id} {}..{}: {e}", piece.from, piece.from + piece.len))?;
            }
        }
        self.flush().map_err(|e| format!("upload #{id} final: {e}"))
    }

    /// Writes `contents` (canonical layout) into resource `id`.
    fn put(&mut self, id: i64, contents: &[u8]) -> Result<(), String> {
        let m = self.made(id)?;
        if contents.len() as u64 != layout::total(&m.layout) {
            return Err(format!(
                "#{id}: {} bytes do not fit its {} bytes of subresources",
                contents.len(),
                layout::total(&m.layout)
            ));
        }
        if m.buffer() && m.heap.mapped() {
            let len = (m.desc.Width as usize).min(contents.len());
            return self.map_write(id, 0, &contents[..len]);
        }
        let (r, buffer, bytes, tracked) = (m.resource.clone(), m.buffer(), m.copy_bytes, m.tracked);
        // A multisampled texture's samples are copied into images, drawn from there; it lives until the flush
        // below has run them.
        let samples = if m.desc.SampleDesc.Count > 1 {
            Some(
                unsafe { locum_capture::d3d12::samples::Writer::new(&self.device, &m.desc) }
                    .map_err(|e| format!("#{id}, its samples: {e}"))?,
            )
        } else {
            None
        };
        let pieces = self.pieces(id);
        // A texture's contents laid out as its copies read them, from which each piece is staged.
        let laid = if buffer {
            None
        } else {
            let m = &self.resources[&id];
            let mut laid = vec![0u8; bytes as usize];
            layout::into_footprints(&m.layout, m.joined, &m.footprints, contents, &mut laid)?;
            Some(laid)
        };
        let laid = laid.as_deref().unwrap_or(contents);
        let needs = [self.need_all(id, 0x400)];
        let target = samples.as_ref().map_or_else(|| r.clone(), |w| w.images().clone());
        if tracked {
            self.barriers(&needs, false);
        }
        let last = pieces.len().saturating_sub(1);
        for (k, piece) in pieces.iter().enumerate() {
            let (upload, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, piece.len)?;
            let staged = unsafe { core::slice::from_raw_parts_mut(p, piece.len as usize) };
            staged.copy_from_slice(&laid[piece.from as usize..(piece.from + piece.len) as usize]);
            for copy in &piece.copies {
                match copy {
                    staging::Copy::Buffer { at } => {
                        self.copy_to_buffer(id, *at, &upload, 0, piece.len)?;
                    }
                    staging::Copy::Texture {
                        sub, footprint, y, z, ..
                    } => {
                        let dst = D3D12_TEXTURE_COPY_LOCATION {
                            pResource: borrowed(&target),
                            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                                SubresourceIndex: *sub,
                            },
                        };
                        let src = D3D12_TEXTURE_COPY_LOCATION {
                            pResource: borrowed(&upload),
                            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                                PlacedFootprint: *footprint,
                            },
                        };
                        unsafe { self.list.CopyTextureRegion(&dst, 0, *y, *z, &src, None) };
                    }
                }
            }
            if k < last {
                self.flush()?;
            }
        }
        if let Some(writer) = &samples {
            unsafe { writer.record(&self.list, &r) };
        }
        if tracked {
            self.barriers(&needs, true);
        }
        let ran = self.flush();
        drop(samples);
        ran
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn committed(device: &ID3D12Device2, desc: &D3D12_RESOURCE_DESC) -> ID3D12Resource {
        let mut r: Option<ID3D12Resource> = None;
        unsafe {
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                desc,
                COMMON,
                None,
                &mut r,
            )
        }
        .unwrap();
        r.unwrap()
    }

    fn texture(
        dimension: D3D12_RESOURCE_DIMENSION,
        format: DXGI_FORMAT,
        size: [u32; 3],
        mips: u16,
    ) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: dimension,
            Width: u64::from(size[0]),
            Height: size[1],
            DepthOrArraySize: size[2] as u16,
            MipLevels: mips,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        }
    }

    fn pattern(len: u64) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + i / 251) as u8).collect()
    }

    /// Contents larger than one staging buffer go to the GPU and come back in pieces, as written: a buffer, a
    /// block-compressed texture with mips (bands of rows), a volume (bands of slices); a reserved buffer's mapped tiles
    /// only, its unmapped ones read as zeros.
    #[test]
    fn contents_larger_than_a_staging_buffer_go_and_come_back_in_pieces() {
        let mut d = D3d12::new(true, super::super::Validation::Off).unwrap();
        d.limit = 4096;

        let bytes = 65539;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: bytes,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let buffer = committed(&d.device, &desc);
        d.adopt(1, buffer, Heap::Default, 0, layout::buffer_of(bytes))
            .unwrap();
        assert!(d.pieces(1).len() > 16);
        let contents = pattern(bytes);
        d.put(1, &contents).unwrap();
        assert_eq!(d.read(1).unwrap(), contents, "the buffer");

        let bc1 = texture(
            D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            DXGI_FORMAT_BC1_UNORM,
            [256, 200, 1],
            3,
        );
        let subs = layout::texture(DXGI_FORMAT_BC1_UNORM.0 as u32, 256, 200, 1, 3, 1).unwrap();
        let contents = pattern(layout::total(&subs));
        d.adopt(
            2,
            committed(&d.device, &bc1),
            Heap::Default,
            DXGI_FORMAT_BC1_UNORM.0 as u32,
            subs,
        )
        .unwrap();
        d.put(2, &contents).unwrap();
        assert_eq!(d.read(2).unwrap(), contents, "the block-compressed texture");

        let volume = texture(
            D3D12_RESOURCE_DIMENSION_TEXTURE3D,
            DXGI_FORMAT_R8G8B8A8_UNORM,
            [32, 16, 8],
            1,
        );
        let subs = layout::texture(DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32, 32, 16, 8, 1, 1).unwrap();
        let contents = pattern(layout::total(&subs));
        d.adopt(
            3,
            committed(&d.device, &volume),
            Heap::Default,
            DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
            subs,
        )
        .unwrap();
        d.put(3, &contents).unwrap();
        assert_eq!(d.read(3).unwrap(), contents, "the volume");

        // Four tiles, the first and third mapped.
        let tiles = 4 * locum_capture::tiles::TILE_BYTES;
        let reserved = D3D12_RESOURCE_DESC { Width: tiles, ..desc };
        let mut r: Option<ID3D12Resource> = None;
        unsafe { d.device.CreateReservedResource(&reserved, COMMON, None, &mut r) }.unwrap();
        let r = r.unwrap();
        let mut heap: Option<ID3D12Heap> = None;
        unsafe {
            d.device.CreateHeap(
                &D3D12_HEAP_DESC {
                    SizeInBytes: 2 * locum_capture::tiles::TILE_BYTES,
                    Properties: D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                    ..Default::default()
                },
                &mut heap,
            )
        }
        .unwrap();
        let heap = heap.unwrap();
        let starts = [0u32, 2].map(|x| D3D12_TILED_RESOURCE_COORDINATE {
            X: x,
            ..Default::default()
        });
        let (offsets, counts) = ([0u32, 1], [1u32, 1]);
        unsafe {
            d.queue.UpdateTileMappings(
                &r,
                2,
                Some(starts.as_ptr()),
                None,
                &heap,
                2,
                None,
                Some(offsets.as_ptr()),
                Some(counts.as_ptr()),
                D3D12_TILE_MAPPING_FLAG_NONE,
            )
        };
        let tiled = locum_capture::tiles::d3d12(&r).unwrap();
        d.memory.mapped.insert(
            4,
            staging::Mapped {
                layout: tiled,
                tiles: [0, 2].into(),
            },
        );
        d.adopt(4, r, Heap::Default, 0, layout::buffer_of(tiles)).unwrap();
        let contents = pattern(tiles);
        d.put(4, &contents).unwrap();
        let read = d.read(4).unwrap();
        let tile = locum_capture::tiles::TILE_BYTES as usize;
        for (t, (got, put)) in read.chunks(tile).zip(contents.chunks(tile)).enumerate() {
            if t % 2 == 0 {
                assert_eq!(got, put, "mapped tile {t}");
            } else {
                assert!(got.iter().all(|&b| b == 0), "unmapped tile {t}");
            }
        }
        drop(heap);
    }

    /// A 4 GiB (2^32) reserved buffer clamped to WARP's 4 GiB - 64 KiB limit streams a pieced blob with ZEROS
    /// segments directly into mapped tiles without allocating 4 GiB of RAM.
    #[test]
    fn four_gib_reserved_buffer_and_streaming_pieced_blob_on_warp() {
        let mut d = D3d12::new(true, super::super::Validation::Off).unwrap();
        let tile = locum_capture::tiles::TILE_BYTES;
        let gib4: u64 = 1 << 32;
        let clamped_width = memory::MAX_WARP_BUFFER_BYTES;
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: clamped_width,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut r: Option<ID3D12Resource> = None;
        unsafe { d.device.CreateReservedResource(&desc, COMMON, None, &mut r) }.unwrap();
        let r = r.unwrap();
        let mut heap: Option<ID3D12Heap> = None;
        unsafe {
            d.device.CreateHeap(
                &D3D12_HEAP_DESC {
                    SizeInBytes: tile * 2,
                    Properties: D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                    ..Default::default()
                },
                &mut heap,
            )
        }
        .unwrap();
        let heap = heap.unwrap();
        // Map tile 17 (< 2 GiB) -> heap tile 0, and tile 40000 (> 2 GiB, past WARP's CopyBufferRegion limit) -> heap tile 1.
        let starts = [
            D3D12_TILED_RESOURCE_COORDINATE {
                X: 17,
                ..Default::default()
            },
            D3D12_TILED_RESOURCE_COORDINATE {
                X: 40000,
                ..Default::default()
            },
        ];
        let heap_offsets = [0u32, 1u32];
        let counts = [1u32, 1u32];
        unsafe {
            d.queue.UpdateTileMappings(
                &r,
                2,
                Some(starts.as_ptr()),
                None,
                &heap,
                2,
                None,
                Some(heap_offsets.as_ptr()),
                Some(counts.as_ptr()),
                D3D12_TILE_MAPPING_FLAG_NONE,
            )
        };
        let tiled = locum_capture::tiles::d3d12(&r).unwrap();
        d.memory.mapped.insert(
            24,
            staging::Mapped {
                layout: tiled,
                tiles: [17, 40000].into(),
            },
        );
        d.memory.insert_test_heap(100, heap.clone());
        d.memory
            .tile_map
            .insert(24, [(17, (100, 0)), (40000, (100, 1))].into());
        d.adopt_with_recorded(24, r, Heap::Default, 0, layout::buffer_of(gib4), gib4)
            .unwrap();
        // Build a 4 GiB pieced blob where tile 17 and tile 40000 have data and the rest is ZEROS.
        let dir = std::env::temp_dir().join(format!("locum-test-4gib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blobs = locum_capture::blobs::Blobs::new(&dir);
        let low_bytes = pattern(tile);
        let high_bytes: Vec<u8> = pattern(tile).into_iter().map(|b| b ^ 0x5A).collect();
        let low_key = locum_capture::hash::key(&low_bytes);
        let high_key = locum_capture::hash::key(&high_bytes);
        blobs.put(&low_key, &low_bytes).unwrap();
        blobs.put(&high_key, &high_bytes).unwrap();
        let pieces = vec![
            (locum_capture::blobs::ZEROS.to_owned(), 17 * tile),
            (low_key, tile),
            (locum_capture::blobs::ZEROS.to_owned(), (40000 - 18) * tile),
            (high_key, tile),
            (locum_capture::blobs::ZEROS.to_owned(), gib4 - 40001 * tile),
        ];
        let key = locum_capture::blobs::pieces_key(&pieces);
        blobs.put_pieces(&key, &pieces).unwrap();
        d.put_blob_from(&blobs, 24, &key).unwrap();
        assert_eq!(d.read_bytes(24, 17 * tile, tile).unwrap(), low_bytes);
        assert_eq!(d.read_bytes(24, 40000 * tile, tile).unwrap(), high_bytes);
        let zero = d.read_bytes(24, 0, 256).unwrap();
        assert!(zero.iter().all(|&b| b == 0));
        let _ = blobs.close();
        let _ = std::fs::remove_dir_all(&dir);
        drop(heap);
    }

    /// Replay state/API rules (copy-buffer, copy-resource, barrier, uav-barrier, sync-flush, sync-wait,
    /// clear-color, clear-depth, tile-map, tile-unmap) execute cleanly on WARP and mutate GPU state as expected.
    #[test]
    fn state_and_api_rules_mutate_gpu_state_on_warp() {
        let mut d = D3d12::new(true, super::super::Validation::Layer).unwrap();
        let tile = locum_capture::tiles::TILE_BYTES;
        let buf_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 256,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        d.adopt(
            1,
            committed(&d.device, &buf_desc),
            Heap::Default,
            0,
            layout::buffer_of(256),
        )
        .unwrap();
        d.adopt(
            2,
            committed(&d.device, &buf_desc),
            Heap::Default,
            0,
            layout::buffer_of(256),
        )
        .unwrap();
        d.adopt(
            3,
            committed(&d.device, &buf_desc),
            Heap::Default,
            0,
            layout::buffer_of(256),
        )
        .unwrap();
        let src_bytes = pattern(256);
        d.put(1, &src_bytes).unwrap();
        d.put(2, &vec![0u8; 256]).unwrap();
        d.put(3, &vec![0u8; 256]).unwrap();

        // Render target texture (#4) and depth texture (#5).
        let rt_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let rt_subs = layout::texture(DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32, 4, 4, 1, 1, 1).unwrap();
        d.adopt(
            4,
            committed(&d.device, &rt_desc),
            Heap::Default,
            DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
            rt_subs,
        )
        .unwrap();

        let dsv_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 4,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R32_TYPELESS,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            ..Default::default()
        };
        let dsv_subs = layout::texture(DXGI_FORMAT_R32_TYPELESS.0 as u32, 4, 4, 1, 1, 1).unwrap();
        d.adopt(
            5,
            committed(&d.device, &dsv_desc),
            Heap::Default,
            DXGI_FORMAT_R32_TYPELESS.0 as u32,
            dsv_subs,
        )
        .unwrap();

        // Reserved buffer (#6, 2 tiles) + backing heap (#70, 2 tiles).
        let res_desc = D3D12_RESOURCE_DESC {
            Width: 2 * tile,
            ..buf_desc
        };
        let mut res_r: Option<ID3D12Resource> = None;
        unsafe {
            d.device
                .CreateReservedResource(&res_desc, COMMON, None, &mut res_r)
        }
        .unwrap();
        let res_r = res_r.unwrap();
        let mut heap: Option<ID3D12Heap> = None;
        unsafe {
            d.device.CreateHeap(
                &D3D12_HEAP_DESC {
                    SizeInBytes: 2 * tile,
                    Properties: D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                    ..Default::default()
                },
                &mut heap,
            )
        }
        .unwrap();
        let heap = heap.unwrap();
        d.memory.insert_test_heap(70, heap.clone());
        let tiled = locum_capture::tiles::d3d12(&res_r).unwrap();
        d.memory.mapped.insert(
            6,
            staging::Mapped {
                layout: tiled,
                tiles: BTreeSet::new(),
            },
        );
        d.adopt(6, res_r, Heap::Default, 0, layout::buffer_of(2 * tile))
            .unwrap();

        let (rules, errors) = rules::parse_all(
            "\
            copy-buffer #2 64 #1 16 128 before 100
            barrier #2 uav srv before 100
            uav-barrier #2 before 100
            sync-flush before 100
            sync-wait 81 61887 1710 before 100
            copy-resource #3 #2 after 100
            clear-color #4 1 0 0.5 1 after 100
            clear-depth #5 0.25 after 100
            tile-map #6 0 1 #70 0 before 100
            tile-unmap #6 0 1 after 200
            ",
            std::path::Path::new("."),
            &rules::hex,
        );
        assert!(errors.is_empty(), "{errors:?}");

        d.apply_state_rules(None, 100, None, &rules, false).unwrap();
        d.apply_state_rules(None, 100, None, &rules, true).unwrap();
        d.flush().unwrap();

        // Check copy-buffer and copy-resource results.
        let b2 = d.read(2).unwrap();
        assert_eq!(&b2[64..192], &src_bytes[16..144]);
        assert!(b2[..64].iter().all(|&x| x == 0));
        assert_eq!(d.read(3).unwrap(), b2);

        // Check clear-color result on RGBA8 RT (#4): [255, 0, 128, 255].
        let rt = d.read(4).unwrap();
        assert_eq!(&rt[0..4], &[255, 0, 128, 255]);

        // Check clear-depth result on R32_TYPELESS DSV (#5): 0.25f32.
        let dsv = d.read(5).unwrap();
        let depth_val = f32::from_le_bytes(dsv[0..4].try_into().unwrap());
        assert!((depth_val - 0.25).abs() < 1e-6, "got {depth_val}");

        // Check tile-map made tile 0 writable/readable, then tile-unmap returns it to zeros.
        let mut full_tiles = vec![0u8; (2 * tile) as usize];
        let tile_pat = pattern(256);
        full_tiles[..256].copy_from_slice(&tile_pat);
        d.put(6, &full_tiles).unwrap();
        assert_eq!(d.read_bytes(6, 0, 256).unwrap(), tile_pat);
        d.apply_state_rules(None, 200, None, &rules, true).unwrap();
        d.flush().unwrap();
        assert!(d.read_bytes(6, 0, 256).unwrap().iter().all(|&b| b == 0));
        assert!(d.messages().is_empty(), "D3D12 debug errors: {:?}", d.messages());
        drop(heap);
    }

    /// Tests `map`, `alias`, `clear-bits` (preserving raw IEEE-754 float/NaN payloads), and `seed` on WARP.
    #[test]
    fn map_alias_clear_bits_and_seed_rules_on_warp() {
        let mut d = D3d12::new(true, super::super::Validation::Layer).unwrap();
        let tile = locum_capture::tiles::TILE_BYTES;
        let res_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 2 * tile,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        // Two initially unmapped reserved buffers (#10 and #11).
        for id in [10, 11] {
            let mut r: Option<ID3D12Resource> = None;
            unsafe { d.device.CreateReservedResource(&res_desc, COMMON, None, &mut r) }.unwrap();
            let r = r.unwrap();
            let tiled = locum_capture::tiles::d3d12(&r).unwrap();
            d.memory.mapped.insert(
                id,
                staging::Mapped {
                    layout: tiled,
                    tiles: BTreeSet::new(),
                },
            );
            d.memory.tile_map.insert(id, std::collections::BTreeMap::new());
            d.adopt(id, r, Heap::Default, 0, layout::buffer_of(2 * tile))
                .unwrap();
        }
        // RGBA32F render target (#12) for raw bit-exact clear-bits (including NaN payload), plus #13 for seed target.
        let rt_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 2,
            Height: 2,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let rt_subs = layout::texture(DXGI_FORMAT_R32G32B32A32_FLOAT.0 as u32, 2, 2, 1, 1, 1).unwrap();
        for id in [12, 13] {
            d.adopt(
                id,
                committed(&d.device, &rt_desc),
                Heap::Default,
                DXGI_FORMAT_R32G32B32A32_FLOAT.0 as u32,
                rt_subs.clone(),
            )
            .unwrap();
        }
        let (rules, errors) = rules::parse_all(
            "\
            map #10 0 1 before 50
            alias #11 #10 0 0 1 before 50
            clear-bits #12 0x7fc01234 0x3f800000 0xbf000000 0x00000000 before 50
            seed #13 from #12 after 50
            ",
            std::path::Path::new("."),
            &rules::hex,
        );
        assert!(errors.is_empty(), "{errors:?}");
        d.apply_state_rules(None, 50, None, &rules, false).unwrap();

        // Writing through #10 is immediately visible through aliased #11 (shared backing physical heap tile!).
        let mut full = vec![0u8; (2 * tile) as usize];
        let pat = pattern(256);
        full[..256].copy_from_slice(&pat);
        d.put(10, &full).unwrap();
        assert_eq!(d.read_bytes(11, 0, 256).unwrap(), pat);

        // Check raw bit-exact clear-bits on #12 (including NaN payload 0x7fc01234) and seed into #13.
        d.apply_state_rules(None, 50, None, &rules, true).unwrap();
        let rt12 = d.read(12).unwrap();
        let rt13 = d.read(13).unwrap();
        let w = |buf: &[u8], i: usize| u32::from_le_bytes(buf[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(w(&rt12, 0), 0x7fc01234);
        assert_eq!(w(&rt12, 1), 0x3f800000);
        assert_eq!(w(&rt12, 2), 0xbf000000);
        assert_eq!(w(&rt12, 3), 0x00000000);
        assert_eq!(rt13, rt12);
        assert!(d.messages().is_empty(), "D3D12 debug errors: {:?}", d.messages());
    }

    /// Tests sparse tile-pool backing of a 4 GiB buffer (only touched tiles backed, both below and above WARP's
    /// 2 GiB copy limit) and `map all-unmapped` backing 0-tile and partially-mapped reserved textures while leaving
    /// giant sparse buffers untouched.
    #[test]
    fn sparse_large_buffer_and_map_all_unmapped_textures_on_warp() {
        let mut d = D3d12::new(true, super::super::Validation::Layer).unwrap();
        let tile = locum_capture::tiles::TILE_BYTES;
        let gib4: u64 = 1 << 32;

        // 1. 4 GiB buffer (#25) backed sparsely with only tiles 5 (< 2 GiB) and 40000 (> 2 GiB) touched.
        d.memory.touched_tiles.insert(25, [5, 40000].into());
        let big_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: gib4,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let r25 = d.make_sparse_buffer(25, &big_desc, COMMON, None).unwrap();
        d.adopt_with_recorded(25, r25, Heap::Default, 0, layout::buffer_of(gib4), gib4)
            .unwrap();
        let low_pat = pattern(512);
        let high_pat: Vec<u8> = pattern(512).into_iter().map(|b| b ^ 0xA5).collect();
        let dummy = Capture {
            db: rusqlite::Connection::open_in_memory().unwrap(),
            path: std::path::PathBuf::new(),
            blobs: locum_capture::blobs::Blobs::new("."),
            names: crate::join::Names::default(),
        };
        d.write_bytes(&dummy, 25, 5 * tile, &low_pat).unwrap();
        d.write_bytes(&dummy, 25, 40000 * tile, &high_pat).unwrap();
        assert_eq!(d.read_bytes(25, 5 * tile, 512).unwrap(), low_pat);
        assert_eq!(d.read_bytes(25, 40000 * tile, 512).unwrap(), high_pat);
        assert!(d.read_bytes(25, 0, 512).unwrap().iter().all(|&b| b == 0));

        // 2. Two reserved 256x256 RGBA8 textures (4 tiles each): #50 with 0 tiles mapped, #51 with tile 0 mapped.
        let tex_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let tex_subs = layout::texture(DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32, 256, 256, 1, 1, 1).unwrap();
        for id in [50i64, 51i64] {
            let mut tr: Option<ID3D12Resource> = None;
            unsafe { d.device.CreateReservedResource(&tex_desc, COMMON, None, &mut tr) }.unwrap();
            let tr = tr.unwrap();
            let layout = locum_capture::tiles::d3d12(&tr).unwrap();
            assert_eq!(layout.total, 4);
            d.memory.reserved_info.insert(
                id,
                memory::ReservedInfo {
                    layout: layout.clone(),
                    mapped_tiles: BTreeSet::new(),
                },
            );
            d.memory.mapped.insert(
                id,
                staging::Mapped {
                    layout,
                    tiles: BTreeSet::new(),
                },
            );
            d.adopt(
                id,
                tr,
                Heap::Default,
                DXGI_FORMAT_R8G8B8A8_UNORM.0 as u32,
                tex_subs.clone(),
            )
            .unwrap();
        }
        // Pre-map tile 0 of #51 and write data to it; `map all-unmapped` must preserve tile 0 while backing tiles 1..4.
        let mut h51: Option<ID3D12Heap> = None;
        unsafe {
            d.device.CreateHeap(
                &D3D12_HEAP_DESC {
                    SizeInBytes: tile,
                    Properties: D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    Alignment: tile,
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
                },
                &mut h51,
            )
        }
        .unwrap();
        let h51 = h51.unwrap();
        let r51 = d.resource(51).unwrap();
        let coord0 = D3D12_TILED_RESOURCE_COORDINATE::default();
        let sz1 = D3D12_TILE_REGION_SIZE {
            NumTiles: 1,
            ..Default::default()
        };
        let off0 = 0u32;
        let cnt1 = 1u32;
        unsafe {
            d.queue.UpdateTileMappings(
                &r51,
                1,
                Some(&coord0),
                Some(&sz1),
                &h51,
                1,
                None,
                Some(&off0),
                Some(&cnt1),
                D3D12_TILE_MAPPING_FLAG_NONE,
            );
        }
        d.memory.mapped.get_mut(&51).unwrap().tiles.insert(0);
        d.memory
            .reserved_info
            .get_mut(&51)
            .unwrap()
            .mapped_tiles
            .insert(0);
        let init_tex = pattern(layout::total(&tex_subs));
        d.put(51, &init_tex).unwrap();
        let before_51 = d.read(51).unwrap();
        assert_eq!(&before_51[..256], &init_tex[..256]);

        // Apply `map all-unmapped`: backs all 4 tiles of #50 + tiles 1..4 of #51, skips 4 GiB #25!
        let (rules, errors) = rules::parse_all(
            "map all-unmapped before 10",
            std::path::Path::new("."),
            &rules::hex,
        );
        assert!(errors.is_empty(), "{errors:?}");
        d.apply_state_rules(None, 10, None, &rules, false).unwrap();
        d.flush().unwrap();

        assert_eq!(d.memory.reserved_info[&50].mapped_tiles.len(), 4);
        assert_eq!(d.memory.reserved_info[&51].mapped_tiles.len(), 4);
        // #25 stayed sparse (still only its 2 touched tiles!).
        assert_eq!(d.memory.reserved_info[&25].mapped_tiles.len(), 2);
        // Tile 0 of #51 kept its original data untouched, and #50 now persists writes across the whole texture.
        assert_eq!(&d.read(51).unwrap()[..256], &init_tex[..256]);
        d.put(50, &init_tex).unwrap();
        assert_eq!(d.read(50).unwrap(), init_tex);
        assert!(d.messages().is_empty(), "D3D12 debug errors: {:?}", d.messages());
        drop(h51);
    }

    /// End-to-end GPU/WARP proof of runtime bindless slot tracing: compiles an SM 6.6 compute shader that loads
    /// dynamic SRV (slot 7) and UAV (slot 13) indices from a CBV, instruments its `createHandleFromHeap` calls,
    /// executes it on WARP under the D3D12 debug layer, and verifies the bitmask readback reports exact slots
    /// `[(7, read), (13, write)]` while preserving the shader's own output!
    #[test]
    fn runtime_bindless_trace_discovers_dynamic_cbv_loaded_slots_on_warp() {
        let mut d = D3d12::new(true, super::super::Validation::Layer).unwrap();
        let src = r#"
            cbuffer Params : register(b0) { uint srvIdx; uint uavIdx; };
            [numthreads(1,1,1)]
            void main() {
                ByteAddressBuffer s = ResourceDescriptorHeap[srvIdx];
                RWByteAddressBuffer u = ResourceDescriptorHeap[uavIdx];
                u.Store(0, s.Load(0) + 0x100);
            }
        "#;
        let orig = crate::dxil::compile(src, "cs_6_6").expect("compile cs_6_6");
        let log_slot = 64u32;
        let patched = crate::dxil::instrument_bindless(&orig, log_slot, log_slot)
            .expect("instrument_bindless")
            .expect("uses ResourceDescriptorHeap");

        // Root signature: 1 root CBV at b0 + CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED.
        let param = D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let rs_desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: 1,
            pParameters: &param,
            NumStaticSamplers: 0,
            pStaticSamplers: core::ptr::null(),
            Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED,
        };
        let mut blob = None;
        unsafe { D3D12SerializeRootSignature(&rs_desc, D3D_ROOT_SIGNATURE_VERSION_1_0, &mut blob, None) }
            .expect("serialize root sig");
        let blob = blob.unwrap();
        let rs_bytes = unsafe {
            core::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize())
        };
        let root_sig: ID3D12RootSignature =
            unsafe { d.device.CreateRootSignature(0, rs_bytes) }.expect("CreateRootSignature");

        let pso_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root_sig.clone())),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: patched.as_ptr().cast(),
                BytecodeLength: patched.len(),
            },
            ..Default::default()
        };
        let pso: ID3D12PipelineState =
            unsafe { d.device.CreateComputePipelineState(&pso_desc) }.expect("CreateComputePipelineState");
        let _ = ManuallyDrop::into_inner(pso_desc.pRootSignature);

        // Resources: CBV (#1) holding [7, 13], SRV source (#2) holding 0x42, UAV dest (#3) holding 0.
        let buf_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: 256,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        for id in [1i64, 2, 3] {
            d.adopt(
                id,
                committed(&d.device, &buf_desc),
                Heap::Default,
                0,
                layout::buffer_of(256),
            )
            .unwrap();
        }
        let mut cb_bytes = vec![0u8; 256];
        cb_bytes[0..4].copy_from_slice(&7u32.to_le_bytes());
        cb_bytes[4..8].copy_from_slice(&13u32.to_le_bytes());
        d.put(1, &cb_bytes).unwrap();
        let mut srv_bytes = vec![0u8; 256];
        srv_bytes[0..4].copy_from_slice(&0x42u32.to_le_bytes());
        d.put(2, &srv_bytes).unwrap();
        d.put(3, &vec![0u8; 256]).unwrap();

        // Descriptor heap of 65 slots: slot 7 = SRV #2, slot 13 = UAV #3, slot 64 = bindless trace log buffer.
        let heap = mirror(&d.device, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 65, true).unwrap();
        let srv_d = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 0,
                    NumElements: 64,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        let uav_d = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    FirstElement: 0,
                    NumElements: 64,
                    StructureByteStride: 0,
                    CounterOffsetInBytes: 0,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                },
            },
        };
        unsafe {
            d.device
                .CreateShaderResourceView(&d.resource(2).unwrap(), Some(&srv_d), heap.cpu(7));
            d.device
                .CreateUnorderedAccessView(&d.resource(3).unwrap(), None, Some(&uav_d), heap.cpu(13));
        }
        let log = d.make_bindless_log(log_slot, heap.cpu(log_slot)).unwrap();
        unsafe {
            d.list
                .CopyBufferRegion(&log.gpu_buf, 0, &log.zero_buf, 0, log.bytes);
            let b = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                        pResource: borrowed(&log.gpu_buf),
                        Subresource: ALL,
                        StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                        StateAfter: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    }),
                },
            };
            d.list.ResourceBarrier(&[b]);
            d.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
            d.list.SetComputeRootSignature(&root_sig);
            d.list.SetPipelineState(&pso);
            d.list.SetComputeRootConstantBufferView(0, d.address(1, 0));
            d.list.Dispatch(1, 1, 1);
            let b2 = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                        pResource: borrowed(&log.gpu_buf),
                        Subresource: ALL,
                        StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                    }),
                },
            };
            d.list.ResourceBarrier(&[b2]);
            d.list
                .CopyBufferRegion(&log.readback_buf, 0, &log.gpu_buf, 0, log.bytes);
            let b3 = D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                        pResource: borrowed(&log.gpu_buf),
                        Subresource: ALL,
                        StateBefore: D3D12_RESOURCE_STATE_COPY_SOURCE,
                        StateAfter: COMMON,
                    }),
                },
            };
            d.list.ResourceBarrier(&[b3]);
        }
        d.flush().unwrap();

        // Check shader output was preserved (0x42 + 0x100 = 0x142) AND exact runtime bindless slots were logged!
        let out3 = d.read_bytes(3, 0, 4).unwrap();
        assert_eq!(u32::from_le_bytes(out3.try_into().unwrap()), 0x142);
        let log_slice = unsafe { core::slice::from_raw_parts(log.readback_ptr, log.bytes as usize) };
        assert_eq!(
            crate::dxil::decode_bindless_log(log_slice, log_slot),
            vec![(7, false), (13, true)]
        );
        assert!(d.messages().is_empty(), "D3D12 debug errors: {:?}", d.messages());
    }

    #[test]
    fn runtime_bindless_trace_graphics_vs_ps_large_heap_on_warp() {
        let mut d = D3d12::new(true, super::super::Validation::Layer).expect("WARP D3D12 device");
        let vs_src = r#"
            float4 main(uint vid : SV_VertexID) : SV_Position {
                ByteAddressBuffer b = ResourceDescriptorHeap[5];
                float x = (vid == 1) ? 3.0 : -1.0;
                float y = (vid == 2) ? 3.0 : -1.0;
                return float4(x, y, float(b.Load(0) & 1) * 0.001, 1.0);
            }
        "#;
        let ps_src = r#"
            float4 main() : SV_Target {
                ByteAddressBuffer b = ResourceDescriptorHeap[11];
                return float4(float(b.Load(0)) / 255.0, 0.0, 0.0, 1.0);
            }
        "#;
        let log_slot: u32 = 999_999;
        let vs_raw = crate::dxil::compile(vs_src, "vs_6_6").expect("compile vs_6_6");
        let ps_raw = crate::dxil::compile(ps_src, "ps_6_6").expect("compile ps_6_6");
        let vs_bin = crate::dxil::instrument_bindless(&vs_raw, log_slot, log_slot)
            .expect("instrument vs")
            .expect("vs bindless");
        let ps_bin = crate::dxil::instrument_bindless(&ps_raw, log_slot, log_slot)
            .expect("instrument ps")
            .expect("ps bindless");
        let root_desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
            Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                    Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED,
                    ..Default::default()
                },
            },
        };
        let mut sig_blob = None;
        unsafe { D3D12SerializeVersionedRootSignature(&root_desc, &mut sig_blob, None) }.unwrap();
        let sig_blob = sig_blob.unwrap();
        let sig_bytes = unsafe {
            core::slice::from_raw_parts(sig_blob.GetBufferPointer().cast::<u8>(), sig_blob.GetBufferSize())
        };
        let root_sig: ID3D12RootSignature = unsafe { d.device.CreateRootSignature(0, sig_bytes) }.unwrap();
        let mut pso_desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root_sig.clone())),
            VS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: vs_bin.as_ptr().cast(),
                BytecodeLength: vs_bin.len(),
            },
            PS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: ps_bin.as_ptr().cast(),
                BytecodeLength: ps_bin.len(),
            },
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        pso_desc.BlendState.RenderTarget[0].RenderTargetWriteMask = 0xF;
        pso_desc.RTVFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        let pso: ID3D12PipelineState = unsafe { d.device.CreateGraphicsPipelineState(&pso_desc) }.unwrap();

        let heap = mirror(&d.device, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 1_000_000, true).unwrap();
        let rtv_heap = mirror(&d.device, D3D12_DESCRIPTOR_HEAP_TYPE_RTV, 1, false).unwrap();
        let rt_desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 8,
            Height: 8,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let mut rt: Option<ID3D12Resource> = None;
        unsafe {
            d.device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &rt_desc,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                    None,
                    &mut rt,
                )
                .unwrap();
        }
        let rt = rt.unwrap();
        unsafe { d.device.CreateRenderTargetView(&rt, None, rtv_heap.cpu(0)) };

        // Make two small raw SRV buffers at slots 5 and 11
        let mk_buf = |id: i64, val: u32| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            };
            let mut r: Option<ID3D12Resource> = None;
            unsafe {
                d.device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        COMMON,
                        None,
                        &mut r,
                    )
                    .unwrap();
            }
            let r = r.unwrap();
            let _ = (id, val);
            r
        };
        let b5 = mk_buf(5, 1);
        let b11 = mk_buf(11, 255);
        let srv_d = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    NumElements: 64,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        unsafe {
            d.device.CreateShaderResourceView(&b5, Some(&srv_d), heap.cpu(5));
            d.device
                .CreateShaderResourceView(&b11, Some(&srv_d), heap.cpu(11));
        }
        let log = d.make_bindless_log(log_slot, heap.cpu(log_slot)).unwrap();
        let vp = D3D12_VIEWPORT {
            Width: 8.0,
            Height: 8.0,
            MaxDepth: 1.0,
            ..Default::default()
        };
        let sc = RECT {
            left: 0,
            top: 0,
            right: 8,
            bottom: 8,
        };
        unsafe {
            d.list
                .CopyBufferRegion(&log.gpu_buf, 0, &log.zero_buf, 0, log.bytes);
            d.list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                        pResource: borrowed(&log.gpu_buf),
                        Subresource: ALL,
                        StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                        StateAfter: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    }),
                },
            }]);
            d.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
            d.list.SetGraphicsRootSignature(&root_sig);
            d.list.SetPipelineState(&pso);
            d.list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            d.list.RSSetViewports(&[vp]);
            d.list.RSSetScissorRects(&[sc]);
            let rtv_h = rtv_heap.cpu(0);
            d.list.OMSetRenderTargets(1, Some(&rtv_h), false, None);
            d.list.DrawInstanced(3, 1, 0, 0);
            d.list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                        pResource: borrowed(&log.gpu_buf),
                        Subresource: ALL,
                        StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                    }),
                },
            }]);
            d.list
                .CopyBufferRegion(&log.readback_buf, 0, &log.gpu_buf, 0, log.bytes);
        }
        d.flush().unwrap();
        let log_slice = unsafe { core::slice::from_raw_parts(log.readback_ptr, log.bytes as usize) };
        assert_eq!(
            crate::dxil::decode_bindless_log(log_slice, log_slot),
            vec![(5, false), (11, false)]
        );
        assert!(d.messages().is_empty(), "D3D12 debug errors: {:?}", d.messages());
    }

    /// Verifies that `ExecuteIndirect` whose argument buffer had no captured contents (and was not written earlier in
    /// the plan) skips cleanly with a diagnostic warning instead of dispatching a zeroed buffer that crashes WARP/GBV.
    #[test]
    fn execute_indirect_missing_args_skips_gracefully_on_warp() {
        let p1 = std::path::Path::new(
            r"C:\Users\kalmarv\modding\.targets\xcl-sky-captures\P1LEVEL\captures\depth-plane\capture.db",
        );
        if !p1.is_file() {
            return;
        }
        let c = crate::open::open(&p1.to_string_lossy(), None).unwrap();
        let mut d = D3d12::new(true, super::super::Validation::Gpu).unwrap();
        // Create #6250 with no contents (as in P1LEVEL draw 16068 / event 135295).
        d.create_blob(&c, 6250, None).unwrap();
        let args: Value = serde_json::json!([1247, 1, 6250, 34768, 0, 0]);
        let resolved = d.indirect_arguments(&c, 135295, &args).unwrap();
        assert!(matches!(resolved, indirect::Arguments::Skip));
        assert!(
            d.warnings
                .iter()
                .any(|w| w.contains("#6250 (+34768)") && w.contains("all zeros")),
            "expected missing indirect-arg warning, got {:?}",
            d.warnings
        );
        assert!(d.messages().is_empty(), "D3D12 debug errors: {:?}", d.messages());
    }

    /// Verifies that WARP+GBV skips large directly-indexed shaders (like P1LEVEL draw 16068's 193 KiB PS `43b649be`)
    /// cleanly with an actionable warning instead of crashing inside `d3d10warp.dll` with `0xc0000005`.
    #[test]
    fn warp_gbv_skips_large_bindless_shader_without_crashing() {
        let p1 = std::path::Path::new(
            r"C:\Users\kalmarv\modding\.targets\xcl-sky-captures\P1LEVEL\captures\depth-plane\capture.db",
        );
        if !p1.is_file() {
            return;
        }
        let c = crate::open::open(&p1.to_string_lossy(), None).unwrap();
        let mut d = D3d12::new(true, super::super::Validation::Gpu).unwrap();
        d.enable_bindless_trace();
        let args: Value = serde_json::json!([1247, 1, 6250, 34768, 0, 0]);
        d.draw(
            &c,
            135295,
            "draw",
            "ExecuteIndirect",
            &args,
            &[],
            indirect::Arguments::AsRecorded,
        )
        .unwrap();
        assert!(
            d.warnings
                .iter()
                .any(|w| w.contains("43b649bed15f98568c4f613872d6ac4f") && w.contains("d3d10warp.dll")),
            "expected WARP+GBV guard warning, got {:?}",
            d.warnings
        );
        assert!(d.messages().is_empty(), "D3D12 debug errors: {:?}", d.messages());
    }

    /// Verifies that a `seed` rule applied before an instrumented bindless dispatch populates a gate buffer so a
    /// dynamically gated `ResourceDescriptorHeap` slot becomes reachable during bindless tracing.
    #[test]
    fn seed_rule_unlocks_gated_bindless_slot_during_trace_on_warp() {
        let mut d = D3d12::new(true, super::super::Validation::Layer).unwrap();
        let src = r#"
            cbuffer Gate : register(b0) { uint enabled; uint srvIdx; };
            [numthreads(1,1,1)]
            void main() {
                if (enabled != 0) {
                    ByteAddressBuffer s = ResourceDescriptorHeap[srvIdx];
                    RWByteAddressBuffer u = ResourceDescriptorHeap[9];
                    u.Store(0, s.Load(0));
                }
            }
        "#;
        let log_slot: u32 = 31;
        let raw_cs = crate::dxil::compile(src, "cs_6_6").unwrap();
        let patched_cs = crate::dxil::instrument_bindless(&raw_cs, log_slot, log_slot)
            .unwrap()
            .unwrap();
        let param = D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Flags: D3D12_ROOT_DESCRIPTOR_FLAG_NONE,
                },
            },
        };
        let root_desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
            Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                    NumParameters: 1,
                    pParameters: &param,
                    Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED,
                    ..Default::default()
                },
            },
        };
        let mut sig_blob = None;
        unsafe { D3D12SerializeVersionedRootSignature(&root_desc, &mut sig_blob, None) }.unwrap();
        let sig_blob = sig_blob.unwrap();
        let sig_bytes = unsafe {
            core::slice::from_raw_parts(sig_blob.GetBufferPointer().cast::<u8>(), sig_blob.GetBufferSize())
        };
        let root_sig: ID3D12RootSignature = unsafe { d.device.CreateRootSignature(0, sig_bytes) }.unwrap();
        let pso_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root_sig.clone())),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: patched_cs.as_ptr().cast(),
                BytecodeLength: patched_cs.len(),
            },
            ..Default::default()
        };
        let pso: ID3D12PipelineState = unsafe { d.device.CreateComputePipelineState(&pso_desc) }.unwrap();

        let mk_buf = |d: &mut D3d12, id: i64| {
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            };
            let mut r: Option<ID3D12Resource> = None;
            unsafe {
                d.device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        COMMON,
                        None,
                        &mut r,
                    )
                    .unwrap();
            }
            d.adopt(id, r.unwrap(), Heap::Default, 0, layout::buffer(256))
                .unwrap();
        };
        mk_buf(&mut d, 1); // gate CBV (starts all zeros -> branch disabled)
        mk_buf(&mut d, 2); // SRV at slot 5
        mk_buf(&mut d, 3); // UAV at slot 9
        let heap = mirror(&d.device, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV, 32, true).unwrap();
        let srv_d = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    NumElements: 64,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        let uav_d = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: 64,
                    Flags: D3D12_BUFFER_UAV_FLAG_RAW,
                    ..Default::default()
                },
            },
        };
        unsafe {
            d.device
                .CreateShaderResourceView(&d.resource(2).unwrap(), Some(&srv_d), heap.cpu(5));
            d.device
                .CreateUnorderedAccessView(&d.resource(3).unwrap(), None, Some(&uav_d), heap.cpu(9));
        }
        let log = d.make_bindless_log(log_slot, heap.cpu(log_slot)).unwrap();

        let run_once = |d: &mut D3d12| -> Vec<(u32, bool)> {
            unsafe {
                d.list
                    .CopyBufferRegion(&log.gpu_buf, 0, &log.zero_buf, 0, log.bytes);
                d.list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: borrowed(&log.gpu_buf),
                            Subresource: ALL,
                            StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                            StateAfter: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                        }),
                    },
                }]);
                d.list.SetDescriptorHeaps(&[Some(heap.heap.clone())]);
                d.list.SetComputeRootSignature(&root_sig);
                d.list.SetPipelineState(&pso);
                d.list.SetComputeRootConstantBufferView(0, d.address(1, 0));
                d.list.Dispatch(1, 1, 1);
                d.list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: borrowed(&log.gpu_buf),
                            Subresource: ALL,
                            StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                            StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                        }),
                    },
                }]);
                d.list
                    .CopyBufferRegion(&log.readback_buf, 0, &log.gpu_buf, 0, log.bytes);
                d.list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                            pResource: borrowed(&log.gpu_buf),
                            Subresource: ALL,
                            StateBefore: D3D12_RESOURCE_STATE_COPY_SOURCE,
                            StateAfter: COMMON,
                        }),
                    },
                }]);
            }
            d.flush().unwrap();
            let slice = unsafe { core::slice::from_raw_parts(log.readback_ptr, log.bytes as usize) };
            crate::dxil::decode_bindless_log(slice, log_slot)
        };

        // Without seed rule: gate CBV is 0 -> 0 slots discovered.
        assert_eq!(run_once(&mut d), vec![]);

        // With `seed 1 from <file> before 100` rule: gate CBV gets enabled=1, srvIdx=5 -> slots (5, read) & (9, write)!
        let seed_path = std::env::temp_dir().join("locum_test_seed_bindless_gate.bin");
        let mut seed_data = vec![0u8; 256];
        seed_data[0..4].copy_from_slice(&1u32.to_le_bytes());
        seed_data[4..8].copy_from_slice(&5u32.to_le_bytes());
        std::fs::write(&seed_path, &seed_data).unwrap();
        let rule = rules::parse(
            &format!("seed 1 from {} before 100", seed_path.display()),
            std::path::Path::new("."),
            &|_, _| Err(String::new()),
        )
        .unwrap();
        d.apply_state_rules(None, 100, None, &[rule], false).unwrap();
        let _ = std::fs::remove_file(&seed_path);

        assert_eq!(run_once(&mut d), vec![(5, false), (9, true)]);
        assert!(d.messages().is_empty(), "D3D12 debug errors: {:?}", d.messages());
    }
}
