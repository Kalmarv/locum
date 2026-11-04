//! Descriptor definitions are immutable versions. CPU-consumed descriptors are copied into commands;
//! GPU tables resolve against the versions visible to each queue submission.
use super::{id, journal, portable::Portable, values::slice};
use core::ffi::c_void;
use serde_json::{Value, json};
use windows::Win32::Graphics::Direct3D12::*;
/// Owned pointer-free native inputs. Common descriptor copies clone these values,
/// not a JSON tree or an atomic reference count shared by all render workers.
#[derive(Clone)]
pub enum Description {
    Srv(D3D12_SHADER_RESOURCE_VIEW_DESC),
    Uav(D3D12_UNORDERED_ACCESS_VIEW_DESC),
    Rtv(D3D12_RENDER_TARGET_VIEW_DESC),
    Dsv(D3D12_DEPTH_STENCIL_VIEW_DESC),
    Sampler(D3D12_SAMPLER_DESC),
    Cb { resource: u64, offset: u64, bytes: u32 },
    DefaultView,
    Null,
    // Only ambiguous addresses and DXR AS descriptors need a resolved variable-size value.
    Json(Value),
}
impl Description {
    pub fn json(&self) -> Value {
        match self {
            Self::Srv(d) => srv_json(d),
            Self::Uav(d) => uav_json(d),
            Self::Rtv(d) => rtv_json(d),
            Self::Dsv(d) => dsv_json(d),
            Self::Sampler(d) => d.portable(),
            Self::Cb {
                resource,
                offset,
                bytes,
            } => json!({"buffer":{"resource":resource,"offset":offset},"bytes":bytes}),
            Self::DefaultView => json!({"default":true}),
            Self::Null => Value::Null,
            Self::Json(v) => v.clone(),
        }
    }
}
pub unsafe fn srv(
    resource: *mut c_void,
    p: *const D3D12_SHADER_RESOURCE_VIEW_DESC,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    if !journal::enabled() {
        return;
    }
    let desc = if let Some(d) = unsafe { p.as_ref() } {
        if d.ViewDimension == D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE {
            Description::Json(srv_json(d))
        } else {
            Description::Srv(*d)
        }
    } else {
        Description::DefaultView
    };
    journal::set_descriptor(handle.ptr as u64, "srv", id(resource), 0, desc);
}
pub unsafe fn uav(
    resource: *mut c_void,
    counter: *mut c_void,
    p: *const D3D12_UNORDERED_ACCESS_VIEW_DESC,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    if !journal::enabled() {
        return;
    }
    let desc = if let Some(d) = unsafe { p.as_ref() } {
        Description::Uav(*d)
    } else {
        Description::DefaultView
    };
    journal::set_descriptor(handle.ptr as u64, "uav", id(resource), id(counter), desc);
}
pub unsafe fn rtv(
    resource: *mut c_void,
    p: *const D3D12_RENDER_TARGET_VIEW_DESC,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    if !journal::enabled() {
        return;
    }
    let desc = if let Some(d) = unsafe { p.as_ref() } {
        Description::Rtv(*d)
    } else {
        Description::DefaultView
    };
    journal::set_descriptor(handle.ptr as u64, "rtv", id(resource), 0, desc);
}
pub unsafe fn dsv(
    resource: *mut c_void,
    p: *const D3D12_DEPTH_STENCIL_VIEW_DESC,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    if !journal::enabled() {
        return;
    }
    let desc = if let Some(d) = unsafe { p.as_ref() } {
        Description::Dsv(*d)
    } else {
        Description::DefaultView
    };
    journal::set_descriptor(handle.ptr as u64, "dsv", id(resource), 0, desc);
}

