//! Replaying a captured frame offline, with or without rules changing it. What runs is planned here from the capture
//! alone (API-neutral); a backend for the capture's API makes the resources and runs the events
//! ([`d3d11`] for Direct3D 11).
//!
//! A draw is replayed with what it depends on: walking back from it, every earlier event that wrote something it (or
//! an event already taken) reads or writes is taken too, so the resources it sees hold what they held in the game.
//! Resources start with their contents as the frame began.

mod d3d11;
mod d3d12;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use locum_capture::environment::Environment;
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
    /// Replaces resource `id`'s contents with `contents` (the canonical layout).
    fn write(&mut self, c: &Capture, id: i64, contents: &[u8]) -> Result<(), String>;
    /// What it could not do as recorded, once each.
    fn warnings(&self) -> Vec<String>;
    /// The adapter, driver and runtime it replays on.
    fn environment(&self) -> locum_capture::environment::Environment;
    /// The errors the API's validation reported so far (none without it).
    fn validation(&self) -> Vec<String> {
        Vec::new()
    }
    /// What it did otherwise than recorded that nothing the GPU ran can depend on (results only the CPU reads), so
    /// no compared output does; said, but the capture is not incomplete for it.
    fn notes(&self) -> Vec<String> {
        Vec::new()
    }
    /// Brings what the API holds outside events up to just before event `seq` (a Direct3D 12 queue's changes to
    /// tile mappings, first-use contents waiting for their submission), where no event the plan ran has.
    fn reach(&mut self, _c: &Capture, _seq: i64) -> Result<(), String> {
        Ok(())
    }
}

/// The checks of the API a replay runs under.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Validation {
    Off,
    /// The debug layer: each call checked as it is made.
    Layer,
    /// The debug layer and, in Direct3D 12, GPU-based validation: descriptors, textures' layouts and bounds checked as
    /// the shaders use them, by shaders it patches; buffers' states are not checked. Patched shaders round some results
    /// otherwise than the game's (in Elden Ring, an SSAO pixel shader with no UAVs left 7 of 921600 texels one step
    /// off), so what a replay under it writes is not the game's to the bit.
    Gpu,
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

/// Whether replay binds a draw's constant buffers from the bytes the capture took at the draw (Direct3D 11), rather
/// than through its own copies of the buffers (Direct3D 12, whose views name buffers by address). Then a buffer a
/// draw reads only as constants is not among what it needs.
fn captured_constants(c: &Capture) -> bool {
    c.meta("api").as_deref() == Some("d3d11")
}

/// The resources an event reads and writes. Where replay binds captured constants, a draw's constant buffers are not
/// among its reads.
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
    // A write to memory resources share writes each of them. The frame's queries are resources of their own.
    if c.meta("api").as_deref() == Some("d3d12") {
        for (event, reads, writes) in d3d12::query_touches(c) {
            let e = out.entry(event).or_default();
            e.0.extend(reads);
            e.1.extend(writes);
        }
        // An acceleration structure names those it is built of by address (a top-level one its instances' bottom
        // levels), which the plan cannot see: what uses one uses them all.
        let structures = d3d12::acceleration_structures(c);
        if !structures.is_empty() {
            for (read, written) in out.values_mut() {
                if read.iter().chain(written.iter()).any(|r| structures.contains(r)) {
                    read.extend(structures.iter().filter(|r| !written.contains(r)));
                }
            }
        }
        let aliases = d3d12::aliases(c);
        if !aliases.is_empty() {
            for (_, written) in out.values_mut() {
                let shared: Vec<i64> = written
                    .iter()
                    .filter_map(|r| aliases.get(r))
                    .flatten()
                    .copied()
                    .collect();
                written.extend(shared);
            }
        }
    }
    if !captured_constants(c) {
        return out;
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
    /// The event the replay is read just before, when it runs no event that far.
    pub until: Option<i64>,
}

/// The events to run to replay event `target`, and the resources they touch.
pub fn plan(c: &Capture, target: i64, mode: Mode) -> Plan {
    let touches = touches(c);
    let mut events: Vec<i64> = Vec::new();
    let mut resources: BTreeSet<i64> = BTreeSet::new();
    match mode {
        Mode::Draw => take(&touches, target, &mut events, &mut resources),
        Mode::Frame => {
            for &seq in touches.keys().filter(|&&s| s <= target) {
                take(&touches, seq, &mut events, &mut resources);
            }
        }
        Mode::Slice => {
            take(&touches, target, &mut events, &mut resources);
            walk_back(&touches, target, &mut events, &mut resources);
        }
    }
    with_bound(c, events, resources)
}

/// The events to run for resource `id` to hold what it held just before event `before`: every earlier event that
/// wrote it, or wrote what such an event read, and so on.
pub fn plan_before(c: &Capture, id: i64, before: i64) -> Plan {
    let touches = touches(c);
    let (mut events, mut resources) = (Vec::new(), BTreeSet::from([id]));
    walk_back(&touches, before, &mut events, &mut resources);
    Plan {
        until: Some(before),
        ..with_bound(c, events, resources)
    }
}

type Touches = BTreeMap<i64, (BTreeSet<i64>, BTreeSet<i64>)>;

fn take(touches: &Touches, seq: i64, events: &mut Vec<i64>, resources: &mut BTreeSet<i64>) {
    if let Some((r, w)) = touches.get(&seq) {
        resources.extend(r);
        resources.extend(w);
    }
    events.push(seq);
}

/// Takes, in order, every event before `before` that wrote a resource taken so far (taking what it touches too).
fn walk_back(touches: &Touches, before: i64, events: &mut Vec<i64>, resources: &mut BTreeSet<i64>) {
    let mut earlier = Vec::new();
    for (&seq, (_, writes)) in touches.range(..before).rev() {
        if writes.iter().any(|w| resources.contains(w)) {
            take(touches, seq, &mut earlier, resources);
        }
    }
    earlier.reverse();
    events.splice(0..0, earlier);
}

