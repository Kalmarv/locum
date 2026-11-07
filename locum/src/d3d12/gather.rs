//! A top-level build's instances given as an array of pointers, gathered on the GPU, armed or not, so a capture
//! holds each acceleration structure such a build made before its frame (see [`super::rays`]). The pointers are
//! GPU memory: where the instances are is not known when the build is recorded, so nothing can be copied from them
//! in the application's list.
//!
//! Right after the build, in the application's list, the pointers are copied into a buffer of locum's (as other
//! inputs are copied, see [`super::passes::record_ranges`]): the application cannot change them before its next
//! barrier, so the copy holds the pointers the build read. Where the list is submitted, a list of locum's follows
//! it in the same ExecuteCommandLists: a dispatch turns each pointer into a record of a command signature of
//! locum's (the pointer as a root SRV, its place as a root constant, one group, or none for a null pointer), and
//! ExecuteIndirect copies each instance it points at into place, with a stamp saying the record ran. Those, and the
//! pointers as read, are copied into readback memory.
//!
//! The gather reads the instances after the whole of the building list, so it holds what the build read unless the
//! list wrote them after the build. That is decided once the pointers are known, from the list's recorded commands
//! after the build ([`analyse`], [`verdict`]): without enhanced barriers, the build reads its instances in
//! NON_PIXEL_SHADER_RESOURCE, a read state, so the list writes them only after a transition of their buffer to a
//! state it can write (an aliasing barrier, after which other memory may be the instances', gathers nothing); with
//! enhanced barriers, only after a barrier whose sync waits for the build, after which every command that writes
//! what locum cannot name makes the instances unknown. Placed buffers that share memory share GPU addresses, so
//! writes are compared by address. Anything else is a `d3d12-acceleration-structures` gap, with the reason.
//!
//! The list of locum's reads the instances' buffers in NON_PIXEL_SHADER_RESOURCE: a buffer the building list left
//! (after the build) in a state without it is moved there and back around the gather, so the next list finds it as
//! the application left it.
use std::collections::{HashMap, HashSet};
use std::mem::ManuallyDrop;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use locum_capture::d3d12 as model;
use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::passes::borrowed;

macro_rules! program {
    ($name:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".dxbc"))
    };
}

const ROOT: &[u8] = program!("gather_root");
const RECORDS: &[u8] = program!("gather_records");
const COPY: &[u8] = program!("gather_copy");

/// The bytes of an instance description, of a pointer to one, of a stamp, and of a record of locum's command
/// signature (the root SRV's address, the constant, a dispatch).
const INSTANCE: u64 = 64;
const POINTER: u64 = 8;
const STAMP: u64 = 4;
const RECORD: u64 = 24;
/// The root parameters (see `gather.hlsl`): the root SRV, the constant, and the first of the four UAVs.
const SOURCE: u32 = 0;
const CONSTANT: u32 = 1;
const UAV: u32 = 2;
/// The threads of a group of `records`.
const GROUP: u32 = 64;

/// D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY and END_ONLY.
const BEGIN_ONLY: u64 = 1;
const END_ONLY: u64 = 2;

const NPSR: i32 = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0;
/// The states in which nothing writes a buffer.
const READ_ONLY: i32 = D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER.0
    | D3D12_RESOURCE_STATE_INDEX_BUFFER.0
    | D3D12_RESOURCE_STATE_DEPTH_READ.0
    | NPSR
    | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0
    | D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT.0
    | D3D12_RESOURCE_STATE_COPY_SOURCE.0
    | D3D12_RESOURCE_STATE_RESOLVE_SOURCE.0
    | D3D12_RESOURCE_STATE_SHADING_RATE_SOURCE.0
    | D3D12_RESOURCE_STATE_VIDEO_DECODE_READ.0
    | D3D12_RESOURCE_STATE_VIDEO_PROCESS_READ.0
    | D3D12_RESOURCE_STATE_VIDEO_ENCODE_READ.0;
/// A buffer an acceleration structure is in stays in this state: it holds no instances and is never moved.
const STRUCTURE: i32 = D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE.0;

/// Whether a buffer in state `s` can be written: COMMON can be promoted to a state that writes.
fn writable(s: i32) -> bool {
    s == 0 || s & !READ_ONLY != 0
}

/// The readback bytes of a gather of `count` instances: the instances, the pointers as read, the stamps.
pub fn bytes(count: u64) -> u64 {
    count * (INSTANCE + POINTER + STAMP)
}

