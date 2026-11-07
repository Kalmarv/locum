//! The buffers ExecuteIndirect's argument records name. A command signature can put GPU addresses in each record
//! (vertex and index buffer views, root CBVs, SRVs and UAVs), and a buffer the frame uses only through those is named
//! by no command it records: its first use is never seen, so it gets no first-use copy, and what a root UAV in the
//! records lets the call write is not copied after it. The records are copied right before each such ExecuteIndirect
//! (see [`super::passes`]); at the Present that closes the capture they are read, every address is found in the live
//! buffers, and each event reads the buffers its records name and writes those a root UAV names. Then the buffers
//! [`model::Prepared::after_frame`] asks for are copied whole, each on its queue: the contents a buffer only records
//! name began the frame with, where nothing the frame recorded writes it, and what an ExecuteIndirect wrote through
//! a root UAV, where nothing after it writes the buffer.
//!
//! A DispatchRays' shader tables are read once more as its submission is made ([`named_at_submission`]): what their
//! records name then gets a first-use copy before the submission, as what its commands name does, so a buffer or
//! texture only a local root argument names begins the frame with its contents.
//!
//! A top-level build whose instances are an array of pointers is read the same way: its pointers are copied right
//! before it, read at the Present for the buffers they point into (which the build reads), and read once more as
//! its submission is made, for their first-use copies.
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use locum_capture::d3d12 as model;
use locum_capture::d3d12::indirect::Kind;
use locum_capture::db;
use serde_json::{Value, json};
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Resource};
use windows::core::Interface;

use super::readback::{Results, Snapshot};

/// What shader tables are read against: the frame as the Present that closes the capture has it, or as a submission
/// is made.
enum Frame<'a> {
    Closed(&'a model::Prepared),
    Submitting(&'a model::Projection),
}

impl Frame<'_> {
    fn description(&self, id: u64) -> Option<&Value> {
        match self {
            Frame::Closed(p) => p.description(id),
            Frame::Submitting(p) => p.description(id),
        }
    }
}

/// What DXIL libraries declare, read from the blob store as tables need them, and the local root signatures their
/// records run with, decoded.
#[derive(Default)]
struct Programs {
    declared: RefCell<HashMap<String, Result<Arc<model::programs::Declared>, String>>>,
    decoded: RefCell<HashMap<Vec<u8>, Result<Value, String>>>,
}

impl Programs {
    fn declared(&self, key: &str) -> Result<Arc<model::programs::Declared>, String> {
        if let Some(found) = self.declared.borrow().get(key) {
            return found.clone();
        }
        // Its blob was queued when the state object was made.
        locum_capture::writer::flush();
        let found = crate::settings::settings()
            .blobs
            .get(key)
            .map_err(|e| format!("a DXIL library's blob {key}: {e}"))
            .and_then(|bytes| model::programs::Declared::read(&bytes))
            .map(Arc::new);
        self.declared.borrow_mut().insert(key.to_string(), found.clone());
        found
    }

    /// What tables `tables` (copied: `bytes`) of `rays` name: the resources it reads, and those it may write; what
    /// could not be placed is counted in `gaps`.
    fn named(
        &self,
        frame: Frame,
        rays: &model::Indirect,
        tables: &[model::tables::Table],
        bytes: &[u8],
        gaps: &mut BTreeMap<String, u64>,
    ) -> (Vec<u64>, Vec<u64>) {
        let mut decode = |local: &model::programs::Local| match local {
            model::programs::Local::Object(id) => {
                let d = frame
                    .description(*id)
                    .ok_or_else(|| format!("local root signature #{id} is not described"))?;
                match &d["decoded"] {
                    v if v["parameters"].is_array() => Ok(v.clone()),
                    v => Err(format!(
                        "local root signature #{id} was not decoded: {}",
                        v["diagnostic"]
                    )),
                }
            }
            model::programs::Local::Declared(serialized) => self
                .decoded
                .borrow_mut()
                .entry(serialized.clone())
                .or_insert_with(|| model::programs::root_layout(serialized))
                .clone(),
        };
        let buffers = |address: u64| {
            super::slots::buffers_at(address)
                .into_iter()
                .map(|(resource, _)| resource)
                .collect()
        };
        let declared = |key: &str| self.declared(key);
        let named = match frame {
            Frame::Closed(p) => p.named_by_tables(rays, tables, bytes, &declared, &mut decode, &buffers),
            Frame::Submitting(p) => p.named_by_tables(rays, tables, bytes, &declared, &mut decode, &buffers),
        };
        for (why, count) in named.unplaced {
            *gaps.entry(why).or_default() += count;
        }
        (named.reads, named.writes)
    }
}

