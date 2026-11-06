//! What a device makes (buffers, textures, views, shaders, states, queries, command lists) is the runtime's own
//! object, not wrapped: locum sees each one made, and what is done with it passes through the context. The one way
//! out is back to the device. `GetDevice` on a texture would hand out the real device, and what the game then made
//! with that, locum would not see, nor would the device be the one the game holds.
//!
//! So in each table the runtime's children use, the slots that hand out a parent are pointed at locum's functions,
//! which call the runtime's and give locum's wrapper of what it returned ([`crate::dxgi::give`]): `GetDevice` of
//! `ID3D11DeviceChild`; `GetParent` and `GetDevice` of the DXGI faces of a resource; and a class linkage's two ways to
//! a class instance, a child it makes itself. A table belongs to a class of the runtime and serves every object of it,
//! so each is changed once, the first time an object of the class comes past, and what is handed out is only changed
//! for a device locum wrapped: these slots give an object locum already wrapped as its face and leave the rest
//! alone ([`crate::dxgi::give_known`]), save an output locum handed out, whose adapter it wraps if it must.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::RwLock;

use windows::Win32::Foundation::{E_FAIL, S_OK};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11ClassLinkage, ID3D11ClassLinkage_Vtbl, ID3D11Device, ID3D11DeviceChild_Vtbl,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDeviceSubObject, IDXGIDeviceSubObject_Vtbl, IDXGIObject, IDXGIObject_Vtbl, IDXGIResource,
    IDXGISurface,
};
use windows::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};
use windows::core::{GUID, HRESULT, Interface, PCSTR};

use crate::log::log;

const SLOT: usize = size_of::<usize>();
const CHILD_GET_DEVICE: usize = core::mem::offset_of!(ID3D11DeviceChild_Vtbl, GetDevice) / SLOT;
const DXGI_GET_PARENT: usize = core::mem::offset_of!(IDXGIObject_Vtbl, GetParent) / SLOT;
const DXGI_GET_DEVICE: usize = core::mem::offset_of!(IDXGIDeviceSubObject_Vtbl, GetDevice) / SLOT;
const LINKAGE_GET_INSTANCE: usize = core::mem::offset_of!(ID3D11ClassLinkage_Vtbl, GetClassInstance) / SLOT;
const LINKAGE_CREATE_INSTANCE: usize =
    core::mem::offset_of!(ID3D11ClassLinkage_Vtbl, CreateClassInstance) / SLOT;

/// The runtime's function each changed slot held, by the slot's address.
static ORIGINALS: RwLock<Option<HashMap<usize, usize>>> = RwLock::new(None);

/// Points slot `slot` of `object`'s table at `hook`, unless it already is (or the table is locum's own).
unsafe fn patch(object: *mut c_void, slot: usize, hook: usize) {
    let table = unsafe { *(object as *const *mut usize) };
    let at = unsafe { table.add(slot) };
    if ORIGINALS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|o| o.contains_key(&(at as usize)))
    {
        return;
    }
    let mut originals = ORIGINALS.write().unwrap_or_else(|e| e.into_inner());
    let originals = originals.get_or_insert_with(HashMap::new);
    if originals.contains_key(&(at as usize)) {
        return;
    }
    let mut old = PAGE_PROTECTION_FLAGS(0);
    if let Err(e) = unsafe { VirtualProtect(at.cast(), SLOT, PAGE_READWRITE, &mut old) } {
        log!("cannot change the runtime's table at {at:p} ({e}): its objects' parents stay real");
        return;
    }
    // Recorded before the slot changes: a call through it on another thread waits for this lock, then finds it.
    originals.insert(at as usize, unsafe { at.read() });
    unsafe { at.write_volatile(hook) };
    let mut changed = PAGE_PROTECTION_FLAGS(0);
    let _ = unsafe { VirtualProtect(at.cast(), SLOT, old, &mut changed) };
}

/// The runtime's function in slot `slot` of `this`'s table, before locum changed it.
unsafe fn original(this: *mut c_void, slot: usize) -> Option<usize> {
    let at = unsafe { (*(this as *const *const usize)).add(slot) } as usize;
    ORIGINALS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()?
        .get(&at)
        .copied()
}

/// Whether `table` is one of locum's (a wrapper of its own, not a runtime object).
fn ours(table: *const c_void) -> bool {
    static LOCUM: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let locum =
        *LOCUM.get_or_init(|| crate::system::module_of(ours as *const c_void).map_or(0, |m| m.0 as usize));
    crate::system::module_of(table).is_some_and(|m| m.0 as usize == locum)
}

