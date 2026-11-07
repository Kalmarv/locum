//! Acceleration structures, armed or not: what each was last built from, so a capture can give replay the builds of
//! those made before its frame. An acceleration structure's bytes are the driver's own (a copy of them is not even
//! allowed), so replay builds it again from the same inputs, on its own device.
//!
//! A build's inputs (vertices, indices and transforms, or AABBs; a top-level build's instance descriptions) are
//! copied right after `BuildRaytracingAccelerationStructure`, in the application's list, into readback memory of
//! locum's: the application cannot change them before its next barrier, so the copy reads what the build read. Each
//! buffer is copied from the state or layout this list's barriers left it in (see [`super::passes::record_ranges`]).
//! The copies belong to the list until it is reset or destroyed; then those that are still some acceleration
//! structure's latest build are kept, the rest freed once the GPU is done with them.
//!
//! Lists execute in the order they are submitted, so that is the order builds take effect: an acceleration
//! structure's latest build is the last one submitted into it. `CopyRaytracingAccelerationStructure` makes the
//! destination a clone (or compaction) of its source's build.
use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use serde_json::Value;
use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::Interface;

use super::states::{Access, Read};

/// Readback memory every kept build's inputs may hold at once.
const BUDGET: u64 = 4 << 30;
static BYTES: AtomicU64 = AtomicU64::new(0);

/// An acceleration structure: the buffer it is in, and its offset there.
pub type Key = (u64, u64);

/// One range of a build's inputs, copied: its part (`vertices`, `indices`, `transform`, `aabbs`, `instances`), the
/// geometry it is of, where the application had it, and where the copy holds it.
#[derive(Clone, Debug)]
pub struct Input {
    pub part: &'static str,
    pub geometry: Option<usize>,
    pub resource: u64,
    pub offset: u64,
    pub bytes: u64,
    at: u64,
}

/// A build as the application recorded it, and its inputs as it read them.
pub struct Build {
    pub dest: Key,
    /// `values::acceleration_structure_build`'s encoding of the build's description.
    pub description: Value,
    pub inputs: Vec<Input>,
    /// Why its inputs were not copied.
    pub error: Option<String>,
    copy: Option<Copy>,
    /// The fence value locum signalled after the latest execution of the list it is in.
    last: Mutex<Option<(ID3D12Fence, u64)>>,
}

struct Copy {
    buffer: ID3D12Resource,
    bytes: u64,
}

