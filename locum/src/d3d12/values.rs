//! Portable values copied while the application's call arguments are alive.
//! Unions are read only through the API's discriminator; padding and pointers are never serialized.
pub use super::id;
pub use super::journal::{address, descriptor, render_targets, slice};
use core::ffi::c_void;
use serde_json::{Value, json};
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{IUnknown, Interface};

unsafe fn output<T: Interface>(out: *mut *mut c_void) -> Option<T> {
    if out.is_null() {
        return None;
    }
    let pointer = super::unwrap(unsafe { *out });
    unsafe { IUnknown::from_raw_borrowed(&pointer) }?.cast().ok()
}

pub unsafe fn created_queue(device: u64, out: *mut *mut c_void, desc: *const D3D12_COMMAND_QUEUE_DESC) {
    if out.is_null() {
        return;
    }
    if let Some(d) = unsafe { desc.as_ref() } {
        super::journal::describe(
            id(unsafe { *out }),
            device,
            json!({"type":d.Type.0,"priority":d.Priority,"flags":d.Flags.0,"node_mask":d.NodeMask}),
        );
    }
}

/// A command signature: its stride, root signature and arguments (type, and each type's parameters), which say
/// whether ExecuteIndirect draws or dispatches with it.
pub unsafe fn created_command_signature(
    device: u64,
    out: *mut *mut c_void,
    desc: *const D3D12_COMMAND_SIGNATURE_DESC,
    root: *mut c_void,
) {
    let (Some(d), false) = (unsafe { desc.as_ref() }, out.is_null()) else {
        return;
    };
    let arguments: Vec<Value> = unsafe { slice(d.pArgumentDescs, d.NumArgumentDescs as usize) }
        .iter()
        .map(|a| {
            let mut v = json!({"type":a.Type.0});
            unsafe {
                match a.Type {
                    D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW => {
                        v["slot"] = json!(a.Anonymous.VertexBuffer.Slot);
                    }
                    D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT => {
                        let c = a.Anonymous.Constant;
                        v["root_parameter"] = json!(c.RootParameterIndex);
                        v["offset"] = json!(c.DestOffsetIn32BitValues);
                        v["count"] = json!(c.Num32BitValuesToSet);
                    }
                    // One constant, the command's index: its description has no count.
                    D3D12_INDIRECT_ARGUMENT_TYPE_INCREMENTING_CONSTANT => {
                        let c = a.Anonymous.IncrementingConstant;
                        v["root_parameter"] = json!(c.RootParameterIndex);
                        v["offset"] = json!(c.DestOffsetIn32BitValues);
                    }
                    D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW => {
                        v["root_parameter"] = json!(a.Anonymous.ConstantBufferView.RootParameterIndex);
                    }
                    D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW => {
                        v["root_parameter"] = json!(a.Anonymous.ShaderResourceView.RootParameterIndex);
                    }
                    D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW => {
                        v["root_parameter"] = json!(a.Anonymous.UnorderedAccessView.RootParameterIndex);
                    }
                    _ => {}
                }
            }
            v
        })
        .collect();
    let description =
        json!({"stride":d.ByteStride,"node_mask":d.NodeMask,"root_signature":id(root),"arguments":arguments});
    super::passes::command_signature(id(unsafe { *out }), &description);
    super::outputs::signature(id(unsafe { *out }), &description);
    super::journal::describe(id(unsafe { *out }), device, description);
}

/// How a resource begins: in a legacy resource state, or (created through the enhanced-barrier creators) in a layout.
pub enum Initial {
    State(i32),
    Layout(i32),
}
/// How a resource was made: on a heap of its own, at an offset in a heap, or reserved (tiles mapped later).
pub enum Placement {
    Committed,
    Placed(u64, u64),
    Reserved,
}

