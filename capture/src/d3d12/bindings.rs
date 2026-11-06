//! Browsing projection of authoritative root arguments and descriptor versions.
//! Reachability comes from the root signature and API state, never shader reflection.
use super::{Descriptor, Pipeline};
use crate::{db, hash};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The bindings draw `draw` makes. `from_records` are the root parameters an ExecuteIndirect's argument records set
/// (see [`super::indirect::root_parameters`]): the call does not use what the list set there, so they bind nothing
/// here, and what their records name is added to the event after the frame.
pub(super) fn project(
    c: &mut db::Capture,
    draw: u64,
    compute: bool,
    p: &Pipeline,
    objects: &BTreeMap<u64, Value>,
    descriptors: &BTreeMap<(u64, u32), Descriptor>,
    from_records: &[u32],
) {
    let (root, args) = if compute {
        (p.compute_root, &p.compute_args)
    } else {
        (p.graphics_root, &p.graphics_args)
    };
    let Some(root) = objects.get(&root) else {
        return;
    };
    // A root parameter visible to every stage binds only the stages the pipeline has a shader for.
    let present: Option<Vec<&str>> = objects
        .get(&p.pso)
        .and_then(|pso| pso["shaders"].as_array())
        .map(|shaders| shaders.iter().filter_map(|s| s["stage"].as_str()).collect());
    let visible = |visibility: &Value| -> Vec<&'static str> {
        stages(visibility.as_u64().unwrap_or(0), compute)
            .into_iter()
            .filter(|s| present.as_ref().is_none_or(|p| p.contains(s)))
            .collect()
    };
    for sampler in root["decoded"]["samplers"].as_array().into_iter().flatten() {
        let state = super::graphics::state(c, "sampler", sampler);
        for stage in visible(&sampler["ShaderVisibility"]) {
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
    for (&index, arg) in args.iter().filter(|(i, _)| !from_records.contains(i)) {
        let Some(parameter) = parameters.get(index as usize) else {
            continue;
        };
        let stages = visible(&parameter["visibility"]);
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
                let slot = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
                // A range reaches only descriptors of its own type: games point tables of different types at
                // overlapping heap slots (Elden Ring's CBV table b0-b3 starts one slot before its SRV table t0-t3),
                // and the slots holding the other type are not what the shader sees through this range.
                let kind = match range["RangeType"].as_u64() {
                    Some(0) => "srv",
                    Some(1) => "uav",
                    Some(2) => "cb",
                    Some(3) => "sampler",
                    _ => "",
                };
                for descriptor in descriptors
                    .range((heap, slot(start))..(heap, slot(end)))
                    .map(|(_, d)| d)
                    .filter(|d| d.kind == kind)
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
            let kind = match parameter["type"].as_u64() {
                Some(2) => "cb",
                Some(3) => "srv",
                Some(4) => "uav",
                _ => continue,
            };
            // A root CBV has no size of its own: at most the largest constant buffer view, within its buffer.
            let size = resource
                .and_then(|r| objects.get(&r))
                .and_then(|r| r["width"].as_u64())
                .and_then(|w| w.checked_sub(offset))
                .map(|rest| if kind == "cb" { rest.min(65536) } else { rest });
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
/// The shader-stage bindings this locum projects for every draw of the D3D12 capture at `conn`, from what the capture
/// holds authoritatively: each draw's pipeline and root arguments (`d12_draw_state`), the objects as described
/// (`d12_objects`: root signatures, pipelines, resources), and the descriptor definitions as of the draw's submission
/// (`d12_submission_descriptors`, applied in the order the frame submitted). A capture projected by an older locum
/// keeps its own rows; these say which of them the current projection makes. Constant-buffer contents are not
/// composed here (they come from the capture's upload and first-use copies), and sampler rows' `state` numbers the
/// returned capture's `states`.
pub fn reproject(conn: &rusqlite::Connection) -> rusqlite::Result<db::Capture> {
    let u = |v: i64| v as u64;
    let objects: BTreeMap<u64, Value> = conn
        .prepare("SELECT id, description FROM d12_objects")?
        .query_map([], |r| Ok((u(r.get(0)?), r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(id, d)| (id, serde_json::from_str(&d).unwrap_or(Value::Null)))
        .collect();
    let mut versions: BTreeMap<u64, Descriptor> = BTreeMap::new();
    for row in conn
        .prepare("SELECT id, heap, slot, kind, resource, counter_resource, description FROM d12_descriptor_versions")?
        .query_map([], |r| {
            Ok((
                u(r.get(0)?),
                u(r.get(1)?),
                r.get::<_, i64>(2)? as u32,
                r.get::<_, String>(3)?,
                u(r.get(4)?),
                u(r.get(5)?),
                r.get::<_, String>(6)?,
            ))
        })?
    {
        let (id, heap, slot, kind, resource, counter, description) = row?;
        let kind = match kind.as_str() {
            "cb" => "cb",
            "srv" => "srv",
            "uav" => "uav",
            "sampler" => "sampler",
            "rtv" => "rtv",
            "dsv" => "dsv",
            "empty" => "empty",
            _ => continue,
        };
        let description = serde_json::from_str(&description).unwrap_or(Value::Null);
        versions.insert(id, Descriptor { id, heap, slot, kind, resource, counter, description });
    }
    let mut changes: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for row in conn
        .prepare("SELECT submission, version FROM d12_submission_descriptors ORDER BY submission, version")?
        .query_map([], |r| Ok((u(r.get(0)?), u(r.get(1)?))))?
    {
        let (submission, version) = row?;
        changes.entry(submission).or_default().push(version);
    }
    // Each submission's event, in the frame's order: a draw belongs to the last submission before it.
    let submits: Vec<(i64, u64)> = conn
        .prepare("SELECT seq, args FROM events WHERE kind = 'submit' ORDER BY seq")?
        .query_map([], |r| Ok((r.get(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(seq, args)| {
            let args: Value = serde_json::from_str(&args).ok()?;
            Some((seq, args["submission"].as_u64()?))
        })
        .collect();
    type Row = (u64, i64, String, String, String, String);
    let draws: Vec<Row> = conn
        .prepare(
            "SELECT s.draw, d.event, e.kind, s.root_args_json, e.call, e.args FROM d12_draw_state s \
             JOIN draws d ON d.n = s.draw JOIN events e ON e.seq = d.event ORDER BY d.event",
        )?
        .query_map([], |r| {
            Ok((
                u(r.get(0)?),
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let args = |v: &Value| -> BTreeMap<u32, Value> {
        v.as_object()
            .into_iter()
            .flatten()
            .filter_map(|(k, v)| Some((k.parse().ok()?, v.clone())))
            .collect()
    };
    let mut capture = db::Capture::default();
    let mut descriptors: BTreeMap<(u64, u32), Descriptor> = BTreeMap::new();
    let mut next = submits.iter().peekable();
    for (draw, event, kind, state, call, call_args) in draws {
        while let Some((_, submission)) = next.next_if(|(seq, _)| *seq < event) {
            for d in changes
                .get(submission)
                .into_iter()
                .flatten()
                .filter_map(|v| versions.get(v))
            {
                if d.kind == "empty" {
                    descriptors.remove(&(d.heap, d.slot));
                } else {
                    descriptors.insert((d.heap, d.slot), d.clone());
                }
            }
        }
        let state: Value = serde_json::from_str(&state).unwrap_or(Value::Null);
        let pipeline = Pipeline {
            pso: state["pso"].as_u64().unwrap_or(0),
            graphics_root: state["graphics_root"].as_u64().unwrap_or(0),
            compute_root: state["compute_root"].as_u64().unwrap_or(0),
            graphics_args: args(&state["graphics_args"]),
            compute_args: args(&state["compute_args"]),
            ..Pipeline::default()
        };
        let from_records = if call == "ExecuteIndirect" {
            serde_json::from_str::<Value>(&call_args)
                .ok()
                .and_then(|a| objects.get(&a[0].as_u64()?).map(super::indirect::root_parameters))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        project(
            &mut capture,
            draw,
            kind == "dispatch",
            &pipeline,
            &objects,
            &descriptors,
            &from_records,
        );
    }
    Ok(capture)
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
        // A structured buffer view's element size.
        b.stride = d.description["payload"]["StructureByteStride"]
            .as_u64()
            .filter(|s| *s != 0)
            .map(|s| s as u32);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn descriptor(slot: u32, kind: &'static str, resource: u64) -> ((u64, u32), Descriptor) {
        let d = Descriptor {
            id: u64::from(slot) + 100,
            heap: 9,
            slot,
            kind,
            resource,
            counter: 0,
            description: json!({}),
        };
        ((9, slot), d)
    }

    #[test]
    fn a_range_binds_only_descriptors_of_its_type() {
        // Elden Ring's shape: a pixel CBV table b0-b1 at slot 10 and a pixel SRV table t0-t1 at slot 11, so slot
        // 11 lies under both ranges and holds an SRV.
        let range = |kind: u64, count: u64| {
            json!([{"RangeType":kind,"BaseShaderRegister":0,"NumDescriptors":count,"RegisterSpace":0,
                "OffsetInDescriptorsFromTableStart":0}])
        };
        let root = json!({"decoded":{"parameters":[
            {"type":0,"visibility":5,"value":range(2, 2)},
            {"type":0,"visibility":5,"value":range(0, 2)},
        ],"samplers":[]}});
        let objects = BTreeMap::from([(1, root)]);
        let descriptors = BTreeMap::from([
            descriptor(10, "cb", 50),
            descriptor(11, "srv", 51),
            descriptor(12, "srv", 52),
        ]);
        let pipeline = Pipeline {
            graphics_root: 1,
            graphics_args: BTreeMap::from([
                (0, json!({"value":{"heap":9,"slot":10}})),
                (1, json!({"value":{"heap":9,"slot":11}})),
            ]),
            ..Pipeline::default()
        };
        let mut capture = db::Capture::default();
        project(&mut capture, 1, false, &pipeline, &objects, &descriptors, &[]);
        let mut bound: Vec<_> = capture
            .bindings
            .iter()
            .map(|b| (b.stage, b.kind, b.slot, b.resource))
            .collect();
        bound.sort();
        assert_eq!(
            bound,
            [
                ("ps", "cb", 0, Some(50)),
                ("ps", "srv", 0, Some(51)),
                ("ps", "srv", 1, Some(52))
            ]
        );
    }
}
