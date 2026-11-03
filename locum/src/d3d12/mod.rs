//! D3D12 pass-through: one identity with separately negotiated interface facets.
#![allow(clippy::missing_safety_doc)]
mod exports;
pub mod inputs;
use crate::log::log;
use core::ffi::c_void;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::E_POINTER;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

#[repr(C)]
pub struct Object {
    vtable: *const c_void,
    real: IUnknown,
    identity: usize,
    seen: AtomicU64,
}
struct Identity {
    // Every facet owns its negotiated real interface reference. No registry strong
    // reference survives the last application reference.
    refs: u32,
    primary: usize,
    facets: HashMap<GUID, usize>,
    private_interfaces: HashSet<GUID>,
    calls: HashSet<&'static str>,
    id: u64,
}
#[derive(Default)]
struct Registry {
    identities: HashMap<usize, Identity>,
    pointers: HashMap<usize, usize>,
    next_id: u64,
}
fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}
fn lock() -> std::sync::MutexGuard<'static, Registry> {
    registry().lock().unwrap_or_else(|e| e.into_inner())
}

pub fn unwrap(p: *mut c_void) -> *mut c_void {
    let r = lock();
    if r.pointers.contains_key(&(p as usize)) {
        unsafe { (*(p as *const Object)).real() }
    } else {
        p
    }
}
pub fn is_wrapped(p: *mut c_void) -> bool {
    lock().pointers.contains_key(&(p as usize))
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
    let p = Box::into_raw(Box::new(Object {
        vtable: table,
        real: input,
        identity: key,
        seen: AtomicU64::new(0),
    })) as usize;
    r.next_id += 1;
    let id = r.next_id;
    let control = r.identities.entry(key).or_insert_with(|| Identity {
        refs: 0,
        primary: p,
        facets: HashMap::new(),
        private_interfaces: HashSet::new(),
        calls: HashSet::new(),
        id,
    });
    control.refs += 1;
    control.facets.insert(wanted, p);
    let id = control.id;
    r.pointers.insert(p, key);
    unsafe { *out = p as *mut c_void };
    drop(r);
    log!("D3D12 object {id}: wrapped {name} real identity {key:x} as {p:x}");
    if name == "ID3D12CommandQueue" {
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
        for p in c.facets.values() {
            r.pointers.remove(p);
        }
        drop(r);
        for p in c.facets.values() {
            drop(unsafe { Box::from_raw(*p as *mut Object) });
        }
        0
    }
    pub fn called(&self, name: &'static str) {
        let index = match name {
            "ExecuteCommandLists" => 0,
            "Close" => 1,
            "Reset" => 2,
            "DrawInstanced" => 3,
            "DrawIndexedInstanced" => 4,
            "Dispatch" => 5,
            "ExecuteIndirect" => 6,
            "DispatchRays" => 7,
            "CreateStateObject" => 8,
            "CreatePipelineLibrary" => 9,
            "LoadPipeline" => 10,
            "LoadGraphicsPipeline" => 11,
            "LoadComputePipeline" => 12,
            _ => 63,
        };
        let bit = 1u64 << index;
        if self.seen.load(Ordering::Relaxed) & bit != 0 {
            return;
        }
        if self.seen.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
            return;
        }
        let mut r = lock();
        let c = r.identities.get_mut(&self.identity).unwrap();
        if c.calls.insert(name) {
            let id = c.id;
            drop(r);
            log!("D3D12 object {id}: first {name}");
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

#[cfg(test)]
mod tests;
