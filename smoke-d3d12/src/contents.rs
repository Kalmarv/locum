//! First-use contents: resources in every kind of state an armed frame first finds them in, copied on the GPU before
//! the frame's first use and checked byte for byte, under the debug layer, which validates locum's own transitions.
//! A render target the frame clears again must keep its previous colour in the capture; a multisampled one is a gap.
use super::*;

const RGBA8: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;

fn desc(
    dimension: D3D12_RESOURCE_DIMENSION,
    format: DXGI_FORMAT,
    (width, height): (u64, u32),
    (mips, array, samples): (u16, u16, u32),
    flags: D3D12_RESOURCE_FLAGS,
) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: dimension,
        Width: width,
        Height: height,
        DepthOrArraySize: array,
        MipLevels: mips,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: samples,
            Quality: 0,
        },
        Layout: if dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
            D3D12_TEXTURE_LAYOUT_ROW_MAJOR
        } else {
            D3D12_TEXTURE_LAYOUT_UNKNOWN
        },
        Flags: flags,
        ..Default::default()
    }
}

unsafe fn committed(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
    d: &D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> ID3D12Resource {
    let mut r = None;
    unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: heap,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            d,
            state,
            None,
            &mut r,
        )
    }
    .unwrap();
    r.unwrap()
}

fn buffer_desc(bytes: u64) -> D3D12_RESOURCE_DESC {
    desc(
        D3D12_RESOURCE_DIMENSION_BUFFER,
        DXGI_FORMAT_UNKNOWN,
        (bytes, 1),
        (1, 1, 1),
        D3D12_RESOURCE_FLAG_NONE,
    )
}

/// Each subresource's bytes, tightly packed: a pattern that differs per subresource.
fn pattern(subs: &[locum_capture::layout::Sub], seed: u8) -> Vec<Vec<u8>> {
    subs.iter()
        .map(|s| {
            (0..s.size)
                .map(|i| {
                    seed.wrapping_add((s.index as u8).wrapping_mul(37))
                        .wrapping_add(i as u8)
                })
                .collect()
        })
        .collect()
}

/// Copies `data` (each subresource tightly packed) into `texture` through an upload buffer, kept alive in `keep`.
unsafe fn fill(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    data: &[Vec<u8>],
    keep: &mut Vec<ID3D12Resource>,
) {
    unsafe {
        let d = texture.GetDesc();
        let n = data.len();
        let mut layouts = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); n];
        let mut rows = vec![0u32; n];
        let mut row_bytes = vec![0u64; n];
        let mut total = 0;
        device.GetCopyableFootprints(
            &d,
            0,
            n as u32,
            0,
            Some(layouts.as_mut_ptr()),
            Some(rows.as_mut_ptr()),
            Some(row_bytes.as_mut_ptr()),
            Some(&mut total),
        );
        let upload = committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &buffer_desc(total),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut p = core::ptr::null_mut();
        upload.Map(0, None, Some(&mut p)).unwrap();
        for i in 0..n {
            let (f, r, b) = (layouts[i], rows[i] as usize, row_bytes[i] as usize);
            for y in 0..r {
                core::ptr::copy_nonoverlapping(
                    data[i][y * b..].as_ptr(),
                    p.cast::<u8>()
                        .add(f.Offset as usize + y * f.Footprint.RowPitch as usize),
                    b,
                );
            }
            let mut dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(texture.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: i as u32,
                },
            };
            let mut src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: ManuallyDrop::new(Some(upload.clone())),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
            };
            list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
            ManuallyDrop::drop(&mut dst.pResource);
            ManuallyDrop::drop(&mut src.pResource);
        }
        upload.Unmap(0, None);
        keep.push(upload);
    }
}

/// The application's own read of a texture's subresource 0: a copy into `readback` at `offset`.
unsafe fn read(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    texture: &ID3D12Resource,
    readback: &ID3D12Resource,
    offset: u64,
) -> D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
    unsafe {
        let d = texture.GetDesc();
        let mut f = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
        device.GetCopyableFootprints(&d, 0, 1, offset, Some(&mut f), None, None, None);
        let mut dst = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(readback.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: f },
        };
        let mut src = D3D12_TEXTURE_COPY_LOCATION {
            pResource: ManuallyDrop::new(Some(texture.clone())),
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: 0 },
        };
        list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        ManuallyDrop::drop(&mut dst.pResource);
        ManuallyDrop::drop(&mut src.pResource);
        f
    }
}

