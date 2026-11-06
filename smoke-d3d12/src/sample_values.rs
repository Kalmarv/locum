//! What the `samples` fixtures of both smokes draw into multisampled textures, and the bits each sample must then
//! hold in the canonical layout (`locum_capture::layout::multisampled`): one file, included by both.
use windows::Win32::Graphics::Dxgi::Common::*;

pub const SAMPLES: u32 = 4;
pub const WIDTH: u32 = 8;
pub const HEIGHT: u32 = 4;

pub const VERTICES: &str = "float4 main(uint id : SV_VertexID) : SV_Position { \
    float2 p[3] = {float2(-1,-1), float2(-1,3), float2(3,-1)}; return float4(p[id], 0.5, 1); }";
pub const CONSTANTS: &str = "cbuffer c : register(b0) { uint slice; uint alt; uint2 pad; float4 d[2]; };";
// The colours choose sample 3's other value by a select, not by returning early: WARP's sample-frequency shading
// writes no sample at all from a shader with a return inside an `if` (D3D12 WARP, 2026-11; hardware writes them).
pub const COLOUR: &str = "uint4 main(float4 p : SV_Position, uint s : SV_SampleIndex) : SV_Target { \
    uint x = (uint)p.x, y = (uint)p.y; \
    return alt != 0 && s == 3 ? uint4(0x7F, 0xFF - x, y, slice) \
        : uint4(0x80, (s << 6) | x, (y << 4) | slice, 0xA5 ^ s); }";
pub const FLOATS: &str = "float4 main(float4 p : SV_Position, uint s : SV_SampleIndex) : SV_Target { \
    uint x = (uint)p.x, y = (uint)p.y; float v[4] = {0.5, 1, 2, 3}; \
    return alt != 0 && s == 3 ? float4(4, v[(x + 1) % 4], 0.25, 1) : float4(v[s], v[x % 4], v[y % 4], 1); }";
pub const DEPTH: &str =
    "float main(float4 p : SV_Position) : SV_Depth { uint x = (uint)p.x; return d[x >> 2][x & 3]; }";

/// The samples' values: `alt` for sample 3 as the captured frame draws it.
fn colour(slice: u32, s: u32, x: u32, y: u32, alt: bool) -> [u8; 4] {
    if alt && s == 3 {
        [0x7F, 0xFF - x as u8, y as u8, slice as u8]
    } else {
        [
            0x80,
            ((s << 6) | x) as u8,
            ((y << 4) | slice) as u8,
            0xA5 ^ s as u8,
        ]
    }
}

fn floats(s: u32, x: u32, y: u32, alt: bool) -> [f32; 3] {
    let v = [0.5, 1.0, 2.0, 3.0];
    if alt && s == 3 {
        [4.0, v[(x as usize + 1) % 4], 0.25]
    } else {
        [v[s as usize], v[x as usize % 4], v[y as usize % 4]]
    }
}

/// A float of few mantissa bits as a small float of `mantissa` bits (5-bit exponent, no sign).
fn small(f: f32, mantissa: u32) -> u32 {
    let b = f.to_bits();
    let exponent = ((b >> 23) & 0xFF) as i32 - 127 + 15;
    assert!(
        (1..31).contains(&exponent) && b & ((1 << (23 - mantissa)) - 1) == 0,
        "{f} as a small float"
    );
    (exponent as u32) << mantissa | (b >> (23 - mantissa)) & ((1 << mantissa) - 1)
}

fn r11g11b10(v: [f32; 3]) -> u32 {
    small(v[0], 6) | small(v[1], 6) << 11 | small(v[2], 5) << 22
}

/// D24's depth bits: the edges in column 0, at most 2^22 elsewhere (where a float's rounding to 24 bits cannot be
/// ambiguous).
fn d24(s: u32, x: u32, alt: bool) -> u32 {
    if alt && s == 3 {
        (0x2A_AAAA + x * 0x1111) & 0x3F_FFFF
    } else if x == 0 {
        [0, 1, 0xFF_FFFF, 0x40_0000][s as usize]
    } else {
        (0x01_2345 * x + 0x0_F0F0 * s) & 0x3F_FFFF
    }
}

fn d16(s: u32, x: u32, alt: bool) -> u32 {
    if alt && s == 3 {
        0x4321 + x
    } else if x == 0 {
        [0, 1, 0xFFFF, 0x8000][s as usize]
    } else {
        (0x1234 * x + 0x111 * s) & 0xFFFF
    }
}

fn d32(s: u32, x: u32, alt: bool) -> f32 {
    if alt && s == 3 {
        0.75 - x as f32 / 32.0
    } else if x == 0 {
        [0.0, 1.0, 0.25, f32::from_bits(0x3F7F_FFFF)][s as usize]
    } else {
        x as f32 / 8.0 + s as f32 / 64.0
    }
}

