//! Replaying a captured frame offline, with or without rules changing it. What runs is planned here from the capture
//! alone (API-neutral); a backend for the capture's API makes the resources and runs the events
//! ([`d3d11`] for Direct3D 11).
//!
//! A draw is replayed with what it depends on: walking back from it, every earlier event that wrote something it (or
//! an event already taken) reads or writes is taken too, so the resources it sees hold what they held in the game.
//! Resources start with their contents as the frame began.

mod d3d11;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use locum_capture::format;
use locum_capture::rules::{self, Rule};
use locum_capture::texels::{self, Range};

use crate::describe::{Shape, mib, shape};
use crate::export::png;
use crate::open::Capture;
use crate::out;

/// What a backend for one graphics API does.
pub trait Backend {
    /// Makes resource `id` as the capture describes it, holding `contents` (the canonical layout) if given.
    fn create(&mut self, c: &Capture, id: i64, contents: Option<&[u8]>) -> Result<(), String>;
    /// Runs event `seq` as the capture recorded it, draws and dispatches with `rules` applied.
    fn run(&mut self, c: &Capture, seq: i64, rules: &[Rule]) -> Result<(), String>;
    /// Resource `id`'s contents now, in the canonical layout.
    fn read(&mut self, id: i64) -> Result<Vec<u8>, String>;
    /// What it could not do as recorded, once each.
    fn warnings(&self) -> Vec<String>;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// The draw and the earlier events it depends on.
    Slice,
    /// Every event up to the draw.
    Frame,
    /// The draw alone, on contents as the frame began.
    Draw,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        match s {
            "slice" => Some(Mode::Slice),
            "frame" => Some(Mode::Frame),
            "draw" => Some(Mode::Draw),
            _ => None,
        }
    }
}

/// The resources an event reads and writes. A draw's constant buffers are not among its reads: replay binds the
/// bytes the GPU read, captured at the draw.
fn touches(c: &Capture) -> BTreeMap<i64, (BTreeSet<i64>, BTreeSet<i64>)> {
    let mut out: BTreeMap<i64, (BTreeSet<i64>, BTreeSet<i64>)> = BTreeMap::new();
    for seq in c.rows("SELECT seq FROM events", [], |r| r.get::<_, i64>(0)) {
        out.insert(seq, Default::default());
    }
    for (event, resource, access) in c.rows("SELECT event, resource, access FROM event_resources", [], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?))
    }) {
        let e = out.entry(event).or_default();
        if access == "write" {
            e.1.insert(resource);
        } else {
            e.0.insert(resource);
        }
    }
    // Drop reads that are only constant buffers of the draw.
    let cb_only: Vec<(i64, i64)> = c.rows(
        "SELECT d.event, b.resource FROM bindings b JOIN draws d ON d.n = b.draw WHERE b.resource IS NOT NULL \
         GROUP BY d.event, b.resource HAVING sum(b.kind <> 'cb') = 0",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    for (event, resource) in cb_only {
        if let Some(e) = out.get_mut(&event) {
            e.0.remove(&resource);
        }
    }
    out
}

pub struct Plan {
    pub events: Vec<i64>,
    pub resources: BTreeSet<i64>,
}

/// The events to run to replay event `target`, and the resources they touch.
pub fn plan(c: &Capture, target: i64, mode: Mode) -> Plan {
    let touches = touches(c);
    let mut events: Vec<i64> = Vec::new();
    let mut resources: BTreeSet<i64> = BTreeSet::new();
    let mut take = |seq: i64, resources: &mut BTreeSet<i64>| {
        if let Some((r, w)) = touches.get(&seq) {
            resources.extend(r);
            resources.extend(w);
        }
        events.push(seq);
    };
    match mode {
        Mode::Draw => take(target, &mut resources),
        Mode::Frame => {
            for &seq in touches.keys().filter(|&&s| s <= target) {
                take(seq, &mut resources);
            }
        }
        Mode::Slice => {
            take(target, &mut resources);
            for (&seq, (_, writes)) in touches.range(..target).rev() {
                if writes.iter().any(|w| resources.contains(w)) {
                    take(seq, &mut resources);
                }
            }
            events.reverse();
        }
    }
    // Everything a draw has bound, and the resource of every view an event names, in one query (one per event
    // scans the bindings once per event: minutes on a game's frame).
    let mut bound: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for (event, resource) in c.rows(
        "SELECT DISTINCT d.event, b.resource FROM bindings b JOIN draws d ON d.n = b.draw \
         WHERE b.resource IS NOT NULL AND b.kind <> 'cb'",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)),
    ) {
        bound.entry(event).or_default().push(resource);
    }
    for seq in &events {
        if let Some(b) = bound.get(seq) {
            resources.extend(b);
        }
    }
    Plan { events, resources }
}

