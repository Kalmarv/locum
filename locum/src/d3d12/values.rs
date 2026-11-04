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

pub unsafe fn created_resource(device: u64, out: *mut *mut c_void, state: i32, heap: u64, offset: u64) {
    let Some(resource) = (unsafe { output::<ID3D12Resource>(out) }) else {
        return;
    };
    let id = id(unsafe { *out });
    let d = unsafe { resource.GetDesc() };
    let mut properties = D3D12_HEAP_PROPERTIES::default();
    let mut flags = D3D12_HEAP_FLAGS::default();
    let heap_properties = unsafe { resource.GetHeapProperties(Some(&mut properties),Some(&mut flags)) }.ok().map(|()|
        json!({"type":properties.Type.0,"cpu_page":properties.CPUPageProperty.0,"memory_pool":properties.MemoryPoolPreference.0,
            "creation_node_mask":properties.CreationNodeMask,"visible_node_mask":properties.VisibleNodeMask,"flags":flags.0}));
    let description = json!({"dimension":d.Dimension.0,"alignment":d.Alignment,"width":d.Width,"height":d.Height,
        "depth_or_array_size":d.DepthOrArraySize,"mips":d.MipLevels,"format":d.Format.0,"sample_count":d.SampleDesc.Count,
        "sample_quality":d.SampleDesc.Quality,"layout":d.Layout.0,"flags":d.Flags.0,"initial_state":state,
        "heap":heap,"heap_offset":offset,"heap_properties":heap_properties});
    if d.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
        super::journal::resource_address(id, unsafe { resource.GetGPUVirtualAddress() }, d.Width);
    }
    super::journal::describe(id, device, description);
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
