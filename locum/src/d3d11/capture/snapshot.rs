//! The state a draw or dispatch runs with, read from the real context with its getters, and the GPU copies of its
//! constant buffers.

use locum_capture::db;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::{BOOL, Interface};

use super::ids::{resource_id, state_id, view_ids};
use super::{CbCopy, Draw, state};
use crate::d3d11::shaders::{self, Stage};
use crate::d3d11::stages;
use locum_d3d11::predication::unpredicated;

/// Staging buffers for constant-buffer copies: 32 MiB each, at most 16 (512 MiB) per capture.
pub(super) const CHUNK: u32 = 32 << 20;
const MAX_CHUNKS: usize = 16;
/// The most of one bound constant-buffer range copied (the whole D3D11 limit is 64 KiB: 4096 constants).
const MAX_CB_COPY: u32 = 65536;
/// UAV slots: 64 on Direct3D 11.1.
const UAV_SLOTS: u32 = D3D11_1_UAV_SLOT_COUNT;

fn stage_index(stage: Stage) -> usize {
    match stage {
        Stage::Vertex => 0,
        Stage::Hull => 1,
        Stage::Domain => 2,
        Stage::Geometry => 3,
        Stage::Pixel => 4,
        Stage::Compute => 5,
    }
}

fn binding(stage: &'static str, kind: &'static str, slot: u32) -> db::Binding {
    db::Binding {
        stage,
        kind,
        slot,
        ..Default::default()
    }
}

/// What a draw reads and writes, by resource number.
#[derive(Default)]
pub(super) struct Touches {
    pub reads: Vec<u64>,
    pub writes: Vec<u64>,
}

