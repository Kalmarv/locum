//! Render and depth targets read back at the end of each pass, written as DDS files in the format the pass drew in.

use core::ffi::c_void;
use std::path::Path;

use serde::Serialize;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::ids::view_ids;
use super::state;
use crate::log::log;

#[derive(Clone, Serialize)]
pub struct Target {
    /// `rt0` to `rt7`, or `depth`.
    target: String,
    view: u64,
    resource: u64,
    width: u32,
    height: u32,
    /// The DXGI format of the file: the view's, with depth and typeless formats as the readable format of the same
    /// bits.
    format: i32,
    /// The DDS file, relative to the capture's folder (filled in when the frame is written).
    file: Option<String>,
    #[serde(skip)]
    staging: Option<ID3D11Texture2D>,
}

/// A target bound at a draw, kept with the draw so that it can be copied later, on another context.
#[derive(Clone)]
pub struct Bound {
    /// `rt0` to `rt7`, or `depth`.
    target: String,
    view: ID3D11View,
    format: DXGI_FORMAT,
    dimension: i32,
    mip: u32,
    slice: u32,
    depth: bool,
}

/// Whether two sets of targets are the same views in the same slots.
pub(super) fn same(a: &[Bound], b: &[Bound]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.target == y.target && x.view.as_raw() == y.view.as_raw())
}

fn render_target(slot: usize, view: &ID3D11RenderTargetView) -> Bound {
    let mut d = D3D11_RENDER_TARGET_VIEW_DESC::default();
    unsafe { view.GetDesc(&mut d) };
    let (mip, slice) = unsafe {
        match d.ViewDimension {
            D3D11_RTV_DIMENSION_TEXTURE2D => (d.Anonymous.Texture2D.MipSlice, 0),
            D3D11_RTV_DIMENSION_TEXTURE2DARRAY => (
                d.Anonymous.Texture2DArray.MipSlice,
                d.Anonymous.Texture2DArray.FirstArraySlice,
            ),
            D3D11_RTV_DIMENSION_TEXTURE2DMSARRAY => (0, d.Anonymous.Texture2DMSArray.FirstArraySlice),
            _ => (0, 0),
        }
    };
    Bound {
        target: format!("rt{slot}"),
        view: view.cast().expect("a render target view is a view"),
        format: d.Format,
        dimension: d.ViewDimension.0,
        mip,
        slice,
        depth: false,
    }
}

fn depth_target(view: &ID3D11DepthStencilView) -> Bound {
    let mut d = D3D11_DEPTH_STENCIL_VIEW_DESC::default();
    unsafe { view.GetDesc(&mut d) };
    let (mip, slice) = unsafe {
        match d.ViewDimension {
            D3D11_DSV_DIMENSION_TEXTURE2D => (d.Anonymous.Texture2D.MipSlice, 0),
            D3D11_DSV_DIMENSION_TEXTURE2DARRAY => (
                d.Anonymous.Texture2DArray.MipSlice,
                d.Anonymous.Texture2DArray.FirstArraySlice,
            ),
            D3D11_DSV_DIMENSION_TEXTURE2DMSARRAY => (0, d.Anonymous.Texture2DMSArray.FirstArraySlice),
            _ => (0, 0),
        }
    };
    Bound {
        target: "depth".into(),
        view: view.cast().expect("a depth stencil view is a view"),
        format: d.Format,
        dimension: d.ViewDimension.0,
        mip,
        slice,
        depth: true,
    }
}

/// The targets bound on `c`.
pub(super) unsafe fn bound(c: &ID3D11DeviceContext4) -> Vec<Bound> {
    let mut rtvs: [Option<ID3D11RenderTargetView>; 8] = Default::default();
    let mut dsv = None;
    unsafe { c.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv)) };
    let mut out: Vec<Bound> = rtvs
        .iter()
        .enumerate()
        .filter_map(|(slot, v)| v.as_ref().map(|v| render_target(slot, v)))
        .collect();
    out.extend(dsv.as_ref().map(depth_target));
    out
}

/// The targets an `OMSetRenderTargets` call is about to bind.
///
/// # Safety
/// `rtvs` must hold null or live render target views, `dsv` null or a live depth stencil view.
pub(super) unsafe fn from_raw(rtvs: &[*mut c_void], dsv: *mut c_void) -> Vec<Bound> {
    let mut out = Vec::new();
    for (slot, raw) in rtvs.iter().enumerate() {
        if let Some(v) = unsafe { ID3D11RenderTargetView::from_raw_borrowed(raw) } {
            out.push(render_target(slot, v));
        }
    }
    if let Some(v) = unsafe { ID3D11DepthStencilView::from_raw_borrowed(&dsv) } {
        out.push(depth_target(v));
    }
    out
}