/// The plan, with every resource the planned draws have bound.
fn with_bound(c: &Capture, events: Vec<i64>, mut resources: BTreeSet<i64>) -> Plan {
    // Everything a draw has bound, and the resource of every view an event names, in one query (one per event
    // scans the bindings once per event: minutes on a game's frame).
    let mut bound: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    let constants = if captured_constants(c) {
        "AND b.kind <> 'cb'"
    } else {
        ""
    };
    for (event, resource) in c.rows(
        &format!(
            "SELECT DISTINCT d.event, b.resource FROM bindings b JOIN draws d ON d.n = b.draw \
             WHERE b.resource IS NOT NULL {constants}"
        ),
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
    // Queries are resources only to the planner (negative ids, d3d12::query_touches).
    resources.retain(|&r| r >= 0);
    Plan {
        events,
        resources,
        until: None,
    }
}

/// Resource `id`'s contents just before event `before` (canonical layout), replayed from the frame's start with the
/// events that led to them; the number of events run, and what the replay could not do as recorded.
pub fn contents_at(
    c: &Capture,
    id: i64,
    before: i64,
    warp: bool,
) -> Result<(Vec<u8>, usize, Vec<String>), String> {
    let plan = plan_before(c, id, before);
    let out = Output {
        name: format!("#{id}"),
        resource: id,
        sub: None,
        z: 0,
    };
    let (mut bytes, warnings) = execute(c, &plan, &[], warp, Validation::Off, &[out])?;
    let contents = bytes.remove(&id).ok_or("the replay did not read it back")?;
    Ok((contents, plan.events.len(), warnings))
}

/// One output of a draw: a target or UAV and the subresource of its resource it covers.
struct Output {
    name: String,
    resource: i64,
    sub: Option<u32>,
    z: u32,
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
    .flat_map(|(kind, slot, resource, json)| {
        let name = match kind.as_str() {
            "rtv" => format!("rt{slot}"),
            "dsv" => "depth".into(),
            _ => format!("u{slot}"),
        };
        let range = json
            .and_then(|j| serde_json::from_str::<serde_json::Value>(&j).ok())
            .map(|j| j["range"].clone())
            .unwrap_or_default();
        let Some(s) = shape(c, resource) else { return Vec::new(); };
        if s.kind == "buffer" {
            return vec![Output { name, resource, sub: None, z: 0 }];
        }
        let mip = range["mip"].as_u64().unwrap_or(0) as u32;
        let volume = s.kind == "texture3d";
        let first = range[if volume { "first_w" } else { "first_slice" }].as_u64().unwrap_or(0) as u32;
        let available = if volume { (s.depth >> mip).max(1) } else { s.array.max(1) };
        let count = (range[if volume { "w" } else { "slices" }].as_u64().unwrap_or(1) as u32)
            .min(available.saturating_sub(first));
        (first..first + count).map(|slice| Output {
            name: if count > 1 || volume {
                format!("{name}[{}{slice}]", if volume { "z" } else { "slice" })
            } else { name.clone() },
            resource,
            sub: Some(if volume { mip } else { mip + slice * s.mips.max(1) }),
            z: if volume { slice } else { 0 },
        }).collect::<Vec<_>>()
    })
    .collect()
}

/// A subresource's bytes and shape out of a resource's canonical contents.
fn sub_of<'a>(
    s: &Shape,
    bytes: &'a [u8],
    sub: Option<u32>,
) -> Option<(&'a [u8], locum_capture::layout::Sub)> {
    // A multisampled subresource is its samples stacked, as its targets are captured.
    let x = locum_capture::layout::stacked(&s.subs(), sub.unwrap_or(0))?;
    Some((bytes.get(x.offset as usize..(x.offset + x.size) as usize)?, x))
}

/// One depth plane, with a shape describing only those bytes.
fn plane_of<'a>(
    s: &Shape,
    bytes: &'a [u8],
    sub: Option<u32>,
    z: u32,
) -> Option<(&'a [u8], locum_capture::layout::Sub)> {
    let (bytes, mut x) = sub_of(s, bytes, sub)?;
    if z >= x.depth {
        return None;
    }
    let size = x.slice_bytes();
    let start = u64::from(z) * size;
    x.offset += start;
    x.size = size;
    x.depth = 1;
    Some((bytes.get(start as usize..(start + size) as usize)?, x))
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
    output: &Output,
    range: Range,
) -> Result<PathBuf, String> {
    if s.kind == "buffer" {
        let path = dir.join(format!("{name}.bin"));
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        return Ok(path);
    }
    let (data, x) = plane_of(s, bytes, output.sub, output.z).ok_or("no such subresource or depth plane")?;
    let path = dir.join(format!("{name}.png"));
    let image = texels::decode(s.format, x.width, x.height, &data[..x.slice_bytes() as usize])?;
    png(&path, x.width, x.height, &image.rgba8(range))?;
    Ok(path)
}

/// Canonical bytes keyed by resource, shared by all its output views and planes.
type ResourceBytes = HashMap<i64, Vec<u8>>;

/// Makes the plan's resources on a new backend and runs its events with `rules`; returns each output's contents.
fn execute(
    c: &Capture,
    plan: &Plan,
    rules: &[Rule],
    warp: bool,
    validation: Validation,
    outs: &[Output],
) -> Result<(ResourceBytes, Vec<String>), String> {
    let mut results = HashMap::new();
    let warnings = run(
        c,
        plan,
        rules,
        warp,
        validation,
        |_, _| Ok(()),
        |b| {
            for o in outs {
                if let std::collections::hash_map::Entry::Vacant(e) = results.entry(o.resource) {
                    e.insert(b.read(o.resource)?);
                }
            }
            Ok(())
        },
    )?;
    Ok((results, warnings))
}

/// Runs `plan`: makes its resources with their contents as the frame began, then runs its events in order, calling
/// `after` with each event and the backend once it has run (and its order-dependent outputs are put back), and `end`
/// at the end. Gives what the backend could not do as recorded.
pub fn run(
    c: &Capture,
    plan: &Plan,
    rules: &[Rule],
    warp: bool,
    validation: Validation,
    mut after: impl FnMut(i64, &mut dyn Backend) -> Result<(), String>,
    end: impl FnOnce(&mut dyn Backend) -> Result<(), String>,
) -> Result<Vec<String>, String> {
    let (mut b, missing) = made(c, plan, warp, validation)?;
    // Only order-dependent outputs are put back: the game's bytes for any other would undo what a rule changed.
    let written = recorded(c);
    let mut settled = 0;
    for &seq in &plan.events {
        b.run(c, seq, rules).map_err(|e| format!("event {seq}: {e}"))?;
        settled += settle(c, b.as_mut(), &written, seq, false, None)
            .map_err(|e| format!("event {seq}: {e}"))?
            .iter()
            .filter(|s| s.ordered)
            .count();
        after(seq, b.as_mut())?;
    }
    if let Some(until) = plan.until {
        b.reach(c, until)
            .map_err(|e| format!("up to event {until}: {e}"))?;
    }
    end(b.as_mut())?;
    let mut warnings = b.warnings();
    warnings.extend(b.notes());
    warnings.extend(b.validation());
    if missing > 0 {
        warnings.push(format!(
            "{missing} resources had no contents in the capture and started empty"
        ));
    }
    if settled > 0 && !rules.is_empty() {
        warnings.push(format!(
            "{settled} outputs of order-dependent draws and dispatches are the game's, so rules do not change them"
        ));
    }
    Ok(warnings)
}

