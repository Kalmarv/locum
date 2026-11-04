//! Owned, portable D3D12 command history. A list recording and an execution of that
//! recording have different identities; resetting a list cannot change an earlier execution.
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use rusqlite::{Transaction, params};
use serde::Serialize;
use serde_json::Value;

use crate::db;
mod bindings;
mod graphics;

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
    /// The targets copied inside this generation where its passes ended.
    #[serde(skip)]
    pub checkpoints: Vec<Checkpoint>,
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

#[derive(Clone, Debug, Default)]
pub struct Frame {
    pub objects: Vec<Object>,
    pub submissions: Vec<Submission>,
    pub sync: Vec<Sync>,
    pub diagnostics: Vec<String>,
}

/// The database extension and the common-table projection share this one prepared plan.
pub struct Prepared {
    frame: Frame,
    projection: Projection,
    first_use: BTreeMap<u64, FirstUse>,
    /// Checkpoint copies read back: the key of each target's bytes, or why there are none.
    checkpoints: BTreeMap<u64, Result<String, String>>,
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
}

impl Prepared {
    /// A checkpoint's bytes, read back after the GPU ran its list.
    pub fn set_checkpoint(&mut self, id: u64, content: Result<String, String>) {
        self.checkpoints.insert(id, content);
    }
    /// A resource's contents before its first use.
    pub fn set_first_use(&mut self, resource: u64, first_use: FirstUse) {
        self.first_use.insert(resource, first_use);
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
                    Ok((key, subs)) => {
                        // Exact: d12_first_use says which submission it was copied before.
                        resource.content = Some(key.clone());
                        resource.content_source = Some("first-use");
                        hashes.clone_from(subs);
                    }
                    Err(why) => {
                        resource.content_note = Some(why.clone());
                        *missing.entry(why.clone()).or_default() += 1;
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
                for (i, sub) in subs.into_iter().enumerate() {
                    subresources.push(db::Subresource {
                        resource: resource.id,
                        sub,
                        hash: hashes.get(i).filter(|h| !h.is_empty()).cloned(),
                    });
                }
            }
        }
        capture.subresources.extend(subresources);
        let mut uncopied = BTreeMap::<String, u64>::new();
        for (id, target) in &self.projection.targets {
            match self.checkpoints.get(id) {
                Some(Ok(key)) => capture.targets.push(db::Target {
                    blob: Some(key.clone()),
                    ..target.clone()
                }),
                Some(Err(why)) => *uncopied.entry(why.clone()).or_default() += 1,
                None => *uncopied.entry("copy not read back".into()).or_default() += 1,
            }
        }
        let mut skipped = 0;
        for (why, count) in self.projection.uncopied.iter().chain(&uncopied) {
            capture.gap("d3d12-targets", why, *count);
            skipped += count;
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
        capture.meta(
            "pass_ends",
            serde_json::to_value(self.projection.pass_ends).unwrap_or_default(),
        );
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
    /// Pass ends no copy was made for, by reason.
    uncopied: BTreeMap<String, u64>,
    /// The last draw of the list execution being projected that each (target, view) was bound to.
    drawn: BTreeMap<(String, u64), u64>,
    /// How the frame's pass ends went, for the capture's meta: where a frame's targets go missing.
    pass_ends: PassEnds,
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
            "SetPipelineState" => self.pso = a[0].as_u64().unwrap_or(0),
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
                    width: n("width"),
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
            resource.used = resource.written || reads.contains(&resource.id);
        }
        projection.objects.clear();
        Prepared {
            frame: self,
            projection,
            first_use: BTreeMap::new(),
            checkpoints: BTreeMap::new(),
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
            drawn: BTreeMap::new(),
            pass_ends: PassEnds::default(),
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
                *self.uncopied.entry(why).or_default() += 1;
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

    /// Projects one submission, after the frame's earlier ones. Returns the resources its events read or write, each
    /// once, in the order they are first touched.
    pub fn submit(&mut self, capture: &mut db::Capture, submission: &Submission) -> Vec<u64> {
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
        for (index, command) in list.commands.iter().enumerate() {
            self.checkpoints(capture, list, index, first);
            pipeline.apply(command);
            let seq = capture.events.len() as u64 + 1;
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
                capture.draws.push(draw);
                if kind == "draw" {
                    last_draw = Some(n);
                }
                bindings::project(
                    capture,
                    n,
                    kind == "dispatch",
                    pipeline,
                    &self.objects,
                    &self.descriptors,
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
                .entry("list recorded before the capture was armed: its pass ends were not copied".into())
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

/// A resource's subresources in the canonical layout (see [`crate::layout`]), from its description. D3D12's planar
/// depth-stencil formats are laid out as the Direct3D 11 format of the same name, one texel holding both planes.
pub fn layout(d: &Value) -> Option<Vec<crate::layout::Sub>> {
    let n = |key: &str| d[key].as_u64().unwrap_or(0) as u32;
    if d["dimension"] == 1 {
        return Some(crate::layout::buffer(n("width")));
    }
    let (depth, array) = extent(d);
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
"#;

fn integer(value: u64) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
