//! Hashes. Blobs and contents are named by XXH3-128 (32 hex digits); shaders keep the FNV-1a 64 name earlier
//! captures of these games used.

use xxhash_rust::xxh3::{Xxh3, xxh3_128};

/// The blob key of `bytes`: XXH3-128 as 32 lowercase hex digits.
pub fn key(bytes: &[u8]) -> String {
    format!("{:032x}", xxh3_128(bytes))
}

/// A key built from several pieces, the same as [`key`] of their concatenation.
#[derive(Default)]
pub struct KeyHasher(Xxh3);

impl KeyHasher {
    pub fn new() -> KeyHasher {
        KeyHasher(Xxh3::new())
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub fn finish(&self) -> String {
        format!("{:032x}", self.0.digest128())
    }
}

/// FNV-1a 64 of shader bytecode, from the offset basis `1469598103934665603` (the standard basis with its last digit
/// dropped). Earlier captures of these games used this seed, so their hashes and locum's match.
pub fn shader(code: &[u8]) -> u64 {
    let mut h: u64 = 1_469_598_103_934_665_603;
    for &b in code {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// A shader hash as written in captures: 16 lowercase hex digits.
pub fn shader_name(hash: u64) -> String {
    format!("{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pieces_match_the_whole() {
        let mut h = KeyHasher::new();
        h.update(b"hello ");
        h.update(b"world");
        assert_eq!(h.finish(), key(b"hello world"));
        assert_eq!(key(b"").len(), 32);
    }

    #[test]
    fn shader_hash_seed() {
        assert_eq!(shader(&[]), 1_469_598_103_934_665_603);
        let expected = (1_469_598_103_934_665_603u64 ^ 0x61).wrapping_mul(0x0000_0100_0000_01b3);
        assert_eq!(shader(b"a"), expected);
    }
}
