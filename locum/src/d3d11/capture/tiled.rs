//! Tiled API calls, retained in order even before native pool replay is available.
use super::{Ctx, ids::resource_id, new_event, record};
use locum_capture::{hash, stacks, writer};
use serde_json::{Value, json};
use std::ffi::c_void;
use windows::{
    Win32::Graphics::Direct3D11::*,
    core::{IUnknown, Interface},
};

pub fn special(resource: &ID3D11Resource) -> bool {
    crate::d3d11::registry::dims_of(resource).is_some_and(|d| {
        d.misc & (D3D11_RESOURCE_MISC_TILED.0 as u32 | D3D11_RESOURCE_MISC_TILE_POOL.0 as u32) != 0
    })
}
/// A barrier can name either a resource or a view; its underlying resource supplies the dependency.
pub fn id(raw: *mut c_void) -> u64 {
    let Some(o) = (unsafe { IUnknown::from_raw_borrowed(&raw) }) else {
        return 0;
    };
    let resource = o.cast::<ID3D11Resource>().ok().or_else(|| {
        o.cast::<ID3D11View>()
            .ok()
            .and_then(|v| unsafe { v.GetResource() }.ok())
    });
    resource.as_ref().map_or(0, resource_id)
}
/// # Safety
/// `p` is null or points to `count` elements of this call's array.
pub unsafe fn array<T>(p: *const T, count: u32, f: impl Fn(&T) -> Value) -> Value {
    if p.is_null() {
        return Value::Null;
    }
    Value::Array(
        unsafe { std::slice::from_raw_parts(p, count as usize) }
            .iter()
            .map(f)
            .collect(),
    )
}
pub fn coordinate(c: &D3D11_TILED_RESOURCE_COORDINATE) -> Value {
    json!([c.X, c.Y, c.Z, c.Subresource])
}
pub fn size(s: &D3D11_TILE_REGION_SIZE) -> Value {
    json!([s.NumTiles, s.bUseBox.as_bool(), s.Width, s.Height, s.Depth])
}
/// # Safety
/// The pointer is null or references the call's coordinate.
pub unsafe fn coord(p: *const D3D11_TILED_RESOURCE_COORDINATE) -> Value {
    unsafe { p.as_ref() }.map_or(Value::Null, coordinate)
}
/// # Safety
/// The pointer is null or references the call's tile-region size.
pub unsafe fn region(p: *const D3D11_TILE_REGION_SIZE) -> Value {
    unsafe { p.as_ref() }.map_or(Value::Null, size)
}
/// # Safety
/// The pointers describe the valid source memory of UpdateTiles.
pub unsafe fn data(p: *const c_void, size: *const D3D11_TILE_REGION_SIZE) -> Option<Vec<u8>> {
    if p.is_null() {
        return None;
    }
    let s = unsafe { size.as_ref() }?;
    let count = if s.bUseBox.as_bool() {
        u64::from(s.Width) * u64::from(s.Height) * u64::from(s.Depth)
    } else {
        u64::from(s.NumTiles)
    };
    let bytes = usize::try_from(count.checked_mul(65536)?).ok()?;
    Some(unsafe { std::slice::from_raw_parts(p.cast::<u8>(), bytes) }.to_vec())
}
#[allow(clippy::too_many_arguments)]
pub fn event(
    ctx: Ctx,
    kind: &'static str,
    call: &'static str,
    args: Value,
    reads: &[u64],
    writes: &[u64],
    data: Option<Vec<u8>>,
) {
    let mut e = new_event(kind, call, ctx.id, ctx.deferred, args);
    e.reads = reads.iter().copied().filter(|id| *id != 0).collect();
    e.writes = writes.iter().copied().filter(|id| *id != 0).collect();
    e.stack = stacks::capture();
    if let Some(data) = data {
        let key = hash::key(&data);
        writer::blob(&crate::settings::settings().blobs, key.clone(), data, true);
        e.data = Some(key);
    }
    record(ctx.c, e);
}
