//! Exercises real WARP12 calls through the shims. This is not a game launcher.
use core::{ffi::c_void, mem::ManuallyDrop};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::{Direct3D::*, Direct3D12::*, Dxgi::Common::*, Dxgi::*};
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::*;
mod graphics;
mod modern;

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
unsafe extern "system" fn debug_message(
    _: D3D12_MESSAGE_CATEGORY,
    severity: D3D12_MESSAGE_SEVERITY,
    id: D3D12_MESSAGE_ID,
    description: PCSTR,
    _: *mut c_void,
) {
    if severity == D3D12_MESSAGE_SEVERITY_ERROR || severity == D3D12_MESSAGE_SEVERITY_CORRUPTION {
        eprintln!(
            "D3D12 debug {id:?}: {}",
            unsafe { description.to_string() }.unwrap_or_default()
        );
    }
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
        // Emit errors when they happen, including fatal validation during release.
        let info = device.cast::<ID3D12InfoQueue1>().ok();
        let mut cookie = 0;
        if let Some(info) = &info {
            info.RegisterMessageCallback(
                Some(debug_message),
                D3D12_MESSAGE_CALLBACK_FLAG_NONE,
                core::ptr::null_mut(),
                &mut cookie,
            )
            .unwrap();
        }
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
        journal_capture(&device, &queue, &fence, &chain, &code);
        finish(&queue, &fence, 9);
        graphics::capture(&device, &queue, &fence, &chain);
        // The fixture's closing Present queues backbuffer work after its readback
        // fence. Complete that work before ResizeBuffers releases the buffers.
        finish(&queue, &fence, 11);
        modern::capture(&device, &queue, &fence, &chain);
        finish(&queue, &fence, 13);
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
        if let Some(info) = info {
            info.UnregisterMessageCallback(cookie).unwrap();
        }
    }
}

