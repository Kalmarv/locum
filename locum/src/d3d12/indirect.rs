//! The buffers ExecuteIndirect's argument records name. A command signature can put GPU addresses in each record
//! (vertex and index buffer views, root CBVs, SRVs and UAVs), and a buffer the frame uses only through those is named
//! by no command it records: its first use is never seen, so it gets no first-use copy, and what a root UAV in the
//! records lets the call write is not copied after it. The records are copied right before each such ExecuteIndirect
//! (see [`super::passes`]); at the Present that closes the capture they are read, every address is found in the live
//! buffers, and each event reads the buffers its records name and writes those a root UAV names. Then the buffers
//! [`model::Prepared::after_frame`] asks for are copied whole, each on its queue: the contents a buffer only records
//! name began the frame with, where nothing the frame recorded writes it, and what an ExecuteIndirect wrote through
//! a root UAV, where nothing after it writes the buffer.
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::time::Duration;

use locum_capture::d3d12 as model;
use locum_capture::d3d12::indirect::Kind;
use locum_capture::db;
use serde_json::json;
use windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue;
use windows::core::Interface;

use super::readback::{Results, Snapshot};

/// How long the Present waits for the GPU to finish the frame's copies of argument records.
const PATIENCE: Duration = Duration::from_secs(10);

/// Reads the armed frame's argument records into `capture` (see the module's documentation) and copies the buffers
/// they call for; `used` holds the resources the frame's submissions made first-use copies of (or gave up). The
/// copies' results go to `results`, for [`model::Prepared::set_after_frame`]; those that cannot be made are there at
/// once, with the reason.
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
        let (mut reads, mut writes) = (BTreeSet::new(), BTreeSet::new());
        let mut unplaced = false;
        model::indirect::addresses(bytes, &i.arguments.layout, records, |address, kind| {
            let found = super::slots::buffers_at(address);
            unplaced |= found.is_empty();
            let into = if kind == Kind::UnorderedAccess {
                &mut writes
            } else {
                &mut reads
            };
            into.extend(found.into_iter().map(|(resource, _)| resource));
        });
        if unplaced {
            *gaps
                .entry("an address in its records is in no live buffer".into())
                .or_default() += 1;
        }
        let reads: Vec<u64> = reads.difference(&writes).copied().collect();
        let writes: Vec<u64> = writes.into_iter().collect();
        prepared.named_by_arguments(capture, i, &reads, &writes);
    }
    for (why, count) in gaps {
        capture.gap("d3d12-indirect-arguments", why, count);
    }
    let mut by_queue = BTreeMap::<(u64, u64), Vec<Snapshot>>::new();
    for copy in prepared.after_frame(capture, |r| used.contains(&r)) {
        by_queue
            .entry((copy.submission, copy.queue))
            .or_default()
            .push(Snapshot {
                resource: copy.resource,
                // Buffers are COMMON whenever no submission runs.
                states: Vec::new(),
                subresources: 1,
                layout: copy.layout,
                joined: None,
            });
    }
    for ((submission, queue), snapshots) in by_queue {
        let Some(real) = super::real(queue).and_then(|q| q.cast::<ID3D12CommandQueue>().ok()) else {
            let failed = snapshots
                .iter()
                .map(|s| {
                    (
                        s.resource,
                        model::FirstUse {
                            submission,
                            states: json!([]),
                            content: Err("the queue to copy it on after the frame was destroyed".into()),
                        },
                    )
                })
                .collect();
            results
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(super::readback::Batch {
                    submission,
                    contents: failed,
                    ..Default::default()
                });
            continue;
        };
        unsafe { super::readback::submit(real.as_raw(), submission, Vec::new(), snapshots, results) };
    }
}
