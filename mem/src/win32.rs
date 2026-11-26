//! Out-of-process, zero-suspend Win32 virtual-memory, working-set, and D3DKMT video-memory inspection.
//!
//! Opens the target process strictly with `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ` (no suspend
//! permissions, zero `SuspendThread` / `NtSuspendProcess` / `PssCaptureSnapshot` calls).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::c_void;

use serde::Serialize;
use windows::Wdk::Graphics::Direct3D::{
    D3DKMT_ADAPTERINFO, D3DKMT_CLOSEADAPTER, D3DKMT_ENUMADAPTERS2, D3DKMT_MEMORY_SEGMENT_GROUP_LOCAL,
    D3DKMT_MEMORY_SEGMENT_GROUP_NON_LOCAL, D3DKMT_QUERYVIDEOMEMORYINFO, D3DKMTCloseAdapter,
    D3DKMTEnumAdapters2, D3DKMTQueryVideoMemoryInfo,
};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LUID};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_IMAGE, MEM_MAPPED, MEM_PRIVATE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_GUARD,
    PAGE_NOACCESS, PAGE_WRITECOMBINE, VirtualQueryEx,
};
use windows::Win32::System::ProcessStatus::{
    GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX, PROCESS_MEMORY_COUNTERS_EX2,
    PSAPI_WORKING_SET_EX_INFORMATION, QueryWorkingSetEx,
};
use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_VM_READ};

use crate::THREAD_SUSPENDS;

/// Process-wide OS counters from `GetProcessMemoryInfo`.
#[derive(Clone, Debug, Default, Serialize)]
pub struct ProcessCounters {
    pub private_usage: u64,
    pub working_set_size: u64,
    pub private_working_set_size: Option<u64>,
    pub shared_commit_usage: Option<u64>,
    pub peak_working_set_size: u64,
    pub peak_pagefile_usage: u64,
    pub page_fault_count: u64,
}

/// Out-of-process `D3DKMTQueryVideoMemoryInfo` (identical data source to `IDXGIAdapter3::QueryVideoMemoryInfo`).
#[derive(Clone, Debug, Default, Serialize)]
pub struct AdapterVideoMemoryInfo {
    pub adapter_luid_low: u32,
    pub adapter_luid_high: i32,
    pub adapter_name: String,
    pub local_usage: u64,
    pub local_budget: u64,
    pub local_reservation: u64,
    pub non_local_usage: u64,
    pub non_local_budget: u64,
    pub non_local_reservation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    Private,
    Mapped,
    Image,
    Other(u32),
}

impl MemoryType {
    fn from_win32(v: u32) -> Self {
        if v == MEM_PRIVATE.0 {
            Self::Private
        } else if v == MEM_MAPPED.0 {
            Self::Mapped
        } else if v == MEM_IMAGE.0 {
            Self::Image
        } else {
            Self::Other(v)
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Mapped => "mapped",
            Self::Image => "image",
            Self::Other(_) => "other",
        }
    }
}

/// One `MEM_COMMIT` or `MEM_RESERVE` virtual-memory region with exact page residency breakdown.
#[derive(Clone, Debug, Serialize)]
pub struct RegionInfo {
    pub base: u64,
    pub allocation_base: u64,
    pub size: u64,
    pub committed: bool,
    pub mem_type: MemoryType,
    pub protect: u32,
    pub write_combined: bool,
    pub no_access: bool,
    /// Bytes currently resident in system RAM working set (`Valid == 1` with PFN working-set entry).
    pub resident_bytes: u64,
    /// Resident private RAM bytes (`Valid == 1 && Shared == 0`, excluding PFN-less BAR/IO mappings).
    pub resident_private_bytes: u64,
    /// Resident shared RAM bytes (`Valid == 1 && Shared == 1`).
    pub resident_shared_bytes: u64,
    /// Valid write-combined PCIe BAR / device-IO mapped bytes (`Valid == 1`, `PAGE_WRITECOMBINE`,
    /// zero WS priority bits `0x7F00_0000` — mapped to device VRAM rather than system RAM WS).
    pub bar_mapped_bytes: u64,
    /// Committed bytes backed and trimmed / paged-out / in standby/modified/compressed store
    /// (`Valid == 0` with non-zero kernel working-set flags).
    pub paged_out_bytes: u64,
    /// Committed bytes never touched (demand-zero PTE, `Valid == 0` with zero kernel flags).
    pub untouched_bytes: u64,
    pub locked_bytes: u64,
    pub large_page_bytes: u64,
}

