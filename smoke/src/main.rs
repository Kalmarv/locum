//! A few Direct3D 11 calls through locum, checked. Run from a folder that holds this program and locum's three DLLs
//! (`cargo xtask smoke` stages one): Windows then loads locum's `d3d11.dll` and `dxgi.dll` for it as it would for a
//! game. The window it renders to is never shown.
//!
//! Checks:
//! - the `d3d11.dll` and `dxgi.dll` in use are locum's, and `locum.dll` is loaded;
//! - object identity: `QueryInterface(IUnknown)`, `GetDevice` and `GetImmediateContext` give back the objects the game
//!   already holds, and the swap chain's device is the game's device;
//! - the way from the device to a swap chain that games use (device -> IDXGIDevice -> GetAdapter -> GetParent, the
//!   factory -> CreateSwapChainForHwnd) stays on locum's objects, and an adapter a factory enumerates has that factory
//!   as its parent;
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
    // The swap chain is made the way games usually do it: from the factory that made the device's adapter.
    let dxgi_device: IDXGIDevice = device.cast()?;
    checks.check(
        same(&dxgi_device, &device),
        "device: QueryInterface(IDXGIDevice) is the same object",
    );
    let adapter = unsafe { dxgi_device.GetAdapter() }?;
    checks.check(
        same(&unsafe { dxgi_device.GetParent::<IDXGIAdapter>() }?, &adapter),
        "IDXGIDevice: GetParent is the adapter GetAdapter gives",
    );
    let device_factory: IDXGIFactory2 = unsafe { adapter.GetParent() }?;
    let enumerated = unsafe { factory.EnumAdapters(0) }?;
    checks.check(
        same(&unsafe { enumerated.GetParent::<IDXGIFactory2>() }?, &factory),
        "factory: an adapter it enumerates has it as parent",
    );
    let swapchain = unsafe { device_factory.CreateSwapChainForHwnd(&device, hwnd, &desc, None, None) }?;
    checks.check(
        same(&unsafe { swapchain.GetDevice::<ID3D11Device>() }?, &device),
        "swap chain (made from the device's adapter's factory): GetDevice is the game's device",
    );
    checks.check(
        same(
            &unsafe { swapchain.GetParent::<IDXGIFactory2>() }?,
            &device_factory,
        ),
        "swap chain: GetParent is the factory that made it",
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
    overrides(checks, &device, &immediate, &deferred, &swapchain)?;

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

/// What a command list recorded in `frame` maps into the dynamic vertex buffer the frame's draws read.
fn dynamic_vertex_value(frame: u32) -> f32 {
    frame as f32 * 3.0 + 7.0
}

/// The frame whose command list fills the dynamic vertex buffer last before the capture.
const DYNAMIC_FILLED: u32 = CAPTURED - 5;

/// A 64x64 render target of the swap chain's format, and its view.
fn target(device: &ID3D11Device) -> Result<(ID3D11Texture2D, ID3D11RenderTargetView)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 64,
        Height: 64,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        ..Default::default()
    };
    let (mut texture, mut view) = (None, None);
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut texture))?;
        let texture = texture.as_ref().expect("texture");
        device.CreateRenderTargetView(texture, None, Some(&mut view))?;
    }
    Ok((texture.expect("texture"), view.expect("view")))
}

/// The first pixel of a 64x64 B8G8R8A8 texture (B, G, R, A).
fn first_pixel(
    device: &ID3D11Device,
    immediate: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
) -> Result<[u8; 4]> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    desc.Usage = D3D11_USAGE_STAGING;
    desc.BindFlags = 0;
    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    let mut staging = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }?;
    let staging = staging.expect("staging texture");
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        immediate.CopyResource(&staging, texture);
        immediate.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let pixel = *(mapped.pData as *const [u8; 4]);
        immediate.Unmap(&staging, 0);
        Ok(pixel)
    }
}

/// The smoke test's shaders: a full-screen triangle whose colour is ps b3 / 2000.
struct Shaders {
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    vs_hash: String,
    ps_hash: String,
}

impl Shaders {
    fn new(device: &ID3D11Device) -> Result<Shaders> {
        let vs_code = compile(VS, s!("vs_5_0"))?;
        let ps_code = compile(PS, s!("ps_5_0"))?;
        let (mut vs, mut ps) = (None, None);
        unsafe {
            device.CreateVertexShader(&vs_code, None, Some(&mut vs))?;
            device.CreatePixelShader(&ps_code, None, Some(&mut ps))?;
        }
        Ok(Shaders {
            vs: vs.expect("vertex shader"),
            ps: ps.expect("pixel shader"),
            vs_hash: format!("{:016x}", fnv(&vs_code)),
            ps_hash: format!("{:016x}", fnv(&ps_code)),
        })
    }

