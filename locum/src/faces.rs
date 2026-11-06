//! A wrapper is another face of the real object under it. Its `AddRef` and `Release` are the object's, so the game
//! sees the counts it would see without locum (a child holds its device, a back buffer its swap chain), and locum
//! holds no reference of its own. A wrapper lives exactly as long as its object: it is made once per object, the first
//! time the object comes past locum; it is found again by any pointer of the object; and it is freed when the object
//! is destroyed. The object says when that is by releasing the private-data interface locum set on it.

use core::ffi::c_void;
use core::mem::ManuallyDrop;
use core::sync::atomic::{AtomicU32, Ordering};
use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Foundation::{E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::Graphics::Direct3D11::{ID3D11Device, ID3D11DeviceChild};
use windows::Win32::Graphics::Direct3D12::ID3D12Object;
use windows::Win32::Graphics::Dxgi::IDXGIObject;
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

/// What a wrapper is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Device,
    Factory,
    Adapter,
    SwapChain,
}

impl core::fmt::Display for Kind {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.write_str(match self {
            Kind::Device => "device",
            Kind::Factory => "factory",
            Kind::Adapter => "adapter",
            Kind::SwapChain => "swap chain",
        })
    }
}

#[derive(Default)]
struct Faces {
    /// Each pointer of a wrapped object locum knows (its IUnknown and the interfaces its wrapper keeps): the wrapper.
    by_pointer: HashMap<usize, (Kind, usize)>,
    /// Each wrapper's pointers, to forget them when its object is destroyed.
    pointers: HashMap<usize, Vec<usize>>,
}

static FACES: Mutex<Option<Faces>> = Mutex::new(None);
/// Held while an object is looked up and wrapped, so two threads given one object make one wrapper.
static ADOPTING: Mutex<()> = Mutex::new(());

fn faces<R>(f: impl FnOnce(&mut Faces) -> R) -> R {
    f(FACES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default))
}

/// `AddRef` on a raw COM pointer.
///
/// # Safety
/// `object` must be a live COM object.
pub unsafe fn add_ref(object: *mut c_void) -> u32 {
    unsafe { ((**(object as *const *const IUnknown_Vtbl)).AddRef)(object) }
}

/// `Release` on a raw COM pointer.
///
/// # Safety
/// `object` must be a live COM object the caller holds a reference to.
pub unsafe fn release(object: *mut c_void) -> u32 {
    unsafe { ((**(object as *const *const IUnknown_Vtbl)).Release)(object) }
}

/// `owned`'s pointer without its reference: for a wrapper to keep while the object's own count keeps it alive.
pub fn unowned<T: Interface>(owned: T) -> ManuallyDrop<T> {
    let kept = ManuallyDrop::new(owned);
    unsafe { release(kept.as_raw()) };
    kept
}

/// The object's IUnknown, the pointer COM says is its identity.
///
/// # Safety
/// `object` must be a live COM object.
pub unsafe fn identity(object: *mut c_void) -> Option<usize> {
    let mut unknown = core::ptr::null_mut();
    let hr = unsafe { crate::com::query(object, &IUnknown::IID, &mut unknown) };
    if hr.is_err() || unknown.is_null() {
        return None;
    }
    // The object outlives this reference: the caller holds one.
    unsafe { release(unknown) };
    Some(unknown as usize)
}

/// The wrapper of the object `real` points into, if locum made one: by the pointer itself, else by the object's
/// IUnknown.
///
/// # Safety
/// `real` must be null or a live COM object.
pub unsafe fn find(real: *mut c_void) -> Option<(Kind, usize)> {
    if real.is_null() {
        return None;
    }
    if let Some(found) = faces(|f| f.by_pointer.get(&(real as usize)).copied()) {
        return Some(found);
    }
    let id = unsafe { identity(real) }?;
    faces(|f| f.by_pointer.get(&id).copied())
}

/// Records `wrapper` as the face of the object `pointers` point into (its IUnknown is added).
///
/// # Safety
/// Each pointer must be a live COM object, all the same one.
pub unsafe fn register(kind: Kind, wrapper: usize, pointers: &[*mut c_void]) {
    let mut keys: Vec<usize> = pointers.iter().map(|&p| p as usize).collect();
    if let Some(&first) = pointers.first()
        && let Some(id) = unsafe { identity(first) }
    {
        keys.push(id);
    }
    keys.sort_unstable();
    keys.dedup();
    faces(|f| {
        for &k in &keys {
            f.by_pointer.insert(k, (kind, wrapper));
        }
        f.pointers.insert(wrapper, keys);
    });
}

