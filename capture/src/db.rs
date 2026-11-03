//! A capture's database, `capture.db`: SQLite, one file per captured frame. Large contents (resource data, targets,
//! map and update data) are blobs in the blob store, named by their key; the database holds everything else.
//!
//! The tables are not tied to Direct3D 11: a binding is a stage, a kind (`cb`, `srv`, `uav`, `sampler`, `rtv`,
//! `dsv`, `vb`, `ib`, `so`), a slot and a register space, which is how Direct3D 12's root signatures name them too.
//! Where an API's own description of an object is needed to recreate it, it is kept as the API's struct (`desc`)
//! with a JSON rendering beside it.

use std::path::Path;

use rusqlite::{Connection, params};

/// Bumped when a table changes meaning.
pub const SCHEMA_VERSION: u32 = 1;

pub const SCHEMA: &str = r#"
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- The modules loaded in the process at the capture, for reading the stacks.
CREATE TABLE modules(id INTEGER PRIMARY KEY, path TEXT NOT NULL, base INTEGER NOT NULL, size INTEGER NOT NULL);
-- Call stacks: return addresses (u64, little-endian), innermost first, locum's own frames removed.
CREATE TABLE stacks(id INTEGER PRIMARY KEY, frames BLOB NOT NULL);
-- Every resource alive at the capture. Those the frame used have their contents before the frame (content).
CREATE TABLE resources(
  id INTEGER PRIMARY KEY,
  kind TEXT NOT NULL,              -- buffer, texture1d, texture2d, texture3d
  width INTEGER, height INTEGER, depth INTEGER, mips INTEGER, array INTEGER, format INTEGER, samples INTEGER,
  bytes INTEGER NOT NULL,          -- size of the contents in the canonical layout
  stride INTEGER, usage INTEGER, bind INTEGER, cpu INTEGER, misc INTEGER,
  desc BLOB,                       -- the API's description struct
  created_frame INTEGER, created_stack INTEGER,
  upload TEXT,                     -- key of the data it was created with (canonical layout)
  upload_stored INTEGER NOT NULL DEFAULT 0,
  updated_frame INTEGER, updated_stack INTEGER, update_count INTEGER NOT NULL DEFAULT 0,
  used INTEGER NOT NULL DEFAULT 0, -- bound or written by the captured frame
  written INTEGER NOT NULL DEFAULT 0,
  content TEXT,                    -- key of its contents when the frame began
  content_source TEXT,             -- first-use (copied just before the timeline first touched it) | upload
  content_note TEXT                -- why content is missing, or how it may differ
);
CREATE TABLE subresources(
  resource INTEGER NOT NULL, sub INTEGER NOT NULL, mip INTEGER, slice INTEGER,
  width INTEGER, height INTEGER, depth INTEGER, row_bytes INTEGER, rows INTEGER,
  offset INTEGER NOT NULL, size INTEGER NOT NULL,
  hash TEXT,                       -- key of this subresource's bytes
  PRIMARY KEY(resource, sub)
);
CREATE TABLE views(id INTEGER PRIMARY KEY, kind TEXT NOT NULL, resource INTEGER, format INTEGER, dimension INTEGER,
  desc BLOB, json TEXT);
-- Pipeline state objects: rasterizer, blend, depth_stencil, sampler, input_layout (signature: key of the bytecode it
-- was created against).
CREATE TABLE states(id INTEGER PRIMARY KEY, kind TEXT NOT NULL, desc BLOB, json TEXT, signature TEXT);
CREATE TABLE shaders(hash TEXT PRIMARY KEY, stage TEXT NOT NULL, blob TEXT, size INTEGER);
-- Everything the frame did on the GPU's timeline, in order: draws and dispatches, and every call that changes a
-- resource (clears, copies, updates, maps, resolves, mips). data: key of the bytes an update or map wrote.
CREATE TABLE events(seq INTEGER PRIMARY KEY, kind TEXT NOT NULL, call TEXT NOT NULL, context INTEGER,
  deferred INTEGER, list INTEGER, draw INTEGER, args TEXT, data TEXT, stack INTEGER);
-- The resources each event read and wrote (a draw: what it had bound to read, and its targets and UAVs).
CREATE TABLE event_resources(event INTEGER NOT NULL, resource INTEGER NOT NULL, access TEXT NOT NULL);
CREATE TABLE draws(n INTEGER PRIMARY KEY, event INTEGER NOT NULL, call TEXT NOT NULL, args TEXT, pass INTEGER,
  vs TEXT, hs TEXT, ds TEXT, gs TEXT, ps TEXT, cs TEXT,
  topology INTEGER, layout INTEGER, rasterizer INTEGER, blend INTEGER, depth_stencil INTEGER,
  blend_factor TEXT, sample_mask INTEGER, stencil_ref INTEGER, predicate INTEGER, predicate_value INTEGER);
