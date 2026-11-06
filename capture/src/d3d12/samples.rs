//! A multisampled texture's samples, bit for bit, on Direct3D 12: the passes of [`crate::samples`], recorded on a
//! direct command list of the caller's (locum's own in a game, replay's). A [`Reader`] or [`Writer`] holds what its
//! passes use, so it must live until the GPU has run them.

use core::mem::ManuallyDrop;

use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::format::{self, Samples};
use crate::samples as programs;

/// The objects every pass of one texture's uses.
struct Passes {
    desc: D3D12_RESOURCE_DESC,
    /// The slices drawn: all of them, or one.
    first: u32,
    slices: u32,
    samples: Samples,
    root: ID3D12RootSignature,
    /// The pixel shader's two views, shader visible.
    views: ID3D12DescriptorHeap,
    /// The render target or depth-stencil views drawn into, one per image or slice.
    targets: ID3D12DescriptorHeap,
    target_size: u32,
}

impl Passes {
    unsafe fn new(
        device: &ID3D12Device,
        desc: &D3D12_RESOURCE_DESC,
        slice: Option<u32>,
        targets: D3D12_DESCRIPTOR_HEAP_TYPE,
    ) -> Result<Passes, String> {
        let name = format::name(desc.Format.0 as u32);
        let samples = format::samples(desc.Format.0 as u32)
            .ok_or_else(|| format!("format {name} has no samples locum can read"))?;
        if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
            return Err("not a 2D texture".into());
        }
        let count = desc.SampleDesc.Count;
        if count > programs::MOST {
            return Err(format!("{count} samples, more than {}", programs::MOST));
        }
        if count == 1 && desc.MipLevels != 1 {
            return Err("a one-sample texture of more than one mip".into());
        }
        let (first, slices) = match slice {
            Some(s) if s < u32::from(desc.DepthOrArraySize) => (s, 1),
            Some(s) => return Err(format!("no slice {s}")),
            None => (0, u32::from(desc.DepthOrArraySize)),
        };
        if slices * count > programs::MOST_IMAGES.min(2048) {
            return Err(format!("{slices} slices of {count} samples"));
        }
        let root: ID3D12RootSignature = unsafe { device.CreateRootSignature(0, programs::ROOT) }
            .map_err(|e| format!("the passes' root signature: {e}"))?;
        let heap = |kind, n: u32, flags| -> Result<ID3D12DescriptorHeap, String> {
            unsafe {
                device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: n.max(1),
                    Flags: flags,
                    NodeMask: 0,
                })
            }
            .map_err(|e| format!("a descriptor heap: {e}"))
        };
        let views = heap(
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
            2,
            D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
        )?;
        // Room for a view per image read into (a view per slice written takes fewer).
        let target_heap = heap(targets, slices * count, D3D12_DESCRIPTOR_HEAP_FLAG_NONE)?;
        Ok(Passes {
            desc: *desc,
            first,
            slices,
            samples,
            root,
            views,
            targets: target_heap,
            target_size: unsafe { device.GetDescriptorHandleIncrementSize(targets) },
        })
    }

    fn count(&self) -> u32 {
        self.desc.SampleDesc.Count
    }

    fn slices(&self) -> u32 {
        self.slices
    }

    fn images(&self) -> u32 {
        self.slices() * self.count()
    }

    fn multisampled(&self) -> bool {
        self.count() > 1
    }

    /// A committed resource of `desc` in a default heap, first in `state`.
    unsafe fn committed(
        device: &ID3D12Device,
        desc: &D3D12_RESOURCE_DESC,
        state: D3D12_RESOURCE_STATES,
        what: &str,
    ) -> Result<ID3D12Resource, String> {
        let mut made: Option<ID3D12Resource> = None;
        unsafe {
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                desc,
                state,
                None,
                &mut made,
            )
        }
        .map_err(|e| format!("{what} of format {}: {e}", format::name(desc.Format.0 as u32)))?;
        made.ok_or_else(|| format!("no {what}"))
    }

    /// A texture like the one read or written in the twin's format, with `flags`.
    unsafe fn twin(
        &self,
        device: &ID3D12Device,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
    ) -> Result<ID3D12Resource, String> {
        let desc = D3D12_RESOURCE_DESC {
            Format: DXGI_FORMAT(self.samples.twin() as i32),
            Alignment: 0,
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: flags,
            ..self.desc
        };
        unsafe { Self::committed(device, &desc, state, "a twin") }
    }

    /// The description of the one-sample images of every sample, in the canonical layout's order.
    fn images_desc(&self, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
        D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Alignment: 0,
            Width: self.desc.Width,
            Height: self.desc.Height,
            DepthOrArraySize: self.images() as u16,
            MipLevels: 1,
            Format: DXGI_FORMAT(self.samples.image() as i32),
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: flags,
        }
    }

    /// View `at` (0 or 1) of the pixel shader's: `resource` as `format`, every slice, multisampled where `ms`; plane
    /// `plane` of a one-sample depth-stencil. No resource makes a null view.
    unsafe fn view(
        &self,
        device: &ID3D12Device,
        at: u32,
        resource: Option<&ID3D12Resource>,
        format: u32,
        (slices, ms, plane): (u32, bool, u32),
    ) {
        let mut desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT(format as i32),
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            ..Default::default()
        };
        if ms {
            desc.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY;
            desc.Anonymous.Texture2DMSArray = D3D12_TEX2DMS_ARRAY_SRV {
                FirstArraySlice: 0,
                ArraySize: slices,
            };
        } else {
            desc.ViewDimension = D3D12_SRV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                FirstArraySlice: 0,
                ArraySize: slices,
                PlaneSlice: plane,
                ResourceMinLODClamp: 0.0,
            };
        }
        let mut handle = unsafe { self.views.GetCPUDescriptorHandleForHeapStart() };
        handle.ptr += (at
            * unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) })
            as usize;
        unsafe { device.CreateShaderResourceView(resource, Some(&desc), handle) };
    }

    fn target(&self, at: u32) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let mut handle = unsafe { self.targets.GetCPUDescriptorHandleForHeapStart() };
        handle.ptr += (at * self.target_size) as usize;
        handle
    }

    /// A render target view of slice `slice` of `resource` as `format`, multisampled where `ms`, at `at`.
    unsafe fn render_target(
        &self,
        device: &ID3D12Device,
        at: u32,
        resource: &ID3D12Resource,
        format: u32,
        slice: u32,
        ms: bool,
    ) {
        let mut desc = D3D12_RENDER_TARGET_VIEW_DESC {
            Format: DXGI_FORMAT(format as i32),
            ..Default::default()
        };
        if ms {
            desc.ViewDimension = D3D12_RTV_DIMENSION_TEXTURE2DMSARRAY;
            desc.Anonymous.Texture2DMSArray = D3D12_TEX2DMS_ARRAY_RTV {
                FirstArraySlice: slice,
                ArraySize: 1,
            };
        } else {
            desc.ViewDimension = D3D12_RTV_DIMENSION_TEXTURE2DARRAY;
            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_RTV {
                MipSlice: 0,
                FirstArraySlice: slice,
                ArraySize: 1,
                PlaneSlice: 0,
            };
        }
        unsafe { device.CreateRenderTargetView(resource, Some(&desc), self.target(at)) };
    }

    /// A pipeline of the passes' vertex shader and `pixels`, drawing into `targets` (render target formats) or a
    /// depth-stencil of `depth` with `depth_stencil`, multisampled as the texture where `ms`.
    unsafe fn pipeline(
        &self,
        device: &ID3D12Device,
        pixels: &[u8],
        targets: &[u32],
        depth: Option<(u32, D3D12_DEPTH_STENCIL_DESC)>,
        ms: bool,
    ) -> Result<ID3D12PipelineState, String> {
        let bytecode = |b: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: b.as_ptr().cast(),
            BytecodeLength: b.len(),
        };
        let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
        for (f, t) in formats.iter_mut().zip(targets) {
            *f = DXGI_FORMAT(*t as i32);
        }
        let write_all = D3D12_RENDER_TARGET_BLEND_DESC {
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        };
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(self.root.clone())),
            VS: bytecode(programs::VERTICES),
            PS: bytecode(pixels),
            BlendState: D3D12_BLEND_DESC {
                AlphaToCoverageEnable: false.into(),
                IndependentBlendEnable: false.into(),
                RenderTarget: [write_all; 8],
            },
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            DepthStencilState: depth.map(|d| d.1).unwrap_or_default(),
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: targets.len() as u32,
            RTVFormats: formats,
            DSVFormat: DXGI_FORMAT(depth.map_or(0, |d| d.0) as i32),
            SampleDesc: if ms {
                self.desc.SampleDesc
            } else {
                DXGI_SAMPLE_DESC { Count: 1, Quality: 0 }
            },
            ..Default::default()
        };
        let made = unsafe { device.CreateGraphicsPipelineState(&desc) }
            .map_err(|e| format!("a pass's pipeline: {e}"));
        unsafe { ManuallyDrop::drop(&mut desc.pRootSignature) };
        made
    }

    /// Sets what every pass draws with on `list`.
    unsafe fn bind(&self, list: &ID3D12GraphicsCommandList) {
        unsafe {
            list.SetDescriptorHeaps(&[Some(self.views.clone())]);
            list.SetGraphicsRootSignature(&self.root);
            list.SetGraphicsRootDescriptorTable(1, self.views.GetGPUDescriptorHandleForHeapStart());
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.desc.Width as f32,
                Height: self.desc.Height as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]);
            list.RSSetScissorRects(&[windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: self.desc.Width as i32,
                bottom: self.desc.Height as i32,
            }]);
        }
    }
}

