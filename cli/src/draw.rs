//! `draw` and `cb`: one draw or dispatch, everything it had bound and its constants by name, as text or as JSON
//! (`--json`; with `all`, one object per line for every draw, so a census over a frame is one process).

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

use locum_capture::format;
use serde::Serialize;
use serde_json::Value;

use crate::describe::{self, Reflection, STAGE_ORDER, register_name, shape, short, value};
use crate::draws::shader_of;
use crate::join::Vb;
use crate::open::Capture;
use crate::out;
use crate::states;

/// A binding row as the capture has it.
pub(crate) struct Row {
    pub stage: String,
    pub kind: String,
    pub slot: u32,
    pub space: u32,
    pub view: Option<i64>,
    pub resource: Option<i64>,
    pub state: Option<i64>,
    pub offset: Option<i64>,
    pub size: Option<i64>,
    pub stride: Option<i64>,
    pub format: Option<i64>,
    pub content: Option<String>,
}

pub(crate) fn rows(c: &Capture, draw: i64) -> Vec<Row> {
    c.rows(
        &format!(
            "SELECT stage, kind, slot, space, view, resource, state, offset, size, stride, format, content \
             FROM bindings WHERE draw = ?1 ORDER BY {STAGE_ORDER}, kind, slot"
        ),
        [draw],
        |r| {
            Ok(Row {
                stage: r.get(0)?,
                kind: r.get(1)?,
                slot: r.get::<_, i64>(2)? as u32,
                space: r.get::<_, i64>(3)? as u32,
                view: r.get(4)?,
                resource: r.get(5)?,
                state: r.get(6)?,
                offset: r.get(7)?,
                size: r.get(8)?,
                stride: r.get(9)?,
                format: r.get(10)?,
                content: r.get(11)?,
            })
        },
    )
}

/// A D3D12 draw's place in the queue's work.
#[derive(Serialize)]
pub struct Queued {
    pub queue: i64,
    pub execution: i64,
    pub list: i64,
    pub generation: i64,
    pub command: i64,
}

/// One draw or dispatch, as `draw` shows it.
#[derive(Serialize)]
pub struct Draw {
    pub draw: i64,
    pub event: i64,
    pub call: String,
    /// The call's arguments as the API orders them.
    pub args: Value,
    /// The same, by name (`count=714 start=885904 base=0`).
    pub named_args: String,
    pub pass: Option<i64>,
    pub deferred: bool,
    /// The command list it was recorded in, for a deferred context's.
    pub list: Option<i64>,
    pub context: i64,
    /// Where a D3D12 capture's draw ran: its queue, the execution, and the command list and command within it.
    pub queue: Option<Queued>,
    /// The call stack, innermost first.
    pub stack: Vec<String>,
    /// Shader hash by stage (vs, hs, ds, gs, ps, cs).
    pub shaders: BTreeMap<&'static str, String>,
    /// The debug name of each stage's program that has one (`rdef::debug_name`).
    pub shader_names: BTreeMap<&'static str, String>,
    pub topology: Option<i64>,
    pub topology_name: Option<String>,
    /// Input layout, rasterizer, blend and depth-stencil state objects.
    pub states: Vec<State>,
    pub blend_factor: Option<Value>,
    pub sample_mask: Option<u32>,
    pub stencil_ref: Option<i64>,
    pub predicate: Option<i64>,
    pub predicate_value: Option<i64>,
    pub viewports: Vec<Viewport>,
    pub scissors: Vec<Scissor>,
    pub bindings: Vec<Binding>,
    /// Bound registers left out because the shaders' reflection does not list them (`--all` keeps them): a D3D12
    /// root table binds every descriptor of its ranges, whatever the program reads.
    pub unused: usize,
    /// Bound vertex buffer slots left out because no element of the input layout reads them (`--all` keeps them): a
    /// game can leave buffers bound in slots a later draw's layout does not use.
    pub unread_slots: usize,
    /// Registers bound to more than one descriptor, which no valid root signature does: captures whose D3D12 bindings
    /// a locum before 2026-11-04 projected let a table range reach descriptors of another type.
    pub doubled: usize,
    pub constants: Vec<Constants>,
    /// Targets read back after it (the last draw of a pass).
    pub targets: Vec<Target>,
}

#[derive(Serialize)]
pub struct State {
    /// input_layout, rasterizer, blend, depth_stencil or sampler.
    pub kind: String,
    pub id: i64,
    /// Its description with Direct3D's enums named.
    pub text: String,
    /// Its description as the capture keeps it (the API's numbers).
    pub desc: Value,
}

#[derive(Serialize)]
pub struct Viewport {
    pub i: i64,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub min_depth: f64,
    pub max_depth: f64,
}

#[derive(Serialize)]
pub struct Scissor {
    pub i: i64,
    pub left: i64,
    pub top: i64,
    pub right: i64,
    pub bottom: i64,
}

#[derive(Serialize)]
pub struct View {
    pub id: i64,
    pub dimension: Option<String>,
    pub format: Option<u32>,
    pub format_name: Option<&'static str>,
    pub range: Value,
}

#[derive(Serialize)]
pub struct Binding {
    pub stage: String,
    /// cb, srv, uav, sampler, rtv, dsv, vb, ib or so.
    pub kind: String,
    pub slot: u32,
    pub space: u32,
    /// Its name in the shader's reflection.
    pub name: Option<String>,
    /// Whether the shader uses the slot: false for one its reflection does not list, null when unknown.
    pub used: Option<bool>,
    pub view: Option<View>,
    pub state: Option<State>,
    pub resource: Option<i64>,
    /// The resource's shape (`texture2d 1920x1080 R11G11B10_FLOAT`).
    pub shape: Option<String>,
    /// The game file it was loaded from, when an index names it.
    pub file: Option<String>,
    /// Every file an index holds with the same bytes, `file`'s first (at most 17): which of them it was the capture
    /// cannot say.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<String>,
    /// What the reader that listed `file` said of it (a key list's fourth column: a mesh's material, say).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub about: Option<Value>,
    pub offset: Option<i64>,
    pub size: Option<i64>,
    pub stride: Option<i64>,
    pub format: Option<i64>,
    pub format_name: Option<&'static str>,
    /// For a constant buffer, the key of the bytes the GPU read; otherwise the key of the resource's contents as
    /// the frame began.
    pub contents: Option<String>,
    /// Where those contents came from (first-use, upload, ...).
    pub contents_source: Option<String>,
    /// Why the contents are missing, or how they may differ.
    pub contents_note: Option<String>,
    /// The last event before this draw that wrote the resource: when set, `contents` is not what this draw saw
    /// (`export --at` gives that).
    pub written_before: Option<i64>,
}

