//! The real `d3d11.dll` and `dxgi.dll`, loaded from the system folder by full path: our own `d3d11.dll` and
//! `dxgi.dll` are loaded under those names first, so a bare name gives back ours.

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
        // Windows gives a DLL asked for by name the module already loaded under that name. A game that loads one of
        // ours only later, by name (Unreal Engine 5 delay-loads d3d11.dll), would be given the real one loaded below,
        // and its device would never reach locum: so ours beside locum.dll go in first.
        if let Some(own) = module_of(system as *const c_void).map(module_path)
            && let Some(own) = own.parent()
        {
            for name in ["d3d11.dll", "dxgi.dll", "d3d12.dll"] {
                if own.join(name).is_file() && !same_folder(own, &folder) {
                    load(own, name);
                }
            }
        }
        let d3d11 = load(&folder, "d3d11.dll").unwrap_or_default();
        let dxgi = load(&folder, "dxgi.dll").unwrap_or_default();
        System { folder, d3d11, dxgi }
    })
}

/// The real `d3d12.dll`, loaded from the system folder the first time a D3D12 export is called (a D3D11 game never
/// gets it).
pub fn system_d3d12() -> HMODULE {
    static D3D12: OnceLock<usize> = OnceLock::new();
    HMODULE(
        *D3D12.get_or_init(|| load(&system().folder, "d3d12.dll").map_or(0, |h| h.0 as usize)) as *mut c_void,
    )
}

