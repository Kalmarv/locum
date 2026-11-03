//! Shader identity. Every shader the game creates is hashed from its bytecode; the hash is stored on the real shader
//! object (`SetPrivateData`), so a draw can read it back from whatever shader is bound, and nothing goes stale when
//! the game frees a shader and its address is reused. Each distinct bytecode is written once to
//! `<LOCUM_DIR>/shaders/<hash>.<stage>.dxbc`.

use core::ffi::c_void;
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D11::ID3D11DeviceChild;
use windows::core::{GUID, HRESULT, Interface};

use crate::log::{folder, log};

/// The private-data key under which locum stores a shader's hash on the real object.
const HASH_KEY: GUID = GUID::from_u128(0x6c6f_6375_6d00_4a5e_9d1b_7a2f_8c3e_0001);

/// FNV-1a 64 of the bytecode, from the offset basis `1469598103934665603`: the standard basis
/// `14695981039346656037` with its last digit dropped. Earlier captures of these games used this seed, so their
/// hashes and locum's match.
pub fn hash(code: &[u8]) -> u64 {
    let mut h: u64 = 1_469_598_103_934_665_603;
    for &b in code {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Vertex,
    Hull,
    Domain,
    Geometry,
    Pixel,
    Compute,
}

impl Stage {
    /// The stages a draw runs, in pipeline order.
    pub const GRAPHICS: [Stage; 5] = [
        Stage::Vertex,
        Stage::Hull,
        Stage::Domain,
        Stage::Geometry,
        Stage::Pixel,
    ];

    pub fn short(self) -> &'static str {
        match self {
            Stage::Vertex => "vs",
            Stage::Hull => "hs",
            Stage::Domain => "ds",
            Stage::Geometry => "gs",
            Stage::Pixel => "ps",
            Stage::Compute => "cs",
        }
    }
}

/// After a `Create*Shader`: tags the new shader with its hash and writes its bytecode the first time it is seen.
///
/// # Safety
/// `code` must point to `len` readable bytes; `out` is the call's out pointer.
pub unsafe fn created(stage: Stage, code: *const c_void, len: usize, hr: HRESULT, out: *mut *mut c_void) {
    if hr.is_err() || out.is_null() || code.is_null() || unsafe { (*out).is_null() } {
        return;
    }
    let bytes = unsafe { core::slice::from_raw_parts(code.cast::<u8>(), len) };
    let h = hash(bytes);
    if let Some(shader) = unsafe { ID3D11DeviceChild::from_raw_borrowed(&*out) } {
        let _ = unsafe { shader.SetPrivateData(&HASH_KEY, 8, Some((&h as *const u64).cast())) };
    }
    static CREATED: AtomicU64 = AtomicU64::new(0);
    static SEEN: Mutex<Option<HashSet<u64>>> = Mutex::new(None);
    let created = CREATED.fetch_add(1, Ordering::Relaxed) + 1;
    let new = SEEN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(HashSet::new)
        .insert(h);
    if new && let Some(dir) = folder().map(|d| d.join("shaders")) {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(format!("{h:016x}.{}.dxbc", stage.short())), bytes);
    }
    if created.is_power_of_two() || created.is_multiple_of(5000) {
        let distinct = SEEN
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, HashSet::len);
        log!("shaders created: {created}, distinct bytecode: {distinct}");
    }
}

/// The hash locum stored on a shader, if it is one locum saw created.
///
/// # Safety
/// `shader` must be null or a live device child.
pub unsafe fn hash_of(shader: *mut c_void) -> Option<u64> {
    let shader = unsafe { ID3D11DeviceChild::from_raw_borrowed(&shader) }?;
    let mut h = 0u64;
    let mut size = 8u32;
    unsafe { shader.GetPrivateData(&HASH_KEY, &mut size, Some((&mut h as *mut u64).cast())) }.ok()?;
    (size == 8).then_some(h)
}

#[cfg(test)]
mod tests {
    use super::hash;

    #[test]
    fn empty_input_is_the_seed() {
        assert_eq!(hash(&[]), 1_469_598_103_934_665_603);
    }

    #[test]
    fn one_byte() {
        // (seed ^ 0x61) * prime, wrapping
        let expected = (1_469_598_103_934_665_603u64 ^ 0x61).wrapping_mul(0x0000_0100_0000_01b3);
        assert_eq!(hash(b"a"), expected);
    }
}