/// Where the pointers and the stamps are in a gather's readback (and in its buffer of locum's) of `count`.
fn pointers_at(count: u64) -> u64 {
    count * INSTANCE
}
fn stamps_at(count: u64) -> u64 {
    count * (INSTANCE + POINTER)
}

/// What every gather on a device runs: the root signature, the two pipelines and the command signature.
struct Pipelines {
    root: ID3D12RootSignature,
    records: ID3D12PipelineState,
    copy: ID3D12PipelineState,
    signature: ID3D12CommandSignature,
}

/// The pipelines of the real device `device`, made once while some gather holds them.
unsafe fn pipelines(device: &ID3D12Device) -> Result<Arc<Pipelines>, String> {
    static MADE: Mutex<Vec<(usize, Weak<Pipelines>)>> = Mutex::new(Vec::new());
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    made.retain(|(_, p)| p.strong_count() > 0);
    let key = device.as_raw() as usize;
    if let Some(p) = made
        .iter()
        .find(|(k, _)| *k == key)
        .and_then(|(_, p)| p.upgrade())
    {
        return Ok(p);
    }
    let root: ID3D12RootSignature = unsafe { device.CreateRootSignature(0, ROOT) }
        .map_err(|e| format!("the gather's root signature: {e}"))?;
    let pipeline = |code: &[u8]| {
        let mut desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            },
            ..Default::default()
        };
        let made = unsafe { device.CreateComputePipelineState::<ID3D12PipelineState>(&desc) }
            .map_err(|e| format!("the gather's pipeline: {e}"));
        unsafe { ManuallyDrop::drop(&mut desc.pRootSignature) };
        made
    };
    let (records, copy) = (pipeline(RECORDS)?, pipeline(COPY)?);
    let arguments = [
        D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW,
            Anonymous: D3D12_INDIRECT_ARGUMENT_DESC_0 {
                ShaderResourceView: D3D12_INDIRECT_ARGUMENT_DESC_0_3 {
                    RootParameterIndex: SOURCE,
                },
            },
        },
        D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT,
            Anonymous: D3D12_INDIRECT_ARGUMENT_DESC_0 {
                Constant: D3D12_INDIRECT_ARGUMENT_DESC_0_1 {
                    RootParameterIndex: CONSTANT,
                    DestOffsetIn32BitValues: 0,
                    Num32BitValuesToSet: 1,
                },
            },
        },
        D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH,
            ..Default::default()
        },
    ];
    let mut signature: Option<ID3D12CommandSignature> = None;
    unsafe {
        device.CreateCommandSignature(
            &D3D12_COMMAND_SIGNATURE_DESC {
                ByteStride: RECORD as u32,
                NumArgumentDescs: arguments.len() as u32,
                pArgumentDescs: arguments.as_ptr(),
                NodeMask: 0,
            },
            &root,
            &mut signature,
        )
    }
    .map_err(|e| format!("the gather's command signature: {e}"))?;
    let p = Arc::new(Pipelines {
        root,
        records,
        copy,
        signature: signature.ok_or("no command signature")?,
    });
    made.push((key, Arc::downgrade(&p)));
    Ok(p)
}

/// A buffer of locum's in a default heap that shaders can write, in COMMON (as buffers are made).
unsafe fn buffer(device: &ID3D12Device, bytes: u64) -> Result<ID3D12Resource, String> {
    let mut made: Option<ID3D12Resource> = None;
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: bytes.max(1),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut made,
        )
    }
    .map_err(|e| format!("a buffer of the gather's: {e}"))?;
    made.ok_or_else(|| "no buffer".into())
}

/// One build's gather: where its list holds the build, how many instances, and the buffers it runs on.
pub struct Gather {
    /// The build's place among its list's commands.
    pub at: usize,
    pub count: u32,
    /// The pointers, copied in the application's list (then COPY_DEST, by promotion); the records; the instances,
    /// pointers as read and stamps.
    pointers: ID3D12Resource,
    records: ID3D12Resource,
    out: ID3D12Resource,
    pipelines: Arc<Pipelines>,
    /// What the list does after the build, decided at its first submission (an error: no gather is made).
    pub analysis: OnceLock<Result<Analysis, String>>,
    /// Why the latest submission's gather was not made.
    pub failed: Mutex<Option<String>>,
}

