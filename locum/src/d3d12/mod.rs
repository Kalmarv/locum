//! D3D12 pass-through: one identity with separately negotiated interface facets.
//!
//! An object that takes private data (every `ID3D12Object`: devices, queues, lists, resources, heaps) is watched: its
//! facets are faces of it ([`crate::faces`]), counting on its own count, and they are freed when it is destroyed. So
//! the game sees the counts it would see without locum: a child holds its device, and a heap a placed resource lives
//! in stays usable after the game released it. Each release of such an object goes through the registry's lock (the
//! game's through a facet, locum's own through [`Real`]), so a lookup by number ([`real`]) never takes a reference to
//! an object being destroyed. The runtime's other interfaces (debug, configuration) are counted here instead: each
//! facet holds a reference, and the application's last release frees them.
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
use core::mem::ManuallyDrop;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use windows::Win32::Foundation::E_POINTER;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

#[repr(C)]
pub struct Object {
    vtable: *const c_void,
    /// The facet's interface of the real object: a reference of the facet's own if the object is counted here, none
    /// if it is watched. Released when the facet is freed ([`free`]).
    real: ManuallyDrop<IUnknown>,
    watched: bool,
    identity: usize,
    pub data: Arc<journal::Data>,
}
struct Identity {
    /// The object is watched: its count is its own. Otherwise each facet owns its negotiated real interface reference
    /// and `refs` counts the application's, and no registry reference survives the last of those.
    watched: bool,
    refs: u32,
    primary: usize,
    /// The facet of each interface.
    facets: HashMap<GUID, usize>,
    /// Every facet, those of an interface the object later answered with another pointer among them.
    all: Vec<usize>,
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

thread_local! {
    /// The registry, while this thread holds its lock in a release ([`release_real`]). What that release runs (the
    /// destructor of an application's private data may call locum) uses it so, not the lock.
    static HELD: Cell<*mut Registry> = const { Cell::new(core::ptr::null_mut()) };
}

/// The watched objects destroyed since the registry last forgot them, by identity. A destruction says so here, not in
/// the registry: it runs inside the runtime, maybe on a thread that holds none of locum's locks.
static DYING: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static ANY_DYING: AtomicBool = AtomicBool::new(false);

fn dying(key: usize) {
    let mut d = DYING.lock().unwrap_or_else(|e| e.into_inner());
    d.push(key);
    ANY_DYING.store(true, Ordering::Release);
}

/// Forgets the identities of the objects destroyed meanwhile, giving them back to free.
fn forget_dying(r: &mut Registry) -> Vec<Identity> {
    if !ANY_DYING.swap(false, Ordering::AcqRel) {
        return Vec::new();
    }
    let keys = core::mem::take(&mut *DYING.lock().unwrap_or_else(|e| e.into_inner()));
    keys.into_iter().filter_map(|key| remove(r, key)).collect()
}

/// Runs `f` on the registry, the destroyed objects forgotten first (and freed later: [`crate::faces::bury`]).
fn with<R>(f: impl FnOnce(&mut Registry) -> R) -> R {
    let held = HELD.with(Cell::get);
    if !held.is_null() {
        // SAFETY: this thread holds the lock, in a release that does not touch the registry until this returns.
        let r = unsafe { &mut *held };
        let gone = forget_dying(r);
        free(gone, false);
        return f(r);
    }
    let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
    let gone = forget_dying(&mut r);
    let result = f(&mut r);
    drop(r);
    free(gone, false);
    result
}

/// Forgets an identity: its object was destroyed, or the application's last reference to it went.
fn remove(r: &mut Registry, key: usize) -> Option<Identity> {
    let c = r.identities.remove(&key)?;
    r.ids.remove(&c.id);
    for p in &c.all {
        r.pointers.remove(p);
    }
    REGISTRY_REVISION.fetch_add(1, Ordering::Release);
    Some(c)
}

/// Frees forgotten identities: releases what their facets hold, drops their journal entries (`now`, or at the next
/// [`crate::faces::bury`] if the caller may hold the journal's locks) and frees the facets at the next bury (one may
/// be the facet whose release this is).
fn free(gone: Vec<Identity>, now: bool) {
    for c in gone {
        let id = c.id;
        if now {
            journal::destroy(id);
        } else {
            crate::faces::defer(move || journal::destroy(id));
        }
        for p in c.all {
            let mut facet = unsafe { Box::from_raw(p as *mut Object) };
            if !facet.watched {
                unsafe { ManuallyDrop::drop(&mut facet.real) };
            }
            // SAFETY: what is left is memory and the journal's data, which any thread may drop.
            unsafe { crate::faces::later(facet) };
        }
    }
}

/// Releases a reference to a D3D12 object under the registry's lock, so no lookup takes a reference to it while this
/// destroys it. What it destroys is forgotten before the lock goes and freed after; its journal entries are dropped
/// then if `now` (the application released it, holding none of locum's locks), else at the next bury.
unsafe fn release_real(real: *mut c_void, now: bool) -> u32 {
    if !HELD.with(Cell::get).is_null() {
        // Inside another release on this thread, which forgets what this destroys.
        return unsafe { crate::faces::release(real) };
    }
    let mut r = registry().lock().unwrap_or_else(|e| e.into_inner());
    let mut gone = forget_dying(&mut r);
    HELD.with(|h| h.set(&mut *r));
    let left = unsafe { crate::faces::release(real) };
    HELD.with(|h| h.set(core::ptr::null_mut()));
    gone.extend(forget_dying(&mut r));
    drop(r);
    free(gone, now);
    left
}

/// Whether this thread is inside a release of a D3D12 object, the registry's lock held ([`release_real`]).
pub fn releasing() -> bool {
    !HELD.with(Cell::get).is_null()
}

/// A reference locum takes itself to a D3D12 object, released like the application's ([`release_real`]).
pub struct Real<T: Interface>(ManuallyDrop<T>);
impl<T: Interface> Real<T> {
    /// Takes over a reference the runtime gave locum (from `GetDevice`, say).
    pub fn new(real: T) -> Real<T> {
        Real(ManuallyDrop::new(real))
    }
}
impl<T: Interface> core::ops::Deref for Real<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}
impl<T: Interface> Clone for Real<T> {
    fn clone(&self) -> Real<T> {
        Real::new((*self.0).clone())
    }
}
impl<T: Interface> Drop for Real<T> {
    fn drop(&mut self) {
        unsafe { release_real(self.0.as_raw(), false) };
    }
}

