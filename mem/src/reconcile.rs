//! Reconciliation, conservation accounting, silent host-fallback detection, and report formatting.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::THREAD_SUSPENDS;
use crate::capture_join::CaptureJoinReport;
use crate::etw::{EtwReport, SegmentKind, VidMmAllocation};
use crate::labels::LabelMap;
use crate::win32::{MemoryType, ProcessMemorySnapshot, live_children_of};

/// Standard human-readable size buckets for VidMm and Win32 VA histograms.
pub const SIZE_BUCKETS: [&str; 7] = [
    "<64K", "64K..1M", "1..4M", "4..16M", "16..64M", "64..256M", ">=256M",
];

pub fn size_bucket_index(bytes: u64) -> usize {
    const K: u64 = 1024;
    const M: u64 = 1024 * 1024;
    if bytes < 64 * K {
        0
    } else if bytes < M {
        1
    } else if bytes < 4 * M {
        2
    } else if bytes < 16 * M {
        3
    } else if bytes < 64 * M {
        4
    } else if bytes < 256 * M {
        5
    } else {
        6
    }
}

/// Parses human-friendly byte sizes like `64K`, `64KiB`, `1M`, `64MiB`, `2G`, or raw byte integers.
pub fn parse_byte_size(raw: &str) -> Result<u64, String> {
    let s = raw.trim().replace('_', "");
    if s.is_empty() {
        return Err("empty size".into());
    }
    let lower = s.to_ascii_lowercase();
    let (num_str, mult): (&str, u64) = if let Some(p) = lower.strip_suffix("gib") {
        (p, 1024 * 1024 * 1024)
    } else if let Some(p) = lower.strip_suffix("gb") {
        (p, 1024 * 1024 * 1024)
    } else if let Some(p) = lower.strip_suffix('g') {
        (p, 1024 * 1024 * 1024)
    } else if let Some(p) = lower.strip_suffix("mib") {
        (p, 1024 * 1024)
    } else if let Some(p) = lower.strip_suffix("mb") {
        (p, 1024 * 1024)
    } else if let Some(p) = lower.strip_suffix('m') {
        (p, 1024 * 1024)
    } else if let Some(p) = lower.strip_suffix("kib") {
        (p, 1024)
    } else if let Some(p) = lower.strip_suffix("kb") {
        (p, 1024)
    } else if let Some(p) = lower.strip_suffix('k') {
        (p, 1024)
    } else if let Some(p) = lower.strip_suffix('b') {
        (p, 1)
    } else {
        (lower.as_str(), 1)
    };
    let val: f64 = num_str
        .trim()
        .parse()
        .map_err(|e| format!("invalid size {raw:?}: {e}"))?;
    if !val.is_finite() || val < 0.0 {
        return Err(format!("invalid negative/non-finite size {raw:?}"));
    }
    Ok((val * mult as f64).round() as u64)
}

/// Filter for VidMm allocation placement in text tables and `mem diff`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlacementFilter {
    #[default]
    All,
    Local,
    Vram,
    Bar,
    Host,
    Demoted,
    Evicted,
    Unknown,
}

impl PlacementFilter {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "all" => Ok(Self::All),
            "local" => Ok(Self::Local),
            "vram" | "local_vram" => Ok(Self::Vram),
            "bar" | "local_bar" | "local_bar_vram" => Ok(Self::Bar),
            "host" | "nonlocal" | "non_local" | "non_local_host" | "sysmem" => Ok(Self::Host),
            "demoted" => Ok(Self::Demoted),
            "evicted" => Ok(Self::Evicted),
            "unknown" => Ok(Self::Unknown),
            other => Err(format!(
                "invalid --placement {other:?} (expected: all, local, vram, bar, host, demoted, evicted, unknown)"
            )),
        }
    }

    pub fn matches_label(self, label: &str) -> bool {
        match self {
            Self::All => true,
            Self::Local => matches!(label, "local_vram" | "local_bar_vram"),
            Self::Vram => label == "local_vram",
            Self::Bar => label == "local_bar_vram",
            Self::Host => matches!(label, "non_local_host" | "demoted_host"),
            Self::Demoted => label == "demoted_host",
            Self::Evicted => label == "evicted",
            Self::Unknown => label == "unknown",
        }
    }
}

/// Sort order for Top-N allocation tables.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AllocSort {
    #[default]
    Size,
    Newest,
    Oldest,
}

impl AllocSort {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "size" => Ok(Self::Size),
            "newest" | "new" | "recent" => Ok(Self::Newest),
            "oldest" | "old" | "time" => Ok(Self::Oldest),
            other => Err(format!(
                "invalid --sort {other:?} (expected: size, newest, oldest)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Size => "size",
            Self::Newest => "newest",
            Self::Oldest => "oldest",
        }
    }
}

/// Formatting and filtering options for `format_text_with_options` and `format_diff_text`.
#[derive(Clone, Debug)]
pub struct FormatOptions {
    pub top: usize,
    pub placement: PlacementFilter,
    pub min_bytes: Option<u64>,
    pub max_bytes: Option<u64>,
    pub sort: AllocSort,
    pub all_segments: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        Self {
            top: 25,
            placement: PlacementFilter::All,
            min_bytes: None,
            max_bytes: None,
            sort: AllocSort::Size,
            all_segments: false,
        }
    }
}

pub fn alloc_placement_label(e: &EtwReport, a: &VidMmAllocation) -> &'static str {
    if a.demoted {
        return "demoted_host";
    }
    if a.evicted {
        return "evicted";
    }
    a.current_segment_id
        .map(|s| e.segment_kind(a.adapter, s).as_str())
        .unwrap_or("unknown")
}

pub fn alloc_matches_placement(e: &EtwReport, a: &VidMmAllocation, filter: PlacementFilter) -> bool {
    match filter {
        PlacementFilter::All => true,
        PlacementFilter::Demoted => a.demoted,
        PlacementFilter::Evicted => a.evicted,
        _ => {
            let kind = a
                .current_segment_id
                .map(|s| e.segment_kind(a.adapter, s))
                .unwrap_or(SegmentKind::Unknown);
            match filter {
                PlacementFilter::Local => kind.is_local(),
                PlacementFilter::Vram => kind == SegmentKind::LocalVram,
                PlacementFilter::Bar => kind == SegmentKind::LocalCpuVisibleVram,
                PlacementFilter::Host => kind.is_non_local() || a.demoted,
                PlacementFilter::Unknown => kind == SegmentKind::Unknown,
                PlacementFilter::All | PlacementFilter::Demoted | PlacementFilter::Evicted => unreachable!(),
            }
        }
    }
}

/// Alert raised when `QueryVideoMemoryInfo` or requested heap placement disagrees with true OS/VidMm placement.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlacementAlert {
    pub kind: String,
    pub owner: Option<String>,
    pub bytes: u64,
    pub detail: String,
}

/// Per-owner summary built from `--labels <ranges.tsv>` across Win32 VA, HardFaults, VidMm ETW, and Capture.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OwnerBreakdown {
    pub owner: String,
    pub va_commit_bytes: u64,
    pub va_reserve_bytes: u64,
    pub resident_bytes: u64,
    pub resident_private_bytes: u64,
    pub resident_shared_bytes: u64,
    pub bar_mapped_bytes: u64,
    /// Total `Valid == 0, Flags != 0` bytes (`paged_out_private_bytes + non_ws_mapped_bytes`).
    pub paged_out_bytes: u64,
    /// `MEM_PRIVATE` pages trimmed/paged-out to pagefile/compressed/standby/modified lists.
    #[serde(default)]
    pub paged_out_private_bytes: u64,
    /// `MEM_MAPPED` / `MEM_IMAGE` section pages merely not in the working set (not private pagefile paging).
    #[serde(default)]
    pub non_ws_mapped_bytes: u64,
    pub untouched_bytes: u64,
    pub write_combined_bytes: u64,
    pub region_count: usize,
    pub vidmm_local_bytes: u64,
    pub vidmm_local_bar_bytes: u64,
    pub vidmm_non_local_bytes: u64,
    pub vidmm_demoted_bytes: u64,
    pub hard_fault_count: u64,
    pub capture_descriptors: u64,
    pub capture_draws: u64,
}

