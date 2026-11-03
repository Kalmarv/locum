//! `draws`, `draw`, `cb`, `find` and `passes`: the frame's draws and dispatches, what each had bound, and its
//! constants by name.

use std::collections::{BTreeMap, HashMap};

use locum_capture::format;
use rusqlite::types::Value;

use crate::describe::{self, Reflection, STAGE_ORDER, register_name, shape, short, value};
use crate::open::Capture;
use crate::out;
use crate::states;

#[derive(Default)]
pub struct Filter {
    pub vs: Option<String>,
    pub ps: Option<String>,
    pub cs: Option<String>,
    pub shader: Option<String>,
    pub pass: Option<i64>,
    pub uses: Option<i64>,
    pub target: Option<i64>,
    pub content: Option<String>,
    pub call: Option<String>,
}

const SHADER_COLUMNS: [&str; 6] = ["vs", "hs", "ds", "gs", "ps", "cs"];

pub fn draws(c: &Capture, f: &Filter) -> Result<(), String> {
    let mut wh: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    let mut param = |v: Value| {
        params.push(v);
        format!("?{}", params.len())
    };
    for (col, v) in [("vs", &f.vs), ("ps", &f.ps), ("cs", &f.cs)] {
        if let Some(v) = v {
            wh.push(format!("d.{col} LIKE {} || '%'", param(Value::Text(v.clone()))));
        }
    }
    if let Some(v) = &f.shader {
        let p = param(Value::Text(v.clone()));
        let any: Vec<String> = SHADER_COLUMNS
            .iter()
            .map(|s| format!("d.{s} LIKE {p} || '%'"))
            .collect();
        wh.push(format!("({})", any.join(" OR ")));
    }
    if let Some(p) = f.pass {
        wh.push(format!("d.pass = {}", param(Value::Integer(p))));
    }
    if let Some(r) = f.uses {
        wh.push(format!(
            "EXISTS (SELECT 1 FROM bindings b WHERE b.draw = d.n AND b.resource = {})",
            param(Value::Integer(r))
        ));
    }
    if let Some(r) = f.target {
        wh.push(format!(
            "EXISTS (SELECT 1 FROM bindings b WHERE b.draw = d.n AND b.kind IN ('rtv', 'dsv') AND b.resource = {})",
            param(Value::Integer(r))
        ));
    }
    if let Some(k) = &f.content {
        let p = param(Value::Text(k.clone()));
        wh.push(format!(
            "EXISTS (SELECT 1 FROM bindings b LEFT JOIN resources r ON r.id = b.resource WHERE b.draw = d.n AND \
             (b.content LIKE {p} || '%' OR r.content LIKE {p} || '%' OR r.upload LIKE {p} || '%'))"
        ));
    }
    if let Some(call) = &f.call {
        wh.push(format!("d.call LIKE {}", param(Value::Text(call.clone()))));
    }
    let sql = format!(
        "SELECT d.n, d.pass, d.call, d.args, e.deferred, e.list, d.vs, d.ps, d.cs FROM draws d \
         JOIN events e ON e.seq = d.event {} ORDER BY d.n",
        if wh.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", wh.join(" AND "))
        }
    );
    type Row = (
        i64,
        Option<i64>,
        String,
        String,
        bool,
        Option<i64>,
        [Option<String>; 3],
    );
    let rows: Vec<Row> = c.rows(&sql, rusqlite::params_from_iter(params), |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            [r.get(6)?, r.get(7)?, r.get(8)?],
        ))
    });
    out!(
        "{:>5} {:>4} {:<26} {:<8} {:<16} {:<16} targets",
        "draw",
        "pass",
        "call",
        "context",
        "vs (cs)",
        "ps"
    );
    for (n, pass, call, args, deferred, list, [vs, ps, cs]) in &rows {
        let targets: Vec<String> = c.rows(
            "SELECT kind, slot, resource FROM bindings WHERE draw = ?1 AND kind IN ('rtv', 'dsv', 'uav') \
             ORDER BY kind DESC, slot",
            [n],
            |r| {
                let (kind, slot, res): (String, i64, Option<i64>) = (r.get(0)?, r.get(1)?, r.get(2)?);
                let name = match kind.as_str() {
                    "rtv" => format!("rt{slot}"),
                    "dsv" => "depth".into(),
                    _ => format!("u{slot}"),
                };
                Ok(format!("{name} #{}", res.unwrap_or(-1)))
            },
        );
        let context = match (deferred, list) {
            (true, Some(l)) => format!("list {l}"),
            (true, None) => "list".into(),
            _ => "imm".into(),
        };
        let first = cs.as_ref().or(vs.as_ref());
        out!(
            "{n:>5} {:>4} {:<26} {context:<8} {:<16} {:<16} {}",
            pass.map_or("-".into(), |p| p.to_string()),
            format!("{call} {args}"),
            first.map_or("-", String::as_str),
            ps.as_deref().unwrap_or("-"),
            targets.join(" ")
        );
    }
    out!("{} draws", rows.len());
    Ok(())
}

