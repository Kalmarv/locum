//! Numbers for the resources, views and state objects a capture records, and a description of each, written once
//! per capture. Resources are numbered by the registry when created; views and states the first time a capture sees
//! them. The number lives on the object (private data), so it survives the game's own reuse of addresses.

use core::ffi::c_void;

use locum_capture::db;
use serde_json::{Value, json};
use windows::Win32::Graphics::Direct3D11::*;
use windows::core::{GUID, Interface};

use super::{Tracked, state};
use crate::d3d11::registry::{self, id_on, next_id, set_id};

/// The private-data key of an input layout's elements (JSON) and the key of the bytecode it was created against.
pub const LAYOUT_KEY: GUID = GUID::from_u128(0x6c6f_6375_6d00_4a5e_9d1b_7a2f_8c3e_0003);

fn bytes_of<T>(t: &T) -> Vec<u8> {
    unsafe { core::slice::from_raw_parts((t as *const T).cast::<u8>(), size_of::<T>()) }.to_vec()
}

/// Whether `object` was already described in the current capture; marks it described. Returns its number.
fn number(object: &ID3D11DeviceChild, capture: u64) -> (u64, bool) {
    match id_on(object) {
        Some((id, c)) if c == capture => (id, true),
        Some((id, _)) => {
            set_id(object, id, capture);
            (id, false)
        }
        None => {
            let id = next_id();
            set_id(object, id, capture);
            (id, false)
        }
    }
}

/// The number of a resource, recording it as used by the current capture (holding a reference until the capture
/// ends, so it can be read back).
pub(super) fn resource_id(resource: &ID3D11Resource) -> u64 {
    let id = registry::id_of(resource);
    let mut s = state();
    if let std::collections::btree_map::Entry::Vacant(slot) = s.resources.entry(id) {
        let dims = registry::entry(id)
            .map(|e| e.dims)
            .or_else(|| registry::dims_of(resource));
        slot.insert(Tracked {
            resource: resource.clone(),
            dims: dims.unwrap_or_default(),
            written_before: false,
            first_write: None,
            claimed: false,
        });
    }
    id
}

/// The id of a buffer passed to an indirect draw (0 for null).
///
/// # Safety
/// `buffer` must be null or a live `ID3D11Buffer`.
pub unsafe fn buffer_id(buffer: *mut c_void) -> u64 {
    let Some(buffer) = (unsafe { ID3D11Resource::from_raw_borrowed(&buffer) }) else {
        return 0;
    };
    resource_id(buffer)
}

/// A view's number and its resource's number, describing the view the first time this capture sees it.
pub(super) fn view_ids(view: &ID3D11View) -> (u64, u64) {
    let resource = unsafe { view.GetResource() }.ok();
    let resource_id = resource.as_ref().map_or(0, resource_id);
    let child: ID3D11DeviceChild = view.cast().expect("a view is a device child");
    let capture = state().captures;
    let (id, known) = number(&child, capture);
    if !known {
        let row = describe_view(id, resource_id, view);
        state().views.push(row);
    }
    (id, resource_id)
}

