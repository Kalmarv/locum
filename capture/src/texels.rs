//! Reading texels: one subresource of a DXGI format, in the canonical layout, as RGBA floats, and those as 8-bit
//! pixels for a picture. Block-compressed formats are decoded to 8 bits a channel first (BC6H too, so its values are
//! clamped to 0..1); BC4 and BC5 are read as unsigned.

use crate::format;

/// An image of RGBA floats; `channels` says how many of them the format has (1 for depth and single-channel formats).
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    /// Whether the values are unbounded (float formats), not 0..1 or integers.
    pub float: bool,
    pub texels: Vec<[f32; 4]>,
}

/// How values are mapped to 0..255 for a picture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Range {
    /// Single-channel images stretched from their smallest to their largest finite value; float colour tone-mapped;
    /// the rest clamped.
    Auto,
    Clamp,
    /// x / (1 + x) per channel.
    Tonemap,
    /// From the first value to the second.
    Fixed(f32, f32),
}

impl Range {
    /// `auto`, `clamp`, `tonemap` or `LO:HI`.
    pub fn parse(s: &str) -> Option<Range> {
        match s {
            "auto" => Some(Range::Auto),
            "clamp" => Some(Range::Clamp),
            "tonemap" => Some(Range::Tonemap),
            _ => {
                let (lo, hi) = s.split_once(':')?;
                Some(Range::Fixed(lo.trim().parse().ok()?, hi.trim().parse().ok()?))
            }
        }
    }
}

/// A half-precision float's value.
pub fn f16(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((bits >> 10) & 31);
    let mant = f32::from(bits & 0x3ff);
    sign * match exp {
        0 => mant * 2f32.powi(-24),
        31 if mant == 0.0 => f32::INFINITY,
        31 => f32::NAN,
        e => (1.0 + mant / 1024.0) * 2f32.powi(e - 15),
    }
}

/// An unsigned float with a 5-bit exponent and `mantissa` bits (R11G11B10_FLOAT).
fn small_float(bits: u32, mantissa: u32) -> f32 {
    let exp = ((bits >> mantissa) & 31) as i32;
    let mant = (bits & ((1 << mantissa) - 1)) as f32 / (1 << mantissa) as f32;
    match exp {
        0 => mant * 2f32.powi(-14),
        31 => f32::INFINITY,
        e => (1.0 + mant) * 2f32.powi(e - 15),
    }
}

/// How a plain format's texel is read: `n` channels of `bits` each, as `kind`.
#[derive(Clone, Copy)]
enum Kind {
    Float,
    Unorm,
    Snorm,
    Uint,
    Sint,
}

fn plain(format: u32) -> Option<(u8, u32, Kind)> {
    use Kind::*;
    Some(match format {
        2 => (4, 32, Float),
        3 => (4, 32, Uint),
        4 => (4, 32, Sint),
        6 => (3, 32, Float),
        7 => (3, 32, Uint),
        8 => (3, 32, Sint),
        10 => (4, 16, Float),
        11 => (4, 16, Unorm),
        12 => (4, 16, Uint),
        13 => (4, 16, Snorm),
        14 => (4, 16, Sint),
        16 => (2, 32, Float),
        17 => (2, 32, Uint),
        18 => (2, 32, Sint),
        28 | 29 => (4, 8, Unorm),
        30 => (4, 8, Uint),
        31 => (4, 8, Snorm),
        32 => (4, 8, Sint),
        34 => (2, 16, Float),
        35 => (2, 16, Unorm),
        36 => (2, 16, Uint),
        37 => (2, 16, Snorm),
        38 => (2, 16, Sint),
        41 => (1, 32, Float),
        42 => (1, 32, Uint),
        43 => (1, 32, Sint),
        49 => (2, 8, Unorm),
        50 => (2, 8, Uint),
        51 => (2, 8, Snorm),
        52 => (2, 8, Sint),
        54 => (1, 16, Float),
        56 => (1, 16, Unorm),
        57 => (1, 16, Uint),
        58 => (1, 16, Snorm),
        59 => (1, 16, Sint),
        61 => (1, 8, Unorm),
        62 => (1, 8, Uint),
        63 => (1, 8, Snorm),
        64 => (1, 8, Sint),
        _ => return None,
    })
}

