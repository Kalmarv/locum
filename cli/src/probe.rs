//! `locum-cli mem probe`: dispatch-scoped compute buffer probe + CPU-alias comparator joined with `locum-mem`.

use std::path::PathBuf;

use clap::Args;
use locum_capture::probe::{
    AliasMap, BindingSelector, DescriptorLayout, DispatchProbeResult, ProbeQuery, ShaderHashMap,
    probe_capture,
};
use locum_mem::etw::parse_etl;
use locum_mem::labels::LabelMap;
use locum_mem::reconcile::mib;
use locum_mem::win32::{InspectOptions, inspect_process, read_process_bytes, resolve_target_pid};

use crate::out;

#[derive(Args)]
pub struct ProbeDescArgs {
    /// Bound compute descriptor holding the raw/indexed descriptor table: `srv:<slot>[:<space>]`, `uav:<slot>`, `cb:<slot>`, `param:<idx>`, or `va:<hex>`.
    #[arg(long)]
    pub binding: Option<String>,
    /// Base byte offset of the descriptor table inside the bound buffer (hex `0x...` or decimal).
    #[arg(long, default_value = "0")]
    pub table_offset: String,
    /// Row stride in bytes when indexing into a table of rows (e.g. `0x200`).
    #[arg(long, default_value = "0")]
    pub row_stride: String,
    /// Row index to inspect (default 0).
    #[arg(long, default_value_t = 0)]
    pub row: u64,
    /// Byte offset of the descriptor within the row (e.g. `0x120`).
    #[arg(long, default_value = "0")]
    pub desc_offset: String,
    /// Raw descriptor size in bytes to read and display (default 32).
    #[arg(long, default_value_t = 32)]
    pub desc_size: usize,
    /// Explicit byte offset of the 64-bit GPU VA inside the raw descriptor (omit for auto-scan against live buffers).
    #[arg(long)]
    pub va_offset: Option<usize>,
    /// Bitmask applied to the decoded VA word when `--va-offset` is set (default `0xffffffffffffffff`).
    #[arg(long)]
    pub va_mask: Option<String>,
    /// Left-shift applied to the masked VA word when `--va-offset` is set (default 0).
    #[arg(long, default_value_t = 0)]
    pub va_shift: u32,
    /// Optional byte offset of the buffer size field inside the raw descriptor.
    #[arg(long)]
    pub size_offset: Option<usize>,
    /// Optional 2-level indirection heap base GPU VA (reads a u32 slot index at `row+desc_offset`, then decodes the guest view at `heap_va + slot_index * indirect_stride`, e.g. `0x44c610000`).
    #[arg(long)]
    pub indirect_heap_va: Option<String>,
    /// Stride in bytes of entries in `--indirect-heap-va` (default 32).
    #[arg(long, default_value = "32")]
    pub indirect_stride: String,
    /// Size in bytes of the indirect guest view to read and decode (default 16).
    #[arg(long, default_value_t = 16)]
    pub indirect_view_size: usize,
    /// Optional window base GPU VA added to relative offsets decoded from a guest view.
    #[arg(long)]
    pub window_base: Option<String>,
    /// Explicitly dereference a shader-decoded descriptor inside the bound buffer (auto-enabled when `--row-stride`, `--desc-offset`, `--table-offset`, `--va-offset`, `--size-offset`, or `--indirect-heap-va` is set).
    #[arg(long)]
    pub follow: bool,
    /// Do not dereference a descriptor inside the buffer; inspect the bound view directly (default when no descriptor layout offsets are given).
    #[arg(long)]
    pub no_follow: bool,
}

