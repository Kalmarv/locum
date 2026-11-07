//! What DispatchRays' shader tables name. Each record of a table is a shader identifier and the local root arguments
//! of the export it identifies (see [`super::programs`]): root CBVs, SRVs and UAVs as GPU virtual addresses, and
//! descriptor tables as GPU descriptor handles. A buffer or descriptor the frame uses only through them is named by no
//! command it records, so capture reads the tables after the frame (copied right before each direct DispatchRays, or
//! where an ExecuteIndirect's records name them) to learn what each dispatch reads and may write.
//!
//! A table may hold records no ray reaches, whose bytes are anything (D3D12 reads only the records a dispatch's
//! rays index): what cannot be placed is counted, by reason, and named nothing.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use super::programs::{self, IDENTIFIER, Kind, Local, Source};

/// A shader table a DispatchRays names: which, where it starts (a GPU virtual address), its bytes, and its stride (a
/// ray generation record's is its bytes: it is one record).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Table {
    pub which: Which,
    pub address: u64,
    pub bytes: u64,
    pub stride: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Which {
    #[default]
    RayGeneration,
    Miss,
    HitGroups,
    Callable,
}

impl Table {
    /// The tables of a `D3D12_DISPATCH_RAYS_DESC`'s bytes (as an ExecuteIndirect's record holds one) that hold
    /// any.
    pub fn of_desc(desc: &[u8]) -> Vec<Table> {
        let u64_at = |at: usize| {
            desc.get(at..at + 8)
                .map_or(0, |b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
        };
        let mut out = vec![Table {
            which: Which::RayGeneration,
            address: u64_at(0),
            bytes: u64_at(8),
            stride: u64_at(8),
        }];
        for (which, at) in [(Which::Miss, 16), (Which::HitGroups, 40), (Which::Callable, 64)] {
            out.push(Table {
                which,
                address: u64_at(at),
                bytes: u64_at(at + 8),
                stride: u64_at(at + 16),
            });
        }
        out.retain(|t| t.address != 0 && t.bytes != 0);
        out
    }

    /// The records the table holds: each `stride` bytes (a stride of 0 repeats the first record: one record), as
    /// many as start within it.
    fn records(&self) -> u64 {
        if self.stride == 0 {
            1
        } else {
            self.bytes.div_ceil(self.stride)
        }
    }
}

/// What the records of a dispatch's tables name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Named {
    /// GPU virtual addresses of root CBVs, SRVs and UAVs, and which.
    pub addresses: Vec<(u64, Kind)>,
    /// GPU descriptor handles of descriptor tables, with the table's ranges (as capture decodes root signatures).
    pub handles: Vec<(u64, Value)>,
    /// Records, and arguments, that name nothing that can be placed, by reason.
    pub unplaced: BTreeMap<String, u64>,
}

/// The identifiers the application asked state object `id` (or what it grew from) for, by their bytes: the export
/// each identifies.
fn identifiers(source: &dyn Source, mut id: u64) -> HashMap<[u8; IDENTIFIER], String> {
    let mut out = HashMap::new();
    for _ in 0..64 {
        let Some(d) = source.description(id) else {
            break;
        };
        for (export, hex) in d["identifiers"].as_object().into_iter().flatten() {
            let Some(bytes) = hex.as_str().and_then(unhex) else {
                continue;
            };
            out.entry(bytes).or_insert_with(|| export.clone());
        }
        id = d["parent"].as_u64().unwrap_or(0);
        if id == 0 {
            break;
        }
    }
    out
}

fn unhex(s: &str) -> Option<[u8; IDENTIFIER]> {
    let mut out = [0; IDENTIFIER];
    if s.len() != 2 * IDENTIFIER {
        return None;
    }
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A record of a dispatch's tables that runs a shader: where it starts in the tables' bytes and, where it can be
/// placed, the export its identifier is one the application asked the state object for, and that export's local
/// root arguments (none where it has no local root signature) with the signature they are laid out by.
pub struct Record<'a> {
    pub at: usize,
    pub placed: Option<Placed<'a>>,
}

/// What a record runs: see [`Record`].
pub struct Placed<'a> {
    pub export: &'a str,
    pub arguments: &'a [programs::Argument],
    pub decoded: &'a Value,
}

