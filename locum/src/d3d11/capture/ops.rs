//! The calls besides draws that change what is on the GPU, recorded as events while a capture runs: maps, updates,
//! copies, clears, resolves, mip generation, discards.
//!
//! **Maps.** While a capture runs, a write map hands the game a shadow in locum's memory instead of the driver's
//! (write-combined, slow to read) memory; at `Unmap` the whole shadow is copied to the driver's memory and the bytes
//! the game wrote are recorded. A discard map's shadow starts as a known pattern, so what differs from it is what was
//! written; any other write map's shadow starts as the resource's contents (from locum's own copy when it has one,
//! else read from the mapping) and what changed is what was written. The game's data reaches the driver in full
//! either way. Read maps are passed through.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use locum_capture::cost::Cost;
use locum_capture::layout;
use locum_capture::{hash, maps, stacks, writer};
use serde_json::{Value, json};
use windows::Win32::Graphics::Direct3D11::*;
use windows::core::{HRESULT, Interface};

pub use super::Ctx;
use super::ids::{resource_id, view_ids};
use super::{armed, new_event, record};
use crate::d3d11::registry;
use crate::settings::settings;

/// A map handed out with a shadow.
struct Shadow {
    real: *mut c_void,
    data: Vec<u8>,
    /// What the shadow started as, when not the discard pattern.
    reference: Option<Vec<u8>>,
    map_type: i32,
    row_pitch: u32,
    depth_pitch: u32,
    resource_id: u64,
    context: u32,
    deferred: bool,
}

unsafe impl Send for Shadow {}

/// Shadows by (context, resource, subresource) address.
type Shadows = HashMap<(usize, usize, u32), Shadow>;
static SHADOWS: Mutex<Option<Shadows>> = Mutex::new(None);
/// Maps given a shadow (bytes) and unmaps of one recorded, on the game's threads.
pub static SHADOWED: Cost = Cost::new();
static OUTSTANDING: AtomicUsize = AtomicUsize::new(0);
/// The last bytes locum wrote through each (context, resource, subresource) during this capture: the contents a
/// following no-overwrite map starts from.
type Mirrors = HashMap<(u32, u64, u32), Vec<u8>>;
static MIRRORS: Mutex<Option<Mirrors>> = Mutex::new(None);