pub unsafe fn created_resource(device: u64, out: *mut *mut c_void, initial: Initial, placement: Placement) {
    let Some(resource) = (unsafe { output::<ID3D12Resource>(out) }) else {
        return;
    };
    let id = id(unsafe { *out });
    let d = unsafe { resource.GetDesc() };
    let mut properties = D3D12_HEAP_PROPERTIES::default();
    let mut flags = D3D12_HEAP_FLAGS::default();
    let (heap, offset) = match placement {
        Placement::Placed(heap, offset) => (heap, offset),
        _ => (0, 0),
    };
    // A reserved resource has no heap of its own; the runtime refuses to be asked.
    let heap_properties = (!matches!(placement, Placement::Reserved))
        .then(|| unsafe { resource.GetHeapProperties(Some(&mut properties), Some(&mut flags)) }.ok())
        .flatten()
        .map(|()|
        json!({"type":properties.Type.0,"cpu_page":properties.CPUPageProperty.0,"memory_pool":properties.MemoryPoolPreference.0,
            "creation_node_mask":properties.CreationNodeMask,"visible_node_mask":properties.VisibleNodeMask,"flags":flags.0}));
    let description = json!({"dimension":d.Dimension.0,"alignment":d.Alignment,"width":d.Width,"height":d.Height,
        "depth_or_array_size":d.DepthOrArraySize,"mips":d.MipLevels,"format":d.Format.0,"sample_count":d.SampleDesc.Count,
        "sample_quality":d.SampleDesc.Quality,"layout":d.Layout.0,"flags":d.Flags.0,
        "heap":heap,"heap_offset":offset,"heap_properties":heap_properties});
    let mut description = description;
    // What it takes of its heap from its offset: replay places it there only where it takes no more.
    if heap != 0 {
        let mut parent: Option<ID3D12Device> = None;
        if unsafe { resource.GetDevice(&mut parent) }.is_ok()
            && let Some(parent) = parent
        {
            let info = unsafe { parent.GetResourceAllocationInfo(0, &[d]) };
            if info.SizeInBytes != u64::MAX {
                description["bytes"] = json!(info.SizeInBytes);
            }
        }
    }
    let initial = match initial {
        Initial::State(s) => {
            description["initial_state"] = json!(s);
            super::states::Access::State(s)
        }
        Initial::Layout(l) => {
            description["initial_layout"] = json!(l);
            super::passes::created_in_layout(id);
            super::states::Access::Layout(l)
        }
    };
    if d.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
        // Kept in the description too: ExecuteIndirect's argument records hold the game's addresses, which replay,
        // its buffers elsewhere, maps back to (buffer, offset) by these.
        let address = unsafe { resource.GetGPUVirtualAddress() };
        description["gpu_address"] = json!(address);
        super::journal::resource_address(id, address, d.Width);
    }
    super::journal::track(id, &description, initial);
    super::journal::describe(id, device, description);
}

/// A heap's description: placed resources and reserved resources' tiles name it.
pub unsafe fn created_heap(device: u64, out: *mut *mut c_void) {
    let Some(heap) = (unsafe { output::<ID3D12Heap>(out) }) else {
        return;
    };
    let id = id(unsafe { *out });
    let d = unsafe { heap.GetDesc() };
    let p = d.Properties;
    super::journal::describe(
        id,
        device,
        json!({"bytes":d.SizeInBytes,"alignment":d.Alignment,"flags":d.Flags.0,
            "heap_properties":{"type":p.Type.0,"cpu_page":p.CPUPageProperty.0,"memory_pool":p.MemoryPoolPreference.0,
                "creation_node_mask":p.CreationNodeMask,"visible_node_mask":p.VisibleNodeMask}}),
    );
}

pub unsafe fn created_descriptor_heap(device: u64, out: *mut *mut c_void) {
    let Some(heap) = (unsafe { output::<ID3D12DescriptorHeap>(out) }) else {
        return;
    };
    let id = id(unsafe { *out });
    let d = unsafe { heap.GetDesc() };
    let mut dev = None;
    unsafe { heap.GetDevice::<ID3D12Device>(&mut dev) }.expect("descriptor heap device");
    let stride = unsafe { dev.unwrap().GetDescriptorHandleIncrementSize(d.Type) };
    let cpu = unsafe { heap.GetCPUDescriptorHandleForHeapStart() }.ptr as u64;
    let gpu = if d.Flags.contains(D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE) {
        unsafe { heap.GetGPUDescriptorHandleForHeapStart() }.ptr
    } else {
        0
    };
    super::journal::descriptor_addresses(id, cpu, gpu, d.NumDescriptors, stride);
    super::journal::describe(
        id,
        device,
        json!({"type":d.Type.0,"count":d.NumDescriptors,"flags":d.Flags.0,"node_mask":d.NodeMask}),
    );
}

