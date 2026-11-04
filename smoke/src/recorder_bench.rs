//! Measures the real deferred-context path. Run the same EXE beside baseline/new DLLs.
use super::*;
use windows::Win32::System::{
    ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
    Threading::GetCurrentProcess,
};
fn private_bytes() -> usize {
    let mut info = PROCESS_MEMORY_COUNTERS_EX::default();
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            (&mut info as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
            size_of_val(&info) as u32,
        )
    }
    .unwrap();
    info.PrivateUsage
}
pub(super) fn run() -> Result<()> {
    let warp = std::env::args().any(|a| a == "--warp");
    let mut device = None;
    unsafe {
        D3D11CreateDevice(
            None,
            if warp {
                windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_WARP
            } else {
                D3D_DRIVER_TYPE_HARDWARE
            },
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )
    }?;
    let device = device.unwrap();
    let mut deferred = None;
    unsafe { device.CreateDeferredContext(0, Some(&mut deferred)) }?;
    let deferred = deferred.unwrap();
    println!(
        "recorder benchmark WARP={warp}; d3d11={}",
        module_path(w!("d3d11.dll"))
    );
    if std::env::args().any(|a| a == "--maps") {
        return maps(&device, &deferred);
    }
    if std::env::args().any(|a| a == "--state") {
        return state(&device, &deferred);
    }
    let cb = constant_buffer(&device, false)?;
    let sh = Shaders::new(&device)?;
    let (_, rtv) = target(&device)?;
    for trial in 0..6 {
        let before = private_bytes();
        let started = std::time::Instant::now();
        let mut lists = Vec::new();
        for _ in 0..100 {
            sh.bind(&deferred, &rtv, &cb);
            for n in 0..500 {
                let data = [n as f32, 0., 0., 2000.];
                unsafe {
                    deferred.UpdateSubresource(&cb, 0, None, data.as_ptr().cast(), 0, 0);
                    deferred.Draw(3, 0)
                };
            }
            let mut list = None;
            unsafe { deferred.FinishCommandList(false, Some(&mut list)) }?;
            lists.push(list.unwrap());
        }
        let elapsed = started.elapsed();
        let after = private_bytes();
        println!(
            "trial {trial}: 100 lists, 50000 draws + 50000 16-byte uploads, {:.3} ms, private bytes delta {}",
            elapsed.as_secs_f64() * 1000.,
            after as i64 - before as i64
        );
        drop(lists);
    }
    Ok(())
}

