//! A texture an armed frame first reads on a compute queue, left in PIXEL_SHADER_RESOURCE | NON_PIXEL_SHADER_RESOURCE.
//! A compute list may not name the pixel-shader state, so locum copies it on a direct queue of its own, between fences
//! that hold the compute queue until the copy is done. The debug layer validates both queues' barriers.
use super::*;

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
) {
    unsafe {
        use graphics::transition;
        let shader_read =
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
        let subs = locum_capture::layout::texture(28, 4, 4, 1, 2, 1).unwrap();
        let data = contents::pattern(&subs, 60);
        let texture = contents::committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &contents::desc(
                D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                DXGI_FORMAT_R8G8B8A8_UNORM,
                (4, 4),
                (2, 1, 1),
                D3D12_RESOURCE_FLAG_NONE,
            ),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );

        // Before the frame, on the direct queue: fill it and leave it in both shader-resource states.
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        let mut keep = Vec::new();
        contents::fill(device, &list, &texture, &data, &mut keep);
        transition(&list, &texture, D3D12_RESOURCE_STATE_COPY_DEST, shader_read);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 19);

        // A dispatch whose root signature reaches the texture through an SRV table (reachability comes from the root
        // signature, so the shader need not read it).
        let range = D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
            NumDescriptors: 1,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 0,
        };
        let parameter = D3D12_ROOT_PARAMETER {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                    NumDescriptorRanges: 1,
                    pDescriptorRanges: &range,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let mut serialized = None;
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: 1,
                pParameters: &parameter,
                ..Default::default()
            },
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut serialized,
            None,
        )
        .unwrap();
        let serialized = serialized.unwrap();
        let root: ID3D12RootSignature = device
            .CreateRootSignature(
                0,
                core::slice::from_raw_parts(serialized.GetBufferPointer().cast(), serialized.GetBufferSize()),
            )
            .unwrap();
        let code = graphics::compile(b"[numthreads(1, 1, 1)] void main() {}", s!("cs_5_0"));
        let mut pso_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.GetBufferPointer(),
                BytecodeLength: code.GetBufferSize(),
            },
            ..Default::default()
        };
        let pso: ID3D12PipelineState = device.CreateComputePipelineState(&pso_desc).unwrap();
        ManuallyDrop::drop(&mut pso_desc.pRootSignature);
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                ..Default::default()
            })
            .unwrap();
        device.CreateShaderResourceView(&texture, None, heap.GetCPUDescriptorHandleForHeapStart());
        let compute: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COMPUTE,
                ..Default::default()
            })
            .unwrap();
        let compute_allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COMPUTE)
            .unwrap();
        let compute_list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COMPUTE, &compute_allocator, &pso)
            .unwrap();
        compute_list.SetComputeRootSignature(&root);
        compute_list.SetDescriptorHeaps(&[Some(heap.clone())]);
        compute_list.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
        compute_list.Dispatch(1, 1, 1);
        compute_list.Close().unwrap();
        let compute_fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();

        // The frame: the compute submission is the texture's first use.
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("compute-first-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        compute.ExecuteCommandLists(&[Some(compute_list.cast().unwrap())]);
        finish(&compute, &compute_fence, 1);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        // The application's own read afterwards finds the texture in its state, bytes unchanged.
        let readback = contents::committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &contents::buffer_desc(4096),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        allocator.Reset().unwrap();
        list.Reset(&allocator, None).unwrap();
        transition(&list, &texture, shader_read, D3D12_RESOURCE_STATE_COPY_SOURCE);
        let f = contents::read(device, &list, &texture, &readback, 0);
        transition(&list, &texture, D3D12_RESOURCE_STATE_COPY_SOURCE, shader_read);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 20);
        let mut p = core::ptr::null_mut();
        readback.Map(0, None, Some(&mut p)).unwrap();
        let row = core::slice::from_raw_parts(
            p.cast::<u8>()
                .add(f.Offset as usize + f.Footprint.RowPitch as usize),
            16,
        );
        assert_eq!(row, &data[0][16..32], "the application's read after the frame");
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
        let (content, source, note): (Option<String>, Option<String>, Option<String>) = db
            .query_row(
                "SELECT content, content_source, content_note FROM resources WHERE width=4 AND height=4 AND mips=2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(source.as_deref(), Some("first-use"), "{note:?}");
        let blobs = locum_capture::blobs::Blobs::new(dir.join("blobs"));
        assert_eq!(
            blobs.get(&content.unwrap()).unwrap(),
            data.concat(),
            "both mips as filled"
        );
        // Copied before the compute submission, from the states it found.
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM d12_first_use f JOIN d12_submissions s ON s.id=f.submission \
                 WHERE f.states_json='[{\"state\":192},{\"state\":192}]' AND s.queue=(SELECT queue FROM d12_submissions \
                 ORDER BY id LIMIT 1)",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap(),
            1
        );
        drop(keep);
        debug_errors(device);
    }
    println!(
        "D3D12 compute-queue first use verified: a texture in both shader-resource states copied on a direct queue"
    );
}
