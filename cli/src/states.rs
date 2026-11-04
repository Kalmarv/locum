//! Fixed-function state as text: the descriptions captures keep (numbers, as the API has them) with Direct3D's enums
//! named, in the viewer's words (viewer/src/util.tsx).

use locum_capture::format;
use serde_json::Value;

pub fn topology(t: i64) -> String {
    let name = match t {
        0 => "undefined",
        1 => "point list",
        2 => "line list",
        3 => "line strip",
        4 => "triangle list",
        5 => "triangle strip",
        10 => "line list adj",
        11 => "line strip adj",
        12 => "triangle list adj",
        13 => "triangle strip adj",
        33..=64 => return format!("{} control point patch list ({t})", t - 32),
        _ => return t.to_string(),
    };
    format!("{name} ({t})")
}

fn named(table: &[(u64, &str)], v: &Value) -> String {
    match v.as_u64() {
        Some(n) => table
            .iter()
            .find(|(k, _)| *k == n)
            .map_or_else(|| n.to_string(), |(_, s)| s.to_string()),
        None => v.to_string(),
    }
}

const CULL: &[(u64, &str)] = &[(1, "none"), (2, "front"), (3, "back")];
const FILL: &[(u64, &str)] = &[(2, "wireframe"), (3, "solid")];
const COMPARISON: &[(u64, &str)] = &[
    (1, "never"),
    (2, "less"),
    (3, "equal"),
    (4, "less_equal"),
    (5, "greater"),
    (6, "not_equal"),
    (7, "greater_equal"),
    (8, "always"),
];
const BLEND: &[(u64, &str)] = &[
    (1, "zero"),
    (2, "one"),
    (3, "src_color"),
    (4, "inv_src_color"),
    (5, "src_alpha"),
    (6, "inv_src_alpha"),
    (7, "dest_alpha"),
    (8, "inv_dest_alpha"),
    (9, "dest_color"),
    (10, "inv_dest_color"),
    (11, "src_alpha_sat"),
    (14, "blend_factor"),
    (15, "inv_blend_factor"),
    (16, "src1_color"),
    (17, "inv_src1_color"),
    (18, "src1_alpha"),
    (19, "inv_src1_alpha"),
];
const BLEND_OP: &[(u64, &str)] = &[
    (1, "add"),
    (2, "subtract"),
    (3, "rev_subtract"),
    (4, "min"),
    (5, "max"),
];
const LOGIC_OP: &[(u64, &str)] = &[
    (0, "clear"),
    (1, "set"),
    (2, "copy"),
    (3, "copy_inverted"),
    (4, "noop"),
    (5, "invert"),
    (6, "and"),
    (7, "nand"),
    (8, "or"),
    (9, "nor"),
    (10, "xor"),
    (11, "equiv"),
    (12, "and_reverse"),
    (13, "and_inverted"),
    (14, "or_reverse"),
    (15, "or_inverted"),
];
const STENCIL_OP: &[(u64, &str)] = &[
    (1, "keep"),
    (2, "zero"),
    (3, "replace"),
    (4, "incr_sat"),
    (5, "decr_sat"),
    (6, "invert"),
    (7, "incr"),
    (8, "decr"),
];
const ADDRESS: &[(u64, &str)] = &[
    (1, "wrap"),
    (2, "mirror"),
    (3, "clamp"),
    (4, "border"),
    (5, "mirror_once"),
];

fn filter(f: u64) -> String {
    let reduction = ["", "comparison ", "minimum ", "maximum "][((f >> 7) & 3) as usize];
    if f & 0x40 != 0 {
        return format!("{reduction}anisotropic");
    }
    let pt = |s: u32| if (f >> s) & 1 != 0 { "linear" } else { "point" };
    format!("{reduction}min {}, mag {}, mip {}", pt(4), pt(2), pt(0))
}

fn write_mask(m: u64) -> String {
    if m == 0 {
        return "none".into();
    }
    "rgba"
        .chars()
        .enumerate()
        .filter(|(i, _)| m & (1 << i) != 0)
        .map(|(_, c)| c)
        .collect()
}

fn on(v: &Value) -> &'static str {
    if v.as_bool().unwrap_or(false) { "on" } else { "off" }
}

