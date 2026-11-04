//! Generated field encoders from windows metadata by cargo xtask gen.
#![allow(non_snake_case, dead_code)]
use serde_json::{Value, json};
use windows::Win32::Graphics::Direct3D12::*;
pub trait Portable {
    fn portable(&self) -> Value;
}
impl<T: Portable, const N: usize> Portable for [T; N] {
    fn portable(&self) -> Value {
        Value::Array(self.iter().map(Portable::portable).collect())
    }
}
impl Portable for u8 {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for i8 {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for u16 {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for i16 {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for u32 {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for i32 {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for u64 {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for i64 {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for f32 {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for f64 {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for bool {
    fn portable(&self) -> Value {
        json!(self)
    }
}
impl Portable for windows::core::BOOL {
    fn portable(&self) -> Value {
        json!(self.as_bool())
    }
}
impl Portable for windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_TEX2DMS_ARRAY_DSV {
    fn portable(&self) -> Value {
        json!({
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        })
    }
}
impl Portable for D3D12_TEX2DMS_DSV {
    fn portable(&self) -> Value {
        json!({
        "UnusedField_NothingToDefine":self.UnusedField_NothingToDefine.portable(),
        })
    }
}
impl Portable for D3D12_TEX2D_ARRAY_DSV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        })
    }
}
impl Portable for D3D12_TEX2D_DSV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        })
    }
}
impl Portable for D3D12_TEX1D_ARRAY_DSV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        })
    }
}
impl Portable for D3D12_TEX1D_DSV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        })
    }
}
impl Portable for D3D12_TEX3D_RTV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        "FirstWSlice":self.FirstWSlice.portable(),
        "WSize":self.WSize.portable(),
        })
    }
}
impl Portable for D3D12_TEX2DMS_ARRAY_RTV {
    fn portable(&self) -> Value {
        json!({
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        })
    }
}
impl Portable for D3D12_TEX2DMS_RTV {
    fn portable(&self) -> Value {
        json!({
        "UnusedField_NothingToDefine":self.UnusedField_NothingToDefine.portable(),
        })
    }
}
impl Portable for D3D12_TEX2D_ARRAY_RTV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        "PlaneSlice":self.PlaneSlice.portable(),
        })
    }
}
impl Portable for D3D12_TEX2D_RTV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        "PlaneSlice":self.PlaneSlice.portable(),
        })
    }
}
impl Portable for D3D12_TEX1D_ARRAY_RTV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        })
    }
}
impl Portable for D3D12_TEX1D_RTV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        })
    }
}
impl Portable for D3D12_BUFFER_RTV {
    fn portable(&self) -> Value {
        json!({
        "FirstElement":self.FirstElement.portable(),
        "NumElements":self.NumElements.portable(),
        })
    }
}
impl Portable for D3D12_TEX3D_UAV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        "FirstWSlice":self.FirstWSlice.portable(),
        "WSize":self.WSize.portable(),
        })
    }
}
impl Portable for D3D12_TEX2D_ARRAY_UAV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        "PlaneSlice":self.PlaneSlice.portable(),
        })
    }
}
impl Portable for D3D12_TEX2D_UAV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        "PlaneSlice":self.PlaneSlice.portable(),
        })
    }
}
impl Portable for D3D12_TEX1D_ARRAY_UAV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        })
    }
}
impl Portable for D3D12_TEX1D_UAV {
    fn portable(&self) -> Value {
        json!({
        "MipSlice":self.MipSlice.portable(),
        })
    }
}
impl Portable for D3D12_BUFFER_UAV {
    fn portable(&self) -> Value {
        json!({
        "FirstElement":self.FirstElement.portable(),
        "NumElements":self.NumElements.portable(),
        "StructureByteStride":self.StructureByteStride.portable(),
        "CounterOffsetInBytes":self.CounterOffsetInBytes.portable(),
        "Flags":self.Flags.portable(),
        })
    }
}
impl Portable for D3D12_BUFFER_UAV_FLAGS {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_TEXCUBE_ARRAY_SRV {
    fn portable(&self) -> Value {
        json!({
        "MostDetailedMip":self.MostDetailedMip.portable(),
        "MipLevels":self.MipLevels.portable(),
        "First2DArrayFace":self.First2DArrayFace.portable(),
        "NumCubes":self.NumCubes.portable(),
        "ResourceMinLODClamp":self.ResourceMinLODClamp.portable(),
        })
    }
}
impl Portable for D3D12_TEXCUBE_SRV {
    fn portable(&self) -> Value {
        json!({
        "MostDetailedMip":self.MostDetailedMip.portable(),
        "MipLevels":self.MipLevels.portable(),
        "ResourceMinLODClamp":self.ResourceMinLODClamp.portable(),
        })
    }
}
impl Portable for D3D12_TEX3D_SRV {
    fn portable(&self) -> Value {
        json!({
        "MostDetailedMip":self.MostDetailedMip.portable(),
        "MipLevels":self.MipLevels.portable(),
        "ResourceMinLODClamp":self.ResourceMinLODClamp.portable(),
        })
    }
}
impl Portable for D3D12_TEX2DMS_ARRAY_SRV {
    fn portable(&self) -> Value {
        json!({
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        })
    }
}
impl Portable for D3D12_TEX2DMS_SRV {
    fn portable(&self) -> Value {
        json!({
        "UnusedField_NothingToDefine":self.UnusedField_NothingToDefine.portable(),
        })
    }
}
impl Portable for D3D12_TEX2D_ARRAY_SRV {
    fn portable(&self) -> Value {
        json!({
        "MostDetailedMip":self.MostDetailedMip.portable(),
        "MipLevels":self.MipLevels.portable(),
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        "PlaneSlice":self.PlaneSlice.portable(),
        "ResourceMinLODClamp":self.ResourceMinLODClamp.portable(),
        })
    }
}
impl Portable for D3D12_TEX2D_SRV {
    fn portable(&self) -> Value {
        json!({
        "MostDetailedMip":self.MostDetailedMip.portable(),
        "MipLevels":self.MipLevels.portable(),
        "PlaneSlice":self.PlaneSlice.portable(),
        "ResourceMinLODClamp":self.ResourceMinLODClamp.portable(),
        })
    }
}
impl Portable for D3D12_TEX1D_ARRAY_SRV {
    fn portable(&self) -> Value {
        json!({
        "MostDetailedMip":self.MostDetailedMip.portable(),
        "MipLevels":self.MipLevels.portable(),
        "FirstArraySlice":self.FirstArraySlice.portable(),
        "ArraySize":self.ArraySize.portable(),
        "ResourceMinLODClamp":self.ResourceMinLODClamp.portable(),
        })
    }
}
impl Portable for D3D12_TEX1D_SRV {
    fn portable(&self) -> Value {
        json!({
        "MostDetailedMip":self.MostDetailedMip.portable(),
        "MipLevels":self.MipLevels.portable(),
        "ResourceMinLODClamp":self.ResourceMinLODClamp.portable(),
        })
    }
}
impl Portable for D3D12_BUFFER_SRV {
    fn portable(&self) -> Value {
        json!({
        "FirstElement":self.FirstElement.portable(),
        "NumElements":self.NumElements.portable(),
        "StructureByteStride":self.StructureByteStride.portable(),
        "Flags":self.Flags.portable(),
        })
    }
}
impl Portable for D3D12_BUFFER_SRV_FLAGS {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_ROOT_DESCRIPTOR1 {
    fn portable(&self) -> Value {
        json!({
        "ShaderRegister":self.ShaderRegister.portable(),
        "RegisterSpace":self.RegisterSpace.portable(),
        "Flags":self.Flags.portable(),
        })
    }
}
impl Portable for D3D12_ROOT_DESCRIPTOR_FLAGS {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_ROOT_CONSTANTS {
    fn portable(&self) -> Value {
        json!({
        "ShaderRegister":self.ShaderRegister.portable(),
        "RegisterSpace":self.RegisterSpace.portable(),
        "Num32BitValues":self.Num32BitValues.portable(),
        })
    }
}
impl Portable for D3D12_DESCRIPTOR_RANGE1 {
    fn portable(&self) -> Value {
        json!({
        "RangeType":self.RangeType.portable(),
        "NumDescriptors":self.NumDescriptors.portable(),
        "BaseShaderRegister":self.BaseShaderRegister.portable(),
        "RegisterSpace":self.RegisterSpace.portable(),
        "Flags":self.Flags.portable(),
        "OffsetInDescriptorsFromTableStart":self.OffsetInDescriptorsFromTableStart.portable(),
        })
    }
}
impl Portable for D3D12_DESCRIPTOR_RANGE_FLAGS {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_DESCRIPTOR_RANGE_TYPE {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_SAMPLER_DESC {
    fn portable(&self) -> Value {
        json!({
        "Filter":self.Filter.portable(),
        "AddressU":self.AddressU.portable(),
        "AddressV":self.AddressV.portable(),
        "AddressW":self.AddressW.portable(),
        "MipLODBias":self.MipLODBias.portable(),
        "MaxAnisotropy":self.MaxAnisotropy.portable(),
        "ComparisonFunc":self.ComparisonFunc.portable(),
        "BorderColor":self.BorderColor.portable(),
        "MinLOD":self.MinLOD.portable(),
        "MaxLOD":self.MaxLOD.portable(),
        })
    }
}
impl Portable for D3D12_COMPARISON_FUNC {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_TEXTURE_ADDRESS_MODE {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_FILTER {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_STATIC_SAMPLER_DESC1 {
    fn portable(&self) -> Value {
        json!({
        "Filter":self.Filter.portable(),
        "AddressU":self.AddressU.portable(),
        "AddressV":self.AddressV.portable(),
        "AddressW":self.AddressW.portable(),
        "MipLODBias":self.MipLODBias.portable(),
        "MaxAnisotropy":self.MaxAnisotropy.portable(),
        "ComparisonFunc":self.ComparisonFunc.portable(),
        "BorderColor":self.BorderColor.portable(),
        "MinLOD":self.MinLOD.portable(),
        "MaxLOD":self.MaxLOD.portable(),
        "ShaderRegister":self.ShaderRegister.portable(),
        "RegisterSpace":self.RegisterSpace.portable(),
        "ShaderVisibility":self.ShaderVisibility.portable(),
        "Flags":self.Flags.portable(),
        })
    }
}
impl Portable for D3D12_SAMPLER_FLAGS {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_SHADER_VISIBILITY {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_STATIC_BORDER_COLOR {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_STATIC_SAMPLER_DESC {
    fn portable(&self) -> Value {
        json!({
        "Filter":self.Filter.portable(),
        "AddressU":self.AddressU.portable(),
        "AddressV":self.AddressV.portable(),
        "AddressW":self.AddressW.portable(),
        "MipLODBias":self.MipLODBias.portable(),
        "MaxAnisotropy":self.MaxAnisotropy.portable(),
        "ComparisonFunc":self.ComparisonFunc.portable(),
        "BorderColor":self.BorderColor.portable(),
        "MinLOD":self.MinLOD.portable(),
        "MaxLOD":self.MaxLOD.portable(),
        "ShaderRegister":self.ShaderRegister.portable(),
        "RegisterSpace":self.RegisterSpace.portable(),
        "ShaderVisibility":self.ShaderVisibility.portable(),
        })
    }
}
impl Portable for D3D12_RT_FORMAT_ARRAY {
    fn portable(&self) -> Value {
        json!({
        "RTFormats":self.RTFormats.portable(),
        "NumRenderTargets":self.NumRenderTargets.portable(),
        })
    }
}
impl Portable for D3D12_DEPTH_STENCIL_DESC2 {
    fn portable(&self) -> Value {
        json!({
        "DepthEnable":self.DepthEnable.portable(),
        "DepthWriteMask":self.DepthWriteMask.portable(),
        "DepthFunc":self.DepthFunc.portable(),
        "StencilEnable":self.StencilEnable.portable(),
        "FrontFace":self.FrontFace.portable(),
        "BackFace":self.BackFace.portable(),
        "DepthBoundsTestEnable":self.DepthBoundsTestEnable.portable(),
        })
    }
}
impl Portable for D3D12_DEPTH_STENCILOP_DESC1 {
    fn portable(&self) -> Value {
        json!({
        "StencilFailOp":self.StencilFailOp.portable(),
        "StencilDepthFailOp":self.StencilDepthFailOp.portable(),
        "StencilPassOp":self.StencilPassOp.portable(),
        "StencilFunc":self.StencilFunc.portable(),
        "StencilReadMask":self.StencilReadMask.portable(),
        "StencilWriteMask":self.StencilWriteMask.portable(),
        })
    }
}
impl Portable for D3D12_STENCIL_OP {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_DEPTH_WRITE_MASK {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_DEPTH_STENCIL_DESC1 {
    fn portable(&self) -> Value {
        json!({
        "DepthEnable":self.DepthEnable.portable(),
        "DepthWriteMask":self.DepthWriteMask.portable(),
        "DepthFunc":self.DepthFunc.portable(),
        "StencilEnable":self.StencilEnable.portable(),
        "StencilReadMask":self.StencilReadMask.portable(),
        "StencilWriteMask":self.StencilWriteMask.portable(),
        "FrontFace":self.FrontFace.portable(),
        "BackFace":self.BackFace.portable(),
        "DepthBoundsTestEnable":self.DepthBoundsTestEnable.portable(),
        })
    }
}
impl Portable for D3D12_DEPTH_STENCILOP_DESC {
    fn portable(&self) -> Value {
        json!({
        "StencilFailOp":self.StencilFailOp.portable(),
        "StencilDepthFailOp":self.StencilDepthFailOp.portable(),
        "StencilPassOp":self.StencilPassOp.portable(),
        "StencilFunc":self.StencilFunc.portable(),
        })
    }
}
impl Portable for D3D12_DEPTH_STENCIL_DESC {
    fn portable(&self) -> Value {
        json!({
        "DepthEnable":self.DepthEnable.portable(),
        "DepthWriteMask":self.DepthWriteMask.portable(),
        "DepthFunc":self.DepthFunc.portable(),
        "StencilEnable":self.StencilEnable.portable(),
        "StencilReadMask":self.StencilReadMask.portable(),
        "StencilWriteMask":self.StencilWriteMask.portable(),
        "FrontFace":self.FrontFace.portable(),
        "BackFace":self.BackFace.portable(),
        })
    }
}
impl Portable for D3D12_RASTERIZER_DESC2 {
    fn portable(&self) -> Value {
        json!({
        "FillMode":self.FillMode.portable(),
        "CullMode":self.CullMode.portable(),
        "FrontCounterClockwise":self.FrontCounterClockwise.portable(),
        "DepthBias":self.DepthBias.portable(),
        "DepthBiasClamp":self.DepthBiasClamp.portable(),
        "SlopeScaledDepthBias":self.SlopeScaledDepthBias.portable(),
        "DepthClipEnable":self.DepthClipEnable.portable(),
        "LineRasterizationMode":self.LineRasterizationMode.portable(),
        "ForcedSampleCount":self.ForcedSampleCount.portable(),
        "ConservativeRaster":self.ConservativeRaster.portable(),
        })
    }
}
impl Portable for D3D12_CONSERVATIVE_RASTERIZATION_MODE {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_LINE_RASTERIZATION_MODE {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_CULL_MODE {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_FILL_MODE {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_RASTERIZER_DESC1 {
    fn portable(&self) -> Value {
        json!({
        "FillMode":self.FillMode.portable(),
        "CullMode":self.CullMode.portable(),
        "FrontCounterClockwise":self.FrontCounterClockwise.portable(),
        "DepthBias":self.DepthBias.portable(),
        "DepthBiasClamp":self.DepthBiasClamp.portable(),
        "SlopeScaledDepthBias":self.SlopeScaledDepthBias.portable(),
        "DepthClipEnable":self.DepthClipEnable.portable(),
        "MultisampleEnable":self.MultisampleEnable.portable(),
        "AntialiasedLineEnable":self.AntialiasedLineEnable.portable(),
        "ForcedSampleCount":self.ForcedSampleCount.portable(),
        "ConservativeRaster":self.ConservativeRaster.portable(),
        })
    }
}
impl Portable for D3D12_RASTERIZER_DESC {
    fn portable(&self) -> Value {
        json!({
        "FillMode":self.FillMode.portable(),
        "CullMode":self.CullMode.portable(),
        "FrontCounterClockwise":self.FrontCounterClockwise.portable(),
        "DepthBias":self.DepthBias.portable(),
        "DepthBiasClamp":self.DepthBiasClamp.portable(),
        "SlopeScaledDepthBias":self.SlopeScaledDepthBias.portable(),
        "DepthClipEnable":self.DepthClipEnable.portable(),
        "MultisampleEnable":self.MultisampleEnable.portable(),
        "AntialiasedLineEnable":self.AntialiasedLineEnable.portable(),
        "ForcedSampleCount":self.ForcedSampleCount.portable(),
        "ConservativeRaster":self.ConservativeRaster.portable(),
        })
    }
}
impl Portable for D3D12_BLEND_DESC {
    fn portable(&self) -> Value {
        json!({
        "AlphaToCoverageEnable":self.AlphaToCoverageEnable.portable(),
        "IndependentBlendEnable":self.IndependentBlendEnable.portable(),
        "RenderTarget":self.RenderTarget.portable(),
        })
    }
}
impl Portable for D3D12_RENDER_TARGET_BLEND_DESC {
    fn portable(&self) -> Value {
        json!({
        "BlendEnable":self.BlendEnable.portable(),
        "LogicOpEnable":self.LogicOpEnable.portable(),
        "SrcBlend":self.SrcBlend.portable(),
        "DestBlend":self.DestBlend.portable(),
        "BlendOp":self.BlendOp.portable(),
        "SrcBlendAlpha":self.SrcBlendAlpha.portable(),
        "DestBlendAlpha":self.DestBlendAlpha.portable(),
        "BlendOpAlpha":self.BlendOpAlpha.portable(),
        "LogicOp":self.LogicOp.portable(),
        "RenderTargetWriteMask":self.RenderTargetWriteMask.portable(),
        })
    }
}
impl Portable for D3D12_LOGIC_OP {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_BLEND_OP {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
impl Portable for D3D12_BLEND {
    fn portable(&self) -> Value {
        json!(self.0)
    }
}
