//! Pass-end targets: render and depth targets copied inside the application's own lists where each pass ends, and
//! read back as the frame's draws left them, under the debug layer, which validates the copies' transitions. Passes
//! end at a barrier, a rebind, a clear, Close, a native render pass's end and an enhanced barrier, or go on in the
//! next list submitted with theirs; the targets are a depth-stencil buffer (both planes), a mip of an array slice and
//! a texture made in a layout.
use super::contents::{buffer_desc, committed, desc, layout_barrier, read};
use super::graphics::{compile, transition};
use super::*;

/// A `targets` row: draw, target, width, height, format, sub, blob.
type Row = (i64, String, u32, u32, u32, u32, Option<String>);

const RGBA8: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;
const D24S8: DXGI_FORMAT = DXGI_FORMAT_D24_UNORM_S8_UINT;

unsafe fn pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    vs: &ID3DBlob,
    ps: &ID3DBlob,
    depth: bool,
) -> ID3D12PipelineState {
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
                DepthEnable: depth.into(),
                DepthWriteMask: if depth {
                    D3D12_DEPTH_WRITE_MASK_ALL
                } else {
                    D3D12_DEPTH_WRITE_MASK_ZERO
                },
                DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                ..Default::default()
            },
            PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
            NumRenderTargets: 1,
            RTVFormats: formats,
            DSVFormat: if depth { D24S8 } else { DXGI_FORMAT_UNKNOWN },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        let pso = device.CreateGraphicsPipelineState(&p).unwrap();
        ManuallyDrop::drop(&mut p.pRootSignature);
        pso
    }
}

