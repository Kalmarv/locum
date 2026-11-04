//! Two attachments consumed during recording, a cached bundle, and actual GPU output.
use super::*;

unsafe fn compile(source: &[u8], target: PCSTR) -> ID3DBlob {
    let mut code = None;
    let mut errors = None;
    unsafe {
        Fxc::D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            None,
            None,
            None,
            s!("main"),
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    }
    .unwrap();
    code.unwrap()
}
unsafe fn transition(
    list: &ID3D12GraphicsCommandList,
    r: &ID3D12Resource,
    from: D3D12_RESOURCE_STATES,
    to: D3D12_RESOURCE_STATES,
) {
    let mut barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(r.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: from,
                StateAfter: to,
            }),
        },
        ..Default::default()
    };
    unsafe {
        list.ResourceBarrier(std::slice::from_ref(&barrier));
        ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
    }
}
pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
) {
    unsafe {
        let vs=compile(b"float4 main(uint id:SV_VertexID):SV_Position { float2 p[3]={float2(-1,-1),float2(-1,3),float2(3,-1)}; return float4(p[id],0,1); }",s!("vs_5_0"));
        let ps=compile(b"struct O {float4 a:SV_Target0;float4 b:SV_Target1;}; O main(){O o;o.a=float4(1,0,0,1);o.b=float4(0,1,0,1);return o;}",s!("ps_5_0"));
        let mut blob = None;
        let sampler = D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            MaxAnisotropy: 1,
            ComparisonFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            MaxLOD: f32::MAX,
            ShaderRegister: 3,
            RegisterSpace: 7,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
            ..Default::default()
        };
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC {
                NumStaticSamplers: 1,
                pStaticSamplers: &sampler,
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
        let rt = D3D12_RENDER_TARGET_BLEND_DESC {
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_ZERO,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_ZERO,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: 15,
            ..Default::default()
        };
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
                RenderTarget: [rt; 8],
                ..Default::default()
            },
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 2,
            RTVFormats: [
                DXGI_FORMAT_R8G8B8A8_UNORM,
                DXGI_FORMAT_R8G8B8A8_UNORM,
                DXGI_FORMAT_UNKNOWN,
                DXGI_FORMAT_UNKNOWN,
                DXGI_FORMAT_UNKNOWN,
                DXGI_FORMAT_UNKNOWN,
                DXGI_FORMAT_UNKNOWN,
                DXGI_FORMAT_UNKNOWN,
            ],
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let pso: ID3D12PipelineState = device.CreateGraphicsPipelineState(&p).unwrap();
        ManuallyDrop::drop(&mut p.pRootSignature);
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 8,
            Height: 8,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ..Default::default()
        };
        let targets: Vec<ID3D12Resource> = (0..2)
            .map(|_| {
                let mut r = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &desc,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        None,
                        &mut r,
                    )
                    .unwrap();
                r.unwrap()
            })
            .collect();
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 2,
                ..Default::default()
            })
            .unwrap();
        let cpu = heap.GetCPUDescriptorHandleForHeapStart();
        let stride = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        for (i, target) in targets.iter().enumerate() {
            device.CreateRenderTargetView(
                target,
                None,
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: cpu.ptr + i * stride as usize,
                },
            );
        }
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let bundle_allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_BUNDLE)
            .unwrap();
        let bundle: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_BUNDLE, &bundle_allocator, &pso)
            .unwrap();
        bundle.SetGraphicsRootSignature(&root);
        bundle.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        bundle.DrawInstanced(3, 1, 0, 0);
        bundle.Close().unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, &pso)
            .unwrap();
        list.SetGraphicsRootSignature(&root);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            Width: 8.,
            Height: 8.,
            MaxDepth: 1.,
            ..Default::default()
        }]);
        list.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: 8,
            bottom: 8,
        }]);
        list.OMSetRenderTargets(2, Some(&cpu), true, None);
        list.ExecuteBundle(&bundle);
        // A bundle's topology and PSO carry into the caller.
        list.DrawInstanced(3, 1, 0, 0);
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut bytes = 0;
        device.GetCopyableFootprints(&desc, 0, 1, 0, Some(&mut footprint), None, None, Some(&mut bytes));
        bytes = bytes.next_multiple_of(u64::from(D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT));
        let mut readback: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_READBACK,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: bytes * 2,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut readback,
            )
            .unwrap();
        let readback = readback.unwrap();
        for (i, target) in targets.iter().enumerate() {
            transition(
                &list,
                target,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            let mut dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(readback.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: bytes * i as u64,
                        ..footprint
                    },
                },
            };
            let mut src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(target.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
            };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            ManuallyDrop::drop(&mut dst.pResource);
            ManuallyDrop::drop(&mut src.pResource);
        }
        list.Close().unwrap();
        // CPU RTV handles are consumed at recording: overwriting slot zero must
        // not change either native execution or the captured original attachment.
        device.CreateRenderTargetView(&targets[1], None, cpu);
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("graphics-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 10);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        let mut pointer = std::ptr::null_mut();
        readback.Map(0, None, Some(&mut pointer)).unwrap();
        for target in 0..2 {
            for y in 0..8 {
                for x in 0..8 {
                    let pixel = std::slice::from_raw_parts(
                        pointer
                            .cast::<u8>()
                            .add(target * bytes as usize + y * footprint.Footprint.RowPitch as usize + x * 4),
                        4,
                    );
                    assert_eq!(
                        pixel,
                        if target == 0 {
                            &[255, 0, 0, 255]
                        } else {
                            &[0, 255, 0, 255]
                        }
                    );
                }
            }
        }
        readback.Unmap(0, None);
        let path = dir.join("captures").join(label).join("capture.db");
        let until = Instant::now() + Duration::from_secs(15);
        while !path.is_file() {
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(10));
        }
        let db = rusqlite::Connection::open(&path).unwrap();
        for (sql, n) in [
            ("SELECT count(*) FROM draws", 2),
            ("SELECT count(*) FROM d12_executions", 2),
            (
                "SELECT count(*) FROM d12_executions WHERE parent_bundle_execution IS NOT NULL",
                1,
            ),
            (
                "SELECT count(*) FROM draws WHERE vs IS NOT NULL AND ps IS NOT NULL AND topology=4 AND rasterizer IS NOT NULL AND blend IS NOT NULL",
                2,
            ),
            ("SELECT count(*) FROM bindings WHERE kind='rtv'", 4),
            (
                "SELECT count(*) FROM bindings WHERE kind='sampler' AND stage='ps' AND slot=3 AND space=7 AND state IS NOT NULL",
                2,
            ),
            (
                "SELECT count(DISTINCT resource) FROM bindings WHERE kind='rtv'",
                2,
            ),
            (
                "SELECT count(*) FROM views v LEFT JOIN d12_descriptor_versions d ON d.id=v.id WHERE d.id IS NULL",
                0,
            ),
            (
                "SELECT count(*) FROM states WHERE kind='blend' AND json_type(json,'$.targets')='array'",
                1,
            ),
            (
                "SELECT count(*) FROM states WHERE kind='rasterizer' AND json_extract(json,'$.fill')=3",
                1,
            ),
            ("SELECT count(*) FROM viewports WHERE width=8 AND height=8", 2),
            ("SELECT count(*) FROM scissors WHERE right=8 AND bottom=8", 2),
            (
                "SELECT count(*) FROM passes WHERE first_draw=1 AND last_draw=2",
                1,
            ),
            ("SELECT count(*) FROM pragma_foreign_key_check", 0),
        ] {
            assert_eq!(db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap(), n, "{sql}");
        }
        debug_errors(device);
        println!(
            "D3D12 graphics capture: cached bundle inheritance, two consumed RTV versions, GPU red/green outputs and inspector projection verified: {}",
            path.display()
        );
    }
}
