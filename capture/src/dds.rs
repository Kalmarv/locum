//! DDS files of resource contents. The canonical layout (each array slice's mips in order, rows tight) is the order
//! DDS stores them in, so a file is a header and the bytes as they are.

/// What a DDS file holds.
#[derive(Clone, Copy, Debug, Default)]
pub struct Texture {
    pub format: u32,
    /// 1, 2 or 3.
    pub dimensions: u8,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mips: u32,
    /// Array slices; for a cube, six per cube.
    pub array: u32,
    pub cube: bool,
}

/// The header, with the DX10 extension (148 bytes), for `t` whose mip 0 rows are `row_bytes` long.
pub fn header(t: &Texture, row_bytes: u32, compressed: bool) -> Vec<u8> {
    const CAPS: u32 = 0x1;
    const HEIGHT: u32 = 0x2;
    const WIDTH: u32 = 0x4;
    const PITCH: u32 = 0x8;
    const PIXELFORMAT: u32 = 0x1000;
    const MIPMAPCOUNT: u32 = 0x2_0000;
    const LINEARSIZE: u32 = 0x8_0000;
    const DEPTH: u32 = 0x80_0000;
    const FOURCC: u32 = 0x4;
    const COMPLEX: u32 = 0x8;
    const TEXTURE: u32 = 0x1000;
    const MIPMAP: u32 = 0x40_0000;
    const CUBEMAP_ALL: u32 = 0xfe00;
    const VOLUME: u32 = 0x20_0000;
    let mut flags = CAPS | HEIGHT | WIDTH | PIXELFORMAT | MIPMAPCOUNT;
    flags |= if compressed { LINEARSIZE } else { PITCH };
    if t.dimensions == 3 {
        flags |= DEPTH;
    }
    let pitch = if compressed {
        row_bytes * t.height.div_ceil(4)
    } else {
        row_bytes
    };
    let mut caps = TEXTURE;
    if t.mips > 1 || t.array > 1 || t.cube || t.dimensions == 3 {
        caps |= COMPLEX;
    }
    if t.mips > 1 {
        caps |= MIPMAP;
    }
    let caps2 = if t.cube {
        CUBEMAP_ALL
    } else if t.dimensions == 3 {
        VOLUME
    } else {
        0
    };
    let mut words = vec![
        124,
        flags,
        t.height.max(1),
        t.width.max(1),
        pitch,
        if t.dimensions == 3 { t.depth } else { 0 },
        t.mips.max(1),
    ];
    words.extend([0; 11]);
    words.extend([32, FOURCC, u32::from_le_bytes(*b"DX10"), 0, 0, 0, 0, 0]);
    words.extend([caps, caps2, 0, 0, 0]);
    // DX10 header: format, dimension (2 1D, 3 2D, 4 3D), misc (4: cube), array size (cubes count six faces as one),
    // alpha mode unknown.
    let array = if t.cube { t.array / 6 } else { t.array };
    words.extend([
        t.format,
        u32::from(t.dimensions) + 1,
        if t.cube { 4 } else { 0 },
        array.max(1),
        0,
    ]);
    let mut out = Vec::with_capacity(148);
    out.extend_from_slice(b"DDS ");
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

/// A DDS file of `t` holding `bytes` (the canonical layout of all its subresources, or of one subresource when `t`
/// says one mip and one slice).
pub fn file(t: &Texture, bytes: &[u8]) -> Vec<u8> {
    let layout = crate::format::layout(t.format);
    let row_bytes = layout.map_or(0, |l| l.row_bytes(t.width));
    let mut out = header(t, row_bytes, crate::format::is_compressed(t.format));
    out.extend_from_slice(bytes);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn header_fields() {
        let t = Texture {
            format: 87,
            dimensions: 2,
            width: 64,
            height: 32,
            depth: 1,
            mips: 1,
            array: 1,
            cube: false,
        };
        let h = header(&t, 256, false);
        assert_eq!(h.len(), 148);
        assert_eq!(&h[..4], b"DDS ");
        assert_eq!(u32_at(&h, 12), 32);
        assert_eq!(u32_at(&h, 16), 64);
        assert_eq!(u32_at(&h, 20), 256);
        assert_eq!(&h[84..88], b"DX10");
        assert_eq!(u32_at(&h, 128), 87);
        assert_eq!(u32_at(&h, 132), 3);
    }

    #[test]
    fn cube_counts_faces_as_one() {
        let t = Texture {
            format: 71,
            dimensions: 2,
            width: 16,
            height: 16,
            depth: 1,
            mips: 5,
            array: 6,
            cube: true,
        };
        let h = header(&t, 32, true);
        assert_eq!(u32_at(&h, 20), 32 * 4);
        assert_eq!(u32_at(&h, 28), 5);
        assert_eq!(u32_at(&h, 136), 4);
        assert_eq!(u32_at(&h, 140), 1);
    }
}
