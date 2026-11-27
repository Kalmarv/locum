//! `locum-cli mem`: out-of-process, zero-suspend CPU and GPU (VidMm/DxgKrnl ETW) memory ground truth.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Subcommand};
use locum_mem::capture_join::join_capture;
use locum_mem::diff::{diff_reports, format_diff_text, load_report_or_etl};
use locum_mem::etw::{
    DXGKRNL_KEYWORDS_ALLOCS_ONLY, DXGKRNL_KEYWORDS_DEFAULT, LiveTraceOptions, TraceSession, parse_etl,
};
use locum_mem::flight::{FlightRecorderOptions, FlightTriggers, run_flight_recorder_with_callback};
use locum_mem::labels::LabelMap;
use locum_mem::reconcile::{
    AllocSort, FormatOptions, PlacementFilter, format_text_with_options, parse_byte_size, reconcile,
};
use locum_mem::symbols::Symbolizer;
use locum_mem::timeline::{
    LiveTimelineOptions, TimelineCutOptions, TimelineEventKind, TimelineFormatOptions, capture_live_timeline,
    format_capabilities_text, format_timeline_text, parse_timeline_etl_with_options, probe_etw_capabilities,
};
use locum_mem::win32::{InspectOptions, inspect_process, resolve_target_pid};

use crate::out;
use crate::probe::ProbeArgs;

#[derive(Args)]
pub struct TargetArgs {
    /// Target process ID (mutually exclusive with --pid-of-child / --exe-name).
    #[arg(long, conflicts_with_all = ["pid_of_child", "exe_name"])]
    pub pid: Option<u32>,
    /// Resolve target as the live child/descendant process of this parent PID (e.g. a launcher/runner PID); may be combined with --exe-name.
    #[arg(long, alias = "child-of")]
    pub pid_of_child: Option<u32>,
    /// Resolve target by executable name (case-insensitive, with or without `.exe`); may be combined with --pid-of-child.
    #[arg(long, alias = "name")]
    pub exe_name: Option<String>,
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

#[derive(Args)]
pub struct InspectArgs {
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
}

#[derive(Args)]
pub struct TraceArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Recording window in seconds.
    #[arg(long, default_value_t = 5.0)]
    duration: f64,
    /// Low-overhead mode: drops high-volume `Base` (per-draw/scheduler/VSync) and `GPUVA` steady-state events while still capturing full `CAPTURE_STATE` rundown for segments and live allocations.
    #[arg(long)]
    allocs_only: bool,
    /// Maximum ETL file size cap in MiB (default 2048; if reached, recording truncates with a warning rather than failing). On large D3D12 titles (~60k+ allocs), initial + final rundowns alone can use ~250-350 MiB.
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
}

#[derive(Args)]
pub struct EtlArgs {
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
}

#[derive(Args)]
pub struct DiffArgs {
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
}

