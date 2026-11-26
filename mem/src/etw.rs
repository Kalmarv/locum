//! `Microsoft-Windows-DxgKrnl` + kernel `SystemMemory` ETW live capture and fast TDH-cached `.etl` parser.
//!
//! Works without administrator elevation when the current account belongs to `Performance Log Users`.
//! Live capture uses `StartTraceW` + `EnableTraceEx2` (including `EVENT_CONTROL_CODE_CAPTURE_STATE`
//! rundown so already-live VidMm segments, devices, allocations, and GPU VA mappings are recorded).

use std::collections::{BTreeMap, HashMap};
use std::ffi::c_void;
use std::path::{Path, PathBuf};

use serde::Serialize;
use windows::Win32::Foundation::{
    ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, WIN32_ERROR,
};
use windows::Win32::System::Diagnostics::Etw::{
    CONTROLTRACE_HANDLE, CloseTrace, ControlTraceW, ENABLE_TRACE_PARAMETERS,
    EVENT_CONTROL_CODE_CAPTURE_STATE, EVENT_CONTROL_CODE_ENABLE_PROVIDER, EVENT_HEADER_FLAG_32_BIT_HEADER,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_FILE_MODE_SEQUENTIAL, EVENT_TRACE_LOGFILEW,
    EVENT_TRACE_PROPERTIES, EnableTraceEx2, OpenTraceW, PROCESS_TRACE_MODE_EVENT_RECORD, PROCESSTRACE_HANDLE,
    ProcessTrace, StartTraceW, TRACE_EVENT_INFO, TdhGetEventInformation, WNODE_FLAG_TRACED_GUID,
};
use windows::core::{GUID, HSTRING, PCWSTR, PWSTR};

/// `Microsoft-Windows-DxgKrnl` (`{802EC45A-1E99-4B83-9920-87C98277BA9D}`).
pub const DXGKRNL_PROVIDER: GUID = GUID::from_u128(0x802ec45a_1e99_4b83_9920_87c98277ba9d);

/// `SystemMemoryProvider` (`{82958CA9-9C6D-490D-8364-592E28955110}`) — modern Windows 10/11 kernel memory provider.
pub const SYSTEM_MEMORY_PROVIDER: GUID = GUID::from_u128(0x82958ca9_9c6d_490d_8364_592e28955110);

/// Classic `PageFault` MOF provider (`{3D6FA8D3-FE05-11D0-9DDA-00C04FD7BA7C}`) from WPR/xperf kernel traces.
pub const CLASSIC_PAGEFAULT_GUID: GUID = GUID::from_u128(0x3d6fa8d3_fe05_11d0_9dda_00c04fd7ba7c);

/// Default DxgKrnl keyword mask:
/// `Base (0x1) | References (0x4) | Resource (0x40) | Memory (0x80) | GPUVA (0x100000)`.
pub const DXGKRNL_KEYWORDS_DEFAULT: u64 = 0x0010_00C5;

/// `SYSTEM_MEMORY_KW_HARD_FAULTS` (`0x2000`) on `SystemMemoryProvider`.
pub const SYSTEM_MEMORY_KW_HARD_FAULTS: u64 = 0x2000;

const TRACE_LEVEL_INFORMATION: u8 = 4;

// TDH InType constants (from tdh.h).
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

/// Segment classification from `ReportSegment`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentKind {
    LocalVram,
    LocalCpuVisibleVram,
    NonLocalHost,
    Unknown,
}

impl SegmentKind {
    pub fn is_local(self) -> bool {
        matches!(self, Self::LocalVram | Self::LocalCpuVisibleVram)
    }

    pub fn is_non_local(self) -> bool {
        matches!(self, Self::NonLocalHost)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::LocalVram => "local_vram",
            Self::LocalCpuVisibleVram => "local_bar_vram",
            Self::NonLocalHost => "non_local_host",
            Self::Unknown => "unknown",
        }
    }
}

/// Video-memory segment metadata decoded from DxgKrnl `ReportSegment` (id=78).
#[derive(Clone, Debug, Serialize)]
pub struct SegmentInfo {
    pub adapter: u64,
    pub segment_id: u32,
    pub size: u64,
    pub commit_limit: u64,
    pub flags: u32,
    pub memory_segment_group: u8,
    pub aperture: bool,
    pub cpu_visible: bool,
    pub populated_from_system_memory: bool,
    pub kind: SegmentKind,
    /// Last/min/max bytes resident reported by `TotalBytesResidentInSegment` (adapter-wide).
    pub resident_last: u64,
    pub resident_min: u64,
    pub resident_max: u64,
    pub resident_samples: u64,
}

/// One VidMm global allocation tracked across DxgKrnl events.
#[derive(Clone, Debug, Serialize)]
pub struct VidMmAllocation {
    pub global_handle: u64,
    pub adapter: u64,
    pub pid: u32,
    pub device: u64,
    pub size: u64,
    pub flags: u32,
    /// Raw `PreferredSegment` field from `AdapterAllocation` (WDDM `DXGK_SEGMENTPREFERENCE`).
    pub preferred_segment_raw: u32,
    /// Primary preferred segment ID (`preferred_segment_raw & 0x1F`, 0 if none).
    pub preferred_segment_id: u32,
    /// Read/Write supported segment bitmasks (bit `k` corresponds to 1-based segment `k+1`).
    pub read_segment_set: u32,
    pub write_segment_set: u32,
    /// Actual current segment ID where VidMm committed/paged-in this allocation (0 = unknown/sysmem).
    pub current_segment_id: Option<u32>,
    /// True if VidMm placed this allocation in NonLocal/Host when it preferred Local VRAM,
    /// or was explicitly demoted / evicted.
    pub demoted: bool,
    pub evicted: bool,
    pub page_table_or_dir: bool,
    pub backing_store_pinned: bool,
    pub cpu_va: Option<u64>,
    pub gpu_va_start: Option<u64>,
    pub gpu_va_end: Option<u64>,
    pub page_in_count: u64,
    pub evict_count: u64,
    pub fault_count: u64,
    pub alive: bool,
}

/// Per-process VidMm commitment/usage/demotion reported directly by VidMm ETW events (366/367/370/371).
#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcessVidMmCounters {
    pub pid: u32,
    pub local_usage: Option<u64>,
    pub non_local_usage: Option<u64>,
    pub local_commitment: Option<u64>,
    pub non_local_commitment: Option<u64>,
    pub demoted_commitment: Option<u64>,
    pub local_budget: Option<u64>,
    pub non_local_budget: Option<u64>,
    pub max_local_usage: u64,
    pub max_non_local_usage: u64,
    pub max_demoted_commitment: u64,
}

/// GPU virtual-address range mapping (`GpuVirtualAddressRangeMapping` 333/334/435).
#[derive(Clone, Debug, Serialize)]
pub struct GpuVaMapping {
    pub start: u64,
    pub end: u64,
    pub owner_handle: u64,
    pub mapped_size: u64,
    pub segment_id: Option<u32>,
    pub placement: String,
}

