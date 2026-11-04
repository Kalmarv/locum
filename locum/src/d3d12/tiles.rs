//! Reserved resources' tile mappings, followed through every `UpdateTileMappings` and `CopyTileMappings`, armed or
//! not, in the model [`locum_capture::tiles`] shares with D3D11: an armed frame records each reserved resource's page
//! table as the frame found it and the frame's changes to it, in queue order (see [`super::journal`]).
//!
//! A reserved resource's contents are copied whole at its first use, like any other resource's: on tiled resources
//! tier 2 and up a copy reads zero from an unmapped tile, as a shader does, so the copy is what the frame could read.
use core::ffi::c_void;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, MutexGuard};

use locum_capture::tiles::{Layout, Pages, Range, Region};
use serde_json::{Value, json};
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::journal::{self, slice};

struct Entry {
    layout: Layout,
    pages: Pages,
    /// Why the page table is no longer known: a change it could not follow. It stays for the resource's life.
    error: Option<String>,
}

fn entries() -> MutexGuard<'static, BTreeMap<u64, Entry>> {
    static ENTRIES: Mutex<BTreeMap<u64, Entry>> = Mutex::new(BTreeMap::new());
    ENTRIES.lock().unwrap_or_else(|e| e.into_inner())
}

/// The device's tile numbering of a reserved resource.
pub fn layout(resource: &ID3D12Resource) -> Result<Layout, String> {
    let d = unsafe { resource.GetDesc() };
    let (mips, array) = match d.Dimension {
        D3D12_RESOURCE_DIMENSION_BUFFER => (1, 1),
        D3D12_RESOURCE_DIMENSION_TEXTURE3D => (u32::from(d.MipLevels), 1),
        _ => (u32::from(d.MipLevels), u32::from(d.DepthOrArraySize)),
    };
    if mips == 0 || array == 0 {
        return Err("the resource reports no subresources".into());
    }
    let mut device: Option<ID3D12Device> = None;
    unsafe { resource.GetDevice(&mut device) }.map_err(|e| format!("GetDevice: {e}"))?;
    let device = device.ok_or("GetDevice gave no device")?;
    let mut count = mips.checked_mul(array).ok_or("subresource count overflow")?;
    let mut subs = vec![D3D12_SUBRESOURCE_TILING::default(); count as usize];
    let mut total = 0;
    let mut packed = D3D12_PACKED_MIP_INFO::default();
    let mut shape = D3D12_TILE_SHAPE::default();
    unsafe {
        device.GetResourceTiling(
            resource,
            Some(&mut total),
            Some(&mut packed),
            Some(&mut shape),
            Some(&mut count),
            0,
            subs.as_mut_ptr(),
        )
    };
    subs.truncate(count as usize);
    if total == 0 {
        return Err("the device reports no tiles for the resource".into());
    }
    Ok(Layout {
        total,
        mips,
        array,
        packed: [
            u32::from(packed.NumStandardMips),
            u32::from(packed.NumPackedMips),
            packed.NumTilesForPackedMips,
            packed.StartTileIndexInOverallResource,
        ],
        shape: [shape.WidthInTexels, shape.HeightInTexels, shape.DepthInTexels],
        subs: subs
            .iter()
            .map(|s| {
                [
                    s.WidthInTiles,
                    u32::from(s.HeightInTiles),
                    u32::from(s.DepthInTiles),
                    s.StartTileIndexInOverallResource,
                ]
            })
            .collect(),
    })
}

/// Runs `f` on the entry of reserved resource `id` (`resource`, real), made first if it has none.
fn with_entry<R>(id: u64, resource: &ID3D12Resource, f: impl FnOnce(&mut Entry) -> R) -> Result<R, String> {
    let fresh = if entries().contains_key(&id) {
        None
    } else {
        Some(Entry {
            layout: layout(resource)?,
            pages: Pages::default(),
            error: None,
        })
    };
    let mut all = entries();
    Ok(f(all.entry(id).or_insert_with(|| fresh.expect("made above"))))
}

/// The real resource behind a wrapped pointer an application passed.
fn real(wrapped: *mut c_void) -> Option<ID3D12Resource> {
    let raw = super::unwrap(wrapped);
    unsafe { ID3D12Resource::from_raw_borrowed(&raw) }.cloned()
}

