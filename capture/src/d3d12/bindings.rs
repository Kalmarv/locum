//! Browsing projection of authoritative root arguments and descriptor versions.
//! Reachability comes from the root signature and API state, never shader reflection.
use super::{Descriptor, Pipeline, Submission};
use crate::{db, hash};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub(super) fn project(
    c: &mut db::Capture,
    draw: u64,
    compute: bool,
    p: &Pipeline,
    objects: &BTreeMap<u64, Value>,
    submission: &Submission,
) {
    let (root, args) = if compute {
        (p.compute_root, &p.compute_args)
    } else {
        (p.graphics_root, &p.graphics_args)
    };
    let Some(root) = objects.get(&root) else {
        return;
    };
    for sampler in root["decoded"]["samplers"].as_array().into_iter().flatten() {
        let state = super::graphics::state(c, "sampler", sampler);
        for stage in stages(sampler["ShaderVisibility"].as_u64().unwrap_or(0), compute) {
            c.bindings.push(db::Binding {
                draw,
                stage,
                kind: "sampler",
                slot: sampler["ShaderRegister"].as_u64().unwrap_or(0) as u32,
                space: sampler["RegisterSpace"].as_u64().unwrap_or(0) as u32,
                state,
                ..Default::default()
            });
        }
    }
    let Some(parameters) = root["decoded"]["parameters"].as_array() else {
        return;
    };
    for (&index, arg) in args {
        let Some(parameter) = parameters.get(index as usize) else {
            continue;
        };
        let stages = stages(parameter["visibility"].as_u64().unwrap_or(0), compute);
        let d = &parameter["value"];
        if parameter["type"] == 0 {
            let table = &arg["value"];
            let (Some(heap), Some(base)) = (table["heap"].as_u64(), table["slot"].as_u64()) else {
                continue;
            };
            let Some(ranges) = d.as_array() else {
                continue;
            };
            let mut append = 0u64;
            for range in ranges {
                let offset = range["OffsetInDescriptorsFromTableStart"].as_u64().unwrap_or(0);
                let offset = if offset == u32::MAX as u64 { append } else { offset };
                let count = range["NumDescriptors"].as_u64().unwrap_or(0);
                let start = base.saturating_add(offset);
                let end = start.saturating_add(count);
                for descriptor in submission
                    .descriptors
                    .iter()
                    .filter(|v| v.heap == heap && u64::from(v.slot) >= start && u64::from(v.slot) < end)
                {
                    let register = range["BaseShaderRegister"].as_u64().unwrap_or(0)
                        + (u64::from(descriptor.slot) - start);
                    let Ok(register) = u32::try_from(register) else {
                        continue;
                    };
                    for stage in &stages {
                        descriptor_binding(
                            c,
                            draw,
                            stage,
                            register,
                            range["RegisterSpace"].as_u64().unwrap_or(0) as u32,
                            descriptor,
                        );
                    }
                }
                append = offset.saturating_add(count);
            }
        } else if parameter["type"] == 1 {
            let Some(words) = arg["words"].as_array() else {
                continue;
            };
            let bytes: Option<Vec<u8>> = words
                .iter()
                .map(|v| v.as_u64().map(|n| (n as u32).to_le_bytes()))
                .collect::<Option<Vec<_>>>()
                .map(|v| v.into_iter().flatten().collect());
            let Some(bytes) = bytes else {
                continue;
            };
            let key = hash::key(&bytes);
            let size = bytes.len() as u64;
            c.cb_data.push((key.clone(), bytes));
            for stage in &stages {
                c.bindings.push(db::Binding {
                    draw,
                    stage,
                    kind: "cb",
                    slot: d["ShaderRegister"].as_u64().unwrap_or(0) as u32,
                    space: d["RegisterSpace"].as_u64().unwrap_or(0) as u32,
                    content: Some(key.clone()),
                    size: Some(size),
                    ..db::Binding::default()
                });
            }
        } else {
            let value = &arg["value"];
            let resource = value["resource"].as_u64();
            let offset = value["offset"].as_u64().unwrap_or(0);
            let size = resource
                .and_then(|r| objects.get(&r))
                .and_then(|r| r["width"].as_u64())
                .and_then(|w| w.checked_sub(offset));
            let kind = match parameter["type"].as_u64() {
                Some(2) => "cb",
                Some(3) => "srv",
                Some(4) => "uav",
                _ => continue,
            };
            for stage in &stages {
                c.bindings.push(db::Binding {
                    draw,
                    stage,
                    kind,
                    slot: d["ShaderRegister"].as_u64().unwrap_or(0) as u32,
                    space: d["RegisterSpace"].as_u64().unwrap_or(0) as u32,
                    resource,
                    offset: Some(offset),
                    size,
                    ..db::Binding::default()
                });
            }
        }
    }
}
fn stages(visibility: u64, compute: bool) -> Vec<&'static str> {
    if compute {
        return if visibility == 0 { vec!["cs"] } else { Vec::new() };
    }
    match visibility {
        0 => vec!["vs", "hs", "ds", "gs", "ps"],
        1 => vec!["vs"],
        2 => vec!["hs"],
        3 => vec!["ds"],
        4 => vec!["gs"],
        5 => vec!["ps"],
        6 => vec!["as"],
        7 => vec!["ms"],
        _ => Vec::new(),
    }
}
pub(super) fn descriptor_binding(
    c: &mut db::Capture,
    draw: u64,
    stage: &'static str,
    slot: u32,
    space: u32,
    d: &Descriptor,
) {
    let mut b = db::Binding {
        draw,
        stage,
        kind: d.kind,
        slot,
        space,
        resource: (d.resource != 0).then_some(d.resource),
        ..db::Binding::default()
    };
    if d.kind == "cb" {
        b.offset = d.description["buffer"]["offset"].as_u64();
        b.size = d.description["bytes"].as_u64();
    } else if d.kind == "sampler" {
        b.state = super::graphics::state(c, "sampler", &d.description);
    } else {
        b.view = Some(d.id);
        if !c.views.iter().any(|v| v.id == d.id) {
            let resource = c.resources.iter().find(|r| r.id == d.resource);
            let format = d.description["format"]
                .as_u64()
                .map(|v| v as u32)
                .or_else(|| (d.description["default"] == true).then(|| resource.map_or(0, |r| r.format)))
                .unwrap_or(0);
            let dimension = d.description["dimension"].as_u64().unwrap_or(0) as u32;
            c.views.push(db::View {
                id: d.id,
                kind: d.kind,
                resource: d.resource,
                format,
                dimension,
                desc: d.description.to_string().into_bytes(),
                json: display_view(d, format, dimension, resource.map(|r| r.kind)).to_string(),
            });
        }
    }
    c.bindings.push(b);
    if d.counter != 0 {
        c.bindings.push(db::Binding {
            draw,
            stage,
            kind: "uav_counter",
            slot,
            space,
            resource: Some(d.counter),
            offset: d.description["payload"]["CounterOffsetInBytes"].as_u64(),
            size: Some(4),
            ..Default::default()
        });
    }
}

