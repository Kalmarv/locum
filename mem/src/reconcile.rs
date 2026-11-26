//! Reconciliation, conservation accounting, silent host-fallback detection, and report formatting.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::THREAD_SUSPENDS;
use crate::capture_join::CaptureJoinReport;
use crate::etw::{EtwReport, SegmentKind};
use crate::labels::LabelMap;
use crate::win32::{MemoryType, ProcessMemorySnapshot};

/// Alert raised when `QueryVideoMemoryInfo` or requested heap placement disagrees with true OS/VidMm placement.
#[derive(Clone, Debug, Serialize)]
pub struct PlacementAlert {
    pub kind: String,
    pub owner: Option<String>,
    pub bytes: u64,
    pub detail: String,
}

/// Per-owner summary built from `--labels <ranges.tsv>` across Win32 VA, HardFaults, VidMm ETW, and Capture.
#[derive(Clone, Debug, Default, Serialize)]
pub struct OwnerBreakdown {
    pub owner: String,
    pub va_commit_bytes: u64,
    pub va_reserve_bytes: u64,
    pub resident_bytes: u64,
    pub resident_private_bytes: u64,
    pub resident_shared_bytes: u64,
    pub bar_mapped_bytes: u64,
    pub paged_out_bytes: u64,
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
#[derive(Clone, Debug, Default, Serialize)]
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
    pub vidmm_accounted_bytes: u64,
    pub vidmm_reference_bytes: u64,
    pub vidmm_accounted_pct: Option<f64>,
}

/// Unified ground-truth memory report combining Win32 inspection, ETW VidMm/HardFaults, labels, and capture join.
#[derive(Clone, Debug, Serialize)]
pub struct MemReport {
    pub pid: Option<u32>,
    pub thread_suspends: u64,
    pub conservation: ConservationSummary,
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
    // Compare tracked live allocations (Local + BAR + NonLocal + Unknown) against reference
    // (`QVMI Local + NonLocal`, or ETW `VidMmProcessCommitmentChange` / `TotalBytesResidentInSegment`).
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
        let seg_resident_total: u64 = e.segments.iter().map(|s| s.resident_last).sum();

