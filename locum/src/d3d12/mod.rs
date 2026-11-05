//! D3D12 pass-through: one identity with separately negotiated interface facets.
#![allow(clippy::missing_safety_doc)]
#[cfg(test)]
mod benchmark;
mod cost;
pub mod descriptors;
mod exports;
mod indirect;
pub mod inputs;
pub mod journal;
mod outputs;
pub mod passes;
pub mod pipelines;
mod portable;
mod ranges;
mod readback;
pub mod shaders;
mod slots;
pub mod states;
pub mod tiles;
pub mod values;
use crate::log::log;
use core::ffi::c_void;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use windows::Win32::Foundation::E_POINTER;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

#[repr(C)]
pub struct Object {
    vtable: *const c_void,
    real: IUnknown,
    identity: usize,
    pub data: Arc<journal::Data>,
}
struct Identity {
    // Every facet owns its negotiated real interface reference. No registry strong
    // reference survives the last application reference.
    refs: u32,
    primary: usize,
    facets: HashMap<GUID, usize>,
    private_interfaces: HashSet<GUID>,
    id: u64,
    data: Arc<journal::Data>,
}
#[derive(Default)]
struct Registry {
    identities: HashMap<usize, Identity>,
    pointers: HashMap<usize, usize>,
    ids: HashMap<u64, usize>,
    next_id: u64,
}
static REGISTRY_REVISION: AtomicU64 = AtomicU64::new(0);
fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}
fn lock() -> std::sync::MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

/// The real object behind one of our wrappers: a D3D12 object, or a DXGI adapter (device factories take one).
pub fn unwrap(p: *mut c_void) -> *mut c_void {
    if p.is_null() {
        return p;
    }
    let r = lock();
    if r.pointers.contains_key(&(p as usize)) {
        unsafe { (*(p as *const Object)).real() }
    } else {
        drop(r);
        crate::dxgi::Adapter::unwrap_raw(p)
    }
}
pub fn is_wrapped(p: *mut c_void) -> bool {
    lock().pointers.contains_key(&(p as usize))
}

pub fn data(p: *mut c_void) -> Option<Arc<journal::Data>> {
    let r = lock();
    let identity = r.pointers.get(&(p as usize))?;
    Some(r.identities.get(identity)?.data.clone())
}

pub fn id(p: *mut c_void) -> u64 {
    if p.is_null() {
        return 0;
    }
    // IDs do not own COM references. Cache only while the pointer registry's
    // revision matches; facet removal/reuse invalidates every prior association.
    thread_local! {static IDS: RefCell<[(usize,u64,u64);256]> = const {RefCell::new([(0,0,0);256])};}
    let pointer = p as usize;
    let revision = REGISTRY_REVISION.load(Ordering::Acquire);
    IDS.with(|cache| {
        let mut cache = cache.borrow_mut();
        let entry = &mut cache[((pointer >> 4) ^ (pointer >> 12)) & 255];
        if entry.0 == pointer && entry.1 == revision {
            return entry.2;
        }
        let r = lock();
        let id = r
            .pointers
            .get(&pointer)
            .and_then(|key| r.identities.get(key))
            .map_or(0, |i| i.id);
        *entry = (pointer, revision, id);
        id
    })
}

pub fn real(id: u64) -> Option<IUnknown> {
    let r = lock();
    let identity = r.identities.get(r.ids.get(&id)?)?;
    Some(unsafe { (*(identity.primary as *const Object)).real.clone() })
}

