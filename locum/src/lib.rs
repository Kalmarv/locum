//! locum: captures everything a Direct3D 11 game draws in a frame, to look at, query, change and replay. The game loads
//! locum's d3d11.dll and dxgi.dll from its own folder; they forward to this DLL, which hands every call on to the
//! real runtime and watches it go by.
//!
//! The exports below are what `d3d11.dll` and `dxgi.dll` beside the game forward to. Each calls the same function
//! in the real DLL and wraps what comes back: the device and its contexts, the DXGI factory and its swap chains.

// The exports are Direct3D's own entry points: their safety contract is Microsoft's documentation of each.
#![allow(clippy::missing_safety_doc)]

mod com;
mod cost;
mod crash;
mod d3d11;
mod d3d12;
mod dump;
mod dxgi;
mod log;
mod passthrough;
mod settings;
mod system;
mod values;
mod vtables;

use core::ffi::c_void;
use std::sync::Once;

use windows::Win32::Foundation::{E_NOTIMPL, E_POINTER, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{D3D11_CREATE_DEVICE_FLAG, ID3D11Device};
use windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_DESC;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

use crate::d3d11::device::Device;
use crate::dxgi::{Adapter, Factory, SwapChain};
use crate::log::log;
use crate::system::{caller_of_caller, export, is_system_caller, module_of, module_path, system};
use crate::vtables::device::DeviceMethods;

/// The first call into locum logs who loaded it.
fn hello() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let exe = std::env::current_exe()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        log!(
            "locum {} in {exe} (pid {})",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        );
        crate::crash::install();
        crate::values::start();
    });
}

fn caller_name(address: *const c_void) -> String {
    module_of(address)
        .map(|m| module_path(m).display().to_string())
        .unwrap_or_else(|| format!("{address:p}"))
}

type CreateDeviceFn = unsafe extern "system" fn(
    *mut c_void,
    D3D_DRIVER_TYPE,
    HMODULE,
    D3D11_CREATE_DEVICE_FLAG,
    *const D3D_FEATURE_LEVEL,
    u32,
    u32,
    *mut *mut c_void,
    *mut D3D_FEATURE_LEVEL,
    *mut *mut c_void,
) -> HRESULT;

type CreateDeviceAndSwapChainFn = unsafe extern "system" fn(
    *mut c_void,
    D3D_DRIVER_TYPE,
    HMODULE,
    D3D11_CREATE_DEVICE_FLAG,
    *const D3D_FEATURE_LEVEL,
    u32,
    u32,
    *const DXGI_SWAP_CHAIN_DESC,
    *mut *mut c_void,
    *mut *mut c_void,
    *mut D3D_FEATURE_LEVEL,
    *mut *mut c_void,
) -> HRESULT;

