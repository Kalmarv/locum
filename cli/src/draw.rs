//! `draw` and `cb`: one draw or dispatch, everything it had bound and its constants by name, as text or as JSON
//! (`--json`; with `all`, one object per line for every draw, so a census over a frame is one process).

use std::collections::BTreeMap;

use locum_capture::format;
use serde::Serialize;
use serde_json::Value;

use crate::describe::{self, Reflection, STAGE_ORDER, register_name, shape, short, value};
use crate::draws::shader_of;
use crate::open::Capture;
use crate::out;
use crate::states;

/// A binding row as the capture has it.
struct Row {
    stage: String,
    kind: String,
    slot: u32,
    space: u32,
    view: Option<i64>,
    resource: Option<i64>,
    state: Option<i64>,
    offset: Option<i64>,
    size: Option<i64>,
    stride: Option<i64>,
    format: Option<i64>,
    content: Option<String>,
}

fn rows(c: &Capture, draw: i64) -> Vec<Row> {
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
    /// Its name in the shader's reflection (srv, uav and sampler slots).
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
    pub buffer: Option<i64>,
    /// The key of the bytes the GPU read, and their length.
    pub key: Option<String>,
    pub bytes: usize,
    /// The 4-byte words shown: those of registers the shader uses, or all of them.
    pub words: Vec<Word>,
    /// Registers left out because the shader does not use them.
    pub skipped: usize,
}

#[derive(Serialize)]
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
    text: String,
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
fn number(bytes: [u8; 4], base: Option<u16>) -> Value {
    match base {
        Some(1) => Value::from(u32::from_le_bytes(bytes) != 0),
        Some(2) => Value::from(i32::from_le_bytes(bytes)),
        Some(19 | 20) => Value::from(u32::from_le_bytes(bytes)),
        _ => Value::from(f64::from(f32::from_le_bytes(bytes))),
    }
}

/// Draw `n` as `draw` shows it. Without `all`, only constant registers the shader uses.
pub fn describe(c: &Capture, n: i64, all: bool, reflection: &mut Reflection) -> Result<Draw, String> {
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
    let mut bindings = Vec::with_capacity(rows.len());
    for b in &rows {
        let (name, used) = if matches!(b.kind.as_str(), "srv" | "uav" | "sampler")
            && let Some(shader) = shader_of(c, n, &b.stage)
        {
            match reflection.slot(c, &shader, &b.kind, b.slot, b.space) {
                describe::Slot::Named(name) => (Some(name), Some(true)),
                describe::Slot::Unused => (None, Some(false)),
                describe::Slot::Unknown => (None, None),
            }
        } else {
            (None, None)
        };
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
            file: b
                .resource
                .and_then(|r| c.names.of(c, r))
                .map(|m| m.file.name.clone()),
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
                binding.contents_note = note;
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
    for b in rows.iter().filter(|b| b.kind == "cb") {
        constants.push(constants_of(c, reflection, n, b, all)?);
    }
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
        constants,
        targets,
    })
}