fn describe_view(id: u64, resource: u64, view: &ID3D11View) -> db::View {
    unsafe {
        if let Ok(v) = view.cast::<ID3D11ShaderResourceView>() {
            let mut d = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
            v.GetDesc(&mut d);
            return db::View {
                id,
                kind: "srv",
                resource,
                format: d.Format.0 as u32,
                dimension: d.ViewDimension.0 as u32,
                desc: bytes_of(&d),
                json: srv_json(&d).to_string(),
            };
        }
        if let Ok(v) = view.cast::<ID3D11RenderTargetView>() {
            let mut d = D3D11_RENDER_TARGET_VIEW_DESC::default();
            v.GetDesc(&mut d);
            return db::View {
                id,
                kind: "rtv",
                resource,
                format: d.Format.0 as u32,
                dimension: d.ViewDimension.0 as u32,
                desc: bytes_of(&d),
                json: rtv_json(&d).to_string(),
            };
        }
        if let Ok(v) = view.cast::<ID3D11DepthStencilView>() {
            let mut d = D3D11_DEPTH_STENCIL_VIEW_DESC::default();
            v.GetDesc(&mut d);
            return db::View {
                id,
                kind: "dsv",
                resource,
                format: d.Format.0 as u32,
                dimension: d.ViewDimension.0 as u32,
                desc: bytes_of(&d),
                json: dsv_json(&d).to_string(),
            };
        }
        if let Ok(v) = view.cast::<ID3D11UnorderedAccessView>() {
            let mut d = D3D11_UNORDERED_ACCESS_VIEW_DESC::default();
            v.GetDesc(&mut d);
            return db::View {
                id,
                kind: "uav",
                resource,
                format: d.Format.0 as u32,
                dimension: d.ViewDimension.0 as u32,
                desc: bytes_of(&d),
                json: uav_json(&d).to_string(),
            };
        }
    }
    db::View {
        id,
        kind: "unknown",
        resource,
        ..Default::default()
    }
}

fn srv_json(d: &D3D11_SHADER_RESOURCE_VIEW_DESC) -> Value {
    let a = &d.Anonymous;
    let range = unsafe {
        match d.ViewDimension.0 {
            1 => {
                json!({"first_element": a.Buffer.Anonymous1.FirstElement, "elements": a.Buffer.Anonymous2.NumElements})
            }
            2 => json!({"first_mip": a.Texture1D.MostDetailedMip, "mips": a.Texture1D.MipLevels}),
            3 => json!({"first_mip": a.Texture1DArray.MostDetailedMip, "mips": a.Texture1DArray.MipLevels,
                "first_slice": a.Texture1DArray.FirstArraySlice, "slices": a.Texture1DArray.ArraySize}),
            4 => json!({"first_mip": a.Texture2D.MostDetailedMip, "mips": a.Texture2D.MipLevels}),
            5 => json!({"first_mip": a.Texture2DArray.MostDetailedMip, "mips": a.Texture2DArray.MipLevels,
                "first_slice": a.Texture2DArray.FirstArraySlice, "slices": a.Texture2DArray.ArraySize}),
            7 => {
                json!({"first_slice": a.Texture2DMSArray.FirstArraySlice, "slices": a.Texture2DMSArray.ArraySize})
            }
            8 => json!({"first_mip": a.Texture3D.MostDetailedMip, "mips": a.Texture3D.MipLevels}),
            9 => json!({"first_mip": a.TextureCube.MostDetailedMip, "mips": a.TextureCube.MipLevels}),
            10 => {
                json!({"first_mip": a.TextureCubeArray.MostDetailedMip, "mips": a.TextureCubeArray.MipLevels,
                "first_cube": a.TextureCubeArray.First2DArrayFace, "cubes": a.TextureCubeArray.NumCubes})
            }
            11 => json!({"first_element": a.BufferEx.FirstElement, "elements": a.BufferEx.NumElements,
                "flags": a.BufferEx.Flags}),
            _ => json!({}),
        }
    };
    json!({"format": d.Format.0, "dimension": SRV_DIMENSIONS.get(d.ViewDimension.0 as usize).copied().unwrap_or("?"), "range": range})
}

const SRV_DIMENSIONS: [&str; 12] = [
    "unknown",
    "buffer",
    "texture1d",
    "texture1darray",
    "texture2d",
    "texture2darray",
    "texture2dms",
    "texture2dmsarray",
    "texture3d",
    "texturecube",
    "texturecubearray",
    "bufferex",
];

