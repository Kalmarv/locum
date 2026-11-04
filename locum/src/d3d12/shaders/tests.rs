use super::*;
use crate::vtables::{d12_device, d12_library, d12_pso};
use d12_device::Device12Methods;
use d12_library::Library12Methods;
use d12_pso::Pso12Methods;
use std::sync::{Arc, Barrier};
use windows::Win32::Foundation::{E_NOINTERFACE, S_FALSE, S_OK};
use windows::Win32::Graphics::Direct3D::ID3DBlob_Vtbl;
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR};

fn temporary(label: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
        "../target/shader-archive-{label}-{}-{stamp}",
        std::process::id()
    ))
}
fn saved(dir: &Path, suffix: &str, bytes: &[u8]) {
    assert_eq!(
        std::fs::read(dir.join(format!("{}.{suffix}", locum_capture::hash::key(bytes)))).unwrap(),
        bytes
    );
}

#[test]
fn deduplication_concurrency_and_retry() {
    let dir = temporary("dedupe");
    let archive = Archive::default();
    let bytes = b"DXBC opaque bytes: deliberately not parseable";
    let barrier = Barrier::new(8);
    let first = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    archive.save(&dir, "cs.dxbc", bytes).unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|w| usize::from(w.join().unwrap()))
            .sum::<usize>()
    });
    assert_eq!(first, 1);
    saved(&dir, "cs.dxbc", bytes);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    assert!(!archive.save(&dir, "cs.dxbc", bytes).unwrap());
    // No filesystem access after first observation: even a non-directory path is ignored for seen content.
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, b"not a directory").unwrap();
    assert!(!archive.save(&blocker, "cs.dxbc", bytes).unwrap());
    assert!(archive.save(&blocker, "lib.dxbc", bytes).is_err());
    assert!(archive.save(&dir, "lib.dxbc", bytes).unwrap());
    saved(&dir, "lib.dxbc", bytes);
    assert!(!archive.save(&dir, "cs.dxbc", b"").unwrap());
}

// CPU-only native stand-ins. Successful validation-only creates return S_FALSE and no COM output;
// the real generated Object implementations still forward, rebase descriptors and archive the inputs.
macro_rules! unknown {
    () => {
        fn inner(&self) -> *mut c_void {
            panic!("unexpected mock method")
        }
        unsafe fn QueryInterface(&self, _: *const GUID, out: *mut *mut c_void) -> HRESULT {
            unsafe {
                *out = core::ptr::null_mut();
            }
            E_NOINTERFACE
        }
        unsafe fn AddRef(&self) -> u32 {
            1
        }
        unsafe fn Release(&self) -> u32 {
            1
        }
    };
}
#[repr(C)]
struct Device {
    table: *const ID3D12Device7_Vtbl,
    grow_from: *mut c_void,
}
unsafe impl Device12Methods for Device {
    unknown!();
    unsafe fn CreateGraphicsPipelineState(
        &self,
        _: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
        _: *const GUID,
        _: *mut *mut c_void,
    ) -> HRESULT {
        S_FALSE
    }
    unsafe fn CreateComputePipelineState(
        &self,
        _: *const D3D12_COMPUTE_PIPELINE_STATE_DESC,
        _: *const GUID,
        _: *mut *mut c_void,
    ) -> HRESULT {
        S_FALSE
    }
    unsafe fn CreatePipelineState(
        &self,
        _: *const D3D12_PIPELINE_STATE_STREAM_DESC,
        _: *const GUID,
        _: *mut *mut c_void,
    ) -> HRESULT {
        S_FALSE
    }
    unsafe fn CreateStateObject(
        &self,
        _: *const D3D12_STATE_OBJECT_DESC,
        _: *const GUID,
        _: *mut *mut c_void,
    ) -> HRESULT {
        S_FALSE
    }
    unsafe fn CreatePipelineLibrary(
        &self,
        _: *const c_void,
        _: usize,
        _: *const GUID,
        _: *mut *mut c_void,
    ) -> HRESULT {
        S_FALSE
    }
    unsafe fn AddToStateObject(
        &self,
        desc: *const D3D12_STATE_OBJECT_DESC,
        grow_from: *mut c_void,
        _: *const GUID,
        _: *mut *mut c_void,
    ) -> HRESULT {
        assert_eq!(grow_from, self.grow_from, "forward the native state object");
        assert_eq!(unsafe { (*desc).NumSubobjects }, 1);
        S_FALSE
    }
}

