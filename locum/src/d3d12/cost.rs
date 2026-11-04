//! Per-thread counters avoid moving one cache line between every rendering worker.
use locum_capture::cost::{Cost, Counted};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

#[repr(align(64))]
struct ThreadCounters([Cost; 7]);
fn registry() -> &'static Mutex<Vec<Arc<ThreadCounters>>> {
    static REGISTRY: OnceLock<Mutex<Vec<Arc<ThreadCounters>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}
thread_local! {
    static LOCAL: Arc<ThreadCounters> = {
        let counters=Arc::new(ThreadCounters(std::array::from_fn(|_|Cost::new())));
        registry().lock().unwrap_or_else(|e|e.into_inner()).push(counters.clone());
        counters
    };
}
pub struct Counter(usize);
impl Counter {
    pub const fn new(index: usize) -> Self {
        Self(index)
    }
    pub fn add(&self, start: Instant, bytes: u64) {
        LOCAL.with(|local| local.0[self.0].add(start, bytes));
    }
}
/// Drain every category before retiring threads that had already exited. An active
/// thread can finish during this report; keep it until the next drain in that case.
pub fn take() -> [Counted; 7] {
    let mut threads = registry().lock().unwrap_or_else(|e| e.into_inner());
    let mut result = [Counted::default(); 7];
    threads.retain(|thread| {
        let active = Arc::strong_count(thread) > 1;
        for (out, counter) in result.iter_mut().zip(&thread.0) {
            let c = counter.take();
            out.calls += c.calls;
            out.nanos += c.nanos;
            out.bytes += c.bytes;
        }
        active
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exited_threads_are_drained_before_retirement() {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..1000 {
                        Counter::new(6).add(Instant::now(), 1);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(take()[6].bytes, 8000);
        assert_eq!(take()[6].bytes, 0);
    }
}
