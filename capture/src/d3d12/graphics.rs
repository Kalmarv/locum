//! The common inspector tables project D3D12 graphics state; root and PSO records remain authoritative.
use super::{Command, Descriptor, bindings};
use crate::db;
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Serialize)]
pub(super) struct State {
    topology: i32,
    vertex: BTreeMap<u32, Value>,
    index: Value,
    stream_output: BTreeMap<u32, Value>,
    rtvs: Value,
    dsv: Value,
    viewports: Value,
    scissors: Value,
    blend_factor: Value,
    stencil_ref: Option<u32>,
    predicate: Value,
    #[serde(skip)]
    pass: Option<u64>,
}
impl Default for State {
    fn default() -> Self {
        // ClearState defines the non-PSO initial state, including zero blend factors.
        // https://learn.microsoft.com/windows/win32/api/d3d12/nf-d3d12-id3d12graphicscommandlist-clearstate
        Self {
            topology: 0,
            vertex: BTreeMap::new(),
            index: Value::Null,
            stream_output: BTreeMap::new(),
            rtvs: Value::Null,
            dsv: Value::Null,
            viewports: Value::Null,
            scissors: Value::Null,
            blend_factor: json!([0., 0., 0., 0.]),
            stencil_ref: Some(0),
            predicate: Value::Null,
            pass: None,
        }
    }
}
impl State {
    pub(super) fn begin_bundle(&mut self) {
        // https://learn.microsoft.com/windows/win32/direct3d12/managing-graphics-pipeline-state-in-direct3d-12
        self.topology = 0;
    }

    pub(super) fn apply(&mut self, c: &Command) {
        let a = &c.args;
        match c.call {
            "IASetPrimitiveTopology" => self.topology = a[0].as_i64().unwrap_or(0) as i32,
            "IASetVertexBuffers" | "SOSetTargets" => {
                let values = if c.call == "IASetVertexBuffers" {
                    &mut self.vertex
                } else {
                    &mut self.stream_output
                };
                if let Some(entries) = a[1].as_array() {
                    let start = a[0].as_u64().unwrap_or(0) as u32;
                    for (i, v) in entries.iter().enumerate() {
                        values.insert(start + i as u32, v.clone());
                    }
                }
            }
            "IASetIndexBuffer" => self.index = a[0].clone(),
            "OMSetRenderTargets" => {
                if self.rtvs != a[0] || self.dsv != a[1] {
                    self.pass = None;
                }
                self.rtvs = a[0].clone();
                self.dsv = a[1].clone();
            }
            // A native render pass binds its attachments' views, as OMSetRenderTargets does, and starts a pass.
            "BeginRenderPass" => {
                self.rtvs = Value::Array(
                    a[0].as_array()
                        .into_iter()
                        .flatten()
                        .map(|t| t["view"].clone())
                        .collect(),
                );
                self.dsv = a[1]["view"].clone();
                self.pass = None;
            }
            "RSSetViewports" => self.viewports = a[0].clone(),
            "RSSetScissorRects" => self.scissors = a[0].clone(),
            "OMSetBlendFactor" => self.blend_factor = a[0].clone(),
            "OMSetStencilRef" => self.stencil_ref = a[0].as_u64().map(|v| v as u32),
            "SetPredication" => self.predicate = a.clone(),
            "Dispatch" | "ExecuteIndirect" | "EndRenderPass" => self.pass = None,
            _ => {}
        }
    }
}

pub(super) fn state(c: &mut db::Capture, kind: &'static str, value: &Value) -> Option<u64> {
    if value.is_null() {
        return None;
    }
    let desc = value.to_string().into_bytes();
    if let Some(s) = c.states.iter().find(|s| s.kind == kind && s.desc == desc) {
        return Some(s.id);
    }
    let id = c.states.len() as u64 + 1;
    c.states.push(db::State {
        id,
        kind,
        desc,
        json: display_state(kind, value).to_string(),
        ..Default::default()
    });
    Some(id)
}

