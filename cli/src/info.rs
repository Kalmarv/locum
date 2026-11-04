//! `captures` and `info`: what captures there are, and what one holds and lacks.

use std::collections::BTreeSet;
use std::path::Path;

use crate::describe::mib;
use crate::open::{self, Capture};
use crate::out;

pub fn captures(dir: &Path, blobs: Option<&Path>) -> Result<(), String> {
    let found = open::list(dir);
    if found.is_empty() {
        out!("no captures in {}", dir.join("captures").display());
        return Ok(());
    }
    for path in found {
        let name = path
            .parent()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match open::open(&path.display().to_string(), blobs) {
            Ok(c) => {
                let counts = c.meta_json("counts");
                let exe = c.meta("exe").unwrap_or_default();
                let exe = exe.rsplit(['\\', '/']).next().unwrap_or(&exe).to_string();
                out!(
                    "{name:<24} frame {:<7} {:>6} draws {:>7} events  {:>9} contents  {exe}",
                    c.meta("frame").unwrap_or_default(),
                    counts["draws"],
                    counts["events"],
                    mib(counts["content_bytes"].as_u64().unwrap_or(0))
                );
            }
            Err(e) => out!("{name:<24} {e}"),
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
    out!("  request: {request}");
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
    // D3D12 captures before the target counts went into meta have only the rows.
    match counts["targets"].as_u64() {
        Some(targets) => out!(
            "targets: {targets} read back after their pass ({}), {} skipped",
            mib(n("target_bytes")),
            n("targets_skipped")
        ),
        None => out!(
            "targets: {} read back after their pass (sizes and skips not counted in this capture)",
            c.count("SELECT count(*) FROM targets", [])
        ),
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
        c.rows("SELECT what, detail, count FROM gaps ORDER BY what", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        });
    for (what, detail, count) in gaps {
        missing.push(format!("{what}: {} ({count})", detail.unwrap_or_default()));
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