#[derive(Args)]
pub struct ProbeArgs {
    /// Path to a `locum` `capture.db` (or capture folder containing `capture.db`).
    pub capture: PathBuf,
    /// Specific event sequence number (`events.seq`) to inspect (supports both compute dispatches and graphics draws).
    #[arg(long)]
    pub event: Option<u64>,
    /// Compute or graphics shader hash prefix or debug name substring (e.g. `7700e4c3`).
    #[arg(long)]
    pub shader: Option<String>,
    /// Optional guest-to-host shader hash map file (`guest_hash<TAB>host_hash` per line) so guest hashes match translated `draws.cs`.
    #[arg(long)]
    pub shader_map: Option<PathBuf>,
    /// Shader stage filter (`cs`, `vs`, `hs`, `ds`, `gs`, `ps`, `all`).
    #[arg(long)]
    pub stage: Option<String>,
    /// Specific dispatch/draw number (`draws.n`).
    #[arg(long, alias = "dispatch")]
    pub draw: Option<u64>,
    #[command(flatten)]
    pub desc: ProbeDescArgs,
    /// Number of bytes to inspect/compare between shader-visible GPU address and CPU alias (default 4096).
    #[arg(long, default_value_t = 4096)]
    pub bytes: usize,
    /// App-supplied GPU VA <-> CPU VA alias map (`gpu_start<TAB>cpu_start<TAB>bytes[<TAB>label]` or `sidecar.txt` `visible` lines).
    #[arg(long)]
    pub alias_map: Option<PathBuf>,
    /// Optional DxgKrnl `.etl` trace to join true VidMm segment placement (`local_bar_vram`, `non_local_host`, `demoted_host`).
    #[arg(long)]
    pub etl: Option<PathBuf>,
    /// Optional `--labels` TSV to attribute GPU and CPU addresses to owners.
    #[arg(long)]
    pub labels: Option<PathBuf>,
    #[command(flatten)]
    pub target: crate::mem::TargetArgs,
    /// Maximum matching dispatches/draws to probe (default 10; pass 0 for all matching events).
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
    /// Print only one greppable `VERDICT` summary line per matched event (ideal for scanning 100+ culling calls).
    #[arg(long)]
    pub summary_only: bool,
    /// Emit structured JSON.
    #[arg(long)]
    pub json: bool,
}

fn parse_u64_arg(name: &str, raw: &str) -> Result<u64, String> {
    let s = raw.trim().replace('_', "");
    if let Some(h) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u64::from_str_radix(h, 16).map_err(|e| format!("{name}: invalid hex {raw:?}: {e}"))
    } else {
        s.parse::<u64>()
            .map_err(|e| format!("{name}: invalid integer {raw:?}: {e}"))
    }
}