// The common inspector consumes this schema for both APIs. The native description
// stays in State::desc and in the authoritative PSO/root-signature records.
fn display_state(kind: &str, d: &Value) -> Value {
    match kind {
        "rasterizer" => json!({
            "fill":d["FillMode"],"cull":d["CullMode"],"front_ccw":d["FrontCounterClockwise"],
            "depth_bias":d["DepthBias"],"depth_bias_clamp":d["DepthBiasClamp"],
            "slope_scaled_depth_bias":d["SlopeScaledDepthBias"],"depth_clip":d["DepthClipEnable"],
            "scissor":true,"multisample":d["MultisampleEnable"],
            "antialiased_line":d["AntialiasedLineEnable"],"line_mode":d["LineRasterizationMode"],
            "forced_sample_count":d["ForcedSampleCount"],"conservative":d["ConservativeRaster"]
        }),
        "blend" => json!({
            "alpha_to_coverage":d["AlphaToCoverageEnable"],"independent":d["IndependentBlendEnable"],
            "targets":d["RenderTarget"].as_array().into_iter().flatten().map(|t| json!({
                "blend":t["BlendEnable"],"logic":t["LogicOpEnable"],"src":t["SrcBlend"],
                "dst":t["DestBlend"],"op":t["BlendOp"],"src_alpha":t["SrcBlendAlpha"],
                "dst_alpha":t["DestBlendAlpha"],"op_alpha":t["BlendOpAlpha"],
                "logic_op":t["LogicOp"],"write_mask":t["RenderTargetWriteMask"]
            })).collect::<Vec<_>>()
        }),
        "depth_stencil" => {
            let face = |f: &Value| {
                json!({"fail":f["StencilFailOp"],"depth_fail":f["StencilDepthFailOp"],
                "pass":f["StencilPassOp"],"func":f["StencilFunc"],
                "read_mask":f["StencilReadMask"],"write_mask":f["StencilWriteMask"]})
            };
            json!({"depth":d["DepthEnable"],"write":d["DepthWriteMask"] == 1,
                "func":d["DepthFunc"],"stencil":d["StencilEnable"],
                "read_mask":d["StencilReadMask"],"write_mask":d["StencilWriteMask"],
                "front":face(&d["FrontFace"]),"back":face(&d["BackFace"]),
                "depth_bounds":d["DepthBoundsTestEnable"]})
        }
        "sampler" => {
            // Static samplers use D3D12_STATIC_BORDER_COLOR; heap samplers store RGBA.
            let border = match d["BorderColor"].as_u64() {
                Some(0) => json!([0, 0, 0, 0]),
                Some(1 | 3) => json!([0, 0, 0, 1]),
                Some(2 | 4) => json!([1, 1, 1, 1]),
                _ => d["BorderColor"].clone(),
            };
            json!({"filter":d["Filter"],"address":[d["AddressU"],d["AddressV"],d["AddressW"]],
                "mip_lod_bias":d["MipLODBias"],"max_anisotropy":d["MaxAnisotropy"],
                "compare":d["ComparisonFunc"],"border":border,
                "min_lod":d["MinLOD"],"max_lod":d["MaxLOD"],"flags":d["Flags"]})
        }
        "input_layout" => json!(
            d.as_array()
                .into_iter()
                .flatten()
                .map(|e| {
                    let mut e = e.clone();
                    if let Some(bytes) = e["semantic"].as_array() {
                        let bytes: Vec<u8> = bytes
                            .iter()
                            .filter_map(|v| v.as_u64().and_then(|v| u8::try_from(v).ok()))
                            .collect();
                        e["semantic"] = json!(String::from_utf8_lossy(&bytes));
                    }
                    e["per_instance"] = json!(e["class"] == 1);
                    e
                })
                .collect::<Vec<_>>()
        ),
        _ => d.clone(),
    }
}