/// A backend holding the plan's resources with their contents as the frame began, and how many of them had no
/// contents in the capture (tiled resources made of their tile events aside).
fn made(
    c: &Capture,
    plan: &Plan,
    warp: bool,
    validation: Validation,
) -> Result<(Box<dyn Backend>, usize), String> {
    let mut b = backend(c, warp, validation)?;
    let lacking = lacking(c);
    let mut missing = 0;
    for &id in &plan.resources {
        let key: Option<String> = c
            .row("SELECT content FROM resources WHERE id = ?1", [id], |r| r.get(0))
            .flatten();
        let contents = match key {
            Some(k) => Some(c.blob(&k)?),
            None => {
                missing += usize::from(lacking.contains(&id));
                None
            }
        };
        b.create(c, id, contents.as_deref())
            .map_err(|e| format!("resource #{id}: {e}"))?;
    }
    Ok((b, missing))
}

/// The resources whose contents as the frame began matter but the capture does not hold. Not those made of the
/// frame's tile events (a D3D11 tiled resource's pages), nor those on a Direct3D 12 readback heap: the GPU only
/// copies into them, and nothing it runs reads them.
fn lacking(c: &Capture) -> HashSet<i64> {
    let readback = if c.has_table("d12_objects") {
        "AND id NOT IN (SELECT id FROM d12_objects WHERE kind LIKE 'ID3D12Resource%' \
         AND json_extract(description, '$.heap_properties.type') = 3)"
    } else {
        ""
    };
    c.rows(
        &format!(
            "SELECT id FROM resources WHERE content IS NULL AND coalesce(content_source, '') NOT IN ('tile-events', \
             'acceleration-structure-builds') {readback}"
        ),
        [],
        |r| r.get(0),
    )
    .into_iter()
    .collect()
}

/// A resource checkpoint after a draw/dispatch UAV write or a clear.
struct Written {
    resource: i64,
    /// The subresource the game's bytes are of; None, the whole resource.
    sub: Option<u32>,
    key: String,
    /// One of its shaders writes what depends on the order the GPU runs its threads.
    ordered: bool,
}

/// Output checkpoints from draws, dispatches and clears, by event. Captures from before every output was
/// recorded hold only order-dependent ones, without the column saying so; older ones have none.
fn recorded(c: &Capture) -> HashMap<i64, Vec<Written>> {
    let mut out: HashMap<i64, Vec<Written>> = HashMap::new();
    let columns: Vec<String> = c.rows("SELECT name FROM pragma_table_info('results')", [], |r| r.get(0));
    if columns.is_empty() {
        return out;
    }
    let ordered = if columns.iter().any(|n| n == "order_dependent") {
        "order_dependent"
    } else {
        "1"
    };
    let sub = if columns.iter().any(|n| n == "sub") {
        "sub"
    } else {
        "NULL"
    };
    for (event, w) in c.rows(
        &format!("SELECT event, resource, content, {ordered}, {sub} FROM results"),
        [],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                Written {
                    resource: r.get(1)?,
                    key: r.get(2)?,
                    ordered: r.get(3)?,
                    sub: r.get(4)?,
                },
            ))
        },
    ) {
        out.entry(event).or_default().push(w);
    }
    out
}

/// How an event's output in replay compares with the game's.
enum Settled {
    Identical,
    /// The same records, `stride` bytes each, in another order.
    Reordered {
        stride: usize,
    },
    /// Records of `stride` bytes that each name a slice of another output of the event, `list`, the same records
    /// there as the game's but at other offsets: the slices were allocated from an atomic counter (a tiled light
    /// list, say), so where each landed depends on the order the GPU ran its threads. The 4-byte word `start` of a
    /// record starts its slice and `length` says how long it is; every other word is the game's.
    Sliced {
        stride: usize,
        list: i64,
        start: usize,
        length: Length,
    },
    Differs(String),
}

/// How a record says the length of its slice: a count of the list's records in one of its words; two counts in its
/// 16-bit halves, the slice the one run after the other (Sekiro's tile lists: point lights, then spot lights); or the
/// word where the slice ends.
#[derive(Clone, Copy)]
enum Length {
    Count(usize),
    Halves(usize),
    End(usize),
}

impl std::fmt::Display for Settled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Settled::Sliced {
                stride,
                list,
                start,
                length,
            } => {
                write!(
                    f,
                    "{stride}-byte records naming the game's slices of #{list} at other offsets (word {start} starts \
                     each, "
                )?;
                match length {
                    Length::Count(w) => write!(f, "word {w} counts it)"),
                    Length::Halves(w) => write!(f, "word {w}'s 16-bit halves count it)"),
                    Length::End(w) => write!(f, "word {w} ends it)"),
                }
            }
            Settled::Reordered { stride } => write!(f, "the game's {stride}-byte records in another order"),
            Settled::Identical => write!(f, "identical"),
            Settled::Differs(how) => write!(f, "{how}"),
        }
    }
}

/// One output of an event, compared.
struct Compared {
    resource: i64,
    sub: Option<u32>,
    ordered: bool,
    settled: Settled,
    /// The XXH3-128 key of replay's bytes.
    replay: String,
}

/// Every comparison a verify made, its result, and the machine it replayed on: what `verify --report` writes, and
/// what `conform` compares across two replays.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Report {
    pub checks: Vec<Checked>,
    /// Why an identical replay would not prove the frame (verify's `incomplete:` lines).
    pub incomplete: Vec<String>,
    pub ok: bool,
    /// verify's last line.
    pub result: String,
    pub environment: Option<Environment>,
}

/// One comparison of replay's bytes with the game's.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Checked {
    /// What it compared, the same in every replay of the capture: `target <draw> <name> <sub> <plane>` or
    /// `output <event> #<resource> <sub>`.
    pub key: String,
    /// `target`, `output` (of a draw or dispatch), `clear` or `ordered` (an order-dependent output).
    pub kind: String,
    pub event: i64,
    /// What verify calls it: `draw 7 rt0 #208 texture2d 8x8 R8G8B8A8_UNORM` or `Dispatch 3 (event 40) #12`.
    pub what: String,
    /// The game's bytes, or for an order-dependent output the game's records.
    pub identical: bool,
    /// How it compared, in verify's words.
    pub detail: String,
    /// The XXH3-128 key of replay's bytes.
    pub replay: String,
}

