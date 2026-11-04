//! `vertices`: what a draw fed its vertex shader. Each vertex the draw reads, in the order it reads them, with its
//! attributes decoded through the input layout from the bound vertex buffers (and the index from the index buffer),
//! as the draw saw those buffers: what took a script over the layout's JSON, the bindings' offsets and the format
//! codes before.

use std::collections::HashMap;

use locum_capture::format;
use locum_capture::texels::{self, Number};
use serde_json::{Value, json};

use crate::describe::{shape, short};
use crate::draw::{Row, file_of, layout_slots, read_vertices, rows};
use crate::open::Capture;
use crate::out;

/// An input-layout element, its offset resolved.
pub(crate) struct Element {
    pub semantic: String,
    pub index: u32,
    pub slot: u32,
    pub offset: u32,
    pub format: u32,
    pub per_instance: bool,
    /// Instances drawn with one element of per-instance data before the next (0: every instance reads the first).
    pub step: u32,
}

impl Element {
    /// `TEXCOORD1`.
    pub fn label(&self) -> String {
        format!("{}{}", self.semantic, self.index)
    }
}

/// D3D11_APPEND_ALIGNED_ELEMENT: the element follows the previous one of its slot.
const APPEND_ALIGNED: u64 = 0xffff_ffff;

/// The elements of input layout `layout` (a state's id). An element placed after the previous one of its slot
/// (D3D11_APPEND_ALIGNED_ELEMENT) is given the offset where that one ends, rounded up to 4 bytes.
pub(crate) fn elements(c: &Capture, layout: Option<i64>) -> Vec<Element> {
    let json: Option<String> = layout.and_then(|id| {
        c.row("SELECT json FROM states WHERE id = ?1", [id], |r| r.get(0))
            .flatten()
    });
    let list: Vec<Value> = json
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    let number = |e: &Value, k: &str| e[k].as_u64().unwrap_or(0);
    let mut ends: HashMap<u32, u32> = HashMap::new();
    list.iter()
        .map(|e| {
            let slot = number(e, "slot") as u32;
            let format = number(e, "format") as u32;
            let offset = match number(e, "offset") {
                APPEND_ALIGNED => ends.get(&slot).copied().unwrap_or(0).next_multiple_of(4),
                o => o as u32,
            };
            let size = format::layout(format).map_or(0, |l| l.block_bytes);
            ends.insert(slot, offset + size);
            Element {
                semantic: e["semantic"].as_str().unwrap_or_default().to_string(),
                index: number(e, "index") as u32,
                slot,
                offset,
                format,
                per_instance: e["per_instance"].as_bool().unwrap_or(false),
                step: number(e, "step") as u32,
            }
        })
        .collect()
}

/// What a draw call draws: its counts and starts, from its arguments or, for an indirect call, its argument buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Call {
    indexed: bool,
    count: u64,
    instances: u64,
    start: u64,
    base: i64,
    first_instance: u64,
}

impl Call {
    /// From the call's recorded arguments; an indirect call's are read from `indirect(buffer, offset)`, which gives
    /// the words there.
    fn of(
        call: &str,
        args: &str,
        indirect: impl FnOnce(i64, u64) -> Result<Vec<u32>, String>,
    ) -> Result<Call, String> {
        let a: Vec<i64> = serde_json::from_str(args).unwrap_or_default();
        let at = |i: usize| a.get(i).copied().unwrap_or(0);
        let u = |i: usize| at(i).max(0) as u64;
        Ok(match call {
            "Draw" => Call::plain(u(0), 1, u(1), 0),
            "DrawInstanced" => Call::plain(u(0), u(1), u(2), u(3)),
            "DrawIndexed" => Call::indexed(u(0), 1, u(1), at(2), 0),
            "DrawIndexedInstanced" => Call::indexed(u(0), u(1), u(2), at(3), u(4)),
            "DrawInstancedIndirect" | "DrawIndexedInstancedIndirect" => {
                let indexed = call == "DrawIndexedInstancedIndirect";
                let w = indirect(at(0), u(1))?;
                let need = if indexed { 5 } else { 4 };
                if w.len() < need {
                    return Err(format!(
                        "the argument buffer #{} holds {} words at {}, {need} needed",
                        at(0),
                        w.len(),
                        u(1)
                    ));
                }
                let w = |i: usize| u64::from(w[i]);
                if indexed {
                    Call::indexed(w(0), w(1), w(2), i64::from(w(3) as u32 as i32), w(4))
                } else {
                    Call::plain(w(0), w(1), w(2), w(3))
                }
            }
            other => {
                return Err(format!(
                    "{other} draws no vertices locum reads: `vertices` takes Draw, DrawIndexed, their instanced and \
                     indirect forms"
                ));
            }
        })
    }

