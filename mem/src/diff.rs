//! Diff two `MemReport` JSON snapshots or `.etl` traces to pinpoint newly leaked/grown
//! VidMm allocations and Win32 virtual-memory regions grouped by placement, owner, and size class.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::Serialize;

use crate::etw::{EtwReport, SegmentKind, VidMmAllocation, parse_etl};
use crate::labels::LabelMap;
use crate::reconcile::{
    AllocSort, FormatOptions, MemReport, SIZE_BUCKETS, alloc_placement_label, mib, reconcile,
    size_bucket_index,
};
use crate::win32::MemoryType;

/// Grouped count + total bytes for newly created allocations or Win32 VA regions.
#[derive(Clone, Debug, Serialize)]
pub struct NewAllocGroup {
    pub placement: String,
    pub owner: String,
    pub size_class: String,
    pub count: usize,
    pub total_bytes: u64,
}

/// One newly created VidMm allocation present in B but not in A.
#[derive(Clone, Debug, Serialize)]
pub struct NewVidMmAlloc {
    pub global_handle: u64,
    pub size: u64,
    pub preferred_segment_id: u32,
    pub current_segment_id: Option<u32>,
    pub placement: String,
    pub demoted: bool,
    pub flags: u32,
    pub first_seen_secs: Option<f64>,
    pub created_during_trace: bool,
    pub cpu_va: Option<u64>,
    pub gpu_va_start: Option<u64>,
    pub owner: String,
}

/// One newly committed Win32 VA allocation present in B but not in A.
#[derive(Clone, Debug, Serialize)]
pub struct NewVaAlloc {
    pub allocation_base: u64,
    pub commit_bytes: u64,
    pub resident_bytes: u64,
    pub paged_out_private_bytes: u64,
    pub non_ws_mapped_bytes: u64,
    pub untouched_bytes: u64,
    pub mem_type: String,
    pub write_combined: bool,
    pub owner: String,
}

/// Per-owner delta between report A and report B.
#[derive(Clone, Debug, Serialize)]
pub struct OwnerDelta {
    pub owner: String,
    pub commit_delta: i64,
    pub ram_delta: i64,
    pub paged_private_delta: i64,
    pub non_ws_mapped_delta: i64,
    pub vram_delta: i64,
    pub host_delta: i64,
    pub demoted_delta: i64,
}

/// Win32 CPU + QVMI delta summary.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Win32DiffSummary {
    pub private_usage_a: u64,
    pub private_usage_b: u64,
    pub private_usage_delta: i64,
    pub working_set_a: u64,
    pub working_set_b: u64,
    pub working_set_delta: i64,
    pub commit_a: u64,
    pub commit_b: u64,
    pub commit_delta: i64,
    pub qvmi_local_a: u64,
    pub qvmi_local_b: u64,
    pub qvmi_local_delta: i64,
    pub qvmi_non_local_a: u64,
    pub qvmi_non_local_b: u64,
    pub qvmi_non_local_delta: i64,
    pub new_va_alloc_count: usize,
    pub new_va_commit_bytes: u64,
    pub freed_va_alloc_count: usize,
    pub freed_va_commit_bytes: u64,
    pub new_va_groups: Vec<NewAllocGroup>,
    pub new_va_allocs: Vec<NewVaAlloc>,
}

/// VidMm ETW allocation delta summary.
#[derive(Clone, Debug, Default, Serialize)]
pub struct VidMmDiffSummary {
    pub allocs_a: usize,
    pub allocs_b: usize,
    pub total_bytes_a: u64,
    pub total_bytes_b: u64,
    pub local_bytes_delta: i64,
    pub non_local_bytes_delta: i64,
    pub demoted_bytes_delta: i64,
    pub new_count: usize,
    pub new_bytes: u64,
    pub freed_count: usize,
    pub freed_bytes: u64,
    pub persisted_count: usize,
    pub placement_changed_count: usize,
    pub new_groups: Vec<NewAllocGroup>,
    pub new_allocs: Vec<NewVidMmAlloc>,
}

