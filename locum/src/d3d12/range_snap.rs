//! Capture-side buffer range snapshotting configuration (`snapshot-ranges`, `snapshot-traced`, `snapshot-draw`).
//!
//! Allows capturing sparse byte ranges of large buffers (such as XCL's 4 GiB UMA-window reserved buffers) at
//! capture start and immediately before a target draw/submission, without trying to copy gigabytes of untouched
//! window memory.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use locum_capture::tiles::TILE_BYTES;
use serde_json::Value;

/// How a target draw for a draw-time range snapshot is selected.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DrawTarget {
    /// Inclusive draw number range `[lo, hi]` (1-based frame draw index).
    DrawRange(u64, u64),
    /// Lowercase hex prefix of any shader blob hash bound on the active PSO.
    Shader(String),
    /// Pipeline state object ID.
    Pso(u64),
    /// Case-insensitive substring of an active PIX/D3D12 event/marker.
    Marker(String),
}

impl DrawTarget {
    /// Parses `4306`, `4300..4306`, `4300..=4306`, `draw=...`, `shader=<hex>`, `pso=<id>`, or `marker=<text>`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err("empty draw target".into());
        }
        if let Some((key, val)) = spec.split_once('=') {
            let key = key.trim().to_ascii_lowercase();
            let val = val.trim();
            if val.is_empty() {
                return Err(format!("missing value in {spec:?}"));
            }
            match key.as_str() {
                "draw" => parse_u64_range(val)
                    .map(|(a, b)| Self::DrawRange(a, b))
                    .map_err(|e| format!("draw={val:?}: {e}")),
                "shader" => {
                    let hex = val.to_ascii_lowercase();
                    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                        return Err(format!("shader={val:?} is not hex"));
                    }
                    Ok(Self::Shader(hex))
                }
                "pso" => val
                    .trim_start_matches('#')
                    .parse::<u64>()
                    .map(Self::Pso)
                    .map_err(|_| format!("pso={val:?} is not an integer")),
                "marker" => Ok(Self::Marker(val.to_ascii_lowercase())),
                _ => Err(format!("unknown snapshot-draw target key {key:?}")),
            }
        } else {
            parse_u64_range(spec)
                .map(|(a, b)| Self::DrawRange(a, b))
                .map_err(|e| format!("snapshot-draw {spec:?}: {e}"))
        }
    }
}

/// Resolved capture-time range snapshot plan (merged from `settings.txt`, `capture` request, and traced files).
#[derive(Clone, Debug, Default)]
pub struct RangeSnapshotConfig {
    /// Merged non-overlapping half-open `[start, end)` byte intervals per resource to read at capture start
    /// (and at target draws matched by [`targets`]).
    pub ranges: BTreeMap<u64, Vec<(u64, u64)>>,
    /// Resources explicitly named by `snapshot-ranges` or marked `uncaptured` in `snapshot-traced`: when such a
    /// buffer is large (`> 64 MiB`), full-buffer `first_use` is skipped in favor of the pieced range snapshot.
    pub prefer_range_only: BTreeSet<u64>,
    /// Target draws where [`ranges`] should be snapshotted right before execution.
    pub targets: Vec<DrawTarget>,
    /// Optional per-draw range sets inferred from `snapshot-traced` when the trace names specific draws (up to
    /// [`MAX_AUTO_TRACED_DRAWS`] unique draws; beyond that, the union is attached to the first and last traced draw).
    pub per_draw: BTreeMap<u64, BTreeMap<u64, Vec<(u64, u64)>>>,
}

/// Cap on distinct auto-inferred draws from a `snapshot-traced` file (when no explicit `snapshot-draw` is set) so
/// a whole-pass trace with hundreds of draws does not inject hundreds of in-list barriers.
const MAX_AUTO_TRACED_DRAWS: usize = 16;