/// The real object behind one of our wrappers: a D3D12 object, or a DXGI adapter (device factories take one).
pub fn unwrap(p: *mut c_void) -> *mut c_void {
    if p.is_null() {
        return p;
    }
    if with(|r| r.pointers.contains_key(&(p as usize))) {
        unsafe { (*(p as *const Object)).real() }
    } else {
        crate::dxgi::Adapter::unwrap_raw(p)
    }
}
pub fn is_wrapped(p: *mut c_void) -> bool {
    with(|r| r.pointers.contains_key(&(p as usize)))
}

pub fn data(p: *mut c_void) -> Option<Arc<journal::Data>> {
    with(|r| {
        let identity = r.pointers.get(&(p as usize))?;
        Some(r.identities.get(identity)?.data.clone())
    })
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
        if entry.0 == pointer && entry.1 == revision && !ANY_DYING.load(Ordering::Acquire) {
            return entry.2;
        }
        let id = with(|r| {
            r.pointers
                .get(&pointer)
                .and_then(|key| r.identities.get(key))
                .map_or(0, |i| i.id)
        });
        *entry = (pointer, revision, id);
        id
    })
}

/// Object `id` as interface `T`, if it lives and answers to it.
pub fn real<T: Interface>(id: u64) -> Option<Real<T>> {
    with(|r| {
        let identity = r.identities.get(r.ids.get(&id)?)?;
        let primary = unsafe { &*(identity.primary as *const Object) };
        primary.real.cast::<T>().ok().map(Real::new)
    })
}