/// Full diff report between snapshot/trace A and snapshot/trace B.
#[derive(Clone, Debug, Serialize)]
pub struct DiffReport {
    pub source_a: String,
    pub source_b: String,
    pub pid_a: Option<u32>,
    pub pid_b: Option<u32>,
    pub win32: Option<Win32DiffSummary>,
    pub vidmm: Option<VidMmDiffSummary>,
    pub by_owner: Vec<OwnerDelta>,
}

/// Loads either a `MemReport` JSON file or an `.etl` trace file (parsed + reconciled with optional `pid` and `labels`).
pub fn load_report_or_etl(path: &Path, pid: Option<u32>, labels: &LabelMap) -> Result<MemReport, String> {
    let is_etl = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("etl"));
    if is_etl {
        let etw = parse_etl(path, pid)?;
        Ok(reconcile(None, Some(etw), labels, None))
    } else {
        let text =
            std::fs::read_to_string(path).map_err(|e| format!("reading report {}: {e}", path.display()))?;
        let mut rep: MemReport = serde_json::from_str(&text)
            .map_err(|e| format!("parsing MemReport JSON {}: {e}", path.display()))?;
        if let Some(ref mut e) = rep.etw
            && e.warnings.is_empty()
            && !e.coverage.has_allocation_rundown
            && !e.allocations.is_empty()
        {
            // Legacy JSON written before `coverage` existed: infer coverage from present records.
            e.coverage.has_allocation_rundown = true;
            e.coverage.has_segment_descriptions = e.segments.iter().any(|s| s.size > 0);
            e.coverage.hard_faults_available = e.system_memory_events > 0 || e.hard_faults.total_count > 0;
        }
        Ok(rep)
    }
}

/// Computes the diff from report `a` to report `b` (applying optional `labels` to classify new allocations).
pub fn diff_reports(
    source_a: &str,
    a: &MemReport,
    source_b: &str,
    b: &MemReport,
    labels: &LabelMap,
) -> DiffReport {
    let win32 = match (&a.win32, &b.win32) {
        (Some(wa), Some(wb)) => Some(diff_win32(wa, wb, labels)),
        _ => None,
    };
    let vidmm = match (&a.etw, &b.etw) {
        (Some(ea), Some(eb)) => Some(diff_vidmm(ea, eb, labels)),
        _ => None,
    };

    // Owner deltas across both reports.
    let map_a: HashMap<&str, _> = a.by_owner.iter().map(|o| (o.owner.as_str(), o)).collect();
    let map_b: HashMap<&str, _> = b.by_owner.iter().map(|o| (o.owner.as_str(), o)).collect();
    let mut all_owners: BTreeMap<&str, ()> = BTreeMap::new();
    for k in map_a.keys().chain(map_b.keys()) {
        all_owners.insert(k, ());
    }
    let mut by_owner = Vec::new();
    for owner in all_owners.into_keys() {
        let oa = map_a.get(owner);
        let ob = map_b.get(owner);
        let d_commit = sub_i64(
            ob.map(|o| o.va_commit_bytes).unwrap_or(0),
            oa.map(|o| o.va_commit_bytes).unwrap_or(0),
        );
        let d_ram = sub_i64(
            ob.map(|o| o.resident_bytes).unwrap_or(0),
            oa.map(|o| o.resident_bytes).unwrap_or(0),
        );
        let d_ppg = sub_i64(
            ob.map(|o| o.paged_out_private_bytes).unwrap_or(0),
            oa.map(|o| o.paged_out_private_bytes).unwrap_or(0),
        );
        let d_mnws = sub_i64(
            ob.map(|o| o.non_ws_mapped_bytes).unwrap_or(0),
            oa.map(|o| o.non_ws_mapped_bytes).unwrap_or(0),
        );
        let d_vram = sub_i64(
            ob.map(|o| o.vidmm_local_bytes + o.vidmm_local_bar_bytes)
                .unwrap_or(0),
            oa.map(|o| o.vidmm_local_bytes + o.vidmm_local_bar_bytes)
                .unwrap_or(0),
        );
        let d_host = sub_i64(
            ob.map(|o| o.vidmm_non_local_bytes).unwrap_or(0),
            oa.map(|o| o.vidmm_non_local_bytes).unwrap_or(0),
        );
        let d_dem = sub_i64(
            ob.map(|o| o.vidmm_demoted_bytes).unwrap_or(0),
            oa.map(|o| o.vidmm_demoted_bytes).unwrap_or(0),
        );
        if d_commit != 0 || d_ram != 0 || d_vram != 0 || d_host != 0 || d_dem != 0 {
            by_owner.push(OwnerDelta {
                owner: owner.to_string(),
                commit_delta: d_commit,
                ram_delta: d_ram,
                paged_private_delta: d_ppg,
                non_ws_mapped_delta: d_mnws,
                vram_delta: d_vram,
                host_delta: d_host,
                demoted_delta: d_dem,
            });
        }
    }
    by_owner.sort_by(|x, y| {
        let kx = x
            .host_delta
            .abs()
            .max(x.vram_delta.abs())
            .max(x.commit_delta.abs());
        let ky = y
            .host_delta
            .abs()
            .max(y.vram_delta.abs())
            .max(y.commit_delta.abs());
        ky.cmp(&kx).then_with(|| x.owner.cmp(&y.owner))
    });

    DiffReport {
        source_a: source_a.to_string(),
        source_b: source_b.to_string(),
        pid_a: a.pid,
        pid_b: b.pid,
        win32,
        vidmm,
        by_owner,
    }
}

