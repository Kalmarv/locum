//! Multisampled textures whose samples differ, captured bit for bit: their first-use contents (what an earlier frame
//! drew into every sample) and their pass-end targets (the captured frame draws sample 3 again). An R8G8B8A8 array of
//! two slices drawn through its UINT view, with SNORM's 0x80 in every texel; an R11G11B10_FLOAT, which has no UINT
//! member; D24_UNORM_S8_UINT with depth's edges 0, 1 and 0xFFFFFF and stencils 0x00, 0xA5 and 0xFF; D32_FLOAT_S8X24_UINT,
//! D32_FLOAT and D16_UNORM. The colours are drawn at sample frequency, each depth-stencil sample by a draw of its own
//! (the pipeline's sample mask).
use super::contents::{committed, desc};
use super::graphics::compile;
use super::sample_values::*;
use super::*;

const RENDER_TARGET: i32 = 0x4;
const DEPTH_WRITE: i32 = 0x10;

/// A pipeline of `ps` for `kind`, drawing the samples in `mask`.
unsafe fn pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    (vs, ps): (&ID3DBlob, &ID3DBlob),
    kind: Kind,
    mask: u32,
) -> ID3D12PipelineState {
    let (_, view, _) = kind.formats();
    let replace = D3D12_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D12_STENCIL_OP_KEEP,
        StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
        StencilPassOp: D3D12_STENCIL_OP_REPLACE,
        StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
    };
    let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
    if !kind.depth() {
        formats[0] = view;
    }
    unsafe {
        let mut p = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            VS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: vs.GetBufferPointer(),
                BytecodeLength: vs.GetBufferSize(),
            },
            PS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: ps.GetBufferPointer(),
                BytecodeLength: ps.GetBufferSize(),
            },
            BlendState: D3D12_BLEND_DESC {
                RenderTarget: [D3D12_RENDER_TARGET_BLEND_DESC {
                    RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
                    ..Default::default()
                }; 8],
                ..Default::default()
            },
            SampleMask: mask,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: kind.depth().into(),
                DepthWriteMask: if kind.depth() {
                    D3D12_DEPTH_WRITE_MASK_ALL
                } else {
                    D3D12_DEPTH_WRITE_MASK_ZERO
                },
                DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                StencilEnable: kind.stencil().into(),
                StencilReadMask: 0xFF,
                StencilWriteMask: 0xFF,
                FrontFace: replace,
                BackFace: replace,
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: u32::from(!kind.depth()),
            RTVFormats: formats,
            DSVFormat: if kind.depth() { view } else { DXGI_FORMAT_UNKNOWN },
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: SAMPLES,
                Quality: 0,
            },
            ..Default::default()
        };
        let pso = device
            .CreateGraphicsPipelineState(&p)
            .unwrap_or_else(|e| panic!("samples: a pipeline of {:?}: {e}", view));
        ManuallyDrop::drop(&mut p.pRootSignature);
        pso
    }
}

/// One multisampled texture of the fixture's, with its views (one a slice) and pipelines.
struct Texture {
    kind: Kind,
    /// Held for its views: the checks read it from the capture.
    _resource: ID3D12Resource,
    views: Vec<D3D12_CPU_DESCRIPTOR_HANDLE>,
    /// Every sample at once for a colour, then sample 3; a depth-stencil's one a sample.
    pipelines: Vec<ID3D12PipelineState>,
}

impl Texture {
    /// Records the draws of every sample of every slice (`alt`: sample 3 only, as the captured frame's).
    unsafe fn draw(&self, list: &ID3D12GraphicsCommandList, alt: bool) {
        let (_, _, slices) = self.kind.formats();
        unsafe {
            for (slice, view) in self.views.iter().enumerate().take(slices as usize) {
                if self.kind.depth() {
                    list.OMSetRenderTargets(0, None, false, Some(view));
                } else {
                    list.OMSetRenderTargets(1, Some(view), false, None);
                }
                let samples: Vec<u32> = if alt { vec![3] } else { (0..SAMPLES).collect() };
                for s in samples {
                    let (pipeline, depths) = if self.kind.depth() {
                        (&self.pipelines[s as usize], self.kind.depths(s, alt))
                    } else {
                        (&self.pipelines[usize::from(alt)], [0.0; 8])
                    };
                    let mut constants = vec![slice as u32, u32::from(alt), 0, 0];
                    constants.extend(depths.map(f32::to_bits));
                    list.SetPipelineState(pipeline);
                    list.SetGraphicsRoot32BitConstants(
                        0,
                        constants.len() as u32,
                        constants.as_ptr().cast(),
                        0,
                    );
                    list.OMSetStencilRef(u32::from(stencil(s, alt)));
                    list.DrawInstanced(3, 1, 0, 0);
                    if !self.kind.depth() {
                        // A colour's draw at sample frequency writes every sample it covers.
                        break;
                    }
                }
            }
        }
    }
}

