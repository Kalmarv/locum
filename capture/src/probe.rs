//! Dispatch-scoped compute buffer probe (`d12_dispatch_probes`), indexed raw-buffer descriptor
//! decoder (`row+0x120` style), app-supplied CPU/GPU alias map, and dispatch-exact byte comparator.
//!
//! Works BOTH with live in-list `d12_dispatch_probes` captured right before a `Dispatch` AND
//! post-hoc against any existing D3D12 `capture.db` (composing UAV `results`, submission
//! `d12_upload_versions`, `CopyBufferRegion`/`CopyResource` chains over `first-use` contents,
//! and `sidecar_regions` / live process memory for the CPU alias).

use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::blobs::Blobs;

/// Additive SQLite table for live dispatch-scoped probes (created only when probes exist).
pub const DISPATCH_PROBES_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS d12_dispatch_probes(
  dispatch_draw INTEGER NOT NULL,
  event INTEGER NOT NULL,
  shader TEXT NOT NULL,
  sample_point TEXT NOT NULL,
  descriptor_source TEXT NOT NULL,
  raw_descriptor BLOB,
  resolved_gpu_va INTEGER,
  resolved_resource INTEGER,
  resolved_offset INTEGER,
  resolved_size INTEGER,
  gpu_bytes_blob TEXT,
  cpu_alias_va INTEGER,
  cpu_bytes_blob TEXT,
  compare_bytes INTEGER NOT NULL,
  mismatch_count INTEGER,
  first_mismatch_offset INTEGER,
  status TEXT NOT NULL,
  note TEXT
);
CREATE INDEX IF NOT EXISTS d12_dispatch_probes_draw ON d12_dispatch_probes(dispatch_draw);
"#;

/// One live dispatch-scoped probe row stored in `d12_dispatch_probes`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DispatchProbeRecord {
    pub dispatch_draw: u64,
    pub event: u64,
    pub shader: String,
    pub sample_point: String,
    pub descriptor_source: String,
    pub raw_descriptor: Vec<u8>,
    pub resolved_gpu_va: Option<u64>,
    pub resolved_resource: Option<u64>,
    pub resolved_offset: Option<u64>,
    pub resolved_size: Option<u64>,
    pub gpu_bytes_blob: Option<String>,
    pub cpu_alias_va: Option<u64>,
    pub cpu_bytes_blob: Option<String>,
    pub compare_bytes: u64,
    pub mismatch_count: Option<u64>,
    pub first_mismatch_offset: Option<u64>,
    pub status: String,
    pub note: Option<String>,
}

/// Appends `rows` to the additive `d12_dispatch_probes` table of an existing `capture.db`.
pub fn append_table(db_path: &Path, rows: &[DispatchProbeRecord]) -> Result<(), String> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut conn = Connection::open(db_path).map_err(|e| format!("open {}: {e}", db_path.display()))?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("begin tx {}: {e}", db_path.display()))?;
    write_table(&tx, rows).map_err(|e| format!("write d12_dispatch_probes: {e}"))?;
    tx.commit()
        .map_err(|e| format!("commit d12_dispatch_probes: {e}"))?;
    Ok(())
}

/// Parses one `settings.txt` `dispatch-probe` specification string:
/// `<shader_prefix> [binding=srv:0] [table-offset=0] [row-stride=0x200] [row=0] [desc-offset=0x120] [bytes=4096] [alias-map=PATH]`.
pub fn parse_settings_probe(spec: &str) -> Result<(ProbeQuery, Option<std::path::PathBuf>), String> {
    let mut tokens = spec.split_whitespace();
    let Some(shader) = tokens.next() else {
        return Err("empty dispatch-probe spec".into());
    };
    let mut query = ProbeQuery {
        shader: Some(shader.to_string()),
        ..Default::default()
    };
    let mut alias_path = None;
    for tok in tokens {
        let Some((k, v)) = tok.split_once('=') else {
            continue;
        };
        match k {
            "binding" => query.binding = BindingSelector::parse(v)?,
            "table-offset" => query.layout.table_offset = parse_hex_or_dec(v, false)?,
            "row-stride" => query.layout.row_stride = parse_hex_or_dec(v, false)?,
            "row" => query.layout.row_index = parse_hex_or_dec(v, false)?,
            "desc-offset" => query.layout.desc_offset = parse_hex_or_dec(v, false)?,
            "desc-size" => query.layout.desc_size = parse_hex_or_dec(v, false)? as usize,
            "va-offset" => query.layout.va_offset = Some(parse_hex_or_dec(v, false)? as usize),
            "bytes" => query.compare_bytes = parse_hex_or_dec(v, false)? as usize,
            "follow" => query.follow_descriptor = !matches!(v, "0" | "false" | "off"),
            "alias-map" => alias_path = Some(std::path::PathBuf::from(v)),
            _ => {}
        }
    }
    Ok((query, alias_path))
}

