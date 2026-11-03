//! The DXGI factory, its adapters and the swap chains it makes.
//!
//! A swap chain is created from a device: the factory gives the real DXGI the real device under our wrapper, and
//! wraps the swap chain it gets back so that `GetDevice` returns our device and every `Present` passes through
//! locum (the frame boundary).
//!
//! Adapters are wrapped so that the way from a device to its factory (`IDXGIDevice::GetAdapter`, then
//! `IDXGIAdapter::GetParent`) ends at one of our factories; a game that makes its swap chain from that factory gets
//! one of our swap chains.

use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LUID};
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

use crate::com::answer_query;
use crate::d3d11::capture;
use crate::d3d11::device::Device;
use crate::log::log;
use crate::vtables::adapter::{AdapterMethods, real as real_adapter};
use crate::vtables::factory::{FactoryMethods, real as real_factory};
use crate::vtables::swapchain::{SwapChainMethods, real as real_swapchain};

#[repr(C)]
pub struct Factory {
    vtbl: *const IDXGIFactory7_Vtbl,
    refs: AtomicU32,
    inner: IDXGIFactory7,
}

#[repr(C)]
pub struct SwapChain {
    vtbl: *const IDXGISwapChain4_Vtbl,
    refs: AtomicU32,
    inner: IDXGISwapChain4,
    /// Our device, with one reference, or null when the swap chain was made for a device we do not wrap.
    device: *const Device,
    /// The factory that made it, with one reference, or null.
    factory: *const Factory,
}

#[repr(C)]
pub struct Adapter {
    vtbl: *const IDXGIAdapter4_Vtbl,
    refs: AtomicU32,
    inner: IDXGIAdapter4,
    /// Our factory, its parent, with one reference: the factory that enumerated it, or else the real parent wrapped
    /// when first asked for (0 until then, 1 if that failed).
    factory: OnceLock<usize>,
}

static FACTORY_VTBL: IDXGIFactory7_Vtbl = crate::vtables::factory::vtable::<Factory>();
static ADAPTER_VTBL: IDXGIAdapter4_Vtbl = crate::vtables::adapter::vtable::<Adapter>();
static SWAPCHAIN_VTBL: IDXGISwapChain4_Vtbl = crate::vtables::swapchain::vtable::<SwapChain>();

const ADAPTER_IIDS: [GUID; 7] = [
    IUnknown::IID,
    IDXGIObject::IID,
    IDXGIAdapter::IID,
    IDXGIAdapter1::IID,
    IDXGIAdapter2::IID,
    IDXGIAdapter3::IID,
    IDXGIAdapter4::IID,
];

const FACTORY_IIDS: [GUID; 10] = [
    IUnknown::IID,
    IDXGIObject::IID,
    IDXGIFactory::IID,
    IDXGIFactory1::IID,
    IDXGIFactory2::IID,
    IDXGIFactory3::IID,
    IDXGIFactory4::IID,
    IDXGIFactory5::IID,
    IDXGIFactory6::IID,
    IDXGIFactory7::IID,
];

const SWAPCHAIN_IIDS: [GUID; 8] = [
    IUnknown::IID,
    IDXGIObject::IID,
    IDXGIDeviceSubObject::IID,
    IDXGISwapChain::IID,
    IDXGISwapChain1::IID,
    IDXGISwapChain2::IID,
    IDXGISwapChain3::IID,
    IDXGISwapChain4::IID,
];

impl Factory {
    /// Wraps a factory the real DXGI created, taking over the caller's reference. Gives the factory back if it is
    /// not an `IDXGIFactory7`.
    pub fn wrap(factory: IUnknown) -> Result<*mut Factory, IUnknown> {
        let Ok(inner) = factory.cast::<IDXGIFactory7>() else {
            log!("factory is not an IDXGIFactory7: left unwrapped");
            return Err(factory);
        };
        let wrapper = Box::into_raw(Box::new(Factory {
            vtbl: &FACTORY_VTBL,
            refs: AtomicU32::new(1),
            inner,
        }));
        log!("wrapped factory {:p} as {wrapper:p}", factory.as_raw());
        Ok(wrapper)
    }

