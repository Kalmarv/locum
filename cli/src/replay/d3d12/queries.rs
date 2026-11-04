//! The frame's queries. Each event replays in a command list of its own, and an occlusion or pipeline-statistics
//! query must begin and end in one list, so the game's queries cannot run as recorded. What they count adds up over
//! the draws and dispatches between their begin and end, though. Replay runs a query of its own around each draw or
//! dispatch the game's list made while one of its queries was open, adds what that counted to it, and writes the sums
//! where the game resolved them. A timestamp is the replay's own GPU clock at the event: no replay can make it the
//! game's.
//!
//! For planning, each query (heap and index) is a resource with a negative id ([`pseudo`]). Its begin, its end and
//! the draws between write it, and a resolve reads it, so a replay that stops early still runs the draws it counts.
use std::collections::{BTreeSet, HashMap};

use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::*;

use super::{D3d12, u};
use crate::open::Capture;

const OCCLUSION: u64 = 0;
const BINARY_OCCLUSION: u64 = 1;
const TIMESTAMP: u64 = 2;
const PIPELINE_STATISTICS: u64 = 3;
/// D3D12_QUERY_DATA_PIPELINE_STATISTICS: eleven counters.
const STATISTICS: usize = 11;

/// Where replay's own queries resolve in its readback buffer.
const AT_OCCLUSION: u64 = 0;
const AT_TIMESTAMP: u64 = 8;
const AT_STATISTICS: u64 = 16;
const RESULTS: u64 = AT_STATISTICS + 8 * STATISTICS as u64;

/// A game's query: its heap's id and its index there.
type Query = (i64, u64);

/// An event as planning reads it for queries.
struct Event {
    seq: i64,
    kind: String,
    call: String,
    args: Value,
    list: Option<i64>,
}

/// Query `index` of heap `heap` as a resource for planning: negative, so it is no resource's id.
pub(super) fn pseudo(heap: i64, index: u64) -> i64 {
    -((heap << 24) | index as i64) - 1
}

/// For planning: each event that begins, ends, counts into or resolves a query, with the queries it reads and writes.
pub(super) fn touches(c: &Capture) -> Vec<(i64, Vec<i64>, Vec<i64>)> {
    let events: Vec<Event> = c.rows(
        "SELECT seq, kind, call, args, list FROM events WHERE call IN ('BeginQuery', 'EndQuery', 'ResolveQueryData') \
         OR kind IN ('draw', 'dispatch') ORDER BY seq",
        [],
        |r| {
            Ok(Event {
                seq: r.get(0)?,
                kind: r.get(1)?,
                call: r.get(2)?,
                args: r
                    .get::<_, Option<String>>(3)?
                    .and_then(|a| serde_json::from_str(&a).ok())
                    .unwrap_or_default(),
                list: r.get(4)?,
            })
        },
    );
    let mut open: HashMap<Query, Option<i64>> = HashMap::new();
    let mut out = Vec::new();
    for Event {
        seq,
        kind,
        call,
        args: a,
        list,
    } in events
    {
        let (heap, index) = (u(&a, 0) as i64, u(&a, 2));
        match call.as_str() {
            "BeginQuery" => {
                open.insert((heap, index), list);
                out.push((seq, Vec::new(), vec![pseudo(heap, index)]));
            }
            "EndQuery" => {
                if u(&a, 1) != TIMESTAMP {
                    open.remove(&(heap, index));
                }
                out.push((seq, Vec::new(), vec![pseudo(heap, index)]));
            }
            "ResolveQueryData" => {
                let reads = (index..index + u(&a, 3)).map(|i| pseudo(heap, i)).collect();
                out.push((seq, reads, Vec::new()));
            }
            _ if kind == "draw" || kind == "dispatch" => {
                let writes: Vec<i64> = open
                    .iter()
                    .filter(|(_, l)| **l == list)
                    .map(|(&(h, i), _)| pseudo(h, i))
                    .collect();
                if !writes.is_empty() {
                    out.push((seq, Vec::new(), writes));
                }
            }
            _ => {}
        }
    }
    out
}

/// A query of the game's, begun and not yet ended: its type, the list it is in, and what it counted so far.
struct Open {
    kind: u64,
    list: Option<i64>,
    counts: Vec<u64>,
}