pub fn stencil(s: u32, alt: bool) -> u8 {
    if alt && s == 3 {
        0x3C
    } else {
        [0x00, 0xA5, 0xFF, 0x5A][s as usize]
    }
}

/// The fixture's textures, each of a family the samples are read and written by in its own way.
#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Colour,
    Floats,
    D24S8,
    D32S8,
    D32,
    D16,
    // Families with no `uint` member, read through their own float view (`Samples::Floats`) as the R11G11B10 is.
    Bgra,
    Bgrx,
    B5G6R5,
    B5G5R5A1,
    B4G4R4A4,
    A8,
    Xr,
}

impl Kind {
    pub const ALL: [Kind; 13] = [
        Kind::Colour,
        Kind::Floats,
        Kind::D24S8,
        Kind::D32S8,
        Kind::D32,
        Kind::D16,
        Kind::Bgra,
        Kind::Bgrx,
        Kind::B5G6R5,
        Kind::B5G5R5A1,
        Kind::B4G4R4A4,
        Kind::A8,
        Kind::Xr,
    ];

    /// The resource's format, the view's, and its slices.
    pub fn formats(self) -> (DXGI_FORMAT, DXGI_FORMAT, u32) {
        match self {
            Kind::Colour => (DXGI_FORMAT_R8G8B8A8_TYPELESS, DXGI_FORMAT_R8G8B8A8_UINT, 2),
            Kind::Floats => (DXGI_FORMAT_R11G11B10_FLOAT, DXGI_FORMAT_R11G11B10_FLOAT, 1),
            Kind::D24S8 => (DXGI_FORMAT_R24G8_TYPELESS, DXGI_FORMAT_D24_UNORM_S8_UINT, 1),
            Kind::D32S8 => (DXGI_FORMAT_R32G8X24_TYPELESS, DXGI_FORMAT_D32_FLOAT_S8X24_UINT, 1),
            Kind::D32 => (DXGI_FORMAT_R32_TYPELESS, DXGI_FORMAT_D32_FLOAT, 1),
            Kind::D16 => (DXGI_FORMAT_R16_TYPELESS, DXGI_FORMAT_D16_UNORM, 1),
            Kind::Bgra => (DXGI_FORMAT_B8G8R8A8_TYPELESS, DXGI_FORMAT_B8G8R8A8_UNORM, 1),
            Kind::Bgrx => (DXGI_FORMAT_B8G8R8X8_TYPELESS, DXGI_FORMAT_B8G8R8X8_UNORM, 1),
            Kind::B5G6R5 => (DXGI_FORMAT_B5G6R5_UNORM, DXGI_FORMAT_B5G6R5_UNORM, 1),
            Kind::B5G5R5A1 => (DXGI_FORMAT_B5G5R5A1_UNORM, DXGI_FORMAT_B5G5R5A1_UNORM, 1),
            Kind::B4G4R4A4 => (DXGI_FORMAT_B4G4R4A4_UNORM, DXGI_FORMAT_B4G4R4A4_UNORM, 1),
            Kind::A8 => (DXGI_FORMAT_A8_UNORM, DXGI_FORMAT_A8_UNORM, 1),
            Kind::Xr => (
                DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM,
                DXGI_FORMAT_R10G10B10_XR_BIAS_A2_UNORM,
                1,
            ),
        }
    }

    /// Whether a device may lack it as a multisampled render target (D3D requires the first six): the fixture names
    /// one it lacks and goes on without it.
    pub fn optional(self) -> bool {
        !matches!(
            self,
            Kind::Colour | Kind::Floats | Kind::D24S8 | Kind::D32S8 | Kind::D32 | Kind::D16
        )
    }

    /// The largest value of each channel (red, green, blue, alpha; 0 for one the format lacks) of a family drawn as
    /// unorm integers.
    fn maxima(self) -> Option<[u32; 4]> {
        Some(match self {
            Kind::Bgra | Kind::Bgrx => [255, 255, 255, 255],
            Kind::B5G6R5 => [31, 63, 31, 0],
            Kind::B5G5R5A1 => [31, 31, 31, 1],
            Kind::B4G4R4A4 => [15, 15, 15, 15],
            Kind::A8 => [0, 0, 0, 255],
            Kind::Xr => [1023, 1023, 1023, 3],
            _ => return None,
        })
    }

