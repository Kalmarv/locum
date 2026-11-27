//! Out-of-process, zero-suspend CPU stack symbolization via `dbghelp.dll` + `TH32CS_SNAPMODULE` / ETW `ImageLoad`.
//!
//! Uses a synthetic `dbghelp` session handle (`fInvadeProcess = FALSE`) loaded with module ranges
//! gathered either from `CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid)`
//! (zero thread suspends) or from ETW `ImageLoad` events (works offline after the process exits).

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, FreeLibrary, HANDLE, HMODULE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, MODULEENTRY32W, Module32FirstW, Module32NextW, TH32CS_SNAPMODULE,
    TH32CS_SNAPMODULE32,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::core::{HSTRING, PCSTR, PCWSTR};

/// Loaded module address range for out-of-process / offline symbolization.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleInfo {
    pub base: u64,
    pub size: u64,
    pub name: String,
    pub path: String,
}

/// Captures the loaded module list of `pid` out-of-process with zero thread suspends.
pub fn snapshot_process_modules(pid: u32) -> Result<Vec<ModuleInfo>, String> {
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) }
        .map_err(|e| format!("CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid={pid}): {e}"))?;

    let mut entry = MODULEENTRY32W {
        dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut out = Vec::new();
    if unsafe { Module32FirstW(snap, &mut entry) }.is_ok() {
        loop {
            let name_len = entry
                .szModule
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szModule.len());
            let path_len = entry
                .szExePath
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExePath.len());
            let name = String::from_utf16_lossy(&entry.szModule[..name_len]);
            let path = String::from_utf16_lossy(&entry.szExePath[..path_len]);
            let base = entry.modBaseAddr as u64;
            let size = entry.modBaseSize as u64;
            if base != 0 && size > 0 {
                out.push(ModuleInfo {
                    base,
                    size,
                    name,
                    path,
                });
            }
            if unsafe { Module32NextW(snap, &mut entry) }.is_err() {
                break;
            }
        }
    }
    unsafe {
        let _ = CloseHandle(snap);
    }
    out.sort_by_key(|m| m.base);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Dynamic `dbghelp.dll` bindings (avoids link-time dbghelp.lib dependency)
// ---------------------------------------------------------------------------

const SYMOPT_UNDNAME: u32 = 0x0000_0002;
const SYMOPT_DEFERRED_LOADS: u32 = 0x0000_0004;
const SYMOPT_LOAD_LINES: u32 = 0x0000_0010;
const SYMOPT_FAIL_CRITICAL_ERRORS: u32 = 0x0000_0200;

const MAX_SYM_NAME: usize = 512;

#[repr(C)]
struct SymbolInfoW {
    size_of_struct: u32,
    type_index: u32,
    reserved: [u64; 2],
    index: u32,
    size: u32,
    mod_base: u64,
    flags: u32,
    value: u64,
    address: u64,
    register: u32,
    scope: u32,
    tag: u32,
    name_len: u32,
    max_name_len: u32,
    name: [u16; MAX_SYM_NAME],
}

#[repr(C)]
struct ImageHlpLineW64 {
    size_of_struct: u32,
    key: *mut c_void,
    line_number: u32,
    file_name: *mut u16,
    address: u64,
}

type FnSymSetOptions = unsafe extern "system" fn(u32) -> u32;
type FnSymInitializeW = unsafe extern "system" fn(HANDLE, PCWSTR, i32) -> i32;
type FnSymCleanup = unsafe extern "system" fn(HANDLE) -> i32;
type FnSymLoadModuleExW =
    unsafe extern "system" fn(HANDLE, HANDLE, PCWSTR, PCWSTR, u64, u32, *mut c_void, u32) -> u64;
type FnSymFromAddrW = unsafe extern "system" fn(HANDLE, u64, *mut u64, *mut SymbolInfoW) -> i32;
type FnSymGetLineFromAddrW64 = unsafe extern "system" fn(HANDLE, u64, *mut u32, *mut ImageHlpLineW64) -> i32;

struct DbgHelp {
    dll: HMODULE,
    session: HANDLE,
    sym_cleanup: FnSymCleanup,
    sym_load_module: FnSymLoadModuleExW,
    sym_from_addr: FnSymFromAddrW,
    sym_get_line: Option<FnSymGetLineFromAddrW64>,
}

static NEXT_SESSION_ID: AtomicUsize = AtomicUsize::new(0x4C4F_4301);