pub unsafe fn rects(p: *const RECT, n: u32) -> Value {
    json!(
        unsafe { slice(p, n as usize) }
            .iter()
            .map(|r| [r.left, r.top, r.right, r.bottom])
            .collect::<Vec<_>>()
    )
}
pub unsafe fn viewports(p: *const D3D12_VIEWPORT, n: u32) -> Value {
    json!(
        unsafe { slice(p, n as usize) }
            .iter()
            .map(|v| [v.TopLeftX, v.TopLeftY, v.Width, v.Height, v.MinDepth, v.MaxDepth])
            .collect::<Vec<_>>()
    )
}
pub unsafe fn index_buffer(p: *const D3D12_INDEX_BUFFER_VIEW) -> Value {
    unsafe { p.as_ref() }.map_or(
        Value::Null,
        |v| json!({"buffer":address(v.BufferLocation),"bytes":v.SizeInBytes,"format":v.Format.0}),
    )
}
pub unsafe fn vertex_buffers(p: *const D3D12_VERTEX_BUFFER_VIEW, n: u32) -> Value {
    json!(
        unsafe { slice(p, n as usize) }
            .iter()
            .map(
                |v| json!({"buffer":address(v.BufferLocation),"bytes":v.SizeInBytes,"stride":v.StrideInBytes})
            )
            .collect::<Vec<_>>()
    )
}
pub unsafe fn stream_outputs(p: *const D3D12_STREAM_OUTPUT_BUFFER_VIEW, n: u32) -> Value {
    json!(unsafe { slice(p,n as usize) }.iter().map(|v|json!({"buffer":address(v.BufferLocation),"bytes":v.SizeInBytes,"filled_size":address(v.BufferFilledSizeLocation)})).collect::<Vec<_>>())
}
pub unsafe fn box_value(p: *const D3D12_BOX) -> Value {
    unsafe { p.as_ref() }.map_or(Value::Null, |b| {
        json!([b.left, b.top, b.front, b.right, b.bottom, b.back])
    })
}
pub unsafe fn texture_copy(p: *const D3D12_TEXTURE_COPY_LOCATION) -> Value {
    let Some(v) = (unsafe { p.as_ref() }) else {
        return Value::Null;
    };
    let resource = v.pResource.as_ref().map_or(0, |r| id(r.as_raw()));
    if v.Type == D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX {
        json!({"resource":resource,"subresource":unsafe { v.Anonymous.SubresourceIndex }})
    } else if v.Type == D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT {
        let f = unsafe { v.Anonymous.PlacedFootprint };
        json!({"resource":resource,"footprint":{"offset":f.Offset,"format":f.Footprint.Format.0,
            "width":f.Footprint.Width,"height":f.Footprint.Height,"depth":f.Footprint.Depth,"row_pitch":f.Footprint.RowPitch}})
    } else {
        json!({"resource":resource,"type":v.Type.0,"diagnostic":"invalid texture copy type"})
    }
}
pub unsafe fn barriers(p: *const D3D12_RESOURCE_BARRIER, n: u32) -> Value {
    json!(unsafe { slice(p,n as usize) }.iter().map(|v| {
        let payload = match v.Type {
            D3D12_RESOURCE_BARRIER_TYPE_TRANSITION => {
                let t = unsafe { &v.Anonymous.Transition };
                json!({"resource":t.pResource.as_ref().map_or(0,|r|id(r.as_raw())),"subresource":t.Subresource,"before":t.StateBefore.0,"after":t.StateAfter.0})
            }
            D3D12_RESOURCE_BARRIER_TYPE_UAV => json!({"resource":unsafe { &v.Anonymous.UAV }.pResource.as_ref().map_or(0,|r|id(r.as_raw()))}),
            D3D12_RESOURCE_BARRIER_TYPE_ALIASING => {
                let a = unsafe { &v.Anonymous.Aliasing };
                json!({"before":a.pResourceBefore.as_ref().map_or(0,|r|id(r.as_raw())),"after":a.pResourceAfter.as_ref().map_or(0,|r|id(r.as_raw()))})
            }
            _ => json!({"diagnostic":"invalid barrier type"}),
        };
        json!({"type":v.Type.0,"flags":v.Flags.0,"payload":payload})
    }).collect::<Vec<_>>())
}
pub unsafe fn write_immediate(
    p: *const D3D12_WRITEBUFFERIMMEDIATE_PARAMETER,
    modes: *const D3D12_WRITEBUFFERIMMEDIATE_MODE,
    n: u32,
) -> Value {
    // A null mode array means DEFAULT for every write.
    let modes = (!modes.is_null()).then(|| unsafe { slice(modes, n as usize) });
    json!(
        unsafe { slice(p, n as usize) }
            .iter()
            .enumerate()
            .map(
                |(i, w)| json!({"address":address(w.Dest),"value":w.Value,"mode":modes.map_or(0, |m| m[i].0)})
            )
            .collect::<Vec<_>>()
    )
}
pub unsafe fn discard_region(p: *const D3D12_DISCARD_REGION) -> Value {
    unsafe { p.as_ref() }.map_or(Value::Null, |r| {
        json!({"rects":unsafe { rects(r.pRects, r.NumRects) },"first_subresource":r.FirstSubresource,
            "subresources":r.NumSubresources})
    })
}
pub unsafe fn tile_coordinate(p: *const D3D12_TILED_RESOURCE_COORDINATE) -> Value {
    unsafe { p.as_ref() }.map_or(
        Value::Null,
        |c| json!({"x":c.X,"y":c.Y,"z":c.Z,"subresource":c.Subresource}),
    )
}
pub unsafe fn tile_region(p: *const D3D12_TILE_REGION_SIZE) -> Value {
    unsafe { p.as_ref() }.map_or(Value::Null, |r| {
        json!({"tiles":r.NumTiles,"use_box":r.UseBox.as_bool(),"width":r.Width,"height":r.Height,"depth":r.Depth})
    })
}
pub unsafe fn dependent_resources(
    resources: *const *mut c_void,
    ranges: *const D3D12_SUBRESOURCE_RANGE_UINT64,
    n: u32,
) -> Value {
    let resources = unsafe { slice(resources, n as usize) };
    let ranges = unsafe { slice(ranges, n as usize) };
    json!(resources
        .iter()
        .zip(ranges)
        .map(|(r, s)| json!({"resource":id(*r),"subresource":s.Subresource,"begin":s.Range.Begin,"end":s.Range.End}))
        .collect::<Vec<_>>())
}
pub unsafe fn sample_positions(p: *const D3D12_SAMPLE_POSITION, samples: u32, pixels: u32) -> Value {
    json!(
        unsafe { slice(p, (samples * pixels) as usize) }
            .iter()
            .map(|s| [s.X, s.Y])
            .collect::<Vec<_>>()
    )
}
unsafe fn clear_value(v: &D3D12_CLEAR_VALUE, depth: bool) -> Value {
    if depth {
        let d = unsafe { v.Anonymous.DepthStencil };
        json!({"format":v.Format.0,"depth":d.Depth,"stencil":d.Stencil})
    } else {
        json!({"format":v.Format.0,"color":unsafe { v.Anonymous.Color }})
    }
}
unsafe fn beginning(b: &D3D12_RENDER_PASS_BEGINNING_ACCESS, depth: bool) -> Value {
    if b.Type == D3D12_RENDER_PASS_BEGINNING_ACCESS_TYPE_CLEAR {
        json!({"type":b.Type.0,"clear":unsafe { clear_value(&b.Anonymous.Clear.ClearValue, depth) }})
    } else {
        json!({"type":b.Type.0})
    }
}
unsafe fn ending_access(e: &D3D12_RENDER_PASS_ENDING_ACCESS) -> Value {
    if e.Type != D3D12_RENDER_PASS_ENDING_ACCESS_TYPE_RESOLVE {
        return json!({"type":e.Type.0});
    }
    let r = unsafe { &*e.Anonymous.Resolve };
    let resource = |r: &Option<ID3D12Resource>| r.as_ref().map_or(0, |r| id(r.as_raw()));
    let subresources: Vec<_> = unsafe { slice(r.pSubresourceParameters, r.SubresourceCount as usize) }
        .iter()
        .map(|s| {
            json!({"src":s.SrcSubresource,"dst":s.DstSubresource,"x":s.DstX,"y":s.DstY,
                "rect":[s.SrcRect.left,s.SrcRect.top,s.SrcRect.right,s.SrcRect.bottom]})
        })
        .collect();
    json!({"type":e.Type.0,"resolve":{"src":resource(&r.pSrcResource),"dst":resource(&r.pDstResource),
        "subresources":subresources,"format":r.Format.0,"mode":r.ResolveMode.0,"preserve_source":r.PreserveResolveSource.as_bool()}})
}
pub unsafe fn render_pass_targets(p: *const D3D12_RENDER_PASS_RENDER_TARGET_DESC, n: u32) -> Value {
    json!(unsafe { slice(p, n as usize) }
        .iter()
        .map(|t| {
            json!({"view":descriptor(t.cpuDescriptor.ptr as u64,false),
                "begin":unsafe { beginning(&t.BeginningAccess, false) },"end":unsafe { ending_access(&t.EndingAccess) }})
        })
        .collect::<Vec<_>>())
}
pub unsafe fn render_pass_depth(p: *const D3D12_RENDER_PASS_DEPTH_STENCIL_DESC) -> Value {
    unsafe { p.as_ref() }.map_or(Value::Null, |d| {
        json!({"view":descriptor(d.cpuDescriptor.ptr as u64,false),
            "depth_begin":unsafe { beginning(&d.DepthBeginningAccess, true) },
            "stencil_begin":unsafe { beginning(&d.StencilBeginningAccess, true) },
            "depth_end":unsafe { ending_access(&d.DepthEndingAccess) },
            "stencil_end":unsafe { ending_access(&d.StencilEndingAccess) }})
    })
}
fn strided(v: &D3D12_GPU_VIRTUAL_ADDRESS_AND_STRIDE) -> Value {
    json!({"address":address(v.StartAddress),"stride":v.StrideInBytes})
}
unsafe fn geometry(g: &D3D12_RAYTRACING_GEOMETRY_DESC) -> Value {
    let shape = if g.Type == D3D12_RAYTRACING_GEOMETRY_TYPE_TRIANGLES {
        let t = unsafe { &g.Anonymous.Triangles };
        json!({"transform":address(t.Transform3x4),"index_format":t.IndexFormat.0,"vertex_format":t.VertexFormat.0,
            "indices":t.IndexCount,"vertices":t.VertexCount,"index_buffer":address(t.IndexBuffer),
            "vertex_buffer":strided(&t.VertexBuffer)})
    } else if g.Type == D3D12_RAYTRACING_GEOMETRY_TYPE_PROCEDURAL_PRIMITIVE_AABBS {
        let a = unsafe { &g.Anonymous.AABBs };
        json!({"aabbs":a.AABBCount,"buffer":strided(&a.AABBs)})
    } else {
        json!({"diagnostic":"geometry type not represented"})
    };
    json!({"type":g.Type.0,"flags":g.Flags.0,"shape":shape})
}
/// A build's inputs: a top-level build's instance array address, or a bottom-level build's geometry, read from
/// either layout.
pub unsafe fn acceleration_structure_inputs(
    i: &D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_INPUTS,
) -> Value {
    let n = i.NumDescs as usize;
    let content = if i.Type == D3D12_RAYTRACING_ACCELERATION_STRUCTURE_TYPE_TOP_LEVEL {
        json!({"instances":address(unsafe { i.Anonymous.InstanceDescs })})
    } else if i.DescsLayout == D3D12_ELEMENTS_LAYOUT_ARRAY {
        let g = unsafe { slice(i.Anonymous.pGeometryDescs, n) };
        json!({"geometry":g.iter().map(|g| unsafe { geometry(g) }).collect::<Vec<_>>()})
    } else {
        let g = unsafe { slice(i.Anonymous.ppGeometryDescs, n) };
        json!({"geometry":g.iter().map(|g| unsafe { g.as_ref() }.map_or(Value::Null, |g| unsafe { geometry(g) })).collect::<Vec<_>>()})
    };
    json!({"type":i.Type.0,"flags":i.Flags.0,"count":i.NumDescs,"layout":i.DescsLayout.0,"content":content})
}
pub unsafe fn acceleration_structure_build(
    p: *const D3D12_BUILD_RAYTRACING_ACCELERATION_STRUCTURE_DESC,
) -> Value {
    unsafe { p.as_ref() }.map_or(Value::Null, |d| {
        json!({"dest":address(d.DestAccelerationStructureData),"source":address(d.SourceAccelerationStructureData),
            "scratch":address(d.ScratchAccelerationStructureData),"inputs":unsafe { acceleration_structure_inputs(&d.Inputs) }})
    })
}
pub unsafe fn postbuild_info(
    p: *const D3D12_RAYTRACING_ACCELERATION_STRUCTURE_POSTBUILD_INFO_DESC,
    n: u32,
) -> Value {
    json!(
        unsafe { slice(p, n as usize) }
            .iter()
            .map(|d| json!({"dest":address(d.DestBuffer),"info":d.InfoType.0}))
            .collect::<Vec<_>>()
    )
}
pub unsafe fn addresses(p: *const u64, n: u32) -> Value {
    json!(
        unsafe { slice(p, n as usize) }
            .iter()
            .map(|&a| address(a))
            .collect::<Vec<_>>()
    )
}
pub unsafe fn dispatch_rays(p: *const D3D12_DISPATCH_RAYS_DESC) -> Value {
    let table = |t: &D3D12_GPU_VIRTUAL_ADDRESS_RANGE_AND_STRIDE| json!({"address":address(t.StartAddress),"bytes":t.SizeInBytes,"stride":t.StrideInBytes});
    unsafe { p.as_ref() }.map_or(Value::Null, |d| {
        let r = &d.RayGenerationShaderRecord;
        json!({"ray_generation":{"address":address(r.StartAddress),"bytes":r.SizeInBytes},
            "miss":table(&d.MissShaderTable),"hit_groups":table(&d.HitGroupTable),"callable":table(&d.CallableShaderTable),
            "size":[d.Width,d.Height,d.Depth]})
    })
}
/// Enhanced barriers, by group: global, texture (with subresource range and layouts) or buffer (with range).
pub unsafe fn barrier_groups(p: *const D3D12_BARRIER_GROUP, n: u32) -> Value {
    json!(unsafe { slice(p, n as usize) }
        .iter()
        .map(|g| {
            let count = g.NumBarriers as usize;
            let barriers: Vec<Value> = match g.Type {
                D3D12_BARRIER_TYPE_GLOBAL => unsafe { slice(g.Anonymous.pGlobalBarriers, count) }
                    .iter()
                    .map(|b| json!({"sync":[b.SyncBefore.0,b.SyncAfter.0],"access":[b.AccessBefore.0,b.AccessAfter.0]}))
                    .collect(),
                D3D12_BARRIER_TYPE_TEXTURE => unsafe { slice(g.Anonymous.pTextureBarriers, count) }
                    .iter()
                    .map(|b| {
                        let s = &b.Subresources;
                        json!({"resource":b.pResource.as_ref().map_or(0, |r| id(r.as_raw())),
                            "sync":[b.SyncBefore.0,b.SyncAfter.0],"access":[b.AccessBefore.0,b.AccessAfter.0],
                            "layout":[b.LayoutBefore.0,b.LayoutAfter.0],
                            "subresources":{"index_or_first_mip":s.IndexOrFirstMipLevel,"mips":s.NumMipLevels,
                                "first_slice":s.FirstArraySlice,"slices":s.NumArraySlices,"first_plane":s.FirstPlane,
                                "planes":s.NumPlanes},"flags":b.Flags.0})
                    })
                    .collect(),
                D3D12_BARRIER_TYPE_BUFFER => unsafe { slice(g.Anonymous.pBufferBarriers, count) }
                    .iter()
                    .map(|b| {
                        json!({"resource":b.pResource.as_ref().map_or(0, |r| id(r.as_raw())),
                            "sync":[b.SyncBefore.0,b.SyncAfter.0],"access":[b.AccessBefore.0,b.AccessAfter.0],
                            "offset":b.Offset,"size":b.Size})
                    })
                    .collect(),
                _ => vec![json!({"diagnostic":"invalid barrier group type"})],
            };
            json!({"type":g.Type.0,"barriers":barriers})
        })
        .collect::<Vec<_>>())
}
pub unsafe fn set_program(p: *const D3D12_SET_PROGRAM_DESC) -> Value {
    unsafe { p.as_ref() }.map_or(Value::Null, |d| unsafe {
        match d.Type {
            D3D12_PROGRAM_TYPE_GENERIC_PIPELINE => {
                json!({"type":d.Type.0,"program":d.Anonymous.GenericPipeline.ProgramIdentifier.OpaqueData})
            }
            D3D12_PROGRAM_TYPE_RAYTRACING_PIPELINE => {
                json!({"type":d.Type.0,"program":d.Anonymous.RaytracingPipeline.ProgramIdentifier.OpaqueData})
            }
            D3D12_PROGRAM_TYPE_WORK_GRAPH => {
                let w = &d.Anonymous.WorkGraph;
                let t = &w.NodeLocalRootArgumentsTable;
                json!({"type":d.Type.0,"program":w.ProgramIdentifier.OpaqueData,"flags":w.Flags.0,
                    "backing":{"address":address(w.BackingMemory.StartAddress),"bytes":w.BackingMemory.SizeInBytes},
                    "local_root_arguments":{"address":address(t.StartAddress),"bytes":t.SizeInBytes,"stride":t.StrideInBytes}})
            }
            _ => json!({"type":d.Type.0,"diagnostic":"program type not represented"}),
        }
    })
}
unsafe fn node_cpu_input(n: &D3D12_NODE_CPU_INPUT) -> Value {
    let bytes = (n.NumRecords as usize).saturating_mul(n.RecordStrideInBytes as usize);
    json!({"entrypoint":n.EntrypointIndex,"records":n.NumRecords,"stride":n.RecordStrideInBytes,
        "data":unsafe { slice(n.pRecords.cast::<u8>(), bytes) }})
}
/// A graph dispatch's inputs: CPU records are copied as bytes, GPU inputs are addresses.
pub unsafe fn dispatch_graph(p: *const D3D12_DISPATCH_GRAPH_DESC) -> Value {
    unsafe { p.as_ref() }.map_or(Value::Null, |d| unsafe {
        let input = match d.Mode {
            D3D12_DISPATCH_MODE_NODE_CPU_INPUT => node_cpu_input(&d.Anonymous.NodeCPUInput),
            D3D12_DISPATCH_MODE_NODE_GPU_INPUT => json!({"address":address(d.Anonymous.NodeGPUInput)}),
            D3D12_DISPATCH_MODE_MULTI_NODE_CPU_INPUT => {
                let m = &d.Anonymous.MultiNodeCPUInput;
                let base = m.pNodeInputs.cast::<u8>();
                let nodes: Vec<Value> = (0..m.NumNodeInputs as usize)
                    .filter_map(|i| {
                        base.add(i * m.NodeInputStrideInBytes as usize)
                            .cast::<D3D12_NODE_CPU_INPUT>()
                            .as_ref()
                    })
                    .map(|n| node_cpu_input(n))
                    .collect();
                json!({"nodes":nodes})
            }
            D3D12_DISPATCH_MODE_MULTI_NODE_GPU_INPUT => {
                json!({"address":address(d.Anonymous.MultiNodeGPUInput)})
            }
            _ => json!({"diagnostic":"dispatch mode not represented"}),
        };
        json!({"mode":d.Mode.0,"input":input})
    })
}
