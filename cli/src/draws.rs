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
            format!("{call} {}", describe::call_args(call, args)),
            first.map_or("-", String::as_str),
            ps.as_deref().unwrap_or("-"),
            targets.join(" ")
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
