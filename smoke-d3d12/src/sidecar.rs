//! A sidecar beside the capture: a region of the application's memory copied, a visible range over one of the frame's
//! buffers, and two descriptors in it, one as the buffer holds it and one resolving to nothing.
use super::*;
use contents::{buffer_desc, committed};

/// A width no other smoke buffer has, to find it in the capture by.
const BYTES: u64 = (64 << 10) + 8192;
/// Where the sidecar says the application sees the buffer.
const SEEN: u64 = 0x1_0000_0000;

fn byte(i: u64) -> u8 {
    (i * 13 + (i >> 7)) as u8
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
        frame.CopyBufferRegion(&readback, 0, &buffer, 0, 256);
        frame.Close().unwrap();

        // Application memory for the region: two pages, the second all zeros.
        let memory: Vec<u8> = (0..128u32 << 10)
            .map(|i| if i < 64 << 10 { (i % 251) as u8 + 1 } else { 0 })
            .collect();
        let words =
            |from: u64, n: u64| -> String { (from..from + n).map(|i| format!("{:02x}", byte(i))).collect() };
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
        let sidecar = dir.join("sidecar.txt");
        std::fs::write(
            &sidecar,
            format!(
                "# the smoke's\nregion {:#x} {:x} smoke memory\nvisible {SEEN:#x} {BYTES:x} {:#x}\n\
                 descriptor {:#x} {} {:#x} 40 as the buffer holds it\ndescriptor {:#x} 00000000 0 0 resolving to \
                 nothing\n",
                memory.as_ptr() as u64,
                memory.len(),
                buffer.GetGPUVirtualAddress(),
                SEEN + 16,
                words(16, 16),
                SEEN + 256,
                SEEN + 32,
            ),
        )
        .unwrap();
        let label = format!("sidecar-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        queue.ExecuteCommandLists(&[Some(frame.cast().unwrap())]);
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
        // Later captures have none.
        std::fs::remove_file(&sidecar).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        let blobs = locum_capture::blobs::Blobs::new(dir.join("blobs"));
        let (name, content): (String, String) = db
            .query_row("SELECT name, content FROM sidecar_regions", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(name, "smoke memory");
        assert!(
            blobs.get(&content).unwrap() == memory,
            "the region as the application held it"
        );
        let pieces = blobs.pieces(&content).unwrap().expect("a pieced blob");
        assert_eq!(
            pieces
                .iter()
                .map(|(k, n)| (k == locum_capture::blobs::ZEROS, *n))
                .collect::<Vec<_>>(),
            [(false, 64 << 10), (true, 64 << 10)],
            "its page of zeros stored nowhere"
        );
        drop(memory);
        // The descriptors, checked as the CLI does: through the visible range to the buffer's bytes in the capture.
        let visible: Vec<locum_capture::sidecar::Visible> = db
            .prepare("SELECT address, bytes, gpu FROM sidecar_visible")
            .unwrap()
            .query_map([], |r| {
                Ok(locum_capture::sidecar::Visible {
                    address: r.get::<_, i64>(0)? as u64,
                    bytes: r.get::<_, i64>(1)? as u64,
                    gpu: r.get::<_, i64>(2)? as u64,
                })
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let (resource, key): (i64, String) = db
            .query_row(
                "SELECT id, content FROM resources WHERE kind='buffer' AND width=?1",
                [BYTES as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let buffers = [locum_capture::sidecar::Buffer {
            resource: resource as u64,
            gpu: buffer.GetGPUVirtualAddress(),
            bytes: BYTES,
            content: Some(key),
        }];
        let descriptors: Vec<locum_capture::sidecar::Descriptor> = db
            .prepare("SELECT address, words, target, bytes, label FROM sidecar_descriptors ORDER BY address")
            .unwrap()
            .query_map([], |r| {
                Ok(locum_capture::sidecar::Descriptor {
                    address: r.get::<_, i64>(0)? as u64,
                    words: r.get(1)?,
                    target: r.get::<_, i64>(2)? as u64,
                    bytes: r.get::<_, i64>(3)? as u64,
                    label: r.get(4)?,
                })
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let mut read = |k: &str, offset: u64, n: u64| blobs.read(k, offset, Some(n)).ok();
        let checked: Vec<_> = descriptors
            .iter()
            .map(|d| locum_capture::sidecar::check(d, &visible, &buffers, &mut read))
            .collect();
        assert_eq!(
            checked
                .iter()
                .map(|c| (c.resource, c.offset, c.found, c.flag))
                .collect::<Vec<_>>(),
            [
                (Some(resource as u64), Some(16), "matched", None),
                (Some(resource as u64), Some(32), "differs", Some("empty")),
            ]
        );
        finish(queue, fence, n + 2);
        debug_errors(device);
    }
    println!(
        "D3D12 sidecar: a region copied as pieces, its descriptors found in the frame's buffer and checked against it"
    );
}
