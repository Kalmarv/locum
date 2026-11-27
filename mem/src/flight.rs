//! 10-second in-memory real-time ETW flight-recorder ring mode (zero disk writes during steady state).
//!
//! Runs a real-time ETW consumer (`EVENT_TRACE_REAL_TIME_MODE`) feeding a bounded QPC ring buffer
//! (`--ring-secs 10.0`) and dumps a symbolized unified QPC timeline automatically on:
//! - GPU device removal / TDR (`0x887A0006` / `0x887A0005` / `0x887A0007` or DxgKrnl TDR 540/541/547/548)
//! - Submit stall (`--stall-secs 2.0`: in-flight GPU packet or silence after submissions while process is alive)
//! - Optional target process exit (`--dump-on-exit`) or bounded wait (`--max-wait`).

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS};
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, EVENT_RECORD, EVENT_TRACE_LOGFILEW, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROCESS_TRACE_MODE_REAL_TIME, PROCESSTRACE_HANDLE, ProcessTrace,
};
use windows::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::core::PWSTR;

use crate::symbols::Symbolizer;
use crate::timeline::{
    FlightTriggerInfo, StalledPacketInfo, TimelineBuilder, TimelineReport, qpc_frequency, qpc_now,
    start_timeline_session, stop_session_handle,
};

/// Which conditions should trigger a flight-recorder dump (`--on removal,tdr`, `--on all`, etc.).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlightTriggers {
    pub removal_tdr: bool,
    pub stall: bool,
    pub exit: bool,
}

impl Default for FlightTriggers {
    fn default() -> Self {
        Self {
            removal_tdr: true,
            stall: true,
            exit: true,
        }
    }
}

impl FlightTriggers {
    /// Parses `all` or a comma-separated list from `{removal, tdr, stall, exit}`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let s = spec.trim().to_ascii_lowercase();
        if s.is_empty() || s == "all" {
            return Ok(Self::default());
        }
        let mut out = Self {
            removal_tdr: false,
            stall: false,
            exit: false,
        };
        for tok in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match tok {
                "removal" | "device_removal" | "tdr" | "hang" => out.removal_tdr = true,
                "stall" | "gpu_stall" | "submit_stall" => out.stall = true,
                "exit" | "process_exit" => out.exit = true,
                "all" => return Ok(Self::default()),
                other => {
                    return Err(format!(
                        "unknown --on trigger {other:?} (expected comma-separated: removal, tdr, stall, exit, or all)"
                    ));
                }
            }
        }
        if !out.removal_tdr && !out.stall && !out.exit {
            return Err("--on must enable at least one trigger (removal, tdr, stall, exit, or all)".into());
        }
        Ok(out)
    }
}

/// Configuration for the real-time in-memory flight recorder.
#[derive(Clone, Debug)]
pub struct FlightRecorderOptions {
    pub pid: u32,
    /// In-memory ring duration in seconds (default 10.0).
    pub ring_secs: f64,
    /// Stall threshold in seconds (default 2.0): triggers if an in-flight GPU packet or silence after
    /// active GPU submission exceeds this duration while the target process is still alive.
    pub stall_secs: f64,
    /// Trigger set (`--on`, default `all`: `removal,tdr,stall,exit`).
    pub triggers: FlightTriggers,
    /// Keep watching after a non-fatal dump (e.g. stall) until `--max-wait`, fatal removal/TDR, or exit.
    pub rearm: bool,
    /// Maximum wall-clock time to arm before dumping cleanly (`None` = wait until trigger).
    pub max_wait_secs: Option<f64>,
    /// Trigger a dump if the target process exits (also gated by `triggers.exit`).
    pub dump_on_exit: bool,
    /// Capture inline ETW CPU stacks (symbolized out-of-process via dbghelp on dump).
    pub enable_stacks: bool,
    /// Hard event cap in the in-memory ring to bound RAM usage (default 600,000 ~ 10 s at 60k ev/s).
    pub max_ring_events: usize,
}

