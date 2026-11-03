//! The real `d3d11.dll` and `dxgi.dll`, loaded from the system folder by full path: our own `d3d11.dll` and
//! `dxgi.dll` are already loaded under those names, so a bare name would give them back.

use core::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW,
    GetModuleHandleExW, GetProcAddress, LOAD_LIBRARY_FLAGS, LoadLibraryExW,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::core::{PCSTR, PCWSTR};

use crate::log::log;

pub struct System {
    pub folder: PathBuf,
    pub d3d11: HMODULE,
    pub dxgi: HMODULE,
}

// Module handles are process-wide and immutable once loaded.
unsafe impl Send for System {}
unsafe impl Sync for System {}

pub fn system() -> &'static System {
    static SYSTEM: OnceLock<System> = OnceLock::new();
    SYSTEM.get_or_init(|| {
        let mut buf = [0u16; 260];
        let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
        let folder = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
        let d3d11 = load(&folder, "d3d11.dll").unwrap_or_default();
        let dxgi = load(&folder, "dxgi.dll").unwrap_or_default();
        System { folder, d3d11, dxgi }
    })
}

/// The address of `name` in `module`, as the function type `F`.
///
/// # Safety
/// `F` must be the export's real function pointer type.
/// Loads `name` from `folder` by full path.
fn load(folder: &std::path::Path, name: &str) -> Option<HMODULE> {
    let path = folder.join(name);
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    match unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_LIBRARY_FLAGS(0)) } {
        Ok(h) => {
            log!("loaded {} at {:p}", path.display(), h.0);
            Some(h)
        }
        Err(e) => {
            log!("FAILED to load {}: {e}", path.display());
            None
        }
    }
}

/// Loads a DLL from the system folder (once; later calls give the same module).
pub fn load_system_dll(name: &'static str) -> Option<HMODULE> {
    static LOADED: std::sync::Mutex<Vec<(&'static str, usize)>> = std::sync::Mutex::new(Vec::new());
    let mut loaded = LOADED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(&(_, h)) = loaded.iter().find(|(n, _)| *n == name) {
        return Some(HMODULE(h as *mut c_void));
    }
    let h = load(&system().folder, name)?;
    loaded.push((name, h.0 as usize));
    Some(h)
}

pub unsafe fn export<F: Copy>(module: HMODULE, name: &'static core::ffi::CStr) -> Option<F> {
    assert_eq!(size_of::<F>(), size_of::<usize>());
    let f = unsafe { GetProcAddress(module, PCSTR(name.as_ptr().cast())) }?;
    Some(unsafe { core::mem::transmute_copy(&f) })
}

/// The address the function calling this one will return to, i.e. its caller's code.
#[inline(never)]
pub fn caller_of_caller() -> *const c_void {
    let mut frames = [core::ptr::null_mut::<c_void>(); 1];
    // Frame 0 is this function, 1 the export that called it, 2 that export's caller.
    let n = unsafe { RtlCaptureStackBackTrace(2, &mut frames, None) };
    if n == 1 { frames[0] } else { core::ptr::null() }
}

/// The module containing `address` (no reference taken), if any.
pub fn module_of(address: *const c_void) -> Option<HMODULE> {
    let mut module = HMODULE::default();
    let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    unsafe { GetModuleHandleExW(flags, PCWSTR(address.cast()), &mut module) }.ok()?;
    Some(module)
}

pub fn module_path(module: HMODULE) -> PathBuf {
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..n]))
}

/// Whether code at `address` belongs to Windows or the graphics driver rather than the game: such callers (the
/// real d3d11.dll asking dxgi for a factory, for one) get the real objects, unwrapped.
pub fn is_system_caller(address: *const c_void) -> bool {
    let Some(module) = module_of(address) else {
        return false;
    };
    let path = module_path(module).to_string_lossy().to_lowercase();
    let system = system().folder.to_string_lossy().to_lowercase();
    path.starts_with(&system) || path.contains("\\driverstore\\")
}
