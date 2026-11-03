//! The end of a capture: the copies of the contents of every resource the frame used (made before the timeline
//! first touched each) are read, and the whole capture is handed to the writer thread as `captures/<name>/capture.db`.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use locum_capture::db::{self, Capture};
use locum_capture::stacks::{self, Interner};
use locum_capture::{hash, writer};
use serde_json::json;
use windows::Win32::Graphics::Direct3D11::*;

use super::contents;
use super::snapshot::CHUNK;
use super::{Tracked, state, touched};
use crate::d3d11::registry;
use crate::log::{folder, log};
use crate::settings::settings;

/// Where a used resource's contents came from.
struct Content {
    key: String,
    subs: Vec<String>,
    source: &'static str,
    note: Option<&'static str>,
}

/// An immutable resource's contents when its upload is stored: the upload's key and its subresources' keys.
pub(super) fn upload_of(id: u64) -> Option<(String, Vec<String>)> {
    let e = registry::entry(id).filter(|e| e.dims.immutable() && e.upload_stored)?;
    let subs = e
        .sub_hashes
        .iter()
        .map(|h| h.clone().unwrap_or_default())
        .collect();
    Some((e.upload?, subs))
}

/// The ids without repeats, in order.
fn sorted(ids: &[u64]) -> Vec<u64> {
    let mut v = ids.to_vec();
    v.sort_unstable();
    v.dedup();
    v
}

