//! What a program indexing a descriptor heap directly (`ResourceDescriptorHeap`) may read: it picks its slots at run
//! time, so no event names them, and a view of a resource replay did not make is made null. A planned draw or
//! dispatch whose root signature indexes the CBV, SRV and UAV heap directly takes every resource the heap's slots
//! name as its submission found them, as the capture copied them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde_json::Value;

use crate::open::Capture;

/// `D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED`.
const DIRECTLY_INDEXED: u64 = 0x400;

/// The resources (and counters) the CBV, SRV and UAV heaps that `events`' draws and dispatches index directly name
/// at their submissions.
pub fn named(c: &Capture, events: &BTreeSet<i64>) -> BTreeSet<i64> {
    let mut out = BTreeSet::new();
    if !c.has_table("d12_draw_state") || !c.has_table("d12_event_locations") {
        return out;
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
        return out;
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
    // The heaps each submission's planned draws and dispatches index directly.
    let mut wanted: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
    for (event, kind, graphics, compute, state, submission) in c.rows(
        "SELECT d.event, e.kind, s.graphics_root, s.compute_root, s.root_args_json, x.submission FROM draws d \
         JOIN d12_draw_state s ON s.draw = d.n JOIN events e ON e.seq = d.event \
         JOIN d12_event_locations l ON l.event = d.event JOIN d12_executions x ON x.id = l.execution",
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        },
    ) {
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
        let state: Value = serde_json::from_str(&state).unwrap_or(Value::Null);
        for heap in state["descriptor_heaps"].as_array().into_iter().flatten().filter_map(Value::as_i64) {
            if heaps.contains(&heap) {
                wanted.entry(submission).or_default().insert(heap);
            }
        }
    }
    if wanted.is_empty() {
        return out;
    }
    // Each submission's descriptors are the changes it brought: the heaps' slots as each wanted one found them.
    let order: Vec<i64> = c.rows("SELECT id FROM d12_submissions ORDER BY cpu_seq, id", [], |r| {
        r.get(0)
    });
    let last = order.iter().rposition(|s| wanted.contains_key(s)).unwrap_or(0);
    let mut slots: HashMap<(i64, i64), [i64; 2]> = HashMap::new();
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
            if !heaps.contains(&heap) {
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
                    out.extend(named.iter().copied().filter(|&r| r > 0));
                }
            }
        }
    }
    out
}