impl FlightRecorderOptions {
    pub fn for_pid(pid: u32) -> Self {
        Self {
            pid,
            ring_secs: 10.0,
            stall_secs: 2.0,
            triggers: FlightTriggers::default(),
            rearm: false,
            max_wait_secs: None,
            dump_on_exit: true,
            enable_stacks: true,
            max_ring_events: 600_000,
        }
    }
}

struct SharedRing {
    builder: Mutex<TimelineBuilder>,
}

unsafe extern "system" fn flight_event_cb(rec: *mut EVENT_RECORD) {
    if rec.is_null() {
        return;
    }
    let rec_ref = unsafe { &*rec };
    if rec_ref.UserContext.is_null() {
        return;
    }
    let shared = unsafe { &*rec_ref.UserContext.cast::<SharedRing>() };
    if let Ok(mut b) = shared.builder.lock() {
        b.on_event(rec_ref);
    }
}

fn is_process_alive(pid: u32) -> bool {
    const STILL_ACTIVE: u32 = 259;
    let Ok(h) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    let mut code = 0u32;
    let alive = unsafe { GetExitCodeProcess(h, &mut code) }.is_ok() && code == STILL_ACTIVE;
    unsafe {
        let _ = CloseHandle(h);
    }
    alive
}

/// Arms the real-time in-memory flight recorder and blocks until a trigger fires (device removal /
/// TDR, submit stall, process exit, or `max_wait_secs`), then returns the symbolized 10-second ring report.
pub fn run_flight_recorder(opts: FlightRecorderOptions) -> Result<TimelineReport, String> {
    let mut last: Option<TimelineReport> = None;
    let mut single_opts = opts;
    single_opts.rearm = false;
    run_flight_recorder_with_callback(single_opts, |_, rep| {
        last = Some(rep);
        Ok(())
    })?;
    last.ok_or_else(|| "flight recorder stopped without producing a report".into())
}

