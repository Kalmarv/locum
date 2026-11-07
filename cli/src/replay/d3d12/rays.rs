//! Raytracing on replay's device. An acceleration structure's bytes are the driver's own, so replay builds each one
//! the frame uses again: from the inputs the capture copied at its latest build before the frame
//! (`d12_acceleration_structures`), or by running the frame's own builds and copies. Each of the game's acceleration
//! structures (a buffer and an offset in it) gets storage of replay's own, sized for the largest build the capture
//! makes into it on this device, so every address that names one (an acceleration structure view, a root SRV, a
//! top-level structure's instances, a build's source, a copy) is replaced with its storage's. One the frame began
//! with is built when replay first names it, before the event that does.
//!
//! A raytracing state object is made again from its description, the collections and the state object it grew from
//! (AddToStateObject) first. A DispatchRays' shader tables are read as replay's GPU holds them right before it, and
//! run from a copy in which each record's shader identifier is replay's for the same export (by the names the game
//! asked for identifiers by), and each local root argument the export's local root signature lays out is replay's
//! own: a descriptor handle the slot of replay's mirror of the heap, a root CBV, SRV or UAV the address of replay's
//! buffer (or acceleration structure).

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::mem::ManuallyDrop;
use std::sync::Arc;

use locum_capture::d3d12::programs::{self, Declared, IDENTIFIER, Kind, Local, Source};
use locum_capture::d3d12::tables::{self, Table, Which};
use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{HSTRING, Interface, PCWSTR};

use super::D3d12;
use crate::open::Capture;

/// One of the game's acceleration structures: the buffer it is in, and its offset there.
pub type Key = (i64, u64);

/// `D3D12_RAYTRACING_INSTANCE_DESC`: its bytes, and where it holds its bottom-level structure's address.
const INSTANCE: usize = 64;
const INSTANCE_STRUCTURE: usize = 56;
/// `D3D12_SERIALIZED_RAYTRACING_ACCELERATION_STRUCTURE_HEADER`'s bytes; the bottom-level addresses a serialized
/// top-level structure holds follow it.
const SERIALIZED_HEADER: usize = 56;
const PERFORM_UPDATE: i32 = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PERFORM_UPDATE.0;
/// Where a DispatchRays argument keeps each table's address (`D3D12_DISPATCH_RAYS_DESC`).
const TABLE_AT: [(Which, usize); 4] = [
    (Which::RayGeneration, 0),
    (Which::Miss, 16),
    (Which::HitGroups, 40),
    (Which::Callable, 64),
];

/// A structure's build before the frame, as recorded, and its inputs (or why they are not known).
type Before = (Value, Result<Value, String>);

/// Replay's storage for one of the game's acceleration structures.
struct Storage {
    buffer: ID3D12Resource,
    address: u64,
    bytes: u64,
}

/// A state object as replay made it, and the identifiers it gave each export asked for.
struct Made {
    object: ID3D12StateObject,
    identifiers: HashMap<String, Option<[u8; IDENTIFIER]>>,
}

#[derive(Default)]
pub(super) struct Rays {
    storage: HashMap<Key, Storage>,
    /// Those whose storage holds what the game's held at this point of the frame.
    current: HashSet<Key>,
    /// The builds the frame began with: each structure's build description, and its inputs (or why they are not
    /// known).
    before: Option<HashMap<Key, Before>>,
    /// The bytes each structure takes at most on this device, of every build and copy the capture makes into it.
    sizes: Option<HashMap<Key, u64>>,
    /// The resources that hold acceleration structures.
    structures: Option<BTreeSet<i64>>,
    scratch: Option<(ID3D12Resource, u64)>,
    state_objects: HashMap<i64, Option<Made>>,
    declared: RefCell<HashMap<String, Result<Arc<Declared>, String>>>,
    heaps: Option<Vec<(u64, Value)>>,
}

/// The capture's descriptions, as [`programs`] reads state objects.
struct Described<'a> {
    c: &'a Capture,
    declared: &'a RefCell<HashMap<String, Result<Arc<Declared>, String>>>,
}

impl Source for Described<'_> {
    fn description(&self, id: u64) -> Option<Value> {
        self.c
            .row(
                "SELECT description FROM d12_objects WHERE id = ?1",
                [id as i64],
                |r| r.get::<_, String>(0),
            )
            .and_then(|d| serde_json::from_str(&d).ok())
    }

    fn declared(&self, blob: &str) -> Result<Arc<Declared>, String> {
        if let Some(found) = self.declared.borrow().get(blob) {
            return found.clone();
        }
        let found = self
            .c
            .blob(blob)
            .and_then(|bytes| Declared::read(&bytes))
            .map(Arc::new);
        self.declared.borrow_mut().insert(blob.to_string(), found.clone());
        found
    }
}

/// The acceleration structure an address (`{resource, offset}`, or the candidates of one shared memory makes
/// ambiguous) names.
fn key(v: &Value) -> Option<Key> {
    let v = if v["resource"].is_null() {
        &v["candidates"][0]
    } else {
        v
    };
    Some((v["resource"].as_i64()?, v["offset"].as_u64().unwrap_or(0)))
}

fn uav_barrier() -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: ManuallyDrop::new(None),
            }),
        },
    }
}

/// An interface as a field of a structure the API takes, without the reference the field would own.
fn weak<T: Interface>(t: &T) -> ManuallyDrop<Option<T>> {
    unsafe { std::mem::transmute_copy(t) }
}

/// A build's inputs as the API takes them.
struct Inputs {
    inputs: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    geometry: Vec<D3D12_RAYTRACING_GEOMETRY_DESC>,
}

impl Inputs {
    /// From a build's recorded inputs (`values::acceleration_structure_inputs`), each address given by `address`
    /// (the part it is of, its geometry, and the address as recorded). A top-level build's instances are an array.
    fn of(v: &Value, address: &mut dyn FnMut(&str, Option<usize>, &Value) -> u64) -> Result<Inputs, String> {
        let n = |v: &Value, k: &str| v[k].as_u64().unwrap_or(0);
        let ty = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE(n(v, "type") as i32);
        let mut inputs = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
            Type: ty,
            Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS(n(v, "flags") as i32),
            NumDescs: n(v, "count") as u32,
            DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
            ..Default::default()
        };
        let mut geometry = Vec::new();
        let content = &v["content"];
        if ty == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL {
            inputs.Anonymous.InstanceDescs = address("instances", None, &content["instances"]);
        } else {
            for (k, g) in content["geometry"].as_array().into_iter().flatten().enumerate() {
                if g.is_null() {
                    return Err("a null geometry description".into());
                }
                let s = &g["shape"];
                let mut d = D3D12_RAYTRACING_GEOMETRY_DESC {
                    Type: D3D12_RAYTRACING_GEOMETRY_TYPE(n(g, "type") as i32),
                    Flags: D3D12_RAYTRACING_GEOMETRY_FLAGS(n(g, "flags") as i32),
                    ..Default::default()
                };
                if d.Type == D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES {
                    d.Anonymous.Triangles = D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                        Transform3x4: address("transform", Some(k), &s["transform"]),
                        IndexFormat: DXGI_FORMAT(n(s, "index_format") as i32),
                        VertexFormat: DXGI_FORMAT(n(s, "vertex_format") as i32),
                        IndexCount: n(s, "indices") as u32,
                        VertexCount: n(s, "vertices") as u32,
                        IndexBuffer: address("indices", Some(k), &s["index_buffer"]),
                        VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                            StartAddress: address("vertices", Some(k), &s["vertex_buffer"]["address"]),
                            StrideInBytes: n(&s["vertex_buffer"], "stride"),
                        },
                    };
                } else if d.Type == D3D12_RAYTRACING_GEOMETRY_TYPE_PROCEDURAL_PRIMITIVE_AABBS {
                    d.Anonymous.AABBs = D3D12_RAYTRACING_GEOMETRY_AABBS_DESC {
                        AABBCount: n(s, "aabbs"),
                        AABBs: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                            StartAddress: address("aabbs", Some(k), &s["buffer"]["address"]),
                            StrideInBytes: n(&s["buffer"], "stride"),
                        },
                    };
                } else {
                    return Err(format!(
                        "geometry {k} is of type {}, which the capture does not hold",
                        d.Type.0
                    ));
                }
                geometry.push(d);
            }
            if geometry.len() != inputs.NumDescs as usize {
                return Err(format!(
                    "{} geometry descriptions recorded of {}",
                    geometry.len(),
                    inputs.NumDescs
                ));
            }
        }
        Ok(Inputs { inputs, geometry })
    }

    fn get(&mut self) -> &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
        if self.inputs.Type == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL {
            self.inputs.Anonymous.pGeometryDescs = self.geometry.as_ptr();
        }
        &self.inputs
    }
}

