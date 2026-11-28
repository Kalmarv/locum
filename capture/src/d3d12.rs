//! Owned, portable D3D12 command history. A list recording and an execution of that
//! recording have different identities; resetting a list cannot change an earlier execution.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use rusqlite::{Transaction, params};
use serde::Serialize;
use serde_json::Value;

use crate::db;
mod bindings;
mod graphics;
pub mod indirect;
pub mod programs;
pub mod samples;
pub mod tables;

pub use bindings::{FLAGS, reproject};

/// Makes `d12_draw_descriptors` and writes `rows` into it: a capture's, or `upgrade`'s for a capture from before it.
pub fn write_draw_descriptors(tx: &Transaction<'_>, rows: &[db::DrawDescriptor]) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE d12_draw_descriptors(draw INTEGER NOT NULL,stage TEXT NOT NULL,kind TEXT NOT NULL,
         slot INTEGER NOT NULL,space INTEGER NOT NULL,parameter INTEGER NOT NULL,heap INTEGER,heap_slot INTEGER,
         version INTEGER,resource INTEGER,address INTEGER,size INTEGER,stride INTEGER,format INTEGER,dimension INTEGER,
         flag TEXT);
         CREATE INDEX d12_draw_descriptors_draw ON d12_draw_descriptors(draw);",
    )?;
    let mut s = tx.prepare(
        "INSERT INTO d12_draw_descriptors VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
    )?;
    let n = |v: Option<u64>| v.map(integer).transpose();
    for d in rows {
        s.execute(params![
            integer(d.draw)?,
            d.stage,
            d.kind,
            d.slot,
            d.space,
            d.parameter,
            n(d.heap)?,
            d.heap_slot,
            n(d.version)?,
            n(d.resource)?,
            n(d.address)?,
            n(d.size)?,
            d.stride,
            d.format,
            d.dimension,
            d.flag
        ])?;
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
pub struct Command {
    pub call: &'static str,
    pub args: Value,
    /// A bundle's closed generation at ExecuteBundle, retained across later resets.
    #[serde(skip)]
    pub bundle: Option<Arc<List>>,
    /// An API not yet represented semantically is visible, never a complete capture.
    pub diagnostic: Option<String>,
}

/// Targets (resource, subresource) a list draws to before writing them otherwise, with the views bound then.
pub type Continues = Vec<((u64, u32), Vec<u64>)>;

#[derive(Clone, Debug, Default, Serialize)]
pub struct List {
    pub id: u64,
    pub generation: u64,
    pub kind: i32,
    pub allocator: u64,
    pub initial_pso: u64,
    pub closed: bool,
    pub commands: Vec<Command>,
    /// Whether this generation was recorded while a capture was armed, so its pass ends were copied.
    #[serde(skip)]
    pub instrumented: bool,
    /// The targets copied inside this generation where its passes ended, and those whose copies its Close left
    /// waiting, copied after it where it is submitted unless a later list continues the pass.
    #[serde(skip)]
    pub checkpoints: Vec<Checkpoint>,
    /// What its draws, dispatches and UAV clears wrote through UAVs, copied inside it right after each.
    #[serde(skip)]
    pub outputs: Vec<Output>,
    /// The argument records of its ExecuteIndirects whose signatures put GPU addresses in them, copied inside it
    /// right before each.
    #[serde(skip)]
    pub arguments: Vec<Arguments>,
    /// The targets (resource, subresource) this generation draws to before writing them any other way, with the
    /// views bound at the first such draw: where an earlier list left the copy of a pass with the same views
    /// waiting, this one continues the pass.
    #[serde(skip)]
    pub continues: Continues,
    /// The resources it writes other than by drawing to them as bound targets (clears, discards, copies, barriers
    /// that discard or allow unordered-access writes).
    #[serde(skip)]
    pub written: Vec<u64>,
    /// The buffers copies of locum's inside it read where its own barriers had not moved them, each once.
    #[serde(skip)]
    pub promoted: Vec<Promoted>,
}

/// A buffer a copy of locum's inside a list read by implicit promotion to COPY_SOURCE: the list's own barriers had
/// not moved it, so the copy joined COPY_SOURCE to the state it was in. `settled`: a later transition of the list's
/// own moved it, and locum first moved it from that transition's before-state with COPY_SOURCE back to that state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Promoted {
    pub resource: u64,
    pub settled: bool,
}

/// A render or depth target copied inside a list where a pass ended, before the command at `at` (after all the
/// commands before it). Its bytes are read once the GPU has run the list.
#[derive(Clone, Debug, Default)]
pub struct Checkpoint {
    pub id: u64,
    pub at: usize,
    /// `rt0` to `rt7`, or `depth`.
    pub target: String,
    pub resource: u64,
    /// The view's descriptor version.
    pub view: u64,
    pub width: u32,
    pub height: u32,
    /// The view's format.
    pub format: u32,
    pub sub: u32,
    /// Depth slice of a volume target.
    pub z: u32,
    /// Why no copy was made.
    pub gap: Option<String>,
}

/// A resource a draw, dispatch or UAV clear (the command at `command`) wrote through a UAV, copied inside its list
/// right after it, before the command at `at`. Its bytes are read once the GPU has run the list.
#[derive(Clone, Debug, Default)]
pub struct Output {
    pub id: u64,
    pub at: usize,
    pub command: usize,
    pub resource: u64,
    /// The subresource copied (a texture's, one a view covers); None, a buffer.
    pub sub: Option<u32>,
    /// Of a buffer, the bytes copied (offset, length): what the views it was written through cover; None, all of it.
    pub range: Option<(u64, u64)>,
    /// Why no copy was made.
    pub gap: Option<String>,
}

/// The argument records of an ExecuteIndirect (the command at `command`) whose signature puts GPU addresses in them,
/// with its count when it has a count buffer, copied inside its list right before it: `bytes` of records from the
/// argument offset, then the count's 4 bytes. They are read after the frame, for the buffers the addresses are in. A
/// bundle's ExecuteIndirect has none: D3D12 lets a bundle run only a signature of one draw or dispatch, without a
/// count buffer, so its records name nothing and are read as any buffer is.
///
/// Or the shader tables of a DispatchRays (`tables` holds them): each table's `bytes` one after another, copied right
/// before it, read after the frame for what their records' local root arguments name (see [`tables`]).
#[derive(Clone, Debug, Default)]
pub struct Arguments {
    pub id: u64,
    pub command: usize,
    pub signature: u64,
    pub max_count: u64,
    pub arg_buffer: u64,
    pub arg_offset: u64,
    pub count_buffer: u64,
    pub count_offset: u64,
    pub layout: indirect::Layout,
    pub bytes: u64,
    pub counted: bool,
    pub tables: Vec<tables::Table>,
    /// A top-level build's instances given as an array of pointers: how many pointers were copied (see
    /// [`instance_pointers`]), and the GPU address they are at.
    pub pointers: u32,
    pub pointers_at: u64,
    /// Why no copy was made.
    pub gap: Option<String>,
}

impl Arguments {
    /// The records the copied bytes `copy` hold that the call runs: as many as fit, and no more than its count.
    pub fn records(&self, copy: &[u8]) -> usize {
        let fit = (self.bytes as usize) / self.layout.stride.max(1);
        let count = self
            .counted
            .then(|| copy.get(self.bytes as usize..self.bytes as usize + 4))
            .flatten()
            .map_or(usize::MAX, |c| {
                u32::from_le_bytes(c.try_into().expect("4 bytes")) as usize
            });
        fit.min(count)
    }
}

/// How many instances a top-level build (`build`, its recorded description) gives as an array of pointers (each the
/// GPU address of one `D3D12_RAYTRACING_INSTANCE_DESC`): 0 for any other build.
pub fn instance_pointers(build: &Value) -> u64 {
    const ARRAY_OF_POINTERS: i64 = 1;
    let inputs = &build["inputs"];
    if inputs["type"] == 0 && inputs["layout"].as_i64() == Some(ARRAY_OF_POINTERS) {
        inputs["count"].as_u64().unwrap_or(0)
    } else {
        0
    }
}

/// The GPU addresses an array of instance pointers holds (`bytes`, as copied).
pub fn pointers(bytes: &[u8]) -> impl Iterator<Item = u64> + '_ {
    bytes.as_chunks::<8>().0.iter().map(|p| u64::from_le_bytes(*p))
}

/// An ExecuteIndirect event whose argument records were copied, a DispatchRays whose shader tables were, or a
/// top-level build whose instance pointers were (copy `arguments.id`), executed in `submission` on `queue`.
#[derive(Clone, Debug)]
pub struct Indirect {
    pub event: u64,
    pub submission: u64,
    pub queue: u64,
    /// The raytracing state object the call ran with (SetPipelineState1), or 0.
    pub state_object: u64,
    pub arguments: Arguments,
}

/// Kept beside each live list, rather than under the process-wide identity lock.
#[derive(Default)]
pub struct Recording {
    current: Arc<List>,
}

impl Recording {
    pub fn new(id: u64, kind: i32, allocator: u64, pso: u64, closed: bool) -> Self {
        Self {
            current: Arc::new(List {
                id,
                kind,
                allocator,
                initial_pso: pso,
                closed,
                ..List::default()
            }),
        }
    }

    pub fn reset(&mut self, allocator: u64, pso: u64) {
        self.current = Arc::new(List {
            id: self.current.id,
            generation: self.current.generation + 1,
            kind: self.current.kind,
            allocator,
            initial_pso: pso,
            ..List::default()
        });
    }

    pub fn close(&mut self) {
        Arc::make_mut(&mut self.current).closed = true;
    }

    pub fn instrument(&mut self) {
        Arc::make_mut(&mut self.current).instrumented = true;
    }

    pub fn checkpoint(&mut self, checkpoint: Checkpoint) {
        Arc::make_mut(&mut self.current).checkpoints.push(checkpoint);
    }

    pub fn output(&mut self, output: Output) {
        Arc::make_mut(&mut self.current).outputs.push(output);
    }

    pub fn arguments(&mut self, arguments: Arguments) {
        Arc::make_mut(&mut self.current).arguments.push(arguments);
    }

    /// A copy of locum's read buffer `resource` by implicit promotion (see [`Promoted`]).
    pub fn promote(&mut self, resource: u64) {
        let promoted = &mut Arc::make_mut(&mut self.current).promoted;
        if !promoted.iter().any(|p| p.resource == resource && !p.settled) {
            promoted.push(Promoted {
                resource,
                settled: false,
            });
        }
    }

    /// The buffers copies of locum's read by implicit promotion so far.
    pub fn promoted(&self) -> &[Promoted] {
        &self.current.promoted
    }

    /// Whether buffer `resource` is promoted and not yet settled; it is settled from now.
    pub fn settle(&mut self, resource: u64) -> bool {
        if !self
            .current
            .promoted
            .iter()
            .any(|p| p.resource == resource && !p.settled)
        {
            return false;
        }
        let promoted = &mut Arc::make_mut(&mut self.current).promoted;
        for p in promoted.iter_mut().filter(|p| p.resource == resource) {
            p.settled = true;
        }
        true
    }

    /// What the generation's pass tracking learned of the targets it continues and the resources it names.
    pub fn touches(&mut self, continues: Continues, written: Vec<u64>) {
        let current = Arc::make_mut(&mut self.current);
        current.continues = continues;
        current.written = written;
    }

    /// Commands recorded so far.
    pub fn len(&self) -> usize {
        self.current.commands.len()
    }

    pub fn is_empty(&self) -> bool {
        self.current.commands.is_empty()
    }

    pub fn record(&mut self, command: Command) {
        Arc::make_mut(&mut self.current).commands.push(command);
    }

    pub fn snapshot(&self) -> Arc<List> {
        self.current.clone()
    }
}

#[derive(Clone, Debug)]
pub struct Object {
    pub id: u64,
    pub kind: &'static str,
    pub device: u64,
    pub description: Value,
}

#[derive(Clone, Debug)]
pub struct Submission {
    pub id: u64,
    pub queue: u64,
    pub queue_seq: u64,
    pub cpu_seq: u64,
    pub lists: Vec<Arc<List>>,
    /// The definitions descriptor slots took since the capture's previous submission (every defined slot at its
    /// first); kind `empty` marks a slot emptied.
    pub descriptors: Vec<Descriptor>,
    pub uploads: Vec<(u64, Value)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Descriptor {
    pub id: u64,
    pub heap: u64,
    pub slot: u32,
    pub kind: &'static str,
    pub resource: u64,
    pub counter: u64,
    pub description: Value,
}

#[derive(Clone, Debug)]
pub struct Sync {
    pub queue: u64,
    pub queue_seq: u64,
    pub cpu_seq: u64,
    pub kind: &'static str,
    pub fence: u64,
    pub value: u64,
}

/// A reserved resource's tile mappings as the frame found them: when its first use or the frame's first change to
/// its mappings saw it (`cpu_seq`), its tile numbering ([`crate::tiles::Layout::json`]) and page table
/// ([`crate::tiles::Pages::runs`]); or why they are not known.
#[derive(Clone, Debug)]
pub struct TilePages {
    pub resource: u64,
    pub cpu_seq: u64,
    pub pages: Result<(Value, Value), String>,
}

/// A change to tile mappings in the frame, in its queue's order: `UpdateTileMappings` or `CopyTileMappings`, its
/// arguments (resources and heaps by id), and the destination's page table after it, or why that is not known.
#[derive(Clone, Debug)]
pub struct TileMapping {
    pub queue: u64,
    pub queue_seq: u64,
    pub cpu_seq: u64,
    pub call: &'static str,
    pub resource: u64,
    pub source: Option<u64>,
    pub args: Value,
    pub after: Result<Value, String>,
}

/// An acceleration structure the frame began with (in buffer `resource` at `offset`): its latest build before the
/// frame, as `BuildRaytracingAccelerationStructure` described it, and the inputs that build read, each
/// `{part, geometry, resource, offset, bytes, blob}`; or why they are not known.
#[derive(Clone, Debug)]
pub struct AccelerationStructure {
    pub resource: u64,
    pub offset: u64,
    pub build: Value,
    pub inputs: Result<Value, String>,
}

/// A submission selected for complete pass capture, with its active bindless heap versions and referenced resources.
#[derive(Clone, Debug)]
pub struct SelectedPass {
    pub submission: u64,
    pub queue: u64,
    pub cpu_seq: u64,
    pub reason: String,
    pub bindless_heaps: Value,
    pub bindless_max_version: u64,
    pub resources: Value,
}

/// Captured ExecuteIndirect argument buffer bytes and optional count value for one event.
#[derive(Clone, Debug)]
pub struct IndirectArguments {
    pub event: u64,
    pub submission: u64,
    pub queue: u64,
    pub signature: u64,
    pub max_count: u64,
    pub arg_buffer: u64,
    pub arg_offset: u64,
    pub arg_bytes: u64,
    pub count_buffer: u64,
    pub count_offset: u64,
    pub count_value: Option<u32>,
    pub stride: u32,
    pub blob: Option<String>,
    pub diagnostic: Option<String>,
}

/// Live fence state snapshot (at capture arming or at a submission), recording completed value and last producer.
#[derive(Clone, Debug)]
pub struct FenceState {
    pub phase: &'static str,
    pub submission: Option<u64>,
    pub cpu_seq: u64,
    pub fence: u64,
    pub completed: u64,
    pub producer_queue: Option<u64>,
    pub producer_submission: Option<u64>,
    pub producer_value: Option<u64>,
    pub producer_cpu_seq: Option<u64>,
}

/// Producer queue/fence wait dependency observed when a submission is queued.
#[derive(Clone, Debug)]
pub struct SubmissionSync {
    pub submission: u64,
    pub queue: u64,
    pub cpu_seq: u64,
    pub fence: u64,
    pub wait_value: u64,
    pub completed_value: Option<u64>,
    pub producer_queue: Option<u64>,
    pub producer_submission: Option<u64>,
    pub producer_value: Option<u64>,
    pub satisfied: bool,
}

/// Post-remap resident tile bytes and page-table snapshot for a reserved buffer referenced by a selected pass.
#[derive(Clone, Debug)]
pub struct TileSnapshot {
    pub submission: u64,
    pub resource: u64,
    pub cpu_seq: u64,
    pub pages_json: Option<String>,
    pub blob: Option<String>,
    pub diagnostic: Option<String>,
}

/// Bindless descriptor heap slot referenced by a draw or dispatch (via root constants or descriptor tables
/// on a directly-indexed root signature).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BindlessUsage {
    pub draw: u64,
    pub event: u64,
    pub heap: u64,
    pub slot: u32,
    pub kind: String,
    pub resource: u64,
    pub access: &'static str,
    pub source: &'static str,
}

