//! Every layer of a view, and outputs with no draw at all.
use super::*;
use replay_rows::{begin, finish};

pub(super) fn run(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
) -> Result<()> {
    let shaders = Shaders::new(device)?;
    let cb = constant_buffer(device, false)?;
    let mut gs = None;
    let mut ps = None;
    unsafe {
        device.CreateGeometryShader(
            &compile(
                r#"
struct V { float4 p : SV_Position; };
struct O { float4 p : SV_Position; uint layer : SV_RenderTargetArrayIndex; float4 color : COLOR; };
[maxvertexcount(6)] void main(triangle V input[3], inout TriangleStream<O> stream) {
    for(uint layer=0; layer<2; ++layer) {
        for(uint i=0; i<3; ++i) { O o; o.p=input[i].p; o.p.z=0.25+0.25*layer;
            o.layer=layer; o.color=layer==0 ? float4(1,0,0,1) : float4(0,1,0,1); stream.Append(o); }
        stream.RestartStrip();
    }
}"#,
                s!("gs_5_0"),
            )?,
            None,
            Some(&mut gs),
        )?;
        device.CreatePixelShader(
            &compile(
                "float4 main(float4 p:SV_Position, uint layer:SV_RenderTargetArrayIndex, float4 color:COLOR):SV_Target { return color; }",
                s!("ps_5_0"),
            )?,
            None,
            Some(&mut ps),
        )?;
    }
    for volume in [false, true] {
        let label = if volume { "layers-volume" } else { "layers-array" };
        let (mut rtv, mut dsv) = (None, None);
        unsafe {
            c.ClearState();
            if volume {
                let mut texture = None;
                device.CreateTexture3D(
                    &D3D11_TEXTURE3D_DESC {
                        Width: 128,
                        Height: 128,
                        Depth: 6,
                        MipLevels: 2,
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut texture),
                )?;
                device.CreateRenderTargetView(
                    texture.as_ref().unwrap(),
                    Some(&D3D11_RENDER_TARGET_VIEW_DESC {
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        ViewDimension: D3D11_RTV_DIMENSION_TEXTURE3D,
                        Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                            Texture3D: D3D11_TEX3D_RTV {
                                MipSlice: 1,
                                FirstWSlice: 1,
                                WSize: u32::MAX,
                            },
                        },
                    }),
                    Some(&mut rtv),
                )?;
            } else {
                for depth in [false, true] {
                    let mut texture = None;
                    let format = if depth {
                        DXGI_FORMAT_D32_FLOAT
                    } else {
                        DXGI_FORMAT_R8G8B8A8_UNORM
                    };
                    device.CreateTexture2D(
                        &D3D11_TEXTURE2D_DESC {
                            Width: 128,
                            Height: 128,
                            MipLevels: 2,
                            ArraySize: 4,
                            Format: format,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Usage: D3D11_USAGE_DEFAULT,
                            BindFlags: if depth {
                                D3D11_BIND_DEPTH_STENCIL.0
                            } else {
                                D3D11_BIND_RENDER_TARGET.0
                            } as u32,
                            ..Default::default()
                        },
                        None,
                        Some(&mut texture),
                    )?;
                    if depth {
                        device.CreateDepthStencilView(
                            texture.as_ref().unwrap(),
                            Some(&D3D11_DEPTH_STENCIL_VIEW_DESC {
                                Format: format,
                                ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DARRAY,
                                Anonymous: D3D11_DEPTH_STENCIL_VIEW_DESC_0 {
                                    Texture2DArray: D3D11_TEX2D_ARRAY_DSV {
                                        MipSlice: 1,
                                        FirstArraySlice: 1,
                                        ArraySize: 2,
                                    },
                                },
                                ..Default::default()
                            }),
                            Some(&mut dsv),
                        )?;
                        c.ClearDepthStencilView(dsv.as_ref().unwrap(), D3D11_CLEAR_DEPTH.0, 1.0, 0);
                    } else {
                        device.CreateRenderTargetView(
                            texture.as_ref().unwrap(),
                            Some(&D3D11_RENDER_TARGET_VIEW_DESC {
                                Format: format,
                                ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DARRAY,
                                Anonymous: D3D11_RENDER_TARGET_VIEW_DESC_0 {
                                    Texture2DArray: D3D11_TEX2D_ARRAY_RTV {
                                        MipSlice: 1,
                                        FirstArraySlice: 1,
                                        ArraySize: 2,
                                    },
                                },
                            }),
                            Some(&mut rtv),
                        )?;
                    }
                }
            }
        }
        let dir = begin(swap, label)?;
        unsafe {
            shaders.bind(c, rtv.as_ref().unwrap(), &cb);
            c.OMSetRenderTargets(Some(&[rtv]), dsv.as_ref());
            c.GSSetShader(gs.as_ref(), None);
            c.PSSetShader(ps.as_ref(), None);
            c.Draw(3, 0);
            c.OMSetRenderTargets(None, None);
        }
        finish(checks, swap, &dir, label)?;
        let db = rusqlite::Connection::open(dir.join(format!("captures/{label}/capture.db"))).unwrap();
        let count: u32 = db
            .query_row("SELECT count(*) FROM targets", [], |r| r.get(0))
            .unwrap();
        checks.check(
            count == if volume { 2 } else { 4 },
            &format!("{label}: all view slices captured (got {count})"),
        );
        let distinct: u32 = db
            .query_row(
                "SELECT count(DISTINCT blob) FROM targets WHERE target LIKE 'rt0%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        checks.check(
            distinct == 2,
            &format!("{label}: the two rendered layers have different pixels"),
        );
        let keys: Vec<String> = db
            .prepare("SELECT blob FROM targets WHERE target LIKE 'rt0%' ORDER BY target")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let blobs = locum_capture::blobs::Blobs::new(dir.join("blobs"));
        let expected = [[255, 0, 0, 255], [0, 255, 0, 255]];
        checks.check(
            keys.len() == 2
                && keys.iter().zip(expected).all(|(k, pixel)| {
                    blobs.get(k).is_ok_and(|b| {
                        b.len() == 64 * 64 * 4 && b.as_chunks::<4>().0.iter().all(|p| *p == pixel)
                    })
                }),
            &format!("{label}: layer 1 is solid red and layer 2 solid green"),
        );
        // Change only the second layer's expected output; the first layer remains a valid control.
        let bad = clone_capture(&dir, label, &format!("{label}-tampered"));
        bad.execute("UPDATE targets SET blob=(SELECT blob FROM targets WHERE target LIKE 'rt0%' ORDER BY target LIMIT 1) WHERE target=(SELECT target FROM targets WHERE target LIKE 'rt0%' ORDER BY target DESC LIMIT 1)", []).unwrap();
        if !volume {
            let old = clone_capture(&dir, label, "layers-legacy");
            let has_z: bool = old
                .query_row(
                    "SELECT count(*)>0 FROM pragma_table_info('targets') WHERE name='z'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            if has_z {
                old.execute("ALTER TABLE targets DROP COLUMN z", []).unwrap();
            }
        }
    }
    unsafe {
        c.ClearState();
    }
    let (_texture, rtv) = target(device)?;
    let (mut depth, mut dsv) = (None, None);
    let mut uavs = Vec::new();
    unsafe {
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: 16,
                Height: 16,
                MipLevels: 1,
                ArraySize: 2,
                Format: DXGI_FORMAT_D32_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut depth),
        )?;
        device.CreateDepthStencilView(depth.as_ref().unwrap(), None, Some(&mut dsv))?;
        for format in [DXGI_FORMAT_R32_UINT, DXGI_FORMAT_R32_FLOAT] {
            let (mut texture, mut view) = (None, None);
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: 16,
                    Height: 16,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: D3D11_BIND_UNORDERED_ACCESS.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut texture),
            )?;
            device.CreateUnorderedAccessView(texture.as_ref().unwrap(), None, Some(&mut view))?;
            uavs.push(view.unwrap());
        }
    }
    let dir = begin(swap, "layers-clear")?;
    unsafe {
        // Deliberately never bound and never followed by a draw.
        c.ClearRenderTargetView(&rtv, &[0.0, 1.0, 1.0, 1.0]);
        c.ClearRenderTargetView(&rtv, &[1.0, 0.0, 0.0, 1.0]);
        c.ClearDepthStencilView(dsv.as_ref().unwrap(), D3D11_CLEAR_DEPTH.0, 0.25, 0);
        c.ClearUnorderedAccessViewUint(&uavs[0], &[19, 0, 0, 0]);
        c.ClearUnorderedAccessViewFloat(&uavs[1], &[0.5, 0.0, 0.0, 0.0]);
        c.cast::<ID3D11DeviceContext1>()?.ClearView(
            &rtv,
            &[0.0, 0.0, 1.0, 1.0],
            Some(&[windows::Win32::Foundation::RECT {
                left: 8,
                top: 9,
                right: 17,
                bottom: 21,
            }]),
        );
    }
    finish(checks, swap, &dir, "layers-clear")?;
    let db = rusqlite::Connection::open(dir.join("captures/layers-clear/capture.db")).unwrap();
    let count: u32 = db
        .query_row(
            "SELECT count(*) FROM results JOIN events ON events.seq=results.event WHERE kind='clear'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    checks.check(
        count == 6,
        &format!("layers-clear: all six clear events compared (got {count})"),
    );
    // Corrupt the first clear's replay arguments. Later clears overwrite it, so a final-frame check misses it.
    let bad = clone_capture(&dir, "layers-clear", "layers-clear-tampered");
    bad.execute("UPDATE events SET args=json_set(args,'$.color',json('[0,0,0,0]')) WHERE seq=(SELECT min(seq) FROM events WHERE kind='clear')", []).unwrap();
    deferred_clears(checks, device, c, swap, &rtv)?;
    budget(checks, device, c, swap)?;
    Ok(())
}