/// Writes bytes `verify --out` keeps.
fn kept(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))
}

/// After event `seq` ran, compares what it wrote through UAVs with what the game's GPU wrote. Where they differ, both
/// are written to `keep` when given, and the game's bytes are put in place for order-dependent outputs always, and for
/// every output when `all`: everything after then follows the order the game's GPU took, and starts from the game's
/// state, so a difference later is that later event's own. An order-dependent output can hold the same records in
/// another order, or differ in more than order: a shader that writes what its atomics returned (an offset into a list
/// it allocates from, say) writes other values when its threads run in another order. Where those values start
/// slices of another output that holds the same records reordered, and each slice holds the game's, it is
/// [`Settled::Sliced`]. Any other output is identical or differs.
fn settle(
    c: &Capture,
    b: &mut dyn Backend,
    written: &HashMap<i64, Vec<Written>>,
    seq: i64,
    all: bool,
    keep: Option<&Path>,
) -> Result<Vec<Compared>, String> {
    let Some(written) = written.get(&seq) else {
        return Ok(Vec::new());
    };
    // Each output compared first; then an order-dependent one that differs is checked as slices of another that holds
    // the same records reordered; then each kept and put back.
    let mut each = Vec::new();
    for w in written {
        let s = shape(c, w.resource).ok_or("resource vanished")?;
        let game = c.blob(&w.key)?;
        let whole = b.read(w.resource)?;
        // A subresource's row holds its bytes only.
        let mine = match w.sub {
            None => whole.clone(),
            Some(sub) => sub_of(&s, &whole, Some(sub))
                .ok_or_else(|| format!("#{} has no subresource {sub}", w.resource))?
                .0
                .to_vec(),
        };
        // A structured buffer's records are its structures; anything else is taken in 4-byte words, the unit UAV
        // atomics work in.
        let stride = if s.kind == "buffer" && s.stride > 0 {
            s.stride as usize
        } else {
            4
        };
        let settled = if mine == game {
            Settled::Identical
        } else if mine.len() != game.len() {
            Settled::Differs(format!("{} bytes, the game's {}", mine.len(), game.len()))
        } else if w.ordered
            && s.kind == "buffer"
            && game.len() % stride == 0
            && same_records(&mine, &game, stride)
        {
            Settled::Reordered { stride }
        } else {
            let differ = mine
                .chunks(stride)
                .zip(game.chunks(stride))
                .filter(|(a, g)| a != g)
                .count();
            Settled::Differs(format!(
                "{differ} of {} {stride}-byte records differ",
                game.len().div_ceil(stride)
            ))
        };
        // Records sliced from another output are only looked for in buffers; a structured one's are its
        // structures, anything else's are found.
        let records = (s.kind == "buffer").then_some((s.stride > 0).then_some(stride));
        each.push((w, s, game, whole, mine, records, settled));
    }
    for i in 0..each.len() {
        if !(each[i].0.ordered && matches!(each[i].6, Settled::Differs(_))) {
            continue;
        }
        let found = each.iter().enumerate().find_map(|(j, list)| {
            let Settled::Reordered { stride } = list.6 else {
                return None;
            };
            let records = each[i].5?;
            (j != i)
                .then(|| sliced(&each[i].4, &each[i].2, records, &list.4, &list.2, stride))
                .flatten()
                .map(|(stride, start, length)| Settled::Sliced {
                    stride,
                    list: list.0.resource,
                    start,
                    length,
                })
        });
        if let Some(found) = found {
            each[i].6 = found;
        }
    }
    let mut out = Vec::new();
    for (w, s, game, whole, mine, _, settled) in each {
        if !matches!(settled, Settled::Identical) {
            if let Some(dir) = keep {
                let part = w.sub.map_or(String::new(), |sub| format!("-sub{sub}"));
                let file = |side: &str| dir.join(format!("event-{seq}-{}{part}.{side}.bin", w.resource));
                kept(&file("replay"), &mine)?;
                kept(&file("game"), &game)?;
            }
            if w.ordered || all {
                let contents = match w.sub {
                    None => game.clone(),
                    Some(sub) => {
                        let (_, x) = sub_of(&s, &whole, Some(sub)).expect("read above");
                        if game.len() as u64 != x.size {
                            return Err(format!(
                                "#{} subresource {sub}: the game's {} bytes, {} expected",
                                w.resource,
                                game.len(),
                                x.size
                            ));
                        }
                        let mut contents = whole.clone();
                        contents[x.offset as usize..(x.offset + x.size) as usize].copy_from_slice(&game);
                        contents
                    }
                };
                b.write(c, w.resource, &contents)?;
            }
        }
        out.push(Compared {
            resource: w.resource,
            sub: w.sub,
            ordered: w.ordered,
            settled,
            replay: locum_capture::hash::key(&mine),
        });
    }
    Ok(out)
}

/// Whether the records of `mine` and `game` name the same slices of a list (`list`, `theirs`: records of
/// `list_stride` bytes) at other offsets: some word of each starts its slice, another counts or ends it, every other
/// word is the game's, and each slice holds the game's records in the game's order. The records are `stride` bytes,
/// or for a buffer without a structure the first size from 8 to 64 bytes that fits, with any bytes past the last
/// whole record the game's. Gives the record size, the start word and the length's.
fn sliced(
    mine: &[u8],
    game: &[u8],
    stride: Option<usize>,
    list: &[u8],
    theirs: &[u8],
    list_stride: usize,
) -> Option<(usize, usize, Length)> {
    if mine.len() != game.len() || list.len() != theirs.len() {
        return None;
    }
    let tried = stride
        .map_or(8..=64, |s| s..=s)
        .filter(|s| s % 4 == 0 && *s >= 8 && *s <= 64);
    tried
        .filter(|&s| game.len() >= s && mine[game.len() / s * s..] == game[game.len() / s * s..])
        .find_map(|s| {
            sliced_as(mine, game, s, list, theirs, list_stride).map(|(start, length)| (s, start, length))
        })
}

