//! Finding and opening a capture, and the blob store its keys refer to.

use std::path::{Path, PathBuf};

use crate::join::Names;
use locum_capture::blobs::Blobs;
use rusqlite::{Connection, OpenFlags, Params, Row};

/// locum's folder: `LOCUM_DIR`, or `%LOCALAPPDATA%\locum`.
pub fn locum_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LOCUM_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(d);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(|d| PathBuf::from(d).join("locum"))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub struct Capture {
    pub db: Connection,
    pub path: PathBuf,
    pub blobs: Blobs,
    /// What its resources were made from, by the indexes given.
    pub names: Names,
}

/// The captures under `dir/captures`, newest first.
pub fn list(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir.join("captures"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path().join("capture.db"))
        .filter(|p| p.is_file())
        .map(|p| {
            (
                p.metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH),
                p,
            )
        })
        .collect();
    found.sort_by_key(|f| std::cmp::Reverse(f.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// The capture named by `arg`: a `capture.db`, a folder holding one, a capture's name under locum's folder, or
/// `last` for the newest there. Blobs come from `blobs`, else `LOCUM_BLOBS`, else the store the capture names, else
/// `blobs` beside the `captures` folder.
pub fn open(arg: &str, blobs: Option<&Path>) -> Result<Capture, String> {
    let given = PathBuf::from(arg);
    let path = if given.is_file() {
        given
    } else if given.join("capture.db").is_file() {
        given.join("capture.db")
    } else if arg == "last" {
        list(&locum_dir())
            .into_iter()
            .next()
            .ok_or_else(|| format!("no captures in {}", locum_dir().join("captures").display()))?
    } else {
        let named = locum_dir().join("captures").join(arg).join("capture.db");
        if given.join("draws.jsonl").is_file() {
            return Err(format!(
                "{arg} holds a capture in locum's first format (draws.jsonl, from before capture.db), which this \
                 CLI does not read; capture the frame again"
            ));
        }
        if !named.is_file() {
            return Err(format!(
                "{arg} is not a capture.db, a folder with one, or a capture in {}",
                locum_dir().join("captures").display()
            ));
        }
        named
    };
    let db = Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let named: Option<String> = db
        .query_row("SELECT value FROM meta WHERE key = 'blobs'", [], |r| r.get(0))
        .ok();
    let root = blobs
        .map(Path::to_path_buf)
        .or_else(|| {
            std::env::var_os("LOCUM_BLOBS")
                .filter(|d| !d.is_empty())
                .map(PathBuf::from)
        })
        .or_else(|| named.map(PathBuf::from).filter(|p| p.is_dir()))
        .or_else(|| {
            path.parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .map(|d| d.join("blobs"))
        })
        .unwrap_or_else(|| PathBuf::from("blobs"));
    Ok(Capture {
        db,
        path,
        blobs: Blobs::new(root),
        names: Names::default(),
    })
}

impl Capture {
    /// The capture, naming its resources from the indexes at `paths`.
    pub fn named(mut self, paths: &[PathBuf]) -> Result<Capture, String> {
        self.names = Names::open(paths)?;
        Ok(self)
    }

    pub fn meta(&self, key: &str) -> Option<String> {
        self.db
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .ok()
    }

    /// The meta value `key` as JSON (`counts`, `request`).
    pub fn meta_json(&self, key: &str) -> serde_json::Value {
        self.meta(key)
            .and_then(|v| serde_json::from_str(&v).ok())
            .unwrap_or_default()
    }

    pub fn blob(&self, key: &str) -> Result<Vec<u8>, String> {
        self.blobs
            .get(key)
            .map_err(|e| format!("blob {key} is not in {} ({e})", self.blobs.root().display()))
    }

    /// Every row of `sql`, each through `f`.
    pub fn rows<T, P: Params>(
        &self,
        sql: &str,
        params: P,
        f: impl FnMut(&Row) -> rusqlite::Result<T>,
    ) -> Vec<T> {
        let mut q = match self.db.prepare(sql) {
            Ok(q) => q,
            Err(e) => panic!("{sql}: {e}"),
        };
        q.query_map(params, f)
            .and_then(Iterator::collect)
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    /// The one row of `sql`, if there is one.
    pub fn row<T, P: Params>(
        &self,
        sql: &str,
        params: P,
        f: impl FnOnce(&Row) -> rusqlite::Result<T>,
    ) -> Option<T> {
        match self.db.query_row(sql, params, f) {
            Ok(v) => Some(v),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => panic!("{sql}: {e}"),
        }
    }

    pub fn count<P: Params>(&self, sql: &str, params: P) -> i64 {
        self.row(sql, params, |r| r.get(0)).unwrap_or(0)
    }
}
