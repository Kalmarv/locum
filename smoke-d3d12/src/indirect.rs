//! ExecuteIndirect whose argument records hold GPU addresses. Replay makes its own buffers, at other addresses than
//! the game's, so the records only do what they did in the game if replay rewrites those addresses; and a buffer the
//! frame uses only through records is named by no command, so its contents and what a root UAV in them wrote are
//! learned only after the frame. Each command signature of the `indirect` capture runs in the armed frame:
//!
//! - a vertex buffer view and a root CBV, then a draw: records the CPU wrote; records a dispatch of the frame wrote at
//!   a non-zero offset, with a count the same dispatch wrote at a non-zero offset, lower than the most records (the
//!   record past the count holds addresses of no buffer);
//! - a draw alone, run by a bundle recorded before the frame (D3D12 lets a bundle run only a signature of one draw or
//!   dispatch, without a count buffer, so a bundle's records never hold addresses), its colours and vertices set in
//!   the bundle;
//! - an index buffer view, root constants and a root SRV the vertex shader reads its positions through, then an
//!   indexed draw;
//! - a root UAV and a root constant, then a dispatch: what it writes is the dispatch's output;
//! - an incrementing constant, then a draw: the runtime gives each command its index as a root constant, which the
//!   shaders place and colour its half by (over a stale value the list set before), where the device has
//!   ExecuteIndirect tier 1.1;
//! - a root CBV, then a mesh dispatch, where the adapter has mesh shaders and a DXIL compiler is found.
//!
//! The vertex, index, position and mesh constant buffers are in default heaps and nothing but the records names them,
//! as a game's mesh pool would be. Colours are 0 or 1 in every channel, so hardware and WARP write the same bytes.
//!
//! The `indirect-gaps` capture holds what cannot be proven: an ExecuteIndirect of a list recorded before the capture
//! was armed, and a buffer only a root UAV in records names, whose contents as the frame began nobody copied.
//!
//! The `indirect-rays` capture runs a DispatchRays argument, where the device has raytracing tier 1.1: its record
//! names the ray generation shader table, which nothing else names, so the capture reads the table from the record.
use super::*;

const RGBA8: DXGI_FORMAT = DXGI_FORMAT_R8G8B8A8_UNORM;
/// A record of signature A: VERTEX_BUFFER_VIEW (16 bytes), CONSTANT_BUFFER_VIEW (8), DRAW (16).
const STRIDE_A: usize = 40;
/// Signature B: INDEX_BUFFER_VIEW (16), CONSTANT of 4 words (16), SHADER_RESOURCE_VIEW (8), DRAW_INDEXED (20).
const STRIDE_B: usize = 60;
/// Signature C: UNORDERED_ACCESS_VIEW (8), CONSTANT of 1 word (4), DISPATCH (12).
const STRIDE_C: usize = 24;
/// Signature D: CONSTANT_BUFFER_VIEW (8), DISPATCH_MESH (12).
const STRIDE_D: usize = 20;
/// Signature E, a bundle's: DRAW (16).
const STRIDE_E: usize = 16;
/// Signature F: INCREMENTING_CONSTANT (no bytes), DRAW (16).
const STRIDE_F: usize = 16;
/// Each target's left and right halves, in order: CPU records, GPU records, the indexed draws, the bundle's, the
/// incrementing constant's, the mesh dispatches'.
const COLOURS: [[f32; 4]; 12] = [
    [1., 0., 0., 1.],
    [0., 0., 1., 1.],
    [0., 1., 0., 1.],
    [1., 1., 1., 1.],
    [1., 1., 0., 1.],
    [0., 1., 1., 1.],
    [1., 0., 1., 1.],
    [0., 1., 0., 1.],
    [0., 0., 1., 1.],
    [1., 1., 0., 1.],
    [1., 0., 0., 1.],
    [0., 1., 1., 1.],
];
/// An address in no buffer: the record past the count holds it, so a record run or patched past the count fails.
const NOWHERE: u64 = 0xdead_0000_0000;

fn words(out: &mut Vec<u8>, words: &[u32]) {
    out.extend(words.iter().flat_map(|w| w.to_le_bytes()));
}

/// Two records of signature A: the left half in `COLOURS[colours.0]`, the right half in `COLOURS[colours.1]`.
fn records_a(vertices: u64, constants: u64, colours: (u64, u64)) -> Vec<u8> {
    let mut out = Vec::new();
    for (half, colour) in [(0u64, colours.0), (1, colours.1)] {
        out.extend((vertices + half * 256).to_le_bytes());
        words(&mut out, &[48, 8]);
        out.extend((constants + colour * 256).to_le_bytes());
        words(&mut out, &[6, 1, 0, 0]);
    }
    assert_eq!(out.len(), 2 * STRIDE_A);
    out
}

pub(super) unsafe fn upload(device: &ID3D12Device, bytes: &[u8], size: u64) -> ID3D12Resource {
    unsafe {
        let r = contents::committed(
            device,
            D3D12_HEAP_TYPE_UPLOAD,
            &contents::buffer_desc(size),
            D3D12_RESOURCE_STATE_GENERIC_READ,
        );
        let mut p = core::ptr::null_mut();
        r.Map(0, None, Some(&mut p)).unwrap();
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        r.Unmap(0, None);
        r
    }
}

/// A buffer in a default heap, in COPY_DEST.
pub(super) unsafe fn default_buffer(device: &ID3D12Device, size: u64, uav: bool) -> ID3D12Resource {
    unsafe {
        contents::committed(
            device,
            D3D12_HEAP_TYPE_DEFAULT,
            &contents::desc(
                D3D12_RESOURCE_DIMENSION_BUFFER,
                DXGI_FORMAT_UNKNOWN,
                (size, 1),
                (1, 1, 1),
                if uav {
                    D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
                } else {
                    D3D12_RESOURCE_FLAG_NONE
                },
            ),
            D3D12_RESOURCE_STATE_COPY_DEST,
        )
    }
}

pub(super) unsafe fn root_signature(
    device: &ID3D12Device,
    parameters: &[D3D12_ROOT_PARAMETER],
    flags: D3D12_ROOT_SIGNATURE_FLAGS,
) -> ID3D12RootSignature {
    unsafe {
        let mut blob = None;
        D3D12SerializeRootSignature(
            &D3D12_ROOT_SIGNATURE_DESC {
                NumParameters: parameters.len() as u32,
                pParameters: parameters.as_ptr(),
                Flags: flags,
                ..Default::default()
            },
            D3D_ROOT_SIGNATURE_VERSION_1,
            &mut blob,
            None,
        )
        .unwrap();
        let blob = blob.unwrap();
        device
            .CreateRootSignature(
                0,
                core::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()),
            )
            .unwrap()
    }
}

