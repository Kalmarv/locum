//! The rules that change what is drawn, shared by locum in the game (`overrides.txt`) and by replay. One rule per
//! line, `#` starts a comment:
//!
//! - `skip vs=<hash>`: draws with that shader bound are not drawn (`skip cs=<hash>`: dispatches are not run);
//! - `solid vs=<hash> R G B A`: draws with that shader bound get a pixel shader writing that colour to every target;
//! - `shader ps=<hash> FILE`: that shader is replaced by the bytecode in FILE (relative to the rules' folder);
//! - `constant ps b3 c0 X Y Z W`: the draw sees constant 0 of its ps b3 buffer (counted from the first constant
//!   bound) as X Y Z W; a value is a float or `0x` and 8 hex digits (raw bits).
//!
//! Replay also supports state/API mutation rules (resource ids accept `#N`, `rN`, or `N`; optional `before|after|at E`
//! targets an event sequence number `E`):
//!
//! - `tile-map <res> <tile> <count> <heap> <heap_tile> [before|after|at E]`
//! - `tile-unmap <res> <tile> <count> [before|after|at E]`
//! - `skip-tile-remap [<res>] [at E]`
//! - `skip-clear [<res>] [at E]`
//! - `clear-color <res> R G B A [before|after|at E]`
//! - `clear-depth <res> D [S] [before|after|at E]`
//! - `barrier <res> <before_state> <after_state> [before|after|at E]`
//! - `uav-barrier [<res>] [before|after|at E]`
//! - `skip-aliasing [<res>] [at E]`
//! - `sync-flush [before|after|at E]`
//! - `sync-wait [<queue> <fence> <value>] [before|after|at E]`
//! - `skip-wait [at E]`
//! - `copy-buffer <dst> <dst_off> <src> <src_off> <bytes> [before|after|at E]`
//! - `copy-resource <dst> <src> [before|after|at E]`
//!
//! Any shader rule can end with conditions, `if vs=<hash> ps=<hash>`, all of which must hold; `vs=<hash>` after `skip`,
//! `solid` and `shader` is such a condition too. Any stage can be named: vs hs ds gs ps cs. Stages are numbered in
//! that order (as in the capture's `draws` table). In the game a hash is the whole hash; replay also takes the start of
//! one, if it names exactly one shader of that stage in the capture.

use std::path::{Path, PathBuf};

/// The stages by number.
pub const STAGES: [&str; 6] = ["vs", "hs", "ds", "gs", "ps", "cs"];
pub const PIXEL: usize = 4;
pub const COMPUTE: usize = 5;
/// Constant-buffer slots a stage has, and constants (16 bytes) a buffer can hold.
pub const CB_SLOTS: u32 = 14;
pub const CONSTANTS: u32 = 4096;

/// When a replay state/API mutation triggers relative to a planned event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Timing {
    /// Before the target event (or once before the first planned event / on every matching event when `None`).
    Before(Option<i64>),
    /// Immediately after the target event.
    After(i64),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Skip,
    /// A pixel shader writing one colour.
    Solid([f32; 4]),
    /// A shader of the rule's stage, from bytecode.
    Shader(Vec<u8>),
    Constant {
        slot: u32,
        index: u32,
        bits: [u32; 4],
    },
    /// Maps `count` contiguous buffer tiles of reserved `resource` starting at `tile` to `heap` starting at `heap_tile`.
    TileMap {
        resource: i64,
        tile: u32,
        count: u32,
        heap: i64,
        heap_tile: u32,
        when: Timing,
    },
    /// Unmaps (maps to NULL) `count` tiles of reserved `resource` starting at `tile`.
    TileUnmap {
        resource: i64,
        tile: u32,
        count: u32,
        when: Timing,
    },
    /// Skips recorded `UpdateTileMappings` / `CopyTileMappings` for `resource` (or all if `None`).
    SkipTileRemap {
        resource: Option<i64>,
        event: Option<i64>,
    },
    /// Skips recorded clear events targeting `resource` (or all if `None`).
    SkipClear {
        resource: Option<i64>,
        event: Option<i64>,
    },
    /// Overrides a recorded RTV clear's color, or injects an RTV clear before/after an event.
    ClearColor {
        resource: i64,
        color: [f32; 4],
        when: Timing,
    },
    /// Overrides a recorded DSV clear's depth/stencil, or injects a DSV clear before/after an event.
    ClearDepth {
        resource: i64,
        depth: f32,
        stencil: Option<u8>,
        when: Timing,
    },
    /// Injects an explicit D3D12 transition barrier on `resource`.
    Barrier {
        resource: i64,
        before: i32,
        after: i32,
        when: Timing,
    },
    /// Injects a D3D12 UAV barrier on `resource` (or global if `None`).
    UavBarrier {
        resource: Option<i64>,
        when: Timing,
    },
    /// Skips recorded aliasing barriers naming `resource` (or all if `None`).
    SkipAliasing {
        resource: Option<i64>,
        event: Option<i64>,
    },
    /// Forces a command-list flush, GPU fence wait, and global UAV barrier.
    SyncFlush {
        when: Timing,
    },
    /// Injects an explicit queue fence signal+wait synchronization point (optionally tagged with capture queue/fence/value).
    SyncWait {
        queue: Option<i64>,
        fence: Option<i64>,
        value: Option<u64>,
        when: Timing,
    },
    /// Skips recorded submission fence wait synchronization checks at `event` (or all if `None`).
    SkipWait {
        event: Option<i64>,
    },
    /// Injects a `CopyBufferRegion` between two buffers.
    CopyBuffer {
        dst: i64,
        dst_offset: u64,
        src: i64,
        src_offset: u64,
        bytes: u64,
        when: Timing,
    },
    /// Injects a `CopyResource` between two resources.
    CopyResource {
        dst: i64,
        src: i64,
        when: Timing,
    },
    /// Skips replaying events whose sequence number falls in `start..=end`.
    SkipEvent {
        start: i64,
        end: i64,
    },
    /// Backs an unmapped/0-tile reserved resource (or all unmapped reserved resources when `resource` is `None`)
    /// with freshly allocated heap tiles.
    MapUnmapped {
        resource: Option<i64>,
        tile: Option<u32>,
        count: Option<u32>,
        when: Timing,
    },
    /// Maps reserved buffer `dst` tiles onto `src`'s backing heap range (whether `src` is placed or reserved)
    /// so writer/reader pairs share physical GPU pages.
    AliasTiles {
        dst: i64,
        src: i64,
        dst_tile: u32,
        src_tile: u32,
        count: Option<u32>,
        when: Timing,
    },
    /// Overrides a recorded clear (or injects an RTV/DSV clear when `resource` is `Some`) using exact raw u32 bits
    /// (preserving NaN / integer bit payloads).
    ClearBits {
        resource: Option<i64>,
        bits: [u32; 4],
        count: u8,
        when: Timing,
    },
    /// Seeds a resource (or specific subresource / depth-stencil plane) from another resource or a raw file.
    Seed {
        resource: i64,
        sub: Option<u32>,
        plane: Option<SeedPlane>,
        source: SeedSource,
        when: Timing,
    },
    /// Overrides a shader-visible descriptor slot (`heap`, `slot`) with a null, SRV, UAV, or CBV view.
    DescriptorOverride {
        heap: i64,
        slot: u32,
        kind: DescKind,
        resource: i64,
        offset: u64,
        size: u64,
        when: Timing,
    },
    /// Overrides a root CBV/SRV/UAV descriptor GPU address at root parameter `parameter`.
    RootView {
        parameter: u32,
        kind: RootKind,
        resource: i64,
        offset: u64,
        when: Timing,
    },
    /// Overrides a single 32-bit root constant word at `(parameter, index)`.
    RootConst {
        parameter: u32,
        index: u32,
        bits: u32,
        when: Timing,
    },
}

