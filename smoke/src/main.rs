//! A few Direct3D 11 calls through locum, checked. Run from a folder that holds this program and locum's three DLLs
//! (`cargo xtask smoke` stages one): Windows then loads locum's `d3d11.dll` and `dxgi.dll` for it as it would for a
//! game. The window it renders to is never shown.
//!
//! Checks:
//! - the `d3d11.dll` and `dxgi.dll` in use are locum's, and `locum.dll` is loaded;
//! - object identity: `QueryInterface(IUnknown)`, `GetDevice` and `GetImmediateContext` give back the objects the game
//!   already holds, and the swap chain's device is the game's device;
//! - work recorded on a deferred context reaches the swap chain's back buffer: a clear to a known colour, read back;
//! - a one-frame capture: a frame with one draw on the immediate context and one from a command list recorded the
//!   frame before (as engines that build the next frame while the GPU draws this one do) gives both draws, in order,
//!   with their shaders' hashes and the constant-buffer contents each draw saw.

use std::process::ExitCode;

use windows::Win32::Foundation::{HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST, ID3DBlob, ID3DInclude,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetModuleHandleW};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPEDWINDOW,
};
use windows::core::{IUnknown, Interface, PCSTR, Result, s, w};

fn module_path(name: windows::core::PCWSTR) -> String {
    let Ok(module) = (unsafe { GetModuleHandleW(name) }) else {
        return "(not loaded)".into();
    };
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

    let exe_dir = std::env::current_exe()
        .expect("exe")
        .parent()
        .expect("folder")
        .to_path_buf();
    let d3d11 = module_path(w!("d3d11.dll"));
    let dxgi = module_path(w!("dxgi.dll"));
    let locum = module_path(w!("locum.dll"));
    println!("d3d11.dll: {d3d11}\ndxgi.dll:  {dxgi}\nlocum.dll: {locum}");
    let here = |p: &str| std::path::Path::new(p).parent() == Some(exe_dir.as_path());
    checks.check(
        here(&d3d11) && here(&dxgi) && here(&locum),
        "the d3d11.dll, dxgi.dll and locum.dll in use are locum's",
    );

    checks.check(
        same(&device, &device.cast::<ID3D11Device5>()?),
        "device: QueryInterface(ID3D11Device5) is the same object",
    );
    checks.check(
        same(&unsafe { immediate.GetDevice() }?, &device),
        "immediate context: GetDevice is the game's device",
    );
    checks.check(
        same(&unsafe { device.GetImmediateContext() }?, &immediate),
        "device: GetImmediateContext is the same context",
    );

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
    checks.check(
        same(&unsafe { swapchain.GetDevice::<ID3D11Device>() }?, &device),
        "swap chain: GetDevice is the game's device",
    );

    let back: ID3D11Texture2D = unsafe { swapchain.GetBuffer(0) }?;
    let mut rtv = None;
    unsafe { device.CreateRenderTargetView(&back, None, Some(&mut rtv)) }?;
    let rtv = rtv.expect("render target view");

    // Record the clear on a deferred context and run it on the immediate one.
    let mut deferred = None;
    unsafe { device.CreateDeferredContext(0, Some(&mut deferred)) }?;
    let deferred = deferred.expect("deferred context");
    checks.check(
        same(&unsafe { deferred.GetDevice() }?, &device),
        "deferred context: GetDevice is the game's device",
    );
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

    capture(checks, &device, &immediate, &deferred, &swapchain, &rtv)?;

    drop((swapchain, back, rtv, deferred, staging));
    unsafe { DestroyWindow(hwnd) }?;
    Ok(())
}

/// FNV-1a 64 from locum's offset basis (its shader and constant-buffer hashes).
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(1_469_598_103_934_665_603u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn compile(source: &str, target: PCSTR) -> Result<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            s!("smoke"),
            None,
            None::<&ID3DInclude>,
            s!("main"),
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            let text = unsafe {
                std::slice::from_raw_parts(errors.GetBufferPointer() as *const u8, errors.GetBufferSize())
            };
            println!("{}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.expect("bytecode");
    Ok(
        unsafe { std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize()) }
            .to_vec(),
    )
}

const VS: &str = "float4 main(uint id : SV_VertexID) : SV_Position {
    float2 p = float2((id << 1) & 2, id & 2);
    return float4(p * float2(2, -2) + float2(-1, 1), 0, 1);
}";

const PS: &str = "cbuffer Frame : register(b3) { float4 value; };
float4 main() : SV_Target { return value / 2000.0; }";

/// The frame whose draws are captured: the request is seen at the 30th present, the next frame is pre-recorded, and
/// the one after it captured.
const CAPTURED: u32 = 32;

/// The constant a frame's draw on the immediate context sees, and the one its command-list draw sees.
fn immediate_value(frame: u32) -> f32 {
    1000.0 + frame as f32
}
fn deferred_value(frame: u32) -> f32 {
    frame as f32
}

/// The hex locum writes for a constant buffer holding `value` four times.
fn cb_hex(value: f32) -> String {
    value
        .to_le_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .repeat(4)
}