pub fn run(args: ProbeArgs) -> Result<(), String> {
    let binding = match args.desc.binding.as_deref() {
        Some(spec) => BindingSelector::parse(spec)?,
        None => BindingSelector::default(),
    };
    let table_offset = parse_u64_arg("--table-offset", &args.desc.table_offset)?;
    let row_stride = parse_u64_arg("--row-stride", &args.desc.row_stride)?;
    let desc_offset = parse_u64_arg("--desc-offset", &args.desc.desc_offset)?;
    let indirect_heap_va = args
        .desc
        .indirect_heap_va
        .as_deref()
        .map(|s| parse_u64_arg("--indirect-heap-va", s))
        .transpose()?;
    let indirect_stride = parse_u64_arg("--indirect-stride", &args.desc.indirect_stride)?;
    let window_base = args
        .desc
        .window_base
        .as_deref()
        .map(|s| parse_u64_arg("--window-base", s))
        .transpose()?;
    let layout = DescriptorLayout {
        table_offset,
        row_stride,
        row_index: args.desc.row,
        desc_offset,
        desc_size: args.desc.desc_size.max(8),
        va_offset: args.desc.va_offset,
        va_mask: match args.desc.va_mask.as_deref() {
            Some(m) => parse_u64_arg("--va-mask", m)?,
            None => u64::MAX,
        },
        va_shift: args.desc.va_shift,
        size_offset: args.desc.size_offset,
        size_width: 4,
        indirect_heap_va,
        indirect_stride: indirect_stride.max(4),
        indirect_view_size: args.desc.indirect_view_size.max(8),
        window_base,
    };
    let has_indexed_layout = table_offset != 0
        || row_stride != 0
        || desc_offset != 0
        || args.desc.va_offset.is_some()
        || args.desc.size_offset.is_some()
        || indirect_heap_va.is_some();
    let follow_descriptor = if args.desc.no_follow {
        false
    } else if args.desc.follow {
        true
    } else {
        has_indexed_layout
    };
    let shader_aliases = match (args.shader.as_deref(), args.shader_map.as_deref()) {
        (Some(sh), Some(mp)) => ShaderHashMap::from_file(mp)?.expand(sh),
        _ => Vec::new(),
    };
    let query = ProbeQuery {
        shader: args.shader,
        shader_aliases,
        draw: args.draw,
        event: args.event,
        stage: args.stage,
        binding,
        layout,
        follow_descriptor,
        compare_bytes: args.bytes.max(1),
        limit: args.limit,
    };
    let alias_map = match args.alias_map.as_deref() {
        Some(p) => AliasMap::from_file(p)?,
        None => AliasMap::default(),
    };
    let label_map = match args.labels.as_deref() {
        Some(p) => LabelMap::from_file(p)?,
        None => LabelMap::default(),
    };
    let live_pid = if args.target.pid.is_some()
        || args.target.pid_of_child.is_some()
        || args.target.exe_name.is_some()
    {
        Some(resolve_target_pid(
            args.target.pid,
            args.target.pid_of_child,
            args.target.exe_name.as_deref(),
        )?)
    } else {
        None
    };
    let etw_rep = args.etl.as_deref().map(|p| parse_etl(p, live_pid)).transpose()?;

    let mut results = if let Some(pid) = live_pid {
        probe_capture(
            &args.capture,
            &query,
            &alias_map,
            Some(move |va: u64, len: usize| read_process_bytes(pid, va, len)),
        )?
    } else {
        probe_capture::<fn(u64, usize) -> Result<Vec<u8>, String>>(&args.capture, &query, &alias_map, None)?
    };

    // Optional live Win32 page-residency lookup for CPU alias VAs.
    let win32_snap = if let Some(pid) = live_pid
        && results.iter().any(|r| r.cpu_alias_va.is_some())
    {
        inspect_process(pid, InspectOptions { sample_step: 16 }).ok()
    } else {
        None
    };

    for r in &mut results {
        if let Some(gva) = r.target_gpu_va {
            if let Some(ref etw) = etw_rep {
                let vidmm = etw.placement_of_gpu_va(gva).unwrap_or("unmapped_in_etw");
                r.gpu_vidmm_placement = Some(vidmm.to_string());
                r.observed_placement = if r.d3d12_placement != "unknown" {
                    format!("vidmm={vidmm} | d3d12={}", r.d3d12_placement)
                } else {
                    format!("vidmm={vidmm}")
                };
            }
            if !label_map.is_empty() {
                r.gpu_label_owner = Some(label_map.owner_of(gva).unwrap_or("<unlabeled>").to_string());
            }
        }
        if let Some(cva) = r.cpu_alias_va {
            if !label_map.is_empty() {
                r.cpu_label_owner = Some(label_map.owner_of(cva).unwrap_or("<unlabeled>").to_string());
            }
            if let Some(ref snap) = win32_snap
                && let Some(reg) = snap
                    .regions
                    .iter()
                    .find(|reg| cva >= reg.base && cva < reg.base.saturating_add(reg.size))
            {
                r.cpu_win32_residency = Some(format!(
                    "type={}{} commit={:.2}M ram={:.2}M bar={:.2}M paged={:.2}M untch={:.2}M",
                    reg.mem_type.as_str(),
                    if reg.write_combined { "_wc" } else { "" },
                    mib(reg.size),
                    mib(reg.resident_bytes),
                    mib(reg.bar_mapped_bytes),
                    mib(reg.paged_out_bytes),
                    mib(reg.untouched_bytes),
                ));
            }
        }
    }

    if args.json {
        let s = serde_json::to_string_pretty(&results).map_err(|e| format!("serialize probe: {e}"))?;
        out!("{s}");
    } else {
        out!("{}", format_probe_text(&results, args.summary_only));
    }
    Ok(())
}

