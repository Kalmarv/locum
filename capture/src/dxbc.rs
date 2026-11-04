//! Instruction-level detection of potentially order-dependent SM4/5 atomics, over dx's `dxbc-reader` decoding.

/// One kind of atomic instruction, counted statically (not by executions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtomicOp {
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
}
