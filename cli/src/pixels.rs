//! `pixel` and `changes`: what the frame's passes did to a target's texels, over every readback of it after a pass.

use locum_capture::format;
use locum_capture::texels::{self, Image};

use crate::open::Capture;
use crate::out;

/// One subresource of a target resource, as `#ID` (or `ID`), or `DRAW:TARGET` for the one a draw's readback holds.
struct Subject {
    resource: i64,
    sub: i64,
    z: i64,
}

fn subject(c: &Capture, arg: &str, sub: Option<u32>) -> Result<Subject, String> {
    if let Some((draw, name)) = arg.split_once(':') {
        let draw: i64 = draw.parse().map_err(|_| format!("{arg}: not DRAW:TARGET"))?;
        if let Some((resource, s, z)) = c.row(
            "SELECT resource, sub, z FROM targets WHERE draw = ?1 AND target = ?2",
            rusqlite::params![draw, name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        ) {
            return Ok(Subject {
                resource,
                sub: sub.map_or(s, i64::from),
                z,
            });
        }
        let why: Option<String> = c
            .has_table("missing_targets")
            .then(|| {
                c.row(
                    "SELECT why FROM missing_targets WHERE draw = ?1 AND target = ?2",
                    rusqlite::params![draw, name],
                    |r| r.get(0),
                )
            })
            .flatten();
        return Err(match why {
            Some(why) => format!("{name} after draw {draw} was not read back: {why}"),
            None => format!("no {name} read back after draw {draw}"),
        });
    }
    let resource: i64 = arg
        .trim_start_matches('#')
        .parse()
        .map_err(|_| format!("{arg}: not #ID or DRAW:TARGET"))?;
    let sub = match sub {
        Some(s) => i64::from(s),
        None => c
            .row(
                "SELECT min(sub) FROM targets WHERE resource = ?1",
                [resource],
                |r| r.get::<_, Option<i64>>(0),
            )
            .flatten()
            .ok_or_else(|| format!("#{resource} was never read back after a pass"))?,
    };
    Ok(Subject { resource, sub, z: 0 })
}

/// A readback of the subject, or a pass end of it that was not read back, in the frame's order.
enum Entry {
    Read {
        draw: i64,
        pass: Option<i64>,
        target: String,
        image: Result<Image, String>,
        size: (u32, u32),
    },
    Missing {
        draw: i64,
        target: String,
        why: String,
    },
}

/// A `targets` row: draw, pass, target, width, height, format, blob.
type Readback = (i64, Option<i64>, String, u32, u32, u32, Option<String>);

fn history(c: &Capture, s: &Subject) -> Result<(String, Vec<Entry>), String> {
    let rows: Vec<Readback> = c.rows(
        "SELECT t.draw, d.pass, t.target, t.width, t.height, t.format, t.blob FROM targets t \
         LEFT JOIN draws d ON d.n = t.draw WHERE t.resource = ?1 AND t.sub = ?2 AND t.z = ?3 ORDER BY t.event",
        [s.resource, s.sub, s.z],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?)),
    );
    let Some(&(_, _, _, w, h, f, _)) = rows.first() else {
        return Err(format!(
            "#{} sub {} was never read back after a pass",
            s.resource, s.sub
        ));
    };
    let title = format!("#{} sub {} ({w}x{h} {})", s.resource, s.sub, format::name(f));
    let mut entries: Vec<Entry> = rows
        .into_iter()
        .map(|(draw, pass, target, w, h, f, blob)| Entry::Read {
            draw,
            pass,
            target,
            image: blob
                .ok_or_else(|| "not stored".to_string())
                .and_then(|k| c.blob(&k))
                .and_then(|b| texels::decode(f, w, h, &b)),
            size: (w, h),
        })
        .collect();
    if c.has_table("missing_targets") {
        // The rows do not say which subresource a missing pass end was of: those of the resource are all shown.
        let missing: Vec<(i64, String, String)> = c.rows(
            "SELECT draw, target, why FROM missing_targets WHERE resource = ?1",
            [s.resource],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        );
        entries.extend(
            missing
                .into_iter()
                .map(|(draw, target, why)| Entry::Missing { draw, target, why }),
        );
        let draw = |e: &Entry| match e {
            Entry::Read { draw, .. } | Entry::Missing { draw, .. } => *draw,
        };
        entries.sort_by_key(draw);
    }
    Ok((title, entries))
}

