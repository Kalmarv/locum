//! `resources`, `resource`, `events` and `stack`: what was alive, what the frame did to it, and where from.

use locum_capture::format;
use rusqlite::types::Value;

use crate::describe::{self, SHAPE_COLUMNS, Shape, mib, shape, short};
use crate::open::Capture;
use crate::out;

#[derive(Default)]
pub struct Filter {
    pub used: bool,
    pub written: bool,
    pub kind: Option<String>,
    pub format: Option<String>,
    pub hash: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// How many of each kind and format, instead of one line each.
    pub counts: bool,
}

pub fn resources(c: &Capture, f: &Filter) -> Result<(), String> {
    let mut wh: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();
    let mut param = |v: Value| {
        params.push(v);
        format!("?{}", params.len())
    };
    if f.used {
        wh.push("used = 1".into());
    }
    if f.written {
        wh.push("written = 1".into());
    }
    if let Some(k) = &f.kind {
        wh.push(format!("kind = {}", param(Value::Text(k.clone()))));
    }
    if let Some(name) = &f.format {
        let n = format::by_name(name).ok_or_else(|| format!("no DXGI format {name}"))?;
        wh.push(format!("format = {}", param(Value::Integer(i64::from(n)))));
    }
    if let Some(w) = f.width {
        wh.push(format!("width = {}", param(Value::Integer(w))));
    }
    if let Some(h) = f.height {
        wh.push(format!("height = {}", param(Value::Integer(h))));
    }
    if let Some(h) = &f.hash {
        let p = param(Value::Text(h.clone()));
        wh.push(format!(
            "(content LIKE {p} || '%' OR upload LIKE {p} || '%' OR id IN \
             (SELECT resource FROM subresources WHERE hash LIKE {p} || '%'))"
        ));
    }
    let sql = format!(
        "SELECT {SHAPE_COLUMNS}, used, written, content, content_source, upload, upload_stored, content_note \
         FROM resources {} ORDER BY id",
        if wh.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", wh.join(" AND "))
        }
    );
    type Row = (
        Shape,
        bool,
        bool,
        Option<String>,
        Option<String>,
        Option<String>,
        bool,
        Option<String>,
    );
    let rows: Vec<Row> = c.rows(&sql, rusqlite::params_from_iter(params), |r| {
        Ok((
            Shape::from_row(r)?,
            r.get(12)?,
            r.get(13)?,
            r.get(14)?,
            r.get(15)?,
            r.get(16)?,
            r.get(17)?,
            r.get(18)?,
        ))
    });
    if f.counts {
        // kind, format name -> count, bytes, used
        let mut by: std::collections::BTreeMap<(String, &str), (usize, u64, usize)> = Default::default();
        for (s, used, ..) in &rows {
            let e = by.entry((s.kind.clone(), format::name(s.format))).or_default();
            e.0 += 1;
            e.1 += s.bytes;
            e.2 += usize::from(*used);
        }
        out!("kind\tformat\tresources\tused\tbytes");
        for ((kind, name), (n, bytes, used)) in by {
            let name = if kind == "buffer" { "-" } else { name };
            out!("{kind}\t{name}\t{n}\t{used}\t{}", mib(bytes));
        }
        return Ok(());
    }
    let mut total = 0u64;
    for (s, used, written, content, source, upload, stored, note) in &rows {
        total += s.bytes;
        let flags = format!(
            "{}{}",
            if *used { "U" } else { "-" },
            if *written { "W" } else { "-" }
        );
        let contents = match (content, source) {
            (Some(k), Some(src)) => format!("contents {} ({src})", short(k)),
            _ => String::new(),
        };
        let upload = upload
            .as_ref()
            .map(|u| format!("upload {}{}", short(u), if *stored { "" } else { " (hash)" }))
            .unwrap_or_default();
        out!(
            "{flags} {:<60} {contents:<32} {upload}{}",
            s.to_string(),
            note.as_ref().map(|n| format!("  [{n}]")).unwrap_or_default()
        );
    }
    out!(
        "{} resources, {}  (U used by the frame, W written by it)",
        rows.len(),
        mib(total)
    );
    Ok(())
}

