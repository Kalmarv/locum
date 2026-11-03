//! Capture/replay regressions for boxed uploads, constant buffers and UAV counters.

use super::*;
use windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_BUFFER;

pub(super) fn row21(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    deferred: &ID3D11DeviceContext,
    swapchain: &IDXGISwapChain1,
) -> Result<()> {
    unsafe {
        c.ClearState();
        deferred.ClearState();
    }
    let sh = Shaders::new(device)?;
    let cb = constant_buffer(device, false)?;
    let (texture, target) = target(device)?;
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 256,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
        StructureByteStride: 4,
        ..Default::default()
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }?;
    let buffer = buffer.unwrap();
    let mut views = Vec::new();
    for (first, flags, count) in [
        (0, D3D11_BUFFER_UAV_FLAG_APPEND, 0),
        (0, D3D11_BUFFER_UAV_FLAG_COUNTER, 7),
        (32, D3D11_BUFFER_UAV_FLAG_APPEND, 5),
        (32, D3D11_BUFFER_UAV_FLAG_COUNTER, 2),
    ] {
        let desc = D3D11_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D11_UAV_DIMENSION_BUFFER,
            Anonymous: D3D11_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D11_BUFFER_UAV {
                    FirstElement: first,
                    NumElements: 32,
                    Flags: flags.0 as u32,
                },
            },
        };
        let mut view = None;
        unsafe {
            device.CreateUnorderedAccessView(&buffer, Some(&desc), Some(&mut view))?;
            c.CSSetUnorderedAccessViews(0, 1, Some(&view), Some(&count));
            c.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
        }
        views.push(view.unwrap());
    }
    let mut cs = None;
    unsafe {
        device.CreateComputeShader(&compile("AppendStructuredBuffer<float> items : register(u0); [numthreads(1,1,1)] void main() { items.Append(1); }", s!("cs_5_0"))?, None, Some(&mut cs))?;
        c.CSSetShader(cs.as_ref(), None);
        c.CSSetUnorderedAccessViews(0, 1, Some(&Some(views[0].clone())), None);
        c.Dispatch(3, 1, 1);
        c.ClearState();
    }
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 16,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_UINT,
        ViewDimension: D3D_SRV_DIMENSION_BUFFER,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D11_BUFFER_SRV {
                Anonymous1: D3D11_BUFFER_SRV_0 { FirstElement: 0 },
                Anonymous2: D3D11_BUFFER_SRV_1 { NumElements: 4 },
            },
        },
    };
    let (mut counts, mut srv) = (None, None);
    unsafe {
        device.CreateBuffer(&desc, None, Some(&mut counts))?;
        device.CreateShaderResourceView(counts.as_ref().unwrap(), Some(&srv_desc), Some(&mut srv))?;
    }
    let counts = counts.unwrap();
    let mut before = None;
    let dir = begin_with(swapchain, "row21", || {
        unsafe {
            deferred.CopyStructureCount(&counts, 0, &views[3]);
            deferred.FinishCommandList(false, Some(&mut before))?;
            // The deferred list has not run: capture must read 6 at execution, not 2 at recording.
            c.CSSetUnorderedAccessViews(0, 1, Some(&Some(views[3].clone())), Some(&6));
            c.ClearState();
        }
        Ok(())
    })?;
    for (case, expected) in [3u32, 7, 5, 11, 13, 12, 9, 6, 14].into_iter().enumerate() {
        let index = match case {
            1 | 4 => 1,
            2 => 2,
            _ => 0,
        };
        unsafe {
            c.ClearState();
            if case == 3 {
                c.CSSetUnorderedAccessViews(0, 1, Some(&Some(views[0].clone())), Some(&11));
            }
            if case == 7 {
                c.ExecuteCommandList(before.as_ref().unwrap(), false);
            } else if case == 8 {
                c.CSSetUnorderedAccessViews(0, 1, Some(&Some(views[3].clone())), Some(&14));
                c.CopyStructureCount(&counts, 0, &views[3]);
            } else if case == 4 {
                deferred.CSSetUnorderedAccessViews(0, 1, Some(&Some(views[1].clone())), Some(&13));
                deferred.CopyStructureCount(&counts, 0, &views[1]);
                let mut list = None;
                deferred.FinishCommandList(false, Some(&mut list))?;
                c.ExecuteCommandList(list.as_ref().unwrap(), false);
            } else {
                if case == 5 {
                    c.CSSetShader(cs.as_ref(), None);
                    c.CSSetUnorderedAccessViews(0, 1, Some(&Some(views[0].clone())), None);
                    c.Dispatch(1, 1, 1);
                }
                if case == 6 {
                    c.OMSetRenderTargetsAndUnorderedAccessViews(
                        None,
                        None,
                        0,
                        1,
                        Some(&Some(views[0].clone())),
                        Some(&9),
                    );
                }
                c.CopyStructureCount(&counts, 0, &views[index]);
            }
            c.ClearState();
            sh.bind(c, &target, &cb);
            c.PSSetConstantBuffers(3, Some(&[None]));
            let mut ps = None;
            device.CreatePixelShader(&compile(&format!("Buffer<uint> count : register(t0); float4 main() : SV_Target {{ return count[0] == {expected} ? float4(1,0,0,1) : float4(0,0,1,1); }}"), s!("ps_5_0"))?, None, Some(&mut ps))?;
            c.PSSetShader(ps.as_ref(), None);
            c.PSSetShaderResources(0, Some(&[srv.clone()]));
            c.Draw(3, 0);
            c.OMSetRenderTargets(None, None);
        }
        let pixel = first_pixel(device, c, &texture)?;
        checks.check(
            pixel == [0, 0, 255, 255],
            &format!("row21: case {case} hidden count is {expected}"),
        );
    }
    finish(checks, swapchain, &dir, "row21")?;
    let db = rusqlite::Connection::open(dir.join("captures/row21/capture.db")).unwrap();
    let initial: Vec<u32> = db
        .prepare("SELECT count FROM uav_counters ORDER BY count")
        .ok()
        .and_then(|mut q| {
            q.query_map([], |r| r.get(0))
                .ok()?
                .collect::<std::result::Result<Vec<_>, _>>()
                .ok()
        })
        .unwrap_or_default();
    checks.check(
        initial == [3, 5, 6, 7],
        &format!("row21: per-view first-touch counters are [3, 5, 6, 7], got {initial:?}"),
    );
    Ok(())
}

