//! DXGI formats: their names and memory layout. Direct3D 11 and 12 share them.

/// How one format lays out its texels: blocks of `block_width` x `block_height` texels, `block_bytes` each. Plain
/// formats have 1x1 blocks; block-compressed formats 4x4; packed 4:2:2 formats 2x1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub block_width: u32,
    pub block_height: u32,
    pub block_bytes: u32,
}

impl Layout {
    const fn plain(bytes: u32) -> Option<Layout> {
        Some(Layout {
            block_width: 1,
            block_height: 1,
            block_bytes: bytes,
        })
    }

    const fn blocks(width: u32, height: u32, bytes: u32) -> Option<Layout> {
        Some(Layout {
            block_width: width,
            block_height: height,
            block_bytes: bytes,
        })
    }

    /// Bytes in one row of blocks of an image `width` texels wide.
    pub fn row_bytes(self, width: u32) -> u32 {
        width.div_ceil(self.block_width) * self.block_bytes
    }

    /// Rows of blocks in an image `height` texels high.
    pub fn rows(self, height: u32) -> u32 {
        height.div_ceil(self.block_height)
    }
}

/// The layout of a format, or None for formats that are not one plane of blocks (planar video formats, `UNKNOWN`).
pub fn layout(format: u32) -> Option<Layout> {
    match format {
        1..=4 => Layout::plain(16),
        5..=8 => Layout::plain(12),
        9..=22 => Layout::plain(8),
        23..=47 => Layout::plain(4),
        48..=59 => Layout::plain(2),
        60..=65 => Layout::plain(1),
        // R1_UNORM: one bit per texel, eight texels per byte.
        66 => Layout::blocks(8, 1, 1),
        67 => Layout::plain(4),
        68 | 69 => Layout::blocks(2, 1, 4),
        70..=72 | 79..=81 => Layout::blocks(4, 4, 8),
        73..=78 | 82..=84 | 94..=99 => Layout::blocks(4, 4, 16),
        85 | 86 | 115 | 189 | 190 => Layout::plain(2),
        87..=93 | 100 | 101 => Layout::plain(4),
        102 => Layout::plain(8),
        107 => Layout::blocks(2, 1, 4),
        108 | 109 => Layout::blocks(2, 1, 8),
        111..=113 => Layout::plain(1),
        114 => Layout::plain(2),
        _ => None,
    }
}

/// Whether a format is block-compressed (BC1 to BC7).
pub fn is_compressed(format: u32) -> bool {
    matches!(format, 70..=84 | 94..=99)
}

/// Whether a format is a depth (or depth-stencil) format or one of the typeless formats depth textures are made of.
pub fn is_depth(format: u32) -> bool {
    matches!(format, 19 | 20 | 39 | 40 | 44 | 45 | 53 | 55)
}

/// Whether Direct3D 12 keeps a format's depth and stencil in separate planes (copied apart: depth 4 bytes a texel,
/// stencil 1), where Direct3D 11 interleaves them in one texel.
pub fn is_depth_stencil(format: u32) -> bool {
    matches!(format, 19 | 20 | 44 | 45)
}

/// How a canonical texel of a depth format's family lays out its bytes (see `layout::join_depth_stencil`), or None
/// for other formats, whose names say it.
pub fn texel(format: u32) -> Option<&'static str> {
    match format {
        // R32G8X24_TYPELESS, D32_FLOAT_S8X24_UINT, R32_FLOAT_X8X24_TYPELESS, X32_TYPELESS_G8X24_UINT.
        19..=22 => {
            Some("8 bytes a texel: the 32-bit float depth, then the stencil byte and three zero bytes")
        }
        // R32_TYPELESS, D32_FLOAT, R32_FLOAT.
        39..=41 => Some("4 bytes a texel: the 32-bit float depth"),
        // R24G8_TYPELESS, D24_UNORM_S8_UINT, R24_UNORM_X8_TYPELESS, X24_TYPELESS_G8_UINT.
        44..=47 => {
            Some("4 bytes a texel: the 24-bit unorm depth in the low bits, the stencil in the top byte")
        }
        // R16_TYPELESS, D16_UNORM, R16_UNORM.
        53 | 55 | 56 => Some("2 bytes a texel: the 16-bit unorm depth"),
        _ => None,
    }
}

