//! The machine a capture was made on, and the one a replay runs on: the adapter and its driver, Windows, the
//! Direct3D 12 runtime and the game's executable. A replay is compared byte for byte with the game's GPU, and drivers
//! differ in their last bits (NVIDIA and WARP clear to a fractional UNORM colour differently), so a verify result
//! means something only with both ends named. A capture keeps it as JSON in `meta.environment`; captures from before
//! it have none. A probe can fail on one machine and not on another: what one could not learn is left out, and its
//! part's `missing` says why (the capture also has an `environment` gap for it), so nothing is dropped unsaid.

use std::collections::BTreeMap;
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

/// Why the parts a probe did not learn are missing, by name.
pub type Missing = BTreeMap<String, String>;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Environment {
    /// The parts of these not learned (`adapter`, `os`, `exe`, `d3d12`), and why. A part not probed (the replay's
    /// executable, Direct3D 11's runtime) is neither here nor missing.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub missing: Missing,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adapter: Option<Adapter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<Os>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exe: Option<Exe>,
    /// The Direct3D 12 runtime the process loaded (none for Direct3D 11).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub d3d12: Option<Runtime>,
    /// The Direct3D and DXGI DLLs a replay loaded from outside the system folder, by name: a translation layer
    /// (DXVK's `d3d11.dll` and `dxgi.dll`, vkd3d-proton's `d3d12.dll`) that `locum-cli conform` put beside it. Not
    /// probed at capture, where locum's own stand-ins have these names.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub layers: BTreeMap<String, Layer>,
}

/// A DLL of a translation layer: where it was loaded from, its version and its SHA-256 (layers often have no version
/// resource, and a build is known by its bytes).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layer {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// `version` or `sha256`, when it could not be read, and why.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub missing: Missing,
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
    /// What of these was not learned (`driver`, `umd`, `umd.version`), and why.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub missing: Missing,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Module {
    pub path: String,
    /// The file's version resource (`VS_FIXEDFILEINFO`), `a.b.c.d`. None: the file has none, or, where its owner's
    /// `missing` says why, it could not be read.
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
    /// What of these was not learned (`version`: a part of it, written 0; `release`), and why.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub missing: Missing,
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
    /// `version`, when it was not learned, and why.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub missing: Missing,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Runtime {
    /// The `D3D12Core.dll` loaded in the process.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub core: Option<Module>,
    /// The core is not the system's, and the executable asks for an Agility SDK: one the game ships. A core from
    /// elsewhere that nothing asked for is a translation layer's (see [`Environment::layers`]).
    pub agility: bool,
    /// What the executable exports as `D3D12SDKVersion` and `D3D12SDKPath`, which ask for an Agility SDK.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk_version: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sdk_path: Option<String>,
    /// What of these was not learned (`core`, `core.version`), and why. An executable exporting no SDK version asks
    /// for none: that is not missing.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub missing: Missing,
}

/// Which API's user-mode driver an adapter's description names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    D3d11,
    D3d12,
}

impl Environment {
    /// This process's: the adapter `adapter` names (or why it has none), Windows, the executable when `exe`, and the
    /// Direct3D 12 runtime for `api` 12. The executable is hashed here, so call it off the game's threads.
    pub fn probe(adapter: Result<Adapter, String>, api: Api, exe: bool) -> Environment {
        let mut e = Environment {
            os: Some(os()),
            d3d12: (api == Api::D3d12).then(d3d12_runtime),
            ..Default::default()
        };
        match adapter {
            Ok(a) => e.adapter = Some(a),
            Err(why) => _ = e.missing.insert("adapter".into(), why),
        }
        if exe {
            match self::exe() {
                Ok(x) => e.exe = Some(x),
                Err(why) => _ = e.missing.insert("exe".into(), why),
            }
        }
        e
    }

    /// A capture's, from its `meta.environment`.
    pub fn parse(json: &str) -> Option<Environment> {
        serde_json::from_str(json).ok()
    }