/// Reserved resource `id`'s tile numbering and page table as captures record them ([`Layout::json`],
/// [`Pages::runs`]); one never mapped has an empty page table.
pub fn pages(id: u64) -> Result<(Value, Value), String> {
    let resource = super::real(id)
        .and_then(|r| r.cast::<ID3D12Resource>().ok())
        .ok_or("no live resource")?;
    with_entry(id, &resource, |e| match &e.error {
        Some(error) => Err(error.clone()),
        None => Ok((e.layout.json(), json!(e.pages.runs()))),
    })?
}

pub fn destroyed(id: u64) {
    entries().remove(&id);
}

fn coordinate(c: &D3D12_TILED_RESOURCE_COORDINATE) -> [u32; 4] {
    [c.X, c.Y, c.Z, c.Subresource]
}

fn region(r: &D3D12_TILE_REGION_SIZE) -> Region {
    Region {
        count: r.NumTiles,
        boxed: r.UseBox.as_bool(),
        width: r.Width,
        height: u32::from(r.Height),
        depth: u32::from(r.Depth),
    }
}

/// The resource's tiles regions name, in order: a region with no start coordinate starts at the first tile, one with
/// neither coordinate nor size is the whole resource (as [`Layout::region`] reads them).
unsafe fn locations(
    layout: &Layout,
    n: u32,
    starts: *const D3D12_TILED_RESOURCE_COORDINATE,
    sizes: *const D3D12_TILE_REGION_SIZE,
) -> Result<Vec<u32>, String> {
    let starts = unsafe { slice(starts, n as usize) };
    let sizes = unsafe { slice(sizes, n as usize) };
    let mut out = Vec::new();
    for i in 0..n as usize {
        out.extend(layout.region(starts.get(i).map(coordinate), sizes.get(i).map(region))?);
    }
    Ok(out)
}

/// After the application's `UpdateTileMappings` on `queue`, the `queue_seq`th operation in its order (the queue's
/// lock is held): the page table follows it, and an armed frame records it.
///
/// # Safety
/// The arguments are the call's, as the application passed them.
#[allow(clippy::too_many_arguments)]
pub unsafe fn update(
    queue: &journal::Data,
    queue_seq: u64,
    resource: *mut c_void,
    regions: u32,
    starts: *const D3D12_TILED_RESOURCE_COORDINATE,
    sizes: *const D3D12_TILE_REGION_SIZE,
    heap: *mut c_void,
    ranges: u32,
    flags: *const D3D12_TILE_RANGE_FLAGS,
    offsets: *const u32,
    counts: *const u32,
    mapping: D3D12_TILE_MAPPING_FLAGS,
) {
    if !journal::enabled() {
        return;
    }
    let id = super::id(resource);
    let heap = (!heap.is_null()).then(|| super::id(heap));
    let n = ranges as usize;
    let (flag_values, offset_values, count_values) = unsafe {
        (
            slice(flags, n).iter().map(|f| f.0 as u32).collect::<Vec<_>>(),
            slice(offsets, n).to_vec(),
            slice(counts, n).to_vec(),
        )
    };
    let args = json!({
        "resource": id,
        "regions": regions,
        "starts": unsafe { slice(starts, regions as usize) }.iter().map(coordinate).collect::<Vec<_>>(),
        "sizes": unsafe { slice(sizes, regions as usize) }.iter().map(|s| {
            json!([s.NumTiles, s.UseBox.as_bool(), s.Width, s.Height, s.Depth])
        }).collect::<Vec<_>>(),
        "heap": heap,
        "ranges": ranges,
        "range_flags": if flags.is_null() { Value::Null } else { json!(flag_values) },
        "offsets": if offsets.is_null() { Value::Null } else { json!(offset_values) },
        "counts": if counts.is_null() { Value::Null } else { json!(count_values) },
        "flags": mapping.0,
    });
    journal::tiles_changing(&[id]);
    let after = match real(resource) {
        None => Err("no live resource".to_string()),
        Some(r) => with_entry(id, &r, |e| {
            let changed = unsafe { locations(&e.layout, regions, starts, sizes) }.and_then(|locations| {
                // Null counts: one range covers every location, several cover one tile each.
                let ranges: Vec<_> = (0..n)
                    .map(|i| Range {
                        flag: flag_values.get(i).copied().unwrap_or(0),
                        offset: offset_values.get(i).copied().unwrap_or(0),
                        count: count_values.get(i).copied().unwrap_or(if n == 1 {
                            locations.len() as u32
                        } else {
                            1
                        }),
                    })
                    .collect();
                e.pages.update(heap, &locations, &ranges)
            });
            settle(e, changed)
        })
        .and_then(|r| r),
    };
    journal::tile_mapping(queue.id, queue_seq, "UpdateTileMappings", id, None, args, after);
}

