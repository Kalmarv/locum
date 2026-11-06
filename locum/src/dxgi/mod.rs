//! The DXGI factory, its adapters and the swap chains it makes.
//!
//! Each wrapper is a face of its real object ([`crate::faces`]): its count is the object's, it is made once per
//! object, and it is freed when the object is destroyed. A swap chain is created from a device: the factory gives the
//! real DXGI the real device under our wrapper, and wraps the swap chain it gets back so that every `Present` passes
//! through locum (the frame boundary). Whatever hands out a parent (an adapter's factory, a swap chain's device, a
//! device's adapter, an output's adapter) asks the real object and gives locum's face of the answer ([`give`]), so
//! the way from a device to its factory (`IDXGIDevice::GetAdapter`, then `IDXGIAdapter::GetParent`) ends at one of
//! our factories, and a game that makes its swap chain from that factory gets one of our swap chains.

use core::ffi::c_void;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use windows::Win32::Foundation::{HWND, LUID};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceContext4};
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

use crate::com::answer_query;
use crate::d3d11::capture;
use crate::d3d11::device::Device;
use crate::faces::{self, Kind, unowned};
use crate::log::log;
use crate::vtables::adapter::{AdapterMethods, real as real_adapter};
use crate::vtables::device::DeviceMethods;
use crate::vtables::factory::{FactoryMethods, real as real_factory};
use crate::vtables::swapchain::{SwapChainMethods, real as real_swapchain};

#[repr(C)]
pub struct Factory {
    vtbl: *const IDXGIFactory7_Vtbl,
    /// The real factory, without a reference of locum's.
    inner: ManuallyDrop<IDXGIFactory7>,
}

/// The API that presents through a swap chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    D3d11,
    /// With locum's number of the queue it presents on.
    D3d12 {
        queue: u64,
    },
}

#[repr(C)]
pub struct SwapChain {
    vtbl: *const IDXGISwapChain4_Vtbl,
    /// The real swap chain, without a reference of locum's.
    inner: ManuallyDrop<IDXGISwapChain4>,
    api: Api,
}

#[repr(C)]
pub struct Adapter {
    vtbl: *const IDXGIAdapter4_Vtbl,
    /// The real adapter, without a reference of locum's.
    inner: ManuallyDrop<IDXGIAdapter4>,
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

/// The wrapper of `real`'s object: the one locum has, or one `make` makes (with the pointers of the object to find
/// it by) the first time. Takes over the caller's reference to `real`, which counts on the object for the wrapper from
/// then on; gives `real` back if `make` cannot wrap it.
fn wrap<T: 'static>(
    kind: Kind,
    real: IUnknown,
    make: impl FnOnce(&IUnknown) -> Option<(T, Vec<*mut c_void>)>,
) -> Result<*mut T, IUnknown> {
    let raw = real.as_raw();
    let made = unsafe {
        faces::adopt(raw, || {
            let (wrapper, pointers) = make(&real)?;
            let address = Box::into_raw(Box::new(wrapper)) as usize;
            faces::register(kind, address, &pointers);
            faces::watch(raw, move || {
                faces::forget(address);
                faces::later(Box::from_raw(address as *mut T));
            });
            log!("wrapped {kind} {raw:p} as {address:#x}");
            Some(address)
        })
    };
    match made {
        Some(address) => {
            core::mem::forget(real);
            Ok(address as *mut T)
        }
        None => Err(real),
    }
}

/// Our face `iid` of the wrapper at `address`, of kind `kind`, with a reference.
unsafe fn face(kind: Kind, address: usize, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    unsafe {
        match kind {
            Kind::Device => DeviceMethods::QueryInterface(&*(address as *const Device), iid, out),
            Kind::Factory => FactoryMethods::QueryInterface(&*(address as *const Factory), iid, out),
            Kind::Adapter => AdapterMethods::QueryInterface(&*(address as *const Adapter), iid, out),
            Kind::SwapChain => SwapChainMethods::QueryInterface(&*(address as *const SwapChain), iid, out),
        }
    }
}

/// Replaces the real object a successful call wrote to `out` (as interface `iid`) with locum's face of it: of a
/// device, factory, adapter or swap chain locum wrapped, or of an adapter or factory it had not seen, wrapped now.
/// Anything else, or an interface the wrapper does not take, stays the real object. Returns `hr`. For a call on
/// one of locum's wrappers: what it hands out is of the family locum wraps.
///
/// # Safety
/// `iid` and `out` must be what the call was given.
pub unsafe fn give(hr: HRESULT, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    unsafe { hand(hr, iid, out, true) }
}