/// Writes `rows` into the additive `d12_dispatch_probes` table inside `tx`.
pub fn write_table(tx: &Transaction<'_>, rows: &[DispatchProbeRecord]) -> rusqlite::Result<()> {
    tx.execute_batch(DISPATCH_PROBES_SCHEMA)?;
    let mut s = tx.prepare(
        "INSERT INTO d12_dispatch_probes VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
    )?;
    for r in rows {
        s.execute(params![
            r.dispatch_draw as i64,
            r.event as i64,
            r.shader,
            r.sample_point,
            r.descriptor_source,
            r.raw_descriptor,
            r.resolved_gpu_va.map(|v| v as i64),
            r.resolved_resource.map(|v| v as i64),
            r.resolved_offset.map(|v| v as i64),
            r.resolved_size.map(|v| v as i64),
            r.gpu_bytes_blob,
            r.cpu_alias_va.map(|v| v as i64),
            r.cpu_bytes_blob,
            r.compare_bytes as i64,
            r.mismatch_count.map(|v| v as i64),
            r.first_mismatch_offset.map(|v| v as i64),
            r.status,
            r.note,
        ])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// App-supplied GPU VA <-> CPU VA Alias Map
// ---------------------------------------------------------------------------

/// One mapping between a shader-visible GPU virtual-address range and its CPU alias range.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AliasRange {
    pub gpu_start: u64,
    pub cpu_start: u64,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AliasMap {
    ranges: Vec<AliasRange>,
}

impl AliasMap {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading alias map {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parses an alias map file. Accepted line formats (comments `#` and blank lines ignored):
    /// - `gpu_start<TAB>cpu_start<TAB>bytes[<TAB>label]` (or whitespace-separated; optional header)
    /// - `visible <cpu_start> <bytes> <gpu_start>` (compatible with `locum` `sidecar.txt`!)
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut ranges = Vec::new();
        let mut saw_data = false;
        for (idx, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let lineno = idx + 1;
            let cols: Vec<&str> = if line.contains('\t') {
                line.split('\t')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect()
            } else {
                line.split_whitespace().collect()
            };
            if cols.is_empty() {
                continue;
            }
            if !saw_data
                && matches!(
                    cols[0].to_ascii_lowercase().as_str(),
                    "gpu" | "gpu_va" | "gpu_start" | "gpu_addr"
                )
            {
                continue;
            }
            saw_data = true;
            if cols[0].eq_ignore_ascii_case("visible") {
                if cols.len() < 4 {
                    return Err(format!("line {lineno}: expected `visible <cpu> <bytes> <gpu>`"));
                }
                let cpu_start =
                    parse_hex_or_dec(cols[1], true).map_err(|e| format!("line {lineno}: cpu_start: {e}"))?;
                let bytes =
                    parse_hex_or_dec(cols[2], true).map_err(|e| format!("line {lineno}: bytes: {e}"))?;
                let gpu_start =
                    parse_hex_or_dec(cols[3], true).map_err(|e| format!("line {lineno}: gpu_start: {e}"))?;
                let label = (cols.len() > 4).then(|| cols[4..].join(" "));
                ranges.push(AliasRange {
                    gpu_start,
                    cpu_start,
                    bytes,
                    label,
                });
            } else if cols.len() >= 3 {
                let gpu_start =
                    parse_hex_or_dec(cols[0], false).map_err(|e| format!("line {lineno}: gpu_start: {e}"))?;
                let cpu_start =
                    parse_hex_or_dec(cols[1], false).map_err(|e| format!("line {lineno}: cpu_start: {e}"))?;
                let bytes =
                    parse_hex_or_dec(cols[2], false).map_err(|e| format!("line {lineno}: bytes: {e}"))?;
                let label = (cols.len() > 3).then(|| cols[3..].join(" "));
                if bytes == 0 {
                    return Err(format!("line {lineno}: zero-length alias range"));
                }
                ranges.push(AliasRange {
                    gpu_start,
                    cpu_start,
                    bytes,
                    label,
                });
            } else {
                return Err(format!(
                    "line {lineno}: expected `gpu_start\\tcpu_start\\tbytes[\\tlabel]` or `visible <cpu> <bytes> <gpu>`"
                ));
            }
        }
        Ok(Self { ranges })
    }

    pub fn add(&mut self, range: AliasRange) {
        if range.bytes > 0 {
            self.ranges.push(range);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }

    pub fn ranges(&self) -> &[AliasRange] {
        &self.ranges
    }

    /// Resolves a shader-visible GPU VA to its CPU alias VA (preferring the narrowest enclosing range).
    pub fn gpu_to_cpu(&self, gpu_va: u64, len: u64) -> Option<(u64, Option<&str>)> {
        let need = len.max(1);
        let best = self
            .ranges
            .iter()
            .filter(|r| {
                gpu_va >= r.gpu_start && gpu_va.saturating_add(need) <= r.gpu_start.saturating_add(r.bytes)
            })
            .min_by_key(|r| r.bytes)?;
        Some((
            best.cpu_start.saturating_add(gpu_va - best.gpu_start),
            best.label.as_deref(),
        ))
    }

    /// Loads `sidecar_visible` ranges from `capture.db` (if present) into this alias map.
    pub fn extend_from_db(&mut self, conn: &Connection) {
        let has: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sidecar_visible'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if !has {
            return;
        }
        if let Ok(mut stmt) = conn.prepare("SELECT address, bytes, gpu FROM sidecar_visible")
            && let Ok(rows) = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                ))
            })
        {
            for (cpu, bytes, gpu) in rows.flatten() {
                self.add(AliasRange {
                    gpu_start: gpu,
                    cpu_start: cpu,
                    bytes,
                    label: Some("sidecar_visible".into()),
                });
            }
        }
    }
}

fn parse_hex_or_dec(raw: &str, default_hex: bool) -> Result<u64, String> {
    let s = raw.trim().replace('_', "");
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).map_err(|e| format!("invalid hex {raw:?}: {e}"))
    } else if default_hex {
        u64::from_str_radix(&s, 16).map_err(|e| format!("invalid hex {raw:?}: {e}"))
    } else {
        s.parse::<u64>()
            .map_err(|e| format!("invalid integer {raw:?}: {e}"))
    }
}

// ---------------------------------------------------------------------------
// Indexed Raw-Buffer Descriptor Layout & Decoder (`row+0x120` style)
// ---------------------------------------------------------------------------

/// How to locate and decode a shader-decoded descriptor inside a raw/structured buffer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DescriptorLayout {
    /// Base byte offset of the descriptor table inside the bound buffer.
    pub table_offset: u64,
    /// Stride in bytes between rows (0 if single descriptor at `table_offset + desc_offset`).
    pub row_stride: u64,
    /// Row index to inspect.
    pub row_index: u64,
    /// Byte offset of the descriptor within the row (e.g. `0x120`).
    pub desc_offset: u64,
    /// Number of raw descriptor bytes to read and print (default 32).
    pub desc_size: usize,
    /// Byte offset of the 64-bit GPU VA field inside the descriptor (`None` = auto-scan for a valid buffer VA).
    pub va_offset: Option<usize>,
    /// Optional bitmask applied to the decoded VA word (e.g. `0x0000_FFFF_FFFF_FFFF` for 48-bit VA).
    pub va_mask: u64,
    /// Optional left-shift applied after masking (e.g. 8 for 256-byte-aligned packed descriptors).
    pub va_shift: u32,
    /// Optional byte offset of a u32/u64 byte-length field inside the descriptor.
    pub size_offset: Option<usize>,
    /// Width of the size field (4 or 8 bytes; default 4).
    pub size_width: usize,
}

impl Default for DescriptorLayout {
    fn default() -> Self {
        Self {
            table_offset: 0,
            row_stride: 0,
            row_index: 0,
            desc_offset: 0,
            desc_size: 32,
            va_offset: None,
            va_mask: u64::MAX,
            va_shift: 0,
            size_offset: None,
            size_width: 4,
        }
    }
}

