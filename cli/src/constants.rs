//! `constants`: named constants across draws, and across captures: every word of the bound constant buffers whose
//! name in the program's reflection matches (`SC_CameraPos`, `BloodAmount|TexScroll`), one tab-separated row each, or
//! each name's distinct values with how many draws read them. What a census of one constant over a frame, or a camera
//! over several frames, took a script over `cb_data` and the reflection for.

use std::collections::BTreeMap;

use locum_capture::rdef::{ConstantBuffer, Type};

use crate::describe::Reflection;
use crate::describe::short;
use crate::draw::{Lookup, constants_of, file_of, layout_slots, read_vertices, rows};
use crate::draws::shader_of;
use crate::open::Capture;
use crate::out;

pub struct Filter {
    /// Parts of the constant's full name (`cbSceneParam.SC_CameraPos.x`), any case, separated by `|`.
    pub name: Vec<String>,
    /// Draws `first..=last`.
    pub draws: Option<(i64, i64)>,
    pub stage: Option<String>,
    /// Each name's distinct values and the draws that read each, instead of a row per draw.
    pub distinct: bool,
    /// Name ranges bound larger than declared as further elements of the buffer's last array.
    pub extend: bool,
    /// Columns of the draw to add to each row (and, with `distinct`, to group by): `WITH`.
    pub with: Vec<String>,
}

/// What `--with` can add: a stage's program, the stencil reference, the topology, the file the draw's mesh was made
/// from.
pub const WITH: [&str; 9] = ["vs", "hs", "ds", "gs", "ps", "cs", "stencil", "topology", "file"];

/// The `with` columns of draw `n`: a program by its name (or hash), the rest as `draw` shows them; `-` for none.
fn draw_columns(c: &Capture, n: i64, with: &[String], reflection: &mut Reflection) -> Vec<String> {
    with.iter()
        .map(|w| match w.as_str() {
            "stencil" | "topology" => {
                let column = if w == "stencil" { "stencil_ref" } else { "topology" };
                c.row(&format!("SELECT {column} FROM draws WHERE n = ?1"), [n], |r| {
                    r.get::<_, Option<i64>>(0)
                })
                .flatten()
                .map_or("-".into(), |v| v.to_string())
            }
            "file" => mesh_file(c, n).unwrap_or_else(|| "-".into()),
            stage => match shader_of(c, n, stage) {
                Some(h) => reflection.name(c, &h).unwrap_or_else(|| short(&h).to_string()),
                None => "-".into(),
            },
        })
        .collect()
}

