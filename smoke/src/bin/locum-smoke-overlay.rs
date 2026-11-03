//! An overlay that redirects the system `d3d11.dll`'s `D3D11CreateDevice` export to its own code, which calls the
//! `D3D11CreateDevice` it found under the bare name `d3d11.dll`, i.e. locum's (Epic Online Services' overlay does this,
//! and Risk of Rain 2 died of the recursion between the two). The device the game then makes through locum must be
//! made once, without locum's call to the real function coming back into the overlay.
//! Run from a folder that holds it and locum's three DLLs, with `LOCUM_DIR` set (`cargo xtask smoke` does both).

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, ID3D11Device, ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualAlloc,
    VirtualProtect,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::core::{HRESULT, PCWSTR, s, w};

type CreateDevice = unsafe extern "system" fn(
    adapter: *mut core::ffi::c_void,
    driver: D3D_DRIVER_TYPE,
    software: HMODULE,
    flags: D3D11_CREATE_DEVICE_FLAG,
    levels: *const D3D_FEATURE_LEVEL,
    count: u32,
    sdk: u32,
    device: *mut Option<ID3D11Device>,
    level: *mut D3D_FEATURE_LEVEL,
    context: *mut Option<ID3D11DeviceContext>,
) -> HRESULT;

/// What the overlay calls on: the export under the bare name (locum's), and the system's own, to get out of a loop.
static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static SYSTEM: AtomicUsize = AtomicUsize::new(0);
static DEPTH: AtomicU32 = AtomicU32::new(0);
static CALLS: AtomicU32 = AtomicU32::new(0);
static RECURSED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn overlay(
    adapter: *mut core::ffi::c_void,
    driver: D3D_DRIVER_TYPE,
    software: HMODULE,
    flags: D3D11_CREATE_DEVICE_FLAG,
    levels: *const D3D_FEATURE_LEVEL,
    count: u32,
    sdk: u32,
    device: *mut Option<ID3D11Device>,
    level: *mut D3D_FEATURE_LEVEL,
    context: *mut Option<ID3D11DeviceContext>,
) -> HRESULT {
    CALLS.fetch_add(1, Ordering::SeqCst);
    let depth = DEPTH.fetch_add(1, Ordering::SeqCst);
    // The real overlay would recurse until the stack runs out; this one notes it and calls the system's own.
    let next = if depth > 0 {
        RECURSED.store(true, Ordering::SeqCst);
        SYSTEM.load(Ordering::SeqCst)
    } else {
        ORIGINAL.load(Ordering::SeqCst)
    };
    let next: CreateDevice = unsafe { core::mem::transmute(next) };
    let hr = unsafe {
        next(
            adapter, driver, software, flags, levels, count, sdk, device, level, context,
        )
    };
    DEPTH.fetch_sub(1, Ordering::SeqCst);
    hr
}

/// Points `module`'s export `name` at `target` through a jump placed within 4 GB above the module (an export table
/// holds 32-bit offsets). Returns the export's previous offset.
fn redirect_export(module: HMODULE, name: &[u8], target: usize) -> Option<u32> {
    let base = module.0 as usize;
    let read = |o: usize| unsafe { ((base + o) as *const u32).read_unaligned() };
    let pe = read(0x3c) as usize;
    let size = read(pe + 24 + 56) as usize;
    let dir = read(pe + 24 + 112) as usize;
    let (count, functions, names, ordinals) = (
        read(dir + 24) as usize,
        read(dir + 28) as usize,
        read(dir + 32) as usize,
        read(dir + 36) as usize,
    );
    let index = (0..count).find(|&i| {
        let s = unsafe { std::ffi::CStr::from_ptr((base + read(names + 4 * i) as usize) as *const i8) };
        s.to_bytes() == name
    })?;
    let ordinal = unsafe { ((base + ordinals + 2 * index) as *const u16).read_unaligned() } as usize;
    let entry = (base + functions + 4 * ordinal) as *mut u32;
    // A page for the jump, after the module's image.
    let mut stub = core::ptr::null_mut();
    for step in 0..4096usize {
        let at = (base + size + 0xffff + step * 0x10000) & !0xffff;
        stub = unsafe {
            VirtualAlloc(
                Some(at as *const _),
                4096,
                MEM_COMMIT | MEM_RESERVE,
                PAGE_EXECUTE_READWRITE,
            )
        };
        if !stub.is_null() {
            break;
        }
    }
    if stub.is_null() || stub as usize - base > u32::MAX as usize {
        return None;
    }
    // mov rax, target; jmp rax
    let mut code = vec![0x48, 0xb8];
    code.extend_from_slice(&(target as u64).to_le_bytes());
    code.extend_from_slice(&[0xff, 0xe0]);
    unsafe { core::ptr::copy_nonoverlapping(code.as_ptr(), stub.cast::<u8>(), code.len()) };
    let mut old = PAGE_PROTECTION_FLAGS(0);
    unsafe { VirtualProtect(entry.cast(), 4, PAGE_READWRITE, &mut old) }.ok()?;
    let previous = unsafe { entry.read_unaligned() };
    unsafe { entry.write_unaligned((stub as usize - base) as u32) };
    unsafe { VirtualProtect(entry.cast(), 4, old, &mut old) }.ok()?;
    Some(previous)
}

