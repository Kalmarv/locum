//! `upgrade`: what an older locum derived in a capture, brought up to the current locum from the capture's own
//! authoritative records. D3D12 bindings are projected again from each draw's root arguments and the descriptors of
//! its submission (`locum_capture::d3d12::reproject`): an older projection let a table range reach descriptors of
//! another type and bound root parameters to stages the pipeline has no program for, so its extra rows go and the rest
//! keep what the capture composed for them (constant-buffer contents). Structured buffer views get their element size,
//! and `shaders` the programs' debug names, where the capture predates them. What a capture never recorded (the bytes
//! of a buffer it did not copy) stays missing.
//!
//! The plan is worked out on the read-only capture; only then, and without `--check`, is it written, in one
//! transaction.

use std::collections::{HashMap, HashSet};

use rusqlite::Connection;

use crate::open::Capture;
use crate::out;

/// A shader-stage binding as both projections name it: draw, stage, kind, register, space, view, resource, offset,
/// and a sampler's description.
type Key = (
    i64,
    String,
    String,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Vec<u8>,
);

/// What `upgrade` changes.
#[derive(Default)]
struct Plan {
    /// Binding rows the current projection does not make.
    remove: Vec<i64>,
    /// Rows it makes that the capture lacks: the key, size and stride.
    add: Vec<(Key, Option<i64>, Option<i64>)>,
    /// Element sizes for rows that lack them, by rowid.
    strides: Vec<(i64, i64)>,
    /// Debug names for programs that lack them, by hash.
    names: Vec<(String, String)>,
    /// Whether `shaders` lacks the `name` column.
    name_column: bool,
}