/// Clears the mirrors when a capture ends.
pub(super) fn reset() {
    *MIRRORS.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// `Map` on a context. `real` makes the real call.
///
/// # Safety
/// The arguments are the call's.
#[allow(clippy::too_many_arguments)]
pub unsafe fn map(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    resource: *mut c_void,
    sub: u32,
    map_type: D3D11_MAP,
    out: *mut D3D11_MAPPED_SUBRESOURCE,
    real: impl FnOnce() -> HRESULT,
) -> HRESULT {
    if !armed() {
        return real();
    }
    let Some(r) = (unsafe { ID3D11Resource::from_raw_borrowed(&resource) }) else {
        return real();
    };
    let id = resource_id(r);
    let write = map_type != D3D11_MAP_READ;
    let mut e = new_event(
        "map",
        "Map",
        context,
        deferred,
        json!({"resource": id, "sub": sub, "type": map_type.0}),
    );
    // The resource is copied before the map (a mapped resource cannot be copied); a write is recorded at the unmap,
    // a read now.
    if write {
        e.kind = "map_begin";
        e.writes.push(id);
        super::touch_only(c, &e);
    } else {
        e.kind = "map_read";
        e.reads.push(id);
        super::touch_only(c, &e);
    }
    let hr = real();
    if hr.is_err() || out.is_null() || !write {
        if hr.is_ok() && !write {
            record(c, e);
        }
        return hr;
    }
    let Some((kind, Some(s))) = registry::sub_of(id, sub) else {
        return hr;
    };
    let m = unsafe { &mut *out };
    let size = if kind == "buffer" {
        s.size as usize
    } else if s.depth > 1 {
        m.DepthPitch as usize * s.depth as usize
    } else {
        m.RowPitch as usize * s.rows as usize
    };
    if size == 0 || m.pData.is_null() {
        return hr;
    }
    let started = std::time::Instant::now();
    let mut data = vec![0u8; size];
    let reference = if map_type == D3D11_MAP_WRITE_DISCARD {
        maps::fill(&mut data);
        None
    } else {
        // A dynamic resource changes only through maps, so what locum last wrote through this context is what its
        // memory holds; anything else (a staging resource the GPU copies into) is read from the map itself.
        let mirror = dynamic(r)
            .then(|| {
                MIRRORS
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .as_ref()
                    .and_then(|m| m.get(&(context, id, sub)).filter(|v| v.len() == size).cloned())
            })
            .flatten();
        match mirror {
            Some(v) => data = v,
            None => unsafe { core::ptr::copy_nonoverlapping(m.pData.cast::<u8>(), data.as_mut_ptr(), size) },
        }
        Some(data.clone())
    };
    let shadow = Shadow {
        real: m.pData,
        data,
        reference,
        map_type: map_type.0,
        row_pitch: m.RowPitch,
        depth_pitch: m.DepthPitch,
        resource_id: id,
        context,
        deferred,
    };
    let mut shadows = SHADOWS.lock().unwrap_or_else(|e| e.into_inner());
    let map = shadows.get_or_insert_with(HashMap::new);
    let key = (c.as_raw() as usize, resource as usize, sub);
    let entry = map.entry(key).insert_entry(shadow);
    m.pData = entry.get().data.as_ptr() as *mut c_void;
    OUTSTANDING.fetch_add(1, Ordering::Relaxed);
    SHADOWED.add(started, size as u64);
    hr
}

/// `Unmap` on a context. `real` makes the real call.
///
/// # Safety
/// The arguments are the call's.
pub unsafe fn unmap(c: &ID3D11DeviceContext4, resource: *mut c_void, sub: u32, real: impl FnOnce()) {
    if OUTSTANDING.load(Ordering::Relaxed) == 0 {
        return real();
    }
    let shadow = SHADOWS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(|m| m.remove(&(c.as_raw() as usize, resource as usize, sub)));
    let Some(shadow) = shadow else { return real() };
    OUTSTANDING.fetch_sub(1, Ordering::Relaxed);
    // A discarding map hands out memory with nothing in it, so all of the shadow goes (the pattern where the game
    // wrote nothing); any other map gets back only the words the game wrote, and the rest stays as it was.
    match &shadow.reference {
        None => unsafe {
            core::ptr::copy_nonoverlapping(shadow.data.as_ptr(), shadow.real.cast::<u8>(), shadow.data.len())
        },
        Some(reference) => {
            for (a, b) in maps::changed(&shadow.data, Some(reference)) {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        shadow.data.as_ptr().add(a),
                        shadow.real.cast::<u8>().add(a),
                        b - a,
                    )
                };
            }
        }
    }
    real();
    if !armed() {
        return;
    }
    let started = std::time::Instant::now();
    let ranges = maps::written(&shadow.data, shadow.reference.as_deref());
    let texture = registry::sub_of(shadow.resource_id, sub).filter(|(kind, _)| *kind != "buffer");
    let (bytes, args) = match texture {
        // A texture's map is recorded as its whole subresource, tightly packed.
        Some((_, s)) => {
            let Some(s) = s else { return };
            let mut packed = Vec::with_capacity(s.size as usize);
            unsafe {
                layout::pack(
                    &s,
                    shadow.data.as_ptr(),
                    shadow.row_pitch as usize,
                    shadow.depth_pitch as usize,
                    &mut packed,
                )
            };
            (
                packed,
                json!({"resource": shadow.resource_id, "sub": sub, "type": shadow.map_type, "whole": true}),
            )
        }
        None => {
            let mut out = Vec::new();
            for &(a, b) in &ranges {
                out.extend_from_slice(&shadow.data[a..b]);
            }
            let r: Vec<Value> = ranges.iter().map(|&(a, b)| json!([a, b - a])).collect();
            (
                out,
                json!({"resource": shadow.resource_id, "sub": sub, "type": shadow.map_type, "ranges": r}),
            )
        }
    };
    MIRRORS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert((shadow.context, shadow.resource_id, sub), shadow.data);
    let key = hash::key(&bytes);
    let blobs = &settings().blobs;
    writer::blob(blobs, key.clone(), bytes, true);
    let mut e = new_event("map", "Unmap", shadow.context, shadow.deferred, args);
    e.data = Some(key);
    e.stack = stacks::capture();
    e.writes.push(shadow.resource_id);
    record(c, e);
    SHADOWED.add(started, 0);
}

