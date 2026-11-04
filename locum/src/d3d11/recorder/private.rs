use super::*;
const LIST_KEY: windows::core::GUID = windows::core::GUID::from_u128(0x6c6f6375_6d00_4a5e_9d1b_7a2f8c3e0020);
#[repr(C)]
struct ListData {
    vtable: *const windows::core::IUnknown_Vtbl,
    refs: std::sync::atomic::AtomicU32,
    plan: std::sync::Arc<super::Plan>,
}
unsafe extern "system" fn query(
    this: *mut c_void,
    iid: *const windows::core::GUID,
    out: *mut *mut c_void,
) -> windows::core::HRESULT {
    if out.is_null() {
        return windows::Win32::Foundation::E_POINTER;
    }
    unsafe {
        *out = std::ptr::null_mut();
    }
    if !iid.is_null() && unsafe { *iid == IUnknown::IID } {
        unsafe {
            *out = this;
            add_ref(this);
        }
        windows::Win32::Foundation::S_OK
    } else {
        windows::Win32::Foundation::E_NOINTERFACE
    }
}
unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    unsafe { &*this.cast::<ListData>() }
        .refs
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        + 1
}
unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    let n = unsafe { &*this.cast::<ListData>() }
        .refs
        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
        - 1;
    if n == 0 {
        drop(unsafe { Box::from_raw(this.cast::<ListData>()) });
    }
    n
}
static LIST_VTABLE: windows::core::IUnknown_Vtbl = windows::core::IUnknown_Vtbl {
    QueryInterface: query,
    AddRef: add_ref,
    Release: release,
};
/// # Safety
/// `list` is the successful FinishCommandList result, or null when discarded.
pub unsafe fn attach(list: *mut c_void, plan: std::sync::Arc<super::Plan>) {
    let Some(list) = (unsafe { ID3D11CommandList::from_raw_borrowed(&list) }) else {
        return;
    };
    let data = Box::new(ListData {
        vtable: &LIST_VTABLE,
        refs: std::sync::atomic::AtomicU32::new(1),
        plan,
    });
    let unknown = unsafe { IUnknown::from_raw(Box::into_raw(data).cast()) };
    if let Err(e) = unsafe { list.SetPrivateDataInterface(&LIST_KEY, &unknown) } {
        crate::log::log!("command list recorder metadata: {e}");
    }
}

pub unsafe fn get(list: *mut c_void) -> Option<std::sync::Arc<super::Plan>> {
    let list = unsafe { ID3D11CommandList::from_raw_borrowed(&list) }?;
    let mut data: *mut c_void = std::ptr::null_mut();
    let mut bytes = size_of::<*mut c_void>() as u32;
    unsafe {
        list.GetPrivateData(
            &LIST_KEY,
            &mut bytes,
            Some((&mut data as *mut *mut c_void).cast()),
        )
    }
    .ok()?;
    if data.is_null() {
        return None;
    }
    let unknown = unsafe { IUnknown::from_raw(data) };
    let plan = unsafe { &*data.cast::<ListData>() }.plan.clone();
    drop(unknown);
    Some(plan)
}
