//! Changing what the game draws, by rules in `overrides.txt` in locum's folder. The file is read again when it
//! changes (checked every 30 frames); removing it removes the rules. One rule per line, `#` starts a comment:
//!
//! - `skip vs=<hash>`: draws with that shader bound are not drawn (`skip cs=<hash>`: dispatches are not run);
//! - `solid vs=<hash> R G B A`: draws with that shader bound get a pixel shader writing that colour to every target;
//! - `shader ps=<hash> FILE`: that shader is replaced by the bytecode in FILE (relative to locum's folder);
//! - `constant ps b3 c0 X Y Z W`: the draw sees constant 0 of its ps b3 buffer (counted from the first constant
//!   bound) as X Y Z W; a value is a float or `0x` and 8 hex digits (raw bits).
//!
//! Any rule can end with conditions, `if vs=<hash> ps=<hash>`, all of which must hold; `vs=<hash>` after `skip`,
//! `solid` and `shader` is such a condition too. Any stage can be named: vs hs ds gs ps cs.
//!
//! Rules apply at the draw, on the draw's own context, and are undone after it: a replaced shader or constant buffer
//! is bound for that draw only. A constant is changed in a copy of the bound buffer made by the GPU at the draw, so
//! draws on deferred contexts get theirs when their command list runs.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::SystemTime;

use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D11::*;
use windows::core::{HRESULT, Interface, PCSTR};

use crate::log::{folder, log};
use crate::shaders::{self, Stage};
use crate::stages::{self, CbBinding};
use crate::system::{export, load_system_dll};

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether any rule is loaded: the only check on the draw path otherwise.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

enum Action {
    Skip,
    /// A pixel shader writing one colour.
    Solid([f32; 4]),
    /// A shader of `Rule::stage` from bytecode.
    Shader(Vec<u8>),
    Constant {
        slot: u32,
        index: u32,
        bits: [u32; 4],
    },
}

struct Rule {
    text: String,
    /// The stage the rule changes (pixel for `solid`).
    stage: Stage,
    when: Vec<(Stage, u64)>,
    action: Action,
    applied: AtomicU64,
    /// The replacement shader, made on first use (None if that failed).
    made: Mutex<Option<Option<ID3D11DeviceChild>>>,
    /// For `constant`: the 16 bytes to copy in, and scratch buffers by size.
    values: Mutex<Option<Option<ID3D11Buffer>>>,
    scratch: Mutex<HashMap<u32, ID3D11Buffer>>,
}

struct Rules {
    rules: Vec<Rule>,
    loaded: Option<SystemTime>,
}

static RULES: RwLock<Option<Arc<Rules>>> = RwLock::new(None);

fn current() -> Option<Arc<Rules>> {
    RULES.read().unwrap_or_else(|e| e.into_inner()).clone()
}

fn stage_named(name: &str) -> Option<Stage> {
    Some(match name {
        "vs" => Stage::Vertex,
        "hs" => Stage::Hull,
        "ds" => Stage::Domain,
        "gs" => Stage::Geometry,
        "ps" => Stage::Pixel,
        "cs" => Stage::Compute,
        _ => return None,
    })
}

/// `vs=0123456789abcdef`.
fn condition(word: &str) -> Result<(Stage, u64), String> {
    let (stage, hash) = word
        .split_once('=')
        .ok_or_else(|| format!("{word:?} is not stage=hash"))?;
    let stage = stage_named(stage).ok_or_else(|| format!("unknown stage {stage:?}"))?;
    let hash = u64::from_str_radix(hash, 16).map_err(|_| format!("{hash:?} is not a hex hash"))?;
    Ok((stage, hash))
}

fn value_bits(word: &str) -> Result<u32, String> {
    if let Some(hex) = word.strip_prefix("0x") {
        return u32::from_str_radix(hex, 16).map_err(|_| format!("{word:?} is not 0x and hex digits"));
    }
    word.parse::<f32>()
        .map(f32::to_bits)
        .map_err(|_| format!("{word:?} is not a number"))
}

fn numbered(word: Option<&str>, prefix: char, what: &str) -> Result<u32, String> {
    word.and_then(|w| w.strip_prefix(prefix))
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("expected {what} ({prefix}N)"))
}

