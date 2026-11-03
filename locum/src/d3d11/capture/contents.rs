//! Reading resource contents back: a GPU copy into a staging resource on some context, read later on the immediate
//! context into the canonical layout, hashed and handed to the blob store. Multisampled colour textures are resolved
//! first; depth textures are copied through their typeless format.

use std::collections::BTreeMap;

use locum_capture::format;
use locum_capture::hash::key;
use locum_capture::layout::{self, Sub};
use locum_capture::writer;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

use crate::d3d11::registry::Dims;
use crate::settings::settings;

/// A copy of a resource waiting to be read.
pub struct Readback {
    staging: ID3D11Resource,
    subs: Vec<Sub>,
    buffer: bool,
}

/// Bytes of staging copies the start of a capture has in flight at once.
const BATCH: u64 = 512 << 20;

/// The typeless format a depth format's texture can be copied through.
fn copyable(f: u32) -> u32 {
    match f {
        20 => 19,
        40 => 39,
        45 => 44,
        55 => 53,
        other => other,
    }
}

/// Records a copy of `resource` into a new staging resource on `c`.
///
/// # Safety
/// `c` must be a context of the device that made `resource`.
pub unsafe fn issue(
    c: &ID3D11DeviceContext4,
    resource: &ID3D11Resource,
    dims: &Dims,
) -> Result<Readback, String> {
    let device = unsafe { c.GetDevice() }.map_err(|e| format!("no device: {e}"))?;
    let subs = dims
        .subs()
        .ok_or_else(|| format!("format {} has no readable layout", format::name(dims.format)))?;
    if dims.kind == "buffer" {
        let d = D3D11_BUFFER_DESC {
            ByteWidth: dims.width,
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..Default::default()
        };
        let mut b = None;
        unsafe { device.CreateBuffer(&d, None, Some(&mut b)) }.map_err(|e| format!("staging buffer: {e}"))?;
        let b = b.ok_or("no staging buffer")?;
        unsafe { c.CopyResource(&b, resource) };
        return Ok(Readback {
            staging: b.cast().expect("a buffer is a resource"),
            subs,
            buffer: true,
        });
    }
    let staging: ID3D11Resource = match dims.kind {
        "texture2d" => {
            let t: ID3D11Texture2D = resource.cast().map_err(|e| e.to_string())?;
            let mut d = D3D11_TEXTURE2D_DESC::default();
            unsafe { t.GetDesc(&mut d) };
            let mut source: ID3D11Resource = resource.clone();
            if d.SampleDesc.Count > 1 {
                if format::is_depth(d.Format.0 as u32) {
                    return Err("multisampled depth cannot be resolved".into());
                }
                let typed = DXGI_FORMAT(format::readable(d.Format.0 as u32) as i32);
                let resolved = D3D11_TEXTURE2D_DESC {
                    MipLevels: 1,
                    Format: typed,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: 0,
                    CPUAccessFlags: 0,
                    MiscFlags: 0,
                    ..d
                };
                let mut r = None;
                unsafe { device.CreateTexture2D(&resolved, None, Some(&mut r)) }
                    .map_err(|e| format!("texture to resolve into: {e}"))?;
                let r = r.ok_or("no texture to resolve into")?;
                for slice in 0..d.ArraySize {
                    unsafe { c.ResolveSubresource(&r, slice, resource, slice, typed) };
                }
                source = r.cast().expect("a texture is a resource");
                d = resolved;
            }
            let sd = D3D11_TEXTURE2D_DESC {
                Format: DXGI_FORMAT(copyable(d.Format.0 as u32) as i32),
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..d
            };
            let mut s = None;
            unsafe { device.CreateTexture2D(&sd, None, Some(&mut s)) }.map_err(|e| {
                format!(
                    "staging texture of format {}: {e}",
                    format::name(sd.Format.0 as u32)
                )
            })?;
            let s = s.ok_or("no staging texture")?;
            unsafe { c.CopyResource(&s, &source) };
            s.cast().expect("a texture is a resource")
        }
        "texture3d" => {
            let t: ID3D11Texture3D = resource.cast().map_err(|e| e.to_string())?;
            let mut d = D3D11_TEXTURE3D_DESC::default();
            unsafe { t.GetDesc(&mut d) };
            let sd = D3D11_TEXTURE3D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..d
            };
            let mut s = None;
            unsafe { device.CreateTexture3D(&sd, None, Some(&mut s)) }
                .map_err(|e| format!("staging volume: {e}"))?;
            let s = s.ok_or("no staging volume")?;
            unsafe { c.CopyResource(&s, resource) };
            s.cast().expect("a texture is a resource")
        }
        "texture1d" => {
            let t: ID3D11Texture1D = resource.cast().map_err(|e| e.to_string())?;
            let mut d = D3D11_TEXTURE1D_DESC::default();
            unsafe { t.GetDesc(&mut d) };
            let sd = D3D11_TEXTURE1D_DESC {
                Format: DXGI_FORMAT(copyable(d.Format.0 as u32) as i32),
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..d
            };
            let mut s = None;
            unsafe { device.CreateTexture1D(&sd, None, Some(&mut s)) }
                .map_err(|e| format!("staging 1D: {e}"))?;
            let s = s.ok_or("no staging 1D texture")?;
            unsafe { c.CopyResource(&s, resource) };
            s.cast().expect("a texture is a resource")
        }
        other => return Err(format!("unknown resource kind {other}")),
    };
    Ok(Readback {
        staging,
        subs,
        buffer: false,
    })
}