/// Optional plane selector for `seed <res>.depth|.stencil`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeedPlane {
    Depth,
    Stencil,
}

/// Data source for `seed <res> from <src_res|file>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SeedSource {
    Resource(i64),
    File(PathBuf),
}

/// View kind for `descriptor <heap> <slot> null|srv|uav|cbv ...`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DescKind {
    Null,
    Srv,
    Uav,
    Cbv,
}

/// Root descriptor kind for `root <param> cbv|srv|uav ...`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootKind {
    Cbv,
    Srv,
    Uav,
}

#[derive(Clone, Debug)]
pub struct Rule {
    pub text: String,
    /// The stage the rule changes (pixel for `solid`).
    pub stage: usize,
    /// Shaders that must be bound, by stage.
    pub when: Vec<(usize, u128)>,
    pub action: Action,
}

pub fn stage_named(name: &str) -> Option<usize> {
    STAGES.iter().position(|s| *s == name)
}

/// How the hash in `stage=hash` names a shader, given the stage's number and the text after `=`.
pub type Resolve<'a> = &'a dyn Fn(usize, &str) -> Result<u128, String>;

/// How `draw <N>` in a timing clause or `skip-draw <N>` resolves a draw number to its event sequence number.
pub type ResolveDraw<'a> = &'a dyn Fn(i64) -> Result<i64, String>;

/// Default draw resolver when no capture is attached (rejects `draw <N>`).
pub fn no_draws(draw: i64) -> Result<i64, String> {
    Err(format!("draw {draw} requires a capture to resolve to an event"))
}

/// The text read as the hash, in hex: for the game, where a rule can name a shader made after it is read.
pub fn hex(_stage: usize, hash: &str) -> Result<u128, String> {
    u128::from_str_radix(hash, 16).map_err(|_| format!("{hash:?} is not a hex hash"))
}

/// `vs=0123456789abcdef`.
fn condition(word: &str, resolve: Resolve) -> Result<(usize, u128), String> {
    let (stage, hash) = word
        .split_once('=')
        .ok_or_else(|| format!("{word:?} is not stage=hash"))?;
    let stage = stage_named(stage).ok_or_else(|| format!("unknown stage {stage:?}"))?;
    Ok((stage, resolve(stage, hash)?))
}

fn value_bits(word: &str) -> Result<u32, String> {
    if let Some(hex) = word.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).map_err(|_| format!("{word:?} is not 0x and hex digits"));
    }
    word.parse::<f32>()
        .map(f32::to_bits)
        .map_err(|_| format!("{word:?} is not a number"))
}

fn numbered(word: Option<&str>, prefix: char, what: &str) -> Result<u32, String> {
    word.and_then(|w| w.strip_prefix(prefix))
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("expected {what} ({prefix}N)"))
}

fn parse_id(word: Option<&str>, what: &str) -> Result<i64, String> {
    let w = word.ok_or_else(|| format!("expected {what}"))?;
    let digits = w
        .strip_prefix('#')
        .or_else(|| w.strip_prefix('r'))
        .or_else(|| w.strip_prefix('R'))
        .unwrap_or(w);
    digits
        .parse::<i64>()
        .ok()
        .filter(|&n| n > 0)
        .ok_or_else(|| format!("{w:?} is not a valid {what}"))
}

