//! `locum-cli mem`: out-of-process, zero-suspend CPU and GPU (VidMm/DxgKrnl ETW) memory ground truth.

use std::path::PathBuf;
use std::time::Duration;

use clap::Subcommand;
use locum_mem::capture_join::join_capture;
use locum_mem::etw::{DXGKRNL_KEYWORDS_DEFAULT, LiveTraceOptions, TraceSession, parse_etl};
use locum_mem::labels::LabelMap;
use locum_mem::reconcile::{format_text, reconcile};
use locum_mem::win32::{InspectOptions, inspect_process, resolve_target_pid};

use crate::out;

#[derive(Subcommand)]
pub enum MemCommand {
    /// Zero-suspend Win32 + D3DKMT snapshot of a running process (optionally enriched with an .etl, --labels, and --capture).
    Inspect {
        /// Target process ID (mutually exclusive with --pid-of-child / --exe-name).
        #[arg(long, conflicts_with_all = ["pid_of_child", "exe_name"])]
        pid: Option<u32>,
        /// Resolve target as the live child/descendant process of this parent PID (e.g. a launcher/runner PID); may be combined with --exe-name.
        #[arg(long)]
        pid_of_child: Option<u32>,
        /// Resolve target by executable name (case-insensitive, with or without `.exe`); may be combined with --pid-of-child.
        #[arg(long)]
        exe_name: Option<String>,
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
        /// Number of top allocations / owners to show in text output.
        #[arg(long, default_value_t = 25)]
        top: usize,
        /// Emit the full reconciled report as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Live zero-suspend trace: records DxgKrnl + kernel HardFaults ETW (with rundown), snapshots Win32 + D3DKMT, and reconciles.
    Trace {
        /// Target process ID (mutually exclusive with --pid-of-child / --exe-name).
        #[arg(long, conflicts_with_all = ["pid_of_child", "exe_name"])]
        pid: Option<u32>,
        /// Resolve target as the live child/descendant process of this parent PID (e.g. a launcher/runner PID); may be combined with --exe-name.
        #[arg(long)]
        pid_of_child: Option<u32>,
        /// Resolve target by executable name (case-insensitive, with or without `.exe`); may be combined with --pid-of-child.
        #[arg(long)]
        exe_name: Option<String>,
        /// Recording window in seconds.
        #[arg(long, default_value_t = 5.0)]
        duration: f64,
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
        /// Number of top allocations / owners to show in text output.
        #[arg(long, default_value_t = 25)]
        top: usize,
        /// Emit the full reconciled report as JSON.
        #[arg(long)]
        json: bool,
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
        /// Number of top allocations / owners to show in text output.
        #[arg(long, default_value_t = 25)]
        top: usize,
        /// Emit the full reconciled report as JSON.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(cmd: MemCommand) -> Result<(), String> {
    match cmd {
        MemCommand::Inspect {
            pid,
            pid_of_child,
            exe_name,
            labels,
            etl,
            capture,
            sample_step,
            top,
            json,
        } => {
            let pid = resolve_target_pid(pid, pid_of_child, exe_name.as_deref())?;
            let label_map = load_labels(labels.as_deref())?;
            let win32 = inspect_process(pid, InspectOptions { sample_step })?;
            let etw_rep = etl.as_deref().map(|p| parse_etl(p, Some(pid))).transpose()?;
            let cap_rep = capture
                .as_deref()
                .map(|p| join_capture(p, &label_map, etw_rep.as_ref()))
                .transpose()?;
            let rep = reconcile(Some(win32), etw_rep, &label_map, cap_rep);
            emit(&rep, top, json)
        }
        MemCommand::Trace {
            pid,
            pid_of_child,
            exe_name,
            duration,
            out: etl_out,
            keep_etl,
            labels,
            capture,
            sample_step,
            top,
            json,
        } => {
            let pid = resolve_target_pid(pid, pid_of_child, exe_name.as_deref())?;
            let label_map = load_labels(labels.as_deref())?;
            let is_temp = etl_out.is_none();
            let etl_path = etl_out.unwrap_or_else(|| {
                std::env::temp_dir().join(format!("locum_mem_{}_{}.etl", pid, std::process::id()))
            });
            let mut sess = TraceSession::start(LiveTraceOptions::new(etl_path.clone()))?;
            let hf_enabled = sess.hard_fault_provider_enabled;
            let warnings = sess.warnings.clone();

            let dur_ms = (duration.max(0.1) * 1000.0) as u64;
            std::thread::sleep(Duration::from_millis(dur_ms));

            // Take zero-suspend Win32 + D3DKMT snapshot while target is live and trigger final rundown.
            let win32 = inspect_process(pid, InspectOptions { sample_step })?;
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
            let rep = reconcile(Some(win32), Some(etw_rep), &label_map, cap_rep);
            emit(&rep, top, json)
        }
        MemCommand::Etl {
            etl,
            pid,
            labels,
            capture,
            top,
            json,
        } => {
            let label_map = load_labels(labels.as_deref())?;
            let etw_rep = parse_etl(&etl, pid)?;
            let cap_rep = capture
                .as_deref()
                .map(|p| join_capture(p, &label_map, Some(&etw_rep)))
                .transpose()?;
            let rep = reconcile(None, Some(etw_rep), &label_map, cap_rep);
            emit(&rep, top, json)
        }
    }
}

fn load_labels(path: Option<&std::path::Path>) -> Result<LabelMap, String> {
    match path {
        Some(p) => LabelMap::from_file(p),
        None => Ok(LabelMap::default()),
    }
}

fn emit(rep: &locum_mem::reconcile::MemReport, top: usize, json: bool) -> Result<(), String> {
    if json {
        let s = serde_json::to_string_pretty(rep).map_err(|e| format!("serialize report: {e}"))?;
        out!("{s}");
    } else {
        out!("{}", format_text(rep, top));
    }
    Ok(())
}
