//! Reading resource contents back for a capture (the copy and the canonical layout are in
//! [`locum_d3d11::readback`]): each read is hashed, whole and per subresource, and handed to the blob store.

use locum_capture::hash::key;
use locum_capture::writer;
use locum_d3d11::readback;
pub use locum_d3d11::readback::Readback;
use windows::Win32::Graphics::Direct3D11::*;

use crate::settings::settings;

/// Bytes of staging copies a capture has in flight before it reads them (waiting for the GPU).
pub const IN_FLIGHT: u64 = 512 << 20;

/// Contents read back: the key of their bytes in the canonical layout and each subresource's key, or why there are
/// none.
pub type Read = Result<(String, Vec<String>), String>;

/// Records a copy of `resource` into a new staging resource on `c`.
///
/// # Safety
/// `c` must be a context of the device that made `resource`.
pub unsafe fn issue(c: &ID3D11DeviceContext4, resource: &ID3D11Resource) -> Result<Readback, String> {
    if super::tiled::special(resource) {
        return Err("tiled resource/pool requires mapped-tile readback; whole-resource copies can touch unmapped tiles".into());
    }
    unsafe { readback::issue(c, resource) }
}

/// The key of a copy's bytes in the canonical layout, and each subresource's key. Waits for the GPU. The bytes go to
/// the blob store.
///
/// # Safety
/// `immediate` must be the immediate context of the device that made the copy.
pub unsafe fn read(immediate: &ID3D11DeviceContext4, rb: &Readback) -> Read {
    let bytes = unsafe { rb.read(immediate) }?;
    let hashes = locum_capture::layout::subresources(&rb.subs)
        .iter()
        .map(|s| key(&bytes[s.offset as usize..(s.offset + s.size) as usize]))
        .collect();
    let k = key(&bytes);
    let blobs = &settings().blobs;
    writer::blob(blobs, k.clone(), bytes, true);
    Ok((k, hashes))
}

/// Reads `copies` (or keeps why they could not be made).
///
/// # Safety
/// `immediate` must be the immediate context of the device that made the copies.
pub unsafe fn read_all(
    immediate: &ID3D11DeviceContext4,
    copies: Vec<(u64, Result<Readback, String>)>,
) -> Vec<(u64, Read)> {
    copies
        .into_iter()
        .map(|(id, rb)| (id, rb.and_then(|rb| unsafe { read(immediate, &rb) })))
        .collect()
}
