//! Persistent tile mappings, ordered tiled APIs, and canonical seeds for shared physical pool tiles.
mod packed;
use super::{Ctx, ids::resource_id, new_event, record};
use crate::d3d11::registry;
use locum_capture::{hash, stacks, writer};
use locum_d3d11::tiles::{Layout, Mappings, Range, Region};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::c_void,
    sync::Mutex,
};
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
    resource.as_ref().map_or(0, |r| {
        if super::armed() {
            resource_id(r)
        } else {
            crate::d3d11::registry::id_of(r)
        }
    })
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
    if !super::armed() {
        return;
    }
    packed::check_call(kind, &args);
    let mut e = new_event(kind, call, ctx.id, ctx.deferred, args);
    e.reads = reads.iter().copied().filter(|id| *id != 0).collect();
    e.writes = writes.iter().copied().filter(|id| *id != 0).collect();
    e.stack = stacks::capture();
    if let Some(data) = data {
        let key = hash::key(&data);
        writer::blob(&crate::settings::settings().blobs, key.clone(), data, true);
        e.data = Some(key);
    }
    if matches!(kind, "tile_map" | "tile_copy_map" | "tile_resize") && !ctx.deferred {
        dependencies(&mut e);
        if super::state().records(false) {
            super::state().timeline.push(e);
        }
    } else {
        record(ctx.c, e);
    }
}

struct Entry {
    layout: Layout,
    mappings: Mappings,
    pool: Option<ID3D11Resource>,
    /// Why the mappings are no longer known: a change the model couldn't follow. It stays until a change replaces the
    /// whole page table (one into another pool).
    error: Option<String>,
    /// Counts the changes, so a capture looks at the mappings again only when they changed.
    generation: u64,
}
impl Entry {
    fn new(resource: &ID3D11Resource) -> Result<Self, String> {
        Ok(Self {
            layout: Layout::query(resource)?,
            mappings: Mappings::default(),
            pool: None,
            error: None,
            generation: 0,
        })
    }
}
static MAPPINGS: Mutex<BTreeMap<u64, Entry>> = Mutex::new(BTreeMap::new());
fn mappings() -> std::sync::MutexGuard<'static, BTreeMap<u64, Entry>> {
    MAPPINGS.lock().unwrap_or_else(|e| e.into_inner())
}
static POOL_SIZES: Mutex<BTreeMap<u64, u64>> = Mutex::new(BTreeMap::new());
pub fn pool_size(id: u64) -> Option<u64> {
    POOL_SIZES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&id)
        .copied()
}
pub fn resized(raw: *mut c_void, bytes: u64) {
    let id = id(raw);
    POOL_SIZES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id, bytes);
    // Mapped tiles beyond the new size remain alive. Only unreferenced tail tiles can be discarded.
    if super::armed() {
        let live: BTreeSet<u32> = mappings()
            .values()
            .filter(|e| e.mappings.pool == Some(id))
            .flat_map(|e| e.mappings.tiles.values().copied())
            .collect();
        super::state()
            .tile_seeded
            .retain(|&(pool, tile)| pool != id || u64::from(tile) * 65536 < bytes || live.contains(&tile));
        if live.iter().any(|&tile| u64::from(tile) * 65536 >= bytes) && super::state().records(false) {
            gap("pool shrink retains mappings beyond its new size; replay of that lifetime is not verified yet".into());
        }
    }
}
pub fn destroyed(id: u64) {
    POOL_SIZES.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
    packed::destroyed(id);
    // Releasing a pool can enter the registry's destruction callback again.
    let old = mappings().remove(&id);
    drop(old);
}
/// Runs `f` on the resource's entry, made first if it has none, in place: a page table can hold a great many tiles.
/// Whatever `f` returns is dropped after the lock is released, so a pool reference it takes out of the entry can
/// enter the registry's destruction callback (which takes the lock) safely.
fn with_entry<R>(resource: &ID3D11Resource, f: impl FnOnce(&mut Entry) -> R) -> Result<R, String> {
    let id = registry::id_of(resource);
    let fresh = if mappings().contains_key(&id) {
        None
    } else {
        Some(Entry::new(resource)?)
    };
    let mut all = mappings();
    let entry = match fresh {
        Some(fresh) => all.entry(id).or_insert(fresh),
        None => all
            .get_mut(&id)
            .ok_or("the resource's tile mappings were dropped")?,
    };
    Ok(f(entry))
}
fn number(v: &Value) -> u32 {
    v.as_u64().unwrap_or(0) as u32
}
fn location(v: &Value) -> Option<[u32; 4]> {
    v.as_array().map(|_| [0, 1, 2, 3].map(|i| number(&v[i])))
}
fn area(v: &Value) -> Option<Region> {
    v.as_array().map(|_| Region {
        count: number(&v[0]),
        boxed: v[1].as_bool().unwrap_or(false),
        width: number(&v[2]),
        height: number(&v[3]),
        depth: number(&v[4]),
    })
}
/// What a CopyTileMappings copies, read from its source.
struct Source {
    pool: Option<u64>,
    values: Vec<Option<u32>>,
    resource: Option<ID3D11Resource>,
}
#[derive(Clone)]
pub struct Change {
    kind: &'static str,
    args: Value,
    dst: ID3D11Resource,
    src: Option<ID3D11Resource>,
}
impl Change {
    fn apply(&self) -> Result<(), String> {
        let a = &self.args;
        // A copy reads the source's mappings before it changes the destination's: they can be the same resource.
        let source = match (self.kind, &self.src) {
            ("tile_copy_map", Some(src)) => Some(with_entry(src, |s| {
                let from = s.layout.region(location(&a["src_at"]), area(&a["size"]))?;
                Ok::<_, String>(Source {
                    pool: s.mappings.pool,
                    values: s.mappings.at(&from),
                    resource: s.pool.clone(),
                })
            })??),
            ("tile_copy_map", None) => return Err("CopyTileMappings has no source".into()),
            _ => None,
        };
        let released = with_entry(&self.dst, |dst| {
            dst.generation += 1;
            let pool_before = dst.mappings.pool;
            let result = self.change(dst, source);
            if result.is_ok() {
                packed::mapped(dst);
            }
            if result.is_ok() && dst.mappings.pool != pool_before {
                dst.error = None;
            }
            if let Err(e) = &result {
                dst.error.get_or_insert_with(|| e.clone());
            }
            result
        })??;
        // The pool reference the entry let go of is released here, after the lock.
        drop(released);
        Ok(())
    }