    /// Binds the shaders, `target`, a 64x64 viewport and `cb` at ps b3 on `c`.
    fn bind(&self, c: &ID3D11DeviceContext, target: &ID3D11RenderTargetView, cb: &ID3D11Buffer) {
        let viewport = D3D11_VIEWPORT {
            Width: 64.0,
            Height: 64.0,
            MaxDepth: 1.0,
            ..Default::default()
        };
        unsafe {
            c.OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            c.RSSetViewports(Some(&[viewport]));
            c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            c.VSSetShader(&self.vs, None);
            c.PSSetShader(&self.ps, None);
            c.PSSetConstantBuffers(3, Some(&[Some(cb.clone())]));
        }
    }
}

const CS: &str = "RWByteAddressBuffer output : register(u0);
[numthreads(1, 1, 1)] void main() { output.Store(0, 0xc0ffee); }";

/// What the compute shader `CS` writes.
const DISPATCH_VALUE: u32 = 0xc0ffee;

/// Runs `CS` once on `immediate`, leaving the other stages' shaders bound, and returns what it wrote (0 if it did
/// not run).
fn dispatch(device: &ID3D11Device, immediate: &ID3D11DeviceContext) -> Result<u32> {
    let code = compile(CS, s!("cs_5_0"))?;
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 16,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_ALLOW_RAW_VIEWS.0 as u32,
        ..Default::default()
    };
    let zeros = [0u32; 4];
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: zeros.as_ptr().cast(),
        ..Default::default()
    };
    let view_desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R32_TYPELESS,
        ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
        Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D11_BUFFER_UAV {
                FirstElement: 0,
                NumElements: 4,
                Flags: D3D11_BUFFER_UAV_FLAG_RAW.0 as u32,
            },
        },
    };
    let staging_desc = D3D11_BUFFER_DESC {
        ByteWidth: 16,
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    let (mut cs, mut buffer, mut view, mut staging) = (None, None, None, None);
    unsafe {
        device.CreateComputeShader(&code, None, Some(&mut cs))?;
        device.CreateBuffer(&desc, Some(&initial), Some(&mut buffer))?;
        let buffer = buffer.as_ref().expect("buffer");
        device.CreateUnorderedAccessView(buffer, Some(&view_desc), Some(&mut view))?;
        device.CreateBuffer(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.as_ref().expect("staging buffer");
        immediate.CSSetShader(cs.as_ref(), None);
        immediate.CSSetUnorderedAccessViews(0, 1, Some(&view), None);
        immediate.Dispatch(1, 1, 1);
        immediate.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
        immediate.CSSetShader(None, None);
        immediate.CopyResource(staging, buffer);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        immediate.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let written = *(mapped.pData as *const u32);
        immediate.Unmap(staging, 0);
        Ok(written)
    }
}

/// A 16-byte constant buffer: default usage (updated with UpdateSubresource) or dynamic (Map).
fn constant_buffer(device: &ID3D11Device, dynamic: bool) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 16,
        Usage: if dynamic {
            D3D11_USAGE_DYNAMIC
        } else {
            D3D11_USAGE_DEFAULT
        },
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        CPUAccessFlags: if dynamic {
            D3D11_CPU_ACCESS_WRITE.0 as u32
        } else {
            0
        },
        ..Default::default()
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }?;
    Ok(buffer.expect("buffer"))
}

/// A 16-byte dynamic vertex buffer.
fn dynamic_vertex_buffer(device: &ID3D11Device) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 16,
        Usage: D3D11_USAGE_DYNAMIC,
        BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
        CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
        ..Default::default()
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }?;
    Ok(buffer.expect("buffer"))
}

/// Distinct sizes let the database checks identify each contents fixture unambiguously.
fn contents_buffer(device: &ID3D11Device, bytes: u32, staging: bool, value: u8) -> Result<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: bytes,
        Usage: if staging {
            D3D11_USAGE_STAGING
        } else {
            D3D11_USAGE_DEFAULT
        },
        CPUAccessFlags: if staging {
            D3D11_CPU_ACCESS_READ.0 as u32
        } else {
            0
        },
        ..Default::default()
    };
    let data = vec![value; bytes as usize];
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: data.as_ptr().cast(),
        ..Default::default()
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, Some(&initial), Some(&mut buffer)) }?;
    Ok(buffer.expect("contents buffer"))
}

