//! Memory the game shared between resources. Its heaps are made again from their descriptions; a placed resource is
//! made at its offset in the made heap, and a reserved resource reserved, its tiles mapped into the made heaps as the
//! frame found them (`d12_tile_pages`). The frame's changes to tile mappings (`d12_tile_mappings`) are run where the
//! CPU made them among the submissions, as replay reaches them. Such a resource's first-use contents are written when
//! replay reaches the submission they were copied at, not at the start: memory a resource shares holds, at each
//! point, what the frame's own copy saw there. The game's aliasing barriers, and its discards of render and depth
//! targets so placed (the operations that begin such a target's use), are run.
//!
//! A heap the capture does not describe (captures before heaps were described) is made from what the resources placed
//! in it record: the heap properties and flags GetHeapProperties gave the game for each, and a size that holds each at
//! its offset as this device lays it out, and each reserved tile mapped into it; replay says so.
//!
//! A heap is laid out for this device (see [`Relocation`]): where it takes more of the heap for a placed resource than
//! the game's GPU did, or another alignment, the heap is stretched to hold it, and every offset after moves with it.
//!
//! Where sharing cannot be done at all, the resource is made committed instead, and replay says why: a heap the
//! capture does not describe and no resource in it describes either, a resource this device refuses, a reserved
//! resource this device tiles otherwise, or one the capture holds no page table of.
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use locum_capture::tiles::{Layout, Pages};
use serde_json::Value;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::*;

use super::D3d12;
use crate::open::Capture;

/// How the game made a resource, from its description.
#[derive(Clone, Copy, Debug)]
pub(super) enum Placement {
    Committed,
    /// In heap `heap` at `offset`.
    Placed {
        heap: i64,
        offset: u64,
    },
    Reserved,
}

impl Placement {
    pub(super) fn of(d: &Value) -> Placement {
        match d["heap"].as_i64().filter(|h| *h != 0) {
            Some(heap) => Placement::Placed {
                heap,
                offset: d["heap_offset"].as_u64().unwrap_or(0),
            },
            // A reserved resource has no heap properties of its own.
            None if !d["heap_properties"].is_object() => Placement::Reserved,
            None => Placement::Committed,
        }
    }
}

/// A change to tile mappings the frame made, as recorded.
struct Mapping {
    cpu_seq: i64,
    call: String,
    resource: i64,
    source: Option<i64>,
    args: Value,
    pages: Option<String>,
}

/// A range of a game's heap that something takes: placed at `start`, ending at `end` in the game (where the capture
/// says), taking `bytes` from its start on this device, which must start it at a multiple of `alignment`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Span {
    pub(super) start: u64,
    pub(super) end: Option<u64>,
    pub(super) bytes: u64,
    pub(super) alignment: u64,
}

impl Span {
    /// Where it ends in the game: where the capture says, else where it ends on this device.
    fn game_end(&self) -> u64 {
        self.end.unwrap_or(self.start + self.bytes)
    }
}

/// Where the offsets of a game's heap are in the heap made again on this device. Each offset a span starts or ends
/// at keeps its order and its distance from the one before, except where a span takes more on this device than in
/// the game (the heap is stretched at its end to hold it) or must start at another alignment (stretched to that).
/// Nothing overlaps here that did not in the game, and on a device that lays everything out as the game's did every
/// offset is the game's. Two spans that overlapped in the game can stop overlapping where one is stretched past the
/// other's start; [`Relocation::lost`] counts them.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Relocation {
    /// (game offset, offset here), by game offset.
    points: Vec<(u64, u64)>,
    /// The heap's size here.
    pub(super) size: u64,
}

impl Relocation {
    /// Lays out `spans` of a heap of `size` bytes in the game, here, in a heap of `alignment`.
    pub(super) fn new(spans: &[Span], size: u64, alignment: u64) -> Relocation {
        // By offset: the alignment the spans starting there need, and (start, bytes here) of those ending there.
        let mut starts: HashMap<u64, u64> = HashMap::new();
        let mut ends: HashMap<u64, Vec<(u64, u64)>> = HashMap::new();
        let mut points = BTreeSet::from([0, size]);
        for s in spans.iter().filter(|s| s.game_end() > s.start) {
            let a = starts.entry(s.start).or_insert(1);
            *a = (*a).max(s.alignment);
            ends.entry(s.game_end()).or_default().push((s.start, s.bytes));
            points.insert(s.start);
            points.insert(s.game_end());
        }
        let mut out: Vec<(u64, u64)> = Vec::with_capacity(points.len());
        let mut here: HashMap<u64, u64> = HashMap::new();
        for p in points {
            let mut at = out.last().map_or(p, |&(q, r)| r + (p - q));
            for &(start, bytes) in ends.get(&p).into_iter().flatten() {
                at = at.max(here[&start] + bytes);
            }
            let at = at.next_multiple_of(starts.get(&p).copied().unwrap_or(1).max(1));
            here.insert(p, at);
            out.push((p, at));
        }
        let end = out.last().map_or(0, |&(_, r)| r);
        Relocation {
            points: out,
            size: end.next_multiple_of(alignment.max(1)),
        }
    }

    /// The offset here of the game's offset `game`.
    pub(super) fn at(&self, game: u64) -> u64 {
        match self.points.partition_point(|&(p, _)| p <= game) {
            0 => game,
            i => {
                let (p, r) = self.points[i - 1];
                r + (game - p)
            }
        }
    }

    /// The tile here of the game's tile `tile` (in 64 KiB units).
    pub(super) fn tile(&self, tile: u32) -> u32 {
        let tile_bytes = locum_capture::tiles::TILE_BYTES;
        (self.at(u64::from(tile) * tile_bytes) / tile_bytes) as u32
    }

    /// Whether any offset is not the game's.
    pub(super) fn moved(&self) -> bool {
        self.points.iter().any(|(p, r)| p != r)
    }

    /// How many pairs of `spans` overlap in the game and do not here.
    pub(super) fn lost(&self, spans: &[Span]) -> usize {
        let mut sorted: Vec<&Span> = spans.iter().filter(|s| s.game_end() > s.start).collect();
        sorted.sort_by_key(|s| s.start);
        let mut lost = 0;
        for (i, a) in sorted.iter().enumerate() {
            let at = self.at(a.start);
            for b in &sorted[i + 1..] {
                if b.start >= a.game_end() {
                    break;
                }
                let bt = self.at(b.start);
                if bt >= at + a.bytes || at >= bt + b.bytes {
                    lost += 1;
                }
            }
        }
        lost
    }
}

