//! Writing a captured frame to `captures/frame-<n>/`:
//! - `draws.jsonl`, one JSON object per line:
//!   - a `frame` line: draw count, command lists spliced in or executed without a recording, copy failures, targets
//!     read back and skipped;
//!   - a `resource`, `view` or `state` line for each object the frame used, with the number the draws refer to;
//!   - a `cb` line for each distinct constant-buffer content (hex);
//!   - a `draw` line for each draw and dispatch in the order the GPU runs them;
//! - `targets/<draw>-<target>.dds`: the targets after each pass, with `targets`.

use std::collections::HashSet;
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::json;
use windows::Win32::Graphics::Direct3D11::*;

use super::snapshot::CHUNK;
use super::{Draw, state};
use crate::log::{folder, log};
use crate::d3d11::shaders;

#[derive(Serialize)]
struct DrawLine<'a> {
    e: &'static str,
    i: usize,
    #[serde(flatten)]
    draw: &'a Draw,
}

/// Reads the copies back and writes the frame. Runs on the presenting thread after the real Present.
pub(super) fn finish(immediate: &ID3D11DeviceContext4) {
    let mut s = state();
    let frame = s.frame;
    let Some(dir) = folder().map(|d| d.join("captures").join(format!("frame-{frame}"))) else {
        s.reset();
        return;
    };
    let _ = std::fs::create_dir_all(&dir);

    // Map every constant-buffer staging chunk (waits for the GPU to finish the frame's copies).
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    for (i, staging) in s.staging.iter().enumerate() {
        let len = if i + 1 == s.staging.len() { s.used } else { CHUNK } as usize;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if unsafe { immediate.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.is_ok() {
            chunks.push(unsafe { core::slice::from_raw_parts(mapped.pData as *const u8, len) }.to_vec());
            unsafe { immediate.Unmap(staging, 0) };
        } else {
            chunks.push(Vec::new());
        }
    }

    let mut timeline = std::mem::take(&mut s.timeline);
    let mut out = String::new();
    let mut seen: HashSet<u64> = HashSet::new();
    let mut cb_lines = String::new();
    let (mut targets, mut target_bytes) = (0u32, 0u64);
    let targets_dir = dir.join("targets");
    for (index, d) in timeline.iter_mut().enumerate() {
        for cb in &mut d.cbs {
            let data = cb.copy.and_then(|(chunk, offset, bytes)| {
                chunks
                    .get(chunk as usize)?
                    .get(offset as usize..(offset + bytes) as usize)
            });
            cb.content = data.map(|bytes| {
                let h = shaders::hash(bytes);
                if seen.insert(h) {
                    let line = json!({"e": "cb", "hash": format!("{h:016x}"), "bytes": bytes.len(), "hex": hex(bytes)});
                    let _ = writeln!(cb_lines, "{line}");
                }
                format!("{h:016x}")
            });
        }
        if !d.after.is_empty() {
            let _ = std::fs::create_dir_all(&targets_dir);
        }
        for t in &mut d.after {
            let name = format!("{index:05}-{}.dds", t.target_name());
            let written = unsafe { t.write(immediate, &targets_dir, name) };
            if written > 0 {
                targets += 1;
                target_bytes += written;
            }
        }
    }

    let frame_line = json!({"e": "frame", "frame": frame, "draws": timeline.len(), "lists_spliced": s.lists_spliced,
        "lists_unrecorded": s.lists_unrecorded, "lists_prerecorded": s.lists_prerecorded, "copy_failures": s.copy_failures, "targets": targets,
        "targets_skipped": s.targets_skipped,
        "targets_skipped_mib": s.targets_skipped_bytes >> 20});
    let _ = writeln!(out, "{frame_line}");
    for line in &s.objects {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&cb_lines);
    for (i, draw) in timeline.iter().enumerate() {
        let line = serde_json::to_string(&DrawLine { e: "draw", i, draw }).expect("a draw serialises");
        out.push_str(&line);
        out.push('\n');
    }
    let path = dir.join("draws.jsonl");
    let written = std::fs::write(&path, out).is_ok();
    log!(
        "capture of frame {frame}: {} draws, {} command lists spliced, {} executed without a recording, {} copy failures, {targets} targets ({} MiB, {} skipped, {} MiB more needed) -> {} ({})",
        timeline.len(),
        s.lists_spliced,
        s.lists_unrecorded,
        s.copy_failures,
        target_bytes >> 20,
        s.targets_skipped,
        s.targets_skipped_bytes >> 20,
        path.display(),
        if written { "written" } else { "NOT written" }
    );
    s.reset();
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
