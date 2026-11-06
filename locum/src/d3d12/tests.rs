//! A deliberately base-only fence checks that wrapping does not require Fence1,
//! that every facet has one IUnknown identity, and that the last Release destroys
//! the real object rather than leaving a registry-owned reference behind.
use super::*;
use crate::vtables::d12_fence::Fence12Methods;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32},
};
use windows::Win32::{
    Foundation::{E_NOINTERFACE, S_OK},
    Graphics::Direct3D12::*,
};
#[repr(C)]
struct BaseFence {
    table: *const ID3D12Fence1_Vtbl,
    refs: AtomicU32,
    destroyed: Arc<AtomicBool>,
}
static TABLE: ID3D12Fence1_Vtbl = crate::vtables::d12_fence::vtable::<BaseFence>();
impl Drop for BaseFence {
    fn drop(&mut self) {
        self.destroyed.store(true, Ordering::SeqCst);
    }
}
unsafe impl Fence12Methods for BaseFence {
    fn inner(&self) -> *mut c_void {
        panic!("mock operation outside this test's contract")
    }
    unsafe fn QueryInterface(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        if out.is_null() || iid.is_null() {
            return E_POINTER;
        }
        unsafe {
            *out = core::ptr::null_mut();
        }
        if unsafe { *iid } == IUnknown::IID || unsafe { *iid } == ID3D12Fence::IID {
            unsafe {
                self.AddRef();
                *out = (self as *const Self).cast_mut().cast();
            }
            S_OK
        } else {
            E_NOINTERFACE
        }
    }
    unsafe fn AddRef(&self) -> u32 {
        self.refs.fetch_add(1, Ordering::SeqCst) + 1
    }
    unsafe fn Release(&self) -> u32 {
        let n = self.refs.fetch_sub(1, Ordering::SeqCst) - 1;
        if n == 0 {
            drop(unsafe { Box::from_raw((self as *const Self).cast_mut()) });
        }
        n
    }
    unsafe fn GetCompletedValue(&self) -> u64 {
        7
    }
}
#[test]
fn base_interface_identity_and_final_release() {
    unsafe {
        let destroyed = Arc::new(AtomicBool::new(false));
        let mut out = Box::into_raw(Box::new(BaseFence {
            table: &TABLE,
            refs: AtomicU32::new(1),
            destroyed: destroyed.clone(),
        }))
        .cast::<c_void>();
        let key = out as usize;
        wrap_output(S_OK, &ID3D12Fence::IID, &mut out);
        assert!(is_wrapped(out));
        let wrapped_id = id(out);
        assert_ne!(wrapped_id, 0);
        assert_eq!(id(out), wrapped_id);
        let fence = ID3D12Fence::from_raw(out);
        assert_eq!(fence.GetCompletedValue(), 7);
        assert!(fence.cast::<ID3D12Fence1>().is_err());
        let a = fence.cast::<IUnknown>().unwrap();
        let b = fence.cast::<IUnknown>().unwrap();
        assert_eq!(a.as_raw(), b.as_raw());
        drop(fence);
        drop(a);
        assert!(!destroyed.load(Ordering::SeqCst));
        drop(b);
        assert!(destroyed.load(Ordering::SeqCst));
        assert!(!with(|r| r.identities.contains_key(&key)));
        assert_eq!(id(out), 0, "last Release invalidates the cached pointer ID");
    }
}

type CreateDevice = unsafe extern "system" fn(
    *mut c_void,
    windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL,
    *const GUID,
    *mut *mut c_void,
) -> HRESULT;

/// The count `AddRef` gives back, the reference it took given back.
unsafe fn count(object: *mut c_void) -> u32 {
    let n = unsafe { crate::faces::add_ref(object) };
    unsafe { crate::faces::release(object) };
    n
}

/// What a game sees of counts and lifetimes on a WARP device `create` makes: each count a child's creation and
/// release leave on the device, whether a child's `GetDevice` answers with the device the game holds, and what a heap
/// a placed resource lives in says after the game's last release of it, and after that.
unsafe fn lifetimes(create: CreateDevice) -> Vec<u32> {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
    use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIFactory4};
    type Factory = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> HRESULT;
    let mut seen = Vec::new();
    unsafe {
        let factory_fn =
            crate::system::export::<Factory>(crate::system::system().dxgi, c"CreateDXGIFactory2").unwrap();
        let mut raw = core::ptr::null_mut();
        factory_fn(0, &IDXGIFactory4::IID, &mut raw).ok().unwrap();
        let factory = IDXGIFactory4::from_raw(raw);
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter().unwrap();
        create(
            adapter.as_raw(),
            D3D_FEATURE_LEVEL_11_0,
            &ID3D12Device::IID,
            &mut raw,
        )
        .ok()
        .unwrap();
        let device = ID3D12Device::from_raw(raw);
        seen.push(count(device.as_raw()));
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        seen.push(count(device.as_raw()));
        let mut parent: Option<ID3D12Device> = None;
        allocator.GetDevice(&mut parent).unwrap();
        seen.push((parent.as_ref().unwrap().as_raw() == device.as_raw()) as u32);
        seen.push(count(device.as_raw()));
        drop(parent);
        let mut heap: Option<ID3D12Heap> = None;
        device
            .CreateHeap(
                &D3D12_HEAP_DESC {
                    SizeInBytes: 65536,
                    Properties: D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
                    ..Default::default()
                },
                &mut heap,
            )
            .unwrap();
        let heap = heap.unwrap();
        let mut resource: Option<ID3D12Resource> = None;
        device
            .CreatePlacedResource(
                &heap,
                0,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: 65536,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut resource,
            )
            .unwrap();
        let resource = resource.unwrap();
        seen.push(count(device.as_raw()));
        // The game's last reference to the heap goes; the resource placed in it keeps it, and it stays usable.
        let heap = heap.into_raw();
        seen.push(crate::faces::release(heap));
        seen.push(crate::faces::add_ref(heap));
        let desc = ID3D12Heap::from_raw_borrowed(&heap).unwrap().GetDesc();
        seen.push((desc.SizeInBytes == 65536) as u32);
        seen.push(crate::faces::release(heap));
        seen.push((resource.GetGPUVirtualAddress() != 0) as u32);
        drop(resource);
        seen.push(count(device.as_raw()));
        drop(allocator);
        seen.push(count(device.as_raw()));
        seen.push(crate::faces::release(device.into_raw()));
    }
    seen
}

