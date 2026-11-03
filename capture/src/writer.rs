//! A background thread for disk writes, so the game's threads hand off blobs and databases instead of waiting on
//! the disk. Jobs run in the order they were given. Blobs in flight are limited to [`BUDGET`] bytes: a caller that
//! must not block (a resource being created) has its blob dropped when the budget is spent, one that can (a capture
//! being written) waits.

use std::sync::mpsc::{Sender, channel};
use std::sync::{Condvar, Mutex, OnceLock};

use crate::blobs::Blobs;

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
                            let _ = blobs.put(&key, &data);
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

/// Queues a blob for writing. With `wait`, blocks while the budget is spent; without, drops the blob and returns
/// false instead.
pub fn blob(blobs: &Blobs, key: String, data: Vec<u8>, wait: bool) -> bool {
    let w = writer();
    let bytes = data.len() as u64;
    {
        let mut p = w.pending.lock().unwrap_or_else(|e| e.into_inner());
        while p.0 > 0 && p.0 + bytes > BUDGET {
            if !wait {
                return false;
            }
            p = w.changed.wait(p).unwrap_or_else(|e| e.into_inner());
        }
    }
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
