//! Generated deferred-context calls by `cargo xtask gen`; do not edit: each written to a tape and replayed from it.
#![allow(
    non_snake_case,
    non_upper_case_globals,
    clippy::too_many_arguments,
    clippy::missing_safety_doc,
    dead_code
)]
use crate::d3d11::recorder::{Reader, Tape};
use core::ffi::c_void;
/// Each recorded call's tag on a tape.
pub mod tag {
    pub const VSSetConstantBuffers: u32 = 0;
    pub const PSSetShaderResources: u32 = 1;
    pub const PSSetShader: u32 = 2;
    pub const PSSetSamplers: u32 = 3;
    pub const VSSetShader: u32 = 4;
    pub const DrawIndexed: u32 = 5;
    pub const Draw: u32 = 6;
    pub const PSSetConstantBuffers: u32 = 7;
    pub const IASetInputLayout: u32 = 8;
    pub const IASetVertexBuffers: u32 = 9;
    pub const IASetIndexBuffer: u32 = 10;
    pub const DrawIndexedInstanced: u32 = 11;
    pub const DrawInstanced: u32 = 12;
    pub const GSSetConstantBuffers: u32 = 13;
    pub const GSSetShader: u32 = 14;
    pub const IASetPrimitiveTopology: u32 = 15;
    pub const VSSetShaderResources: u32 = 16;
    pub const VSSetSamplers: u32 = 17;
    pub const Begin: u32 = 18;
    pub const End: u32 = 19;
    pub const SetPredication: u32 = 20;
    pub const GSSetShaderResources: u32 = 21;
    pub const GSSetSamplers: u32 = 22;
    pub const OMSetRenderTargets: u32 = 23;
    pub const OMSetRenderTargetsAndUnorderedAccessViews: u32 = 24;
    pub const OMSetBlendState: u32 = 25;
    pub const OMSetDepthStencilState: u32 = 26;
    pub const SOSetTargets: u32 = 27;
    pub const DrawAuto: u32 = 28;
    pub const DrawIndexedInstancedIndirect: u32 = 29;
    pub const DrawInstancedIndirect: u32 = 30;
    pub const Dispatch: u32 = 31;
    pub const DispatchIndirect: u32 = 32;
    pub const RSSetState: u32 = 33;
    pub const RSSetViewports: u32 = 34;
    pub const RSSetScissorRects: u32 = 35;
    pub const CopySubresourceRegion: u32 = 36;
    pub const CopyResource: u32 = 37;
    pub const CopyStructureCount: u32 = 38;
    pub const ClearRenderTargetView: u32 = 39;
    pub const ClearUnorderedAccessViewUint: u32 = 40;
    pub const ClearUnorderedAccessViewFloat: u32 = 41;
    pub const ClearDepthStencilView: u32 = 42;
    pub const GenerateMips: u32 = 43;
    pub const SetResourceMinLOD: u32 = 44;
    pub const ResolveSubresource: u32 = 45;
    pub const HSSetShaderResources: u32 = 46;
    pub const HSSetShader: u32 = 47;
    pub const HSSetSamplers: u32 = 48;
    pub const HSSetConstantBuffers: u32 = 49;
    pub const DSSetShaderResources: u32 = 50;
    pub const DSSetShader: u32 = 51;
    pub const DSSetSamplers: u32 = 52;
    pub const DSSetConstantBuffers: u32 = 53;
    pub const CSSetShaderResources: u32 = 54;
    pub const CSSetUnorderedAccessViews: u32 = 55;
    pub const CSSetShader: u32 = 56;
    pub const CSSetSamplers: u32 = 57;
    pub const CSSetConstantBuffers: u32 = 58;
    pub const ClearState: u32 = 59;
    pub const CopySubresourceRegion1: u32 = 60;
    pub const DiscardResource: u32 = 61;
    pub const DiscardView: u32 = 62;
    pub const VSSetConstantBuffers1: u32 = 63;
    pub const HSSetConstantBuffers1: u32 = 64;
    pub const DSSetConstantBuffers1: u32 = 65;
    pub const GSSetConstantBuffers1: u32 = 66;
    pub const PSSetConstantBuffers1: u32 = 67;
    pub const CSSetConstantBuffers1: u32 = 68;
    pub const ClearView: u32 = 69;
    pub const DiscardView1: u32 = 70;
    pub const UpdateTileMappings: u32 = 71;
    pub const CopyTileMappings: u32 = 72;
    pub const CopyTiles: u32 = 73;
    pub const ResizeTilePool: u32 = 74;
    pub const TiledResourceBarrier: u32 = 75;
}
impl Tape {
    pub unsafe fn VSSetConstantBuffers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::VSSetConstantBuffers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn PSSetShaderResources(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::PSSetShaderResources);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn PSSetShader(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) {
        self.tag(tag::PSSetShader);
        unsafe { self.object(a0) };
        unsafe { self.objects(a1, a2) };
        self.put(a2);
    }
    pub unsafe fn PSSetSamplers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::PSSetSamplers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn VSSetShader(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) {
        self.tag(tag::VSSetShader);
        unsafe { self.object(a0) };
        unsafe { self.objects(a1, a2) };
        self.put(a2);
    }
    pub unsafe fn DrawIndexed(&mut self, a0: u32, a1: u32, a2: i32) {
        self.tag(tag::DrawIndexed);
        self.put(a0);
        self.put(a1);
        self.put(a2);
    }
    pub unsafe fn Draw(&mut self, a0: u32, a1: u32) {
        self.tag(tag::Draw);
        self.put(a0);
        self.put(a1);
    }
    pub unsafe fn PSSetConstantBuffers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::PSSetConstantBuffers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn IASetInputLayout(&mut self, a0: *mut core::ffi::c_void) {
        self.tag(tag::IASetInputLayout);
        unsafe { self.object(a0) };
    }
    pub unsafe fn IASetVertexBuffers(
        &mut self,
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) {
        self.tag(tag::IASetVertexBuffers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
        unsafe { self.array(a3, a1) };
        unsafe { self.array(a4, a1) };
    }
    pub unsafe fn IASetIndexBuffer(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
        a2: u32,
    ) {
        self.tag(tag::IASetIndexBuffer);
        unsafe { self.object(a0) };
        self.put(a1);
        self.put(a2);
    }
    pub unsafe fn DrawIndexedInstanced(&mut self, a0: u32, a1: u32, a2: u32, a3: i32, a4: u32) {
        self.tag(tag::DrawIndexedInstanced);
        self.put(a0);
        self.put(a1);
        self.put(a2);
        self.put(a3);
        self.put(a4);
    }
    pub unsafe fn DrawInstanced(&mut self, a0: u32, a1: u32, a2: u32, a3: u32) {
        self.tag(tag::DrawInstanced);
        self.put(a0);
        self.put(a1);
        self.put(a2);
        self.put(a3);
    }
    pub unsafe fn GSSetConstantBuffers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::GSSetConstantBuffers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn GSSetShader(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) {
        self.tag(tag::GSSetShader);
        unsafe { self.object(a0) };
        unsafe { self.objects(a1, a2) };
        self.put(a2);
    }
    pub unsafe fn IASetPrimitiveTopology(
        &mut self,
        a0: windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY,
    ) {
        self.tag(tag::IASetPrimitiveTopology);
        self.put(a0);
    }
    pub unsafe fn VSSetShaderResources(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::VSSetShaderResources);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn VSSetSamplers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::VSSetSamplers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn Begin(&mut self, a0: *mut core::ffi::c_void) {
        self.tag(tag::Begin);
        unsafe { self.object(a0) };
    }
    pub unsafe fn End(&mut self, a0: *mut core::ffi::c_void) {
        self.tag(tag::End);
        unsafe { self.object(a0) };
    }
    pub unsafe fn SetPredication(&mut self, a0: *mut core::ffi::c_void, a1: windows::core::BOOL) {
        self.tag(tag::SetPredication);
        unsafe { self.object(a0) };
        self.put(a1);
    }
    pub unsafe fn GSSetShaderResources(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::GSSetShaderResources);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn GSSetSamplers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::GSSetSamplers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn OMSetRenderTargets(
        &mut self,
        a0: u32,
        a1: *const *mut core::ffi::c_void,
        a2: *mut core::ffi::c_void,
    ) {
        self.tag(tag::OMSetRenderTargets);
        self.put(a0);
        unsafe { self.objects(a1, if a0 == u32::MAX { 0 } else { a0 }) };
        unsafe { self.object(a2) };
    }
    pub unsafe fn OMSetRenderTargetsAndUnorderedAccessViews(
        &mut self,
        a0: u32,
        a1: *const *mut core::ffi::c_void,
        a2: *mut core::ffi::c_void,
        a3: u32,
        a4: u32,
        a5: *const *mut core::ffi::c_void,
        a6: *const u32,
    ) {
        self.tag(tag::OMSetRenderTargetsAndUnorderedAccessViews);
        self.put(a0);
        unsafe { self.objects(a1, if a0 == u32::MAX { 0 } else { a0 }) };
        unsafe { self.object(a2) };
        self.put(a3);
        self.put(a4);
        unsafe { self.objects(a5, if a4 == u32::MAX { 0 } else { a4 }) };
        unsafe { self.array(a6, if a4 == u32::MAX { 0 } else { a4 }) };
    }
    pub unsafe fn OMSetBlendState(&mut self, a0: *mut core::ffi::c_void, a1: *const f32, a2: u32) {
        self.tag(tag::OMSetBlendState);
        unsafe { self.object(a0) };
        unsafe { self.array(a1, 4) };
        self.put(a2);
    }
    pub unsafe fn OMSetDepthStencilState(&mut self, a0: *mut core::ffi::c_void, a1: u32) {
        self.tag(tag::OMSetDepthStencilState);
        unsafe { self.object(a0) };
        self.put(a1);
    }
    pub unsafe fn SOSetTargets(&mut self, a0: u32, a1: *const *mut core::ffi::c_void, a2: *const u32) {
        self.tag(tag::SOSetTargets);
        self.put(a0);
        unsafe { self.objects(a1, a0) };
        unsafe { self.array(a2, a0) };
    }
    pub unsafe fn DrawAuto(&mut self) {
        self.tag(tag::DrawAuto);
    }
    pub unsafe fn DrawIndexedInstancedIndirect(&mut self, a0: *mut core::ffi::c_void, a1: u32) {
        self.tag(tag::DrawIndexedInstancedIndirect);
        unsafe { self.object(a0) };
        self.put(a1);
    }
    pub unsafe fn DrawInstancedIndirect(&mut self, a0: *mut core::ffi::c_void, a1: u32) {
        self.tag(tag::DrawInstancedIndirect);
        unsafe { self.object(a0) };
        self.put(a1);
    }
    pub unsafe fn Dispatch(&mut self, a0: u32, a1: u32, a2: u32) {
        self.tag(tag::Dispatch);
        self.put(a0);
        self.put(a1);
        self.put(a2);
    }
    pub unsafe fn DispatchIndirect(&mut self, a0: *mut core::ffi::c_void, a1: u32) {
        self.tag(tag::DispatchIndirect);
        unsafe { self.object(a0) };
        self.put(a1);
    }
    pub unsafe fn RSSetState(&mut self, a0: *mut core::ffi::c_void) {
        self.tag(tag::RSSetState);
        unsafe { self.object(a0) };
    }
    pub unsafe fn RSSetViewports(
        &mut self,
        a0: u32,
        a1: *const windows::Win32::Graphics::Direct3D11::D3D11_VIEWPORT,
    ) {
        self.tag(tag::RSSetViewports);
        self.put(a0);
        unsafe { self.array(a1, a0) };
    }
    pub unsafe fn RSSetScissorRects(&mut self, a0: u32, a1: *const windows::Win32::Foundation::RECT) {
        self.tag(tag::RSSetScissorRects);
        self.put(a0);
        unsafe { self.array(a1, a0) };
    }
    pub unsafe fn CopySubresourceRegion(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: u32,
        a2: u32,
        a3: u32,
        a4: u32,
        a5: *mut core::ffi::c_void,
        a6: u32,
        a7: *const windows::Win32::Graphics::Direct3D11::D3D11_BOX,
    ) {
        self.tag(tag::CopySubresourceRegion);
        unsafe { self.object(a0) };
        self.put(a1);
        self.put(a2);
        self.put(a3);
        self.put(a4);
        unsafe { self.object(a5) };
        self.put(a6);
        unsafe { self.array(a7, 1) };
    }
    pub unsafe fn CopyResource(&mut self, a0: *mut core::ffi::c_void, a1: *mut core::ffi::c_void) {
        self.tag(tag::CopyResource);
        unsafe { self.object(a0) };
        unsafe { self.object(a1) };
    }
    pub unsafe fn CopyStructureCount(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: u32,
        a2: *mut core::ffi::c_void,
    ) {
        self.tag(tag::CopyStructureCount);
        unsafe { self.object(a0) };
        self.put(a1);
        unsafe { self.object(a2) };
    }
    pub unsafe fn ClearRenderTargetView(&mut self, a0: *mut core::ffi::c_void, a1: *const f32) {
        self.tag(tag::ClearRenderTargetView);
        unsafe { self.object(a0) };
        unsafe { self.array(a1, 4) };
    }
    pub unsafe fn ClearUnorderedAccessViewUint(&mut self, a0: *mut core::ffi::c_void, a1: *const u32) {
        self.tag(tag::ClearUnorderedAccessViewUint);
        unsafe { self.object(a0) };
        unsafe { self.array(a1, 4) };
    }
    pub unsafe fn ClearUnorderedAccessViewFloat(&mut self, a0: *mut core::ffi::c_void, a1: *const f32) {
        self.tag(tag::ClearUnorderedAccessViewFloat);
        unsafe { self.object(a0) };
        unsafe { self.array(a1, 4) };
    }
    pub unsafe fn ClearDepthStencilView(&mut self, a0: *mut core::ffi::c_void, a1: u32, a2: f32, a3: u8) {
        self.tag(tag::ClearDepthStencilView);
        unsafe { self.object(a0) };
        self.put(a1);
        self.put(a2);
        self.put(a3);
    }
    pub unsafe fn GenerateMips(&mut self, a0: *mut core::ffi::c_void) {
        self.tag(tag::GenerateMips);
        unsafe { self.object(a0) };
    }
    pub unsafe fn SetResourceMinLOD(&mut self, a0: *mut core::ffi::c_void, a1: f32) {
        self.tag(tag::SetResourceMinLOD);
        unsafe { self.object(a0) };
        self.put(a1);
    }
    pub unsafe fn ResolveSubresource(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: u32,
        a2: *mut core::ffi::c_void,
        a3: u32,
        a4: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    ) {
        self.tag(tag::ResolveSubresource);
        unsafe { self.object(a0) };
        self.put(a1);
        unsafe { self.object(a2) };
        self.put(a3);
        self.put(a4);
    }
    pub unsafe fn HSSetShaderResources(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::HSSetShaderResources);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn HSSetShader(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) {
        self.tag(tag::HSSetShader);
        unsafe { self.object(a0) };
        unsafe { self.objects(a1, a2) };
        self.put(a2);
    }
    pub unsafe fn HSSetSamplers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::HSSetSamplers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn HSSetConstantBuffers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::HSSetConstantBuffers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn DSSetShaderResources(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::DSSetShaderResources);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn DSSetShader(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) {
        self.tag(tag::DSSetShader);
        unsafe { self.object(a0) };
        unsafe { self.objects(a1, a2) };
        self.put(a2);
    }
    pub unsafe fn DSSetSamplers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::DSSetSamplers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn DSSetConstantBuffers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::DSSetConstantBuffers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn CSSetShaderResources(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::CSSetShaderResources);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn CSSetUnorderedAccessViews(
        &mut self,
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
    ) {
        self.tag(tag::CSSetUnorderedAccessViews);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
        unsafe { self.array(a3, a1) };
    }
    pub unsafe fn CSSetShader(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: *const *mut core::ffi::c_void,
        a2: u32,
    ) {
        self.tag(tag::CSSetShader);
        unsafe { self.object(a0) };
        unsafe { self.objects(a1, a2) };
        self.put(a2);
    }
    pub unsafe fn CSSetSamplers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::CSSetSamplers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn CSSetConstantBuffers(&mut self, a0: u32, a1: u32, a2: *const *mut core::ffi::c_void) {
        self.tag(tag::CSSetConstantBuffers);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
    }
    pub unsafe fn ClearState(&mut self) {
        self.tag(tag::ClearState);
    }
    pub unsafe fn CopySubresourceRegion1(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: u32,
        a2: u32,
        a3: u32,
        a4: u32,
        a5: *mut core::ffi::c_void,
        a6: u32,
        a7: *const windows::Win32::Graphics::Direct3D11::D3D11_BOX,
        a8: u32,
    ) {
        self.tag(tag::CopySubresourceRegion1);
        unsafe { self.object(a0) };
        self.put(a1);
        self.put(a2);
        self.put(a3);
        self.put(a4);
        unsafe { self.object(a5) };
        self.put(a6);
        unsafe { self.array(a7, 1) };
        self.put(a8);
    }
    pub unsafe fn DiscardResource(&mut self, a0: *mut core::ffi::c_void) {
        self.tag(tag::DiscardResource);
        unsafe { self.object(a0) };
    }
    pub unsafe fn DiscardView(&mut self, a0: *mut core::ffi::c_void) {
        self.tag(tag::DiscardView);
        unsafe { self.object(a0) };
    }
    pub unsafe fn VSSetConstantBuffers1(
        &mut self,
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) {
        self.tag(tag::VSSetConstantBuffers1);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
        unsafe { self.array(a3, a1) };
        unsafe { self.array(a4, a1) };
    }
    pub unsafe fn HSSetConstantBuffers1(
        &mut self,
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) {
        self.tag(tag::HSSetConstantBuffers1);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
        unsafe { self.array(a3, a1) };
        unsafe { self.array(a4, a1) };
    }
    pub unsafe fn DSSetConstantBuffers1(
        &mut self,
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) {
        self.tag(tag::DSSetConstantBuffers1);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
        unsafe { self.array(a3, a1) };
        unsafe { self.array(a4, a1) };
    }
    pub unsafe fn GSSetConstantBuffers1(
        &mut self,
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) {
        self.tag(tag::GSSetConstantBuffers1);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
        unsafe { self.array(a3, a1) };
        unsafe { self.array(a4, a1) };
    }
    pub unsafe fn PSSetConstantBuffers1(
        &mut self,
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) {
        self.tag(tag::PSSetConstantBuffers1);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
        unsafe { self.array(a3, a1) };
        unsafe { self.array(a4, a1) };
    }
    pub unsafe fn CSSetConstantBuffers1(
        &mut self,
        a0: u32,
        a1: u32,
        a2: *const *mut core::ffi::c_void,
        a3: *const u32,
        a4: *const u32,
    ) {
        self.tag(tag::CSSetConstantBuffers1);
        self.put(a0);
        self.put(a1);
        unsafe { self.objects(a2, a1) };
        unsafe { self.array(a3, a1) };
        unsafe { self.array(a4, a1) };
    }
    pub unsafe fn ClearView(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: *const f32,
        a2: *const windows::Win32::Foundation::RECT,
        a3: u32,
    ) {
        self.tag(tag::ClearView);
        unsafe { self.object(a0) };
        unsafe { self.array(a1, 4) };
        unsafe { self.array(a2, a3) };
        self.put(a3);
    }
    pub unsafe fn DiscardView1(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: *const windows::Win32::Foundation::RECT,
        a2: u32,
    ) {
        self.tag(tag::DiscardView1);
        unsafe { self.object(a0) };
        unsafe { self.array(a1, a2) };
        self.put(a2);
    }
    pub unsafe fn UpdateTileMappings(
        &mut self,
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
    ) {
        self.tag(tag::UpdateTileMappings);
        unsafe { self.object(a0) };
        self.put(a1);
        unsafe { self.array(a2, a1) };
        unsafe { self.array(a3, a1) };
        unsafe { self.object(a4) };
        self.put(a5);
        unsafe { self.array(a6, a5) };
        unsafe { self.array(a7, a5) };
        unsafe { self.array(a8, a5) };
        self.put(a9);
    }
    pub unsafe fn CopyTileMappings(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE,
        a2: *mut core::ffi::c_void,
        a3: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE,
        a4: *const windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE,
        a5: u32,
    ) {
        self.tag(tag::CopyTileMappings);
        unsafe { self.object(a0) };
        unsafe { self.array(a1, 1) };
        unsafe { self.object(a2) };
        unsafe { self.array(a3, 1) };
        unsafe { self.array(a4, 1) };
        self.put(a5);
    }
    pub unsafe fn CopyTiles(
        &mut self,
        a0: *mut core::ffi::c_void,
        a1: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE,
        a2: *const windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE,
        a3: *mut core::ffi::c_void,
        a4: u64,
        a5: u32,
    ) {
        self.tag(tag::CopyTiles);
        unsafe { self.object(a0) };
        unsafe { self.array(a1, 1) };
        unsafe { self.array(a2, 1) };
        unsafe { self.object(a3) };
        self.put(a4);
        self.put(a5);
    }
    pub unsafe fn ResizeTilePool(&mut self, a0: *mut core::ffi::c_void, a1: u64) {
        self.tag(tag::ResizeTilePool);
        unsafe { self.object(a0) };
        self.put(a1);
    }
    pub unsafe fn TiledResourceBarrier(&mut self, a0: *mut core::ffi::c_void, a1: *mut core::ffi::c_void) {
        self.tag(tag::TiledResourceBarrier);
        unsafe { self.object(a0) };
        unsafe { self.object(a1) };
    }
}
/// Replays the call tagged `tag`, its arguments read from `r`.
pub unsafe fn call(tag: u32, r: &mut Reader, context: *mut c_void) -> windows::core::Result<()> {
    match tag {
        tag::VSSetConstantBuffers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::VSSetConstantBuffers(context, a0, a1, a2) };
            Ok(())
        }
        tag::PSSetShaderResources => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::PSSetShaderResources(context, a0, a1, a2) };
            Ok(())
        }
        tag::PSSetShader => {
            let a0 = r.object();
            let a1 = r.objects();
            let a2: u32 = r.get();
            unsafe { crate::vtables::context::real::PSSetShader(context, a0, a1, a2) };
            Ok(())
        }
        tag::PSSetSamplers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::PSSetSamplers(context, a0, a1, a2) };
            Ok(())
        }
        tag::VSSetShader => {
            let a0 = r.object();
            let a1 = r.objects();
            let a2: u32 = r.get();
            unsafe { crate::vtables::context::real::VSSetShader(context, a0, a1, a2) };
            Ok(())
        }
        tag::DrawIndexed => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2: i32 = r.get();
            unsafe { crate::vtables::context::real::DrawIndexed(context, a0, a1, a2) };
            Ok(())
        }
        tag::Draw => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            unsafe { crate::vtables::context::real::Draw(context, a0, a1) };
            Ok(())
        }
        tag::PSSetConstantBuffers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::PSSetConstantBuffers(context, a0, a1, a2) };
            Ok(())
        }
        tag::IASetInputLayout => {
            let a0 = r.object();
            unsafe { crate::vtables::context::real::IASetInputLayout(context, a0) };
            Ok(())
        }
        tag::IASetVertexBuffers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            let a3: *const u32 = r.array();
            let a4: *const u32 = r.array();
            unsafe { crate::vtables::context::real::IASetVertexBuffers(context, a0, a1, a2, a3, a4) };
            Ok(())
        }
        tag::IASetIndexBuffer => {
            let a0 = r.object();
            let a1: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT = r.get();
            let a2: u32 = r.get();
            unsafe { crate::vtables::context::real::IASetIndexBuffer(context, a0, a1, a2) };
            Ok(())
        }
        tag::DrawIndexedInstanced => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2: u32 = r.get();
            let a3: i32 = r.get();
            let a4: u32 = r.get();
            unsafe { crate::vtables::context::real::DrawIndexedInstanced(context, a0, a1, a2, a3, a4) };
            Ok(())
        }
        tag::DrawInstanced => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2: u32 = r.get();
            let a3: u32 = r.get();
            unsafe { crate::vtables::context::real::DrawInstanced(context, a0, a1, a2, a3) };
            Ok(())
        }
        tag::GSSetConstantBuffers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::GSSetConstantBuffers(context, a0, a1, a2) };
            Ok(())
        }
        tag::GSSetShader => {
            let a0 = r.object();
            let a1 = r.objects();
            let a2: u32 = r.get();
            unsafe { crate::vtables::context::real::GSSetShader(context, a0, a1, a2) };
            Ok(())
        }
        tag::IASetPrimitiveTopology => {
            let a0: windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY = r.get();
            unsafe { crate::vtables::context::real::IASetPrimitiveTopology(context, a0) };
            Ok(())
        }
        tag::VSSetShaderResources => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::VSSetShaderResources(context, a0, a1, a2) };
            Ok(())
        }
        tag::VSSetSamplers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::VSSetSamplers(context, a0, a1, a2) };
            Ok(())
        }
        tag::Begin => {
            let a0 = r.object();
            unsafe { crate::vtables::context::real::Begin(context, a0) };
            Ok(())
        }
        tag::End => {
            let a0 = r.object();
            unsafe { crate::vtables::context::real::End(context, a0) };
            Ok(())
        }
        tag::SetPredication => {
            let a0 = r.object();
            let a1: windows::core::BOOL = r.get();
            unsafe { crate::vtables::context::real::SetPredication(context, a0, a1) };
            Ok(())
        }
        tag::GSSetShaderResources => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::GSSetShaderResources(context, a0, a1, a2) };
            Ok(())
        }
        tag::GSSetSamplers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::GSSetSamplers(context, a0, a1, a2) };
            Ok(())
        }
        tag::OMSetRenderTargets => {
            let a0: u32 = r.get();
            let a1 = r.objects();
            let a2 = r.object();
            unsafe { crate::vtables::context::real::OMSetRenderTargets(context, a0, a1, a2) };
            Ok(())
        }
        tag::OMSetRenderTargetsAndUnorderedAccessViews => {
            let a0: u32 = r.get();
            let a1 = r.objects();
            let a2 = r.object();
            let a3: u32 = r.get();
            let a4: u32 = r.get();
            let a5 = r.objects();
            let a6: *const u32 = r.array();
            unsafe {
                crate::vtables::context::real::OMSetRenderTargetsAndUnorderedAccessViews(
                    context, a0, a1, a2, a3, a4, a5, a6,
                )
            };
            Ok(())
        }
        tag::OMSetBlendState => {
            let a0 = r.object();
            let a1: *const f32 = r.array();
            let a2: u32 = r.get();
            unsafe { crate::vtables::context::real::OMSetBlendState(context, a0, a1, a2) };
            Ok(())
        }
        tag::OMSetDepthStencilState => {
            let a0 = r.object();
            let a1: u32 = r.get();
            unsafe { crate::vtables::context::real::OMSetDepthStencilState(context, a0, a1) };
            Ok(())
        }
        tag::SOSetTargets => {
            let a0: u32 = r.get();
            let a1 = r.objects();
            let a2: *const u32 = r.array();
            unsafe { crate::vtables::context::real::SOSetTargets(context, a0, a1, a2) };
            Ok(())
        }
        tag::DrawAuto => {
            unsafe { crate::vtables::context::real::DrawAuto(context) };
            Ok(())
        }
        tag::DrawIndexedInstancedIndirect => {
            let a0 = r.object();
            let a1: u32 = r.get();
            unsafe { crate::vtables::context::real::DrawIndexedInstancedIndirect(context, a0, a1) };
            Ok(())
        }
        tag::DrawInstancedIndirect => {
            let a0 = r.object();
            let a1: u32 = r.get();
            unsafe { crate::vtables::context::real::DrawInstancedIndirect(context, a0, a1) };
            Ok(())
        }
        tag::Dispatch => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2: u32 = r.get();
            unsafe { crate::vtables::context::real::Dispatch(context, a0, a1, a2) };
            Ok(())
        }
        tag::DispatchIndirect => {
            let a0 = r.object();
            let a1: u32 = r.get();
            unsafe { crate::vtables::context::real::DispatchIndirect(context, a0, a1) };
            Ok(())
        }
        tag::RSSetState => {
            let a0 = r.object();
            unsafe { crate::vtables::context::real::RSSetState(context, a0) };
            Ok(())
        }
        tag::RSSetViewports => {
            let a0: u32 = r.get();
            let a1: *const windows::Win32::Graphics::Direct3D11::D3D11_VIEWPORT = r.array();
            unsafe { crate::vtables::context::real::RSSetViewports(context, a0, a1) };
            Ok(())
        }
        tag::RSSetScissorRects => {
            let a0: u32 = r.get();
            let a1: *const windows::Win32::Foundation::RECT = r.array();
            unsafe { crate::vtables::context::real::RSSetScissorRects(context, a0, a1) };
            Ok(())
        }
        tag::CopySubresourceRegion => {
            let a0 = r.object();
            let a1: u32 = r.get();
            let a2: u32 = r.get();
            let a3: u32 = r.get();
            let a4: u32 = r.get();
            let a5 = r.object();
            let a6: u32 = r.get();
            let a7: *const windows::Win32::Graphics::Direct3D11::D3D11_BOX = r.array();
            unsafe {
                crate::vtables::context::real::CopySubresourceRegion(context, a0, a1, a2, a3, a4, a5, a6, a7)
            };
            Ok(())
        }
        tag::CopyResource => {
            let a0 = r.object();
            let a1 = r.object();
            unsafe { crate::vtables::context::real::CopyResource(context, a0, a1) };
            Ok(())
        }
        tag::CopyStructureCount => {
            let a0 = r.object();
            let a1: u32 = r.get();
            let a2 = r.object();
            unsafe { crate::vtables::context::real::CopyStructureCount(context, a0, a1, a2) };
            Ok(())
        }
        tag::ClearRenderTargetView => {
            let a0 = r.object();
            let a1: *const f32 = r.array();
            unsafe { crate::vtables::context::real::ClearRenderTargetView(context, a0, a1) };
            Ok(())
        }
        tag::ClearUnorderedAccessViewUint => {
            let a0 = r.object();
            let a1: *const u32 = r.array();
            unsafe { crate::vtables::context::real::ClearUnorderedAccessViewUint(context, a0, a1) };
            Ok(())
        }
        tag::ClearUnorderedAccessViewFloat => {
            let a0 = r.object();
            let a1: *const f32 = r.array();
            unsafe { crate::vtables::context::real::ClearUnorderedAccessViewFloat(context, a0, a1) };
            Ok(())
        }
        tag::ClearDepthStencilView => {
            let a0 = r.object();
            let a1: u32 = r.get();
            let a2: f32 = r.get();
            let a3: u8 = r.get();
            unsafe { crate::vtables::context::real::ClearDepthStencilView(context, a0, a1, a2, a3) };
            Ok(())
        }
        tag::GenerateMips => {
            let a0 = r.object();
            unsafe { crate::vtables::context::real::GenerateMips(context, a0) };
            Ok(())
        }
        tag::SetResourceMinLOD => {
            let a0 = r.object();
            let a1: f32 = r.get();
            unsafe { crate::vtables::context::real::SetResourceMinLOD(context, a0, a1) };
            Ok(())
        }
        tag::ResolveSubresource => {
            let a0 = r.object();
            let a1: u32 = r.get();
            let a2 = r.object();
            let a3: u32 = r.get();
            let a4: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT = r.get();
            unsafe { crate::vtables::context::real::ResolveSubresource(context, a0, a1, a2, a3, a4) };
            Ok(())
        }
        tag::HSSetShaderResources => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::HSSetShaderResources(context, a0, a1, a2) };
            Ok(())
        }
        tag::HSSetShader => {
            let a0 = r.object();
            let a1 = r.objects();
            let a2: u32 = r.get();
            unsafe { crate::vtables::context::real::HSSetShader(context, a0, a1, a2) };
            Ok(())
        }
        tag::HSSetSamplers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::HSSetSamplers(context, a0, a1, a2) };
            Ok(())
        }
        tag::HSSetConstantBuffers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::HSSetConstantBuffers(context, a0, a1, a2) };
            Ok(())
        }
        tag::DSSetShaderResources => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::DSSetShaderResources(context, a0, a1, a2) };
            Ok(())
        }
        tag::DSSetShader => {
            let a0 = r.object();
            let a1 = r.objects();
            let a2: u32 = r.get();
            unsafe { crate::vtables::context::real::DSSetShader(context, a0, a1, a2) };
            Ok(())
        }
        tag::DSSetSamplers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::DSSetSamplers(context, a0, a1, a2) };
            Ok(())
        }
        tag::DSSetConstantBuffers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::DSSetConstantBuffers(context, a0, a1, a2) };
            Ok(())
        }
        tag::CSSetShaderResources => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::CSSetShaderResources(context, a0, a1, a2) };
            Ok(())
        }
        tag::CSSetUnorderedAccessViews => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            let a3: *const u32 = r.array();
            unsafe { crate::vtables::context::real::CSSetUnorderedAccessViews(context, a0, a1, a2, a3) };
            Ok(())
        }
        tag::CSSetShader => {
            let a0 = r.object();
            let a1 = r.objects();
            let a2: u32 = r.get();
            unsafe { crate::vtables::context::real::CSSetShader(context, a0, a1, a2) };
            Ok(())
        }
        tag::CSSetSamplers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::CSSetSamplers(context, a0, a1, a2) };
            Ok(())
        }
        tag::CSSetConstantBuffers => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            unsafe { crate::vtables::context::real::CSSetConstantBuffers(context, a0, a1, a2) };
            Ok(())
        }
        tag::ClearState => {
            unsafe { crate::vtables::context::real::ClearState(context) };
            Ok(())
        }
        tag::CopySubresourceRegion1 => {
            let a0 = r.object();
            let a1: u32 = r.get();
            let a2: u32 = r.get();
            let a3: u32 = r.get();
            let a4: u32 = r.get();
            let a5 = r.object();
            let a6: u32 = r.get();
            let a7: *const windows::Win32::Graphics::Direct3D11::D3D11_BOX = r.array();
            let a8: u32 = r.get();
            unsafe {
                crate::vtables::context::real::CopySubresourceRegion1(
                    context, a0, a1, a2, a3, a4, a5, a6, a7, a8,
                )
            };
            Ok(())
        }
        tag::DiscardResource => {
            let a0 = r.object();
            unsafe { crate::vtables::context::real::DiscardResource(context, a0) };
            Ok(())
        }
        tag::DiscardView => {
            let a0 = r.object();
            unsafe { crate::vtables::context::real::DiscardView(context, a0) };
            Ok(())
        }
        tag::VSSetConstantBuffers1 => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            let a3: *const u32 = r.array();
            let a4: *const u32 = r.array();
            unsafe { crate::vtables::context::real::VSSetConstantBuffers1(context, a0, a1, a2, a3, a4) };
            Ok(())
        }
        tag::HSSetConstantBuffers1 => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            let a3: *const u32 = r.array();
            let a4: *const u32 = r.array();
            unsafe { crate::vtables::context::real::HSSetConstantBuffers1(context, a0, a1, a2, a3, a4) };
            Ok(())
        }
        tag::DSSetConstantBuffers1 => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            let a3: *const u32 = r.array();
            let a4: *const u32 = r.array();
            unsafe { crate::vtables::context::real::DSSetConstantBuffers1(context, a0, a1, a2, a3, a4) };
            Ok(())
        }
        tag::GSSetConstantBuffers1 => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            let a3: *const u32 = r.array();
            let a4: *const u32 = r.array();
            unsafe { crate::vtables::context::real::GSSetConstantBuffers1(context, a0, a1, a2, a3, a4) };
            Ok(())
        }
        tag::PSSetConstantBuffers1 => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            let a3: *const u32 = r.array();
            let a4: *const u32 = r.array();
            unsafe { crate::vtables::context::real::PSSetConstantBuffers1(context, a0, a1, a2, a3, a4) };
            Ok(())
        }
        tag::CSSetConstantBuffers1 => {
            let a0: u32 = r.get();
            let a1: u32 = r.get();
            let a2 = r.objects();
            let a3: *const u32 = r.array();
            let a4: *const u32 = r.array();
            unsafe { crate::vtables::context::real::CSSetConstantBuffers1(context, a0, a1, a2, a3, a4) };
            Ok(())
        }
        tag::ClearView => {
            let a0 = r.object();
            let a1: *const f32 = r.array();
            let a2: *const windows::Win32::Foundation::RECT = r.array();
            let a3: u32 = r.get();
            unsafe { crate::vtables::context::real::ClearView(context, a0, a1, a2, a3) };
            Ok(())
        }
        tag::DiscardView1 => {
            let a0 = r.object();
            let a1: *const windows::Win32::Foundation::RECT = r.array();
            let a2: u32 = r.get();
            unsafe { crate::vtables::context::real::DiscardView1(context, a0, a1, a2) };
            Ok(())
        }
        tag::UpdateTileMappings => {
            let a0 = r.object();
            let a1: u32 = r.get();
            let a2: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE = r.array();
            let a3: *const windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE = r.array();
            let a4 = r.object();
            let a5: u32 = r.get();
            let a6: *const u32 = r.array();
            let a7: *const u32 = r.array();
            let a8: *const u32 = r.array();
            let a9: u32 = r.get();
            unsafe {
                crate::vtables::context::real::UpdateTileMappings(
                    context, a0, a1, a2, a3, a4, a5, a6, a7, a8, a9,
                )
            }
            .ok()
        }
        tag::CopyTileMappings => {
            let a0 = r.object();
            let a1: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE = r.array();
            let a2 = r.object();
            let a3: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE = r.array();
            let a4: *const windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE = r.array();
            let a5: u32 = r.get();
            unsafe { crate::vtables::context::real::CopyTileMappings(context, a0, a1, a2, a3, a4, a5) }.ok()
        }
        tag::CopyTiles => {
            let a0 = r.object();
            let a1: *const windows::Win32::Graphics::Direct3D11::D3D11_TILED_RESOURCE_COORDINATE = r.array();
            let a2: *const windows::Win32::Graphics::Direct3D11::D3D11_TILE_REGION_SIZE = r.array();
            let a3 = r.object();
            let a4: u64 = r.get();
            let a5: u32 = r.get();
            unsafe { crate::vtables::context::real::CopyTiles(context, a0, a1, a2, a3, a4, a5) };
            Ok(())
        }
        tag::ResizeTilePool => {
            let a0 = r.object();
            let a1: u64 = r.get();
            unsafe { crate::vtables::context::real::ResizeTilePool(context, a0, a1) }.ok()
        }
        tag::TiledResourceBarrier => {
            let a0 = r.object();
            let a1 = r.object();
            unsafe { crate::vtables::context::real::TiledResourceBarrier(context, a0, a1) };
            Ok(())
        }
        _ => unreachable!("tag {tag} is no recorded call"),
    }
}