/// Conservation check metrics for CPU (`PrivateUsage`, `WorkingSetSize`) and GPU (`VidMm Local + NonLocal`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConservationSummary {
    pub thread_suspends: u64,
    // CPU WorkingSet conservation
    pub os_working_set_bytes: u64,
    pub accounted_working_set_bytes: u64,
    pub working_set_accounted_pct: f64,
    // CPU PrivateUsage conservation
    pub os_private_usage_bytes: u64,
    pub va_private_commit_bytes: u64,
    pub vidmm_kernel_backing_estimate_bytes: u64,
    pub accounted_private_usage_bytes: u64,
    pub private_usage_accounted_pct: f64,
    // GPU VidMm conservation
    pub qvmi_local_bytes: u64,
    pub qvmi_non_local_bytes: u64,
    pub qvmi_total_bytes: u64,
    pub etw_local_bytes: u64,
    pub etw_local_bar_bytes: u64,
    pub etw_non_local_bytes: u64,
    pub etw_demoted_bytes: u64,
    pub etw_unknown_bytes: u64,
    pub etw_process_local_commitment: Option<u64>,
    pub etw_process_non_local_commitment: Option<u64>,
    pub etw_process_demoted_commitment: Option<u64>,
    /// Adapter-wide sum of `TotalBytesResidentInSegment.resident_last` across all processes on the adapter
    /// (explicitly separate from PID-attributed VidMm conservation).
    #[serde(default)]
    pub adapter_segment_resident_bytes: u64,
    pub vidmm_accounted_bytes: u64,
    pub vidmm_reference_bytes: u64,
    pub vidmm_accounted_pct: Option<f64>,
    /// Scope of `vidmm_accounted_pct` (e.g. `pid_allocations_vs_qvmi`, `pid_allocations_vs_process_counters`,
    /// or `unavailable` when allocation/process rundown is missing).
    #[serde(default)]
    pub vidmm_conservation_scope: String,
    /// Explanation when `vidmm_accounted_pct` is `None`.
    #[serde(default)]
    pub vidmm_conservation_unavailable_reason: Option<String>,
}

/// Unified ground-truth memory report combining Win32 inspection, ETW VidMm/HardFaults, labels, and capture join.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MemReport {
    pub pid: Option<u32>,
    pub thread_suspends: u64,
    pub conservation: ConservationSummary,
    #[serde(default)]
    pub warnings: Vec<String>,
    pub alerts: Vec<PlacementAlert>,
    pub by_owner: Vec<OwnerBreakdown>,
    pub win32: Option<ProcessMemorySnapshot>,
    pub etw: Option<EtwReport>,
    pub capture: Option<CaptureJoinReport>,
}

