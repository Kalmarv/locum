//! Pipeline states from a Direct3D 12 capture's descriptions, made again through a pipeline state stream: a stream
//! description (`kind: "stream"`) as its recorded subobjects, a graphics or compute description as the subobjects
//! that say the same. Shaders come from the capture's blobs; a rule can put another shader at a stage.

use std::ffi::CString;

use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR};

use crate::open::Capture;

/// Subobject types (`D3D12_PIPELINE_STATE_SUBOBJECT_TYPE`).
const ROOT: u32 = 0;
const VS: u32 = 1;
const PS: u32 = 2;
const DS: u32 = 3;
const HS: u32 = 4;
const GS: u32 = 5;
const CS: u32 = 6;
const STREAM_OUTPUT: u32 = 7;
const BLEND: u32 = 8;
const SAMPLE_MASK: u32 = 9;
const RASTERIZER: u32 = 10;
const DEPTH_STENCIL: u32 = 11;
const INPUT_LAYOUT: u32 = 12;
const STRIP_CUT: u32 = 13;
const TOPOLOGY: u32 = 14;
const RT_FORMATS: u32 = 15;
const DS_FORMAT: u32 = 16;
const SAMPLE_DESC: u32 = 17;
const NODE_MASK: u32 = 18;
const CACHED: u32 = 19;
const FLAGS: u32 = 20;
const DEPTH_STENCIL1: u32 = 21;
const AS: u32 = 24;
const MS: u32 = 25;

/// The subobject a stage's shader goes in, by the stage numbers rules use (vs hs ds gs ps cs).
pub fn shader_type(stage: usize) -> u32 {
    [VS, HS, DS, GS, PS, CS][stage]
}

/// A pipeline state's subobjects as (type, recorded value).
pub fn subobjects(desc: &Value) -> Vec<(u32, Value)> {
    match desc["kind"].as_str() {
        Some("stream") => desc["subobjects"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| (s["type"].as_u64().unwrap_or(u64::MAX) as u32, s["value"].clone()))
            .collect(),
        Some("compute") => vec![
            (ROOT, desc["root"].clone()),
            (CS, desc["shaders"][5].clone()),
            (NODE_MASK, desc["node_mask"].clone()),
            (FLAGS, desc["flags"].clone()),
        ],
        _ => {
            let mut out = vec![(ROOT, desc["root"].clone())];
            for s in desc["shaders"].as_array().into_iter().flatten() {
                let t = match s["stage"].as_str() {
                    Some("vs") => VS,
                    Some("hs") => HS,
                    Some("ds") => DS,
                    Some("gs") => GS,
                    Some("ps") => PS,
                    _ => continue,
                };
                out.push((t, s.clone()));
            }
            if desc["stream_output"]["entries"]
                .as_array()
                .is_some_and(|e| !e.is_empty())
            {
                out.push((STREAM_OUTPUT, desc["stream_output"].clone()));
            }
            let rt = serde_json::json!({
                "NumRenderTargets": desc["render_target_count"],
                "RTFormats": desc["rt_formats"],
            });
            let sample =
                serde_json::json!({"count": desc["sample_count"], "quality": desc["sample_quality"]});
            out.extend([
                (BLEND, desc["blend"].clone()),
                (SAMPLE_MASK, desc["sample_mask"].clone()),
                (RASTERIZER, desc["rasterizer"].clone()),
                (DEPTH_STENCIL, desc["depth_stencil"].clone()),
                (INPUT_LAYOUT, desc["input_layout"].clone()),
                (STRIP_CUT, desc["strip_cut"].clone()),
                (TOPOLOGY, desc["topology_type"].clone()),
                (RT_FORMATS, rt),
                (DS_FORMAT, desc["ds_format"].clone()),
                (SAMPLE_DESC, sample),
                (NODE_MASK, desc["node_mask"].clone()),
                (FLAGS, desc["flags"].clone()),
            ]);
            out
        }
    }
}

/// The bytes of a pipeline state stream, each subobject its type and then its structure, aligned to a pointer;
/// and what its pointers point at, kept alive with it.
#[derive(Default)]
struct Stream {
    words: Vec<u64>,
    len: usize,
    shaders: Vec<Vec<u8>>,
    names: Vec<CString>,
    elements: Vec<Vec<D3D12_INPUT_ELEMENT_DESC>>,
}