pub(super) fn root_view(kind: D3D12_ROOT_PARAMETER_TYPE, register: u32) -> D3D12_ROOT_PARAMETER {
    D3D12_ROOT_PARAMETER {
        ParameterType: kind,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            Descriptor: D3D12_ROOT_DESCRIPTOR {
                ShaderRegister: register,
                RegisterSpace: 0,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    }
}

pub(super) fn root_constants(register: u32, count: u32) -> D3D12_ROOT_PARAMETER {
    D3D12_ROOT_PARAMETER {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
        Anonymous: D3D12_ROOT_PARAMETER_0 {
            Constants: D3D12_ROOT_CONSTANTS {
                ShaderRegister: register,
                RegisterSpace: 0,
                Num32BitValues: count,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    }
}

fn argument(kind: D3D12_INDIRECT_ARGUMENT_TYPE, root: u32, count: u32) -> D3D12_INDIRECT_ARGUMENT_DESC {
    D3D12_INDIRECT_ARGUMENT_DESC {
        Type: kind,
        Anonymous: match kind {
            D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW => D3D12_INDIRECT_ARGUMENT_DESC_0 {
                VertexBuffer: D3D12_INDIRECT_ARGUMENT_DESC_0_0 { Slot: 0 },
            },
            D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT => D3D12_INDIRECT_ARGUMENT_DESC_0 {
                Constant: D3D12_INDIRECT_ARGUMENT_DESC_0_1 {
                    RootParameterIndex: root,
                    DestOffsetIn32BitValues: 0,
                    Num32BitValuesToSet: count,
                },
            },
            D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW
            | D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW
            | D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW => D3D12_INDIRECT_ARGUMENT_DESC_0 {
                ConstantBufferView: D3D12_INDIRECT_ARGUMENT_DESC_0_2 {
                    RootParameterIndex: root,
                },
            },
            _ => Default::default(),
        },
    }
}

unsafe fn signature(
    device: &ID3D12Device,
    arguments: &[D3D12_INDIRECT_ARGUMENT_DESC],
    stride: usize,
    root: Option<&ID3D12RootSignature>,
) -> ID3D12CommandSignature {
    let mut signature: Option<ID3D12CommandSignature> = None;
    unsafe {
        device
            .CreateCommandSignature(
                &D3D12_COMMAND_SIGNATURE_DESC {
                    ByteStride: stride as u32,
                    NumArgumentDescs: arguments.len() as u32,
                    pArgumentDescs: arguments.as_ptr(),
                    NodeMask: 0,
                },
                root,
                &mut signature,
            )
            .unwrap();
    }
    signature.unwrap()
}

unsafe fn graphics_pso(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    vs: &ID3DBlob,
    ps: &ID3DBlob,
    elements: &[D3D12_INPUT_ELEMENT_DESC],
) -> ID3D12PipelineState {
    let mut formats = [DXGI_FORMAT_UNKNOWN; 8];
    formats[0] = RGBA8;
    let bytecode = |b: &ID3DBlob| unsafe {
        D3D12_SHADER_BYTECODE {
            pShaderBytecode: b.GetBufferPointer(),
            BytecodeLength: b.GetBufferSize(),
        }
    };
    let mut p = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root.clone())),
        VS: bytecode(vs),
        PS: bytecode(ps),
        BlendState: blend(),
        SampleMask: u32::MAX,
        RasterizerState: rasterizer(),
        DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
            DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
            ..Default::default()
        },
        InputLayout: D3D12_INPUT_LAYOUT_DESC {
            pInputElementDescs: if elements.is_empty() {
                core::ptr::null()
            } else {
                elements.as_ptr()
            },
            NumElements: elements.len() as u32,
        },
        PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        NumRenderTargets: 1,
        RTVFormats: formats,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        ..Default::default()
    };
    let pso = unsafe { device.CreateGraphicsPipelineState(&p) }.unwrap();
    unsafe { ManuallyDrop::drop(&mut p.pRootSignature) };
    pso
}

fn blend() -> D3D12_BLEND_DESC {
    let rt = D3D12_RENDER_TARGET_BLEND_DESC {
        SrcBlend: D3D12_BLEND_ONE,
        DestBlend: D3D12_BLEND_ZERO,
        BlendOp: D3D12_BLEND_OP_ADD,
        SrcBlendAlpha: D3D12_BLEND_ONE,
        DestBlendAlpha: D3D12_BLEND_ZERO,
        BlendOpAlpha: D3D12_BLEND_OP_ADD,
        LogicOp: D3D12_LOGIC_OP_NOOP,
        RenderTargetWriteMask: 15,
        ..Default::default()
    };
    D3D12_BLEND_DESC {
        RenderTarget: [rt; 8],
        ..Default::default()
    }
}

fn rasterizer() -> D3D12_RASTERIZER_DESC {
    D3D12_RASTERIZER_DESC {
        FillMode: D3D12_FILL_MODE_SOLID,
        CullMode: D3D12_CULL_MODE_NONE,
        DepthClipEnable: true.into(),
        ..Default::default()
    }
}

pub(super) unsafe fn compute_pso(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    cs: &ID3DBlob,
) -> ID3D12PipelineState {
    let mut c = D3D12_COMPUTE_PIPELINE_STATE_DESC {
        pRootSignature: ManuallyDrop::new(Some(root.clone())),
        CS: unsafe {
            D3D12_SHADER_BYTECODE {
                pShaderBytecode: cs.GetBufferPointer(),
                BytecodeLength: cs.GetBufferSize(),
            }
        },
        ..Default::default()
    };
    let pso = unsafe { device.CreateComputePipelineState(&c) }.unwrap();
    unsafe { ManuallyDrop::drop(&mut c.pRootSignature) };
    pso
}

/// DXIL for `profile` (`ms_6_5`, or a library's `lib_6_3`), from `LOCUM_DXCOMPILER` or the newest Windows SDK's
/// `dxcompiler.dll` (which signs it with the `dxil.dll` beside it); or why there is none.
pub(super) fn dxil(source: &str, profile: &str) -> std::result::Result<Vec<u8>, String> {
    use windows::Win32::Graphics::Direct3D::Dxc::*;
    let path = match std::env::var_os("LOCUM_DXCOMPILER") {
        Some(p) => std::path::PathBuf::from(p),
        None => {
            let root = std::path::PathBuf::from(
                std::env::var_os("ProgramFiles(x86)").ok_or("no ProgramFiles(x86)")?,
            )
            .join(r"Windows Kits\10\bin");
            std::fs::read_dir(&root)
                .map_err(|e| format!("{}: {e}", root.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path().join(r"x64\dxcompiler.dll"))
                .filter(|p| p.is_file())
                .max()
                .ok_or("no Windows SDK has an x64 dxcompiler.dll; set LOCUM_DXCOMPILER")?
        }
    };
    // The compiler signs what it makes with dxil.dll, which it loads by name: loaded first from beside it, that load
    // finds it. Unsigned DXIL is refused by the runtime.
    let load = |path: &std::path::Path| {
        unsafe { LoadLibraryW(&HSTRING::from(path.display().to_string())) }
            .map_err(|e| format!("{}: {e}", path.display()))
    };
    load(&path.with_file_name("dxil.dll"))?;
    let module = load(&path)?;
    type Create = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
    let create: Create = unsafe {
        core::mem::transmute(GetProcAddress(module, s!("DxcCreateInstance")).ok_or("no DxcCreateInstance")?)
    };
    let mut raw = core::ptr::null_mut();
    unsafe { create(&CLSID_DxcCompiler, &IDxcCompiler3::IID, &mut raw) }
        .ok()
        .map_err(|e| e.to_string())?;
    let compiler = unsafe { IDxcCompiler3::from_raw(raw) };
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    // A library (`lib_6_3`) has no entry point: its shaders are exports.
    let entry = if profile.starts_with("lib_") {
        &[][..]
    } else {
        &["-E", "main"][..]
    };
    let args: Vec<Vec<u16>> = entry.iter().chain(&["-T", profile]).map(|a| wide(a)).collect();
    let args: Vec<PCWSTR> = args.iter().map(|w| PCWSTR(w.as_ptr())).collect();
    let buffer = DxcBuffer {
        Ptr: source.as_ptr().cast(),
        Size: source.len(),
        Encoding: 0,
    };
    let result: IDxcResult =
        unsafe { compiler.Compile(&buffer, Some(&args), None) }.map_err(|e| e.to_string())?;
    let bytes = |b: &IDxcBlob| unsafe {
        core::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()).to_vec()
    };
    if unsafe { result.GetStatus() }.map_err(|e| e.to_string())?.is_err() {
        let errors = unsafe { result.GetErrorBuffer() }
            .map(|b| String::from_utf8_lossy(&bytes(&b.cast().unwrap())).into_owned())
            .unwrap_or_default();
        panic!("{profile}: {errors}");
    }
    Ok(bytes(&unsafe { result.GetResult() }.map_err(|e| e.to_string())?))
}