fn diff_vidmm(ea: &EtwReport, eb: &EtwReport, labels: &LabelMap) -> VidMmDiffSummary {
    let allocs_a: HashMap<u64, &VidMmAllocation> = ea
        .allocations
        .iter()
        .filter(|a| !a.page_table_or_dir)
        .map(|a| (a.global_handle, a))
        .collect();
    let allocs_b: HashMap<u64, &VidMmAllocation> = eb
        .allocations
        .iter()
        .filter(|a| !a.page_table_or_dir)
        .map(|a| (a.global_handle, a))
        .collect();

    let mut new_allocs = Vec::new();
    let mut new_bytes = 0u64;
    let mut persisted_count = 0usize;
    let mut placement_changed_count = 0usize;
    let mut groups: BTreeMap<(String, String, usize), (usize, u64)> = BTreeMap::new();

    for (&gh, &ab) in &allocs_b {
        if let Some(&aa) = allocs_a.get(&gh) {
            persisted_count += 1;
            let ka = aa
                .current_segment_id
                .map(|s| ea.segment_kind(aa.adapter, s))
                .unwrap_or(SegmentKind::Unknown);
            let kb = ab
                .current_segment_id
                .map(|s| eb.segment_kind(ab.adapter, s))
                .unwrap_or(SegmentKind::Unknown);
            if ka != kb || aa.demoted != ab.demoted {
                placement_changed_count += 1;
            }
        } else {
            let placement = alloc_placement_label(eb, ab).to_string();
            let owner = ab
                .cpu_va
                .and_then(|va| labels.owner_of(va))
                .or_else(|| ab.gpu_va_start.and_then(|va| labels.owner_of(va)))
                .unwrap_or("<unlabeled>")
                .to_string();
            let bucket = size_bucket_index(ab.size);
            let g = groups
                .entry((placement.clone(), owner.clone(), bucket))
                .or_insert((0, 0));
            g.0 += 1;
            g.1 = g.1.saturating_add(ab.size);
            new_bytes = new_bytes.saturating_add(ab.size);
            new_allocs.push(NewVidMmAlloc {
                global_handle: gh,
                size: ab.size,
                preferred_segment_id: ab.preferred_segment_id,
                current_segment_id: ab.current_segment_id,
                placement,
                demoted: ab.demoted,
                flags: ab.flags,
                first_seen_secs: ab.first_seen_secs,
                created_during_trace: ab.created_during_trace,
                cpu_va: ab.cpu_va,
                gpu_va_start: ab.gpu_va_start,
                owner,
            });
        }
    }

    let mut freed_count = 0usize;
    let mut freed_bytes = 0u64;
    for (&gh, &aa) in &allocs_a {
        if !allocs_b.contains_key(&gh) {
            freed_count += 1;
            freed_bytes = freed_bytes.saturating_add(aa.size);
        }
    }

    let mut new_groups: Vec<NewAllocGroup> = groups
        .into_iter()
        .map(
            |((placement, owner, bucket), (count, total_bytes))| NewAllocGroup {
                placement,
                owner,
                size_class: SIZE_BUCKETS[bucket].to_string(),
                count,
                total_bytes,
            },
        )
        .collect();
    new_groups.sort_by(|x, y| {
        y.total_bytes
            .cmp(&x.total_bytes)
            .then_with(|| y.count.cmp(&x.count))
    });
    new_allocs.sort_by_key(|a| std::cmp::Reverse(a.size));

    VidMmDiffSummary {
        allocs_a: allocs_a.len(),
        allocs_b: allocs_b.len(),
        total_bytes_a: ea.tracked_total_bytes,
        total_bytes_b: eb.tracked_total_bytes,
        local_bytes_delta: sub_i64(
            eb.tracked_local_vram_bytes + eb.tracked_local_bar_bytes,
            ea.tracked_local_vram_bytes + ea.tracked_local_bar_bytes,
        ),
        non_local_bytes_delta: sub_i64(eb.tracked_non_local_host_bytes, ea.tracked_non_local_host_bytes),
        demoted_bytes_delta: sub_i64(eb.tracked_demoted_bytes, ea.tracked_demoted_bytes),
        new_count: new_allocs.len(),
        new_bytes,
        freed_count,
        freed_bytes,
        persisted_count,
        placement_changed_count,
        new_groups,
        new_allocs,
    }
}