/// Totals across all virtual-memory regions of a process.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VaTotals {
    pub commit_bytes: u64,
    pub reserve_bytes: u64,
    pub private_commit_bytes: u64,
    pub mapped_commit_bytes: u64,
    pub image_commit_bytes: u64,
    pub write_combined_commit_bytes: u64,
    pub resident_bytes: u64,
    pub resident_private_bytes: u64,
    pub resident_shared_bytes: u64,
    pub bar_mapped_bytes: u64,
    pub paged_out_bytes: u64,
    pub untouched_bytes: u64,
    pub locked_bytes: u64,
    pub large_page_bytes: u64,
    pub region_count: usize,
    pub allocation_count: usize,
    pub pages_queried: u64,
}

/// Complete zero-suspend Win32 + D3DKMT snapshot of a process.
#[derive(Clone, Debug, Serialize)]
pub struct ProcessMemorySnapshot {
    pub pid: u32,
    pub page_size: u64,
    pub sample_step: u64,
    pub thread_suspends: u64,
    pub elapsed_ms: f64,
    pub counters: ProcessCounters,
    pub totals: VaTotals,
    pub video_memory: Vec<AdapterVideoMemoryInfo>,
    pub regions: Vec<RegionInfo>,
}

/// Inspection options for `inspect_process`.
#[derive(Clone, Copy, Debug)]
pub struct InspectOptions {
    /// Query every `sample_step`-th page with `QueryWorkingSetEx`. `1` = exact every 4 KiB page.
    pub sample_step: u64,
}

impl Default for InspectOptions {
    fn default() -> Self {
        Self { sample_step: 1 }
    }
}

/// Inspects `pid` out-of-process with zero thread suspends.
pub fn inspect_process(pid: u32, opts: InspectOptions) -> Result<ProcessMemorySnapshot, String> {
    let t0 = std::time::Instant::now();
    let step = opts.sample_step.max(1);

    let handle = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) }
        .map_err(|e| format!("OpenProcess({pid}, QUERY_INFORMATION|VM_READ): {e}"))?;

    let result = inspect_handle(handle, pid, step, t0);
    unsafe {
        let _ = CloseHandle(handle);
    }
    result
}

