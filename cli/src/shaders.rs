//! `shaders` and `shader`: the shaders the frame used, their constant buffers and disassembly.

use std::collections::HashMap;

use locum_capture::rdef;
use serde::Serialize;
use windows::Win32::Graphics::Direct3D::Fxc::D3DDisassemble;

use crate::open::Capture;
use crate::out;

pub fn shaders(c: &Capture) -> Result<(), String> {
    let mut uses: HashMap<String, i64> = HashMap::new();
    for stage in ["vs", "hs", "ds", "gs", "ps", "cs"] {
        let counts: Vec<(String, i64)> = c.rows(
            &format!("SELECT {stage}, count(*) FROM draws WHERE {stage} IS NOT NULL GROUP BY {stage}"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        );
        for (h, n) in counts {
            *uses.entry(h).or_default() += n;
        }
    }
    let rows: Vec<(String, String, Option<i64>)> = c.rows(
        "SELECT hash, stage, size FROM shaders ORDER BY stage, hash",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    );
    out!("{:<5} {:<16} {:>7} {:>6}", "stage", "hash", "bytes", "draws");
    for (h, stage, size) in rows {
        out!(
            "{stage:<5} {h:<16} {:>7} {:>6}",
            size.map_or("-".into(), |s| s.to_string()),
            uses.get(&h).copied().unwrap_or(0)
        );
    }
    Ok(())
}

/// A shader's reflection, as `shader --json` gives it.
#[derive(Serialize)]
struct Shader {
    hash: String,
    stage: String,
    bytes: usize,
    blob: String,
    /// Every resource it binds, constant buffers included; null without reflection.
    bindings: Option<Vec<Binding>>,
    constant_buffers: Option<Vec<Buffer>>,
}

#[derive(Serialize)]
struct Binding {
    name: String,
    /// `D3D_SHADER_INPUT_TYPE`, and the register class it takes (cb, srv, sampler, uav).
    kind: u32,
    kind_name: &'static str,
    class: &'static str,
    slot: u32,
    count: u32,
    space: u32,
}

#[derive(Serialize)]
struct Buffer {
    name: String,
    slot: Option<u32>,
    space: u32,
    size: u32,
    variables: Vec<Variable>,
    /// Every 4-byte value a variable covers, by its full name (`cbMtd.FC_AlphaRef.x`) and base type.
    leaves: Vec<Leaf>,
}

#[derive(Serialize)]
struct Variable {
    name: String,
    offset: u32,
    size: u32,
    used: bool,
    #[serde(rename = "type")]
    ty: String,
}

#[derive(Serialize)]
struct Leaf {
    offset: u32,
    name: String,
    /// `D3D_SHADER_VARIABLE_TYPE`: 1 bool, 2 int, 3 float, 19 uint.
    base: u16,
}

/// `D3D_SHADER_INPUT_TYPE` as the disassembly's binding table spells it.
fn input_kind(kind: u32) -> &'static str {
    match kind {
        0 => "cbuffer",
        1 => "tbuffer",
        2 => "texture",
        3 => "sampler",
        4 => "UAV typed",
        5 => "structured",
        6 => "UAV structured",
        7 => "byteaddress",
        8 => "UAV byteaddress",
        9 => "UAV append",
        10 => "UAV consume",
        11 => "UAV structured with counter",
        12 => "acceleration structure",
        13 => "UAV feedback texture",
        _ => "?",
    }
}

/// The register letter of a class.
fn letter(class: &str) -> &'static str {
    match class {
        "cb" => "b",
        "sampler" => "s",
        "uav" => "u",
        _ => "t",
    }
}

pub fn shader(c: &Capture, hash: &str, disassemble: bool, as_json: bool) -> Result<(), String> {
    let (h, stage, key): (String, String, Option<String>) = c
        .row(
            "SELECT hash, stage, blob FROM shaders WHERE hash LIKE ?1 || '%' ORDER BY hash LIMIT 1",
            [hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok_or_else(|| format!("no shader {hash}"))?;
    let key = key.ok_or("its bytecode was not found when the capture was written")?;
    let code = c.blob(&key)?;
    let bindings = rdef::bindings(&code);
    let cbs = rdef::constant_buffers(&code);
    if as_json {
        let s = Shader {
            hash: h,
            stage,
            bytes: code.len(),
            blob: key,
            bindings: bindings.map(|bs| {
                bs.into_iter()
                    .map(|b| Binding {
                        kind_name: input_kind(b.kind),
                        class: b.class(),
                        kind: b.kind,
                        slot: b.slot,
                        count: b.count,
                        space: b.space,
                        name: b.name,
                    })
                    .collect()
            }),
            constant_buffers: cbs.map(|cbs| {
                cbs.into_iter()
                    .map(|cb| Buffer {
                        leaves: (0..cb.size / 4)
                            .filter_map(|i| {
                                cb.at(i * 4).map(|(name, base)| Leaf {
                                    offset: i * 4,
                                    name,
                                    base,
                                })
                            })
                            .collect(),
                        variables: cb
                            .variables
                            .iter()
                            .map(|v| Variable {
                                name: v.name.clone(),
                                offset: v.offset,
                                size: v.size,
                                used: v.used,
                                ty: v.ty.name(),
                            })
                            .collect(),
                        name: cb.name,
                        slot: cb.slot,
                        space: cb.space,
                        size: cb.size,
                    })
                    .collect()
            }),
        };
        out!("{}", serde_json::to_string(&s).map_err(|e| e.to_string())?);
        return Ok(());
    }
    out!("{stage} {h}: {} bytes, blob {key}", code.len());
    match cbs {
        Some(cbs) => {
            for cb in cbs {
                out!(
                    "cbuffer {} : b{}{} ({} bytes)",
                    cb.name,
                    cb.slot.map_or("?".into(), |s| s.to_string()),
                    if cb.space != 0 {
                        format!(" space {}", cb.space)
                    } else {
                        String::new()
                    },
                    cb.size
                );
                for v in &cb.variables {
                    out!(
                        "  c{:<3} +{:<3} {:<4} {}{}",
                        v.offset / 16,
                        v.offset % 16,
                        v.size,
                        v.name,
                        if v.used { "" } else { "  (unused)" }
                    );
                }
            }
        }
        None => out!("no reflection (RDEF) in its bytecode"),
    }
    // The resources it reads and writes other than constant buffers.
    for b in bindings.unwrap_or_default().iter().filter(|b| b.class() != "cb") {
        let count = match b.count {
            1 => String::new(),
            0 => "[]".into(),
            n => format!("[{n}]"),
        };
        let space = if b.space != 0 {
            format!(" space {}", b.space)
        } else {
            String::new()
        };
        out!(
            "{} {}{count} : {}{}{space}",
            input_kind(b.kind),
            b.name,
            letter(b.class()),
            b.slot
        );
    }
    if disassemble {
        out!("");
        out!("{}", disassembly(&code)?);
    }
    Ok(())
}

/// The bytecode's disassembly, from the system's shader compiler.
pub fn disassembly(code: &[u8]) -> Result<String, String> {
    let blob = unsafe { D3DDisassemble(code.as_ptr().cast(), code.len(), 0, None) }
        .map_err(|e| format!("D3DDisassemble: {e}"))?;
    let text =
        unsafe { core::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) };
    Ok(String::from_utf8_lossy(text).trim_end_matches('\0').to_string())
}
