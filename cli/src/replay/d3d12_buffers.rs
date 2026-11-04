//! A Direct3D 12 buffer's contents at an event, without a replay: its contents as the frame began, with each write
//! the capture keeps the bytes of applied in order. A copy from an upload buffer reads the bytes that buffer held when
//! the copy's submission ran (`d12_upload_versions`), a copy from another buffer reads that buffer at the copy (the
//! same way), and `WriteBufferImmediate` writes its words. What the GPU wrote some other way (a dispatch's UAV writes,
//! resolved queries) the capture holds no bytes of, so a buffer such a write reached before the event is refused.

use serde_json::Value;

use crate::open::Capture;

/// Buffer `id`'s bytes just before event `before`, how many writes were applied, and warnings.
pub fn buffer_at(c: &Capture, id: i64, before: i64) -> Result<(Vec<u8>, usize, Vec<String>), String> {
    let mut warnings = Vec::new();
    let bytes = at(c, id, before, &mut warnings, 0)?;
    let applied = writes(c, id, before).len();
    Ok((bytes, applied, warnings))
}

/// The events before `before` that wrote resource `id`: (event, call, args).
fn writes(c: &Capture, id: i64, before: i64) -> Vec<(i64, String, String)> {
    c.rows(
        "SELECT e.seq, e.call, coalesce(e.args, '') FROM event_resources r JOIN events e ON e.seq = r.event \
         WHERE r.resource = ?1 AND r.access = 'write' AND e.seq < ?2 ORDER BY e.seq",
        [id, before],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )
}

fn at(c: &Capture, id: i64, before: i64, warnings: &mut Vec<String>, depth: usize) -> Result<Vec<u8>, String> {
    if depth > 16 {
        return Err(format!("#{id}: copies chain more than 16 buffers deep"));
    }
    let (kind, content): (String, Option<String>) = c
        .row(
            "SELECT kind, content FROM resources WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok_or_else(|| format!("no resource #{id}"))?;
    if kind != "buffer" {
        return Err(format!(
            "#{id} is a {kind}: on Direct3D 12 only buffers are rebuilt at an event (there is no replay yet)"
        ));
    }
    let key = content.ok_or_else(|| format!("#{id} has no contents as the frame began in this capture"))?;
    let mut bytes = c.blob(&key)?;
    for (event, call, args) in writes(c, id, before) {
        let a: Value = serde_json::from_str(&args).unwrap_or(Value::Null);
        let resource = |v: &Value| v["resource"].as_i64().or_else(|| v.as_i64());
        match call.as_str() {
            "CopyBufferRegion" => {
                let field = |i: usize| a[i].as_u64().ok_or_else(|| format!("event {event}: {call} {args}"));
                let (to, from, size) = (field(1)?, field(3)?, field(4)?);
                let source = resource(&a[2]).ok_or_else(|| format!("event {event}: {call} {args}"))?;
                let read = source_bytes(c, source, from, size, event, warnings, depth)?;
                put(&mut bytes, to, &read, id, event)?;
            }
            "CopyResource" => {
                let source = resource(&a[1]).ok_or_else(|| format!("event {event}: {call} {args}"))?;
                let len = bytes.len() as u64;
                let read = source_bytes(c, source, 0, len, event, warnings, depth)?;
                put(&mut bytes, 0, &read, id, event)?;
            }
            "WriteBufferImmediate" => {
                for p in a[0].as_array().into_iter().flatten() {
                    if resource(&p["address"]) != Some(id) {
                        continue;
                    }
                    let (Some(offset), Some(value)) = (p["address"]["offset"].as_u64(), p["value"].as_u64()) else {
                        return Err(format!("event {event}: {call} {args}"));
                    };
                    put(&mut bytes, offset, &(value as u32).to_le_bytes(), id, event)?;
                }
            }
            _ => {
                return Err(format!(
                    "#{id} was written at event {event} ({call}) before event {before}, and a Direct3D 12 capture \
                     keeps no bytes of what that call wrote; its contents as the frame began are the last known"
                ));
            }
        }
    }
    Ok(bytes)
}

/// `size` bytes at `from` of buffer `source` as the copy at `event` read them.
fn source_bytes(
    c: &Capture,
    source: i64,
    from: u64,
    size: u64,
    event: i64,
    warnings: &mut Vec<String>,
    depth: usize,
) -> Result<Vec<u8>, String> {
    let upload = c
        .row(
            "SELECT json_extract(description, '$.heap_properties.type') FROM d12_objects WHERE id = ?1",
            [source],
            |r| r.get::<_, Option<i64>>(0),
        )
        .flatten()
        == Some(2);
    let whole = if upload {
        // The upload buffer's bytes as the copy's submission ran, the smallest kept range covering the read.
        let kept: Vec<String> = c.rows(
            "SELECT u.content_json FROM d12_upload_versions u JOIN d12_executions x ON x.submission = u.submission \
             JOIN d12_event_locations l ON l.execution = x.id WHERE l.event = ?1 AND u.resource = ?2",
            [event, source],
            |r| r.get(0),
        );
        let covering = kept
            .iter()
            .filter_map(|j| serde_json::from_str::<Value>(j).ok())
            .filter_map(|j| Some((j["offset"].as_u64()?, j["size"].as_u64()?, j["blob"].as_str()?.to_string())))
            .filter(|(o, n, _)| *o <= from && from + size <= o + n)
            .min_by_key(|(_, n, _)| *n);
        match covering {
            Some((offset, _, key)) => {
                let bytes = c.blob(&key)?;
                let start = (from - offset) as usize;
                return bytes
                    .get(start..start + size as usize)
                    .map(<[u8]>::to_vec)
                    .ok_or_else(|| format!("upload range {key} of #{source} is shorter than recorded"));
            }
            None => {
                warnings.push(format!(
                    "event {event} copied #{source} bytes {from}..{} whose upload bytes the capture did not keep: \
                     they are #{source} as the frame began, which the CPU may have changed since",
                    from + size
                ));
                let key: String = c
                    .row("SELECT content FROM resources WHERE id = ?1", [source], |r| r.get(0))
                    .flatten()
                    .ok_or_else(|| format!("#{source} has no contents in this capture"))?;
                c.blob(&key)?
            }
        }
    } else {
        at(c, source, event, warnings, depth + 1)?
    };
    whole
        .get(from as usize..(from + size) as usize)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| format!("event {event} read past the end of #{source}"))
}

fn put(bytes: &mut [u8], at: u64, data: &[u8], id: i64, event: i64) -> Result<(), String> {
    let at = at as usize;
    bytes
        .get_mut(at..at + data.len())
        .ok_or_else(|| format!("event {event} wrote past the end of #{id}"))?
        .copy_from_slice(data);
    Ok(())
}
