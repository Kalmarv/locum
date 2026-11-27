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

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, ENABLE_TRACE_PARAMETERS,
    EVENT_CONTROL_CODE_CAPTURE_STATE, EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_HEADER_FLAG_32_BIT_HEADER,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_FILE_MODE_SEQUENTIAL, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_PROPERTIES, EVENT_TRACE_SYSTEM_LOGGER_MODE, EnableTraceEx2, OpenTraceW,
    PROCESS_TRACE_MODE_EVENT_RECORD, PROCESSTRACE_HANDLE, ProcessTrace, StartTraceW, TRACE_EVENT_INFO,
    TdhGetEventInformation, WNODE_FLAG_TRACED_GUID,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::core::{GUID, HSTRING, PWSTR};

use crate::THREAD_SUSPENDS;
use crate::etw::{CLASSIC_PAGEFAULT_GUID, DXGKRNL_PROVIDER, SYSTEM_MEMORY_PROVIDER};
use crate::symbols::Symbolizer;

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
}

/// Per-thread wait attribution summary (CSwitch+ReadyThread when available, or DxgKrnl fence/MakeResident waits).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WaitSummary {
    pub tid: u32,
    pub kind: String,
    pub count: u64,
    pub total_ms: f64,
    pub max_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_waker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_stack: Option<String>,
}

/// Per-present frame slice on the unified QPC clock.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FrameSummary {
    pub frame: u64,
    pub start_qpc: i64,
    pub t_ms: f64,
    pub duration_ms: f64,
    pub gpu_busy_ms: f64,
    pub fence_wait_ms: f64,
    pub os_wait_ms: f64,
    pub paging_events: u64,
    pub virtual_protect_events: u64,
    pub hard_faults: u64,
    pub presents: u64,
}

/// Top symbolized CPU stack summary.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackSummary {
    pub count: u64,
    pub kind: String,
    pub stack: String,
}

/// Trigger metadata when produced by the 10-second flight recorder.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FlightTriggerInfo {
    pub reason: String,
    pub trigger_qpc: i64,
    pub ring_secs: f64,
    pub total_observed_events: u64,
    pub evicted_ring_events: u64,
}

/// Complete unified QPC timeline report (text-greppable + JSON).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TimelineReport {
    pub source: String,
    pub pid: Option<u32>,
    pub thread_suspends: u64,
    pub qpc_freq_hz: u64,
    pub start_qpc: i64,
    pub end_qpc: i64,
    pub duration_ms: f64,
    pub total_raw_events: u64,
    pub target_events: usize,
    pub stack_samples_captured: u64,
    pub wait_attribution_mode: String,
    pub providers_active: Vec<String>,
    pub warnings: Vec<String>,
    pub alerts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flight_trigger: Option<FlightTriggerInfo>,
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
}

impl Default for TimelineFormatOptions {
    fn default() -> Self {
        Self {
            min_ms: None,
            kind: None,
            tid: None,
            top: 20,
            limit: 120,
        }
    }
}

// ---------------------------------------------------------------------------
// Internal Raw Event Decoder (shared by offline `.etl`, live `timeline`, and `flight` ring!)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct FieldSpec {
    offset: usize,
    size: usize,
}