pub fn resource(c: &Capture, id: i64) -> Result<(), String> {
    let s = shape(c, id).ok_or_else(|| format!("no resource #{id}"))?;
    type Row = (
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<i64>,
        Option<String>,
        bool,
        Option<i64>,
        Option<i64>,
        i64,
        bool,
        bool,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let r: Row = c
        .row(
            "SELECT stride, usage, bind, cpu, misc, samples, created_frame, created_stack, upload, upload_stored, \
             updated_frame, updated_stack, update_count, used, written, content, content_source, content_note \
             FROM resources WHERE id = ?1",
            [id],
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
                    r.get(15)?,
                    r.get(16)?,
                    r.get(17)?,
                ))
            },
        )
        .ok_or("no row")?;
    let (
        _stride,
        usage,
        bind,
        cpu,
        misc,
        _samples,
        created,
        created_stack,
        upload,
        stored,
        updated,
        updated_stack,
        updates,
        used,
        written,
        content,
        source,
        note,
    ) = r;
    out!("{s}  ({})", mib(s.bytes));
    out!(
        "usage {} bind 0x{:x} cpu 0x{:x} misc 0x{:x}",
        usage.unwrap_or(0),
        bind.unwrap_or(0),
        cpu.unwrap_or(0),
        misc.unwrap_or(0)
    );
    match created {
        Some(f) => out!("created at frame {f}"),
        None => out!("created before locum numbered it (frame unknown)"),
    }
    if let Some(st) = created_stack {
        print_stack(c, "  from", st, 8);
    }
    match (&upload, stored) {
        (Some(u), true) => out!("upload {u} (stored)"),
        (Some(u), false) => out!("upload {u} (hash only)"),
        _ => out!("no upload hash (created without data, or updated in parts)"),
    }
    if let Some(m) = c.names.of(c, id) {
        out!("made from {m}");
    }
    if updates > 0 {
        out!(
            "updated {updates} times, last at frame {}",
            updated.map_or("?".into(), |f| f.to_string())
        );
        if let Some(st) = updated_stack {
            print_stack(c, "  last from", st, 8);
        }
    }
    out!(
        "the frame {} it{}",
        if used { "used" } else { "did not use" },
        if written { " and wrote" } else { "" }
    );
    match (content, source) {
        (Some(k), Some(src)) => out!("contents as the frame began: {k} ({src})"),
        _ if used => out!("contents: missing"),
        _ => {}
    }
    if let Some(n) = note {
        out!("  note: {n}");
    }
    let subs: Vec<(i64, Option<String>)> = c.rows(
        "SELECT sub, hash FROM subresources WHERE resource = ?1 ORDER BY sub",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    if subs.len() > 1 {
        let layout = s.subs();
        out!("subresources:");
        for (sub, hash) in subs {
            let shape = layout.iter().find(|l| i64::from(l.index) == sub);
            out!(
                "  {sub:>4} {} {}",
                shape.map_or(String::new(), |l| format!(
                    "mip {} slice {} {}x{}{}",
                    l.mip,
                    l.slice,
                    l.width,
                    l.height,
                    if l.depth > 1 {
                        format!("x{}", l.depth)
                    } else {
                        String::new()
                    }
                )),
                hash.as_deref().unwrap_or("-")
            );
        }
    }
    let views: Vec<(i64, String, Option<String>)> = c.rows(
        "SELECT id, kind, json FROM views WHERE resource = ?1 ORDER BY id",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    );
    // Identical views once each, with their numbers: a D3D12 game can make a view of one texture per use.
    let mut grouped: Vec<((String, String), Vec<i64>)> = Vec::new();
    for (v, kind, json) in views {
        let desc = (kind, json.unwrap_or_default());
        match grouped.iter_mut().find(|g| g.0 == desc) {
            Some(g) => g.1.push(v),
            None => grouped.push((desc, vec![v])),
        }
    }
    for ((kind, json), ids) in grouped {
        match ids.as_slice() {
            [v] => out!("view #{v} {kind} {json}"),
            _ => {
                let mut listed: Vec<String> = ids.iter().take(4).map(|v| format!("#{v}")).collect();
                if ids.len() > 4 {
                    listed.push(format!("... #{}", ids[ids.len() - 1]));
                }
                out!("{} views {kind} {json} ({})", ids.len(), listed.join(" "));
            }
        }
    }
    let bound: Vec<(i64, String, String, i64)> = c.rows(
        "SELECT draw, stage, kind, slot FROM bindings WHERE resource = ?1 ORDER BY draw",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    );
    if !bound.is_empty() {
        let mut draws: Vec<i64> = bound.iter().map(|b| b.0).collect();
        draws.dedup();
        out!("{} bindings in {} draws:", bound.len(), draws.len());
        for (d, stage, kind, slot) in bound.iter().take(24) {
            out!("  draw {d}: {stage} {kind}{slot}");
        }
        if bound.len() > 24 {
            out!("  ... ({} more; `draws --uses {id}`)", bound.len() - 24);
        }
    }
    let events: Vec<(i64, String, String, String)> = c.rows(
        "SELECT e.seq, e.kind, e.call, x.access FROM event_resources x JOIN events e ON e.seq = x.event \
         WHERE x.resource = ?1 AND (x.access = 'write' OR e.kind NOT IN ('draw', 'dispatch')) ORDER BY e.seq",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    );
    if !events.is_empty() {
        out!("events that wrote it or copied from it:");
        for (seq, kind, call, access) in events.iter().take(40) {
            out!("  event {seq}: {kind} {call} ({access})");
        }
        if events.len() > 40 {
            out!("  ... ({} more; `events --resource {id}`)", events.len() - 40);
        }
    }
    Ok(())
}

fn print_stack(c: &Capture, label: &str, id: i64, depth: usize) {
    let frames = describe::stack(c, id);
    out!(
        "{label} stack {id}: {}",
        frames.iter().take(depth).cloned().collect::<Vec<_>>().join(" < ")
    );
}

pub struct EventFilter {
    pub kind: Option<String>,
    pub resource: Option<i64>,
    pub from: i64,
    pub limit: i64,
}