fn diff_win32(
    wa: &crate::win32::ProcessMemorySnapshot,
    wb: &crate::win32::ProcessMemorySnapshot,
    labels: &LabelMap,
) -> Win32DiffSummary {
    let agg = |w: &crate::win32::ProcessMemorySnapshot| -> HashMap<u64, NewVaAlloc> {
        let mut m: HashMap<u64, NewVaAlloc> = HashMap::new();
        for r in &w.regions {
            if !r.committed {
                continue;
            }
            let e = m.entry(r.allocation_base).or_insert_with(|| NewVaAlloc {
                allocation_base: r.allocation_base,
                commit_bytes: 0,
                resident_bytes: 0,
                paged_out_private_bytes: 0,
                non_ws_mapped_bytes: 0,
                untouched_bytes: 0,
                mem_type: r.mem_type.as_str().to_string(),
                write_combined: r.write_combined,
                owner: labels
                    .owner_of(r.allocation_base)
                    .unwrap_or("<unlabeled>")
                    .to_string(),
            });
            e.commit_bytes += r.size;
            e.resident_bytes += r.resident_bytes;
            if r.mem_type == MemoryType::Private {
                e.paged_out_private_bytes += r.paged_out_bytes;
            } else {
                e.non_ws_mapped_bytes += r.paged_out_bytes;
            }
            e.untouched_bytes += r.untouched_bytes;
            e.write_combined |= r.write_combined;
        }
        m
    };

    let map_a = agg(wa);
    let map_b = agg(wb);

    let mut new_va_allocs = Vec::new();
    let mut new_va_commit_bytes = 0u64;
    let mut groups: BTreeMap<(String, String, usize), (usize, u64)> = BTreeMap::new();
    for (&base, ab) in &map_b {
        let delta = match map_a.get(&base) {
            None => ab.commit_bytes,
            Some(aa) if ab.commit_bytes > aa.commit_bytes => ab.commit_bytes - aa.commit_bytes,
            Some(_) => 0,
        };
        if delta > 0 {
            let placement = if ab.write_combined {
                format!("{}_wc", ab.mem_type)
            } else {
                ab.mem_type.clone()
            };
            let bucket = size_bucket_index(delta);
            let g = groups
                .entry((placement, ab.owner.clone(), bucket))
                .or_insert((0, 0));
            g.0 += 1;
            g.1 += delta;
            new_va_commit_bytes += delta;
            if !map_a.contains_key(&base) {
                new_va_allocs.push(ab.clone());
            } else {
                let mut grown = ab.clone();
                grown.commit_bytes = delta;
                new_va_allocs.push(grown);
            }
        }
    }

    let mut freed_va_alloc_count = 0usize;
    let mut freed_va_commit_bytes = 0u64;
    for (&base, aa) in &map_a {
        if !map_b.contains_key(&base) {
            freed_va_alloc_count += 1;
            freed_va_commit_bytes += aa.commit_bytes;
        }
    }

    let mut new_va_groups: Vec<NewAllocGroup> = groups
        .into_iter()
        .map(
            |((placement, owner, bucket), (count, total_bytes))| NewAllocGroup {
                placement,
                owner,
                size_class: SIZE_BUCKETS[bucket].to_string(),
                count,
                total_bytes,
            },
        )
        .collect();
    new_va_groups.sort_by(|x, y| {
        y.total_bytes
            .cmp(&x.total_bytes)
            .then_with(|| y.count.cmp(&x.count))
    });
    new_va_allocs.sort_by_key(|a| std::cmp::Reverse(a.commit_bytes));

    let qvmi_loc_a: u64 = wa.video_memory.iter().map(|v| v.local_usage).sum();
    let qvmi_loc_b: u64 = wb.video_memory.iter().map(|v| v.local_usage).sum();
    let qvmi_nl_a: u64 = wa.video_memory.iter().map(|v| v.non_local_usage).sum();
    let qvmi_nl_b: u64 = wb.video_memory.iter().map(|v| v.non_local_usage).sum();

    Win32DiffSummary {
        private_usage_a: wa.counters.private_usage,
        private_usage_b: wb.counters.private_usage,
        private_usage_delta: sub_i64(wb.counters.private_usage, wa.counters.private_usage),
        working_set_a: wa.counters.working_set_size,
        working_set_b: wb.counters.working_set_size,
        working_set_delta: sub_i64(wb.counters.working_set_size, wa.counters.working_set_size),
        commit_a: wa.totals.commit_bytes,
        commit_b: wb.totals.commit_bytes,
        commit_delta: sub_i64(wb.totals.commit_bytes, wa.totals.commit_bytes),
        qvmi_local_a: qvmi_loc_a,
        qvmi_local_b: qvmi_loc_b,
        qvmi_local_delta: sub_i64(qvmi_loc_b, qvmi_loc_a),
        qvmi_non_local_a: qvmi_nl_a,
        qvmi_non_local_b: qvmi_nl_b,
        qvmi_non_local_delta: sub_i64(qvmi_nl_b, qvmi_nl_a),
        new_va_alloc_count: new_va_allocs.len(),
        new_va_commit_bytes,
        freed_va_alloc_count,
        freed_va_commit_bytes,
        new_va_groups,
        new_va_allocs,
    }
}