    pub fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Every part not learned, by its path (`adapter.umd.version`), and why.
    pub fn gaps(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> =
            self.missing.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut add = |part: &str, missing: &Missing| {
            out.extend(missing.iter().map(|(k, v)| (format!("{part}.{k}"), v.clone())));
        };
        if let Some(a) = &self.adapter {
            add("adapter", &a.missing);
        }
        if let Some(o) = &self.os {
            add("os", &o.missing);
        }
        if let Some(x) = &self.exe {
            add("exe", &x.missing);
        }
        if let Some(r) = &self.d3d12 {
            add("d3d12", &r.missing);
        }
        for (name, l) in &self.layers {
            add(&format!("layers.{name}"), &l.missing);
        }
        out
    }

    /// The adapter and its driver in one phrase: `NVIDIA GeForce RTX 5070 Ti (10de:2c05), driver 32.0.15.8180`; what
    /// is not known, with why.
    pub fn adapter_line(&self) -> String {
        let Some(a) = &self.adapter else {
            return match self.missing.get("adapter") {
                Some(why) => format!("an adapter not recorded ({why})"),
                None => "an adapter not recorded".into(),
            };
        };
        let mut s = format!("{} ({:04x}:{:04x})", a.description, a.vendor, a.device);
        match (&a.driver, a.umd.as_ref().and_then(|u| u.version.as_ref())) {
            (Some(d), _) => s += &format!(", driver {d}"),
            (None, Some(v)) => s += &format!(", driver {v}"),
            (None, None) => match a.missing.get("driver") {
                Some(why) => s += &format!(", driver not recorded ({why})"),
                None => s += ", driver not recorded",
            },
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
        // Each folder's layer DLLs, with what tells their build apart.
        let mut folders = BTreeMap::<String, Vec<String>>::new();
        for (name, l) in &self.layers {
            let folder = Path::new(&l.path)
                .parent()
                .map_or_else(String::new, |p| p.display().to_string());
            // A layer's version resource can be a stand-in (DXVK's say 10.0.17763.1): its bytes tell builds apart.
            let build = match (&l.version, &l.sha256) {
                (Some(v), Some(h)) => format!("{v} sha256 {}", &h[..h.len().min(12)]),
                (Some(v), None) => v.clone(),
                (None, Some(h)) => format!("sha256 {}", &h[..h.len().min(12)]),
                (None, None) => "of an unknown build".into(),
            };
            folders.entry(folder).or_default().push(format!("{name} {build}"));
        }
        for (folder, names) in folders {
            s += &format!(", through {} from {folder}", names.join(", "));
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
            .unwrap_or_else(
                || match self.missing.get("core").or(self.missing.get("core.version")) {
                    Some(why) => format!("of an unknown version ({why})"),
                    None => "of an unknown version".into(),
                },
            );
        let folder = self
            .core
            .as_ref()
            .and_then(|c| Path::new(&c.path).parent().map(Path::to_path_buf));
        if self.agility {
            format!("D3D12Core {version} (Agility SDK)")
        } else if let Some(folder) = folder
            .filter(|f| !f.ends_with("System32") && !f.ends_with("system32") && !f.ends_with("SYSTEM32"))
        {
            format!("D3D12Core {version} (from {})", folder.display())
        } else {
            format!("D3D12Core {version} (system)")
        }
    }
}

/// What DXGI and the kernel say of `adapter` and the driver `api` loads for it; why not, if DXGI cannot describe it.
pub fn adapter(adapter: &IDXGIAdapter, api: Api) -> Result<Adapter, String> {
    let desc = unsafe { adapter.cast::<IDXGIAdapter1>().and_then(|a| a.GetDesc1()) }
        .map_err(|e| format!("IDXGIAdapter1::GetDesc1: {e}"))?;
    let mut out = Adapter::default();
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
    match unsafe { adapter.CheckInterfaceSupport(&IDXGIDevice::IID) } {
        Ok(v) => out.driver = Some(four(v as u64)),
        Err(e) => {
            _ = out
                .missing
                .insert("driver".into(), format!("CheckInterfaceSupport: {e}"))
        }
    }
    let umd = umd(desc.AdapterLuid, api).or_else(|why| {
        // The kernel's Basic Render Driver names no user-mode driver; WARP is the module the API loaded.
        if !out.software {
            return Err(why);
        }
        loaded("d3d10warp.dll").map_err(|w| format!("{why}; and of WARP: {w}"))
    });
    match umd {
        Ok((module, version)) => {
            if let Err(why) = version {
                out.missing.insert("umd.version".into(), why);
            }
            out.umd = Some(module);
        }
        Err(why) => _ = out.missing.insert("umd".into(), why),
    }
    Ok(out)
}

/// The adapter a Direct3D 11 device runs on.
pub fn device_adapter(device: &impl Interface) -> Result<IDXGIAdapter, String> {
    let device = device
        .cast::<IDXGIDevice>()
        .map_err(|e| format!("the device as an IDXGIDevice: {e}"))?;
    unsafe { device.GetAdapter() }.map_err(|e| format!("IDXGIDevice::GetAdapter: {e}"))
}

/// A module at `path`, with its version or why it has none.
type Found = (Module, Result<(), String>);

fn module(path: String) -> Found {
    let version = version(Path::new(&path));
    let module = Module {
        version: version.clone().ok().flatten(),
        path,
    };
    (module, version.map(|_| ()))
}

/// The user-mode driver the kernel names for the adapter with `luid` and `api`, or why it names none. The thunks are
/// looked up in gdi32 when called: locum's `d3d11.dll` exports the same names (the system's does), so linking them
/// would define them twice.
fn umd(luid: LUID, api: Api) -> Result<Found, String> {
    type Open = unsafe extern "system" fn(*mut D3DKMT_OPENADAPTERFROMLUID) -> NTSTATUS;
    type Query = unsafe extern "system" fn(*const D3DKMT_QUERYADAPTERINFO) -> NTSTATUS;
    type Close = unsafe extern "system" fn(*const D3DKMT_CLOSEADAPTER) -> NTSTATUS;
    let gdi = unsafe { LoadLibraryW(&HSTRING::from("gdi32.dll")) }.map_err(|e| format!("gdi32.dll: {e}"))?;
    let thunk = |name: &'static [u8]| {
        unsafe { GetProcAddress(gdi, PCSTR(name.as_ptr())) }.ok_or_else(|| {
            format!(
                "gdi32.dll has no {}",
                String::from_utf8_lossy(&name[..name.len() - 1])
            )
        })
    };
    let open_adapter: Open = unsafe { std::mem::transmute(thunk(b"D3DKMTOpenAdapterFromLuid\0")?) };
    let query_adapter: Query = unsafe { std::mem::transmute(thunk(b"D3DKMTQueryAdapterInfo\0")?) };
    let close_adapter: Close = unsafe { std::mem::transmute(thunk(b"D3DKMTCloseAdapter\0")?) };
    let mut open = D3DKMT_OPENADAPTERFROMLUID {
        AdapterLuid: luid,
        hAdapter: 0,
    };
    let status = unsafe { open_adapter(&mut open) };
    if status.is_err() {
        return Err(format!("D3DKMTOpenAdapterFromLuid: NTSTATUS {:#010x}", status.0));
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
    let status = unsafe { query_adapter(&query) };
    unsafe {
        let _ = close_adapter(&D3DKMT_CLOSEADAPTER {
            hAdapter: open.hAdapter,
        });
    }
    if status.is_err() {
        return Err(format!(
            "D3DKMTQueryAdapterInfo(KMTQAITYPE_UMDRIVERNAME): NTSTATUS {:#010x}",
            status.0
        ));
    }
    let end = info.UmdFileName.iter().position(|&c| c == 0).unwrap_or(0);
    if end == 0 {
        return Err("the kernel names no user-mode driver for it".into());
    }
    Ok(module(String::from_utf16_lossy(&info.UmdFileName[..end])))
}

/// A module loaded in this process, by name.
fn loaded(name: &str) -> Result<Found, String> {
    let handle =
        unsafe { GetModuleHandleW(&HSTRING::from(name)) }.map_err(|_| format!("{name} is not loaded"))?;
    let mut buf = vec![0u16; 1024];
    let n = unsafe { GetModuleFileNameW(Some(handle), &mut buf) } as usize;
    if n == 0 {
        return Err(format!(
            "GetModuleFileNameW({name}): {}",
            windows::core::Error::from_thread()
        ));
    }
    Ok(module(String::from_utf16_lossy(&buf[..n])))
}

/// A file's version (its fixed file version) as `a.b.c.d`, from its version resource with the file mapped as data.
/// Not through `GetFileVersionInfoW`: in a process whose manifest does not declare Windows 10, it gives Windows' own
/// files the Windows version that process is shown, and capture and replay are different processes. D3D12Core.dll
/// 10.0.26100.9549 read 6.2.26100.9549 in locum-cli and the smoke fixture, and 10.0.26100.9549 in Motor Town
/// (2026-11-05). None when the file has no version resource (the smoke fixtures have none); why not, when it cannot
/// be read.
pub fn version(path: &Path) -> Result<Option<String>, String> {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let flags = LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE;
    let module = unsafe { LoadLibraryExW(PCWSTR(wide.as_ptr()), None, flags) }
        .map_err(|e| format!("{} as data: {e}", path.display()))?;
    let resource = || {
        // VS_VERSION_INFO, of type RT_VERSION.
        let r = unsafe { FindResourceW(Some(module), PCWSTR(1 as _), PCWSTR(16 as _)) };
        if r.is_invalid() {
            return Ok(None);
        }
        let len = unsafe { SizeofResource(Some(module), r) } as usize;
        let data =
            unsafe { LoadResource(Some(module), r) }.map_err(|e| format!("its version resource: {e}"))?;
        let p = unsafe { LockResource(data) };
        if p.is_null() {
            return Err("its version resource could not be locked".into());
        }
        fixed_version(unsafe { std::slice::from_raw_parts(p as *const u8, len) })
            .map(Some)
            .ok_or_else(|| "its version resource holds no VS_FIXEDFILEINFO".into())
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
            .ok()
            .map(|()| v)
            .map_err(|e| format!("{name}: {e}"))
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
            .ok()
            .map(|()| {
                let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                String::from_utf16_lossy(&buf[..end])
            })
            .map_err(|e| format!("{name}: {e}"))
        };
        let mut missing = Missing::new();
        let mut unread = Vec::new();
        let mut part = |r: Result<String, String>| {
            r.unwrap_or_else(|why| {
                unread.push(why);
                "0".into()
            })
        };
        let version = format!(
            "{}.{}.{}.{}",
            part(dword("CurrentMajorVersionNumber").map(|v| v.to_string())),
            part(dword("CurrentMinorVersionNumber").map(|v| v.to_string())),
            part(text("CurrentBuildNumber")),
            part(dword("UBR").map(|v| v.to_string()))
        );
        if !unread.is_empty() {
            missing.insert(
                "version".into(),
                format!(r"HKLM\{key} lacks {}", unread.join("; ")),
            );
        }
        let release = text("DisplayVersion")
            .map_err(|why| missing.insert("release".into(), format!(r"HKLM\{key} lacks {why}")))
            .ok();
        Os {
            version,
            release,
            missing,
        }
    })
    .clone()
}