#[derive(Args)]
pub struct TimelineFilterArgs {
    /// Filter printed timeline events to duration >= N ms (e.g. `1.0`).
    #[arg(long)]
    min_ms: Option<f64>,
    /// Filter printed timeline events by kind (`gpu_busy`, `submit`, `fence_signal`, `fence_wait`, `present`, `paging`, `vprotect`, `hard_fault`, `cswitch_wait`, `cpu_sample`, `tdr`, `device_removed`).
    #[arg(long)]
    kind: Option<String>,
    /// Filter printed timeline events to a single thread ID.
    #[arg(long)]
    tid: Option<u32>,
    /// Number of top queues / waits / stacks / frames to show.
    #[arg(long, default_value_t = 20)]
    top: usize,
    /// Maximum timeline event lines to print in greppable text mode (0 hides individual `EV` lines).
    #[arg(long, default_value_t = 120)]
    limit: usize,
    /// Print active DxgKrnl process summary (`PIDS`) and exit without per-event listing (helpful to pick `--pid`).
    #[arg(long)]
    list_pids: bool,
    /// Emit structured JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
pub struct TimelineArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// Optional existing `.etl` trace to decode offline instead of recording live.
    #[arg(long)]
    etl: Option<PathBuf>,
    /// Live recording duration in seconds (ignored when `--etl` is given).
    #[arg(long, default_value_t = 3.0)]
    duration: f64,
    /// Disable inline ETW stack capture during live recording.
    #[arg(long)]
    no_stacks: bool,
    /// Maximum ETL file size cap in MiB for live timeline recording (default 1024).
    #[arg(long, default_value_t = 1024)]
    max_etl_mb: u32,
    /// Optional path to save the recorded `.etl` file.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Keep the recorded `.etl` file on disk after parsing.
    #[arg(long)]
    keep_etl: bool,
    /// Cut the timeline (queues, waits, paging/demotion, frames, stacks, EV) strictly to complete present-to-present boundaries `[present[0], present[last]]` (drops pre-first-present startup/rundown and post-last-present tail). Combine with `--frame N` to isolate a single frame.
    #[arg(long)]
    between_presents: bool,
    /// Isolate one 0-based complete present-to-present frame `[present[N], present[N+1]]` (implies `--between-presents`).
    #[arg(long)]
    frame: Option<usize>,
    /// Opt-in exact-size VidMm allocation lookup for the target PID (e.g. `1792MiB`): prints every alive match (resident, demoted, evicted, or uncommitted) with handle, CpuVisible flag, rundown priority, segment/residency, and CPU/GPU VA ranges, with no top-N cutoff and an explicit zero-match count.
    #[arg(long, value_name = "SIZE")]
    alloc_size: Option<String>,
    /// Print every exact-size bucket of the PID's alive VidMm allocation histogram (by segment/residency, kind, and priority) and every handle-joined eviction row without top-N truncation.
    #[arg(long)]
    alloc_hist: bool,
    #[command(flatten)]
    view: TimelineFilterArgs,
}

#[derive(Args)]
pub struct FlightArgs {
    #[command(flatten)]
    target: TargetArgs,
    /// In-memory QPC ring duration in seconds (default 10.0; zero disk writes while armed).
    #[arg(long, default_value_t = 10.0)]
    ring_secs: f64,
    /// Submit stall threshold in seconds (default 2.0): dumps ring on in-flight GPU stall or submit silence while process is alive.
    #[arg(long, default_value_t = 2.0)]
    stall_secs: f64,
    /// Comma-separated trigger set (`all`, or subset of `removal,tdr,stall,exit`, e.g. `--on removal,tdr`).
    #[arg(long, default_value = "all")]
    on: String,
    /// Keep watching after a non-fatal dump (e.g. stall) until `--max-wait`, fatal removal/TDR, or exit (writes numbered files when `--out` is set).
    #[arg(long)]
    rearm: bool,
    /// Optional maximum wall-clock seconds to stay armed before dumping cleanly.
    #[arg(long)]
    max_wait: Option<f64>,
    /// Do not dump if the target process exits cleanly.
    #[arg(long)]
    no_dump_on_exit: bool,
    /// Disable inline ETW stack capture.
    #[arg(long)]
    no_stacks: bool,
    /// Maximum retained events in the in-memory ring (default 600000 ~ 10s at 60k target ev/s).
    #[arg(long, default_value_t = 600_000)]
    max_ring_events: usize,
    /// Optional output file path for the dumped flight report (also prints a one-line summary to stdout).
    #[arg(long)]
    out: Option<PathBuf>,
    #[command(flatten)]
    view: TimelineFilterArgs,
}

#[derive(Subcommand)]
pub enum MemCommand {
    /// Zero-suspend Win32 + D3DKMT snapshot of a running process (optionally enriched with an .etl, --labels, and --capture).
    Inspect(Box<InspectArgs>),
    /// Live zero-suspend trace: records system-wide DxgKrnl + kernel HardFaults ETW (with rundown), snapshots Win32 + D3DKMT, and reconciles.
    /// Note: DxgKrnl is a system-wide kernel provider (~150-300k events/s, ~70 MB/s on active D3D12 titles); pass --allocs-only and/or --max-etl-mb to reduce ETL volume beside timed runs.
    Trace(Box<TraceArgs>),
    /// Offline analysis of an existing DxgKrnl / WPR / xperf `.etl` trace (optionally joined with --labels and --capture).
    Etl(Box<EtlArgs>),
    /// Diff two `locum-cli mem` JSON reports or `.etl` traces to show new/grown allocations grouped by placement, owner, and size class.
    Diff(Box<DiffArgs>),
    /// Unified QPC timeline + wait attribution + out-of-process symbolized CPU stacks (live or offline `.etl`), greppable text + JSON.
    Timeline(Box<TimelineArgs>),
    /// 10-second in-memory real-time ETW flight-recorder ring that dumps on device removal (`0x887a0006`/TDR) or 2 s submit stall (zero disk writes while armed).
    Flight(Box<FlightArgs>),
    /// Probe which ETW kernel/system providers and StackWalk flags succeed non-admin on this machine.
    EtwCaps {
        /// Emit structured JSON.
        #[arg(long)]
        json: bool,
    },
    /// Dispatch-scoped compute buffer probe: follow indexed raw-buffer descriptors (`row+0x120` style) for a shader hash and compare GPU bytes against their CPU alias, joined with `locum-mem` placement.
    Probe(Box<ProbeArgs>),
}

