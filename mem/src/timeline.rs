//! Unified QPC Timeline + Wait Attribution + ETW Capability Probing.
//!
//! Correlates on **one QPC clock** (zero target suspends):
//! - DxgKrnl GPU hardware queue busy intervals (`QueuePacket` / `DmaPacket` per `hContext`/`hHwQueue`/`NodeOrdinal`)
//! - Fence signals and waits (`SignalSynchronizationObject*`, `WaitForSynchronizationObject*`, `Fence`)
//! - Per-present framing (`Present`, `PresentHistory`, `Flip`, `PresentQueuePacket`)
//! - Paging & memory transitions (DxgKrnl `PageIn`/`Evict`/`AllocFault`/`MakeResident`, `SystemMemory` HardFaults/VirtualAlloc, `Kernel-Memory` WS/VirtualProtect)
//! - Device removal / TDR (`0x887A0006` / `0x887A0005` / `0x887A0007`, DxgKrnl TDR 540/541/547/548)
//! - CPU stacks (inline `EVENT_HEADER_EXT_TYPE_STACK_TRACE64` + classic `StackWalk` / `SampledProfile`), symbolized out-of-process via `dbghelp` + `TH32CS_SNAPMODULE` / ETW `ImageLoad`
//! - Thread wait attribution: full `CSwitch` + `ReadyThread` when permitted (offline WPR/xperf or one-time ACL), and automatic non-admin fallback via DxgKrnl CPU/GPU fence waits + `MakeResident` stalls + CPU stacks.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS,
};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, ENABLE_TRACE_PARAMETERS,
    EVENT_CONTROL_CODE_CAPTURE_STATE, EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_HEADER_FLAG_32_BIT_HEADER,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_FILE_MODE_SEQUENTIAL, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_SYSTEM_LOGGER_MODE, EnableTraceEx2, OpenTraceW,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESS_TRACE_MODE_RAW_TIMESTAMP, PROCESSTRACE_HANDLE, ProcessTrace,
    StartTraceW, TRACE_EVENT_INFO, TdhGetEventInformation, WNODE_FLAG_TRACED_GUID,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
use windows::core::{GUID, HSTRING, PWSTR};

use crate::THREAD_SUSPENDS;
use crate::etw::{CLASSIC_PAGEFAULT_GUID, DXGKRNL_PROVIDER, SYSTEM_MEMORY_PROVIDER};
use crate::symbols::Symbolizer;
use crate::win32::list_processes;

// ---------------------------------------------------------------------------
// Provider GUIDs & Keyword Constants
// ---------------------------------------------------------------------------

/// `SystemSchedulerProvider` (`CSwitch` 0x10, `ReadyThread` 0x20).
pub const SYSTEM_SCHEDULER_PROVIDER: GUID = GUID::from_u128(0x599a2a76_4d91_4910_9ac7_7d33f2e97a6c);
/// `SystemProfileProvider` (`SampledProfile` 0x1).
pub const SYSTEM_PROFILE_PROVIDER: GUID = GUID::from_u128(0xbfeb0324_1cee_496f_a409_2ac2b48a6322);
/// `SystemProcessProvider` (`Process` 0x1, `Thread` 0x2, `ImageLoad` 0x10).
pub const SYSTEM_PROCESS_PROVIDER: GUID = GUID::from_u128(0x151f55dc_467d_471f_83b5_5f889d46ff66);
/// `Microsoft-Windows-Kernel-Memory` (WS swap, VirtualAlloc/VirtualProtect).
pub const KERNEL_MEMORY_PROVIDER: GUID = GUID::from_u128(0xd1d93ef7_e1f2_4f45_9943_03d245fe6c00);
/// `Microsoft-Windows-Kernel-Process` (`Process` 0x10, `Thread` 0x20, `ImageLoad` 0x40).
pub const KERNEL_PROCESS_PROVIDER: GUID = GUID::from_u128(0x22fb2cd6_0e7b_422b_a0c7_2fad1fd0e716);

// Classic NT Kernel Logger MOF provider GUIDs (for offline WPR/xperf `.etl` files).
pub const CLASSIC_THREAD_GUID: GUID = GUID::from_u128(0x3d6fa8d1_fe05_11d0_9dda_00c04fd7ba7c);
pub const CLASSIC_PERFINFO_GUID: GUID = GUID::from_u128(0xce1dbb13_8028_49e3_8f34_8a4c7245047e);
pub const CLASSIC_IMAGE_GUID: GUID = GUID::from_u128(0x2cb15d1d_5fc1_11d2_abe1_00a0c911f518);
pub const CLASSIC_STACKWALK_GUID: GUID = GUID::from_u128(0xdef2fe46_7bd6_4b80_bd94_f57fe20d0ce3);

/// DxgKrnl keywords for unified QPC timeline:
/// `Base (0x1) | References (0x4) | Resource (0x40) | Memory (0x80) | LongHaul (0x800) | Present (0x0800_0000)`.
pub const DXGKRNL_KEYWORDS_TIMELINE: u64 = 0x0800_08C5;
/// `SystemMemoryProvider`: `HardFaults (0x2000) | VirtualAlloc (0x4000)`.
pub const SYSTEM_MEMORY_KW_TIMELINE: u64 = 0x6000;
/// `SystemSchedulerProvider`: `CSwitch (0x10) | ReadyThread (0x20)`.
pub const SYSTEM_SCHEDULER_KW_WAIT: u64 = 0x30;
/// `EVENT_ENABLE_PROPERTY_STACK_TRACE` (`0x4`).
pub const EVENT_ENABLE_PROPERTY_STACK_TRACE: u32 = 0x0000_0004;

const EVENT_HEADER_EXT_TYPE_STACK_TRACE32: u16 = 5;
const EVENT_HEADER_EXT_TYPE_STACK_TRACE64: u16 = 6;
const TRACE_LEVEL_INFORMATION: u8 = 4;

// TDH InType constants
const TDH_INTYPE_INT8: u16 = 3;
const TDH_INTYPE_UINT8: u16 = 4;
const TDH_INTYPE_INT16: u16 = 5;
const TDH_INTYPE_UINT16: u16 = 6;
const TDH_INTYPE_INT32: u16 = 7;
const TDH_INTYPE_UINT32: u16 = 8;
const TDH_INTYPE_INT64: u16 = 9;
const TDH_INTYPE_UINT64: u16 = 10;
const TDH_INTYPE_BOOLEAN: u16 = 13;
const TDH_INTYPE_POINTER: u16 = 16;
const TDH_INTYPE_SIZET: u16 = 19;
const TDH_INTYPE_HEXINT32: u16 = 20;
const TDH_INTYPE_HEXINT64: u16 = 21;

/// Returns the system QPC frequency (ticks per second).
pub fn qpc_frequency() -> u64 {
    let mut freq = 0i64;
    let _ = unsafe { QueryPerformanceFrequency(&mut freq) };
    if freq > 0 { freq as u64 } else { 10_000_000 }
}

/// Returns the current raw QPC counter value.
pub fn qpc_now() -> i64 {
    let mut now = 0i64;
    let _ = unsafe { QueryPerformanceCounter(&mut now) };
    now
}

/// Checks whether `pid` is still running (zero target suspends).
/// Returns `(alive, exit_code)` where `exit_code` is populated when the process has terminated
/// and its handle is still queryable. Treats `ERROR_ACCESS_DENIED` (5) on protected processes as alive.
pub(crate) fn probe_process_alive(pid: u32) -> (bool, Option<u32>) {
    const STILL_ACTIVE: u32 = 259;
    let h = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => h,
        Err(_) => {
            let err = std::io::Error::last_os_error().raw_os_error();
            return (err == Some(5), None);
        }
    };
    let mut code = 0u32;
    let ok = unsafe { GetExitCodeProcess(h, &mut code) }.is_ok();
    unsafe {
        let _ = CloseHandle(h);
    }
    if ok {
        if code == STILL_ACTIVE {
            (true, None)
        } else {
            (false, Some(code))
        }
    } else {
        (false, None)
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn parse_ctx_from_queue_label(label: &str) -> Option<u64> {
    let rest = label.strip_prefix("ctx=0x")?;
    let hex = rest.split('/').next()?;
    u64::from_str_radix(hex, 16).ok()
}

// ---------------------------------------------------------------------------
// Non-admin ETW Capability Probe
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCapability {
    pub name: String,
    pub guid: String,
    pub with_stack: bool,
    pub allowed: bool,
    pub win32_error: u32,
    pub note: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EtwCapabilitiesReport {
    pub system_logger_allowed: bool,
    pub providers: Vec<ProviderCapability>,
    pub one_time_setup_note: String,
}

/// Probes which ETW providers and stackwalk flags succeed under the current (non-admin) token.
/// Opens a temporary empty ETW session, tests `EnableTraceEx2`, and stops immediately (zero GPU work).
pub fn probe_etw_capabilities() -> Result<EtwCapabilitiesReport, String> {
    let name = format!("locum_mem_caps_{}", std::process::id());
    let etl = std::env::temp_dir().join(format!("{name}.etl"));
    let _ = std::fs::remove_file(&etl);

    let mut props = build_session_properties(
        &name,
        Some(&etl),
        16,
        EVENT_TRACE_FILE_MODE_SEQUENTIAL | EVENT_TRACE_SYSTEM_LOGGER_MODE,
    );
    let name_w = HSTRING::from(&name);
    let mut handle = CONTROLTRACE_HANDLE::default();
    let mut rc = unsafe { StartTraceW(&mut handle, &name_w, props.as_mut_ptr().cast()) };
    if rc == ERROR_ALREADY_EXISTS {
        stop_session_by_name(&name);
        rc = unsafe { StartTraceW(&mut handle, &name_w, props.as_mut_ptr().cast()) };
    }
    let mut sys_logger = rc == ERROR_SUCCESS;
    if !sys_logger {
        // Fallback to standard user-mode logger.
        props = build_session_properties(&name, Some(&etl), 16, EVENT_TRACE_FILE_MODE_SEQUENTIAL);
        rc = unsafe { StartTraceW(&mut handle, &name_w, props.as_mut_ptr().cast()) };
        if rc != ERROR_SUCCESS {
            return Err(format!(
                "StartTraceW failed with Win32 error {} (add account to Performance Log Users)",
                rc.0
            ));
        }
        sys_logger = false;
    }

    let probes: &[(&str, &str, GUID, u64, bool)] = &[
        (
            "DxgKrnl (queues/fences/presents/paging) + StackWalk",
            "{802EC45A-1E99-4B83-9920-87C98277BA9D}",
            DXGKRNL_PROVIDER,
            DXGKRNL_KEYWORDS_TIMELINE,
            true,
        ),
        (
            "SystemMemoryProvider (HardFaults/VirtualAlloc) + StackWalk",
            "{82958CA9-9C6D-490D-8364-592E28955110}",
            SYSTEM_MEMORY_PROVIDER,
            SYSTEM_MEMORY_KW_TIMELINE,
            true,
        ),
        (
            "Kernel-Memory (WS/VirtualProtect) + StackWalk",
            "{D1D93EF7-E1F2-4F45-9943-03D245FE6C00}",
            KERNEL_MEMORY_PROVIDER,
            0xFF,
            true,
        ),
        (
            "SystemSchedulerProvider (CSwitch/ReadyThread)",
            "{599A2A76-4D91-4910-9AC7-7D33F2E97A6C}",
            SYSTEM_SCHEDULER_PROVIDER,
            SYSTEM_SCHEDULER_KW_WAIT,
            true,
        ),
        (
            "SystemProfileProvider (SampledProfile)",
            "{BFEB0324-1CEE-496F-A409-2AC2B48A6322}",
            SYSTEM_PROFILE_PROVIDER,
            0x1,
            true,
        ),
        (
            "SystemProcessProvider (ImageLoad/Thread)",
            "{151F55DC-467D-471F-83B5-5F889D46FF66}",
            SYSTEM_PROCESS_PROVIDER,
            0x13,
            false,
        ),
        (
            "Kernel-Process (ImageLoad/Thread)",
            "{22FB2CD6-0E7B-422B-A0C7-2FAD1FD0E716}",
            KERNEL_PROCESS_PROVIDER,
            0x70,
            false,
        ),
    ];

    let mut providers = Vec::with_capacity(probes.len());
    for &(label, guid_str, guid, kw, with_stack) in probes {
        let ep = ENABLE_TRACE_PARAMETERS {
            Version: 2,
            EnableProperty: if with_stack {
                EVENT_ENABLE_PROPERTY_STACK_TRACE
            } else {
                0
            },
            ..Default::default()
        };
        let erc = unsafe {
            EnableTraceEx2(
                handle,
                &guid,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                TRACE_LEVEL_INFORMATION,
                kw,
                0,
                0,
                Some(&ep),
            )
        };
        let note = if erc == ERROR_SUCCESS {
            "available non-admin".into()
        } else if erc.0 == 5 {
            "ERROR_ACCESS_DENIED (5) non-admin; timeline uses DxgKrnl fence/wait + stack fallback or offline WPR/xperf ETL".into()
        } else {
            format!("Win32 error {}", erc.0)
        };
        providers.push(ProviderCapability {
            name: label.into(),
            guid: guid_str.into(),
            with_stack,
            allowed: erc == ERROR_SUCCESS,
            win32_error: erc.0,
            note,
        });
    }

    stop_session_handle(handle, &name);
    let _ = std::fs::remove_file(&etl);

    Ok(EtwCapabilitiesReport {
        system_logger_allowed: sys_logger,
        providers,
        one_time_setup_note: "Timeline and 10s Flight Recorder work 100% non-admin with Performance Log Users using DxgKrnl queues/fences/presents/TDR + SystemMemory + Kernel-Memory + inline StackWalk + TH32CS_SNAPMODULE/dbghelp. If CSwitch/ReadyThread/SampledProfile show ERROR_ACCESS_DENIED (5), Windows restricts those kernel providers via WMI\\Security ACLs (or SeSystemProfilePrivilege); you can still decode full CSwitch/ReadyThread/SampledProfile from any WPR/xperf .etl offline.".into(),
    })
}

pub fn format_capabilities_text(rep: &EtwCapabilitiesReport) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "locum-mem ETW capabilities | system_logger_mode={}",
        rep.system_logger_allowed
    );
    for p in &rep.providers {
        let status = if p.allowed { "OK  " } else { "DENY" };
        let _ = writeln!(
            out,
            "  [{status}] {:<58} rc={:<4} {}",
            p.name, p.win32_error, p.note
        );
    }
    let _ = writeln!(out, "\nNote: {}", rep.one_time_setup_note);
    out
}

// ---------------------------------------------------------------------------
// Unified QPC Timeline Data Structures
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimelineEventKind {
    GpuBusy,
    GpuSubmit,
    FenceSignal,
    FenceWait,
    Present,
    Paging,
    VirtualProtect,
    HardFault,
    CSwitchWait,
    CpuSample,
    DeviceRemoved,
    Tdr,
}

impl TimelineEventKind {
    pub fn tag(self) -> &'static str {
        match self {
            Self::GpuBusy => "GPU_BUSY",
            Self::GpuSubmit => "GPU_SUBMIT",
            Self::FenceSignal => "FENCE_SIG",
            Self::FenceWait => "FENCE_WAIT",
            Self::Present => "PRESENT",
            Self::Paging => "PAGING",
            Self::VirtualProtect => "VPROTECT",
            Self::HardFault => "HARD_FAULT",
            Self::CSwitchWait => "CSWITCH_WAIT",
            Self::CpuSample => "CPU_SAMPLE",
            Self::DeviceRemoved => "DEV_REMOVED",
            Self::Tdr => "TDR",
        }
    }

    pub fn parse_filter(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "gpu" | "gpu_busy" | "busy" => Ok(Self::GpuBusy),
            "submit" | "gpu_submit" => Ok(Self::GpuSubmit),
            "signal" | "fence_signal" | "fence_sig" => Ok(Self::FenceSignal),
            "wait" | "fence_wait" => Ok(Self::FenceWait),
            "present" | "flip" => Ok(Self::Present),
            "paging" | "page" => Ok(Self::Paging),
            "vprotect" | "virtual_protect" | "protect" | "valloc" => Ok(Self::VirtualProtect),
            "hard_fault" | "hardfault" | "hfault" => Ok(Self::HardFault),
            "cswitch" | "cswitch_wait" | "os_wait" => Ok(Self::CSwitchWait),
            "sample" | "cpu_sample" | "cpu" => Ok(Self::CpuSample),
            "dev_removed" | "device_removed" | "removed" => Ok(Self::DeviceRemoved),
            "tdr" => Ok(Self::Tdr),
            other => Err(format!(
                "unknown --kind {other:?} (expected: gpu_busy, submit, fence_signal, fence_wait, present, paging, vprotect, hard_fault, cswitch_wait, cpu_sample, tdr, device_removed)"
            )),
        }
    }
}

/// One event placed on the unified QPC timeline.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimelineEvent {
    pub qpc: i64,
    pub t_ms: f64,
    pub frame: u64,
    pub pid: u32,
    pub tid: u32,
    pub kind: TimelineEventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stack: Vec<String>,
}

/// Per-hardware-queue GPU busy & submission summary.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct QueueSummary {
    pub queue_id: String,
    pub context: u64,
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    pub node_ordinal: Option<u32>,
    pub engine_type: Option<u32>,
    pub packets_started: u64,
    pub packets_completed: u64,
    pub in_flight_at_end: u64,
    pub busy_ms: f64,
    pub busy_pct: f64,
    pub max_packet_ms: f64,
    pub fence_signals: u64,
    pub fence_waits: u64,
    /// True in ring-buffer flight dumps when no packet completed in the retained window and `in_flight_at_end == 0`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale_in_window: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sibling_note: Option<String>,
}

/// Per-thread wait attribution summary (CSwitch+ReadyThread when available, or DxgKrnl fence/MakeResident waits).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WaitSummary {
    pub tid: u32,
    pub kind: String,
    pub count: u64,
    /// Number of waits in `count` that had a measured start->stop duration (`0` = count-only event).
    pub measured_count: u64,
    /// Union wall-clock ms waited by this thread for this wait kind (non-overlapping; guaranteed `<= trace_dur_ms`).
    pub wall_ms: f64,
    /// Sum of individual wait durations (can exceed `trace_dur_ms` when multiple GPU waits overlap).
    pub total_ms: f64,
    pub max_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_waker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_stack: Option<String>,
}

/// Per-present frame slice on the unified QPC clock (intervals strictly clipped to `[start_qpc, end_qpc]`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FrameSummary {
    pub frame: u64,
    pub pid: u32,
    pub start_qpc: i64,
    pub end_qpc: i64,
    pub t_ms: f64,
    pub duration_ms: f64,
    /// Clipped union wall-clock GPU busy ms (any queue of `pid` busy; guaranteed `<= duration_ms`).
    pub gpu_busy_ms: f64,
    /// Clipped GPU idle ms in this frame (`duration_ms - gpu_busy_ms`, `>= 0`).
    pub gpu_idle_ms: f64,
    /// Clipped wall-clock busy ms of the single busiest queue of `pid` (`<= gpu_busy_ms <= duration_ms`).
    pub busiest_queue_ms: f64,
    /// Sum of clipped busy ms across all queues of `pid` (shows multi-queue overlap separately from saturation).
    pub queue_sum_ms: f64,
    /// Top GPU-submitting thread ID for `pid` (when known).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submit_tid: Option<u32>,
    /// Clipped CPU fence wait ms specifically for `submit_tid` (discriminates submit-thread blocking from background threads).
    pub submit_tid_cpu_wait_ms: f64,
    /// Clipped measured fence wait ms in this frame (`cpu_fence_wait_ms + gpu_fence_wait_ms`).
    pub fence_wait_ms: f64,
    pub cpu_fence_wait_ms: f64,
    pub gpu_fence_wait_ms: f64,
    pub os_wait_ms: f64,
    pub paging_events: u64,
    pub virtual_protect_events: u64,
    pub hard_faults: u64,
    pub presents: u64,
    pub present_event_id: u16,
}