/// The format to read a texture of `format` back as: typeless formats as their plain typed format, depth formats as
/// the colour format of the same bits. Copies between the two are bit-exact.
pub fn readable(format: u32) -> u32 {
    match format {
        1 => 2,
        5 => 6,
        9 => 10,
        15 => 16,
        19 | 20 => 21,
        23 => 24,
        27 => 28,
        33 => 34,
        39 | 40 => 41,
        44 | 45 => 46,
        48 => 49,
        53 | 55 => 56,
        60 => 61,
        70 => 71,
        73 => 74,
        76 => 77,
        79 => 80,
        82 => 83,
        90 => 87,
        92 => 88,
        94 => 95,
        97 => 98,
        other => other,
    }
}

/// The typeless format of `format`'s family (the formats with the same bits that views of one texture may mix), or
/// `format` itself for a format with no family.
pub fn typeless(format: u32) -> u32 {
    match format {
        1..=4 => 1,
        5..=8 => 5,
        9..=14 => 9,
        15..=18 => 15,
        19..=22 => 19,
        23..=25 => 23,
        27..=32 => 27,
        33..=38 => 33,
        39..=43 => 39,
        44..=47 => 44,
        48..=52 => 48,
        53..=59 => 53,
        60..=64 => 60,
        70..=72 => 70,
        73..=75 => 73,
        76..=78 => 76,
        79..=81 => 79,
        82..=84 => 82,
        87 | 90 | 91 => 90,
        88 | 92 | 93 => 92,
        94..=96 => 94,
        97..=99 => 97,
        other => other,
    }
}

/// The format's name without the `DXGI_FORMAT_` prefix.
pub fn name(format: u32) -> &'static str {
    NAMES
        .iter()
        .find(|(f, _)| *f == format)
        .map_or("UNKNOWN_FORMAT", |(_, n)| n)
}

/// The format named `name` (with or without the `DXGI_FORMAT_` prefix, any case).
pub fn by_name(name: &str) -> Option<u32> {
    let prefix = "DXGI_FORMAT_";
    let name = match name.get(..prefix.len()) {
        Some(p) if p.eq_ignore_ascii_case(prefix) => &name[prefix.len()..],
        _ => name,
    };
    NAMES
        .iter()
        .find(|(_, n)| n.eq_ignore_ascii_case(name))
        .map(|(f, _)| *f)
}