impl DbgHelp {
    fn open(search_path: Option<&str>) -> Option<Self> {
        let dll = unsafe { LoadLibraryW(&HSTRING::from("dbghelp.dll")) }.ok()?;
        unsafe fn proc<T>(dll: HMODULE, name: &[u8]) -> Option<T> {
            let p = unsafe { GetProcAddress(dll, PCSTR(name.as_ptr())) }?;
            Some(unsafe { std::mem::transmute_copy::<_, T>(&p) })
        }

        let sym_set_options: FnSymSetOptions = unsafe { proc(dll, b"SymSetOptions\0") }?;
        let sym_initialize: FnSymInitializeW = unsafe { proc(dll, b"SymInitializeW\0") }?;
        let sym_cleanup: FnSymCleanup = unsafe { proc(dll, b"SymCleanup\0") }?;
        let sym_load_module: FnSymLoadModuleExW = unsafe { proc(dll, b"SymLoadModuleExW\0") }?;
        let sym_from_addr: FnSymFromAddrW = unsafe { proc(dll, b"SymFromAddrW\0") }?;
        let sym_get_line: Option<FnSymGetLineFromAddrW64> = unsafe { proc(dll, b"SymGetLineFromAddrW64\0") };

        let id = NEXT_SESSION_ID.fetch_add(4, Ordering::Relaxed);
        let session = HANDLE(id as *mut c_void);
        unsafe {
            sym_set_options(
                SYMOPT_UNDNAME | SYMOPT_DEFERRED_LOADS | SYMOPT_LOAD_LINES | SYMOPT_FAIL_CRITICAL_ERRORS,
            );
        }
        let search_h = search_path.map(HSTRING::from);
        let search_ptr = search_h
            .as_ref()
            .map(|h| PCWSTR(h.as_ptr()))
            .unwrap_or(PCWSTR::null());
        let ok = unsafe { sym_initialize(session, search_ptr, 0) };
        if ok == 0 {
            unsafe {
                let _ = FreeLibrary(dll);
            }
            return None;
        }
        Some(Self {
            dll,
            session,
            sym_cleanup,
            sym_load_module,
            sym_from_addr,
            sym_get_line,
        })
    }

    fn load_module(&self, m: &ModuleInfo) {
        if m.path.is_empty() || m.base == 0 || m.size == 0 {
            return;
        }
        // Skip pseudo NT kernel paths that don't resolve as Win32 paths unless normalized.
        let norm = normalize_module_path(&m.path);
        let img = HSTRING::from(norm.as_str());
        let mod_name = HSTRING::from(m.name.as_str());
        let size32 = m.size.min(u32::MAX as u64) as u32;
        unsafe {
            let _ = (self.sym_load_module)(
                self.session,
                HANDLE::default(),
                PCWSTR(img.as_ptr()),
                PCWSTR(mod_name.as_ptr()),
                m.base,
                size32,
                std::ptr::null_mut(),
                0,
            );
        }
    }

    fn lookup(&self, addr: u64, mod_name: &str) -> Option<String> {
        let mut info = SymbolInfoW {
            size_of_struct: (std::mem::offset_of!(SymbolInfoW, name) + 2) as u32,
            type_index: 0,
            reserved: [0; 2],
            index: 0,
            size: 0,
            mod_base: 0,
            flags: 0,
            value: 0,
            address: 0,
            register: 0,
            scope: 0,
            tag: 0,
            name_len: 0,
            max_name_len: MAX_SYM_NAME as u32,
            name: [0u16; MAX_SYM_NAME],
        };
        let mut disp = 0u64;
        let ok = unsafe { (self.sym_from_addr)(self.session, addr, &mut disp, &mut info) };
        if ok == 0 || info.name_len == 0 {
            return None;
        }
        let len = (info.name_len as usize).min(MAX_SYM_NAME);
        let sym = String::from_utf16_lossy(&info.name[..len]);
        let mut out = if disp > 0 {
            format!("{mod_name}!{sym}+{disp:#x}")
        } else {
            format!("{mod_name}!{sym}")
        };
        if let Some(get_line) = self.sym_get_line {
            let mut line = ImageHlpLineW64 {
                size_of_struct: std::mem::size_of::<ImageHlpLineW64>() as u32,
                key: std::ptr::null_mut(),
                line_number: 0,
                file_name: std::ptr::null_mut(),
                address: 0,
            };
            let mut line_disp = 0u32;
            if unsafe { get_line(self.session, addr, &mut line_disp, &mut line) } != 0
                && !line.file_name.is_null()
            {
                let file = unsafe { read_wide_z(line.file_name, 512) };
                let base_file = Path::new(&file)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&file);
                if !base_file.is_empty() {
                    out.push_str(&format!(" ({base_file}:{})", line.line_number));
                }
            }
        }
        Some(out)
    }
}

impl Drop for DbgHelp {
    fn drop(&mut self) {
        unsafe {
            let _ = (self.sym_cleanup)(self.session);
            let _ = FreeLibrary(self.dll);
        }
    }
}

unsafe fn read_wide_z(ptr: *const u16, max_len: usize) -> String {
    let mut out = Vec::new();
    for i in 0..max_len {
        let ch = unsafe { *ptr.add(i) };
        if ch == 0 {
            break;
        }
        out.push(ch);
    }
    String::from_utf16_lossy(&out)
}

