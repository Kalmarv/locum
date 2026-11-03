//! The canonical byte layout of a resource's contents, the one blobs and hashes use: every subresource in
//! subresource order (`mip + slice * mips`), each as tightly packed rows of blocks with no padding, depth slices one
//! after another. Row pitches of mapped memory or initial data are removed, so the same texels always give the same
//! bytes, whatever the driver's pitch.

use crate::format;

/// One subresource in the canonical layout.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sub {
    /// The subresource index, `mip + slice * mips`.
    pub index: u32,
    pub mip: u32,
    pub slice: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    /// Bytes per row of blocks, and rows of blocks per depth slice.
    pub row_bytes: u32,
    pub rows: u32,
    /// Where it starts in the resource's contents, and its size.
    pub offset: u64,
    pub size: u64,
}

impl Sub {
    pub fn slice_bytes(&self) -> u64 {
        u64::from(self.row_bytes) * u64::from(self.rows)
    }
}

/// A texture's subresources, or None for a format without a one-plane block layout.
pub fn texture(format: u32, width: u32, height: u32, depth: u32, mips: u32, array: u32) -> Option<Vec<Sub>> {
    let f = format::layout(format)?;
    let mips = mips.max(1);
    let mut out = Vec::with_capacity((mips * array.max(1)) as usize);
    let mut offset = 0u64;
    for slice in 0..array.max(1) {
        for mip in 0..mips {
            let (w, h, d) = (
                (width >> mip).max(1),
                (height >> mip).max(1),
                (depth >> mip).max(1),
            );
            let (row_bytes, rows) = (f.row_bytes(w), f.rows(h));
            let size = u64::from(row_bytes) * u64::from(rows) * u64::from(d);
            out.push(Sub {
                index: mip + slice * mips,
                mip,
                slice,
                width: w,
                height: h,
                depth: d,
                row_bytes,
                rows,
                offset,
                size,
            });
            offset += size;
        }
    }
    Some(out)
}

/// A buffer as one subresource of `bytes` bytes.
pub fn buffer(bytes: u32) -> Vec<Sub> {
    vec![Sub {
        index: 0,
        mip: 0,
        slice: 0,
        width: bytes,
        height: 1,
        depth: 1,
        row_bytes: bytes,
        rows: 1,
        offset: 0,
        size: u64::from(bytes),
    }]
}

/// The total size of a set of subresources.
pub fn total(subs: &[Sub]) -> u64 {
    subs.last().map_or(0, |s| s.offset + s.size)
}

/// Copies one subresource out of pitched memory (`row_pitch` bytes between rows, `depth_pitch` between depth
/// slices) into `out`, tightly packed.
///
/// # Safety
/// `src` must point to the whole subresource in that pitched layout.
pub unsafe fn pack(sub: &Sub, src: *const u8, row_pitch: usize, depth_pitch: usize, out: &mut Vec<u8>) {
    let row = sub.row_bytes as usize;
    for z in 0..sub.depth as usize {
        for y in 0..sub.rows as usize {
            let line = unsafe { core::slice::from_raw_parts(src.add(z * depth_pitch + y * row_pitch), row) };
            out.extend_from_slice(line);
        }
    }
}

/// Copies one tightly packed subresource into pitched memory.
///
/// # Safety
/// `dst` must point to room for the whole subresource in that pitched layout; `src` must hold `sub.size` bytes.
pub unsafe fn unpack(sub: &Sub, src: &[u8], dst: *mut u8, row_pitch: usize, depth_pitch: usize) {
    let row = sub.row_bytes as usize;
    let rows = sub.rows as usize;
    for z in 0..sub.depth as usize {
        for y in 0..rows {
            let from = &src[(z * rows + y) * row..][..row];
            unsafe {
                core::ptr::copy_nonoverlapping(from.as_ptr(), dst.add(z * depth_pitch + y * row_pitch), row)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mip_chain() {
        // 4x2 RGBA8, 3 mips, 2 slices: mips 4x2, 2x1, 1x1.
        let subs = texture(28, 4, 2, 1, 3, 2).unwrap();
        assert_eq!(subs.len(), 6);
        assert_eq!(
            subs.iter().map(|s| s.size).collect::<Vec<_>>(),
            [32, 8, 4, 32, 8, 4]
        );
        assert_eq!(subs[3].index, 3);
        assert_eq!(subs[3].slice, 1);
        assert_eq!(subs[3].offset, 44);
        assert_eq!(total(&subs), 88);
    }

    #[test]
    fn volume() {
        let subs = texture(28, 4, 4, 4, 2, 1).unwrap();
        assert_eq!(subs[0].size, 4 * 4 * 4 * 4);
        assert_eq!((subs[1].width, subs[1].depth), (2, 2));
    }

    #[test]
    fn pack_removes_pitch() {
        let sub = texture(28, 2, 2, 1, 1, 1).unwrap()[0];
        let mut pitched = vec![0u8; 32];
        pitched[..8].copy_from_slice(&[1; 8]);
        pitched[16..24].copy_from_slice(&[2; 8]);
        let mut out = Vec::new();
        unsafe { pack(&sub, pitched.as_ptr(), 16, 32, &mut out) };
        assert_eq!(out, [[1u8; 8], [2u8; 8]].concat());
        let mut back = vec![0u8; 32];
        unsafe { unpack(&sub, &out, back.as_mut_ptr(), 16, 32) };
        assert_eq!(back, pitched);
    }
}
