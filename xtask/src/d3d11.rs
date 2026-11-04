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
fn fields(e: &Entry) -> Vec<(String, String, String)> {
    e.params
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let a = format!("a{i}");
            let count = count(&e.name, i);
            match arg(t) {
                Arg::Scalar => (t.clone(), a.clone(), format!("*{a}")),
                Arg::Object => (
                    "Object".into(),
                    format!("unsafe {{ Object::copy({a}) }}"),
                    format!("{a}.raw()"),
                ),
                Arg::Objects => (
                    "Objects".into(),
                    format!("unsafe {{ Objects::copy({a}, {count}) }}"),
                    format!("{a}.raw()"),
                ),
                Arg::Array(t) => (
                    format!("Array<{t}>"),
                    format!("unsafe {{ Array::copy({a}, {count}) }}"),
                    format!("{a}.raw()"),
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
        "//! Generated owned context commands by `cargo xtask gen`; do not edit.\n#![allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc, dead_code)]\nuse crate::d3d11::recorder::{Object, Objects, Array};\nuse core::ffi::c_void;\n",
    );
    // Each payload is independently sized: a Draw does not pay the size of UpdateTileMappings.
    for e in &entries {
        writeln!(out, "#[derive(Clone)]\npub struct {} {{", e.name).unwrap();
        for (i, (ty, _, _)) in fields(e).iter().enumerate() {
            writeln!(out, "    a{i}: {ty},").unwrap();
        }
        out.push_str("}\n");
    }
    out.push_str("#[derive(Clone)]\npub enum Command {\n");
    for e in &entries {
        writeln!(out, "    {}(Box<{}>),", e.name, e.name).unwrap();
    }
    out.push_str("}\nimpl Command {\n");
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
            "pub unsafe fn capture_{}({params}) -> Self {{ Self::{}(Box::new({} {{",
            e.name, e.name, e.name
        )
        .unwrap();
        for (i, (_, take, _)) in fields(e).iter().enumerate() {
            if take == &format!("a{i}") {
                writeln!(out, "a{i},").unwrap();
            } else {
                writeln!(out, "a{i}: {take},").unwrap();
            }
        }
        out.push_str("})) }\n");
    }
    out.push_str(
        "pub unsafe fn replay(&self, context: *mut c_void) -> windows::core::Result<()> { match self {\n",
    );
    for e in &entries {
        let names = (0..e.params.len())
            .map(|i| format!("a{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let args = fields(e)
            .iter()
            .map(|(_, _, r)| r.clone())
            .collect::<Vec<_>>()
            .join(", ");
        let comma = if args.is_empty() { "" } else { ", " };
        writeln!(
            out,
            "Self::{}(p) => {{ let {} {{ {names} }} = &**p;",
            e.name, e.name
        )
        .unwrap();
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
    out.push_str("} }\n}\n");
    out
}
