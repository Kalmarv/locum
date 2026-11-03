//! Reading a resource back: a GPU copy into a staging resource on some context, mapped later on the immediate context
//! and packed into the canonical layout (`locum_capture::layout`). Multisampled colour textures are resolved first;
//! depth textures are copied through their typeless format.

use locum_capture::format;
use locum_capture::layout::{self, Sub};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

/// A copy of a resource waiting to be read.
#[derive(Clone)]
pub struct Readback {
    staging: ID3D11Resource,
    pub subs: Vec<Sub>,
    buffer: bool,
}

impl Readback {
    /// Which copy this is: the same for every clone of it (a copy recorded in a command list is one copy however
    /// often the list runs).
    pub fn copy_id(&self) -> usize {
        self.staging.as_raw() as usize
    }
}

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

/// A resource's subresources in the canonical layout, from its description.
pub fn subs(resource: &ID3D11Resource) -> Result<Vec<Sub>, String> {
    let no_layout = |f: DXGI_FORMAT| format!("format {} has no readable layout", format::name(f.0 as u32));
    unsafe {
        if let Ok(b) = resource.cast::<ID3D11Buffer>() {
            let mut d = D3D11_BUFFER_DESC::default();
            b.GetDesc(&mut d);
            return Ok(layout::buffer(d.ByteWidth));
        }
        if let Ok(t) = resource.cast::<ID3D11Texture2D>() {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            t.GetDesc(&mut d);
            return layout::texture(d.Format.0 as u32, d.Width, d.Height, 1, d.MipLevels, d.ArraySize)
                .ok_or_else(|| no_layout(d.Format));
        }
        if let Ok(t) = resource.cast::<ID3D11Texture3D>() {
            let mut d = D3D11_TEXTURE3D_DESC::default();
            t.GetDesc(&mut d);
            return layout::texture(d.Format.0 as u32, d.Width, d.Height, d.Depth, d.MipLevels, 1)
                .ok_or_else(|| no_layout(d.Format));
        }
        if let Ok(t) = resource.cast::<ID3D11Texture1D>() {
            let mut d = D3D11_TEXTURE1D_DESC::default();
            t.GetDesc(&mut d);
            return layout::texture(d.Format.0 as u32, d.Width, 1, 1, d.MipLevels, d.ArraySize)
                .ok_or_else(|| no_layout(d.Format));
        }
    }
    Err("not a buffer or texture".into())
}

/// The bytes a readback of `resource` holds: its subresources in the canonical layout.
pub fn bytes(resource: &ID3D11Resource) -> Result<u64, String> {
    Ok(subs(resource)?.iter().map(|s| s.size).sum())
}

/// Records a copy of `resource` into a new staging resource on `c`, whatever predicate `c` has set.
///
/// # Safety
/// `c` must be a context of the device that made `resource`.
pub unsafe fn issue(c: &ID3D11DeviceContext, resource: &ID3D11Resource) -> Result<Readback, String> {
    unsafe { crate::predication::unpredicated(c, || issue_now(c, resource)) }
}

unsafe fn issue_now(c: &ID3D11DeviceContext, resource: &ID3D11Resource) -> Result<Readback, String> {
    let device = unsafe { c.GetDevice() }.map_err(|e| format!("no device: {e}"))?;
    let subs = subs(resource)?;
    if let Ok(b) = resource.cast::<ID3D11Buffer>() {
        let mut d = D3D11_BUFFER_DESC::default();
        unsafe { b.GetDesc(&mut d) };
        let sd = D3D11_BUFFER_DESC {
            ByteWidth: d.ByteWidth,
            Usage: D3D11_USAGE_STAGING,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            ..Default::default()
        };
        let mut s = None;
        unsafe { device.CreateBuffer(&sd, None, Some(&mut s)) }
            .map_err(|e| format!("staging buffer: {e}"))?;
        let s = s.ok_or("no staging buffer")?;
        unsafe { c.CopyResource(&s, resource) };
        return Ok(Readback {
            staging: s.cast().expect("a buffer is a resource"),
            subs,
            buffer: true,
        });
    }
    let staging: ID3D11Resource = if let Ok(t) = resource.cast::<ID3D11Texture2D>() {
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
    } else if let Ok(t) = resource.cast::<ID3D11Texture3D>() {
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
    } else if let Ok(t) = resource.cast::<ID3D11Texture1D>() {
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
        unsafe { device.CreateTexture1D(&sd, None, Some(&mut s)) }.map_err(|e| format!("staging 1D: {e}"))?;
        let s = s.ok_or("no staging 1D texture")?;
        unsafe { c.CopyResource(&s, resource) };
        s.cast().expect("a texture is a resource")
    } else {
        return Err("not a buffer or texture".into());
    };
    Ok(Readback {
        staging,
        subs,
        buffer: false,
    })
}

impl Readback {
    /// The bytes it holds in the canonical layout. Waits for the GPU.
    ///
    /// # Safety
    /// `immediate` must be the immediate context of the device that made the copy.
    pub unsafe fn read(&self, immediate: &ID3D11DeviceContext) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::with_capacity(layout::total(&self.subs) as usize);
        for sub in &self.subs {
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe { immediate.Map(&self.staging, sub.index, D3D11_MAP_READ, 0, Some(&mut mapped)) }
                .map_err(|e| format!("map of subresource {}: {e}", sub.index))?;
            let (row, slice) = if self.buffer {
                (sub.row_bytes as usize, sub.slice_bytes() as usize)
            } else {
                (mapped.RowPitch as usize, mapped.DepthPitch as usize)
            };
            unsafe { layout::pack(sub, mapped.pData.cast(), row, slice, &mut bytes) };
            unsafe { immediate.Unmap(&self.staging, sub.index) };
        }
        Ok(bytes)
    }
}