impl Stream {
    fn push<T: Copy>(&mut self, ty: u32, value: T) {
        let start = self.len.next_multiple_of(8);
        let inner = (start + 4).next_multiple_of(align_of::<T>());
        let end = (inner + size_of::<T>()).next_multiple_of(8);
        self.words.resize(end / 8, 0);
        let base = self.words.as_mut_ptr().cast::<u8>();
        unsafe {
            base.add(start).cast::<u32>().write(ty);
            base.add(inner).cast::<T>().write_unaligned(value);
        }
        self.len = end;
    }
}

fn u(v: &Value, key: &str) -> u32 {
    v[key].as_u64().map_or(0, |n| n as u32)
}

fn b(v: &Value, key: &str) -> windows::core::BOOL {
    (v[key] == true).into()
}

fn blend(v: &Value) -> D3D12_BLEND_DESC {
    let mut d = D3D12_BLEND_DESC {
        AlphaToCoverageEnable: b(v, "AlphaToCoverageEnable"),
        IndependentBlendEnable: b(v, "IndependentBlendEnable"),
        ..Default::default()
    };
    for (i, t) in v["RenderTarget"]
        .as_array()
        .into_iter()
        .flatten()
        .take(8)
        .enumerate()
    {
        d.RenderTarget[i] = D3D12_RENDER_TARGET_BLEND_DESC {
            BlendEnable: b(t, "BlendEnable"),
            LogicOpEnable: b(t, "LogicOpEnable"),
            SrcBlend: D3D12_BLEND(u(t, "SrcBlend") as i32),
            DestBlend: D3D12_BLEND(u(t, "DestBlend") as i32),
            BlendOp: D3D12_BLEND_OP(u(t, "BlendOp") as i32),
            SrcBlendAlpha: D3D12_BLEND(u(t, "SrcBlendAlpha") as i32),
            DestBlendAlpha: D3D12_BLEND(u(t, "DestBlendAlpha") as i32),
            BlendOpAlpha: D3D12_BLEND_OP(u(t, "BlendOpAlpha") as i32),
            LogicOp: D3D12_LOGIC_OP(u(t, "LogicOp") as i32),
            RenderTargetWriteMask: u(t, "RenderTargetWriteMask") as u8,
        };
    }
    d
}

fn rasterizer(v: &Value) -> D3D12_RASTERIZER_DESC {
    D3D12_RASTERIZER_DESC {
        FillMode: D3D12_FILL_MODE(u(v, "FillMode") as i32),
        CullMode: D3D12_CULL_MODE(u(v, "CullMode") as i32),
        FrontCounterClockwise: b(v, "FrontCounterClockwise"),
        DepthBias: v["DepthBias"].as_i64().unwrap_or(0) as i32,
        DepthBiasClamp: v["DepthBiasClamp"].as_f64().unwrap_or(0.0) as f32,
        SlopeScaledDepthBias: v["SlopeScaledDepthBias"].as_f64().unwrap_or(0.0) as f32,
        DepthClipEnable: b(v, "DepthClipEnable"),
        MultisampleEnable: b(v, "MultisampleEnable"),
        AntialiasedLineEnable: b(v, "AntialiasedLineEnable"),
        ForcedSampleCount: u(v, "ForcedSampleCount"),
        ConservativeRaster: D3D12_CONSERVATIVE_RASTERIZATION_MODE(u(v, "ConservativeRaster") as i32),
    }
}

fn face(v: &Value) -> D3D12_DEPTH_STENCILOP_DESC {
    D3D12_DEPTH_STENCILOP_DESC {
        StencilFailOp: D3D12_STENCIL_OP(u(v, "StencilFailOp") as i32),
        StencilDepthFailOp: D3D12_STENCIL_OP(u(v, "StencilDepthFailOp") as i32),
        StencilPassOp: D3D12_STENCIL_OP(u(v, "StencilPassOp") as i32),
        StencilFunc: D3D12_COMPARISON_FUNC(u(v, "StencilFunc") as i32),
    }
}