    /// Changes the page table; returns the pool reference the entry held before, when it now holds another.
    fn change(&self, dst: &mut Entry, source: Option<Source>) -> Result<Option<ID3D11Resource>, String> {
        let a = &self.args;
        if let Some(Source {
            pool,
            values,
            resource,
        }) = source
        {
            let to = dst.layout.region(location(&a["dst_at"]), area(&a["size"]))?;
            dst.mappings.copy(&to, pool, &values)?;
            return Ok(std::mem::replace(&mut dst.pool, resource));
        }
        let mut locations = Vec::new();
        for i in 0..number(&a["regions"]) as usize {
            locations.extend(
                dst.layout
                    .region(location(&a["starts"][i]), area(&a["sizes"][i]))?,
            );
        }
        // Null counts: one range covers every location, several cover one tile each.
        let ranges: Vec<_> = (0..number(&a["ranges"]) as usize)
            .map(|i| Range {
                flag: number(&a["range_flags"][i]),
                offset: number(&a["offsets"][i]),
                count: a["counts"][i].as_u64().map_or(
                    if number(&a["ranges"]) == 1 {
                        locations.len() as u32
                    } else {
                        1
                    },
                    |n| n as u32,
                ),
            })
            .collect();
        dst.mappings
            .update(self.src.as_ref().map(registry::id_of), &locations, &ranges)?;
        Ok(match &self.src {
            Some(pool) => dst.pool.replace(pool.clone()),
            None => None,
        })
    }
}
static PENDING: Mutex<BTreeMap<u32, Vec<Change>>> = Mutex::new(BTreeMap::new());
fn pending() -> std::sync::MutexGuard<'static, BTreeMap<u32, Vec<Change>>> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner())
}
pub fn context_destroyed(id: u32) {
    let old = pending().remove(&id);
    drop(old);
}
/// Successful mapping calls are tracked outside captures too. Deferred changes wait for execution.
/// # Safety
/// `dst` and non-null `src` are live resources supplied to the successful call.
pub unsafe fn changed(ctx: Ctx, kind: &'static str, args: Value, dst: *mut c_void, src: *mut c_void) {
    let Some(dst) = (unsafe { ID3D11Resource::from_raw_borrowed(&dst) }) else {
        return;
    };
    let src = unsafe { ID3D11Resource::from_raw_borrowed(&src) }.cloned();
    let change = Change {
        kind,
        args,
        dst: dst.clone(),
        src,
    };
    if ctx.deferred {
        pending().entry(ctx.id).or_default().push(change);
    } else if let Err(e) = change.apply() {
        crate::log::log!("tile mapping tracking: {e}");
    }
}
/// Capture the old mapping before a call that changes it or resizes its pool.
pub fn before(ctx: Ctx, resources: &[*mut c_void]) {
    if !super::armed() || ctx.deferred {
        return;
    }
    let mut e = new_event("touch", "tile mapping before", ctx.id, false, Value::Null);
    e.reads = resources.iter().map(|&r| id(r)).filter(|&id| id != 0).collect();
    super::first_touch(ctx.c, &e);
}
/// Add physical-pool dependencies: writes through an alias must survive slicing another alias.
pub(super) fn dependencies(event: &mut super::Event) {
    let mut resources = Vec::new();
    {
        let entries = mappings();
        if event.kind == "tile_barrier" && (event.args["before"] == 0 || event.args["after"] == 0) {
            for e in entries.values() {
                if let Some(pool) = &e.pool {
                    resources.push((pool.clone(), false));
                    resources.push((pool.clone(), true));
                }
            }
        }
        for (ids, write) in [(&event.reads, false), (&event.writes, true)] {
            for id in ids {
                if let Some(pool) = entries.get(id).and_then(|e| e.pool.clone()) {
                    resources.push((pool, write));
                }
            }
        }
    }
    for (pool, write) in resources {
        let id = resource_id(&pool);
        if write {
            event.writes.push(id);
        } else {
            event.reads.push(id);
        }
    }
}
fn gap(error: String) {
    *super::state().gaps.entry(("tiled".into(), error)).or_default() += 1;
}
/// Initial page tables and canonical seeds for physical tiles, issued through their actual mapping.
pub(super) fn touch(c: &ID3D11DeviceContext4, resource: &ID3D11Resource) {
    let id = resource_id(resource);
    let Some(dims) = registry::dims_of(resource) else {
        return;
    };
    if dims.misc & D3D11_RESOURCE_MISC_TILE_POOL.0 as u32 != 0 {
        super::state().tile_initialized.insert(id);
        return;
    }
    // Every event that touches a tiled resource comes here; its mappings are only read again after they changed.
    let seen = super::state().tile_seen.get(&id).copied();
    let packed_generation = packed::generation();
    let looked = with_entry(resource, |e| {
        (seen != Some((e.generation, packed_generation))).then(|| {
            (
                e.generation,
                e.error.clone(),
                e.layout.clone(),
                e.mappings.tiles.iter().map(|(&v, &p)| (v, p)).collect::<Vec<_>>(),
                e.pool.clone(),
            )
        })
    });
    let (generation, error, layout, mapped, pool) = match looked {
        Ok(Some(looked)) => looked,
        Ok(None) => return,
        Err(e) => {
            gap(e);
            return;
        }
    };
    super::state()
        .tile_seen
        .insert(id, (generation, packed_generation));
    if let Some(error) = error {
        gap(error);
        return;
    }
    let pool = pool.as_ref().map_or(0, resource_id);
    packed::check_aliases(id, pool, &layout, &mapped);
    if pool_size(pool).is_some_and(|bytes| mapped.iter().any(|&(_, tile)| u64::from(tile) * 65536 >= bytes)) {
        gap("pool has live mappings beyond its shrunken size; native bootstrap is not verified yet".into());
    }
    if super::state().tile_initialized.insert(id) {
        let l = &layout;
        let mut boot = new_event(
            "tile_bootstrap",
            "InitialTileMappings",
            0,
            false,
            json!({"dst": id, "pool": pool, "tiles": mapped,
                "layout": {"total": l.total, "mips": l.mips, "array": l.array, "packed": l.packed,
                    "shape": l.shape, "subs": l.subs}}),
        );
        boot.writes.push(id);
        if pool != 0 {
            boot.reads.push(pool);
            super::state().tile_initialized.insert(pool);
        }
        super::state().timeline.push(boot);
    }
    packed::seed(c, resource, id, pool, &layout, &mapped);
    let standard: Vec<_> = mapped
        .into_iter()
        .filter(|&(v, _)| match layout.coordinate(v) {
            Ok(at) => !layout.is_packed(at[3]),
            Err(error) => {
                gap(error);
                false
            }
        })
        .collect();
    let mut chosen = BTreeSet::new();
    let tiles: Vec<_> = {
        let s = super::state();
        standard
            .into_iter()
            .filter(|&(_, p)| !s.tile_seeded.contains(&(pool, p)) && chosen.insert(p))
            .collect()
    };
    for chunk in tiles.chunks(256) {
        let coordinates: Result<Vec<_>, _> = chunk.iter().map(|&(v, _)| layout.coordinate(v)).collect();
        let coordinates = match coordinates {
            Ok(v) => v,
            Err(e) => {
                gap(e);
                continue;
            }
        };
        let bytes = coordinates.len() as u64 * 65536;
        if super::state().in_flight_bytes + bytes > super::contents::IN_FLIGHT {
            super::read_in_flight(c);
        }
        let rb = match unsafe { locum_d3d11::readback::issue_tiles(c, resource, &coordinates) } {
            Ok(rb) => rb,
            Err(e) => {
                gap(e);
                continue;
            }
        };
        let mut seed = new_event(
            "tile_seed",
            "InitialTileContents",
            0,
            false,
            json!({"dst": id, "pool": pool, "coordinates": coordinates}),
        );
        seed.reads.push(id);
        seed.writes.push(pool);
        seed.data_readback = Some(rb);
        let mut s = super::state();
        s.in_flight_bytes += bytes;
        s.tile_seeded.extend(chunk.iter().map(|&(_, p)| (pool, p)));
        let at = s.timeline.len();
        s.tile_seeds.push(at);
        s.timeline.push(seed);
    }
}

