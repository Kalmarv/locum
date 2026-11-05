//! The machine a capture was made on, and the one a replay runs on: the adapter and its driver, Windows, the
//! Direct3D 12 runtime and the game's executable. A replay is compared byte for byte with the game's GPU, and drivers
//! differ in their last bits (NVIDIA and WARP clear to a fractional UNORM colour differently), so a verify result
//! means something only with both ends named. A capture keeps it as JSON in `meta.environment`; captures from before
//! it have none, and every part is optional, since a probe can fail on one machine and not on another.

use std::ffi::c_void;
use std::path::Path;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};
use windows::Wdk::Graphics::Direct3D::{
    D3DKMT_CLOSEADAPTER, D3DKMT_OPENADAPTERFROMLUID, D3DKMT_QUERYADAPTERINFO, D3DKMT_UMDFILENAMEINFO,
    KMTQAITYPE_UMDRIVERNAME, KMTUMDVERSION_DX11, KMTUMDVERSION_DX12,
};
use windows::Win32::Foundation::{FreeLibrary, LUID, NTSTATUS};
use windows::Win32::Graphics::Dxgi::{DXGI_ADAPTER_FLAG_SOFTWARE, IDXGIAdapter, IDXGIAdapter1, IDXGIDevice};
use windows::Win32::System::LibraryLoader::{
    FindResourceW, GetModuleFileNameW, GetModuleHandleW, GetProcAddress, LOAD_LIBRARY_AS_DATAFILE,
    LOAD_LIBRARY_AS_IMAGE_RESOURCE, LoadLibraryExW, LoadLibraryW, LoadResource, LockResource, SizeofResource,
};
use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::core::{HSTRING, Interface, PCSTR, PCWSTR};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Environment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<Adapter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<Os>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe: Option<Exe>,
    /// The Direct3D 12 runtime the process loaded (none for Direct3D 11).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d3d12: Option<Runtime>,
}

/// An adapter as DXGI describes it, and its driver.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Adapter {
    pub description: String,
    pub vendor: u32,
    pub device: u32,
    pub subsystem: u32,
    pub revision: u32,
    /// `HIGH:LOW` in hex, as DXGI's `LUID` (it changes when the machine restarts).
    pub luid: String,
    /// WARP, or another adapter DXGI says is software.
    pub software: bool,
    pub video_memory: u64,
    /// The user-mode driver's version as DXGI reports it (`CheckInterfaceSupport`), what Device Manager shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    /// The user-mode driver DLL the API loads for this adapter, as the kernel names it (WARP: the module loaded).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub umd: Option<Module>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Module {
    pub path: String,
    /// The file's version resource (`VS_FIXEDFILEINFO`), `a.b.c.d`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Os {
    /// `major.minor.build.revision` (the revision is the update build, `UBR`).
    pub version: String,
    /// The release Windows names itself by (`25H2`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Exe {
    pub path: String,
    pub bytes: u64,
    /// As `table.txt`'s `exe sha256` writes it.
    pub sha256: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Runtime {
    /// The `D3D12Core.dll` loaded in the process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core: Option<Module>,
    /// The core is not the system's: an Agility SDK the game ships.
    pub agility: bool,
    /// What the executable exports as `D3D12SDKVersion` and `D3D12SDKPath`, which ask for an Agility SDK.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk_path: Option<String>,
}

/// Which API's user-mode driver an adapter's description names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    D3d11,
    D3d12,
}

impl Environment {
    /// A capture's, from its `meta.environment`.
    pub fn parse(json: &str) -> Option<Environment> {
        serde_json::from_str(json).ok()
    }

    pub fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The adapter and its driver in one phrase: `NVIDIA GeForce RTX 5070 Ti (10de:2c05), driver 32.0.15.8180`.
    pub fn adapter_line(&self) -> String {
        let Some(a) = &self.adapter else {
            return "an adapter not recorded".into();
        };
        let mut s = format!("{} ({:04x}:{:04x})", a.description, a.vendor, a.device);
        match (&a.driver, a.umd.as_ref().and_then(|u| u.version.as_ref())) {
            (Some(d), _) => s += &format!(", driver {d}"),
            (None, Some(v)) => s += &format!(", driver {v}"),
            (None, None) => s += ", driver not recorded",
        }
        s
    }