fn main() -> ExitCode {
    let mut failed = 0;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        failed += u32::from(!ok);
    };
    // dxgi.dll first: this loads locum, which loads its own d3d11.dll and then the system's.
    let factory: windows::core::Result<IDXGIFactory1> = unsafe { CreateDXGIFactory1() };
    check(
        factory.is_ok(),
        "overlay: CreateDXGIFactory1 through locum's dxgi.dll",
    );

    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    let path: Vec<u16> = String::from_utf16_lossy(&buf[..n])
        .encode_utf16()
        .chain("\\d3d11.dll".encode_utf16())
        .chain(Some(0))
        .collect();
    let (Ok(system), Ok(ours)) = (unsafe { GetModuleHandleW(PCWSTR(path.as_ptr())) }, unsafe {
        GetModuleHandleW(w!("d3d11.dll"))
    }) else {
        check(
            false,
            "overlay: the system's and locum's d3d11.dll are both loaded",
        );
        return ExitCode::FAILURE;
    };
    check(
        system != ours,
        "overlay: d3d11.dll by bare name is locum's, not the system's",
    );
    let (Some(original), Some(own)) = (unsafe { GetProcAddress(ours, s!("D3D11CreateDevice")) }, unsafe {
        GetProcAddress(system, s!("D3D11CreateDevice"))
    }) else {
        check(false, "overlay: both D3D11CreateDevice exports found");
        return ExitCode::FAILURE;
    };
    ORIGINAL.store(original as usize, Ordering::SeqCst);
    SYSTEM.store(own as usize, Ordering::SeqCst);
    let Some(_) = redirect_export(system, b"D3D11CreateDevice", overlay as *const () as usize) else {
        check(false, "overlay: the system's D3D11CreateDevice export redirected");
        return ExitCode::FAILURE;
    };
    let redirected = unsafe { GetProcAddress(system, s!("D3D11CreateDevice")) }.map(|f| f as usize);
    check(
        redirected.is_some_and(|f| f != own as usize),
        "overlay: the system's export now leads to the overlay",
    );

    // The game makes its device through the d3d11.dll it loaded, locum's.
    let create: CreateDevice = unsafe { core::mem::transmute(original) };
    let (mut device, mut context) = (None, None);
    let made = unsafe {
        create(
            core::ptr::null_mut(),
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            core::ptr::null(),
            0,
            D3D11_SDK_VERSION,
            &mut device,
            core::ptr::null_mut(),
            &mut context,
        )
    };
    check(
        made.is_ok() && device.is_some() && context.is_some(),
        "overlay: the game's device is made",
    );
    check(
        !RECURSED.load(Ordering::SeqCst),
        &format!(
            "overlay: locum's call to the real D3D11CreateDevice did not come back to it through the overlay ({} \
             overlay calls)",
            CALLS.load(Ordering::SeqCst)
        ),
    );
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