/// Per-PID DxgKrnl activity summary (powers `--list-pids` and `pid=all` disambiguation).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PidActivitySummary {
    pub pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    pub dxg_events: u64,
    pub submits: u64,
    pub presents: u64,
    pub fence_waits: u64,
    pub fence_signals: u64,
    pub paging_events: u64,
}

/// Top symbolized CPU stack summary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackSummary {
    pub count: u64,
    pub kind: String,
    pub stack: String,
}

/// Detailed identity of an unresolved in-flight GPU packet (for flight-recorder stall diagnostics).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StalledPacketInfo {
    /// `"queue_pkt"`, `"dma_pkt"`, `"hwq_dma"`, or `"wait_pkt"`.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packet_type: Option<u64>,
    pub context: u64,
    pub queue: String,
    pub seq: u64,
    pub tid: u32,
    pub submit_qpc: i64,
    pub age_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync_obj: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fence_val: Option<u64>,
}

impl StalledPacketInfo {
    pub fn format_compact(&self) -> String {
        let mut s = match self.packet_type {
            Some(pt) => format!(
                "kind={}(type={pt}) queue={} ctx={:#x} seq={} tid={} submit_qpc={} age={:.2}s",
                self.kind, self.queue, self.context, self.seq, self.tid, self.submit_qpc, self.age_secs
            ),
            None => format!(
                "kind={} queue={} ctx={:#x} seq={} tid={} submit_qpc={} age={:.2}s",
                self.kind, self.queue, self.context, self.seq, self.tid, self.submit_qpc, self.age_secs
            ),
        };
        if let Some(sync) = self.sync_obj {
            s.push_str(&format!(" sync={sync:#x}"));
        }
        if let Some(val) = self.fence_val {
            s.push_str(&format!(" val={val}"));
        }
        s
    }
}

/// Trigger metadata when produced by the 10-second flight recorder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlightTriggerInfo {
    pub reason: String,
    pub trigger_qpc: i64,
    pub ring_secs: f64,
    /// Actual QPC duration retained in the dumped ring (can be less than `ring_secs` if `max_ring_events` cap hit).
    pub retained_secs: f64,
    pub max_ring_events: usize,
    pub capped_by_event_limit: bool,
    pub total_observed_events: u64,
    pub evicted_ring_events: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stalled_packet: Option<StalledPacketInfo>,
    /// Sibling-context stalled packets merged into the same trigger (prevents duplicate `--rearm` dumps).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sibling_stalled_packets: Vec<StalledPacketInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oldest_wait_packet: Option<StalledPacketInfo>,
}

/// Complete unified QPC timeline report (text-greppable + JSON).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TimelineReport {
    pub source: String,
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submit_tid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_exited_at_ms: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_exit_code: Option<u32>,
    pub thread_suspends: u64,
    pub ts_unit: String,
    pub qpc_freq_hz: u64,
    pub start_qpc: i64,
    pub end_qpc: i64,
    /// Total recording/ETL duration from first raw event to last raw event (ms).
    pub duration_ms: f64,
    /// First-to-last target-PID event span (ms).
    pub target_span_ms: f64,
    pub total_raw_events: u64,
    pub target_events: usize,
    pub ev_truncated: bool,
    pub dxg_pid_events: u64,
    pub dxg_ctx_other_pid_events: u64,
    pub stack_samples_captured: u64,
    pub measured_waits: u64,
    pub wait_attribution_mode: String,
    pub providers_active: Vec<String>,
    pub warnings: Vec<String>,
    pub alerts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flight_trigger: Option<FlightTriggerInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pids: Vec<PidActivitySummary>,
    pub queues: Vec<QueueSummary>,
    pub waits: Vec<WaitSummary>,
    pub frames: Vec<FrameSummary>,
    pub top_stacks: Vec<StackSummary>,
    pub events: Vec<TimelineEvent>,
}

/// Formatting options for greppable timeline text output.
#[derive(Clone, Debug)]
pub struct TimelineFormatOptions {
    pub min_ms: Option<f64>,
    pub kind: Option<TimelineEventKind>,
    pub tid: Option<u32>,
    pub top: usize,
    pub limit: usize,
    pub list_pids: bool,
}

impl Default for TimelineFormatOptions {
    fn default() -> Self {
        Self {
            min_ms: None,
            kind: None,
            tid: None,
            top: 20,
            limit: 120,
            list_pids: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Fast Zero-Clone Event Decoder (with PropertyParamCount dynamic arrays!)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PropPlan {
    name: String,
    elem_size: usize,
    count_prop_idx: Option<usize>,
    fixed_count: usize,
}

#[derive(Clone, Debug)]
struct CachedSchema {
    props: Vec<PropPlan>,
    /// Fixed-offset scalar fields before the first dynamic array (O(1) read!).
    fixed: HashMap<String, (usize, usize)>,
    has_dynamic: bool,
}

impl CachedSchema {
    fn from_record(rec: &EVENT_RECORD) -> Option<Self> {
        let ptr_size = if (rec.EventHeader.Flags & EVENT_HEADER_FLAG_32_BIT_HEADER as u16) != 0 {
            4usize
        } else {
            8usize
        };
        let mut buf_size = 0u32;
        let rc = unsafe { TdhGetEventInformation(rec, None, None, &mut buf_size) };
        if rc != ERROR_INSUFFICIENT_BUFFER.0 || buf_size == 0 {
            return None;
        }
        let mut buf = vec![0u8; buf_size as usize];
        let info = buf.as_mut_ptr().cast::<TRACE_EVENT_INFO>();
        let rc = unsafe { TdhGetEventInformation(rec, None, Some(info), &mut buf_size) };
        if rc != ERROR_SUCCESS.0 {
            return None;
        }
        let mut props = Vec::new();
        let mut fixed = HashMap::new();
        let mut has_dynamic = false;
        let top_count = unsafe { (*info).TopLevelPropertyCount } as usize;
        let props_ptr = unsafe {
            std::ptr::addr_of!((*info).EventPropertyInfoArray)
                .cast::<windows::Win32::System::Diagnostics::Etw::EVENT_PROPERTY_INFO>()
        };
        let mut offset = 0usize;
        for i in 0..top_count.min(32) {
            let pi = unsafe { &*props_ptr.add(i) };
            // Stop at struct properties (rare in DxgKrnl hot path).
            if (pi.Flags.0 & 0x1) != 0 {
                break;
            }
            let in_type = unsafe { pi.Anonymous1.nonStructType.InType };
            let fsize = match in_type {
                TDH_INTYPE_INT8 | TDH_INTYPE_UINT8 => 1,
                TDH_INTYPE_INT16 | TDH_INTYPE_UINT16 => 2,
                TDH_INTYPE_INT32 | TDH_INTYPE_UINT32 | TDH_INTYPE_BOOLEAN | TDH_INTYPE_HEXINT32 => 4,
                TDH_INTYPE_INT64 | TDH_INTYPE_UINT64 | TDH_INTYPE_HEXINT64 => 8,
                TDH_INTYPE_POINTER | TDH_INTYPE_SIZET => ptr_size,
                _ => {
                    let len = unsafe { pi.Anonymous3.length } as usize;
                    if len > 0 && (pi.Flags.0 & 0x2) == 0 {
                        len
                    } else {
                        break;
                    }
                }
            };
            let name = read_utf16_at(&buf, pi.NameOffset as usize);
            let is_param_count = (pi.Flags.0 & 0x4) != 0;
            let (count_prop_idx, fixed_count) = if is_param_count {
                let cidx = unsafe { pi.Anonymous2.countPropertyIndex } as usize;
                if cidx < props.len() {
                    has_dynamic = true;
                    (Some(cidx), 0)
                } else {
                    break;
                }
            } else {
                let c = unsafe { pi.Anonymous2.count } as usize;
                if c != 1 {
                    has_dynamic = true;
                }
                (None, c)
            };
            if !has_dynamic {
                fixed.insert(name.clone(), (offset, fsize));
                offset += fsize;
            }
            props.push(PropPlan {
                name,
                elem_size: fsize,
                count_prop_idx,
                fixed_count,
            });
        }
        Some(Self {
            props,
            fixed,
            has_dynamic,
        })
    }

    #[inline]
    fn read_scalar(data: &[u8], off: usize, size: usize) -> Option<u64> {
        if off + size > data.len() {
            return None;
        }
        let s = &data[off..off + size];
        match size {
            1 => Some(s[0] as u64),
            2 => Some(u16::from_le_bytes(s.try_into().ok()?) as u64),
            4 => Some(u32::from_le_bytes(s.try_into().ok()?) as u64),
            8 => Some(u64::from_le_bytes(s.try_into().ok()?)),
            _ => None,
        }
    }

    /// Evaluates all properties (including fields following `PropertyParamCount` dynamic arrays!)
    /// into a stack-allocated `[Option<u64>; 32]` with zero heap allocations.
    #[inline]
    fn eval_all(&self, data: &[u8]) -> [Option<u64>; 32] {
        let mut vals = [None; 32];
        let mut cursor = 0usize;
        for (i, p) in self.props.iter().enumerate() {
            let count = match p.count_prop_idx {
                Some(ci) => vals[ci].unwrap_or(0) as usize,
                None => p.fixed_count,
            };
            if count > 0 {
                vals[i] = Self::read_scalar(data, cursor, p.elem_size);
            }
            cursor = cursor.saturating_add(count.saturating_mul(p.elem_size));
            if cursor > data.len() {
                break;
            }
        }
        vals
    }

    #[inline]
    fn get_u64(&self, data: &[u8], name: &str) -> Option<u64> {
        if let Some(&(off, sz)) = self.fixed.get(name) {
            return Self::read_scalar(data, off, sz);
        }
        if !self.has_dynamic {
            return None;
        }
        let idx = self.props.iter().position(|p| p.name == name)?;
        self.eval_all(data)[idx]
    }

    #[inline]
    fn get_any_u64(&self, data: &[u8], names: &[&str]) -> Option<u64> {
        if !self.has_dynamic {
            for &n in names {
                if let Some(&(off, sz)) = self.fixed.get(n) {
                    return Self::read_scalar(data, off, sz);
                }
            }
            return None;
        }
        let vals = self.eval_all(data);
        for &n in names {
            if let Some(idx) = self.props.iter().position(|p| p.name == n)
                && let Some(v) = vals[idx]
            {
                return Some(v);
            }
        }
        None
    }

    #[inline]
    fn get_from_vals(&self, vals: &[Option<u64>; 32], names: &[&str]) -> Option<u64> {
        for &n in names {
            if let Some(idx) = self.props.iter().position(|p| p.name == n)
                && let Some(v) = vals[idx]
            {
                return Some(v);
            }
        }
        None
    }
}

fn read_utf16_at(buf: &[u8], off: usize) -> String {
    if off == 0 || off >= buf.len() {
        return String::new();
    }
    let mut u16s = Vec::new();
    let mut i = off;
    while i + 1 < buf.len() {
        let w = u16::from_le_bytes([buf[i], buf[i + 1]]);
        if w == 0 {
            break;
        }
        u16s.push(w);
        i += 2;
    }
    String::from_utf16_lossy(&u16s)
}

/// Extracts inline `STACK_TRACE64` / `STACK_TRACE32` instruction pointers from `EVENT_RECORD.ExtendedData`.
pub fn extract_inline_stack(rec: &EVENT_RECORD) -> Vec<u64> {
    if rec.ExtendedDataCount == 0 || rec.ExtendedData.is_null() {
        return Vec::new();
    }
    let items = unsafe { std::slice::from_raw_parts(rec.ExtendedData, rec.ExtendedDataCount as usize) };
    for item in items {
        if item.DataPtr == 0 || item.DataSize <= 8 {
            continue;
        }
        if item.ExtType == EVENT_HEADER_EXT_TYPE_STACK_TRACE64 {
            let byte_len = (item.DataSize as usize).saturating_sub(8);
            let n = (byte_len / 8).min(64);
            let ptr = (item.DataPtr as usize + 8) as *const u64;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let addr = unsafe { ptr.add(i).read_unaligned() };
                if addr != 0 {
                    out.push(addr);
                }
            }
            return out;
        } else if item.ExtType == EVENT_HEADER_EXT_TYPE_STACK_TRACE32 {
            let byte_len = (item.DataSize as usize).saturating_sub(8);
            let n = (byte_len / 4).min(64);
            let ptr = (item.DataPtr as usize + 8) as *const u32;
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let addr = unsafe { ptr.add(i).read_unaligned() } as u64;
                if addr != 0 {
                    out.push(addr);
                }
            }
            return out;
        }
    }
    Vec::new()
}

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct SchemaKey {
    provider: u128,
    id: u16,
    version: u8,
    is_32bit: bool,
}

#[derive(Clone, Debug)]
struct ContextMeta {
    pid: u32,
    node_ordinal: Option<u32>,
    engine_type: Option<u32>,
}

/// Pre-symbolization timeline event (keeps raw instruction pointers until report finalization).
#[derive(Clone, Debug)]
pub(crate) struct RawTimelineEvent {
    pub qpc: i64,
    pub frame: u64,
    pub pid: u32,
    pub tid: u32,
    pub kind: TimelineEventKind,
    pub queue: Option<String>,
    pub duration_qpc: Option<i64>,
    pub detail: String,
    pub raw_stack: Vec<u64>,
    pub waker_tid: Option<u32>,
    pub wait_kind: Option<String>,
}

#[derive(Clone, Debug)]
struct PendingFenceWait {
    wait_qpc: i64,
    pid: u32,
    tid: u32,
    queue: Option<String>,
    sync_obj: u64,
    wait_val: u64,
    is_cpu: bool,
    event_handle: u64,
    raw_stack: Vec<u64>,
}

/// `(total_count, measured_count, total_ms, max_ms, waker_counts, stack_counts)`
type WaitAccEntry = (u64, u64, f64, f64, HashMap<u32, u64>, HashMap<String, u64>);
type InFlightWaitPkt = (i64, u32, u32, u64, u64);

/// Stateful streaming QPC timeline builder used by both offline `.etl` parsing and the live 10s flight ring.
pub(crate) struct TimelineBuilder {
    pub filter_pid: Option<u32>,
    pub qpc_freq_hz: u64,
    pub ts_unit: String,
    schemas: HashMap<SchemaKey, Option<Arc<CachedSchema>>>,
    pub start_qpc: Option<i64>,
    pub end_qpc: Option<i64>,
    pub target_start_qpc: Option<i64>,
    pub target_end_qpc: Option<i64>,
    pub total_raw_events: u64,
    pub dxg_pid_events: u64,
    pub dxg_ctx_other_pid_events: u64,
    pub stack_samples_captured: u64,
    pub saw_cswitch: bool,
    pub saw_ready_thread: bool,
    pub saw_sampled_profile: bool,
    pub saw_dxgkrnl: bool,
    pub saw_sysmem: bool,
    pub saw_kmem: bool,

    // Persistent ownership maps (kept across ring evictions)
    device_pid: HashMap<u64, u32>,
    context_meta: HashMap<u64, ContextMeta>,
    hwqueue_ctx: HashMap<u64, u64>,
    /// `pDxgSyncObject` / `KMTHandle` -> canonical `VidSchSyncObject` (from DxgKrnl id=1070)
    sync_alias: HashMap<u64, u64>,
    /// `(hContext, submit_seq)` -> `(start_qpc, pid, tid, packet_type)` for QueuePacket (178)
    in_flight_queue_pkts: HashMap<(u64, u64), (i64, u32, u32, u64)>,
    /// `(hContext, submit_seq)` -> `(start_qpc, pid, tid, sync, val)` for QueuePacket Wait (244)
    in_flight_wait_pkts: HashMap<(u64, u64), InFlightWaitPkt>,
    /// `(hContext, submit_seq)` -> `(pid, tid, sync, val)` for QueuePacket Signal (245)
    in_flight_sig_pkts: HashMap<(u64, u64), (u32, u32, u64, u64)>,
    /// `(hContext, submit_seq)` -> `(start_qpc, pid, tid)` for DmaPacket (175->176)
    in_flight_dma_pkts: HashMap<(u64, u64), (i64, u32, u32)>,
    /// `(hHwQueue, progress_fence)` -> `(start_qpc, pid, tid)` for HAGS DmaPacket (450->451)
    in_flight_hwq_pkts: HashMap<(u64, u64), (i64, u32, u32)>,
    /// Pending CPU fence waits keyed by kernel `Event` pointer (`297` -> `300`)
    pending_cpu_waits_by_event: HashMap<u64, PendingFenceWait>,
    /// Pending CPU/GPU fence waits keyed by canonical `sync_obj` (`297`/`100`/`295`/`231` -> `300`/`550`/`551`/`294`/`296`/`245+180`)
    pending_waits_by_sync: HashMap<u64, Vec<PendingFenceWait>>,
    /// `tid` -> `(switch_out_qpc, stack)` for CSwitch wait attribution
    off_cpu_threads: HashMap<u32, (i64, Vec<u64>)>,
    /// `tid` -> `waker_tid` from ReadyThread
    last_waker: HashMap<u32, u32>,
    /// ETW `ImageLoad` modules observed for offline symbolization
    pub etw_images: Vec<(u64, u64, String)>,
    /// Learned PID -> process name (from live ToolHelp + ETW ImageLoad `.exe`)
    pid_names: HashMap<u32, String>,

    pub current_frame: u64,
    pub last_present_qpc: Option<i64>,
    pub last_submit_qpc: Option<i64>,
    pub last_complete_qpc: Option<i64>,
    pub total_target_submits: u64,
    pub tdr_or_removal_alert: Option<(i64, String)>,

    pub alerts: Vec<String>,
    pub pid_stats: HashMap<u32, PidActivitySummary>,
    pub queues: BTreeMap<u64, QueueSummary>,
    /// Modal PID counts observed per `hContext` (recovers true owning PID when `DC_Start` arrives late).
    ctx_pid_counts: HashMap<u64, HashMap<u32, u64>>,
    /// Per-`(pid, tid)` GPU packet submit counts (identifies the top submitting thread for `frame_pid`).
    pid_tid_submits: HashMap<(u32, u32), u64>,

    // Compact full-trace accumulators (used in offline / non-ring mode so 9M+ event ETLs never lose summary accuracy!)
    full_gpu_spans: Vec<(i64, i64, u32, u64)>,
    /// `(start_qpc, end_qpc, pid, tid, kind_code)` where `0=cpu_fence, 1=gpu_fence, 2=cswitch`
    full_wait_spans: Vec<(i64, i64, u32, u32, u8)>,
    /// `(qpc, pid, event_id)` for canonical Present deduplication
    full_present_marks: Vec<(i64, u32, u16)>,
    /// `(qpc, pid, aux_code)` where `0=paging, 1=vprot, 2=hfault`
    full_aux_marks: Vec<(i64, u32, u8)>,
    full_wait_acc: BTreeMap<(u32, String), WaitAccEntry>,
    full_measured_waits: u64,

