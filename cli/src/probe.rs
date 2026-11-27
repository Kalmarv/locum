//! `locum-cli mem probe`: dispatch-scoped compute buffer probe + CPU-alias comparator joined with `locum-mem`.

use std::path::PathBuf;

use clap::Args;
use locum_capture::probe::{
    AliasMap, BindingSelector, DescriptorLayout, DispatchProbeResult, ProbeQuery, probe_capture,
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
    /// Do not dereference a descriptor inside the buffer; compare bytes directly at the bound buffer + offset.
    #[arg(long)]
    pub no_follow: bool,
}

#[derive(Args)]
pub struct ProbeArgs {
    /// Path to a `locum` `capture.db` (or capture folder containing `capture.db`).
    pub capture: PathBuf,
    /// Compute shader hash prefix or debug name substring (e.g. `7700e4c3`).
    #[arg(long)]
    pub shader: Option<String>,
    /// Specific dispatch draw number (`draws.n`).
    #[arg(long, alias = "dispatch")]
    pub draw: Option<u64>,
    #[command(flatten)]
    pub desc: ProbeDescArgs,
    /// Number of bytes to compare between shader-visible GPU address and CPU alias (default 4096).
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
    /// Maximum matching compute dispatches to probe (default 10).
    #[arg(long, default_value_t = 10)]
    pub limit: usize,
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
    let layout = DescriptorLayout {
        table_offset: parse_u64_arg("--table-offset", &args.desc.table_offset)?,
        row_stride: parse_u64_arg("--row-stride", &args.desc.row_stride)?,
        row_index: args.desc.row,
        desc_offset: parse_u64_arg("--desc-offset", &args.desc.desc_offset)?,
        desc_size: args.desc.desc_size.max(8),
        va_offset: args.desc.va_offset,
        va_mask: match args.desc.va_mask.as_deref() {
            Some(m) => parse_u64_arg("--va-mask", m)?,
            None => u64::MAX,
        },
        va_shift: args.desc.va_shift,
        size_offset: args.desc.size_offset,
        size_width: 4,
    };
    let query = ProbeQuery {
        shader: args.shader,
        draw: args.draw,
        binding,
        layout,
        follow_descriptor: !args.desc.no_follow,
        compare_bytes: args.bytes.max(1),
        limit: args.limit.max(1),
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
                r.gpu_vidmm_placement = Some(
                    etw.placement_of_gpu_va(gva)
                        .unwrap_or("unmapped_in_etw")
                        .to_string(),
                );
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
        out!("{}", format_probe_text(&results));
    }
    Ok(())
}

pub fn format_probe_text(results: &[DispatchProbeResult]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "locum-mem dispatch probe | matches={} | thread_suspends=0",
        results.len()
    );
    for r in results {
        let cs_name = r.dispatch.cs_name.as_deref().unwrap_or("-");
        let sub = r
            .dispatch
            .submission
            .map(|s| s.to_string())
            .unwrap_or_else(|| "-".into());
        let _ = writeln!(
            out,
            "\n== Dispatch draw={} event={} sub={} call={} args={} cs={} ({}) ==",
            r.dispatch.draw,
            r.dispatch.event,
            sub,
            r.dispatch.call,
            r.dispatch.args,
            r.dispatch.cs_hash,
            cs_name,
        );
        let _ = writeln!(out, "  CS Descriptors ({}):", r.dispatch.descriptors.len());
        for d in r.dispatch.descriptors.iter().take(16) {
            let res_s = d.resource.map(|v| format!("#{v}")).unwrap_or_else(|| "-".into());
            let va_s = d.address.map(|v| format!("{v:#x}")).unwrap_or_else(|| "-".into());
            let sz_s = d.size.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
            let flg = d.flag.as_deref().unwrap_or("-");
            let _ = writeln!(
                out,
                "    {}:{} space={} param={} res={:<6} gpu_va={:<18} size={:<10} flag={}",
                d.kind, d.slot, d.space, d.parameter, res_s, va_s, sz_s, flg
            );
        }
        let _ = writeln!(
            out,
            "  Descriptor Source   : {} [sample_point: {}]",
            r.descriptor_source, r.descriptor_sample_point
        );
        if let Some(ref note) = r.descriptor_note {
            let _ = writeln!(out, "  Descriptor Note     : {note}");
        }
        if let Some(ref dec) = r.decoded {
            let _ = writeln!(out, "  Raw Descriptor Hex  : {}", dec.raw_hex);
            let words: Vec<String> = dec.words_u32.iter().map(|w| format!("{w:#010x}")).collect();
            let _ = writeln!(out, "  Raw Descriptor u32s : [{}]", words.join(", "));
            let _ = writeln!(
                out,
                "  Decoded Target      : gpu_va={} size={} ({})",
                dec.gpu_va
                    .map(|v| format!("{v:#x}"))
                    .unwrap_or_else(|| "-".into()),
                dec.decoded_size
                    .map(|v| format!("{v} B"))
                    .unwrap_or_else(|| "-".into()),
                dec.decode_note,
            );
        }
        let _ = writeln!(
            out,
            "  Resolved Resource   : res={} offset={} buf_width={} gpu_va={}",
            r.target_resource
                .map(|v| format!("#{v}"))
                .unwrap_or_else(|| "none".into()),
            r.target_offset
                .map(|v| format!("{v:#x}"))
                .unwrap_or_else(|| "-".into()),
            r.target_buffer_width
                .map(|v| format!("{v} B"))
                .unwrap_or_else(|| "-".into()),
            r.target_gpu_va
                .map(|v| format!("{v:#x}"))
                .unwrap_or_else(|| "-".into()),
        );
        let _ = writeln!(
            out,
            "  GPU Sample Point    : {}{}",
            r.gpu_sample_point,
            r.gpu_unavailable_reason
                .as_deref()
                .map(|u| format!(" ({u})"))
                .unwrap_or_default(),
        );
        let _ = writeln!(
            out,
            "  CPU Alias           : va={} label={} [sample_point: {}]{}",
            r.cpu_alias_va
                .map(|v| format!("{v:#x}"))
                .unwrap_or_else(|| "unmapped".into()),
            r.cpu_alias_label.as_deref().unwrap_or("-"),
            r.cpu_sample_point,
            r.cpu_unavailable_reason
                .as_deref()
                .map(|u| format!(" ({u})"))
                .unwrap_or_default(),
        );
        if r.gpu_vidmm_placement.is_some()
            || r.gpu_label_owner.is_some()
            || r.cpu_label_owner.is_some()
            || r.cpu_win32_residency.is_some()
        {
            let _ = writeln!(
                out,
                "  locum-mem Placement : vidmm={} | gpu_owner={} | cpu_owner={} | cpu_ws={}",
                r.gpu_vidmm_placement.as_deref().unwrap_or("n/a"),
                r.gpu_label_owner.as_deref().unwrap_or("n/a"),
                r.cpu_label_owner.as_deref().unwrap_or("n/a"),
                r.cpu_win32_residency.as_deref().unwrap_or("n/a"),
            );
        }
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
    out
}
