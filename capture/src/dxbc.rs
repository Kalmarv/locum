//! Instruction-level detection of potentially order-dependent atomics: SM4/5 programs over dx's `dxbc-reader`
//! decoding, DXIL programs over its `dxil-reader`.

/// The key (XXH3-128, as blob keys are) of a program's code alone: its `SHEX`/`SHDR` part, or its `DXIL` part. Two
/// programs built from the same source under other names, or whose reflection or debug parts differ, have the same
/// code key where their blob keys differ. None for bytes that are not a shader container with code.
pub fn code_key(container: &[u8]) -> Option<String> {
    let parts = dxbc_reader::parts(container).ok()?;
    let code = parts
        .iter()
        .find(|p| matches!(&p.kind, b"DXIL" | b"SHEX" | b"SHDR"))?;
    Some(format!("{:032x}", xxhash_rust::xxh3::xxh3_128(code.data)))
}

/// One kind of atomic instruction, counted statically (not by executions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtomicOp {
    /// The SM4/5 opcode; in DXIL, the `dx.op` code (0 for LLVM's own `atomicrmw` and `cmpxchg`).
    pub opcode: u16,
    pub name: &'static str,
    pub count: usize,
}

/// Potentially order-dependent atomics, in ascending opcode order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderDependence {
    pub ops: Vec<AtomicOp>,
}

/// The atomics whose effect can depend on which thread gets there first: the conditional store (172) and every atomic
/// that returns the value it replaced (178 to 189, `imm_atomic_*`). D3D10_SB_OPCODE_TYPE in Microsoft's
/// D3D12TokenizedProgramFormat.hpp.
const ATOMICS: [u16; 13] = [172, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189];

/// Find atomics whose returned old value or conditional store can depend on
/// thread scheduling. This is a conservative opcode scan, not a data-flow proof:
/// dead code, unused return values and uncontended addresses still count.
/// Non-returning integer and/or/xor/add/min/max atomics are not flagged.
/// Racing plain stores to one address cannot be detected by this scan; `None`
/// is therefore not a guarantee of deterministic output.
///
/// The program is decoded in full (`dxbc_reader::decode`): a container or token stream it cannot read is an error
/// naming the word where reading stopped. This is not a replacement for the D3D bytecode validator.
pub fn order_dependent(bytecode: &[u8]) -> Result<Option<OrderDependence>, String> {
    if dxil_reader::parts(bytecode).is_ok_and(|parts| parts.iter().any(|p| &p.kind == b"DXIL")) {
        return dxil(bytecode);
    }
    let words = dxbc_reader::code(bytecode).map_err(|e| e.to_string())?;
    let program = dxbc_reader::decode(&words).map_err(|e| e.to_string())?;
    let ops: Vec<_> = ATOMICS
        .iter()
        .filter_map(|&opcode| {
            let count = program.instructions.iter().filter(|i| i.opcode == opcode).count();
            (count != 0).then(|| AtomicOp {
                opcode,
                name: dxbc_reader::opcode_name(opcode),
                count,
            })
        })
        .collect();
    Ok((!ops.is_empty()).then_some(OrderDependence { ops }))
}