/// One output of a draw: a target or UAV and the subresource of its resource it covers.
struct Output {
    name: String,
    resource: i64,
    sub: Option<u32>,
}

fn outputs(c: &Capture, draw: i64) -> Vec<Output> {
    c.rows(
        "SELECT b.kind, b.slot, b.resource, v.json FROM bindings b LEFT JOIN views v ON v.id = b.view \
         WHERE b.draw = ?1 AND b.kind IN ('rtv', 'dsv', 'uav') AND b.resource IS NOT NULL ORDER BY b.kind DESC, b.slot",
        [draw],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        },
    )
    .into_iter()
    .map(|(kind, slot, resource, json)| {
        let name = match kind.as_str() {
            "rtv" => format!("rt{slot}"),
            "dsv" => "depth".into(),
            _ => format!("u{slot}"),
        };
        let range = json
            .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
            .map(|j| j["range"].clone())
            .unwrap_or_default();
        let sub = shape(c, resource).and_then(|s| {
            (s.kind != "buffer").then(|| {
                let mip = range["mip"].as_u64().unwrap_or(0) as u32;
                let slice = range["first_slice"].as_u64().unwrap_or(0) as u32;
                mip + slice * s.mips.max(1)
            })
        });
        Output {
            name,
            resource,
            sub,
        }
    })
    .collect()
}

/// A subresource's bytes and shape out of a resource's canonical contents.
fn sub_of<'a>(
    s: &Shape,
    bytes: &'a [u8],
    sub: Option<u32>,
) -> Option<(&'a [u8], locum_capture::layout::Sub)> {
    let subs = s.subs();
    let x = *subs.iter().find(|x| Some(x.index) == sub.or(Some(0)))?;
    Some((bytes.get(x.offset as usize..(x.offset + x.size) as usize)?, x))
}

/// How two images of the same format differ: texels that differ, of how many, the largest difference in a decoded
/// channel, and the box holding the differing texels.
struct Difference {
    differ: u64,
    total: u64,
    largest: f32,
    bounds: Option<(u32, u32, u32, u32)>,
}

fn compare(format: u32, sub: &locum_capture::layout::Sub, a: &[u8], b: &[u8]) -> Difference {
    let block = format::layout(format)
        .map_or(4, |l| l.block_bytes as usize)
        .max(1);
    let (bw, bh) = format::layout(format).map_or((1, 1), |l| (l.block_width, l.block_height));
    let per_row = (sub.row_bytes as usize / block).max(1);
    let mut d = Difference {
        differ: 0,
        total: (a.len() / block) as u64,
        largest: 0.0,
        bounds: None,
    };
    for (i, (x, y)) in a.chunks(block).zip(b.chunks(block)).enumerate() {
        if x != y {
            d.differ += 1;
            let (px, py) = (
                (i % per_row) as u32 * bw,
                ((i / per_row) % sub.rows as usize) as u32 * bh,
            );
            d.bounds = Some(match d.bounds {
                None => (px, py, px + bw, py + bh),
                Some((l, t, r, btm)) => (l.min(px), t.min(py), r.max(px + bw), btm.max(py + bh)),
            });
        }
    }
    if d.differ > 0
        && let (Ok(ia), Ok(ib)) = (
            texels::decode(format, sub.width, sub.height, a),
            texels::decode(format, sub.width, sub.height, b),
        )
    {
        for (x, y) in ia.texels.iter().zip(&ib.texels) {
            for k in 0..4 {
                let diff = (x[k] - y[k]).abs();
                if diff.is_finite() {
                    d.largest = d.largest.max(diff);
                }
            }
        }
    }
    d
}

impl std::fmt::Display for Difference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.differ == 0 {
            return write!(f, "identical");
        }
        write!(
            f,
            "{} of {} texels differ ({:.3}%), largest channel difference {}",
            self.differ,
            self.total,
            self.differ as f64 * 100.0 / self.total.max(1) as f64,
            self.largest
        )?;
        if let Some((l, t, r, b)) = self.bounds {
            write!(f, ", within x {l}..{r} y {t}..{b}")?;
        }
        Ok(())
    }
}

