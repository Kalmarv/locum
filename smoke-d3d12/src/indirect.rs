//! ExecuteIndirect whose argument records hold GPU addresses: each record sets a vertex buffer view and a root CBV,
//! then draws. Replay makes its own buffers, at other addresses than the game's, so the records only draw what they
//! drew in the game if replay rewrites those addresses. One argument buffer the CPU wrote before the frame; the
//! other a dispatch of the frame copies from a buffer of addresses, with a count buffer. The vertex buffer is in a
//! default heap and nothing but the records names it, as a game's mesh pool would be. Colours are 0 or 1 in every
//! channel, so hardware and WARP write the same bytes.
use super::*;

const RGBA8: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;
/// A record: VERTEX_BUFFER_VIEW (16 bytes), CONSTANT_BUFFER_VIEW (8), DRAW (16).
const STRIDE: usize = 40;
const COLOURS: [[f32; 4]; 4] = [
    [1., 0., 0., 1.],
    [0., 0., 1., 1.],
    [0., 1., 0., 1.],
    [1., 1., 1., 1.],
];

/// The bytes of two records: the left half of the target in `colours.0` and the right half in `colours.1`.
fn records(vertices: u64, constants: u64, colours: (u64, u64)) -> Vec<u8> {
    let mut out = Vec::new();
    for (half, colour) in [(0u64, colours.0), (1, colours.1)] {
        out.extend((vertices + half * 256).to_le_bytes());
        out.extend(48u32.to_le_bytes());
        out.extend(8u32.to_le_bytes());
        out.extend((constants + colour * 256).to_le_bytes());
        for word in [6u32, 1, 0, 0] {
            out.extend(word.to_le_bytes());
        }
    }
    assert_eq!(out.len(), 2 * STRIDE);
    out
}

unsafe fn upload(device: &ID3D12Device, bytes: &[u8], size: u64) -> ID3D12Resource {
    unsafe {
        let r = contents::committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &contents::buffer_desc(size),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut p = core::ptr::null_mut();
        r.Map(0, None, Some(&mut p)).unwrap();
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        r.Unmap(0, None);
        r
    }
}

unsafe fn root_signature(
    device: &ID3D12Device,
    parameters: &[D3D12_ROOT_PARAMETER],
    flags: D3D12_ROOT_SIGNATURE_FLAGS,
) -> ID3D12RootSignature {
    unsafe {
        let mut blob = None;
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: parameters.len() as u32,
                pParameters: parameters.as_ptr(),
                Flags: flags,
                ..Default::default()
            },
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut blob,
            None,
        )
        .unwrap();
        let blob = blob.unwrap();
        device
            .CreateRootSignature(
                0,
                core::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()),
            )
            .unwrap()
    }
}

