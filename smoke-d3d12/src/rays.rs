//! The `rays` capture: raytracing as a frame uses it, where the device has raytracing tier 1.1.
//!
//! Before the frame, two bottom-level acceleration structures are built (a triangle, indexed, and a procedural box)
//! and a top-level one of two instances of them, and then the buffers they were built from are overwritten: replay
//! has only what capture copied at the builds. The triangle's is compacted into another buffer, which the scene's
//! instance names, and the triangle's own then built again of another triangle; the box's is built from another
//! box and updated to its own. Those are three lists of one ExecuteCommandLists, which read inputs their own barriers
//! have not moved: where the list before left them, and where nothing has. The pipeline is a collection (the miss shader) and a library (a ray
//! generation shader, the hit groups' shaders), and AddToStateObject adds the ray generation shader again under
//! another name. Local root signatures give the ray generation records a constant, the scene as a root SRV and the
//! output UAV through a descriptor table; the hit group records a constant each, and the callable shader's record one
//! the ray generation shader adds to its own.
//!
//! The frame: a DispatchRays into one output; a bottom-level build of a triangle no ray reaches from a buffer the
//! frame writes, updated in place to another, and a top-level build of it (its instance given by an array of
//! pointers), then cloned; a DispatchRays of the added shader, through the new scene, into a second output; a compute
//! dispatch tracing inline (RayQuery) through the first scene, its view in a descriptor table, into a third; and
//! another through the clone, a root SRV, into a fourth. Nothing but the shader tables names the first two outputs.
use super::indirect::{
    check, default_buffer, dxil, fill, read_buffer, request, root_constants, root_signature, root_view,
    upload, written,
};
use super::*;

/// The rays cast: wide and high.
const SIZE: (u32, u32) = (8, 4);
/// The ray generation records' constants (`k`), and the hit groups' (`group`).
const K: [u32; 2] = [0x11, 0x12];
const GROUPS: [u32; 2] = [0x21, 0x22];
/// The callable shader's record's constant, which it adds to the ray generation record's.
const CALL: u32 = 0x30;
/// The instances' IDs: the triangle and the box in the first scene, the frame's triangle in the second.
const INSTANCES: [u32; 3] = [5, 7, 9];

const LIBRARY: &str = r#"
struct Payload { uint v; };
struct Face { uint face; };
struct Call { uint v; };
RaytracingAccelerationStructure scene : register(t0);
RWByteAddressBuffer written : register(u0);
cbuffer Ray : register(b0) { uint k; };
cbuffer Group : register(b1) { uint group; };

[shader("raygeneration")] void Gen() {
    uint2 at = DispatchRaysIndex().xy;
    uint2 size = DispatchRaysDimensions().xy;
    RayDesc r;
    r.Origin = float3((at.x + 0.5) / size.x * 4.0 - 2.0, (at.y + 0.5) / size.y * 4.0 - 2.0, -1.0);
    r.Direction = float3(0, 0, 1);
    r.TMin = 0;
    r.TMax = 10;
    Payload p;
    p.v = 0;
    TraceRay(scene, RAY_FLAG_NONE, 0xff, 0, 1, 0, r, p);
    Call c;
    c.v = k;
    CallShader(0, c);
    written.Store((at.y * size.x + at.x) * 4, p.v | (c.v << 24));
}
[shader("callable")] void Called(inout Call c) { c.v += group; }
[shader("miss")] void Miss(inout Payload p) { p.v = 1; }
[shader("closesthit")] void Hit(inout Payload p, BuiltInTriangleIntersectionAttributes a) {
    p.v = 2 | (InstanceID() << 4) | (PrimitiveIndex() << 8) | (group << 16);
}
[shader("intersection")] void Box() {
    Face f;
    f.face = 0;
    ReportHit(1.0, 0, f);
}
[shader("closesthit")] void BoxHit(inout Payload p, Face f) {
    p.v = 3 | (InstanceID() << 4) | (PrimitiveIndex() << 8) | (group << 16);
}
"#;

const QUERY: &str = r#"
RaytracingAccelerationStructure scene : register(t0);
RWByteAddressBuffer written : register(u0);
[numthreads(8, 4, 1)] void main(uint3 at : SV_DispatchThreadID) {
    RayDesc r;
    r.Origin = float3((at.x + 0.5) / 8.0 * 4.0 - 2.0, (at.y + 0.5) / 4.0 * 4.0 - 2.0, -1.0);
    r.Direction = float3(0, 0, 1);
    r.TMin = 0;
    r.TMax = 10;
    RayQuery<RAY_FLAG_FORCE_OPAQUE> q;
    q.TraceRayInline(scene, RAY_FLAG_NONE, 0xff, r);
    while (q.Proceed()) {}
    uint v = q.CommittedStatus() == COMMITTED_TRIANGLE_HIT
        ? 2 | (q.CommittedInstanceID() << 4) | (q.CommittedPrimitiveIndex() << 8) : 1;
    written.Store((at.y * 8 + at.x) * 4, v);
}
"#;

/// Where ray `(x, y)` starts, in the plane z = -1.
fn origin(x: u32, y: u32) -> (f32, f32) {
    let (w, h) = SIZE;
    (
        (x as f32 + 0.5) / w as f32 * 4.0 - 2.0,
        (y as f32 + 0.5) / h as f32 * 4.0 - 2.0,
    )
}

/// The first scene's triangle, (-2, -2), (0.2, -2), (-2, 2.2), and the frame's, its mirror through the origin.
fn triangle(mirrored: bool) -> [f32; 9] {
    let s = if mirrored { -1.0 } else { 1.0 };
    [
        -2.0 * s,
        -2.0 * s,
        0.0,
        0.2 * s,
        -2.0 * s,
        0.0,
        -2.0 * s,
        2.2 * s,
        0.0,
    ]
}

