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
    /// Optional 2-level indirection: read a `u32` slot index at `byte_offset()`, then read the
    /// guest view at `indirect_heap_va + slot_index * indirect_stride` (e.g. `0x4c610000 + 32 * 0x8bf6`).
    pub indirect_heap_va: Option<u64>,
    /// Stride in bytes of entries in `indirect_heap_va` (default 32).
    pub indirect_stride: u64,
    /// Size in bytes of the indirect guest view to read and decode (default 16).
    pub indirect_view_size: usize,
    /// Optional window base GPU VA added when decoding a relative offset inside a guest view.
    pub window_base: Option<u64>,
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
            indirect_heap_va: None,
            indirect_stride: 32,
            indirect_view_size: 16,
            window_base: None,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indirect_view_va: Option<u64>,
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
            let mut va = (word & layout.va_mask).wrapping_shl(layout.va_shift);
            if let Some(wb) = layout.window_base {
                va = wb.saturating_add(va);
            }
            let sz = layout.size_offset.and_then(read_size);
            return DecodedDescriptor {
                slot_index: None,
                indirect_view_va: None,
                raw_hex,
                words_u32,
                gpu_va: (va != 0).then_some(va),
                decoded_size: sz,
                decode_note: format!(
                    "explicit u64 at +{off:#x} (mask={:#x}, shift={}, window_base={:?})",
                    layout.va_mask,
                    layout.va_shift,
                    layout.window_base.map(|w| format!("{w:#x}"))
                ),
            };
        }
        return DecodedDescriptor {
            slot_index: None,
            indirect_view_va: None,
            raw_hex,
            words_u32,
            gpu_va: None,
            decoded_size: None,
            decode_note: format!("descriptor too short ({} B) for va_offset +{off:#x}", raw.len()),
        };
    }

    // Auto-scan mode: try each 4-byte-aligned 64-bit word (raw, 48-bit masked, << 8, and window_base-relative) against live GPU buffers.
    let mut off = 0usize;
    while off + 8 <= raw.len() {
        let word = u64::from_le_bytes(raw[off..off + 8].try_into().unwrap());
        let mut candidates = vec![
            (word, "raw u64"),
            (word & 0x0000_FFFF_FFFF_FFFF, "48-bit VA"),
            ((word & 0x0000_00FF_FFFF_FFFF) << 8, "40-bit<<8 VA"),
        ];
        if let Some(wb) = layout.window_base {
            candidates.push((wb.saturating_add(word & 0xFFFF_FFFF), "window_base+u32"));
            candidates.push((
                wb.saturating_add((word & 0xFFFF_FFFF) << 8),
                "window_base+(u32<<8)",
            ));
        }
        for (cand, mode) in candidates {
            if cand != 0 && find_buffer(live_buffers, cand).is_some() {
                let sz = layout.size_offset.and_then(read_size).or_else(|| {
                    raw.get(off + 8..off + 12)
                        .map(|s| u32::from_le_bytes(s.try_into().unwrap()) as u64)
                        .filter(|&v| v > 0)
                });
                return DecodedDescriptor {
                    slot_index: None,
                    indirect_view_va: None,
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
        .filter(|&v| v != 0)
        .map(|v| layout.window_base.map_or(v, |wb| wb.saturating_add(v)));
    DecodedDescriptor {
        slot_index: None,
        indirect_view_va: None,
        raw_hex,
        words_u32,
        gpu_va: fallback_va,
        decoded_size: layout.size_offset.and_then(read_size),
        decode_note: "no word matched a live D3D12 buffer; fell back to u64 at +0x0".into(),
    }
}

/// Guest shader hash -> translated host D3D12 `draws.cs` hash mapping (`--shader-map`).
#[derive(Clone, Debug, Default)]
pub struct ShaderHashMap {
    pairs: Vec<(String, String)>,
}

impl ShaderHashMap {
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading shader map {}: {e}", path.display()))?;
        Ok(Self::parse(&text))
    }

    /// Parses lines of `guest_hash<TAB/space>host_hash[...]` (comments `#` and blank lines ignored).
    pub fn parse(text: &str) -> Self {
        let mut pairs = Vec::new();
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() >= 2 {
                pairs.push((cols[0].to_ascii_lowercase(), cols[1].to_ascii_lowercase()));
            }
        }
        Self { pairs }
    }

    /// Expands `query` (e.g. guest hash `7700e4c3`) into all matching host/guest hashes.
    pub fn expand(&self, query: &str) -> Vec<String> {
        let q = query.trim().to_ascii_lowercase();
        let mut out = vec![q.clone()];
        for (a, b) in &self.pairs {
            if (a.contains(&q) || q.contains(a)) && !out.contains(b) {
                out.push(b.clone());
            }
            if (b.contains(&q) || q.contains(b)) && !out.contains(a) {
                out.push(a.clone());
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Dispatch-Scoped Probe Engine against `capture.db`
// ---------------------------------------------------------------------------

/// Live `ID3D12Heap` metadata loaded from `d12_objects`.
#[derive(Clone, Debug, Default)]
pub struct HeapMeta {
    pub id: u64,
    pub bytes: u64,
    pub heap_type: Option<u64>,
    pub cpu_page: Option<u64>,
    pub memory_pool: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct LiveBuffer {
    pub resource: u64,
    pub gpu_address: u64,
    pub width: u64,
    pub content: Option<String>,
    pub content_source: Option<String>,
    pub content_note: Option<String>,
    pub heap: Option<u64>,
    pub heap_offset: Option<u64>,
    pub heap_type: Option<u64>,
    pub cpu_page: Option<u64>,
    pub memory_pool: Option<u64>,
    pub reserved: bool,
}

pub fn find_buffer(buffers: &[LiveBuffer], gpu_va: u64) -> Option<(&LiveBuffer, u64)> {
    buffers
        .iter()
        .filter(|b| gpu_va >= b.gpu_address && gpu_va < b.gpu_address.saturating_add(b.width))
        .min_by_key(|b| b.width)
        .map(|b| (b, gpu_va - b.gpu_address))
}

fn format_heap_props(htype: Option<u64>, cpu_page: Option<u64>, pool: Option<u64>) -> String {
    let ty = match htype {
        Some(1) => "DEFAULT",
        Some(2) => "UPLOAD",
        Some(3) => "READBACK",
        Some(4) => "CUSTOM",
        Some(5) => "GPU_UPLOAD",
        _ => "UNKNOWN",
    };
    let mut parts = vec![ty.to_string()];
    match pool {
        Some(1) => parts.push("L0_SYS".into()),
        Some(2) => parts.push("L1_VRAM".into()),
        _ => {}
    }
    match cpu_page {
        Some(1) => parts.push("CPU_NONE".into()),
        Some(2) => parts.push("CPU_WC".into()),
        Some(3) => parts.push("CPU_WB".into()),
        _ => {}
    }
    parts.join("/")
}

fn describe_d3d12_placement(
    buf: &LiveBuffer,
    heaps: &BTreeMap<u64, HeapMeta>,
    sparse: Option<&SparseTileCoverage>,
) -> String {
    if buf.reserved {
        return match sparse {
            Some(sp) if sp.mapped_tiles == 0 => {
                if let Some(ref diag) = sp.diagnostic {
                    format!("reserved(tile_table_unknown: {diag})")
                } else {
                    "reserved(unmapped_tiles)".into()
                }
            }
            Some(sp) => {
                let heap_desc: Vec<String> = sp
                    .backing_heaps
                    .iter()
                    .map(|hid| match heaps.get(hid) {
                        Some(h) if h.heap_type.is_some() => format!(
                            "heap#{hid}:{}",
                            format_heap_props(h.heap_type, h.cpu_page, h.memory_pool)
                        ),
                        _ => format!("heap#{hid}"),
                    })
                    .collect();
                let state = if sp.unmapped_tiles == 0 {
                    "mapped".to_string()
                } else {
                    format!("partial {}/{} tiles", sp.mapped_tiles, sp.tile_count)
                };
                format!("reserved({state} -> {})", heap_desc.join(","))
            }
            None => "reserved(tile_table_unknown)".into(),
        };
    }
    if let Some(hid) = buf.heap {
        let off = buf.heap_offset.unwrap_or(0);
        let props = heaps
            .get(&hid)
            .filter(|h| h.heap_type.is_some())
            .map(|h| format_heap_props(h.heap_type, h.cpu_page, h.memory_pool))
            .or_else(|| {
                buf.heap_type
                    .map(|_| format_heap_props(buf.heap_type, buf.cpu_page, buf.memory_pool))
            });
        return match props {
            Some(p) => format!("placed(heap#{hid}+{off:#x}, {p})"),
            None => format!("placed(heap#{hid}+{off:#x})"),
        };
    }
    if buf.heap_type.is_some() {
        return format!(
            "committed({})",
            format_heap_props(buf.heap_type, buf.cpu_page, buf.memory_pool)
        );
    }
    "unknown".into()
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

/// Summary of one dispatch or draw event found in `capture.db` matching `--event` / `--shader` / `--draw`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComputeDispatchInfo {
    pub draw: u64,
    pub event: u64,
    pub call: String,
    pub args: String,
    pub cs_hash: String,
    pub cs_name: Option<String>,
    pub stage: String,
    pub submission: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submission_cpu_seq: Option<u64>,
    pub descriptors: Vec<ComputeDescriptorRow>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ComputeDescriptorRow {
    pub stage: String,
    pub kind: String,
    pub slot: u32,
    pub space: u32,
    pub parameter: u32,
    pub heap: Option<u64>,
    pub heap_slot: Option<u32>,
    pub version: Option<u64>,
    pub resource: Option<u64>,
    pub address: Option<u64>,
    pub size: Option<u64>,
    pub stride: Option<u32>,
    pub format: Option<u32>,
    pub dimension: Option<u32>,
    pub flag: Option<String>,
}

/// Sparse/tiled page coverage for a reserved (`ID3D12Resource` with null `heap_properties`) buffer range.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SparseTileCoverage {
    pub tile_size: u64,
    pub first_tile: u32,
    pub tile_count: u32,
    pub mapped_tiles: u32,
    pub unmapped_tiles: u32,
    pub backing_heaps: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_tile_mapping: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

/// Resolved shader binding view and byte range at this event (Requirement #1).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResolvedView {
    /// `bound`, `missing_root_arg`, `empty_descriptor_slot`, `other_descriptor_type`,
    /// `no_heap`, `null_or_unmapped_va`, `unmapped_sparse_tile`, `partially_mapped_sparse_tile`, or `flagged:<flag>`.
    pub binding_status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub binding_reason: Option<String>,
    pub stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub space: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameter: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heap: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heap_slot: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub descriptor_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_va: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspected_byte_offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inspected_byte_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_width: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stride: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sparse_tiles: Option<SparseTileCoverage>,
}

/// The write or copy that supplied the bytes in the resolved range before execution (Requirement #2).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LastWriterInfo {
    /// `uav_result`, `copy_buffer_region`, `copy_resource`, `upload_version`,
    /// `clear_uav`, `unrecorded_gpu_write`, `first_use_initial`, `unmapped_tile`, `none`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submission: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_resource: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_range: Option<(u64, u64)>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upload_submission: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
    pub detail: String,
}

/// Explicitly distinguishes a missing binding or unmapped sparse tile from a legitimately zero-filled asset.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ContentVerdict {
    /// `missing_binding`, `unmapped_sparse_tile`, `legitimate_zero`, `non_zero`, `bytes_unavailable`.
    pub verdict: String,
    pub inspected_bytes: usize,
    pub nonzero_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_nonzero_offset: Option<usize>,
    pub summary: String,
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

/// Earlier pre-write snapshot bytes (e.g. `first-use` or prior upload/copy) exposed separately
/// when execution-time bytes are unavailable because a subsequent unrecorded GPU write could overlap the slice.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EarlierSnapshot {
    pub sample_point: String,
    pub inspected_bytes: usize,
    pub nonzero_bytes: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_nonzero_offset: Option<usize>,
    pub hex_preview: String,
}

/// Full result of probing one compute dispatch or draw event (including view, last-writer, placement, and optional CPU-alias diff).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DispatchProbeResult {
    pub dispatch: ComputeDispatchInfo,
    pub resolved_view: ResolvedView,
    pub last_writer: LastWriterInfo,
    pub content_verdict: ContentVerdict,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub earlier_snapshot: Option<EarlierSnapshot>,
    pub d3d12_placement: String,
    pub observed_placement: String,
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

/// Options for probing dispatches/draws in a `capture.db`.
#[derive(Clone, Debug)]
pub struct ProbeQuery {
    /// Substring/prefix match against `draws.cs` (or selected `--stage`) or `shaders.name` (e.g. `7700e4c3`).
    pub shader: Option<String>,
    /// Additional shader hash aliases expanded from `--shader-map` (e.g. guest `7700e4c3` -> translated `draws.cs`).
    pub shader_aliases: Vec<String>,
    /// Explicit `draws.n` filter.
    pub draw: Option<u64>,
    /// Explicit `events.seq` filter (works for both compute dispatches and graphics draws).
    pub event: Option<u64>,
    /// Optional shader stage filter (`cs`, `vs`, `hs`, `ds`, `gs`, `ps`, `all`; default `cs` when searching by shader, auto for `--event`/`--draw`).
    pub stage: Option<String>,
    /// Which binding holds the target view or indexed raw descriptor buffer (`srv:0`, `uav:1`, `cb:0`, `param:2`, or `va:...`).
    pub binding: BindingSelector,
    /// Layout of the indexed descriptor inside that buffer (`row+0x120` style).
    pub layout: DescriptorLayout,
    /// Follow the decoded descriptor VA to compare target bytes (`true`), or inspect the bound view directly (`false`).
    pub follow_descriptor: bool,
    /// Number of bytes to inspect/compare at the target GPU VA vs CPU alias (default 4096).
    pub compare_bytes: usize,
    /// Maximum number of matching events to probe (0 = unlimited).
    pub limit: usize,
}

impl Default for ProbeQuery {
    fn default() -> Self {
        Self {
            shader: None,
            shader_aliases: Vec::new(),
            draw: None,
            event: None,
            stage: None,
            binding: BindingSelector::default(),
            layout: DescriptorLayout::default(),
            follow_descriptor: true,
            compare_bytes: 4096,
            limit: 10,
        }
    }
}

/// Runs the dispatch/event-scoped buffer probe against `capture_path` (`capture.db` or folder),
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

    let (live_buffers, live_heaps) = load_live_buffers_and_heaps(&conn);
    let sidecar_regions = load_sidecar_regions(&conn);
    let dispatches = find_matching_events(
        &conn,
        query.shader.as_deref(),
        &query.shader_aliases,
        query.draw,
        query.event,
        query.stage.as_deref(),
        query.limit,
    )?;
    if dispatches.is_empty() {
        return Err(format!(
            "no matching dispatches/draws found in {} (event={:?}, shader={:?}, draw={:?}, stage={:?})",
            db_path.display(),
            query.event,
            query.shader,
            query.draw,
            query.stage,
        ));
    }

    let mut out = Vec::with_capacity(dispatches.len());
    for disp in dispatches {
        let mut res = probe_one_dispatch(
            &conn,
            &blobs,
            &live_buffers,
            &live_heaps,
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
            res.earlier_snapshot = None;
            res.content_verdict = classify_content(
                &res.resolved_view,
                Some(&gbytes),
                query.compare_bytes,
                &res.last_writer,
                None,
                None,
            );
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
    live_heaps: &BTreeMap<u64, HeapMeta>,
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

    // Step 1: Resolve shader binding (root arg -> descriptor -> view) AND optionally follow indexed raw descriptor.
    let mut view = ResolvedView {
        binding_status: "missing_root_arg".into(),
        binding_reason: None,
        stage: disp.stage.clone(),
        kind: query.binding.kind.clone(),
        slot: query.binding.slot,
        space: query.binding.space,
        parameter: query.binding.parameter,
        heap: None,
        heap_slot: None,
        descriptor_version: None,
        flag: None,
        resource: None,
        gpu_va: None,
        byte_offset: None,
        byte_length: None,
        inspected_byte_offset: None,
        inspected_byte_length: None,
        resource_width: None,
        stride: None,
        format: None,
        dimension: None,
        sparse_tiles: None,
    };

    let (desc_source, desc_sample_point, desc_note, decoded, target_va_opt, view_len_hint) =
        if let Some(va) = query.binding.direct_gpu_va {
            view.binding_status = "bound".into();
            view.gpu_va = Some(va.saturating_add(desc_byte_offset));
            if !query.follow_descriptor {
                (
                    format!("va:{va:#x}"),
                    "direct".into(),
                    None,
                    None,
                    Some(va.saturating_add(desc_byte_offset)),
                    None,
                )
            } else {
                let raw_va = va.saturating_add(desc_byte_offset);
                let (src, sp, note, dec, target) = read_and_decode_at_va(
                    conn,
                    blobs,
                    live_buffers,
                    disp,
                    raw_va,
                    format!("va:{va:#x}+{desc_byte_offset:#x}"),
                    &query.layout,
                );
                let sz = dec.as_ref().and_then(|d| d.decoded_size);
                (src, sp, note, dec, target, sz)
            }
        } else {
            match select_binding(&disp.descriptors, &query.binding) {
                Some(b) => {
                    let base_label = format!(
                        "{}:{}:{} space={} param={}",
                        b.stage, b.kind, b.slot, b.space, b.parameter
                    );
                    view.stage = b.stage.clone();
                    view.kind = Some(b.kind.clone());
                    view.slot = Some(b.slot);
                    view.space = Some(b.space);
                    view.parameter = Some(b.parameter);
                    view.heap = b.heap;
                    view.heap_slot = b.heap_slot;
                    view.descriptor_version = b.version;
                    view.flag = b.flag.clone();
                    view.stride = b.stride;
                    view.format = b.format;
                    view.dimension = b.dimension;

                    let base_va = b.address.or_else(|| {
                        b.resource.and_then(|rid| {
                            live_buffers
                                .iter()
                                .find(|lb| lb.resource == rid)
                                .map(|lb| lb.gpu_address)
                        })
                    });

                    if let Some(ref flg) = b.flag {
                        view.binding_status = match flg.as_str() {
                            "empty" => "empty_descriptor_slot".into(),
                            "other-type" => "other_descriptor_type".into(),
                            "no-heap" => "no_heap".into(),
                            "no-resource" | "null" => "null_or_unmapped_va".into(),
                            other => format!("flagged:{other}"),
                        };
                        let why = format!(
                            "binding {base_label} flagged '{flg}' (heap={:?}, slot={:?}, version={:?})",
                            b.heap, b.heap_slot, b.version
                        );
                        view.binding_reason = Some(why.clone());
                        (base_label, "unavailable".into(), Some(why), None, None, b.size)
                    } else if let Some(va) = base_va {
                        view.binding_status = "bound".into();
                        if !query.follow_descriptor {
                            let eff_va = va.saturating_add(desc_byte_offset);
                            let eff_size = b.size.map(|s| s.saturating_sub(desc_byte_offset));
                            view.gpu_va = Some(eff_va);
                            view.byte_length = eff_size;
                            (
                                if desc_byte_offset > 0 {
                                    format!("{base_label}+{desc_byte_offset:#x}")
                                } else {
                                    base_label
                                },
                                "bound-descriptor".into(),
                                None,
                                None,
                                Some(eff_va),
                                eff_size,
                            )
                        } else {
                            let raw_va = va.saturating_add(desc_byte_offset);
                            let (src, sp, note, dec, target) = read_and_decode_at_va(
                                conn,
                                blobs,
                                live_buffers,
                                disp,
                                raw_va,
                                format!("{base_label}+{desc_byte_offset:#x}"),
                                &query.layout,
                            );
                            let sz = dec.as_ref().and_then(|d| d.decoded_size);
                            if target.is_none() {
                                view.binding_status = "null_or_unmapped_va".into();
                                view.binding_reason = note
                                    .clone()
                                    .or_else(|| Some("raw descriptor decoded to null/zero GPU VA".into()));
                            }
                            (src, sp, note, dec, target, sz)
                        }
                    } else {
                        view.binding_status = "null_or_unmapped_va".into();
                        let why = format!(
                            "binding {base_label} has no GPU address / live resource (resource={:?})",
                            b.resource
                        );
                        view.binding_reason = Some(why.clone());
                        (base_label, "unavailable".into(), Some(why), None, None, b.size)
                    }
                }
                None => {
                    let why = diagnose_missing_binding(conn, disp, &query.binding);
                    view.binding_status = "missing_root_arg".into();
                    view.binding_reason = Some(why.clone());
                    ("none".into(), "unavailable".into(), Some(why), None, None, None)
                }
            }
        };

    // Step 2: Resolve target GPU VA -> (resource, offset, byte range, sparse tiles, last-writer, D3D12 placement).
    let mut last_writer = LastWriterInfo {
        kind: "none".into(),
        event: None,
        submission: None,
        call: None,
        source_resource: None,
        source_offset: None,
        byte_range: None,
        upload_submission: None,
        blob: None,
        detail: "no resolved target resource".into(),
    };
    let mut d3d12_placement = "unknown".to_string();
    let mut earlier_snapshot: Option<EarlierSnapshot> = None;

    let (target_resource, target_offset, target_buffer_width, gpu_bytes, gpu_sample_point, gpu_unavail) =
        match target_va_opt {
            None => (
                None,
                None,
                None,
                None,
                "unavailable".into(),
                view.binding_reason
                    .clone()
                    .or_else(|| Some("no target GPU VA resolved from binding/descriptor".into())),
            ),
            Some(va) => match find_buffer(live_buffers, va) {
                None => {
                    view.binding_status = "null_or_unmapped_va".into();
                    let why = format!("GPU VA {va:#x} is not inside any live ID3D12Resource buffer");
                    view.binding_reason = Some(why.clone());
                    (None, None, None, None, "unavailable".into(), Some(why))
                }
                Some((buf, off)) => {
                    let view_len = view_len_hint
                        .map(|s| s.min(buf.width.saturating_sub(off)))
                        .unwrap_or_else(|| buf.width.saturating_sub(off));
                    let avail_len = (view_len.max(buf.width.saturating_sub(off)) as usize)
                        .min(buf.width.saturating_sub(off) as usize)
                        .min(query.compare_bytes);
                    view.resource = Some(buf.resource);
                    view.gpu_va = Some(va);
                    view.byte_offset = Some(off);
                    view.byte_length = Some(view_len);
                    view.inspected_byte_offset = Some(off);
                    view.inspected_byte_length = Some(avail_len as u64);
                    view.resource_width = Some(buf.width);

                    // Check sparse/tiled mapping coverage for reserved resources as of this event's submission!
                    if buf.reserved {
                        let slice_len = (avail_len as u64).max(1);
                        let view_range = view_len.max(slice_len);
                        let sp_slice = inspect_sparse_tiles(conn, buf.resource, off, slice_len, disp);
                        let sp = if view_range > slice_len {
                            inspect_sparse_tiles(conn, buf.resource, off, view_range, disp)
                        } else {
                            sp_slice.clone()
                        };
                        if sp_slice.mapped_tiles == 0 && sp_slice.diagnostic.is_none() {
                            view.binding_status = "unmapped_sparse_tile".into();
                            view.binding_reason = Some(format!(
                                "reserved resource #{} inspected tiles {}..{} ({} tile(s) at [{off:#x}, {:#x})) are UNMAPPED at submission {:?} (GPU reads zero; whole view has {}/{} tiles mapped){}",
                                buf.resource,
                                sp_slice.first_tile,
                                sp_slice.first_tile + sp_slice.tile_count,
                                sp_slice.tile_count,
                                off.saturating_add(slice_len),
                                disp.submission,
                                sp.mapped_tiles,
                                sp.tile_count,
                                sp.last_tile_mapping
                                    .as_deref()
                                    .map(|m| format!("; last tile op: {m}"))
                                    .unwrap_or_default(),
                            ));
                        } else if sp.unmapped_tiles > 0 && sp.diagnostic.is_none() {
                            view.binding_status = "partially_mapped_sparse_tile".into();
                            view.binding_reason = Some(format!(
                                "reserved resource #{} has {}/{} tiles mapped ({} unmapped) covering [{off:#x}, {:#x}); inspected slice [{off:#x}, {:#x}) has {}/{} mapped",
                                buf.resource,
                                sp.mapped_tiles,
                                sp.tile_count,
                                sp.unmapped_tiles,
                                off.saturating_add(view_range),
                                off.saturating_add(slice_len),
                                sp_slice.mapped_tiles,
                                sp_slice.tile_count,
                            ));
                        }
                        view.sparse_tiles = Some(sp);
                    }

                    d3d12_placement = describe_d3d12_placement(buf, live_heaps, view.sparse_tiles.as_ref());

                    if view.binding_status == "unmapped_sparse_tile" {
                        last_writer = LastWriterInfo {
                            kind: "unmapped_tile".into(),
                            event: None,
                            submission: disp.submission,
                            call: view
                                .sparse_tiles
                                .as_ref()
                                .and_then(|s| s.last_tile_mapping.clone()),
                            source_resource: None,
                            source_offset: None,
                            byte_range: Some((off, off.saturating_add(view_len))),
                            upload_submission: None,
                            blob: None,
                            detail: view
                                .binding_reason
                                .clone()
                                .unwrap_or_else(|| "unmapped sparse tiles".into()),
                        };
                        (
                            Some(buf.resource),
                            Some(off),
                            Some(buf.width),
                            None,
                            "unmapped-sparse-tile".into(),
                            view.binding_reason.clone(),
                        )
                    } else if avail_len == 0 {
                        let why = format!("offset {off:#x} is at end of resource #{}", buf.resource);
                        last_writer.detail = why.clone();
                        (
                            Some(buf.resource),
                            Some(off),
                            Some(buf.width),
                            None,
                            "unavailable".into(),
                            Some(why),
                        )
                    } else {
                        let read = read_buffer_with_provenance(
                            conn,
                            blobs,
                            live_buffers,
                            buf,
                            off,
                            avail_len as u64,
                            disp,
                        );
                        last_writer = read.last_writer;
                        if let Some(ref eb) = read.earlier_snapshot_bytes {
                            earlier_snapshot = Some(summarize_earlier_snapshot(
                                eb,
                                query.compare_bytes,
                                read.earlier_snapshot_sample_point
                                    .as_deref()
                                    .unwrap_or("earlier snapshot"),
                            ));
                        }
                        (
                            Some(buf.resource),
                            Some(off),
                            Some(buf.width),
                            read.bytes,
                            read.sample_point,
                            read.unavailable_reason,
                        )
                    }
                }
            },
        };

    let content_verdict = classify_content(
        &view,
        gpu_bytes.as_deref(),
        query.compare_bytes,
        &last_writer,
        gpu_unavail.as_deref(),
        earlier_snapshot.as_ref(),
    );

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

    let observed_placement = if d3d12_placement != "unknown" {
        format!("d3d12={d3d12_placement} (vidmm=unknown; pass --etl for live VidMm segment)")
    } else {
        "unknown".into()
    };

    DispatchProbeResult {
        dispatch: disp.clone(),
        resolved_view: view,
        last_writer,
        content_verdict,
        earlier_snapshot,
        d3d12_placement,
        observed_placement,
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

fn summarize_earlier_snapshot(bytes: &[u8], limit_bytes: usize, sample_point: &str) -> EarlierSnapshot {
    let n = bytes.len().min(limit_bytes);
    let mut nonzero = 0usize;
    let mut first_nz = None;
    for (i, &b) in bytes[..n].iter().enumerate() {
        if b != 0 {
            nonzero += 1;
            if first_nz.is_none() {
                first_nz = Some(i);
            }
        }
    }
    let hex_preview: String = bytes[..n]
        .iter()
        .take(32)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("");
    EarlierSnapshot {
        sample_point: sample_point.to_string(),
        inspected_bytes: n,
        nonzero_bytes: nonzero,
        first_nonzero_offset: first_nz,
        hex_preview,
    }
}

fn classify_content(
    view: &ResolvedView,
    gpu_bytes: Option<&[u8]>,
    limit_bytes: usize,
    last_writer: &LastWriterInfo,
    unavail: Option<&str>,
    earlier_snapshot: Option<&EarlierSnapshot>,
) -> ContentVerdict {
    if view.binding_status == "unmapped_sparse_tile" {
        return ContentVerdict {
            verdict: "unmapped_sparse_tile".into(),
            inspected_bytes: 0,
            nonzero_bytes: 0,
            first_nonzero_offset: None,
            summary: view.binding_reason.clone().unwrap_or_else(|| {
                "reserved buffer tiles covering this view are unmapped (GPU reads zero)".into()
            }),
        };
    }
    if view.binding_status != "bound" && view.binding_status != "partially_mapped_sparse_tile" {
        return ContentVerdict {
            verdict: "missing_binding".into(),
            inspected_bytes: 0,
            nonzero_bytes: 0,
            first_nonzero_offset: None,
            summary: format!(
                "{}: {}",
                view.binding_status,
                view.binding_reason
                    .as_deref()
                    .unwrap_or("binding did not resolve to a live resource")
            ),
        };
    }
    if let Some(bytes) = gpu_bytes
        && unavail.is_none()
        && last_writer.kind != "unrecorded_gpu_write"
    {
        let n = bytes.len().min(limit_bytes);
        let mut nonzero = 0usize;
        let mut first_nz = None;
        for (i, &b) in bytes[..n].iter().enumerate() {
            if b != 0 {
                nonzero += 1;
                if first_nz.is_none() {
                    first_nz = Some(i);
                }
            }
        }
        if nonzero == 0 {
            ContentVerdict {
                verdict: "legitimate_zero".into(),
                inspected_bytes: n,
                nonzero_bytes: 0,
                first_nonzero_offset: None,
                summary: format!(
                    "valid bound view ({n} B inspected) legitimately contains all zeros; supplied by {} ({})",
                    last_writer.kind, last_writer.detail
                ),
            }
        } else {
            ContentVerdict {
                verdict: "non_zero".into(),
                inspected_bytes: n,
                nonzero_bytes: nonzero,
                first_nonzero_offset: first_nz,
                summary: format!(
                    "valid bound view has {nonzero}/{n} non-zero bytes (first non-zero at +{:#x}); supplied by {} ({})",
                    first_nz.unwrap_or(0),
                    last_writer.kind,
                    last_writer.detail,
                ),
            }
        }
    } else {
        let earlier_note = earlier_snapshot
            .map(|es| {
                format!(
                    "; earlier snapshot [{}] had {}/{} non-zero bytes (hex={})",
                    es.sample_point, es.nonzero_bytes, es.inspected_bytes, es.hex_preview
                )
            })
            .unwrap_or_default();
        ContentVerdict {
            verdict: "bytes_unavailable".into(),
            inspected_bytes: 0,
            nonzero_bytes: 0,
            first_nonzero_offset: None,
            summary: format!(
                "valid bound view, but execution-time bytes are unavailable ({}); last writer: {} ({}){}",
                unavail.unwrap_or("not captured"),
                last_writer.kind,
                last_writer.detail,
                earlier_note,
            ),
        }
    }
}

fn diagnose_missing_binding(conn: &Connection, disp: &ComputeDispatchInfo, sel: &BindingSelector) -> String {
    let want = if let Some(p) = sel.parameter {
        format!("param:{p}")
    } else if let Some(ref k) = sel.kind {
        format!("{}:{} space={}", k, sel.slot.unwrap_or(0), sel.space.unwrap_or(0))
    } else {
        "any buffer binding".into()
    };

    let has_state: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='d12_draw_state'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if has_state
        && let Ok((g_root, c_root, root_args_s)) = conn.query_row(
            "SELECT graphics_root, compute_root, root_args_json FROM d12_draw_state WHERE draw=?1",
            params![disp.draw as i64],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?.map(|v| v as u64),
                    r.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                    r.get::<_, String>(2)?,
                ))
            },
        )
    {
        let root_id = if disp.stage == "cs" {
            c_root
        } else {
            g_root.or(c_root)
        };
        let Some(rid) = root_id else {
            return format!(
                "missing binding {want} at draw={} event={}: no root signature bound for stage {}",
                disp.draw, disp.event, disp.stage
            );
        };
        let args_v: Value = serde_json::from_str(&root_args_s).unwrap_or_default();
        let stage_args = if disp.stage == "cs" {
            &args_v["compute"]
        } else {
            &args_v["graphics"]
        };
        let bound_params: Vec<String> = stage_args
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(p) = sel.parameter {
            let pkey = p.to_string();
            if !bound_params.contains(&pkey) {
                return format!(
                    "missing binding {want} at draw={} event={}: root parameter {p} was never set on root signature #{rid} (bound params: [{}])",
                    disp.draw,
                    disp.event,
                    bound_params.join(", ")
                );
            }
        }
        return format!(
            "missing binding {want} at draw={} event={} (root #{rid}, bound params [{}], {} projected descriptors)",
            disp.draw,
            disp.event,
            bound_params.join(", "),
            disp.descriptors.len()
        );
    }

    format!(
        "no matching descriptor for {want} in d12_draw_descriptors at draw={} event={} ({} descriptors present)",
        disp.draw,
        disp.event,
        disp.descriptors.len()
    )
}

fn inspect_sparse_tiles(
    conn: &Connection,
    resource: u64,
    offset: u64,
    byte_len: u64,
    disp: &ComputeDispatchInfo,
) -> SparseTileCoverage {
    const TILE_SIZE: u64 = 65536;
    let first_tile = (offset / TILE_SIZE) as u32;
    let last_tile = (offset.saturating_add(byte_len.saturating_sub(1)) / TILE_SIZE) as u32;
    let tile_count = last_tile.saturating_sub(first_tile) + 1;

    let mut pages_json: Option<String> = None;
    let mut last_tile_mapping: Option<String> = None;
    let mut diagnostic: Option<String> = None;

    // 1. Check latest `d12_tile_mappings` at or before this event's submission `cpu_seq`.
    let has_tm: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='d12_tile_mappings'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if has_tm {
        let max_cpu = disp
            .submission_cpu_seq
            .and_then(|s| i64::try_from(s).ok())
            .unwrap_or(i64::MAX);
        if let Ok((id, queue, cpu_seq, call, pjson, diag)) = conn.query_row(
            "SELECT id, queue, cpu_seq, call, pages_json, diagnostic FROM d12_tile_mappings \
             WHERE resource=?1 AND cpu_seq <= ?2 ORDER BY cpu_seq DESC, id DESC LIMIT 1",
            params![resource as i64, max_cpu],
            |r| {
                Ok((
                    r.get::<_, i64>(0)? as u64,
                    r.get::<_, i64>(1)? as u64,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                ))
            },
        ) {
            last_tile_mapping = Some(format!("{call} (id={id}, queue=#{queue}, cpu_seq={cpu_seq})"));
            pages_json = pjson;
            diagnostic = diag;
        }
    }

    // 2. Fallback to initial `d12_tile_pages` captured at frame start / first use.
    if pages_json.is_none() && diagnostic.is_none() {
        let has_tp: bool = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='d12_tile_pages'",
                [],
                |_| Ok(true),
            )
            .unwrap_or(false);
        if has_tp
            && let Ok((pjson, diag)) = conn.query_row(
                "SELECT pages_json, diagnostic FROM d12_tile_pages WHERE resource=?1",
                params![resource as i64],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?)),
            )
        {
            pages_json = pjson;
            diagnostic = diag;
        } else {
            diagnostic = Some("no d12_tile_pages / d12_tile_mappings record for reserved resource".into());
        }
    }

    let mut mapped_tiles = 0u32;
    let mut unmapped_tiles = 0u32;
    let mut heaps = std::collections::BTreeSet::new();

    if let Some(ref pj) = pages_json
        && let Ok(val) = serde_json::from_str::<Value>(pj)
        && let Some(pages) = crate::tiles::Pages::from_runs(&val)
    {
        for t in first_tile..=last_tile {
            if let Some(&(heap, _page)) = pages.tiles.get(&t) {
                mapped_tiles += 1;
                heaps.insert(heap);
            } else {
                unmapped_tiles += 1;
            }
        }
    } else {
        unmapped_tiles = tile_count;
    }

    SparseTileCoverage {
        tile_size: TILE_SIZE,
        first_tile,
        tile_count,
        mapped_tiles,
        unmapped_tiles,
        backing_heaps: heaps.into_iter().collect(),
        last_tile_mapping,
        diagnostic,
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

    // Optional 2-level slot-index indirection: read u32 slot index at `raw_va`, then read the
    // guest view at `indirect_heap_va + slot_index * indirect_stride`!
    if let Some(heap_base) = layout.indirect_heap_va {
        let r1 = read_buffer_with_provenance(conn, blobs, live_buffers, buf, off, 4, disp);
        let (slot_opt, sp1, note1) = match (r1.bytes, r1.earlier_snapshot_bytes) {
            (Some(b), _) => (Some(b), r1.sample_point, r1.unavailable_reason),
            (None, Some(eb)) => (
                Some(eb),
                format!(
                    "{} [UNVERIFIED: {}]",
                    r1.earlier_snapshot_sample_point
                        .as_deref()
                        .unwrap_or("earlier-snapshot"),
                    r1.unavailable_reason
                        .as_deref()
                        .unwrap_or("tainted by earlier unrecorded write")
                ),
                r1.unavailable_reason,
            ),
            (None, None) => (None, r1.sample_point, r1.unavailable_reason),
        };
        let Some(slot_bytes) = slot_opt else {
            return (source_label, sp1, note1, None, None);
        };
        if slot_bytes.len() < 4 {
            return (
                source_label,
                sp1,
                Some("slot index read returned fewer than 4 bytes".into()),
                None,
                None,
            );
        }
        let slot_idx = u32::from_le_bytes(slot_bytes[0..4].try_into().unwrap());
        let view_va = heap_base.saturating_add((slot_idx as u64).saturating_mul(layout.indirect_stride));
        let hop_label = format!("{source_label} -> slot={slot_idx:#x} -> view_va={view_va:#x}");
        let Some((vbuf, voff)) = find_buffer(live_buffers, view_va) else {
            return (
                hop_label,
                sp1,
                Some(format!(
                    "indirect guest view address {view_va:#x} (heap {heap_base:#x} + {slot_idx:#x}*{}) is not inside any live ID3D12Resource buffer",
                    layout.indirect_stride
                )),
                Some(DecodedDescriptor {
                    slot_index: Some(slot_idx),
                    indirect_view_va: Some(view_va),
                    raw_hex: format!("{slot_idx:08x}"),
                    words_u32: vec![slot_idx],
                    gpu_va: None,
                    decoded_size: None,
                    decode_note: "indirect view address outside live buffers".into(),
                }),
                None,
            );
        };
        let view_len = layout.indirect_view_size.max(8) as u64;
        let r2 = read_buffer_with_provenance(conn, blobs, live_buffers, vbuf, voff, view_len, disp);
        let (view_opt, sp2, note2) = match (r2.bytes, r2.earlier_snapshot_bytes) {
            (Some(b), _) => (Some(b), r2.sample_point, r2.unavailable_reason),
            (None, Some(eb)) => (
                Some(eb),
                format!(
                    "{} [UNVERIFIED: {}]",
                    r2.earlier_snapshot_sample_point
                        .as_deref()
                        .unwrap_or("earlier-snapshot"),
                    r2.unavailable_reason
                        .as_deref()
                        .unwrap_or("tainted by earlier unrecorded write")
                ),
                r2.unavailable_reason,
            ),
            (None, None) => (None, r2.sample_point, r2.unavailable_reason),
        };
        return match view_opt {
            Some(vraw) => {
                let mut dec = decode_raw_descriptor(&vraw, layout, live_buffers);
                dec.slot_index = Some(slot_idx);
                dec.indirect_view_va = Some(view_va);
                let target = dec.gpu_va;
                (hop_label, sp2, note2.or(note1), Some(dec), target)
            }
            None => (hop_label, sp2, note2.or(note1), None, None),
        };
    }

    let r = read_buffer_with_provenance(
        conn,
        blobs,
        live_buffers,
        buf,
        off,
        layout.desc_size.max(8) as u64,
        disp,
    );
    let (raw_opt, sp, note) = match (r.bytes, r.earlier_snapshot_bytes) {
        (Some(b), _) => (Some(b), r.sample_point, r.unavailable_reason),
        (None, Some(eb)) => (
            Some(eb),
            format!(
                "{} [UNVERIFIED: {}]",
                r.earlier_snapshot_sample_point
                    .as_deref()
                    .unwrap_or("earlier-snapshot"),
                r.unavailable_reason
                    .as_deref()
                    .unwrap_or("tainted by earlier unrecorded write")
            ),
            r.unavailable_reason,
        ),
        (None, None) => (None, r.sample_point, r.unavailable_reason),
    };
    match raw_opt {
        Some(raw) => {
            let dec = decode_raw_descriptor(&raw, layout, live_buffers);
            let target = dec.gpu_va;
            (source_label, sp, note, Some(dec), target)
        }
        None => (source_label, sp, note, None, None),
    }
}

/// Result of reading a buffer slice with write-provenance tracking.
pub struct BufferReadResult {
    /// Verified bytes at execution time (`None` if missing or invalidated by an overlapping unrecorded write).
    pub bytes: Option<Vec<u8>>,
    /// Sample point of `bytes` (or `"unavailable..."`).
    pub sample_point: String,
    /// Reason why execution-time bytes are unavailable.
    pub unavailable_reason: Option<String>,
    /// Earlier pre-write snapshot bytes preserved separately when a subsequent unrecorded write tainted the slice.
    pub earlier_snapshot_bytes: Option<Vec<u8>>,
    /// Sample point of `earlier_snapshot_bytes`.
    pub earlier_snapshot_sample_point: Option<String>,
    /// Last writer info.
    pub last_writer: LastWriterInfo,
}

/// Reads `[offset, offset + len)` of `buf` as of `disp.event` (legacy wrapper returning `(bytes, sample_point, note)`).
pub fn read_buffer_at_dispatch(
    conn: &Connection,
    blobs: &Blobs,
    live_buffers: &[LiveBuffer],
    buf: &LiveBuffer,
    offset: u64,
    len: u64,
    disp: &ComputeDispatchInfo,
) -> (Option<Vec<u8>>, String, Option<String>) {
    let r = read_buffer_with_provenance(conn, blobs, live_buffers, buf, offset, len, disp);
    (r.bytes, r.sample_point, r.unavailable_reason)
}

/// Looks up the latest `d12_upload_versions` run for `(resource, submission <= max_sub)` covering `[offset, offset + len)`.
fn find_upload_version(
    conn: &Connection,
    blobs: &Blobs,
    resource: u64,
    max_sub: Option<u64>,
    offset: u64,
    len: u64,
) -> Option<(u64, u64, u64, String, Vec<u8>)> {
    let has_uv: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='d12_upload_versions'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has_uv {
        return None;
    }
    let sub_cap = max_sub.and_then(|s| i64::try_from(s).ok()).unwrap_or(i64::MAX);
    let mut stmt = conn
        .prepare(
            "SELECT submission, offset, content_json FROM d12_upload_versions \
             WHERE resource=?1 AND submission <= ?2 ORDER BY submission DESC, offset DESC",
        )
        .ok()?;
    let rows = stmt
        .query_map(params![resource as i64, sub_cap], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, String>(2)?,
            ))
        })
        .ok()?;
    for (sub, run_off, json_s) in rows.flatten() {
        if let Ok(v) = serde_json::from_str::<Value>(&json_s)
            && let (Some(sz), Some(key)) = (v["size"].as_u64(), v["blob"].as_str())
            && offset >= run_off
            && offset.saturating_add(len) <= run_off.saturating_add(sz)
            && let Ok(bytes) = blobs.read(key, offset - run_off, Some(len))
        {
            return Some((sub, run_off, sz, key.to_string(), bytes));
        }
    }
    None
}