fn parse_u32(word: Option<&str>, what: &str) -> Result<u32, String> {
    let w = word.ok_or_else(|| format!("expected {what}"))?;
    if let Some(hex) = w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).map_err(|_| format!("{w:?} is not a valid {what}"))
    } else {
        w.parse::<u32>()
            .map_err(|_| format!("{w:?} is not a valid {what}"))
    }
}

fn parse_u64(word: Option<&str>, what: &str) -> Result<u64, String> {
    let w = word.ok_or_else(|| format!("expected {what}"))?;
    if let Some(hex) = w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|_| format!("{w:?} is not a valid {what}"))
    } else {
        w.parse::<u64>()
            .map_err(|_| format!("{w:?} is not a valid {what}"))
    }
}

fn parse_event(word: Option<&str>) -> Result<i64, String> {
    let w = word.ok_or("expected event sequence number")?;
    w.parse::<i64>()
        .ok()
        .filter(|&n| n >= 0)
        .ok_or_else(|| format!("{w:?} is not an event sequence number"))
}

fn parse_event_or_draw<'a>(
    words: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    resolve_draw: ResolveDraw,
) -> Result<i64, String> {
    if words.peek().copied() == Some("draw") {
        words.next();
        let w = words.next().ok_or("expected draw number after `draw`")?;
        let n = w
            .parse::<i64>()
            .ok()
            .filter(|&n| n >= 0)
            .ok_or_else(|| format!("{w:?} is not a draw number"))?;
        return resolve_draw(n);
    }
    parse_event(words.next())
}

fn parse_timing<'a>(
    words: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    resolve_draw: ResolveDraw,
) -> Result<Timing, String> {
    match words.peek().copied() {
        None => Ok(Timing::Before(None)),
        Some("at" | "before") => {
            words.next();
            Ok(Timing::Before(Some(parse_event_or_draw(words, resolve_draw)?)))
        }
        Some("after") => {
            words.next();
            Ok(Timing::After(parse_event_or_draw(words, resolve_draw)?))
        }
        Some(other) => Err(format!("expected before|after|at <event|draw N>, got {other:?}")),
    }
}

fn parse_optional_at<'a>(
    words: &mut std::iter::Peekable<impl Iterator<Item = &'a str>>,
    resolve_draw: ResolveDraw,
) -> Result<Option<i64>, String> {
    match words.peek().copied() {
        None => Ok(None),
        Some("at" | "before") => {
            words.next();
            Ok(Some(parse_event_or_draw(words, resolve_draw)?))
        }
        Some(other) => Err(format!("expected at <event|draw N>, got {other:?}")),
    }
}

fn parse_seed_target(word: Option<&str>) -> Result<(i64, Option<u32>, Option<SeedPlane>), String> {
    let w = word.ok_or("expected resource id (optionally .depth, .stencil, or .subN)")?;
    if let Some((base, suffix)) = w.split_once('.') {
        let id = parse_id(Some(base), "resource id")?;
        let lower = suffix.to_ascii_lowercase();
        if lower == "depth" || lower == "plane0" {
            return Ok((id, None, Some(SeedPlane::Depth)));
        }
        if lower == "stencil" || lower == "plane1" {
            return Ok((id, None, Some(SeedPlane::Stencil)));
        }
        if let Some(sub_str) = lower.strip_prefix("sub") {
            let sub = sub_str
                .parse::<u32>()
                .map_err(|_| format!("{suffix:?} is not a valid subresource suffix"))?;
            return Ok((id, Some(sub), None));
        }
        return Err(format!("unknown seed target suffix {suffix:?}"));
    }
    Ok((parse_id(Some(w), "resource id")?, None, None))
}

/// Parses a D3D12 resource state bitmask by name (pipe-separated allowed), `0x` hex, or decimal integer.
pub fn parse_state(word: Option<&str>) -> Result<i32, String> {
    let w = word.ok_or("expected a D3D12 resource state")?;
    if let Some(hex) = w.strip_prefix("0x").or_else(|| w.strip_prefix("0X")) {
        return i32::from_str_radix(hex, 16).map_err(|_| format!("{w:?} is not a valid resource state"));
    }
    if let Ok(n) = w.parse::<i32>() {
        return Ok(n);
    }
    let mut mask = 0i32;
    for part in w.split('|') {
        let bit = match part.to_ascii_lowercase().as_str() {
            "common" | "present" => 0x0,
            "vb" | "cb" | "vertex_and_constant_buffer" => 0x1,
            "ib" | "index_buffer" => 0x2,
            "rtv" | "render_target" => 0x4,
            "uav" | "unordered_access" => 0x8,
            "dsv" | "depth_write" => 0x10,
            "depth_read" => 0x20,
            "non_ps_srv" | "non_pixel_shader_resource" => 0x40,
            "ps_srv" | "pixel_shader_resource" => 0x80,
            "srv" | "shader_read" | "shader_resource" => 0x40 | 0x80,
            "stream_out" => 0x100,
            "indirect" | "indirect_argument" => 0x200,
            "copy_dst" | "copy_dest" => 0x400,
            "copy_src" | "copy_source" => 0x800,
            "resolve_dst" | "resolve_dest" => 0x1000,
            "resolve_src" | "resolve_source" => 0x2000,
            "generic_read" => 0x1 | 0x2 | 0x40 | 0x80 | 0x200 | 0x800,
            _ => return Err(format!("unknown D3D12 resource state {part:?}")),
        };
        mask |= bit;
    }
    Ok(mask)
}

