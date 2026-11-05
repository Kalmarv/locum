//! The buffers ExecuteIndirect's argument records name. A command signature can put GPU addresses in each record
//! (vertex and index buffer views, root CBVs, SRVs and UAVs), and a buffer the frame reads only through those is
//! named by no command it records: its first use is never seen, so it gets no first-use copy, and replay has none
//! of its contents. The records are copied right before each such ExecuteIndirect (see [`super::passes`]); at the
//! Present that closes the capture they are read, every address is found in the live buffers, each event reads the
//! buffers its records name, and those the frame used no other way are copied whole on the queue that ran the
//! first ExecuteIndirect naming them, filed as their contents at its submission. Nothing the frame recorded writes
//! them (a command that did would name them), so what they hold after the frame is what they held as it began,
//! unless a shader wrote them through a view, or another thread's submission came between.
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::Duration;

use locum_capture::d3d12 as model;
use locum_capture::db;
use windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue;
use windows::core::Interface;

use super::readback::{Results, Snapshot};

/// How long the Present waits for the GPU to finish the frame's copies of argument records.
const PATIENCE: Duration = Duration::from_secs(10);

/// Reads the armed frame's argument records into `capture` (see the module's documentation); `used` holds the
/// resources the frame's submissions made first-use copies of (or gave up), `results` its copies' results.
///
/// # Safety
/// Called at the Present that closes the capture, on the game's thread.
pub unsafe fn after_frame(
    prepared: &mut model::Prepared,
    capture: &mut db::Capture,
    used: &HashSet<u64>,
    results: &Results,
) {
    let copies = super::passes::argument_records(PATIENCE);
    let indirect = prepared.indirect().to_vec();
    let mut gaps = BTreeMap::<String, u64>::new();
    // The resources with no contents, each with the submission and queue of the first event naming it.
    let mut uncovered = BTreeMap::<u64, (u64, u64)>::new();
    for i in &indirect {
        let bytes = match copies.get(&i.arguments.id) {
            Some(Ok(bytes)) => bytes,
            Some(Err(why)) => {
                *gaps.entry(why.clone()).or_default() += 1;
                continue;
            }
            None => {
                *gaps.entry("its copy was not kept".into()).or_default() += 1;
                continue;
            }
        };
        let records = i.arguments.records(bytes);
        let mut named = BTreeSet::new();
        let mut unplaced = false;
        model::indirect::addresses(bytes, &i.arguments.layout, records, |address| {
            let found = super::slots::buffers_at(address);
            unplaced |= found.is_empty();
            named.extend(found.into_iter().map(|(resource, _)| resource));
        });
        if unplaced {
            *gaps
                .entry("an address in its records is in no live buffer".into())
                .or_default() += 1;
        }
        let named: Vec<u64> = named.into_iter().collect();
        prepared.named_by_arguments(capture, i.event, &named);
        for r in named.into_iter().filter(|r| !used.contains(r)) {
            uncovered.entry(r).or_insert((i.submission, i.queue));
        }
    }
    for (why, count) in gaps {
        capture.gap("d3d12-indirect-arguments", why, count);
    }
    let mut copies = BTreeMap::<(u64, u64), Vec<Snapshot>>::new();
    for (resource, at) in uncovered {
        let Some(d) = super::journal::description(resource) else {
            continue;
        };
        // An upload buffer's contents are its copy at the frame's first submission; only the GPU writes a readback
        // buffer. Addresses are only ever buffers'.
        if matches!(d["heap_properties"]["type"].as_u64(), Some(2 | 3)) || d["dimension"] != 1 {
            continue;
        }
        let Some(layout) = model::layout(&d) else {
            continue;
        };
        copies.entry(at).or_default().push(Snapshot {
            resource,
            states: Vec::new(),
            subresources: 1,
            layout,
            joined: None,
        });
        prepared.copied_after_frame(resource);
    }
    for ((submission, queue), snapshots) in copies {
        let Some(real) = super::real(queue).and_then(|q| q.cast::<ID3D12CommandQueue>().ok()) else {
            capture.gap(
                "d3d12-indirect-arguments",
                format!(
                    "the queue that ran them was destroyed: {} buffers they name were not copied",
                    snapshots.len()
                ),
                1,
            );
            continue;
        };
        unsafe { super::readback::submit(real.as_raw(), submission, Vec::new(), snapshots, results) };
    }
}
