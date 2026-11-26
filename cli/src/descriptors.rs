//! `descriptors`: what each register a D3D12 draw's or dispatch's root signature reaches pointed at as it was
//! submitted (`d12_draw_descriptors`): the heap slot and descriptor version, the resource, the GPU address, bytes,
//! element size and format of a buffer or constant buffer view, whether the program reads it, and what is wrong
//! with it (a null or zero-size view, an empty slot, an address in no buffer). `--flagged` lists only those.

use serde::Serialize;

use crate::describe::{Reflection, STAGE_ORDER};
use crate::draw::{Row, register};
use crate::open::Capture;
use crate::out;

/// Which rows to show.
pub struct Filter {
    /// Draws `first..=last`.
    pub draws: Option<(i64, i64)>,
    pub resource: Option<i64>,
    pub kind: Option<String>,
    pub stage: Option<String>,
    /// Only rows with this flag, or (`flagged`) with any.
    pub flag: Option<String>,
    pub flagged: bool,
    /// Only the registers the program reads.
    pub read: bool,
}

#[derive(Serialize)]
struct Line {
    draw: i64,
    stage: String,
    kind: String,
    slot: u32,
    space: u32,
    /// The program's name for the register, and whether it reads it (null when no reflection says).
    name: Option<String>,
    read: Option<bool>,
    parameter: i64,
    heap: Option<i64>,
    heap_slot: Option<i64>,
    version: Option<i64>,
    resource: Option<i64>,
    /// The GPU address, as hex.
    address: Option<String>,
    size: Option<i64>,
    stride: Option<i64>,
    format: Option<&'static str>,
    flag: Option<String>,
}

pub fn descriptors(c: &Capture, f: &Filter, json: bool) -> Result<(), String> {
    if c.meta("api").as_deref() != Some("d3d12") {
        return Err("descriptors: a D3D12 capture's (a D3D11 one's bindings are in `bindings`)".into());
    }
    if !c.has_table("d12_draw_descriptors") {
        return Err(
            "descriptors: this capture is from before locum recorded them: `locum-cli upgrade` projects them from its \
             records"
                .into(),
        );
    }
    let (first, last) = f.draws.unwrap_or((i64::MIN, i64::MAX));
    type Raw = (
        i64,
        String,
        String,
        i64,
        i64,
        i64,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
    );
    let rows: Vec<Raw> = c.rows(
        &format!(
            "SELECT draw, stage, kind, slot, space, parameter, heap, heap_slot, version, resource, address, size, \
             stride, format, flag FROM d12_draw_descriptors WHERE draw BETWEEN ?1 AND ?2 \
             AND (?3 IS NULL OR resource = ?3) AND (?4 IS NULL OR kind = ?4) AND (?5 IS NULL OR stage = ?5) \
             AND (?6 IS NULL OR flag = ?6) AND (?7 = 0 OR flag IS NOT NULL) \
             ORDER BY draw, {STAGE_ORDER}, kind, space, slot"
        ),
        rusqlite::params![first, last, f.resource, f.kind, f.stage, f.flag, f.flagged],
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
                r.get(10)?,
                r.get(11)?,
                r.get(12)?,
                r.get(13)?,
                r.get(14)?,
            ))
        },
    );
    if !json {
        out!(
            "draw\tstage\tkind\tslot\tspace\tread\tname\tparameter\theap\theap_slot\tversion\tresource\taddress\tsize\tstride\tformat\tflag"
        );
    }
    let mut reflection = Reflection::default();
    let mut shown = 0usize;
    for (
        draw,
        stage,
        kind,
        slot,
        space,
        parameter,
        heap,
        heap_slot,
        version,
        resource,
        address,
        size,
        stride,
        format,
        flag,
    ) in rows
    {
        let row = Row {
            stage: stage.clone(),
            kind: kind.clone(),
            slot: slot as u32,
            space: space as u32,
            view: None,
            resource,
            state: None,
            offset: None,
            size,
            stride,
            format,
            content: None,
        };
        let (name, read) = register(c, draw, &row, None, &mut reflection);
        if f.read && read != Some(true) {
            continue;
        }
        let line = Line {
            draw,
            stage,
            kind,
            slot: slot as u32,
            space: space as u32,
            name,
            read,
            parameter,
            heap,
            heap_slot,
            version,
            resource,
            address: address.map(|a| format!("{:#x}", a as u64)),
            size,
            stride,
            format: format.map(|f| locum_capture::format::name(f as u32)),
            flag,
        };
        shown += 1;
        if json {
            out!("{}", serde_json::to_string(&line).map_err(|e| e.to_string())?);
        } else {
            let o = |v: Option<i64>| v.map_or("-".to_string(), |v| v.to_string());
            let t = |v: Option<&str>| v.unwrap_or("-").to_string();
            out!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                line.draw,
                line.stage,
                line.kind,
                line.slot,
                line.space,
                match line.read {
                    Some(true) => "y",
                    Some(false) => "n",
                    None => "?",
                },
                t(line.name.as_deref()),
                line.parameter,
                o(line.heap),
                o(line.heap_slot),
                o(line.version),
                o(line.resource),
                t(line.address.as_deref()),
                o(line.size),
                o(line.stride),
                t(line.format),
                t(line.flag.as_deref())
            );
        }
    }
    if shown == 0 {
        eprintln!("locum-cli: no descriptor matches");
    }
    Ok(())
}