fn event_submission(conn: &Connection, event: u64) -> Option<u64> {
    conn.query_row(
        "SELECT x.submission FROM d12_event_locations l JOIN d12_executions x ON x.id = l.execution WHERE l.event = ?1",
        params![event as i64],
        |r| Ok(r.get::<_, i64>(0)? as u64),
    )
    .ok()
}

/// Reads `[offset, offset + len)` of `buf` as of `disp.event`, returning verified execution-time bytes
/// (or `None` + separate earlier snapshot when invalidated by a potentially overlapping unrecorded write).
pub fn read_buffer_with_provenance(
    conn: &Connection,
    blobs: &Blobs,
    live_buffers: &[LiveBuffer],
    buf: &LiveBuffer,
    offset: u64,
    len: u64,
    disp: &ComputeDispatchInfo,
) -> BufferReadResult {
    // Track the latest UAV `results` row for this resource strictly before `disp.event`.
    let mut latest_uav: Option<(u64, String, u64, u64, String, Vec<u8>)> = None;
    let has_results: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='results'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if has_results
        && let Ok(mut stmt) = conn.prepare(
            "SELECT r.event, r.content, r.offset, r.bytes, COALESCE(e.call, 'UAV') \
             FROM results r LEFT JOIN events e ON e.seq = r.event \
             WHERE r.resource=?1 AND r.event < ?2 ORDER BY r.event DESC",
        )
        && let Ok(rows) = stmt.query_map(params![buf.resource as i64, disp.event as i64], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                r.get::<_, String>(4)?,
            ))
        })
    {
        for (ev, key, res_off, res_bytes, call) in rows.flatten() {
            let base = res_off.unwrap_or(0);
            let avail = res_bytes.unwrap_or(buf.width);
            if offset >= base
                && offset.saturating_add(len) <= base.saturating_add(avail)
                && let Ok(bytes) = blobs.read(&key, offset - base, Some(len))
            {
                latest_uav = Some((ev, key, base, avail, call, bytes));
                break;
            }
        }
    }

    // Start from `d12_upload_versions` (for upload buffers) or `first-use` initial capture as the pre-event base.
    let mut base_bytes: Option<Vec<u8>> = None;
    let mut sample_point = String::new();
    let mut missing_reason: Option<String> = None;
    let mut last_writer = LastWriterInfo {
        kind: "none".into(),
        event: None,
        submission: None,
        call: None,
        source_resource: None,
        source_offset: None,
        byte_range: Some((offset, offset.saturating_add(len))),
        upload_submission: None,
        blob: None,
        detail: "no prior write or initial content captured".into(),
    };

    if let Some((uv_sub, run_off, run_sz, key, bytes)) =
        find_upload_version(conn, blobs, buf.resource, disp.submission, offset, len)
    {
        base_bytes = Some(bytes);
        sample_point = format!("before-dispatch (upload run at submission {uv_sub})");
        last_writer = LastWriterInfo {
            kind: "upload_version".into(),
            event: None,
            submission: Some(uv_sub),
            call: Some("Map/Unmap upload".into()),
            source_resource: Some(buf.resource),
            source_offset: Some(offset),
            byte_range: Some((run_off, run_off.saturating_add(run_sz))),
            upload_submission: Some(uv_sub),
            blob: Some(key),
            detail: format!(
                "d12_upload_versions submission={uv_sub} resource=#{} run=[{run_off:#x}, {:#x})",
                buf.resource,
                run_off.saturating_add(run_sz)
            ),
        };
    } else if let Some(ref key) = buf.content {
        let fu_sub: Option<u64> = conn
            .query_row(
                "SELECT submission FROM d12_first_use WHERE resource=?1",
                params![buf.resource as i64],
                |r| Ok(r.get::<_, i64>(0)? as u64),
            )
            .ok();
        match blobs.read(key, offset, Some(len)) {
            Ok(b) => {
                let src_lbl = buf.content_source.as_deref().unwrap_or("first-use");
                base_bytes = Some(b);
                sample_point = format!("before-dispatch ({src_lbl})");
                last_writer = LastWriterInfo {
                    kind: "first_use_initial".into(),
                    event: None,
                    submission: fu_sub,
                    call: None,
                    source_resource: Some(buf.resource),
                    source_offset: Some(offset),
                    byte_range: Some((offset, offset.saturating_add(len))),
                    upload_submission: None,
                    blob: Some(key.clone()),
                    detail: format!(
                        "no in-frame writer before event {}; initial {src_lbl} snapshot of resource #{}{}",
                        disp.event,
                        buf.resource,
                        fu_sub
                            .map(|s| format!(" (first_use sub={s})"))
                            .unwrap_or_default()
                    ),
                };
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

    // Apply in-frame writes (`events` + `event_resources` WHERE `access='write'` AND `seq < disp.event`) in exact chronological order!
    let mut copy_count = 0usize;
    let mut unfollowed_write: Option<String> = None;
    let mut earlier_snapshot_bytes: Option<Vec<u8>> = None;
    let mut earlier_snapshot_sample_point: Option<String> = None;
    let uav_ev = latest_uav.as_ref().map(|u| u.0).unwrap_or(0);

    let taint = |why: String,
                 base_bytes: &mut Option<Vec<u8>>,
                 sample_point: &str,
                 earlier_b: &mut Option<Vec<u8>>,
                 earlier_sp: &mut Option<String>,
                 unfollowed: &mut Option<String>| {
        if let Some(b) = base_bytes.take() {
            *earlier_b = Some(b);
            *earlier_sp = Some(sample_point.to_string());
        }
        *unfollowed = Some(why);
    };

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
            // If this event is our captured UAV `results` event, apply it in chronological order!
            if seq == uav_ev
                && let Some((ev, ref key, base, avail, ref ucall, ref ubytes)) = latest_uav
            {
                base_bytes = Some(ubytes.clone());
                unfollowed_write = None;
                earlier_snapshot_bytes = None;
                earlier_snapshot_sample_point = None;
                sample_point = format!("before-dispatch (from UAV result at event {ev})");
                last_writer = LastWriterInfo {
                    kind: "uav_result".into(),
                    event: Some(ev),
                    submission: event_submission(conn, ev),
                    call: Some(ucall.clone()),
                    source_resource: Some(buf.resource),
                    source_offset: Some(offset),
                    byte_range: Some((base, base.saturating_add(avail))),
                    upload_submission: None,
                    blob: Some(key.clone()),
                    detail: format!(
                        "{ucall} at event {ev} wrote UAV resource #{} range [{base:#x}, {:#x})",
                        buf.resource,
                        base.saturating_add(avail)
                    ),
                };
                continue;
            }

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
                if lo < hi {
                    let copy_sub = event_submission(conn, seq).or(disp.submission);
                    let src_read_off = src_off.saturating_add(lo - dst_off);
                    let src_read_len = hi - lo;
                    let mut got_src: Option<(Vec<u8>, Option<u64>, Option<String>)> = None;

                    if let Some(sid) = src_id {
                        // Prefer source's submission-scoped `d12_upload_versions` (critical for UPLOAD -> DEFAULT copies!)
                        if let Some((uv_sub, _, _, key, bytes)) =
                            find_upload_version(conn, blobs, sid, copy_sub, src_read_off, src_read_len)
                        {
                            got_src = Some((bytes, Some(uv_sub), Some(key)));
                        } else if let Some(src_buf) = live_buffers.iter().find(|b| b.resource == sid)
                            && let Some(ref skey) = src_buf.content
                            && let Ok(bytes) = blobs.read(skey, src_read_off, Some(src_read_len))
                        {
                            got_src = Some((bytes, None, Some(skey.clone())));
                        }
                    }

                    let kind = if call == "CopyBufferRegion" {
                        "copy_buffer_region"
                    } else {
                        "copy_resource"
                    };
                    if let Some((src_bytes, uv_sub, src_blob)) = got_src {
                        let covers_full = lo == offset && hi == offset.saturating_add(len);
                        if covers_full || unfollowed_write.is_none() {
                            let dst = base_bytes.get_or_insert_with(|| vec![0u8; len as usize]);
                            let d_lo = (lo - offset) as usize;
                            let d_hi = (d_lo + src_bytes.len()).min(dst.len());
                            dst[d_lo..d_hi].copy_from_slice(&src_bytes[..d_hi - d_lo]);
                            copy_count += 1;
                            if covers_full {
                                unfollowed_write = None;
                                earlier_snapshot_bytes = None;
                                earlier_snapshot_sample_point = None;
                            }
                        }
                        last_writer = LastWriterInfo {
                            kind: kind.into(),
                            event: Some(seq),
                            submission: copy_sub,
                            call: Some(call.clone()),
                            source_resource: src_id,
                            source_offset: Some(src_read_off),
                            byte_range: Some((lo, hi)),
                            upload_submission: uv_sub,
                            blob: src_blob,
                            detail: format!(
                                "{call} at event {seq} copied [{src_read_off:#x}, {:#x}) from resource #{:?} into [{lo:#x}, {hi:#x}){}",
                                src_read_off.saturating_add(src_read_len),
                                src_id.unwrap_or(0),
                                uv_sub
                                    .map(|s| format!(" (from upload sub={s})"))
                                    .unwrap_or_default(),
                            ),
                        };
                    } else {
                        let why = format!(
                            "{call} at event {seq} wrote [{lo:#x}, {hi:#x}) from resource #{:?} whose source bytes were not captured",
                            src_id.unwrap_or(0)
                        );
                        taint(
                            why.clone(),
                            &mut base_bytes,
                            &sample_point,
                            &mut earlier_snapshot_bytes,
                            &mut earlier_snapshot_sample_point,
                            &mut unfollowed_write,
                        );
                        last_writer = LastWriterInfo {
                            kind: kind.into(),
                            event: Some(seq),
                            submission: copy_sub,
                            call: Some(call.clone()),
                            source_resource: src_id,
                            source_offset: Some(src_read_off),
                            byte_range: Some((lo, hi)),
                            upload_submission: None,
                            blob: None,
                            detail: why,
                        };
                    }
                }
            } else if call.starts_with("ClearUnorderedAccessView") {
                let copy_sub = event_submission(conn, seq);
                let why = format!("{call} at event {seq} cleared resource #{}", buf.resource);
                taint(
                    why.clone(),
                    &mut base_bytes,
                    &sample_point,
                    &mut earlier_snapshot_bytes,
                    &mut earlier_snapshot_sample_point,
                    &mut unfollowed_write,
                );
                last_writer = LastWriterInfo {
                    kind: "clear_uav".into(),
                    event: Some(seq),
                    submission: copy_sub,
                    call: Some(call.clone()),
                    source_resource: None,
                    source_offset: None,
                    byte_range: Some((offset, offset.saturating_add(len))),
                    upload_submission: None,
                    blob: None,
                    detail: why,
                };
            } else if seq > uav_ev {
                // Check whether the writer event's bound UAV views on `buf.resource` are strictly disjoint
                // from `[buf.gpu_address + offset, buf.gpu_address + offset + len)`!
                if writer_uav_views_are_disjoint(conn, seq, buf.resource, buf.gpu_address + offset, len) {
                    continue;
                }
                let copy_sub = event_submission(conn, seq);
                let why = format!(
                    "written earlier by {call} at event {seq} without a captured results row covering [{offset:#x}, {:#x})",
                    offset.saturating_add(len)
                );
                taint(
                    why.clone(),
                    &mut base_bytes,
                    &sample_point,
                    &mut earlier_snapshot_bytes,
                    &mut earlier_snapshot_sample_point,
                    &mut unfollowed_write,
                );
                last_writer = LastWriterInfo {
                    kind: "unrecorded_gpu_write".into(),
                    event: Some(seq),
                    submission: copy_sub,
                    call: Some(call.clone()),
                    source_resource: None,
                    source_offset: None,
                    byte_range: Some((offset, offset.saturating_add(len))),
                    upload_submission: None,
                    blob: None,
                    detail: why,
                };
            }
        }
    }

    // If `results` had a UAV capture that wasn't listed in `event_resources` (e.g. ExecuteIndirect), still apply it!
    if let Some((ev, key, base, avail, ucall, ubytes)) = latest_uav
        && last_writer.event.is_none_or(|prev| ev > prev)
    {
        base_bytes = Some(ubytes);
        unfollowed_write = None;
        earlier_snapshot_bytes = None;
        earlier_snapshot_sample_point = None;
        sample_point = format!("before-dispatch (from UAV result at event {ev})");
        last_writer = LastWriterInfo {
            kind: "uav_result".into(),
            event: Some(ev),
            submission: event_submission(conn, ev),
            call: Some(ucall.clone()),
            source_resource: Some(buf.resource),
            source_offset: Some(offset),
            byte_range: Some((base, base.saturating_add(avail))),
            upload_submission: None,
            blob: Some(key),
            detail: format!(
                "{ucall} at event {ev} wrote UAV resource #{} range [{base:#x}, {:#x})",
                buf.resource,
                base.saturating_add(avail)
            ),
        };
    }

    if copy_count > 0 && last_writer.kind.starts_with("copy_") && unfollowed_write.is_none() {
        sample_point = format!(
            "before-dispatch (composed over {copy_count} copy op(s); last at event {})",
            last_writer.event.unwrap_or(0)
        );
    }
    if unfollowed_write.is_some() {
        BufferReadResult {
            bytes: None,
            sample_point: "unavailable (tainted by earlier unrecorded write)".into(),
            unavailable_reason: unfollowed_write,
            earlier_snapshot_bytes: earlier_snapshot_bytes.or(base_bytes),
            earlier_snapshot_sample_point,
            last_writer,
        }
    } else if base_bytes.is_some() {
        BufferReadResult {
            bytes: base_bytes,
            sample_point,
            unavailable_reason: None,
            earlier_snapshot_bytes: None,
            earlier_snapshot_sample_point: None,
            last_writer,
        }
    } else {
        BufferReadResult {
            bytes: None,
            sample_point: "unavailable".into(),
            unavailable_reason: missing_reason,
            earlier_snapshot_bytes: None,
            earlier_snapshot_sample_point: None,
            last_writer,
        }
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
    // Auto-select: first valid buffer SRV or UAV bound.
    descriptors
        .iter()
        .find(|d| d.flag.is_none() && d.address.is_some() && matches!(d.kind.as_str(), "srv" | "uav"))
        .or_else(|| descriptors.iter().find(|d| d.address.is_some()))
}