#[derive(Clone, Debug)]
struct CachedSchema {
    fields: HashMap<String, FieldSpec>,
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
        let mut fields = HashMap::new();
        let top_count = unsafe { (*info).TopLevelPropertyCount } as usize;
        let props_ptr = unsafe {
            std::ptr::addr_of!((*info).EventPropertyInfoArray)
                .cast::<windows::Win32::System::Diagnostics::Etw::EVENT_PROPERTY_INFO>()
        };
        let mut offset = 0usize;
        for i in 0..top_count {
            let pi = unsafe { &*props_ptr.add(i) };
            if (pi.Flags.0 & 0x1) != 0 {
                break;
            }
            let count = unsafe { pi.Anonymous2.count };
            let is_array = (pi.Flags.0 & 0x4) != 0 || count != 1;
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
            fields.insert(name, FieldSpec { offset, size: fsize });
            if is_array {
                // Element 0 of the first array (e.g. ObjectHandleArray / BroadcastContext) is now recorded;
                // subsequent offsets depend on runtime array length, so stop here.
                break;
            }
            offset += fsize;
        }
        Some(Self { fields })
    }

    #[inline]
    fn get_u64(&self, data: &[u8], name: &str) -> Option<u64> {
        let f = self.fields.get(name)?;
        if f.offset + f.size > data.len() {
            return None;
        }
        let s = &data[f.offset..f.offset + f.size];
        match f.size {
            1 => Some(s[0] as u64),
            2 => Some(u16::from_le_bytes(s.try_into().ok()?) as u64),
            4 => Some(u32::from_le_bytes(s.try_into().ok()?) as u64),
            8 => Some(u64::from_le_bytes(s.try_into().ok()?)),
            _ => None,
        }
    }

    #[inline]
    fn get_any_u64(&self, data: &[u8], names: &[&str]) -> Option<u64> {
        for &n in names {
            if let Some(v) = self.get_u64(data, n) {
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

type PendingFenceWait = (i64, u32, u32, Option<String>, u64, Vec<u64>);
type WaitAccEntry = (u64, f64, f64, HashMap<u32, u64>, HashMap<String, u64>);

/// Stateful streaming QPC timeline builder used by both offline `.etl` parsing and the live 10s flight ring.
pub(crate) struct TimelineBuilder {
    pub filter_pid: Option<u32>,
    pub qpc_freq_hz: u64,
    schemas: HashMap<SchemaKey, Option<CachedSchema>>,
    pub start_qpc: Option<i64>,
    pub end_qpc: Option<i64>,
    pub total_raw_events: u64,
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
    /// `(hContext, submit_seq)` -> `(start_qpc, tid, packet_type)`
    in_flight_queue_pkts: HashMap<(u64, u64), (i64, u32, u64)>,
    /// `(hContext, submit_seq)` -> `(start_qpc, tid)`
    in_flight_dma_pkts: HashMap<(u64, u64), (i64, u32)>,
    /// `sync_obj` -> `(wait_qpc, pid, tid, queue, wait_val, raw_stack)`
    pending_fence_waits: HashMap<u64, PendingFenceWait>,
    /// `tid` -> `(switch_out_qpc, stack)` for CSwitch wait attribution
    off_cpu_threads: HashMap<u32, (i64, Vec<u64>)>,
    /// `tid` -> `waker_tid` from ReadyThread
    last_waker: HashMap<u32, u32>,
    /// ETW `ImageLoad` modules observed for offline symbolization
    pub etw_images: Vec<(u64, u64, String)>,

    pub current_frame: u64,
    pub last_present_qpc: Option<i64>,
    pub last_submit_qpc: Option<i64>,
    pub last_complete_qpc: Option<i64>,
    pub total_target_submits: u64,
    pub tdr_or_removal_alert: Option<(i64, String)>,

    pub alerts: Vec<String>,
    pub queues: BTreeMap<u64, QueueSummary>,
    pub raw_events: Vec<RawTimelineEvent>,
    /// Optional ring capacity (`None` = keep up to 200k events; `Some((max_events, ring_qpc_window))` = evict older).
    pub ring_window_qpc: Option<i64>,
    pub max_retained_events: usize,
    pub evicted_events: u64,
}

impl TimelineBuilder {
    pub fn new(filter_pid: Option<u32>, qpc_freq_hz: u64) -> Self {
        Self {
            filter_pid,
            qpc_freq_hz: qpc_freq_hz.max(1),
            schemas: HashMap::new(),
            start_qpc: None,
            end_qpc: None,
            total_raw_events: 0,
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
            in_flight_queue_pkts: HashMap::new(),
            in_flight_dma_pkts: HashMap::new(),
            pending_fence_waits: HashMap::new(),
            off_cpu_threads: HashMap::new(),
            last_waker: HashMap::new(),
            etw_images: Vec::new(),
            current_frame: 0,
            last_present_qpc: None,
            last_submit_qpc: None,
            last_complete_qpc: None,
            total_target_submits: 0,
            tdr_or_removal_alert: None,
            alerts: Vec::new(),
            queues: BTreeMap::new(),
            raw_events: Vec::new(),
            ring_window_qpc: None,
            max_retained_events: 200_000,
            evicted_events: 0,
        }
    }

    pub fn with_ring(mut self, ring_secs: f64, max_events: usize) -> Self {
        self.ring_window_qpc = Some((ring_secs.max(0.5) * self.qpc_freq_hz as f64) as i64);
        self.max_retained_events = max_events.max(1024);
        self
    }

    fn matches_pid(&self, pid: u32) -> bool {
        match self.filter_pid {
            Some(want) => pid == want,
            None => true,
        }
    }

    fn schema_for(&mut self, rec: &EVENT_RECORD) -> Option<CachedSchema> {
        let key = SchemaKey {
            provider: rec.EventHeader.ProviderId.to_u128(),
            id: rec.EventHeader.EventDescriptor.Id,
            version: rec.EventHeader.EventDescriptor.Version,
            is_32bit: (rec.EventHeader.Flags & EVENT_HEADER_FLAG_32_BIT_HEADER as u16) != 0,
        };
        self.schemas
            .entry(key)
            .or_insert_with(|| CachedSchema::from_record(rec))
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

    fn ensure_queue(&mut self, ctx: u64) -> &mut QueueSummary {
        let meta = self.context_meta.get(&ctx).cloned();
        let label = self.queue_label(ctx);
        let q = self.queues.entry(ctx).or_insert_with(|| QueueSummary {
            queue_id: label,
            context: ctx,
            node_ordinal: meta.as_ref().and_then(|m| m.node_ordinal),
            engine_type: meta.as_ref().and_then(|m| m.engine_type),
            ..Default::default()
        });
        if q.node_ordinal.is_none() {
            q.node_ordinal = meta.as_ref().and_then(|m| m.node_ordinal);
        }
        q
    }

    fn push_event(&mut self, ev: RawTimelineEvent) {
        if !ev.raw_stack.is_empty() {
            self.stack_samples_captured += 1;
        }
        self.raw_events.push(ev);
        if let Some(win) = self.ring_window_qpc {
            if let Some(end) = self.end_qpc {
                let cutoff = end.saturating_sub(win);
                let slack = (win / 40).max(1);
                let over_cap = self.raw_events.len() > self.max_retained_events;
                let over_time = self
                    .raw_events
                    .first()
                    .is_some_and(|e| e.qpc < cutoff.saturating_sub(slack));
                if over_cap || over_time {
                    let mut drop_n = self.raw_events.partition_point(|e| e.qpc < cutoff);
                    let remaining = self.raw_events.len().saturating_sub(drop_n);
                    if remaining > self.max_retained_events {
                        drop_n += remaining - self.max_retained_events;
                    }
                    if drop_n > 0 {
                        self.raw_events.drain(..drop_n);
                        self.evicted_events += drop_n as u64;
                    }
                }
            }
        } else if self.raw_events.len() > self.max_retained_events {
            let drop_n = self.raw_events.len() - self.max_retained_events;
            self.raw_events.drain(..drop_n);
            self.evicted_events += drop_n as u64;
        }
    }

    /// Returns the oldest unresolved in-flight GPU packet age in seconds relative to `now_qpc`, if any.
    pub fn oldest_in_flight_secs(&self, now_qpc: i64) -> Option<f64> {
        let mut oldest: Option<i64> = None;
        for &(start, _, _) in self.in_flight_queue_pkts.values() {
            oldest = Some(oldest.map_or(start, |o| o.min(start)));
        }
        for &(start, _) in self.in_flight_dma_pkts.values() {
            oldest = Some(oldest.map_or(start, |o| o.min(start)));
        }
        oldest.map(|o| (now_qpc - o).max(0) as f64 / self.qpc_freq_hz as f64)
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

    fn on_dxgkrnl(&mut self, rec: &EVENT_RECORD, qpc: i64) {
        let id = rec.EventHeader.EventDescriptor.Id;
        if !matches!(
            id,
            27..=32
                | 47
                | 71
                | 73
                | 74
                | 92..=95
                | 100
                | 101
                | 168
                | 171..=173
                | 175..=180
                | 184
                | 230
                | 231
                | 242
                | 244
                | 245
                | 294..=297
                | 320
                | 321
                | 338
                | 374
                | 422..=424
                | 462
                | 469
                | 470
                | 540
                | 541
                | 547
                | 548
        ) {
            return;
        }
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
                if hq != 0 && hctx != 0 {
                    self.hwqueue_ctx.insert(hq, hctx);
                }
            }
            // QueuePacket Start (178, 244, 245) / Stop (180)
            178 | 244 | 245 => {
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
                if !self.matches_pid(pid) {
                    return;
                }
                self.last_submit_qpc = Some(qpc);
                self.total_target_submits += 1;
                if ctx != 0 {
                    self.ensure_queue(ctx).packets_started += 1;
                    self.in_flight_queue_pkts
                        .insert((ctx, seq), (qpc, hdr_tid, ptype));
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
            180 => {
                let hq = schema.get_u64(data, "hHwQueue").unwrap_or(0);
                let mut ctx = schema.get_u64(data, "hContext").unwrap_or(0);
                if ctx == 0 && hq != 0 {
                    ctx = self.hwqueue_ctx.get(&hq).copied().unwrap_or(hq);
                }
                let seq = schema
                    .get_any_u64(data, &["SubmitSequence", "ulSubmitSequence"])
                    .unwrap_or(0);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .unwrap_or(hdr_pid);
                if let Some((start_qpc, start_tid, ptype)) = self.in_flight_queue_pkts.remove(&(ctx, seq)) {
                    let dur_qpc = (qpc - start_qpc).max(0);
                    let dur_ms = dur_qpc as f64 * 1e3 / self.qpc_freq_hz as f64;
                    self.last_complete_qpc = Some(qpc);
                    let q = self.ensure_queue(ctx);
                    q.packets_completed += 1;
                    q.busy_ms += dur_ms;
                    if dur_ms > q.max_packet_ms {
                        q.max_packet_ms = dur_ms;
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
                self.last_submit_qpc = Some(qpc);
                self.total_target_submits += 1;
                if ctx != 0 {
                    self.in_flight_dma_pkts.insert((ctx, seq), (qpc, hdr_tid));
                }
            }
            176 => {
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
                if let Some((start_qpc, start_tid)) = self.in_flight_dma_pkts.remove(&(ctx, seq)) {
                    let dur_qpc = (qpc - start_qpc).max(0);
                    let dur_ms = dur_qpc as f64 * 1e3 / self.qpc_freq_hz as f64;
                    self.last_complete_qpc = Some(qpc);
                    let q = self.ensure_queue(ctx);
                    if q.packets_completed == 0 {
                        q.packets_started += 1;
                        q.packets_completed += 1;
                        q.busy_ms += dur_ms;
                        if dur_ms > q.max_packet_ms {
                            q.max_packet_ms = dur_ms;
                        }
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
            // WaitForSynchronizationObject* (100, 231, 295=FromGpu, 297=FromCpu)
            100 | 231 | 295 | 297 => {
                let ctx = schema
                    .get_any_u64(data, &["hContext", "BroadcastContext", "hHwQueue"])
                    .unwrap_or(0);
                let hdev = schema.get_u64(data, "hDevice").unwrap_or(0);
                let sync = schema
                    .get_any_u64(
                        data,
                        &[
                            "hSyncObject",
                            "ObjectHandle",
                            "ObjectHandleArray",
                            "pSyncObject",
                            "hFence",
                        ],
                    )
                    .unwrap_or(0);
                let val = schema
                    .get_any_u64(
                        data,
                        &["FenceValue", "MonitoredFenceValueArray", "Value", "NewFenceValue"],
                    )
                    .unwrap_or(0);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .or_else(|| self.device_pid.get(&hdev).copied().filter(|&p| p != 0))
                    .unwrap_or(hdr_pid);
                if !self.matches_pid(pid) {
                    return;
                }
                let qname = (ctx != 0).then(|| self.queue_label(ctx));
                if ctx != 0 {
                    self.ensure_queue(ctx).fence_waits += 1;
                }
                let stack = extract_inline_stack(rec);
                let is_cpu_wait = id == 297;
                if sync != 0 {
                    self.pending_fence_waits
                        .insert(sync, (qpc, pid, hdr_tid, qname.clone(), val, stack.clone()));
                }
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
                        if is_cpu_wait { "cpu_wait" } else { "gpu_wait" }
                    ),
                    raw_stack: stack,
                    waker_tid: None,
                    wait_kind: Some(if is_cpu_wait {
                        "dxg_fence_cpu_wait".into()
                    } else {
                        "dxg_fence_gpu_wait".into()
                    }),
                });
            }
            // SignalSynchronizationObject* (101, 230, 294=FromGpu, 296=FromCpu, 462=Immediate)
            101 | 230 | 294 | 296 | 462 => {
                let ctx = schema
                    .get_any_u64(data, &["hContext", "BroadcastContext", "hHwQueue"])
                    .unwrap_or(0);
                let hdev = schema.get_u64(data, "hDevice").unwrap_or(0);
                let sync = schema
                    .get_any_u64(
                        data,
                        &[
                            "hSyncObject",
                            "ObjectHandle",
                            "ObjectHandleArray",
                            "pSyncObject",
                            "hFence",
                        ],
                    )
                    .unwrap_or(0);
                let val = schema
                    .get_any_u64(
                        data,
                        &[
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
                if ctx != 0 && self.matches_pid(pid) {
                    self.ensure_queue(ctx).fence_signals += 1;
                }
                // Resolve matching pending wait to compute fence wait latency!
                if sync != 0
                    && let Some((w_qpc, w_pid, w_tid, w_queue, w_val, w_stack)) =
                        self.pending_fence_waits.remove(&sync)
                {
                    let dur_qpc = (qpc - w_qpc).max(0);
                    self.push_event(RawTimelineEvent {
                        qpc: w_qpc,
                        frame: self.current_frame,
                        pid: w_pid,
                        tid: w_tid,
                        kind: TimelineEventKind::FenceWait,
                        queue: w_queue,
                        duration_qpc: Some(dur_qpc),
                        detail: format!("resolved sync={sync:#x} val={w_val}->{val} by_tid={hdr_tid}"),
                        raw_stack: w_stack,
                        waker_tid: Some(hdr_tid),
                        wait_kind: Some("dxg_fence_wait_resolved".into()),
                    });
                }
                if !self.matches_pid(pid) {
                    return;
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
                        "{} sync={sync:#x} val={val}",
                        if id == 296 { "cpu_sig" } else { "gpu_sig" }
                    ),
                    raw_stack: stack,
                    waker_tid: None,
                    wait_kind: None,
                });
            }
            // Present (184) / PresentHistory (171) / Flip (168) / PresentQueuePacket (469)
            184 | 171 | 168 | 469 => {
                let ctx = schema.get_u64(data, "hContext").unwrap_or(0);
                let pid = self
                    .context_meta
                    .get(&ctx)
                    .map(|m| m.pid)
                    .filter(|&p| p != 0)
                    .unwrap_or(hdr_pid);
                if !self.matches_pid(pid) {
                    return;
                }
                let dur_qpc = self.last_present_qpc.map(|prev| (qpc - prev).max(0));
                self.last_present_qpc = Some(qpc);
                self.last_submit_qpc = Some(qpc);
                self.total_target_submits += 1;
                let status = schema
                    .get_any_u64(data, &["Status", "hResult", "Result"])
                    .unwrap_or(0) as u32;
                let frame = self.current_frame;
                if matches!(id, 184 | 171 | 469) {
                    self.current_frame += 1;
                }
                if is_device_removed_status(status) {
                    let msg = format!(
                        "Device removal status {status:#010x} observed on Present (id={id}, pid={pid}, frame={frame})"
                    );
                    self.record_removal(qpc, pid, hdr_tid, msg);
                }
                let stack = extract_inline_stack(rec);
                self.push_event(RawTimelineEvent {
                    qpc,
                    frame,
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
            // VidMm Paging & Residency events
            47 | 71 | 73 | 74 | 242 | 320 | 321 | 338 | 374 => {
                if !self.matches_pid(hdr_pid) && self.filter_pid.is_some() {
                    return;
                }
                let label = match id {
                    73 => "PageIn",
                    74 => "Evict",
                    71 => "AllocFault",
                    47 => "Migrate",
                    242 => "ResetBacking",
                    320 | 338 | 374 => "MakeResident",
                    321 => "VidMmEvict",
                    _ => "Paging",
                };
                let stack = extract_inline_stack(rec);
                let is_wait = matches!(id, 320 | 338 | 374);
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
            _ => {
                // Check for device-removal HRESULT/NTSTATUS in Fence/other events
                if let Some(status) = schema.get_any_u64(data, &["Status", "hResult", "Result"]) {
                    let s32 = status as u32;
                    if is_device_removed_status(s32) {
                        let msg = format!(
                            "DxgKrnl device-removal status {s32:#010x} (event id={id}, pid={hdr_pid})"
                        );
                        self.record_removal(qpc, hdr_pid, hdr_tid, msg);
                    }
                }
            }
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
        let (kind, detail) = if opcode == 98 || id == 98 {
            (TimelineEventKind::VirtualProtect, format!("VirtualAlloc id={id}"))
        } else {
            (
                TimelineEventKind::HardFault,
                format!("HardFault id={id} op={opcode}"),
            )
        };
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
            wait_kind: Some("hard_fault".into()),
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

    /// Finalizes the unified QPC timeline, symbolizing all captured stacks out-of-process.
    pub fn finish(
        mut self,
        source: String,
        symbolizer: &mut Symbolizer,
        warnings: Vec<String>,
        flight_trigger: Option<FlightTriggerInfo>,
    ) -> TimelineReport {
        for (base, size, path) in &self.etw_images {
            symbolizer.add_etw_image(*base, *size, path);
        }

        let start_qpc = self
            .raw_events
            .first()
            .map(|e| e.qpc)
            .or(self.start_qpc)
            .unwrap_or(0);
        let end_qpc = self
            .raw_events
            .last()
            .map(|e| e.qpc)
            .or(self.end_qpc)
            .unwrap_or(start_qpc);
        let dur_ms = (end_qpc - start_qpc).max(0) as f64 * 1e3 / self.qpc_freq_hz as f64;
        let qpc_to_ms = |ticks: i64| ticks as f64 * 1e3 / self.qpc_freq_hz as f64;

        // Account for unresolved in-flight packets at end of capture.
        for &(ctx, _) in self.in_flight_queue_pkts.keys() {
            if let Some(q) = self.queues.get_mut(&ctx) {
                q.in_flight_at_end += 1;
            }
        }

        let mut frames_map: BTreeMap<u64, FrameSummary> = BTreeMap::new();
        let mut frame_gpu_intervals: BTreeMap<u64, Vec<(i64, i64)>> = BTreeMap::new();
        let mut queue_gpu_intervals: HashMap<String, Vec<(i64, i64)>> = HashMap::new();
        let mut wait_acc: BTreeMap<(u32, String), WaitAccEntry> = BTreeMap::new();
        let mut stack_acc: HashMap<(String, String), u64> = HashMap::new();

        let mut events = Vec::with_capacity(self.raw_events.len());
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

            let fr = frames_map.entry(raw.frame).or_insert_with(|| FrameSummary {
                frame: raw.frame,
                start_qpc: raw.qpc,
                t_ms,
                ..Default::default()
            });
            match raw.kind {
                TimelineEventKind::GpuBusy => {
                    if let Some(dq) = raw.duration_qpc
                        && dq > 0
                    {
                        let s = raw.qpc.max(start_qpc);
                        let e = raw.qpc.saturating_add(dq).min(end_qpc);
                        if e > s {
                            frame_gpu_intervals.entry(raw.frame).or_default().push((s, e));
                            if let Some(ref qname) = raw.queue {
                                queue_gpu_intervals.entry(qname.clone()).or_default().push((s, e));
                            }
                        }
                    }
                }
                TimelineEventKind::FenceWait => {
                    fr.fence_wait_ms += duration_ms.unwrap_or(0.0);
                }
                TimelineEventKind::CSwitchWait => {
                    fr.os_wait_ms += duration_ms.unwrap_or(0.0);
                }
                TimelineEventKind::Paging => fr.paging_events += 1,
                TimelineEventKind::VirtualProtect => fr.virtual_protect_events += 1,
                TimelineEventKind::HardFault => fr.hard_faults += 1,
                TimelineEventKind::Present => {
                    fr.presents += 1;
                    if let Some(d) = duration_ms {
                        fr.duration_ms = d;
                    }
                }
                _ => {}
            }

            if let Some(ref wk) = raw.wait_kind {
                let d = duration_ms.unwrap_or(0.0);
                let e = wait_acc
                    .entry((raw.tid, wk.clone()))
                    .or_insert_with(|| (0, 0.0, 0.0, HashMap::new(), HashMap::new()));
                e.0 += 1;
                e.1 += d;
                if d > e.2 {
                    e.2 = d;
                }
                if let Some(w) = raw.waker_tid {
                    *e.3.entry(w).or_insert(0) += 1;
                }
                if let Some(ref s) = stack_line {
                    *e.4.entry(s.clone()).or_insert(0) += 1;
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

        // Compute merged union wall-clock busy time per queue (bounded 0..100%) and per frame.
        for (fnum, intervals) in frame_gpu_intervals {
            if let Some(fr) = frames_map.get_mut(&fnum) {
                fr.gpu_busy_ms = qpc_to_ms(union_interval_qpc(intervals));
            }
        }
        let mut queues: Vec<QueueSummary> = self
            .queues
            .into_values()
            .map(|mut q| {
                if let Some(intervals) = queue_gpu_intervals.remove(&q.queue_id) {
                    q.busy_ms = qpc_to_ms(union_interval_qpc(intervals));
                } else {
                    q.busy_ms = q.busy_ms.min(dur_ms);
                }
                q.busy_pct = if dur_ms > 0.0 {
                    ((q.busy_ms / dur_ms) * 100.0).min(100.0)
                } else {
                    0.0
                };
                q
            })
            .collect();
        queues.sort_by(|a, b| {
            b.busy_ms
                .total_cmp(&a.busy_ms)
                .then_with(|| b.packets_started.cmp(&a.packets_started))
        });

        let mut waits: Vec<WaitSummary> = wait_acc
            .into_iter()
            .map(|((tid, kind), (count, total_ms, max_ms, wakers, stacks))| {
                let top_waker = wakers
                    .into_iter()
                    .max_by_key(|&(_, n)| n)
                    .map(|(w, n)| format!("tid={w} ({n}x)"));
                let top_stack = stacks.into_iter().max_by_key(|(_, n)| *n).map(|(s, _)| s);
                WaitSummary {
                    tid,
                    kind,
                    count,
                    total_ms,
                    max_ms,
                    top_waker,
                    top_stack,
                }
            })
            .collect();
        waits.sort_by(|a, b| {
            b.total_ms
                .total_cmp(&a.total_ms)
                .then_with(|| b.count.cmp(&a.count))
        });

        let mut top_stacks: Vec<StackSummary> = stack_acc
            .into_iter()
            .map(|((kind, stack), count)| StackSummary { count, kind, stack })
            .collect();
        top_stacks.sort_by_key(|s| std::cmp::Reverse(s.count));
        top_stacks.truncate(50);

        let frames: Vec<FrameSummary> = frames_map.into_values().collect();
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
            thread_suspends: THREAD_SUSPENDS,
            qpc_freq_hz: self.qpc_freq_hz,
            start_qpc,
            end_qpc,
            duration_ms: dur_ms,
            total_raw_events: self.total_raw_events,
            target_events: events.len(),
            stack_samples_captured: self.stack_samples_captured,
            wait_attribution_mode,
            providers_active,
            warnings,
            alerts: self.alerts,
            flight_trigger,
            queues,
            waits,
            frames,
            top_stacks,
            events,
        }
    }
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
    logfile.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_EVENT_RECORD;
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
    if logfile.LogfileHeader.PerfFreq > 0 {
        builder.qpc_freq_hz = logfile.LogfileHeader.PerfFreq as u64;
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

    std::thread::sleep(std::time::Duration::from_millis(
        (opts.duration_secs.max(0.1) * 1000.0) as u64,
    ));

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
    std::thread::sleep(std::time::Duration::from_millis(100));
    stop_session_handle(handle, &name);

    let mut rep = parse_timeline_etl(&etl_path, Some(opts.pid), &mut symbolizer)?;
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
        (*p).BufferSize = 1024;
        (*p).MinimumBuffers = 16;
        (*p).MaximumBuffers = 128;
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
    let _ = writeln!(
        out,
        "HEADER source={} pid={} dur_ms={:.2} qpc_hz={} raw_events={} target_events={} stacks={} wait_mode={} providers={} thread_suspends={}",
        rep.source,
        pid_s,
        rep.duration_ms,
        rep.qpc_freq_hz,
        rep.total_raw_events,
        rep.target_events,
        rep.stack_samples_captured,
        rep.wait_attribution_mode,
        rep.providers_active.join(","),
        rep.thread_suspends,
    );
    if let Some(ref trig) = rep.flight_trigger {
        let _ = writeln!(
            out,
            "TRIGGER reason={:?} trigger_qpc={} ring_secs={:.1} observed={} evicted={}",
            trig.reason,
            trig.trigger_qpc,
            trig.ring_secs,
            trig.total_observed_events,
            trig.evicted_ring_events,
        );
    }
    for w in &rep.warnings {
        let _ = writeln!(out, "WARN {w}");
    }
    for a in &rep.alerts {
        let _ = writeln!(out, "ALERT {a}");
    }

    if !rep.queues.is_empty() {
        let _ = writeln!(out, "\n== GPU Hardware Queues (QPC busy / signals / waits) ==");
        for q in rep.queues.iter().take(opts.top) {
            let _ = writeln!(
                out,
                "QUEUE {:<20} pkts={}/{} in_flight={} busy_ms={:>8.2} ({:>5.1}%) max_pkt_ms={:>7.2} sig={} wait={}",
                q.queue_id,
                q.packets_completed,
                q.packets_started,
                q.in_flight_at_end,
                q.busy_ms,
                q.busy_pct,
                q.max_packet_ms,
                q.fence_signals,
                q.fence_waits,
            );
        }
    }

    if !rep.waits.is_empty() {
        let _ = writeln!(out, "\n== Wait Attribution (per thread) ==");
        for w in rep.waits.iter().take(opts.top) {
            let waker = w.top_waker.as_deref().unwrap_or("-");
            let stack = w.top_stack.as_deref().unwrap_or("-");
            let _ = writeln!(
                out,
                "WAIT tid={:<6} kind={:<24} n={:<5} total_ms={:>8.2} max_ms={:>7.2} waker={:<18} stack={}",
                w.tid, w.kind, w.count, w.total_ms, w.max_ms, waker, stack
            );
        }
    }

    if !rep.top_stacks.is_empty() {
        let _ = writeln!(out, "\n== Top Symbolized CPU Stacks ==");
        for s in rep.top_stacks.iter().take(opts.top) {
            let _ = writeln!(out, "STACK n={:<5} kind={:<12} {}", s.count, s.kind, s.stack);
        }
    }

    if !rep.frames.is_empty() {
        let _ = writeln!(out, "\n== Per-Present Frames (QPC) ==");
        for f in rep.frames.iter().take(opts.top.max(30)) {
            let _ = writeln!(
                out,
                "FRAME f={:<4} t_ms={:>9.2} dur_ms={:>7.2} gpu_busy_ms={:>7.2} fence_wait_ms={:>7.2} os_wait_ms={:>7.2} paging={} vprot={} hfault={} presents={}",
                f.frame,
                f.t_ms,
                f.duration_ms,
                f.gpu_busy_ms,
                f.fence_wait_ms,
                f.os_wait_ms,
                f.paging_events,
                f.virtual_protect_events,
                f.hard_faults,
                f.presents,
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
        let _ = writeln!(
            out,
            "\n== Unified QPC Timeline Events (showing {} of {} matching) ==",
            opts.limit.min(filtered.len()),
            filtered.len(),
        );
        for e in filtered.iter().take(opts.limit) {
            let dur = e
                .duration_ms
                .map(|d| format!(" dur={d:.3}ms"))
                .unwrap_or_default();
            let q = e.queue.as_deref().map(|q| format!(" {q}")).unwrap_or_default();
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
                "EV t=+{:>9.3}ms qpc={} f={:<4} tid={:<6} [{}]{}{} {}{}",
                e.t_ms,
                e.qpc,
                e.frame,
                e.tid,
                e.kind.tag(),
                q,
                dur,
                e.detail,
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
            raw_stack: vec![0x7FF6_1000_1234],
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
            detail: "resolved sync=0xabc val=1->1 by_tid=1002".into(),
            raw_stack: vec![0x7FF6_1000_1234],
            waker_tid: Some(1002),
            wait_kind: Some("dxg_fence_wait_resolved".into()),
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
        assert_eq!(rep.waits.len(), 1);
        assert_eq!(rep.waits[0].top_waker.as_deref(), Some("tid=1002 (1x)"));
        assert_eq!(rep.events[0].stack, vec!["xcl.exe+0x1234"]);

        let text = format_timeline_text(&rep, &TimelineFormatOptions::default());
        assert!(text.contains("HEADER source=unit.etl pid=42"));
        assert!(text.contains("QUEUE ctx=0x1000/n0"));
        assert!(text.contains("WAIT tid=1001"));
        assert!(text.contains("FRAME f=0"));
        assert!(text.contains("[GPU_BUSY]"));
        assert!(text.contains("xcl.exe+0x1234"));
    }

    #[test]
    fn ring_eviction_keeps_only_last_window_events() {
        let freq = 10_000_000u64;
        let mut b = TimelineBuilder::new(Some(7), freq).with_ring(10.0, 10_000);
        // Push 1500 events spanning 20 seconds (0..20s).
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
            (b.raw_events.last().unwrap().qpc - b.raw_events.first().unwrap().qpc) as f64 / freq as f64;
        assert!(
            span_secs <= 10.5,
            "retained ring span should be ~10s, got {span_secs:.2}s"
        );
    }
}