fn parse_rule(line: &str, dir: &std::path::Path) -> Result<Rule, String> {
    let (body, conditions) = match line.split_once(" if ") {
        Some((b, c)) => (
            b,
            c.split_whitespace()
                .map(condition)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        None => (line, Vec::new()),
    };
    let mut words = body.split_whitespace();
    let verb = words.next().ok_or("empty rule")?;
    let mut when = conditions;
    let (stage, action) = match verb {
        "skip" | "solid" | "shader" => {
            let first = condition(words.next().ok_or("expected stage=hash")?)?;
            when.insert(0, first);
            match verb {
                "skip" => (first.0, Action::Skip),
                "solid" => {
                    let mut c = [0f32; 4];
                    for v in &mut c {
                        let w = words.next().ok_or("solid needs R G B A")?;
                        *v = w.parse().map_err(|_| format!("{w:?} is not a number"))?;
                    }
                    (Stage::Pixel, Action::Solid(c))
                }
                _ => {
                    let file = words.next().ok_or("shader needs a file")?;
                    let code = std::fs::read(dir.join(file)).map_err(|e| format!("{file}: {e}"))?;
                    (first.0, Action::Shader(code))
                }
            }
        }
        "constant" => {
            let stage = words
                .next()
                .and_then(stage_named)
                .ok_or("constant needs a stage")?;
            let slot = numbered(words.next(), 'b', "a constant buffer slot")?;
            let index = numbered(words.next(), 'c', "a constant")?;
            let mut bits = [0u32; 4];
            for b in &mut bits {
                *b = value_bits(words.next().ok_or("constant needs X Y Z W")?)?;
            }
            if slot >= stages::CB_SLOTS || index >= D3D11_REQ_CONSTANT_BUFFER_ELEMENT_COUNT {
                return Err("slot or constant out of range".into());
            }
            (stage, Action::Constant { slot, index, bits })
        }
        other => return Err(format!("unknown rule {other:?}")),
    };
    if let Some(extra) = words.next() {
        return Err(format!("unexpected {extra:?}"));
    }
    Ok(Rule {
        text: line.to_string(),
        stage,
        when,
        action,
        applied: AtomicU64::new(0),
        made: Mutex::new(None),
        values: Mutex::new(None),
        scratch: Mutex::new(HashMap::new()),
    })
}

/// At each present: reads `overrides.txt` again if it changed (every 30 frames), and reports how often each rule
/// applied (every 600).
pub fn poll(frame: u64) {
    if frame.is_multiple_of(600)
        && let Some(rules) = current()
    {
        for r in &rules.rules {
            log!(
                "override {:?}: applied {} times",
                r.text,
                r.applied.load(Ordering::Relaxed)
            );
        }
    }
    if !frame.is_multiple_of(30) {
        return;
    }
    let Some(dir) = folder() else { return };
    let path = dir.join("overrides.txt");
    let modified = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    let loaded = current().and_then(|r| r.loaded);
    if modified == loaded {
        return;
    }
    let rules = match (modified, std::fs::read_to_string(&path)) {
        (Some(_), Ok(text)) => load(&text, &dir),
        _ => Vec::new(),
    };
    log!("overrides: {} rules from {}", rules.len(), path.display());
    ACTIVE.store(!rules.is_empty(), Ordering::Relaxed);
    *RULES.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(Rules {
        rules,
        loaded: modified,
    }));
}

fn load(text: &str, dir: &std::path::Path) -> Vec<Rule> {
    let mut rules = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        match parse_rule(line, dir) {
            Ok(r) => rules.push(r),
            Err(e) => log!("overrides.txt line {}: {e}: ignored", n + 1),
        }
    }
    rules
}

/// The shaders bound at a draw, hashed once each when a rule asks.
struct Bound<'a> {
    c: &'a ID3D11DeviceContext4,
    hashes: [Option<Option<u64>>; 6],
}

impl Bound<'_> {
    fn hash(&mut self, stage: Stage) -> Option<u64> {
        *self.hashes[stage as usize].get_or_insert_with(|| {
            let shader = unsafe { stages::shader(self.c, stage) }?;
            unsafe { shaders::hash_of(shader.as_raw()) }
        })
    }

    fn holds(&mut self, rule: &Rule) -> bool {
        rule.when
            .iter()
            .all(|&(stage, hash)| self.hash(stage) == Some(hash))
    }
}

enum Undo {
    Shader(Stage, Option<ID3D11DeviceChild>),
    Cb(Stage, u32, CbBinding),
}

