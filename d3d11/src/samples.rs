//! A multisampled texture's samples, bit for bit, on Direct3D 11 (the programs and which formats go which way are in
//! [`locum_capture::samples`] and [`locum_capture::format::Samples`]). No copy reads or writes one sample, so draws
//! do: recorded on a deferred context of the texture's device, and run on the caller's context with its state
//! restored after them, so the caller's bindings are as they were.

use locum_capture::format::{self, Samples};
use locum_capture::layout::{self, Sub};
use locum_capture::samples as programs;
use windows::Win32::Graphics::Direct3D::D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// What draws into or out of one multisampled texture need: its description, how its samples go, and its device.
struct Texture {
    desc: D3D11_TEXTURE2D_DESC,
    samples: Samples,
    device: ID3D11Device,
}

impl Texture {
    fn of(c: &ID3D11DeviceContext, texture: &ID3D11Texture2D) -> Result<Texture, String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let name = format::name(desc.Format.0 as u32);
        let samples = format::samples(desc.Format.0 as u32)
            .ok_or_else(|| format!("format {name} has no samples locum can read"))?;
        if desc.SampleDesc.Count > programs::MOST {
            return Err(format!(
                "{} samples, more than {}",
                desc.SampleDesc.Count,
                programs::MOST
            ));
        }
        if desc.SampleDesc.Count == 1 && desc.MipLevels != 1 {
            return Err("a one-sample texture of more than one mip".into());
        }
        if desc.ArraySize * desc.SampleDesc.Count > programs::MOST_IMAGES {
            return Err(format!(
                "{} slices of {} samples",
                desc.ArraySize, desc.SampleDesc.Count
            ));
        }
        let device = unsafe { c.GetDevice() }.map_err(|e| format!("no device: {e}"))?;
        Ok(Texture {
            desc,
            samples,
            device,
        })
    }

    fn count(&self) -> u32 {
        self.desc.SampleDesc.Count
    }

    fn multisampled(&self) -> bool {
        self.count() > 1
    }

    /// One image per sample of each slice, in the canonical layout's order.
    fn images(&self) -> u32 {
        self.desc.ArraySize * self.count()
    }

    /// A texture like this one in the twin's format, bound as `bind`.
    fn twin(&self, bind: D3D11_BIND_FLAG) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Format: DXGI_FORMAT(self.samples.twin() as i32),
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
            ..self.desc
        };
        let mut twin = None;
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut twin)) }
            .map_err(|e| format!("a twin of format {}: {e}", format::name(desc.Format.0 as u32)))?;
        twin.ok_or_else(|| "no twin".into())
    }

    /// One-sample images of every sample, bound as `bind`, holding `contents` (canonical, see [`subs`]) if given.
    fn images_texture(
        &self,
        bind: D3D11_BIND_FLAG,
        contents: Option<&[u8]>,
    ) -> Result<ID3D11Texture2D, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: self.desc.Width,
            Height: self.desc.Height,
            MipLevels: 1,
            ArraySize: self.images(),
            Format: DXGI_FORMAT(self.samples.image() as i32),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: bind.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let initial: Option<Vec<D3D11_SUBRESOURCE_DATA>> = match contents {
            Some(bytes) => {
                let subs = self.subs()?;
                if bytes.len() as u64 != layout::total(&subs) {
                    return Err(format!(
                        "{} bytes of samples for a {}-byte layout",
                        bytes.len(),
                        layout::total(&subs)
                    ));
                }
                Some(
                    subs.iter()
                        .map(|s| D3D11_SUBRESOURCE_DATA {
                            pSysMem: bytes[s.offset as usize..].as_ptr().cast(),
                            SysMemPitch: s.row_bytes,
                            SysMemSlicePitch: s.slice_bytes() as u32,
                        })
                        .collect(),
                )
            }
            None => None,
        };
        let mut images = None;
        unsafe {
            self.device
                .CreateTexture2D(&desc, initial.as_ref().map(|i| i.as_ptr()), Some(&mut images))
        }
        .map_err(|e| format!("images of format {}: {e}", format::name(desc.Format.0 as u32)))?;
        images.ok_or_else(|| "no images".into())
    }

    /// The canonical layout of the samples: one image a sample of each slice.
    fn subs(&self) -> Result<Vec<Sub>, String> {
        let d = &self.desc;
        layout::multisampled(
            d.Format.0 as u32,
            d.Width,
            d.Height,
            d.ArraySize,
            d.SampleDesc.Count,
        )
        .ok_or_else(|| {
            format!(
                "format {} has no canonical layout",
                format::name(d.Format.0 as u32)
            )
        })
    }

    /// A view of `resource` as `view`, every slice, multisampled or not as this texture is.
    fn source_view(&self, resource: &ID3D11Texture2D, view: u32) -> Result<ID3D11ShaderResourceView, String> {
        let mut desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT(view as i32),
            ..Default::default()
        };
        if self.multisampled() {
            desc.ViewDimension = windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2DMSARRAY;
            desc.Anonymous.Texture2DMSArray = D3D11_TEX2DMS_ARRAY_SRV {
                FirstArraySlice: 0,
                ArraySize: self.desc.ArraySize,
            };
        } else {
            desc.ViewDimension = windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D11_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                FirstArraySlice: 0,
                ArraySize: self.desc.ArraySize,
            };
        }
        self.view(resource, &desc)
    }

    /// A view of the images as `format`.
    fn images_view(&self, images: &ID3D11Texture2D, format: u32) -> Result<ID3D11ShaderResourceView, String> {
        let mut desc = D3D11_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT(format as i32),
            ViewDimension: windows::Win32::Graphics::Direct3D::D3D11_SRV_DIMENSION_TEXTURE2DARRAY,
            ..Default::default()
        };
        desc.Anonymous.Texture2DArray = D3D11_TEX2D_ARRAY_SRV {
            MostDetailedMip: 0,
            MipLevels: 1,
            FirstArraySlice: 0,
            ArraySize: self.images(),
        };
        self.view(images, &desc)
    }

    fn view(
        &self,
        resource: &ID3D11Texture2D,
        desc: &D3D11_SHADER_RESOURCE_VIEW_DESC,
    ) -> Result<ID3D11ShaderResourceView, String> {
        let mut view = None;
        unsafe {
            self.device
                .CreateShaderResourceView(resource, Some(desc), Some(&mut view))
        }
        .map_err(|e| format!("a view as {}: {e}", format::name(desc.Format.0 as u32)))?;
        view.ok_or_else(|| "no view".into())
    }

    /// A render target view of slice `slice` of `resource` as `format`: an image (one sample), or a slice of the
    /// twin, multisampled as this texture is.
    fn target(
        &self,
        resource: &ID3D11Texture2D,
        format: u32,
        slice: u32,
        multisampled: bool,
    ) -> Result<ID3D11RenderTargetView, String> {
        let mut desc = D3D11_RENDER_TARGET_VIEW_DESC {
            Format: DXGI_FORMAT(format as i32),
            ..Default::default()
        };
        if multisampled {
            desc.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2DMSARRAY;
            desc.Anonymous.Texture2DMSArray = D3D11_TEX2DMS_ARRAY_RTV {
                FirstArraySlice: slice,
                ArraySize: 1,
            };
        } else {
            desc.ViewDimension = D3D11_RTV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D11_TEX2D_ARRAY_RTV {
                MipSlice: 0,
                FirstArraySlice: slice,
                ArraySize: 1,
            };
        }
        let mut view = None;
        unsafe {
            self.device
                .CreateRenderTargetView(resource, Some(&desc), Some(&mut view))
        }
        .map_err(|e| format!("a render target of {}: {e}", format::name(format)))?;
        view.ok_or_else(|| "no render target".into())
    }

    /// A depth-stencil view of slice `slice` of the twin.
    fn depth_target(&self, twin: &ID3D11Texture2D, slice: u32) -> Result<ID3D11DepthStencilView, String> {
        // The family's depth-stencil format: D24_UNORM_S8_UINT or D32_FLOAT_S8X24_UINT.
        let format = self.samples.twin() + 1;
        let mut desc = D3D11_DEPTH_STENCIL_VIEW_DESC {
            Format: DXGI_FORMAT(format as i32),
            ..Default::default()
        };
        if self.multisampled() {
            desc.ViewDimension = D3D11_DSV_DIMENSION_TEXTURE2DMSARRAY;
            desc.Anonymous.Texture2DMSArray = D3D11_TEX2DMS_ARRAY_DSV {
                FirstArraySlice: slice,
                ArraySize: 1,
            };
        } else {
            desc.ViewDimension = D3D11_DSV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D11_TEX2D_ARRAY_DSV {
                MipSlice: 0,
                FirstArraySlice: slice,
                ArraySize: 1,
            };
        }
        let mut view = None;
        unsafe {
            self.device
                .CreateDepthStencilView(twin, Some(&desc), Some(&mut view))
        }
        .map_err(|e| format!("a depth-stencil view of {}: {e}", format::name(format)))?;
        view.ok_or_else(|| "no depth-stencil view".into())
    }

    /// A deferred context set up to draw the passes: one triangle over a `Width` x `Height` image, nothing culled.
    /// A deferred context set up for the passes, with the constant buffer of their `k` bound (see [`draw`]).
    fn passes(
        &self,
        pixels: &[u8],
    ) -> Result<(ID3D11DeviceContext, ID3D11Buffer, ID3D11PixelShader), String> {
        let d = &self.device;
        let mut context = None;
        unsafe { d.CreateDeferredContext(0, Some(&mut context)) }
            .map_err(|e| format!("a deferred context: {e}"))?;
        let context = context.ok_or("no deferred context")?;
        let mut vs = None;
        unsafe { d.CreateVertexShader(programs::VERTICES, None, Some(&mut vs)) }
            .map_err(|e| format!("the vertex shader: {e}"))?;
        let ps = self.pixel(pixels)?;
        let mut constant = None;
        let c = D3D11_BUFFER_DESC {
            ByteWidth: 16,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        unsafe { d.CreateBuffer(&c, None, Some(&mut constant)) }
            .map_err(|e| format!("the passes' constant buffer: {e}"))?;
        let constant = constant.ok_or("no constant buffer")?;
        let mut rasterizer = None;
        let r = D3D11_RASTERIZER_DESC {
            FillMode: D3D11_FILL_SOLID,
            CullMode: D3D11_CULL_NONE,
            DepthClipEnable: true.into(),
            ..Default::default()
        };
        unsafe { d.CreateRasterizerState(&r, Some(&mut rasterizer)) }
            .map_err(|e| format!("a rasterizer: {e}"))?;
        let viewport = D3D11_VIEWPORT {
            TopLeftX: 0.0,
            TopLeftY: 0.0,
            Width: self.desc.Width as f32,
            Height: self.desc.Height as f32,
            MinDepth: 0.0,
            MaxDepth: 1.0,
        };
        unsafe {
            context.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            context.VSSetShader(vs.as_ref(), None);
            context.VSSetConstantBuffers(0, Some(&[Some(constant.clone())]));
            context.PSSetShader(&ps, None);
            context.RSSetState(rasterizer.as_ref());
            context.RSSetViewports(Some(&[viewport]));
        }
        Ok((context, constant, ps))
    }

    fn pixel(&self, program: &[u8]) -> Result<ID3D11PixelShader, String> {
        let mut ps = None;
        unsafe { self.device.CreatePixelShader(program, None, Some(&mut ps)) }
            .map_err(|e| format!("a pixel shader: {e}"))?;
        ps.ok_or_else(|| "no pixel shader".into())
    }
}