/// Strips a `#` comment from a rules-file line while keeping `#<digits>` resource ids intact.
fn strip_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'#' {
            let followed_by_digit = bytes.get(i + 1).is_some_and(u8::is_ascii_digit);
            if i == 0 || !followed_by_digit {
                return line[..i].trim();
            }
        }
    }
    line.trim()
}

/// One rule; `dir` is where a `shader` or `seed` rule's file is looked for, and `resolve` names the shaders.
pub fn parse(line: &str, dir: &Path, resolve: Resolve) -> Result<Rule, String> {
    parse_with(line, dir, resolve, &no_draws)
}

/// One rule, also resolving `draw <N>` timing/skip references via `resolve_draw`.
pub fn parse_with(
    line: &str,
    dir: &Path,
    resolve: Resolve,
    resolve_draw: ResolveDraw,
) -> Result<Rule, String> {
    let (body, conditions) = match line.split_once(" if ") {
        Some((b, c)) => (
            b,
            c.split_whitespace()
                .map(|w| condition(w, resolve))
                .collect::<Result<Vec<_>, _>>()?,
        ),
        None => (line, Vec::new()),
    };
    let mut words = body.split_whitespace().peekable();
    let mut verb = words.next().ok_or("empty rule")?;
    // Allow optional `insert` prefix (`insert clear-color ...`, `insert copy-buffer ...`, `insert clear ...`).
    if verb == "insert" {
        verb = words.next().ok_or("expected rule verb after `insert`")?;
    }
    let mut when = conditions;
    let (stage, action) = match verb {
        "skip" | "solid" | "shader" => {
            let first = condition(words.next().ok_or("expected stage=hash")?, resolve)?;
            when.insert(0, first);
            match verb {
                "skip" => (first.0, Action::Skip),
                "solid" => {
                    let mut c = [0f32; 4];
                    for v in &mut c {
                        let w = words.next().ok_or("solid needs R G B A")?;
                        *v = w.parse().map_err(|_| format!("{w:?} is not a number"))?;
                    }
                    (PIXEL, Action::Solid(c))
                }
                _ => {
                    let file = words.next().ok_or("shader needs a file")?;
                    let code = std::fs::read(dir.join(file)).map_err(|e| format!("{file}: {e}"))?;
                    (first.0, Action::Shader(code))
                }
            }
        }
        "constant" => {
            let stage = words
                .next()
                .and_then(stage_named)
                .ok_or("constant needs a stage")?;
            let slot = numbered(words.next(), 'b', "a constant buffer slot")?;
            let index = numbered(words.next(), 'c', "a constant")?;
            let mut bits = [0u32; 4];
            for b in &mut bits {
                *b = value_bits(words.next().ok_or("constant needs X Y Z W")?)?;
            }
            if slot >= CB_SLOTS || index >= CONSTANTS {
                return Err("slot or constant out of range".into());
            }
            (stage, Action::Constant { slot, index, bits })
        }
        "tile-map" => {
            let resource = parse_id(words.next(), "resource id")?;
            let tile = parse_u32(words.next(), "tile index")?;
            let count = parse_u32(words.next(), "tile count")?;
            if count == 0 {
                return Err("tile-map count must be > 0".into());
            }
            let heap = parse_id(words.next(), "heap id")?;
            let heap_tile = parse_u32(words.next(), "heap tile offset")?;
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::TileMap {
                    resource,
                    tile,
                    count,
                    heap,
                    heap_tile,
                    when,
                },
            )
        }
        "tile-unmap" => {
            let resource = parse_id(words.next(), "resource id")?;
            let tile = parse_u32(words.next(), "tile index")?;
            let count = parse_u32(words.next(), "tile count")?;
            if count == 0 {
                return Err("tile-unmap count must be > 0".into());
            }
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::TileUnmap {
                    resource,
                    tile,
                    count,
                    when,
                },
            )
        }
        "map" => {
            let target = words.next().ok_or("map needs <res> or all-unmapped")?;
            let resource = if target == "all-unmapped" {
                None
            } else {
                Some(parse_id(Some(target), "resource id")?)
            };
            let (tile, count) = match words.peek().copied() {
                None | Some("at" | "before" | "after") => (None, None),
                Some(_) => {
                    let t = parse_u32(words.next(), "tile index")?;
                    let c = parse_u32(words.next(), "tile count")?;
                    if c == 0 {
                        return Err("map tile count must be > 0".into());
                    }
                    (Some(t), Some(c))
                }
            };
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::MapUnmapped {
                    resource,
                    tile,
                    count,
                    when,
                },
            )
        }
        "alias" => {
            let dst = parse_id(words.next(), "destination resource id")?;
            let src = parse_id(words.next(), "source resource id")?;
            let (dst_tile, src_tile, count) = match words.peek().copied() {
                None | Some("at" | "before" | "after") => (0, 0, None),
                Some(_) => {
                    let dt = parse_u32(words.next(), "destination tile")?;
                    let st = parse_u32(words.next(), "source tile")?;
                    let c = parse_u32(words.next(), "tile count")?;
                    if c == 0 {
                        return Err("alias tile count must be > 0".into());
                    }
                    (dt, st, Some(c))
                }
            };
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::AliasTiles {
                    dst,
                    src,
                    dst_tile,
                    src_tile,
                    count,
                    when,
                },
            )
        }
        "skip-tile-remap" => {
            let resource = match words.peek().copied() {
                None | Some("at" | "before") => None,
                Some(_) => Some(parse_id(words.next(), "resource id")?),
            };
            let event = parse_optional_at(&mut words, resolve_draw)?;
            (0, Action::SkipTileRemap { resource, event })
        }
        "skip-clear" => {
            let resource = match words.peek().copied() {
                None | Some("at" | "before") => None,
                Some(_) => Some(parse_id(words.next(), "resource id")?),
            };
            let event = parse_optional_at(&mut words, resolve_draw)?;
            (0, Action::SkipClear { resource, event })
        }
        "skip-event" => {
            let span = words.next().ok_or("skip-event needs <seq> or <start>..<end>")?;
            let (start, end) = if let Some((a, b)) = span.split_once("..") {
                let s = parse_event(Some(a))?;
                let e = parse_event(Some(b))?;
                if e < s {
                    return Err(format!("invalid event range {span:?}"));
                }
                (s, e)
            } else {
                let s = parse_event(Some(span))?;
                (s, s)
            };
            (0, Action::SkipEvent { start, end })
        }
        "skip-draw" => {
            let w = words.next().ok_or("skip-draw needs a draw number")?;
            let n = w
                .parse::<i64>()
                .ok()
                .filter(|&n| n >= 0)
                .ok_or_else(|| format!("{w:?} is not a draw number"))?;
            let seq = resolve_draw(n)?;
            (0, Action::SkipEvent { start: seq, end: seq })
        }
        "clear-color" => {
            let resource = parse_id(words.next(), "resource id")?;
            let mut color = [0f32; 4];
            for v in &mut color {
                let w = words.next().ok_or("clear-color needs R G B A")?;
                *v = w.parse().map_err(|_| format!("{w:?} is not a number"))?;
            }
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::ClearColor {
                    resource,
                    color,
                    when,
                },
            )
        }
        "clear-depth" => {
            let resource = parse_id(words.next(), "resource id")?;
            let w = words.next().ok_or("clear-depth needs D [S]")?;
            let depth: f32 = w.parse().map_err(|_| format!("{w:?} is not a depth float"))?;
            let stencil = match words.peek().copied() {
                None | Some("at" | "before" | "after") => None,
                Some(sw) => {
                    words.next();
                    Some(
                        sw.parse::<u8>()
                            .map_err(|_| format!("{sw:?} is not a stencil byte (0..255)"))?,
                    )
                }
            };
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::ClearDepth {
                    resource,
                    depth,
                    stencil,
                    when,
                },
            )
        }
        "clear-bits" => {
            let resource = Some(parse_id(words.next(), "resource id")?);
            let mut bits = [0u32; 4];
            let mut count = 0u8;
            while count < 4 {
                match words.peek().copied() {
                    None | Some("at" | "before" | "after") => break,
                    Some(w) => {
                        words.next();
                        bits[count as usize] = value_bits(w)?;
                        count += 1;
                    }
                }
            }
            if count == 0 {
                return Err("clear-bits needs 1 to 4 raw 32-bit values (0xXXXXXXXX or float)".into());
            }
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::ClearBits {
                    resource,
                    bits,
                    count,
                    when,
                },
            )
        }
        "clear" => {
            // `clear <seq> bits 0x... [0x... ...]` OR `clear <res> R G B A [before|after|at ...]`
            let first = words
                .next()
                .ok_or("clear needs <seq> bits ... or <res> R G B A ...")?;
            if words.peek().copied() == Some("bits") {
                words.next();
                let seq = parse_event(Some(first))?;
                let mut bits = [0u32; 4];
                let mut count = 0u8;
                while count < 4 {
                    match words.peek().copied() {
                        None => break,
                        Some(w) => {
                            words.next();
                            bits[count as usize] = value_bits(w)?;
                            count += 1;
                        }
                    }
                }
                if count == 0 {
                    return Err("clear <seq> bits needs 1 to 4 32-bit values".into());
                }
                (
                    0,
                    Action::ClearBits {
                        resource: None,
                        bits,
                        count,
                        when: Timing::Before(Some(seq)),
                    },
                )
            } else {
                let resource = parse_id(Some(first), "resource id")?;
                let mut color = [0f32; 4];
                for v in &mut color {
                    let w = words.next().ok_or("clear <res> needs R G B A")?;
                    *v = w.parse().map_err(|_| format!("{w:?} is not a number"))?;
                }
                let when = parse_timing(&mut words, resolve_draw)?;
                (
                    0,
                    Action::ClearColor {
                        resource,
                        color,
                        when,
                    },
                )
            }
        }
        "seed" => {
            let (resource, sub, plane) = parse_seed_target(words.next())?;
            if words.next() != Some("from") {
                return Err("seed syntax: seed <res>[.depth|.stencil|.subN] from <src_res|file> [before|after|at ...]".into());
            }
            let src_word = words
                .next()
                .ok_or("seed needs a source resource id or file path")?;
            let source = if src_word.starts_with('#')
                || ((src_word.starts_with('r') || src_word.starts_with('R'))
                    && src_word[1..].chars().all(|c| c.is_ascii_digit())
                    && src_word.len() > 1)
                || src_word.chars().all(|c| c.is_ascii_digit())
            {
                SeedSource::Resource(parse_id(Some(src_word), "source resource id")?)
            } else {
                let p = PathBuf::from(src_word);
                SeedSource::File(if p.is_absolute() { p } else { dir.join(p) })
            };
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::Seed {
                    resource,
                    sub,
                    plane,
                    source,
                    when,
                },
            )
        }
        "descriptor" => {
            let heap = parse_id(words.next(), "descriptor heap id")?;
            let slot = parse_u32(words.next(), "descriptor slot")?;
            let kind = match words.next().ok_or("descriptor needs null|srv|uav|cbv")? {
                "null" => DescKind::Null,
                "srv" => DescKind::Srv,
                "uav" => DescKind::Uav,
                "cbv" => DescKind::Cbv,
                other => {
                    return Err(format!(
                        "unknown descriptor kind {other:?} (expected null|srv|uav|cbv)"
                    ));
                }
            };
            let (resource, offset, size) = if kind == DescKind::Null {
                (0, 0, 0)
            } else {
                let res = parse_id(words.next(), "resource id")?;
                let (off, sz) = match words.peek().copied() {
                    None | Some("at" | "before" | "after") => (0, 0),
                    Some(_) => {
                        let o = parse_u64(words.next(), "byte offset")?;
                        let s = match words.peek().copied() {
                            None | Some("at" | "before" | "after") => 0,
                            Some(_) => parse_u64(words.next(), "byte size")?,
                        };
                        (o, s)
                    }
                };
                (res, off, sz)
            };
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::DescriptorOverride {
                    heap,
                    slot,
                    kind,
                    resource,
                    offset,
                    size,
                    when,
                },
            )
        }
        "root" => {
            let parameter = parse_u32(words.next(), "root parameter index")?;
            match words.next().ok_or("root needs cbv|srv|uav|const")? {
                "const" => {
                    let index = parse_u32(words.next(), "32-bit constant index")?;
                    let bits = value_bits(words.next().ok_or("root const needs a value")?)?;
                    let when = parse_timing(&mut words, resolve_draw)?;
                    (
                        0,
                        Action::RootConst {
                            parameter,
                            index,
                            bits,
                            when,
                        },
                    )
                }
                kind_s @ ("cbv" | "srv" | "uav") => {
                    let kind = match kind_s {
                        "cbv" => RootKind::Cbv,
                        "srv" => RootKind::Srv,
                        _ => RootKind::Uav,
                    };
                    let resource = parse_id(words.next(), "resource id")?;
                    let offset = match words.peek().copied() {
                        None | Some("at" | "before" | "after") => 0,
                        Some(_) => parse_u64(words.next(), "byte offset")?,
                    };
                    let when = parse_timing(&mut words, resolve_draw)?;
                    (
                        0,
                        Action::RootView {
                            parameter,
                            kind,
                            resource,
                            offset,
                            when,
                        },
                    )
                }
                other => {
                    return Err(format!(
                        "unknown root override {other:?} (expected cbv|srv|uav|const)"
                    ));
                }
            }
        }
        "barrier" => {
            let resource = parse_id(words.next(), "resource id")?;
            let before = parse_state(words.next())?;
            let after = parse_state(words.next())?;
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::Barrier {
                    resource,
                    before,
                    after,
                    when,
                },
            )
        }
        "uav-barrier" => {
            let resource = match words.peek().copied() {
                None | Some("at" | "before" | "after") => None,
                Some(_) => Some(parse_id(words.next(), "resource id")?),
            };
            let when = parse_timing(&mut words, resolve_draw)?;
            (0, Action::UavBarrier { resource, when })
        }
        "skip-aliasing" => {
            let resource = match words.peek().copied() {
                None | Some("at" | "before") => None,
                Some(_) => Some(parse_id(words.next(), "resource id")?),
            };
            let event = parse_optional_at(&mut words, resolve_draw)?;
            (0, Action::SkipAliasing { resource, event })
        }
        "sync-flush" => {
            let when = parse_timing(&mut words, resolve_draw)?;
            (0, Action::SyncFlush { when })
        }
        "sync-wait" => {
            let (queue, fence, value) = match words.peek().copied() {
                None | Some("at" | "before" | "after") => (None, None, None),
                Some(_) => (
                    Some(parse_id(words.next(), "queue id")?),
                    Some(parse_id(words.next(), "fence id")?),
                    Some(parse_u64(words.next(), "fence value")?),
                ),
            };
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::SyncWait {
                    queue,
                    fence,
                    value,
                    when,
                },
            )
        }
        "skip-wait" => {
            let event = parse_optional_at(&mut words, resolve_draw)?;
            (0, Action::SkipWait { event })
        }
        "copy-buffer" => {
            let dst = parse_id(words.next(), "destination buffer id")?;
            let dst_offset = parse_u64(words.next(), "destination byte offset")?;
            let src = parse_id(words.next(), "source buffer id")?;
            let src_offset = parse_u64(words.next(), "source byte offset")?;
            let bytes = parse_u64(words.next(), "byte length")?;
            let when = parse_timing(&mut words, resolve_draw)?;
            (
                0,
                Action::CopyBuffer {
                    dst,
                    dst_offset,
                    src,
                    src_offset,
                    bytes,
                    when,
                },
            )
        }
        "copy-resource" | "copy" => {
            let dst = parse_id(words.next(), "destination resource id")?;
            let src = parse_id(words.next(), "source resource id")?;
            let when = parse_timing(&mut words, resolve_draw)?;
            (0, Action::CopyResource { dst, src, when })
        }
        other => return Err(format!("unknown rule {other:?}")),
    };
    if let Some(extra) = words.next() {
        return Err(format!("unexpected {extra:?}"));
    }
    Ok(Rule {
        text: line.to_string(),
        stage,
        when,
        action,
    })
}

