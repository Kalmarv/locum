//! 10-second in-memory real-time ETW flight-recorder ring mode (zero disk writes during steady state).
//!
//! Runs a real-time ETW consumer (`EVENT_TRACE_REAL_TIME_MODE`) feeding a bounded QPC ring buffer
//! (`--ring-secs 10.0`) and dumps a symbolized unified QPC timeline automatically on:
//! - GPU device removal / TDR (`0x887A0006` / `0x887A0005` / `0x887A0007` or DxgKrnl TDR 540/541/547/548)
//! - Submit stall (`--stall-secs 2.0`: in-flight GPU packet or silence after submissions while process is alive)
//! - Optional target process exit (`--dump-on-exit`) or bounded wait (`--max-wait`).

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
    FlightTriggerInfo, TimelineBuilder, TimelineReport, qpc_frequency, qpc_now, start_timeline_session,
    stop_session_handle,
};

/// Configuration for the real-time in-memory flight recorder.
#[derive(Clone, Debug)]
pub struct FlightRecorderOptions {
    pub pid: u32,
    /// In-memory ring duration in seconds (default 10.0).
    pub ring_secs: f64,
    /// Stall threshold in seconds (default 2.0): triggers if an in-flight GPU packet or silence after
    /// active GPU submission exceeds this duration while the target process is still alive.
    pub stall_secs: f64,
    /// Maximum wall-clock time to arm before dumping the ring cleanly (`None` = wait until trigger).
    pub max_wait_secs: Option<f64>,
    /// Trigger a dump if the target process exits.
    pub dump_on_exit: bool,
    /// Capture inline ETW CPU stacks (symbolized out-of-process via dbghelp on dump).
    pub enable_stacks: bool,
    /// Hard event cap in the in-memory ring to bound RAM usage.
    pub max_ring_events: usize,
}

impl FlightRecorderOptions {
    pub fn for_pid(pid: u32) -> Self {
        Self {
            pid,
            ring_secs: 10.0,
            stall_secs: 2.0,
            max_wait_secs: None,
            dump_on_exit: true,
            enable_stacks: true,
            max_ring_events: 250_000,
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
    let trigger_reason: String;
    let trigger_qpc: i64;

    loop {
        std::thread::sleep(Duration::from_millis(50));
        let now_qpc = qpc_now();

        // Check device-removal / TDR and submit stall conditions under lock.
        let check = {
            let b = shared.builder.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((qpc, ref msg)) = b.tdr_or_removal_alert {
                Some((qpc, format!("device_removal_or_tdr: {msg}")))
            } else if opts.stall_secs > 0.0 && b.total_target_submits > 0 {
                if let Some(age) = b.oldest_in_flight_secs(now_qpc)
                    && age >= opts.stall_secs
                {
                    Some((
                        now_qpc,
                        format!(
                            "gpu_packet_stall: in-flight packet stalled for {age:.2}s (>= {:.1}s threshold)",
                            opts.stall_secs
                        ),
                    ))
                } else if let Some(last_sub) = b.last_submit_qpc {
                    let silence = (now_qpc - last_sub).max(0) as f64 / freq as f64;
                    if silence >= opts.stall_secs && is_process_alive(opts.pid) {
                        Some((
                            now_qpc,
                            format!(
                                "submit_stall: no GPU submission on pid={} for {silence:.2}s (>= {:.1}s threshold after {} submits)",
                                opts.pid, opts.stall_secs, b.total_target_submits
                            ),
                        ))
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

        if let Some((qpc, reason)) = check {
            trigger_qpc = qpc;
            trigger_reason = reason;
            break;
        }

        if opts.dump_on_exit && !is_process_alive(opts.pid) {
            trigger_qpc = now_qpc;
            trigger_reason = format!("process_exited: pid={} terminated", opts.pid);
            break;
        }

        if let Some(max_s) = opts.max_wait_secs
            && started.elapsed().as_secs_f64() >= max_s
        {
            trigger_qpc = now_qpc;
            trigger_reason = format!("max_wait_elapsed: {max_s:.1}s window completed cleanly");
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

    let trig = FlightTriggerInfo {
        reason: trigger_reason,
        trigger_qpc,
        ring_secs: opts.ring_secs,
        total_observed_events: builder.total_raw_events,
        evicted_ring_events: builder.evicted_events,
    };
    Ok(builder.finish(
        format!("flight_ring(pid={}, {:.0}s)", opts.pid, opts.ring_secs),
        &mut symbolizer,
        warnings,
        Some(trig),
    ))
}