pub fn run(cmd: MemCommand) -> Result<(), String> {
    match cmd {
        MemCommand::Inspect(args) => {
            let InspectArgs {
                target,
                labels,
                etl,
                capture,
                sample_step,
                filter,
            } = *args;
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
        MemCommand::Trace(args) => {
            let TraceArgs {
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
            } = *args;
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
            let mut extra_warnings = Vec::new();

            let dur_ms = (duration.max(0.1) * 1000.0) as u64;
            std::thread::sleep(Duration::from_millis(dur_ms));

            // Take zero-suspend Win32 + D3DKMT snapshot while target is live; if the target is a
            // protected process (e.g. dwm.exe -> Access Denied), still keep the ETW adapter-wide + PID report!
            let win32 = match inspect_process(pid, InspectOptions { sample_step }) {
                Ok(w) => Some(w),
                Err(e) => {
                    extra_warnings.push(format!(
                        "Win32 process inspection unavailable ({e}); showing ETW adapter-wide / PID data only"
                    ));
                    None
                }
            };
            sess.capture_state(DXGKRNL_KEYWORDS_DEFAULT);
            std::thread::sleep(Duration::from_millis(150));
            let stop_res = sess.stop_with_stats()?;
            let mut warnings = stop_res.warnings;
            warnings.extend(extra_warnings);

            let mut etw_rep = parse_etl(&stop_res.etl_path, Some(pid))?;
            etw_rep.events_lost = stop_res.events_lost;
            etw_rep.truncated_by_max_size = stop_res.truncated_by_max_size;
            etw_rep.apply_live_session_metadata(hf_enabled, warnings);

            if is_temp && !keep_etl {
                let _ = std::fs::remove_file(&stop_res.etl_path);
            }

            let cap_rep = capture
                .as_deref()
                .map(|p| join_capture(p, &label_map, Some(&etw_rep)))
                .transpose()?;
            let rep = reconcile(win32, Some(etw_rep), &label_map, cap_rep);
            emit(&rep, Some(&label_map), &fmt_opts, filter.json)
        }
        MemCommand::Etl(args) => {
            let EtlArgs {
                etl,
                pid,
                labels,
                capture,
                filter,
            } = *args;
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
        MemCommand::Diff(args) => {
            let DiffArgs {
                a,
                b,
                pid,
                labels,
                filter,
            } = *args;
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
        MemCommand::Timeline(args) => {
            let kind_filter = args
                .view
                .kind
                .as_deref()
                .map(TimelineEventKind::parse_filter)
                .transpose()?;
            let fmt_opts = TimelineFormatOptions {
                min_ms: args.view.min_ms,
                kind: kind_filter,
                tid: args.view.tid,
                top: args.view.top,
                limit: args.view.limit,
                list_pids: args.view.list_pids,
            };
            let alloc_size_bytes = args.alloc_size.as_deref().map(parse_byte_size).transpose()?;
            let cut = TimelineCutOptions {
                between_presents: args.between_presents || args.frame.is_some(),
                frame_index: args.frame,
                alloc_size_bytes,
                full_alloc_hist: args.alloc_hist,
            };
            let rep = if let Some(ref etl_path) = args.etl {
                let pid = if args.target.pid.is_some()
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
                let mut sym = pid
                    .map(Symbolizer::for_process)
                    .unwrap_or_else(|| Symbolizer::new(None));
                parse_timeline_etl_with_options(etl_path, pid, &mut sym, cut)?
            } else {
                let pid = resolve_target_pid(
                    args.target.pid,
                    args.target.pid_of_child,
                    args.target.exe_name.as_deref(),
                )?;
                capture_live_timeline(LiveTimelineOptions {
                    pid,
                    duration_secs: args.duration,
                    enable_stacks: !args.no_stacks,
                    max_etl_mb: args.max_etl_mb,
                    etl_out: args.out,
                    keep_etl: args.keep_etl,
                    cut,
                })?
            };
            if args.view.json {
                let s = serde_json::to_string_pretty(&rep).map_err(|e| format!("serialize timeline: {e}"))?;
                out!("{s}");
            } else {
                out!("{}", format_timeline_text(&rep, &fmt_opts));
            }
            if rep.target_events == 0
                && let Some(exit_ms) = rep.target_exited_at_ms
            {
                let code_note = rep
                    .target_exit_code
                    .map(|c| format!(" (exit_code={c:#x})"))
                    .unwrap_or_default();
                return Err(format!(
                    "target pid {} exited at +{exit_ms:.0} ms{code_note} with 0 target events",
                    rep.pid.unwrap_or(0)
                ));
            }
            Ok(())
        }
        MemCommand::Flight(args) => {
            let pid = resolve_target_pid(
                args.target.pid,
                args.target.pid_of_child,
                args.target.exe_name.as_deref(),
            )?;
            let kind_filter = args
                .view
                .kind
                .as_deref()
                .map(TimelineEventKind::parse_filter)
                .transpose()?;
            let fmt_opts = TimelineFormatOptions {
                min_ms: args.view.min_ms,
                kind: kind_filter,
                tid: args.view.tid,
                top: args.view.top,
                limit: args.view.limit,
                list_pids: args.view.list_pids,
            };
            let triggers = FlightTriggers::parse(&args.on)?;
            let rearm = args.rearm;
            let out_base = args.out.clone();
            let as_json = args.view.json;
            let dumps = run_flight_recorder_with_callback(
                FlightRecorderOptions {
                    pid,
                    ring_secs: args.ring_secs,
                    stall_secs: args.stall_secs,
                    triggers,
                    rearm,
                    max_wait_secs: args.max_wait,
                    dump_on_exit: !args.no_dump_on_exit,
                    enable_stacks: !args.no_stacks,
                    max_ring_events: args.max_ring_events,
                },
                |dump_idx, rep| {
                    let rendered = if as_json {
                        serde_json::to_string_pretty(&rep)
                            .map_err(|e| format!("serialize flight report: {e}"))?
                    } else {
                        format_timeline_text(&rep, &fmt_opts)
                    };
                    if let Some(ref base) = out_base {
                        let dest = if rearm {
                            let stem = base.file_stem().and_then(|s| s.to_str()).unwrap_or("flight");
                            let ext = base.extension().and_then(|s| s.to_str());
                            let name = match ext {
                                Some(e) => format!("{stem}.{dump_idx}.{e}"),
                                None => format!("{stem}.{dump_idx}"),
                            };
                            base.with_file_name(name)
                        } else {
                            base.clone()
                        };
                        if let Some(parent) = dest.parent()
                            && !parent.as_os_str().is_empty()
                        {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        std::fs::write(&dest, &rendered)
                            .map_err(|e| format!("write flight report {}: {e}", dest.display()))?;
                        out!(
                            "flight dump #{dump_idx} written to {} (trigger={:?}, events={}, retained_secs={:.2})",
                            dest.display(),
                            rep.flight_trigger.as_ref().map(|t| t.reason.as_str()),
                            rep.target_events,
                            rep.flight_trigger
                                .as_ref()
                                .map(|t| t.retained_secs)
                                .unwrap_or(rep.duration_ms / 1000.0),
                        );
                    } else {
                        out!("{rendered}");
                    }
                    Ok(())
                },
            )?;
            if dumps == 0 {
                out!(
                    "flight recorder exited cleanly without firing any configured trigger (--on={})",
                    args.on
                );
            }
            Ok(())
        }
        MemCommand::EtwCaps { json } => {
            let rep = probe_etw_capabilities()?;
            if json {
                let s = serde_json::to_string_pretty(&rep).map_err(|e| format!("serialize etw-caps: {e}"))?;
                out!("{s}");
            } else {
                out!("{}", format_capabilities_text(&rep));
            }
            Ok(())
        }
        MemCommand::Probe(args) => crate::probe::run(*args),
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