fn display_view(d: &Descriptor, format: u32, dimension: u32, resource_kind: Option<&str>) -> Value {
    let default = d.description["default"] == true;
    let name = if default {
        format!("default {}", resource_kind.unwrap_or("resource"))
    } else {
        // DSV's enumeration omits BUFFER, shifting the six texture dimensions by one.
        let dimension = if d.kind == "dsv" && dimension != 0 {
            dimension + 1
        } else {
            dimension
        };
        match dimension {
            0 => "unknown", 1 => "buffer", 2 => "texture1d", 3 => "texture1darray",
            4 => "texture2d", 5 => "texture2darray", 6 => "texture2dms",
            7 => "texture2dmsarray", 8 => "texture3d", 9 => "texturecube",
            10 => "texturecubearray", 11 => "raytracing_acceleration_structure",
            _ => return json!({"format":format,"dimension":format!("dimension {dimension}"),"native":d.description}),
        }.to_string()
    };
    let mut range = serde_json::Map::new();
    for (native, common) in [
        ("FirstElement", "first_element"),
        ("NumElements", "elements"),
        ("StructureByteStride", "stride"),
        ("MostDetailedMip", "first_mip"),
        ("MipLevels", "mips"),
        ("MipSlice", "mip"),
        ("FirstArraySlice", "first_slice"),
        ("ArraySize", "slices"),
        ("PlaneSlice", "plane"),
        ("FirstWSlice", "first_w"),
        ("WSize", "w"),
        ("First2DArrayFace", "first_face"),
        ("NumCubes", "cubes"),
        ("ResourceMinLODClamp", "min_lod"),
        ("Flags", "flags"),
        ("CounterOffsetInBytes", "counter_offset"),
        ("address", "address"),
    ] {
        if let Some(value) = d.description["payload"].get(native) {
            range.insert(common.into(), value.clone());
        }
    }
    json!({"format":format,"dimension":name,"range":range,"default":default,"native":d.description})
}
