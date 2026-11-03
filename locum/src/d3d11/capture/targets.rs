//! Render and depth targets read back at the end of each pass, in the format the pass drew in (depth as the colour
//! format of the same bits), into the blob store.

use core::ffi::c_void;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use super::ids::view_ids;
use super::state;
use crate::log::log;
use locum_d3d11::predication::unpredicated;

#[derive(Clone)]
pub struct Target {
    /// `rt0` to `rt7`, or `depth`.
    pub target: String,
    pub view: u64,
    pub resource: u64,
    pub width: u32,
    pub height: u32,
    /// The format it is read as: the view's, with depth and typeless formats as the readable format of the same bits.
    pub format: i32,
    /// The subresource of the resource the view writes.
    pub sub: u32,
    /// Depth plane within a 3D mip; zero for 2D targets.
    pub z: u32,
    staging: Option<ID3D11Resource>,
}

/// A target bound at a draw, kept with the draw so that it can be copied later, on another context.
#[derive(Clone)]
pub struct Bound {
    /// `rt0` to `rt7`, or `depth`.
    target: String,
    view: ID3D11View,
    format: DXGI_FORMAT,
    mip: u32,
    slice: u32,
    slices: u32,
    volume: bool,
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
    let (mip, slice, slices) = unsafe {
        match d.ViewDimension {
            D3D11_RTV_DIMENSION_TEXTURE2D => (d.Anonymous.Texture2D.MipSlice, 0, 1),
            D3D11_RTV_DIMENSION_TEXTURE2DARRAY => (
                d.Anonymous.Texture2DArray.MipSlice,
                d.Anonymous.Texture2DArray.FirstArraySlice,
                d.Anonymous.Texture2DArray.ArraySize,
            ),
            D3D11_RTV_DIMENSION_TEXTURE2DMSARRAY => (
                0,
                d.Anonymous.Texture2DMSArray.FirstArraySlice,
                d.Anonymous.Texture2DMSArray.ArraySize,
            ),
            D3D11_RTV_DIMENSION_TEXTURE3D => (
                d.Anonymous.Texture3D.MipSlice,
                d.Anonymous.Texture3D.FirstWSlice,
                d.Anonymous.Texture3D.WSize,
            ),
            _ => (0, 0, 1),
        }
    };
    Bound {
        target: format!("rt{slot}"),
        view: view.cast().expect("a render target view is a view"),
        format: d.Format,
        volume: d.ViewDimension == D3D11_RTV_DIMENSION_TEXTURE3D,
        mip,
        slice,
        slices,
        depth: false,
    }
}

fn depth_target(view: &ID3D11DepthStencilView) -> Bound {
    let mut d = D3D11_DEPTH_STENCIL_VIEW_DESC::default();
    unsafe { view.GetDesc(&mut d) };
    let (mip, slice, slices) = unsafe {
        match d.ViewDimension {
            D3D11_DSV_DIMENSION_TEXTURE2D => (d.Anonymous.Texture2D.MipSlice, 0, 1),
            D3D11_DSV_DIMENSION_TEXTURE2DARRAY => (
                d.Anonymous.Texture2DArray.MipSlice,
                d.Anonymous.Texture2DArray.FirstArraySlice,
                d.Anonymous.Texture2DArray.ArraySize,
            ),
            D3D11_DSV_DIMENSION_TEXTURE2DMSARRAY => (
                0,
                d.Anonymous.Texture2DMSArray.FirstArraySlice,
                d.Anonymous.Texture2DMSArray.ArraySize,
            ),
            _ => (0, 0, 1),
        }
    };
    Bound {
        target: "depth".into(),
        view: view.cast().expect("a depth stencil view is a view"),
        format: d.Format,
        volume: false,
        mip,
        slice,
        slices,
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
    let mut out = Vec::new();
    for t in targets {
        let Ok(resource) = (unsafe { t.view.GetResource() }) else {
            continue;
        };
        let layers = if t.volume {
            let Ok(texture) = resource.cast::<ID3D11Texture3D>() else {
                continue;
            };
            let mut d = D3D11_TEXTURE3D_DESC::default();
            unsafe { texture.GetDesc(&mut d) };
            (d.Depth >> t.mip).max(1)
        } else {
            let Ok(texture) = resource.cast::<ID3D11Texture2D>() else {
                skip("not a 2D or 3D texture");
                continue;
            };
            let mut d = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut d) };
            d.ArraySize
        };
        let count = t.slices.min(layers.saturating_sub(t.slice));
        for slice in t.slice..t.slice + count {
            let name = if count > 1 || t.volume {
                format!("{}[{}{}]", t.target, if t.volume { "z" } else { "slice" }, slice)
            } else {
                t.target.clone()
            };
            out.extend(unsafe { copy_one(c, &device, t, name, slice) });
        }
    }
    out
}

