//! ExecuteIndirect's argument records. A command signature can set a vertex or index buffer view or a root CBV, SRV
//! or UAV per record, and each of those is a GPU virtual address in the argument buffer: capture reads the records to
//! learn which buffers they name, and replay rewrites them to its own buffers' addresses.

use serde_json::Value;

/// Where in a record the signature puts each address: byte offsets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    pub stride: usize,
    pub addresses: Vec<usize>,
}

/// Each argument type's bytes in a record, and where an address is among them (D3D12's argument structures, packed
/// in the order the signature gives them).
fn argument(a: &Value) -> Result<(usize, Option<usize>), String> {
    let t = a["type"].as_i64().unwrap_or(-1);
    Ok(match t {
        0 => (16, None),                                            // DRAW
        1 => (20, None),                                            // DRAW_INDEXED
        2 => (12, None),                                            // DISPATCH
        3 | 4 => (16, Some(0)), // VERTEX_BUFFER_VIEW, INDEX_BUFFER_VIEW: the address, then size and stride/format
        5 => (4 * a["count"].as_u64().unwrap_or(0) as usize, None), // CONSTANT
        6..=8 => (8, Some(0)),  // CONSTANT_BUFFER_VIEW, SHADER_RESOURCE_VIEW, UNORDERED_ACCESS_VIEW
        10 => (12, None),       // DISPATCH_MESH
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
        if let Some(o) = address {
            addresses.push(at + o);
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

/// Calls `f` with each nonzero address in the first `records` records of `bytes` (an address of 0 is a null view);
/// records past the end of `bytes` are not read.
pub fn addresses(bytes: &[u8], layout: &Layout, records: usize, mut f: impl FnMut(u64)) {
    for record in 0..records {
        for &at in &layout.addresses {
            let at = record * layout.stride + at;
            if let Some(field) = bytes.get(at..at + 8) {
                let address = u64::from_le_bytes(field.try_into().expect("8 bytes"));
                if address != 0 {
                    f(address);
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
        for &at in &layout.addresses {
            let at = record * layout.stride + at;
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

    #[test]
    fn layouts_follow_the_argument_structures() {
        let s = json!({"stride": 56, "arguments": [{"type": 3, "slot": 0}, {"type": 6, "root_parameter": 0},
            {"type": 5, "count": 2}, {"type": 7}, {"type": 0}]});
        assert_eq!(
            layout(&s).unwrap(),
            Layout {
                stride: 56,
                addresses: vec![0, 16, 32]
            }
        );
        let draws = json!({"stride": 20, "arguments": [{"type": 1}]});
        assert_eq!(layout(&draws).unwrap().addresses, Vec::<usize>::new());
        assert!(layout(&json!({"stride": 8, "arguments": [{"type": 6}, {"type": 0}]})).is_err());
        assert!(layout(&json!({"stride": 64, "arguments": [{"type": 11}, {"type": 0}]})).is_err());
    }

    #[test]
    fn addresses_are_read_up_to_the_records_and_the_bytes() {
        let layout = Layout {
            stride: 24,
            addresses: vec![0, 16],
        };
        let mut bytes = vec![0u8; 64];
        for (at, va) in [(0, 0x1000u64), (16, 0x1100), (24, 0x2000), (48, 0x3000)] {
            bytes[at..at + 8].copy_from_slice(&va.to_le_bytes());
        }
        let mut seen = Vec::new();
        addresses(&bytes, &layout, 2, |a| seen.push(a));
        assert_eq!(seen, vec![0x1000, 0x1100, 0x2000]);
        // The third record's second address would run past the bytes.
        seen.clear();
        addresses(&bytes, &layout, 3, |a| seen.push(a));
        assert_eq!(seen, vec![0x1000, 0x1100, 0x2000, 0x3000]);
    }

    #[test]
    fn records_are_patched_up_to_the_count() {
        let layout = Layout {
            stride: 24,
            addresses: vec![0, 16],
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
