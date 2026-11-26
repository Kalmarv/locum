//! Generic address-range label table (`--labels <ranges.tsv>`).
//!
//! Maps half-open address intervals `[start, end)` to human-readable owner names (and optional notes).
//! Nested and overlapping ranges are supported: the innermost (narrowest) range wins on every
//! sub-interval (ties broken by later line order).
//!
//! Accepted line formats (comments starting with `#` and blank lines are ignored):
//! - `start\tend\towner[\tnote]` (default; hex `0x...` or decimal)
//! - `start..end\towner[\tnote]`
//! - `[start..end] -> owner` (or `start..end -> owner`)
//! - With a header whose second column is `size`, `bytes`, or `len`: `start\tsize\towner[\tnote]`

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

/// One half-open labeled address interval `[start, end)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LabelRange {
    pub start: u64,
    pub end: u64,
    pub owner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Collection of address-range labels with pre-flattened disjoint intervals (innermost range wins).
#[derive(Clone, Debug, Default)]
pub struct LabelMap {
    /// Original declared ranges (preserved so all declared owners can be pre-seeded).
    ranges: Vec<LabelRange>,
    /// Disjoint, sorted intervals flattened from `ranges` where the narrowest (innermost)
    /// containing range wins (tie-broken by later line order).
    resolved: Vec<LabelRange>,
}

impl LabelMap {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("reading labels {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let mut ranges = Vec::new();
        let mut second_is_size = false;
        let mut saw_data = false;

        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let cols: Vec<&str> = line.split('\t').map(str::trim).collect();
            if !saw_data && is_header(&cols) {
                if let Some(c1) = cols.get(1) {
                    let lc = c1.to_ascii_lowercase();
                    if matches!(lc.as_str(), "size" | "bytes" | "len" | "length") {
                        second_is_size = true;
                    }
                }
                continue;
            }
            saw_data = true;
            let lineno = idx + 1;

            let (start, end, owner, note) = if cols.len() >= 2 && cols[0].contains("..") {
                let (s, e) = parse_range_token(cols[0], lineno)?;
                let owner = cols[1].to_string();
                let note = cols.get(2).filter(|s| !s.is_empty()).map(|s| (*s).to_string());
                (s, e, owner, note)
            } else if cols.len() >= 3 {
                let s = parse_u64(cols[0]).map_err(|e| format!("line {lineno}: start: {e}"))?;
                let second = parse_u64(cols[1]).map_err(|e| format!("line {lineno}: end/size: {e}"))?;
                let e = if second_is_size {
                    s.checked_add(second)
                        .ok_or_else(|| format!("line {lineno}: start + size overflow"))?
                } else {
                    second
                };
                let owner = cols[2].to_string();
                let note = cols.get(3).filter(|s| !s.is_empty()).map(|s| (*s).to_string());
                (s, e, owner, note)
            } else if let Some((lhs, rhs)) = line.split_once("->") {
                let (s, e) = parse_range_token(lhs, lineno)?;
                let owner = rhs.trim().to_string();
                (s, e, owner, None)
            } else {
                return Err(format!(
                    "line {lineno}: expected `start\\tend\\towner`, `start..end\\towner`, or `[start..end] -> owner`, got {raw:?}"
                ));
            };

            if owner.is_empty() {
                return Err(format!("line {lineno}: empty owner"));
            }
            if end <= start {
                return Err(format!(
                    "line {lineno}: empty/inverted range {start:#x}..{end:#x}"
                ));
            }
            ranges.push(LabelRange {
                start,
                end,
                owner,
                note,
            });
        }

        let resolved = flatten_innermost(&ranges);
        ranges.sort_by_key(|r| (r.start, r.end));
        Ok(Self { ranges, resolved })
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn len(&self) -> usize {
        self.ranges.len()
    }

    pub fn ranges(&self) -> &[LabelRange] {
        &self.ranges
    }

    /// Owner of a single address `addr` (innermost containing range wins).
    pub fn owner_of(&self, addr: u64) -> Option<&str> {
        let hi = self.resolved.partition_point(|r| r.start <= addr);
        if hi == 0 {
            return None;
        }
        let r = &self.resolved[hi - 1];
        if addr < r.end {
            Some(r.owner.as_str())
        } else {
            None
        }
    }

    /// Splits a half-open region `[base, end)` into disjoint sub-spans attributed to each overlapping
    /// label (and `None` for any unlabeled gaps). For nested/overlapping labels, the innermost
    /// (narrowest) range wins without double-counting any bytes.
    pub fn split_region(&self, base: u64, end: u64) -> Vec<(u64, u64, Option<&str>)> {
        if end <= base || self.resolved.is_empty() {
            return if end > base {
                vec![(base, end, None)]
            } else {
                Vec::new()
            };
        }
        // First resolved interval whose end > base (disjoint & sorted, so partition_point is exact).
        let lo = self.resolved.partition_point(|r| r.end <= base);
        let mut cursor = base;
        let mut out = Vec::new();
        for r in &self.resolved[lo..] {
            if r.start >= end {
                break;
            }
            if r.start > cursor {
                out.push((cursor, r.start.min(end), None));
                cursor = r.start.min(end);
            }
            let seg_end = r.end.min(end);
            if seg_end > cursor {
                out.push((cursor, seg_end, Some(r.owner.as_str())));
                cursor = seg_end;
            }
            if cursor >= end {
                break;
            }
        }
        if cursor < end {
            out.push((cursor, end, None));
        }
        out
    }

    /// Aggregates bytes of `[base, end)` across owners (returning owner -> bytes, `None` for unlabeled).
    pub fn attribute_span(&self, base: u64, end: u64) -> BTreeMap<Option<String>, u64> {
        let mut map = BTreeMap::new();
        for (s, e, owner) in self.split_region(base, end) {
            *map.entry(owner.map(str::to_string)).or_insert(0) += e - s;
        }
        map
    }
}