impl RangeSnapshotConfig {
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    /// Builds a [`RangeSnapshotConfig`] from `snapshot-ranges` specs, `snapshot-traced` file paths (resolved
    /// relative to `base_dir`), `snapshot-draw` specs, and optional `complete-pass` selectors (draw/shader/pso/marker
    /// targets inherited when no explicit `snapshot-draw` was given).
    pub fn load(
        base_dir: &Path,
        range_specs: &[String],
        traced_paths: &[String],
        draw_specs: &[String],
        complete_pass_specs: &[String],
    ) -> (Self, Vec<String>) {
        let mut cfg = Self::default();
        let mut errors = Vec::new();
        let mut raw_ranges: BTreeMap<u64, Vec<(u64, u64)>> = BTreeMap::new();
        let mut raw_per_draw: BTreeMap<u64, BTreeMap<u64, Vec<(u64, u64)>>> = BTreeMap::new();

        for spec in range_specs {
            let (parsed_ranges, parsed_targets, errs) = parse_ranges_line(spec);
            errors.extend(errs);
            for (res, start, end) in parsed_ranges {
                raw_ranges.entry(res).or_default().push((start, end));
                cfg.prefer_range_only.insert(res);
            }
            cfg.targets.extend(parsed_targets);
        }

        for raw_path in traced_paths {
            let raw_path = raw_path.trim();
            if raw_path.is_empty() {
                continue;
            }
            let path = Path::new(raw_path);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                base_dir.join(path)
            };
            match std::fs::read_to_string(&resolved) {
                Ok(text) => {
                    let parsed = parse_traced_text(&text);
                    errors.extend(parsed.errors);
                    for (res, start, end, uncaptured, draw) in parsed.entries {
                        raw_ranges.entry(res).or_default().push((start, end));
                        if uncaptured {
                            cfg.prefer_range_only.insert(res);
                        }
                        if let Some(d) = draw {
                            raw_per_draw
                                .entry(d)
                                .or_default()
                                .entry(res)
                                .or_default()
                                .push((start, end));
                        }
                    }
                    cfg.targets.extend(parsed.targets);
                }
                Err(e) => errors.push(format!("snapshot-traced {}: {e}", resolved.display())),
            }
        }

        for spec in draw_specs {
            for tok in spec.split_whitespace() {
                match DrawTarget::parse(tok) {
                    Ok(t) => cfg.targets.push(t),
                    Err(e) => errors.push(e),
                }
            }
        }

        // Merge all per-resource intervals.
        for (res, list) in raw_ranges {
            let merged = merge_intervals(list);
            if !merged.is_empty() {
                cfg.ranges.insert(res, merged);
            }
        }

        // If no explicit draw targets were given, inherit draw/shader/pso/marker targets from `complete-pass`,
        // and/or infer target draws from `snapshot-traced`.
        if cfg.targets.is_empty() && !cfg.ranges.is_empty() {
            for spec in complete_pass_specs {
                for tok in spec.split_whitespace() {
                    if let Ok(t) = DrawTarget::parse(tok) {
                        cfg.targets.push(t);
                    }
                }
            }
        }
        if cfg.targets.is_empty() && !raw_per_draw.is_empty() {
            if raw_per_draw.len() <= MAX_AUTO_TRACED_DRAWS {
                for (draw, by_res) in raw_per_draw {
                    let mut merged_res = BTreeMap::new();
                    for (res, list) in by_res {
                        let m = merge_intervals(list);
                        if !m.is_empty() {
                            merged_res.insert(res, m);
                        }
                    }
                    if !merged_res.is_empty() {
                        cfg.per_draw.insert(draw, merged_res);
                    }
                }
            } else {
                // Multi-draw slice/list trace (e.g. list 369 with hundreds of draws): snapshot the full union at
                // the first and last (target) traced draw to keep overhead tiny while capturing entry & target state.
                if let Some(&first_draw) = raw_per_draw.keys().next() {
                    cfg.targets.push(DrawTarget::DrawRange(first_draw, first_draw));
                }
                if let Some(&last_draw) = raw_per_draw.keys().next_back()
                    && !cfg.targets.contains(&DrawTarget::DrawRange(last_draw, last_draw))
                {
                    cfg.targets.push(DrawTarget::DrawRange(last_draw, last_draw));
                }
            }
        }