/// The DXIL counterpart: the `dx.op` atomics that return the value they replaced (`atomicBinOp` 78,
/// `atomicCompareExchange` 79) and the counter's `bufferUpdateCounter` 70 (append and consume), and LLVM's
/// `atomicrmw` and `cmpxchg` on groupshared memory, each where its result is used. DXIL's atomics always return a
/// value; an unused one is SM5's non-returning form. Conservative as the SM4/5 scan is.
fn dxil(bytecode: &[u8]) -> Result<Option<OrderDependence>, String> {
    use dxil_reader::module::Op;
    let parts = dxil_reader::parts(bytecode).map_err(|e| e.to_string())?;
    let module = dxil_reader::module(&parts, b"DXIL")
        .map_err(|e| e.to_string())?
        .ok_or("no DXIL part")?;
    let mut counts: std::collections::BTreeMap<(u16, &'static str), usize> = Default::default();
    for f in &module.functions {
        let used: std::collections::HashSet<_> =
            f.instructions.iter().flat_map(|i| i.op.operands()).collect();
        let constant = |id: dxil_reader::module::ValueId| {
            let value = match id.checked_sub(f.first) {
                None => module.values.get(id),
                Some(local) => f.locals.get(local),
            };
            match value {
                Some(dxil_reader::module::Value::Constant(dxil_reader::module::Constant::Integer(i))) => {
                    Some(i)
                }
                _ => None,
            }
        };
        for inst in &f.instructions {
            if !inst.result.is_some_and(|(r, _)| used.contains(&r)) {
                continue;
            }
            let op = match &inst.op {
                Op::Call { function, args, .. }
                    if module.name(*function).is_some_and(|n| n.starts_with("dx.op.")) =>
                {
                    match args.first().and_then(|&a| constant(a)) {
                        Some(70) => (70, "bufferUpdateCounter"),
                        Some(78) => (78, "atomicBinOp"),
                        Some(79) => (79, "atomicCompareExchange"),
                        _ => continue,
                    }
                }
                Op::AtomicRmw { .. } => (0, "atomicrmw"),
                Op::CmpXchg { .. } => (0, "cmpxchg"),
                _ => continue,
            };
            *counts.entry(op).or_default() += 1;
        }
    }
    let ops: Vec<_> = counts
        .into_iter()
        .map(|((opcode, name), count)| AtomicOp { opcode, name, count })
        .collect();
    Ok((!ops.is_empty()).then_some(OrderDependence { ops }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fxc 10.0.26100.0 `cs_5_0` of
    /// `RWByteAddressBuffer b : register(u0); [numthreads(1, 1, 1)] void main() { uint o; b.InterlockedAdd(0, 1, o);
    /// b.InterlockedOr(4, 1); b.InterlockedCompareStore(8, 0, 1); b.Store(12, o); }`, its token stream after the
    /// version and length.
    const DECLARATIONS: [u32; 10] = [
        0x0100086a, 0x0300009d, 0x0011e000, 0x00000000, 0x02000068, 0x00000001, 0x0400009b, 0x00000001,
        0x00000001, 0x00000001,
    ];
    /// `imm_atomic_iadd r0.x, u0, l(0), l(1)`
    const IADD: [u32; 9] = [
        0x090000b4, 0x00100012, 0x00000000, 0x0011e000, 0x00000000, 0x00004001, 0x00000000, 0x00004001,
        0x00000001,
    ];
    /// `atomic_or u0, l(4), l(1)`, `atomic_cmp_store u0, l(8), l(0), l(1)`, `store_raw u0.x, l(12), r0.x`
    const REST: [u32; 23] = [
        0x070000aa, 0x0011e000, 0x00000000, 0x00004001, 0x00000004, 0x00004001, 0x00000001, 0x090000ac,
        0x0011e000, 0x00000000, 0x00004001, 0x00000008, 0x00004001, 0x00000000, 0x00004001, 0x00000001,
        0x070000a6, 0x0011e012, 0x00000000, 0x00004001, 0x0000000c, 0x0010000a, 0x00000000,
    ];
    const RET: u32 = 0x0100003e;

    fn container(tag: &[u8; 4], version: u32, instructions: &[u32]) -> Vec<u8> {
        // An unrelated chunk precedes the shader, as the parts of a compiler's container lie one after another.
        let mut bytes = b"DXBC".to_vec();
        bytes.extend([0; 16]);
        for word in [1, 64 + instructions.len() as u32 * 4, 2, 40, 48] {
            bytes.extend(word.to_le_bytes());
        }
        bytes.extend(b"TEST\0\0\0\0");
        bytes.extend(tag);
        bytes.extend((8 + instructions.len() as u32 * 4).to_le_bytes());
        bytes.extend(version.to_le_bytes());
        bytes.extend((2 + instructions.len() as u32).to_le_bytes());
        for word in instructions {
            bytes.extend(word.to_le_bytes());
        }
        bytes
    }

    fn program(iadds: usize, tail: &[u32]) -> Vec<u32> {
        let mut words = DECLARATIONS.to_vec();
        for _ in 0..iadds {
            words.extend(IADD);
        }
        words.extend(REST);
        words.extend(tail);
        words.push(RET);
        words
    }

    /// dxc 10.0.26100 `cs_6_0` of `RWByteAddressBuffer b : register(u0); groupshared uint g;
    /// [numthreads(1, 1, 1)] void main() { uint o; b.InterlockedAdd(0, 1, o); b.InterlockedOr(4, 1); uint s;
    /// InterlockedAdd(g, 1, s); b.Store(12, o + s); }`, and of one with only the `InterlockedOr` and a store of 7.
    #[test]
    fn dxil_atomics_count_where_their_results_are_used() {
        let found = order_dependent(include_bytes!("testdata/dxil-atomics.cso"))
            .unwrap()
            .unwrap();
        // InterlockedOr's atomicBinOp returns a value nothing reads.
        assert_eq!(
            found.ops,
            [
                AtomicOp {
                    opcode: 0,
                    name: "atomicrmw",
                    count: 1
                },
                AtomicOp {
                    opcode: 78,
                    name: "atomicBinOp",
                    count: 1
                }
            ]
        );
        assert_eq!(
            order_dependent(include_bytes!("testdata/dxil-plain.cso")),
            Ok(None)
        );
        // Its one UAV, u0 in space 0, as its PSV0 part declares it.
        assert_eq!(
            crate::rdef::uav_registers(include_bytes!("testdata/dxil-atomics.cso")),
            Some(vec![(0, 0, 0)])
        );
    }

    #[test]
    fn accepts_sm4_and_empty_programs() {
        // a pixel program of each model that only returns
        for version in [0x40u32, 0x41, 0x50, 0x51] {
            assert_eq!(order_dependent(&container(b"SHDR", version, &[RET])), Ok(None));
        }
        assert_eq!(order_dependent(&container(b"SHEX", 0x5_0050, &[])), Ok(None));
    }

    #[test]
    fn reports_the_returning_atomics_and_the_conditional_store_with_their_counts() {
        for tag in [b"SHDR", b"SHEX"] {
            let found = order_dependent(&container(tag, 0x5_0050, &program(3, &[])))
                .unwrap()
                .unwrap();
            // atomic_or does not return what it replaced, so the order its threads run in does not show
            assert_eq!(
                found.ops,
                [
                    AtomicOp {
                        opcode: 172,
                        name: "atomic_cmp_store",
                        count: 1
                    },
                    AtomicOp {
                        opcode: 180,
                        name: "imm_atomic_iadd",
                        count: 3
                    }
                ]
            );
        }
    }

    #[test]
    fn custom_data_is_not_read_as_instructions() {
        // a comment block (class 0) of 4 words whose payload is two atomic opcode tokens
        let comment = [0x0000_0035, 4, 0x0100_00b4, 0x0100_00b5];
        let found = order_dependent(&container(b"SHEX", 0x5_0050, &program(1, &comment)))
            .unwrap()
            .unwrap();
        let counts: Vec<(u16, usize)> = found.ops.iter().map(|o| (o.opcode, o.count)).collect();
        assert_eq!(counts, [(172, 1), (180, 1)]);
    }

    #[test]
    fn rejects_bad_instruction_framing_even_after_a_match() {
        for tail in [
            vec![0],
            vec![180 | 2 << 24],
            vec![53],
            vec![53, 1],
            vec![53, u32::MAX],
            vec![0x8100_0036],
            vec![0x8200_0036, 0x8000_0000],
            vec![0x8000_0091, 1],
            // imm_atomic_iadd without its operands
            vec![180 | 1 << 24],
        ] {
            let mut words = DECLARATIONS.to_vec();
            words.extend(IADD);
            words.extend(&tail);
            assert!(
                order_dependent(&container(b"SHEX", 0x5_0050, &words)).is_err(),
                "{tail:x?}"
            );
        }
    }

    #[test]
    fn rejects_bad_containers_without_panicking() {
        let valid = container(b"SHEX", 0x5_0050, &program(1, &[]));
        assert!(order_dependent(&valid).unwrap().is_some());
        for end in 0..valid.len() {
            assert!(order_dependent(&valid[..end]).is_err(), "cut at {end}");
        }
        assert!(order_dependent(&container(b"DXIL", 0x5_0050, &[])).is_err());
    }

    #[test]
    fn the_code_key_follows_the_code_alone() {
        let program = include_bytes!("testdata/dxbc-stripped.cso");
        let key = code_key(program).expect("a container with code");
        // The container's checksum (bytes 4..20) is not code: changed, the key stays.
        let mut other = program.to_vec();
        other[4] ^= 0xff;
        assert_eq!(code_key(&other).as_deref(), Some(key.as_str()));
        // The last byte is in the code part (SHEX ends the container fxc writes when stripped).
        let mut changed = program.to_vec();
        *changed.last_mut().unwrap() ^= 1;
        let changed = code_key(&changed).expect("still a container");
        assert_ne!(changed, key);
        assert_eq!(code_key(b"not a program"), None);
    }
}
