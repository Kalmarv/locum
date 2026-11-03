//! Exercises real WARP12 calls through the shims. This is not a game launcher.
use core::{ffi::c_void, mem::ManuallyDrop};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::{Direct3D::*, Direct3D12::*, Dxgi::Common::*, Dxgi::*};
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;

fn identity<T: Interface, U: Interface>(a: &T, b: &U) {
    assert_eq!(
        a.cast::<IUnknown>().unwrap().as_raw(),
        b.cast::<IUnknown>().unwrap().as_raw()
    );
}
unsafe fn finish(queue: &ID3D12CommandQueue, fence: &ID3D12Fence, n: u64) {
    unsafe { queue.Signal(fence, n) }.unwrap();
    let until = Instant::now() + Duration::from_secs(15);
    while unsafe { fence.GetCompletedValue() } < n {
        assert!(Instant::now() < until, "GPU fence timed out");
        std::thread::yield_now();
    }
}
unsafe fn buffer(
    device: &ID3D12Device,
    heap: D3D12_HEAP_TYPE,
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
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: 256,
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
unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}
fn main() {
    unsafe {
        run();
    }
    println!("D3D12 WARP smoke: all checks passed");
}
unsafe fn run() {
    unsafe {
        // Loading the DXGI shim first must not hide the local D3D12 shim.
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let warp: IDXGIAdapter = factory.EnumWarpAdapter().unwrap();
        let dll = LoadLibraryW(w!("d3d12.dll")).unwrap();
        let named = GetProcAddress(dll, s!("D3D12CreateDevice")).unwrap();
        let ordinal = GetProcAddress(dll, PCSTR(101usize as *const u8)).unwrap();
        assert_eq!(named as usize, ordinal as usize);
        let debug_name = GetProcAddress(dll, s!("D3D12GetDebugInterface")).unwrap();
        assert_eq!(
            debug_name as usize,
            GetProcAddress(dll, PCSTR(102usize as *const u8)).unwrap() as usize
        );
        type Debug = unsafe extern "system" fn(*const GUID, *mut *mut c_void) -> HRESULT;
        let debug: Debug = core::mem::transmute(debug_name);
        let mut raw = core::ptr::null_mut();
        let debug_enabled = debug(&ID3D12Debug::IID, &mut raw).is_ok();
        if debug_enabled {
            let d = ID3D12Debug::from_raw(raw);
            d.EnableDebugLayer();
        }
        println!("debug layer enabled: {debug_enabled}");
        type Create = unsafe extern "system" fn(
            *mut c_void,
            D3D_FEATURE_LEVEL,
            *const GUID,
            *mut *mut c_void,
        ) -> HRESULT;
        let create: Create = core::mem::transmute(ordinal);
        assert!(
            create(
                warp.as_raw(),
                D3D_FEATURE_LEVEL_11_0,
                &ID3D12Device::IID,
                core::ptr::null_mut()
            )
            .is_ok()
        );
        raw = core::ptr::null_mut();
        create(
            warp.as_raw(),
            D3D_FEATURE_LEVEL_11_0,
            &ID3D12Device::IID,
            &mut raw,
        )
        .ok()
        .unwrap();
        let device = ID3D12Device::from_raw(raw);
        let device5 = device.cast::<ID3D12Device5>().unwrap();
        identity(&device, &device5);
        let bad = GUID::from_u128(0xdeadbeef_1111_2222_3333_444444444444);
        let mut none = usize::MAX as *mut c_void;
        assert_eq!(
            (device.vtable().base__.base__.QueryInterface)(device.as_raw(), &bad, &mut none),
            E_NOINTERFACE
        );
        assert!(none.is_null());
        let queue: ID3D12CommandQueue = device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .unwrap();
        let mut parent: Option<ID3D12Device> = None;
        queue.GetDevice(&mut parent).unwrap();
        identity(&device, parent.as_ref().unwrap());
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        identity(&list, &list.cast::<ID3D12GraphicsCommandList4>().unwrap());
        let input = buffer(&device, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
        let output = buffer(&device, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
        let mut mapped = core::ptr::null_mut();
        input.Map(0, None, Some(&mut mapped)).unwrap();
        for i in 0..64 {
            mapped.cast::<u32>().add(i).write(0x12340000 + i as u32);
        }
        input.Unmap(0, None);
        list.CopyBufferRegion(&output, 0, &input, 0, 256);
        list.Close().unwrap();
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        for n in 1..=2 {
            queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
            finish(&queue, &fence, n);
        }
        mapped = core::ptr::null_mut();
        output.Map(0, None, Some(&mut mapped)).unwrap();
        for i in 0..64 {
            assert_eq!(mapped.cast::<u32>().add(i).read(), 0x12340000 + i as u32);
        }
        output.Unmap(0, None);
        allocator.Reset().unwrap();
        list.Reset(&allocator, None).unwrap();
        list.Close().unwrap();
        println!(
            "identity, optional QI, null output, allocator reset, repeated list and copy bytes verified"
        );
        // Registry must preserve interface-valued private data without leaking a raw IUnknown.
        let key = GUID::from_u128(0x12345678_1111_2222_3333_444444444444);
        input.SetPrivateDataInterface(&key, &output).unwrap();
        let mut bytes = size_of::<usize>() as u32;
        raw = core::ptr::null_mut();
        input
            .GetPrivateData(&key, &mut bytes, Some((&mut raw as *mut *mut c_void).cast()))
            .unwrap();
        let private = IUnknown::from_raw(raw);
        identity(&private, &output);
        input.SetPrivateDataInterface(&key, None).unwrap();
        // Serializer is an ABI-transparent tail call, including its output parameters.
        type Serialize = unsafe extern "system" fn(
            *const D3D12_ROOT_SIGNATURE_DESC,
            D3D_ROOT_SIGNATURE_VERSION,
            *mut *mut c_void,
            *mut *mut c_void,
        ) -> HRESULT;
        let serialize: Serialize =
            core::mem::transmute(GetProcAddress(dll, s!("D3D12SerializeRootSignature")).unwrap());
        let mut blob = core::ptr::null_mut();
        let mut errors = core::ptr::null_mut();
        serialize(
            &D3D12_ROOT_SIGNATURE_DESC::default(),
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut blob,
            &mut errors,
        )
        .ok()
        .unwrap();
        if !errors.is_null() {
            drop(ID3DBlob::from_raw(errors));
        }
        let blob = ID3DBlob::from_raw(blob);
        let root: ID3D12RootSignature = device
            .CreateRootSignature(
                0,
                core::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()),
            )
            .unwrap();
        let source = b"[numthreads(1,1,1)] void main(uint3 id:SV_DispatchThreadID) {}";
        let mut code = None;
        let mut errors = None;
        windows::Win32::Graphics::Direct3D::Fxc::D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            None,
            None,
            None,
            s!("main"),
            s!("cs_5_0"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
        .unwrap();
        let code = code.unwrap();
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.GetBufferPointer(),
                BytecodeLength: code.GetBufferSize(),
            },
            ..Default::default()
        };
        let pso: ID3D12PipelineState = device.CreateComputePipelineState(&desc).unwrap();
        // The same nested root must be unwrapped in stream and library paths too.
        #[repr(C, align(8))]
        struct RootSub {
            kind: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE,
            root: *mut c_void,
        }
        #[repr(C, align(8))]
        struct ShaderSub {
            kind: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE,
            code: D3D12_SHADER_BYTECODE,
        }
        #[repr(C)]
        struct ComputeStream {
            root: RootSub,
            shader: ShaderSub,
        }
        let mut stream = ComputeStream {
            root: RootSub {
                kind: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
                root: root.as_raw(),
            },
            shader: ShaderSub {
                kind: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS,
                code: desc.CS,
            },
        };
        let stream_desc = D3D12_PIPELINE_STATE_STREAM_DESC {
            SizeInBytes: size_of::<ComputeStream>(),
            pPipelineStateSubobjectStream: (&mut stream as *mut ComputeStream).cast(),
        };
        let streamed: ID3D12PipelineState = device5.CreatePipelineState(&stream_desc).unwrap();
        let library: ID3D12PipelineLibrary1 = device5.CreatePipelineLibrary(&[]).unwrap();
        library.StorePipeline(w!("compute"), &pso).unwrap();
        let loaded: ID3D12PipelineState = library.LoadComputePipeline(w!("compute"), &desc).unwrap();
        identity(&loaded, &pso);
        library.StorePipeline(w!("stream"), &streamed).unwrap();
        let loaded_stream: ID3D12PipelineState = library.LoadPipeline(w!("stream"), &stream_desc).unwrap();
        identity(&loaded_stream, &streamed);
        let mut unknown_raw = core::ptr::null_mut();
        create(
            warp.as_raw(),
            D3D_FEATURE_LEVEL_11_0,
            &IUnknown::IID,
            &mut unknown_raw,
        )
        .ok()
        .unwrap();
        let unknown_device = IUnknown::from_raw(unknown_raw);
        identity(&unknown_device, &device);
        println!("PSO streams, pipeline-library loads and creation by IUnknown verified");
        let mut desc = desc;
        ManuallyDrop::drop(&mut desc.pRootSignature);
        list.Reset(&allocator, Some(&pso)).unwrap();
        list.SetComputeRootSignature(&root);
        list.Dispatch(1, 1, 1);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(&queue, &fence, 3);
        println!("root-signature serializer, nested PSO root pointer, dispatch and fence verified");
        let heaps: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                ..Default::default()
            })
            .unwrap();
        device.CreateConstantBufferView(
            Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: input.GetGPUVirtualAddress(),
                SizeInBytes: 256,
            }),
            heaps.GetCPUDescriptorHandleForHeapStart(),
        );
        list.Reset(&allocator, None).unwrap();
        list.SetDescriptorHeaps(&[Some(heaps.clone())]);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(&queue, &fence, 4);
        debug_errors(&device);
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
        let mut placed: Option<ID3D12Resource> = None;
        device
            .CreatePlacedResource(
                &heap,
                0,
                &input.GetDesc(),
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut placed,
            )
            .unwrap();
        let placed = placed.unwrap();
        list.Reset(&allocator, None).unwrap();
        list.CopyBufferRegion(&placed, 0, &input, 0, 256);
        let mut barrier = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: ManuallyDrop::new(Some(placed.clone())),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_COPY_DEST,
                    StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                }),
            },
        };
        list.ResourceBarrier(core::slice::from_ref(&barrier));
        ManuallyDrop::drop(&mut (*barrier.Anonymous.Transition).pResource);
        list.CopyBufferRegion(&output, 0, &placed, 0, 256);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(&queue, &fence, 5);
        output.Map(0, None, Some(&mut mapped)).unwrap();
        for i in 0..64 {
            assert_eq!(mapped.cast::<u32>().add(i).read(), 0x12340000 + i as u32);
        }
        output.Unmap(0, None);
        debug_errors(&device);
        println!("placed heap resource, nested transition barrier and readback bytes verified");
        let instance = GetModuleHandleW(None).unwrap();
        let class = w!("LocumD3d12Smoke");
        assert_ne!(
            RegisterClassW(&WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance.into(),
                lpszClassName: class,
                ..Default::default()
            }),
            0
        );
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("locum WARP12 smoke"),
            WS_OVERLAPPEDWINDOW,
            0,
            0,
            64,
            64,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .unwrap();
        let chain = factory
            .CreateSwapChainForHwnd(
                &queue,
                hwnd,
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
                None,
            )
            .unwrap();
        let from_chain: ID3D12Device = chain.GetDevice().unwrap();
        identity(&device, &from_chain);
        let back: ID3D12Resource = chain.GetBuffer(0).unwrap();
        let again: ID3D12Resource = chain.GetBuffer(0).unwrap();
        identity(&back, &again);
        let parent: IDXGIFactory4 = chain.GetParent().unwrap();
        identity(&parent, &factory);
        drop(back);
        drop(again);
        chain.Present(0, DXGI_PRESENT_TEST).ok().unwrap();
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        finish(&queue, &fence, 6);
        chain
            .ResizeBuffers(2, 64, 64, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SWAP_CHAIN_FLAG(0))
            .unwrap();
        let chain3 = chain.cast::<IDXGISwapChain3>().unwrap();
        let masks = [1u32; 2];
        let queues = [
            Some(queue.cast::<IUnknown>().unwrap()),
            Some(queue.cast::<IUnknown>().unwrap()),
        ];
        chain3
            .ResizeBuffers1(
                0,
                64,
                64,
                DXGI_FORMAT_R8G8B8A8_UNORM,
                DXGI_SWAP_CHAIN_FLAG(0),
                masks.as_ptr(),
                queues.as_ptr(),
            )
            .unwrap();
        drop(chain3);
        drop(chain);
        DestroyWindow(hwnd).unwrap();
        println!("DXGI queue unwrap, GetDevice/GetParent, backbuffer identity and resize verified");
        // Concurrent GetDevice/QI acquisition shares one canonical wrapper identity.
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    for _ in 0..100 {
                        let mut d: Option<ID3D12Device> = None;
                        input.GetDevice(&mut d).unwrap();
                        identity(&device, d.as_ref().unwrap());
                    }
                });
            }
        });
        debug_errors(&device);
        println!("concurrent identity queries and debug error census verified");
    }
}

unsafe fn debug_errors(device: &ID3D12Device) {
    unsafe {
        if let Ok(info) = device.cast::<ID3D12InfoQueue>() {
            let count = info.GetNumStoredMessagesAllowedByRetrievalFilter();
            let mut errors = 0;
            for i in 0..count {
                let mut n = 0;
                info.GetMessage(i, None, &mut n).unwrap();
                let mut words = vec![0usize; n.div_ceil(size_of::<usize>())];
                let msg = words.as_mut_ptr().cast::<D3D12_MESSAGE>();
                info.GetMessage(i, Some(msg), &mut n).unwrap();
                if (*msg).Severity == D3D12_MESSAGE_SEVERITY_ERROR
                    || (*msg).Severity == D3D12_MESSAGE_SEVERITY_CORRUPTION
                {
                    errors += 1;
                    eprintln!(
                        "debug error: {}",
                        String::from_utf8_lossy(core::slice::from_raw_parts(
                            (*msg).pDescription.cast(),
                            (*msg).DescriptionByteLength
                        ))
                    );
                }
            }
            assert_eq!(errors, 0, "D3D12 debug errors");
        }
    }
}
