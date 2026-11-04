//! Run explicitly in release mode, alone, outside game measurement windows.
use super::{descriptors, journal};
use std::hint::black_box;
use std::sync::{Arc, Barrier};
use std::time::Instant;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_FLOAT;
use windows::core::{GUID, HRESULT, Interface};

fn wrapped_resource() -> ID3D12Resource {
    use core::ffi::c_void;
    use windows::Win32::Graphics::{Direct3D::*, Dxgi::Common::*, Dxgi::*};
    unsafe {
        // Dynamic system exports avoid linking duplicate shim export names into the test executable.
        type Factory = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> HRESULT;
        type Device = unsafe extern "system" fn(
            *mut c_void,
            D3D_FEATURE_LEVEL,
            *const GUID,
            *mut *mut c_void,
        ) -> HRESULT;
        let factory_fn =
            crate::system::export::<Factory>(crate::system::system().dxgi, c"CreateDXGIFactory2").unwrap();
        let device_fn =
            crate::system::export::<Device>(crate::system::system_d3d12(), c"D3D12CreateDevice").unwrap();
        let mut raw = std::ptr::null_mut();
        factory_fn(0, &IDXGIFactory4::IID, &mut raw).ok().unwrap();
        let factory = IDXGIFactory4::from_raw(raw);
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter().unwrap();
        device_fn(
            adapter.as_raw(),
            D3D_FEATURE_LEVEL_11_0,
            &ID3D12Device::IID,
            &mut raw,
        )
        .ok()
        .unwrap();
        let device = ID3D12Device::from_raw(raw);
        let mut resource: Option<ID3D12Resource> = None;
        device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
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
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut resource,
            )
            .unwrap();
        let mut raw = resource.unwrap().into_raw();
        super::wrap_output(HRESULT(0), &ID3D12Resource::IID, &mut raw);
        ID3D12Resource::from_raw(raw)
    }
}

const BASE: usize = 0x100000000;
const HEAP_SPAN: usize = 0x100000;
const STRIDE: usize = 32;
const SLOTS: u32 = 2072;
fn handle(heap: usize, slot: usize) -> D3D12_CPU_DESCRIPTOR_HANDLE {
    D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: BASE + heap * HEAP_SPAN + slot * STRIDE,
    }
}
fn srv() -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R32_FLOAT,
        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                NumElements: 64,
                ..Default::default()
            },
        },
    }
}

#[test]
#[ignore = "release multithread performance experiment; run alone"]
fn real_journal_hooks() {
    assert!(journal::enabled());
    let resource = wrapped_resource();
    let resource_pointer = resource.as_raw() as usize;
    let iterations = std::env::var("LOCUM_D3D12_BENCH_ITERS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(100_000);
    for heap in 0..81 {
        journal::descriptor_addresses(
            100_000 + heap as u64,
            handle(heap, 0).ptr as u64,
            handle(heap, 0).ptr as u64 + 0x1000000000,
            SLOTS,
            STRIDE as u32,
        );
        for slot in 0..SLOTS as usize {
            unsafe {
                descriptors::srv(std::ptr::null_mut(), &srv(), handle(heap, slot));
            }
        }
    }
    for i in 0..1295 {
        journal::resource_address(200_000 + i, 0x2000000000 + i * 4096, 4096);
    }
    println!(
        "real journal hooks, 81 heaps, {} initialized slots, 1295 VA ranges; native D3D12 call excluded",
        81 * SLOTS
    );
    println!("hook,threads,operations,wall_ms,wall_ns_per_op,summed_thread_ns_per_op");
    for threads in [1, 4, 8] {
        for case in [
            "create_srv",
            "create_resource",
            "create_cbv",
            "copy_1",
            "copy_32",
            "address",
            "gpu_handle",
            "copy_core",
            "address_core",
            "gpu_core",
        ] {
            let barrier = Arc::new(Barrier::new(threads + 1));
            let workers: Vec<_> = (0..threads)
                .map(|worker| {
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        let desc = srv();
                        let per_call = if matches!(case, "copy_32" | "copy_core") {
                            32
                        } else {
                            1
                        };
                        let calls = iterations / per_call;
                        barrier.wait();
                        let start = Instant::now();
                        for n in 0..calls {
                            let slot = worker * 256 + (n % 8) * 32;
                            match case {
                                "create_srv" => unsafe {
                                    descriptors::srv(std::ptr::null_mut(), black_box(&desc), handle(1, slot));
                                },
                                "create_resource" => unsafe {
                                    descriptors::srv(
                                        resource_pointer as *mut _,
                                        black_box(&desc),
                                        handle(1, slot),
                                    );
                                },
                                "create_cbv" => unsafe {
                                    descriptors::cb(
                                        &D3D12_CONSTANT_BUFFER_VIEW_DESC {
                                            BufferLocation: 0x2000000000 + (n % 1295) as u64 * 4096 + 256,
                                            SizeInBytes: 256,
                                        },
                                        handle(1, slot),
                                    );
                                },
                                "copy_1" | "copy_32" => unsafe {
                                    descriptors::copy_simple(
                                        per_call as u32,
                                        handle(1, slot),
                                        handle(0, (n % 8) * 32),
                                    );
                                },
                                "copy_core" => {
                                    super::slots::copy_simple(
                                        per_call as u32,
                                        handle(1, slot).ptr as u64,
                                        handle(0, (n % 8) * 32).ptr as u64,
                                    );
                                }
                                "address_core" => {
                                    black_box(super::slots::address(
                                        0x2000000000 + (n % 1295) as u64 * 4096 + 128,
                                    ));
                                }
                                "gpu_core" => {
                                    black_box(super::slots::descriptor(
                                        handle(1, slot).ptr as u64 + 0x1000000000,
                                        true,
                                    ));
                                }
                                "address" => {
                                    black_box(journal::address(
                                        0x2000000000 + (n % 1295) as u64 * 4096 + 128,
                                    ));
                                }
                                "gpu_handle" => {
                                    black_box(journal::descriptor(
                                        handle(1, slot).ptr as u64 + 0x1000000000,
                                        true,
                                    ));
                                }
                                _ => unreachable!(),
                            }
                        }
                        (start.elapsed(), calls * per_call)
                    })
                })
                .collect();
            let start = Instant::now();
            barrier.wait();
            let mut duration = 0u128;
            let mut operations = 0;
            for worker in workers {
                let (time, count) = worker.join().unwrap();
                duration += time.as_nanos();
                operations += count;
            }
            let wall = start.elapsed();
            println!(
                "{case},{threads},{operations},{:.3},{:.1},{:.1}",
                wall.as_secs_f64() * 1000.,
                wall.as_nanos() as f64 / operations as f64,
                duration as f64 / operations as f64
            );
        }
    }
    let d = journal::descriptor(handle(1, 0).ptr as u64, false);
    assert_eq!(d["definition"]["kind"], "srv");
    assert_eq!(d["definition"]["description"]["payload"]["NumElements"], 64);
    for heap in 0..81 {
        journal::destroy(100_000 + heap);
    }
    for i in 0..1295 {
        journal::destroy(200_000 + i);
    }
}
