//! State inherited by a new list. Queries run only at list boundaries, without GPU copies.
use crate::vtables::{context::real, recorded_context::Command};
use core::ffi::c_void;
use windows::{
    Win32::{
        Foundation::RECT,
        Graphics::{Direct3D::D3D_PRIMITIVE_TOPOLOGY, Direct3D11::*, Dxgi::Common::DXGI_FORMAT},
    },
    core::{BOOL, IUnknown, Interface},
};
fn raw(p: &Option<IUnknown>) -> *mut c_void {
    p.as_ref().map_or(core::ptr::null_mut(), Interface::as_raw)
}
fn out(p: &mut Option<IUnknown>) -> *mut *mut c_void {
    (p as *mut Option<IUnknown>).cast()
}
/// # Safety
/// `c` is a live real device context; getters must be serialized with its setters.
pub(super) unsafe fn snapshot(c: *mut c_void, so: Option<&Command>) -> Vec<Command> {
    let context = unsafe { ID3D11DeviceContext4::from_raw_borrowed(&c) }.expect("live deferred context");
    let uav_count = unsafe { context.GetDevice() }.map_or(8, |d| {
        if unsafe { d.GetFeatureLevel() }.0 >= windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_1.0 {
            64
        } else {
            8
        }
    });
    let mut commands = Vec::new();
    macro_rules! stage {
        ($shader:ident,$set_shader:ident,$cb:ident,$set_cb:ident,$srv:ident,$set_srv:ident,$sampler:ident,$set_sampler:ident) => {{
            let mut shader = None;
            let mut classes = [const { None::<IUnknown> }; 256];
            let mut count = 256;
            unsafe { real::$shader(c, out(&mut shader), classes.as_mut_ptr().cast(), &mut count) };
            commands
                .push(unsafe { Command::$set_shader(raw(&shader), classes.as_ptr().cast(), count.min(256)) });
            let mut buffers = [const { None::<IUnknown> }; 14];
            let mut first = [0; 14];
            let mut counts = [0; 14];
            unsafe {
                real::$cb(
                    c,
                    0,
                    14,
                    buffers.as_mut_ptr().cast(),
                    first.as_mut_ptr(),
                    counts.as_mut_ptr(),
                )
            };
            commands.push(unsafe {
                Command::$set_cb(0, 14, buffers.as_ptr().cast(), first.as_ptr(), counts.as_ptr())
            });
            let mut views = [const { None::<IUnknown> }; 128];
            unsafe { real::$srv(c, 0, 128, views.as_mut_ptr().cast()) };
            commands.push(unsafe { Command::$set_srv(0, 128, views.as_ptr().cast()) });
            let mut samplers = [const { None::<IUnknown> }; 16];
            unsafe { real::$sampler(c, 0, 16, samplers.as_mut_ptr().cast()) };
            commands.push(unsafe { Command::$set_sampler(0, 16, samplers.as_ptr().cast()) });
        }};
    }
    stage!(
        VSGetShader,
        capture_VSSetShader,
        VSGetConstantBuffers1,
        capture_VSSetConstantBuffers1,
        VSGetShaderResources,
        capture_VSSetShaderResources,
        VSGetSamplers,
        capture_VSSetSamplers
    );
    stage!(
        HSGetShader,
        capture_HSSetShader,
        HSGetConstantBuffers1,
        capture_HSSetConstantBuffers1,
        HSGetShaderResources,
        capture_HSSetShaderResources,
        HSGetSamplers,
        capture_HSSetSamplers
    );
    stage!(
        DSGetShader,
        capture_DSSetShader,
        DSGetConstantBuffers1,
        capture_DSSetConstantBuffers1,
        DSGetShaderResources,
        capture_DSSetShaderResources,
        DSGetSamplers,
        capture_DSSetSamplers
    );
    stage!(
        GSGetShader,
        capture_GSSetShader,
        GSGetConstantBuffers1,
        capture_GSSetConstantBuffers1,
        GSGetShaderResources,
        capture_GSSetShaderResources,
        GSGetSamplers,
        capture_GSSetSamplers
    );
    stage!(
        PSGetShader,
        capture_PSSetShader,
        PSGetConstantBuffers1,
        capture_PSSetConstantBuffers1,
        PSGetShaderResources,
        capture_PSSetShaderResources,
        PSGetSamplers,
        capture_PSSetSamplers
    );
    stage!(
        CSGetShader,
        capture_CSSetShader,
        CSGetConstantBuffers1,
        capture_CSSetConstantBuffers1,
        CSGetShaderResources,
        capture_CSSetShaderResources,
        CSGetSamplers,
        capture_CSSetSamplers
    );
    unsafe {
        let mut layout = None;
        real::IAGetInputLayout(c, out(&mut layout));
        commands.push(Command::capture_IASetInputLayout(raw(&layout)));
        let mut buffers = [const { None::<IUnknown> }; 32];
        let mut strides = [0; 32];
        let mut offsets = [0; 32];
        real::IAGetVertexBuffers(
            c,
            0,
            32,
            buffers.as_mut_ptr().cast(),
            strides.as_mut_ptr(),
            offsets.as_mut_ptr(),
        );
        commands.push(Command::capture_IASetVertexBuffers(
            0,
            32,
            buffers.as_ptr().cast(),
            strides.as_ptr(),
            offsets.as_ptr(),
        ));
        let mut index = None;
        let mut format = DXGI_FORMAT::default();
        let mut offset = 0;
        real::IAGetIndexBuffer(c, out(&mut index), &mut format, &mut offset);
        commands.push(Command::capture_IASetIndexBuffer(raw(&index), format, offset));
        let mut topology = D3D_PRIMITIVE_TOPOLOGY::default();
        real::IAGetPrimitiveTopology(c, &mut topology);
        commands.push(Command::capture_IASetPrimitiveTopology(topology));
        let mut rs = None;
        real::RSGetState(c, out(&mut rs));
        commands.push(Command::capture_RSSetState(raw(&rs)));
        let mut viewports = [D3D11_VIEWPORT::default(); 16];
        let mut count = 16;
        real::RSGetViewports(c, &mut count, viewports.as_mut_ptr());
        commands.push(Command::capture_RSSetViewports(count.min(16), viewports.as_ptr()));
        let mut rects = [RECT::default(); 16];
        let mut count = 16;
        real::RSGetScissorRects(c, &mut count, rects.as_mut_ptr());
        commands.push(Command::capture_RSSetScissorRects(count.min(16), rects.as_ptr()));
        let mut blend = None;
        let mut factors = [0.; 4];
        let mut mask = 0;
        real::OMGetBlendState(c, out(&mut blend), factors.as_mut_ptr(), &mut mask);
        commands.push(Command::capture_OMSetBlendState(
            raw(&blend),
            factors.as_ptr(),
            mask,
        ));
        let mut depth = None;
        let mut stencil = 0;
        real::OMGetDepthStencilState(c, out(&mut depth), &mut stencil);
        commands.push(Command::capture_OMSetDepthStencilState(raw(&depth), stencil));
        let mut rtvs = [const { None::<IUnknown> }; 8];
        let mut dsv = None;
        real::OMGetRenderTargets(c, 8, rtvs.as_mut_ptr().cast(), out(&mut dsv));
        let num_rtvs = rtvs.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
        let mut uavs = [const { None::<IUnknown> }; 64];
        real::OMGetRenderTargetsAndUnorderedAccessViews(
            c,
            0,
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            0,
            uav_count,
            uavs.as_mut_ptr().cast(),
        );
        let counts = [u32::MAX; 64];
        commands.push(Command::capture_OMSetRenderTargetsAndUnorderedAccessViews(
            num_rtvs as u32,
            rtvs.as_ptr().cast(),
            raw(&dsv),
            num_rtvs as u32,
            uav_count - num_rtvs as u32,
            uavs[num_rtvs..].as_ptr().cast(),
            counts.as_ptr(),
        ));
        let mut uavs = [const { None::<IUnknown> }; 64];
        real::CSGetUnorderedAccessViews(c, 0, uav_count, uavs.as_mut_ptr().cast());
        commands.push(Command::capture_CSSetUnorderedAccessViews(
            0,
            uav_count,
            uavs.as_ptr().cast(),
            counts.as_ptr(),
        ));
        let mut predicate = None;
        let mut value = BOOL::default();
        real::GetPredication(c, out(&mut predicate), &mut value);
        commands.push(Command::capture_SetPredication(raw(&predicate), value));
    }
    if let Some(so) = so {
        commands.push(so.clone());
    }
    commands
}
