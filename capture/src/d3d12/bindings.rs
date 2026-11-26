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
            // A handle in no live heap (or in several) reaches nothing the capture knows.
            let location = table["heap"].as_u64().zip(table["slot"].as_u64());
            let Some(ranges) = d.as_array() else {
                continue;
            };
            let mut append = 0u64;
            for range in ranges {
                let offset = range["OffsetInDescriptorsFromTableStart"].as_u64().unwrap_or(0);
                let offset = if offset == u32::MAX as u64 { append } else { offset };
                let count = range["NumDescriptors"].as_u64().unwrap_or(0);
                append = offset.saturating_add(count);
                let slot = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
                // A range reaches only descriptors of its own type: games point tables of different types at
                // overlapping heap slots (Elden Ring's CBV table b0-b3 starts one slot before its SRV table t0-t3),
                // and the slots holding the other type are not what the shader sees through this range.
                let kind = match range["RangeType"].as_u64() {
                    Some(0) => "srv",
                    Some(1) => "uav",
                    Some(2) => "cb",
                    Some(3) => "sampler",
                    _ => continue,
                };
                let first_register = range["BaseShaderRegister"].as_u64().unwrap_or(0);
                let space = range["RegisterSpace"].as_u64().unwrap_or(0) as u32;
                // Register `register`, through heap slot `at` (none where the handle is in no heap) holding `found`.
                let reach = |c: &mut db::Capture,
                             register: u64,
                             at: Option<(u64, u32)>,
                             found: Option<&Descriptor>| {
                    let Ok(register) = u32::try_from(register) else {
                        return;
                    };
                    for stage in &stages {
                        if let Some(d) = found.filter(|d| d.kind == kind) {
                            descriptor_binding(c, draw, stage, register, space, d);
                        }
                        let mut row = match found {
                            Some(d) if d.kind == kind => pointee(d, objects),
                            Some(_) => flagged("other-type"),
                            None if at.is_some() => flagged("empty"),
                            None => flagged("no-heap"),
                        };
                        row.draw = draw;
                        row.stage = stage;
                        row.kind = kind;
                        row.slot = register;
                        row.space = space;
                        row.parameter = index;
                        row.heap = at.map(|a| a.0);
                        row.heap_slot = at.map(|a| a.1);
                        row.version = found.map(|d| d.id);
                        c.draw_descriptors.push(row);
                    }
                };
                let Some((heap, base)) = location else {
                    if count <= EVERY_SLOT {
                        for n in 0..count {
                            reach(c, first_register + n, None, None);
                        }
                    }
                    continue;
                };
                let start = base.saturating_add(offset);
                let end = start.saturating_add(count);
                if count <= EVERY_SLOT {
                    // Every register, those whose slot holds nothing of the range's type too.
                    for n in start..end {
                        reach(
                            c,
                            first_register + (n - start),
                            Some((heap, slot(n))),
                            descriptors.get(&(heap, slot(n))),
                        );
                    }
                } else {
                    for descriptor in descriptors
                        .range((heap, slot(start))..(heap, slot(end)))
                        .map(|(_, d)| d)
                        .filter(|d| d.kind == kind)
                    {
                        let register = first_register + (u64::from(descriptor.slot) - start);
                        reach(c, register, Some((heap, descriptor.slot)), Some(descriptor));
                    }
                }
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
            let (_, address, flag) = located(value, objects);
            let (slot, space) = (
                d["ShaderRegister"].as_u64().unwrap_or(0) as u32,
                d["RegisterSpace"].as_u64().unwrap_or(0) as u32,
            );
            for stage in &stages {
                c.bindings.push(db::Binding {
                    draw,
                    stage,
                    kind,
                    slot,
                    space,
                    resource,
                    offset: Some(offset),
                    size,
                    ..db::Binding::default()
                });
                c.draw_descriptors.push(db::DrawDescriptor {
                    draw,
                    stage,
                    kind,
                    slot,
                    space,
                    parameter: index,
                    resource,
                    address,
                    size,
                    flag,
                    ..db::DrawDescriptor::default()
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
            state_object: state["state_object"].as_u64().unwrap_or(0),
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

/// A table range of at most this many descriptors has each of its registers in `d12_draw_descriptors`, those whose
/// slot holds nothing of its type too; a longer one (a bindless table) only the descriptors of its type it reaches.
const EVERY_SLOT: u64 = 64;

/// What a `d12_draw_descriptors` row's `flag` says.
pub const FLAGS: [(&str, &str); 7] = [
    (
        "null",
        "a null descriptor, or a root descriptor or constant buffer view at address 0",
    ),
    ("no-resource", "its address is in no live buffer"),
    (
        "zero-size",
        "a buffer view of no elements, or a constant buffer view of no bytes",
    ),
    ("past-end", "the bytes it covers run past its buffer's end"),
    ("empty", "the table's slot holds no descriptor"),
    (
        "other-type",
        "the table's slot holds a descriptor of another type than its range's",
    ),
    (
        "no-heap",
        "the table's handle is in no live descriptor heap (or in several)",
    ),
];

fn flagged(flag: &'static str) -> db::DrawDescriptor {
    db::DrawDescriptor {
        flag: Some(flag),
        ..db::DrawDescriptor::default()
    }
}

/// The buffer, GPU address and flag of an address as the capture records it: `{resource, offset}`, null for address
/// 0, or the candidates (and the address) of one no single live buffer holds. The flag is `null` or `no-resource`;
/// an address several buffers share (placed buffers) is not flagged.
fn located(v: &Value, objects: &BTreeMap<u64, Value>) -> (Option<u64>, Option<u64>, Option<&'static str>) {
    let gpu = |v: &Value| {
        let base = objects.get(&v["resource"].as_u64()?)?["gpu_address"].as_u64()?;
        base.checked_add(v["offset"].as_u64().unwrap_or(0))
    };
    if v.is_null() {
        return (None, None, Some("null"));
    }
    if let Some(resource) = v["resource"].as_u64() {
        return (Some(resource), gpu(v), None);
    }
    let candidates = v["candidates"].as_array();
    let address = v["address"]
        .as_u64()
        .or_else(|| candidates.and_then(|c| c.first()).and_then(gpu));
    let flag = candidates.is_none_or(|c| c.is_empty()).then_some("no-resource");
    (None, address, flag)
}

/// What descriptor `d`, of its range's type, points at: its resource, the GPU address, bytes and element size of a
/// buffer view or constant buffer view, its format and dimension, and the flag of a null or out-of-bounds one.
fn pointee(d: &Descriptor, objects: &BTreeMap<u64, Value>) -> db::DrawDescriptor {
    let desc = &d.description;
    let buffer = objects.get(&d.resource);
    let width = buffer.and_then(|b| b["width"].as_u64());
    let base = buffer.and_then(|b| b["gpu_address"].as_u64());
    let mut row = db::DrawDescriptor {
        resource: (d.resource != 0).then_some(d.resource),
        ..db::DrawDescriptor::default()
    };
    // Bytes `offset..offset + size` of the buffer.
    let covers = |row: &mut db::DrawDescriptor, offset: u64, size: u64| {
        row.address = base.and_then(|b| b.checked_add(offset));
        row.size = Some(size);
        row.flag = if size == 0 {
            Some("zero-size")
        } else if width.is_some_and(|w| offset.saturating_add(size) > w) {
            Some("past-end")
        } else {
            None
        };
    };
    match d.kind {
        "cb" => {
            let bytes = desc["bytes"].as_u64().unwrap_or(0);
            if desc.is_null() {
                row.flag = Some("null");
            } else if row.resource.is_some() {
                covers(&mut row, desc["buffer"]["offset"].as_u64().unwrap_or(0), bytes);
            } else {
                let (_, address, flag) = located(&desc["buffer"], objects);
                row.address = address.or(desc["address"].as_u64().filter(|a| *a != 0));
                row.size = Some(bytes);
                row.flag = flag.or((bytes == 0).then_some("zero-size"));
            }
        }
        "srv" | "uav" => {
            let dimension = desc["dimension"].as_u64().map(|v| v as u32);
            row.dimension = dimension;
            row.format = desc["format"].as_u64().map(|v| v as u32);
            if desc["default"] == true {
                // The resource's own format, and a buffer whole.
                row.format = buffer.and_then(|b| b["format"].as_u64()).map(|v| v as u32);
                if buffer.is_some_and(|b| b["dimension"] == 1) {
                    covers(&mut row, 0, width.unwrap_or(0));
                }
            } else if dimension == Some(11) && d.kind == "srv" {
                // An acceleration structure view names its buffer by address.
                let (resource, address, flag) = located(&desc["payload"]["address"], objects);
                row.resource = resource;
                row.address = address;
                row.flag = flag;
                return row;
            } else if dimension == Some(1) {
                let payload = &desc["payload"];
                let raw = payload["Flags"].as_u64().unwrap_or(0) & 1 != 0;
                let element = match payload["StructureByteStride"].as_u64().unwrap_or(0) {
                    0 if raw => Some(4),
                    0 => row
                        .format
                        .and_then(crate::format::layout)
                        .map(|l| u64::from(l.block_bytes)),
                    stride => Some(stride),
                };
                if let Some(element) = element {
                    row.stride = u32::try_from(element).ok();
                    let first = payload["FirstElement"].as_u64().unwrap_or(0);
                    let count = payload["NumElements"].as_u64().unwrap_or(0);
                    covers(
                        &mut row,
                        first.saturating_mul(element),
                        count.saturating_mul(element),
                    );
                }
            }
            // A null descriptor reads zero whatever its description says.
            if row.resource.is_none() && desc["sampler_feedback"].is_null() {
                (row.address, row.size, row.stride) = (None, None, None);
                row.flag = Some("null");
            }
        }
        _ => {}
    }
    row
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

    #[test]
    fn draw_descriptors_say_what_each_register_points_at() {
        // A compute root signature: a table of b0, t0-t3 and u0, a root CBV b1 and a root SRV t4.
        let range = |kind: u64, register: u64, count: u64, offset: u64| {
            json!({"RangeType":kind,"BaseShaderRegister":register,"NumDescriptors":count,"RegisterSpace":0,
                "OffsetInDescriptorsFromTableStart":offset})
        };
        let root = json!({"decoded":{"parameters":[
            {"type":0,"visibility":0,"value":[range(2, 0, 1, 0), range(0, 0, 4, 1), range(1, 0, 1, 5)]},
            {"type":2,"visibility":0,"value":{"ShaderRegister":1,"RegisterSpace":0}},
            {"type":3,"visibility":0,"value":{"ShaderRegister":4,"RegisterSpace":0}},
        ],"samplers":[]}});
        let buffer = |width: u64, gpu: u64| json!({"dimension":1,"width":width,"gpu_address":gpu,"format":0});
        let objects = BTreeMap::from([(1, root), (50, buffer(4096, 0x10000)), (51, buffer(256, 0x20000))]);
        let at = |slot: u32, kind: &'static str, resource: u64, description: Value| {
            let d = Descriptor {
                id: u64::from(slot) + 100,
                heap: 9,
                slot,
                kind,
                resource,
                counter: 0,
                description,
            };
            ((9, slot), d)
        };
        let view = |first: u64, count: u64, stride: u64| {
            json!({"format":0,"dimension":1,"payload":{"FirstElement":first,"NumElements":count,
                "StructureByteStride":stride,"Flags":0}})
        };
        let descriptors = BTreeMap::from([
            at(
                20,
                "cb",
                50,
                json!({"buffer":{"resource":50,"offset":256},"bytes":512}),
            ),
            at(21, "srv", 50, view(4, 16, 16)),
            at(22, "srv", 50, view(0, 0, 16)),
            at(23, "srv", 0, view(0, 8, 16)),
            // slot 24 holds nothing; the UAV's slot 25 an SRV
            at(25, "srv", 51, view(0, 1, 4)),
        ]);
        let pipeline = Pipeline {
            compute_root: 1,
            compute_args: BTreeMap::from([
                (0, json!({"kind":"DescriptorTable","value":{"heap":9,"slot":20}})),
                (1, json!({"kind":"ConstantBufferView","value":null})),
                (
                    2,
                    json!({"kind":"ShaderResourceView","value":{"address":0x90000,"candidates":[]}}),
                ),
            ]),
            ..Pipeline::default()
        };
        let mut capture = db::Capture::default();
        project(&mut capture, 7, true, &pipeline, &objects, &descriptors, &[]);
        let rows: Vec<_> = capture
            .draw_descriptors
            .iter()
            .map(|d| (d.kind, d.slot, d.heap_slot, d.resource, d.address, d.size, d.flag))
            .collect();
        assert_eq!(
            rows,
            [
                ("cb", 0, Some(20), Some(50), Some(0x10100), Some(512), None),
                ("srv", 0, Some(21), Some(50), Some(0x10040), Some(256), None),
                (
                    "srv",
                    1,
                    Some(22),
                    Some(50),
                    Some(0x10000),
                    Some(0),
                    Some("zero-size")
                ),
                ("srv", 2, Some(23), None, None, None, Some("null")),
                ("srv", 3, Some(24), None, None, None, Some("empty")),
                ("uav", 0, Some(25), None, None, None, Some("other-type")),
                ("cb", 1, None, None, None, None, Some("null")),
                ("srv", 4, None, None, Some(0x90000), None, Some("no-resource")),
            ]
        );
        assert!(
            capture
                .draw_descriptors
                .iter()
                .all(|d| d.draw == 7 && d.stage == "cs")
        );
        assert_eq!(capture.draw_descriptors[1].stride, Some(16));
        assert_eq!(
            capture.draw_descriptors[5].version,
            Some(125),
            "the other type's descriptor is named"
        );
        // The bindings stay what they were: the descriptors of each range's type, and the root descriptors.
        assert_eq!(capture.bindings.len(), 6);

        // A view running past its buffer's end, and a table whose handle is in no heap.
        let descriptors = BTreeMap::from([at(
            20,
            "cb",
            51,
            json!({"buffer":{"resource":51,"offset":128},"bytes":256}),
        )]);
        let mut capture = db::Capture::default();
        project(&mut capture, 8, true, &pipeline, &objects, &descriptors, &[]);
        assert_eq!(capture.draw_descriptors[0].flag, Some("past-end"));
        let pipeline = Pipeline {
            compute_root: 1,
            compute_args: BTreeMap::from([(0, json!({"kind":"DescriptorTable","value":{"candidates":[]}}))]),
            ..Pipeline::default()
        };
        let mut capture = db::Capture::default();
        project(&mut capture, 9, true, &pipeline, &objects, &descriptors, &[]);
        assert_eq!(capture.draw_descriptors.len(), 6);
        assert!(
            capture
                .draw_descriptors
                .iter()
                .all(|d| d.flag == Some("no-heap") && d.heap.is_none())
        );
    }
}