    pub raw_events: VecDeque<RawTimelineEvent>,
    /// Optional ring capacity (`None` = keep up to 200k EV events; `Some(ring_qpc_window)` = evict older).
    pub ring_window_qpc: Option<i64>,
    pub max_retained_events: usize,
    pub evicted_events: u64,
    pub ev_truncated: bool,
}

impl TimelineBuilder {
    pub fn new(filter_pid: Option<u32>, qpc_freq_hz: u64) -> Self {
        let mut pid_names = HashMap::new();
        if let Ok(procs) = list_processes() {
            for p in procs {
                pid_names.insert(p.pid, p.exe_name);
            }
        }
        Self {
            filter_pid,
            qpc_freq_hz: qpc_freq_hz.max(1),
            ts_unit: "qpc".into(),
            schemas: HashMap::new(),
            start_qpc: None,
            end_qpc: None,
            target_start_qpc: None,
            target_end_qpc: None,
            total_raw_events: 0,
            dxg_pid_events: 0,
            dxg_ctx_other_pid_events: 0,
            stack_samples_captured: 0,
            saw_cswitch: false,
            saw_ready_thread: false,
            saw_sampled_profile: false,
            saw_dxgkrnl: false,
            saw_sysmem: false,
            saw_kmem: false,
            device_pid: HashMap::new(),
            context_meta: HashMap::new(),
            hwqueue_ctx: HashMap::new(),
            sync_alias: HashMap::new(),
            in_flight_queue_pkts: HashMap::new(),
            in_flight_wait_pkts: HashMap::new(),
            in_flight_sig_pkts: HashMap::new(),
            in_flight_dma_pkts: HashMap::new(),
            in_flight_hwq_pkts: HashMap::new(),
            pending_cpu_waits_by_event: HashMap::new(),
            pending_waits_by_sync: HashMap::new(),
            off_cpu_threads: HashMap::new(),
            last_waker: HashMap::new(),
            etw_images: Vec::new(),
            pid_names,
            current_frame: 0,
            last_present_qpc: None,
            last_submit_qpc: None,
            last_complete_qpc: None,
            total_target_submits: 0,
            tdr_or_removal_alert: None,
            alerts: Vec::new(),
            pid_stats: HashMap::new(),
            queues: BTreeMap::new(),
            ctx_pid_counts: HashMap::new(),
            pid_tid_submits: HashMap::new(),
            full_gpu_spans: Vec::new(),
            full_wait_spans: Vec::new(),
            full_present_marks: Vec::new(),
            full_aux_marks: Vec::new(),
            full_wait_acc: BTreeMap::new(),
            full_measured_waits: 0,
            raw_events: VecDeque::new(),
            ring_window_qpc: None,
            max_retained_events: 200_000,
            evicted_events: 0,
            ev_truncated: false,
        }
    }

    pub fn with_ring(mut self, ring_secs: f64, max_events: usize) -> Self {
        self.ring_window_qpc = Some((ring_secs.max(0.5) * self.qpc_freq_hz as f64) as i64);
        self.max_retained_events = max_events.max(1024);
        self
    }

    #[inline]
    fn matches_pid(&self, pid: u32) -> bool {
        match self.filter_pid {
            Some(want) => pid == want,
            None => true,
        }
    }

    #[inline]
    fn canonical_sync(&self, sync: u64) -> u64 {
        if sync == 0 {
            return 0;
        }
        self.sync_alias.get(&sync).copied().unwrap_or(sync)
    }

    #[inline]
    fn note_target_qpc(&mut self, qpc: i64) {
        if qpc > 0 {
            if self.target_start_qpc.is_none() {
                self.target_start_qpc = Some(qpc);
            }
            self.target_end_qpc = Some(qpc);
        }
    }

    #[inline]
    fn bump_pid(&mut self, pid: u32) -> &mut PidActivitySummary {
        let name = self.pid_names.get(&pid).cloned();
        self.pid_stats.entry(pid).or_insert_with(|| PidActivitySummary {
            pid,
            process_name: name,
            ..Default::default()
        })
    }

    #[inline]
    fn record_wait_acc(
        &mut self,
        tid: u32,
        kind: &str,
        dur_qpc: Option<i64>,
        waker_tid: Option<u32>,
        increment_count: bool,
    ) {
        if self.ring_window_qpc.is_some() {
            return;
        }
        let e = self
            .full_wait_acc
            .entry((tid, kind.to_string()))
            .or_insert_with(|| (0, 0, 0.0, 0.0, HashMap::new(), HashMap::new()));
        if increment_count {
            e.0 += 1;
        }
        if let Some(dq) = dur_qpc {
            let ms = dq as f64 * 1e3 / self.qpc_freq_hz as f64;
            e.1 += 1;
            if e.1 > e.0 {
                e.0 = e.1;
            }
            e.2 += ms;
            if ms > e.3 {
                e.3 = ms;
            }
            self.full_measured_waits += 1;
        }
        if let Some(w) = waker_tid {
            *e.4.entry(w).or_insert(0) += 1;
        }
    }

    #[inline]
    fn schema_for(&mut self, rec: &EVENT_RECORD) -> Option<Arc<CachedSchema>> {
        let key = SchemaKey {
            provider: rec.EventHeader.ProviderId.to_u128(),
            id: rec.EventHeader.EventDescriptor.Id,
            version: rec.EventHeader.EventDescriptor.Version,
            is_32bit: (rec.EventHeader.Flags & EVENT_HEADER_FLAG_32_BIT_HEADER as u16) != 0,
        };
        self.schemas
            .entry(key)
            .or_insert_with(|| CachedSchema::from_record(rec).map(Arc::new))
            .clone()
    }

    fn queue_label(&self, ctx: u64) -> String {
        if let Some(m) = self.context_meta.get(&ctx)
            && let Some(node) = m.node_ordinal
        {
            return format!("ctx={ctx:#x}/n{node}");
        }
        format!("ctx={ctx:#x}")
    }

    #[inline]
    fn note_ctx_pid(&mut self, ctx: u64, pid: u32) {
        if ctx != 0 && pid != 0 && pid != 4 {
            *self
                .ctx_pid_counts
                .entry(ctx)
                .or_default()
                .entry(pid)
                .or_insert(0) += 1;
        }
    }

    #[inline]
    fn note_submit_tid(&mut self, pid: u32, tid: u32) {
        if pid != 0 && pid != 4 && tid != 0 {
            *self.pid_tid_submits.entry((pid, tid)).or_insert(0) += 1;
        }
    }

    fn ensure_queue(&mut self, ctx: u64, pid_hint: u32) -> &mut QueueSummary {
        let meta = self.context_meta.get(&ctx).cloned();
        let pid = meta
            .as_ref()
            .map(|m| m.pid)
            .filter(|&p| p != 0)
            .unwrap_or(pid_hint);
        let pname = self.pid_names.get(&pid).cloned();
        let label = self.queue_label(ctx);
        let q = self.queues.entry(ctx).or_insert_with(|| QueueSummary {
            queue_id: label,
            context: ctx,
            pid,
            process_name: pname,
            node_ordinal: meta.as_ref().and_then(|m| m.node_ordinal),
            engine_type: meta.as_ref().and_then(|m| m.engine_type),
            ..Default::default()
        });
        if q.pid == 0 && pid != 0 {
            q.pid = pid;
            q.process_name = self.pid_names.get(&pid).cloned();
        }
        if q.node_ordinal.is_none() {
            q.node_ordinal = meta.as_ref().and_then(|m| m.node_ordinal);
        }
        q
    }

    fn push_event(&mut self, ev: RawTimelineEvent) {
        self.note_target_qpc(ev.qpc);
        if !ev.raw_stack.is_empty() {
            self.stack_samples_captured += 1;
        }
        if let Some(win) = self.ring_window_qpc {
            self.raw_events.push_back(ev);
            if let Some(end) = self.end_qpc {
                let cutoff = end.saturating_sub(win);
                while self.raw_events.len() > self.max_retained_events
                    || self.raw_events.front().is_some_and(|e| e.qpc < cutoff)
                {
                    self.raw_events.pop_front();
                    self.evicted_events += 1;
                }
            }
        } else {
            if self.raw_events.len() >= self.max_retained_events {
                self.raw_events.pop_front();
                self.evicted_events += 1;
                self.ev_truncated = true;
            }
            self.raw_events.push_back(ev);
        }
    }

    /// Returns the oldest unresolved GPU work packet (`queue_pkt` / `dma_pkt` / `hwq_dma`) together
    /// with any sibling-context in-flight work packets belonging to the same stall event (same
    /// submitting thread within 50 ms, OR lockstep sibling context sharing `(packets_started,
    /// packets_completed)` within 500 ms).
    ///
    /// Skips any `(ctx, seq)` already acknowledged in `ack_pkts` AND any later packet queued on a
    /// context that already has an acknowledged still-in-flight packet (FIFO queue blocked behind
    /// the same stall).
    pub fn oldest_in_flight_work_packet_group(
        &self,
        now_qpc: i64,
        ack_pkts: Option<&HashSet<(u64, u64)>>,
    ) -> Option<(StalledPacketInfo, Vec<StalledPacketInfo>)> {
        let mut acked_live_ctxs: HashSet<u64> = HashSet::new();
        if let Some(acks) = ack_pkts {
            for &(ctx, seq) in self.in_flight_queue_pkts.keys() {
                if acks.contains(&(ctx, seq)) {
                    acked_live_ctxs.insert(ctx);
                }
            }
            for &(ctx, seq) in self.in_flight_dma_pkts.keys() {
                if acks.contains(&(ctx, seq)) {
                    acked_live_ctxs.insert(ctx);
                }
            }
            for &(hq, pf) in self.in_flight_hwq_pkts.keys() {
                let ctx = self.hwqueue_ctx.get(&hq).copied().unwrap_or(hq);
                if acks.contains(&(ctx, pf)) {
                    acked_live_ctxs.insert(ctx);
                }
            }
        }

        let freq = self.qpc_freq_hz.max(1) as f64;
        let mut all: Vec<StalledPacketInfo> = Vec::new();
        for (&(ctx, seq), &(start, _, tid, ptype)) in &self.in_flight_queue_pkts {
            if ack_pkts.is_some_and(|a| a.contains(&(ctx, seq))) || acked_live_ctxs.contains(&ctx) {
                continue;
            }
            all.push(StalledPacketInfo {
                kind: "queue_pkt".into(),
                packet_type: Some(ptype),
                context: ctx,
                queue: self.queue_label(ctx),
                seq,
                tid,
                submit_qpc: start,
                age_secs: (now_qpc - start).max(0) as f64 / freq,
                sync_obj: None,
                fence_val: None,
            });
        }
        for (&(ctx, seq), &(start, _, tid)) in &self.in_flight_dma_pkts {
            if ack_pkts.is_some_and(|a| a.contains(&(ctx, seq))) || acked_live_ctxs.contains(&ctx) {
                continue;
            }
            all.push(StalledPacketInfo {
                kind: "dma_pkt".into(),
                packet_type: None,
                context: ctx,
                queue: self.queue_label(ctx),
                seq,
                tid,
                submit_qpc: start,
                age_secs: (now_qpc - start).max(0) as f64 / freq,
                sync_obj: None,
                fence_val: None,
            });
        }
        for (&(hq, pf), &(start, _, tid)) in &self.in_flight_hwq_pkts {
            let ctx = self.hwqueue_ctx.get(&hq).copied().unwrap_or(hq);
            if ack_pkts.is_some_and(|a| a.contains(&(ctx, pf))) || acked_live_ctxs.contains(&ctx) {
                continue;
            }
            all.push(StalledPacketInfo {
                kind: "hwq_dma".into(),
                packet_type: None,
                context: ctx,
                queue: self.queue_label(ctx),
                seq: pf,
                tid,
                submit_qpc: start,
                age_secs: (now_qpc - start).max(0) as f64 / freq,
                sync_obj: None,
                fence_val: None,
            });
        }
        if all.is_empty() {
            return None;
        }
        all.sort_by_key(|p| (p.submit_qpc, p.context, p.seq));
        let primary = all.remove(0);

        let same_tid_win = (self.qpc_freq_hz as i64) / 20; // 50 ms
        let lockstep_win = (self.qpc_freq_hz as i64) / 2; // 500 ms
        let primary_counts = self
            .queues
            .get(&primary.context)
            .map(|q| (q.packets_started, q.packets_completed));

        let mut siblings = Vec::new();
        for p in all {
            if p.context == primary.context {
                continue;
            }
            let dt = (p.submit_qpc - primary.submit_qpc).abs();
            let same_tid_pair = primary.tid != 0 && p.tid == primary.tid && dt <= same_tid_win;
            let lockstep_pair = dt <= lockstep_win
                && primary_counts.is_some_and(|(ps, pc)| {
                    ps >= 2
                        && self
                            .queues
                            .get(&p.context)
                            .is_some_and(|q| q.packets_started == ps && q.packets_completed == pc)
                });
            if same_tid_pair || lockstep_pair {
                siblings.push(p);
            }
        }

        Some((primary, siblings))
    }

    /// Returns detailed identity of the oldest unresolved queue-side fence wait packet (`244` -> `180`), if any.
    pub fn oldest_in_flight_wait_packet(&self, now_qpc: i64) -> Option<StalledPacketInfo> {
        let freq = self.qpc_freq_hz.max(1) as f64;
        self.in_flight_wait_pkts
            .iter()
            .min_by_key(|&(_, &(start, _, _, _, _))| start)
            .map(|(&(ctx, seq), &(start, _, tid, sync, val))| StalledPacketInfo {
                kind: "wait_pkt".into(),
                packet_type: None,
                context: ctx,
                queue: self.queue_label(ctx),
                seq,
                tid,
                submit_qpc: start,
                age_secs: (now_qpc - start).max(0) as f64 / freq,
                sync_obj: (sync != 0).then_some(sync),
                fence_val: Some(val),
            })
    }

    /// Prunes stale in-flight entries on `ctx` with sequence strictly earlier than `completed_seq`
    /// (FIFO hardware queues cannot leave an older sequence permanently in-flight once newer packets complete).
    #[inline]
    fn prune_older_in_flight_seq<V>(map: &mut HashMap<(u64, u64), V>, ctx: u64, completed_seq: u64) {
        if completed_seq == 0 || map.is_empty() {
            return;
        }
        map.retain(|&(c, s), _| {
            if c != ctx || s == 0 {
                return true;
            }
            let diff = completed_seq.wrapping_sub(s);
            !(diff > 0 && diff < (1u64 << 31))
        });
    }

    /// Returns the actual QPC span currently held in the ring buffer, in seconds.
    pub fn retained_ring_secs(&self) -> f64 {
        match (self.raw_events.front(), self.raw_events.back()) {
            (Some(f), Some(b)) => (b.qpc - f.qpc).max(0) as f64 / self.qpc_freq_hz.max(1) as f64,
            _ => 0.0,
        }
    }

    /// Ingests one ETW `EVENT_RECORD` onto the unified QPC timeline.
    pub fn on_event(&mut self, rec: &EVENT_RECORD) {
        self.total_raw_events += 1;
        let qpc = rec.EventHeader.TimeStamp;
        if qpc > 0 {
            if self.start_qpc.is_none() {
                self.start_qpc = Some(qpc);
            }
            self.end_qpc = Some(qpc);
        }

        let prov = rec.EventHeader.ProviderId;
        if prov == DXGKRNL_PROVIDER {
            self.saw_dxgkrnl = true;
            self.on_dxgkrnl(rec, qpc);
        } else if prov == SYSTEM_MEMORY_PROVIDER || prov == CLASSIC_PAGEFAULT_GUID {
            self.saw_sysmem = true;
            self.on_memory_event(rec, qpc);
        } else if prov == KERNEL_MEMORY_PROVIDER {
            self.saw_kmem = true;
            self.on_kernel_memory(rec, qpc);
        } else if prov == SYSTEM_SCHEDULER_PROVIDER || prov == CLASSIC_THREAD_GUID {
            self.on_scheduler_event(rec, qpc);
        } else if prov == SYSTEM_PROFILE_PROVIDER || prov == CLASSIC_PERFINFO_GUID {
            self.on_profile_event(rec, qpc);
        } else if prov == SYSTEM_PROCESS_PROVIDER
            || prov == KERNEL_PROCESS_PROVIDER
            || prov == CLASSIC_IMAGE_GUID
        {
            self.on_image_event(rec);
        } else if prov == CLASSIC_STACKWALK_GUID {
            self.on_classic_stackwalk(rec);
        }
    }

    fn resolve_waits_for_sync(&mut self, sync: u64, sig_val: u64, qpc: i64, waker_tid: u32) {
        let canon = self.canonical_sync(sync);
        if canon == 0 {
            return;
        }
        let Some(mut waiters) = self.pending_waits_by_sync.remove(&canon) else {
            return;
        };
        let mut still_pending = Vec::new();
        for w in waiters.drain(..) {
            if w.wait_val == 0 || sig_val == 0 || sig_val >= w.wait_val {
                if w.event_handle != 0 {
                    self.pending_cpu_waits_by_event.remove(&w.event_handle);
                }
                self.emit_resolved_wait(w, sig_val, qpc, waker_tid);
            } else {
                still_pending.push(w);
            }
        }
        if !still_pending.is_empty() {
            self.pending_waits_by_sync.insert(canon, still_pending);
        }
    }

    fn emit_resolved_wait(&mut self, w: PendingFenceWait, sig_val: u64, end_qpc: i64, waker_tid: u32) {
        let dur_qpc = (end_qpc - w.wait_qpc).max(0);
        if dur_qpc > (self.qpc_freq_hz as i64).saturating_mul(30) {
            return;
        }
        let kind_name = if w.is_cpu {
            "dxg_fence_cpu_wait"
        } else {
            "dxg_fence_gpu_wait"
        };
        let waker = (waker_tid != 0).then_some(waker_tid);
        if self.ring_window_qpc.is_none() {
            self.full_wait_spans
                .push((w.wait_qpc, end_qpc, w.pid, w.tid, if w.is_cpu { 0 } else { 1 }));
            // `count` was already incremented when the wait started (`297`/`100`/`295`/`231`);
            // record the measured duration without double-counting `n`.
            self.record_wait_acc(w.tid, kind_name, Some(dur_qpc), waker, false);
        }
        self.push_event(RawTimelineEvent {
            qpc: w.wait_qpc,
            frame: self.current_frame,
            pid: w.pid,
            tid: w.tid,
            kind: TimelineEventKind::FenceWait,
            queue: w.queue,
            duration_qpc: Some(dur_qpc),
            detail: format!(
                "{} sync={:#x} val={}->{} by_tid={}",
                if w.is_cpu {
                    "cpu_wait_done"
                } else {
                    "gpu_wait_done"
                },
                w.sync_obj,
                w.wait_val,
                sig_val,
                waker_tid
            ),
            raw_stack: w.raw_stack,
            waker_tid: waker,
            wait_kind: Some(kind_name.into()),
        });
    }

    fn note_dxg_pid_match(&mut self, resolved_pid: u32, hdr_pid: u32) {
        if let Some(want) = self.filter_pid {
            if resolved_pid == want {
                if hdr_pid == want {
                    self.dxg_pid_events += 1;
                } else {
                    self.dxg_ctx_other_pid_events += 1;
                }
            }
        } else {
            self.dxg_pid_events += 1;
        }
    }

    fn on_dxgkrnl(&mut self, rec: &EVENT_RECORD, qpc: i64) {
        let id = rec.EventHeader.EventDescriptor.Id;
        if !matches!(
            id,
            27..=32
                | 47
                | 71
                | 73
                | 74
                | 100
                | 101
                | 171
                | 175..=176
                | 178
                | 180
                | 184
                | 230
                | 231
                | 242
                | 244
                | 245
                | 294..=297
                | 300
                | 320
                | 321
                | 338
                | 374
                | 422..=424
                | 450
                | 451
                | 462
                | 469
                | 470
                | 540
                | 541
                | 547
                | 548
                | 550
                | 551
                | 1070
        ) {
            return;
        }
        let opcode = rec.EventHeader.EventDescriptor.Opcode;
        let data = if rec.UserData.is_null() || rec.UserDataLength == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(rec.UserData.cast::<u8>(), rec.UserDataLength as usize) }
        };
        let hdr_pid = rec.EventHeader.ProcessId;
        let hdr_tid = rec.EventHeader.ThreadId;
        let Some(schema) = self.schema_for(rec) else {
            return;
        };