unsafe fn journal_capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    code: &ID3DBlob,
) {
    unsafe {
        let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").expect("smoke LOCUM_DIR"));
        let range = D3D12_DESCRIPTOR_RANGE {
            RangeType: D3D12_DESCRIPTOR_RANGE_TYPE_CBV,
            NumDescriptors: 1,
            BaseShaderRegister: 3,
            RegisterSpace: 7,
            OffsetInDescriptorsFromTableStart: 0,
        };
        let parameters = [
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE {
                        NumDescriptorRanges: 1,
                        pDescriptorRanges: &range,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 4,
                        RegisterSpace: 7,
                        Num32BitValues: 4,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
            D3D12_ROOT_PARAMETER {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_CBV,
                Anonymous: D3D12_ROOT_PARAMETER_0 {
                    Descriptor: D3D12_ROOT_DESCRIPTOR {
                        ShaderRegister: 5,
                        RegisterSpace: 7,
                    },
                },
                ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
            },
        ];
        let root_desc = D3D12_ROOT_SIGNATURE_DESC {
            NumParameters: parameters.len() as u32,
            pParameters: parameters.as_ptr(),
            ..Default::default()
        };
        let mut serialized = None;
        D3D12SerializeRootSignature(&root_desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut serialized, None).unwrap();
        let serialized = serialized.unwrap();
        let root: ID3D12RootSignature = device
            .CreateRootSignature(
                0,
                core::slice::from_raw_parts(serialized.GetBufferPointer().cast(), serialized.GetBufferSize()),
            )
            .unwrap();
        let mut pso_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: ManuallyDrop::new(Some(root.clone())),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.GetBufferPointer(),
                BytecodeLength: code.GetBufferSize(),
            },
            ..Default::default()
        };
        let pso: ID3D12PipelineState = device.CreateComputePipelineState(&pso_desc).unwrap();
        ManuallyDrop::drop(&mut pso_desc.pRootSignature);

        let upload_a = buffer(device, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
        let upload_b = buffer(device, D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ);
        let readback = buffer(device, D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST);
        let mut host = core::ptr::null_mut();
        upload_a.Map(0, None, Some(&mut host)).unwrap();
        host.cast::<u8>().write_bytes(0x11, 256);
        let mut other = core::ptr::null_mut();
        upload_b.Map(0, None, Some(&mut other)).unwrap();
        other.cast::<u8>().write_bytes(0x33, 256);
        upload_b.Unmap(0, None);
        let cpu_heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                ..Default::default()
            })
            .unwrap();
        let gpu_heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                ..Default::default()
            })
            .unwrap();
        let cpu = cpu_heap.GetCPUDescriptorHandleForHeapStart();
        let gpu = gpu_heap.GetCPUDescriptorHandleForHeapStart();
        device.CreateConstantBufferView(
            Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: upload_a.GetGPUVirtualAddress(),
                SizeInBytes: 256,
            }),
            cpu,
        );
        device.CopyDescriptorsSimple(1, gpu, cpu, D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
        // Overwriting the source must not retroactively change the copied destination.
        device.CreateConstantBufferView(
            Some(&D3D12_CONSTANT_BUFFER_VIEW_DESC {
                BufferLocation: upload_b.GetGPUVirtualAddress(),
                SizeInBytes: 256,
            }),
            cpu,
        );
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, &pso)
            .unwrap();
        // Record before the capture request: the test fails if capture relies on armed-only history.
        list.SetDescriptorHeaps(&[Some(gpu_heap.clone())]);
        list.SetDescriptorHeaps(&[Some(gpu_heap.clone())]);
        list.SetComputeRootSignature(&root);
        list.SetComputeRootDescriptorTable(0, gpu_heap.GetGPUDescriptorHandleForHeapStart());
        list.SetComputeRoot32BitConstants(1, 2, [11u32, 12].as_ptr().cast(), 0);
        list.SetComputeRoot32BitConstants(1, 2, [13u32, 14].as_ptr().cast(), 2);
        list.SetComputeRootConstantBufferView(2, upload_a.GetGPUVirtualAddress());
        list.Dispatch(2, 3, 1);
        list.CopyBufferRegion(&readback, 0, &upload_a, 0, 256);
        list.Close().unwrap();
        let label = format!("journal-{}", std::process::id());
        std::fs::write(dir.join("capture"), format!("label {label}\nskip memory\n")).unwrap();
        chain.Present(0, DXGI_PRESENT_TEST).ok().unwrap();
        assert!(
            dir.join("capture").exists(),
            "TEST present must not consume a capture"
        );
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 6);
        verify_fill(&readback, 0x11);
        host.cast::<u8>().write_bytes(0x22, 256);
        let count = 1;
        device.CopyDescriptors(
            1,
            &gpu,
            Some(&count),
            1,
            &cpu,
            Some(&count),
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
        );
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 7);
        verify_fill(&readback, 0x22);
        allocator.Reset().unwrap();
        list.Reset(&allocator, &pso).unwrap();
        list.SetDescriptorHeaps(&[Some(gpu_heap.clone())]);
        list.SetComputeRootSignature(&root);
        list.SetComputeRootDescriptorTable(0, gpu_heap.GetGPUDescriptorHandleForHeapStart());
        list.SetComputeRoot32BitConstants(1, 2, [11u32, 12].as_ptr().cast(), 0);
        list.SetComputeRoot32BitConstants(1, 2, [13u32, 14].as_ptr().cast(), 2);
        list.SetComputeRootConstantBufferView(2, upload_a.GetGPUVirtualAddress());
        list.Dispatch(7, 1, 1);
        list.CopyBufferRegion(&readback, 0, &upload_a, 0, 256);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, 8);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        upload_a.Unmap(0, None);
        let path = dir.join("captures").join(label).join("capture.db");
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
        for (query, expected) in [
            ("SELECT count(*) FROM d12_submissions", 3),
            // Each submission records the descriptor definitions that changed since the previous one: all three
            // at the first, the copied CBV at the second, nothing at the third.
            (
                "SELECT count(*) FROM d12_submission_descriptors WHERE submission=(SELECT min(id) FROM d12_submissions)",
                3,
            ),
            (
                "SELECT count(*) FROM d12_submission_descriptors WHERE submission=(SELECT min(id) FROM d12_submissions)+1",
                1,
            ),
            (
                "SELECT count(*) FROM d12_submission_descriptors WHERE submission=(SELECT max(id) FROM d12_submissions)",
                0,
            ),
            ("SELECT count(*) FROM d12_executions", 3),
            ("SELECT count(*) FROM d12_lists", 2),
            ("SELECT count(*) FROM draws WHERE call='Dispatch'", 3),
            ("SELECT count(*) FROM draws WHERE args='[2,3,1]'", 2),
            ("SELECT count(*) FROM draws WHERE args='[7,1,1]'", 1),
            ("SELECT count(*) FROM pragma_foreign_key_check", 0),
            ("SELECT count(*) FROM draws WHERE cs IS NOT NULL", 3),
            (
                "SELECT count(*) FROM bindings WHERE stage='cs' AND kind='cb' AND space=7",
                9,
            ),
            (
                "SELECT count(*) FROM bindings WHERE kind='cb' AND content IS NOT NULL",
                9,
            ),
            // Each dispatch reads its own submission's bytes: the root CBV sees 0x11 then the host's 0x22, the
            // descriptor CBV the copied-in buffer's 0x33 after CopyDescriptors.
            (
                "SELECT count(*) FROM bindings b JOIN cb_data d ON d.hash=b.content WHERE b.kind='cb' AND                  hex(substr(d.bytes,1,1))=CASE WHEN b.draw=1 THEN '11' WHEN b.slot=5 THEN '22' ELSE '33' END AND                  b.slot IN (3,5)",
                6,
            ),
            (
                "SELECT count(*) FROM d12_draw_state WHERE pso!=0 AND compute_root!=0",
                3,
            ),
            (
                "SELECT count(DISTINCT r.id) FROM d12_roots r JOIN d12_draw_state s ON s.compute_root=r.id WHERE json_array_length(r.decoded_json,'$.parameters')=3",
                1,
            ),
            (
                "SELECT count(*) FROM (SELECT heap FROM d12_descriptor_versions WHERE kind='cb' GROUP BY heap HAVING count(DISTINCT resource)=2)",
                1,
            ),
        ] {
            assert_eq!(
                db.query_row(query, [], |r| r.get::<_, i64>(0)).unwrap(),
                expected,
                "{query}"
            );
        }
        assert_eq!(
            db.query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            locum_capture::db::D3D12_SCHEMA_VERSION.to_string()
        );
        let blobs = locum_capture::blobs::Blobs::new(dir.join("blobs"));
        let mut versions = std::collections::BTreeMap::<i64, Vec<u8>>::new();
        let mut wholes = std::collections::BTreeMap::<i64, u32>::new();
        let mut statement = db
            .prepare("SELECT resource,content_json FROM d12_upload_versions ORDER BY submission")
            .unwrap();
        for row in statement
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
        {
            let (resource, text) = row.unwrap();
            let content: serde_json::Value = serde_json::from_str(&text).unwrap();
            let bytes = blobs.get(content["blob"].as_str().unwrap()).unwrap();
            if content["whole"] == true {
                // The frame-start copy, once per upload buffer.
                assert_eq!(bytes.len() as u64, content["size"].as_u64().unwrap());
                *wholes.entry(resource).or_default() += 1;
            } else {
                versions.entry(resource).or_default().push(bytes[0]);
            }
        }
        assert!(
            versions.values().any(|v| v == &[0x11, 0x22, 0x22]),
            "persistent upload writes must be distinct per submission: {versions:?}"
        );
        assert!(
            wholes.values().all(|&n| n == 1) && versions.keys().all(|r| wholes.contains_key(r)),
            "each upload buffer is copied whole once, at the first captured submission: {wholes:?}"
        );
        let mut statement = db.prepare("SELECT hash,blob FROM shaders").unwrap();
        for row in statement
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
        {
            let (hash, key) = row.unwrap();
            assert_eq!(locum_capture::hash::key(&blobs.get(&key).unwrap()), hash);
        }
        debug_errors(device);
        println!(
            "D3D12 capture: cached lists, reset generations, PSO/shader/root data, descriptor copies/reuse, persistent upload versions, GPU bytes and SQL links verified: {}",
            path.display()
        );
    }
}

unsafe fn verify_fill(resource: &ID3D12Resource, value: u8) {
    let mut pointer = core::ptr::null_mut();
    unsafe { resource.Map(0, None, Some(&mut pointer)) }.unwrap();
    assert!(
        unsafe { core::slice::from_raw_parts(pointer.cast::<u8>(), 256) }
            .iter()
            .all(|&v| v == value)
    );
    unsafe { resource.Unmap(0, None) };
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