/// Right after a top-level build of `count` instances given as an array of pointers (`source`, read in the
/// application's real list `list` at the build's place `at`): the pointers copied into a buffer of locum's (see the
/// module's documentation). Returns the gather and the buffers the copy read by promotion.
///
/// # Safety
/// `list` is the application's real list, open for recording, of the real device `device`.
pub unsafe fn prepare(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    source: super::passes::Source,
    count: u32,
    at: usize,
) -> Result<(Gather, Vec<u64>), String> {
    let n = u64::from(count);
    let pipelines = unsafe { pipelines(device) }?;
    let (pointers, records, out) = unsafe {
        (
            buffer(device, n * POINTER)?,
            buffer(device, n * RECORD)?,
            buffer(device, bytes(n))?,
        )
    };
    let promoted = unsafe {
        super::passes::record_ranges(
            list,
            &[source],
            D3D12_BARRIER_ACCESS_SHADER_RESOURCE,
            None,
            &pointers,
        )
    }?;
    Ok((
        Gather {
            at,
            count,
            pointers,
            records,
            out,
            pipelines,
            analysis: OnceLock::new(),
            failed: Mutex::new(None),
        },
        promoted,
    ))
}

/// What a list does after a build whose instances are gathered (see [`analyse`]).
#[derive(Debug, Default, PartialEq)]
pub struct Analysis {
    /// The buffers it leaves, after the build, in a state without NON_PIXEL_SHADER_RESOURCE: each with that state.
    pub wraps: Vec<(u64, i32)>,
    /// The GPU addresses it may write after the build, from the first to past the last, each with why.
    pub writes: Vec<(u64, u64, String)>,
    /// Why what it writes after the build is not known.
    pub unknown: Option<String>,
}

/// The resources whole command `c` writes, of those that write only what they name; None for one that writes
/// nothing, an error for one whose writes locum does not name.
fn written(c: &model::Command) -> Result<Vec<Option<u64>>, ()> {
    const NONE: &[&str] = &[
        "ResourceBarrier",
        "Barrier",
        "BeginEvent",
        "EndEvent",
        "SetMarker",
        "BeginQuery",
        "EndQuery",
        "ClearState",
        "Close",
    ];
    if NONE.contains(&c.call)
        || c.call.starts_with("Set")
        || c.call.starts_with("IASet")
        || c.call.starts_with("OMSet")
        || c.call.starts_with("RSSet")
        || c.call == "SOSetTargets"
    {
        return Ok(Vec::new());
    }
    let a = &c.args;
    let named = |v: &Value| v["resource"].as_u64();
    let each = |v: &Value, f: &dyn Fn(&Value) -> Option<u64>| -> Vec<Option<u64>> {
        v.as_array().into_iter().flatten().map(f).collect()
    };
    Ok(match c.call {
        "CopyBufferRegion" | "CopyResource" => vec![a[0].as_u64()],
        "BuildRaytracingAccelerationStructure" => {
            let mut out = vec![named(&a[0]["dest"]), named(&a[0]["scratch"])];
            out.extend(each(&a[1], &|p| named(&p["dest"])));
            out
        }
        "CopyRaytracingAccelerationStructure" => vec![named(&a[0])],
        "EmitRaytracingAccelerationStructurePostbuildInfo" => each(&a[0], &|p| named(&p["dest"])),
        "WriteBufferImmediate" => each(&a[0], &|w| named(&w["address"])),
        _ => return Err(()),
    })
}

