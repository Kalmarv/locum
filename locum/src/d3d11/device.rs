//! The device and its contexts.
//!
//! The immediate context lives inside the device and shares its reference count, as in DXVK: a game that holds only
//! the immediate context keeps the device alive. A deferred context has its own count and holds one reference to the
//! device.
//!
//! The device is also its own DXGI device (`IDXGIDevice` to `IDXGIDevice4`), as in DXVK: the usual way to a swap chain
//! is device -> IDXGIDevice -> GetAdapter -> GetParent (the factory) -> CreateSwapChain, and every object on that path
//! has to be locum's for the swap chain to be. The DXGI device lives inside the device like the immediate context.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, Ordering};

use std::sync::OnceLock;

use windows::Win32::Foundation::{E_NOINTERFACE, E_POINTER, RECT, S_OK};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIDevice1, IDXGIDevice2, IDXGIDevice3, IDXGIDevice4, IDXGIDevice4_Vtbl, IDXGIObject,
};
use windows::core::{BOOL, GUID, HRESULT, IUnknown, Interface};

use crate::com::answer_query;
use crate::d3d11::capture::{self, ops};
use crate::d3d11::overrides;
use crate::d3d11::registry;
use crate::d3d11::shaders::{self, Stage};
use crate::dxgi::Adapter;
use crate::log::log;
use crate::vtables::adapter::AdapterMethods;
use crate::vtables::context::{ContextMethods, real as real_context};
use crate::vtables::device::{DeviceMethods, real};
use crate::vtables::dxgi_device::DxgiDeviceMethods;

#[repr(C)]
pub struct Device {
    vtbl: *const ID3D11Device5_Vtbl,
    refs: AtomicU32,
    inner: ID3D11Device5,
    immediate: Context,
    dxgi: DxgiDevice,
    /// Our wrapper of the device's adapter, with one reference (0 until asked for, 1 if it could not be wrapped).
    adapter: OnceLock<usize>,
}

/// The device's `IDXGIDevice4` face. Counts on the device.
#[repr(C)]
pub struct DxgiDevice {
    vtbl: *const IDXGIDevice4_Vtbl,
    inner: IDXGIDevice4,
    device: *const Device,
}

#[repr(C)]
pub struct Context {
    vtbl: *const ID3D11DeviceContext4_Vtbl,
    /// Deferred contexts only; the immediate context counts on its device.
    refs: AtomicU32,
    inner: ID3D11DeviceContext4,
    device: *const Device,
    deferred: bool,
    /// locum's number for the context, in captures.
    id: u32,
}

static DEVICE_VTBL: ID3D11Device5_Vtbl = crate::vtables::device::vtable::<Device>();
static CONTEXT_VTBL: ID3D11DeviceContext4_Vtbl = crate::vtables::context::vtable::<Context>();
static DXGI_DEVICE_VTBL: IDXGIDevice4_Vtbl = crate::vtables::dxgi_device::vtable::<DxgiDevice>();

const DXGI_DEVICE_IIDS: [GUID; 6] = [
    IDXGIObject::IID,
    IDXGIDevice::IID,
    IDXGIDevice1::IID,
    IDXGIDevice2::IID,
    IDXGIDevice3::IID,
    IDXGIDevice4::IID,
];

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

impl Context {
    /// Runs the real draw or dispatch, with the overrides that hold for it if any are loaded.
    fn run(&self, compute: bool, real: impl FnOnce()) {
        if overrides::active() {
            unsafe {
                overrides::around(
                    &self.inner,
                    compute,
                    |rule| {
                        if capture::armed() {
                            capture::rule_applied(self.ctx(), rule);
                        }
                    },
                    real,
                )
            }
        } else {
            real()
        }
        if capture::armed() {
            capture::after_draw(self.ctx());
        }
    }

    /// Records a draw or dispatch if a capture is running.
    fn draw(&self, call: &'static str, args: &[i64]) {
        if capture::armed() {
            capture::on_draw(self.ctx(), call, args);
        }
    }

    /// Targets about to be bound on this context, if a capture is running (see [`capture::targets_changing`]).
    unsafe fn targets_changing(&self, count: u32, rtvs: *const *mut c_void, dsv: *mut c_void) {
        if capture::armed() {
            let rtvs = if rtvs.is_null() || count == 0 {
                &[][..]
            } else {
                unsafe { core::slice::from_raw_parts(rtvs, count.min(8) as usize) }
            };
            unsafe { capture::targets_changing(self.ctx(), rtvs, dsv) };
        }
    }