/// Sees an object the device made: from now on its class's way back to the device gives locum's device.
///
/// # Safety
/// `object` must be null or a live COM object.
pub unsafe fn adopt(object: *mut c_void) {
    if object.is_null() || ours(unsafe { *(object as *const *const c_void) }) {
        return;
    }
    unsafe { patch(object, CHILD_GET_DEVICE, child_get_device as *const () as usize) };
    // A resource's DXGI faces have tables of their own, maybe one each.
    for iid in [IDXGIDeviceSubObject::IID, IDXGIResource::IID, IDXGISurface::IID] {
        let mut face = core::ptr::null_mut();
        if unsafe { crate::com::query(object, &iid, &mut face) }.is_ok() && !face.is_null() {
            unsafe {
                patch(face, DXGI_GET_PARENT, dxgi_get_parent as *const () as usize);
                patch(face, DXGI_GET_DEVICE, dxgi_get_device as *const () as usize);
                crate::faces::release(face);
            }
        }
    }
    let mut linkage = core::ptr::null_mut();
    if unsafe { crate::com::query(object, &ID3D11ClassLinkage::IID, &mut linkage) }.is_ok()
        && !linkage.is_null()
    {
        unsafe {
            patch(
                linkage,
                LINKAGE_GET_INSTANCE,
                linkage_get_instance as *const () as usize,
            );
            patch(
                linkage,
                LINKAGE_CREATE_INSTANCE,
                linkage_create_instance as *const () as usize,
            );
            crate::faces::release(linkage);
        }
    }
}

/// The private-data key that marks an output locum handed out.
const HANDED_OUT: GUID = GUID::from_u128(0x6c6f_6375_6d00_4a5e_9d1b_7a2f_8c3e_0004);

/// Sees a DXGI output (the runtime's, not wrapped) locum hands out: from now on its way back to the adapter gives
/// locum's, wrapped then if it was not yet. An output of the runtime's own (of a device it made without locum) shares
/// the table, and its way back stays the runtime's: the mark tells the two apart.
///
/// # Safety
/// `output` must be null or a live COM object.
pub unsafe fn adopt_output(output: *mut c_void) {
    if output.is_null() || ours(unsafe { *(output as *const *const c_void) }) {
        return;
    }
    if let Some(object) = unsafe { IDXGIObject::from_raw_borrowed(&output) } {
        let _ = unsafe { object.SetPrivateData(&HANDED_OUT, 1, [1u8].as_ptr().cast()) };
    }
    unsafe { patch(output, DXGI_GET_PARENT, dxgi_get_parent as *const () as usize) };
}

/// Whether `object` is an output locum handed out ([`adopt_output`]).
unsafe fn handed_out(object: *mut c_void) -> bool {
    let Some(object) = (unsafe { IDXGIObject::from_raw_borrowed(&object) }) else {
        return false;
    };
    let mut mark = 0u8;
    let mut size = 1;
    unsafe { object.GetPrivateData(&HANDED_OUT, &mut size, (&raw mut mark).cast()) }.is_ok()
}

type GetDevice = unsafe extern "system" fn(*mut c_void, *mut *mut c_void);
type GetParent = unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT;
type GetInstance = unsafe extern "system" fn(*mut c_void, PCSTR, u32, *mut *mut c_void) -> HRESULT;
type CreateInstance =
    unsafe extern "system" fn(*mut c_void, PCSTR, u32, u32, u32, u32, *mut *mut c_void) -> HRESULT;

unsafe extern "system" fn child_get_device(this: *mut c_void, out: *mut *mut c_void) {
    let Some(f) = (unsafe { original(this, CHILD_GET_DEVICE) }) else {
        if !out.is_null() {
            unsafe { *out = core::ptr::null_mut() };
        }
        return;
    };
    unsafe { core::mem::transmute::<usize, GetDevice>(f)(this, out) };
    // `GetDevice` gives no result: what it wrote is the device, or null.
    let _ = unsafe { crate::dxgi::give_known(S_OK, &ID3D11Device::IID, out) };
}

unsafe extern "system" fn dxgi_get_parent(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(f) = (unsafe { original(this, DXGI_GET_PARENT) }) else {
        return E_FAIL;
    };
    let hr = unsafe { core::mem::transmute::<usize, GetParent>(f)(this, iid, out) };
    if unsafe { handed_out(this) } {
        unsafe { crate::dxgi::give(hr, iid, out) }
    } else {
        unsafe { crate::dxgi::give_known(hr, iid, out) }
    }
}

unsafe extern "system" fn dxgi_get_device(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(f) = (unsafe { original(this, DXGI_GET_DEVICE) }) else {
        return E_FAIL;
    };
    let hr = unsafe { core::mem::transmute::<usize, GetParent>(f)(this, iid, out) };
    unsafe { crate::dxgi::give_known(hr, iid, out) }
}

unsafe extern "system" fn linkage_get_instance(
    this: *mut c_void,
    name: PCSTR,
    index: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(f) = (unsafe { original(this, LINKAGE_GET_INSTANCE) }) else {
        return E_FAIL;
    };
    let hr = unsafe { core::mem::transmute::<usize, GetInstance>(f)(this, name, index, out) };
    if hr.is_ok() && !out.is_null() {
        unsafe { adopt(*out) };
    }
    hr
}

unsafe extern "system" fn linkage_create_instance(
    this: *mut c_void,
    name: PCSTR,
    buffer: u32,
    vector: u32,
    texture: u32,
    sampler: u32,
    out: *mut *mut c_void,
) -> HRESULT {
    let Some(f) = (unsafe { original(this, LINKAGE_CREATE_INSTANCE) }) else {
        return E_FAIL;
    };
    let hr = unsafe {
        core::mem::transmute::<usize, CreateInstance>(f)(this, name, buffer, vector, texture, sampler, out)
    };
    if hr.is_ok() && !out.is_null() {
        unsafe { adopt(*out) };
    }
    hr
}