impl Drop for Copy {
    fn drop(&mut self) {
        BYTES.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl Build {
    fn done(&self) -> bool {
        self.last
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_none_or(|(f, v)| unsafe { f.GetCompletedValue() } >= *v)
    }

    /// Its inputs' bytes, in the order of [`Build::inputs`], once the GPU has copied them (waiting up to `patience`).
    pub fn read(&self, patience: Duration) -> Result<Vec<Vec<u8>>, String> {
        if let Some(why) = &self.error {
            return Err(why.clone());
        }
        let copy = self.copy.as_ref().ok_or("its inputs were not copied")?;
        let last = self.last.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let Some((fence, value)) = last else {
            return Err("its list was never submitted".into());
        };
        unsafe { wait(&fence, value, patience) }?;
        let mut mapped = core::ptr::null_mut();
        unsafe { copy.buffer.Map(0, None, Some(&mut mapped)) }.map_err(|e| format!("Map: {e}"))?;
        let all = unsafe { core::slice::from_raw_parts(mapped.cast::<u8>(), copy.bytes as usize) };
        let parts = self
            .inputs
            .iter()
            .map(|i| all[i.at as usize..(i.at + i.bytes) as usize].to_vec())
            .collect();
        unsafe { copy.buffer.Unmap(0, None) };
        Ok(parts)
    }
}

unsafe fn wait(fence: &ID3D12Fence, value: u64, patience: Duration) -> Result<(), String> {
    if unsafe { fence.GetCompletedValue() } >= value {
        return Ok(());
    }
    let event = unsafe { CreateEventW(None, false, false, None) }.map_err(|e| format!("an event: {e}"))?;
    let waited = unsafe { fence.SetEventOnCompletion(value, event) }
        .map_err(|e| format!("SetEventOnCompletion: {e}"))
        .and_then(|()| {
            (unsafe { WaitForSingleObject(event, patience.as_millis() as u32) } == WAIT_OBJECT_0)
                .then_some(())
                .ok_or_else(|| format!("the GPU did not copy its inputs within {} s", patience.as_secs()))
        });
    unsafe { CloseHandle(event) }.ok();
    waited
}

/// What a list did to acceleration structures, in its order.
enum Step {
    Build(Arc<Build>),
    /// `CopyRaytracingAccelerationStructure(dest, source, mode)`.
    Copy(Key, Key, i32),
}

#[derive(Default)]
struct State {
    /// Each list's steps, by list.
    lists: HashMap<u64, Vec<Step>>,
    /// Each acceleration structure's latest build.
    latest: HashMap<Key, Arc<Build>>,
    /// Builds no list holds that the GPU may still copy into.
    leaving: Vec<Arc<Build>>,
}

fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The acceleration structure at GPU address `address`: its buffer and offset.
fn key(address: u64) -> Option<Key> {
    super::slots::unique_address(address).or_else(|| super::slots::buffers_at(address).first().copied())
}

/// The bytes of `count` elements `stride` apart whose last is `last` bytes.
fn extent(count: u64, stride: u64, last: u64) -> u64 {
    if count == 0 {
        0
    } else {
        (count - 1) * stride + last
    }
}

/// A range a build reads: its part, the geometry it is of, its address and its bytes.
type Range = (&'static str, Option<usize>, u64, u64);

/// The ranges a build reads.
unsafe fn ranges(i: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS) -> Result<Vec<Range>, String> {
    let n = i.NumDescs as usize;
    if i.Type == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL {
        if i.DescsLayout != D3D12_ELEMENTS_LAYOUT_ARRAY {
            return Err(
                "its instance descriptions are an array of pointers, which locum does not follow".into(),
            );
        }
        let bytes = n as u64 * size_of::<D3D12_RAYTRACING_INSTANCE_DESC>() as u64;
        return Ok(vec![(
            "instances",
            None,
            unsafe { i.Anonymous.InstanceDescs },
            bytes,
        )]);
    }
    let geometry: Vec<&D3D12_RAYTRACING_GEOMETRY_DESC> = if i.DescsLayout == D3D12_ELEMENTS_LAYOUT_ARRAY {
        unsafe { super::values::slice(i.Anonymous.pGeometryDescs, n) }
            .iter()
            .collect()
    } else {
        unsafe { super::values::slice(i.Anonymous.ppGeometryDescs, n) }
            .iter()
            .map(|g| unsafe { g.as_ref() }.ok_or("a null geometry description"))
            .collect::<Result<_, _>>()?
    };
    let mut out = Vec::new();
    for (k, g) in geometry.into_iter().enumerate() {
        if g.Type == D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES {
            let t = unsafe { &g.Anonymous.Triangles };
            let vertex = locum_capture::format::layout(t.VertexFormat.0 as u32)
                .map(|f| u64::from(f.block_bytes))
                .ok_or_else(|| format!("vertex format {} has no size locum knows", t.VertexFormat.0))?;
            out.push((
                "vertices",
                Some(k),
                t.VertexBuffer.StartAddress,
                extent(u64::from(t.VertexCount), t.VertexBuffer.StrideInBytes, vertex),
            ));
            if t.IndexBuffer != 0 && t.IndexCount > 0 {
                let index = if t.IndexFormat == windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16_UINT {
                    2
                } else {
                    4
                };
                out.push(("indices", Some(k), t.IndexBuffer, u64::from(t.IndexCount) * index));
            }
            if t.Transform3x4 != 0 {
                out.push(("transform", Some(k), t.Transform3x4, 48));
            }
        } else if g.Type == D3D12_RAYTRACING_GEOMETRY_TYPE_PROCEDURAL_PRIMITIVE_AABBS {
            let a = unsafe { &g.Anonymous.AABBs };
            out.push((
                "aabbs",
                Some(k),
                a.AABBs.StartAddress,
                extent(
                    a.AABBCount,
                    a.AABBs.StrideInBytes,
                    size_of::<D3D12_RAYTRACING_AABB>() as u64,
                ),
            ));
        } else {
            return Err(format!("geometry type {} is not represented", g.Type.0));
        }
    }
    Ok(out)
}

/// How a copy reads buffer `id` at this point of a list whose commands are `commands` (see [`Read::of`]), from
/// what the list's own transitions left it in.
fn access(commands: &[locum_capture::d3d12::Command], id: u64, real: &ID3D12Resource) -> Read {
    let mut known = None;
    let mut enhanced = false;
    for c in commands {
        match c.call {
            "ResourceBarrier" => {
                for b in c.args[0].as_array().into_iter().flatten() {
                    let p = &b["payload"];
                    // A transition, not the first half of a split one.
                    if b["type"] == 0 && b["flags"] != 1 && p["resource"].as_u64() == Some(id) {
                        known = p["after"].as_i64().map(|s| Access::State(s as i32));
                    }
                }
            }
            "Barrier" => enhanced = true,
            _ => {}
        }
    }
    Read::of(known, enhanced, real)
}

/// The predication the list's commands leave set: (buffer, offset, operation).
fn predication(commands: &[locum_capture::d3d12::Command]) -> Option<(u64, u64, i32)> {
    let last = commands.iter().rev().find(|c| c.call == "SetPredication")?;
    let a = &last.args;
    a[0].as_u64()
        .filter(|r| *r != 0)
        .map(|r| (r, a[1].as_u64().unwrap_or(0), a[2].as_i64().unwrap_or(0) as i32))
}

/// After `BuildRaytracingAccelerationStructure(p)` was recorded into the real list `real` (list `data`): its inputs
/// are copied after it (see the module's documentation).
///
/// # Safety
/// `real` is the application's real list, `p` the description it gave.
pub unsafe fn built(
    data: &super::journal::Data,
    real: *mut c_void,
    p: *const D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC,
) {
    if !super::journal::enabled() {
        return;
    }
    let Some(d) = (unsafe { p.as_ref() }) else {
        return;
    };
    let Some(dest) = key(d.DestAccelerationStructureData) else {
        return;
    };
    let description = unsafe { super::values::acceleration_structure_build(p) };
    let (inputs, copy, error) = match unsafe { copy_inputs(data, real, &d.Inputs) } {
        Ok((inputs, copy)) => (inputs, copy, None),
        Err(why) => (Vec::new(), None, Some(why)),
    };
    let build = Arc::new(Build {
        dest,
        description,
        inputs,
        error,
        copy,
        last: Mutex::new(None),
    });
    state().lists.entry(data.id).or_default().push(Step::Build(build));
}

unsafe fn copy_inputs(
    data: &super::journal::Data,
    real: *mut c_void,
    i: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
) -> Result<(Vec<Input>, Option<Copy>), String> {
    let ranges = unsafe { ranges(i) }?;
    let list = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&real) }.ok_or("no list")?;
    let commands = data
        .recording
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|r| r.snapshot())
        .ok_or("its list is not recorded")?;
    let commands = &commands.commands;
    let mut inputs = Vec::new();
    let mut copies = Vec::new();
    let mut at = 0;
    for (part, geometry, address, bytes) in ranges {
        let (resource, offset) =
            key(address).ok_or_else(|| format!("its {part} are in no live buffer locum knows"))?;
        let real = super::real::<ID3D12Resource>(resource).ok_or("an input buffer was destroyed")?;
        copies.push(super::passes::Source {
            resource,
            read: access(commands, resource, &real),
            real,
            offset,
            bytes,
        });
        inputs.push(Input {
            part,
            geometry,
            resource,
            offset,
            bytes,
            at,
        });
        at += bytes;
    }
    if at == 0 {
        return Ok((inputs, None));
    }
    if BYTES.fetch_add(at, Ordering::AcqRel) + at > BUDGET {
        BYTES.fetch_sub(at, Ordering::AcqRel);
        return Err(format!(
            "the {} GiB kept of builds' inputs are spent",
            BUDGET >> 30
        ));
    }
    let mut device: Option<ID3D12Device> = None;
    let made = unsafe { list.GetDevice(&mut device) }
        .map_err(|e| e.to_string())
        .and_then(|()| unsafe { super::passes::readback_resource(device.as_ref().ok_or("no device")?, at) });
    let buffer = match made {
        Ok(b) => b,
        Err(why) => {
            BYTES.fetch_sub(at, Ordering::AcqRel);
            return Err(why);
        }
    };
    let copy = Copy { buffer, bytes: at };
    let predicate = predication(commands)
        .and_then(|(r, offset, op)| super::real::<ID3D12Resource>(r).map(|r| (r, offset, op)));
    let promoted = unsafe {
        super::passes::record_ranges(
            list,
            &copies,
            D3D12_BARRIER_ACCESS_SHADER_RESOURCE,
            predicate,
            &copy.buffer,
        )
    }?;
    if let Some(r) = data.recording.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
        for resource in promoted {
            r.promote(resource);
        }
    }
    Ok((inputs, Some(copy)))
}

