//! Views and samplers from the descriptions a Direct3D 12 capture records: `{dimension, format, mapping, flags,
//! payload}` with the payload's fields named as in the API's structures, or none for a view made without a
//! description (`{"default": true}`, or JSON null). Also the subresources a view covers, for the barriers of the
//! textures replay tracks the state of.

use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

fn u(v: &Value, key: &str) -> u32 {
    v[key].as_u64().map_or(0, |n| n as u32)
}

fn u64_of(v: &Value, key: &str) -> u64 {
    v[key].as_u64().unwrap_or(0)
}

fn f(v: &Value, key: &str) -> f32 {
    v[key].as_f64().unwrap_or(0.0) as f32
}

/// Whether a description names no view: the view a resource's own description gives.
pub fn is_default(desc: &Value) -> bool {
    desc.is_null() || desc["default"] == true
}

fn format(desc: &Value) -> DXGI_FORMAT {
    DXGI_FORMAT(u(desc, "format") as i32)
}

pub fn srv(desc: &Value) -> D3D12_SHADER_RESOURCE_VIEW_DESC {
    let p = &desc["payload"];
    let mut d = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: format(desc),
        ViewDimension: D3D12_SRV_DIMENSION(u(desc, "dimension") as i32),
        Shader4ComponentMapping: desc["mapping"].as_u64().map_or(D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING, |m| m as u32),
        ..Default::default()
    };
    let clamp = f(p, "ResourceMinLODClamp");
    d.Anonymous = match d.ViewDimension {
        D3D12_SRV_DIMENSION_BUFFER => D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_SRV {
                FirstElement: u64_of(p, "FirstElement"),
                NumElements: u(p, "NumElements"),
                StructureByteStride: u(p, "StructureByteStride"),
                Flags: D3D12_BUFFER_SRV_FLAGS(u(p, "Flags") as i32),
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE1D => D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture1D: D3D12_TEX1D_SRV {
                MostDetailedMip: u(p, "MostDetailedMip"),
                MipLevels: u(p, "MipLevels"),
                ResourceMinLODClamp: clamp,
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE1DARRAY => D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture1DArray: D3D12_TEX1D_ARRAY_SRV {
                MostDetailedMip: u(p, "MostDetailedMip"),
                MipLevels: u(p, "MipLevels"),
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
                ResourceMinLODClamp: clamp,
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE2D => D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MostDetailedMip: u(p, "MostDetailedMip"),
                MipLevels: u(p, "MipLevels"),
                PlaneSlice: u(p, "PlaneSlice"),
                ResourceMinLODClamp: clamp,
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE2DARRAY => D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                MostDetailedMip: u(p, "MostDetailedMip"),
                MipLevels: u(p, "MipLevels"),
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
                PlaneSlice: u(p, "PlaneSlice"),
                ResourceMinLODClamp: clamp,
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE2DMS => D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DMS: D3D12_TEX2DMS_SRV::default(),
        },
        D3D12_SRV_DIMENSION_TEXTURE2DMSARRAY => D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2DMSArray: D3D12_TEX2DMS_ARRAY_SRV {
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
            },
        },
        D3D12_SRV_DIMENSION_TEXTURE3D => D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture3D: D3D12_TEX3D_SRV {
                MostDetailedMip: u(p, "MostDetailedMip"),
                MipLevels: u(p, "MipLevels"),
                ResourceMinLODClamp: clamp,
            },
        },
        D3D12_SRV_DIMENSION_TEXTURECUBE => D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            TextureCube: D3D12_TEXCUBE_SRV {
                MostDetailedMip: u(p, "MostDetailedMip"),
                MipLevels: u(p, "MipLevels"),
                ResourceMinLODClamp: clamp,
            },
        },
        D3D12_SRV_DIMENSION_TEXTURECUBEARRAY => D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            TextureCubeArray: D3D12_TEXCUBE_ARRAY_SRV {
                MostDetailedMip: u(p, "MostDetailedMip"),
                MipLevels: u(p, "MipLevels"),
                First2DArrayFace: u(p, "First2DArrayFace"),
                NumCubes: u(p, "NumCubes"),
                ResourceMinLODClamp: clamp,
            },
        },
        _ => D3D12_SHADER_RESOURCE_VIEW_DESC_0::default(),
    };
    d
}

