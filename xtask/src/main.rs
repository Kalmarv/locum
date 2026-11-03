//! `cargo xtask gen`: writes `locum/src/vtables/*.rs`, the forwarding vtables of the interfaces locum wraps.
//!
//! The `windows` crate publishes every COM interface's vtable as a `#[repr(C)]` struct of `extern "system"` function
//! pointers with the exact ABI (`ID3D11DeviceContext_Vtbl` and so on). For each wrapped interface this reads that
//! struct and its bases from the crate's source and generates:
//! - a trait with one method per vtable entry. Every method but the three of IUnknown has a default that calls the
//!   wrapped object's own entry, so a wrapper forwards everything it does not override;
//! - one `extern "system"` thunk per entry, which turns the COM `this` back into the wrapper and calls the trait method;
//! - a `const fn` that builds the whole vtable from those thunks for a given wrapper type.
//!
//! Regenerate after changing the `windows` version. The generated files are checked in.

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use quote::ToTokens;
use syn::{Fields, Item, Type};

/// A wrapped interface: the generated module's name, the trait's name, the most derived interface and the module
/// (under `windows::`) whose source defines it.
struct Class {
    module: &'static str,
    trait_name: &'static str,
    interface: &'static str,
    source: &'static [&'static str],
}

const CLASSES: &[Class] = &[
    Class {
        module: "device",
        trait_name: "DeviceMethods",
        interface: "ID3D11Device5",
        source: &["Win32", "Graphics", "Direct3D11"],
    },
    Class {
        module: "context",
        trait_name: "ContextMethods",
        interface: "ID3D11DeviceContext4",
        source: &["Win32", "Graphics", "Direct3D11"],
    },
    Class {
        module: "factory",
        trait_name: "FactoryMethods",
        interface: "IDXGIFactory7",
        source: &["Win32", "Graphics", "Dxgi"],
    },
    Class {
        module: "swapchain",
        trait_name: "SwapChainMethods",
        interface: "IDXGISwapChain4",
        source: &["Win32", "Graphics", "Dxgi"],
    },
    Class {
        module: "dxgi_device",
        trait_name: "DxgiDeviceMethods",
        interface: "IDXGIDevice4",
        source: &["Win32", "Graphics", "Dxgi"],
    },
    Class {
        module: "adapter",
        trait_name: "AdapterMethods",
        interface: "IDXGIAdapter4",
        source: &["Win32", "Graphics", "Dxgi"],
    },
];

/// Source modules whose vtable structs are read (bases can live in another module than the interface).
const MODULES: &[&[&str]] = &[
    &["Win32", "Graphics", "Direct3D11"],
    &["Win32", "Graphics", "Dxgi"],
];

/// One vtable entry: its name, its parameters after `this`, and its return type (absolute paths).
struct Entry {
    name: String,
    params: Vec<String>,
    ret: Option<String>,
}

/// A vtable struct: the base it extends (`None` for IUnknown) and its own entries.
struct Vtbl {
    base: Option<String>,
    entries: Vec<Entry>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf();
    match args.first().map(String::as_str) {
        Some("gen") => generate_all(&root),
        Some("smoke") => smoke(&root),
        _ => {
            eprintln!("usage: cargo xtask gen | smoke");
            std::process::exit(2);
        }
    }
}

/// Builds the release DLLs and the smoke program, puts them in `target/smoke/` and runs it there, with locum's log
/// in `target/smoke/log/`.
fn smoke(root: &Path) {
    let status = Command::new(env!("CARGO"))
        .current_dir(root)
        .args([
            "build",
            "--release",
            "-p",
            "locum",
            "-p",
            "locum-shim-d3d11",
            "-p",
            "locum-shim-dxgi",
            "-p",
            "locum-smoke",
            "-p",
            "locum-cli",
        ])
        .status()
        .expect("run cargo build");
    assert!(status.success(), "build failed");
    let release = root.join("target/release");
    let stage = root.join("target/smoke");
    let log_dir = stage.join("log");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&log_dir).expect("create target/smoke");
    for file in [
        "locum.dll",
        "d3d11.dll",
        "dxgi.dll",
        "locum-smoke.exe",
        "locum-cli.exe",
    ] {
        std::fs::copy(release.join(file), stage.join(file)).unwrap_or_else(|e| panic!("copy {file}: {e}"));
    }
    // Captures dump the process's memory with locum-cli, into log/dumps.
    std::fs::write(
        log_dir.join("settings.txt"),
        format!(
            "dumper {}
dumps {}
",
            stage.join("locum-cli.exe").display(),
            log_dir.join("dumps").display()
        ),
    )
    .expect("write settings.txt");
    let status = Command::new(stage.join("locum-smoke.exe"))
        .current_dir(&stage)
        .env("LOCUM_DIR", &log_dir)
        .status()
        .expect("run locum-smoke");
    println!("--- {}", log_dir.join("locum.log").display());
    print!(
        "{}",
        std::fs::read_to_string(log_dir.join("locum.log")).unwrap_or_else(|e| format!(
            "(no log: {e})
"
        ))
    );
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    if !cli_checks(&stage.join("locum-cli.exe"), &log_dir) {
        std::process::exit(1);
    }
}