fn read_buffer(c: &ID3D11DeviceContext, buffer: &ID3D11Buffer, size: usize) -> Result<Vec<u8>> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        c.Map(buffer, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let bytes = std::slice::from_raw_parts(mapped.pData.cast::<u8>(), size).to_vec();
        c.Unmap(buffer, 0);
        Ok(bytes)
    }
}

/// Writes `value` four times to a dynamic buffer with Map(WRITE_DISCARD) on `c`.
fn write_dynamic(c: &ID3D11DeviceContext, cb: &ID3D11Buffer, value: f32) -> Result<()> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        c.Map(cb, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
        std::ptr::copy_nonoverlapping([value; 4].as_ptr(), mapped.pData.cast(), 4);
        c.Unmap(cb, 0);
    }
    Ok(())
}

/// The 8-bit value a channel holds after a draw with `value` in ps b3.
fn channel(value: f32) -> u8 {
    (value / 2000.0 * 255.0).round() as u8
}

/// A texture made with initial data whose rows are padded (5x3 B8G8R8A8, three mips, row pitch 32), so the hash taken
/// at creation and the one of its contents read back agree only if both are packed the same way.
fn uploaded_texture(device: &ID3D11Device) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView, Vec<u8>)> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 5,
        Height: 3,
        MipLevels: 3,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    // Each mip: rows of 32 bytes, the pixels then 0xEE padding; the tight bytes are what the blob should hold.
    let mips = [(5usize, 3usize), (2, 1), (1, 1)];
    let mut padded: Vec<Vec<u8>> = Vec::new();
    let mut tight = Vec::new();
    for (m, &(w, h)) in mips.iter().enumerate() {
        let mut data = vec![0xEEu8; 32 * h];
        for y in 0..h {
            for x in 0..w * 4 {
                let b = (m * 64 + y * 16 + x) as u8;
                data[y * 32 + x] = b;
                tight.push(b);
            }
        }
        padded.push(data);
    }
    let initial: Vec<D3D11_SUBRESOURCE_DATA> = padded
        .iter()
        .map(|d| D3D11_SUBRESOURCE_DATA {
            pSysMem: d.as_ptr().cast(),
            SysMemPitch: 32,
            SysMemSlicePitch: d.len() as u32,
        })
        .collect();
    let (mut texture, mut view) = (None, None);
    unsafe {
        device.CreateTexture2D(&desc, Some(initial.as_ptr()), Some(&mut texture))?;
        device.CreateShaderResourceView(texture.as_ref().expect("texture"), None, Some(&mut view))?;
    }
    Ok((texture.expect("texture"), view.expect("view"), tight))
}

/// An immutable 48-byte vertex buffer.
fn immutable_buffer(device: &ID3D11Device) -> Result<(ID3D11Buffer, Vec<u8>)> {
    let bytes: Vec<u8> = (0..48u8).map(|b| b.wrapping_mul(7)).collect();
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 48,
        Usage: D3D11_USAGE_IMMUTABLE,
        BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
        ..Default::default()
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: bytes.as_ptr().cast(),
        ..Default::default()
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, Some(&initial), Some(&mut buffer)) }?;
    Ok((buffer.expect("buffer"), bytes))
}

/// A resource's id, the key of its contents, where they came from, and the key of its upload.
type ResourceRow = (i64, Option<String>, Option<String>, Option<String>);

/// `value` four times, as bytes.
fn cb_bytes(value: f32) -> Vec<u8> {
    value.to_le_bytes().repeat(4)
}

