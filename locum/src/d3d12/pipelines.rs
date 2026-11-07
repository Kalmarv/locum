//! Own PSO inputs on every creation path, including warm pipeline-library loads.
//! Shader bytes are captured without depending on any reflection result.
use super::{id, journal, portable::Portable, values::slice};
use core::ffi::c_void;
use serde_json::{Value, json};
use windows::Win32::Graphics::{Direct3D12::*, Dxgi::Common::*};
use windows::core::{GUID, HRESULT, Interface, PCSTR, PCWSTR};

/// Bytes into the blob store by content hash. A full writer queue is waited for, never dropped: the capture's
/// shader and upload records point at these blobs.
pub unsafe fn bytes(pointer: *const c_void, len: usize) -> Value {
    if len == 0 {
        return Value::Null;
    }
    let data = unsafe { slice(pointer.cast::<u8>(), len) };
    let hash = locum_capture::hash::key(data);
    locum_capture::writer::blob(
        &crate::settings::settings().blobs,
        hash.clone(),
        data.to_vec(),
        true,
    );
    json!({"blob":hash,"size":len})
}
unsafe fn shader(stage: &str, code: &D3D12_SHADER_BYTECODE) -> Value {
    let mut value = unsafe { bytes(code.pShaderBytecode, code.BytecodeLength) };
    if let Some(key) = value["blob"].as_str() {
        let data = unsafe { slice(code.pShaderBytecode.cast::<u8>(), code.BytecodeLength) };
        super::outputs::program(key, data);
    }
    if let Some(o) = value.as_object_mut() {
        o.insert("stage".into(), json!(stage));
    }
    value
}
/// A state object's DXIL library, its UAV registers read as a program's are.
unsafe fn library(code: &D3D12_SHADER_BYTECODE) -> Value {
    let value = unsafe { bytes(code.pShaderBytecode, code.BytecodeLength) };
    if let Some(key) = value["blob"].as_str() {
        let data = unsafe { slice(code.pShaderBytecode.cast::<u8>(), code.BytecodeLength) };
        super::outputs::program(key, data);
    }
    value
}
unsafe fn semantic(s: PCSTR) -> Value {
    if s.is_null() {
        Value::Null
    } else {
        json!(unsafe { s.as_bytes() })
    }
}
unsafe fn input_layout(d: &D3D12_INPUT_LAYOUT_DESC) -> Value {
    json!(unsafe {slice(d.pInputElementDescs,d.NumElements as usize)}.iter().map(|e|json!({
        "semantic":unsafe {semantic(e.SemanticName)},"index":e.SemanticIndex,"format":e.Format.0,
        "slot":e.InputSlot,"offset":e.AlignedByteOffset,"class":e.InputSlotClass.0,"step":e.InstanceDataStepRate})).collect::<Vec<_>>())
}
unsafe fn stream_output(d: &D3D12_STREAM_OUTPUT_DESC) -> Value {
    json!({"entries":unsafe {slice(d.pSODeclaration,d.NumEntries as usize)}.iter().map(|e|json!({
        "stream":e.Stream,"semantic":unsafe {semantic(e.SemanticName)},"index":e.SemanticIndex,
        "start":e.StartComponent,"count":e.ComponentCount,"slot":e.OutputSlot})).collect::<Vec<_>>(),
        "strides":unsafe {slice(d.pBufferStrides,d.NumStrides as usize)},"rasterized_stream":d.RasterizedStream})
}