impl DescriptorLayout {
    /// Exact byte offset inside the bound descriptor buffer: `table_offset + row_index * row_stride + desc_offset`.
    pub fn byte_offset(&self) -> u64 {
        self.table_offset
            .saturating_add(self.row_index.saturating_mul(self.row_stride))
            .saturating_add(self.desc_offset)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DecodedDescriptor {
    pub raw_hex: String,
    pub words_u32: Vec<u32>,
    pub gpu_va: Option<u64>,
    pub decoded_size: Option<u64>,
    pub decode_note: String,
}

/// Decodes a shader-visible GPU VA (and optional size) from raw descriptor bytes.
pub fn decode_raw_descriptor(
    raw: &[u8],
    layout: &DescriptorLayout,
    live_buffers: &[LiveBuffer],
) -> DecodedDescriptor {
    let raw_hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let words_u32: Vec<u32> = raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();

    let read_size = |off: usize| -> Option<u64> {
        if layout.size_width == 8 {
            let s = raw.get(off..off + 8)?;
            Some(u64::from_le_bytes(s.try_into().ok()?))
        } else {
            let s = raw.get(off..off + 4)?;
            Some(u32::from_le_bytes(s.try_into().ok()?) as u64)
        }
    };

    if let Some(off) = layout.va_offset {
        if let Some(slice) = raw.get(off..off + 8) {
            let word = u64::from_le_bytes(slice.try_into().unwrap());
            let va = (word & layout.va_mask).wrapping_shl(layout.va_shift);
            let sz = layout.size_offset.and_then(read_size);
            return DecodedDescriptor {
                raw_hex,
                words_u32,
                gpu_va: (va != 0).then_some(va),
                decoded_size: sz,
                decode_note: format!(
                    "explicit u64 at +{off:#x} (mask={:#x}, shift={})",
                    layout.va_mask, layout.va_shift
                ),
            };
        }
        return DecodedDescriptor {
            raw_hex,
            words_u32,
            gpu_va: None,
            decoded_size: None,
            decode_note: format!("descriptor too short ({} B) for va_offset +{off:#x}", raw.len()),
        };
    }

    // Auto-scan mode: try each 4-byte-aligned 64-bit word (raw, 48-bit masked, and << 8) against live GPU buffers.
    let mut off = 0usize;
    while off + 8 <= raw.len() {
        let word = u64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
        let candidates = [
            (word, "raw u64"),
            (word & 0x0000_FFFF_FFFF_FFFF, "48-bit VA"),
            ((word & 0x0000_00FF_FFFF_FFFF) << 8, "40-bit<<8 VA"),
        ];
        for (cand, mode) in candidates {
            if cand != 0 && find_buffer(live_buffers, cand).is_some() {
                let sz = layout.size_offset.and_then(read_size).or_else(|| {
                    raw.get(off + 8..off + 12)
                        .map(|s| u32::from_le_bytes(s.try_into().unwrap()) as u64)
                        .filter(|&v| v > 0)
                });
                return DecodedDescriptor {
                    raw_hex,
                    words_u32,
                    gpu_va: Some(cand),
                    decoded_size: sz,
                    decode_note: format!("auto-matched {mode} at +{off:#x} inside live D3D12 buffer"),
                };
            }
        }
        off += 4;
    }

    // Fallback: take first non-zero 64-bit word at +0 even if outside known D3D12 buffers (flags unmapped!).
    let fallback_va = raw
        .get(0..8)
        .map(|s| u64::from_le_bytes(s.try_into().unwrap()))
        .filter(|&v| v != 0);
    DecodedDescriptor {
        raw_hex,
        words_u32,
        gpu_va: fallback_va,
        decoded_size: layout.size_offset.and_then(read_size),
        decode_note: "no word matched a live D3D12 buffer; fell back to u64 at +0x0".into(),
    }
}

// ---------------------------------------------------------------------------
// Dispatch-Scoped Probe Engine against `capture.db`
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct LiveBuffer {
    pub resource: u64,
    pub gpu_address: u64,
    pub width: u64,
    pub content: Option<String>,
    pub content_source: Option<String>,
    pub content_note: Option<String>,
}

pub fn find_buffer(buffers: &[LiveBuffer], gpu_va: u64) -> Option<(&LiveBuffer, u64)> {
    buffers
        .iter()
        .filter(|b| gpu_va >= b.gpu_address && gpu_va < b.gpu_address.saturating_add(b.width))
        .min_by_key(|b| b.width)
        .map(|b| (b, gpu_va - b.gpu_address))
}

/// Binding selector for which compute-dispatch resource holds the raw descriptor table (or target buffer).
#[derive(Clone, Debug, Default)]
pub struct BindingSelector {
    /// Optional kind (`srv`, `uav`, `cb`).
    pub kind: Option<String>,
    /// Optional register slot (`t0` -> 0).
    pub slot: Option<u32>,
    /// Optional register space (default 0 when slot is specified).
    pub space: Option<u32>,
    /// Optional root parameter index.
    pub parameter: Option<u32>,
    /// Optional explicit GPU VA override (skips looking up a bound table buffer).
    pub direct_gpu_va: Option<u64>,
}

impl BindingSelector {
    /// Parses `srv:0`, `uav:1:2`, `cb:0`, `param:3`, or `va:0x...`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let s = spec.trim();
        if let Some(rest) = s.strip_prefix("va:") {
            let va = parse_hex_or_dec(rest, false)?;
            return Ok(Self {
                direct_gpu_va: Some(va),
                ..Default::default()
            });
        }
        if let Some(rest) = s.strip_prefix("param:") {
            let p: u32 = rest
                .parse()
                .map_err(|e| format!("invalid param index {rest:?}: {e}"))?;
            return Ok(Self {
                parameter: Some(p),
                ..Default::default()
            });
        }
        let parts: Vec<&str> = s.split(':').collect();
        if parts.len() >= 2 && matches!(parts[0], "srv" | "uav" | "cb") {
            let slot: u32 = parts[1]
                .trim_start_matches(['t', 'u', 'b'])
                .parse()
                .map_err(|e| format!("invalid register slot {:?}: {e}", parts[1]))?;
            let space = parts
                .get(2)
                .map(|v| {
                    v.trim_start_matches("space")
                        .parse::<u32>()
                        .map_err(|e| format!("invalid register space {v:?}: {e}"))
                })
                .transpose()?;
            return Ok(Self {
                kind: Some(parts[0].to_string()),
                slot: Some(slot),
                space,
                ..Default::default()
            });
        }
        Err(format!(
            "invalid --binding {spec:?} (expected `srv:<slot>[:<space>]`, `uav:<slot>`, `cb:<slot>`, `param:<idx>`, or `va:<hex>`)"
        ))
    }
}

/// Summary of one compute dispatch found in `capture.db` matching `--shader` / `--draw`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComputeDispatchInfo {
    pub draw: u64,
    pub event: u64,
    pub call: String,
    pub args: String,
    pub cs_hash: String,
    pub cs_name: Option<String>,
    pub submission: Option<u64>,
    pub descriptors: Vec<ComputeDescriptorRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComputeDescriptorRow {
    pub kind: String,
    pub slot: u32,
    pub space: u32,
    pub parameter: u32,
    pub resource: Option<u64>,
    pub address: Option<u64>,
    pub size: Option<u64>,
    pub stride: Option<u32>,
    pub flag: Option<String>,
}

/// Byte-by-byte comparison result between shader-visible GPU bytes and CPU alias bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ByteComparison {
    pub requested_bytes: usize,
    pub compared_bytes: usize,
    pub mismatch_count: usize,
    pub first_mismatch_offset: Option<usize>,
    pub status: String,
    pub gpu_preview_hex: String,
    pub cpu_preview_hex: String,
    pub first_mismatch_detail: Option<String>,
}

