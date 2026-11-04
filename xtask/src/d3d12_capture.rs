//! Explicit capture policy, separate from ABI forwarding. Every list command is
//! either represented semantically here or emits a visible development diagnostic.
use super::{Class, Entry};

/// Generate field-wise encoders only for a reviewed set of pointer-free structures.
/// Recursive dependencies must also be pointer-free; unions and pointers fail generation.
pub fn portable(windows: &std::path::Path) -> String {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fmt::Write;
    use syn::{Fields, Item, Type};
    let file = std::fs::read_to_string(windows.join("src/Windows/Win32/Graphics/Direct3D12/mod.rs")).unwrap();
    let source = syn::parse_file(&file).unwrap();
    let structs: BTreeMap<_, _> = source
        .items
        .into_iter()
        .filter_map(|item| match item {
            Item::Struct(s) => Some((s.ident.to_string(), s)),
            _ => None,
        })
        .collect();
    let mut pending: Vec<String> = [
        "D3D12_BLEND_DESC",
        "D3D12_RASTERIZER_DESC",
        "D3D12_RASTERIZER_DESC1",
        "D3D12_RASTERIZER_DESC2",
        "D3D12_DEPTH_STENCIL_DESC",
        "D3D12_DEPTH_STENCIL_DESC1",
        "D3D12_DEPTH_STENCIL_DESC2",
        "D3D12_RT_FORMAT_ARRAY",
        "D3D12_STATIC_SAMPLER_DESC",
        "D3D12_STATIC_SAMPLER_DESC1",
        "D3D12_SAMPLER_DESC",
        "D3D12_DESCRIPTOR_RANGE1",
        "D3D12_ROOT_CONSTANTS",
        "D3D12_ROOT_DESCRIPTOR1",
        "D3D12_BUFFER_SRV",
        "D3D12_TEX1D_SRV",
        "D3D12_TEX1D_ARRAY_SRV",
        "D3D12_TEX2D_SRV",
        "D3D12_TEX2D_ARRAY_SRV",
        "D3D12_TEX2DMS_SRV",
        "D3D12_TEX2DMS_ARRAY_SRV",
        "D3D12_TEX3D_SRV",
        "D3D12_TEXCUBE_SRV",
        "D3D12_TEXCUBE_ARRAY_SRV",
        "D3D12_BUFFER_UAV",
        "D3D12_TEX1D_UAV",
        "D3D12_TEX1D_ARRAY_UAV",
        "D3D12_TEX2D_UAV",
        "D3D12_TEX2D_ARRAY_UAV",
        "D3D12_TEX3D_UAV",
        "D3D12_BUFFER_RTV",
        "D3D12_TEX1D_RTV",
        "D3D12_TEX1D_ARRAY_RTV",
        "D3D12_TEX2D_RTV",
        "D3D12_TEX2D_ARRAY_RTV",
        "D3D12_TEX2DMS_RTV",
        "D3D12_TEX2DMS_ARRAY_RTV",
        "D3D12_TEX3D_RTV",
        "D3D12_TEX1D_DSV",
        "D3D12_TEX1D_ARRAY_DSV",
        "D3D12_TEX2D_DSV",
        "D3D12_TEX2D_ARRAY_DSV",
        "D3D12_TEX2DMS_DSV",
        "D3D12_TEX2DMS_ARRAY_DSV",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let mut code = String::from(
        "//! Generated field encoders from windows metadata by cargo xtask gen.\n#![allow(non_snake_case,dead_code)]\nuse windows::Win32::Graphics::Direct3D12::*;\nuse serde_json::{Value,json};\npub trait Portable { fn portable(&self) -> Value; }\nimpl<T:Portable,const N:usize> Portable for [T;N] { fn portable(&self)->Value { Value::Array(self.iter().map(Portable::portable).collect()) } }\n",
    );
    let primitive = [
        "u8", "i8", "u16", "i16", "u32", "i32", "u64", "i64", "f32", "f64", "bool",
    ];
    for t in primitive {
        writeln!(
            code,
            "impl Portable for {t} {{ fn portable(&self)->Value {{ json!(self) }} }}"
        )
        .unwrap();
    }
    code.push_str("impl Portable for windows::core::BOOL { fn portable(&self)->Value { json!(self.as_bool()) } }\nimpl Portable for windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT { fn portable(&self)->Value { json!(self.0) } }\n");
    let mut seen = BTreeSet::new();
    while let Some(name) = pending.pop() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let s = structs
            .get(&name)
            .unwrap_or_else(|| panic!("portable type missing: {name}"));
        writeln!(code, "impl Portable for {name} {{ fn portable(&self)->Value {{").unwrap();
        match &s.fields {
            Fields::Unnamed(f) if f.unnamed.len() == 1 => code.push_str("json!(self.0)\n"),
            Fields::Named(fields) => {
                code.push_str("json!({\n");
                for field in &fields.named {
                    let id = field.ident.as_ref().unwrap();
                    let mut ty = &field.ty;
                    while let Type::Array(array) = ty {
                        ty = &array.elem;
                    }
                    let Type::Path(path) = ty else {
                        panic!("non-value field in {name}.{id}");
                    };
                    let dep = path.path.segments.last().unwrap().ident.to_string();
                    if dep.starts_with("D3D12_") {
                        pending.push(dep);
                    }
                    writeln!(code, "\"{id}\":self.{id}.portable(),").unwrap();
                }
                code.push_str("})\n");
            }
            _ => panic!("non-value structure {name}"),
        }
        code.push_str("} }\n");
    }
    code
}