fn inspect_handle(
    handle: HANDLE,
    pid: u32,
    step: u64,
    t0: std::time::Instant,
) -> Result<ProcessMemorySnapshot, String> {
    let mut sys_info = SYSTEM_INFO::default();
    unsafe { GetSystemInfo(&mut sys_info) };
    let page_size = (sys_info.dwPageSize as u64).max(4096);
    let max_addr = sys_info.lpMaximumApplicationAddress as u64;

    let counters = query_process_counters(handle)?;
    let video_memory = query_video_memory_info(handle);

    let mut regions = Vec::with_capacity(2048);
    let mut totals = VaTotals::default();
    let mut alloc_bases = BTreeMap::<u64, ()>::new();

    // Reusable 64K-entry buffer (~1 MiB) for batched QueryWorkingSetEx calls (256 MiB/call at step=1).
    const BATCH_PAGES: usize = 65_536;
    let mut ws_buf: Vec<PSAPI_WORKING_SET_EX_INFORMATION> =
        vec![PSAPI_WORKING_SET_EX_INFORMATION::default(); BATCH_PAGES];

    let mut addr: u64 = 0;
    let mbi_size = size_of::<MEMORY_BASIC_INFORMATION>();

    while addr < max_addr {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        let ret = unsafe { VirtualQueryEx(handle, Some(addr as *const c_void), &mut mbi, mbi_size) };
        if ret == 0 || mbi.RegionSize == 0 {
            break;
        }
        let base = mbi.BaseAddress as u64;
        let size = mbi.RegionSize as u64;
        let next = base.saturating_add(size);
        if next <= addr {
            break;
        }
        addr = next;

        let committed = mbi.State == MEM_COMMIT;
        let reserved = mbi.State == MEM_RESERVE;
        if !committed && !reserved {
            continue;
        }

        let mem_type = MemoryType::from_win32(mbi.Type.0);
        let protect = mbi.Protect.0;
        let write_combined = (protect & PAGE_WRITECOMBINE.0) != 0;
        let no_access = (protect & (PAGE_NOACCESS.0 | PAGE_GUARD.0)) != 0;

        alloc_bases.insert(mbi.AllocationBase as u64, ());

        let mut reg = RegionInfo {
            base,
            allocation_base: mbi.AllocationBase as u64,
            size,
            committed,
            mem_type,
            protect,
            write_combined,
            no_access,
            resident_bytes: 0,
            resident_private_bytes: 0,
            resident_shared_bytes: 0,
            bar_mapped_bytes: 0,
            paged_out_bytes: 0,
            untouched_bytes: 0,
            locked_bytes: 0,
            large_page_bytes: 0,
        };

        if committed {
            let total_pages = size / page_size;
            if total_pages > 0 {
                let sample_count = total_pages.div_ceil(step);
                let mut res_priv = 0u64;
                let mut res_shared = 0u64;
                let mut bar_mapped = 0u64;
                let mut paged_out = 0u64;
                let mut untouched = 0u64;
                let mut locked = 0u64;
                let mut large = 0u64;

                let mut done = 0u64;
                while done < sample_count {
                    let chunk = ((sample_count - done) as usize).min(BATCH_PAGES);
                    for (i, entry) in ws_buf[..chunk].iter_mut().enumerate() {
                        let page_idx = (done + i as u64) * step;
                        entry.VirtualAddress = (base + page_idx * page_size) as *mut c_void;
                        entry.VirtualAttributes.Flags = 0;
                    }
                    let cb = (chunk * size_of::<PSAPI_WORKING_SET_EX_INFORMATION>()) as u32;
                    let ok = unsafe { QueryWorkingSetEx(handle, ws_buf.as_mut_ptr().cast::<c_void>(), cb) };
                    if ok.is_ok() {
                        totals.pages_queried += chunk as u64;
                        for entry in &ws_buf[..chunk] {
                            let flags = unsafe { entry.VirtualAttributes.Flags };
                            classify_ws_flags(
                                flags,
                                write_combined,
                                &mut res_priv,
                                &mut res_shared,
                                &mut bar_mapped,
                                &mut paged_out,
                                &mut untouched,
                                &mut locked,
                                &mut large,
                            );
                        }
                    } else {
                        // Conservative fallback: if QueryWorkingSetEx rejects a special region,
                        // treat pages as untouched rather than inventing resident RAM.
                        untouched += chunk as u64;
                    }
                    done += chunk as u64;
                }

                let scale = |count: u64| -> u64 {
                    if step == 1 {
                        count * page_size
                    } else {
                        ((count as u128 * size as u128) / sample_count as u128) as u64
                    }
                };
                reg.resident_private_bytes = scale(res_priv);
                reg.resident_shared_bytes = scale(res_shared);
                reg.resident_bytes = reg.resident_private_bytes + reg.resident_shared_bytes;
                reg.bar_mapped_bytes = scale(bar_mapped);
                reg.paged_out_bytes = scale(paged_out);
                reg.untouched_bytes = size
                    .saturating_sub(reg.resident_bytes)
                    .saturating_sub(reg.bar_mapped_bytes)
                    .saturating_sub(reg.paged_out_bytes);
                reg.locked_bytes = scale(locked);
                reg.large_page_bytes = scale(large);
            }

            totals.commit_bytes += size;
            match mem_type {
                MemoryType::Private => totals.private_commit_bytes += size,
                MemoryType::Mapped => totals.mapped_commit_bytes += size,
                MemoryType::Image => totals.image_commit_bytes += size,
                MemoryType::Other(_) => {}
            }
            if write_combined {
                totals.write_combined_commit_bytes += size;
            }
            totals.resident_bytes += reg.resident_bytes;
            totals.resident_private_bytes += reg.resident_private_bytes;
            totals.resident_shared_bytes += reg.resident_shared_bytes;
            totals.bar_mapped_bytes += reg.bar_mapped_bytes;
            totals.paged_out_bytes += reg.paged_out_bytes;
            totals.untouched_bytes += reg.untouched_bytes;
            totals.locked_bytes += reg.locked_bytes;
            totals.large_page_bytes += reg.large_page_bytes;
        } else {
            totals.reserve_bytes += size;
        }

        totals.region_count += 1;
        regions.push(reg);
    }
    totals.allocation_count = alloc_bases.len();

    Ok(ProcessMemorySnapshot {
        pid,
        page_size,
        sample_step: step,
        thread_suspends: THREAD_SUSPENDS,
        elapsed_ms: t0.elapsed().as_secs_f64() * 1e3,
        counters,
        totals,
        video_memory,
        regions,
    })
}