/// Texel (x, y): its channels, and its stencil for a depth-stencil format.
fn value(image: &Image, x: u32, y: u32) -> Option<(Vec<f32>, Option<u8>)> {
    let i = (x < image.width && y < image.height).then_some((y * image.width + x) as usize)?;
    Some((
        image.texels[i][..usize::from(image.channels.max(1))].to_vec(),
        image.stencil.as_ref().map(|s| s[i]),
    ))
}

fn bits(v: &[f32]) -> Vec<u32> {
    v.iter().map(|f| f.to_bits()).collect()
}

/// Texel (x, y) of a target in every readback of it: where each pass left it, and which passes changed it.
pub fn pixel(
    c: &Capture,
    arg: &str,
    x: u32,
    y: u32,
    sub: Option<u32>,
    changed_only: bool,
) -> Result<(), String> {
    let s = subject(c, arg, sub)?;
    let (title, entries) = history(c, &s)?;
    out!(
        "{title}, texel ({x}, {y}): {} readbacks and missing pass ends",
        entries.len()
    );
    out!("{:>5} {:>6} {:<6} value", "pass", "draw", "target");
    let mut last: Option<Vec<u32>> = None;
    for e in &entries {
        match e {
            Entry::Read {
                draw,
                pass,
                target,
                image,
                size,
            } => {
                let pass = pass.map_or("-".into(), |p| p.to_string());
                let v = match image {
                    Ok(image) => {
                        value(image, x, y).ok_or_else(|| format!("outside its {}x{}", size.0, size.1))
                    }
                    Err(e) => Err(e.clone()),
                };
                let (shown, note) = match &v {
                    Ok((v, stencil)) => {
                        let mut now = bits(v);
                        now.extend(stencil.map(u32::from));
                        let note = match &last {
                            None => "first readback",
                            Some(l) if *l != now => "changed",
                            Some(_) => "",
                        };
                        let changed = last.as_ref() != Some(&now);
                        last = Some(now);
                        if changed_only && !changed {
                            continue;
                        }
                        let values = v.iter().map(|f| f.to_string()).collect::<Vec<_>>().join(" ");
                        (
                            match stencil {
                                Some(s) => format!("depth {values} stencil {s}"),
                                None => values,
                            },
                            note,
                        )
                    }
                    Err(why) => (format!("({why})"), ""),
                };
                out!("{pass:>5} {draw:>6} {target:<6} {shown}  {note}");
            }
            Entry::Missing { draw, target, why } => {
                out!("{:>5} {draw:>6} {target:<6} not read back: {why}", "-");
            }
        }
    }
    Ok(())
}

