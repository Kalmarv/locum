//! The device and its contexts.
//!
//! The immediate context lives inside the device and shares its reference count, as in DXVK: a game that holds only
//! the immediate context keeps the device alive. A deferred context has its own count and holds one reference to the
//! device.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use windows::Win32::Graphics::Direct3D11::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

use crate::com::answer_query;
use crate::log::log;
use crate::vtables::context::ContextMethods;
use crate::vtables::device::{DeviceMethods, real};

#[repr(C)]
pub struct Device {
    vtbl: *const ID3D11Device5_Vtbl,
    refs: AtomicU32,
    inner: ID3D11Device5,
    immediate: Context,
}

#[repr(C)]
pub struct Context {
    vtbl: *const ID3D11DeviceContext4_Vtbl,
    /// Deferred contexts only; the immediate context counts on its device.
    refs: AtomicU32,
    inner: ID3D11DeviceContext4,
    device: *const Device,
    deferred: bool,
}

static DEVICE_VTBL: ID3D11Device5_Vtbl = crate::vtables::device::vtable::<Device>();
static CONTEXT_VTBL: ID3D11DeviceContext4_Vtbl = crate::vtables::context::vtable::<Context>();

const DEVICE_IIDS: [GUID; 7] = [
    IUnknown::IID,
    ID3D11Device::IID,
    ID3D11Device1::IID,
    ID3D11Device2::IID,
    ID3D11Device3::IID,
    ID3D11Device4::IID,
    ID3D11Device5::IID,
];

const CONTEXT_IIDS: [GUID; 7] = [
    IUnknown::IID,
    ID3D11DeviceChild::IID,
    ID3D11DeviceContext::IID,
    ID3D11DeviceContext1::IID,
    ID3D11DeviceContext2::IID,
    ID3D11DeviceContext3::IID,
    ID3D11DeviceContext4::IID,
];

impl Device {
    /// Wraps a device the real runtime created. Takes over the caller's reference to `device` and returns the
    /// wrapper with one reference, or gives `device` back if it is not an `ID3D11Device5`.
    pub fn wrap(device: ID3D11Device) -> Result<*mut Device, ID3D11Device> {
        let Ok(inner) = device.cast::<ID3D11Device5>() else {
            log!("device is not an ID3D11Device5: left unwrapped");
            return Err(device);
        };
        let immediate = match unsafe { inner.GetImmediateContext3() }.and_then(|c| c.cast::<ID3D11DeviceContext4>()) {
            Ok(c) => c,
            Err(e) => {
                log!("immediate context is not an ID3D11DeviceContext4 ({e}): device left unwrapped");
                return Err(device);
            }
        };
        let wrapper = Box::into_raw(Box::new(Device {
            vtbl: &DEVICE_VTBL,
            refs: AtomicU32::new(1),
            inner,
            immediate: Context {
                vtbl: &CONTEXT_VTBL,
                refs: AtomicU32::new(0),
                inner: immediate,
                device: core::ptr::null(),
                deferred: false,
            },
        }));
        unsafe { (*wrapper).immediate.device = wrapper };
        log!("wrapped device {:p} as {wrapper:p}", device.as_raw());
        Ok(wrapper)
    }

    /// If `object` is one of our devices, the real device under it; otherwise `object` unchanged.
    pub fn unwrap_raw(object: *mut c_void) -> *mut c_void {
        match Self::from_raw(object) {
            Some(device) => device.inner.as_raw(),
            None => object,
        }
    }

    /// `object` as one of our devices, if it is one.
    pub fn from_raw<'a>(object: *mut c_void) -> Option<&'a Device> {
        if object.is_null() {
            return None;
        }
        let vtbl = unsafe { *(object as *const *const ID3D11Device5_Vtbl) };
        core::ptr::eq(vtbl, &DEVICE_VTBL).then(|| unsafe { &*(object as *const Device) })
    }

    pub fn immediate_raw(&self) -> *mut c_void {
        &self.immediate as *const Context as *mut c_void
    }

    /// Hands out the immediate context with a reference.
    unsafe fn give_immediate(&self, out: *mut *mut c_void) {
        if out.is_null() {
            return;
        }
        unsafe {
            self.AddRef();
            *out = self.immediate_raw();
        }
    }