pub fn format_probe_text(results: &[DispatchProbeResult], summary_only: bool) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "locum-mem dispatch probe | matches={} | thread_suspends=0",
        results.len()
    );
    for r in results {
        let v = &r.resolved_view;
        let lw = &r.last_writer;
        let cv = &r.content_verdict;
        let res_s = v
            .resource
            .map(|id| format!("#{id}"))
            .unwrap_or_else(|| "none".into());
        let view_range_s = match (v.byte_offset, v.byte_length) {
            (Some(off), Some(len)) => format!("[{off:#x}, {:#x}) ({len} B)", off.saturating_add(len)),
            (Some(off), None) => format!("[{off:#x}, ?)"),
            _ => "-".into(),
        };
        let inspected_range_s = match (v.inspected_byte_offset, v.inspected_byte_length) {
            (Some(off), Some(len)) => format!("[{off:#x}, {:#x}) ({len} B)", off.saturating_add(len)),
            _ => view_range_s.clone(),
        };
        let writer_at = match (lw.event, lw.submission) {
            (Some(ev), Some(sub)) => format!("{}@ev={ev}/sub={sub}", lw.kind),
            (Some(ev), None) => format!("{}@ev={ev}", lw.kind),
            (None, Some(sub)) => format!("{}@sub={sub}", lw.kind),
            (None, None) => lw.kind.clone(),
        };
        let _ = writeln!(
            out,
            "VERDICT event={:<6} draw={:<5} call={:<18} verdict={:<21} binding={:<22} res={:<6} inspected={:<26} view={:<28} writer={:<28} placement={}",
            r.dispatch.event,
            r.dispatch.draw,
            r.dispatch.call,
            cv.verdict,
            v.binding_status,
            res_s,
            inspected_range_s,
            view_range_s,
            writer_at,
            r.observed_placement,
        );
        if summary_only {
            continue;
        }

        let cs_name = r.dispatch.cs_name.as_deref().unwrap_or("-");
        let sub = r
            .dispatch
            .submission
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(
            out,
            "\n== Event {} (draw={} sub={} stage={} call={} args={} shader={} ({})) ==",
            r.dispatch.event,
            r.dispatch.draw,
            sub,
            r.dispatch.stage,
            r.dispatch.call,
            r.dispatch.args,
            r.dispatch.cs_hash,
            cs_name,
        );
        let _ = writeln!(
            out,
            "  1. Resolved View    : status={} binding={}:{}:{} space={} param={} heap={:?} slot={:?} ver={:?} flag={}",
            v.binding_status,
            v.stage,
            v.kind.as_deref().unwrap_or("-"),
            v.slot.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
            v.space.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
            v.parameter.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
            v.heap,
            v.heap_slot,
            v.descriptor_version,
            v.flag.as_deref().unwrap_or("-"),
        );
        let _ = writeln!(
            out,
            "     Byte Range       : res={} gpu_va={} inspected={} view={} buf_width={} stride={} fmt={}",
            res_s,
            v.gpu_va.map(|a| format!("{a:#x}")).unwrap_or_else(|| "-".into()),
            inspected_range_s,
            view_range_s,
            v.resource_width
                .map(|w| format!("{w} B"))
                .unwrap_or_else(|| "-".into()),
            v.stride.map(|s| format!("{s} B")).unwrap_or_else(|| "-".into()),
            v.format.map(|f| f.to_string()).unwrap_or_else(|| "-".into()),
        );
        if let Some(ref why) = v.binding_reason {
            let _ = writeln!(out, "     Binding Note     : {why}");
        }
        if let Some(ref sp) = v.sparse_tiles {
            let _ = writeln!(
                out,
                "     Sparse Tiles     : tiles=[{}..{}) mapped={}/{} unmapped={} heaps={:?}{}{}",
                sp.first_tile,
                sp.first_tile + sp.tile_count,
                sp.mapped_tiles,
                sp.tile_count,
                sp.unmapped_tiles,
                sp.backing_heaps,
                sp.last_tile_mapping
                    .as_deref()
                    .map(|m| format!(" last_op={m}"))
                    .unwrap_or_default(),
                sp.diagnostic
                    .as_deref()
                    .map(|d| format!(" diag={d}"))
                    .unwrap_or_default(),
            );
        }
        let _ = writeln!(
            out,
            "  2. Last Writer      : kind={} event={} sub={} call={} | {}",
            lw.kind,
            lw.event.map(|e| e.to_string()).unwrap_or_else(|| "-".into()),
            lw.submission.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
            lw.call.as_deref().unwrap_or("-"),
            lw.detail,
        );
        let _ = writeln!(
            out,
            "     Content Verdict  : {} ({})",
            cv.verdict.to_ascii_uppercase(),
            cv.summary,
        );
        if let Some(ref es) = r.earlier_snapshot {
            let _ = writeln!(
                out,
                "     Earlier Snapshot : {} ({}/{} non-zero bytes{}, hex={}) [NOT execution-time bytes]",
                es.sample_point,
                es.nonzero_bytes,
                es.inspected_bytes,
                es.first_nonzero_offset
                    .map(|o| format!(", first non-zero at +{o:#x}"))
                    .unwrap_or_default(),
                es.hex_preview,
            );
        }
        let _ = writeln!(
            out,
            "  3. Placement        : {} (vidmm={} | gpu_owner={} | cpu_owner={} | cpu_ws={})",
            r.observed_placement,
            r.gpu_vidmm_placement.as_deref().unwrap_or("unknown"),
            r.gpu_label_owner.as_deref().unwrap_or("n/a"),
            r.cpu_label_owner.as_deref().unwrap_or("n/a"),
            r.cpu_win32_residency.as_deref().unwrap_or("n/a"),
        );
        let _ = writeln!(out, "  Descriptors ({}):", r.dispatch.descriptors.len());
        for d in r.dispatch.descriptors.iter().take(16) {
            let d_res = d.resource.map(|x| format!("#{x}")).unwrap_or_else(|| "-".into());
            let d_va = d.address.map(|x| format!("{x:#x}")).unwrap_or_else(|| "-".into());
            let d_sz = d.size.map(|x| x.to_string()).unwrap_or_else(|| "-".into());
            let flg = d.flag.as_deref().unwrap_or("-");
            let _ = writeln!(
                out,
                "    {}:{}:{} space={} param={} res={:<6} gpu_va={:<18} size={:<10} flag={}",
                d.stage, d.kind, d.slot, d.space, d.parameter, d_res, d_va, d_sz, flg
            );
        }
        if let Some(ref dec) = r.decoded {
            let _ = writeln!(
                out,
                "  Descriptor Source   : {} [sample_point: {}]",
                r.descriptor_source, r.descriptor_sample_point
            );
            if let Some(slot_idx) = dec.slot_index {
                let _ = writeln!(
                    out,
                    "  Indirect Slot Hop   : slot_index={slot_idx:#x} ({slot_idx}) -> view_va={}",
                    dec.indirect_view_va
                        .map(|a| format!("{a:#x}"))
                        .unwrap_or_else(|| "-".into()),
                );
            }
            let _ = writeln!(out, "  Raw Descriptor Hex  : {}", dec.raw_hex);
            let words: Vec<String> = dec.words_u32.iter().map(|w| format!("{w:#010x}")).collect();
            let _ = writeln!(out, "  Raw Descriptor u32s : [{}]", words.join(", "));
            let _ = writeln!(
                out,
                "  Decoded Target      : gpu_va={} size={} ({})",
                dec.gpu_va
                    .map(|a| format!("{a:#x}"))
                    .unwrap_or_else(|| "-".into()),
                dec.decoded_size
                    .map(|s| format!("{s} B"))
                    .unwrap_or_else(|| "-".into()),
                dec.decode_note,
            );
        }
        let _ = writeln!(
            out,
            "  GPU Sample Point    : {}{}",
            r.gpu_sample_point,
            r.gpu_unavailable_reason
                .as_deref()
                .map(|u| format!(" ({u})"))
                .unwrap_or_default(),
        );
        if r.cpu_alias_va.is_some() || r.comparison.is_some() {
            let _ = writeln!(
                out,
                "  CPU Alias           : va={} label={} [sample_point: {}]{}",
                r.cpu_alias_va
                    .map(|a| format!("{a:#x}"))
                    .unwrap_or_else(|| "unmapped".into()),
                r.cpu_alias_label.as_deref().unwrap_or("-"),
                r.cpu_sample_point,
                r.cpu_unavailable_reason
                    .as_deref()
                    .map(|u| format!(" ({u})"))
                    .unwrap_or_default(),
            );
            match &r.comparison {
                Some(cmp) => {
                    let _ = writeln!(
                        out,
                        "  Byte Comparison     : {} ({}/{} bytes compared, {} differing bytes{})",
                        cmp.status.to_ascii_uppercase(),
                        cmp.compared_bytes,
                        cmp.requested_bytes,
                        cmp.mismatch_count,
                        cmp.first_mismatch_offset
                            .map(|o| format!(", first mismatch at +{o:#x}"))
                            .unwrap_or_default(),
                    );
                    if let Some(ref d) = cmp.first_mismatch_detail {
                        let _ = writeln!(out, "  First Mismatch      : {d}");
                    } else {
                        let _ = writeln!(out, "  Head 32B Preview    : {}", cmp.gpu_preview_hex);
                    }
                }
                None => {
                    let _ = writeln!(
                        out,
                        "  Byte Comparison     : UNAVAILABLE (gpu={}, cpu={})",
                        r.gpu_unavailable_reason.as_deref().unwrap_or("ok"),
                        r.cpu_unavailable_reason.as_deref().unwrap_or("ok"),
                    );
                }
            }
        }
    }
    out
}