/// Pipeline state stream subobjects, each aligned to a pointer (`D3D12_PIPELINE_STATE_STREAM_DESC`).
#[derive(Default)]
struct Stream(Vec<u64>);

impl Stream {
    fn push<T: Copy>(&mut self, kind: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, value: T) {
        let align = align_of::<T>().max(4);
        let at = (4usize).next_multiple_of(align);
        let words = (at + size_of::<T>()).div_ceil(8);
        let start = self.0.len();
        self.0.resize(start + words, 0);
        unsafe {
            let base = self.0.as_mut_ptr().add(start).cast::<u8>();
            base.cast::<u32>().write(kind.0 as u32);
            base.add(at).cast::<T>().write_unaligned(value);
        }
    }
}

/// The mesh dispatch's pipeline, where the adapter has mesh shaders and DXIL can be compiled; or why not.
unsafe fn mesh_pso(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
) -> std::result::Result<ID3D12PipelineState, String> {
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
    unsafe {
        device.CheckFeatureSupport(
            D3D12_FEATURE_D3D12_OPTIONS7,
            (&mut options as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS7).cast(),
            size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS7>() as u32,
        )
    }
    .map_err(|e| format!("CheckFeatureSupport(OPTIONS7): {e}"))?;
    if options.MeshShaderTier == D3D12_MESH_SHADER_TIER_NOT_SUPPORTED {
        return Err("the adapter has no mesh shaders".into());
    }
    let constants = "cbuffer C : register(b0) { float4 colour; float4 rect; };";
    let ms = dxil(
        &format!(
            "{constants} struct V {{ float4 p : SV_Position; }};
             [outputtopology(\"triangle\")] [numthreads(1, 1, 1)]
             void main(out vertices V v[4], out indices uint3 t[2]) {{
                 SetMeshOutputCounts(4, 2);
                 v[0].p = float4(rect.x, -1, 0, 1); v[1].p = float4(rect.x, 1, 0, 1);
                 v[2].p = float4(rect.y, -1, 0, 1); v[3].p = float4(rect.y, 1, 0, 1);
                 t[0] = uint3(0, 1, 2); t[1] = uint3(2, 1, 3);
             }}"
        ),
        "ms_6_5",
    )?;
    let ps = dxil(
        &format!("{constants} float4 main() : SV_Target {{ return colour; }}"),
        "ps_6_5",
    )?;
    let code = |b: &[u8]| D3D12_SHADER_BYTECODE {
        pShaderBytecode: b.as_ptr().cast(),
        BytecodeLength: b.len(),
    };
    let mut formats = D3D12_RT_FORMAT_ARRAY {
        NumRenderTargets: 1,
        ..Default::default()
    };
    formats.RTFormats[0] = RGBA8;
    let mut s = Stream::default();
    s.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE, root.as_raw());
    s.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, code(&ms));
    s.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, code(&ps));
    s.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, blend());
    s.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, rasterizer());
    s.push(
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL,
        D3D12_DEPTH_STENCIL_DESC::default(),
    );
    s.push(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS, formats);
    s.push(
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC,
        DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
    );
    let device: ID3D12Device2 = device.cast().map_err(|e| e.to_string())?;
    let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: s.0.len() * 8,
        pPipelineStateSubobjectStream: s.0.as_mut_ptr().cast(),
    };
    unsafe { device.CreatePipelineState(&desc) }.map_err(|e| format!("mesh pipeline: {e}"))
}

/// Signature F's root signature, pipeline and signature, where the device has ExecuteIndirect tier 1.1; or why not.
/// Root constant word 1 is the command's index: command i draws the half from x = i - 1 to i, in `COLOURS[8 + i]`.
unsafe fn incrementing_f(device: &ID3D12Device) -> std::result::Result<DrawA, String> {
    unsafe {
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS21::default();
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS21,
                (&mut options as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS21).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS21>() as u32,
            )
            .map_err(|e| format!("CheckFeatureSupport(OPTIONS21): {e}"))?;
        if options.ExecuteIndirectTier.0 < D3D12_EXECUTE_INDIRECT_TIER_1_1.0 {
            return Err(format!(
                "the device's ExecuteIndirect tier is {}, not 1.1",
                options.ExecuteIndirectTier.0
            ));
        }
        let [l, r] = [COLOURS[8], COLOURS[9]].map(|c| format!("float4({},{},{},{})", c[0], c[1], c[2], c[3]));
        let constants = "cbuffer K:register(b0) { uint unused; uint index; };";
        let vs = graphics::compile(
            format!(
                "{constants} float4 main(uint v:SV_VertexID):SV_Position {{ \
                 float x0 = float(index) - 1, x1 = float(index); \
                 float2 q[6] = {{ float2(x0,-1), float2(x0,1), float2(x1,-1), float2(x1,-1), float2(x0,1), \
                 float2(x1,1) }}; return float4(q[v],0,1); }}"
            )
            .as_bytes(),
            s!("vs_5_0"),
        );
        let ps = graphics::compile(
            format!("{constants} float4 main():SV_Target {{ return index == 0 ? {l} : {r}; }}").as_bytes(),
            s!("ps_5_0"),
        );
        let root = root_signature(device, &[root_constants(0, 2)], D3D12_ROOT_SIGNATURE_FLAG_NONE);
        let pso = graphics_pso(device, &root, &vs, &ps, &[]);
        let signature = signature(
            device,
            &[
                D3D12_INDIRECT_ARGUMENT_DESC {
                    Type: D3D12_INDIRECT_ARGUMENT_TYPE_INCREMENTING_CONSTANT,
                    Anonymous: D3D12_INDIRECT_ARGUMENT_DESC_0 {
                        IncrementingConstant: D3D12_INDIRECT_ARGUMENT_DESC_0_5 {
                            RootParameterIndex: 0,
                            DestOffsetIn32BitValues: 1,
                        },
                    },
                },
                argument(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW, 0, 0),
            ],
            STRIDE_F,
            Some(&root),
        );
        Ok(DrawA { root, pso, signature })
    }
}