/// What a state object's subobjects point at, kept alive while it is made.
#[derive(Default)]
struct Keep {
    values: Vec<Box<dyn std::any::Any>>,
    wide: Vec<Vec<u16>>,
    names: Vec<Vec<PCWSTR>>,
    exports: Vec<Vec<D3D12_EXPORT_DESC>>,
    blobs: Vec<Vec<u8>>,
    interfaces: Vec<windows::core::IUnknown>,
}

impl Keep {
    fn value<T: 'static>(&mut self, v: T) -> *const core::ffi::c_void {
        let b = Box::new(v);
        let p = (&*b as *const T).cast();
        self.values.push(b);
        p
    }

    fn wide(&mut self, v: &Value) -> PCWSTR {
        match v.as_str() {
            Some(s) => {
                let w: Vec<u16> = s.encode_utf16().chain([0]).collect();
                let p = PCWSTR(w.as_ptr());
                self.wide.push(w);
                p
            }
            None => PCWSTR::null(),
        }
    }

    fn names(&mut self, v: &Value) -> (u32, *const PCWSTR) {
        let names: Vec<PCWSTR> = v.as_array().into_iter().flatten().map(|n| self.wide(n)).collect();
        if names.is_empty() {
            return (0, core::ptr::null());
        }
        let (n, p) = (names.len() as u32, names.as_ptr());
        self.names.push(names);
        (n, p)
    }

    fn exports(&mut self, v: &Value) -> (u32, *const D3D12_EXPORT_DESC) {
        let exports: Vec<D3D12_EXPORT_DESC> = v
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| D3D12_EXPORT_DESC {
                Name: self.wide(&e["name"]),
                ExportToRename: self.wide(&e["rename"]),
                Flags: D3D12_EXPORT_FLAGS(e["flags"].as_i64().unwrap_or(0) as i32),
            })
            .collect();
        if exports.is_empty() {
            return (0, core::ptr::null());
        }
        let (n, p) = (exports.len() as u32, exports.as_ptr());
        self.exports.push(exports);
        (n, p)
    }
}

impl D3d12 {
    fn device5(&self) -> Result<ID3D12Device5, String> {
        self.device
            .cast::<ID3D12Device5>()
            .map_err(|e| format!("this device does no raytracing (ID3D12Device5): {e}"))
    }

    fn list4(&self) -> Result<ID3D12GraphicsCommandList4, String> {
        self.list
            .cast::<ID3D12GraphicsCommandList4>()
            .map_err(|e| format!("this device's command lists do no raytracing: {e}"))
    }

    /// Whether resource `id` holds acceleration structures.
    pub(super) fn holds_structures(&mut self, c: &Capture, id: i64) -> bool {
        self.rays
            .structures
            .get_or_insert_with(|| super::acceleration_structures(c))
            .contains(&id)
    }