fn rtv_json(d: &D3D11_RENDER_TARGET_VIEW_DESC) -> Value {
    let a = &d.Anonymous;
    let range = unsafe {
        match d.ViewDimension.0 {
            1 => {
                json!({"first_element": a.Buffer.Anonymous1.FirstElement, "elements": a.Buffer.Anonymous2.NumElements})
            }
            2 => json!({"mip": a.Texture1D.MipSlice}),
            3 => json!({"mip": a.Texture1DArray.MipSlice, "first_slice": a.Texture1DArray.FirstArraySlice,
                "slices": a.Texture1DArray.ArraySize}),
            4 => json!({"mip": a.Texture2D.MipSlice}),
            5 => json!({"mip": a.Texture2DArray.MipSlice, "first_slice": a.Texture2DArray.FirstArraySlice,
                "slices": a.Texture2DArray.ArraySize}),
            7 => {
                json!({"first_slice": a.Texture2DMSArray.FirstArraySlice, "slices": a.Texture2DMSArray.ArraySize})
            }
            8 => {
                json!({"mip": a.Texture3D.MipSlice, "first_w": a.Texture3D.FirstWSlice, "w": a.Texture3D.WSize})
            }
            _ => json!({}),
        }
    };
    let dims = [
        "unknown",
        "buffer",
        "texture1d",
        "texture1darray",
        "texture2d",
        "texture2darray",
        "texture2dms",
        "texture2dmsarray",
        "texture3d",
    ];
    json!({"format": d.Format.0, "dimension": dims.get(d.ViewDimension.0 as usize).copied().unwrap_or("?"), "range": range})
}

fn dsv_json(d: &D3D11_DEPTH_STENCIL_VIEW_DESC) -> Value {
    let a = &d.Anonymous;
    let range = unsafe {
        match d.ViewDimension.0 {
            1 => json!({"mip": a.Texture1D.MipSlice}),
            2 => json!({"mip": a.Texture1DArray.MipSlice, "first_slice": a.Texture1DArray.FirstArraySlice,
                "slices": a.Texture1DArray.ArraySize}),
            3 => json!({"mip": a.Texture2D.MipSlice}),
            4 => json!({"mip": a.Texture2DArray.MipSlice, "first_slice": a.Texture2DArray.FirstArraySlice,
                "slices": a.Texture2DArray.ArraySize}),
            6 => {
                json!({"first_slice": a.Texture2DMSArray.FirstArraySlice, "slices": a.Texture2DMSArray.ArraySize})
            }
            _ => json!({}),
        }
    };
    let dims = [
        "unknown",
        "texture1d",
        "texture1darray",
        "texture2d",
        "texture2darray",
        "texture2dms",
        "texture2dmsarray",
    ];
    json!({"format": d.Format.0, "dimension": dims.get(d.ViewDimension.0 as usize).copied().unwrap_or("?"),
        "read_only_depth": d.Flags & 1 != 0, "read_only_stencil": d.Flags & 2 != 0, "range": range})
}

fn uav_json(d: &D3D11_UNORDERED_ACCESS_VIEW_DESC) -> Value {
    let a = &d.Anonymous;
    let range = unsafe {
        match d.ViewDimension.0 {
            1 => json!({"first_element": a.Buffer.FirstElement, "elements": a.Buffer.NumElements,
                "flags": a.Buffer.Flags}),
            2 => json!({"mip": a.Texture1D.MipSlice}),
            3 => json!({"mip": a.Texture1DArray.MipSlice, "first_slice": a.Texture1DArray.FirstArraySlice,
                "slices": a.Texture1DArray.ArraySize}),
            4 => json!({"mip": a.Texture2D.MipSlice}),
            5 => json!({"mip": a.Texture2DArray.MipSlice, "first_slice": a.Texture2DArray.FirstArraySlice,
                "slices": a.Texture2DArray.ArraySize}),
            8 => {
                json!({"mip": a.Texture3D.MipSlice, "first_w": a.Texture3D.FirstWSlice, "w": a.Texture3D.WSize})
            }
            _ => json!({}),
        }
    };
    let dims = [
        "unknown",
        "buffer",
        "texture1d",
        "texture1darray",
        "texture2d",
        "texture2darray",
        "?",
        "?",
        "texture3d",
    ];
    json!({"format": d.Format.0, "dimension": dims.get(d.ViewDimension.0 as usize).copied().unwrap_or("?"), "range": range})
}

