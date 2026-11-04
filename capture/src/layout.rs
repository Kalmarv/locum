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

/// Joins one subresource's depth and stencil planes, as Direct3D 12 copies them, into the canonical texel of
/// Direct3D 11's format: for the D24S8 family (4-byte texels) depth in the low 24 bits and stencil in the top byte;
/// for the D32S8X24 family (8-byte texels) the 32-bit depth, then the stencil byte and three bytes of zero.
/// `depth` holds 4 bytes a texel and `stencil` 1, both tightly packed.
pub fn join_depth_stencil(texel_bytes: u32, depth: &[u8], stencil: &[u8], out: &mut Vec<u8>) {
    for (d, s) in depth.as_chunks::<4>().0.iter().zip(stencil) {
        if texel_bytes == 8 {
            out.extend_from_slice(d);
            out.extend_from_slice(&[*s, 0, 0, 0]);
        } else {
            out.extend_from_slice(&d[..3]);
            out.push(*s);
        }
    }
}

/// Splits canonical texels of the D24S8 family (4 bytes) or the D32S8X24 family (8 bytes) into the depth plane (4
/// bytes a texel) and stencil plane (1 byte) Direct3D 12 copies them as: the inverse of [`join_depth_stencil`]. The
/// D24S8 depth plane's top byte is zero.
pub fn split_depth_stencil(texel_bytes: u32, texels: &[u8], depth: &mut Vec<u8>, stencil: &mut Vec<u8>) {
    for t in texels.chunks_exact(texel_bytes as usize) {
        if texel_bytes == 8 {
            depth.extend_from_slice(&t[..4]);
            stencil.push(t[4]);
        } else {
            depth.extend_from_slice(&[t[0], t[1], t[2], 0]);
            stencil.push(t[3]);
        }
    }
}

/// Where a copy between a texture and a buffer puts one plane of one subresource in the buffer: Direct3D 12's placed
/// footprint, with the rows and bytes a row `GetCopyableFootprints` gives for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Footprint {
    pub offset: u64,
    pub row_pitch: u32,
    pub depth: u32,
    pub rows: u32,
    pub row_bytes: u64,
}

impl Footprint {
    fn at(&self, z: usize, y: usize) -> usize {
        self.offset as usize + (z * self.rows as usize + y) * self.row_pitch as usize
    }
}

/// Subresources' bytes in the canonical layout `layout`, from a buffer `data` they were copied into at
/// `footprints`: every subresource of plane 0, then, for a depth-stencil format whose canonical texel is `joined`
/// bytes, every one of plane 1. No footprints means a buffer, whose bytes are its contents.
pub fn from_footprints(
    layout: &[Sub],
    joined: Option<u32>,
    footprints: &[Footprint],
    data: &[u8],
) -> Result<Vec<u8>, String> {
    let total = total(layout);
    if footprints.is_empty() {
        return Ok(data[..(total as usize).min(data.len())].to_vec());
    }
    // One plane of one subresource, tightly packed.
    let plane = |index: usize| -> Result<(Vec<u8>, u64), String> {
        let f = footprints
            .get(index)
            .ok_or(format!("no footprint for subresource {index}"))?;
        let mut out = Vec::with_capacity(f.row_bytes as usize * f.rows as usize * f.depth as usize);
        for z in 0..f.depth as usize {
            for y in 0..f.rows as usize {
                let at = f.at(z, y);
                out.extend_from_slice(
                    data.get(at..at + f.row_bytes as usize)
                        .ok_or(format!("subresource {index} lies past the copy's {} bytes", data.len()))?,
                );
            }
        }
        Ok((out, f.row_bytes))
    };
    let mut out = Vec::with_capacity(total as usize);
    for (i, sub) in layout.iter().enumerate() {
        let (first, row_bytes) = plane(i)?;
        match joined {
            Some(texel) => {
                let (stencil, _) = plane(i + layout.len())?;
                join_depth_stencil(texel, &first, &stencil, &mut out);
            }
            None => {
                if row_bytes != u64::from(sub.row_bytes) || first.len() as u64 != sub.size {
                    return Err(format!(
                        "subresource {i}: the copy's {row_bytes}-byte rows ({} bytes) differ from the canonical layout's {} ({} bytes)",
                        first.len(),
                        sub.row_bytes,
                        sub.size
                    ));
                }
                out.extend_from_slice(&first);
            }
        }
    }
    if out.len() as u64 != total {
        return Err(format!(
            "{} bytes copied for a {total}-byte canonical layout",
            out.len()
        ));
    }
    Ok(out)
}