/// Hands the wrapped device and its immediate context to the caller's out pointers (either may be null). Takes over
/// `device`'s reference.
unsafe fn give_device(device: *mut c_void, device_out: *mut *mut c_void, context_out: *mut *mut c_void) {
    let real = unsafe { ID3D11Device::from_raw(device) };
    match Device::wrap(real) {
        Ok(wrapper) => unsafe {
            let w = &*wrapper;
            if !context_out.is_null() {
                w.AddRef();
                *context_out = w.immediate_raw();
            }
            if device_out.is_null() {
                w.Release();
            } else {
                *device_out = wrapper.cast();
            }
        },
        Err(real) => unsafe {
            if !context_out.is_null() {
                *context_out = real
                    .GetImmediateContext()
                    .map_or(core::ptr::null_mut(), |c| c.into_raw());
            }
            if !device_out.is_null() {
                *device_out = real.into_raw();
            }
        },
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3D11CreateDevice(
    adapter: *mut c_void,
    driver_type: D3D_DRIVER_TYPE,
    software: HMODULE,
    flags: D3D11_CREATE_DEVICE_FLAG,
    levels: *const D3D_FEATURE_LEVEL,
    level_count: u32,
    sdk_version: u32,
    device_out: *mut *mut c_void,
    level_out: *mut D3D_FEATURE_LEVEL,
    context_out: *mut *mut c_void,
) -> HRESULT {
    hello();
    let caller = caller_of_caller();
    let Some(real) = (unsafe { export::<CreateDeviceFn>(system().d3d11, c"D3D11CreateDevice") }) else {
        return E_NOTIMPL;
    };
    let adapter = Adapter::unwrap_raw(adapter);
    if is_system_caller(caller) {
        // The system's and the driver's own devices (overlays, NVIDIA's camera) stay real, as their factories do.
        let hr = unsafe {
            real(
                adapter,
                driver_type,
                software,
                flags,
                levels,
                level_count,
                sdk_version,
                device_out,
                level_out,
                context_out,
            )
        };
        log!(
            "D3D11CreateDevice from {} flags {:#x} -> {hr:?} (real device)",
            caller_name(caller),
            flags.0
        );
        return hr;
    }
    let mut device = core::ptr::null_mut();
    let wants = !device_out.is_null() || !context_out.is_null();
    let hr = unsafe {
        real(
            adapter,
            driver_type,
            software,
            flags,
            levels,
            level_count,
            sdk_version,
            if wants { &mut device } else { core::ptr::null_mut() },
            level_out,
            core::ptr::null_mut(),
        )
    };
    log!(
        "D3D11CreateDevice from {} flags {:#x} -> {hr:?}",
        caller_name(caller),
        flags.0
    );
    if hr.is_ok() && !device.is_null() {
        unsafe { give_device(device, device_out, context_out) };
    }
    hr
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3D11CreateDeviceAndSwapChain(
    adapter: *mut c_void,
    driver_type: D3D_DRIVER_TYPE,
    software: HMODULE,
    flags: D3D11_CREATE_DEVICE_FLAG,
    levels: *const D3D_FEATURE_LEVEL,
    level_count: u32,
    sdk_version: u32,
    swapchain_desc: *const DXGI_SWAP_CHAIN_DESC,
    swapchain_out: *mut *mut c_void,
    device_out: *mut *mut c_void,
    level_out: *mut D3D_FEATURE_LEVEL,
    context_out: *mut *mut c_void,
) -> HRESULT {
    hello();
    let caller = caller_of_caller();
    let Some(real) =
        (unsafe { export::<CreateDeviceAndSwapChainFn>(system().d3d11, c"D3D11CreateDeviceAndSwapChain") })
    else {
        return E_NOTIMPL;
    };
    let adapter = Adapter::unwrap_raw(adapter);
    let mut device = core::ptr::null_mut();
    let mut swapchain = core::ptr::null_mut();
    let hr = unsafe {
        real(
            adapter,
            driver_type,
            software,
            flags,
            levels,
            level_count,
            sdk_version,
            swapchain_desc,
            if swapchain_out.is_null() {
                core::ptr::null_mut()
            } else {
                &mut swapchain
            },
            &mut device,
            level_out,
            core::ptr::null_mut(),
        )
    };
    log!(
        "D3D11CreateDeviceAndSwapChain from {} flags {:#x} -> {hr:?}",
        caller_name(caller),
        flags.0
    );
    if hr.is_err() || device.is_null() {
        return hr;
    }
    // The device first, with a reference of our own until the swap chain holds one.
    let mut ours = core::ptr::null_mut();
    unsafe { give_device(device, &mut ours, context_out) };
    if !swapchain.is_null() {
        let real = unsafe { IUnknown::from_raw(swapchain) };
        let wrapped = match SwapChain::wrap(real, Device::from_raw(ours), None) {
            Ok(w) => w.cast(),
            Err(real) => real.into_raw(),
        };
        unsafe { *swapchain_out = wrapped };
    }
    unsafe {
        if device_out.is_null() {
            match Device::from_raw(ours) {
                Some(d) => {
                    d.Release();
                }
                None => {
                    IUnknown::from_raw(ours);
                }
            }
        } else {
            *device_out = ours;
        }
    }
    hr
}

type CreateFactoryFn = unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> HRESULT;
type CreateFactory2Fn = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> HRESULT;

/// A factory the real DXGI made, wrapped unless Windows or the driver asked for it (the real d3d11.dll gets its
/// factory from `CreateDXGIFactory2`, and with our dxgi.dll loaded under that name the call can land here).
unsafe fn give_factory(name: &str, caller: *const c_void, hr: HRESULT, out: *mut *mut c_void) -> HRESULT {
    let system_caller = is_system_caller(caller);
    log!(
        "{name} from {} -> {hr:?}{}",
        caller_name(caller),
        if system_caller { " (real factory)" } else { "" }
    );
    if hr.is_err() || out.is_null() || unsafe { (*out).is_null() } || system_caller {
        return hr;
    }
    let real = unsafe { IUnknown::from_raw(*out) };
    match Factory::wrap(real) {
        Ok(w) => unsafe { *out = w.cast() },
        Err(real) => unsafe { *out = real.into_raw() },
    }
    hr
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn CreateDXGIFactory(iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    hello();
    let caller = caller_of_caller();
    if out.is_null() {
        return E_POINTER;
    }
    let Some(real) = (unsafe { export::<CreateFactoryFn>(system().dxgi, c"CreateDXGIFactory") }) else {
        return E_NOTIMPL;
    };
    let hr = unsafe { real(iid, out) };
    unsafe { give_factory("CreateDXGIFactory", caller, hr, out) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn CreateDXGIFactory1(iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    hello();
    let caller = caller_of_caller();
    if out.is_null() {
        return E_POINTER;
    }
    let Some(real) = (unsafe { export::<CreateFactoryFn>(system().dxgi, c"CreateDXGIFactory1") }) else {
        return E_NOTIMPL;
    };
    let hr = unsafe { real(iid, out) };
    unsafe { give_factory("CreateDXGIFactory1", caller, hr, out) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn CreateDXGIFactory2(
    flags: u32,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    hello();
    let caller = caller_of_caller();
    if out.is_null() {
        return E_POINTER;
    }
    let Some(real) = (unsafe { export::<CreateFactory2Fn>(system().dxgi, c"CreateDXGIFactory2") }) else {
        return E_NOTIMPL;
    };
    let hr = unsafe { real(flags, iid, out) };
    unsafe { give_factory("CreateDXGIFactory2", caller, hr, out) }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DXGIDeclareAdapterRemovalSupport() -> HRESULT {
    hello();
    type F = unsafe extern "system" fn() -> HRESULT;
    match unsafe { export::<F>(system().dxgi, c"DXGIDeclareAdapterRemovalSupport") } {
        Some(real) => unsafe { real() },
        None => E_NOTIMPL,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn DXGIGetDebugInterface1(
    flags: u32,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    hello();
    match unsafe { export::<CreateFactory2Fn>(system().dxgi, c"DXGIGetDebugInterface1") } {
        Some(real) => unsafe { real(flags, iid, out) },
        None => E_NOTIMPL,
    }
}