/// Structured annotation keyed to an event sequence number, command list, or submission (from in-stream
/// PIX/D3D12 structured markers or an external JSONL sidecar such as XCL's D3D12.X / barrier / placement log).
#[derive(Clone, Debug, PartialEq)]
pub struct Annotation {
    pub seq: Option<u64>,
    pub list: Option<u64>,
    pub submission: Option<u64>,
    pub category: String,
    pub payload: Value,
}

#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub objects: Vec<Object>,
    pub submissions: Vec<Submission>,
    pub sync: Vec<Sync>,
    pub tile_pages: Vec<TilePages>,
    pub tile_mappings: Vec<TileMapping>,
    pub acceleration_structures: Vec<AccelerationStructure>,
    pub selected_passes: Vec<SelectedPass>,
    pub indirect_arguments: Vec<IndirectArguments>,
    pub fence_states: Vec<FenceState>,
    pub submission_syncs: Vec<SubmissionSync>,
    pub tile_snapshots: Vec<TileSnapshot>,
    pub bindless_usage: Vec<BindlessUsage>,
    pub annotations: Vec<Annotation>,
    pub diagnostics: Vec<String>,
}

/// What shader tables are read against (see [`named_by_tables`]): objects by id as described (state objects, root
/// signatures, descriptor heaps), what a DXIL library declares by its blob key, a local root signature's decoded
/// description, and the live buffers a GPU virtual address is in.
pub struct Tables<'a> {
    pub objects: &'a BTreeMap<u64, &'a Value>,
    pub declared: &'a dyn Fn(&str) -> Result<Arc<programs::Declared>, String>,
    pub decode: &'a mut dyn FnMut(&programs::Local) -> Result<Value, String>,
    pub buffers_at: &'a dyn Fn(u64) -> Vec<u64>,
}

/// What shader tables name: the resources a DispatchRays reads through them, those it may write (a UAV's, and its
/// counter's), and what named nothing that could be placed, by reason.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TableNames {
    pub reads: Vec<u64>,
    pub writes: Vec<u64>,
    pub unplaced: BTreeMap<String, u64>,
}

/// What the shader tables a DispatchRays ran with name (`tables`, their bytes `bytes`, one after another): read by
/// [`tables::read`] against state object `state_object` and `read`'s objects, each root CBV, SRV or UAV's buffers,
/// and the resources the descriptors a descriptor table covers view (`descriptors`, the slots as the DispatchRays'
/// submission found them).
pub fn named_by_tables<'d>(
    read: Tables,
    state_object: u64,
    tables: &[tables::Table],
    bytes: &[u8],
    descriptors: impl FnOnce() -> BTreeMap<(u64, u32), &'d Descriptor>,
) -> TableNames {
    struct Objects<'a> {
        objects: &'a BTreeMap<u64, &'a Value>,
        declared: &'a dyn Fn(&str) -> Result<Arc<programs::Declared>, String>,
    }
    impl programs::Source for Objects<'_> {
        fn description(&self, id: u64) -> Option<Value> {
            self.objects.get(&id).map(|&d| d.clone())
        }
        fn declared(&self, blob: &str) -> Result<Arc<programs::Declared>, String> {
            (self.declared)(blob)
        }
    }
    let Tables {
        objects,
        declared,
        decode,
        buffers_at,
    } = read;
    let source = Objects { objects, declared };
    let mut named = tables::read(&source, state_object, tables, bytes, decode);
    let (mut reads, mut writes) = (BTreeSet::new(), BTreeSet::new());
    let mut unplaced = |why: &str| *named.unplaced.entry(why.to_string()).or_default() += 1;
    for &(address, kind) in &named.addresses {
        let found = buffers_at(address);
        if found.is_empty() {
            unplaced("a root descriptor's address is in no live buffer");
        }
        let into = if kind == programs::Kind::UnorderedAccess {
            &mut writes
        } else {
            &mut reads
        };
        into.extend(found);
    }
    if !named.handles.is_empty() {
        let heaps: Vec<(u64, &Value)> = objects
            .iter()
            .filter(|(_, d)| d["gpu_start"].as_u64().is_some_and(|g| g != 0))
            .map(|(&id, &d)| (id, d))
            .collect();
        let descriptors = descriptors();
        for (handle, ranges) in &named.handles {
            let Some((heap, base)) = tables::slot(heaps.iter().copied(), *handle) else {
                unplaced("a descriptor table's handle is in no shader-visible heap");
                continue;
            };
            let count = objects[&heap]["count"].as_u64().unwrap_or(0) as u32;
            for (start, end, kind) in tables::ranges(ranges, base, count) {
                for d in descriptors.range((heap, start)..(heap, end)).map(|(_, d)| d) {
                    if d.kind != kind {
                        continue;
                    }
                    // An acceleration structure's view names its buffer by address.
                    let viewed = Some(d.resource).filter(|&r| r != 0).or_else(|| {
                        d.description["payload"]["address"]["resource"]
                            .as_u64()
                            .filter(|&r| r != 0)
                    });
                    if kind == "uav" {
                        writes.extend(viewed);
                        writes.extend(Some(d.counter).filter(|&c| c != 0));
                    } else {
                        reads.extend(viewed);
                    }
                }
            }
        }
    }
    let reads = reads.difference(&writes).copied().collect();
    TableNames {
        reads,
        writes: writes.into_iter().collect(),
        unplaced: named.unplaced,
    }
}

/// The database extension and the common-table projection share this one prepared plan.
pub struct Prepared {
    frame: Frame,
    projection: Projection,
    first_use: BTreeMap<u64, FirstUse>,
    /// Checkpoint copies read back: the key of each target's bytes, or why there are none.
    checkpoints: BTreeMap<u64, Result<String, String>>,
    /// Checkpoints a list's Close made whose pass the next list submitted with it continued: no copy, and no gap.
    continued: BTreeSet<u64>,
    /// What ExecuteIndirect argument records named (see [`Prepared::named_by_arguments`]).
    named: Named,
}

/// What the frame's ExecuteIndirect argument records named, read after the frame, and the copies made then.
#[derive(Default)]
struct Named {
    /// Each buffer the records name, with the submission and queue of the first ExecuteIndirect naming it.
    first: BTreeMap<u64, (u64, u64)>,
    /// (event, buffer): a root UAV in the event's records names the buffer, so the event may write it.
    writes: BTreeSet<(u64, u64)>,
    /// Each buffer copied after the frame, and what the copy is for.
    planned: BTreeMap<u64, AfterFrame>,
    /// The buffers whose copy after the frame is their contents as the frame began.
    contents: BTreeSet<u64>,
    /// What ExecuteIndirect events wrote through root UAVs their records set: (event, buffer) and the key of the
    /// buffer's bytes after the frame, or why there are none.
    outputs: BTreeMap<(u64, u64), Result<String, String>>,
    /// Such writes no copy holds, by reason.
    uncopied: BTreeMap<String, u64>,
}

/// What a copy of a buffer after the frame is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterFrame {
    /// Its contents as the frame began: only records name it, and nothing the frame recorded writes it.
    Contents,
    /// What ExecuteIndirect event `0` wrote to it through a root UAV its records set: no later event writes it.
    Output(u64),
}

/// A buffer to copy whole after the frame (see [`Prepared::after_frame`]), on `queue`, filed at `submission`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AfterFrameCopy {
    pub resource: u64,
    pub submission: u64,
    pub queue: u64,
    pub layout: Vec<crate::layout::Sub>,
}

/// A resource's contents as its first use in the frame found them, copied on the GPU just before the submission that
/// first used it, or why they could not be.
#[derive(Clone, Debug)]
pub struct FirstUse {
    pub submission: u64,
    /// The state or layout of each subresource the copy began from (empty for buffers, which are always COMMON
    /// between submissions).
    pub states: Value,
    /// The key of the bytes in the canonical layout, and each subresource's key; or the reason there are none.
    pub content: Result<(String, Vec<String>), String>,
    /// How copied contents may differ from those the frame began with (the resource's `content_note`).
    pub note: Option<String>,
}

impl Prepared {
    /// A checkpoint's bytes, read back after the GPU ran its list.
    pub fn set_checkpoint(&mut self, id: u64, content: Result<String, String>) {
        self.checkpoints.insert(id, content);
    }
    /// A checkpoint whose pass the next list continued, so it ended later.
    pub fn set_continued(&mut self, id: u64) {
        self.continued.insert(id);
    }
    /// A resource's contents before its first use.
    pub fn set_first_use(&mut self, resource: u64, first_use: FirstUse) {
        self.first_use.insert(resource, first_use);
    }
    pub fn set_indirect_arguments(&mut self, arguments: IndirectArguments) {
        self.frame.indirect_arguments.push(arguments);
    }
    pub fn add_tile_snapshot(&mut self, snapshot: TileSnapshot) {
        self.frame.tile_snapshots.push(snapshot);
    }
    pub fn add_annotations(&mut self, rows: Vec<Annotation>) {
        self.frame.annotations.extend(rows);
    }
    /// The frame's ExecuteIndirect events whose argument records were copied.
    pub fn indirect(&self) -> &[Indirect] {
        &self.projection.indirect
    }
    /// What the argument records of ExecuteIndirect `indirect` name: the event reads `reads`, and reads and may write
    /// `writes`, which a root UAV in them names. The frame therefore uses them all, and writes `writes`.
    pub fn named_by_arguments(
        &mut self,
        capture: &mut db::Capture,
        indirect: &Indirect,
        reads: &[u64],
        writes: &[u64],
    ) {
        let event = indirect.event;
        if let Some(e) = capture.events.iter_mut().find(|e| e.seq == event) {
            e.reads.extend(reads.iter().chain(writes));
            e.reads.sort_unstable();
            e.reads.dedup();
            e.writes.extend(writes);
            e.writes.sort_unstable();
            e.writes.dedup();
        }
        for r in capture
            .resources
            .iter_mut()
            .filter(|r| reads.contains(&r.id) || writes.contains(&r.id))
        {
            r.used = true;
            r.written |= writes.contains(&r.id);
        }
        for &r in reads.iter().chain(writes) {
            self.named
                .first
                .entry(r)
                .or_insert((indirect.submission, indirect.queue));
        }
        self.named.writes.extend(writes.iter().map(|&w| (event, w)));
    }

    /// Object `id` as the frame last described it.
    pub fn description(&self, id: u64) -> Option<&Value> {
        self.frame
            .objects
            .iter()
            .rev()
            .find(|o| o.id == id)
            .map(|o| &o.description)
    }

    /// The descriptor definitions of every slot as of submission `id`: the frame's definitions up to it, the latest
    /// of each slot.
    pub fn descriptors_at(&self, id: u64) -> BTreeMap<(u64, u32), &Descriptor> {
        let mut out = BTreeMap::new();
        for s in &self.frame.submissions {
            for d in &s.descriptors {
                if d.kind == "empty" {
                    out.remove(&(d.heap, d.slot));
                } else {
                    out.insert((d.heap, d.slot), d);
                }
            }
            if s.id == id {
                break;
            }
        }
        out
    }

    /// What the shader tables DispatchRays event `rays` ran with name (see [`named_by_tables`]), against the frame's
    /// objects as last described and its descriptors as of the event's submission.
    pub fn named_by_tables(
        &self,
        rays: &Indirect,
        tables: &[tables::Table],
        bytes: &[u8],
        declared: &dyn Fn(&str) -> Result<Arc<programs::Declared>, String>,
        decode: &mut dyn FnMut(&programs::Local) -> Result<Value, String>,
        buffers_at: &dyn Fn(u64) -> Vec<u64>,
    ) -> TableNames {
        let objects: BTreeMap<u64, &Value> = self
            .frame
            .objects
            .iter()
            .map(|o| (o.id, &o.description))
            .collect();
        let read = Tables {
            objects: &objects,
            declared,
            decode,
            buffers_at,
        };
        named_by_tables(read, rays.state_object, tables, bytes, || {
            self.descriptors_at(rays.submission)
        })
    }

    /// The buffers to copy whole after the frame, once every ExecuteIndirect's records are known
    /// ([`Prepared::named_by_arguments`]); `copied` says whether a resource's contents were copied (or given up)
    /// before the frame first used it.
    ///
    /// A buffer only records name has no copy from before its first use, since no submission was seen to use it. If
    /// nothing the frame recorded writes it (records' root UAVs included), what it holds after the frame is what it
    /// held as the frame began: copied on the queue of the first ExecuteIndirect naming it, as its contents at that
    /// one's submission. If the frame writes it, what it held as the frame began is not known.
    ///
    /// What an ExecuteIndirect wrote through a root UAV its records set was not copied right after it, since which
    /// buffer that is is learned only now. Where no later event of the frame writes the buffer, and every event that
    /// writes it ran on that ExecuteIndirect's queue, the buffer after the frame holds what it wrote: copied on that
    /// queue, as its output. Otherwise its output is not known.
    pub fn after_frame(
        &mut self,
        capture: &db::Capture,
        copied: impl Fn(u64) -> bool,
    ) -> Vec<AfterFrameCopy> {
        let queues: HashMap<u64, u64> = self
            .projection
            .locations
            .iter()
            .map(|&(seq, _, _, queue)| (seq, queue))
            .collect();
        let submissions: HashMap<u64, u64> = self
            .projection
            .indirect
            .iter()
            .map(|i| (i.event, i.submission))
            .collect();
        let mut writers = BTreeMap::<u64, Vec<u64>>::new();
        for e in &capture.events {
            for &r in &e.writes {
                writers.entry(r).or_default().push(e.seq);
            }
        }
        let descriptions: BTreeMap<u64, &Value> = self
            .frame
            .objects
            .iter()
            .map(|o| (o.id, &o.description))
            .collect();
        let mut planned = BTreeMap::<u64, (AfterFrame, u64, u64)>::new();
        for (&resource, &(submission, queue)) in &self.named.first {
            if copied(resource) {
                continue;
            }
            let why = if writers.contains_key(&resource) {
                "only ExecuteIndirect argument records, shader tables or instance pointers name it, and a UAV they \
                 name lets the frame write it: what it held as the frame began was not copied (after the frame it \
                 holds what the frame wrote)"
            } else {
                match descriptions.get(&resource) {
                    None => {
                        "only ExecuteIndirect argument records, shader tables or instance pointers name it, and its \
                         description is not known"
                    }
                    // An upload buffer's contents are its copy at the frame's first submission; only the GPU writes a
                    // readback buffer (as `contents` says).
                    Some(d) if matches!(d["heap_properties"]["type"].as_u64(), Some(2 | 3)) => continue,
                    Some(d) if d["dimension"] != 1 => {
                        "an argument record's address names it, but it is not a buffer"
                    }
                    Some(_) => {
                        planned.insert(resource, (AfterFrame::Contents, submission, queue));
                        continue;
                    }
                }
            };
            self.first_use.insert(
                resource,
                FirstUse {
                    submission,
                    states: serde_json::json!([]),
                    content: Err(why.into()),
                    note: None,
                },
            );
        }
        for &(event, resource) in &self.named.writes {
            let queue = queues.get(&event).copied();
            let all = writers.get(&resource).map_or(&[][..], Vec::as_slice);
            let why = if all.iter().any(|&w| w > event) {
                "the frame writes it again later"
            } else if all.iter().any(|w| queues.get(w) != queue.as_ref()) {
                "the frame also writes it on another queue"
            } else if descriptions.get(&resource).is_none_or(|d| d["dimension"] != 1) {
                "it is not a buffer locum knows"
            } else if let (Some(queue), Some(&submission)) = (queue, submissions.get(&event)) {
                planned.insert(resource, (AfterFrame::Output(event), submission, queue));
                continue;
            } else {
                "its event's queue is not known"
            };
            *self
                .named
                .uncopied
                .entry(format!(
                    "an ExecuteIndirect wrote it through a root UAV its records set, which is learned only after the \
                     frame, and {why}: what that ExecuteIndirect wrote was not copied"
                ))
                .or_default() += 1;
        }
        let mut out = Vec::new();
        for (resource, (purpose, submission, queue)) in planned {
            // A buffer always has one.
            let Some(layout) = descriptions.get(&resource).and_then(|d| layout(d)) else {
                continue;
            };
            self.named.planned.insert(resource, purpose);
            out.push(AfterFrameCopy {
                resource,
                submission,
                queue,
                layout,
            });
        }
        out
    }

