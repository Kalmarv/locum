//! Forward d3d11.dll's exports to locum.dll, with the ordinals of the system DLL (the same list DXVK exports).

fn main() {
    for (name, ordinal) in [
        ("D3D11CoreCreateDevice", 18),
        ("D3D11CreateDevice", 22),
        ("D3D11CreateDeviceAndSwapChain", 23),
        ("D3D11On12CreateDevice", 24),
    ] {
        println!("cargo:rustc-cdylib-link-arg=/EXPORT:{name}=locum.{name},@{ordinal}");
    }
}
