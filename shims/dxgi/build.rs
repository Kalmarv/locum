//! Forward dxgi.dll's exports to locum.dll, with the ordinals of the system DLL (the same list DXVK exports).

fn main() {
    for (name, ordinal) in [
        ("CreateDXGIFactory", 9),
        ("CreateDXGIFactory1", 10),
        ("CreateDXGIFactory2", 11),
        ("DXGIDeclareAdapterRemovalSupport", 16),
        ("DXGIGetDebugInterface1", 17),
    ] {
        println!("cargo:rustc-cdylib-link-arg=/EXPORT:{name}=locum.{name},@{ordinal}");
    }
}