/// Hard-fault summary from kernel ETW (`SystemMemoryProvider` / classic `PageFault`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct HardFaultSummary {
    pub total_count: u64,
    pub total_bytes: u64,
    pub by_pid: BTreeMap<u32, u64>,
    /// Sampled/collected faulting virtual addresses for the filtered PID (capped to avoid unbounded RAM).
    pub target_fault_vas: Vec<u64>,
}

/// `UpdateTileMappings` (`UpdateGpuVirtualAddress` id=512) summary.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TileMappingSummary {
    pub call_count: u64,
    pub total_operations: u64,
    pub by_pid: BTreeMap<u32, u64>,
    pub gpuva_map_events: u64,
    pub gpuva_unmap_events: u64,
}

/// VidMm paging / residency activity summary.
#[derive(Clone, Debug, Default, Serialize)]
pub struct PagingActivity {
    pub allocation_faults: u64,
    pub page_in_allocations: u64,
    pub evict_allocations: u64,
    pub migrate_allocations: u64,
    pub make_resident_calls: u64,
    pub vidmm_evict_calls: u64,
    pub reset_backing_store_calls: u64,
    pub reset_backing_store_pages: u64,
    pub paging_queue_packets: u64,
    pub reserve_resource_calls: u64,
    pub reserve_resource_by_segment: BTreeMap<u32, u64>,
}

/// Full parsed ETW report for a trace (optionally filtered to `filter_pid`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct EtwReport {
    pub etl_path: String,
    pub filter_pid: Option<u32>,
    pub total_events: u64,
    pub dxgkrnl_events: u64,
    pub system_memory_events: u64,
    pub duration_secs: f64,
    pub warnings: Vec<String>,
    pub segments: Vec<SegmentInfo>,
    pub process_counters: BTreeMap<u32, ProcessVidMmCounters>,
    pub allocations: Vec<VidMmAllocation>,
    pub gpu_va_mappings: Vec<GpuVaMapping>,
    pub tile_mappings: TileMappingSummary,
    pub paging: PagingActivity,
    pub hard_faults: HardFaultSummary,
    /// Aggregate bytes of live tracked allocations (for `filter_pid` if set, else all PIDs).
    pub tracked_local_vram_bytes: u64,
    pub tracked_local_bar_bytes: u64,
    pub tracked_non_local_host_bytes: u64,
    pub tracked_unknown_segment_bytes: u64,
    pub tracked_demoted_bytes: u64,
    pub tracked_evicted_bytes: u64,
    pub tracked_total_bytes: u64,
}

impl EtwReport {
    /// Looks up the VidMm placement string for a GPU VA (from `GpuVirtualAddressRangeMapping`).
    pub fn placement_of_gpu_va(&self, va: u64) -> Option<&str> {
        let hi = self.gpu_va_mappings.partition_point(|m| m.start <= va);
        for m in self.gpu_va_mappings[..hi].iter().rev() {
            if va < m.end {
                return Some(m.placement.as_str());
            }
        }
        None
    }

    pub fn segment_kind(&self, adapter: u64, seg_id: u32) -> SegmentKind {
        if seg_id == 0 {
            return SegmentKind::NonLocalHost;
        }
        if let Some(s) = self
            .segments
            .iter()
            .find(|s| (adapter == 0 || s.adapter == adapter) && s.segment_id == seg_id)
        {
            return s.kind;
        }
        if let Some(s) = self.segments.iter().find(|s| s.segment_id == seg_id) {
            return s.kind;
        }
        SegmentKind::Unknown
    }
}

/// Live ETW session guard. Automatically stops the session on `Drop`.
pub struct TraceSession {
    name: String,
    etl_path: PathBuf,
    handle: CONTROLTRACE_HANDLE,
    active: bool,
    pub warnings: Vec<String>,
}

/// Options for starting a live ETW trace session.
#[derive(Clone, Debug)]
pub struct LiveTraceOptions {
    pub etl_path: PathBuf,
    pub dxgkrnl_keywords: u64,
    pub enable_hard_faults: bool,
    pub max_file_mb: u32,
}

impl LiveTraceOptions {
    pub fn new(etl_path: PathBuf) -> Self {
        Self {
            etl_path,
            dxgkrnl_keywords: DXGKRNL_KEYWORDS_DEFAULT,
            enable_hard_faults: true,
            max_file_mb: 2048,
        }
    }
}

impl TraceSession {
    /// Starts a live ETW session recording `Microsoft-Windows-DxgKrnl` (and best-effort kernel
    /// `HardFaults`) to `opts.etl_path`, immediately triggering `EVENT_CONTROL_CODE_CAPTURE_STATE`
    /// rundown so existing segments, devices, and allocations are captured.
    pub fn start(opts: LiveTraceOptions) -> Result<Self, String> {
        let name = format!("locum_mem_{}_{}", std::process::id(), now_nonce());
        let mut warnings = Vec::new();

        if let Some(parent) = opts.etl_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create ETL directory {}: {e}", parent.display()))?;
        }
        let _ = std::fs::remove_file(&opts.etl_path);

        let mut props_buf = build_trace_properties(&name, Some(&opts.etl_path), opts.max_file_mb);
        let props = props_buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>();
        let mut handle = CONTROLTRACE_HANDLE::default();
        let name_w = HSTRING::from(&name);

        let err = unsafe { StartTraceW(&mut handle, &name_w, props) };
        if err == ERROR_ALREADY_EXISTS {
            let mut stop_buf = build_trace_properties(&name, None, 0);
            unsafe {
                let _ = ControlTraceW(
                    CONTROLTRACE_HANDLE::default(),
                    &name_w,
                    stop_buf.as_mut_ptr().cast(),
                    EVENT_TRACE_CONTROL_STOP,
                );
            }
            let err2 = unsafe { StartTraceW(&mut handle, &name_w, props) };
            if err2 != ERROR_SUCCESS {
                return Err(format_start_trace_err(err2));
            }
        } else if err != ERROR_SUCCESS {
            return Err(format_start_trace_err(err));
        }

        let mut sess = Self {
            name,
            etl_path: opts.etl_path,
            handle,
            active: true,
            warnings: Vec::new(),
        };

        let enable_params = ENABLE_TRACE_PARAMETERS {
            Version: 2,
            ..Default::default()
        };

        let e1 = unsafe {
            EnableTraceEx2(
                sess.handle,
                &DXGKRNL_PROVIDER,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                TRACE_LEVEL_INFORMATION,
                opts.dxgkrnl_keywords,
                0,
                0,
                Some(&enable_params),
            )
        };
        if e1 != ERROR_SUCCESS {
            return Err(format!(
                "EnableTraceEx2(Microsoft-Windows-DxgKrnl) failed with Win32 error {}",
                e1.0
            ));
        }