pub fn uav(desc: &Value) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let p = &desc["payload"];
    let mut d = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: format(desc),
        ViewDimension: D3D12_UAV_DIMENSION(u(desc, "dimension") as i32),
        ..Default::default()
    };
    d.Anonymous = match d.ViewDimension {
        D3D12_UAV_DIMENSION_BUFFER => D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_UAV {
                FirstElement: u64_of(p, "FirstElement"),
                NumElements: u(p, "NumElements"),
                StructureByteStride: u(p, "StructureByteStride"),
                CounterOffsetInBytes: u64_of(p, "CounterOffsetInBytes"),
                Flags: D3D12_BUFFER_UAV_FLAGS(u(p, "Flags") as i32),
            },
        },
        D3D12_UAV_DIMENSION_TEXTURE1D => D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture1D: D3D12_TEX1D_UAV { MipSlice: u(p, "MipSlice") },
        },
        D3D12_UAV_DIMENSION_TEXTURE1DARRAY => D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture1DArray: D3D12_TEX1D_ARRAY_UAV {
                MipSlice: u(p, "MipSlice"),
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
            },
        },
        D3D12_UAV_DIMENSION_TEXTURE2D => D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_UAV {
                MipSlice: u(p, "MipSlice"),
                PlaneSlice: u(p, "PlaneSlice"),
            },
        },
        D3D12_UAV_DIMENSION_TEXTURE2DARRAY => D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2DArray: D3D12_TEX2D_ARRAY_UAV {
                MipSlice: u(p, "MipSlice"),
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
                PlaneSlice: u(p, "PlaneSlice"),
            },
        },
        D3D12_UAV_DIMENSION_TEXTURE3D => D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture3D: D3D12_TEX3D_UAV {
                MipSlice: u(p, "MipSlice"),
                FirstWSlice: u(p, "FirstWSlice"),
                WSize: u(p, "WSize"),
            },
        },
        _ => D3D12_UNORDERED_ACCESS_VIEW_DESC_0::default(),
    };
    d
}

pub fn rtv(desc: &Value) -> D3D12_RENDER_TARGET_VIEW_DESC {
    let p = &desc["payload"];
    let mut d = D3D12_RENDER_TARGET_VIEW_DESC {
        Format: format(desc),
        ViewDimension: D3D12_RTV_DIMENSION(u(desc, "dimension") as i32),
        ..Default::default()
    };
    d.Anonymous = match d.ViewDimension {
        D3D12_RTV_DIMENSION_BUFFER => D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Buffer: D3D12_BUFFER_RTV {
                FirstElement: u64_of(p, "FirstElement"),
                NumElements: u(p, "NumElements"),
            },
        },
        D3D12_RTV_DIMENSION_TEXTURE1D => D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Texture1D: D3D12_TEX1D_RTV { MipSlice: u(p, "MipSlice") },
        },
        D3D12_RTV_DIMENSION_TEXTURE1DARRAY => D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Texture1DArray: D3D12_TEX1D_ARRAY_RTV {
                MipSlice: u(p, "MipSlice"),
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
            },
        },
        D3D12_RTV_DIMENSION_TEXTURE2D => D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_RTV {
                MipSlice: u(p, "MipSlice"),
                PlaneSlice: u(p, "PlaneSlice"),
            },
        },
        D3D12_RTV_DIMENSION_TEXTURE2DARRAY => D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Texture2DArray: D3D12_TEX2D_ARRAY_RTV {
                MipSlice: u(p, "MipSlice"),
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
                PlaneSlice: u(p, "PlaneSlice"),
            },
        },
        D3D12_RTV_DIMENSION_TEXTURE2DMS => D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Texture2DMS: D3D12_TEX2DMS_RTV::default(),
        },
        D3D12_RTV_DIMENSION_TEXTURE2DMSARRAY => D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Texture2DMSArray: D3D12_TEX2DMS_ARRAY_RTV {
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
            },
        },
        D3D12_RTV_DIMENSION_TEXTURE3D => D3D12_RENDER_TARGET_VIEW_DESC_0 {
            Texture3D: D3D12_TEX3D_RTV {
                MipSlice: u(p, "MipSlice"),
                FirstWSlice: u(p, "FirstWSlice"),
                WSize: u(p, "WSize"),
            },
        },
        _ => D3D12_RENDER_TARGET_VIEW_DESC_0::default(),
    };
    d
}

