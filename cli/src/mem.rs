//! `locum-cli mem`: out-of-process, zero-suspend CPU and GPU (VidMm/DxgKrnl ETW) memory ground truth.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use locum_mem::capture_join::join_capture;
use locum_mem::diff::{diff_reports, format_diff_text, load_report_or_etl};
use locum_mem::etw::{
    DXGKRNL_KEYWORDS_ALLOCS_ONLY, DXGKRNL_KEYWORDS_DEFAULT, LiveTraceOptions, TraceSession, parse_etl,
};
use locum_mem::labels::LabelMap;
use locum_mem::reconcile::{
    AllocSort, FormatOptions, PlacementFilter, format_text_with_options, parse_byte_size, reconcile,
};
use locum_mem::win32::{InspectOptions, inspect_process, resolve_target_pid};

use crate::out;

#[derive(Args)]
pub struct TargetArgs {
    /// Target process ID (mutually exclusive with --pid-of-child / --exe-name).
    #[arg(long, conflicts_with_all = ["pid_of_child", "exe_name"])]
    pid: Option<u32>,
    /// Resolve target as the live child/descendant process of this parent PID (e.g. a launcher/runner PID); may be combined with --exe-name.
    #[arg(long, alias = "child-of")]
    pid_of_child: Option<u32>,
    /// Resolve target by executable name (case-insensitive, with or without `.exe`); may be combined with --pid-of-child.
    #[arg(long, alias = "name")]
    exe_name: Option<String>,
}