/// Whether `r` was made with `D3D11_USAGE_DYNAMIC`.
fn dynamic(r: &ID3D11Resource) -> bool {
    let usage = if let Ok(b) = r.cast::<ID3D11Buffer>() {
        let mut d = D3D11_BUFFER_DESC::default();
        unsafe { b.GetDesc(&mut d) };
        d.Usage
    } else if let Ok(t) = r.cast::<ID3D11Texture2D>() {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { t.GetDesc(&mut d) };
        d.Usage
    } else if let Ok(t) = r.cast::<ID3D11Texture1D>() {
        let mut d = D3D11_TEXTURE1D_DESC::default();
        unsafe { t.GetDesc(&mut d) };
        d.Usage
    } else if let Ok(t) = r.cast::<ID3D11Texture3D>() {
        let mut d = D3D11_TEXTURE3D_DESC::default();
        unsafe { t.GetDesc(&mut d) };
        d.Usage
    } else {
        return false;
    };
    usage == D3D11_USAGE_DYNAMIC
}

fn res(raw: *mut c_void) -> Option<ID3D11Resource> {
    unsafe { ID3D11Resource::from_raw_borrowed(&raw) }.cloned()
}

fn view(raw: *mut c_void) -> Option<ID3D11View> {
    unsafe { ID3D11View::from_raw_borrowed(&raw) }.cloned()
}

fn box_json(b: *const D3D11_BOX) -> Value {
    if b.is_null() {
        return Value::Null;
    }
    let b = unsafe { &*b };
    json!([b.left, b.top, b.front, b.right, b.bottom, b.back])
}

/// An event that reads `reads` and writes `writes`.
fn op(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    kind: &'static str,
    call: &'static str,
    args: Value,
    reads: &[u64],
    writes: &[u64],
) {
    let mut e = new_event(kind, call, context, deferred, args);
    e.reads = reads.to_vec();
    e.writes = writes.to_vec();
    record(c, e);
}

pub fn copy_resource(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    dst: *mut c_void,
    src: *mut c_void,
) {
    let (Some(d), Some(s)) = (res(dst), res(src)) else {
        return;
    };
    let (d, s) = (resource_id(&d), resource_id(&s));
    op(
        Ctx {
            c,
            id: context,
            deferred,
        },
        "copy",
        "CopyResource",
        json!({"dst": d, "src": s}),
        &[s],
        &[d],
    );
}

#[allow(clippy::too_many_arguments)]
pub fn copy_region(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    call: &'static str,
    dst: *mut c_void,
    dst_sub: u32,
    at: [u32; 3],
    src: *mut c_void,
    src_sub: u32,
    region: *const D3D11_BOX,
    flags: u32,
) {
    let (Some(d), Some(s)) = (res(dst), res(src)) else {
        return;
    };
    let (d, s) = (resource_id(&d), resource_id(&s));
    let args = json!({"dst": d, "dst_sub": dst_sub, "at": at, "src": s, "src_sub": src_sub, "box": box_json(region),
        "flags": flags});
    op(
        Ctx {
            c,
            id: context,
            deferred,
        },
        "copy_region",
        call,
        args,
        &[s],
        &[d],
    );
}

/// `UpdateSubresource` while armed: the bytes it writes (canonical layout of the region) go to the blob store.
///
/// # Safety
/// The arguments are the call's.
#[allow(clippy::too_many_arguments)]
pub unsafe fn update(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    call: &'static str,
    dst: *mut c_void,
    sub: u32,
    region: *const D3D11_BOX,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
    flags: u32,
) {
    let Some(d) = res(dst) else { return };
    let id = resource_id(&d);
    let region_ref = if region.is_null() {
        None
    } else {
        Some(unsafe { &*region })
    };
    let bytes = unsafe { registry::updated(&d, sub, region_ref, data, row_pitch, depth_pitch, true) };
    let mut e = new_event(
        "update",
        call,
        context,
        deferred,
        json!({"dst": id, "sub": sub, "box": box_json(region), "flags": flags}),
    );
    if let Some((bytes, key)) = bytes {
        let blobs = &settings().blobs;
        writer::blob(blobs, key.clone(), bytes, true);
        e.data = Some(key);
    }
    e.stack = stacks::capture();
    e.writes.push(id);
    record(c, e);
}

fn view_target(raw: *mut c_void) -> Option<(u64, u64)> {
    view(raw).map(|v| view_ids(&v))
}

pub fn clear_rtv(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    rtv: *mut c_void,
    color: *const f32,
) {
    let Some((v, r)) = view_target(rtv) else { return };
    let color: [f32; 4] = if color.is_null() {
        [0.0; 4]
    } else {
        unsafe { *(color as *const [f32; 4]) }
    };
    op(
        Ctx {
            c,
            id: context,
            deferred,
        },
        "clear",
        "ClearRenderTargetView",
        json!({"view": v, "color": color}),
        &[],
        &[r],
    );
}

