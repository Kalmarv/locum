//! DDS files of resource contents. The canonical layout (each array slice's mips in order, rows tight) is the order
//! DDS stores them in, so a file is a header and the bytes as they are, both ways: [`file`] writes one, [`read`]
//! takes one apart (the DX10 header, and the older pixel formats named the way DirectXTex names them).

use crate::layout;

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

/// The DXGI format of an older DDS pixel format (no DX10 header): `flags`, `fourcc`, `bits` and the four masks.
fn legacy_format(flags: u32, fourcc: u32, bits: u32, masks: [u32; 4]) -> Option<u32> {
    const ALPHA: u32 = 0x2;
    const FOURCC: u32 = 0x4;
    const RGB: u32 = 0x40;
    const LUMINANCE: u32 = 0x2_0000;
    const ALPHA_ONLY: u32 = 0x2;
    if flags & FOURCC != 0 {
        return Some(match &fourcc.to_le_bytes() {
            b"DXT1" => 71,
            b"DXT2" | b"DXT3" => 74,
            b"DXT4" | b"DXT5" => 77,
            b"ATI1" | b"BC4U" => 80,
            b"BC4S" => 81,
            b"ATI2" | b"BC5U" => 83,
            b"BC5S" => 84,
            // D3DFORMAT numbers.
            _ => match fourcc {
                36 => 11,
                110 => 13,
                111 => 54,
                112 => 34,
                113 => 10,
                114 => 41,
                115 => 16,
                116 => 2,
                _ => return None,
            },
        });
    }
    let has_alpha = flags & ALPHA != 0;
    let [r, g, b, a] = masks;
    if flags & RGB != 0 {
        return match (bits, r, g, b, a) {
            (32, 0xff_0000, 0xff00, 0xff, 0xff00_0000) => Some(87),
            (32, 0xff_0000, 0xff00, 0xff, _) => Some(88),
            (32, 0xff, 0xff00, 0xff_0000, 0xff00_0000) => Some(28),
            (32, 0xff, 0xff00, 0xff_0000, 0) if !has_alpha => Some(28),
            (32, 0xffff, 0xffff_0000, 0, _) => Some(35),
            (32, 0x3ff, 0xf_fc00, 0x3ff0_0000, _) => Some(24),
            (32, 0xffff_ffff, 0, 0, 0) => Some(41),
            (16, 0x7c00, 0x3e0, 0x1f, 0x8000) => Some(86),
            (16, 0xf800, 0x7e0, 0x1f, 0) => Some(85),
            (16, 0xf00, 0xf0, 0xf, 0xf000) => Some(115),
            (8, 0xff, 0, 0, 0) => Some(61),
            _ => None,
        };
    }
    if flags & LUMINANCE != 0 {
        return match (bits, r, a) {
            (8, 0xff, _) => Some(61),
            (16, 0xffff, _) => Some(56),
            (16, 0xff, 0xff00) => Some(49),
            _ => None,
        };
    }
    if flags & ALPHA_ONLY != 0 && bits == 8 {
        return Some(65);
    }
    None
}

/// Whether `payload` holds every subresource of `t`.
fn holds(t: &Texture, payload: &[u8]) -> bool {
    layout::texture(t.format, t.width, t.height, t.depth, t.mips, t.array)
        .is_some_and(|subs| layout::total(&subs) <= payload.len() as u64)
}

