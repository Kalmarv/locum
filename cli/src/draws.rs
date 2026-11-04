//! `draws`, `find` and `passes`: the frame's draws and dispatches, which constants hold a value, and the passes.

use std::collections::{BTreeMap, HashMap};

use rusqlite::types::Value;

use crate::describe::{self, Reflection, shape};
use crate::open::Capture;
use crate::out;

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
    /// A part of a program's debug name, matched in any case.
    pub name: Option<String>,
    /// A resource or constant buffer name a program declares, matched whole in any case.
    pub binds: Option<String>,
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
        "SELECT d.n, d.pass, d.call, d.args, e.deferred, e.list, d.vs, d.hs, d.ds, d.gs, d.ps, d.cs FROM draws d \
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
        [Option<String>; 6],
    );
    let rows: Vec<Row> = c.rows(&sql, rusqlite::params_from_iter(params), |r| {
        Ok((
            r.get(0)?,
            r.get(1)?,
            r.get(2)?,
            r.get(3)?,
            r.get(4)?,
            r.get(5)?,
            [r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?],
        ))
    });
    // Each program's debug name, read once per program.
    let mut reflection = Reflection::default();
    let wanted = f.name.as_ref().map(|n| n.to_lowercase());
    let rows: Vec<(Row, Vec<String>)> = rows
        .into_iter()
        .filter_map(|row| {
            let names: Vec<(&str, String)> = SHADER_COLUMNS
                .iter()
                .zip(&row.6)
                .filter_map(|(stage, h)| Some((*stage, reflection.name(c, h.as_ref()?)?.to_string())))
                .collect();
            if let Some(w) = &wanted
                && !names.iter().any(|(_, n)| n.to_lowercase().contains(w))
            {
                return None;
            }
            if let Some(b) = &f.binds
                && !row.6.iter().flatten().any(|h| reflection.declares(c, h, b))
            {
                return None;
            }
            let names = names.into_iter().map(|(s, n)| format!("{s} {n}")).collect();
            Some((row, names))
        })
        .collect();
    out!(
        "{:>5} {:>4} {:<26} {:<8} {:<16} {:<16} targets",
        "draw",
        "pass",
        "call",
        "context",
        "vs (cs)",
        "ps"
    );
    for ((n, pass, call, args, deferred, list, [vs, _, _, _, ps, cs]), names) in &rows {
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
            "{n:>5} {:>4} {:<26} {context:<8} {:<16} {:<16} {}{}",
            pass.map_or("-".into(), |p| p.to_string()),
            format!("{call} {}", describe::call_args(call, args)),
            first.map_or("-", String::as_str),
            ps.as_deref().unwrap_or("-"),
            targets.join(" "),
            if names.is_empty() {
                String::new()
            } else {
                format!("  ({})", names.join(", "))
            }
        );
    }
    out!("{} draws", rows.len());
    Ok(())
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
    // (stage, slot, space, shader, offset, name) -> draws
    type ConstantLocation = (String, u32, u32, String, u32, String);
    let mut found: BTreeMap<ConstantLocation, Vec<i64>> = BTreeMap::new();
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
                    .entry((stage.clone(), slot as u32, space as u32, shader.clone(), o, name))
                    .or_default()
                    .push(draw);
            }
        }
    }
    out!(
        "{:<4} {:<5} {:<6} {:<16} {:<10} {:<40} draws",
        "stage",
        "slot",
        "space",
        "shader",
        "register",
        "name"
    );
    for ((stage, slot, space, shader, o, name), mut draws) in found {
        draws.sort_unstable();
        draws.dedup();
        let listed: Vec<String> = draws.iter().take(12).map(i64::to_string).collect();
        out!(
            "{stage:<5} b{slot:<4} {space:<6} {shader:<16} c{}.{:<8} {name:<40} {}{}",
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

/// One pass as `passes` lists it: its draws, where it was recorded, and its targets (name and shape, and resources).
struct Pass {
    n: i64,
    first: i64,
    last: i64,
    place: String,
    targets: Vec<String>,
    resources: Vec<i64>,
}

pub fn passes(c: &Capture) -> Result<(), String> {
    let rows: Vec<(i64, i64, i64, bool, Option<i64>)> = c.rows(
        "SELECT p.n, p.first_draw, p.last_draw, e.deferred, e.list FROM passes p JOIN draws d ON d.n = p.first_draw \
         JOIN events e ON e.seq = d.event ORDER BY p.n",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    );
    let list: Vec<Pass> = rows
        .into_iter()
        .map(|(n, first, last, deferred, list)| {
            let bound: Vec<(String, i64, Option<i64>)> = c.rows(
                "SELECT kind, slot, resource FROM bindings WHERE draw = ?1 AND kind IN ('rtv', 'dsv') ORDER BY kind DESC, slot",
                [first],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            );
            let targets = bound
                .iter()
                .map(|(kind, slot, res)| {
                    let name = if kind == "dsv" { "depth".to_string() } else { format!("rt{slot}") };
                    let s = res.and_then(|r| shape(c, r)).map_or("?".into(), |s| s.to_string());
                    format!("{name} {s}")
                })
                .collect();
            let place = match (deferred, list) {
                (true, Some(l)) => format!("list {l}"),
                (true, None) => "list".into(),
                _ => "imm".into(),
            };
            Pass {
                n,
                first,
                last,
                place,
                targets,
                resources: bound.iter().filter_map(|b| b.2).collect(),
            }
        })
        .collect();
    out!(
        "{:>4} {:>11} {:>5} {:<9} targets",
        "pass",
        "draws",
        "count",
        "list"
    );
    let missing = c.has_table("missing_targets");
    // A capture that reads back no target says so once (`info` too), not once a pass.
    let none = c.count("SELECT count(*) FROM targets", []) == 0;
    if none {
        out!("(no target was read back in this capture)");
    }
    // Why a capture from before the per-pass reasons read back fewer targets: its gaps, which count them.
    let gaps: Vec<String> = if missing {
        Vec::new()
    } else {
        c.rows(
            "SELECT detail FROM gaps WHERE what LIKE '%targets%' AND detail IS NOT NULL",
            [],
            |r| r.get(0),
        )
    };
    for (i, p) in list.iter().enumerate() {
        let count = c.count("SELECT count(*) FROM draws WHERE pass = ?1", [p.n]);
        // Games that record one pass on several lists (Elden Ring's G-buffer) split it where each list ends.
        let shown = if i > 0 && list[i - 1].targets == p.targets && !p.targets.is_empty() {
            "the same targets as the pass before".to_string()
        } else {
            p.targets.join(", ")
        };
        out!(
            "{:>4} {:>11} {count:>5} {:<9} {shown}",
            p.n,
            format!("{}-{}", p.first, p.last),
            p.place
        );
        // Targets of this pass not read back where it ended, by reason.
        let lost: Vec<(String, String)> = if missing {
            c.rows(
                "SELECT why, group_concat(DISTINCT target) FROM missing_targets WHERE draw BETWEEN ?1 AND ?2 \
                 GROUP BY why",
                [p.first, p.last],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        } else {
            Vec::new()
        };
        let note = |text: &str| out!("{:>4} {:>11} {:>5} {:<9} {text}", "", "", "", "");
        for (why, targets) in &lost {
            note(&format!("not read back: {targets}: {why}"));
        }
        if lost.is_empty()
            && !none
            && let Some(text) = readback(c, &list, i, &gaps)
        {
            note(&text);
        }
    }
    Ok(())
}

/// Where the targets of pass `i` were read back when none was after its own draws: with a later pass its targets go
/// on into (a pass recorded on several lists is read where the last ends), or not at all, with the capture's reason
/// when it gives one for the whole capture (`gaps`) rather than per pass.
fn readback(c: &Capture, passes: &[Pass], i: usize, gaps: &[String]) -> Option<String> {
    let p = &passes[i];
    let read = c.count(
        "SELECT count(*) FROM targets WHERE draw BETWEEN ?1 AND ?2",
        [p.first, p.last],
    );
    if p.resources.is_empty() || read > 0 {
        return None;
    }
    let ids = p
        .resources
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    let next: Option<(i64, i64)> = c.row(
        &format!(
            "SELECT t.draw, d.pass FROM targets t JOIN draws d ON d.n = t.draw WHERE t.draw > ?1 \
             AND t.resource IN ({ids}) ORDER BY t.draw LIMIT 1"
        ),
        [p.last],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let why = if gaps.is_empty() {
        String::new()
    } else {
        format!(" ({}; the capture does not say which passes)", gaps.join("; "))
    };
    Some(match next {
        Some((draw, pass))
            if passes
                .iter()
                .filter(|q| q.n > p.n && q.n <= pass)
                .all(|q| q.targets == p.targets) =>
        {
            format!("read back with pass {pass}, after draw {draw}: the same targets go on to there")
        }
        Some((draw, pass)) => {
            format!(
                "not read back where it ends{why}; its targets are next read back after draw {draw}, pass {pass}"
            )
        }
        None => format!("not read back{why}"),
    })
}