/// Every rule in `text`, and for each line that is not one, its number (from 1) and why.
pub fn parse_all(text: &str, dir: &Path, resolve: Resolve) -> (Vec<Rule>, Vec<(usize, String)>) {
    parse_all_with(text, dir, resolve, &no_draws)
}

/// Every rule in `text`, also resolving `draw <N>` references via `resolve_draw`.
pub fn parse_all_with(
    text: &str,
    dir: &Path,
    resolve: Resolve,
    resolve_draw: ResolveDraw,
) -> (Vec<Rule>, Vec<(usize, String)>) {
    let (mut rules, mut errors) = (Vec::new(), Vec::new());
    for (n, line) in text.lines().enumerate() {
        let line = strip_comment(line);
        if line.is_empty() {
            continue;
        }
        match parse_with(line, dir, resolve, resolve_draw) {
            Ok(r) => rules.push(r),
            Err(e) => errors.push((n + 1, e)),
        }
    }
    (rules, errors)
}

impl Rule {
    /// Whether this rule is a replay-only state/API mutation (rather than a shader/constant override).
    pub fn is_state_rule(&self) -> bool {
        !matches!(
            self.action,
            Action::Skip | Action::Solid(_) | Action::Shader(_) | Action::Constant { .. }
        )
    }

    /// Resource ids referenced by a replay state/API rule (so replay ensures they are created).
    pub fn referenced_resources(&self) -> Vec<i64> {
        match &self.action {
            Action::TileMap { resource, .. }
            | Action::TileUnmap { resource, .. }
            | Action::ClearColor { resource, .. }
            | Action::ClearDepth { resource, .. }
            | Action::Barrier { resource, .. }
            | Action::RootView { resource, .. } => vec![*resource],
            Action::UavBarrier {
                resource: Some(r), ..
            }
            | Action::MapUnmapped {
                resource: Some(r), ..
            }
            | Action::ClearBits {
                resource: Some(r), ..
            } => vec![*r],
            Action::DescriptorOverride { resource, .. } if *resource > 0 => vec![*resource],
            Action::CopyBuffer { dst, src, .. }
            | Action::CopyResource { dst, src, .. }
            | Action::AliasTiles { dst, src, .. } => vec![*dst, *src],
            Action::Seed { resource, source, .. } => match source {
                SeedSource::Resource(src) => vec![*resource, *src],
                SeedSource::File(_) => vec![*resource],
            },
            _ => Vec::new(),
        }
    }