/// Runs `draw` (the real draw or dispatch on `c`) with the rules that hold for it applied, then undoes them.
pub unsafe fn around(c: &ID3D11DeviceContext4, compute: bool, draw: impl FnOnce()) {
    let Some(rules) = current() else { return draw() };
    let mut bound = Bound { c, hashes: [None; 6] };
    let applies = |r: &Rule| compute == (r.stage == Stage::Compute);
    if rules.rules.iter().any(|r| {
        matches!(r.action, Action::Skip) && applies(r) && bound.holds(r) && {
            r.applied.fetch_add(1, Ordering::Relaxed);
            true
        }
    }) {
        return;
    }
    let mut undo = Vec::new();
    let mut replaced = [false; 6];
    for r in &rules.rules {
        if matches!(r.action, Action::Skip) || !applies(r) || !bound.holds(r) {
            continue;
        }
        match &r.action {
            Action::Solid(_) | Action::Shader(_) if !replaced[r.stage as usize] => {
                let Some(shader) = (unsafe { replacement(c, r) }) else {
                    continue;
                };
                replaced[r.stage as usize] = true;
                undo.push(Undo::Shader(r.stage, unsafe { stages::shader(c, r.stage) }));
                unsafe { stages::set_shader(c, r.stage, Some(&shader)) };
            }
            Action::Constant { slot, index, bits } => {
                let Some(original) = (unsafe { patched(c, r, *slot, *index, bits) }) else {
                    continue;
                };
                undo.push(Undo::Cb(r.stage, *slot, original));
            }
            _ => continue,
        }
        r.applied.fetch_add(1, Ordering::Relaxed);
    }
    draw();
    for u in undo.into_iter().rev() {
        match u {
            Undo::Shader(stage, shader) => unsafe { stages::set_shader(c, stage, shader.as_ref()) },
            Undo::Cb(stage, slot, binding) => unsafe {
                stages::set_constant_buffer(c, stage, slot, &binding)
            },
        }
    }
}

/// The rule's replacement shader, made on the real device the first time.
unsafe fn replacement(c: &ID3D11DeviceContext4, r: &Rule) -> Option<ID3D11DeviceChild> {
    let mut made = r.made.lock().unwrap_or_else(|e| e.into_inner());
    made.get_or_insert_with(|| {
        let device = unsafe { c.GetDevice() }.ok()?;
        let code = match &r.action {
            Action::Solid(colour) => solid_ps(colour)?,
            Action::Shader(code) => code.clone(),
            _ => return None,
        };
        match unsafe { stages::create_shader(&device, r.stage, &code) } {
            Ok(s) => Some(s),
            Err(e) => {
                log!("override {:?}: the shader could not be made: {e}", r.text);
                None
            }
        }
    })
    .clone()
}

/// Binds a copy of the constant buffer at `slot` with one constant changed; returns the binding to restore.
unsafe fn patched(
    c: &ID3D11DeviceContext4,
    r: &Rule,
    slot: u32,
    index: u32,
    bits: &[u32; 4],
) -> Option<CbBinding> {
    let original = unsafe { stages::constant_buffers(c, r.stage) }
        .into_iter()
        .nth(slot as usize)?;
    let source = original.buffer.clone()?;
    let mut desc = D3D11_BUFFER_DESC::default();
    unsafe { source.GetDesc(&mut desc) };
    let at = (original.first + index) * 16;
    if at + 16 > desc.ByteWidth {
        return None;
    }
    let device = unsafe { c.GetDevice() }.ok()?;
    let values = r
        .values
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(|| unsafe { buffer(&device, 16, Some(bits)) })
        .clone()?;
    let scratch = {
        let mut scratch = r.scratch.lock().unwrap_or_else(|e| e.into_inner());
        match scratch.get(&desc.ByteWidth) {
            Some(b) => b.clone(),
            None => {
                let b = unsafe { buffer(&device, desc.ByteWidth, None) }?;
                scratch.insert(desc.ByteWidth, b.clone());
                b
            }
        }
    };
    let whole = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: desc.ByteWidth,
        bottom: 1,
        back: 1,
    };
    let one = D3D11_BOX {
        left: 0,
        top: 0,
        front: 0,
        right: 16,
        bottom: 1,
        back: 1,
    };
    unsafe {
        c.CopySubresourceRegion(&scratch, 0, 0, 0, 0, &source, 0, Some(&whole));
        c.CopySubresourceRegion(&scratch, 0, at, 0, 0, &values, 0, Some(&one));
        let binding = CbBinding {
            buffer: Some(scratch),
            first: original.first,
            count: original.count,
        };
        stages::set_constant_buffer(c, r.stage, slot, &binding);
    }
    Some(original)
}