/// What the list whose commands are `commands` does after the build at `at` whose instances are gathered (see the
/// module's documentation): `range` gives the GPU addresses of a live buffer (its first and its bytes), None for a
/// texture. An error where no gather can read what the build read.
pub fn analyse(
    commands: &[model::Command],
    at: usize,
    range: &dyn Fn(u64) -> Option<(u64, u64)>,
) -> Result<Analysis, String> {
    if commands
        .get(at)
        .is_none_or(|c| c.call != "BuildRaytracingAccelerationStructure")
    {
        return Err("locum lost the build's place in its list".into());
    }
    let mut out = Analysis::default();
    let mut left: HashMap<u64, i32> = HashMap::new();
    let mut open: HashSet<u64> = HashSet::new();
    // The enhanced barrier after which the list may write anything it reaches.
    let mut ordered = false;
    let sync_waits = D3D12_BARRIER_SYNC_ALL.0 | D3D12_BARRIER_SYNC_BUILD_RAYTRACING_ACCELERATION_STRUCTURE.0;
    for c in &commands[at + 1..] {
        match c.call {
            "ResourceBarrier" => {
                for b in c.args[0].as_array().into_iter().flatten() {
                    let p = &b["payload"];
                    match b["type"].as_i64() {
                        Some(0) => {
                            let r = p["resource"].as_u64().unwrap_or(0);
                            let Some((first, bytes)) = range(r) else {
                                continue;
                            };
                            let flags = b["flags"].as_u64().unwrap_or(0);
                            if flags & BEGIN_ONLY != 0 {
                                open.insert(r);
                                continue;
                            }
                            if flags & END_ONLY != 0 {
                                open.remove(&r);
                            }
                            let after = p["after"].as_i64().unwrap_or(0) as i32;
                            left.insert(r, after);
                            if writable(after) {
                                out.writes.push((
                                    first,
                                    first + bytes,
                                    format!("its list moves buffer #{r} to state {after:#x} after the build"),
                                ));
                            }
                        }
                        Some(1) => {
                            return Err(format!(
                                "its list aliases memory after the build (#{} to #{}), so the instances may be other \
                                 resources' by its end",
                                p["before"].as_u64().unwrap_or(0),
                                p["after"].as_u64().unwrap_or(0)
                            ));
                        }
                        _ => {}
                    }
                }
            }
            "Barrier" => {
                let waits = c.args[0]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|g| g["barriers"].as_array().into_iter().flatten())
                    .any(|b| b["sync"][0].as_i64().unwrap_or(0) as i32 & sync_waits != 0);
                ordered |= waits;
            }
            _ if ordered && out.unknown.is_none() => match written(c) {
                Ok(resources) => {
                    for r in resources {
                        match r.and_then(|r| range(r).map(|g| (r, g))) {
                            Some((r, (first, bytes))) => out.writes.push((
                                first,
                                first + bytes,
                                format!("its list writes buffer #{r} by {} after the build", c.call),
                            )),
                            None => {
                                out.unknown = Some(format!(
                                    "its list's {} after the build writes what is in no buffer locum knows",
                                    c.call
                                ));
                            }
                        }
                    }
                }
                Err(()) => {
                    out.unknown = Some(format!(
                        "its list runs {} after an enhanced barrier waits for the build, and locum does not follow \
                         what that writes",
                        c.call
                    ));
                }
            },
            _ => {}
        }
    }
    if let Some(r) = open.iter().min() {
        return Err(format!(
            "a split barrier of buffer #{r} begun after the build is still open at the end of its list, so locum \
             cannot read the buffer after it"
        ));
    }
    let mut wraps: Vec<(u64, i32)> = left
        .into_iter()
        .filter(|&(_, s)| s & NPSR == 0 && s & STRUCTURE == 0)
        .collect();
    wraps.sort();
    out.wraps = wraps;
    Ok(out)
}

/// Whether a gather of `count` instances (its readback `all`) holds what the build read, given what its list does
/// after the build (`analysis`): otherwise why not.
pub fn verdict(all: &[u8], count: u32, analysis: &Analysis) -> Result<(), String> {
    let n = u64::from(count);
    let field = |at: u64, bytes: u64| &all[at as usize..(at + n * bytes) as usize];
    let pointers: Vec<u64> = field(pointers_at(n), POINTER)
        .as_chunks::<8>()
        .0
        .iter()
        .map(|p| u64::from_le_bytes(*p))
        .collect();
    if let Some(i) = pointers.iter().position(|&p| p == 0) {
        return Err(format!("instance pointer {i} of {n} is null"));
    }
    let stamps = field(stamps_at(n), STAMP).as_chunks::<4>().0;
    if let Some(i) = (0..n as usize).find(|&i| u32::from_le_bytes(stamps[i]) != i as u32 + 1) {
        return Err(format!(
            "locum's gather copied no instance for pointer {i} of {n}"
        ));
    }
    if let Some(why) = &analysis.unknown {
        return Err(format!(
            "{why}, so the instances may have changed after the build read them"
        ));
    }
    for (i, &p) in pointers.iter().enumerate() {
        if let Some((_, _, why)) = analysis
            .writes
            .iter()
            .find(|(first, end, _)| p < *end && p + INSTANCE > *first)
        {
            return Err(format!(
                "instance {i} of {n}: {why}, so it may have changed after the build read it"
            ));
        }
    }
    Ok(())
}

