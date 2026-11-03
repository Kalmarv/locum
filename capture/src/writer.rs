//! A background thread for disk writes, so the game's threads hand off blobs and databases instead of waiting on
//! the disk. Jobs run in the order they were given. Blobs in flight are limited to [`BUDGET`] bytes: a caller that
//! must not block (a resource being created) has its blob dropped when the budget is spent, one that can (a capture
//! being written) waits. A key already taken for a store is not taken again, so callers hand over whatever they have
//! without asking the disk first.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Sender, channel};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};

use crate::blobs::Blobs;
use crate::cost::Cost;

/// Blobs written (bytes, time on the writer thread), blobs not queued because their key was taken before, blobs
/// dropped because the budget was spent, and blobs whose caller waited for the budget (time waited).
pub static WRITTEN: Cost = Cost::new();
pub static KNOWN: Cost = Cost::new();
pub static DROPPED: Cost = Cost::new();
pub static WAITED: Cost = Cost::new();

/// Keys queued or written, by store root.
static TAKEN: Mutex<Option<HashMap<PathBuf, HashSet<String>>>> = Mutex::new(None);

fn taken() -> MutexGuard<'static, Option<HashMap<PathBuf, HashSet<String>>>> {
    TAKEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bytes of blobs that may wait to be written at once.
pub const BUDGET: u64 = 1 << 30;

enum Job {
    Blob(Blobs, String, Vec<u8>),
    Run(Box<dyn FnOnce() + Send>),
}

pub struct Writer {
    tx: Mutex<Sender<Job>>,
    /// Bytes queued, and jobs queued.
    pending: Mutex<(u64, u64)>,
    changed: Condvar,
}

fn writer() -> &'static Writer {
    static WRITER: OnceLock<Writer> = OnceLock::new();
    WRITER.get_or_init(|| {
        let (tx, rx) = channel::<Job>();
        std::thread::Builder::new()
            .name("locum writer".into())
            .spawn(move || {
                for job in rx {
                    let bytes = match job {
                        Job::Blob(blobs, key, data) => {
                            let started = std::time::Instant::now();
                            if blobs.put(&key, &data).is_err() {
                                // Not there after all: the next caller with it queues it again.
                                if let Some(t) = taken().as_mut().and_then(|t| t.get_mut(blobs.root())) {
                                    t.remove(&key);
                                }
                            }
                            WRITTEN.add(started, data.len() as u64);
                            data.len() as u64
                        }
                        Job::Run(f) => {
                            f();
                            0
                        }
                    };
                    let w = writer();
                    let mut p = w.pending.lock().unwrap_or_else(|e| e.into_inner());
                    p.0 -= bytes;
                    p.1 -= 1;
                    w.changed.notify_all();
                }
            })
            .expect("start the writer thread");
        Writer {
            tx: Mutex::new(tx),
            pending: Mutex::new((0, 0)),
            changed: Condvar::new(),
        }
    })
}

fn send(job: Job, bytes: u64) {
    let w = writer();
    {
        let mut p = w.pending.lock().unwrap_or_else(|e| e.into_inner());
        p.0 += bytes;
        p.1 += 1;
    }
    let _ = w.tx.lock().unwrap_or_else(|e| e.into_inner()).send(job);
}

/// Queues a blob for writing, unless its key was taken before. With `wait`, blocks while the budget is spent;
/// without, drops the blob and returns false instead. Returns whether the blob is (or will be) in the store.
pub fn blob(blobs: &Blobs, key: String, data: Vec<u8>, wait: bool) -> bool {
    let started = std::time::Instant::now();
    if taken()
        .as_ref()
        .and_then(|t| t.get(blobs.root()))
        .is_some_and(|t| t.contains(&key))
    {
        KNOWN.add(started, data.len() as u64);
        return true;
    }
    let w = writer();
    let bytes = data.len() as u64;
    {
        let mut p = w.pending.lock().unwrap_or_else(|e| e.into_inner());
        if p.0 > 0 && p.0 + bytes > BUDGET {
            if !wait {
                DROPPED.add(std::time::Instant::now(), bytes);
                return false;
            }
            let waiting = std::time::Instant::now();
            while p.0 > 0 && p.0 + bytes > BUDGET {
                p = w.changed.wait(p).unwrap_or_else(|e| e.into_inner());
            }
            WAITED.add(waiting, bytes);
        }
    }
    taken()
        .get_or_insert_with(HashMap::new)
        .entry(blobs.root().to_path_buf())
        .or_default()
        .insert(key.clone());
    send(Job::Blob(blobs.clone(), key, data), bytes);
    true
}

/// Queues `f` to run on the writer thread after everything queued before it.
pub fn run(f: impl FnOnce() + Send + 'static) {
    send(Job::Run(Box::new(f)), 0);
}

/// Waits until everything queued so far is written.
pub fn flush() {
    let w = writer();
    let mut p = w.pending.lock().unwrap_or_else(|e| e.into_inner());
    while p.1 > 0 {
        p = w.changed.wait(p).unwrap_or_else(|e| e.into_inner());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_is_taken_once() {
        let dir = std::env::temp_dir().join(format!("locum-writer-test-{}", std::process::id()));
        let blobs = Blobs::new(&dir);
        let key = crate::hash::key(b"once");
        assert!(blob(&blobs, key.clone(), b"once".to_vec(), true));
        flush();
        assert!(blobs.has(&key));
        // Gone from the disk behind the writer's back: the second hand-over is not queued, so it stays gone.
        std::fs::remove_file(blobs.path(&key)).unwrap();
        assert!(blob(&blobs, key.clone(), b"once".to_vec(), true));
        flush();
        assert!(!blobs.has(&key));
        // Another store takes it afresh.
        let other = Blobs::new(dir.join("other"));
        assert!(blob(&other, key.clone(), b"once".to_vec(), true));
        flush();
        assert!(other.has(&key));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