// Private COM data makes the recorded mapping metadata live exactly as long as the real list.
const LIST_KEY: windows::core::GUID = windows::core::GUID::from_u128(0x6c6f6375_6d00_4a5e_9d1b_7a2f8c3e0019);
#[repr(C)]
struct ListData {
    vtable: *const windows::core::IUnknown_Vtbl,
    refs: std::sync::atomic::AtomicU32,
    changes: Vec<Change>,
}
unsafe extern "system" fn query(
    this: *mut c_void,
    iid: *const windows::core::GUID,
    out: *mut *mut c_void,
) -> windows::core::HRESULT {
    if out.is_null() {
        return windows::Win32::Foundation::E_POINTER;
    }
    unsafe {
        *out = std::ptr::null_mut();
    }
    if !iid.is_null() && unsafe { *iid == IUnknown::IID } {
        unsafe {
            *out = this;
            add_ref(this);
        }
        windows::Win32::Foundation::S_OK
    } else {
        windows::Win32::Foundation::E_NOINTERFACE
    }
}
unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    unsafe { &*this.cast::<ListData>() }
        .refs
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1
}
unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    let n = unsafe { &*this.cast::<ListData>() }
        .refs
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
        - 1;
    if n == 0 {
        drop(unsafe { Box::from_raw(this.cast::<ListData>()) });
    }
    n
}
static LIST_VTABLE: windows::core::IUnknown_Vtbl = windows::core::IUnknown_Vtbl {
    QueryInterface: query,
    AddRef: add_ref,
    Release: release,
};
/// # Safety
/// `list` is the successful FinishCommandList result, or null when discarded.
pub unsafe fn finish(context: u32, list: *mut c_void) {
    let changes = pending().remove(&context).unwrap_or_default();
    if changes.is_empty() {
        return;
    }
    let Some(list) = (unsafe { ID3D11CommandList::from_raw_borrowed(&list) }) else {
        return;
    };
    let data = Box::new(ListData {
        vtable: &LIST_VTABLE,
        refs: std::sync::atomic::AtomicU32::new(1),
        changes,
    });
    let unknown = unsafe { IUnknown::from_raw(Box::into_raw(data).cast()) };
    if let Err(e) = unsafe { list.SetPrivateDataInterface(&LIST_KEY, &unknown) } {
        crate::log::log!("command list tile metadata: {e}");
    }
}
/// # Safety
/// `list` is a live command list supplied to ExecuteCommandList.
pub unsafe fn execute(ctx: Ctx, list: *mut c_void) {
    let Some(list) = (unsafe { ID3D11CommandList::from_raw_borrowed(&list) }) else {
        return;
    };
    let mut data: *mut c_void = std::ptr::null_mut();
    let mut bytes = size_of::<*mut c_void>() as u32;
    if unsafe {
        list.GetPrivateData(
            &LIST_KEY,
            &mut bytes,
            Some((&mut data as *mut *mut c_void).cast()),
        )
    }
    .is_err()
        || data.is_null()
    {
        return;
    }
    let unknown = unsafe { IUnknown::from_raw(data) };
    let changes = unsafe { &*data.cast::<ListData>() }.changes.clone();
    drop(unknown);
    if ctx.deferred {
        pending().entry(ctx.id).or_default().extend(changes);
    } else {
        if super::armed() && super::state().records(false) {
            gap("tile mapping changes inside a deferred list require execution-time tile seeds".into());
        }
        for change in changes {
            if let Err(e) = change.apply() {
                gap(e);
            }
        }
    }
}