#[repr(C)]
struct State {
    table: *const ID3D12StateObject_Vtbl,
}
unsafe impl crate::vtables::d12_state::State12Methods for State {
    fn inner(&self) -> *mut c_void {
        panic!("unexpected state method")
    }
    unsafe fn QueryInterface(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
        unsafe {
            if *iid == IUnknown::IID || *iid == ID3D12StateObject::IID {
                *out = (self as *const Self).cast_mut().cast();
                S_OK
            } else {
                *out = core::ptr::null_mut();
                E_NOINTERFACE
            }
        }
    }
    unsafe fn AddRef(&self) -> u32 {
        1
    }
    unsafe fn Release(&self) -> u32 {
        1
    }
}
#[repr(C)]
struct Library {
    table: *const ID3D12PipelineLibrary1_Vtbl,
    output: *mut c_void,
    capacity: usize,
}
unsafe impl Library12Methods for Library {
    unknown!();
    unsafe fn LoadGraphicsPipeline(
        &self,
        _: PCWSTR,
        _: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
        _: *const GUID,
        _: *mut *mut c_void,
    ) -> HRESULT {
        S_FALSE
    }
    unsafe fn LoadComputePipeline(
        &self,
        _: PCWSTR,
        _: *const D3D12_COMPUTE_PIPELINE_STATE_DESC,
        _: *const GUID,
        _: *mut *mut c_void,
    ) -> HRESULT {
        S_FALSE
    }
    unsafe fn LoadPipeline(
        &self,
        _: PCWSTR,
        _: *const D3D12_PIPELINE_STATE_STREAM_DESC,
        _: *const GUID,
        _: *mut *mut c_void,
    ) -> HRESULT {
        S_FALSE
    }
    unsafe fn GetSerializedSize(&self) -> usize {
        panic!("archival must not query serialization size")
    }
    unsafe fn Serialize(&self, out: *mut c_void, capacity: usize) -> HRESULT {
        assert_eq!(out, self.output);
        assert_eq!(capacity, self.capacity);
        unsafe {
            core::slice::from_raw_parts_mut(out.cast::<u8>(), capacity).fill(0x5a);
        }
        S_OK
    }
}
#[repr(C)]
struct Pso {
    table: *const ID3D12PipelineState_Vtbl,
    blob: *mut c_void,
}
unsafe impl Pso12Methods for Pso {
    unknown!();
    unsafe fn GetCachedBlob(&self, out: *mut *mut c_void) -> HRESULT {
        unsafe {
            *out = self.blob;
        }
        S_OK
    }
}
#[repr(C)]
struct Blob {
    table: *const ID3DBlob_Vtbl,
    bytes: Vec<u8>,
}
unsafe extern "system" fn blob_qi(_: *mut c_void, _: *const GUID, _: *mut *mut c_void) -> HRESULT {
    E_NOINTERFACE
}
unsafe extern "system" fn blob_ref(_: *mut c_void) -> u32 {
    panic!("archive must only borrow the native blob")
}
unsafe extern "system" fn blob_pointer(this: *mut c_void) -> *mut c_void {
    unsafe { (*this.cast::<Blob>()).bytes.as_mut_ptr().cast() }
}
unsafe extern "system" fn blob_size(this: *mut c_void) -> usize {
    unsafe { (*this.cast::<Blob>()).bytes.len() }
}
static BLOB: ID3DBlob_Vtbl = ID3DBlob_Vtbl {
    base__: IUnknown_Vtbl {
        QueryInterface: blob_qi,
        AddRef: blob_ref,
        Release: blob_ref,
    },
    GetBufferPointer: blob_pointer,
    GetBufferSize: blob_size,
};
unsafe fn object<T>(mock: &mut T) -> super::super::Object {
    super::super::Object {
        vtable: core::ptr::null(),
        real: unsafe { IUnknown::from_raw((mock as *mut T).cast()) },
        identity: 0,
        data: Arc::new(super::super::journal::Data::new(17)),
    }
}

#[test]
fn hooks_with_and_without_journal() {
    for mode in ["on", "off"] {
        let dir = temporary(mode);
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "d3d12::shaders::tests::hook_probe",
                "--ignored",
                "--nocapture",
            ])
            .env("LOCUM_DIR", &dir)
            .env("LOCUM_D3D12_JOURNAL", mode)
            .env("LOCUM_ARCHIVE_PROBE", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let log = std::fs::read_to_string(dir.join("locum.log")).unwrap();
        assert_eq!(log.matches("D3D12 first-seen pipeline-library.bin:").count(), 1);
        assert_eq!(log.matches("D3D12 first-seen cached-pso.bin:").count(), 2);
        assert_eq!(log.matches("D3D12 Serialize: library=17 capacity=31").count(), 1);
        assert!(log.contains("bytes=4096"));
        assert_eq!(log.matches("D3D12 first AddToStateObject").count(), 1);
        for tag in [32766, 32767] {
            assert_eq!(
                log.matches(&format!("PSO stream type {tag} at byte 0 has no ABI layout"))
                    .count(),
                1
            );
        }
    }
}

