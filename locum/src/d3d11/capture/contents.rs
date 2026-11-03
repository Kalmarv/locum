//! Reading resource contents back for a capture (the copy and the canonical layout are in
//! [`locum_d3d11::readback`]): each read is hashed, whole and per subresource, and handed to the blob store.

use std::collections::BTreeMap;

use locum_capture::hash::key;
use locum_capture::layout;
use locum_capture::writer;
use locum_d3d11::readback;
pub use locum_d3d11::readback::Readback;
use windows::Win32::Graphics::Direct3D11::*;

use crate::d3d11::registry::Dims;
use crate::settings::settings;

/// Bytes of staging copies the start of a capture has in flight at once.
const BATCH: u64 = 512 << 20;

/// Records a copy of `resource` into a new staging resource on `c`.
///
/// # Safety
/// `c` must be a context of the device that made `resource`.
pub unsafe fn issue(c: &ID3D11DeviceContext4, resource: &ID3D11Resource) -> Result<Readback, String> {
    unsafe { readback::issue(c, resource) }
}

/// The key of a copy's bytes in the canonical layout, and each subresource's key. Waits for the GPU. The bytes go to
/// the blob store.
///
/// # Safety
/// `immediate` must be the immediate context of the device that made the copy.
pub unsafe fn read(immediate: &ID3D11DeviceContext4, rb: &Readback) -> Result<(String, Vec<String>), String> {
    let bytes = unsafe { rb.read(immediate) }?;
    let hashes = rb
        .subs
        .iter()
        .map(|s| key(&bytes[s.offset as usize..(s.offset + s.size) as usize]))
        .collect();
    let k = key(&bytes);
    let blobs = &settings().blobs;
    if !blobs.has(&k) {
        writer::blob(blobs, k.clone(), bytes, true);
    }
    Ok((k, hashes))
}

/// Copies and reads `todo` on the immediate context now, a batch at a time.
///
/// # Safety
/// `immediate` must be the immediate context of the device that made the resources.
pub unsafe fn read_now(
    immediate: &ID3D11DeviceContext4,
    todo: Vec<(u64, ID3D11Resource, Dims)>,
) -> BTreeMap<u64, Result<(String, Vec<String>), String>> {
    let mut out = BTreeMap::new();
    let mut batch: Vec<(u64, Result<Readback, String>)> = Vec::new();
    let mut batch_bytes = 0u64;
    let flush = |batch: &mut Vec<(u64, Result<Readback, String>)>,
                 out: &mut BTreeMap<u64, Result<(String, Vec<String>), String>>| {
        for (id, rb) in batch.drain(..) {
            out.insert(id, rb.and_then(|rb| unsafe { read(immediate, &rb) }));
        }
    };
    for (id, resource, dims) in todo {
        let size = dims.subs().map_or(0, |s| layout::total(&s));
        batch.push((id, unsafe { issue(immediate, &resource) }));
        batch_bytes += size;
        if batch_bytes >= BATCH {
            flush(&mut batch, &mut out);
            batch_bytes = 0;
        }
    }
    flush(&mut batch, &mut out);
    out
}
