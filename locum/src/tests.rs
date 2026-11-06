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

/// The count `AddRef` gives back, the reference it took given back.
unsafe fn count(object: *mut c_void) -> u32 {
    let n = unsafe { crate::faces::add_ref(object) };
    unsafe { crate::faces::release(object) };
    n
}

/// What a game sees of a WARP device `create` makes and its family: the counts a child, the immediate context and
/// the way through DXGI to the factory leave on the device; whether each way back (a child's `GetDevice`, a
/// resource's DXGI `GetDevice`, the factory found twice) answers with what the game already holds; and the factory's
/// table, to tell whose it is.
fn family(create: CreateDeviceFn) -> (Vec<u32>, *const c_void) {
    use windows::Win32::Graphics::Direct3D11::*;
    use windows::Win32::Graphics::Dxgi::{IDXGIDevice, IDXGIFactory1, IDXGIResource};
    let mut seen = Vec::new();
    let table;
    unsafe {
        let mut raw = core::ptr::null_mut();
        create(
            core::ptr::null_mut(),
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            core::ptr::null(),
            0,
            D3D11_SDK_VERSION,
            &mut raw,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
        .ok()
        .unwrap();
        let device = ID3D11Device::from_raw(raw);
        seen.push(count(device.as_raw()));
        let mut buffer = None;
        device
            .CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: 16,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut buffer),
            )
            .unwrap();
        let buffer = buffer.unwrap();
        seen.push(count(device.as_raw()));
        // Without an out pointer a create only checks its arguments: S_FALSE, and nothing made.
        let checked = (Interface::vtable(&device).CreateBuffer)(
            device.as_raw(),
            &D3D11_BUFFER_DESC {
                ByteWidth: 16,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            },
            core::ptr::null(),
            core::ptr::null_mut(),
        );
        seen.push(checked.0 as u32);
        seen.push(count(device.as_raw()));
        let parent = buffer.GetDevice().unwrap();
        seen.push((parent.as_raw() == device.as_raw()) as u32);
        drop(parent);
        let resource: IDXGIResource = buffer.cast().unwrap();
        let parent: ID3D11Device = resource.GetDevice().unwrap();
        seen.push((parent.as_raw() == device.as_raw()) as u32);
        drop(parent);
        drop(resource);
        let context = device.GetImmediateContext().unwrap();
        seen.push(count(device.as_raw()));
        seen.push(count(context.as_raw()));
        let again = device.GetImmediateContext().unwrap();
        seen.push((again.as_raw() == context.as_raw()) as u32);
        seen.push(count(context.as_raw()));
        let parent = again.GetDevice().unwrap();
        seen.push((parent.as_raw() == device.as_raw()) as u32);
        drop((parent, again, context));
        seen.push(count(device.as_raw()));
        let factory = |device: &ID3D11Device| {
            let adapter = device.cast::<IDXGIDevice>().unwrap().GetAdapter().unwrap();
            adapter.GetParent::<IDXGIFactory1>().unwrap()
        };
        let first = factory(&device);
        let second = factory(&device);
        seen.push((first.as_raw() == second.as_raw()) as u32);
        seen.push(count(first.as_raw()));
        table = *(first.as_raw() as *const *const c_void);
        drop((first, second));
        drop(buffer);
        seen.push(count(device.as_raw()));
        seen.push(crate::faces::release(device.into_raw()));
    }
    crate::faces::bury();
    (seen, table)
}

/// Through locum, a device's family counts and answers as the runtime's does, and its way to the factory ends at
/// locum's factory.
#[test]
fn a_device_family_is_the_runtimes() {
    let system_create = unsafe { export::<CreateDeviceFn>(system().d3d11, c"D3D11CreateDevice") }.unwrap();
    let (native, _) = family(system_create);
    let (ours, table) = family(D3D11CreateDevice);
    assert_eq!(ours, native, "through locum, against the runtime alone");
    assert_eq!(
        crate::system::module_of(table),
        crate::system::module_of(family as *const c_void),
        "the factory found from the device is locum's"
    );
}

/// The first output of the first adapter of a factory `create` makes, with that adapter; `None` on a machine with no
/// display.
fn first_output(
    create: CreateFactoryFn,
) -> Option<(
    windows::Win32::Graphics::Dxgi::IDXGIAdapter,
    windows::Win32::Graphics::Dxgi::IDXGIOutput,
)> {
    use windows::Win32::Graphics::Dxgi::IDXGIFactory1;
    let mut raw = core::ptr::null_mut();
    unsafe { create(&IDXGIFactory1::IID, &mut raw) }.ok().unwrap();
    let factory = unsafe { IDXGIFactory1::from_raw(raw) };
    let adapter = unsafe { factory.EnumAdapters(0) }.unwrap();
    let output = unsafe { adapter.EnumOutputs(0) }.ok()?;
    Some((adapter, output))
}