pub unsafe fn graphics(p: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC) -> Value {
    let Some(d) = (unsafe { p.as_ref() }) else {
        return Value::Null;
    };
    json!({"kind":"graphics","root":d.pRootSignature.as_ref().map_or(0,|r|id(r.as_raw())),
        "shaders":[unsafe {shader("vs",&d.VS)},unsafe {shader("hs",&d.HS)},unsafe {shader("ds",&d.DS)},
            unsafe {shader("gs",&d.GS)},unsafe {shader("ps",&d.PS)},Value::Null],
        "stream_output":unsafe {stream_output(&d.StreamOutput)},"blend":d.BlendState.portable(),"sample_mask":d.SampleMask,
        "rasterizer":d.RasterizerState.portable(),"depth_stencil":d.DepthStencilState.portable(),
        "input_layout":unsafe {input_layout(&d.InputLayout)},"strip_cut":d.IBStripCutValue.0,
        "topology_type":d.PrimitiveTopologyType.0,"render_target_count":d.NumRenderTargets,"rt_formats":d.RTVFormats.portable(),
        "ds_format":d.DSVFormat.0,"sample_count":d.SampleDesc.Count,"sample_quality":d.SampleDesc.Quality,
        "node_mask":d.NodeMask,"flags":d.Flags.0,"cached":unsafe {bytes(d.CachedPSO.pCachedBlob,d.CachedPSO.CachedBlobSizeInBytes)}})
}
pub unsafe fn compute(p: *const D3D12_COMPUTE_PIPELINE_STATE_DESC) -> Value {
    let Some(d) = (unsafe { p.as_ref() }) else {
        return Value::Null;
    };
    json!({"kind":"compute","root":d.pRootSignature.as_ref().map_or(0,|r|id(r.as_raw())),
        "shaders":[Value::Null,Value::Null,Value::Null,Value::Null,Value::Null,unsafe {shader("cs",&d.CS)}],
        "node_mask":d.NodeMask,"flags":d.Flags.0,"cached":unsafe {bytes(d.CachedPSO.pCachedBlob,d.CachedPSO.CachedBlobSizeInBytes)}})
}
pub unsafe fn stream(p: *const D3D12_PIPELINE_STATE_STREAM_DESC) -> Value {
    let Some(d) = (unsafe { p.as_ref() }) else {
        return Value::Null;
    };
    let mut records = Vec::new();
    let mut shaders = vec![Value::Null; 6];
    let mut root = 0;
    let mut common = serde_json::Map::new();
    let mut at = 0usize;
    let data = unsafe { slice(d.pPipelineStateSubobjectStream.cast::<u8>(), d.SizeInBytes) };
    while at + 4 <= data.len() {
        let t = D3D12_PIPELINE_STATE_SUBOBJECT_TYPE(i32::from_le_bytes(data[at..at + 4].try_into().unwrap()));
        let Some((size, alignment)) = super::inputs::payload(t) else {
            records.push(json!({"type":t.0,"diagnostic":"unknown PSO stream subobject"}));
            break;
        };
        let offset = at + 4usize.next_multiple_of(alignment);
        if offset.checked_add(size).is_none_or(|end| end > data.len()) {
            records.push(json!({"type":t.0,"diagnostic":"truncated PSO stream subobject"}));
            break;
        }
        let pointer = unsafe { data.as_ptr().add(offset) };
        macro_rules! read {
            ($ty:ty) => {
                unsafe { pointer.cast::<$ty>().read_unaligned() }
            };
        }
        let value = match t {
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE => {
                root = id(read!(*mut c_void));
                json!(root)
            }
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_HS
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DS
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS => {
                let (stage, index) = match t {
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS => ("vs", 0),
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_HS => ("hs", 1),
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DS => ("ds", 2),
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS => ("gs", 3),
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS => ("ps", 4),
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS => ("cs", 5),
                    D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS => ("as", 6),
                    _ => ("ms", 7),
                };
                let code = read!(D3D12_SHADER_BYTECODE);
                let value = unsafe { shader(stage, &code) };
                if index < 6 {
                    shaders[index] = value.clone();
                }
                value
            }
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_STREAM_OUTPUT => {
                let d = read!(D3D12_STREAM_OUTPUT_DESC);
                unsafe { stream_output(&d) }
            }
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT => {
                let d = read!(D3D12_INPUT_LAYOUT_DESC);
                unsafe { input_layout(&d) }
            }
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND => read!(D3D12_BLEND_DESC).portable(),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER => read!(D3D12_RASTERIZER_DESC).portable(),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER1 => read!(D3D12_RASTERIZER_DESC1).portable(),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2 => read!(D3D12_RASTERIZER_DESC2).portable(),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL => read!(D3D12_DEPTH_STENCIL_DESC).portable(),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1 => read!(D3D12_DEPTH_STENCIL_DESC1).portable(),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2 => read!(D3D12_DEPTH_STENCIL_DESC2).portable(),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS => {
                read!(D3D12_RT_FORMAT_ARRAY).portable()
            }
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC => {
                let s = read!(DXGI_SAMPLE_DESC);
                json!({"count":s.Count,"quality":s.Quality})
            }
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CACHED_PSO => {
                let s = read!(D3D12_CACHED_PIPELINE_STATE);
                unsafe { bytes(s.pCachedBlob, s.CachedBlobSizeInBytes) }
            }
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VIEW_INSTANCING => {
                let s = read!(D3D12_VIEW_INSTANCING_DESC);
                json!({"flags":s.Flags.0,"locations":unsafe {slice(s.pViewInstanceLocations,s.ViewInstanceCount as usize)}.iter()
                    .map(|v|json!({"viewport":v.ViewportArrayIndex,"render_target":v.RenderTargetArrayIndex})).collect::<Vec<_>>()})
            }
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_NODE_MASK
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_FLAGS => json!(read!(u32)),
            _ => json!({"diagnostic":"PSO stream encoder needs this subobject type"}),
        };
        let key = match t {
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT => Some("input_layout"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND => Some("blend"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER1
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2 => Some("rasterizer"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1
            | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2 => Some("depth_stencil"),
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK => Some("sample_mask"),
            _ => None,
        };
        if let Some(key) = key {
            common.insert(key.into(), value.clone());
        }
        records.push(json!({"type":t.0,"value":value}));
        at = (offset + size).next_multiple_of(size_of::<usize>());
    }
    common.extend(
        json!({"kind":"stream","root":root,"shaders":shaders,"subobjects":records})
            .as_object()
            .unwrap()
            .clone(),
    );
    Value::Object(common)
}

/// A wide string's text, or null.
unsafe fn wide(s: PCWSTR) -> Value {
    if s.is_null() {
        Value::Null
    } else {
        unsafe { s.to_string() }.map_or(Value::Null, Value::String)
    }
}
unsafe fn names(p: *const PCWSTR, n: u32) -> Value {
    json!(
        unsafe { slice(p, n as usize) }
            .iter()
            .map(|s| unsafe { wide(*s) })
            .collect::<Vec<_>>()
    )
}
unsafe fn exports(p: *const D3D12_EXPORT_DESC, n: u32) -> Value {
    json!(
        unsafe { slice(p, n as usize) }
            .iter()
            .map(|e| json!({"name":unsafe { wide(e.Name) },
        "rename":unsafe { wide(e.ExportToRename) },"flags":e.Flags.0}))
            .collect::<Vec<_>>()
    )
}
/// A state object (CreateStateObject, or AddToStateObject onto `parent`): its type and every subobject in order, as
/// replay makes it again. An association names the subobject it associates by its index in this description; one
/// pointing outside it (which the runtime rejects) keeps its index null. The identifiers the application asks for
/// are added as it asks ([`identifier`]).
pub unsafe fn state_object(p: *const D3D12_STATE_OBJECT_DESC, parent: u64) -> Value {
    let Some(d) = (unsafe { p.as_ref() }) else {
        return Value::Null;
    };
    let subobjects = unsafe { slice(d.pSubobjects, d.NumSubobjects as usize) };
    let index = |target: *const D3D12_STATE_SUBOBJECT| {
        subobjects
            .iter()
            .position(|s| core::ptr::eq(s, target))
            .map_or(Value::Null, |i| json!(i))
    };
    let encoded: Vec<Value> = subobjects
        .iter()
        .map(|s| {
            let pointer = s.pDesc;
            macro_rules! read {
                ($ty:ty) => {
                    unsafe { pointer.cast::<$ty>().read_unaligned() }
                };
            }
            let value = if pointer.is_null() {
                Value::Null
            } else {
                match s.Type {
                    D3D12_STATE_SUBOBJECT_TYPE_STATE_OBJECT_CONFIG => json!(read!(u32)),
                    D3D12_STATE_SUBOBJECT_TYPE_GLOBAL_ROOT_SIGNATURE
                    | D3D12_STATE_SUBOBJECT_TYPE_LOCAL_ROOT_SIGNATURE => json!(id(read!(*mut c_void))),
                    D3D12_STATE_SUBOBJECT_TYPE_NODE_MASK => json!(read!(u32)),
                    D3D12_STATE_SUBOBJECT_TYPE_DXIL_LIBRARY => {
                        let l = unsafe { &*pointer.cast::<D3D12_DXIL_LIBRARY_DESC>() };
                        json!({"library":unsafe { library(&l.DXILLibrary) },
                            "exports":unsafe { exports(l.pExports, l.NumExports) }})
                    }
                    D3D12_STATE_SUBOBJECT_TYPE_EXISTING_COLLECTION => {
                        let c = unsafe { &*pointer.cast::<D3D12_EXISTING_COLLECTION_DESC>() };
                        json!({"collection":c.pExistingCollection.as_ref().map_or(0, |o| id(o.as_raw())),
                            "exports":unsafe { exports(c.pExports, c.NumExports) }})
                    }
                    D3D12_STATE_SUBOBJECT_TYPE_SUBOBJECT_TO_EXPORTS_ASSOCIATION => {
                        let a = unsafe { &*pointer.cast::<D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION>() };
                        json!({"subobject":index(a.pSubobjectToAssociate),
                            "exports":unsafe { names(a.pExports, a.NumExports) }})
                    }
                    D3D12_STATE_SUBOBJECT_TYPE_DXIL_SUBOBJECT_TO_EXPORTS_ASSOCIATION => {
                        let a = unsafe { &*pointer.cast::<D3D12_DXIL_SUBOBJECT_TO_EXPORTS_ASSOCIATION>() };
                        json!({"subobject":unsafe { wide(a.SubobjectToAssociate) },
                            "exports":unsafe { names(a.pExports, a.NumExports) }})
                    }
                    D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_SHADER_CONFIG => {
                        let c = read!(D3D12_RAYTRACING_SHADER_CONFIG);
                        json!({"payload":c.MaxPayloadSizeInBytes,"attributes":c.MaxAttributeSizeInBytes})
                    }
                    D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_PIPELINE_CONFIG => {
                        json!({"depth":read!(D3D12_RAYTRACING_PIPELINE_CONFIG).MaxTraceRecursionDepth})
                    }
                    D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_PIPELINE_CONFIG1 => {
                        let c = read!(D3D12_RAYTRACING_PIPELINE_CONFIG1);
                        json!({"depth":c.MaxTraceRecursionDepth,"flags":c.Flags.0})
                    }
                    D3D12_STATE_SUBOBJECT_TYPE_HIT_GROUP => {
                        let h = read!(D3D12_HIT_GROUP_DESC);
                        json!({"name":unsafe { wide(h.HitGroupExport) },"type":h.Type.0,
                            "any_hit":unsafe { wide(h.AnyHitShaderImport) },
                            "closest_hit":unsafe { wide(h.ClosestHitShaderImport) },
                            "intersection":unsafe { wide(h.IntersectionShaderImport) }})
                    }
                    // Work graphs and generic programs (SetProgram) are not raytracing; replay refuses them by name.
                    _ => json!({"diagnostic":"state subobject type not represented"}),
                }
            };
            json!({"type":s.Type.0,"value":value})
        })
        .collect();
    json!({"kind":"state_object","type":d.Type.0,"parent":parent,"subobjects":encoded,"identifiers":{}})
}

/// The application asked state object `id` for `export`'s shader identifier and got `identifier`: kept in its
/// description, by which replay maps the identifiers in shader tables to its own.
pub unsafe fn identifier(id: u64, export: PCWSTR, identifier: *const c_void) {
    if !journal::enabled() || identifier.is_null() || export.is_null() {
        return;
    }
    let Ok(name) = (unsafe { export.to_string() }) else {
        return;
    };
    let bytes = unsafe {
        slice(
            identifier.cast::<u8>(),
            D3D12_SHADER_IDENTIFIER_SIZE_IN_BYTES as usize,
        )
    };
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    journal::amend(id, |d| {
        if d["identifiers"][&name] == json!(hex) {
            return false;
        }
        d["identifiers"][&name] = json!(hex);
        true
    });
}

pub unsafe fn created(device: u64, out: *mut *mut c_void, description: impl FnOnce() -> Value) {
    if !journal::enabled() || out.is_null() || unsafe { (*out).is_null() } {
        return;
    }
    let (id, description) = (id(unsafe { *out }), description());
    if description.get("shaders").is_some() {
        super::outputs::pipeline(id, &description);
    } else if description["kind"] == "state_object" {
        super::outputs::state_object(id, &description);
    }
    journal::describe(id, device, description);
}

pub unsafe fn root_signature(
    device: u64,
    out: *mut *mut c_void,
    pointer: *const c_void,
    len: usize,
    node: u32,
) {
    if !journal::enabled() || out.is_null() || unsafe { (*out).is_null() } {
        return;
    }
    let blob = unsafe { bytes(pointer, len) };
    let decoded = unsafe { decode_root(pointer, len) }.unwrap_or_else(|e| json!({"diagnostic":e}));
    super::outputs::root(id(unsafe { *out }), &decoded);
    journal::describe(
        id(unsafe { *out }),
        device,
        json!({"kind":"root_signature","node_mask":node,"original":blob,"decoded":decoded}),
    );
}
unsafe fn decode_root(pointer: *const c_void, len: usize) -> Result<Value, String> {
    type F = unsafe extern "system" fn(*const c_void, usize, *const GUID, *mut *mut c_void) -> HRESULT;
    let f = unsafe {
        crate::system::export::<F>(
            crate::system::system_d3d12(),
            c"D3D12CreateVersionedRootSignatureDeserializer",
        )
    }
    .ok_or("root deserializer export absent")?;
    let mut raw = core::ptr::null_mut();
    unsafe {
        f(
            pointer,
            len,
            &ID3D12VersionedRootSignatureDeserializer::IID,
            &mut raw,
        )
    }
    .ok()
    .map_err(|e| e.to_string())?;
    let decoder = unsafe { ID3D12VersionedRootSignatureDeserializer::from_raw(raw) };
    let original = unsafe { &*decoder.GetUnconvertedRootSignatureDesc() };
    let (parameters, samplers, flags) = if original.Version == D3D_ROOT_SIGNATURE_VERSION_1_2 {
        let d = unsafe { original.Anonymous.Desc_1_2 };
        (
            unsafe { slice(d.pParameters, d.NumParameters as usize) },
            Value::Array(
                unsafe { slice(d.pStaticSamplers, d.NumStaticSamplers as usize) }
                    .iter()
                    .map(Portable::portable)
                    .collect(),
            ),
            d.Flags.0,
        )
    } else {
        let d = unsafe { decoder.GetRootSignatureDescAtVersion(D3D_ROOT_SIGNATURE_VERSION_1_1) }
            .map_err(|e| e.to_string())?;
        let d = unsafe { (*d).Anonymous.Desc_1_1 };
        (
            unsafe { slice(d.pParameters, d.NumParameters as usize) },
            Value::Array(
                unsafe { slice(d.pStaticSamplers, d.NumStaticSamplers as usize) }
                    .iter()
                    .map(Portable::portable)
                    .collect(),
            ),
            d.Flags.0,
        )
    };
    let parameters: Vec<_> = parameters
        .iter()
        .map(|p| {
            let value = match p.ParameterType {
                D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE => {
                    let t = unsafe { p.Anonymous.DescriptorTable };
                    Value::Array(
                        unsafe { slice(t.pDescriptorRanges, t.NumDescriptorRanges as usize) }
                            .iter()
                            .map(Portable::portable)
                            .collect(),
                    )
                }
                D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS => unsafe { p.Anonymous.Constants }.portable(),
                _ => unsafe { p.Anonymous.Descriptor }.portable(),
            };
            json!({"type":p.ParameterType.0,"visibility":p.ShaderVisibility.0,"value":value})
        })
        .collect();
    Ok(json!({"version":original.Version.0,"flags":flags,"parameters":parameters,"samplers":samplers}))
}