        match id {
            // Device Start/Stop/DC_Start
            27..=29 => {
                let hdev = schema.get_u64(data, "hDevice").unwrap_or(0);
                let pid = schema
                    .get_any_u64(data, &["hProcessId", "ProcessId"])
                    .map(|v| v as u32)
                    .filter(|&p| p != 0)
                    .unwrap_or(hdr_pid);
                if hdev != 0 && pid != 0 {
                    self.device_pid.insert(hdev, pid);
                }
            }
            // Context Start/Stop/DC_Start
            30..=32 => {
                let hctx = schema
                    .get_any_u64(data, &["hContext", "pDxgContext"])
                    .unwrap_or(0);
                let hdev = schema.get_u64(data, "hDevice").unwrap_or(0);
                let mut pid = schema
                    .get_any_u64(data, &["hProcessId", "ProcessId"])
                    .map(|v| v as u32)
                    .unwrap_or(0);
                if pid == 0 && hdev != 0 {
                    pid = self.device_pid.get(&hdev).copied().unwrap_or(0);
                }
                if pid == 0 && hdr_pid != 4 {
                    pid = hdr_pid;
                }
                let node = schema
                    .get_any_u64(data, &["NodeOrdinal", "ulNodeOrdinal"])
                    .map(|v| v as u32);
                let engine = schema
                    .get_any_u64(data, &["ContextType", "EngineAffinity"])
                    .map(|v| v as u32);
                if hctx != 0 {
                    if id == 31 {
                        // Context Stop: clear any orphaned in-flight packets on the destroyed context.
                        self.in_flight_queue_pkts.retain(|&(c, _), _| c != hctx);
                        self.in_flight_wait_pkts.retain(|&(c, _), _| c != hctx);
                        self.in_flight_sig_pkts.retain(|&(c, _), _| c != hctx);
                        self.in_flight_dma_pkts.retain(|&(c, _), _| c != hctx);
                    }
                    self.context_meta.insert(
                        hctx,
                        ContextMeta {
                            pid,
                            node_ordinal: node,
                            engine_type: engine,
                        },
                    );
                }
            }
            // HwQueue Start/Stop/DC_Start
            422..=424 => {
                let hq = schema.get_u64(data, "hHwQueue").unwrap_or(0);
                let hctx = schema
                    .get_any_u64(data, &["hContext", "hParentContext"])
                    .unwrap_or(0);
                if hq != 0 && id == 423 {
                    self.in_flight_hwq_pkts.retain(|&(q, _), _| q != hq);
                }
                if hq != 0 && hctx != 0 {
                    self.hwqueue_ctx.insert(hq, hctx);
                }
            }
            // SyncObject handle alias map (`pSyncObject`, `KMTHandle`, `pDxgSyncObject`)
            1070 => {
                let vsync = schema.get_u64(data, "pSyncObject").unwrap_or(0);
                let dsync = schema.get_u64(data, "pDxgSyncObject").unwrap_or(0);
                let kmt = schema.get_u64(data, "KMTHandle").unwrap_or(0);
                if vsync != 0 {
                    if dsync != 0 {
                        self.sync_alias.insert(dsync, vsync);
                    }
                    if kmt != 0 {
                        self.sync_alias.insert(kmt, vsync);
                    }
                }
            }
            // QueuePacket Start (178 = Render/Compute DMA)
            178 => {
                let hq = schema.get_u64(data, "hHwQueue").unwrap_or(0);
                let mut ctx = schema.get_u64(data, "hContext").unwrap_or(0);
                if ctx == 0 && hq != 0 {
                    ctx = self.hwqueue_ctx.get(&hq).copied().unwrap_or(hq);
                }
                let seq = schema
                    .get_any_u64(data, &["SubmitSequence", "ulSubmitSequence"])
                    .unwrap_or(0);
                let ptype = schema.get_u64(data, "PacketType").unwrap_or(0);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .unwrap_or(hdr_pid);
                if pid != 0 {
                    let ps = self.bump_pid(pid);
                    ps.dxg_events += 1;
                    ps.submits += 1;
                }
                if !self.matches_pid(pid) {
                    return;
                }
                self.note_dxg_pid_match(pid, hdr_pid);
                self.note_submit_tid(pid, hdr_tid);
                self.note_ctx_pid(ctx, pid);
                self.last_submit_qpc = Some(qpc);
                self.total_target_submits += 1;
                if ctx != 0 {
                    self.ensure_queue(ctx, pid).packets_started += 1;
                    self.in_flight_queue_pkts
                        .insert((ctx, seq), (qpc, pid, hdr_tid, ptype));
                }
                let stack = extract_inline_stack(rec);
                if !stack.is_empty() {
                    let qname = self.queue_label(ctx);
                    self.push_event(RawTimelineEvent {
                        qpc,
                        frame: self.current_frame,
                        pid,
                        tid: hdr_tid,
                        kind: TimelineEventKind::GpuSubmit,
                        queue: Some(qname),
                        duration_qpc: None,
                        detail: format!("pkt_type={ptype} seq={seq}"),
                        raw_stack: stack,
                        waker_tid: None,
                        wait_kind: None,
                    });
                }
            }
            // QueuePacket Wait Start (244) -> paired with QueuePacket Stop (180) for EXACT GPU fence wait duration!
            244 => {
                let vals = schema.eval_all(data);
                let ctx = schema.get_from_vals(&vals, &["hContext"]).unwrap_or(0);
                let seq = schema
                    .get_from_vals(&vals, &["SubmitSequence", "ulSubmitSequence"])
                    .unwrap_or(0);
                let sync = schema
                    .get_from_vals(&vals, &["hSyncObject", "DxgSyncObject", "ObjectArray"])
                    .unwrap_or(0);
                let val = schema
                    .get_from_vals(&vals, &["FenceValue", "MonitoredFenceValue"])
                    .unwrap_or(0);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .unwrap_or(hdr_pid);
                if pid != 0 {
                    let ps = self.bump_pid(pid);
                    ps.dxg_events += 1;
                    ps.fence_waits += 1;
                }
                if !self.matches_pid(pid) {
                    return;
                }
                self.note_dxg_pid_match(pid, hdr_pid);
                if ctx != 0 {
                    self.ensure_queue(ctx, pid).fence_waits += 1;
                    self.in_flight_wait_pkts
                        .insert((ctx, seq), (qpc, pid, hdr_tid, sync, val));
                }
            }
            // QueuePacket Signal Start (245) -> paired with QueuePacket Stop (180) for EXACT GPU fence signal completion!
            245 => {
                let vals = schema.eval_all(data);
                let ctx = schema.get_from_vals(&vals, &["hContext"]).unwrap_or(0);
                let seq = schema
                    .get_from_vals(&vals, &["SubmitSequence", "ulSubmitSequence"])
                    .unwrap_or(0);
                let sync = schema
                    .get_from_vals(&vals, &["ObjectArray", "DxgSyncObjectArray", "hSyncObject"])
                    .unwrap_or(0);
                let val = schema
                    .get_from_vals(&vals, &["FenceValue", "MonitoredFenceValue"])
                    .unwrap_or(0);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .unwrap_or(hdr_pid);
                if pid != 0 {
                    let ps = self.bump_pid(pid);
                    ps.dxg_events += 1;
                    ps.fence_signals += 1;
                }
                if ctx != 0 && self.matches_pid(pid) {
                    self.note_dxg_pid_match(pid, hdr_pid);
                    self.ensure_queue(ctx, pid).fence_signals += 1;
                }
                if ctx != 0 {
                    self.in_flight_sig_pkts
                        .insert((ctx, seq), (pid, hdr_tid, sync, val));
                }
            }
            // QueuePacket Stop (180): completes 178 (GpuBusy), 244 (GPU FenceWait), OR 245 (GPU FenceSignal)!
            180 => {
                let hq = schema.get_u64(data, "hHwQueue").unwrap_or(0);
                let mut ctx = schema.get_u64(data, "hContext").unwrap_or(0);
                if ctx == 0 && hq != 0 {
                    ctx = self.hwqueue_ctx.get(&hq).copied().unwrap_or(hq);
                }
                let seq = schema
                    .get_any_u64(data, &["SubmitSequence", "ulSubmitSequence"])
                    .unwrap_or(0);

                if let Some((start_qpc, pid, start_tid, sync, val)) =
                    self.in_flight_wait_pkts.remove(&(ctx, seq))
                {
                    Self::prune_older_in_flight_seq(&mut self.in_flight_wait_pkts, ctx, seq);
                    let dur_qpc = (qpc - start_qpc).max(0);
                    self.last_complete_qpc = Some(qpc);
                    if self.ring_window_qpc.is_none() {
                        self.full_wait_spans.push((start_qpc, qpc, pid, start_tid, 1));
                        self.record_wait_acc(start_tid, "dxg_fence_gpu_wait", Some(dur_qpc), None, true);
                    }
                    let qname = self.queue_label(ctx);
                    self.push_event(RawTimelineEvent {
                        qpc: start_qpc,
                        frame: self.current_frame,
                        pid,
                        tid: start_tid,
                        kind: TimelineEventKind::FenceWait,
                        queue: Some(qname),
                        duration_qpc: Some(dur_qpc),
                        detail: format!("gpu_wait_pkt sync={sync:#x} val={val} seq={seq}"),
                        raw_stack: Vec::new(),
                        waker_tid: None,
                        wait_kind: Some("dxg_fence_gpu_wait".into()),
                    });
                    return;
                }

                if let Some((pid, sig_tid, sync, val)) = self.in_flight_sig_pkts.remove(&(ctx, seq)) {
                    self.last_complete_qpc = Some(qpc);
                    self.resolve_waits_for_sync(sync, val, qpc, sig_tid);
                    if self.matches_pid(pid) {
                        let qname = self.queue_label(ctx);
                        self.push_event(RawTimelineEvent {
                            qpc,
                            frame: self.current_frame,
                            pid,
                            tid: sig_tid,
                            kind: TimelineEventKind::FenceSignal,
                            queue: Some(qname),
                            duration_qpc: None,
                            detail: format!("gpu_sig_done sync={sync:#x} val={val} seq={seq}"),
                            raw_stack: Vec::new(),
                            waker_tid: None,
                            wait_kind: None,
                        });
                    }
                    return;
                }

                if let Some((start_qpc, pid, start_tid, ptype)) =
                    self.in_flight_queue_pkts.remove(&(ctx, seq))
                {
                    Self::prune_older_in_flight_seq(&mut self.in_flight_queue_pkts, ctx, seq);
                    let dur_qpc = (qpc - start_qpc).max(0);
                    let dur_ms = dur_qpc as f64 * 1e3 / self.qpc_freq_hz as f64;
                    self.last_complete_qpc = Some(qpc);
                    self.note_dxg_pid_match(pid, hdr_pid);
                    let q = self.ensure_queue(ctx, pid);
                    q.packets_completed += 1;
                    q.busy_ms += dur_ms;
                    if dur_ms > q.max_packet_ms {
                        q.max_packet_ms = dur_ms;
                    }
                    if self.ring_window_qpc.is_none() && dur_qpc > 0 {
                        self.full_gpu_spans.push((start_qpc, qpc, pid, ctx));
                    }
                    let qname = self.queue_label(ctx);
                    self.push_event(RawTimelineEvent {
                        qpc: start_qpc,
                        frame: self.current_frame,
                        pid,
                        tid: start_tid,
                        kind: TimelineEventKind::GpuBusy,
                        queue: Some(qname),
                        duration_qpc: Some(dur_qpc),
                        detail: format!("queue_pkt type={ptype} seq={seq}"),
                        raw_stack: Vec::new(),
                        waker_tid: None,
                        wait_kind: None,
                    });
                }
            }
            // DmaPacket Start (175) / Stop (176)
            175 => {
                let ctx = schema.get_u64(data, "hContext").unwrap_or(0);
                let seq = schema
                    .get_any_u64(data, &["SubmitSequence", "ulSubmitSequence"])
                    .unwrap_or(0);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .unwrap_or(hdr_pid);
                if !self.matches_pid(pid) {
                    return;
                }
                self.note_dxg_pid_match(pid, hdr_pid);
                self.note_submit_tid(pid, hdr_tid);
                self.note_ctx_pid(ctx, pid);
                self.last_submit_qpc = Some(qpc);
                self.total_target_submits += 1;
                if ctx != 0 {
                    self.in_flight_dma_pkts.insert((ctx, seq), (qpc, pid, hdr_tid));
                }
            }
            176 => {
                let ctx = schema.get_u64(data, "hContext").unwrap_or(0);
                let seq = schema
                    .get_any_u64(data, &["SubmitSequence", "ulSubmitSequence"])
                    .unwrap_or(0);
                if let Some((start_qpc, pid, start_tid)) = self.in_flight_dma_pkts.remove(&(ctx, seq)) {
                    Self::prune_older_in_flight_seq(&mut self.in_flight_dma_pkts, ctx, seq);
                    let dur_qpc = (qpc - start_qpc).max(0);
                    let dur_ms = dur_qpc as f64 * 1e3 / self.qpc_freq_hz as f64;
                    self.last_complete_qpc = Some(qpc);
                    let q = self.ensure_queue(ctx, pid);
                    if q.packets_completed == 0 {
                        q.packets_started += 1;
                        q.packets_completed += 1;
                        q.busy_ms += dur_ms;
                        if dur_ms > q.max_packet_ms {
                            q.max_packet_ms = dur_ms;
                        }
                    }
                    if self.ring_window_qpc.is_none() && dur_qpc > 0 {
                        self.full_gpu_spans.push((start_qpc, qpc, pid, ctx));
                    }
                    let qname = self.queue_label(ctx);
                    self.push_event(RawTimelineEvent {
                        qpc: start_qpc,
                        frame: self.current_frame,
                        pid,
                        tid: start_tid,
                        kind: TimelineEventKind::GpuBusy,
                        queue: Some(qname),
                        duration_qpc: Some(dur_qpc),
                        detail: format!("dma_pkt seq={seq}"),
                        raw_stack: Vec::new(),
                        waker_tid: None,
                        wait_kind: None,
                    });
                }
            }
            // HAGS HwQueue DmaPacket Submit (450) -> Complete (451)
            450 => {
                let hq = schema.get_u64(data, "hHwQueue").unwrap_or(0);
                let pf = schema.get_u64(data, "ProgressFenceValue").unwrap_or(0);
                let ctx = self.hwqueue_ctx.get(&hq).copied().unwrap_or(hq);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .unwrap_or(hdr_pid);
                if !self.matches_pid(pid) {
                    return;
                }
                self.note_dxg_pid_match(pid, hdr_pid);
                self.note_submit_tid(pid, hdr_tid);
                self.note_ctx_pid(ctx, pid);
                self.last_submit_qpc = Some(qpc);
                self.total_target_submits += 1;
                if hq != 0 && pf != 0 {
                    self.in_flight_hwq_pkts.insert((hq, pf), (qpc, pid, hdr_tid));
                }
            }
            451 => {
                let hq = schema.get_u64(data, "hHwQueue").unwrap_or(0);
                let pf = schema.get_u64(data, "ProgressFenceValue").unwrap_or(0);
                if let Some((start_qpc, pid, start_tid)) = self.in_flight_hwq_pkts.remove(&(hq, pf)) {
                    Self::prune_older_in_flight_seq(&mut self.in_flight_hwq_pkts, hq, pf);
                    let ctx = self.hwqueue_ctx.get(&hq).copied().unwrap_or(hq);
                    let dur_qpc = (qpc - start_qpc).max(0);
                    let dur_ms = dur_qpc as f64 * 1e3 / self.qpc_freq_hz as f64;
                    self.last_complete_qpc = Some(qpc);
                    let q = self.ensure_queue(ctx, pid);
                    if q.packets_completed == 0 {
                        q.packets_started += 1;
                        q.packets_completed += 1;
                        q.busy_ms += dur_ms;
                        if dur_ms > q.max_packet_ms {
                            q.max_packet_ms = dur_ms;
                        }
                    }
                    if self.ring_window_qpc.is_none() && dur_qpc > 0 {
                        self.full_gpu_spans.push((start_qpc, qpc, pid, ctx));
                    }
                    let qname = self.queue_label(ctx);
                    self.push_event(RawTimelineEvent {
                        qpc: start_qpc,
                        frame: self.current_frame,
                        pid,
                        tid: start_tid,
                        kind: TimelineEventKind::GpuBusy,
                        queue: Some(qname),
                        duration_qpc: Some(dur_qpc),
                        detail: format!("hwq_dma pf={pf}"),
                        raw_stack: Vec::new(),
                        waker_tid: None,
                        wait_kind: None,
                    });
                }
            }
            // WaitForSynchronizationObject* (100, 231, 295=FromGpu, 297=FromCpu)
            100 | 231 | 295 | 297 => {
                let vals = schema.eval_all(data);
                let ctx = schema
                    .get_from_vals(&vals, &["hContext", "BroadcastContext", "hHwQueue"])
                    .unwrap_or(0);
                let hdev = schema.get_from_vals(&vals, &["hDevice"]).unwrap_or(0);
                let raw_sync = schema
                    .get_from_vals(
                        &vals,
                        &[
                            "ObjectArray",
                            "SyncObjects",
                            "hSyncObject",
                            "ObjectHandle",
                            "ObjectHandleArray",
                            "pSyncObject",
                            "DxgSyncObjects",
                            "hFence",
                        ],
                    )
                    .unwrap_or(0);
                let sync = self.canonical_sync(raw_sync);
                let val = schema
                    .get_from_vals(
                        &vals,
                        &[
                            "MonitoredFenceValue",
                            "FenceValues",
                            "FenceValue",
                            "MonitoredFenceValueArray",
                            "Value",
                            "NewFenceValue",
                        ],
                    )
                    .unwrap_or(0);
                let ev_handle = schema
                    .get_from_vals(&vals, &["hAsyncEvent", "Event", "hEvent"])
                    .unwrap_or(0);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .or_else(|| self.device_pid.get(&hdev).copied().filter(|&p| p != 0))
                    .unwrap_or(hdr_pid);
                if pid != 0 {
                    let ps = self.bump_pid(pid);
                    ps.dxg_events += 1;
                    ps.fence_waits += 1;
                }
                if !self.matches_pid(pid) {
                    return;
                }
                self.note_dxg_pid_match(pid, hdr_pid);
                let qname = (ctx != 0).then(|| self.queue_label(ctx));
                if ctx != 0 {
                    self.ensure_queue(ctx, pid).fence_waits += 1;
                }
                let stack = extract_inline_stack(rec);
                let is_cpu_wait = id == 297 || id == 100;
                let pw = PendingFenceWait {
                    wait_qpc: qpc,
                    pid,
                    tid: hdr_tid,
                    queue: qname.clone(),
                    sync_obj: sync,
                    wait_val: val,
                    is_cpu: is_cpu_wait,
                    event_handle: ev_handle,
                    raw_stack: stack.clone(),
                };
                if is_cpu_wait && ev_handle != 0 {
                    self.pending_cpu_waits_by_event.insert(ev_handle, pw.clone());
                }
                if sync != 0 {
                    let entry = self.pending_waits_by_sync.entry(sync).or_default();
                    if entry.len() < 16 {
                        entry.push(pw);
                    }
                }
                let wk = if is_cpu_wait {
                    "dxg_fence_cpu_wait"
                } else {
                    "dxg_fence_gpu_wait"
                };
                self.record_wait_acc(hdr_tid, wk, None, None, true);
                self.push_event(RawTimelineEvent {
                    qpc,
                    frame: self.current_frame,
                    pid,
                    tid: hdr_tid,
                    kind: TimelineEventKind::FenceWait,
                    queue: qname,
                    duration_qpc: None,
                    detail: format!(
                        "{} sync={sync:#x} val={val}",
                        if is_cpu_wait {
                            "cpu_wait_start"
                        } else {
                            "gpu_wait_start"
                        }
                    ),
                    raw_stack: stack,
                    waker_tid: None,
                    wait_kind: Some(wk.into()),
                });
            }
            // UnwaitCpuWaiter (300): exact stop event for WaitForSynchronizationObjectFromCpu (297)!
            300 => {
                let vals = schema.eval_all(data);
                let ev_handle = schema.get_from_vals(&vals, &["Event"]).unwrap_or(0);
                let sync1 = schema
                    .get_from_vals(&vals, &["SyncObjects", "DxgSyncObjects"])
                    .unwrap_or(0);
                let val = schema.get_from_vals(&vals, &["FenceValues"]).unwrap_or(0);
                if ev_handle != 0
                    && let Some(w) = self.pending_cpu_waits_by_event.remove(&ev_handle)
                {
                    self.emit_resolved_wait(w, val, qpc, hdr_tid);
                    return;
                }
                if sync1 != 0 {
                    self.resolve_waits_for_sync(sync1, val, qpc, hdr_tid);
                }
            }
            // SignalSynchronizationObject* (101, 230, 294=FromGpu, 296=FromCpu, 462=Immediate, 550=VidSchiCpu, 551=VidSchGpuImm)
            101 | 230 | 294 | 296 | 462 | 550 | 551 => {
                let vals = schema.eval_all(data);
                let ctx = schema
                    .get_from_vals(&vals, &["hContext", "BroadcastContext", "hHwQueue"])
                    .unwrap_or(0);
                let hdev = schema.get_from_vals(&vals, &["hDevice"]).unwrap_or(0);
                let raw_sync = schema
                    .get_from_vals(
                        &vals,
                        &[
                            "VidSchSyncObjectArray",
                            "ObjectArray",
                            "DxgSyncObjectArray",
                            "hSyncObject",
                            "ObjectHandle",
                            "ObjectHandleArray",
                            "pSyncObject",
                            "hFence",
                        ],
                    )
                    .unwrap_or(0);
                let sync = self.canonical_sync(raw_sync);
                let val = schema
                    .get_from_vals(
                        &vals,
                        &[
                            "MonitoredFenceValue",
                            "FenceValues",
                            "FenceValue",
                            "MonitoredFenceValueArray",
                            "FenceValueArray",
                            "Value",
                            "NewFenceValue",
                        ],
                    )
                    .unwrap_or(0);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .or_else(|| self.device_pid.get(&hdev).copied().filter(|&p| p != 0))
                    .unwrap_or(hdr_pid);
                if sync != 0 {
                    self.resolve_waits_for_sync(sync, val, qpc, hdr_tid);
                }
                if pid != 0 && pid != 4 {
                    let ps = self.bump_pid(pid);
                    ps.dxg_events += 1;
                    ps.fence_signals += 1;
                }
                if !self.matches_pid(pid) {
                    return;
                }
                self.note_dxg_pid_match(pid, hdr_pid);
                if ctx != 0 {
                    self.ensure_queue(ctx, pid).fence_signals += 1;
                }
                let qname = (ctx != 0).then(|| self.queue_label(ctx));
                let stack = extract_inline_stack(rec);
                self.push_event(RawTimelineEvent {
                    qpc,
                    frame: self.current_frame,
                    pid,
                    tid: hdr_tid,
                    kind: TimelineEventKind::FenceSignal,
                    queue: qname,
                    duration_qpc: None,
                    detail: format!(
                        "{} id={id} sync={sync:#x} val={val}",
                        if matches!(id, 296 | 550) {
                            "cpu_sig"
                        } else {
                            "gpu_sig"
                        }
                    ),
                    raw_stack: stack,
                    waker_tid: None,
                    wait_kind: None,
                });
            }
            // Canonical Present candidates: Present (184), PresentQueuePacket Start (469), PresentHistory Start (171)
            184 | 469 | 171 => {
                let ctx = schema.get_u64(data, "hContext").unwrap_or(0);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .unwrap_or(hdr_pid);
                if pid != 0 {
                    let ps = self.bump_pid(pid);
                    ps.dxg_events += 1;
                    ps.presents += 1;
                }
                if !self.matches_pid(pid) {
                    return;
                }
                self.note_dxg_pid_match(pid, hdr_pid);
                let status = schema
                    .get_any_u64(data, &["Status", "hResult", "Result"])
                    .unwrap_or(0) as u32;
                if is_device_removed_status(status) {
                    let msg = format!(
                        "Device removal status {status:#010x} observed on Present (id={id}, pid={pid})"
                    );
                    self.record_removal(qpc, pid, hdr_tid, msg);
                }
                if self.ring_window_qpc.is_none() {
                    self.full_present_marks.push((qpc, pid, id));
                }
                let min_gap = (self.qpc_freq_hz as i64) / 2000;
                let dur_qpc = self.last_present_qpc.map(|prev| (qpc - prev).max(0));
                if dur_qpc.is_none_or(|d| d >= min_gap) {
                    self.last_present_qpc = Some(qpc);
                    self.current_frame += 1;
                }
                self.last_submit_qpc = Some(qpc);
                self.total_target_submits += 1;
                let stack = extract_inline_stack(rec);
                self.push_event(RawTimelineEvent {
                    qpc,
                    frame: self.current_frame,
                    pid,
                    tid: hdr_tid,
                    kind: TimelineEventKind::Present,
                    queue: (ctx != 0).then(|| self.queue_label(ctx)),
                    duration_qpc: dur_qpc,
                    detail: format!("id={id} status={status:#x}"),
                    raw_stack: stack,
                    waker_tid: None,
                    wait_kind: None,
                });
            }
            // VidMm Paging & Residency events (drop id=374 allocation rundown; also skip DC_Start=3 / DC_Stop=4 rundown snapshots)
            47 | 71 | 73 | 74 | 242 | 320 | 321 | 338 => {
                if matches!(opcode, 3 | 4) {
                    return;
                }
                if hdr_pid != 0 {
                    let ps = self.bump_pid(hdr_pid);
                    ps.dxg_events += 1;
                    ps.paging_events += 1;
                }
                if !self.matches_pid(hdr_pid) && self.filter_pid.is_some() {
                    return;
                }
                self.note_dxg_pid_match(hdr_pid, hdr_pid);
                if self.ring_window_qpc.is_none() {
                    self.full_aux_marks.push((qpc, hdr_pid, 0));
                }
                let label = match id {
                    73 => "PageIn",
                    74 => "Evict",
                    71 => "AllocFault",
                    47 => "Migrate",
                    242 => "ResetBacking",
                    320 | 338 => "MakeResident",
                    321 => "VidMmEvict",
                    _ => "Paging",
                };
                let stack = extract_inline_stack(rec);
                let is_wait = matches!(id, 320 | 338);
                if is_wait {
                    self.record_wait_acc(hdr_tid, "vidmm_make_resident", None, None, true);
                }
                self.push_event(RawTimelineEvent {
                    qpc,
                    frame: self.current_frame,
                    pid: hdr_pid,
                    tid: hdr_tid,
                    kind: TimelineEventKind::Paging,
                    queue: None,
                    duration_qpc: None,
                    detail: format!("{label} id={id}"),
                    raw_stack: stack,
                    waker_tid: None,
                    wait_kind: is_wait.then(|| "vidmm_make_resident".into()),
                });
            }
            // TDR / GPU Hang / Device Removal events (540, 541, 547, 548)
            540 | 541 | 547 | 548 => {
                let kind_name = match id {
                    540 => "TdrPayloadEngineTimeout",
                    541 => "TdrPayloadVSyncTimeout",
                    547 => "TdrCaptureDumpStart",
                    548 => "TdrCaptureDumpFinish",
                    _ => "Tdr",
                };
                let msg = format!(
                    "DxgKrnl {kind_name} (id={id}, pid={hdr_pid}, tid={hdr_tid}) — GPU hang / device removal (0x887a0006)"
                );
                self.record_removal(qpc, hdr_pid, hdr_tid, msg);
            }
            _ => {}
        }
    }

    fn record_removal(&mut self, qpc: i64, pid: u32, tid: u32, msg: String) {
        if self.tdr_or_removal_alert.is_none() {
            self.tdr_or_removal_alert = Some((qpc, msg.clone()));
        }
        self.alerts.push(msg.clone());
        self.push_event(RawTimelineEvent {
            qpc,
            frame: self.current_frame,
            pid,
            tid,
            kind: TimelineEventKind::Tdr,
            queue: None,
            duration_qpc: None,
            detail: msg,
            raw_stack: Vec::new(),
            waker_tid: None,
            wait_kind: None,
        });
    }

    fn on_memory_event(&mut self, rec: &EVENT_RECORD, qpc: i64) {
        let pid = rec.EventHeader.ProcessId;
        if !self.matches_pid(pid) {
            return;
        }
        let tid = rec.EventHeader.ThreadId;
        let opcode = rec.EventHeader.EventDescriptor.Opcode;
        let id = rec.EventHeader.EventDescriptor.Id;
        let stack = extract_inline_stack(rec);
        let (kind, detail, aux_code) = if opcode == 98 || id == 98 {
            (
                TimelineEventKind::VirtualProtect,
                format!("VirtualAlloc id={id}"),
                1u8,
            )
        } else {
            (
                TimelineEventKind::HardFault,
                format!("HardFault id={id} op={opcode}"),
                2u8,
            )
        };
        if self.ring_window_qpc.is_none() {
            self.full_aux_marks.push((qpc, pid, aux_code));
            if aux_code == 2 {
                self.record_wait_acc(tid, "hard_fault", None, None, true);
            }
        }
        self.push_event(RawTimelineEvent {
            qpc,
            frame: self.current_frame,
            pid,
            tid,
            kind,
            queue: None,
            duration_qpc: None,
            detail,
            raw_stack: stack,
            waker_tid: None,
            wait_kind: (aux_code == 2).then(|| "hard_fault".into()),
        });
    }

    fn on_kernel_memory(&mut self, rec: &EVENT_RECORD, qpc: i64) {
        let pid = rec.EventHeader.ProcessId;
        if !self.matches_pid(pid) {
            return;
        }
        let tid = rec.EventHeader.ThreadId;
        let id = rec.EventHeader.EventDescriptor.Id;
        let stack = extract_inline_stack(rec);
        if self.ring_window_qpc.is_none() {
            self.full_aux_marks.push((qpc, pid, 1));
        }
        self.push_event(RawTimelineEvent {
            qpc,
            frame: self.current_frame,
            pid,
            tid,
            kind: TimelineEventKind::VirtualProtect,
            queue: None,
            duration_qpc: None,
            detail: format!("KernelMemory id={id}"),
            raw_stack: stack,
            waker_tid: None,
            wait_kind: None,
        });
    }

    fn on_scheduler_event(&mut self, rec: &EVENT_RECORD, qpc: i64) {
        let opcode = rec.EventHeader.EventDescriptor.Opcode;
        let data = if rec.UserData.is_null() || rec.UserDataLength == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(rec.UserData.cast::<u8>(), rec.UserDataLength as usize) }
        };
        // Classic/SystemScheduler CSwitch = opcode 36; ReadyThread = opcode 50.
        if opcode == 36 && data.len() >= 8 {
            self.saw_cswitch = true;
            let new_tid = u32::from_le_bytes(data[0..4].try_into().unwrap_or([0; 4]));
            let old_tid = u32::from_le_bytes(data[4..8].try_into().unwrap_or([0; 4]));
            let stack = extract_inline_stack(rec);
            if old_tid != 0 {
                self.off_cpu_threads.insert(old_tid, (qpc, stack));
            }
            if let Some((out_qpc, out_stack)) = self.off_cpu_threads.remove(&new_tid) {
                let pid = rec.EventHeader.ProcessId;
                if self.matches_pid(pid) {
                    let dur_qpc = (qpc - out_qpc).max(0);
                    let waker = self.last_waker.remove(&new_tid);
                    if self.ring_window_qpc.is_none() {
                        self.full_wait_spans.push((out_qpc, qpc, pid, new_tid, 2));
                        self.record_wait_acc(new_tid, "cswitch_wait", Some(dur_qpc), waker, true);
                    }
                    self.push_event(RawTimelineEvent {
                        qpc: out_qpc,
                        frame: self.current_frame,
                        pid,
                        tid: new_tid,
                        kind: TimelineEventKind::CSwitchWait,
                        queue: None,
                        duration_qpc: Some(dur_qpc),
                        detail: match waker {
                            Some(w) => format!("off_cpu waker_tid={w}"),
                            None => "off_cpu".into(),
                        },
                        raw_stack: out_stack,
                        waker_tid: waker,
                        wait_kind: Some("cswitch_wait".into()),
                    });
                }
            }
        } else if opcode == 50 && data.len() >= 4 {
            self.saw_ready_thread = true;
            let readied_tid = u32::from_le_bytes(data[0..4].try_into().unwrap_or([0; 4]));
            let waker_tid = rec.EventHeader.ThreadId;
            if readied_tid != 0 && waker_tid != 0 {
                self.last_waker.insert(readied_tid, waker_tid);
            }
        }
    }

    fn on_profile_event(&mut self, rec: &EVENT_RECORD, qpc: i64) {
        let opcode = rec.EventHeader.EventDescriptor.Opcode;
        if opcode != 46 {
            return;
        }
        self.saw_sampled_profile = true;
        let pid = rec.EventHeader.ProcessId;
        if !self.matches_pid(pid) {
            return;
        }
        let tid = rec.EventHeader.ThreadId;
        let mut stack = extract_inline_stack(rec);
        if stack.is_empty() && !rec.UserData.is_null() && rec.UserDataLength >= 8 {
            let data =
                unsafe { std::slice::from_raw_parts(rec.UserData.cast::<u8>(), rec.UserDataLength as usize) };
            let ip = u64::from_le_bytes(data[0..8].try_into().unwrap_or([0; 8]));
            if ip != 0 {
                stack.push(ip);
            }
        }
        self.push_event(RawTimelineEvent {
            qpc,
            frame: self.current_frame,
            pid,
            tid,
            kind: TimelineEventKind::CpuSample,
            queue: None,
            duration_qpc: None,
            detail: "SampledProfile".into(),
            raw_stack: stack,
            waker_tid: None,
            wait_kind: None,
        });
    }

    fn on_image_event(&mut self, rec: &EVENT_RECORD) {
        let opcode = rec.EventHeader.EventDescriptor.Opcode;
        if !matches!(opcode, 3 | 10 | 1) {
            return;
        }
        let data = if rec.UserData.is_null() || rec.UserDataLength == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(rec.UserData.cast::<u8>(), rec.UserDataLength as usize) }
        };
        if let Some(schema) = self.schema_for(rec) {
            let base = schema
                .get_any_u64(data, &["ImageBase", "BaseAddress"])
                .unwrap_or(0);
            let size = schema.get_any_u64(data, &["ImageSize", "Size"]).unwrap_or(0);
            if base != 0 && size != 0 {
                let path = extract_trailing_utf16(data).unwrap_or_else(|| format!("mod_{base:#x}"));
                self.etw_images.push((base, size, path));
            }
        }
    }

    fn on_classic_stackwalk(&mut self, rec: &EVENT_RECORD) {
        let data = if rec.UserData.is_null() || rec.UserDataLength < 24 {
            return;
        } else {
            unsafe { std::slice::from_raw_parts(rec.UserData.cast::<u8>(), rec.UserDataLength as usize) }
        };
        // Classic StackWalk_Event (x64): EventTimeStamp(8), StackProcess(4), StackThread(4), Stack[N](8 each).
        let ev_ts = i64::from_le_bytes(data[0..8].try_into().unwrap_or([0; 8]));
        let pid = u32::from_le_bytes(data[8..12].try_into().unwrap_or([0; 4]));
        let tid = u32::from_le_bytes(data[12..16].try_into().unwrap_or([0; 4]));
        if !self.matches_pid(pid) {
            return;
        }
        let mut frames = Vec::new();
        for chunk in data[16..].as_chunks::<8>().0.iter().take(32) {
            let addr = u64::from_le_bytes(*chunk);
            if addr != 0 {
                frames.push(addr);
            }
        }
        if frames.is_empty() {
            return;
        }
        // Attach to the most recent matching event for (tid, ev_ts).
        for ev in self.raw_events.iter_mut().rev().take(64) {
            if ev.tid == tid && (ev.qpc == ev_ts || (ev.qpc - ev_ts).abs() < 500) && ev.raw_stack.is_empty() {
                ev.raw_stack = frames;
                self.stack_samples_captured += 1;
                return;
            }
        }
    }

    /// Non-consuming snapshot of the current ring state (used by `--rearm` so the flight recorder stays armed).
    pub fn snapshot_report(
        &self,
        source: String,
        symbolizer: &mut Symbolizer,
        warnings: Vec<String>,
        flight_trigger: Option<FlightTriggerInfo>,
    ) -> TimelineReport {
        let mut copy = TimelineBuilder::new(self.filter_pid, self.qpc_freq_hz);
        copy.ts_unit = self.ts_unit.clone();
        copy.start_qpc = self.start_qpc;
        copy.end_qpc = self.end_qpc;
        copy.target_start_qpc = self.target_start_qpc;
        copy.target_end_qpc = self.target_end_qpc;
        copy.total_raw_events = self.total_raw_events;
        copy.dxg_pid_events = self.dxg_pid_events;
        copy.dxg_ctx_other_pid_events = self.dxg_ctx_other_pid_events;
        copy.stack_samples_captured = self.stack_samples_captured;
        copy.saw_cswitch = self.saw_cswitch;
        copy.saw_ready_thread = self.saw_ready_thread;
        copy.saw_sampled_profile = self.saw_sampled_profile;
        copy.saw_dxgkrnl = self.saw_dxgkrnl;
        copy.saw_sysmem = self.saw_sysmem;
        copy.saw_kmem = self.saw_kmem;
        copy.context_meta = self.context_meta.clone();
        copy.hwqueue_ctx = self.hwqueue_ctx.clone();
        copy.in_flight_queue_pkts = self.in_flight_queue_pkts.clone();
        copy.in_flight_dma_pkts = self.in_flight_dma_pkts.clone();
        copy.in_flight_hwq_pkts = self.in_flight_hwq_pkts.clone();
        copy.etw_images = self.etw_images.clone();
        copy.pid_names = self.pid_names.clone();
        copy.alerts = self.alerts.clone();
        copy.pid_stats = self.pid_stats.clone();
        copy.queues = self.queues.clone();
        copy.ctx_pid_counts = self.ctx_pid_counts.clone();
        copy.pid_tid_submits = self.pid_tid_submits.clone();
        copy.raw_events = self.raw_events.clone();
        copy.ring_window_qpc = self.ring_window_qpc;
        copy.max_retained_events = self.max_retained_events;
        copy.evicted_events = self.evicted_events;
        copy.ev_truncated = self.ev_truncated;
        copy.finish(source, symbolizer, warnings, flight_trigger)
    }

    /// Finalizes the unified QPC timeline, symbolizing all captured stacks out-of-process,
    /// clipping per-frame GPU/wait intervals to exact canonical present bounds, and sorting EV lines.
    pub fn finish(
        mut self,
        source: String,
        symbolizer: &mut Symbolizer,
        mut warnings: Vec<String>,
        flight_trigger: Option<FlightTriggerInfo>,
    ) -> TimelineReport {
        for (base, size, path) in &self.etw_images {
            symbolizer.add_etw_image(*base, *size, path);
        }

        let is_ring = self.ring_window_qpc.is_some();
        let start_qpc = if is_ring {
            self.raw_events
                .front()
                .map(|e| e.qpc)
                .or(self.start_qpc)
                .unwrap_or(0)
        } else {
            self.start_qpc
                .or_else(|| self.raw_events.front().map(|e| e.qpc))
                .unwrap_or(0)
        };
        let end_qpc = if is_ring {
            self.raw_events
                .back()
                .map(|e| e.qpc)
                .or(self.end_qpc)
                .unwrap_or(start_qpc)
        } else {
            self.end_qpc
                .or_else(|| self.raw_events.back().map(|e| e.qpc))
                .unwrap_or(start_qpc)
        };
        let trace_dur_ms = (end_qpc - start_qpc).max(0) as f64 * 1e3 / self.qpc_freq_hz as f64;
        let target_span_ms = if is_ring {
            trace_dur_ms
        } else {
            match (self.target_start_qpc, self.target_end_qpc) {
                (Some(ts), Some(te)) => (te - ts).max(0) as f64 * 1e3 / self.qpc_freq_hz as f64,
                _ => 0.0,
            }
        };
        let qpc_to_ms = |ticks: i64| ticks as f64 * 1e3 / self.qpc_freq_hz as f64;

        for &(ctx, _) in self.in_flight_queue_pkts.keys() {
            if let Some(q) = self.queues.get_mut(&ctx) {
                q.in_flight_at_end += 1;
            }
        }

        let mut stack_acc: HashMap<(String, String), u64> = HashMap::new();
        let mut wait_acc = if is_ring {
            BTreeMap::new()
        } else {
            std::mem::take(&mut self.full_wait_acc)
        };
        let mut measured_waits = if is_ring { 0 } else { self.full_measured_waits };
        let mut gpu_spans = if is_ring {
            Vec::new()
        } else {
            std::mem::take(&mut self.full_gpu_spans)
        };
        let mut wait_spans = if is_ring {
            Vec::new()
        } else {
            std::mem::take(&mut self.full_wait_spans)
        };
        let mut present_marks = if is_ring {
            Vec::new()
        } else {
            std::mem::take(&mut self.full_present_marks)
        };
        let mut aux_marks = if is_ring {
            Vec::new()
        } else {
            std::mem::take(&mut self.full_aux_marks)
        };
        let qname_to_ctx: HashMap<String, u64> = self
            .queues
            .values()
            .map(|q| (q.queue_id.clone(), q.context))
            .collect();

        let mut events = Vec::with_capacity(self.raw_events.len());
        let had_full_gpu = !gpu_spans.is_empty();
        let had_full_presents = !present_marks.is_empty();
        let had_full_waits = !wait_acc.is_empty();
        let had_full_wait_spans = !wait_spans.is_empty();

        for raw in self.raw_events {
            let t_ms = qpc_to_ms(raw.qpc - start_qpc);
            let duration_ms = raw.duration_qpc.map(qpc_to_ms);
            let stack = if raw.raw_stack.is_empty() {
                Vec::new()
            } else {
                symbolizer.resolve_stack(&raw.raw_stack, 8)
            };
            let stack_line = if stack.is_empty() {
                None
            } else {
                let joined = stack.iter().take(4).cloned().collect::<Vec<_>>().join(" <- ");
                *stack_acc
                    .entry((raw.kind.tag().to_string(), joined.clone()))
                    .or_insert(0) += 1;
                Some(joined)
            };

            if (is_ring || !had_full_gpu)
                && raw.kind == TimelineEventKind::GpuBusy
                && let Some(dq) = raw.duration_qpc
                && dq > 0
            {
                let ctx = raw
                    .queue
                    .as_deref()
                    .and_then(|qn| {
                        qname_to_ctx
                            .get(qn)
                            .copied()
                            .or_else(|| parse_ctx_from_queue_label(qn))
                    })
                    .unwrap_or(0);
                gpu_spans.push((raw.qpc, raw.qpc.saturating_add(dq), raw.pid, ctx));
            }
            if (is_ring || !had_full_presents) && raw.kind == TimelineEventKind::Present {
                present_marks.push((raw.qpc, raw.pid, 184));
            }
            if is_ring || !had_full_wait_spans {
                match raw.kind {
                    TimelineEventKind::Paging if is_ring => aux_marks.push((raw.qpc, raw.pid, 0)),
                    TimelineEventKind::VirtualProtect if is_ring => aux_marks.push((raw.qpc, raw.pid, 1)),
                    TimelineEventKind::HardFault if is_ring => aux_marks.push((raw.qpc, raw.pid, 2)),
                    TimelineEventKind::FenceWait => {
                        if let Some(dq) = raw.duration_qpc {
                            let is_cpu = raw.wait_kind.as_deref() == Some("dxg_fence_cpu_wait");
                            wait_spans.push((
                                raw.qpc,
                                raw.qpc.saturating_add(dq),
                                raw.pid,
                                raw.tid,
                                if is_cpu { 0 } else { 1 },
                            ));
                        }
                    }
                    TimelineEventKind::CSwitchWait => {
                        if let Some(dq) = raw.duration_qpc {
                            wait_spans.push((raw.qpc, raw.qpc.saturating_add(dq), raw.pid, raw.tid, 2));
                        }
                    }
                    _ => {}
                }
            }

            if let Some(ref wk) = raw.wait_kind {
                if is_ring || !had_full_waits {
                    let e = wait_acc
                        .entry((raw.tid, wk.clone()))
                        .or_insert_with(|| (0, 0, 0.0, 0.0, HashMap::new(), HashMap::new()));
                    e.0 += 1;
                    if let Some(d) = duration_ms {
                        e.1 += 1;
                        e.2 += d;
                        if d > e.3 {
                            e.3 = d;
                        }
                        measured_waits += 1;
                    }
                    if let Some(w) = raw.waker_tid {
                        *e.4.entry(w).or_insert(0) += 1;
                    }
                    if let Some(ref s) = stack_line {
                        *e.5.entry(s.clone()).or_insert(0) += 1;
                    }
                } else if let Some(ref s) = stack_line
                    && let Some(e) = wait_acc.get_mut(&(raw.tid, wk.clone()))
                {
                    *e.5.entry(s.clone()).or_insert(0) += 1;
                }
            }

            events.push(TimelineEvent {
                qpc: raw.qpc,
                t_ms,
                frame: raw.frame,
                pid: raw.pid,
                tid: raw.tid,
                kind: raw.kind,
                queue: raw.queue,
                duration_ms,
                detail: raw.detail,
                stack,
            });
        }

        events.sort_by_key(|e| e.qpc);

        // Resolve true owning PID per `hContext` from final `context_meta` (or modal non-System PID)
        // so late DC_Start rundowns or stray interrupted-thread hdr_pids never misattribute queues.
        let mut ctx_true_pid: HashMap<u64, u32> = HashMap::new();
        for (&ctx, meta) in &self.context_meta {
            if meta.pid != 0 && meta.pid != 4 {
                ctx_true_pid.insert(ctx, meta.pid);
            }
        }
        for (&ctx, counts) in &self.ctx_pid_counts {
            ctx_true_pid.entry(ctx).or_insert_with(|| {
                counts
                    .iter()
                    .max_by_key(|&(_, &c)| c)
                    .map(|(&p, _)| p)
                    .unwrap_or(0)
            });
        }
        for span in &mut gpu_spans {
            if span.3 != 0
                && let Some(&tp) = ctx_true_pid.get(&span.3)
                && tp != 0
            {
                span.2 = tp;
            }
        }

        let mut queue_intervals: HashMap<u64, Vec<(i64, i64)>> = HashMap::new();
        for &(s, e, _, ctx) in &gpu_spans {
            let cs = s.max(start_qpc);
            let ce = e.min(end_qpc);
            if ce > cs {
                queue_intervals.entry(ctx).or_default().push((cs, ce));
            }
        }
        let mut queues: Vec<QueueSummary> = self
            .queues
            .into_values()
            .map(|mut q| {
                if let Some(&tp) = ctx_true_pid.get(&q.context)
                    && tp != 0
                {
                    q.pid = tp;
                    q.process_name = self.pid_names.get(&tp).cloned();
                } else if q.process_name.is_none() && q.pid != 0 {
                    q.process_name = self.pid_names.get(&q.pid).cloned();
                }
                if let Some(intervals) = queue_intervals.remove(&q.context) {
                    q.busy_ms = qpc_to_ms(union_interval_qpc(intervals));
                    q.stale_in_window = false;
                } else if is_ring {
                    // Never carry cumulative pre-window busy_ms into a ring snapshot when no packet
                    // completed on this queue inside [start_qpc, end_qpc].
                    q.busy_ms = 0.0;
                    q.stale_in_window = q.in_flight_at_end == 0;
                } else if had_full_gpu {
                    q.busy_ms = 0.0;
                    q.stale_in_window = false;
                } else {
                    q.busy_ms = q.busy_ms.min(trace_dur_ms);
                    q.stale_in_window = false;
                }
                q.busy_pct = if trace_dur_ms > 0.0 {
                    ((q.busy_ms / trace_dur_ms) * 100.0).min(100.0)
                } else {
                    0.0
                };
                q
            })
            .collect();
        queues.sort_by(|a, b| {
            a.stale_in_window
                .cmp(&b.stale_in_window)
                .then_with(|| b.busy_ms.total_cmp(&a.busy_ms))
                .then_with(|| b.in_flight_at_end.cmp(&a.in_flight_at_end))
                .then_with(|| b.packets_started.cmp(&a.packets_started))
        });
        // Annotate lockstep sibling contexts of the same PID (identical pkts and busy_ms within 5 ms).
        let mut seen_sig: HashMap<(u32, u64, u64), (String, f64)> = HashMap::new();
        for q in &mut queues {
            if q.packets_started >= 4 && q.busy_ms >= 1.0 {
                let key = (q.pid, q.packets_started, q.packets_completed);
                if let Some((first_id, first_busy)) = seen_sig.get(&key) {
                    if (q.busy_ms - first_busy).abs() <= 5.0 {
                        q.sibling_note = Some(format!("sibling_of={first_id}"));
                    }
                } else {
                    seen_sig.insert(key, (q.queue_id.clone(), q.busy_ms));
                }
            }
        }

        let mut pids: Vec<PidActivitySummary> = self
            .pid_stats
            .into_values()
            .map(|mut p| {
                if p.process_name.is_none() {
                    p.process_name = self.pid_names.get(&p.pid).cloned();
                }
                p
            })
            .collect();
        pids.sort_by(|a, b| {
            let a_has_p = a.presents > 0 && a.pid != 0 && a.pid != 4;
            let b_has_p = b.presents > 0 && b.pid != 0 && b.pid != 4;
            b_has_p
                .cmp(&a_has_p)
                .then_with(|| b.submits.cmp(&a.submits))
                .then_with(|| b.presents.cmp(&a.presents))
                .then_with(|| b.dxg_events.cmp(&a.dxg_events))
        });

        let frame_pid = self.filter_pid.or_else(|| {
            pids.iter()
                .find(|p| p.pid != 0 && p.pid != 4 && p.presents > 0)
                .or_else(|| pids.iter().find(|p| p.pid != 0 && p.pid != 4 && p.submits > 0))
                .map(|p| p.pid)
                .or_else(|| present_marks.first().map(|m| m.1))
        });
        let submit_tid = frame_pid.and_then(|fpid| {
            self.pid_tid_submits
                .iter()
                .filter(|&(&(p, t), _)| p == fpid && t != 0)
                .max_by_key(|&(_, &c)| c)
                .map(|(&(_, t), _)| t)
                .or_else(|| {
                    let mut counts: HashMap<u32, u64> = HashMap::new();
                    for ev in &events {
                        if ev.pid == fpid
                            && ev.tid != 0
                            && matches!(ev.kind, TimelineEventKind::GpuBusy | TimelineEventKind::GpuSubmit)
                        {
                            *counts.entry(ev.tid).or_insert(0) += 1;
                        }
                    }
                    counts.into_iter().max_by_key(|&(_, c)| c).map(|(t, _)| t)
                })
        });
        if self.filter_pid.is_none()
            && let Some(fp) = frame_pid
            && pids.iter().filter(|p| p.presents > 0).count() > 1
        {
            let nm = self
                .pid_names
                .get(&fp)
                .map(|n| format!(" ({n})"))
                .unwrap_or_default();
            warnings.push(format!(
                "pid=all had presents from multiple processes; FRAME table scoped to pid={fp}{nm} (pass --pid <PID> or --list-pids to pick another)"
            ));
        }

        let frames = build_clipped_frames(
            frame_pid,
            submit_tid,
            start_qpc,
            end_qpc,
            self.qpc_freq_hz,
            &present_marks,
            &gpu_spans,
            &wait_spans,
            &aux_marks,
        );

        // Compute union wall-clock ms per (tid, wait_kind) clipped to [start_qpc, end_qpc] (<= trace_dur_ms).
        let mut wait_intervals: HashMap<(u32, u8), Vec<(i64, i64)>> = HashMap::new();
        for &(s, e, _, tid, kc) in &wait_spans {
            let cs = s.max(start_qpc);
            let ce = e.min(end_qpc);
            if ce > cs {
                wait_intervals.entry((tid, kc)).or_default().push((cs, ce));
            }
        }
        let mut waits: Vec<WaitSummary> = wait_acc
            .into_iter()
            .map(
                |((tid, kind), (count, measured_count, total_ms, max_ms, wakers, stacks))| {
                    let top_waker = wakers
                        .into_iter()
                        .max_by_key(|&(_, n)| n)
                        .map(|(w, n)| format!("tid={w} ({n}x)"));
                    let top_stack = stacks.into_iter().max_by_key(|(_, n)| *n).map(|(s, _)| s);
                    let kc = match kind.as_str() {
                        "dxg_fence_cpu_wait" => Some(0u8),
                        "dxg_fence_gpu_wait" => Some(1u8),
                        "cswitch_off_cpu" => Some(2u8),
                        _ => None,
                    };
                    let wall_ms = kc
                        .and_then(|c| wait_intervals.remove(&(tid, c)))
                        .map(|ivs| qpc_to_ms(union_interval_qpc(ivs)).min(trace_dur_ms))
                        .unwrap_or_else(|| total_ms.min(trace_dur_ms));
                    WaitSummary {
                        tid,
                        kind,
                        count,
                        measured_count,
                        wall_ms,
                        total_ms,
                        max_ms,
                        top_waker,
                        top_stack,
                    }
                },
            )
            .collect();
        waits.sort_by(|a, b| {
            b.wall_ms
                .total_cmp(&a.wall_ms)
                .then_with(|| b.total_ms.total_cmp(&a.total_ms))
                .then_with(|| b.count.cmp(&a.count))
        });

        let mut top_stacks: Vec<StackSummary> = stack_acc
            .into_iter()
            .map(|((kind, stack), count)| StackSummary { count, kind, stack })
            .collect();
        top_stacks.sort_by_key(|s| std::cmp::Reverse(s.count));
        top_stacks.truncate(50);

        let mut providers_active = Vec::new();
        if self.saw_dxgkrnl {
            providers_active.push("DxgKrnl".into());
        }
        if self.saw_sysmem {
            providers_active.push("SystemMemory".into());
        }
        if self.saw_kmem {
            providers_active.push("Kernel-Memory".into());
        }
        if self.saw_cswitch || self.saw_ready_thread {
            providers_active.push("Scheduler(CSwitch/ReadyThread)".into());
        }
        if self.saw_sampled_profile {
            providers_active.push("SampledProfile".into());
        }

        let wait_attribution_mode = if self.saw_cswitch && self.saw_ready_thread {
            "cswitch_ready_thread+dxgkrnl_fences".into()
        } else if self.saw_cswitch {
            "cswitch+dxgkrnl_fences".into()
        } else {
            "non_admin_dxgkrnl_fences_and_stacks".into()
        };

        TimelineReport {
            source,
            pid: self.filter_pid,
            frame_pid,
            submit_tid,
            target_exited_at_ms: None,
            target_exit_code: None,
            thread_suspends: THREAD_SUSPENDS,
            ts_unit: self.ts_unit,
            qpc_freq_hz: self.qpc_freq_hz,
            start_qpc,
            end_qpc,
            duration_ms: trace_dur_ms,
            target_span_ms,
            total_raw_events: self.total_raw_events,
            target_events: events.len(),
            ev_truncated: self.ev_truncated,
            dxg_pid_events: self.dxg_pid_events,
            dxg_ctx_other_pid_events: self.dxg_ctx_other_pid_events,
            stack_samples_captured: self.stack_samples_captured,
            measured_waits,
            wait_attribution_mode,
            providers_active,
            warnings,
            alerts: self.alerts,
            flight_trigger,
            pids,
            queues,
            waits,
            frames,
            top_stacks,
            events,
        }
    }
}

