//! A memory dump of the process at a captured frame. A process writing a dump of itself can deadlock on locks its
//! other threads hold, so another process writes it: `locum-cli dump` (settings.txt `dumper`). The game waits in
//! Present only while that process takes a snapshot of this one (a copy-on-write clone); the dump is written from the
//! snapshot afterwards, while the game runs on.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::HSTRING;

use crate::settings::settings;

/// What became of a dump.
pub struct Dumped {
    /// Where the dump is being written (it is `<path>.part` until complete).
    pub path: PathBuf,
    /// How long the game waited for the snapshot.
    pub held: Duration,
}

/// Starts a dump of this process named `name`, in the dumps folder (settings.txt `dumps`) or `dir`, and waits until
/// the snapshot is taken.
pub fn dump(name: &str, dir: &Path) -> Result<Dumped, String> {
    let dumper = settings()
        .dumper
        .clone()
        .ok_or("no dumper set (settings.txt: dumper PATH-TO-locum-cli.exe)")?;
    let dir = settings().dumps.clone().unwrap_or_else(|| dir.to_path_buf());
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("{name}.dmp"));
    let pid = std::process::id();
    let event_name = format!("Local\\locum-dump-{pid}-{name}");
    let event = unsafe { CreateEventW(None, true, false, &HSTRING::from(event_name.as_str())) }
        .map_err(|e| format!("CreateEventW: {e}"))?;
    let started = Instant::now();
    let child = Command::new(&dumper)
        .arg("dump")
        .arg("--pid")
        .arg(pid.to_string())
        .arg("--out")
        .arg(&path)
        .arg("--signal")
        .arg(&event_name)
        .spawn();
    let result = match child {
        Err(e) => Err(format!("{}: {e}", dumper.display())),
        Ok(mut child) => loop {
            if unsafe { WaitForSingleObject(event, 50) } == WAIT_OBJECT_0 {
                break Ok(Dumped {
                    path,
                    held: started.elapsed(),
                });
            }
            if let Ok(Some(status)) = child.try_wait() {
                break Err(format!("the dumper ended before its snapshot: {status}"));
            }
            if started.elapsed() > Duration::from_secs(120) {
                break Err("no snapshot after 120 s".into());
            }
        },
    };
    let _ = unsafe { CloseHandle(event) };
    result
}
