//! The programs that read a multisampled texture's samples out and write them back bit for bit (see
//! [`crate::format::Samples`] for which formats go which way), shader model 5.0 for Direct3D 11 and 12, compiled
//! from `samples/samples.hlsl` when the crate is built. Each pass draws one triangle over an image, no vertex buffer
//! and no input layout, with its `k` in the vertex shader's constant `b0` (Direct3D 12: root constant 0, before the
//! views' table, parameter 1):
//!
//! - reading, into each one-sample image (a render target array slice of [`Samples::image`]'s format, one per sample
//!   of each slice: image `slice * samples + sample`), `k` is `slice * 32 + sample`; the source's view is `t0` (a
//!   depth-stencil's depth `t0` and stencil `t1`), a `Texture2DMSArray` of the twin;
//! - writing, `k` ([`write_k`]) is the image to read (`t0`, a `Texture2DArray` of the images) and the sample to write,
//!   which the pixel shader alone covers (`SV_Coverage`): into a render target of the twin's `uint` or float view, or
//!   the depth through `SV_Depth` and the stencil one bit at a time, a pass per bit that writes only that bit (its
//!   stencil write mask) with stencil reference 255, and covers no sample where the bit is clear.
//!
//! A 24-bit unorm depth is read as the integer it is (`round(z * (2^24 - 1))`, in 64-bit integer arithmetic) and
//! written as the float nearest that integer over `2^24 - 1`: a GPU whose conversions round to nearest gives back
//! every depth.

use crate::format::Samples;

macro_rules! program {
    ($name:literal) => {
        include_bytes!(concat!(env!("OUT_DIR"), "/", $name, ".dxbc"))
    };
}

/// Direct3D 12's root signature for every pass, serialized: the pass's `k` (one root constant, parameter 0), and a
/// table of the pixel shader's two views (`t0`, `t1`, parameter 1).
pub const ROOT: &[u8] = program!("root");

/// The vertex shader every pass draws with.
pub const VERTICES: &[u8] = program!("vs");

/// The pixel shader that reads samples of format family `s` into the images; with `multisampled` false it reads
/// sample 0 of a one-sample texture (what tests hold the conversions to, against copies).
pub fn read(s: Samples, multisampled: bool) -> &'static [u8] {
    match (s, multisampled) {
        (Samples::Bits { .. }, true) => program!("read_bits"),
        (Samples::Floats { .. }, true) => program!("read_floats"),
        (Samples::DepthStencil { unorm: true, .. }, true) => program!("read_d24s8"),
        (Samples::DepthStencil { unorm: false, .. }, true) => program!("read_d32s8"),
        (Samples::Bits { .. }, false) => program!("read_bits_one"),
        (Samples::Floats { .. }, false) => program!("read_floats_one"),
        (Samples::DepthStencil { unorm: true, .. }, false) => program!("read_d24s8_one"),
        (Samples::DepthStencil { unorm: false, .. }, false) => program!("read_d32s8_one"),
    }
}

/// The pixel shaders that write images of format family `s` back: the colour's, or the depth's and the stencil
/// bit's.
pub fn write(s: Samples) -> (&'static [u8], Option<&'static [u8]>) {
    match s {
        Samples::Bits { .. } => (program!("write_bits"), None),
        Samples::Floats { .. } => (program!("write_floats"), None),
        Samples::DepthStencil { unorm: true, .. } => {
            (program!("write_d24"), Some(program!("write_stencil24")))
        }
        Samples::DepthStencil { unorm: false, .. } => {
            (program!("write_d32"), Some(program!("write_stencil32")))
        }
    }
}

/// `k` for reading sample `sample` of slice `slice`.
pub fn read_k(slice: u32, sample: u32) -> u32 {
    slice * 32 + sample
}

/// `k` for writing image `image` into sample `sample` (and for the pass of its stencil bit `bit`).
pub fn write_k(image: u32, sample: u32, bit: Option<u32>) -> u32 {
    image + bit.map_or(0, |b| (b + 1) << 16) + (sample << 24)
}

/// The most images a pass takes: [`write_k`] keeps the image in 16 bits.
pub const MOST_IMAGES: u32 = 1 << 16;

/// The most samples a texture may have: [`read_k`] keeps the sample in 5 bits.
pub const MOST: u32 = 32;

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn every_program_is_compiled() {
        let families = [
            Samples::Bits { twin: 27, view: 30 },
            Samples::Floats { twin: 90, view: 87 },
            Samples::DepthStencil {
                twin: 44,
                depth: 46,
                stencil: 47,
                unorm: true,
            },
            Samples::DepthStencil {
                twin: 19,
                depth: 21,
                stencil: 22,
                unorm: false,
            },
        ];
        let mut all = vec![VERTICES];
        for s in families {
            all.extend([read(s, true), read(s, false), write(s).0]);
            all.extend(write(s).1);
        }
        for p in all {
            assert_eq!(&p[..4], b"DXBC");
        }
    }
}
