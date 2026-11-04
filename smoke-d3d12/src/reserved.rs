//! Memory resources share. A reserved texture whose tiles map two heaps, one tile left unmapped, first used in the
//! frame, remapped by UpdateTileMappings and CopyTileMappings, then read again; and two buffers placed at one offset
//! of one heap, the frame writing one and reading the other. The capture holds the texture's contents as its first
//! use read them (zero in the unmapped tile), its page table as the frame found it, the frame's two changes in order
//! with the page table after each, the heaps' descriptions and what each placed buffer takes of its heap; replay
//! (xtask) gives the second read the remapped tile, and the read buffer what the frame wrote through the other.
use super::*;
use contents::{buffer_desc, committed, fill, pattern, read};

const TILE: u64 = 65536;

fn coordinate(x: u32, y: u32, subresource: u32) -> D3D12_TILED_RESOURCE_COORDINATE {
    D3D12_TILED_RESOURCE_COORDINATE {
        X: x,
        Y: y,
        Z: 0,
        Subresource: subresource,
    }
}

fn tiles(n: u32) -> D3D12_TILE_REGION_SIZE {
    D3D12_TILE_REGION_SIZE {
        NumTiles: n,
        ..Default::default()
    }
}

unsafe fn heap(device: &ID3D12Device, tiles: u64) -> ID3D12Heap {
    let mut h: Option<ID3D12Heap> = None;
    unsafe {
        device.CreateHeap(
            &D3D12_HEAP_DESC {
                SizeInBytes: tiles * TILE,
                Properties: D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                Alignment: 0,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
            },
            &mut h,
        )
    }
    .unwrap();
    h.unwrap()
}

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
) {
    unsafe {
        use graphics::transition;
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut options).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS>() as u32,
            )
            .unwrap();
        if options.TiledResourcesTier.0 < D3D12_TILED_RESOURCES_TIER_2.0 {
            println!(
                "D3D12 reserved resources: skipped, tiled resources tier {}",
                options.TiledResourcesTier.0
            );
            return;
        }
        let rgba8 = DXGI_FORMAT_R8G8B8A8_UNORM;
        let d = D3D12_RESOURCE_DESC {
            // A reserved texture's tiles are 64 KiB in a layout of the driver's own.
            Layout: D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE,
            ..contents::desc(
                D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                rgba8,
                (256, 256),
                (2, 1, 1),
                D3D12_RESOURCE_FLAG_NONE,
            )
        };
        let mut r: Option<ID3D12Resource> = None;
        device
            .CreateReservedResource(&d, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut r)
            .unwrap();
        let r = r.unwrap();
        // Mip 0 is 2x2 tiles of 128x128 texels (tiles 0 to 3); mip 1 and anything packed follow.
        let mut total = 0;
        device.GetResourceTiling(&r, Some(&mut total), None, None, None, 0, core::ptr::null_mut());
        let rest = total - 4;
        assert!(
            rest >= 1,
            "a 256x256 RGBA8 texture with two mips has more than mip 0's 4 tiles"
        );
        // Sizes no other heap of the smoke test has, so the capture's heaps are found by them.
        let (a, b) = (heap(device, 23), heap(device, u64::from(rest) + 29));
        // Mip 0's tiles 0, 1 and 3 in heap A (tiles 0 to 2), tile 2 unmapped; the rest in heap B.
        queue.UpdateTileMappings(
            &r,
            1,
            Some(&coordinate(0, 0, 0)),
            Some(&tiles(4)),
            &a,
            3,
            Some(
                [
                    D3D12_TILE_RANGE_FLAG_NONE,
                    D3D12_TILE_RANGE_FLAG_SKIP,
                    D3D12_TILE_RANGE_FLAG_NONE,
                ]
                .as_ptr(),
            ),
            Some([0u32, 0, 2].as_ptr()),
            Some([2u32, 1, 1].as_ptr()),
            D3D12_TILE_MAPPING_FLAG_NONE,
        );
        queue.UpdateTileMappings(
            &r,
            1,
            Some(&coordinate(0, 0, 1)),
            Some(&tiles(rest)),
            &b,
            1,
            None,
            Some(&0u32),
            Some(&rest),
            D3D12_TILE_MAPPING_FLAG_NONE,
        );
        let subs = locum_capture::layout::texture(28, 256, 256, 1, 2, 1).unwrap();
        let data = pattern(&subs, 77);
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        let mut keep = Vec::new();
        fill(device, &list, &r, &data, &mut keep);
        transition(
            &list,
            &r,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 40);

        // The frame: the texture's first use, then two changes to its mappings.
        let readback = committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &buffer_desc(512 << 10),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        allocator.Reset().unwrap();
        list.Reset(&allocator, None).unwrap();
        transition(
            &list,
            &r,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        read(device, &list, &r, &readback, 0);
        transition(
            &list,
            &r,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        list.Close().unwrap();
        // Two buffers at offset 0 of one heap (of a size no other heap of the smoke test has), and the bytes the
        // frame writes through the first.
        let shared = {
            let mut h: Option<ID3D12Heap> = None;
            device
                .CreateHeap(
                    &D3D12_HEAP_DESC {
                        SizeInBytes: 5 * TILE,
                        Properties: D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        Alignment: 0,
                        Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                    },
                    &mut h,
                )
                .unwrap();
            h.unwrap()
        };
        let placed = || {
            let mut b: Option<ID3D12Resource> = None;
            device
                .CreatePlacedResource(
                    &shared,
                    0,
                    &buffer_desc(TILE),
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut b,
                )
                .unwrap();
            b.unwrap()
        };
        let (written, aliased) = (placed(), placed());
        let bytes: Vec<u8> = (0..TILE as usize).map(|i| (i * 7 + 3) as u8).collect();
        let upload = committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &buffer_desc(TILE),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut p = core::ptr::null_mut();
        upload.Map(0, None, Some(&mut p)).unwrap();
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        upload.Unmap(0, None);
        let after = committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &buffer_desc(512 << 10),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let allocator2: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list2: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator2, None)
            .unwrap();
        list2.Close().unwrap();
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("reserved-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        // Mip 1's first tile unmapped; tile 2 mapped where tile 0 is.
        queue.UpdateTileMappings(
            &r,
            1,
            Some(&coordinate(0, 0, 1)),
            Some(&tiles(1)),
            None::<&ID3D12Heap>,
            1,
            Some(&D3D12_TILE_RANGE_FLAG_NULL),
            None,
            Some(&1u32),
            D3D12_TILE_MAPPING_FLAG_NONE,
        );
        queue.CopyTileMappings(
            &r,
            &coordinate(0, 1, 0),
            &r,
            &coordinate(0, 0, 0),
            &tiles(1),
            D3D12_TILE_MAPPING_FLAG_NONE,
        );
        // Mip 0 again after the changes; then the bytes written through one placed buffer, read through the other.
        allocator2.Reset().unwrap();
        list2.Reset(&allocator2, None).unwrap();
        transition(
            &list2,
            &r,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        read(device, &list2, &r, &after, 0);
        transition(
            &list2,
            &r,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        list2.CopyBufferRegion(&written, 0, &upload, 0, TILE);
        let mut alias = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_ALIASING,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Aliasing: ManuallyDrop::new(D3D12_RESOURCE_ALIASING_BARRIER {
                    pResourceBefore: ManuallyDrop::new(Some(written.clone())),
                    pResourceAfter: ManuallyDrop::new(Some(aliased.clone())),
                }),
            },
        };
        list2.ResourceBarrier(core::slice::from_ref(&alias));
        ManuallyDrop::drop(&mut (*alias.Anonymous.Aliasing).pResourceBefore);
        ManuallyDrop::drop(&mut (*alias.Anonymous.Aliasing).pResourceAfter);
        list2.CopyBufferRegion(&after, 256 << 10, &aliased, 0, TILE);
        list2.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list2.cast().unwrap())]);
        finish(queue, fence, 41);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        // What the GPU read: tile 2 (x 0..128, y 128..256 of mip 0) as tile 0, the rest as filled; the aliased
        // buffer as written through the other.
        let mut p = core::ptr::null_mut();
        after.Map(0, None, Some(&mut p)).unwrap();
        let got = core::slice::from_raw_parts(p.cast::<u8>(), 512 << 10);
        let filled = &data[0];
        for y in 0..256 {
            for x in 0..1024 {
                let from = if y >= 128 && x < 512 {
                    (y - 128) * 1024 + x
                } else {
                    y * 1024 + x
                };
                assert_eq!(
                    got[y * 1024 + x],
                    filled[from],
                    "mip 0 after the remaps at byte {x} of row {y}"
                );
            }
        }
        assert!(
            got[256 << 10..(256 << 10) + TILE as usize] == bytes[..],
            "the aliased buffer holds what was written through the other"
        );
        after.Unmap(0, None);

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
        let (id, content, source, note): (i64, Option<String>, Option<String>, Option<String>) = db
            .query_row(
                "SELECT id, content, content_source, content_note FROM resources WHERE width=256 AND height=256 AND mips=2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(source.as_deref(), Some("first-use"), "reserved texture: {note:?}");
        // As its first use read it: the fill, except tile 2 (texels x 0..128, y 128..256 of mip 0), which reads zero.
        let mut expected = data.concat();
        for y in 128..256 {
            expected[y * 1024..y * 1024 + 512].fill(0);
        }
        assert!(
            blobs.get(&content.unwrap()).unwrap() == expected,
            "reserved texture: contents as first used, tile 2 zero"
        );
        let heap_id = |tiles: u64| -> i64 {
            db.query_row(
                "SELECT id FROM d12_objects WHERE kind LIKE 'ID3D12Heap%' AND json_extract(description, '$.bytes') = ?1",
                [(tiles * TILE) as i64],
                |r| r.get(0),
            )
            .unwrap_or_else(|e| panic!("heap of {tiles} tiles: {e}"))
        };
        let (a_id, b_id) = (heap_id(23), heap_id(u64::from(rest) + 29));
        let rest = u64::from(rest);
        let (layout, pages): (String, String) = db
            .query_row(
                "SELECT layout_json, pages_json FROM d12_tile_pages WHERE resource = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let layout =
            locum_capture::tiles::Layout::from_json(&serde_json::from_str(&layout).unwrap()).unwrap();
        assert_eq!(layout.total, total, "the page table's layout is the device's");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&pages).unwrap(),
            serde_json::json!([[0, 2, a_id, 0], [3, 1, a_id, 2], [4, rest, b_id, 0]]),
            "the page table as the frame found it"
        );
        let changes: Vec<(String, String)> = db
            .prepare("SELECT call, pages_json FROM d12_tile_mappings WHERE resource = ?1 ORDER BY cpu_seq")
            .unwrap()
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let after = |i: usize| serde_json::from_str::<serde_json::Value>(&changes[i].1).unwrap();
        assert_eq!(changes.len(), 2, "the frame's two changes: {changes:?}");
        assert_eq!(changes[0].0, "UpdateTileMappings");
        let rest_after = if rest > 1 {
            serde_json::json!([[0, 2, a_id, 0], [3, 1, a_id, 2], [5, rest - 1, b_id, 1]])
        } else {
            serde_json::json!([[0, 2, a_id, 0], [3, 1, a_id, 2]])
        };
        assert_eq!(after(0), rest_after, "mip 1's first tile unmapped");
        assert_eq!(changes[1].0, "CopyTileMappings");
        let mut copied = rest_after.as_array().unwrap().clone();
        copied.insert(1, serde_json::json!([2, 1, a_id, 0]));
        assert_eq!(
            after(1),
            serde_json::Value::Array(copied),
            "tile 2 mapped where tile 0 is"
        );
        println!(
            "D3D12 reserved resources: contents as first used (unmapped tile zero), page table over two heaps, \
             UpdateTileMappings and CopyTileMappings in the frame"
        );
        drop(keep);
    }
}