/// `n` 8x8 targets in RENDER_TARGET, their RTV heap and each one's handle.
unsafe fn targets(
    device: &ID3D12Device,
    n: usize,
) -> (
    Vec<ID3D12Resource>,
    ID3D12DescriptorHeap,
    Vec<D3D12_CPU_DESCRIPTOR_HANDLE>,
) {
    unsafe {
        let targets: Vec<ID3D12Resource> = (0..n)
            .map(|_| {
                contents::committed(
                    device,
                    D3D12_HEAP_TYPE_DEFAULT,
                    &contents::desc(
                        D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                        RGBA8,
                        (8, 8),
                        (1, 1, 1),
                        D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                    ),
                    D3D12_RESOURCE_STATE_RENDER_TARGET,
                )
            })
            .collect();
        let heap: ID3D12DescriptorHeap = device
            .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: n as u32,
                ..Default::default()
            })
            .unwrap();
        let start = heap.GetCPUDescriptorHandleForHeapStart();
        let increment = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) as usize;
        let handles: Vec<_> = (0..n)
            .map(|i| D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: start.ptr + i * increment,
            })
            .collect();
        for (t, h) in targets.iter().zip(&handles) {
            device.CreateRenderTargetView(t, None, *h);
        }
        (targets, heap, handles)
    }
}

/// Copies `staging` into each buffer of `copies` (default, in COPY_DEST) before the frame.
pub(super) unsafe fn fill(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    value: u64,
    copies: &[(&ID3D12Resource, &ID3D12Resource, u64)],
) {
    unsafe {
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        for (to, from, bytes) in copies {
            list.CopyBufferRegion(*to, 0, *from, 0, *bytes);
        }
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, value);
    }
}

fn quad(x0: f32, x1: f32) -> Vec<u8> {
    [[x0, -1.], [x0, 1.], [x1, -1.], [x1, -1.], [x0, 1.], [x1, 1.]]
        .iter()
        .flatten()
        .flat_map(|f: &f32| f.to_le_bytes())
        .collect()
}

/// Two quads of `quad`, the left half of clip space at 0 and the right half at 256.
fn halves() -> Vec<u8> {
    let mut bytes = quad(-1., 0.);
    bytes.resize(256, 0);
    bytes.extend(quad(0., 1.));
    bytes.resize(512, 0);
    bytes
}

/// 256 bytes of each colour (a constant buffer view's alignment).
fn colours() -> Vec<u8> {
    COLOURS
        .iter()
        .flat_map(|c| {
            let mut b: Vec<u8> = c.iter().flat_map(|f| f.to_le_bytes()).collect();
            b.resize(256, 0);
            b
        })
        .collect()
}

/// Waits for the capture a fixture asked for; its database.
pub(super) fn written(label: &str) -> rusqlite::Connection {
    let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
    let path = dir.join("captures").join(label).join("capture.db");
    let until = Instant::now() + Duration::from_secs(15);
    while !path.is_file() {
        assert!(Instant::now() < until, "no capture at {}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
    rusqlite::Connection::open(&path).unwrap()
}

pub(super) fn request(label: &str) {
    let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
    std::fs::write(dir.join("capture"), format!("label {label}\n")).unwrap();
}

pub(super) fn check(db: &rusqlite::Connection, checks: &[(&str, i64)]) {
    for (sql, n) in checks {
        assert_eq!(
            db.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap(),
            *n,
            "{sql}"
        );
    }
}

/// Signature A's root signature, pipeline and signature: positions from a vertex buffer, colour from a root CBV.
struct DrawA {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    signature: ID3D12CommandSignature,
}

unsafe fn draw_a(device: &ID3D12Device) -> DrawA {
    unsafe {
        use graphics::compile;
        let vs = compile(
            b"float4 main(float2 p:POSITION):SV_Position { return float4(p,0,1); }",
            s!("vs_5_0"),
        );
        let ps = compile(
            b"cbuffer C:register(b0) { float4 colour; }; float4 main():SV_Target { return colour; }",
            s!("ps_5_0"),
        );
        let root = root_signature(
            device,
            &[root_view(D3D12_ROOT_PARAMETER_TYPE_CBV, 0)],
            D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT,
        );
        let element = D3D12_INPUT_ELEMENT_DESC {
            SemanticName: s!("POSITION"),
            SemanticIndex: 0,
            Format: DXGI_FORMAT_R32G32_FLOAT,
            InputSlot: 0,
            AlignedByteOffset: 0,
            InputSlotClass: D3D12_INPUT_CLASSIFICATION_PER_VERTEX_DATA,
            InstanceDataStepRate: 0,
        };
        let pso = graphics_pso(device, &root, &vs, &ps, &[element]);
        let signature = signature(
            device,
            &[
                argument(D3D12_INDIRECT_ARGUMENT_TYPE_VERTEX_BUFFER_VIEW, 0, 0),
                argument(D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW, 0, 0),
                argument(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW, 0, 0),
            ],
            STRIDE_A,
            Some(&root),
        );
        DrawA { root, pso, signature }
    }
}

/// Signature C's root signature, pipeline and signature: a dispatch writing `value + i` to word i of a root UAV, for
/// 4 threads a group.
unsafe fn dispatch_c(device: &ID3D12Device) -> DrawA {
    unsafe {
        let cs = graphics::compile(
            b"RWByteAddressBuffer dst:register(u0); cbuffer K:register(b0) { uint value; }; \
              [numthreads(4,1,1)] void main(uint i:SV_DispatchThreadID) { dst.Store(i*4, value + i); }",
            s!("cs_5_0"),
        );
        let root = root_signature(
            device,
            &[root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0), root_constants(0, 1)],
            D3D12_ROOT_SIGNATURE_FLAG_NONE,
        );
        let pso = compute_pso(device, &root, &cs);
        let signature = signature(
            device,
            &[
                argument(D3D12_INDIRECT_ARGUMENT_TYPE_UNORDERED_ACCESS_VIEW, 0, 0),
                argument(D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT, 1, 1),
                argument(D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH, 0, 0),
            ],
            STRIDE_C,
            Some(&root),
        );
        DrawA { root, pso, signature }
    }
}

/// A record of signature C: the root UAV at `uav`, `value`, and `groups` groups of 4 threads.
fn record_c(uav: u64, value: u32, groups: u32) -> Vec<u8> {
    let mut out = uav.to_le_bytes().to_vec();
    words(&mut out, &[value, groups, 1, 1]);
    assert_eq!(out.len(), STRIDE_C);
    out
}

/// What each target's halves must hold.
unsafe fn check_targets(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    value: u64,
    targets: &[ID3D12Resource],
    halves: &[(usize, usize)],
) {
    unsafe {
        let readback = contents::committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &contents::buffer_desc(targets.len() as u64 * 65536),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        let mut footprints = Vec::new();
        for (i, t) in targets.iter().enumerate() {
            graphics::transition(
                &list,
                t,
                D3D12_RESOURCE_STATE_RENDER_TARGET,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            );
            footprints.push(contents::read(device, &list, t, &readback, i as u64 * 65536));
        }
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, value);
        let mut pointer = core::ptr::null_mut();
        readback.Map(0, None, Some(&mut pointer)).unwrap();
        for (i, f) in footprints.iter().enumerate() {
            for y in 0..8 {
                for x in 0..8 {
                    let at = f.Offset as usize + y * f.Footprint.RowPitch as usize + x * 4;
                    let pixel = core::slice::from_raw_parts(pointer.cast::<u8>().add(at), 4);
                    let half = if x >= 4 { halves[i].1 } else { halves[i].0 };
                    let colour = COLOURS[half].map(|c| (c * 255.) as u8);
                    assert_eq!(pixel, colour, "indirect target {i} at {x},{y}");
                }
            }
        }
        readback.Unmap(0, None);
    }
}