    /// An update: recorded while a capture runs, else only noted on the resource's registry entry.
    #[allow(clippy::too_many_arguments)]
    unsafe fn update(
        &self,
        call: &'static str,
        dst: *mut c_void,
        sub: u32,
        region: *const D3D11_BOX,
        data: *const c_void,
        row: u32,
        depth: u32,
        flags: u32,
    ) {
        if dst.is_null() || data.is_null() {
            return;
        }
        // Emulated command lists add the destination offset to UpdateSubresource's source pointer.
        // Record those bytes, including when the application already compensated for that runtime behavior.
        // UpdateSubresource1 and native command lists use the supplied pointer without this offset.
        let mut data = data;
        if self.deferred && call == "UpdateSubresource" && !region.is_null() {
            let mut threading = D3D11_FEATURE_DATA_THREADING::default();
            let emulated = unsafe { self.inner.GetDevice() }.is_ok_and(|device| {
                unsafe {
                    device.CheckFeatureSupport(
                        D3D11_FEATURE_THREADING,
                        (&mut threading as *mut D3D11_FEATURE_DATA_THREADING).cast(),
                        size_of_val(&threading) as u32,
                    )
                }
                .is_ok()
                    && !threading.DriverCommandLists.as_bool()
            });
            if emulated
                && let Some(resource) = unsafe { ID3D11Resource::from_raw_borrowed(&dst) }
                && let Some(dims) = registry::dims_of(resource)
                && let Some(offset) = registry::source_offset(&dims, unsafe { &*region }, row, depth)
            {
                data = data.wrapping_byte_add(offset);
            }
        }
        if capture::armed() {
            unsafe { ops::update(self.ctx(), call, dst, sub, region, data, row, depth, flags) };
        } else if let Some(r) = unsafe { ID3D11Resource::from_raw_borrowed(&dst) } {
            let region = if region.is_null() {
                None
            } else {
                Some(unsafe { &*region })
            };
            unsafe { registry::updated(r, sub, region, data, row, depth, false) };
        }
    }

    /// This context as the capture's ops name it.
    fn ctx(&self) -> ops::Ctx<'_> {
        ops::Ctx {
            c: &self.inner,
            id: self.id,
            deferred: self.deferred,
        }
    }

    /// The real context.
    pub fn real(&self) -> &ID3D11DeviceContext4 {
        &self.inner
    }

    /// Records an indirect draw or dispatch if a capture is running: its arguments are the buffer's id and offset.
    unsafe fn draw_indirect(&self, call: &'static str, buffer: *mut c_void, offset: u32) {
        if capture::armed() {
            unsafe { capture::on_indirect(self.ctx(), call, buffer, offset) };
        }
    }
}

impl Device {
    /// Wraps a device the real runtime created. Takes over the caller's reference to `device` and returns the
    /// wrapper with one reference, or gives `device` back if it is not an `ID3D11Device5`.
    pub fn wrap(device: ID3D11Device) -> Result<*mut Device, ID3D11Device> {
        let Ok(inner) = device.cast::<ID3D11Device5>() else {
            log!("device is not an ID3D11Device5: left unwrapped");
            return Err(device);
        };
        let immediate =
            match unsafe { inner.GetImmediateContext3() }.and_then(|c| c.cast::<ID3D11DeviceContext4>()) {
                Ok(c) => c,
                Err(e) => {
                    log!("immediate context is not an ID3D11DeviceContext4 ({e}): device left unwrapped");
                    return Err(device);
                }
            };
        let dxgi = match inner.cast::<IDXGIDevice4>() {
            Ok(d) => d,
            Err(e) => {
                log!("device is not an IDXGIDevice4 ({e}): left unwrapped");
                return Err(device);
            }
        };
        let wrapper = Box::into_raw(Box::new(Device {
            vtbl: &DEVICE_VTBL,
            refs: AtomicU32::new(1),
            inner,
            dxgi: DxgiDevice {
                vtbl: &DXGI_DEVICE_VTBL,
                inner: dxgi,
                device: core::ptr::null(),
            },
            adapter: OnceLock::new(),
            immediate: Context {
                vtbl: &CONTEXT_VTBL,
                refs: AtomicU32::new(0),
                inner: immediate,
                device: core::ptr::null(),
                deferred: false,
                id: capture::next_context_id(),
            },
        }));
        unsafe {
            (*wrapper).immediate.device = wrapper;
            (*wrapper).dxgi.device = wrapper;
        }
        log!("wrapped device {:p} as {wrapper:p}", device.as_raw());
        Ok(wrapper)
    }