type ResourceContentMeta = (Option<String>, Option<String>, Option<String>);

fn load_live_buffers_and_heaps(conn: &Connection) -> (Vec<LiveBuffer>, BTreeMap<u64, HeapMeta>) {
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

    let mut heaps = BTreeMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id, description FROM d12_objects WHERE kind='ID3D12Heap'")
        && let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?)))
    {
        for (id, desc_s) in rows.flatten() {
            if let Ok(v) = serde_json::from_str::<Value>(&desc_s) {
                let hp = &v["heap_properties"];
                heaps.insert(
                    id,
                    HeapMeta {
                        id,
                        bytes: v["bytes"].as_u64().unwrap_or(0),
                        heap_type: hp["type"].as_u64(),
                        cpu_page: hp["cpu_page"].as_u64(),
                        memory_pool: hp["memory_pool"].as_u64(),
                    },
                );
            }
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
                let heap = v["heap"].as_u64().filter(|&h| h != 0);
                let heap_offset = heap.and_then(|_| v["heap_offset"].as_u64());
                let hp = &v["heap_properties"];
                let heap_type = hp["type"].as_u64();
                let cpu_page = hp["cpu_page"].as_u64();
                let memory_pool = hp["memory_pool"].as_u64();
                let reserved = v
                    .as_object()
                    .and_then(|m| m.get("heap_properties"))
                    .is_some_and(Value::is_null)
                    && heap.is_none();
                out.push(LiveBuffer {
                    resource: id,
                    gpu_address: va,
                    width: w,
                    content,
                    content_source,
                    content_note,
                    heap,
                    heap_offset,
                    heap_type,
                    cpu_page,
                    memory_pool,
                    reserved,
                });
            }
        }
    }
    (out, heaps)
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