/// Each record of `tables` (their bytes one after another in `bytes`, as copied) run by state object
/// `state_object` that runs a shader, given to `visit`: its export by its identifier, the export's local root
/// signature (`decode` gives a local root signature's decoded description), and the arguments that signature lays
/// out after the identifier, all within the record. What cannot be placed is counted by reason, as returned, and
/// given to `visit` unplaced (a record shorter than an identifier is not).
pub fn walk(
    source: &dyn Source,
    state_object: u64,
    tables: &[Table],
    bytes: &[u8],
    decode: &mut dyn FnMut(&Local) -> Result<Value, String>,
    visit: &mut dyn FnMut(Record),
) -> BTreeMap<String, u64> {
    let mut unplaced = BTreeMap::<String, u64>::new();
    let mut count = |why: String| *unplaced.entry(why).or_default() += 1;
    if state_object == 0 {
        count("no raytracing state object was set (SetPipelineState1)".into());
        return unplaced;
    }
    let identifiers = identifiers(source, state_object);
    type Layout = Result<(Vec<programs::Argument>, usize, Value), String>;
    let mut layouts = HashMap::<String, Layout>::new();
    let none = (Vec::new(), IDENTIFIER, Value::Null);
    let mut start = 0usize;
    for table in tables {
        let Some(table_bytes) = bytes.get(start..start + table.bytes as usize) else {
            count(format!("{:?} table: its bytes were not all copied", table.which));
            break;
        };
        let base = start;
        start += table.bytes as usize;
        for record in 0..table.records() {
            let at = (record * table.stride) as usize;
            let end = if table.stride == 0 {
                table_bytes.len()
            } else {
                (at + table.stride as usize).min(table_bytes.len())
            };
            let record = &table_bytes[at..end];
            let Some(identifier) = record.get(..IDENTIFIER) else {
                count("a record is shorter than a shader identifier".into());
                continue;
            };
            // A null identifier runs no shader.
            if identifier.iter().all(|&b| b == 0) {
                continue;
            }
            let unplaced = Record {
                at: base + at,
                placed: None,
            };
            let Some(export) = identifiers.get(identifier) else {
                count(
                    "a record's shader identifier is none the application asked the state object for".into(),
                );
                visit(unplaced);
                continue;
            };
            let layout = layouts.entry(export.clone()).or_insert_with(|| {
                let Some(local) = programs::local(source, state_object, export)? else {
                    return Ok(none.clone());
                };
                let decoded = decode(&local)?;
                let (arguments, end) = programs::arguments(&decoded)?;
                Ok((arguments, end, decoded))
            });
            let (arguments, end, decoded) = match layout {
                Ok(l) => l,
                Err(why) => {
                    count(format!("a record's local root signature: {why}"));
                    visit(unplaced);
                    continue;
                }
            };
            if record.len() < *end {
                count("a record is shorter than its local root arguments".into());
                visit(unplaced);
                continue;
            }
            visit(Record {
                at: base + at,
                placed: Some(Placed {
                    export,
                    arguments,
                    decoded,
                }),
            });
        }
    }
    unplaced
}

/// What the records of `tables` (their bytes one after another in `bytes`, as copied) name, run by state object
/// `state_object` (see [`walk`]).
pub fn read(
    source: &dyn Source,
    state_object: u64,
    tables: &[Table],
    bytes: &[u8],
    decode: &mut dyn FnMut(&Local) -> Result<Value, String>,
) -> Named {
    let mut named = Named::default();
    let (addresses, handles) = (&mut named.addresses, &mut named.handles);
    named.unplaced = walk(source, state_object, tables, bytes, decode, &mut |r| {
        let Some(p) = r.placed else { return };
        for a in p.arguments {
            if matches!(a.kind, Kind::Constants { .. }) {
                continue;
            }
            let at = r.at + a.at;
            let value = u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"));
            // A null descriptor or handle names nothing (and a shader that reaches it reads zeros, or faults).
            if value == 0 {
                continue;
            }
            if a.kind == Kind::Table {
                handles.push((
                    value,
                    p.decoded["parameters"][a.parameter as usize]["value"].clone(),
                ));
            } else {
                addresses.push((value, a.kind));
            }
        }
    });
    named.addresses.sort_unstable();
    named.addresses.dedup();
    named
}

/// The heap slot GPU descriptor handle `handle` is in, among the shader-visible heaps `heaps` describes (by id:
/// their `gpu_start`, `count` and `stride`).
pub fn slot<'a>(heaps: impl IntoIterator<Item = (u64, &'a Value)>, handle: u64) -> Option<(u64, u32)> {
    heaps.into_iter().find_map(|(id, d)| {
        let (start, count, stride) = (
            d["gpu_start"].as_u64()?,
            d["count"].as_u64()?,
            d["stride"].as_u64()?,
        );
        let offset = handle.checked_sub(start).filter(|_| start != 0 && stride != 0)?;
        (offset < count * stride && offset % stride == 0).then(|| (id, (offset / stride) as u32))
    })
}