fn in_triangle((x, y): (f32, f32), mirrored: bool) -> bool {
    let (x, y) = if mirrored { (-x, -y) } else { (x, y) };
    (x + 2.0) / 2.2 + (y + 2.0) / 4.2 <= 1.0
}

/// A triangle no ray reaches.
const FAR: [f32; 9] = [100.0, 100.0, 0.0, 101.0, 100.0, 0.0, 100.0, 101.0, 0.0];

/// The box: x 0.5 to 1.5, y -1 to 1, z 0 to 1.
const BOX: [f32; 6] = [0.5, -1.0, 0.0, 1.5, 1.0, 1.0];
/// The box its structure is first built from, before the update to [`BOX`]: rays reach it that [`BOX`] misses.
const OLD_BOX: [f32; 6] = [0.5, 1.2, 0.0, 1.5, 1.9, 1.0];

fn in_box((x, y): (f32, f32)) -> bool {
    (BOX[0]..=BOX[3]).contains(&x) && (BOX[1]..=BOX[4]).contains(&y)
}

/// What ray `(x, y)` of DispatchRays `n` (0: the first scene, 1: the frame's) writes.
fn traced(n: usize, x: u32, y: u32) -> u32 {
    let at = origin(x, y);
    let hit = if n == 0 && in_triangle(at, false) {
        2 | (INSTANCES[0] << 4) | (GROUPS[0] << 16)
    } else if n == 0 && in_box(at) {
        3 | (INSTANCES[1] << 4) | (GROUPS[1] << 16)
    } else if n == 1 && in_triangle(at, true) {
        2 | (INSTANCES[2] << 4) | (GROUPS[0] << 16)
    } else {
        1
    };
    hit | ((K[n] + CALL) << 24)
}

/// What inline trace `n` (0: the first scene, 1: the frame's) writes at `(x, y)`: the scene's triangle (the box is
/// a candidate it does not commit).
fn queried(n: usize, x: u32, y: u32) -> u32 {
    if in_triangle(origin(x, y), n == 1) {
        2 | (INSTANCES[n * 2] << 4)
    } else {
        1
    }
}

fn bytes_of<T>(values: &[T]) -> &[u8] {
    unsafe { core::slice::from_raw_parts(values.as_ptr().cast(), size_of_val(values)) }
}

/// A buffer an acceleration structure (or a build's scratch) can live in, in `state`.
unsafe fn structure_buffer(
    device: &ID3D12Device,
    bytes: u64,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    unsafe {
        contents::committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &contents::desc(
                D3D12_RESOURCE_DIMENSION_BUFFER,
                DXGI_FORMAT_UNKNOWN,
                (bytes.max(256), 1),
                (1, 1, 1),
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ),
            state,
        )
    }
}

/// An acceleration structure each of `builds` can be built into, and scratch enough to build or update it, made on
/// `device`.
unsafe fn structure(
    device: &ID3D12Device5,
    builds: &[&D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS],
) -> (ID3D12Resource, ID3D12Resource) {
    unsafe {
        let (mut result, mut scratch) = (0, 0);
        for inputs in builds {
            let mut info = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_PREBUILD_INFO::default();
            device.GetRaytracingAccelerationStructurePrebuildInfo(*inputs, &mut info);
            result = info.ResultDataMaxSizeInBytes.max(result);
            scratch = info
                .ScratchDataSizeInBytes
                .max(info.UpdateScratchDataSizeInBytes)
                .max(scratch);
        }
        let d: ID3D12Device = device.cast().unwrap();
        (
            structure_buffer(&d, result, D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE),
            structure_buffer(&d, scratch, D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
        )
    }
}

/// `CopyRaytracingAccelerationStructure(dest, source, mode)`, ordered before what uses either.
unsafe fn copy_structure(
    list: &ID3D12GraphicsCommandList4,
    dest: &ID3D12Resource,
    source: &ID3D12Resource,
    mode: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE,
) {
    unsafe {
        list.CopyRaytracingAccelerationStructure(
            dest.GetGPUVirtualAddress(),
            source.GetGPUVirtualAddress(),
            mode,
        );
        uav_barrier(list, dest);
        uav_barrier(list, source);
    }
}

unsafe fn direct_list(device: &ID3D12Device) -> ID3D12GraphicsCommandList4 {
    unsafe {
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap()
    }
}

unsafe fn transitions(
    list: &ID3D12GraphicsCommandList4,
    buffers: &[&ID3D12Resource],
    from: D3D12_RESOURCE_STATES,
    to: D3D12_RESOURCE_STATES,
) {
    let plain: ID3D12GraphicsCommandList = list.cast().unwrap();
    for b in buffers {
        unsafe { graphics::transition(&plain, b, from, to) };
    }
}

/// A triangle's geometry: three vertices at GPU address `vertices`, indexed by three 16-bit indices at `indices`.
fn triangles(vertices: u64, indices: Option<u64>) -> D3D12_RAYTRACING_GEOMETRY_DESC {
    D3D12_RAYTRACING_GEOMETRY_DESC {
        Type: D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES,
        Flags: D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE,
        Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
            Triangles: D3D12_RAYTRACING_GEOMETRY_TRIANGLES_DESC {
                Transform3x4: 0,
                IndexFormat: if indices.is_some() {
                    DXGI_FORMAT_R16_UINT
                } else {
                    DXGI_FORMAT_UNKNOWN
                },
                VertexFormat: DXGI_FORMAT_R32G32B32_FLOAT,
                IndexCount: if indices.is_some() { 3 } else { 0 },
                VertexCount: 3,
                IndexBuffer: indices.unwrap_or(0),
                VertexBuffer: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                    StartAddress: vertices,
                    StrideInBytes: 12,
                },
            },
        },
    }
}

