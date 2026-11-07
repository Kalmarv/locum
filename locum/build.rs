//! Compiles the entry points of `src/d3d12/gather.hlsl` (shader model 5.0, for Direct3D 12) with the system's
//! `d3dcompiler_47.dll` into `OUT_DIR`, where `d3d12/gather.rs` includes them. A host that is not Windows (docs.rs)
//! gets empty programs: documentation runs none.

/// Each program: its file, entry point and profile.
const PROGRAMS: &[(&str, &str, &str)] = &[
    ("gather_root", "ROOT", "rootsig_1_0"),
    ("gather_records", "records", "cs_5_0"),
    ("gather_copy", "copy", "cs_5_0"),
];

fn main() {
    let source_path = "src/d3d12/gather.hlsl";
    println!("cargo:rerun-if-changed={source_path}");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let source = std::fs::read(source_path).expect("gather.hlsl");
    for &(file, entry, profile) in PROGRAMS {
        let bytes = compile(&source, entry, profile);
        std::fs::write(out.join(format!("{file}.dxbc")), bytes).expect("write a program");
    }
}

#[cfg(windows)]
fn compile(source: &[u8], entry: &str, profile: &str) -> Vec<u8> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    use windows::core::PCSTR;
    let c = |s: &str| std::ffi::CString::new(s).expect("no NUL");
    let (entry_c, profile_c) = (c(entry), c(profile));
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    let blob = |b: &ID3DBlob| unsafe {
        core::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()).to_vec()
    };
    let result = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(c"gather.hlsl".as_ptr().cast()),
            None,
            None,
            PCSTR(entry_c.as_ptr().cast()),
            PCSTR(profile_c.as_ptr().cast()),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        let said = errors
            .map(|e| String::from_utf8_lossy(&blob(&e)).into_owned())
            .unwrap_or_default();
        panic!("gather.hlsl {entry} ({profile}): {e}: {said}");
    }
    blob(&code.expect("compiled code"))
}

#[cfg(not(windows))]
fn compile(_: &[u8], _: &str, _: &str) -> Vec<u8> {
    Vec::new()
}