struct Binding {
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

fn bindings(c: &Capture, draw: i64) -> Vec<Binding> {
    c.rows(
        &format!(
            "SELECT stage, kind, slot, space, view, resource, state, offset, size, stride, format, content \
             FROM bindings WHERE draw = ?1 ORDER BY {STAGE_ORDER}, kind, slot"
        ),
        [draw],
        |r| {
            Ok(Binding {
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

/// The shader a draw had bound at `stage`.
pub fn shader_of(c: &Capture, draw: i64, stage: &str) -> Option<String> {
    if !SHADER_COLUMNS.contains(&stage) {
        return None;
    }
    c.row(&format!("SELECT {stage} FROM draws WHERE n = ?1"), [draw], |r| {
        r.get(0)
    })
    .flatten()
}

/// A state object, its enums named.
fn state_text(c: &Capture, id: i64) -> String {
    c.row("SELECT kind, json FROM states WHERE id = ?1", [id], |r| {
        Ok(states::describe(
            &r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.as_deref(),
        ))
    })
    .unwrap_or_else(|| "?".into())
}

pub fn draw(c: &Capture, n: i64, all: bool) -> Result<(), String> {
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
    let head: Head = c
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
    let (event, call, args, pass, deferred, list, context, stack) = head;
    let place = if deferred {
        format!(
            "command list {} (recorded on context {context})",
            list.map_or("?".into(), |l| l.to_string())
        )
    } else {
        "the immediate context".into()
    };
    out!(
        "draw {n}: {call} {args}, event {event}, pass {}, on {place}",
        pass.map_or("-".into(), |p| p.to_string())
    );
    if let Some(s) = stack {
        let frames = describe::stack(c, s);
        out!(
            "  called from {}",
            frames.iter().take(4).cloned().collect::<Vec<_>>().join(" < ")
        );
    }

    let shaders: Vec<Option<String>> = c
        .row(
            "SELECT vs, hs, ds, gs, ps, cs FROM draws WHERE n = ?1",
            [n],
            |r| (0..6).map(|i| r.get(i)).collect::<rusqlite::Result<Vec<_>>>(),
        )
        .unwrap_or_default();
    let named: Vec<String> = SHADER_COLUMNS
        .iter()
        .zip(&shaders)
        .filter_map(|(s, h)| h.as_ref().map(|h| format!("{s} {h}")))
        .collect();
    out!("shaders: {}", named.join("  "));

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
    let fixed: Fixed = c
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
    let (topology, layout, raster, blend, depth, factor, mask, stencil_ref, predicate, predicate_value) =
        fixed;
    if let Some(t) = topology {
        out!("topology: {}", states::topology(t));
    }
    for (name, id) in [
        ("input layout", layout),
        ("rasterizer", raster),
        ("blend", blend),
        ("depth-stencil", depth),
    ] {
        if let Some(id) = id {
            let text = state_text(c, id);
            let gap = if text.starts_with('\n') { "" } else { " " };
            out!("{name} #{id}:{gap}{text}");
        }
    }
    if blend.is_some() || factor.is_some() {
        out!(
            "blend factor {}, sample mask 0x{:x}",
            factor.unwrap_or_default(),
            mask.unwrap_or(-1) as u32
        );
    }
    if depth.is_some() {
        out!("stencil ref {}", stencil_ref.unwrap_or(0));
    }
    if let Some(p) = predicate {
        out!("predicate #{p}, drawn when {}", predicate_value.unwrap_or(0) != 0);
    }
    let viewports: Vec<String> = c.rows(
        "SELECT i, x, y, width, height, min_depth, max_depth FROM viewports WHERE draw = ?1 ORDER BY i",
        [n],
        |r| {
            Ok(format!(
                "{}: {},{} {}x{} depth {}..{}",
                r.get::<_, i64>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, f64>(4)?,
                r.get::<_, f64>(5)?,
                r.get::<_, f64>(6)?
            ))
        },
    );
    if !viewports.is_empty() {
        out!("viewports: {}", viewports.join("; "));
    }
    let scissors: Vec<String> = c.rows(
        "SELECT i, left, top, right, bottom FROM scissors WHERE draw = ?1 ORDER BY i",
        [n],
        |r| {
            Ok(format!(
                "{}: ({}, {})-({}, {})",
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?
            ))
        },
    );
    if !scissors.is_empty() {
        out!("scissors: {}", scissors.join("; "));
    }

    out!("bound:");
    let list = bindings(c, n);
    for b in &list {
        let mut line = format!("  {} {}{}", b.stage, b.kind, b.slot);
        if b.space != 0 {
            line += &format!(" space {}", b.space);
        }
        if let Some(v) = b.view {
            line += &format!(" view #{v}");
            if let Some(json) = c
                .row("SELECT json FROM views WHERE id = ?1", [v], |r| {
                    r.get::<_, Option<String>>(0)
                })
                .flatten()
                && let Ok(j) = serde_json::from_str::<serde_json::Value>(&json)
            {
                let f = j["format"].as_u64().unwrap_or(0) as u32;
                line += &format!(" ({} {}", j["dimension"].as_str().unwrap_or("?"), format::name(f));
                if let Some(r) = j["range"].as_object().filter(|r| !r.is_empty()) {
                    line += &format!(" {}", serde_json::Value::Object(r.clone()));
                }
                line += ")";
            }
        }
        if let Some(s) = b.state {
            line += &format!(" #{s}: {}", state_text(c, s));
        }
        if let Some(r) = b.resource {
            match shape(c, r) {
                Some(s) => line += &format!(" -> {s}"),
                None => line += &format!(" -> #{r}"),
            }
            if let Some(m) = c.names.of(c, r) {
                line += &format!(" = {}", m.file.name);
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
        if let Some(f) = b.format {
            line += &format!(" {}", format::name(f as u32));
        }
        if let Some(r) = b.resource
            && b.kind != "cb"
        {
            let src: Option<(Option<String>, Option<String>)> = c.row(
                "SELECT content, content_source FROM resources WHERE id = ?1",
                [r],
                |r| Ok((r.get(0)?, r.get(1)?)),
            );
            if let Some((Some(key), source)) = src {
                line += &format!("  contents {} ({})", short(&key), source.unwrap_or_default());
            }
            // Written earlier in the frame: its contents at this draw are not those of the frame's start.
            let earlier: Option<i64> = c
                .row(
                    "SELECT max(event) FROM event_resources WHERE resource = ?1 AND access = 'write' AND event < ?2",
                    [r, event],
                    |r| r.get(0),
                )
                .flatten();
            if let Some(e) = earlier {
                line += &format!(", last written at event {e}");
            }
        }
        out!("{line}");
    }

    let mut reflection = Reflection::default();
    for b in list.iter().filter(|b| b.kind == "cb") {
        constants(c, &mut reflection, n, b, all)?;
    }

    let targets: Vec<(String, Option<String>, i64, i64, i64)> = c.rows(
        "SELECT target, blob, width, height, format FROM targets WHERE draw = ?1",
        [n],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    );
    for (t, blob, w, h, f) in targets {
        out!(
            "after it, {t} read back: {w}x{h} {} {}",
            format::name(f as u32),
            blob.as_deref().unwrap_or("-")
        );
    }
    Ok(())
}

/// Prints a bound constant buffer register by register, with names from the shader's reflection. Without `all`,
/// only the registers holding variables the shader uses.
fn constants(
    c: &Capture,
    reflection: &mut Reflection,
    draw: i64,
    b: &Binding,
    all: bool,
) -> Result<(), String> {
    let Some(key) = &b.content else {
        out!("{} b{}: contents not captured", b.stage, b.slot);
        return Ok(());
    };
    let bytes: Vec<u8> = c
        .row("SELECT bytes FROM cb_data WHERE hash = ?1", [key], |r| r.get(0))
        .ok_or_else(|| format!("constant buffer contents {key} missing"))?;
    let shader = shader_of(c, draw, &b.stage);
    let cb = shader
        .as_deref()
        .and_then(|s| reflection.cb(c, s, b.slot, b.space));
    out!(
        "{} b{} ({}, {} bytes{}):",
        b.stage,
        b.slot,
        cb.map_or("no reflection".into(), |cb| cb.name.clone()),
        bytes.len(),
        b.resource.map_or(String::new(), |r| format!(", buffer #{r}"))
    );
    let used: Vec<(u32, u32)> = cb
        .map(|cb| {
            cb.variables
                .iter()
                .filter(|v| v.used)
                .map(|v| (v.offset, v.offset + v.size))
                .collect()
        })
        .unwrap_or_default();
    let mut skipped = 0;
    for (r, reg) in bytes.chunks(16).enumerate() {
        let at = r as u32 * 16;
        let show = all || cb.is_none() || used.iter().any(|&(a, e)| a < at + 16 && at < e);
        if !show {
            skipped += 1;
            continue;
        }
        let mut names: [Option<String>; 4] = Default::default();
        let mut values = Vec::new();
        for (i, word) in reg.chunks(4).enumerate() {
            let named = cb.and_then(|cb| cb.at(at + i as u32 * 4));
            let base = named.as_ref().map(|n| n.1);
            names[i] = named.map(|n| n.0);
            let mut w = [0u8; 4];
            w[..word.len()].copy_from_slice(word);
            values.push(value(w, base));
        }
        out!(
            "  c{r:<4}{:<53}  {}",
            values.iter().map(|v| format!(" {v:>12}")).collect::<String>(),
            register_name(&names)
        );
    }
    if skipped > 0 {
        out!("  ({skipped} registers the shader does not use; --all shows them)");
    }
    Ok(())
}

pub fn cb(c: &Capture, draw: i64, stage: &str, slot: u32, raw: bool) -> Result<(), String> {
    let b = bindings(c, draw)
        .into_iter()
        .find(|b| b.kind == "cb" && b.stage == stage && b.slot == slot)
        .ok_or_else(|| format!("draw {draw} has no {stage} b{slot}"))?;
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
    constants(c, &mut Reflection::default(), draw, &b, true)
}

/// Draws whose bound constants hold `target` (as a float, or an integer with `int`).
pub fn find(c: &Capture, target: f64, tolerance: f64, int: bool) -> Result<(), String> {
    let data: Vec<(String, Vec<u8>)> = c.rows("SELECT hash, bytes FROM cb_data", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
    });
    let mut hits: HashMap<String, Vec<u32>> = HashMap::new();
    for (key, bytes) in &data {
        for (i, &w) in bytes.as_chunks::<4>().0.iter().enumerate() {
            let matched = if int {
                f64::from(i32::from_le_bytes(w)) == target || f64::from(u32::from_le_bytes(w)) == target
            } else {
                let v = f64::from(f32::from_le_bytes(w));
                (v - target).abs() <= tolerance * target.abs().max(1.0)
            };
            if matched {
                hits.entry(key.clone()).or_default().push(i as u32 * 4);
            }
        }
    }
    if hits.is_empty() {
        out!("no bound constant holds {target}");
        return Ok(());
    }
    let mut reflection = Reflection::default();
    // (stage, slot, shader, offset, name) -> draws
    let mut found: BTreeMap<(String, u32, String, u32, String), Vec<i64>> = BTreeMap::new();
    for (key, offsets) in &hits {
        let users: Vec<(i64, String, i64, i64)> = c.rows(
            "SELECT draw, stage, slot, space FROM bindings WHERE kind = 'cb' AND content = ?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        );
        for (draw, stage, slot, space) in users {
            let shader = shader_of(c, draw, &stage).unwrap_or_default();
            for &o in offsets {
                let name = reflection
                    .cb(c, &shader, slot as u32, space as u32)
                    .and_then(|cb| cb.name_at(o))
                    .unwrap_or_else(|| "?".into());
                found
                    .entry((stage.clone(), slot as u32, shader.clone(), o, name))
                    .or_default()
                    .push(draw);
            }
        }
    }
    out!(
        "{:<4} {:<5} {:<16} {:<10} {:<40} draws",
        "stage",
        "slot",
        "shader",
        "register",
        "name"
    );
    for ((stage, slot, shader, o, name), mut draws) in found {
        draws.sort_unstable();
        draws.dedup();
        let listed: Vec<String> = draws.iter().take(12).map(i64::to_string).collect();
        out!(
            "{stage:<5} b{slot:<4} {shader:<16} c{}.{:<8} {name:<40} {}{}",
            o / 16,
            ["x", "y", "z", "w"][(o % 16 / 4) as usize],
            listed.join(" "),
            if draws.len() > 12 {
                format!(" ... ({} draws)", draws.len())
            } else {
                String::new()
            }
        );
    }
    Ok(())
}

pub fn passes(c: &Capture) -> Result<(), String> {
    let rows: Vec<(i64, i64, i64)> = c.rows(
        "SELECT n, first_draw, last_draw FROM passes ORDER BY n",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    );
    out!("{:>4} {:>11} {:>5}  targets", "pass", "draws", "count");
    for (n, first, last) in rows {
        let targets: Vec<String> = c.rows(
            "SELECT kind, slot, resource FROM bindings WHERE draw = ?1 AND kind IN ('rtv', 'dsv') ORDER BY kind DESC, slot",
            [first],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<i64>>(2)?)),
        )
        .into_iter()
        .map(|(kind, slot, res)| {
            let name = if kind == "dsv" { "depth".to_string() } else { format!("rt{slot}") };
            let s = res.and_then(|r| shape(c, r)).map_or("?".into(), |s| s.to_string());
            format!("{name} {s}")
        })
        .collect();
        let count = c.count("SELECT count(*) FROM draws WHERE pass = ?1", [n]);
        out!(
            "{n:>4} {:>11} {count:>5}  {}",
            format!("{first}-{last}"),
            targets.join(", ")
        );
    }
    Ok(())
}