        if cons.qvmi_total_bytes > 0 {
            cons.vidmm_reference_bytes = cons.qvmi_total_bytes;
            cons.vidmm_accounted_bytes = if etw_tracked > 0 {
                etw_tracked
            } else if etw_proc_total > 0 {
                etw_proc_total
            } else {
                0
            };
            cons.vidmm_accounted_pct = Some(pct(cons.vidmm_accounted_bytes, cons.vidmm_reference_bytes));
        } else if etw_proc_total > 0 {
            cons.vidmm_reference_bytes = etw_proc_total;
            cons.vidmm_accounted_bytes = if etw_tracked > 0 {
                etw_tracked
            } else {
                etw_proc_total
            };
            cons.vidmm_accounted_pct = Some(pct(cons.vidmm_accounted_bytes, cons.vidmm_reference_bytes));
        } else if seg_resident_total > 0 {
            // Existing ETL recorded without Resource/DC_Start (e.g. dxgetl.ps1 with Base|Memory):
            // TotalBytesResidentInSegment directly accounts for adapter-wide segment residency.
            cons.vidmm_reference_bytes = seg_resident_total;
            cons.vidmm_accounted_bytes = seg_resident_total;
            cons.vidmm_accounted_pct = Some(100.0);
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

    MemReport {
        pid,
        thread_suspends: THREAD_SUSPENDS,
        conservation: cons,
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
    let mut out = String::new();
    use std::fmt::Write;

    let pid_str = rep.pid.map(|p| p.to_string()).unwrap_or_else(|| "all".into());
    let _ = writeln!(
        out,
        "locum-mem ground-truth report | pid={pid_str} | thread_suspends={}",
        rep.thread_suspends
    );

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
        let _ = writeln!(
            out,
            "  VA Split              : RAM resident {:>9.2} MiB (priv {:.2}, shared {:.2}) | BAR/IO {:>8.2} MiB | paged-out {:>8.2} MiB | untouched {:>9.2} MiB",
            mib(w.totals.resident_bytes),
            mib(w.totals.resident_private_bytes),
            mib(w.totals.resident_shared_bytes),
            mib(w.totals.bar_mapped_bytes),
            mib(w.totals.paged_out_bytes),
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

        if !w.video_memory.is_empty() {
            let _ = writeln!(out, "\n== Out-of-Process QueryVideoMemoryInfo (D3DKMT) ==");
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
        }
    }

    if let Some(ref e) = rep.etw {
        let _ = writeln!(
            out,
            "\n== VidMm / DxgKrnl + Kernel ETW ({} events, {} dxgkrnl, {} sysmem, {:.2} s) ==",
            e.total_events, e.dxgkrnl_events, e.system_memory_events, e.duration_secs
        );
        for w in &e.warnings {
            let _ = writeln!(out, "  [warn] {w}");
        }
        if !e.segments.is_empty() {
            let _ = writeln!(out, "  Segments:");
            for s in &e.segments {
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
            }
        }
        let _ = writeln!(
            out,
            "  Tracked Live Allocs   : {} allocs | Local VRAM {:.2} MiB | Local BAR {:.2} MiB | NonLocal Host {:.2} MiB | Demoted {:.2} MiB | Evicted {:.2} MiB | Unknown {:.2} MiB",
            e.allocations.len(),
            mib(e.tracked_local_vram_bytes),
            mib(e.tracked_local_bar_bytes),
            mib(e.tracked_non_local_host_bytes),
            mib(e.tracked_demoted_bytes),
            mib(e.tracked_evicted_bytes),
            mib(e.tracked_unknown_segment_bytes),
        );
        if let Some(pct_v) = rep.conservation.vidmm_accounted_pct {
            let _ = writeln!(
                out,
                "  VidMm Conservation    : {:.2}% ({:.2} / {:.2} MiB)",
                pct_v,
                mib(rep.conservation.vidmm_accounted_bytes),
                mib(rep.conservation.vidmm_reference_bytes),
            );
        }
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
        let _ = writeln!(
            out,
            "  UpdateTileMappings    : {} calls ({} ops) | GPUVA map={} unmap={} | live_mappings={}",
            e.tile_mappings.call_count,
            e.tile_mappings.total_operations,
            e.tile_mappings.gpuva_map_events,
            e.tile_mappings.gpuva_unmap_events,
            e.gpu_va_mappings.len(),
        );
        let _ = writeln!(
            out,
            "  Kernel HardFaults     : {} faults ({:.2} MiB)",
            e.hard_faults.total_count,
            mib(e.hard_faults.total_bytes),
        );

        if !e.allocations.is_empty() && top > 0 {
            let _ = writeln!(
                out,
                "\n  Top {} Live VidMm Allocations:",
                top.min(e.allocations.len())
            );
            let _ = writeln!(
                out,
                "  {:>10} {:>6} {:>6} {:>15} {:>7} {:>18} {:>18}",
                "size_MiB", "pref", "actual", "placement", "demoted", "cpu_va", "gpu_va"
            );
            for a in e.allocations.iter().take(top) {
                let actual = a
                    .current_segment_id
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "-".into());
                let kind = a
                    .current_segment_id
                    .map(|s| e.segment_kind(a.adapter, s).as_str())
                    .unwrap_or("unknown");
                let cpu = a.cpu_va.map(|v| format!("{v:#x}")).unwrap_or_else(|| "-".into());
                let gpu = a
                    .gpu_va_start
                    .map(|v| format!("{v:#x}"))
                    .unwrap_or_else(|| "-".into());
                let _ = writeln!(
                    out,
                    "  {:>10.2} {:>6} {:>6} {:>15} {:>7} {:>18} {:>18}",
                    mib(a.size),
                    a.preferred_segment_id,
                    actual,
                    kind,
                    if a.demoted { "YES" } else { "no" },
                    cpu,
                    gpu,
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
        let _ = writeln!(out, "\n== Breakdown by Owner (--labels) ==");
        let _ = writeln!(
            out,
            "  {:<22} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>8} {:>6}",
            "owner",
            "commit_M",
            "ram_M",
            "bar_M",
            "paged_M",
            "untch_M",
            "vram_M",
            "host_M",
            "demot_M",
            "hfault"
        );
        for o in rep.by_owner.iter().take(top.max(40)) {
            let _ = writeln!(
                out,
                "  {:<22} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>8.2} {:>6}",
                o.owner,
                mib(o.va_commit_bytes),
                mib(o.resident_bytes),
                mib(o.bar_mapped_bytes),
                mib(o.paged_out_bytes),
                mib(o.untouched_bytes),
                mib(o.vidmm_local_bytes + o.vidmm_local_bar_bytes),
                mib(o.vidmm_non_local_bytes),
                mib(o.vidmm_demoted_bytes),
                o.hard_fault_count,
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