/// Fence values `first` and `first + 1`.
pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    first: u64,
) {
    unsafe {
        let vs = compile(VERTICES.as_bytes(), s!("vs_5_0"));
        let program = |body: &str| compile(format!("{CONSTANTS} {body}").as_bytes(), s!("ps_5_0"));
        let (colour_ps, floats_ps, depth_ps) = (program(COLOUR), program(FLOATS), program(DEPTH));
        let parameter = D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 12,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        };
        let mut blob = None;
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: 1,
                pParameters: &parameter,
                ..Default::default()
            },
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut blob,
            None,
        )
        .unwrap();
        let blob = blob.unwrap();
        let root: ID3D12RootSignature = device
            .CreateRootSignature(
                0,
                std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()),
            )
            .unwrap();
        let heap = |kind, n| -> ID3D12DescriptorHeap {
            device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: n,
                    ..Default::default()
                })
                .unwrap()
        };
        let (rtvs, dsvs) = (
            heap(D3D12_DESCRIPTOR_HEAP_TYPE_RTV, 3),
            heap(D3D12_DESCRIPTOR_HEAP_TYPE_DSV, 4),
        );
        let (mut rtv, mut dsv) = (0usize, 0usize);
        let mut textures = Vec::new();
        for kind in Kind::ALL {
            let (format, view, slices) = kind.formats();
            let mut levels = D3D12_FEATURE_DATA_MULTISAMPLE_QUALITY_LEVELS {
                Format: view,
                SampleCount: SAMPLES,
                ..Default::default()
            };
            device
                .CheckFeatureSupport(
                    D3D12_FEATURE_MULTISAMPLE_QUALITY_LEVELS,
                    (&raw mut levels).cast(),
                    size_of_val(&levels) as u32,
                )
                .unwrap();
            assert!(
                levels.NumQualityLevels > 0,
                "samples: {view:?} has no {SAMPLES} samples here"
            );
            let flags = if kind.depth() {
                D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL
            } else {
                D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
            };
            let state = if kind.depth() { DEPTH_WRITE } else { RENDER_TARGET };
            let resource = committed(
                device,
                D3D12_HEAP_TYPE_DEFAULT,
                &desc(
                    D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    format,
                    (u64::from(WIDTH), HEIGHT),
                    (1, slices as u16, SAMPLES),
                    flags,
                ),
                D3D12_RESOURCE_STATES(state),
            );
            let mut views = Vec::new();
            for slice in 0..slices {
                if kind.depth() {
                    let mut handle = dsvs.GetCPUDescriptorHandleForHeapStart();
                    handle.ptr += dsv
                        * device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV) as usize;
                    dsv += 1;
                    device.CreateDepthStencilView(
                        &resource,
                        Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                            Format: view,
                            ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2DMS,
                            ..Default::default()
                        }),
                        handle,
                    );
                    views.push(handle);
                } else {
                    let mut handle = rtvs.GetCPUDescriptorHandleForHeapStart();
                    handle.ptr += rtv
                        * device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
                    rtv += 1;
                    let mut d = D3D12_RENDER_TARGET_VIEW_DESC {
                        Format: view,
                        ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2DMSARRAY,
                        ..Default::default()
                    };
                    d.Anonymous.Texture2DMSArray = D3D12_TEX2DMS_ARRAY_RTV {
                        FirstArraySlice: slice,
                        ArraySize: 1,
                    };
                    device.CreateRenderTargetView(&resource, Some(&d), handle);
                    views.push(handle);
                }
            }
            let pipelines = match kind {
                Kind::Colour | Kind::Floats => {
                    let ps = if kind == Kind::Colour {
                        &colour_ps
                    } else {
                        &floats_ps
                    };
                    vec![
                        pipeline(device, &root, (&vs, ps), kind, u32::MAX),
                        pipeline(device, &root, (&vs, ps), kind, 1 << 3),
                    ]
                }
                _ => (0..SAMPLES)
                    .map(|s| pipeline(device, &root, (&vs, &depth_ps), kind, 1 << s))
                    .collect(),
            };
            textures.push(Texture {
                kind,
                _resource: resource,
                views,
                pipelines,
            });
        }
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        let record = |alt: bool| {
            list.SetGraphicsRootSignature(&root);
            list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            list.RSSetViewports(&[D3D12_VIEWPORT {
                Width: WIDTH as f32,
                Height: HEIGHT as f32,
                MaxDepth: 1.,
                ..Default::default()
            }]);
            list.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right: WIDTH as i32,
                bottom: HEIGHT as i32,
            }]);
            for t in &textures {
                t.draw(&list, alt);
            }
            list.Close().unwrap();
        };
        // A frame before the capture draws every sample: the captured frame's first use finds them.
        record(false);
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, first);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("samples-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\nskip memory\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        allocator.Reset().unwrap();
        list.Reset(&allocator, None).unwrap();
        // Each target's pass ends where the next is bound, the last where the list closes.
        record(true);
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, first + 1);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        let path = dir.join("captures").join(&label).join("capture.db");
        let until = Instant::now() + Duration::from_secs(30);
        while !path.is_file() {
            assert!(
                Instant::now() < until,
                "capture writer timeout: {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let db = rusqlite::Connection::open(&path).unwrap();
        let blobs = locum_capture::blobs::Blobs::new(dir.join("blobs"));
        for kind in Kind::ALL {
            let (format, view, slices) = kind.formats();
            let (id, content, note): (i64, Option<String>, Option<String>) = db
                .query_row(
                    "SELECT id, content, content_note FROM resources WHERE samples = ?1 AND format = ?2",
                    rusqlite::params![SAMPLES, format.0],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap_or_else(|e| panic!("samples: the {view:?} texture: {e}"));
            // As the frame before left every sample.
            let content =
                content.unwrap_or_else(|| panic!("samples: #{id} ({view:?}) has no contents: {note:?}"));
            let bytes = blobs.get(&content).unwrap();
            let want: Vec<u8> = (0..slices).flat_map(|slice| kind.slice(slice, false)).collect();
            assert!(
                bytes == want,
                "samples: #{id} ({view:?}): its contents are not every sample's bits ({} bytes, {} expected; first \
                 difference at byte {:?})",
                bytes.len(),
                want.len(),
                bytes.iter().zip(&want).position(|(a, b)| a != b)
            );
            // A row a subresource, its samples stacked.
            let rows: Vec<(u32, u32, i64)> = db
                .prepare("SELECT sub, height, size FROM subresources WHERE resource = ?1 ORDER BY sub")
                .unwrap()
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            let size = want.len() as i64 / i64::from(slices);
            assert_eq!(
                rows,
                (0..slices)
                    .map(|s| (s, HEIGHT * SAMPLES, size))
                    .collect::<Vec<_>>(),
                "samples: #{id} ({view:?})'s subresources"
            );
            // Where each pass ended: sample 3 as the captured frame drew it.
            let targets: Vec<(u32, u32, u32, Option<String>)> = db
                .prepare("SELECT sub, width, height, blob FROM targets WHERE resource = ?1 ORDER BY sub")
                .unwrap()
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .map(|r| r.unwrap())
                .collect();
            assert_eq!(
                targets.len(),
                slices as usize,
                "samples: #{id} ({view:?})'s pass ends: {targets:?}"
            );
            for (slice, (sub, width, height, blob)) in targets.into_iter().enumerate() {
                assert_eq!(
                    (sub, width, height),
                    (slice as u32, WIDTH, HEIGHT * SAMPLES),
                    "samples: #{id} ({view:?}) slice {slice}: its samples' images stacked"
                );
                let bytes = blobs.get(&blob.expect("stored")).unwrap();
                let want = kind.slice(slice as u32, true);
                assert!(
                    bytes == want,
                    "samples: #{id} ({view:?}) slice {slice}: the pass end is not every sample's bits (first \
                     difference at byte {:?} of {})",
                    bytes.iter().zip(&want).position(|(a, b)| a != b),
                    want.len()
                );
            }
        }
    }
    println!(
        "D3D12 multisampled textures ({SAMPLES} samples, each different; RGBA8 UINT over 2 slices, R11G11B10, D24S8, \
         D32S8X24, D32, D16): first-use contents and pass-end targets every sample's bits"
    );
}

/// Copies of `sources` (resource, subresource) into one readback buffer: their footprints, and the buffer.
unsafe fn read_back(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    sources: &[(&ID3D12Resource, u32)],
) -> (Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>, ID3D12Resource) {
    unsafe {
        let mut footprints = Vec::new();
        let mut bytes = 0u64;
        for (r, sub) in sources {
            let mut f = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut rows = 0u32;
            device.GetCopyableFootprints(
                &r.GetDesc(),
                *sub,
                1,
                bytes,
                Some(&mut f),
                Some(&mut rows),
                None,
                None,
            );
            bytes = (f.Offset + u64::from(f.Footprint.RowPitch) * u64::from(rows))
                .next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT));
            footprints.push(f);
        }
        let readback = committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &super::contents::buffer_desc(bytes),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        for ((r, sub), f) in sources.iter().zip(&footprints) {
            list.CopyTextureRegion(
                &D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some(readback.clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: *f },
                },
                0,
                0,
                0,
                &D3D12_TEXTURE_COPY_LOCATION {
                    pResource: ManuallyDrop::new(Some((*r).clone())),
                    Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                    Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                        SubresourceIndex: *sub,
                    },
                },
                None,
            );
        }
        (footprints, readback)
    }
}