    /// A copy [`Prepared::after_frame`] asked for, made after the frame: its bytes, or why there are none.
    pub fn set_after_frame(&mut self, resource: u64, copy: FirstUse) {
        match self.named.planned.get(&resource) {
            Some(AfterFrame::Contents) => {
                if copy.content.is_ok() {
                    self.named.contents.insert(resource);
                }
                self.first_use.insert(resource, copy);
            }
            Some(&AfterFrame::Output(event)) => {
                self.named
                    .outputs
                    .insert((event, resource), copy.content.map(|(key, _)| key));
            }
            None => {}
        }
    }
    /// An acceleration structure the frame began with, its build's inputs read back after the frame.
    pub fn add_acceleration_structure(&mut self, a: AccelerationStructure) {
        self.frame.acceleration_structures.push(a);
    }
    /// Upload copies a submission made, read back after the frame (in one batch or several).
    pub fn add_uploads(&mut self, submission: u64, uploads: Vec<(u64, Value)>) {
        if let Some(s) = self.frame.submissions.iter_mut().find(|s| s.id == submission) {
            s.uploads.extend(uploads);
        }
    }
    /// Populate CPU upload contents and per-draw constant bytes after queued blobs are durable.
    pub fn contents(&self, capture: &mut db::Capture, blobs: &crate::blobs::Blobs) {
        // Per submission, runs of upload bytes keyed by where they begin; and each upload buffer whole as the frame
        // began.
        let runs: BTreeMap<_, _> = self
            .frame
            .submissions
            .iter()
            .flat_map(|s| {
                s.uploads
                    .iter()
                    .map(move |(r, v)| ((s.id, *r, v["offset"].as_u64().unwrap_or(0)), v))
            })
            .filter(|(_, v)| v["whole"] != true)
            .collect();
        let whole: BTreeMap<_, _> = self
            .frame
            .submissions
            .iter()
            .flat_map(|s| s.uploads.iter())
            .filter(|(_, v)| v["whole"] == true)
            .map(|(r, v)| (*r, v))
            .collect();
        let draw_events: BTreeMap<_, _> = capture.draws.iter().map(|d| (d.n, d.event)).collect();
        let executions: BTreeMap<_, _> = self
            .projection
            .executions
            .iter()
            .map(|x| (x.id, x.submission))
            .collect();
        let mut written = BTreeMap::<u64, Vec<&db::Event>>::new();
        for event in &capture.events {
            for resource in &event.writes {
                written.entry(*resource).or_default().push(event);
            }
        }
        let composer = Composer {
            runs: &runs,
            blobs,
            first_use: &self.first_use,
            submissions: self
                .projection
                .locations
                .iter()
                .filter_map(|(seq, execution, ..)| Some((*seq, *executions.get(execution)?)))
                .collect(),
            written,
        };
        // A buffer's bytes before a draw depend only on the draw's submission (an upload buffer's bytes are as the GPU
        // reached it) and the writes before it: one composition for the draws that share both.
        type Key = (u64, u64, u64, Option<u64>, usize);
        let mut composed = HashMap::<Key, Result<String, String>>::new();
        let mut unread = BTreeMap::<String, u64>::new();
        for binding in &mut capture.bindings {
            if binding.kind != "cb" || binding.content.is_some() {
                continue;
            }
            let (Some(resource), Some(event)) = (binding.resource, draw_events.get(&binding.draw)) else {
                continue;
            };
            let offset = binding.offset.unwrap_or(0);
            // Upload bytes run only as far as the submission read them: a root CBV's size is the most it can be.
            let len = match (binding.size, composer.run_len(resource, offset, *event)) {
                (Some(size), Some(run)) => size.min(run),
                (Some(n), None) | (None, Some(n)) => n,
                (None, None) => continue,
            };
            let writes = composer.writes(resource, *event).len();
            let submission = composer.submissions.get(event).copied();
            let content = composed
                .entry((resource, offset, len, submission, writes))
                .or_insert_with(|| {
                    composer.at(resource, offset, len, *event, 0).map(|bytes| {
                        let key = crate::hash::key(&bytes);
                        capture.cb_data.push((key.clone(), bytes));
                        key
                    })
                });
            match content {
                Ok(key) => binding.content = Some(key.clone()),
                Err(why) => *unread.entry(why.clone()).or_default() += 1,
            }
        }
        let descriptions: BTreeMap<_, _> = self
            .frame
            .objects
            .iter()
            .map(|o| (o.id, &o.description))
            .collect();
        let mut missing = BTreeMap::<String, u64>::new();
        let mut unbuilt = BTreeMap::<String, u64>::new();
        let mut subresources = Vec::new();
        for resource in &mut capture.resources {
            let mut hashes = Vec::new();
            if let Some(content) = whole.get(&resource.id)
                && let Some(key) = content["blob"].as_str()
                && blobs.has(key)
            {
                resource.content = Some(key.into());
                resource.content_source = Some("upload-at-first-submission");
                resource.content_note =
                    Some("upload bytes as the GPU reached the frame's first captured submission".into());
            } else if let Some(first) = self.first_use.get(&resource.id) {
                match &first.content {
                    Ok((key, subs)) if self.named.contents.contains(&resource.id) => {
                        resource.content = Some(key.clone());
                        resource.content_source = Some("after-frame");
                        resource.content_note = Some(
                            "copied after the frame: only ExecuteIndirect argument records, shader tables or instance \
                             pointers name it, and nothing the frame recorded writes it, UAVs they name included (a \
                             shader indexing the descriptor heap itself could)"
                                .into(),
                        );
                        hashes.clone_from(subs);
                    }
                    Ok((key, subs)) => {
                        // Exact: d12_first_use says which submission it was copied before.
                        resource.content = Some(key.clone());
                        resource.content_source = Some("first-use");
                        resource.content_note.clone_from(&first.note);
                        hashes.clone_from(subs);
                    }
                    Err(why) => {
                        resource.content_note = Some(why.clone());
                        *missing.entry(why.clone()).or_default() += 1;
                    }
                }
            } else if descriptions
                .get(&resource.id)
                .is_some_and(|d| acceleration_structures(d))
            {
                // Its bytes are the driver's: replay builds each structure in it again. One the frame began with
                // whose inputs are not known is built empty.
                resource.content_source = Some("acceleration-structure-builds");
                resource.content_note = Some(
                    "acceleration structures: those the frame began with built again from \
                     d12_acceleration_structures, the rest as the frame builds them"
                        .into(),
                );
                if resource.used {
                    let structures = self
                        .frame
                        .acceleration_structures
                        .iter()
                        .filter(|a| a.resource == resource.id);
                    for why in structures.filter_map(|a| a.inputs.as_ref().err()) {
                        *unbuilt.entry(why.clone()).or_default() += 1;
                    }
                }
            } else if descriptions
                .get(&resource.id)
                .is_some_and(|d| d["heap_properties"]["type"] == 3)
            {
                resource.content_note = Some("readback heap: only the GPU writes it".into());
            } else if resource.used {
                let why = "used, but no copy was made before its first use".to_string();
                resource.content_note = Some(why.clone());
                *missing.entry(why).or_default() += 1;
            }
            if resource.used || resource.content.is_some() {
                let subs = descriptions
                    .get(&resource.id)
                    .and_then(|d| layout(d))
                    .unwrap_or_default();
                for (i, sub) in crate::layout::subresources(&subs).into_iter().enumerate() {
                    subresources.push(db::Subresource {
                        resource: resource.id,
                        sub,
                        hash: hashes.get(i).filter(|h| !h.is_empty()).cloned(),
                    });
                }
            }
        }
        capture.subresources.extend(subresources);
        // Program names for SQL, from the bytecode the store holds.
        for s in &mut capture.shaders {
            s.name = s
                .blob
                .as_ref()
                .and_then(|k| blobs.get(k).ok())
                .and_then(|code| crate::rdef::debug_name(&code));
        }
        let mut uncopied = BTreeMap::<String, u64>::new();
        let mut continued = 0;
        for (id, target) in &self.projection.targets {
            if self.continued.contains(id) {
                continued += 1;
                continue;
            }
            let why = match self.checkpoints.get(id) {
                Some(Ok(key)) => {
                    capture.targets.push(db::Target {
                        blob: Some(key.clone()),
                        ..target.clone()
                    });
                    continue;
                }
                Some(Err(why)) => why.clone(),
                None => "copy not read back".into(),
            };
            *uncopied.entry(why.clone()).or_default() += 1;
            capture.missing_targets.push(db::MissingTarget {
                draw: target.draw,
                target: target.target.clone(),
                resource: target.resource,
                why,
            });
        }
        capture
            .missing_targets
            .extend(self.projection.missing.iter().cloned());
        capture.missing_targets.sort_by_key(|m| m.draw);
        let mut skipped = 0;
        for (why, count) in self.projection.uncopied.iter().chain(&uncopied) {
            capture.gap("d3d12-targets", why, *count);
            skipped += count;
        }
        // What draws, dispatches and clears wrote, each flagged where one of its programs is order-dependent.
        let programs: BTreeMap<u64, Vec<String>> = capture
            .draws
            .iter()
            .map(|d| (d.event, d.shaders.iter().flatten().cloned().collect()))
            .collect();
        let mut order_dependent = BTreeMap::<String, bool>::new();
        let mut ordered = |event: u64| {
            programs.get(&event).is_some_and(|keys| {
                keys.iter().any(|k| {
                    *order_dependent.entry(k.clone()).or_insert_with(|| {
                        blobs
                            .get(k)
                            .ok()
                            .and_then(|code| crate::dxbc::order_dependent(&code).ok().flatten())
                            .is_some()
                    })
                })
            })
        };
        let mut unrecorded = BTreeMap::<String, u64>::new();
        // What draws, dispatches and clears wrote through views the list bound, copied right after each; then what
        // ExecuteIndirects wrote through root UAVs their records set, copied after the frame.
        let copies =
            self.projection
                .outputs
                .iter()
                .map(|&(id, event, resource, sub, range)| {
                    let content = self.checkpoints.get(&id).cloned();
                    (event, resource, sub, range, content)
                })
                .chain(self.named.outputs.iter().map(|(&(event, resource), content)| {
                    (event, resource, None, None, Some(content.clone()))
                }));
        for (event, resource, sub, range, content) in copies {
            match content {
                Some(Ok(key)) => capture.results.push(db::Output {
                    event,
                    resource,
                    sub,
                    range,
                    content: key,
                    ordered: ordered(event),
                }),
                Some(Err(why)) => *unrecorded.entry(why).or_default() += 1,
                None => *unrecorded.entry("copy not read back".into()).or_default() += 1,
            }
        }
        // A copy asked for after the frame and never filed was not read back.
        let filed = |r: &u64, event: u64| self.named.outputs.contains_key(&(event, *r));
        for (resource, purpose) in &self.named.planned {
            if let AfterFrame::Output(event) = purpose
                && !filed(resource, *event)
            {
                *unrecorded.entry("copy not read back".into()).or_default() += 1;
            }
        }
        for (why, count) in self
            .projection
            .unrecorded
            .iter()
            .chain(&self.named.uncopied)
            .chain(&unrecorded)
        {
            capture.gap("d3d12-results", why, *count);
        }
        for (why, count) in &self.projection.unread_arguments {
            capture.gap("d3d12-indirect-arguments", why, *count);
        }
        let target_bytes: u64 = capture
            .targets
            .iter()
            .filter_map(|t| crate::layout::texture(t.format, t.width, t.height, 1, 1, 1))
            .map(|l| crate::layout::total(&l))
            .sum();
        for (why, count) in unread {
            capture.gap("d3d12-constant-content", why, count);
        }
        for (why, count) in missing {
            capture.gap("contents", why, count);
        }
        for (why, count) in unbuilt {
            capture.gap(
                "d3d12-acceleration-structures",
                format!("a structure the frame began with, in a buffer it uses, is built empty: {why}"),
                count,
            );
        }
        capture.meta("counts", serde_json::json!({
            "events":capture.events.len(), "draws":capture.draws.len(),
            "resources_live":capture.resources.len(),
            "resources_used":capture.resources.iter().filter(|r|r.used).count(),
            "resources_written":capture.resources.iter().filter(|r|r.written).count(),
            "contents":capture.resources.iter().filter(|r|r.content.is_some()).count(),
            "content_bytes":capture.resources.iter().filter(|r|r.content.is_some()).map(|r|r.bytes).sum::<u64>(),
            "submissions":self.frame.submissions.len(),
            "executions":self.projection.executions.len(),
            "targets":capture.targets.len(), "target_bytes":target_bytes, "targets_skipped":skipped
        }));
        let mut pass_ends = serde_json::to_value(self.projection.pass_ends).unwrap_or_default();
        // Lists that closed with a pass open which the next list submitted with them continued.
        pass_ends["continued"] = continued.into();
        capture.meta("pass_ends", pass_ends);
    }
    pub fn write(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        self.frame.write(tx, &self.projection, &self.first_use)
    }
}

struct Execution {
    id: u64,
    submission: u64,
    list: Arc<List>,
    ordinal: usize,
    parent: Option<u64>,
}

/// The bytes a buffer held at an event of the frame, for constant buffers: upload bytes as the GPU reached the event's
/// submission, or the buffer as its first use found it with every earlier copy into it laid over in event order.
struct Composer<'a> {
    /// Per submission, runs of upload bytes keyed by where they begin.
    runs: &'a BTreeMap<(u64, u64, u64), &'a Value>,
    blobs: &'a crate::blobs::Blobs,
    first_use: &'a BTreeMap<u64, FirstUse>,
    /// Each event's submission.
    submissions: HashMap<u64, u64>,
    /// Each resource's writes, in event order.
    written: BTreeMap<u64, Vec<&'a db::Event>>,
}

impl Composer<'_> {
    /// Copies through more buffers than this are not followed.
    const DEPTH: u32 = 8;

    /// The upload run of `event`'s submission that covers `resource` at `offset`, as (where it begins, its value).
    /// Merged runs never overlap, so the one beginning at or before the offset is the only one that can.
    fn run(&self, resource: u64, offset: u64, event: u64) -> Option<(u64, &Value)> {
        let submission = *self.submissions.get(&event)?;
        self.runs
            .range((submission, resource, 0)..=(submission, resource, offset))
            .next_back()
            .filter(|((_, _, begin), v)| offset < begin + v["size"].as_u64().unwrap_or(0))
            .map(|(&(_, _, begin), v)| (begin, *v))
    }

    /// How far the covering upload run reaches past `offset`: a root CBV's size when nothing else gives it.
    fn run_len(&self, resource: u64, offset: u64, event: u64) -> Option<u64> {
        let (begin, run) = self.run(resource, offset, event)?;
        Some(begin + run["size"].as_u64()? - offset)
    }

    /// `resource`'s writes before `event`.
    fn writes(&self, resource: u64, event: u64) -> &[&db::Event] {
        self.written
            .get(&resource)
            .map_or(&[], |w| &w[..w.partition_point(|e| e.seq < event)])
    }

    /// `resource`'s bytes `[offset, offset + len)` just before `event`, or why they are not known.
    fn at(&self, resource: u64, offset: u64, len: u64, event: u64, depth: u32) -> Result<Vec<u8>, String> {
        if depth > Self::DEPTH {
            return Err(format!("copied through more than {} buffers", Self::DEPTH));
        }
        if let Some((begin, run)) = self.run(resource, offset, event) {
            let key = run["blob"].as_str().ok_or("an upload run without bytes")?;
            // Only these bytes: a run is megabytes, and Elden Ring has thousands of CB bindings per frame.
            let rest = begin + run["size"].as_u64().unwrap_or(0) - offset;
            if len <= rest {
                return self
                    .blobs
                    .read(key, offset - begin, Some(len))
                    .map_err(|e| format!("upload bytes: {e}"));
            }
        }
        let size = usize::try_from(len).map_err(|_| "too large".to_string())?;
        let mut bytes = vec![0u8; size];
        let mut known = vec![false; size];
        let mut base = None;
        match self.first_use.get(&resource).map(|f| &f.content) {
            Some(Ok((key, _))) => {
                let read = self
                    .blobs
                    .read(key, offset, Some(len))
                    .map_err(|e| format!("first-use bytes: {e}"))?;
                let n = read.len().min(size);
                bytes[..n].copy_from_slice(&read[..n]);
                known[..n].fill(true);
            }
            Some(Err(why)) => base = Some(why.clone()),
            None => base = Some("no copy of it was made before its first use".to_string()),
        }
        let address = |v: &Value| v["resource"].as_u64().or_else(|| v.as_u64());
        for write in self.writes(resource, event) {
            let a: Value = serde_json::from_str(&write.args).unwrap_or_default();
            let (to, from, from_offset, n) = match write.call {
                "CopyBufferRegion" => (a[1].as_u64(), address(&a[2]), a[3].as_u64(), a[4].as_u64()),
                "CopyResource" => (Some(0), address(&a[1]), Some(0), Some(u64::MAX)),
                call => {
                    return Err(format!(
                        "written in the frame by {call}, which locum does not follow"
                    ));
                }
            };
            let (Some(to), Some(from), Some(from_offset), Some(n)) = (to, from, from_offset, n) else {
                return Err(format!(
                    "a {} into it whose arguments locum cannot read",
                    write.call
                ));
            };
            let lo = offset.max(to);
            let hi = (offset + len).min(to.saturating_add(n));
            if lo >= hi {
                continue;
            }
            let source = self
                .at(from, from_offset + (lo - to), hi - lo, write.seq, depth + 1)
                .map_err(|why| {
                    format!("copied in the frame from a buffer whose bytes are not known ({why})")
                })?;
            let (lo, hi) = ((lo - offset) as usize, (hi - offset) as usize);
            bytes[lo..hi].copy_from_slice(&source);
            known[lo..hi].fill(true);
        }
        if known.iter().all(|k| *k) {
            Ok(bytes)
        } else {
            Err(base.unwrap_or_else(|| "its first-use bytes end before the range".to_string()))
        }
    }
}