/// This process's executable: its path, size, SHA-256 and version, or why not. Hashed once per process (an Unreal
/// game's is a few hundred MB), read a MiB at a time, so call it off the game's threads.
pub fn exe() -> Result<Exe, String> {
    static EXE: OnceLock<Result<Exe, String>> = OnceLock::new();
    EXE.get_or_init(|| {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let path = std::env::current_exe().map_err(|e| format!("its path: {e}"))?;
        let at = |e: std::io::Error| format!("{}: {e}", path.display());
        let mut file = std::fs::File::open(&path).map_err(at)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut bytes = 0u64;
        loop {
            let n = file.read(&mut buf).map_err(at)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            bytes += n as u64;
        }
        let version = version(&path);
        let mut missing = Missing::new();
        if let Err(why) = &version {
            missing.insert("version".into(), why.clone());
        }
        Ok(Exe {
            path: path.display().to_string(),
            bytes,
            sha256: hasher.finalize().iter().map(|b| format!("{b:02x}")).collect(),
            version: version.ok().flatten(),
            missing,
        })
    })
    .clone()
}

/// The DLLs of [`Environment::layers`] this process loaded from outside the system folder.
pub fn layers() -> BTreeMap<String, Layer> {
    let mut system = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(Some(&mut system)) } as usize;
    let system = String::from_utf16_lossy(&system[..n]).to_ascii_lowercase();
    let mut out = BTreeMap::new();
    for name in [
        "d3d11.dll",
        "dxgi.dll",
        "d3d12.dll",
        "d3d12core.dll",
        "d3d10core.dll",
    ] {
        let Ok((module, version)) = loaded(name) else {
            continue;
        };
        let path = Path::new(&module.path);
        if path
            .parent()
            .is_some_and(|f| f.display().to_string().to_ascii_lowercase() == system)
        {
            continue;
        }
        let mut layer = Layer {
            version: module.version.clone(),
            ..Default::default()
        };
        if let Err(why) = version {
            layer.missing.insert("version".into(), why);
        }
        match std::fs::read(path) {
            Ok(bytes) => {
                use sha2::{Digest, Sha256};
                layer.sha256 = Some(
                    Sha256::digest(&bytes)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect(),
                );
            }
            Err(e) => {
                _ = layer
                    .missing
                    .insert("sha256".into(), format!("{}: {e}", path.display()))
            }
        }
        layer.path = module.path;
        out.insert(name.to_string(), layer);
    }
    out
}

