//! Packed groups are stored as canonical mip texels, never as opaque pool bytes.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static GENERATION: AtomicU64 = AtomicU64::new(0);
// Packed bytes can outlive every mapping that exposed them. Keep this conservative provenance
// until pool destruction, including across captures and pool shrink/regrowth.
static PACKED_TILES: Mutex<BTreeMap<u64, BTreeSet<u32>>> = Mutex::new(BTreeMap::new());
pub(super) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}
pub(super) fn destroyed(id: u64) {
    PACKED_TILES.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
}
pub(super) fn mapped(entry: &Entry) {
    if entry.layout.packed[1] == 0 {
        return;
    }
    if let Some(pool) = entry.mappings.pool {
        let mut all = PACKED_TILES.lock().unwrap_or_else(|e| e.into_inner());
        let history = all.entry(pool).or_default();
        for slice in 0..entry.layout.array {
            if let Ok(range) = entry.layout.packed_tiles(slice) {
                history.extend(entry.mappings.tiles.range(range).map(|(_, &p)| p));
            }
        }
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Tile-specific copies of packed coordinates are undefined, even if their canonical mips were seeded.
pub(super) fn check_call(kind: &str, args: &Value) {
    let id = match kind {
        "tile_copy" => args["resource"].as_u64(),
        "tile_update" => args["dst"].as_u64(),
        _ => return,
    };
    let packed = {
        let all = mappings();
        id.and_then(|id| all.get(&id)).is_some_and(|entry| {
            entry
                .layout
                .region(location(&args["at"]), area(&args["size"]))
                .is_ok_and(|tiles| {
                    tiles.into_iter().any(|v| {
                        entry
                            .layout
                            .coordinate(v)
                            .is_ok_and(|at| entry.layout.is_packed(at[3]))
                    })
                })
        })
    };
    if packed {
        gap("CopyTiles/UpdateTiles accesses packed mip tiles; their raw layout is not replayable".into());
    }
}

/// Check persistent mappings, including packed resources not yet touched by this capture. Otherwise
/// a CopyTiles read through a standard alias could be mistaken for portable initial pool contents.
pub(super) fn check_aliases(id: u64, pool: u64, layout: &Layout, mapped: &[(u32, u32)]) {
    if pool == 0 || mapped.is_empty() {
        return;
    }
    let reads_old_packing = {
        let history = PACKED_TILES.lock().unwrap_or_else(|e| e.into_inner());
        history.get(&pool).is_some_and(|tiles| {
            mapped.iter().any(|&(v, p)| {
                tiles.contains(&p) && layout.coordinate(v).is_ok_and(|at| !layout.is_packed(at[3]))
            })
        })
    };
    let mut owners: BTreeMap<u32, Vec<(u64, u32)>> = BTreeMap::new();
    let alias = {
        let all = mappings();
        for (&other, entry) in all
            .iter()
            .filter(|(_, e)| e.mappings.pool == Some(pool) && e.layout.packed[1] != 0)
        {
            for slice in 0..entry.layout.array {
                if let Ok(range) = entry.layout.packed_tiles(slice) {
                    for (&v, &p) in entry.mappings.tiles.range(range) {
                        owners.entry(p).or_default().push((other, v));
                    }
                }
            }
        }
        let reads_other_packing = mapped.iter().any(|&(v, p)| {
            owners
                .get(&p)
                .is_some_and(|owners| owners.iter().any(|&owner| owner != (id, v)))
        });
        // Also reject a packed group overlapping a standard alias that already exists. Mere mapping
        // does not prove which alias supplied the bytes; support requires a separate verified case.
        let packed_here: BTreeSet<_> = mapped
            .iter()
            .filter_map(|&(v, p)| {
                layout
                    .coordinate(v)
                    .ok()
                    .filter(|at| layout.is_packed(at[3]))
                    .map(|_| p)
            })
            .collect();
        reads_old_packing
            || reads_other_packing
            || (!packed_here.is_empty()
                && all
                    .iter()
                    .filter(|(_, entry)| entry.mappings.pool == Some(pool))
                    .any(|(&other, entry)| {
                        entry.mappings.tiles.iter().any(|(&v, &p)| {
                            packed_here.contains(&p)
                                && (other != id
                                    || entry
                                        .layout
                                        .coordinate(v)
                                        .is_ok_and(|at| !entry.layout.is_packed(at[3])))
                        })
                    }))
    };
    if alias {
        gap("packed mip pool tiles have an unverified alias, including raw CopyTiles reads".into());
    }
}

pub(super) fn seed(
    c: &ID3D11DeviceContext4,
    resource: &ID3D11Resource,
    id: u64,
    pool: u64,
    layout: &Layout,
    mapped: &[(u32, u32)],
) {
    if layout.packed[1] == 0 {
        return;
    }
    for slice in 0..layout.array {
        let range = match layout.packed_tiles(slice) {
            Ok(range) => range,
            Err(error) => {
                gap(error);
                continue;
            }
        };
        let tiles: Vec<_> = range
            .clone()
            .filter_map(|v| {
                mapped
                    .binary_search_by_key(&v, |&(v, _)| v)
                    .ok()
                    .map(|at| mapped[at].1)
            })
            .collect();
        if tiles.is_empty() {
            continue;
        }
        if tiles.len() != range.len() || tiles.iter().collect::<BTreeSet<_>>().len() != tiles.len() {
            gap("packed mip group is partially mapped or aliases itself".into());
            continue;
        }
        let seeded = {
            let s = super::super::state();
            tiles
                .iter()
                .filter(|&&p| s.tile_seeded.contains(&(pool, p)))
                .count()
        };
        if seeded == tiles.len() {
            if !super::super::state()
                .tile_packed_seeded
                .contains(&(id, pool, slice, tiles.clone()))
            {
                gap(
                    "packed mip group reinterprets previously seeded pool tiles through a different mapping"
                        .into(),
                );
            }
            continue;
        }
        if seeded != 0 {
            gap("packed mip group partially overlaps previously seeded pool tiles".into());
            continue;
        }
        // Prepare the whole group before changing the timeline or seeded sets. A failed later mip
        // must not leave a partial seed prefix that the next touch would append again.
        let copies: Result<Vec<_>, _> = (layout.packed[0]..layout.mips)
            .map(|mip| {
                let sub = slice * layout.mips + mip;
                // Only this resident mip is copied; an unmapped standard prefix is untouched.
                unsafe { locum_d3d11::readback::issue_subresource(c, resource, sub) }
                    .map(|rb| (sub, rb))
                    .map_err(|error| format!("packed mip {sub} readback: {error}"))
            })
            .collect();
        let copies = match copies {
            Ok(copies) => copies,
            Err(error) => {
                gap(error);
                continue;
            }
        };
        let bytes = copies
            .iter()
            .flat_map(|(_, rb)| &rb.subs)
            .map(|s| s.size)
            .sum::<u64>();
        if super::super::state().in_flight_bytes + bytes > super::super::contents::IN_FLIGHT {
            super::super::read_in_flight(c);
        }
        let events: Vec<_> = copies
            .into_iter()
            .map(|(sub, rb)| {
                let mut event = new_event(
                    "tile_packed_seed",
                    "InitialPackedMipContents",
                    0,
                    false,
                    json!({"dst": id, "pool": pool, "sub": sub, "tiles": tiles}),
                );
                event.reads.push(id);
                event.writes.push(pool);
                event.data_readback = Some(rb);
                event
            })
            .collect();
        let mut s = super::super::state();
        s.in_flight_bytes += bytes;
        for event in events {
            let at = s.timeline.len();
            s.tile_seeds.push(at);
            s.timeline.push(event);
        }
        s.tile_seeded.extend(tiles.iter().map(|&p| (pool, p)));
        s.tile_packed_seeded.insert((id, pool, slice, tiles));
    }
}