/// The file draw `n`'s mesh was made from, by the indexes given: its index buffer's range, else the first vertex
/// buffer its layout reads.
fn mesh_file(c: &Capture, n: i64) -> Option<String> {
    let (call, args, layout): (String, String, Option<i64>) = c.row(
        "SELECT call, coalesce(args, ''), layout FROM draws WHERE n = ?1",
        [n],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    let bound = rows(c, n);
    let slots = layout_slots(c, layout);
    let vbs = read_vertices(&bound, slots.as_deref());
    bound
        .iter()
        .filter(|b| b.kind == "ib")
        .chain(
            bound
                .iter()
                .filter(|b| b.kind == "vb" && slots.as_ref().is_none_or(|s| s.contains(&b.slot))),
        )
        .find_map(|b| file_of(c, b, &call, &args, &vbs))
}

impl Filter {
    pub fn new(
        name: &str,
        draws: Option<(i64, i64)>,
        stage: Option<String>,
        distinct: bool,
        extend: bool,
        with: Vec<String>,
    ) -> Filter {
        Filter {
            name: name
                .split('|')
                .map(|p| p.trim().to_lowercase())
                .filter(|p| !p.is_empty())
                .collect(),
            draws,
            stage,
            distinct,
            extend,
            with,
        }
    }

    fn matches(&self, name: &str) -> bool {
        let name = name.to_lowercase();
        self.name.iter().any(|p| name.contains(p))
    }

    /// Whether a word of buffer `cb` can match: every identifier of a part (`VC_SpeedTreeWind`, `u_vDirection` of
    /// `VC_SpeedTreeWind[1].u_vDirection`) is in one of the buffer's names, its own, its variables' or their members'.
    fn may_match(&self, cb: &ConstantBuffer) -> bool {
        let mut names = vec![cb.name.to_lowercase()];
        fn walk(ty: &Type, names: &mut Vec<String>) {
            for m in &ty.members {
                names.push(m.name.to_lowercase());
                walk(&m.ty, names);
            }
        }
        for v in &cb.variables {
            names.push(v.name.to_lowercase());
            walk(&v.ty, &mut names);
        }
        self.name.iter().any(|p| {
            p.split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                // array indices and components are not names
                .filter(|t| !t.is_empty() && !t.chars().all(|ch| ch.is_ascii_digit()))
                .filter(|t| !matches!(*t, "x" | "y" | "z" | "w"))
                .all(|t| names.iter().any(|n| n.contains(t)))
        })
    }
}

/// One matching word of a capture.
struct Found {
    draw: i64,
    pass: Option<i64>,
    stage: String,
    slot: u32,
    buffer: String,
    register: String,
    name: String,
    value: String,
    hex: String,
    /// The `--with` columns.
    with: Vec<String>,
}

fn found(c: &Capture, f: &Filter) -> Result<(Vec<Found>, usize), String> {
    let (first, last) = f.draws.unwrap_or((i64::MIN, i64::MAX));
    let draws: Vec<(i64, Option<i64>)> = c.rows(
        "SELECT n, pass FROM draws WHERE n BETWEEN ?1 AND ?2 ORDER BY n",
        [first, last],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let mut reflection = Reflection::default();
    let lookup = Lookup::new(f.extend, None);
    let mut out = Vec::new();
    let mut uncaptured = 0;
    for (n, pass) in draws {
        let mut with: Option<Vec<String>> = None;
        for b in rows(c, n).iter().filter(|b| b.kind == "cb") {
            if f.stage.as_ref().is_some_and(|s| *s != b.stage) {
                continue;
            }
            // Only the buffers whose declared variables can match: reading every buffer's words is the slow part.
            let Some(shader) = shader_of(c, n, &b.stage) else {
                continue;
            };
            let Some(cb) = reflection.cb(c, &shader, b.slot, b.space) else {
                continue;
            };
            if !f.may_match(cb) {
                continue;
            }
            let k = constants_of(c, &mut reflection, n, b, false, &lookup)?;
            if k.why.is_some() {
                uncaptured += 1;
                continue;
            }
            let buffer = k.name.clone().unwrap_or_default();
            for w in k.words {
                let Some(name) = w.name.filter(|name| f.matches(name)) else {
                    continue;
                };
                let with = with
                    .get_or_insert_with(|| draw_columns(c, n, &f.with, &mut reflection))
                    .clone();
                out.push(Found {
                    draw: n,
                    pass,
                    stage: b.stage.clone(),
                    slot: b.slot,
                    buffer: buffer.clone(),
                    register: w.register,
                    name,
                    value: w.text.trim().to_string(),
                    hex: w.hex,
                    with,
                });
            }
        }
    }
    Ok((out, uncaptured))
}

/// The constants named like `f.name` in each capture of `captures` (opened by `open`).
pub fn constants(
    captures: &[String],
    open: impl Fn(&str) -> Result<Capture, String>,
    f: &Filter,
) -> Result<(), String> {
    if f.name.is_empty() {
        return Err("--name: give a part of a constant's name".into());
    }
    let several = captures.len() > 1;
    let lead = |capture: &str| {
        if several {
            format!("{capture}\t")
        } else {
            String::new()
        }
    };
    let head = if several { "capture\t" } else { "" };
    let extra: String = f.with.iter().map(|w| format!("\t{w}")).collect();
    if f.distinct {
        out!("{head}name\tvalue\thex{extra}\tdraws\tfirst\tlast");
    } else {
        out!("{head}draw\tpass\tstage\tslot\tbuffer\tregister\tname\tvalue\thex{extra}");
    }
    let mut any = false;
    for capture in captures {
        let c = open(capture)?;
        let (words, uncaptured) = found(&c, f)?;
        if uncaptured > 0 {
            eprintln!(
                "locum-cli: {capture}: {uncaptured} bound buffers declaring a matching name have no contents recorded \
                 (`info` says why)"
            );
        }
        any |= !words.is_empty();
        if f.distinct {
            // name -> (value, hex, with) -> draws reading it
            type Values = BTreeMap<(String, String, Vec<String>), Vec<i64>>;
            let mut by: BTreeMap<String, Values> = BTreeMap::new();
            for w in words {
                by.entry(w.name)
                    .or_default()
                    .entry((w.value, w.hex, w.with))
                    .or_default()
                    .push(w.draw);
            }
            for (name, values) in by {
                for ((value, hex, with), mut draws) in values {
                    draws.sort_unstable();
                    draws.dedup();
                    let with: String = with.iter().map(|w| format!("\t{w}")).collect();
                    out!(
                        "{}{name}\t{value}\t{hex}{with}\t{}\t{}\t{}",
                        lead(capture),
                        draws.len(),
                        draws[0],
                        draws[draws.len() - 1]
                    );
                }
            }
        } else {
            for w in words {
                let with: String = w.with.iter().map(|v| format!("\t{v}")).collect();
                out!(
                    "{}{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}{with}",
                    lead(capture),
                    w.draw,
                    w.pass.map_or("-".into(), |p| p.to_string()),
                    w.stage,
                    w.slot,
                    w.buffer,
                    w.register,
                    w.name,
                    w.value,
                    w.hex
                );
            }
        }
    }
    if !any {
        eprintln!(
            "locum-cli: no constant named like that is bound with contents{}",
            if f.extend || !f.name.iter().any(|p| p.contains('[')) {
                ""
            } else {
                " (an element past an array's declared length is named with --extend)"
            }
        );
    }
    Ok(())
}
