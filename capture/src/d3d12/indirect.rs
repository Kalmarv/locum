//! ExecuteIndirect's argument records. A command signature can set a vertex or index buffer view or a root CBV, SRV
//! or UAV per record, and a DispatchRays argument names its shader tables, and each of those is a GPU virtual address
//! in the argument buffer: capture reads the records to learn which buffers they name (and which a root UAV lets the
//! call write), and replay rewrites them to its own buffers' addresses.

use core::mem::offset_of;

use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::D3D12_DISPATCH_RAYS_DESC;

/// What an address in a record sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    VertexBuffer,
    IndexBuffer,
    ConstantBuffer,
    ShaderResource,
    /// A root UAV: the call may write what it names.
    UnorderedAccess,
    /// A shader table of a DispatchRays argument (its ray generation record, or its miss, hit group or callable
    /// table): the call reads its shader records.
    ShaderTable,
}

/// An address's byte offset in a record, and what it sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Address {
    pub at: usize,
    pub kind: Kind,
}

/// Where in a record the signature puts each address.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub stride: usize,
    pub addresses: Vec<Address>,
}

/// The bytes of a DispatchRays argument: the structure's size, its three 4-byte sizes padded to its 8-byte alignment,
/// as the runtime and vkd3d-proton size it.
const DISPATCH_RAYS: usize = size_of::<D3D12_DISPATCH_RAYS_DESC>();
/// Where a DispatchRays argument keeps its tables' addresses (each table's range starts with its address).
const SHADER_TABLES: [usize; 4] = [
    offset_of!(D3D12_DISPATCH_RAYS_DESC, RayGenerationShaderRecord),
    offset_of!(D3D12_DISPATCH_RAYS_DESC, MissShaderTable),
    offset_of!(D3D12_DISPATCH_RAYS_DESC, HitGroupTable),
    offset_of!(D3D12_DISPATCH_RAYS_DESC, CallableShaderTable),
];

/// Each argument type's bytes in a record, and where addresses are among them (D3D12's argument structures, packed
/// in the order the signature gives them).
fn argument(a: &Value) -> Result<(usize, Vec<Address>), String> {
    let t = a["type"].as_i64().unwrap_or(-1);
    let at = |kind| vec![Address { at: 0, kind }];
    let none = Vec::new;
    Ok(match t {
        0 => (16, none()), // DRAW
        1 => (20, none()), // DRAW_INDEXED
        2 => (12, none()), // DISPATCH
        // VERTEX_BUFFER_VIEW, INDEX_BUFFER_VIEW: the address, then size and stride or format.
        3 => (16, at(Kind::VertexBuffer)),
        4 => (16, at(Kind::IndexBuffer)),
        5 => (4 * a["count"].as_u64().unwrap_or(0) as usize, none()), // CONSTANT
        6 => (8, at(Kind::ConstantBuffer)),
        7 => (8, at(Kind::ShaderResource)),
        8 => (8, at(Kind::UnorderedAccess)),
        // DISPATCH_RAYS
        9 => (
            DISPATCH_RAYS,
            SHADER_TABLES
                .iter()
                .map(|&at| Address {
                    at,
                    kind: Kind::ShaderTable,
                })
                .collect(),
        ),
        10 => (12, none()), // DISPATCH_MESH
        // INCREMENTING_CONSTANT: the runtime sets the root constant to each command's index; it has no bytes in a
        // record.
        11 => (0, none()),
        t => return Err(format!("argument type {t}, which locum does not know")),
    })
}

/// A signature's record layout from its description (`d12_objects`: `stride` and `arguments`).
pub fn layout(signature: &Value) -> Result<Layout, String> {
    let mut at = 0;
    let mut addresses = Vec::new();
    for a in signature["arguments"].as_array().into_iter().flatten() {
        let (bytes, here) = argument(a)?;
        addresses.extend(here.into_iter().map(|address| Address {
            at: at + address.at,
            ..address
        }));
        at += bytes;
    }
    let stride = signature["stride"].as_u64().unwrap_or(0) as usize;
    if stride < at {
        return Err(format!(
            "its stride {stride} is shorter than its arguments' {at} bytes"
        ));
    }
    Ok(Layout { stride, addresses })
}