        (cfg, errors)
    }

    /// Total configured capture-start byte span (sum of merged intervals).
    pub fn total_bytes(&self) -> u64 {
        self.ranges
            .values()
            .flat_map(|v| v.iter())
            .map(|&(a, b)| b.saturating_sub(a))
            .sum()
    }

    /// Number of merged capture-start intervals across all resources.
    pub fn range_count(&self) -> usize {
        self.ranges.values().map(Vec::len).sum()
    }

    /// Whether any draw-time target is configured.
    pub fn has_draw_targets(&self) -> bool {
        !self.targets.is_empty() || !self.per_draw.is_empty()
    }

    /// Returns the ranges to snapshot for draw `draw_n` (1-based) if it matches either an explicit `DrawRange`
    /// target or an inferred `per_draw` entry; `None` if this draw is not targeted by draw number.
    pub fn ranges_for_draw_number(&self, draw_n: u64) -> Option<&BTreeMap<u64, Vec<(u64, u64)>>> {
        if self
            .targets
            .iter()
            .any(|t| matches!(t, DrawTarget::DrawRange(lo, hi) if draw_n >= *lo && draw_n <= *hi))
        {
            return Some(&self.ranges);
        }
        self.per_draw.get(&draw_n)
    }

    /// Whether any non-draw-number target matches the given PSO ID, PSO shader blob hashes, or active markers.
    pub fn matches_pipeline(&self, pso: u64, shader_hashes: &[String], markers: &[String]) -> bool {
        for t in &self.targets {
            match t {
                DrawTarget::Pso(id) if pso != 0 && pso == *id => return true,
                DrawTarget::Shader(prefix)
                    if shader_hashes
                        .iter()
                        .any(|h| h.to_ascii_lowercase().starts_with(prefix)) =>
                {
                    return true;
                }
                DrawTarget::Marker(needle) if markers.iter().any(|m| m.contains(needle.as_str())) => {
                    return true;
                }
                _ => {}
            }
        }
        false
    }
}

/// Parsed result of one `snapshot-ranges` line: `(ranges, draw_targets, errors)`.
pub type ParsedRangesLine = (Vec<(u64, u64, u64)>, Vec<DrawTarget>, Vec<String>);

/// Per-resource merged half-open byte intervals `[start, end)`.
pub type ResourceRanges = BTreeMap<u64, Vec<(u64, u64)>>;

/// Parses a `snapshot-ranges` line: comma- or whitespace-separated `<res>:<start>..<end>` entries (also `-`, `..=`,
/// `+len`, optional `#` on resource ID, hex or decimal) plus optional inline draw selectors (`draw=...`, `shader=...`,
/// `pso=...`, `marker=...`).
pub fn parse_ranges_line(line: &str) -> ParsedRangesLine {
    let mut ranges = Vec::new();
    let mut targets = Vec::new();
    let mut errors = Vec::new();
    for raw in line.split(|c: char| c == ',' || c.is_whitespace()) {
        let tok = raw.trim();
        if tok.is_empty() {
            continue;
        }
        if tok.contains('=') && !tok.contains(':') {
            match DrawTarget::parse(tok) {
                Ok(t) => targets.push(t),
                Err(e) => errors.push(e),
            }
            continue;
        }
        match parse_one_range(tok) {
            Ok(r) => ranges.push(r),
            Err(e) => errors.push(e),
        }
    }
    (ranges, targets, errors)
}

/// Parses a single `<res>:<start>..<end>` / `<res>:<start>-<end>` / `<res>:<start>..=<end>` / `<res>:<start>+<len>`.
pub fn parse_one_range(tok: &str) -> Result<(u64, u64, u64), String> {
    let Some((res_str, span)) = tok.split_once(':') else {
        return Err(format!("snapshot-ranges {tok:?}: expected <res>:<start>..<end>"));
    };
    let res = res_str
        .trim()
        .trim_start_matches('#')
        .parse::<u64>()
        .map_err(|_| format!("snapshot-ranges {tok:?}: invalid resource id {res_str:?}"))?;
    if res == 0 {
        return Err(format!("snapshot-ranges {tok:?}: resource id must be > 0"));
    }
    let (start, end) = parse_byte_span(span).map_err(|e| format!("snapshot-ranges {tok:?}: {e}"))?;
    if start >= end {
        return Err(format!(
            "snapshot-ranges {tok:?}: empty range {start:#x}..{end:#x}"
        ));
    }
    Ok((res, start, end))
}

fn parse_num(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|_| format!("invalid hex number {s:?}"))
    } else {
        s.parse::<u64>().map_err(|_| format!("invalid number {s:?}"))
    }
}

