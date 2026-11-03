//! Settings for the whole session, read once from `settings.txt` in locum's folder (one per line, `#` starts a
//! comment):
//!
//! - `uploads store` (default): every resource's initial data, and every update covering a whole subresource, is
//!   hashed and written to the blob store as it is created, so the game's own textures and meshes are there without a
//!   readback; `uploads hash` keeps the hashes only; `uploads off` neither.
//! - `blobs DIR`: the blob store (default `<locum's folder>/blobs`; the `LOCUM_BLOBS` variable overrides both).
//! - `dumper EXE`: `locum-cli.exe`, started to write a memory dump of the process when a capture asks for one.
//! - `dumps DIR`: where memory dumps go (default the capture's folder).

use std::path::PathBuf;
use std::sync::OnceLock;

use locum_capture::blobs::Blobs;

use crate::log::{folder, log};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Uploads {
    Store,
    Hash,
    Off,
}

pub struct Settings {
    pub uploads: Uploads,
    pub blobs: Blobs,
    pub dumper: Option<PathBuf>,
    pub dumps: Option<PathBuf>,
}

pub fn settings() -> &'static Settings {
    static SETTINGS: OnceLock<Settings> = OnceLock::new();
    SETTINGS.get_or_init(|| {
        let dir = folder().unwrap_or_default();
        let mut s = Settings {
            uploads: Uploads::Store,
            blobs: Blobs::new(dir.join("blobs")),
            dumper: None,
            dumps: None,
        };
        if let Ok(text) = std::fs::read_to_string(dir.join("settings.txt")) {
            for (n, line) in text.lines().enumerate() {
                let line = line.split('#').next().unwrap_or("").trim();
                let Some((key, value)) = line.split_once(char::is_whitespace) else {
                    if !line.is_empty() {
                        log!("settings.txt line {}: {line:?} has no value: ignored", n + 1);
                    }
                    continue;
                };
                let value = value.trim();
                match key {
                    "uploads" => match value {
                        "store" => s.uploads = Uploads::Store,
                        "hash" => s.uploads = Uploads::Hash,
                        "off" => s.uploads = Uploads::Off,
                        other => log!("settings.txt line {}: uploads {other:?}: ignored", n + 1),
                    },
                    "blobs" => s.blobs = Blobs::new(value),
                    "dumper" => s.dumper = Some(PathBuf::from(value)),
                    "dumps" => s.dumps = Some(PathBuf::from(value)),
                    other => log!("settings.txt line {}: unknown setting {other:?}: ignored", n + 1),
                }
            }
        }
        if let Some(dir) = std::env::var_os("LOCUM_BLOBS").filter(|d| !d.is_empty()) {
            s.blobs = Blobs::new(PathBuf::from(dir));
        }
        log!(
            "settings: uploads {:?}, blobs {}, dumper {}, dumps {}",
            s.uploads,
            s.blobs.root().display(),
            s.dumper
                .as_ref()
                .map_or("none".into(), |p| p.display().to_string()),
            s.dumps
                .as_ref()
                .map_or("capture folder".into(), |p| p.display().to_string())
        );
        s
    })
}