pub fn before(c: &Class, e: &Entry) -> String {
    let n = e.name.as_str();
    if c.module == "d12_resource" && n == "Unmap" {
        return "crate::d3d12::journal::unmap(self.data.id,a0);\n".into();
    }
    if c.module == "d12_queue"
        && matches!(
            n,
            "ExecuteCommandLists" | "Signal" | "Wait" | "UpdateTileMappings" | "CopyTileMappings"
        )
    {
        let mut s = "let mut queue_order = self.data.queue_seq.lock().unwrap_or_else(|e|e.into_inner());\n*queue_order += 1;\n".to_string();
        if n == "ExecuteCommandLists" {
            // Locum's copy lists go right after the lists whose open passes they copy (see d3d12::passes).
            s.push_str("let mut submission = crate::d3d12::journal::submit(&self.data,self.real(),*queue_order,a0,a1);\n");
            s.push_str("let (count, lists) = submission.arrange(a0, mapped1.ptr());\n");
        }
        return s;
    }
    if c.module == "d12_list" && n == "Close" {
        return "self.data.closing(self.real());\n".into();
    }
    if c.module != "d12_list"
        || matches!(
            n,
            "GetPrivateData"
                | "SetPrivateData"
                | "SetPrivateDataInterface"
                | "SetName"
                | "GetDevice"
                | "GetType"
                | "Close"
                | "Reset"
        )
    {
        return String::new();
    }
    if n == "ExecuteBundle" {
        return "self.data.bundle(self.real(),a0);\n".into();
    }
    let expr = match n {
        "DrawInstanced" => "[a0,a1,a2,a3]",
        "DrawIndexedInstanced" => "[a0,a1,a2,a3,a4]",
        "Dispatch" => "[a0,a1,a2]",
        "ClearState"
        | "SetPipelineState"
        | "SetGraphicsRootSignature"
        | "SetComputeRootSignature"
        | "SetProtectedResourceSession"
        | "SetPipelineState1" => "[id(a0)]",
        "CopyBufferRegion" => "[id(a0),a1,id(a2),a3,a4]",
        "CopyResource" => "[id(a0),id(a1)]",
        "CopyTextureRegion" => "[texture_copy(a0),a1,a2,a3,texture_copy(a4),box_value(a5)]",
        "ResolveSubresource" => "[id(a0),a1,id(a2),a3,a4.0]",
        "IASetPrimitiveTopology" => "[a0.0]",
        "RSSetViewports" => "[viewports(a1,a0)]",
        "RSSetScissorRects" => "[rects(a1,a0)]",
        "OMSetBlendFactor" => "[slice(a0,4)]",
        "OMSetStencilRef" | "SetViewInstanceMask" => "[a0]",
        "OMSetDepthBounds" => "[a0,a1]",
        "SetDescriptorHeaps" => "[slice(a1,a0 as usize).iter().map(|p|id(*p)).collect::<Vec<_>>() ]",
        "SetGraphicsRootDescriptorTable" | "SetComputeRootDescriptorTable" => "[a0,descriptor(a1.ptr,true)]",
        "SetGraphicsRoot32BitConstant" | "SetComputeRoot32BitConstant" => "[a0,a1,a2]",
        "SetGraphicsRoot32BitConstants" | "SetComputeRoot32BitConstants" => {
            "[a0,slice(a2.cast::<u32>(),a1 as usize),a3]"
        }
        "SetGraphicsRootConstantBufferView"
        | "SetComputeRootConstantBufferView"
        | "SetGraphicsRootShaderResourceView"
        | "SetComputeRootShaderResourceView"
        | "SetGraphicsRootUnorderedAccessView"
        | "SetComputeRootUnorderedAccessView" => "[a0,address(a1)]",
        "IASetIndexBuffer" => "[index_buffer(a0)]",
        "IASetVertexBuffers" => "[a0,vertex_buffers(a2,a1)]",
        "SOSetTargets" => "[a0,stream_outputs(a2,a1)]",
        "ResourceBarrier" => "[barriers(a1,a0)]",
        "OMSetRenderTargets" => {
            "[render_targets(a1,a0,a2.as_bool()),a3.as_ref().map(|d|descriptor(d.ptr as u64,false))]"
        }
        "ClearRenderTargetView" => "[descriptor(a0.ptr as u64,false),slice(a1,4),rects(a3,a2)]",
        "ClearDepthStencilView" => "[descriptor(a0.ptr as u64,false),a1.0,a2,a3,rects(a5,a4)]",
        "ClearUnorderedAccessViewUint" | "ClearUnorderedAccessViewFloat" => {
            "[descriptor(a0.ptr,true),descriptor(a1.ptr as u64,false),id(a2),slice(a3,4),rects(a5,a4)]"
        }
        "BeginQuery" | "EndQuery" => "[id(a0),a1.0,a2]",
        "ResolveQueryData" => "[id(a0),a1.0,a2,a3,id(a4),a5]",
        "SetPredication" => "[id(a0),a1,a2.0]",
        "ExecuteIndirect" => "[id(a0),a1,id(a2),a3,id(a4),a5]",
        "SetMarker" | "BeginEvent" => "[a0,slice(a1.cast::<u8>(),a2 as usize)]",
        "EndEvent" | "EndRenderPass" => "[]",
        "WriteBufferImmediate" => "[write_immediate(a1,a2,a0)]",
        "DiscardResource" => "[id(a0),discard_region(a1)]",
        "CopyTiles" => "[id(a0),tile_coordinate(a1),tile_region(a2),id(a3),a4,a5.0]",
        "AtomicCopyBufferUINT" | "AtomicCopyBufferUINT64" => {
            "[id(a0),a1,id(a2),a3,dependent_resources(a5,a6,a4)]"
        }
        "SetSamplePositions" => "[a0,a1,sample_positions(a2,a0,a1)]",
        "ResolveSubresourceRegion" => {
            "[id(a0),a1,a2,a3,id(a4),a5,rects(a6,u32::from(!a6.is_null())),a7.0,a8.0]"
        }
        "BeginRenderPass" => "[render_pass_targets(a1,a0),render_pass_depth(a2),a3.0]",
        "InitializeMetaCommand" | "ExecuteMetaCommand" => "[id(a0),slice(a1.cast::<u8>(),a2)]",
        "BuildRaytracingAccelerationStructure" => "[acceleration_structure_build(a0),postbuild_info(a2,a1)]",
        "EmitRaytracingAccelerationStructurePostbuildInfo" => "[postbuild_info(a0,1),addresses(a2,a1)]",
        "CopyRaytracingAccelerationStructure" => "[address(a0),address(a1),a2.0]",
        "DispatchRays" => "[dispatch_rays(a0)]",
        "RSSetShadingRate" => "[a0.0,slice(a1,2).iter().map(|c|c.0).collect::<Vec<_>>()]",
        "RSSetShadingRateImage" => "[id(a0)]",
        "DispatchMesh" => "[a0,a1,a2]",
        "Barrier" => "[barrier_groups(a1,a0)]",
        "OMSetFrontAndBackStencilRef" => "[a0,a1]",
        "RSSetDepthBias" => "[a0,a1,a2]",
        "IASetIndexBufferStripCutValue" => "[a0.0]",
        "SetProgram" => "[set_program(a0)]",
        "DispatchGraph" => "[dispatch_graph(a0)]",
        // Every list method is represented: one without an entry fails generation rather than recording blindly.
        _ => panic!("no capture entry for command list method {n}"),
    };
    format!(
        "self.data.record(self.real(),\"{n}\", || {{ #[allow(unused_imports)] use crate::d3d12::values::*; serde_json::json!({expr}) }});\n"
    )
}