/// The descriptor slots a table at `(heap, base)` of `ranges` (`D3D12_DESCRIPTOR_RANGE1`s as decoded) covers, each
/// range's first and end slot and its descriptors' kind; an unbounded range runs to the heap's end (`count`).
pub fn ranges(ranges: &Value, base: u32, count: u32) -> Vec<(u32, u32, &'static str)> {
    let mut append = 0u64;
    let mut out = Vec::new();
    for range in ranges.as_array().into_iter().flatten() {
        let offset = range["OffsetInDescriptorsFromTableStart"].as_u64().unwrap_or(0);
        let offset = if offset == u32::MAX as u64 { append } else { offset };
        let n = range["NumDescriptors"].as_u64().unwrap_or(0);
        let start = u64::from(base).saturating_add(offset);
        let end = if n == u32::MAX as u64 {
            u64::from(count)
        } else {
            start.saturating_add(n).min(u64::from(count))
        };
        let kind = match range["RangeType"].as_u64() {
            Some(0) => "srv",
            Some(1) => "uav",
            Some(2) => "cb",
            Some(3) => "sampler",
            _ => "",
        };
        out.push((start.min(u64::from(count)) as u32, end as u32, kind));
        append = offset.saturating_add(n);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d3d12::programs::Declared;
    use serde_json::json;
    use std::sync::Arc;

    struct One(Value);

    impl Source for One {
        fn description(&self, id: u64) -> Option<Value> {
            (id == 1).then(|| self.0.clone())
        }
        fn declared(&self, _: &str) -> Result<Arc<Declared>, String> {
            Ok(Arc::new(Declared {
                shaders: vec!["gen".into(), "miss".into()],
                subobjects: Vec::new(),
            }))
        }
    }

    fn hex(b: u8) -> String {
        format!("{b:02x}").repeat(IDENTIFIER)
    }

    #[test]
    fn records_name_their_local_arguments_by_their_exports_signature() {
        // `gen` runs with root signature #7 (a constant, a root SRV, a descriptor table); `miss` with none.
        let so = json!({"kind": "state_object", "parent": 0, "subobjects": [
            {"type": 5, "value": {"library": {"blob": "lib", "size": 1}, "exports": []}},
            {"type": 2, "value": 7},
            {"type": 7, "value": {"subobject": 1, "exports": ["gen"]}},
        ], "identifiers": {"gen": hex(0xaa), "miss": hex(0xbb)}});
        let decoded = json!({"parameters": [
            {"type": 1, "value": {"Num32BitValues": 1}},
            {"type": 3, "value": {}},
            {"type": 0, "value": [{"RangeType": 1, "NumDescriptors": 2, "OffsetInDescriptorsFromTableStart": 0}]},
        ]});
        let mut decodes = 0;
        let mut decode = |l: &Local| {
            decodes += 1;
            assert_eq!(l, &Local::Object(7));
            Ok(decoded.clone())
        };
        // A 64-byte ray generation record: identifier, constant at 32, SRV at 40, table at 48.
        let mut raygen = vec![0xaa; IDENTIFIER];
        raygen.extend(5u32.to_le_bytes());
        raygen.extend([0; 4]);
        raygen.extend(0x1000u64.to_le_bytes());
        raygen.extend(0x2000u64.to_le_bytes());
        raygen.extend([0; 8]);
        // A miss table of three 32-byte records: `miss`, a null record, and garbage no ray reaches.
        let mut miss = vec![0xbb; IDENTIFIER];
        miss.extend([0; IDENTIFIER]);
        miss.extend([0x5a; IDENTIFIER]);
        // A second ray generation record of `gen`, whose SRV is null.
        let mut again = raygen.clone();
        again[40..48].fill(0);
        let tables = [
            Table {
                which: Which::RayGeneration,
                address: 1,
                bytes: 64,
                stride: 64,
            },
            Table {
                which: Which::Miss,
                address: 2,
                bytes: 96,
                stride: 32,
            },
            Table {
                which: Which::Callable,
                address: 3,
                bytes: 64,
                stride: 64,
            },
        ];
        let bytes = [raygen, miss, again].concat();
        let named = read(&One(so), 1, &tables, &bytes, &mut decode);
        assert_eq!(named.addresses, [(0x1000, Kind::ShaderResource)]);
        assert_eq!(named.handles.len(), 2);
        assert!(
            named
                .handles
                .iter()
                .all(|(h, r)| *h == 0x2000 && r[0]["NumDescriptors"] == 2)
        );
        assert_eq!(
            named.unplaced,
            BTreeMap::from([(
                "a record's shader identifier is none the application asked the state object for".to_string(),
                1
            )])
        );
        assert_eq!(decodes, 1, "one decode per export");
    }

    /// Every record that runs a shader is visited, those that cannot be placed too (replay nulls their identifiers):
    /// a null record is not.
    #[test]
    fn records_that_cannot_be_placed_are_visited_unplaced() {
        let so = json!({"kind": "state_object", "parent": 0, "subobjects": [
            {"type": 5, "value": {"library": {"blob": "lib", "size": 1}, "exports": []}},
            {"type": 2, "value": 7},
            {"type": 7, "value": {"subobject": 1, "exports": ["gen"]}},
        ], "identifiers": {"gen": hex(0xaa), "miss": hex(0xbb)}});
        // `gen`'s signature lays out a root SRV after its identifier, which its 32-byte record has no room for.
        let mut decode = |_: &Local| Ok(json!({"parameters": [{"type": 3, "value": {}}]}));
        let table = Table {
            which: Which::Miss,
            address: 1,
            bytes: 128,
            stride: 32,
        };
        let bytes = [
            [0xbb; IDENTIFIER],
            [0; IDENTIFIER],
            [0x5a; IDENTIFIER],
            [0xaa; IDENTIFIER],
        ]
        .concat();
        let mut visited = Vec::new();
        let unplaced = walk(&One(so), 1, &[table], &bytes, &mut decode, &mut |r| {
            visited.push((r.at, r.placed.map(|p| p.export.to_string())))
        });
        assert_eq!(visited, [(0, Some("miss".to_string())), (64, None), (96, None)]);
        assert_eq!(unplaced.values().sum::<u64>(), 2);
    }

    #[test]
    fn short_records_and_copies_are_counted() {
        let so = json!({"kind": "state_object", "parent": 0, "subobjects": [
            {"type": 5, "value": {"library": {"blob": "lib", "size": 1}, "exports": []}},
            {"type": 2, "value": 7},
        ], "identifiers": {"gen": hex(0xaa)}});
        let mut decode = |_: &Local| Ok(json!({"parameters": [{"type": 3, "value": {}}]}));
        let table = Table {
            which: Which::RayGeneration,
            address: 1,
            bytes: 32,
            stride: 32,
        };
        let named = read(&One(so.clone()), 1, &[table], &[0xaa; 32], &mut decode);
        assert_eq!(
            named.unplaced["a record is shorter than its local root arguments"],
            1
        );
        let named = read(&One(so), 1, &[table], &[0xaa; 16], &mut decode);
        assert_eq!(
            named.unplaced["RayGeneration table: its bytes were not all copied"],
            1
        );
    }

    #[test]
    fn desc_bytes_give_the_tables_that_hold_records() {
        let mut desc = vec![0u8; 104];
        desc[0..8].copy_from_slice(&0x100u64.to_le_bytes());
        desc[8..16].copy_from_slice(&64u64.to_le_bytes());
        desc[40..48].copy_from_slice(&0x200u64.to_le_bytes());
        desc[48..56].copy_from_slice(&96u64.to_le_bytes());
        desc[56..64].copy_from_slice(&32u64.to_le_bytes());
        assert_eq!(
            Table::of_desc(&desc),
            [
                Table {
                    which: Which::RayGeneration,
                    address: 0x100,
                    bytes: 64,
                    stride: 64
                },
                Table {
                    which: Which::HitGroups,
                    address: 0x200,
                    bytes: 96,
                    stride: 32
                },
            ]
        );
    }

    #[test]
    fn handles_find_their_heap_slot_and_ranges_their_slots() {
        let heap = json!({"gpu_start": 0x10000, "count": 8, "stride": 32});
        let heaps = [(4, &heap)];
        assert_eq!(slot(heaps, 0x10000 + 3 * 32), Some((4, 3)));
        assert_eq!(slot(heaps, 0x10000 + 8 * 32), None);
        assert_eq!(slot(heaps, 0x10000 + 5), None);
        let r = json!([
            {"RangeType": 0, "NumDescriptors": 2, "OffsetInDescriptorsFromTableStart": u32::MAX},
            {"RangeType": 1, "NumDescriptors": u32::MAX, "OffsetInDescriptorsFromTableStart": u32::MAX},
        ]);
        assert_eq!(ranges(&r, 3, 8), [(3, 5, "srv"), (5, 8, "uav")]);
    }
}