/// Runs what `context` recorded on `c`, which keeps its state.
fn run(c: &ID3D11DeviceContext, context: &ID3D11DeviceContext) -> Result<(), String> {
    let mut list = None;
    unsafe { context.FinishCommandList(false, Some(&mut list)) }.map_err(|e| format!("the passes: {e}"))?;
    unsafe { c.ExecuteCommandList(list.as_ref().ok_or("no command list")?, true) };
    Ok(())
}

/// Draws every sample of `texture` (multisampled; or of one sample and one mip, sample 0) into a new one-sample
/// image array of [`Samples::image`]'s format, bound as a render target: image `slice * samples + sample`, so the
/// images in subresource order are the canonical layout's bytes ([`layout::multisampled`]).
///
/// # Safety
/// `c` must be a context of the device that made `texture`.
pub unsafe fn read(c: &ID3D11DeviceContext, texture: &ID3D11Texture2D) -> Result<ID3D11Texture2D, String> {
    let t = Texture::of(c, texture)?;
    // A multisampled texture is a render target or a depth-stencil too.
    let bound = match t.samples {
        Samples::DepthStencil { .. } => D3D11_BIND_DEPTH_STENCIL,
        _ => D3D11_BIND_RENDER_TARGET,
    };
    let twin = t.twin(D3D11_BIND_SHADER_RESOURCE | bound)?;
    let images = t.images_texture(D3D11_BIND_RENDER_TARGET, None)?;
    let views = match t.samples {
        Samples::Bits { view, .. } | Samples::Floats { view, .. } => vec![Some(t.source_view(&twin, view)?)],
        Samples::DepthStencil { depth, stencil, .. } => {
            vec![
                Some(t.source_view(&twin, depth)?),
                Some(t.source_view(&twin, stencil)?),
            ]
        }
    };
    let (context, k, _ps) = t.passes(programs::read(t.samples, t.multisampled()))?;
    unsafe {
        context.CopyResource(&twin, texture);
        context.PSSetShaderResources(0, Some(&views));
        for slice in 0..t.desc.ArraySize {
            for sample in 0..t.count() {
                let image = slice * t.count() + sample;
                let target = t.target(&images, t.samples.image(), image, false)?;
                context.OMSetRenderTargets(Some(&[Some(target)]), None);
                draw(&context, &k, programs::read_k(slice, sample));
            }
        }
    }
    run(c, &context)?;
    Ok(images)
}

