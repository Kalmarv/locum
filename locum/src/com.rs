//! Shared COM plumbing for the wrappers.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

use crate::log::log;

/// `QueryInterface` on a raw COM pointer.
///
/// # Safety
/// `object` must be a live COM object.
pub unsafe fn query(object: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    unsafe { ((**(object as *const *const windows::core::IUnknown_Vtbl)).QueryInterface)(object, iid, out) }
}

/// Answers `QueryInterface` for a wrapper: `iids` are the interfaces the wrapper itself implements (it hands out
/// itself); anything else goes to the wrapped object, and the game then holds a real object locum does not see.
/// Those side doors are logged, once per wrapper kind and interface.
///
/// # Safety
/// `this` must be the wrapper, `inner` the live wrapped object, `add_ref` the wrapper's own `AddRef`.
pub unsafe fn answer_query(
    kind: &'static str,
    this: *const c_void,
    inner: *mut c_void,
    iids: &[GUID],
    iid: *const GUID,
    out: *mut *mut c_void,
    add_ref: impl FnOnce(),
) -> HRESULT {
    if out.is_null() || iid.is_null() {
        return windows::Win32::Foundation::E_POINTER;
    }
    let wanted = unsafe { *iid };
    if iids.contains(&wanted) {
        add_ref();
        unsafe { *out = this as *mut c_void };
        return windows::Win32::Foundation::S_OK;
    }
    let hr = unsafe { query(inner, iid, out) };
    side_door(kind, &wanted, hr);
    hr
}

fn side_door(kind: &'static str, iid: &GUID, hr: HRESULT) {
    static SEEN: Mutex<Option<HashMap<(&'static str, u128), u64>>> = Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let count = seen
        .get_or_insert_with(HashMap::new)
        .entry((kind, iid.to_u128()))
        .or_insert(0);
    *count += 1;
    if *count == 1 {
        log!(
            "side door: {kind} QueryInterface {} ({}) -> {hr:?}, answered by the real object",
            guid(iid),
            name_of(iid)
        );
    }
}

pub fn guid(g: &GUID) -> String {
    format!("{g:?}")
}

/// A readable name for the interfaces a game is likely to ask about.
pub fn name_of(iid: &GUID) -> &'static str {
    let known: &[(GUID, &str)] = &[
        (IUnknown::IID, "IUnknown"),
        (ID3D11Device::IID, "ID3D11Device"),
        (ID3D11Device1::IID, "ID3D11Device1"),
        (ID3D11Device2::IID, "ID3D11Device2"),
        (ID3D11Device3::IID, "ID3D11Device3"),
        (ID3D11Device4::IID, "ID3D11Device4"),
        (ID3D11Device5::IID, "ID3D11Device5"),
        (ID3D11DeviceContext::IID, "ID3D11DeviceContext"),
        (ID3D11DeviceContext1::IID, "ID3D11DeviceContext1"),
        (ID3D11DeviceContext2::IID, "ID3D11DeviceContext2"),
        (ID3D11DeviceContext3::IID, "ID3D11DeviceContext3"),
        (ID3D11DeviceContext4::IID, "ID3D11DeviceContext4"),
        (ID3D11Multithread::IID, "ID3D11Multithread"),
        (ID3DUserDefinedAnnotation::IID, "ID3DUserDefinedAnnotation"),
        (ID3D11VideoDevice::IID, "ID3D11VideoDevice"),
        (ID3D11VideoContext::IID, "ID3D11VideoContext"),
        (ID3D11Debug::IID, "ID3D11Debug"),
        (ID3D11InfoQueue::IID, "ID3D11InfoQueue"),
        (IDXGIObject::IID, "IDXGIObject"),
        (IDXGIDevice::IID, "IDXGIDevice"),
        (IDXGIDevice1::IID, "IDXGIDevice1"),
        (IDXGIDevice2::IID, "IDXGIDevice2"),
        (IDXGIDevice3::IID, "IDXGIDevice3"),
        (IDXGIDevice4::IID, "IDXGIDevice4"),
        (IDXGIAdapter::IID, "IDXGIAdapter"),
        (IDXGIAdapter1::IID, "IDXGIAdapter1"),
        (IDXGIFactory::IID, "IDXGIFactory"),
        (IDXGIFactory1::IID, "IDXGIFactory1"),
        (IDXGIFactory2::IID, "IDXGIFactory2"),
        (IDXGIFactory3::IID, "IDXGIFactory3"),
        (IDXGIFactory4::IID, "IDXGIFactory4"),
        (IDXGIFactory5::IID, "IDXGIFactory5"),
        (IDXGIFactory6::IID, "IDXGIFactory6"),
        (IDXGIFactory7::IID, "IDXGIFactory7"),
        (IDXGISwapChain::IID, "IDXGISwapChain"),
        (IDXGISwapChain1::IID, "IDXGISwapChain1"),
        (IDXGISwapChain2::IID, "IDXGISwapChain2"),
        (IDXGISwapChain3::IID, "IDXGISwapChain3"),
        (IDXGISwapChain4::IID, "IDXGISwapChain4"),
        (IDXGIDeviceSubObject::IID, "IDXGIDeviceSubObject"),
        (IDXGIResource::IID, "IDXGIResource"),
        (IDXGIOutput::IID, "IDXGIOutput"),
    ];
    known.iter().find(|(g, _)| g == iid).map_or("?", |(_, n)| n)
}