    /// The adapter, Windows and the Direct3D 12 runtime in one line, as `verify` says where each end ran.
    pub fn line(&self) -> String {
        let mut s = self.adapter_line();
        if let Some(os) = &self.os {
            s += &format!(", Windows {}", os.version);
        }
        if let Some(r) = &self.d3d12 {
            s += &format!(", {}", r.line());
        }
        s
    }
}

impl Runtime {
    pub fn line(&self) -> String {
        let version = self
            .core
            .as_ref()
            .and_then(|c| c.version.clone())
            .unwrap_or_else(|| "of an unknown version".into());
        if self.agility {
            format!("D3D12Core {version} (Agility SDK)")
        } else {
            format!("D3D12Core {version} (system)")
        }
    }
}

/// What DXGI and the kernel say of `adapter` and the driver `api` loads for it.
pub fn adapter(adapter: &IDXGIAdapter, api: Api) -> Adapter {
    let mut out = Adapter::default();
    let Ok(desc) = (unsafe { adapter.cast::<IDXGIAdapter1>().and_then(|a| a.GetDesc1()) }) else {
        return out;
    };
    let end = desc
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(desc.Description.len());
    out.description = String::from_utf16_lossy(&desc.Description[..end]);
    out.vendor = desc.VendorId;
    out.device = desc.DeviceId;
    out.subsystem = desc.SubSysId;
    out.revision = desc.Revision;
    out.luid = format!(
        "{:08x}:{:08x}",
        desc.AdapterLuid.HighPart, desc.AdapterLuid.LowPart
    );
    out.software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
    out.video_memory = desc.DedicatedVideoMemory as u64;
    // IDXGIDevice is the interface every driver answers this for (Direct3D 11 and 12 ones say unsupported).
    out.driver = unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) }
        .ok()
        .map(|v| four(v as u64));
    out.umd = umd(desc.AdapterLuid, api).or_else(|| {
        // The kernel's Basic Render Driver names no user-mode driver; WARP is the module the API loaded.
        out.software.then(|| loaded("d3d10warp.dll")).flatten()
    });
    out
}

/// The adapter a Direct3D 11 device runs on.
pub fn device_adapter(device: &impl Interface) -> Option<IDXGIAdapter> {
    unsafe { device.cast::<IDXGIDevice>().ok()?.GetAdapter().ok() }
}

/// The user-mode driver the kernel names for the adapter with `luid` and `api`. The thunks are looked up in gdi32 when
/// called: locum's `d3d11.dll` exports the same names (the system's does), so linking them would define them twice.
fn umd(luid: LUID, api: Api) -> Option<Module> {
    type Open = unsafe extern "system" fn(*mut D3DKMT_OPENADAPTERFROMLUID) -> NTSTATUS;
    type Query = unsafe extern "system" fn(*const D3DKMT_QUERYADAPTERINFO) -> NTSTATUS;
    type Close = unsafe extern "system" fn(*const D3DKMT_CLOSEADAPTER) -> NTSTATUS;
    let gdi = unsafe { LoadLibraryW(&HSTRING::from("gdi32.dll")) }.ok()?;
    let thunk = |name: &'static [u8]| unsafe { GetProcAddress(gdi, PCSTR(name.as_ptr())) };
    let open_adapter: Open = unsafe { std::mem::transmute(thunk(b"D3DKMTOpenAdapterFromLuid\0")?) };
    let query_adapter: Query = unsafe { std::mem::transmute(thunk(b"D3DKMTQueryAdapterInfo\0")?) };
    let close_adapter: Close = unsafe { std::mem::transmute(thunk(b"D3DKMTCloseAdapter\0")?) };
    let mut open = D3DKMT_OPENADAPTERFROMLUID {
        AdapterLuid: luid,
        hAdapter: 0,
    };
    if unsafe { open_adapter(&mut open) }.is_err() {
        return None;
    }
    let mut info = D3DKMT_UMDFILENAMEINFO {
        Version: match api {
            Api::D3d11 => KMTUMDVERSION_DX11,
            Api::D3d12 => KMTUMDVERSION_DX12,
        },
        ..Default::default()
    };
    let query = D3DKMT_QUERYADAPTERINFO {
        hAdapter: open.hAdapter,
        Type: KMTQAITYPE_UMDRIVERNAME,
        pPrivateDriverData: &mut info as *mut _ as *mut c_void,
        PrivateDriverDataSize: size_of::<D3DKMT_UMDFILENAMEINFO>() as u32,
    };
    let found = unsafe { query_adapter(&query) }.is_ok();
    unsafe {
        let _ = close_adapter(&D3DKMT_CLOSEADAPTER {
            hAdapter: open.hAdapter,
        });
    }
    let end = info.UmdFileName.iter().position(|&c| c == 0).unwrap_or(0);
    (found && end > 0).then(|| {
        let path = String::from_utf16_lossy(&info.UmdFileName[..end]);
        Module {
            version: version(Path::new(&path)),
            path,
        }
    })
}