    fn before(&mut self, c: &Capture) -> &HashMap<Key, Before> {
        self.rays.before.get_or_insert_with(|| {
            if !c.has_table("d12_acceleration_structures") {
                return HashMap::new();
            }
            c.rows(
                "SELECT resource, offset, build_json, inputs_json, diagnostic FROM d12_acceleration_structures",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .into_iter()
            .map(|(resource, offset, build, inputs, why)| {
                let build = serde_json::from_str(&build).unwrap_or(Value::Null);
                let inputs = match inputs {
                    Some(i) => serde_json::from_str(&i).map_err(|e| e.to_string()),
                    None => Err(why.unwrap_or_else(|| "not recorded".into())),
                };
                ((resource, offset as u64), (build, inputs))
            })
            .collect()
        })
    }

    /// What a build of `inputs` takes on this device: its result's bytes at most, and the scratch it (or an update
    /// of it) needs.
    fn prebuild(
        &self,
        inputs: &mut Inputs,
    ) -> Result<D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO, String> {
        let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
        unsafe {
            self.device5()?
                .GetRaytracingAccelerationStructurePrebuildInfo(inputs.get(), &mut info)
        };
        if info.ResultDataMaxSizeInBytes == 0 && inputs.inputs.NumDescs != 0 {
            return Err("the device sized none of it".into());
        }
        Ok(info)
    }

    /// The bytes each acceleration structure takes at most on this device: of its build before the frame, and of
    /// every build and copy the frame makes into it, in order.
    fn sizes(&mut self, c: &Capture) -> HashMap<Key, u64> {
        if let Some(s) = &self.rays.sizes {
            return s.clone();
        }
        // Addresses only have to be there for sizing: none is read.
        let mut placeholder = |_: &str, _: Option<usize>, v: &Value| if v.is_null() { 0 } else { 0x10000 };
        let mut sizes = HashMap::<Key, u64>::new();
        let before: Vec<(Key, Value)> = self
            .before(c)
            .iter()
            .map(|(k, (b, _))| (*k, b["inputs"].clone()))
            .collect();
        for (k, inputs) in before {
            if let Ok(info) = Inputs::of(&inputs, &mut placeholder).and_then(|mut i| self.prebuild(&mut i)) {
                let s = sizes.entry(k).or_default();
                *s = (*s).max(info.ResultDataMaxSizeInBytes);
            }
        }
        for (call, args) in c.rows(
            "SELECT call, args FROM events WHERE call IN ('BuildRaytracingAccelerationStructure', \
             'CopyRaytracingAccelerationStructure') ORDER BY seq",
            [],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        ) {
            let a: Value = args
                .and_then(|a| serde_json::from_str(&a).ok())
                .unwrap_or_default();
            if call == "BuildRaytracingAccelerationStructure" {
                let Some(dest) = key(&a[0]["dest"]) else { continue };
                if let Ok(info) =
                    Inputs::of(&a[0]["inputs"], &mut placeholder).and_then(|mut i| self.prebuild(&mut i))
                {
                    let s = sizes.entry(dest).or_default();
                    *s = (*s).max(info.ResultDataMaxSizeInBytes);
                }
            } else if let (Some(dest), Some(source)) = (key(&a[0]), key(&a[1])) {
                // A clone or compaction takes its source's bytes at most.
                let from = sizes.get(&source).copied().unwrap_or(0);
                let s = sizes.entry(dest).or_default();
                *s = (*s).max(from);
            }
        }
        self.rays.sizes = Some(sizes.clone());
        sizes
    }

    /// Replay's storage for acceleration structure `key`, made at the bytes it takes (at least `bytes`).
    fn storage(&mut self, c: &Capture, key: Key, bytes: u64) -> Result<u64, String> {
        let wanted = self.sizes(c).get(&key).copied().unwrap_or(0).max(bytes).max(256);
        if let Some(s) = self.rays.storage.get(&key) {
            if s.bytes >= wanted {
                return Ok(s.address);
            }
            self.warn(format!(
                "acceleration structure #{}+{}: a deserialized structure takes more than the capture's builds; \
                 what named it before names its old storage",
                key.0, key.1
            ));
        }
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: wanted.next_multiple_of(256),
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
            self.device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
                None,
                &mut r,
            )
        }
        .map_err(|e| format!("storage for acceleration structure #{}+{}: {e}", key.0, key.1))?;
        let buffer = r.ok_or("no storage")?;
        super::name(&buffer, format!("#{}+{} (acceleration structure)", key.0, key.1));
        let address = unsafe { buffer.GetGPUVirtualAddress() };
        if let Some(old) = self.rays.storage.insert(
            key,
            Storage {
                buffer,
                address,
                bytes: desc.Width,
            },
        ) {
            // Recorded commands may name it.
            self.kept.push(old.buffer);
        }
        Ok(address)
    }

    /// Scratch memory of at least `bytes` for a build.
    fn scratch(&mut self, bytes: u64) -> Result<u64, String> {
        if let Some((r, have)) = &self.rays.scratch
            && *have >= bytes
        {
            return Ok(unsafe { r.GetGPUVirtualAddress() });
        }
        let width = bytes.max(1 << 16).next_power_of_two();
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: width,
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
            self.device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                None,
                &mut r,
            )
        }
        .map_err(|e| format!("{width} bytes of build scratch: {e}"))?;
        let r = r.ok_or("no scratch")?;
        super::name(&r, "acceleration structure scratch".into());
        let address = unsafe { r.GetGPUVirtualAddress() };
        if let Some((old, _)) = self.rays.scratch.replace((r, width)) {
            self.kept.push(old);
        }
        Ok(address)
    }

    /// Records a build into the open list, then a barrier that orders what uses it after it.
    fn record_build(
        &mut self,
        dest: u64,
        source: u64,
        inputs: &mut Inputs,
        postbuild: &[D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC],
    ) -> Result<(), String> {
        let info = self.prebuild(inputs)?;
        let update = inputs.inputs.Flags.0 & PERFORM_UPDATE != 0;
        let scratch = self.scratch(if update {
            info.UpdateScratchDataSizeInBytes
        } else {
            info.ScratchDataSizeInBytes
        })?;
        let desc = D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
            DestAccelerationStructureData: dest,
            Inputs: *inputs.get(),
            SourceAccelerationStructureData: source,
            ScratchAccelerationStructureData: scratch,
        };
        let list = self.list4()?;
        unsafe {
            list.BuildRaytracingAccelerationStructure(&desc, (!postbuild.is_empty()).then_some(postbuild));
            list.ResourceBarrier(&[uav_barrier()]);
        }
        Ok(())
    }

    /// The game's GPU address of a bottom-level structure (in an instance, or after a serialized top-level
    /// structure's header) as replay's: its storage, built as it was then. 0 stays 0.
    fn bottom_level(&mut self, c: &Capture, game: u64, why: &str) -> Result<u64, String> {
        if game == 0 {
            return Ok(0);
        }
        let found = self.addresses_of(c).find(game);
        let known = self.before(c).keys().copied().collect::<HashSet<_>>();
        let key = found
            .iter()
            .copied()
            .find(|k| known.contains(k) || self.rays.storage.contains_key(k))
            .or_else(|| {
                found
                    .iter()
                    .copied()
                    .find(|(id, _)| self.holds_structures(c, *id))
            });
        match key {
            Some(k) => self.structure(c, k, false),
            None => {
                self.warn(format!(
                    "{why} names address {game:#x}, which is in no acceleration structure the capture knows: it \
                     names none in replay"
                ));
                Ok(0)
            }
        }
    }

    /// Instances (`bytes`, a top-level build's array of them) with replay's bottom-level structures.
    fn instances(&mut self, c: &Capture, bytes: &mut [u8]) -> Result<(), String> {
        for instance in bytes.as_chunks_mut::<INSTANCE>().0 {
            let field = &mut instance[INSTANCE_STRUCTURE..INSTANCE];
            let game = u64::from_le_bytes((&*field).try_into().expect("8 bytes"));
            let mine = self.bottom_level(c, game, "an instance")?;
            field.copy_from_slice(&mine.to_le_bytes());
        }
        Ok(())
    }

    /// Acceleration structure `key` as replay holds it: its storage's address, holding what the game's held at
    /// this point of the frame (built now from its build before the frame, where the frame has not built it yet;
    /// `top`: what it is taken as where the capture holds no build of it, which replay builds empty).
    pub(super) fn structure(&mut self, c: &Capture, key: Key, top: bool) -> Result<u64, String> {
        let address = self.storage(c, key, 0)?;
        if !self.rays.current.insert(key) {
            return Ok(address);
        }
        let name = format!("acceleration structure #{}+{}", key.0, key.1);
        let found = self.before(c).get(&key).cloned();
        let (build, parts) = match found {
            Some((build, Ok(parts))) => (build, parts),
            Some((build, Err(why))) => {
                self.warn(format!(
                    "{name}: its build before the frame is not known ({why}): replay's is empty"
                ));
                let top = build["inputs"]["type"].as_i64().map_or(top, |t| t == 0);
                return self.empty(address, top).map(|()| address);
            }
            None => {
                self.warn(format!(
                    "{name}: the capture holds no build of it: replay's is empty"
                ));
                return self.empty(address, top).map(|()| address);
            }
        };
        // Each part of its inputs in an upload buffer of replay's, as the build read it.
        let parts: Vec<(String, Option<usize>, Vec<u8>)> = parts
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| {
                let bytes = c.blob(p["blob"].as_str().unwrap_or(""))?;
                Ok((
                    p["part"].as_str().unwrap_or("").to_string(),
                    p["geometry"].as_u64().map(|g| g as usize),
                    bytes,
                ))
            })
            .collect::<Result<_, String>>()?;
        let mut placed = Vec::new();
        let mut at = 0u64;
        for (_, _, bytes) in &parts {
            placed.push(at);
            at = (at + bytes.len() as u64).next_multiple_of(256);
        }
        let (upload, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, at.max(256))?;
        let base = unsafe { upload.GetGPUVirtualAddress() };
        for ((part, _, bytes), &offset) in parts.iter().zip(&placed) {
            let mut bytes = bytes.clone();
            if part == "instances" {
                self.instances(c, &mut bytes)?;
            }
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), p.add(offset as usize), bytes.len()) };
        }
        let mut address_of = |part: &str, geometry: Option<usize>, v: &Value| {
            if v.is_null() {
                return 0;
            }
            parts
                .iter()
                .zip(&placed)
                .find(|((p, g, _), _)| p == part && *g == geometry)
                .map_or(0, |(_, &offset)| base + offset)
        };
        let mut inputs = match Inputs::of(&build["inputs"], &mut address_of) {
            Ok(i) => i,
            Err(why) => {
                self.warn(format!(
                    "{name}: its build before the frame cannot be made again ({why}): replay's is empty"
                ));
                return self.empty(address, top).map(|()| address);
            }
        };
        // Built whole: an update's result is a build of the same inputs.
        inputs.inputs.Flags.0 &= !PERFORM_UPDATE;
        self.record_build(address, 0, &mut inputs, &[])?;
        Ok(address)
    }

    fn empty(&mut self, address: u64, top: bool) -> Result<(), String> {
        let mut inputs = Inputs {
            inputs: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
                Type: if top {
                    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL
                } else {
                    D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL
                },
                DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
                ..Default::default()
            },
            geometry: Vec::new(),
        };
        self.record_build(address, 0, &mut inputs, &[])
    }

    /// The address a view or root argument names, as replay's: an acceleration structure's storage where it names
    /// one (`top`: as it is taken where the capture holds no build of it), else replay's buffer.
    pub(super) fn view_address(
        &mut self,
        c: &Capture,
        resource: i64,
        offset: u64,
        top: bool,
    ) -> Result<u64, String> {
        let key = (resource, offset);
        let known = self.rays.storage.contains_key(&key) || self.before(c).contains_key(&key);
        if known || self.holds_structures(c, resource) {
            return self.structure(c, key, top);
        }
        Ok(self.address(resource, offset))
    }

    /// An acceleration structure view's descriptor at `handle`.
    pub(super) fn structure_view(&mut self, c: &Capture, desc: &Value, handle: D3D12_CPU_DESCRIPTOR_HANDLE) {
        let Some(k) = key(&desc["payload"]["address"]) else {
            // A null view.
            return self.null_structure_view(handle);
        };
        match self.structure(c, k, true) {
            Ok(location) => {
                let d = D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_UNKNOWN,
                    ViewDimension: D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        RaytracingAccelerationStructure: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_SRV {
                            Location: location,
                        },
                    },
                };
                unsafe { self.device.CreateShaderResourceView(None, Some(&d), handle) };
            }
            Err(why) => {
                self.warn(format!("an acceleration structure view: {why}"));
                self.null_structure_view(handle);
            }
        }
    }

    fn null_structure_view(&mut self, handle: D3D12_CPU_DESCRIPTOR_HANDLE) {
        let d = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            ..Default::default()
        };
        unsafe { self.device.CreateShaderResourceView(None, Some(&d), handle) };
    }

    /// A game's buffer address in an event's arguments as replay's.
    fn buffer_address(&self, v: &Value) -> u64 {
        let v = if v["resource"].is_null() {
            &v["candidates"][0]
        } else {
            v
        };
        match v["resource"].as_i64() {
            Some(id) => self.address(id, v["offset"].as_u64().unwrap_or(0)),
            None => 0,
        }
    }

    /// `BuildRaytracingAccelerationStructure(a[0], a[1])`, as the frame ran it, into replay's storage.
    pub(super) fn build(&mut self, c: &Capture, a: &Value) -> Result<(), String> {
        let b = &a[0];
        let dest = key(&b["dest"]).ok_or("its destination is in no buffer the capture knows")?;
        let recorded = &b["inputs"];
        let top = recorded["type"].as_i64() == Some(0);
        let count = recorded["count"].as_u64().unwrap_or(0) as usize;
        // A top-level build's instances as replay's GPU holds them now, read before anything is recorded.
        let mut instances = None;
        if top && count > 0 {
            let at = &recorded["content"]["instances"];
            let (id, offset) = key(at).ok_or("its instances are in no buffer the capture knows")?;
            instances = Some(
                if recorded["layout"].as_i64() == Some(D3D12_ELEMENTS_LAYOUT_ARRAY_OF_POINTERS.0 as i64) {
                    self.pointed_instances(c, id, offset, count)?
                } else {
                    self.read_range(id, offset, (count * INSTANCE) as u64)?
                },
            );
        }
        let update = recorded["flags"].as_i64().unwrap_or(0) as i32 & PERFORM_UPDATE != 0;
        let source = match key(&b["source"]) {
            Some(s) if update => self.structure(c, s, top)?,
            _ => 0,
        };
        let address = self.storage(c, dest, 0)?;
        let mut staged = 0;
        if let Some(mut bytes) = instances {
            self.instances(c, &mut bytes)?;
            let (upload, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, bytes.len() as u64)?;
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len()) };
            staged = unsafe { upload.GetGPUVirtualAddress() };
        }
        let mut address_of = |part: &str, _: Option<usize>, v: &Value| {
            if part == "instances" {
                staged
            } else {
                self.buffer_address(v)
            }
        };
        let mut inputs = Inputs::of(recorded, &mut address_of)?;
        let postbuild: Vec<_> = a[1]
            .as_array()
            .into_iter()
            .flatten()
            .map(|p| D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC {
                DestBuffer: self.buffer_address(&p["dest"]),
                InfoType: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_TYPE(
                    p["info"].as_i64().unwrap_or(0) as i32,
                ),
            })
            .collect();
        if !postbuild.is_empty() {
            self.warn("acceleration structures' postbuild information is replay's device's own");
        }
        self.record_build(address, source, &mut inputs, &postbuild)?;
        self.rays.current.insert(dest);
        Ok(())
    }

    /// A top-level build's instances given as an array of `count` addresses at `offset` in buffer `id`: each
    /// instance read from where it points, as an array.
    fn pointed_instances(
        &mut self,
        c: &Capture,
        id: i64,
        offset: u64,
        count: usize,
    ) -> Result<Vec<u8>, String> {
        let pointers = self.read_range(id, offset, (count * 8) as u64)?;
        let addresses = self.addresses_of(c);
        let at: Vec<Option<(i64, u64)>> = pointers
            .as_chunks::<8>()
            .0
            .iter()
            .map(|p| {
                let game = u64::from_le_bytes(*p);
                addresses
                    .find(game)
                    .into_iter()
                    .find(|(id, _)| self.resources.contains_key(id))
            })
            .collect();
        // Each buffer read once, over the instances it holds.
        let mut spans = BTreeMap::<i64, (u64, u64)>::new();
        for &(id, o) in at.iter().flatten() {
            let s = spans.entry(id).or_insert((o, o + INSTANCE as u64));
            *s = (s.0.min(o), s.1.max(o + INSTANCE as u64));
        }
        let mut read = HashMap::new();
        for (id, (start, end)) in spans {
            read.insert(id, (start, self.read_range(id, start, end - start)?));
        }
        let mut out = vec![0u8; count * INSTANCE];
        for (i, place) in at.iter().enumerate() {
            match place {
                Some((id, o)) => {
                    let (start, bytes) = &read[id];
                    let from = (o - start) as usize;
                    out[i * INSTANCE..(i + 1) * INSTANCE].copy_from_slice(&bytes[from..from + INSTANCE]);
                }
                None => self.warn(
                    "a top-level build's instance pointer is in no buffer replay made: the instance is left out",
                ),
            }
        }
        Ok(out)
    }

    /// `CopyRaytracingAccelerationStructure(a[0], a[1], a[2])`, as the frame ran it.
    pub(super) fn copy_structure(&mut self, c: &Capture, a: &Value) -> Result<(), String> {
        let mode = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE(a[2].as_i64().unwrap_or(0) as i32);
        let list = self.list4()?;
        match mode {
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE
            | D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT => {
                let (dest, source) = (
                    key(&a[0]).ok_or("its destination is in no buffer the capture knows")?,
                    key(&a[1]).ok_or("its source is in no buffer the capture knows")?,
                );
                let from = self.structure(c, source, false)?;
                let to = self.storage(c, dest, 0)?;
                unsafe {
                    list.CopyRaytracingAccelerationStructure(to, from, mode);
                    list.ResourceBarrier(&[uav_barrier()]);
                }
                self.rays.current.insert(dest);
            }
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_SERIALIZE
            | D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_VISUALIZATION_DECODE_FOR_TOOLS => {
                let source = key(&a[1]).ok_or("its source is in no buffer the capture knows")?;
                let from = self.structure(c, source, false)?;
                let to = self.buffer_address(&a[0]);
                self.warn(
                    "serialized and visualized acceleration structures are replay's device's own bytes",
                );
                unsafe {
                    list.CopyRaytracingAccelerationStructure(to, from, mode);
                    list.ResourceBarrier(&[uav_barrier()]);
                }
            }
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_DESERIALIZE => self.deserialize(c, a)?,
            other => self.warn(format!(
                "acceleration structure copies of mode {} are not replayed",
                other.0
            )),
        }
        Ok(())
    }

    /// A structure deserialized from the game's bytes: where this device's driver reads what the game's wrote,
    /// with the bottom-level addresses a top-level one holds as replay's.
    fn deserialize(&mut self, c: &Capture, a: &Value) -> Result<(), String> {
        let dest = key(&a[0]).ok_or("its destination is in no buffer the capture knows")?;
        let (id, offset) = key(&a[1]).ok_or("its source is in no buffer the capture knows")?;
        let header = self.read_range(id, offset, SERIALIZED_HEADER as u64)?;
        let u = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().expect("8 bytes"));
        let identifier = D3D12_SERIALIZED_DATA_DRIVER_MATCHING_IDENTIFIER {
            DriverOpaqueGUID: unsafe { header.as_ptr().cast::<windows::core::GUID>().read_unaligned() },
            DriverOpaqueVersioningData: header[16..32].try_into().expect("16 bytes"),
        };
        let status = unsafe {
            self.device5()?.CheckDriverMatchingIdentifier(
                D3D12_SERIALIZED_DATA_RAYTRACING_ACCELERATION_STRUCTURE,
                &identifier,
            )
        };
        if status != D3D12_DRIVER_MATCHING_IDENTIFIER_COMPATIBLE_WITH_DEVICE {
            self.warn(format!(
                "an acceleration structure serialized by the game's driver cannot be read by this device's ({}): \
                 replay's is empty",
                status.0
            ));
            let to = self.storage(c, dest, 0)?;
            self.rays.current.insert(dest);
            return self.empty(to, true);
        }
        let (whole, deserialized, bottom) = (u(32), u(40), u(48) as usize);
        let mut bytes = self.read_range(id, offset, whole)?;
        for i in 0..bottom {
            let at = SERIALIZED_HEADER + 8 * i;
            let game = u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
            let mine = self.bottom_level(c, game, "a serialized top-level structure")?;
            bytes[at..at + 8].copy_from_slice(&mine.to_le_bytes());
        }
        let (upload, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, whole)?;
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len()) };
        let to = self.storage(c, dest, deserialized)?;
        let list = self.list4()?;
        unsafe {
            list.CopyRaytracingAccelerationStructure(
                to,
                upload.GetGPUVirtualAddress(),
                D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_DESERIALIZE,
            );
            list.ResourceBarrier(&[uav_barrier()]);
        }
        self.rays.current.insert(dest);
        Ok(())
    }

    /// `EmitRaytracingAccelerationStructurePostbuildInfo(a[0], a[1])`: replay's device's own sizes.
    pub(super) fn postbuild(&mut self, c: &Capture, a: &Value) -> Result<(), String> {
        let mut sources = Vec::new();
        for s in a[1].as_array().into_iter().flatten() {
            let k = key(s).ok_or("a source is in no buffer the capture knows")?;
            sources.push(self.structure(c, k, false)?);
        }
        let Some(p) = a[0].as_array().and_then(|d| d.first()) else {
            return Ok(());
        };
        let desc = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC {
            DestBuffer: self.buffer_address(&p["dest"]),
            InfoType: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_TYPE(
                p["info"].as_i64().unwrap_or(0) as i32,
            ),
        };
        self.warn("acceleration structures' postbuild information is replay's device's own");
        let list = self.list4()?;
        unsafe { list.EmitRaytracingAccelerationStructurePostbuildInfo(&desc, &sources) };
        Ok(())
    }

    /// State object `id` as replay made it (with the collections and the state object it grew from first).
    pub(super) fn state_object(&mut self, c: &Capture, id: i64) -> Option<ID3D12StateObject> {
        if !self.rays.state_objects.contains_key(&id) {
            let made = match self.make_state_object(c, id, 0) {
                Ok(object) => {
                    super::name(&object, format!("state object #{id}"));
                    Some(Made {
                        object,
                        identifiers: HashMap::new(),
                    })
                }
                Err(why) => {
                    self.warn(format!(
                        "state object #{id} could not be made: {why}{}",
                        self.validation()
                    ));
                    None
                }
            };
            self.rays.state_objects.insert(id, made);
        }
        self.rays.state_objects[&id].as_ref().map(|m| m.object.clone())
    }

    fn make_state_object(&mut self, c: &Capture, id: i64, depth: u32) -> Result<ID3D12StateObject, String> {
        if depth > 64 {
            return Err("its collections or parents run more than 64 deep".into());
        }
        let d = Described {
            c,
            declared: &self.rays.declared,
        }
        .description(id as u64)
        .filter(|d| d["kind"] == "state_object")
        .ok_or("it is not described in the capture")?;
        let recorded = d["subobjects"].as_array().cloned().unwrap_or_default();
        let mut keep = Keep::default();
        let mut subobjects = vec![D3D12_STATE_SUBOBJECT::default(); recorded.len()];
        let base = subobjects.as_mut_ptr();
        for (i, s) in recorded.iter().enumerate() {
            let ty = D3D12_STATE_SUBOBJECT_TYPE(s["type"].as_i64().unwrap_or(-1) as i32);
            let v = &s["value"];
            let desc = match ty {
                D3D12_STATE_SUBOBJECT_TYPE_STATE_OBJECT_CONFIG => keep.value(D3D12_STATE_OBJECT_CONFIG {
                    Flags: D3D12_STATE_OBJECT_FLAGS(v.as_i64().unwrap_or(0) as i32),
                }),
                D3D12_STATE_SUBOBJECT_TYPE_GLOBAL_ROOT_SIGNATURE
                | D3D12_STATE_SUBOBJECT_TYPE_LOCAL_ROOT_SIGNATURE => {
                    let root = v.as_i64().unwrap_or(0);
                    let signature = self
                        .root(c, root)
                        .map(|r| r.signature.clone())
                        .ok_or_else(|| format!("root signature #{root} could not be made"))?;
                    let p = if ty == D3D12_STATE_SUBOBJECT_TYPE_GLOBAL_ROOT_SIGNATURE {
                        keep.value(D3D12_GLOBAL_ROOT_SIGNATURE {
                            pGlobalRootSignature: weak(&signature),
                        })
                    } else {
                        keep.value(D3D12_LOCAL_ROOT_SIGNATURE {
                            pLocalRootSignature: weak(&signature),
                        })
                    };
                    keep.interfaces.push(signature.into());
                    p
                }
                D3D12_STATE_SUBOBJECT_TYPE_NODE_MASK => keep.value(D3D12_NODE_MASK {
                    NodeMask: v.as_u64().unwrap_or(0) as u32,
                }),
                D3D12_STATE_SUBOBJECT_TYPE_DXIL_LIBRARY => {
                    let code = c.blob(v["library"]["blob"].as_str().unwrap_or(""))?;
                    let (n, exports) = keep.exports(&v["exports"]);
                    let library = D3D12_DXIL_LIBRARY_DESC {
                        DXILLibrary: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: code.as_ptr().cast(),
                            BytecodeLength: code.len(),
                        },
                        NumExports: n,
                        pExports: exports,
                    };
                    keep.blobs.push(code);
                    keep.value(library)
                }
                D3D12_STATE_SUBOBJECT_TYPE_EXISTING_COLLECTION => {
                    let collection = v["collection"].as_i64().unwrap_or(0);
                    let object = self
                        .make_state_object(c, collection, depth + 1)
                        .map_err(|e| format!("collection #{collection}: {e}"))?;
                    let (n, exports) = keep.exports(&v["exports"]);
                    let p = keep.value(D3D12_EXISTING_COLLECTION_DESC {
                        pExistingCollection: weak(&object),
                        NumExports: n,
                        pExports: exports,
                    });
                    keep.interfaces.push(object.into());
                    p
                }
                D3D12_STATE_SUBOBJECT_TYPE_SUBOBJECT_TO_EXPORTS_ASSOCIATION => {
                    let index = v["subobject"]
                        .as_u64()
                        .filter(|&i| (i as usize) < recorded.len())
                        .ok_or("an association names a subobject outside the state object")?;
                    let (n, exports) = keep.names(&v["exports"]);
                    keep.value(D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION {
                        pSubobjectToAssociate: unsafe { base.add(index as usize) },
                        NumExports: n,
                        pExports: exports,
                    })
                }
                D3D12_STATE_SUBOBJECT_TYPE_DXIL_SUBOBJECT_TO_EXPORTS_ASSOCIATION => {
                    let (n, exports) = keep.names(&v["exports"]);
                    let name = keep.wide(&v["subobject"]);
                    keep.value(D3D12_DXIL_SUBOBJECT_TO_EXPORTS_ASSOCIATION {
                        SubobjectToAssociate: name,
                        NumExports: n,
                        pExports: exports,
                    })
                }
                D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_SHADER_CONFIG => {
                    keep.value(D3D12_RAYTRACING_SHADER_CONFIG {
                        MaxPayloadSizeInBytes: v["payload"].as_u64().unwrap_or(0) as u32,
                        MaxAttributeSizeInBytes: v["attributes"].as_u64().unwrap_or(0) as u32,
                    })
                }
                D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_PIPELINE_CONFIG => {
                    keep.value(D3D12_RAYTRACING_PIPELINE_CONFIG {
                        MaxTraceRecursionDepth: v["depth"].as_u64().unwrap_or(0) as u32,
                    })
                }
                D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_PIPELINE_CONFIG1 => {
                    keep.value(D3D12_RAYTRACING_PIPELINE_CONFIG1 {
                        MaxTraceRecursionDepth: v["depth"].as_u64().unwrap_or(0) as u32,
                        Flags: D3D12_RAYTRACING_PIPELINE_FLAGS(v["flags"].as_i64().unwrap_or(0) as i32),
                    })
                }
                D3D12_STATE_SUBOBJECT_TYPE_HIT_GROUP => {
                    let group = D3D12_HIT_GROUP_DESC {
                        HitGroupExport: keep.wide(&v["name"]),
                        Type: D3D12_HIT_GROUP_TYPE(v["type"].as_i64().unwrap_or(0) as i32),
                        AnyHitShaderImport: keep.wide(&v["any_hit"]),
                        ClosestHitShaderImport: keep.wide(&v["closest_hit"]),
                        IntersectionShaderImport: keep.wide(&v["intersection"]),
                    };
                    keep.value(group)
                }
                other => {
                    return Err(format!(
                        "subobject {i} is of type {}, which replay does not make (work graphs and generic programs \
                         are not raytracing)",
                        other.0
                    ));
                }
            };
            subobjects[i] = D3D12_STATE_SUBOBJECT {
                Type: ty,
                pDesc: desc,
            };
        }
        let desc = D3D12_STATE_OBJECT_DESC {
            Type: D3D12_STATE_OBJECT_TYPE(d["type"].as_i64().unwrap_or(0) as i32),
            NumSubobjects: subobjects.len() as u32,
            pSubobjects: subobjects.as_ptr(),
        };
        let parent = d["parent"].as_i64().unwrap_or(0);
        let made = if parent == 0 {
            unsafe { self.device5()?.CreateStateObject::<ID3D12StateObject>(&desc) }
        } else {
            let grown_from = self
                .make_state_object(c, parent, depth + 1)
                .map_err(|e| format!("the state object it grew from, #{parent}: {e}"))?;
            let device7 = self
                .device
                .cast::<ID3D12Device7>()
                .map_err(|e| format!("this device cannot grow state objects (ID3D12Device7): {e}"))?;
            unsafe { device7.AddToStateObject::<_, ID3D12StateObject>(&desc, &grown_from) }
        };
        drop(keep);
        made.map_err(|e| e.to_string())
    }

    /// Replay's identifier for `export` of state object `id`.
    fn identifier(&mut self, id: i64, export: &str) -> Option<[u8; IDENTIFIER]> {
        let made = self.rays.state_objects.get_mut(&id)?.as_mut()?;
        if let Some(found) = made.identifiers.get(export) {
            return *found;
        }
        let found = made
            .object
            .cast::<ID3D12StateObjectProperties>()
            .ok()
            .and_then(|p| {
                let raw = unsafe { p.GetShaderIdentifier(&HSTRING::from(export)) };
                (!raw.is_null()).then(|| {
                    let mut out = [0u8; IDENTIFIER];
                    unsafe { core::ptr::copy_nonoverlapping(raw.cast::<u8>(), out.as_mut_ptr(), IDENTIFIER) };
                    out
                })
            });
        made.identifiers.insert(export.to_string(), found);
        found
    }

    fn heap_descriptions(&mut self, c: &Capture) -> Vec<(u64, Value)> {
        self.rays
            .heaps
            .get_or_insert_with(|| {
                c.rows(
                    "SELECT id, description FROM d12_objects WHERE kind = 'ID3D12DescriptorHeap'",
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .into_iter()
                .filter_map(|(id, d)| Some((id as u64, serde_json::from_str(&d).ok()?)))
                .collect()
            })
            .clone()
    }

    /// A game's GPU address in a shader record as replay's: of its buffer, or an acceleration structure's storage
    /// (`srv`: a root SRV, which may name one).
    fn record_address(&mut self, c: &Capture, game: u64, srv: bool) -> Result<u64, String> {
        let found = self.addresses_of(c).find(game);
        if srv && let Some(&(id, at)) = found.iter().find(|(id, _)| self.holds_structures(c, *id)) {
            return self.structure(c, (id, at), true);
        }
        match found.iter().find(|(id, _)| self.resources.contains_key(id)) {
            Some(&(id, at)) => Ok(self.address(id, at)),
            None => Err(match found.first() {
                None => format!("address {game:#x} is in no buffer the capture describes"),
                Some((id, _)) => format!("address {game:#x} is in #{id}, which replay did not make"),
            }),
        }
    }

    /// The tables `tables` of a dispatch run by state object `state_object`, `bytes` as replay's GPU holds them:
    /// rewritten to replay's identifiers and local root arguments (see the module's documentation). What cannot be
    /// placed is named and left null.
    fn rewrite(
        &mut self,
        c: &Capture,
        state_object: i64,
        tables: &[Table],
        bytes: &mut [u8],
        what: &str,
    ) -> Result<(), String> {
        if self.state_object(c, state_object).is_none() {
            return Err(format!("its state object #{state_object} was not made"));
        }
        let mut records = Vec::new();
        let unplaced = {
            let source = Described {
                c,
                declared: &self.rays.declared,
            };
            let mut decode = |local: &Local| match local {
                Local::Object(id) => c
                    .row(
                        "SELECT decoded_json FROM d12_roots WHERE id = ?1",
                        [*id as i64],
                        |r| r.get::<_, String>(0),
                    )
                    .and_then(|d| serde_json::from_str::<Value>(&d).ok())
                    .filter(|d| d["parameters"].is_array())
                    .ok_or_else(|| format!("local root signature #{id} was not decoded")),
                Local::Declared(serialized) => programs::root_layout(serialized),
            };
            tables::walk(
                &source,
                state_object as u64,
                tables,
                bytes,
                &mut decode,
                &mut |r| {
                    let placed = r.placed.map(|p| (p.export.to_string(), p.arguments.to_vec()));
                    records.push((r.at, placed))
                },
            )
        };
        let mut failed = BTreeMap::<String, u64>::new();
        for (why, n) in unplaced {
            *failed.entry(why).or_default() += n;
        }
        let heaps = self.heap_descriptions(c);
        for (at, placed) in records {
            // One that cannot be placed runs no shader: the game's identifier means nothing to replay's driver.
            let Some((export, arguments)) = placed else {
                bytes[at..at + IDENTIFIER].fill(0);
                continue;
            };
            let identifier = self.identifier(state_object, &export);
            if identifier.is_none() {
                *failed
                    .entry(format!(
                        "replay's state object gives export {export:?} no identifier"
                    ))
                    .or_default() += 1;
            }
            bytes[at..at + IDENTIFIER].copy_from_slice(&identifier.unwrap_or([0; IDENTIFIER]));
            for a in arguments {
                if matches!(a.kind, Kind::Constants { .. }) {
                    continue;
                }
                let field = at + a.at;
                let game = u64::from_le_bytes(bytes[field..field + 8].try_into().expect("8 bytes"));
                if game == 0 {
                    continue;
                }
                let mine = match a.kind {
                    Kind::Table => match tables::slot(heaps.iter().map(|(id, d)| (*id, d)), game) {
                        Some((heap, slot)) => match self.heap(c, heap as i64) {
                            Some(m) => Ok(m.gpu(slot).ptr),
                            None => Err(format!("descriptor heap #{heap} was not made")),
                        },
                        None => Err(format!(
                            "descriptor handle {game:#x} is in no shader-visible heap"
                        )),
                    },
                    Kind::ShaderResource => self.record_address(c, game, true),
                    _ => self.record_address(c, game, false),
                };
                let mine = mine.unwrap_or_else(|why| {
                    *failed.entry(format!("a local root argument: {why}")).or_default() += 1;
                    0
                });
                bytes[field..field + 8].copy_from_slice(&mine.to_le_bytes());
            }
        }
        for (why, n) in failed {
            self.warn(format!("{what}: {n} shader records: {why}; left null"));
        }
        Ok(())
    }

    /// Copies of tables (their bytes one after another in `bytes`) in upload memory of replay's, each at an
    /// address a table may start at: their addresses.
    fn staged_tables(&mut self, tables: &[Table], bytes: &[u8]) -> Result<Vec<u64>, String> {
        let mut out = Vec::new();
        let mut start = 0;
        for t in tables {
            let (upload, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, t.bytes)?;
            unsafe { core::ptr::copy_nonoverlapping(bytes[start..].as_ptr(), p, t.bytes as usize) };
            start += t.bytes as usize;
            out.push(unsafe { upload.GetGPUVirtualAddress() });
        }
        Ok(out)
    }

    /// A direct DispatchRays (event `seq`, recorded description `a[0]`), run on replay's copies of its tables. Read
    /// before the event records anything.
    pub(super) fn dispatch_rays(
        &mut self,
        c: &Capture,
        seq: i64,
        a: &Value,
    ) -> Result<super::indirect::Arguments, String> {
        let state_object = c
            .row(
                "SELECT s.root_args_json FROM d12_draw_state s JOIN draws d ON d.n = s.draw WHERE d.event = ?1",
                [seq],
                |r| r.get::<_, String>(0),
            )
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .and_then(|s| s["state_object"].as_i64())
            .unwrap_or(0);
        let d = &a[0];
        let mut tables = Vec::new();
        let mut bytes = Vec::new();
        for (which, name) in [
            (Which::RayGeneration, "ray_generation"),
            (Which::Miss, "miss"),
            (Which::HitGroups, "hit_groups"),
            (Which::Callable, "callable"),
        ] {
            let t = &d[name];
            let size = t["bytes"].as_u64().unwrap_or(0);
            let stride = if which == Which::RayGeneration {
                size
            } else {
                t["stride"].as_u64().unwrap_or(0)
            };
            let Some((id, offset)) = key(&t["address"]).filter(|_| size > 0) else {
                continue;
            };
            bytes.extend(self.read_range(id, offset, size)?);
            tables.push(Table {
                which,
                address: offset,
                bytes: size,
                stride,
            });
        }
        let what = format!("DispatchRays (event {seq})");
        if let Err(why) = self.rewrite(c, state_object, &tables, &mut bytes, &what) {
            self.warn(format!("{what} is not replayed: {why}"));
            return Ok(super::indirect::Arguments::Skip);
        }
        let staged = self.staged_tables(&tables, &bytes)?;
        let mut desc = D3D12_DISPATCH_RAYS_DESC {
            Width: d["size"][0].as_u64().unwrap_or(0) as u32,
            Height: d["size"][1].as_u64().unwrap_or(0) as u32,
            Depth: d["size"][2].as_u64().unwrap_or(0) as u32,
            ..Default::default()
        };
        for (t, address) in tables.iter().zip(staged) {
            let range = D3D12_GPU_VIRTUAL_ADDRESS_RANGE_AND_STRIDE {
                StartAddress: address,
                SizeInBytes: t.bytes,
                StrideInBytes: t.stride,
            };
            match t.which {
                Which::RayGeneration => {
                    desc.RayGenerationShaderRecord = D3D12_GPU_VIRTUAL_ADDRESS_RANGE {
                        StartAddress: address,
                        SizeInBytes: t.bytes,
                    }
                }
                Which::Miss => desc.MissShaderTable = range,
                Which::HitGroups => desc.HitGroupTable = range,
                Which::Callable => desc.CallableShaderTable = range,
            }
        }
        Ok(super::indirect::Arguments::Rays(desc))
    }

    /// The tables each of an ExecuteIndirect's DispatchRays arguments names (`tables`, by record, as the game's
    /// records hold them), run by state object `state_object`: replay's copies of them, as replay's GPU holds them
    /// now, each where the record's argument holds the table's address (see [`table_field`]).
    pub(super) fn indirect_tables(
        &mut self,
        c: &Capture,
        seq: i64,
        state_object: i64,
        tables: &[Vec<Table>],
    ) -> Result<Vec<Vec<(usize, u64)>>, String> {
        let what = format!("ExecuteIndirect (event {seq})");
        // Each distinct table read and rewritten once.
        let mut staged = HashMap::<(u64, u64, u64), u64>::new();
        let mut out = Vec::with_capacity(tables.len());
        for record in tables {
            let mut fields = Vec::new();
            for t in record {
                let k = (t.address, t.bytes, t.stride);
                let address = match staged.get(&k) {
                    Some(&a) => a,
                    None => {
                        let found = self.addresses_of(c).find(t.address);
                        let Some(&(id, offset)) =
                            found.iter().find(|(id, _)| self.resources.contains_key(id))
                        else {
                            return Err(format!(
                                "a shader table at {:#x} is in no buffer replay made",
                                t.address
                            ));
                        };
                        let mut bytes = self.read_range(id, offset, t.bytes)?;
                        self.rewrite(c, state_object, std::slice::from_ref(t), &mut bytes, &what)?;
                        let a = self.staged_tables(std::slice::from_ref(t), &bytes)?[0];
                        staged.insert(k, a);
                        a
                    }
                };
                fields.push((table_field(t.which), address));
            }
            out.push(fields);
        }
        Ok(out)
    }
}