/// As [`give`], but only an object locum already wrapped becomes its face; one it had not seen stays the real
/// object. For a call on the runtime's own object through a slot locum changed ([`crate::d3d11::children`]): the
/// table serves every object of its class, the runtime's own devices' too, and theirs are not locum's to wrap.
///
/// # Safety
/// `iid` and `out` must be what the call was given.
pub unsafe fn give_known(hr: HRESULT, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    unsafe { hand(hr, iid, out, false) }
}

unsafe fn hand(hr: HRESULT, iid: *const GUID, out: *mut *mut c_void, wrap_new: bool) -> HRESULT {
    if hr.is_err() || iid.is_null() || out.is_null() || unsafe { (*out).is_null() } {
        return hr;
    }
    let real = unsafe { *out };
    let found = unsafe { faces::find(real) }.or_else(|| {
        if !wrap_new {
            return None;
        }
        let object = unsafe { IUnknown::from_raw_borrowed(&real) }?;
        let made = if object.cast::<IDXGIAdapter>().is_ok() {
            Adapter::wrap(object.clone())
                .ok()
                .map(|w| (Kind::Adapter, w as usize))
        } else if object.cast::<IDXGIFactory>().is_ok() {
            Factory::wrap(object.clone())
                .ok()
                .map(|w| (Kind::Factory, w as usize))
        } else {
            None
        };
        if made.is_some() {
            // The new wrapper kept the clone's reference: the object counts one more than the call gave.
            unsafe { faces::release(real) };
        }
        made
    });
    let Some((kind, address)) = found else {
        return hr;
    };
    let mut ours = core::ptr::null_mut();
    if unsafe { face(kind, address, iid, &mut ours) }.is_ok() {
        unsafe {
            *out = ours;
            // The face took a reference of its own, so the call's goes; the object lives on in the face's.
            faces::release(real);
        }
    }
    hr
}

impl Factory {
    /// The wrapper of a real factory, made the first time; takes over the caller's reference. Gives the factory back
    /// if it is not an `IDXGIFactory7`.
    pub fn wrap(factory: IUnknown) -> Result<*mut Factory, IUnknown> {
        wrap(Kind::Factory, factory, |real| {
            let inner = real.cast::<IDXGIFactory7>().ok()?;
            let pointers = vec![inner.as_raw()];
            Some((
                Factory {
                    vtbl: &FACTORY_VTBL,
                    inner: unowned(inner),
                },
                pointers,
            ))
        })
        .inspect_err(|_| log!("factory is not an IDXGIFactory7: left unwrapped"))
    }

    /// Replaces the real swap chain the factory wrote to `out` with a wrapper, if the game passed one of our devices
    /// or queues.
    unsafe fn wrap_swapchain(&self, hr: HRESULT, device: *mut c_void, out: *mut *mut c_void) -> HRESULT {
        if hr.is_err() || out.is_null() || unsafe { (*out).is_null() } {
            return hr;
        }
        let real = unsafe { IUnknown::from_raw(*out) };
        let api = if Device::from_raw(device).is_some() {
            Api::D3d11
        } else if crate::d3d12::is_wrapped(device) {
            Api::D3d12 {
                queue: crate::d3d12::id(device),
            }
        } else {
            log!("swap chain made for an unwrapped owner ({device:p}): left unwrapped");
            unsafe { *out = real.into_raw() };
            return hr;
        };
        unsafe {
            *out = match SwapChain::wrap(real, api) {
                Ok(w) => w.cast(),
                Err(real) => real.into_raw(),
            }
        };
        hr
    }
}