    fn plain(count: u64, instances: u64, start: u64, first_instance: u64) -> Call {
        Call {
            indexed: false,
            count,
            instances,
            start,
            base: 0,
            first_instance,
        }
    }

    fn indexed(count: u64, instances: u64, start: u64, base: i64, first_instance: u64) -> Call {
        Call {
            indexed: true,
            count,
            instances,
            start,
            base,
            first_instance,
        }
    }
}

/// Where the bytes of a buffer the draw read came from.
struct Bytes {
    bytes: Vec<u8>,
    /// `as the frame began (first-use)`, `replayed: as event 9559 saw it, 2 events`.
    source: String,
}

pub struct Options {
    /// The first of the draw's vertices (its place in the draw) to list.
    pub from: u64,
    /// At most this many (0: all).
    pub limit: u64,
    /// Each vertex once, in the order the draw first reads it.
    pub unique: bool,
    /// The instance whose per-instance attributes are shown.
    pub instance: u64,
    /// The buffers' contents as the frame began even when the frame wrote them before the draw (no replay).
    pub frame_start: bool,
    pub warp: bool,
    pub json: bool,
}

/// A buffer's bytes as event `event` read them: its contents as the frame began when nothing wrote it since, or
/// else (unless `frame_start`) replayed to just before the event, as `export --at` does.
fn bytes_at(
    c: &Capture,
    id: i64,
    event: i64,
    o: &Options,
    warnings: &mut Vec<String>,
) -> Result<Bytes, String> {
    let written: Option<i64> = c
        .row(
            "SELECT max(event) FROM event_resources WHERE resource = ?1 AND access = 'write' AND event < ?2",
            [id, event],
            |r| r.get(0),
        )
        .flatten();
    if let Some(last) = written.filter(|_| !o.frame_start) {
        let (bytes, events, w) = crate::replay::contents_at(c, id, event, o.warp)
            .map_err(|e| format!("#{id} was written at event {last}, before the draw; replaying it: {e} (--frame-start reads its contents as the frame began)"))?;
        warnings.extend(w);
        return Ok(Bytes {
            bytes,
            source: format!("replayed: as event {event} saw it, from the frame's start with {events} events"),
        });
    }
    type Src = (Option<String>, Option<String>, Option<String>);
    let (key, source, note): Src = c
        .row(
            "SELECT content, content_source, content_note FROM resources WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok_or_else(|| format!("no resource #{id}"))?;
    let key = key.ok_or_else(|| {
        format!(
            "#{id} has no contents in this capture{}",
            note.map(|n| format!(" ({n})")).unwrap_or_default()
        )
    })?;
    let mut source = format!(
        "{} as the frame began ({})",
        short(&key),
        source.as_deref().unwrap_or("contents")
    );
    if let Some(last) = written {
        source += &format!(", not as the draw saw them: written since, last at event {last}");
    }
    Ok(Bytes {
        bytes: c.blob(&key)?,
        source,
    })
}

/// A bound buffer: its binding, and where its bytes for the draw came from.
struct Stream<'a> {
    row: &'a Row,
    data: std::rc::Rc<Bytes>,
}

impl Stream<'_> {
    /// The bytes the binding covers: from its offset, its size when the binding says, else to the buffer's end.
    fn range(&self) -> &[u8] {
        let all = &self.data.bytes;
        let from = (self.row.offset.unwrap_or(0).max(0) as usize).min(all.len());
        let to = match self.row.size {
            Some(s) if s >= 0 => (from + s as usize).min(all.len()),
            _ => all.len(),
        };
        &all[from..to]
    }
}

fn number_text(n: Number) -> String {
    match n {
        Number::Float(f) => format!("{f}"),
        Number::Uint(u) => u.to_string(),
        Number::Sint(i) => i.to_string(),
    }
}

fn number_json(n: Number) -> Value {
    match n {
        Number::Float(f) if f.is_finite() => json!(f),
        Number::Float(_) => Value::Null,
        Number::Uint(u) => json!(u),
        Number::Sint(i) => json!(i),
    }
}