/// Texel `i` (row-major) of footprint `f` in `data`, `bytes` bytes of it.
fn at<'a>(data: &'a [u8], f: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT, i: u32, bytes: usize) -> &'a [u8] {
    let (x, y) = (i % f.Footprint.Width, i / f.Footprint.Width);
    let o = f.Offset as usize + y as usize * f.Footprint.RowPitch as usize + x as usize * bytes;
    &data[o..o + bytes]
}

/// Every one of D24's 2^24 depths, with a stencil each, written into a one-sample D24_UNORM_S8_UINT by locum's
/// passes (a float a depth, rounded by the GPU to 24 bits), read back as the planes hold them, and read again by
/// locum's passes (each depth taken as a float and back to its 24 bits): what both conversions do on this GPU, for
/// every value they can meet. Fence value `first`.
pub(super) unsafe fn conversions(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    first: u64,
) {
    use locum_capture::d3d12::samples::{Reader, Writer};
    const SIDE: u32 = 4096;
    let value = |k: u32| k | ((k.wrapping_mul(0x9E37_79B9) >> 24) << 24);
    unsafe {
        let d = desc(
            D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            DXGI_FORMAT_R24G8_TYPELESS,
            (u64::from(SIDE), SIDE),
            (1, 1, 1),
            D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
        );
        let texture = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &d,
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let writer = Writer::new(device, &d).unwrap_or_else(|e| panic!("conversions: a writer: {e}"));
        let images = writer.images().clone();
        let mut f = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut total = 0u64;
        device.GetCopyableFootprints(
            &writer.images_desc(),
            0,
            1,
            0,
            Some(&mut f),
            None,
            None,
            Some(&mut total),
        );
        let upload = committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &super::contents::buffer_desc(total),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut p = core::ptr::null_mut();
        upload.Map(0, None, Some(&mut p)).unwrap();
        for y in 0..SIDE {
            let row = p
                .cast::<u8>()
                .add(f.Offset as usize + (y * f.Footprint.RowPitch) as usize)
                .cast::<u32>();
            for x in 0..SIDE {
                *row.add(x as usize) = value(y * SIDE + x);
            }
        }
        upload.Unmap(0, None);
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        list.CopyTextureRegion(
            &D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(images.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            },
            0,
            0,
            0,
            &D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(upload.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
            },
            None,
        );
        writer.record(&list, &texture);
        super::graphics::transition(
            &list,
            &texture,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        // The planes as the texture holds them: depth in each texel's low 24 bits, then the stencils.
        let (planes, raw) = read_back(device, &list, &[(&texture, 0), (&texture, 1)]);
        let reader = Reader::new(device, &d, None).unwrap_or_else(|e| panic!("conversions: a reader: {e}"));
        reader.record(&list, &texture);
        let (read, again) = read_back(device, &list, &[(reader.images(), 0)]);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, first);
        let mapped = |r: &ID3D12Resource| -> Vec<u8> {
            let mut p = core::ptr::null_mut();
            r.Map(0, None, Some(&mut p)).unwrap();
            let bytes = std::slice::from_raw_parts(p.cast::<u8>(), r.GetDesc().Width as usize).to_vec();
            r.Unmap(0, None);
            bytes
        };
        let (raw, again) = (mapped(&raw), mapped(&again));
        let mut written = Vec::new();
        let mut reread = Vec::new();
        for k in 0..SIDE * SIDE {
            let want = value(k);
            let depth = u32::from_le_bytes(at(&raw, &planes[0], k, 4).try_into().unwrap()) & 0xFF_FFFF;
            let stencil = u32::from(at(&raw, &planes[1], k, 1)[0]);
            if depth | stencil << 24 != want {
                written.push((k, depth | stencil << 24));
            }
            let read = u32::from_le_bytes(at(&again, &read[0], k, 4).try_into().unwrap());
            if read != want {
                reread.push((k, read));
            }
        }
        assert!(
            written.is_empty(),
            "conversions: {} of 2^24 depths or stencils written wrong, first {:x?}",
            written.len(),
            &written[..written.len().min(4)]
        );
        assert!(
            reread.is_empty(),
            "conversions: {} of 2^24 depths or stencils read back wrong, first {:x?}",
            reread.len(),
            &reread[..reread.len().min(4)]
        );
    }
    println!(
        "D3D12 D24_UNORM_S8_UINT: all 2^24 depths, with stencils, written and read back exactly by the sample passes"
    );
}
