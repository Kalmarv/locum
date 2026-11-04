//! A depth-stencil copied where its pass ends, from a state that allows no write, with the next pass writing it again
//! in the same list: Elden Ring's split-plane depth-stencil (m17, draws 2078 and 2079). Each draw writes a bit of the
//! stencil through a view that only reads depth; a barrier per plane moves it into shader-resource states between
//! them, and an empty pass on another view moves it into DEPTH_WRITE and back. The copy after the first draw must
//! hold its bit and not the second's. On an NVIDIA driver a copy whose planes go from COPY_SOURCE straight back to
//! the shader-resource states overlapped the second draw (every copy wrong from row 32 on); WARP keeps the order, so
//! `--hardware` runs this alone on the default adapter at the game's size.
use super::contents::{committed, desc};
use super::graphics::compile;
use super::*;

const SHADER_RESOURCE: i32 = 0xC0;
const DEPTH_READ: i32 = 0xE0;
const DEPTH_WRITE: i32 = 0x10;

/// Legacy transitions of the depth-stencil's planes, (subresource, before, after) each.
unsafe fn planes(list: &ID3D12GraphicsCommandList, r: &ID3D12Resource, t: &[(u32, i32, i32)]) {
    let mut barriers: Vec<D3D12_RESOURCE_BARRIER> = t
        .iter()
        .map(|&(sub, before, after)| D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: ManuallyDrop::new(Some(r.clone())),
                    Subresource: sub,
                    StateBefore: D3D12_RESOURCE_STATES(before),
                    StateAfter: D3D12_RESOURCE_STATES(after),
                }),
            },
        })
        .collect();
    unsafe {
        list.ResourceBarrier(&barriers);
        for b in &mut barriers {
            ManuallyDrop::drop(&mut (*b.Anonymous.Transition).pResource);
        }
    }
}