/// A default-usage constant buffer of `bytes`, holding `bits` if given.
unsafe fn buffer(device: &ID3D11Device, bytes: u32, bits: Option<&[u32; 4]>) -> Option<ID3D11Buffer> {
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: bytes,
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
        ..Default::default()
    };
    let init = bits.map(|b| D3D11_SUBRESOURCE_DATA {
        pSysMem: b.as_ptr().cast(),
        ..Default::default()
    });
    let mut out = None;
    unsafe { device.CreateBuffer(&desc, init.as_ref().map(|i| i as *const _), Some(&mut out)) }.ok()?;
    out
}

type CompileFn = unsafe extern "system" fn(
    *const c_void,
    usize,
    PCSTR,
    *const c_void,
    *mut c_void,
    PCSTR,
    PCSTR,
    u32,
    u32,
    *mut *mut c_void,
    *mut *mut c_void,
) -> HRESULT;

/// A ps_5_0 shader writing `colour` to all eight targets, compiled with the system's d3dcompiler_47.dll.
fn solid_ps(colour: &[f32; 4]) -> Option<Vec<u8>> {
    let [r, g, b, a] = colour;
    let targets: String = (0..8).map(|i| format!("float4 c{i} : SV_Target{i}; ")).collect();
    let fill: String = (0..8).map(|i| format!("o.c{i} = k; ")).collect();
    let source = format!(
        "struct O {{ {targets}}}; O main() {{ float4 k = float4({r:?}, {g:?}, {b:?}, {a:?}); O o; {fill}return o; }}"
    );
    let module = load_system_dll("d3dcompiler_47.dll")?;
    let compile = unsafe { export::<CompileFn>(module, c"D3DCompile") }?;
    let (mut code, mut errors) = (core::ptr::null_mut(), core::ptr::null_mut());
    let hr = unsafe {
        compile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(c"locum-solid".as_ptr().cast()),
            core::ptr::null(),
            core::ptr::null_mut(),
            PCSTR(c"main".as_ptr().cast()),
            PCSTR(c"ps_5_0".as_ptr().cast()),
            0,
            0,
            &mut code,
            &mut errors,
        )
    };
    let blob = |p: *mut c_void| (!p.is_null()).then(|| unsafe { ID3DBlob::from_raw(p) });
    let bytes = |b: &ID3DBlob| unsafe {
        core::slice::from_raw_parts(b.GetBufferPointer() as *const u8, b.GetBufferSize()).to_vec()
    };
    if let Some(e) = blob(errors) {
        log!("solid shader: {}", String::from_utf8_lossy(&bytes(&e)));
    }
    let code = blob(code);
    if hr.is_err() {
        log!("solid shader did not compile: {hr:?}");
        return None;
    }
    code.map(|c| bytes(&c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rule(line: &str) -> Result<Rule, String> {
        parse_rule(line, &PathBuf::from("."))
    }

    #[test]
    fn rules_parse() {
        let r = rule("skip vs=00000000000000ff if ps=10").unwrap();
        assert!(matches!(r.action, Action::Skip));
        assert_eq!(r.when.len(), 2);
        let r = rule("constant vs b11 c0 4 4 0x3f800000 -1.5").unwrap();
        match r.action {
            Action::Constant { slot, index, bits } => {
                assert_eq!((slot, index), (11, 0));
                assert_eq!(
                    bits,
                    [
                        4f32.to_bits(),
                        4f32.to_bits(),
                        1f32.to_bits(),
                        (-1.5f32).to_bits()
                    ]
                );
            }
            _ => panic!("not a constant rule"),
        }
        assert!(matches!(
            rule("solid vs=1 1 0 1 1").unwrap().action,
            Action::Solid([1.0, 0.0, 1.0, 1.0])
        ));
        assert!(rule("constant vs b14 c0 0 0 0 0").is_err());
        assert!(rule("skip vs").is_err());
        assert!(rule("frobnicate").is_err());
    }
}