/// The description of a resource placed as `desc` says, as this device takes it, and what it takes: at the alignment
/// the game placed it with, or where this device refuses that (a small alignment), at the default. None if this
/// device refuses it.
fn allocation(
    device: &ID3D12Device,
    desc: &D3D12_RESOURCE_DESC,
) -> Option<(D3D12_RESOURCE_DESC, D3D12_RESOURCE_ALLOCATION_INFO)> {
    let info = unsafe { device.GetResourceAllocationInfo(0, &[*desc]) };
    if info.SizeInBytes != u64::MAX {
        return Some((*desc, info));
    }
    let default = D3D12_RESOURCE_DESC {
        Alignment: 0,
        ..*desc
    };
    let info = unsafe { device.GetResourceAllocationInfo(0, &[default]) };
    (desc.Alignment != 0 && info.SizeInBytes != u64::MAX).then_some((default, info))
}

/// Largest buffer width WARP's D3D12 implementation accepts for both reserved and committed buffers:
/// `65535 * 64 KiB` (`4 GiB - 64 KiB`). Exact `4 GiB` (`2^32`) returns `E_OUTOFMEMORY` (`0x8007000E`) on WARP.
pub(super) const MAX_WARP_BUFFER_BYTES: u64 = 65535 * locum_capture::tiles::TILE_BYTES;

/// On WARP, `CopyBufferRegion` on a reserved buffer fails with `DXGI_ERROR_DRIVER_INTERNAL_ERROR` (`0x887A0020`)
/// whenever `offset + len >= 2 GiB` (`0x8000_0000`). Writing/reading those tiles through a temporary placed buffer
/// alias in their backing heap keeps every `CopyBufferRegion` offset below 2 GiB.
pub(super) const WARP_COPY_LIMIT: u64 = 0x8000_0000;

/// One contiguous run of a reserved buffer's mapped tiles inside a backing heap, for placed-buffer aliasing.
pub(super) struct PlacedRun {
    pub(super) heap: ID3D12Heap,
    pub(super) heap_offset: u64,
    pub(super) width: u64,
    pub(super) at: u64,
    pub(super) slice_offset: u64,
    pub(super) len: u64,
}

#[derive(Default)]
pub(super) struct Memory {
    /// The game's heaps, made when a resource first needs one, with where its offsets are here; or why one could not
    /// be.
    heaps: HashMap<i64, Result<(ID3D12Heap, std::rc::Rc<Relocation>), String>>,
    /// Current mapped tile table for each reserved resource: resource id -> { resource tile -> (heap id, relocated heap tile) }.
    pub(super) tile_map: HashMap<i64, BTreeMap<u32, (i64, u32)>>,
    /// Planned resources for scoping page-table and heap-span lookups, when known.
    planned: Option<HashSet<i64>>,
    /// Cached per-heap mapped tile indices from `d12_tile_pages` and `d12_tile_mappings`, built once on first heap.
    heap_tiles: Option<HashMap<i64, BTreeSet<u32>>>,
    /// Reserved buffers clamped to a smaller tile count for this device (e.g. WARP's 4 GiB - 64 KiB limit):
    /// resource id -> actual tile count on this device.
    clamped_tiles: HashMap<i64, u32>,
    /// Count of reserved buffers clamped for this device, and mapped tiles past the clamp that could not be mapped.
    clamped_buffers: usize,
    dropped_tiles: usize,
    /// Heaps laid out otherwise than the game's, the placed resources and mapped tiles not at the game's offsets in
    /// them, and the pairs of those that shared memory in the game and do not here.
    relocated: usize,
    moved: usize,
    lost: usize,
    /// Resources made in a heap (placed, or reserved with their tiles mapped).
    pub(super) shared: HashSet<i64>,
    /// The tiles mapped of each reserved resource the frame does not map otherwise: only what they hold is written
    /// and read (see [`super::staging`]).
    pub(super) mapped: HashMap<i64, super::staging::Mapped>,
    /// First-use contents waiting for the submission they were copied at: by submission, (resource, blob key).
    pending: HashMap<i64, Vec<(i64, String)>>,
    /// The submission each resource's waiting contents belong to.
    waiting: HashMap<i64, i64>,
    /// Where those contents are.
    blobs: Option<locum_capture::blobs::Blobs>,
    /// The frame's changes to tile mappings in the order the CPU made them, and how many have been reached.
    mappings: Option<Vec<Mapping>>,
    reached: usize,
    pub(super) placed: usize,
    pub(super) reserved: usize,
    /// Resources made committed instead, by why.
    committed: BTreeMap<String, usize>,
    /// Heaps the capture does not describe, made from what their resources record.
    inferred: usize,
    /// Changes to tile mappings run, and those of resources made committed, which are not.
    pub(super) remaps: usize,
    pub(super) not_remapped: usize,
}

impl Memory {
    /// Limits page-table scans to the planned resources (and any resources sharing their heaps).
    pub(super) fn scope(&mut self, resources: &BTreeSet<i64>) {
        self.planned = Some(resources.iter().copied().collect());
        self.heap_tiles = None;
    }

    #[cfg(test)]
    pub(super) fn insert_test_heap(&mut self, id: i64, heap: ID3D12Heap) {
        self.heaps.insert(
            id,
            Ok((
                heap,
                std::rc::Rc::new(Relocation::new(&[], 0, locum_capture::tiles::TILE_BYTES)),
            )),
        );
    }

    /// Says what replay could not make as the game did.
    pub(super) fn warnings(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .committed
            .iter()
            .map(|(why, n)| {
                format!("{n} placed or reserved resources were made committed, memory the game shared not shared: {why}")
            })
            .collect();
        if self.inferred > 0 {
            out.push(format!(
                "{} heaps the capture does not describe (captured before heaps were described) were made from what \
                 the resources in them record: their heap properties and flags, and a size that holds each at its \
                 offset on this device",
                self.inferred
            ));
        }
        if self.not_remapped > 0 {
            out.push(format!(
                "the frame's {} changes to the tile mappings of reserved resources made committed are not run",
                self.not_remapped
            ));
        }
        if self.dropped_tiles > 0 {
            out.push(format!(
                "{} mapped tiles of clamped 4 GiB+ reserved buffers lay past this device's buffer width limit and \
                 were left unmapped",
                self.dropped_tiles
            ));
        }
        if self.lost > 0 {
            out.push(format!(
                "{} pairs of resources that shared memory in the game do not here: this device takes more of their \
                 heaps for one of them than the game's did, past where the other starts",
                self.lost
            ));
        }
        out
    }

    /// Says what replay did otherwise than the game and the same in effect.
    pub(super) fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.clamped_buffers > 0 {
            out.push(format!(
                "{} reserved buffers of 4 GiB+ were clamped to {} bytes ({} tiles) for this device",
                self.clamped_buffers,
                MAX_WARP_BUFFER_BYTES,
                MAX_WARP_BUFFER_BYTES / locum_capture::tiles::TILE_BYTES
            ));
        }
        if self.relocated > 0 {
            out.push(format!(
                "{} heaps were laid out again for this device, which takes more of them or another alignment for some of \
                 what is placed in them than the game's did: {} placed resources and mapped tiles are at other offsets, \
                 each sharing memory with what it shared in the game{}",
                self.relocated,
                self.moved,
                if self.lost > 0 {
                    " but for the pairs above"
                } else {
                    ""
                }
            ));
        }
        out
    }

    pub(super) fn fell_back(&mut self, why: String) {
        *self.committed.entry(why).or_default() += 1;
    }
}