#[test]
#[ignore = "CPU subprocess fixture, run by hooks_with_and_without_journal"]
fn hook_probe() {
    assert_eq!(std::env::var("LOCUM_ARCHIVE_PROBE").as_deref(), Ok("1"));
    assert_eq!(
        super::super::journal::enabled(),
        std::env::var("LOCUM_D3D12_JOURNAL").unwrap() == "on"
    );
    let dir = directory().unwrap();
    assert!(crate::vtables::d12_registry::table(&ID3D12Device7::IID).is_some());
    let mut state_parent = State {
        table: &crate::vtables::d12_state::vtable::<State>(),
    };
    let mut device = Device {
        table: &d12_device::vtable::<Device>(),
        grow_from: (&mut state_parent as *mut State).cast(),
    };
    let mut serialize_output = [0xa5u8; 33];
    let mut library = Library {
        table: &d12_library::vtable::<Library>(),
        output: serialize_output[1..].as_mut_ptr().cast(),
        capacity: 31,
    };
    let cache = [0x39u8; 4096];
    let mut blob = Blob {
        table: &BLOB,
        bytes: b"opaque native cached blob output".to_vec(),
    };
    let mut pso = Pso {
        table: &d12_pso::vtable::<Pso>(),
        blob: (&mut blob as *mut Blob).cast(),
    };
    unsafe {
        let device = object(&mut device);
        let library = object(&mut library);
        let pso = object(&mut pso);
        let mut out = core::ptr::null_mut();
        // Arbitrary bytes prove neither a DXIL nor a DXBC reader gates storage. Reuse one address with
        // changed contents between each hook to catch accidental pointer-based deduplication.
        let mut code = *b"DXBCtest";
        for route in 0..6 {
            code[4] = route;
            let bytecode = D3D12_SHADER_BYTECODE {
                pShaderBytecode: code.as_ptr().cast(),
                BytecodeLength: code.len(),
            };
            let cached = D3D12_CACHED_PIPELINE_STATE {
                pCachedBlob: cache.as_ptr().cast(),
                CachedBlobSizeInBytes: cache.len(),
            };
            let graphics = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                VS: bytecode,
                HS: bytecode,
                DS: bytecode,
                GS: bytecode,
                PS: bytecode,
                CachedPSO: cached,
                ..Default::default()
            };
            let compute = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                CS: bytecode,
                CachedPSO: cached,
                ..Default::default()
            };
            // Stream ABI: pointer-aligned tag + pointer + length. Include a scalar between shaders.
            let mut stream_data = Vec::<usize>::new();
            for tag in [
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS,
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS,
            ] {
                stream_data.extend([tag.0 as usize, code.as_ptr() as usize, code.len()]);
                stream_data.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK.0 as usize);
            }
            stream_data.extend([
                D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CACHED_PSO.0 as usize,
                cache.as_ptr() as usize,
                cache.len(),
            ]);
            let stream = D3D12_PIPELINE_STATE_STREAM_DESC {
                SizeInBytes: stream_data.len() * size_of::<usize>(),
                pPipelineStateSubobjectStream: stream_data.as_mut_ptr().cast(),
            };
            let result = match route {
                0 => device.CreateGraphicsPipelineState(&graphics, &ID3D12PipelineState::IID, &mut out),
                1 => device.CreateComputePipelineState(&compute, &ID3D12PipelineState::IID, &mut out),
                2 => device.CreatePipelineState(&stream, &ID3D12PipelineState::IID, &mut out),
                3 => library.LoadGraphicsPipeline(
                    PCWSTR::null(),
                    &graphics,
                    &ID3D12PipelineState::IID,
                    &mut out,
                ),
                4 => {
                    library.LoadComputePipeline(PCWSTR::null(), &compute, &ID3D12PipelineState::IID, &mut out)
                }
                5 => library.LoadPipeline(PCWSTR::null(), &stream, &ID3D12PipelineState::IID, &mut out),
                _ => unreachable!(),
            };
            assert_eq!(result, S_FALSE);
            let suffixes: &[&str] = match route % 3 {
                0 => &["vs.dxbc", "hs.dxbc", "ds.dxbc", "gs.dxbc", "ps.dxbc"],
                1 => &["cs.dxbc"],
                _ => &["as.dxbc", "ms.dxbc"],
            };
            for suffix in suffixes {
                saved(dir, suffix, &code);
            }
        }
        let dxil = b"DXBC raw DXIL library with multiple exports";
        let dxil_library = D3D12_DXIL_LIBRARY_DESC {
            DXILLibrary: D3D12_SHADER_BYTECODE {
                pShaderBytecode: dxil.as_ptr().cast(),
                BytecodeLength: dxil.len(),
            },
            ..Default::default()
        };
        let subobject = D3D12_STATE_SUBOBJECT {
            Type: D3D12_STATE_SUBOBJECT_TYPE_DXIL_LIBRARY,
            pDesc: (&dxil_library as *const D3D12_DXIL_LIBRARY_DESC).cast(),
        };
        let state = D3D12_STATE_OBJECT_DESC {
            Type: D3D12_STATE_OBJECT_TYPE_RAYTRACING_PIPELINE,
            NumSubobjects: 1,
            pSubobjects: &subobject,
        };
        assert_eq!(
            device.CreateStateObject(&state, &ID3D12StateObject::IID, &mut out),
            S_FALSE
        );
        saved(dir, "lib.dxbc", dxil);
        // This library appears only through AddToStateObject, so CreateStateObject cannot mask a missing hook.
        let addition_bytes = b"DXBC distinct runtime-added DXIL library";
        let addition_library = D3D12_DXIL_LIBRARY_DESC {
            DXILLibrary: D3D12_SHADER_BYTECODE {
                pShaderBytecode: addition_bytes.as_ptr().cast(),
                BytecodeLength: addition_bytes.len(),
            },
            ..Default::default()
        };
        let addition_sub = D3D12_STATE_SUBOBJECT {
            Type: D3D12_STATE_SUBOBJECT_TYPE_DXIL_LIBRARY,
            pDesc: (&addition_library as *const D3D12_DXIL_LIBRARY_DESC).cast(),
        };
        let addition = D3D12_STATE_OBJECT_DESC {
            Type: D3D12_STATE_OBJECT_TYPE_RAYTRACING_PIPELINE,
            NumSubobjects: 1,
            pSubobjects: &addition_sub,
        };
        let native_parent = (&mut state_parent as *mut State).cast();
        let mut wrapped_parent = native_parent;
        super::super::wrap_output(S_OK, &ID3D12StateObject::IID, &mut wrapped_parent);
        assert_ne!(native_parent, wrapped_parent);
        let parent = ID3D12StateObject::from_raw(wrapped_parent);
        assert_eq!(
            device.AddToStateObject(&addition, parent.as_raw(), &ID3D12StateObject::IID, &mut out),
            S_FALSE
        );
        saved(dir, "lib.dxbc", addition_bytes);
        assert_eq!(
            addition_sub.pDesc,
            (&addition_library as *const D3D12_DXIL_LIBRARY_DESC).cast()
        );
        drop(parent);
        for _ in 0..2 {
            assert_eq!(
                device.CreatePipelineLibrary(
                    cache.as_ptr().cast(),
                    cache.len(),
                    &ID3D12PipelineLibrary::IID,
                    &mut out
                ),
                S_FALSE
            );
            assert_eq!(pso.GetCachedBlob(&mut out), S_OK);
            assert_eq!(out, (&mut blob as *mut Blob).cast());
            out = core::ptr::null_mut();
        }
        saved(dir, "pipeline-library.bin", &cache);
        saved(dir, "cached-pso.bin", &cache);
        saved(dir, "cached-pso.bin", &blob.bytes);
        let before = std::fs::read_dir(dir).unwrap().count();
        assert_eq!(
            library.Serialize(serialize_output[1..].as_mut_ptr().cast(), 31),
            S_OK
        );
        assert_eq!(serialize_output[0], 0xa5);
        assert_eq!(serialize_output[32], 0xa5);
        assert_eq!(&serialize_output[1..32], &[0x5a; 31]);
        assert_eq!(std::fs::read_dir(dir).unwrap().count(), before);
        assert_eq!(before, 21);
    }
    std::thread::scope(|scope| {
        for _ in 0..8 {
            scope.spawn(|| {
                for _ in 0..16 {
                    for tag in [32766u64, 32767] {
                        let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
                            SizeInBytes: size_of::<u64>(),
                            pPipelineStateSubobjectStream: (&tag as *const u64).cast_mut().cast(),
                        };
                        unsafe {
                            stream(&desc);
                        }
                    }
                }
            });
        }
    });
}