fn channel(bytes: &[u8], bits: u32, kind: Kind) -> f32 {
    match (bits, kind) {
        (32, Kind::Float) => f32::from_le_bytes(bytes[..4].try_into().unwrap()),
        (32, Kind::Uint) => u32::from_le_bytes(bytes[..4].try_into().unwrap()) as f32,
        (32, _) => i32::from_le_bytes(bytes[..4].try_into().unwrap()) as f32,
        (16, k) => {
            let v = u16::from_le_bytes(bytes[..2].try_into().unwrap());
            match k {
                Kind::Float => f16(v),
                Kind::Unorm => f32::from(v) / 65535.0,
                Kind::Snorm => (f32::from(v as i16) / 32767.0).max(-1.0),
                Kind::Uint => f32::from(v),
                Kind::Sint => f32::from(v as i16),
            }
        }
        (_, k) => {
            let v = bytes[0];
            match k {
                Kind::Unorm | Kind::Float => f32::from(v) / 255.0,
                Kind::Snorm => (f32::from(v as i8) / 127.0).max(-1.0),
                Kind::Uint => f32::from(v),
                Kind::Sint => f32::from(v as i8),
            }
        }
    }
}

type BlockFn = fn(&[u8], &mut [u32]);

fn block_decoder(format: u32) -> Option<(BlockFn, u8)> {
    use texture2ddecoder as t;
    Some(match format {
        71 | 72 => (t::decode_bc1_block as BlockFn, 4),
        74 | 75 => (t::decode_bc2_block, 4),
        77 | 78 => (t::decode_bc3_block, 4),
        80 | 81 => (t::decode_bc4_block, 1),
        83 | 84 => (t::decode_bc5_block, 2),
        95 => (t::decode_bc6_block_unsigned, 3),
        96 => (t::decode_bc6_block_signed, 3),
        98 | 99 => (t::decode_bc7_block, 4),
        _ => return None,
    })
}