/// Reconciles Win32 snapshot, ETW report, `--labels`, and optional `capture.db` join into one `MemReport`.
pub fn reconcile(
    win32: Option<ProcessMemorySnapshot>,
    etw: Option<EtwReport>,
    labels: &LabelMap,
    capture: Option<CaptureJoinReport>,
) -> MemReport {
    let pid = win32
        .as_ref()
        .map(|w| w.pid)
        .or_else(|| etw.as_ref().and_then(|e| e.filter_pid));
    let mut owners: BTreeMap<String, OwnerBreakdown> = BTreeMap::new();
    let mut alerts = Vec::new();

    // Pre-seed every label from ranges.tsv so even 0-byte or untouched owners appear explicitly.
    for r in labels.ranges() {
        owners.entry(r.owner.clone()).or_insert_with(|| OwnerBreakdown {
            owner: r.owner.clone(),
            ..Default::default()
        });
    }

    // 1. Attribute Win32 VA regions across --labels (splitting regions that straddle label boundaries).
    if let Some(ref w) = win32 {
        for reg in &w.regions {
            let end = reg.base.saturating_add(reg.size);
            let spans = labels.split_region(reg.base, end);
            let total_len = reg.size.max(1) as u128;
            for (s, e, owner_opt) in spans {
                let len = e.saturating_sub(s);
                if len == 0 {
                    continue;
                }
                let owner_key = match owner_opt {
                    Some(o) => o.to_string(),
                    None => {
                        if labels.is_empty() {
                            format!("<{}>", reg.mem_type.as_str())
                        } else {
                            "<unlabeled>".to_string()
                        }
                    }
                };
                let entry = owners.entry(owner_key.clone()).or_insert_with(|| OwnerBreakdown {
                    owner: owner_key,
                    ..Default::default()
                });
                entry.region_count += 1;
                if reg.committed {
                    entry.va_commit_bytes += len;
                    if reg.write_combined {
                        entry.write_combined_bytes += len;
                    }
                    let frac = |v: u64| ((v as u128 * len as u128) / total_len) as u64;
                    let rp = frac(reg.resident_private_bytes);
                    let rs = frac(reg.resident_shared_bytes);
                    let bar = frac(reg.bar_mapped_bytes);
                    let po = frac(reg.paged_out_bytes);
                    let res = rp + rs;
                    let unt = len.saturating_sub(res).saturating_sub(bar).saturating_sub(po);
                    entry.resident_private_bytes += rp;
                    entry.resident_shared_bytes += rs;
                    entry.resident_bytes += res;
                    entry.bar_mapped_bytes += bar;
                    entry.paged_out_bytes += po;
                    if reg.mem_type == MemoryType::Private {
                        entry.paged_out_private_bytes += po;
                    } else {
                        entry.non_ws_mapped_bytes += po;
                    }
                    entry.untouched_bytes += unt;
                } else {
                    entry.va_reserve_bytes += len;
                }
            }
        }
    }

    // 2. Attribute ETW VidMm allocations & HardFaults to labels.
    if let Some(ref e) = etw {
        for a in &e.allocations {
            if a.page_table_or_dir {
                continue;
            }
            let owner_opt = a
                .cpu_va
                .and_then(|va| labels.owner_of(va))
                .or_else(|| a.gpu_va_start.and_then(|va| labels.owner_of(va)));
            let owner_key = owner_opt.unwrap_or("<unlabeled>").to_string();
            let entry = owners.entry(owner_key.clone()).or_insert_with(|| OwnerBreakdown {
                owner: owner_key,
                ..Default::default()
            });
            let kind = a
                .current_segment_id
                .map(|s| e.segment_kind(a.adapter, s))
                .unwrap_or(SegmentKind::Unknown);
            match kind {
                SegmentKind::LocalVram => entry.vidmm_local_bytes += a.size,
                SegmentKind::LocalCpuVisibleVram => entry.vidmm_local_bar_bytes += a.size,
                SegmentKind::NonLocalHost => entry.vidmm_non_local_bytes += a.size,
                SegmentKind::Unknown => {}
            }
            if a.demoted {
                entry.vidmm_demoted_bytes += a.size;
                alerts.push(PlacementAlert {
                    kind: "vidmm_demoted_allocation".into(),
                    owner: owner_opt.map(str::to_string),
                    bytes: a.size,
                    detail: format!(
                        "VidMm allocation {:#x} ({:.1} MiB) preferred segment {} ({}) but placed in segment {:?} ({})",
                        a.global_handle,
                        mib(a.size),
                        a.preferred_segment_id,
                        e.segment_kind(a.adapter, a.preferred_segment_id).as_str(),
                        a.current_segment_id,
                        kind.as_str(),
                    ),
                });
            }
        }

        for &va in &e.hard_faults.target_fault_vas {
            let owner_key = labels.owner_of(va).unwrap_or("<unlabeled>").to_string();
            let entry = owners.entry(owner_key.clone()).or_insert_with(|| OwnerBreakdown {
                owner: owner_key,
                ..Default::default()
            });
            entry.hard_fault_count += 1;
        }
    }

    // 3. Attribute capture.db descriptor join to owners.
    if let Some(ref cap) = capture {
        for o in &cap.by_owner {
            let entry = owners.entry(o.owner.clone()).or_insert_with(|| OwnerBreakdown {
                owner: o.owner.clone(),
                ..Default::default()
            });
            entry.capture_descriptors += o.descriptor_rows;
            entry.capture_draws += o.distinct_draws;
        }
    }

    // 4. Compute CPU + GPU conservation and detect silent GPU_UPLOAD host fallback!
    let mut cons = ConservationSummary {
        thread_suspends: THREAD_SUSPENDS,
        ..Default::default()
    };

    if let Some(ref w) = win32 {
        cons.os_working_set_bytes = w.counters.working_set_size;
        cons.accounted_working_set_bytes = w.totals.resident_bytes;
        cons.working_set_accounted_pct = pct(cons.accounted_working_set_bytes, cons.os_working_set_bytes);

        cons.os_private_usage_bytes = w.counters.private_usage;
        cons.va_private_commit_bytes = w.totals.private_commit_bytes;

        for vm in &w.video_memory {
            cons.qvmi_local_bytes = cons.qvmi_local_bytes.saturating_add(vm.local_usage);
            cons.qvmi_non_local_bytes = cons.qvmi_non_local_bytes.saturating_add(vm.non_local_usage);
        }
        cons.qvmi_total_bytes = cons.qvmi_local_bytes.saturating_add(cons.qvmi_non_local_bytes);
    }

    if let Some(ref e) = etw {
        cons.etw_local_bytes = e.tracked_local_vram_bytes;
        cons.etw_local_bar_bytes = e.tracked_local_bar_bytes;
        cons.etw_non_local_bytes = e.tracked_non_local_host_bytes;
        cons.etw_demoted_bytes = e.tracked_demoted_bytes;
        cons.etw_unknown_bytes = e.tracked_unknown_segment_bytes;

        if let Some(target_pid) = pid {
            if let Some(pc) = e.process_counters.get(&target_pid) {
                cons.etw_process_local_commitment = pc.local_commitment.or(pc.local_usage);
                cons.etw_process_non_local_commitment = pc.non_local_commitment.or(pc.non_local_usage);
                cons.etw_process_demoted_commitment = pc.demoted_commitment;
                if pc.max_demoted_commitment > 0 {
                    alerts.push(PlacementAlert {
                        kind: "vidmm_process_demoted_commitment".into(),
                        owner: None,
                        bytes: pc.max_demoted_commitment,
                        detail: format!(
                            "VidMmProcessDemotedCommitmentChange reported {:.1} MiB demoted out of local VRAM for PID {}",
                            mib(pc.max_demoted_commitment),
                            target_pid
                        ),
                    });
                }
            }
        } else {
            // Aggregate across all processes in an unfiltered ETL.
            let loc: u64 = e
                .process_counters
                .values()
                .filter_map(|p| p.local_commitment.or(p.local_usage))
                .sum();
            let non: u64 = e
                .process_counters
                .values()
                .filter_map(|p| p.non_local_commitment.or(p.non_local_usage))
                .sum();
            let dem: u64 = e
                .process_counters
                .values()
                .filter_map(|p| p.demoted_commitment)
                .sum();
            if loc > 0 || non > 0 {
                cons.etw_process_local_commitment = Some(loc);
                cons.etw_process_non_local_commitment = Some(non);
                cons.etw_process_demoted_commitment = Some(dem);
            }
        }
    }

    // Reconcile PrivateUsage gap:
    // On WDDM 2.x/3.x, kernel-mode VidMm backing stores for VRAM/GPU heaps that are NOT mapped into
    // user-mode `MEM_PRIVATE` VA are still billed to `PrivateUsage` (PagefileUsage) by Windows MM!
    // Conversely, allocations mapped into user VA (`MEM_PRIVATE`) are already in `va_private_commit_bytes`.
    if let Some(ref w) = win32 {
        let gap = cons
            .os_private_usage_bytes
            .saturating_sub(cons.va_private_commit_bytes);
        if gap > 0 {
            // Estimate kernel VidMm backing commit charged to PrivateUsage:
            // tracked VidMm allocations without a user-mode MEM_PRIVATE mapping, or QVMI local+nonlocal.
            let vidmm_unmapped_backing = if let Some(ref e) = etw {
                let unmapped_etw: u64 = e
                    .allocations
                    .iter()
                    .filter(|a| !a.page_table_or_dir && a.cpu_va.is_none())
                    .map(|a| a.size)
                    .sum();
                if unmapped_etw > 0 {
                    unmapped_etw
                } else {
                    cons.qvmi_total_bytes
                }
            } else {
                cons.qvmi_total_bytes
            };
            cons.vidmm_kernel_backing_estimate_bytes = gap.min(vidmm_unmapped_backing);
        }
        // Also check MEM_MAPPED pagefile-backed regions if still below PrivateUsage (rare).
        cons.accounted_private_usage_bytes = cons
            .va_private_commit_bytes
            .saturating_add(cons.vidmm_kernel_backing_estimate_bytes);
        cons.private_usage_accounted_pct =
            pct(cons.accounted_private_usage_bytes, cons.os_private_usage_bytes);

        // Silent host-fallback detection:
        // Case A: QueryVideoMemoryInfo reports `local_usage`, while ETW VidMm true placement shows
        // substantially less in Local+BAR VRAM (and/or substantially more in NonLocal host / demoted).
        let etw_true_local = cons.etw_local_bytes.saturating_add(cons.etw_local_bar_bytes);
        if cons.qvmi_local_bytes >= (64 << 20)
            && etw.is_some()
            && (etw_true_local > 0 || cons.etw_non_local_bytes > 0)
        {
            let missing_local = cons.qvmi_local_bytes.saturating_sub(etw_true_local);
            if missing_local >= (256 << 20)
                && cons.etw_non_local_bytes >= cons.qvmi_non_local_bytes.saturating_add(128 << 20)
            {
                alerts.push(PlacementAlert {
                    kind: "silent_host_fallback_qvmi_mismatch".into(),
                    owner: None,
                    bytes: missing_local,
                    detail: format!(
                        "QueryVideoMemoryInfo reports {:.1} MiB Local / {:.1} MiB NonLocal, but DxgKrnl VidMm placed only {:.1} MiB in Local+BAR and {:.1} MiB in NonLocal host (discrepancy {:.1} MiB)",
                        mib(cons.qvmi_local_bytes),
                        mib(cons.qvmi_non_local_bytes),
                        mib(etw_true_local),
                        mib(cons.etw_non_local_bytes),
                        mib(missing_local),
                    ),
                });
            }
        }

        // Case B: Per-label Win32 + VidMm cross-check for large driver CPU mappings (>2 GiB single
        // write-combined or mapped GPU_UPLOAD buffer silently backed in system RAM / NonLocal segment).
        // When NVIDIA falls back a >2 GiB GPU_UPLOAD buffer to host memory, its CPU mapping shows up
        // as a >2 GiB MEM_PRIVATE/MEM_MAPPED region whose pages are backed in host RAM (or VidMm NonLocal)
        // even if QueryVideoMemoryInfo counts it in Local!
        for reg in &w.regions {
            if !reg.committed || reg.size < (2048 << 20) {
                continue;
            }
            if reg.write_combined && reg.mem_type == MemoryType::Private {
                let owner = labels.owner_of(reg.base).map(str::to_string);
                // Check if ETW confirms NonLocal or if resident host RAM backs a WC mapping.
                let etw_nonlocal_for_owner = owner
                    .as_deref()
                    .and_then(|o| owners.get(o))
                    .map(|b| b.vidmm_non_local_bytes)
                    .unwrap_or(0);
                if etw_nonlocal_for_owner > 0 || reg.resident_bytes > (256 << 20) {
                    alerts.push(PlacementAlert {
                        kind: "gpu_upload_silent_host_fallback".into(),
                        owner: owner.clone(),
                        bytes: reg.size,
                        detail: format!(
                            "Large write-combined mapping at {:#x} ({:.1} MiB, owner={}) has {:.1} MiB host RAM resident / {:.1} MiB VidMm NonLocal (silent host fallback >2 GiB BAR limit)",
                            reg.base,
                            mib(reg.size),
                            owner.as_deref().unwrap_or("<unlabeled>"),
                            mib(reg.resident_bytes),
                            mib(etw_nonlocal_for_owner),
                        ),
                    });
                }
            }
        }
    }

    // Compute GPU VidMm conservation:
    // Compare tracked live allocations (Local + BAR + NonLocal + Unknown) against an independent reference
    // (`QVMI Local + NonLocal`, or ETW `VidMmProcessCommitmentChange`).
    // Never divide adapter-wide `TotalBytesResidentInSegment` by itself to fake 100% conservation!
    if let Some(ref e) = etw {
        let etw_tracked = e.tracked_total_bytes;
        let etw_proc_total = match (
            cons.etw_process_local_commitment,
            cons.etw_process_non_local_commitment,
        ) {
            (Some(l), Some(n)) => l.saturating_add(n),
            (Some(l), None) => l,
            (None, Some(n)) => n,
            (None, None) => 0,
        };
        let seg_resident_total = e.adapter_segment_resident_bytes;
        cons.adapter_segment_resident_bytes = seg_resident_total;

        if cons.qvmi_total_bytes > 0 {
            cons.vidmm_reference_bytes = cons.qvmi_total_bytes;
            if etw_tracked > 0 {
                cons.vidmm_accounted_bytes = etw_tracked;
                cons.vidmm_accounted_pct = Some(pct(cons.vidmm_accounted_bytes, cons.vidmm_reference_bytes));
                cons.vidmm_conservation_scope = "pid_allocations_vs_qvmi".into();
            } else if etw_proc_total > 0 {
                cons.vidmm_accounted_bytes = etw_proc_total;
                cons.vidmm_accounted_pct = Some(pct(cons.vidmm_accounted_bytes, cons.vidmm_reference_bytes));
                cons.vidmm_conservation_scope = "pid_process_counters_vs_qvmi".into();
            } else {
                cons.vidmm_accounted_bytes = 0;
                cons.vidmm_accounted_pct = None;
                cons.vidmm_conservation_scope = "unavailable".into();
                cons.vidmm_conservation_unavailable_reason = Some(
                    "no per-allocation or per-process VidMm rundown in trace to reconcile against QueryVideoMemoryInfo"
                        .into(),
                );
            }
        } else if etw_proc_total > 0 && etw_tracked > 0 {
            cons.vidmm_reference_bytes = etw_proc_total;
            cons.vidmm_accounted_bytes = etw_tracked;
            cons.vidmm_accounted_pct = Some(pct(cons.vidmm_accounted_bytes, cons.vidmm_reference_bytes));
            cons.vidmm_conservation_scope = if pid.is_some() {
                "pid_allocations_vs_process_counters".into()
            } else {
                "all_pids_allocations_vs_process_counters".into()
            };
        } else {
            cons.vidmm_reference_bytes = 0;
            cons.vidmm_accounted_bytes = etw_tracked;
            cons.vidmm_accounted_pct = None;
            cons.vidmm_conservation_scope = "unavailable".into();
            let reason = if !e.coverage.has_allocation_rundown && !e.coverage.has_process_counters {
                if seg_resident_total > 0 {
                    format!(
                        "no per-allocation or per-process VidMm rundown in trace (only adapter-wide segment residency total {:.2} MiB across all processes is present)",
                        mib(seg_resident_total)
                    )
                } else {
                    "no per-allocation or per-process VidMm rundown in trace".into()
                }
            } else if !e.coverage.has_allocation_rundown {
                "no per-allocation VidMm rundown in trace (process counters present, but individual allocations/heaps are untracked)".into()
            } else {
                "no QueryVideoMemoryInfo or per-process VidMm commitment reference available for this PID"
                    .into()
            };
            cons.vidmm_conservation_unavailable_reason = Some(reason);
        }
    } else {
        cons.vidmm_conservation_scope = "unavailable".into();
        if cons.qvmi_total_bytes > 0 {
            cons.vidmm_reference_bytes = cons.qvmi_total_bytes;
            cons.vidmm_conservation_unavailable_reason =
                Some("no DxgKrnl ETW trace supplied to reconcile against QueryVideoMemoryInfo".into());
        }
    }

    let mut by_owner: Vec<OwnerBreakdown> = owners.into_values().collect();
    by_owner.sort_by(|a, b| {
        let ka = a
            .va_commit_bytes
            .max(a.vidmm_local_bytes + a.vidmm_local_bar_bytes + a.vidmm_non_local_bytes);
        let kb = b
            .va_commit_bytes
            .max(b.vidmm_local_bytes + b.vidmm_local_bar_bytes + b.vidmm_non_local_bytes);
        kb.cmp(&ka).then_with(|| a.owner.cmp(&b.owner))
    });

    let mut warnings = Vec::new();
    // Warn if the target process exited or was tearing down during Win32 inspection.
    if let Some(ref w) = win32 {
        if w.process_exited {
            let code = w
                .exit_code
                .map(|c| format!("{c:#x}"))
                .unwrap_or_else(|| "unknown".into());
            warnings.push(format!(
                "PID {} exited before/during inspection (exit_code={code}); VA, working set, and D3DKMT counters reflect a partial teardown state",
                w.pid
            ));
        }
        if w.video_memory.is_empty() {
            if let Some(ref err) = w.video_memory_error {
                warnings.push(format!(
                    "Out-of-process D3DKMT QueryVideoMemoryInfo unavailable ({err}); process may be terminating or have no D3D device"
                ));
            } else {
                warnings.push(
                    "Out-of-process D3DKMT QueryVideoMemoryInfo returned no adapters; process may be terminating or have no D3D device"
                        .into(),
                );
            }
        }
        if !w.process_exited
            && w.video_memory.is_empty()
            && w.totals.image_commit_bytes < (2 << 20)
            && w.totals.commit_bytes > (16 << 20)
        {
            warnings.push(format!(
                "Very low MEM_IMAGE commit ({:.2} MiB of {:.2} MiB committed VA) with missing D3DKMT: process was likely terminating / tearing down its address space during inspection",
                mib(w.totals.image_commit_bytes),
                mib(w.totals.commit_bytes),
            ));
        }
    }
    // Warn if the inspected/traced PID has no D3D device usage or non-page-table VidMm allocations
    // and has live child processes (common when passing a launcher/runner PID instead of the title PID).
    let has_non_pt_vidmm = etw
        .as_ref()
        .is_some_and(|e| e.tracked_total_bytes > 0 || e.allocations.iter().any(|a| !a.page_table_or_dir));
    if let Some(target_pid) = pid
        && cons.qvmi_total_bytes == 0
        && !has_non_pt_vidmm
    {
        let kids = live_children_of(target_pid);
        if !kids.is_empty() {
            let list = kids
                .iter()
                .map(|k| format!("pid={} ({})", k.pid, k.exe_name))
                .collect::<Vec<_>>()
                .join(", ");
            warnings.push(format!(
                "PID {target_pid} has no D3D video-memory usage or VidMm allocations and has live child process(es): {list}; did you mean `--pid-of-child {target_pid}`?"
            ));
        }
    }

    MemReport {
        pid,
        thread_suspends: THREAD_SUSPENDS,
        conservation: cons,
        warnings,
        alerts,
        by_owner,
        win32,
        etw,
        capture,
    }
}