/// The frame's command history projected into the common tables, one submission at a time.
/// What an ExecuteIndirect with command signature `signature` is: its last argument draws or dispatches.
/// Whether a root signature lets its programs index the CBV, SRV and UAV heap directly
/// (`D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED`, which `ResourceDescriptorHeap` needs).
fn directly_indexed(root: &Value) -> bool {
    root["decoded"]["flags"].as_u64().is_some_and(|f| f & 0x400 != 0)
}

fn indirect(signature: Option<&Value>) -> &'static str {
    let last = signature.and_then(|s| s["arguments"].as_array()?.last()?["type"].as_i64());
    match last {
        // DRAW, DRAW_INDEXED, DISPATCH_MESH.
        Some(0 | 1 | 10) => "draw",
        // DISPATCH, DISPATCH_RAYS.
        Some(2 | 9) => "dispatch",
        _ => "indirect",
    }
}

/// A copy of what an event wrote through UAVs: the copy, the event, the resource, its subresource (a texture's) and
/// the bytes (offset, length) of it (a buffer's, None for all of it).
type Copied = (u64, u64, u64, Option<u32>, Option<(u64, u64)>);

pub struct Projection {
    next_execution: u64,
    executions: Vec<Execution>,
    locations: Vec<(u64, u64, usize, u64)>,
    objects: BTreeMap<u64, Value>,
    draw_states: Vec<(u64, u64, Pipeline)>,
    /// Each descriptor slot's definition as of the submission being projected, by (heap, slot).
    descriptors: BTreeMap<(u64, u32), Descriptor>,
    /// List generations executed so far: only a generation's first execution has its checkpoints' bytes.
    executed: std::collections::HashSet<(u64, u64)>,
    /// The targets the frame's checkpoints copied, by checkpoint, each on its pass's last draw.
    targets: Vec<(u64, db::Target)>,
    /// Pass ends no copy was made for, by reason, and those of them on a draw.
    uncopied: BTreeMap<String, u64>,
    missing: Vec<db::MissingTarget>,
    /// The last draw of the list execution being projected that each (target, view) was bound to.
    drawn: BTreeMap<(String, u64), u64>,
    /// How the frame's pass ends went, for the capture's meta: where a frame's targets go missing.
    pass_ends: PassEnds,
    /// What the frame's outputs copied, by copy.
    outputs: Vec<Copied>,
    /// Outputs no copy was made for, by reason; draws and dispatches in lists that copied none (not instrumented).
    unrecorded: BTreeMap<String, u64>,
    /// The ExecuteIndirect events whose argument records were copied.
    indirect: Vec<Indirect>,
    /// ExecuteIndirects with addresses in their records that no copy was made for, by reason.
    unread_arguments: BTreeMap<String, u64>,
    /// The CBV, SRV and UAV heaps a draw or dispatch of the submission being projected ran with under a root signature
    /// that indexes them directly (`ResourceDescriptorHeap`): its programs may read any slot.
    indexed: std::collections::BTreeSet<u64>,
    /// The resources those heaps' slots named at a submission of the frame, as the submission found them.
    heap_named: std::collections::BTreeSet<u64>,
    /// Bindless descriptor heap slots referenced by draws/dispatches on directly-indexed root signatures.
    bindless_usage: Vec<BindlessUsage>,
    /// Structured annotations extracted from in-stream SetMarker/BeginEvent payloads.
    annotations: Vec<Annotation>,
}

/// Counts of the frame's direct-list executions and their checkpoints.
#[derive(Clone, Copy, Default, Serialize)]
struct PassEnds {
    /// Top-level direct-list executions with draws, instrumented or not (recorded before the capture was armed).
    lists_instrumented: u64,
    lists_uninstrumented: u64,
    /// Checkpoints the executed lists carried; those after the last draw that had their target bound (a pass of
    /// clears, copies or dispatches only), which hold no draw's output.
    checkpoints: u64,
    after_no_draw: u64,
}

#[derive(Clone, Default, Serialize)]
struct Pipeline {
    pso: u64,
    /// The raytracing state object SetPipelineState1 set (each of it and SetPipelineState unsets the other's).
    state_object: u64,
    graphics_root: u64,
    compute_root: u64,
    graphics_args: BTreeMap<u32, Value>,
    compute_args: BTreeMap<u32, Value>,
    descriptor_heaps: Value,
    graphics: graphics::State,
}

impl Pipeline {
    fn apply(&mut self, command: &Command) {
        self.graphics.apply(command);
        let a = &command.args;
        match command.call {
            "ClearState" => {
                *self = Self {
                    pso: a[0].as_u64().unwrap_or(0),
                    ..Self::default()
                };
            }
            "SetPipelineState" => {
                self.pso = a[0].as_u64().unwrap_or(0);
                self.state_object = 0;
            }
            "SetPipelineState1" => {
                self.state_object = a[0].as_u64().unwrap_or(0);
                self.pso = 0;
            }
            "SetGraphicsRootSignature" => {
                let id = a[0].as_u64().unwrap_or(0);
                if self.graphics_root != id {
                    self.graphics_args.clear();
                }
                self.graphics_root = id;
            }
            "SetComputeRootSignature" => {
                let id = a[0].as_u64().unwrap_or(0);
                if self.compute_root != id {
                    self.compute_args.clear();
                }
                self.compute_root = id;
            }
            "SetDescriptorHeaps" => {
                if self.descriptor_heaps == a[0] {
                    return;
                }
                self.descriptor_heaps = a[0].clone();
                self.graphics_args.retain(|_, v| v["kind"] != "DescriptorTable");
                self.compute_args.retain(|_, v| v["kind"] != "DescriptorTable");
            }
            call => {
                let (args, kind) = if let Some(k) = call.strip_prefix("SetGraphicsRoot") {
                    (&mut self.graphics_args, k)
                } else if let Some(k) = call.strip_prefix("SetComputeRoot") {
                    (&mut self.compute_args, k)
                } else {
                    return;
                };
                let index = a[0].as_u64().unwrap_or(0) as u32;
                if matches!(kind, "32BitConstant" | "32BitConstants") {
                    let (offset, values) = if kind == "32BitConstant" {
                        (a[2].as_u64().unwrap_or(0), vec![a[1].clone()])
                    } else {
                        (
                            a[2].as_u64().unwrap_or(0),
                            a[1].as_array().cloned().unwrap_or_default(),
                        )
                    };
                    let slot = args
                        .entry(index)
                        .or_insert_with(|| serde_json::json!({"kind":"constants","words":[]}));
                    let Some(words) = slot["words"].as_array_mut() else {
                        return;
                    };
                    // D3D12 root signatures have at most 64 DWORDs. Invalid input cannot allocate arbitrarily.
                    if offset + values.len() as u64 > 64 {
                        return;
                    }
                    words.resize(words.len().max(offset as usize + values.len()), Value::Null);
                    words[offset as usize..offset as usize + values.len()].clone_from_slice(&values);
                } else {
                    args.insert(index, serde_json::json!({"kind":kind,"value":a[1]}));
                }
            }
        }
    }
}

impl Frame {
    /// The frame's resources and shaders, from its objects as they were when it ended.
    fn project_objects(&self, capture: &mut db::Capture) {
        for diagnostic in &self.diagnostics {
            capture.gap("d3d12-submission", diagnostic, 1);
        }
        for object in &self.objects {
            let d = &object.description;
            if let Some(dimension) = d["dimension"].as_u64() {
                let n = |key: &str| d[key].as_u64().unwrap_or(0) as u32;
                let kind = match dimension {
                    1 => "buffer",
                    2 => "texture1d",
                    3 => "texture2d",
                    4 => "texture3d",
                    _ => "unknown",
                };
                let (depth, array) = extent(d);
                let bytes = layout(d).as_deref().map_or(0, crate::layout::total);
                capture.resources.push(db::Resource {
                    id: object.id,
                    kind,
                    // (a buffer's bytes, which may pass 4 GiB)
                    width: d["width"].as_u64().unwrap_or(0),
                    height: n("height"),
                    depth,
                    array,
                    mips: n("mips"),
                    format: n("format"),
                    samples: n("sample_count"),
                    bytes,
                    desc: d.to_string().into_bytes(),
                    ..db::Resource::default()
                });
            }
            if let Some(shaders) = d["shaders"].as_array() {
                for (i, s) in shaders.iter().take(6).enumerate() {
                    if let Some(hash) = s["blob"].as_str() {
                        capture.shaders.push(db::Shader {
                            hash: hash.into(),
                            stage: db::stage_names()[i],
                            blob: Some(hash.into()),
                            size: s["size"].as_u64().unwrap_or(0),
                            name: None,
                        });
                    }
                }
            }
        }
    }

    /// Projects every submission at once and prepares the capture (a journal projects each submission as it is
    /// made, with [`Projection::submit`], then calls [`Frame::finish`]).
    pub fn prepare(self, capture: &mut db::Capture) -> Prepared {
        let mut projection = Projection::new(&self.objects);
        for submission in &self.submissions {
            projection.submit(capture, submission);
            projection.heap_indexed();
        }
        self.finish(capture, projection)
    }

    /// Adds the frame's resources and shaders to a capture whose submissions `projection` has projected, and marks
    /// the resources its events read and wrote.
    pub fn finish(self, capture: &mut db::Capture, mut projection: Projection) -> Prepared {
        self.project_objects(capture);
        let reads: std::collections::BTreeSet<_> = capture
            .events
            .iter()
            .flat_map(|e| e.reads.iter().copied())
            .collect();
        let writes: std::collections::BTreeSet<_> = capture
            .events
            .iter()
            .flat_map(|e| e.writes.iter().copied())
            .collect();
        for resource in &mut capture.resources {
            resource.written = writes.contains(&resource.id);
            resource.used = resource.written
                || reads.contains(&resource.id)
                || projection.heap_named.contains(&resource.id);
        }
        projection.objects.clear();
        Prepared {
            frame: self,
            projection,
            first_use: BTreeMap::new(),
            checkpoints: BTreeMap::new(),
            continued: Default::default(),
            named: Named::default(),
        }
    }

    fn write(
        &self,
        tx: &Transaction<'_>,
        p: &Projection,
        first_use: &BTreeMap<u64, FirstUse>,
    ) -> rusqlite::Result<()> {
        tx.execute_batch(SCHEMA)?;
        for object in &self.objects {
            tx.execute(
                "INSERT INTO d12_objects VALUES(?1,?2,?3,1,?4)",
                params![
                    integer(object.id)?,
                    object.kind,
                    integer(object.device)?,
                    object.description.to_string()
                ],
            )?;
            let d = &object.description;
            if d["shaders"].is_array() {
                tx.execute(
                    "INSERT INTO d12_psos VALUES(?1,?2,?3)",
                    params![
                        integer(object.id)?,
                        d["kind"].as_str().unwrap_or(""),
                        d.to_string()
                    ],
                )?;
            }
            if d["kind"] == "root_signature" {
                tx.execute(
                    "INSERT INTO d12_roots VALUES(?1,?2,?3)",
                    params![
                        integer(object.id)?,
                        d["original"]["blob"].as_str(),
                        d["decoded"].to_string()
                    ],
                )?;
            }
        }
        for (draw, _, state) in &p.draw_states {
            tx.execute(
                "INSERT INTO d12_draw_state VALUES(?1,?2,?3,?4,?5)",
                params![
                    integer(*draw)?,
                    integer(state.pso)?,
                    integer(state.graphics_root)?,
                    integer(state.compute_root)?,
                    serde_json::to_string(state).unwrap()
                ],
            )?;
        }
        let mut lists = BTreeMap::new();
        for execution in &p.executions {
            let list = &execution.list;
            lists.insert((list.id, list.generation), list);
        }
        for list in lists.values() {
            for command in &list.commands {
                write_consumed_descriptors(tx, &command.args)?;
            }
            tx.execute(
                "INSERT INTO d12_lists VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    integer(list.id)?,
                    integer(list.generation)?,
                    list.kind,
                    integer(list.allocator)?,
                    integer(list.initial_pso)?,
                    list.closed,
                    serde_json::to_string(&list.commands).expect("owned JSON commands")
                ],
            )?;
        }
        for s in &self.submissions {
            tx.execute(
                "INSERT INTO d12_submissions VALUES(?1,?2,?3,?4)",
                params![
                    integer(s.id)?,
                    integer(s.queue)?,
                    integer(s.queue_seq)?,
                    integer(s.cpu_seq)?
                ],
            )?;
            for d in &s.descriptors {
                tx.execute(
                    "INSERT OR IGNORE INTO d12_descriptor_versions VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        integer(d.id)?,
                        integer(d.heap)?,
                        d.slot,
                        d.kind,
                        integer(d.resource)?,
                        integer(d.counter)?,
                        d.description.to_string()
                    ],
                )?;
                tx.execute(
                    "INSERT INTO d12_submission_descriptors VALUES(?1,?2)",
                    params![integer(s.id)?, integer(d.id)?],
                )?;
            }
        }
        for Execution {
            id,
            submission,
            list,
            ordinal,
            parent,
        } in &p.executions
        {
            tx.execute(
                "INSERT INTO d12_executions VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    integer(*id)?,
                    integer(*submission)?,
                    integer(list.id)?,
                    integer(list.generation)?,
                    integer(*ordinal as u64)?,
                    parent.map(integer).transpose()?
                ],
            )?;
        }
        for &(event, execution, index, queue) in &p.locations {
            tx.execute(
                "INSERT INTO d12_event_locations VALUES(?1,?2,?3,?4)",
                params![
                    integer(event)?,
                    integer(execution)?,
                    integer(index as u64)?,
                    integer(queue)?
                ],
            )?;
        }
        for s in &self.sync {
            tx.execute("INSERT INTO d12_sync(queue,queue_seq,cpu_seq,kind,fence,value_u64) VALUES(?1,?2,?3,?4,?5,?6)",
                params![integer(s.queue)?,integer(s.queue_seq)?,integer(s.cpu_seq)?,s.kind,integer(s.fence)?,s.value.to_le_bytes().as_slice()])?;
        }
        for t in &self.tile_pages {
            let (layout, pages) = match &t.pages {
                Ok((layout, pages)) => (Some(layout.to_string()), Some(pages.to_string())),
                Err(_) => (None, None),
            };
            tx.execute(
                "INSERT INTO d12_tile_pages VALUES(?1,?2,?3,?4,?5)",
                params![
                    integer(t.resource)?,
                    integer(t.cpu_seq)?,
                    layout,
                    pages,
                    t.pages.as_ref().err()
                ],
            )?;
        }
        for a in &self.acceleration_structures {
            tx.execute(
                "INSERT INTO d12_acceleration_structures VALUES(?1,?2,?3,?4,?5)",
                params![
                    integer(a.resource)?,
                    integer(a.offset)?,
                    a.build.to_string(),
                    a.inputs.as_ref().ok().map(Value::to_string),
                    a.inputs.as_ref().err()
                ],
            )?;
        }
        for m in &self.tile_mappings {
            tx.execute(
                "INSERT INTO d12_tile_mappings(queue,queue_seq,cpu_seq,call,resource,source,args_json,pages_json,diagnostic) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    integer(m.queue)?,
                    integer(m.queue_seq)?,
                    integer(m.cpu_seq)?,
                    m.call,
                    integer(m.resource)?,
                    m.source.map(integer).transpose()?,
                    m.args.to_string(),
                    m.after.as_ref().ok().map(Value::to_string),
                    m.after.as_ref().err()
                ],
            )?;
        }
        for (resource, first) in first_use {
            tx.execute(
                "INSERT INTO d12_first_use VALUES(?1,?2,?3,?4)",
                params![
                    integer(*resource)?,
                    integer(first.submission)?,
                    first.states.to_string(),
                    first.content.as_ref().err()
                ],
            )?;
        }
        for s in &self.submissions {
            for (resource, content) in &s.uploads {
                tx.execute(
                    "INSERT INTO d12_upload_versions VALUES(?1,?2,?3,?4,?5)",
                    params![
                        integer(s.id)?,
                        integer(*resource)?,
                        content["whole"] == true,
                        integer(content["offset"].as_u64().unwrap_or(0))?,
                        content.to_string()
                    ],
                )?;
            }
        }
        for p in &self.selected_passes {
            tx.execute(
                "INSERT INTO d12_selected_passes VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![
                    integer(p.submission)?,
                    integer(p.queue)?,
                    integer(p.cpu_seq)?,
                    p.reason,
                    p.bindless_heaps.to_string(),
                    integer(p.bindless_max_version)?,
                    p.resources.to_string()
                ],
            )?;
        }
        for a in &self.indirect_arguments {
            tx.execute(
                "INSERT OR REPLACE INTO d12_indirect_arguments VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
                params![
                    integer(a.event)?,
                    integer(a.submission)?,
                    integer(a.queue)?,
                    integer(a.signature)?,
                    integer(a.max_count)?,
                    integer(a.arg_buffer)?,
                    integer(a.arg_offset)?,
                    integer(a.arg_bytes)?,
                    integer(a.count_buffer)?,
                    integer(a.count_offset)?,
                    a.count_value.map(i64::from),
                    i64::from(a.stride),
                    a.blob,
                    a.diagnostic
                ],
            )?;
        }
        for f in &self.fence_states {
            tx.execute(
                "INSERT INTO d12_fence_state(phase,submission,cpu_seq,fence,completed_u64,producer_queue,\
                 producer_submission,producer_value_u64,producer_cpu_seq) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    f.phase,
                    f.submission.map(integer).transpose()?,
                    integer(f.cpu_seq)?,
                    integer(f.fence)?,
                    f.completed.to_le_bytes().as_slice(),
                    f.producer_queue.map(integer).transpose()?,
                    f.producer_submission.map(integer).transpose()?,
                    f.producer_value.map(|v| v.to_le_bytes().to_vec()),
                    f.producer_cpu_seq.map(integer).transpose()?
                ],
            )?;
        }
        for s in &self.submission_syncs {
            tx.execute(
                "INSERT INTO d12_submission_sync(submission,queue,cpu_seq,fence,wait_value_u64,\
                 completed_value_u64,producer_queue,producer_submission,producer_value_u64,satisfied) \
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
                params![
                    integer(s.submission)?,
                    integer(s.queue)?,
                    integer(s.cpu_seq)?,
                    integer(s.fence)?,
                    s.wait_value.to_le_bytes().as_slice(),
                    s.completed_value.map(|v| v.to_le_bytes().to_vec()),
                    s.producer_queue.map(integer).transpose()?,
                    s.producer_submission.map(integer).transpose()?,
                    s.producer_value.map(|v| v.to_le_bytes().to_vec()),
                    s.satisfied
                ],
            )?;
        }
        for t in &self.tile_snapshots {
            tx.execute(
                "INSERT OR REPLACE INTO d12_tile_snapshots VALUES(?1,?2,?3,?4,?5,?6)",
                params![
                    integer(t.submission)?,
                    integer(t.resource)?,
                    integer(t.cpu_seq)?,
                    t.pages_json,
                    t.blob,
                    t.diagnostic
                ],
            )?;
        }
        for u in self.bindless_usage.iter().chain(&p.bindless_usage) {
            tx.execute(
                "INSERT OR IGNORE INTO d12_bindless_usage VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    integer(u.draw)?,
                    integer(u.event)?,
                    integer(u.heap)?,
                    i64::from(u.slot),
                    u.kind,
                    integer(u.resource)?,
                    u.access,
                    u.source
                ],
            )?;
        }
        for a in self.annotations.iter().chain(&p.annotations) {
            tx.execute(
                "INSERT INTO d12_annotations(seq,list,submission,category,payload_json) VALUES(?1,?2,?3,?4,?5)",
                params![
                    a.seq.map(integer).transpose()?,
                    a.list.map(integer).transpose()?,
                    a.submission.map(integer).transpose()?,
                    a.category,
                    a.payload.to_string()
                ],
            )?;
        }
        Ok(())
    }
}