fn parse_range_token(tok: &str, lineno: usize) -> Result<(u64, u64), String> {
    let trimmed = tok.trim().trim_start_matches('[').trim_end_matches(']').trim();
    let (a, b) = trimmed
        .split_once("..")
        .ok_or_else(|| format!("line {lineno}: invalid range {tok:?}"))?;
    let s = parse_u64(a).map_err(|e| format!("line {lineno}: start: {e}"))?;
    let e = parse_u64(b.trim_start_matches('=')).map_err(|e| format!("line {lineno}: end: {e}"))?;
    Ok((s, e))
}

/// Flattens potentially nested/overlapping `ranges` into sorted, disjoint intervals where the
/// innermost (smallest `end - start`) range wins; equal-size overlaps break ties by later line order.
fn flatten_innermost(ranges: &[LabelRange]) -> Vec<LabelRange> {
    if ranges.is_empty() {
        return Vec::new();
    }
    let mut coords = Vec::with_capacity(ranges.len() * 2);
    for r in ranges {
        coords.push(r.start);
        coords.push(r.end);
    }
    coords.sort_unstable();
    coords.dedup();

    let mut out: Vec<LabelRange> = Vec::new();
    for win in coords.windows(2) {
        let (a, b) = (win[0], win[1]);
        if b <= a {
            continue;
        }
        let best = ranges
            .iter()
            .enumerate()
            .filter(|(_, r)| r.start <= a && b <= r.end)
            .min_by_key(|(idx, r)| (r.end - r.start, std::cmp::Reverse(*idx)));
        if let Some((_, winner)) = best {
            if let Some(prev) = out.last_mut()
                && prev.end == a
                && prev.owner == winner.owner
                && prev.note == winner.note
            {
                prev.end = b;
            } else {
                out.push(LabelRange {
                    start: a,
                    end: b,
                    owner: winner.owner.clone(),
                    note: winner.note.clone(),
                });
            }
        }
    }
    out
}

fn is_header(cols: &[&str]) -> bool {
    let Some(c0) = cols.first() else {
        return false;
    };
    let lc = c0.to_ascii_lowercase();
    matches!(
        lc.as_str(),
        "start" | "base" | "addr" | "address" | "range" | "va"
    )
}

pub fn parse_u64(s: &str) -> Result<u64, String> {
    let s = s.trim().replace('_', "");
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(hex, 16).map_err(|e| format!("invalid hex {s:?}: {e}"))
    } else {
        s.parse::<u64>()
            .map_err(|e| format!("invalid integer {s:?}: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tsv_formats_and_splits_regions() {
        let tsv = "\
# comment
start\tend\towner\tnote
0x1000\t0x3000\tuma_win0\tsparse
0x5000..0x6000\tgpu_upload_1g
[0x7000..0x8000] -> arrow_syntax
";
        let m = LabelMap::parse(tsv).unwrap();
        assert_eq!(m.len(), 3);
        assert_eq!(m.owner_of(0x1000), Some("uma_win0"));
        assert_eq!(m.owner_of(0x2FFF), Some("uma_win0"));
        assert_eq!(m.owner_of(0x3000), None);
        assert_eq!(m.owner_of(0x5500), Some("gpu_upload_1g"));
        assert_eq!(m.owner_of(0x7000), Some("arrow_syntax"));
        assert_eq!(m.owner_of(0x8000), None);

        let parts = m.split_region(0x0, 0x9000);
        assert_eq!(
            parts,
            vec![
                (0x0, 0x1000, None),
                (0x1000, 0x3000, Some("uma_win0")),
                (0x3000, 0x5000, None),
                (0x5000, 0x6000, Some("gpu_upload_1g")),
                (0x6000, 0x7000, None),
                (0x7000, 0x8000, Some("arrow_syntax")),
                (0x8000, 0x9000, None),
            ]
        );
    }

    #[test]
    fn nested_and_overlapping_ranges_innermost_wins_without_double_counting() {
        let tsv = "\
0x10000\t0x50000\tarena_outer
0x20000\t0x40000\theap_mid
0x28000\t0x30000\twindow3_inner
";
        let m = LabelMap::parse(tsv).unwrap();
        assert_eq!(m.owner_of(0x15000), Some("arena_outer"));
        assert_eq!(m.owner_of(0x22000), Some("heap_mid"));
        assert_eq!(m.owner_of(0x2A000), Some("window3_inner"));
        assert_eq!(m.owner_of(0x35000), Some("heap_mid"));
        assert_eq!(m.owner_of(0x45000), Some("arena_outer"));

        let parts = m.split_region(0x10000, 0x50000);
        assert_eq!(
            parts,
            vec![
                (0x10000, 0x20000, Some("arena_outer")),
                (0x20000, 0x28000, Some("heap_mid")),
                (0x28000, 0x30000, Some("window3_inner")),
                (0x30000, 0x40000, Some("heap_mid")),
                (0x40000, 0x50000, Some("arena_outer")),
            ]
        );
        // Conservation: split spans sum to exact region size.
        let total: u64 = parts.iter().map(|(s, e, _)| e - s).sum();
        assert_eq!(total, 0x40000);
    }

    #[test]
    fn parses_start_size_header() {
        let tsv = "base\tsize\towner\n0x10000\t0x4000\tcustom_l0\n";
        let m = LabelMap::parse(tsv).unwrap();
        assert_eq!(m.owner_of(0x13FFF), Some("custom_l0"));
        assert_eq!(m.owner_of(0x14000), None);
    }
}