pub fn dsv(desc: &Value) -> D3D12_DEPTH_STENCIL_VIEW_DESC {
    let p = &desc["payload"];
    let mut d = D3D12_DEPTH_STENCIL_VIEW_DESC {
        Format: format(desc),
        ViewDimension: D3D12_DSV_DIMENSION(u(desc, "dimension") as i32),
        Flags: D3D12_DSV_FLAGS(u(desc, "flags") as i32),
        ..Default::default()
    };
    d.Anonymous = match d.ViewDimension {
        D3D12_DSV_DIMENSION_TEXTURE1D => D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
            Texture1D: D3D12_TEX1D_DSV { MipSlice: u(p, "MipSlice") },
        },
        D3D12_DSV_DIMENSION_TEXTURE1DARRAY => D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
            Texture1DArray: D3D12_TEX1D_ARRAY_DSV {
                MipSlice: u(p, "MipSlice"),
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
            },
        },
        D3D12_DSV_DIMENSION_TEXTURE2D => D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_DSV { MipSlice: u(p, "MipSlice") },
        },
        D3D12_DSV_DIMENSION_TEXTURE2DARRAY => D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
            Texture2DArray: D3D12_TEX2D_ARRAY_DSV {
                MipSlice: u(p, "MipSlice"),
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
            },
        },
        D3D12_DSV_DIMENSION_TEXTURE2DMS => D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
            Texture2DMS: D3D12_TEX2DMS_DSV::default(),
        },
        D3D12_DSV_DIMENSION_TEXTURE2DMSARRAY => D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
            Texture2DMSArray: D3D12_TEX2DMS_ARRAY_DSV {
                FirstArraySlice: u(p, "FirstArraySlice"),
                ArraySize: u(p, "ArraySize"),
            },
        },
        _ => D3D12_DEPTH_STENCIL_VIEW_DESC_0::default(),
    };
    d
}

pub fn sampler(desc: &Value) -> D3D12_SAMPLER_DESC {
    let border = &desc["BorderColor"];
    D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER(u(desc, "Filter") as i32),
        AddressU: D3D12_TEXTURE_ADDRESS_MODE(u(desc, "AddressU") as i32),
        AddressV: D3D12_TEXTURE_ADDRESS_MODE(u(desc, "AddressV") as i32),
        AddressW: D3D12_TEXTURE_ADDRESS_MODE(u(desc, "AddressW") as i32),
        MipLODBias: f(desc, "MipLODBias"),
        MaxAnisotropy: u(desc, "MaxAnisotropy"),
        ComparisonFunc: D3D12_COMPARISON_FUNC(u(desc, "ComparisonFunc") as i32),
        BorderColor: [0, 1, 2, 3].map(|i| border[i].as_f64().unwrap_or(0.0) as f32),
        MinLOD: f(desc, "MinLOD"),
        MaxLOD: f(desc, "MaxLOD"),
    }
}

/// The resource a constant buffer description reads, the offset into it and the bytes, or None for a null view.
pub fn cbv(desc: &Value) -> Option<(i64, u64, u32)> {
    let b = &desc["buffer"];
    Some((b["resource"].as_i64()?, b["offset"].as_u64()?, u(desc, "bytes")))
}