/// Classifies one `PSAPI_WORKING_SET_EX_BLOCK.Flags` word.
///
/// Bit layout on Windows x64 (`psapi.h` / `ntmmapi.h`), empirically verified on Windows 11 26200:
/// - Bit 0 (`0x1`): `Valid` (hardware PTE is valid)
/// - Bit 15 (`0x8000`): `Shared`
/// - Bit 22 (`0x40_0000`): `Locked` (when Valid == 1)
/// - Bit 23 (`0x80_0000`): `LargePage` (when Valid == 1)
/// - Bits 24..30 (`0x7F00_0000`): working-set priority / kernel WS entry state.
///   PFN-backed system RAM pages in `PROCESS_MEMORY_COUNTERS.WorkingSetSize` carry a non-zero priority
///   (e.g. `0x0400_0041` / `0x0500_0041` / `0x0500_4041` for host-fallback WC RAM), whereas PCIe BAR
///   device-VRAM mappings (`PAGE_WRITECOMBINE` IO space, e.g. `0x0000_4041`) are not in the system RAM
///   working set and have zero bits in `0x7F00_0000`.
/// - Bit 31 (`0x8000_0000`): `Bad`
/// - When `Valid == 0`:
///   - `Flags & !BAD == 0`: demand-zero / untouched `MEM_COMMIT` page (never faulted or clean zero).
///   - `Flags & !BAD != 0`: backed page trimmed / paged-out (standby, modified, compressed store, or pagefile).
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn classify_ws_flags(
    flags: usize,
    write_combined: bool,
    res_priv: &mut u64,
    res_shared: &mut u64,
    bar_mapped: &mut u64,
    paged_out: &mut u64,
    untouched: &mut u64,
    locked: &mut u64,
    large: &mut u64,
) {
    const VALID_BIT: usize = 1 << 0;
    const SHARED_BIT: usize = 1 << 15;
    const LOCKED_BIT: usize = 1 << 22;
    const LARGE_BIT: usize = 1 << 23;
    const WS_PRIORITY_MASK: usize = 0x7F00_0000;
    const BAD_BIT: usize = 1 << 31;

    if (flags & VALID_BIT) != 0 {
        if write_combined && (flags & WS_PRIORITY_MASK) == 0 {
            *bar_mapped += 1;
        } else {
            if (flags & SHARED_BIT) != 0 {
                *res_shared += 1;
            } else {
                *res_priv += 1;
            }
            if (flags & LOCKED_BIT) != 0 {
                *locked += 1;
            }
            if (flags & LARGE_BIT) != 0 {
                *large += 1;
            }
        }
    } else if (flags & !BAD_BIT) != 0 {
        *paged_out += 1;
    } else {
        *untouched += 1;
    }
}

