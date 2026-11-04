//! Constant buffers in a default heap that the armed frame fills by copying: Elden Ring copies its light sets from an
//! upload buffer into one before the draws that read it. Each dispatch's constants must be the bytes it read: the
//! buffer as its first use found it with the frame's earlier copies laid over, through a GPU-to-GPU copy as well. A
//! buffer the frame writes some other way (a UAV clear) has no constants, and the capture says why.
use super::*;

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
) {
    unsafe {
        use graphics::transition;
        let upload = |fill: &dyn Fn(usize) -> u8| {
            let b = contents::committed(
                device,
                D3D12_HEAP_TYPE_UPLOAD,
                &contents::buffer_desc(512),
                D3D12_RESOURCE_STATE_GENERIC_READ,
            );
            let mut p = core::ptr::null_mut();
            b.Map(0, None, Some(&mut p)).unwrap();
            for i in 0..512 {
                p.cast::<u8>().add(i).write(fill(i));
            }
            b.Unmap(0, None);
            b
        };
        let before = upload(&|_| 0xaa);
        let source: Vec<u8> = (0..512).map(|i| (i * 7 + 3) as u8).collect();
        let src = upload(&|i| source[i]);
        let default = |flags| {
            contents::committed(
                device,
                D3D12_HEAP_TYPE_DEFAULT,
                &contents::desc(
                    D3D12_RESOURCE_DIMENSION_BUFFER,
                    DXGI_FORMAT_UNKNOWN,
                    (512, 1),
                    (1, 1, 1),
                    flags,
                ),
                D3D12_RESOURCE_STATE_COMMON,
            )
        };
        let constants = default(D3D12_RESOURCE_FLAG_NONE);
        let chained = default(D3D12_RESOURCE_FLAG_NONE);
        let cleared = default(D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS);

        // Before the frame: `constants` holds 0xaa.
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        list.CopyBufferRegion(&constants, 0, &before, 0, 512);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 22);

        // b0-b2 in one table: `constants`, `chained` and `cleared`, 256 bytes each.
        let range = D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_CBV,
            NumDescriptors: 3,
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
                NumDescriptors: 4,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                ..Default::default()
            })
            .unwrap();
        let cpu_heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                ..Default::default()
            })
            .unwrap();
        let step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
        let start = heap.GetCPUDescriptorHandleForHeapStart();
        let at = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: start.ptr + i * step,
        };
        for (i, b) in [&constants, &chained, &cleared].into_iter().enumerate() {
            device.CreateConstantBufferView(
                Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC {
                    BufferLocation: b.GetGPUVirtualAddress(),
                    SizeInBytes: 256,
                }),
                at(i),
            );
        }
        let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: 128,
                    ..Default::default()
                },
            },
        };
        device.CreateUnorderedAccessView(&cleared, None, Some(&uav), at(3));
        let cpu_uav = cpu_heap.GetCPUDescriptorHandleForHeapStart();
        device.CreateUnorderedAccessView(&cleared, None, Some(&uav), cpu_uav);

        // The frame: bytes 64-191 of `constants` from the upload buffer, then all of it into `chained`, `cleared`
        // cleared; a dispatch; then the second half of the upload buffer over all of `constants`, and a dispatch.
        allocator.Reset().unwrap();
        list.Reset(&allocator, &pso).unwrap();
        list.SetComputeRootSignature(&root);
        list.SetDescriptorHeaps(&[Some(heap.clone())]);
        list.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
        let cb = D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER;
        list.CopyBufferRegion(&constants, 64, &src, 0, 128);
        transition(
            &list,
            &constants,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        list.CopyBufferRegion(&chained, 0, &constants, 0, 512);
        transition(
            &list,
            &cleared,
            D3D12_RESOURCE_STATE_COMMON,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        let gpu_uav = D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: heap.GetGPUDescriptorHandleForHeapStart().ptr + 3 * step as u64,
        };
        list.ClearUnorderedAccessViewUint(gpu_uav, cpu_uav, &cleared, &[7, 7, 7, 7], &[]);
        transition(&list, &constants, D3D12_RESOURCE_STATE_COPY_SOURCE, cb);
        transition(&list, &chained, D3D12_RESOURCE_STATE_COPY_DEST, cb);
        transition(&list, &cleared, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, cb);
        list.Dispatch(1, 1, 1);
        transition(&list, &constants, cb, D3D12_RESOURCE_STATE_COPY_DEST);
        list.CopyBufferRegion(&constants, 0, &src, 256, 256);
        transition(&list, &constants, D3D12_RESOURCE_STATE_COPY_DEST, cb);
        list.Dispatch(2, 1, 1);
        list.Close().unwrap();
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("cb-copied-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 23);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

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
        let constants_of = |args: &str, slot: u32| -> Option<Vec<u8>> {
            db.query_row(
                "SELECT d.bytes FROM bindings b JOIN draws w ON w.n=b.draw LEFT JOIN cb_data d ON d.hash=b.content \
                 WHERE w.args=?1 AND b.kind='cb' AND b.slot=?2",
                rusqlite::params![args, slot],
                |r| r.get(0),
            )
            .unwrap()
        };
        let first: Vec<u8> = [&[0xaa; 64][..], &source[..128], &[0xaa; 64][..]].concat();
        assert_eq!(
            constants_of("[1,1,1]", 0).as_deref(),
            Some(&first[..]),
            "first dispatch, b0"
        );
        assert_eq!(
            constants_of("[1,1,1]", 1).as_deref(),
            Some(&first[..]),
            "first dispatch, b1 (copied on the GPU)"
        );
        assert_eq!(constants_of("[1,1,1]", 2), None, "first dispatch, b2 (cleared)");
        assert_eq!(
            constants_of("[2,1,1]", 0).as_deref(),
            Some(&source[256..]),
            "second dispatch, b0"
        );
        assert_eq!(
            constants_of("[2,1,1]", 1).as_deref(),
            Some(&first[..]),
            "second dispatch, b1"
        );
        assert_eq!(
            db.query_row(
                "SELECT count FROM gaps WHERE what='d3d12-constant-content' AND detail LIKE '%ClearUnorderedAccessViewUint%'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap(),
            2
        );
        debug_errors(device);
    }
    println!(
        "D3D12 copied constants verified: a default-heap CB as each dispatch read it, through upload and GPU copies"
    );
}