/// What one of replay's own queries counts for.
#[derive(Clone, Copy)]
enum Counted {
    Occlusion,
    Statistics,
    Timestamp,
}

/// Replay's own query heaps, one query each, and the readback buffer they resolve to: read after each event runs.
struct Own {
    occlusion: ID3D12QueryHeap,
    statistics: ID3D12QueryHeap,
    timestamp: ID3D12QueryHeap,
    results: ID3D12Resource,
}

/// The game's open queries an event being run counts into, by what replay counts them with.
struct Counting {
    occlusion: Vec<Query>,
    statistics: Vec<Query>,
}

#[derive(Default)]
pub(super) struct Queries {
    open: HashMap<Query, Open>,
    /// What the game's queries ended in the frame hold.
    ended: HashMap<Query, Vec<u64>>,
    /// The game's open queries the event being run counts into.
    counting: Option<Counting>,
    /// Replay's queries in the open list, and the game's queries each goes to.
    pending: Vec<(Counted, Vec<Query>)>,
    own: Option<Own>,
    /// Timestamps resolved, which hold the replay's GPU clock.
    clock: Resolved,
    /// Results resolved of queries that ended before the frame.
    before: Resolved,
    unsupported: BTreeSet<u64>,
}

/// Results resolved otherwise than the game's: into a readback buffer, which the GPU only copies into and nothing it
/// runs reads, so no output of the frame depends on them; or anywhere else.
#[derive(Default)]
struct Resolved {
    read: usize,
    unread: usize,
}

impl Resolved {
    fn count(&mut self, readback: bool) {
        if readback {
            self.unread += 1;
        } else {
            self.read += 1;
        }
    }
}

const CLOCK: &str = "timestamp results hold the replay's GPU clock, not the game's";
const BEFORE: &str = "query results the frame resolved are of queries that ended before it: the buffers keep their \
                      bytes from the frame's start there";

impl Queries {
    /// What the frame's outputs may depend on.
    pub(super) fn warnings(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.clock.read > 0 {
            out.push(format!(
                "{} {CLOCK}, resolved where the GPU can read them",
                self.clock.read
            ));
        }
        if self.before.read > 0 {
            out.push(format!(
                "{} {BEFORE}, where the GPU can read them",
                self.before.read
            ));
        }
        for kind in &self.unsupported {
            out.push(format!("queries of type {kind} are not replayed"));
        }
        out
    }

    /// What none of them can: results resolved into readback buffers only.
    pub(super) fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.clock.unread > 0 {
            out.push(format!(
                "{} {CLOCK}, in readback buffers the GPU does not read",
                self.clock.unread
            ));
        }
        if self.before.unread > 0 {
            out.push(format!(
                "{} {BEFORE}, in readback buffers the GPU does not read",
                self.before.unread
            ));
        }
        out
    }
}