/// Copies `targets` into new staging textures, on `c`.
pub(super) unsafe fn copy(c: &ID3D11DeviceContext4, targets: &[Bound]) -> Vec<Target> {
    let Ok(device) = (unsafe { c.GetDevice() }) else {
        return Vec::new();
    };
    targets
        .iter()
        .filter_map(|t| unsafe {
            copy_one(
                c,
                &device,
                &t.view,
                t.target.clone(),
                t.format,
                t.dimension,
                t.mip,
                t.slice,
                t.depth,
            )
        })
        .collect()
}

fn skip(why: &str) {
    let mut s = state();
    s.targets_skipped += 1;
    if s.targets_skipped <= 8 {
        log!("capture: a target not read back: {why}");
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn copy_one(
    c: &ID3D11DeviceContext4,
    device: &ID3D11Device,
    view: &ID3D11View,
    target: String,
    view_format: DXGI_FORMAT,
    dimension: i32,
    mip: u32,
    slice: u32,
    depth: bool,
) -> Option<Target> {
    let resource = unsafe { view.GetResource() }.ok()?;
    let Ok(texture) = resource.cast::<ID3D11Texture2D>() else {
        skip("not a 2D texture");
        return None;
    };
    let mut d = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut d) };
    let format = file_format(view_format, d.Format, depth);
    let Some(bits) = bits_per_pixel(format) else {
        skip(&format!("format {} has no readable layout", format.0));
        return None;
    };
    let (width, height) = ((d.Width >> mip).max(1), (d.Height >> mip).max(1));
    let bytes = u64::from(width) * u64::from(height) * u64::from(bits) / 8;
    {
        let mut s = state();
        if s.target_bytes + bytes > s.request.max_target_bytes {
            s.targets_skipped_bytes += bytes;
            drop(s);
            skip("over the capture's memory limit (max_mb)");
            return None;
        }
        s.target_bytes += bytes;
    }

    let mut source: ID3D11Resource = resource;
    let mut source_sub = mip + slice * d.MipLevels;
    if d.SampleDesc.Count > 1 {
        if depth {
            skip("multisampled depth");
            return None;
        }
        // A multisampled target is resolved into a one-sample texture first.
        let resolved = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: d.Format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            ..Default::default()
        };
        let mut t = None;
        if unsafe { device.CreateTexture2D(&resolved, None, Some(&mut t)) }.is_err() {
            skip("could not make a texture to resolve into");
            return None;
        }
        let t = t?;
        unsafe { c.ResolveSubresource(&t, 0, &source, source_sub, view_format) };
        source = t.cast().expect("a texture is a resource");
        source_sub = 0;
    }
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: d.Format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    let mut staging = None;
    if let Err(e) = unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) } {
        skip(&format!("no staging texture of format {} ({e})", d.Format.0));
        return None;
    }
    let staging = staging?;
    unsafe { c.CopySubresourceRegion(&staging, 0, 0, 0, 0, &source, source_sub, None) };
    let kind = if depth { "dsv" } else { "rtv" };
    let (view, resource) = unsafe { view_ids(view, kind, view_format, dimension) };
    Some(Target {
        target,
        view,
        resource,
        width,
        height,
        format: format.0,
        file: None,
        staging: Some(staging),
    })
}

impl Target {
    pub(super) fn target_name(&self) -> &str {
        &self.target
    }