-- What each draw had bound. cb: offset and size of the bound range in bytes, content the bytes the GPU read.
-- vb: offset, stride. ib: offset, format. so: offset.
CREATE TABLE bindings(draw INTEGER NOT NULL, stage TEXT NOT NULL, kind TEXT NOT NULL, slot INTEGER NOT NULL,
  space INTEGER NOT NULL DEFAULT 0, view INTEGER, resource INTEGER, state INTEGER, offset INTEGER, size INTEGER,
  stride INTEGER, format INTEGER, content TEXT);
CREATE TABLE cb_data(hash TEXT PRIMARY KEY, bytes BLOB NOT NULL);
CREATE TABLE viewports(draw INTEGER NOT NULL, i INTEGER NOT NULL, x REAL, y REAL, width REAL, height REAL,
  min_depth REAL, max_depth REAL);
CREATE TABLE scissors(draw INTEGER NOT NULL, i INTEGER NOT NULL, left INTEGER, top INTEGER, right INTEGER,
  bottom INTEGER);
-- Passes: runs of draws with the same targets on the GPU's timeline.
CREATE TABLE passes(n INTEGER PRIMARY KEY, first_draw INTEGER NOT NULL, last_draw INTEGER NOT NULL);
-- Targets read back where a pass ends, after its last draw.
CREATE TABLE targets(draw INTEGER NOT NULL, target TEXT NOT NULL, view INTEGER, resource INTEGER, width INTEGER,
  height INTEGER, format INTEGER, sub INTEGER, blob TEXT, PRIMARY KEY(draw, target));
-- What the capture could not record, for `locum-cli info`.
CREATE TABLE gaps(what TEXT NOT NULL, detail TEXT, count INTEGER NOT NULL);
CREATE INDEX bindings_draw ON bindings(draw);
CREATE INDEX bindings_resource ON bindings(resource);
CREATE INDEX bindings_content ON bindings(content);
CREATE INDEX draws_vs ON draws(vs);
CREATE INDEX draws_ps ON draws(ps);
CREATE INDEX events_kind ON events(kind);
CREATE INDEX event_resources_resource ON event_resources(resource);
CREATE INDEX resources_upload ON resources(upload);
CREATE INDEX resources_content ON resources(content);
CREATE INDEX subresources_hash ON subresources(hash);
"#;