/// The description of the one-sample images every slice's samples of a texture of `desc` are read into and written
/// from, made with no flags, or None for a format whose samples locum cannot read: their copyable footprints are the
/// canonical layout's ([`crate::layout::multisampled`]).
pub fn images_desc(desc: &D3D12_RESOURCE_DESC) -> Option<D3D12_RESOURCE_DESC> {
    let samples = format::samples(desc.Format.0 as u32)?;
    Some(D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: desc.Width,
        Height: desc.Height,
        DepthOrArraySize: (u32::from(desc.DepthOrArraySize) * desc.SampleDesc.Count.max(1))
            .try_into()
            .ok()?,
        MipLevels: 1,
        Format: DXGI_FORMAT(samples.image() as i32),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_NONE,
    })
}

/// A transition of every subresource of `resource`.
fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // Borrowed: the barrier holds no reference of its own.
                pResource: unsafe { core::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// Reads every sample of a texture (or of one slice of it) into one-sample images (see [`crate::samples`]): a copy
/// into a twin ([`Reader::copy`], which leaves a list's state alone, so it can go in a game's list), then the draws
/// out of the twin ([`Reader::draw`]) on a list of the caller's.
pub struct Reader {
    passes: Passes,
    twin: ID3D12Resource,
    images: ID3D12Resource,
    pipeline: ID3D12PipelineState,
}

impl Reader {
    /// What reading a texture of `desc` (multisampled; or of one sample and one mip, its sample 0) needs, made on
    /// `device`: every slice's samples, or only slice `slice`'s.
    ///
    /// # Safety
    /// `device` is a live device.
    pub unsafe fn new(
        device: &ID3D12Device,
        desc: &D3D12_RESOURCE_DESC,
        slice: Option<u32>,
    ) -> Result<Reader, String> {
        let passes = unsafe { Passes::new(device, desc, slice, D3D12_DESCRIPTOR_HEAP_TYPE_RTV) }?;
        // A multisampled resource is a render target or a depth-stencil.
        let flags = match passes.samples {
            Samples::DepthStencil { .. } => D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            _ => D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
        };
        let twin = unsafe { passes.twin(device, flags, D3D12_RESOURCE_STATE_COPY_DEST) }?;
        let image_desc = passes.images_desc(D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET);
        let images =
            unsafe { Passes::committed(device, &image_desc, D3D12_RESOURCE_STATE_RENDER_TARGET, "images") }?;
        let ms = passes.multisampled();
        // The views take every slice of the twin: a pass names its slice.
        let all = (u32::from(desc.DepthOrArraySize), ms);
        match passes.samples {
            Samples::Bits { view, .. } | Samples::Floats { view, .. } => unsafe {
                passes.view(device, 0, Some(&twin), view, (all.0, all.1, 0));
                passes.view(device, 1, None, view, (all.0, all.1, 0));
            },
            Samples::DepthStencil { depth, stencil, .. } => unsafe {
                passes.view(device, 0, Some(&twin), depth, (all.0, all.1, 0));
                passes.view(device, 1, Some(&twin), stencil, (all.0, all.1, 1));
            },
        }
        for image in 0..passes.images() {
            unsafe { passes.render_target(device, image, &images, passes.samples.image(), image, false) };
        }
        let pipeline = unsafe {
            passes.pipeline(
                device,
                programs::read(passes.samples, ms),
                &[passes.samples.image()],
                None,
                false,
            )
        }?;
        Ok(Reader {
            passes,
            twin,
            images,
            pipeline,
        })
    }

    /// The images the samples are drawn into, in COPY_SOURCE after [`Reader::draw`]: image `slice * samples +
    /// sample` of the slices read, whose subresources in order hold the canonical layout's bytes
    /// ([`crate::layout::multisampled`]).
    pub fn images(&self) -> &ID3D12Resource {
        &self.images
    }

    /// The images' description, for their copyable footprints.
    pub fn images_desc(&self) -> D3D12_RESOURCE_DESC {
        self.passes.images_desc(D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET)
    }

    /// Records on `list` the copy of the slices read of `texture` (each plane of them in COPY_SOURCE) into the
    /// twin, whole subresource by subresource: a copy, which changes none of `list`'s state.
    ///
    /// # Safety
    /// `texture` is the texture of the description this was made for, on the same device as `list`; the twin is in
    /// COPY_DEST, as it is made and as [`Reader::restore`] leaves it.
    pub unsafe fn copy(&self, list: &ID3D12GraphicsCommandList, texture: &ID3D12Resource) {
        let p = &self.passes;
        let array = u32::from(p.desc.DepthOrArraySize);
        let planes = match p.samples {
            Samples::DepthStencil { .. } => 2,
            _ => 1,
        };
        let at = |resource: &ID3D12Resource, sub| D3D12_TEXTURE_COPY_LOCATION {
            // Borrowed: the location holds no reference of its own.
            pResource: unsafe { core::mem::transmute_copy(resource) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: sub,
            },
        };
        for plane in 0..planes {
            for slice in p.first..p.first + p.slices() {
                // One mip: a subresource is a plane's slice.
                let sub = plane * array + slice;
                unsafe { list.CopyTextureRegion(&at(&self.twin, sub), 0, 0, 0, &at(texture, sub), None) };
            }
        }
    }

    /// Records on `list` (a direct list, run after the copy's) the draws of the twin's samples into the images, which
    /// it leaves in COPY_SOURCE. `list`'s descriptor heaps, root signature and pipeline are then the passes'.
    ///
    /// # Safety
    /// This lives until the GPU has run `list`.
    pub unsafe fn draw(&self, list: &ID3D12GraphicsCommandList) {
        let p = &self.passes;
        unsafe {
            list.ResourceBarrier(&[transition(
                &self.twin,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )]);
            p.bind(list);
            list.SetPipelineState(&self.pipeline);
            for at in 0..p.slices() {
                for sample in 0..p.count() {
                    let image = at * p.count() + sample;
                    list.OMSetRenderTargets(1, Some(&p.target(image)), false, None);
                    draw(list, programs::read_k(p.first + at, sample));
                }
            }
            list.ResourceBarrier(&[transition(
                &self.images,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
        }
    }

    /// Records on `list`, after the images were copied, the transitions back to the states [`Reader::copy`] and
    /// [`Reader::draw`] begin from, so the passes can run again (a game's list executed again).
    ///
    /// # Safety
    /// After [`Reader::draw`], and the copies of the images.
    pub unsafe fn restore(&self, list: &ID3D12GraphicsCommandList) {
        unsafe {
            list.ResourceBarrier(&[
                transition(
                    &self.twin,
                    D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ),
                transition(
                    &self.images,
                    D3D12_RESOURCE_STATE_COPY_SOURCE,
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                ),
            ])
        };
    }

    /// [`Reader::copy`] then [`Reader::draw`], on one direct list of the caller's.
    ///
    /// # Safety
    /// As theirs.
    pub unsafe fn record(&self, list: &ID3D12GraphicsCommandList, texture: &ID3D12Resource) {
        unsafe {
            self.copy(list, texture);
            self.draw(list);
        }
    }
}

/// Writes one-sample images (as [`Reader`] reads them) into every sample of a texture.
pub struct Writer {
    passes: Passes,
    twin: ID3D12Resource,
    images: ID3D12Resource,
    /// The colour's or the depth's pipeline, then one per stencil bit.
    pipelines: Vec<ID3D12PipelineState>,
}

impl Writer {
    /// What writing a texture of `desc` (multisampled; or of one sample and one mip) needs, made on `device`.
    ///
    /// # Safety
    /// `device` is a live device.
    pub unsafe fn new(device: &ID3D12Device, desc: &D3D12_RESOURCE_DESC) -> Result<Writer, String> {
        let depth_stencil = matches!(
            format::samples(desc.Format.0 as u32),
            Some(Samples::DepthStencil { .. })
        );
        let kind = if depth_stencil {
            D3D12_DESCRIPTOR_HEAP_TYPE_DSV
        } else {
            D3D12_DESCRIPTOR_HEAP_TYPE_RTV
        };
        let passes = unsafe { Passes::new(device, desc, None, kind) }?;
        let ms = passes.multisampled();
        let slices = passes.slices();
        let image_desc = passes.images_desc(D3D12_RESOURCE_FLAG_NONE);
        let images =
            unsafe { Passes::committed(device, &image_desc, D3D12_RESOURCE_STATE_COPY_DEST, "images") }?;
        let (colour, stencil) = programs::write(passes.samples);
        let (twin, pipelines) = match passes.samples {
            Samples::Bits { view, .. } | Samples::Floats { view, .. } => {
                let twin = unsafe {
                    passes.twin(
                        device,
                        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                    )
                }?;
                unsafe {
                    passes.view(device, 0, Some(&images), view, (passes.images(), false, 0));
                    passes.view(device, 1, None, view, (passes.images(), false, 0));
                    for slice in 0..slices {
                        passes.render_target(device, slice, &twin, view, slice, ms);
                    }
                }
                let pipeline = unsafe { passes.pipeline(device, colour, &[view], None, ms) }?;
                (twin, vec![pipeline])
            }
            Samples::DepthStencil { .. } => {
                let twin = unsafe {
                    passes.twin(
                        device,
                        D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
                        D3D12_RESOURCE_STATE_DEPTH_WRITE,
                    )
                }?;
                // The family's depth-stencil format: D24_UNORM_S8_UINT or D32_FLOAT_S8X24_UINT.
                let dsv = passes.samples.twin() + 1;
                let image = passes.samples.image();
                unsafe {
                    passes.view(device, 0, Some(&images), image, (passes.images(), false, 0));
                    passes.view(device, 1, None, image, (passes.images(), false, 0));
                    for slice in 0..slices {
                        let mut desc = D3D12_DEPTH_STENCIL_VIEW_DESC {
                            Format: DXGI_FORMAT(dsv as i32),
                            ..Default::default()
                        };
                        if ms {
                            desc.ViewDimension = D3D12_DSV_DIMENSION_TEXTURE2DMSARRAY;
                            desc.Anonymous.Texture2DMSArray = D3D12_TEX2DMS_ARRAY_DSV {
                                FirstArraySlice: slice,
                                ArraySize: 1,
                            };
                        } else {
                            desc.ViewDimension = D3D12_DSV_DIMENSION_TEXTURE2DARRAY;
                            desc.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_DSV {
                                MipSlice: 0,
                                FirstArraySlice: slice,
                                ArraySize: 1,
                            };
                        }
                        device.CreateDepthStencilView(&twin, Some(&desc), passes.target(slice));
                    }
                }
                let depth = D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                    StencilEnable: false.into(),
                    ..Default::default()
                };
                let replace = D3D12_DEPTH_STENCILOP_DESC {
                    StencilFailOp: D3D12_STENCIL_OP_KEEP,
                    StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
                    StencilPassOp: D3D12_STENCIL_OP_REPLACE,
                    StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                };
                let mut pipelines =
                    vec![unsafe { passes.pipeline(device, colour, &[], Some((dsv, depth)), ms) }?];
                let stencil = stencil.expect("a depth-stencil family writes its stencil");
                for bit in 0..8 {
                    let state = D3D12_DEPTH_STENCIL_DESC {
                        DepthEnable: false.into(),
                        DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                        DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                        StencilEnable: true.into(),
                        StencilReadMask: 0xFF,
                        StencilWriteMask: 1 << bit,
                        FrontFace: replace,
                        BackFace: replace,
                    };
                    pipelines.push(unsafe { passes.pipeline(device, stencil, &[], Some((dsv, state)), ms) }?);
                }
                (twin, pipelines)
            }
        };
        Ok(Writer {
            passes,
            twin,
            images,
            pipelines,
        })
    }

    /// The images to fill with the samples (in COPY_DEST, canonical layout's order: image `slice * samples +
    /// sample`) before [`Writer::record`].
    pub fn images(&self) -> &ID3D12Resource {
        &self.images
    }

    /// The images' description, for their copyable footprints.
    pub fn images_desc(&self) -> D3D12_RESOURCE_DESC {
        self.passes.images_desc(D3D12_RESOURCE_FLAG_NONE)
    }

    /// Records the passes on `list` (a direct list, after the copies that fill the images): every sample of the
    /// twin is drawn from the images, and the twin copied into `texture`, every subresource in COPY_DEST. `list`'s
    /// descriptor heaps, root signature and pipeline are then the passes'.
    ///
    /// # Safety
    /// `texture` is the texture of the description this was made for, on the same device as `list`; this lives until
    /// the GPU has run `list`, and runs once.
    pub unsafe fn record(&self, list: &ID3D12GraphicsCommandList, texture: &ID3D12Resource) {
        let p = &self.passes;
        let depth_stencil = self.pipelines.len() > 1;
        let drawn = if depth_stencil {
            D3D12_RESOURCE_STATE_DEPTH_WRITE
        } else {
            D3D12_RESOURCE_STATE_RENDER_TARGET
        };
        unsafe {
            list.ResourceBarrier(&[transition(
                &self.images,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            )]);
            p.bind(list);
            for slice in 0..p.slices() {
                let target = p.target(slice);
                if depth_stencil {
                    list.OMSetRenderTargets(0, None, false, Some(&target));
                    list.ClearDepthStencilView(
                        target,
                        D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
                        0.0,
                        0,
                        None,
                    );
                    list.OMSetStencilRef(0xFF);
                } else {
                    list.OMSetRenderTargets(1, Some(&target), false, None);
                }
                for sample in 0..p.count() {
                    let image = slice * p.count() + sample;
                    list.SetPipelineState(&self.pipelines[0]);
                    draw(list, programs::write_k(image, sample, None));
                    for (bit, pipeline) in self.pipelines[1..].iter().enumerate() {
                        list.SetPipelineState(pipeline);
                        draw(list, programs::write_k(image, sample, Some(bit as u32)));
                    }
                }
            }
            list.ResourceBarrier(&[transition(&self.twin, drawn, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
            list.CopyResource(texture, &self.twin);
        }
    }
}

/// One pass's triangle, its `k` in root constant 0 (see [`programs`]).
unsafe fn draw(list: &ID3D12GraphicsCommandList, k: u32) {
    unsafe {
        list.SetGraphicsRoot32BitConstant(0, k, 0);
        list.DrawInstanced(3, 1, 0, 0);
    }
}