fn skip(why: &str) {
    let mut s = state();
    s.targets_skipped += 1;
    if s.targets_skipped <= 8 {
        log!("capture: a target not read back: {why}");
    }
}

unsafe fn copy_one(
    c: &ID3D11DeviceContext4,
    device: &ID3D11Device,
    bound: &Bound,
    target: String,
    slice: u32,
) -> Option<Target> {
    let (view, view_format, mip, depth) = (&bound.view, bound.format, bound.mip, bound.depth);
    let resource = unsafe { view.GetResource() }.ok()?;
    if super::tiled::special(&resource) {
        skip("tiled target requires mapped-tile readback");
        return None;
    }
    let mut d = D3D11_TEXTURE2D_DESC::default();
    if bound.volume {
        let texture = resource.cast::<ID3D11Texture3D>().ok()?;
        let mut v = D3D11_TEXTURE3D_DESC::default();
        unsafe { texture.GetDesc(&mut v) };
        d.Width = v.Width;
        d.Height = v.Height;
        d.MipLevels = v.MipLevels;
        d.Format = v.Format;
        d.SampleDesc.Count = 1;
    } else {
        unsafe { resource.cast::<ID3D11Texture2D>().ok()?.GetDesc(&mut d) };
    }
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
            skip(super::OVER_LIMIT);
            return None;
        }
        s.target_bytes += bytes;
    }

    let mut source: ID3D11Resource = resource.clone();
    let sub = if bound.volume {
        mip
    } else {
        mip + slice * d.MipLevels
    };
    let mut source_sub = sub;
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
        unsafe {
            unpredicated(c, || {
                c.ResolveSubresource(&t, 0, &source, source_sub, view_format)
            })
        };
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
    let staging: windows::core::Result<ID3D11Resource> = if bound.volume {
        let desc = D3D11_TEXTURE3D_DESC {
            Width: width,
            Height: height,
            Depth: 1,
            MipLevels: 1,
            Format: d.Format,
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..Default::default()
        };
        let mut staging = None;
        unsafe { device.CreateTexture3D(&desc, None, Some(&mut staging)) }
            .and_then(|()| staging.expect("created texture").cast())
    } else {
        let mut staging = None;
        unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
            .and_then(|()| staging.expect("created texture").cast())
    };
    let staging = match staging {
        Ok(s) => s,
        Err(e) => {
            skip(&format!("no staging texture of format {} ({e})", d.Format.0));
            return None;
        }
    };
    let region = D3D11_BOX {
        left: 0,
        top: 0,
        front: slice,
        right: width,
        bottom: height,
        back: slice + 1,
    };
    unsafe {
        unpredicated(c, || {
            c.CopySubresourceRegion(
                &staging,
                0,
                0,
                0,
                0,
                &source,
                source_sub,
                bound.volume.then_some(&region as *const _),
            )
        })
    };
    let (view, resource) = view_ids(view);
    Some(Target {
        target,
        view,
        resource,
        width,
        height,
        format: format.0,
        sub,
        z: if bound.volume { slice } else { 0 },
        staging: Some(staging),
    })
}

impl Target {
    /// Which copy holds it: the same for each execution of the command list it was copied in.
    pub(super) fn copy_id(&self) -> Option<usize> {
        self.staging.as_ref().map(|s| s.as_raw() as usize)
    }

    /// Maps the copy (waiting for the GPU) and hands its rows, tightly packed, to the blob store. Returns the key and
    /// the bytes read.
    pub(super) unsafe fn read(&mut self, immediate: &ID3D11DeviceContext4) -> Option<(String, u64)> {
        let staging = self.staging.take()?;
        let bits = bits_per_pixel(DXGI_FORMAT(self.format)).expect("checked when copied");
        let row = (self.width * bits / 8) as usize;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { immediate.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.ok()?;
        let mut out = Vec::with_capacity(row * self.height as usize);
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
        let key = locum_capture::hash::key(&out);
        let bytes = out.len() as u64;
        let blobs = &crate::settings::settings().blobs;
        locum_capture::writer::blob(blobs, key.clone(), out, true);
        Some((key, bytes))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_is_written_as_colour() {
        let f = file_format(DXGI_FORMAT_D24_UNORM_S8_UINT, DXGI_FORMAT_R24G8_TYPELESS, true);
        assert_eq!(f, DXGI_FORMAT_R24_UNORM_X8_TYPELESS);
        assert_eq!(bits_per_pixel(f), Some(32));
        assert_eq!(bits_per_pixel(DXGI_FORMAT_R11G11B10_FLOAT), Some(32));
        assert_eq!(bits_per_pixel(DXGI_FORMAT_BC1_UNORM), None);
    }
}