/// Runs locum-cli on the smoke test's capture and checks what it says about it.
fn cli_checks(cli: &Path, log_dir: &Path) -> bool {
    let capture = log_dir.join("captures").join("frame-32");
    let run = |args: &[&str]| -> String {
        let out = Command::new(cli)
            .arg(args[0])
            .arg(&capture)
            .args(&args[1..])
            .env("LOCUM_DIR", log_dir)
            .output()
            .expect("run locum-cli");
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        println!(
            "--- locum-cli {}
{text}",
            args.join(" ")
        );
        text
    };
    let checks = [
        (
            run(&["info"]).contains("3 draws and dispatches in 3 passes"),
            "locum-cli info: three draws in three passes",
        ),
        (
            {
                let t = run(&["draw", "0"]);
                t.contains("Frame.value")
                    && t.contains("1032")
                    && t.contains("(upload)")
                    && t.contains("(end)")
            },
            "locum-cli draw 0: ps b3 named Frame.value holding 1032; contents from the upload and read at the end",
        ),
        (
            run(&["find", "32"]).contains("Frame.value.x"),
            "locum-cli find 32: the command list's constant, by name",
        ),
        (
            run(&["events", "--kind", "map"]).contains("Unmap"),
            "locum-cli events --kind map: the map in the command list",
        ),
        (
            {
                // The dump is written after the capture, by its own process: wait for it.
                let dump = log_dir.join("dumps").join("frame-32.dmp");
                for _ in 0..600 {
                    if dump.is_file() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                let t = run(&["memfind", "--cb", "0", "ps", "3"]);
                t.contains("Frame.value: ") && !t.contains("Frame.value: 0 found")
            },
            "locum-cli memfind --cb 0 ps 3: the constant draw 0 saw, found in the process's memory",
        ),
        (
            run(&["verify"]).contains("3 of 3 targets identical"),
            "locum-cli verify: the replayed frame's three targets are the game's, bit for bit",
        ),
        (
            {
                let out = log_dir.join("replay").display().to_string();
                let t = run(&[
                    "replay",
                    "0",
                    "--rule",
                    "constant ps b3 c0 500 500 500 500",
                    "-o",
                    &out,
                ]);
                t.contains("against the game's: identical") && t.contains("4096 of 4096 texels differ")
            },
            "locum-cli replay 0 with a constant rule: as the game drew it before, every texel changed after",
        ),
        (
            {
                let t = run(&["diff", &capture.display().to_string()]);
                t.contains("3 matched, 0 only in the first, 0 only in the second")
                    && t.contains("0 matched draws differ")
            },
            "locum-cli diff: a capture against itself matches every draw and finds nothing different",
        ),
    ];
    let mut ok = true;
    for (passed, what) in checks {
        println!("{}  {what}", if passed { "ok  " } else { "FAIL" });
        ok &= passed;
    }
    ok
}

fn generate_all(root: &Path) {
    let root = root.to_path_buf();
    let windows = windows_source(&root);
    let mut vtbls = HashMap::new();
    let mut features = BTreeSet::new();
    for module in MODULES {
        let file = windows.join("src/Windows").join(module.join("/")).join("mod.rs");
        read_vtbls(&file, module, &mut vtbls, &mut features);
    }
    let out = root.join("locum/src/vtables");
    std::fs::create_dir_all(&out).expect("create locum/src/vtables");
    let mut mods = String::from("//! Generated by `cargo xtask gen`; do not edit.\n\n");
    for class in CLASSES {
        let code = generate(class, &vtbls);
        let path = out.join(format!("{}.rs", class.module));
        std::fs::write(&path, code).expect("write generated module");
        rustfmt(&path);
        writeln!(mods, "pub mod {};", class.module).unwrap();
    }
    let mod_rs = out.join("mod.rs");
    std::fs::write(&mod_rs, mods).expect("write vtables/mod.rs");
    rustfmt(&mod_rs);
    println!("windows features the generated vtables need:");
    for feature in features {
        println!("  {feature}");
    }
}

/// The source folder of the `windows` crate this workspace resolves, from `cargo metadata`.
fn windows_source(root: &Path) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("run cargo metadata");
    assert!(output.status.success(), "cargo metadata failed");
    let text = String::from_utf8(output.stdout).expect("cargo metadata output is UTF-8");
    // The package entry reads `"name":"windows","version":"…",…,"manifest_path":"…"`; find it without a JSON parser.
    let at = text
        .find("\"name\":\"windows\",\"version\":")
        .expect("the windows package is in the dependency graph");
    let rest = &text[at..];
    let key = "\"manifest_path\":\"";
    let start = rest.find(key).expect("manifest_path") + key.len();
    let end = start + rest[start..].find('"').expect("end of manifest_path");
    let manifest = rest[start..end].replace("\\\\", "\\");
    Path::new(&manifest).parent().expect("crate folder").to_path_buf()
}