/// A default buffer's bytes, read back.
pub(super) unsafe fn read_buffer(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    value: u64,
    buffer: &ID3D12Resource,
    bytes: u64,
) -> Vec<u8> {
    unsafe {
        let readback = contents::committed(
            device,
            D3D12_HEAP_TYPE_READBACK,
            &contents::buffer_desc(bytes),
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        list.CopyBufferRegion(&readback, 0, buffer, 0, bytes);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, value);
        let mut p = core::ptr::null_mut();
        readback.Map(0, None, Some(&mut p)).unwrap();
        let out = core::slice::from_raw_parts(p.cast::<u8>(), bytes as usize).to_vec();
        readback.Unmap(0, None);
        out
    }
}

/// The `indirect` capture (see the module's documentation). Fence values `first` to `first + 4` are used.
pub(super) unsafe fn capture(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    first: u64,
) {
    unsafe {
        use graphics::{compile, transition};
        let a = draw_a(device);
        let c = dispatch_c(device);
        // The copier writes the GPU's records at byte 40 of their buffer and its count at byte 12 of another.
        let copier = compile(
            b"ByteAddressBuffer src:register(t0); RWByteAddressBuffer dst:register(u0); \
              RWByteAddressBuffer count:register(u1); \
              [numthreads(30,1,1)] void main(uint i:SV_DispatchThreadID) { \
                dst.Store(40 + i*4, src.Load(i*4)); if (i == 0) count.Store(12, 2); }",
            s!("cs_5_0"),
        );
        let copier_root = root_signature(
            device,
            &[
                root_view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0),
                root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0),
                root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 1),
            ],
            D3D12_ROOT_SIGNATURE_FLAG_NONE,
        );
        let copier = compute_pso(device, &copier_root, &copier);
        // B: positions pulled through a root SRV by SV_VertexID, the index buffer's values; colour in root constants.
        let vs = compile(
            b"StructuredBuffer<float2> p:register(t0); \
              float4 main(uint id:SV_VertexID):SV_Position { return float4(p[id],0,1); }",
            s!("vs_5_0"),
        );
        let ps = compile(
            b"cbuffer C:register(b0) { float4 colour; }; float4 main():SV_Target { return colour; }",
            s!("ps_5_0"),
        );
        let root_b = root_signature(
            device,
            &[root_constants(0, 4), root_view(D3D12_ROOT_PARAMETER_TYPE_SRV, 0)],
            D3D12_ROOT_SIGNATURE_FLAG_NONE,
        );
        let pso_b = graphics_pso(device, &root_b, &vs, &ps, &[]);
        let signature_b = signature(
            device,
            &[
                D3D12_INDIRECT_ARGUMENT_DESC {
                    Type: D3D12_INDIRECT_ARGUMENT_TYPE_INDEX_BUFFER_VIEW,
                    ..Default::default()
                },
                argument(D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT, 0, 4),
                argument(D3D12_INDIRECT_ARGUMENT_TYPE_SHADER_RESOURCE_VIEW, 1, 0),
                argument(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW_INDEXED, 0, 0),
            ],
            STRIDE_B,
            Some(&root_b),
        );
        // D: a mesh dispatch, colour and rectangle from a root CBV.
        let root_d = root_signature(
            device,
            &[root_view(D3D12_ROOT_PARAMETER_TYPE_CBV, 0)],
            D3D12_ROOT_SIGNATURE_FLAG_NONE,
        );
        let mesh = mesh_pso(device, &root_d);
        let signature_d = mesh.is_ok().then(|| {
            signature(
                device,
                &[
                    argument(D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT_BUFFER_VIEW, 0, 0),
                    D3D12_INDIRECT_ARGUMENT_DESC {
                        Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_MESH,
                        ..Default::default()
                    },
                ],
                STRIDE_D,
                Some(&root_d),
            )
        });
        match &mesh {
            Ok(_) => println!("indirect: mesh dispatch included"),
            Err(why) => println!("indirect: mesh dispatch SKIPPED: {why}"),
        }
        // F: an incrementing constant.
        let f = incrementing_f(device);
        match &f {
            Ok(_) => println!("indirect: incrementing constant included"),
            Err(why) => println!("indirect: incrementing constant SKIPPED: {why}"),
        }

        // The buffers only records name, filled before the frame: quads, indices, positions, mesh constants.
        let vertices = default_buffer(device, 512, false);
        let indices = default_buffer(device, 256, false);
        let positions = default_buffer(device, 512, false);
        let mesh_constants = default_buffer(device, 512, false);
        // And one the bundle binds itself.
        let bundle_vertices = default_buffer(device, 512, false);
        // Both halves index vertices 0-5, from byte 0 and byte 64; their root SRVs say which quad those are.
        let mut index_bytes: Vec<u8> = (0u16..6).flat_map(|i| i.to_le_bytes()).collect();
        index_bytes.resize(64, 0);
        index_bytes.extend_from_within(..64);
        let mut mesh_bytes = Vec::new();
        for (colour, rect) in [
            (COLOURS[10], [-1f32, 0., 0., 0.]),
            (COLOURS[11], [0., 1., 0., 0.]),
        ] {
            let mut b: Vec<u8> = colour.iter().chain(&rect).flat_map(|f| f.to_le_bytes()).collect();
            b.resize(256, 0);
            mesh_bytes.extend(b);
        }
        let staging = [
            upload(device, &halves(), 512),
            upload(device, &index_bytes, 256),
            upload(device, &halves(), 512),
            upload(device, &mesh_bytes, 512),
        ];
        // The positions are the vertex buffer's bytes: each quad's float2s.
        fill(
            device,
            queue,
            fence,
            first,
            &[
                (&vertices, &staging[0], 512),
                (&indices, &staging[1], 256),
                (&positions, &staging[2], 512),
                (&mesh_constants, &staging[3], 512),
                (&bundle_vertices, &staging[0], 512),
            ],
        );
        let constants = upload(device, &colours(), COLOURS.len() as u64 * 256);
        let (vertex_va, constant_va) = (vertices.GetGPUVirtualAddress(), constants.GetGPUVirtualAddress());
        let cpu_args = upload(device, &records_a(vertex_va, constant_va, (0, 1)), 256);
        // The GPU's records: two to run, and a third past the count that names no buffer.
        let mut source = records_a(vertex_va, constant_va, (2, 3));
        source.extend(NOWHERE.to_le_bytes());
        words(&mut source, &[48, 8]);
        source.extend(NOWHERE.to_le_bytes());
        words(&mut source, &[6, 1, 0, 0]);
        let source = upload(device, &source, 256);
        let gpu_args = default_buffer(device, 256, true);
        let count = default_buffer(device, 256, true);
        // The bundle's: a draw of a quad, its vertices bound by the bundle.
        let mut e_records = Vec::new();
        words(&mut e_records, &[6, 1, 0, 0]);
        assert_eq!(e_records.len(), STRIDE_E);
        let bundle_args = upload(device, &e_records, 256);
        let mut b_records = Vec::new();
        for (half, colour) in [(0u64, 4usize), (1, 5)] {
            b_records.extend((indices.GetGPUVirtualAddress() + half * 64).to_le_bytes());
            words(&mut b_records, &[12, DXGI_FORMAT_R16_UINT.0 as u32]);
            words(&mut b_records, &COLOURS[colour].map(f32::to_bits));
            b_records.extend((positions.GetGPUVirtualAddress() + half * 256).to_le_bytes());
            words(&mut b_records, &[6, 1, 0, 0, 0]);
        }
        assert_eq!(b_records.len(), 2 * STRIDE_B);
        let b_args = upload(device, &b_records, 256);
        // C writes what the frame first copies in: words 0-3 and 64-67 of `out`, from root UAVs in its records.
        let out = default_buffer(device, 512, true);
        let out_init: Vec<u8> = (0..512u32).map(|i| (i * 7) as u8).collect();
        let out_init = upload(device, &out_init, 512);
        let mut c_records = record_c(out.GetGPUVirtualAddress(), 0x100, 1);
        c_records.extend(record_c(out.GetGPUVirtualAddress() + 256, 0x200, 1));
        let c_args = upload(device, &c_records, 256);
        let mut d_records = Vec::new();
        for half in 0..2u64 {
            d_records.extend((mesh_constants.GetGPUVirtualAddress() + half * 256).to_le_bytes());
            words(&mut d_records, &[1, 1, 1]);
        }
        let d_args = upload(device, &d_records, 256);
        let mut f_records = Vec::new();
        words(&mut f_records, &[6, 1, 0, 0, 6, 1, 0, 0]);
        assert_eq!(f_records.len(), 2 * STRIDE_F);
        let f_args = upload(device, &f_records, 256);

        // Each target's halves' colours: A's two, B's, the bundle's, then F's and D's where they run.
        let mut halves: Vec<(usize, usize)> = vec![(0, 1), (2, 3), (4, 5), (6, 7)];
        if f.is_ok() {
            halves.push((8, 9));
        }
        if mesh.is_ok() {
            halves.push((10, 11));
        }
        let n = halves.len();
        let (targets, _rtvs, rtv) = targets(device, n);
        // A bundle recorded before the frame, as engines record them once: an ExecuteIndirect of draws alone for each
        // half, its vertex buffer and colour set before it.
        let signature_e = signature(
            device,
            &[argument(D3D12_INDIRECT_ARGUMENT_TYPE_DRAW, 0, 0)],
            STRIDE_E,
            None,
        );
        let bundle_allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_BUNDLE)
            .unwrap();
        let bundle: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_BUNDLE, &bundle_allocator, &a.pso)
            .unwrap();
        bundle.SetGraphicsRootSignature(&a.root);
        bundle.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        for (half, colour) in [(0u64, 6u64), (1, 7)] {
            bundle.IASetVertexBuffers(
                0,
                Some(&[D3D12_VERTEX_BUFFER_VIEW {
                    BufferLocation: bundle_vertices.GetGPUVirtualAddress() + half * 256,
                    SizeInBytes: 48,
                    StrideInBytes: 8,
                }]),
            );
            bundle.SetGraphicsRootConstantBufferView(0, constant_va + colour * 256);
            bundle.ExecuteIndirect(&signature_e, 1, &bundle_args, 0, None, 0);
        }
        bundle.Close().unwrap();

        // The frame's list is recorded armed, so its pass ends, outputs and records are copied.
        let label = format!("indirect-{}", std::process::id());
        request(&label);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, &copier)
            .unwrap();
        list.CopyBufferRegion(&out, 0, &out_init, 0, 512);
        transition(
            &list,
            &out,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        for b in [&gpu_args, &count] {
            transition(
                &list,
                b,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            );
        }
        list.SetComputeRootSignature(&copier_root);
        list.SetComputeRootShaderResourceView(0, source.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(1, gpu_args.GetGPUVirtualAddress());
        list.SetComputeRootUnorderedAccessView(2, count.GetGPUVirtualAddress());
        list.Dispatch(1, 1, 1);
        for b in [&gpu_args, &count] {
            transition(
                &list,
                b,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            );
        }
        list.SetPipelineState(&a.pso);
        list.SetGraphicsRootSignature(&a.root);
        list.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        list.RSSetViewports(&[D3D12_VIEWPORT {
            Width: 8.,
            Height: 8.,
            MaxDepth: 1.,
            ..Default::default()
        }]);
        list.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: 8,
            bottom: 8,
        }]);
        let target = |i: usize| {
            list.ClearRenderTargetView(rtv[i], &[0., 0., 0., 1.], None);
            list.OMSetRenderTargets(1, Some(&rtv[i]), false, None);
        };
        // CPU records; GPU records from byte 40, 2 of at most 3 by the count at byte 12.
        target(0);
        list.ExecuteIndirect(&a.signature, 2, &cpu_args, 0, None, 0);
        target(1);
        list.ExecuteIndirect(&a.signature, 3, &gpu_args, 40, &count, 12);
        // Index buffer views, root constants and root SRVs.
        list.SetPipelineState(&pso_b);
        list.SetGraphicsRootSignature(&root_b);
        target(2);
        list.ExecuteIndirect(&signature_b, 2, &b_args, 0, None, 0);
        // The bundle's records.
        list.SetPipelineState(&a.pso);
        list.SetGraphicsRootSignature(&a.root);
        target(3);
        list.ExecuteBundle(&bundle);
        let mut next = 4;
        if let Ok(f) = &f {
            list.SetPipelineState(&f.pso);
            list.SetGraphicsRootSignature(&f.root);
            // A stale index: if the commands ran without the runtime's, both would draw off the target.
            list.SetGraphicsRoot32BitConstants(0, 2, [0u32, 7].as_ptr().cast(), 0);
            target(next);
            list.ExecuteIndirect(&f.signature, 2, &f_args, 0, None, 0);
            next += 1;
        }
        if let (Ok(pso), Some(signature)) = (&mesh, &signature_d) {
            list.SetPipelineState(pso);
            list.SetGraphicsRootSignature(&root_d);
            target(next);
            list.ExecuteIndirect(signature, 2, &d_args, 0, None, 0);
        }
        // Root UAVs in records: what the dispatch writes is known only through them.
        list.SetPipelineState(&c.pso);
        list.SetComputeRootSignature(&c.root);
        list.ExecuteIndirect(&c.signature, 2, &c_args, 0, None, 0);
        transition(
            &list,
            &out,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, first + 1);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();

        // The game's own pictures and output: what verify's replay must make again.
        check_targets(device, queue, fence, first + 2, &targets, &halves);
        let written_out = read_buffer(device, queue, fence, first + 3, &out, 512);
        for i in 0..128usize {
            let word = u32::from_le_bytes(written_out[i * 4..i * 4 + 4].try_into().unwrap());
            let expected = match i {
                0..4 => 0x100 + i as u32,
                64..68 => 0x200 + (i as u32 - 64),
                _ => u32::from_le_bytes(core::array::from_fn(|b| ((i * 4 + b) * 7) as u8)),
            };
            assert_eq!(word, expected, "out word {i}");
        }

        let db = written(&label);
        let n = n as i64;
        let (with_f, with_mesh) = (i64::from(f.is_ok()), i64::from(mesh.is_ok()));
        let ei = "d.call = 'ExecuteIndirect'";
        check(
            &db,
            &[
                // A1, A2, B, the bundle's two, C, and F and D where they ran.
                (&format!("SELECT count(*) FROM draws d WHERE {ei}"), n + 2),
                // F's signature: the incrementing constant described without a count, and a record of 16 bytes.
                (
                    "SELECT count(*) FROM d12_objects WHERE kind = 'ID3D12CommandSignature' AND \
                     json_extract(description, '$.arguments[0].type') = 11 AND \
                     json_extract(description, '$.arguments[0].offset') = 1 AND \
                     json_extract(description, '$.arguments[0].count') IS NULL AND \
                     json_extract(description, '$.stride') = 16",
                    with_f,
                ),
                ("SELECT count(*) FROM draws WHERE call = 'Dispatch'", 1),
                ("SELECT count(*) FROM targets", n),
                (
                    "SELECT count(*) FROM meta WHERE key = 'environment' AND json_extract(value, '$.adapter.luid') IS NOT NULL",
                    1,
                ),
                // The buffers only records name: each ExecuteIndirect but the bundle's reads those its records name,
                // and their contents were copied after the frame.
                (
                    "SELECT count(*) FROM resources WHERE content_source = 'after-frame' AND used = 1",
                    3 + with_mesh,
                ),
                (
                    &format!(
                        "SELECT count(DISTINCT e.event) FROM event_resources e JOIN draws d ON d.event = e.event \
                         JOIN resources r ON r.id = e.resource WHERE {ei} AND r.content_source = 'after-frame'"
                    ),
                    3 + with_mesh,
                ),
                (
                    "SELECT count(*) FROM gaps WHERE what = 'd3d12-indirect-arguments'",
                    0,
                ),
                (
                    "SELECT count(*) FROM gaps WHERE what IN ('d3d12-results', 'contents')",
                    0,
                ),
                // What C wrote through its records' root UAVs: an output of its event, which writes `out`.
                (
                    &format!(
                        "SELECT count(*) FROM results o JOIN draws d ON d.event = o.event JOIN resources r ON \
                         r.id = o.resource WHERE {ei} AND r.width = 512 AND r.content_source = 'first-use'"
                    ),
                    1,
                ),
                (
                    &format!(
                        "SELECT count(*) FROM event_resources e JOIN draws d ON d.event = e.event JOIN resources r \
                         ON r.id = e.resource WHERE {ei} AND e.access = 'write' AND r.width = 512"
                    ),
                    1,
                ),
            ],
        );
        finish(queue, fence, first + 4);
        debug_errors(device);
        println!(
            "D3D12 ExecuteIndirect: vertex and index buffer views, root CBVs, SRVs, UAVs and constants, a GPU count and \
             offsets, a bundle{}{}: every half drawn and every word written: {label}",
            if with_f == 1 {
                ", an incrementing constant"
            } else {
                ""
            },
            if with_mesh == 1 {
                " and mesh dispatches"
            } else {
                ""
            }
        );
    }
}