/// A module loaded in this process, by name.
fn loaded(name: &str) -> Option<Module> {
    let module = unsafe { GetModuleHandleW(&HSTRING::from(name)) }.ok()?;
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    (n > 0).then(|| {
        let path = String::from_utf16_lossy(&buf[..n]);
        Module {
            version: version(Path::new(&path)),
            path,
        }
    })
}

/// A file's version (its fixed file version) as `a.b.c.d`, from its version resource with the file mapped as data.
/// Not through `GetFileVersionInfoW`: in a process whose manifest does not declare Windows 10, it gives Windows' own
/// files the Windows version that process is shown, and capture and replay are different processes. D3D12Core.dll
/// 10.0.26100.9549 read 6.2.26100.9549 in locum-cli and the smoke fixture, and 10.0.26100.9549 in Motor Town
/// (2026-11-05).
pub fn version(path: &Path) -> Option<String> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let flags = LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE;
    let module = unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, flags) }.ok()?;
    let resource = || {
        // VS_VERSION_INFO, of type RT_VERSION.
        let r = unsafe { FindResourceW(Some(module), PCWSTR(1 as _), PCWSTR(16 as _)) };
        if r.is_invalid() {
            return None;
        }
        let len = unsafe { SizeofResource(Some(module), r) } as usize;
        let p = unsafe { LockResource(LoadResource(Some(module), r).ok()?) };
        (!p.is_null()).then(|| fixed_version(unsafe { std::slice::from_raw_parts(p as *const u8, len) }))?
    };
    let found = resource();
    let _ = unsafe { FreeLibrary(module) };
    found
}

/// The file version in a VS_VERSIONINFO block: after its 6-byte header and its key, `VS_VERSION_INFO` in UTF-16 with
/// its terminator, padded to 4 bytes, comes VS_FIXEDFILEINFO: its signature, its structure version, then the file
/// version's high and low 32 bits.
fn fixed_version(block: &[u8]) -> Option<String> {
    const FIXED: usize = (6 + 2 * 16 + 3) & !3;
    let word = |at: usize| {
        block
            .get(FIXED + at..FIXED + at + 4)
            .map(|w| u32::from_le_bytes(w.try_into().expect("4 bytes")))
    };
    (word(0)? == 0xFEEF_04BD).then_some(())?;
    Some(four((u64::from(word(8)?) << 32) | u64::from(word(12)?)))
}

/// Four 16-bit parts, most significant first.
fn four(v: u64) -> String {
    format!(
        "{}.{}.{}.{}",
        v >> 48,
        (v >> 32) & 0xffff,
        (v >> 16) & 0xffff,
        v & 0xffff
    )
}

/// Windows' version, from the registry: unlike `GetVersionEx`, no compatibility shim the game runs under changes it.
pub fn os() -> Os {
    static OS: OnceLock<Os> = OnceLock::new();
    OS.get_or_init(|| {
        let key = HSTRING::from(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion");
        let dword = |name: &str| {
            let mut v = 0u32;
            let mut size = 4u32;
            unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    &key,
                    &HSTRING::from(name),
                    RRF_RT_REG_DWORD,
                    None,
                    Some(&mut v as *mut u32 as *mut c_void),
                    Some(&mut size),
                )
            }
            .is_ok()
            .then_some(v)
        };
        let text = |name: &str| {
            let mut buf = [0u16; 128];
            let mut size = (buf.len() * 2) as u32;
            unsafe {
                RegGetValueW(
                    HKEY_LOCAL_MACHINE,
                    &key,
                    &HSTRING::from(name),
                    RRF_RT_REG_SZ,
                    None,
                    Some(buf.as_mut_ptr() as *mut c_void),
                    Some(&mut size),
                )
            }
            .is_ok()
            .then(|| {
                let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                String::from_utf16_lossy(&buf[..end])
            })
        };
        Os {
            version: format!(
                "{}.{}.{}.{}",
                dword("CurrentMajorVersionNumber").unwrap_or(0),
                dword("CurrentMinorVersionNumber").unwrap_or(0),
                text("CurrentBuildNumber").unwrap_or_default(),
                dword("UBR").unwrap_or(0)
            ),
            release: text("DisplayVersion"),
        }
    })
    .clone()
}

