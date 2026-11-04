//! Owning deferred-command arguments, generated from the same ABI as the forwarding context.
use super::{Entry, Vtbl, chain};
use std::{collections::HashMap, fmt::Write};

pub fn recorded(name: &str) -> bool {
    !matches!(name, "UpdateSubresource" | "UpdateSubresource1" | "UpdateTiles")
        && ([
            "Draw",
            "Dispatch",
            "Copy",
            "Clear",
            "Discard",
            "Resolve",
            "GenerateMips",
            "UpdateTileMappings",
            "ResizeTilePool",
            "TiledResourceBarrier",
            "IASet",
            "OMSet",
            "RSSet",
            "SOSet",
        ]
        .iter()
        .any(|p| name.starts_with(p))
            || ["VS", "HS", "DS", "GS", "PS", "CS"]
                .iter()
                .any(|s| name.starts_with(&format!("{s}Set")))
            || matches!(
                name,
                "Begin" | "End" | "SetPredication" | "SetResourceMinLOD" | "ExecuteCommandList"
            ))
}
fn count(name: &str, i: usize) -> String {
    if name.ends_with("SetShader") {
        return "a2".into();
    }
    if name.ends_with("SetConstantBuffers")
        || name.ends_with("SetConstantBuffers1")
        || name.ends_with("SetShaderResources")
        || name.ends_with("SetSamplers")
        || name == "IASetVertexBuffers"
        || name == "CSSetUnorderedAccessViews"
    {
        return "a1".into();
    }
    match (name, i) {
        ("OMSetRenderTargets", 1) | ("OMSetRenderTargetsAndUnorderedAccessViews", 1) => {
            "if a0 == u32::MAX {0} else {a0}"
        }
        ("OMSetRenderTargetsAndUnorderedAccessViews", 5 | 6) => "if a4 == u32::MAX {0} else {a4}",
        ("SOSetTargets", _) | ("RSSetViewports" | "RSSetScissorRects", 1) => "a0",
        (
            "OMSetBlendState"
            | "ClearRenderTargetView"
            | "ClearUnorderedAccessViewUint"
            | "ClearUnorderedAccessViewFloat",
            1,
        ) => "4",
        ("ClearView", 1) => "4",
        ("ClearView", 2) => "a3",
        ("DiscardView1", 1) => "a2",
        ("UpdateTileMappings", 2 | 3) => "a1",
        ("UpdateTileMappings", 6..=8) => "a5",
        _ => "1",
    }
    .into()
}
enum Arg {
    Scalar,
    Object,
    Objects,
    Array(String),
}
fn arg(ty: &str) -> Arg {
    let compact = ty.replace(' ', "");
    if compact == "*mutcore::ffi::c_void" {
        Arg::Object
    } else if compact == "*const*mutcore::ffi::c_void" {
        Arg::Objects
    } else if let Some(t) = compact.strip_prefix("*const") {
        Arg::Array(t.into())
    } else {
        assert!(!ty.contains('*'), "unhandled owning argument {ty}");
        Arg::Scalar
    }
}
/// How each argument of a call is written to a tape and read back for its replay.
fn fields(e: &Entry) -> Vec<(String, String)> {
    e.params
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let a = format!("a{i}");
            let count = count(&e.name, i);
            match arg(t) {
                Arg::Scalar => (format!("self.put({a});"), format!("let {a}: {t} = r.get();")),
                Arg::Object => (
                    format!("unsafe {{ self.object({a}) }};"),
                    format!("let {a} = r.object();"),
                ),
                Arg::Objects => (
                    format!("unsafe {{ self.objects({a}, {count}) }};"),
                    format!("let {a} = r.objects();"),
                ),
                Arg::Array(t) => (
                    format!("unsafe {{ self.array({a}, {count}) }};"),
                    format!("let {a}: *const {t} = r.array();"),
                ),
            }
        })
        .collect()
}
pub fn commands(vtbls: &HashMap<String, Vtbl>) -> String {
    let entries: Vec<_> = chain("ID3D11DeviceContext4", vtbls)
        .iter()
        .flat_map(|(_, v)| &v.entries)
        .filter(|e| recorded(&e.name))
        .collect();
    let mut out = String::from(
        "//! Generated deferred-context calls by `cargo xtask gen`; do not edit: each written to a tape and replayed from it.\n#![allow(non_snake_case, non_upper_case_globals, clippy::too_many_arguments, clippy::missing_safety_doc, dead_code)]\nuse crate::d3d11::recorder::{Reader, Tape};\nuse core::ffi::c_void;\n",
    );
    out.push_str("/// Each recorded call's tag on a tape.\npub mod tag {\n");
    for (n, e) in entries.iter().enumerate() {
        writeln!(out, "    pub const {}: u32 = {n};", e.name).unwrap();
    }
    out.push_str("}\nimpl Tape {\n");
    for e in &entries {
        let params = e
            .params
            .iter()
            .enumerate()
            .map(|(i, t)| format!("a{i}: {t}"))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            out,
            "pub unsafe fn {}(&mut self, {params}) {{ self.tag(tag::{});",
            e.name, e.name
        )
        .unwrap();
        for (write, _) in fields(e) {
            writeln!(out, "{write}").unwrap();
        }
        out.push_str("}\n");
    }
    out.push_str(
        "}\n/// Replays the call tagged `tag`, its arguments read from `r`.\npub unsafe fn call(tag: u32, r: &mut Reader, context: *mut c_void) -> windows::core::Result<()> { match tag {\n",
    );
    for e in &entries {
        writeln!(out, "tag::{} => {{", e.name).unwrap();
        for (_, read) in fields(e) {
            writeln!(out, "{read}").unwrap();
        }
        let args = (0..e.params.len())
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let comma = if args.is_empty() { "" } else { ", " };
        let call = format!(
            "unsafe {{ crate::vtables::context::real::{}(context{comma}{args}) }}",
            e.name
        );
        if e.ret.is_some() {
            writeln!(out, "{call}.ok() }}").unwrap();
        } else {
            writeln!(out, "{call}; Ok(()) }}").unwrap();
        }
    }
    out.push_str("_ => unreachable!(\"tag {tag} is no recorded call\"),\n} }\n");
    out
}
