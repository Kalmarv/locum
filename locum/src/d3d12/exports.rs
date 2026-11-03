//! Public D3D12 creation entrypoints; other exports are ABI-transparent tail calls.
use crate::log::log;
use crate::system::{export, system_d3d12};
use core::ffi::c_void;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL;
use windows::Win32::System::LibraryLoader::GetProcAddress;
use windows::core::{GUID, HRESULT, PCSTR};
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3D12CreateDevice(
    adapter: *mut c_void,
    level: D3D_FEATURE_LEVEL,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    crate::hello();
    type F =
        unsafe extern "system" fn(*mut c_void, D3D_FEATURE_LEVEL, *const GUID, *mut *mut c_void) -> HRESULT;
    let Some(f) = (unsafe { export::<F>(system_d3d12(), c"D3D12CreateDevice") }) else {
        return windows::Win32::Foundation::E_NOTIMPL;
    };
    let hr = unsafe { f(crate::dxgi::Adapter::unwrap_raw(adapter), level, iid, out) };
    log!("D3D12CreateDevice level={level:?} -> {hr:?}");
    unsafe { super::wrap_output(hr, iid, out) };
    hr
}
#[unsafe(no_mangle)]
pub unsafe extern "system" fn D3D12GetDebugInterface(iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    type F = unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> HRESULT;
    let Some(f) = (unsafe { export::<F>(system_d3d12(), c"D3D12GetDebugInterface") }) else {
        return windows::Win32::Foundation::E_NOTIMPL;
    };
    let hr = unsafe { f(iid, out) };
    unsafe { super::wrap_output(hr, iid, out) };
    hr
}
// Private exports have no public ABI to guess. Preserve all argument registers,
// the original stack and return address, then tail-call the real ordinal.
extern "system" fn resolve_ordinal(ordinal: usize) -> usize {
    unsafe { GetProcAddress(system_d3d12(), PCSTR(ordinal as *const u8)) }
        .map(|f| f as usize)
        .unwrap_or_else(|| {
            log!("missing D3D12 ordinal {ordinal}");
            std::process::abort()
        })
}
macro_rules! forward {
    ($name:ident,$ordinal:literal)=>{
        #[unsafe(naked)]
        #[unsafe(no_mangle)]
        pub unsafe extern "system" fn $name() {
            core::arch::naked_asm!(
                "push rcx", "push rdx", "push r8", "push r9", "sub rsp, 104",
                "movdqu [rsp + 32], xmm0", "movdqu [rsp + 48], xmm1",
                "movdqu [rsp + 64], xmm2", "movdqu [rsp + 80], xmm3",
                "mov ecx, {ordinal}", "call {resolve}",
                "movdqu xmm0, [rsp + 32]", "movdqu xmm1, [rsp + 48]",
                "movdqu xmm2, [rsp + 64]", "movdqu xmm3, [rsp + 80]",
                "add rsp, 104", "pop r9", "pop r8", "pop rdx", "pop rcx", "jmp rax",
                ordinal=const $ordinal, resolve=sym resolve_ordinal,
            );
        }
    }
}
forward!(D3D12Ordinal99, 99);
forward!(SetAppCompatStringPointer, 100);
forward!(D3D12CoreCreateLayeredDevice, 103);
forward!(D3D12CoreGetLayeredDeviceSize, 104);
forward!(D3D12CoreRegisterLayers, 105);
forward!(D3D12CreateRootSignatureDeserializer, 106);
forward!(D3D12CreateVersionedRootSignatureDeserializer, 107);
forward!(D3D12DeviceRemovedExtendedData, 108);
forward!(D3D12EnableExperimentalFeatures, 109);
forward!(D3D12GetInterface, 110);
forward!(D3D12PIXEventsReplaceBlock, 111);
forward!(D3D12PIXGetThreadInfo, 112);
forward!(D3D12PIXNotifyWakeFromFenceSignal, 113);
forward!(D3D12PIXReportCounter, 114);
forward!(D3D12SerializeRootSignature, 115);
forward!(D3D12SerializeVersionedRootSignature, 116);
forward!(GetBehaviorValue, 117);