/// After `CopyRaytracingAccelerationStructure(dest, source, mode)` was recorded into list `list`.
pub fn copied(list: u64, dest: u64, source: u64, mode: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE) {
    if !super::journal::enabled() {
        return;
    }
    if let (Some(dest), Some(source)) = (key(dest), key(source)) {
        state()
            .lists
            .entry(list)
            .or_default()
            .push(Step::Copy(dest, source, mode.0));
    }
}

/// List `list` was reset or destroyed: its builds no longer run again.
pub fn retire(list: u64) {
    let mut s = state();
    if let Some(steps) = s.lists.remove(&list) {
        for step in steps {
            if let Step::Build(b) = step {
                s.leaving.push(b);
            }
        }
    }
    sweep(&mut s);
}

/// Frees the copies no acceleration structure's latest build and no list holds, once the GPU is done with them.
fn sweep(s: &mut State) {
    s.leaving.retain(|b| Arc::strong_count(b) > 1 || !b.done());
}

/// Resource `id` was destroyed: the acceleration structures in it go.
pub fn destroyed(id: u64) {
    let mut s = state();
    let gone: Vec<Key> = s.latest.keys().filter(|k| k.0 == id).copied().collect();
    for k in gone {
        if let Some(b) = s.latest.remove(&k) {
            s.leaving.push(b);
        }
    }
}