pub(super) fn row18(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swapchain: &IDXGISwapChain1,
) -> Result<()> {
    unsafe { c.ClearState() };
    let sh = Shaders::new(device)?;
    let cb = constant_buffer(device, false)?;
    unsafe { c.UpdateSubresource(&cb, 0, None, [2000.0f32, 0.0, 0.0, 2000.0].as_ptr().cast(), 0, 0) };
    let (texture, view) = target(device)?;
    let c1: ID3D11DeviceContext1 = c.cast()?;
    let (mut vs, mut ps) = (None, None);
    unsafe {
        device.CreateVertexShader(&compile("cbuffer Frame : register(b3) { float4 value; }; struct V { float4 p : SV_Position; float4 color : COLOR; }; V main(uint id : SV_VertexID) { V v; float2 p = float2((id << 1) & 2, id & 2); v.p = float4(p * float2(2,-2) + float2(-1,1),0,1); v.color = value / 2000.0; return v; }", s!("vs_5_0"))?, None, Some(&mut vs))?;
        device.CreatePixelShader(
            &compile(
                "float4 main(float4 p : SV_Position, float4 color : COLOR) : SV_Target { return color; }",
                s!("ps_5_0"),
            )?,
            None,
            Some(&mut ps),
        )?;
    }
    let dir = begin(swapchain, "row18")?;
    for vertex in [false, true] {
        for (first, count) in [(0, 0), (16, 0), (16, 16), (0, 16), (0, 4096)] {
            sh.bind(c, &view, &cb);
            let bound = [Some(cb.clone())];
            unsafe {
                if vertex {
                    c.VSSetShader(vs.as_ref(), None);
                    c.PSSetShader(ps.as_ref(), None);
                    c.PSSetConstantBuffers(3, Some(&[None]));
                    c1.VSSetConstantBuffers1(3, 1, Some(bound.as_ptr()), Some(&first), Some(&count));
                } else {
                    c1.PSSetConstantBuffers1(3, 1, Some(bound.as_ptr()), Some(&first), Some(&count));
                }
                c.Draw(3, 0);
                c.OMSetRenderTargets(None, None);
            }
            let pixel = first_pixel(device, c, &texture)?;
            let expected = if count == 0 || first != 0 {
                [0; 4]
            } else {
                [0, 0, 255, 255]
            };
            let stage = if vertex { "vs" } else { "ps" };
            println!("row18: {stage} first={first} count={count} GPU BGRA={pixel:?}");
            checks.check(
                pixel == expected,
                &format!("row18: {stage} range {first}/{count} reads the expected constants"),
            );
        }
    }
    finish(checks, swapchain, &dir, "row18")
}

