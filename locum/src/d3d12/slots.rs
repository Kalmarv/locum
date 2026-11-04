//! Address metadata and descriptor slots, independent of capture/object bookkeeping.
//! Copies share immutable descriptor payloads; only a command/capture needs JSON.
use super::ranges::Ranges;
use locum_capture::d3d12 as model;
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

#[derive(Clone)]
struct Payload {
    kind: &'static str,
    resource: u64,
    counter: u64,
    description: super::descriptors::Description,
}
#[derive(Clone, Default)]
struct Slot {
    version: u64,
    payload: Option<Payload>,
}
#[derive(Default)]
struct Entry {
    version: AtomicU64,
    value: Mutex<Slot>,
}
#[derive(Clone, Default)]
struct CachedSlot {
    heap: u64,
    slot: usize,
    value: Slot,
}
struct Heap {
    id: u64,
    cpu: u64,
    gpu: u64,
    stride: u32,
    slots: Box<[Entry]>,
    initialized: AtomicUsize,
}
impl Heap {
    fn set(&self, slot: usize, payload: Option<Payload>) {
        let entry = &self.slots[slot];
        let mut dest = entry.value.lock().unwrap_or_else(|e| e.into_inner());
        if dest.payload.is_none() && payload.is_some() {
            self.initialized.fetch_add(1, Ordering::Relaxed);
        }
        if dest.payload.is_some() && payload.is_none() {
            self.initialized.fetch_sub(1, Ordering::Relaxed);
        }
        *dest = Slot {
            version: next_version(),
            payload,
        };
        entry.version.store(dest.version, Ordering::Release);
    }
    fn get(&self, slot: usize) -> Slot {
        // Most copies repeatedly read stable CPU source descriptors. A bounded
        // cache reads only the publication stamp until the source changes; no
        // shared lock/cache-line write is needed to read an unchanged definition.
        thread_local! {static SOURCES: RefCell<Vec<CachedSlot>> = RefCell::new(vec![CachedSlot::default();1024]);}
        let entry = &self.slots[slot];
        let version = entry.version.load(Ordering::Acquire);
        SOURCES.with(|sources| {
            let mut sources = sources.borrow_mut();
            let cached = &mut sources[((self.id as usize).wrapping_mul(31) ^ slot) & 1023];
            if cached.heap != self.id || cached.slot != slot || cached.value.version != version {
                *cached = CachedSlot {
                    heap: self.id,
                    slot,
                    value: entry.value.lock().unwrap_or_else(|e| e.into_inner()).clone(),
                };
            }
            cached.value.clone()
        })
    }
    fn definition(&self, slot: usize) -> Option<model::Descriptor> {
        let s = self.get(slot);
        let p = s.payload?;
        Some(model::Descriptor {
            id: s.version,
            heap: self.id,
            slot: slot as u32,
            kind: p.kind,
            resource: p.resource,
            counter: p.counter,
            description: p.description.json(),
        })
    }
}
fn next_version() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    thread_local! {static BLOCK: Cell<(u64,u64)> = const {Cell::new((0,0))};}
    BLOCK.with(|block| {
        let (mut next, mut end) = block.get();
        if next == end {
            next = NEXT.fetch_add(4096, Ordering::Relaxed);
            end = next + 4096;
        }
        block.set((next + 1, end));
        next
    })
}