/// Reads the state a draw runs with from the real context into `draw`, and the resources it reads and writes.
pub(super) unsafe fn snapshot(c: &ID3D11DeviceContext4, compute: bool, draw: &mut Draw) -> Touches {
    let mut t = Touches::default();
    let stages: &[Stage] = if compute {
        &[Stage::Compute]
    } else {
        &Stage::GRAPHICS
    };
    for &stage in stages {
        let Some(shader) = (unsafe { stages::shader(c, stage) }) else {
            continue;
        };
        let name = stage.short();
        let hash = unsafe { shaders::hash_of(shader.as_raw()) };
        draw.order_dependent |= hash.is_some_and(shaders::order_dependent);
        draw.shaders[stage_index(stage)] =
            Some(hash.map_or_else(|| "unknown".into(), |h| format!("{h:016x}")));
        for (slot, b) in unsafe { stages::constant_buffers(c, stage) }
            .into_iter()
            .enumerate()
        {
            let Some(buffer) = b.buffer else { continue };
            let resource: ID3D11Resource = buffer.cast().expect("a buffer is a resource");
            let id = resource_id(&resource);
            t.reads.push(id);
            let mut row = binding(name, "cb", slot as u32);
            row.resource = Some(id);
            row.offset = Some(u64::from(b.first) * 16);
            row.size = Some(u64::from(b.count) * 16);
            draw.cb_copies.push(CbCopy {
                binding: draw.bindings.len(),
                source: Some(buffer),
                first: b.first,
                count: b.count,
                copy: None,
            });
            draw.bindings.push(row);
        }
        for (slot, view) in unsafe { stages::shader_resources(c, stage) }.iter().enumerate() {
            let Some(view) = view else { continue };
            let v: ID3D11View = view.cast().expect("a shader resource view is a view");
            let (id, resource) = view_ids(&v);
            t.reads.push(resource);
            let mut row = binding(name, "srv", slot as u32);
            row.view = Some(id);
            row.resource = Some(resource);
            draw.bindings.push(row);
        }
        for (slot, sampler) in unsafe { stages::samplers(c, stage) }.into_iter().enumerate() {
            let Some(id) = state_id(sampler, "sampler") else {
                continue;
            };
            let mut row = binding(name, "sampler", slot as u32);
            row.state = Some(id);
            draw.bindings.push(row);
        }
    }
    unsafe {
        let D3D_PRIMITIVE_TOPOLOGY(topology) = c.IAGetPrimitiveTopology();
        draw.topology = topology;
        let mut predicate = None;
        let mut value = BOOL::default();
        c.GetPredication(Some(&mut predicate), Some(&mut value));
        if predicate.is_some() {
            draw.predicate = state_id(predicate, "predicate");
            draw.predicate_value = Some(value.as_bool());
        }
    }
    if compute {
        let mut uavs: [Option<ID3D11UnorderedAccessView>; UAV_SLOTS as usize] =
            [const { None }; UAV_SLOTS as usize];
        unsafe { c.CSGetUnorderedAccessViews(0, Some(&mut uavs)) };
        uav_bindings(draw, &mut t, "cs", &uavs);
        return t;
    }
    unsafe {
        let mut rtvs: [Option<ID3D11RenderTargetView>; 8] = Default::default();
        let mut dsv = None;
        c.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv));
        for (slot, view) in rtvs.iter().enumerate() {
            let Some(view) = view else { continue };
            let v: ID3D11View = view.cast().expect("a render target view is a view");
            let (id, resource) = view_ids(&v);
            t.reads.push(resource);
            t.writes.push(resource);
            let mut row = binding("om", "rtv", slot as u32);
            row.view = Some(id);
            row.resource = Some(resource);
            draw.bindings.push(row);
        }
        if let Some(view) = dsv {
            let v: ID3D11View = view.cast().expect("a depth stencil view is a view");
            let (id, resource) = view_ids(&v);
            t.reads.push(resource);
            t.writes.push(resource);
            let mut row = binding("om", "dsv", 0);
            row.view = Some(id);
            row.resource = Some(resource);
            draw.bindings.push(row);
        }
        let mut uavs: [Option<ID3D11UnorderedAccessView>; UAV_SLOTS as usize] =
            [const { None }; UAV_SLOTS as usize];
        c.OMGetRenderTargetsAndUnorderedAccessViews(None, None, 0, Some(&mut uavs));
        uav_bindings(draw, &mut t, "om", &uavs);

        draw.layout = state_id(c.IAGetInputLayout().ok(), "input_layout");
        let mut vbs: [Option<ID3D11Buffer>; 32] = Default::default();
        let mut strides = [0u32; 32];
        let mut offsets = [0u32; 32];
        c.IAGetVertexBuffers(
            0,
            32,
            Some(vbs.as_mut_ptr()),
            Some(strides.as_mut_ptr()),
            Some(offsets.as_mut_ptr()),
        );
        for (slot, vb) in vbs.iter().enumerate() {
            let Some(vb) = vb else { continue };
            let resource: ID3D11Resource = vb.cast().expect("a buffer is a resource");
            let id = resource_id(&resource);
            t.reads.push(id);
            let mut row = binding("ia", "vb", slot as u32);
            row.resource = Some(id);
            row.offset = Some(u64::from(offsets[slot]));
            row.stride = Some(strides[slot]);
            draw.bindings.push(row);
        }
        let mut ib = None;
        let mut ib_format = DXGI_FORMAT::default();
        let mut ib_offset = 0u32;
        c.IAGetIndexBuffer(Some(&mut ib), Some(&mut ib_format), Some(&mut ib_offset));
        if let Some(ib) = ib {
            let resource: ID3D11Resource = ib.cast().expect("a buffer is a resource");
            let id = resource_id(&resource);
            t.reads.push(id);
            let mut row = binding("ia", "ib", 0);
            row.resource = Some(id);
            row.offset = Some(u64::from(ib_offset));
            row.format = Some(ib_format.0 as u32);
            draw.bindings.push(row);
        }
        let mut so: [Option<ID3D11Buffer>; 4] = Default::default();
        c.SOGetTargets(Some(&mut so));
        for (slot, b) in so.iter().enumerate() {
            let Some(b) = b else { continue };
            let resource: ID3D11Resource = b.cast().expect("a buffer is a resource");
            let id = resource_id(&resource);
            t.writes.push(id);
            let mut row = binding("so", "so", slot as u32);
            row.resource = Some(id);
            draw.bindings.push(row);
        }

        draw.rasterizer = state_id(c.RSGetState().ok(), "rasterizer");
        let mut blend = None;
        let mut factor = [0f32; 4];
        let mut mask = 0u32;
        c.OMGetBlendState(Some(&mut blend), Some(&mut factor), Some(&mut mask));
        draw.blend = state_id(blend, "blend");
        draw.blend_factor = Some(factor);
        draw.sample_mask = Some(mask);
        let mut depth = None;
        let mut stencil_ref = 0u32;
        c.OMGetDepthStencilState(Some(&mut depth), Some(&mut stencil_ref));
        draw.depth_stencil = state_id(depth, "depth_stencil");
        draw.stencil_ref = Some(stencil_ref);

        let mut count = D3D11_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE;
        let mut viewports = [D3D11_VIEWPORT::default(); 16];
        c.RSGetViewports(&mut count, Some(viewports.as_mut_ptr()));
        for (i, v) in viewports[..count.min(16) as usize].iter().enumerate() {
            draw.viewports.push(db::Viewport {
                draw: 0,
                i: i as u32,
                x: v.TopLeftX,
                y: v.TopLeftY,
                width: v.Width,
                height: v.Height,
                min_depth: v.MinDepth,
                max_depth: v.MaxDepth,
            });
        }
        let mut count = D3D11_VIEWPORT_AND_SCISSORRECT_OBJECT_COUNT_PER_PIPELINE;
        let mut rects = [RECT::default(); 16];
        c.RSGetScissorRects(&mut count, Some(rects.as_mut_ptr()));
        for (i, r) in rects[..count.min(16) as usize].iter().enumerate() {
            draw.scissors.push(db::Scissor {
                draw: 0,
                i: i as u32,
                left: r.left,
                top: r.top,
                right: r.right,
                bottom: r.bottom,
            });
        }
    }
    t
}

fn uav_bindings(
    draw: &mut Draw,
    t: &mut Touches,
    stage: &'static str,
    uavs: &[Option<ID3D11UnorderedAccessView>],
) {
    for (slot, view) in uavs.iter().enumerate() {
        let Some(view) = view else { continue };
        let v: ID3D11View = view.cast().expect("an unordered access view is a view");
        let (id, resource) = view_ids(&v);
        t.reads.push(resource);
        t.writes.push(resource);
        let mut row = binding(stage, "uav", slot as u32);
        row.view = Some(id);
        row.resource = Some(resource);
        draw.bindings.push(row);
    }
}

/// Records a GPU copy of the bound range of a constant buffer into the staging pool, on context `c`.
pub(super) unsafe fn copy_cb(c: &ID3D11DeviceContext4, cb: &mut CbCopy) -> Option<(u32, u32, u32)> {
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
    unsafe {
        unpredicated(c, || {
            c.CopySubresourceRegion(&staging, 0, offset, 0, 0, &source, 0, Some(&source_box))
        })
    };
    Some((chunk, offset, bytes))
}