    /// Replaces the real swap chain the factory wrote to `out` with a wrapper bound to the device the game passed.
    unsafe fn wrap_swapchain(&self, hr: HRESULT, device: *mut c_void, out: *mut *mut c_void) -> HRESULT {
        if hr.is_err() || out.is_null() || unsafe { (*out).is_null() } {
            return hr;
        }
        let real = unsafe { IUnknown::from_raw(*out) };
        let ours = Device::from_raw(device);
        if ours.is_none() {
            log!("swap chain made for a device locum does not wrap ({device:p}): left unwrapped");
            unsafe { *out = real.into_raw() };
            return hr;
        }
        match SwapChain::wrap(real, ours, Some(self)) {
            Ok(w) => unsafe { *out = w.cast() },
            Err(real) => unsafe { *out = real.into_raw() },
        }
        hr
    }

    /// Replaces the real adapter an enumeration wrote to `out` with a wrapper whose parent is this factory.
    unsafe fn wrap_adapter(&self, hr: HRESULT, out: *mut *mut c_void) -> HRESULT {
        if hr.is_err() || out.is_null() || unsafe { (*out).is_null() } {
            return hr;
        }
        let real = unsafe { IUnknown::from_raw(*out) };
        match Adapter::wrap(real, Some(self)) {
            Ok(w) => unsafe { *out = w.cast() },
            Err(real) => unsafe { *out = real.into_raw() },
        }
        hr
    }
}

impl Adapter {
    /// Wraps a real adapter, taking over the caller's reference; `factory`, its parent if known, gains a reference.
    pub fn wrap(adapter: IUnknown, factory: Option<&Factory>) -> Result<*mut Adapter, IUnknown> {
        let Ok(inner) = adapter.cast::<IDXGIAdapter4>() else {
            log!("adapter is not an IDXGIAdapter4: left unwrapped");
            return Err(adapter);
        };
        let parent = OnceLock::new();
        if let Some(f) = factory {
            unsafe { f.AddRef() };
            let _ = parent.set(f as *const Factory as usize);
        }
        let wrapper = Box::into_raw(Box::new(Adapter {
            vtbl: &ADAPTER_VTBL,
            refs: AtomicU32::new(1),
            inner,
            factory: parent,
        }));
        log!("wrapped adapter {:p} as {wrapper:p}", adapter.as_raw());
        Ok(wrapper)
    }

    /// If `object` is one of our adapters, the real adapter under it; otherwise `object` unchanged.
    pub fn unwrap_raw(object: *mut c_void) -> *mut c_void {
        if object.is_null() {
            return object;
        }
        let vtbl = unsafe { *(object as *const *const IDXGIAdapter4_Vtbl) };
        if core::ptr::eq(vtbl, &ADAPTER_VTBL) {
            unsafe { (*(object as *const Adapter)).inner.as_raw() }
        } else {
            object
        }
    }

    /// Our factory for this adapter's parent.
    fn factory(&self) -> Option<&Factory> {
        let p = *self.factory.get_or_init(|| {
            let real = match unsafe { self.inner.GetParent::<IDXGIFactory7>() } {
                Ok(f) => f,
                Err(e) => {
                    log!("IDXGIAdapter::GetParent on the real adapter failed: {e}");
                    return 1;
                }
            };
            match Factory::wrap(real.into()) {
                Ok(w) => w as usize,
                Err(_) => 1,
            }
        });
        (p > 1).then(|| unsafe { &*(p as *const Factory) })
    }
}

impl SwapChain {
    /// Wraps a real swap chain, taking over the caller's reference; `device` and `factory` gain a reference each.
    pub fn wrap(
        swapchain: IUnknown,
        device: Option<&Device>,
        factory: Option<&Factory>,
    ) -> Result<*mut SwapChain, IUnknown> {
        let Ok(inner) = swapchain.cast::<IDXGISwapChain4>() else {
            log!("swap chain is not an IDXGISwapChain4: left unwrapped");
            return Err(swapchain);
        };
        let device = device.map_or(core::ptr::null(), |d| {
            unsafe { crate::vtables::device::DeviceMethods::AddRef(d) };
            d as *const Device
        });
        let factory = factory.map_or(core::ptr::null(), |f| {
            unsafe { f.AddRef() };
            f as *const Factory
        });
        let wrapper = Box::into_raw(Box::new(SwapChain {
            vtbl: &SWAPCHAIN_VTBL,
            refs: AtomicU32::new(1),
            inner,
            device,
            factory,
        }));
        log!("wrapped swap chain {:p} as {wrapper:p}", swapchain.as_raw());
        Ok(wrapper)
    }
}