/// What a resource is made of, as subresources count them.
#[derive(Clone, Copy, Debug)]
pub struct Extent {
    pub mips: u32,
    /// Array slices (1 for a volume).
    pub slices: u32,
    pub planes: u32,
}

impl Extent {
    pub fn count(&self) -> u32 {
        self.mips * self.slices * self.planes
    }

    pub fn index(&self, mip: u32, slice: u32, plane: u32) -> u32 {
        mip + slice * self.mips + plane * self.mips * self.slices
    }
}

/// The subresources a view of `kind` with description `desc` covers in a texture of extent `e`; all of them for a
/// view without a description.
pub fn subresources(kind: &str, desc: &Value, e: Extent) -> Vec<u32> {
    let all = || (0..e.count()).collect();
    if is_default(desc) {
        return all();
    }
    let p = &desc["payload"];
    let dim = u(desc, "dimension");
    let span = |first: u32, count: u32, limit: u32| {
        let first = first.min(limit);
        first..if count == u32::MAX { limit } else { (first + count).min(limit) }
    };
    // Mips, slices and planes, by the dimension's numbering in each kind of view.
    let (mips, slices, planes) = match (kind, dim) {
        ("srv", 2 | 4 | 8) =>(span(u(p, "MostDetailedMip"), u(p, "MipLevels"), e.mips), 0..1, plane(p, e)),
        ("srv", 3 | 5) => (
            span(u(p, "MostDetailedMip"), u(p, "MipLevels"), e.mips),
            span(u(p, "FirstArraySlice"), u(p, "ArraySize"), e.slices),
            plane(p, e),
        ),
        ("srv", 6) => (0..1, 0..1, 0..1),
        ("srv", 7) => (0..1, span(u(p, "FirstArraySlice"), u(p, "ArraySize"), e.slices), 0..1),
        ("srv", 10) => (
            span(u(p, "MostDetailedMip"), u(p, "MipLevels"), e.mips),
            span(u(p, "First2DArrayFace"), u(p, "NumCubes").saturating_mul(6), e.slices),
            0..1,
        ),
        ("srv", 9) => (span(u(p, "MostDetailedMip"), u(p, "MipLevels"), e.mips), 0..6.min(e.slices), 0..1),
        ("rtv" | "uav", 2 | 4 | 8) => (span(u(p, "MipSlice"), 1, e.mips), 0..1, plane(p, e)),
        ("rtv" | "uav", 3 | 5) => (
            span(u(p, "MipSlice"), 1, e.mips),
            span(u(p, "FirstArraySlice"), u(p, "ArraySize"), e.slices),
            plane(p, e),
        ),
        ("rtv", 6) => (0..1, 0..1, 0..1),
        ("rtv", 7) => (0..1, span(u(p, "FirstArraySlice"), u(p, "ArraySize"), e.slices), 0..1),
        // A depth-stencil view covers both planes.
        ("dsv", 1 | 3) => (span(u(p, "MipSlice"), 1, e.mips), 0..1, 0..e.planes),
        ("dsv", 2 | 4) => (
            span(u(p, "MipSlice"), 1, e.mips),
            span(u(p, "FirstArraySlice"), u(p, "ArraySize"), e.slices),
            0..e.planes,
        ),
        ("dsv", 5) => (0..1, 0..1, 0..e.planes),
        ("dsv", 6) => (0..1, span(u(p, "FirstArraySlice"), u(p, "ArraySize"), e.slices), 0..e.planes),
        _ => return all(),
    };
    let mut out = Vec::new();
    for plane in planes {
        for slice in slices.clone() {
            for mip in mips.clone() {
                out.push(e.index(mip, slice, plane));
            }
        }
    }
    out
}

fn plane(p: &Value, e: Extent) -> std::ops::Range<u32> {
    let k = u(p, "PlaneSlice").min(e.planes.saturating_sub(1));
    k..k + 1
}
