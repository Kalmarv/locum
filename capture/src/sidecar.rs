//! A sidecar: what a tool beside the application (a translation layer's diagnostic, say) tells locum about the
//! application's memory outside the graphics API, read when a capture is written. locum decodes none of it: it copies
//! the regions named, keeps the rest as rows, and the CLI checks each descriptor against the frame's resources.
//!
//! The file, a line each; numbers in hex (`0x` optional), `#` starts a comment:
//!
//! ```text
//! region <address> <bytes> <name>                     memory to copy into the capture as it is
//! visible <address> <bytes> <gpu address>             application memory the GPU sees, from that GPU address on
//! descriptor <address> <words> <target> <bytes> [label]
//!                                                     a descriptor the application wrote at <address> (its bytes,
//!                                                     as hex), which the tool resolved to <bytes> at <target>
//! ```
//!
//! Tables `sidecar_regions`, `sidecar_visible` and `sidecar_descriptors`.

use rusqlite::{Transaction, params};

#[derive(Clone, Debug, PartialEq)]
pub struct Region {
    pub address: u64,
    pub bytes: u64,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Visible {
    pub address: u64,
    pub bytes: u64,
    pub gpu: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Descriptor {
    pub address: u64,
    pub words: Vec<u8>,
    pub target: u64,
    pub bytes: u64,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Sidecar {
    pub regions: Vec<Region>,
    pub visible: Vec<Visible>,
    pub descriptors: Vec<Descriptor>,
}

/// A region as the capture copied it: the key of its bytes (a pieced blob for a big one), or why there are none, and
/// what else to know (pages that could not be read, read as zeros).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Copied {
    pub content: Option<String>,
    pub note: Option<String>,
}

/// A sidecar and its regions' copies, in the order of `sidecar.regions` (the file it was read from is
/// `meta.sidecar`).
#[derive(Clone, Debug, Default)]
pub struct Captured {
    pub sidecar: Sidecar,
    pub copied: Vec<Copied>,
}

fn number(s: &str) -> Option<u64> {
    u64::from_str_radix(s.strip_prefix("0x").unwrap_or(s), 16).ok()
}

fn hex_bytes(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.is_empty() || !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

/// The sidecar `text` says, and a warning for each line it could not read (skipped).
pub fn parse(text: &str) -> (Sidecar, Vec<String>) {
    let mut s = Sidecar::default();
    let mut warnings = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let words: Vec<&str> = line.split_whitespace().collect();
        let rest = |from: usize| words.get(from..).map(|w| w.join(" ")).unwrap_or_default();
        let read = match words[0] {
            "region" if words.len() >= 4 => (|| {
                s.regions.push(Region {
                    address: number(words[1])?,
                    bytes: number(words[2])?,
                    name: rest(3),
                });
                Some(())
            })(),
            "visible" if words.len() == 4 => (|| {
                s.visible.push(Visible {
                    address: number(words[1])?,
                    bytes: number(words[2])?,
                    gpu: number(words[3])?,
                });
                Some(())
            })(),
            "descriptor" if words.len() >= 5 => (|| {
                s.descriptors.push(Descriptor {
                    address: number(words[1])?,
                    words: hex_bytes(words[2])?,
                    target: number(words[3])?,
                    bytes: number(words[4])?,
                    label: rest(5),
                });
                Some(())
            })(),
            _ => None,
        };
        if read.is_none() {
            warnings.push(format!("sidecar line {}: {line:?} not read", n + 1));
        }
    }
    (s, warnings)
}

fn integer(value: u64) -> rusqlite::Result<i64> {
    i64::try_from(value).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
}

/// Makes the sidecar tables and writes `c` into them.
pub fn write(tx: &Transaction<'_>, c: &Captured) -> rusqlite::Result<()> {
    tx.execute_batch(
        "CREATE TABLE sidecar_regions(address INTEGER NOT NULL,bytes INTEGER NOT NULL,name TEXT NOT NULL,content TEXT,
          note TEXT);
         CREATE TABLE sidecar_visible(address INTEGER NOT NULL,bytes INTEGER NOT NULL,gpu INTEGER NOT NULL);
         CREATE TABLE sidecar_descriptors(address INTEGER NOT NULL,words BLOB NOT NULL,target INTEGER NOT NULL,
          bytes INTEGER NOT NULL,label TEXT NOT NULL);",
    )?;
    let mut s = tx.prepare("INSERT INTO sidecar_regions VALUES(?1,?2,?3,?4,?5)")?;
    for (i, r) in c.sidecar.regions.iter().enumerate() {
        let copied = c.copied.get(i).cloned().unwrap_or_default();
        s.execute(params![
            integer(r.address)?,
            integer(r.bytes)?,
            r.name,
            copied.content,
            copied.note
        ])?;
    }
    let mut s = tx.prepare("INSERT INTO sidecar_visible VALUES(?1,?2,?3)")?;
    for v in &c.sidecar.visible {
        s.execute(params![integer(v.address)?, integer(v.bytes)?, integer(v.gpu)?])?;
    }
    let mut s = tx.prepare("INSERT INTO sidecar_descriptors VALUES(?1,?2,?3,?4,?5)")?;
    for d in &c.sidecar.descriptors {
        s.execute(params![
            integer(d.address)?,
            d.words,
            integer(d.target)?,
            integer(d.bytes)?,
            d.label
        ])?;
    }
    Ok(())
}

/// A buffer of the frame: its resource, GPU address and bytes, and the key of its contents as the capture has them.
#[derive(Clone, Debug)]
pub struct Buffer {
    pub resource: u64,
    pub gpu: u64,
    pub bytes: u64,
    pub content: Option<String>,
}

/// Where a descriptor is and what is wrong with it.
#[derive(Clone, Debug, PartialEq)]
pub struct Checked {
    /// Its GPU address, through the `visible` range holding it.
    pub gpu: Option<u64>,
    /// The buffer holding it there, and the offset.
    pub resource: Option<u64>,
    pub offset: Option<u64>,
    /// What the capture's bytes there say: `matched` (they are its words), `differs`, `not-visible` (no `visible`
    /// range holds its address), `no-buffer` (no buffer holds its GPU address) or `no-contents`.
    pub found: &'static str,
    /// `empty` (it resolves to no bytes, or to address 0) or `outside` (to bytes no `visible` range holds).
    pub flag: Option<&'static str>,
}

/// The flags [`Checked::flag`] can hold, and what they mean.
pub const FLAGS: [(&str, &str); 2] = [
    ("empty", "it resolves to no bytes, or to address 0"),
    (
        "outside",
        "it resolves to memory the GPU does not see (in no visible range)",
    ),
];

/// Reads `n` bytes of the contents of key `.0` from offset `.1`: None when they are not in the store.
pub type Read<'a> = dyn FnMut(&str, u64, u64) -> Option<Vec<u8>> + 'a;

/// Checks descriptor `d` against the `visible` ranges and the frame's `buffers`, reading `len` bytes of contents `key`
/// at an offset through `read`.
pub fn check(d: &Descriptor, visible: &[Visible], buffers: &[Buffer], read: &mut Read<'_>) -> Checked {
    let holds = |v: &Visible, at: u64, n: u64| {
        at >= v.address && at.saturating_add(n) <= v.address.saturating_add(v.bytes)
    };
    let flag = if d.bytes == 0 || d.target == 0 {
        Some("empty")
    } else if !visible.iter().any(|v| holds(v, d.target, d.bytes)) {
        Some("outside")
    } else {
        None
    };
    let n = d.words.len() as u64;
    let mut c = Checked {
        gpu: None,
        resource: None,
        offset: None,
        found: "not-visible",
        flag,
    };
    let Some(v) = visible.iter().find(|v| holds(v, d.address, n.max(1))) else {
        return c;
    };
    let gpu = v.gpu + (d.address - v.address);
    c.gpu = Some(gpu);
    // The smallest buffer holding it: a buffer placed inside a bigger one names its bytes more closely.
    let Some(b) = buffers
        .iter()
        .filter(|b| gpu >= b.gpu && gpu.saturating_add(n) <= b.gpu.saturating_add(b.bytes))
        .min_by_key(|b| b.bytes)
    else {
        c.found = "no-buffer";
        return c;
    };
    c.resource = Some(b.resource);
    c.offset = Some(gpu - b.gpu);
    c.found = match b.content.as_deref().and_then(|k| read(k, gpu - b.gpu, n)) {
        None => "no-contents",
        Some(bytes) if bytes == d.words => "matched",
        Some(_) => "differs",
    };
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_lines_it_knows_and_warns_of_the_rest() {
        let (s, warnings) = parse(
            "# made by a tool\n\
             region 0x7ffff0000 10000 the arena's CPU view\n\
             visible 100000000 100000000 0x2000000000\n\
             descriptor 0x100000040 00000000ffff0000 0 0 buffer view\n\
             descriptor 100000050 0a0b 7ffff0000 400\n\
             visible 1 2\n\
             descriptor 1 abc 0 0\n\
             nonsense 1 2 3\n",
        );
        assert_eq!(
            s.regions,
            [Region {
                address: 0x7_ffff_0000,
                bytes: 0x10000,
                name: "the arena's CPU view".into()
            }]
        );
        assert_eq!(
            s.visible,
            [Visible {
                address: 0x1_0000_0000,
                bytes: 0x1_0000_0000,
                gpu: 0x20_0000_0000
            }]
        );
        assert_eq!(s.descriptors.len(), 2);
        assert_eq!(s.descriptors[0].words, [0, 0, 0, 0, 0xff, 0xff, 0, 0]);
        assert_eq!(s.descriptors[0].label, "buffer view");
        assert_eq!(
            (
                s.descriptors[1].target,
                s.descriptors[1].bytes,
                s.descriptors[1].label.as_str()
            ),
            (0x7_ffff_0000, 0x400, "")
        );
        // A visible line short of a field, a descriptor of an odd number of digits, an unknown kind.
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].starts_with("sidecar line 6:"));
    }

    #[test]
    fn descriptors_are_found_in_the_frame_and_flagged() {
        let visible = [Visible {
            address: 0x1_0000_0000,
            bytes: 0x1000,
            gpu: 0x50_0000,
        }];
        let buffers = [
            Buffer {
                resource: 7,
                gpu: 0x50_0000,
                bytes: 0x1000,
                content: Some("k7".into()),
            },
            // A smaller buffer placed inside #7, without contents.
            Buffer {
                resource: 8,
                gpu: 0x50_0800,
                bytes: 0x100,
                content: None,
            },
        ];
        let memory: Vec<u8> = (0..0x1000u32).map(|i| i as u8).collect();
        let mut read = |key: &str, offset: u64, n: u64| {
            (key == "k7").then(|| memory[offset as usize..(offset + n) as usize].to_vec())
        };
        let d = |address: u64, words: Vec<u8>, target: u64, bytes: u64| Descriptor {
            address,
            words,
            target,
            bytes,
            label: String::new(),
        };
        // Its bytes as captured; resolving to visible memory.
        let c = check(
            &d(0x1_0000_0010, vec![0x10, 0x11], 0x1_0000_0100, 4),
            &visible,
            &buffers,
            &mut read,
        );
        assert_eq!(
            c,
            Checked {
                gpu: Some(0x50_0010),
                resource: Some(7),
                offset: Some(0x10),
                found: "matched",
                flag: None
            }
        );
        // Other bytes; resolving to no bytes.
        let c = check(
            &d(0x1_0000_0010, vec![0, 0], 0x1_0000_0100, 0),
            &visible,
            &buffers,
            &mut read,
        );
        assert_eq!((c.found, c.flag), ("differs", Some("empty")));
        // In the buffer placed inside, which has no contents; resolving outside what the GPU sees.
        let c = check(
            &d(0x1_0000_0810, vec![0; 4], 0x7_ffff_0000, 16),
            &visible,
            &buffers,
            &mut read,
        );
        assert_eq!(
            (c.resource, c.offset, c.found, c.flag),
            (Some(8), Some(0x10), "no-contents", Some("outside"))
        );
        // Its address in no visible range, or a visible one no buffer holds.
        let c = check(
            &d(0x2_0000_0000, vec![0; 4], 0, 16),
            &visible,
            &buffers,
            &mut read,
        );
        assert_eq!((c.gpu, c.found, c.flag), (None, "not-visible", Some("empty")));
        let c = check(
            &d(0x1_0000_0ffe, vec![0; 4], 0x1_0000_0000, 16),
            &visible,
            &buffers,
            &mut read,
        );
        assert_eq!(c.found, "not-visible", "it runs past the visible range");
        let none: [Buffer; 0] = [];
        let c = check(
            &d(0x1_0000_0010, vec![0; 4], 0x1_0000_0000, 16),
            &visible,
            &none,
            &mut read,
        );
        assert_eq!((c.gpu, c.found), (Some(0x50_0010), "no-buffer"));
    }

    #[test]
    fn written_and_read_back() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        let (sidecar, _) = parse("region 10 20 r\nvisible 0 100 1000\ndescriptor 40 0102 0 0 v\n");
        let tx = db.transaction().unwrap();
        write(
            &tx,
            &Captured {
                sidecar,
                copied: vec![Copied {
                    content: Some("k".into()),
                    note: None,
                }],
            },
        )
        .unwrap();
        tx.commit().unwrap();
        let (address, content): (i64, String) = db
            .query_row("SELECT address, content FROM sidecar_regions", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((address, content.as_str()), (0x10, "k"));
        let words: Vec<u8> = db
            .query_row("SELECT words FROM sidecar_descriptors", [], |r| r.get(0))
            .unwrap();
        assert_eq!(words, [1, 2]);
        let gpu: i64 = db
            .query_row("SELECT gpu FROM sidecar_visible", [], |r| r.get(0))
            .unwrap();
        assert_eq!(gpu, 0x1000);
    }
}
