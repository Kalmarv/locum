//! A buffer first used behind a wait that another queue signals only later: its copy runs past the wait, just before
//! the lists, and the capture says so in its note.
use super::*;
use contents::{buffer_desc, committed};

/// A width no other smoke buffer has, to find it in the capture by.
const BYTES: u64 = (64 << 10) + 4096;

fn byte(i: u64) -> u8 {
    (i * 7 + (i >> 9)) as u8
}

pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    n: u64,
) {
    unsafe {
        let buffer = committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &buffer_desc(BYTES),
            D3D12_RESOURCE_STATE_COMMON,
        );
        let data = committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &buffer_desc(BYTES),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut p = core::ptr::null_mut();
        data.Map(0, None, Some(&mut p)).unwrap();
        let s = core::slice::from_raw_parts_mut(p.cast::<u8>(), BYTES as usize);
        for (i, v) in s.iter_mut().enumerate() {
            *v = byte(i as u64);
        }
        data.Unmap(0, None);
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        list.CopyBufferRegion(&buffer, 0, &data, 0, BYTES);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, n);

        // The frame: the queue waits for `gate`, which a second queue signals after the queue's submission.
        let other: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .unwrap();
        let gate: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let readback = committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &buffer_desc(256),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let frame_allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let frame: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &frame_allocator, None)
            .unwrap();
        frame.CopyBufferRegion(&readback, 0, &buffer, BYTES - 256, 256);
        frame.Close().unwrap();
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let label = format!("behind-wait-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        queue.Wait(&gate, 1).unwrap();
        queue.ExecuteCommandLists(&[Some(frame.cast().unwrap())]);
        other.Signal(&gate, 1).unwrap();
        finish(queue, fence, n + 1);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        let mut p = core::ptr::null_mut();
        readback.Map(0, None, Some(&mut p)).unwrap();
        let read = core::slice::from_raw_parts(p.cast::<u8>(), 256).to_vec();
        readback.Unmap(0, None);
        assert!(
            (0..256).all(|i| read[i as usize] == byte(BYTES - 256 + i)),
            "the frame's own read"
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
        let (content, source, note): (Option<String>, Option<String>, Option<String>) = db
            .query_row(
                "SELECT content, content_source, content_note FROM resources WHERE kind='buffer' AND width=?1",
                [BYTES as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(source.as_deref(), Some("first-use"), "{note:?}");
        let note = note.expect("a note");
        assert!(
            note.contains("waits for fence") && note.contains("copied when the queue got past the wait"),
            "{note}"
        );
        let blobs = locum_capture::blobs::Blobs::new(dir.join("blobs"));
        let bytes = blobs.get(&content.unwrap()).unwrap();
        assert!(
            bytes.len() as u64 == BYTES && bytes.iter().enumerate().all(|(i, &v)| v == byte(i as u64)),
            "the buffer as filled"
        );
        finish(queue, fence, n + 2);
        debug_errors(device);
    }
    println!(
        "D3D12 behind a wait: a buffer first used behind a wait signalled later copied past it, with a note"
    );
}
