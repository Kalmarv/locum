//! The exports of the system's `d3d11.dll` and `dxgi.dll` that locum does not intercept, passed through untouched.
//! Our `d3d11.dll` and `dxgi.dll` are loaded under those names, so the process finds ours for whatever it asks of them
//! later. The Direct3D 11 debug layer (`d3d11_3SDKLayers.dll`) imports `D3D11CoreRegisterLayers`,
//! `D3D11CoreGetLayeredDeviceSize` and `D3D11CoreCreateLayeredDevice` from `d3d11.dll`; without them it does not load,
//! and no debug device can be made (DXGI_ERROR_SDK_COMPONENT_MISSING). PIX asks `dxgi.dll` for `PIXBeginCapture`.
//! So the shims export every name the system DLL exports (`shims/*/build.rs`), and those not intercepted come here.
//!
//! Each is a naked stub. It keeps the argument registers, asks for the real function (resolved once) and jumps to it
//! with the caller's stack and registers as they were, so its signature does not matter.

use core::ffi::CStr;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows::Win32::Foundation::{E_NOTIMPL, HMODULE};
use windows::core::HRESULT;

use crate::log::log;
use crate::system::{export, system};

/// One export passed through: the system DLL it is in, its name there, and where it was found.
pub struct Slot {
    dll: fn() -> HMODULE,
    name: &'static CStr,
    /// Logged the first time it is called.
    note: Option<&'static str>,
    address: AtomicUsize,
}

fn d3d11() -> HMODULE {
    system().d3d11
}

fn dxgi() -> HMODULE {
    system().dxgi
}

/// Returned when the system DLL lacks the export: ignores its arguments and fails.
extern "system" fn not_implemented() -> HRESULT {
    E_NOTIMPL
}

extern "system" fn resolve(slot: &'static Slot) -> usize {
    match slot.address.load(Ordering::Acquire) {
        0 => {
            if let Some(note) = slot.note {
                crate::hello();
                log!("{}: {note}", slot.name.to_string_lossy());
            }
            let address = unsafe { export::<usize>((slot.dll)(), slot.name) }.unwrap_or_else(|| {
                log!(
                    "{}: not in the system DLL, so it fails (E_NOTIMPL)",
                    slot.name.to_string_lossy()
                );
                not_implemented as *const () as usize
            });
            slot.address.store(address, Ordering::Release);
            address
        }
        address => address,
    }
}

macro_rules! note {
    () => {
        None
    };
    ($note:literal) => {
        Some($note)
    };
}

/// `dll Symbol c"Name" [note "..."];`: exports `Symbol`, which runs the system DLL's `Name`.
macro_rules! pass {
    ($($dll:ident $symbol:ident $name:literal $(note $note:literal)?;)*) => {$(
        #[allow(non_snake_case)]
        mod $symbol {
            pub static SLOT: super::Slot = super::Slot {
                dll: super::$dll,
                name: $name,
                note: note!($($note)?),
                address: super::AtomicUsize::new(0),
            };
        }
        #[unsafe(naked)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn $symbol() {
            core::arch::naked_asm!(
                "push rcx", "push rdx", "push r8", "push r9", "sub rsp, 104",
                "movdqu [rsp + 32], xmm0", "movdqu [rsp + 48], xmm1",
                "movdqu [rsp + 64], xmm2", "movdqu [rsp + 80], xmm3",
                "lea rcx, [rip + {slot}]", "call {resolve}",
                "movdqu xmm0, [rsp + 32]", "movdqu xmm1, [rsp + 48]",
                "movdqu xmm2, [rsp + 64]", "movdqu xmm3, [rsp + 80]",
                "add rsp, 104", "pop r9", "pop r8", "pop rdx", "pop rcx", "jmp rax",
                slot = sym $symbol::SLOT, resolve = sym resolve,
            );
        }
    )*};
}

