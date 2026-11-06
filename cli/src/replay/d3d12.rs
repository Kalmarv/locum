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

mod indirect;
mod memory;
mod pipelines;
mod queries;
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
    heap: Heap,
    /// A texture replay moves between states with barriers (see the module's note).
    tracked: bool,
    extent: Extent,
    /// The canonical layout of its contents, and for a depth-stencil format the bytes of its joined texel.
    layout: Vec<Sub>,
    joined: Option<u32>,
    /// Where a copy of each D3D12 subresource (planes included) lies in a buffer, as the API gives it and as the
    /// canonical layout reads it, and that buffer's size.
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

/// A transition of a tracked texture around one event: its subresources and the state they take.
struct Need {
    resource: i64,
    subs: Vec<u32>,
    state: i32,
}

pub struct D3d12 {
    device: ID3D12Device2,
    queue: ID3D12CommandQueue,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
    fence: ID3D12Fence,
    fenced: u64,
    /// Buffers the open list copies from or into, kept until it completes.
    kept: Vec<ID3D12Resource>,
    resources: HashMap<i64, Made>,
    /// Mirrors of the game's shader-visible heaps (None for a heap that is not, or could not be made).
    heaps: HashMap<i64, Option<Mirror>>,
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
    /// The game's buffers by GPU address, for the addresses in ExecuteIndirect's argument records.
    addresses: Option<indirect::Addresses>,
    /// By command list execution, its depth bounds and shading rate calls, which `d12_draw_state` does not hold.
    list_state: Option<HashMap<i64, Vec<ListCall>>>,
    replaced: HashMap<String, Option<Vec<u8>>>,
    /// Heaps, and the resources made in them.
    memory: memory::Memory,
    queries: queries::Queries,
    /// Resources the frame uses that the capture holds no description of.
    undescribed: Vec<i64>,
    warnings: BTreeSet<String>,
    debug: Option<ID3D12InfoQueue>,
}

/// The resources each resource of a Direct3D 12 capture shares memory with (see [`memory`]).
pub fn aliases(c: &Capture) -> HashMap<i64, BTreeSet<i64>> {
    memory::aliases(c)
}