const NAMES: &[(u32, &str)] = &[
    (0, "UNKNOWN"),
    (1, "R32G32B32A32_TYPELESS"),
    (2, "R32G32B32A32_FLOAT"),
    (3, "R32G32B32A32_UINT"),
    (4, "R32G32B32A32_SINT"),
    (5, "R32G32B32_TYPELESS"),
    (6, "R32G32B32_FLOAT"),
    (7, "R32G32B32_UINT"),
    (8, "R32G32B32_SINT"),
    (9, "R16G16B16A16_TYPELESS"),
    (10, "R16G16B16A16_FLOAT"),
    (11, "R16G16B16A16_UNORM"),
    (12, "R16G16B16A16_UINT"),
    (13, "R16G16B16A16_SNORM"),
    (14, "R16G16B16A16_SINT"),
    (15, "R32G32_TYPELESS"),
    (16, "R32G32_FLOAT"),
    (17, "R32G32_UINT"),
    (18, "R32G32_SINT"),
    (19, "R32G8X24_TYPELESS"),
    (20, "D32_FLOAT_S8X24_UINT"),
    (21, "R32_FLOAT_X8X24_TYPELESS"),
    (22, "X32_TYPELESS_G8X24_UINT"),
    (23, "R10G10B10A2_TYPELESS"),
    (24, "R10G10B10A2_UNORM"),
    (25, "R10G10B10A2_UINT"),
    (26, "R11G11B10_FLOAT"),
    (27, "R8G8B8A8_TYPELESS"),
    (28, "R8G8B8A8_UNORM"),
    (29, "R8G8B8A8_UNORM_SRGB"),
    (30, "R8G8B8A8_UINT"),
    (31, "R8G8B8A8_SNORM"),
    (32, "R8G8B8A8_SINT"),
    (33, "R16G16_TYPELESS"),
    (34, "R16G16_FLOAT"),
    (35, "R16G16_UNORM"),
    (36, "R16G16_UINT"),
    (37, "R16G16_SNORM"),
    (38, "R16G16_SINT"),
    (39, "R32_TYPELESS"),
    (40, "D32_FLOAT"),
    (41, "R32_FLOAT"),
    (42, "R32_UINT"),
    (43, "R32_SINT"),
    (44, "R24G8_TYPELESS"),
    (45, "D24_UNORM_S8_UINT"),
    (46, "R24_UNORM_X8_TYPELESS"),
    (47, "X24_TYPELESS_G8_UINT"),
    (48, "R8G8_TYPELESS"),
    (49, "R8G8_UNORM"),
    (50, "R8G8_UINT"),
    (51, "R8G8_SNORM"),
    (52, "R8G8_SINT"),
    (53, "R16_TYPELESS"),
    (54, "R16_FLOAT"),
    (55, "D16_UNORM"),
    (56, "R16_UNORM"),
    (57, "R16_UINT"),
    (58, "R16_SNORM"),
    (59, "R16_SINT"),
    (60, "R8_TYPELESS"),
    (61, "R8_UNORM"),
    (62, "R8_UINT"),
    (63, "R8_SNORM"),
    (64, "R8_SINT"),
    (65, "A8_UNORM"),
    (66, "R1_UNORM"),
    (67, "R9G9B9E5_SHAREDEXP"),
    (68, "R8G8_B8G8_UNORM"),
    (69, "G8R8_G8B8_UNORM"),
    (70, "BC1_TYPELESS"),
    (71, "BC1_UNORM"),
    (72, "BC1_UNORM_SRGB"),
    (73, "BC2_TYPELESS"),
    (74, "BC2_UNORM"),
    (75, "BC2_UNORM_SRGB"),
    (76, "BC3_TYPELESS"),
    (77, "BC3_UNORM"),
    (78, "BC3_UNORM_SRGB"),
    (79, "BC4_TYPELESS"),
    (80, "BC4_UNORM"),
    (81, "BC4_SNORM"),
    (82, "BC5_TYPELESS"),
    (83, "BC5_UNORM"),
    (84, "BC5_SNORM"),
    (85, "B5G6R5_UNORM"),
    (86, "B5G5R5A1_UNORM"),
    (87, "B8G8R8A8_UNORM"),
    (88, "B8G8R8X8_UNORM"),
    (89, "R10G10B10_XR_BIAS_A2_UNORM"),
    (90, "B8G8R8A8_TYPELESS"),
    (91, "B8G8R8A8_UNORM_SRGB"),
    (92, "B8G8R8X8_TYPELESS"),
    (93, "B8G8R8X8_UNORM_SRGB"),
    (94, "BC6H_TYPELESS"),
    (95, "BC6H_UF16"),
    (96, "BC6H_SF16"),
    (97, "BC7_TYPELESS"),
    (98, "BC7_UNORM"),
    (99, "BC7_UNORM_SRGB"),
    (100, "AYUV"),
    (101, "Y410"),
    (102, "Y416"),
    (103, "NV12"),
    (104, "P010"),
    (105, "P016"),
    (106, "420_OPAQUE"),
    (107, "YUY2"),
    (108, "Y210"),
    (109, "Y216"),
    (110, "NV11"),
    (111, "AI44"),
    (112, "IA44"),
    (113, "P8"),
    (114, "A8P8"),
    (115, "B4G4R4A4_UNORM"),
    (130, "P208"),
    (131, "V208"),
    (132, "V408"),
    (189, "A4B4G4R4_UNORM"),
    (190, "A4R4G4B4_UNORM"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts() {
        assert_eq!(layout(28).unwrap().row_bytes(1920), 7680);
        let bc1 = layout(71).unwrap();
        assert_eq!((bc1.row_bytes(1024), bc1.rows(1024)), (2048, 256));
        // A 2x2 mip of a BC7 texture is still one 16-byte block.
        let bc7 = layout(98).unwrap();
        assert_eq!((bc7.row_bytes(2), bc7.rows(2)), (16, 1));
        assert_eq!(layout(103), None);
    }

    #[test]
    fn names_round_trip() {
        for &(f, n) in NAMES {
            assert_eq!(by_name(n), Some(f));
            assert_eq!(name(f), n);
        }
        assert_eq!(by_name("dxgi_format_bc7_unorm"), Some(98));
    }

    #[test]
    fn readable_formats() {
        assert_eq!(readable(45), 46);
        assert_eq!(readable(19), 21);
        assert_eq!(readable(27), 28);
        assert_eq!(readable(98), 98);
    }

    #[test]
    fn a_family_is_its_typeless_format_with_the_same_layout() {
        for &(f, _) in NAMES {
            let t = typeless(f);
            if t != f || name(f).ends_with("_TYPELESS") {
                assert!(name(t).ends_with("_TYPELESS"), "{} -> {}", name(f), name(t));
            }
            if t != f && layout(f).is_some() {
                assert_eq!(layout(t), layout(f), "{} -> {}", name(f), name(t));
            }
        }
        assert_eq!(typeless(29), 27);
        assert_eq!(typeless(91), 90);
        assert_eq!(typeless(26), 26);
    }
}