fn query_process_counters(handle: HANDLE) -> Result<ProcessCounters, String> {
    let mut ex2 = PROCESS_MEMORY_COUNTERS_EX2 {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX2>() as u32,
        ..Default::default()
    };
    let ok2 = unsafe {
        GetProcessMemoryInfo(
            handle,
            (&mut ex2 as *mut PROCESS_MEMORY_COUNTERS_EX2).cast::<PROCESS_MEMORY_COUNTERS>(),
            ex2.cb,
        )
    };
    if ok2.is_ok() {
        return Ok(ProcessCounters {
            private_usage: ex2.PrivateUsage as u64,
            working_set_size: ex2.WorkingSetSize as u64,
            private_working_set_size: Some(ex2.PrivateWorkingSetSize as u64),
            shared_commit_usage: Some(ex2.SharedCommitUsage),
            peak_working_set_size: ex2.PeakWorkingSetSize as u64,
            peak_pagefile_usage: ex2.PeakPagefileUsage as u64,
            page_fault_count: ex2.PageFaultCount as u64,
        });
    }

    let mut ex = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    unsafe {
        GetProcessMemoryInfo(
            handle,
            (&mut ex as *mut PROCESS_MEMORY_COUNTERS_EX).cast::<PROCESS_MEMORY_COUNTERS>(),
            ex.cb,
        )
    }
    .map_err(|e| format!("GetProcessMemoryInfo: {e}"))?;

    Ok(ProcessCounters {
        private_usage: ex.PrivateUsage as u64,
        working_set_size: ex.WorkingSetSize as u64,
        private_working_set_size: None,
        shared_commit_usage: None,
        peak_working_set_size: ex.PeakWorkingSetSize as u64,
        peak_pagefile_usage: ex.PeakPagefileUsage as u64,
        page_fault_count: ex.PageFaultCount as u64,
    })
}