pub unsafe fn cb(p: *const D3D12_CONSTANT_BUFFER_VIEW_DESC, handle: D3D12_CPU_DESCRIPTOR_HANDLE) {
    if !journal::enabled() {
        return;
    }
    let desc = unsafe { p.as_ref() }.map_or(Description::Null, |d| {
        journal::constant_buffer(d.BufferLocation, d.SizeInBytes)
    });
    let resource = match &desc {
        Description::Cb { resource, .. } => *resource,
        _ => 0,
    };
    journal::set_descriptor(handle.ptr as u64, "cb", resource, 0, desc);
}
pub unsafe fn sampler(p: *const D3D12_SAMPLER_DESC, handle: D3D12_CPU_DESCRIPTOR_HANDLE) {
    if !journal::enabled() {
        return;
    }
    let desc = unsafe { p.as_ref() }.map_or(Description::Null, |d| Description::Sampler(*d));
    journal::set_descriptor(handle.ptr as u64, "sampler", 0, 0, desc);
}
unsafe fn ranges(n: u32, starts: *const D3D12_CPU_DESCRIPTOR_HANDLE, sizes: *const u32) -> Vec<u64> {
    let mut handles = Vec::new();
    for (i, start) in unsafe { slice(starts, n as usize) }.iter().enumerate() {
        let count = if sizes.is_null() {
            1
        } else {
            unsafe { *sizes.add(i) }
        };
        let Some(stride) = journal::descriptor_stride(start.ptr as u64) else {
            continue;
        };
        for offset in 0..count {
            handles.push(start.ptr as u64 + u64::from(offset) * u64::from(stride));
        }
    }
    handles
}
pub unsafe fn copy(
    nd: u32,
    d: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    ds: *const u32,
    ns: u32,
    s: *const D3D12_CPU_DESCRIPTOR_HANDLE,
    ss: *const u32,
) {
    if !journal::enabled() {
        return;
    }
    let destinations = unsafe { ranges(nd, d, ds) };
    let sources = unsafe { ranges(ns, s, ss) };
    journal::copy_descriptors(&destinations, &sources);
}
pub unsafe fn copy_simple(n: u32, d: D3D12_CPU_DESCRIPTOR_HANDLE, s: D3D12_CPU_DESCRIPTOR_HANDLE) {
    journal::copy_descriptors_simple(n, d.ptr as u64, s.ptr as u64);
}

fn srv_json(d: &D3D12_SHADER_RESOURCE_VIEW_DESC) -> Value {
    let payload = match d.ViewDimension {
        D3D12_SRV_DIMENSION_BUFFER => unsafe { d.Anonymous.Buffer }.portable(),
        D3D12_SRV_DIMENSION_TEXTURE1D => unsafe { d.Anonymous.Texture1D }.portable(),
        D3D12_SRV_DIMENSION_TEXTURE1DARRAY => unsafe { d.Anonymous.Texture1DArray }.portable(),
        D3D12_SRV_DIMENSION_TEXTURE2D => unsafe { d.Anonymous.Texture2D }.portable(),
        D3D12_SRV_DIMENSION_TEXTURE2DARRAY => unsafe { d.Anonymous.Texture2DArray }.portable(),
        D3D12_SRV_DIMENSION_TEXTURE2DMS => json!({}),
        D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY => unsafe { d.Anonymous.Texture2DMSArray }.portable(),
        D3D12_SRV_DIMENSION_TEXTURE3D => unsafe { d.Anonymous.Texture3D }.portable(),
        D3D12_SRV_DIMENSION_TEXTURECUBE => unsafe { d.Anonymous.TextureCube }.portable(),
        D3D12_SRV_DIMENSION_TEXTURECUBEARRAY => unsafe { d.Anonymous.TextureCubeArray }.portable(),
        D3D12_SRV_DIMENSION_RAYTRACING_ACCELERATION_STRUCTURE => {
            json!({"address":journal::address(unsafe {d.Anonymous.RaytracingAccelerationStructure}.Location)})
        }
        _ => json!({"diagnostic":"invalid descriptor dimension"}),
    };
    json!({"format":d.Format.0,"dimension":d.ViewDimension.0,"payload":payload,"mapping":d.Shader4ComponentMapping})
}

fn uav_json(d: &D3D12_UNORDERED_ACCESS_VIEW_DESC) -> Value {
    let payload = match d.ViewDimension {
        D3D12_UAV_DIMENSION_BUFFER => unsafe { d.Anonymous.Buffer }.portable(),
        D3D12_UAV_DIMENSION_TEXTURE1D => unsafe { d.Anonymous.Texture1D }.portable(),
        D3D12_UAV_DIMENSION_TEXTURE1DARRAY => unsafe { d.Anonymous.Texture1DArray }.portable(),
        D3D12_UAV_DIMENSION_TEXTURE2D => unsafe { d.Anonymous.Texture2D }.portable(),
        D3D12_UAV_DIMENSION_TEXTURE2DARRAY => unsafe { d.Anonymous.Texture2DArray }.portable(),
        D3D12_UAV_DIMENSION_TEXTURE3D => unsafe { d.Anonymous.Texture3D }.portable(),
        _ => json!({"diagnostic":"invalid descriptor dimension"}),
    };
    json!({"format":d.Format.0,"dimension":d.ViewDimension.0,"payload":payload})
}

