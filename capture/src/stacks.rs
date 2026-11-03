//! Call stacks and the process's modules, for tying a call back to the code that made it.

use core::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::Diagnostics::Debug::RtlCaptureStackBackTrace;
use windows::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleHandleExW,
};
use windows::Win32::System::ProcessStatus::{
    EnumProcessModules, GetModuleFileNameExW, GetModuleInformation, MODULEINFO,
};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::PCWSTR;

use crate::db::Module;

/// The address range of the module this code is in (locum's own), whose frames stacks leave out.
fn own_range() -> (u64, u64) {
    static RANGE: OnceLock<(u64, u64)> = OnceLock::new();
    *RANGE.get_or_init(|| {
        let mut module = HMODULE::default();
        let flags = GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        let here = own_range as *const c_void;
        if unsafe { GetModuleHandleExW(flags, PCWSTR(here.cast()), &mut module) }.is_err() {
            return (0, 0);
        }
        let mut info = MODULEINFO::default();
        let ok = unsafe {
            GetModuleInformation(
                GetCurrentProcess(),
                module,
                &mut info,
                size_of::<MODULEINFO>() as u32,
            )
        };
        if ok.is_err() {
            return (0, 0);
        }
        let base = info.lpBaseOfDll as u64;
        (base, base + u64::from(info.SizeOfImage))
    })
}

/// The return addresses of the calling thread's stack, innermost first, without locum's own frames: the first
/// address is in the code that called into locum.
pub fn capture() -> Vec<u64> {
    let mut frames = [core::ptr::null_mut::<c_void>(); 48];
    let n = unsafe { RtlCaptureStackBackTrace(1, &mut frames, None) } as usize;
    let (lo, hi) = own_range();
    frames[..n]
        .iter()
        .map(|&f| f as u64)
        .filter(|&f| !(lo..hi).contains(&f))
        .collect()
}

/// Every module loaded in the process.
pub fn modules() -> Vec<Module> {
    let process = unsafe { GetCurrentProcess() };
    let mut handles = vec![HMODULE::default(); 1024];
    let mut needed = 0u32;
    loop {
        let bytes = (handles.len() * size_of::<HMODULE>()) as u32;
        if unsafe { EnumProcessModules(process, handles.as_mut_ptr(), bytes, &mut needed) }.is_err() {
            return Vec::new();
        }
        let count = needed as usize / size_of::<HMODULE>();
        if count <= handles.len() {
            handles.truncate(count);
            break;
        }
        handles.resize(count, HMODULE::default());
    }
    handles
        .iter()
        .enumerate()
        .filter_map(|(i, &m)| {
            let mut info = MODULEINFO::default();
            unsafe { GetModuleInformation(process, m, &mut info, size_of::<MODULEINFO>() as u32) }.ok()?;
            let mut name = [0u16; 1024];
            let len = unsafe { GetModuleFileNameExW(Some(process), Some(m), &mut name) } as usize;
            Some(Module {
                id: i as u64,
                path: String::from_utf16_lossy(&name[..len]),
                base: info.lpBaseOfDll as u64,
                size: u64::from(info.SizeOfImage),
            })
        })
        .collect()
}

/// Gives each distinct stack a number, for the `stacks` table.
#[derive(Default)]
pub struct Interner {
    ids: std::collections::HashMap<Vec<u64>, u64>,
}

impl Interner {
    pub fn id(&mut self, frames: &[u64]) -> Option<u64> {
        if frames.is_empty() {
            return None;
        }
        let next = self.ids.len() as u64 + 1;
        Some(*self.ids.entry(frames.to_vec()).or_insert(next))
    }

    pub fn into_rows(self) -> Vec<(u64, Vec<u64>)> {
        let mut rows: Vec<(u64, Vec<u64>)> = self.ids.into_iter().map(|(f, id)| (id, f)).collect();
        rows.sort_by_key(|r| r.0);
        rows
    }
}
