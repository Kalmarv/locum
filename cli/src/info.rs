//! `captures` and `info`: what captures there are, and what one holds and lacks.

use std::collections::BTreeSet;
use std::path::Path;

use crate::describe::mib;
use crate::open::{self, Capture};
use crate::out;

pub fn captures(dir: &Path, blobs: Option<&Path>) -> Result<(), String> {
    let found = open::list(dir);
    if found.is_empty() {
        out!("no captures under {}", dir.display());
        return Ok(());
    }
    // What each holds, so which capture can answer a question is one look: its constant buffer contents, resource
    // contents as the frame began, the passes whose targets were read back, and whether it names the presented buffer.
    out!("capture\tapi\tframe\tdraws\tconstants\tcontents\ttargets\tpresented\texe");
    for path in found {
        // The capture's folder from `dir` (`m15-sky/locum-log/captures/eldenring-sky` in a folder of runs), which
        // every command takes as the capture.
        let folder = path.parent().unwrap_or(&path);
        let name = folder
            .strip_prefix(dir)
            .unwrap_or(folder)
            .to_string_lossy()
            .replace('\\', "/");
        match open::open(&path.display().to_string(), blobs) {
            Ok(c) => {
                let counts = c.meta_json("counts");
                let exe = c.meta("exe").unwrap_or_default();
                let exe = exe.rsplit(['\\', '/']).next().unwrap_or(&exe).to_string();
                let passes = c.count("SELECT count(*) FROM passes", []);
                let read = c.count(
                    "SELECT count(DISTINCT d.pass) FROM targets t JOIN draws d ON d.n = t.draw WHERE t.blob IS NOT NULL",
                    [],
                );
                out!(
                    "{name}\t{}\t{}\t{}\t{}\t{}\t{read} of {passes} passes\t{}\t{exe}",
                    c.meta("api").unwrap_or_default(),
                    c.meta("frame").unwrap_or_default(),
                    counts["draws"],
                    c.count("SELECT count(*) FROM cb_data", []),
                    mib(counts["content_bytes"].as_u64().unwrap_or(0)),
                    if c.meta("presented").is_some() {
                        "yes"
                    } else {
                        "-"
                    },
                );
            }
            Err(e) => out!("{name}\t{e}"),
        }
    }
    Ok(())
}