impl D3d12 {
    fn own_queries(&mut self) -> Result<&Own, String> {
        if self.queries.own.is_none() {
            let heap = |kind| -> Result<ID3D12QueryHeap, String> {
                let mut h: Option<ID3D12QueryHeap> = None;
                unsafe {
                    self.device.CreateQueryHeap(
                        &D3D12_QUERY_HEAP_DESC {
                            Type: kind,
                            Count: 1,
                            NodeMask: 0,
                        },
                        &mut h,
                    )
                }
                .map_err(|e| format!("a query heap of type {}: {e}", kind.0))?;
                h.ok_or_else(|| "no query heap".into())
            };
            let (occlusion, statistics, timestamp) = (
                heap(D3D12_QUERY_HEAP_TYPE_OCCLUSION)?,
                heap(D3D12_QUERY_HEAP_TYPE_PIPELINE_STATISTICS)?,
                heap(D3D12_QUERY_HEAP_TYPE_TIMESTAMP)?,
            );
            let mut results: Option<ID3D12Resource> = None;
            unsafe {
                self.device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_READBACK,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: RESULTS,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut results,
                )
            }
            .map_err(|e| format!("the query results buffer: {e}"))?;
            self.queries.own = Some(Own {
                occlusion,
                statistics,
                timestamp,
                results: results.ok_or("no query results buffer")?,
            });
        }
        Ok(self.queries.own.as_ref().expect("made"))
    }

    pub(super) fn query_begin(&mut self, a: &Value, list: Option<i64>) {
        let (heap, kind, index) = (u(a, 0) as i64, u(a, 1), u(a, 2));
        let counts = match kind {
            OCCLUSION | BINARY_OCCLUSION => vec![0],
            PIPELINE_STATISTICS => vec![0; STATISTICS],
            _ => {
                self.queries.unsupported.insert(kind);
                return;
            }
        };
        self.queries
            .open
            .insert((heap, index), Open { kind, list, counts });
    }

    pub(super) fn query_end(&mut self, a: &Value) -> Result<(), String> {
        let (heap, kind, index) = (u(a, 0) as i64, u(a, 1), u(a, 2));
        if kind == TIMESTAMP {
            let list = self.list.clone();
            let own = self.own_queries()?;
            unsafe {
                list.EndQuery(&own.timestamp, D3D12_QUERY_TYPE_TIMESTAMP, 0);
                list.ResolveQueryData(
                    &own.timestamp,
                    D3D12_QUERY_TYPE_TIMESTAMP,
                    0,
                    1,
                    &own.results,
                    AT_TIMESTAMP,
                );
            }
            self.queries
                .pending
                .push((Counted::Timestamp, vec![(heap, index)]));
        } else if let Some(open) = self.queries.open.remove(&(heap, index)) {
            self.queries.ended.insert((heap, index), open.counts);
        }
        // One begun before the frame has counted what replay cannot know: a resolve of it keeps the buffer's bytes.
        Ok(())
    }

    /// Before a draw or dispatch of the game's list `list`: begins replay's queries for its queries open there.
    pub(super) fn count_begin(&mut self, list: Option<i64>) -> Result<(), String> {
        let (mut occlusion, mut statistics) = (Vec::new(), Vec::new());
        for (&key, open) in &self.queries.open {
            if open.list == list {
                if open.kind == PIPELINE_STATISTICS {
                    statistics.push(key);
                } else {
                    occlusion.push(key);
                }
            }
        }
        if occlusion.is_empty() && statistics.is_empty() {
            return Ok(());
        }
        let list = self.list.clone();
        let own = self.own_queries()?;
        unsafe {
            if !occlusion.is_empty() {
                list.BeginQuery(&own.occlusion, D3D12_QUERY_TYPE_OCCLUSION, 0);
            }
            if !statistics.is_empty() {
                list.BeginQuery(&own.statistics, D3D12_QUERY_TYPE_PIPELINE_STATISTICS, 0);
            }
        }
        self.queries.counting = Some(Counting {
            occlusion,
            statistics,
        });
        Ok(())
    }

    /// After the draw or dispatch: ends the queries [`count_begin`](Self::count_begin) began.
    pub(super) fn count_end(&mut self) -> Result<(), String> {
        let Some(Counting {
            occlusion,
            statistics,
        }) = self.queries.counting.take()
        else {
            return Ok(());
        };
        let list = self.list.clone();
        let own = self.own_queries()?;
        unsafe {
            if !occlusion.is_empty() {
                list.EndQuery(&own.occlusion, D3D12_QUERY_TYPE_OCCLUSION, 0);
                list.ResolveQueryData(
                    &own.occlusion,
                    D3D12_QUERY_TYPE_OCCLUSION,
                    0,
                    1,
                    &own.results,
                    AT_OCCLUSION,
                );
            }
            if !statistics.is_empty() {
                list.EndQuery(&own.statistics, D3D12_QUERY_TYPE_PIPELINE_STATISTICS, 0);
                list.ResolveQueryData(
                    &own.statistics,
                    D3D12_QUERY_TYPE_PIPELINE_STATISTICS,
                    0,
                    1,
                    &own.results,
                    AT_STATISTICS,
                );
            }
        }
        if !occlusion.is_empty() {
            self.queries.pending.push((Counted::Occlusion, occlusion));
        }
        if !statistics.is_empty() {
            self.queries.pending.push((Counted::Statistics, statistics));
        }
        Ok(())
    }

    /// Once the event's list has run: adds what replay's queries counted to the game's.
    pub(super) fn collect_queries(&mut self) -> Result<(), String> {
        if self.queries.pending.is_empty() {
            return Ok(());
        }
        let results = self.own_queries()?.results.clone();
        let mut words = [0u64; (RESULTS / 8) as usize];
        unsafe {
            let mut p = core::ptr::null_mut();
            results
                .Map(
                    0,
                    Some(&D3D12_RANGE {
                        Begin: 0,
                        End: RESULTS as usize,
                    }),
                    Some(&mut p),
                )
                .map_err(|e| format!("query results Map: {e}"))?;
            core::ptr::copy_nonoverlapping(p.cast::<u64>(), words.as_mut_ptr(), words.len());
            results.Unmap(0, Some(&D3D12_RANGE::default()));
        }
        let at = |offset: u64| (offset / 8) as usize;
        for (counted, targets) in std::mem::take(&mut self.queries.pending) {
            for key in targets {
                match counted {
                    Counted::Timestamp => {
                        self.queries.ended.insert(key, vec![words[at(AT_TIMESTAMP)]]);
                    }
                    Counted::Occlusion => {
                        if let Some(open) = self.queries.open.get_mut(&key) {
                            open.counts[0] += words[at(AT_OCCLUSION)];
                        }
                    }
                    Counted::Statistics => {
                        if let Some(open) = self.queries.open.get_mut(&key) {
                            let counted = &words[at(AT_STATISTICS)..at(AT_STATISTICS) + STATISTICS];
                            for (sum, n) in open.counts.iter_mut().zip(counted) {
                                *sum += n;
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// ResolveQueryData: writes what each query in its range holds into the buffer at the offset, as the game's
    /// resolve did (eight bytes each; eleven counters of eight for pipeline statistics).
    pub(super) fn resolve_queries(&mut self, a: &Value) -> Result<(), String> {
        let (heap, kind, start, count, dst, offset) =
            (u(a, 0) as i64, u(a, 1), u(a, 2), u(a, 3), u(a, 4) as i64, u(a, 5));
        if !matches!(
            kind,
            OCCLUSION | BINARY_OCCLUSION | TIMESTAMP | PIPELINE_STATISTICS
        ) {
            self.queries.unsupported.insert(kind);
            return Ok(());
        }
        let size = if kind == PIPELINE_STATISTICS {
            8 * STATISTICS as u64
        } else {
            8
        };
        let readback = matches!(self.made(dst)?.heap, super::Heap::Readback);
        // Runs of results known, written as they end: an unknown one keeps the buffer's bytes there.
        let mut runs: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut run: Option<(u64, Vec<u8>)> = None;
        for i in 0..count {
            match self.queries.ended.get(&(heap, start + i)) {
                Some(values) => {
                    if kind == TIMESTAMP {
                        self.queries.clock.count(readback);
                    }
                    let (_, bytes) = run.get_or_insert_with(|| (i, Vec::new()));
                    for (k, &v) in values.iter().enumerate() {
                        let v = if kind == BINARY_OCCLUSION && k == 0 {
                            u64::from(v > 0)
                        } else {
                            v
                        };
                        bytes.extend_from_slice(&v.to_le_bytes());
                    }
                }
                None => {
                    self.queries.before.count(readback);
                    runs.extend(run.take());
                }
            }
        }
        runs.extend(run);
        for (first, bytes) in runs {
            self.write_bytes(dst, offset + first * size, &bytes)?;
        }
        Ok(())
    }

    /// Writes `bytes` at byte `offset` of buffer `id`: through its mapping where the CPU reaches it, else copied in
    /// from an upload buffer in the open list.
    fn write_bytes(&mut self, id: i64, offset: u64, bytes: &[u8]) -> Result<(), String> {
        let m = self.made(id)?;
        if m.heap.mapped() {
            return self.map_write(id, offset, bytes);
        }
        let r = m.resource.clone();
        let (upload, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, bytes.len() as u64)?;
        unsafe {
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), p, bytes.len());
            self.list
                .CopyBufferRegion(&r, offset, &upload, 0, bytes.len() as u64);
        }
        Ok(())
    }
}