fn root_view(kind: D3D12_ROOT_PARAMETER_TYPE, register: u32) -> D3D12_ROOT_PARAMETER {
    D3D12_ROOT_PARAMETER {
        ParameterType: kind,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            Descriptor: D3D12_ROOT_DESCRIPTOR {
                ShaderRegister: register,
                RegisterSpace: 0,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    }
}

/// Fence values `first` to `first + 2` are used.
pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    first: u64,
) {
    unsafe {
        use graphics::{compile, transition};
        let vs = compile(
            b"float4 main(float2 p:POSITION):SV_Position { return float4(p,0,1); }",
            s!("vs_5_0"),
        );
        let ps = compile(
            b"cbuffer C:register(b0) { float4 colour; }; float4 main():SV_Target { return colour; }",
            s!("ps_5_0"),
        );
        let cs = compile(
            b"ByteAddressBuffer src:register(t0); RWByteAddressBuffer dst:register(u0); \
              [numthreads(20,1,1)] void main(uint i:SV_DispatchThreadID) { dst.Store(i*4, src.Load(i*4)); }",
            s!("cs_5_0"),
        );
        let root = root_signature(
            device,
            &[root_view(D3D12_ROOT_PARAMETER_TYPE_CBV, 0)],
            D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
        );
        let compute_root = root_signature(
            device,
            &[
                root_view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
                root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0),
            ],
            D3D12_ROOT_SIGNATURE_FLAG_NONE,
        );
        let element = D3D12_INPUT_ELEMENT_DESC {
            SemanticName: s!("POSITION"),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        };
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
        let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
        formats[0] = RGBA8;
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
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: &element,
                NumElements: 1,
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: formats,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let pso: ID3D12PipelineState = device.CreateGraphicsPipelineState(&p).unwrap();
        ManuallyDrop::drop(&mut p.pRootSignature);
        let mut c = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(compute_root.clone())),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: cs.GetBufferPointer(),
                BytecodeLength: cs.GetBufferSize(),
            },
            ..Default::default()
        };
        let copier: ID3D12PipelineState = device.CreateComputePipelineState(&c).unwrap();
        ManuallyDrop::drop(&mut c.pRootSignature);
        let arguments = [
            D3D12_INDIRECT_ARGUMENT_DESC {
                Type: D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW,
                Anonymous: D3D12_INDIRECT_ARGUMENT_DESC_0 {
                    VertexBuffer: D3D12_INDIRECT_ARGUMENT_DESC_0_0 { Slot: 0 },
                },
            },
            D3D12_INDIRECT_ARGUMENT_DESC {
                Type: D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW,
                Anonymous: D3D12_INDIRECT_ARGUMENT_DESC_0 {
                    ConstantBufferView: D3D12_INDIRECT_ARGUMENT_DESC_0_2 {
                        RootParameterIndex: 0,
                    },
                },
            },
            D3D12_INDIRECT_ARGUMENT_DESC {
                Type: D3D12_INDIRECT_ARGUMENT_TYPE_DRAW,
                ..Default::default()
            },
        ];
        let mut signature: Option<ID3D12CommandSignature> = None;
        device
            .CreateCommandSignature(
                &D3D12_COMMAND_SIGNATURE_DESC {
                    ByteStride: STRIDE as u32,
                    NumArgumentDescs: arguments.len() as u32,
                    pArgumentDescs: arguments.as_ptr(),
                    NodeMask: 0,
                },
                &root,
                &mut signature,
            )
            .unwrap();
        let signature = signature.unwrap();

        // Two quads, the left half of clip space at 0 and the right half at 256, in a default heap.
        let quad = |x0: f32, x1: f32| -> Vec<u8> {
            [[x0, -1.], [x0, 1.], [x1, -1.], [x1, -1.], [x0, 1.], [x1, 1.]]
                .iter()
                .flatten()
                .flat_map(|f: &f32| f.to_le_bytes())
                .collect()
        };
        let mut vertex_bytes = quad(-1., 0.);
        vertex_bytes.resize(256, 0);
        vertex_bytes.extend(quad(0., 1.));
        let vertices = contents::committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &contents::buffer_desc(512),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let staging = upload(device, &vertex_bytes, 512);
        let constant_bytes: Vec<u8> = COLOURS
            .iter()
            .flat_map(|c| {
                let mut b: Vec<u8> = c.iter().flat_map(|f| f.to_le_bytes()).collect();
                b.resize(256, 0);
                b
            })
            .collect();
        let constants = upload(device, &constant_bytes, 1024);
        let (vertex_va, constant_va) = (vertices.GetGPUVirtualAddress(), constants.GetGPUVirtualAddress());
        // Red left, blue right; then green left, white right through the dispatch's copy.
        let cpu_args = upload(device, &records(vertex_va, constant_va, (0, 1)), 256);
        let source = upload(device, &records(vertex_va, constant_va, (2, 3)), 256);
        let count = upload(device, &2u32.to_le_bytes(), 256);
        let gpu_args = contents::committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &contents::desc(
                D3D12_RESOURCE_DIMENSION_BUFFER,
                DXGI_FORMAT_UNKNOWN,
                (256, 1),
                (1, 1, 1),
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ),
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );

        // Before the frame: the vertices copied into their default heap.
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        list.CopyBufferRegion(&vertices, 0, &staging, 0, 512);
        transition(
            &list,
            &vertices,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER,
        );
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, first);

        let targets: Vec<ID3D12Resource> = (0..2)
            .map(|_| {
                contents::committed(
                    device,
                    D3D12_HEAP_TYPE_DEFAULT,
                    &contents::desc(
                        D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                        RGBA8,
                        (8, 8),
                        (1, 1, 1),
                        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                    ),
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                )
            })
            .collect();
        let rtvs: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 2,
                ..Default::default()
            })
            .unwrap();
        let start = rtvs.GetCPUDescriptorHandleForHeapStart();
        let increment = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
        let rtv = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + i * increment,
        };
        for (i, t) in targets.iter().enumerate() {
            device.CreateRenderTargetView(t, None, rtv(i));
        }
        let readback = contents::committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &contents::buffer_desc(2 * 65536),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        // The frame's list is recorded armed, so its pass ends and the dispatch's output are copied.
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("indirect-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, &pso)
            .unwrap();
        // The dispatch writes the second argument buffer from the addresses in `source`.
        list.SetPipelineState(&copier);
        list.SetComputeRootSignature(&compute_root);
        list.SetComputeRootShaderResourceView(0, source.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(1, gpu_args.GetGPUVirtualAddress());
        list.Dispatch(1, 1, 1);
        transition(
            &list,
            &gpu_args,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
        );
        list.SetPipelineState(&pso);
        list.SetGraphicsRootSignature(&root);
        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
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
        for (i, (args, count)) in [(&cpu_args, None), (&gpu_args, Some(&count))]
            .into_iter()
            .enumerate()
        {
            list.ClearRenderTargetView(rtv(i), &[0., 0., 0., 1.], None);
            list.OMSetRenderTargets(1, Some(&rtv(i)), false, None);
            list.ExecuteIndirect(&signature, 2, args, 0, count, 0);
        }
        let mut footprints = Vec::new();
        for (i, t) in targets.iter().enumerate() {
            transition(
                &list,
                t,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            footprints.push(contents::read(device, &list, t, &readback, i as u64 * 65536));
        }
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, first + 1);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        // The game's own pictures: what verify's replay must make again.
        let mut pointer = core::ptr::null_mut();
        readback.Map(0, None, Some(&mut pointer)).unwrap();
        for (i, f) in footprints.iter().enumerate() {
            for y in 0..8 {
                for x in 0..8 {
                    let at = f.Offset as usize + y * f.Footprint.RowPitch as usize + x * 4;
                    let pixel = core::slice::from_raw_parts(pointer.cast::<u8>().add(at), 4);
                    let colour = COLOURS[i * 2 + usize::from(x >= 4)].map(|c| (c * 255.) as u8);
                    assert_eq!(pixel, colour, "indirect target {i} at {x},{y}");
                }
            }
        }
        readback.Unmap(0, None);
        let path = dir.join("captures").join(&label).join("capture.db");
        let until = Instant::now() + Duration::from_secs(15);
        while !path.is_file() {
            assert!(Instant::now() < until, "no capture at {}", path.display());
            std::thread::sleep(Duration::from_millis(10));
        }
        let db = rusqlite::Connection::open(&path).unwrap();
        for (sql, n) in [
            ("SELECT count(*) FROM draws WHERE call = 'ExecuteIndirect'", 2),
            ("SELECT count(*) FROM draws WHERE call = 'Dispatch'", 1),
            (
                "SELECT count(*) FROM meta WHERE key = 'environment' AND json_extract(value, '$.adapter.luid') IS NOT NULL",
                1,
            ),
            // The vertex buffer only the records name: each ExecuteIndirect reads it, and its contents were copied
            // after the frame. The constants are an upload buffer, copied whole at the first submission.
            (
                "SELECT count(DISTINCT e.event) FROM event_resources e JOIN draws d ON d.event = e.event \
                 JOIN resources r ON r.id = e.resource \
                 WHERE d.call = 'ExecuteIndirect' AND r.content_source = 'after-frame' AND r.used = 1",
                2,
            ),
            (
                "SELECT count(*) FROM resources WHERE content_source = 'after-frame'",
                1,
            ),
            (
                "SELECT count(*) FROM gaps WHERE what = 'd3d12-indirect-arguments'",
                0,
            ),
        ] {
            assert_eq!(db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap(), n, "{sql}");
        }
        finish(queue, fence, first + 2);
        debug_errors(device);
        println!(
            "D3D12 ExecuteIndirect with vertex buffer and root CBV arguments, CPU- and GPU-written: red/blue and green/white halves drawn: {}",
            path.display()
        );
    }
}