fn bottom(
    geometry: &D3D12_RAYTRACING_GEOMETRY_DESC,
    flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAGS,
) -> D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
        Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_BOTTOM_LEVEL,
        Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE | flags,
        NumDescs: 1,
        DescsLayout: D3D12_ELEMENTS_LAYOUT_ARRAY,
        Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
            pGeometryDescs: geometry,
        },
    }
}

fn top(
    instances: u64,
    count: u32,
    layout: D3D12_ELEMENTS_LAYOUT,
) -> D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
    D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS {
        Type: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL,
        Flags: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PREFER_FAST_TRACE,
        NumDescs: count,
        DescsLayout: layout,
        Anonymous: D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS_0 {
            InstanceDescs: instances,
        },
    }
}

/// A `D3D12_RAYTRACING_INSTANCE_DESC`: the identity transform, `id`, mask 0xFF, hit group offset `group`, of the
/// structure at `structure`.
fn instance(id: u32, group: u32, structure: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    for v in [1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0] {
        out.extend(v.to_le_bytes());
    }
    out.extend((id | (0xFF << 24)).to_le_bytes());
    out.extend(group.to_le_bytes());
    out.extend(structure.to_le_bytes());
    out
}

unsafe fn build(
    list: &ID3D12GraphicsCommandList4,
    inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    structure: &(ID3D12Resource, ID3D12Resource),
) {
    unsafe { build_from(list, inputs, structure, false) }
}

/// An update of `structure` in place, to `inputs` (built with ALLOW_UPDATE).
unsafe fn update(
    list: &ID3D12GraphicsCommandList4,
    inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    structure: &(ID3D12Resource, ID3D12Resource),
) {
    unsafe { build_from(list, inputs, structure, true) }
}

unsafe fn build_from(
    list: &ID3D12GraphicsCommandList4,
    inputs: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
    (dest, scratch): &(ID3D12Resource, ID3D12Resource),
    in_place: bool,
) {
    unsafe {
        let mut inputs = *inputs;
        if in_place {
            inputs.Flags |= D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_PERFORM_UPDATE;
        }
        list.BuildRaytracingAccelerationStructure(
            &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC {
                DestAccelerationStructureData: dest.GetGPUVirtualAddress(),
                Inputs: inputs,
                SourceAccelerationStructureData: if in_place { dest.GetGPUVirtualAddress() } else { 0 },
                ScratchAccelerationStructureData: scratch.GetGPUVirtualAddress(),
            },
            None,
        );
        uav_barrier(list, dest);
    }
}

unsafe fn uav_barrier(list: &ID3D12GraphicsCommandList4, r: &ID3D12Resource) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            UAV: ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                pResource: ManuallyDrop::new(Some(r.clone())),
            }),
        },
        ..Default::default()
    };
    unsafe {
        list.ResourceBarrier(std::slice::from_ref(&barrier));
        ManuallyDrop::drop(&mut (*barrier.Anonymous.UAV).pResource);
    }
}

fn subobject<T>(kind: D3D12_STATE_SUBOBJECT_TYPE, desc: &T) -> D3D12_STATE_SUBOBJECT {
    D3D12_STATE_SUBOBJECT {
        Type: kind,
        pDesc: (desc as *const T).cast(),
    }
}

fn bytecode(b: &[u8]) -> D3D12_SHADER_BYTECODE {
    D3D12_SHADER_BYTECODE {
        pShaderBytecode: b.as_ptr().cast(),
        BytecodeLength: b.len(),
    }
}

/// The local root signatures: the ray generation records' (a constant, the scene, a table of the output) and the
/// hit groups' (a constant).
unsafe fn local_roots(device: &ID3D12Device) -> (ID3D12RootSignature, ID3D12RootSignature) {
    let range = D3D12_DESCRIPTOR_RANGE {
        RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
        NumDescriptors: 1,
        BaseShaderRegister: 0,
        RegisterSpace: 0,
        OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
    };
    let table = D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                NumDescriptorRanges: 1,
                pDescriptorRanges: &range,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    };
    unsafe {
        (
            root_signature(
                device,
                &[
                    root_constants(0, 1),
                    root_view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
                    table,
                ],
                D3D12_ROOT_SIGNATURE_FLAG_LOCAL_ROOT_SIGNATURE,
            ),
            root_signature(
                device,
                &[root_constants(1, 1)],
                D3D12_ROOT_SIGNATURE_FLAG_LOCAL_ROOT_SIGNATURE,
            ),
        )
    }
}