impl Adapter {
    /// The wrapper of a real adapter, made the first time; takes over the caller's reference.
    pub fn wrap(adapter: IUnknown) -> Result<*mut Adapter, IUnknown> {
        wrap(Kind::Adapter, adapter, |real| {
            let inner = real.cast::<IDXGIAdapter4>().ok()?;
            let pointers = vec![inner.as_raw()];
            Some((
                Adapter {
                    vtbl: &ADAPTER_VTBL,
                    inner: unowned(inner),
                },
                pointers,
            ))
        })
        .inspect_err(|_| log!("adapter is not an IDXGIAdapter4: left unwrapped"))
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
}

impl SwapChain {
    /// The wrapper of a real swap chain `api` presents through, made the first time; takes over the caller's
    /// reference.
    pub fn wrap(swapchain: IUnknown, api: Api) -> Result<*mut SwapChain, IUnknown> {
        wrap(Kind::SwapChain, swapchain, |real| {
            let inner = real.cast::<IDXGISwapChain4>().ok()?;
            let pointers = vec![inner.as_raw()];
            Some((
                SwapChain {
                    vtbl: &SWAPCHAIN_VTBL,
                    inner: unowned(inner),
                    api,
                },
                pointers,
            ))
        })
        .inspect_err(|_| log!("swap chain is not an IDXGISwapChain4: left unwrapped"))
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
        unsafe { faces::add_ref(self.inner()) }
    }

    unsafe fn Release(&self) -> u32 {
        unsafe { faces::release(self.inner()) }
    }

    unsafe fn CreateSwapChain(
        &self,
        a0: *mut c_void,
        a1: *const DXGI_SWAP_CHAIN_DESC,
        a2: *mut *mut c_void,
    ) -> HRESULT {
        unsafe {
            let hr = real_factory::CreateSwapChain(
                self.inner(),
                crate::d3d12::unwrap(Device::unwrap_raw(a0)),
                a1,
                a2,
            );
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
                crate::d3d12::unwrap(Device::unwrap_raw(a0)),
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
                crate::d3d12::unwrap(Device::unwrap_raw(a0)),
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
            let hr = real_factory::CreateSwapChainForComposition(
                self.inner(),
                crate::d3d12::unwrap(Device::unwrap_raw(a0)),
                a1,
                a2,
                a3,
            );
            self.wrap_swapchain(hr, a0, a3)
        }
    }

    unsafe fn EnumAdapters(&self, a0: u32, a1: *mut *mut c_void) -> HRESULT {
        unsafe {
            give(
                real_factory::EnumAdapters(self.inner(), a0, a1),
                &IDXGIAdapter::IID,
                a1,
            )
        }
    }

    unsafe fn EnumAdapters1(&self, a0: u32, a1: *mut *mut c_void) -> HRESULT {
        unsafe {
            give(
                real_factory::EnumAdapters1(self.inner(), a0, a1),
                &IDXGIAdapter1::IID,
                a1,
            )
        }
    }

    unsafe fn EnumAdapterByLuid(&self, a0: LUID, a1: *const GUID, a2: *mut *mut c_void) -> HRESULT {
        unsafe { give(real_factory::EnumAdapterByLuid(self.inner(), a0, a1, a2), a1, a2) }
    }

    unsafe fn EnumWarpAdapter(&self, a0: *const GUID, a1: *mut *mut c_void) -> HRESULT {
        unsafe { give(real_factory::EnumWarpAdapter(self.inner(), a0, a1), a0, a1) }
    }