/// The Direct3D 12 runtime this process loaded, and the Agility SDK its executable asks for.
pub fn d3d12_runtime() -> Runtime {
    let mut missing = Missing::new();
    let core = match loaded("D3D12Core.dll") {
        Ok((core, version)) => {
            if let Err(why) = version {
                missing.insert("core.version".into(), why);
            }
            Some(core)
        }
        Err(why) => {
            missing.insert("core".into(), why);
            None
        }
    };
    let mut system = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(Some(&mut system)) } as usize;
    let system = String::from_utf16_lossy(&system[..n]).to_ascii_lowercase();
    let elsewhere = core.as_ref().is_some_and(|c| {
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
        agility: elsewhere && sdk_version.is_some(),
        sdk_version,
        sdk_path,
        missing,
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
    fn what_a_probe_could_not_learn_is_said() {
        let mut e = Environment::probe(Err("IDXGIAdapter1::GetDesc1: no".into()), Api::D3d12, false);
        assert_eq!(
            e.adapter_line(),
            "an adapter not recorded (IDXGIAdapter1::GetDesc1: no)"
        );
        assert!(e.line().contains(", Windows 10.0."), "{}", e.line());
        e.adapter = Some(Adapter {
            description: "X".into(),
            missing: [("driver".to_string(), "CheckInterfaceSupport: no".to_string())].into(),
            ..Default::default()
        });
        e.missing.clear();
        e.d3d12 = Some(Runtime {
            missing: [("core".to_string(), "D3D12Core.dll is not loaded".to_string())].into(),
            ..Default::default()
        });
        assert_eq!(
            e.line().replace(&format!(", Windows {}", os().version), ""),
            "X (0000:0000), driver not recorded (CheckInterfaceSupport: no), D3D12Core of an unknown version \
             (D3D12Core.dll is not loaded) (system)"
        );
        // Every part missing, by its path; and kept through the capture's JSON.
        let gaps = e.gaps();
        assert!(gaps.contains(&("adapter.driver".into(), "CheckInterfaceSupport: no".into())));
        assert!(gaps.contains(&("d3d12.core".into(), "D3D12Core.dll is not loaded".into())));
        assert_eq!(Environment::parse(&e.json()), Some(e));
        // What was never probed is not missing.
        let e = Environment::probe(Err("none".into()), Api::D3d11, false);
        assert_eq!((e.exe.as_ref(), e.d3d12.as_ref()), (None, None));
        assert_eq!(e.gaps(), vec![("adapter".to_string(), "none".to_string())]);
    }

    #[test]
    fn layers_are_named_by_folder_and_build() {
        let mut e = Environment::default();
        assert_eq!(e.line(), "an adapter not recorded");
        e.layers.insert(
            "d3d11.dll".into(),
            Layer {
                path: r"C:\dxvk\x64\d3d11.dll".into(),
                sha256: Some("0123456789abcdef".repeat(4)),
                ..Default::default()
            },
        );
        e.layers.insert(
            "dxgi.dll".into(),
            Layer {
                path: r"C:\dxvk\x64\dxgi.dll".into(),
                version: Some("2.7.1.0".into()),
                ..Default::default()
            },
        );
        assert_eq!(
            e.line(),
            r"an adapter not recorded, through d3d11.dll sha256 0123456789ab, dxgi.dll 2.7.1.0 from C:\dxvk\x64"
        );
        // A core from elsewhere that nothing asked for is said with its folder, not called an Agility SDK.
        let core = |path: &str| Runtime {
            core: Some(Module {
                path: path.into(),
                version: Some("1.2".into()),
            }),
            ..Default::default()
        };
        assert_eq!(
            core(r"C:\vkd3d\d3d12core.dll").line(),
            r"D3D12Core 1.2 (from C:\vkd3d)"
        );
        assert_eq!(
            core(r"C:\Windows\System32\D3D12Core.dll").line(),
            "D3D12Core 1.2 (system)"
        );
        assert!(Environment::parse(&e.json()).is_some_and(|p| p == e));
        // A replay with no layer loaded has none: the test executable loads no Direct3D DLL from elsewhere.
        assert_eq!(layers(), BTreeMap::new());
    }

    #[test]
    fn warp_is_described_in_full() {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIFactory4};
        let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)) }.unwrap();
        let warp: IDXGIAdapter = unsafe { factory.EnumWarpAdapter() }.unwrap();
        let a = adapter(&warp, Api::D3d12).unwrap();
        assert!(a.software && a.vendor == 0x1414, "{a:?}");
        assert!(a.driver.is_some(), "{a:?}");
        // The Basic Render Driver names no user-mode driver, and no device loaded WARP here: both said.
        if a.umd.is_none() {
            assert!(a.missing["umd"].contains("d3d10warp.dll is not loaded"), "{a:?}");
        }
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
        assert!(
            version(Path::new("C:/no/such.dll"))
                .unwrap_err()
                .contains("as data")
        );
        // This test's executable declares no Windows 10 support: GetFileVersionInfoW would say 6.2 here.
        let kernel = version(Path::new("C:/Windows/System32/kernel32.dll"))
            .unwrap()
            .unwrap();
        assert!(kernel.starts_with("10.0."), "{kernel}");
    }

    #[test]
    fn this_machine_is_described() {
        let os = os();
        assert!(os.version.starts_with("10.0."), "{os:?}");
        assert!(os.missing.is_empty(), "{os:?}");
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