/// A heap's recorded properties: its type, and a custom heap's CPU page property and memory pool.
fn heap_properties(p: &Value) -> D3D12_HEAP_PROPERTIES {
    let n = |v: &Value| v.as_i64().unwrap_or(0) as i32;
    D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE(n(&p["type"])),
        CPUPageProperty: D3D12_CPU_PAGE_PROPERTY(n(&p["cpu_page"])),
        MemoryPoolPreference: D3D12_MEMORY_POOL(n(&p["memory_pool"])),
        ..Default::default()
    }
}

/// A heap's recorded flags, those replay keeps: the deny flags and shader atomics; sharing, display and residency
/// flags are dropped.
fn heap_flags(v: &Value) -> D3D12_HEAP_FLAGS {
    const KEPT: i32 = 0x4 | 0x40 | 0x80 | 0x400;
    D3D12_HEAP_FLAGS(v.as_i64().unwrap_or(0) as i32 & KEPT)
}

/// Every page table the capture holds of a reserved resource, as the frame found it and after each of its changes,
/// with the resource's id. When `only` is `Some`, only rows for resources in `only` are parsed (skipping tens of
/// thousands of unreferenced reserved resources in large captures).
fn page_tables(c: &Capture, only: Option<&HashSet<i64>>) -> Vec<(i64, Pages)> {
    let mut tables: Vec<(i64, String)> = Vec::new();
    for table in ["d12_tile_pages", "d12_tile_mappings"] {
        if !c.has_table(table) {
            continue;
        }
        if let Some(only) = only
            && only.len() <= 512
        {
            for &id in only {
                tables.extend(c.rows(
                    &format!("SELECT resource, pages_json FROM {table} WHERE resource = ?1 AND pages_json IS NOT NULL"),
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                ));
            }
        } else {
            tables.extend(
                c.rows(
                    &format!("SELECT resource, pages_json FROM {table} WHERE pages_json IS NOT NULL"),
                    [],
                    |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                )
                .into_iter()
                .filter(|(id, _)| only.is_none_or(|o| o.contains(id))),
            );
        }
    }
    tables
        .into_iter()
        .filter_map(|(id, json)| {
            let pages = serde_json::from_str(&json)
                .ok()
                .and_then(|v| Pages::from_runs(&v))?;
            Some((id, pages))
        })
        .collect()
}

/// An UpdateTileMappings' heap ranges (`flags`, starting tiles `at`, tile `counts`) as they are here: each range's
/// start where `relocation` keeps its tiles consecutive, else the range one tile at a time. Ranges with no heap tiles
/// of their own (NULL, SKIP) are as they were; a REUSE_SINGLE_TILE range's one tile is where it is here.
fn ranges_here(
    relocation: &Relocation,
    flags: Option<&[D3D12_TILE_RANGE_FLAGS]>,
    at: &[u32],
    counts: &[u32],
) -> (Vec<D3D12_TILE_RANGE_FLAGS>, Vec<u32>, Vec<u32>) {
    // (flag, first tile, tiles) a range.
    let mut out: Vec<(D3D12_TILE_RANGE_FLAGS, u32, u32)> = Vec::new();
    for (i, (&start, &count)) in at.iter().zip(counts).enumerate() {
        let flag = flags
            .and_then(|f| f.get(i).copied())
            .unwrap_or(D3D12_TILE_RANGE_FLAG_NONE);
        if flag.0 & (D3D12_TILE_RANGE_FLAG_NULL.0 | D3D12_TILE_RANGE_FLAG_SKIP.0) != 0 {
            out.push((flag, start, count));
        } else if flag.0 & D3D12_TILE_RANGE_FLAG_REUSE_SINGLE_TILE.0 != 0
            || (0..count).all(|k| relocation.tile(start + k) == relocation.tile(start) + k)
        {
            out.push((flag, relocation.tile(start), count));
        } else {
            out.extend((0..count).map(|k| (flag, relocation.tile(start + k), 1)));
        }
    }
    let f = out.iter().map(|r| r.0).collect();
    let o = out.iter().map(|r| r.1).collect();
    let t = out.iter().map(|r| r.2).collect();
    (f, o, t)
}

/// A recorded tile coordinate, `[x, y, z, subresource]`.
fn coordinate(v: &Value) -> D3D12_TILED_RESOURCE_COORDINATE {
    let n = |i: usize| v[i].as_u64().unwrap_or(0) as u32;
    D3D12_TILED_RESOURCE_COORDINATE {
        X: n(0),
        Y: n(1),
        Z: n(2),
        Subresource: n(3),
    }
}

/// A recorded region size, `[tiles, use box, width, height, depth]`.
fn size(v: &Value) -> D3D12_TILE_REGION_SIZE {
    let n = |i: usize| v[i].as_u64().unwrap_or(0) as u32;
    D3D12_TILE_REGION_SIZE {
        NumTiles: n(0),
        UseBox: v[1].as_bool().unwrap_or(false).into(),
        Width: n(2),
        Height: n(3) as u16,
        Depth: n(4) as u16,
    }
}

fn array<T>(v: &Value, f: impl Fn(&Value) -> T) -> Option<Vec<T>> {
    v.as_array().map(|v| v.iter().map(f).collect())
}

impl D3d12 {
    /// Cached per-heap mapped tile indices from `d12_tile_pages` / `d12_tile_mappings`, built once on first heap.
    fn mapped_tiles_in_heap(&mut self, c: &Capture, id: i64) -> BTreeSet<u32> {
        let by_heap = self.memory.heap_tiles.get_or_insert_with(|| {
            let mut map: HashMap<i64, BTreeSet<u32>> = HashMap::new();
            for (_, pages) in page_tables(c, self.memory.planned.as_ref()) {
                for &(heap, at) in pages.tiles.values() {
                    map.entry(heap as i64).or_default().insert(at);
                }
            }
            map
        });
        by_heap.get(&id).cloned().unwrap_or_default()
    }

