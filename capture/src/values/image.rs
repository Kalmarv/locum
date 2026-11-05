//! An executable's code, where a table's symbols are found: its sections that execute (every one, duplicate names
//! included: protected executables carry two `.text`), read from the PE headers whether the image is mapped (the
//! running game, its sections at their addresses) or a file on disk (its sections laid out at their offsets in the
//! image, as the loader would). A packed executable's code can differ on disk from what runs.

use super::table::{Find, Pattern};

/// A section header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    /// Offset in the image once loaded.
    pub rva: u32,
    /// Length in the image once loaded.
    pub size: u32,
    /// Where its data starts in the file, and how much of it the file holds.
    pub file_at: u32,
    pub file_size: u32,
    pub characteristics: u32,
}

/// `IMAGE_SCN_MEM_EXECUTE`
const EXECUTE: u32 = 0x2000_0000;

impl Section {
    pub fn executes(&self) -> bool {
        self.characteristics & EXECUTE != 0
    }
}

fn u16_at(b: &[u8], at: usize) -> Result<u16, String> {
    b.get(at..at + 2)
        .map(|s| u16::from_le_bytes([s[0], s[1]]))
        .ok_or_else(|| "the PE headers are cut short".into())
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| "the PE headers are cut short".into())
}

/// Where the PE header of the 64-bit image whose headers start `headers` is.
fn nt_header(headers: &[u8]) -> Result<usize, String> {
    if headers.get(..2) != Some(b"MZ") {
        return Err("no DOS header (MZ)".into());
    }
    let nt = u32_at(headers, 0x3c)? as usize;
    if headers.get(nt..nt + 4) != Some(b"PE\0\0") {
        return Err("no PE header".into());
    }
    if u16_at(headers, nt + 24)? != 0x20b {
        return Err("not a 64-bit (PE32+) image".into());
    }
    Ok(nt)
}

/// The image's length once loaded (`SizeOfImage`).
pub fn size(headers: &[u8]) -> Result<u32, String> {
    u32_at(headers, nt_header(headers)? + 24 + 56)
}

/// The section table of the 64-bit PE image whose headers start `headers`.
pub fn sections(headers: &[u8]) -> Result<Vec<Section>, String> {
    let nt = nt_header(headers)?;
    let count = u16_at(headers, nt + 6)? as usize;
    let optional = u16_at(headers, nt + 20)? as usize;
    let first = nt + 24 + optional;
    (0..count)
        .map(|i| {
            let s = first + i * 40;
            let raw = headers
                .get(s..s + 8)
                .ok_or_else(|| "the PE headers are cut short".to_string())?;
            let name = String::from_utf8_lossy(raw.split(|&b| b == 0).next().unwrap_or(raw)).into_owned();
            Ok(Section {
                name,
                size: u32_at(headers, s + 8)?,
                rva: u32_at(headers, s + 12)?,
                file_size: u32_at(headers, s + 16)?,
                file_at: u32_at(headers, s + 20)?,
                characteristics: u32_at(headers, s + 36)?,
            })
        })
        .collect()
}

/// Code to search: each executable section's address and bytes.
pub type Code<'a> = [(u64, &'a [u8])];

/// A PE file's sections: every header, and the bytes of those that execute.
pub struct OnDisk {
    pub sections: Vec<Section>,
    /// Each executable section laid out at its offset in the image (RVA, bytes), zero past what the file holds.
    pub code: Vec<(u64, Vec<u8>)>,
}

impl OnDisk {
    /// The code to search, its addresses offsets in the image.
    pub fn code(&self) -> Vec<(u64, &[u8])> {
        self.code.iter().map(|(rva, b)| (*rva, b.as_slice())).collect()
    }
}

/// The sections of a PE file's bytes.
pub fn on_disk(file: &[u8]) -> Result<OnDisk, String> {
    let all = sections(file)?;
    let code = all
        .iter()
        .filter(|s| s.executes())
        .map(|s| {
            let size = if s.size == 0 { s.file_size } else { s.size } as usize;
            let mut bytes = vec![0u8; size];
            let from = (s.file_at as usize).min(file.len());
            let n = (s.file_size as usize).min(size).min(file.len() - from);
            bytes[..n].copy_from_slice(&file[from..from + n]);
            (s.rva as u64, bytes)
        })
        .collect();
    Ok(OnDisk { sections: all, code })
}

/// Every match of `pattern` in `bytes`, as offsets.
pub fn matches(bytes: &[u8], pattern: &Pattern) -> Vec<usize> {
    if pattern.len() > bytes.len() {
        return Vec::new();
    }
    // anchor on the first fixed byte so the scan is a memchr-like skip
    let (anchor, first) = pattern
        .iter()
        .enumerate()
        .find_map(|(i, b)| b.map(|b| (i, b)))
        .expect("a fixed byte");
    let mut out = Vec::new();
    let last = bytes.len() - pattern.len();
    let mut i = 0;
    while i <= last {
        match bytes[i + anchor..=last + anchor].iter().position(|&b| b == first) {
            None => break,
            Some(skip) => i += skip,
        }
        if pattern
            .iter()
            .zip(&bytes[i..])
            .all(|(p, b)| p.is_none_or(|p| p == *b))
        {
            out.push(i);
        }
        i += 1;
    }
    out
}