#[derive(Args)]
pub struct FilterArgs {
    /// Filter Top-N VidMm allocations by placement (`all`, `local`, `vram`, `bar`, `host`, `demoted`, `evicted`, `unknown`).
    #[arg(long, default_value = "all")]
    placement: String,
    /// Minimum allocation size filter for Top-N tables (e.g. `64K`, `1M`, `64MiB`).
    #[arg(long)]
    min: Option<String>,
    /// Maximum allocation size filter for Top-N tables (e.g. `4M`, `256MiB`).
    #[arg(long)]
    max: Option<String>,
    /// Sort order for Top-N VidMm allocations (`size`, `newest`, `oldest`).
    #[arg(long, default_value = "size")]
    sort: String,
    /// Show inactive adapter segments with 0 samples in text output.
    #[arg(long)]
    all_segments: bool,
    /// Number of top allocations / owners to show in text output.
    #[arg(long, default_value_t = 25)]
    top: usize,
    /// Emit the full report as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
pub enum MemCommand {
    /// Zero-suspend Win32 + D3DKMT snapshot of a running process (optionally enriched with an .etl, --labels, and --capture).
    Inspect {
        #[command(flatten)]
        target: TargetArgs,
        /// Optional TSV mapping address ranges to owners: `start<TAB>end<TAB>owner` (or `start..end<TAB>owner`, `[start..end] -> owner`, or header `start<TAB>size<TAB>owner`); nested ranges allowed (innermost wins).
        #[arg(long)]
        labels: Option<PathBuf>,
        /// Optional DxgKrnl/kernel .etl trace to reconcile against Win32 and QueryVideoMemoryInfo.
        #[arg(long)]
        etl: Option<PathBuf>,
        /// Optional locum capture.db (or capture folder) to join `d12_draw_descriptors`.
        #[arg(long)]
        capture: Option<PathBuf>,
        /// Page stride for QueryWorkingSetEx (1 = exact every 4 KiB page).
        #[arg(long, default_value_t = 1)]
        sample_step: u64,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Live zero-suspend trace: records system-wide DxgKrnl + kernel HardFaults ETW (with rundown), snapshots Win32 + D3DKMT, and reconciles.
    /// Note: DxgKrnl is a system-wide kernel provider (~150-300k events/s, ~70 MB/s on active D3D12 titles); pass --allocs-only and/or --max-etl-mb to reduce ETL volume beside timed runs.
    Trace {
        #[command(flatten)]
        target: TargetArgs,
        /// Recording window in seconds.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
        /// Low-overhead mode: drops high-volume `Base` (per-draw/scheduler/VSync) and `GPUVA` steady-state events while still capturing full `CAPTURE_STATE` rundown for segments and live allocations.
        #[arg(long)]
        allocs_only: bool,
        /// Maximum ETL file size cap in MiB (default 2048).
        #[arg(long, default_value_t = 2048)]
        max_etl_mb: u32,
        /// Path for the recorded .etl file (defaults to a temp file deleted after parsing unless --keep-etl).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Keep the recorded .etl file on disk after parsing.
        #[arg(long)]
        keep_etl: bool,
        /// Optional TSV mapping address ranges to owners: `start<TAB>end<TAB>owner` (or `start..end<TAB>owner`, `[start..end] -> owner`, or header `start<TAB>size<TAB>owner`); nested ranges allowed (innermost wins).
        #[arg(long)]
        labels: Option<PathBuf>,
        /// Optional locum capture.db (or capture folder) to join `d12_draw_descriptors`.
        #[arg(long)]
        capture: Option<PathBuf>,
        /// Page stride for QueryWorkingSetEx (1 = exact every 4 KiB page).
        #[arg(long, default_value_t = 1)]
        sample_step: u64,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Offline analysis of an existing DxgKrnl / WPR / xperf `.etl` trace (optionally joined with --labels and --capture).
    Etl {
        /// Path to the `.etl` file.
        etl: PathBuf,
        /// Optional target PID filter.
        #[arg(long)]
        pid: Option<u32>,
        /// Optional TSV mapping address ranges to owners: `start<TAB>end<TAB>owner` (or `start..end<TAB>owner`, `[start..end] -> owner`, or header `start<TAB>size<TAB>owner`); nested ranges allowed (innermost wins).
        #[arg(long)]
        labels: Option<PathBuf>,
        /// Optional locum capture.db (or capture folder) to join `d12_draw_descriptors`.
        #[arg(long)]
        capture: Option<PathBuf>,
        #[command(flatten)]
        filter: FilterArgs,
    },
    /// Diff two `locum-cli mem` JSON reports or `.etl` traces to show new/grown allocations grouped by placement, owner, and size class.
    Diff {
        /// Baseline snapshot JSON or `.etl` trace (`A`).
        a: PathBuf,
        /// Later snapshot JSON or `.etl` trace (`B`).
        b: PathBuf,
        /// Optional PID filter when loading `.etl` inputs.
        #[arg(long)]
        pid: Option<u32>,
        /// Optional TSV mapping address ranges to owners (used to attribute new allocations).
        #[arg(long)]
        labels: Option<PathBuf>,
        #[command(flatten)]
        filter: FilterArgs,
    },
}

pub fn run(cmd: MemCommand) -> Result<(), String> {
    match cmd {
        MemCommand::Inspect {
            target,
            labels,
            etl,
            capture,
            sample_step,
            filter,
        } => {
            let fmt_opts = build_format_options(&filter)?;
            let pid = resolve_target_pid(target.pid, target.pid_of_child, target.exe_name.as_deref())?;
            let label_map = load_labels(labels.as_deref())?;
            let mut etw_rep = etl.as_deref().map(|p| parse_etl(p, Some(pid))).transpose()?;
            let win32 = match inspect_process(pid, InspectOptions { sample_step }) {
                Ok(w) => Some(w),
                Err(e) if etw_rep.is_some() => {
                    if let Some(ref mut er) = etw_rep {
                        er.warnings.push(format!(
                            "Win32 process inspection unavailable ({e}); showing ETW adapter-wide / PID data only"
                        ));
                    }
                    None
                }
                Err(e) => return Err(e),
            };
            let cap_rep = capture
                .as_deref()
                .map(|p| join_capture(p, &label_map, etw_rep.as_ref()))
                .transpose()?;
            let rep = reconcile(win32, etw_rep, &label_map, cap_rep);
            emit(&rep, Some(&label_map), &fmt_opts, filter.json)
        }
        MemCommand::Trace {
            target,
            duration,
            allocs_only,
            max_etl_mb,
            out: etl_out,
            keep_etl,
            labels,
            capture,
            sample_step,
            filter,
        } => {
            let fmt_opts = build_format_options(&filter)?;
            let pid = resolve_target_pid(target.pid, target.pid_of_child, target.exe_name.as_deref())?;
            let label_map = load_labels(labels.as_deref())?;
            let is_temp = etl_out.is_none();
            let etl_path = etl_out.unwrap_or_else(|| {
                std::env::temp_dir().join(format!("locum_mem_{}_{}.etl", pid, std::process::id()))
            });
            let mut trace_opts = LiveTraceOptions::new(etl_path.clone());
            trace_opts.max_file_mb = max_etl_mb.max(16);
            if allocs_only {
                trace_opts.dxgkrnl_keywords = DXGKRNL_KEYWORDS_ALLOCS_ONLY;
            }
            let mut sess = TraceSession::start(trace_opts)?;
            let hf_enabled = sess.hard_fault_provider_enabled;
            let mut warnings = sess.warnings.clone();

            let dur_ms = (duration.max(0.1) * 1000.0) as u64;
            std::thread::sleep(Duration::from_millis(dur_ms));

            // Take zero-suspend Win32 + D3DKMT snapshot while target is live; if the target is a
            // protected process (e.g. dwm.exe -> Access Denied), still keep the ETW adapter-wide + PID report!
            let win32 = match inspect_process(pid, InspectOptions { sample_step }) {
                Ok(w) => Some(w),
                Err(e) => {
                    warnings.push(format!(
                        "Win32 process inspection unavailable ({e}); showing ETW adapter-wide / PID data only"
                    ));
                    None
                }
            };
            sess.capture_state(DXGKRNL_KEYWORDS_DEFAULT);
            std::thread::sleep(Duration::from_millis(150));
            let written = sess.stop()?;

            let mut etw_rep = parse_etl(&written, Some(pid))?;
            etw_rep.apply_live_session_metadata(hf_enabled, warnings);

            if is_temp && !keep_etl {
                let _ = std::fs::remove_file(&written);
            }

            let cap_rep = capture
                .as_deref()
                .map(|p| join_capture(p, &label_map, Some(&etw_rep)))
                .transpose()?;
            let rep = reconcile(win32, Some(etw_rep), &label_map, cap_rep);
            emit(&rep, Some(&label_map), &fmt_opts, filter.json)
        }
        MemCommand::Etl {
            etl,
            pid,
            labels,
            capture,
            filter,
        } => {
            let fmt_opts = build_format_options(&filter)?;
            let label_map = load_labels(labels.as_deref())?;
            let etw_rep = parse_etl(&etl, pid)?;
            let cap_rep = capture
                .as_deref()
                .map(|p| join_capture(p, &label_map, Some(&etw_rep)))
                .transpose()?;
            let rep = reconcile(None, Some(etw_rep), &label_map, cap_rep);
            emit(&rep, Some(&label_map), &fmt_opts, filter.json)
        }
        MemCommand::Diff {
            a,
            b,
            pid,
            labels,
            filter,
        } => {
            let fmt_opts = build_format_options(&filter)?;
            let label_map = load_labels(labels.as_deref())?;
            let rep_a = load_report_or_etl(&a, pid, &label_map)?;
            let rep_b = load_report_or_etl(&b, pid, &label_map)?;
            let diff = diff_reports(
                &a.display().to_string(),
                &rep_a,
                &b.display().to_string(),
                &rep_b,
                &label_map,
            );
            if filter.json {
                let s = serde_json::to_string_pretty(&diff).map_err(|e| format!("serialize diff: {e}"))?;
                out!("{s}");
            } else {
                out!("{}", format_diff_text(&diff, &fmt_opts));
            }
            Ok(())
        }
    }
}

fn build_format_options(filter: &FilterArgs) -> Result<FormatOptions, String> {
    Ok(FormatOptions {
        top: filter.top,
        placement: PlacementFilter::parse(&filter.placement)?,
        min_bytes: filter.min.as_deref().map(parse_byte_size).transpose()?,
        max_bytes: filter.max.as_deref().map(parse_byte_size).transpose()?,
        sort: AllocSort::parse(&filter.sort)?,
        all_segments: filter.all_segments,
    })
}

fn load_labels(path: Option<&std::path::Path>) -> Result<LabelMap, String> {
    match path {
        Some(p) => LabelMap::from_file(p),
        None => Ok(LabelMap::default()),
    }
}

fn emit(
    rep: &locum_mem::reconcile::MemReport,
    labels: Option<&LabelMap>,
    opts: &FormatOptions,
    json: bool,
) -> Result<(), String> {
    if json {
        let s = serde_json::to_string_pretty(rep).map_err(|e| format!("serialize report: {e}"))?;
        out!("{s}");
    } else {
        out!("{}", format_text_with_options(rep, labels, opts));
    }
    Ok(())
}
