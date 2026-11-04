//! Queries: occlusion, binary occlusion, pipeline statistics and timestamps around draws of known coverage, resolved
//! into a readback buffer and into a default-heap one. The results the GPU gave the application go to
//! `queries-expected.bin` in locum's folder, for the replay's to be compared with (xtask): each event replays in a
//! list of its own, so replay counts each draw a game's query spans with a query of its own and adds them up.
use super::contents::{buffer_desc, committed, desc};
use super::graphics::compile;
use super::*;

const W: u32 = 64;
const H: u32 = 32;

/// Where the results are resolved in the readback buffer: the occlusion query, the two binary ones, the pipeline
/// statistics (eleven counters) and two timestamps.
pub(super) const AT_OCCLUSION: usize = 0;
pub(super) const AT_BINARY: usize = 8;
pub(super) const AT_STATISTICS: usize = 64;
pub(super) const AT_TIMESTAMPS: usize = 160;
const RESULTS: usize = AT_TIMESTAMPS + 16;

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
) {
    unsafe {
        let vs = compile(
            b"float4 main(uint id : SV_VertexID) : SV_Position { float2 p[3] = {float2(-1,-1), float2(-1,3), float2(3,-1)}; return float4(p[id], 0.5, 1); }",
            s!("vs_5_0"),
        );
        let ps = compile(
            b"float4 main() : SV_Target { return float4(1, 0, 1, 1); }",
            s!("ps_5_0"),
        );
        let mut blob = None;
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC::default(),
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
        let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
        formats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
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
                    RenderTargetWriteMask: 0xf,
                    ..Default::default()
                }; 8],
                ..Default::default()
            },
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: formats,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let pso: ID3D12PipelineState = device.CreateGraphicsPipelineState(&p).unwrap();
        ManuallyDrop::drop(&mut p.pRootSignature);
        let target = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &desc(
                D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                DXGI_FORMAT_R8G8B8A8_UNORM,
                (u64::from(W), H),
                (1, 1, 1),
                D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
            ),
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        let rtvs: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 1,
                ..Default::default()
            })
            .unwrap();
        let rtv = rtvs.GetCPUDescriptorHandleForHeapStart();
        device.CreateRenderTargetView(&target, None, rtv);
        let heap = |kind, count| -> ID3D12QueryHeap {
            let mut h = None;
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: kind,
                        Count: count,
                        NodeMask: 0,
                    },
                    &mut h,
                )
                .unwrap();
            h.unwrap()
        };
        let occlusion = heap(D3D12_QUERY_HEAP_TYPE_OCCLUSION, 3);
        let statistics = heap(D3D12_QUERY_HEAP_TYPE_PIPELINE_STATISTICS, 1);
        let timestamps = heap(D3D12_QUERY_HEAP_TYPE_TIMESTAMP, 2);
        let readback = committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &buffer_desc(256),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let resolved = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &buffer_desc(256),
            D3D12_RESOURCE_STATE_COMMON,
        );
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        list.Close().unwrap();

        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("queries-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\nskip memory\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        allocator.Reset().unwrap();
        list.Reset(&allocator, None).unwrap();
        list.OMSetRenderTargets(1, Some(&rtv), false, None);
        list.SetGraphicsRootSignature(&root);
        list.SetPipelineState(&pso);
        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            Width: W as f32,
            Height: H as f32,
            MaxDepth: 1.,
            ..Default::default()
        }]);
        let scissor = |right: i32, bottom: i32| {
            list.RSSetScissorRects(&[RECT {
                left: 0,
                top: 0,
                right,
                bottom,
            }])
        };
        let draw = || list.DrawInstanced(3, 1, 0, 0);
        list.EndQuery(&timestamps, D3D12_QUERY_TYPE_TIMESTAMP, 0);
        // Occlusion over two draws: the whole target and a 16x8 corner, 2048 + 128 samples.
        list.BeginQuery(&occlusion, D3D12_QUERY_TYPE_OCCLUSION, 0);
        scissor(W as i32, H as i32);
        draw();
        scissor(16, 8);
        draw();
        list.EndQuery(&occlusion, D3D12_QUERY_TYPE_OCCLUSION, 0);
        // Binary: a draw scissored to nothing (0), then the corner (1).
        list.BeginQuery(&occlusion, D3D12_QUERY_TYPE_BINARY_OCCLUSION, 1);
        scissor(0, 0);
        draw();
        list.EndQuery(&occlusion, D3D12_QUERY_TYPE_BINARY_OCCLUSION, 1);
        list.BeginQuery(&occlusion, D3D12_QUERY_TYPE_BINARY_OCCLUSION, 2);
        scissor(16, 8);
        draw();
        list.EndQuery(&occlusion, D3D12_QUERY_TYPE_BINARY_OCCLUSION, 2);
        // Pipeline statistics of one whole-target draw.
        list.BeginQuery(&statistics, D3D12_QUERY_TYPE_PIPELINE_STATISTICS, 0);
        scissor(W as i32, H as i32);
        draw();
        list.EndQuery(&statistics, D3D12_QUERY_TYPE_PIPELINE_STATISTICS, 0);
        list.EndQuery(&timestamps, D3D12_QUERY_TYPE_TIMESTAMP, 1);
        let at = |offset: usize| offset as u64;
        list.ResolveQueryData(
            &occlusion,
            D3D12_QUERY_TYPE_OCCLUSION,
            0,
            1,
            &readback,
            at(AT_OCCLUSION),
        );
        list.ResolveQueryData(
            &occlusion,
            D3D12_QUERY_TYPE_BINARY_OCCLUSION,
            1,
            2,
            &readback,
            at(AT_BINARY),
        );
        list.ResolveQueryData(
            &statistics,
            D3D12_QUERY_TYPE_PIPELINE_STATISTICS,
            0,
            1,
            &readback,
            at(AT_STATISTICS),
        );
        list.ResolveQueryData(
            &timestamps,
            D3D12_QUERY_TYPE_TIMESTAMP,
            0,
            2,
            &readback,
            at(AT_TIMESTAMPS),
        );
        // The occlusion result again, into a buffer the CPU does not reach.
        list.ResolveQueryData(&occlusion, D3D12_QUERY_TYPE_OCCLUSION, 0, 1, &resolved, 0);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 46);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        let mut mapped = core::ptr::null_mut();
        readback
            .Map(
                0,
                Some(&D3D12_RANGE {
                    Begin: 0,
                    End: RESULTS,
                }),
                Some(&mut mapped),
            )
            .unwrap();
        let results = std::slice::from_raw_parts(mapped.cast::<u8>(), RESULTS).to_vec();
        readback.Unmap(0, Some(&D3D12_RANGE::default()));
        let word = |offset: usize| u64::from_le_bytes(results[offset..offset + 8].try_into().unwrap());
        assert_eq!(word(AT_OCCLUSION), 2048 + 128, "occlusion over both draws");
        assert_eq!((word(AT_BINARY), word(AT_BINARY + 8)), (0, 1), "binary occlusion");
        assert!(
            word(AT_TIMESTAMPS + 8) >= word(AT_TIMESTAMPS),
            "timestamps in order"
        );
        std::fs::write(dir.join("queries-expected.bin"), &results).unwrap();

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
    }
    println!(
        "D3D12 queries: occlusion 2176, binary 0 and 1, statistics and timestamps resolved, and captured"
    );
}