/// A picture of where two images differ: unchanged texels dim grey, changed ones as they are now tinted magenta, so a
/// change to black still shows.
fn diff_png(
    path: &Path,
    format: u32,
    sub: &locum_capture::layout::Sub,
    before: &[u8],
    after: &[u8],
    range: Range,
) -> Result<(), String> {
    let a = texels::decode(format, sub.width, sub.height, before)?;
    let b = texels::decode(format, sub.width, sub.height, after)?;
    let (pa, pb) = (a.rgba8(range), b.rgba8(range));
    let mut out = Vec::with_capacity(pb.len());
    for (x, y) in pa.chunks(4).zip(pb.chunks(4)) {
        if x == y {
            let grey = ((u16::from(y[0]) + u16::from(y[1]) + u16::from(y[2])) / 12) as u8;
            out.extend([grey, grey, grey, 255]);
        } else {
            let tint = |c: u8, t: u16| ((u16::from(c) + t) / 2) as u8;
            out.extend([tint(y[0], 255), tint(y[1], 0), tint(y[2], 255), 255]);
        }
    }
    png(path, sub.width, sub.height, &out)
}

fn write_output(
    dir: &Path,
    name: &str,
    s: &Shape,
    bytes: &[u8],
    sub: Option<u32>,
    range: Range,
) -> Result<PathBuf, String> {
    if s.kind == "buffer" {
        let path = dir.join(format!("{name}.bin"));
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(path);
    }
    let (data, x) = sub_of(s, bytes, sub).ok_or("no such subresource")?;
    let path = dir.join(format!("{name}.png"));
    let image = texels::decode(s.format, x.width, x.height, &data[..x.slice_bytes() as usize])?;
    png(&path, x.width, x.height, &image.rgba8(range))?;
    Ok(path)
}

/// Makes the plan's resources on a new backend and runs its events with `rules`; returns each output's contents.
fn execute(
    c: &Capture,
    plan: &Plan,
    rules: &[Rule],
    warp: bool,
    outs: &[Output],
) -> Result<(Vec<Vec<u8>>, Vec<String>), String> {
    let mut b = backend(c, warp)?;
    let mut missing = 0;
    for &id in &plan.resources {
        let key: Option<String> = c
            .row("SELECT content FROM resources WHERE id = ?1", [id], |r| r.get(0))
            .flatten();
        let contents = match key {
            Some(k) => Some(c.blob(&k)?),
            None => {
                missing += 1;
                None
            }
        };
        b.create(c, id, contents.as_deref())
            .map_err(|e| format!("resource #{id}: {e}"))?;
    }
    for &seq in &plan.events {
        b.run(c, seq, rules).map_err(|e| format!("event {seq}: {e}"))?;
    }
    let mut results = Vec::new();
    for o in outs {
        results.push(b.read(o.resource)?);
    }
    let mut warnings = b.warnings();
    if missing > 0 {
        warnings.push(format!(
            "{missing} resources had no contents in the capture and started empty"
        ));
    }
    Ok((results, warnings))
}

fn backend(c: &Capture, warp: bool) -> Result<Box<dyn Backend>, String> {
    match c.meta("api").as_deref() {
        Some("d3d11") => Ok(Box::new(d3d11::D3d11::new(warp)?)),
        other => Err(format!("no replay for API {other:?}")),
    }
}

/// The draw named on the command line: a number, or `last`.
pub fn draw_number(c: &Capture, arg: &str) -> Result<i64, String> {
    if arg == "last" {
        return c
            .row("SELECT max(n) FROM draws", [], |r| r.get::<_, Option<i64>>(0))
            .flatten()
            .ok_or_else(|| "the capture has no draws".into());
    }
    arg.parse()
        .map_err(|_| format!("{arg:?} is not a draw number or `last`"))
}

pub struct Options {
    pub mode: Mode,
    pub rules: Vec<String>,
    pub rules_file: Option<PathBuf>,
    pub warp: bool,
    pub out: PathBuf,
    pub range: Range,
}