unsafe impl FactoryMethods for Factory {
    fn inner(&self) -> *mut c_void {
        self.inner.as_raw()
    }

    unsafe fn QueryInterface(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        unsafe {
            answer_query(
                "factory",
                self as *const _ as _,
                self.inner(),
                &FACTORY_IIDS,
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
            drop(unsafe { Box::from_raw(self as *const Factory as *mut Factory) });
        }
        left
    }

    unsafe fn CreateSwapChain(
        &self,
        a0: *mut c_void,
        a1: *const DXGI_SWAP_CHAIN_DESC,
        a2: *mut *mut c_void,
    ) -> HRESULT {
        unsafe {
            let hr = real_factory::CreateSwapChain(self.inner(), Device::unwrap_raw(a0), a1, a2);
            self.wrap_swapchain(hr, a0, a2)
        }
    }

    unsafe fn CreateSwapChainForHwnd(
        &self,
        a0: *mut c_void,
        a1: HWND,
        a2: *const DXGI_SWAP_CHAIN_DESC1,
        a3: *const DXGI_SWAP_CHAIN_FULLSCREEN_DESC,
        a4: *mut c_void,
        a5: *mut *mut c_void,
    ) -> HRESULT {
        unsafe {
            let hr = real_factory::CreateSwapChainForHwnd(
                self.inner(),
                Device::unwrap_raw(a0),
                a1,
                a2,
                a3,
                a4,
                a5,
            );
            self.wrap_swapchain(hr, a0, a5)
        }
    }

    unsafe fn CreateSwapChainForCoreWindow(
        &self,
        a0: *mut c_void,
        a1: *mut c_void,
        a2: *const DXGI_SWAP_CHAIN_DESC1,
        a3: *mut c_void,
        a4: *mut *mut c_void,
    ) -> HRESULT {
        unsafe {
            let hr = real_factory::CreateSwapChainForCoreWindow(
                self.inner(),
                Device::unwrap_raw(a0),
                a1,
                a2,
                a3,
                a4,
            );
            self.wrap_swapchain(hr, a0, a4)
        }
    }

    unsafe fn CreateSwapChainForComposition(
        &self,
        a0: *mut c_void,
        a1: *const DXGI_SWAP_CHAIN_DESC1,
        a2: *mut c_void,
        a3: *mut *mut c_void,
    ) -> HRESULT {
        unsafe {
            let hr =
                real_factory::CreateSwapChainForComposition(self.inner(), Device::unwrap_raw(a0), a1, a2, a3);
            self.wrap_swapchain(hr, a0, a3)
        }
    }

    unsafe fn EnumAdapters(&self, a0: u32, a1: *mut *mut c_void) -> HRESULT {
        unsafe { self.wrap_adapter(real_factory::EnumAdapters(self.inner(), a0, a1), a1) }
    }

    unsafe fn EnumAdapters1(&self, a0: u32, a1: *mut *mut c_void) -> HRESULT {
        unsafe { self.wrap_adapter(real_factory::EnumAdapters1(self.inner(), a0, a1), a1) }
    }

    unsafe fn EnumAdapterByLuid(&self, a0: LUID, a1: *const GUID, a2: *mut *mut c_void) -> HRESULT {
        unsafe { self.wrap_adapter(real_factory::EnumAdapterByLuid(self.inner(), a0, a1, a2), a2) }
    }

    unsafe fn EnumWarpAdapter(&self, a0: *const GUID, a1: *mut *mut c_void) -> HRESULT {
        unsafe { self.wrap_adapter(real_factory::EnumWarpAdapter(self.inner(), a0, a1), a1) }
    }

    unsafe fn EnumAdapterByGpuPreference(
        &self,
        a0: u32,
        a1: DXGI_GPU_PREFERENCE,
        a2: *const GUID,
        a3: *mut *mut c_void,
    ) -> HRESULT {
        unsafe {
            self.wrap_adapter(
                real_factory::EnumAdapterByGpuPreference(self.inner(), a0, a1, a2, a3),
                a3,
            )
        }
    }
}

unsafe impl AdapterMethods for Adapter {
    fn inner(&self) -> *mut c_void {
        self.inner.as_raw()
    }