fn buffer(c: &mut db::Capture, draw: u64, stage: &'static str, kind: &'static str, slot: u32, v: &Value) {
    let Some(resource) = v["buffer"]["resource"].as_u64() else {
        return;
    };
    c.bindings.push(db::Binding {
        draw,
        stage,
        kind,
        slot,
        resource: Some(resource),
        offset: v["buffer"]["offset"].as_u64(),
        size: v["bytes"].as_u64(),
        stride: v["stride"].as_u64().map(|v| v as u32),
        format: v["format"].as_u64().map(|v| v as u32),
        ..Default::default()
    });
    if let Some(resource) = v["filled_size"]["resource"].as_u64() {
        c.bindings.push(db::Binding {
            draw,
            stage,
            kind: "so_counter",
            slot,
            resource: Some(resource),
            offset: v["filled_size"]["offset"].as_u64(),
            size: Some(8),
            ..Default::default()
        });
    }
}

fn attachment(c: &mut db::Capture, draw: u64, slot: u32, kind: &'static str, v: &Value) {
    let d = &v["definition"];
    let Some(id) = d["id"].as_u64() else {
        return;
    };
    bindings::descriptor_binding(
        c,
        draw,
        "om",
        slot,
        0,
        &Descriptor {
            id,
            heap: d["heap"].as_u64().unwrap_or(0),
            slot: d["slot"].as_u64().unwrap_or(0) as u32,
            kind,
            resource: d["resource"].as_u64().unwrap_or(0),
            counter: 0,
            description: d["description"].clone(),
        },
    );
}

pub(super) fn project(c: &mut db::Capture, d: &mut db::Draw, s: &mut State, pso: Option<&Value>) {
    d.topology = s.topology;
    d.stencil_ref = s.stencil_ref;
    d.blend_factor = s
        .blend_factor
        .as_array()
        .filter(|v| v.len() == 4)
        .map(|v| std::array::from_fn(|i| v[i].as_f64().unwrap_or(0.) as f32));
    if let Some(p) = pso {
        d.layout = state(c, "input_layout", &p["input_layout"]);
        d.rasterizer = state(c, "rasterizer", &p["rasterizer"]);
        d.blend = state(c, "blend", &p["blend"]);
        d.depth_stencil = state(c, "depth_stencil", &p["depth_stencil"]);
        d.sample_mask = p["sample_mask"].as_u64().map(|v| v as u32);
    }
    for (&slot, v) in &s.vertex {
        buffer(c, d.n, "ia", "vb", slot, v);
    }
    buffer(c, d.n, "ia", "ib", 0, &s.index);
    for (&slot, v) in &s.stream_output {
        buffer(c, d.n, "so", "so", slot, v);
    }
    if let Some(rtvs) = s.rtvs.as_array() {
        for (slot, v) in rtvs.iter().enumerate() {
            attachment(c, d.n, slot as u32, "rtv", v);
        }
    }
    attachment(c, d.n, 0, "dsv", &s.dsv);
    if let Some(v) = s.viewports.as_array() {
        for (i, v) in v.iter().enumerate() {
            let n = |i| v[i].as_f64().unwrap_or(0.) as f32;
            c.viewports.push(db::Viewport {
                draw: d.n,
                i: i as u32,
                x: n(0),
                y: n(1),
                width: n(2),
                height: n(3),
                min_depth: n(4),
                max_depth: n(5),
            });
        }
    }
    if let Some(v) = s.scissors.as_array() {
        for (i, v) in v.iter().enumerate() {
            let n = |i| v[i].as_i64().unwrap_or(0) as i32;
            c.scissors.push(db::Scissor {
                draw: d.n,
                i: i as u32,
                left: n(0),
                top: n(1),
                right: n(2),
                bottom: n(3),
            });
        }
    }
    if s.pass.is_none() {
        let n = c.passes.len() as u64 + 1;
        c.passes.push(db::Pass {
            n,
            first_draw: d.n,
            last_draw: d.n,
        });
        s.pass = Some(n);
    }
    d.pass = s.pass;
    if let Some(pass) = s.pass.and_then(|p| c.passes.iter_mut().find(|v| v.n == p)) {
        pass.last_draw = d.n;
    }
    if let Some(resource) = s.predicate[0].as_u64().filter(|v| *v != 0) {
        c.bindings.push(db::Binding {
            draw: d.n,
            stage: "ia",
            kind: "predicate",
            resource: Some(resource),
            offset: s.predicate[1].as_u64(),
            size: Some(8),
            ..Default::default()
        });
    }
}