/// The pipeline (see the module's documentation) and what AddToStateObject made of it; or why the device cannot.
unsafe fn pipelines(
    device: &ID3D12Device,
    global: &ID3D12RootSignature,
    (gen_root, group_root): &(ID3D12RootSignature, ID3D12RootSignature),
) -> std::result::Result<(ID3D12StateObject, ID3D12StateObject), String> {
    unsafe {
        let library = dxil(LIBRARY, "lib_6_3")?;
        let device5: ID3D12Device5 = device.cast().map_err(|e| format!("ID3D12Device5: {e}"))?;
        // AddToStateObject: tier 1.1 devices have it.
        let device7: ID3D12Device7 = device.cast().expect("rays: ID3D12Device7");
        let config = D3D12_STATE_OBJECT_CONFIG {
            Flags: D3D12_STATE_OBJECT_FLAG_ALLOW_STATE_OBJECT_ADDITIONS,
        };
        let global = D3D12_GLOBAL_ROOT_SIGNATURE {
            pGlobalRootSignature: ManuallyDrop::new(Some(global.clone())),
        };
        let shader = D3D12_RAYTRACING_SHADER_CONFIG {
            MaxPayloadSizeInBytes: 4,
            MaxAttributeSizeInBytes: 8,
        };
        let pipeline = D3D12_RAYTRACING_PIPELINE_CONFIG {
            MaxTraceRecursionDepth: 1,
        };
        let export = |name: PCWSTR, rename: PCWSTR| D3D12_EXPORT_DESC {
            Name: name,
            ExportToRename: rename,
            Flags: D3D12_EXPORT_FLAG_NONE,
        };

        // The collection: the miss shader alone.
        let mut miss_export = [export(w!("Miss"), PCWSTR::null())];
        let miss_library = D3D12_DXIL_LIBRARY_DESC {
            DXILLibrary: bytecode(&library),
            NumExports: 1,
            pExports: miss_export.as_mut_ptr(),
        };
        let collection_subobjects = [
            subobject(D3D12_STATE_SUBOBJECT_TYPE_STATE_OBJECT_CONFIG, &config),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_DXIL_LIBRARY, &miss_library),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_GLOBAL_ROOT_SIGNATURE, &global),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_SHADER_CONFIG, &shader),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_PIPELINE_CONFIG, &pipeline),
        ];
        let collection: ID3D12StateObject = device5
            .CreateStateObject(&D3D12_STATE_OBJECT_DESC {
                Type: D3D12_STATE_OBJECT_TYPE_COLLECTION,
                NumSubobjects: collection_subobjects.len() as u32,
                pSubobjects: collection_subobjects.as_ptr(),
            })
            .unwrap_or_else(|e| panic!("rays: CreateStateObject (collection): {e}"));

        // The pipeline: the collection, the rest of the library, its hit groups and local root signatures.
        let mut exports = [
            export(w!("Gen"), PCWSTR::null()),
            export(w!("Hit"), PCWSTR::null()),
            export(w!("Box"), PCWSTR::null()),
            export(w!("BoxHit"), PCWSTR::null()),
            export(w!("Called"), PCWSTR::null()),
        ];
        let rest = D3D12_DXIL_LIBRARY_DESC {
            DXILLibrary: bytecode(&library),
            NumExports: exports.len() as u32,
            pExports: exports.as_mut_ptr(),
        };
        let existing = D3D12_EXISTING_COLLECTION_DESC {
            pExistingCollection: ManuallyDrop::new(Some(collection.clone())),
            NumExports: 0,
            pExports: core::ptr::null_mut(),
        };
        let triangles = D3D12_HIT_GROUP_DESC {
            HitGroupExport: w!("Triangles"),
            Type: D3D12_HIT_GROUP_TYPE_TRIANGLES,
            AnyHitShaderImport: PCWSTR::null(),
            ClosestHitShaderImport: w!("Hit"),
            IntersectionShaderImport: PCWSTR::null(),
        };
        let boxes = D3D12_HIT_GROUP_DESC {
            HitGroupExport: w!("Boxes"),
            Type: D3D12_HIT_GROUP_TYPE_PROCEDURAL_PRIMITIVE,
            AnyHitShaderImport: PCWSTR::null(),
            ClosestHitShaderImport: w!("BoxHit"),
            IntersectionShaderImport: w!("Box"),
        };
        let gen_local = D3D12_LOCAL_ROOT_SIGNATURE {
            pLocalRootSignature: ManuallyDrop::new(Some(gen_root.clone())),
        };
        let group_local = D3D12_LOCAL_ROOT_SIGNATURE {
            pLocalRootSignature: ManuallyDrop::new(Some(group_root.clone())),
        };
        let gen_names = [w!("Gen")];
        let group_names = [w!("Triangles"), w!("Boxes"), w!("Called")];
        let mut gen_association = D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION {
            pSubobjectToAssociate: core::ptr::null(),
            NumExports: 1,
            pExports: gen_names.as_ptr() as *mut PCWSTR,
        };
        let mut group_association = D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION {
            pSubobjectToAssociate: core::ptr::null(),
            NumExports: group_names.len() as u32,
            pExports: group_names.as_ptr() as *mut PCWSTR,
        };
        let mut subobjects = [
            subobject(D3D12_STATE_SUBOBJECT_TYPE_STATE_OBJECT_CONFIG, &config),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_GLOBAL_ROOT_SIGNATURE, &global),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_SHADER_CONFIG, &shader),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_PIPELINE_CONFIG, &pipeline),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_EXISTING_COLLECTION, &existing),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_DXIL_LIBRARY, &rest),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_HIT_GROUP, &triangles),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_HIT_GROUP, &boxes),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_LOCAL_ROOT_SIGNATURE, &gen_local),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_LOCAL_ROOT_SIGNATURE, &group_local),
            subobject(
                D3D12_STATE_SUBOBJECT_TYPE_SUBOBJECT_TO_EXPORTS_ASSOCIATION,
                &gen_association,
            ),
            subobject(
                D3D12_STATE_SUBOBJECT_TYPE_SUBOBJECT_TO_EXPORTS_ASSOCIATION,
                &group_association,
            ),
        ];
        gen_association.pSubobjectToAssociate = &subobjects[8];
        group_association.pSubobjectToAssociate = &subobjects[9];
        subobjects[10].pDesc = (&gen_association as *const D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION).cast();
        subobjects[11].pDesc = (&group_association as *const D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION).cast();
        let made: ID3D12StateObject = device5
            .CreateStateObject(&D3D12_STATE_OBJECT_DESC {
                Type: D3D12_STATE_OBJECT_TYPE_RAYTRACING_PIPELINE,
                NumSubobjects: subobjects.len() as u32,
                pSubobjects: subobjects.as_ptr(),
            })
            .unwrap_or_else(|e| panic!("rays: CreateStateObject (pipeline): {e}"));

        // The addition: the ray generation shader again as `Gen2`, with its local root signature.
        let mut renamed = [export(w!("Gen2"), w!("Gen"))];
        let again = D3D12_DXIL_LIBRARY_DESC {
            DXILLibrary: bytecode(&library),
            NumExports: 1,
            pExports: renamed.as_mut_ptr(),
        };
        let gen2_names = [w!("Gen2")];
        let mut gen2_association = D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION {
            pSubobjectToAssociate: core::ptr::null(),
            NumExports: 1,
            pExports: gen2_names.as_ptr() as *mut PCWSTR,
        };
        let mut added = [
            subobject(D3D12_STATE_SUBOBJECT_TYPE_STATE_OBJECT_CONFIG, &config),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_GLOBAL_ROOT_SIGNATURE, &global),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_SHADER_CONFIG, &shader),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_PIPELINE_CONFIG, &pipeline),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_DXIL_LIBRARY, &again),
            subobject(D3D12_STATE_SUBOBJECT_TYPE_LOCAL_ROOT_SIGNATURE, &gen_local),
            subobject(
                D3D12_STATE_SUBOBJECT_TYPE_SUBOBJECT_TO_EXPORTS_ASSOCIATION,
                &gen2_association,
            ),
        ];
        gen2_association.pSubobjectToAssociate = &added[5];
        added[6].pDesc = (&gen2_association as *const D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION).cast();
        let grown = device7
            .AddToStateObject(
                &D3D12_STATE_OBJECT_DESC {
                    Type: D3D12_STATE_OBJECT_TYPE_RAYTRACING_PIPELINE,
                    NumSubobjects: added.len() as u32,
                    pSubobjects: added.as_ptr(),
                },
                &made,
            )
            .unwrap_or_else(|e| panic!("rays: AddToStateObject: {e}"));
        let mut global = global;
        ManuallyDrop::drop(&mut global.pGlobalRootSignature);
        let mut existing = existing;
        ManuallyDrop::drop(&mut existing.pExistingCollection);
        let (mut gen_local, mut group_local) = (gen_local, group_local);
        ManuallyDrop::drop(&mut gen_local.pLocalRootSignature);
        ManuallyDrop::drop(&mut group_local.pLocalRootSignature);
        Ok((made, grown))
    }
}