/// Whether two folders are the same (Windows paths compare without case).
fn same_folder(a: &std::path::Path, b: &std::path::Path) -> bool {
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

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

/// The address of `name` in `module`, as the function type `F`.
///
/// Overlays can redirect a system DLL's exports to their own code (Epic Online Services' overlay does, for
/// `D3D11CreateDevice`), and their code then calls the export they found under the bare name, which is ours: an
/// address outside `module` is therefore not used, but the export's own, read from the DLL's file.
///
/// # Safety
/// `F` must be the export's real function pointer type.
pub unsafe fn export<F: Copy>(module: HMODULE, name: &'static core::ffi::CStr) -> Option<F> {
    assert_eq!(size_of::<F>(), size_of::<usize>());
    let address = checked_export(module, Export::Name(name))?;
    Some(unsafe { core::mem::transmute_copy(&address) })
}

/// Resolves a numbered export with the same overlay-redirection check as named exports.
pub fn export_ordinal(module: HMODULE, ordinal: u16) -> Option<usize> {
    checked_export(module, Export::Ordinal(ordinal))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Export {
    Name(&'static core::ffi::CStr),
    Ordinal(u16),
}

impl core::fmt::Display for Export {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Name(name) => write!(f, "{}", name.to_string_lossy()),
            Self::Ordinal(ordinal) => write!(f, "ordinal {ordinal}"),
        }
    }
}

fn checked_export(module: HMODULE, export: Export) -> Option<usize> {
    let name = match export {
        Export::Name(name) => PCSTR(name.as_ptr().cast()),
        Export::Ordinal(ordinal) => PCSTR(usize::from(ordinal) as *const u8),
    };
    let found = unsafe { GetProcAddress(module, name) }? as usize;
    checked_address(module, export, found)
}

fn checked_address(module: HMODULE, export: Export, found: usize) -> Option<usize> {
    let base = module.0 as usize;
    let size = unsafe { image_size(module) };
    let address = if (base..base.checked_add(size)?).contains(&found) {
        found
    } else {
        let own = own_export(module, export)?;
        static LOGGED: std::sync::Mutex<Vec<(usize, Export)>> = std::sync::Mutex::new(Vec::new());
        let mut logged = LOGGED.lock().unwrap_or_else(|e| e.into_inner());
        if !logged.contains(&(base, export)) {
            logged.push((base, export));
            let by = module_of(found as *const c_void).map(module_path);
            log!(
                "{} of {} leads to {:#x} in {}: called at its own address {:#x} instead",
                export,
                module_path(module).display(),
                found,
                by.map_or("no module".into(), |p| p.display().to_string()),
                own
            );
        }
        own
    };
    Some(address)
}

/// The size of a loaded module's image, from its PE headers.
///
/// # Safety
/// `module` must be a loaded module.
unsafe fn image_size(module: HMODULE) -> usize {
    let base = module.0 as *const u8;
    unsafe {
        let pe = base.add(base.add(0x3c).cast::<u32>().read_unaligned() as usize);
        // SizeOfImage: the optional header (after the 4-byte signature and 20-byte file header), offset 56.
        pe.add(24 + 56).cast::<u32>().read_unaligned() as usize
    }
}

/// The address of `name` in `module` as the module's file exports it (what its in-memory export table said before
/// anything changed it). None for an export forwarded to another DLL or not found.
fn own_export(module: HMODULE, export: Export) -> Option<usize> {
    let file = std::fs::read(module_path(module)).ok()?;
    let rva = pe_export_rva(&file, export)?;
    (rva != 0 && (rva as usize) < unsafe { image_size(module) })
        .then(|| (module.0 as usize).checked_add(rva as usize))?
}

/// The RVA of an exported function in a PE32+ file's bytes, or None (forwarded, absent or malformed).
fn pe_export_rva(file: &[u8], export: Export) -> Option<u32> {
    let u16_at = |o: usize| Some(u16::from_le_bytes(file.get(o..o + 2)?.try_into().ok()?));
    let u32_at = |o: usize| Some(u32::from_le_bytes(file.get(o..o + 4)?.try_into().ok()?));
    let pe = u32_at(0x3c)? as usize;
    if file.get(pe..pe + 4)? != b"PE\0\0" {
        return None;
    }
    let sections = u16_at(pe + 6)? as usize;
    let optional = pe + 24;
    let optional_size = u16_at(pe + 20)? as usize;
    if u16_at(optional)? != 0x20b {
        return None;
    }
    // Data directory 0 (exports) of a PE32+ optional header.
    let (dir, dir_size) = (u32_at(optional + 112)?, u32_at(optional + 116)?);
    let table = optional + optional_size;
    let offset = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|i| {
            let s = table + i * 40;
            let (size, address, raw) = (u32_at(s + 8)?, u32_at(s + 12)?, u32_at(s + 20)?);
            (address..address.checked_add(size)?)
                .contains(&rva)
                .then(|| (rva - address + raw) as usize)
        })
    };
    let d = offset(dir)?;
    let (count, functions, names, ordinals) = (
        u32_at(d + 24)? as usize,
        u32_at(d + 28)?,
        u32_at(d + 32)?,
        u32_at(d + 36)?,
    );
    let functions = offset(functions)?;
    let index = match export {
        Export::Ordinal(ordinal) => u32::from(ordinal).checked_sub(u32_at(d + 16)?)?,
        Export::Name(name) => {
            let name = name.to_bytes();
            let (names, ordinals) = (offset(names)?, offset(ordinals)?);
            let index = (0..count).find(|&i| {
                u32_at(names + 4 * i)
                    .and_then(offset)
                    .and_then(|o| file.get(o..o + name.len() + 1))
                    .is_some_and(|s| &s[..name.len()] == name && s[name.len()] == 0)
            })?;
            u32::from(u16_at(ordinals + 2 * index)?)
        }
    };
    if index >= u32_at(d + 20)? {
        return None;
    }
    let rva = u32_at(functions + 4 * index as usize)?;
    // An address inside the export directory names another DLL's export, not code.
    (rva != 0 && !(dir..dir.checked_add(dir_size)?).contains(&rva)).then_some(rva)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The file's export table gives the address the loader gives for an unchanged system DLL.
    #[test]
    fn own_export_matches_the_loader() {
        let folder = system_folder();
        for (dll, names) in [
            (
                "d3d11.dll",
                &[c"D3D11CreateDevice", c"D3D11CreateDeviceAndSwapChain"][..],
            ),
            ("dxgi.dll", &[c"CreateDXGIFactory1", c"CreateDXGIFactory2"][..]),
        ] {
            let module = load(&folder, dll).expect("system DLL");
            for name in names {
                let loader =
                    unsafe { GetProcAddress(module, PCSTR(name.as_ptr().cast())) }.expect("export") as usize;
                assert_eq!(
                    own_export(module, Export::Name(name)),
                    Some(loader),
                    "{dll} {name:?}"
                );
            }
            assert_eq!(own_export(module, Export::Name(c"NoSuchExport")), None);
        }
    }

    #[test]
    fn ordinal_exports_recover_from_out_of_module_redirection() {
        let module = load(&system_folder(), "d3d12.dll").expect("system D3D12");
        // Every ordinal the loader knows (which ones depends on the Windows build), and none it doesn't.
        let mut found = 0;
        for ordinal in 0..=1024 {
            let export = Export::Ordinal(ordinal);
            let loader = unsafe { GetProcAddress(module, PCSTR(usize::from(ordinal) as *const u8)) }
                .map(|f| f as usize);
            assert_eq!(own_export(module, export), loader, "ordinal {ordinal}");
            assert_eq!(export_ordinal(module, ordinal), loader);
            if let Some(loader) = loader {
                found += 1;
                // Inject the address an overlay could return, without modifying a live system module.
                let foreign = ordinal_exports_recover_from_out_of_module_redirection as *const () as usize;
                assert_eq!(checked_address(module, export, foreign), Some(loader));
            }
        }
        assert!(found > 0);
        assert_eq!(own_export(module, Export::Ordinal(u16::MAX)), None);
        let name = Export::Name(c"D3D12CreateDevice");
        assert_eq!(checked_address(module, name, 1), own_export(module, name));
    }

    fn system_folder() -> PathBuf {
        let mut buf = [0u16; 260];
        let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
        PathBuf::from(String::from_utf16_lossy(&buf[..n]))
    }
}