impl Projection {
    pub fn new(objects: &[Object]) -> Self {
        Self {
            next_execution: 1,
            executions: Vec::new(),
            locations: Vec::new(),
            objects: objects.iter().map(|o| (o.id, o.description.clone())).collect(),
            draw_states: Vec::new(),
            descriptors: BTreeMap::new(),
            executed: std::collections::HashSet::new(),
            targets: Vec::new(),
            uncopied: BTreeMap::new(),
            missing: Vec::new(),
            drawn: BTreeMap::new(),
            outputs: Vec::new(),
            unrecorded: BTreeMap::new(),
            indirect: Vec::new(),
            unread_arguments: BTreeMap::new(),
            pass_ends: PassEnds::default(),
            indexed: std::collections::BTreeSet::new(),
            heap_named: std::collections::BTreeSet::new(),
            bindless_usage: Vec::new(),
            annotations: Vec::new(),
        }
    }

    /// The argument records a list copied, each on the event of its ExecuteIndirect (`seqs`, the events of the list's
    /// commands). Every execution of a generation copies them again: read after the frame, they hold its last.
    fn arguments(&mut self, list: &List, seqs: &[u64], state_objects: &[u64], submission: &Submission) {
        for a in &list.arguments {
            match (&a.gap, seqs.get(a.command)) {
                (None, Some(&event)) => self.indirect.push(Indirect {
                    event,
                    submission: submission.id,
                    queue: submission.queue,
                    state_object: state_objects.get(a.command).copied().unwrap_or(0),
                    arguments: a.clone(),
                }),
                (Some(why), _) => *self.unread_arguments.entry(why.clone()).or_default() += 1,
                (None, None) => {
                    *self
                        .unread_arguments
                        .entry("its command was not projected".into())
                        .or_default() += 1
                }
            }
        }
        // Those no copy was made at, whose records hold addresses or cannot be read, DispatchRays (whose tables
        // always hold identifiers) and builds of instances by pointer: the list was not instrumented.
        let uncopied = list.commands.iter().enumerate().filter(|(index, c)| {
            !list.arguments.iter().any(|a| a.command == *index)
                && (c.call == "DispatchRays"
                    || c.call == "BuildRaytracingAccelerationStructure" && instance_pointers(&c.args[0]) > 0
                    || c.call == "ExecuteIndirect"
                        && self
                            .objects
                            .get(&c.args[0].as_u64().unwrap_or(0))
                            .is_none_or(|s| indirect::layout(s).map_or(true, |l| !l.addresses.is_empty())))
        });
        let uncopied = uncopied.count() as u64;
        if uncopied > 0 {
            *self
                .unread_arguments
                .entry(
                    "its list was not instrumented (recorded before the capture was armed, or a compute or copy \
                     list)"
                        .into(),
                )
                .or_default() += uncopied;
        }
    }

    /// The outputs a list copied before its command `at`, each on the event of the command that wrote it (`seqs`, the
    /// events of the list's commands so far). A generation executed again has the same copies, which hold its first
    /// execution's outputs.
    fn outputs(&mut self, list: &List, at: usize, seqs: &[u64], first: bool) {
        for o in list.outputs.iter().filter(|o| o.at == at) {
            let why = if first {
                o.gap.clone()
            } else {
                Some(
                    "the list was executed again in the frame: its copies hold the first execution's outputs"
                        .into(),
                )
            };
            match (why, seqs.get(o.command)) {
                (None, Some(&event)) => self.outputs.push((o.id, event, o.resource, o.sub, o.range)),
                (Some(why), _) => *self.unrecorded.entry(why).or_default() += 1,
                (None, None) => {
                    *self
                        .unrecorded
                        .entry("its command was not projected".into())
                        .or_default() += 1
                }
            }
        }
    }

    /// The checkpoints a list made before its command `at`, each as its target after the last draw of this
    /// execution that had it bound. A generation executed again has the same copies, which hold its first
    /// execution's bytes.
    fn checkpoints(&mut self, capture: &db::Capture, list: &List, at: usize, first: bool) {
        for c in list.checkpoints.iter().filter(|c| c.at == at) {
            self.pass_ends.checkpoints += 1;
            // A copy after no draw that had its target bound (an indirect dispatch, a bundle without draws) holds
            // no draw's output.
            let Some(draw) = self.drawn.get(&(c.target.clone(), c.view)).copied() else {
                self.pass_ends.after_no_draw += 1;
                continue;
            };
            let why = if first {
                c.gap.clone()
            } else {
                Some(
                    "the list was executed again in the frame: its copies hold the first execution's targets"
                        .into(),
                )
            };
            if let Some(why) = why {
                *self.uncopied.entry(why.clone()).or_default() += 1;
                self.missing.push(db::MissingTarget {
                    draw,
                    target: c.target.clone(),
                    resource: c.resource,
                    why,
                });
                continue;
            }
            self.targets.push((
                c.id,
                db::Target {
                    draw,
                    target: c.target.clone(),
                    view: c.view,
                    resource: c.resource,
                    width: c.width,
                    height: c.height,
                    format: c.format,
                    sub: c.sub,
                    blob: None,
                    event: capture.events.len() as u64,
                    z: c.z,
                },
            ));
        }
    }

    /// An object (re)described while the frame is projected.
    pub fn describe(&mut self, id: u64, description: &Value) {
        self.objects.insert(id, description.clone());
    }

    /// Object `id` as described so far.
    pub fn description(&self, id: u64) -> Option<&Value> {
        self.objects.get(&id)
    }

    /// The DispatchRays whose shader tables were copied, and the ExecuteIndirects whose argument records were, in the
    /// submissions projected so far.
    pub fn indirect(&self) -> &[Indirect] {
        &self.indirect
    }

    /// What the shader tables DispatchRays `rays` runs with name (see [`named_by_tables`]), as its submission is
    /// made: against the objects as described so far and the descriptors as that submission finds them.
    pub fn named_by_tables(
        &self,
        rays: &Indirect,
        tables: &[tables::Table],
        bytes: &[u8],
        declared: &dyn Fn(&str) -> Result<Arc<programs::Declared>, String>,
        decode: &mut dyn FnMut(&programs::Local) -> Result<Value, String>,
        buffers_at: &dyn Fn(u64) -> Vec<u64>,
    ) -> TableNames {
        let objects: BTreeMap<u64, &Value> = self.objects.iter().map(|(&id, d)| (id, d)).collect();
        let read = Tables {
            objects: &objects,
            declared,
            decode,
            buffers_at,
        };
        named_by_tables(read, rays.state_object, tables, bytes, || {
            self.descriptors.iter().map(|(&slot, d)| (slot, d)).collect()
        })
    }

    /// Projects one submission, after the frame's earlier ones. Returns the resources its events read or write, each
    /// once, in the order they are first touched; [`Projection::heap_indexed`] then names those a program indexing a
    /// descriptor heap directly may read besides.
    pub fn submit(&mut self, capture: &mut db::Capture, submission: &Submission) -> Vec<u64> {
        self.indexed.clear();
        for d in &submission.descriptors {
            if d.kind == "empty" {
                self.descriptors.remove(&(d.heap, d.slot));
            } else {
                self.descriptors.insert((d.heap, d.slot), d.clone());
            }
        }
        let first = capture.events.len();
        capture.events.push(db::Event {
            seq: capture.events.len() as u64 + 1,
            kind: "submit",
            call: "ExecuteCommandLists",
            args: serde_json::json!({"submission":submission.id,"queue":submission.queue,
                "queue_seq":submission.queue_seq,"cpu_seq":submission.cpu_seq,
                "lists":submission.lists.iter().map(|l| [l.id,l.generation]).collect::<Vec<_>>()})
            .to_string(),
            ..db::Event::default()
        });
        for (ordinal, list) in submission.lists.iter().enumerate() {
            self.execute(
                capture,
                submission,
                list,
                (ordinal, None, 0),
                &mut Pipeline::default(),
            );
        }
        let mut seen = std::collections::HashSet::new();
        capture.events[first..]
            .iter()
            .flat_map(|e| e.reads.iter().chain(&e.writes).copied())
            .filter(|r| seen.insert(*r))
            .collect()
    }

    /// The resources the last submission projected may read through a descriptor heap its root signatures index
    /// directly, beyond what its events name: every one a slot of the heap names as the submission found it (and
    /// each view's counter), in slot order. A program picks its slots at run time, so any of them may be read.
    pub fn heap_indexed(&mut self) -> Vec<u64> {
        let mut named = Vec::new();
        for &heap in &self.indexed {
            for d in self
                .descriptors
                .range((heap, 0)..=(heap, u32::MAX))
                .map(|(_, d)| d)
            {
                for r in [d.resource, d.counter] {
                    if r != 0 && self.heap_named.insert(r) {
                        named.push(r);
                    }
                }
            }
        }
        named
    }