/// The shader identifier of `export` in `pipeline`.
unsafe fn identifier(pipeline: &ID3D12StateObject, export: PCWSTR) -> Vec<u8> {
    unsafe {
        let properties: ID3D12StateObjectProperties = pipeline.cast().unwrap();
        let id = properties.GetShaderIdentifier(export);
        assert!(!id.is_null(), "{}'s shader identifier", export.display());
        core::slice::from_raw_parts(id.cast::<u8>(), D3D12_SHADER_IDENTIFIER_SIZE_IN_BYTES as usize).to_vec()
    }
}

/// The bytes of a shader record of `stride`: `identifier`, then `arguments` at their offsets.
fn record(identifier: &[u8], arguments: &[(usize, &[u8])], stride: usize) -> Vec<u8> {
    let mut out = identifier.to_vec();
    out.resize(stride, 0);
    for (at, bytes) in arguments {
        out[*at..*at + bytes.len()].copy_from_slice(bytes);
    }
    out
}

/// A raw UAV of `bytes` of `buffer`, at `handle`.
unsafe fn raw_uav(
    device: &ID3D12Device,
    buffer: &ID3D12Resource,
    bytes: u64,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    unsafe {
        device.CreateUnorderedAccessView(
            buffer,
            None,
            Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
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
            }),
            handle,
        );
    }
}

