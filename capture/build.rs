//! Compiles the entry points of `src/samples/samples.hlsl` (shader model 5.0, for Direct3D 11 and 12) with the
//! system's `d3dcompiler_47.dll` into `OUT_DIR`, where `samples.rs` includes them. A host that is not Windows
//! (docs.rs) gets empty programs: documentation runs none.

/// Each program: its file, entry point and profile, and whether it reads a multisampled texture (`MS`).
const PROGRAMS: &[(&str, &str, &str, bool)] = &[
    ("root", "ROOT", "rootsig_1_0", true),
    ("vs", "vs", "vs_5_0", true),
    ("read_bits", "read_bits", "ps_5_0", true),
    ("read_floats", "read_floats", "ps_5_0", true),
    ("read_d24s8", "read_d24s8", "ps_5_0", true),
    ("read_d32s8", "read_d32s8", "ps_5_0", true),
    ("read_bits_one", "read_bits", "ps_5_0", false),
    ("read_floats_one", "read_floats", "ps_5_0", false),
    ("read_d24s8_one", "read_d24s8", "ps_5_0", false),
    ("read_d32s8_one", "read_d32s8", "ps_5_0", false),
    ("write_bits", "write_bits", "ps_5_0", true),
    ("write_floats", "write_floats", "ps_5_0", true),
    ("write_d24", "write_d24", "ps_5_0", true),
    ("write_d32", "write_d32", "ps_5_0", true),
    ("write_stencil24", "write_stencil24", "ps_5_0", true),
    ("write_stencil32", "write_stencil32", "ps_5_0", true),
];

fn main() {
    let source_path = "src/samples/samples.hlsl";
    println!("cargo:rerun-if-changed={source_path}");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let source = std::fs::read(source_path).expect("samples.hlsl");
    for &(file, entry, profile, multisampled) in PROGRAMS {
        let bytes = compile(&source, entry, profile, multisampled);
        std::fs::write(out.join(format!("{file}.dxbc")), bytes).expect("write a program");
    }
}

#[cfg(windows)]
fn compile(source: &[u8], entry: &str, profile: &str, multisampled: bool) -> Vec<u8> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::{D3D_SHADER_MACRO, ID3DBlob};
    use windows::core::PCSTR;
    let c = |s: &str| std::ffi::CString::new(s).expect("no NUL");
    let (entry_c, profile_c, ms) = (c(entry), c(profile), c(if multisampled { "1" } else { "0" }));
    let name = c("MS");
    let defines = [
        D3D_SHADER_MACRO {
            Name: PCSTR(name.as_ptr().cast()),
            Definition: PCSTR(ms.as_ptr().cast()),
        },
        D3D_SHADER_MACRO::default(),
    ];
    let (mut code, mut errors): (Option<ID3DBlob>, Option<ID3DBlob>) = (None, None);
    let blob = |b: &ID3DBlob| unsafe {
        core::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()).to_vec()
    };
    let result = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR(c"samples.hlsl".as_ptr().cast()),
            Some(defines.as_ptr()),
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
        panic!("samples.hlsl {entry} ({profile}): {e}: {said}");
    }
    blob(&code.expect("compiled code"))
}

#[cfg(not(windows))]
fn compile(_: &[u8], _: &str, _: &str, _: bool) -> Vec<u8> {
    Vec::new()
}
