//! What locum's own work costs: calls, time and bytes, counted where the work is done and reported (and reset) in
//! locum's log.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

pub struct Cost {
    calls: AtomicU64,
    nanos: AtomicU64,
    bytes: AtomicU64,
}

/// What a [`Cost`] counted since it was last taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counted {
    pub calls: u64,
    pub nanos: u64,
    pub bytes: u64,
}

impl Cost {
    pub const fn new() -> Cost {
        Cost {
            calls: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
        }
    }

    /// One call that began at `started` and handled `bytes`.
    pub fn add(&self, started: Instant, bytes: u64) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.nanos
            .fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// What was counted since the last take, and starts again from zero.
    pub fn take(&self) -> Counted {
        Counted {
            calls: self.calls.swap(0, Ordering::Relaxed),
            nanos: self.nanos.swap(0, Ordering::Relaxed),
            bytes: self.bytes.swap(0, Ordering::Relaxed),
        }
    }
}

impl Default for Cost {
    fn default() -> Cost {
        Cost::new()
    }
}

impl std::fmt::Display for Counted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} ({:.1} MiB) in {:.0} ms",
            self.calls,
            self.bytes as f64 / (1 << 20) as f64,
            self.nanos as f64 / 1e6
        )
    }
}