/// Parses a byte interval `<start>..<end>`, `<start>..=<end>`, `<start>-<end>`, or `<start>+<len>` into `[start, end)`.
pub fn parse_byte_span(span: &str) -> Result<(u64, u64), String> {
    let span = span.trim();
    if let Some((a, b)) = span.split_once("..=") {
        let start = parse_num(a)?;
        let end = parse_num(b)?.saturating_add(1);
        Ok((start, end))
    } else if let Some((a, b)) = span.split_once("..") {
        let start = parse_num(a)?;
        let end = parse_num(b)?;
        Ok((start, end))
    } else if let Some((a, b)) = span.split_once('+') {
        let start = parse_num(a)?;
        let len = parse_num(b)?;
        Ok((start, start.saturating_add(len)))
    } else if let Some((a, b)) = span.split_once('-') {
        let start = parse_num(a)?;
        let end = parse_num(b)?;
        Ok((start, end))
    } else {
        Err(format!(
            "invalid byte span {span:?}: expected start..end, start-end, or start+len"
        ))
    }
}

fn parse_u64_range(s: &str) -> Result<(u64, u64), String> {
    if let Some((a, b)) = s.split_once("..=") {
        let a = parse_num(a)?;
        let b = parse_num(b)?;
        Ok((a.min(b), a.max(b)))
    } else if let Some((a, b)) = s.split_once("..") {
        let a = parse_num(a)?;
        let b = parse_num(b)?;
        Ok((a.min(b), a.max(b)))
    } else if let Some((a, b)) = s.split_once('-') {
        let a = parse_num(a)?;
        let b = parse_num(b)?;
        Ok((a.min(b), a.max(b)))
    } else {
        let n = parse_num(s)?;
        Ok((n, n))
    }
}

pub struct ParsedTraced {
    /// `(resource, start, end, is_uncaptured, draw)`
    pub entries: Vec<(u64, u64, u64, bool, Option<u64>)>,
    pub targets: Vec<DrawTarget>,
    pub errors: Vec<String>,
}

/// Parses the contents of a `snapshot-traced <file>` file. Accepts:
/// 1. `locum-cli bindless --trace ... --raw-loads` text lines (`raw-load\tdraw N\t...\t#RES\t0xA..0xB\t...\tSTATUS`);
/// 2. `locum-cli bindless --trace ... --raw-loads --json` JSONL lines (`{"kind":"raw_load",...}`);
/// 3. Plain `<res>:<start>..<end>` range lines (and inline `draw=...` / `shader=...` / `pso=...` / `marker=...`).
pub fn parse_traced_text(text: &str) -> ParsedTraced {
    let mut out = ParsedTraced {
        entries: Vec::new(),
        targets: Vec::new(),
        errors: Vec::new(),
    };
    for (idx, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with("warning:") {
            continue;
        }
        // 1. JSONL line (from `locum-cli bindless --trace ... --raw-loads --json`).
        if line.starts_with('{') {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                out.errors
                    .push(format!("snapshot-traced line {}: invalid JSON", idx + 1));
                continue;
            };
            if let Some(kind) = v["kind"].as_str()
                && kind != "raw_load"
            {
                // Ignore bindless-slot JSON lines in mixed trace output.
                continue;
            }
            if v["status"].as_str() == Some("unmapped") {
                continue;
            }
            let Some(res) = v["resource"].as_u64().filter(|&r| r > 0) else {
                continue;
            };
            let Some(start) = v["start"].as_u64() else {
                continue;
            };
            let Some(end) = v["end"].as_u64().filter(|&e| e > start) else {
                continue;
            };
            let uncaptured = v["status"].as_str().is_none_or(|s| s.starts_with("uncaptured"));
            let draw = v["draw"].as_u64();
            out.entries.push((res, start, end, uncaptured, draw));
            continue;
        }
        // 2. Text `raw-load` line (from `locum-cli bindless --trace ... --raw-loads`).
        if line.starts_with("raw-load") {
            if let Some(entry) = parse_raw_load_text_line(line) {
                out.entries.push(entry);
            }
            continue;
        }
        // Skip non-range `draw N\tevent E\theap #H[S]...` bindless slot lines from mixed trace output.
        if line.starts_with("draw ") && !line.contains(':') {
            continue;
        }
        // 3. Plain `snapshot-ranges` line (`#27:0x1000..0x2000`, `draw=4306`, etc.).
        let (ranges, targets, errs) = parse_ranges_line(line);
        for (res, start, end) in ranges {
            out.entries.push((res, start, end, true, None));
        }
        out.targets.extend(targets);
        for e in errs {
            out.errors.push(format!("snapshot-traced line {}: {e}", idx + 1));
        }
    }
    out
}