/// Writes `contents` (the samples in the canonical layout, as [`read`] reads them) into every sample of `texture`.
///
/// # Safety
/// `c` must be a context of the device that made `texture`.
pub unsafe fn write(
    c: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    contents: &[u8],
) -> Result<(), String> {
    let t = Texture::of(c, texture)?;
    let images = t.images_texture(D3D11_BIND_SHADER_RESOURCE, Some(contents))?;
    let (colour, stencil) = programs::write(t.samples);
    let (context, k, _ps) = t.passes(colour)?;
    match t.samples {
        Samples::Bits { view, .. } | Samples::Floats { view, .. } => {
            let twin = t.twin(D3D11_BIND_RENDER_TARGET)?;
            let source = t.images_view(&images, view)?;
            unsafe {
                context.PSSetShaderResources(0, Some(&[Some(source)]));
                for slice in 0..t.desc.ArraySize {
                    let target = t.target(&twin, view, slice, t.multisampled())?;
                    context.OMSetRenderTargets(Some(&[Some(target)]), None);
                    for sample in 0..t.count() {
                        draw(
                            &context,
                            &k,
                            programs::write_k(slice * t.count() + sample, sample, None),
                        );
                    }
                }
                context.CopyResource(texture, &twin);
            }
        }
        Samples::DepthStencil { .. } => {
            let twin = t.twin(D3D11_BIND_DEPTH_STENCIL)?;
            let source = t.images_view(&images, t.samples.image())?;
            let stencil = t.pixel(stencil.expect("a depth-stencil family writes its stencil"))?;
            let state = |desc: &D3D11_DEPTH_STENCIL_DESC| -> Result<ID3D11DepthStencilState, String> {
                let mut s = None;
                unsafe { t.device.CreateDepthStencilState(desc, Some(&mut s)) }
                    .map_err(|e| format!("a depth-stencil state: {e}"))?;
                s.ok_or_else(|| "no depth-stencil state".into())
            };
            let depth = state(&D3D11_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ALL,
                DepthFunc: D3D11_COMPARISON_ALWAYS,
                StencilEnable: false.into(),
                ..Default::default()
            })?;
            let replace = D3D11_DEPTH_STENCILOP_DESC {
                StencilFailOp: D3D11_STENCIL_OP_KEEP,
                StencilDepthFailOp: D3D11_STENCIL_OP_KEEP,
                StencilPassOp: D3D11_STENCIL_OP_REPLACE,
                StencilFunc: D3D11_COMPARISON_ALWAYS,
            };
            let bits = (0..8)
                .map(|bit| {
                    state(&D3D11_DEPTH_STENCIL_DESC {
                        DepthEnable: false.into(),
                        DepthWriteMask: D3D11_DEPTH_WRITE_MASK_ZERO,
                        DepthFunc: D3D11_COMPARISON_ALWAYS,
                        StencilEnable: true.into(),
                        StencilReadMask: 0xFF,
                        StencilWriteMask: 1 << bit,
                        FrontFace: replace,
                        BackFace: replace,
                    })
                })
                .collect::<Result<Vec<_>, String>>()?;
            let colour = t.pixel(colour)?;
            unsafe {
                context.PSSetShaderResources(0, Some(&[Some(source)]));
                for slice in 0..t.desc.ArraySize {
                    let target = t.depth_target(&twin, slice)?;
                    context.OMSetRenderTargets(None, &target);
                    let both = D3D11_CLEAR_DEPTH.0 | D3D11_CLEAR_STENCIL.0;
                    context.ClearDepthStencilView(&target, both, 0.0, 0);
                    for sample in 0..t.count() {
                        let image = slice * t.count() + sample;
                        context.PSSetShader(&colour, None);
                        context.OMSetDepthStencilState(&depth, 0);
                        draw(&context, &k, programs::write_k(image, sample, None));
                        context.PSSetShader(&stencil, None);
                        for (bit, state) in bits.iter().enumerate() {
                            context.OMSetDepthStencilState(state, 0xFF);
                            draw(&context, &k, programs::write_k(image, sample, Some(bit as u32)));
                        }
                    }
                }
                context.CopyResource(texture, &twin);
            }
        }
    }
    run(c, &context)
}

/// One pass's triangle, its `k` in the vertex shader's constant buffer `constant` (see [`programs`]).
unsafe fn draw(context: &ID3D11DeviceContext, constant: &ID3D11Buffer, k: u32) {
    let words = [k, 0, 0, 0];
    unsafe {
        context.UpdateSubresource(constant, 0, None, words.as_ptr().cast(), 0, 0);
        context.Draw(3, 0);
    }
}
