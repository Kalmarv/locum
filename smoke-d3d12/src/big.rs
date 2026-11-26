//! Buffers bigger than one readback chunk (256 MiB): one is copied in pieces of a chunk each, and a reserved one only
//! as its mapped tiles, the rest zeros. Each is named in the capture by one key that reads as the whole buffer (a
//! pieced blob), as the frame found it.
use super::*;
use contents::{buffer_desc, committed};

const TILE: u64 = 65536;

/// The byte at `i` of the smoke test's big pattern: it differs between pieces and between tiles.
fn byte(i: u64) -> u8 {
    (i ^ (i >> 8) ^ (i >> 16) ^ (i >> 24)) as u8
}

/// An upload buffer of `bytes` holding [`byte`] from `from` on.
unsafe fn upload(device: &ID3D12Device, from: u64, bytes: u64) -> ID3D12Resource {
    unsafe {
        let b = committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &buffer_desc(bytes),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut p = core::ptr::null_mut();
        b.Map(0, None, Some(&mut p)).unwrap();
        let s = core::slice::from_raw_parts_mut(p.cast::<u8>(), bytes as usize);
        for (i, v) in s.iter_mut().enumerate() {
            *v = byte(from + i as u64);
        }
        b.Unmap(0, None);
        b
    }
}

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    n: u64,
) {
    unsafe {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut options).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS>() as u32,
            )
            .unwrap();
        let reserved_too = options.TiledResourcesTier.0 >= D3D12_TILED_RESOURCES_TIER_2.0;
        // A buffer of a chunk and 4 MiB: two pieces.
        const PLAIN: u64 = (256 << 20) + (4 << 20);
        let plain = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &buffer_desc(PLAIN),
            D3D12_RESOURCE_STATE_COMMON,
        );
        let plain_data = upload(device, 0, PLAIN);
        // A reserved buffer of 512 MiB (8192 tiles), tiles 3-4 and 6000 mapped into one heap.
        const RESERVED: u64 = 512 << 20;
        let mapped: [(u64, u64); 2] = [(3, 2), (6000, 1)];
        let reserved = reserved_too.then(|| {
            let mut r: Option<ID3D12Resource> = None;
            device
                .CreateReservedResource(&buffer_desc(RESERVED), D3D12_RESOURCE_STATE_COMMON, None, &mut r)
                .unwrap();
            let r = r.unwrap();
            let mut h: Option<ID3D12Heap> = None;
            device
                .CreateHeap(
                    &D3D12_HEAP_DESC {
                        SizeInBytes: 3 * TILE,
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
            let h = h.unwrap();
            let mut heap_tile = 0u32;
            for (first, count) in mapped {
                queue.UpdateTileMappings(
                    &r,
                    1,
                    Some(&D3D12_TILED_RESOURCE_COORDINATE {
                        X: first as u32,
                        ..Default::default()
                    }),
                    Some(&D3D12_TILE_REGION_SIZE {
                        NumTiles: count as u32,
                        ..Default::default()
                    }),
                    &h,
                    1,
                    None,
                    Some(&heap_tile),
                    Some(&(count as u32)),
                    D3D12_TILE_MAPPING_FLAG_NONE,
                );
                heap_tile += count as u32;
            }
            (r, h)
        });
        let tile_data: Vec<ID3D12Resource> = mapped
            .iter()
            .map(|&(first, count)| upload(device, first * TILE, count * TILE))
            .collect();

        // Before the frame: the buffers filled.
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        list.CopyBufferRegion(&plain, 0, &plain_data, 0, PLAIN);
        if let Some((r, _)) = &reserved {
            for (&(first, count), data) in mapped.iter().zip(&tile_data) {
                list.CopyBufferRegion(r, first * TILE, data, 0, count * TILE);
            }
        }
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, n);

        // The frame reads 256 bytes of each.
        let readback = committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &buffer_desc(512),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let frame_allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let frame: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &frame_allocator, None)
            .unwrap();
        frame.CopyBufferRegion(&readback, 0, &plain, PLAIN - 256, 256);
        if let Some((r, _)) = &reserved {
            frame.CopyBufferRegion(&readback, 256, r, 6000 * TILE, 256);
        }
        frame.Close().unwrap();
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("big-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        queue.ExecuteCommandLists(&[Some(frame.cast().unwrap())]);
        finish(queue, fence, n + 1);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        let mut p = core::ptr::null_mut();
        readback.Map(0, None, Some(&mut p)).unwrap();
        let read = core::slice::from_raw_parts(p.cast::<u8>(), 512).to_vec();
        readback.Unmap(0, None);
        assert!(
            (0..256).all(|i| read[i as usize] == byte(PLAIN - 256 + i)),
            "the frame's own read of the plain buffer"
        );

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
        // (found by size: a width past 4 GiB is recorded whole)
        let content = |width: u64| -> String {
            let (content, source, note): (Option<String>, Option<String>, Option<String>) = db
                .query_row(
                    "SELECT content, content_source, content_note FROM resources WHERE kind='buffer' AND width=?1",
                    [width as i64],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap();
            assert_eq!(
                source.as_deref(),
                Some("first-use"),
                "buffer of {width} bytes: {note:?}"
            );
            content.unwrap()
        };
        let key = content(PLAIN);
        let pieces = blobs.pieces(&key).unwrap().expect("a pieced blob");
        assert_eq!(
            pieces.iter().map(|p| p.1).collect::<Vec<_>>(),
            [256 << 20, 4 << 20],
            "a chunk and the rest"
        );
        assert_eq!(blobs.size(&key), Some(PLAIN));
        let bytes = blobs.get(&key).unwrap();
        assert!(
            bytes.iter().enumerate().all(|(i, &v)| v == byte(i as u64)),
            "the plain buffer as filled"
        );
        drop(bytes);
        if reserved.is_some() {
            assert!(
                (0..256).all(|i| read[256 + i as usize] == byte(6000 * TILE + i)),
                "the frame's own read of the reserved buffer"
            );
            let key = content(RESERVED);
            let pieces = blobs.pieces(&key).unwrap().expect("a pieced blob");
            let zeros = locum_capture::blobs::ZEROS;
            assert_eq!(
                pieces
                    .iter()
                    .map(|(k, n)| (k == zeros, *n / TILE))
                    .collect::<Vec<_>>(),
                [
                    (true, 3),
                    (false, 2),
                    (true, 5995),
                    (false, 1),
                    (true, 8192 - 6001)
                ],
                "the mapped tiles copied, the rest zeros"
            );
            let bytes = blobs.get(&key).unwrap();
            assert_eq!(bytes.len() as u64, RESERVED);
            let in_tile = |i: u64| mapped.iter().any(|&(f, c)| i / TILE >= f && i / TILE < f + c);
            assert!(
                bytes
                    .iter()
                    .enumerate()
                    .all(|(i, &v)| v == if in_tile(i as u64) { byte(i as u64) } else { 0 }),
                "mapped tiles as filled, unmapped ones zero"
            );
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM gaps WHERE what='contents'", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
        // The frame's closing Present queues work too: complete it before the buffers go.
        finish(queue, fence, n + 2);
        debug_errors(device);
        drop(reserved);
    }
    println!(
        "D3D12 big buffers verified: a buffer of more than a readback chunk in pieces, a reserved one as its mapped \
         tiles and zeros"
    );
}