/// After the application's `CopyTileMappings` on `queue` (see [`update`]).
///
/// # Safety
/// The arguments are the call's, as the application passed them.
#[allow(clippy::too_many_arguments)]
pub unsafe fn copy(
    queue: &journal::Data,
    queue_seq: u64,
    dst: *mut c_void,
    dst_at: *const D3D12_TILED_RESOURCE_COORDINATE,
    src: *mut c_void,
    src_at: *const D3D12_TILED_RESOURCE_COORDINATE,
    size: *const D3D12_TILE_REGION_SIZE,
    mapping: D3D12_TILE_MAPPING_FLAGS,
) {
    if !journal::enabled() {
        return;
    }
    let (dst_id, src_id) = (super::id(dst), super::id(src));
    let (dst_at, src_at, size) = unsafe { (dst_at.as_ref(), src_at.as_ref(), size.as_ref()) };
    let args = json!({
        "resource": dst_id,
        "dst_at": dst_at.map(coordinate),
        "source": src_id,
        "src_at": src_at.map(coordinate),
        "size": size.map(|s| json!([s.NumTiles, s.UseBox.as_bool(), s.Width, s.Height, s.Depth])),
        "flags": mapping.0,
    });
    journal::tiles_changing(&[dst_id, src_id]);
    let after = (|| {
        let (dst, src) = (
            real(dst).ok_or("no live destination")?,
            real(src).ok_or("no live source")?,
        );
        let (dst_at, src_at, size) = (
            dst_at.ok_or("no destination coordinate")?,
            src_at.ok_or("no source coordinate")?,
            size.ok_or("no region size")?,
        );
        // The source's pages are read before the destination changes: they can be the same resource.
        let values = with_entry(src_id, &src, |e| match &e.error {
            Some(error) => Err(error.clone()),
            None => Ok(e
                .pages
                .at(&e.layout.region(Some(coordinate(src_at)), Some(region(size)))?)),
        })??;
        with_entry(dst_id, &dst, |e| {
            let changed = e
                .layout
                .region(Some(coordinate(dst_at)), Some(region(size)))
                .and_then(|to| e.pages.copy(&to, &values));
            settle(e, changed)
        })?
    })();
    journal::tile_mapping(
        queue.id,
        queue_seq,
        "CopyTileMappings",
        dst_id,
        Some(src_id),
        args,
        after,
    );
}

/// The page table after a change, or why it is not known: a change it could not follow makes it unknown for good.
fn settle(e: &mut Entry, changed: Result<(), String>) -> Result<Value, String> {
    if let Err(error) = changed {
        e.error.get_or_insert(error);
    }
    match &e.error {
        Some(error) => Err(error.clone()),
        None => Ok(json!(e.pages.runs())),
    }
}

/// Why a reserved resource's contents cannot be copied as a shader would read them, if they cannot: on tiled
/// resources tier 1 an unmapped tile reads undefined values.
pub fn unreadable(resource: u64) -> Option<String> {
    static TIERS: Mutex<Option<HashMap<usize, i32>>> = Mutex::new(None);
    let resource = super::real(resource).and_then(|r| r.cast::<ID3D12Resource>().ok())?;
    let mut device: Option<ID3D12Device> = None;
    unsafe { resource.GetDevice(&mut device) }.ok()?;
    let device = device?;
    let mut tiers = TIERS.lock().unwrap_or_else(|e| e.into_inner());
    let tier = *tiers
        .get_or_insert_with(HashMap::new)
        .entry(device.as_raw() as usize)
        .or_insert_with(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
            unsafe {
                device.CheckFeatureSupport(
                    D3D12_FEATURE_D3D12_OPTIONS,
                    (&raw mut options).cast(),
                    size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS>() as u32,
                )
            }
            .map_or(0, |()| options.TiledResourcesTier.0)
        });
    (tier < D3D12_TILED_RESOURCES_TIER_2.0).then(|| {
        format!("reserved resource on tiled resources tier {tier}: an unmapped tile reads undefined values")
    })
}