    /// Whether the rule is for a dispatch (`compute`) or a draw.
    pub fn applies(&self, compute: bool) -> bool {
        !self.is_state_rule() && compute == (self.stage == COMPUTE)
    }

    /// Whether its conditions hold, given the hash of the shader bound at each stage.
    pub fn holds(&self, mut hash: impl FnMut(usize) -> Option<u128>) -> bool {
        self.when.iter().all(|&(stage, h)| hash(stage) == Some(h))
    }
}

/// HLSL (shader model 5) for a pixel shader writing `colour` to all eight targets.
pub fn solid_source(colour: &[f32; 4]) -> String {
    let [r, g, b, a] = colour;
    let targets: String = (0..8).map(|i| format!("float4 c{i} : SV_Target{i}; ")).collect();
    let fill: String = (0..8).map(|i| format!("o.c{i} = k; ")).collect();
    format!(
        "struct O {{ {targets}}}; O main() {{ float4 k = float4({r:?}, {g:?}, {b:?}, {a:?}); O o; {fill}return o; }}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rule(line: &str) -> Result<Rule, String> {
        parse(line, &PathBuf::from("."), &hex)
    }

    #[test]
    fn rules_parse() {
        let r = rule("skip vs=00000000000000ff if ps=10").unwrap();
        assert_eq!(r.action, Action::Skip);
        assert_eq!(r.when, vec![(0, 0xff), (4, 0x10)]);
        assert!(!r.is_state_rule());
        let r = rule("constant vs b11 c0 4 4 0x3f800000 -1.5").unwrap();
        assert_eq!(
            r.action,
            Action::Constant {
                slot: 11,
                index: 0,
                bits: [
                    4f32.to_bits(),
                    4f32.to_bits(),
                    1f32.to_bits(),
                    (-1.5f32).to_bits()
                ]
            }
        );
        assert_eq!(
            rule("solid vs=1 1 0 1 1").unwrap().action,
            Action::Solid([1.0, 0.0, 1.0, 1.0])
        );
        assert!(rule("constant vs b14 c0 0 0 0 0").is_err());
        assert!(rule("skip vs").is_err());
        assert!(rule("frobnicate").is_err());
    }

    #[test]
    fn conditions() {
        let r = rule("skip cs=ab if vs=1").unwrap();
        assert!(r.applies(true) && !r.applies(false));
        assert!(r.holds(|s| Some(if s == COMPUTE { 0xab } else { 1 })));
        assert!(!r.holds(|_| None));
    }

    #[test]
    fn state_and_api_rules_parse_and_preserve_comments() {
        let script = "\
# full-line comment
tile-map #9 17 2 #17 4 before 13712 # trailing comment
tile-unmap r9 17 1 after 13712
skip-tile-remap #9 at 13700
skip-clear #10130 at 13710
clear-color #10130 1 0 0.5 1 after 13712
clear-depth r10131 0.5 2 before 13712
barrier #18 uav srv|copy_src before 13712
uav-barrier after 13712
skip-aliasing #18
sync-flush before 13712
sync-wait 81 61887 1710 before 13712
skip-wait at 13712
copy-buffer #24 64 #1548 128 256 before 13712
copy-resource r24 r1548 after 13712
";
        let (rules, errors) = parse_all(script, &PathBuf::from("."), &hex);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(rules.len(), 14);
        assert!(rules.iter().all(Rule::is_state_rule));
        assert!(rules.iter().all(|r| !r.applies(false) && !r.applies(true)));
        assert_eq!(
            rules[0].action,
            Action::TileMap {
                resource: 9,
                tile: 17,
                count: 2,
                heap: 17,
                heap_tile: 4,
                when: Timing::Before(Some(13712))
            }
        );
        assert_eq!(
            rules[1].action,
            Action::TileUnmap {
                resource: 9,
                tile: 17,
                count: 1,
                when: Timing::After(13712)
            }
        );
        assert_eq!(
            rules[4].action,
            Action::ClearColor {
                resource: 10130,
                color: [1.0, 0.0, 0.5, 1.0],
                when: Timing::After(13712)
            }
        );
        assert_eq!(
            rules[5].action,
            Action::ClearDepth {
                resource: 10131,
                depth: 0.5,
                stencil: Some(2),
                when: Timing::Before(Some(13712))
            }
        );
        assert_eq!(
            rules[6].action,
            Action::Barrier {
                resource: 18,
                before: 0x8,
                after: 0x40 | 0x80 | 0x800,
                when: Timing::Before(Some(13712))
            }
        );
        assert_eq!(
            rules[12].action,
            Action::CopyBuffer {
                dst: 24,
                dst_offset: 64,
                src: 1548,
                src_offset: 128,
                bytes: 256,
                when: Timing::Before(Some(13712))
            }
        );
        assert_eq!(rules[12].referenced_resources(), vec![24, 1548]);
        assert!(rule("tile-map #9 0 0 #17 0").is_err());
        assert!(rule("barrier #18 bogus srv").is_err());
        assert!(rule("copy-buffer #1 0 #2").is_err());
    }

    #[test]
    fn extended_tile_clear_seed_descriptor_root_and_draw_rules_parse() {
        let resolve_draw = |n: i64| -> Result<i64, String> {
            match n {
                4682 => Ok(40619),
                1594 => Ok(13684),
                _ => Err(format!("no draw {n}")),
            }
        };
        let script = "\
map all-unmapped before draw 4682
map #25 0 16 before 40619
alias #25 #24 0 8 4 before draw 4682
clear 13690 bits 0x7fc01234 0x00000000 0x3f800000 0xbf800000
clear-bits #10130 0x7fc0abcd after draw 4682
insert clear #10130 1 0 0 1 before draw 4682
insert copy #10130 #10131 after draw 4682
seed #10132.depth from #10135 before draw 4682
seed #10132 from dump.bin after 40619
descriptor #100 7 uav #18 256 4096 before draw 4682
descriptor #100 8 null before 40619
root 2 uav #18 512 before draw 4682
root 3 const 1 0xdeadbeef before draw 4682
skip-event 13614..13684
skip-draw 1594
";
        let (rules, errors) = parse_all_with(script, &PathBuf::from("."), &hex, &resolve_draw);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(rules.len(), 15);
        assert_eq!(
            rules[0].action,
            Action::MapUnmapped {
                resource: None,
                tile: None,
                count: None,
                when: Timing::Before(Some(40619))
            }
        );
        assert_eq!(
            rules[2].action,
            Action::AliasTiles {
                dst: 25,
                src: 24,
                dst_tile: 0,
                src_tile: 8,
                count: Some(4),
                when: Timing::Before(Some(40619))
            }
        );
        assert_eq!(
            rules[3].action,
            Action::ClearBits {
                resource: None,
                bits: [0x7fc01234, 0, 0x3f800000, 0xbf800000],
                count: 4,
                when: Timing::Before(Some(13690))
            }
        );
        assert_eq!(
            rules[7].action,
            Action::Seed {
                resource: 10132,
                sub: None,
                plane: Some(SeedPlane::Depth),
                source: SeedSource::Resource(10135),
                when: Timing::Before(Some(40619))
            }
        );
        assert_eq!(rules[7].referenced_resources(), vec![10132, 10135]);
        assert_eq!(
            rules[9].action,
            Action::DescriptorOverride {
                heap: 100,
                slot: 7,
                kind: DescKind::Uav,
                resource: 18,
                offset: 256,
                size: 4096,
                when: Timing::Before(Some(40619))
            }
        );
        assert_eq!(
            rules[11].action,
            Action::RootView {
                parameter: 2,
                kind: RootKind::Uav,
                resource: 18,
                offset: 512,
                when: Timing::Before(Some(40619))
            }
        );
        assert_eq!(
            rules[12].action,
            Action::RootConst {
                parameter: 3,
                index: 1,
                bits: 0xdeadbeef,
                when: Timing::Before(Some(40619))
            }
        );
        assert_eq!(
            rules[13].action,
            Action::SkipEvent {
                start: 13614,
                end: 13684
            }
        );
        assert_eq!(
            rules[14].action,
            Action::SkipEvent {
                start: 13684,
                end: 13684
            }
        );
    }
}