fn system_create() -> CreateDevice {
    unsafe { crate::system::export::<CreateDevice>(crate::system::system_d3d12(), c"D3D12CreateDevice") }
        .unwrap()
}

/// What locum's `D3D12CreateDevice` does: the system's, its device wrapped. (The export itself, linked into the test
/// executable, would bring the stand-ins' export names with it.)
unsafe extern "system" fn locum_create(
    adapter: *mut c_void,
    level: windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    let hr = unsafe { system_create()(adapter, level, iid, out) };
    unsafe { wrap_output(hr, iid, out) };
    hr
}

/// Through locum, the counts and lifetimes a game sees are the runtime's own.
#[test]
fn counts_and_lifetimes_are_the_runtimes() {
    let native = unsafe { lifetimes(system_create()) };
    let ours = unsafe { lifetimes(locum_create) };
    assert_eq!(ours, native, "through locum, against the runtime alone");
    crate::faces::bury();
}

/// A swap chain on a removed device says why it was removed, for the log. In a process of its own: a process has one
/// device per adapter, and other tests use WARP's.
#[test]
fn a_swap_chain_tells_why_its_device_was_removed() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "d3d12::tests::removal_probe",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[test]
#[ignore = "WARP subprocess fixture that removes the device, run by a_swap_chain_tells_why_its_device_was_removed"]
fn removal_probe() {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::*;
    unsafe {
        let mut raw = core::ptr::null_mut();
        crate::CreateDXGIFactory2(0, &IDXGIFactory4::IID, &mut raw)
            .ok()
            .unwrap();
        let factory = IDXGIFactory4::from_raw(raw);
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter().unwrap();
        locum_create(
            adapter.as_raw(),
            D3D_FEATURE_LEVEL_11_0,
            &ID3D12Device::IID,
            &mut raw,
        )
        .ok()
        .unwrap();
        let device = ID3D12Device::from_raw(raw);
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .unwrap();
        let chain = factory
            .CreateSwapChainForComposition(
                &queue,
                &DXGI_SWAP_CHAIN_DESC1 {
                    Width: 64,
                    Height: 64,
                    Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                    BufferCount: 2,
                    SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                    ..Default::default()
                },
                None,
            )
            .unwrap();
        let ours = &*(chain.as_raw() as *const crate::dxgi::SwapChain);
        assert_eq!(ours.removal(), Some(S_OK), "a device not removed");
        device.cast::<ID3D12Device5>().unwrap().RemoveDevice();
        assert_eq!(chain.Present(0, DXGI_PRESENT(0)), DXGI_ERROR_DEVICE_REMOVED);
        assert_eq!(
            ours.removal(),
            Some(DXGI_ERROR_DEVICE_REMOVED),
            "removed by RemoveDevice"
        );
        drop((chain, queue, device, adapter, factory));
    }
    crate::faces::bury();
}

/// A direct list made while a capture is armed records its passes. In a process of its own: arming the capture changes
/// what every other test's lists do.
#[test]
fn a_list_made_while_armed_is_instrumented() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "d3d12::tests::armed_list_probe",
            "--ignored",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}

#[test]
#[ignore = "WARP subprocess fixture that arms a capture, run by a_list_made_while_armed_is_instrumented"]
fn armed_list_probe() {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Dxgi::*;
    unsafe {
        let mut raw = core::ptr::null_mut();
        crate::CreateDXGIFactory2(0, &IDXGIFactory4::IID, &mut raw)
            .ok()
            .unwrap();
        let factory = IDXGIFactory4::from_raw(raw);
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter().unwrap();
        locum_create(
            adapter.as_raw(),
            D3D_FEATURE_LEVEL_11_0,
            &ID3D12Device::IID,
            &mut raw,
        )
        .ok()
        .unwrap();
        let device = ID3D12Device::from_raw(raw);
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        passes::ARMED.store(true, Ordering::Release);
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        passes::ARMED.store(false, Ordering::Release);
        let data = data(list.as_raw()).expect("a wrapped list");
        assert!(
            data.pass.lock().unwrap().is_some(),
            "the new list records its passes"
        );
        drop((list, allocator, device, adapter, factory));
    }
}
