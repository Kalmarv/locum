//! Multisampled textures whose samples differ, captured bit for bit (the D3D12 smoke's `samples` fixture, on
//! Direct3D 11): their contents as the captured frame first uses them (what the frame before drew into every sample)
//! and their pass-end targets (the captured frame draws sample 3 again). The colours are drawn at sample frequency,
//! each depth-stencil sample by a draw of its own (the blend state's sample mask).
use super::*;

#[path = "../../smoke-d3d12/src/sample_values.rs"]
mod values;
use values::*;

/// One multisampled texture of the fixture's, with its views (one a slice) and the pixel shader that draws it.
struct Texture {
    kind: Kind,
    views: Vec<View>,
    ps: ID3D11PixelShader,
}

enum View {
    Target(ID3D11RenderTargetView),
    Depth(ID3D11DepthStencilView),
}

pub(super) fn run(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
) -> Result<()> {
    let vs = compile(VERTICES, s!("vs_5_0"))?;
    let program = |body: &str| -> Result<ID3D11PixelShader> {
        let code = compile(&format!("{CONSTANTS} {body}"), s!("ps_5_0"))?;
        let mut ps = None;
        unsafe { device.CreatePixelShader(&code, None, Some(&mut ps)) }?;
        Ok(ps.expect("pixel shader"))
    };
    let depth_ps = program(DEPTH)?;
    let mut vertices = None;
    let mut constants = None;
    let mut rasterizer = None;
    unsafe {
        device.CreateVertexShader(&vs, None, Some(&mut vertices))?;
        device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: 48,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut constants),
        )?;
        device.CreateRasterizerState(
            &D3D11_RASTERIZER_DESC {
                FillMode: D3D11_FILL_SOLID,
                CullMode: D3D11_CULL_NONE,
                DepthClipEnable: true.into(),
                MultisampleEnable: true.into(),
                ..Default::default()
            },
            Some(&mut rasterizer),
        )?;
    }
    let (vertices, constants, rasterizer) = (vertices.unwrap(), constants.unwrap(), rasterizer.unwrap());
    let replace = D3D11_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D11_STENCIL_OP_KEEP,
        StencilDepthFailOp: D3D11_STENCIL_OP_KEEP,
        StencilPassOp: D3D11_STENCIL_OP_REPLACE,
        StencilFunc: D3D11_COMPARISON_ALWAYS,
    };
    let depth_state = |depth: bool, stencil: bool| -> Result<ID3D11DepthStencilState> {
        let mut state = None;
        unsafe {
            device.CreateDepthStencilState(
                &D3D11_DEPTH_STENCIL_DESC {
                    DepthEnable: depth.into(),
                    DepthWriteMask: if depth {
                        D3D11_DEPTH_WRITE_MASK_ALL
                    } else {
                        D3D11_DEPTH_WRITE_MASK_ZERO
                    },
                    DepthFunc: D3D11_COMPARISON_ALWAYS,
                    StencilEnable: stencil.into(),
                    StencilReadMask: 0xFF,
                    StencilWriteMask: 0xFF,
                    FrontFace: replace,
                    BackFace: replace,
                },
                Some(&mut state),
            )
        }?;
        Ok(state.expect("depth-stencil state"))
    };
    let mut textures = Vec::new();
    let mut lacking = Vec::new();
    for kind in Kind::ALL {
        let (format, view, slices) = kind.formats();
        let name = locum_capture::format::name(view.0 as u32);
        let levels = unsafe { device.CheckMultisampleQualityLevels(view, SAMPLES) }?;
        let target = if kind.depth() {
            D3D11_FORMAT_SUPPORT_DEPTH_STENCIL
        } else {
            D3D11_FORMAT_SUPPORT_RENDER_TARGET
        }
        .0 as u32
            | D3D11_FORMAT_SUPPORT_MULTISAMPLE_RENDERTARGET.0 as u32;
        let support = unsafe { device.CheckFormatSupport(view) }.unwrap_or(0);
        let drawable = levels > 0 && support & target == target;
        if !drawable && kind.optional() {
            println!("samples: {name} is no {SAMPLES}-sample render target on this device: not drawn");
            lacking.push(name);
            continue;
        }
        checks.check(drawable, &format!("samples: {name} has {SAMPLES} samples here"));
        if !drawable {
            return Ok(());
        }
        let mut texture = None;
        unsafe {
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: WIDTH,
                    Height: HEIGHT,
                    MipLevels: 1,
                    ArraySize: slices,
                    Format: format,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: SAMPLES,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: if kind.depth() {
                        D3D11_BIND_DEPTH_STENCIL.0 as u32
                    } else {
                        D3D11_BIND_RENDER_TARGET.0 as u32
                    },
                    ..Default::default()
                },
                None,
                Some(&mut texture),
            )
        }?;
        let texture = texture.expect("texture");
        let mut views = Vec::new();
        for slice in 0..slices {
            if kind.depth() {
                let mut v = None;
                unsafe {
                    device.CreateDepthStencilView(
                        &texture,
                        Some(&D3D11_DEPTH_STENCIL_VIEW_DESC {
                            Format: view,
                            ViewDimension: D3D11_DSV_DIMENSION_TEXTURE2DMS,
                            ..Default::default()
                        }),
                        Some(&mut v),
                    )
                }?;
                views.push(View::Depth(v.expect("view")));
            } else {
                let mut d = D3D11_RENDER_TARGET_VIEW_DESC {
                    Format: view,
                    ViewDimension: D3D11_RTV_DIMENSION_TEXTURE2DMSARRAY,
                    ..Default::default()
                };
                d.Anonymous.Texture2DMSArray = D3D11_TEX2DMS_ARRAY_RTV {
                    FirstArraySlice: slice,
                    ArraySize: 1,
                };
                let mut v = None;
                unsafe { device.CreateRenderTargetView(&texture, Some(&d), Some(&mut v)) }?;
                views.push(View::Target(v.expect("view")));
            }
        }
        let ps = if kind.depth() {
            depth_ps.clone()
        } else {
            program(&kind.shader())?
        };
        textures.push(Texture { kind, views, ps });
    }
    let (colour_state, depth_only, depth_stencil) = (
        depth_state(false, false)?,
        depth_state(true, false)?,
        depth_state(true, true)?,
    );
    let draw = |alt: bool| unsafe {
        c.ClearState();
        c.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        c.VSSetShader(&vertices, None);
        c.PSSetConstantBuffers(0, Some(&[Some(constants.clone())]));
        c.RSSetState(&rasterizer);
        c.RSSetViewports(Some(&[D3D11_VIEWPORT {
            Width: WIDTH as f32,
            Height: HEIGHT as f32,
            MaxDepth: 1.0,
            ..Default::default()
        }]));
        for t in &textures {
            let state = match t.kind {
                k if k.stencil() => &depth_stencil,
                k if k.depth() => &depth_only,
                _ => &colour_state,
            };
            c.PSSetShader(&t.ps, None);
            for (slice, view) in t.views.iter().enumerate() {
                match view {
                    View::Target(v) => c.OMSetRenderTargets(Some(&[Some(v.clone())]), None),
                    View::Depth(v) => c.OMSetRenderTargets(None, v),
                }
                let samples: Vec<u32> = if alt { vec![3] } else { (0..SAMPLES).collect() };
                for s in samples {
                    // A colour's draw at sample frequency writes every sample the mask lets it.
                    let (mask, depths) = match (t.kind.depth(), alt) {
                        (true, _) => (1 << s, t.kind.depths(s, alt)),
                        (false, true) => (1 << 3, [0.0; 8]),
                        (false, false) => (u32::MAX, [0.0; 8]),
                    };
                    let mut words = vec![slice as u32, u32::from(alt), 0, 0];
                    words.extend(depths.map(f32::to_bits));
                    c.UpdateSubresource(&constants, 0, None, words.as_ptr().cast(), 0, 0);
                    c.OMSetBlendState(None, None, mask);
                    c.OMSetDepthStencilState(state, u32::from(stencil(s, alt)));
                    c.Draw(3, 0);
                    if !t.kind.depth() {
                        break;
                    }
                }
            }
        }
        c.ClearState();
    };
    // A frame before the capture draws every sample: the captured frame's first use finds them.
    draw(false);
    unsafe { swap.Present(0, DXGI_PRESENT(0)) }.ok()?;
    let dir = replay_rows::begin_config(swap, "samples", "", || Ok(()))?;
    // Each target's pass ends where the next is bound.
    draw(true);
    replay_rows::finish(checks, swap, &dir, "samples")?;
    let db = rusqlite::Connection::open(dir.join("captures/samples/capture.db")).unwrap();
    let blobs = locum_capture::blobs::Blobs::new(dir.join("blobs"));
    for kind in textures.iter().map(|t| t.kind) {
        let (format, view, slices) = kind.formats();
        let Ok((id, content, note)) = db.query_row(
            "SELECT id, content, content_note FROM resources WHERE samples = ?1 AND format = ?2",
            rusqlite::params![SAMPLES, format.0],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        ) else {
            checks.check(false, &format!("samples: the {view:?} texture is in the capture"));
            continue;
        };
        let bytes = content.as_ref().and_then(|k| blobs.get(k).ok());
        let want: Vec<u8> = (0..slices).flat_map(|slice| kind.slice(slice, false)).collect();
        checks.check(
            bytes.as_ref().is_some_and(|b| kind.same(b, &want)),
            &format!(
                "samples: #{id} ({view:?})'s contents are every sample's bits as the frame before left them ({:?}; \
                 first difference at byte {:?})",
                note,
                bytes.as_ref().and_then(|b| b.iter().zip(&want).position(|(a, b)| a != b))
            ),
        );
        let rows: Vec<(u32, u32, i64)> = db
            .prepare("SELECT sub, height, size FROM subresources WHERE resource = ?1 ORDER BY sub")
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let size = want.len() as i64 / i64::from(slices);
        checks.check(
            rows == (0..slices)
                .map(|s| (s, HEIGHT * SAMPLES, size))
                .collect::<Vec<_>>(),
            &format!("samples: #{id} ({view:?}): a subresource row a slice, its samples stacked: {rows:?}"),
        );
        let targets: Vec<(u32, u32, u32, Option<String>)> = db
            .prepare("SELECT sub, width, height, blob FROM targets WHERE resource = ?1 ORDER BY sub")
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        checks.check(
            targets.len() == slices as usize,
            &format!("samples: #{id} ({view:?}): a pass end a slice: {targets:?}"),
        );
        for (slice, (sub, width, height, blob)) in targets.into_iter().enumerate() {
            let bytes = blob.and_then(|k| blobs.get(&k).ok());
            let want = kind.slice(slice as u32, true);
            checks.check(
                (sub, width, height) == (slice as u32, WIDTH, HEIGHT * SAMPLES)
                    && bytes.as_ref().is_some_and(|b| kind.same(b, &want)),
                &format!(
                    "samples: #{id} ({view:?}) slice {slice}'s pass end is every sample's bits, stacked (first \
                     difference at byte {:?})",
                    bytes.as_ref().and_then(|b| b.iter().zip(&want).position(|(a, b)| a != b))
                ),
            );
        }
    }
    let drawn: Vec<&str> = textures
        .iter()
        .filter(|t| t.kind.optional())
        .map(|t| locum_capture::format::name(t.kind.formats().1.0 as u32))
        .collect();
    println!(
        "samples: drawn through float views: {}; not render targets here: {}",
        drawn.join(", "),
        if lacking.is_empty() {
            "none".to_string()
        } else {
            lacking.join(", ")
        }
    );
    Ok(())
}