pub(super) fn row17(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swapchain: &IDXGISwapChain1,
) -> Result<()> {
    let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS::default();
    unsafe {
        device.CheckFeatureSupport(
            D3D11_FEATURE_D3D11_OPTIONS,
            (&mut options as *mut D3D11_FEATURE_DATA_D3D11_OPTIONS).cast(),
            size_of_val(&options) as u32,
        )
    }?;
    if !options.MapNoOverwriteOnDynamicConstantBuffer.as_bool() {
        println!("skip row17: MapNoOverwriteOnDynamicConstantBuffer is unsupported");
        return Ok(());
    }
    unsafe { c.ClearState() };
    let sh = Shaders::new(device)?;
    let cb = constant_buffer(device, true)?;
    let (texture, view) = target(device)?;
    let initial: Vec<u8> = [0.0f32, 2000.0, 0.0, 0.0]
        .iter()
        .flat_map(|v| v.to_le_bytes())
        .collect();
    map_bytes(c, &cb, D3D11_MAP_WRITE_DISCARD, 0, &initial)?;
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 16,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let srv_desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32G32B32A32_FLOAT,
        ViewDimension: D3D_SRV_DIMENSION_BUFFER,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D11_BUFFER_SRV {
                Anonymous1: D3D11_BUFFER_SRV_0 { FirstElement: 0 },
                Anonymous2: D3D11_BUFFER_SRV_1 { NumElements: 1 },
            },
        },
    };
    let (mut sink, mut srv, mut ps) = (None, None, None);
    unsafe {
        device.CreateBuffer(&desc, None, Some(&mut sink))?;
        device.CreateShaderResourceView(sink.as_ref().unwrap(), Some(&srv_desc), Some(&mut srv))?;
        device.CreatePixelShader(&compile("Buffer<float4> data : register(t0); float4 main() : SV_Target { return all(asuint(data[0]) == uint4(0x44fa0000, 0x44fa0000, 0, 0x44fa0000)) ? float4(1,0,0,1) : float4(0,0,1,1); }", s!("ps_5_0"))?, None, Some(&mut ps))?;
    }
    let dir = begin(swapchain, "row17")?;
    map_bytes(c, &cb, D3D11_MAP_WRITE_NO_OVERWRITE, 0, &2000.0f32.to_le_bytes())?;
    map_bytes(c, &cb, D3D11_MAP_WRITE_NO_OVERWRITE, 12, &2000.0f32.to_le_bytes())?;
    unsafe {
        c.CopyResource(sink.as_ref().unwrap(), &cb);
        sh.bind(c, &view, &cb);
        c.PSSetShader(ps.as_ref(), None);
        c.PSSetConstantBuffers(3, Some(&[None]));
        c.PSSetShaderResources(0, Some(&[srv]));
        c.Draw(3, 0);
        c.OMSetRenderTargets(None, None);
    }
    let pixel = first_pixel(device, c, &texture)?;
    println!("row17: GPU BGRA={pixel:?}");
    checks.check(
        pixel == [0, 0, 255, 255],
        "row17: sparse CB maps preserve other bytes through CopyResource and SRV",
    );
    finish(checks, swapchain, &dir, "row17")
}