#[derive(Clone, Debug, Default)]
pub struct Module {
    pub id: u64,
    pub path: String,
    pub base: u64,
    pub size: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Resource {
    pub id: u64,
    pub kind: &'static str,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mips: u32,
    pub array: u32,
    pub format: u32,
    pub samples: u32,
    pub bytes: u64,
    pub stride: u32,
    pub usage: u32,
    pub bind: u32,
    pub cpu: u32,
    pub misc: u32,
    pub desc: Vec<u8>,
    pub created_frame: Option<u64>,
    pub created_stack: Option<u64>,
    pub upload: Option<String>,
    pub upload_stored: bool,
    pub updated_frame: Option<u64>,
    pub updated_stack: Option<u64>,
    pub update_count: u64,
    pub used: bool,
    pub written: bool,
    pub content: Option<String>,
    pub content_source: Option<&'static str>,
    pub content_note: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Subresource {
    pub resource: u64,
    pub sub: crate::layout::Sub,
    pub hash: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct View {
    pub id: u64,
    pub kind: &'static str,
    pub resource: u64,
    pub format: u32,
    pub dimension: u32,
    pub desc: Vec<u8>,
    pub json: String,
}

#[derive(Clone, Debug, Default)]
pub struct State {
    pub id: u64,
    pub kind: &'static str,
    pub desc: Vec<u8>,
    pub json: String,
    pub signature: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Shader {
    pub hash: String,
    pub stage: &'static str,
    pub blob: Option<String>,
    pub size: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Event {
    pub seq: u64,
    pub kind: &'static str,
    pub call: &'static str,
    pub context: u32,
    pub deferred: bool,
    pub list: Option<u64>,
    pub draw: Option<u64>,
    pub args: String,
    pub data: Option<String>,
    pub stack: Option<u64>,
    pub reads: Vec<u64>,
    pub writes: Vec<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct Draw {
    pub n: u64,
    pub event: u64,
    pub call: &'static str,
    pub args: String,
    pub pass: Option<u64>,
    /// vs, hs, ds, gs, ps, cs
    pub shaders: [Option<String>; 6],
    pub topology: i32,
    pub layout: Option<u64>,
    pub rasterizer: Option<u64>,
    pub blend: Option<u64>,
    pub depth_stencil: Option<u64>,
    pub blend_factor: Option<[f32; 4]>,
    pub sample_mask: Option<u32>,
    pub stencil_ref: Option<u32>,
    pub predicate: Option<u64>,
    pub predicate_value: Option<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct Binding {
    pub draw: u64,
    pub stage: &'static str,
    pub kind: &'static str,
    pub slot: u32,
    pub space: u32,
    pub view: Option<u64>,
    pub resource: Option<u64>,
    pub state: Option<u64>,
    pub offset: Option<u64>,
    pub size: Option<u64>,
    pub stride: Option<u32>,
    pub format: Option<u32>,
    pub content: Option<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Viewport {
    pub draw: u64,
    pub i: u32,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub min_depth: f32,
    pub max_depth: f32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Scissor {
    pub draw: u64,
    pub i: u32,
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Pass {
    pub n: u64,
    pub first_draw: u64,
    pub last_draw: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Target {
    pub draw: u64,
    pub target: String,
    pub view: u64,
    pub resource: u64,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub sub: u32,
    pub blob: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Gap {
    pub what: String,
    pub detail: String,
    pub count: u64,
}

/// Everything a capture puts in its database.
#[derive(Default)]
pub struct Capture {
    pub meta: Vec<(String, String)>,
    pub modules: Vec<Module>,
    pub stacks: Vec<(u64, Vec<u64>)>,
    pub resources: Vec<Resource>,
    pub subresources: Vec<Subresource>,
    pub views: Vec<View>,
    pub states: Vec<State>,
    pub shaders: Vec<Shader>,
    pub events: Vec<Event>,
    pub draws: Vec<Draw>,
    pub bindings: Vec<Binding>,
    pub cb_data: Vec<(String, Vec<u8>)>,
    pub viewports: Vec<Viewport>,
    pub scissors: Vec<Scissor>,
    pub passes: Vec<Pass>,
    pub targets: Vec<Target>,
    pub gaps: Vec<Gap>,
}

impl Capture {
    pub fn meta(&mut self, key: &str, value: impl ToString) {
        self.meta.push((key.to_string(), value.to_string()));
    }

    pub fn gap(&mut self, what: &str, detail: impl Into<String>, count: u64) {
        self.gaps.push(Gap {
            what: what.into(),
            detail: detail.into(),
            count,
        });
    }
}

const STAGES: [&str; 6] = ["vs", "hs", "ds", "gs", "ps", "cs"];

fn opt_i64(v: Option<u64>) -> Option<i64> {
    v.map(|v| v as i64)
}

/// Writes `capture` to a new database at `path` (to a temporary file first, renamed into place when complete).
pub fn write(path: &Path, capture: &Capture) -> rusqlite::Result<()> {
    let temp = path.with_extension("db.part");
    let _ = std::fs::remove_file(&temp);
    let mut db = Connection::open(&temp)?;
    db.execute_batch("PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA page_size=65536;")?;
    db.execute_batch(SCHEMA)?;
    let tx = db.transaction()?;
    {
        let mut s = tx.prepare("INSERT INTO meta VALUES(?1, ?2)")?;
        s.execute(params!["schema_version", SCHEMA_VERSION.to_string()])?;
        for (k, v) in &capture.meta {
            s.execute(params![k, v])?;
        }
        let mut s = tx.prepare("INSERT INTO modules VALUES(?1, ?2, ?3, ?4)")?;
        for m in &capture.modules {
            s.execute(params![m.id as i64, m.path, m.base as i64, m.size as i64])?;
        }
        let mut s = tx.prepare("INSERT INTO stacks VALUES(?1, ?2)")?;
        for (id, frames) in &capture.stacks {
            let bytes: Vec<u8> = frames.iter().flat_map(|f| f.to_le_bytes()).collect();
            s.execute(params![*id as i64, bytes])?;
        }
        let mut s = tx.prepare(
            "INSERT INTO resources VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,\
             ?22,?23,?24,?25,?26,?27,?28)",
        )?;
        for r in &capture.resources {
            s.execute(params![
                r.id as i64,
                r.kind,
                r.width,
                r.height,
                r.depth,
                r.mips,
                r.array,
                r.format,
                r.samples,
                r.bytes as i64,
                r.stride,
                r.usage,
                r.bind,
                r.cpu,
                r.misc,
                r.desc,
                opt_i64(r.created_frame),
                opt_i64(r.created_stack),
                r.upload,
                r.upload_stored,
                opt_i64(r.updated_frame),
                opt_i64(r.updated_stack),
                r.update_count as i64,
                r.used,
                r.written,
                r.content,
                r.content_source,
                r.content_note,
            ])?;
        }
        let mut s = tx.prepare("INSERT INTO subresources VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)")?;
        for r in &capture.subresources {
            let u = &r.sub;
            s.execute(params![
                r.resource as i64,
                u.index,
                u.mip,
                u.slice,
                u.width,
                u.height,
                u.depth,
                u.row_bytes,
                u.rows,
                u.offset as i64,
                u.size as i64,
                r.hash
            ])?;
        }
        let mut s = tx.prepare("INSERT INTO views VALUES(?1,?2,?3,?4,?5,?6,?7)")?;
        for v in &capture.views {
            s.execute(params![
                v.id as i64,
                v.kind,
                v.resource as i64,
                v.format,
                v.dimension,
                v.desc,
                v.json
            ])?;
        }
        let mut s = tx.prepare("INSERT INTO states VALUES(?1,?2,?3,?4,?5)")?;
        for v in &capture.states {
            s.execute(params![v.id as i64, v.kind, v.desc, v.json, v.signature])?;
        }
        let mut s = tx.prepare("INSERT OR IGNORE INTO shaders VALUES(?1,?2,?3,?4)")?;
        for v in &capture.shaders {
            s.execute(params![v.hash, v.stage, v.blob, v.size as i64])?;
        }
        let mut s = tx.prepare("INSERT INTO events VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)")?;
        for e in &capture.events {
            s.execute(params![
                e.seq as i64,
                e.kind,
                e.call,
                e.context,
                e.deferred,
                opt_i64(e.list),
                opt_i64(e.draw),
                e.args,
                e.data,
                opt_i64(e.stack)
            ])?;
        }
        let mut s = tx.prepare("INSERT INTO event_resources VALUES(?1,?2,?3)")?;
        for e in &capture.events {
            for (ids, access) in [(&e.reads, "read"), (&e.writes, "write")] {
                for &id in ids {
                    s.execute(params![e.seq as i64, id as i64, access])?;
                }
            }
        }
        let mut s = tx.prepare(
            "INSERT INTO draws VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21)",
        )?;
        for d in &capture.draws {
            let [vs, hs, ds, gs, ps, cs] = &d.shaders;
            s.execute(params![
                d.n as i64,
                d.event as i64,
                d.call,
                d.args,
                opt_i64(d.pass),
                vs,
                hs,
                ds,
                gs,
                ps,
                cs,
                d.topology,
                opt_i64(d.layout),
                opt_i64(d.rasterizer),
                opt_i64(d.blend),
                opt_i64(d.depth_stencil),
                d.blend_factor
                    .map(|f| format!("[{},{},{},{}]", f[0], f[1], f[2], f[3])),
                d.sample_mask,
                d.stencil_ref,
                opt_i64(d.predicate),
                d.predicate_value
            ])?;
        }
        let mut s = tx.prepare("INSERT INTO bindings VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)")?;
        for b in &capture.bindings {
            s.execute(params![
                b.draw as i64,
                b.stage,
                b.kind,
                b.slot,
                b.space,
                opt_i64(b.view),
                opt_i64(b.resource),
                opt_i64(b.state),
                opt_i64(b.offset),
                opt_i64(b.size),
                b.stride,
                b.format,
                b.content
            ])?;
        }
        let mut s = tx.prepare("INSERT OR IGNORE INTO cb_data VALUES(?1,?2)")?;
        for (h, bytes) in &capture.cb_data {
            s.execute(params![h, bytes])?;
        }
        let mut s = tx.prepare("INSERT INTO viewports VALUES(?1,?2,?3,?4,?5,?6,?7,?8)")?;
        for v in &capture.viewports {
            s.execute(params![
                v.draw as i64,
                v.i,
                v.x,
                v.y,
                v.width,
                v.height,
                v.min_depth,
                v.max_depth
            ])?;
        }
        let mut s = tx.prepare("INSERT INTO scissors VALUES(?1,?2,?3,?4,?5,?6)")?;
        for v in &capture.scissors {
            s.execute(params![v.draw as i64, v.i, v.left, v.top, v.right, v.bottom])?;
        }
        let mut s = tx.prepare("INSERT INTO passes VALUES(?1,?2,?3)")?;
        for p in &capture.passes {
            s.execute(params![p.n as i64, p.first_draw as i64, p.last_draw as i64])?;
        }
        let mut s = tx.prepare("INSERT OR REPLACE INTO targets VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)")?;
        for t in &capture.targets {
            s.execute(params![
                t.draw as i64,
                t.target,
                t.view as i64,
                t.resource as i64,
                t.width,
                t.height,
                t.format,
                t.sub,
                t.blob
            ])?;
        }
        let mut s = tx.prepare("INSERT INTO gaps VALUES(?1,?2,?3)")?;
        for g in &capture.gaps {
            s.execute(params![g.what, g.detail, g.count as i64])?;
        }
    }
    tx.commit()?;
    drop(db);
    let _ = std::fs::remove_file(path);
    std::fs::rename(&temp, path).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    Ok(())
}

/// The stage names in the order of [`Draw::shaders`].
pub fn stage_names() -> [&'static str; 6] {
    STAGES
}