    /// The game's heap `id`, made from its description on first use, laid out for this device: with where the game's
    /// offsets in it are here.
    fn game_heap(&mut self, c: &Capture, id: i64) -> Result<(ID3D12Heap, std::rc::Rc<Relocation>), String> {
        if !self.memory.heaps.contains_key(&id) {
            let heap_tiles = self.mapped_tiles_in_heap(c, id);
            let made = self
                .heap_desc(c, id, &heap_tiles)
                .and_then(|(mut desc, inferred)| {
                    let spans = self.spans(c, id, &heap_tiles);
                    // A resource this device aligns at 4 MiB (a multisampled one) needs a heap aligned so.
                    let widest = spans.iter().map(|s| s.alignment).max().unwrap_or(0);
                    desc.Alignment = desc.Alignment.max(widest).max(64 << 10);
                    let relocation = Relocation::new(&spans, desc.SizeInBytes, desc.Alignment);
                    let moved = spans.iter().filter(|s| relocation.at(s.start) != s.start).count();
                    let lost = relocation.lost(&spans);
                    desc.SizeInBytes = relocation.size;
                    let mut heap: Option<ID3D12Heap> = None;
                    let created = unsafe { self.device.CreateHeap(&desc, &mut heap) }.or_else(|e| {
                        // D3D12_HEAP_TYPE_GPU_UPLOAD (5) is not supported on older devices/WARP: fall back to DEFAULT.
                        if desc.Properties.Type.0 == 5 {
                            let fallback = D3D12_HEAP_DESC {
                                Properties: D3D12_HEAP_PROPERTIES {
                                    Type: D3D12_HEAP_TYPE_DEFAULT,
                                    ..Default::default()
                                },
                                ..desc
                            };
                            unsafe { self.device.CreateHeap(&fallback, &mut heap) }
                        } else {
                            Err(e)
                        }
                    });
                    created.map_err(|e| format!("their heap could not be made on this device: {e}"))?;
                    let heap = heap.ok_or("CreateHeap gave no heap")?;
                    super::name(&heap, format!("#{id}"));
                    Ok((heap, relocation, inferred, moved, lost))
                });
            let made = made.map(|(heap, relocation, inferred, moved, lost)| {
                self.memory.inferred += usize::from(inferred);
                if relocation.moved() {
                    self.memory.relocated += 1;
                    self.memory.moved += moved;
                    self.memory.lost += lost;
                }
                (heap, std::rc::Rc::new(relocation))
            });
            self.memory.heaps.insert(id, made);
        }
        self.memory.heaps[&id].clone()
    }