#[derive(Clone, Default)]
struct Index {
    addresses: HashMap<u64, (u64, u64)>,
    va: Ranges,
    cpu: Ranges,
    gpu: Ranges,
    heaps: BTreeMap<u64, Arc<Heap>>,
}
impl Index {
    fn locations(&self, value: u64, gpu: bool) -> impl Iterator<Item = (&Heap, usize)> {
        (if gpu { &self.gpu } else { &self.cpu })
            .get(value)
            .filter_map(move |id| {
                let heap = self.heaps.get(&id)?;
                let base = if gpu { heap.gpu } else { heap.cpu };
                let offset = value.checked_sub(base)?;
                (base != 0
                    && heap.stride != 0
                    && offset.is_multiple_of(heap.stride as u64)
                    && offset / (heap.stride as u64) < heap.slots.len() as u64)
                    .then(|| (heap.as_ref(), (offset / heap.stride as u64) as usize))
            })
    }
    fn unique(&self, value: u64, gpu: bool) -> Option<(&Heap, usize)> {
        let mut matches = self.locations(value, gpu);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    }
}
struct Catalog {
    current: Mutex<Arc<Index>>,
    revision: AtomicU64,
}
fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| Catalog {
        current: Mutex::new(Arc::new(Index::default())),
        revision: AtomicU64::new(0),
    })
}
// A lookup loads one revision and borrows a thread-local Arc. Metadata publication
// is rare; slot values remain shared and independently synchronized in each Heap.
fn read<R>(f: impl FnOnce(&Index) -> R) -> R {
    thread_local! {static CACHE: RefCell<(u64,Option<Arc<Index>>)> = const {RefCell::new((0,None))};}
    let c = catalog();
    let revision = c.revision.load(Ordering::Acquire);
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.0 != revision || cache.1.is_none() {
            cache.1 = Some(c.current.lock().unwrap_or_else(|e| e.into_inner()).clone());
            cache.0 = revision;
        }
        f(cache.1.as_ref().unwrap())
    })
}
fn update(f: impl FnOnce(&mut Index)) {
    let c = catalog();
    let mut current = c.current.lock().unwrap_or_else(|e| e.into_inner());
    f(Arc::make_mut(&mut current));
    c.revision.fetch_add(1, Ordering::Release);
}
pub fn register_address(id: u64, address: u64, bytes: u64) {
    update(|index| {
        index.addresses.insert(id, (address, bytes));
        index.va.insert(id, address, bytes);
    });
}
pub fn register_heap(id: u64, cpu: u64, gpu: u64, count: u32, stride: u32) {
    let heap = Arc::new(Heap {
        id,
        cpu,
        gpu,
        stride,
        slots: (0..count).map(|_| Entry::default()).collect(),
        initialized: AtomicUsize::new(0),
    });
    update(|index| {
        index.cpu.insert(id, cpu, count as u64 * stride as u64);
        if gpu != 0 {
            index.gpu.insert(id, gpu, count as u64 * stride as u64);
        }
        index.heaps.insert(id, heap);
    });
}
pub fn destroy(id: u64) {
    // Most destroyed D3D12 objects are neither a heap nor a buffer range.
    if !read(|index| index.heaps.contains_key(&id) || index.addresses.contains_key(&id)) {
        return;
    }
    update(|index| {
        index.addresses.remove(&id);
        index.va.remove(id);
        if index.heaps.remove(&id).is_some() {
            index.cpu.remove(id);
            index.gpu.remove(id);
        }
    });
}
pub fn address(value: u64) -> Value {
    read(|index| {
        let mut matches = index.va.get(value).filter_map(|id| {
            let &(base, size) = index.addresses.get(&id)?;
            value
                .checked_sub(base)
                .filter(|&o| o < size)
                .map(|offset| (id, offset))
        });
        let first = matches.next();
        let second = matches.next();
        if let (Some((resource, offset)), None) = (first, second) {
            return json!({"resource":resource,"offset":offset});
        }
        let candidates: Vec<_> = first
            .into_iter()
            .chain(second)
            .chain(matches)
            .map(|(resource, offset)| json!({"resource":resource,"offset":offset}))
            .collect();
        json!({"candidates":candidates,"diagnostic":"GPU address is not uniquely associated with a live resource"})
    })
}
pub fn unique_address(value: u64) -> Option<(u64, u64)> {
    read(|index| {
        let mut matches = index.va.get(value);
        let resource = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        let &(base, bytes) = index.addresses.get(&resource)?;
        value
            .checked_sub(base)
            .filter(|&offset| offset < bytes)
            .map(|offset| (resource, offset))
    })
}
pub fn descriptor(value: u64, gpu: bool) -> Value {
    read(|index| {
        let make = |(heap, slot): (&Heap, usize)| {
            let mut value = json!({"heap":heap.id,"slot":slot});
            if !gpu {
                value["definition"] = heap.definition(slot).map_or(Value::Null, |d| json!(d));
            }
            value
        };
        if let Some(location) = index.unique(value, gpu) {
            return make(location);
        }
        let candidates: Vec<_> = index.locations(value, gpu).map(make).collect();
        json!({"candidates":candidates,"diagnostic":"descriptor handle is not uniquely associated with a live heap"})
    })
}
pub fn set(
    handle: u64,
    kind: &'static str,
    resource: u64,
    counter: u64,
    description: super::descriptors::Description,
) {
    read(|index| {
        let payload = Payload {
            kind,
            resource,
            counter,
            description,
        };
        if let Some((heap, slot)) = index.unique(handle, false) {
            heap.set(slot, Some(payload));
        }
    })
}
pub fn copy(destinations: &[u64], sources: &[u64]) {
    read(|index| {
        // Read every source first, including overlapping ranges in the same API call.
        let values: Vec<_> = sources
            .iter()
            .map(|&source| {
                index
                    .unique(source, false)
                    .and_then(|(heap, slot)| heap.get(slot).payload)
            })
            .collect();
        for (&destination, payload) in destinations.iter().zip(values) {
            if let Some((heap, slot)) = index.unique(destination, false) {
                heap.set(slot, payload);
            }
        }
    })
}
pub fn copy_simple(count: u32, destination: u64, source: u64) {
    read(|index| {
        let (Some((src, first_src)), Some((dst, first_dst))) =
            (index.unique(source, false), index.unique(destination, false))
        else {
            return;
        };
        let count = count as usize;
        if first_src + count > src.slots.len() || first_dst + count > dst.slots.len() {
            return;
        }
        if count == 1 {
            dst.set(first_dst, src.get(first_src).payload);
        } else {
            let values: Vec<_> = (first_src..first_src + count)
                .map(|slot| src.get(slot).payload)
                .collect();
            for (slot, value) in (first_dst..first_dst + count).zip(values) {
                dst.set(slot, value);
            }
        }
    })
}
pub fn stride(handle: u64) -> Option<u32> {
    read(|index| index.unique(handle, false).map(|(heap, _)| heap.stride))
}
pub fn snapshot() -> Vec<model::Descriptor> {
    read(|index| {
        index
            .heaps
            .values()
            .flat_map(|heap| (0..heap.slots.len()).filter_map(|slot| heap.definition(slot)))
            .collect()
    })
}
pub fn counts() -> (usize, usize, usize) {
    read(|index| {
        (
            index.addresses.len(),
            index.heaps.len(),
            index
                .heaps
                .values()
                .map(|heap| heap.initialized.load(Ordering::Relaxed))
                .sum(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::descriptors::Description;
    use super::*;
    #[test]
    fn published_slot_and_heap_reuse_invalidates_worker_caches() {
        const BASE: u64 = 0xA000000000;
        register_heap(1_000_000, BASE, 0, 16, 32);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let other = barrier.clone();
        let writer = std::thread::spawn(move || {
            for epoch in 0..400 {
                set(
                    BASE,
                    "cb",
                    42,
                    0,
                    Description::Cb {
                        resource: 42,
                        offset: epoch,
                        bytes: 256,
                    },
                );
                other.wait();
                other.wait();
            }
        });
        for epoch in 0..400 {
            barrier.wait();
            copy_simple(1, BASE + 32, BASE);
            let d = descriptor(BASE + 32, false);
            assert_eq!(d["definition"]["description"]["buffer"]["offset"], epoch);
            barrier.wait();
        }
        writer.join().unwrap();
        let frozen = descriptor(BASE + 32, false);
        destroy(1_000_000);
        register_heap(1_000_001, BASE, 0, 16, 32);
        assert_eq!(descriptor(BASE, false)["heap"], 1_000_001);
        assert!(descriptor(BASE, false)["definition"].is_null());
        assert_eq!(frozen["definition"]["resource"], 42);
        let workers: Vec<_> = (0..8)
            .map(|worker| {
                std::thread::spawn(move || {
                    let mut versions = Vec::new();
                    for epoch in 0..100 {
                        let handle = BASE + worker * 32;
                        set(
                            handle,
                            "cb",
                            42,
                            0,
                            Description::Cb {
                                resource: 42,
                                offset: epoch,
                                bytes: 256,
                            },
                        );
                        versions.push(descriptor(handle, false)["definition"]["id"].as_u64().unwrap());
                    }
                    versions
                })
            })
            .collect();
        let versions: std::collections::HashSet<_> =
            workers.into_iter().flat_map(|w| w.join().unwrap()).collect();
        assert_eq!(versions.len(), 800);
        destroy(1_000_001);
        register_address(1_000_002, BASE, 256);
        assert_eq!(address(BASE + 64)["resource"], 1_000_002);
        register_address(1_000_003, BASE + 32, 256);
        assert_eq!(address(BASE + 64)["candidates"].as_array().unwrap().len(), 2);
        destroy(1_000_002);
        assert_eq!(address(BASE + 64)["resource"], 1_000_003);
        assert_eq!(address(BASE + 64)["offset"], 32);
        destroy(1_000_003);
    }
}