/// Full result of probing one compute dispatch (including optional `locum-mem` placement enrichment).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DispatchProbeResult {
    pub dispatch: ComputeDispatchInfo,
    pub descriptor_source: String,
    pub descriptor_sample_point: String,
    pub descriptor_note: Option<String>,
    pub decoded: Option<DecodedDescriptor>,
    pub target_gpu_va: Option<u64>,
    pub target_resource: Option<u64>,
    pub target_offset: Option<u64>,
    pub target_buffer_width: Option<u64>,
    pub gpu_sample_point: String,
    pub gpu_unavailable_reason: Option<String>,
    pub cpu_alias_va: Option<u64>,
    pub cpu_alias_label: Option<String>,
    pub cpu_sample_point: String,
    pub cpu_unavailable_reason: Option<String>,
    pub comparison: Option<ByteComparison>,
    // Enriched by CLI with `locum-mem` (`--etl`, `--labels`, live Win32):
    pub gpu_vidmm_placement: Option<String>,
    pub gpu_label_owner: Option<String>,
    pub cpu_label_owner: Option<String>,
    pub cpu_win32_residency: Option<String>,
}

/// Options for probing compute dispatches in a `capture.db`.
#[derive(Clone, Debug)]
pub struct ProbeQuery {
    /// Substring/prefix match against `draws.cs` or `shaders.name` (e.g. `7700e4c3`).
    pub shader: Option<String>,
    /// Explicit `draws.n` filter.
    pub draw: Option<u64>,
    /// Which compute binding holds the indexed raw descriptor buffer (or `va:...`).
    pub binding: BindingSelector,
    /// Layout of the indexed descriptor inside that buffer (`row+0x120` style).
    pub layout: DescriptorLayout,
    /// Follow the decoded descriptor VA to compare target bytes (`true`), or compare the bound buffer slice directly (`false`).
    pub follow_descriptor: bool,
    /// Number of bytes to compare at the target GPU VA vs CPU alias (default 4096).
    pub compare_bytes: usize,
    /// Maximum number of matching dispatches to probe (default 10).
    pub limit: usize,
}

impl Default for ProbeQuery {
    fn default() -> Self {
        Self {
            shader: None,
            draw: None,
            binding: BindingSelector::default(),
            layout: DescriptorLayout::default(),
            follow_descriptor: true,
            compare_bytes: 4096,
            limit: 10,
        }
    }
}