    /// Maps the copy (waiting for the GPU) and writes it to `dir/name`. Returns the bytes written.
    pub(super) unsafe fn write(&mut self, immediate: &ID3D11DeviceContext4, dir: &Path, name: String) -> u64 {
        let Some(staging) = self.staging.take() else {
            return 0;
        };
        let bits = bits_per_pixel(DXGI_FORMAT(self.format)).expect("checked when copied");
        let row = (self.width * bits / 8) as usize;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if unsafe { immediate.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.is_err() {
            return 0;
        }
        let mut out = dds_header(self.format, self.width, self.height, row as u32);
        for y in 0..self.height as usize {
            let line = unsafe {
                core::slice::from_raw_parts(
                    (mapped.pData as *const u8).add(y * mapped.RowPitch as usize),
                    row,
                )
            };
            out.extend_from_slice(line);
        }
        unsafe { immediate.Unmap(&staging, 0) };
        if std::fs::write(dir.join(&name), &out).is_err() {
            return 0;
        }
        self.file = Some(name);
        out.len() as u64
    }
}

/// The format a target is written in: the view's (or the resource's), with depth formats as the colour format of the
/// same bits and typeless formats as their plain typed format.
fn file_format(view: DXGI_FORMAT, resource: DXGI_FORMAT, depth: bool) -> DXGI_FORMAT {
    let f = if view == DXGI_FORMAT_UNKNOWN {
        resource
    } else {
        view
    };
    if depth {
        return match f {
            DXGI_FORMAT_R32G8X24_TYPELESS | DXGI_FORMAT_D32_FLOAT_S8X24_UINT => {
                DXGI_FORMAT_R32_FLOAT_X8X24_TYPELESS
            }
            DXGI_FORMAT_R32_TYPELESS | DXGI_FORMAT_D32_FLOAT => DXGI_FORMAT_R32_FLOAT,
            DXGI_FORMAT_R24G8_TYPELESS | DXGI_FORMAT_D24_UNORM_S8_UINT => DXGI_FORMAT_R24_UNORM_X8_TYPELESS,
            DXGI_FORMAT_R16_TYPELESS | DXGI_FORMAT_D16_UNORM => DXGI_FORMAT_R16_UNORM,
            other => other,
        };
    }
    match f {
        DXGI_FORMAT_R32G32B32A32_TYPELESS => DXGI_FORMAT_R32G32B32A32_FLOAT,
        DXGI_FORMAT_R16G16B16A16_TYPELESS => DXGI_FORMAT_R16G16B16A16_FLOAT,
        DXGI_FORMAT_R32G32_TYPELESS => DXGI_FORMAT_R32G32_FLOAT,
        DXGI_FORMAT_R10G10B10A2_TYPELESS => DXGI_FORMAT_R10G10B10A2_UNORM,
        DXGI_FORMAT_R8G8B8A8_TYPELESS => DXGI_FORMAT_R8G8B8A8_UNORM,
        DXGI_FORMAT_R16G16_TYPELESS => DXGI_FORMAT_R16G16_FLOAT,
        DXGI_FORMAT_R32_TYPELESS => DXGI_FORMAT_R32_FLOAT,
        DXGI_FORMAT_R8G8_TYPELESS => DXGI_FORMAT_R8G8_UNORM,
        DXGI_FORMAT_R16_TYPELESS => DXGI_FORMAT_R16_FLOAT,
        DXGI_FORMAT_R8_TYPELESS => DXGI_FORMAT_R8_UNORM,
        DXGI_FORMAT_B8G8R8A8_TYPELESS => DXGI_FORMAT_B8G8R8A8_UNORM,
        DXGI_FORMAT_B8G8R8X8_TYPELESS => DXGI_FORMAT_B8G8R8X8_UNORM,
        other => other,
    }
}

/// Bits per pixel of the uncompressed formats a target can have; None for the rest.
fn bits_per_pixel(f: DXGI_FORMAT) -> Option<u32> {
    Some(match f.0 {
        1..=4 => 128,
        5..=8 => 96,
        9..=22 => 64,
        23..=47 | 67 | 87..=93 => 32,
        48..=59 | 85 | 86 | 115 => 16,
        60..=65 => 8,
        _ => return None,
    })
}

/// A DDS header with the DX10 extension: one 2D image of `format`, rows of `pitch` bytes.
fn dds_header(format: i32, width: u32, height: u32, pitch: u32) -> Vec<u8> {
    const CAPS: u32 = 0x1;
    const HEIGHT: u32 = 0x2;
    const WIDTH: u32 = 0x4;
    const PITCH: u32 = 0x8;
    const PIXELFORMAT: u32 = 0x1000;
    const MIPMAPCOUNT: u32 = 0x2_0000;
    const FOURCC: u32 = 0x4;
    const TEXTURE: u32 = 0x1000;
    let dx10 = u32::from_le_bytes(*b"DX10");
    let mut words = vec![
        124,
        CAPS | HEIGHT | WIDTH | PITCH | PIXELFORMAT | MIPMAPCOUNT,
        height,
        width,
        pitch,
        0,
        1,
    ];
    words.extend([0; 11]);
    words.extend([32, FOURCC, dx10, 0, 0, 0, 0, 0]);
    words.extend([TEXTURE, 0, 0, 0, 0]);
    // DX10 header: format, 2D, no flags, one image, alpha mode unknown.
    words.extend([format as u32, 3, 0, 1, 0]);
    let mut out = Vec::with_capacity(4 + words.len() * 4);
    out.extend_from_slice(b"DDS ");
    for w in words {
        out.extend_from_slice(&w.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dds_header_is_148_bytes() {
        let h = dds_header(DXGI_FORMAT_B8G8R8A8_UNORM.0, 64, 32, 256);
        assert_eq!(h.len(), 148);
        assert_eq!(&h[..4], b"DDS ");
        assert_eq!(u32::from_le_bytes(h[12..16].try_into().unwrap()), 32); // height
        assert_eq!(u32::from_le_bytes(h[16..20].try_into().unwrap()), 64); // width
        assert_eq!(u32::from_le_bytes(h[128..132].try_into().unwrap()), 87); // DXGI format
    }

    #[test]
    fn depth_is_written_as_colour() {
        let f = file_format(DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_R24G8_TYPELESS, true);
        assert_eq!(f, DXGI_FORMAT_R24_UNORM_X8_TYPELESS);
        assert_eq!(bits_per_pixel(f), Some(32));
        assert_eq!(bits_per_pixel(DXGI_FORMAT_R11G11B10_FLOAT), Some(32));
        assert_eq!(bits_per_pixel(DXGI_FORMAT_BC1_UNORM), None);
    }
}
