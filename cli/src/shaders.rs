//! `shaders` and `shader`: the shaders the frame used, their constant buffers and disassembly.

use std::collections::HashMap;
use std::path::Path;

use locum_capture::rdef;
use serde::Serialize;
use windows::Win32::Graphics::Direct3D::Fxc::{D3DCompile, D3DDisassemble};
use windows::Win32::Graphics::Direct3D::{ID3DBlob, ID3DInclude};
use windows::core::PCSTR;

use crate::open::Capture;
use crate::out;

/// The frame's programs, with the draws using each and their names: of stage `stage`, and whose name holds `name`
/// (any case), when given; with `bindings`, each with the registers its reflection declares.
pub fn shaders(c: &Capture, stage: Option<&str>, name: Option<&str>, bindings: bool) -> Result<(), String> {
    let wanted = name.map(str::to_lowercase);
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
    let mut reflection = crate::describe::Reflection::default();
    out!(
        "{:<5} {:<16} {:>7} {:>6}  name",
        "stage",
        "hash",
        "bytes",
        "draws"
    );
    let mut shown = 0;
    for (h, stage_of, size) in rows {
        if stage.is_some_and(|s| s != stage_of) {
            continue;
        }
        let named = reflection.name(c, &h).map(str::to_string);
        if wanted
            .as_ref()
            .is_some_and(|w| !named.as_ref().is_some_and(|n| n.to_lowercase().contains(w)))
        {
            continue;
        }
        shown += 1;
        out!(
            "{stage_of:<5} {h:<16} {:>7} {:>6}  {}",
            size.map_or("-".into(), |s| s.to_string()),
            uses.get(&h).copied().unwrap_or(0),
            named.as_deref().unwrap_or("-")
        );
        if bindings {
            let declared = c
                .row(
                    "SELECT blob FROM shaders WHERE hash = ?1 AND blob IS NOT NULL",
                    [&h],
                    |r| r.get::<_, String>(0),
                )
                .and_then(|key| c.blob(&key).ok())
                .and_then(|code| locum_capture::rdef::bindings(&code));
            match declared {
                Some(list) if !list.is_empty() => {
                    for b in list {
                        let letter = letter(b.class());
                        let count = match b.count {
                            0 => "[]".to_string(),
                            1 => String::new(),
                            n => format!("[{n}]"),
                        };
                        let space = match b.space {
                            0 => String::new(),
                            s => format!(" space{s}"),
                        };
                        out!("      {letter}{}{space}  {}{count}", b.slot, b.name);
                    }
                }
                Some(_) => out!("      (declares no registers)"),
                None => out!("      (no reflection)"),
            }
        }
    }
    if shown == 0 && (stage.is_some() || name.is_some()) {
        out!(
            "no program matches (names come from the programs' debug names, or from the files an index names: --index)"
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

pub fn shader(
    c: &Capture,
    hash: &str,
    disassemble: bool,
    wgsl: bool,
    as_json: bool,
    evidence: Option<&Path>,
) -> Result<(), String> {
    let (h, stage, key): (String, String, Option<String>) = c
        .row(
            "SELECT hash, stage, blob FROM shaders WHERE hash LIKE ?1 || '%' ORDER BY hash LIMIT 1",
            [hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok_or_else(|| format!("no shader {hash}"))?;
    let key = key.ok_or("its bytecode was not found when the capture was written")?;
    let code = c.blob(&key)?;
    if as_json {
        let s = Shader {
            hash: h,
            stage,
            bytes: code.len(),
            blob: key,
            bindings: rdef::bindings(&code).map(|bs| {
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
            constant_buffers: rdef::constant_buffers(&code).map(|cbs| {
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
    show(
        &code,
        &format!("{}:shader/{h}", c.path.display()),
        disassemble,
        wgsl,
        evidence,
    )
}

/// `shader-file`: the same for a shader on disk, with `--evidence` writing a DXIL program's evidence to a file.
pub fn file(path: &Path, disassemble: bool, wgsl: bool, evidence: Option<&Path>) -> Result<(), String> {
    let code = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    out!("{}: {} bytes", path.display(), code.len());
    show(&code, &path.display().to_string(), disassemble, wgsl, evidence)
}

/// A program's interface: for DXIL, the reflection answers of dx, Microsoft's compiler and iridia side by side (also
/// written to `evidence`); for DXBC, its constant buffers and bindings. Then its disassembly and WGSL when asked.
fn show(
    code: &[u8],
    source: &str,
    disassemble: bool,
    wgsl: bool,
    evidence: Option<&Path>,
) -> Result<(), String> {
    if let Some(name) = rdef::debug_name(code) {
        out!("debug name: {name}");
    }
    if crate::dxil::is_dxil(code) {
        constant_buffers_and_bindings(code);
        // The reflection above is dxil-reader's; Microsoft's and iridia's are checked against it.
        let report = crate::dxil::evidence(code, source);
        if let Some(path) = evidence {
            let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
            std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
        }
        out!(
            "reflection cross-check (dxil-reader, Microsoft's, iridia): {}{}",
            report["comparison"].as_str().unwrap_or("?"),
            if evidence.is_none() && report["comparison"] != "agree" {
                " (--evidence FILE writes every answer)"
            } else {
                ""
            }
        );
        for (key, who) in [
            ("dxil_reader", "dxil-reader"),
            ("microsoft", "Microsoft's"),
            ("iridia", "iridia"),
        ] {
            if let Some(e) = report[key]["error"].as_str() {
                out!("  {who}: {e}");
            }
        }
    } else if evidence.is_some() {
        return Err(format!("{source} is DXBC: --evidence compares DXIL reflection"));
    } else {
        constant_buffers_and_bindings(code);
    }
    if disassemble {
        out!("");
        out!("{}", disassembly(code)?);
    }
    if wgsl {
        out!("");
        out!("{}", crate::dxil::wgsl(code)?);
    }
    Ok(())
}

fn constant_buffers_and_bindings(code: &[u8]) {
    match rdef::constant_buffers(code) {
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
        None => out!("no reflection in its bytecode (DXBC RDEF, or DXIL STAT or DXIL)"),
    }
    // The resources it reads and writes other than constant buffers.
    for b in rdef::bindings(code)
        .unwrap_or_default()
        .iter()
        .filter(|b| b.class() != "cb")
    {
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
}

/// The bytecode's disassembly, from the system's shader compiler.
pub fn disassembly(code: &[u8]) -> Result<String, String> {
    if crate::dxil::is_dxil(code) {
        return crate::dxil::disassemble(code);
    }
    let blob = unsafe { D3DDisassemble(code.as_ptr().cast(), code.len(), 0, None) }
        .map_err(|e| format!("D3DDisassemble: {e}"))?;
    let text =
        unsafe { core::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize()) };
    Ok(String::from_utf8_lossy(text).trim_end_matches('\0').to_string())
}

/// Compiles a shader for `target` (`ps_5_0`) with the system's shader compiler.
pub fn compile(source: &str, target: &core::ffi::CStr) -> Result<Vec<u8>, String> {
    let (mut code, mut errors) = (None, None);
    let r = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(c"locum-replay".as_ptr().cast()),
            None,
            None::<&ID3DInclude>,
            PCSTR(c"main".as_ptr().cast()),
            PCSTR(target.as_ptr().cast()),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    let bytes = |b: &ID3DBlob| unsafe {
        core::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec()
    };
    if let Err(e) = r {
        let text = errors
            .map(|e| String::from_utf8_lossy(&bytes(&e)).into_owned())
            .unwrap_or_default();
        return Err(format!("{e}: {text}"));
    }
    Ok(code.map(|c| bytes(&c)).unwrap_or_default())
}