/// What a DDS file holds and its bytes after the header(s), which are the canonical layout of every subresource
/// (and possibly more).
pub fn read(bytes: &[u8]) -> Result<(Texture, &[u8]), String> {
    let word = |at: usize| -> Result<u32, String> {
        bytes
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("four bytes")))
            .ok_or_else(|| format!("DDS: ends at {} before the header does", bytes.len()))
    };
    if bytes.get(..4) != Some(b"DDS ") || word(4)? != 124 {
        return Err("not a DDS file".into());
    }
    const DEPTH: u32 = 0x80_0000;
    const CUBEMAP: u32 = 0x200;
    const VOLUME: u32 = 0x20_0000;
    let flags = word(8)?;
    let (height, width) = (word(12)?, word(16)?);
    let depth = if flags & DEPTH != 0 { word(24)?.max(1) } else { 1 };
    let mips = word(28)?.max(1);
    let (pf_flags, fourcc, bits) = (word(80)?, word(84)?, word(88)?);
    let masks = [word(92)?, word(96)?, word(100)?, word(104)?];
    let caps2 = word(112)?;
    if fourcc == u32::from_le_bytes(*b"DX10") && pf_flags & 0x4 != 0 {
        let (format, dimension, misc, array) = (word(128)?, word(132)?, word(136)?, word(140)?);
        let cube = misc & 0x4 != 0;
        let dimensions = match dimension {
            2 => 1,
            3 => 2,
            4 => 3,
            d => return Err(format!("DDS: resource dimension {d}")),
        };
        let mut t = Texture {
            format,
            dimensions,
            width,
            height: if dimensions == 1 { 1 } else { height },
            depth: if dimensions == 3 { depth } else { 1 },
            mips,
            array: array.max(1) * if cube { 6 } else { 1 },
            cube,
        };
        let payload = &bytes[148..];
        // The array size of a cube counts cubes, but some writers (FromSoftware's, for one) count its faces there: a
        // file whose bytes hold that many faces and not six times as many is read that way.
        let faces = Texture {
            array: array.max(1),
            ..t
        };
        if cube && array % 6 == 0 && !holds(&t, payload) && holds(&faces, payload) {
            t = faces;
        }
        return Ok((t, payload));
    }
    let format = legacy_format(pf_flags, fourcc, bits, masks).ok_or_else(|| {
        format!(
            "DDS: pixel format without a DXGI format (flags {pf_flags:#x}, fourcc {fourcc:#x}, {bits} bits, masks {masks:x?})"
        )
    })?;
    let cube = caps2 & CUBEMAP != 0;
    let volume = caps2 & VOLUME != 0 || depth > 1;
    let t = Texture {
        format,
        dimensions: if volume { 3 } else { 2 },
        width,
        height,
        depth: if volume { depth } else { 1 },
        mips,
        // An older cube map holds the faces its caps name; files in use hold all six.
        array: if cube { 6 } else { 1 },
        cube,
    };
    Ok((t, &bytes[128..]))
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
    fn a_cube_whose_array_size_counts_faces_reads_as_its_bytes_say() {
        let one = Texture {
            format: 71,
            dimensions: 2,
            width: 8,
            height: 8,
            depth: 1,
            mips: 1,
            array: 6,
            cube: true,
        };
        // Eight 4x4 blocks of 8 bytes would be 2x2 blocks: 32 bytes a face.
        let mut file = file(&one, &[7u8; 6 * 32]);
        // As FromSoftware writes them: the faces counted in the array size.
        file[140..144].copy_from_slice(&6u32.to_le_bytes());
        let (t, _) = read(&file).unwrap();
        assert_eq!((t.array, t.cube), (6, true));
        // Six cubes, with all their bytes, stay six cubes.
        let six = Texture { array: 36, ..one };
        let (t, _) = read(&super::file(&six, &[7u8; 36 * 32])).unwrap();
        assert_eq!(t.array, 36);
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
    #[test]
    fn a_written_file_reads_back() {
        let t = Texture {
            format: 71,
            dimensions: 2,
            width: 16,
            height: 8,
            depth: 1,
            mips: 3,
            array: 6,
            cube: true,
        };
        let payload: Vec<u8> = (0..200u8).collect();
        let f = file(&t, &payload);
        let (r, bytes) = read(&f).unwrap();
        assert_eq!(
            (r.format, r.width, r.height, r.mips, r.array, r.cube),
            (71, 16, 8, 3, 6, true)
        );
        assert_eq!(bytes, &payload[..]);
    }

    #[test]
    fn an_older_header_names_its_format() {
        let mut h = vec![0u8; 128];
        h[..4].copy_from_slice(b"DDS ");
        let put = |h: &mut Vec<u8>, at: usize, v: u32| h[at..at + 4].copy_from_slice(&v.to_le_bytes());
        put(&mut h, 4, 124);
        put(&mut h, 8, 0x1007);
        put(&mut h, 12, 4);
        put(&mut h, 16, 8);
        put(&mut h, 28, 2);
        put(&mut h, 76, 32);
        put(&mut h, 80, 0x4);
        put(&mut h, 84, u32::from_le_bytes(*b"DXT5"));
        let (t, rest) = read(&h).unwrap();
        assert_eq!(
            (t.format, t.width, t.height, t.mips, t.array, t.dimensions),
            (77, 8, 4, 2, 1, 2)
        );
        assert!(rest.is_empty());
        put(&mut h, 80, 0x41);
        put(&mut h, 84, 0);
        put(&mut h, 88, 32);
        put(&mut h, 92, 0xff_0000);
        put(&mut h, 96, 0xff00);
        put(&mut h, 100, 0xff);
        put(&mut h, 104, 0xff00_0000);
        assert_eq!(read(&h).unwrap().0.format, 87);
        assert!(read(b"DDS nope").is_err());
    }
}