/// Parses one text line emitted by `locum-cli bindless --trace ... --raw-loads`:
/// `raw-load\tdraw 1238\tevent 10659\theap #72[4]\t#24\t0x21d6e000..0x21d70000\ttiles 8662..8663\tcaptured-nonzero`
fn parse_raw_load_text_line(line: &str) -> Option<(u64, u64, u64, bool, Option<u64>)> {
    let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
    if cols.len() >= 6 {
        let status = cols.last().copied().unwrap_or("");
        if status == "unmapped" {
            return None;
        }
        let draw = cols
            .iter()
            .find_map(|c| c.strip_prefix("draw ").and_then(|n| n.trim().parse::<u64>().ok()));
        let res = cols
            .iter()
            .find_map(|c| c.strip_prefix('#').and_then(|n| n.parse::<u64>().ok()))
            .filter(|&r| r > 0)?;
        let (start, end) = cols.iter().find_map(|c| {
            if c.contains("..") && !c.starts_with("tiles ") {
                parse_byte_span(c).ok()
            } else {
                None
            }
        })?;
        if start >= end {
            return None;
        }
        let uncaptured = status.starts_with("uncaptured") || !status.starts_with("captured");
        return Some((res, start, end, uncaptured, draw));
    }
    // Space-separated fallback (e.g. copy-pasted text with spaces instead of tabs).
    let toks: Vec<&str> = line.split_whitespace().collect();
    if toks.last().copied() == Some("unmapped") {
        return None;
    }
    let draw = toks
        .windows(2)
        .find_map(|w| (w[0] == "draw").then(|| w[1].parse::<u64>().ok()).flatten());
    let res = toks
        .iter()
        .find_map(|t| t.strip_prefix('#').and_then(|n| n.parse::<u64>().ok()))
        .filter(|&r| r > 0)?;
    let (start, end) = toks.iter().enumerate().find_map(|(i, t)| {
        let prev_tiles = i > 0 && toks[i - 1] == "tiles";
        if !prev_tiles && t.contains("..") {
            parse_byte_span(t).ok()
        } else {
            None
        }
    })?;
    if start >= end {
        return None;
    }
    let uncaptured = toks
        .last()
        .is_none_or(|s| s.starts_with("uncaptured") || !s.starts_with("captured"));
    Some((res, start, end, uncaptured, draw))
}

/// Merges overlapping or adjacent half-open byte intervals `[start, end)`.
pub fn merge_intervals(mut ranges: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    ranges.retain(|&(a, b)| a < b);
    ranges.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::new();
    for (start, end) in ranges {
        match out.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => out.push((start, end)),
        }
    }
    out
}

