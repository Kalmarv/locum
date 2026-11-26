//! A dispatch whose root signature indexes the CBV, SRV and UAV heap directly: its program reads a texture and a
//! buffer through `ResourceDescriptorHeap`, which no descriptor table names, and writes them out through a root UAV.
//! Every resource the heap's slots name gets a first-use copy, a texture the program never reads too, and the
//! dispatch's output replays as the application's.
use super::*;
use contents::{buffer_desc, committed};

/// Widths no other smoke resource has, to find them in the capture by.
const BYTES: u64 = 3 * 4096 + 256;
const SIDE: u32 = 6;
const OTHER: u32 = 10;

const SOURCE: &str = "RWByteAddressBuffer o : register(u0);
[numthreads(1, 1, 1)] void main() {
    Texture2D<uint4> t = ResourceDescriptorHeap[1];
    ByteAddressBuffer b = ResourceDescriptorHeap[2];
    o.Store4(0, t.Load(int3(1, 2, 0)));
    o.Store4(16, b.Load4(32));
}";

fn byte(i: u64) -> u8 {
    (i * 7 + (i >> 8)) as u8
}

/// The dispatch's pipeline, where the device has shader model 6.6 and DXIL can be compiled; or why not.
unsafe fn pipeline(
    device: &ID3D12Device,
) -> std::result::Result<(ID3D12RootSignature, ID3D12PipelineState), String> {
    unsafe {
        let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
            HighestShaderModel: D3D_SHADER_MODEL_6_6,
        };
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_SHADER_MODEL,
                (&mut model as *mut D3D12_FEATURE_DATA_SHADER_MODEL).cast(),
                size_of::<D3D12_FEATURE_DATA_SHADER_MODEL>() as u32,
            )
            .map_err(|e| format!("CheckFeatureSupport(SHADER_MODEL): {e}"))?;
        if model.HighestShaderModel.0 < D3D_SHADER_MODEL_6_6.0 {
            return Err("the device has no shader model 6.6".into());
        }
        let code = indirect::dxil(SOURCE, "cs_6_6")?;
        let parameter = D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_UAV,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        };
        let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
            Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
            Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                    NumParameters: 1,
                    pParameters: &parameter,
                    Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED,
                    ..Default::default()
                },
            },
        };
        let mut serialized = None;
        D3D12SerializeVersionedRootSignature(&desc, &mut serialized, None).map_err(|e| e.to_string())?;
        let serialized = serialized.ok_or("no root signature")?;
        let root: ID3D12RootSignature = device
            .CreateRootSignature(
                0,
                core::slice::from_raw_parts(serialized.GetBufferPointer().cast(), serialized.GetBufferSize()),
            )
            .map_err(|e| e.to_string())?;
        let mut c = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            },
            ..Default::default()
        };
        let pso = device.CreateComputePipelineState(&c).map_err(|e| e.to_string());
        ManuallyDrop::drop(&mut c.pRootSignature);
        Ok((root, pso?))
    }
}