/// Lists `lists` are being executed, in this order: their builds and copies take effect. The builds they hold,
/// which [`submitted`] marks with locum's fence.
pub fn executing(lists: &[u64]) -> Vec<Arc<Build>> {
    let mut guard = state();
    let State {
        lists: steps,
        latest,
        leaving,
    } = &mut *guard;
    let mut builds = Vec::new();
    for step in lists.iter().flat_map(|l| steps.get(l)).flatten() {
        let (dest, now) = match step {
            Step::Build(b) => {
                builds.push(b.clone());
                (b.dest, Some(b.clone()))
            }
            // A clone or compaction is its source as built.
            Step::Copy(dest, source, mode)
                if *mode == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE.0
                    || *mode == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT.0 =>
            {
                (*dest, latest.get(source).cloned())
            }
            // Deserialized from bytes, or not an acceleration structure (serialized, visualized): no build of it.
            Step::Copy(dest, ..) => (*dest, None),
        };
        let replaced = match now {
            Some(b) => latest.insert(dest, b),
            None => latest.remove(&dest),
        };
        leaving.extend(replaced);
    }
    sweep(&mut guard);
    builds
}

/// After the lists holding `builds` were submitted to the real queue `queue` (number `queue_id`): locum's fence
/// marks when their copies are made.
///
/// # Safety
/// `queue` is the real queue, called right after the application's ExecuteCommandLists on it.
pub unsafe fn submitted(queue_id: u64, queue: *mut c_void, builds: Vec<Arc<Build>>) {
    if builds.is_empty() {
        return;
    }
    match unsafe { super::passes::mark(queue_id, queue) } {
        Ok(marked) => {
            for b in builds {
                *b.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(marked.clone());
            }
        }
        Err(why) => crate::log::log!("D3D12 acceleration structures: {why} on queue {queue_id}"),
    }
}

/// Every acceleration structure's latest build, as a frame begins: a clone's or compaction's is its source's.
pub fn latest() -> Vec<(Key, Arc<Build>)> {
    let s = state();
    let mut all: Vec<_> = s.latest.iter().map(|(k, b)| (*k, b.clone())).collect();
    all.sort_by_key(|(k, _)| *k);
    all
}

/// How long a capture waits for the GPU to have copied a build's inputs.
const PATIENCE: Duration = Duration::from_secs(10);

/// The acceleration structure at `key`, built (or cloned from one built) by `b`, as a capture records it: its
/// inputs' bytes into the blob store.
pub fn record(key: Key, b: &Build) -> locum_capture::d3d12::AccelerationStructure {
    let inputs = b.read(PATIENCE).map(|parts| {
        let blobs = &crate::settings::settings().blobs;
        Value::Array(
            b.inputs
                .iter()
                .zip(parts)
                .map(|(i, bytes)| {
                    let hash = locum_capture::hash::key(&bytes);
                    locum_capture::writer::blob(blobs, hash.clone(), bytes, true);
                    serde_json::json!({"part":i.part,"geometry":i.geometry,"resource":i.resource,"offset":i.offset,
                        "bytes":i.bytes,"blob":hash})
                })
                .collect(),
        )
    });
    locum_capture::d3d12::AccelerationStructure {
        resource: key.0,
        offset: key.1,
        build: b.description.clone(),
        inputs,
    }
}