/// The `indirect-gaps` capture (see the module's documentation). Fence values `first` to `first + 3` are used.
pub(super) unsafe fn gaps(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    first: u64,
) {
    unsafe {
        use graphics::transition;
        let a = draw_a(device);
        let c = dispatch_c(device);
        let vertices = default_buffer(device, 512, false);
        let staging = upload(device, &halves(), 512);
        fill(device, queue, fence, first, &[(&vertices, &staging, 512)]);
        let constants = upload(device, &colours(), COLOURS.len() as u64 * 256);
        let args = upload(
            device,
            &records_a(
                vertices.GetGPUVirtualAddress(),
                constants.GetGPUVirtualAddress(),
                (0, 1),
            ),
            256,
        );
        // Only this record's root UAV names `only`, and the dispatch writes every word of it.
        let only = default_buffer(device, 256, true);
        let c_args = upload(device, &record_c(only.GetGPUVirtualAddress(), 0x300, 16), 256);
        let (targets, _rtvs, rtv) = targets(device, 1);

        // Recorded before the capture is armed, so not instrumented: its records are not copied.
        let early_allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let early: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &early_allocator, &a.pso)
            .unwrap();
        early.SetGraphicsRootSignature(&a.root);
        early.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        early.RSSetViewports(&[D3D12_VIEWPORT {
            Width: 8.,
            Height: 8.,
            MaxDepth: 1.,
            ..Default::default()
        }]);
        early.RSSetScissorRects(&[RECT {
            left: 0,
            top: 0,
            right: 8,
            bottom: 8,
        }]);
        early.ClearRenderTargetView(rtv[0], &[0., 0., 0., 1.], None);
        early.OMSetRenderTargets(1, Some(&rtv[0]), false, None);
        early.ExecuteIndirect(&a.signature, 2, &args, 0, None, 0);
        early.Close().unwrap();

        let label = format!("indirect-gaps-{}", std::process::id());
        request(&label);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, &c.pso)
            .unwrap();
        transition(
            &list,
            &only,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        list.SetComputeRootSignature(&c.root);
        list.ExecuteIndirect(&c.signature, 1, &c_args, 0, None, 0);
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(early.cast().unwrap()), Some(list.cast().unwrap())]);
        finish(queue, fence, first + 1);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        check_targets(device, queue, fence, first + 2, &targets, &[(0, 1)]);

        let db = written(&label);
        check(
            &db,
            &[
                (
                    "SELECT count(*) FROM gaps WHERE what = 'd3d12-indirect-arguments' AND detail LIKE \
                     'its list was not instrumented%' AND count = 1",
                    1,
                ),
                // Named only by the records' root UAV: written, so its contents as the frame began are not known;
                // what the dispatch wrote is its output, copied after the frame.
                (
                    "SELECT count(*) FROM resources WHERE width = 256 AND used = 1 AND written = 1 AND content IS NULL \
                     AND content_note LIKE 'only ExecuteIndirect argument records, shader tables or instance pointers name it, and a UAV%'",
                    1,
                ),
                (
                    "SELECT count(*) FROM results o JOIN resources r ON r.id = o.resource WHERE r.width = 256 \
                     AND r.content IS NULL",
                    1,
                ),
                (
                    "SELECT count(*) FROM resources WHERE content_source = 'after-frame'",
                    0,
                ),
            ],
        );
        finish(queue, fence, first + 3);
        debug_errors(device);
        println!(
            "D3D12 ExecuteIndirect gaps: an uninstrumented list's records, and a buffer only a root UAV names: {label}"
        );
    }
}