/// A pipeline with no targets but the depth-stencil, reading depth and writing stencil bit `mask` with the reference.
unsafe fn pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    vs: &ID3DBlob,
    mask: u8,
) -> ID3D12PipelineState {
    let op = D3D12_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D12_STENCIL_OP_KEEP,
        StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
        StencilPassOp: D3D12_STENCIL_OP_REPLACE,
        StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
    };
    unsafe {
        let mut p = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            VS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: vs.GetBufferPointer(),
                BytecodeLength: vs.GetBufferSize(),
            },
            SampleMask: u32::MAX,
            RasterizerState: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_NONE,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ZERO,
                DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                StencilEnable: true.into(),
                StencilReadMask: 0xff,
                StencilWriteMask: mask,
                FrontFace: op,
                BackFace: op,
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            DSVFormat: DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let pso = device.CreateGraphicsPipelineState(&p).unwrap();
        ManuallyDrop::drop(&mut p.pRootSignature);
        pso
    }
}

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    (width, height): (u32, u32),
) {
    unsafe {
        let vs = compile(
            b"float4 main(uint id : SV_VertexID) : SV_Position { float2 p[3] = {float2(-1,-1), float2(-1,3), float2(3,-1)}; return float4(p[id], 0.5, 1); }",
            s!("vs_5_0"),
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
        let (bit0, bit1) = (pipeline(device, &root, &vs, 1), pipeline(device, &root, &vs, 2));
        // As the game's: R32G8X24_TYPELESS, its depth view D32_FLOAT_S8X24_UINT, resting in the shader-resource states.
        let z = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &desc(
                D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                DXGI_FORMAT_R32G8X24_TYPELESS,
                (u64::from(width), height),
                (1, 1, 1),
                D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            ),
            D3D12_RESOURCE_STATES(SHADER_RESOURCE),
        );
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: 2,
                ..Default::default()
            })
            .unwrap();
        let step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV) as usize;
        let writable = heap.GetCPUDescriptorHandleForHeapStart();
        let read_only = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: writable.ptr + step,
        };
        for (handle, flags) in [
            (writable, D3D12_DSV_FLAG_NONE),
            (read_only, D3D12_DSV_FLAG_READ_ONLY_DEPTH),
        ] {
            device.CreateDepthStencilView(
                &z,
                Some(&D3D12_DEPTH_STENCIL_VIEW_DESC {
                    Format: DXGI_FORMAT_D32_FLOAT_S8X24_UINT,
                    ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2D,
                    Flags: flags,
                    Anonymous: D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                        Texture2D: D3D12_TEX2D_DSV { MipSlice: 0 },
                    },
                }),
                handle,
            );
        }
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        list.Close().unwrap();

        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("planes-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\nskip memory\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        allocator.Reset().unwrap();
        list.Reset(&allocator, None).unwrap();
        // Depth 0.5, stencil 0x10.
        planes(
            &list,
            &z,
            &[
                (0, SHADER_RESOURCE, DEPTH_WRITE),
                (1, SHADER_RESOURCE, DEPTH_WRITE),
            ],
        );
        list.ClearDepthStencilView(
            writable,
            D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
            0.5,
            0x10,
            None,
        );
        planes(
            &list,
            &z,
            &[
                (0, DEPTH_WRITE, SHADER_RESOURCE),
                (1, DEPTH_WRITE, SHADER_RESOURCE),
            ],
        );
        list.SetGraphicsRootSignature(&root);
        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            Width: width as f32,
            Height: height as f32,
            MaxDepth: 1.,
            ..Default::default()
        }]);
        list.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        }]);
        // Draws 1 and 2 as the game's: the view reads depth, the depth plane in DEPTH_READ and the shader-resource
        // states, the stencil plane in DEPTH_WRITE; the barrier back to the shader-resource states ends the pass.
        let draw = |pso: &ID3D12PipelineState, reference: u32| {
            list.OMSetRenderTargets(0, None, false, Some(&read_only));
            planes(
                &list,
                &z,
                &[
                    (0, SHADER_RESOURCE, DEPTH_READ),
                    (1, SHADER_RESOURCE, DEPTH_WRITE),
                ],
            );
            list.SetPipelineState(pso);
            list.OMSetStencilRef(reference);
            list.DrawInstanced(3, 1, 0, 0);
            planes(
                &list,
                &z,
                &[
                    (0, DEPTH_READ, SHADER_RESOURCE),
                    (1, DEPTH_WRITE, SHADER_RESOURCE),
                ],
            );
        };
        // Draw 1: stencil 0x11. Its copy waits for the next command naming the target, the rebind.
        draw(&bit0, 1);
        // An empty pass on the writable view: into DEPTH_WRITE and straight back.
        list.OMSetRenderTargets(0, None, false, Some(&writable));
        planes(
            &list,
            &z,
            &[
                (0, SHADER_RESOURCE, DEPTH_WRITE),
                (1, SHADER_RESOURCE, DEPTH_WRITE),
            ],
        );
        planes(
            &list,
            &z,
            &[
                (0, DEPTH_WRITE, SHADER_RESOURCE),
                (1, DEPTH_WRITE, SHADER_RESOURCE),
            ],
        );
        // Draw 2: stencil 0x13.
        draw(&bit1, 2);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 43);
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
        let targets: Vec<(i64, String, Option<String>)> = db
            .prepare("SELECT draw, target, blob FROM targets ORDER BY draw")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let shape: Vec<_> = targets.iter().map(|(d, t, _)| (*d, t.as_str())).collect();
        assert_eq!(shape, [(1, "depth"), (2, "depth")], "{targets:?}");
        for ((draw, _, blob), stencil) in targets.iter().zip([0x11u8, 0x13]) {
            let bytes = blobs.get(blob.as_ref().unwrap()).unwrap();
            assert_eq!(
                bytes.len() as u64,
                u64::from(width) * u64::from(height) * 8,
                "draw {draw}: depth and stencil joined, 8 bytes a texel"
            );
            let wrong: Vec<usize> = (0..height as usize)
                .filter(|y| {
                    bytes[y * width as usize * 8..(y + 1) * width as usize * 8]
                        .chunks(8)
                        .any(|t| t[..4] != 0.5f32.to_le_bytes() || t[4] != stencil)
                })
                .collect();
            assert!(
                wrong.is_empty(),
                "draw {draw}: {} of {height} rows hold another depth or stencil than 0.5 and {stencil:#x}, first {:?} (a copy that overlapped the next draw)",
                wrong.len(),
                wrong.first()
            );
        }
    }
    println!(
        "D3D12 pass-end copy of a split-plane depth-stencil from the shader-resource states, written again in the same \
         list: each draw's stencil as it left it ({width}x{height})"
    );
}