/// A transition barrier of `r` from `before` to `after`.
fn transition(r: &ID3D12Resource, before: i32, after: i32) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: borrowed(r),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: D3D12_RESOURCE_STATES(before),
                StateAfter: D3D12_RESOURCE_STATES(after),
            }),
        },
    }
}

/// The state a buffer the list left in `s` is read in: `s` with NON_PIXEL_SHADER_RESOURCE, where `s` is a read
/// state (or COMMON), else that alone.
fn wrapped(s: i32) -> i32 {
    if writable(s) && s != 0 { NPSR } else { s | NPSR }
}

/// The buffers a gather list moves to read them, those `analyses` (all of one application list) say it left without
/// NON_PIXEL_SHADER_RESOURCE: each once, with the state it was left in and the state it is read in. Its root SRVs read
/// them there; the list moves each back after the gathers.
fn wraps(analyses: &[&Analysis]) -> Vec<(u64, i32, i32)> {
    let mut wraps: Vec<(u64, i32)> = analyses.iter().flat_map(|a| a.wraps.iter().copied()).collect();
    wraps.sort();
    wraps.dedup_by_key(|(r, _)| *r);
    wraps.into_iter().map(|(r, s)| (r, s, wrapped(s))).collect()
}

/// Records into locum's list `list` the gathers of `gathers` (each with what its list does after the build and the
/// readback it is copied into), all of builds in one application list, submitted right before `list`.
///
/// # Safety
/// `list` is a real list of locum's, open for recording, on the device the gathers were made on; the application's
/// buffers the analyses name are live.
pub unsafe fn record(list: &ID3D12GraphicsCommandList, gathers: &[(&Gather, &Analysis, &ID3D12Resource)]) {
    let analyses: Vec<&Analysis> = gathers.iter().map(|(_, a, _)| *a).collect();
    // Held while the barriers borrow them.
    let held: Vec<(super::Real<ID3D12Resource>, i32, i32)> = wraps(&analyses)
        .into_iter()
        .filter_map(|(r, left, read)| super::real::<ID3D12Resource>(r).map(|real| (real, left, read)))
        .collect();
    let around = |back: bool| -> Vec<D3D12_RESOURCE_BARRIER> {
        held.iter()
            .map(|(r, left, read)| {
                if back {
                    transition(r, *read, *left)
                } else {
                    transition(r, *left, *read)
                }
            })
            .collect()
    };
    let uav_barrier = |r: &ID3D12Resource| D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: borrowed(r),
            }),
        },
    };
    const COMMON: i32 = 0;
    const UAV_STATE: i32 = D3D12_RESOURCE_STATE_UNORDERED_ACCESS.0;
    const ARGUMENTS: i32 = D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT.0;
    const SOURCE_STATE: i32 = D3D12_RESOURCE_STATE_COPY_SOURCE.0;
    const DEST: i32 = D3D12_RESOURCE_STATE_COPY_DEST.0;
    unsafe {
        let wrap = around(false);
        if !wrap.is_empty() {
            list.ResourceBarrier(&wrap);
        }
        for &(g, _, readback) in gathers {
            let n = u64::from(g.count);
            let out = g.out.GetGPUVirtualAddress();
            list.ResourceBarrier(&[
                transition(&g.pointers, DEST, UAV_STATE),
                transition(&g.records, COMMON, UAV_STATE),
                transition(&g.out, COMMON, UAV_STATE),
            ]);
            list.SetComputeRootSignature(&g.pipelines.root);
            list.SetPipelineState(&g.pipelines.records);
            list.SetComputeRoot32BitConstant(CONSTANT, g.count, 0);
            list.SetComputeRootUnorderedAccessView(UAV, g.pointers.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(UAV + 1, g.records.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(UAV + 2, out + pointers_at(n));
            list.SetComputeRootUnorderedAccessView(UAV + 3, out + stamps_at(n));
            list.Dispatch(g.count.div_ceil(GROUP), 1, 1);
            list.ResourceBarrier(&[transition(&g.records, UAV_STATE, ARGUMENTS), uav_barrier(&g.out)]);
            list.SetPipelineState(&g.pipelines.copy);
            list.SetComputeRootUnorderedAccessView(UAV, out);
            list.ExecuteIndirect(&g.pipelines.signature, g.count, &g.records, 0, None, 0);
            list.ResourceBarrier(&[transition(&g.out, UAV_STATE, SOURCE_STATE)]);
            list.CopyBufferRegion(readback, 0, &g.out, 0, bytes(n));
            list.ResourceBarrier(&[
                transition(&g.pointers, UAV_STATE, COMMON),
                transition(&g.records, ARGUMENTS, COMMON),
                transition(&g.out, SOURCE_STATE, COMMON),
            ]);
        }
        let back = around(true);
        if !back.is_empty() {
            list.ResourceBarrier(&back);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn command(call: &'static str, args: Value) -> model::Command {
        model::Command {
            call,
            args,
            bundle: None,
            diagnostic: None,
        }
    }

    fn transition_of(resource: u64, before: i32, after: i32, flags: u64) -> Value {
        json!({"type": 0, "flags": flags, "payload": {"resource": resource, "subresource": 0, "before": before,
            "after": after}})
    }

    /// Buffers 1 to 9 at 0x1000 apart, 0x100 bytes each; 50 is a texture.
    fn range(r: u64) -> Option<(u64, u64)> {
        (1..10).contains(&r).then(|| (r * 0x1000, 0x100))
    }

    fn build() -> model::Command {
        command("BuildRaytracingAccelerationStructure", json!([{}, []]))
    }

    #[test]
    fn transitions_after_the_build_are_writes_and_wraps() {
        let copy_source = D3D12_RESOURCE_STATE_COPY_SOURCE.0;
        let copy_dest = D3D12_RESOURCE_STATE_COPY_DEST.0;
        let commands = [
            command("ResourceBarrier", json!([[transition_of(4, NPSR, copy_dest, 0)]])),
            build(),
            command(
                "ResourceBarrier",
                json!([[
                    transition_of(1, NPSR, copy_source, 0),
                    transition_of(2, NPSR, copy_dest, 0),
                    transition_of(3, copy_dest, NPSR | copy_source, 0),
                    transition_of(50, NPSR, copy_dest, 0),
                ]]),
            ),
            command("ResourceBarrier", json!([[transition_of(5, NPSR, 0, 0)]])),
        ];
        let a = analyse(&commands, 1, &range).unwrap();
        // Buffer 4 was moved before the build; 50 is a texture; 3 is left readable as the gather reads.
        assert_eq!(a.wraps, [(1, copy_source), (2, copy_dest), (5, 0)]);
        let written: Vec<(u64, u64)> = a.writes.iter().map(|w| (w.0, w.1)).collect();
        assert_eq!(
            written,
            [(0x2000, 0x2100), (0x5000, 0x5100)],
            "COPY_DEST, and COMMON, which promotes"
        );
        assert_eq!(a.unknown, None);
        assert_eq!(wrapped(copy_source), copy_source | NPSR);
        assert_eq!(wrapped(copy_dest), NPSR);
        assert_eq!(wrapped(0), NPSR);
    }

    #[test]
    fn a_gather_list_moves_each_buffer_left_unreadable_once_to_read_it() {
        let copy_source = D3D12_RESOURCE_STATE_COPY_SOURCE.0;
        let copy_dest = D3D12_RESOURCE_STATE_COPY_DEST.0;
        let first = Analysis {
            wraps: vec![(2, copy_dest), (1, copy_source)],
            ..Analysis::default()
        };
        let second = Analysis {
            wraps: vec![(2, copy_dest), (5, 0)],
            ..Analysis::default()
        };
        assert_eq!(
            wraps(&[&first, &second]),
            [
                (1, copy_source, copy_source | NPSR),
                (2, copy_dest, NPSR),
                (5, 0, NPSR)
            ]
        );
        assert_eq!(wraps(&[&Analysis::default()]), []);
    }

    #[test]
    fn what_no_gather_can_read_is_an_error() {
        let alias = json!({"type": 1, "flags": 0, "payload": {"before": 1, "after": 2}});
        let aliased = [build(), command("ResourceBarrier", json!([[alias]]))];
        assert!(analyse(&aliased, 0, &range).unwrap_err().contains("aliases"));
        let split = [
            build(),
            command(
                "ResourceBarrier",
                json!([[transition_of(1, NPSR, 0x400, BEGIN_ONLY)]]),
            ),
        ];
        assert!(
            analyse(&split, 0, &range)
                .unwrap_err()
                .contains("split barrier of buffer #1")
        );
        let ended = [
            build(),
            command(
                "ResourceBarrier",
                json!([[transition_of(1, NPSR, 0x400, BEGIN_ONLY)]]),
            ),
            command(
                "ResourceBarrier",
                json!([[transition_of(1, NPSR, 0x400, END_ONLY)]]),
            ),
        ];
        assert_eq!(analyse(&ended, 0, &range).unwrap().writes.len(), 1);
        assert!(
            analyse(&[command("Dispatch", json!([1, 1, 1]))], 0, &range)
                .unwrap_err()
                .contains("lost")
        );
    }

    #[test]
    fn after_an_enhanced_barrier_that_waits_for_the_build_writes_are_named_or_unknown() {
        let barrier = |sync_before: i32| {
            command(
                "Barrier",
                json!([[{"type": 0, "barriers": [{"sync": [sync_before, 0], "access": [0, 0]}]}]]),
            )
        };
        let copy = command("CopyBufferRegion", json!([3, 0, 4, 0, 16]));
        let dispatch = command("Dispatch", json!([1, 1, 1]));
        // A barrier that does not wait for the build orders nothing after it.
        let compute = D3D12_BARRIER_SYNC_COMPUTE_SHADING.0;
        let free = [build(), barrier(compute), dispatch.clone()];
        assert_eq!(analyse(&free, 0, &range).unwrap(), Analysis::default());
        let build_sync = D3D12_BARRIER_SYNC_BUILD_RAYTRACING_ACCELERATION_STRUCTURE.0;
        let named = [
            build(),
            barrier(build_sync),
            command("SetPipelineState", json!([7])),
            copy.clone(),
        ];
        let a = analyse(&named, 0, &range).unwrap();
        assert_eq!(a.writes.len(), 1);
        assert_eq!((a.writes[0].0, a.writes[0].1), (0x3000, 0x3100));
        assert_eq!(a.unknown, None);
        let unknown = [build(), barrier(D3D12_BARRIER_SYNC_ALL.0), copy, dispatch];
        assert!(
            analyse(&unknown, 0, &range)
                .unwrap()
                .unknown
                .unwrap()
                .contains("Dispatch")
        );
        let texture = [
            build(),
            barrier(build_sync),
            command("CopyResource", json!([50, 4])),
        ];
        assert!(
            analyse(&texture, 0, &range)
                .unwrap()
                .unknown
                .unwrap()
                .contains("no buffer")
        );
    }

    /// A gather's readback of `pointers`, with `stamped` of them stamped.
    fn readback(pointers: &[u64], stamped: usize) -> Vec<u8> {
        let n = pointers.len() as u64;
        let mut all = vec![0u8; bytes(n) as usize];
        for (i, p) in pointers.iter().enumerate() {
            let at = (pointers_at(n) + i as u64 * POINTER) as usize;
            all[at..at + 8].copy_from_slice(&p.to_le_bytes());
            if i < stamped {
                let at = (stamps_at(n) + i as u64 * STAMP) as usize;
                all[at..at + 4].copy_from_slice(&(i as u32 + 1).to_le_bytes());
            }
        }
        all
    }

    #[test]
    fn a_gather_holds_what_the_build_read_unless_something_says_otherwise() {
        let none = Analysis::default();
        assert_eq!(verdict(&readback(&[0x2040, 0x1000], 2), 2, &none), Ok(()));
        assert!(
            verdict(&readback(&[0x2040, 0], 2), 2, &none)
                .unwrap_err()
                .contains("pointer 1 of 2 is null")
        );
        assert!(
            verdict(&readback(&[0x2040, 0x1000], 1), 2, &none)
                .unwrap_err()
                .contains("for pointer 1 of 2")
        );
        let written = Analysis {
            writes: vec![(0x2000, 0x2100, "moved".into())],
            ..Default::default()
        };
        assert!(
            verdict(&readback(&[0x1000, 0x20c0], 2), 2, &written)
                .unwrap_err()
                .starts_with("instance 1 of 2: moved")
        );
        // An instance that ends where a write begins is untouched.
        assert_eq!(verdict(&readback(&[0x1fc0], 1), 1, &written), Ok(()));
        let unknown = Analysis {
            unknown: Some("it dispatches".into()),
            ..Default::default()
        };
        assert!(
            verdict(&readback(&[0x1000], 1), 1, &unknown)
                .unwrap_err()
                .starts_with("it dispatches")
        );
    }
}
