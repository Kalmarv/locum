//! Forward dxgi.dll's exports to locum.dll: every name the system DLL exports, with the ordinals of Windows 11's
//! (build 26200). Ordinals differ between Windows builds, so callers find these by name. The process finds ours for
//! whatever it asks of dxgi.dll, as ours is loaded under that name: an export left out is one nothing can find (the
//! Direct3D 11 debug layer imports D3D11CoreRegisterLayers and its kin). Those locum does not intercept pass through
//! (`locum/src/passthrough.rs`).

include!("../forward.rs");

fn main() {
    forward(
        "dxgi",
        &[
            ("ApplyCompatResolutionQuirking", 1),
            ("CompatString", 2),
            ("CompatValue", 3),
            ("DXGIDumpJournal", 4),
            ("PIXBeginCapture", 5),
            ("PIXEndCapture", 6),
            ("PIXGetCaptureState", 7),
            ("SetAppCompatStringPointer=DxgiSetAppCompatStringPointer", 8),
            ("UpdateHMDEmulationStatus", 9),
            ("CreateDXGIFactory", 10),
            ("CreateDXGIFactory1", 11),
            ("CreateDXGIFactory2", 12),
            ("DXGID3D10CreateDevice", 13),
            ("DXGID3D10CreateLayeredDevice", 14),
            ("DXGID3D10GetLayeredDeviceSize", 15),
            ("DXGID3D10RegisterLayers", 16),
            ("DXGIDeclareAdapterRemovalSupport", 17),
            ("DXGIDisableVBlankVirtualization", 18),
            ("DXGIGetDebugInterface1", 19),
            ("DXGIReportAdapterConfiguration", 20),
        ],
    );
}