fn depth_stencil1(v: &Value) -> D3D12_DEPTH_STENCIL_DESC1 {
    D3D12_DEPTH_STENCIL_DESC1 {
        DepthEnable: b(v, "DepthEnable"),
        DepthWriteMask: D3D12_DEPTH_WRITE_MASK(u(v, "DepthWriteMask") as i32),
        DepthFunc: D3D12_COMPARISON_FUNC(u(v, "DepthFunc") as i32),
        StencilEnable: b(v, "StencilEnable"),
        StencilReadMask: u(v, "StencilReadMask") as u8,
        StencilWriteMask: u(v, "StencilWriteMask") as u8,
        FrontFace: face(&v["FrontFace"]),
        BackFace: face(&v["BackFace"]),
        DepthBoundsTestEnable: b(v, "DepthBoundsTestEnable"),
    }
}

/// A pipeline state made from description `desc`, with `root` its root signature and `replaced` the shaders rules put
/// at stages (by subobject type).
pub fn make(
    c: &Capture,
    device: &ID3D12Device2,
    desc: &Value,
    root: &ID3D12RootSignature,
    replaced: &[(u32, Vec<u8>)],
) -> Result<ID3D12PipelineState, String> {
    let mut s = Stream::default();
    let mut objects = subobjects(desc);
    for (i, (t, _)) in replaced.iter().enumerate() {
        objects.retain(|(ty, _)| ty != t);
        objects.push((*t, serde_json::json!({ "replaced": i })));
    }
    for (ty, v) in &objects {
        match *ty {
            ROOT => s.push(*ty, root.as_raw()),
            VS | PS | DS | HS | GS | CS | AS | MS => {
                let code = if let Some(i) = v["replaced"].as_u64() {
                    replaced[i as usize].1.clone()
                } else if let Some(key) = v["blob"].as_str() {
                    c.blob(key)?
                } else {
                    // No shader at this stage.
                    continue;
                };
                s.shaders.push(code);
                let code = s.shaders.last().expect("just pushed");
                s.push(
                    *ty,
                    D3D12_SHADER_BYTECODE {
                        pShaderBytecode: code.as_ptr().cast(),
                        BytecodeLength: code.len(),
                    },
                );
            }
            STREAM_OUTPUT => return Err("stream output pipelines are not replayed".into()),
            BLEND => s.push(*ty, blend(v)),
            SAMPLE_MASK => s.push(*ty, v.as_u64().unwrap_or(u64::from(u32::MAX)) as u32),
            RASTERIZER => s.push(*ty, rasterizer(v)),
            DEPTH_STENCIL => {
                let d = depth_stencil1(v);
                s.push(
                    *ty,
                    D3D12_DEPTH_STENCIL_DESC {
                        DepthEnable: d.DepthEnable,
                        DepthWriteMask: d.DepthWriteMask,
                        DepthFunc: d.DepthFunc,
                        StencilEnable: d.StencilEnable,
                        StencilReadMask: d.StencilReadMask,
                        StencilWriteMask: d.StencilWriteMask,
                        FrontFace: d.FrontFace,
                        BackFace: d.BackFace,
                    },
                )
            }
            DEPTH_STENCIL1 => s.push(*ty, depth_stencil1(v)),
            INPUT_LAYOUT => {
                let elements = v.as_array().cloned().unwrap_or_default();
                let first = s.names.len();
                for e in &elements {
                    let name: Vec<u8> = e["semantic"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_u64)
                        .map(|b| b as u8)
                        .take_while(|&b| b != 0)
                        .collect();
                    s.names.push(CString::new(name).map_err(|e| e.to_string())?);
                }
                let descs: Vec<D3D12_INPUT_ELEMENT_DESC> = elements
                    .iter()
                    .zip(&s.names[first..])
                    .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
                        SemanticName: PCSTR(name.as_ptr().cast()),
                        SemanticIndex: u(e, "index"),
                        Format: DXGI_FORMAT(u(e, "format") as i32),
                        InputSlot: u(e, "slot"),
                        AlignedByteOffset: u(e, "offset"),
                        InputSlotClass: D3D12_INPUT_CLASSIFICATION(u(e, "class") as i32),
                        InstanceDataStepRate: u(e, "step"),
                    })
                    .collect();
                s.elements.push(descs);
                let descs = s.elements.last().expect("just pushed");
                s.push(
                    *ty,
                    D3D12_INPUT_LAYOUT_DESC {
                        pInputElementDescs: if descs.is_empty() {
                            core::ptr::null()
                        } else {
                            descs.as_ptr()
                        },
                        NumElements: descs.len() as u32,
                    },
                );
            }
            STRIP_CUT => s.push(
                *ty,
                D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(v.as_u64().unwrap_or(0) as i32),
            ),
            TOPOLOGY => s.push(*ty, D3D12_PRIMITIVE_TOPOLOGY_TYPE(v.as_u64().unwrap_or(0) as i32)),
            RT_FORMATS => {
                let mut f = D3D12_RT_FORMAT_ARRAY {
                    NumRenderTargets: u(v, "NumRenderTargets"),
                    ..Default::default()
                };
                for (i, x) in v["RTFormats"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(8)
                    .enumerate()
                {
                    f.RTFormats[i] = DXGI_FORMAT(x.as_u64().unwrap_or(0) as i32);
                }
                s.push(*ty, f);
            }
            DS_FORMAT => s.push(*ty, DXGI_FORMAT(v.as_u64().unwrap_or(0) as i32)),
            SAMPLE_DESC => s.push(
                *ty,
                DXGI_SAMPLE_DESC {
                    Count: u(v, "count").max(1),
                    Quality: u(v, "quality"),
                },
            ),
            // One device: its node, whatever the game's mask named.
            NODE_MASK => s.push(*ty, D3D12_NODE_MASK { NodeMask: 0 }),
            // A cached blob is the game's driver's; the state is made from its description.
            CACHED => {}
            FLAGS => s.push(*ty, D3D12_PIPELINE_STATE_FLAGS(v.as_u64().unwrap_or(0) as i32)),
            other => return Err(format!("pipeline subobject type {other} is not replayed")),
        }
    }
    let stream = D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: s.len,
        pPipelineStateSubobjectStream: s.words.as_mut_ptr().cast(),
    };
    unsafe { device.CreatePipelineState(&stream) }.map_err(|e| e.to_string())
}