/// Forgets `wrapper`'s pointers: its object is being destroyed.
pub fn forget(wrapper: usize) {
    faces(|f| {
        for k in f.pointers.remove(&wrapper).unwrap_or_default() {
            if f.by_pointer.get(&k).is_some_and(|&(_, w)| w == wrapper) {
                f.by_pointer.remove(&k);
            }
        }
    });
}

/// The wrapper of `real`'s object: the one locum has, or the one `make` makes (and registers and watches), with no
/// other thread making one for the same object meanwhile.
///
/// # Safety
/// `real` must be a live COM object.
pub unsafe fn adopt(real: *mut c_void, make: impl FnOnce() -> Option<usize>) -> Option<usize> {
    bury();
    let _adopting = ADOPTING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, wrapper)) = unsafe { find(real) } {
        return Some(wrapper);
    }
    make()
}

/// What a destroyed object's wrapper leaves to free. Freeing it releases other objects (a deferred context's recorder
/// holds the resources it names), which is not done inside the destruction that freed it: [`bury`] drops it later.
struct Grave(#[allow(dead_code)] Box<dyn core::any::Any>);
// SAFETY: what goes in a grave is only dropped, and `later`'s callers promise that may happen on any thread.
unsafe impl Send for Grave {}

static GRAVES: Mutex<Vec<Grave>> = Mutex::new(Vec::new());

/// Drops `thing` at the next [`bury`].
///
/// # Safety
/// `thing` must be safe to drop on any thread.
pub unsafe fn later<T: 'static>(thing: T) {
    GRAVES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(Grave(Box::new(thing)));
}

