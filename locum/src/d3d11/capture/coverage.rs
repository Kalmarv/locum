//! Whether every command of a deferred list was observed in one capture generation.
use std::sync::atomic::{AtomicU64, Ordering};

/// Zero: no commands since FinishCommandList. MAX: a command was outside this capture (or spanned captures).
#[derive(Default)]
pub struct Coverage(AtomicU64);

impl Coverage {
    pub fn command(&self) {
        let previous = self.0.load(Ordering::Relaxed);
        if previous == u64::MAX {
            return;
        }
        let epoch = super::epoch();
        let next = if epoch == 0 || (previous != 0 && previous != epoch) {
            u64::MAX
        } else {
            epoch
        };
        if next != previous {
            self.0.store(next, Ordering::Relaxed);
        }
    }

    /// Only called after a successful FinishCommandList, whether a capture is armed or not.
    pub fn finish(&self) -> bool {
        let previous = self.0.swap(0, Ordering::Relaxed);
        previous == 0 || (previous != u64::MAX && previous == super::epoch())
    }
}
