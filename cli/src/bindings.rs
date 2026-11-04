//! `bindings`: every register the frame's draws had bound, one row each across draws, with what `draw` adds to the
//! capture's rows: the program's name for the register and whether it reads it, the resource's shape and the file an
//! index names, the command list, and the last write before the draw. A census over a frame (which draws read
//! `g_SSSStrengthTexture`, where a CPU-written buffer is bound in each list) is one command, not a script over
//! `bindings`, `states` and the reflection.

use std::collections::HashMap;

use serde::Serialize;

use crate::describe::{Reflection, shape};
use crate::draw::{file_of, layout_slots, read_vertices, register, rows};
use crate::open::Capture;
use crate::out;

/// Which rows to show.
pub struct Filter {
    /// Draws `first..=last`.
    pub draws: Option<(i64, i64)>,
    pub resource: Option<i64>,
    pub kind: Option<String>,
    pub stage: Option<String>,
    /// Part of the register's name in the program (any case).
    pub name: Option<String>,
    /// Part of the name of the file an index names (any case).
    pub file: Option<String>,
    /// Only the registers the program reads (or the layout, for vertex buffers), or `false` only those it does not.
    pub read: Option<bool>,
}

#[derive(Serialize)]
struct Line {
    draw: i64,
    pass: Option<i64>,
    /// The command list the draw was recorded in: a D3D12 list, or a D3D11 deferred context's.
    list: Option<i64>,
    stage: String,
    kind: String,
    slot: u32,
    space: u32,
    /// The program's name for the register.
    name: Option<String>,
    /// Whether the program (or, for a vertex buffer, the input layout) reads it; null when no reflection says.
    read: Option<bool>,
    resource: Option<i64>,
    shape: Option<String>,
    file: Option<String>,
    offset: Option<i64>,
    size: Option<i64>,
    stride: Option<i64>,
    /// The last event before the draw that wrote the resource: its contents at the draw are not those the frame
    /// began with.
    written_before: Option<i64>,
}

pub fn bindings(c: &Capture, f: &Filter, json: bool) -> Result<(), String> {
    let (first, last) = f.draws.unwrap_or((i64::MIN, i64::MAX));
    // D3D12 captures from before the queue's record have no list locations.
    let d3d12 = c.meta("api").as_deref() == Some("d3d12") && c.has_table("d12_event_locations");
    // n, event, pass, list, call, args, input layout
    type Head = (i64, i64, Option<i64>, Option<i64>, String, String, Option<i64>);
    let draws: Vec<Head> = c.rows(
        &format!(
            "SELECT d.n, d.event, d.pass, {}, d.call, coalesce(d.args, ''), d.layout FROM draws d \
             JOIN events e ON e.seq = d.event WHERE d.n BETWEEN ?1 AND ?2 {} ORDER BY d.n",
            if d3d12 {
                "(SELECT x.list FROM d12_event_locations l JOIN d12_executions x ON x.id = l.execution \
                 WHERE l.event = d.event)"
            } else if c.meta("api").as_deref() == Some("d3d12") {
                "NULL"
            } else {
                "CASE WHEN e.deferred THEN e.list END"
            },
            if f.resource.is_some() {
                "AND d.n IN (SELECT draw FROM bindings WHERE resource = ?3)"
            } else {
                "AND ?3 IS NULL"
            }
        ),
        rusqlite::params![first, last, f.resource],
        |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
            ))
        },
    );
    // Each written resource's write events in order, read once: the last write before a draw is a search.
    let mut writes: HashMap<i64, Vec<i64>> = HashMap::new();
    for (resource, event) in c.rows(
        "SELECT resource, event FROM event_resources WHERE access = 'write' ORDER BY resource, event",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    ) {
        writes.entry(resource).or_default().push(event);
    }
    let mut shapes: HashMap<i64, Option<String>> = HashMap::new();
    let mut reflection = Reflection::default();
    let lower = |s: &Option<String>| s.as_ref().map(|s| s.to_lowercase());
    let (name_part, file_part) = (lower(&f.name), lower(&f.file));
    if !json {
        out!(
            "draw\tpass\tlist\tstage\tkind\tslot\tspace\tread\tname\tresource\tshape\tfile\toffset\tsize\tstride\twritten_before"
        );
    }
    let mut shown = 0usize;
    for (n, event, pass, list, call, args, layout) in draws {
        let rows = rows(c, n);
        let slots = layout_slots(c, layout);
        let vbs = read_vertices(&rows, slots.as_deref());
        for b in &rows {
            if f.resource.is_some_and(|r| b.resource != Some(r))
                || f.kind.as_ref().is_some_and(|k| *k != b.kind)
                || f.stage.as_ref().is_some_and(|s| *s != b.stage)
            {
                continue;
            }
            let (name, read) = register(c, n, b, slots.as_deref(), &mut reflection);
            if f.read.is_some_and(|want| read != Some(want))
                || name_part.as_ref().is_some_and(|p| {
                    !name.as_ref().is_some_and(|n| n.to_lowercase().contains(p))
                })
            {
                continue;
            }
            let file = file_of(c, b, &call, &args, &vbs);
            if file_part.as_ref().is_some_and(|p| {
                !file.as_ref().is_some_and(|f| f.to_lowercase().contains(p))
            }) {
                continue;
            }
            let shape = b.resource.and_then(|r| {
                shapes
                    .entry(r)
                    // without the `#id` the resource column has
                    .or_insert_with(|| {
                        shape(c, r).map(|s| {
                            let s = s.to_string();
                            s.strip_prefix(&format!("#{r} ")).map_or(s.clone(), str::to_string)
                        })
                    })
                    .clone()
            });
            let written_before = b.resource.and_then(|r| {
                let w = writes.get(&r)?;
                let i = w.partition_point(|&e| e < event);
                i.checked_sub(1).map(|i| w[i])
            });
            let line = Line {
                draw: n,
                pass,
                list,
                stage: b.stage.clone(),
                kind: b.kind.clone(),
                slot: b.slot,
                space: b.space,
                name,
                read,
                resource: b.resource,
                shape,
                file,
                offset: b.offset,
                size: b.size,
                stride: b.stride,
                written_before,
            };
            shown += 1;
            if json {
                out!("{}", serde_json::to_string(&line).map_err(|e| e.to_string())?);
            } else {
                let o = |v: Option<i64>| v.map_or("-".to_string(), |v| v.to_string());
                let t = |v: &Option<String>| v.clone().unwrap_or_else(|| "-".into());
                out!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    line.draw,
                    o(line.pass),
                    o(line.list),
                    line.stage,
                    line.kind,
                    line.slot,
                    line.space,
                    match line.read {
                        Some(true) => "y",
                        Some(false) => "n",
                        None => "?",
                    },
                    t(&line.name),
                    o(line.resource),
                    t(&line.shape),
                    t(&line.file),
                    o(line.offset),
                    o(line.size),
                    o(line.stride),
                    o(line.written_before)
                );
            }
        }
    }
    if shown == 0 {
        eprintln!("locum-cli: no binding matches");
    }
    Ok(())
}