/// Renders a diff report as structured text honoring `opts` (`top`, `placement`, `min_bytes`, `max_bytes`, `sort`).
pub fn format_diff_text(diff: &DiffReport, opts: &FormatOptions) -> String {
    use std::fmt::Write;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "locum-mem diff | A={} (pid={}) -> B={} (pid={})",
        diff.source_a,
        diff.pid_a.map(|p| p.to_string()).unwrap_or_else(|| "all".into()),
        diff.source_b,
        diff.pid_b.map(|p| p.to_string()).unwrap_or_else(|| "all".into()),
    );

    if let Some(ref w) = diff.win32 {
        let _ = writeln!(out, "\n== Win32 CPU + QVMI Delta ==");
        let _ = writeln!(
            out,
            "  OS PrivateUsage   : {:>9.2} -> {:>9.2} MiB ({:>+9.2} MiB)",
            mib(w.private_usage_a),
            mib(w.private_usage_b),
            mib_signed(w.private_usage_delta),
        );
        let _ = writeln!(
            out,
            "  OS WorkingSetSize : {:>9.2} -> {:>9.2} MiB ({:>+9.2} MiB)",
            mib(w.working_set_a),
            mib(w.working_set_b),
            mib_signed(w.working_set_delta),
        );
        let _ = writeln!(
            out,
            "  VA Committed      : {:>9.2} -> {:>9.2} MiB ({:>+9.2} MiB)",
            mib(w.commit_a),
            mib(w.commit_b),
            mib_signed(w.commit_delta),
        );
        if w.qvmi_local_a > 0 || w.qvmi_local_b > 0 || w.qvmi_non_local_a > 0 || w.qvmi_non_local_b > 0 {
            let _ = writeln!(
                out,
                "  QVMI Local        : {:>9.2} -> {:>9.2} MiB ({:>+9.2} MiB)",
                mib(w.qvmi_local_a),
                mib(w.qvmi_local_b),
                mib_signed(w.qvmi_local_delta),
            );
            let _ = writeln!(
                out,
                "  QVMI NonLocal     : {:>9.2} -> {:>9.2} MiB ({:>+9.2} MiB)",
                mib(w.qvmi_non_local_a),
                mib(w.qvmi_non_local_b),
                mib_signed(w.qvmi_non_local_delta),
            );
        }
        let _ = writeln!(
            out,
            "  VA Allocs Delta   : +{} new/grown (+{:.2} MiB) | -{} freed (-{:.2} MiB)",
            w.new_va_alloc_count,
            mib(w.new_va_commit_bytes),
            w.freed_va_alloc_count,
            mib(w.freed_va_commit_bytes),
        );
        if !w.new_va_groups.is_empty() {
            let _ = writeln!(
                out,
                "\n  New/Grown Win32 VA Regions by (type, owner, size_class):"
            );
            let _ = writeln!(
                out,
                "    {:<12} {:<26} {:<12} {:>8} {:>10}",
                "type", "owner", "size_class", "count", "delta_MiB"
            );
            for g in w.new_va_groups.iter().take(opts.top.max(20)) {
                let _ = writeln!(
                    out,
                    "    {:<12} {:<26} {:<12} {:>8} {:>10.2}",
                    g.placement,
                    truncate(&g.owner, 26),
                    g.size_class,
                    g.count,
                    mib(g.total_bytes),
                );
            }
        }
        if opts.top > 0 && !w.new_va_allocs.is_empty() && diff.vidmm.is_none() {
            let filtered: Vec<_> = w
                .new_va_allocs
                .iter()
                .filter(|a| {
                    opts.min_bytes.is_none_or(|m| a.commit_bytes >= m)
                        && opts.max_bytes.is_none_or(|m| a.commit_bytes <= m)
                })
                .collect();
            let _ = writeln!(
                out,
                "\n  Top {} New/Grown Win32 VA Allocations ({} matching):",
                opts.top.min(filtered.len()),
                filtered.len()
            );
            let _ = writeln!(
                out,
                "    {:>10} {:>9} {:>9} {:<10} {:>18} owner",
                "delta_MiB", "ram_MiB", "priv_pg_M", "type", "base"
            );
            for a in filtered.iter().take(opts.top) {
                let ty = if a.write_combined {
                    format!("{}_wc", a.mem_type)
                } else {
                    a.mem_type.clone()
                };
                let _ = writeln!(
                    out,
                    "    {:>10.2} {:>9.2} {:>9.2} {:<10} {:>18} {}",
                    mib(a.commit_bytes),
                    mib(a.resident_bytes),
                    mib(a.paged_out_private_bytes),
                    ty,
                    format!("{:#x}", a.allocation_base),
                    a.owner,
                );
            }
        }
    }

    if let Some(ref v) = diff.vidmm {
        let _ = writeln!(out, "\n== VidMm Live Allocation Delta ==");
        let _ = writeln!(
            out,
            "  Live Allocs       : {} -> {} ({:+}) | {:.2} -> {:.2} MiB ({:+.2} MiB)",
            v.allocs_a,
            v.allocs_b,
            v.allocs_b as i64 - v.allocs_a as i64,
            mib(v.total_bytes_a),
            mib(v.total_bytes_b),
            mib_signed(sub_i64(v.total_bytes_b, v.total_bytes_a)),
        );
        let _ = writeln!(
            out,
            "  Placement Delta   : Local/BAR {:+.2} MiB | NonLocal Host {:+.2} MiB | Demoted {:+.2} MiB",
            mib_signed(v.local_bytes_delta),
            mib_signed(v.non_local_bytes_delta),
            mib_signed(v.demoted_bytes_delta),
        );
        let _ = writeln!(
            out,
            "  Turnover          : +{} new (+{:.2} MiB) | -{} freed (-{:.2} MiB) | {} persisted ({} changed placement)",
            v.new_count,
            mib(v.new_bytes),
            v.freed_count,
            mib(v.freed_bytes),
            v.persisted_count,
            v.placement_changed_count,
        );
        if !v.new_groups.is_empty() {
            let _ = writeln!(
                out,
                "\n  New VidMm Allocations Grouped by (placement, owner, size_class):"
            );
            let _ = writeln!(
                out,
                "    {:<15} {:<26} {:<12} {:>8} {:>10}",
                "placement", "owner", "size_class", "count", "new_MiB"
            );
            for g in v
                .new_groups
                .iter()
                .filter(|g| opts.placement.matches_label(&g.placement))
                .take(opts.top.max(25))
            {
                let _ = writeln!(
                    out,
                    "    {:<15} {:<26} {:<12} {:>8} {:>10.2}",
                    g.placement,
                    truncate(&g.owner, 26),
                    g.size_class,
                    g.count,
                    mib(g.total_bytes),
                );
            }
        }
        if opts.top > 0 && !v.new_allocs.is_empty() {
            let mut filtered: Vec<&NewVidMmAlloc> = v
                .new_allocs
                .iter()
                .filter(|a| {
                    opts.placement.matches_label(&a.placement)
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
                "\n  Top {} New VidMm Allocations ({} matching filter, sorted by {}):",
                opts.top.min(filtered.len()),
                filtered.len(),
                opts.sort.as_str(),
            );
            let _ = writeln!(
                out,
                "    {:>10} {:>4} {:>4} {:<15} {:>8} {:>10} {:>18} {:>18} owner",
                "size_MiB", "pref", "act", "placement", "t_s", "flags", "cpu_va", "gpu_va"
            );
            for a in filtered.iter().take(opts.top) {
                let act = a
                    .current_segment_id
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "-".into());
                let ts = if a.created_during_trace {
                    a.first_seen_secs
                        .map(|t| format!("+{t:.2}s"))
                        .unwrap_or_else(|| "new".into())
                } else {
                    "rundown".into()
                };
                let cpu = a.cpu_va.map(|x| format!("{x:#x}")).unwrap_or_else(|| "-".into());
                let gpu = a
                    .gpu_va_start
                    .map(|x| format!("{x:#x}"))
                    .unwrap_or_else(|| "-".into());
                let _ = writeln!(
                    out,
                    "    {:>10.2} {:>4} {:>4} {:<15} {:>8} {:>#10x} {:>18} {:>18} {}",
                    mib(a.size),
                    a.preferred_segment_id,
                    act,
                    a.placement,
                    ts,
                    a.flags,
                    cpu,
                    gpu,
                    a.owner,
                );
            }
        }
    }

    if !diff.by_owner.is_empty() {
        let owner_w = diff
            .by_owner
            .iter()
            .take(40)
            .map(|o| o.owner.len())
            .max()
            .unwrap_or(22)
            .clamp(22, 42);
        let _ = writeln!(out, "\n== Owner Deltas (B - A, MiB) ==");
        let _ = writeln!(
            out,
            "  {:<owner_w$} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9} {:>9}",
            "owner",
            "commit_d",
            "ram_d",
            "privpg_d",
            "mapnws_d",
            "vram_d",
            "host_d",
            "demot_d",
            owner_w = owner_w,
        );
        for o in diff.by_owner.iter().take(40) {
            let _ = writeln!(
                out,
                "  {:<owner_w$} {:>+9.2} {:>+9.2} {:>+9.2} {:>+9.2} {:>+9.2} {:>+9.2} {:>+9.2}",
                o.owner,
                mib_signed(o.commit_delta),
                mib_signed(o.ram_delta),
                mib_signed(o.paged_private_delta),
                mib_signed(o.non_ws_mapped_delta),
                mib_signed(o.vram_delta),
                mib_signed(o.host_delta),
                mib_signed(o.demoted_delta),
                owner_w = owner_w,
            );
        }
    }

    out
}

