//! Generated owned context commands by `cargo xtask gen`; do not edit.
#![allow(
    non_snake_case,
    clippy::too_many_arguments,
    clippy::missing_safety_doc,
    dead_code
)]
use crate::d3d11::recorder::{Array, Object, Objects};
use core::ffi::c_void;
#[derive(Clone)]
pub struct VSSetConstantBuffers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct PSSetShaderResources {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct PSSetShader {
    a0: Object,
    a1: Objects,
    a2: u32,
}
#[derive(Clone)]
pub struct PSSetSamplers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct VSSetShader {
    a0: Object,
    a1: Objects,
    a2: u32,
}
#[derive(Clone)]
pub struct DrawIndexed {
    a0: u32,
    a1: u32,
    a2: i32,
}
#[derive(Clone)]
pub struct Draw {
    a0: u32,
    a1: u32,
}
#[derive(Clone)]
pub struct PSSetConstantBuffers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct IASetInputLayout {
    a0: Object,
}
#[derive(Clone)]
pub struct IASetVertexBuffers {
    a0: u32,
    a1: u32,
    a2: Objects,
    a3: Array<u32>,
    a4: Array<u32>,
}
#[derive(Clone)]
pub struct IASetIndexBuffer {
    a0: Object,
    a1: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    a2: u32,
}
#[derive(Clone)]
pub struct DrawIndexedInstanced {
    a0: u32,
    a1: u32,
    a2: u32,
    a3: i32,
    a4: u32,
}
#[derive(Clone)]
pub struct DrawInstanced {
    a0: u32,
    a1: u32,
    a2: u32,
    a3: u32,
}
#[derive(Clone)]
pub struct GSSetConstantBuffers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct GSSetShader {
    a0: Object,
    a1: Objects,
    a2: u32,
}
#[derive(Clone)]
pub struct IASetPrimitiveTopology {
    a0: windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY,
}
#[derive(Clone)]
pub struct VSSetShaderResources {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct VSSetSamplers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct Begin {
    a0: Object,
}
#[derive(Clone)]
pub struct End {
    a0: Object,
}
#[derive(Clone)]
pub struct SetPredication {
    a0: Object,
    a1: windows::core::BOOL,
}
#[derive(Clone)]
pub struct GSSetShaderResources {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct GSSetSamplers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct OMSetRenderTargets {
    a0: u32,
    a1: Objects,
    a2: Object,
}
#[derive(Clone)]
pub struct OMSetRenderTargetsAndUnorderedAccessViews {
    a0: u32,
    a1: Objects,
    a2: Object,
    a3: u32,
    a4: u32,
    a5: Objects,
    a6: Array<u32>,
}
#[derive(Clone)]
pub struct OMSetBlendState {
    a0: Object,
    a1: Array<f32>,
    a2: u32,
}
#[derive(Clone)]
pub struct OMSetDepthStencilState {
    a0: Object,
    a1: u32,
}
#[derive(Clone)]
pub struct SOSetTargets {
    a0: u32,
    a1: Objects,
    a2: Array<u32>,
}
#[derive(Clone)]
pub struct DrawAuto {}
#[derive(Clone)]
pub struct DrawIndexedInstancedIndirect {
    a0: Object,
    a1: u32,
}
#[derive(Clone)]
pub struct DrawInstancedIndirect {
    a0: Object,
    a1: u32,
}
#[derive(Clone)]
pub struct Dispatch {
    a0: u32,
    a1: u32,
    a2: u32,
}
#[derive(Clone)]
pub struct DispatchIndirect {
    a0: Object,
    a1: u32,
}
#[derive(Clone)]
pub struct RSSetState {
    a0: Object,
}
#[derive(Clone)]
pub struct RSSetViewports {
    a0: u32,
    a1: Array<windows::Win32::Graphics::Direct3D11::D3D11_VIEWPORT>,
}
#[derive(Clone)]
pub struct RSSetScissorRects {
    a0: u32,
    a1: Array<windows::Win32::Foundation::RECT>,
}
#[derive(Clone)]
pub struct CopySubresourceRegion {
    a0: Object,
    a1: u32,
    a2: u32,
    a3: u32,
    a4: u32,
    a5: Object,
    a6: u32,
    a7: Array<windows::Win32::Graphics::Direct3D11::D3D11_BOX>,
}
#[derive(Clone)]
pub struct CopyResource {
    a0: Object,
    a1: Object,
}
#[derive(Clone)]
pub struct CopyStructureCount {
    a0: Object,
    a1: u32,
    a2: Object,
}
#[derive(Clone)]
pub struct ClearRenderTargetView {
    a0: Object,
    a1: Array<f32>,
}
#[derive(Clone)]
pub struct ClearUnorderedAccessViewUint {
    a0: Object,
    a1: Array<u32>,
}
#[derive(Clone)]
pub struct ClearUnorderedAccessViewFloat {
    a0: Object,
    a1: Array<f32>,
}
#[derive(Clone)]
pub struct ClearDepthStencilView {
    a0: Object,
    a1: u32,
    a2: f32,
    a3: u8,
}
#[derive(Clone)]
pub struct GenerateMips {
    a0: Object,
}
#[derive(Clone)]
pub struct SetResourceMinLOD {
    a0: Object,
    a1: f32,
}
#[derive(Clone)]
pub struct ResolveSubresource {
    a0: Object,
    a1: u32,
    a2: Object,
    a3: u32,
    a4: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
}
#[derive(Clone)]
pub struct ExecuteCommandList {
    a0: Object,
    a1: windows::core::BOOL,
}
#[derive(Clone)]
pub struct HSSetShaderResources {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct HSSetShader {
    a0: Object,
    a1: Objects,
    a2: u32,
}
#[derive(Clone)]
pub struct HSSetSamplers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct HSSetConstantBuffers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct DSSetShaderResources {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct DSSetShader {
    a0: Object,
    a1: Objects,
    a2: u32,
}
#[derive(Clone)]
pub struct DSSetSamplers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct DSSetConstantBuffers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct CSSetShaderResources {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct CSSetUnorderedAccessViews {
    a0: u32,
    a1: u32,
    a2: Objects,
    a3: Array<u32>,
}
#[derive(Clone)]
pub struct CSSetShader {
    a0: Object,
    a1: Objects,
    a2: u32,
}
#[derive(Clone)]
pub struct CSSetSamplers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct CSSetConstantBuffers {
    a0: u32,
    a1: u32,
    a2: Objects,
}
#[derive(Clone)]
pub struct ClearState {}
#[derive(Clone)]
pub struct CopySubresourceRegion1 {
    a0: Object,
    a1: u32,
    a2: u32,
    a3: u32,
    a4: u32,
    a5: Object,
    a6: u32,
    a7: Array<windows::Win32::Graphics::Direct3D11::D3D11_BOX>,
    a8: u32,
}
#[derive(Clone)]
pub struct DiscardResource {
    a0: Object,
}
#[derive(Clone)]
pub struct DiscardView {
    a0: Object,
}
#[derive(Clone)]
pub struct VSSetConstantBuffers1 {
    a0: u32,
    a1: u32,
    a2: Objects,
    a3: Array<u32>,
    a4: Array<u32>,
}
#[derive(Clone)]
pub struct HSSetConstantBuffers1 {
    a0: u32,
    a1: u32,
    a2: Objects,
    a3: Array<u32>,
    a4: Array<u32>,
}
#[derive(Clone)]
pub struct DSSetConstantBuffers1 {
    a0: u32,
    a1: u32,
    a2: Objects,
    a3: Array<u32>,
    a4: Array<u32>,
}
#[derive(Clone)]
pub struct GSSetConstantBuffers1 {
    a0: u32,
    a1: u32,
    a2: Objects,
    a3: Array<u32>,
    a4: Array<u32>,
}
#[derive(Clone)]
pub struct PSSetConstantBuffers1 {
    a0: u32,
    a1: u32,
    a2: Objects,
    a3: Array<u32>,
    a4: Array<u32>,
}
#[derive(Clone)]
pub struct CSSetConstantBuffers1 {
    a0: u32,
    a1: u32,
    a2: Objects,
    a3: Array<u32>,
    a4: Array<u32>,
}
#[derive(Clone)]
pub struct ClearView {
    a0: Object,
    a1: Array<f32>,
    a2: Array<windows::Win32::Foundation::RECT>,
    a3: u32,
}
#[derive(Clone)]
pub struct DiscardView1 {
    a0: Object,
    a1: Array<windows::Win32::Foundation::RECT>,
    a2: u32,
}
#[derive(Clone)]
pub struct UpdateTileMappings {
    a0: Object,
    a1: u32,
    a2: Array<windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE>,
    a3: Array<windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE>,
    a4: Object,
    a5: u32,
    a6: Array<u32>,
    a7: Array<u32>,
    a8: Array<u32>,
    a9: u32,
}
#[derive(Clone)]
pub struct CopyTileMappings {
    a0: Object,
    a1: Array<windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE>,
    a2: Object,
    a3: Array<windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE>,
    a4: Array<windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE>,
    a5: u32,
}
#[derive(Clone)]
pub struct CopyTiles {
    a0: Object,
    a1: Array<windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE>,
    a2: Array<windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE>,
    a3: Object,
    a4: u64,
    a5: u32,
}
#[derive(Clone)]
pub struct ResizeTilePool {
    a0: Object,
    a1: u64,
}
#[derive(Clone)]
pub struct TiledResourceBarrier {
    a0: Object,
    a1: Object,
}
#[derive(Clone)]
pub enum Command {
    VSSetConstantBuffers(Box<VSSetConstantBuffers>),
    PSSetShaderResources(Box<PSSetShaderResources>),
    PSSetShader(Box<PSSetShader>),
    PSSetSamplers(Box<PSSetSamplers>),
    VSSetShader(Box<VSSetShader>),
    DrawIndexed(Box<DrawIndexed>),
    Draw(Box<Draw>),
    PSSetConstantBuffers(Box<PSSetConstantBuffers>),
    IASetInputLayout(Box<IASetInputLayout>),
    IASetVertexBuffers(Box<IASetVertexBuffers>),
    IASetIndexBuffer(Box<IASetIndexBuffer>),
    DrawIndexedInstanced(Box<DrawIndexedInstanced>),
    DrawInstanced(Box<DrawInstanced>),
    GSSetConstantBuffers(Box<GSSetConstantBuffers>),
    GSSetShader(Box<GSSetShader>),
    IASetPrimitiveTopology(Box<IASetPrimitiveTopology>),
    VSSetShaderResources(Box<VSSetShaderResources>),
    VSSetSamplers(Box<VSSetSamplers>),
    Begin(Box<Begin>),
    End(Box<End>),
    SetPredication(Box<SetPredication>),
    GSSetShaderResources(Box<GSSetShaderResources>),
    GSSetSamplers(Box<GSSetSamplers>),
    OMSetRenderTargets(Box<OMSetRenderTargets>),
    OMSetRenderTargetsAndUnorderedAccessViews(Box<OMSetRenderTargetsAndUnorderedAccessViews>),
    OMSetBlendState(Box<OMSetBlendState>),
    OMSetDepthStencilState(Box<OMSetDepthStencilState>),
    SOSetTargets(Box<SOSetTargets>),
    DrawAuto(Box<DrawAuto>),
    DrawIndexedInstancedIndirect(Box<DrawIndexedInstancedIndirect>),
    DrawInstancedIndirect(Box<DrawInstancedIndirect>),
    Dispatch(Box<Dispatch>),
    DispatchIndirect(Box<DispatchIndirect>),
    RSSetState(Box<RSSetState>),
    RSSetViewports(Box<RSSetViewports>),
    RSSetScissorRects(Box<RSSetScissorRects>),
    CopySubresourceRegion(Box<CopySubresourceRegion>),
    CopyResource(Box<CopyResource>),
    CopyStructureCount(Box<CopyStructureCount>),
    ClearRenderTargetView(Box<ClearRenderTargetView>),
    ClearUnorderedAccessViewUint(Box<ClearUnorderedAccessViewUint>),
    ClearUnorderedAccessViewFloat(Box<ClearUnorderedAccessViewFloat>),
    ClearDepthStencilView(Box<ClearDepthStencilView>),
    GenerateMips(Box<GenerateMips>),
    SetResourceMinLOD(Box<SetResourceMinLOD>),
    ResolveSubresource(Box<ResolveSubresource>),
    ExecuteCommandList(Box<ExecuteCommandList>),
    HSSetShaderResources(Box<HSSetShaderResources>),
    HSSetShader(Box<HSSetShader>),
    HSSetSamplers(Box<HSSetSamplers>),
    HSSetConstantBuffers(Box<HSSetConstantBuffers>),
    DSSetShaderResources(Box<DSSetShaderResources>),
    DSSetShader(Box<DSSetShader>),
    DSSetSamplers(Box<DSSetSamplers>),
    DSSetConstantBuffers(Box<DSSetConstantBuffers>),
    CSSetShaderResources(Box<CSSetShaderResources>),
    CSSetUnorderedAccessViews(Box<CSSetUnorderedAccessViews>),
    CSSetShader(Box<CSSetShader>),
    CSSetSamplers(Box<CSSetSamplers>),
    CSSetConstantBuffers(Box<CSSetConstantBuffers>),
    ClearState(Box<ClearState>),
    CopySubresourceRegion1(Box<CopySubresourceRegion1>),
    DiscardResource(Box<DiscardResource>),
    DiscardView(Box<DiscardView>),
    VSSetConstantBuffers1(Box<VSSetConstantBuffers1>),
    HSSetConstantBuffers1(Box<HSSetConstantBuffers1>),
    DSSetConstantBuffers1(Box<DSSetConstantBuffers1>),
    GSSetConstantBuffers1(Box<GSSetConstantBuffers1>),
    PSSetConstantBuffers1(Box<PSSetConstantBuffers1>),
    CSSetConstantBuffers1(Box<CSSetConstantBuffers1>),
    ClearView(Box<ClearView>),
    DiscardView1(Box<DiscardView1>),
    UpdateTileMappings(Box<UpdateTileMappings>),
    CopyTileMappings(Box<CopyTileMappings>),
    CopyTiles(Box<CopyTiles>),
    ResizeTilePool(Box<ResizeTilePool>),
    TiledResourceBarrier(Box<TiledResourceBarrier>),
}
impl Command {
    pub unsafe fn capture_VSSetConstantBuffers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::VSSetConstantBuffers(Box::new(VSSetConstantBuffers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_PSSetShaderResources(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::PSSetShaderResources(Box::new(PSSetShaderResources {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_PSSetShader(
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) -> Self {
        Self::PSSetShader(Box::new(PSSetShader {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Objects::copy(a1, a2) },
            a2,
        }))
    }
    pub unsafe fn capture_PSSetSamplers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::PSSetSamplers(Box::new(PSSetSamplers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_VSSetShader(
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) -> Self {
        Self::VSSetShader(Box::new(VSSetShader {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Objects::copy(a1, a2) },
            a2,
        }))
    }
    pub unsafe fn capture_DrawIndexed(a0: u32, a1: u32, a2: i32) -> Self {
        Self::DrawIndexed(Box::new(DrawIndexed { a0, a1, a2 }))
    }
    pub unsafe fn capture_Draw(a0: u32, a1: u32) -> Self {
        Self::Draw(Box::new(Draw { a0, a1 }))
    }
    pub unsafe fn capture_PSSetConstantBuffers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::PSSetConstantBuffers(Box::new(PSSetConstantBuffers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_IASetInputLayout(a0: *mut core::ffi::c_void) -> Self {
        Self::IASetInputLayout(Box::new(IASetInputLayout {
            a0: unsafe { Object::copy(a0) },
        }))
    }
    pub unsafe fn capture_IASetVertexBuffers(
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) -> Self {
        Self::IASetVertexBuffers(Box::new(IASetVertexBuffers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
            a3: unsafe { Array::copy(a3, a1) },
            a4: unsafe { Array::copy(a4, a1) },
        }))
    }
    pub unsafe fn capture_IASetIndexBuffer(
        a0: *mut core::ffi::c_void,
        a1: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        a2: u32,
    ) -> Self {
        Self::IASetIndexBuffer(Box::new(IASetIndexBuffer {
            a0: unsafe { Object::copy(a0) },
            a1,
            a2,
        }))
    }
    pub unsafe fn capture_DrawIndexedInstanced(a0: u32, a1: u32, a2: u32, a3: i32, a4: u32) -> Self {
        Self::DrawIndexedInstanced(Box::new(DrawIndexedInstanced { a0, a1, a2, a3, a4 }))
    }
    pub unsafe fn capture_DrawInstanced(a0: u32, a1: u32, a2: u32, a3: u32) -> Self {
        Self::DrawInstanced(Box::new(DrawInstanced { a0, a1, a2, a3 }))
    }
    pub unsafe fn capture_GSSetConstantBuffers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::GSSetConstantBuffers(Box::new(GSSetConstantBuffers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_GSSetShader(
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) -> Self {
        Self::GSSetShader(Box::new(GSSetShader {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Objects::copy(a1, a2) },
            a2,
        }))
    }
    pub unsafe fn capture_IASetPrimitiveTopology(
        a0: windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY,
    ) -> Self {
        Self::IASetPrimitiveTopology(Box::new(IASetPrimitiveTopology { a0 }))
    }
    pub unsafe fn capture_VSSetShaderResources(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::VSSetShaderResources(Box::new(VSSetShaderResources {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_VSSetSamplers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::VSSetSamplers(Box::new(VSSetSamplers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_Begin(a0: *mut core::ffi::c_void) -> Self {
        Self::Begin(Box::new(Begin {
            a0: unsafe { Object::copy(a0) },
        }))
    }
    pub unsafe fn capture_End(a0: *mut core::ffi::c_void) -> Self {
        Self::End(Box::new(End {
            a0: unsafe { Object::copy(a0) },
        }))
    }
    pub unsafe fn capture_SetPredication(a0: *mut core::ffi::c_void, a1: windows::core::BOOL) -> Self {
        Self::SetPredication(Box::new(SetPredication {
            a0: unsafe { Object::copy(a0) },
            a1,
        }))
    }
    pub unsafe fn capture_GSSetShaderResources(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::GSSetShaderResources(Box::new(GSSetShaderResources {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_GSSetSamplers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::GSSetSamplers(Box::new(GSSetSamplers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_OMSetRenderTargets(
        a0: u32,
        a1: *const *mut core::ffi::c_void,
        a2: *mut core::ffi::c_void,
    ) -> Self {
        Self::OMSetRenderTargets(Box::new(OMSetRenderTargets {
            a0,
            a1: unsafe { Objects::copy(a1, if a0 == u32::MAX { 0 } else { a0 }) },
            a2: unsafe { Object::copy(a2) },
        }))
    }
    pub unsafe fn capture_OMSetRenderTargetsAndUnorderedAccessViews(
        a0: u32,
        a1: *const *mut core::ffi::c_void,
        a2: *mut core::ffi::c_void,
        a3: u32,
        a4: u32,
        a5: *const *mut core::ffi::c_void,
        a6: *const u32,
    ) -> Self {
        Self::OMSetRenderTargetsAndUnorderedAccessViews(Box::new(OMSetRenderTargetsAndUnorderedAccessViews {
            a0,
            a1: unsafe { Objects::copy(a1, if a0 == u32::MAX { 0 } else { a0 }) },
            a2: unsafe { Object::copy(a2) },
            a3,
            a4,
            a5: unsafe { Objects::copy(a5, if a4 == u32::MAX { 0 } else { a4 }) },
            a6: unsafe { Array::copy(a6, if a4 == u32::MAX { 0 } else { a4 }) },
        }))
    }
    pub unsafe fn capture_OMSetBlendState(a0: *mut core::ffi::c_void, a1: *const f32, a2: u32) -> Self {
        Self::OMSetBlendState(Box::new(OMSetBlendState {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Array::copy(a1, 4) },
            a2,
        }))
    }
    pub unsafe fn capture_OMSetDepthStencilState(a0: *mut core::ffi::c_void, a1: u32) -> Self {
        Self::OMSetDepthStencilState(Box::new(OMSetDepthStencilState {
            a0: unsafe { Object::copy(a0) },
            a1,
        }))
    }
    pub unsafe fn capture_SOSetTargets(a0: u32, a1: *const *mut core::ffi::c_void, a2: *const u32) -> Self {
        Self::SOSetTargets(Box::new(SOSetTargets {
            a0,
            a1: unsafe { Objects::copy(a1, a0) },
            a2: unsafe { Array::copy(a2, a0) },
        }))
    }
    pub unsafe fn capture_DrawAuto() -> Self {
        Self::DrawAuto(Box::new(DrawAuto {}))
    }
    pub unsafe fn capture_DrawIndexedInstancedIndirect(a0: *mut core::ffi::c_void, a1: u32) -> Self {
        Self::DrawIndexedInstancedIndirect(Box::new(DrawIndexedInstancedIndirect {
            a0: unsafe { Object::copy(a0) },
            a1,
        }))
    }
    pub unsafe fn capture_DrawInstancedIndirect(a0: *mut core::ffi::c_void, a1: u32) -> Self {
        Self::DrawInstancedIndirect(Box::new(DrawInstancedIndirect {
            a0: unsafe { Object::copy(a0) },
            a1,
        }))
    }
    pub unsafe fn capture_Dispatch(a0: u32, a1: u32, a2: u32) -> Self {
        Self::Dispatch(Box::new(Dispatch { a0, a1, a2 }))
    }
    pub unsafe fn capture_DispatchIndirect(a0: *mut core::ffi::c_void, a1: u32) -> Self {
        Self::DispatchIndirect(Box::new(DispatchIndirect {
            a0: unsafe { Object::copy(a0) },
            a1,
        }))
    }
    pub unsafe fn capture_RSSetState(a0: *mut core::ffi::c_void) -> Self {
        Self::RSSetState(Box::new(RSSetState {
            a0: unsafe { Object::copy(a0) },
        }))
    }
    pub unsafe fn capture_RSSetViewports(
        a0: u32,
        a1: *const windows::Win32::Graphics::Direct3D11::D3D11_VIEWPORT,
    ) -> Self {
        Self::RSSetViewports(Box::new(RSSetViewports {
            a0,
            a1: unsafe { Array::copy(a1, a0) },
        }))
    }
    pub unsafe fn capture_RSSetScissorRects(a0: u32, a1: *const windows::Win32::Foundation::RECT) -> Self {
        Self::RSSetScissorRects(Box::new(RSSetScissorRects {
            a0,
            a1: unsafe { Array::copy(a1, a0) },
        }))
    }
    pub unsafe fn capture_CopySubresourceRegion(
        a0: *mut core::ffi::c_void,
        a1: u32,
        a2: u32,
        a3: u32,
        a4: u32,
        a5: *mut core::ffi::c_void,
        a6: u32,
        a7: *const windows::Win32::Graphics::Direct3D11::D3D11_BOX,
    ) -> Self {
        Self::CopySubresourceRegion(Box::new(CopySubresourceRegion {
            a0: unsafe { Object::copy(a0) },
            a1,
            a2,
            a3,
            a4,
            a5: unsafe { Object::copy(a5) },
            a6,
            a7: unsafe { Array::copy(a7, 1) },
        }))
    }
    pub unsafe fn capture_CopyResource(a0: *mut core::ffi::c_void, a1: *mut core::ffi::c_void) -> Self {
        Self::CopyResource(Box::new(CopyResource {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Object::copy(a1) },
        }))
    }
    pub unsafe fn capture_CopyStructureCount(
        a0: *mut core::ffi::c_void,
        a1: u32,
        a2: *mut core::ffi::c_void,
    ) -> Self {
        Self::CopyStructureCount(Box::new(CopyStructureCount {
            a0: unsafe { Object::copy(a0) },
            a1,
            a2: unsafe { Object::copy(a2) },
        }))
    }
    pub unsafe fn capture_ClearRenderTargetView(a0: *mut core::ffi::c_void, a1: *const f32) -> Self {
        Self::ClearRenderTargetView(Box::new(ClearRenderTargetView {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Array::copy(a1, 4) },
        }))
    }
    pub unsafe fn capture_ClearUnorderedAccessViewUint(a0: *mut core::ffi::c_void, a1: *const u32) -> Self {
        Self::ClearUnorderedAccessViewUint(Box::new(ClearUnorderedAccessViewUint {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Array::copy(a1, 4) },
        }))
    }
    pub unsafe fn capture_ClearUnorderedAccessViewFloat(a0: *mut core::ffi::c_void, a1: *const f32) -> Self {
        Self::ClearUnorderedAccessViewFloat(Box::new(ClearUnorderedAccessViewFloat {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Array::copy(a1, 4) },
        }))
    }
    pub unsafe fn capture_ClearDepthStencilView(
        a0: *mut core::ffi::c_void,
        a1: u32,
        a2: f32,
        a3: u8,
    ) -> Self {
        Self::ClearDepthStencilView(Box::new(ClearDepthStencilView {
            a0: unsafe { Object::copy(a0) },
            a1,
            a2,
            a3,
        }))
    }
    pub unsafe fn capture_GenerateMips(a0: *mut core::ffi::c_void) -> Self {
        Self::GenerateMips(Box::new(GenerateMips {
            a0: unsafe { Object::copy(a0) },
        }))
    }
    pub unsafe fn capture_SetResourceMinLOD(a0: *mut core::ffi::c_void, a1: f32) -> Self {
        Self::SetResourceMinLOD(Box::new(SetResourceMinLOD {
            a0: unsafe { Object::copy(a0) },
            a1,
        }))
    }
    pub unsafe fn capture_ResolveSubresource(
        a0: *mut core::ffi::c_void,
        a1: u32,
        a2: *mut core::ffi::c_void,
        a3: u32,
        a4: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) -> Self {
        Self::ResolveSubresource(Box::new(ResolveSubresource {
            a0: unsafe { Object::copy(a0) },
            a1,
            a2: unsafe { Object::copy(a2) },
            a3,
            a4,
        }))
    }
    pub unsafe fn capture_ExecuteCommandList(a0: *mut core::ffi::c_void, a1: windows::core::BOOL) -> Self {
        Self::ExecuteCommandList(Box::new(ExecuteCommandList {
            a0: unsafe { Object::copy(a0) },
            a1,
        }))
    }
    pub unsafe fn capture_HSSetShaderResources(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::HSSetShaderResources(Box::new(HSSetShaderResources {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_HSSetShader(
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) -> Self {
        Self::HSSetShader(Box::new(HSSetShader {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Objects::copy(a1, a2) },
            a2,
        }))
    }
    pub unsafe fn capture_HSSetSamplers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::HSSetSamplers(Box::new(HSSetSamplers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_HSSetConstantBuffers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::HSSetConstantBuffers(Box::new(HSSetConstantBuffers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_DSSetShaderResources(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::DSSetShaderResources(Box::new(DSSetShaderResources {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_DSSetShader(
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) -> Self {
        Self::DSSetShader(Box::new(DSSetShader {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Objects::copy(a1, a2) },
            a2,
        }))
    }
    pub unsafe fn capture_DSSetSamplers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::DSSetSamplers(Box::new(DSSetSamplers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_DSSetConstantBuffers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::DSSetConstantBuffers(Box::new(DSSetConstantBuffers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_CSSetShaderResources(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::CSSetShaderResources(Box::new(CSSetShaderResources {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_CSSetUnorderedAccessViews(
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
    ) -> Self {
        Self::CSSetUnorderedAccessViews(Box::new(CSSetUnorderedAccessViews {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
            a3: unsafe { Array::copy(a3, a1) },
        }))
    }
    pub unsafe fn capture_CSSetShader(
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) -> Self {
        Self::CSSetShader(Box::new(CSSetShader {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Objects::copy(a1, a2) },
            a2,
        }))
    }
    pub unsafe fn capture_CSSetSamplers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::CSSetSamplers(Box::new(CSSetSamplers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_CSSetConstantBuffers(a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) -> Self {
        Self::CSSetConstantBuffers(Box::new(CSSetConstantBuffers {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
        }))
    }
    pub unsafe fn capture_ClearState() -> Self {
        Self::ClearState(Box::new(ClearState {}))
    }
    pub unsafe fn capture_CopySubresourceRegion1(
        a0: *mut core::ffi::c_void,
        a1: u32,
        a2: u32,
        a3: u32,
        a4: u32,
        a5: *mut core::ffi::c_void,
        a6: u32,
        a7: *const windows::Win32::Graphics::Direct3D11::D3D11_BOX,
        a8: u32,
    ) -> Self {
        Self::CopySubresourceRegion1(Box::new(CopySubresourceRegion1 {
            a0: unsafe { Object::copy(a0) },
            a1,
            a2,
            a3,
            a4,
            a5: unsafe { Object::copy(a5) },
            a6,
            a7: unsafe { Array::copy(a7, 1) },
            a8,
        }))
    }
    pub unsafe fn capture_DiscardResource(a0: *mut core::ffi::c_void) -> Self {
        Self::DiscardResource(Box::new(DiscardResource {
            a0: unsafe { Object::copy(a0) },
        }))
    }
    pub unsafe fn capture_DiscardView(a0: *mut core::ffi::c_void) -> Self {
        Self::DiscardView(Box::new(DiscardView {
            a0: unsafe { Object::copy(a0) },
        }))
    }
    pub unsafe fn capture_VSSetConstantBuffers1(
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) -> Self {
        Self::VSSetConstantBuffers1(Box::new(VSSetConstantBuffers1 {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
            a3: unsafe { Array::copy(a3, a1) },
            a4: unsafe { Array::copy(a4, a1) },
        }))
    }
    pub unsafe fn capture_HSSetConstantBuffers1(
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) -> Self {
        Self::HSSetConstantBuffers1(Box::new(HSSetConstantBuffers1 {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
            a3: unsafe { Array::copy(a3, a1) },
            a4: unsafe { Array::copy(a4, a1) },
        }))
    }
    pub unsafe fn capture_DSSetConstantBuffers1(
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) -> Self {
        Self::DSSetConstantBuffers1(Box::new(DSSetConstantBuffers1 {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
            a3: unsafe { Array::copy(a3, a1) },
            a4: unsafe { Array::copy(a4, a1) },
        }))
    }
    pub unsafe fn capture_GSSetConstantBuffers1(
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) -> Self {
        Self::GSSetConstantBuffers1(Box::new(GSSetConstantBuffers1 {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
            a3: unsafe { Array::copy(a3, a1) },
            a4: unsafe { Array::copy(a4, a1) },
        }))
    }
    pub unsafe fn capture_PSSetConstantBuffers1(
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) -> Self {
        Self::PSSetConstantBuffers1(Box::new(PSSetConstantBuffers1 {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
            a3: unsafe { Array::copy(a3, a1) },
            a4: unsafe { Array::copy(a4, a1) },
        }))
    }
    pub unsafe fn capture_CSSetConstantBuffers1(
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) -> Self {
        Self::CSSetConstantBuffers1(Box::new(CSSetConstantBuffers1 {
            a0,
            a1,
            a2: unsafe { Objects::copy(a2, a1) },
            a3: unsafe { Array::copy(a3, a1) },
            a4: unsafe { Array::copy(a4, a1) },
        }))
    }
    pub unsafe fn capture_ClearView(
        a0: *mut core::ffi::c_void,
        a1: *const f32,
        a2: *const windows::Win32::Foundation::RECT,
        a3: u32,
    ) -> Self {
        Self::ClearView(Box::new(ClearView {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Array::copy(a1, 4) },
            a2: unsafe { Array::copy(a2, a3) },
            a3,
        }))
    }
    pub unsafe fn capture_DiscardView1(
        a0: *mut core::ffi::c_void,
        a1: *const windows::Win32::Foundation::RECT,
        a2: u32,
    ) -> Self {
        Self::DiscardView1(Box::new(DiscardView1 {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Array::copy(a1, a2) },
            a2,
        }))
    }
    pub unsafe fn capture_UpdateTileMappings(
        a0: *mut core::ffi::c_void,
        a1: u32,
        a2: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE,
        a3: *const windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE,
        a4: *mut core::ffi::c_void,
        a5: u32,
        a6: *const u32,
        a7: *const u32,
        a8: *const u32,
        a9: u32,
    ) -> Self {
        Self::UpdateTileMappings(Box::new(UpdateTileMappings {
            a0: unsafe { Object::copy(a0) },
            a1,
            a2: unsafe { Array::copy(a2, a1) },
            a3: unsafe { Array::copy(a3, a1) },
            a4: unsafe { Object::copy(a4) },
            a5,
            a6: unsafe { Array::copy(a6, a5) },
            a7: unsafe { Array::copy(a7, a5) },
            a8: unsafe { Array::copy(a8, a5) },
            a9,
        }))
    }
    pub unsafe fn capture_CopyTileMappings(
        a0: *mut core::ffi::c_void,
        a1: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE,
        a2: *mut core::ffi::c_void,
        a3: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE,
        a4: *const windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE,
        a5: u32,
    ) -> Self {
        Self::CopyTileMappings(Box::new(CopyTileMappings {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Array::copy(a1, 1) },
            a2: unsafe { Object::copy(a2) },
            a3: unsafe { Array::copy(a3, 1) },
            a4: unsafe { Array::copy(a4, 1) },
            a5,
        }))
    }
    pub unsafe fn capture_CopyTiles(
        a0: *mut core::ffi::c_void,
        a1: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE,
        a2: *const windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE,
        a3: *mut core::ffi::c_void,
        a4: u64,
        a5: u32,
    ) -> Self {
        Self::CopyTiles(Box::new(CopyTiles {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Array::copy(a1, 1) },
            a2: unsafe { Array::copy(a2, 1) },
            a3: unsafe { Object::copy(a3) },
            a4,
            a5,
        }))
    }
    pub unsafe fn capture_ResizeTilePool(a0: *mut core::ffi::c_void, a1: u64) -> Self {
        Self::ResizeTilePool(Box::new(ResizeTilePool {
            a0: unsafe { Object::copy(a0) },
            a1,
        }))
    }
    pub unsafe fn capture_TiledResourceBarrier(
        a0: *mut core::ffi::c_void,
        a1: *mut core::ffi::c_void,
    ) -> Self {
        Self::TiledResourceBarrier(Box::new(TiledResourceBarrier {
            a0: unsafe { Object::copy(a0) },
            a1: unsafe { Object::copy(a1) },
        }))
    }
    pub unsafe fn replay(&self, context: *mut c_void) -> windows::core::Result<()> {
        match self {
            Self::VSSetConstantBuffers(p) => {
                let VSSetConstantBuffers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::VSSetConstantBuffers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::PSSetShaderResources(p) => {
                let PSSetShaderResources { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::PSSetShaderResources(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::PSSetShader(p) => {
                let PSSetShader { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::PSSetShader(context, a0.raw(), a1.raw(), *a2) };
                Ok(())
            }
            Self::PSSetSamplers(p) => {
                let PSSetSamplers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::PSSetSamplers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::VSSetShader(p) => {
                let VSSetShader { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::VSSetShader(context, a0.raw(), a1.raw(), *a2) };
                Ok(())
            }
            Self::DrawIndexed(p) => {
                let DrawIndexed { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::DrawIndexed(context, *a0, *a1, *a2) };
                Ok(())
            }
            Self::Draw(p) => {
                let Draw { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::Draw(context, *a0, *a1) };
                Ok(())
            }
            Self::PSSetConstantBuffers(p) => {
                let PSSetConstantBuffers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::PSSetConstantBuffers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::IASetInputLayout(p) => {
                let IASetInputLayout { a0 } = &**p;
                unsafe { crate::vtables::context::real::IASetInputLayout(context, a0.raw()) };
                Ok(())
            }
            Self::IASetVertexBuffers(p) => {
                let IASetVertexBuffers { a0, a1, a2, a3, a4 } = &**p;
                unsafe {
                    crate::vtables::context::real::IASetVertexBuffers(
                        context,
                        *a0,
                        *a1,
                        a2.raw(),
                        a3.raw(),
                        a4.raw(),
                    )
                };
                Ok(())
            }
            Self::IASetIndexBuffer(p) => {
                let IASetIndexBuffer { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::IASetIndexBuffer(context, a0.raw(), *a1, *a2) };
                Ok(())
            }
            Self::DrawIndexedInstanced(p) => {
                let DrawIndexedInstanced { a0, a1, a2, a3, a4 } = &**p;
                unsafe {
                    crate::vtables::context::real::DrawIndexedInstanced(context, *a0, *a1, *a2, *a3, *a4)
                };
                Ok(())
            }
            Self::DrawInstanced(p) => {
                let DrawInstanced { a0, a1, a2, a3 } = &**p;
                unsafe { crate::vtables::context::real::DrawInstanced(context, *a0, *a1, *a2, *a3) };
                Ok(())
            }
            Self::GSSetConstantBuffers(p) => {
                let GSSetConstantBuffers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::GSSetConstantBuffers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::GSSetShader(p) => {
                let GSSetShader { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::GSSetShader(context, a0.raw(), a1.raw(), *a2) };
                Ok(())
            }
            Self::IASetPrimitiveTopology(p) => {
                let IASetPrimitiveTopology { a0 } = &**p;
                unsafe { crate::vtables::context::real::IASetPrimitiveTopology(context, *a0) };
                Ok(())
            }
            Self::VSSetShaderResources(p) => {
                let VSSetShaderResources { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::VSSetShaderResources(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::VSSetSamplers(p) => {
                let VSSetSamplers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::VSSetSamplers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::Begin(p) => {
                let Begin { a0 } = &**p;
                unsafe { crate::vtables::context::real::Begin(context, a0.raw()) };
                Ok(())
            }
            Self::End(p) => {
                let End { a0 } = &**p;
                unsafe { crate::vtables::context::real::End(context, a0.raw()) };
                Ok(())
            }
            Self::SetPredication(p) => {
                let SetPredication { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::SetPredication(context, a0.raw(), *a1) };
                Ok(())
            }
            Self::GSSetShaderResources(p) => {
                let GSSetShaderResources { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::GSSetShaderResources(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::GSSetSamplers(p) => {
                let GSSetSamplers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::GSSetSamplers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::OMSetRenderTargets(p) => {
                let OMSetRenderTargets { a0, a1, a2 } = &**p;
                unsafe {
                    crate::vtables::context::real::OMSetRenderTargets(context, *a0, a1.raw(), a2.raw())
                };
                Ok(())
            }
            Self::OMSetRenderTargetsAndUnorderedAccessViews(p) => {
                let OMSetRenderTargetsAndUnorderedAccessViews {
                    a0,
                    a1,
                    a2,
                    a3,
                    a4,
                    a5,
                    a6,
                } = &**p;
                unsafe {
                    crate::vtables::context::real::OMSetRenderTargetsAndUnorderedAccessViews(
                        context,
                        *a0,
                        a1.raw(),
                        a2.raw(),
                        *a3,
                        *a4,
                        a5.raw(),
                        a6.raw(),
                    )
                };
                Ok(())
            }
            Self::OMSetBlendState(p) => {
                let OMSetBlendState { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::OMSetBlendState(context, a0.raw(), a1.raw(), *a2) };
                Ok(())
            }
            Self::OMSetDepthStencilState(p) => {
                let OMSetDepthStencilState { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::OMSetDepthStencilState(context, a0.raw(), *a1) };
                Ok(())
            }
            Self::SOSetTargets(p) => {
                let SOSetTargets { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::SOSetTargets(context, *a0, a1.raw(), a2.raw()) };
                Ok(())
            }
            Self::DrawAuto(p) => {
                let DrawAuto {} = &**p;
                unsafe { crate::vtables::context::real::DrawAuto(context) };
                Ok(())
            }
            Self::DrawIndexedInstancedIndirect(p) => {
                let DrawIndexedInstancedIndirect { a0, a1 } = &**p;
                unsafe {
                    crate::vtables::context::real::DrawIndexedInstancedIndirect(context, a0.raw(), *a1)
                };
                Ok(())
            }
            Self::DrawInstancedIndirect(p) => {
                let DrawInstancedIndirect { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::DrawInstancedIndirect(context, a0.raw(), *a1) };
                Ok(())
            }
            Self::Dispatch(p) => {
                let Dispatch { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::Dispatch(context, *a0, *a1, *a2) };
                Ok(())
            }
            Self::DispatchIndirect(p) => {
                let DispatchIndirect { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::DispatchIndirect(context, a0.raw(), *a1) };
                Ok(())
            }
            Self::RSSetState(p) => {
                let RSSetState { a0 } = &**p;
                unsafe { crate::vtables::context::real::RSSetState(context, a0.raw()) };
                Ok(())
            }
            Self::RSSetViewports(p) => {
                let RSSetViewports { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::RSSetViewports(context, *a0, a1.raw()) };
                Ok(())
            }
            Self::RSSetScissorRects(p) => {
                let RSSetScissorRects { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::RSSetScissorRects(context, *a0, a1.raw()) };
                Ok(())
            }
            Self::CopySubresourceRegion(p) => {
                let CopySubresourceRegion {
                    a0,
                    a1,
                    a2,
                    a3,
                    a4,
                    a5,
                    a6,
                    a7,
                } = &**p;
                unsafe {
                    crate::vtables::context::real::CopySubresourceRegion(
                        context,
                        a0.raw(),
                        *a1,
                        *a2,
                        *a3,
                        *a4,
                        a5.raw(),
                        *a6,
                        a7.raw(),
                    )
                };
                Ok(())
            }
            Self::CopyResource(p) => {
                let CopyResource { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::CopyResource(context, a0.raw(), a1.raw()) };
                Ok(())
            }
            Self::CopyStructureCount(p) => {
                let CopyStructureCount { a0, a1, a2 } = &**p;
                unsafe {
                    crate::vtables::context::real::CopyStructureCount(context, a0.raw(), *a1, a2.raw())
                };
                Ok(())
            }
            Self::ClearRenderTargetView(p) => {
                let ClearRenderTargetView { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::ClearRenderTargetView(context, a0.raw(), a1.raw()) };
                Ok(())
            }
            Self::ClearUnorderedAccessViewUint(p) => {
                let ClearUnorderedAccessViewUint { a0, a1 } = &**p;
                unsafe {
                    crate::vtables::context::real::ClearUnorderedAccessViewUint(context, a0.raw(), a1.raw())
                };
                Ok(())
            }
            Self::ClearUnorderedAccessViewFloat(p) => {
                let ClearUnorderedAccessViewFloat { a0, a1 } = &**p;
                unsafe {
                    crate::vtables::context::real::ClearUnorderedAccessViewFloat(context, a0.raw(), a1.raw())
                };
                Ok(())
            }
            Self::ClearDepthStencilView(p) => {
                let ClearDepthStencilView { a0, a1, a2, a3 } = &**p;
                unsafe {
                    crate::vtables::context::real::ClearDepthStencilView(context, a0.raw(), *a1, *a2, *a3)
                };
                Ok(())
            }
            Self::GenerateMips(p) => {
                let GenerateMips { a0 } = &**p;
                unsafe { crate::vtables::context::real::GenerateMips(context, a0.raw()) };
                Ok(())
            }
            Self::SetResourceMinLOD(p) => {
                let SetResourceMinLOD { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::SetResourceMinLOD(context, a0.raw(), *a1) };
                Ok(())
            }
            Self::ResolveSubresource(p) => {
                let ResolveSubresource { a0, a1, a2, a3, a4 } = &**p;
                unsafe {
                    crate::vtables::context::real::ResolveSubresource(
                        context,
                        a0.raw(),
                        *a1,
                        a2.raw(),
                        *a3,
                        *a4,
                    )
                };
                Ok(())
            }
            Self::ExecuteCommandList(p) => {
                let ExecuteCommandList { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::ExecuteCommandList(context, a0.raw(), *a1) };
                Ok(())
            }
            Self::HSSetShaderResources(p) => {
                let HSSetShaderResources { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::HSSetShaderResources(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::HSSetShader(p) => {
                let HSSetShader { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::HSSetShader(context, a0.raw(), a1.raw(), *a2) };
                Ok(())
            }
            Self::HSSetSamplers(p) => {
                let HSSetSamplers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::HSSetSamplers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::HSSetConstantBuffers(p) => {
                let HSSetConstantBuffers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::HSSetConstantBuffers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::DSSetShaderResources(p) => {
                let DSSetShaderResources { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::DSSetShaderResources(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::DSSetShader(p) => {
                let DSSetShader { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::DSSetShader(context, a0.raw(), a1.raw(), *a2) };
                Ok(())
            }
            Self::DSSetSamplers(p) => {
                let DSSetSamplers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::DSSetSamplers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::DSSetConstantBuffers(p) => {
                let DSSetConstantBuffers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::DSSetConstantBuffers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::CSSetShaderResources(p) => {
                let CSSetShaderResources { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::CSSetShaderResources(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::CSSetUnorderedAccessViews(p) => {
                let CSSetUnorderedAccessViews { a0, a1, a2, a3 } = &**p;
                unsafe {
                    crate::vtables::context::real::CSSetUnorderedAccessViews(
                        context,
                        *a0,
                        *a1,
                        a2.raw(),
                        a3.raw(),
                    )
                };
                Ok(())
            }
            Self::CSSetShader(p) => {
                let CSSetShader { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::CSSetShader(context, a0.raw(), a1.raw(), *a2) };
                Ok(())
            }
            Self::CSSetSamplers(p) => {
                let CSSetSamplers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::CSSetSamplers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::CSSetConstantBuffers(p) => {
                let CSSetConstantBuffers { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::CSSetConstantBuffers(context, *a0, *a1, a2.raw()) };
                Ok(())
            }
            Self::ClearState(p) => {
                let ClearState {} = &**p;
                unsafe { crate::vtables::context::real::ClearState(context) };
                Ok(())
            }
            Self::CopySubresourceRegion1(p) => {
                let CopySubresourceRegion1 {
                    a0,
                    a1,
                    a2,
                    a3,
                    a4,
                    a5,
                    a6,
                    a7,
                    a8,
                } = &**p;
                unsafe {
                    crate::vtables::context::real::CopySubresourceRegion1(
                        context,
                        a0.raw(),
                        *a1,
                        *a2,
                        *a3,
                        *a4,
                        a5.raw(),
                        *a6,
                        a7.raw(),
                        *a8,
                    )
                };
                Ok(())
            }
            Self::DiscardResource(p) => {
                let DiscardResource { a0 } = &**p;
                unsafe { crate::vtables::context::real::DiscardResource(context, a0.raw()) };
                Ok(())
            }
            Self::DiscardView(p) => {
                let DiscardView { a0 } = &**p;
                unsafe { crate::vtables::context::real::DiscardView(context, a0.raw()) };
                Ok(())
            }
            Self::VSSetConstantBuffers1(p) => {
                let VSSetConstantBuffers1 { a0, a1, a2, a3, a4 } = &**p;
                unsafe {
                    crate::vtables::context::real::VSSetConstantBuffers1(
                        context,
                        *a0,
                        *a1,
                        a2.raw(),
                        a3.raw(),
                        a4.raw(),
                    )
                };
                Ok(())
            }
            Self::HSSetConstantBuffers1(p) => {
                let HSSetConstantBuffers1 { a0, a1, a2, a3, a4 } = &**p;
                unsafe {
                    crate::vtables::context::real::HSSetConstantBuffers1(
                        context,
                        *a0,
                        *a1,
                        a2.raw(),
                        a3.raw(),
                        a4.raw(),
                    )
                };
                Ok(())
            }
            Self::DSSetConstantBuffers1(p) => {
                let DSSetConstantBuffers1 { a0, a1, a2, a3, a4 } = &**p;
                unsafe {
                    crate::vtables::context::real::DSSetConstantBuffers1(
                        context,
                        *a0,
                        *a1,
                        a2.raw(),
                        a3.raw(),
                        a4.raw(),
                    )
                };
                Ok(())
            }
            Self::GSSetConstantBuffers1(p) => {
                let GSSetConstantBuffers1 { a0, a1, a2, a3, a4 } = &**p;
                unsafe {
                    crate::vtables::context::real::GSSetConstantBuffers1(
                        context,
                        *a0,
                        *a1,
                        a2.raw(),
                        a3.raw(),
                        a4.raw(),
                    )
                };
                Ok(())
            }
            Self::PSSetConstantBuffers1(p) => {
                let PSSetConstantBuffers1 { a0, a1, a2, a3, a4 } = &**p;
                unsafe {
                    crate::vtables::context::real::PSSetConstantBuffers1(
                        context,
                        *a0,
                        *a1,
                        a2.raw(),
                        a3.raw(),
                        a4.raw(),
                    )
                };
                Ok(())
            }
            Self::CSSetConstantBuffers1(p) => {
                let CSSetConstantBuffers1 { a0, a1, a2, a3, a4 } = &**p;
                unsafe {
                    crate::vtables::context::real::CSSetConstantBuffers1(
                        context,
                        *a0,
                        *a1,
                        a2.raw(),
                        a3.raw(),
                        a4.raw(),
                    )
                };
                Ok(())
            }
            Self::ClearView(p) => {
                let ClearView { a0, a1, a2, a3 } = &**p;
                unsafe {
                    crate::vtables::context::real::ClearView(context, a0.raw(), a1.raw(), a2.raw(), *a3)
                };
                Ok(())
            }
            Self::DiscardView1(p) => {
                let DiscardView1 { a0, a1, a2 } = &**p;
                unsafe { crate::vtables::context::real::DiscardView1(context, a0.raw(), a1.raw(), *a2) };
                Ok(())
            }
            Self::UpdateTileMappings(p) => {
                let UpdateTileMappings {
                    a0,
                    a1,
                    a2,
                    a3,
                    a4,
                    a5,
                    a6,
                    a7,
                    a8,
                    a9,
                } = &**p;
                unsafe {
                    crate::vtables::context::real::UpdateTileMappings(
                        context,
                        a0.raw(),
                        *a1,
                        a2.raw(),
                        a3.raw(),
                        a4.raw(),
                        *a5,
                        a6.raw(),
                        a7.raw(),
                        a8.raw(),
                        *a9,
                    )
                }
                .ok()
            }
            Self::CopyTileMappings(p) => {
                let CopyTileMappings {
                    a0,
                    a1,
                    a2,
                    a3,
                    a4,
                    a5,
                } = &**p;
                unsafe {
                    crate::vtables::context::real::CopyTileMappings(
                        context,
                        a0.raw(),
                        a1.raw(),
                        a2.raw(),
                        a3.raw(),
                        a4.raw(),
                        *a5,
                    )
                }
                .ok()
            }
            Self::CopyTiles(p) => {
                let CopyTiles {
                    a0,
                    a1,
                    a2,
                    a3,
                    a4,
                    a5,
                } = &**p;
                unsafe {
                    crate::vtables::context::real::CopyTiles(
                        context,
                        a0.raw(),
                        a1.raw(),
                        a2.raw(),
                        a3.raw(),
                        *a4,
                        *a5,
                    )
                };
                Ok(())
            }
            Self::ResizeTilePool(p) => {
                let ResizeTilePool { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::ResizeTilePool(context, a0.raw(), *a1) }.ok()
            }
            Self::TiledResourceBarrier(p) => {
                let TiledResourceBarrier { a0, a1 } = &**p;
                unsafe { crate::vtables::context::real::TiledResourceBarrier(context, a0.raw(), a1.raw()) };
                Ok(())
            }
        }
    }
}
