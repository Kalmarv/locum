//! GPU copies of the bytes an armed frame reads, made on the submitting queue just before the application's lists,
//! into readback memory the CPU reads once the frame is over.
//!
//! The CPU cannot read upload heaps quickly: they are write-combined, and with resizable BAR they can be video memory
//! across PCIe. Elden Ring's armed frame read 742 MiB of them at 6.6 MB/s, 112 s. A copy on the queue reads the bytes
//! when the GPU reaches the application's work, which is when that work reads them, and upload resources stay in
//! GENERIC_READ for their whole life, so the copies need no barriers.
use core::ffi::c_void;
use serde_json::{Value, json};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::Interface;

/// One range of a buffer to copy: `whole` marks the copy of a whole upload buffer at the frame's first submission.
pub struct Copy {
    pub resource: u64,
    pub offset: u64,
    pub len: u64,
    pub whole: bool,
}

/// Copies queued on the GPU: where each landed in the readback buffer, and the fence value that says they are done.
pub struct Pending {
    copies: Vec<(Copy, u64)>,
    readback: ID3D12Resource,
    fence: ID3D12Fence,
    // The list and allocator live until the copies are done.
    _list: ID3D12GraphicsCommandList,
    _allocator: ID3D12CommandAllocator,
}

/// Offsets in the readback buffer are aligned like placed footprints, which keeps each copy's start aligned.
const ALIGN: u64 = D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT as u64;

/// Records `copies` on a list of the queue's type and executes it on the real queue `queue`, then signals a fence of
/// our own. Errors say which step failed; every copy of a failed batch then has a diagnostic instead of bytes.
///
/// # Safety
/// `queue` is the application's real queue, called from its ExecuteCommandLists before the application's lists.
pub unsafe fn submit(queue: *mut c_void, copies: Vec<Copy>) -> Result<Pending, (Vec<Copy>, String)> {
    if copies.is_empty() {
        return Err((copies, "nothing to copy".into()));
    }
    let Some(queue) = (unsafe { ID3D12CommandQueue::from_raw_borrowed(&queue) }) else {
        return Err((copies, "no queue".into()));
    };
    let fail =
        |copies, step: &str, e: windows::core::Error| Err((copies, format!("upload readback {step}: {e}")));
    let mut device: Option<ID3D12Device> = None;
    if let Err(e) = unsafe { queue.GetDevice(&mut device) } {
        return fail(copies, "GetDevice", e);
    }
    let device = device.expect("GetDevice succeeded");
    let kind = unsafe { queue.GetDesc() }.Type;
    let mut placed = Vec::with_capacity(copies.len());
    let mut total = 0u64;
    for c in copies {
        placed.push((c, total));
        total = (total + placed.last().unwrap().0.len).next_multiple_of(ALIGN);
    }
    let unplace = |placed: Vec<(Copy, u64)>| placed.into_iter().map(|(c, _)| c).collect::<Vec<_>>();
    let mut readback: Option<ID3D12Resource> = None;
    if let Err(e) = unsafe {
        device.CreateCommittedResource(
            &D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_READBACK,
                ..Default::default()
            },
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: total.max(1),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_COPY_DEST,
            None,
            &mut readback,
        )
    } {
        return fail(unplace(placed), &format!("readback buffer of {total} bytes"), e);
    }
    let readback = readback.expect("created");
    let allocator: ID3D12CommandAllocator = match unsafe { device.CreateCommandAllocator(kind) } {
        Ok(a) => a,
        Err(e) => return fail(unplace(placed), "CreateCommandAllocator", e),
    };
    let list: ID3D12GraphicsCommandList = match unsafe { device.CreateCommandList(0, kind, &allocator, None) }
    {
        Ok(l) => l,
        Err(e) => return fail(unplace(placed), "CreateCommandList", e),
    };
    for (c, at) in &placed {
        // The application's resource: its real object, so our list never sees a wrapper.
        if let Some(source) = super::real(c.resource).and_then(|r| r.cast::<ID3D12Resource>().ok()) {
            unsafe { list.CopyBufferRegion(&readback, *at, &source, c.offset, c.len) };
        }
    }
    if let Err(e) = unsafe { list.Close() } {
        return fail(unplace(placed), "Close", e);
    }
    let fence: ID3D12Fence = match unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) } {
        Ok(f) => f,
        Err(e) => return fail(unplace(placed), "CreateFence", e),
    };
    unsafe { queue.ExecuteCommandLists(&[Some(list.cast().expect("a graphics list is a command list"))]) };
    if let Err(e) = unsafe { queue.Signal(&fence, 1) } {
        return fail(unplace(placed), "Signal", e);
    }
    Ok(Pending {
        copies: placed,
        readback,
        fence,
        _list: list,
        _allocator: allocator,
    })
}

/// Waits for the copies, then stores each range in the blob store: (resource, content) as the capture records
/// upload versions.
pub fn collect(pending: Pending) -> Vec<(u64, Value)> {
    let Pending {
        copies,
        readback,
        fence,
        ..
    } = pending;
    // A null event makes SetEventOnCompletion wait until the fence reaches the value.
    if let Err(e) = unsafe { fence.SetEventOnCompletion(1, HANDLE::default()) } {
        return failed(
            copies.into_iter().map(|(c, _)| c).collect(),
            &format!("upload readback wait: {e}"),
        );
    }
    let mut p = core::ptr::null_mut();
    if let Err(e) = unsafe { readback.Map(0, None, Some(&mut p)) } {
        return failed(
            copies.into_iter().map(|(c, _)| c).collect(),
            &format!("upload readback Map: {e}"),
        );
    }
    let out = copies
        .into_iter()
        .map(|(c, at)| {
            let start = std::time::Instant::now();
            let mut content =
                unsafe { super::pipelines::bytes(p.cast::<u8>().add(at as usize).cast(), c.len as usize) };
            super::journal::UPLOAD_COST.add(start, c.len);
            content["offset"] = json!(c.offset);
            if c.whole {
                content["whole"] = json!(true);
            }
            (c.resource, content)
        })
        .collect();
    unsafe { readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 })) };
    out
}

/// Every copy of a batch that failed, with the reason in place of its bytes.
pub fn failed(copies: Vec<Copy>, reason: &str) -> Vec<(u64, Value)> {
    copies
        .into_iter()
        .map(|c| {
            let mut content = json!({"diagnostic":reason,"offset":c.offset,"size":c.len});
            if c.whole {
                content["whole"] = json!(true);
            }
            (c.resource, content)
        })
        .collect()
}