/// Runs `f` at the next [`bury`]: what must not run inside a destruction, or where its caller may hold locks `f`
/// takes.
pub fn defer(f: impl FnOnce() + Send + 'static) {
    struct Deferred(Option<Box<dyn FnOnce() + Send>>);
    impl Drop for Deferred {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    // SAFETY: a `Send` function may run on any thread.
    unsafe { later(Deferred(Some(Box::new(f)))) };
}

/// Drops what destroyed objects' wrappers left: at each present, each new wrapper, and each release of the game's
/// that destroyed an object. Not inside a Direct3D 12 release, whose lock what it drops may take.
pub fn bury() {
    if crate::d3d12::releasing() {
        return;
    }
    // Dropping what was left may leave more (a journal entry dropped releases what it held).
    loop {
        let graves = core::mem::take(&mut *GRAVES.lock().unwrap_or_else(|e| e.into_inner()));
        if graves.is_empty() {
            break;
        }
        drop(graves);
    }
}

/// The private-data key of locum's sentinel on each object it wraps.
const SENTINEL_KEY: GUID = GUID::from_u128(0x6c6f_6375_6d00_4a5e_9d1b_7a2f_8c3e_0003);

/// An IUnknown whose last release runs a function once.
#[repr(C)]
struct Sentinel {
    vtbl: *const IUnknown_Vtbl,
    refs: AtomicU32,
    gone: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

static SENTINEL_VTBL: IUnknown_Vtbl = IUnknown_Vtbl {
    QueryInterface: sentinel_query,
    AddRef: sentinel_add_ref,
    Release: sentinel_release,
};

unsafe extern "system" fn sentinel_query(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if iid.is_null() || out.is_null() {
        return E_POINTER;
    }
    if unsafe { *iid } == IUnknown::IID {
        unsafe {
            sentinel_add_ref(this);
            *out = this;
        }
        return S_OK;
    }
    unsafe { *out = core::ptr::null_mut() };
    E_NOINTERFACE
}

unsafe extern "system" fn sentinel_add_ref(this: *mut c_void) -> u32 {
    unsafe { (*(this as *const Sentinel)).refs.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn sentinel_release(this: *mut c_void) -> u32 {
    let left = unsafe { (*(this as *const Sentinel)).refs.fetch_sub(1, Ordering::AcqRel) - 1 };
    if left == 0 {
        let sentinel = unsafe { Box::from_raw(this as *mut Sentinel) };
        let gone = sentinel.gone.lock().unwrap_or_else(|e| e.into_inner()).take();
        drop(sentinel);
        if let Some(gone) = gone {
            gone();
        }
    }
    left
}

/// Runs `gone` when the object `real` points into is destroyed: it releases its private data then, locum's sentinel
/// with it. `gone` runs inside the object's destruction, so it must not call the object. False (and `gone` dropped
/// unrun) if the object takes no private-data interface.
///
/// # Safety
/// `real` must be a live COM object.
pub unsafe fn watch(real: *mut c_void, gone: impl FnOnce() + Send + 'static) -> bool {
    let sentinel = Box::into_raw(Box::new(Sentinel {
        vtbl: &SENTINEL_VTBL,
        refs: AtomicU32::new(1),
        gone: Mutex::new(Some(Box::new(gone))),
    }));
    let unknown = unsafe { IUnknown::from_raw(sentinel.cast()) };
    let object = unsafe { IUnknown::from_raw_borrowed(&real) }.expect("a live object");
    let set = if let Ok(o) = object.cast::<ID3D11Device>() {
        unsafe { o.SetPrivateDataInterface(&SENTINEL_KEY, &unknown) }
    } else if let Ok(o) = object.cast::<ID3D11DeviceChild>() {
        unsafe { o.SetPrivateDataInterface(&SENTINEL_KEY, &unknown) }
    } else if let Ok(o) = object.cast::<IDXGIObject>() {
        unsafe { o.SetPrivateDataInterface(&SENTINEL_KEY, &unknown) }
    } else if let Ok(o) = object.cast::<ID3D12Object>() {
        unsafe { o.SetPrivateDataInterface(&SENTINEL_KEY, &unknown) }
    } else {
        Err(E_NOINTERFACE.into())
    };
    if let Err(e) = &set {
        crate::log::log!(
            "object {real:p} takes no private-data interface ({e}): locum cannot see it destroyed"
        );
        unsafe { (*sentinel).gone.lock().unwrap_or_else(|e| e.into_inner()).take() };
    }
    drop(unknown);
    set.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL};
    use windows::Win32::Graphics::Direct3D11::*;

    /// A WARP device from the system's `D3D11CreateDevice` (the `windows` crate's import of it would clash with
    /// locum's export of the name in the test executable).
    fn warp() -> ID3D11Device {
        type Create = unsafe extern "system" fn(
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
        let create =
            unsafe { crate::system::export::<Create>(crate::system::system().d3d11, c"D3D11CreateDevice") }
                .unwrap();
        let mut device = core::ptr::null_mut();
        unsafe {
            create(
                core::ptr::null_mut(),
                D3D_DRIVER_TYPE_WARP,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                core::ptr::null(),
                0,
                D3D11_SDK_VERSION,
                &mut device,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        }
        .ok()
        .expect("a WARP device");
        unsafe { ID3D11Device::from_raw(device) }
    }

    /// The sentinel's function runs when the object is destroyed, not before, and the watch holds no reference.
    #[test]
    fn a_watch_ends_with_its_object() {
        let device = warp();
        let gone = Arc::new(AtomicBool::new(false));
        let seen = gone.clone();
        let before = unsafe { add_ref(device.as_raw()) };
        unsafe { release(device.as_raw()) };
        assert!(unsafe { watch(device.as_raw(), move || seen.store(true, Ordering::SeqCst)) });
        let after = unsafe { add_ref(device.as_raw()) };
        unsafe { release(device.as_raw()) };
        assert_eq!(before, after, "the watch takes no reference to the object");
        let child = unsafe {
            let mut b = None;
            device
                .CreateBuffer(
                    &D3D11_BUFFER_DESC {
                        ByteWidth: 16,
                        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut b),
                )
                .unwrap();
            b.unwrap()
        };
        drop(device);
        assert!(!gone.load(Ordering::SeqCst), "a child keeps its device");
        drop(child);
        assert!(
            gone.load(Ordering::SeqCst),
            "the device's destruction runs the watch"
        );
    }

    /// A face registered by one pointer is found by any other of the object, and forgotten with it.
    #[test]
    fn faces_are_found_by_any_pointer() {
        let device = warp();
        let dxgi = device
            .cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>()
            .unwrap();
        unsafe { register(Kind::Device, 0x1000, &[device.as_raw()]) };
        assert_eq!(unsafe { find(dxgi.as_raw()) }, Some((Kind::Device, 0x1000)));
        forget(0x1000);
        assert_eq!(unsafe { find(device.as_raw()) }, None);
    }
}