/// Draws a triangle on each context every frame, requests a capture and checks what it holds.
fn capture(
    checks: &mut Checks,
    device: &ID3D11Device,
    immediate: &ID3D11DeviceContext,
    deferred: &ID3D11DeviceContext,
    swapchain: &IDXGISwapChain1,
    rtv: &ID3D11RenderTargetView,
) -> Result<()> {
    let Some(dir) = std::env::var_os("LOCUM_DIR").map(std::path::PathBuf::from) else {
        checks.check(
            false,
            "LOCUM_DIR is set (the capture is requested and read there)",
        );
        return Ok(());
    };
    let vs_code = compile(VS, s!("vs_5_0"))?;
    let ps_code = compile(PS, s!("ps_5_0"))?;
    let (mut vs, mut ps) = (None, None);
    unsafe {
        device.CreateVertexShader(&vs_code, None, Some(&mut vs))?;
        device.CreatePixelShader(&ps_code, None, Some(&mut ps))?;
    }
    let (vs, ps) = (vs.expect("vertex shader"), ps.expect("pixel shader"));
    let (vs_hash, ps_hash) = (
        format!("{:016x}", fnv(&vs_code)),
        format!("{:016x}", fnv(&ps_code)),
    );
    let shaders = dir.join("shaders");
    checks.check(
        shaders.join(format!("{vs_hash}.vs.dxbc")).is_file()
            && shaders.join(format!("{ps_hash}.ps.dxbc")).is_file(),
        "shader bytecode written to shaders/<hash>.<stage>.dxbc",
    );

    // The immediate context's constant buffer is updated with UpdateSubresource, the command lists' with a
    // Map(WRITE_DISCARD) on the deferred context.
    let cb = |usage, cpu| -> Result<ID3D11Buffer> {
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: usage,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            CPUAccessFlags: cpu,
            ..Default::default()
        };
        let mut buffer = None;
        unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }?;
        Ok(buffer.expect("buffer"))
    };
    let immediate_cb = cb(D3D11_USAGE_DEFAULT, 0)?;
    let deferred_cb = cb(D3D11_USAGE_DYNAMIC, D3D11_CPU_ACCESS_WRITE.0 as u32)?;
    let viewport = D3D11_VIEWPORT {
        Width: 64.0,
        Height: 64.0,
        MaxDepth: 1.0,
        ..Default::default()
    };
    let bind = |c: &ID3D11DeviceContext, buffer: &ID3D11Buffer| unsafe {
        c.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
        c.RSSetViewports(Some(&[viewport]));
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        c.VSSetShader(&vs, None);
        c.PSSetShader(&ps, None);
        c.PSSetConstantBuffers(3, Some(&[Some(buffer.clone())]));
    };

    std::fs::write(dir.join("capture"), b"").expect("write the capture request");
    let mut next_list: Option<ID3D11CommandList> = None;
    for frame in 1..=CAPTURED + 2 {
        unsafe {
            immediate.UpdateSubresource(
                &immediate_cb,
                0,
                None,
                [immediate_value(frame); 4].as_ptr().cast(),
                0,
                0,
            );
            bind(immediate, &immediate_cb);
            immediate.Draw(3, 0);
            if let Some(list) = next_list.take() {
                immediate.ExecuteCommandList(&list, false);
            }

            // The next frame's command list.
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            deferred.Map(&deferred_cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            std::ptr::copy_nonoverlapping([deferred_value(frame + 1); 4].as_ptr(), mapped.pData.cast(), 4);
            deferred.Unmap(&deferred_cb, 0);
            bind(deferred, &deferred_cb);
            deferred.Draw(3, 0);
            let mut list = None;
            deferred.FinishCommandList(false, Some(&mut list))?;
            next_list = list;

            swapchain.Present(0, DXGI_PRESENT(0)).ok()?;
        }
    }

    let path = dir
        .join("captures")
        .join(format!("frame-{CAPTURED}"))
        .join("draws.jsonl");
    let Ok(text) = std::fs::read_to_string(&path) else {
        checks.check(false, &format!("capture written to {}", path.display()));
        return Ok(());
    };
    checks.check(true, &format!("capture written to {}", path.display()));
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|l| serde_json::from_str(l).expect("a JSON line"))
        .collect();
    let of = |e: &'static str| lines.iter().filter(move |l| l["e"] == e);
    let frame = of("frame").next().expect("a frame line");
    println!("frame line: {frame}");
    checks.check(
        frame["draws"] == 2
            && frame["lists_spliced"] == 1
            && frame["lists_unrecorded"] == 0
            && frame["copy_failures"] == 0,
        "capture: two draws, the command list spliced in, nothing unrecorded, no copy failures",
    );
    let draws: Vec<&serde_json::Value> = of("draw").collect();
    let content = |hash: &serde_json::Value| {
        of("cb")
            .find(|c| &c["hash"] == hash)
            .and_then(|c| c["hex"].as_str())
            .map(String::from)
    };
    for (i, (deferred, value)) in [
        (false, immediate_value(CAPTURED)),
        (true, deferred_value(CAPTURED)),
    ]
    .into_iter()
    .enumerate()
    {
        let Some(d) = draws.get(i) else {
            checks.check(false, &format!("draw {i} captured"));
            continue;
        };
        let which = if deferred {
            "command-list draw"
        } else {
            "immediate draw"
        };
        println!("draw {i}: {d}");
        checks.check(
            d["deferred"] == deferred
                && d["call"] == "Draw"
                && d["args"] == serde_json::json!([3, 0])
                && d["topology"] == 4,
            &format!("draw {i} is the {which}: Draw(3, 0), triangle list"),
        );
        checks.check(
            d["shaders"]["vs"] == vs_hash.as_str() && d["shaders"]["ps"] == ps_hash.as_str(),
            &format!("draw {i}: its shaders' hashes are those of their bytecode"),
        );
        let cb = &d["cbs"][0];
        checks.check(
            d["cbs"].as_array().map(Vec::len) == Some(1)
                && cb["stage"] == "ps"
                && cb["slot"] == 3
                && content(&cb["content"]) == Some(cb_hex(value)),
            &format!("draw {i}: ps b3 holds {value} four times, as the draw saw it"),
        );
        checks.check(
            d["rtvs"].as_array().map(Vec::len) == Some(1),
            &format!("draw {i}: one render target"),
        );
    }
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