/// Runs the dispatch-scoped compute buffer probe against `capture_path` (`capture.db` or folder),
/// using `alias_map` and an optional live CPU reader callback `read_live_cpu(cpu_va, len)`.
pub fn probe_capture<F>(
    capture_path: &Path,
    query: &ProbeQuery,
    alias_map: &AliasMap,
    mut read_live_cpu: Option<F>,
) -> Result<Vec<DispatchProbeResult>, String>
where
    F: FnMut(u64, usize) -> Result<Vec<u8>, String>,
{
    let db_path = if capture_path.is_dir() {
        capture_path.join("capture.db")
    } else {
        capture_path.to_path_buf()
    };
    let conn = Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open capture {}: {e}", db_path.display()))?;

    let blobs_root: String = conn
        .query_row("SELECT value FROM meta WHERE key='blobs'", [], |r| r.get(0))
        .unwrap_or_else(|_| {
            db_path
                .parent()
                .unwrap_or(Path::new("."))
                .join("blobs")
                .display()
                .to_string()
        });
    let blobs = Blobs::new(&blobs_root);

    let mut merged_aliases = alias_map.clone();
    merged_aliases.extend_from_db(&conn);

    let live_buffers = load_live_buffers(&conn);
    let sidecar_regions = load_sidecar_regions(&conn);
    let dispatches = find_compute_dispatches(&conn, query.shader.as_deref(), query.draw, query.limit)?;
    if dispatches.is_empty() {
        return Err(format!(
            "no matching compute dispatches found in {} (shader={:?}, draw={:?})",
            db_path.display(),
            query.shader,
            query.draw
        ));
    }

    let mut out = Vec::with_capacity(dispatches.len());
    for disp in dispatches {
        let mut res = probe_one_dispatch(
            &conn,
            &blobs,
            &live_buffers,
            &sidecar_regions,
            &merged_aliases,
            &disp,
            query,
            read_live_cpu.as_mut(),
        );
        // Check if `d12_dispatch_probes` has an exact pre-captured live probe for this dispatch!
        if let Some(live) = load_live_probe(&conn, disp.draw)
            && res.gpu_unavailable_reason.is_some()
            && let Some(ref key) = live.gpu_bytes_blob
            && let Ok(gbytes) = blobs.get(key)
        {
            res.gpu_sample_point = live.sample_point.clone();
            res.gpu_unavailable_reason = None;
            let cpu_bytes = live.cpu_bytes_blob.as_deref().and_then(|k| blobs.get(k).ok());
            if let Some(cbytes) = cpu_bytes {
                res.cpu_sample_point = format!("{} (captured cpu alias)", live.sample_point);
                res.cpu_unavailable_reason = None;
                res.comparison = Some(compare_slices(&gbytes, &cbytes, query.compare_bytes));
            }
        }
        out.push(res);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn probe_one_dispatch<F>(
    conn: &Connection,
    blobs: &Blobs,
    live_buffers: &[LiveBuffer],
    sidecar_regions: &[(u64, u64, String, Option<String>)],
    aliases: &AliasMap,
    disp: &ComputeDispatchInfo,
    query: &ProbeQuery,
    read_live_cpu: Option<&mut F>,
) -> DispatchProbeResult
where
    F: FnMut(u64, usize) -> Result<Vec<u8>, String>,
{
    let desc_byte_offset = query.layout.byte_offset();

    // Step 1: Locate either the direct GPU VA or the bound descriptor buffer holding the raw descriptor.
    let (desc_source, desc_sample_point, desc_note, decoded, target_va_opt) = if let Some(va) =
        query.binding.direct_gpu_va
    {
        if !query.follow_descriptor {
            (
                format!("va:{va:#x}"),
                "direct".into(),
                None,
                None,
                Some(va.saturating_add(desc_byte_offset)),
            )
        } else {
            let raw_va = va.saturating_add(desc_byte_offset);
            read_and_decode_at_va(
                conn,
                blobs,
                live_buffers,
                disp,
                raw_va,
                format!("va:{va:#x}+{desc_byte_offset:#x}"),
                &query.layout,
            )
        }
    } else {
        match select_binding(&disp.descriptors, &query.binding) {
            Some(b) => {
                let base_label = format!("{}:{} space={} param={}", b.kind, b.slot, b.space, b.parameter);
                let base_va = b.address.or_else(|| {
                    b.resource.and_then(|rid| {
                        live_buffers
                            .iter()
                            .find(|lb| lb.resource == rid)
                            .map(|lb| lb.gpu_address)
                    })
                });
                if let Some(va) = base_va {
                    if !query.follow_descriptor {
                        (
                            format!("{base_label}+{desc_byte_offset:#x}"),
                            "bound-descriptor".into(),
                            None,
                            None,
                            Some(va.saturating_add(desc_byte_offset)),
                        )
                    } else {
                        let raw_va = va.saturating_add(desc_byte_offset);
                        read_and_decode_at_va(
                            conn,
                            blobs,
                            live_buffers,
                            disp,
                            raw_va,
                            format!("{base_label}+{desc_byte_offset:#x}"),
                            &query.layout,
                        )
                    }
                } else {
                    (
                        base_label,
                        "unavailable".into(),
                        Some("selected compute binding has no GPU address / live resource".into()),
                        None,
                        None,
                    )
                }
            }
            None => (
                "none".into(),
                "unavailable".into(),
                Some(format!(
                    "no matching compute descriptor in d12_draw_descriptors for draw={} ({} cs descriptors present)",
                    disp.draw,
                    disp.descriptors.len()
                )),
                None,
                None,
            ),
        }
    };

    // Step 2: Resolve target GPU VA -> (resource, offset) and read N GPU bytes at this exact dispatch!
    let (target_resource, target_offset, target_buffer_width, gpu_bytes, gpu_sample_point, gpu_unavail) =
        match target_va_opt {
            None => (
                None,
                None,
                None,
                None,
                "unavailable".into(),
                Some("no target GPU VA decoded from descriptor".into()),
            ),
            Some(va) => match find_buffer(live_buffers, va) {
                None => (
                    None,
                    None,
                    None,
                    None,
                    "unavailable".into(),
                    Some(format!(
                        "GPU VA {va:#x} is not inside any live ID3D12Resource buffer"
                    )),
                ),
                Some((buf, off)) => {
                    let avail_len = (buf.width.saturating_sub(off) as usize).min(query.compare_bytes);
                    if avail_len == 0 {
                        (
                            Some(buf.resource),
                            Some(off),
                            Some(buf.width),
                            None,
                            "unavailable".into(),
                            Some(format!("offset {off:#x} is at end of resource #{}", buf.resource)),
                        )
                    } else {
                        let (bytes, sp, note) = read_buffer_at_dispatch(
                            conn,
                            blobs,
                            live_buffers,
                            buf,
                            off,
                            avail_len as u64,
                            disp,
                        );
                        (Some(buf.resource), Some(off), Some(buf.width), bytes, sp, note)
                    }
                }
            },
        };

    // Step 3: Resolve CPU alias VA from alias_map / sidecar_visible and read N CPU bytes!
    let (cpu_alias_va, cpu_alias_label) = match target_va_opt {
        Some(va) => match aliases.gpu_to_cpu(va, query.compare_bytes as u64) {
            Some((cva, lbl)) => (Some(cva), lbl.map(str::to_string)),
            None => (None, None),
        },
        None => (None, None),
    };

    let (cpu_bytes, cpu_sample_point, cpu_unavail) = match cpu_alias_va {
        None => (
            None,
            "unavailable".into(),
            Some(if aliases.is_empty() {
                "no --alias-map supplied and capture has no sidecar_visible table".into()
            } else {
                format!(
                    "target GPU VA {:?} is not covered by any alias-map / sidecar_visible range",
                    target_va_opt.map(|v| format!("{v:#x}"))
                )
            }),
        ),
        Some(cva) => {
            // Priority 1: `sidecar_regions` inside capture.db
            if let Some((reg_base, _reg_size, reg_name, Some(blob_key))) =
                sidecar_regions.iter().find(|(b, sz, _, k)| {
                    k.is_some()
                        && cva >= *b
                        && cva.saturating_add(query.compare_bytes as u64) <= b.saturating_add(*sz)
                })
            {
                let off = cva - reg_base;
                match blobs.read(blob_key, off, Some(query.compare_bytes as u64)) {
                    Ok(bytes) => (
                        Some(bytes),
                        format!("sidecar_region({reg_name}, after-frame)"),
                        None,
                    ),
                    Err(e) => (
                        None,
                        "unavailable".into(),
                        Some(format!("failed reading sidecar_region blob {blob_key}: {e}")),
                    ),
                }
            } else if let Some(reader) = read_live_cpu {
                // Priority 2: Live process ReadProcessMemory (zero suspends)
                match reader(cva, query.compare_bytes) {
                    Ok(bytes) => (Some(bytes), "live_process(ReadProcessMemory)".into(), None),
                    Err(e) => (
                        None,
                        "unavailable".into(),
                        Some(format!("live ReadProcessMemory({cva:#x}): {e}")),
                    ),
                }
            } else {
                (
                    None,
                    "unavailable".into(),
                    Some(format!(
                        "CPU alias {cva:#x} resolved, but no sidecar_regions blob covers it and no --pid supplied for live ReadProcessMemory"
                    )),
                )
            }
        }
    };

    let comparison = match (&gpu_bytes, &cpu_bytes) {
        (Some(g), Some(c)) => Some(compare_slices(g, c, query.compare_bytes)),
        _ => None,
    };

    DispatchProbeResult {
        dispatch: disp.clone(),
        descriptor_source: desc_source,
        descriptor_sample_point: desc_sample_point,
        descriptor_note: desc_note,
        decoded,
        target_gpu_va: target_va_opt,
        target_resource,
        target_offset,
        target_buffer_width,
        gpu_sample_point,
        gpu_unavailable_reason: gpu_unavail,
        cpu_alias_va,
        cpu_alias_label,
        cpu_sample_point,
        cpu_unavailable_reason: cpu_unavail,
        comparison,
        gpu_vidmm_placement: None,
        gpu_label_owner: None,
        cpu_label_owner: None,
        cpu_win32_residency: None,
    }
}

fn read_and_decode_at_va(
    conn: &Connection,
    blobs: &Blobs,
    live_buffers: &[LiveBuffer],
    disp: &ComputeDispatchInfo,
    raw_va: u64,
    source_label: String,
    layout: &DescriptorLayout,
) -> (
    String,
    String,
    Option<String>,
    Option<DecodedDescriptor>,
    Option<u64>,
) {
    let Some((buf, off)) = find_buffer(live_buffers, raw_va) else {
        return (
            source_label,
            "unavailable".into(),
            Some(format!(
                "descriptor address {raw_va:#x} does not fall inside any live ID3D12Resource buffer"
            )),
            None,
            None,
        );
    };
    let (raw_opt, sp, note) = read_buffer_at_dispatch(
        conn,
        blobs,
        live_buffers,
        buf,
        off,
        layout.desc_size.max(8) as u64,
        disp,
    );
    match raw_opt {
        Some(raw) => {
            let dec = decode_raw_descriptor(&raw, layout, live_buffers);
            let target = dec.gpu_va;
            (source_label, sp, note, Some(dec), target)
        }
        None => (source_label, sp, note, None, None),
    }
}

/// Reads `[offset, offset + len)` of `buf` as of `disp.event` in strict causal priority order.
pub fn read_buffer_at_dispatch(
    conn: &Connection,
    blobs: &Blobs,
    live_buffers: &[LiveBuffer],
    buf: &LiveBuffer,
    offset: u64,
    len: u64,
    disp: &ComputeDispatchInfo,
) -> (Option<Vec<u8>>, String, Option<String>) {
    // 1. Check latest UAV `results` row for this resource strictly before `disp.event`.
    let has_results: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='results'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if has_results
        && let Ok((ev, key, res_off, res_bytes)) = conn.query_row(
            "SELECT event, content, offset, bytes FROM results WHERE resource=?1 AND event < ?2 ORDER BY event DESC LIMIT 1",
            params![buf.resource as i64, disp.event as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                    r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                ))
            },
        )
    {
        let base = res_off.unwrap_or(0);
        let avail = res_bytes.unwrap_or(buf.width);
        if offset >= base
            && offset.saturating_add(len) <= base.saturating_add(avail)
            && let Ok(bytes) = blobs.read(&key, offset - base, Some(len))
        {
            return (
                Some(bytes),
                format!("before-dispatch (from UAV result at event {ev})"),
                None,
            );
        }
    }

    // 2. Check submission-scoped `d12_upload_versions` run covering `[offset, offset + len)`.
    if let Some(sub) = disp.submission {
        let has_uv: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='d12_upload_versions'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if has_uv
            && let Ok(mut stmt) = conn.prepare(
                "SELECT offset, content_json FROM d12_upload_versions WHERE submission=?1 AND resource=?2 ORDER BY offset DESC",
            )
            && let Ok(rows) = stmt.query_map(params![sub as i64, buf.resource as i64], |r| {
                Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?))
            })
        {
            for (run_off, json_s) in rows.flatten() {
                if let Ok(v) = serde_json::from_str::<Value>(&json_s)
                    && let (Some(sz), Some(key)) = (v["size"].as_u64(), v["blob"].as_str())
                    && offset >= run_off
                    && offset.saturating_add(len) <= run_off.saturating_add(sz)
                    && let Ok(bytes) = blobs.read(key, offset - run_off, Some(len))
                {
                    return (
                        Some(bytes),
                        format!("before-dispatch (submission {sub} upload run)"),
                        None,
                    );
                }
            }
        }
    }

    // 3. Check `CopyBufferRegion` / `CopyResource` into this resource before `disp.event`!
    let mut base_bytes: Option<Vec<u8>> = None;
    let mut sample_point = String::new();
    let mut missing_reason: Option<String> = None;

    if let Some(ref key) = buf.content {
        match blobs.read(key, offset, Some(len)) {
            Ok(b) => {
                base_bytes = Some(b);
                sample_point = format!(
                    "before-dispatch ({})",
                    buf.content_source.as_deref().unwrap_or("first-use")
                );
            }
            Err(e) => {
                missing_reason = Some(format!(
                    "failed to read resource #{} content blob {key}: {e}",
                    buf.resource
                ));
            }
        }
    } else {
        missing_reason = Some(format!(
            "resource #{} has no captured content ({})",
            buf.resource,
            buf.content_note
                .as_deref()
                .unwrap_or("not copied before first use")
        ));
    }

    // Replay any `CopyBufferRegion` / `CopyResource` writes into `buf.resource` strictly before `disp.event`.
    let mut copy_count = 0usize;
    let mut unfollowed_write: Option<String> = None;
    if let Ok(mut stmt) = conn.prepare(
        "SELECT e.seq, e.call, e.args FROM events e JOIN event_resources er ON er.event = e.seq \
         WHERE er.resource = ?1 AND er.access = 'write' AND e.seq < ?2 ORDER BY e.seq",
    ) && let Ok(rows) = stmt.query_map(params![buf.resource as i64, disp.event as i64], |r| {
        Ok((
            r.get::<_, i64>(0)? as u64,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    }) {
        for (seq, call, args_s) in rows.flatten() {
            if call == "CopyBufferRegion" || call == "CopyResource" {
                let a: Value = serde_json::from_str(&args_s).unwrap_or_default();
                let res_id = |v: &Value| v["resource"].as_u64().or_else(|| v.as_u64());
                let (dst_off, src_id, src_off, copy_len) = if call == "CopyBufferRegion" {
                    (
                        a[1].as_u64().unwrap_or(0),
                        res_id(&a[2]),
                        a[3].as_u64().unwrap_or(0),
                        a[4].as_u64().unwrap_or(0),
                    )
                } else {
                    (0, res_id(&a[1]), 0, buf.width)
                };
                let lo = offset.max(dst_off);
                let hi = offset.saturating_add(len).min(dst_off.saturating_add(copy_len));
                if lo < hi
                    && let Some(sid) = src_id
                    && let Some(src_buf) = live_buffers.iter().find(|b| b.resource == sid)
                    && let Some(ref skey) = src_buf.content
                    && let Ok(src_bytes) = blobs.read(skey, src_off + (lo - dst_off), Some(hi - lo))
                {
                    let dst = base_bytes.get_or_insert_with(|| vec![0u8; len as usize]);
                    let d_lo = (lo - offset) as usize;
                    let d_hi = (d_lo + src_bytes.len()).min(dst.len());
                    dst[d_lo..d_hi].copy_from_slice(&src_bytes[..d_hi - d_lo]);
                    copy_count += 1;
                }
            } else {
                unfollowed_write = Some(format!(
                    "written earlier by {call} at event {seq} without a captured results row"
                ));
            }
        }
    }

    if copy_count > 0 {
        sample_point = format!("before-dispatch (composed over {copy_count} copy op(s))");
    }
    if base_bytes.is_some() {
        (base_bytes, sample_point, unfollowed_write)
    } else {
        (None, "unavailable".into(), missing_reason.or(unfollowed_write))
    }
}

pub fn compare_slices(gpu: &[u8], cpu: &[u8], requested: usize) -> ByteComparison {
    let n = gpu.len().min(cpu.len()).min(requested);
    let mut mismatch_count = 0usize;
    let mut first_off = None;
    for i in 0..n {
        if gpu[i] != cpu[i] {
            mismatch_count += 1;
            if first_off.is_none() {
                first_off = Some(i);
            }
        }
    }
    let hex_preview = |b: &[u8]| -> String {
        b.iter()
            .take(32)
            .map(|x| format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join("")
    };
    let first_mismatch_detail = first_off.map(|off| {
        let g_end = (off + 16).min(n);
        let c_end = (off + 16).min(n);
        format!(
            "at +{off:#x}: gpu={} cpu={}",
            hex_preview(&gpu[off..g_end]),
            hex_preview(&cpu[off..c_end]),
        )
    });
    let status = if mismatch_count == 0 {
        "matched".into()
    } else {
        "differs".into()
    };
    ByteComparison {
        requested_bytes: requested,
        compared_bytes: n,
        mismatch_count,
        first_mismatch_offset: first_off,
        status,
        gpu_preview_hex: hex_preview(&gpu[..n]),
        cpu_preview_hex: hex_preview(&cpu[..n]),
        first_mismatch_detail,
    }
}

fn select_binding<'a>(
    descriptors: &'a [ComputeDescriptorRow],
    sel: &BindingSelector,
) -> Option<&'a ComputeDescriptorRow> {
    if let Some(p) = sel.parameter {
        return descriptors.iter().find(|d| d.parameter == p);
    }
    if let Some(ref k) = sel.kind {
        return descriptors.iter().find(|d| {
            d.kind == *k && sel.slot.is_none_or(|s| d.slot == s) && sel.space.is_none_or(|sp| d.space == sp)
        });
    }
    // Auto-select: first valid buffer SRV or UAV bound to CS.
    descriptors
        .iter()
        .find(|d| d.flag.is_none() && d.address.is_some() && matches!(d.kind.as_str(), "srv" | "uav"))
        .or_else(|| descriptors.iter().find(|d| d.address.is_some()))
}