fn read_vtbls(
    file: &Path,
    module: &[&str],
    vtbls: &mut HashMap<String, Vtbl>,
    features: &mut BTreeSet<String>,
) {
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
    let parsed = syn::parse_file(&text).unwrap_or_else(|e| panic!("parse {}: {e}", file.display()));
    for item in parsed.items {
        let Item::Struct(s) = item else { continue };
        let name = s.ident.to_string();
        if !name.ends_with("_Vtbl") {
            continue;
        }
        let Fields::Named(fields) = s.fields else { continue };
        let mut base = None;
        let mut entries = Vec::new();
        for field in fields.named {
            let field_name = field.ident.as_ref().expect("named field").to_string();
            match cfg_of(&field.attrs) {
                Cfg::Not => continue,
                Cfg::Feature(f) => {
                    features.insert(f);
                }
                Cfg::None => {}
            }
            if field_name == "base__" {
                base = Some(last_ident(&field.ty));
                continue;
            }
            let Type::BareFn(f) = &field.ty else {
                panic!("{name}.{field_name} is not a function pointer")
            };
            let mut inputs = f.inputs.iter();
            inputs.next().expect("this");
            let params = inputs.map(|a| absolute(&a.ty, module)).collect();
            let ret = match &f.output {
                syn::ReturnType::Default => None,
                syn::ReturnType::Type(_, t) => Some(absolute(t, module)),
            };
            entries.push(Entry {
                name: field_name,
                params,
                ret,
            });
        }
        let base = base.filter(|b| b != "IUnknown_Vtbl");
        vtbls.insert(name, Vtbl { base, entries });
    }
}

enum Cfg {
    None,
    Feature(String),
    Not,
}

/// A field's `#[cfg(feature = "…")]` (kept, the feature is recorded) or `#[cfg(not(…))]` (the placeholder, dropped).
fn cfg_of(attrs: &[syn::Attribute]) -> Cfg {
    for attr in attrs {
        if !attr.path().is_ident("cfg") {
            continue;
        }
        let tokens = attr.meta.to_token_stream().to_string();
        if tokens.contains("not (") || tokens.contains("not(") {
            return Cfg::Not;
        }
        let start = tokens.find('"').expect("feature name") + 1;
        let end = start + tokens[start..].find('"').expect("end of feature name");
        return Cfg::Feature(tokens[start..end].to_string());
    }
    Cfg::None
}

fn last_ident(ty: &Type) -> String {
    let Type::Path(p) = ty else {
        panic!("base__ is not a path")
    };
    p.path.segments.last().expect("segment").ident.to_string()
}

/// A type from the source module, written with absolute paths: `windows_core::X` becomes `windows::core::X`,
/// `super::super::Foundation::X` and bare names are resolved against the module, primitives and `core::` stay.
fn absolute(ty: &Type, module: &[&str]) -> String {
    let mut ty = ty.clone();
    rewrite(&mut ty, module);
    ty.to_token_stream().to_string()
}

