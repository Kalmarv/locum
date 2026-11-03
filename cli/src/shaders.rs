//! `shaders` and `shader`: the shaders the frame used, their constant buffers and disassembly.

use std::collections::HashMap;

use locum_capture::rdef;
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

pub fn shader(c: &Capture, hash: &str, disassemble: bool) -> Result<(), String> {
    let (h, stage, key): (String, String, Option<String>) = c
        .row(
            "SELECT hash, stage, blob FROM shaders WHERE hash LIKE ?1 || '%' ORDER BY hash LIMIT 1",
            [hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok_or_else(|| format!("no shader {hash}"))?;
    let key = key.ok_or("its bytecode was not found when the capture was written")?;
    let code = c.blob(&key)?;
    out!("{stage} {h}: {} bytes, blob {key}", code.len());
    match rdef::constant_buffers(&code) {
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