/// For each readback of a target, how many texels (in `rect`, or all) differ from the readback before, and where.
pub fn changes(c: &Capture, arg: &str, sub: Option<u32>, rect: Option<[u32; 4]>) -> Result<(), String> {
    let s = subject(c, arg, sub)?;
    let (title, entries) = history(c, &s)?;
    let region = rect.map_or("all texels".into(), |[x0, y0, x1, y1]| {
        format!("texels {x0},{y0} to {x1},{y1}")
    });
    out!("{title}: {region} changed since the readback before");
    out!(
        "{:>5} {:>6} {:<6} {:>9}  bounds (x0,y0 to x1,y1)",
        "pass",
        "draw",
        "target",
        "changed"
    );
    let mut last: Option<Image> = None;
    for e in entries {
        match e {
            Entry::Read {
                draw,
                pass,
                target,
                image,
                ..
            } => {
                let pass = pass.map_or("-".into(), |p| p.to_string());
                let image = match image {
                    Ok(i) => i,
                    Err(why) => {
                        out!("{pass:>5} {draw:>6} {target:<6} ({why})");
                        continue;
                    }
                };
                let [x0, y0, x1, y1] = rect.unwrap_or([0, 0, image.width, image.height]);
                let (x1, y1) = (x1.min(image.width), y1.min(image.height));
                let Some(before) = last
                    .as_ref()
                    .filter(|b| b.width == image.width && b.height == image.height)
                else {
                    out!("{pass:>5} {draw:>6} {target:<6} {:>9}", "first");
                    last = Some(image);
                    continue;
                };
                let (mut n, mut bounds) = (0u64, [u32::MAX, u32::MAX, 0, 0]);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let i = (y * image.width + x) as usize;
                        let differs = image.texels[i]
                            .iter()
                            .zip(&before.texels[i])
                            .any(|(a, b)| a.to_bits() != b.to_bits())
                            || image.stencil.as_ref().map(|s| s[i]) != before.stencil.as_ref().map(|s| s[i]);
                        if differs {
                            n += 1;
                            bounds = [
                                bounds[0].min(x),
                                bounds[1].min(y),
                                bounds[2].max(x + 1),
                                bounds[3].max(y + 1),
                            ];
                        }
                    }
                }
                let at = if n == 0 {
                    String::new()
                } else {
                    format!("{},{} to {},{}", bounds[0], bounds[1], bounds[2], bounds[3])
                };
                out!("{pass:>5} {draw:>6} {target:<6} {n:>9}  {at}");
                last = Some(image);
            }
            Entry::Missing { draw, target, why } => {
                out!("{:>5} {draw:>6} {target:<6} not read back: {why}", "-");
            }
        }
    }
    Ok(())
}