    /// Projects one execution of a list (or a bundle it executes); returns its last graphics draw.
    fn execute(
        &mut self,
        capture: &mut db::Capture,
        submission: &Submission,
        list: &Arc<List>,
        location: (usize, Option<u64>, usize),
        pipeline: &mut Pipeline,
    ) -> Option<u64> {
        let (ordinal, parent, depth) = location;
        if depth > 32 {
            capture.gap(
                "d3d12-bundle-cycle",
                format!("list {} generation {}", list.id, list.generation),
                1,
            );
            return None;
        }
        let first = self.executed.insert((list.id, list.generation));
        if depth == 0 {
            self.drawn.clear();
        }
        let mut last_draw = None;
        let execution = self.next_execution;
        self.next_execution += 1;
        self.executions.push(Execution {
            id: execution,
            submission: submission.id,
            list: list.clone(),
            ordinal,
            parent,
        });
        if !list.closed {
            capture.gap(
                "d3d12-open-list",
                format!(
                    "list {} generation {} submitted before successful Close",
                    list.id, list.generation
                ),
                1,
            );
        }
        pipeline.pso = list.initial_pso;
        if parent.is_some() {
            pipeline.graphics.begin_bundle();
        }
        let mut seqs = Vec::with_capacity(list.commands.len());
        // The raytracing state object each command ran with: a DispatchRays' tables identify its exports.
        let mut state_objects = Vec::with_capacity(list.commands.len());
        for (index, command) in list.commands.iter().enumerate() {
            self.checkpoints(capture, list, index, first);
            self.outputs(list, index, &seqs, first);
            pipeline.apply(command);
            let seq = capture.events.len() as u64 + 1;
            seqs.push(seq);
            state_objects.push(pipeline.state_object);
            if matches!(command.call, "SetMarker" | "BeginEvent")
                && let Some(ann) = marker_annotation(&command.args, seq, list.id, submission.id)
            {
                self.annotations.push(ann);
            }
            // The event kinds D3D11 captures use, so both APIs browse alike.
            let kind = match command.call {
                "DrawInstanced" | "DrawIndexedInstanced" | "DispatchMesh" => "draw",
                // Ray and graph dispatches bind through the compute root signature, as a dispatch does.
                "Dispatch" | "DispatchRays" | "DispatchGraph" => "dispatch",
                "ExecuteIndirect" => indirect(self.objects.get(&command.args[0].as_u64().unwrap_or(0))),
                "CopyBufferRegion"
                | "CopyTextureRegion"
                | "CopyResource"
                | "CopyTiles"
                | "AtomicCopyBufferUINT"
                | "AtomicCopyBufferUINT64"
                | "CopyRaytracingAccelerationStructure"
                | "BuildRaytracingAccelerationStructure"
                | "EmitRaytracingAccelerationStructurePostbuildInfo"
                | "ResolveQueryData" => "copy",
                "ClearRenderTargetView"
                | "ClearDepthStencilView"
                | "ClearUnorderedAccessViewUint"
                | "ClearUnorderedAccessViewFloat" => "clear",
                "ResolveSubresource" | "ResolveSubresourceRegion" => "resolve",
                "DiscardResource" => "discard",
                "WriteBufferImmediate" => "update",
                "ResourceBarrier" | "Barrier" => "barrier",
                "ExecuteBundle" => "bundle",
                _ => "state",
            };
            if kind == "dispatch" && !list.instrumented {
                *self
                    .unrecorded
                    .entry(
                        "a dispatch of a list recorded before the capture was armed, or not for a direct queue, is \
                         not followed by copies"
                            .into(),
                    )
                    .or_default() += 1;
            }
            // The bindings this event's draw adds, which say what it reads and writes.
            let bound = capture.bindings.len();
            let draw = if matches!(kind, "draw" | "dispatch" | "indirect") {
                let n = capture.draws.len() as u64 + 1;
                let mut shaders = std::array::from_fn(|_| None);
                if let Some(pso) = self.objects.get(&pipeline.pso) {
                    for (i, slot) in shaders.iter_mut().enumerate() {
                        *slot = pso["shaders"][i]["blob"].as_str().map(str::to_string);
                    }
                }
                let mut draw = db::Draw {
                    n,
                    event: seq,
                    call: command.call,
                    args: command.args.to_string(),
                    shaders,
                    ..db::Draw::default()
                };
                if kind == "draw" {
                    graphics::project(
                        capture,
                        &mut draw,
                        &mut pipeline.graphics,
                        self.objects.get(&pipeline.pso),
                    );
                }
                let roots = match kind {
                    "draw" => [pipeline.graphics_root, 0],
                    "dispatch" => [pipeline.compute_root, 0],
                    _ => [pipeline.graphics_root, pipeline.compute_root],
                };
                if roots
                    .iter()
                    .any(|r| self.objects.get(r).is_some_and(directly_indexed))
                {
                    let mut direct_heaps = Vec::new();
                    let heaps = pipeline.descriptor_heaps.as_array().into_iter().flatten();
                    for heap in heaps.filter_map(Value::as_u64) {
                        // Type 0: CBV, SRV and UAV; a sampler heap names no resource.
                        if self
                            .objects
                            .get(&heap)
                            .is_some_and(|d| d["type"].as_u64() == Some(0))
                        {
                            self.indexed.insert(heap);
                            direct_heaps.push(heap);
                        }
                    }
                    // Track bindless descriptor indices passed via 32-bit root constants or descriptor tables.
                    let active_args = if kind == "dispatch" {
                        &pipeline.compute_args
                    } else {
                        &pipeline.graphics_args
                    };
                    let mut seen_usage = std::collections::HashSet::new();
                    for arg in active_args.values() {
                        match arg["kind"].as_str() {
                            Some("constants") => {
                                for w in arg["words"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .filter_map(Value::as_u64)
                                {
                                    let slot = w as u32;
                                    for &heap in &direct_heaps {
                                        if let Some(d) = self.descriptors.get(&(heap, slot))
                                            && d.resource != 0
                                        {
                                            let access = if d.kind == "uav" { "write" } else { "read" };
                                            if seen_usage.insert((heap, slot, access)) {
                                                self.bindless_usage.push(BindlessUsage {
                                                    draw: n,
                                                    event: seq,
                                                    heap,
                                                    slot,
                                                    kind: d.kind.to_string(),
                                                    resource: d.resource,
                                                    access,
                                                    source: "root-constant",
                                                });
                                            }
                                        }
                                    }
                                }
                            }
                            Some("DescriptorTable") => {
                                let heap = arg["value"]["heap"].as_u64().unwrap_or(0);
                                let slot = arg["value"]["slot"].as_u64().unwrap_or(0) as u32;
                                if direct_heaps.contains(&heap)
                                    && let Some(d) = self.descriptors.get(&(heap, slot))
                                    && d.resource != 0
                                {
                                    let access = if d.kind == "uav" { "write" } else { "read" };
                                    if seen_usage.insert((heap, slot, access)) {
                                        self.bindless_usage.push(BindlessUsage {
                                            draw: n,
                                            event: seq,
                                            heap,
                                            slot,
                                            kind: d.kind.to_string(),
                                            resource: d.resource,
                                            access,
                                            source: "descriptor-table",
                                        });
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                capture.draws.push(draw);
                if kind == "draw" {
                    last_draw = Some(n);
                }
                let from_records = match command.call {
                    "ExecuteIndirect" => self
                        .objects
                        .get(&command.args[0].as_u64().unwrap_or(0))
                        .map(indirect::root_parameters)
                        .unwrap_or_default(),
                    _ => Vec::new(),
                };
                bindings::project(
                    capture,
                    n,
                    kind == "dispatch",
                    pipeline,
                    &self.objects,
                    &self.descriptors,
                    &from_records,
                );
                for b in capture.bindings[bound..].iter().filter(|b| b.stage == "om") {
                    if let Some(view) = b.view {
                        let target = if b.kind == "dsv" {
                            "depth".to_string()
                        } else {
                            format!("rt{}", b.slot)
                        };
                        self.drawn.insert((target, view), n);
                    }
                }
                self.draw_states.push((n, submission.id, pipeline.clone()));
                Some(n)
            } else {
                None
            };
            capture.events.push(db::Event {
                seq,
                kind,
                call: command.call,
                list: Some(list.id),
                deferred: true,
                draw,
                args: command.args.to_string(),
                ..db::Event::default()
            });
            let event = capture.events.last_mut().unwrap();
            accesses(command, &mut event.reads, &mut event.writes);
            for binding in &capture.bindings[bound..] {
                if let Some(resource) = binding.resource {
                    if matches!(
                        binding.kind,
                        "uav" | "uav_counter" | "rtv" | "dsv" | "so" | "so_counter"
                    ) {
                        event.writes.push(resource);
                    }
                    event.reads.push(resource);
                }
            }
            event.reads.sort_unstable();
            event.reads.dedup();
            event.writes.sort_unstable();
            event.writes.dedup();
            self.locations.push((seq, execution, index, submission.queue));
            if let Some(diagnostic) = &command.diagnostic {
                capture.gap(
                    "d3d12-command",
                    format!("event {seq}, {}: {diagnostic}", command.call),
                    1,
                );
            }
            if let Some(bundle) = &command.bundle {
                last_draw = self
                    .execute(
                        capture,
                        submission,
                        bundle,
                        (index, Some(execution), depth + 1),
                        pipeline,
                    )
                    .or(last_draw);
            }
        }
        self.checkpoints(capture, list, list.commands.len(), first);
        self.outputs(list, list.commands.len(), &seqs, first);
        self.arguments(list, &seqs, &state_objects, submission);
        if depth == 0 && list.kind == 0 && last_draw.is_some() {
            if list.instrumented {
                self.pass_ends.lists_instrumented += 1;
            } else {
                self.pass_ends.lists_uninstrumented += 1;
            }
        }
        if depth == 0 && list.kind == 0 && !list.instrumented && last_draw.is_some() {
            *self
                .uncopied
                .entry(
                    "list recorded before the capture was armed: its pass ends were not copied (a request's \
                     `early N` or `arm-early on` in settings.txt instruments such lists)"
                        .into(),
                )
                .or_default() += 1;
        }
        last_draw
    }
}

/// A resource's depth and array size: a volume's DepthOrArraySize is its depth, any other texture's its slices.
fn extent(d: &Value) -> (u32, u32) {
    let n = d["depth_or_array_size"].as_u64().unwrap_or(1) as u32;
    if d["dimension"] == 4 { (n, 1) } else { (1, n) }
}

/// Whether a resource (its description) holds acceleration structures: a buffer made in
/// RAYTRACING_ACCELERATION_STRUCTURE, or with the flag of that name. It stays so for its life, and no copy may read
/// or write it: its contents are the builds into it.
pub fn acceleration_structures(d: &Value) -> bool {
    d["initial_state"].as_i64() == Some(0x40_0000) || d["flags"].as_i64().is_some_and(|f| f & 0x100 != 0)
}

/// A resource's subresources in the canonical layout (see [`crate::layout`]), from its description. D3D12's planar
/// depth-stencil formats are laid out as the Direct3D 11 format of the same name, one texel holding both planes; a
/// multisampled texture's slices as an image a sample ([`crate::layout::multisampled`]).
pub fn layout(d: &Value) -> Option<Vec<crate::layout::Sub>> {
    let n = |key: &str| d[key].as_u64().unwrap_or(0) as u32;
    if d["dimension"] == 1 {
        return Some(crate::layout::buffer_of(d["width"].as_u64().unwrap_or(0)));
    }
    let (depth, array) = extent(d);
    if n("sample_count") > 1 {
        return crate::layout::multisampled(n("format"), n("width"), n("height"), array, n("sample_count"));
    }
    crate::layout::texture(n("format"), n("width"), n("height"), depth, n("mips"), array)
}

/// The resources a command reads and writes by itself, apart from the views and buffers a draw or dispatch binds
/// (those come from its bindings).
fn accesses(command: &Command, r: &mut Vec<u64>, w: &mut Vec<u64>) {
    let a = &command.args;
    // A resource id, or an address resolved to one.
    let at = |v: &Value| v["resource"].as_u64().or_else(|| v.as_u64()).filter(|r| *r != 0);
    let view = |v: &Value| v["definition"]["resource"].as_u64().filter(|r| *r != 0);
    match command.call {
        "CopyBufferRegion" => {
            r.extend(at(&a[2]));
            w.extend(at(&a[0]));
        }
        "CopyResource" => {
            r.extend(at(&a[1]));
            w.extend(at(&a[0]));
        }
        "CopyTextureRegion" => {
            r.extend(at(&a[4]));
            w.extend(at(&a[0]));
        }
        "ResolveSubresource" => {
            r.extend(at(&a[2]));
            w.extend(at(&a[0]));
        }
        "ResolveSubresourceRegion" => {
            r.extend(at(&a[4]));
            w.extend(at(&a[0]));
        }
        "AtomicCopyBufferUINT" | "AtomicCopyBufferUINT64" => {
            r.extend(at(&a[2]));
            r.extend(a[4].as_array().into_iter().flatten().filter_map(at));
            w.extend(at(&a[0]));
        }
        // LINEAR_BUFFER_TO_SWIZZLED_TILED_RESOURCE fills the tiled resource; otherwise the buffer is filled.
        "CopyTiles" if a[5].as_u64().unwrap_or(0) & 2 != 0 => {
            r.extend(at(&a[3]));
            w.extend(at(&a[0]));
        }
        "CopyTiles" => {
            r.extend(at(&a[0]));
            w.extend(at(&a[3]));
        }
        "ResolveQueryData" => w.extend(at(&a[4])),
        "ClearRenderTargetView" | "ClearDepthStencilView" => w.extend(view(&a[0])),
        "ClearUnorderedAccessViewUint" | "ClearUnorderedAccessViewFloat" => w.extend(at(&a[2])),
        "DiscardResource" => w.extend(at(&a[0])),
        "WriteBufferImmediate" => w.extend(
            a[0].as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| at(&v["address"])),
        ),
        "ExecuteIndirect" => {
            r.extend(at(&a[2]));
            r.extend(at(&a[4]));
        }
        "SetPredication" | "RSSetShadingRateImage" => r.extend(at(&a[0])),
        "BeginRenderPass" => {
            for t in a[0].as_array().into_iter().flatten() {
                w.extend(view(&t["view"]));
                r.extend(at(&t["end"]["resolve"]["src"]));
                w.extend(at(&t["end"]["resolve"]["dst"]));
            }
            w.extend(view(&a[1]["view"]));
            for end in ["depth_end", "stencil_end"] {
                r.extend(at(&a[1][end]["resolve"]["src"]));
                w.extend(at(&a[1][end]["resolve"]["dst"]));
            }
        }
        "BuildRaytracingAccelerationStructure" => {
            let b = &a[0];
            w.extend(at(&b["dest"]));
            w.extend(at(&b["scratch"]));
            r.extend(at(&b["source"]));
            let content = &b["inputs"]["content"];
            r.extend(at(&content["instances"]));
            for g in content["geometry"].as_array().into_iter().flatten() {
                let s = &g["shape"];
                for v in [
                    &s["transform"],
                    &s["index_buffer"],
                    &s["vertex_buffer"]["address"],
                    &s["buffer"]["address"],
                ] {
                    r.extend(at(v));
                }
            }
            w.extend(
                a[1].as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| at(&p["dest"])),
            );
        }
        "EmitRaytracingAccelerationStructurePostbuildInfo" => {
            w.extend(
                a[0].as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| at(&p["dest"])),
            );
            r.extend(a[1].as_array().into_iter().flatten().filter_map(at));
        }
        "CopyRaytracingAccelerationStructure" => {
            r.extend(at(&a[1]));
            w.extend(at(&a[0]));
        }
        "DispatchRays" => {
            let d = &a[0];
            for table in ["ray_generation", "miss", "hit_groups", "callable"] {
                r.extend(at(&d[table]["address"]));
            }
        }
        "DispatchGraph" => r.extend(at(&a[0]["input"]["address"])),
        "SetProgram" => w.extend(at(&a[0]["backing"]["address"])),
        _ => {}
    }
}

// CPU handles are consumed while recording. Their immutable definitions may no
// longer occupy a heap slot when the list is submitted (or the capture is armed).
fn write_consumed_descriptors(tx: &Transaction<'_>, args: &Value) -> rusqlite::Result<()> {
    match args {
        Value::Object(fields) => {
            if let Some(d) = fields.get("definition").filter(|d| !d.is_null()) {
                let id = |key: &str| {
                    d[key]
                        .as_u64()
                        .ok_or(rusqlite::Error::InvalidQuery)
                        .and_then(integer)
                };
                tx.execute(
                    "INSERT OR IGNORE INTO d12_descriptor_versions VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    params![
                        id("id")?,
                        id("heap")?,
                        id("slot")?,
                        d["kind"].as_str().ok_or(rusqlite::Error::InvalidQuery)?,
                        id("resource")?,
                        id("counter")?,
                        d["description"].to_string()
                    ],
                )?;
            }
            for (key, value) in fields {
                if key != "definition" {
                    write_consumed_descriptors(tx, value)?;
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                write_consumed_descriptors(tx, value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

const SCHEMA: &str = r#"
CREATE TABLE d12_objects(id INTEGER PRIMARY KEY, kind TEXT NOT NULL, device INTEGER NOT NULL,
 desc_version INTEGER NOT NULL, description TEXT NOT NULL);
CREATE TABLE d12_lists(id INTEGER NOT NULL,generation INTEGER NOT NULL,type INTEGER NOT NULL,
 allocator INTEGER NOT NULL,initial_pso INTEGER NOT NULL,closed INTEGER NOT NULL,commands_json TEXT NOT NULL,
 PRIMARY KEY(id,generation));
CREATE TABLE d12_submissions(id INTEGER PRIMARY KEY,queue INTEGER NOT NULL,queue_seq INTEGER NOT NULL,cpu_seq INTEGER NOT NULL);
CREATE TABLE d12_executions(id INTEGER PRIMARY KEY,submission INTEGER NOT NULL,list INTEGER NOT NULL,
 generation INTEGER NOT NULL,ordinal INTEGER NOT NULL,parent_bundle_execution INTEGER,
 FOREIGN KEY(submission) REFERENCES d12_submissions(id),FOREIGN KEY(list,generation) REFERENCES d12_lists(id,generation));
CREATE TABLE d12_event_locations(event INTEGER PRIMARY KEY,execution INTEGER NOT NULL,command_index INTEGER NOT NULL,
 queue INTEGER NOT NULL,FOREIGN KEY(event) REFERENCES events(seq),FOREIGN KEY(execution) REFERENCES d12_executions(id));
CREATE TABLE d12_sync(id INTEGER PRIMARY KEY,queue INTEGER NOT NULL,queue_seq INTEGER NOT NULL,cpu_seq INTEGER NOT NULL,
 kind TEXT NOT NULL,fence INTEGER NOT NULL,value_u64 BLOB NOT NULL CHECK(length(value_u64)=8));
CREATE TABLE d12_tile_pages(resource INTEGER PRIMARY KEY,cpu_seq INTEGER NOT NULL,layout_json TEXT,pages_json TEXT,
 diagnostic TEXT);
CREATE TABLE d12_acceleration_structures(resource INTEGER NOT NULL,offset INTEGER NOT NULL,build_json TEXT NOT NULL,
 inputs_json TEXT,diagnostic TEXT,PRIMARY KEY(resource,offset));
CREATE TABLE d12_tile_mappings(id INTEGER PRIMARY KEY,queue INTEGER NOT NULL,queue_seq INTEGER NOT NULL,
 cpu_seq INTEGER NOT NULL,call TEXT NOT NULL,resource INTEGER NOT NULL,source INTEGER,args_json TEXT NOT NULL,
 pages_json TEXT,diagnostic TEXT);
CREATE TABLE d12_psos(id INTEGER PRIMARY KEY,kind TEXT NOT NULL,description TEXT NOT NULL);
CREATE TABLE d12_roots(id INTEGER PRIMARY KEY,blob TEXT,decoded_json TEXT NOT NULL);
CREATE TABLE d12_draw_state(draw INTEGER PRIMARY KEY,pso INTEGER,graphics_root INTEGER,compute_root INTEGER,root_args_json TEXT NOT NULL);
CREATE TABLE d12_descriptor_versions(id INTEGER PRIMARY KEY,heap INTEGER NOT NULL,slot INTEGER NOT NULL,
 kind TEXT NOT NULL,resource INTEGER NOT NULL,counter_resource INTEGER NOT NULL,description TEXT NOT NULL);
CREATE TABLE d12_submission_descriptors(submission INTEGER NOT NULL,version INTEGER NOT NULL,PRIMARY KEY(submission,version));
CREATE TABLE d12_first_use(resource INTEGER PRIMARY KEY,submission INTEGER NOT NULL,states_json TEXT NOT NULL,
 diagnostic TEXT);
CREATE TABLE d12_upload_versions(submission INTEGER NOT NULL,resource INTEGER NOT NULL,whole INTEGER NOT NULL,
 offset INTEGER NOT NULL,content_json TEXT NOT NULL,PRIMARY KEY(submission,resource,whole,offset));
CREATE TABLE d12_selected_passes(submission INTEGER PRIMARY KEY,queue INTEGER NOT NULL,cpu_seq INTEGER NOT NULL,
 reason TEXT NOT NULL,bindless_heaps_json TEXT NOT NULL,bindless_max_version INTEGER NOT NULL,resources_json TEXT NOT NULL);
CREATE TABLE d12_indirect_arguments(event INTEGER PRIMARY KEY,submission INTEGER NOT NULL,queue INTEGER NOT NULL,
 signature INTEGER NOT NULL,max_count INTEGER NOT NULL,arg_buffer INTEGER NOT NULL,arg_offset INTEGER NOT NULL,
 arg_bytes INTEGER NOT NULL,count_buffer INTEGER NOT NULL,count_offset INTEGER NOT NULL,count_value INTEGER,
 stride INTEGER NOT NULL,blob TEXT,diagnostic TEXT);
CREATE TABLE d12_fence_state(id INTEGER PRIMARY KEY,phase TEXT NOT NULL,submission INTEGER,cpu_seq INTEGER NOT NULL,
 fence INTEGER NOT NULL,completed_u64 BLOB NOT NULL CHECK(length(completed_u64)=8),producer_queue INTEGER,
 producer_submission INTEGER,producer_value_u64 BLOB,producer_cpu_seq INTEGER);
CREATE TABLE d12_submission_sync(id INTEGER PRIMARY KEY,submission INTEGER NOT NULL,queue INTEGER NOT NULL,
 cpu_seq INTEGER NOT NULL,fence INTEGER NOT NULL,wait_value_u64 BLOB NOT NULL CHECK(length(wait_value_u64)=8),
 completed_value_u64 BLOB,producer_queue INTEGER,producer_submission INTEGER,producer_value_u64 BLOB,
 satisfied INTEGER NOT NULL);
CREATE TABLE d12_tile_snapshots(submission INTEGER NOT NULL,resource INTEGER NOT NULL,cpu_seq INTEGER NOT NULL,
 pages_json TEXT,blob TEXT,diagnostic TEXT,PRIMARY KEY(submission,resource));
CREATE TABLE d12_bindless_usage(draw INTEGER NOT NULL,event INTEGER NOT NULL,heap INTEGER NOT NULL,
 slot INTEGER NOT NULL,kind TEXT NOT NULL,resource INTEGER NOT NULL,access TEXT NOT NULL,source TEXT NOT NULL,
 PRIMARY KEY(draw,heap,slot,access));
CREATE TABLE d12_annotations(id INTEGER PRIMARY KEY,seq INTEGER,list INTEGER,submission INTEGER,
 category TEXT NOT NULL,payload_json TEXT NOT NULL);
"#;

/// Decodes a PIX/D3D12 SetMarker/BeginEvent payload if it carries structured JSON (`{...}`) or an `xcl:`/`locum:`
/// annotation prefix.
fn marker_annotation(args: &Value, seq: u64, list: u64, submission: u64) -> Option<Annotation> {
    let bytes: Vec<u8> = args
        .get(1)?
        .as_array()?
        .iter()
        .filter_map(|v| v.as_u64().map(|n| n as u8))
        .collect();
    if bytes.is_empty() {
        return None;
    }
    let text = if bytes.len() >= 2 && bytes.len().is_multiple_of(2) && bytes[1] == 0 {
        let u16s: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&w| w != 0)
            .collect();
        String::from_utf16_lossy(&u16s)
    } else {
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    };
    let trimmed = text.trim();
    if trimmed.starts_with('{')
        && let Ok(val) = serde_json::from_str::<Value>(trimmed)
    {
        let category = val["category"]
            .as_str()
            .or_else(|| val["cat"].as_str())
            .unwrap_or("marker")
            .to_string();
        return Some(Annotation {
            seq: Some(seq),
            list: Some(list),
            submission: Some(submission),
            category,
            payload: val,
        });
    }
    if let Some(rest) = trimmed
        .strip_prefix("xcl:")
        .or_else(|| trimmed.strip_prefix("locum:"))
    {
        let rest = rest.trim();
        let (category, payload) = match rest.split_once(char::is_whitespace) {
            Some((cat, body)) => {
                let payload = serde_json::from_str::<Value>(body.trim())
                    .unwrap_or_else(|_| serde_json::json!({"text": body.trim()}));
                (cat.to_string(), payload)
            }
            None => ("marker".to_string(), serde_json::json!({"text": rest})),
        };
        return Some(Annotation {
            seq: Some(seq),
            list: Some(list),
            submission: Some(submission),
            category,
            payload,
        });
    }
    None
}

/// Parses newline-delimited JSON annotations (`annotations.jsonl`) where each line is a JSON object with optional
/// `seq`, `list`, `submission`, `category` (or `cat`), and `payload` (defaults to the whole object).
pub fn parse_annotations_jsonl(text: &str) -> Vec<Annotation> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let category = v["category"]
            .as_str()
            .or_else(|| v["cat"].as_str())
            .unwrap_or("sidecar")
            .to_string();
        let payload = if !v["payload"].is_null() {
            v["payload"].clone()
        } else {
            v.clone()
        };
        out.push(Annotation {
            seq: v["seq"].as_u64().or_else(|| v["event"].as_u64()),
            list: v["list"].as_u64(),
            submission: v["submission"].as_u64(),
            category,
            payload,
        });
    }
    out
}

/// Appends annotations into `d12_annotations` of an existing capture database (creating the table if absent).
pub fn append_annotations(db_path: &std::path::Path, rows: &[Annotation]) -> rusqlite::Result<()> {
    let mut conn = rusqlite::Connection::open(db_path)?;
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS d12_annotations(id INTEGER PRIMARY KEY,seq INTEGER,list INTEGER,\
         submission INTEGER,category TEXT NOT NULL,payload_json TEXT NOT NULL);",
    )?;
    for a in rows {
        tx.execute(
            "INSERT INTO d12_annotations(seq,list,submission,category,payload_json) VALUES(?1,?2,?3,?4,?5)",
            params![
                a.seq.map(integer).transpose()?,
                a.list.map(integer).transpose()?,
                a.submission.map(integer).transpose()?,
                a.category,
                a.payload.to_string()
            ],
        )?;
    }
    tx.commit()
}

fn integer(value: u64) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn only_top_level_builds_by_pointer_have_instance_pointers() {
        let build = |kind, layout| json!({"inputs": {"type": kind, "layout": layout, "count": 3}});
        assert_eq!(instance_pointers(&build(0, 1)), 3);
        assert_eq!(
            instance_pointers(&build(0, 0)),
            0,
            "an array of instance descriptions"
        );
        assert_eq!(instance_pointers(&build(1, 1)), 0, "a bottom-level build");
        assert_eq!(instance_pointers(&json!({})), 0);
        let mut bytes = Vec::new();
        for p in [0x1234_5678_9abc_u64, 0, u64::MAX] {
            bytes.extend(p.to_le_bytes());
        }
        bytes.push(7);
        assert_eq!(
            pointers(&bytes).collect::<Vec<_>>(),
            [0x1234_5678_9abc, 0, u64::MAX],
            "a partial pointer is none"
        );
    }

    #[test]
    fn set_pipeline_state_and_set_pipeline_state1_unset_each_other() {
        let command = |call, id: u64| Command {
            call,
            args: json!([id]),
            bundle: None,
            diagnostic: None,
        };
        let mut p = Pipeline::default();
        p.apply(&command("SetPipelineState", 5));
        p.apply(&command("SetPipelineState1", 9));
        assert_eq!((p.pso, p.state_object), (0, 9));
        p.apply(&command("SetPipelineState", 6));
        assert_eq!((p.pso, p.state_object), (6, 0));
    }

    #[test]
    fn old_recordings_repeated_executions_and_bundle_generations_stay_distinct() {
        let mut bundle = Recording::new(2, 1, 20, 0, false);
        bundle.record(Command {
            call: "DrawInstanced",
            args: json!([3, 1, 0, 0]),
            bundle: None,
            diagnostic: None,
        });
        bundle.close();
        let mut list = Recording::new(1, 0, 10, 0, false);
        list.record(Command {
            call: "ExecuteBundle",
            args: json!({"list":2}),
            bundle: Some(bundle.snapshot()),
            diagnostic: None,
        });
        list.close();
        let old = list.snapshot();
        list.reset(11, 50);
        list.record(Command {
            call: "Dispatch",
            args: json!([7, 2, 1]),
            bundle: None,
            diagnostic: None,
        });
        list.close();
        bundle.reset(21, 0);
        let frame = Frame {
            submissions: vec![Submission {
                id: 1,
                queue: 7,
                queue_seq: 1,
                cpu_seq: 1,
                lists: vec![old.clone(), old, list.snapshot()],
                descriptors: Vec::new(),
                uploads: Vec::new(),
            }],
            sync: vec![Sync {
                queue: 7,
                queue_seq: 2,
                cpu_seq: 2,
                kind: "Signal",
                fence: 88,
                value: u64::MAX,
            }],
            ..Frame::default()
        };
        let mut c = db::Capture::default();
        let prepared = frame.prepare(&mut c);
        assert_eq!(c.draws.len(), 3);
        assert_eq!(c.draws[0].call, "DrawInstanced");
        assert_eq!(c.draws[2].call, "Dispatch");
        assert!(c.gaps.is_empty());
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(db::SCHEMA).unwrap();
        let tx = conn.transaction().unwrap();
        for event in &c.events {
            tx.execute(
                "INSERT INTO events(seq,kind,call) VALUES(?1,?2,?3)",
                params![integer(event.seq).unwrap(), event.kind, event.call],
            )
            .unwrap();
        }
        prepared.write(&tx).unwrap();
        assert_eq!(
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            tx.query_row("SELECT count(*) FROM d12_lists", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            tx.query_row("SELECT count(*) FROM d12_executions", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            5
        );
        assert_eq!(
            tx.query_row("SELECT value_u64 FROM d12_sync", [], |r| r.get::<_, Vec<u8>>(0))
                .unwrap(),
            u64::MAX.to_le_bytes()
        );
    }

    #[test]
    fn shader_tables_name_what_their_records_local_arguments_do_as_of_their_submission() {
        let object = |id, kind, description| Object {
            id,
            kind,
            device: 1,
            description,
        };
        let id = "aa".repeat(programs::IDENTIFIER);
        // State object #20 runs `gen` with local root signature #21 (its default): a root SRV, then a table of two UAVs.
        let objects = vec![
            object(
                20,
                "ID3D12StateObject",
                json!({"kind": "state_object", "parent": 0, "identifiers": {"gen": id}, "subobjects": [
                    {"type": 5, "value": {"library": {"blob": "lib", "size": 1}, "exports": []}},
                    {"type": 2, "value": 21},
                ]}),
            ),
            object(
                21,
                "ID3D12RootSignature",
                json!({"decoded": {"parameters": [
                    {"type": 3, "value": {}},
                    {"type": 0, "value": [{"RangeType": 1, "NumDescriptors": 2, "OffsetInDescriptorsFromTableStart": 0}]},
                ]}}),
            ),
            object(
                30,
                "ID3D12DescriptorHeap",
                json!({"gpu_start": 0x10000, "count": 8, "stride": 32}),
            ),
        ];
        let uav = |id, slot, resource, counter| Descriptor {
            id,
            heap: 30,
            slot,
            kind: "uav",
            resource,
            counter,
            description: json!({}),
        };
        let mut list = Recording::new(1, 0, 10, 0, false);
        list.instrument();
        list.record(Command {
            call: "SetPipelineState1",
            args: json!([20]),
            bundle: None,
            diagnostic: None,
        });
        list.record(Command {
            call: "DispatchRays",
            args: json!([{}]),
            bundle: None,
            diagnostic: None,
        });
        let table = tables::Table {
            which: tables::Which::RayGeneration,
            address: 0x5000,
            bytes: 48,
            stride: 48,
        };
        list.arguments(Arguments {
            id: 40,
            command: 1,
            bytes: 48,
            tables: vec![table],
            ..Arguments::default()
        });
        list.close();
        let submission = |id, descriptors| Submission {
            id,
            queue: 7,
            queue_seq: id,
            cpu_seq: id,
            lists: vec![list.snapshot()],
            descriptors,
            uploads: Vec::new(),
        };
        // Slot 3 holds #12's view (with counter #13) at the first submission, #11's from the second.
        let frame = Frame {
            objects,
            submissions: vec![
                submission(1, vec![uav(1, 2, 10, 0), uav(2, 3, 12, 13)]),
                submission(2, vec![uav(3, 3, 11, 0)]),
            ],
            ..Frame::default()
        };
        let mut c = db::Capture::default();
        let prepared = frame.prepare(&mut c);
        let rays: Vec<_> = prepared.indirect().to_vec();
        assert_eq!(rays.len(), 2);
        assert!(
            rays.iter()
                .all(|r| r.state_object == 20 && r.arguments.tables == [table])
        );
        // The record: identifier, the root SRV's address (in #9), the table's handle (slot 2).
        let mut record = vec![0xaa; programs::IDENTIFIER];
        record.extend(0x9000u64.to_le_bytes());
        record.extend((0x10000u64 + 2 * 32).to_le_bytes());
        let declared = |_: &str| {
            Ok(Arc::new(programs::Declared {
                shaders: vec!["gen".into()],
                subobjects: Vec::new(),
            }))
        };
        let mut decode = |l: &programs::Local| {
            assert_eq!(l, &programs::Local::Object(21));
            Ok(prepared.frame.objects[1].description["decoded"].clone())
        };
        let buffers = |a: u64| if a == 0x9000 { vec![9] } else { Vec::new() };
        let first = prepared.named_by_tables(&rays[0], &[table], &record, &declared, &mut decode, &buffers);
        let names = |reads: Vec<u64>, writes: Vec<u64>| TableNames {
            reads,
            writes,
            unplaced: BTreeMap::new(),
        };
        assert_eq!(first, names(vec![9], vec![10, 12, 13]));
        let second = prepared.named_by_tables(&rays[1], &[table], &record, &declared, &mut decode, &buffers);
        assert_eq!(second, names(vec![9], vec![10, 11]));
        // An address in no buffer, and a handle in no heap, are counted.
        record[32..40].copy_from_slice(&0x7000u64.to_le_bytes());
        record[40..48].copy_from_slice(&0x90000u64.to_le_bytes());
        let TableNames {
            reads,
            writes,
            unplaced,
        } = prepared.named_by_tables(&rays[0], &[table], &record, &declared, &mut decode, &buffers);
        assert!(reads.is_empty() && writes.is_empty());
        assert_eq!(
            unplaced.into_iter().collect::<Vec<_>>(),
            [
                (
                    "a descriptor table's handle is in no shader-visible heap".to_string(),
                    1
                ),
                ("a root descriptor's address is in no live buffer".to_string(), 1)
            ]
        );
    }

    #[test]
    fn a_heap_a_root_signature_indexes_directly_names_every_resource_its_slots_hold() {
        let object = |id, kind, description| Object {
            id,
            kind,
            device: 1,
            description,
        };
        let buffer = |id| {
            object(
                id,
                "ID3D12Resource",
                json!({"dimension": 1, "width": 256, "height": 1, "mips": 1, "heap_properties": {"type": 1}}),
            )
        };
        // Root signature #21 indexes the heap directly, #22 does not; #30 is a CBV/SRV/UAV heap, #31 a sampler heap.
        let mut objects = vec![
            object(
                21,
                "ID3D12RootSignature",
                json!({"decoded": {"flags": 0x400, "parameters": []}}),
            ),
            object(
                22,
                "ID3D12RootSignature",
                json!({"decoded": {"flags": 0, "parameters": []}}),
            ),
            object(
                30,
                "ID3D12DescriptorHeap",
                json!({"type": 0, "count": 8, "stride": 32}),
            ),
            object(
                31,
                "ID3D12DescriptorHeap",
                json!({"type": 1, "count": 8, "stride": 32}),
            ),
        ];
        objects.extend((10..16).map(buffer));
        let view = |id, slot, resource, counter| Descriptor {
            id,
            heap: 30,
            slot,
            kind: "uav",
            resource,
            counter,
            description: json!({}),
        };
        let list = |id, root| {
            let mut list = Recording::new(id, 0, 10 + id, 0, false);
            for (call, args) in [
                ("SetDescriptorHeaps", json!([[30, 31]])),
                ("SetComputeRootSignature", json!([root])),
                ("Dispatch", json!([1, 1, 1])),
            ] {
                list.record(Command {
                    call,
                    args,
                    bundle: None,
                    diagnostic: None,
                });
            }
            list.close();
            list.snapshot()
        };
        let (plain, indexed) = (list(1, 22), list(2, 21));
        let submission = |id, list: &Arc<List>, descriptors| Submission {
            id,
            queue: 7,
            queue_seq: id,
            cpu_seq: id,
            lists: vec![list.clone()],
            descriptors,
            uploads: Vec::new(),
        };
        let frame = Frame {
            objects,
            ..Frame::default()
        };
        let mut c = db::Capture::default();
        let mut projection = Projection::new(&frame.objects);
        // A root signature that does not index the heap: its slots are read through tables alone.
        projection.submit(
            &mut c,
            &submission(1, &plain, vec![view(1, 0, 10, 0), view(2, 1, 11, 12)]),
        );
        assert!(projection.heap_indexed().is_empty());
        // One that does: every slot's resource and counter, those of slots written at this submission too.
        projection.submit(&mut c, &submission(2, &indexed, vec![view(3, 2, 13, 0)]));
        assert_eq!(projection.heap_indexed(), [10, 11, 12, 13]);
        // A later submission names only what the frame's earlier ones did not.
        projection.submit(&mut c, &submission(3, &indexed, vec![view(4, 0, 14, 0)]));
        assert_eq!(projection.heap_indexed(), [14]);
        // The frame uses them all, the resource no slot named only not.
        let _ = frame.finish(&mut c, projection);
        let used = |id| c.resources.iter().find(|r| r.id == id).unwrap().used;
        assert!((10..15).all(used) && !used(15));
    }

    #[test]
    fn indirect_records_are_found_on_their_events_or_are_gaps() {
        let object = |id, kind, description| Object {
            id,
            kind,
            device: 1,
            description,
        };
        let command = |call, args, bundle| Command {
            call,
            args,
            bundle,
            diagnostic: None,
        };
        let execute = |signature: u64| command("ExecuteIndirect", json!([signature, 2, 8, 0, 0, 0]), None);
        let buffer = |id| {
            object(
                id,
                "ID3D12Resource",
                json!({"dimension": 1, "width": 256, "height": 1, "mips": 1, "heap_properties": {"type": 1}}),
            )
        };
        // Signatures whose records set a root CBV, of draws alone, and whose records set a root UAV; and the buffers
        // the records name.
        let objects = vec![
            object(
                5,
                "ID3D12CommandSignature",
                json!({"stride": 24, "arguments": [{"type": 6, "root_parameter": 0}, {"type": 0}]}),
            ),
            object(
                6,
                "ID3D12CommandSignature",
                json!({"stride": 16, "arguments": [{"type": 0}]}),
            ),
            object(
                8,
                "ID3D12CommandSignature",
                json!({"stride": 20, "arguments": [{"type": 8, "root_parameter": 0}, {"type": 2}]}),
            ),
            buffer(9),
            buffer(10),
            buffer(11),
            buffer(12),
        ];
        let copy = |id, command, signature: usize| Arguments {
            id,
            command,
            layout: indirect::layout(&objects[signature].description).unwrap(),
            bytes: 48,
            counted: false,
            ..Default::default()
        };
        // A bundle may run only a signature of draws or dispatches alone: its records name nothing, so it copies none.
        let mut bundle = Recording::new(3, 1, 12, 0, false);
        bundle.record(execute(6));
        bundle.close();
        // Instrumented: its records copied before each ExecuteIndirect.
        let mut copied = Recording::new(1, 0, 10, 0, false);
        copied.instrument();
        copied.record(execute(5));
        copied.record(execute(8));
        copied.record(command(
            "ExecuteBundle",
            json!({"list": 3}),
            Some(bundle.snapshot()),
        ));
        copied.record(execute(8));
        for a in [copy(40, 0, 0), copy(41, 1, 2), copy(42, 3, 2)] {
            copied.arguments(a);
        }
        copied.close();
        // Recorded before arming: none copied.
        let mut uncopied = Recording::new(2, 0, 11, 0, false);
        uncopied.record(execute(5));
        uncopied.record(execute(6));
        uncopied.record(command(
            "ExecuteBundle",
            json!({"list": 3}),
            Some(bundle.snapshot()),
        ));
        uncopied.close();
        let frame = Frame {
            objects,
            submissions: vec![Submission {
                id: 3,
                queue: 7,
                queue_seq: 1,
                cpu_seq: 1,
                lists: vec![copied.snapshot(), uncopied.snapshot()],
                descriptors: Vec::new(),
                uploads: Vec::new(),
            }],
            ..Frame::default()
        };
        let mut c = db::Capture::default();
        let mut prepared = frame.prepare(&mut c);
        let found = prepared.indirect().to_vec();
        assert_eq!(
            found.iter().map(|i| i.arguments.id).collect::<Vec<_>>(),
            vec![40, 41, 42]
        );
        assert!(found.iter().all(|i| (i.submission, i.queue) == (3, 7)));
        let event = |i: usize| found[i].event;
        let call = |seq: u64| c.events.iter().find(|e| e.seq == seq).unwrap().call;
        assert!((0..3).all(|i| call(event(i)) == "ExecuteIndirect"));
        // The last is after the bundle's events.
        assert_eq!(call(event(2) - 2), "ExecuteBundle");
        // The draws-only signature's records name nothing: no gap for them, in the bundles or the uncopied list. The
        // uncopied list's root CBV records are a gap.
        let gaps: Vec<_> = prepared.projection.unread_arguments.iter().collect();
        assert_eq!(gaps.len(), 1);
        assert!(gaps[0].0.starts_with("its list was not instrumented"));
        assert_eq!(*gaps[0].1, 1);

        // What the records name. #9 is read only through them; #10 written only through them; #11 written through
        // two ExecuteIndirects' records; #12 written through the last's, and used otherwise (copied before).
        assert!(!c.resources.iter().find(|r| r.id == 9).unwrap().used);
        prepared.named_by_arguments(&mut c, &found[0], &[9], &[]);
        prepared.named_by_arguments(&mut c, &found[1], &[], &[10, 11]);
        prepared.named_by_arguments(&mut c, &found[2], &[], &[11, 12]);
        let e = |seq: u64| c.events.iter().find(|e| e.seq == seq).unwrap();
        assert!(e(event(0)).reads.contains(&9) && e(event(0)).writes.is_empty());
        assert_eq!(e(event(1)).writes, vec![10, 11]);
        assert!(e(event(1)).reads.contains(&10));
        let r = |id: u64| c.resources.iter().find(|r| r.id == id).unwrap();
        assert!(r(9).used && !r(9).written);
        assert!(r(10).used && r(10).written);

        let copies = prepared.after_frame(&c, |r| r == 12);
        let planned: Vec<_> = copies
            .iter()
            .map(|a| (a.resource, a.submission, a.queue))
            .collect();
        assert_eq!(planned, vec![(9, 3, 7), (10, 3, 7), (11, 3, 7), (12, 3, 7)]);
        // #10 and #11 began the frame as nothing recorded says: written, and named only by records.
        for id in [10, 11] {
            let first = &prepared.first_use[&id];
            assert!(
                first
                    .content
                    .as_ref()
                    .is_err_and(|why| why.contains("a UAV they name lets the frame write it")),
                "#{id}"
            );
        }
        let ok = |key: &str| FirstUse {
            submission: 3,
            states: json!([]),
            content: Ok((key.to_string(), vec![key.to_string()])),
            note: None,
        };
        prepared.set_after_frame(9, ok("k9"));
        prepared.set_after_frame(
            10,
            FirstUse {
                content: Err("readback budget".into()),
                ..ok("")
            },
        );
        prepared.set_after_frame(11, ok("k11"));
        prepared.set_after_frame(12, ok("k12"));
        prepared.set_first_use(
            12,
            FirstUse {
                note: Some("copied behind a wait".into()),
                ..ok("k12-before")
            },
        );
        let dir = std::env::temp_dir().join(format!("locum-indirect-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        prepared.contents(&mut c, &crate::blobs::Blobs::new(&dir));
        let r = |id: u64| c.resources.iter().find(|r| r.id == id).unwrap();
        // #9's copy after the frame is its contents; #10's and #11's are not known; #12's are its first use.
        assert_eq!(
            (r(9).content.as_deref(), r(9).content_source),
            (Some("k9"), Some("after-frame"))
        );
        for id in [10, 11] {
            assert_eq!(r(id).content, None);
            assert!(r(id).content_note.as_ref().unwrap().contains("a UAV they name"));
        }
        assert_eq!(r(12).content.as_deref(), Some("k12-before"));
        // A first use's note is the resource's.
        assert_eq!(r(12).content_note.as_deref(), Some("copied behind a wait"));
        // Outputs: the last ExecuteIndirect's, of #11 and #12. The first writing #11 wrote it again later, and #10's
        // copy failed: gaps.
        let mut outputs: Vec<_> = c
            .results
            .iter()
            .map(|o| (o.event, o.resource, o.content.as_str()))
            .collect();
        outputs.sort();
        assert_eq!(outputs, vec![(event(2), 11, "k11"), (event(2), 12, "k12")]);
        let gaps: Vec<_> = c
            .gaps
            .iter()
            .filter(|g| g.what == "d3d12-results")
            .map(|g| (g.detail.clone(), g.count))
            .collect();
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        assert!(gaps.contains(&("readback budget".into(), 1)));
        assert!(
            gaps.iter()
                .any(|(why, n)| why.contains("writes it again later") && *n == 1)
        );
        let _ = std::fs::remove_dir_all(&dir);

        // Its records: two fit 48 bytes, and a count would lower that.
        let mut a = found[0].arguments.clone();
        assert_eq!(a.records(&[0; 48]), 2);
        a.counted = true;
        let mut bytes = vec![0u8; 52];
        bytes[48..].copy_from_slice(&1u32.to_le_bytes());
        assert_eq!(a.records(&bytes), 1);
    }

    #[test]
    fn complete_pass_tables_round_trip() {
        let dir = std::env::temp_dir().join(format!("locum-d12-tables-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("capture.db");

        let mut frame = Frame::default();
        frame.selected_passes.push(SelectedPass {
            submission: 5,
            queue: 81,
            cpu_seq: 11,
            reason: "shader=0c29154c".into(),
            bindless_heaps: json!([100, 101]),
            bindless_max_version: 42,
            resources: json!([18, 1548]),
        });
        frame.fence_states.push(FenceState {
            phase: "arm",
            submission: None,
            cpu_seq: 10,
            fence: 61887,
            completed: 1709,
            producer_queue: Some(80),
            producer_submission: Some(4),
            producer_value: Some(1710),
            producer_cpu_seq: Some(9),
        });
        frame.submission_syncs.push(SubmissionSync {
            submission: 5,
            queue: 81,
            cpu_seq: 11,
            fence: 61887,
            wait_value: 1710,
            completed_value: Some(1709),
            producer_queue: Some(80),
            producer_submission: Some(4),
            producer_value: Some(1710),
            satisfied: false,
        });

        let mut cap = crate::db::Capture::default();
        let proj = Projection::new(&[]);
        let mut prepared = frame.finish(&mut cap, proj);
        prepared.set_indirect_arguments(IndirectArguments {
            event: 77,
            submission: 5,
            queue: 81,
            signature: 500,
            max_count: 16,
            arg_buffer: 1548,
            arg_offset: 256,
            arg_bytes: 64,
            count_buffer: 1549,
            count_offset: 0,
            count_value: Some(3),
            stride: 16,
            blob: Some("deadbeef".into()),
            diagnostic: None,
        });
        prepared.add_tile_snapshot(TileSnapshot {
            submission: 5,
            resource: 18,
            cpu_seq: 12,
            pages_json: Some("[[0,4,1,0]]".into()),
            blob: Some("cafe01".into()),
            diagnostic: None,
        });
        cap.d3d12 = Some(prepared);
        crate::db::write(&db_path, &cap).unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let sel: (i64, String, i64, String) = conn
            .query_row(
                "SELECT submission, reason, bindless_max_version, resources_json FROM d12_selected_passes",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(sel, (5, "shader=0c29154c".into(), 42, "[18,1548]".into()));

        let ind: (i64, i64, Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT event, arg_buffer, count_value, blob FROM d12_indirect_arguments",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(ind, (77, 1548, Some(3), Some("deadbeef".into())));

        let fence: (String, i64, Vec<u8>, Option<i64>) = conn
            .query_row(
                "SELECT phase, fence, completed_u64, producer_queue FROM d12_fence_state",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            fence,
            ("arm".into(), 61887, 1709u64.to_le_bytes().to_vec(), Some(80))
        );

        let sync: (i64, i64, Vec<u8>, Option<i64>, i64) = conn
            .query_row(
                "SELECT submission, fence, wait_value_u64, producer_submission, satisfied FROM d12_submission_sync",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(sync, (5, 61887, 1710u64.to_le_bytes().to_vec(), Some(4), 0));

        let snap: (i64, i64, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT resource, submission, pages_json, blob FROM d12_tile_snapshots",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(snap, (18, 5, Some("[[0,4,1,0]]".into()), Some("cafe01".into())));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bindless_usage_and_annotations_project_and_round_trip() {
        let dir = std::env::temp_dir().join(format!("locum-d12-bindless-ann-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db_path = dir.join("capture.db");

        let objects = vec![
            Object {
                id: 100,
                kind: "ID3D12DescriptorHeap",
                device: 1,
                description: json!({"type": 0, "count": 64, "flags": 1}),
            },
            Object {
                id: 200,
                kind: "ID3D12RootSignature",
                device: 1,
                description: json!({"decoded": {"flags": 0x400, "parameters": []}}),
            },
        ];
        let mut list = Recording::new(10, 0, 1, 0, false);
        let marker_bytes: Vec<u8> = r#"{"cat":"xcl.barrier","group":7}"#.bytes().collect();
        list.record(Command {
            call: "SetMarker",
            args: json!([0, marker_bytes]),
            bundle: None,
            diagnostic: None,
        });
        list.record(Command {
            call: "SetDescriptorHeaps",
            args: json!([[100]]),
            bundle: None,
            diagnostic: None,
        });
        list.record(Command {
            call: "SetComputeRootSignature",
            args: json!([200]),
            bundle: None,
            diagnostic: None,
        });
        list.record(Command {
            call: "SetComputeRoot32BitConstant",
            args: json!([0, 7, 0]),
            bundle: None,
            diagnostic: None,
        });
        list.record(Command {
            call: "Dispatch",
            args: json!([1, 1, 1]),
            bundle: None,
            diagnostic: None,
        });
        list.close();

        let frame = Frame {
            objects,
            submissions: vec![Submission {
                id: 1,
                queue: 80,
                queue_seq: 1,
                cpu_seq: 1,
                lists: vec![list.snapshot()],
                descriptors: vec![Descriptor {
                    id: 1,
                    heap: 100,
                    slot: 7,
                    kind: "uav",
                    resource: 999,
                    counter: 0,
                    description: json!({}),
                }],
                uploads: Vec::new(),
            }],
            ..Frame::default()
        };
        let mut cap = crate::db::Capture::default();
        let mut prepared = frame.prepare(&mut cap);
        let sidecar = parse_annotations_jsonl(
            "# comment\n{\"seq\":5,\"category\":\"xcl.placement\",\"payload\":{\"res\":999,\"unmapped\":true}}\n",
        );
        assert_eq!(sidecar.len(), 1);
        prepared.add_annotations(sidecar);
        cap.d3d12 = Some(prepared);
        crate::db::write(&db_path, &cap).unwrap();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let usage: (i64, i64, i64, String, i64, String, String) = conn
            .query_row(
                "SELECT draw, heap, slot, kind, resource, access, source FROM d12_bindless_usage",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(
            usage,
            (
                1,
                100,
                7,
                "uav".into(),
                999,
                "write".into(),
                "root-constant".into()
            )
        );
        let ann_count: i64 = conn
            .query_row("SELECT count(*) FROM d12_annotations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ann_count, 2);
        drop(conn);

        // Offline append_annotations adds rows to existing DB cleanly.
        let extra = parse_annotations_jsonl("{\"seq\":9,\"cat\":\"xcl.shader\",\"console_hash\":\"abc\"}");
        append_annotations(&db_path, &extra).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let ann_count2: i64 = conn
            .query_row("SELECT count(*) FROM d12_annotations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ann_count2, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