/// Queries per-adapter `QueryVideoMemoryInfo` (`Local` and `NonLocal`) for `handle` out-of-process.
pub fn query_video_memory_info(handle: HANDLE) -> Vec<AdapterVideoMemoryInfo> {
    let mut enum_req = D3DKMT_ENUMADAPTERS2::default();
    let status = unsafe { D3DKMTEnumAdapters2(&mut enum_req) };
    if status.0 != 0 || enum_req.NumAdapters == 0 {
        return Vec::new();
    }
    let mut adapters = vec![D3DKMT_ADAPTERINFO::default(); enum_req.NumAdapters as usize];
    enum_req.pAdapters = adapters.as_mut_ptr();
    let status = unsafe { D3DKMTEnumAdapters2(&mut enum_req) };
    if status.0 != 0 {
        return Vec::new();
    }
    adapters.truncate(enum_req.NumAdapters as usize);

    let dxgi_factory: Option<IDXGIFactory4> = unsafe { CreateDXGIFactory1().ok() };
    let mut out = Vec::with_capacity(adapters.len());

    for a in &adapters {
        let mut local = D3DKMT_QUERYVIDEOMEMORYINFO {
            hProcess: handle,
            hAdapter: a.hAdapter,
            MemorySegmentGroup: D3DKMT_MEMORY_SEGMENT_GROUP_LOCAL,
            PhysicalAdapterIndex: 0,
            ..Default::default()
        };
        let mut nonlocal = D3DKMT_QUERYVIDEOMEMORYINFO {
            hProcess: handle,
            hAdapter: a.hAdapter,
            MemorySegmentGroup: D3DKMT_MEMORY_SEGMENT_GROUP_NON_LOCAL,
            PhysicalAdapterIndex: 0,
            ..Default::default()
        };
        let s_loc = unsafe { D3DKMTQueryVideoMemoryInfo(&mut local) };
        let s_non = unsafe { D3DKMTQueryVideoMemoryInfo(&mut nonlocal) };

        let adapter_name = dxgi_factory
            .as_ref()
            .and_then(|f| {
                let luid = LUID {
                    LowPart: a.AdapterLuid.LowPart,
                    HighPart: a.AdapterLuid.HighPart,
                };
                unsafe { f.EnumAdapterByLuid::<IDXGIAdapter1>(luid).ok() }
            })
            .and_then(|ad| unsafe { ad.GetDesc1().ok() })
            .map(|d| {
                String::from_utf16_lossy(&d.Description)
                    .trim_end_matches('\0')
                    .to_string()
            })
            .unwrap_or_else(|| format!("LUID {:#x}:{:#x}", a.AdapterLuid.HighPart, a.AdapterLuid.LowPart));

        if s_loc.0 == 0 || s_non.0 == 0 {
            out.push(AdapterVideoMemoryInfo {
                adapter_luid_low: a.AdapterLuid.LowPart,
                adapter_luid_high: a.AdapterLuid.HighPart,
                adapter_name,
                local_usage: if s_loc.0 == 0 { local.CurrentUsage } else { 0 },
                local_budget: if s_loc.0 == 0 { local.Budget } else { 0 },
                local_reservation: if s_loc.0 == 0 { local.CurrentReservation } else { 0 },
                non_local_usage: if s_non.0 == 0 { nonlocal.CurrentUsage } else { 0 },
                non_local_budget: if s_non.0 == 0 { nonlocal.Budget } else { 0 },
                non_local_reservation: if s_non.0 == 0 {
                    nonlocal.CurrentReservation
                } else {
                    0
                },
            });
        }

        let close = D3DKMT_CLOSEADAPTER { hAdapter: a.hAdapter };
        unsafe {
            let _ = D3DKMTCloseAdapter(&close);
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    #[test]
    fn untouched_mem_commit_reports_zero_resident() {
        const BYTES: usize = 4 << 20;
        let ptr = unsafe { VirtualAlloc(None, BYTES, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE) };
        assert!(!ptr.is_null());
        let base = ptr as u64;
        let snap = inspect_process(std::process::id(), InspectOptions::default()).unwrap();
        let reg = snap
            .regions
            .iter()
            .find(|r| r.base <= base && base < r.base + r.size)
            .expect("committed region found");
        assert!(reg.committed);
        assert_eq!(
            reg.resident_bytes, 0,
            "untouched MEM_COMMIT must have 0 resident bytes"
        );
        assert_eq!(reg.untouched_bytes, reg.size);
        assert_eq!(snap.thread_suspends, 0);

        // Now touch 16 pages and verify exact residency.
        unsafe {
            for i in 0..16usize {
                (ptr as *mut u8).add(i * 4096).write_volatile(0xA5);
            }
        }
        let snap2 = inspect_process(std::process::id(), InspectOptions::default()).unwrap();
        let reg2 = snap2
            .regions
            .iter()
            .find(|r| r.base <= base && base < r.base + r.size)
            .expect("committed region found after touch");
        assert_eq!(reg2.resident_bytes, 16 * 4096);
        assert_eq!(reg2.untouched_bytes, reg2.size - 16 * 4096);

        unsafe {
            let _ = VirtualFree(ptr, 0, MEM_RELEASE);
        }
    }

    #[test]
    fn ws_flags_distinguish_bar_vram_host_fallback_trimmed_and_untouched() {
        let run = |flags: usize, wc: bool| {
            let (mut rp, mut rs, mut bar, mut po, mut un, mut lk, mut lg) = (0, 0, 0, 0, 0, 0, 0);
            classify_ws_flags(
                flags, wc, &mut rp, &mut rs, &mut bar, &mut po, &mut un, &mut lk, &mut lg,
            );
            (rp, rs, bar, po, un)
        };
        // Untouched demand-zero MEM_COMMIT
        assert_eq!(run(0x0000_0000, false), (0, 0, 0, 0, 1));
        // Trimmed / paged-out dirty page
        assert_eq!(run(0x0c40_0000, false), (0, 0, 0, 1, 0));
        // 1 GiB GPU_UPLOAD in PCIe BAR VRAM (Valid=1, WC, priority=0)
        assert_eq!(run(0x0000_4041, true), (0, 0, 1, 0, 0));
        // 3 GiB GPU_UPLOAD silent host fallback in system RAM (Valid=1, WC, priority=5)
        assert_eq!(run(0x0500_4041, true), (1, 0, 0, 0, 0));
        // CUSTOM L0 WriteBack in system RAM (Valid=1, non-WC, priority=5)
        assert_eq!(run(0x0500_0041, false), (1, 0, 0, 0, 0));
    }

    #[test]
    fn resolves_child_and_exe_name_from_process_list() {
        let procs = vec![
            ProcessEntry {
                pid: 100,
                parent_pid: 1,
                exe_name: "runner.exe".into(),
            },
            ProcessEntry {
                pid: 101,
                parent_pid: 100,
                exe_name: "conhost.exe".into(),
            },
            ProcessEntry {
                pid: 200,
                parent_pid: 100,
                exe_name: "GameTitle.exe".into(),
            },
            ProcessEntry {
                pid: 300,
                parent_pid: 200,
                exe_name: "CrashHelper.exe".into(),
            },
        ];
        // Direct child skips conhost.exe and picks GameTitle.exe
        assert_eq!(
            resolve_target_pid_from_list(&procs, None, Some(100), None, 999).unwrap(),
            200
        );
        // Combined --pid-of-child + --exe-name can match grandchild too
        assert_eq!(
            resolve_target_pid_from_list(&procs, None, Some(100), Some("crashhelper"), 999).unwrap(),
            300
        );
        // --exe-name works with or without .exe, case-insensitively
        assert_eq!(
            resolve_target_pid_from_list(&procs, None, None, Some("gametitle"), 999).unwrap(),
            200
        );
        assert_eq!(
            resolve_target_pid_from_list(&procs, None, None, Some("GameTitle.exe"), 999).unwrap(),
            200
        );
        // Explicit --pid passes through
        assert_eq!(
            resolve_target_pid_from_list(&procs, Some(200), None, None, 999).unwrap(),
            200
        );
    }
}

/// Entry from `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessEntry {
    pub pid: u32,
    pub parent_pid: u32,
    pub exe_name: String,
}

/// Lists all running processes via `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)` (zero suspends).
pub fn list_processes() -> Result<Vec<ProcessEntry>, String> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|e| format!("CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS) failed: {e}"))?;

    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    if unsafe { Process32FirstW(snap, &mut entry) }.is_ok() {
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let exe_name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            out.push(ProcessEntry {
                pid: entry.th32ProcessID,
                parent_pid: entry.th32ParentProcessID,
                exe_name,
            });
            if unsafe { Process32NextW(snap, &mut entry) }.is_err() {
                break;
            }
        }
    }
    unsafe {
        let _ = CloseHandle(snap);
    }
    Ok(out)
}