/// A state of `kind` from its description, on one line (an input layout: one line per element, each starting with a
/// newline). Descriptions this does not know are shown as they are.
pub fn describe(kind: &str, json: Option<&str>) -> String {
    let Some(j) = json.and_then(|s| serde_json::from_str::<Value>(s).ok()) else {
        return json.unwrap_or("").to_string();
    };
    match kind {
        "rasterizer" => rasterizer(&j),
        "depth_stencil" => depth_stencil(&j),
        "blend" => blend(&j),
        "sampler" => sampler(&j),
        "input_layout" => input_layout(&j),
        _ => None,
    }
    .unwrap_or_else(|| j.to_string())
}

fn rasterizer(j: &Value) -> Option<String> {
    let mut s = format!(
        "fill {}, cull {}",
        named(FILL, &j["fill"]),
        named(CULL, &j["cull"])
    );
    if j["front_ccw"].as_bool()? {
        s += ", front ccw";
    }
    s += &format!(", depth bias {}", j["depth_bias"]);
    if !j["depth_bias_clamp"].is_null() {
        s += &format!(
            " (clamp {}, slope {})",
            j["depth_bias_clamp"], j["slope_scaled_depth_bias"]
        );
    }
    s += &format!(
        ", depth clip {}, scissor {}",
        on(&j["depth_clip"]),
        on(&j["scissor"])
    );
    if !j["multisample"].is_null() {
        s += &format!(
            ", msaa {}, aa lines {}",
            on(&j["multisample"]),
            on(&j["antialiased_line"])
        );
    }
    if j["forced_sample_count"].as_u64().is_some_and(|n| n != 0) {
        s += &format!(", forced sample count {}", j["forced_sample_count"]);
    }
    if j["conservative"].as_u64().is_some_and(|n| n != 0) {
        s += ", conservative";
    }
    Some(s)
}

fn depth_stencil(j: &Value) -> Option<String> {
    let mut s = if j["depth"].as_bool()? {
        format!(
            "depth test {}, write {}",
            named(COMPARISON, &j["func"]),
            on(&j["write"])
        )
    } else {
        "depth off".into()
    };
    if j["stencil"].as_bool()? {
        let face = |f: &Value| {
            format!(
                "{}: pass {}, fail {}, depth fail {}",
                named(COMPARISON, &f["func"]),
                named(STENCIL_OP, &f["pass"]),
                named(STENCIL_OP, &f["fail"]),
                named(STENCIL_OP, &f["depth_fail"])
            )
        };
        s += &format!(
            "; stencil read 0x{:x} write 0x{:x}, front {}, back {}",
            j["read_mask"].as_u64()?,
            j["write_mask"].as_u64()?,
            face(&j["front"]),
            face(&j["back"])
        );
    } else {
        s += "; stencil off";
    }
    Some(s)
}

fn blend_target(t: &Value) -> String {
    let mut s = if t["blend"].as_bool().unwrap_or(false) {
        format!(
            "{} {} {} / alpha {} {} {}",
            named(BLEND, &t["src"]),
            named(BLEND_OP, &t["op"]),
            named(BLEND, &t["dst"]),
            named(BLEND, &t["src_alpha"]),
            named(BLEND_OP, &t["op_alpha"]),
            named(BLEND, &t["dst_alpha"])
        )
    } else {
        "off".into()
    };
    if t["logic"].as_bool().unwrap_or(false) {
        s += &format!(", logic {}", named(LOGIC_OP, &t["logic_op"]));
    }
    s + &format!(", write {}", write_mask(t["write_mask"].as_u64().unwrap_or(0)))
}

fn blend(j: &Value) -> Option<String> {
    let targets = j["targets"].as_array()?;
    let mut s = format!("alpha to coverage {}", on(&j["alpha_to_coverage"]));
    if !j["independent"].as_bool()? {
        return Some(s + &format!("; all targets: {}", blend_target(targets.first()?)));
    }
    // Runs of targets with the same description, as one.
    let described: Vec<String> = targets.iter().map(blend_target).collect();
    let mut i = 0;
    while i < described.len() {
        let mut end = i;
        while end + 1 < described.len() && described[end + 1] == described[i] {
            end += 1;
        }
        let range = if end > i {
            format!("rt{i}-{end}")
        } else {
            format!("rt{i}")
        };
        s += &format!("; {range}: {}", described[i]);
        i = end + 1;
    }
    Some(s)
}