/// Decodes one subresource of `width` x `height` texels (one depth slice of a volume: pass its bytes only).
pub fn decode(format: u32, width: u32, height: u32, bytes: &[u8]) -> Result<Image, String> {
    let format = format::readable(format);
    let layout = format::layout(format).ok_or_else(|| format!("{} has no layout", format::name(format)))?;
    let need = u64::from(layout.row_bytes(width)) * u64::from(layout.rows(height));
    if (bytes.len() as u64) < need {
        return Err(format!("{} bytes, {need} needed", bytes.len()));
    }
    let (w, h) = (width as usize, height as usize);
    let mut texels = vec![[0f32; 4]; w * h];
    let mut float = false;
    let channels;
    if let Some((decode_block, n)) = block_decoder(format) {
        channels = n;
        let row_bytes = layout.row_bytes(width) as usize;
        let block_bytes = layout.block_bytes as usize;
        let mut out = [0u32; 16];
        for by in 0..h.div_ceil(4) {
            for bx in 0..w.div_ceil(4) {
                let at = by * row_bytes + bx * block_bytes;
                out.fill(0);
                decode_block(&bytes[at..at + block_bytes], &mut out);
                for (i, px) in out.iter().enumerate() {
                    let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
                    if x < w && y < h {
                        let [b, g, r, a] = px.to_le_bytes();
                        texels[y * w + x] = [r, g, b, a].map(|c| f32::from(c) / 255.0);
                    }
                }
            }
        }
        if n < 4 {
            for t in &mut texels {
                t[3] = 1.0;
            }
        }
    } else if let Some((n, bits, kind)) = plain(format) {
        channels = n;
        float = matches!(kind, Kind::Float);
        let size = (bits / 8) as usize;
        for (i, t) in texels.iter_mut().enumerate() {
            let at = i * size * n as usize;
            for c in 0..n as usize {
                t[c] = channel(&bytes[at + c * size..], bits, kind);
            }
            if n < 4 {
                t[3] = 1.0;
            }
        }
    } else {
        let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        let half = |i: usize| u16::from_le_bytes(bytes[i * 2..i * 2 + 2].try_into().unwrap());
        match format {
            // R10G10B10A2_UNORM / UINT
            24 | 25 => {
                channels = 4;
                let scale = if format == 24 {
                    [1023.0, 1023.0, 1023.0, 3.0]
                } else {
                    [1.0; 4]
                };
                for (i, t) in texels.iter_mut().enumerate() {
                    let v = word(i);
                    let c = [v & 1023, (v >> 10) & 1023, (v >> 20) & 1023, v >> 30];
                    *t = [0, 1, 2, 3].map(|k| c[k] as f32 / scale[k]);
                }
            }
            26 => {
                channels = 3;
                float = true;
                for (i, t) in texels.iter_mut().enumerate() {
                    let v = word(i);
                    *t = [
                        small_float(v & 0x7ff, 6),
                        small_float((v >> 11) & 0x7ff, 6),
                        small_float(v >> 22, 5),
                        1.0,
                    ];
                }
            }
            // R32_FLOAT_X8X24: the depth of D32_FLOAT_S8X24; the stencil word is left out.
            21 => {
                channels = 1;
                float = true;
                for (i, t) in texels.iter_mut().enumerate() {
                    *t = [f32::from_bits(word(i * 2)), 0.0, 0.0, 1.0];
                }
            }
            // R24_UNORM_X8: the depth of D24_UNORM_S8.
            46 => {
                channels = 1;
                for (i, t) in texels.iter_mut().enumerate() {
                    *t = [(word(i) & 0xff_ffff) as f32 / 16_777_215.0, 0.0, 0.0, 1.0];
                }
            }
            65 => {
                channels = 4;
                for (i, t) in texels.iter_mut().enumerate() {
                    *t = [0.0, 0.0, 0.0, f32::from(bytes[i]) / 255.0];
                }
            }
            67 => {
                channels = 3;
                float = true;
                for (i, t) in texels.iter_mut().enumerate() {
                    let v = word(i);
                    let scale = 2f32.powi((v >> 27) as i32 - 15 - 9);
                    *t = [
                        (v & 511) as f32 * scale,
                        ((v >> 9) & 511) as f32 * scale,
                        ((v >> 18) & 511) as f32 * scale,
                        1.0,
                    ];
                }
            }
            85 => {
                channels = 3;
                for (i, t) in texels.iter_mut().enumerate() {
                    let v = half(i);
                    *t = [
                        f32::from(v >> 11) / 31.0,
                        f32::from((v >> 5) & 63) / 63.0,
                        f32::from(v & 31) / 31.0,
                        1.0,
                    ];
                }
            }
            86 => {
                channels = 4;
                for (i, t) in texels.iter_mut().enumerate() {
                    let v = half(i);
                    *t = [
                        f32::from((v >> 10) & 31) / 31.0,
                        f32::from((v >> 5) & 31) / 31.0,
                        f32::from(v & 31) / 31.0,
                        f32::from(v >> 15),
                    ];
                }
            }
            115 => {
                channels = 4;
                for (i, t) in texels.iter_mut().enumerate() {
                    let v = half(i);
                    *t = [8, 4, 0, 12].map(|s| f32::from((v >> s) & 15) / 15.0);
                }
            }
            87 | 88 | 91 | 93 => {
                channels = if matches!(format, 87 | 91) { 4 } else { 3 };
                for (i, t) in texels.iter_mut().enumerate() {
                    let [b, g, r, a] = [0, 1, 2, 3].map(|k| f32::from(bytes[i * 4 + k]) / 255.0);
                    *t = [r, g, b, if channels == 4 { a } else { 1.0 }];
                }
            }
            other => return Err(format!("{} cannot be decoded", format::name(other))),
        }
    }
    Ok(Image {
        width,
        height,
        channels,
        float,
        texels,
    })
}

impl Image {
    /// Channel `c` (0 to 3) alone, as a single-channel image.
    pub fn channel(&self, c: usize) -> Image {
        Image {
            width: self.width,
            height: self.height,
            channels: 1,
            float: self.float,
            texels: self.texels.iter().map(|t| [t[c.min(3)], 0.0, 0.0, 1.0]).collect(),
        }
    }

    /// The texels from (x, y), `w` wide and `h` high, clipped to the image.
    pub fn crop(&self, [x, y, w, h]: [u32; 4]) -> Image {
        let (x0, y0) = (x.min(self.width), y.min(self.height));
        let (x1, y1) = (
            x.saturating_add(w).min(self.width),
            y.saturating_add(h).min(self.height),
        );
        let mut texels = Vec::with_capacity(((x1 - x0) * (y1 - y0)) as usize);
        for row in y0..y1 {
            let at = (row * self.width) as usize;
            texels.extend_from_slice(&self.texels[at + x0 as usize..at + x1 as usize]);
        }
        Image {
            width: x1 - x0,
            height: y1 - y0,
            channels: self.channels,
            float: self.float,
            texels,
        }
    }