/// A full-target triangle in `colour` at `depth`.
unsafe fn draw(list: &ID3D12GraphicsCommandList, (width, height): (u32, u32), colour: [f32; 4], depth: f32) {
    unsafe {
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
        let constants = [colour[0], colour[1], colour[2], colour[3], depth];
        list.SetGraphicsRoot32BitConstants(0, 5, constants.as_ptr().cast(), 0);
        list.DrawInstanced(3, 1, 0, 0);
    }
}

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
) {
    unsafe {
        let constants = b"cbuffer C : register(b0) { float4 colour; float depth; };";
        let vs = compile(
            &[
                &constants[..],
                b"float4 main(uint id : SV_VertexID) : SV_Position { float2 p[3] = {float2(-1,-1), float2(-1,3), float2(3,-1)}; return float4(p[id], depth, 1); }",
            ]
            .concat(),
            s!("vs_5_0"),
        );
        let ps = compile(
            &[&constants[..], b"float4 main() : SV_Target { return colour; }"].concat(),
            s!("ps_5_0"),
        );
        let parameter = D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                Constants: D3D12_ROOT_CONSTANTS {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Num32BitValues: 5,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
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
        let with_depth = pipeline(device, &root, &vs, &ps, true);
        let colour_only = pipeline(device, &root, &vs, &ps, false);

        let tex2d = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        let rt = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
        let target = |w: u64, h: u32, mips: u16, array: u16| {
            committed(
                device,
                D3D12_HEAP_TYPE_DEFAULT,
                &desc(tex2d, RGBA8, (w, h), (mips, array, 1), rt),
                D3D12_RESOURCE_STATE_RENDER_TARGET,
            )
        };
        let t1 = target(16, 8, 1, 1);
        let t2 = target(16, 8, 1, 1);
        // Mip 1 of slice 1: subresource 3, 2 by 2.
        let array = target(4, 4, 2, 2);
        let z = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &desc(
                tex2d,
                D24S8,
                (16, 8),
                (1, 1, 1),
                D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            ),
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
        );
        // Made in a layout: its passes end at enhanced barriers.
        let device10: ID3D12Device10 = device.cast().unwrap();
        let mut h: Option<ID3D12Resource> = None;
        device10
            .CreateCommittedResource3(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC1 {
                    Dimension: tex2d,
                    Width: 8,
                    Height: 8,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: RGBA8,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Flags: rt,
                    ..Default::default()
                },
                D3D12_BARRIER_LAYOUT_RENDER_TARGET,
                None,
                None,
                None,
                &mut h,
            )
            .unwrap();
        let h = h.unwrap();

        let rtv_heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 4,
                ..Default::default()
            })
            .unwrap();
        let stride = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
        let start = rtv_heap.GetCPUDescriptorHandleForHeapStart();
        let rtv = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + i * stride,
        };
        device.CreateRenderTargetView(&t1, None, rtv(0));
        device.CreateRenderTargetView(&t2, None, rtv(1));
        device.CreateRenderTargetView(
            &array,
            Some(&D3D12_RENDER_TARGET_VIEW_DESC {
                Format: RGBA8,
                ViewDimension: D3D12_RTV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D12_RENDER_TARGET_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_RTV {
                        MipSlice: 1,
                        FirstArraySlice: 1,
                        ArraySize: 1,
                        PlaneSlice: 0,
                    },
                },
            }),
            rtv(2),
        );
        device.CreateRenderTargetView(&h, None, rtv(3));
        let dsv_heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: 1,
                ..Default::default()
            })
            .unwrap();
        let dsv = dsv_heap.GetCPUDescriptorHandleForHeapStart();
        device.CreateDepthStencilView(&z, None, dsv);
        let readback = committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &buffer_desc(64 << 10),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        // A list made before the capture is armed, reset in the frame: instrumented.
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        list.Close().unwrap();

        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("passes-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        let (red, green, blue, yellow, magenta, cyan, white) = (
            [1., 0., 0., 1.],
            [0., 1., 0., 1.],
            [0., 0., 1., 1.],
            [1., 1., 0., 1.],
            [1., 0., 1., 1.],
            [0., 1., 1., 1.],
            [1., 1., 1., 1.],
        );
        allocator.Reset().unwrap();
        list.Reset(&allocator, &with_depth).unwrap();
        list.SetGraphicsRootSignature(&root);
        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        // Draw 1: t1 red, depth 0.25 over stencil 0x33. A barrier ends t1's pass.
        list.OMSetRenderTargets(1, Some(&rtv(0)), true, Some(&dsv));
        list.ClearRenderTargetView(rtv(0), &[0., 0., 0., 1.], None);
        list.ClearDepthStencilView(
            dsv,
            D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
            1.0,
            0x33,
            None,
        );
        draw(&list, (16, 8), red, 0.25);
        transition(
            &list,
            &t1,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        // Rebinding ends the depth's pass. Draw 2: t2 green, which a clear ends.
        list.OMSetRenderTargets(1, Some(&rtv(1)), true, Some(&dsv));
        draw(&list, (16, 8), green, 0.5);
        list.ClearRenderTargetView(rtv(1), &white, None);
        // Draw 3: t2 blue, depth 0.75; a rebind ends both.
        draw(&list, (16, 8), blue, 0.75);
        list.SetPipelineState(&colour_only);
        list.OMSetRenderTargets(1, Some(&rtv(2)), true, None);
        // Draw 4: the array's mip, yellow; Close ends it.
        draw(&list, (2, 2), yellow, 0.);
        transition(
            &list,
            &t1,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        list.Close().unwrap();

        // A list created in the frame: a native render pass, then a target in a layout.
        let allocator2: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list2: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator2, &colour_only)
            .unwrap();
        let list4: ID3D12GraphicsCommandList4 = list2.cast().unwrap();
        let list7: ID3D12GraphicsCommandList7 = list2.cast().unwrap();
        list2.SetGraphicsRootSignature(&root);
        list2.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list4.BeginRenderPass(
            Some(&[D3D12_RENDER_PASS_RENDER_TARGET_DESC {
                cpuDescriptor: rtv(1),
                BeginningAccess: D3D12_RENDER_PASS_BEGINNING_ACCESS {
                    Type: D3D12_RENDER_PASS_BEGINNING_ACCESS_TYPE_PRESERVE,
                    ..Default::default()
                },
                EndingAccess: D3D12_RENDER_PASS_ENDING_ACCESS {
                    Type: D3D12_RENDER_PASS_ENDING_ACCESS_TYPE_PRESERVE,
                    ..Default::default()
                },
            }]),
            None,
            D3D12_RENDER_PASS_FLAG_NONE,
        );
        // Draw 5: t2 magenta; copied after EndRenderPass, before the application's own barrier and read.
        draw(&list2, (16, 8), magenta, 0.);
        list4.EndRenderPass();
        transition(
            &list2,
            &t2,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        let t2_read = read(device, &list2, &t2, &readback, 0);
        transition(
            &list2,
            &t2,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        // Draw 6: h cyan; an enhanced barrier ends it.
        list2.OMSetRenderTargets(1, Some(&rtv(3)), true, None);
        draw(&list2, (8, 8), cyan, 0.);
        layout_barrier(
            &list7,
            &h,
            [D3D12_BARRIER_SYNC_RENDER_TARGET, D3D12_BARRIER_SYNC_ALL_SHADING],
            [
                D3D12_BARRIER_ACCESS_RENDER_TARGET,
                D3D12_BARRIER_ACCESS_SHADER_RESOURCE,
            ],
            [
                D3D12_BARRIER_LAYOUT_RENDER_TARGET,
                D3D12_BARRIER_LAYOUT_SHADER_RESOURCE,
            ],
        );
        layout_barrier(
            &list7,
            &h,
            [D3D12_BARRIER_SYNC_ALL_SHADING, D3D12_BARRIER_SYNC_RENDER_TARGET],
            [
                D3D12_BARRIER_ACCESS_SHADER_RESOURCE,
                D3D12_BARRIER_ACCESS_RENDER_TARGET,
            ],
            [
                D3D12_BARRIER_LAYOUT_SHADER_RESOURCE,
                D3D12_BARRIER_LAYOUT_RENDER_TARGET,
            ],
        );
        // Draw 7: t1 white, bound while it is a shader resource and only then moved into RENDER_TARGET (as Elden
        // Ring's passes do): the barrier does not end a pass not yet drawn to. Close ends it.
        transition(
            &list2,
            &t1,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        list2.OMSetRenderTargets(1, Some(&rtv(0)), true, None);
        transition(
            &list2,
            &t1,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        draw(&list2, (16, 8), white, 0.);
        list2.Close().unwrap();
        // Draw 8: a third list continues t1's pass, drawing to it with the same view bound before writing it any
        // other way, as Elden Ring's G-buffer goes on over lists submitted together: no copy where list2 closed, one
        // where list3 does. Where the first list closed (draws 3 and 4), no later list draws to the mip or the depth,
        // and list2 draws to t2 with other views bound: they are copied by a list of locum's after it.
        let list3: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator2, &colour_only)
            .unwrap();
        list3.SetGraphicsRootSignature(&root);
        list3.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list3.OMSetRenderTargets(1, Some(&rtv(0)), true, None);
        draw(&list3, (16, 8), green, 0.);
        list3.Close().unwrap();
        queue.ExecuteCommandLists(&[
            Some(list.cast().unwrap()),
            Some(list2.cast().unwrap()),
            Some(list3.cast().unwrap()),
        ]);
        // Draw 9: list3 again, once its first execution is done (a fence of its own: the shared one's values are
        // taken). Its copies hold the first execution's target: this pass end is missing, with why.
        let again: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        finish(queue, &again, 1);
        queue.ExecuteCommandLists(&[Some(list3.cast().unwrap())]);
        finish(queue, fence, 17);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        // The application's own read after the render pass: the copies left the state as they found it.
        let mut p = core::ptr::null_mut();
        readback.Map(0, None, Some(&mut p)).unwrap();
        let row = core::slice::from_raw_parts(p.cast::<u8>().add(t2_read.Offset as usize), 64);
        assert!(
            row.chunks(4).all(|t| t == [255, 0, 255, 255]),
            "t2 after the render pass: {row:?}"
        );
        readback.Unmap(0, None);

        let path = dir.join("captures").join(&label).join("capture.db");
        let until = Instant::now() + Duration::from_secs(15);
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
        let mut statement = db
            .prepare("SELECT draw, target, width, height, format, sub, blob FROM targets ORDER BY event, target DESC")
            .unwrap();
        let rows: Vec<Row> = statement
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            })
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let shape: Vec<_> = rows
            .iter()
            .map(|(draw, target, w, h, _, sub, _)| (*draw, target.as_str(), *w, *h, *sub))
            .collect();
        assert_eq!(
            shape,
            [
                (1, "rt0", 16, 8, 0),
                (1, "depth", 16, 8, 0),
                (2, "rt0", 16, 8, 0),
                (3, "rt0", 16, 8, 0),
                (3, "depth", 16, 8, 0),
                (4, "rt0", 2, 2, 3),
                (5, "rt0", 16, 8, 0),
                (6, "rt0", 8, 8, 0),
                (8, "rt0", 16, 8, 0),
            ],
            "{rows:?}"
        );
        let bytes = |i: usize| blobs.get(rows[i].6.as_ref().unwrap()).unwrap();
        let colour = |i: usize, rgba: [u8; 4]| {
            let b = bytes(i);
            assert_eq!(b.len() as u32, rows[i].2 * rows[i].3 * 4, "{:?}", rows[i]);
            assert!(b.chunks(4).all(|t| t == rgba), "{:?}: {:?}", rows[i], &b[..8]);
        };
        let depth = |i: usize, expected: f64| {
            let b = bytes(i);
            assert_eq!(b.len(), 16 * 8 * 4);
            for t in b.chunks(4) {
                let d = f64::from(u32::from_le_bytes([t[0], t[1], t[2], 0])) / f64::from(0xFF_FFFF);
                assert!(
                    (d - expected).abs() < 1e-6 && t[3] == 0x33,
                    "depth {expected}: {t:?}"
                );
            }
        };
        colour(0, [255, 0, 0, 255]);
        depth(1, 0.25);
        colour(2, [0, 255, 0, 255]);
        colour(3, [0, 0, 255, 255]);
        depth(4, 0.75);
        colour(5, [255, 255, 0, 255]);
        colour(6, [255, 0, 255, 255]);
        colour(7, [0, 255, 255, 255]);
        colour(8, [0, 255, 0, 255]);
        for (sql, n) in [
            ("SELECT sum(count) FROM gaps WHERE what='d3d12-targets'", 1),
            (
                "SELECT count(*) FROM missing_targets WHERE draw = 9 AND target = 'rt0' AND why LIKE '%executed again%'",
                1,
            ),
            ("SELECT count(*) FROM missing_targets", 1),
            ("SELECT count(*) FROM draws", 9),
            (
                "SELECT json_extract(value, '$.continued') FROM meta WHERE key='pass_ends'",
                1,
            ),
            (
                "SELECT json_extract(value, '$.targets') FROM meta WHERE key='counts'",
                9,
            ),
            (
                "SELECT json_extract(value, '$.target_bytes') FROM meta WHERE key='counts'",
                7 * 512 + 16 + 256,
            ),
        ] {
            assert_eq!(db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap(), n, "{sql}");
        }
        debug_errors(device);
    }
    println!(
        "D3D12 pass-end targets verified: barrier, rebind, clear, Close, render pass and enhanced-barrier ends; a target moved into RENDER_TARGET after binding; a pass continued in the next list; a re-executed list's pass end named as missing; depth and stencil, array mip, layout target"
    );
}