/// Where a root signature puts register `b{slot}` (space 0) for `stage` (rules' numbering): a root constant buffer
/// view parameter, a descriptor `offset` into a table parameter, or a root constants parameter of `words` DWORDs.
pub enum CbAt {
    Root(u32),
    Table { parameter: u32, offset: u32 },
    Constants { parameter: u32, words: u32 },
}

pub fn cb_at(root: &Value, stage: usize, slot: u32) -> Option<CbAt> {
    // D3D12_SHADER_VISIBILITY: all, then vs hs ds gs ps; compute sees only `all`.
    let visible = |v: u64| v == 0 || (stage < 5 && v == stage as u64 + 1);
    for (i, p) in root["parameters"].as_array()?.iter().enumerate() {
        if !visible(p["visibility"].as_u64().unwrap_or(0)) {
            continue;
        }
        match p["type"].as_u64() {
            Some(2) if u(&p["value"], "ShaderRegister") == slot && u(&p["value"], "RegisterSpace") == 0 => {
                return Some(CbAt::Root(i as u32));
            }
            Some(1) if u(&p["value"], "ShaderRegister") == slot && u(&p["value"], "RegisterSpace") == 0 => {
                return Some(CbAt::Constants {
                    parameter: i as u32,
                    words: u(&p["value"], "Num32BitValues"),
                });
            }
            Some(0) => {
                let mut next = 0u32;
                for r in p["value"].as_array().into_iter().flatten() {
                    let count = u(r, "NumDescriptors");
                    let at = match u(r, "OffsetInDescriptorsFromTableStart") {
                        u32::MAX => next,
                        o => o,
                    };
                    next = at.saturating_add(count);
                    let base = u(r, "BaseShaderRegister");
                    if u(r, "RangeType") == 2
                        && u(r, "RegisterSpace") == 0
                        && slot >= base
                        && (count == u32::MAX || slot < base + count)
                    {
                        return Some(CbAt::Table {
                            parameter: i as u32,
                            offset: at + (slot - base),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    None
}