    /// The smallest rectangle (x, y, width, height) holding every texel whose channels are not all zero, if any is.
    pub fn content(&self) -> Option<[u32; 4]> {
        let n = usize::from(self.channels.max(1));
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
        for (i, t) in self.texels.iter().enumerate() {
            if t[..n].iter().any(|v| *v != 0.0) {
                let (x, y) = (i as u32 % self.width, i as u32 / self.width);
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x + 1), y1.max(y + 1));
            }
        }
        (x1 > 0).then(|| [x0, y0, x1 - x0, y1 - y0])
    }

    /// How `rgba8` maps values for `range`: `Auto` as it resolves for this image.
    pub fn mapping(&self, range: Range) -> Range {
        match range {
            Range::Auto if self.channels == 1 => {
                let (lo, hi) = self
                    .texels
                    .iter()
                    .map(|t| t[0])
                    .filter(|v| v.is_finite())
                    .fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)));
                if lo < hi {
                    Range::Fixed(lo, hi)
                } else {
                    Range::Clamp
                }
            }
            Range::Auto if self.float => Range::Tonemap,
            Range::Auto => Range::Clamp,
            r => r,
        }
    }

    /// The image as 8-bit RGBA: single-channel images grey, two-channel ones red and green.
    pub fn rgba8(&self, range: Range) -> Vec<u8> {
        let range = self.mapping(range);
        let map = |v: f32| -> u8 {
            let v = if v.is_nan() { 0.0 } else { v };
            let v = match range {
                Range::Tonemap => {
                    let v = v.max(0.0);
                    v / (1.0 + v)
                }
                Range::Fixed(lo, hi) => (v - lo) / (hi - lo),
                _ => v,
            };
            (v.clamp(0.0, 1.0) * 255.0).round() as u8
        };
        let mut out = Vec::with_capacity(self.texels.len() * 4);
        for t in &self.texels {
            let rgb = match self.channels {
                1 => [t[0]; 3],
                2 => [t[0], t[1], 0.0],
                _ => [t[0], t[1], t[2]],
            };
            out.extend(rgb.map(map));
            out.push(if self.channels == 4 {
                (t[3].clamp(0.0, 1.0) * 255.0).round() as u8
            } else {
                255
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_crop_and_content() {
        // A 4x2 R8G8B8A8 image, zero but for texels (1,0) and (2,1).
        let mut bytes = vec![0u8; 4 * 2 * 4];
        bytes[4..8].copy_from_slice(&[255, 0, 0, 255]);
        bytes[24..28].copy_from_slice(&[0, 255, 0, 0]);
        let img = decode(28, 4, 2, &bytes).unwrap();
        assert_eq!(img.content(), Some([1, 0, 2, 2]));
        let part = img.crop([1, 0, 2, 2]);
        assert_eq!((part.width, part.height), (2, 2));
        assert_eq!(part.texels[0], [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(part.texels[3], [0.0, 1.0, 0.0, 0.0]);
        let g = img.channel(1);
        assert_eq!((g.channels, g.texels[6][0], g.texels[1][0]), (1, 1.0, 0.0));
        // Clipped to the image.
        assert_eq!(img.crop([3, 1, 10, 10]).texels.len(), 1);
        assert_eq!(decode(28, 1, 1, &[0; 4]).unwrap().content(), None);
    }

    #[test]
    fn halves() {
        assert_eq!(f16(0x3c00), 1.0);
        assert_eq!(f16(0xc000), -2.0);
        assert_eq!(f16(0x0001), 2f32.powi(-24));
        assert!(f16(0x7c00).is_infinite());
    }

    #[test]
    fn bgra_and_depth() {
        let img = decode(87, 1, 1, &[10, 20, 30, 40]).unwrap();
        assert_eq!(img.rgba8(Range::Clamp), vec![30, 20, 10, 40]);
        let depth = decode(46, 2, 1, &[0, 0, 0, 0xab, 0xff, 0xff, 0xff, 0]).unwrap();
        assert_eq!(depth.channels, 1);
        assert_eq!(depth.texels[1][0], 1.0);
        assert_eq!(depth.rgba8(Range::Auto), vec![0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn bc1_solid_block() {
        // Both endpoints pure red (R5G6B5 0xF800), every index 0.
        let block = [0x00, 0xf8, 0x00, 0xf8, 0, 0, 0, 0];
        let img = decode(71, 4, 4, &block).unwrap();
        assert!(
            img.texels
                .iter()
                .all(|t| t[0] == 1.0 && t[1] == 0.0 && t[2] == 0.0)
        );
    }
}