pub fn upgrade(c: &Capture, check: bool) -> Result<(), String> {
    let mut plan = Plan::default();
    let d3d12 = c.meta("api").as_deref() == Some("d3d12");
    if d3d12 && c.has_table("d12_draw_state") && c.has_table("d12_submission_descriptors") {
        bindings(c, &mut plan)?;
        out!(
            "bindings: {} rows the current projection does not make, {} it makes that were missing",
            plan.remove.len(),
            plan.add.len()
        );
    } else if d3d12 {
        out!("bindings: this capture lacks the D3D12 records to project them again (d12_draw_state)");
    }
    // D3D11 structured buffer views' element size, from the buffer, as D3D11 captures now record it (a D3D12 view
    // carries its own, which the projection above gives).
    if !d3d12 {
        plan.strides = c.rows(
            "SELECT b.rowid, r.stride FROM bindings b JOIN resources r ON r.id = b.resource WHERE b.stride IS NULL \
             AND b.kind IN ('srv', 'uav') AND r.misc & 64 != 0 AND r.stride > 0",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        );
    }
    out!(
        "strides: {} structured buffer views given their element size",
        plan.strides.len()
    );
    // The programs' debug names, for SQL.
    plan.name_column = c.count(
        "SELECT count(*) FROM pragma_table_info('shaders') WHERE name = 'name'",
        [],
    ) == 0;
    let unnamed: Vec<(String, String)> = c.rows(
        if plan.name_column {
            "SELECT hash, blob FROM shaders WHERE blob IS NOT NULL"
        } else {
            "SELECT hash, blob FROM shaders WHERE blob IS NOT NULL AND name IS NULL"
        },
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    plan.names = unnamed
        .into_iter()
        .filter_map(|(hash, blob)| {
            let code = c.blob(&blob).ok()?;
            Some((hash, locum_capture::rdef::debug_name(&code)?))
        })
        .collect();
    out!(
        "shaders: {} given their debug name{}",
        plan.names.len(),
        if plan.name_column { " (and the column)" } else { "" }
    );
    if check {
        out!("(--check: nothing written)");
        return Ok(());
    }
    write(c, &plan)
}

/// Writes `plan` into the capture in one transaction, and marks the capture upgraded.
fn write(c: &Capture, plan: &Plan) -> Result<(), String> {
    let e = |e: rusqlite::Error| e.to_string();
    let mut db = Connection::open(&c.path).map_err(|e| format!("{}: {e}", c.path.display()))?;
    let tx = db.transaction().map_err(e)?;
    for rowid in &plan.remove {
        tx.execute("DELETE FROM bindings WHERE rowid = ?1", [rowid])
            .map_err(e)?;
    }
    for (k, size, stride) in &plan.add {
        tx.execute(
            "INSERT INTO bindings(draw, stage, kind, slot, space, view, resource, offset, size, stride) \
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![k.0, k.1, k.2, k.3, k.4, k.5, k.6, k.7, size, stride],
        )
        .map_err(e)?;
    }
    for (rowid, stride) in &plan.strides {
        tx.execute(
            "UPDATE bindings SET stride = ?2 WHERE rowid = ?1",
            [rowid, stride],
        )
        .map_err(e)?;
    }
    if plan.name_column {
        tx.execute("ALTER TABLE shaders ADD COLUMN name TEXT", [])
            .map_err(e)?;
    }
    for (hash, name) in &plan.names {
        tx.execute("UPDATE shaders SET name = ?2 WHERE hash = ?1", [hash, name])
            .map_err(e)?;
    }
    let done = serde_json::json!({
        "bindings_removed": plan.remove.len(),
        "bindings_added": plan.add.len(),
        "strides": plan.strides.len(),
        "names": plan.names.len(),
    });
    tx.execute(
        "INSERT OR REPLACE INTO meta VALUES('upgraded', ?1)",
        [done.to_string()],
    )
    .map_err(e)?;
    tx.commit().map_err(e)
}

/// Which of the capture's shader-stage binding rows the current projection makes (kept, given its stride where they
/// lack one), which it does not (removed), and which it makes that the capture lacks (added).
fn bindings(c: &Capture, plan: &mut Plan) -> Result<(), String> {
    let fresh =
        locum_capture::d3d12::reproject(&c.db).map_err(|e| format!("projecting the bindings again: {e}"))?;
    let n = |v: Option<u64>| v.map(|v| v as i64);
    let mut wanted: HashMap<Key, &locum_capture::db::Binding> = HashMap::new();
    for b in &fresh.bindings {
        let desc = match b.state.filter(|_| b.kind == "sampler") {
            Some(s) => fresh
                .states
                .iter()
                .find(|x| x.id == s)
                .map(|x| x.desc.clone())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let key = (
            b.draw as i64,
            b.stage.to_string(),
            b.kind.to_string(),
            i64::from(b.slot),
            i64::from(b.space),
            n(b.view),
            n(b.resource),
            n(b.offset),
            desc,
        );
        wanted.insert(key, b);
    }
    let states: HashMap<i64, Vec<u8>> = c
        .rows("SELECT id, desc FROM states WHERE desc IS NOT NULL", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .into_iter()
        .map(|(id, desc): (i64, Vec<u8>)| (id, canonical(desc)))
        .collect();
    let rows: Vec<(i64, Key, Option<i64>)> = c.rows(
        "SELECT rowid, draw, stage, kind, slot, space, view, resource, offset, state, stride FROM bindings \
         WHERE stage NOT IN ('ia', 'om', 'so')",
        [],
        |r| {
            let kind: String = r.get(3)?;
            let state: Option<i64> = r.get(9)?;
            let desc = match state.filter(|_| kind == "sampler") {
                Some(s) => states.get(&s).cloned().unwrap_or_default(),
                None => Vec::new(),
            };
            Ok((
                r.get(0)?,
                (
                    r.get(1)?,
                    r.get(2)?,
                    kind,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    desc,
                ),
                r.get(10)?,
            ))
        },
    );
    let mut kept: HashSet<Key> = HashSet::new();
    for (rowid, key, stride) in rows {
        match wanted.get(&key) {
            Some(b) if !kept.contains(&key) => {
                if stride.is_none()
                    && let Some(s) = b.stride
                {
                    plan.strides.push((rowid, i64::from(s)));
                }
                kept.insert(key);
            }
            _ => plan.remove.push(rowid),
        }
    }
    for (key, b) in wanted {
        if !kept.contains(&key) {
            plan.add.push((key, n(b.size), b.stride.map(i64::from)));
        }
    }
    Ok(())
}

/// A sampler description as the projection writes it: JSON parsed and written again, so the same floats compare equal
/// whichever formatter first wrote them (`3.4028234663852886e+38` and `3.402823466385289e+38`).
fn canonical(desc: Vec<u8>) -> Vec<u8> {
    match serde_json::from_slice::<serde_json::Value>(&desc) {
        Ok(v) => v.to_string().into_bytes(),
        Err(_) => desc,
    }
}