        // Request rundown (DC_Start) of currently live adapters, segments, devices, and allocations.
        let e_cap = unsafe {
            EnableTraceEx2(
                sess.handle,
                &DXGKRNL_PROVIDER,
                EVENT_CONTROL_CODE_CAPTURE_STATE.0,
                TRACE_LEVEL_INFORMATION,
                opts.dxgkrnl_keywords,
                0,
                0,
                Some(&enable_params),
            )
        };
        if e_cap != ERROR_SUCCESS {
            warnings.push(format!(
                "DxgKrnl CAPTURE_STATE rundown returned Win32 error {} (live allocations before trace start may be incomplete)",
                e_cap.0
            ));
        }

        if opts.enable_hard_faults {
            let e_hf = unsafe {
                EnableTraceEx2(
                    sess.handle,
                    &SYSTEM_MEMORY_PROVIDER,
                    EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                    TRACE_LEVEL_INFORMATION,
                    SYSTEM_MEMORY_KW_HARD_FAULTS,
                    0,
                    0,
                    Some(&enable_params),
                )
            };
            if e_hf != ERROR_SUCCESS {
                warnings.push(format!(
                    "SystemMemoryProvider HardFaults enable returned Win32 error {} (falling back to GetProcessMemoryInfo PageFaultCount)",
                    e_hf.0
                ));
            }
        }

        sess.warnings = warnings;
        Ok(sess)
    }

    /// Triggers an additional `CAPTURE_STATE` rundown (useful right before stopping to record
    /// the final committed segment of every live VidMm allocation!).
    pub fn capture_state(&mut self, keywords: u64) {
        if !self.active {
            return;
        }
        let enable_params = ENABLE_TRACE_PARAMETERS {
            Version: 2,
            ..Default::default()
        };
        let err = unsafe {
            EnableTraceEx2(
                self.handle,
                &DXGKRNL_PROVIDER,
                EVENT_CONTROL_CODE_CAPTURE_STATE.0,
                TRACE_LEVEL_INFORMATION,
                keywords,
                0,
                0,
                Some(&enable_params),
            )
        };
        if err != ERROR_SUCCESS {
            self.warnings.push(format!(
                "DxgKrnl final CAPTURE_STATE returned Win32 error {}",
                err.0
            ));
        }
    }

    /// Stops and flushes the ETW session, returning the written `.etl` path.
    pub fn stop(mut self) -> Result<PathBuf, String> {
        self.stop_inner()?;
        Ok(self.etl_path.clone())
    }

    fn stop_inner(&mut self) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        let mut stop_buf = build_trace_properties(&self.name, None, 0);
        let name_w = HSTRING::from(&self.name);
        let err = unsafe {
            ControlTraceW(
                self.handle,
                &name_w,
                stop_buf.as_mut_ptr().cast::<EVENT_TRACE_PROPERTIES>(),
                EVENT_TRACE_CONTROL_STOP,
            )
        };
        if err != ERROR_SUCCESS {
            return Err(format!("ControlTraceW(STOP) failed with Win32 error {}", err.0));
        }
        Ok(())
    }
}

impl Drop for TraceSession {
    fn drop(&mut self) {
        let _ = self.stop_inner();
    }
}

fn format_start_trace_err(err: WIN32_ERROR) -> String {
    if err.0 == 5 {
        "StartTraceW failed with ERROR_ACCESS_DENIED (5): add the account to Performance Log Users or run elevated".into()
    } else {
        format!("StartTraceW failed with Win32 error {}", err.0)
    }
}

fn now_nonce() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64 & 0xFFFF_FFFF)
        .unwrap_or(0)
}