/// Returns true only when `writer_event` is a draw/dispatch with explicit UAV descriptor ranges on
/// `resource` and EVERY such UAV view is strictly disjoint from `[want_va, want_va + want_len)`.
fn writer_uav_views_are_disjoint(
    conn: &Connection,
    writer_event: u64,
    resource: u64,
    want_va: u64,
    want_len: u64,
) -> bool {
    let Ok(draw_n) = conn.query_row(
        "SELECT n FROM draws WHERE event = ?1",
        params![writer_event as i64],
        |r| Ok(r.get::<_, i64>(0)? as u64),
    ) else {
        return false;
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT address, size FROM d12_draw_descriptors WHERE draw = ?1 AND resource = ?2 AND kind = 'uav'",
    ) else {
        return false;
    };
    let Ok(rows) = stmt.query_map(params![draw_n as i64, resource as i64], |r| {
        Ok((
            r.get::<_, Option<i64>>(0)?.map(|v| v as u64),
            r.get::<_, Option<i64>>(1)?.map(|v| v as u64),
        ))
    }) else {
        return false;
    };
    let want_end = want_va.saturating_add(want_len);
    let mut saw_any = false;
    for row in rows.flatten() {
        saw_any = true;
        match row {
            (Some(addr), Some(sz)) if sz > 0 => {
                let end = addr.saturating_add(sz);
                if want_va < end && addr < want_end {
                    return false; // overlaps!
                }
            }
            _ => return false, // unknown or unbounded UAV view -> conservatively overlaps
        }
    }
    saw_any
}

