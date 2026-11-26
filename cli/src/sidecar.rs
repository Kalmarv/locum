//! `sidecar`: what a tool beside the application said of its memory as the capture was written (see
//! `locum_capture::sidecar`): the regions it had copied, the ranges of memory the GPU sees, and each descriptor the
//! application wrote, found in the frame's buffers and checked against the capture's bytes there, flagged when it
//! resolves to no bytes or to memory the GPU does not see. `--flagged` lists only those.

use std::collections::BTreeMap;

use locum_capture::sidecar::{Buffer, Descriptor, Visible, check};
use serde_json::json;

use crate::open::Capture;
use crate::out;

fn hex(v: u64) -> String {
    format!("{v:#x}")
}

/// The frame's buffers with GPU addresses (D3D12), and their contents as the frame began.
fn buffers(c: &Capture) -> Vec<Buffer> {
    if !c.has_table("d12_objects") {
        return Vec::new();
    }
    let contents: BTreeMap<i64, String> = c
        .rows(
            "SELECT id, content FROM resources WHERE kind = 'buffer' AND content IS NOT NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .into_iter()
        .collect();
    c.rows(
        "SELECT id, description FROM d12_objects WHERE kind = 'ID3D12Resource'",
        [],
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
    )
    .into_iter()
    .filter_map(|(id, d)| {
        let d: serde_json::Value = serde_json::from_str(&d).ok()?;
        (d["dimension"].as_u64() == Some(1)).then_some(())?;
        Some(Buffer {
            resource: id as u64,
            gpu: d["gpu_address"].as_u64().filter(|&a| a != 0)?,
            bytes: d["width"].as_u64()?,
            content: contents.get(&id).cloned(),
        })
    })
    .collect()
}

/// A `sidecar_regions` row: address, bytes, name, content and note.
type RegionRow = (i64, i64, String, Option<String>, Option<String>);

pub fn sidecar(c: &Capture, flagged: bool, json: bool) -> Result<(), String> {
    if !c.has_table("sidecar_descriptors") {
        return Err(
            "sidecar: the capture has none (a tool beside the application writes `sidecar.txt` in locum's folder, or \
             the file settings.txt's `sidecar` names)"
                .into(),
        );
    }
    let regions: Vec<RegionRow> = c.rows(
        "SELECT address, bytes, name, content, note FROM sidecar_regions ORDER BY address",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
    );
    let visible: Vec<Visible> = c.rows(
        "SELECT address, bytes, gpu FROM sidecar_visible ORDER BY address",
        [],
        |r| {
            Ok(Visible {
                address: r.get::<_, i64>(0)? as u64,
                bytes: r.get::<_, i64>(1)? as u64,
                gpu: r.get::<_, i64>(2)? as u64,
            })
        },
    );
    let descriptors: Vec<Descriptor> = c.rows(
        "SELECT address, words, target, bytes, label FROM sidecar_descriptors ORDER BY address",
        [],
        |r| {
            Ok(Descriptor {
                address: r.get::<_, i64>(0)? as u64,
                words: r.get(1)?,
                target: r.get::<_, i64>(2)? as u64,
                bytes: r.get::<_, i64>(3)? as u64,
                label: r.get(4)?,
            })
        },
    );
    let buffers = buffers(c);
    let mut read = |key: &str, offset: u64, n: u64| c.blobs.read(key, offset, Some(n)).ok();
    let checked: Vec<_> = descriptors
        .iter()
        .map(|d| check(d, &visible, &buffers, &mut read))
        .collect();
    let mut counts = BTreeMap::<&str, usize>::new();
    for k in &checked {
        *counts.entry(k.found).or_default() += 1;
        if let Some(f) = k.flag {
            *counts.entry(f).or_default() += 1;
        }
    }
    let says = |v: Vec<(&str, usize)>| {
        v.iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    if json {
        for (address, bytes, name, content, note) in &regions {
            out!(
                "{}",
                json!({"kind":"region","address":hex(*address as u64),"bytes":bytes,"name":name,"content":content,"note":note})
            );
        }
        for v in &visible {
            out!(
                "{}",
                json!({"kind":"visible","address":hex(v.address),"bytes":v.bytes,"gpu":hex(v.gpu)})
            );
        }
    } else {
        out!("{} from {}", "sidecar", c.meta("sidecar").unwrap_or_default());
        out!("regions: {}", regions.len());
        for (address, bytes, name, content, note) in &regions {
            out!(
                "  {} {} bytes {name}: {} ({})",
                hex(*address as u64),
                bytes,
                content.as_deref().unwrap_or("no contents"),
                note.as_deref().unwrap_or("")
            );
        }
        out!("visible to the GPU: {}", visible.len());
        for v in &visible {
            out!(
                "  {}..{} at GPU {}",
                hex(v.address),
                hex(v.address + v.bytes),
                hex(v.gpu)
            );
        }
        let flags: Vec<_> = locum_capture::sidecar::FLAGS
            .iter()
            .map(|(f, _)| (*f, counts.get(f).copied().unwrap_or(0)))
            .collect();
        let found: Vec<_> = ["matched", "differs", "no-contents", "no-buffer", "not-visible"]
            .iter()
            .map(|f| (*f, counts.get(f).copied().unwrap_or(0)))
            .filter(|(_, n)| *n > 0)
            .collect();
        out!(
            "descriptors: {} ({}; in the capture: {})",
            descriptors.len(),
            says(flags),
            says(found)
        );
        out!("address\twords\ttarget\tbytes\tflag\tgpu\tresource\toffset\tfound\tlabel");
    }
    for (d, k) in descriptors.iter().zip(&checked) {
        if flagged && k.flag.is_none() {
            continue;
        }
        let words: String = d.words.iter().map(|b| format!("{b:02x}")).collect();
        if json {
            out!(
                "{}",
                json!({"kind":"descriptor","address":hex(d.address),"words":words,"target":hex(d.target),
                    "bytes":d.bytes,"label":d.label,"flag":k.flag,"gpu":k.gpu.map(hex),"resource":k.resource,
                    "offset":k.offset,"found":k.found})
            );
        } else {
            out!(
                "{}\t{words}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                hex(d.address),
                hex(d.target),
                d.bytes,
                k.flag.unwrap_or("-"),
                k.gpu.map_or("-".into(), hex),
                k.resource.map_or("-".into(), |r| format!("#{r}")),
                k.offset.map_or("-".into(), hex),
                k.found,
                d.label
            );
        }
    }
    Ok(())
}