pub fn replay(c: &Capture, draw: i64, o: &Options) -> Result<(), String> {
    let target: i64 = c
        .row("SELECT event FROM draws WHERE n = ?1", [draw], |r| r.get(0))
        .ok_or_else(|| format!("no draw {draw}"))?;
    let mut parsed = Vec::new();
    if let Some(file) = &o.rules_file {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let dir = file.parent().unwrap_or(Path::new("."));
        let (r, errors) = rules::parse_all(&text, dir);
        if let Some((line, e)) = errors.first() {
            return Err(format!("{} line {line}: {e}", file.display()));
        }
        parsed.extend(r);
    }
    for text in &o.rules {
        parsed.push(rules::parse(text, Path::new("."))?);
    }
    let plan = plan(c, target, o.mode);
    let bytes: u64 = plan
        .resources
        .iter()
        .filter_map(|&id| shape(c, id))
        .map(|s| s.bytes)
        .sum();
    out!(
        "replaying draw {draw} (event {target}): {} events, {} resources ({}){}",
        plan.events.len(),
        plan.resources.len(),
        mib(bytes),
        if o.warp { " on WARP" } else { "" }
    );
    if o.mode == Mode::Draw {
        let earlier: Vec<i64> = c.rows(
            "SELECT DISTINCT resource FROM event_resources WHERE access = 'write' AND event < ?1",
            [target],
            |r| r.get(0),
        );
        let stale: Vec<String> = plan
            .resources
            .iter()
            .filter(|r| earlier.contains(r))
            .map(|r| format!("#{r}"))
            .collect();
        if !stale.is_empty() {
            out!(
                "  note: {} were written earlier in the frame; this draw sees them as the frame began",
                stale.join(" ")
            );
        }
    }
    let outs = outputs(c, draw);
    if outs.is_empty() {
        return Err(format!("draw {draw} has no targets or UAVs to show"));
    }
    std::fs::create_dir_all(&o.out).map_err(|e| format!("{}: {e}", o.out.display()))?;
    let t = std::time::Instant::now();
    let (base, warnings) = execute(c, &plan, &[], o.warp, &outs)?;
    out!("  replayed in {:.2} s", t.elapsed().as_secs_f64());
    for w in &warnings {
        out!("  warning: {w}");
    }
    let edited = if parsed.is_empty() {
        None
    } else {
        let (after, _) = execute(c, &plan, &parsed, o.warp, &outs)?;
        Some(after)
    };
    // Captured targets after this draw, to compare the replay against.
    let captured: HashMap<String, String> = c
        .rows(
            "SELECT target, blob FROM targets WHERE draw = ?1 AND blob IS NOT NULL",
            [draw],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .into_iter()
        .collect();
    for (i, output) in outs.iter().enumerate() {
        let s = shape(c, output.resource).ok_or("resource vanished")?;
        let suffix = if edited.is_some() { ".before" } else { "" };
        let path = write_output(
            &o.out,
            &format!("{}{suffix}", output.name),
            &s,
            &base[i],
            output.sub,
            o.range,
        )?;
        out!("{}: {s} -> {}", output.name, path.display());
        if s.kind == "buffer" {
            continue;
        }
        let Some((now, x)) = sub_of(&s, &base[i], output.sub) else {
            continue;
        };
        let fmt = format::readable(s.format);
        if let Some(key) = captured.get(&output.name)
            && let Ok(game) = c.blob(key)
        {
            out!("  against the game's: {}", compare(fmt, &x, &game, now));
        }
        if let Some(after) = &edited {
            let Some((then, _)) = sub_of(&s, &after[i], output.sub) else {
                continue;
            };
            let p = write_output(
                &o.out,
                &format!("{}.after", output.name),
                &s,
                &after[i],
                output.sub,
                o.range,
            )?;
            let d = o.out.join(format!("{}.diff.png", output.name));
            diff_png(&d, fmt, &x, now, then, o.range)?;
            out!(
                "  with the rules: {} -> {}, {}",
                compare(fmt, &x, now, then),
                p.display(),
                d.display()
            );
        }
    }
    Ok(())
}

/// Gaps that are checks the capture could not make or keep (targets not read back, a command list's earlier
/// executions, the CPU's memory), not inputs the replay lacks: shown, but they leave the proof complete.
const CHECKS: &str = "'evidence', 'targets', 'memory'";

/// Replays the whole frame and compares every target the capture read back with the replay's at the same point. Fails
/// unless every target compared is identical and nothing kept the capture or the replay from being complete.
pub fn verify(c: &Capture, warp: bool, limit: Option<i64>) -> Result<(), String> {
    let last: i64 = c
        .row("SELECT max(seq) FROM events", [], |r| r.get::<_, Option<i64>>(0))
        .flatten()
        .ok_or("the capture has no events")?;
    let started = std::time::Instant::now();
    let plan = plan(c, last, Mode::Frame);
    let planned = started.elapsed();
    // Each target is compared after the event it was read after (in captures from before that was recorded, its draw).
    let has_event = c
        .row(
            "SELECT count(*) FROM pragma_table_info('targets') WHERE name = 'event'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_some_and(|n| n > 0);
    let at = if has_event {
        "coalesce(t.event, d.event)"
    } else {
        "d.event"
    };
    let checks: Vec<(i64, i64, String, i64, String, i64)> = c.rows(
        &format!(
            "SELECT t.draw, {at} AS at, t.target, t.resource, t.blob, coalesce(t.sub, 0) FROM targets t \
             JOIN draws d ON d.n = t.draw WHERE t.blob IS NOT NULL AND t.resource IS NOT NULL ORDER BY at"
        ),
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    );
    out!(
        "replaying {} events over {} resources; {} targets to compare",
        plan.events.len(),
        plan.resources.len(),
        checks.len()
    );
    let started = std::time::Instant::now();
    let mut b = backend(c, warp)?;
    for &id in &plan.resources {
        let key: Option<String> = c
            .row("SELECT content FROM resources WHERE id = ?1", [id], |r| r.get(0))
            .flatten();
        let contents = match key {
            Some(k) => Some(c.blob(&k)?),
            None => None,
        };
        b.create(c, id, contents.as_deref())
            .map_err(|e| format!("resource #{id}: {e}"))?;
    }
    out!(
        "planned in {:.1} s; device and resources made in {:.1} s",
        planned.as_secs_f64(),
        started.elapsed().as_secs_f64()
    );
    let mut next = 0;
    let (mut same, mut compared) = (0, 0);
    let t = std::time::Instant::now();
    for &seq in &plan.events {
        b.run(c, seq, &[]).map_err(|e| format!("event {seq}: {e}"))?;
        while next < checks.len() && checks[next].1 == seq {
            let (draw, _, name, resource, key, sub) = &checks[next];
            next += 1;
            if limit.is_some_and(|l| compared >= l) {
                continue;
            }
            let s = shape(c, *resource).ok_or("resource vanished")?;
            let now = b.read(*resource)?;
            let Some((mine, x)) = sub_of(&s, &now, Some(*sub as u32)) else {
                continue;
            };
            let game = c.blob(key)?;
            let d = compare(format::readable(s.format), &x, &game, mine);
            compared += 1;
            if d.differ == 0 {
                same += 1;
            }
            out!("draw {draw:>5} {name:<6} {s}: {d}");
        }
    }
    out!(
        "{same} of {compared} targets identical; replayed in {:.1} s",
        t.elapsed().as_secs_f64()
    );
    // What keeps an identical replay from proving the frame: what the capture could not record, used resources
    // without contents, and events replay could not run as recorded.
    for note in c.rows(
        &format!("SELECT what, coalesce(detail, ''), count FROM gaps WHERE what IN ({CHECKS}) ORDER BY what, detail"),
        [],
        |r| {
            Ok(format!(
                "{} {} ({}x)",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?
            ))
        },
    ) {
        out!("not compared: {note}");
    }
    let mut incomplete: Vec<String> = c.rows(
        &format!("SELECT what, coalesce(detail, ''), count FROM gaps WHERE what NOT IN ({CHECKS}) ORDER BY what, detail"),
        [],
        |r| {
            Ok(format!(
                "capture gap: {} {} ({}x)",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?
            ))
        },
    );
    let missing: i64 = c
        .row(
            "SELECT count(*) FROM resources WHERE used = 1 AND content IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if missing > 0 {
        incomplete.push(format!("{missing} resources the frame used have no contents"));
    }
    incomplete.extend(b.warnings().into_iter().map(|w| format!("replay: {w}")));
    for i in &incomplete {
        out!("incomplete: {i}");
    }
    if limit.is_some_and(|l| compared >= l) {
        out!("(stopped after {compared} comparisons: --limit)");
    }
    if same < compared {
        return Err(format!("{} of {compared} targets differ", compared - same));
    }
    if compared == 0 {
        return Err("no target was compared".into());
    }
    if !incomplete.is_empty() {
        return Err(format!(
            "every target identical, but the capture is incomplete ({} reasons above)",
            incomplete.len()
        ));
    }
    Ok(())
}