fn sampler(j: &Value) -> Option<String> {
    let f = j["filter"].as_u64()?;
    let address: Vec<String> = j["address"]
        .as_array()?
        .iter()
        .map(|a| named(ADDRESS, a))
        .collect();
    let mut s = format!("{}; {}", filter(f), address.join("/"));
    if f & 0x40 != 0 {
        s += &format!("; max anisotropy {}", j["max_anisotropy"]);
    }
    if f & 0x180 == 0x80 {
        s += &format!("; compare {}", named(COMPARISON, &j["compare"]));
    }
    if j["mip_lod_bias"].as_f64().is_some_and(|b| b != 0.0) {
        s += &format!("; lod bias {}", j["mip_lod_bias"]);
    }
    let (min, max) = (j["min_lod"].as_f64()?, j["max_lod"].as_f64()?);
    if min != 0.0 || max < f32::MAX as f64 {
        s += &format!(
            "; lod {}..{}",
            crate::describe::float(min as f32),
            crate::describe::float(max as f32)
        );
    }
    if address.iter().any(|a| a == "border") {
        s += &format!("; border {}", j["border"]);
    }
    Some(s)
}

fn input_layout(j: &Value) -> Option<String> {
    let mut s = String::new();
    for e in j.as_array()? {
        let offset = match e["offset"].as_u64()? {
            0xffff_ffff => "append".to_string(),
            o => o.to_string(),
        };
        let rate = if e["per_instance"].as_bool()? {
            format!("per instance, step {}", e["step"])
        } else {
            "per vertex".into()
        };
        s += &format!(
            "\n    {}{} {} slot {} offset {offset}, {rate}",
            e["semantic"].as_str()?,
            e["index"].as_u64()?,
            format::name(e["format"].as_u64()? as u32),
            e["slot"]
        );
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_what_the_capture_numbers() {
        assert_eq!(topology(4), "triangle list (4)");
        assert_eq!(topology(35), "3 control point patch list (35)");
        assert_eq!(
            describe(
                "rasterizer",
                Some(
                    r#"{"fill":3,"cull":3,"front_ccw":false,"depth_bias":0,"scissor":true,"depth_clip":true}"#
                )
            ),
            "fill solid, cull back, depth bias 0, depth clip on, scissor on"
        );
        assert_eq!(
            describe(
                "depth_stencil",
                Some(
                    r#"{"depth":true,"write":false,"func":7,"stencil":false,"read_mask":255,"write_mask":255,
                    "front":{},"back":{}}"#
                )
            ),
            "depth test greater_equal, write off; stencil off"
        );
        assert_eq!(
            describe(
                "sampler",
                Some(
                    r#"{"filter":85,"address":[1,1,3],"mip_lod_bias":0.0,"max_anisotropy":16,"compare":0,
                    "border":[0,0,0,0],"min_lod":0.0,"max_lod":3.4028234663852886e38}"#
                )
            ),
            "anisotropic; wrap/wrap/clamp; max anisotropy 16"
        );
        assert_eq!(
            describe(
                "sampler",
                Some(
                    r#"{"filter":149,"address":[3,3,3],"mip_lod_bias":0.0,"max_anisotropy":1,"compare":4,
                    "border":[0,0,0,0],"min_lod":0.0,"max_lod":0.0}"#
                )
            ),
            "comparison min linear, mag linear, mip linear; clamp/clamp/clamp; compare less_equal; lod 0..0"
        );
        let target = |blend: bool| {
            format!(
                r#"{{"blend":{blend},"logic":false,"src":5,"dst":6,"op":1,"src_alpha":2,"dst_alpha":1,"op_alpha":1,
                "logic_op":0,"write_mask":15}}"#
            )
        };
        let blend = format!(
            r#"{{"alpha_to_coverage":false,"independent":true,"targets":[{},{},{}]}}"#,
            target(true),
            target(false),
            target(false)
        );
        assert_eq!(
            describe("blend", Some(&blend)),
            "alpha to coverage off; rt0: src_alpha add inv_src_alpha / alpha one add zero, write rgba; \
             rt1-2: off, write rgba"
        );
        assert_eq!(
            describe(
                "input_layout",
                Some(
                    r#"[{"semantic":"POSITION","index":0,"format":6,"slot":0,"offset":0,"per_instance":false,"step":0},
                    {"semantic":"TEXCOORD","index":1,"format":30,"slot":1,"offset":4294967295,"per_instance":true,"step":1}]"#
                )
            ),
            "\n    POSITION0 R32G32B32_FLOAT slot 0 offset 0, per vertex\
             \n    TEXCOORD1 R8G8B8A8_UINT slot 1 offset append, per instance, step 1"
        );
        // Unknown descriptions are shown as they are.
        assert_eq!(describe("rasterizer", Some("{}")), "{}");
        assert_eq!(describe("blend", Some("not json")), "not json");
    }
}