/// A bound constant buffer word by word, named from the shader's reflection. Without `all`, only the registers
/// holding variables the shader uses.
fn constants_of(
    c: &Capture,
    reflection: &mut Reflection,
    draw: i64,
    b: &Row,
    all: bool,
) -> Result<Constants, String> {
    let mut out = Constants {
        stage: b.stage.clone(),
        slot: b.slot,
        space: b.space,
        name: None,
        buffer: b.resource,
        key: b.content.clone(),
        bytes: 0,
        words: Vec::new(),
        skipped: 0,
    };
    let Some(key) = &b.content else {
        return Ok(out);
    };
    let bytes: Vec<u8> = c
        .row("SELECT bytes FROM cb_data WHERE hash = ?1", [key], |r| r.get(0))
        .ok_or_else(|| format!("constant buffer contents {key} missing"))?;
    let shader = shader_of(c, draw, &b.stage);
    let cb = shader
        .as_deref()
        .and_then(|s| reflection.cb(c, s, b.slot, b.space));
    out.name = cb.map(|cb| cb.name.clone());
    out.bytes = bytes.len();
    let used: Vec<(u32, u32)> = cb
        .map(|cb| {
            cb.variables
                .iter()
                .filter(|v| v.used)
                .map(|v| (v.offset, v.offset + v.size))
                .collect()
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
    if k.key.is_none() {
        out!("{} b{}{space}: contents not captured", k.stage, k.slot);
        return;
    }
    out!(
        "{} b{}{space} ({}, {} bytes{}):",
        k.stage,
        k.slot,
        k.name.as_deref().unwrap_or("no reflection"),
        k.bytes,
        k.buffer.map_or(String::new(), |r| format!(", buffer #{r}"))
    );
    for register in k.words.chunks(4) {
        let mut names: [Option<String>; 4] = Default::default();
        for (i, w) in register.iter().enumerate() {
            names[i] = w.name.clone();
        }
        let r = register[0].offset / 16;
        out!(
            "  c{r:<4}{:<53}  {}",
            register
                .iter()
                .map(|w| format!(" {:>12}", w.text))
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
        "SELECT l.queue, l.execution, x.list, x.generation, l.command_index FROM d12_event_locations l          JOIN d12_executions x ON x.id = l.execution WHERE l.event = ?1",
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

fn print(d: &Draw) {
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
        .filter_map(|s| d.shaders.get(s).map(|h| format!("{s} {h}")))
        .collect();
    out!("shaders: {}", named.join("  "));
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

    out!("bound:");
    for b in &d.bindings {
        let mut line = format!("  {} {}{}", b.stage, b.kind, b.slot);
        if b.space != 0 {
            line += &format!(" space {}", b.space);
        }
        match (&b.name, b.used) {
            (Some(name), _) => line += &format!(" {name}"),
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
    }
    for k in &d.constants {
        print_constants(k);
    }
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

fn json<T: Serialize>(v: &T) -> Result<(), String> {
    out!("{}", serde_json::to_string(v).map_err(|e| e.to_string())?);
    Ok(())
}

/// `draw`: one draw (a number or `last`), or with `all` every draw, as text or JSON (one object per line).
pub fn draw(c: &Capture, which: &str, all_constants: bool, as_json: bool) -> Result<(), String> {
    let mut reflection = Reflection::default();
    if which == "all" {
        let numbers: Vec<i64> = c.rows("SELECT n FROM draws ORDER BY n", [], |r| r.get(0));
        for n in numbers {
            let d = describe(c, n, all_constants, &mut reflection)?;
            if as_json {
                json(&d)?;
            } else {
                print(&d);
                out!("");
            }
        }
        return Ok(());
    }
    let n = crate::replay::draw_number(c, which)?;
    let d = describe(c, n, all_constants, &mut reflection)?;
    if as_json {
        json(&d)
    } else {
        print(&d);
        Ok(())
    }
}

/// `cb`: a draw's constant buffer, every register, by name; or its bytes in hex; or as JSON.
pub fn cb(
    c: &Capture,
    draw: i64,
    stage: &str,
    slot: u32,
    space: u32,
    raw: bool,
    as_json: bool,
) -> Result<(), String> {
    let (mut here, elsewhere): (Vec<_>, Vec<_>) = rows(c, draw)
        .into_iter()
        .filter(|b| b.kind == "cb" && b.stage == stage && b.slot == slot)
        .partition(|b| b.space == space);
    let Some(b) = here.pop() else {
        let spaces: Vec<String> = elsewhere.iter().map(|b| b.space.to_string()).collect();
        return Err(if spaces.is_empty() {
            format!("draw {draw} has no {stage} b{slot}")
        } else {
            format!(
                "draw {draw} has no {stage} b{slot} in space {space}; --space {} has one",
                spaces.join(" or ")
            )
        });
    };
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
    let k = constants_of(c, &mut Reflection::default(), draw, &b, true)?;
    if as_json {
        json(&k)
    } else {
        print_constants(&k);
        Ok(())
    }
}
