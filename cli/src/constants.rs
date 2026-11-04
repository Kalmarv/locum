//! `constants`: named constants across draws, and across captures: every word of the bound constant buffers whose
//! name in the program's reflection matches (`SC_CameraPos`, `BloodAmount|TexScroll`), one tab-separated row each, or
//! each name's distinct values with how many draws read them. What a census of one constant over a frame, or a camera
//! over several frames, took a script over `cb_data` and the reflection for.

use std::collections::BTreeMap;

use locum_capture::rdef::{ConstantBuffer, Type};

use crate::describe::Reflection;
use crate::draw::{constants_of, rows};
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
}

impl Filter {
    pub fn new(
        name: &str,
        draws: Option<(i64, i64)>,
        stage: Option<String>,
        distinct: bool,
        extend: bool,
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

/// One capture's matching words: (draw, pass, stage, slot, buffer, register, name, value text, hex).
type Found = (
    i64,
    Option<i64>,
    String,
    u32,
    String,
    String,
    String,
    String,
    String,
);

fn found(c: &Capture, f: &Filter) -> Result<(Vec<Found>, usize), String> {
    let (first, last) = f.draws.unwrap_or((i64::MIN, i64::MAX));
    let draws: Vec<(i64, Option<i64>)> = c.rows(
        "SELECT n, pass FROM draws WHERE n BETWEEN ?1 AND ?2 ORDER BY n",
        [first, last],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let mut reflection = Reflection::default();
    let mut out = Vec::new();
    let mut uncaptured = 0;
    for (n, pass) in draws {
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
            if b.content.is_none() {
                uncaptured += 1;
                continue;
            }
            let k = constants_of(c, &mut reflection, n, b, false, f.extend)?;
            let buffer = k.name.clone().unwrap_or_default();
            for w in k.words {
                let Some(name) = w.name.filter(|name| f.matches(name)) else {
                    continue;
                };
                out.push((
                    n,
                    pass,
                    b.stage.clone(),
                    b.slot,
                    buffer.clone(),
                    w.register,
                    name,
                    w.text.trim().to_string(),
                    w.hex,
                ));
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
    if f.distinct {
        out!("{head}name\tvalue\thex\tdraws\tfirst\tlast");
    } else {
        out!("{head}draw\tpass\tstage\tslot\tbuffer\tregister\tname\tvalue\thex");
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
            // name -> (value, hex) -> draws reading it
            let mut by: BTreeMap<String, BTreeMap<(String, String), Vec<i64>>> = BTreeMap::new();
            for (draw, _, _, _, _, _, name, value, hex) in words {
                by.entry(name)
                    .or_default()
                    .entry((value, hex))
                    .or_default()
                    .push(draw);
            }
            for (name, values) in by {
                for ((value, hex), mut draws) in values {
                    draws.sort_unstable();
                    draws.dedup();
                    out!(
                        "{}{name}\t{value}\t{hex}\t{}\t{}\t{}",
                        lead(capture),
                        draws.len(),
                        draws[0],
                        draws[draws.len() - 1]
                    );
                }
            }
        } else {
            for (draw, pass, stage, slot, buffer, register, name, value, hex) in words {
                out!(
                    "{}{draw}\t{}\t{stage}\t{slot}\t{buffer}\t{register}\t{name}\t{value}\t{hex}",
                    lead(capture),
                    pass.map_or("-".into(), |p| p.to_string())
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
