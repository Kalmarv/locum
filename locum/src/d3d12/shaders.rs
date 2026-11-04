//! Journal-independent archive of API-observed shader bytes and opaque driver-cache inputs.
//! Content is never filtered by reflection. Repeat observations only hash and check the in-memory index.
use core::ffi::c_void;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use windows::Win32::Graphics::{Direct3D::ID3DBlob, Direct3D12::*};
use windows::core::Interface;

#[cfg(test)]
mod tests;

#[derive(Default)]
struct Archive {
    seen: Mutex<HashSet<(String, &'static str)>>,
}
impl Archive {
    fn save(&self, dir: &Path, suffix: &'static str, bytes: &[u8]) -> std::io::Result<bool> {
        if bytes.is_empty() {
            return Ok(false);
        }
        let hash = locum_capture::hash::key(bytes);
        let key = (hash.clone(), suffix);
        if !self
            .seen
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.clone())
        {
            return Ok(false);
        }
        let path = dir.join(format!("{hash}.{suffix}"));
        // A first observation writes synchronously from the borrowed bytes, avoiding an unbounded queue/copy
        // of potentially hundred-MB caches. Atomic publication prevents crashes leaving a partial final file.
        let result = (|| {
            std::fs::create_dir_all(dir)?;
            if std::fs::metadata(&path).is_ok_and(|m| m.len() == bytes.len() as u64) {
                return Ok(());
            }
            let temporary = path.with_extension(format!("{}.part", std::process::id()));
            std::fs::write(&temporary, bytes)?;
            std::fs::rename(&temporary, &path)
        })();
        if let Err(error) = result {
            self.seen.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
            return Err(error);
        }
        if suffix.ends_with(".bin") {
            crate::log::log!(
                "D3D12 first-seen {suffix}: hash={hash} bytes={} path={}",
                bytes.len(),
                path.display()
            );
        }
        Ok(true)
    }
}

fn archive() -> &'static Archive {
    static ARCHIVE: OnceLock<Archive> = OnceLock::new();
    ARCHIVE.get_or_init(Archive::default)
}
fn directory() -> Option<&'static Path> {
    static DIRECTORY: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIRECTORY
        .get_or_init(|| crate::log::folder().map(|p| p.join("shaders")))
        .as_deref()
}
pub unsafe fn bytes(suffix: &'static str, pointer: *const c_void, len: usize) {
    if pointer.is_null() || len == 0 {
        return;
    }
    let Some(dir) = directory() else {
        return;
    };
    let bytes = unsafe { core::slice::from_raw_parts(pointer.cast::<u8>(), len) };
    if let Err(error) = archive().save(dir, suffix, bytes) {
        crate::log::log!("D3D12 shader/cache archive failed ({suffix}, {len} bytes): {error}");
    }
}
unsafe fn shader(stage: &'static str, code: &D3D12_SHADER_BYTECODE) {
    unsafe { bytes(stage, code.pShaderBytecode, code.BytecodeLength) };
}
pub unsafe fn graphics(p: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC) {
    let Some(d) = (unsafe { p.as_ref() }) else {
        return;
    };
    for (stage, code) in [
        ("vs.dxbc", &d.VS),
        ("hs.dxbc", &d.HS),
        ("ds.dxbc", &d.DS),
        ("gs.dxbc", &d.GS),
        ("ps.dxbc", &d.PS),
    ] {
        unsafe { shader(stage, code) };
    }
    unsafe {
        bytes(
            "cached-pso.bin",
            d.CachedPSO.pCachedBlob,
            d.CachedPSO.CachedBlobSizeInBytes,
        )
    };
}
pub unsafe fn compute(p: *const D3D12_COMPUTE_PIPELINE_STATE_DESC) {
    let Some(d) = (unsafe { p.as_ref() }) else {
        return;
    };
    unsafe {
        shader("cs.dxbc", &d.CS);
        bytes(
            "cached-pso.bin",
            d.CachedPSO.pCachedBlob,
            d.CachedPSO.CachedBlobSizeInBytes,
        );
    }
}
pub unsafe fn stream(p: *const D3D12_PIPELINE_STATE_STREAM_DESC) {
    let Some(d) = (unsafe { p.as_ref() }) else {
        return;
    };
    if d.pPipelineStateSubobjectStream.is_null() {
        return;
    }
    let data =
        unsafe { core::slice::from_raw_parts(d.pPipelineStateSubobjectStream.cast::<u8>(), d.SizeInBytes) };
    let mut at = 0usize;
    while let Some(tag) = data.get(at..).and_then(|b| b.get(..4)) {
        let ty = D3D12_PIPELINE_STATE_SUBOBJECT_TYPE(i32::from_le_bytes(tag.try_into().unwrap()));
        let Some((size, alignment)) = super::inputs::payload(ty) else {
            static REPORTED: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
            let first = REPORTED
                .get_or_init(Mutex::default)
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(ty.0);
            if first {
                crate::log::log!(
                    "D3D12 shader archive: PSO stream type {} at byte {at} has no ABI layout",
                    ty.0
                );
            }
            return;
        };
        let Some(offset) = at.checked_add(4usize.next_multiple_of(alignment)) else {
            return;
        };
        let Some(end) = offset.checked_add(size).filter(|&end| end <= data.len()) else {
            return;
        };
        let pointer = unsafe { data.as_ptr().add(offset) };
        let stage = match ty {
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS => Some("vs.dxbc"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_HS => Some("hs.dxbc"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DS => Some("ds.dxbc"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS => Some("gs.dxbc"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS => Some("ps.dxbc"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS => Some("cs.dxbc"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS => Some("as.dxbc"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS => Some("ms.dxbc"),
            _ => None,
        };
        if let Some(stage) = stage {
            unsafe { shader(stage, &pointer.cast::<D3D12_SHADER_BYTECODE>().read_unaligned()) };
        } else if ty == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CACHED_PSO {
            let cache = unsafe { pointer.cast::<D3D12_CACHED_PIPELINE_STATE>().read_unaligned() };
            unsafe { bytes("cached-pso.bin", cache.pCachedBlob, cache.CachedBlobSizeInBytes) };
        }
        let Some(next) = end.checked_next_multiple_of(size_of::<usize>()) else {
            return;
        };
        at = next;
    }
}
pub unsafe fn state_object(p: *const D3D12_STATE_OBJECT_DESC) {
    let Some(d) = (unsafe { p.as_ref() }) else {
        return;
    };
    if d.pSubobjects.is_null() {
        return;
    }
    for s in unsafe { core::slice::from_raw_parts(d.pSubobjects, d.NumSubobjects as usize) } {
        if s.Type == D3D12_STATE_SUBOBJECT_TYPE_DXIL_LIBRARY
            && let Some(library) = unsafe { s.pDesc.cast::<D3D12_DXIL_LIBRARY_DESC>().as_ref() }
        {
            unsafe { shader("lib.dxbc", &library.DXILLibrary) };
        }
    }
}
pub unsafe fn cached_blob(out: *mut *mut c_void) {
    if out.is_null() {
        return;
    }
    if let Some(blob) = unsafe { ID3DBlob::from_raw_borrowed(&*out) } {
        unsafe { bytes("cached-pso.bin", blob.GetBufferPointer(), blob.GetBufferSize()) };
    }
}