#[derive(Serialize)]
pub struct Constants {
    pub stage: String,
    pub slot: u32,
    pub space: u32,
    /// The buffer's name in the shader's reflection.
    pub name: Option<String>,
    /// The bytes the program declares the buffer to hold, when its reflection says: a draw can bind a larger range.
    pub declared: Option<u32>,
    pub buffer: Option<i64>,
    /// The key of the bytes the GPU read, and their length.
    pub key: Option<String>,
    pub bytes: usize,
    /// The 4-byte words shown: those of registers the shader uses, or all of them.
    pub words: Vec<Word>,
    /// Registers left out because the shader does not use them.
    pub skipped: usize,
    /// Why there are no contents, as far as the capture says.
    pub why: Option<String>,
    /// The program's last variable, an array, run on over the bound range (`--extend`); or, without `--extend`, how
    /// far it would run.
    pub extended: Option<Extended>,
    /// The program whose declaration names the words, when not the draw's own (`cb --as`).
    pub layout_from: Option<String>,
    /// Where the bytes came from when the capture recorded none for the binding (`unrecorded`).
    pub source: Option<String>,
}

/// How a constant buffer's words are found and named beyond what the capture recorded for the binding.
#[derive(Default)]
pub struct Lookup {
    /// The registers past the declared size as further elements of the buffer's last array.
    pub extend: bool,
    /// Another program's declaration of a buffer, and that program: for a buffer the draw's programs do not declare.
    pub layout: Option<(locum_capture::rdef::ConstantBuffer, String)>,
    /// Buffers rebuilt by replay for bindings the capture recorded no bytes for, by the buffer and the last event
    /// before the draw that wrote it (every draw until its next write sees the same bytes).
    replayed: RefCell<HashMap<(i64, i64), Replayed>>,
}

/// A buffer's bytes rebuilt by replay and where they came from, or why they could not be.
type Replayed = Result<(Vec<u8>, String), String>;

impl Lookup {
    pub fn new(extend: bool, layout: Option<(locum_capture::rdef::ConstantBuffer, String)>) -> Lookup {
        Lookup {
            extend,
            layout,
            replayed: RefCell::default(),
        }
    }
}

/// The bytes of a constant-buffer binding the capture recorded none for (Direct3D 12 captures from before each
/// draw's constants were copied, where the frame wrote the buffer on the GPU), from the buffer itself: its contents
/// as the frame began when nothing wrote it before the draw, else rebuilt by replaying what did, as `export --at`.
/// With where they came from.
fn unrecorded(c: &Capture, b: &Row, draw: i64, lookup: &Lookup) -> Result<(Vec<u8>, String), String> {
    let r = b.resource.ok_or("no buffer is bound")?;
    let event: i64 = c
        .row("SELECT event FROM draws WHERE n = ?1", [draw], |r| r.get(0))
        .ok_or_else(|| format!("no draw {draw}"))?;
    let last: Option<i64> = c
        .row(
            "SELECT max(event) FROM event_resources WHERE resource = ?1 AND access = 'write' AND event < ?2",
            [r, event],
            |r| r.get(0),
        )
        .flatten();
    let (bytes, source) = match last {
        None => {
            let key: String = c
                .row("SELECT content FROM resources WHERE id = ?1", [r], |r| {
                    r.get::<_, Option<String>>(0)
                })
                .flatten()
                .ok_or_else(|| format!("#{r} has no contents in the capture either"))?;
            let caveat = describe::later_upload(c, r, event)
                .map(|w| format!("; {w}"))
                .unwrap_or_default();
            (
                c.blob(&key)?,
                format!("#{r}'s contents as the frame began ({}){caveat}", short(&key)),
            )
        }
        Some(w) => lookup
            .replayed
            .borrow_mut()
            .entry((r, w))
            .or_insert_with(|| {
                crate::replay::contents_at(c, r, event, false).map(|(bytes, events, warnings)| {
                    let warned = if warnings.is_empty() {
                        String::new()
                    } else {
                        format!("; {}", warnings.join("; "))
                    };
                    (
                        bytes,
                        format!("#{r} replayed to its last write before the draw (event {w}), {events} events{warned}"),
                    )
                })
            })
            .clone()?,
    };
    let from = b.offset.unwrap_or(0).max(0) as usize;
    let to = b
        .size
        .map_or(bytes.len(), |s| from + s.max(0) as usize)
        .min(bytes.len());
    let bytes = bytes.get(from..to).filter(|s| !s.is_empty()).ok_or_else(|| {
        format!(
            "#{r} holds {} bytes, none from the binding's offset {from}",
            bytes.len()
        )
    })?;
    Ok((bytes.to_vec(), source))
}

/// An array a constant buffer ends with, named past its declared length over the bytes bound.
#[derive(Serialize)]
pub struct Extended {
    pub variable: String,
    pub declared: u16,
    pub elements: u16,
    /// Whether its words are named so (`--extend`), not only told.
    pub named: bool,
}

#[derive(Clone, Serialize)]
pub struct Word {
    pub offset: u32,
    /// The register and component (`c3.y`).
    pub register: String,
    /// The variable it belongs to, from the reflection (`cbTexScroll.uvScale.y`).
    pub name: Option<String>,
    /// As its type says (float when unknown).
    pub value: Value,
    pub hex: String,
    /// The value as text shows it (at most 12 characters).
    #[serde(skip)]
    pub text: String,
}

#[derive(Serialize)]
pub struct Target {
    pub target: String,
    pub blob: Option<String>,
    pub width: i64,
    pub height: i64,
    pub format: i64,
    pub format_name: &'static str,
}