/// The `rays` capture (see the module's documentation), where the device can. Fence values `first` to `first + 12`.
pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    first: u64,
) -> std::result::Result<(), String> {
    unsafe {
        use graphics::transition;
        let mut o5 = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS5,
                (&mut o5 as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS5).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS5>() as u32,
            )
            .map_err(|e| format!("CheckFeatureSupport(OPTIONS5): {e}"))?;
        if o5.RaytracingTier.0 < D3D12_RAYTRACING_TIER_1_1.0 {
            return Err(format!("raytracing tier {}, not 1.1", o5.RaytracingTier.0));
        }
        let device5: ID3D12Device5 = device.cast().unwrap();
        let global = root_signature(device, &[], D3D12_ROOT_SIGNATURE_FLAG_NONE);
        let locals = local_roots(device);
        let (pipeline, grown) = pipelines(device, &global, &locals)?;

        // The scenes' inputs, in default heaps, and the buffers that fill them: the box's structure is first built
        // from the box at 0, then updated to the one at 32; the frame's from the triangle at 0, then updated to the
        // one at 128.
        let va = |r: &ID3D12Resource| r.GetGPUVirtualAddress();
        let first_triangle = bytes_of(&triangle(false)).to_vec();
        let mut frame_triangles = bytes_of(&FAR).to_vec();
        frame_triangles.resize(128, 0);
        frame_triangles.extend(bytes_of(&triangle(true)));
        let indices: Vec<u8> = bytes_of(&[0u16, 1, 2, 0]).to_vec();
        let mut boxes = bytes_of(&OLD_BOX).to_vec();
        boxes.resize(32, 0);
        boxes.extend(bytes_of(&BOX));
        let vertices = default_buffer(device, 256, false);
        let index_buffer = default_buffer(device, 256, false);
        let aabbs = default_buffer(device, 256, false);
        let far = default_buffer(device, 256, false);
        let frame_vertices = default_buffer(device, 256, false);
        let staged = [
            upload(device, &first_triangle, 256),
            upload(device, &indices, 256),
            upload(device, &boxes, 256),
            upload(device, bytes_of(&FAR), 256),
        ];
        let inputs = [&vertices, &index_buffer, &aabbs, &far];
        fill(
            device,
            queue,
            fence,
            first,
            &[
                (inputs[0], &staged[0], 256),
                (inputs[1], &staged[1], 256),
                (inputs[2], &staged[2], 256),
                (inputs[3], &staged[3], 256),
            ],
        );
        let boxes_at = |offset: u64| D3D12_RAYTRACING_GEOMETRY_DESC {
            Type: D3D12_RAYTRACING_GEOMETRY_TYPE_PROCEDURAL_PRIMITIVE_AABBS,
            Flags: D3D12_RAYTRACING_GEOMETRY_FLAG_OPAQUE,
            Anonymous: D3D12_RAYTRACING_GEOMETRY_DESC_0 {
                AABBs: D3D12_RAYTRACING_GEOMETRY_AABBS_DESC {
                    AABBCount: 1,
                    AABBs: D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE {
                        StartAddress: va(&aabbs) + offset,
                        StrideInBytes: 24,
                    },
                },
            },
        };
        let geometry = [
            triangles(va(&vertices), Some(va(&index_buffer))),
            triangles(va(&far), None),
            boxes_at(0),
            boxes_at(32),
            triangles(va(&frame_vertices), None),
            triangles(va(&frame_vertices) + 128, None),
        ];
        let compactable = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_COMPACTION;
        let updatable = D3D12_RAYTRACING_ACCELERATION_STRUCTURE_BUILD_FLAG_ALLOW_UPDATE;
        let (triangle_inputs, far_inputs) = (
            bottom(&geometry[0], compactable),
            bottom(&geometry[1], compactable),
        );
        let (old_box_inputs, box_inputs) = (bottom(&geometry[2], updatable), bottom(&geometry[3], updatable));
        let (frame_far_inputs, frame_inputs) =
            (bottom(&geometry[4], updatable), bottom(&geometry[5], updatable));
        let triangle_structure = structure(&device5, &[&triangle_inputs, &far_inputs]);
        let box_structure = structure(&device5, &[&old_box_inputs, &box_inputs]);
        let frame_structure = structure(&device5, &[&frame_far_inputs, &frame_inputs]);

        // Built before the frame, from inputs then overwritten. The triangle's structure, and the box's from its old
        // box, and the size the triangle's compacts to.
        let sizes = default_buffer(device, 256, true);
        let list = direct_list(device);
        transitions(
            &list,
            &inputs[..3],
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        );
        transitions(
            &list,
            &[&sizes],
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        build(&list, &triangle_inputs, &triangle_structure);
        build(&list, &old_box_inputs, &box_structure);
        list.EmitRaytracingAccelerationStructurePostbuildInfo(
            &D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC {
                DestBuffer: va(&sizes),
                InfoType: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_COMPACTED_SIZE,
            },
            &[va(&triangle_structure.0)],
        );
        transitions(
            &list,
            &[&sizes],
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, first + 1);
        let compacted_bytes = u64::from_le_bytes(
            read_buffer(device, queue, fence, first + 2, &sizes, 8)[..8]
                .try_into()
                .unwrap(),
        );
        assert!(
            compacted_bytes > 0,
            "rays: the triangle's structure compacts to nothing"
        );
        let compacted = structure_buffer(
            device,
            compacted_bytes,
            D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE,
        );

        // Then, in one ExecuteCommandLists of three lists, the triangle's compacted, the box's updated to its own box,
        // the scene of the two built, and the triangle's own structure built again, of a triangle no ray reaches.
        // Locum's copies of the inputs read buffers these lists' own barriers have not moved (see `promoted` in
        // locum): the first list moves the box's after its update reads it; the second reads the far triangle where
        // the first left it, and the instances where nothing has moved them; the third moves both.
        let instances = [
            instance(INSTANCES[0], 0, va(&compacted)),
            instance(INSTANCES[1], 1, va(&box_structure.0)),
        ]
        .concat();
        let instance_buffer = default_buffer(device, 256, false);
        let instances_staged = upload(device, &instances, 256);
        fill(
            device,
            queue,
            fence,
            first + 3,
            &[(&instance_buffer, &instances_staged, 256)],
        );
        let scene_inputs = top(va(&instance_buffer), 2, D3D12_ELEMENTS_LAYOUT_ARRAY);
        let scene = structure(&device5, &[&scene_inputs]);
        let lists = [direct_list(device), direct_list(device), direct_list(device)];
        copy_structure(
            &lists[0],
            &compacted,
            &triangle_structure.0,
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_COMPACT,
        );
        update(&lists[0], &box_inputs, &box_structure);
        transitions(
            &lists[0],
            &[&aabbs],
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        transitions(
            &lists[0],
            &[&far],
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        );
        build(&lists[1], &scene_inputs, &scene);
        build(&lists[1], &far_inputs, &triangle_structure);
        transitions(
            &lists[2],
            &[&far, &instance_buffer],
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        for l in &lists {
            l.Close().unwrap();
        }
        queue.ExecuteCommandLists(&lists.each_ref().map(|l| Some(l.cast().unwrap())));
        finish(queue, fence, first + 4);
        let garbage = upload(device, &[0x7f; 256], 256);
        fill(
            device,
            queue,
            fence,
            first + 5,
            &[
                (inputs[0], &garbage, 256),
                (inputs[1], &garbage, 256),
                (inputs[2], &garbage, 256),
                (inputs[3], &garbage, 256),
                (&instance_buffer, &garbage, 256),
            ],
        );

        // The frame's scene: its instance given by an array of one pointer to it, and its clone.
        let frame_instances = upload(device, &instance(INSTANCES[2], 0, va(&frame_structure.0)), 256);
        let pointers = upload(device, &va(&frame_instances).to_le_bytes(), 256);
        let frame_scene_inputs = top(va(&pointers), 1, D3D12_ELEMENTS_LAYOUT_ARRAY_OF_POINTERS);
        let frame_scene = structure(&device5, &[&frame_scene_inputs]);
        let frame_clone = structure(&device5, &[&frame_scene_inputs]).0;

        // The outputs, their views (and the scene's) in a shader-visible heap: the first two, the scene, the third,
        // the fourth.
        let (width, height) = SIZE;
        let bytes = u64::from(width * height * 4);
        let outputs = [
            default_buffer(device, bytes, true),
            default_buffer(device, bytes, true),
            default_buffer(device, bytes, true),
            default_buffer(device, bytes, true),
        ];
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 5,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
            .unwrap();
        let stride = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
        let cpu = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: heap.GetCPUDescriptorHandleForHeapStart().ptr + (i * stride) as usize,
        };
        let gpu = |i: u32| heap.GetGPUDescriptorHandleForHeapStart().ptr + u64::from(i * stride);
        raw_uav(device, &outputs[0], bytes, cpu(0));
        raw_uav(device, &outputs[1], bytes, cpu(1));
        device.CreateShaderResourceView(
            None,
            Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_UNKNOWN,
                ViewDimension: D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    RaytracingAccelerationStructure: D3D12_RAYTRACING_ACCELERATION_STRUCTURE_SRV {
                        Location: scene.0.GetGPUVirtualAddress(),
                    },
                },
            }),
            cpu(2),
        );
        raw_uav(device, &outputs[2], bytes, cpu(3));
        raw_uav(device, &outputs[3], bytes, cpu(4));

        // The shader tables: the ray generation records in a default heap, the rest in upload heaps. The added
        // pipeline's tables take the miss and hit group identifiers its parent gave.
        let ids = [
            identifier(&pipeline, w!("Gen")),
            identifier(&grown, w!("Gen2")),
            identifier(&pipeline, w!("Miss")),
            identifier(&pipeline, w!("Triangles")),
            identifier(&pipeline, w!("Boxes")),
            identifier(&pipeline, w!("Called")),
        ];
        const RECORD: usize = 64;
        let gen_record = |id: &[u8], k: u32, scene: &ID3D12Resource, slot: u32| {
            record(
                id,
                &[
                    (32, &k.to_le_bytes()),
                    (40, &scene.GetGPUVirtualAddress().to_le_bytes()),
                    (48, &gpu(slot).to_le_bytes()),
                ],
                RECORD,
            )
        };
        let gen_records = [
            gen_record(&ids[0], K[0], &scene.0, 0),
            gen_record(&ids[1], K[1], &frame_scene.0, 1),
        ]
        .concat();
        let gen_table = default_buffer(device, 256, false);
        let gen_staged = upload(device, &gen_records, 256);
        fill(device, queue, fence, first + 6, &[(&gen_table, &gen_staged, 256)]);
        let miss_table = upload(device, &record(&ids[2], &[], 32), 256);
        let hit_table = upload(
            device,
            &[
                record(&ids[3], &[(32, &GROUPS[0].to_le_bytes())], RECORD),
                record(&ids[4], &[(32, &GROUPS[1].to_le_bytes())], RECORD),
            ]
            .concat(),
            256,
        );
        let call_table = upload(
            device,
            &record(&ids[5], &[(32, &CALL.to_le_bytes())], RECORD),
            256,
        );
        let dispatch = |offset: u64| D3D12_DISPATCH_RAYS_DESC {
            RayGenerationShaderRecord: D3D12_GPU_VIRTUAL_ADDRESS_RANGE {
                StartAddress: gen_table.GetGPUVirtualAddress() + offset,
                SizeInBytes: RECORD as u64,
            },
            MissShaderTable: D3D12_GPU_VIRTUAL_ADDRESS_RANGE_AND_STRIDE {
                StartAddress: miss_table.GetGPUVirtualAddress(),
                SizeInBytes: 32,
                StrideInBytes: 32,
            },
            HitGroupTable: D3D12_GPU_VIRTUAL_ADDRESS_RANGE_AND_STRIDE {
                StartAddress: hit_table.GetGPUVirtualAddress(),
                SizeInBytes: 2 * RECORD as u64,
                StrideInBytes: RECORD as u64,
            },
            CallableShaderTable: D3D12_GPU_VIRTUAL_ADDRESS_RANGE_AND_STRIDE {
                StartAddress: call_table.GetGPUVirtualAddress(),
                SizeInBytes: RECORD as u64,
                StrideInBytes: RECORD as u64,
            },
            Width: width,
            Height: height,
            Depth: 1,
        };
        // The inline trace: the scene's view and the third output in a table of the compute root signature.
        let ranges = [
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
            D3D12_DESCRIPTOR_RANGE {
                RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
                NumDescriptors: 1,
                BaseShaderRegister: 0,
                RegisterSpace: 0,
                OffsetInDescriptorsFromTableStart: D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND,
            },
        ];
        let query_root = root_signature(
            device,
            &[D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: 2,
                        pDescriptorRanges: ranges.as_ptr(),
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            }],
            D3D12_ROOT_SIGNATURE_FLAG_NONE,
        );
        // The second: the frame's scene's clone as a root SRV, the fourth output in a table.
        let uav_range = [ranges[1]];
        let root_query_root = root_signature(
            device,
            &[
                root_view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
                D3D12_ROOT_PARAMETER {
                    ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                    Anonymous: D3D12_ROOT_PARAMETER_0 {
                        DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                            NumDescriptorRanges: 1,
                            pDescriptorRanges: uav_range.as_ptr(),
                        },
                    },
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                },
            ],
            D3D12_ROOT_SIGNATURE_FLAG_NONE,
        );
        let query_code = dxil(QUERY, "cs_6_5")?;
        let mut query_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(query_root.clone())),
            CS: bytecode(&query_code),
            ..Default::default()
        };
        let query = device.CreateComputePipelineState::<ID3D12PipelineState>(&query_desc);
        ManuallyDrop::drop(&mut query_desc.pRootSignature);
        let query = query.unwrap_or_else(|e| panic!("rays: the inline trace's pipeline: {e}"));
        let mut root_query_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root_query_root.clone())),
            CS: bytecode(&query_code),
            ..Default::default()
        };
        let root_query = device.CreateComputePipelineState::<ID3D12PipelineState>(&root_query_desc);
        ManuallyDrop::drop(&mut root_query_desc.pRootSignature);
        let root_query =
            root_query.unwrap_or_else(|e| panic!("rays: the second inline trace's pipeline: {e}"));

        let label = format!("rays-{}", std::process::id());
        request(&label);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        let list = direct_list(device);
        let plain: ID3D12GraphicsCommandList = list.cast().unwrap();
        transition(
            &plain,
            &gen_table,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        );
        for o in &outputs {
            transition(
                &plain,
                o,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
        }
        list.SetDescriptorHeaps(&[Some(heap.clone())]);
        list.SetComputeRootSignature(&global);
        list.SetPipelineState1(&pipeline);
        list.DispatchRays(&dispatch(0));
        // The frame's scene, from triangles the frame writes: built, updated in place, and cloned.
        let frame_staged = upload(device, &frame_triangles, 256);
        list.CopyBufferRegion(&frame_vertices, 0, &frame_staged, 0, 256);
        transition(
            &plain,
            &frame_vertices,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        );
        build(&list, &frame_far_inputs, &frame_structure);
        update(&list, &frame_inputs, &frame_structure);
        build(&list, &frame_scene_inputs, &frame_scene);
        copy_structure(
            &list,
            &frame_clone,
            &frame_scene.0,
            D3D12_RAYTRACING_ACCELERATION_STRUCTURE_COPY_MODE_CLONE,
        );
        list.SetPipelineState1(&grown);
        list.DispatchRays(&dispatch(RECORD as u64));
        list.SetComputeRootSignature(&query_root);
        list.SetPipelineState(&query);
        list.SetComputeRootDescriptorTable(0, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: gpu(2) });
        list.Dispatch(1, 1, 1);
        list.SetComputeRootSignature(&root_query_root);
        list.SetPipelineState(&root_query);
        list.SetComputeRootShaderResourceView(0, va(&frame_clone));
        list.SetComputeRootDescriptorTable(1, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: gpu(4) });
        list.Dispatch(1, 1, 1);
        for o in &outputs {
            transition(
                &plain,
                o,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
        }
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, first + 7);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        // What the runtime says of the frame comes first: it says why a ray is wrong.
        debug_errors(device);

        for (n, output) in outputs.iter().enumerate() {
            // Each read waits for a value of its own: one the fence has reached waits for nothing.
            let out = read_buffer(device, queue, fence, first + 8 + n as u64, output, bytes);
            for y in 0..height {
                for x in 0..width {
                    let at = ((y * width + x) * 4) as usize;
                    let word = u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
                    let want = if n < 2 {
                        traced(n, x, y)
                    } else {
                        queried(n - 2, x, y)
                    };
                    assert_eq!(
                        word, want,
                        "rays: output {n}, ray ({x}, {y}): {word:#x}, not {want:#x}"
                    );
                }
            }
        }

        let db = written(&label);
        let rays = "d.call = 'DispatchRays'";
        let width_of = |r: &ID3D12Resource| r.GetDesc().Width;
        check(
            &db,
            &[
                ("SELECT count(*) FROM gaps WHERE what = 'd3d12-shader-tables'", 0),
                // The frame's instance, named only by its pointer, is read by the build that points at it.
                (
                    "SELECT count(*) FROM gaps WHERE what = 'd3d12-acceleration-structures'",
                    0,
                ),
                (
                    "SELECT count(*) FROM gaps WHERE what = 'd3d12-indirect-arguments'",
                    0,
                ),
                // Each DispatchRays writes the output its ray generation record's table names, and reads the scene its
                // root SRV names: nothing else names them.
                (
                    &format!(
                        "SELECT count(DISTINCT e.event) FROM event_resources e JOIN draws d ON d.event = e.event \
                         JOIN resources r ON r.id = e.resource WHERE {rays} AND e.access = 'write' AND r.width = {}",
                        width_of(&outputs[0])
                    ),
                    2,
                ),
                (
                    &format!(
                        "SELECT count(DISTINCT e.resource) FROM event_resources e JOIN draws d ON d.event = e.event \
                         JOIN resources r ON r.id = e.resource WHERE {rays} AND e.access = 'write' AND r.width = {}",
                        width_of(&outputs[0])
                    ),
                    2,
                ),
                // The scenes: the buffers made in the acceleration structure state.
                (
                    &format!(
                        "SELECT count(DISTINCT e.resource) FROM event_resources e JOIN draws d ON d.event = e.event \
                         JOIN d12_objects o ON o.id = e.resource WHERE {rays} AND e.access = 'read' AND \
                         json_extract(o.description, '$.initial_state') = {}",
                        D3D12_RESOURCE_STATE_RAYTRACING_ACCELERATION_STRUCTURE.0
                    ),
                    2,
                ),
                // The four structures built before the frame, each with its inputs: the triangle's (built again), its
                // compaction, the box's (updated) and the scene.
                (
                    "SELECT count(*) FROM d12_acceleration_structures WHERE inputs_json IS NOT NULL",
                    4,
                ),
            ],
        );
        finish(queue, fence, first + 12);
        debug_errors(device);
        println!(
            "D3D12 raytracing: two DispatchRays (a collection, AddToStateObject, local root arguments, a callable \
             shader) and two inline traces, through structures compacted, updated, cloned and of instances by \
             pointer, every ray as expected; their tables' outputs and scenes found: {label}"
        );
        Ok(())
    }
}