type ResourceContentMeta = (Option<String>, Option<String>, Option<String>);

fn load_live_buffers(conn: &Connection) -> Vec<LiveBuffer> {
    let mut meta: BTreeMap<u64, ResourceContentMeta> = BTreeMap::new();
    if let Ok(mut stmt) =
        conn.prepare("SELECT id, content, content_source, content_note FROM resources WHERE kind='buffer'")
        && let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
    {
        for (id, c, src, note) in rows.flatten() {
            meta.insert(id, (c, src, note));
        }
    }
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id, description FROM d12_objects WHERE kind='ID3D12Resource'")
        && let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?)))
    {
        for (id, desc_s) in rows.flatten() {
            if let Ok(v) = serde_json::from_str::<Value>(&desc_s)
                && v["dimension"].as_u64() == Some(1)
                && let (Some(va), Some(w)) =
                    (v["gpu_address"].as_u64().filter(|&a| a != 0), v["width"].as_u64())
            {
                let (content, content_source, content_note) = meta.remove(&id).unwrap_or_default();
                out.push(LiveBuffer {
                    resource: id,
                    gpu_address: va,
                    width: w,
                    content,
                    content_source,
                    content_note,
                });
            }
        }
    }
    out
}

fn load_sidecar_regions(conn: &Connection) -> Vec<(u64, u64, String, Option<String>)> {
    let has: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sidecar_regions'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has {
        return Vec::new();
    }
    let mut out = Vec::new();
    if let Ok(mut stmt) = conn.prepare("SELECT address, bytes, name, content FROM sidecar_regions")
        && let Ok(rows) = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })
    {
        for r in rows.flatten() {
            out.push(r);
        }
    }
    out
}