/// A state object, its description and its enums named.
fn state(c: &Capture, id: i64) -> Option<State> {
    c.row("SELECT kind, json FROM states WHERE id = ?1", [id], |r| {
        let kind: String = r.get(0)?;
        let json: Option<String> = r.get(1)?;
        Ok(State {
            text: states::describe(&kind, json.as_deref()),
            desc: json
                .as_deref()
                .and_then(|j| serde_json::from_str(j).ok())
                .unwrap_or(Value::Null),
            kind,
            id,
        })
    })
}

/// A 4-byte constant as its type says: a bool, an int, a uint or a float (float when unknown; null when not finite).
pub fn number(bytes: [u8; 4], base: Option<u16>) -> Value {
    match base {
        Some(1) => Value::from(u32::from_le_bytes(bytes) != 0),
        Some(2) => Value::from(i32::from_le_bytes(bytes)),
        Some(19 | 20) => Value::from(u32::from_le_bytes(bytes)),
        _ => Value::from(f64::from(f32::from_le_bytes(bytes))),
    }
}

/// What a vertex or index binding's bytes were made from when no file is the whole buffer: for indices, the ones the
/// draw reads (a FLVER face set) before the bound range (its data block); for vertices, the bound range.
pub(crate) fn range_name(
    c: &Capture,
    r: i64,
    b: &Row,
    call: &str,
    args: &str,
    vbs: &[Vb],
) -> Option<crate::join::Match> {
    let (offset, size) = (u64::try_from(b.offset?).ok()?, u64::try_from(b.size?).ok()?);
    match b.kind.as_str() {
        "vb" => c.names.range(c, r, offset, size),
        "ib" => {
            let width: u64 = match b.format? {
                57 => 2,
                42 => 4,
                _ => return c.names.indices(c, r, offset, size, vbs),
            };
            let a: Vec<u64> = serde_json::from_str::<Vec<Value>>(args)
                .unwrap_or_default()
                .iter()
                .map(|v| v.as_u64().unwrap_or(0))
                .collect();
            // DrawIndexed(count, start, base); DrawIndexedInstanced(count, instances, start, base, first instance)
            let drawn = match call {
                "DrawIndexed" => a.first().zip(a.get(1)),
                "DrawIndexedInstanced" => a.first().zip(a.get(2)),
                _ => None,
            };
            let range = drawn.map(|(&count, &start)| (offset + start * width, count * width));
            let drawn = range.and_then(|(o, n)| c.names.indices(c, r, o, n, vbs));
            let bound = c.names.indices(c, r, offset, size, vbs);
            // The one fewer files share: a face set's indices are often the same in models of one shape, the data
            // block holding them seldom. The block tells the file; the face set of that file the draw reads is what
            // it draws (and what the key list says of it: its mesh's material).
            match (drawn, bound) {
                (Some(d), b) if b.as_ref().is_none_or(|b| b.others >= d.others) => Some(d),
                (_, Some(b)) => range
                    .and_then(|(o, n)| c.names.part_of(c, r, o, n, &b))
                    .or(Some(b)),
                (d, b) => d.or(b),
            }
        }
        _ => None,
    }
}

/// The vertex buffer slots input layout `layout` reads; none known without a layout.
pub(crate) fn layout_slots(c: &Capture, layout: Option<i64>) -> Option<Vec<u32>> {
    let json: String = c
        .row("SELECT json FROM states WHERE id = ?1", [layout?], |r| r.get(0))
        .flatten()?;
    let elements: Vec<Value> = serde_json::from_str(&json).ok()?;
    elements
        .iter()
        .map(|e| e["slot"].as_u64().and_then(|s| u32::try_from(s).ok()))
        .collect()
}

/// The vertex buffers a draw reads (of its bindings `rows`, those in slots its layout reads), which a match of only
/// index bytes can be corroborated by.
pub(crate) fn read_vertices(rows: &[Row], layout_slots: Option<&[u32]>) -> Vec<Vb> {
    rows.iter()
        .filter(|b| b.kind == "vb")
        .filter(|b| layout_slots.is_none_or(|s| s.contains(&b.slot)))
        .filter_map(|b| Some((b.resource?, b.offset, b.size)))
        .collect()
}

/// What draw `n`'s program calls register `b`, and whether it reads it (`None`: no reflection or layout to say): a
/// D3D12 root table binds every descriptor of its ranges, and a game can leave vertex buffers bound in slots a later
/// draw's input layout does not read.
pub(crate) fn register(
    c: &Capture,
    n: i64,
    b: &Row,
    layout_slots: Option<&[u32]>,
    reflection: &mut Reflection,
) -> (Option<String>, Option<bool>) {
    if matches!(b.kind.as_str(), "srv" | "uav" | "sampler" | "cb")
        && let Some(shader) = shader_of(c, n, &b.stage)
    {
        match reflection.slot(c, &shader, &b.kind, b.slot, b.space) {
            describe::Slot::Named(name) => (Some(name), Some(true)),
            describe::Slot::Unused => (None, Some(false)),
            describe::Slot::Unknown => (None, None),
        }
    } else if b.kind == "vb"
        && let Some(slots) = layout_slots
    {
        (None, Some(slots.contains(&b.slot)))
    } else {
        (None, None)
    }
}

/// The game file binding `b` of a draw (`call` with `args`, reading vertices `vbs`) was made from, when an index
/// names it: the whole resource, or the vertex or index range the draw reads.
pub(crate) fn file_of(c: &Capture, b: &Row, call: &str, args: &str, vbs: &[Vb]) -> Option<String> {
    files_of(c, b, call, args, vbs).map(|n| n.text)
}

/// The game files a binding was made from, as an index names them.
pub(crate) struct Named {
    /// The first file's name, with how many others have the same bytes.
    pub text: String,
    /// Every file found with the same bytes, the first of them first.
    pub files: Vec<String>,
    /// What the reader that listed the first said of it (`index::File::about`).
    pub about: Option<Value>,
}

/// `file_of`, with the names of every file found with the same bytes (the first of them as `file_of` names it) and
/// what the reader that listed it said of it.
pub(crate) fn files_of(c: &Capture, b: &Row, call: &str, args: &str, vbs: &[Vb]) -> Option<Named> {
    let r = b.resource?;
    let m = c
        .names
        .of(c, r)
        .or_else(|| range_name(c, r, b, call, args, vbs))?;
    let text = match m.others {
        0 => m.file.name.clone(),
        n => format!("{} (+{n} with the same bytes)", m.file.name),
    };
    let about = m.file.about.as_deref().and_then(|a| serde_json::from_str(a).ok());
    let mut files = vec![m.file.name];
    files.extend(m.also);
    Some(Named { text, files, about })
}