/// A staging copy of `texture`'s first subresource, mapped: its rows, `pitch` bytes apart.
fn staged(
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
) -> Result<(Vec<u8>, usize)> {
    let mut d = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut d) };
    let d = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..d
    };
    let mut staging = None;
    unsafe { device.CreateTexture2D(&d, None, Some(&mut staging)) }?;
    let staging = staging.expect("staging texture");
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        c.CopyResource(&staging, texture);
        c.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let bytes = std::slice::from_raw_parts(
            mapped.pData.cast::<u8>(),
            mapped.RowPitch as usize * d.Height as usize,
        )
        .to_vec();
        c.Unmap(&staging, 0);
        Ok((bytes, mapped.RowPitch as usize))
    }
}

/// Every one of D24's 2^24 depths, with a stencil each, written into a one-sample D24_UNORM_S8_UINT by locum's
/// passes (a float a depth, rounded by the GPU to 24 bits), copied out as the texture holds them, and read again by
/// locum's passes (each depth taken as a float and back to its 24 bits): what both conversions do on this GPU, for
/// every value they can meet.
pub(super) fn conversions(checks: &mut Checks, device: &ID3D11Device, c: &ID3D11DeviceContext) -> Result<()> {
    const SIDE: u32 = 4096;
    let value = |k: u32| k | ((k.wrapping_mul(0x9E37_79B9) >> 24) << 24);
    let mut texture = None;
    unsafe {
        device.CreateTexture2D(
            &D3D11_TEXTURE2D_DESC {
                Width: SIDE,
                Height: SIDE,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_R24G8_TYPELESS,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_DEPTH_STENCIL.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut texture),
        )
    }?;
    let texture = texture.expect("texture");
    let contents: Vec<u8> = (0..SIDE * SIDE).flat_map(|k| value(k).to_le_bytes()).collect();
    let written = unsafe { locum_d3d11::samples::write(c, &texture, &contents) };
    let read = unsafe { locum_d3d11::samples::read(c, &texture) };
    let (Ok(()), Ok(images)) = (&written, &read) else {
        checks.check(
            false,
            &format!("conversions: the passes ran ({written:?}, {:?})", read.err()),
        );
        return Ok(());
    };
    let wrong = |(bytes, pitch): &(Vec<u8>, usize)| -> Vec<(u32, u32)> {
        (0..SIDE * SIDE)
            .filter_map(|k| {
                let at = (k / SIDE) as usize * pitch + (k % SIDE) as usize * 4;
                let got = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
                (got != value(k)).then_some((k, got))
            })
            .collect()
    };
    let (raw, again) = (
        wrong(&staged(device, c, &texture)?),
        wrong(&staged(device, c, images)?),
    );
    checks.check(
        raw.is_empty(),
        &format!(
            "D3D11 D24_UNORM_S8_UINT: all 2^24 depths, with stencils, written exactly by the sample passes ({} wrong, \
             first {:x?})",
            raw.len(),
            &raw[..raw.len().min(4)]
        ),
    );
    checks.check(
        again.is_empty(),
        &format!(
            "D3D11 D24_UNORM_S8_UINT: all 2^24 depths, with stencils, read back exactly by the sample passes ({} \
             wrong, first {:x?})",
            again.len(),
            &again[..again.len().min(4)]
        ),
    );
    Ok(())
}