pub fn events(c: &Capture, f: &EventFilter) -> Result<(), String> {
    let mut wh = vec!["e.seq >= ?1".to_string()];
    let mut params: Vec<Value> = vec![Value::Integer(f.from)];
    if let Some(k) = &f.kind {
        params.push(Value::Text(k.clone()));
        wh.push(format!("e.kind = ?{}", params.len()));
    }
    // How each listed event touches the resource asked about: a reader's own writes are other resources.
    let mut access = "NULL".to_string();
    if let Some(r) = f.resource {
        params.push(Value::Integer(r));
        let n = params.len();
        wh.push(format!(
            "e.seq IN (SELECT event FROM event_resources WHERE resource = ?{n})"
        ));
        access = format!(
            "(SELECT group_concat(DISTINCT access) FROM event_resources WHERE event = e.seq AND resource = ?{n})"
        );
    }
    // One past the limit, to say whether there are more.
    params.push(Value::Integer(f.limit + 1));
    let sql = format!(
        "SELECT e.seq, e.kind, e.call, e.deferred, e.list, e.draw, e.args, e.data, \
         (SELECT group_concat(resource) FROM event_resources WHERE event = e.seq AND access = 'write'), {access} \
         FROM events e WHERE {} ORDER BY e.seq LIMIT ?{}",
        wh.join(" AND "),
        params.len()
    );
    type Row = (
        i64,
        String,
        String,
        bool,
        Option<i64>,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let mut rows: Vec<Row> = c.rows(&sql, rusqlite::params_from_iter(params), |r| {
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
    });
    let more = rows.len() as i64 > f.limit;
    rows.truncate(f.limit.max(0) as usize);
    let next = rows.last().map(|r| r.0 + 1);
    for (seq, kind, call, deferred, list, draw, args, data, writes, access) in rows {
        let place = if deferred {
            format!("list {}", list.map_or("?".into(), |l| l.to_string()))
        } else {
            "imm".into()
        };
        let touches = match (access.as_deref(), f.resource) {
            (Some(a), Some(r)) => {
                let read = a.split(',').any(|a| a == "read");
                let write = a.split(',').any(|a| a == "write");
                match (read, write) {
                    (true, true) => format!("  reads and writes #{r}"),
                    (true, false) => format!("  reads #{r}"),
                    _ => String::new(),
                }
            }
            _ => String::new(),
        };
        let kept = upload_source(c, seq, &call, args.as_deref()).unwrap_or_default();
        out!(
            "{seq:>6} {kind:<9} {call:<28} {place:<8} {}{}{touches}{kept}{}{}",
            draw.map(|d| format!("draw {d} ")).unwrap_or_default(),
            args.unwrap_or_default(),
            writes
                .map(|w| format!("  writes #{}", w.replace(',', " #")))
                .unwrap_or_default(),
            data.map(|d| format!("  data {}", short(&d))).unwrap_or_default()
        );
    }
    if more && let Some(next) = next {
        out!("(stopped at --limit {}; --from {next} lists the next)", f.limit);
    }
    Ok(())
}

/// For a D3D12 buffer copy from an upload buffer, whether the capture kept the bytes it read: the upload ranges its
/// submission copied (`d12_upload_versions`), or only the buffer as the frame began, which later CPU writes may have
/// changed.
fn upload_source(c: &Capture, event: i64, call: &str, args: Option<&str>) -> Option<String> {
    if c.meta("api").as_deref() != Some("d3d12") {
        return None;
    }
    let a: serde_json::Value = serde_json::from_str(args?).ok()?;
    let id = |v: &serde_json::Value| v["resource"].as_i64().or_else(|| v.as_i64());
    let (source, offset, bytes) = match call {
        "CopyBufferRegion" => (id(&a[2])?, a[3].as_i64()?, a[4].as_i64()?),
        "CopyResource" => (id(&a[1])?, 0, -1),
        _ => return None,
    };
    let upload = c
        .row(
            "SELECT json_extract(description, '$.heap_properties.type') FROM d12_objects WHERE id = ?1",
            [source],
            |r| r.get::<_, Option<i64>>(0),
        )
        .flatten()
        == Some(2);
    if !upload {
        return None;
    }
    let kept: i64 = c.count(
        "SELECT count(*) FROM d12_upload_versions u JOIN d12_executions x ON x.submission = u.submission \
         JOIN d12_event_locations l ON l.execution = x.id \
         WHERE l.event = ?1 AND u.resource = ?2 AND u.offset <= ?3 \
         AND (?4 < 0 OR u.offset + json_extract(u.content_json, '$.size') >= ?3 + ?4) \
         AND (?4 >= 0 OR u.whole = 1)",
        rusqlite::params![event, source, offset, bytes],
    );
    Some(if kept > 0 {
        "  (the upload bytes it read are kept)".into()
    } else {
        "  (the upload bytes it read are not kept: only as the frame began)".into()
    })
}

pub fn stack(c: &Capture, id: i64) -> Result<(), String> {
    let frames = describe::stack(c, id);
    if frames.is_empty() {
        return Err(format!("no stack {id}"));
    }
    for f in frames {
        out!("{f}");
    }
    Ok(())
}