fn sub_i64(b: u64, a: u64) -> i64 {
    b as i128 as i64 - a as i128 as i64
}

fn mib_signed(bytes: i64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('~');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::etw::{SegmentInfo, TraceCoverage};

    #[test]
    fn diff_identifies_new_host_allocations_grouped_by_placement_owner_and_size() {
        let mk_seg = || {
            vec![
                SegmentInfo {
                    adapter: 1,
                    segment_id: 2,
                    described: true,
                    size: 16 << 30,
                    commit_limit: 16 << 30,
                    flags: 0x0009_2c10,
                    memory_segment_group: 0,
                    aperture: false,
                    cpu_visible: true,
                    populated_from_system_memory: false,
                    kind: SegmentKind::LocalCpuVisibleVram,
                    resident_last: 1 << 30,
                    resident_min: 1 << 30,
                    resident_max: 1 << 30,
                    resident_samples: 10,
                },
                SegmentInfo {
                    adapter: 1,
                    segment_id: 3,
                    described: true,
                    size: 32 << 30,
                    commit_limit: 32 << 30,
                    flags: 0x0010_1194,
                    memory_segment_group: 1,
                    aperture: false,
                    cpu_visible: false,
                    populated_from_system_memory: true,
                    kind: SegmentKind::NonLocalHost,
                    resident_last: 2 << 30,
                    resident_min: 2 << 30,
                    resident_max: 2 << 30,
                    resident_samples: 10,
                },
            ]
        };
        let mk_alloc = |gh: u64, size: u64, seg: u32, created: bool, ts: f64| VidMmAllocation {
            global_handle: gh,
            adapter: 1,
            pid: 42,
            device: 1,
            size,
            flags: 0x10,
            preferred_segment_raw: seg,
            preferred_segment_id: seg,
            read_segment_set: 0,
            write_segment_set: 0,
            current_segment_id: Some(seg),
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
            first_seen_secs: Some(ts),
            created_during_trace: created,
        };

        let ea = EtwReport {
            filter_pid: Some(42),
            coverage: TraceCoverage {
                has_segment_descriptions: true,
                has_allocation_rundown: true,
                ..Default::default()
            },
            segments: mk_seg(),
            allocations: vec![mk_alloc(1, 64 << 20, 2, false, 0.0)],
            tracked_local_bar_bytes: 64 << 20,
            tracked_total_bytes: 64 << 20,
            ..Default::default()
        };
        let eb = EtwReport {
            filter_pid: Some(42),
            coverage: TraceCoverage {
                has_segment_descriptions: true,
                has_allocation_rundown: true,
                ..Default::default()
            },
            segments: mk_seg(),
            allocations: vec![
                mk_alloc(1, 64 << 20, 2, false, 0.0),
                mk_alloc(2, 256 << 10, 3, true, 1.5),
                mk_alloc(3, 256 << 10, 3, true, 2.5),
            ],
            tracked_local_bar_bytes: 64 << 20,
            tracked_non_local_host_bytes: 512 << 10,
            tracked_total_bytes: (64 << 20) + (512 << 10),
            ..Default::default()
        };

        let labels = LabelMap::default();
        let ra = reconcile(None, Some(ea), &labels, None);
        let rb = reconcile(None, Some(eb), &labels, None);
        let d = diff_reports("A.etl", &ra, "B.etl", &rb, &labels);
        let vd = d.vidmm.as_ref().unwrap();
        assert_eq!(vd.new_count, 2);
        assert_eq!(vd.new_bytes, 512 << 10);
        assert_eq!(vd.persisted_count, 1);
        assert_eq!(vd.new_groups.len(), 1);
        assert_eq!(vd.new_groups[0].placement, "non_local_host");
        assert_eq!(vd.new_groups[0].size_class, "64K..1M");
        assert_eq!(vd.new_groups[0].count, 2);

        let text = format_diff_text(&d, &FormatOptions::default());
        assert!(text.contains("non_local_host"));
        assert!(text.contains("64K..1M"));
        assert!(text.contains("+2 new"));
    }
}