/// The bytes of a staging copy in the canonical layout and their key, and each subresource's key. Waits for the GPU.
/// The bytes go to the blob store.
///
/// # Safety
/// `immediate` must be the immediate context of the device that made the copy.
pub unsafe fn read(immediate: &ID3D11DeviceContext4, rb: &Readback) -> Result<(String, Vec<String>), String> {
    let mut bytes = Vec::with_capacity(layout::total(&rb.subs) as usize);
    let mut hashes = Vec::with_capacity(rb.subs.len());
    for sub in &rb.subs {
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe { immediate.Map(&rb.staging, sub.index, D3D11_MAP_READ, 0, Some(&mut mapped)) }
            .map_err(|e| format!("map of subresource {}: {e}", sub.index))?;
        let start = bytes.len();
        let (row, slice) = if rb.buffer {
            (sub.row_bytes as usize, sub.slice_bytes() as usize)
        } else {
            (mapped.RowPitch as usize, mapped.DepthPitch as usize)
        };
        unsafe { layout::pack(sub, mapped.pData.cast(), row, slice, &mut bytes) };
        unsafe { immediate.Unmap(&rb.staging, sub.index) };
        hashes.push(key(&bytes[start..]));
    }
    let k = key(&bytes);
    let blobs = &settings().blobs;
    if !blobs.has(&k) {
        writer::blob(blobs, k.clone(), bytes, true);
    }
    Ok((k, hashes))
}

/// Copies and reads `todo` on the immediate context now, a batch at a time.
///
/// # Safety
/// `immediate` must be the immediate context of the device that made the resources.
pub unsafe fn read_now(
    immediate: &ID3D11DeviceContext4,
    todo: Vec<(u64, ID3D11Resource, Dims)>,
) -> BTreeMap<u64, Result<(String, Vec<String>), String>> {
    let mut out = BTreeMap::new();
    let mut batch: Vec<(u64, Result<Readback, String>)> = Vec::new();
    let mut batch_bytes = 0u64;
    let flush = |batch: &mut Vec<(u64, Result<Readback, String>)>,
                 out: &mut BTreeMap<u64, Result<(String, Vec<String>), String>>| {
        for (id, rb) in batch.drain(..) {
            out.insert(id, rb.and_then(|rb| unsafe { read(immediate, &rb) }));
        }
    };
    for (id, resource, dims) in todo {
        let size = dims.subs().map_or(0, |s| layout::total(&s));
        batch.push((id, unsafe { issue(immediate, &resource, &dims) }));
        batch_bytes += size;
        if batch_bytes >= BATCH {
            flush(&mut batch, &mut out);
            batch_bytes = 0;
        }
    }
    flush(&mut batch, &mut out);
    out
}
