//! Owned, portable D3D12 command history. A list recording and an execution of that
//! recording have different identities; resetting a list cannot change an earlier execution.
use std::collections::BTreeMap;
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
}

impl Prepared {
    /// Populate CPU upload contents and per-draw constant bytes after queued blobs are durable.
    pub fn contents(&self, capture: &mut db::Capture, blobs: &crate::blobs::Blobs) {
        let uploads: BTreeMap<_, _> = self
            .frame
            .submissions
            .iter()
            .flat_map(|s| s.uploads.iter().map(move |(r, v)| ((s.id, *r), v)))
            .collect();
        let draws: BTreeMap<_, _> = self
            .projection
            .draw_states
            .iter()
            .map(|(draw, submission, _)| (*draw, *submission))
            .collect();
        let mut gaps = Vec::new();
        for binding in &mut capture.bindings {
            if binding.kind != "cb" || binding.content.is_some() {
                continue;
            }
            let Some(resource) = binding.resource else {
                continue;
            };
            let Some(submission) = draws.get(&binding.draw) else {
                continue;
            };
            let Some(content) = uploads.get(&(*submission, resource)) else {
                continue;
            };
            let Some(key) = content["blob"].as_str() else {
                continue;
            };
            match blobs.get(key) {
                Ok(bytes) => {
                    let start = binding.offset.unwrap_or(0) as usize;
                    let end = binding
                        .size
                        .and_then(|s| (s as usize).checked_add(start))
                        .unwrap_or(bytes.len());
                    if let Some(bytes) = bytes.get(start..end) {
                        let key = crate::hash::key(bytes);
                        capture.cb_data.push((key.clone(), bytes.to_vec()));
                        binding.content = Some(key);
                    } else {
                        gaps.push(format!(
                            "draw {} resource {resource}: CB range exceeds upload bytes",
                            binding.draw
                        ));
                    }
                }
                Err(e) => gaps.push(format!(
                    "draw {} resource {resource}: upload {key}: {e}",
                    binding.draw
                )),
            }
        }
        for resource in &mut capture.resources {
            if let Some((_, content)) = uploads.iter().find(|((_, id), _)| *id == resource.id)
                && let Some(key) = content["blob"].as_str()
                && blobs.has(key)
            {
                resource.content = Some(key.into());
                resource.content_source = Some("cpu-before-submit");
                resource.content_note=Some("CPU upload bytes before the first captured submission; host writes must precede submission".into());
            }
        }
        for gap in gaps {
            capture.gap("d3d12-upload-content", gap, 1);
        }
        capture.meta("counts", serde_json::json!({
            "events":capture.events.len(), "draws":capture.draws.len(),
            "resources_live":capture.resources.len(),
            "resources_used":capture.resources.iter().filter(|r|r.used).count(),
            "resources_written":capture.resources.iter().filter(|r|r.written).count(),
            "contents":capture.resources.iter().filter(|r|r.content.is_some()).count(),
            "content_bytes":capture.resources.iter().filter(|r|r.content.is_some()).map(|r|r.bytes).sum::<u64>(),
            "submissions":self.frame.submissions.len(),
            "executions":self.projection.executions.len()
        }));
    }
    pub fn write(&self, tx: &Transaction<'_>) -> rusqlite::Result<()> {
        self.frame.write(tx, &self.projection)
    }
}

struct Execution {
    id: u64,
    submission: u64,
    list: Arc<List>,
    ordinal: usize,
    parent: Option<u64>,
}