fn rewrite(ty: &mut Type, module: &[&str]) {
    match ty {
        Type::Ptr(p) => rewrite(&mut p.elem, module),
        Type::Reference(r) => rewrite(&mut r.elem, module),
        Type::Array(a) => rewrite(&mut a.elem, module),
        Type::Slice(s) => rewrite(&mut s.elem, module),
        Type::Paren(p) => rewrite(&mut p.elem, module),
        Type::Tuple(t) => t.elems.iter_mut().for_each(|e| rewrite(e, module)),
        Type::Path(p) => {
            for seg in p.path.segments.iter_mut() {
                if let syn::PathArguments::AngleBracketed(args) = &mut seg.arguments {
                    for arg in args.args.iter_mut() {
                        if let syn::GenericArgument::Type(t) = arg {
                            rewrite(t, module);
                        }
                    }
                }
            }
            let segs: Vec<String> = p.path.segments.iter().map(|s| s.ident.to_string()).collect();
            const PRIMITIVES: &[&str] = &[
                "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
                "f32", "f64", "bool",
            ];
            if segs[0] == "core" || (segs.len() == 1 && PRIMITIVES.contains(&segs[0].as_str())) {
                return;
            }
            let full: Vec<String> = if segs[0] == "windows_core" {
                // windows_core::X -> windows::core::X
                ["windows", "core"]
                    .iter()
                    .map(|s| s.to_string())
                    .chain(segs[1..].iter().cloned())
                    .collect()
            } else {
                // super::super::Foundation::X, Common::X or X: relative to the source module
                let ups = segs.iter().take_while(|s| *s == "super").count();
                std::iter::once("windows".to_string())
                    .chain(module[..module.len() - ups].iter().map(|s| s.to_string()))
                    .chain(segs[ups..].iter().cloned())
                    .collect()
            };
            let last_args = p.path.segments.last().expect("segment").arguments.clone();
            let mut path: syn::Path = syn::parse_str(&full.join("::")).expect("path");
            path.segments.last_mut().expect("segment").arguments = last_args;
            p.path = path;
        }
        _ => {}
    }
}

/// The interface's vtable chain, most basic first (the struct after IUnknown), with each struct's name.
fn chain<'a>(top: &str, vtbls: &'a HashMap<String, Vtbl>) -> Vec<(String, &'a Vtbl)> {
    let mut out = Vec::new();
    let mut name = format!("{top}_Vtbl");
    loop {
        let v = vtbls
            .get(&name)
            .unwrap_or_else(|| panic!("no vtable struct {name}"));
        out.push((name.clone(), v));
        match &v.base {
            Some(b) => name = b.clone(),
            None => break,
        }
    }
    out.reverse();
    out
}