/// Resolves a single target PID from `--pid`, `--pid-of-child <PARENT_PID>`, and/or `--exe-name <NAME>`.
///
/// Uses `CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)` only (zero thread suspends).
pub fn resolve_target_pid(
    pid: Option<u32>,
    pid_of_child: Option<u32>,
    exe_name: Option<&str>,
) -> Result<u32, String> {
    if let Some(p) = pid {
        return Ok(p);
    }
    if pid_of_child.is_none() && exe_name.is_none() {
        return Err("specify one of --pid <PID>, --pid-of-child <PARENT_PID>, or --exe-name <NAME>".into());
    }
    let procs = list_processes()?;
    resolve_target_pid_from_list(&procs, pid, pid_of_child, exe_name, std::process::id())
}

fn exe_matches(actual: &str, wanted: &str) -> bool {
    let w = std::path::Path::new(wanted)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(wanted)
        .trim();
    if w.is_empty() {
        return false;
    }
    if actual.eq_ignore_ascii_case(w) {
        return true;
    }
    let actual_stem = actual
        .strip_suffix(".exe")
        .or_else(|| actual.strip_suffix(".EXE"))
        .unwrap_or(actual);
    let wanted_stem = w
        .strip_suffix(".exe")
        .or_else(|| w.strip_suffix(".EXE"))
        .unwrap_or(w);
    actual_stem.eq_ignore_ascii_case(wanted_stem)
}

