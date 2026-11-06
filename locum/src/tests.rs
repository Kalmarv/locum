//! The exports against the system's own: whatever a call hands back through locum, the same call to the real runtime
//! hands back too (a result, which out pointers hold something, which interfaces answer).
use super::*;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::D3D11_SDK_VERSION;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_SWAP_CHAIN_DESC, DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIFactory, IDXGIFactory1, IDXGIFactory2,
    IDXGIFactory3, IDXGIFactory4, IDXGIFactory5, IDXGIFactory7, IDXGIObject,
};

const JUNK: *mut c_void = 0xdead_beef_usize as *mut c_void;

/// What one out pointer held after a call: untouched (the junk it was given), null, or an object (then released).
#[derive(Debug, PartialEq)]
enum Out {
    Untouched,
    Null,
    Object,
}

fn out(p: *mut c_void) -> Out {
    if p == JUNK {
        Out::Untouched
    } else if p.is_null() {
        Out::Null
    } else {
        drop(unsafe { IUnknown::from_raw(p) });
        Out::Object
    }
}

/// `D3D11CreateDeviceAndSwapChain` given junk in the out pointers `want` names (swap chain, device, feature level,
/// context) and null for the others.
fn and_swapchain(
    create: CreateDeviceAndSwapChainFn,
    driver: D3D_DRIVER_TYPE,
    desc: Option<&DXGI_SWAP_CHAIN_DESC>,
    want: [bool; 4],
) -> (HRESULT, [Out; 3], Option<i32>) {
    let (mut swapchain, mut device, mut context) = (JUNK, JUNK, JUNK);
    let mut level = D3D_FEATURE_LEVEL(0xdead);
    let hr = unsafe {
        create(
            core::ptr::null_mut(),
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            core::ptr::null(),
            0,
            D3D11_SDK_VERSION,
            desc.map_or(core::ptr::null(), |d| d as *const _),
            if want[0] {
                &mut swapchain
            } else {
                core::ptr::null_mut()
            },
            if want[1] {
                &mut device
            } else {
                core::ptr::null_mut()
            },
            if want[2] {
                &mut level
            } else {
                core::ptr::null_mut()
            },
            if want[3] {
                &mut context
            } else {
                core::ptr::null_mut()
            },
        )
    };
    let outs = [out(swapchain), out(device), out(context)];
    (hr, outs, want[2].then_some(level.0))
}

fn device(create: CreateDeviceFn, driver: D3D_DRIVER_TYPE, want: [bool; 2]) -> (HRESULT, [Out; 2]) {
    let (mut device, mut context) = (JUNK, JUNK);
    let hr = unsafe {
        create(
            core::ptr::null_mut(),
            driver,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            core::ptr::null(),
            0,
            D3D11_SDK_VERSION,
            if want[0] {
                &mut device
            } else {
                core::ptr::null_mut()
            },
            core::ptr::null_mut(),
            if want[1] {
                &mut context
            } else {
                core::ptr::null_mut()
            },
        )
    };
    (hr, [out(device), out(context)])
}

#[test]
fn device_creation_hands_back_what_the_runtime_does() {
    let system_create = unsafe { export::<CreateDeviceFn>(system().d3d11, c"D3D11CreateDevice") }.unwrap();
    let system_and_swapchain =
        unsafe { export::<CreateDeviceAndSwapChainFn>(system().d3d11, c"D3D11CreateDeviceAndSwapChain") }
            .unwrap();
    for driver in [D3D_DRIVER_TYPE_UNKNOWN, D3D_DRIVER_TYPE_WARP] {
        for want in [[true, true], [true, false], [false, true], [false, false]] {
            assert_eq!(
                device(D3D11CreateDevice, driver, want),
                device(system_create, driver, want),
                "D3D11CreateDevice with driver {driver:?}, out pointers {want:?}"
            );
        }
        // A description with no window: the runtime makes a device for it, but no swap chain.
        let windowless = DXGI_SWAP_CHAIN_DESC {
            BufferDesc: DXGI_MODE_DESC {
                Width: 64,
                Height: 64,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                ..Default::default()
            },
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 1,
            Windowed: true.into(),
            ..Default::default()
        };
        for desc in [None, Some(&windowless)] {
            for bits in 0..16u8 {
                let want = [0, 1, 2, 3].map(|i| bits & (1 << i) != 0);
                assert_eq!(
                    and_swapchain(D3D11CreateDeviceAndSwapChain, driver, desc, want),
                    and_swapchain(system_and_swapchain, driver, desc, want),
                    "D3D11CreateDeviceAndSwapChain with driver {driver:?}, a description {}, out pointers {want:?}",
                    desc.is_some()
                );
            }
        }
    }
}

const FACTORIES: [GUID; 8] = [
    IDXGIObject::IID,
    IDXGIFactory::IID,
    IDXGIFactory1::IID,
    IDXGIFactory2::IID,
    IDXGIFactory3::IID,
    IDXGIFactory4::IID,
    IDXGIFactory5::IID,
    IDXGIFactory7::IID,
];

/// Which of [`FACTORIES`] the factory one of the three entry points makes for `iid` answers to.
fn answers(entry: u8, system: bool, iid: &GUID) -> Option<Vec<bool>> {
    let dxgi = crate::system::system().dxgi;
    let mut factory = core::ptr::null_mut();
    let hr = unsafe {
        match (entry, system) {
            (0, false) => CreateDXGIFactory(iid, &mut factory),
            (1, false) => CreateDXGIFactory1(iid, &mut factory),
            (_, false) => CreateDXGIFactory2(0, iid, &mut factory),
            (0, true) => export::<CreateFactoryFn>(dxgi, c"CreateDXGIFactory").unwrap()(iid, &mut factory),
            (1, true) => export::<CreateFactoryFn>(dxgi, c"CreateDXGIFactory1").unwrap()(iid, &mut factory),
            (_, true) => {
                export::<CreateFactory2Fn>(dxgi, c"CreateDXGIFactory2").unwrap()(0, iid, &mut factory)
            }
        }
    };
    if hr.is_err() {
        return None;
    }
    let factory = unsafe { IUnknown::from_raw(factory) };
    Some(
        FACTORIES
            .iter()
            .map(|wanted| {
                let mut face = core::ptr::null_mut();
                let hr = unsafe { factory.query(wanted, &mut face) };
                if hr.is_ok() {
                    drop(unsafe { IUnknown::from_raw(face) });
                }
                hr.is_ok()
            })
            .collect(),
    )
}

#[test]
fn a_factory_answers_to_what_the_systems_does() {
    for entry in 0..3 {
        for iid in &FACTORIES {
            assert_eq!(
                answers(entry, false, iid),
                answers(entry, true, iid),
                "factory {entry} made for {}",
                crate::com::name_of(iid)
            );
        }
    }
}