    unsafe fn EnumAdapterByGpuPreference(
        &self,
        a0: u32,
        a1: DXGI_GPU_PREFERENCE,
        a2: *const GUID,
        a3: *mut *mut c_void,
    ) -> HRESULT {
        unsafe {
            give(
                real_factory::EnumAdapterByGpuPreference(self.inner(), a0, a1, a2, a3),
                a2,
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
        unsafe { faces::add_ref(self.inner()) }
    }

    unsafe fn Release(&self) -> u32 {
        unsafe { faces::release(self.inner()) }
    }

    /// The parent of an adapter is its factory: ours.
    unsafe fn GetParent(&self, a0: *const GUID, a1: *mut *mut c_void) -> HRESULT {
        unsafe { give(real_adapter::GetParent(self.inner(), a0, a1), a0, a1) }
    }

    /// An output is the runtime's; its way back to the adapter gives ours.
    unsafe fn EnumOutputs(&self, a0: u32, a1: *mut *mut c_void) -> HRESULT {
        let hr = unsafe { real_adapter::EnumOutputs(self.inner(), a0, a1) };
        if hr.is_ok() && !a1.is_null() {
            unsafe { crate::d3d11::children::adopt_output(*a1) };
        }
        hr
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
        unsafe { faces::add_ref(self.inner()) }
    }

    unsafe fn Release(&self) -> u32 {
        unsafe { faces::release(self.inner()) }
    }

    unsafe fn GetDevice(&self, a0: *const GUID, a1: *mut *mut c_void) -> HRESULT {
        let hr = unsafe { real_swapchain::GetDevice(self.inner(), a0, a1) };
        match self.api {
            Api::D3d11 => unsafe { give(hr, a0, a1) },
            Api::D3d12 { .. } => {
                unsafe { crate::d3d12::wrap_output(hr, a0, a1) };
                hr
            }
        }
    }

    unsafe fn GetBuffer(&self, index: u32, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        let hr = unsafe { real_swapchain::GetBuffer(self.inner(), index, iid, out) };
        if hr.is_ok() && self.api == Api::D3d11 && !out.is_null() {
            unsafe { crate::d3d11::children::adopt(*out) };
        }
        if let Api::D3d12 { queue } = self.api {
            unsafe { crate::d3d12::wrap_output(hr, iid, out) };
            if hr.is_ok() && crate::d3d12::journal::enabled() {
                let device = crate::d3d12::journal::owner_device(queue);
                // A back buffer begins in PRESENT, which is COMMON (0).
                let initial = crate::d3d12::values::Initial::State(0);
                unsafe {
                    crate::d3d12::values::created_resource(
                        device,
                        out,
                        initial,
                        crate::d3d12::values::Placement::Committed,
                    )
                };
            }
        }
        hr
    }

    unsafe fn GetParent(&self, a0: *const GUID, a1: *mut *mut c_void) -> HRESULT {
        unsafe { give(real_swapchain::GetParent(self.inner(), a0, a1), a0, a1) }
    }

    unsafe fn GetContainingOutput(&self, a0: *mut *mut c_void) -> HRESULT {
        let hr = unsafe { real_swapchain::GetContainingOutput(self.inner(), a0) };
        if hr.is_ok() && !a0.is_null() {
            unsafe { crate::d3d11::children::adopt_output(*a0) };
        }
        hr
    }

    unsafe fn Present(&self, a0: u32, a1: DXGI_PRESENT) -> HRESULT {
        self.present(a1, || unsafe { real_swapchain::Present(self.inner(), a0, a1) })
    }

    unsafe fn Present1(&self, a0: u32, a1: DXGI_PRESENT, a2: *const DXGI_PRESENT_PARAMETERS) -> HRESULT {
        self.present(a1, || unsafe {
            real_swapchain::Present1(self.inner(), a0, a1, a2)
        })
    }

    // A resize needs every reference to the back buffers released; a capture holds some, so it is given up first.
    unsafe fn ResizeBuffers(
        &self,
        a0: u32,
        a1: u32,
        a2: u32,
        a3: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        a4: u32,
    ) -> HRESULT {
        capture::abandon("the game resized its swap chain");
        unsafe { real_swapchain::ResizeBuffers(self.inner(), a0, a1, a2, a3, a4) }
    }

    unsafe fn ResizeBuffers1(
        &self,
        a0: u32,
        a1: u32,
        a2: u32,
        a3: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        a4: u32,
        a5: *const u32,
        a6: *const *mut c_void,
    ) -> HRESULT {
        capture::abandon("the game resized its swap chain");
        // Zero preserves the existing buffer count; the queue array still has
        // that many entries. Do not hand DXGI an empty temporary array.
        let count = if a0 == 0 && !a6.is_null() {
            let mut desc = DXGI_SWAP_CHAIN_DESC::default();
            let hr = unsafe { real_swapchain::GetDesc(self.inner(), &mut desc) };
            if hr.is_err() {
                return hr;
            }
            desc.BufferCount
        } else {
            a0
        };
        let queues = unsafe { crate::d3d12::inputs::objects(a6, count) };
        unsafe { real_swapchain::ResizeBuffers1(self.inner(), a0, a1, a2, a3, a4, a5, queues.ptr()) }
    }
}

impl SwapChain {
    /// The locum number of the back buffer a Direct3D 11 Present shows: buffer 0, which the runtime rotates.
    fn d3d11_back_buffer(&self) -> Option<u64> {
        use windows::Win32::Graphics::Direct3D11::ID3D11Resource;
        let mut out = core::ptr::null_mut();
        let hr = unsafe { real_swapchain::GetBuffer(self.inner(), 0, &ID3D11Resource::IID, &mut out) };
        if hr.is_err() || out.is_null() {
            return None;
        }
        let resource = unsafe { ID3D11Resource::from_raw(out) };
        Some(crate::d3d11::registry::id_of(&resource))
    }

    /// The real immediate context of the device a Direct3D 11 swap chain presents for.
    fn d3d11_immediate(&self) -> Option<ID3D11DeviceContext4> {
        let mut device = core::ptr::null_mut();
        let hr = unsafe { real_swapchain::GetDevice(self.inner(), &ID3D11Device::IID, &mut device) };
        if hr.is_err() || device.is_null() {
            return None;
        }
        let device = unsafe { ID3D11Device::from_raw(device) };
        unsafe { device.GetImmediateContext() }.ok()?.cast().ok()
    }

    /// The locum number of the back buffer a Direct3D 12 Present shows: the one at the swap chain's current index,
    /// through the proxy the game holds for it.
    fn d3d12_back_buffer(&self) -> Option<u64> {
        use windows::Win32::Graphics::Direct3D12::ID3D12Resource;
        let inner = self.inner();
        let chain = unsafe { IUnknown::from_raw_borrowed(&inner) }?
            .cast::<IDXGISwapChain3>()
            .ok()?;
        let index = unsafe { chain.GetCurrentBackBufferIndex() };
        let mut out = core::ptr::null_mut();
        let hr = unsafe { real_swapchain::GetBuffer(inner, index, &ID3D12Resource::IID, &mut out) };
        // the real buffer's proxy, with a reference added for this call
        unsafe { crate::d3d12::wrap_output(hr, &ID3D12Resource::IID, &mut out) };
        if hr.is_err() || out.is_null() {
            return None;
        }
        let id = crate::d3d12::id(out);
        drop(unsafe { IUnknown::from_raw(out) });
        (id != 0).then_some(id)
    }

    /// A present: the frame boundary for the frame counter and for captures. `TEST` presents show nothing and are
    /// passed straight through.
    fn present(&self, flags: DXGI_PRESENT, real: impl FnOnce() -> HRESULT) -> HRESULT {
        if flags.contains(DXGI_PRESENT_TEST) {
            return real();
        }
        faces::bury();
        let frame = count_frame();
        if self.api != Api::D3d11 {
            if frame == 1 {
                log!("D3D12 first proxied Present");
            }
            let presented = if crate::d3d12::passes::ARMED.load(Ordering::Acquire) {
                self.d3d12_back_buffer()
            } else {
                None
            };
            crate::d3d12::journal::present(frame, presented);
            return self.told(real());
        }
        crate::d3d11::overrides::poll(frame);
        let Some(immediate) = self.d3d11_immediate() else {
            return self.told(real());
        };
        capture::end_frame(&immediate);
        let finished = capture::before_present(|| self.d3d11_back_buffer());
        let hr = self.told(real());
        capture::after_present(finished, &immediate);
        hr
    }

    /// A present's result, logged with the device's reason the first time it says the device was removed.
    fn told(&self, hr: HRESULT) -> HRESULT {
        static TOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if (hr != DXGI_ERROR_DEVICE_REMOVED && hr != DXGI_ERROR_DEVICE_RESET)
            || TOLD.swap(true, Ordering::Relaxed)
        {
            return hr;
        }
        match self.removal() {
            Some(reason) => log!("present: {hr}: the device was removed, its reason {reason}"),
            None => log!("present: {hr}, and the device gives no reason"),
        }
        hr
    }

    /// Why the device this swap chain presents for was removed (`S_OK`: it was not); `None` if it cannot be asked.
    pub(crate) fn removal(&self) -> Option<HRESULT> {
        let mut device = core::ptr::null_mut();
        Some(
            match self.api {
                Api::D3d11 => {
                    unsafe { real_swapchain::GetDevice(self.inner(), &ID3D11Device::IID, &mut device) }
                        .ok()
                        .ok()?;
                    let device = unsafe { ID3D11Device::from_raw(device) };
                    unsafe { device.GetDeviceRemovedReason() }
                }
                Api::D3d12 { queue } => {
                    use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device};
                    let queue = crate::d3d12::real::<ID3D12CommandQueue>(queue)?;
                    let mut device: Option<ID3D12Device> = None;
                    unsafe { queue.GetDevice(&mut device) }.ok()?;
                    unsafe { device?.GetDeviceRemovedReason() }
                }
            }
            .err()
            .map_or(windows::Win32::Foundation::S_OK, |e| e.code()),
        )
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