/// Builds deduplicated canonical per-present frames for `frame_pid` and clips every GPU/wait interval
/// strictly to `[frame.start_qpc, frame.end_qpc]` so `gpu_busy_ms <= dur_ms` is mathematically guaranteed!
#[allow(clippy::too_many_arguments)]
fn build_clipped_frames(
    frame_pid: Option<u32>,
    submit_tid: Option<u32>,
    start_qpc: i64,
    _end_qpc: i64,
    qpc_freq_hz: u64,
    present_marks: &[(i64, u32, u16)],
    gpu_spans: &[(i64, i64, u32, u64)],
    wait_spans: &[(i64, i64, u32, u32, u8)],
    aux_marks: &[(i64, u32, u8)],
) -> Vec<FrameSummary> {
    let Some(fpid) = frame_pid else {
        return Vec::new();
    };
    let qpc_to_ms = |ticks: i64| ticks as f64 * 1e3 / qpc_freq_hz.max(1) as f64;
    let min_gap = (qpc_freq_hz as i64) / 2000; // 0.5 ms dedup threshold for paired Present/PresentHistory

    let mut pid_presents: Vec<(i64, u16)> = present_marks
        .iter()
        .filter(|&&(_, p, _)| p == fpid)
        .map(|&(q, _, id)| (q, id))
        .collect();
    if pid_presents.is_empty() {
        return Vec::new();
    }
    let canon_id = if pid_presents.iter().any(|&(_, id)| id == 184) {
        184u16
    } else if pid_presents.iter().any(|&(_, id)| id == 469) {
        469u16
    } else {
        pid_presents[0].1
    };
    pid_presents.retain(|&(_, id)| id == canon_id);
    pid_presents.sort_unstable_by_key(|&(q, _)| q);

    let mut dedup: Vec<(i64, u16)> = Vec::with_capacity(pid_presents.len());
    for (q, id) in pid_presents {
        if dedup.last().is_none_or(|&(prev, _)| q - prev >= min_gap) {
            dedup.push((q, id));
        }
    }

    let mut windows: Vec<(u64, i64, i64, u16)> = Vec::with_capacity(dedup.len());
    let mut prev_qpc = start_qpc;
    for (idx, &(pq, pid_ev)) in dedup.iter().enumerate() {
        let f_start = if idx == 0 { prev_qpc.min(pq) } else { prev_qpc };
        let f_end = pq.max(f_start);
        windows.push((idx as u64, f_start, f_end, pid_ev));
        prev_qpc = f_end;
    }

    let mut g_spans: Vec<(i64, i64, u64)> = gpu_spans
        .iter()
        .filter(|&&(_, _, p, _)| p == fpid)
        .map(|&(s, e, _, ctx)| (s, e, ctx))
        .collect();
    g_spans.sort_unstable_by_key(|&(s, _, _)| s);

    let mut w_spans: Vec<(i64, i64, u32, u8)> = wait_spans
        .iter()
        .filter(|&&(_, _, p, _, _)| p == fpid)
        .map(|&(s, e, _, tid, kc)| (s, e, tid, kc))
        .collect();
    w_spans.sort_unstable_by_key(|&(s, _, _, _)| s);

    let mut a_marks: Vec<(i64, u8)> = aux_marks
        .iter()
        .filter(|&&(_, p, _)| p == fpid)
        .map(|&(q, _, code)| (q, code))
        .collect();
    a_marks.sort_unstable_by_key(|&(q, _)| q);

    let mut frames = Vec::with_capacity(windows.len());
    let mut aux_idx = 0usize;
    let mut gpu_head = 0usize;
    let mut wait_head = 0usize;

    for (fnum, f_start, f_end, pid_ev) in windows {
        let dur_ticks = (f_end - f_start).max(0);
        let dur_ms = qpc_to_ms(dur_ticks);

        while gpu_head < g_spans.len() && g_spans[gpu_head].1 <= f_start {
            gpu_head += 1;
        }
        let mut all_gpu: Vec<(i64, i64)> = Vec::new();
        let mut by_q: HashMap<u64, Vec<(i64, i64)>> = HashMap::new();
        for &(s, e, ctx) in &g_spans[gpu_head..] {
            if s >= f_end {
                break;
            }
            let cs = s.max(f_start);
            let ce = e.min(f_end);
            if ce > cs {
                all_gpu.push((cs, ce));
                by_q.entry(ctx).or_default().push((cs, ce));
            }
        }
        let gpu_busy_ms = qpc_to_ms(union_interval_qpc(all_gpu)).min(dur_ms);
        let mut busiest_q_ms = 0.0f64;
        let mut q_sum_ms = 0.0f64;
        for ivs in by_q.into_values() {
            let ms = qpc_to_ms(union_interval_qpc(ivs)).min(dur_ms);
            q_sum_ms += ms;
            if ms > busiest_q_ms {
                busiest_q_ms = ms;
            }
        }

        while wait_head < w_spans.len() && w_spans[wait_head].1 <= f_start {
            wait_head += 1;
        }
        let mut cpu_w = Vec::new();
        let mut sub_cpu_w = Vec::new();
        let mut gpu_w = Vec::new();
        let mut os_w = Vec::new();
        for &(s, e, tid, kc) in &w_spans[wait_head..] {
            if s >= f_end {
                break;
            }
            let cs = s.max(f_start);
            let ce = e.min(f_end);
            if ce > cs {
                match kc {
                    0 => {
                        cpu_w.push((cs, ce));
                        if submit_tid == Some(tid) {
                            sub_cpu_w.push((cs, ce));
                        }
                    }
                    1 => gpu_w.push((cs, ce)),
                    _ => os_w.push((cs, ce)),
                }
            }
        }
        let gpu_idle_ms = (dur_ms - gpu_busy_ms).max(0.0);
        let cpu_fence_wait_ms = qpc_to_ms(union_interval_qpc(cpu_w)).min(dur_ms);
        let submit_tid_cpu_wait_ms = qpc_to_ms(union_interval_qpc(sub_cpu_w)).min(dur_ms);
        let gpu_fence_wait_ms = qpc_to_ms(union_interval_qpc(gpu_w)).min(dur_ms);
        let os_wait_ms = qpc_to_ms(union_interval_qpc(os_w)).min(dur_ms);

        while aux_idx < a_marks.len() && a_marks[aux_idx].0 < f_start {
            aux_idx += 1;
        }
        let mut paging = 0u64;
        let mut vprot = 0u64;
        let mut hfault = 0u64;
        while aux_idx < a_marks.len() && a_marks[aux_idx].0 <= f_end {
            match a_marks[aux_idx].1 {
                0 => paging += 1,
                1 => vprot += 1,
                _ => hfault += 1,
            }
            aux_idx += 1;
        }

        frames.push(FrameSummary {
            frame: fnum,
            pid: fpid,
            start_qpc: f_start,
            end_qpc: f_end,
            t_ms: qpc_to_ms(f_start - start_qpc),
            duration_ms: dur_ms,
            gpu_busy_ms,
            gpu_idle_ms,
            busiest_queue_ms: busiest_q_ms,
            queue_sum_ms: q_sum_ms,
            submit_tid,
            submit_tid_cpu_wait_ms,
            fence_wait_ms: cpu_fence_wait_ms + gpu_fence_wait_ms,
            cpu_fence_wait_ms,
            gpu_fence_wait_ms,
            os_wait_ms,
            paging_events: paging,
            virtual_protect_events: vprot,
            hard_faults: hfault,
            presents: 1,
            present_event_id: pid_ev,
        });
    }

    frames
}