/// For planning: the events that begin, end, count into or resolve the frame's queries, with the queries (as
/// resources of negative id) each reads and writes.
pub fn query_touches(c: &Capture) -> Vec<(i64, Vec<i64>, Vec<i64>)> {
    queries::touches(c)
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
            Ok(D3d12 {
                device,
                queue,
                allocator,
                list,
                fence,
                fenced: 0,
                kept: Vec::new(),
                resources: HashMap::new(),
                heaps: HashMap::new(),
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
                list_state: None,
                replaced: HashMap::new(),
                memory: memory::Memory::default(),
                queries: queries::Queries::default(),
                undescribed: Vec::new(),
                warnings: BTreeSet::new(),
                debug,
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
        self.kept.clear();
        unsafe {
            self.allocator
                .Reset()
                .map_err(|e| format!("command allocator Reset: {e}"))?;
            self.list
                .Reset(&self.allocator, None)
                .map_err(|e| format!("command list Reset: {e}"))?;
        }
        removed.map_err(|e| {
            format!(
                "the device was removed: {e}{}{}",
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

    /// Puts in place the descriptors and upload bytes of every submission up to `submission`.
    fn enter(&mut self, c: &Capture, submission: i64) -> Result<(), String> {
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
        while self.applied <= at {
            let (s, cpu_seq) = self.submissions.as_ref().expect("ordered")[self.applied];
            self.applied += 1;
            // The changes to tile mappings made before it, then the first-use contents copied at it, as the frame had
            // them when it ran.
            self.remap_before(c, cpu_seq)?;
            self.first_uses(s)?;
            let versions: Vec<Version> = c.rows(
                &format!(
                    "SELECT {VERSION_COLUMNS} FROM d12_submission_descriptors s JOIN d12_descriptor_versions v \
                     ON v.id = s.version WHERE s.submission = ?1 ORDER BY v.id"
                ),
                [s],
                version,
            );
            for v in &versions {
                self.descriptor(c, v);
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
                if j["whole"] == true {
                    self.resources.get_mut(&id).expect("made").from = Some(key.to_string());
                }
            }
        }
        Ok(())
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
                    match mirror(&self.device, kind, count, true) {
                        Ok(m) => Some(m),
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

    /// Writes a recorded descriptor into its slot of the mirror of its heap.
    fn descriptor(&mut self, c: &Capture, v: &Version) {
        let Some(m) = self.heap(c, v.heap) else {
            return;
        };
        if v.slot >= m.count {
            let count = m.count;
            self.warn(format!(
                "descriptor heap #{}: slot {} is past its {count} descriptors",
                v.heap, v.slot
            ));
            return;
        }
        let handle = m.cpu(v.slot);
        self.view(&v.kind, v.resource, v.counter, &v.description, handle);
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
                "srv" if desc["dimension"] == 11 => {
                    self.warn("acceleration structure views are not replayed");
                }
                "srv" if default => {
                    if let Some(r) = &r {
                        self.device.CreateShaderResourceView(r, None, handle)
                    }
                }
                "srv" => self
                    .device
                    .CreateShaderResourceView(r.as_ref(), Some(&views::srv(desc)), handle),
                "uav" => {
                    let counter = self.resources.get(&counter).map(|m| m.resource.clone());
                    if default {
                        if let Some(r) = &r {
                            self.device
                                .CreateUnorderedAccessView(r, counter.as_ref(), None, handle)
                        }
                    } else {
                        self.device.CreateUnorderedAccessView(
                            r.as_ref(),
                            counter.as_ref(),
                            Some(&views::uav(desc)),
                            handle,
                        )
                    }
                }
                "cb" => {
                    let d = match views::cbv(desc) {
                        Some((buffer, offset, bytes)) => D3D12_CONSTANT_BUFFER_VIEW_DESC {
                            BufferLocation: self.address(buffer, offset),
                            SizeInBytes: if self.resources.contains_key(&buffer) {
                                bytes
                            } else {
                                0
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
                                            " (it has an incrementing constant, which needs ExecuteIndirect tier 1.1;                                              the replay device's is {}.{})",
                                            o.ExecuteIndirectTier.0 / 10,
                                            o.ExecuteIndirectTier.0 % 10
                                        ),
                                        true => String::new(),
                                        false => " (it has an incrementing constant, which needs ExecuteIndirect tier                                                    1.1; the replay device's runtime knows no tiers)"
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
        if applying.iter().any(|r| r.action == Action::Skip) {
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
        let Some(pso) = self.pso(c, pso_id, texts, &replaced) else {
            self.warn(format!("draws of pipeline state #{pso_id} were not run"));
            return Ok(());
        };
        let root_id = if compute { compute_root } else { graphics_root };
        let Some((root, decoded)) = self
            .root(c, root_id)
            .map(|r| (r.signature.clone(), r.decoded.clone()))
        else {
            self.warn(format!("draws with root signature #{root_id} were not run"));
            return Ok(());
        };
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

        let list = self.list.clone();
        unsafe {
            list.SetPipelineState(&pso);
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
                "ExecuteIndirect" => {
                    let signature = self.signature(c, a(0) as i64);
                    let (Some(signature), false) =
                        (signature, matches!(arguments, indirect::Arguments::Skip))
                    else {
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
                        Ok(l) => l.OMSetDepthBounds(
                            a[0].as_f64().unwrap_or(0.0) as f32,
                            a[1].as_f64().unwrap_or(1.0) as f32,
                        ),
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
                    let (dst, src) = (self.resource(u(a, 0) as i64)?, self.resource(u(a, 2) as i64)?);
                    list.CopyBufferRegion(&dst, u(a, 1), &src, u(a, 3), u(a, 4));
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

    fn clear(&mut self, c: &Capture, call: &str, a: &Value) -> Result<(), String> {
        let list = self.list.clone();
        match call {
            "ClearRenderTargetView" => {
                let d = &a[0]["definition"];
                let resource = d["resource"].as_i64().unwrap_or(0);
                if !self.resources.contains_key(&resource) {
                    return Ok(());
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
                unsafe {
                    list.ClearRenderTargetView(handle, &floats(&a[1]), (!r.is_empty()).then_some(&r[..]))
                };
                self.barriers(&needs, true);
            }
            "ClearDepthStencilView" => {
                let d = &a[0]["definition"];
                let resource = d["resource"].as_i64().unwrap_or(0);
                if !self.resources.contains_key(&resource) {
                    return Ok(());
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
                        a[2].as_f64().unwrap_or(0.0) as f32,
                        u(a, 3) as u8,
                        (!r.is_empty()).then_some(&r[..]),
                    )
                };
                self.barriers(&needs, true);
            }
            "ClearUnorderedAccessViewUint" | "ClearUnorderedAccessViewFloat" => {
                let resource = u(a, 2) as i64;
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
                        let v = [0, 1, 2, 3].map(|i| a[3][i].as_u64().unwrap_or(0) as u32);
                        list.ClearUnorderedAccessViewUint(gpu, handle, &target, &v, &r);
                    } else {
                        list.ClearUnorderedAccessViewFloat(gpu, handle, &target, &floats(&a[3]), &r);
                    }
                }
                self.barriers(&needs, true);
            }
            other => self.warn(format!("{other} is not replayed")),
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
    fn create(&mut self, c: &Capture, id: i64, contents: Option<&[u8]>) -> Result<(), String> {
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
        // In the memory the game shared, with the game's own flags, so it takes what it took there.
        let shared = match placement {
            memory::Placement::Committed => None,
            p => match self.make_shared(c, id, p, &desc, state) {
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
                make(&desc).map_err(|e| e.to_string())?
            }
        };
        name(&resource, format!("#{id}"));
        let desc = unsafe { resource.GetDesc() };
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
        let (placed, footprints, copy_bytes) = if buffer {
            (Vec::new(), Vec::new(), desc.Width)
        } else {
            let count = extent.count() as usize;
            let mut placed = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); count];
            let mut rows = vec![0u32; count];
            let mut row_bytes = vec![0u64; count];
            let mut total = 0u64;
            unsafe {
                self.device.GetCopyableFootprints(
                    &desc,
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
        let joined =
            (planes == 2).then(|| locum_capture::format::layout(format).map_or(4, |f| f.block_bytes));
        self.resources.insert(
            id,
            Made {
                resource,
                desc,
                heap,
                tracked: !buffer && desc.Flags.0 & SIMULTANEOUS == 0,
                extent,
                layout: s.subs(),
                joined,
                placed,
                footprints,
                copy_bytes,
                va,
                from: None,
            },
        );
        let subs = s.subs();
        match contents.filter(|b| b.len() as u64 == layout::total(&subs)) {
            Some(_) if samples > 1 => {
                self.warn("multisampled textures start empty (their contents were read back resolved)");
            }
            // In shared memory, written when replay reaches the submission they were copied at.
            Some(_) if self.memory.shared.contains(&id) && self.defer(c, id) => {}
            Some(bytes) => self.write(c, id, bytes)?,
            None => {}
        }
        if heap.mapped() {
            self.resources.get_mut(&id).expect("made").from = c
                .row("SELECT content FROM resources WHERE id = ?1", [id], |r| r.get(0))
                .flatten();
        }
        Ok(())
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
            self.enter(c, s)?;
        }
        match kind.as_str() {
            "draw" | "dispatch" => {
                // Before the event records anything: reading its argument records runs the open list.
                let arguments = if call == "ExecuteIndirect" {
                    self.indirect_arguments(c, seq, &a)?
                } else {
                    indirect::Arguments::AsRecorded
                };
                self.count_begin(list)?;
                self.draw(c, seq, &kind, &call, &a, rules, arguments)?;
                self.count_end()?;
            }
            "copy" if call == "ResolveQueryData" => self.resolve_queries(&a)?,
            "copy" => self.copy(&call, &a)?,
            "clear" => self.clear(c, &call, &a)?,
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
            "barrier" if call == "ResourceBarrier" => self.aliasing(&a),
            "discard" => self.discard(&a),
            "state" if call == "BeginQuery" => self.query_begin(&a, list),
            "state" if call == "EndQuery" => self.query_end(&a)?,
            "state" | "barrier" | "submit" | "bundle" => {}
            other => self.warn(format!("events of kind {other} ({call}) are not replayed")),
        }
        let ran = self.flush().and_then(|()| self.collect_queries());
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
            Some(s) => self.enter(c, s),
            None => Ok(()),
        }
    }

    fn read(&mut self, id: i64) -> Result<Vec<u8>, String> {
        self.first_use_now(id)?;
        let m = self.made(id)?;
        if m.desc.SampleDesc.Count > 1 {
            return Err(format!("#{id} is multisampled, which replay cannot read back"));
        }
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
        let (readback, p) = self.staging(D3D12_HEAP_TYPE_READBACK, bytes)?;
        let needs = [self.need_all(id, 0x800)];
        unsafe {
            if buffer {
                self.list.CopyBufferRegion(&readback, 0, &r, 0, bytes);
            } else {
                if tracked {
                    self.barriers(&needs, false);
                }
                let m = &self.resources[&id];
                for (i, f) in m.placed.iter().enumerate() {
                    let dst = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: borrowed(&readback),
                        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *f },
                    };
                    let src = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: borrowed(&r),
                        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                            SubresourceIndex: i as u32,
                        },
                    };
                    self.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                }
                if tracked {
                    self.barriers(&needs, true);
                }
            }
        }
        self.flush()?;
        let data = unsafe { core::slice::from_raw_parts(p.cast_const(), bytes as usize) };
        let m = &self.resources[&id];
        if buffer {
            return Ok(data.to_vec());
        }
        layout::from_footprints(&m.layout, m.joined, &m.footprints, data)
    }

    fn write(&mut self, _c: &Capture, id: i64, contents: &[u8]) -> Result<(), String> {
        self.put(id, contents)
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
        notes
    }
}

impl D3d12 {
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
            return self.map_write(id, 0, contents);
        }
        let (r, buffer, bytes, tracked) = (m.resource.clone(), m.buffer(), m.copy_bytes, m.tracked);
        let (upload, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, bytes)?;
        let staged = unsafe { core::slice::from_raw_parts_mut(p, bytes as usize) };
        let needs = [self.need_all(id, 0x400)];
        unsafe {
            if buffer {
                staged[..contents.len()].copy_from_slice(contents);
                self.list
                    .CopyBufferRegion(&r, 0, &upload, 0, contents.len() as u64);
            } else {
                let m = &self.resources[&id];
                layout::into_footprints(&m.layout, m.joined, &m.footprints, contents, staged)?;
                if tracked {
                    self.barriers(&needs, false);
                }
                for (i, f) in m.placed.iter().enumerate() {
                    let dst = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: borrowed(&r),
                        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                            SubresourceIndex: i as u32,
                        },
                    };
                    let src = D3D12_TEXTURE_COPY_LOCATION {
                        pResource: borrowed(&upload),
                        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *f },
                    };
                    self.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
                }
                if tracked {
                    self.barriers(&needs, true);
                }
            }
        }
        self.flush()
    }
}