/// A state object's or input layout's number (None for none bound), describing it the first time this capture sees
/// it.
pub(super) fn state_id<T: Interface>(object: Option<T>, kind: &'static str) -> Option<u64> {
    let child = object.and_then(|o| o.cast::<ID3D11DeviceChild>().ok())?;
    let capture = state().captures;
    let (id, known) = number(&child, capture);
    if !known {
        let row = describe_state(id, kind, &child);
        state().states.push(row);
    }
    Some(id)
}

fn describe_state(id: u64, kind: &'static str, object: &ID3D11DeviceChild) -> db::State {
    let mut row = db::State {
        id,
        kind,
        ..Default::default()
    };
    unsafe {
        match kind {
            "rasterizer" => {
                if let Ok(r) = object.cast::<ID3D11RasterizerState2>() {
                    let mut d = D3D11_RASTERIZER_DESC2::default();
                    r.GetDesc2(&mut d);
                    row.desc = bytes_of(&d);
                    row.json = json!({"fill": d.FillMode.0, "cull": d.CullMode.0, "front_ccw": d.FrontCounterClockwise.as_bool(),
                        "depth_bias": d.DepthBias, "depth_bias_clamp": d.DepthBiasClamp,
                        "slope_scaled_depth_bias": d.SlopeScaledDepthBias, "depth_clip": d.DepthClipEnable.as_bool(),
                        "scissor": d.ScissorEnable.as_bool(), "multisample": d.MultisampleEnable.as_bool(),
                        "antialiased_line": d.AntialiasedLineEnable.as_bool(), "forced_sample_count": d.ForcedSampleCount,
                        "conservative": d.ConservativeRaster.0})
                    .to_string();
                } else if let Ok(r) = object.cast::<ID3D11RasterizerState>() {
                    let mut d = D3D11_RASTERIZER_DESC::default();
                    r.GetDesc(&mut d);
                    row.desc = bytes_of(&d);
                    row.json = json!({"fill": d.FillMode.0, "cull": d.CullMode.0, "front_ccw": d.FrontCounterClockwise.as_bool(),
                        "depth_bias": d.DepthBias, "scissor": d.ScissorEnable.as_bool(), "depth_clip": d.DepthClipEnable.as_bool()})
                    .to_string();
                }
            }
            "blend" => {
                if let Ok(b) = object.cast::<ID3D11BlendState1>() {
                    let mut d = D3D11_BLEND_DESC1::default();
                    b.GetDesc1(&mut d);
                    row.desc = bytes_of(&d);
                    let targets: Vec<Value> = d
                        .RenderTarget
                        .iter()
                        .map(|t| {
                            json!({"blend": t.BlendEnable.as_bool(), "logic": t.LogicOpEnable.as_bool(),
                                "src": t.SrcBlend.0, "dst": t.DestBlend.0, "op": t.BlendOp.0, "src_alpha": t.SrcBlendAlpha.0,
                                "dst_alpha": t.DestBlendAlpha.0, "op_alpha": t.BlendOpAlpha.0, "logic_op": t.LogicOp.0,
                                "write_mask": t.RenderTargetWriteMask})
                        })
                        .collect();
                    row.json = json!({"alpha_to_coverage": d.AlphaToCoverageEnable.as_bool(),
                        "independent": d.IndependentBlendEnable.as_bool(), "targets": targets})
                    .to_string();
                }
            }
            "depth_stencil" => {
                if let Ok(s) = object.cast::<ID3D11DepthStencilState>() {
                    let mut d = D3D11_DEPTH_STENCIL_DESC::default();
                    s.GetDesc(&mut d);
                    row.desc = bytes_of(&d);
                    let face = |f: &D3D11_DEPTH_STENCILOP_DESC| {
                        json!({"fail": f.StencilFailOp.0, "depth_fail": f.StencilDepthFailOp.0,
                            "pass": f.StencilPassOp.0, "func": f.StencilFunc.0})
                    };
                    row.json = json!({"depth": d.DepthEnable.as_bool(), "write": d.DepthWriteMask.0 != 0,
                        "func": d.DepthFunc.0, "stencil": d.StencilEnable.as_bool(), "read_mask": d.StencilReadMask,
                        "write_mask": d.StencilWriteMask, "front": face(&d.FrontFace), "back": face(&d.BackFace)})
                    .to_string();
                }
            }
            "sampler" => {
                if let Ok(s) = object.cast::<ID3D11SamplerState>() {
                    let mut d = D3D11_SAMPLER_DESC::default();
                    s.GetDesc(&mut d);
                    row.desc = bytes_of(&d);
                    row.json = json!({"filter": d.Filter.0, "address": [d.AddressU.0, d.AddressV.0, d.AddressW.0],
                        "mip_lod_bias": d.MipLODBias, "max_anisotropy": d.MaxAnisotropy, "compare": d.ComparisonFunc.0,
                        "border": d.BorderColor, "min_lod": d.MinLOD, "max_lod": d.MaxLOD})
                    .to_string();
                }
            }
            "input_layout" => {
                let mut size = 0u32;
                let _ = object.GetPrivateData(&LAYOUT_KEY, &mut size, None);
                if size > 0 {
                    let mut buf = vec![0u8; size as usize];
                    if object
                        .GetPrivateData(&LAYOUT_KEY, &mut size, Some(buf.as_mut_ptr().cast()))
                        .is_ok()
                        && let Ok(v) = serde_json::from_slice::<Value>(&buf)
                    {
                        row.signature = v["signature"].as_str().map(str::to_string);
                        row.json = v["elements"].to_string();
                    }
                }
            }
            _ => {}
        }
    }
    row
}