pub fn clear_dsv(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    dsv: *mut c_void,
    flags: u32,
    depth: f32,
    stencil: u8,
) {
    let Some((v, r)) = view_target(dsv) else { return };
    op(
        Ctx {
            c,
            id: context,
            deferred,
        },
        "clear",
        "ClearDepthStencilView",
        json!({"view": v, "flags": flags, "depth": depth, "stencil": stencil}),
        &[],
        &[r],
    );
}

pub fn clear_uav(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    call: &'static str,
    uav: *mut c_void,
    values: Value,
) {
    let Some((v, r)) = view_target(uav) else { return };
    op(
        Ctx {
            c,
            id: context,
            deferred,
        },
        "clear",
        call,
        json!({"view": v, "values": values}),
        &[],
        &[r],
    );
}

pub fn clear_view(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    target: *mut c_void,
    color: *const f32,
    rects: *const windows::Win32::Foundation::RECT,
    count: u32,
) {
    let Some((v, r)) = view_target(target) else { return };
    let color: [f32; 4] = if color.is_null() {
        [0.0; 4]
    } else {
        unsafe { *(color as *const [f32; 4]) }
    };
    let rects: Vec<Value> = (0..count as usize)
        .filter(|_| !rects.is_null())
        .map(|i| {
            let r = unsafe { &*rects.add(i) };
            json!([r.left, r.top, r.right, r.bottom])
        })
        .collect();
    op(
        Ctx {
            c,
            id: context,
            deferred,
        },
        "clear",
        "ClearView",
        json!({"view": v, "color": color, "rects": rects}),
        &[],
        &[r],
    );
}

#[allow(clippy::too_many_arguments)]
pub fn resolve(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    dst: *mut c_void,
    dst_sub: u32,
    src: *mut c_void,
    src_sub: u32,
    format: i32,
) {
    let (Some(d), Some(s)) = (res(dst), res(src)) else {
        return;
    };
    let (d, s) = (resource_id(&d), resource_id(&s));
    op(
        Ctx {
            c,
            id: context,
            deferred,
        },
        "resolve",
        "ResolveSubresource",
        json!({"dst": d, "dst_sub": dst_sub, "src": s, "src_sub": src_sub, "format": format}),
        &[s],
        &[d],
    );
}

pub fn generate_mips(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    srv: *mut c_void,
) {
    let Some((v, r)) = view_target(srv) else { return };
    op(
        Ctx {
            c,
            id: context,
            deferred,
        },
        "mips",
        "GenerateMips",
        json!({"view": v}),
        &[r],
        &[r],
    );
}

pub fn copy_structure_count(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    dst: *mut c_void,
    offset: u32,
    uav: *mut c_void,
) {
    let (Some(d), Some((v, r))) = (res(dst), view_target(uav)) else {
        return;
    };
    let d = resource_id(&d);
    op(
        Ctx {
            c,
            id: context,
            deferred,
        },
        "copy_count",
        "CopyStructureCount",
        json!({"dst": d, "offset": offset, "view": v}),
        &[r],
        &[d],
    );
}

pub fn discard(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    call: &'static str,
    target: *mut c_void,
) {
    // A discard leaves the resource undefined: it counts as a write, so the contents are copied before it.
    let id = if call == "DiscardResource" {
        res(target).map(|r| {
            let id = resource_id(&r);
            ("resource", id, id)
        })
    } else {
        view_target(target).map(|(v, r)| ("view", v, r))
    };
    let Some((what, id, resource)) = id else {
        return;
    };
    let mut args = serde_json::Map::new();
    args.insert(what.into(), id.into());
    op(
        Ctx {
            c,
            id: context,
            deferred,
        },
        "discard",
        call,
        Value::Object(args),
        &[],
        &[resource],
    );
}

/// UAV counters set when UAVs are bound (`-1` keeps the counter).
pub fn uav_counters(
    Ctx {
        c,
        id: context,
        deferred,
    }: Ctx,
    call: &'static str,
    start: u32,
    views: *const *mut c_void,
    counts: *const u32,
    n: u32,
) {
    if counts.is_null() || views.is_null() {
        return;
    }
    let mut set = Vec::new();
    for i in 0..n as usize {
        let count = unsafe { *counts.add(i) };
        if count == u32::MAX {
            continue;
        }
        let raw = unsafe { *views.add(i) };
        if let Some((v, _)) = view_target(raw) {
            set.push(json!({"slot": start + i as u32, "view": v, "count": count}));
        }
    }
    if !set.is_empty() {
        op(
            Ctx {
                c,
                id: context,
                deferred,
            },
            "uav_counter",
            call,
            json!({"set": set}),
            &[],
            &[],
        );
    }
}