fn load_live_probe(conn: &Connection, draw: u64) -> Option<DispatchProbeRecord> {
    let has: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='d12_dispatch_probes'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has {
        return None;
    }
    conn.query_row(
        "SELECT dispatch_draw, event, shader, sample_point, descriptor_source, raw_descriptor, \
         resolved_gpu_va, resolved_resource, resolved_offset, resolved_size, gpu_bytes_blob, \
         cpu_alias_va, cpu_bytes_blob, compare_bytes, mismatch_count, first_mismatch_offset, status, note \
         FROM d12_dispatch_probes WHERE dispatch_draw=?1 LIMIT 1",
        params![draw as i64],
        |r| {
            Ok(DispatchProbeRecord {
                dispatch_draw: r.get::<_, i64>(0)? as u64,
                event: r.get::<_, i64>(1)? as u64,
                shader: r.get(2)?,
                sample_point: r.get(3)?,
                descriptor_source: r.get(4)?,
                raw_descriptor: r.get::<_, Option<Vec<u8>>>(5)?.unwrap_or_default(),
                resolved_gpu_va: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                resolved_resource: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                resolved_offset: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                resolved_size: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
                gpu_bytes_blob: r.get(10)?,
                cpu_alias_va: r.get::<_, Option<i64>>(11)?.map(|v| v as u64),
                cpu_bytes_blob: r.get(12)?,
                compare_bytes: r.get::<_, i64>(13)? as u64,
                mismatch_count: r.get::<_, Option<i64>>(14)?.map(|v| v as u64),
                first_mismatch_offset: r.get::<_, Option<i64>>(15)?.map(|v| v as u64),
                status: r.get(16)?,
                note: r.get(17)?,
            })
        },
    )
    .ok()
}

