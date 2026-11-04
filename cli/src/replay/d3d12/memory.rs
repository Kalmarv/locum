//! Memory the game shared between resources. Its heaps are made again from their descriptions; a placed resource is
//! made at its offset in the made heap, and a reserved resource reserved, its tiles mapped into the made heaps as the
//! frame found them (`d12_tile_pages`). The frame's changes to tile mappings (`d12_tile_mappings`) are run where the
//! CPU made them among the submissions, as replay reaches them. Such a resource's first-use contents are written when
//! replay reaches the submission they were copied at, not at the start: memory a resource shares holds, at each
//! point, what the frame's own copy saw there. The game's aliasing barriers, and its discards of render and depth
//! targets so placed (the operations that begin such a target's use), are run.
//!
//! Where that cannot be done as the game did, the resource is made committed instead, and replay says why: a heap the
//! capture does not describe (captures before heaps were described), a resource that takes more of its heap on this
//! device than in the game or is not aligned for it there, a reserved resource this device tiles otherwise, or one the
//! capture holds no page table of.
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
    /// In heap `heap` at `offset`, taking `bytes` of it there where the capture says.
    Placed {
        heap: i64,
        offset: u64,
        bytes: Option<u64>,
    },
    Reserved,
}

impl Placement {
    pub(super) fn of(d: &Value) -> Placement {
        match d["heap"].as_i64().filter(|h| *h != 0) {
            Some(heap) => Placement::Placed {
                heap,
                offset: d["heap_offset"].as_u64().unwrap_or(0),
                bytes: d["bytes"].as_u64(),
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
}

#[derive(Default)]
pub(super) struct Memory {
    /// The game's heaps, made when a resource first needs one, with their size; or why one could not be.
    heaps: HashMap<i64, Result<(ID3D12Heap, u64), String>>,
    /// Resources made in a heap (placed, or reserved with their tiles mapped).
    pub(super) shared: HashSet<i64>,
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
    /// Changes to tile mappings run, and those of resources made committed, which are not.
    pub(super) remaps: usize,
    pub(super) not_remapped: usize,
}

impl Memory {
    /// Says what replay could not make as the game did.
    pub(super) fn warnings(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .committed
            .iter()
            .map(|(why, n)| {
                format!("{n} placed or reserved resources were made committed, memory the game shared not shared: {why}")
            })
            .collect();
        if self.not_remapped > 0 {
            out.push(format!(
                "the frame's {} changes to the tile mappings of reserved resources made committed are not run",
                self.not_remapped
            ));
        }
        out
    }

    pub(super) fn fell_back(&mut self, why: String) {
        *self.committed.entry(why).or_default() += 1;
    }
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
    /// The game's heap `id`, made from its description on first use, with its size.
    fn game_heap(&mut self, c: &Capture, id: i64) -> Result<(ID3D12Heap, u64), String> {
        if !self.memory.heaps.contains_key(&id) {
            let made = self.make_heap(c, id);
            self.memory.heaps.insert(id, made);
        }
        self.memory.heaps[&id].clone()
    }

    fn make_heap(&self, c: &Capture, id: i64) -> Result<(ID3D12Heap, u64), String> {
        let d: Value = c
            .row(
                "SELECT description FROM d12_objects WHERE id = ?1 AND kind LIKE 'ID3D12Heap%'",
                [id],
                |r| r.get::<_, String>(0),
            )
            .and_then(|d| serde_json::from_str(&d).ok())
            .unwrap_or(Value::Null);
        let bytes = d["bytes"]
            .as_u64()
            .ok_or("the capture does not describe their heap (captured before heaps were described)")?;
        let p = &d["heap_properties"];
        let n = |v: &Value| v.as_i64().unwrap_or(0) as i32;
        // The deny flags and shader atomics; sharing, display and residency flags are dropped.
        const KEPT: i32 = 0x4 | 0x40 | 0x80 | 0x400;
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: bytes,
            Properties: D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE(n(&p["type"])),
                CPUPageProperty: D3D12_CPU_PAGE_PROPERTY(n(&p["cpu_page"])),
                MemoryPoolPreference: D3D12_MEMORY_POOL(n(&p["memory_pool"])),
                ..Default::default()
            },
            Alignment: d["alignment"].as_u64().unwrap_or(0),
            Flags: D3D12_HEAP_FLAGS(n(&d["flags"]) & KEPT),
        };
        let mut heap: Option<ID3D12Heap> = None;
        unsafe { self.device.CreateHeap(&desc, &mut heap) }
            .map_err(|e| format!("their heap could not be made on this device: {e}"))?;
        let heap = heap.ok_or("CreateHeap gave no heap")?;
        super::name(&heap, format!("#{id}"));
        Ok((heap, bytes))
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
            Placement::Placed { heap, offset, bytes } => {
                let (h, size) = self.game_heap(c, heap)?;
                let info = unsafe { self.device.GetResourceAllocationInfo(0, &[*desc]) };
                if info.SizeInBytes == u64::MAX {
                    return Err("this device refuses their descriptions".into());
                }
                if bytes.is_some_and(|b| info.SizeInBytes > b) {
                    return Err("they take more of their heap on this device than in the game".into());
                }
                if info.Alignment == 0 || offset % info.Alignment != 0 || offset + info.SizeInBytes > size {
                    return Err("their offsets do not fit their heaps on this device".into());
                }
                unsafe {
                    self.device
                        .CreatePlacedResource(&h, offset, desc, state, None, &mut r)
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
                unsafe { self.device.CreateReservedResource(desc, state, None, &mut r) }
                    .map_err(|e| format!("CreateReservedResource refused them: {e}"))?;
                let resource = r.as_ref().ok_or("CreateReservedResource gave no resource")?;
                let layout = locum_capture::tiles::d3d12(resource)?;
                if layout != recorded {
                    return Err("this device tiles them otherwise than the game's".into());
                }
                // Every mapped tile, one region each, by the heap it is in.
                let mut by_heap: BTreeMap<i64, (Vec<D3D12_TILED_RESOURCE_COORDINATE>, Vec<u32>)> =
                    BTreeMap::new();
                for (&tile, &(heap, at)) in &pages.tiles {
                    let [x, y, z, sub] = layout.coordinate(tile)?;
                    let e = by_heap.entry(heap as i64).or_default();
                    e.0.push(D3D12_TILED_RESOURCE_COORDINATE {
                        X: x,
                        Y: y,
                        Z: z,
                        Subresource: sub,
                    });
                    e.1.push(at);
                }
                for (heap, (starts, offsets)) in by_heap {
                    let (h, _) = self.game_heap(c, heap)?;
                    let counts = vec![1u32; starts.len()];
                    unsafe {
                        self.queue.UpdateTileMappings(
                            resource,
                            starts.len() as u32,
                            Some(starts.as_ptr()),
                            None,
                            &h,
                            starts.len() as u32,
                            None,
                            Some(offsets.as_ptr()),
                            Some(counts.as_ptr()),
                            D3D12_TILE_MAPPING_FLAG_NONE,
                        )
                    };
                }
                self.memory.reserved += 1;
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

    /// The bytes of waiting contents.
    fn waited(&self, key: &str) -> Result<Vec<u8>, String> {
        let blobs = self.memory.blobs.as_ref().ok_or("no blob store")?;
        blobs
            .get(key)
            .map_err(|e| format!("blob {key} is not in {} ({e})", blobs.root().display()))
    }

    /// Writes the first-use contents copied at `submission`.
    pub(super) fn first_uses(&mut self, submission: i64) -> Result<(), String> {
        for (id, key) in self.memory.pending.remove(&submission).unwrap_or_default() {
            self.memory.waiting.remove(&id);
            let bytes = self.waited(&key)?;
            self.put(id, &bytes)?;
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
        let bytes = self.waited(&key)?;
        self.put(id, &bytes)
    }

    /// Runs the frame's changes to tile mappings the CPU made before `cpu_seq`, in order.
    pub(super) fn remap_before(&mut self, c: &Capture, cpu_seq: i64) -> Result<(), String> {
        let followed = c.has_table("d12_tile_mappings");
        let mappings = self.memory.mappings.get_or_insert_with(|| {
            if !followed {
                return Vec::new();
            }
            c.rows(
                "SELECT cpu_seq, call, resource, source, args_json FROM d12_tile_mappings ORDER BY cpu_seq, id",
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
            let (call, resource, source, a) = (m.call.clone(), m.resource, m.source, m.args.clone());
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
            self.memory.remaps += 1;
        }
        Ok(())
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
                    Some(h) => Some(self.game_heap(c, h)?.0),
                    None => None,
                };
                let starts = array(&a["starts"], coordinate);
                let sizes = array(&a["sizes"], size);
                let flags = array(&a["range_flags"], |v| D3D12_TILE_RANGE_FLAGS(n(v) as i32));
                let offsets = array(&a["offsets"], n);
                // Null counts mean one tile a range when there are several: spelled out, as WARP needs them.
                let ranges = n(&a["ranges"]);
                let counts =
                    array(&a["counts"], n).or_else(|| (ranges > 1).then(|| vec![1; ranges as usize]));
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
                let s = self.resource(source.ok_or("no source")?)?;
                unsafe {
                    self.queue.CopyTileMappings(
                        &r,
                        &coordinate(&a["dst_at"]),
                        &s,
                        &coordinate(&a["src_at"]),
                        &size(&a["size"]),
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
/// a tile mapped into those ranges as the frame found them or after any of its changes. A placed resource whose size
/// the capture does not give is taken as large as its contents.
pub(super) fn aliases(c: &Capture) -> HashMap<i64, BTreeSet<i64>> {
    // By heap: (first byte, past the last, resource).
    let mut ranges: BTreeMap<i64, Vec<(u64, u64, i64)>> = BTreeMap::new();
    for (id, d) in c.rows(
        "SELECT id, description FROM d12_objects WHERE kind LIKE 'ID3D12Resource%' \
         AND json_extract(description, '$.heap') != 0",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    ) {
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
    // Captures from before reserved resources were followed have no page tables.
    let mut tables: Vec<(i64, String)> = Vec::new();
    for table in ["d12_tile_pages", "d12_tile_mappings"] {
        if c.has_table(table) {
            tables.extend(c.rows(
                &format!("SELECT resource, pages_json FROM {table} WHERE pages_json IS NOT NULL"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            ));
        }
    }
    let mut tiles: HashSet<(i64, u32, i64)> = HashSet::new();
    for (id, json) in tables {
        let Some(pages) = serde_json::from_str(&json)
            .ok()
            .and_then(|v| Pages::from_runs(&v))
        else {
            continue;
        };
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