fn build_trace_properties(name: &str, etl_path: Option<&Path>, max_file_mb: u32) -> Vec<u8> {
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
    // Reserve generous room for BOTH logger name and log file path because ControlTraceW(STOP)
    // writes both strings back into the caller's EVENT_TRACE_PROPERTIES buffer (otherwise ERROR_MORE_DATA=234).
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
        (*p).Wnode.ClientContext = 1; // QPC clock
        (*p).BufferSize = 1024; // 1 MiB buffers
        (*p).MinimumBuffers = 16;
        (*p).MaximumBuffers = 128;
        (*p).MaximumFileSize = max_file_mb;
        (*p).LogFileMode = EVENT_TRACE_FILE_MODE_SEQUENTIAL;
        (*p).FlushTimer = 1;
        (*p).LoggerNameOffset = logger_off as u32;
        (*p).LogFileNameOffset = file_off as u32;
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

// ---------------------------------------------------------------------------
// Fast TDH-schema-cached .etl decoder
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct FieldSpec {
    offset: usize,
    /// Size in bytes (resolved using 32-bit vs 64-bit event pointer width).
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
            // Stop linear fixed-offset walk on struct/array/variable-length properties.
            if (pi.Flags.0 & 0x1) != 0 {
                // PropertyStruct
                break;
            }
            let count = unsafe { pi.Anonymous2.count };
            if (pi.Flags.0 & 0x4) != 0 || count != 1 {
                // PropertyParamCount or array
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
            fields.insert(name, FieldSpec { offset, size: fsize });
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
    fn get_bool(&self, data: &[u8], name: &str) -> Option<bool> {
        self.get_u64(data, name).map(|v| v != 0)
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

#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct SchemaKey {
    provider: u128,
    id: u16,
    version: u8,
    is_32bit: bool,
}

struct ParserState {
    filter_pid: Option<u32>,
    schemas: HashMap<SchemaKey, Option<CachedSchema>>,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    total_events: u64,
    dxgkrnl_events: u64,
    system_memory_events: u64,
    /// `(adapter, segment_id)` -> `SegmentInfo`
    segments: BTreeMap<(u64, u32), SegmentInfo>,
    /// `hDevice` -> `pid`
    device_pid: HashMap<u64, u32>,
    /// `DxgProcess` -> `pid`
    dxg_process_pid: HashMap<u64, u32>,
    /// `hVidMmAlloc` -> `hVidMmGlobalAlloc`
    alloc_to_global: HashMap<u64, u64>,
    /// `hVidMmGlobalAlloc` -> `VidMmAllocation`
    allocs: HashMap<u64, VidMmAllocation>,
    /// `pVaAllocator` -> `pid`
    va_alloc_pid: HashMap<u64, u32>,
    /// `(start_va, end_va)` -> `(owner_handle, mapped_size, pid_hint)`
    gpu_va_live: BTreeMap<(u64, u64), (u64, u64, u32)>,
    process_counters: BTreeMap<u32, ProcessVidMmCounters>,
    tile_mappings: TileMappingSummary,
    paging: PagingActivity,
    hard_faults: HardFaultSummary,
}

impl ParserState {
    fn new(filter_pid: Option<u32>) -> Self {
        Self {
            filter_pid,
            schemas: HashMap::new(),
            first_ts: None,
            last_ts: None,
            total_events: 0,
            dxgkrnl_events: 0,
            system_memory_events: 0,
            segments: BTreeMap::new(),
            device_pid: HashMap::new(),
            dxg_process_pid: HashMap::new(),
            alloc_to_global: HashMap::new(),
            allocs: HashMap::new(),
            va_alloc_pid: HashMap::new(),
            gpu_va_live: BTreeMap::new(),
            process_counters: BTreeMap::new(),
            tile_mappings: TileMappingSummary::default(),
            paging: PagingActivity::default(),
            hard_faults: HardFaultSummary::default(),
        }
    }

    fn schema_for(&mut self, rec: &EVENT_RECORD) -> Option<&CachedSchema> {
        let key = SchemaKey {
            provider: rec.EventHeader.ProviderId.to_u128(),
            id: rec.EventHeader.EventDescriptor.Id,
            version: rec.EventHeader.EventDescriptor.Version,
            is_32bit: (rec.EventHeader.Flags & EVENT_HEADER_FLAG_32_BIT_HEADER as u16) != 0,
        };
        self.schemas
            .entry(key)
            .or_insert_with(|| CachedSchema::from_record(rec))
            .as_ref()
    }

    fn matches_pid(&self, pid: u32) -> bool {
        match self.filter_pid {
            Some(want) => pid == want,
            None => true,
        }
    }

    fn on_event(&mut self, rec: &EVENT_RECORD) {
        self.total_events += 1;
        let ts = rec.EventHeader.TimeStamp;
        if ts > 0 {
            if self.first_ts.is_none() {
                self.first_ts = Some(ts);
            }
            self.last_ts = Some(ts);
        }

        let prov = rec.EventHeader.ProviderId;
        if prov == DXGKRNL_PROVIDER {
            self.dxgkrnl_events += 1;
            self.on_dxgkrnl(rec);
        } else if prov == SYSTEM_MEMORY_PROVIDER {
            self.system_memory_events += 1;
            self.on_system_memory(rec);
        } else if prov == CLASSIC_PAGEFAULT_GUID {
            self.system_memory_events += 1;
            self.on_classic_pagefault(rec);
        }
    }

    fn on_dxgkrnl(&mut self, rec: &EVENT_RECORD) {
        let id = rec.EventHeader.EventDescriptor.Id;
        // Fast reject of high-volume scheduler/vsync events we do not need for memory accounting.
        if !matches!(
            id,
            27 | 28
                | 29
                | 33
                | 34
                | 35
                | 36
                | 37
                | 38
                | 39
                | 40
                | 47
                | 48
                | 49
                | 67
                | 71
                | 73
                | 74
                | 78
                | 80
                | 225
                | 226
                | 227
                | 242
                | 274
                | 320
                | 321
                | 322
                | 323
                | 333
                | 334
                | 335
                | 336
                | 338
                | 366
                | 367
                | 370
                | 371
                | 374
                | 435
                | 471
                | 472
                | 491
                | 492
                | 512
        ) {
            return;
        }

        let data = if rec.UserData.is_null() || rec.UserDataLength == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(rec.UserData.cast::<u8>(), rec.UserDataLength as usize) }
        };
        let hdr_pid = rec.EventHeader.ProcessId;
        let Some(schema) = self.schema_for(rec).cloned() else {
            return;
        };

        match id {
            // Process Start / DC_Start
            471 | 472 | 491 | 492 => {
                if let (Some(dxg_proc), Some(pid)) = (
                    schema.get_u64(data, "DxgProcess"),
                    schema.get_u64(data, "ProcessId"),
                ) && dxg_proc != 0
                    && pid != 0
                {
                    self.dxg_process_pid.insert(dxg_proc, pid as u32);
                }
            }
            // Device Start / Stop / DC_Start
            27..=29 => {
                if let (Some(hdev), Some(pid)) = (
                    schema.get_u64(data, "hDevice"),
                    schema.get_any_u64(data, &["hProcessId", "ProcessId"]),
                ) {
                    let pid = if pid != 0 { pid as u32 } else { hdr_pid };
                    if hdev != 0 && pid != 0 {
                        self.device_pid.insert(hdev, pid);
                    }
                }
            }
            // ReportSegment (defines segment IDs and Local vs NonLocal / CpuVisible / Aperture)
            78 => {
                let seg_id = schema.get_u64(data, "ulSegmentId").unwrap_or(0) as u32;
                let adapter = schema.get_u64(data, "pDxgAdapter").unwrap_or(0);
                let size = schema.get_u64(data, "Size").unwrap_or(0);
                let commit_limit = schema.get_u64(data, "CommitLimit").unwrap_or(0);
                let flags = schema.get_u64(data, "Flags").unwrap_or(0) as u32;
                let group = schema.get_u64(data, "MemorySegmentGroup").unwrap_or(0) as u8;
                let kind = classify_segment(flags, group);
                let entry = self
                    .segments
                    .entry((adapter, seg_id))
                    .or_insert_with(|| SegmentInfo {
                        adapter,
                        segment_id: seg_id,
                        size,
                        commit_limit,
                        flags,
                        memory_segment_group: group,
                        aperture: (flags & 0x1) != 0,
                        cpu_visible: (flags & 0x4) != 0,
                        populated_from_system_memory: (flags & 0x40) != 0,
                        kind,
                        resident_last: 0,
                        resident_min: u64::MAX,
                        resident_max: 0,
                        resident_samples: 0,
                    });
                entry.size = size;
                entry.commit_limit = commit_limit;
                entry.flags = flags;
                entry.memory_segment_group = group;
                entry.aperture = (flags & 0x1) != 0;
                entry.cpu_visible = (flags & 0x4) != 0;
                entry.populated_from_system_memory = (flags & 0x40) != 0;
                entry.kind = kind;
            }
            // TotalBytesResidentInSegment
            274 => {
                let adapter = schema.get_u64(data, "DxgAdapter").unwrap_or(0);
                let seg_id = schema.get_u64(data, "SegmentId").unwrap_or(0) as u32;
                let bytes = schema.get_u64(data, "TotalBytesResident").unwrap_or(0);
                let entry = self
                    .segments
                    .entry((adapter, seg_id))
                    .or_insert_with(|| SegmentInfo {
                        adapter,
                        segment_id: seg_id,
                        size: 0,
                        commit_limit: 0,
                        flags: 0,
                        memory_segment_group: 0,
                        aperture: false,
                        cpu_visible: false,
                        populated_from_system_memory: false,
                        kind: if seg_id == 0 {
                            SegmentKind::NonLocalHost
                        } else {
                            SegmentKind::Unknown
                        },
                        resident_last: 0,
                        resident_min: u64::MAX,
                        resident_max: 0,
                        resident_samples: 0,
                    });
                entry.resident_last = bytes;
                entry.resident_min = entry.resident_min.min(bytes);
                entry.resident_max = entry.resident_max.max(bytes);
                entry.resident_samples += 1;
            }
            // AdapterAllocation Start (33) / Stop (34) / DC_Start (35)
            33..=35 => {
                let gh = schema.get_u64(data, "hVidMmGlobalAlloc").unwrap_or(0);
                if gh == 0 {
                    return;
                }
                if id == 34 {
                    if let Some(a) = self.allocs.get_mut(&gh) {
                        a.alive = false;
                    }
                    return;
                }
                let hdev = schema.get_u64(data, "hDevice").unwrap_or(0);
                let mut pid = schema.get_u64(data, "hProcessId").unwrap_or(0) as u32;
                if pid == 0 && hdev != 0 {
                    pid = self.device_pid.get(&hdev).copied().unwrap_or(0);
                }
                if pid == 0 && hdr_pid != 4 {
                    pid = hdr_pid;
                }
                let adapter = schema.get_u64(data, "pDxgAdapter").unwrap_or(0);
                let size = schema.get_u64(data, "allocSize").unwrap_or(0);
                let flags = schema.get_u64(data, "Flags").unwrap_or(0) as u32;
                let pref_raw = schema.get_u64(data, "PreferredSegment").unwrap_or(0) as u32;
                let pref_id = pref_raw & 0x1F;
                let read_set = schema.get_u64(data, "dwReadSegment").unwrap_or(0) as u32;
                let write_set = schema.get_u64(data, "dwWriteSegment").unwrap_or(0) as u32;
                let pinned = schema.get_bool(data, "BackingStoreWasPinned").unwrap_or(false);
                let page_table = schema.get_bool(data, "PageTableOrDirectory").unwrap_or(false);

                let entry = self.allocs.entry(gh).or_insert_with(|| VidMmAllocation {
                    global_handle: gh,
                    adapter,
                    pid,
                    device: hdev,
                    size,
                    flags,
                    preferred_segment_raw: pref_raw,
                    preferred_segment_id: pref_id,
                    read_segment_set: read_set,
                    write_segment_set: write_set,
                    current_segment_id: None,
                    demoted: false,
                    evicted: false,
                    page_table_or_dir: page_table,
                    backing_store_pinned: pinned,
                    cpu_va: None,
                    gpu_va_start: None,
                    gpu_va_end: None,
                    page_in_count: 0,
                    evict_count: 0,
                    fault_count: 0,
                    alive: true,
                });
                if pid != 0 {
                    entry.pid = pid;
                }
                if adapter != 0 {
                    entry.adapter = adapter;
                }
                if size != 0 {
                    entry.size = size;
                }
                entry.flags = flags;
                entry.preferred_segment_raw = pref_raw;
                entry.preferred_segment_id = pref_id;
                entry.read_segment_set = read_set;
                entry.write_segment_set = write_set;
                entry.backing_store_pinned = pinned;
                entry.page_table_or_dir = page_table;
                entry.alive = true;
            }
            // DeviceAllocation Start (36) / Stop (37) / DC_Start (38)
            36..=38 => {
                let h_alloc = schema.get_u64(data, "hVidMmAlloc").unwrap_or(0);
                let gh = schema.get_u64(data, "hVidMmGlobalAlloc").unwrap_or(0);
                if h_alloc != 0 && gh != 0 {
                    self.alloc_to_global.insert(h_alloc, gh);
                }
                if id == 37 {
                    return;
                }
                let hdev = schema.get_u64(data, "hDevice").unwrap_or(0);
                let mut pid = schema.get_u64(data, "hProcessId").unwrap_or(0) as u32;
                if pid == 0 && hdev != 0 {
                    pid = self.device_pid.get(&hdev).copied().unwrap_or(0);
                }
                let cpu_va = schema.get_u64(data, "pVirtualAddress").filter(|&v| v != 0);
                if gh != 0
                    && let Some(a) = self.allocs.get_mut(&gh)
                {
                    if a.pid == 0 && pid != 0 {
                        a.pid = pid;
                    }
                    if a.cpu_va.is_none() {
                        a.cpu_va = cpu_va;
                    }
                }
            }
            // TerminateAllocation / ProcessTerminateAllocation
            39 | 40 => {
                if let Some(h_alloc) = schema.get_u64(data, "hVidMmAlloc")
                    && let Some(&gh) = self.alloc_to_global.get(&h_alloc)
                    && let Some(a) = self.allocs.get_mut(&gh)
                {
                    a.alive = false;
                }
            }
            // ReportCommittedAllocation (80) & ReportCommittedGlobalAllocation (227)
            80 | 227 => {
                let gh_raw = schema
                    .get_any_u64(data, &["hGlobalAllocationHandle", "hAllocationHandle"])
                    .unwrap_or(0);
                let gh = self.alloc_to_global.get(&gh_raw).copied().unwrap_or(gh_raw);
                let seg_id = schema.get_u64(data, "ulSegmentId").unwrap_or(0) as u32;
                let size = schema.get_u64(data, "Size").unwrap_or(0);
                let pid = schema.get_u64(data, "hProcessId").unwrap_or(0) as u32;
                if gh != 0 {
                    let entry = self.allocs.entry(gh).or_insert_with(|| VidMmAllocation {
                        global_handle: gh,
                        adapter: 0,
                        pid,
                        device: schema.get_u64(data, "hDevice").unwrap_or(0),
                        size,
                        flags: 0,
                        preferred_segment_raw: 0,
                        preferred_segment_id: 0,
                        read_segment_set: 0,
                        write_segment_set: 0,
                        current_segment_id: Some(seg_id),
                        demoted: false,
                        evicted: false,
                        page_table_or_dir: false,
                        backing_store_pinned: false,
                        cpu_va: None,
                        gpu_va_start: None,
                        gpu_va_end: None,
                        page_in_count: 0,
                        evict_count: 0,
                        fault_count: 0,
                        alive: true,
                    });
                    if entry.pid == 0 && pid != 0 {
                        entry.pid = pid;
                    }
                    if entry.size == 0 && size != 0 {
                        entry.size = size;
                    }
                    entry.current_segment_id = Some(seg_id);
                    entry.evicted = false;
                }
            }
            // ReserveResource/Start (67): records actual segment VidMm reserves for an allocation
            67 => {
                let seg_id = schema.get_u64(data, "SegmentId").unwrap_or(0) as u32;
                let gh_raw = schema
                    .get_any_u64(data, &["hAllocationGlobalHandle", "hGlobalAllocationHandle"])
                    .unwrap_or(0);
                let gh = self.alloc_to_global.get(&gh_raw).copied().unwrap_or(gh_raw);
                let mut count_it = self.filter_pid.is_none();
                let filter_pid = self.filter_pid;
                if gh != 0
                    && let Some(a) = self.allocs.get_mut(&gh)
                {
                    a.current_segment_id = Some(seg_id);
                    a.evicted = false;
                    if filter_pid.is_none_or(|p| p == a.pid) {
                        count_it = true;
                    }
                }
                if count_it {
                    self.paging.reserve_resource_calls += 1;
                    *self.paging.reserve_resource_by_segment.entry(seg_id).or_insert(0) += 1;
                }
            }
            // PageInAllocation (73)
            73 => {
                let h = schema.get_u64(data, "hAllocationHandle").unwrap_or(0);
                let gh = self.alloc_to_global.get(&h).copied().unwrap_or(h);
                let seg_opt = schema.get_u64(data, "uiSegmentId").map(|v| v as u32);
                let mut matched = self.filter_pid.is_none();
                let filter_pid = self.filter_pid;
                if gh != 0
                    && let Some(a) = self.allocs.get_mut(&gh)
                {
                    if let Some(s) = seg_opt {
                        a.current_segment_id = Some(s);
                    }
                    a.evicted = false;
                    a.page_in_count += 1;
                    if filter_pid.is_none_or(|p| p == a.pid) {
                        matched = true;
                    }
                }
                if matched {
                    self.paging.page_in_allocations += 1;
                }
            }
            // EvictAllocation (74)
            74 => {
                let gh_raw = schema.get_u64(data, "hGlobalAllocationHandle").unwrap_or(0);
                let gh = self.alloc_to_global.get(&gh_raw).copied().unwrap_or(gh_raw);
                let mut matched = self.filter_pid.is_none();
                let filter_pid = self.filter_pid;
                if gh != 0
                    && let Some(a) = self.allocs.get_mut(&gh)
                {
                    a.evicted = true;
                    a.evict_count += 1;
                    if filter_pid.is_none_or(|p| p == a.pid) {
                        matched = true;
                    }
                }
                if matched {
                    self.paging.evict_allocations += 1;
                }
            }
            // AllocationFault (71)
            71 => {
                let h = schema
                    .get_any_u64(data, &["pVidMmGlobalAlloc", "hAllocationHandle"])
                    .unwrap_or(0);
                let gh = self.alloc_to_global.get(&h).copied().unwrap_or(h);
                let mut matched = self.filter_pid.is_none();
                let filter_pid = self.filter_pid;
                if gh != 0
                    && let Some(a) = self.allocs.get_mut(&gh)
                {
                    a.fault_count += 1;
                    if filter_pid.is_none_or(|p| p == a.pid) {
                        matched = true;
                    }
                }
                if matched {
                    self.paging.allocation_faults += 1;
                }
            }
            // MigrateAllocation
            47 => {
                let h = schema.get_u64(data, "hAllocationGlobalHandle").unwrap_or(0);
                let matched =
                    self.filter_pid.is_none() || self.allocs.get(&h).is_some_and(|a| self.matches_pid(a.pid));
                if matched {
                    self.paging.migrate_allocations += 1;
                }
            }
            // VidMmMakeResident (320/374) / MakeResident (338)
            320 | 338 | 374 => {
                if self.filter_pid.is_none() || self.matches_pid(hdr_pid) {
                    self.paging.make_resident_calls += 1;
                }
            }
            // VidMmEvict (321)
            321 => {
                if self.filter_pid.is_none() || self.matches_pid(hdr_pid) {
                    self.paging.vidmm_evict_calls += 1;
                }
            }
            // ResetBackingStore (242)
            242 => {
                let h = schema.get_u64(data, "hGlobalAllocationHandle").unwrap_or(0);
                let matched =
                    self.filter_pid.is_none() || self.allocs.get(&h).is_some_and(|a| self.matches_pid(a.pid));
                if matched {
                    self.paging.reset_backing_store_calls += 1;
                    self.paging.reset_backing_store_pages += schema.get_u64(data, "cPages").unwrap_or(0);
                }
            }
            // PagingQueuePacket Start (322/323)
            322 | 323 => {
                if self.filter_pid.is_none() || self.matches_pid(hdr_pid) {
                    self.paging.paging_queue_packets += 1;
                }
            }
            // VidMmProcessBudgetChange (366)
            366 => {
                let pid = schema.get_u64(data, "ProcessId").unwrap_or(0) as u32;
                if pid != 0 && self.matches_pid(pid) {
                    let group = schema.get_u64(data, "MemorySegmentGroup").unwrap_or(0);
                    let budget = schema.get_u64(data, "NewBudget").unwrap_or(0);
                    let c = self
                        .process_counters
                        .entry(pid)
                        .or_insert_with(|| ProcessVidMmCounters {
                            pid,
                            ..Default::default()
                        });
                    if is_local_group(group) {
                        c.local_budget = Some(budget);
                    } else {
                        c.non_local_budget = Some(budget);
                    }
                }
            }
            // VidMmProcessUsageChange (367)
            367 => {
                let pid = schema.get_u64(data, "ProcessId").unwrap_or(0) as u32;
                if pid != 0 && self.matches_pid(pid) {
                    let group = schema.get_u64(data, "MemorySegmentGroup").unwrap_or(0);
                    let usage = schema.get_u64(data, "NewUsage").unwrap_or(0);
                    let c = self
                        .process_counters
                        .entry(pid)
                        .or_insert_with(|| ProcessVidMmCounters {
                            pid,
                            ..Default::default()
                        });
                    if is_local_group(group) {
                        c.local_usage = Some(usage);
                        c.max_local_usage = c.max_local_usage.max(usage);
                    } else {
                        c.non_local_usage = Some(usage);
                        c.max_non_local_usage = c.max_non_local_usage.max(usage);
                    }
                }
            }
            // VidMmProcessDemotedCommitmentChange (370)
            370 => {
                let pid = schema.get_u64(data, "ProcessId").unwrap_or(0) as u32;
                if pid != 0 && self.matches_pid(pid) {
                    let commit = schema.get_u64(data, "Commitment").unwrap_or(0);
                    let c = self
                        .process_counters
                        .entry(pid)
                        .or_insert_with(|| ProcessVidMmCounters {
                            pid,
                            ..Default::default()
                        });
                    c.demoted_commitment = Some(commit);
                    c.max_demoted_commitment = c.max_demoted_commitment.max(commit);
                }
            }
            // VidMmProcessCommitmentChange (371)
            371 => {
                let pid = schema.get_u64(data, "ProcessId").unwrap_or(0) as u32;
                if pid != 0 && self.matches_pid(pid) {
                    let group = schema.get_u64(data, "MemorySegmentGroup").unwrap_or(0);
                    let commit = schema.get_u64(data, "Commitment").unwrap_or(0);
                    let c = self
                        .process_counters
                        .entry(pid)
                        .or_insert_with(|| ProcessVidMmCounters {
                            pid,
                            ..Default::default()
                        });
                    if is_local_group(group) {
                        c.local_commitment = Some(commit);
                    } else {
                        c.non_local_commitment = Some(commit);
                    }
                }
            }
            // GpuVirtualAddressAllocator DC_Start/Start (335/336)
            335 | 336 => {
                if let Some(va_alloc) = schema.get_any_u64(data, &["pVaAllocator", "pGpuVaAllocator"]) {
                    let pid = schema
                        .get_any_u64(data, &["ProcessId", "hProcessId"])
                        .map(|v| v as u32)
                        .filter(|&p| p != 0)
                        .unwrap_or(hdr_pid);
                    if va_alloc != 0 && pid != 0 {
                        self.va_alloc_pid.insert(va_alloc, pid);
                    }
                }
            }
            // GpuVirtualAddressRangeMapping DC_Start (333) / Start (334) / Stop (435)
            333 | 334 | 435 => {
                let start = schema.get_u64(data, "StartAddress").unwrap_or(0);
                let end = schema.get_u64(data, "EndAddress").unwrap_or(0);
                let owner = schema.get_u64(data, "pOwner").unwrap_or(0);
                let mapped_size = schema
                    .get_u64(data, "MappedAllocationSize")
                    .unwrap_or(end.saturating_sub(start));
                let va_alloc = schema.get_u64(data, "pVaAllocator").unwrap_or(0);
                let gh = self.alloc_to_global.get(&owner).copied().unwrap_or(owner);
                let pid = self
                    .allocs
                    .get(&gh)
                    .map(|a| a.pid)
                    .filter(|&p| p != 0)
                    .or_else(|| self.va_alloc_pid.get(&va_alloc).copied())
                    .unwrap_or(hdr_pid);
                if start != 0 && end > start {
                    if id == 435 {
                        self.gpu_va_live.remove(&(start, end));
                        if self.matches_pid(pid) {
                            self.tile_mappings.gpuva_unmap_events += 1;
                        }
                    } else {
                        self.gpu_va_live.insert((start, end), (gh, mapped_size, pid));
                        if let Some(a) = self.allocs.get_mut(&gh) {
                            a.gpu_va_start = Some(start);
                            a.gpu_va_end = Some(end);
                        }
                        if self.matches_pid(pid) {
                            self.tile_mappings.gpuva_map_events += 1;
                        }
                    }
                }
            }
            // UpdateGpuVirtualAddress (512) — D3D12 UpdateTileMappings
            512 => {
                let num_ops = schema.get_u64(data, "NumOperations").unwrap_or(1);
                let pid = hdr_pid;
                if self.matches_pid(pid) {
                    self.tile_mappings.call_count += 1;
                    self.tile_mappings.total_operations += num_ops;
                }
                if pid != 0 {
                    *self.tile_mappings.by_pid.entry(pid).or_insert(0) += num_ops;
                }
            }
            // ProcessAllocation (225/226)
            225 | 226 => {}
            _ => {}
        }
    }

    fn on_system_memory(&mut self, rec: &EVENT_RECORD) {
        let pid = rec.EventHeader.ProcessId;
        let data = if rec.UserData.is_null() || rec.UserDataLength == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(rec.UserData.cast::<u8>(), rec.UserDataLength as usize) }
        };
        let mut bytes = 4096u64;
        let mut va = 0u64;
        if let Some(schema) = self.schema_for(rec).cloned() {
            if let Some(bc) = schema.get_any_u64(data, &["ByteCount", "ReadOffset", "IoSize"])
                && bc > 0
                && bc <= (1 << 28)
            {
                bytes = bc;
            }
            if let Some(v) = schema.get_any_u64(data, &["VirtualAddress", "VA"]) {
                va = v;
            }
        }
        self.record_hard_fault(pid, va, bytes);
    }

    fn on_classic_pagefault(&mut self, rec: &EVENT_RECORD) {
        // Classic PageFault HardFault is Opcode 32.
        if rec.EventHeader.EventDescriptor.Opcode != 32 {
            return;
        }
        let pid = rec.EventHeader.ProcessId;
        let data = if rec.UserData.is_null() || rec.UserDataLength == 0 {
            &[][..]
        } else {
            unsafe { std::slice::from_raw_parts(rec.UserData.cast::<u8>(), rec.UserDataLength as usize) }
        };
        // Classic HardFault layout on x64: InitialTime(8), ReadOffset(8), VirtualAddress(8), FileObject(8), TThreadId(4), ByteCount(4)
        let (va, bytes) = if data.len() >= 40 {
            let va = u64::from_le_bytes(data[16..24].try_into().unwrap_or([0; 8]));
            let bc = u32::from_le_bytes(data[36..40].try_into().unwrap_or([0; 4])) as u64;
            (va, if bc > 0 { bc } else { 4096 })
        } else {
            (0, 4096)
        };
        self.record_hard_fault(pid, va, bytes);
    }

    fn record_hard_fault(&mut self, pid: u32, va: u64, bytes: u64) {
        *self.hard_faults.by_pid.entry(pid).or_insert(0) += 1;
        if self.matches_pid(pid) {
            self.hard_faults.total_count += 1;
            self.hard_faults.total_bytes = self.hard_faults.total_bytes.saturating_add(bytes);
            if va != 0 && self.hard_faults.target_fault_vas.len() < 100_000 {
                self.hard_faults.target_fault_vas.push(va);
            }
        }
    }

    fn finish(self, etl_path: &Path) -> EtwReport {
        let mut rep = EtwReport {
            etl_path: etl_path.display().to_string(),
            filter_pid: self.filter_pid,
            total_events: self.total_events,
            dxgkrnl_events: self.dxgkrnl_events,
            system_memory_events: self.system_memory_events,
            duration_secs: match (self.first_ts, self.last_ts) {
                (Some(a), Some(b)) if b > a => (b - a) as f64 / 1e7,
                _ => 0.0,
            },
            warnings: Vec::new(),
            segments: self
                .segments
                .into_values()
                .map(|mut s| {
                    if s.resident_samples == 0 {
                        s.resident_min = 0;
                    }
                    s
                })
                .collect(),
            process_counters: self.process_counters,
            allocations: Vec::new(),
            gpu_va_mappings: Vec::new(),
            tile_mappings: self.tile_mappings,
            paging: self.paging,
            hard_faults: self.hard_faults,
            ..Default::default()
        };

        // Resolve each allocation's actual segment kind & detect silent host fallback / demotion.
        let mut allocs: Vec<VidMmAllocation> = self
            .allocs
            .into_values()
            .filter(|a| a.alive && (self.filter_pid.is_none() || self.filter_pid == Some(a.pid)))
            .collect();

        for a in &mut allocs {
            let pref_kind = if a.preferred_segment_id != 0 {
                rep.segment_kind(a.adapter, a.preferred_segment_id)
            } else {
                SegmentKind::Unknown
            };
            let actual_seg = a.current_segment_id.or_else(|| {
                // If no explicit ReportCommitted/PageIn/ReserveResource event arrived yet, infer from
                // supported segment masks when only a single segment is supported.
                let mask = a.read_segment_set | a.write_segment_set;
                if mask.count_ones() == 1 {
                    Some(mask.trailing_zeros() + 1)
                } else {
                    None
                }
            });
            let actual_kind = actual_seg
                .map(|id| rep.segment_kind(a.adapter, id))
                .unwrap_or(SegmentKind::Unknown);

            // Detect demotion: preferred a Local VRAM segment, but VidMm placed it in NonLocal/Host
            // (or it was evicted out of VRAM).
            if pref_kind.is_local() && (actual_kind.is_non_local() || a.evicted) {
                a.demoted = true;
            }
            // Also detect implicit host placement: allocation requested Local in preferred_segment or
            // supports both Local and NonLocal, and current_segment_id == 0 (sysmem backing).
            if a.current_segment_id == Some(0) && pref_kind.is_local() {
                a.demoted = true;
            }

            if !a.page_table_or_dir {
                rep.tracked_total_bytes = rep.tracked_total_bytes.saturating_add(a.size);
                if a.evicted {
                    rep.tracked_evicted_bytes = rep.tracked_evicted_bytes.saturating_add(a.size);
                }
                if a.demoted {
                    rep.tracked_demoted_bytes = rep.tracked_demoted_bytes.saturating_add(a.size);
                }
                match actual_kind {
                    SegmentKind::LocalVram => {
                        rep.tracked_local_vram_bytes = rep.tracked_local_vram_bytes.saturating_add(a.size);
                    }
                    SegmentKind::LocalCpuVisibleVram => {
                        rep.tracked_local_bar_bytes = rep.tracked_local_bar_bytes.saturating_add(a.size);
                    }
                    SegmentKind::NonLocalHost => {
                        rep.tracked_non_local_host_bytes =
                            rep.tracked_non_local_host_bytes.saturating_add(a.size);
                    }
                    SegmentKind::Unknown => {
                        // Fall back to preferred segment kind only for unknown-segment accounting?
                        // Keep strictly separate so we never lie about unproven segment placement!
                        rep.tracked_unknown_segment_bytes =
                            rep.tracked_unknown_segment_bytes.saturating_add(a.size);
                    }
                }
            }
        }

        // Index live allocations by handle for GPU VA mapping enrichment.
        let alloc_by_gh: HashMap<u64, (Option<u32>, bool, SegmentKind)> = allocs
            .iter()
            .map(|a| {
                let kind = a
                    .current_segment_id
                    .map(|s| rep.segment_kind(a.adapter, s))
                    .unwrap_or(SegmentKind::Unknown);
                (a.global_handle, (a.current_segment_id, a.demoted, kind))
            })
            .collect();

        for ((start, end), (gh, mapped_size, pid)) in self.gpu_va_live {
            if self.filter_pid.is_some() && self.filter_pid != Some(pid) && !alloc_by_gh.contains_key(&gh) {
                continue;
            }
            let (seg_id, placement) = match alloc_by_gh.get(&gh) {
                Some(&(seg, true, _)) => (seg, "demoted_host".to_string()),
                Some(&(seg, false, kind)) => (seg, kind.as_str().to_string()),
                None => (None, "unknown".to_string()),
            };
            rep.gpu_va_mappings.push(GpuVaMapping {
                start,
                end,
                owner_handle: gh,
                mapped_size,
                segment_id: seg_id,
                placement,
            });
        }
        rep.gpu_va_mappings.sort_by_key(|m| (m.start, m.end));

        allocs.sort_by_key(|a| std::cmp::Reverse(a.size));
        rep.allocations = allocs;
        rep
    }
}