fn find_matching_events(
    conn: &Connection,
    shader_filter: Option<&str>,
    shader_aliases: &[String],
    draw_filter: Option<u64>,
    event_filter: Option<u64>,
    stage_filter: Option<&str>,
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
        .prepare("SELECT n, event, call, args, vs, hs, ds, gs, ps, cs FROM draws ORDER BY n")
        .map_err(|e| format!("query draws: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                r.get::<_, i64>(1)? as u64,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                [
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<String>>(8)?,
                    r.get::<_, Option<String>>(9)?,
                ],
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

    let stage_names = ["vs", "hs", "ds", "gs", "ps", "cs"];
    let want_stage = stage_filter.map(|s| s.trim().to_ascii_lowercase());
    let filter_lc = shader_filter.map(|s| s.trim().to_ascii_lowercase());
    let mut out = Vec::new();

    for row in rows.flatten() {
        let (n, event, call, args, shaders) = row;
        if let Some(want_ev) = event_filter
            && event != want_ev
        {
            continue;
        }
        if let Some(want_draw) = draw_filter
            && n != want_draw
        {
            continue;
        }

        // Determine active stage & shader hash for this row.
        let is_compute = shaders[5].is_some();
        let (active_stage, active_hash) = if let Some(ref st) = want_stage {
            if st == "all" {
                if let Some(ref cs) = shaders[5] {
                    ("cs".to_string(), cs.clone())
                } else if let Some((idx, sh)) = shaders
                    .iter()
                    .enumerate()
                    .find_map(|(i, s)| s.as_ref().map(|h| (i, h)))
                {
                    (stage_names[idx].to_string(), sh.clone())
                } else {
                    ("cs".to_string(), String::new())
                }
            } else {
                let idx = stage_names.iter().position(|&s| s == st.as_str());
                match idx.and_then(|i| shaders[i].as_ref()) {
                    Some(h) => (st.clone(), h.clone()),
                    None if event_filter.is_some() || draw_filter.is_some() => (st.clone(), String::new()),
                    None => continue,
                }
            }
        } else if is_compute {
            ("cs".to_string(), shaders[5].clone().unwrap_or_default())
        } else if event_filter.is_some() || draw_filter.is_some() {
            // Graphics draw explicitly selected by `--event` or `--draw`!
            if let Some(ref flt) = filter_lc {
                if let Some((idx, sh)) = shaders.iter().enumerate().find_map(|(i, s)| {
                    let h = s.as_ref()?;
                    let nm = shader_names.get(h).and_then(|o| o.as_deref());
                    if h.to_ascii_lowercase().contains(flt)
                        || nm.is_some_and(|x| x.to_ascii_lowercase().contains(flt))
                    {
                        Some((i, h))
                    } else {
                        None
                    }
                }) {
                    (stage_names[idx].to_string(), sh.clone())
                } else {
                    continue;
                }
            } else if let Some((idx, sh)) = shaders
                .iter()
                .enumerate()
                .find_map(|(i, s)| s.as_ref().map(|h| (i, h)))
            {
                (stage_names[idx].to_string(), sh.clone())
            } else {
                ("vs".to_string(), String::new())
            }
        } else {
            // Default scan without --event/--draw/--stage targets compute dispatches (preserves original behavior).
            continue;
        };

        let cs_name = shader_names.get(&active_hash).cloned().flatten();
        if let Some(ref flt) = filter_lc {
            let active_lc = active_hash.to_ascii_lowercase();
            let hash_match = active_lc.contains(flt)
                || shader_aliases
                    .iter()
                    .any(|a| !a.is_empty() && active_lc.contains(a));
            let name_match = cs_name
                .as_deref()
                .is_some_and(|nm| nm.to_ascii_lowercase().contains(flt));
            if !hash_match && !name_match {
                continue;
            }
        }

        let (submission, submission_cpu_seq) = if has_locs {
            conn.query_row(
                "SELECT x.submission, s.cpu_seq FROM d12_event_locations l \
                 JOIN d12_executions x ON x.id = l.execution \
                 LEFT JOIN d12_submissions s ON s.id = x.submission \
                 WHERE l.event = ?1",
                params![event as i64],
                |r| {
                    Ok((
                        Some(r.get::<_, i64>(0)? as u64),
                        r.get::<_, Option<i64>>(1)?.map(|v| v as u64),
                    ))
                },
            )
            .unwrap_or((None, None))
        } else {
            (None, None)
        };

        let mut descriptors = Vec::new();
        if has_desc {
            let load_all_stages =
                want_stage.as_deref() == Some("all") || (!is_compute && want_stage.is_none());
            let sql = if load_all_stages {
                "SELECT stage, kind, slot, space, parameter, heap, heap_slot, version, \
                 resource, address, size, stride, format, dimension, flag \
                 FROM d12_draw_descriptors WHERE draw=?1 ORDER BY stage, kind, space, slot"
            } else {
                "SELECT stage, kind, slot, space, parameter, heap, heap_slot, version, \
                 resource, address, size, stride, format, dimension, flag \
                 FROM d12_draw_descriptors WHERE draw=?1 AND stage=?2 ORDER BY kind, space, slot"
            };
            if let Ok(mut ds) = conn.prepare(sql) {
                let mapper = |r: &rusqlite::Row<'_>| {
                    Ok(ComputeDescriptorRow {
                        stage: r.get(0)?,
                        kind: r.get(1)?,
                        slot: r.get::<_, i64>(2)? as u32,
                        space: r.get::<_, i64>(3)? as u32,
                        parameter: r.get::<_, i64>(4)? as u32,
                        heap: r.get::<_, Option<i64>>(5)?.map(|v| v as u64),
                        heap_slot: r.get::<_, Option<i64>>(6)?.map(|v| v as u32),
                        version: r.get::<_, Option<i64>>(7)?.map(|v| v as u64),
                        resource: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                        address: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
                        size: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
                        stride: r.get::<_, Option<i64>>(11)?.map(|v| v as u32),
                        format: r.get::<_, Option<i64>>(12)?.map(|v| v as u32),
                        dimension: r.get::<_, Option<i64>>(13)?.map(|v| v as u32),
                        flag: r.get(14)?,
                    })
                };
                if load_all_stages {
                    if let Ok(drows) = ds.query_map(params![n as i64], mapper) {
                        descriptors.extend(drows.flatten());
                    }
                } else if let Ok(drows) = ds.query_map(params![n as i64, active_stage.as_str()], mapper) {
                    descriptors.extend(drows.flatten());
                }
            }
        }

        out.push(ComputeDispatchInfo {
            draw: n,
            event,
            call,
            args,
            cs_hash: active_hash,
            cs_name,
            stage: active_stage,
            submission,
            submission_cpu_seq,
            descriptors,
        });
        if limit > 0 && out.len() >= limit {
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
            shader_aliases: Vec::new(),
            draw: None,
            event: None,
            stage: None,
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
        assert_eq!(r.resolved_view.binding_status, "bound");
        assert_eq!(r.content_verdict.verdict, "non_zero");
        assert_eq!(r.last_writer.kind, "first_use_initial");
        let cmp = r.comparison.as_ref().expect("comparison computed");
        assert_eq!(cmp.status, "differs");
        assert_eq!(cmp.compared_bytes, 4096);
        assert_eq!(cmp.mismatch_count, 1);
        assert_eq!(cmp.first_mismatch_offset, Some(0x10));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn distinguishes_missing_binding_unmapped_sparse_tile_and_legitimate_zero_from_copy_upload() {
        let dir = std::env::temp_dir().join(format!("locum_probe_batman_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blobs = Blobs::new(dir.join("blobs"));

        // Resource 20: UPLOAD buffer (type=2, L0_SYS) containing legitimate all-zero 4 KiB bytes in upload_versions.
        let zeros = vec![0u8; 4096];
        let zero_key = hash::key(&zeros);
        blobs.put(&zero_key, &zeros).unwrap();

        // Resource 21: DEFAULT buffer (type=1, L1_VRAM) populated by CopyBufferRegion at event 10 from upload #20.
        // Resource 22: RESERVED sparse buffer (heap_properties: null) with tile 0 mapped to heap #5 and tile 1 UNMAPPED.
        let mut cap = db::Capture::default();
        cap.meta("api", "d3d12");
        cap.meta("blobs", dir.join("blobs").display());
        cap.shaders.push(db::Shader {
            hash: "cull0001".into(),
            stage: "cs",
            blob: Some("cull0001".into()),
            size: 64,
            name: Some("g10_cull_cs".into()),
        });
        cap.resources.push(db::Resource {
            id: 20,
            kind: "buffer",
            width: 4096,
            bytes: 4096,
            used: true,
            ..Default::default()
        });
        cap.resources.push(db::Resource {
            id: 21,
            kind: "buffer",
            width: 4096,
            bytes: 4096,
            used: true,
            ..Default::default()
        });
        cap.resources.push(db::Resource {
            id: 22,
            kind: "buffer",
            width: 131_072,
            bytes: 131_072,
            used: true,
            ..Default::default()
        });
        // Event 10: CopyBufferRegion(dst=#21, dst_off=0, src=#20, src_off=0, bytes=4096)
        cap.events.push(db::Event {
            seq: 10,
            kind: "copy",
            call: "CopyBufferRegion",
            args: json!([21, 0, 20, 0, 4096]).to_string(),
            reads: vec![20],
            writes: vec![21],
            ..Default::default()
        });
        // Event 11 (draw 1): Dispatch reading srv:0 (#21 legit zero), srv:1 (#22 tile 1 unmapped), srv:2 (empty slot), srv:3 (missing)
        cap.events.push(db::Event {
            seq: 11,
            kind: "dispatch",
            call: "Dispatch",
            draw: Some(1),
            args: "[64,1,1]".into(),
            reads: vec![21, 22],
            ..Default::default()
        });
        cap.draws.push(db::Draw {
            n: 1,
            event: 11,
            call: "Dispatch",
            args: "[64,1,1]".into(),
            shaders: [None, None, None, None, None, Some("cull0001".into())],
            ..Default::default()
        });
        // srv:0 -> DEFAULT buffer #21 (offset 0, 4096 B) -> legitimately zero via CopyBufferRegion <- upload #20!
        cap.draw_descriptors.push(db::DrawDescriptor {
            draw: 1,
            stage: "cs",
            kind: "srv",
            slot: 0,
            space: 0,
            parameter: 0,
            resource: Some(21),
            address: Some(0x4000_0000),
            size: Some(4096),
            ..Default::default()
        });
        // srv:1 -> RESERVED buffer #22 at offset 0x10000 (tile 1, which is UNMAPPED!)
        cap.draw_descriptors.push(db::DrawDescriptor {
            draw: 1,
            stage: "cs",
            kind: "srv",
            slot: 1,
            space: 0,
            parameter: 0,
            resource: Some(22),
            address: Some(0x5001_0000),
            size: Some(4096),
            ..Default::default()
        });
        // srv:2 -> empty descriptor slot in heap #9 slot 7
        cap.draw_descriptors.push(db::DrawDescriptor {
            draw: 1,
            stage: "cs",
            kind: "srv",
            slot: 2,
            space: 0,
            parameter: 0,
            heap: Some(9),
            heap_slot: Some(7),
            flag: Some("empty"),
            ..Default::default()
        });

        let frame = crate::d3d12::Frame {
            objects: vec![
                crate::d3d12::Object {
                    id: 5,
                    kind: "ID3D12Heap",
                    device: 1,
                    description: json!({
                        "bytes": 65536,
                        "heap_properties": {"type": 1, "cpu_page": 1, "memory_pool": 2}
                    }),
                },
                crate::d3d12::Object {
                    id: 20,
                    kind: "ID3D12Resource",
                    device: 1,
                    description: json!({
                        "dimension": 1, "width": 4096, "gpu_address": 0x3000_0000u64,
                        "heap": 0, "heap_offset": 0,
                        "heap_properties": {"type": 2, "cpu_page": 2, "memory_pool": 1}
                    }),
                },
                crate::d3d12::Object {
                    id: 21,
                    kind: "ID3D12Resource",
                    device: 1,
                    description: json!({
                        "dimension": 1, "width": 4096, "gpu_address": 0x4000_0000u64,
                        "heap": 0, "heap_offset": 0,
                        "heap_properties": {"type": 1, "cpu_page": 1, "memory_pool": 2}
                    }),
                },
                crate::d3d12::Object {
                    id: 22,
                    kind: "ID3D12Resource",
                    device: 1,
                    description: json!({
                        "dimension": 1, "width": 131_072, "gpu_address": 0x5000_0000u64,
                        "heap": 0, "heap_offset": 0,
                        "heap_properties": null
                    }),
                },
            ],
            submissions: vec![crate::d3d12::Submission {
                id: 1,
                queue: 100,
                queue_seq: 1,
                cpu_seq: 50,
                lists: Vec::new(),
                descriptors: Vec::new(),
                uploads: vec![(20, json!({"offset": 0, "size": 4096, "blob": zero_key}))],
            }],
            tile_pages: vec![crate::d3d12::TilePages {
                resource: 22,
                cpu_seq: 10,
                // Only tile 0 mapped to heap #5; tile 1 (0x10000..0x20000) is unmapped!
                pages: Ok((json!({}), json!([[0, 1, 5, 0]]))),
            }],
            ..Default::default()
        };
        let proj = crate::d3d12::Projection::new(&frame.objects);
        cap.d3d12 = Some(frame.finish(&mut db::Capture::default(), proj));

        let db_path = dir.join("capture.db");
        db::write(&db_path, &cap).unwrap();

        // 1. srv:0 at event 11 -> legitimate_zero supplied by CopyBufferRegion from upload #20, placed in DEFAULT/L1_VRAM.
        let q0 = ProbeQuery {
            event: Some(11),
            binding: BindingSelector::parse("srv:0").unwrap(),
            follow_descriptor: false,
            compare_bytes: 4096,
            ..Default::default()
        };
        let r0 = &probe_capture::<fn(u64, usize) -> Result<Vec<u8>, String>>(
            &db_path,
            &q0,
            &AliasMap::default(),
            None,
        )
        .unwrap()[0];
        assert_eq!(r0.resolved_view.binding_status, "bound");
        assert_eq!(r0.resolved_view.resource, Some(21));
        assert_eq!(r0.resolved_view.byte_offset, Some(0));
        assert_eq!(r0.resolved_view.byte_length, Some(4096));
        assert_eq!(r0.last_writer.kind, "copy_buffer_region");
        assert_eq!(r0.last_writer.event, Some(10));
        assert_eq!(r0.last_writer.source_resource, Some(20));
        assert_eq!(r0.last_writer.upload_submission, Some(1));
        assert_eq!(r0.content_verdict.verdict, "legitimate_zero");
        assert_eq!(r0.d3d12_placement, "committed(DEFAULT/L1_VRAM/CPU_NONE)");

        // 2. srv:1 at event 11 -> unmapped_sparse_tile (reserved #22 tile 1 unmapped).
        let q1 = ProbeQuery {
            event: Some(11),
            binding: BindingSelector::parse("srv:1").unwrap(),
            follow_descriptor: false,
            compare_bytes: 4096,
            ..Default::default()
        };
        let r1 = &probe_capture::<fn(u64, usize) -> Result<Vec<u8>, String>>(
            &db_path,
            &q1,
            &AliasMap::default(),
            None,
        )
        .unwrap()[0];
        assert_eq!(r1.resolved_view.binding_status, "unmapped_sparse_tile");
        assert_eq!(r1.content_verdict.verdict, "unmapped_sparse_tile");
        assert_eq!(r1.last_writer.kind, "unmapped_tile");
        assert_eq!(r1.d3d12_placement, "reserved(unmapped_tiles)");
        let sp = r1.resolved_view.sparse_tiles.as_ref().unwrap();
        assert_eq!((sp.first_tile, sp.mapped_tiles, sp.unmapped_tiles), (1, 0, 1));

        // 3. srv:2 at event 11 -> missing_binding (empty_descriptor_slot).
        let q2 = ProbeQuery {
            event: Some(11),
            binding: BindingSelector::parse("srv:2").unwrap(),
            follow_descriptor: false,
            ..Default::default()
        };
        let r2 = &probe_capture::<fn(u64, usize) -> Result<Vec<u8>, String>>(
            &db_path,
            &q2,
            &AliasMap::default(),
            None,
        )
        .unwrap()[0];
        assert_eq!(r2.resolved_view.binding_status, "empty_descriptor_slot");
        assert_eq!(r2.content_verdict.verdict, "missing_binding");

        // 4. srv:3 at event 11 -> missing_binding (missing_root_arg).
        let q3 = ProbeQuery {
            event: Some(11),
            binding: BindingSelector::parse("srv:3").unwrap(),
            follow_descriptor: false,
            ..Default::default()
        };
        let r3 = &probe_capture::<fn(u64, usize) -> Result<Vec<u8>, String>>(
            &db_path,
            &q3,
            &AliasMap::default(),
            None,
        )
        .unwrap()[0];
        assert_eq!(r3.resolved_view.binding_status, "missing_root_arg");
        assert_eq!(r3.content_verdict.verdict, "missing_binding");

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

    #[test]
    fn two_level_slot_index_indirection_and_shader_hash_map() {
        let smap = ShaderHashMap::parse("# guest host\n7700e4c3 1e38f9dfec9d1cea3b296a55bd7e8610\n");
        let expanded = smap.expand("7700e4c3");
        assert!(expanded.contains(&"1e38f9dfec9d1cea3b296a55bd7e8610".to_string()));

        let dir = std::env::temp_dir().join(format!("locum_probe_2lvl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blobs = Blobs::new(dir.join("blobs"));

        // Resource 10 (0x4_0000_0000): table row 2 (stride 336) + 0x120 holds u32 slot index 0x0010.
        let mut tbl = vec![0u8; 4096];
        let slot_off = 2 * 336 + 0x120;
        let slot_idx = 0x10u32;
        tbl[slot_off..slot_off + 4].copy_from_slice(&slot_idx.to_le_bytes());
        let tbl_key = hash::key(&tbl);
        blobs.put(&tbl_key, &tbl).unwrap();

        // Resource 11 (0x4c61_0000): descriptor heap; entry 0x10 * 32 = +0x200 holds 16-byte guest view pointing to 0x6000_0400 (len 256).
        let mut heap = vec![0u8; 4096];
        let view_off = (slot_idx as usize) * 32;
        heap[view_off..view_off + 8].copy_from_slice(&0x6000_0400u64.to_le_bytes());
        heap[view_off + 8..view_off + 12].copy_from_slice(&256u32.to_le_bytes());
        let heap_key = hash::key(&heap);
        blobs.put(&heap_key, &heap).unwrap();

        // Resource 17 (0x6000_0000): target buffer with 0x5A at +0x400.
        let mut tgt = vec![0u8; 4096];
        tgt[0x400..0x500].fill(0x5A);
        let tgt_key = hash::key(&tgt);
        blobs.put(&tgt_key, &tgt).unwrap();

        let mut cap = db::Capture::default();
        cap.meta("api", "d3d12");
        cap.meta("blobs", dir.join("blobs").display());
        for (id, k) in [(10, tbl_key), (11, heap_key), (17, tgt_key)] {
            cap.resources.push(db::Resource {
                id,
                kind: "buffer",
                width: 4096,
                bytes: 4096,
                used: true,
                content: Some(k),
                content_source: Some("first-use"),
                ..Default::default()
            });
        }
        cap.draws.push(db::Draw {
            n: 271,
            event: 13106,
            call: "Dispatch",
            args: "[1,1,1]".into(),
            shaders: [
                None,
                None,
                None,
                None,
                None,
                Some("1e38f9dfec9d1cea3b296a55bd7e8610".into()),
            ],
            ..Default::default()
        });
        cap.draw_descriptors.push(db::DrawDescriptor {
            draw: 271,
            stage: "cs",
            kind: "uav",
            slot: 2,
            space: 0,
            parameter: 2,
            resource: Some(10),
            address: Some(0x4_0000_0000),
            size: Some(4096),
            ..Default::default()
        });
        let frame = crate::d3d12::Frame {
            objects: vec![
                crate::d3d12::Object {
                    id: 10,
                    kind: "ID3D12Resource",
                    device: 1,
                    description: json!({"dimension": 1, "width": 4096, "gpu_address": 0x4_0000_0000u64, "heap_properties": {"type": 1}}),
                },
                crate::d3d12::Object {
                    id: 11,
                    kind: "ID3D12Resource",
                    device: 1,
                    description: json!({"dimension": 1, "width": 4096, "gpu_address": 0x4c61_0000u64, "heap_properties": {"type": 1}}),
                },
                crate::d3d12::Object {
                    id: 17,
                    kind: "ID3D12Resource",
                    device: 1,
                    description: json!({"dimension": 1, "width": 4096, "gpu_address": 0x6000_0000u64, "heap_properties": {"type": 1}}),
                },
            ],
            ..Default::default()
        };
        let proj = crate::d3d12::Projection::new(&frame.objects);
        cap.d3d12 = Some(frame.finish(&mut db::Capture::default(), proj));
        let db_path = dir.join("capture.db");
        db::write(&db_path, &cap).unwrap();

        let query = ProbeQuery {
            shader: Some("7700e4c3".into()),
            shader_aliases: expanded,
            binding: BindingSelector::parse("uav:2").unwrap(),
            layout: DescriptorLayout {
                row_stride: 336,
                row_index: 2,
                desc_offset: 0x120,
                indirect_heap_va: Some(0x4c61_0000),
                indirect_stride: 32,
                indirect_view_size: 16,
                ..Default::default()
            },
            follow_descriptor: true,
            compare_bytes: 256,
            ..Default::default()
        };
        let res = probe_capture::<fn(u64, usize) -> Result<Vec<u8>, String>>(
            &db_path,
            &query,
            &AliasMap::default(),
            None,
        )
        .unwrap();
        assert_eq!(res.len(), 1);
        let r = &res[0];
        let dec = r.decoded.as_ref().unwrap();
        assert_eq!(dec.slot_index, Some(0x10));
        assert_eq!(dec.indirect_view_va, Some(0x4c61_0200));
        assert_eq!(r.target_resource, Some(17));
        assert_eq!(r.target_offset, Some(0x400));
        assert_eq!(r.content_verdict.verdict, "non_zero");
        assert_eq!(r.content_verdict.nonzero_bytes, 256);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unrecorded_overlapping_dispatch_returns_bytes_unavailable_with_earlier_snapshot_while_disjoint_cites_actual_writer()
     {
        let dir = std::env::temp_dir().join(format!("locum_probe_taint_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let blobs = Blobs::new(dir.join("blobs"));

        // Resource 10 (GPU VA 0x4_0000_0000, 4096 B): first-use has 0x0000_8bf6 at +0x120 and 0x11223344 at +0x500.
        let mut buf = vec![0u8; 4096];
        buf[0x120..0x124].copy_from_slice(&0x0000_8bf6u32.to_le_bytes());
        buf[0x500..0x504].copy_from_slice(&0x1122_3344u32.to_le_bytes());
        let buf_key = hash::key(&buf);
        blobs.put(&buf_key, &buf).unwrap();

        let mut cap = db::Capture::default();
        cap.meta("api", "d3d12");
        cap.meta("blobs", dir.join("blobs").display());
        cap.resources.push(db::Resource {
            id: 10,
            kind: "buffer",
            width: 4096,
            bytes: 4096,
            used: true,
            content: Some(buf_key),
            content_source: Some("first-use"),
            ..Default::default()
        });
        // Earlier Dispatch at event 13094 (draw 270) wrote resource #10 with UAV covering ONLY [0x0, 0x300)
        // -> overlaps +0x120..+0x124, strictly disjoint from +0x500..+0x504!
        cap.events.push(db::Event {
            seq: 13094,
            kind: "dispatch",
            call: "Dispatch",
            draw: Some(270),
            args: "[1,1,1]".into(),
            writes: vec![10],
            ..Default::default()
        });
        cap.draws.push(db::Draw {
            n: 270,
            event: 13094,
            call: "Dispatch",
            args: "[1,1,1]".into(),
            shaders: [None, None, None, None, None, Some("deadbeef".into())],
            ..Default::default()
        });
        cap.draw_descriptors.push(db::DrawDescriptor {
            draw: 270,
            stage: "cs",
            kind: "uav",
            slot: 0,
            space: 0,
            parameter: 0,
            resource: Some(10),
            address: Some(0x4_0000_0000),
            size: Some(0x300),
            ..Default::default()
        });

        // Target Dispatch at event 13106 (draw 271) binds full resource #10 at uav:2.
        cap.draws.push(db::Draw {
            n: 271,
            event: 13106,
            call: "Dispatch",
            args: "[1,1,1]".into(),
            shaders: [None, None, None, None, None, Some("1e38f9df".into())],
            ..Default::default()
        });
        cap.draw_descriptors.push(db::DrawDescriptor {
            draw: 271,
            stage: "cs",
            kind: "uav",
            slot: 2,
            space: 0,
            parameter: 2,
            resource: Some(10),
            address: Some(0x4_0000_0000),
            size: Some(4096),
            ..Default::default()
        });
        let frame = crate::d3d12::Frame {
            objects: vec![crate::d3d12::Object {
                id: 10,
                kind: "ID3D12Resource",
                device: 1,
                description: json!({"dimension": 1, "width": 4096, "gpu_address": 0x4_0000_0000u64, "heap_properties": {"type": 1}}),
            }],
            ..Default::default()
        };
        let proj = crate::d3d12::Projection::new(&frame.objects);
        cap.d3d12 = Some(frame.finish(&mut db::Capture::default(), proj));
        let db_path = dir.join("capture.db");
        db::write(&db_path, &cap).unwrap();

        // Case A: probe 4 bytes at +0x120 (overlapped by Dispatch 13094's UAV [0x0, 0x300))
        // -> MUST return bytes_unavailable at execution, expose earlier first-use snapshot separately,
        //    and record inspected [0x120, 0x124) separately from bound view [0x120, 0x1000).
        let q_overlap = ProbeQuery {
            event: Some(13106),
            binding: BindingSelector::parse("uav:2").unwrap(),
            layout: DescriptorLayout {
                desc_offset: 0x120,
                ..Default::default()
            },
            follow_descriptor: false,
            compare_bytes: 4,
            ..Default::default()
        };
        let r_ov = &probe_capture::<fn(u64, usize) -> Result<Vec<u8>, String>>(
            &db_path,
            &q_overlap,
            &AliasMap::default(),
            None,
        )
        .unwrap()[0];
        assert_eq!(r_ov.content_verdict.verdict, "bytes_unavailable");
        assert_eq!(r_ov.content_verdict.inspected_bytes, 0);
        assert_eq!(r_ov.last_writer.kind, "unrecorded_gpu_write");
        assert_eq!(r_ov.last_writer.event, Some(13094));
        assert_eq!(r_ov.resolved_view.inspected_byte_offset, Some(0x120));
        assert_eq!(r_ov.resolved_view.inspected_byte_length, Some(4));
        assert_eq!(r_ov.resolved_view.byte_offset, Some(0x120));
        assert_eq!(r_ov.resolved_view.byte_length, Some(4096 - 0x120));
        let snap = r_ov.earlier_snapshot.as_ref().expect("earlier snapshot exposed");
        assert_eq!(snap.inspected_bytes, 4);
        assert_eq!(snap.nonzero_bytes, 2);
        assert_eq!(snap.hex_preview, "f68b0000");

        // Case B: probe 4 bytes at +0x500 (strictly disjoint from Dispatch 13094's UAV [0x0, 0x300))
        // -> MUST skip Dispatch 13094, cite actual supplying writer (first_use_initial), and return non_zero!
        let q_disjoint = ProbeQuery {
            event: Some(13106),
            binding: BindingSelector::parse("uav:2").unwrap(),
            layout: DescriptorLayout {
                desc_offset: 0x500,
                ..Default::default()
            },
            follow_descriptor: false,
            compare_bytes: 4,
            ..Default::default()
        };
        let r_dj = &probe_capture::<fn(u64, usize) -> Result<Vec<u8>, String>>(
            &db_path,
            &q_disjoint,
            &AliasMap::default(),
            None,
        )
        .unwrap()[0];
        assert_eq!(r_dj.content_verdict.verdict, "non_zero");
        assert_eq!(r_dj.content_verdict.inspected_bytes, 4);
        assert_eq!(r_dj.content_verdict.nonzero_bytes, 4);
        assert_eq!(r_dj.last_writer.kind, "first_use_initial");
        assert!(r_dj.earlier_snapshot.is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