fn begin(swapchain: &IDXGISwapChain1, label: &str) -> Result<std::path::PathBuf> {
    begin_with(swapchain, label, || Ok(()))
}

fn begin_with(
    swapchain: &IDXGISwapChain1,
    label: &str,
    before: impl FnOnce() -> Result<()>,
) -> Result<std::path::PathBuf> {
    let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").expect("LOCUM_DIR"));
    let request = dir.join("capture");
    std::fs::write(&request, format!("label {label}\nskip memory\n")).unwrap();
    for _ in 0..31 {
        unsafe { swapchain.Present(0, DXGI_PRESENT(0)) }.ok()?;
        if !request.exists() {
            break;
        }
    }
    assert!(!request.exists(), "capture request consumed");
    before()?;
    unsafe { swapchain.Present(0, DXGI_PRESENT(0)) }.ok()?;
    Ok(dir)
}

fn finish(
    checks: &mut Checks,
    swapchain: &IDXGISwapChain1,
    dir: &std::path::Path,
    label: &str,
) -> Result<()> {
    unsafe { swapchain.Present(0, DXGI_PRESENT(0)) }.ok()?;
    checks.check(
        wait_for(&dir.join(format!("captures/{label}/capture.db"))),
        &format!("{label}: capture written"),
    );
    Ok(())
}

pub(super) fn row16(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    deferred: &ID3D11DeviceContext,
    swapchain: &IDXGISwapChain1,
) -> Result<()> {
    boxed_upload(checks, device, c, deferred, swapchain, ("row16", false, false))?;
    let (mut device, mut c) = (None, None);
    unsafe {
        D3D11CreateDevice(
            None,
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut c),
        )
    }?;
    let (device, c) = (device.unwrap(), c.unwrap());
    let mut deferred = None;
    unsafe { device.CreateDeferredContext(0, Some(&mut deferred)) }?;
    let deferred = deferred.unwrap();
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("STATIC"),
            w!("locum boxed upload"),
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
    let dxgi: IDXGIDevice = device.cast()?;
    let factory: IDXGIFactory2 = unsafe { dxgi.GetAdapter()?.GetParent() }?;
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
    for case in [
        ("row16-warp-raw", false, false),
        ("row16-warp-adjusted", true, false),
        ("row16-warp-modern", false, true),
    ] {
        boxed_upload(checks, &device, &c, &deferred, &swapchain, case)?;
    }
    unsafe {
        c.ClearState();
    }
    drop((swapchain, deferred));
    unsafe { DestroyWindow(hwnd) }?;
    Ok(())
}

