//! What a program indexing a descriptor heap directly (`ResourceDescriptorHeap`) may read: it picks its slots at run
//! time, so no event names them, and a view of a resource replay did not make is made null. A planned draw or
//! dispatch whose root signature indexes the CBV, SRV and UAV heap directly AND whose shader actually uses
//! `ResourceDescriptorHeap` (`SFI0` bit 25, or unknown bytecode) takes the resources the heap's slots name as its
//! submission found them — skipping heap-only resources that have no captured contents and are not written by any
//! planned event (a null view reads zero, matching an empty resource without allocating tens of gigabytes).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::Value;

use crate::open::Capture;

/// `D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED`.
const DIRECTLY_INDEXED: u64 = 0x400;

/// What [`named`] found for directly indexed descriptor heaps: the resources to create, and how many heap-only
/// resources with no captured contents were left null instead of allocating empty storage.
#[derive(Default)]
pub struct HeapNamed {
    pub resources: BTreeSet<i64>,
    pub skipped_empty: usize,
}

/// The resources (and counters) the CBV, SRV and UAV heaps that `events`' draws and dispatches index directly name
/// at their submissions, excluding heap-only resources with no captured contents that nothing in `events` writes.
pub fn named(
    c: &Capture,
    events: &BTreeSet<i64>,
    existing: &BTreeSet<i64>,
    written_in_plan: &BTreeSet<i64>,
) -> HeapNamed {
    let mut result = HeapNamed::default();
    if events.is_empty() || !c.has_table("d12_draw_state") || !c.has_table("d12_event_locations") {
        return result;
    }
    let indexed: BTreeSet<i64> = c
        .rows("SELECT id, decoded_json FROM d12_roots", [], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })
        .into_iter()
        .filter(|(_, json)| {
            serde_json::from_str::<Value>(json)
                .ok()
                .and_then(|d| d["flags"].as_u64())
                .is_some_and(|f| f & DIRECTLY_INDEXED != 0)
        })
        .map(|(id, _)| id)
        .collect();
    if indexed.is_empty() {
        return result;
    }
    let heaps: BTreeSet<i64> = c
        .rows(
            "SELECT id, description FROM d12_objects WHERE kind = 'ID3D12DescriptorHeap'",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        )
        .into_iter()
        .filter(|(_, d)| serde_json::from_str::<Value>(d).is_ok_and(|d| d["type"].as_u64() == Some(0)))
        .map(|(id, _)| id)
        .collect();
    if heaps.is_empty() {
        return result;
    }

    // Cache shader SFI0 checks (whether a shader actually uses ResourceDescriptorHeap).
    let mut shader_rdh: HashMap<String, bool> = HashMap::new();
    let mut shader_uses_rdh = |hash: &str| -> bool {
        if let Some(&cached) = shader_rdh.get(hash) {
            return cached;
        }
        let uses = c
            .row("SELECT blob FROM shaders WHERE hash = ?1 LIMIT 1", [hash], |r| {
                r.get::<_, Option<String>>(0)
            })
            .flatten()
            .and_then(|key| c.blob(&key).ok())
            .and_then(|code| locum_capture::rdef::uses_resource_descriptor_heap(&code))
            // Conservative when bytecode is missing or unreadable.
            .unwrap_or(true);
        shader_rdh.insert(hash.to_string(), uses);
        uses
    };

    // The heaps each submission's planned draws and dispatches index directly (only when a shader actually uses RDH).
    let mut wanted: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    type DrawRow = (i64, String, String, i64, i64, String, i64, [Option<String>; 6]);
    let draw_rows: Vec<DrawRow> = if events.len() <= 32 {
        // Fast point lookup for single-draw / tiny slices.
        let mut rows = Vec::new();
        for &ev in events {
            rows.extend(c.rows(
                "SELECT d.event, e.kind, e.call, s.graphics_root, s.compute_root, s.root_args_json, x.submission, \
                 d.vs, d.hs, d.ds, d.gs, d.ps, d.cs \
                 FROM draws d JOIN d12_draw_state s ON s.draw = d.n JOIN events e ON e.seq = d.event \
                 JOIN d12_event_locations l ON l.event = d.event JOIN d12_executions x ON x.id = l.execution \
                 WHERE d.event = ?1",
                [ev],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                        [
                            r.get(7)?,
                            r.get(8)?,
                            r.get(9)?,
                            r.get(10)?,
                            r.get(11)?,
                            r.get(12)?,
                        ],
                    ))
                },
            ));
        }
        rows
    } else {
        c.rows(
            "SELECT d.event, e.kind, e.call, s.graphics_root, s.compute_root, s.root_args_json, x.submission, \
             d.vs, d.hs, d.ds, d.gs, d.ps, d.cs \
             FROM draws d JOIN d12_draw_state s ON s.draw = d.n JOIN events e ON e.seq = d.event \
             JOIN d12_event_locations l ON l.event = d.event JOIN d12_executions x ON x.id = l.execution",
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                    r.get::<_, String>(5)?,
                    r.get::<_, i64>(6)?,
                    [
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                        r.get(11)?,
                        r.get(12)?,
                    ],
                ))
            },
        )
    };

    for (event, kind, call, graphics, compute, state, submission, shaders) in draw_rows {
        if !events.contains(&event) {
            continue;
        }
        let roots = match kind.as_str() {
            "draw" => [graphics, 0],
            "dispatch" => [compute, 0],
            _ => [graphics, compute],
        };
        if !roots.iter().any(|r| indexed.contains(r)) {
            continue;
        }
        // Check whether any shader of this draw/dispatch actually uses ResourceDescriptorHeap.
        // DispatchRays uses a state object whose libraries are not in `draws.{vs..cs}`; stay conservative there.
        let active_shaders: Vec<&str> = match kind.as_str() {
            "dispatch" => shaders[5].as_deref().into_iter().collect(),
            "draw" => shaders[..5].iter().filter_map(|s| s.as_deref()).collect(),
            _ => shaders.iter().filter_map(|s| s.as_deref()).collect(),
        };
        let uses_heap = if call == "DispatchRays" || active_shaders.is_empty() {
            true
        } else {
            active_shaders.iter().any(|h| shader_uses_rdh(h))
        };
        if !uses_heap {
            continue;
        }
        let state: Value = serde_json::from_str(&state).unwrap_or(Value::Null);
        for heap in state["descriptor_heaps"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_i64)
        {
            if heaps.contains(&heap) {
                wanted.entry(submission).or_default().insert(heap);
            }
        }
    }
    if wanted.is_empty() {
        return result;
    }

    // Resources that have captured contents (or tile events / AS builds) are worth materializing for bindless access.
    // Heap-only resources with no contents and not written by any planned event are left null (reads 0, same as empty).
    let with_content: HashSet<i64> = c
        .rows(
            "SELECT id FROM resources WHERE content IS NOT NULL \
             OR coalesce(content_source, '') IN ('tile-events', 'acceleration-structure-builds')",
            [],
            |r| r.get(0),
        )
        .into_iter()
        .collect();

    // Each submission's descriptors are the changes it brought: the heaps' slots as each wanted one found them.
    let order: Vec<i64> = c.rows("SELECT id FROM d12_submissions ORDER BY cpu_seq, id", [], |r| {
        r.get(0)
    });
    let last = order.iter().rposition(|s| wanted.contains_key(s)).unwrap_or(0);
    let wanted_heaps: HashSet<i64> = wanted.values().flatten().copied().collect();
    let mut slots: HashMap<(i64, i64), [i64; 2]> = HashMap::new();
    let mut skipped: HashSet<i64> = HashSet::new();
    for s in &order[..=last] {
        for (heap, slot, kind, resource, counter) in c.rows(
            "SELECT v.heap, v.slot, v.kind, v.resource, v.counter_resource FROM d12_submission_descriptors d \
             JOIN d12_descriptor_versions v ON v.id = d.version WHERE d.submission = ?1 ORDER BY v.id",
            [s],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                ))
            },
        ) {
            if !wanted_heaps.contains(&heap) {
                continue;
            }
            if kind == "empty" {
                slots.remove(&(heap, slot));
            } else {
                slots.insert((heap, slot), [resource, counter]);
            }
        }
        if let Some(indexed) = wanted.get(s) {
            for (&(heap, _), named) in &slots {
                if indexed.contains(&heap) {
                    for &r in named.iter().filter(|&&r| r > 0) {
                        if existing.contains(&r) || written_in_plan.contains(&r) || with_content.contains(&r)
                        {
                            result.resources.insert(r);
                        } else {
                            skipped.insert(r);
                        }
                    }
                }
            }
        }
    }
    skipped.retain(|r| !result.resources.contains(r) && !existing.contains(r));
    result.skipped_empty = skipped.len();
    result
}