/// Takes ownership of the successful output reference. The real QI, performed
/// before locking, is authoritative about support; no fabricated higher versions.
pub unsafe fn wrap_output(hr: HRESULT, iid: *const GUID, out: *mut *mut c_void) {
    if hr.is_err() || iid.is_null() || out.is_null() || unsafe { (*out).is_null() } {
        return;
    }
    crate::faces::bury();
    let wanted = unsafe { *iid };
    let input = unsafe { IUnknown::from_raw(*out) };
    let Ok(identity) = input.cast::<IUnknown>() else {
        unsafe { *out = input.into_raw() };
        return;
    };
    let key = identity.as_raw() as usize;
    // The call's reference keeps the object alive.
    drop(identity);
    let existing = with(|r| {
        let c = r.identities.get_mut(&key)?;
        let p = if wanted == IUnknown::IID {
            c.primary
        } else {
            let p = *c.facets.get(&wanted)?;
            // A watched object's facet counts on the pointer it keeps: another pointer gets a facet of its own.
            if c.watched && unsafe { (*(p as *const Object)).real() } != input.as_raw() {
                return None;
            }
            p
        };
        if !c.watched {
            c.refs += 1;
        }
        Some((p, c.watched))
    });
    if let Some((p, watched)) = existing {
        unsafe { *out = p as *mut c_void };
        if watched {
            // The call's reference is the application's, on the object the facet is a face of.
            core::mem::forget(input);
        } else {
            drop(input);
        }
        return;
    }
    if wanted == IUnknown::IID && !with(|r| r.identities.contains_key(&key)) {
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
        log!("D3D12 interface escape {wanted:?}");
        unsafe { *out = input.into_raw() };
        return;
    };
    let (p, id) = with(|r| {
        let known = r.identities.get(&key).map(|c| (c.data.clone(), c.watched));
        let (data, watched) = known.unwrap_or_else(|| {
            r.next_id += 1;
            // Called under the lock, so the object is watched once: the registry knows it until it is destroyed.
            let watched = unsafe { crate::faces::watch(key as *mut c_void, move || dying(key)) };
            (Arc::new(journal::Data::new(r.next_id)), watched)
        });
        let id = data.id;
        let p = Box::into_raw(Box::new(Object {
            vtable: table,
            real: ManuallyDrop::new(input),
            watched,
            identity: key,
            data: data.clone(),
        })) as usize;
        let control = r.identities.entry(key).or_insert_with(|| Identity {
            watched,
            refs: 0,
            primary: p,
            facets: HashMap::new(),
            all: Vec::new(),
            private_interfaces: HashSet::new(),
            id,
            data,
        });
        if !watched {
            control.refs += 1;
        }
        control.facets.insert(wanted, p);
        control.all.push(p);
        r.pointers.insert(p, key);
        r.ids.insert(id, key);
        REGISTRY_REVISION.fetch_add(1, Ordering::Release);
        (p, id)
    });
    unsafe { *out = p as *mut c_void };
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
        if self.watched {
            return unsafe { crate::faces::add_ref(self.real()) };
        }
        with(|r| {
            let c = r.identities.get_mut(&self.identity).expect("live identity");
            c.refs += 1;
            c.refs
        })
    }
    pub unsafe fn release(&self) -> u32 {
        if self.watched {
            return unsafe { release_real(self.real(), true) };
        }
        let gone = with(|r| {
            let c = r.identities.get_mut(&self.identity).expect("live identity");
            c.refs -= 1;
            if c.refs != 0 {
                return Err(c.refs);
            }
            Ok(remove(r, self.identity))
        });
        match gone {
            Err(left) => left,
            Ok(gone) => {
                free(gone.into_iter().collect(), true);
                0
            }
        }
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
        with(|r| {
            let c = r.identities.get_mut(&self.identity).unwrap();
            if interface {
                c.private_interfaces.insert(unsafe { *key });
            } else {
                c.private_interfaces.remove(unsafe { &*key });
            }
        });
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
        let interface = with(|r| {
            r.identities
                .get(&self.identity)
                .unwrap()
                .private_interfaces
                .contains(unsafe { &*key })
        });
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
        with(|r| r.identities.len())
    );
}

#[cfg(test)]
mod tests;