/// Reads everything back and hands the capture to the writer. Runs on the presenting thread after the real Present.
pub(super) fn finish(immediate: &ID3D11DeviceContext4) {
    let started = std::time::Instant::now();
    let mut s = state();
    let frame = s.frame;
    let request = s.request.clone();
    let name = request.label.clone().unwrap_or_else(|| format!("frame-{frame}"));
    let Some(dir) = folder().map(|d| d.join("captures").join(&name)) else {
        s.reset();
        return;
    };
    let timeline = std::mem::take(&mut s.timeline);
    let tracked: BTreeMap<u64, Tracked> = std::mem::take(&mut s.resources);
    let in_flight = std::mem::take(&mut s.in_flight);
    let mut read = std::mem::take(&mut s.contents);
    let counters = std::mem::take(&mut s.counter_copies);
    let views = std::mem::take(&mut s.views);
    let states = std::mem::take(&mut s.states);
    let staging = std::mem::take(&mut s.staging);
    let used_bytes = s.used;
    let counts = (
        s.lists_spliced,
        s.lists_before,
        s.lists_unrecorded,
        s.copy_failures,
        s.targets_skipped,
        s.targets_skipped_bytes,
    );
    let mut gaps = std::mem::take(&mut s.gaps);
    s.reset();
    drop(s);
    super::ops::reset();

    // The process's memory, as close to the frame as can be: before anything is read back.
    let memory = if request.memory {
        match crate::dump::dump(&name, &dir) {
            Ok(d) => {
                log!(
                    "memory dump started: {} (the game waited {:.0} ms for the snapshot)",
                    d.path.display(),
                    d.held.as_secs_f64() * 1000.0
                );
                json!({"path": d.path.display().to_string(), "held_ms": d.held.as_millis() as u64})
            }
            Err(e) => {
                log!("no memory dump: {e}");
                gaps.insert(("memory".into(), e.clone()), 1);
                json!({"error": e})
            }
        }
    } else {
        serde_json::Value::Null
    };

    // Constant-buffer copies (waits for the GPU to finish the frame).
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    for (i, buffer) in staging.iter().enumerate() {
        let len = if i + 1 == staging.len() { used_bytes } else { CHUNK } as usize;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if unsafe { immediate.Map(buffer, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.is_ok() {
            chunks.push(unsafe { core::slice::from_raw_parts(mapped.pData as *const u8, len) }.to_vec());
            unsafe { immediate.Unmap(buffer, 0) };
        } else {
            chunks.push(Vec::new());
        }
    }
    drop(staging);

    // Contents of the resources the frame used, as the timeline first touched them.
    let (used, written) = touched(&timeline);
    let mut content: BTreeMap<u64, Result<Content, String>> = BTreeMap::new();
    if request.contents {
        read.extend(unsafe { contents::read_all(immediate, in_flight) });
        for &id in &used {
            let multisampled = tracked.get(&id).is_some_and(|t| t.dims.samples > 1);
            let c = match read.remove(&id) {
                Some(r) => r.map(|(key, subs)| Content {
                    key,
                    subs,
                    source: "first-use",
                    note: multisampled.then_some("multisampled: the copy is the resolve of its samples"),
                }),
                None => match upload_of(id) {
                    Some((key, subs)) => Ok(Content {
                        key,
                        subs,
                        source: "upload",
                        note: None,
                    }),
                    None if tracked.contains_key(&id) => {
                        Err("not copied before the timeline first touched it".into())
                    }
                    None => Err("not known to the capture".into()),
                },
            };
            content.insert(id, c);
        }
    }

    // Number draws, read targets, collect stacks.
    let mut capture = Capture::default();
    for (view, copy) in counters {
        match copy.and_then(|buffer| super::counters::read(immediate, &buffer)) {
            Ok(count) => capture.uav_counters.push((view, count)),
            Err(e) => {
                *gaps
                    .entry(("counters".into(), format!("view #{view}: {e}")))
                    .or_insert(0) += 1;
            }
        }
    }
    let mut interner = Interner::default();
    let mut cb_seen: HashMap<String, ()> = HashMap::new();
    let mut shader_hashes: BTreeSet<(String, &'static str)> = BTreeSet::new();
    let (mut targets_read, mut target_bytes) = (0u32, 0u64);
    // A command list executed more than once made its copies (constants, targets) into the same places each time,
    // so they hold what its last execution saw: earlier executions' are not kept (replay works their constants out
    // from the replayed buffers instead).
    let mut last_cb: HashMap<(u32, u32), usize> = HashMap::new();
    let mut last_target: HashMap<usize, usize> = HashMap::new();
    let mut last_result: HashMap<usize, usize> = HashMap::new();
    for (seq, e) in timeline.iter().enumerate() {
        for (_, r) in &e.results {
            if let Ok(rb) = r {
                last_result.insert(rb.copy_id(), seq);
            }
        }
        let Some(d) = e.draw.as_deref() else { continue };
        for cb in &d.cb_copies {
            if let Some((chunk, offset, _)) = cb.copy {
                last_cb.insert((chunk, offset), seq);
            }
        }
        for t in &d.after {
            if let Some(id) = t.copy_id() {
                last_target.insert(id, seq);
            }
        }
    }
    let (mut cbs_repeated, mut targets_repeated, mut results_repeated) = (0u64, 0u64, 0u64);
    let mut outputs_repeated = 0u64;
    let mut n = 0u64;
    let mut pass: Option<(u64, Vec<(u32, u64)>)> = None;
    let mut timeline = timeline;
    for (seq, e) in timeline.iter_mut().enumerate() {
        let index = seq;
        let seq = seq as u64;
        let stack = interner.id(&e.stack);
        let draw_n = e.draw.as_ref().map(|_| n);
        capture.events.push(db::Event {
            seq,
            kind: e.kind,
            call: e.call,
            context: e.context,
            deferred: e.deferred,
            list: e.list,
            draw: draw_n,
            args: e.args.to_string(),
            data: e.data.clone(),
            stack,
            reads: sorted(&e.reads),
            writes: sorted(&e.writes),
        });
        // Without an order-dependent event's output replay cannot go on in the game's order (a gap that leaves the
        // capture incomplete); without another's, that output is only not compared.
        let ordered = e.draw.as_ref().is_some_and(|d| d.order_dependent);
        for (resource, r) in std::mem::take(&mut e.results) {
            let read = match r {
                Ok(rb) if last_result.get(&rb.copy_id()) != Some(&index) => {
                    if ordered {
                        results_repeated += 1;
                    } else {
                        outputs_repeated += 1;
                    }
                    continue;
                }
                Ok(rb) => unsafe { contents::read(immediate, &rb) },
                Err(e) => Err(e),
            };
            match read {
                Ok((key, _)) => capture.results.push((seq, resource, key, ordered)),
                Err(e) => {
                    let gap = if ordered {
                        (
                            "results".into(),
                            format!("an order-dependent shader's output not read: {e}"),
                        )
                    } else {
                        ("outputs".into(), format!("event output not read: {e}"))
                    };
                    *gaps.entry(gap).or_insert(0) += 1;
                }
            }
        }
        let Some(d) = e.draw.as_deref_mut() else { continue };
        for cb in &d.cb_copies {
            if let Some((chunk, offset, _)) = cb.copy
                && last_cb.get(&(chunk, offset)) != Some(&index)
            {
                cbs_repeated += 1;
                continue;
            }
            let data = if cb.empty {
                Some(&[][..])
            } else {
                cb.copy.and_then(|(chunk, offset, bytes)| {
                    chunks
                        .get(chunk as usize)?
                        .get(offset as usize..(offset + bytes) as usize)
                })
            };
            if let Some(bytes) = data {
                let key = hash::key(bytes);
                if cb_seen.insert(key.clone(), ()).is_none() {
                    capture.cb_data.push((key.clone(), bytes.to_vec()));
                }
                d.bindings[cb.binding].content = Some(key);
                d.bindings[cb.binding].size = Some(bytes.len() as u64);
            }
        }
        // Passes: runs of draws with the same targets.
        let mut target_set: Vec<(u32, u64)> = d
            .bindings
            .iter()
            .filter(|b| b.kind == "rtv" || b.kind == "dsv")
            .map(|b| (if b.kind == "dsv" { 99 } else { b.slot }, b.view.unwrap_or(0)))
            .collect();
        target_set.sort();
        let graphics = e.kind == "draw";
        let pass_n = if graphics {
            match &mut pass {
                Some((p, set)) if *set == target_set => {
                    let last = capture.passes.last_mut().expect("a pass");
                    last.last_draw = n;
                    Some(*p)
                }
                _ => {
                    let p = capture.passes.len() as u64;
                    capture.passes.push(db::Pass {
                        n: p,
                        first_draw: n,
                        last_draw: n,
                    });
                    pass = Some((p, target_set));
                    Some(p)
                }
            }
        } else {
            None
        };
        for (i, sh) in d.shaders.iter().enumerate() {
            if let Some(h) = sh.as_ref().filter(|h| h.as_str() != "unknown") {
                shader_hashes.insert((h.clone(), db::stage_names()[i]));
            }
        }
        let after = seq + u64::from(d.after_events);
        for t in &mut d.after {
            if t.copy_id().is_some_and(|id| last_target.get(&id) != Some(&index)) {
                targets_repeated += 1;
                continue;
            }
            if let Some((key, bytes)) = unsafe { t.read(immediate) } {
                targets_read += 1;
                target_bytes += bytes;
                capture.targets.push(db::Target {
                    draw: n,
                    target: t.target.clone(),
                    view: t.view,
                    resource: t.resource,
                    width: t.width,
                    height: t.height,
                    format: t.format as u32,
                    sub: t.sub,
                    z: t.z,
                    blob: Some(key),
                    event: after,
                });
            }
        }
        for mut b in d.bindings.drain(..) {
            b.draw = n;
            capture.bindings.push(b);
        }
        for mut v in d.viewports.drain(..) {
            v.draw = n;
            capture.viewports.push(v);
        }
        for mut v in d.scissors.drain(..) {
            v.draw = n;
            capture.scissors.push(v);
        }
        capture.draws.push(db::Draw {
            n,
            event: seq,
            call: e.call,
            args: e.args.to_string(),
            pass: pass_n,
            shaders: d.shaders.clone(),
            topology: d.topology,
            layout: d.layout,
            rasterizer: d.rasterizer,
            blend: d.blend,
            depth_stencil: d.depth_stencil,
            blend_factor: d.blend_factor,
            sample_mask: d.sample_mask,
            stencil_ref: d.stencil_ref,
            predicate: d.predicate,
            predicate_value: d.predicate_value,
        });
        n += 1;
    }
    let draws = n;
    drop(timeline);

    // Every live resource, and any used one the registry does not know.
    let mut ids: BTreeSet<u64> = registry::live().into_iter().collect();
    ids.extend(used.iter().copied());
    let (mut with_content, mut content_bytes, mut missing) = (0u64, 0u64, 0u64);
    for id in ids {
        let entry = registry::entry(id);
        let dims = entry
            .as_ref()
            .map(|e| e.dims.clone())
            .or_else(|| tracked.get(&id).map(|t| t.dims.clone()))
            .unwrap_or_default();
        let subs = dims.subs().unwrap_or_default();
        let bytes = locum_capture::layout::total(&subs);
        let mut row = db::Resource {
            id,
            kind: if dims.kind.is_empty() {
                "unknown"
            } else {
                dims.kind
            },
            width: dims.width,
            height: dims.height,
            depth: dims.depth,
            mips: dims.mips,
            array: dims.array,
            format: dims.format,
            samples: dims.samples,
            bytes,
            stride: dims.stride,
            usage: dims.usage,
            bind: dims.bind,
            cpu: dims.cpu,
            misc: dims.misc,
            desc: dims.desc.clone(),
            used: used.contains(&id),
            written: written.contains(&id),
            ..Default::default()
        };
        let mut sub_hashes: Vec<Option<String>> = vec![None; subs.len()];
        if let Some(e) = &entry {
            row.created_frame = e.created_frame;
            row.created_stack = interner.id(&e.created_stack);
            row.upload = e.upload.clone();
            row.upload_stored = e.upload_stored;
            row.updated_frame = e.updated_frame;
            row.updated_stack = interner.id(&e.updated_stack);
            row.update_count = e.update_count;
            for (i, h) in e.sub_hashes.iter().enumerate() {
                if let Some(slot) = sub_hashes.get_mut(i) {
                    slot.clone_from(h);
                }
            }
        }
        match content.remove(&id) {
            Some(Ok(c)) => {
                with_content += 1;
                content_bytes += bytes;
                row.content = Some(c.key);
                row.content_source = Some(c.source);
                row.content_note = c.note.map(String::from);
                for (i, h) in c.subs.into_iter().enumerate() {
                    if let Some(slot) = sub_hashes.get_mut(i)
                        && !h.is_empty()
                    {
                        *slot = Some(h);
                    }
                }
            }
            Some(Err(why)) => {
                missing += 1;
                row.content_note = Some(why.clone());
                *gaps.entry(("contents".into(), why)).or_insert(0) += 1;
            }
            None if row.used && request.contents => {
                missing += 1;
                row.content_note = Some("not read".into());
            }
            None => {}
        }
        for (sub, hash) in subs.iter().zip(sub_hashes) {
            if row.used || hash.is_some() {
                capture.subresources.push(db::Subresource {
                    resource: id,
                    sub: *sub,
                    hash,
                });
            }
        }
        capture.resources.push(row);
    }
    drop(tracked);
    capture.views = views;
    capture.states = states;

    // Shader bytecode, from the files written as shaders were created.
    let shader_dir = folder().map(|d| d.join("shaders"));
    for (h, stage) in shader_hashes {
        let mut row = db::Shader {
            hash: h.clone(),
            stage,
            ..Default::default()
        };
        if let Some(dir) = &shader_dir
            && let Ok(code) = std::fs::read(dir.join(format!("{h}.{stage}.dxbc")))
        {
            let key = hash::key(&code);
            row.size = code.len() as u64;
            let blobs = &settings().blobs;
            writer::blob(blobs, key.clone(), code, true);
            row.blob = Some(key);
        } else {
            *gaps
                .entry(("shaders".into(), "bytecode not found".into()))
                .or_insert(0) += 1;
        }
        capture.shaders.push(row);
    }

    let (spliced, before, unrecorded, copy_failures, targets_skipped, skipped_bytes) = counts;
    capture.modules = stacks::modules();
    capture.stacks = interner.into_rows();
    capture.meta("api", "d3d11");
    capture.meta("name", &name);
    capture.meta("frame", frame);
    capture.meta(
        "exe",
        std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
    );
    capture.meta("pid", std::process::id());
    if !memory.is_null() {
        capture.meta("memory", memory.to_string());
    }
    capture.meta("locum", env!("CARGO_PKG_VERSION"));
    capture.meta("blobs", settings().blobs.root().display());
    capture.meta(
        "time",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    );
    capture.meta(
        "request",
        json!({"targets": request.targets, "contents": request.contents, "memory": request.memory,
            "max_target_mb": request.max_target_bytes >> 20})
        .to_string(),
    );
    capture.meta(
        "counts",
        json!({"events": capture.events.len(), "draws": draws, "lists_spliced": spliced,
            "lists_recorded_before": before, "lists_unrecorded": unrecorded, "cb_copy_failures": copy_failures,
            "targets": targets_read, "target_bytes": target_bytes, "targets_skipped": targets_skipped,
            "targets_skipped_bytes": skipped_bytes, "resources_live": capture.resources.len(),
            "resources_used": used.len(), "resources_written": written.len(), "contents": with_content,
            "content_bytes": content_bytes, "contents_missing": missing})
        .to_string(),
    );
    if unrecorded > 0 {
        gaps.insert(
            ("lists".into(), "executed without a recording".into()),
            u64::from(unrecorded),
        );
    }
    if cbs_repeated > 0 {
        gaps.insert(
            (
                "evidence".into(),
                "constants of a command list executed more than once: only its last execution's were kept (replay \
                 reads the others from the replayed buffers)"
                    .into(),
            ),
            cbs_repeated,
        );
    }
    if results_repeated > 0 {
        gaps.insert(
            (
                "results".into(),
                "an order-dependent shader in a command list executed more than once: only its last execution's \
                 output was kept"
                    .into(),
            ),
            results_repeated,
        );
    }
    if outputs_repeated > 0 {
        gaps.insert(
            (
                "outputs".into(),
                "outputs in a command list executed more than once: only its last \
                 execution's output was kept"
                    .into(),
            ),
            outputs_repeated,
        );
    }
    if targets_repeated > 0 {
        gaps.insert(
            (
                "evidence".into(),
                "targets of a command list executed more than once: only its last execution's were read back"
                    .into(),
            ),
            targets_repeated,
        );
    }
    if copy_failures > 0 {
        gaps.insert(
            ("constants".into(), "constant-buffer copies failed".into()),
            u64::from(copy_failures),
        );
    }
    if targets_skipped > 0 {
        gaps.insert(
            (
                "targets".into(),
                "over the target memory limit or unreadable".into(),
            ),
            u64::from(targets_skipped),
        );
    }
    if skipped_bytes > 0 {
        gaps.insert(
            (
                "targets".into(),
                format!("{} bytes {}", skipped_bytes, super::OVER_LIMIT),
            ),
            1,
        );
    }
    for ((what, detail), count) in gaps {
        capture.gap(&what, detail, count);
    }
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("capture.db");
    let summary = format!(
        "capture of frame {frame} ({name}): {draws} draws, {} events, {spliced} command lists spliced, {unrecorded} \
         executed without a recording, {copy_failures} constant copy failures, {targets_read} targets ({} MiB), {} of \
         {} used resources with contents ({} MiB), {missing} missing; read back in {:.1} s",
        capture.events.len(),
        target_bytes >> 20,
        with_content,
        used.len(),
        content_bytes >> 20,
        started.elapsed().as_secs_f64()
    );
    log!("{summary}");
    log!("  the frame, {}", crate::cost::line());
    writer::run(move || {
        let t = std::time::Instant::now();
        match db::write(&path, &capture) {
            Ok(()) => log!(
                "capture written: {} ({:.1} s after the readback)",
                path.display(),
                t.elapsed().as_secs_f64()
            ),
            Err(e) => log!("capture NOT written to {}: {e}", path.display()),
        }
    });
}
