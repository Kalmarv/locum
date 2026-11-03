//! A program that imports only `dxgi.dll` and loads `d3d11.dll` by name later, as a game that delay-loads it does
//! (Unreal Engine 5 games): the `d3d11.dll` it gets must be locum's, and the device it makes must reach locum.
//! Run from a folder that holds it and locum's three DLLs, with `LOCUM_DIR` set (`cargo xtask smoke` does both).

use std::process::ExitCode;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE, D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, ID3D11Device, ID3D11DeviceContext,
};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW};
use windows::core::{HRESULT, s, w};

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

fn main() -> ExitCode {
    let mut failed = 0;
    let mut check = |ok: bool, what: &str| {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        failed += u32::from(!ok);
    };
    // dxgi.dll first, as the game's import table has it: this loads locum, which loads the real DLLs.
    let factory: windows::core::Result<IDXGIFactory1> = unsafe { CreateDXGIFactory1() };
    check(
        factory.is_ok(),
        "late: CreateDXGIFactory1 through locum's dxgi.dll",
    );
    let _adapter: Option<IDXGIAdapter> = factory.ok().and_then(|f| unsafe { f.EnumAdapters(0) }.ok());

    // Then d3d11.dll by name, as the delay-load helper asks for it.
    let module = unsafe { LoadLibraryW(w!("d3d11.dll")) };
    let Ok(module) = module else {
        check(false, "late: d3d11.dll loads by name");
        return ExitCode::FAILURE;
    };
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    let path = std::path::PathBuf::from(String::from_utf16_lossy(&buf[..n]));
    let exe_dir = std::env::current_exe()
        .expect("exe")
        .parent()
        .expect("folder")
        .to_path_buf();
    println!("late: d3d11.dll by name is {}", path.display());
    check(
        path.parent() == Some(exe_dir.as_path()),
        "late: d3d11.dll loaded by name after locum started is locum's, not the system's",
    );
    let Some(create) = (unsafe { GetProcAddress(module, s!("D3D11CreateDevice")) }) else {
        check(false, "late: D3D11CreateDevice found");
        return ExitCode::FAILURE;
    };
    let create: CreateDevice = unsafe { core::mem::transmute(create) };
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
        made.is_ok() && device.is_some(),
        "late: D3D11CreateDevice succeeds",
    );
    let log = std::env::var_os("LOCUM_DIR")
        .map(|d| std::fs::read_to_string(std::path::Path::new(&d).join("locum.log")).unwrap_or_default())
        .unwrap_or_default();
    check(
        log.lines()
            .any(|l| l.contains("D3D11CreateDevice from") && l.contains("locum-smoke-late.exe"))
            && log.contains("wrapped device"),
        "late: locum saw the device made and wrapped it",
    );
    drop((device, context));
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