/// Classifies a WDDM segment from `ReportSegment.Flags` (`DXGK_SEGMENTFLAGS`) and `MemorySegmentGroup`.
///
/// Observed on Windows 11 26200 (DxgKrnl ETW):
/// - `DXGK_SEGMENTFLAGS`: bit 0 = `Aperture`, bit 2 = `CpuVisible`, bit 6 = `PopulatedFromSystemMemory`,
///   bit 13 (`0x2000`) = `SupportsCpuHostAperture`, bit 19 (`0x80000`) = `LocalBudgetGroup`,
///   bit 20 (`0x100000`) = `NonLocalBudgetGroup`.
/// - `MemorySegmentGroup` in DxgKrnl ETW is 0-based: `0` = `LOCAL`, `1` = `NON_LOCAL`.
pub fn classify_segment(flags: u32, group: u8) -> SegmentKind {
    let aperture = (flags & 0x1) != 0;
    let cpu_visible = (flags & 0x4) != 0 || (flags & 0x2000) != 0;
    let sysmem = (flags & 0x40) != 0;
    let non_local_budget = (flags & (1 << 20)) != 0;
    let local_budget = (flags & (1 << 19)) != 0;

    if group == 1 || non_local_budget || aperture || sysmem {
        SegmentKind::NonLocalHost
    } else if group == 0 || local_budget {
        if cpu_visible {
            SegmentKind::LocalCpuVisibleVram
        } else {
            SegmentKind::LocalVram
        }
    } else {
        SegmentKind::Unknown
    }
}