pub fn after(c: &Class, e: &Entry) -> String {
    let n = e.name.as_str();
    let pso = match (c.module, n) {
        ("d12_device", "CreateGraphicsPipelineState") => Some(("graphics", "a0", "a2", "self.data.id")),
        ("d12_device", "CreateComputePipelineState") => Some(("compute", "a0", "a2", "self.data.id")),
        ("d12_device", "CreatePipelineState") => Some(("stream", "a0", "a2", "self.data.id")),
        ("d12_library", "LoadGraphicsPipeline") => Some((
            "graphics",
            "a1",
            "a3",
            "crate::d3d12::journal::owner_device(self.data.id)",
        )),
        ("d12_library", "LoadComputePipeline") => Some((
            "compute",
            "a1",
            "a3",
            "crate::d3d12::journal::owner_device(self.data.id)",
        )),
        ("d12_library", "LoadPipeline") => Some((
            "stream",
            "a1",
            "a3",
            "crate::d3d12::journal::owner_device(self.data.id)",
        )),
        _ => None,
    };
    if let Some((function, input, out, device)) = pso {
        return format!(
            "if result.is_ok() {{ crate::d3d12::shaders::{function}({input}); crate::d3d12::pipelines::created({device},{out},||crate::d3d12::pipelines::{function}({input})); }}\n"
        );
    }
    let code = match (c.module, n) {
        ("d12_resource", "Map") => "if result.is_ok() {crate::d3d12::journal::mapped(self.data.id,a0,a2);}",
        ("d12_device", "CreateShaderResourceView") => "crate::d3d12::descriptors::srv(a0,a1,a2);",
        ("d12_device", "CreateUnorderedAccessView") => "crate::d3d12::descriptors::uav(a0,a1,a2,a3);",
        ("d12_device", "CreateRenderTargetView") => "crate::d3d12::descriptors::rtv(a0,a1,a2);",
        ("d12_device", "CreateDepthStencilView") => "crate::d3d12::descriptors::dsv(a0,a1,a2);",
        ("d12_device", "CreateConstantBufferView") => "crate::d3d12::descriptors::cb(a0,a1);",
        ("d12_device", "CreateSampler") => "crate::d3d12::descriptors::sampler(a0,a1);",
        ("d12_device", "CopyDescriptors") => "crate::d3d12::descriptors::copy(a0,a1,a2,a3,a4,a5);",
        ("d12_device", "CopyDescriptorsSimple") => "crate::d3d12::descriptors::copy_simple(a0,a1,a2);",

        ("d12_device", "CreateRootSignature") => {
            "if result.is_ok() { crate::d3d12::pipelines::root_signature(self.data.id,a4,a1,a2,a0); }"
        }
        ("d12_device", "CreatePipelineLibrary") => {
            "if result.is_ok() { crate::d3d12::shaders::bytes(\"pipeline-library.bin\",a0,a1); crate::d3d12::pipelines::created(self.data.id,a3,||serde_json::json!({\"kind\":\"pipeline_library\",\"initial\":crate::d3d12::pipelines::bytes(a0,a1)})); }"
        }
        ("d12_device", "CreateStateObject" | "AddToStateObject") => {
            "if result.is_ok() { crate::d3d12::shaders::state_object(a0); }"
        }
        ("d12_pso", "GetCachedBlob") => "if result.is_ok() { crate::d3d12::shaders::cached_blob(a0); }",
        ("d12_library", "Serialize") => {
            "crate::log::log!(\"D3D12 Serialize: library={} capacity={}\",self.data.id,a1);"
        }
        ("d12_list", "Close") => "self.data.close(result);",
        ("d12_list", "Reset") => "self.data.reset(self.real(),result,a0,a1);",
        ("d12_queue", "ExecuteCommandLists") => {
            "crate::d3d12::journal::submitted(&self.data,self.real(),submission);"
        }
        ("d12_queue", "UpdateTileMappings") => {
            "crate::d3d12::tiles::update(&self.data,*queue_order,a0,a1,a2,a3,a4,a5,a6,a7,a8,a9);"
        }
        ("d12_queue", "CopyTileMappings") => {
            "crate::d3d12::tiles::copy(&self.data,*queue_order,a0,a1,a2,a3,a4,a5);"
        }
        ("d12_device", "CreateHeap") => {
            "if result.is_ok() { crate::d3d12::values::created_heap(self.data.id,a2); }"
        }
        ("d12_device", "CreateHeap1") => {
            "if result.is_ok() { crate::d3d12::values::created_heap(self.data.id,a3); }"
        }
        ("d12_queue", "Signal" | "Wait") => {
            return format!("crate::d3d12::journal::sync(&self.data,*queue_order,\"{n}\",a0,a1,result);\n");
        }
        ("d12_device", "CreateCommandList") => {
            "if result.is_ok() { crate::d3d12::journal::created_list(self.data.id,a5,a1.0,a2,a3,false); }"
        }
        ("d12_device", "CreateCommandList1") => {
            "if result.is_ok() { crate::d3d12::journal::created_list(self.data.id,a4,a1.0,core::ptr::null_mut(),core::ptr::null_mut(),true); }"
        }
        ("d12_device", "CreateCommandQueue") => {
            "if result.is_ok() { crate::d3d12::values::created_queue(self.data.id,a2,a0); }"
        }
        ("d12_device", "CreateCommandSignature") => {
            "if result.is_ok() { crate::d3d12::values::created_command_signature(self.data.id,a3,a0,a1); }"
        }
        ("d12_device", "CreateDescriptorHeap") => {
            "if result.is_ok() { crate::d3d12::values::created_descriptor_heap(self.data.id,a2); }"
        }
        ("d12_device", "CreateCommittedResource") => {
            "if result.is_ok() { crate::d3d12::values::created_resource(self.data.id,a6,crate::d3d12::values::Initial::State(a3.0),crate::d3d12::values::Placement::Committed); }"
        }
        ("d12_device", "CreateCommittedResource1") => {
            "if result.is_ok() { crate::d3d12::values::created_resource(self.data.id,a7,crate::d3d12::values::Initial::State(a3.0),crate::d3d12::values::Placement::Committed); }"
        }
        ("d12_device", "CreatePlacedResource") => {
            "if result.is_ok() { crate::d3d12::values::created_resource(self.data.id,a6,crate::d3d12::values::Initial::State(a3.0),crate::d3d12::values::Placement::Placed(crate::d3d12::id(a0),a1)); }"
        }
        ("d12_device", "CreateReservedResource") => {
            "if result.is_ok() { crate::d3d12::values::created_resource(self.data.id,a4,crate::d3d12::values::Initial::State(a1.0),crate::d3d12::values::Placement::Reserved); }"
        }
        ("d12_device", "CreateReservedResource1") => {
            "if result.is_ok() { crate::d3d12::values::created_resource(self.data.id,a5,crate::d3d12::values::Initial::State(a1.0),crate::d3d12::values::Placement::Reserved); }"
        }
        ("d12_device", "CreateCommittedResource2") => {
            "if result.is_ok() { crate::d3d12::values::created_resource(self.data.id,a7,crate::d3d12::values::Initial::State(a3.0),crate::d3d12::values::Placement::Committed); }"
        }
        ("d12_device", "CreatePlacedResource1") => {
            "if result.is_ok() { crate::d3d12::values::created_resource(self.data.id,a6,crate::d3d12::values::Initial::State(a3.0),crate::d3d12::values::Placement::Placed(crate::d3d12::id(a0),a1)); }"
        }
        ("d12_device", "CreateCommittedResource3") => {
            "if result.is_ok() { crate::d3d12::values::created_resource(self.data.id,a9,crate::d3d12::values::Initial::Layout(a3.0),crate::d3d12::values::Placement::Committed); }"
        }
        ("d12_device", "CreatePlacedResource2") => {
            "if result.is_ok() { crate::d3d12::values::created_resource(self.data.id,a8,crate::d3d12::values::Initial::Layout(a3.0),crate::d3d12::values::Placement::Placed(crate::d3d12::id(a0),a1)); }"
        }
        ("d12_device", "CreateReservedResource2") => {
            "if result.is_ok() { crate::d3d12::values::created_resource(self.data.id,a7,crate::d3d12::values::Initial::Layout(a1.0),crate::d3d12::values::Placement::Reserved); }"
        }
        ("d12_device", "CreateCommandQueue1") => {
            "if result.is_ok() { crate::d3d12::values::created_queue(self.data.id,a3,a0); }"
        }
        ("d12_device", "CreateSampler2") => "crate::d3d12::descriptors::sampler2(a0,a1);",
        ("d12_device", "CreateSamplerFeedbackUnorderedAccessView") => {
            "crate::d3d12::descriptors::sampler_feedback(a0,a1,a2);"
        }
        _ => return String::new(),
    };
    format!("{code}\n")
}