fn rtv_json(d: &D3D12_RENDER_TARGET_VIEW_DESC) -> Value {
    let payload = match d.ViewDimension {
        D3D12_RTV_DIMENSION_BUFFER => unsafe { d.Anonymous.Buffer }.portable(),
        D3D12_RTV_DIMENSION_TEXTURE1D => unsafe { d.Anonymous.Texture1D }.portable(),
        D3D12_RTV_DIMENSION_TEXTURE1DARRAY => unsafe { d.Anonymous.Texture1DArray }.portable(),
        D3D12_RTV_DIMENSION_TEXTURE2D => unsafe { d.Anonymous.Texture2D }.portable(),
        D3D12_RTV_DIMENSION_TEXTURE2DARRAY => unsafe { d.Anonymous.Texture2DArray }.portable(),
        D3D12_RTV_DIMENSION_TEXTURE2DMS => json!({}),
        D3D12_RTV_DIMENSION_TEXTURE2DMSARRAY => unsafe { d.Anonymous.Texture2DMSArray }.portable(),
        D3D12_RTV_DIMENSION_TEXTURE3D => unsafe { d.Anonymous.Texture3D }.portable(),
        _ => json!({"diagnostic":"invalid descriptor dimension"}),
    };
    json!({"format":d.Format.0,"dimension":d.ViewDimension.0,"payload":payload})
}

fn dsv_json(d: &D3D12_DEPTH_STENCIL_VIEW_DESC) -> Value {
    let payload = match d.ViewDimension {
        D3D12_DSV_DIMENSION_TEXTURE1D => unsafe { d.Anonymous.Texture1D }.portable(),
        D3D12_DSV_DIMENSION_TEXTURE1DARRAY => unsafe { d.Anonymous.Texture1DArray }.portable(),
        D3D12_DSV_DIMENSION_TEXTURE2D => unsafe { d.Anonymous.Texture2D }.portable(),
        D3D12_DSV_DIMENSION_TEXTURE2DARRAY => unsafe { d.Anonymous.Texture2DArray }.portable(),
        D3D12_DSV_DIMENSION_TEXTURE2DMS => json!({}),
        D3D12_DSV_DIMENSION_TEXTURE2DMSARRAY => unsafe { d.Anonymous.Texture2DMSArray }.portable(),
        _ => json!({"diagnostic":"invalid descriptor dimension"}),
    };
    json!({"format":d.Format.0,"dimension":d.ViewDimension.0,"payload":payload,"flags":d.Flags.0})
}
/// A sampler whose border colour is float or integer, as its flags say.
pub unsafe fn sampler2(p: *const D3D12_SAMPLER_DESC2, handle: D3D12_CPU_DESCRIPTOR_HANDLE) {
    if !journal::enabled() {
        return;
    }
    let desc = unsafe { p.as_ref() }.map_or(Description::Null, |d| {
        let border = if d.Flags.0 & D3D12_SAMPLER_FLAG_UINT_BORDER_COLOR.0 != 0 {
            json!(unsafe { d.Anonymous.UintBorderColor })
        } else {
            json!(unsafe { d.Anonymous.FloatBorderColor })
        };
        Description::Json(
            json!({"Filter":d.Filter.0,"AddressU":d.AddressU.0,"AddressV":d.AddressV.0,
            "AddressW":d.AddressW.0,"MipLODBias":d.MipLODBias,"MaxAnisotropy":d.MaxAnisotropy,
            "ComparisonFunc":d.ComparisonFunc.0,"BorderColor":border,"MinLOD":d.MinLOD,"MaxLOD":d.MaxLOD,
            "Flags":d.Flags.0}),
        )
    });
    journal::set_descriptor(handle.ptr as u64, "sampler", 0, 0, desc);
}
/// A sampler-feedback UAV: the feedback map it writes, for the resource whose sampling it records.
pub unsafe fn sampler_feedback(
    target: *mut c_void,
    feedback: *mut c_void,
    handle: D3D12_CPU_DESCRIPTOR_HANDLE,
) {
    if !journal::enabled() {
        return;
    }
    let desc = Description::Json(json!({"sampler_feedback":{"target":id(target)}}));
    journal::set_descriptor(handle.ptr as u64, "uav", id(feedback), 0, desc);
}