pub fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn pct(part: u64, total: u64) -> f64 {
    if total == 0 {
        100.0
    } else {
        (part as f64 / total as f64) * 100.0
    }
}

/// Renders a concise, structured text report suitable for terminal and log inspection.
pub fn format_text(rep: &MemReport, top: usize) -> String {
    format_text_with_options(
        rep,
        None,
        &FormatOptions {
            top,
            ..Default::default()
        },
    )
}

/// Renders a structured text report with explicit filtering, sorting, histograms, and owner labels.
pub fn format_text_with_options(rep: &MemReport, labels: Option<&LabelMap>, opts: &FormatOptions) -> String {
    let mut out = String::new();
    use std::fmt::Write;
    let top = opts.top;

    let pid_str = rep.pid.map(|p| p.to_string()).unwrap_or_else(|| "all".into());
    let _ = writeln!(
        out,
        "locum-mem ground-truth report | pid={pid_str} | thread_suspends={}",
        rep.thread_suspends
    );
    for w in &rep.warnings {
        let _ = writeln!(out, "  [warn] {w}");
    }

    if let Some(ref w) = rep.win32 {
        let _ = writeln!(
            out,
            "\n== Win32 CPU Memory (walk {:.1} ms, step={}, {} regions, {} allocs, {} pages queried) ==",
            w.elapsed_ms,
            w.sample_step,
            w.totals.region_count,
            w.totals.allocation_count,
            w.totals.pages_queried
        );
        let _ = writeln!(
            out,
            "  OS PrivateUsage       : {:>10.2} MiB   (Peak Pagefile: {:.2} MiB)",
            mib(w.counters.private_usage),
            mib(w.counters.peak_pagefile_usage),
        );
        let _ = writeln!(
            out,
            "  OS WorkingSetSize     : {:>10.2} MiB   (Private WS: {} | PageFaults: {})",
            mib(w.counters.working_set_size),
            w.counters
                .private_working_set_size
                .map(|v| format!("{:.2} MiB", mib(v)))
                .unwrap_or_else(|| "n/a".into()),
            w.counters.page_fault_count
        );
        let _ = writeln!(
            out,
            "  VA Committed          : {:>10.2} MiB   (private {:.2}, mapped {:.2}, image {:.2}, WC {:.2}) | Reserved: {:.2} MiB",
            mib(w.totals.commit_bytes),
            mib(w.totals.private_commit_bytes),
            mib(w.totals.mapped_commit_bytes),
            mib(w.totals.image_commit_bytes),
            mib(w.totals.write_combined_commit_bytes),
            mib(w.totals.reserve_bytes),
        );
        // Backfill split for legacy JSONs that only serialized `paged_out_bytes`.
        let (paged_priv, non_ws_mapped) = if w.totals.paged_out_private_bytes == 0
            && w.totals.non_ws_mapped_bytes == 0
            && w.totals.paged_out_bytes > 0
        {
            let mut pp = 0u64;
            let mut nm = 0u64;
            for r in &w.regions {
                if r.committed {
                    if r.mem_type == MemoryType::Private {
                        pp += r.paged_out_bytes;
                    } else {
                        nm += r.paged_out_bytes;
                    }
                }
            }
            (pp, nm)
        } else {
            (w.totals.paged_out_private_bytes, w.totals.non_ws_mapped_bytes)
        };
        let _ = writeln!(
            out,
            "  VA Split              : RAM resident {:>9.2} MiB (priv {:.2}, shared {:.2}) | BAR/IO {:>8.2} MiB | paged-out priv {:>8.2} MiB | non-WS mapped/img {:>8.2} MiB | untouched {:>9.2} MiB",
            mib(w.totals.resident_bytes),
            mib(w.totals.resident_private_bytes),
            mib(w.totals.resident_shared_bytes),
            mib(w.totals.bar_mapped_bytes),
            mib(paged_priv),
            mib(non_ws_mapped),
            mib(w.totals.untouched_bytes),
        );
        let _ = writeln!(
            out,
            "  CPU Conservation      : WorkingSet {:.2}% ({:.2}/{:.2} MiB) | PrivateUsage {:.2}% (VA {:.2} + VidMm backing {:.2} / {:.2} MiB)",
            rep.conservation.working_set_accounted_pct,
            mib(rep.conservation.accounted_working_set_bytes),
            mib(rep.conservation.os_working_set_bytes),
            rep.conservation.private_usage_accounted_pct,
            mib(rep.conservation.va_private_commit_bytes),
            mib(rep.conservation.vidmm_kernel_backing_estimate_bytes),
            mib(rep.conservation.os_private_usage_bytes),
        );

        // Win32 committed allocations grouped by allocation_base: histogram + Top-N (especially valuable for `inspect`).
        if !w.regions.is_empty() {
            struct Win32AllocAgg {
                base: u64,
                commit: u64,
                ram: u64,
                bar: u64,
                paged_priv: u64,
                non_ws_mapped: u64,
                untouched: u64,
                mem_type: MemoryType,
                wc: bool,
                regions: usize,
            }
            let mut by_base: BTreeMap<u64, Win32AllocAgg> = BTreeMap::new();
            for r in &w.regions {
                if !r.committed {
                    continue;
                }
                let e = by_base.entry(r.allocation_base).or_insert(Win32AllocAgg {
                    base: r.allocation_base,
                    commit: 0,
                    ram: 0,
                    bar: 0,
                    paged_priv: 0,
                    non_ws_mapped: 0,
                    untouched: 0,
                    mem_type: r.mem_type,
                    wc: r.write_combined,
                    regions: 0,
                });
                e.commit += r.size;
                e.ram += r.resident_bytes;
                e.bar += r.bar_mapped_bytes;
                if r.mem_type == MemoryType::Private {
                    e.paged_priv += r.paged_out_bytes;
                } else {
                    e.non_ws_mapped += r.paged_out_bytes;
                }
                e.untouched += r.untouched_bytes;
                e.wc |= r.write_combined;
                e.regions += 1;
            }
            if !by_base.is_empty() {
                let mut hist: BTreeMap<&str, [(usize, u64); 7]> = BTreeMap::new();
                for a in by_base.values() {
                    let key = if a.wc {
                        match a.mem_type {
                            MemoryType::Private => "private_wc",
                            MemoryType::Mapped => "mapped_wc",
                            MemoryType::Image => "image",
                            MemoryType::Other(_) => "other",
                        }
                    } else {
                        a.mem_type.as_str()
                    };
                    let b = size_bucket_index(a.commit);
                    let row = hist.entry(key).or_insert([(0, 0); 7]);
                    row[b].0 += 1;
                    row[b].1 += a.commit;
                }
                let _ = writeln!(out, "\n  Win32 Committed VA Size Histogram (by allocation_base):");
                let _ = writeln!(
                    out,
                    "    {:<12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
                    "type",
                    SIZE_BUCKETS[0],
                    SIZE_BUCKETS[1],
                    SIZE_BUCKETS[2],
                    SIZE_BUCKETS[3],
                    SIZE_BUCKETS[4],
                    SIZE_BUCKETS[5],
                    SIZE_BUCKETS[6]
                );
                for (ty, row) in &hist {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|&(n, b)| {
                            if n == 0 {
                                "-".into()
                            } else {
                                format!("{n}/{:.0}M", mib(b))
                            }
                        })
                        .collect();
                    let _ = writeln!(
                        out,
                        "    {:<12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
                        ty, cells[0], cells[1], cells[2], cells[3], cells[4], cells[5], cells[6]
                    );
                }

                if top > 0 && rep.etw.is_none() {
                    let mut list: Vec<Win32AllocAgg> = by_base
                        .into_values()
                        .filter(|a| {
                            opts.min_bytes.is_none_or(|m| a.commit >= m)
                                && opts.max_bytes.is_none_or(|m| a.commit <= m)
                        })
                        .collect();
                    list.sort_by_key(|a| std::cmp::Reverse(a.commit));
                    let _ = writeln!(
                        out,
                        "\n  Top {} Win32 Committed Allocations ({} matching filter):",
                        top.min(list.len()),
                        list.len(),
                    );
                    let _ = writeln!(
                        out,
                        "    {:>9} {:>8} {:>8} {:>9} {:>9} {:>8} {:<10} {:>18} owner",
                        "commit_M", "ram_M", "bar_M", "privpg_M", "mapnws_M", "untch_M", "type", "alloc_base"
                    );
                    for a in list.iter().take(top) {
                        let ty = if a.wc {
                            format!("{}_wc", a.mem_type.as_str())
                        } else {
                            a.mem_type.as_str().into()
                        };
                        let owner = labels.and_then(|l| l.owner_of(a.base)).unwrap_or("<unlabeled>");
                        let _ = writeln!(
                            out,
                            "    {:>9.2} {:>8.2} {:>8.2} {:>9.2} {:>9.2} {:>8.2} {:<10} {:>18} {}",
                            mib(a.commit),
                            mib(a.ram),
                            mib(a.bar),
                            mib(a.paged_priv),
                            mib(a.non_ws_mapped),
                            mib(a.untouched),
                            ty,
                            format!("{:#x}", a.base),
                            owner,
                        );
                    }
                }
            }
        }

        let _ = writeln!(out, "\n== Out-of-Process QueryVideoMemoryInfo (D3DKMT) ==");
        if !w.video_memory.is_empty() {
            for vm in &w.video_memory {
                if vm.local_usage == 0 && vm.non_local_usage == 0 && vm.local_budget == 0 {
                    continue;
                }
                let _ = writeln!(
                    out,
                    "  {:<36} Local {:>8.2} / {:>8.2} MiB | NonLocal {:>8.2} / {:>8.2} MiB",
                    vm.adapter_name,
                    mib(vm.local_usage),
                    mib(vm.local_budget),
                    mib(vm.non_local_usage),
                    mib(vm.non_local_budget),
                );
            }
        } else {
            let reason = w
                .video_memory_error
                .as_deref()
                .unwrap_or("no adapters returned; process may be terminating or have no D3D device");
            let _ = writeln!(out, "  unavailable ({reason})");
        }
    }

    if let Some(ref e) = rep.etw {
        let rate_part = if e.duration_secs > 0.0 {
            let ev_k = (e.total_events as f64 / e.duration_secs) / 1e3;
            if e.etl_size_bytes > 0 {
                let mb = mib(e.etl_size_bytes);
                let mb_s = mb / e.duration_secs;
                format!(", {ev_k:.1}k/s, {mb:.1} MiB ({mb_s:.1} MiB/s)")
            } else {
                format!(", {ev_k:.1}k/s")
            }
        } else if e.etl_size_bytes > 0 {
            format!(", {:.1} MiB", mib(e.etl_size_bytes))
        } else {
            String::new()
        };
        let split_part = if e.rundown_events > 0 {
            let steady = e.total_events.saturating_sub(e.rundown_events);
            format!(", rundown={}, steady={steady}", e.rundown_events)
        } else {
            String::new()
        };
        let loss_part = match (e.truncated_by_max_size, e.events_lost > 0) {
            (true, true) => format!(", TRUNCATED, lost={}", e.events_lost),
            (true, false) => ", TRUNCATED".into(),
            (false, true) => format!(", lost={}", e.events_lost),
            (false, false) => String::new(),
        };
        let _ = writeln!(
            out,
            "\n== VidMm / DxgKrnl + Kernel ETW ({} events{rate_part}{split_part}{loss_part}, {:.2} s | {} dxgkrnl, {} sysmem) ==",
            e.total_events, e.duration_secs, e.dxgkrnl_events, e.system_memory_events
        );
        for w in &e.warnings {
            let _ = writeln!(out, "  [warn] {w}");
        }
        if !e.segments.is_empty() {
            if let Some(pid) = rep.pid {
                let _ = writeln!(
                    out,
                    "  Adapter-Wide Segments (all processes on adapter; NOT filtered to pid={pid}):"
                );
            } else {
                let _ = writeln!(out, "  Adapter-Wide Segments (all processes on adapter):");
            }
            let mut hidden = 0usize;
            for s in &e.segments {
                if !opts.all_segments && s.resident_samples == 0 && s.resident_last == 0 {
                    hidden += 1;
                    continue;
                }
                if s.described {
                    let _ = writeln!(
                        out,
                        "    seg {:<2} adapter={:#x} {:<15} size={:>8.1} MiB  flags={:#010x} grp={} resident_last={:>8.1} MiB (min {:.1}, max {:.1}, n={})",
                        s.segment_id,
                        s.adapter,
                        s.kind.as_str(),
                        mib(s.size),
                        s.flags,
                        s.memory_segment_group,
                        mib(s.resident_last),
                        mib(s.resident_min),
                        mib(s.resident_max),
                        s.resident_samples,
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "    seg {:<2} adapter={:#x} {:<24} size=     n/a  flags=       n/a grp=n/a resident_last={:>8.1} MiB (min {:.1}, max {:.1}, n={})",
                        s.segment_id,
                        s.adapter,
                        "unknown (no ReportSegment)",
                        mib(s.resident_last),
                        mib(s.resident_min),
                        mib(s.resident_max),
                        s.resident_samples,
                    );
                }
            }
            if hidden > 0 {
                let _ = writeln!(
                    out,
                    "    ({hidden} inactive segment(s) with 0 samples hidden; pass --all-segments or --json to show)"
                );
            }
            if e.adapter_segment_resident_bytes > 0 {
                let _ = writeln!(
                    out,
                    "    Adapter-Wide Resident Total: {:.2} MiB (all processes on adapter; not PID-attributed)",
                    mib(e.adapter_segment_resident_bytes),
                );
            }
        }
        let alloc_scope = rep
            .pid
            .map(|p| format!("pid={p}"))
            .unwrap_or_else(|| "all pids".into());
        if e.coverage.has_allocation_rundown {
            let non_pt_count = e.allocations.iter().filter(|a| !a.page_table_or_dir).count();
            let pt_count = e.allocations.len().saturating_sub(non_pt_count);
            let alloc_count_str = if pt_count > 0 {
                format!("{non_pt_count} allocs (+{pt_count} page-table)")
            } else {
                format!("{non_pt_count} allocs")
            };
            let local_total = e
                .tracked_local_vram_bytes
                .saturating_add(e.tracked_local_bar_bytes);
            let local_str = if e.tracked_local_vram_bytes > 0 && e.tracked_local_bar_bytes > 0 {
                format!(
                    "Local VRAM {:.2} MiB (non-vis {:.2}, BAR {:.2})",
                    mib(local_total),
                    mib(e.tracked_local_vram_bytes),
                    mib(e.tracked_local_bar_bytes),
                )
            } else if e.tracked_local_bar_bytes > 0 {
                format!("Local VRAM {:.2} MiB (CPU-vis BAR)", mib(local_total))
            } else {
                format!("Local VRAM {:.2} MiB", mib(local_total))
            };
            let _ = writeln!(
                out,
                "  Tracked Live Allocs ({alloc_scope}) : {alloc_count_str} | {local_str} | NonLocal Host {:.2} MiB | Demoted {:.2} MiB | Evicted {:.2} MiB | Unknown {:.2} MiB",
                mib(e.tracked_non_local_host_bytes),
                mib(e.tracked_demoted_bytes),
                mib(e.tracked_evicted_bytes),
                mib(e.tracked_unknown_segment_bytes),
            );
        } else if e.truncated_by_max_size {
            let _ = writeln!(
                out,
                "  Tracked Live Allocs ({alloc_scope}) : unavailable (truncated before allocation rundown completed; raise --max-etl-mb)"
            );
        } else {
            let _ = writeln!(
                out,
                "  Tracked Live Allocs ({alloc_scope}) : unavailable (no allocation rundown/lifecycle events in trace; cannot identify per-allocation placement or which heap was demoted)"
            );
        }
        if let Some(pct_v) = rep.conservation.vidmm_accounted_pct {
            let _ = writeln!(
                out,
                "  VidMm Conservation    : {:.2}% ({:.2} / {:.2} MiB) [{}]",
                pct_v,
                mib(rep.conservation.vidmm_accounted_bytes),
                mib(rep.conservation.vidmm_reference_bytes),
                rep.conservation.vidmm_conservation_scope,
            );
        } else if let Some(ref reason) = rep.conservation.vidmm_conservation_unavailable_reason {
            let _ = writeln!(out, "  VidMm Conservation    : unavailable ({reason})");
        }
        if let Some(pid) = e.filter_pid
            && !e.coverage.has_allocation_rundown
        {
            let _ = writeln!(
                out,
                "  Paging / Faults (pid={pid}, thread-context only): MakeResident={} VidMmEvict={} PagingPkts={}",
                e.paging.make_resident_calls, e.paging.vidmm_evict_calls, e.paging.paging_queue_packets,
            );
            if let Some(ref u) = e.unattributed_paging {
                let _ = writeln!(
                    out,
                    "  Paging / Faults (adapter-wide, unattributed to pid={pid} without alloc rundown): PageIn={} Evict={} AllocFault={} Migrate={} ResetBacking={} ({} pages) ReserveRes={}",
                    u.page_in_allocations,
                    u.evict_allocations,
                    u.allocation_faults,
                    u.migrate_allocations,
                    u.reset_backing_store_calls,
                    u.reset_backing_store_pages,
                    u.reserve_resource_calls,
                );
            } else {
                let _ = writeln!(
                    out,
                    "  Paging / Faults (handle-keyed, pid={pid}): unavailable without allocation rundown"
                );
            }
        } else {
            let _ = writeln!(
                out,
                "  Paging / Faults       : PageIn={} Evict={} AllocFault={} MakeResident={} Migrate={} ResetBacking={} ({} pages) PagingPkts={} ReserveRes={}",
                e.paging.page_in_allocations,
                e.paging.evict_allocations,
                e.paging.allocation_faults,
                e.paging.make_resident_calls,
                e.paging.migrate_allocations,
                e.paging.reset_backing_store_calls,
                e.paging.reset_backing_store_pages,
                e.paging.paging_queue_packets,
                e.paging.reserve_resource_calls,
            );
        }
        if e.coverage.has_gpu_va_events || e.tile_mappings.call_count > 0 {
            let gpuva_part = if e.tile_mappings.gpuva_rundown_map_events > 0
                || e.tile_mappings.gpuva_live_map_events > 0
                || e.tile_mappings.gpuva_map_events == 0
            {
                format!(
                    "GPUVA live_map={} unmap={} (rundown_map={})",
                    e.tile_mappings.gpuva_live_map_events,
                    e.tile_mappings.gpuva_unmap_events,
                    e.tile_mappings.gpuva_rundown_map_events,
                )
            } else {
                format!(
                    "GPUVA map={} unmap={}",
                    e.tile_mappings.gpuva_map_events, e.tile_mappings.gpuva_unmap_events,
                )
            };
            let _ = writeln!(
                out,
                "  UpdateTileMappings    : {} calls ({} ops) | {gpuva_part} | live_mappings={}",
                e.tile_mappings.call_count,
                e.tile_mappings.total_operations,
                e.gpu_va_mappings.len(),
            );
        } else if e.truncated_by_max_size {
            let _ = writeln!(
                out,
                "  UpdateTileMappings    : unavailable (truncated before final rundown; no GPUVA / UpdateGpuVirtualAddress events captured)"
            );
        } else {
            let _ = writeln!(
                out,
                "  UpdateTileMappings    : unavailable (no GPUVA / UpdateGpuVirtualAddress events in trace)"
            );
        }
        if e.coverage.hard_faults_available {
            let _ = writeln!(
                out,
                "  Kernel HardFaults     : {} faults ({:.2} MiB)",
                e.hard_faults.total_count,
                mib(e.hard_faults.total_bytes),
            );
        } else {
            let _ = writeln!(
                out,
                "  Kernel HardFaults     : unavailable (no SystemMemoryProvider / PageFault provider events in trace)"
            );
        }
        if let Some(ref guidance) = e.coverage.capture_guidance {
            let _ = writeln!(out, "  Capture Guidance      : {guidance}");
        }

        if !e.allocations.is_empty() {
            // Size histogram per placement (count/MiB per size class).
            let mut hist: BTreeMap<&str, [(usize, u64); 7]> = BTreeMap::new();
            let mut created_in_trace = 0usize;
            let mut created_in_trace_bytes = 0u64;
            for a in &e.allocations {
                if a.page_table_or_dir {
                    continue;
                }
                if a.created_during_trace {
                    created_in_trace += 1;
                    created_in_trace_bytes += a.size;
                }
                let pl = alloc_placement_label(e, a);
                let b = size_bucket_index(a.size);
                let row = hist.entry(pl).or_insert([(0, 0); 7]);
                row[b].0 += 1;
                row[b].1 += a.size;
            }
            if !hist.is_empty() {
                let _ = writeln!(
                    out,
                    "\n  VidMm Live Allocation Size Histogram (count/MiB; created during trace: {} allocs, {:.2} MiB):",
                    created_in_trace,
                    mib(created_in_trace_bytes),
                );
                let _ = writeln!(
                    out,
                    "    {:<15} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
                    "placement",
                    SIZE_BUCKETS[0],
                    SIZE_BUCKETS[1],
                    SIZE_BUCKETS[2],
                    SIZE_BUCKETS[3],
                    SIZE_BUCKETS[4],
                    SIZE_BUCKETS[5],
                    SIZE_BUCKETS[6],
                );
                for (pl, row) in &hist {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|&(n, b)| {
                            if n == 0 {
                                "-".into()
                            } else {
                                format!("{n}/{:.0}M", mib(b))
                            }
                        })
                        .collect();
                    let _ = writeln!(
                        out,
                        "    {:<15} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12} {:>12}",
                        pl, cells[0], cells[1], cells[2], cells[3], cells[4], cells[5], cells[6],
                    );
                }
            }
        }

        if !e.allocations.is_empty() && top > 0 {
            let mut filtered: Vec<&VidMmAllocation> = e
                .allocations
                .iter()
                .filter(|a| {
                    !a.page_table_or_dir
                        && alloc_matches_placement(e, a, opts.placement)
                        && opts.min_bytes.is_none_or(|m| a.size >= m)
                        && opts.max_bytes.is_none_or(|m| a.size <= m)
                })
                .collect();
            match opts.sort {
                AllocSort::Size => filtered.sort_by_key(|a| std::cmp::Reverse(a.size)),
                AllocSort::Newest => filtered.sort_by(|x, y| {
                    y.created_during_trace
                        .cmp(&x.created_during_trace)
                        .then_with(|| {
                            y.first_seen_secs
                                .unwrap_or(-1.0)
                                .total_cmp(&x.first_seen_secs.unwrap_or(-1.0))
                        })
                        .then_with(|| y.size.cmp(&x.size))
                }),
                AllocSort::Oldest => filtered.sort_by(|x, y| {
                    x.first_seen_secs
                        .unwrap_or(f64::MAX)
                        .total_cmp(&y.first_seen_secs.unwrap_or(f64::MAX))
                        .then_with(|| y.size.cmp(&x.size))
                }),
            }
            let _ = writeln!(
                out,
                "\n  Top {} Live VidMm Allocations ({} matching filter, sorted by {}):",
                top.min(filtered.len()),
                filtered.len(),
                opts.sort.as_str(),
            );
            let _ = writeln!(
                out,
                "  {:>10} {:>4} {:>4} {:<15} {:>8} {:>10} {:>18} {:>18} owner",
                "size_MiB", "pref", "act", "placement", "t_s", "flags", "cpu_va", "gpu_va"
            );
            for a in filtered.iter().take(top) {
                let actual = a
                    .current_segment_id
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "-".into());
                let kind = alloc_placement_label(e, a);
                let ts = if a.created_during_trace {
                    a.first_seen_secs
                        .map(|t| format!("+{t:.2}s"))
                        .unwrap_or_else(|| "new".into())
                } else {
                    "rundown".into()
                };
                let cpu = a.cpu_va.map(|v| format!("{v:#x}")).unwrap_or_else(|| "-".into());
                let gpu = a
                    .gpu_va_start
                    .map(|v| format!("{v:#x}"))
                    .unwrap_or_else(|| "-".into());
                let owner = labels
                    .and_then(|l| {
                        a.cpu_va
                            .and_then(|va| l.owner_of(va))
                            .or_else(|| a.gpu_va_start.and_then(|va| l.owner_of(va)))
                    })
                    .unwrap_or("<unlabeled>");
                let _ = writeln!(
                    out,
                    "  {:>10.2} {:>4} {:>4} {:<15} {:>8} {:>#10x} {:>18} {:>18} {}",
                    mib(a.size),
                    a.preferred_segment_id,
                    actual,
                    kind,
                    ts,
                    a.flags,
                    cpu,
                    gpu,
                    owner,
                );
            }
        }
    }

    if !rep.alerts.is_empty() {
        let _ = writeln!(out, "\n== Placement / Fallback Alerts ({}) ==", rep.alerts.len());
        for a in &rep.alerts {
            let _ = writeln!(out, "  [!] {} ({:.2} MiB): {}", a.kind, mib(a.bytes), a.detail);
        }
    }

    if !rep.by_owner.is_empty() {
        let has_win32 = rep.win32.is_some();
        let has_vidmm_allocs = rep
            .etw
            .as_ref()
            .is_some_and(|e| e.coverage.has_allocation_rundown);
        let has_hard_faults = rep.etw.as_ref().is_some_and(|e| e.coverage.hard_faults_available);
        let limit = top.max(40);
        let owner_w = rep
            .by_owner
            .iter()
            .take(limit)
            .map(|o| o.owner.len())
            .max()
            .unwrap_or(22)
            .clamp(22, 48);

        let _ = writeln!(out, "\n== Breakdown by Owner (--labels) ==");
        if !has_win32 || !has_vidmm_allocs || !has_hard_faults {
            if rep.etw.is_none() && has_win32 {
                let _ = writeln!(
                    out,
                    "  (vram_M/host_M/demot_M/hfault = n/a: needs a live trace or --etl; run `locum-cli mem trace`)"
                );
            } else {
                let mut missing = Vec::new();
                if !has_win32 {
                    missing.push("commit_M..untch_M (no Win32 process snapshot)");
                }
                if !has_vidmm_allocs {
                    missing.push("vram_M/host_M/demot_M (no DxgKrnl allocation rundown)");
                }
                if !has_hard_faults {
                    missing.push("hfault (no SystemMemoryProvider / PageFault events)");
                }
                let _ = writeln!(out, "  (n/a columns: {})", missing.join("; "));
            }
        }
        let _ = writeln!(
            out,
            "  {:<owner_w$} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>6}",
            "owner",
            "commit_M",
            "ram_M",
            "bar_M",
            "privpg_M",
            "mapnws_M",
            "untch_M",
            "vram_M",
            "host_M",
            "demot_M",
            "hfault",
            owner_w = owner_w,
        );
        for o in rep.by_owner.iter().take(limit) {
            let (commit_s, ram_s, bar_s, privpg_s, mapnws_s, untch_s) = if has_win32 {
                (
                    format!("{:>8.2}", mib(o.va_commit_bytes)),
                    format!("{:>8.2}", mib(o.resident_bytes)),
                    format!("{:>8.2}", mib(o.bar_mapped_bytes)),
                    format!("{:>8.2}", mib(o.paged_out_private_bytes)),
                    format!("{:>8.2}", mib(o.non_ws_mapped_bytes)),
                    format!("{:>8.2}", mib(o.untouched_bytes)),
                )
            } else {
                (
                    "     n/a".into(),
                    "     n/a".into(),
                    "     n/a".into(),
                    "     n/a".into(),
                    "     n/a".into(),
                    "     n/a".into(),
                )
            };
            let (vram_s, host_s, demot_s) = if has_vidmm_allocs {
                (
                    format!("{:>8.2}", mib(o.vidmm_local_bytes + o.vidmm_local_bar_bytes)),
                    format!("{:>8.2}", mib(o.vidmm_non_local_bytes)),
                    format!("{:>8.2}", mib(o.vidmm_demoted_bytes)),
                )
            } else {
                ("     n/a".into(), "     n/a".into(), "     n/a".into())
            };
            let hfault_s = if has_hard_faults {
                format!("{:>6}", o.hard_fault_count)
            } else {
                "   n/a".into()
            };
            let _ = writeln!(
                out,
                "  {:<owner_w$} {} {} {} {} {} {} {} {} {} {}",
                o.owner,
                commit_s,
                ram_s,
                bar_s,
                privpg_s,
                mapnws_s,
                untch_s,
                vram_s,
                host_s,
                demot_s,
                hfault_s,
                owner_w = owner_w,
            );
        }
    }

    if let Some(ref c) = rep.capture {
        let _ = writeln!(
            out,
            "\n== Capture Join ({}, {} descriptor rows, {} with GPU VA, {} flagged) ==",
            c.capture_path, c.total_descriptor_rows, c.rows_with_gpu_address, c.flagged_rows
        );
        for o in c.by_owner.iter().take(top.max(20)) {
            let _ = writeln!(
                out,
                "  owner={:<24} rows={:<8} draws={:<6} resources={:<6} flagged={}",
                o.owner, o.descriptor_rows, o.distinct_draws, o.distinct_resources, o.flagged_rows
            );
        }
        for p in &c.by_vidmm_placement {
            let _ = writeln!(
                out,
                "  vidmm={:<24} rows={:<8} draws={:<6} resources={}",
                p.placement, p.descriptor_rows, p.distinct_draws, p.distinct_resources
            );
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::etw::{PagingActivity, SegmentInfo, TraceCoverage};

    #[test]
    fn missing_rundown_and_sysmem_marks_conservation_and_hard_faults_unavailable() {
        let mut etw = EtwReport {
            etl_path: "TDp/dxgkrnl.etl".into(),
            filter_pid: Some(22328),
            total_events: 4_028_070,
            dxgkrnl_events: 4_028_068,
            system_memory_events: 0,
            duration_secs: 30.05,
            coverage: TraceCoverage {
                segment_residency_events: 3852,
                unattributed_handle_paging_events: 11_711,
                ..Default::default()
            },
            segments: vec![
                SegmentInfo {
                    adapter: 0xffff_d58f_9bf6_0000,
                    segment_id: 2,
                    described: false,
                    size: 0,
                    commit_limit: 0,
                    flags: 0,
                    memory_segment_group: 0,
                    aperture: false,
                    cpu_visible: false,
                    populated_from_system_memory: false,
                    kind: SegmentKind::Unknown,
                    resident_last: 16_712_294_400,
                    resident_min: 16_691_892_224,
                    resident_max: 16_759_136_256,
                    resident_samples: 3126,
                },
                SegmentInfo {
                    adapter: 0xffff_d58f_9bf6_0000,
                    segment_id: 3,
                    described: false,
                    size: 0,
                    commit_limit: 0,
                    flags: 0,
                    memory_segment_group: 0,
                    aperture: false,
                    cpu_visible: false,
                    populated_from_system_memory: false,
                    kind: SegmentKind::Unknown,
                    resident_last: 8_209_506_304,
                    resident_min: 7_584_358_400,
                    resident_max: 8_209_506_304,
                    resident_samples: 726,
                },
            ],
            adapter_segment_resident_bytes: 16_712_294_400 + 8_209_506_304,
            paging: PagingActivity {
                make_resident_calls: 864,
                paging_queue_packets: 4352,
                ..Default::default()
            },
            unattributed_paging: Some(PagingActivity {
                page_in_allocations: 3642,
                evict_allocations: 974,
                allocation_faults: 3614,
                reset_backing_store_calls: 187,
                reset_backing_store_pages: 7425,
                reserve_resource_calls: 3294,
                ..Default::default()
            }),
            ..Default::default()
        };
        etw.refresh_coverage_diagnostics();

        assert!(!etw.coverage.has_segment_descriptions);
        assert!(!etw.coverage.has_allocation_rundown);
        assert!(!etw.coverage.has_process_counters);
        assert!(!etw.coverage.hard_faults_available);
        assert!(!etw.coverage.can_attribute_heap_demotion);
        assert!(etw.warnings.len() >= 4);
        assert!(
            etw.coverage
                .capture_guidance
                .as_deref()
                .is_some_and(|g| g.contains("0x001000c5") && g.contains("locum-cli mem trace --pid 22328"))
        );

        let rep = reconcile(None, Some(etw), &LabelMap::default(), None);
        assert_eq!(rep.conservation.vidmm_accounted_pct, None);
        assert_eq!(rep.conservation.vidmm_conservation_scope, "unavailable");
        assert_eq!(
            rep.conservation.adapter_segment_resident_bytes,
            16_712_294_400 + 8_209_506_304
        );
        assert!(rep.conservation.vidmm_conservation_unavailable_reason.is_some());

        let text = format_text(&rep, 10);
        assert!(text.contains("Adapter-Wide Segments (all processes on adapter; NOT filtered to pid=22328)"));
        assert!(text.contains("unknown (no ReportSegment)"));
        assert!(text.contains("Tracked Live Allocs (pid=22328) : unavailable"));
        assert!(text.contains("VidMm Conservation    : unavailable"));
        assert!(text.contains("Kernel HardFaults     : unavailable"));
        assert!(text.contains("Capture Guidance      :"));
        assert!(text.contains("PageIn=3642"));
        assert!(!text.contains("100.00%"));
    }

    #[test]
    fn live_session_with_hard_fault_provider_enabled_reports_zero_faults_as_available() {
        let mut etw = EtwReport {
            dxgkrnl_events: 100,
            system_memory_events: 0,
            coverage: TraceCoverage {
                segment_descriptor_events: 3,
                allocation_rundown_events: 10,
                process_counter_events: 4,
                gpu_va_events: 5,
                ..Default::default()
            },
            ..Default::default()
        };
        etw.apply_live_session_metadata(true, Vec::new());
        assert!(etw.coverage.hard_faults_available);
        assert!(etw.coverage.can_attribute_heap_demotion);
        assert!(etw.warnings.is_empty());
        assert!(etw.coverage.capture_guidance.is_none());
    }

    #[test]
    fn owner_breakdown_prints_na_without_etw() {
        let labels = LabelMap::parse("[0x1000..0x3000] -> outer_res\n0x2000\t0x2800\tinner_win\n").unwrap();
        let snap = ProcessMemorySnapshot {
            pid: 99,
            page_size: 4096,
            sample_step: 1,
            thread_suspends: 0,
            elapsed_ms: 1.0,
            process_exited: false,
            exit_code: None,
            video_memory_error: None,
            counters: Default::default(),
            totals: Default::default(),
            video_memory: Vec::new(),
            regions: Vec::new(),
        };
        let rep = reconcile(Some(snap), None, &labels, None);
        let text = format_text(&rep, 25);
        assert!(text.contains("vram_M/host_M/demot_M/hfault = n/a: needs a live trace or --etl"));
        assert!(text.contains("outer_res"));
        assert!(text.contains("inner_win"));
        assert!(text.contains("     n/a      n/a      n/a    n/a"));

        // Without Win32 snapshot (e.g. offline `mem etl`), CPU columns also print `n/a`.
        let rep_no_win32 = reconcile(None, None, &labels, None);
        let text_no_win32 = format_text(&rep_no_win32, 25);
        assert!(text_no_win32.contains("commit_M..untch_M (no Win32 process snapshot)"));
    }

    #[test]
    fn terminating_snapshot_emits_warnings_and_d3dkmt_unavailable() {
        let snap = ProcessMemorySnapshot {
            pid: 21960,
            page_size: 4096,
            sample_step: 16,
            thread_suspends: 0,
            elapsed_ms: 500.0,
            process_exited: false,
            exit_code: None,
            video_memory_error: Some(
                "D3DKMTQueryVideoMemoryInfo failed: STATUS_PROCESS_IS_TERMINATING (0xc000010a)".into(),
            ),
            counters: Default::default(),
            totals: crate::win32::VaTotals {
                commit_bytes: 22 << 30,
                image_commit_bytes: 680 * 1024,
                ..Default::default()
            },
            video_memory: Vec::new(),
            regions: Vec::new(),
        };
        let rep = reconcile(Some(snap), None, &LabelMap::default(), None);
        assert!(
            rep.warnings
                .iter()
                .any(|w| w.contains("STATUS_PROCESS_IS_TERMINATING"))
        );
        assert!(
            rep.warnings
                .iter()
                .any(|w| w.contains("Very low MEM_IMAGE commit"))
        );
        let text = format_text(&rep, 10);
        assert!(text.contains("== Out-of-Process QueryVideoMemoryInfo (D3DKMT) =="));
        assert!(
            text.contains("unavailable (D3DKMTQueryVideoMemoryInfo failed: STATUS_PROCESS_IS_TERMINATING")
        );
    }

    #[test]
    fn rebar_local_vram_gpuva_split_and_etl_rate_render_clearly() {
        let etw = EtwReport {
            etl_path: "trace2.etl".into(),
            etl_size_bytes: 160 << 20,
            filter_pid: Some(23916),
            total_events: 1_000_000,
            dxgkrnl_events: 999_998,
            rundown_events: 600_000,
            system_memory_events: 0,
            duration_secs: 5.0,
            coverage: TraceCoverage {
                has_segment_descriptions: true,
                has_allocation_rundown: true,
                has_gpu_va_events: true,
                hard_faults_available: true,
                ..Default::default()
            },
            allocations: vec![
                VidMmAllocation {
                    global_handle: 1,
                    adapter: 1,
                    pid: 23916,
                    device: 1,
                    size: 1024 << 20,
                    flags: 0,
                    preferred_segment_raw: 2,
                    preferred_segment_id: 2,
                    read_segment_set: 0,
                    write_segment_set: 0,
                    current_segment_id: Some(2),
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
                    first_seen_secs: Some(0.0),
                    created_during_trace: false,
                },
                VidMmAllocation {
                    global_handle: 2,
                    adapter: 1,
                    pid: 23916,
                    device: 1,
                    size: 4096,
                    flags: 0,
                    preferred_segment_raw: 2,
                    preferred_segment_id: 2,
                    read_segment_set: 0,
                    write_segment_set: 0,
                    current_segment_id: Some(2),
                    demoted: false,
                    evicted: false,
                    page_table_or_dir: true,
                    backing_store_pinned: false,
                    cpu_va: None,
                    gpu_va_start: None,
                    gpu_va_end: None,
                    page_in_count: 0,
                    evict_count: 0,
                    fault_count: 0,
                    alive: true,
                    first_seen_secs: Some(0.0),
                    created_during_trace: false,
                },
            ],
            tracked_local_bar_bytes: 1024 << 20,
            tracked_total_bytes: 1024 << 20,
            tile_mappings: crate::etw::TileMappingSummary {
                call_count: 10,
                total_operations: 12,
                by_pid: BTreeMap::new(),
                gpuva_map_events: 100_050,
                gpuva_rundown_map_events: 100_000,
                gpuva_live_map_events: 50,
                gpuva_unmap_events: 20,
            },
            ..Default::default()
        };
        let rep = reconcile(None, Some(etw), &LabelMap::default(), None);
        let text = format_text(&rep, 5);
        assert!(text.contains("200.0k/s, 160.0 MiB (32.0 MiB/s), rundown=600000, steady=400000"));
        assert!(text.contains("1 allocs (+1 page-table) | Local VRAM 1024.00 MiB (CPU-vis BAR)"));
        assert!(text.contains("GPUVA live_map=50 unmap=20 (rundown_map=100000)"));

        // Truncated trace missing GPUVA rundown says so explicitly.
        let trunc_etw = EtwReport {
            etl_path: "cap128.etl".into(),
            etl_size_bytes: 128 << 20,
            filter_pid: Some(2820),
            total_events: 269_630,
            dxgkrnl_events: 269_628,
            rundown_events: 144_047,
            truncated_by_max_size: true,
            duration_secs: 3.2,
            coverage: TraceCoverage {
                has_allocation_rundown: true,
                has_gpu_va_events: false,
                hard_faults_available: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let trunc_rep = reconcile(None, Some(trunc_etw), &LabelMap::default(), None);
        let trunc_text = format_text(&trunc_rep, 5);
        assert!(trunc_text.contains("TRUNCATED"));
        assert!(trunc_text.contains(
            "UpdateTileMappings    : unavailable (truncated before final rundown; no GPUVA / UpdateGpuVirtualAddress events captured)"
        ));
    }
}