/// Writes canonical `contents` into a buffer `out` at `footprints`, ready to be copied into the texture: the inverse
/// of [`from_footprints`].
pub fn into_footprints(
    layout: &[Sub],
    joined: Option<u32>,
    footprints: &[Footprint],
    contents: &[u8],
    out: &mut [u8],
) -> Result<(), String> {
    if contents.len() as u64 != total(layout) {
        return Err(format!(
            "{} bytes of contents for a {}-byte canonical layout",
            contents.len(),
            total(layout)
        ));
    }
    let size = out.len();
    let mut put = |index: usize, plane: &[u8]| -> Result<(), String> {
        let f = footprints
            .get(index)
            .ok_or(format!("no footprint for subresource {index}"))?;
        let row = f.row_bytes as usize;
        if plane.len() != row * f.rows as usize * f.depth as usize {
            return Err(format!(
                "subresource {index}: {} bytes for {} rows of {row} bytes",
                plane.len(),
                f.rows as usize * f.depth as usize
            ));
        }
        for (k, line) in plane.chunks_exact(row.max(1)).enumerate() {
            let at = f.at(k / f.rows as usize, k % f.rows as usize);
            out.get_mut(at..at + row)
                .ok_or(format!("subresource {index} lies past the {size}-byte buffer"))?
                .copy_from_slice(line);
        }
        Ok(())
    };
    for (i, sub) in layout.iter().enumerate() {
        let bytes = &contents[sub.offset as usize..(sub.offset + sub.size) as usize];
        match joined {
            Some(texel) => {
                let (mut depth, mut stencil) = (Vec::new(), Vec::new());
                split_depth_stencil(texel, bytes, &mut depth, &mut stencil);
                put(i, &depth)?;
                put(i + layout.len(), &stencil)?;
            }
            None => put(i, bytes)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn footprints_lose_their_row_padding_and_depth_stencil_planes_join() {
        let footprint = |offset: u64, rows: u32, row_bytes: u64| Footprint {
            offset,
            row_pitch: 256,
            depth: 1,
            rows,
            row_bytes,
        };
        // A 2x2 RGBA8 texture copied with 256-byte rows.
        let mut data = vec![0u8; 512];
        data[..8].copy_from_slice(&[1; 8]);
        data[256..264].copy_from_slice(&[2; 8]);
        let layout = texture(28, 2, 2, 1, 1, 1).unwrap();
        let out = from_footprints(&layout, None, &[footprint(0, 2, 8)], &data).unwrap();
        assert_eq!(out, [[1u8; 8], [2u8; 8]].concat());
        let mut back = vec![0u8; 512];
        into_footprints(&layout, None, &[footprint(0, 2, 8)], &out, &mut back).unwrap();
        assert_eq!(back, data);
        // A 1x1 D24S8 texture: depth plane then stencil plane.
        let mut data = vec![0u8; 1024];
        data[..4].copy_from_slice(&[0x11, 0x22, 0x33, 0]);
        data[512] = 0x7F;
        let layout = texture(45, 1, 1, 1, 1, 1).unwrap();
        let planes = [footprint(0, 1, 4), footprint(512, 1, 1)];
        let out = from_footprints(&layout, Some(4), &planes, &data).unwrap();
        assert_eq!(out, [0x11, 0x22, 0x33, 0x7F]);
        let mut back = vec![0u8; 1024];
        into_footprints(&layout, Some(4), &planes, &out, &mut back).unwrap();
        assert_eq!(back, data);
    }

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
    fn depth_and_stencil_planes_join_into_direct3d_11_texels() {
        let depth = [0x11, 0x22, 0x33, 0xEE, 0x44, 0x55, 0x66, 0xEE];
        let mut out = Vec::new();
        join_depth_stencil(4, &depth, &[0xA1, 0xA2], &mut out);
        assert_eq!(out, [0x11, 0x22, 0x33, 0xA1, 0x44, 0x55, 0x66, 0xA2]);
        out.clear();
        join_depth_stencil(8, &depth[..4], &[0xA1], &mut out);
        assert_eq!(out, [0x11, 0x22, 0x33, 0xEE, 0xA1, 0, 0, 0]);
        let (mut d, mut s) = (Vec::new(), Vec::new());
        split_depth_stencil(8, &out, &mut d, &mut s);
        assert_eq!((d.as_slice(), s.as_slice()), (&depth[..4], &[0xA1][..]));
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