/// This process's executable: its path, size, SHA-256 and version. Hashed once per process (an Unreal game's is a few
/// hundred MB), read a MiB at a time, so call it off the game's threads.
pub fn exe() -> Option<Exe> {
    static EXE: OnceLock<Option<Exe>> = OnceLock::new();
    EXE.get_or_init(|| {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let path = std::env::current_exe().ok()?;
        let mut file = std::fs::File::open(&path).ok()?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut bytes = 0u64;
        loop {
            let n = file.read(&mut buf).ok()?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            bytes += n as u64;
        }
        Some(Exe {
            path: path.display().to_string(),
            bytes,
            sha256: hasher.finalize().iter().map(|b| format!("{b:02x}")).collect(),
            version: version(&path),
        })
    })
    .clone()
}

/// The Direct3D 12 runtime this process loaded, and the Agility SDK its executable asks for.
pub fn d3d12_runtime() -> Runtime {
    let core = loaded("D3D12Core.dll");
    let mut system = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(Some(&mut system)) } as usize;
    let system = String::from_utf16_lossy(&system[..n]).to_ascii_lowercase();
    let agility = core.as_ref().is_some_and(|c| {
        let folder = Path::new(&c.path)
            .parent()
            .map(|p| p.display().to_string().to_ascii_lowercase());
        folder.is_some_and(|f| f != system)
    });
    let exe = unsafe { GetModuleHandleW(PCWSTR::null()) }.ok();
    let export = |name: &'static [u8]| exe.and_then(|m| unsafe { GetProcAddress(m, PCSTR(name.as_ptr())) });
    let sdk_version = export(b"D3D12SDKVersion\0").map(|p| unsafe { *(p as *const u32) });
    let sdk_path = export(b"D3D12SDKPath\0").and_then(|p| {
        let p = unsafe { *(p as *const *const u8) };
        (!p.is_null()).then(|| {
            unsafe { std::ffi::CStr::from_ptr(p as *const core::ffi::c_char) }
                .to_string_lossy()
                .into_owned()
        })
    });
    Runtime {
        core,
        agility,
        sdk_version,
        sdk_path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_and_partial_environments_read() {
        assert_eq!(Environment::parse("{}"), Some(Environment::default()));
        let e =
            Environment::parse(r#"{"adapter":{"description":"WARP","software":true},"later":1}"#).unwrap();
        assert_eq!(e.adapter.as_ref().unwrap().description, "WARP");
        assert_eq!(e.adapter_line(), "WARP (0000:0000), driver not recorded");
        assert_eq!(Environment::default().line(), "an adapter not recorded");
    }

    #[test]
    fn versions_are_the_files_own() {
        let mut block = vec![0u8; 40];
        block.extend(0xFEEF_04BDu32.to_le_bytes());
        block.extend(0x0001_0000u32.to_le_bytes());
        block.extend(0x000A_0000u32.to_le_bytes());
        block.extend(0x6594_254Du32.to_le_bytes());
        assert_eq!(fixed_version(&block).as_deref(), Some("10.0.26004.9549"));
        assert_eq!(fixed_version(&block[..50]), None);
        block[40] = 0;
        assert_eq!(fixed_version(&block), None);
        // This test's executable declares no Windows 10 support: GetFileVersionInfoW would say 6.2 here.
        let kernel = version(Path::new("C:/Windows/System32/kernel32.dll")).unwrap();
        assert!(kernel.starts_with("10.0."), "{kernel}");
    }

    #[test]
    fn this_machine_is_described() {
        let os = os();
        assert!(os.version.starts_with("10.0."), "{os:?}");
        let exe = exe().unwrap();
        assert_eq!(exe.sha256.len(), 64);
        assert!(exe.bytes > 0);
        let round = Environment {
            os: Some(os),
            exe: Some(exe),
            ..Default::default()
        };
        assert_eq!(Environment::parse(&round.json()), Some(round));
    }
}
