//! The state a draw runs with, read from the real context, and the GPU copies of its constant buffers.

use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::Interface;

use super::ids::{resource_id, state_id, view_ids};
use super::{CbBind, Draw, state};
use crate::shaders::{self, Stage};

/// Staging buffers for constant-buffer copies: 32 MiB each, at most 16 (512 MiB) per capture.
pub(super) const CHUNK: u32 = 32 << 20;
const MAX_CHUNKS: usize = 16;
/// The most of one bound constant-buffer range copied (the whole D3D11 limit is 64 KiB: 4096 constants).
const MAX_CB_COPY: u32 = 65536;

const CB_SLOTS: u32 = D3D11_COMMONSHADER_CONSTANT_BUFFER_API_SLOT_COUNT;
const SRV_SLOTS: usize = D3D11_COMMONSHADER_INPUT_RESOURCE_SLOT_COUNT as usize;

/// What is bound to one shader stage.
struct StageBindings {
    shader: Option<ID3D11DeviceChild>,
    buffers: [Option<ID3D11Buffer>; CB_SLOTS as usize],
    firsts: [u32; CB_SLOTS as usize],
    counts: [u32; CB_SLOTS as usize],
    views: [Option<ID3D11ShaderResourceView>; SRV_SLOTS],
}

/// Reads one stage's shader, constant buffers (with their ranges) and shader resource views.
unsafe fn stage_bindings(c: &ID3D11DeviceContext4, stage: Stage) -> StageBindings {
    let mut b = StageBindings {
        shader: None,
        buffers: Default::default(),
        firsts: [0; CB_SLOTS as usize],
        counts: [0; CB_SLOTS as usize],
        views: [const { None }; SRV_SLOTS],
    };
    let (buffers, firsts, counts) = (
        b.buffers.as_mut_ptr(),
        b.firsts.as_mut_ptr(),
        b.counts.as_mut_ptr(),
    );
    macro_rules! read {
        ($($stage:ident => $shader:ident, $cbs:ident, $srvs:ident;)*) => {
            match stage {
                $(Stage::$stage => unsafe {
                    let mut shader = None;
                    c.$shader(&mut shader, None, None);
                    b.shader = shader.map(|s| s.cast().expect("a shader is a device child"));
                    if b.shader.is_some() {
                        c.$cbs(0, CB_SLOTS, Some(buffers), Some(firsts), Some(counts));
                        c.$srvs(0, Some(&mut b.views));
                    }
                })*
            }
        };
    }
    read! {
        Vertex => VSGetShader, VSGetConstantBuffers1, VSGetShaderResources;
        Hull => HSGetShader, HSGetConstantBuffers1, HSGetShaderResources;
        Domain => DSGetShader, DSGetConstantBuffers1, DSGetShaderResources;
        Geometry => GSGetShader, GSGetConstantBuffers1, GSGetShaderResources;
        Pixel => PSGetShader, PSGetConstantBuffers1, PSGetShaderResources;
        Compute => CSGetShader, CSGetConstantBuffers1, CSGetShaderResources;
    }
    b
}