/// Maps `MemorySegmentGroup` from VidMm process events (366/367/371):
/// DxgKrnl ETW uses 0-based DXGI/D3DKMT values: `0 = Local`, `1 = NonLocal`.
#[inline]
fn is_local_group(group: u64) -> bool {
    group == 0
}

unsafe extern "system" fn event_record_callback(rec: *mut EVENT_RECORD) {
    if rec.is_null() {
        return;
    }
    let rec_ref = unsafe { &*rec };
    if rec_ref.UserContext.is_null() {
        return;
    }
    let state = unsafe { &mut *rec_ref.UserContext.cast::<ParserState>() };
    state.on_event(rec_ref);
}

/// Parses an `.etl` trace file directly via `OpenTraceW` + `ProcessTrace` + cached TDH schemas.
pub fn parse_etl(path: &Path, filter_pid: Option<u32>) -> Result<EtwReport, String> {
    if !path.is_file() {
        return Err(format!("ETL file not found: {}", path.display()));
    }
    let mut wide_path: Vec<u16> = path
        .as_os_str()
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut state = Box::new(ParserState::new(filter_pid));
    let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
    logfile.LogFileName = PWSTR(wide_path.as_mut_ptr());
    logfile.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_EVENT_RECORD;
    logfile.Anonymous2.EventRecordCallback = Some(event_record_callback);
    logfile.Context = (&mut *state as *mut ParserState).cast::<c_void>();

    let handle: PROCESSTRACE_HANDLE = unsafe { OpenTraceW(&mut logfile) };
    if handle.Value == u64::MAX {
        return Err(format!(
            "OpenTraceW({}) failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }

    let handles = [handle];
    let rc = unsafe { ProcessTrace(&handles, None, None) };
    unsafe {
        let _ = CloseTrace(handle);
    }
    if rc != ERROR_SUCCESS {
        return Err(format!(
            "ProcessTrace({}) failed with Win32 error {}",
            path.display(),
            rc.0
        ));
    }

    Ok(state.finish(path))
}

/// Helper to pass a `PCWSTR` cleanly if needed.
#[allow(dead_code)]
fn _assert_pcwstr(_: PCWSTR) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_classification_distinguishes_bar_vram_and_host() {
        // Local dedicated VRAM (not CPU visible), group=0
        assert_eq!(classify_segment(0x0008_0000, 0), SegmentKind::LocalVram);
        // Local CPU-visible BAR VRAM (GPU_UPLOAD / ReBAR, RTX 5070 Ti seg 2), group=0
        assert_eq!(classify_segment(0x0009_2c10, 0), SegmentKind::LocalCpuVisibleVram);
        // Aperture / Non-local host memory, group=1
        assert_eq!(classify_segment(0x0010_0091, 1), SegmentKind::NonLocalHost);
        // System memory segment, group=1
        assert_eq!(classify_segment(0x0010_1194, 1), SegmentKind::NonLocalHost);
        // PopulatedFromSystemMemory even if group=0
        assert_eq!(classify_segment(0x0000_0040, 0), SegmentKind::NonLocalHost);
    }
}