/// The `indirect-rays` capture's ray generation shader: each ray writes its index into the UAV, so a dispatch that ran
/// is seen.
const RAYS: &str = r#"
RWByteAddressBuffer written : register(u0);

[shader("raygeneration")]
void Ray()
{
    uint2 at = DispatchRaysIndex().xy;
    written.Store((at.y * DispatchRaysDimensions().x + at.x) * 4, 0x52590000 | (at.y << 8) | at.x);
}
"#;
/// The rays the indirect DispatchRays casts, wide and high.
const RAYS_CAST: (u32, u32) = (4, 2);
/// A record of signature G: DISPATCH_RAYS (`D3D12_DISPATCH_RAYS_DESC`, its three sizes padded to 8 bytes).
const STRIDE_G: usize = 104;

/// What ray `(x, y)` writes.
fn ray(x: u32, y: u32) -> u32 {
    0x5259_0000 | (y << 8) | x
}

/// A raytracing pipeline of [`RAYS`] with `root` as its global root signature, where the device has raytracing tier
/// 1.1 (ExecuteIndirect of DispatchRays) and DXIL can be compiled; or why not.
unsafe fn rays_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
) -> std::result::Result<ID3D12StateObject, String> {
    unsafe {
        let mut o5 = D3D12_FEATURE_DATA_D3D12_OPTIONS5::default();
        device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS5,
                (&mut o5 as *mut D3D12_FEATURE_DATA_D3D12_OPTIONS5).cast(),
                size_of::<D3D12_FEATURE_DATA_D3D12_OPTIONS5>() as u32,
            )
            .map_err(|e| format!("CheckFeatureSupport(OPTIONS5): {e}"))?;
        if o5.RaytracingTier.0 < D3D12_RAYTRACING_TIER_1_1.0 {
            return Err(format!(
                "raytracing tier {}.{}, not 1.1",
                o5.RaytracingTier.0 / 10,
                o5.RaytracingTier.0 % 10
            ));
        }
        let library = dxil(RAYS, "lib_6_3")?;
        let dxil_library = D3D12_DXIL_LIBRARY_DESC {
            DXILLibrary: D3D12_SHADER_BYTECODE {
                pShaderBytecode: library.as_ptr().cast(),
                BytecodeLength: library.len(),
            },
            NumExports: 0,
            pExports: core::ptr::null_mut(),
        };
        let mut global = D3D12_GLOBAL_ROOT_SIGNATURE {
            pGlobalRootSignature: ManuallyDrop::new(Some(root.clone())),
        };
        let shader = D3D12_RAYTRACING_SHADER_CONFIG {
            MaxPayloadSizeInBytes: 4,
            MaxAttributeSizeInBytes: 8,
        };
        let pipeline = D3D12_RAYTRACING_PIPELINE_CONFIG {
            MaxTraceRecursionDepth: 1,
        };
        let subobject = |kind, desc: *const c_void| D3D12_STATE_SUBOBJECT {
            Type: kind,
            pDesc: desc,
        };
        let subobjects = [
            subobject(
                D3D12_STATE_SUBOBJECT_TYPE_DXIL_LIBRARY,
                (&dxil_library as *const D3D12_DXIL_LIBRARY_DESC).cast(),
            ),
            subobject(
                D3D12_STATE_SUBOBJECT_TYPE_GLOBAL_ROOT_SIGNATURE,
                (&global as *const D3D12_GLOBAL_ROOT_SIGNATURE).cast(),
            ),
            subobject(
                D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_SHADER_CONFIG,
                (&shader as *const D3D12_RAYTRACING_SHADER_CONFIG).cast(),
            ),
            subobject(
                D3D12_STATE_SUBOBJECT_TYPE_RAYTRACING_PIPELINE_CONFIG,
                (&pipeline as *const D3D12_RAYTRACING_PIPELINE_CONFIG).cast(),
            ),
        ];
        let made = device
            .cast::<ID3D12Device5>()
            .map_err(|e| format!("ID3D12Device5: {e}"))
            .and_then(|d| {
                d.CreateStateObject(&D3D12_STATE_OBJECT_DESC {
                    Type: D3D12_STATE_OBJECT_TYPE_RAYTRACING_PIPELINE,
                    NumSubobjects: subobjects.len() as u32,
                    pSubobjects: subobjects.as_ptr(),
                })
                .map_err(|e| format!("CreateStateObject: {e}"))
            });
        ManuallyDrop::drop(&mut global.pGlobalRootSignature);
        made
    }
}