pub fn info(c: &Capture) -> Result<(), String> {
    let counts = c.meta_json("counts");
    let request = c.meta_json("request");
    let n = |k: &str| counts[k].as_u64().unwrap_or(0);
    out!(
        "capture {} ({})",
        c.meta("name").unwrap_or_default(),
        c.path.display()
    );
    out!(
        "  frame {} of {} (pid {}), {}, locum {}",
        c.meta("frame").unwrap_or_default(),
        c.meta("exe").unwrap_or_default(),
        c.meta("pid").unwrap_or_default(),
        c.meta("api").unwrap_or_default(),
        c.meta("locum").unwrap_or_default()
    );
    if request.is_null() {
        out!("  request: not recorded (locum before the request was kept)");
    } else {
        out!("  request: {request}");
    }
    out!("  blob store: {}", c.blobs.root().display());
    out!("");

    let kinds: Vec<(String, i64)> = c.rows(
        "SELECT kind, count(*) FROM events GROUP BY kind ORDER BY count(*) DESC",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let kinds: Vec<String> = kinds.iter().map(|(k, n)| format!("{k} {n}")).collect();
    out!(
        "timeline: {} events ({}), {} draws and dispatches in {} passes",
        n("events"),
        kinds.join(", "),
        n("draws"),
        c.count("SELECT count(*) FROM passes", [])
    );
    if c.meta("api").as_deref() == Some("d3d12") {
        out!(
            "command lists: {} submissions, {} executions, {} recorded generations; queue order is retained separately from display order",
            c.count("SELECT count(*) FROM d12_submissions", []),
            c.count("SELECT count(*) FROM d12_executions", []),
            c.count("SELECT count(*) FROM d12_lists", [])
        );
    } else {
        out!(
            "command lists: {} spliced in ({} recorded the frame before), {} executed without a recording",
            n("lists_spliced"),
            n("lists_recorded_before"),
            n("lists_unrecorded")
        );
    }
    out!(
        "constants: {} distinct buffer contents, {} copies failed",
        c.count("SELECT count(*) FROM cb_data", []),
        n("cb_copy_failures")
    );

    let sources: Vec<(Option<String>, i64)> = c.rows(
        "SELECT content_source, count(*) FROM resources WHERE used = 1 GROUP BY content_source",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    let sources: Vec<String> = sources
        .iter()
        .map(|(s, n)| format!("{} {n}", s.as_deref().unwrap_or("none")))
        .collect();
    out!(
        "resources: {} live, {} used by the frame ({} written); contents as the frame began for {} ({}): {}",
        n("resources_live"),
        n("resources_used"),
        n("resources_written"),
        n("contents"),
        mib(n("content_bytes")),
        sources.join(", ")
    );
    let uploads = c.count("SELECT count(*) FROM resources WHERE upload IS NOT NULL", []);
    let stored = c.count("SELECT count(*) FROM resources WHERE upload_stored = 1", []);
    out!(
        "uploads: {uploads} live resources were created with data or updated whole ({stored} stored, {} hash only); \
         {} have no upload hash",
        uploads - stored,
        c.count("SELECT count(*) FROM resources WHERE upload IS NULL", [])
    );
    // Which passes have their targets: a capture can read back few of them (lists recorded before it was armed, a
    // spent budget), and pixel questions need them.
    let passes = c.count("SELECT count(*) FROM passes", []);
    let read = c.count(
        "SELECT count(DISTINCT d.pass) FROM targets t JOIN draws d ON d.n = t.draw WHERE t.blob IS NOT NULL",
        [],
    );
    let reach = if read == 0 {
        "none read back".to_string()
    } else {
        format!("read back in {read} of {passes} passes")
    };
    // D3D12 captures before the target counts went into meta have only the rows.
    match counts["targets"].as_u64() {
        Some(targets) => {
            let skipped = n("targets_skipped");
            let lost = if c.has_table("missing_targets") && skipped > 0 {
                let passes = c.count(
                    "SELECT count(DISTINCT d.pass) FROM missing_targets m JOIN draws d ON d.n = m.draw",
                    [],
                );
                let s = if passes == 1 { "" } else { "es" };
                format!(" (in {passes} pass{s}: `passes` names them and why)")
            } else {
                String::new()
            };
            // Pass ends a later list submitted with theirs continued: the pass's targets are read where it ends.
            let continued = c.meta_json("pass_ends")["continued"].as_u64().unwrap_or(0);
            let continued = match continued {
                0 => String::new(),
                1 => "; 1 pass continued in a later list, read where it ends".into(),
                n => format!("; {n} passes continued in later lists, read where they end"),
            };
            out!(
                "targets: {reach}: {targets} after their pass ({}), {skipped} skipped{lost}{continued}",
                mib(n("target_bytes")),
            )
        }
        None => out!(
            "targets: {reach}: {} after their pass (sizes and skips not counted in this capture)",
            c.count("SELECT count(*) FROM targets", [])
        ),
    }
    if let Some(id) = c.meta("presented") {
        let read = c.count(
            "SELECT count(*) FROM targets t JOIN bindings b ON b.draw = t.draw AND b.kind = 'rtv' \
             AND 'rt' || b.slot = t.target WHERE b.resource = ?1 AND t.blob IS NOT NULL",
            [&id],
        );
        out!(
            "presented: #{id}, {}",
            if read > 0 {
                "read back (`target presented`)"
            } else {
                "not read back"
            }
        );
    }
    out!(
        "shaders: {} used ({} with bytecode)",
        c.count("SELECT count(*) FROM shaders", []),
        c.count("SELECT count(*) FROM shaders WHERE blob IS NOT NULL", [])
    );
    out!("");

    // What is missing.
    let mut missing = Vec::new();
    let gaps: Vec<(String, Option<String>, i64)> =
        c.rows("SELECT what, detail, count FROM gaps ORDER BY what, rowid", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        });
    // One line a kind of gap, with the first details: an older D3D12 locum recorded one row per draw and resource.
    let mut by_what: Vec<(String, Vec<(String, i64)>)> = Vec::new();
    for (what, detail, count) in gaps {
        match by_what.last_mut() {
            Some((w, rows)) if *w == what => rows.push((detail.unwrap_or_default(), count)),
            _ => by_what.push((what, vec![(detail.unwrap_or_default(), count)])),
        }
    }
    for (what, rows) in by_what {
        if rows.len() <= 3 {
            for (detail, count) in rows {
                missing.push(format!("{what}: {detail} ({count})"));
            }
            continue;
        }
        let total: i64 = rows.iter().map(|r| r.1).sum();
        let mut first: Vec<&str> = Vec::new();
        for (detail, _) in &rows {
            if !first.contains(&detail.as_str()) {
                first.push(detail);
            }
            if first.len() == 2 {
                break;
            }
        }
        let counted = if total == rows.len() as i64 {
            format!("{total} records")
        } else {
            format!("{total} in {} records", rows.len())
        };
        missing.push(format!(
            "{what}: {counted} (`sql CAPTURE \"SELECT detail, count FROM gaps WHERE what = '{what}'\"` lists them), \
             the first: {}",
            first.join("; ")
        ));
    }
    // No valid root signature binds a register twice; a locum before 2026-11-04 let a D3D12 table range reach
    // descriptors of another type.
    let doubled = c.count(
        "SELECT count(*) FROM (SELECT 1 FROM bindings GROUP BY draw, stage, kind, slot, space HAVING count(*) > 1)",
        [],
    );
    if doubled > 0 {
        missing.push(format!(
            "bindings: {doubled} registers of draws are bound to more than one descriptor: an older locum projected \
             them, and only one of each is what the program read (`locum-cli upgrade` projects them again)"
        ));
    }
    let unread: Vec<(String, i64)> = c.rows(
        "SELECT coalesce(content_note, 'not read'), count(*) FROM resources WHERE used = 1 AND content IS NULL \
         GROUP BY content_note",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    for (why, count) in unread {
        missing.push(format!("contents of {count} used resources: {why}"));
    }
    let approximate: Vec<(String, i64)> = c.rows(
        "SELECT content_note, count(*) FROM resources WHERE content IS NOT NULL AND content_note IS NOT NULL \
         GROUP BY content_note",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    );
    for (why, count) in approximate {
        missing.push(format!("contents of {count} resources may be off: {why}"));
    }
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for sql in [
        "SELECT content FROM resources WHERE content IS NOT NULL",
        "SELECT upload FROM resources WHERE upload_stored = 1",
        "SELECT blob FROM targets WHERE blob IS NOT NULL",
        "SELECT data FROM events WHERE data IS NOT NULL",
        "SELECT blob FROM shaders WHERE blob IS NOT NULL",
    ] {
        keys.extend(c.rows(sql, [], |r| r.get::<_, String>(0)));
    }
    let absent = keys.iter().filter(|k| !c.blobs.has(k)).count();
    if absent > 0 {
        missing.push(format!(
            "{absent} of {} blobs it refers to are not in the store",
            keys.len()
        ));
    }
    let memory = c.meta_json("memory");
    match memory["path"].as_str() {
        Some(path) => {
            let p = std::path::Path::new(path);
            match std::fs::metadata(p) {
                Ok(m) => out!(
                    "memory: {path} ({}; the game waited {} ms for the snapshot)",
                    mib(m.len()),
                    memory["held_ms"]
                ),
                Err(_) if std::path::Path::new(&format!("{path}.part")).is_file() => missing.push(format!(
                    "memory: {path} is still being written, or its writer died"
                )),
                Err(_) => missing.push(format!("memory: {path} is gone")),
            }
        }
        // A failed dump is among the gaps.
        None if request["memory"] == true && memory["error"].is_null() => {
            missing.push("memory: not dumped (no dumper set: settings.txt `dumper`)".into())
        }
        None => {}
    }
    if missing.is_empty() {
        out!("missing: nothing");
    } else {
        out!("missing:");
        for m in missing {
            out!("  {m}");
        }
    }
    Ok(())
}