    /// If `object` is one of our devices (either face), the real device under it; otherwise `object` unchanged.
    pub fn unwrap_raw(object: *mut c_void) -> *mut c_void {
        match Self::from_raw(object) {
            Some(device) => device.inner.as_raw(),
            None => object,
        }
    }

    /// `object` as one of our devices, if it is one: its `ID3D11Device` face or its `IDXGIDevice` face.
    pub fn from_raw<'a>(object: *mut c_void) -> Option<&'a Device> {
        if object.is_null() {
            return None;
        }
        let vtbl = unsafe { *(object as *const *const c_void) };
        if core::ptr::eq(vtbl, (&DEVICE_VTBL as *const ID3D11Device5_Vtbl).cast()) {
            return Some(unsafe { &*(object as *const Device) });
        }
        if core::ptr::eq(vtbl, (&DXGI_DEVICE_VTBL as *const IDXGIDevice4_Vtbl).cast()) {
            return Some(unsafe { &*(*(object as *const DxgiDevice)).device });
        }
        None
    }

    /// Our wrapper of the device's adapter, made the first time it is asked for.
    fn adapter(&self) -> Option<&Adapter> {
        let p = *self.adapter.get_or_init(|| {
            let real = match unsafe { self.dxgi.inner.GetAdapter() } {
                Ok(a) => a,
                Err(e) => {
                    log!("IDXGIDevice::GetAdapter on the real device failed: {e}");
                    return 1;
                }
            };
            match Adapter::wrap(real.into(), None) {
                Ok(w) => w as usize,
                Err(_) => 1,
            }
        });
        (p > 1).then(|| unsafe { &*(p as *const Adapter) })
    }

    pub fn immediate(&self) -> &Context {
        &self.immediate
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
                    id: capture::next_context_id(),
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
        if !iid.is_null() && !out.is_null() && DXGI_DEVICE_IIDS.contains(unsafe { &*iid }) {
            unsafe {
                self.AddRef();
                *out = &self.dxgi as *const DxgiDevice as *mut c_void;
            }
            return S_OK;
        }
        unsafe {
            answer_query(
                "device",
                self as *const _ as _,
                self.inner(),
                &DEVICE_IIDS,
                iid,
                out,
                || {
                    self.AddRef();
                },
            )
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

    unsafe fn CreateVertexShader(
        &self,
        a0: *const c_void,
        a1: usize,
        a2: *mut c_void,
        a3: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateVertexShader(self.inner(), a0, a1, a2, a3) };
        unsafe { shaders::created(Stage::Vertex, a0, a1, hr, a3) };
        hr
    }

    unsafe fn CreateHullShader(
        &self,
        a0: *const c_void,
        a1: usize,
        a2: *mut c_void,
        a3: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateHullShader(self.inner(), a0, a1, a2, a3) };
        unsafe { shaders::created(Stage::Hull, a0, a1, hr, a3) };
        hr
    }