    /// The pixel shader that draws a colour family's samples (after [`CONSTANTS`]).
    pub fn shader(self) -> String {
        let Some(m) = self.maxima() else {
            return (if self == Kind::Colour { COLOUR } else { FLOATS }).to_string();
        };
        // Each channel's integer as [`Kind::channels`] gives it, then as the float the format stores it from: unorm
        // over the channel's largest value, XR's colour channels biased (`(v - 384) / 510`).
        let colour = if self == Kind::Xr {
            "(float3(v.rgb) - 384.0) / 510.0"
        } else {
            "float3(v.rgb) / float3(max(m.rgb, 1))"
        };
        format!(
            "float4 main(float4 p : SV_Position, uint s : SV_SampleIndex) : SV_Target {{ \
             uint x = (uint)p.x, y = (uint)p.y; uint4 m = uint4({}, {}, {}, {}); \
             uint base = s * 37 + x * 11 + y * 5 + (alt != 0 && s == 3 ? 101 : 0); \
             uint4 v = (base + uint4(0, 3, 6, 9)) % (m + 1); \
             return float4({colour}, float(v.a) / float(max(m.a, 1))); }}",
            m[0], m[1], m[2], m[3]
        )
    }

    /// The channels' integers (red, green, blue, alpha) of sample `s` of texel (x, y), as [`Kind::shader`] draws them.
    fn channels(self, m: [u32; 4], s: u32, x: u32, y: u32, alt: bool) -> [u32; 4] {
        let base = s * 37 + x * 11 + y * 5 + if alt && s == 3 { 101 } else { 0 };
        core::array::from_fn(|i| (base + 3 * i as u32) % (m[i] + 1))
    }

    /// Whether `got` holds `want`'s bits: every byte, but for B8G8R8X8's X byte, which no view reads (D3D defines
    /// none for it, and a write leaves there what the GPU puts).
    pub fn same(self, got: &[u8], want: &[u8]) -> bool {
        got.len() == want.len()
            && got
                .iter()
                .zip(want)
                .enumerate()
                .all(|(i, (a, b))| a == b || (self == Kind::Bgrx && i % 4 == 3))
    }

    pub fn depth(self) -> bool {
        matches!(self, Kind::D24S8 | Kind::D32S8 | Kind::D32 | Kind::D16)
    }

    pub fn stencil(self) -> bool {
        matches!(self, Kind::D24S8 | Kind::D32S8)
    }

    /// Sample `s` of texel (x, y) of `slice` in the canonical layout.
    pub fn texel(self, slice: u32, s: u32, x: u32, y: u32, alt: bool) -> Vec<u8> {
        match self {
            Kind::Colour => colour(slice, s, x, y, alt).to_vec(),
            Kind::Floats => r11g11b10(floats(s, x, y, alt)).to_le_bytes().to_vec(),
            Kind::D24S8 => (d24(s, x, alt) | u32::from(stencil(s, alt)) << 24)
                .to_le_bytes()
                .to_vec(),
            Kind::D32S8 => {
                let mut t = d32(s, x, alt).to_le_bytes().to_vec();
                t.extend([stencil(s, alt), 0, 0, 0]);
                t
            }
            Kind::D32 => d32(s, x, alt).to_le_bytes().to_vec(),
            Kind::D16 => (d16(s, x, alt) as u16).to_le_bytes().to_vec(),
            _ => {
                let m = self.maxima().expect("a unorm family");
                let [r, g, b, a] = self.channels(m, s, x, y, alt);
                match self {
                    Kind::Bgra | Kind::Bgrx => vec![b as u8, g as u8, r as u8, a as u8],
                    Kind::B5G6R5 => ((b | g << 5 | r << 11) as u16).to_le_bytes().to_vec(),
                    Kind::B5G5R5A1 => ((b | g << 5 | r << 10 | a << 15) as u16).to_le_bytes().to_vec(),
                    Kind::B4G4R4A4 => ((b | g << 4 | r << 8 | a << 12) as u16).to_le_bytes().to_vec(),
                    Kind::A8 => vec![a as u8],
                    _ => (r | g << 10 | b << 20 | a << 30).to_le_bytes().to_vec(),
                }
            }
        }
    }

    /// Slice `slice`'s samples, an image each (the canonical layout's order); `alt` as the captured frame leaves them.
    pub fn slice(self, slice: u32, alt: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for s in 0..SAMPLES {
            for y in 0..HEIGHT {
                for x in 0..WIDTH {
                    out.extend(self.texel(slice, s, x, y, alt));
                }
            }
        }
        out
    }

    /// The depth a draw of sample `s` writes in each column.
    pub fn depths(self, s: u32, alt: bool) -> [f32; 8] {
        core::array::from_fn(|x| {
            let x = x as u32;
            match self {
                Kind::D24S8 => (f64::from(d24(s, x, alt)) / 16_777_215.0) as f32,
                Kind::D16 => (f64::from(d16(s, x, alt)) / 65_535.0) as f32,
                _ => d32(s, x, alt),
            }
        })
    }
}
