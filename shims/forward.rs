// Shared by the shims' build scripts (`include!`d): makes the DLL's export table one of forwarders to locum.dll.
// Each export forwards to the same name in locum.dll, or to the name after `=` (`Name=LocumName`) where locum.dll
// exports that name for another DLL.
//
// The forwarders are written as a module-definition file and turned into an export file (`.exp`) by `lib.exe`,
// which the link then takes as an input. A forwarder given straight to the linker (`/DEF` with `name=locum.name`,
// or `/EXPORT:name=locum.name`) fails with LNK2001 "unresolved external symbol name"; the `.exp` route works.

fn forward(dll: &str, exports: &[(&str, u32)]) {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    let def = out.join(format!("{dll}.def"));
    let mut text = format!("LIBRARY {dll}\nEXPORTS\n");
    for (export, ordinal) in exports {
        let (name, target) = export.split_once('=').unwrap_or((export, export));
        let noname = if name == "D3D12Ordinal99" { " NONAME" } else { "" };
        text.push_str(&format!("    {name}=locum.{target} @{ordinal}{noname}\n"));
    }
    std::fs::write(&def, text).expect("write the .def");
    let target = std::env::var("TARGET").expect("TARGET");
    let mut lib = find_msvc_tools::find(&target, "lib.exe").expect("lib.exe (MSVC build tools)");
    let status = lib
        .arg("/nologo")
        .arg(format!("/def:{}", def.display()))
        .arg(format!("/out:{}", out.join(format!("{dll}-forward.lib")).display()))
        .arg("/machine:x64")
        .status()
        .expect("run lib.exe");
    assert!(status.success(), "lib.exe failed");
    println!("cargo:rustc-cdylib-link-arg={}", out.join(format!("{dll}-forward.exp")).display());
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../forward.rs");
}