/// [`sliced`] with records of `stride` bytes, the bytes past the last whole one left out.
fn sliced_as(
    mine: &[u8],
    game: &[u8],
    stride: usize,
    list: &[u8],
    theirs: &[u8],
    list_stride: usize,
) -> Option<(usize, Length)> {
    let words = stride / 4;
    let word = |b: &[u8], record: usize, w: usize| {
        let at = record * stride + w * 4;
        u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes")) as usize
    };
    let records = game.len() / stride;
    fn slice(b: &[u8], first: usize, n: usize, stride: usize) -> Option<&[u8]> {
        b.get(first.checked_mul(stride)?..first.checked_add(n)?.checked_mul(stride)?)
    }
    // The words that differ somewhere: the start, and for an end, the end.
    let moved: Vec<usize> = (0..words)
        .filter(|&w| (0..records).any(|r| word(mine, r, w) != word(game, r, w)))
        .collect();
    let fits = |start: usize, length: Length| {
        (0..records).all(|r| {
            let rest_same = (0..words).all(|w| {
                w == start
                    || matches!(length, Length::End(e) if e == w)
                    || word(mine, r, w) == word(game, r, w)
            });
            let (m, g) = (word(mine, r, start), word(game, r, start));
            let n = match length {
                Length::Count(k) => Some(word(game, r, k)),
                Length::Halves(k) => Some((word(game, r, k) & 0xffff) + (word(game, r, k) >> 16)),
                Length::End(e) => {
                    let (nm, ng) = (word(mine, r, e).checked_sub(m), word(game, r, e).checked_sub(g));
                    (nm == ng).then_some(ng).flatten()
                }
            };
            rest_same
                && n.is_some_and(|n| {
                    let (a, b) = (slice(list, m, n, list_stride), slice(theirs, g, n, list_stride));
                    a.is_some() && a == b
                })
        })
    };
    match moved.as_slice() {
        [start] => (0..words)
            .filter(|&k| k != *start)
            .flat_map(|k| [Length::Count(k), Length::Halves(k)])
            .find(|&length| fits(*start, length))
            .map(|length| (*start, length)),
        [a, b] => [(*a, *b), (*b, *a)]
            .into_iter()
            .find(|&(start, end)| fits(start, Length::End(end)))
            .map(|(start, end)| (start, Length::End(end))),
        _ => None,
    }
}

/// Whether `a` and `b` hold the same `stride`-byte records, as many times each, in any order.
fn same_records(a: &[u8], b: &[u8], stride: usize) -> bool {
    let mut x: Vec<&[u8]> = a.chunks_exact(stride).collect();
    let mut y: Vec<&[u8]> = b.chunks_exact(stride).collect();
    x.sort_unstable();
    y.sort_unstable();
    x == y
}

fn backend(c: &Capture, warp: bool, validation: Validation) -> Result<Box<dyn Backend>, String> {
    match c.meta("api").as_deref() {
        Some("d3d11") => Ok(Box::new(d3d11::D3d11::new(warp, validation != Validation::Off)?)),
        Some("d3d12") => Ok(Box::new(d3d12::D3d12::new(warp, validation)?)),
        Some(api) => Err(format!("no replay for {api} captures yet")),
        None => Err("the capture names no API to replay".into()),
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
    pub validation: Validation,
    pub out: PathBuf,
    pub range: Range,
}

/// The shader a rule names, by its hash or the start of it, among the capture's shaders of that stage: a hash that
/// names none of them, or more than one, is an error rather than a rule that never applies.
fn shader_named(c: &Capture, stage: usize, hash: &str) -> Result<u128, String> {
    let name = rules::STAGES[stage];
    if hash.is_empty() || !hash.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(format!("{hash:?} is not a hex hash"));
    }
    let found: Vec<String> = c.rows(
        "SELECT hash FROM shaders WHERE stage = ?1 AND hash LIKE ?2 || '%' ORDER BY hash LIMIT 3",
        (name, hash.to_ascii_lowercase()),
        |r| r.get(0),
    );
    match found.as_slice() {
        [one] => u128::from_str_radix(one, 16).map_err(|_| format!("{one:?} is not a hex hash")),
        [] => Err(format!("the capture has no {name} shader {hash}")),
        [a, b, ..] => Err(format!(
            "{name}={hash} names more than one shader ({a}, {b}): give more of the hash"
        )),
    }
}

