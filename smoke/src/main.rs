//! A few Direct3D 11 calls through locum, checked. Run from a folder that holds this program and locum's three DLLs
//! (`cargo xtask smoke` stages one): Windows then loads locum's `d3d11.dll` and `dxgi.dll` for it as it would for a
//! game. The window it renders to is never shown.
//!
//! Checks:
//! - the `d3d11.dll` and `dxgi.dll` in use are locum's, and `locum.dll` is loaded;
//! - object identity: `QueryInterface(IUnknown)`, `GetDevice` and `GetImmediateContext` give back the objects the game
//!   already holds, and the swap chain's device is the game's device;
//! - work recorded on a deferred context reaches the swap chain's back buffer: a clear to a known colour, read back.

use std::process::ExitCode;

use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::UI::WindowsAndMessaging::{CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW};
use windows::core::{IUnknown, Interface, Result, w};

fn module_path(name: windows::core::PCWSTR) -> String {
    let Ok(module) = (unsafe { GetModuleHandleW(name) }) else { return "(not loaded)".into() };
    let mut buf = [0u16; 1024];
    let n = unsafe { GetModuleFileNameW(Some(module), &mut buf) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

struct Checks {
    failed: u32,
}

impl Checks {
    fn check(&mut self, ok: bool, what: &str) {
        println!("{} {what}", if ok { "ok  " } else { "FAIL" });
        if !ok {
            self.failed += 1;
        }
    }
}

fn same<A: Interface, B: Interface>(a: &A, b: &B) -> bool {
    // COM identity: the IUnknown of both.
    let a: IUnknown = a.cast().expect("IUnknown");
    let b: IUnknown = b.cast().expect("IUnknown");
    a.as_raw() == b.as_raw()
}

fn run(checks: &mut Checks) -> Result<()> {
    let factory: IDXGIFactory2 = unsafe { CreateDXGIFactory1() }?;

    let mut device = None;
    let mut immediate = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut immediate),
        )
    }?;
    let device = device.expect("device");
    let immediate = immediate.expect("immediate context");

    let exe_dir = std::env::current_exe().expect("exe").parent().expect("folder").to_path_buf();
    let d3d11 = module_path(w!("d3d11.dll"));
    let dxgi = module_path(w!("dxgi.dll"));
    let locum = module_path(w!("locum.dll"));
    println!("d3d11.dll: {d3d11}\ndxgi.dll:  {dxgi}\nlocum.dll: {locum}");
    let here = |p: &str| std::path::Path::new(p).parent() == Some(exe_dir.as_path());
    checks.check(here(&d3d11) && here(&dxgi) && here(&locum), "the d3d11.dll, dxgi.dll and locum.dll in use are locum's");

    checks.check(same(&device, &device.cast::<ID3D11Device5>()?), "device: QueryInterface(ID3D11Device5) is the same object");
    checks.check(same(&unsafe { immediate.GetDevice() }?, &device), "immediate context: GetDevice is the game's device");
    checks.check(same(&unsafe { device.GetImmediateContext() }?, &immediate), "device: GetImmediateContext is the same context");

    let hwnd: HWND = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("locum smoke"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            64,
            64,
            None,
            None,
            None,
            None,
        )
    }?;
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: 64,
        Height: 64,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        ..Default::default()
    };
    let swapchain = unsafe { factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None) }?;
    checks.check(same(&unsafe { swapchain.GetDevice::<ID3D11Device>() }?, &device), "swap chain: GetDevice is the game's device");

    let back: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0) }?;
    let mut rtv = None;
    unsafe { device.CreateRenderTargetView(&back, None, Some(&mut rtv)) }?;
    let rtv = rtv.expect("render target view");

    // Record the clear on a deferred context and run it on the immediate one.
    let mut deferred = None;
    unsafe { device.CreateDeferredContext(0, Some(&mut deferred)) }?;
    let deferred = deferred.expect("deferred context");
    checks.check(same(&unsafe { deferred.GetDevice() }?, &device), "deferred context: GetDevice is the game's device");
    unsafe { deferred.ClearRenderTargetView(&rtv, &[0.25, 0.5, 0.75, 1.0]) };
    let mut list = None;
    unsafe { deferred.FinishCommandList(false, Some(&mut list)) }?;
    unsafe { immediate.ExecuteCommandList(&list.expect("command list"), false) };

    // Read the cleared pixel back.
    let mut staging_desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { back.GetDesc(&mut staging_desc) };
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    staging_desc.MiscFlags = 0;
    let mut staging = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }?;
    let staging = staging.expect("staging texture");
    unsafe { immediate.CopyResource(&staging, &back) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { immediate.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }?;
    let pixel = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, 4) }.to_vec();
    unsafe { immediate.Unmap(&staging, 0) };
    println!("pixel (B, G, R, A): {pixel:?}");
    let near = |a: u8, b: u8| a.abs_diff(b) <= 1;
    checks.check(
        near(pixel[0], 191) && near(pixel[1], 128) && near(pixel[2], 64) && pixel[3] == 255,
        "a clear recorded on a deferred context reaches the back buffer",
    );

    for _ in 0..3 {
        unsafe { swapchain.Present(0, DXGI_PRESENT(0)) }.ok()?;
    }
    checks.check(true, "three presents");

    drop((swapchain, back, rtv, deferred, staging));
    unsafe { DestroyWindow(hwnd) }?;
    Ok(())
}

fn main() -> ExitCode {
    let mut checks = Checks { failed: 0 };
    if let Err(e) = run(&mut checks) {
        println!("FAIL error: {e}");
        checks.failed += 1;
    }
    if checks.failed == 0 {
        println!("all checks passed");
        ExitCode::SUCCESS
    } else {
        println!("{} check(s) failed", checks.failed);
        ExitCode::FAILURE
    }
}
