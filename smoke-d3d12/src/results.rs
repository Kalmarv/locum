//! What dispatches and a UAV clear write through UAVs, copied right after each: a structured buffer and a texture's
//! mip in one array slice through a descriptor table, a raw buffer through a root UAV, an atomic whose results are
//! used (order-dependent), and the same pipeline again in a list that moves its buffers with enhanced barriers and
//! writes a texture made in the UNORDERED_ACCESS layout. Each output must hold what its own command wrote, not what
//! a later one did, and the debug layer validates locum's transitions.
use super::*;

const R32_UINT: DXGI_FORMAT = DXGI_FORMAT_R32_UINT;

/// u0 (a structured buffer of words) and u1 (a texture array's slice) through a table, u2 (raw) as a root UAV, the
/// constant `k` as a root constant.
const WRITE: &[u8] = b"RWStructuredBuffer<uint> buf : register(u0);
RWTexture2DArray<uint> tex : register(u1);
RWByteAddressBuffer raw : register(u2);
cbuffer c : register(b0) { uint k; }
[numthreads(4, 4, 1)] void main(uint3 id : SV_DispatchThreadID) {
    uint i = id.y * 4 + id.x;
    buf[i] = k * 100 + i;
    tex[uint3(id.xy, 0)] = k + id.x + id.y * 16;
    raw.Store(i * 4, k * 1000 + id.x);
}";

/// Each thread takes a ticket from buf[63] and stores it: the order of the tickets is the GPU's.
const TICKETS: &[u8] = b"RWStructuredBuffer<uint> buf : register(u0);
RWByteAddressBuffer raw : register(u2);
[numthreads(4, 1, 1)] void main(uint3 id : SV_DispatchThreadID) {
    uint ticket;
    InterlockedAdd(buf[63], 1, ticket);
    raw.Store(64 + id.x * 4, ticket);
}";

/// A `results` row: the dispatch's number (None for the clear), the subresource, order-dependent, the bytes.
type Output = (Option<i64>, Option<i64>, bool, Vec<u8>);

