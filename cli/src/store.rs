//! `store`: a blob store shared by every run of a game. `info` says what it holds and which captures use it; `merge`
//! moves the captures under a folder onto it, bringing the blobs they name from the stores they used and deleting
//! those once everything has arrived; `collect` drops the blobs no listed capture names, which is how deleting a run
//! gives its space back.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use locum_capture::blobs::Blobs;
use rusqlite::{Connection, OpenFlags};

use crate::{open, out};

fn gib(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / (1u64 << 30) as f64)
}

fn absolute(p: &Path) -> Result<PathBuf, String> {
    std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()))
}

pub fn info(root: &Path) -> Result<(), String> {
    let blobs = Blobs::new(root);
    let entries = blobs.entries().map_err(|e| e.to_string())?;
    let packs: Vec<u64> = std::fs::read_dir(root.join("packs"))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|f| f.file_name().to_string_lossy().ends_with(".pack"))
        .filter_map(|f| f.metadata().ok().map(|m| m.len()))
        .collect();
    out!(
        "{}: {} blobs, {}; {} packs, {}",
        root.display(),
        entries.len(),
        gib(entries.iter().map(|e| e.size).sum()),
        packs.len(),
        gib(packs.iter().sum())
    );
    let captures = blobs.captures().map_err(|e| e.to_string())?;
    let gone = captures.iter().filter(|c| !c.is_file()).count();
    out!(
        "{} captures listed as using it ({gone} of them deleted since: `store collect` forgets them)",
        captures.len()
    );
    for c in &captures {
        out!(
            "  {}{}",
            c.display(),
            if c.is_file() { "" } else { "  (deleted)" }
        );
    }
    Ok(())
}

pub fn merge(folder: &Path, into: &Path) -> Result<(), String> {
    let found = open::list(folder);
    if found.is_empty() {
        return Err(format!("no captures under {}", folder.display()));
    }
    let (folder, into) = (absolute(folder)?, absolute(into)?);
    // The stores the captures use now, and the captures using each.
    let mut sources: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for capture in &found {
        let db = Connection::open_with_flags(capture, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("{}: {e}", capture.display()))?;
        let named: String = db
            .query_row("SELECT value FROM meta WHERE key = 'blobs'", [], |r| r.get(0))
            .map_err(|e| format!("{}: the blob store it uses: {e}", capture.display()))?;
        sources
            .entry(absolute(Path::new(&named))?)
            .or_default()
            .push(capture.clone());
    }
    let shared = Blobs::new(&into);
    for (source, captures) in &sources {
        if *source == into {
            out!("{} captures use {} already", captures.len(), into.display());
            continue;
        }
        if !source.is_dir() {
            return Err(format!(
                "{} captures use {}, which is not there: nothing moved",
                captures.len(),
                source.display()
            ));
        }
        let done = shared
            .copy_from(&Blobs::new(source))
            .map_err(|e| format!("copying {} into {}: {e}", source.display(), into.display()))?;
        out!(
            "{}: {} blobs copied ({}), {} there already; all {} arrived",
            source.display(),
            done.copied,
            gib(done.copied_bytes),
            done.present,
            done.copied + done.present
        );
    }
    let named = into.display().to_string();
    for capture in &found {
        // Listed first: from here a collection of the shared store keeps what the capture names.
        shared
            .register(capture)
            .map_err(|e| format!("listing {}: {e}", capture.display()))?;
        let db = Connection::open(capture).map_err(|e| format!("{}: {e}", capture.display()))?;
        db.busy_timeout(std::time::Duration::from_secs(30))
            .map_err(|e| e.to_string())?;
        db.execute("UPDATE meta SET value = ?1 WHERE key = 'blobs'", [&named])
            .map_err(|e| format!("{}: {e}", capture.display()))?;
    }
    out!("{} captures now use {}", found.len(), into.display());
    for source in sources.keys().filter(|s| **s != into) {
        if !source.starts_with(&folder) {
            out!(
                "{} left: it is outside {}, and other captures may use it",
                source.display(),
                folder.display()
            );
            continue;
        }
        let old = Blobs::new(source);
        old.close().map_err(|e| e.to_string())?;
        std::fs::remove_dir_all(source).map_err(|e| format!("deleting {}: {e}", source.display()))?;
        out!("{} deleted", source.display());
    }
    Ok(())
}

pub fn collect(root: &Path, count_only: bool) -> Result<(), String> {
    let blobs = Blobs::new(root);
    let captures = blobs.captures().map_err(|e| e.to_string())?;
    if captures.is_empty() && !blobs.entries().map_err(|e| e.to_string())?.is_empty() {
        return Err(format!(
            "no captures are listed as using {}: collecting would drop every blob (`store merge` lists the captures \
             under a folder)",
            root.display()
        ));
    }
    let mut keep = HashSet::new();
    let (mut live, mut gone) = (0, 0);
    for capture in &captures {
        if !capture.is_file() {
            gone += 1;
            if !count_only {
                blobs.unregister(capture).map_err(|e| e.to_string())?;
            }
            continue;
        }
        let db = Connection::open_with_flags(capture, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("{}: {e}: nothing collected", capture.display()))?;
        locum_capture::db::keys(&db, &mut keep)
            .map_err(|e| format!("{}: {e}: nothing collected", capture.display()))?;
        live += 1;
    }
    out!(
        "{live} captures name {} keys; {gone} listed captures are deleted{}",
        keep.len(),
        if count_only { "" } else { " (forgotten)" }
    );
    let done = blobs
        .collect(&|k| keep.contains(k), count_only)
        .map_err(|e| e.to_string())?;
    let verb = if count_only { "would be" } else { "were" };
    out!("kept {} blobs ({})", done.kept, gib(done.kept_bytes));
    out!(
        "{} blobs ({}) {verb} dropped; {} packs {verb} rewritten",
        done.dropped,
        gib(done.dropped_bytes),
        done.packs_rewritten
    );
    if done.orphan_packs > 0 || done.orphan_bytes > 0 {
        out!(
            "{} packs and second copies no entry names ({}) {verb} deleted",
            done.orphan_packs,
            gib(done.orphan_bytes)
        );
    }
    if done.files_packed > 0 {
        out!("{} one-file blobs {verb} moved into packs", done.files_packed);
    }
    if done.missing > 0 {
        out!(
            "{} index entries name packs that are not there: left as they are",
            done.missing
        );
    }
    Ok(())
}