unsafe fn layout_barrier(
    list: &ID3D12GraphicsCommandList7,
    texture: &ID3D12Resource,
    sync: [D3D12_BARRIER_SYNC; 2],
    access: [D3D12_BARRIER_ACCESS; 2],
    layout: [D3D12_BARRIER_LAYOUT; 2],
) {
    let mut barrier = D3D12_TEXTURE_BARRIER {
        SyncBefore: sync[0],
        SyncAfter: sync[1],
        AccessBefore: access[0],
        AccessAfter: access[1],
        LayoutBefore: layout[0],
        LayoutAfter: layout[1],
        pResource: ManuallyDrop::new(Some(texture.clone())),
        Subresources: D3D12_BARRIER_SUBRESOURCE_RANGE {
            IndexOrFirstMipLevel: 0xFFFF_FFFF,
            ..Default::default()
        },
        Flags: D3D12_TEXTURE_BARRIER_FLAG_NONE,
    };
    let group = D3D12_BARRIER_GROUP {
        Type: D3D12_BARRIER_TYPE_TEXTURE,
        NumBarriers: 1,
        Anonymous: D3D12_BARRIER_GROUP_0 {
            pTextureBarriers: &barrier,
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
        use graphics::transition;
        let tex2d = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        let rt = D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET;
        let none = D3D12_RESOURCE_FLAG_NONE;
        let layout = |format: u32, w: u32, h: u32, mips: u32, array: u32| {
            locum_capture::layout::texture(format, w, h, 1, mips, array).unwrap()
        };
        // A: mips and slices, in PIXEL_SHADER_RESOURCE.
        let a_subs = layout(28, 4, 4, 3, 2);
        let a_data = pattern(&a_subs, 1);
        let a = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &desc(tex2d, RGBA8, (4, 4), (3, 2, 1), none),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        // B: a render target the frame clears again.
        let b = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &desc(tex2d, RGBA8, (12, 8), (1, 1, 1), rt),
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        // C: depth and stencil in DEPTH_WRITE, two planes.
        let c = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &desc(
                tex2d,
                DXGI_FORMAT_D24_UNORM_S8_UINT,
                (8, 8),
                (1, 1, 1),
                D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
            ),
            D3D12_RESOURCE_STATE_DEPTH_WRITE,
        );
        // D: made in COMMON and filled without a barrier, which promotes it to COPY_DEST for good.
        let d_subs = layout(28, 4, 2, 1, 1);
        let d_data = pattern(&d_subs, 90);
        let d = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &desc(tex2d, RGBA8, (4, 2), (1, 1, 1), none),
            D3D12_RESOURCE_STATE_COMMON,
        );
        // E: a buffer.
        let e = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &buffer_desc(320),
            D3D12_RESOURCE_STATE_COMMON,
        );
        // F: multisampled.
        let f = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &desc(tex2d, RGBA8, (8, 8), (1, 1, 4), rt),
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        // G: block-compressed with two mips, in NON_PIXEL_SHADER_RESOURCE.
        let g_subs = layout(71, 8, 8, 2, 1);
        let g_data = pattern(&g_subs, 200);
        let g = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &desc(tex2d, DXGI_FORMAT_BC1_UNORM, (8, 8), (2, 1, 1), none),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        // H: made in a layout and moved to SHADER_RESOURCE by enhanced barriers.
        let h_subs = layout(28, 2, 4, 1, 1);
        let h_data = pattern(&h_subs, 150);
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
                    Width: 2,
                    Height: 4,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    Format: RGBA8,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    ..Default::default()
                },
                D3D12_BARRIER_LAYOUT_COPY_DEST,
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
                NumDescriptors: 2,
                ..Default::default()
            })
            .unwrap();
        let stride = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
        let rtv_b = rtv_heap.GetCPUDescriptorHandleForHeapStart();
        let rtv_f = D3D12_CPU_DESCRIPTOR_HANDLE {
            ptr: rtv_b.ptr + stride,
        };
        device.CreateRenderTargetView(&b, None, rtv_b);
        device.CreateRenderTargetView(&f, None, rtv_f);
        let dsv_heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: 1,
                ..Default::default()
            })
            .unwrap();
        let dsv = dsv_heap.GetCPUDescriptorHandleForHeapStart();
        device.CreateDepthStencilView(&c, None, dsv);
        let e_upload = committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &buffer_desc(320),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut p = core::ptr::null_mut();
        e_upload.Map(0, None, Some(&mut p)).unwrap();
        p.cast::<u8>().write_bytes(0x5E, 320);
        e_upload.Unmap(0, None);

        // Before the frame: fill and leave everything in its state.
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        let list7: ID3D12GraphicsCommandList7 = list.cast().unwrap();
        let mut keep = Vec::new();
        fill(device, &list, &a, &a_data, &mut keep);
        transition(
            &list,
            &a,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        list.ClearRenderTargetView(rtv_b, &[1.0, 0.0, 0.0, 1.0], None);
        list.ClearDepthStencilView(
            dsv,
            D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
            0.5,
            0x5A,
            None,
        );
        fill(device, &list, &d, &d_data, &mut keep);
        list.CopyBufferRegion(&e, 0, &e_upload, 0, 320);
        fill(device, &list, &g, &g_data, &mut keep);
        transition(
            &list,
            &g,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        );
        fill(device, &list, &h, &h_data, &mut keep);
        layout_barrier(
            &list7,
            &h,
            [D3D12_BARRIER_SYNC_COPY, D3D12_BARRIER_SYNC_ALL_SHADING],
            [
                D3D12_BARRIER_ACCESS_COPY_DEST,
                D3D12_BARRIER_ACCESS_SHADER_RESOURCE,
            ],
            [
                D3D12_BARRIER_LAYOUT_COPY_DEST,
                D3D12_BARRIER_LAYOUT_SHADER_RESOURCE,
            ],
        );
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 14);

        // The frame: each resource's first use, from the state it was left in.
        let readback = committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &buffer_desc(64 << 10),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        allocator.Reset().unwrap();
        list.Reset(&allocator, None).unwrap();
        transition(
            &list,
            &a,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        let a_read = read(device, &list, &a, &readback, 0);
        transition(
            &list,
            &a,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        list.ClearRenderTargetView(rtv_b, &[0.0, 1.0, 0.0, 1.0], None);
        list.ClearDepthStencilView(
            dsv,
            D3D12_CLEAR_FLAG_DEPTH | D3D12_CLEAR_FLAG_STENCIL,
            1.0,
            0x11,
            None,
        );
        transition(
            &list,
            &d,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        let d_read = read(device, &list, &d, &readback, 4096);
        list.CopyBufferRegion(&readback, 8192, &e, 0, 320);
        list.ClearRenderTargetView(rtv_f, &[0.0, 0.0, 1.0, 1.0], None);
        transition(
            &list,
            &g,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        let g_read = read(device, &list, &g, &readback, 12288);
        layout_barrier(
            &list7,
            &h,
            [D3D12_BARRIER_SYNC_ALL_SHADING, D3D12_BARRIER_SYNC_COPY],
            [
                D3D12_BARRIER_ACCESS_SHADER_RESOURCE,
                D3D12_BARRIER_ACCESS_COPY_SOURCE,
            ],
            [
                D3D12_BARRIER_LAYOUT_SHADER_RESOURCE,
                D3D12_BARRIER_LAYOUT_COPY_SOURCE,
            ],
        );
        let h_read = read(device, &list, &h, &readback, 16384);
        list.Close().unwrap();
        // A second submission of the frame that uses A again: its contents are copied once, before the first.
        let allocator2: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let again: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator2, None)
            .unwrap();
        transition(
            &again,
            &a,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        read(device, &again, &a, &readback, 20480);
        transition(
            &again,
            &a,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE,
        );
        transition(
            &again,
            &b,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        let b_read = read(device, &again, &b, &readback, 24576);
        transition(
            &again,
            &b,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        );
        again.Close().unwrap();

        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("contents-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        queue.ExecuteCommandLists(&[Some(again.cast().unwrap())]);
        finish(queue, fence, 15);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        // The application's own reads saw its data: locum's copies left every state as it found it.
        let mut p = core::ptr::null_mut();
        readback.Map(0, None, Some(&mut p)).unwrap();
        let at = |f: D3D12_PLACED_SUBRESOURCE_FOOTPRINT, row: usize, len: usize| {
            core::slice::from_raw_parts(
                p.cast::<u8>()
                    .add(f.Offset as usize + row * f.Footprint.RowPitch as usize),
                len,
            )
        };
        assert_eq!(at(a_read, 1, 16), &a_data[0][16..32]);
        assert_eq!(at(d_read, 1, 16), &d_data[0][16..32]);
        assert!(
            core::slice::from_raw_parts(p.cast::<u8>().add(8192), 320)
                .iter()
                .all(|&v| v == 0x5E)
        );
        assert_eq!(at(g_read, 1, 16), &g_data[0][16..32]);
        assert_eq!(at(h_read, 3, 8), &h_data[0][24..32]);
        assert_eq!(at(b_read, 0, 4), &[0, 255, 0, 255], "the frame's own clear");
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
        let resource = |filter: &str| -> (i64, Option<String>, Option<String>, Option<String>) {
            db.query_row(
                &format!("SELECT id, content, content_source, content_note FROM resources WHERE {filter}"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap_or_else(|e| panic!("{filter}: {e}"))
        };
        let contents = |filter: &str| -> Vec<u8> {
            let (id, content, source, note) = resource(filter);
            assert_eq!(
                source.as_deref(),
                Some("first-use"),
                "{filter}: resource {id}: {note:?}"
            );
            blobs.get(&content.unwrap()).unwrap()
        };
        let a_bytes = contents("width=4 AND height=4 AND mips=3 AND array=2");
        assert_eq!(a_bytes, a_data.concat(), "A: every mip of every slice, as filled");
        let (a_id, ..) = resource("width=4 AND height=4 AND mips=3 AND array=2");
        let mut statement = db
            .prepare("SELECT sub, hash FROM subresources WHERE resource=?1 ORDER BY sub")
            .unwrap();
        let hashes: Vec<(u32, String)> = statement
            .query_map([a_id], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            hashes,
            a_data
                .iter()
                .enumerate()
                .map(|(i, d)| (i as u32, locum_capture::hash::key(d)))
                .collect::<Vec<_>>()
        );
        let b_bytes = contents("width=12 AND height=8");
        assert!(
            b_bytes.chunks(4).all(|t| t == [255, 0, 0, 255]),
            "B: the colour before the frame's clear"
        );
        let c_bytes = contents("format=45");
        assert_eq!(c_bytes.len(), 8 * 8 * 4);
        for t in c_bytes.chunks(4) {
            let depth = u32::from_le_bytes([t[0], t[1], t[2], 0]);
            assert!(
                matches!(depth, 0x7F_FFFF | 0x80_0000) && t[3] == 0x5A,
                "C: depth 0.5 and stencil 0x5A: {t:?}"
            );
        }
        assert_eq!(
            contents("width=4 AND height=2"),
            d_data.concat(),
            "D: promoted to COPY_DEST by its fill"
        );
        assert!(
            contents("kind='buffer' AND width=320 AND content_note NOT LIKE 'upload%'")
                .iter()
                .all(|&v| v == 0x5E)
        );
        assert_eq!(contents("format=71"), g_data.concat(), "G: both mips' blocks");
        assert_eq!(
            contents("width=2 AND height=4"),
            h_data.concat(),
            "H: copied from its layout"
        );
        let (_, content, _, note) = resource("samples=4");
        assert!(
            content.is_none() && note.as_deref().is_some_and(|n| n.starts_with("multisampled")),
            "{note:?}"
        );
        for (sql, n) in [
            (
                "SELECT count(*) FROM gaps WHERE what='contents' AND detail LIKE 'multisampled%'",
                1,
            ),
            (
                "SELECT count(*) FROM resources WHERE content_source='first-use'",
                7,
            ),
            // Each resource once, at the first submission of the frame that used it.
            ("SELECT count(DISTINCT submission) FROM d12_first_use", 1),
            (
                "SELECT count(*) FROM d12_first_use f JOIN resources r ON r.id=f.resource WHERE r.mips=3 \
                 AND f.states_json=json_array(json('{\"state\":128}'),json('{\"state\":128}'),json('{\"state\":128}'),\
                 json('{\"state\":128}'),json('{\"state\":128}'),json('{\"state\":128}'))",
                1,
            ),
            (
                "SELECT count(*) FROM d12_first_use f JOIN resources r ON r.id=f.resource WHERE r.width=4 \
                 AND r.height=2 AND f.states_json='[{\"state\":1024}]'",
                1,
            ),
            (
                "SELECT count(*) FROM d12_first_use f JOIN resources r ON r.id=f.resource WHERE r.width=2 \
                 AND r.height=4 AND f.states_json='[{\"layout\":6}]'",
                1,
            ),
        ] {
            assert_eq!(db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap(), n, "{sql}");
        }
        drop(keep);
        debug_errors(device);
    }
    println!(
        "D3D12 first-use contents verified: mips and slices, render target before its clear, depth-stencil planes, promoted COPY_DEST, buffer, BC1, enhanced layout, MSAA gap"
    );
}