/// The one match of `pattern` in the code, as its address and the bytes from there to the end of its section.
fn find_unique<'a>(code: &Code<'a>, pattern: &Pattern) -> Result<(u64, &'a [u8]), String> {
    let found: Vec<(u64, &[u8])> = code
        .iter()
        .flat_map(|&(start, bytes)| {
            matches(bytes, pattern)
                .into_iter()
                .map(move |o| (start + o as u64, &bytes[o..]))
        })
        .collect();
    match found[..] {
        [one] => Ok(one),
        [] => Err("the pattern matches nowhere in the executable's code".into()),
        _ => Err(format!("the pattern matches {} places, not one", found.len())),
    }
}

/// A symbol's address in the code.
pub fn resolve(code: &Code, find: &Find) -> Result<u64, String> {
    match find {
        Find::Code { pattern, offset } => Ok(find_unique(code, pattern)?.0.wrapping_add(*offset as u64)),
        Find::Global { pattern, at, end } => {
            // the table's parser keeps the operand inside the pattern, so inside the match
            let (site, bytes) = find_unique(code, pattern)?;
            let disp = i32::from_le_bytes(bytes[*at..at + 4].try_into().expect("4 bytes"));
            Ok((site + *end as u64).wrapping_add(disp as i64 as u64))
        }
    }
}

/// The SHA-256 of an executable's bytes, as `table.txt`'s `exe sha256` writes it.
pub fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::table::{parse, pattern};

    #[test]
    fn patterns_match_with_wildcards() {
        let bytes = [0x00, 0x48, 0x8b, 0x05, 0x11, 0x22, 0x48, 0x8b, 0x07, 0x48];
        assert_eq!(matches(&bytes, &pattern("48 8b ??").unwrap()), vec![1, 6]);
        assert_eq!(matches(&bytes, &pattern("?? 8b 05").unwrap()), vec![1]);
        assert_eq!(matches(&bytes, &pattern("48 8b 07 48").unwrap()), vec![6]);
        assert!(matches(&bytes, &pattern("48 8b 07 48 00").unwrap()).is_empty());
    }

    /// A PE32+ file of two sections, the second executable, its code given.
    fn pe(code: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 0x200];
        f[..2].copy_from_slice(b"MZ");
        f[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        f[0x40..0x44].copy_from_slice(b"PE\0\0");
        f[0x46..0x48].copy_from_slice(&2u16.to_le_bytes());
        f[0x54..0x56].copy_from_slice(&0xf0u16.to_le_bytes());
        f[0x58..0x5a].copy_from_slice(&0x20bu16.to_le_bytes());
        f[0x58 + 56..0x58 + 60].copy_from_slice(&0x3000u32.to_le_bytes());
        let mut section = |i: usize, name: &[u8], rva: u32, size: u32, file_at: u32, flags: u32| {
            let s = 0x58 + 0xf0 + i * 40;
            f[s..s + name.len()].copy_from_slice(name);
            f[s + 8..s + 12].copy_from_slice(&size.to_le_bytes());
            f[s + 12..s + 16].copy_from_slice(&rva.to_le_bytes());
            f[s + 16..s + 20].copy_from_slice(&size.to_le_bytes());
            f[s + 20..s + 24].copy_from_slice(&file_at.to_le_bytes());
            f[s + 36..s + 40].copy_from_slice(&flags.to_le_bytes());
        };
        section(0, b".data", 0x1000, 0x10, 0x200, 0xc000_0040);
        section(1, b".text", 0x2000, code.len() as u32, 0x210, 0x6000_0020);
        f.extend_from_slice(&[0u8; 0x10]);
        f.extend_from_slice(code);
        f
    }

    #[test]
    fn symbols_are_found_in_a_file() {
        // a lea rax, [rip+0x...] at rva 0x2004 whose target is 0x1008, in .data
        let disp = (0x1008i64 - (0x2004 + 7)) as i32;
        let mut code = vec![0xcc, 0xcc, 0xcc, 0xcc, 0x48, 0x8d, 0x05];
        code.extend_from_slice(&disp.to_le_bytes());
        code.extend_from_slice(&[0xc3, 0xcc]);
        let file = pe(&code);
        let disk = on_disk(&file).unwrap();
        assert_eq!(size(&file), Ok(0x3000));
        assert_eq!(disk.sections.len(), 2);
        assert_eq!(disk.sections[0].name, ".data");
        assert_eq!(disk.code.len(), 1);
        let code = disk.code();
        let t = parse("global G = 48 8D 05 ?? ?? ?? ?? C3 @ 3 7\ncode F = 48 8D 05 -4\ncode N = CC").unwrap();
        assert_eq!(resolve(&code, &t.symbols[0].1), Ok(0x1008));
        assert_eq!(resolve(&code, &t.symbols[1].1), Ok(0x2000));
        assert!(
            resolve(&code, &t.symbols[2].1)
                .unwrap_err()
                .contains("places, not one")
        );
        assert!(sections(&file[..0x100]).unwrap_err().contains("cut short"));
    }
}