fn deferred_clears(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
    rtv: &ID3D11RenderTargetView,
) -> Result<()> {
    let mut deferred = None;
    unsafe {
        device.CreateDeferredContext(0, Some(&mut deferred))?;
    }
    let deferred = deferred.unwrap();
    let mut list = None;
    let dir = replay_rows::begin_config(swap, "layers-deferred", "", || {
        unsafe {
            deferred.ClearRenderTargetView(rtv, &[0.0, 1.0, 0.0, 1.0]);
            deferred.ClearRenderTargetView(rtv, &[1.0, 0.0, 0.0, 1.0]);
            deferred.FinishCommandList(false, Some(&mut list))?;
        }
        Ok(())
    })?;
    unsafe {
        c.ExecuteCommandList(list.as_ref().unwrap(), false);
        c.ClearRenderTargetView(rtv, &[0.0, 0.0, 1.0, 1.0]);
        c.ExecuteCommandList(list.as_ref().unwrap(), false);
    }
    finish(checks, swap, &dir, "layers-deferred")?;
    let db = rusqlite::Connection::open(dir.join("captures/layers-deferred/capture.db")).unwrap();
    let counts: (u32, u32) = db
        .query_row(
            "SELECT (SELECT count(*) FROM events WHERE kind='clear'), (SELECT count(*) FROM results)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    checks.check(
        counts == (5, 5),
        "layers-deferred: each repeated-list clear retains its own copy",
    );
    Ok(())
}