/// The root parameters a signature's records set (root constants, CBVs, SRVs and UAVs), whatever else it holds: for
/// the call, these come from each record, not from what the list set before it.
pub fn root_parameters(signature: &Value) -> Vec<u32> {
    let mut out: Vec<u32> = signature["arguments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| matches!(a["type"].as_i64(), Some(5..=8 | 11)))
        .filter_map(|a| a["root_parameter"].as_u64())
        .map(|p| p as u32)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// Calls `f` with each nonzero address in the first `records` records of `bytes` (an address of 0 is a null view) and
/// what it sets; records past the end of `bytes` are not read.
pub fn addresses(bytes: &[u8], layout: &Layout, records: usize, mut f: impl FnMut(u64, Kind)) {
    for record in 0..records {
        for a in &layout.addresses {
            let at = record * layout.stride + a.at;
            if let Some(field) = bytes.get(at..at + 8) {
                let address = u64::from_le_bytes(field.try_into().expect("8 bytes"));
                if address != 0 {
                    f(address, a.kind);
                }
            }
        }
    }
}

/// The shader tables the DispatchRays argument of each of the first `records` records of `bytes` names (see
/// [`super::tables::Table::of_desc`]), where the signature's records hold one; records past the end of `bytes` name
/// none.
pub fn shader_tables(bytes: &[u8], layout: &Layout, records: usize) -> Vec<Vec<super::tables::Table>> {
    // The ray generation record's range starts the argument.
    let Some(start) = layout
        .addresses
        .iter()
        .find(|a| a.kind == Kind::ShaderTable)
        .map(|a| a.at)
    else {
        return Vec::new();
    };
    (0..records)
        .map(|record| {
            let at = record * layout.stride + start;
            bytes
                .get(at..at + DISPATCH_RAYS)
                .map_or_else(Vec::new, super::tables::Table::of_desc)
        })
        .collect()
}

/// Rewrites the addresses of the first `records` records of `bytes` by `map` (an address of 0 stays 0). Fails at the
/// first address `map` cannot place, naming the record.
pub fn patch(
    bytes: &mut [u8],
    layout: &Layout,
    records: usize,
    mut map: impl FnMut(u64) -> Result<u64, String>,
) -> Result<usize, String> {
    let mut patched = 0;
    for record in 0..records {
        for a in &layout.addresses {
            let at = record * layout.stride + a.at;
            let Some(field) = bytes.get_mut(at..at + 8) else {
                return Err(format!("record {record} runs past the argument buffer"));
            };
            let game = u64::from_le_bytes(field.try_into().expect("8 bytes"));
            if game == 0 {
                continue;
            }
            let mine = map(game).map_err(|e| format!("record {record}: {e}"))?;
            field.copy_from_slice(&mine.to_le_bytes());
            patched += 1;
        }
    }
    Ok(patched)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(at: usize, kind: Kind) -> Address {
        Address { at, kind }
    }

    #[test]
    fn layouts_follow_the_argument_structures() {
        let s = json!({"stride": 80, "arguments": [{"type": 3, "slot": 0}, {"type": 6, "root_parameter": 0},
            {"type": 5, "root_parameter": 1, "count": 2}, {"type": 7, "root_parameter": 2}, {"type": 4},
            {"type": 8, "root_parameter": 3}, {"type": 0}]});
        assert_eq!(
            layout(&s).unwrap(),
            Layout {
                stride: 80,
                addresses: vec![
                    at(0, Kind::VertexBuffer),
                    at(16, Kind::ConstantBuffer),
                    at(32, Kind::ShaderResource),
                    at(40, Kind::IndexBuffer),
                    at(56, Kind::UnorderedAccess),
                ]
            }
        );
        assert_eq!(root_parameters(&s), vec![0, 1, 2, 3]);
        let draws = json!({"stride": 20, "arguments": [{"type": 1}]});
        assert_eq!(layout(&draws).unwrap().addresses, Vec::new());
        assert_eq!(root_parameters(&draws), Vec::<u32>::new());
        assert!(layout(&json!({"stride": 8, "arguments": [{"type": 6}, {"type": 0}]})).is_err());
        // An incrementing constant takes no bytes of a record, and sets its root parameter.
        let incrementing = json!({"stride": 24, "arguments": [{"type": 11, "root_parameter": 4, "offset": 1},
            {"type": 6, "root_parameter": 0}, {"type": 0}]});
        assert_eq!(
            layout(&incrementing).unwrap(),
            Layout {
                stride: 24,
                addresses: vec![at(0, Kind::ConstantBuffer)]
            }
        );
        assert_eq!(root_parameters(&incrementing), vec![0, 4]);
        // A DispatchRays argument (104 bytes) names its four tables, after a root constant's 4 bytes.
        assert_eq!((DISPATCH_RAYS, SHADER_TABLES), (104, [0, 16, 40, 64]));
        let rays =
            json!({"stride": 108, "arguments": [{"type": 5, "root_parameter": 1, "count": 1}, {"type": 9}]});
        assert_eq!(
            layout(&rays).unwrap(),
            Layout {
                stride: 108,
                addresses: [4, 20, 44, 68].map(|a| at(a, Kind::ShaderTable)).to_vec()
            }
        );
        assert!(
            layout(&json!({"stride": 104, "arguments": [{"type": 5, "count": 1}, {"type": 9}]})).is_err()
        );
        // Each record's argument gives its tables: here one record's ray generation record and hit group table.
        let mut record = vec![0u8; 108 * 2];
        for (at, v) in [(4, 0x100u64), (12, 64), (44, 0x200), (52, 96), (60, 32)] {
            record[at..at + 8].copy_from_slice(&v.to_le_bytes());
        }
        let found = shader_tables(&record, &layout(&rays).unwrap(), 3);
        use super::super::tables::{Table, Which};
        assert_eq!(
            found,
            [
                vec![
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
                    }
                ],
                Vec::new(),
                Vec::new()
            ]
        );
        assert!(shader_tables(&record, &layout(&draws).unwrap(), 1).is_empty());
    }

    #[test]
    fn addresses_are_read_up_to_the_records_and_the_bytes() {
        let layout = Layout {
            stride: 24,
            addresses: vec![at(0, Kind::VertexBuffer), at(16, Kind::UnorderedAccess)],
        };
        let mut bytes = vec![0u8; 64];
        for (at, va) in [(0, 0x1000u64), (16, 0x1100), (24, 0x2000), (48, 0x3000)] {
            bytes[at..at + 8].copy_from_slice(&va.to_le_bytes());
        }
        let mut seen = Vec::new();
        addresses(&bytes, &layout, 2, |a, k| seen.push((a, k)));
        assert_eq!(
            seen,
            vec![
                (0x1000, Kind::VertexBuffer),
                (0x1100, Kind::UnorderedAccess),
                (0x2000, Kind::VertexBuffer)
            ]
        );
        // The third record's second address would run past the bytes.
        seen.clear();
        addresses(&bytes, &layout, 3, |a, _| seen.push((a, Kind::VertexBuffer)));
        assert_eq!(seen.len(), 4);
    }

    #[test]
    fn records_are_patched_up_to_the_count() {
        let layout = Layout {
            stride: 24,
            addresses: vec![at(0, Kind::IndexBuffer), at(16, Kind::ShaderResource)],
        };
        let mut bytes = vec![0u8; 72];
        for (record, va) in [(0, 0x1000u64), (1, 0x2000), (2, 0x3000)] {
            bytes[record * 24..record * 24 + 8].copy_from_slice(&va.to_le_bytes());
        }
        let n = patch(&mut bytes, &layout, 2, |va| Ok(va + 0x10_0000)).unwrap();
        assert_eq!(n, 2);
        let read = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
        assert_eq!(
            (read(0), read(16), read(24), read(48)),
            (0x10_1000, 0, 0x10_2000, 0x3000)
        );
        let e = patch(&mut bytes, &layout, 3, |va| {
            if va == 0x3000 {
                Err("no buffer".into())
            } else {
                Ok(va)
            }
        });
        assert_eq!(e, Err("record 2: no buffer".into()));
        assert!(patch(&mut [0u8; 8], &layout, 1, Ok).is_err());
    }
}