/// An output locum hands out gives locum's adapter back; one of the runtime's own, whose table locum changed for
/// it, still gives the runtime's adapter.
#[test]
fn an_output_gives_back_its_own_adapter() {
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
    let Some((ours, output)) = first_output(CreateDXGIFactory1) else {
        eprintln!("no display: nothing to check");
        return;
    };
    let parent: IDXGIAdapter = unsafe { output.GetParent() }.unwrap();
    assert_eq!(parent.as_raw(), ours.as_raw(), "locum's output, locum's adapter");
    let system_create = unsafe { export::<CreateFactoryFn>(system().dxgi, c"CreateDXGIFactory1") }.unwrap();
    let (theirs, output) = first_output(system_create).unwrap();
    assert_eq!(
        unsafe { *(output.as_raw() as *const *const c_void) },
        unsafe { *(first_output(CreateDXGIFactory1).unwrap().1.as_raw() as *const *const c_void) },
        "the two outputs share the runtime's table"
    );
    let parent: IDXGIAdapter = unsafe { output.GetParent() }.unwrap();
    assert_eq!(
        parent.as_raw(),
        theirs.as_raw(),
        "the runtime's output, the runtime's adapter"
    );
    drop((parent, output, theirs, ours));
    crate::faces::bury();
}

/// The counts a WARP device `create` makes is left with through deferred contexts and their command lists: one
/// recorded with state kept and one without, run on the immediate context, and one recorded on a second deferred
/// context and run on the first.
fn deferred_counts(create: CreateDeviceFn) -> Vec<u32> {
    use windows::Win32::Graphics::Direct3D11::*;
    let mut seen = Vec::new();
    unsafe {
        let mut raw = core::ptr::null_mut();
        create(
            core::ptr::null_mut(),
            D3D_DRIVER_TYPE_WARP,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            core::ptr::null(),
            0,
            D3D11_SDK_VERSION,
            &mut raw,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
        .ok()
        .unwrap();
        let device = ID3D11Device::from_raw(raw);
        let mut buffer = None;
        device
            .CreateBuffer(
                &D3D11_BUFFER_DESC {
                    ByteWidth: 16,
                    BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                    ..Default::default()
                },
                None,
                Some(&mut buffer),
            )
            .unwrap();
        let immediate = device.GetImmediateContext().unwrap();
        seen.push(count(device.as_raw()));
        let mut deferred = None;
        device.CreateDeferredContext(0, Some(&mut deferred)).unwrap();
        let deferred = deferred.unwrap();
        seen.push(count(device.as_raw()));
        deferred.PSSetConstantBuffers(0, Some(&[buffer.clone()]));
        let (mut kept, mut cleared) = (None, None);
        deferred.FinishCommandList(true, Some(&mut kept)).unwrap();
        deferred.FinishCommandList(false, Some(&mut cleared)).unwrap();
        seen.push(count(device.as_raw()));
        immediate.ExecuteCommandList(kept.as_ref(), true);
        immediate.ExecuteCommandList(kept.as_ref(), false);
        drop((kept, cleared));
        seen.push(count(device.as_raw()));
        let mut second = None;
        device.CreateDeferredContext(0, Some(&mut second)).unwrap();
        let second = second.unwrap();
        second.PSSetConstantBuffers(0, Some(&[buffer.clone()]));
        let mut list = None;
        second.FinishCommandList(false, Some(&mut list)).unwrap();
        deferred.ExecuteCommandList(list.as_ref(), true);
        deferred.ExecuteCommandList(list.as_ref(), false);
        drop((list, second));
        seen.push(count(device.as_raw()));
        drop(deferred);
        seen.push(count(device.as_raw()));
        drop((buffer, immediate));
        seen.push(count(device.as_raw()));
        seen.push(crate::faces::release(device.into_raw()));
    }
    crate::faces::bury();
    seen
}

/// Through locum, deferred contexts and their command lists leave the device the counts the runtime's do.
#[test]
fn deferred_contexts_count_as_the_runtimes() {
    let system_create = unsafe { export::<CreateDeviceFn>(system().d3d11, c"D3D11CreateDevice") }.unwrap();
    assert_eq!(deferred_counts(D3D11CreateDevice), deferred_counts(system_create));
}