unsafe fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        ..Default::default()
    };
    unsafe { list.ResourceBarrier(std::slice::from_ref(&barrier)) };
}

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
) {
    unsafe {
        use graphics::transition;
        let uav = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
        let common = D3D12_RESOURCE_STATE_COMMON;
        let buffer = |bytes: u64| {
            contents::committed(
                device,
                D3D12_HEAP_TYPE_DEFAULT,
                &contents::desc(
                    D3D12_RESOURCE_DIMENSION_BUFFER,
                    DXGI_FORMAT_UNKNOWN,
                    (bytes, 1),
                    (1, 1, 1),
                    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ),
                common,
            )
        };
        let (buf, raw, buf2, raw2) = (buffer(256), buffer(128), buffer(256), buffer(128));
        // 8x8, two mips, two slices: the view writes mip 1 of slice 1, subresource 3.
        let tex = contents::committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &contents::desc(
                D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                R32_UINT,
                (8, 8),
                (2, 2, 1),
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ),
            common,
        );
        // 4x4, made in the UNORDERED_ACCESS layout.
        let device10: ID3D12Device10 = device.cast().unwrap();
        let mut ltex: Option<ID3D12Resource> = None;
        device10
            .CreateCommittedResource3(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC1 {
                    Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                    Width: 4,
                    Height: 4,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: R32_UINT,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                    ..Default::default()
                },
                D3D12_BARRIER_LAYOUT_UNORDERED_ACCESS,
                None,
                None,
                None,
                &mut ltex,
            )
            .unwrap();
        let ltex = ltex.unwrap();

        // Before the frame: the buffers hold zeros.
        let zeros = contents::committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &contents::buffer_desc(256),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut p = core::ptr::null_mut();
        zeros.Map(0, None, Some(&mut p)).unwrap();
        core::ptr::write_bytes(p.cast::<u8>(), 0, 256);
        zeros.Unmap(0, None);
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        for (b, bytes) in [(&buf, 256), (&raw, 128), (&buf2, 256), (&raw2, 128)] {
            list.CopyBufferRegion(b, 0, &zeros, 0, bytes);
        }
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 25);

        // Root parameters: the table of u0-u1, the root UAV u2, one root constant b0.
        let range = D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
            NumDescriptors: 2,
            BaseShaderRegister: 0,
            RegisterSpace: 0,
            OffsetInDescriptorsFromTableStart: 0,
        };
        let parameters = [
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: 1,
                        pDescriptorRanges: &range,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR {
                        ShaderRegister: 2,
                        RegisterSpace: 0,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 0,
                        RegisterSpace: 0,
                        Num32BitValues: 1,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
        ];
        let mut serialized = None;
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: parameters.len() as u32,
                pParameters: parameters.as_ptr(),
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
        let pipeline = |source: &[u8]| -> ID3D12PipelineState {
            let code = graphics::compile(source, s!("cs_5_1"));
            let mut desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: ManuallyDrop::new(Some(root.clone())),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.GetBufferPointer(),
                    BytecodeLength: code.GetBufferSize(),
                },
                ..Default::default()
            };
            let pso = device.CreateComputePipelineState(&desc).unwrap();
            ManuallyDrop::drop(&mut desc.pRootSignature);
            pso
        };
        let (write, tickets) = (pipeline(WRITE), pipeline(TICKETS));

        // Slots 0-1: buf and tex's mip 1 of slice 1; 2-3: buf2 and ltex. The CPU heap holds tex's view, to clear it.
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
        let gpu = |i: usize| D3D12_GPU_DESCRIPTOR_HANDLE {
            ptr: heap.GetGPUDescriptorHandleForHeapStart().ptr + (i * step) as u64,
        };
        let structured = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_UNKNOWN,
            ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_UAV {
                    NumElements: 64,
                    StructureByteStride: 4,
                    ..Default::default()
                },
            },
        };
        let slice = |mip: u32, first: u32| D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2DARRAY,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_UAV {
                    MipSlice: mip,
                    FirstArraySlice: first,
                    ArraySize: 1,
                    PlaneSlice: 0,
                },
            },
        };
        device.CreateUnorderedAccessView(&buf, None, Some(&structured), at(0));
        device.CreateUnorderedAccessView(&tex, None, Some(&slice(1, 1)), at(1));
        device.CreateUnorderedAccessView(&buf2, None, Some(&structured), at(2));
        device.CreateUnorderedAccessView(&ltex, None, Some(&slice(0, 0)), at(3));
        let cpu_uav = cpu_heap.GetCPUDescriptorHandleForHeapStart();
        device.CreateUnorderedAccessView(&tex, None, Some(&slice(1, 1)), cpu_uav);

        // The frame, armed by this Present: lists recorded from here on copy what they write.
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("results-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        // List A (legacy barriers): k = 1, then k = 2 over the same outputs, the tickets, a clear of tex.
        allocator.Reset().unwrap();
        list.Reset(&allocator, &write).unwrap();
        list.SetComputeRootSignature(&root);
        list.SetDescriptorHeaps(&[Some(heap.clone())]);
        list.SetComputeRootDescriptorTable(0, gpu(0));
        list.SetComputeRootUnorderedAccessView(1, raw.GetGPUVirtualAddress());
        for r in [&buf, &raw, &tex] {
            transition(&list, r, common, uav);
        }
        list.SetComputeRoot32BitConstant(2, 1, 0);
        list.Dispatch(1, 1, 1);
        uav_barrier(&list);
        list.SetComputeRoot32BitConstant(2, 2, 0);
        list.Dispatch(1, 1, 1);
        uav_barrier(&list);
        list.SetPipelineState(&tickets);
        list.Dispatch(1, 1, 1);
        uav_barrier(&list);
        list.ClearUnorderedAccessViewUint(gpu(1), cpu_uav, &tex, &[9, 9, 9, 9], &[]);
        list.Close().unwrap();

        // List B (enhanced barriers): k = 3 into buf2, ltex and raw2.
        let allocator_b: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list_b: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator_b, &write)
            .unwrap();
        let list7: ID3D12GraphicsCommandList7 = list_b.cast().unwrap();
        for b in [&buf2, &raw2] {
            modern::buffer_barrier(
                &list7,
                b,
                [D3D12_BARRIER_SYNC_NONE, D3D12_BARRIER_SYNC_COMPUTE_SHADING],
                [
                    D3D12_BARRIER_ACCESS_NO_ACCESS,
                    D3D12_BARRIER_ACCESS_UNORDERED_ACCESS,
                ],
            );
        }
        list_b.SetComputeRootSignature(&root);
        list_b.SetDescriptorHeaps(&[Some(heap.clone())]);
        list_b.SetComputeRootDescriptorTable(0, gpu(2));
        list_b.SetComputeRootUnorderedAccessView(1, raw2.GetGPUVirtualAddress());
        list_b.SetComputeRoot32BitConstant(2, 3, 0);
        list_b.Dispatch(1, 1, 1);
        list_b.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap()), Some(list_b.cast().unwrap())]);
        finish(queue, fence, 26);
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
        let blobs = locum_capture::blobs::Blobs::new(dir.join("blobs"));
        // What each dispatch (by its number) and the clear wrote, in event order: (draw, sub, ordered, bytes).
        let rows: Vec<Output> = {
            let mut q = db
                .prepare(
                    "SELECT d.n, r.sub, r.order_dependent, r.content FROM results r LEFT JOIN draws d ON \
                     d.event = r.event ORDER BY r.event, r.resource, r.sub",
                )
                .unwrap();
            q.query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get::<_, i64>(2)? != 0,
                    row.get::<_, String>(3)?,
                ))
            })
            .unwrap()
            .map(|r| {
                let (n, sub, ordered, key) = r.unwrap();
                (n, sub, ordered, blobs.get(&key).unwrap())
            })
            .collect()
        };
        let words = |b: &[u8]| -> Vec<u32> {
            b.chunks(4)
                .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
                .collect()
        };
        let gaps: Vec<String> = {
            let mut q = db
                .prepare("SELECT detail FROM gaps WHERE what = 'd3d12-results'")
                .unwrap();
            q.query_map([], |r| r.get(0))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert!(gaps.is_empty(), "results gaps: {gaps:?}");
        // Dispatches 1, 2 and 4 write three outputs each, the tickets two, the clear one.
        assert_eq!(
            rows.len(),
            12,
            "results: {:?}",
            rows.iter().map(|r| (r.0, r.1, r.2)).collect::<Vec<_>>()
        );
        let of = |n: i64| rows.iter().filter(move |r| r.0 == Some(n));
        for (n, k) in [(1, 1u32), (2, 2)] {
            let outputs: Vec<_> = of(n).collect();
            assert_eq!(outputs.len(), 3, "dispatch {n}'s outputs");
            let buffer = outputs
                .iter()
                .find(|r| r.1.is_none() && r.3.len() == 256)
                .unwrap();
            let expect: Vec<u32> = (0..64).map(|i| if i < 16 { k * 100 + i } else { 0 }).collect();
            assert_eq!(words(&buffer.3), expect, "dispatch {n}: buf as it left it");
            let texture = outputs
                .iter()
                .find(|r| r.1 == Some(3))
                .expect("tex's subresource 3");
            let expect: Vec<u32> = (0..16).map(|i| k + i % 4 + i / 4 * 16).collect();
            assert_eq!(words(&texture.3), expect, "dispatch {n}: tex mip 1 slice 1");
            let raw = outputs
                .iter()
                .find(|r| r.1.is_none() && r.3.len() == 128)
                .unwrap();
            let expect: Vec<u32> = (0..32)
                .map(|i| if i < 16 { k * 1000 + i % 4 } else { 0 })
                .collect();
            assert_eq!(words(&raw.3), expect, "dispatch {n}: raw");
            assert!(
                outputs.iter().all(|r| !r.2),
                "dispatch {n} is not order-dependent"
            );
        }
        let tickets: Vec<_> = of(3).collect();
        assert_eq!(tickets.len(), 2, "the tickets' outputs");
        assert!(tickets.iter().all(|r| r.2), "the tickets are order-dependent");
        let raw = tickets.iter().find(|r| r.3.len() == 128).unwrap();
        let mut taken = words(&raw.3)[16..20].to_vec();
        taken.sort();
        assert_eq!(taken, [0, 1, 2, 3], "each ticket taken once");
        let clear: Vec<_> = rows.iter().filter(|r| r.0.is_none()).collect();
        assert_eq!(clear.len(), 1, "the clear's output");
        assert_eq!(clear[0].1, Some(3));
        assert_eq!(words(&clear[0].3), [9; 16], "the clear of tex mip 1 slice 1");
        let b: Vec<_> = of(4).collect();
        assert_eq!(b.len(), 3, "list B's dispatch's outputs");
        let texture = b.iter().find(|r| r.1 == Some(0)).expect("ltex's subresource 0");
        let expect: Vec<u32> = (0..16).map(|i| 3 + i % 4 + i / 4 * 16).collect();
        assert_eq!(words(&texture.3), expect, "list B: ltex");
        debug_errors(device);
    }
    println!(
        "D3D12 results verified: each dispatch's and the clear's UAV outputs as it left them, an atomic's flagged \
         order-dependent, through legacy and enhanced barriers"
    );
}
