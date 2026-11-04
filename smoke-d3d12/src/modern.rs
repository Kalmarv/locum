//! The newest interfaces through the wrappers, with real WARP output: an ID3D12Device10 buffer created in a barrier
//! layout, written by WriteBufferImmediate between enhanced barriers on ID3D12GraphicsCommandList7, and a native
//! render pass that clears an attachment.
use super::*;

unsafe fn sized(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
    bytes: u64,
) -> ID3D12Resource {
    let mut r = None;
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: heap,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: bytes,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            },
            state,
            None,
            &mut r,
        )
    }
    .unwrap();
    r.unwrap()
}

unsafe fn buffer_barrier(
    list: &ID3D12GraphicsCommandList7,
    resource: &ID3D12Resource,
    sync: [D3D12_BARRIER_SYNC; 2],
    access: [D3D12_BARRIER_ACCESS; 2],
) {
    let mut barrier = D3D12_BUFFER_BARRIER {
        SyncBefore: sync[0],
        SyncAfter: sync[1],
        AccessBefore: access[0],
        AccessAfter: access[1],
        pResource: ManuallyDrop::new(Some(resource.clone())),
        Offset: 0,
        Size: u64::MAX,
    };
    let group = D3D12_BARRIER_GROUP {
        Type: D3D12_BARRIER_TYPE_BUFFER,
        NumBarriers: 1,
        Anonymous: D3D12_BARRIER_GROUP_0 {
            pBufferBarriers: &barrier,
        },
    };
    unsafe {
        list.Barrier(&[group]);
        ManuallyDrop::drop(&mut barrier.pResource);
    }
}

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
) {
    unsafe {
        let device10: ID3D12Device10 = device.cast().expect("the runtime offers ID3D12Device10");
        identity(device, &device10);
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS12::default();
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS12,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .unwrap();
        assert!(
            options.EnhancedBarriersSupported.as_bool(),
            "WARP supports enhanced barriers"
        );

        // A default-heap buffer created through the enhanced-barrier creator: buffers begin in the UNDEFINED layout.
        let mut target: Option<ID3D12Resource> = None;
        device10
            .CreateCommittedResource3(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC1 {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: 256,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                },
                D3D12_BARRIER_LAYOUT_UNDEFINED,
                None,
                None,
                None,
                &mut target,
            )
            .unwrap();
        let target = target.unwrap();
        let words = sized(
            device,
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_STATE_COPY_DEST,
            256,
        );

        // An 8x8 attachment cleared by a native render pass.
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
        let mut texture: Option<ID3D12Resource> = None;
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
                &mut texture,
            )
            .unwrap();
        let texture = texture.unwrap();
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: 1,
                ..Default::default()
            })
            .unwrap();
        let rtv = heap.GetCPUDescriptorHandleForHeapStart();
        device.CreateRenderTargetView(&texture, None, rtv);
        let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        let mut bytes = 0u64;
        device.GetCopyableFootprints(&desc, 0, 1, 0, Some(&mut footprint), None, None, Some(&mut bytes));
        let pixels = sized(
            device,
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_STATE_COPY_DEST,
            bytes,
        );

        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        let list7: ID3D12GraphicsCommandList7 = list
            .cast()
            .expect("the runtime offers ID3D12GraphicsCommandList7");
        identity(&list, &list7);
        let address = target.GetGPUVirtualAddress();
        buffer_barrier(
            &list7,
            &target,
            [D3D12_BARRIER_SYNC_NONE, D3D12_BARRIER_SYNC_COPY],
            [D3D12_BARRIER_ACCESS_NO_ACCESS, D3D12_BARRIER_ACCESS_COPY_DEST],
        );
        let writes = [
            D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
                Dest: address,
                Value: 0xAABB_CCDD,
            },
            D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
                Dest: address + 4,
                Value: 0x1122_3344,
            },
        ];
        list7.WriteBufferImmediate(2, writes.as_ptr(), None);
        buffer_barrier(
            &list7,
            &target,
            [D3D12_BARRIER_SYNC_COPY, D3D12_BARRIER_SYNC_COPY],
            [D3D12_BARRIER_ACCESS_COPY_DEST, D3D12_BARRIER_ACCESS_COPY_SOURCE],
        );
        list.CopyBufferRegion(&words, 0, &target, 0, 8);
        let mut clear = D3D12_CLEAR_VALUE {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ..Default::default()
        };
        clear.Anonymous.Color = [0.25, 0.5, 0.75, 1.0];
        let attachment = D3D12_RENDER_PASS_RENDER_TARGET_DESC {
            cpuDescriptor: rtv,
            BeginningAccess: D3D12_RENDER_PASS_BEGINNING_ACCESS {
                Type: D3D12_RENDER_PASS_BEGINNING_ACCESS_TYPE_CLEAR,
                Anonymous: D3D12_RENDER_PASS_BEGINNING_ACCESS_0 {
                    Clear: D3D12_RENDER_PASS_BEGINNING_ACCESS_CLEAR_PARAMETERS { ClearValue: clear },
                },
            },
            EndingAccess: D3D12_RENDER_PASS_ENDING_ACCESS {
                Type: D3D12_RENDER_PASS_ENDING_ACCESS_TYPE_PRESERVE,
                ..Default::default()
            },
        };
        list7.BeginRenderPass(Some(&[attachment]), None, D3D12_RENDER_PASS_FLAG_NONE);
        list7.EndRenderPass();
        graphics::transition(
            &list,
            &texture,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        let mut dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(pixels.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: footprint,
            },
        };
        let mut src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(texture.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        ManuallyDrop::drop(&mut dst.pResource);
        ManuallyDrop::drop(&mut src.pResource);
        list.Close().unwrap();

        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("modern-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 12);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        // The GPU ran the forwarded commands: the immediate words, and the render pass's clear colour.
        let mut p = std::ptr::null_mut();
        words.Map(0, None, Some(&mut p)).unwrap();
        assert_eq!(
            std::slice::from_raw_parts(p.cast::<u8>(), 8),
            &[0xDD, 0xCC, 0xBB, 0xAA, 0x44, 0x33, 0x22, 0x11]
        );
        words.Unmap(0, None);
        pixels.Map(0, None, Some(&mut p)).unwrap();
        for y in 0..8 {
            let row = p.cast::<u8>().add(y * footprint.Footprint.RowPitch as usize);
            for x in 0..8 {
                assert_eq!(
                    std::slice::from_raw_parts(row.add(x * 4), 4),
                    &[64, 128, 191, 255]
                );
            }
        }
        pixels.Unmap(0, None);

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
        let target_id: i64 = db
            .query_row(
                "SELECT id FROM d12_objects WHERE description LIKE '%\"initial_layout\"%'",
                [],
                |r| r.get(0),
            )
            .expect("the CreateCommittedResource3 buffer is described with its initial layout");
        for (sql, n) in [
            (
                "SELECT count(*) FROM events WHERE call='WriteBufferImmediate' AND kind='update' \
                 AND json_extract(args,'$[0][0].value')=2864434397 AND json_extract(args,'$[0][1].value')=287454020",
                1,
            ),
            (
                &format!(
                    "SELECT count(*) FROM events WHERE call='Barrier' AND kind='barrier' \
                     AND json_extract(args,'$[0][0].barriers[0].resource')={target_id}"
                ),
                2,
            ),
            (
                "SELECT count(*) FROM events WHERE call='BeginRenderPass' \
                 AND json_extract(args,'$[0][0].begin.clear.color[2]')=0.75",
                1,
            ),
            ("SELECT count(*) FROM events WHERE call='EndRenderPass'", 1),
        ] {
            assert_eq!(db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap(), n, "{sql}");
        }
        let log = std::fs::read_to_string(dir.join("locum.log")).unwrap();
        assert!(
            !log.contains("interface escape"),
            "every interface the fixture used was wrapped"
        );
        debug_errors(device);
    }
    println!(
        "ID3D12Device10 layout creation, enhanced barriers, immediate writes and a native render pass verified"
    );
}