fn boxed_upload(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    deferred: &ID3D11DeviceContext,
    swapchain: &IDXGISwapChain1,
    case: (&str, bool, bool),
) -> Result<()> {
    let (label, adjusted, modern) = case;
    unsafe {
        c.ClearState();
        deferred.ClearState();
    }
    let mut threading = D3D11_FEATURE_DATA_THREADING::default();
    unsafe {
        device.CheckFeatureSupport(
            D3D11_FEATURE_THREADING,
            (&mut threading as *mut D3D11_FEATURE_DATA_THREADING).cast(),
            size_of_val(&threading) as u32,
        )
    }?;
    println!(
        "{label}: DriverCommandLists={}",
        threading.DriverCommandLists.as_bool()
    );
    let sh = Shaders::new(device)?;
    let cb = constant_buffer(device, false)?;
    let (texture, view) = target(device)?;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 4,
        Height: 4,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        ..Default::default()
    };
    let initial = [0u8; 64];
    let data = D3D11_SUBRESOURCE_DATA {
        pSysMem: initial.as_ptr().cast(),
        SysMemPitch: 16,
        ..Default::default()
    };
    let (mut source, mut srv, mut ps) = (None, None, None);
    unsafe {
        device.CreateTexture2D(&desc, Some(&data), Some(&mut source))?;
        device.CreateShaderResourceView(source.as_ref().unwrap(), None, Some(&mut srv))?;
        device.CreatePixelShader(&compile("Texture2D<float4> data : register(t0); float4 main(float4 p : SV_Position) : SV_Target { return data.Load(int3((uint2)p.xy / 16, 0)); }", s!("ps_5_0"))?, None, Some(&mut ps))?;
    }
    let dir = begin(swapchain, label)?;
    let region = D3D11_BOX {
        left: 1,
        top: 1,
        front: 0,
        right: 3,
        bottom: 3,
        back: 1,
    };
    // Two packed source rows with padding and extra readable sentinels for the legacy deferred offset bug.
    let mut pixels = [0x7bu8; 160];
    pixels[64..72].copy_from_slice(&[255, 0, 0, 255, 0, 255, 0, 255]);
    pixels[88..96].copy_from_slice(&[0, 0, 255, 255, 255, 255, 255, 255]);
    unsafe {
        let start = if adjusted { 64 - 28 } else { 64 };
        let data = pixels.as_ptr().add(start).cast();
        if modern {
            deferred.cast::<ID3D11DeviceContext1>()?.UpdateSubresource1(
                source.as_ref().unwrap(),
                0,
                Some(&region),
                data,
                24,
                0,
                0,
            );
        } else {
            deferred.UpdateSubresource(source.as_ref().unwrap(), 0, Some(&region), data, 24, 0);
        }
        let mut list = None;
        deferred.FinishCommandList(false, Some(&mut list))?;
        c.ExecuteCommandList(list.as_ref().unwrap(), false);
        sh.bind(c, &view, &cb);
        c.PSSetShader(ps.as_ref(), None);
        c.PSSetConstantBuffers(3, Some(&[None]));
        c.PSSetShaderResources(0, Some(&[srv]));
        c.Draw(3, 0);
        c.OMSetRenderTargets(None, None);
    }
    // Check the actual upload, independent of replay and per-draw constant snapshots.
    let mut staging_desc = desc;
    staging_desc.Usage = D3D11_USAGE_STAGING;
    staging_desc.BindFlags = 0;
    staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
    let mut staging = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }?;
    let staging = staging.unwrap();
    unsafe { c.CopyResource(&staging, source.as_ref().unwrap()) };
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { c.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }?;
    let mut actual = Vec::new();
    for y in 1..3 {
        actual.extend_from_slice(unsafe {
            std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize + 4), 8)
        });
    }
    unsafe { c.Unmap(&staging, 0) };
    let driver_offset = if !modern && !threading.DriverCommandLists.as_bool() {
        28
    } else {
        0
    };
    let start = (if adjusted { 64 - 28 } else { 64 }) + driver_offset;
    let expected = [&pixels[start..start + 8], &pixels[start + 24..start + 32]].concat();
    println!("{label}: GPU rectangle={actual:?}");
    checks.check(
        actual == expected,
        &format!("{label}: GPU source matches the context and driver semantics"),
    );
    finish(checks, swapchain, &dir, label)?;
    let db = rusqlite::Connection::open(dir.join(format!("captures/{label}/capture.db"))).unwrap();
    let key: String = db
        .query_row(
            "SELECT data FROM events WHERE kind='update' AND deferred=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let recorded = locum_capture::blobs::Blobs::new(dir.join("blobs")).get(&key);
    checks.check(
        recorded.ok().as_deref() == Some(actual.as_slice()),
        &format!("{label}: recorded box bytes equal the GPU rectangle"),
    );
    drop(texture);
    Ok(())
}