    unsafe fn QueryInterface(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        unsafe {
            answer_query(
                "adapter",
                self as *const _ as _,
                self.inner(),
                &ADAPTER_IIDS,
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
            let parent = self.factory.get().copied();
            drop(unsafe { Box::from_raw(self as *const Adapter as *mut Adapter) });
            if let Some(p) = parent
                && p > 1
            {
                unsafe { (*(p as *const Factory)).Release() };
            }
        }
        left
    }

    /// The parent of an adapter is its factory: ours.
    unsafe fn GetParent(&self, a0: *const GUID, a1: *mut *mut c_void) -> HRESULT {
        match self.factory() {
            Some(factory) => unsafe { factory.QueryInterface(a0, a1) },
            None => unsafe { real_adapter::GetParent(self.inner(), a0, a1) },
        }
    }
}

unsafe impl SwapChainMethods for SwapChain {
    fn inner(&self) -> *mut c_void {
        self.inner.as_raw()
    }

    unsafe fn QueryInterface(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        unsafe {
            answer_query(
                "swap chain",
                self as *const _ as _,
                self.inner(),
                &SWAPCHAIN_IIDS,
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
            let (device, factory) = (self.device, self.factory);
            drop(unsafe { Box::from_raw(self as *const SwapChain as *mut SwapChain) });
            unsafe {
                if !device.is_null() {
                    crate::vtables::device::DeviceMethods::Release(&*device);
                }
                if !factory.is_null() {
                    (*factory).Release();
                }
            }
        }
        left
    }

    unsafe fn GetDevice(&self, a0: *const GUID, a1: *mut *mut c_void) -> HRESULT {
        if self.device.is_null() {
            return unsafe { real_swapchain::GetDevice(self.inner(), a0, a1) };
        }
        unsafe { crate::vtables::device::DeviceMethods::QueryInterface(&*self.device, a0, a1) }
    }

    unsafe fn GetParent(&self, a0: *const GUID, a1: *mut *mut c_void) -> HRESULT {
        if self.factory.is_null() {
            return unsafe { real_swapchain::GetParent(self.inner(), a0, a1) };
        }
        unsafe { (*self.factory).QueryInterface(a0, a1) }
    }

    unsafe fn Present(&self, a0: u32, a1: DXGI_PRESENT) -> HRESULT {
        self.present(a1, || unsafe { real_swapchain::Present(self.inner(), a0, a1) })
    }

    unsafe fn Present1(&self, a0: u32, a1: DXGI_PRESENT, a2: *const DXGI_PRESENT_PARAMETERS) -> HRESULT {
        self.present(a1, || unsafe {
            real_swapchain::Present1(self.inner(), a0, a1, a2)
        })
    }
}

impl SwapChain {
    /// A present: the frame boundary for the frame counter and for captures. `TEST` presents show nothing and are
    /// passed straight through.
    fn present(&self, flags: DXGI_PRESENT, real: impl FnOnce() -> HRESULT) -> HRESULT {
        if flags.contains(DXGI_PRESENT_TEST) || self.device.is_null() {
            return real();
        }
        let frame = count_frame();
        crate::d3d11::overrides::poll(frame);
        let immediate = unsafe { (*self.device).immediate() };
        capture::end_frame(immediate.real());
        let finished = capture::before_present();
        let hr = real();
        capture::after_present(finished, immediate.real());
        hr
    }
}

static FRAMES: AtomicU64 = AtomicU64::new(0);

/// The frame being made: 1 until the first present, then the number of the present that will end it (captures name
/// frames the same way).
pub fn frame() -> u64 {
    FRAMES.load(Ordering::Relaxed) + 1
}

/// Counts a frame and logs the frame rate every 600.
fn count_frame() -> u64 {
    static SINCE: Mutex<Option<Instant>> = Mutex::new(None);
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if frame == 1 || frame.is_multiple_of(600) {
        let mut since = SINCE.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if let Some(t) = *since {
            let secs = now.duration_since(t).as_secs_f64();
            log!("frame {frame}: {:.2} frames/s over the last 600", 600.0 / secs);
            log!("  {}", crate::cost::line());
        } else {
            log!("first present");
        }
        *since = Some(now);
    }
    frame
}
