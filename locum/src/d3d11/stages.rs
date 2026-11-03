//! A shader stage's bindings on a real context, by stage. Direct3D 11 has a copy of each getter and setter per stage
//! (`VSGetShader`, `PSGetShader`, ...); these pick the right one.

use windows::Win32::Graphics::Direct3D11::*;
use windows::core::{Interface, Result};

use crate::d3d11::shaders::Stage;

/// Constant-buffer slots per stage.
pub const CB_SLOTS: u32 = D3D11_COMMONSHADER_CONSTANT_BUFFER_API_SLOT_COUNT;
/// Shader resource view slots per stage.
pub const SRV_SLOTS: usize = D3D11_COMMONSHADER_INPUT_RESOURCE_SLOT_COUNT as usize;

/// Sampler slots per stage.
pub const SAMPLER_SLOTS: usize = D3D11_COMMONSHADER_SAMPLER_SLOT_COUNT as usize;

/// A constant buffer bound to a slot, with its range in constants (16 bytes each).
pub struct CbBinding {
    pub buffer: Option<ID3D11Buffer>,
    pub first: u32,
    pub count: u32,
}

macro_rules! per_stage {
    ($($stage:ident => $ty:ident, $get:ident, $set:ident, $get_cbs:ident, $set_cbs:ident, $get_srvs:ident, $get_samplers:ident, $create:ident;)*) => {
        /// The shader bound to `stage`.
        pub unsafe fn shader(c: &ID3D11DeviceContext4, stage: Stage) -> Option<ID3D11DeviceChild> {
            match stage {
                $(Stage::$stage => {
                    let mut s = None;
                    unsafe { c.$get(&mut s, None, None) };
                    s.map(|s| s.cast().expect("a shader is a device child"))
                })*
            }
        }

        /// Binds `shader` (which must be a shader of `stage`) to `stage`, without class instances.
        pub unsafe fn set_shader(c: &ID3D11DeviceContext4, stage: Stage, shader: Option<&ID3D11DeviceChild>) {
            match stage {
                $(Stage::$stage => {
                    let s = shader.map(|s| s.cast::<$ty>().expect("a shader of this stage"));
                    unsafe { c.$set(s.as_ref(), None) };
                })*
            }
        }

        /// The constant buffers bound to `stage` from slot 0, with their ranges.
        pub unsafe fn constant_buffers(
            c: &ID3D11DeviceContext4,
            stage: Stage,
        ) -> [CbBinding; CB_SLOTS as usize] {
            let mut buffers: [Option<ID3D11Buffer>; CB_SLOTS as usize] = Default::default();
            let mut firsts = [0u32; CB_SLOTS as usize];
            let mut counts = [0u32; CB_SLOTS as usize];
            let (b, f, n) = (buffers.as_mut_ptr(), firsts.as_mut_ptr(), counts.as_mut_ptr());
            match stage {
                $(Stage::$stage => unsafe { c.$get_cbs(0, CB_SLOTS, Some(b), Some(f), Some(n)) },)*
            }
            let mut i = 0;
            buffers.map(|buffer| {
                let binding = CbBinding { buffer, first: firsts[i], count: counts[i] };
                i += 1;
                binding
            })
        }

        /// Binds one constant buffer to `slot` of `stage`. A range of (0, 4096) is what the runtime reports for a
        /// buffer bound whole, and binds it whole.
        pub unsafe fn set_constant_buffer(c: &ID3D11DeviceContext4, stage: Stage, slot: u32, binding: &CbBinding) {
            let buffers = [binding.buffer.clone()];
            let (first, count) = (binding.first, binding.count);
            let whole = first == 0 && count == D3D11_REQ_CONSTANT_BUFFER_ELEMENT_COUNT;
            let (f, n) = if whole { (None, None) } else { (Some(&first as *const u32), Some(&count as *const u32)) };
            match stage {
                $(Stage::$stage => unsafe { c.$set_cbs(slot, 1, Some(buffers.as_ptr()), f, n) },)*
            }
        }

        /// The shader resource views bound to `stage`.
        pub unsafe fn shader_resources(c: &ID3D11DeviceContext4, stage: Stage) -> [Option<ID3D11ShaderResourceView>; SRV_SLOTS] {
            let mut views = [const { None }; SRV_SLOTS];
            match stage {
                $(Stage::$stage => unsafe { c.$get_srvs(0, Some(&mut views)) },)*
            }
            views
        }

        /// The samplers bound to `stage`.
        pub unsafe fn samplers(c: &ID3D11DeviceContext4, stage: Stage) -> [Option<ID3D11SamplerState>; SAMPLER_SLOTS] {
            let mut samplers = [const { None }; SAMPLER_SLOTS];
            match stage {
                $(Stage::$stage => unsafe { c.$get_samplers(0, Some(&mut samplers)) },)*
            }
            samplers
        }

        /// Creates a shader of `stage` from bytecode on the real device.
        pub unsafe fn create_shader(device: &ID3D11Device, stage: Stage, code: &[u8]) -> Result<ID3D11DeviceChild> {
            match stage {
                $(Stage::$stage => {
                    let mut s = None;
                    unsafe { device.$create(code, None, Some(&mut s)) }?;
                    Ok(s.expect("a created shader").cast().expect("a shader is a device child"))
                })*
            }
        }
    };
}

per_stage! {
    Vertex => ID3D11VertexShader, VSGetShader, VSSetShader, VSGetConstantBuffers1, VSSetConstantBuffers1, VSGetShaderResources, VSGetSamplers, CreateVertexShader;
    Hull => ID3D11HullShader, HSGetShader, HSSetShader, HSGetConstantBuffers1, HSSetConstantBuffers1, HSGetShaderResources, HSGetSamplers, CreateHullShader;
    Domain => ID3D11DomainShader, DSGetShader, DSSetShader, DSGetConstantBuffers1, DSSetConstantBuffers1, DSGetShaderResources, DSGetSamplers, CreateDomainShader;
    Geometry => ID3D11GeometryShader, GSGetShader, GSSetShader, GSGetConstantBuffers1, GSSetConstantBuffers1, GSGetShaderResources, GSGetSamplers, CreateGeometryShader;
    Pixel => ID3D11PixelShader, PSGetShader, PSSetShader, PSGetConstantBuffers1, PSSetConstantBuffers1, PSGetShaderResources, PSGetSamplers, CreatePixelShader;
    Compute => ID3D11ComputeShader, CSGetShader, CSSetShader, CSGetConstantBuffers1, CSSetConstantBuffers1, CSGetShaderResources, CSGetSamplers, CreateComputeShader;
}