    unsafe fn CreateDomainShader(
        &self,
        a0: *const c_void,
        a1: usize,
        a2: *mut c_void,
        a3: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateDomainShader(self.inner(), a0, a1, a2, a3) };
        unsafe { shaders::created(Stage::Domain, a0, a1, hr, a3) };
        hr
    }

    unsafe fn CreateGeometryShader(
        &self,
        a0: *const c_void,
        a1: usize,
        a2: *mut c_void,
        a3: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateGeometryShader(self.inner(), a0, a1, a2, a3) };
        unsafe { shaders::created(Stage::Geometry, a0, a1, hr, a3) };
        hr
    }

    unsafe fn CreateGeometryShaderWithStreamOutput(
        &self,
        a0: *const c_void,
        a1: usize,
        a2: *const D3D11_SO_DECLARATION_ENTRY,
        a3: u32,
        a4: *const u32,
        a5: u32,
        a6: u32,
        a7: *mut c_void,
        a8: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe {
            real::CreateGeometryShaderWithStreamOutput(self.inner(), a0, a1, a2, a3, a4, a5, a6, a7, a8)
        };
        unsafe { shaders::created(Stage::Geometry, a0, a1, hr, a8) };
        hr
    }

    unsafe fn CreatePixelShader(
        &self,
        a0: *const c_void,
        a1: usize,
        a2: *mut c_void,
        a3: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreatePixelShader(self.inner(), a0, a1, a2, a3) };
        unsafe { shaders::created(Stage::Pixel, a0, a1, hr, a3) };
        hr
    }

    unsafe fn CreateComputeShader(
        &self,
        a0: *const c_void,
        a1: usize,
        a2: *mut c_void,
        a3: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateComputeShader(self.inner(), a0, a1, a2, a3) };
        unsafe { shaders::created(Stage::Compute, a0, a1, hr, a3) };
        hr
    }

    unsafe fn CreateBuffer(
        &self,
        a0: *const D3D11_BUFFER_DESC,
        a1: *const D3D11_SUBRESOURCE_DATA,
        a2: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateBuffer(self.inner(), a0, a1, a2) };
        unsafe { registry::created(hr, a2, a1) };
        hr
    }

    unsafe fn CreateTexture1D(
        &self,
        a0: *const D3D11_TEXTURE1D_DESC,
        a1: *const D3D11_SUBRESOURCE_DATA,
        a2: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateTexture1D(self.inner(), a0, a1, a2) };
        unsafe { registry::created(hr, a2, a1) };
        hr
    }

    unsafe fn CreateTexture2D(
        &self,
        a0: *const D3D11_TEXTURE2D_DESC,
        a1: *const D3D11_SUBRESOURCE_DATA,
        a2: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateTexture2D(self.inner(), a0, a1, a2) };
        unsafe { registry::created(hr, a2, a1) };
        hr
    }

    unsafe fn CreateTexture3D(
        &self,
        a0: *const D3D11_TEXTURE3D_DESC,
        a1: *const D3D11_SUBRESOURCE_DATA,
        a2: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateTexture3D(self.inner(), a0, a1, a2) };
        unsafe { registry::created(hr, a2, a1) };
        hr
    }

    unsafe fn CreateTexture2D1(
        &self,
        a0: *const D3D11_TEXTURE2D_DESC1,
        a1: *const D3D11_SUBRESOURCE_DATA,
        a2: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateTexture2D1(self.inner(), a0, a1, a2) };
        unsafe { registry::created(hr, a2, a1) };
        hr
    }

    unsafe fn CreateTexture3D1(
        &self,
        a0: *const D3D11_TEXTURE3D_DESC1,
        a1: *const D3D11_SUBRESOURCE_DATA,
        a2: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateTexture3D1(self.inner(), a0, a1, a2) };
        unsafe { registry::created(hr, a2, a1) };
        hr
    }

    unsafe fn CreateInputLayout(
        &self,
        a0: *const D3D11_INPUT_ELEMENT_DESC,
        a1: u32,
        a2: *const c_void,
        a3: usize,
        a4: *mut *mut c_void,
    ) -> HRESULT {
        let hr = unsafe { real::CreateInputLayout(self.inner(), a0, a1, a2, a3, a4) };
        unsafe { capture::input_layout_created(hr, a0, a1, a2, a3, a4) };
        hr
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

impl Drop for Device {
    fn drop(&mut self) {
        if let Some(&p) = self.adapter.get()
            && p > 1
        {
            unsafe { AdapterMethods::Release(&*(p as *const Adapter)) };
        }
    }
}

unsafe impl DxgiDeviceMethods for DxgiDevice {
    fn inner(&self) -> *mut c_void {
        self.inner.as_raw()
    }

    unsafe fn QueryInterface(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        unsafe { (*self.device).QueryInterface(iid, out) }
    }

    unsafe fn AddRef(&self) -> u32 {
        unsafe { (*self.device).AddRef() }
    }

    unsafe fn Release(&self) -> u32 {
        unsafe { (*self.device).Release() }
    }

    unsafe fn GetAdapter(&self, a0: *mut *mut c_void) -> HRESULT {
        if a0.is_null() {
            return E_POINTER;
        }
        match unsafe { (*self.device).adapter() } {
            Some(adapter) => unsafe {
                adapter.AddRef();
                *a0 = adapter as *const Adapter as *mut c_void;
                S_OK
            },
            None => E_NOINTERFACE,
        }
    }

    /// The parent of a DXGI device is its adapter.
    unsafe fn GetParent(&self, a0: *const GUID, a1: *mut *mut c_void) -> HRESULT {
        match unsafe { (*self.device).adapter() } {
            Some(adapter) => unsafe { adapter.QueryInterface(a0, a1) },
            None => E_NOINTERFACE,
        }
    }
}

unsafe impl ContextMethods for Context {
    fn inner(&self) -> *mut c_void {
        self.inner.as_raw()
    }

    unsafe fn QueryInterface(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        let kind = if self.deferred {
            "deferred context"
        } else {
            "immediate context"
        };
        unsafe {
            answer_query(
                kind,
                self as *const _ as _,
                self.inner(),
                &CONTEXT_IIDS,
                iid,
                out,
                || {
                    self.AddRef();
                },
            )
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

    unsafe fn Draw(&self, a0: u32, a1: u32) {
        self.draw("Draw", &[a0.into(), a1.into()]);
        self.run(false, || unsafe { real_context::Draw(self.inner(), a0, a1) })
    }

    unsafe fn DrawIndexed(&self, a0: u32, a1: u32, a2: i32) {
        self.draw("DrawIndexed", &[a0.into(), a1.into(), a2.into()]);
        self.run(false, || unsafe {
            real_context::DrawIndexed(self.inner(), a0, a1, a2)
        })
    }

    unsafe fn DrawInstanced(&self, a0: u32, a1: u32, a2: u32, a3: u32) {
        self.draw("DrawInstanced", &[a0.into(), a1.into(), a2.into(), a3.into()]);
        self.run(false, || unsafe {
            real_context::DrawInstanced(self.inner(), a0, a1, a2, a3)
        })
    }

    unsafe fn DrawIndexedInstanced(&self, a0: u32, a1: u32, a2: u32, a3: i32, a4: u32) {
        self.draw(
            "DrawIndexedInstanced",
            &[a0.into(), a1.into(), a2.into(), a3.into(), a4.into()],
        );
        self.run(false, || unsafe {
            real_context::DrawIndexedInstanced(self.inner(), a0, a1, a2, a3, a4)
        })
    }

    unsafe fn DrawAuto(&self) {
        self.draw("DrawAuto", &[]);
        self.run(false, || unsafe { real_context::DrawAuto(self.inner()) })
    }

    unsafe fn DrawInstancedIndirect(&self, a0: *mut c_void, a1: u32) {
        unsafe {
            self.draw_indirect("DrawInstancedIndirect", a0, a1);
            self.run(false, || {
                real_context::DrawInstancedIndirect(self.inner(), a0, a1)
            })
        }
    }

    unsafe fn DrawIndexedInstancedIndirect(&self, a0: *mut c_void, a1: u32) {
        unsafe {
            self.draw_indirect("DrawIndexedInstancedIndirect", a0, a1);
            self.run(false, || {
                real_context::DrawIndexedInstancedIndirect(self.inner(), a0, a1)
            })
        }
    }

    unsafe fn Dispatch(&self, a0: u32, a1: u32, a2: u32) {
        self.draw("Dispatch", &[a0.into(), a1.into(), a2.into()]);
        self.run(true, || unsafe {
            real_context::Dispatch(self.inner(), a0, a1, a2)
        })
    }

    unsafe fn DispatchIndirect(&self, a0: *mut c_void, a1: u32) {
        unsafe {
            self.draw_indirect("DispatchIndirect", a0, a1);
            self.run(true, || real_context::DispatchIndirect(self.inner(), a0, a1))
        }
    }

    unsafe fn Map(
        &self,
        a0: *mut c_void,
        a1: u32,
        a2: D3D11_MAP,
        a3: u32,
        a4: *mut D3D11_MAPPED_SUBRESOURCE,
    ) -> HRESULT {
        unsafe {
            ops::map(self.ctx(), a0, a1, a2, a4, || {
                real_context::Map(self.inner(), a0, a1, a2, a3, a4)
            })
        }
    }

    unsafe fn Unmap(&self, a0: *mut c_void, a1: u32) {
        unsafe { ops::unmap(&self.inner, a0, a1, || real_context::Unmap(self.inner(), a0, a1)) }
    }

    unsafe fn UpdateSubresource(
        &self,
        a0: *mut c_void,
        a1: u32,
        a2: *const D3D11_BOX,
        a3: *const c_void,
        a4: u32,
        a5: u32,
    ) {
        unsafe {
            self.update("UpdateSubresource", a0, a1, a2, a3, a4, a5, 0);
            real_context::UpdateSubresource(self.inner(), a0, a1, a2, a3, a4, a5)
        }
    }

    unsafe fn UpdateSubresource1(
        &self,
        a0: *mut c_void,
        a1: u32,
        a2: *const D3D11_BOX,
        a3: *const c_void,
        a4: u32,
        a5: u32,
        a6: u32,
    ) {
        unsafe {
            self.update("UpdateSubresource1", a0, a1, a2, a3, a4, a5, a6);
            real_context::UpdateSubresource1(self.inner(), a0, a1, a2, a3, a4, a5, a6)
        }
    }

    unsafe fn CopyResource(&self, a0: *mut c_void, a1: *mut c_void) {
        if capture::armed() {
            ops::copy_resource(self.ctx(), a0, a1);
        }
        unsafe { real_context::CopyResource(self.inner(), a0, a1) }
    }

    unsafe fn CopySubresourceRegion(
        &self,
        a0: *mut c_void,
        a1: u32,
        a2: u32,
        a3: u32,
        a4: u32,
        a5: *mut c_void,
        a6: u32,
        a7: *const D3D11_BOX,
    ) {
        if capture::armed() {
            let call = "CopySubresourceRegion";
            ops::copy_region(self.ctx(), call, a0, a1, [a2, a3, a4], a5, a6, a7, 0);
        }
        unsafe { real_context::CopySubresourceRegion(self.inner(), a0, a1, a2, a3, a4, a5, a6, a7) }
    }

    unsafe fn CopySubresourceRegion1(
        &self,
        a0: *mut c_void,
        a1: u32,
        a2: u32,
        a3: u32,
        a4: u32,
        a5: *mut c_void,
        a6: u32,
        a7: *const D3D11_BOX,
        a8: u32,
    ) {
        if capture::armed() {
            let call = "CopySubresourceRegion1";
            ops::copy_region(self.ctx(), call, a0, a1, [a2, a3, a4], a5, a6, a7, a8);
        }
        unsafe { real_context::CopySubresourceRegion1(self.inner(), a0, a1, a2, a3, a4, a5, a6, a7, a8) }
    }

    unsafe fn ClearRenderTargetView(&self, a0: *mut c_void, a1: *const f32) {
        if capture::armed() {
            ops::clear_rtv(self.ctx(), a0, a1);
        }
        unsafe { real_context::ClearRenderTargetView(self.inner(), a0, a1) }
        if capture::armed() {
            capture::after_clear(self.ctx());
        }
    }

    unsafe fn ClearDepthStencilView(&self, a0: *mut c_void, a1: u32, a2: f32, a3: u8) {
        if capture::armed() {
            ops::clear_dsv(self.ctx(), a0, a1, a2, a3);
        }
        unsafe { real_context::ClearDepthStencilView(self.inner(), a0, a1, a2, a3) }
        if capture::armed() {
            capture::after_clear(self.ctx());
        }
    }

    unsafe fn ClearUnorderedAccessViewUint(&self, a0: *mut c_void, a1: *const u32) {
        if capture::armed() {
            let values = if a1.is_null() {
                [0u32; 4]
            } else {
                unsafe { *(a1 as *const [u32; 4]) }
            };
            let call = "ClearUnorderedAccessViewUint";
            ops::clear_uav(self.ctx(), call, a0, values.to_vec().into());
        }
        unsafe { real_context::ClearUnorderedAccessViewUint(self.inner(), a0, a1) }
        if capture::armed() {
            capture::after_clear(self.ctx());
        }
    }

    unsafe fn ClearUnorderedAccessViewFloat(&self, a0: *mut c_void, a1: *const f32) {
        if capture::armed() {
            let values = if a1.is_null() {
                [0f32; 4]
            } else {
                unsafe { *(a1 as *const [f32; 4]) }
            };
            let call = "ClearUnorderedAccessViewFloat";
            ops::clear_uav(self.ctx(), call, a0, values.to_vec().into());
        }
        unsafe { real_context::ClearUnorderedAccessViewFloat(self.inner(), a0, a1) }
        if capture::armed() {
            capture::after_clear(self.ctx());
        }
    }

    unsafe fn ClearView(&self, a0: *mut c_void, a1: *const f32, a2: *const RECT, a3: u32) {
        if capture::armed() {
            ops::clear_view(self.ctx(), a0, a1, a2, a3);
        }
        unsafe { real_context::ClearView(self.inner(), a0, a1, a2, a3) }
        if capture::armed() {
            capture::after_clear(self.ctx());
        }
    }

    unsafe fn ResolveSubresource(&self, a0: *mut c_void, a1: u32, a2: *mut c_void, a3: u32, a4: DXGI_FORMAT) {
        if capture::armed() {
            ops::resolve(self.ctx(), a0, a1, a2, a3, a4.0);
        }
        unsafe { real_context::ResolveSubresource(self.inner(), a0, a1, a2, a3, a4) }
    }

    unsafe fn GenerateMips(&self, a0: *mut c_void) {
        if capture::armed() {
            ops::generate_mips(self.ctx(), a0);
        }
        unsafe { real_context::GenerateMips(self.inner(), a0) }
    }

    unsafe fn CopyStructureCount(&self, a0: *mut c_void, a1: u32, a2: *mut c_void) {
        if capture::armed() {
            ops::copy_structure_count(self.ctx(), a0, a1, a2);
        }
        unsafe { real_context::CopyStructureCount(self.inner(), a0, a1, a2) }
    }

    unsafe fn DiscardResource(&self, a0: *mut c_void) {
        if capture::armed() {
            ops::discard(self.ctx(), "DiscardResource", a0);
        }
        unsafe { real_context::DiscardResource(self.inner(), a0) }
    }

    unsafe fn DiscardView(&self, a0: *mut c_void) {
        if capture::armed() {
            ops::discard(self.ctx(), "DiscardView", a0);
        }
        unsafe { real_context::DiscardView(self.inner(), a0) }
    }

    unsafe fn DiscardView1(&self, a0: *mut c_void, a1: *const RECT, a2: u32) {
        if capture::armed() {
            ops::discard(self.ctx(), "DiscardView1", a0);
        }
        unsafe { real_context::DiscardView1(self.inner(), a0, a1, a2) }
    }

    unsafe fn CSSetUnorderedAccessViews(&self, a0: u32, a1: u32, a2: *const *mut c_void, a3: *const u32) {
        if capture::armed() {
            let call = "CSSetUnorderedAccessViews";
            ops::uav_counters(self.ctx(), call, a0, a2, a3, a1);
        }
        unsafe { real_context::CSSetUnorderedAccessViews(self.inner(), a0, a1, a2, a3) }
    }

    unsafe fn OMSetRenderTargets(&self, a0: u32, a1: *const *mut c_void, a2: *mut c_void) {
        unsafe { self.targets_changing(a0, a1, a2) };
        unsafe { real_context::OMSetRenderTargets(self.inner(), a0, a1, a2) }
    }

    unsafe fn OMSetRenderTargetsAndUnorderedAccessViews(
        &self,
        a0: u32,
        a1: *const *mut c_void,
        a2: *mut c_void,
        a3: u32,
        a4: u32,
        a5: *const *mut c_void,
        a6: *const u32,
    ) {
        if a0 != D3D11_KEEP_RENDER_TARGETS_AND_DEPTH_STENCIL {
            unsafe { self.targets_changing(a0, a1, a2) };
        }
        if capture::armed() && a4 != D3D11_KEEP_UNORDERED_ACCESS_VIEWS {
            let call = "OMSetRenderTargetsAndUnorderedAccessViews";
            ops::uav_counters(self.ctx(), call, a3, a5, a6, a4);
        }
        unsafe {
            real_context::OMSetRenderTargetsAndUnorderedAccessViews(self.inner(), a0, a1, a2, a3, a4, a5, a6)
        }
    }

    unsafe fn ClearState(&self) {
        unsafe { self.targets_changing(0, core::ptr::null(), core::ptr::null_mut()) };
        unsafe { real_context::ClearState(self.inner()) }
    }

    unsafe fn FinishCommandList(&self, a0: BOOL, a1: *mut *mut c_void) -> HRESULT {
        let hr = unsafe { real_context::FinishCommandList(self.inner(), a0, a1) };
        if capture::armed() && hr.is_ok() && !a1.is_null() {
            capture::on_finish_list(self.id, unsafe { *a1 });
        }
        hr
    }

    unsafe fn ExecuteCommandList(&self, a0: *mut c_void, a1: BOOL) {
        if capture::armed() {
            capture::on_execute_list(self.ctx(), a0);
        }
        unsafe { real_context::ExecuteCommandList(self.inner(), a0, a1) }
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