/// A texture of `side` by `side` RGBA8 integers, filled with `data` through `list`.
unsafe fn texture(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    side: u32,
    data: &[Vec<u8>],
    keep: &mut Vec<ID3D12Resource>,
) -> ID3D12Resource {
    unsafe {
        let t = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &contents::desc(
                D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                DXGI_FORMAT_R8G8B8A8_UINT,
                (side as u64, side),
                (1, 1, 1),
                D3D12_RESOURCE_FLAG_NONE,
            ),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        contents::fill(device, list, &t, data, keep);
        graphics::transition(
            list,
            &t,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        );
        t
    }
}

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    n: u64,
) -> std::result::Result<(), String> {
    unsafe {
        let (root, pso) = pipeline(device)?;
        let subs = |side| locum_capture::layout::texture(30, side, side, 1, 1, 1).unwrap();
        let (read, unread) = (
            contents::pattern(&subs(SIDE), 90),
            contents::pattern(&subs(OTHER), 140),
        );
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        let mut keep = Vec::new();
        let read_texture = texture(device, &list, SIDE, &read, &mut keep);
        let unread_texture = texture(device, &list, OTHER, &unread, &mut keep);
        let buffer = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &buffer_desc(BYTES),
            D3D12_RESOURCE_STATE_COMMON,
        );
        let upload = committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &buffer_desc(BYTES),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut p = core::ptr::null_mut();
        upload.Map(0, None, Some(&mut p)).unwrap();
        let bytes: Vec<u8> = (0..BYTES).map(byte).collect();
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        upload.Unmap(0, None);
        list.CopyBufferRegion(&buffer, 0, &upload, 0, BYTES);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, n);

        // Slot 1 the texture the program reads, 2 the buffer (raw), 3 a texture it does not; slot 0 holds nothing.
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 4,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                ..Default::default()
            })
            .unwrap();
        let stride = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
        let slot = |i: usize| D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: heap.GetCPUDescriptorHandleForHeapStart().ptr + i * stride,
        };
        device.CreateShaderResourceView(&read_texture, None, slot(1));
        let raw = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R32_TYPELESS,
            ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Buffer: D3D12_BUFFER_SRV {
                    FirstElement: 0,
                    NumElements: (BYTES / 4) as u32,
                    StructureByteStride: 0,
                    Flags: D3D12_BUFFER_SRV_FLAG_RAW,
                },
            },
        };
        device.CreateShaderResourceView(&buffer, Some(&raw), slot(2));
        device.CreateShaderResourceView(&unread_texture, None, slot(3));
        let output = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &contents::desc(
                D3D12_RESOURCE_DIMENSION_BUFFER,
                DXGI_FORMAT_UNKNOWN,
                (256, 1),
                (1, 1, 1),
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ),
            D3D12_RESOURCE_STATE_COMMON,
        );

        // The frame, armed by this Present: the list recorded after it copies what the dispatch writes.
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("heap-indexed-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        allocator.Reset().unwrap();
        list.Reset(&allocator, &pso).unwrap();
        list.SetDescriptorHeaps(&[Some(heap.clone())]);
        list.SetComputeRootSignature(&root);
        list.SetComputeRootUnorderedAccessView(0, output.GetGPUVirtualAddress());
        list.Dispatch(1, 1, 1);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, n + 1);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        let path = dir.join("captures").join(&label).join("capture.db");
        let until = Instant::now() + Duration::from_secs(60);
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
        // Each named only by the heap, copied before the dispatch's submission as the frame found it.
        let copied = |kind: &str, width: u64| -> Vec<u8> {
            let (content, source, note): (Option<String>, Option<String>, Option<String>) = db
                .query_row(
                    "SELECT content, content_source, content_note FROM resources WHERE kind=?1 AND width=?2",
                    rusqlite::params![kind, width as i64],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(source.as_deref(), Some("first-use"), "{kind} {width}: {note:?}");
            assert!(
                note.as_deref()
                    .is_some_and(|n| n.contains("named only by a descriptor heap")),
                "{kind} {width}: {note:?}"
            );
            blobs.get(&content.unwrap()).unwrap()
        };
        assert_eq!(copied("buffer", BYTES), bytes, "the buffer the program reads");
        assert_eq!(
            copied("texture2d", SIDE as u64),
            read.concat(),
            "the texture it reads"
        );
        assert_eq!(
            copied("texture2d", OTHER as u64),
            unread.concat(),
            "the texture it does not"
        );
        let results: i64 = db
            .query_row("SELECT count(*) FROM results", [], |r| r.get(0))
            .unwrap();
        assert_eq!(results, 1, "the dispatch's output copied");
        // The closing Present's work, and locum's after it, done before the resources go.
        finish(queue, fence, n + 2);
        drop(keep);
        debug_errors(device);
    }
    println!(
        "D3D12 heap-indexed: a dispatch that reads through ResourceDescriptorHeap has every resource its heap names copied"
    );
    Ok(())
}
