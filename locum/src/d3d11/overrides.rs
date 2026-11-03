//! Changing what the game draws, by rules in `overrides.txt` in locum's folder (the language is in
//! [`locum_capture::rules`], shared with replay). The file is read again when it changes (checked every 30 frames);
//! removing it removes the rules.
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

use locum_capture::rules::{self, Action};

use crate::d3d11::shaders::{self, Stage};
use crate::d3d11::stages::{self, CbBinding};
use crate::log::{folder, log};
use crate::system::{export, load_system_dll};

static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether any rule is loaded: the only check on the draw path otherwise.
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// The shader stages by the rules' numbering.
const STAGES: [Stage; 6] = [
    Stage::Vertex,
    Stage::Hull,
    Stage::Domain,
    Stage::Geometry,
    Stage::Pixel,
    Stage::Compute,
];

/// A rule, and what applying it in the game needs.
struct Rule {
    rule: rules::Rule,
    stage: Stage,
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

/// At each present: reads `overrides.txt` again if it changed (every 30 frames), and reports how often each rule
/// applied (every 600).
pub fn poll(frame: u64) {
    if frame.is_multiple_of(600)
        && let Some(rules) = current()
    {
        for r in &rules.rules {
            log!(
                "override {:?}: applied {} times",
                r.rule.text,
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
    let (parsed, errors) = rules::parse_all(text, dir, &rules::hex);
    for (line, e) in errors {
        log!("overrides.txt line {line}: {e}: ignored");
    }
    parsed
        .into_iter()
        .map(|rule| Rule {
            stage: STAGES[rule.stage],
            rule,
            applied: AtomicU64::new(0),
            made: Mutex::new(None),
            values: Mutex::new(None),
            scratch: Mutex::new(HashMap::new()),
        })
        .collect()
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
        rule.rule.holds(|stage| self.hash(STAGES[stage]))
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
    let applies = |r: &Rule| r.rule.applies(compute);
    if rules.rules.iter().any(|r| {
        matches!(r.rule.action, Action::Skip) && applies(r) && bound.holds(r) && {
            r.applied.fetch_add(1, Ordering::Relaxed);
            true
        }
    }) {
        return;
    }
    let mut undo = Vec::new();
    let mut replaced = [false; 6];
    for r in &rules.rules {
        if matches!(r.rule.action, Action::Skip) || !applies(r) || !bound.holds(r) {
            continue;
        }
        match &r.rule.action {
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
        let code = match &r.rule.action {
            Action::Solid(colour) => solid_ps(colour)?,
            Action::Shader(code) => code.clone(),
            _ => return None,
        };
        match unsafe { stages::create_shader(&device, r.stage, &code) } {
            Ok(s) => Some(s),
            Err(e) => {
                log!("override {:?}: the shader could not be made: {e}", r.rule.text);
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
    let source = rules::solid_source(colour);
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
