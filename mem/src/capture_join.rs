//! Optional read-only join against a `locum` `capture.db`'s `d12_draw_descriptors` table.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};

use crate::etw::EtwReport;
use crate::labels::LabelMap;

/// Summary of `d12_draw_descriptors` joined against `--labels` and/or ETW GPU VA mappings.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CaptureJoinReport {
    pub capture_path: String,
    pub total_descriptor_rows: u64,
    pub rows_with_gpu_address: u64,
    pub flagged_rows: u64,
    pub by_owner: Vec<CaptureOwnerSummary>,
    pub by_vidmm_placement: Vec<CapturePlacementSummary>,
    pub flag_counts: BTreeMap<String, u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaptureOwnerSummary {
    pub owner: String,
    pub descriptor_rows: u64,
    pub distinct_draws: u64,
    pub distinct_resources: u64,
    pub max_view_size: u64,
    pub total_view_bytes: u64,
    pub flagged_rows: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturePlacementSummary {
    pub placement: String,
    pub descriptor_rows: u64,
    pub distinct_draws: u64,
    pub distinct_resources: u64,
}

/// Opens `capture_path` (either `capture.db` or a capture folder containing `capture.db`) read-only
/// and joins `d12_draw_descriptors` against `labels` and optional `etw` GPU VA mappings.
pub fn join_capture(
    capture_path: &Path,
    labels: &LabelMap,
    etw: Option<&EtwReport>,
) -> Result<CaptureJoinReport, String> {
    let db_path = if capture_path.is_dir() {
        capture_path.join("capture.db")
    } else {
        capture_path.to_path_buf()
    };
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("open capture {}: {e}", db_path.display()))?;

    let has_table: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='d12_draw_descriptors'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has_table {
        return Err(format!(
            "{}: no `d12_draw_descriptors` table (run `locum-cli upgrade` on older D3D12 captures)",
            db_path.display()
        ));
    }

    let mut report = CaptureJoinReport {
        capture_path: db_path.display().to_string(),
        ..Default::default()
    };

    #[derive(Default)]
    struct OwnerAcc {
        rows: u64,
        draws: BTreeSet<i64>,
        resources: BTreeSet<i64>,
        max_size: u64,
        total_size: u64,
        flagged: u64,
    }
    #[derive(Default)]
    struct PlaceAcc {
        rows: u64,
        draws: BTreeSet<i64>,
        resources: BTreeSet<i64>,
    }

    let mut by_owner: BTreeMap<String, OwnerAcc> = BTreeMap::new();
    let mut by_place: BTreeMap<String, PlaceAcc> = BTreeMap::new();

    // Load resource GPU VA fallback from d12_objects for descriptors whose address is null/0.
    let mut res_gpu_va: BTreeMap<i64, (u64, u64)> = BTreeMap::new();
    let has_objects: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='d12_objects'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if has_objects
        && let Ok(mut stmt) =
            conn.prepare("SELECT id, description FROM d12_objects WHERE kind = 'ID3D12Resource'")
        && let Ok(rows) = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
    {
        for row in rows.flatten() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&row.1)
                && let Some(addr) = v["gpu_address"].as_u64().filter(|&a| a != 0)
            {
                let width = v["width"].as_u64().unwrap_or(0);
                res_gpu_va.insert(row.0, (addr, width));
            }
        }
    }

    let mut stmt = conn
        .prepare("SELECT draw, resource, address, size, flag FROM d12_draw_descriptors")
        .map_err(|e| format!("prepare d12_draw_descriptors: {e}"))?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, Option<i64>>(3)?,
                r.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|e| format!("query d12_draw_descriptors: {e}"))?;

    for row in rows {
        let (draw, resource, addr_opt, size_opt, flag_opt) =
            row.map_err(|e| format!("read d12_draw_descriptors row: {e}"))?;
        report.total_descriptor_rows += 1;
        if let Some(ref f) = flag_opt {
            report.flagged_rows += 1;
            *report.flag_counts.entry(f.clone()).or_insert(0) += 1;
        }
        let mut gpu_va = addr_opt.map(|a| a as u64).filter(|&a| a != 0);
        let mut view_size = size_opt.map(|s| s.max(0) as u64).unwrap_or(0);
        if gpu_va.is_none()
            && let Some(res) = resource
            && let Some(&(va, w)) = res_gpu_va.get(&res)
        {
            gpu_va = Some(va);
            if view_size == 0 {
                view_size = w;
            }
        }

        if let Some(va) = gpu_va {
            report.rows_with_gpu_address += 1;
            let owner = labels.owner_of(va).unwrap_or("<unlabeled>").to_string();
            let acc = by_owner.entry(owner).or_default();
            acc.rows += 1;
            acc.draws.insert(draw);
            if let Some(r) = resource {
                acc.resources.insert(r);
            }
            acc.max_size = acc.max_size.max(view_size);
            acc.total_size = acc.total_size.saturating_add(view_size);
            if flag_opt.is_some() {
                acc.flagged += 1;
            }

            if let Some(etw_rep) = etw {
                let placement = etw_rep
                    .placement_of_gpu_va(va)
                    .unwrap_or("unmapped_in_etw")
                    .to_string();
                let pacc = by_place.entry(placement).or_default();
                pacc.rows += 1;
                pacc.draws.insert(draw);
                if let Some(r) = resource {
                    pacc.resources.insert(r);
                }
            }
        }
    }

    report.by_owner = by_owner
        .into_iter()
        .map(|(owner, a)| CaptureOwnerSummary {
            owner,
            descriptor_rows: a.rows,
            distinct_draws: a.draws.len() as u64,
            distinct_resources: a.resources.len() as u64,
            max_view_size: a.max_size,
            total_view_bytes: a.total_size,
            flagged_rows: a.flagged,
        })
        .collect();
    report
        .by_owner
        .sort_by_key(|a| std::cmp::Reverse(a.descriptor_rows));

    report.by_vidmm_placement = by_place
        .into_iter()
        .map(|(placement, a)| CapturePlacementSummary {
            placement,
            descriptor_rows: a.rows,
            distinct_draws: a.draws.len() as u64,
            distinct_resources: a.resources.len() as u64,
        })
        .collect();
    report
        .by_vidmm_placement
        .sort_by_key(|a| std::cmp::Reverse(a.descriptor_rows));

    Ok(report)
}