pub fn replay(c: &Capture, draw: i64, o: &Options) -> Result<(), String> {
    let target: i64 = c
        .row("SELECT event FROM draws WHERE n = ?1", [draw], |r| r.get(0))
        .ok_or_else(|| format!("no draw {draw}"))?;
    let mut parsed = Vec::new();
    if let Some(file) = &o.rules_file {
        let text = std::fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let dir = file.parent().unwrap_or(Path::new("."));
        let (r, errors) = rules::parse_all(&text, dir, &|stage, hash| shader_named(c, stage, hash));
        if let Some((line, e)) = errors.first() {
            return Err(format!("{} line {line}: {e}", file.display()));
        }
        parsed.extend(r);
    }
    for text in &o.rules {
        parsed.push(rules::parse(text, Path::new("."), &|stage, hash| {
            shader_named(c, stage, hash)
        })?);
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
    let (base, warnings) = execute(c, &plan, &[], o.warp, o.validation, &outs)?;
    out!("  replayed in {:.2} s", t.elapsed().as_secs_f64());
    for w in &warnings {
        out!("  warning: {w}");
    }
    let edited = if parsed.is_empty() {
        None
    } else {
        let (after, _) = execute(c, &plan, &parsed, o.warp, o.validation, &outs)?;
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
    for output in &outs {
        let s = shape(c, output.resource).ok_or("resource vanished")?;
        let suffix = if edited.is_some() { ".recorded" } else { "" };
        let path = write_output(
            &o.out,
            &format!("{}{suffix}", output.name),
            &s,
            &base[&output.resource],
            output,
            o.range,
        )?;
        out!("{}: {s} -> {}", output.name, path.display());
        if s.kind == "buffer" {
            continue;
        }
        let Some((now, x)) = plane_of(&s, &base[&output.resource], output.sub, output.z) else {
            continue;
        };
        let fmt = format::readable(s.format);
        if let Some(key) = captured.get(&output.name)
            && let Ok(game) = c.blob(key)
        {
            out!("  against the game's: {}", compare(fmt, &x, &game, now));
        }
        if let Some(after) = &edited {
            let Some((then, _)) = plane_of(&s, &after[&output.resource], output.sub, output.z) else {
                continue;
            };
            let p = write_output(
                &o.out,
                &format!("{}.rules", output.name),
                &s,
                &after[&output.resource],
                output,
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

/// Gaps that are checks the capture could not make or keep (targets or outputs not read back, a command list's earlier
/// executions, the CPU's memory), not inputs the replay lacks: shown, but they leave the proof complete.
const CHECKS: &str = "'evidence', 'targets', 'outputs', 'memory', 'd3d12-targets', 'd3d12-results'";
/// Gaps that say what is not known of the machine the frame ran on (a probe failed): neither a check nor a want of
/// what the replay needs, so verify notes them.
const NOTED: &str = "'environment'";

/// Replays the whole frame and compares every target the capture read back with the replay's at the same point. Fails
/// unless every target compared is identical and nothing kept the capture or the replay from being complete.
/// With `keep`, each output and target that differs is written there: the replay's and the game's bytes
/// (`event-<seq>-<resource>.{replay,game}.bin`, `draw-<n>-<target>.{replay,game}.bin`), and for a target its two
/// pictures and where they differ (`.replay.png`, `.game.png`, `.diff.png`). Each run is a row of the capture's
/// `verifications`: when, its flags, the machine it replayed on and its result.
pub fn verify(
    c: &Capture,
    warp: bool,
    debug: bool,
    limit: Option<i64>,
    keep: Option<&Path>,
    report_to: Option<&Path>,
) -> Result<(), String> {
    let mut report = Report::default();
    let result = verified(c, warp, debug, limit, keep, &mut report);
    report.ok = result.is_ok();
    report.result = match &result {
        Ok(line) | Err(line) => line.clone(),
    };
    if let Some(path) = report_to {
        let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let replayed = report.environment.clone();
    let verification = locum_capture::db::Verification {
        time: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        locum: env!("CARGO_PKG_VERSION").into(),
        flags: serde_json::json!({"warp": warp, "debug": debug, "limit": limit}),
        environment: replayed,
        ok: result.is_ok(),
        result: match &result {
            Ok(line) | Err(line) => line.clone(),
        },
    };
    if let Err(e) = locum_capture::db::verification(&c.path, &verification) {
        out!("note: this verification is not recorded in the capture: {e}");
    }
    out!("{}", result?);
    Ok(())
}

/// [`verify`]'s replay and comparisons: its last line, `verified: …` or why not. `report` gets each comparison, what
/// keeps the capture from being complete, and the machine the replay ran on once it has made its device.
fn verified(
    c: &Capture,
    warp: bool,
    debug: bool,
    limit: Option<i64>,
    keep: Option<&Path>,
    report: &mut Report,
) -> Result<String, String> {
    if let Some(dir) = keep {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let last: i64 = c
        .row("SELECT max(seq) FROM events", [], |r| r.get::<_, Option<i64>>(0))
        .flatten()
        .ok_or_else(|| {
            let gaps: Vec<String> = c.rows(
                "SELECT what, coalesce(detail, ''), count FROM gaps ORDER BY what, detail",
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
            if gaps.is_empty() {
                "the capture has no events".to_string()
            } else {
                format!("the capture has no events; {}", gaps.join("; "))
            }
        })?;
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
    let has_z = c
        .row(
            "SELECT count(*) FROM pragma_table_info('targets') WHERE name = 'z'",
            [],
            |r| r.get::<_, i64>(0),
        )
        .is_some_and(|n| n > 0);
    let z = if has_z { "coalesce(t.z, 0)" } else { "0" };
    let checks: Vec<(i64, i64, String, i64, String, i64, u32)> = c.rows(
        &format!(
            "SELECT t.draw, {at} AS at, t.target, t.resource, t.blob, coalesce(t.sub, 0), {z} FROM targets t \
             JOIN draws d ON d.n = t.draw WHERE t.blob IS NOT NULL AND t.resource IS NOT NULL ORDER BY at"
        ),
        [],
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
    out!(
        "replaying {} events over {} resources; {} targets to compare",
        plan.events.len(),
        plan.resources.len(),
        checks.len()
    );
    // GPU-based validation patches the shaders, and patched shaders do not round as the game's did: it runs a pass of
    // its own, for its errors, and the pass that compares runs under the debug layer alone.
    let mut validated = BTreeSet::new();
    if debug && c.meta("api").as_deref() == Some("d3d12") {
        out!(
            "validating: the frame under GPU-based validation, its errors only (its patched shaders round otherwise)"
        );
        let started = std::time::Instant::now();
        run(
            c,
            &plan,
            &[],
            warp,
            Validation::Gpu,
            |_, _| Ok(()),
            |b| {
                validated.extend(b.validation());
                Ok(())
            },
        )?;
        out!(
            "validated in {:.1} s: {} errors",
            started.elapsed().as_secs_f64(),
            validated.len()
        );
    }
    let started = std::time::Instant::now();
    let validation = if debug { Validation::Layer } else { Validation::Off };
    let (mut b, _) = made(c, &plan, warp, validation)?;
    out!(
        "planned in {:.1} s; device and resources made in {:.1} s",
        planned.as_secs_f64(),
        started.elapsed().as_secs_f64()
    );
    let environment = b.environment();
    let captured = c.meta("environment").and_then(|e| Environment::parse(&e));
    let ends = ends(captured.as_ref(), &environment);
    report.environment = Some(environment);
    out!("{ends}");
    let written = recorded(c);
    // Draws and dispatches are named by their number, as targets are.
    let numbered: HashMap<i64, (i64, String)> = c
        .rows("SELECT event, n, call FROM draws", [], |r| {
            Ok((r.get::<_, i64>(0)?, (r.get::<_, i64>(1)?, r.get::<_, String>(2)?)))
        })
        .into_iter()
        .collect();
    let calls: HashMap<i64, String> = c
        .rows("SELECT seq, call FROM events", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .into_iter()
        .collect();
    let mut next = 0;
    let (mut same, mut compared) = (0, 0);
    let (mut settled_same, mut reordered, mut differing) = (0, 0, 0);
    let (mut outputs_same, mut outputs_differing) = (0, 0);
    let (mut clears_same, mut clears_differing) = (0, 0);
    let t = std::time::Instant::now();
    for &seq in &plan.events {
        b.run(c, seq, &[]).map_err(|e| format!("event {seq}: {e}"))?;
        let who = numbered.get(&seq).map_or_else(
            || {
                format!(
                    "{} (event {seq})",
                    calls.get(&seq).map_or("unknown", String::as_str)
                )
            },
            |(n, call)| format!("{call} {n} (event {seq})"),
        );
        for out in
            settle(c, b.as_mut(), &written, seq, true, keep).map_err(|e| format!("event {seq}: {e}"))?
        {
            let resource = out.resource;
            let kind = if out.ordered {
                "ordered"
            } else if numbered.contains_key(&seq) {
                "output"
            } else {
                "clear"
            };
            report.checks.push(Checked {
                key: format!(
                    "output {seq} #{resource} {}",
                    out.sub.map_or(String::new(), |s| s.to_string())
                ),
                kind: kind.into(),
                event: seq,
                what: format!("{who} #{resource}"),
                identical: matches!(
                    out.settled,
                    Settled::Identical | Settled::Reordered { .. } | Settled::Sliced { .. }
                ),
                detail: out.settled.to_string(),
                replay: out.replay.clone(),
            });
            match (out.ordered, out.settled) {
                (true, Settled::Identical) => settled_same += 1,
                (false, Settled::Identical) => {
                    if numbered.contains_key(&seq) {
                        outputs_same += 1;
                    } else {
                        clears_same += 1;
                    }
                }
                (_, settled @ (Settled::Reordered { .. } | Settled::Sliced { .. })) => {
                    reordered += 1;
                    out!("{who} #{resource}: {settled}; going on from the game's");
                }
                (true, Settled::Differs(how)) => {
                    differing += 1;
                    out!(
                        "{who} #{resource}: order-dependent, differs from the game's output in more than order \
                         ({how}); going on from the game's"
                    );
                }
                (false, Settled::Differs(how)) => {
                    if numbered.contains_key(&seq) {
                        outputs_differing += 1;
                    } else {
                        clears_differing += 1;
                    }
                    out!(
                        "{who} #{resource}: wrote other bytes than the game's ({how}); going on from the game's"
                    );
                }
            }
        }
        // Several planes of one resource share a checkpoint; read its canonical bytes only once.
        let mut read = HashMap::new();
        while next < checks.len() && checks[next].1 == seq {
            let (draw, _, name, resource, key, sub, z) = &checks[next];
            next += 1;
            if limit.is_some_and(|l| compared >= l) {
                continue;
            }
            let s = shape(c, *resource).ok_or("resource vanished")?;
            let now = match read.entry(*resource) {
                std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
                std::collections::hash_map::Entry::Vacant(e) => e.insert(b.read(*resource)?),
            };
            let (mine, x) = plane_of(&s, now, Some(*sub as u32), *z)
                .ok_or_else(|| format!("event {seq}: target {name} has no subresource {sub}, plane {z}"))?;
            let game = c.blob(key)?;
            if game.len() != mine.len() {
                return Err(format!(
                    "event {seq}: target {name} has {} recorded bytes, expected {}",
                    game.len(),
                    mine.len()
                ));
            }
            let fmt = format::readable(s.format);
            let d = compare(fmt, &x, &game, mine);
            compared += 1;
            if d.differ == 0 {
                same += 1;
            } else if let Some(dir) = keep {
                let file = |ext: &str| dir.join(format!("draw-{draw}-{name}.{ext}"));
                kept(&file("replay.bin"), mine)?;
                kept(&file("game.bin"), &game)?;
                // Pictures only of the formats texels can decode; the bytes are written whatever the format.
                let decode = |bytes| texels::decode(fmt, x.width, x.height, bytes);
                if let (Ok(replay), Ok(theirs)) = (decode(mine), decode(&game)) {
                    let range = Range::parse("auto").expect("auto is a range");
                    png(&file("replay.png"), x.width, x.height, &replay.rgba8(range))?;
                    png(&file("game.png"), x.width, x.height, &theirs.rgba8(range))?;
                    diff_png(&file("diff.png"), fmt, &x, &game, mine, range)?;
                }
            }
            report.checks.push(Checked {
                key: format!("target {draw} {name} {sub} {z}"),
                kind: "target".into(),
                event: seq,
                what: format!("draw {draw} {name} {s}"),
                identical: d.differ == 0,
                detail: d.to_string(),
                replay: locum_capture::hash::key(mine),
            });
            out!("draw {draw:>5} {name:<6} {s} (event {seq}): {d}");
        }
    }
    out!(
        "{same} of {compared} targets identical; replayed in {:.1} s",
        t.elapsed().as_secs_f64()
    );
    if outputs_same + outputs_differing > 0 {
        out!(
            "what draws and dispatches wrote through UAVs: {outputs_same} identical to the game's, {outputs_differing} \
             different"
        );
    }
    if clears_same + clears_differing > 0 {
        out!("clear outputs: {clears_same} identical to the game's, {clears_differing} different");
    }
    if settled_same + reordered + differing > 0 {
        out!(
            "order-dependent shaders' outputs: {settled_same} identical to the game's, {reordered} the same records in \
             another order or at other offsets, {differing} different"
        );
    }
    // What keeps an identical replay from proving the frame: what the capture could not record, used resources
    // without contents, and events replay could not run as recorded. What order-dependent events wrote is the game's
    // from there on; replay's own output was compared, but no exact comparison of it can hold.
    if differing > 0 {
        out!(
            "not compared: {differing} outputs of order-dependent draws and dispatches that differ from the game's in \
             more than order (the game's were used)"
        );
    }
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
    for note in c.rows(
        &format!(
            "SELECT what, coalesce(detail, ''), count FROM gaps WHERE what IN ({NOTED}) ORDER BY what, detail"
        ),
        [],
        |r| Ok(format!("{} {}", r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    ) {
        out!("note: capture gap: {note}");
    }
    let mut incomplete: Vec<String> = c.rows(
        &format!(
            "SELECT what, coalesce(detail, ''), count FROM gaps WHERE what NOT IN ({CHECKS}, {NOTED}) ORDER BY what, \
             detail"
        ),
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
    let lacking = lacking(c);
    let missing: Vec<i64> = c
        .rows("SELECT id FROM resources WHERE used = 1 ORDER BY id", [], |r| {
            r.get(0)
        })
        .into_iter()
        .filter(|id| lacking.contains(id))
        .collect();
    if !missing.is_empty() {
        let named: Vec<String> = missing.iter().take(12).map(|id| format!("#{id}")).collect();
        incomplete.push(format!(
            "{} resources the frame used have no contents (`resource` says why): {}{}",
            missing.len(),
            named.join(" "),
            if missing.len() > named.len() { " .." } else { "" }
        ));
    }
    incomplete.extend(b.warnings().into_iter().map(|w| format!("replay: {w}")));
    for note in b.notes() {
        out!("note: replay: {note}");
    }
    validated.extend(b.validation());
    incomplete.extend(validated.into_iter().map(|w| format!("replay: {w}")));
    for i in &incomplete {
        out!("incomplete: {i}");
    }
    report.incomplete = incomplete.clone();
    if limit.is_some_and(|l| compared >= l) {
        out!("(stopped after {compared} comparisons: --limit)");
    }
    let mut failed = Vec::new();
    if same < compared {
        failed.push(format!("{} of {compared} targets differ", compared - same));
    }
    if outputs_differing > 0 {
        failed.push(format!(
            "{outputs_differing} of {} outputs of draws and dispatches differ",
            outputs_same + outputs_differing
        ));
    }
    if clears_differing > 0 {
        failed.push(format!("{clears_differing} clear outputs differ"));
    }
    if !failed.is_empty() {
        return Err(format!("{} ({ends})", failed.join("; ")));
    }
    if compared + outputs_same + clears_same + settled_same + reordered == 0 {
        return Err(format!("no target or event output was compared ({ends})"));
    }
    if !incomplete.is_empty() {
        return Err(format!(
            "every target identical, but the capture is incomplete ({} reasons above) ({ends})",
            incomplete.len()
        ));
    }
    Ok(format!(
        "verified: every target and output compared is the game's ({ends})"
    ))
}

/// Where the capture was made (`captured`, its `meta.environment`) and where it is replayed. Bit-exact comparisons
/// hold between the same adapter and driver; another can differ in the last bits (NVIDIA and WARP clear to a
/// fractional UNORM colour differently).
fn ends(captured: Option<&Environment>, replay: &Environment) -> String {
    let at = captured.map_or_else(
        || "an adapter the capture does not record".into(),
        Environment::line,
    );
    let on = replay.line();
    if at == on {
        return format!("captured and replayed on {on}");
    }
    let mut s = format!("captured on {at}; replayed on {on}");
    if let (Some(a), Some(b)) = (captured.and_then(|e| e.adapter.as_ref()), &replay.adapter)
        && (a.vendor, a.device, &a.driver) != (b.vendor, b.device, &b.driver)
    {
        s += " (another adapter or driver than the capture's)";
    }
    s
}

#[cfg(test)]
mod tests {
    use super::{Length, ends, same_records, sliced};
    use locum_capture::environment::{Adapter, Environment, Os};

    #[test]
    fn both_ends_are_named() {
        let on = |description: &str, driver: Option<&str>| Environment {
            adapter: Some(Adapter {
                description: description.into(),
                vendor: 0x10de,
                device: 0x2c05,
                driver: driver.map(Into::into),
                ..Default::default()
            }),
            os: Some(Os {
                version: "10.0.26200.9711".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let a = on("RTX", Some("32.0.16.1074"));
        assert_eq!(
            ends(Some(&a), &a),
            "captured and replayed on RTX (10de:2c05), driver 32.0.16.1074, Windows 10.0.26200.9711"
        );
        // Another driver is flagged; an end not recorded says so, and why where it is known.
        let b = on("RTX", Some("32.0.16.1100"));
        assert!(ends(Some(&a), &b).ends_with("(another adapter or driver than the capture's)"));
        assert!(
            ends(None, &a).starts_with("captured on an adapter the capture does not record; replayed on RTX")
        );
        let mut lost = on("RTX", None);
        lost.adapter
            .as_mut()
            .unwrap()
            .missing
            .insert("driver".into(), "CheckInterfaceSupport: E_FAIL".into());
        assert_eq!(
            ends(Some(&a), &lost),
            "captured on RTX (10de:2c05), driver 32.0.16.1074, Windows 10.0.26200.9711; replayed on RTX (10de:2c05), \
             driver not recorded (CheckInterfaceSupport: E_FAIL), Windows 10.0.26200.9711 (another adapter or driver \
             than the capture's)"
        );
        let none = Environment {
            missing: [("adapter".to_string(), "EnumAdapterByLuid: not found".to_string())].into(),
            ..Default::default()
        };
        assert!(
            ends(Some(&a), &none)
                .ends_with("replayed on an adapter not recorded (EnumAdapterByLuid: not found)")
        );
    }

    #[test]
    fn records_in_another_order_are_the_same_records() {
        let a = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let b = [5u8, 6, 7, 8, 1, 2, 3, 4];
        assert!(same_records(&a, &b, 4));
        // The same bytes, but not the same 4-byte records.
        let c = [1u8, 2, 7, 8, 5, 6, 3, 4];
        assert!(!same_records(&a, &c, 4));
        // As often each: a record twice is not the same as two records.
        assert!(!same_records(&[1, 1, 2, 2], &[1, 1, 1, 1], 2));
    }

    fn bytes(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|w| w.to_le_bytes()).collect()
    }

    #[test]
    fn slices_at_other_offsets_are_found() {
        // Three tiles of (start, count, other) and a tail word; the list holds their slices in another order.
        let game = bytes(&[0, 2, 9, 2, 1, 9, 3, 0, 9, 77]);
        let theirs = bytes(&[10, 11, 12, 0, 0]);
        let mine = bytes(&[1, 2, 9, 0, 1, 9, 3, 0, 9, 77]);
        let list = bytes(&[12, 10, 11, 0, 0]);
        let found = sliced(&mine, &game, None, &list, &theirs, 4);
        assert!(
            matches!(found, Some((12, 0, Length::Count(1)))),
            "{:?}",
            found.map(|f| (f.0, f.1))
        );
        // A slice whose records differ is not the game's.
        let other = bytes(&[12, 11, 10, 0, 0]);
        assert!(sliced(&mine, &game, None, &other, &theirs, 4).is_none());
        // Nor is a record whose other words differ, nor a different tail.
        let mut changed = mine.clone();
        changed[8] = 8;
        assert!(sliced(&changed, &game, None, &list, &theirs, 4).is_none());
        let mut tail = mine.clone();
        tail[36] = 76;
        assert!(sliced(&tail, &game, None, &list, &theirs, 4).is_none());
        // Starts and ends: (start, end) records.
        let game = bytes(&[0, 2, 2, 3]);
        let mine = bytes(&[1, 3, 0, 1]);
        let found = sliced(&mine, &game, Some(8), &list, &theirs, 4);
        assert!(matches!(found, Some((8, 0, Length::End(1)))));
        // Two counts in one word's halves, as Sekiro's tile lists have them: (first entry, points | spots << 16).
        let game = bytes(&[0, 1 | 1 << 16, 2, 1]);
        let mine = bytes(&[1, 1 | 1 << 16, 0, 1]);
        let found = sliced(&mine, &game, Some(8), &list, &theirs, 4);
        assert!(matches!(found, Some((8, 0, Length::Halves(1)))));
    }
}