fn is_console_helper(exe: &str) -> bool {
    exe.eq_ignore_ascii_case("conhost.exe") || exe.eq_ignore_ascii_case("openconsole.exe")
}

fn resolve_target_pid_from_list(
    procs: &[ProcessEntry],
    pid: Option<u32>,
    pid_of_child: Option<u32>,
    exe_name: Option<&str>,
    self_pid: u32,
) -> Result<u32, String> {
    if let Some(p) = pid {
        return Ok(p);
    }

    if let Some(ppid) = pid_of_child {
        let mut children_by_parent: HashMap<u32, Vec<&ProcessEntry>> = HashMap::new();
        for p in procs {
            if p.pid != 0 && p.pid != self_pid {
                children_by_parent.entry(p.parent_pid).or_default().push(p);
            }
        }
        let mut descendants: Vec<(&ProcessEntry, usize)> = Vec::new();
        let mut q = VecDeque::new();
        q.push_back((ppid, 0usize));
        while let Some((cur, depth)) = q.pop_front() {
            if let Some(kids) = children_by_parent.get(&cur) {
                for &k in kids {
                    if k.pid == ppid || descendants.iter().any(|(d, _)| d.pid == k.pid) {
                        continue;
                    }
                    descendants.push((k, depth + 1));
                    q.push_back((k.pid, depth + 1));
                }
            }
        }

        if let Some(want) = exe_name {
            let matches: Vec<_> = descendants
                .into_iter()
                .filter(|(p, _)| exe_matches(&p.exe_name, want))
                .collect();
            return match matches.as_slice() {
                [] => Err(format!(
                    "no live child/descendant of parent PID {ppid} matching --exe-name {want:?}"
                )),
                [(only, _)] => Ok(only.pid),
                many => {
                    let list = many
                        .iter()
                        .map(|(p, d)| format!("pid={} ({}, depth={d})", p.pid, p.exe_name))
                        .collect::<Vec<_>>()
                        .join(", ");
                    Err(format!(
                        "multiple descendants of parent PID {ppid} match --exe-name {want:?}: {list}; pass --pid"
                    ))
                }
            };
        }

        // No --exe-name filter: prefer direct non-console children, then deeper non-console descendants.
        let non_console: Vec<_> = descendants
            .iter()
            .copied()
            .filter(|(p, _)| !is_console_helper(&p.exe_name))
            .collect();
        let pool = if non_console.is_empty() {
            descendants
        } else {
            non_console
        };
        let direct: Vec<_> = pool.iter().copied().filter(|(_, d)| *d == 1).collect();
        let candidates = if !direct.is_empty() { direct } else { pool };

        return match candidates.as_slice() {
            [] => Err(format!("no live child process found for parent PID {ppid}")),
            [(only, _)] => Ok(only.pid),
            many => {
                let list = many
                    .iter()
                    .map(|(p, d)| format!("pid={} ({}, depth={d})", p.pid, p.exe_name))
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!(
                    "multiple child processes found for parent PID {ppid}: {list}; narrow with --exe-name <NAME> or pass --pid"
                ))
            }
        };
    }

    if let Some(want) = exe_name {
        let matches: Vec<&ProcessEntry> = procs
            .iter()
            .filter(|p| p.pid != 0 && p.pid != self_pid && exe_matches(&p.exe_name, want))
            .collect();
        return match matches.as_slice() {
            [] => Err(format!("no running process matching --exe-name {want:?}")),
            [only] => Ok(only.pid),
            many => {
                let list = many
                    .iter()
                    .map(|p| format!("pid={} (parent={}, {})", p.pid, p.parent_pid, p.exe_name))
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(format!(
                    "multiple running processes match --exe-name {want:?}: {list}; narrow with --pid-of-child <PPID> or pass --pid"
                ))
            }
        };
    }

    Err("specify one of --pid <PID>, --pid-of-child <PARENT_PID>, or --exe-name <NAME>".into())
}
