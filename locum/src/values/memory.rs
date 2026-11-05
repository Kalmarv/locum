//! The process's own memory, touched only where it is mapped for that use: reads need committed readable pages,
//! writes need committed writable pages that hold no code (no code is patched): pages that do not execute, or a data
//! section of a loaded image (protected executables make their data executable too), and calls need an executable
//! page of a loaded image.

use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_IMAGE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE, PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE,
    PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS, PAGE_PROTECTION_FLAGS, PAGE_READONLY, PAGE_READWRITE,
    PAGE_WRITECOPY, VirtualQuery,
};

use locum_capture::values::expr::Num;
use locum_capture::values::image;
use locum_capture::values::table::{MAX_ARGS, Type};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    Read,
    Write,
    Execute,
}

fn query(address: u64) -> Option<MEMORY_BASIC_INFORMATION> {
    let mut m = MEMORY_BASIC_INFORMATION::default();
    let n = unsafe {
        VirtualQuery(
            Some(address as *const _),
            &mut m,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    (n != 0 && m.State == MEM_COMMIT).then_some(m)
}

fn allows(protect: PAGE_PROTECTION_FLAGS, usage: Use) -> bool {
    if protect.0 & (PAGE_GUARD.0 | PAGE_NOACCESS.0) != 0 {
        return false;
    }
    let p = PAGE_PROTECTION_FLAGS(protect.0 & 0xff);
    match usage {
        Use::Read => [
            PAGE_READONLY,
            PAGE_READWRITE,
            PAGE_WRITECOPY,
            PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE,
            PAGE_EXECUTE_WRITECOPY,
        ]
        .contains(&p),
        Use::Write => [
            PAGE_READWRITE,
            PAGE_WRITECOPY,
            PAGE_EXECUTE_READWRITE,
            PAGE_EXECUTE_WRITECOPY,
        ]
        .contains(&p),
        Use::Execute => [
            PAGE_EXECUTE,
            PAGE_EXECUTE_READ,
            PAGE_EXECUTE_READWRITE,
            PAGE_EXECUTE_WRITECOPY,
        ]
        .contains(&p),
    }
}

/// Whether `len` bytes at `address` are mapped for `usage`, region by region.
fn mapped(address: u64, len: usize, usage: Use) -> Result<(), String> {
    let end = address
        .checked_add(len as u64)
        .ok_or_else(|| format!("{address:#x}: past the end of memory"))?;
    let mut at = address;
    while at < end {
        let m = query(at).ok_or_else(|| format!("{at:#x} is not mapped"))?;
        if !allows(m.Protect, usage) {
            let what = match usage {
                Use::Read => "readable",
                Use::Write => "writable data",
                Use::Execute => "code",
            };
            return Err(format!("{at:#x} is not {what} (protection {:#x})", m.Protect.0));
        }
        if usage == Use::Execute && m.Type != MEM_IMAGE {
            return Err(format!("{at:#x} is not in a loaded image"));
        }
        if usage == Use::Write && allows(m.Protect, Use::Execute) && !image_data(&m, at) {
            return Err(format!(
                "{at:#x} may be code (protection {:#x}, not a data section of a loaded image)",
                m.Protect.0
            ));
        }
        at = m.BaseAddress as u64 + m.RegionSize as u64;
    }
    Ok(())
}

/// Whether `at`, in the region `m`, lies in a section of a loaded image whose header says it does not execute.
fn image_data(m: &MEMORY_BASIC_INFORMATION, at: u64) -> bool {
    if m.Type != MEM_IMAGE {
        return false;
    }
    let base = m.AllocationBase as u64;
    let Ok(sections) = read_bytes(base, 0x1000).and_then(|h| image::sections(&h)) else {
        return false;
    };
    sections
        .iter()
        .find(|s| (base + s.rva as u64..base + s.rva as u64 + s.size as u64).contains(&at))
        .is_some_and(|s| !s.executes())
}

pub fn read_bytes(address: u64, len: usize) -> Result<Vec<u8>, String> {
    mapped(address, len, Use::Read)?;
    let mut out = vec![0u8; len];
    unsafe { core::ptr::copy_nonoverlapping(address as *const u8, out.as_mut_ptr(), len) };
    Ok(out)
}

pub fn pointer(address: u64) -> Result<u64, String> {
    let b = read_bytes(address, 8)?;
    Ok(u64::from_le_bytes(b.try_into().expect("8 bytes")))
}

pub fn read(ty: Type, address: u64) -> Result<Num, String> {
    let b = read_bytes(address, ty.size())?;
    Ok(ty.decode(&b))
}

/// Writes `value` as `ty` at `address` and reads it back.
pub fn write(ty: Type, address: u64, value: Num) -> Result<Num, String> {
    let b = ty.encode(value, &read_bytes(address, ty.size())?)?;
    mapped(address, b.len(), Use::Write)?;
    unsafe { core::ptr::copy_nonoverlapping(b.as_ptr(), address as *mut u8, b.len()) };
    read(ty, address)
}

/// Calls the function at `address` with up to eight integer or pointer arguments (the x64 convention: the first
/// four in registers, the rest on the stack; extra zero arguments are harmless to a function that takes fewer).
/// What it does is the function's business: a function that faults takes the process down, as Cheat Engine's
/// `executeCodeEx` would.
pub fn call(address: u64, args: &[u64]) -> Result<u64, String> {
    mapped(address, 1, Use::Execute)?;
    let mut a = [0u64; MAX_ARGS];
    a[..args.len()].copy_from_slice(args);
    type Function = unsafe extern "system" fn(u64, u64, u64, u64, u64, u64, u64, u64) -> u64;
    let f: Function = unsafe { core::mem::transmute::<usize, Function>(address as usize) };
    Ok(unsafe { f(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]) })
}

/// The executable's image: its address and its headers (which lie in its first page).
fn exe() -> Result<(u64, Vec<u8>), String> {
    let module = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?.0 as u64;
    Ok((module, read_bytes(module, 0x1000)?))
}

/// The executable's image as its address and length.
pub fn exe_image() -> Result<(u64, u64), String> {
    let (module, headers) = exe()?;
    Ok((module, image::size(&headers)? as u64))
}

/// The executable's code: every section of its image that executes, as mapped (the image stays loaded for the
/// process's life).
pub fn code() -> Result<Vec<(u64, &'static [u8])>, String> {
    let (module, headers) = exe()?;
    let sections = image::sections(&headers)?;
    let mut out = Vec::new();
    for s in sections.iter().filter(|s| s.executes()) {
        let start = module + s.rva as u64;
        mapped(start, s.size as usize, Use::Read)?;
        out.push((start, unsafe {
            core::slice::from_raw_parts(start as *const u8, s.size as usize)
        }));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_memory_reads_and_unmapped_memory_refuses() {
        let x: u64 = 0x1234_5678_9abc_def0;
        assert_eq!(pointer(&x as *const u64 as u64).unwrap(), x);
        assert!(pointer(0x10).is_err());
        let code = code().unwrap();
        assert!(!code.is_empty());
        // data is not code, and code is not writable data
        assert!(call(&x as *const u64 as u64, &[]).is_err());
        assert!(write(Type::U8, code[0].0, Num::Int(0)).is_err());
    }

    #[test]
    fn executable_memory_outside_an_image_is_not_written() {
        use windows::Win32::System::Memory::{
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, VirtualAlloc, VirtualFree,
        };
        let p =
            unsafe { VirtualAlloc(None, 0x1000, MEM_COMMIT | MEM_RESERVE, PAGE_EXECUTE_READWRITE) } as u64;
        assert_ne!(p, 0);
        let e = write(Type::U8, p, Num::Int(1)).unwrap_err();
        assert!(e.contains("may be code"), "{e}");
        // a bit is written over what is there
        static mut FLAGS: u8 = 0b100;
        let flags = &raw mut FLAGS as u64;
        assert_eq!(write(Type::Bit(0), flags, Num::Int(1)), Ok(Num::Int(1)));
        assert_eq!(read(Type::U8, flags), Ok(Num::Int(0b101)));
        unsafe { VirtualFree(p as *mut _, 0, MEM_RELEASE) }.unwrap();
    }
}