/// The `indirect-rays` capture: an ExecuteIndirect of signature G, whose record names a ray generation shader table in
/// a default heap that nothing else names, where the device can (see [`rays_pipeline`]). Fence values `first` to
/// `first + 3` are used.
pub(super) unsafe fn rays(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    fence: &ID3D12Fence,
    chain: &IDXGISwapChain1,
    first: u64,
) -> std::result::Result<(), String> {
    unsafe {
        use graphics::transition;
        let root = root_signature(
            device,
            &[root_view(D3D12_ROOT_PARAMETER_TYPE_UAV, 0)],
            D3D12_ROOT_SIGNATURE_FLAG_NONE,
        );
        let pipeline = rays_pipeline(device, &root)?;
        let properties: ID3D12StateObjectProperties = pipeline.cast().unwrap();
        let identifier = properties.GetShaderIdentifier(w!("Ray"));
        assert!(!identifier.is_null(), "Ray's shader identifier");
        let mut record = core::slice::from_raw_parts(
            identifier.cast::<u8>(),
            D3D12_SHADER_IDENTIFIER_SIZE_IN_BYTES as usize,
        )
        .to_vec();
        record.resize(D3D12_RAYTRACING_SHADER_TABLE_BYTE_ALIGNMENT as usize, 0);
        let table = default_buffer(device, record.len() as u64, false);
        let staging = upload(device, &record, record.len() as u64);
        fill(
            device,
            queue,
            fence,
            first,
            &[(&table, &staging, record.len() as u64)],
        );
        let (width, height) = RAYS_CAST;
        let bytes = u64::from(width * height * 4);
        let target = default_buffer(device, bytes, true);
        let desc = D3D12_DISPATCH_RAYS_DESC {
            RayGenerationShaderRecord: D3D12_GPU_VIRTUAL_ADDRESS_RANGE {
                StartAddress: table.GetGPUVirtualAddress(),
                SizeInBytes: u64::from(D3D12_SHADER_IDENTIFIER_SIZE_IN_BYTES),
            },
            Width: width,
            Height: height,
            Depth: 1,
            ..Default::default()
        };
        assert_eq!(size_of::<D3D12_DISPATCH_RAYS_DESC>(), STRIDE_G);
        let args = upload(
            device,
            core::slice::from_raw_parts((&desc as *const D3D12_DISPATCH_RAYS_DESC).cast(), STRIDE_G),
            256,
        );
        let g = signature(
            device,
            &[argument(D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH_RAYS, 0, 0)],
            STRIDE_G,
            None,
        );

        let label = format!("indirect-rays-{}", std::process::id());
        request(&label);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList4 = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        transition(
            &list,
            &table,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        );
        transition(
            &list,
            &target,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        );
        list.SetComputeRootSignature(&root);
        list.SetComputeRootUnorderedAccessView(0, target.GetGPUVirtualAddress());
        list.SetPipelineState1(&pipeline);
        list.ExecuteIndirect(&g, 1, &args, 0, None, 0);
        transition(
            &list,
            &target,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COPY_SOURCE,
        );
        list.Close().unwrap();
        queue.ExecuteCommandLists(&[Some(list.cast().unwrap())]);
        finish(queue, fence, first + 1);
        chain.Present(0, DXGI_PRESENT(0)).ok().unwrap();
        let out = read_buffer(device, queue, fence, first + 2, &target, bytes);
        for y in 0..height {
            for x in 0..width {
                let at = ((y * width + x) * 4) as usize;
                let word = u32::from_le_bytes(out[at..at + 4].try_into().unwrap());
                assert_eq!(word, ray(x, y), "ray ({x}, {y})");
            }
        }

        let db = written(&label);
        let ei = "d.call = 'ExecuteIndirect'";
        check(
            &db,
            &[
                (
                    "SELECT count(*) FROM gaps WHERE what = 'd3d12-indirect-arguments'",
                    0,
                ),
                // The shader table only the record names: read by the ExecuteIndirect, its contents copied after the
                // frame.
                (
                    &format!(
                        "SELECT count(*) FROM event_resources e JOIN draws d ON d.event = e.event JOIN resources r \
                         ON r.id = e.resource WHERE {ei} AND e.access = 'read' AND r.width = {} AND \
                         r.content_source = 'after-frame'",
                        record.len()
                    ),
                    1,
                ),
            ],
        );
        finish(queue, fence, first + 3);
        debug_errors(device);
        println!("D3D12 ExecuteIndirect of DispatchRays: every ray written, its shader table read: {label}");
        Ok(())
    }
}