/// What a reader said of a file, in a line: each field's value, an array's length or an object's field count.
pub(crate) fn about_line(about: &Value) -> String {
    let Some(fields) = about.as_object() else {
        return about.to_string();
    };
    fields
        .iter()
        .map(|(k, v)| match v {
            Value::String(s) => format!("{k} {s}"),
            Value::Array(a) => format!("{k} [{}]", a.len()),
            Value::Object(o) => format!("{k} {{{}}}", o.len()),
            v => format!("{k} {v}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Draw `n` as `draw` shows it. Without `all`, only constant registers the shader uses.
pub fn describe(
    c: &Capture,
    n: i64,
    all: bool,
    reflection: &mut Reflection,
    lookup: &Lookup,
) -> Result<Draw, String> {
    type Head = (
        i64,
        String,
        String,
        Option<i64>,
        bool,
        Option<i64>,
        i64,
        Option<i64>,
    );
    let (event, call, args, pass, deferred, list, context, stack): Head = c
        .row(
            "SELECT d.event, d.call, d.args, d.pass, e.deferred, e.list, e.context, e.stack FROM draws d \
             JOIN events e ON e.seq = d.event WHERE d.n = ?1",
            [n],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                ))
            },
        )
        .ok_or_else(|| format!("no draw {n}"))?;
    let shaders: BTreeMap<&'static str, String> = c
        .row(
            "SELECT vs, hs, ds, gs, ps, cs FROM draws WHERE n = ?1",
            [n],
            |r| {
                (0..6)
                    .map(|i| r.get(i))
                    .collect::<rusqlite::Result<Vec<Option<String>>>>()
            },
        )
        .unwrap_or_default()
        .into_iter()
        .zip(["vs", "hs", "ds", "gs", "ps", "cs"])
        .filter_map(|(h, s)| h.map(|h| (s, h)))
        .collect();
    type Fixed = (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
    );
    let (topology, layout, raster, blend, depth, factor, mask, stencil_ref, predicate, predicate_value): Fixed = c
        .row(
            "SELECT topology, layout, rasterizer, blend, depth_stencil, blend_factor, sample_mask, stencil_ref, \
             predicate, predicate_value FROM draws WHERE n = ?1",
            [n],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        )
        .unwrap_or_default();
    let viewports = c.rows(
        "SELECT i, x, y, width, height, min_depth, max_depth FROM viewports WHERE draw = ?1 ORDER BY i",
        [n],
        |r| {
            Ok(Viewport {
                i: r.get(0)?,
                x: r.get(1)?,
                y: r.get(2)?,
                width: r.get(3)?,
                height: r.get(4)?,
                min_depth: r.get(5)?,
                max_depth: r.get(6)?,
            })
        },
    );
    let scissors = c.rows(
        "SELECT i, left, top, right, bottom FROM scissors WHERE draw = ?1 ORDER BY i",
        [n],
        |r| {
            Ok(Scissor {
                i: r.get(0)?,
                left: r.get(1)?,
                top: r.get(2)?,
                right: r.get(3)?,
                bottom: r.get(4)?,
            })
        },
    );

    let rows = rows(c, n);
    let layout_slots = layout_slots(c, layout);
    let vbs = read_vertices(&rows, layout_slots.as_deref());
    let mut bindings = Vec::with_capacity(rows.len());
    for b in &rows {
        let (name, used) = register(c, n, b, layout_slots.as_deref(), reflection);
        let view = b.view.map(|id| {
            let j: Value = c
                .row("SELECT json FROM views WHERE id = ?1", [id], |r| {
                    r.get::<_, Option<String>>(0)
                })
                .flatten()
                .and_then(|j| serde_json::from_str(&j).ok())
                .unwrap_or(Value::Null);
            let format = j["format"].as_u64().map(|f| f as u32);
            View {
                id,
                dimension: j["dimension"].as_str().map(str::to_string),
                format,
                format_name: format.map(format::name),
                range: j["range"].clone(),
            }
        });
        // Every file with the bytes, when there is more than one.
        let named = files_of(c, b, &call, &args, &vbs);
        let mut binding = Binding {
            stage: b.stage.clone(),
            kind: b.kind.clone(),
            slot: b.slot,
            space: b.space,
            name,
            used,
            view,
            state: b.state.and_then(|s| state(c, s)),
            resource: b.resource,
            shape: b.resource.and_then(|r| shape(c, r)).map(|s| s.to_string()),
            file: named.as_ref().map(|n| n.text.clone()),
            files: named
                .as_ref()
                .map(|n| n.files.clone())
                .filter(|f| f.len() > 1)
                .unwrap_or_default(),
            about: named.and_then(|n| n.about),
            offset: b.offset,
            size: b.size,
            stride: b.stride,
            format: b.format,
            format_name: b.format.map(|f| format::name(f as u32)),
            contents: None,
            contents_source: None,
            contents_note: None,
            written_before: None,
        };
        if b.kind == "cb" {
            binding.contents = b.content.clone();
        } else if let Some(r) = b.resource {
            type Src = (Option<String>, Option<String>, Option<String>);
            if let Some((key, source, note)) = c.row::<Src, _>(
                "SELECT content, content_source, content_note FROM resources WHERE id = ?1",
                [r],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            ) {
                binding.contents = key;
                binding.contents_source = source;
                binding.contents_note = match (note, describe::later_upload(c, r, event)) {
                    (Some(n), Some(w)) => Some(format!("{n}; {w}")),
                    (n, w) => n.or(w),
                };
            }
            // Written earlier in the frame: its contents at this draw are not those of the frame's start.
            binding.written_before = c
                .row(
                    "SELECT max(event) FROM event_resources WHERE resource = ?1 AND access = 'write' AND event < ?2",
                    [r, event],
                    |r| r.get(0),
                )
                .flatten();
        }
        bindings.push(binding);
    }

    let mut constants = Vec::new();
    for (b, binding) in rows.iter().zip(&bindings).filter(|(b, _)| b.kind == "cb") {
        if all || binding.used != Some(false) {
            constants.push(constants_of(c, reflection, n, b, all, lookup)?);
        }
    }
    let unread = |b: &Binding| b.used == Some(false);
    let unused = bindings.iter().filter(|b| unread(b) && b.kind != "vb").count();
    let unread_slots = bindings.iter().filter(|b| unread(b) && b.kind == "vb").count();
    if !all {
        bindings.retain(|b| !unread(b));
    }
    let mut registers: HashMap<(&str, &str, u32, u32), usize> = HashMap::new();
    for b in &rows {
        *registers
            .entry((b.stage.as_str(), b.kind.as_str(), b.slot, b.space))
            .or_default() += 1;
    }
    let doubled = registers.values().filter(|&&n| n > 1).count();
    let targets = c.rows(
        "SELECT target, blob, width, height, format FROM targets WHERE draw = ?1",
        [n],
        |r| {
            let format: i64 = r.get(4)?;
            Ok(Target {
                target: r.get(0)?,
                blob: r.get(1)?,
                width: r.get(2)?,
                height: r.get(3)?,
                format,
                format_name: format::name(format as u32),
            })
        },
    );
    Ok(Draw {
        draw: n,
        event,
        named_args: describe::call_args(&call, &args),
        args: serde_json::from_str(&args).unwrap_or(Value::String(args)),
        call,
        pass,
        deferred,
        list,
        context,
        queue: queued(c, event),
        stack: stack.map(|s| describe::stack(c, s)).unwrap_or_default(),
        shader_names: shaders
            .iter()
            .filter_map(|(s, h)| Some((*s, reflection.name(c, h)?)))
            .collect(),
        shaders,
        topology_name: topology.map(states::topology),
        topology,
        states: [layout, raster, blend, depth]
            .into_iter()
            .flatten()
            .filter_map(|id| state(c, id))
            .collect(),
        blend_factor: factor.map(|f| serde_json::from_str(&f).unwrap_or(Value::String(f))),
        sample_mask: mask.map(|m| m as u32),
        stencil_ref,
        predicate,
        predicate_value,
        viewports,
        scissors,
        bindings,
        unused,
        unread_slots,
        doubled,
        constants,
        targets,
    })
}

/// Why a capture has constant buffers without contents: its gaps about constants, by reason and count. The rows
/// do not say which reason was a given binding's.
fn uncaptured(c: &Capture) -> String {
    let gaps: Vec<(String, String, i64)> = c.rows(
        "SELECT what, detail, count FROM gaps WHERE what LIKE '%constant%' OR what = 'cb' ORDER BY count DESC",
        [],
        |r| Ok((r.get(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default(), r.get(2)?)),
    );
    if gaps.is_empty() {
        return "the capture records no reason (captures from before the reasons were counted have none)"
            .into();
    }
    let reasons: Vec<String> = gaps
        .iter()
        .map(|(what, why, n)| format!("{what}: {why} ({n})"))
        .collect();
    format!("this capture's reasons: {}", reasons.join("; "))
}

/// A bound constant buffer word by word, named from the shader's reflection. Without `all`, only the registers
/// holding variables the shader uses.
pub(crate) fn constants_of(
    c: &Capture,
    reflection: &mut Reflection,
    draw: i64,
    b: &Row,
    all: bool,
    lookup: &Lookup,
) -> Result<Constants, String> {
    let mut out = Constants {
        stage: b.stage.clone(),
        slot: b.slot,
        space: b.space,
        name: None,
        declared: None,
        buffer: b.resource,
        key: b.content.clone(),
        bytes: 0,
        words: Vec::new(),
        skipped: 0,
        why: None,
        extended: None,
        layout_from: lookup.layout.as_ref().map(|(_, from)| from.clone()),
        source: None,
    };
    let bytes: Vec<u8> = match &b.content {
        Some(key) => c
            .row("SELECT bytes FROM cb_data WHERE hash = ?1", [key], |r| r.get(0))
            .ok_or_else(|| format!("constant buffer contents {key} missing"))?,
        None => match unrecorded(c, b, draw, lookup) {
            Ok((bytes, source)) => {
                out.source = Some(source);
                bytes
            }
            Err(e) => {
                out.why = Some(format!("{}; from the buffer: {e}", uncaptured(c)));
                return Ok(out);
            }
        },
    };
    let shader = shader_of(c, draw, &b.stage);
    let extend = lookup.extend;
    let cb = match &lookup.layout {
        Some((layout, _)) => Some(layout),
        None => shader
            .as_deref()
            .and_then(|s| reflection.cb(c, s, b.slot, b.space)),
    };
    out.declared = cb.map(|cb| cb.size);
    let wide = cb.and_then(|cb| cb.extended(u32::try_from(bytes.len()).unwrap_or(u32::MAX)));
    out.extended = wide.as_ref().map(|(_, variable, declared, elements)| Extended {
        variable: variable.clone(),
        declared: *declared,
        elements: *elements,
        named: extend,
    });
    let cb = match &wide {
        Some((wide, ..)) if extend => Some(wide),
        _ => cb,
    };
    // a program built without names (Unreal's DXIL) declares the buffer and its size but no variables
    out.name = cb.map(|cb| {
        if cb.name.is_empty() {
            "unnamed".to_string()
        } else {
            cb.name.clone()
        }
    });
    out.bytes = bytes.len();
    // the bytes the program reads: its variables', or, when it lists none, all it declares
    let used: Vec<(u32, u32)> = cb
        .map(|cb| {
            if cb.variables.is_empty() {
                vec![(0, cb.size)]
            } else {
                cb.variables
                    .iter()
                    .filter(|v| v.used)
                    .map(|v| (v.offset, v.offset + v.size))
                    .collect()
            }
        })
        .unwrap_or_default();
    for (r, reg) in bytes.chunks(16).enumerate() {
        let at = r as u32 * 16;
        if !(all || cb.is_none() || used.iter().any(|&(a, e)| a < at + 16 && at < e)) {
            out.skipped += 1;
            continue;
        }
        for (i, word) in reg.chunks(4).enumerate() {
            let offset = at + i as u32 * 4;
            let named = cb.and_then(|cb| cb.at(offset));
            let base = named.as_ref().map(|n| n.1);
            let mut w = [0u8; 4];
            w[..word.len()].copy_from_slice(word);
            out.words.push(Word {
                offset,
                register: format!("c{r}.{}", ["x", "y", "z", "w"][i]),
                name: named.map(|n| n.0),
                value: number(w, base),
                hex: format!("{:08x}", u32::from_le_bytes(w)),
                text: value(w, base),
            });
        }
    }
    Ok(out)
}

fn print_constants(k: &Constants) {
    let space = match k.space {
        0 => String::new(),
        s => format!(" space {s}"),
    };
    if k.why.is_some() {
        let why = k.why.as_deref().map(|w| format!(": {w}")).unwrap_or_default();
        out!("{} b{}{space}: contents not captured{why}", k.stage, k.slot);
        return;
    }
    // A bound range larger than the program declares: the rest is the next records or padding.
    let declared = match k.declared {
        Some(d) if d as usize != k.bytes => format!(" bound, the program declares {d}"),
        _ => String::new(),
    };
    let extended = match &k.extended {
        Some(e) if e.named => format!(
            "; {} named as {} elements, {} past the {} declared",
            e.variable,
            e.elements,
            e.elements - e.declared,
            e.declared
        ),
        Some(e) => format!(
            "; `cb --extend` names the rest as {}[{}..{}]",
            e.variable,
            e.declared,
            e.elements - 1
        ),
        None => String::new(),
    };
    out!(
        "{} b{}{space} ({}{}, {} bytes{declared}{}{extended}):",
        k.stage,
        k.slot,
        k.name.as_deref().unwrap_or("no reflection"),
        k.layout_from
            .as_deref()
            .map(|p| format!(" as program {} declares it, not the draw's", short(p)))
            .unwrap_or_default(),
        k.bytes,
        k.buffer.map_or(String::new(), |r| format!(", buffer #{r}"))
    );
    if let Some(source) = &k.source {
        out!("  (no bytes recorded for this binding; read from the buffer: {source})");
    }
    // One row per register, each word under its component: `--name` can leave some of a register's words out.
    let mut rows: BTreeMap<u32, [Option<&Word>; 4]> = BTreeMap::new();
    for w in &k.words {
        rows.entry(w.offset / 16).or_default()[(w.offset % 16 / 4) as usize] = Some(w);
    }
    for (r, register) in rows {
        let names: [Option<String>; 4] = register.map(|w| w.and_then(|w| w.name.clone()));
        out!(
            "  c{r:<4}{:<53}  {}",
            register
                .iter()
                .map(|w| format!(" {:>12}", w.map_or("", |w| w.text.as_str())))
                .collect::<String>(),
            register_name(&names)
        );
    }
    if k.skipped > 0 {
        out!(
            "  ({} registers the shader does not use; --all shows them)",
            k.skipped
        );
    }
}

fn queued(c: &Capture, event: i64) -> Option<Queued> {
    if c.meta("api").as_deref() != Some("d3d12") {
        return None;
    }
    c.row(
        "SELECT l.queue, l.execution, x.list, x.generation, l.command_index FROM d12_event_locations l \
         JOIN d12_executions x ON x.id = l.execution WHERE l.event = ?1",
        [event],
        |r| {
            Ok(Queued {
                queue: r.get(0)?,
                execution: r.get(1)?,
                list: r.get(2)?,
                generation: r.get(3)?,
                command: r.get(4)?,
            })
        },
    )
}

/// The parts of a draw's text after its first lines (the call and the shaders): `state`, `bound`, `cb`, `targets`;
/// none named shows them all.
pub const SECTIONS: [&str; 4] = ["state", "bound", "cb", "targets"];

fn print(d: &Draw, sections: &[String]) {
    let show = |s: &str| sections.is_empty() || sections.iter().any(|x| x == s);
    let place = if let Some(q) = &d.queue {
        format!(
            "queue {}, execution {}, list {} generation {}, command {}",
            q.queue, q.execution, q.list, q.generation, q.command
        )
    } else if d.deferred {
        format!(
            "command list {} (recorded on context {})",
            d.list.map_or("?".into(), |l| l.to_string()),
            d.context
        )
    } else {
        "the immediate context".into()
    };
    out!(
        "draw {}: {} {}, event {}, pass {}, on {place}",
        d.draw,
        d.call,
        d.named_args,
        d.event,
        d.pass.map_or("-".into(), |p| p.to_string())
    );
    if !d.stack.is_empty() {
        out!(
            "  called from {}",
            d.stack.iter().take(4).cloned().collect::<Vec<_>>().join(" < ")
        );
    }
    let named: Vec<String> = ["vs", "hs", "ds", "gs", "ps", "cs"]
        .iter()
        .filter_map(|s| {
            d.shaders.get(s).map(|h| match d.shader_names.get(s) {
                Some(name) => format!("{s} {h} ({name})"),
                None => format!("{s} {h}"),
            })
        })
        .collect();
    out!("shaders: {}", named.join("  "));
    if show("state") {
        print_state(d);
    }
    if show("bound") {
        print_bound(d);
    }
    if show("cb") {
        for k in &d.constants {
            print_constants(k);
        }
    }
    if show("targets") {
        for t in &d.targets {
            out!(
                "after it, {} read back: {}x{} {} {}",
                t.target,
                t.width,
                t.height,
                t.format_name,
                t.blob.as_deref().unwrap_or("-")
            );
        }
    }
}

/// Topology, pipeline states, predicate, viewports and scissors.
fn print_state(d: &Draw) {
    if let Some(t) = &d.topology_name {
        out!("topology: {t}");
    }
    for s in &d.states {
        let name = match s.kind.as_str() {
            "input_layout" => "input layout",
            "depth_stencil" => "depth-stencil",
            other => other,
        };
        let gap = if s.text.starts_with('\n') { "" } else { " " };
        out!("{name} #{}:{gap}{}", s.id, s.text);
    }
    let has = |kind: &str| d.states.iter().any(|s| s.kind == kind);
    if has("blend") || d.blend_factor.is_some() {
        out!(
            "blend factor {}, sample mask 0x{:x}",
            d.blend_factor.as_ref().map_or(String::new(), Value::to_string),
            d.sample_mask.unwrap_or(u32::MAX)
        );
    }
    if has("depth_stencil") {
        out!("stencil ref {}", d.stencil_ref.unwrap_or(0));
    }
    if let Some(p) = d.predicate {
        out!(
            "predicate #{p}, drawn when {}",
            d.predicate_value.unwrap_or(0) != 0
        );
    }
    if !d.viewports.is_empty() {
        let v: Vec<String> = d
            .viewports
            .iter()
            .map(|v| {
                format!(
                    "{}: {},{} {}x{} depth {}..{}",
                    v.i, v.x, v.y, v.width, v.height, v.min_depth, v.max_depth
                )
            })
            .collect();
        out!("viewports: {}", v.join("; "));
    }
    if !d.scissors.is_empty() {
        let s: Vec<String> = d
            .scissors
            .iter()
            .map(|s| format!("{}: ({}, {})-({}, {})", s.i, s.left, s.top, s.right, s.bottom))
            .collect();
        out!("scissors: {}", s.join("; "));
    }
}

/// Everything bound, with views, resources and contents.
fn print_bound(d: &Draw) {
    out!("bound:");
    for b in &d.bindings {
        let mut line = format!("  {} {}{}", b.stage, b.kind, b.slot);
        if b.space != 0 {
            line += &format!(" space {}", b.space);
        }
        match (&b.name, b.used) {
            (Some(name), _) => line += &format!(" {name}"),
            (None, Some(false)) if b.kind == "vb" => line += " (the input layout does not read it)",
            (None, Some(false)) => line += " (the shader does not use it)",
            _ => {}
        }
        if let Some(v) = &b.view {
            line += &format!(" view #{}", v.id);
            if v.dimension.is_some() || v.format.is_some() {
                line += &format!(
                    " ({} {}",
                    v.dimension.as_deref().unwrap_or("?"),
                    v.format_name.unwrap_or_else(|| format::name(0))
                );
                if let Some(r) = v.range.as_object().filter(|r| !r.is_empty()) {
                    line += &format!(" {}", Value::Object(r.clone()));
                }
                line += ")";
            }
        }
        if let Some(s) = &b.state {
            line += &format!(" #{}: {}", s.id, s.text);
        }
        if let Some(r) = b.resource {
            match &b.shape {
                Some(s) => line += &format!(" -> {s}"),
                None => line += &format!(" -> #{r}"),
            }
            if let Some(f) = &b.file {
                line += &format!(" = {f}");
            }
        }
        if let Some(o) = b.offset.filter(|&o| o != 0) {
            line += &format!(" offset {o}");
        }
        if b.kind == "cb"
            && let Some(sz) = b.size
        {
            line += &format!(" size {sz}");
        }
        if let Some(st) = b.stride {
            line += &format!(" stride {st}");
        }
        if let Some(f) = b.format_name {
            line += &format!(" {f}");
        }
        if b.kind != "cb" && b.resource.is_some() {
            match (&b.contents, b.written_before) {
                (Some(key), Some(e)) => {
                    line += &format!(
                        "  contents {} as the frame began ({}), not as this draw saw them: written since, last at \
                         event {e} (export --at {} gives what it saw)",
                        short(key),
                        b.contents_source.as_deref().unwrap_or_default(),
                        d.event
                    )
                }
                (Some(key), None) => {
                    line += &format!(
                        "  contents {} ({})",
                        short(key),
                        b.contents_source.as_deref().unwrap_or_default()
                    )
                }
                (None, Some(e)) => line += &format!("  written earlier, last at event {e}"),
                (None, None) => {}
            }
            if let Some(note) = &b.contents_note {
                line += &format!(" [{note}]");
            }
        }
        out!("{line}");
        if let Some(about) = &b.about {
            out!("      {}", about_line(about));
        }
    }
    if d.unused > 0 {
        out!(
            "  ({} bound register{} the shaders do not use; --all shows {})",
            d.unused,
            if d.unused == 1 { "" } else { "s" },
            if d.unused == 1 { "it" } else { "them" }
        );
    }
    if d.unread_slots > 0 {
        out!(
            "  ({} bound vertex buffer slot{} the input layout does not read; --all shows {})",
            d.unread_slots,
            if d.unread_slots == 1 { "" } else { "s" },
            if d.unread_slots == 1 { "it" } else { "them" }
        );
    }
    if d.doubled > 0 {
        out!(
            "  ({} register{} bound to more than one descriptor: an older locum projected this capture's bindings and let \
             a table range reach descriptors of another type, so only one of each is what the program read; \
             `locum-cli upgrade` projects them again)",
            d.doubled,
            if d.doubled == 1 { "" } else { "s" }
        );
    }
}

fn json<T: Serialize>(v: &T) -> Result<(), String> {
    out!("{}", serde_json::to_string(v).map_err(|e| e.to_string())?);
    Ok(())
}

/// `draw`: one draw (a number or `last`), several (`120-180`, `5,9,200-last`), or with `all` every draw, as text or
/// JSON (one object per line); with `sections`, only those parts; with `name`, only the constants so named.
pub fn draw(
    c: &Capture,
    which: &str,
    all_constants: bool,
    as_json: bool,
    sections: &[String],
    name: Option<&str>,
) -> Result<(), String> {
    let mut reflection = Reflection::default();
    let lookup = Lookup::default();
    let numbers = numbers(c, which)?;
    let several = numbers.len() > 1;
    for n in numbers {
        let mut d = describe(c, n, all_constants, &mut reflection, &lookup)?;
        if let Some(name) = name {
            keep_named(&mut d, name);
        }
        if as_json {
            json(&sectioned(&d, sections)?)?;
        } else {
            print(&d, sections);
            if several {
                out!("");
            }
        }
    }
    Ok(())
}

/// The draws `which` names: `all`, or items separated by `,`, each a number, `last`, or a range `A-B` (either end
/// `last`), in the order given.
fn numbers(c: &Capture, which: &str) -> Result<Vec<i64>, String> {
    if which == "all" {
        return Ok(c.rows("SELECT n FROM draws ORDER BY n", [], |r| r.get(0)));
    }
    let mut out = Vec::new();
    for item in which.split(',').map(str::trim).filter(|i| !i.is_empty()) {
        match item.split_once('-') {
            Some((a, b)) => {
                let (a, b) = (
                    crate::replay::draw_number(c, a)?,
                    crate::replay::draw_number(c, b)?,
                );
                if a > b {
                    return Err(format!("{item}: the range runs backwards"));
                }
                out.extend(a..=b);
            }
            None => out.push(crate::replay::draw_number(c, item)?),
        }
    }
    if out.is_empty() {
        return Err(format!("{which:?} names no draw"));
    }
    Ok(out)
}

/// Keeps only the constants whose buffer's or word's name holds one of `name`'s `|`-separated parts (any case).
fn keep_named(d: &mut Draw, name: &str) {
    let parts: Vec<String> = name
        .split('|')
        .map(str::to_lowercase)
        .filter(|p| !p.is_empty())
        .collect();
    let holds = |n: &Option<String>| {
        n.as_ref()
            .is_some_and(|n| parts.iter().any(|p| n.to_lowercase().contains(p)))
    };
    d.constants.retain_mut(|k| {
        if !holds(&k.name) {
            k.words.retain(|w| holds(&w.name));
        }
        !k.words.is_empty()
    });
}

/// The draw as JSON, with only `sections` (all when empty) of what follows the call and its shaders.
fn sectioned(d: &Draw, sections: &[String]) -> Result<Value, String> {
    let mut v = serde_json::to_value(d).map_err(|e| e.to_string())?;
    if sections.is_empty() {
        return Ok(v);
    }
    const PARTS: [(&str, &[&str]); 4] = [
        (
            "state",
            &[
                "topology",
                "topology_name",
                "states",
                "blend_factor",
                "sample_mask",
                "stencil_ref",
                "predicate",
                "predicate_value",
                "viewports",
                "scissors",
            ],
        ),
        ("bound", &["bindings", "unused", "unread_slots", "doubled"]),
        ("cb", &["constants"]),
        ("targets", &["targets"]),
    ];
    if let Some(o) = v.as_object_mut() {
        for (section, keys) in PARTS {
            if !sections.iter().any(|s| s == section) {
                for k in keys {
                    o.remove(*k);
                }
            }
        }
    }
    Ok(v)
}

/// `cb`: a draw's constant buffer, every register, by name; or its bytes in hex; or as JSON.
/// Which registers `cb` shows: by register number, or by a part of a variable's name (any case).
#[derive(Default)]
pub struct Only {
    pub registers: Vec<u32>,
    pub name: Option<String>,
    /// Name the registers past the program's declared size as further elements of its last array.
    pub extend: bool,
    /// Name them as the capture's programs declare the constant buffer of this name, not as the draw's does.
    pub layout: Option<String>,
}

#[allow(clippy::too_many_arguments)]
pub fn cb(
    c: &Capture,
    draw: i64,
    stage: &str,
    slot: u32,
    space: u32,
    raw: bool,
    as_json: bool,
    only: &Only,
) -> Result<(), String> {
    let (here, elsewhere): (Vec<_>, Vec<_>) = rows(c, draw)
        .into_iter()
        .filter(|b| b.kind == "cb" && b.stage == stage && b.slot == slot)
        .partition(|b| b.space == space);
    if here.is_empty() {
        let spaces: Vec<String> = elsewhere.iter().map(|b| b.space.to_string()).collect();
        return Err(if spaces.is_empty() {
            format!("draw {draw} has no {stage} b{slot}")
        } else {
            format!(
                "draw {draw} has no {stage} b{slot} in space {space}; --space {} has one",
                spaces.join(" or ")
            )
        });
    }
    // A register bound to several descriptors (`Draw::doubled`): each is shown, since the capture cannot say which
    // the program read.
    let several = here.len() > 1;
    if several {
        let message = format!(
            "{stage} b{slot} is bound to {} descriptors: an older locum projected this capture's bindings, and only \
             one is what the program read (`locum-cli upgrade` projects them again)",
            here.len()
        );
        if as_json {
            eprintln!("{message}; the JSON is the first");
        } else {
            out!("({message})");
        }
    }
    for (i, b) in here.into_iter().enumerate() {
        if several && i > 0 && as_json {
            break;
        }
        if several && !as_json {
            let at = b.offset.map_or(String::new(), |o| format!(" offset {o}"));
            let buffer = b.resource.map_or("no buffer".to_string(), |r| format!("#{r}"));
            out!("descriptor {}: {buffer}{at}", i + 1);
        }
        constants_at(c, draw, b, raw, as_json, only)?;
    }
    Ok(())
}

/// One constant-buffer binding of `cb`: its registers, or with `raw` its bytes.
fn constants_at(c: &Capture, draw: i64, b: Row, raw: bool, as_json: bool, only: &Only) -> Result<(), String> {
    if raw {
        let key = b.content.ok_or("contents not captured")?;
        let bytes: Vec<u8> = c
            .row("SELECT bytes FROM cb_data WHERE hash = ?1", [&key], |r| r.get(0))
            .ok_or("contents missing")?;
        for (i, row) in bytes.chunks(16).enumerate() {
            let hex: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
            out!("{:6x}  {}", i * 16, hex.join(" "));
        }
        return Ok(());
    }
    let mut reflection = Reflection::default();
    let layout =
        match &only.layout {
            Some(name) => Some(reflection.declared_as(c, name).ok_or_else(|| {
                format!("no program of the capture declares a constant buffer called {name}")
            })?),
            None => None,
        };
    let lookup = Lookup::new(only.extend, layout);
    let mut k = constants_of(c, &mut reflection, draw, &b, true, &lookup)?;
    if !only.registers.is_empty() || only.name.is_some() {
        // parts separated by `|`, any of which a name may hold
        let wanted: Vec<String> = only
            .name
            .iter()
            .flat_map(|n| n.split('|'))
            .map(|p| p.trim().to_lowercase())
            .filter(|p| !p.is_empty())
            .collect();
        let keep = |register: &[Word]| {
            let r = register[0].offset / 16;
            only.registers.contains(&r)
                || register.iter().any(|x| {
                    x.name
                        .as_ref()
                        .is_some_and(|n| wanted.iter().any(|w| n.to_lowercase().contains(w)))
                })
        };
        let words = std::mem::take(&mut k.words);
        k.words = words.chunks(4).filter(|r| keep(r)).flatten().cloned().collect();
        k.skipped = 0;
    }
    if as_json {
        json(&k)
    } else {
        print_constants(&k);
        Ok(())
    }
}
