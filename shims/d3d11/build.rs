//! Forward d3d11.dll's exports to locum.dll, with the system DLL's ordinals (the list DXVK exports).

include!("../forward.rs");

fn main() {
    forward(
        "d3d11",
        &[
            ("D3D11CoreCreateDevice", 18),
            ("D3D11CreateDevice", 22),
            ("D3D11CreateDeviceAndSwapChain", 23),
            ("D3D11On12CreateDevice", 24),
        ],
    );
}