/// Waits up to ten seconds for the writer thread to finish `path`.
fn wait_for(path: &std::path::Path) -> bool {
    for _ in 0..100 {
        if path.is_file() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    false
}

/// Each frame: a draw on the immediate context to target A (with a texture and a vertex buffer bound that were made
/// with initial data), then the command list recorded the frame before, which maps its constant buffer, draws to the
/// back buffer, switches to target B and draws again. A capture of one frame is requested and its `capture.db` and
/// blobs checked: the three draws in order with their shaders, constants, viewports and targets; the update and the
/// map with the bytes they wrote; every resource the frame used with its contents as the frame began; and the hash of
/// a texture's initial data equal to that of its contents read back.
fn capture(
    checks: &mut Checks,
    device: &ID3D11Device,
    immediate: &ID3D11DeviceContext,
    deferred: &ID3D11DeviceContext,
    swapchain: &IDXGISwapChain1,
    back: &ID3D11RenderTargetView,
) -> Result<()> {
    let Some(dir) = std::env::var_os("LOCUM_DIR").map(std::path::PathBuf::from) else {
        checks.check(
            false,
            "LOCUM_DIR is set (the capture is requested and read there)",
        );
        return Ok(());
    };
    let sh = Shaders::new(device)?;
    let shaders = dir.join("shaders");
    checks.check(
        shaders.join(format!("{}.vs.dxbc", sh.vs_hash)).is_file()
            && shaders.join(format!("{}.ps.dxbc", sh.ps_hash)).is_file(),
        "shader bytecode written to shaders/<hash>.<stage>.dxbc",
    );
    let immediate_cb = constant_buffer(device, false)?;
    let deferred_cb = constant_buffer(device, true)?;
    let (_a, target_a) = target(device)?;
    let (_b, target_b) = target(device)?;
    let (_texture, texture_view, texture_bytes) = uploaded_texture(device)?;
    let (vertices, vertex_bytes) = immutable_buffer(device)?;
    let dynamic_vertices = dynamic_vertex_buffer(device)?;
    let repeated = contents_buffer(device, 80, false, 0x11)?;
    let first_in_list = contents_buffer(device, 96, false, 0x12)?;
    let staging_read = contents_buffer(device, 112, true, 0x13)?;
    let staging_source = contents_buffer(device, 112, false, 0x34)?;
    // This staging buffer's ONLY captured touch is Map(READ), to catch a copy issued after real Map.
    let read_only = contents_buffer(device, 128, true, 0x14)?;
    let read_only_source = contents_buffer(device, 128, false, 0x45)?;
    let written_between = contents_buffer(device, 144, false, 0x15)?;
    // The texture as files for locum-cli's index: whole, and from its second mip on (as a game that skips its top
    // mip would have it).
    let texture_file = |mips: u32, first: usize| {
        let t = locum_capture::dds::Texture {
            format: DXGI_FORMAT_B8G8R8A8_UNORM.0 as u32,
            dimensions: 2,
            width: 5 >> first,
            height: (3 >> first).max(1),
            depth: 1,
            mips,
            array: 1,
            cube: false,
        };
        locum_capture::dds::file(&t, &texture_bytes[[0, 60][first]..])
    };
    for (folder, bytes) in [("whole", texture_file(3, 0)), ("from-mip-1", texture_file(2, 1))] {
        let d = dir.join("files").join(folder);
        std::fs::create_dir_all(&d).expect("make the files folder");
        std::fs::write(d.join("texture.dds"), bytes).expect("write the texture's file");
    }

    let _ = std::fs::remove_dir_all(dir.join("captures").join(format!("frame-{CAPTURED}")));
    std::fs::write(dir.join("capture"), b"").expect("write the capture request");
    let mut next_list: Option<ID3D11CommandList> = None;
    for frame in 1..=CAPTURED + 2 {
        unsafe {
            // Record the never-executed write BEFORE the captured frame reads this resource.
            if frame == DYNAMIC_FILLED || frame == CAPTURED {
                write_dynamic(deferred, &dynamic_vertices, dynamic_vertex_value(frame))?;
                let mut fill = None;
                deferred.FinishCommandList(false, Some(&mut fill))?;
                if frame == DYNAMIC_FILLED {
                    immediate.ExecuteCommandList(fill.as_ref(), false);
                }
            }
            let value = [immediate_value(frame); 4];
            immediate.UpdateSubresource(&immediate_cb, 0, None, value.as_ptr().cast(), 0, 0);
            sh.bind(immediate, &target_a, &immediate_cb);
            immediate.PSSetShaderResources(0, Some(&[Some(texture_view.clone())]));
            immediate.IASetVertexBuffers(0, 1, Some(&Some(vertices.clone())), Some(&16), Some(&0));
            immediate.Draw(3, 0);
            immediate.PSSetShaderResources(0, Some(&[None]));
            immediate.IASetVertexBuffers(0, 1, Some(&None), Some(&0), Some(&0));
            if let Some(list) = next_list.take() {
                immediate.ExecuteCommandList(&list, false);
            }

            if frame == DYNAMIC_FILLED {
                // Frame-start contents that differ from the uploads, written well before the capture is requested,
                // so nothing about the frames just before it explains them.
                immediate.UpdateSubresource(&repeated, 0, None, [0x21u8; 80].as_ptr().cast(), 0, 0);
                immediate.UpdateSubresource(&first_in_list, 0, None, [0x22u8; 96].as_ptr().cast(), 0, 0);
                immediate.CopyResource(&read_only, &read_only_source);
                immediate.UpdateSubresource(&written_between, 0, None, [0x23u8; 144].as_ptr().cast(), 0, 0);
            }
            if frame == CAPTURED {
                deferred.UpdateSubresource(&repeated, 0, None, [0x71u8; 80].as_ptr().cast(), 0, 0);
                let mut twice = None;
                deferred.FinishCommandList(false, Some(&mut twice))?;
                immediate.ExecuteCommandList(twice.as_ref(), false);
                // A different value between executions catches replacing the initial copy on the second use.
                immediate.UpdateSubresource(&repeated, 0, None, [0x61u8; 80].as_ptr().cast(), 0, 0);
                immediate.ExecuteCommandList(twice.as_ref(), false);

                deferred.UpdateSubresource(&first_in_list, 0, None, [0x72u8; 96].as_ptr().cast(), 0, 0);
                let mut first = None;
                deferred.FinishCommandList(false, Some(&mut first))?;
                immediate.ExecuteCommandList(first.as_ref(), false);

                // A list recorded first, then the immediate context writes the resource, then the list runs: the
                // timeline's first touch is the immediate write, before the list.
                deferred.UpdateSubresource(&written_between, 0, None, [0x73u8; 144].as_ptr().cast(), 0, 0);
                let mut later = None;
                deferred.FinishCommandList(false, Some(&mut later))?;
                immediate.UpdateSubresource(&written_between, 0, None, [0x63u8; 144].as_ptr().cast(), 0, 0);
                immediate.ExecuteCommandList(later.as_ref(), false);

                immediate.CopyResource(&staging_read, &staging_source);
                checks.check(
                    read_buffer(immediate, &staging_read, 112)? == vec![0x34; 112],
                    "staging READ sees the GPU copy made in the captured frame",
                );
                checks.check(
                    read_buffer(immediate, &read_only, 128)? == vec![0x45; 128],
                    "staging first-touch READ sees the GPU copy made before the frame",
                );
            }

            // The next frame's command list.
            write_dynamic(deferred, &deferred_cb, deferred_value(frame + 1))?;
            sh.bind(deferred, back, &deferred_cb);
            deferred.IASetVertexBuffers(0, 1, Some(&Some(dynamic_vertices.clone())), Some(&16), Some(&0));
            deferred.Draw(3, 0);
            deferred.OMSetRenderTargets(Some(&[Some(target_b.clone())]), None);
            deferred.Draw(3, 0);
            let mut list = None;
            deferred.FinishCommandList(false, Some(&mut list))?;
            next_list = list;

            swapchain.Present(0, DXGI_PRESENT(0)).ok()?;
        }
    }

    for (buffer, size, value) in [(&repeated, 80, 0x71), (&first_in_list, 96, 0x72)] {
        let readback = contents_buffer(device, size, true, 0)?;
        unsafe { immediate.CopyResource(&readback, buffer) };
        checks.check(
            read_buffer(immediate, &readback, size as usize)? == vec![value; size as usize],
            &format!("contents fixture {size}: command-list write reached the real GPU buffer"),
        );
    }

    let folder = dir.join("captures").join(format!("frame-{CAPTURED}"));
    let path = folder.join("capture.db");
    if !wait_for(&path) {
        checks.check(false, &format!("capture written to {}", path.display()));
        return Ok(());
    }
    checks.check(true, &format!("capture written to {}", path.display()));
    let db = rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open capture.db");
    let meta = |key: &str| -> String {
        db.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .unwrap_or_default()
    };
    let blobs = locum_capture::blobs::Blobs::new(meta("blobs"));
    let blob = |key: Option<String>| key.and_then(|k| blobs.get(&k).ok());
    let counts: serde_json::Value = serde_json::from_str(&meta("counts")).unwrap_or_default();
    println!("counts: {counts}");
    checks.check(
        counts["draws"] == 3
            && counts["lists_spliced"] == 5
            && counts["lists_recorded_before"] == 1
            && counts["lists_unrecorded"] == 0
            && counts["cb_copy_failures"] == 0
            && counts["targets"] == 3
            && counts["contents_missing"] == 0,
        "capture: three draws, five list executions (one recorded the frame before), three targets read back, \
         no used resource without contents",
    );
    let gaps: Vec<(String, String, i64)> = db
        .prepare("SELECT what, detail, count FROM gaps")
        .and_then(|mut q| {
            q.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get(2)?,
                ))
            })?
            .collect()
        })
        .unwrap_or_default();
    checks.check(gaps.is_empty(), &format!("capture: no gaps ({gaps:?})"));

    // The update and the map, with the bytes each wrote.
    let event = |kind: &str| -> Option<(bool, Option<String>)> {
        db.query_row(
            "SELECT deferred, data FROM events WHERE kind = ?1 ORDER BY seq",
            [kind],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
    };
    let update = event("update");
    checks.check(
        update.as_ref().is_some_and(|(d, _)| !d)
            && blob(update.and_then(|u| u.1)) == Some(cb_bytes(immediate_value(CAPTURED))),
        "capture: the UpdateSubresource on the immediate context, with the bytes it wrote",
    );
    let map = event("map");
    checks.check(
        map.as_ref().is_some_and(|(d, _)| *d)
            && blob(map.and_then(|m| m.1)) == Some(cb_bytes(deferred_value(CAPTURED))),
        "capture: the Map(WRITE_DISCARD) in the command list, with the bytes it wrote",
    );

    let expected = [
        (false, immediate_value(CAPTURED), "the immediate draw to target A"),
        (
            true,
            deferred_value(CAPTURED),
            "the command list's draw to the back buffer",
        ),
        (
            true,
            deferred_value(CAPTURED),
            "the command list's draw to target B",
        ),
    ];
    for (i, (deferred, value, which)) in expected.into_iter().enumerate() {
        let row = db.query_row(
            "SELECT d.call, d.args, d.vs, d.ps, e.deferred FROM draws d JOIN events e ON e.seq = d.event \
             WHERE d.n = ?1",
            [i as i64],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, bool>(4)?,
                ))
            },
        );
        let Ok((call, args, vs, ps, is_deferred)) = row else {
            checks.check(false, &format!("draw {i} captured"));
            continue;
        };
        checks.check(
            is_deferred == deferred && call == "Draw" && args == "[3,0]",
            &format!("draw {i} is {which}"),
        );
        checks.check(
            vs.as_deref() == Some(sh.vs_hash.as_str()) && ps.as_deref() == Some(sh.ps_hash.as_str()),
            &format!("draw {i}: its shaders' hashes are those of their bytecode"),
        );
        let cb: Option<Vec<u8>> = db
            .query_row(
                "SELECT c.bytes FROM bindings b JOIN cb_data c ON c.hash = b.content \
                 WHERE b.draw = ?1 AND b.stage = 'ps' AND b.kind = 'cb' AND b.slot = 3",
                [i as i64],
                |r| r.get(0),
            )
            .ok();
        checks.check(
            cb == Some(cb_bytes(value)),
            &format!("draw {i}: ps b3 holds {value} four times, as the draw saw it"),
        );
        let viewport: Option<(f64, f64)> = db
            .query_row(
                "SELECT width, height FROM viewports WHERE draw = ?1 AND i = 0",
                [i as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        checks.check(
            viewport == Some((64.0, 64.0)),
            &format!("draw {i}: its 64x64 viewport"),
        );
        // The whole target (the triangle covers it) holds the draw's colour in every channel.
        let key: Option<String> = db
            .query_row(
                "SELECT blob FROM targets WHERE draw = ?1 AND target = 'rt0'",
                [i as i64],
                |r| r.get(0),
            )
            .ok()
            .flatten();
        let pixel = blob(key).and_then(|b| b.get(..4).map(<[u8]>::to_vec));
        let expected = channel(value);
        println!("draw {i}: first pixel of its target {pixel:?}");
        checks.check(
            pixel.is_some_and(|p| p.iter().all(|&c| c.abs_diff(expected) <= 1)),
            &format!("draw {i}: its target, read back after it, holds {expected} in every channel"),
        );
    }

    // Contents as the frame began: the constant buffer before this frame's update, the texture and vertex buffer as
    // they were made.
    let resource = |sql: &str| -> Option<ResourceRow> {
        db.query_row(sql, [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .ok()
    };
    let cb = resource(
        "SELECT r.id, r.content, r.content_source, r.upload FROM resources r JOIN bindings b ON b.resource = r.id \
         WHERE b.draw = 0 AND b.stage = 'ps' AND b.kind = 'cb' AND b.slot = 3",
    );
    println!("immediate constant buffer: {cb:?}");
    checks.check(
        cb.and_then(|c| blob(c.1)) == Some(cb_bytes(immediate_value(CAPTURED - 1))),
        "contents: the immediate constant buffer as the frame began (the frame before's value)",
    );
    let texture = resource(
        "SELECT r.id, r.content, r.content_source, r.upload FROM resources r JOIN bindings b ON b.resource = r.id \
         WHERE b.draw = 0 AND b.stage = 'ps' AND b.kind = 'srv' AND b.slot = 0",
    );
    println!("texture: {texture:?}");
    checks.check(
        texture.as_ref().is_some_and(|t| {
            t.1.is_some()
                && t.1 == t.3
                && t.2.as_deref() == Some("first-use")
                && blob(t.1.clone()) == Some(texture_bytes)
        }),
        "contents: a texture made with padded rows reads back tightly packed, its key equal to its upload's",
    );
    let subs: Vec<Option<String>> = texture
        .map(|t| {
            db.prepare("SELECT hash FROM subresources WHERE resource = ?1 ORDER BY sub")
                .and_then(|mut q| q.query_map([t.0], |r| r.get(0))?.collect())
                .unwrap_or_default()
        })
        .unwrap_or_default();
    checks.check(
        subs.len() == 3 && subs.iter().all(Option::is_some),
        "contents: each of the texture's three mips has its own hash",
    );
    let vb = resource(
        "SELECT r.id, r.content, r.content_source, r.upload FROM resources r JOIN bindings b ON b.resource = r.id \
         WHERE b.draw = 0 AND b.kind = 'vb' AND b.slot = 0",
    );
    println!("vertex buffer: {vb:?}");
    checks.check(
        vb.is_some_and(|v| v.2.as_deref() == Some("upload") && blob(v.1) == Some(vertex_bytes)),
        "contents: an immutable vertex buffer's come from its upload, without a readback",
    );
    let unread: i64 = db
        .query_row(
            "SELECT count(*) FROM resources WHERE used = 1 AND content IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(i64::MAX);
    checks.check(unread == 0, "contents: every resource the frame used has them");
    let dynamic = resource(
        "SELECT r.id, r.content, r.content_source, r.upload FROM resources r JOIN bindings b ON b.resource = r.id          WHERE b.draw = 1 AND b.kind = 'vb' AND b.slot = 0",
    );
    println!("dynamic vertex buffer: {dynamic:?}");
    checks.check(
        dynamic.is_some_and(|v| {
            v.2.as_deref() == Some("first-use") && blob(v.1) == Some(cb_bytes(dynamic_vertex_value(DYNAMIC_FILLED)))
        }),
        "contents: a dynamic vertex buffer the frame only read holds what it was last filled with, not what a command list recorded in the frame and never executed maps into it",
    );
    for (size, value, case) in [
        (
            80,
            0x21,
            "list executed twice retains contents before its first execution",
        ),
        (
            96,
            0x22,
            "first touch inside a list retains contents before execution, not frame end",
        ),
        (
            112,
            0x13,
            "staging copied then mapped READ retains contents before the GPU copy",
        ),
        (
            128,
            0x45,
            "staging first-touch Map READ copies before the CPU map",
        ),
        (
            144,
            0x23,
            "a resource written on the immediate context between a list's recording and its execution keeps its contents before that write",
        ),
    ] {
        let sql = format!(
            "SELECT id, content, content_source, upload FROM resources WHERE kind = 'buffer' AND bytes = {size} AND usage = {}",
            if size == 112 || size == 128 { 3 } else { 0 }
        );
        let row = resource(&sql);
        println!("contents fixture {size}: {row:?}");
        if let Some((id, _, _, _)) = &row {
            let (expected_events, kind) = match size {
                80 => (2, "update"),
                96 | 144 => (1, "update"),
                _ => (1, "map_read"),
            };
            let events: i64 = db.query_row(
                "SELECT count(*) FROM events WHERE kind = ?1 AND \
                 CASE WHEN kind = 'update' THEN json_extract(args, '$.dst') ELSE json_extract(args, '$.resource') END = ?2 \
                 AND (kind != 'update' OR deferred = 1)",
                rusqlite::params![kind, id], |r| r.get(0),
            ).unwrap_or(-1);
            checks.check(
                events == expected_events,
                &format!("contents fixture {size}: {expected_events} expected timeline event(s)"),
            );
        }
        checks.check(
            row.is_some_and(|r| r.2.as_deref() == Some("first-use") && blob(r.1) == Some(vec![value; size])),
            case,
        );
    }
    let shader_blobs: Vec<Option<String>> = db
        .prepare("SELECT blob FROM shaders")
        .and_then(|mut q| q.query_map([], |r| r.get(0))?.collect())
        .unwrap_or_default();
    checks.check(
        shader_blobs.len() == 2 && shader_blobs.into_iter().all(|k| blob(k).is_some()),
        "capture: both shaders' bytecode in the blob store",
    );
    Ok(())
}

/// Writes `overrides.txt` (or removes it, for None) and presents 31 frames, so locum reads it again.
fn set_overrides(dir: &std::path::Path, swapchain: &IDXGISwapChain1, rules: Option<&str>) -> Result<()> {
    let path = dir.join("overrides.txt");
    match rules {
        Some(text) => std::fs::write(&path, text).expect("write overrides.txt"),
        None => std::fs::remove_file(&path).expect("remove overrides.txt"),
    }
    for _ in 0..31 {
        unsafe { swapchain.Present(0, DXGI_PRESENT(0)) }.ok()?;
    }
    Ok(())
}

/// Overrides: a constant changed for a draw on the immediate context and for one in a command list, a solid colour,
/// a skipped draw, and drawing as before once the file is gone; the game's own bindings are back after each draw.
fn overrides(
    checks: &mut Checks,
    device: &ID3D11Device,
    immediate: &ID3D11DeviceContext,
    deferred: &ID3D11DeviceContext,
    swapchain: &IDXGISwapChain1,
) -> Result<()> {
    let Some(dir) = std::env::var_os("LOCUM_DIR").map(std::path::PathBuf::from) else {
        return Ok(());
    };
    let sh = Shaders::new(device)?;
    let immediate_cb = constant_buffer(device, false)?;
    let deferred_cb = constant_buffer(device, true)?;
    let (texture, view) = target(device)?;
    let draw = |clear: [f32; 4]| -> Result<[u8; 4]> {
        unsafe {
            immediate.ClearRenderTargetView(&view, &clear);
            let value = [immediate_value(CAPTURED); 4];
            immediate.UpdateSubresource(&immediate_cb, 0, None, value.as_ptr().cast(), 0, 0);
            sh.bind(immediate, &view, &immediate_cb);
            immediate.Draw(3, 0);
        }
        first_pixel(device, immediate, &texture)
    };
    let normal = channel(immediate_value(CAPTURED));

    set_overrides(
        &dir,
        swapchain,
        Some(&format!(
            "constant ps b3 c0 500 500 500 500 if ps={}\n",
            sh.ps_hash
        )),
    )?;
    let pixel = draw([0.0; 4])?;
    println!("constant override, immediate draw: {pixel:?}");
    checks.check(
        pixel.iter().all(|&c| c.abs_diff(channel(500.0)) <= 1),
        "override: constant on an immediate draw",
    );
    let mut bound: [Option<ID3D11Buffer>; 1] = Default::default();
    unsafe { immediate.PSGetConstantBuffers(3, Some(&mut bound)) };
    checks.check(
        bound[0].as_ref().is_some_and(|b| same(b, &immediate_cb)),
        "override: the game's constant buffer is bound again after the draw",
    );
    unsafe {
        immediate.ClearRenderTargetView(&view, &[0.0; 4]);
        write_dynamic(deferred, &deferred_cb, deferred_value(CAPTURED))?;
        sh.bind(deferred, &view, &deferred_cb);
        deferred.Draw(3, 0);
        let mut list = None;
        deferred.FinishCommandList(false, Some(&mut list))?;
        immediate.ExecuteCommandList(&list.expect("command list"), false);
    }
    let pixel = first_pixel(device, immediate, &texture)?;
    println!("constant override, command-list draw: {pixel:?}");
    checks.check(
        pixel.iter().all(|&c| c.abs_diff(channel(500.0)) <= 1),
        "override: constant on a command-list draw",
    );

    set_overrides(
        &dir,
        swapchain,
        Some(&format!("solid ps={} 1 0 0.5 1\n", sh.ps_hash)),
    )?;
    let pixel = draw([0.0; 4])?;
    println!("solid override: {pixel:?}");
    checks.check(
        pixel[0].abs_diff(128) <= 1 && pixel[1] == 0 && pixel[2] == 255 && pixel[3] == 255,
        "override: solid colour (R 1, G 0, B 0.5)",
    );
    let mut ps = None;
    unsafe { immediate.PSGetShader(&mut ps, None, None) };
    checks.check(
        ps.as_ref().is_some_and(|p| same(p, &sh.ps)),
        "override: the game's pixel shader is bound again after the draw",
    );

    set_overrides(&dir, swapchain, Some(&format!("skip vs={}\n", sh.vs_hash)))?;
    let pixel = draw([0.25, 0.5, 0.75, 1.0])?;
    println!("skip override: {pixel:?}");
    checks.check(
        pixel[0].abs_diff(191) <= 1 && pixel[1].abs_diff(128) <= 1 && pixel[2].abs_diff(64) <= 1,
        "override: skipped draw leaves the clear colour",
    );
    let written = dispatch(device, immediate)?;
    println!("dispatch with the skipped vertex shader still bound: wrote {written:#x}");
    checks.check(
        written == DISPATCH_VALUE,
        "override: a skip rule on a vertex shader leaves dispatches alone",
    );

    set_overrides(&dir, swapchain, None)?;
    let pixel = draw([0.0; 4])?;
    println!("no overrides: {pixel:?}");
    checks.check(
        pixel.iter().all(|&c| c.abs_diff(normal) <= 1),
        "override: with the file gone, draws are as before",
    );
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