/// Lists of state-heavy draws, as games record them: per draw, nine calls that bind shader resources, samplers,
/// constant and vertex buffers, blend state, a viewport and a shader. Recording and releasing the lists are timed
/// apart: a list's recording is freed when the application releases it.
fn state(device: &ID3D11Device, deferred: &ID3D11DeviceContext) -> Result<()> {
    let sh = Shaders::new(device)?;
    let (_, rtv) = target(device)?;
    let cb = constant_buffer(device, false)?;
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: 4,
                Height: 4,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut texture),
        )
    }?;
    let texture = texture.unwrap();
    let mut srv = None;
    unsafe { device.CreateShaderResourceView(&texture, None, Some(&mut srv)) }?;
    let srvs: Vec<Option<ID3D11ShaderResourceView>> = vec![srv; 8];
    let mut sampler = None;
    unsafe {
        device.CreateSamplerState(
            &D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ..Default::default()
            },
            Some(&mut sampler),
        )
    }?;
    let samplers = vec![sampler; 4];
    let mut vb = None;
    unsafe {
        device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: 256,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut vb),
        )
    }?;
    let vbs = [vb.clone(), vb];
    let (strides, offsets) = ([16u32, 16], [0u32, 0]);
    let cbs = [Some(cb.clone()), Some(cb.clone())];
    let mut blend = None;
    unsafe { device.CreateBlendState(&D3D11_BLEND_DESC::default(), Some(&mut blend)) }?;
    let blend = blend.unwrap();
    let viewport = D3D11_VIEWPORT {
        Width: 64.0,
        Height: 64.0,
        MaxDepth: 1.0,
        ..Default::default()
    };
    for trial in 0..6 {
        let before = private_bytes();
        let started = std::time::Instant::now();
        let mut lists = Vec::new();
        for _ in 0..100 {
            sh.bind(deferred, &rtv, &cb);
            for _ in 0..500 {
                unsafe {
                    deferred.PSSetShaderResources(0, Some(&srvs));
                    deferred.PSSetSamplers(0, Some(&samplers));
                    deferred.VSSetConstantBuffers(0, Some(&cbs));
                    deferred.PSSetConstantBuffers(0, Some(&cbs));
                    deferred.IASetVertexBuffers(
                        0,
                        2,
                        Some(vbs.as_ptr()),
                        Some(strides.as_ptr()),
                        Some(offsets.as_ptr()),
                    );
                    deferred.OMSetBlendState(&blend, Some(&[1.0; 4]), u32::MAX);
                    deferred.RSSetViewports(Some(&[viewport]));
                    deferred.PSSetShader(&sh.ps, None);
                    deferred.Draw(3, 0);
                }
            }
            let mut list = None;
            unsafe { deferred.FinishCommandList(false, Some(&mut list)) }?;
            lists.push(list.unwrap());
        }
        let recorded = started.elapsed();
        let after = private_bytes();
        let released = std::time::Instant::now();
        drop(lists);
        println!(
            "trial {trial}: 100 lists, 50000 draws with 400000 state calls: recorded in {:.3} ms, released in {:.3} ms, private bytes delta {}",
            recorded.as_secs_f64() * 1000.,
            released.elapsed().as_secs_f64() * 1000.,
            after as i64 - before as i64
        );
    }
    Ok(())
}

fn maps(device: &ID3D11Device, deferred: &ID3D11DeviceContext) -> Result<()> {
    for (name, bytes, count, stride, discard_each) in [
        ("ring-4MiB", 4 << 20, 256u32, 64, false),
        ("ring-16MiB", 16 << 20, 256, 64, false),
        ("ring-16MiB-new-page", 16 << 20, 256, 4096, false),
        ("discard-16MiB", 16 << 20, 32, 64, true),
    ] {
        let mut buffer = None;
        unsafe {
            device.CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: bytes,
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut buffer),
            )
        }?;
        let buffer = buffer.unwrap();
        for trial in 0..4 {
            let before = private_bytes();
            let start = std::time::Instant::now();
            for i in 0..count {
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                unsafe {
                    deferred.Map(
                        &buffer,
                        0,
                        if i == 0 || discard_each {
                            D3D11_MAP_WRITE_DISCARD
                        } else {
                            D3D11_MAP_WRITE_NO_OVERWRITE
                        },
                        0,
                        Some(&mut mapped),
                    )?;
                    std::ptr::write_bytes(
                        mapped.pData.cast::<u8>().add(i as usize * stride),
                        (i % 251 + 1) as u8,
                        64,
                    );
                    deferred.Unmap(&buffer, 0);
                }
            }
            let map_us = start.elapsed().as_secs_f64() * 1e6;
            let mapped_private = private_bytes();
            let finish = std::time::Instant::now();
            let mut list = None;
            unsafe { deferred.FinishCommandList(false, Some(&mut list)) }?;
            let finish_us = finish.elapsed().as_secs_f64() * 1e6;
            let retained_private = private_bytes();
            println!(
                "map benchmark {name} trial {trial}: {count} maps x 64 written bytes, stride {stride}, {:.3} us/map, Finish {:.3} us, private bytes before {before} mapped {mapped_private} retained {retained_private}, delta mapped {} retained {}",
                map_us / f64::from(count),
                finish_us,
                mapped_private as i64 - before as i64,
                retained_private as i64 - before as i64
            );
            drop(list);
        }
    }
    Ok(())
}