fn generate(class: &Class, vtbls: &HashMap<String, Vtbl>) -> String {
    let levels = chain(class.interface, vtbls);
    let top = format!("windows::{}::{}_Vtbl", class.source.join("::"), class.interface);
    let module_of = |vtbl: &str| -> String {
        // Every struct of the chain lives in one of MODULES; find which by looking the name up in each source.
        for m in MODULES {
            let candidate = format!("windows::{}::{vtbl}", m.join("::"));
            if vtbl.starts_with("ID3D11") && m.ends_with(&["Direct3D11"]) {
                return candidate;
            }
            if vtbl.starts_with("IDXGI") && m.ends_with(&["Dxgi"]) {
                return candidate;
            }
        }
        panic!("no module for {vtbl}");
    };

    let mut seen = BTreeSet::new();
    let mut out = String::new();
    writeln!(
        out,
        "//! Generated by `cargo xtask gen` from the `windows` crate's `{}_Vtbl`; do not edit.\n",
        class.interface
    )
    .unwrap();
    out.push_str(
        "#![allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc, dead_code)]\n\n",
    );
    out.push_str("use core::ffi::c_void;\n\n");
    writeln!(
        out,
        "/// Every method of `{iface}` in vtable order. The defaults forward to the wrapped object [`{t}::inner`].\n\
         ///\n/// # Safety\n/// The implementing type must be `#[repr(C)]` with a pointer to [`vtable`]'s table as its first field,\n\
         /// and `inner` must return a live `{iface}`.",
        iface = class.interface,
        t = class.trait_name
    )
    .unwrap();
    writeln!(out, "pub unsafe trait {}: Sized {{", class.trait_name).unwrap();
    writeln!(out, "    /// The wrapped object, an `{}`.", class.interface).unwrap();
    out.push_str("    fn inner(&self) -> *mut c_void;\n");
    out.push_str("    unsafe fn QueryInterface(&self, iid: *const windows::core::GUID, out: *mut *mut c_void) -> windows::core::HRESULT;\n");
    out.push_str("    unsafe fn AddRef(&self) -> u32;\n");
    out.push_str("    unsafe fn Release(&self) -> u32;\n");
    let mut thunks = String::new();
    let mut real = String::from(
        "/// Each method called on the real (wrapped) object: the trait's defaults, and overrides that still want the\n\
         /// real behaviour.\npub mod real {\n    use core::ffi::c_void;\n\n",
    );
    for (depth, (_, v)) in levels.iter().enumerate() {
        let path = "base__.".repeat(levels.len() - 1 - depth);
        for e in &v.entries {
            assert!(
                seen.insert(e.name.clone()),
                "{} appears twice in {}",
                e.name,
                class.interface
            );
            let params: Vec<String> = e
                .params
                .iter()
                .enumerate()
                .map(|(i, t)| format!("a{i}: {t}"))
                .collect();
            let args: Vec<String> = (0..e.params.len()).map(|i| format!("a{i}")).collect();
            let ret = e.ret.as_ref().map(|r| format!(" -> {r}")).unwrap_or_default();
            let comma = if params.is_empty() { "" } else { ", " };
            writeln!(
                out,
                "    unsafe fn {name}(&self{comma}{params}){ret} {{\n        unsafe {{ real::{name}(self.inner(){comma}{args}) }}\n    }}",
                name = e.name,
                params = params.join(", "),
                args = args.join(", "),
            )
            .unwrap();
            writeln!(
                real,
                "    /// `{name}` on the real object.\n    pub unsafe fn {name}(inner: *mut c_void{comma}{params}){ret} {{\n        unsafe {{ ((*(*(inner as *const *const {top}))).{path}{name})(inner{comma}{args}) }}\n    }}",
                name = e.name,
                params = params.join(", "),
                args = args.join(", "),
            )
            .unwrap();
            writeln!(
                thunks,
                "unsafe extern \"system\" fn {name}<T: {t}>(this: *mut c_void{comma}{params}){ret} {{\n    unsafe {{ (*(this as *const T)).{name}({args}) }}\n}}",
                name = e.name,
                t = class.trait_name,
                comma = if params.is_empty() { "" } else { ", " },
                params = params.join(", "),
                args = args.join(", "),
            )
            .unwrap();
        }
    }
    out.push_str("}\n\n");
    real.push_str("}\n\n");
    out.push_str(&real);
    for (name, ret) in [
        (
            "QueryInterface",
            "iid: *const windows::core::GUID, out: *mut *mut c_void) -> windows::core::HRESULT",
        ),
        ("AddRef", ") -> u32"),
        ("Release", ") -> u32"),
    ] {
        let (params, args) = if name == "QueryInterface" {
            (format!(", {ret}"), "iid, out")
        } else {
            (ret.to_string(), "")
        };
        writeln!(
            out,
            "unsafe extern \"system\" fn {name}<T: {t}>(this: *mut c_void{params} {{\n    unsafe {{ (*(this as *const T)).{name}({args}) }}\n}}",
            t = class.trait_name,
        )
        .unwrap();
    }
    out.push_str(&thunks);

    // The vtable literal, nested from the most derived struct down to IUnknown's.
    let mut literal = String::from(
        "windows::core::IUnknown_Vtbl { QueryInterface: QueryInterface::<T>, AddRef: AddRef::<T>, Release: Release::<T> }",
    );
    for (name, v) in &levels {
        let mut s = format!("{} {{ base__: {literal}", module_of(name));
        for e in &v.entries {
            write!(s, ", {0}: {0}::<T>", e.name).unwrap();
        }
        s.push_str(" }");
        literal = s;
    }
    writeln!(
        out,
        "\n/// The `{iface}` vtable whose entries call `T`'s methods.\npub const fn vtable<T: {t}>() -> {top} {{\n    {literal}\n}}",
        iface = class.interface,
        t = class.trait_name,
    )
    .unwrap();
    out
}

fn rustfmt(path: &Path) {
    let status = Command::new("rustfmt")
        .args(["--edition", "2024"])
        .arg(path)
        .status()
        .expect("run rustfmt");
    assert!(status.success(), "rustfmt failed on {}", path.display());
}