/// Texel (x, y) of the target a readback holds (`DRAW:TARGET`) after each event of the frame that wrote it up to the
/// readback, by replaying them: which draws inside a pass wrote it, where the readbacks show only each pass's end.
/// The replayed texel at the end is compared with the readback's.
pub fn within(c: &Capture, arg: &str, x: u32, y: u32, sub: Option<u32>, warp: bool) -> Result<(), String> {
    let Some((draw, name)) = arg.split_once(':') else {
        return Err(format!(
            "{arg}: --draws takes DRAW:TARGET, a readback whose pass is replayed"
        ));
    };
    let draw: i64 = draw.parse().map_err(|_| format!("{arg}: not DRAW:TARGET"))?;
    let s = subject(c, arg, sub)?;
    // Captures from before the readback's event was kept: the draw's.
    let has_event = c.count(
        "SELECT count(*) FROM pragma_table_info('targets') WHERE name = 'event'",
        [],
    ) > 0;
    let (event, readback): (i64, Option<String>) = c
        .row(
            &format!(
                "SELECT {}, t.blob FROM targets t JOIN draws d ON d.n = t.draw WHERE t.draw = ?1 AND t.target = ?2",
                if has_event { "coalesce(t.event, d.event)" } else { "d.event" }
            ),
            rusqlite::params![draw, name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok_or_else(|| format!("no {name} read back after draw {draw}"))?;
    let shape =
        crate::describe::shape(c, s.resource).ok_or_else(|| format!("no resource #{}", s.resource))?;
    let subs = shape.subs();
    let place = subs
        .iter()
        .find(|p| i64::from(p.index) == s.sub)
        .ok_or_else(|| format!("#{} has no subresource {}", s.resource, s.sub))?;
    let block = format::layout(format::readable(shape.format))
        .filter(|l| l.block_width == 1 && l.block_height == 1)
        .ok_or_else(|| {
            format!(
                "{} is block-compressed: --draws reads plain formats",
                format::name(shape.format)
            )
        })?
        .block_bytes;
    if x >= place.width || y >= place.height {
        return Err(format!(
            "({x}, {y}) is outside its {}x{}",
            place.width, place.height
        ));
    }
    let at = (place.offset
        + u64::try_from(s.z).unwrap_or(0) * place.slice_bytes()
        + u64::from(y) * u64::from(place.row_bytes)
        + u64::from(x) * u64::from(block)) as usize;
    // A texel as its channels' bits, and its stencil for a depth-stencil format.
    let texel = |bytes: &[u8]| -> Result<Vec<u32>, String> {
        let one = bytes
            .get(at..at + block as usize)
            .ok_or_else(|| format!("{} bytes, the texel at {at}", bytes.len()))?;
        let image = texels::decode(shape.format, 1, 1, one)?;
        let (v, stencil) = value(&image, 0, 0).ok_or("no texel")?;
        let mut words = bits(&v);
        words.extend(stencil.map(u32::from));
        Ok(words)
    };
    let stencil = matches!(shape.format, 19 | 20 | 44 | 45);
    let shown = |words: &[u32]| -> String {
        let (v, s) = match words.split_last() {
            Some((s, v)) if stencil => (v, Some(s)),
            _ => (words, None),
        };
        let values = v
            .iter()
            .map(|b| f32::from_bits(*b).to_string())
            .collect::<Vec<_>>()
            .join(" ");
        match s {
            Some(s) => format!("depth {values} stencil {s}"),
            None => values,
        }
    };
    let writers: std::collections::HashSet<i64> = c
        .rows(
            "SELECT event FROM event_resources WHERE resource = ?1 AND access = 'write' AND event <= ?2",
            [s.resource, event],
            |r| r.get(0),
        )
        .into_iter()
        .collect();
    let first: Option<Vec<u32>> = c
        .row("SELECT content FROM resources WHERE id = ?1", [s.resource], |r| {
            r.get::<_, Option<String>>(0)
        })
        .flatten()
        .and_then(|k| c.blob(&k).ok())
        .and_then(|b| texel(&b).ok());
    let plan = crate::replay::plan_before(c, s.resource, event + 1);
    let mut after: Vec<(i64, Result<Vec<u32>, String>)> = Vec::new();
    let warnings = crate::replay::run(
        c,
        &plan,
        &[],
        warp,
        crate::replay::Validation::Off,
        |seq, b| {
            if writers.contains(&seq) {
                after.push((seq, b.read(s.resource).and_then(|bytes| texel(&bytes))));
            }
            Ok(())
        },
        |_| Ok(()),
    )?;
    out!(
        "#{} sub {} ({}), texel ({x}, {y}): {} events wrote it up to {name} after draw {draw}, replayed",
        s.resource,
        s.sub,
        format::name(shape.format),
        after.len()
    );
    out!(
        "  as the frame began: {}",
        first.as_deref().map_or("(no contents)".to_string(), shown)
    );
    out!("{:>7} {:>6} {:>5} {:<28} value", "event", "draw", "pass", "call");
    let mut last = first.clone();
    let mut changes = 0;
    for (seq, v) in &after {
        let (n, pass, call): (Option<i64>, Option<i64>, String) = c
            .row(
                "SELECT d.n, d.pass, e.call FROM events e LEFT JOIN draws d ON d.event = e.seq WHERE e.seq = ?1",
                [seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap_or((None, None, "?".into()));
        let o = |v: Option<i64>| v.map_or("-".to_string(), |v| v.to_string());
        match v {
            Ok(words) if last.as_ref() == Some(words) => {}
            Ok(words) => {
                changes += 1;
                out!("{seq:>7} {:>6} {:>5} {call:<28} {}", o(n), o(pass), shown(words));
                last = Some(words.clone());
            }
            Err(e) => out!("{seq:>7} {:>6} {:>5} {call:<28} ({e})", o(n), o(pass)),
        }
    }
    out!(
        "({changes} of the {} writes changed it; the rest left it as it was)",
        after.len()
    );
    let recorded = readback.and_then(|k| c.blob(&k).ok()).and_then(|bytes| {
        let image = texels::decode(shape.format, place.width, place.height, &bytes).ok()?;
        let (v, st) = value(&image, x, y)?;
        let mut w = bits(&v);
        w.extend(st.map(u32::from));
        Some(w)
    });
    match (recorded, &last) {
        (Some(r), Some(l)) if r == *l => out!("the replayed texel is the readback's"),
        (Some(r), Some(l)) => out!(
            "the replayed texel ({}) is not the readback's ({}): the replay is not what the game drew here",
            shown(l),
            shown(&r)
        ),
        _ => out!("(no readback texel to compare with)"),
    }
    for w in warnings {
        eprintln!("warning: {w}");
    }
    Ok(())
}