/// Clamps `[start, end)` intervals to buffer size `width` and, when `mapped_tiles` is `Some` (a reserved buffer
/// whose page table is known), intersects them with the mapped 64 KiB tiles so unmapped holes inside coarse ranges
/// are never copied.
pub fn resolve_buffer_ranges(
    ranges: &[(u64, u64)],
    width: u64,
    mapped_tiles: Option<&[u32]>,
) -> Vec<(u64, u64)> {
    if width == 0 {
        return Vec::new();
    }
    let clamped: Vec<(u64, u64)> = merge_intervals(
        ranges
            .iter()
            .map(|&(a, b)| (a.min(width), b.min(width)))
            .collect(),
    );
    let Some(mapped) = mapped_tiles else {
        return clamped;
    };
    if mapped.is_empty() {
        return Vec::new();
    }
    // Build coalesced byte runs from mapped 64 KiB tiles (already sorted).
    let mut tile_runs: Vec<(u64, u64)> = Vec::new();
    for &t in mapped {
        let a = u64::from(t).saturating_mul(TILE_BYTES).min(width);
        let b = (u64::from(t) + 1).saturating_mul(TILE_BYTES).min(width);
        if a >= b {
            continue;
        }
        match tile_runs.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => tile_runs.push((a, b)),
        }
    }
    // Two-pointer intersection of sorted disjoint intervals.
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < clamped.len() && j < tile_runs.len() {
        let (a0, a1) = clamped[i];
        let (b0, b1) = tile_runs[j];
        let lo = a0.max(b0);
        let hi = a1.min(b1);
        if lo < hi {
            out.push((lo, hi));
        }
        if a1 < b1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    merge_intervals(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_snapshot_ranges_formats_and_merges() {
        let (ranges, targets, errs) = parse_ranges_line(
            "draw=4306 #27:0x12561000-0x1792e000,32:0x71188000..0x713c5000,32:0xb7f68000+0x2000",
        );
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(targets, vec![DrawTarget::DrawRange(4306, 4306)]);
        assert_eq!(
            ranges,
            vec![
                (27, 0x1256_1000, 0x1792_e000),
                (32, 0x7118_8000, 0x713c_5000),
                (32, 0xb7f6_8000, 0xb7f6_a000),
            ]
        );
        assert_eq!(
            merge_intervals(vec![(0x2000, 0x3000), (0x1000, 0x2000), (0x2800, 0x4000)]),
            vec![(0x1000, 0x4000)]
        );
    }

    #[test]
    fn parses_traced_text_and_json_outputs() {
        let text = "\
# comment
draw 4306\tevent 31200\theap #72[4]\tuav\t#27\twrite\truntime
raw-load\tdraw 4306\tevent 31200\theap #72[4]\t#27\t0x12561000..0x12562000\ttiles 4694..4695\tuncaptured
raw-load\tdraw 4306\tevent 31200\theap #72[4]\t#27\t0x12562000..0x12563000\ttiles 4694..4695\tuncaptured-range-snapshot
raw-load\tdraw 4306\tevent 31200\theap #72[7]\t#32\t0x71188000..0x71189000\ttiles 28952..28953\tunmapped
{\"kind\":\"raw_load\",\"draw\":4306,\"event\":31200,\"source\":\"heap #72[7]\",\"resource\":32,\"start\":3086385152,\"end\":3086389248,\"status\":\"uncaptured\"}
{\"kind\":\"bindless\",\"draw\":4306,\"resource\":24}
";
        let parsed = parse_traced_text(text);
        assert!(parsed.errors.is_empty(), "{:?}", parsed.errors);
        assert_eq!(parsed.entries.len(), 3);
        assert_eq!(
            parsed.entries[0],
            (27, 0x1256_1000, 0x1256_2000, true, Some(4306))
        );
        assert_eq!(
            parsed.entries[1],
            (27, 0x1256_2000, 0x1256_3000, true, Some(4306))
        );
        assert_eq!(parsed.entries[2], (32, 3086385152, 3086389248, true, Some(4306)));
    }

    #[test]
    fn resolves_coarse_range_against_mapped_tiles() {
        let t = TILE_BYTES;
        // Coarse range spanning tiles 0..10, only tiles 2 and 5 are mapped.
        let resolved = resolve_buffer_ranges(&[(0x1000, 10 * t)], 16 * t, Some(&[2, 5]));
        assert_eq!(resolved, vec![(2 * t, 3 * t), (5 * t, 6 * t)]);
        // Sub-tile range inside a mapped tile stays exact (does not expand to 64 KiB).
        let exact = resolve_buffer_ranges(&[(2 * t + 0x1000, 2 * t + 0x2000)], 16 * t, Some(&[2]));
        assert_eq!(exact, vec![(2 * t + 0x1000, 2 * t + 0x2000)]);
    }

    #[test]
    fn loads_traced_file_and_infers_auto_targets() {
        let dir = std::env::temp_dir().join(format!("locum_traced_cfg_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let trace_path = dir.join("trace.txt");
        let mut content = String::new();
        for d in 100..125u64 {
            content.push_str(&format!(
                "raw-load\tdraw {d}\tevent {}\theap #72[4]\t#27\t{:#x}..{:#x}\ttiles 0..1\tuncaptured\n",
                d * 10,
                0x1000 + (d - 100) * 0x1000,
                0x2000 + (d - 100) * 0x1000
            ));
        }
        std::fs::write(&trace_path, content).unwrap();
        let (cfg, errs) = RangeSnapshotConfig::load(&dir, &[], &["trace.txt".into()], &[], &[]);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(cfg.ranges.get(&27), Some(&vec![(0x1000, 0x1a000)]));
        assert!(cfg.prefer_range_only.contains(&27));
        // > MAX_AUTO_TRACED_DRAWS unique draws -> auto targets first and last traced draws.
        assert_eq!(
            cfg.targets,
            vec![DrawTarget::DrawRange(100, 100), DrawTarget::DrawRange(124, 124)]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
