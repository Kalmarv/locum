//! A deliberately base-only fence checks that wrapping does not require Fence1,
//! that every facet has one IUnknown identity, and that the last Release destroys
//! the real object rather than leaving a registry-owned reference behind.
use super::*;
use crate::vtables::d12_fence::Fence12Methods;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32},
};
use windows::Win32::{
    Foundation::{E_NOINTERFACE, S_OK},
    Graphics::Direct3D12::*,
};
#[repr(C)]
struct BaseFence {
    table: *const ID3D12Fence1_Vtbl,
    refs: AtomicU32,
    destroyed: Arc<AtomicBool>,
}
static TABLE: ID3D12Fence1_Vtbl = crate::vtables::d12_fence::vtable::<BaseFence>();
impl Drop for BaseFence {
    fn drop(&mut self) {
        self.destroyed.store(true, Ordering::SeqCst);
    }
}
unsafe impl Fence12Methods for BaseFence {
    fn inner(&self) -> *mut c_void {
        panic!("mock operation outside this test's contract")
    }
    unsafe fn QueryInterface(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        if out.is_null() || iid.is_null() {
            return E_POINTER;
        }
        unsafe {
            *out = core::ptr::null_mut();
        }
        if unsafe { *iid } == IUnknown::IID || unsafe { *iid } == ID3D12Fence::IID {
            unsafe {
                self.AddRef();
                *out = (self as *const Self).cast_mut().cast();
            }
            S_OK
        } else {
            E_NOINTERFACE
        }
    }
    unsafe fn AddRef(&self) -> u32 {
        self.refs.fetch_add(1, Ordering::SeqCst) + 1
    }
    unsafe fn Release(&self) -> u32 {
        let n = self.refs.fetch_sub(1, Ordering::SeqCst) - 1;
        if n == 0 {
            drop(unsafe { Box::from_raw((self as *const Self).cast_mut()) });
        }
        n
    }
    unsafe fn GetCompletedValue(&self) -> u64 {
        7
    }
}
#[test]
fn base_interface_identity_and_final_release() {
    unsafe {
        let destroyed = Arc::new(AtomicBool::new(false));
        let mut out = Box::into_raw(Box::new(BaseFence {
            table: &TABLE,
            refs: AtomicU32::new(1),
            destroyed: destroyed.clone(),
        }))
        .cast::<c_void>();
        let key = out as usize;
        wrap_output(S_OK, &ID3D12Fence::IID, &mut out);
        assert!(is_wrapped(out));
        let fence = ID3D12Fence::from_raw(out);
        assert_eq!(fence.GetCompletedValue(), 7);
        assert!(fence.cast::<ID3D12Fence1>().is_err());
        let a = fence.cast::<IUnknown>().unwrap();
        let b = fence.cast::<IUnknown>().unwrap();
        assert_eq!(a.as_raw(), b.as_raw());
        drop(fence);
        drop(a);
        assert!(!destroyed.load(Ordering::SeqCst));
        drop(b);
        assert!(destroyed.load(Ordering::SeqCst));
        assert!(!lock().identities.contains_key(&key));
    }
}