/// Where a DispatchRays argument holds a table's address, from the argument's start.
pub(super) fn table_field(which: Which) -> usize {
    TABLE_AT.iter().find(|(w, _)| *w == which).expect("every table").1
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::mem::{offset_of, size_of};

    #[test]
    fn addresses_name_structures_by_buffer_and_offset() {
        assert_eq!(key(&json!({"resource": 7, "offset": 256})), Some((7, 256)));
        // Shared memory: the first candidate.
        let shared = json!({"candidates": [{"resource": 3, "offset": 0}, {"resource": 4, "offset": 0}],
            "diagnostic": "GPU address is not uniquely associated with a live resource"});
        assert_eq!(key(&shared), Some((3, 0)));
        assert_eq!(key(&Value::Null), None);
    }

    #[test]
    fn recorded_inputs_become_the_apis_with_each_address_as_given() {
        let bottom = json!({"type": 1, "flags": 2, "count": 2, "layout": 1, "content": {"geometry": [
            {"type": 0, "flags": 1, "shape": {"transform": {"resource": 9, "offset": 48}, "index_format": 57,
                "vertex_format": 6, "indices": 3, "vertices": 3, "index_buffer": {"resource": 8, "offset": 0},
                "vertex_buffer": {"address": {"resource": 7, "offset": 16}, "stride": 12}}},
            {"type": 1, "flags": 0, "shape": {"aabbs": 1, "buffer": {"address": {"resource": 6, "offset": 0},
                "stride": 24}}},
        ]}});
        let mut asked = Vec::new();
        let mut address = |part: &str, geometry: Option<usize>, v: &Value| {
            asked.push((part.to_string(), geometry));
            v["resource"]
                .as_u64()
                .map_or(0, |r| r << 16 | v["offset"].as_u64().unwrap())
        };
        let mut i = Inputs::of(&bottom, &mut address).unwrap();
        assert_eq!(
            asked,
            [
                ("transform".to_string(), Some(0)),
                ("indices".into(), Some(0)),
                ("vertices".into(), Some(0)),
                ("aabbs".into(), Some(1))
            ]
        );
        let inputs = *i.get();
        // An array of pointers is read into an array.
        assert_eq!(inputs.DescsLayout, D3D12_ELEMENTS_LAYOUT_ARRAY);
        assert_eq!((inputs.Type.0, inputs.Flags.0, inputs.NumDescs), (1, 2, 2));
        let g = unsafe { core::slice::from_raw_parts(inputs.Anonymous.pGeometryDescs, 2) };
        let t = unsafe { g[0].Anonymous.Triangles };
        assert_eq!(
            (
                t.Transform3x4,
                t.IndexBuffer,
                t.VertexBuffer.StartAddress,
                t.VertexBuffer.StrideInBytes
            ),
            (9 << 16 | 48, 8 << 16, 7 << 16 | 16, 12)
        );
        assert_eq!(
            (t.IndexFormat, t.VertexFormat, t.IndexCount, t.VertexCount),
            (DXGI_FORMAT(57), DXGI_FORMAT(6), 3, 3)
        );
        let a = unsafe { g[1].Anonymous.AABBs };
        assert_eq!(
            (
                g[1].Type.0,
                a.AABBCount,
                a.AABBs.StartAddress,
                a.AABBs.StrideInBytes
            ),
            (1, 1, 6 << 16, 24)
        );

        let top = json!({"type": 0, "flags": 0, "count": 2, "layout": 0,
            "content": {"instances": {"resource": 5, "offset": 64}}});
        let mut i = Inputs::of(&top, &mut |part, _, v| {
            assert_eq!(part, "instances");
            v["offset"].as_u64().unwrap()
        })
        .unwrap();
        assert_eq!(unsafe { i.get().Anonymous.InstanceDescs }, 64);

        let short = json!({"type": 1, "count": 2, "content": {"geometry": [
            {"type": 1, "shape": {"aabbs": 1, "buffer": {"address": null, "stride": 24}}}]}});
        assert!(Inputs::of(&short, &mut |_, _, _| 0).is_err());
        let null = json!({"type": 1, "count": 1, "content": {"geometry": [null]}});
        assert!(Inputs::of(&null, &mut |_, _, _| 0).is_err());
    }

    #[test]
    fn a_dispatch_holds_each_tables_address_where_the_api_does() {
        assert_eq!(
            [
                Which::RayGeneration,
                Which::Miss,
                Which::HitGroups,
                Which::Callable
            ]
            .map(table_field),
            [
                offset_of!(D3D12_DISPATCH_RAYS_DESC, RayGenerationShaderRecord),
                offset_of!(D3D12_DISPATCH_RAYS_DESC, MissShaderTable),
                offset_of!(D3D12_DISPATCH_RAYS_DESC, HitGroupTable),
                offset_of!(D3D12_DISPATCH_RAYS_DESC, CallableShaderTable),
            ]
        );
        assert_eq!(
            (INSTANCE, INSTANCE_STRUCTURE),
            (
                size_of::<D3D12_RAYTRACING_INSTANCE_DESC>(),
                offset_of!(D3D12_RAYTRACING_INSTANCE_DESC, AccelerationStructure)
            )
        );
        assert_eq!(
            SERIALIZED_HEADER,
            size_of::<D3D12_SERIALIZED_RAYTRACING_ACCELERATION_STRUCTURE_HEADER>()
        );
    }
}