/// The resources the shader tables of the DispatchRays a submission runs name, read as it is made (before its lists
/// run, on `queue`, the real queue: what the queue ran before has written them), so that each gets a first-use copy
/// before them. `projection` has projected the submission; its DispatchRays are those from `from` on in its list, and
/// `events` are the frame's so far. A table something in the frame writes is not read: what it held when the
/// DispatchRays ran is not known now. What cannot be read or placed names nothing here, and the Present that closes
/// the capture, reading the tables again from the copies made right before each DispatchRays, says why.
///
/// # Safety
/// Called while the queue lock is held, before the submission is forwarded to `queue`, which waits for nothing that
/// is not yet submitted.
pub unsafe fn named_at_submission(
    projection: &model::Projection,
    events: &[db::Event],
    from: usize,
    queue: &ID3D12CommandQueue,
) -> Vec<u64> {
    let programs = Programs::default();
    let written = |r: u64| events.iter().any(|e| e.writes.contains(&r));
    let mut named = BTreeSet::new();
    for rays in projection.indirect().get(from..).unwrap_or_default() {
        if rays.arguments.pointers > 0 {
            named.extend(unsafe { pointed_at_submission(rays, &written, queue) });
            continue;
        }
        let tables = &rays.arguments.tables;
        if tables.is_empty() {
            continue;
        }
        let mut ranges = Vec::new();
        let readable = tables
            .iter()
            .all(|t| match super::slots::buffers_at(t.address)[..] {
                [(resource, offset)] if !written(resource) => super::real::<ID3D12Resource>(resource)
                    .map(|real| ranges.push((real, offset, t.bytes)))
                    .is_some(),
                _ => false,
            });
        if !readable {
            continue;
        }
        let Ok(bytes) = (unsafe { super::passes::read_now(queue, ranges, PATIENCE) }) else {
            continue;
        };
        let (reads, writes) = programs.named(
            Frame::Submitting(projection),
            rays,
            tables,
            &bytes,
            &mut BTreeMap::new(),
        );
        named.extend(reads.into_iter().chain(writes));
    }
    named.into_iter().collect()
}

/// The buffers the instance pointers of build `build` point into, as its submission is made (`written`: whether the
/// frame writes a resource): none where the frame writes the pointers.
///
/// # Safety
/// As [`named_at_submission`].
unsafe fn pointed_at_submission(
    build: &model::Indirect,
    written: &dyn Fn(u64) -> bool,
    queue: &ID3D12CommandQueue,
) -> Vec<u64> {
    let ranges = match super::slots::buffers_at(build.arguments.pointers_at)[..] {
        [(resource, offset)] if !written(resource) => super::real::<ID3D12Resource>(resource)
            .map(|real| vec![(real, offset, build.arguments.bytes)])
            .unwrap_or_default(),
        _ => return Vec::new(),
    };
    let Ok(bytes) = (unsafe { super::passes::read_now(queue, ranges, PATIENCE) }) else {
        return Vec::new();
    };
    pointed(&bytes).0
}

