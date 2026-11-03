//! Instruction-level detection of potentially order-dependent SM4/5 atomics.

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

// D3D11 tokenized program format: D3D10_SB_OPCODE_TYPE, opcode/length masks,
// Custom-Data Block Format and interface declaration extended lengths.
// Microsoft's maintained, backwards-compatible version of d3d11TokenizedProgramFormat.hpp:
// https://github.com/microsoft/DirectX-Headers/blob/main/include/directx/D3D12TokenizedProgramFormat.hpp
// CUSTOMDATA is 53 (0x35); decimal 35 is IMAD.
const ATOMICS: [(u16, &str); 13] = [
    (172, "atomic_cmp_store"),
    (178, "imm_atomic_alloc"),
    (179, "imm_atomic_consume"),
    (180, "imm_atomic_iadd"),
    (181, "imm_atomic_and"),
    (182, "imm_atomic_or"),
    (183, "imm_atomic_xor"),
    (184, "imm_atomic_exch"),
    (185, "imm_atomic_cmp_exch"),
    (186, "imm_atomic_imax"),
    (187, "imm_atomic_imin"),
    (188, "imm_atomic_umax"),
    (189, "imm_atomic_umin"),
];

/// Find atomics whose returned old value or conditional store can depend on
/// thread scheduling. This is a conservative opcode scan, not a data-flow proof:
/// dead code, unused return values and uncontended addresses still count.
/// Non-returning integer and/or/xor/add/min/max atomics are not flagged.
/// Racing plain stores to one address cannot be detected by this scan; `None`
/// is therefore not a guarantee of deterministic output.
///
/// Reads SHDR/SHEX through the DXBC chunk table. Invalid container bounds,
/// missing/duplicate shader chunks, unsupported shader models and malformed
/// instruction framing return an error. Operands and the checksum are not
/// validated; this is not a replacement for the D3D bytecode validator.
pub fn order_dependent(bytecode: &[u8]) -> Result<Option<OrderDependence>, String> {
    if bytecode.get(..4) != Some(b"DXBC") {
        return Err("not a DXBC container".into());
    }
    if word(bytecode, 20)? != 1 || word(bytecode, 24)? as usize != bytecode.len() {
        return Err("invalid DXBC version or container length".into());
    }
    let chunks = word(bytecode, 28)? as usize;
    if chunks > bytecode.len().saturating_sub(32) / 4 {
        return Err("truncated DXBC chunk table".into());
    }
    let table_end = 32 + chunks * 4;
    let mut shader = None;
    for i in 0..chunks {
        let offset = word(bytecode, 32 + i * 4)? as usize;
        if offset < table_end || offset > bytecode.len().saturating_sub(8) {
            return Err(format!("invalid DXBC chunk {i} offset"));
        }
        let size = word(bytecode, offset + 4)? as usize;
        let start = offset + 8;
        if size > bytecode.len() - start {
            return Err(format!("truncated DXBC chunk {i}"));
        }
        if matches!(&bytecode[offset..offset + 4], b"SHDR" | b"SHEX")
            && shader.replace(&bytecode[start..start + size]).is_some()
        {
            return Err("multiple SHDR/SHEX chunks".into());
        }
    }
    let shader = shader.ok_or("no SHDR/SHEX chunk")?;
    let version = word(shader, 0)?;
    if !matches!((version >> 4) & 15, 4 | 5) {
        return Err(format!("unsupported shader version {version:#x}"));
    }
    let words = word(shader, 4)? as usize;
    if shader.len() % 4 != 0 || words != shader.len() / 4 || words < 2 {
        return Err("invalid shader token length".into());
    }
    let mut counts = [0usize; ATOMICS.len()];
    let mut at = 2;
    while at < words {
        let token = word(shader, at * 4)?;
        let opcode = (token & 0x7ff) as u16;
        let extended = token & 0x8000_0000 != 0;
        // These interface/declaration tokens use an explicit extended length,
        // unlike ordinary opcode extensions, which are included in bits 24..30.
        let explicit_length = opcode == 53 || (extended && matches!(opcode, 120 | 144..=146 | 155));
        let length = if explicit_length {
            word(shader, (at + 1) * 4)? as usize
        } else {
            ((token >> 24) & 0x7f) as usize
        };
        if length < if explicit_length { 2 } else { 1 } || length > words - at {
            return Err(format!("invalid instruction length {length} at token {at}"));
        }
        if extended && !explicit_length {
            let mut next = at + 1;
            let mut more = true;
            while more {
                if next >= at + length {
                    return Err(format!("truncated opcode extension at token {at}"));
                }
                more = word(shader, next * 4)? & 0x8000_0000 != 0;
                next += 1;
            }
        }
        if let Some(index) = ATOMICS.iter().position(|&(op, _)| op == opcode) {
            counts[index] += 1;
        }
        at += length;
    }
    let ops: Vec<_> = ATOMICS
        .iter()
        .zip(counts)
        .filter_map(|(&(opcode, name), count)| (count != 0).then_some(AtomicOp { opcode, name, count }))
        .collect();
    Ok((!ops.is_empty()).then_some(OrderDependence { ops }))
}

