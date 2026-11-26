//! Planted-truth D3D12 memory fixture for `locum-mem` verification.
//!
//! Plants four known allocations (plus a tiled reserved buffer with `UpdateTileMappings`):
//! 1. `1 GiB` `D3D12_HEAP_TYPE_GPU_UPLOAD` (fits in ReBAR local VRAM)
//! 2. `3 GiB` `D3D12_HEAP_TYPE_GPU_UPLOAD` (exceeds single-allocation BAR limit -> silent host fallback)
//! 3. `256 MiB` `D3D12_HEAP_TYPE_CUSTOM` L0 WriteBack with `D3D12_HEAP_FLAG_ALLOW_WRITE_WATCH` (`0x200`)
//! 4. `512 MiB` untouched `VirtualAlloc(MEM_RESERVE | MEM_COMMIT)` (must report 0 resident bytes)
//! 5. `64 MiB` `DEFAULT` VRAM heap mapped via `UpdateTileMappings` onto a reserved buffer
//!
//! Writes `--labels-out <ranges.tsv>`, signals `--ready-file <path>`, and holds allocations alive
//! until `--stop-file <path>` appears (or `--hold-secs` elapses), optionally running a steady
//! GPU+CPU work loop (`--work-loop`) to measure zero-suspend inspection overhead (<2%).

use std::ffi::c_void;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Memory::{
    MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
};
use windows::core::Interface;

const MIB: u64 = 1 << 20;

fn buf_desc(size: u64, flags: D3D12_RESOURCE_FLAGS) -> D3D12_RESOURCE_DESC {
    D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    }
}

unsafe fn query_vmi(a3: &IDXGIAdapter3) -> (u64, u64) {
    let mut loc = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    let mut non = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
    let _ = unsafe { a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut loc) };
    let _ = unsafe { a3.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut non) };
    (loc.CurrentUsage, non.CurrentUsage)
}

