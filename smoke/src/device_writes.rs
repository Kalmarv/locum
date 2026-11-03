//! D3D11.3 default texture writes, including boxed row/depth pitches and a nonzero mip/slice.
use super::*;
use replay_rows::{begin, finish};
use windows::Win32::Graphics::Direct3D::{D3D_SRV_DIMENSION_TEXTURE2DARRAY, D3D_SRV_DIMENSION_TEXTURE3D};

pub(super) fn run(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
    driver: &str,
) -> Result<()> {
    let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS2::default();
    unsafe {
        device.CheckFeatureSupport(
            D3D11_FEATURE_D3D11_OPTIONS2,
            (&mut options as *mut D3D11_FEATURE_DATA_D3D11_OPTIONS2).cast(),
            size_of_val(&options) as u32,
        )?;
    }
    println!(
        "device-write {driver}: MapOnDefaultTextures={} TiledResourcesTier={}",
        options.MapOnDefaultTextures.as_bool(),
        options.TiledResourcesTier.0
    );
    if !options.MapOnDefaultTextures.as_bool() {
        println!("skip device-write {driver}: MapOnDefaultTextures unsupported");
        return Ok(());
    }
    let d3: ID3D11Device3 = device.cast()?;
    for (volume, prearmed, first_write) in [
        (false, false, false),
        (true, false, false),
        (false, true, true),
        (false, false, true),
    ] {
        unsafe {
            c.ClearState();
        }
        let label = format!(
            "device-write-{driver}-{}",
            if prearmed {
                "prearmed"
            } else if first_write {
                "map-first"
            } else if volume {
                "3d"
            } else {
                "2d"
            }
        );
        let zeros = vec![0u8; 16 * 16 * 8 * 4];
        let initial = [D3D11_SUBRESOURCE_DATA {
            pSysMem: zeros.as_ptr().cast(),
            SysMemPitch: 64,
            SysMemSlicePitch: 1024,
        }; 4];
        let resource: ID3D11Resource = if volume {
            let desc = D3D11_TEXTURE3D_DESC1 {
                Width: 16,
                Height: 16,
                Depth: 8,
                MipLevels: 2,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                TextureLayout: D3D11_TEXTURE_LAYOUT_UNDEFINED,
                ..Default::default()
            };
            let mut tex = None;
            if let Err(e) = unsafe { d3.CreateTexture3D1(&desc, Some(initial.as_ptr()), Some(&mut tex)) } {
                if e.code() == windows::Win32::Foundation::E_INVALIDARG {
                    println!("skip {label}: CreateTexture3D1 DEFAULT/CPU_WRITE rejected: {e}");
                    continue;
                }
                return Err(e);
            }
            tex.unwrap().cast()?
        } else {
            let desc = D3D11_TEXTURE2D_DESC1 {
                Width: 16,
                Height: 16,
                MipLevels: 2,
                ArraySize: 2,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                TextureLayout: D3D11_TEXTURE_LAYOUT_UNDEFINED,
                ..Default::default()
            };
            let mut tex = None;
            unsafe {
                d3.CreateTexture2D1(&desc, Some(initial.as_ptr()), Some(&mut tex))?;
            }
            tex.unwrap().cast()?
        };
        let desc = if volume {
            D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ViewDimension: D3D_SRV_DIMENSION_TEXTURE3D,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture3D: D3D11_TEX3D_SRV {
                        MostDetailedMip: 1,
                        MipLevels: 1,
                    },
                },
            }
        } else {
            D3D11_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ViewDimension: D3D_SRV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D11_TEX2D_ARRAY_SRV {
                        MostDetailedMip: 1,
                        MipLevels: 1,
                        FirstArraySlice: 1,
                        ArraySize: 1,
                    },
                },
            }
        };
        let (mut srv, mut ps) = (None, None);
        let source = if volume {
            "Texture3D<float4> t:register(t0); float4 main(float4 p:SV_Position):SV_Target { return t.Load(int4(1+(uint)p.x/16,2+(uint)p.y/32,1+(uint)p.y/32,0)); }"
        } else {
            "Texture2DArray<float4> t:register(t0); float4 main(float4 p:SV_Position):SV_Target { return t.Load(int4(1+(uint)p.x/16,2+(uint)p.y/16,0,0)); }"
        };
        unsafe {
            device.CreateShaderResourceView(&resource, Some(&desc), Some(&mut srv))?;
            device.CreatePixelShader(&compile(source, s!("ps_5_0"))?, None, Some(&mut ps))?;
        }
        let sh = Shaders::new(device)?;
        let cb = constant_buffer(device, false)?;
        let (texture, target) = target(device)?;
        if prearmed {
            unsafe {
                c.Map(&resource, 3, D3D11_MAP_WRITE, 0, None)?;
            }
        }
        let dir = begin(swap, &label)?;
        for after in [false, true] {
            if first_write && !after {
                continue;
            }
            if after {
                let sub = if volume { 1 } else { 3 };
                let region = D3D11_BOX {
                    left: 1,
                    top: 2,
                    front: if volume { 1 } else { 0 },
                    right: 5,
                    bottom: 6,
                    back: if volume { 3 } else { 1 },
                };
                let mut data = vec![0x7eu8; 224];
                for z in 0..2 {
                    for y in 0..4 {
                        for x in 0..4 {
                            let n = z * 112 + y * 24 + x * 4;
                            data[n..n + 4].copy_from_slice(if z == 0 {
                                &[255, 0, 0, 255]
                            } else {
                                &[0, 255, 0, 255]
                            });
                        }
                    }
                }
                unsafe {
                    c.PSSetShaderResources(0, Some(&[None]));
                    if !prearmed {
                        c.Map(&resource, sub, D3D11_MAP_WRITE, 0, None)?;
                    }
                    d3.WriteToSubresource(&resource, sub, Some(&region), data.as_ptr().cast(), 24, 112);
                    let empty = D3D11_BOX {
                        left: 5,
                        right: 1,
                        ..region
                    };
                    d3.WriteToSubresource(&resource, sub, Some(&empty), data.as_ptr().cast(), 24, 112);
                    c.Unmap(&resource, sub);
                }
            }
            unsafe {
                sh.bind(c, &target, &cb);
                c.PSSetShader(ps.as_ref(), None);
                c.PSSetShaderResources(0, Some(&[srv.clone()]));
                c.Draw(3, 0);
                c.OMSetRenderTargets(None, None);
            }
        }
        finish(checks, swap, &dir, &label)?;
        checks.check(
            first_pixel(device, c, &texture)? == [0, 0, 255, 255],
            &format!("{label}: GPU observed the boxed write"),
        );
        let db = rusqlite::Connection::open(dir.join(format!("captures/{label}/capture.db"))).unwrap();
        let count:i64=db.query_row("SELECT count(*) FROM events WHERE call='WriteToSubresource' AND kind='update' AND data IS NOT NULL", [], |r|r.get(0)).unwrap();
        checks.check(
            count == 1,
            &format!("{label}: one ordered WriteToSubresource upload"),
        );
        let key: Option<String> = db
            .query_row(
                "SELECT data FROM events WHERE call='WriteToSubresource'",
                [],
                |r| r.get(0),
            )
            .ok();
        let blobs = locum_capture::blobs::Blobs::new(dir.join("blobs"));
        let mut expected = [255, 0, 0, 255].repeat(16);
        if volume {
            expected.extend([0, 255, 0, 255].repeat(16));
        }
        checks.check(
            key.is_some_and(|k| blobs.get(&k).is_ok_and(|b| b == expected)),
            &format!("{label}: upload excludes row/depth padding; empty box did nothing"),
        );
        if prearmed {
            let count: i64 = db
                .query_row("SELECT count(*) FROM gaps WHERE what='default_map'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            checks.check(
                count == 1,
                &format!("{label}: cannot copy initial contents from a texture mapped before capture"),
            );
        }
    }
    unsafe {
        c.ClearState();
    }
    Ok(())
}