    /// What is placed in, or mapped into, the game's heap `id`: each placed resource this device takes (at the
    /// alignment [`allocation`] gives), and each tile a reserved resource maps there as the frame found it or after
    /// any of its changes.
    fn spans(&self, c: &Capture, id: i64, tiles: &BTreeSet<u32>) -> Vec<Span> {
        let mut spans: Vec<Span> = c
            .rows(
                "SELECT id, description FROM d12_objects WHERE kind LIKE 'ID3D12Resource%' \
                 AND json_extract(description, '$.heap') = ?1",
                [id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .iter()
            .filter(|(rid, _)| self.memory.planned.as_ref().is_none_or(|p| p.contains(rid)))
            .filter_map(|(_, d)| serde_json::from_str::<Value>(d).ok())
            .filter_map(|d| {
                let (_, info) = allocation(&self.device, &super::resource_desc(&d))?;
                let start = d["heap_offset"].as_u64().unwrap_or(0);
                Some(Span {
                    start,
                    end: d["bytes"].as_u64().map(|b| start + b),
                    bytes: info.SizeInBytes,
                    alignment: info.Alignment,
                })
            })
            .collect();
        let tile = locum_capture::tiles::TILE_BYTES;
        spans.extend(tiles.iter().map(|&at| Span {
            start: u64::from(at) * tile,
            end: Some((u64::from(at) + 1) * tile),
            bytes: tile,
            alignment: tile,
        }));
        spans
    }

    /// The description of the game's heap `id`, and whether it was inferred: the capture's, or where it has none
    /// (captures before heaps were described), [`D3d12::inferred_heap`].
    fn heap_desc(
        &self,
        c: &Capture,
        id: i64,
        tiles: &BTreeSet<u32>,
    ) -> Result<(D3D12_HEAP_DESC, bool), String> {
        let d: Value = c
            .row(
                "SELECT description FROM d12_objects WHERE id = ?1 AND kind LIKE 'ID3D12Heap%'",
                [id],
                |r| r.get::<_, String>(0),
            )
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or(Value::Null);
        let Some(bytes) = d["bytes"].as_u64() else {
            return self.inferred_heap(c, id, tiles).map(|desc| (desc, true));
        };
        let p = &d["heap_properties"];
        Ok((
            D3D12_HEAP_DESC {
                SizeInBytes: bytes,
                Properties: heap_properties(p),
                Alignment: d["alignment"].as_u64().unwrap_or(0),
                Flags: heap_flags(&d["flags"]),
            },
            false,
        ))
    }

    /// What heap `id` must have been, from what the resources in it record: the heap properties and flags
    /// GetHeapProperties gave the game for each placed one (which must agree), and a size that holds each placed one
    /// at its offset as this device lays it out (or as the game did, where the capture says and it is more) and each
    /// reserved tile mapped into it, in whole units of its alignment: 4 MiB with a multisampled resource in it, else
    /// 64 KiB.
    fn inferred_heap(&self, c: &Capture, id: i64, tiles: &BTreeSet<u32>) -> Result<D3D12_HEAP_DESC, String> {
        const UNDESCRIBED: &str = "the capture does not describe their heap (captured before heaps were described), nor does any resource \
             placed in it";
        let placed: Vec<Value> = c
            .rows(
                "SELECT description FROM d12_objects WHERE kind LIKE 'ID3D12Resource%' \
                 AND json_extract(description, '$.heap') = ?1",
                [id],
                |r| r.get::<_, String>(0),
            )
            .iter()
            .filter_map(|d| serde_json::from_str(d).ok())
            .collect();
        let said: BTreeSet<String> = placed.iter().map(|d| d["heap_properties"].to_string()).collect();
        let p = match (placed.first(), said.len()) {
            (Some(d), 1) if d["heap_properties"].is_object() => &d["heap_properties"],
            (_, 0 | 1) => return Err(UNDESCRIBED.into()),
            _ => {
                return Err(
                    "the capture does not describe their heap, and the resources placed in it say different heap \
                     properties"
                        .into(),
                )
            }
        };
        let mut end = 0u64;
        let mut multisampled = false;
        for d in &placed {
            let desc = super::resource_desc(d);
            multisampled |= desc.SampleDesc.Count > 1;
            // One this device refuses is made committed when it is reached; it holds nothing of the heap here.
            let Some((_, info)) = allocation(&self.device, &desc) else {
                continue;
            };
            let bytes = info.SizeInBytes.max(d["bytes"].as_u64().unwrap_or(0));
            end = end.max(d["heap_offset"].as_u64().unwrap_or(0) + bytes);
        }
        if let Some(&max_tile) = tiles.last() {
            end = end.max((u64::from(max_tile) + 1) * locum_capture::tiles::TILE_BYTES);
        }
        let alignment: u64 = if multisampled { 4 << 20 } else { 64 << 10 };
        Ok(D3D12_HEAP_DESC {
            SizeInBytes: end.max(1).next_multiple_of(alignment),
            Properties: heap_properties(p),
            Alignment: alignment,
            Flags: heap_flags(&p["flags"]),
        })
    }

    /// Makes resource `id` in the game's memory as `placement` says, from `desc` (the game's own flags), in `state`.
    pub(super) fn make_shared(
        &mut self,
        c: &Capture,
        id: i64,
        placement: Placement,
        desc: &D3D12_RESOURCE_DESC,
        state: D3D12_RESOURCE_STATES,
    ) -> Result<ID3D12Resource, String> {
        let mut r: Option<ID3D12Resource> = None;
        match placement {
            Placement::Committed => return Err("committed".into()),
            Placement::Placed { heap, offset } => {
                let (h, relocation) = self.game_heap(c, heap)?;
                let (desc, info) =
                    allocation(&self.device, desc).ok_or("this device refuses their descriptions")?;
                // The heap was laid out for what this device takes of each: the resource fits where it is here.
                let at = relocation.at(offset);
                if info.Alignment == 0 || at % info.Alignment != 0 || at + info.SizeInBytes > relocation.size
                {
                    return Err("their offsets do not fit their heaps on this device".into());
                }
                unsafe {
                    self.device
                        .CreatePlacedResource(&h, at, &desc, state, None, &mut r)
                }
                .map_err(|e| format!("CreatePlacedResource refused them: {e}"))?;
                self.memory.placed += 1;
            }
            Placement::Reserved => {
                if !c.has_table("d12_tile_pages") {
                    return Err(
                        "the capture holds no page table of them (captured before they were followed)".into(),
                    );
                }
                let (layout, pages): (String, String) = c
                    .row(
                        "SELECT layout_json, pages_json FROM d12_tile_pages WHERE resource = ?1 \
                         AND layout_json IS NOT NULL AND pages_json IS NOT NULL",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .ok_or("the capture holds no page table of them")?;
                let recorded = serde_json::from_str(&layout)
                    .ok()
                    .and_then(|v| Layout::from_json(&v))
                    .ok_or("their page tables do not read")?;
                let pages = serde_json::from_str(&pages)
                    .ok()
                    .and_then(|v| Pages::from_runs(&v))
                    .ok_or("their page tables do not read")?;
                let mut clamped = false;
                let make_desc = if self.warp
                    && desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER
                    && desc.Width > MAX_WARP_BUFFER_BYTES
                {
                    clamped = true;
                    D3D12_RESOURCE_DESC {
                        Width: MAX_WARP_BUFFER_BYTES,
                        ..*desc
                    }
                } else {
                    *desc
                };
                let created = unsafe {
                    self.device
                        .CreateReservedResource(&make_desc, state, None, &mut r)
                };
                if let Err(first_err) = created {
                    if !clamped
                        && desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER
                        && desc.Width > MAX_WARP_BUFFER_BYTES
                    {
                        let clamped_desc = D3D12_RESOURCE_DESC {
                            Width: MAX_WARP_BUFFER_BYTES,
                            ..*desc
                        };
                        unsafe {
                            self.device
                                .CreateReservedResource(&clamped_desc, state, None, &mut r)
                        }
                        .map_err(|e| format!("CreateReservedResource refused them (and clamped): {e}"))?;
                        clamped = true;
                    } else {
                        return Err(format!("CreateReservedResource refused them: {first_err}"));
                    }
                }
                let resource = r.as_ref().ok_or("CreateReservedResource gave no resource")?;
                let layout = locum_capture::tiles::d3d12(resource)?;
                let compatible = if clamped {
                    layout.mips == recorded.mips
                        && layout.array == recorded.array
                        && layout.packed == recorded.packed
                        && layout.shape == recorded.shape
                        && layout.total <= recorded.total
                } else {
                    layout == recorded
                };
                if !compatible {
                    return Err("this device tiles them otherwise than the game's".into());
                }
                if clamped {
                    self.memory.clamped_buffers += 1;
                    self.memory.clamped_tiles.insert(id, layout.total);
                }
                // Every mapped tile within this device's buffer width, coalesced into contiguous runs per heap
                // for buffers (a 4 GiB reserved buffer can have 65,535 tiles across 3 contiguous heap runs).
                #[derive(Default)]
                struct HeapRuns {
                    starts: Vec<D3D12_TILED_RESOURCE_COORDINATE>,
                    sizes: Vec<D3D12_TILE_REGION_SIZE>,
                    offsets: Vec<u32>,
                    counts: Vec<u32>,
                }
                let is_buffer = desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER;
                let mut by_heap: BTreeMap<i64, HeapRuns> = BTreeMap::new();
                let mut mapped_tiles = BTreeSet::new();
                for (&tile, &(heap, at)) in &pages.tiles {
                    if tile >= layout.total {
                        self.memory.dropped_tiles += 1;
                        continue;
                    }
                    let [x, y, z, sub] = layout.coordinate(tile)?;
                    let e = by_heap.entry(heap as i64).or_default();
                    let can_extend = is_buffer
                        && e.starts.last().is_some_and(|prev| {
                            let prev_count = e.sizes.last().map_or(0, |s| s.NumTiles);
                            let prev_off = e.offsets.last().copied().unwrap_or(0);
                            prev.Subresource == sub && prev.X + prev_count == x && prev_off + prev_count == at
                        });
                    if can_extend {
                        e.sizes.last_mut().expect("region").NumTiles += 1;
                        *e.counts.last_mut().expect("count") += 1;
                    } else {
                        e.starts.push(D3D12_TILED_RESOURCE_COORDINATE {
                            X: x,
                            Y: y,
                            Z: z,
                            Subresource: sub,
                        });
                        e.sizes.push(D3D12_TILE_REGION_SIZE {
                            NumTiles: 1,
                            UseBox: false.into(),
                            Width: 0,
                            Height: 0,
                            Depth: 0,
                        });
                        e.offsets.push(at);
                        e.counts.push(1);
                    }
                    mapped_tiles.insert(tile);
                }
                let mut current_tiles: BTreeMap<u32, (i64, u32)> = BTreeMap::new();
                for (heap, runs) in by_heap {
                    let HeapRuns {
                        starts,
                        sizes,
                        offsets,
                        counts,
                    } = runs;
                    let (h, relocation) = self.game_heap(c, heap)?;
                    // Re-split any run whose heap tiles were separated by relocation.
                    let (mut r_starts, mut r_sizes, mut r_offsets, mut r_counts) =
                        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                    for ((start, sz), (off, cnt)) in
                        starts.into_iter().zip(sizes).zip(offsets.into_iter().zip(counts))
                    {
                        if is_buffer {
                            for k in 0..cnt {
                                current_tiles.insert(start.X + k, (heap, relocation.tile(off + k)));
                            }
                        }
                        if !relocation.moved() || cnt == 1 {
                            r_starts.push(start);
                            r_sizes.push(sz);
                            r_offsets.push(relocation.tile(off));
                            r_counts.push(cnt);
                        } else {
                            for k in 0..cnt {
                                r_starts.push(D3D12_TILED_RESOURCE_COORDINATE {
                                    X: start.X + k,
                                    ..start
                                });
                                r_sizes.push(D3D12_TILE_REGION_SIZE {
                                    NumTiles: 1,
                                    UseBox: false.into(),
                                    Width: 0,
                                    Height: 0,
                                    Depth: 0,
                                });
                                r_offsets.push(relocation.tile(off + k));
                                r_counts.push(1);
                            }
                        }
                    }
                    unsafe {
                        self.queue.UpdateTileMappings(
                            resource,
                            r_starts.len() as u32,
                            Some(r_starts.as_ptr()),
                            Some(r_sizes.as_ptr()),
                            &h,
                            r_offsets.len() as u32,
                            None,
                            Some(r_offsets.as_ptr()),
                            Some(r_counts.as_ptr()),
                            D3D12_TILE_MAPPING_FLAG_NONE,
                        )
                    };
                }
                if is_buffer {
                    self.memory.tile_map.insert(id, current_tiles);
                }
                self.memory.reserved += 1;
                let remapped = c.has_table("d12_tile_mappings")
                    && c.row(
                        "SELECT 1 FROM d12_tile_mappings WHERE resource = ?1 LIMIT 1",
                        [id],
                        |r| r.get::<_, i64>(0),
                    )
                    .is_some();
                if !remapped {
                    self.memory.mapped.insert(
                        id,
                        super::staging::Mapped {
                            layout,
                            tiles: mapped_tiles,
                        },
                    );
                }
            }
        }
        self.memory.shared.insert(id);
        r.ok_or_else(|| "no resource made".into())
    }

    /// Holds back resource `id`'s first-use contents for the submission they were copied at; false if the capture
    /// does not say which (they are written now).
    pub(super) fn defer(&mut self, c: &Capture, id: i64) -> bool {
        let found: Option<(i64, String)> = c.row(
            "SELECT f.submission, r.content FROM d12_first_use f JOIN resources r ON r.id = f.resource \
             WHERE f.resource = ?1 AND r.content IS NOT NULL",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        );
        let Some((submission, key)) = found else {
            return false;
        };
        self.memory.pending.entry(submission).or_default().push((id, key));
        self.memory.waiting.insert(id, submission);
        self.memory.blobs.get_or_insert_with(|| c.blobs.clone());
        true
    }

    /// Writes the first-use contents copied at `submission`.
    pub(super) fn first_uses(&mut self, submission: i64) -> Result<(), String> {
        for (id, key) in self.memory.pending.remove(&submission).unwrap_or_default() {
            self.memory.waiting.remove(&id);
            let blobs = self.memory.blobs.clone().ok_or("no blob store")?;
            self.put_blob_from(&blobs, id, &key)
                .map_err(|e| format!("first-use #{id} (submission {submission}): {e}"))?;
        }
        Ok(())
    }

    /// Writes resource `id`'s waiting first-use contents now: they are what it held as the frame began, for a read
    /// before replay reaches them.
    pub(super) fn first_use_now(&mut self, id: i64) -> Result<(), String> {
        let Some(submission) = self.memory.waiting.remove(&id) else {
            return Ok(());
        };
        let list = self.memory.pending.entry(submission).or_default();
        let Some(i) = list.iter().position(|(r, _)| *r == id) else {
            return Ok(());
        };
        let (_, key) = list.remove(i);
        let blobs = self.memory.blobs.clone().ok_or("no blob store")?;
        self.put_blob_from(&blobs, id, &key)
    }

    /// Runs the frame's changes to tile mappings the CPU made before `cpu_seq`, in order.
    pub(super) fn remap_before(&mut self, c: &Capture, cpu_seq: i64) -> Result<(), String> {
        let followed = c.has_table("d12_tile_mappings");
        let mappings = self.memory.mappings.get_or_insert_with(|| {
            if !followed {
                return Vec::new();
            }
            c.rows(
                "SELECT cpu_seq, call, resource, source, args_json, pages_json FROM d12_tile_mappings \
                 ORDER BY cpu_seq, id",
                [],
                |r| {
                    Ok(Mapping {
                        cpu_seq: r.get(0)?,
                        call: r.get(1)?,
                        resource: r.get(2)?,
                        source: r.get(3)?,
                        args: r
                            .get::<_, Option<String>>(4)?
                            .and_then(|a| serde_json::from_str(&a).ok())
                            .unwrap_or(Value::Null),
                        pages: r.get(5)?,
                    })
                },
            )
        });
        let due: Vec<usize> = (self.memory.reached..mappings.len())
            .take_while(|&i| mappings[i].cpu_seq < cpu_seq)
            .collect();
        for i in due {
            self.memory.reached = i + 1;
            let m = &self.memory.mappings.as_ref().expect("loaded")[i];
            let (call, resource, source, a, pages) = (
                m.call.clone(),
                m.resource,
                m.source,
                m.args.clone(),
                m.pages.clone(),
            );
            let shared = |id: i64| self.memory.shared.contains(&id) && self.resources.contains_key(&id);
            if !shared(resource) || source.is_some_and(|s| !shared(s)) {
                // A resource the plan does not make needs none of its changes; one made committed cannot take them.
                if self.resources.contains_key(&resource) {
                    self.memory.not_remapped += 1;
                }
                continue;
            }
            self.remap(c, &call, resource, source, &a)
                .map_err(|e| format!("{call} of #{resource}: {e}"))?;
            if let Some(pages_str) = pages
                && self.memory.tile_map.contains_key(&resource)
                && let Some(parsed) = serde_json::from_str(&pages_str)
                    .ok()
                    .and_then(|v| Pages::from_runs(&v))
            {
                let limit = self
                    .memory
                    .clamped_tiles
                    .get(&resource)
                    .copied()
                    .unwrap_or(u32::MAX);
                let mut updated = BTreeMap::new();
                for (&tile, &(heap, at)) in &parsed.tiles {
                    if tile < limit
                        && let Some(Ok((_, rel))) = self.memory.heaps.get(&(heap as i64))
                    {
                        updated.insert(tile, (heap as i64, rel.tile(at)));
                    }
                }
                self.memory.tile_map.insert(resource, updated);
            }
            self.memory.remaps += 1;
        }
        Ok(())
    }

    /// Breaks bytes `offset..offset+len` of reserved buffer `id` into contiguous backing-heap runs so WARP can
    /// copy to/from tiles `>= 2 GiB` via placed buffer aliases without hitting WARP's 2 GiB `CopyBufferRegion` bug.
    pub(super) fn placed_alias_runs(&self, id: i64, offset: u64, len: u64) -> Option<Vec<PlacedRun>> {
        let map = self.memory.tile_map.get(&id)?;
        if len == 0 {
            return Some(Vec::new());
        }
        let tile = locum_capture::tiles::TILE_BYTES;
        let first_tile = (offset / tile) as u32;
        let last_tile = ((offset + len - 1) / tile) as u32;
        let mut runs: Vec<(i64, u32, u64, PlacedRun)> = Vec::new();
        for t in first_tile..=last_tile {
            let Some(&(heap_id, heap_tile)) = map.get(&t) else {
                continue;
            };
            let b0 = (u64::from(t) * tile).max(offset);
            let b1 = ((u64::from(t) + 1) * tile).min(offset + len);
            let can_extend = runs.last().is_some_and(|(prev_heap, next_ht, prev_end, run)| {
                *prev_heap == heap_id
                    && *next_ht == heap_tile
                    && *prev_end == b0
                    && run.width + tile <= self.limit
            });
            if can_extend {
                let (_, next_ht, prev_end, run) = runs.last_mut().expect("last");
                *next_ht += 1;
                *prev_end = b1;
                run.width += tile;
                run.len += b1 - b0;
            } else {
                let heap = self.memory.heaps.get(&heap_id)?.as_ref().ok()?.0.clone();
                runs.push((
                    heap_id,
                    heap_tile + 1,
                    b1,
                    PlacedRun {
                        heap,
                        heap_offset: u64::from(heap_tile) * tile,
                        width: tile,
                        at: b0 - u64::from(t) * tile,
                        slice_offset: b0 - offset,
                        len: b1 - b0,
                    },
                ));
            }
        }
        Some(runs.into_iter().map(|(_, _, _, r)| r).collect())
    }

    fn remap(
        &mut self,
        c: &Capture,
        call: &str,
        resource: i64,
        source: Option<i64>,
        a: &Value,
    ) -> Result<(), String> {
        let r = self.resource(resource)?;
        let n = |v: &Value| v.as_u64().unwrap_or(0) as u32;
        match call {
            "UpdateTileMappings" => {
                let heap = match a["heap"].as_i64().filter(|h| *h != 0) {
                    Some(h) => Some(self.game_heap(c, h)?),
                    None => None,
                };
                let starts = array(&a["starts"], coordinate);
                let sizes = array(&a["sizes"], size);
                // If this reserved buffer was clamped on this device, skip any remap targeting tiles >= limit.
                if let Some(&limit) = self.memory.clamped_tiles.get(&resource) {
                    let oob = starts.as_ref().is_some_and(|st| {
                        st.iter().enumerate().any(|(i, coord)| {
                            let count = sizes
                                .as_ref()
                                .and_then(|sz| sz.get(i))
                                .map_or(1, |s| s.NumTiles.max(1));
                            coord.X.saturating_add(count) > limit
                        })
                    });
                    if oob {
                        self.memory.dropped_tiles += 1;
                        return Ok(());
                    }
                }
                let mut flags = array(&a["range_flags"], |v| D3D12_TILE_RANGE_FLAGS(n(v) as i32));
                let mut offsets = array(&a["offsets"], n);
                // Null counts mean one tile a range when there are several: spelled out, as WARP needs them.
                let mut ranges = n(&a["ranges"]);
                let mut counts =
                    array(&a["counts"], n).or_else(|| (ranges > 1).then(|| vec![1; ranges as usize]));
                // The heap's tiles where they are here.
                if let (Some((_, relocation)), Some(at)) = (&heap, &offsets) {
                    let regions = n(&a["regions"]);
                    let tiles = counts.clone().unwrap_or_else(|| {
                        // One range and no counts: every tile the regions name.
                        vec![
                            sizes
                                .as_ref()
                                .map_or(regions, |s| s.iter().map(|s| s.NumTiles).sum()),
                        ]
                    });
                    let (f, o, t) = ranges_here(relocation, flags.as_deref(), at, &tiles);
                    ranges = o.len() as u32;
                    (flags, offsets, counts) = (Some(f), Some(o), Some(t));
                }
                let heap = heap.map(|(h, _)| h);
                unsafe {
                    self.queue.UpdateTileMappings(
                        &r,
                        n(&a["regions"]),
                        starts.as_ref().map(|v| v.as_ptr()),
                        sizes.as_ref().map(|v| v.as_ptr()),
                        heap.as_ref(),
                        ranges,
                        flags.as_ref().map(|v| v.as_ptr()),
                        offsets.as_ref().map(|v| v.as_ptr()),
                        counts.as_ref().map(|v| v.as_ptr()),
                        D3D12_TILE_MAPPING_FLAGS(n(&a["flags"]) as i32),
                    )
                };
            }
            "CopyTileMappings" => {
                let src_id = source.ok_or("no source")?;
                let s = self.resource(src_id)?;
                let dst_at = coordinate(&a["dst_at"]);
                let src_at = coordinate(&a["src_at"]);
                let sz = size(&a["size"]);
                let count = sz.NumTiles.max(1);
                if self
                    .memory
                    .clamped_tiles
                    .get(&resource)
                    .is_some_and(|&lim| dst_at.X.saturating_add(count) > lim)
                    || self
                        .memory
                        .clamped_tiles
                        .get(&src_id)
                        .is_some_and(|&lim| src_at.X.saturating_add(count) > lim)
                {
                    self.memory.dropped_tiles += 1;
                    return Ok(());
                }
                unsafe {
                    self.queue.CopyTileMappings(
                        &r,
                        &dst_at,
                        &s,
                        &src_at,
                        &sz,
                        D3D12_TILE_MAPPING_FLAGS(n(&a["flags"]) as i32),
                    )
                };
            }
            other => return Err(format!("unknown tile mapping call {other}")),
        }
        Ok(())
    }

    /// The game's aliasing barriers among the resources made in its memory.
    pub(super) fn aliasing(&mut self, a: &Value) {
        let shared = |id: i64| {
            self.memory
                .shared
                .contains(&id)
                .then(|| self.resources.get(&id).map(|m| m.resource.clone()))
                .flatten()
        };
        let barriers: Vec<D3D12_RESOURCE_BARRIER> = a[0]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"].as_i64() == Some(D3D12_RESOURCE_BARRIER_TYPE_ALIASING.0 as i64))
            .filter_map(|b| {
                let p = &b["payload"];
                let (before, after) = (
                    shared(p["before"].as_i64().unwrap_or(0)),
                    shared(p["after"].as_i64().unwrap_or(0)),
                );
                (before.is_some() || after.is_some()).then(|| D3D12_RESOURCE_BARRIER {
                    Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
                    Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                    Anonymous: D3D12_RESOURCE_BARRIER_0 {
                        Aliasing: std::mem::ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                            pResourceBefore: std::mem::ManuallyDrop::new(before),
                            pResourceAfter: std::mem::ManuallyDrop::new(after),
                        }),
                    },
                })
            })
            .collect();
        if barriers.is_empty() {
            return;
        }
        unsafe { self.list.ResourceBarrier(&barriers) };
        for mut b in barriers {
            unsafe {
                let a = &mut b.Anonymous.Aliasing;
                std::mem::ManuallyDrop::drop(&mut a.pResourceBefore);
                std::mem::ManuallyDrop::drop(&mut a.pResourceAfter);
            }
        }
    }

    /// The game's discard of a render or depth target made in its memory: what begins its use there.
    pub(super) fn discard(&mut self, a: &Value) {
        let id = a[0].as_i64().unwrap_or(0);
        let Some(m) = self
            .memory
            .shared
            .contains(&id)
            .then(|| self.resources.get(&id))
            .flatten()
        else {
            return;
        };
        // D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET, ALLOW_DEPTH_STENCIL.
        let state = match m.desc.Flags.0 & 0x3 {
            0x1 => 0x4,
            0x2 => 0x10,
            _ => return,
        };
        let (r, count) = (m.resource.clone(), m.extent.count());
        let region = &a[1];
        let rects: Vec<RECT> = super::rects(&region["rects"]);
        let (first, subs) = if region.is_object() {
            (
                region["first_subresource"].as_u64().unwrap_or(0) as u32,
                region["subresources"].as_u64().unwrap_or(0) as u32,
            )
        } else {
            (0, count)
        };
        let needs = [super::Need {
            resource: id,
            subs: (first..first + subs).collect(),
            state,
        }];
        let d = D3D12_DISCARD_REGION {
            NumRects: rects.len() as u32,
            pRects: if rects.is_empty() {
                std::ptr::null()
            } else {
                rects.as_ptr()
            },
            FirstSubresource: first,
            NumSubresources: subs,
        };
        self.barriers(&needs, false);
        unsafe { self.list.DiscardResource(&r, Some(&d)) };
        self.barriers(&needs, true);
    }
}

/// The resources each resource shares memory with: placed ones whose ranges of a heap overlap, and reserved ones with
/// a tile mapped into those ranges as the frame found them or after any of its changes. When `relevant` is `Some`,
/// only resources in `relevant` are considered (avoiding scanning tens of thousands of unreferenced tile tables and
/// adding unreferenced resources to every write's dependency set).
pub(super) fn aliases(c: &Capture, relevant: Option<&HashSet<i64>>) -> HashMap<i64, BTreeSet<i64>> {
    // By heap: (first byte, past the last, resource).
    let mut ranges: BTreeMap<i64, Vec<(u64, u64, i64)>> = BTreeMap::new();
    for (id, d) in c.rows(
        "SELECT id, description FROM d12_objects WHERE kind LIKE 'ID3D12Resource%' \
         AND json_extract(description, '$.heap') != 0",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    ) {
        if relevant.is_some_and(|r| !r.contains(&id)) {
            continue;
        }
        let Ok(d) = serde_json::from_str::<Value>(&d) else {
            continue;
        };
        let (Some(heap), offset) = (d["heap"].as_i64(), d["heap_offset"].as_u64().unwrap_or(0)) else {
            continue;
        };
        let bytes = d["bytes"]
            .as_u64()
            .or_else(|| crate::describe::shape(c, id).map(|s| locum_capture::layout::total(&s.subs())));
        if let Some(bytes) = bytes.filter(|b| *b > 0) {
            ranges.entry(heap).or_default().push((offset, offset + bytes, id));
        }
    }
    let tile = locum_capture::tiles::TILE_BYTES;
    let mut tiles: HashSet<(i64, u32, i64)> = HashSet::new();
    for (id, pages) in page_tables(c, relevant) {
        for &(heap, at) in pages.tiles.values() {
            tiles.insert((heap as i64, at, id));
        }
    }
    for (heap, at, id) in tiles {
        let start = u64::from(at) * tile;
        ranges.entry(heap).or_default().push((start, start + tile, id));
    }
    let mut out: HashMap<i64, BTreeSet<i64>> = HashMap::new();
    for list in ranges.values_mut() {
        list.sort_unstable();
        for (i, &(_, end, a)) in list.iter().enumerate() {
            for &(start, _, b) in &list[i + 1..] {
                if start >= end {
                    break;
                }
                if a != b {
                    out.entry(a).or_default().insert(b);
                    out.entry(b).or_default().insert(a);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: u64 = 64 << 10;

    fn span(start: u64, end: u64, bytes: u64, alignment: u64) -> Span {
        Span {
            start: start * K,
            end: Some(end * K),
            bytes: bytes * K,
            alignment,
        }
    }

    #[test]
    fn a_heap_that_fits_is_the_games() {
        let spans = [span(0, 2, 2, K), span(1, 3, 2, K), span(3, 4, 1, 4096)];
        let r = Relocation::new(&spans, 4 * K, K);
        assert!(!r.moved());
        assert_eq!((r.at(K), r.at(3 * K), r.size), (K, 3 * K, 4 * K));
        assert_eq!(r.lost(&spans), 0);
    }

    #[test]
    fn what_takes_more_here_stretches_the_heap_and_keeps_what_shared_memory() {
        // A takes two units here and one in the game, B shares A's unit, C follows it.
        let (a, b, c) = (span(0, 1, 2, K), span(0, 1, 1, K), span(1, 2, 1, K));
        let r = Relocation::new(&[a, b, c], 2 * K, K);
        assert!(r.moved());
        assert_eq!((r.at(0), r.at(K), r.size), (0, 2 * K, 3 * K));
        // A and B still overlap; C, which overlapped neither, starts past A's end here.
        assert_eq!(r.lost(&[a, b, c]), 0);
        // A tile mapped at C's unit is where C is.
        assert_eq!(r.tile(1), 2);
    }

    #[test]
    fn an_offset_this_device_cannot_align_to_moves_to_one_it_can() {
        // A small resource the game placed at 4 KiB, which this device aligns at 64 KiB.
        let d = Span {
            start: 4096,
            end: Some(8192),
            bytes: 4096,
            alignment: K,
        };
        let r = Relocation::new(&[d], K, K);
        assert_eq!((r.at(4096), r.size), (K, 2 * K));
    }

    #[test]
    fn sharing_that_cannot_be_kept_is_counted() {
        // C takes three units here; A overlaps C and B in the game, and B, past C's end, cannot start inside A here.
        let (c, a, b) = (span(0, 1, 3, K), span(0, 2, 2, K), span(1, 3, 2, K));
        let r = Relocation::new(&[c, a, b], 3 * K, K);
        assert_eq!(r.at(K), 3 * K);
        assert_eq!(r.lost(&[c, a, b]), 1);
    }

    #[test]
    fn tile_ranges_follow_their_tiles() {
        // The game's first unit takes two here: its tile 0 stays, tiles 1 and 2 are 2 and 3.
        let r = Relocation::new(&[span(0, 1, 2, K)], 4 * K, K);
        let none = D3D12_TILE_RANGE_FLAG_NONE;
        let (null, reuse) = (
            D3D12_TILE_RANGE_FLAG_NULL,
            D3D12_TILE_RANGE_FLAG_REUSE_SINGLE_TILE,
        );
        // Tiles 0..3 are no longer consecutive: one range a tile. Tiles 1..3 still are.
        assert_eq!(
            ranges_here(&r, None, &[0, 1], &[3, 2]),
            (vec![none; 4], vec![0, 2, 3, 2], vec![1, 1, 1, 2])
        );
        // A NULL range has no tiles of the heap; a reused tile is where it is here.
        assert_eq!(
            ranges_here(&r, Some(&[null, reuse]), &[1, 1], &[4, 4]),
            (vec![null, reuse], vec![1, 2], vec![4, 4])
        );
    }
}