fn word(bytes: &[u8], at: usize) -> Result<u32, String> {
    let end = at.checked_add(4).ok_or("DXBC offset overflow")?;
    let value = bytes
        .get(at..end)
        .ok_or_else(|| format!("truncated DXBC word at byte {at}"))?;
    Ok(u32::from_le_bytes(value.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(tag: &[u8; 4], instructions: &[u32]) -> Vec<u8> {
        // An unrelated chunk precedes the shader; the table is deliberately reversed.
        let mut bytes = b"DXBC".to_vec();
        bytes.extend([0; 16]);
        for word in [1, 64 + instructions.len() as u32 * 4, 2, 48, 40] {
            bytes.extend(word.to_le_bytes());
        }
        bytes.extend(b"TEST\0\0\0\0");
        bytes.extend(tag);
        bytes.extend((8 + instructions.len() as u32 * 4).to_le_bytes());
        bytes.extend(0x0005_0050u32.to_le_bytes());
        bytes.extend((2 + instructions.len() as u32).to_le_bytes());
        for word in instructions {
            bytes.extend(word.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn accepts_sm4_and_empty_programs() {
        for version in [0x40u32, 0x41, 0x50, 0x51] {
            let mut bytes = container(b"SHDR", &[62 | 1 << 24]); // RET
            bytes[56..60].copy_from_slice(&version.to_le_bytes());
            assert_eq!(order_dependent(&bytes), Ok(None));
        }
        assert_eq!(order_dependent(&container(b"SHEX", &[])), Ok(None));
    }

    #[test]
    fn reports_all_atomic_names_and_counts() {
        let instructions: Vec<_> = (172..=189).chain([180, 180]).map(|op| op | 1 << 24).collect();
        for tag in [b"SHDR", b"SHEX"] {
            let found = order_dependent(&container(tag, &instructions)).unwrap().unwrap();
            assert_eq!(found.ops.len(), 13);
            for (actual, &(opcode, name)) in found.ops.iter().zip(&ATOMICS) {
                assert_eq!((actual.opcode, actual.name), (opcode, name));
                assert_eq!(actual.count, if opcode == 180 { 3 } else { 1 });
            }
        }
    }

    #[test]
    fn ignores_commutative_atomics_stores_and_payloads() {
        let mut instructions: Vec<_> = [164, 166, 168, 169, 170, 171, 173, 174, 175, 176, 177]
            .map(|op| op | 1 << 24)
            .into();
        instructions.extend([53 | 0xffff_f800, 4, 180 | 1 << 24, 172 | 1 << 24]);
        instructions.extend([35 | 2 << 24, 180 | 1 << 24]); // IMAD, not customdata
        instructions.extend([0x8400_0036, 0x8000_00b4, 172, 180]); // MOV + extensions + operand
        assert_eq!(order_dependent(&container(b"SHEX", &instructions)), Ok(None));
    }

    #[test]
    fn skips_extended_declaration_length() {
        let mut instructions = vec![145 | 0x8000_0000, 130];
        instructions.resize(130, 180 | 1 << 24);
        instructions.push(178 | 1 << 24);
        let found = order_dependent(&container(b"SHEX", &instructions))
            .unwrap()
            .unwrap();
        assert_eq!(
            found.ops,
            vec![AtomicOp {
                opcode: 178,
                name: "imm_atomic_alloc",
                count: 1
            }]
        );
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
        ] {
            let mut instructions = vec![180 | 1 << 24];
            instructions.extend(tail);
            assert!(order_dependent(&container(b"SHEX", &instructions)).is_err());
        }
    }

    #[test]
    fn rejects_bad_containers_without_panicking() {
        let valid = container(b"SHEX", &[180 | 1 << 24]);
        for end in 0..valid.len() {
            assert!(order_dependent(&valid[..end]).is_err());
        }
        for (offset, value) in [
            (20, 2),
            (24, 1),
            (28, u32::MAX),
            (32, 0),
            (32, u32::MAX),
            (36, 48),
            (52, u32::MAX),
            (56, 0x60),
            (60, 1),
        ] {
            let mut broken = valid.clone();
            broken[offset..offset + 4].copy_from_slice(&u32::to_le_bytes(value));
            assert!(
                order_dependent(&broken).is_err(),
                "offset {offset}, value {value}"
            );
        }
        assert!(order_dependent(&container(b"DXIL", &[])).is_err());
    }
}