fn union_interval_qpc(mut intervals: Vec<(i64, i64)>) -> i64 {
    if intervals.is_empty() {
        return 0;
    }
    intervals.sort_unstable_by_key(|&(s, _)| s);
    let mut total = 0i64;
    let (mut cur_s, mut cur_e) = intervals[0];
    for (s, e) in intervals.into_iter().skip(1) {
        if s <= cur_e {
            if e > cur_e {
                cur_e = e;
            }
        } else {
            total += (cur_e - cur_s).max(0);
            cur_s = s;
            cur_e = e;
        }
    }
    total += (cur_e - cur_s).max(0);
    total
}

fn is_device_removed_status(s: u32) -> bool {
    matches!(s, 0x887A_0005 | 0x887A_0006 | 0x887A_0007 | 0x887A_0020)
}

fn extract_trailing_utf16(data: &[u8]) -> Option<String> {
    if data.len() < 4 {
        return None;
    }
    // Find the last null-terminated UTF-16LE path containing '\\' or '.'.
    let u16s: Vec<u16> = data
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    let end = u16s.iter().rposition(|&c| c != 0).map(|i| i + 1)?;
    let start = u16s[..end]
        .iter()
        .rposition(|&c| c == 0)
        .map(|i| i + 1)
        .unwrap_or(0);
    let s = String::from_utf16_lossy(&u16s[start..end]);
    if s.contains('\\') || s.contains('.') {
        Some(s)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Offline `.etl` Timeline Parser + Live Timeline Recorder
// ---------------------------------------------------------------------------

unsafe extern "system" fn timeline_event_cb(rec: *mut EVENT_RECORD) {
    if rec.is_null() {
        return;
    }
    let rec_ref = unsafe { &*rec };
    if rec_ref.UserContext.is_null() {
        return;
    }
    let builder = unsafe { &mut *rec_ref.UserContext.cast::<TimelineBuilder>() };
    builder.on_event(rec_ref);
}

/// Parses an existing `.etl` trace onto the unified QPC timeline (zero suspends).
pub fn parse_timeline_etl(
    etl_path: &Path,
    filter_pid: Option<u32>,
    symbolizer: &mut Symbolizer,
) -> Result<TimelineReport, String> {
    if !etl_path.is_file() {
        return Err(format!("ETL file not found: {}", etl_path.display()));
    }
    let mut wide_path: Vec<u16> = etl_path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut builder = Box::new(TimelineBuilder::new(filter_pid, qpc_frequency()));
    let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
    logfile.LogFileName = PWSTR(wide_path.as_mut_ptr());
    logfile.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_EVENT_RECORD | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
    logfile.Anonymous2.EventRecordCallback = Some(timeline_event_cb);
    logfile.Context = (&mut *builder as *mut TimelineBuilder).cast::<c_void>();

    let handle: PROCESSTRACE_HANDLE = unsafe { OpenTraceW(&mut logfile) };
    if handle.Value == u64::MAX {
        return Err(format!(
            "OpenTraceW({}) failed: {}",
            etl_path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // ReservedFlags: 1 = QPC, 2 = SystemTime (100 ns FILETIME), 3 = CPU cycle counter.
    if logfile.LogfileHeader.ReservedFlags == 2 {
        builder.ts_unit = "ts_100ns".into();
        builder.qpc_freq_hz = 10_000_000;
    } else {
        builder.ts_unit = "qpc".into();
        if logfile.LogfileHeader.PerfFreq > 0 {
            builder.qpc_freq_hz = logfile.LogfileHeader.PerfFreq as u64;
        }
    }

    let handles = [handle];
    let rc = unsafe { ProcessTrace(&handles, None, None) };
    unsafe {
        let _ = CloseTrace(handle);
    }
    if rc != ERROR_SUCCESS {
        return Err(format!(
            "ProcessTrace({}) failed with Win32 error {}",
            etl_path.display(),
            rc.0
        ));
    }
    Ok(builder.finish(etl_path.display().to_string(), symbolizer, Vec::new(), None))
}

/// Live timeline capture options.
#[derive(Clone, Debug)]
pub struct LiveTimelineOptions {
    pub pid: u32,
    pub duration_secs: f64,
    pub enable_stacks: bool,
    pub max_etl_mb: u32,
    pub etl_out: Option<PathBuf>,
    pub keep_etl: bool,
}

/// Records a live non-admin unified QPC timeline (zero target suspends) and returns the symbolized report.
pub fn capture_live_timeline(opts: LiveTimelineOptions) -> Result<TimelineReport, String> {
    let (alive_at_start, start_exit_code) = probe_process_alive(opts.pid);
    if !alive_at_start {
        let code_note = start_exit_code
            .map(|c| format!(" (exit_code={c:#x})"))
            .unwrap_or_default();
        return Err(format!(
            "target pid {} exited at +0 ms{code_note} (process was not running when timeline capture started)",
            opts.pid
        ));
    }

    let is_temp = opts.etl_out.is_none();
    let etl_path = opts.etl_out.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("locum_timeline_{}_{}.etl", opts.pid, std::process::id()))
    });
    let _ = std::fs::remove_file(&etl_path);
    if let Some(parent) = etl_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create ETL directory {}: {e}", parent.display()))?;
    }

    let name = format!("locum_tl_{}_{}", std::process::id(), qpc_now() & 0xFFFF_FFFF);
    let mut warnings = Vec::new();
    let (handle, _sys_logger) = start_timeline_session(
        &name,
        Some(&etl_path),
        opts.max_etl_mb.max(32),
        opts.enable_stacks,
        false,
        &mut warnings,
    )?;

    // Snapshot modules out-of-process while target is live (zero suspends).
    let mut symbolizer = Symbolizer::for_process(opts.pid);

    let capture_start = Instant::now();
    let total_dur = Duration::from_secs_f64(opts.duration_secs.max(0.1));
    let mut exited_info: Option<(f64, Option<u32>)> = None;
    loop {
        let elapsed = capture_start.elapsed();
        let (alive, exit_code) = probe_process_alive(opts.pid);
        if !alive {
            exited_info = Some((elapsed.as_secs_f64() * 1000.0, exit_code));
            break;
        }
        if elapsed >= total_dur {
            break;
        }
        let remain = total_dur.saturating_sub(elapsed);
        std::thread::sleep(remain.min(Duration::from_millis(50)));
    }

    // Final rundown for devices/contexts/hwqueues.
    let ep = ENABLE_TRACE_PARAMETERS {
        Version: 2,
        ..Default::default()
    };
    let _ = unsafe {
        EnableTraceEx2(
            handle,
            &DXGKRNL_PROVIDER,
            EVENT_CONTROL_CODE_CAPTURE_STATE.0,
            TRACE_LEVEL_INFORMATION,
            DXGKRNL_KEYWORDS_TIMELINE,
            0,
            0,
            Some(&ep),
        )
    };
    std::thread::sleep(Duration::from_millis(100));
    stop_session_handle(handle, &name);

    let mut rep = parse_timeline_etl(&etl_path, Some(opts.pid), &mut symbolizer)?;
    if let Some((exit_ms, exit_code)) = exited_info {
        rep.target_exited_at_ms = Some(exit_ms);
        rep.target_exit_code = exit_code;
        let code_note = exit_code
            .map(|c| format!(" (exit_code={c:#x})"))
            .unwrap_or_default();
        warnings.push(format!(
            "target pid {} exited at +{exit_ms:.0} ms{code_note} during capture",
            opts.pid
        ));
    }
    rep.warnings.extend(warnings);
    if is_temp && !opts.keep_etl {
        let _ = std::fs::remove_file(&etl_path);
    }
    Ok(rep)
}

// ---------------------------------------------------------------------------
// Shared ETW Session Helpers (used by `timeline` and `flight`)
// ---------------------------------------------------------------------------

pub(crate) fn build_session_properties(
    name: &str,
    etl_path: Option<&Path>,
    max_file_mb: u32,
    log_file_mode: u32,
) -> Vec<u8> {
    let name_utf16: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    let path_utf16: Vec<u16> = etl_path
        .map(|p| {
            p.as_os_str()
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect()
        })
        .unwrap_or_default();

    let base_size = size_of::<EVENT_TRACE_PROPERTIES>();
    let name_slot = (name_utf16.len() * 2).max(512);
    let file_slot = (path_utf16.len() * 2).max(1024);
    let logger_off = base_size;
    let file_off = logger_off + name_slot;
    let total_size = file_off + file_slot;

    let mut buf = vec![0u8; total_size];
    let p = buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
    unsafe {
        (*p).Wnode.BufferSize = total_size as u32;
        (*p).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
        (*p).Wnode.ClientContext = 1; // QPC clock!
        (*p).BufferSize = 256;
        (*p).MinimumBuffers = 8;
        (*p).MaximumBuffers = 64;
        (*p).MaximumFileSize = max_file_mb;
        (*p).LogFileMode = log_file_mode;
        (*p).FlushTimer = 1;
        (*p).LoggerNameOffset = logger_off as u32;
        (*p).LogFileNameOffset = if etl_path.is_some() { file_off as u32 } else { 0 };
        std::ptr::copy_nonoverlapping(
            name_utf16.as_ptr().cast::<u8>(),
            buf.as_mut_ptr().add(logger_off),
            name_utf16.len() * 2,
        );
        if !path_utf16.is_empty() {
            std::ptr::copy_nonoverlapping(
                path_utf16.as_ptr().cast::<u8>(),
                buf.as_mut_ptr().add(file_off),
                path_utf16.len() * 2,
            );
        }
    }
    buf
}

pub(crate) fn start_timeline_session(
    name: &str,
    etl_path: Option<&Path>,
    max_file_mb: u32,
    enable_stacks: bool,
    real_time: bool,
    warnings: &mut Vec<String>,
) -> Result<(CONTROLTRACE_HANDLE, bool), String> {
    const EVENT_TRACE_REAL_TIME_MODE: u32 = 0x0000_0100;
    let base_mode = if real_time {
        EVENT_TRACE_REAL_TIME_MODE
    } else {
        EVENT_TRACE_FILE_MODE_SEQUENTIAL
    };

    let name_w = HSTRING::from(name);
    let mut handle = CONTROLTRACE_HANDLE::default();
    let mut props = build_session_properties(
        name,
        etl_path,
        max_file_mb,
        base_mode | EVENT_TRACE_SYSTEM_LOGGER_MODE,
    );
    let mut rc = unsafe { StartTraceW(&mut handle, &name_w, props.as_mut_ptr().cast()) };
    if rc == ERROR_ALREADY_EXISTS {
        stop_session_by_name(name);
        rc = unsafe { StartTraceW(&mut handle, &name_w, props.as_mut_ptr().cast()) };
    }
    let mut sys_logger = rc == ERROR_SUCCESS;
    if !sys_logger {
        props = build_session_properties(name, etl_path, max_file_mb, base_mode);
        rc = unsafe { StartTraceW(&mut handle, &name_w, props.as_mut_ptr().cast()) };
        if rc != ERROR_SUCCESS {
            return Err(format!(
                "StartTraceW({name}) failed with Win32 error {} (add account to Performance Log Users)",
                rc.0
            ));
        }
        sys_logger = false;
    }

    let stack_prop = if enable_stacks {
        EVENT_ENABLE_PROPERTY_STACK_TRACE
    } else {
        0
    };
    let ep_stack = ENABLE_TRACE_PARAMETERS {
        Version: 2,
        EnableProperty: stack_prop,
        ..Default::default()
    };
    let ep_plain = ENABLE_TRACE_PARAMETERS {
        Version: 2,
        ..Default::default()
    };

    // Required: DxgKrnl (queues, fences, presents, paging, TDR) + initial CAPTURE_STATE rundown.
    let e_dxg = unsafe {
        EnableTraceEx2(
            handle,
            &DXGKRNL_PROVIDER,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            TRACE_LEVEL_INFORMATION,
            DXGKRNL_KEYWORDS_TIMELINE,
            0,
            0,
            Some(&ep_stack),
        )
    };
    if e_dxg != ERROR_SUCCESS {
        stop_session_handle(handle, name);
        return Err(format!(
            "EnableTraceEx2(DxgKrnl) failed with Win32 error {}",
            e_dxg.0
        ));
    }
    let _ = unsafe {
        EnableTraceEx2(
            handle,
            &DXGKRNL_PROVIDER,
            EVENT_CONTROL_CODE_CAPTURE_STATE.0,
            TRACE_LEVEL_INFORMATION,
            DXGKRNL_KEYWORDS_TIMELINE,
            0,
            0,
            Some(&ep_plain),
        )
    };

    // Best-effort non-admin memory providers.
    let _ = unsafe {
        EnableTraceEx2(
            handle,
            &SYSTEM_MEMORY_PROVIDER,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            TRACE_LEVEL_INFORMATION,
            SYSTEM_MEMORY_KW_TIMELINE,
            0,
            0,
            Some(&ep_stack),
        )
    };
    let _ = unsafe {
        EnableTraceEx2(
            handle,
            &KERNEL_MEMORY_PROVIDER,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            TRACE_LEVEL_INFORMATION,
            0xFF,
            0,
            0,
            Some(&ep_stack),
        )
    };

    // Best-effort scheduler / profile / process providers (succeed when ACL/elevated; clean fallback non-admin).
    let e_sched = unsafe {
        EnableTraceEx2(
            handle,
            &SYSTEM_SCHEDULER_PROVIDER,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            TRACE_LEVEL_INFORMATION,
            SYSTEM_SCHEDULER_KW_WAIT,
            0,
            0,
            Some(&ep_stack),
        )
    };
    if e_sched.0 == 5 {
        warnings.push(
            "SystemSchedulerProvider (CSwitch/ReadyThread) denied non-admin (Win32 error 5): using non-admin DxgKrnl fence/wait + CPU stack attribution".into(),
        );
    }
    let _ = unsafe {
        EnableTraceEx2(
            handle,
            &SYSTEM_PROFILE_PROVIDER,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            TRACE_LEVEL_INFORMATION,
            0x1,
            0,
            0,
            Some(&ep_stack),
        )
    };
    let _ = unsafe {
        EnableTraceEx2(
            handle,
            &SYSTEM_PROCESS_PROVIDER,
            EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
            TRACE_LEVEL_INFORMATION,
            0x13,
            0,
            0,
            Some(&ep_plain),
        )
    };

    Ok((handle, sys_logger))
}

pub(crate) fn stop_session_handle(handle: CONTROLTRACE_HANDLE, name: &str) {
    let mut stop_buf = build_session_properties(name, None, 0, 0);
    let name_w = HSTRING::from(name);
    let _ = unsafe {
        ControlTraceW(
            handle,
            &name_w,
            stop_buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>(),
            EVENT_TRACE_CONTROL_STOP,
        )
    };
}

pub(crate) fn stop_session_by_name(name: &str) {
    stop_session_handle(CONTROLTRACE_HANDLE::default(), name);
}

// ---------------------------------------------------------------------------
// Greppable Text Formatter (NOT a raw ETL dumper!)
// ---------------------------------------------------------------------------

pub fn format_timeline_text(rep: &TimelineReport, opts: &TimelineFormatOptions) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let pid_s = rep.pid.map(|p| p.to_string()).unwrap_or_else(|| "all".into());
    let fpid_s = rep
        .frame_pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "none".into());
    let stid_s = rep
        .submit_tid
        .map(|t| t.to_string())
        .unwrap_or_else(|| "none".into());
    let stacks_s = if rep.stack_samples_captured == 0 {
        "0(no_stack_events)".to_string()
    } else {
        rep.stack_samples_captured.to_string()
    };
    let waits_s = if rep.measured_waits == 0 && !rep.waits.is_empty() {
        "0(count-only:no_wait_end_pairs)".to_string()
    } else {
        rep.measured_waits.to_string()
    };
    let _ = writeln!(
        out,
        "HEADER source={} pid={} frame_pid={} submit_tid={} trace_dur_ms={:.2} target_span_ms={:.2} ts_unit={} freq_hz={} raw_events={} target_events={}{} dxg_pid_ev={} dxg_ctx_other_pid={} stacks={} measured_waits={} wait_mode={} providers={} thread_suspends={}",
        rep.source,
        pid_s,
        fpid_s,
        stid_s,
        rep.duration_ms,
        rep.target_span_ms,
        rep.ts_unit,
        rep.qpc_freq_hz,
        rep.total_raw_events,
        rep.target_events,
        if rep.ev_truncated { "(ev_capped)" } else { "" },
        rep.dxg_pid_events,
        rep.dxg_ctx_other_pid_events,
        stacks_s,
        waits_s,
        rep.wait_attribution_mode,
        rep.providers_active.join(","),
        rep.thread_suspends,
    );
    if let Some(ref trig) = rep.flight_trigger {
        let cap_note = if trig.capped_by_event_limit && trig.retained_secs > 0.01 {
            let rate = rep.target_events as f64 / trig.retained_secs;
            let needed = ((rate * trig.ring_secs * 1.15).ceil() as usize).max(trig.max_ring_events);
            format!(
                "(capped_by_max_ring_events={}; ~{rate:.0} ev/s -> pass --max-ring-events {needed} for {:.0}s)",
                trig.max_ring_events, trig.ring_secs
            )
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "TRIGGER reason={:?} trigger_qpc={} ring_secs={:.1} retained_secs={:.2}{} observed={} evicted={}",
            trig.reason,
            trig.trigger_qpc,
            trig.ring_secs,
            trig.retained_secs,
            cap_note,
            trig.total_observed_events,
            trig.evicted_ring_events,
        );
        if let Some(ref pkt) = trig.stalled_packet {
            let _ = writeln!(out, "STALLED_PKT {}", pkt.format_compact());
        }
        for sib in &trig.sibling_stalled_packets {
            let _ = writeln!(out, "STALLED_SIBLING {}", sib.format_compact());
        }
        if let Some(ref wpkt) = trig.oldest_wait_packet {
            let _ = writeln!(out, "IN_FLIGHT_WAIT {}", wpkt.format_compact());
        }
    }
    for w in &rep.warnings {
        let _ = writeln!(out, "WARN {w}");
    }
    for a in &rep.alerts {
        let _ = writeln!(out, "ALERT {a}");
    }

    if !rep.pids.is_empty() && (opts.list_pids || rep.pid.is_none()) {
        let _ = writeln!(out, "\n== Active Processes (DxgKrnl) ==");
        for p in rep.pids.iter().take(opts.top.max(12)) {
            let name = p.process_name.as_deref().unwrap_or("-");
            let _ = writeln!(
                out,
                "PID {:<6} name={:<24} submits={:<7} presents={:<5} fence_waits={:<7} fence_sigs={:<7} paging={:<7} dxg_ev={}",
                p.pid,
                name,
                p.submits,
                p.presents,
                p.fence_waits,
                p.fence_signals,
                p.paging_events,
                p.dxg_events
            );
        }
        if opts.list_pids {
            return out;
        }
    }

    if !rep.queues.is_empty() {
        let _ = writeln!(out, "\n== GPU Hardware Queues (union busy / signals / waits) ==");
        for q in rep.queues.iter().take(opts.top) {
            let pname = q
                .process_name
                .as_deref()
                .map(|n| format!("({n})"))
                .unwrap_or_default();
            let sib = q
                .sibling_note
                .as_deref()
                .map(|s| format!(" [{s}]"))
                .unwrap_or_default();
            let stale = if q.stale_in_window {
                " [stale:idle_in_window]"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "QUEUE {:<20} pid={:<6}{:<16} pkts={}/{} in_flight={} busy_ms={:>8.2} ({:>5.1}%) max_pkt_ms={:>7.2} sig={} wait={}{}{}",
                q.queue_id,
                q.pid,
                pname,
                q.packets_completed,
                q.packets_started,
                q.in_flight_at_end,
                q.busy_ms,
                q.busy_pct,
                q.max_packet_ms,
                q.fence_signals,
                q.fence_waits,
                sib,
                stale,
            );
        }
    }

    if !rep.waits.is_empty() {
        let _ = writeln!(
            out,
            "\n== Wait Attribution (per thread; wall_ms=union <= trace_dur) =="
        );
        for w in rep.waits.iter().take(opts.top) {
            let waker = w.top_waker.as_deref().unwrap_or("-");
            let stack = w.top_stack.as_deref().unwrap_or("-");
            if w.measured_count == 0 {
                let _ = writeln!(
                    out,
                    "WAIT tid={:<6} kind={:<22} n={:<6} wall_ms=n/a      sum_ms=n/a(count-only) max_ms=n/a     waker={:<18} stack={}",
                    w.tid, w.kind, w.count, waker, stack
                );
            } else {
                let _ = writeln!(
                    out,
                    "WAIT tid={:<6} kind={:<22} n={:<6} measured={:<6} wall_ms={:>8.2} sum_ms={:>10.2} max_ms={:>7.2} waker={:<18} stack={}",
                    w.tid, w.kind, w.count, w.measured_count, w.wall_ms, w.total_ms, w.max_ms, waker, stack
                );
            }
        }
    }

    if !rep.top_stacks.is_empty() {
        let _ = writeln!(out, "\n== Top Symbolized CPU Stacks ==");
        for s in rep.top_stacks.iter().take(opts.top) {
            let _ = writeln!(out, "STACK n={:<5} kind={:<12} {}", s.count, s.kind, s.stack);
        }
    }

    if !rep.frames.is_empty() {
        let _ = writeln!(
            out,
            "\n== Per-Present Frames (pid={}, submit_tid={}, clipped to frame bounds; gpu_busy+gpu_idle=dur) ==",
            fpid_s, stid_s
        );
        for f in rep.frames.iter().take(opts.top.max(30)) {
            let _ = writeln!(
                out,
                "FRAME f={:<4} pid={:<6} ev_id={:<3} t_ms={:>9.2} dur_ms={:>7.2} gpu_busy_ms={:>7.2} gpu_idle_ms={:>7.2} busiest_q_ms={:>7.2} sub_cpu_wait_ms={:>6.2} all_cpu_wait_ms={:>6.2} gpu_wait_ms={:>6.2} paging={} vprot={} hfault={}",
                f.frame,
                f.pid,
                f.present_event_id,
                f.t_ms,
                f.duration_ms,
                f.gpu_busy_ms,
                f.gpu_idle_ms,
                f.busiest_queue_ms,
                f.submit_tid_cpu_wait_ms,
                f.cpu_fence_wait_ms,
                f.gpu_fence_wait_ms,
                f.paging_events,
                f.virtual_protect_events,
                f.hard_faults,
            );
        }
    }

    let filtered: Vec<&TimelineEvent> = rep
        .events
        .iter()
        .filter(|e| {
            opts.kind.is_none_or(|k| e.kind == k)
                && opts.tid.is_none_or(|t| e.tid == t)
                && opts.min_ms.is_none_or(|m| e.duration_ms.unwrap_or(0.0) >= m)
        })
        .collect();
    if !filtered.is_empty() && opts.limit > 0 {
        // Collapse consecutive bursts sharing (kind, tid, detail, top_stack) within 1.0 ms.
        let mut collapsed: Vec<(&TimelineEvent, usize, f64)> = Vec::new();
        for &e in &filtered {
            let top_s = e.stack.first().map(String::as_str);
            if let Some((prev, cnt, end_t)) = collapsed.last_mut()
                && prev.kind == e.kind
                && prev.tid == e.tid
                && prev.detail == e.detail
                && prev.duration_ms.is_none()
                && e.duration_ms.is_none()
                && prev.stack.first().map(String::as_str) == top_s
                && (e.t_ms - *end_t).abs() <= 1.0
            {
                *cnt += 1;
                *end_t = e.t_ms;
            } else {
                collapsed.push((e, 1, e.t_ms));
            }
        }
        let _ = writeln!(
            out,
            "\n== Unified Timeline Events (showing {} of {} collapsed lines from {} matching) ==",
            opts.limit.min(collapsed.len()),
            collapsed.len(),
            filtered.len(),
        );
        for &(e, cnt, end_t) in collapsed.iter().take(opts.limit) {
            let dur = e
                .duration_ms
                .map(|d| format!(" dur={d:.3}ms"))
                .unwrap_or_default();
            let q = e.queue.as_deref().map(|q| format!(" {q}")).unwrap_or_default();
            let burst = if cnt > 1 {
                format!(" x{cnt} (span={:.3}ms)", (end_t - e.t_ms).max(0.0))
            } else {
                String::new()
            };
            let stk = if e.stack.is_empty() {
                String::new()
            } else {
                format!(
                    " stack={}",
                    e.stack.iter().take(4).cloned().collect::<Vec<_>>().join(" <- ")
                )
            };
            let _ = writeln!(
                out,
                "EV t=+{:>9.3}ms {}={} f={:<4} tid={:<6} [{}]{}{} {}{}{}",
                e.t_ms,
                rep.ts_unit,
                e.qpc,
                e.frame,
                e.tid,
                e.kind.tag(),
                q,
                dur,
                e.detail,
                burst,
                stk,
            );
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unified_qpc_timeline_correlates_queues_fences_frames_and_waits() {
        let freq = 10_000_000u64; // 10 MHz (10,000 ticks = 1 ms)
        let mut b = TimelineBuilder::new(Some(42), freq);
        b.start_qpc = Some(100_000);
        b.end_qpc = Some(300_000);
        b.saw_dxgkrnl = true;
        b.saw_cswitch = true;
        b.saw_ready_thread = true;
        b.queues.insert(
            0x1000,
            QueueSummary {
                queue_id: "ctx=0x1000/n0".into(),
                context: 0x1000,
                pid: 42,
                process_name: Some("xcl.exe".into()),
                node_ordinal: Some(0),
                engine_type: Some(0),
                packets_started: 2,
                packets_completed: 2,
                in_flight_at_end: 0,
                busy_ms: 8.5,
                busy_pct: 0.0,
                max_packet_ms: 5.0,
                fence_signals: 2,
                fence_waits: 1,
                stale_in_window: false,
                sibling_note: None,
            },
        );
        b.push_event(RawTimelineEvent {
            qpc: 100_000,
            frame: 0,
            pid: 42,
            tid: 1001,
            kind: TimelineEventKind::GpuBusy,
            queue: Some("ctx=0x1000/n0".into()),
            duration_qpc: Some(50_000),
            detail: "queue_pkt type=0 seq=1".into(),
            raw_stack: vec![0xFFFF_F804_CF8F_8208, 0x7FF6_1000_1234],
            waker_tid: None,
            wait_kind: None,
        });
        b.push_event(RawTimelineEvent {
            qpc: 150_000,
            frame: 0,
            pid: 42,
            tid: 1001,
            kind: TimelineEventKind::FenceWait,
            queue: Some("ctx=0x1000/n0".into()),
            duration_qpc: Some(25_000),
            detail: "cpu_wait_done sync=0xabc val=1->1 by_tid=1002".into(),
            raw_stack: vec![0xFFFF_F804_CF8F_8208, 0x7FF6_1000_1234],
            waker_tid: Some(1002),
            wait_kind: Some("dxg_fence_cpu_wait".into()),
        });
        b.push_event(RawTimelineEvent {
            qpc: 266_700,
            frame: 0,
            pid: 42,
            tid: 1001,
            kind: TimelineEventKind::Present,
            queue: Some("ctx=0x1000/n0".into()),
            duration_qpc: Some(166_700),
            detail: "id=184 status=0x0".into(),
            raw_stack: Vec::new(),
            waker_tid: None,
            wait_kind: None,
        });

        let mut sym = Symbolizer::new(None);
        sym.add_etw_image(0x7FF6_1000_0000, 0x100_000, r"C:\Games\xcl.exe");
        let rep = b.finish("unit.etl".into(), &mut sym, Vec::new(), None);
        assert_eq!(rep.queues.len(), 1);
        assert_eq!(rep.frames.len(), 1);
        assert!(
            rep.frames[0].gpu_busy_ms <= rep.frames[0].duration_ms,
            "clipped frame gpu_busy_ms must never exceed dur_ms"
        );
        assert!(
            (rep.frames[0].gpu_busy_ms + rep.frames[0].gpu_idle_ms - rep.frames[0].duration_ms).abs() < 1e-6
        );
        assert_eq!(rep.submit_tid, Some(1001));
        assert!(rep.frames[0].submit_tid_cpu_wait_ms > 0.0);
        assert_eq!(rep.waits.len(), 1);
        assert_eq!(rep.waits[0].measured_count, 1);
        assert!(rep.waits[0].wall_ms <= rep.duration_ms);
        assert_eq!(rep.waits[0].top_waker.as_deref(), Some("tid=1002 (1x)"));
        // Kernel ETW prefix must be skipped in favor of the user-mode frame!
        assert_eq!(rep.events[0].stack, vec!["xcl.exe+0x1234"]);

        let text = format_timeline_text(&rep, &TimelineFormatOptions::default());
        assert!(text.contains("HEADER source=unit.etl pid=42"));
        assert!(text.contains("submit_tid=1001"));
        assert!(text.contains("QUEUE ctx=0x1000/n0"));
        assert!(text.contains("WAIT tid=1001"));
        assert!(text.contains("wall_ms="));
        assert!(text.contains("FRAME f=0"));
        assert!(text.contains("gpu_idle_ms="));
        assert!(text.contains("sub_cpu_wait_ms="));
        assert!(text.contains("[GPU_BUSY]"));
        assert!(text.contains("xcl.exe+0x1234"));
    }

    #[test]
    fn ring_eviction_keeps_only_last_window_events() {
        let freq = 10_000_000u64;
        let mut b = TimelineBuilder::new(Some(7), freq).with_ring(10.0, 10_000);
        for i in 0..1500i64 {
            let qpc = 1_000_000 + i * (20 * freq as i64 / 1500);
            b.end_qpc = Some(qpc);
            b.push_event(RawTimelineEvent {
                qpc,
                frame: 0,
                pid: 7,
                tid: 1,
                kind: TimelineEventKind::Paging,
                queue: None,
                duration_qpc: None,
                detail: format!("ev={i}"),
                raw_stack: Vec::new(),
                waker_tid: None,
                wait_kind: None,
            });
        }
        assert!(b.evicted_events > 0, "events older than 10s ring must evict");
        let span_secs =
            (b.raw_events.back().unwrap().qpc - b.raw_events.front().unwrap().qpc) as f64 / freq as f64;
        assert!(
            span_secs <= 10.5,
            "retained ring span should be ~10s, got {span_secs:.2}s"
        );
    }

    #[test]
    fn fifo_prune_drops_missed_older_packets_and_formats_stalled_wait_detail() {
        let freq = 10_000_000u64;
        let mut b = TimelineBuilder::new(Some(99), freq);
        // Simulate seq=10 whose stop event was missed, plus seq=15 and seq=16 on ctx=0xabc.
        b.in_flight_queue_pkts.insert((0xabc, 10), (100_000, 99, 1, 0));
        b.in_flight_queue_pkts.insert((0xabc, 15), (200_000, 99, 1, 0));
        b.in_flight_queue_pkts.insert((0xabc, 16), (300_000, 99, 1, 0));
        b.in_flight_wait_pkts
            .insert((0xdef, 42), (150_000, 99, 2, 0xfeed_face, 777));

        // Completing seq=15 must prune missed seq=10 while keeping newer seq=16.
        b.in_flight_queue_pkts.remove(&(0xabc, 15));
        TimelineBuilder::prune_older_in_flight_seq(&mut b.in_flight_queue_pkts, 0xabc, 15);
        assert!(!b.in_flight_queue_pkts.contains_key(&(0xabc, 10)));
        assert!(b.in_flight_queue_pkts.contains_key(&(0xabc, 16)));

        let (work, sibs) = b
            .oldest_in_flight_work_packet_group(100_300_000, None)
            .expect("seq 16 still in-flight");
        assert_eq!(work.seq, 16);
        assert_eq!(work.context, 0xabc);
        assert!(sibs.is_empty());
        assert!(work.format_compact().contains("kind=queue_pkt(type=0)"));
        assert!(work.format_compact().contains("submit_qpc=300000"));

        let wait = b
            .oldest_in_flight_wait_packet(100_300_000)
            .expect("wait pkt in-flight");
        assert_eq!(wait.sync_obj, Some(0xfeed_face));
        assert_eq!(wait.fence_val, Some(777));
        assert!(wait.format_compact().contains("sync=0xfeedface val=777"));
    }

    #[test]
    fn sibling_stall_contexts_merge_into_one_trigger_and_suppress_duplicate_rearm() {
        let freq = 10_000_000u64;
        let mut b = TimelineBuilder::new(Some(27408), freq);
        // Mirror NR2 flight.1/flight.2: two sibling contexts submitted 108 ticks (10.8 us) apart by tid 14412.
        b.in_flight_queue_pkts
            .insert((0xffff_d58f_e4b3_1470, 36), (817_820_465_605, 27408, 14412, 0));
        b.in_flight_queue_pkts
            .insert((0xffff_d58f_d9b4_0a90, 18), (817_820_465_713, 27408, 14412, 0));
        // A later packet queued behind seq=36 on the same stalled context.
        b.in_flight_queue_pkts
            .insert((0xffff_d58f_e4b3_1470, 37), (817_840_000_000, 27408, 14412, 0));

        let now = 817_920_901_919;
        let (primary, siblings) = b
            .oldest_in_flight_work_packet_group(now, None)
            .expect("stalled group present");
        assert_eq!(primary.context, 0xffff_d58f_e4b3_1470);
        assert_eq!(primary.seq, 36);
        assert_eq!(siblings.len(), 1);
        assert_eq!(siblings[0].context, 0xffff_d58f_d9b4_0a90);
        assert_eq!(siblings[0].seq, 18);

        // Once primary + sibling are acknowledged, neither the sibling nor seq=37 on the same stalled ctx re-fires!
        let mut acks = HashSet::new();
        acks.insert((primary.context, primary.seq));
        for s in &siblings {
            acks.insert((s.context, s.seq));
        }
        assert!(
            b.oldest_in_flight_work_packet_group(now + 10_000_000, Some(&acks))
                .is_none(),
            "acked sibling and FIFO-blocked packet on same stalled ctx must not fire duplicate trigger"
        );

        // Once the stalled packet completes and is removed, a brand new stall on that ctx WILL fire again.
        b.in_flight_queue_pkts.remove(&(0xffff_d58f_e4b3_1470, 36));
        b.in_flight_queue_pkts.remove(&(0xffff_d58f_e4b3_1470, 37));
        b.in_flight_queue_pkts
            .insert((0xffff_d58f_e4b3_1470, 100), (now + 50_000_000, 27408, 14412, 0));
        let (next, _) = b
            .oldest_in_flight_work_packet_group(now + 200_000_000, Some(&acks))
            .expect("new stall after recovery must trigger");
        assert_eq!(next.seq, 100);
    }

    #[test]
    fn ring_snapshot_zeroes_idle_queue_busy_and_marks_stale_rows() {
        let freq = 10_000_000u64;
        let mut b = TimelineBuilder::new(Some(27408), freq).with_ring(10.0, 10_000);
        // Historical queue with huge cumulative busy_ms from earlier in the run, 0 packets in the current ring window.
        b.queues.insert(
            0xAAAA,
            QueueSummary {
                queue_id: "ctx=0xaaaa".into(),
                context: 0xAAAA,
                pid: 27408,
                process_name: Some("xcl.exe".into()),
                packets_started: 152_098,
                packets_completed: 152_098,
                busy_ms: 85_000.0,
                max_packet_ms: 163.85,
                ..Default::default()
            },
        );
        // Active queue that ran 1 short packet inside the ring window (even with /n0 label suffix!).
        b.queues.insert(
            0xBBBB,
            QueueSummary {
                queue_id: "ctx=0xbbbb".into(),
                context: 0xBBBB,
                pid: 27408,
                process_name: Some("xcl.exe".into()),
                packets_started: 4,
                packets_completed: 4,
                busy_ms: 0.4,
                max_packet_ms: 0.15,
                ..Default::default()
            },
        );
        // Ring spans 9.73 s (97_300_000 ticks), only 0xBBBB has a completed packet in window.
        b.end_qpc = Some(100_000_000);
        b.push_event(RawTimelineEvent {
            qpc: 2_700_000,
            frame: 1730,
            pid: 27408,
            tid: 1,
            kind: TimelineEventKind::GpuBusy,
            queue: Some("ctx=0xbbbb/n0".into()),
            duration_qpc: Some(1_500), // 0.15 ms
            detail: "hwq_dma pf=1".into(),
            raw_stack: Vec::new(),
            waker_tid: None,
            wait_kind: None,
        });
        b.push_event(RawTimelineEvent {
            qpc: 100_000_000,
            frame: 1730,
            pid: 27408,
            tid: 1,
            kind: TimelineEventKind::VirtualProtect,
            queue: None,
            duration_qpc: None,
            detail: "KernelMemory id=1".into(),
            raw_stack: Vec::new(),
            waker_tid: None,
            wait_kind: None,
        });

        let mut sym = Symbolizer::new(None);
        let rep = b.snapshot_report("flight_ring(pid=27408, 10s)".into(), &mut sym, Vec::new(), None);
        assert_eq!(rep.queues.len(), 2);
        // Active queue must sort FIRST with its exact 0.15 ms window busy, not buried below stale queues!
        assert_eq!(rep.queues[0].context, 0xBBBB);
        assert!(!rep.queues[0].stale_in_window);
        assert!((rep.queues[0].busy_ms - 0.15).abs() < 1e-4);

        // Idle historical queue must have busy_ms == 0.0 (NOT 9730.0 / 100%!) and be marked stale.
        assert_eq!(rep.queues[1].context, 0xAAAA);
        assert!(rep.queues[1].stale_in_window);
        assert_eq!(rep.queues[1].busy_ms, 0.0);
        assert_eq!(rep.queues[1].busy_pct, 0.0);

        let text = format_timeline_text(&rep, &TimelineFormatOptions::default());
        assert!(text.contains("[stale:idle_in_window]"));
        assert!(!text.contains("100.0%"));
    }
}