struct Projection {
    next_execution: u64,
    executions: Vec<Execution>,
    locations: Vec<(u64, u64, usize, u64)>,
    objects: BTreeMap<u64, Value>,
    draw_states: Vec<(u64, u64, Pipeline)>,
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
    fn project(&self, capture: &mut db::Capture) -> Projection {
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
                let depth = if dimension == 4 {
                    n("depth_or_array_size")
                } else {
                    1
                };
                let array = if dimension == 4 {
                    1
                } else {
                    n("depth_or_array_size")
                };
                let layout = if dimension == 1 {
                    Some(crate::layout::buffer(n("width")))
                } else {
                    crate::layout::texture(n("format"), n("width"), n("height"), depth, n("mips"), array)
                };
                let bytes = layout
                    .as_ref()
                    .and_then(|s| s.last())
                    .map_or(0, |s| s.offset + s.size);
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
                    content_note: Some("GPU initial/checkpoint readbacks pending".into()),
                    ..db::Resource::default()
                });
                for sub in layout.unwrap_or_default() {
                    capture.subresources.push(db::Subresource {
                        resource: object.id,
                        sub,
                        hash: None,
                    });
                }
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
        let mut p = Projection {
            next_execution: 1,
            executions: Vec::new(),
            locations: Vec::new(),
            objects: self
                .objects
                .iter()
                .map(|o| (o.id, o.description.clone()))
                .collect(),
            draw_states: Vec::new(),
        };
        for submission in &self.submissions {
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
                p.execute(
                    capture,
                    submission,
                    list,
                    (ordinal, None, 0),
                    &mut Pipeline::default(),
                );
            }
        }
        p
    }

    /// Appends the shared events/draws projection, leaving D3D12 identities authoritative.
    pub fn prepare(self, capture: &mut db::Capture) -> Prepared {
        let mut projection = self.project(capture);
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
        }
    }

    fn write(&self, tx: &Transaction<'_>, p: &Projection) -> rusqlite::Result<()> {
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
        for s in &self.submissions {
            for (resource, content) in &s.uploads {
                tx.execute(
                    "INSERT INTO d12_upload_versions VALUES(?1,?2,?3)",
                    params![integer(s.id)?, integer(*resource)?, content.to_string()],
                )?;
            }
        }
        Ok(())
    }
}

impl Projection {
    fn execute(
        &mut self,
        capture: &mut db::Capture,
        submission: &Submission,
        list: &Arc<List>,
        location: (usize, Option<u64>, usize),
        pipeline: &mut Pipeline,
    ) {
        let (ordinal, parent, depth) = location;
        if depth > 32 {
            capture.gap(
                "d3d12-bundle-cycle",
                format!("list {} generation {}", list.id, list.generation),
                1,
            );
            return;
        }
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
            pipeline.apply(command);
            let seq = capture.events.len() as u64 + 1;
            let kind = match command.call {
                "DrawInstanced" | "DrawIndexedInstanced" => "draw",
                "Dispatch" => "dispatch",
                "ExecuteIndirect" => "indirect",
                "CopyBufferRegion" | "CopyTextureRegion" | "CopyResource" | "CopyTiles" => "copy",
                "ResourceBarrier" => "barrier",
                "ExecuteBundle" => "bundle",
                _ => "state",
            };
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
                bindings::project(
                    capture,
                    n,
                    kind == "dispatch",
                    pipeline,
                    &self.objects,
                    submission,
                );
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
            let a = &command.args;
            let (read, write) = match command.call {
                "CopyBufferRegion" => (a[2].as_u64(), a[0].as_u64()),
                "CopyResource" => (a[1].as_u64(), a[0].as_u64()),
                "CopyTextureRegion" => (a[4]["resource"].as_u64(), a[0]["resource"].as_u64()),
                "ResolveSubresource" => (a[2].as_u64(), a[0].as_u64()),
                "ResolveQueryData" => (None, a[4].as_u64()),
                "ClearRenderTargetView" | "ClearDepthStencilView" => {
                    (None, a[0]["definition"]["resource"].as_u64())
                }
                "ClearUnorderedAccessViewUint" | "ClearUnorderedAccessViewFloat" => (None, a[2].as_u64()),
                _ => (None, None),
            };
            event.reads.extend(read.filter(|r| *r != 0));
            event.writes.extend(write.filter(|r| *r != 0));
            if let Some(draw) = draw {
                let event = capture.events.last_mut().unwrap();
                for binding in capture.bindings.iter().filter(|b| b.draw == draw) {
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
            }
            self.locations.push((seq, execution, index, submission.queue));
            if let Some(diagnostic) = &command.diagnostic {
                capture.gap(
                    "d3d12-command",
                    format!("event {seq}, {}: {diagnostic}", command.call),
                    1,
                );
            }
            if let Some(bundle) = &command.bundle {
                self.execute(
                    capture,
                    submission,
                    bundle,
                    (index, Some(execution), depth + 1),
                    pipeline,
                );
            }
        }
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
CREATE TABLE d12_upload_versions(submission INTEGER NOT NULL,resource INTEGER NOT NULL,content_json TEXT NOT NULL,
 PRIMARY KEY(submission,resource));
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