/// The buffers the instance pointers `bytes` point into, and whether one points into none.
fn pointed(bytes: &[u8]) -> (Vec<u64>, bool) {
    let mut found = BTreeSet::new();
    let mut unplaced = false;
    for p in model::pointers(bytes).filter(|&p| p != 0) {
        let at = super::slots::buffers_at(p);
        unplaced |= at.is_empty();
        found.extend(at.into_iter().map(|(resource, _)| resource));
    }
    (found.into_iter().collect(), unplaced)
}

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
    let mut table_gaps = BTreeMap::<String, u64>::new();
    let mut pointer_gaps = BTreeMap::<String, u64>::new();
    let programs = Programs::default();
    // ExecuteIndirects whose records run DispatchRays: each with the tables its records name, read once all are known.
    let mut indirect_rays = Vec::new();
    for i in &indirect {
        let bytes = match copies.get(&i.arguments.id) {
            Some(Ok(bytes)) => bytes,
            Some(Err(why)) => {
                let into = if i.arguments.pointers > 0 {
                    &mut pointer_gaps
                } else if i.arguments.tables.is_empty() {
                    &mut gaps
                } else {
                    &mut table_gaps
                };
                *into.entry(why.clone()).or_default() += 1;
                continue;
            }
            None => {
                *gaps.entry("its copy was not kept".into()).or_default() += 1;
                continue;
            }
        };
        if i.arguments.pointers > 0 {
            let (reads, unplaced) = pointed(bytes);
            if unplaced {
                *pointer_gaps
                    .entry("an instance pointer is in no live buffer: replay leaves the instance out".into())
                    .or_default() += 1;
            }
            prepared.named_by_arguments(capture, i, &reads, &[]);
            continue;
        }
        if !i.arguments.tables.is_empty() {
            let (reads, writes) = programs.named(
                Frame::Closed(prepared),
                i,
                &i.arguments.tables,
                bytes,
                &mut table_gaps,
            );
            prepared.named_by_arguments(capture, i, &reads, &writes);
            continue;
        }
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
        let tables: Vec<_> = model::indirect::shader_tables(bytes, &i.arguments.layout, records)
            .into_iter()
            .flatten()
            .collect();
        if !tables.is_empty() {
            indirect_rays.push((i.clone(), tables));
        }
    }
    // What the frame left in the tables ExecuteIndirect's records name is what they held when it ran, where the frame
    // wrote none of them.
    for (i, tables) in indirect_rays {
        let written = |r: u64| capture.events.iter().any(|e| e.writes.contains(&r));
        let mut ranges = Vec::new();
        let read = tables.iter().try_for_each(|t| {
            let (resource, offset) = match super::slots::buffers_at(t.address)[..] {
                [one] => one,
                [] => return Err("a shader table's address is in no live buffer".to_string()),
                _ => return Err("a shader table's address is in more than one live buffer".to_string()),
            };
            if written(resource) {
                return Err(
                    "the frame writes a shader table its records name, so what it held then is not known"
                        .into(),
                );
            }
            let real =
                super::real::<ID3D12Resource>(resource).ok_or("a shader table's buffer was destroyed")?;
            ranges.push((real, offset, t.bytes));
            Ok(())
        });
        let bytes = read.and_then(|()| {
            let queue = super::real::<ID3D12CommandQueue>(i.queue).ok_or("its queue was destroyed")?;
            unsafe { super::passes::read_now(&queue, ranges, PATIENCE) }
        });
        match bytes {
            Ok(bytes) => {
                let (reads, writes) =
                    programs.named(Frame::Closed(prepared), &i, &tables, &bytes, &mut table_gaps);
                prepared.named_by_arguments(capture, &i, &reads, &writes);
            }
            Err(why) => *table_gaps.entry(why).or_default() += 1,
        }
    }
    for (why, count) in gaps {
        capture.gap("d3d12-indirect-arguments", why, count);
    }
    for (why, count) in table_gaps {
        capture.gap("d3d12-shader-tables", why, count);
    }
    for (why, count) in pointer_gaps {
        capture.gap("d3d12-acceleration-structures", why, count);
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
        let Some(real) = super::real::<ID3D12CommandQueue>(queue) else {
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
        unsafe { super::readback::submit(queue, real.as_raw(), submission, Vec::new(), snapshots, results) };
    }
}
