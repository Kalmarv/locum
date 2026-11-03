//! Numbers for the resources, views and state objects a capture records, kept on the objects themselves (private
//! data), and a description of each in the capture's file.

use core::ffi::c_void;

use serde_json::json;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::{GUID, Interface};

use super::state;

/// The private-data key of the number locum gives each object it records.
const ID_KEY: GUID = GUID::from_u128(0x6c6f_6375_6d00_4a5e_9d1b_7a2f_8c3e_0002);

/// The id locum gave `object`, giving it one the first time. An object is described (a line in the capture's file)
/// the first time each capture records it, so every capture's file stands alone.
unsafe fn id_of(object: &ID3D11DeviceChild, describe: impl FnOnce(u64) -> serde_json::Value) -> u64 {
    // [id, number of the capture that last described it]
    let mut data = [0u64; 2];
    let mut size = 16u32;
    let known = unsafe { object.GetPrivateData(&ID_KEY, &mut size, Some(data.as_mut_ptr().cast())) }.is_ok()
        && size == 16;
    let mut s = state();
    if known && data[1] == s.captures {
        return data[0];
    }
    let id = if known {
        data[0]
    } else {
        s.next_id += 1;
        s.next_id - 1
    };
    let data = [id, s.captures];
    drop(s);
    let _ = unsafe { object.SetPrivateData(&ID_KEY, 16, Some(data.as_ptr().cast())) };
    let line = describe(id).to_string();
    state().objects.push(line);
    id
}

pub(super) unsafe fn resource_id(resource: &ID3D11Resource) -> u64 {
    let child: ID3D11DeviceChild = resource.cast().expect("a resource is a device child");
    unsafe { id_of(&child, |id| describe_resource(id, resource)) }
}

/// The id of a buffer passed to an indirect draw (0 for null).
///
/// # Safety
/// `buffer` must be null or a live `ID3D11Buffer`.
pub unsafe fn buffer_id(buffer: *mut c_void) -> u64 {
    let Some(buffer) = (unsafe { ID3D11Resource::from_raw_borrowed(&buffer) }) else {
        return 0;
    };
    unsafe { resource_id(buffer) }
}

fn describe_resource(id: u64, resource: &ID3D11Resource) -> serde_json::Value {
    if let Ok(t) = resource.cast::<ID3D11Texture2D>() {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { t.GetDesc(&mut d) };
        return json!({"e": "resource", "id": id, "kind": "texture2d", "width": d.Width, "height": d.Height,
            "mips": d.MipLevels, "array": d.ArraySize, "format": d.Format.0, "samples": d.SampleDesc.Count,
            "usage": d.Usage.0, "bind": d.BindFlags, "misc": d.MiscFlags});
    }
    if let Ok(b) = resource.cast::<ID3D11Buffer>() {
        let mut d = D3D11_BUFFER_DESC::default();
        unsafe { b.GetDesc(&mut d) };
        return json!({"e": "resource", "id": id, "kind": "buffer", "bytes": d.ByteWidth, "usage": d.Usage.0,
            "bind": d.BindFlags, "misc": d.MiscFlags, "stride": d.StructureByteStride});
    }
    if let Ok(t) = resource.cast::<ID3D11Texture3D>() {
        let mut d = D3D11_TEXTURE3D_DESC::default();
        unsafe { t.GetDesc(&mut d) };
        return json!({"e": "resource", "id": id, "kind": "texture3d", "width": d.Width, "height": d.Height,
            "depth": d.Depth, "mips": d.MipLevels, "format": d.Format.0});
    }
    if let Ok(t) = resource.cast::<ID3D11Texture1D>() {
        let mut d = D3D11_TEXTURE1D_DESC::default();
        unsafe { t.GetDesc(&mut d) };
        return json!({"e": "resource", "id": id, "kind": "texture1d", "width": d.Width, "mips": d.MipLevels,
            "array": d.ArraySize, "format": d.Format.0});
    }
    json!({"e": "resource", "id": id, "kind": "unknown"})
}

/// A view's id, and its resource's id.
pub(super) unsafe fn view_ids(
    view: &ID3D11View,
    kind: &str,
    format: DXGI_FORMAT,
    dimension: i32,
) -> (u64, u64) {
    let resource = unsafe { view.GetResource() }.ok();
    let resource_id = resource.as_ref().map_or(0, |r| unsafe { resource_id(r) });
    let child: ID3D11DeviceChild = view.cast().expect("a view is a device child");
    let id = unsafe {
        id_of(&child, |id| {
            json!({"e": "view", "id": id, "kind": kind, "resource": resource_id, "format": format.0,
                "dimension": dimension})
        })
    };
    (id, resource_id)
}

/// A state object's or input layout's id (0 for none bound).
pub(super) unsafe fn state_id<T: Interface>(object: Option<T>, kind: &str) -> u64 {
    let Some(object) = object.and_then(|o| o.cast::<ID3D11DeviceChild>().ok()) else {
        return 0;
    };
    unsafe { id_of(&object, |id| json!({"e": "state", "id": id, "kind": kind})) }
}