fn find_compute_dispatches(
    conn: &Connection,
    shader_filter: Option<&str>,
    draw_filter: Option<u64>,
    limit: usize,
) -> Result<Vec<ComputeDispatchInfo>, String> {
    let shader_names: BTreeMap<String, Option<String>> = conn
        .prepare("SELECT hash, name FROM shaders")
        .ok()
        .and_then(|mut s| {
            s.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
            })
            .ok()
            .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default();

    let mut stmt = conn
        .prepare("SELECT n, event, call, args, cs FROM draws WHERE cs IS NOT NULL ORDER BY n")
        .map_err(|e| format!("query draws: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| format!("read draws: {e}"))?;

    let has_desc: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='d12_draw_descriptors'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    let has_locs: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='d12_event_locations'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);

    let filter_lc = shader_filter.map(|s| s.trim().to_ascii_lowercase());
    let mut out = Vec::new();
    for row in rows.flatten() {
        let (n, event, call, args, cs_hash) = row;
        if let Some(want) = draw_filter
            && n != want
        {
            continue;
        }
        let cs_name = shader_names.get(&cs_hash).cloned().flatten();
        if let Some(ref flt) = filter_lc {
            let hash_match = cs_hash.to_ascii_lowercase().contains(flt);
            let name_match = cs_name
                .as_deref()
                .is_some_and(|nm| nm.to_ascii_lowercase().contains(flt));
            if !hash_match && !name_match {
                continue;
            }
        }
        let submission = if has_locs {
            conn.query_row(
                "SELECT x.submission FROM d12_event_locations l JOIN d12_executions x ON x.id = l.execution WHERE l.event = ?1",
                params![event as i64],
                |r| Ok(r.get::<_, i64>(0)? as u64),
            )
            .ok()
        } else {
            None
        };
        let mut descriptors = Vec::new();
        if has_desc
            && let Ok(mut ds) = conn.prepare(
                "SELECT kind, slot, space, parameter, resource, address, size, stride, flag \
                 FROM d12_draw_descriptors WHERE draw=?1 AND stage='cs' ORDER BY kind, space, slot",
            )
            && let Ok(drows) = ds.query_map(params![n as i64], |r| {
                Ok(ComputeDescriptorRow {
                    kind: r.get(0)?,
                    slot: r.get::<_, i64>(1)? as u32,
                    space: r.get::<_, i64>(2)? as u32,
                    parameter: r.get::<_, i64>(3)? as u32,
                    resource: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                    address: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                    size: r.get::<_, Option<i64>>(6)?.map(|v| v as u64),
                    stride: r.get::<_, Option<i64>>(7)?.map(|v| v as u32),
                    flag: r.get(8)?,
                })
            })
        {
            descriptors.extend(drows.flatten());
        }
        out.push(ComputeDispatchInfo {
            draw: n,
            event,
            call,
            args,
            cs_hash,
            cs_name,
            submission,
            descriptors,
        });
        if out.len() >= limit.max(1) {
            break;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, hash, sidecar};
    use serde_json::json;

    #[test]
    fn planted_truth_compute_dispatch_probe_follows_row_0x120_descriptor_and_detects_cpu_gpu_mismatch() {
        let dir = std::env::temp_dir().join(format!("locum_probe_planted_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blobs = Blobs::new(dir.join("blobs"));

        // 1. Descriptor table buffer (resource 10 at GPU VA 0x2000_0000):
        //    Row 2 (stride 0x200) + offset 0x120 holds a shader-decoded descriptor pointing to
        //    target GPU VA 0x3000_1000 (size 4096 bytes) inside resource 11 (GPU VA 0x3000_0000).
        let mut desc_buf = vec![0u8; 4096];
        let desc_off = 2 * 0x200 + 0x120;
        let target_gpu_va = 0x3000_1000u64;
        desc_buf[desc_off..desc_off + 8].copy_from_slice(&target_gpu_va.to_le_bytes());
        desc_buf[desc_off + 8..desc_off + 12].copy_from_slice(&4096u32.to_le_bytes());
        let desc_key = hash::key(&desc_buf);
        blobs.put(&desc_key, &desc_buf).unwrap();

        // 2. Target GPU buffer (resource 11 at GPU VA 0x3000_0000, 16 KiB):
        //    At offset 0x1000, GPU sees 0xAA bytes (except byte +0x10 is 0xDE).
        let mut gpu_buf = vec![0xAAu8; 16384];
        gpu_buf[0x1000 + 0x10] = 0xDE;
        let gpu_key = hash::key(&gpu_buf);
        blobs.put(&gpu_key, &gpu_buf).unwrap();

        // 3. CPU alias memory (CPU VA 0x7000_0000 -> GPU VA 0x3000_0000):
        //    CPU has 0xAA everywhere (differs from GPU at +0x10: GPU=0xDE vs CPU=0xAA!).
        let cpu_buf = vec![0xAAu8; 16384];
        let cpu_key = hash::key(&cpu_buf);
        blobs.put(&cpu_key, &cpu_buf).unwrap();

        // Build a minimal D3D12 capture.db with a compute dispatch for shader `7700e4c3deadbeef`.
        let mut cap = db::Capture::default();
        cap.meta("api", "d3d12");
        cap.meta("blobs", dir.join("blobs").display());
        cap.shaders.push(db::Shader {
            hash: "7700e4c3deadbeef".into(),
            stage: "cs",
            blob: Some("7700e4c3deadbeef".into()),
            size: 128,
            name: Some("greybox_cs".into()),
        });
        cap.resources.push(db::Resource {
            id: 10,
            kind: "buffer",
            width: 4096,
            bytes: 4096,
            used: true,
            content: Some(desc_key),
            content_source: Some("first-use"),
            ..Default::default()
        });
        cap.resources.push(db::Resource {
            id: 11,
            kind: "buffer",
            width: 16384,
            bytes: 16384,
            used: true,
            content: Some(gpu_key),
            content_source: Some("first-use"),
            ..Default::default()
        });
        cap.events.push(db::Event {
            seq: 1,
            kind: "dispatch",
            call: "Dispatch",
            draw: Some(1),
            args: "[1,1,1]".into(),
            reads: vec![10, 11],
            ..Default::default()
        });
        cap.draws.push(db::Draw {
            n: 1,
            event: 1,
            call: "Dispatch",
            args: "[1,1,1]".into(),
            shaders: [None, None, None, None, None, Some("7700e4c3deadbeef".into())],
            ..Default::default()
        });
        cap.draw_descriptors.push(db::DrawDescriptor {
            draw: 1,
            stage: "cs",
            kind: "srv",
            slot: 0,
            space: 0,
            parameter: 0,
            resource: Some(10),
            address: Some(0x2000_0000),
            size: Some(4096),
            ..Default::default()
        });
        let frame = crate::d3d12::Frame {
            objects: vec![
                crate::d3d12::Object {
                    id: 10,
                    kind: "ID3D12Resource",
                    device: 1,
                    description: json!({"dimension": 1, "width": 4096, "gpu_address": 0x2000_0000u64}),
                },
                crate::d3d12::Object {
                    id: 11,
                    kind: "ID3D12Resource",
                    device: 1,
                    description: json!({"dimension": 1, "width": 16384, "gpu_address": 0x3000_0000u64}),
                },
            ],
            ..Default::default()
        };
        let proj = crate::d3d12::Projection::new(&frame.objects);
        cap.d3d12 = Some(frame.finish(&mut db::Capture::default(), proj));
        cap.sidecar = Some(sidecar::Captured {
            sidecar: sidecar::Sidecar {
                regions: vec![sidecar::Region {
                    address: 0x7000_0000,
                    bytes: 16384,
                    name: "uma_cpu_alias".into(),
                }],
                visible: vec![sidecar::Visible {
                    address: 0x7000_0000,
                    bytes: 16384,
                    gpu: 0x3000_0000,
                }],
                descriptors: Vec::new(),
            },
            copied: vec![sidecar::Copied {
                content: Some(cpu_key),
                note: None,
            }],
        });

        let db_path = dir.join("capture.db");
        db::write(&db_path, &cap).unwrap();

        let query = ProbeQuery {
            shader: Some("7700e4c3".into()),
            draw: None,
            binding: BindingSelector::parse("srv:0").unwrap(),
            layout: DescriptorLayout {
                row_stride: 0x200,
                row_index: 2,
                desc_offset: 0x120,
                desc_size: 32,
                ..Default::default()
            },
            follow_descriptor: true,
            compare_bytes: 4096,
            limit: 5,
        };
        let results = probe_capture::<fn(u64, usize) -> Result<Vec<u8>, String>>(
            &db_path,
            &query,
            &AliasMap::default(),
            None,
        )
        .unwrap();
        assert_eq!(results.len(), 1);
        let r = &results[0];
        assert_eq!(r.target_gpu_va, Some(0x3000_1000));
        assert_eq!(r.target_resource, Some(11));
        assert_eq!(r.target_offset, Some(0x1000));
        assert_eq!(r.cpu_alias_va, Some(0x7000_1000));
        let cmp = r.comparison.as_ref().expect("comparison computed");
        assert_eq!(cmp.status, "differs");
        assert_eq!(cmp.compared_bytes, 4096);
        assert_eq!(cmp.mismatch_count, 1);
        assert_eq!(cmp.first_mismatch_offset, Some(0x10));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn alias_map_parses_tsv_and_sidecar_visible_syntax() {
        let map = AliasMap::parse(
            "# comment\n\
             gpu_start\tcpu_start\tbytes\tlabel\n\
             0x300000000\t0xb00000000\t0x100000000\tcoherent_4g\n\
             visible 800000000 40000000 200000000 uma_win0\n",
        )
        .unwrap();
        assert_eq!(
            map.gpu_to_cpu(0x300001234, 4096),
            Some((0xb00001234, Some("coherent_4g")))
        );
        assert_eq!(
            map.gpu_to_cpu(0x200000100, 64),
            Some((0x800000100, Some("uma_win0")))
        );
    }
}
