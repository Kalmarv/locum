//! Which Direct3D a replay runs on. locum-cli's `d3d11.dll`, `dxgi.dll` and `d3d12.dll` are delay-loaded (see
//! `build.rs`): Windows would otherwise give its imports the first DLLs of those names in its folder, and beside
//! `target/release/` or a game's folder those are locum's own stand-ins, so a replay would run through locum's
//! wrappers. Before any of them is called, [`load`] loads each by full path: from the system folder, or from a
//! translation layer's folder (`--layer`, DXVK's or vkd3d-proton's DLLs) where it has one. A DLL asked for by name
//! later is the one already loaded under that name, wherever it came from.

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use windows::Win32::System::LibraryLoader::{LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::core::PCWSTR;

/// The DLLs a replay's device comes from, in the order they are loaded: each before those that import it (DXVK's
/// `d3d11.dll` imports its `dxgi.dll`, vkd3d-proton's `d3d12.dll` its `d3d12core.dll`).
pub const DLLS: [&str; 5] = [
    "dxgi.dll",
    "d3d10core.dll",
    "d3d11.dll",
    "d3d12core.dll",
    "d3d12.dll",
];

/// The system folder.
pub fn system() -> PathBuf {
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    PathBuf::from(String::from_utf16_lossy(&buf[..n]))
}

/// Loads the replay's Direct3D DLLs: those `layer` has from it, the rest of `d3d11.dll`, `dxgi.dll` and `d3d12.dll`
/// from the system folder. A layer's DLL finds the DLLs it imports beside it. Fails where one cannot be loaded.
pub fn load(layer: Option<&Path>) -> Result<(), String> {
    let system = system();
    for name in DLLS {
        let from = match layer {
            Some(dir) if dir.join(name).is_file() => dir.join(name),
            // The system's D3D12Core.dll and d3d10core.dll are its d3d12.dll's and d3d11.dll's to load.
            _ if matches!(name, "d3d12core.dll" | "d3d10core.dll") => continue,
            _ => system.join(name),
        };
        let wide: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, LOAD_WITH_ALTERED_SEARCH_PATH) }
            .map_err(|e| format!("load {}: {e}", from.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_system_runtime_is_loaded_by_path() {
        super::load(None).unwrap();
        // Nothing of Direct3D from outside the system folder: what the replay's environment calls layers.
        assert_eq!(locum_capture::environment::layers(), Default::default());
    }
}