/// Reads the state a draw runs with from the real context.
pub(super) unsafe fn snapshot(c: &ID3D11DeviceContext4, compute: bool, draw: &mut Draw) {
    let stages: &[Stage] = if compute {
        &[Stage::Compute]
    } else {
        &Stage::GRAPHICS
    };
    for &stage in stages {
        let b = unsafe { stage_bindings(c, stage) };
        let Some(shader) = b.shader else { continue };
        let name = stage.short();
        let hash = unsafe { shaders::hash_of(shader.as_raw()) };
        draw.shaders.insert(name, hash.map(|h| format!("{h:016x}")));
        for (slot, buffer) in b.buffers.iter().enumerate() {
            let Some(buffer) = buffer else { continue };
            let resource: ID3D11Resource = buffer.cast().expect("a buffer is a resource");
            draw.cbs.push(CbBind {
                stage: name,
                slot: slot as u32,
                buffer: unsafe { resource_id(&resource) },
                first: b.firsts[slot],
                count: b.counts[slot],
                content: None,
                source: Some(buffer.clone()),
                copy: None,
            });
        }
        for (slot, view) in b.views.iter().enumerate() {
            let Some(view) = view else { continue };
            let mut d = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
            unsafe { view.GetDesc(&mut d) };
            let v: ID3D11View = view.cast().expect("a shader resource view is a view");
            let (id, resource) = unsafe { view_ids(&v, "srv", d.Format, d.ViewDimension.0) };
            draw.srvs.push((name, slot as u32, id, resource));
        }
    }
    if compute {
        return;
    }
    unsafe {
        let mut rtvs: [Option<ID3D11RenderTargetView>; 8] = Default::default();
        let mut dsv = None;
        c.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv));
        for (slot, view) in rtvs.iter().enumerate() {
            let Some(view) = view else { continue };
            let mut d = D3D11_RENDER_TARGET_VIEW_DESC::default();
            view.GetDesc(&mut d);
            let v: ID3D11View = view.cast().expect("a render target view is a view");
            let (id, resource) = view_ids(&v, "rtv", d.Format, d.ViewDimension.0);
            draw.rtvs.push((slot as u32, id, resource));
        }
        if let Some(view) = dsv {
            let mut d = D3D11_DEPTH_STENCIL_VIEW_DESC::default();
            view.GetDesc(&mut d);
            let v: ID3D11View = view.cast().expect("a depth stencil view is a view");
            draw.dsv = Some(view_ids(&v, "dsv", d.Format, d.ViewDimension.0));
        }

        let D3D_PRIMITIVE_TOPOLOGY(t) = c.IAGetPrimitiveTopology();
        draw.topology = t;
        draw.layout = state_id(c.IAGetInputLayout().ok(), "input_layout");
        let mut vbs: [Option<ID3D11Buffer>; 16] = Default::default();
        let mut strides = [0u32; 16];
        let mut offsets = [0u32; 16];
        c.IAGetVertexBuffers(
            0,
            16,
            Some(vbs.as_mut_ptr()),
            Some(strides.as_mut_ptr()),
            Some(offsets.as_mut_ptr()),
        );
        for (slot, vb) in vbs.iter().enumerate() {
            let Some(vb) = vb else { continue };
            let resource: ID3D11Resource = vb.cast().expect("a buffer is a resource");
            draw.vbs
                .push((slot as u32, resource_id(&resource), strides[slot], offsets[slot]));
        }
        let mut ib = None;
        let mut ib_format = DXGI_FORMAT::default();
        let mut ib_offset = 0u32;
        c.IAGetIndexBuffer(Some(&mut ib), Some(&mut ib_format), Some(&mut ib_offset));
        if let Some(ib) = ib {
            let resource: ID3D11Resource = ib.cast().expect("a buffer is a resource");
            draw.ib = Some((resource_id(&resource), ib_format.0, ib_offset));
        }

        draw.raster = state_id(c.RSGetState().ok(), "rasterizer");
        let mut blend = None;
        c.OMGetBlendState(Some(&mut blend), None, None);
        draw.blend = state_id(blend, "blend");
        let mut depth = None;
        c.OMGetDepthStencilState(Some(&mut depth), None);
        draw.depth = state_id(depth, "depth_stencil");
    }
}

/// Records a GPU copy of the bound range of a constant buffer into the staging pool, on context `c`.
pub(super) unsafe fn copy_cb(c: &ID3D11DeviceContext4, cb: &mut CbBind) -> Option<(u32, u32, u32)> {
    let source = cb.source.take()?;
    let mut desc = D3D11_BUFFER_DESC::default();
    unsafe { source.GetDesc(&mut desc) };
    let start = cb.first.saturating_mul(16).min(desc.ByteWidth);
    let wanted = if cb.count == 0 {
        desc.ByteWidth
    } else {
        cb.count.saturating_mul(16)
    };
    let bytes = wanted.min(desc.ByteWidth - start).min(MAX_CB_COPY);
    if bytes == 0 {
        return None;
    }
    let (chunk, offset, staging) = {
        let mut s = state();
        let aligned = (bytes + 15) & !15;
        if s.staging.is_empty() || s.used + aligned > CHUNK {
            if s.staging.len() >= MAX_CHUNKS {
                s.copy_failures += 1;
                return None;
            }
            let device = unsafe { c.GetDevice() }.ok()?;
            let d = D3D11_BUFFER_DESC {
                ByteWidth: CHUNK,
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut buffer = None;
            if unsafe { device.CreateBuffer(&d, None, Some(&mut buffer)) }.is_err() {
                s.copy_failures += 1;
                return None;
            }
            s.staging.push(buffer?);
            s.used = 0;
        }
        let chunk = s.staging.len() as u32 - 1;
        let offset = s.used;
        s.used += aligned;
        (chunk, offset, s.staging[chunk as usize].clone())
    };
    let source_box = D3D11_BOX {
        left: start,
        top: 0,
        front: 0,
        right: start + bytes,
        bottom: 1,
        back: 1,
    };
    unsafe { c.CopySubresourceRegion(&staging, 0, offset, 0, 0, &source, 0, Some(&source_box)) };
    Some((chunk, offset, bytes))
}
