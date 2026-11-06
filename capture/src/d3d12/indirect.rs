//! ExecuteIndirect's argument records. A command signature can set a vertex or index buffer view or a root CBV, SRV
//! or UAV per record, and each of those is a GPU virtual address in the argument buffer: capture reads the records to
//! learn which buffers they name (and which a root UAV lets the call write), and replay rewrites them to its own
//! buffers' addresses.

use serde_json::Value;

/// What an address in a record sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    VertexBuffer,
    IndexBuffer,
    ConstantBuffer,
    ShaderResource,
    /// A root UAV: the call may write what it names.
    UnorderedAccess,
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

/// Each argument type's bytes in a record, and where an address is among them (D3D12's argument structures, packed
/// in the order the signature gives them).
fn argument(a: &Value) -> Result<(usize, Option<Address>), String> {
    let t = a["type"].as_i64().unwrap_or(-1);
    let at = |kind| Some(Address { at: 0, kind });
    Ok(match t {
        0 => (16, None), // DRAW
        1 => (20, None), // DRAW_INDEXED
        2 => (12, None), // DISPATCH
        // VERTEX_BUFFER_VIEW, INDEX_BUFFER_VIEW: the address, then size and stride or format.
        3 => (16, at(Kind::VertexBuffer)),
        4 => (16, at(Kind::IndexBuffer)),
        5 => (4 * a["count"].as_u64().unwrap_or(0) as usize, None), // CONSTANT
        6 => (8, at(Kind::ConstantBuffer)),
        7 => (8, at(Kind::ShaderResource)),
        8 => (8, at(Kind::UnorderedAccess)),
        10 => (12, None), // DISPATCH_MESH
        9 => return Err("a DispatchRays argument, whose records locum does not read".into()),
        11 => return Err("an incrementing constant, whose place in a record locum does not know".into()),
        t => return Err(format!("argument type {t}, which locum does not know")),
    })
}

/// A signature's record layout from its description (`d12_objects`: `stride` and `arguments`).
pub fn layout(signature: &Value) -> Result<Layout, String> {
    let mut at = 0;
    let mut addresses = Vec::new();
    for a in signature["arguments"].as_array().into_iter().flatten() {
        let (bytes, address) = argument(a)?;
        if let Some(address) = address {
            addresses.push(Address {
                at: at + address.at,
                ..address
            });
        }
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
        // An incrementing constant's records are not read, but the parameter it sets is known.
        let incrementing =
            json!({"stride": 64, "arguments": [{"type": 11, "root_parameter": 4}, {"type": 0}]});
        assert!(layout(&incrementing).is_err());
        assert_eq!(root_parameters(&incrementing), vec![4]);
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