// d3d11.dll: all but D3D11CreateDevice and D3D11CreateDeviceAndSwapChain, whose devices locum wraps.
pass! {
    d3d11 D3D11CoreCreateDevice c"D3D11CoreCreateDevice" note "passed through, device not wrapped";
    d3d11 D3D11On12CreateDevice c"D3D11On12CreateDevice" note "passed through, device not wrapped";
    d3d11 D3D11CreateDeviceForD3D12 c"D3D11CreateDeviceForD3D12" note "passed through, device not wrapped";
    d3d11 CreateDirect3D11DeviceFromDXGIDevice c"CreateDirect3D11DeviceFromDXGIDevice"
        note "passed through, device not wrapped";
    d3d11 CreateDirect3D11SurfaceFromDXGISurface c"CreateDirect3D11SurfaceFromDXGISurface";
    d3d11 D3D11CoreCreateLayeredDevice c"D3D11CoreCreateLayeredDevice";
    d3d11 D3D11CoreGetLayeredDeviceSize c"D3D11CoreGetLayeredDeviceSize";
    d3d11 D3D11CoreRegisterLayers c"D3D11CoreRegisterLayers";
    d3d11 EnableFeatureLevelUpgrade c"EnableFeatureLevelUpgrade";
    d3d11 OpenAdapter10 c"OpenAdapter10";
    d3d11 OpenAdapter10_2 c"OpenAdapter10_2";
    d3d11 D3DPerformance_BeginEvent c"D3DPerformance_BeginEvent";
    d3d11 D3DPerformance_EndEvent c"D3DPerformance_EndEvent";
    d3d11 D3DPerformance_GetStatus c"D3DPerformance_GetStatus";
    d3d11 D3DPerformance_SetMarker c"D3DPerformance_SetMarker";
    d3d11 D3DKMTCloseAdapter c"D3DKMTCloseAdapter";
    d3d11 D3DKMTCreateAllocation c"D3DKMTCreateAllocation";
    d3d11 D3DKMTCreateContext c"D3DKMTCreateContext";
    d3d11 D3DKMTCreateDevice c"D3DKMTCreateDevice";
    d3d11 D3DKMTCreateSynchronizationObject c"D3DKMTCreateSynchronizationObject";
    d3d11 D3DKMTDestroyAllocation c"D3DKMTDestroyAllocation";
    d3d11 D3DKMTDestroyContext c"D3DKMTDestroyContext";
    d3d11 D3DKMTDestroyDevice c"D3DKMTDestroyDevice";
    d3d11 D3DKMTDestroySynchronizationObject c"D3DKMTDestroySynchronizationObject";
    d3d11 D3DKMTEscape c"D3DKMTEscape";
    d3d11 D3DKMTGetContextSchedulingPriority c"D3DKMTGetContextSchedulingPriority";
    d3d11 D3DKMTGetDeviceState c"D3DKMTGetDeviceState";
    d3d11 D3DKMTGetDisplayModeList c"D3DKMTGetDisplayModeList";
    d3d11 D3DKMTGetMultisampleMethodList c"D3DKMTGetMultisampleMethodList";
    d3d11 D3DKMTGetRuntimeData c"D3DKMTGetRuntimeData";
    d3d11 D3DKMTGetSharedPrimaryHandle c"D3DKMTGetSharedPrimaryHandle";
    d3d11 D3DKMTLock c"D3DKMTLock";
    d3d11 D3DKMTOpenAdapterFromHdc c"D3DKMTOpenAdapterFromHdc";
    d3d11 D3DKMTOpenResource c"D3DKMTOpenResource";
    d3d11 D3DKMTPresent c"D3DKMTPresent";
    d3d11 D3DKMTQueryAdapterInfo c"D3DKMTQueryAdapterInfo";
    d3d11 D3DKMTQueryAllocationResidency c"D3DKMTQueryAllocationResidency";
    d3d11 D3DKMTQueryResourceInfo c"D3DKMTQueryResourceInfo";
    d3d11 D3DKMTRender c"D3DKMTRender";
    d3d11 D3DKMTSetAllocationPriority c"D3DKMTSetAllocationPriority";
    d3d11 D3DKMTSetContextSchedulingPriority c"D3DKMTSetContextSchedulingPriority";
    d3d11 D3DKMTSetDisplayMode c"D3DKMTSetDisplayMode";
    d3d11 D3DKMTSetDisplayPrivateDriverFormat c"D3DKMTSetDisplayPrivateDriverFormat";
    d3d11 D3DKMTSetGammaRamp c"D3DKMTSetGammaRamp";
    d3d11 D3DKMTSetVidPnSourceOwner c"D3DKMTSetVidPnSourceOwner";
    d3d11 D3DKMTSignalSynchronizationObject c"D3DKMTSignalSynchronizationObject";
    d3d11 D3DKMTUnlock c"D3DKMTUnlock";
    d3d11 D3DKMTWaitForSynchronizationObject c"D3DKMTWaitForSynchronizationObject";
    d3d11 D3DKMTWaitForVerticalBlankEvent c"D3DKMTWaitForVerticalBlankEvent";
}

// dxgi.dll: all but the factory creators, DXGIDeclareAdapterRemovalSupport and DXGIGetDebugInterface1 (lib.rs).
// d3d12.dll exports a SetAppCompatStringPointer of its own, so dxgi's is DxgiSetAppCompatStringPointer here.
pass! {
    dxgi ApplyCompatResolutionQuirking c"ApplyCompatResolutionQuirking";
    dxgi CompatString c"CompatString";
    dxgi CompatValue c"CompatValue";
    dxgi DXGID3D10CreateDevice c"DXGID3D10CreateDevice";
    dxgi DXGID3D10CreateLayeredDevice c"DXGID3D10CreateLayeredDevice";
    dxgi DXGID3D10GetLayeredDeviceSize c"DXGID3D10GetLayeredDeviceSize";
    dxgi DXGID3D10RegisterLayers c"DXGID3D10RegisterLayers";
    dxgi DXGIDisableVBlankVirtualization c"DXGIDisableVBlankVirtualization";
    dxgi DXGIDumpJournal c"DXGIDumpJournal";
    dxgi DXGIReportAdapterConfiguration c"DXGIReportAdapterConfiguration";
    dxgi PIXBeginCapture c"PIXBeginCapture";
    dxgi PIXEndCapture c"PIXEndCapture";
    dxgi PIXGetCaptureState c"PIXGetCaptureState";
    dxgi DxgiSetAppCompatStringPointer c"SetAppCompatStringPointer";
    dxgi UpdateHMDEmulationStatus c"UpdateHMDEmulationStatus";
}