/// After `CreateInputLayout`: keeps the elements and the bytecode they were matched against on the layout, for
/// captures (the runtime has no way to read them back).
///
/// # Safety
/// `elements` must hold `count` element descriptions; `code` must point to `len` bytes; `out` is the call's.
pub unsafe fn input_layout_created(
    hr: windows::core::HRESULT,
    elements: *const D3D11_INPUT_ELEMENT_DESC,
    count: u32,
    code: *const c_void,
    len: usize,
    out: *mut *mut c_void,
) {
    if hr.is_err() || out.is_null() || unsafe { (*out).is_null() } {
        return;
    }
    let Some(layout) = (unsafe { ID3D11DeviceChild::from_raw_borrowed(&*out) }) else {
        return;
    };
    let elements: Vec<Value> = (0..count as usize)
        .map(|i| unsafe {
            let e = &*elements.add(i);
            let semantic = if e.SemanticName.is_null() {
                String::new()
            } else {
                e.SemanticName.to_string().unwrap_or_default()
            };
            json!({"semantic": semantic, "index": e.SemanticIndex, "format": e.Format.0, "slot": e.InputSlot,
                "offset": e.AlignedByteOffset, "per_instance": e.InputSlotClass.0 == 1,
                "step": e.InstanceDataStepRate})
        })
        .collect();
    let signature = if code.is_null() || len == 0 {
        None
    } else {
        let bytes = unsafe { core::slice::from_raw_parts(code.cast::<u8>(), len) }.to_vec();
        let key = locum_capture::hash::key(&bytes);
        let blobs = &crate::settings::settings().blobs;
        if !blobs.has(&key) {
            locum_capture::writer::blob(blobs, key.clone(), bytes, true);
        }
        Some(key)
    };
    let text = json!({"elements": elements, "signature": signature}).to_string();
    let _ = unsafe { layout.SetPrivateData(&LAYOUT_KEY, text.len() as u32, Some(text.as_ptr().cast())) };
}