    /// Replaces the real deferred context the runtime wrote to `out` with a wrapper.
    unsafe fn wrap_deferred(&self, hr: HRESULT, out: *mut *mut c_void) -> HRESULT {
        if hr.is_err() || out.is_null() || unsafe { (*out).is_null() } {
            return hr;
        }
        let real = unsafe { ID3D11DeviceContext::from_raw(*out) };
        match real.cast::<ID3D11DeviceContext4>() {
            Ok(inner) => {
                unsafe { self.AddRef() };
                let wrapper = Box::into_raw(Box::new(Context {
                    vtbl: &CONTEXT_VTBL,
                    refs: AtomicU32::new(1),
                    inner,
                    device: self,
                    deferred: true,
                }));
                unsafe { *out = wrapper.cast() };
            }
            Err(e) => {
                log!("deferred context is not an ID3D11DeviceContext4 ({e}): left unwrapped");
                unsafe { *out = real.into_raw() };
            }
        }
        hr
    }
}

unsafe impl DeviceMethods for Device {
    fn inner(&self) -> *mut c_void {
        self.inner.as_raw()
    }

    unsafe fn QueryInterface(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        unsafe {
            answer_query("device", self as *const _ as _, self.inner(), &DEVICE_IIDS, iid, out, || {
                self.AddRef();
            })
        }
    }

    unsafe fn AddRef(&self) -> u32 {
        self.refs.fetch_add(1, Ordering::Relaxed) + 1
    }

    unsafe fn Release(&self) -> u32 {
        let left = self.refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            log!("device {:p} released", self as *const Device);
            drop(unsafe { Box::from_raw(self as *const Device as *mut Device) });
        }
        left
    }

    unsafe fn GetImmediateContext(&self, a0: *mut *mut c_void) {
        unsafe { self.give_immediate(a0) }
    }

    unsafe fn GetImmediateContext1(&self, a0: *mut *mut c_void) {
        unsafe { self.give_immediate(a0) }
    }

    unsafe fn GetImmediateContext2(&self, a0: *mut *mut c_void) {
        unsafe { self.give_immediate(a0) }
    }

    unsafe fn GetImmediateContext3(&self, a0: *mut *mut c_void) {
        unsafe { self.give_immediate(a0) }
    }

    unsafe fn CreateDeferredContext(&self, a0: u32, a1: *mut *mut c_void) -> HRESULT {
        unsafe { self.wrap_deferred(real::CreateDeferredContext(self.inner(), a0, a1), a1) }
    }

    unsafe fn CreateDeferredContext1(&self, a0: u32, a1: *mut *mut c_void) -> HRESULT {
        unsafe { self.wrap_deferred(real::CreateDeferredContext1(self.inner(), a0, a1), a1) }
    }

    unsafe fn CreateDeferredContext2(&self, a0: u32, a1: *mut *mut c_void) -> HRESULT {
        unsafe { self.wrap_deferred(real::CreateDeferredContext2(self.inner(), a0, a1), a1) }
    }

    unsafe fn CreateDeferredContext3(&self, a0: u32, a1: *mut *mut c_void) -> HRESULT {
        unsafe { self.wrap_deferred(real::CreateDeferredContext3(self.inner(), a0, a1), a1) }
    }
}

unsafe impl ContextMethods for Context {
    fn inner(&self) -> *mut c_void {
        self.inner.as_raw()
    }

    unsafe fn QueryInterface(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        let kind = if self.deferred { "deferred context" } else { "immediate context" };
        unsafe {
            answer_query(kind, self as *const _ as _, self.inner(), &CONTEXT_IIDS, iid, out, || {
                self.AddRef();
            })
        }
    }

    unsafe fn AddRef(&self) -> u32 {
        if self.deferred {
            self.refs.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            unsafe { (*self.device).AddRef() }
        }
    }

    unsafe fn Release(&self) -> u32 {
        if !self.deferred {
            return unsafe { (*self.device).Release() };
        }
        let left = self.refs.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let device = self.device;
            drop(unsafe { Box::from_raw(self as *const Context as *mut Context) });
            unsafe { (*device).Release() };
        }
        left
    }

    unsafe fn GetDevice(&self, a0: *mut *mut c_void) {
        if a0.is_null() {
            return;
        }
        unsafe {
            (*self.device).AddRef();
            *a0 = self.device as *mut c_void;
        }
    }
}
