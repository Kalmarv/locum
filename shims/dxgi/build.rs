//! Forward dxgi.dll's exports to locum.dll, with the system DLL's ordinals (the list DXVK exports).

include!("../forward.rs");

fn main() {
    forward("dxgi", &[("CreateDXGIFactory", 9), ("CreateDXGIFactory1", 10), ("CreateDXGIFactory2", 11), ("DXGIDeclareAdapterRemovalSupport", 16), ("DXGIGetDebugInterface1", 17)]);
}