/// One vertex the draw reads.
struct Fetched {
    /// Its place among the draw's vertices.
    i: u64,
    /// The index read, for an indexed draw.
    index: Option<u32>,
    /// The vertex fetched (the index plus the base), or None at a strip's restart index.
    vertex: Option<i64>,
    /// Each element's channels, or why it could not be read.
    values: Vec<Result<Vec<Number>, String>>,
}

pub fn vertices(c: &Capture, which: &str, o: &Options) -> Result<(), String> {
    let n = crate::replay::draw_number(c, which)?;
    let (event, call, args, topology, layout): (i64, String, String, Option<i64>, Option<i64>) = c
        .row(
            "SELECT event, call, args, topology, layout FROM draws WHERE n = ?1",
            [n],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .ok_or_else(|| format!("no draw {n}"))?;
    let mut warnings = Vec::new();
    let mut cache: HashMap<i64, std::rc::Rc<Bytes>> = HashMap::new();
    let mut fetch = |id: i64, warnings: &mut Vec<String>| -> Result<std::rc::Rc<Bytes>, String> {
        if let Some(b) = cache.get(&id) {
            return Ok(b.clone());
        }
        let b = std::rc::Rc::new(bytes_at(c, id, event, o, warnings)?);
        cache.insert(id, b.clone());
        Ok(b)
    };
    // Where an indirect call's arguments were read.
    let mut arguments: Option<(i64, String)> = None;
    let d = Call::of(&call, &args, |buffer, offset| {
        let b = fetch(buffer, &mut warnings)?;
        arguments = Some((buffer, b.source.clone()));
        Ok(b.bytes
            .get(offset as usize..)
            .unwrap_or_default()
            .as_chunks::<4>()
            .0
            .iter()
            .take(5)
            .map(|&w| u32::from_le_bytes(w))
            .collect())
    })?;
    let layout_elements = elements(c, layout);
    if layout_elements.is_empty() {
        return Err(format!(
            "draw {n} has no input layout{}: its vertex shader reads no vertex buffers (it makes its vertices from \
             SV_VertexID, or reads buffers as resources: `draw {n}` lists them)",
            if layout.is_some() { " elements" } else { "" }
        ));
    }
    let bound = rows(c, n);
    let slots = layout_slots(c, layout);
    let vbs = read_vertices(&bound, slots.as_deref());
    let mut streams: HashMap<u32, Result<Stream, String>> = HashMap::new();
    for e in &layout_elements {
        if streams.contains_key(&e.slot) {
            continue;
        }
        let row = bound.iter().find(|b| b.kind == "vb" && b.slot == e.slot);
        let stream = match row.and_then(|r| Some((r, r.resource?))) {
            Some((row, id)) => fetch(id, &mut warnings).map(|data| Stream { row, data }),
            None => Err(format!(
                "nothing is bound at vb{} (the input assembler reads zeros)",
                e.slot
            )),
        };
        streams.insert(e.slot, stream);
    }
    let ib = if d.indexed {
        let row = bound
            .iter()
            .find(|b| b.kind == "ib")
            .ok_or("an indexed draw with no index buffer bound")?;
        let id = row.resource.ok_or("an indexed draw with no index buffer bound")?;
        let width = match row.format {
            Some(57) => 2,
            Some(42) => 4,
            f => {
                return Err(format!(
                    "the index buffer's format is {}, not R16_UINT or R32_UINT",
                    f.map_or("not recorded", |f| format::name(f as u32))
                ));
            }
        };
        Some((
            Stream {
                row,
                data: fetch(id, &mut warnings)?,
            },
            width,
        ))
    } else {
        None
    };
    // A strip's all-ones index starts a new strip.
    let strip = matches!(topology, Some(3 | 5 | 11 | 13));

    let index_at = |k: u64| -> Result<u32, String> {
        let (s, width) = ib.as_ref().expect("an indexed draw");
        let at = ((d.start + k) * width) as usize;
        let r = s.range();
        let b = r
            .get(at..at + *width as usize)
            .ok_or_else(|| format!("index {} is past the {} bytes bound", d.start + k, r.len()))?;
        Ok(if *width == 2 {
            u32::from(u16::from_le_bytes([b[0], b[1]]))
        } else {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        })
    };
    let read = |e: &Element, vertex: i64| -> Result<Vec<Number>, String> {
        let s = streams[&e.slot].as_ref().map_err(String::clone)?;
        let stride = s.row.stride.unwrap_or(0).max(0) as u64;
        let element = if e.per_instance {
            d.first_instance
                + if e.step == 0 {
                    0
                } else {
                    o.instance / u64::from(e.step)
                }
        } else {
            u64::try_from(vertex).map_err(|_| format!("vertex {vertex} is before the buffer"))?
        };
        let at = (element * stride + u64::from(e.offset)) as usize;
        let r = s.range();
        r.get(at..)
            .filter(|b| !b.is_empty())
            .ok_or_else(|| format!("byte {at} is past the {} bytes bound at vb{}", r.len(), e.slot))
            .and_then(|b| texels::element(e.format, b))
    };

    let mut listed = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut k = o.from;
    let mut errors = 0usize;
    while k < d.count && (o.limit == 0 || (listed.len() as u64) < o.limit) {
        let (index, vertex) = if d.indexed {
            match index_at(k) {
                Ok(i)
                    if strip
                        && (i == u32::MAX || (i == 0xffff && ib.as_ref().is_some_and(|(_, w)| *w == 2))) =>
                {
                    (Some(i), None)
                }
                Ok(i) => (Some(i), Some(i64::from(i) + d.base)),
                Err(e) => {
                    warnings.push(e);
                    break;
                }
            }
        } else {
            (None, Some((d.start + k) as i64))
        };
        k += 1;
        if o.unique && !seen.insert(vertex) {
            continue;
        }
        let values: Vec<_> = layout_elements
            .iter()
            .map(|e| match vertex {
                Some(v) => read(e, v),
                None => Err("restart".to_string()),
            })
            .collect();
        errors += values.iter().filter(|v| v.is_err()).count();
        listed.push(Fetched {
            i: k - 1,
            index,
            vertex,
            values,
        });
    }

    let names_of = |slot: u32| -> Option<String> {
        let row = bound.iter().find(|b| b.kind == "vb" && b.slot == slot)?;
        file_of(c, row, &call, &args, &vbs)
    };
    if o.json {
        let stream_json = |slot: u32| match &streams[&slot] {
            Ok(s) => json!({"slot":slot,"resource":s.row.resource,"offset":s.row.offset,"size":s.row.size,
                "stride":s.row.stride,"source":s.data.source,"file":names_of(slot)}),
            Err(e) => json!({"slot":slot,"error":e}),
        };
        let mut slots: Vec<u32> = streams.keys().copied().collect();
        slots.sort_unstable();
        let v = json!({
            "draw": n, "event": event, "call": call,
            "drawn": {"indexed":d.indexed,"count":d.count,"instances":d.instances,"start":d.start,"base":d.base,
                "first_instance":d.first_instance},
            "arguments": arguments.as_ref().map(|(id, source)| json!({"resource":id,"source":source})),
            "instance": o.instance,
            "index_buffer": ib.as_ref().map(|(s, w)| json!({"resource":s.row.resource,"offset":s.row.offset,
                "format":format::name(if *w == 2 { 57 } else { 42 }),"source":s.data.source})),
            "buffers": slots.iter().map(|&s| stream_json(s)).collect::<Vec<_>>(),
            "elements": layout_elements.iter().map(|e| json!({"name":e.label(),"semantic":e.semantic,"index":e.index,
                "slot":e.slot,"offset":e.offset,"format":format::name(e.format),"per_instance":e.per_instance,
                "step":e.step})).collect::<Vec<_>>(),
            "vertices": listed.iter().map(|f| json!({"i":f.i,"index":f.index,"vertex":f.vertex,
                "values": layout_elements.iter().zip(&f.values).map(|(e, v)| (e.label(), match v {
                    Ok(v) => Value::Array(v.iter().copied().map(number_json).collect()),
                    Err(_) => Value::Null,
                })).collect::<serde_json::Map<_, _>>()})).collect::<Vec<_>>(),
            "warnings": warnings,
        });
        out!("{v}");
        return Ok(());
    }

    let quiet = crate::export::QUIET.load(std::sync::atomic::Ordering::Relaxed);
    if !quiet {
        eprintln!(
            "draw {n} {call} {}{}",
            crate::describe::call_args(&call, &args),
            if call.ends_with("Indirect") {
                format!(
                    " (from its argument buffer: count={} instances={} start={} base={} first_instance={}; {})",
                    d.count,
                    d.instances,
                    d.start,
                    d.base,
                    d.first_instance,
                    arguments.as_ref().map_or("", |a| a.1.as_str())
                )
            } else {
                String::new()
            }
        );
        if let Some((s, w)) = &ib {
            eprintln!(
                "  ib {} offset {} {}  {}",
                s.row
                    .resource
                    .and_then(|r| shape(c, r))
                    .map_or("?".into(), |s| s.to_string()),
                s.row.offset.unwrap_or(0),
                format::name(if *w == 2 { 57 } else { 42 }),
                s.data.source
            );
        }
        let mut slots: Vec<u32> = streams.keys().copied().collect();
        slots.sort_unstable();
        for slot in slots {
            match &streams[&slot] {
                Ok(s) => eprintln!(
                    "  vb{slot} {} offset {} stride {}{}  {}",
                    s.row
                        .resource
                        .and_then(|r| shape(c, r))
                        .map_or("?".into(), |s| s.to_string()),
                    s.row.offset.unwrap_or(0),
                    s.row.stride.unwrap_or(0),
                    names_of(slot).map(|f| format!(" = {f}")).unwrap_or_default(),
                    s.data.source
                ),
                Err(e) => eprintln!("  vb{slot}: {e}"),
            }
        }
        for e in &layout_elements {
            eprintln!(
                "  {} vb{} +{} {}{}",
                e.label(),
                e.slot,
                e.offset,
                format::name(e.format),
                if e.per_instance {
                    format!(" per instance (step {}; instance {} shown)", e.step, o.instance)
                } else {
                    String::new()
                }
            );
        }
    }
    let mut header = vec!["i".to_string()];
    if d.indexed {
        header.push("index".into());
    }
    header.push("vertex".into());
    header.extend(layout_elements.iter().map(Element::label));
    out!("{}", header.join("\t"));
    for f in &listed {
        let mut cells = vec![f.i.to_string()];
        if let Some(i) = f.index {
            cells.push(i.to_string());
        }
        cells.push(f.vertex.map_or("restart".into(), |v| v.to_string()));
        cells.extend(f.values.iter().map(|v| match v {
            Ok(v) => v.iter().copied().map(number_text).collect::<Vec<_>>().join(" "),
            Err(e) if e == "restart" => String::new(),
            Err(_) => "?".into(),
        }));
        out!("{}", cells.join("\t"));
    }
    for w in &warnings {
        eprintln!("warning: {w}");
    }
    if !quiet {
        let more = d.count.saturating_sub(k);
        let total = if o.unique { "vertices read" } else { "vertices" };
        if more > 0 {
            eprintln!(
                "({} listed, places {}..{} of the draw's {} {total}; --from and --limit (0: all) list others)",
                listed.len(),
                o.from,
                k,
                d.count
            );
        }
        if errors > 0 {
            let first = listed
                .iter()
                .flat_map(|f| &f.values)
                .find_map(|v| v.as_ref().err().filter(|e| *e != "restart"));
            if let Some(e) = first {
                eprintln!("({errors} values could not be read, shown as ?: {e})");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_name_their_counts_wherever_the_api_puts_them() {
        let none = |_: i64, _: u64| -> Result<Vec<u32>, String> { unreachable!() };
        assert_eq!(
            Call::of("DrawIndexedInstanced", "[36,2,21952,-4,7]", none).unwrap(),
            Call::indexed(36, 2, 21952, -4, 7)
        );
        assert_eq!(
            Call::of("DrawIndexed", "[6,3,10]", none).unwrap(),
            Call::indexed(6, 1, 3, 10, 0)
        );
        assert_eq!(Call::of("Draw", "[4,8]", none).unwrap(), Call::plain(4, 1, 8, 0));
        assert_eq!(
            Call::of("DrawInstanced", "[3,5,1,2]", none).unwrap(),
            Call::plain(3, 5, 1, 2)
        );
        // An indirect call's from its argument buffer, the base vertex signed.
        let words = |b: i64, o: u64| {
            assert_eq!((b, o), (9055, 16));
            Ok(vec![12, 1, 30, (-2i32) as u32, 0])
        };
        assert_eq!(
            Call::of("DrawIndexedInstancedIndirect", "[9055,16]", words).unwrap(),
            Call::indexed(12, 1, 30, -2, 0)
        );
        assert!(Call::of("Dispatch", "[1,1,1]", none).is_err());
    }
}