fn budget(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
) -> Result<()> {
    let shaders = Shaders::new(device)?;
    let cb = constant_buffer(device, false)?;
    let (_small, clear) = target(device)?;
    for (label, volume, clear_first) in [
        ("layers-budget-array", false, false),
        ("layers-budget-volume", true, false),
        ("layers-budget-shared", false, true),
    ] {
        let mut rtv = None;
        unsafe {
            c.ClearState();
            if volume {
                let mut texture = None;
                device.CreateTexture3D(
                    &D3D11_TEXTURE3D_DESC {
                        Width: 512,
                        Height: 512,
                        Depth: 2,
                        MipLevels: 1,
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut texture),
                )?;
                device.CreateRenderTargetView(texture.as_ref().unwrap(), None, Some(&mut rtv))?;
            } else {
                let mut texture = None;
                device.CreateTexture2D(
                    &D3D11_TEXTURE2D_DESC {
                        Width: 512,
                        Height: 512,
                        MipLevels: 1,
                        ArraySize: 2,
                        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Usage: D3D11_USAGE_DEFAULT,
                        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                        ..Default::default()
                    },
                    None,
                    Some(&mut texture),
                )?;
                device.CreateRenderTargetView(texture.as_ref().unwrap(), None, Some(&mut rtv))?;
            }
            c.ClearRenderTargetView(rtv.as_ref().unwrap(), &[0.0; 4]);
        }
        let dir = replay_rows::begin_config(swap, label, "max_mb 1", || Ok(()))?;
        unsafe {
            if clear_first {
                c.ClearRenderTargetView(&clear, &[0.0; 4]);
            }
            shaders.bind(c, rtv.as_ref().unwrap(), &cb);
            c.Draw(3, 0);
            c.OMSetRenderTargets(None, None);
            // Once one plane has spent the budget, even a tiny clear must be reported as over limit.
            if !clear_first {
                c.ClearRenderTargetView(&clear, &[0.0; 4]);
            }
        }
        finish(checks, swap, &dir, label)?;
        let db = rusqlite::Connection::open(dir.join(format!("captures/{label}/capture.db"))).unwrap();
        let (targets, results): (u32, u32) = db
            .query_row(
                "SELECT (SELECT count(*) FROM targets),(SELECT count(*) FROM results)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let gaps: u32 = db
            .query_row(
                "SELECT count(*) FROM gaps WHERE detail LIKE '%max_mb%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        checks.check((targets,results) == if clear_first {(0,1)} else {(1,0)} && gaps > 0, &format!("{label}: layers and clears share max_mb, with explicit gaps (targets={targets}, results={results})"));
    }
    Ok(())
}

fn clone_capture(dir: &std::path::Path, source: &str, dest: &str) -> rusqlite::Connection {
    let dest = dir.join("captures").join(dest);
    std::fs::create_dir_all(&dest).unwrap();
    std::fs::copy(
        dir.join("captures").join(source).join("capture.db"),
        dest.join("capture.db"),
    )
    .unwrap();
    rusqlite::Connection::open(dest.join("capture.db")).unwrap()
}