fn normalize_module_path(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\SystemRoot\") {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        format!(r"{root}\{rest}")
    } else if let Some(rest) = path.strip_prefix(r"\??\") {
        rest.to_string()
    } else {
        path.to_string()
    }
}

/// Out-of-process / offline symbolizer backed by `dbghelp.dll` and a module address table.
pub struct Symbolizer {
    modules: Vec<ModuleInfo>,
    known_bases: HashMap<u64, ()>,
    dbghelp_loaded: HashMap<u64, ()>,
    dbghelp: Option<DbgHelp>,
    cache: HashMap<u64, String>,
}

impl Symbolizer {
    /// Creates a new symbolizer with optional PDB search path (uses `_NT_SYMBOL_PATH` + module dirs by default).
    pub fn new(search_path: Option<&str>) -> Self {
        Self {
            modules: Vec::new(),
            known_bases: HashMap::new(),
            dbghelp_loaded: HashMap::new(),
            dbghelp: DbgHelp::open(search_path),
            cache: HashMap::new(),
        }
    }

    /// Creates a symbolizer pre-populated with `pid`'s live modules (zero thread suspends).
    pub fn for_process(pid: u32) -> Self {
        let mut sym = Self::new(None);
        if let Ok(mods) = snapshot_process_modules(pid) {
            for m in mods {
                sym.add_module(m);
            }
        }
        sym
    }

    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

    pub fn dbghelp_available(&self) -> bool {
        self.dbghelp.is_some()
    }

    pub fn add_module(&mut self, m: ModuleInfo) {
        if m.base == 0 || m.size == 0 {
            return;
        }
        if self.known_bases.contains_key(&m.base) {
            return;
        }
        self.known_bases.insert(m.base, ());
        let pos = self.modules.partition_point(|x| x.base < m.base);
        self.modules.insert(pos, m);
    }

    pub fn add_etw_image(&mut self, base: u64, size: u64, path: &str) {
        let name = Path::new(path)
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(path)
            .to_string();
        self.add_module(ModuleInfo {
            base,
            size,
            name,
            path: path.to_string(),
        });
    }

    fn find_module(&self, addr: u64) -> Option<&ModuleInfo> {
        let hi = self.modules.partition_point(|m| m.base <= addr);
        if hi == 0 {
            return None;
        }
        let m = &self.modules[hi - 1];
        if addr < m.base.saturating_add(m.size) {
            Some(m)
        } else {
            None
        }
    }

    /// Resolves a single instruction address to `module!Symbol+0xdisp (file:line)` or `module+0xRVA`.
    pub fn resolve(&mut self, addr: u64) -> String {
        if let Some(hit) = self.cache.get(&addr) {
            return hit.clone();
        }
        let resolved = if let Some(m) = self.find_module(addr).cloned() {
            if let Some(ref dh) = self.dbghelp {
                self.dbghelp_loaded.entry(m.base).or_insert_with(|| {
                    dh.load_module(&m);
                });
                if let Some(sym) = dh.lookup(addr, &m.name) {
                    sym
                } else {
                    format!("{}+{:#x}", m.name, addr - m.base)
                }
            } else {
                format!("{}+{:#x}", m.name, addr - m.base)
            }
        } else if addr >= 0xFFFF_8000_0000_0000 {
            format!("kernel!{addr:#x}")
        } else {
            format!("{addr:#x}")
        };
        self.cache.insert(addr, resolved.clone());
        resolved
    }

    /// Resolves up to `max_frames` addresses into readable frame names.
    pub fn resolve_stack(&mut self, frames: &[u64], max_frames: usize) -> Vec<String> {
        frames
            .iter()
            .copied()
            .filter(|&a| a != 0)
            .take(max_frames)
            .map(|a| self.resolve(a))
            .collect()
    }

    /// Formats a stack trace as a single greppable `frame0 <- frame1 <- frame2` line.
    pub fn format_stack_oneline(&mut self, frames: &[u64], max_frames: usize) -> String {
        self.resolve_stack(frames, max_frames).join(" <- ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_current_process_function_without_suspends() {
        let mut sym = Symbolizer::for_process(std::process::id());
        assert!(
            sym.module_count() > 0,
            "should find loaded modules in current process"
        );
        let addr = resolves_current_process_function_without_suspends as *const () as u64;
        let name = sym.resolve(addr);
        assert!(
            !name.starts_with("0x"),
            "expected module+rva or symbol for current test function, got {name}"
        );
    }

    #[test]
    fn offline_module_fallback_formats_rva_when_no_pdb() {
        let mut sym = Symbolizer::new(None);
        sym.add_etw_image(0x7FF6_1000_0000, 0x200_000, r"C:\Games\xcl.exe");
        assert_eq!(sym.resolve(0x7FF6_1000_1234), "xcl.exe+0x1234");
        assert_eq!(sym.resolve(0xFFFF_F800_0010_0000), "kernel!0xfffff80000100000");
    }
}