unsafe fn touch_stride(p: *mut u8, size: u64, stride: usize) {
    let mut off = 0usize;
    let total = size as usize;
    while off < total {
        unsafe { p.add(off).write_volatile(0x5A) };
        off += stride;
    }
    if total > 0 {
        unsafe { p.add(total - 1).write_volatile(0xA5) };
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let mut labels_out: Option<PathBuf> = None;
    let mut ready_file: Option<PathBuf> = None;
    let mut stop_file: Option<PathBuf> = None;
    let mut hold_secs: f64 = 15.0;
    let mut work_loop = false;
    let mut touch_all_3g = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--labels-out" => labels_out = args.next().map(PathBuf::from),
            "--ready-file" => ready_file = args.next().map(PathBuf::from),
            "--stop-file" => stop_file = args.next().map(PathBuf::from),
            "--hold-secs" => {
                if let Some(v) = args.next() {
                    hold_secs = v.parse().unwrap_or(15.0);
                }
            }
            "--work-loop" => work_loop = true,
            "--touch-all-3g" => touch_all_3g = true,
            _ => {}
        }
    }

    unsafe {
        let factory: IDXGIFactory6 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByGpuPreference(0, DXGI_GPU_PREFERENCE_HIGH_PERFORMANCE)
            .unwrap();
        let ad = adapter.GetDesc1().unwrap();
        let name = String::from_utf16_lossy(&ad.Description);
        let a3: IDXGIAdapter3 = adapter.cast().unwrap();
        let mut dev: Option<ID3D12Device> = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut dev).unwrap();
        let dev = dev.unwrap();

        let mut o16 = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
        let _ = dev.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS16,
            (&mut o16 as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS16).cast(),
            size_of_val(&o16) as u32,
        );
        let pid = std::process::id();
        println!(
            "fixture pid={pid} adapter={} dedicated_mib={} shared_mib={} gpu_upload_supported={}",
            name.trim_end_matches('\0'),
            ad.DedicatedVideoMemory as u64 / MIB,
            ad.SharedSystemMemory as u64 / MIB,
            o16.GPUUploadHeapSupported.as_bool()
        );

        let queue: ID3D12CommandQueue = dev
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .unwrap();
        let alloc: ID3D12CommandAllocator = dev
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = dev
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None)
            .unwrap();
        list.Close().unwrap();
        let fence: ID3D12Fence = dev.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
        let mut fv = 0u64;

        let mut tsv = String::from("start\tend\towner\tnote\n");
        let uav = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;

        let (loc0, non0) = query_vmi(&a3);
        println!(
            "baseline qvmi: local={} MiB, nonlocal={} MiB",
            loc0 / MIB,
            non0 / MIB
        );

        // 1. 1 GiB GPU_UPLOAD
        let sz_1g = 1024 * MIB;
        let gu_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_GPU_UPLOAD,
            ..Default::default()
        };
        let mut r_1g: Option<ID3D12Resource> = None;
        dev.CreateCommittedResource(
            &gu_props,
            D3D12_HEAP_FLAG_NONE,
            &buf_desc(sz_1g, uav),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut r_1g,
        )
        .unwrap();
        let r_1g = r_1g.unwrap();
        let mut p_1g: *mut c_void = std::ptr::null_mut();
        r_1g.Map(0, None, Some(&mut p_1g)).unwrap();
        touch_stride(p_1g.cast::<u8>(), sz_1g, 65536);
        let gva_1g = r_1g.GetGPUVirtualAddress();
        let (loc1, non1) = query_vmi(&a3);
        println!(
            "planted gpu_upload_1g: cpu={p_1g:p} gpu={gva_1g:#x} delta_local=+{} MiB delta_nonlocal=+{} MiB",
            (loc1.saturating_sub(loc0)) / MIB,
            (non1.saturating_sub(non0)) / MIB
        );
        tsv.push_str(&format!(
            "{:#x}\t{:#x}\tgpu_upload_1g\tcpu_map\n{:#x}\t{:#x}\tgpu_upload_1g\tgpu_va\n",
            p_1g as u64,
            p_1g as u64 + sz_1g,
            gva_1g,
            gva_1g + sz_1g
        ));

        // 2. 3 GiB GPU_UPLOAD (triggers NVIDIA >2 GiB BAR silent host fallback!)
        let sz_3g = 3072 * MIB;
        let mut r_3g: Option<ID3D12Resource> = None;
        dev.CreateCommittedResource(
            &gu_props,
            D3D12_HEAP_FLAG_NONE,
            &buf_desc(sz_3g, uav),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut r_3g,
        )
        .unwrap();
        let r_3g = r_3g.unwrap();
        let mut p_3g: *mut c_void = std::ptr::null_mut();
        r_3g.Map(0, None, Some(&mut p_3g)).unwrap();
        let stride_3g = if touch_all_3g { 4096 } else { 65536 };
        touch_stride(p_3g.cast::<u8>(), sz_3g, stride_3g);
        let gva_3g = r_3g.GetGPUVirtualAddress();
        let (loc2, non2) = query_vmi(&a3);
        println!(
            "planted gpu_upload_3g: cpu={p_3g:p} gpu={gva_3g:#x} delta_local=+{} MiB delta_nonlocal=+{} MiB",
            (loc2.saturating_sub(loc1)) / MIB,
            (non2.saturating_sub(non1)) / MIB
        );
        tsv.push_str(&format!(
            "{:#x}\t{:#x}\tgpu_upload_3g\tcpu_map\n{:#x}\t{:#x}\tgpu_upload_3g\tgpu_va\n",
            p_3g as u64,
            p_3g as u64 + sz_3g,
            gva_3g,
            gva_3g + sz_3g
        ));

        // 3. 256 MiB CUSTOM L0 WriteBack + 0x200 (D3D12_HEAP_FLAG_ALLOW_WRITE_WATCH)
        let sz_l0 = 256 * MIB;
        let l0_props = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let l0_hd = D3D12_HEAP_DESC {
            SizeInBytes: sz_l0,
            Properties: l0_props,
            Alignment: 0x1_0000,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS | D3D12_HEAP_FLAG_ALLOW_WRITE_WATCH,
        };
        assert_eq!(D3D12_HEAP_FLAG_ALLOW_WRITE_WATCH.0, 0x200);
        let mut h_l0: Option<ID3D12Heap> = None;
        dev.CreateHeap(&l0_hd, &mut h_l0).unwrap();
        let h_l0 = h_l0.unwrap();
        let mut r_l0: Option<ID3D12Resource> = None;
        dev.CreatePlacedResource(
            &h_l0,
            0,
            &buf_desc(sz_l0, uav),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut r_l0,
        )
        .unwrap();
        let r_l0 = r_l0.unwrap();
        let mut p_l0: *mut c_void = std::ptr::null_mut();
        r_l0.Map(0, None, Some(&mut p_l0)).unwrap();
        touch_stride(p_l0.cast::<u8>(), sz_l0, 4096);
        let gva_l0 = r_l0.GetGPUVirtualAddress();
        let (loc3, non3) = query_vmi(&a3);
        println!(
            "planted custom_l0_0x200: cpu={p_l0:p} gpu={gva_l0:#x} delta_local=+{} MiB delta_nonlocal=+{} MiB",
            (loc3.saturating_sub(loc2)) / MIB,
            (non3.saturating_sub(non2)) / MIB
        );
        tsv.push_str(&format!(
            "{:#x}\t{:#x}\tcustom_l0_0x200\tcpu_map\n{:#x}\t{:#x}\tcustom_l0_0x200\tgpu_va\n",
            p_l0 as u64,
            p_l0 as u64 + sz_l0,
            gva_l0,
            gva_l0 + sz_l0
        ));

        // 4. 512 MiB untouched MEM_COMMIT (NEVER read or written!)
        let sz_untouched = 512 * MIB;
        let p_untouched = VirtualAlloc(
            None,
            sz_untouched as usize,
            MEM_RESERVE | MEM_COMMIT,
            PAGE_READWRITE,
        );
        assert!(!p_untouched.is_null());
        println!(
            "planted untouched_commit: cpu={p_untouched:p} size={} MiB (0 bytes touched)",
            sz_untouched / MIB
        );
        tsv.push_str(&format!(
            "{:#x}\t{:#x}\tuntouched_commit\tvirtualalloc_untouched\n",
            p_untouched as u64,
            p_untouched as u64 + sz_untouched
        ));

        // 5. 64 MiB DEFAULT VRAM heap + reserved buffer with UpdateTileMappings
        let sz_sparse = 64 * MIB;
        let vram_hd = D3D12_HEAP_DESC {
            SizeInBytes: sz_sparse,
            Properties: D3D12_HEAP_PROPERTIES {
                Type: D3D12_HEAP_TYPE_DEFAULT,
                ..Default::default()
            },
            Alignment: 0,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
        };
        let mut h_vram: Option<ID3D12Heap> = None;
        dev.CreateHeap(&vram_hd, &mut h_vram).unwrap();
        let h_vram = h_vram.unwrap();
        let mut r_sparse: Option<ID3D12Resource> = None;
        dev.CreateReservedResource(
            &buf_desc(sz_sparse, uav),
            D3D12_RESOURCE_STATE_COMMON,
            None,
            &mut r_sparse,
        )
        .unwrap();
        let r_sparse = r_sparse.unwrap();
        let tiles = (sz_sparse / 65536) as u32;
        let coord = D3D12_TILED_RESOURCE_COORDINATE::default();
        let region = D3D12_TILE_REGION_SIZE {
            NumTiles: tiles,
            UseBox: false.into(),
            ..Default::default()
        };
        let off = 0u32;
        queue.UpdateTileMappings(
            &r_sparse,
            1,
            Some(&coord),
            Some(&region),
            &h_vram,
            1,
            Some(&D3D12_TILE_RANGE_FLAG_NONE),
            Some(&off),
            Some(&tiles),
            D3D12_TILE_MAPPING_FLAG_NONE,
        );
        let gva_sparse = r_sparse.GetGPUVirtualAddress();
        tsv.push_str(&format!(
            "{:#x}\t{:#x}\tsparse_default_64m\tgpu_va\n",
            gva_sparse,
            gva_sparse + sz_sparse
        ));

        // Submit a tiny GPU copy touching r_1g, r_3g, r_l0, r_sparse so VidMm commits/pages them in!
        let copy_one = |dst: &ID3D12Resource, src: &ID3D12Resource, fv: &mut u64| {
            alloc.Reset().unwrap();
            list.Reset(&alloc, None).unwrap();
            list.CopyBufferRegion(dst, 0, src, 0, 4096);
            list.Close().unwrap();
            queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().unwrap())]);
            *fv += 1;
            queue.Signal(&fence, *fv).unwrap();
            while fence.GetCompletedValue() < *fv {
                std::thread::yield_now();
            }
        };
        copy_one(&r_sparse, &r_1g, &mut fv);
        copy_one(&r_sparse, &r_3g, &mut fv);
        copy_one(&r_sparse, &r_l0, &mut fv);

        let (loc_f, non_f) = query_vmi(&a3);
        println!(
            "final qvmi: local={} MiB, nonlocal={} MiB",
            loc_f / MIB,
            non_f / MIB
        );
        // Print raw QueryWorkingSetEx flags + random 8-byte CPU read latency for each planted mapping
        for (lbl, ptr, sz) in [
            ("gpu_upload_1g", p_1g, sz_1g),
            ("gpu_upload_3g", p_3g, sz_3g),
            ("custom_l0_0x200", p_l0, sz_l0),
            ("untouched_commit", p_untouched, sz_untouched),
        ] {
            let mut ws = windows::Win32::System::ProcessStatus::PSAPI_WORKING_SET_EX_INFORMATION {
                VirtualAddress: ptr,
                ..Default::default()
            };
            let _ = windows::Win32::System::ProcessStatus::QueryWorkingSetEx(
                windows::Win32::System::Threading::GetCurrentProcess(),
                (&mut ws as *mut windows::Win32::System::ProcessStatus::PSAPI_WORKING_SET_EX_INFORMATION)
                    .cast(),
                size_of_val(&ws) as u32,
            );
            let flags = ws.VirtualAttributes.Flags;
            let rand_ns = if lbl != "untouched_commit" {
                let mut h = 0x1234_5678_9ABC_DEF1u64;
                let mut acc = 0u64;
                let n = 50_000usize;
                let t = Instant::now();
                for _ in 0..n {
                    h ^= h << 13;
                    h ^= h >> 7;
                    h ^= h << 17;
                    let off = (h as usize % ((sz.min(64 * MIB) as usize) / 64)) * 64;
                    acc = acc.wrapping_add((ptr.cast::<u8>().add(off) as *const u64).read_volatile());
                }
                std::hint::black_box(acc);
                t.elapsed().as_nanos() as f64 / n as f64
            } else {
                0.0
            };
            println!("diag {lbl:<18} ptr={ptr:p} ws_flags={flags:#018x} cpu_rand_read_ns={rand_ns:.1}");
        }

        if let Some(ref lp) = labels_out {
            std::fs::write(lp, &tsv).unwrap();
            println!("wrote labels: {}", lp.display());
        }
        if let Some(ref rf) = ready_file {
            std::fs::write(rf, format!("{pid}\n")).unwrap();
        }

        let start = Instant::now();
        let mut loop_iters = 0u64;
        let mut loop_ms_sum = 0.0f64;
        let mut cpu_acc = 0u64;
        while start.elapsed().as_secs_f64() < hold_secs {
            if let Some(ref sf) = stop_file
                && sf.exists()
            {
                break;
            }
            if work_loop {
                let t0 = Instant::now();
                // Repeatable ~3 ms frame-like GPU copy (64 MiB) + CPU work over custom_l0_0x200.
                alloc.Reset().unwrap();
                list.Reset(&alloc, None).unwrap();
                for _ in 0..8 {
                    list.CopyBufferRegion(&r_sparse, 0, &r_1g, 0, sz_sparse);
                }
                list.Close().unwrap();
                queue.ExecuteCommandLists(&[Some(list.cast::<ID3D12CommandList>().unwrap())]);
                fv += 1;
                queue.Signal(&fence, fv).unwrap();
                // CPU work in parallel with GPU copy
                let slice = std::slice::from_raw_parts(p_l0.cast::<u64>(), 1 << 18);
                for &w in slice.iter().step_by(8) {
                    cpu_acc = cpu_acc.wrapping_add(w);
                }
                while fence.GetCompletedValue() < fv {
                    std::thread::yield_now();
                }
                loop_ms_sum += t0.elapsed().as_secs_f64() * 1e3;
                loop_iters += 1;
            } else {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        std::hint::black_box(cpu_acc);
        if work_loop && loop_iters > 0 {
            println!(
                "work_loop: iters={loop_iters} mean_ms={:.4} total_s={:.3}",
                loop_ms_sum / loop_iters as f64,
                start.elapsed().as_secs_f64()
            );
        }

        let _ = VirtualFree(p_untouched, 0, MEM_RELEASE);
        drop((r_sparse, h_vram, r_l0, h_l0, r_3g, r_1g));
    }
}