/// Takes ownership of the successful output reference. The real QI, performed
/// before locking, is authoritative about support; no fabricated higher versions.
pub unsafe fn wrap_output(hr: HRESULT, iid: *const GUID, out: *mut *mut c_void) {
    if hr.is_err() || iid.is_null() || out.is_null() || unsafe { (*out).is_null() } {
        return;
    }
    let wanted = unsafe { *iid };
    let input = unsafe { IUnknown::from_raw(*out) };
    let Ok(identity) = input.cast::<IUnknown>() else {
        unsafe { *out = input.into_raw() };
        return;
    };
    let key = identity.as_raw() as usize;
    let mut r = lock();
    if let Some(control) = r.identities.get_mut(&key) {
        let existing = if wanted == IUnknown::IID {
            Some(control.primary)
        } else {
            control.facets.get(&wanted).copied()
        };
        if let Some(p) = existing {
            control.refs += 1;
            unsafe { *out = p as *mut c_void };
            drop(r);
            drop(identity);
            drop(input);
            return;
        }
    }
    if wanted == IUnknown::IID && !r.identities.contains_key(&key) {
        drop(r);
        use windows::Win32::Graphics::Direct3D12::*;
        for candidate in [
            ID3D12Device::IID,
            ID3D12CommandQueue::IID,
            ID3D12GraphicsCommandList::IID,
            ID3D12CommandAllocator::IID,
            ID3D12Resource::IID,
            ID3D12Heap::IID,
            ID3D12Fence::IID,
            ID3D12DescriptorHeap::IID,
            ID3D12RootSignature::IID,
            ID3D12PipelineState::IID,
            ID3D12PipelineLibrary::IID,
            ID3D12CommandSignature::IID,
            ID3D12QueryHeap::IID,
            ID3D12StateObject::IID,
        ] {
            let mut facet = core::ptr::null_mut();
            let found = unsafe { crate::com::query(input.as_raw(), &candidate, &mut facet) };
            if found.is_ok() && !facet.is_null() {
                unsafe { wrap_output(found, &candidate, &mut facet) };
                unsafe { *out = facet };
                return;
            }
        }
        unsafe { *out = input.into_raw() };
        return;
    }
    let Some((table, name)) = crate::vtables::d12_registry::table(&wanted) else {
        drop(r);
        log!("D3D12 interface escape {wanted:?}");
        unsafe { *out = input.into_raw() };
        return;
    };
    let data = if let Some(c) = r.identities.get(&key) {
        c.data.clone()
    } else {
        r.next_id += 1;
        Arc::new(journal::Data::new(r.next_id))
    };
    let id = data.id;
    let p = Box::into_raw(Box::new(Object {
        vtable: table,
        real: input,
        identity: key,
        data: data.clone(),
    })) as usize;
    let control = r.identities.entry(key).or_insert_with(|| Identity {
        refs: 0,
        primary: p,
        facets: HashMap::new(),
        private_interfaces: HashSet::new(),
        id,
        data,
    });
    control.refs += 1;
    control.facets.insert(wanted, p);
    let id = control.id;
    r.pointers.insert(p, key);
    r.ids.insert(id, key);
    REGISTRY_REVISION.fetch_add(1, Ordering::Release);
    unsafe { *out = p as *mut c_void };
    drop(r);
    journal::register(id, name);
    let first = note_interface(name);
    if first && name == "ID3D12CommandQueue" {
        use windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue;
        let pointer = unsafe { (*(p as *const Object)).real() };
        if let Some(queue) = unsafe { ID3D12CommandQueue::from_raw_borrowed(&pointer) } {
            let desc = unsafe { queue.GetDesc() };
            log!(
                "D3D12 queue {id}: type={:?} priority={} flags={:?} node={}",
                desc.Type,
                desc.Priority,
                desc.Flags,
                desc.NodeMask
            );
        }
    }
}
impl Object {
    pub fn real(&self) -> *mut c_void {
        self.real.as_raw()
    }
    pub unsafe fn query(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        if iid.is_null() || out.is_null() {
            return E_POINTER;
        }
        let hr = unsafe { crate::com::query(self.real(), iid, out) };
        unsafe { wrap_output(hr, iid, out) };
        hr
    }
    pub fn add_ref(&self) -> u32 {
        let mut r = lock();
        let c = r.identities.get_mut(&self.identity).expect("live identity");
        c.refs += 1;
        c.refs
    }
    pub unsafe fn release(&self) -> u32 {
        let mut r = lock();
        let c = r.identities.get_mut(&self.identity).expect("live identity");
        c.refs -= 1;
        let remaining = c.refs;
        if remaining != 0 {
            return remaining;
        }
        let c = r.identities.remove(&self.identity).unwrap();
        r.ids.remove(&c.id);
        for p in c.facets.values() {
            r.pointers.remove(p);
        }
        REGISTRY_REVISION.fetch_add(1, Ordering::Release);
        drop(r);
        journal::destroy(c.id);
        for p in c.facets.values() {
            drop(unsafe { Box::from_raw(*p as *mut Object) });
        }
        0
    }
    pub fn called(&self, name: &'static str) {
        if let Some(index) = METHODS.iter().position(|&n| n == name)
            && METHOD_COUNTS[index].fetch_add(1, Ordering::Relaxed) == 0
        {
            log!("D3D12 first {name}");
        }
    }
    pub unsafe fn private_interface(&self, key: *const GUID, interface: bool) {
        if key.is_null() {
            return;
        }
        let mut r = lock();
        let c = r.identities.get_mut(&self.identity).unwrap();
        if interface {
            c.private_interfaces.insert(unsafe { *key });
        } else {
            c.private_interfaces.remove(unsafe { &*key });
        }
    }
    pub unsafe fn private_output(&self, hr: HRESULT, key: *const GUID, size: *const u32, data: *mut c_void) {
        if hr.is_err()
            || key.is_null()
            || size.is_null()
            || data.is_null()
            || unsafe { *size } != size_of::<usize>() as u32
        {
            return;
        }
        let interface = lock()
            .identities
            .get(&self.identity)
            .unwrap()
            .private_interfaces
            .contains(unsafe { &*key });
        if interface {
            unsafe { wrap_output(hr, &IUnknown::IID, data.cast()) };
        }
    }
}

const METHODS: [&str; 14] = [
    "ExecuteCommandLists",
    "Close",
    "Reset",
    "DrawInstanced",
    "DrawIndexedInstanced",
    "Dispatch",
    "ExecuteIndirect",
    "DispatchRays",
    "CreateStateObject",
    "AddToStateObject",
    "CreatePipelineLibrary",
    "LoadPipeline",
    "LoadGraphicsPipeline",
    "LoadComputePipeline",
];
static METHOD_COUNTS: [AtomicU64; METHODS.len()] = [const { AtomicU64::new(0) }; METHODS.len()];
fn interface_counts() -> &'static Mutex<std::collections::BTreeMap<&'static str, u64>> {
    static COUNTS: OnceLock<Mutex<std::collections::BTreeMap<&'static str, u64>>> = OnceLock::new();
    COUNTS.get_or_init(Default::default)
}
fn note_interface(name: &'static str) -> bool {
    let mut counts = interface_counts().lock().unwrap_or_else(|e| e.into_inner());
    let n = counts.entry(name).or_default();
    *n += 1;
    let first = *n == 1;
    drop(counts);
    if first {
        log!("D3D12 first wrapped {name}");
    }
    first
}
pub fn log_counts() {
    let interfaces = interface_counts()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ");
    let methods = METHODS
        .iter()
        .zip(&METHOD_COUNTS)
        .filter_map(|(name, count)| {
            let n = count.load(Ordering::Relaxed);
            (n != 0).then(|| format!("{name}={n}"))
        })
        .collect::<Vec<_>>()
        .join(" ");
    log!(
        "D3D12 totals: interface facets [{interfaces}]; calls [{methods}]; live identities {}",
        lock().identities.len()
    );
}

#[cfg(test)]
mod tests;
