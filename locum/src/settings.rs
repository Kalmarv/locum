//! Settings for the whole session, read once from `settings.txt` in locum's folder (one per line, `#` starts a
//! comment):
//!
//! - `uploads store` (default): every resource's initial data, and every update covering a whole subresource, is
//!   hashed and written to the blob store as it is created, so the game's own textures and meshes are there without a
//!   readback; `uploads hash` keeps the hashes only; `uploads off` neither.
//! - `blobs DIR`: the blob store (default `<locum's folder>/blobs`; the `LOCUM_BLOBS` variable overrides both).
//! - `dumper EXE`: `locum-cli.exe`, started to write a memory dump of the process when a capture asks for one.
//! - `dumps DIR`: where memory dumps go (default the capture's folder).
//! - `crash-av-rva HEX[,HEX...]`: dump the first AV at a listed executable RVA, even if later handled;
//!   an unhandled-filter fallback also observes fatal AVs at any address.

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
    /// Direct3D 12 direct lists are instrumented from the process's start, not only from a capture's arming
    /// (`arm-early on`), so lists recorded before a request copy their passes' targets too. Every pass of every
    /// direct list then copies its targets in every frame, which costs GPU time and readback memory with no capture.
    pub arm_early: bool,
    pub crash_av_rvas: Vec<usize>,
    /// Readback memory one chunk of an armed frame's copies may use, and the size of the pieces a bigger buffer is
    /// copied in (`readback-chunk-mb`, 256 by default, 1 to 1024).
    pub readback_chunk: u64,
    /// Readback memory an armed frame's copies may hold at once, waiting for the GPU and the collector
    /// (`readback-budget-mb`, 1024 by default, 64 to 65536). Copies behind a wait the application signals later hold
    /// theirs until it does: a frame that first uses more than this behind one gives up the rest after a while.
    pub readback_budget: u64,
    /// The sidecar a tool beside the game may write (`sidecar`, `sidecar.txt` in locum's folder by default; see
    /// `locum_capture::sidecar`), and the bytes of memory its regions may copy into one capture (`sidecar-region-mb`,
    /// 4096 by default).
    pub sidecar: PathBuf,
    pub sidecar_regions: u64,
    /// Direct3D 12: a submission whose draws or dispatches run with a root signature that indexes the CBV, SRV and
    /// UAV heap directly (`ResourceDescriptorHeap`) gets first-use copies of every resource the heap's slots name
    /// (`heap-indexed on`, the default; `off` records them as not copied), at most `heap-indexed-mb` of them in a
    /// frame (32768 by default).
    pub heap_indexed: bool,
    pub heap_indexed_bytes: u64,
    /// Optional dispatch-scoped compute buffer probes (`dispatch-probe <shader> [binding=srv:0] [row-stride=0x200] [row=0] [desc-offset=0x120] [bytes=4096] [alias-map=PATH]`).
    pub dispatch_probes: Vec<String>,
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
            arm_early: false,
            crash_av_rvas: Vec::new(),
            readback_chunk: 256 << 20,
            readback_budget: 1 << 30,
            sidecar: dir.join("sidecar.txt"),
            sidecar_regions: 4 << 30,
            heap_indexed: true,
            heap_indexed_bytes: 32768 << 20,
            dispatch_probes: Vec::new(),
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
                    "arm-early" => match value {
                        "on" => s.arm_early = true,
                        "off" => s.arm_early = false,
                        other => log!(
                            "settings.txt line {}: arm-early {other:?} is not on or off: ignored",
                            n + 1
                        ),
                    },
                    "crash-av-rva" => match crash_rvas(value) {
                        Ok(rvas) => s.crash_av_rvas = rvas,
                        Err(_) => log!(
                            "settings.txt line {}: invalid crash-av-rva {value:?}: ignored",
                            n + 1
                        ),
                    },
                    "readback-chunk-mb" => match value.parse::<u64>() {
                        Ok(mb @ 1..=1024) => s.readback_chunk = mb << 20,
                        _ => log!(
                            "settings.txt line {}: readback-chunk-mb {value:?} is not 1 to 1024: ignored",
                            n + 1
                        ),
                    },
                    "readback-budget-mb" => match value.parse::<u64>() {
                        Ok(mb @ 64..=65536) => s.readback_budget = mb << 20,
                        _ => log!(
                            "settings.txt line {}: readback-budget-mb {value:?} is not 64 to 65536: ignored",
                            n + 1
                        ),
                    },
                    "sidecar" => s.sidecar = PathBuf::from(value),
                    "sidecar-region-mb" => match value.parse::<u64>() {
                        Ok(mb) => s.sidecar_regions = mb << 20,
                        _ => log!(
                            "settings.txt line {}: sidecar-region-mb {value:?} is not a number: ignored",
                            n + 1
                        ),
                    },
                    "heap-indexed" => match value {
                        "on" => s.heap_indexed = true,
                        "off" => s.heap_indexed = false,
                        other => log!(
                            "settings.txt line {}: heap-indexed {other:?} is not on or off: ignored",
                            n + 1
                        ),
                    },
                    "heap-indexed-mb" => match value.parse::<u64>() {
                        Ok(mb) => s.heap_indexed_bytes = mb << 20,
                        _ => log!(
                            "settings.txt line {}: heap-indexed-mb {value:?} is not a number: ignored",
                            n + 1
                        ),
                    },
                    "dispatch-probe" => s.dispatch_probes.push(value.to_string()),
                    other => log!("settings.txt line {}: unknown setting {other:?}: ignored", n + 1),
                }
            }
        }
        if s.arm_early {
            log!("settings: arm-early on: Direct3D 12 direct lists are instrumented from the start");
        }
        if let Some(dir) = std::env::var_os("LOCUM_BLOBS").filter(|d| !d.is_empty()) {
            s.blobs = Blobs::new(PathBuf::from(dir));
        }
        log!(
            "settings: uploads {:?}, blobs {}, readback chunks of {} MiB in {} MiB, dumper {}, dumps {}",
            s.uploads,
            s.blobs.root().display(),
            s.readback_chunk >> 20,
            s.readback_budget >> 20,
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

fn crash_rvas(value: &str) -> Result<Vec<usize>, std::num::ParseIntError> {
    value
        .split(',')
        .map(|value| {
            let value = value.trim();
            usize::from_str_radix(
                value
                    .strip_prefix("0x")
                    .or_else(|| value.strip_prefix("0X"))
                    .unwrap_or(value),
                16,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn crash_rva_list_validates_every_entry() {
        assert_eq!(
            super::crash_rvas("0x1f64f4d, 0X1F64F56, 1f1eb2d").unwrap(),
            [0x1f64f4d, 0x1f64f56, 0x1f1eb2d]
        );
        for invalid in ["", "0x", "0X", "12,", ",12", "12,nope", "0x0x12"] {
            assert!(super::crash_rvas(invalid).is_err(), "{invalid}");
        }
    }
}