/// Arms the real-time flight recorder, calling `on_dump(dump_index, report)` on each trigger.
/// When `opts.rearm` is `true`, non-fatal stall triggers snapshot the ring and stay armed until
/// fatal device removal/TDR, target exit, or `max_wait_secs`.
pub fn run_flight_recorder_with_callback<F>(
    opts: FlightRecorderOptions,
    mut on_dump: F,
) -> Result<usize, String>
where
    F: FnMut(usize, TimelineReport) -> Result<(), String>,
{
    let freq = qpc_frequency();
    let name = format!("locum_flight_{}_{}", std::process::id(), qpc_now() & 0xFFFF_FFFF);
    let mut warnings = Vec::new();

    // Pre-snapshot modules while target is live so we can still symbolize if target crashes/exits!
    let mut symbolizer = Symbolizer::for_process(opts.pid);

    let (sess_handle, _) = start_timeline_session(&name, None, 0, opts.enable_stacks, true, &mut warnings)?;

    let shared = Arc::new(SharedRing {
        builder: Mutex::new(
            TimelineBuilder::new(Some(opts.pid), freq).with_ring(opts.ring_secs, opts.max_ring_events),
        ),
    });

    // Launch real-time ETW consumer thread.
    let consumer_shared = shared.clone();
    let consumer_name = name.clone();
    let consumer = std::thread::spawn(move || -> Result<(), String> {
        let mut wide_name: Vec<u16> = consumer_name.encode_utf16().chain(std::iter::once(0)).collect();
        let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
        logfile.LoggerName = PWSTR(wide_name.as_mut_ptr());
        logfile.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
        logfile.Anonymous2.EventRecordCallback = Some(flight_event_cb);
        logfile.Context = (Arc::as_ptr(&consumer_shared) as *mut SharedRing).cast::<c_void>();

        let h: PROCESSTRACE_HANDLE = unsafe { OpenTraceW(&mut logfile) };
        if h.Value == u64::MAX {
            return Err(format!(
                "OpenTraceW(real-time {consumer_name}) failed: {}",
                std::io::Error::last_os_error()
            ));
        }
        let handles = [h];
        let rc = unsafe { ProcessTrace(&handles, None, None) };
        unsafe {
            let _ = CloseTrace(h);
        }
        if rc != ERROR_SUCCESS && rc.0 != 1223 {
            return Err(format!(
                "ProcessTrace(real-time) failed with Win32 error {}",
                rc.0
            ));
        }
        Ok(())
    });

    let started = Instant::now();
    let mut ack_stalled_pkts: HashSet<(u64, u64)> = HashSet::new();
    let mut last_silence_dump: Option<Instant> = None;
    let mut dump_count = 0usize;
    let final_trigger: Option<(i64, String, Option<StalledPacketInfo>, Option<StalledPacketInfo>)>;

    let make_trigger_info = |b: &TimelineBuilder,
                             trigger_qpc: i64,
                             reason: String,
                             stalled_packet: Option<StalledPacketInfo>,
                             oldest_wait_packet: Option<StalledPacketInfo>| {
        let retained_secs = b.retained_ring_secs();
        let capped = b.raw_events.len() >= opts.max_ring_events && retained_secs + 0.25 < opts.ring_secs;
        FlightTriggerInfo {
            reason,
            trigger_qpc,
            ring_secs: opts.ring_secs,
            retained_secs,
            max_ring_events: opts.max_ring_events,
            capped_by_event_limit: capped,
            total_observed_events: b.total_raw_events,
            evicted_ring_events: b.evicted_events,
            stalled_packet,
            oldest_wait_packet,
        }
    };

    loop {
        std::thread::sleep(Duration::from_millis(50));
        let now_qpc = qpc_now();

        // Check device-removal / TDR and submit stall conditions under lock.
        enum TriggerEvent {
            Fatal(i64, String, Option<StalledPacketInfo>, Option<StalledPacketInfo>),
            RearmableStall(
                i64,
                String,
                Option<StalledPacketInfo>,
                Option<StalledPacketInfo>,
                Option<(u64, u64)>,
            ),
        }

        let check = {
            let b = shared.builder.lock().unwrap_or_else(|e| e.into_inner());
            let oldest_wait = b.oldest_in_flight_wait_packet(now_qpc);
            if opts.triggers.removal_tdr
                && let Some((qpc, ref msg)) = b.tdr_or_removal_alert
            {
                let stalled = b.oldest_in_flight_work_packet(now_qpc, None);
                Some(TriggerEvent::Fatal(
                    qpc,
                    format!("device_removal_or_tdr: {msg}"),
                    stalled,
                    oldest_wait,
                ))
            } else if opts.triggers.stall && opts.stall_secs > 0.0 && b.total_target_submits > 0 {
                if let Some(pkt) = b.oldest_in_flight_work_packet(now_qpc, Some(&ack_stalled_pkts))
                    && pkt.age_secs >= opts.stall_secs
                {
                    let key = (pkt.context, pkt.seq);
                    let reason = format!(
                        "gpu_packet_stall: {} (>= {:.1}s threshold)",
                        pkt.format_compact(),
                        opts.stall_secs
                    );
                    if opts.rearm {
                        Some(TriggerEvent::RearmableStall(
                            now_qpc,
                            reason,
                            Some(pkt),
                            oldest_wait,
                            Some(key),
                        ))
                    } else {
                        Some(TriggerEvent::Fatal(now_qpc, reason, Some(pkt), oldest_wait))
                    }
                } else if let Some(last_sub) = b.last_submit_qpc {
                    let silence = (now_qpc - last_sub).max(0) as f64 / freq as f64;
                    let cooled_down = last_silence_dump
                        .is_none_or(|t| t.elapsed().as_secs_f64() >= opts.stall_secs.max(1.0));
                    if silence >= opts.stall_secs && cooled_down && is_process_alive(opts.pid) {
                        let reason = format!(
                            "submit_stall: no GPU submission on pid={} for {silence:.2}s (>= {:.1}s threshold after {} submits)",
                            opts.pid, opts.stall_secs, b.total_target_submits
                        );
                        if opts.rearm {
                            Some(TriggerEvent::RearmableStall(
                                now_qpc,
                                reason,
                                None,
                                oldest_wait,
                                None,
                            ))
                        } else {
                            Some(TriggerEvent::Fatal(now_qpc, reason, None, oldest_wait))
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some(ev) = check {
            match ev {
                TriggerEvent::Fatal(qpc, reason, stalled, oldest_wait) => {
                    final_trigger = Some((qpc, reason, stalled, oldest_wait));
                    break;
                }
                TriggerEvent::RearmableStall(qpc, reason, stalled, oldest_wait, ack_key) => {
                    if let Some(k) = ack_key {
                        ack_stalled_pkts.insert(k);
                    } else {
                        last_silence_dump = Some(Instant::now());
                    }
                    if let Ok(mods) = crate::symbols::snapshot_process_modules(opts.pid) {
                        for m in mods {
                            symbolizer.add_module(m);
                        }
                    }
                    let rep = {
                        let b = shared.builder.lock().unwrap_or_else(|e| e.into_inner());
                        let trig = make_trigger_info(&b, qpc, reason, stalled, oldest_wait);
                        b.snapshot_report(
                            format!("flight_ring(pid={}, {:.0}s)", opts.pid, opts.ring_secs),
                            &mut symbolizer,
                            warnings.clone(),
                            Some(trig),
                        )
                    };
                    dump_count += 1;
                    on_dump(dump_count, rep)?;
                    continue;
                }
            }
        }

        if !is_process_alive(opts.pid) {
            if opts.dump_on_exit && opts.triggers.exit {
                let (stalled, oldest_wait) = {
                    let b = shared.builder.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        b.oldest_in_flight_work_packet(now_qpc, None),
                        b.oldest_in_flight_wait_packet(now_qpc),
                    )
                };
                final_trigger = Some((
                    now_qpc,
                    format!("process_exited: pid={} terminated", opts.pid),
                    stalled,
                    oldest_wait,
                ));
            } else {
                final_trigger = None;
            }
            break;
        }

        if let Some(max_s) = opts.max_wait_secs
            && started.elapsed().as_secs_f64() >= max_s
        {
            if !opts.rearm || dump_count == 0 {
                let (stalled, oldest_wait) = {
                    let b = shared.builder.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        b.oldest_in_flight_work_packet(now_qpc, None),
                        b.oldest_in_flight_wait_packet(now_qpc),
                    )
                };
                final_trigger = Some((
                    now_qpc,
                    format!("max_wait_elapsed: {max_s:.1}s window completed cleanly"),
                    stalled,
                    oldest_wait,
                ));
            } else {
                final_trigger = None;
            }
            break;
        }
    }

    // Refresh modules one last time if process is still alive (picks up dynamically loaded DLLs).
    if let Ok(mods) = crate::symbols::snapshot_process_modules(opts.pid) {
        for m in mods {
            symbolizer.add_module(m);
        }
    }

    // Stop the real-time ETW session; ProcessTrace returns cleanly once remaining buffers flush.
    stop_session_handle(sess_handle, &name);
    if let Ok(Err(e)) = consumer.join() {
        warnings.push(format!("real-time consumer note: {e}"));
    }

    let builder = match Arc::try_unwrap(shared) {
        Ok(s) => s.builder.into_inner().unwrap_or_else(|e| e.into_inner()),
        Err(arc) => {
            let mut guard = arc.builder.lock().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *guard, TimelineBuilder::new(Some(opts.pid), freq))
        }
    };

    if let Some((trigger_qpc, trigger_reason, stalled, oldest_wait)) = final_trigger {
        let trig = make_trigger_info(&builder, trigger_qpc, trigger_reason, stalled, oldest_wait);
        let rep = builder.finish(
            format!("flight_ring(pid={}, {:.0}s)", opts.pid, opts.ring_secs),
            &mut symbolizer,
            warnings,
            Some(trig),
        );
        dump_count += 1;
        on_dump(dump_count, rep)?;
    }

    Ok(dump_count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flight_triggers() {
        assert_eq!(FlightTriggers::parse("all").unwrap(), FlightTriggers::default());
        assert_eq!(
            FlightTriggers::parse("removal,tdr").unwrap(),
            FlightTriggers {
                removal_tdr: true,
                stall: false,
                exit: false,
            }
        );
        assert_eq!(
            FlightTriggers::parse("stall,exit").unwrap(),
            FlightTriggers {
                removal_tdr: false,
                stall: true,
                exit: true,
            }
        );
        assert!(FlightTriggers::parse("bogus").is_err());
    }
}
