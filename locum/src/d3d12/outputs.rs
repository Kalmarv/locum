//! Which resources a draw, dispatch or UAV clear writes through UAVs, resolved as the command is recorded, for the
//! copies [`super::passes`] makes of them right after it: the D3D12 `results`.
//!
//! A program's UAV registers are read as it is made (its `PSV0` part, or a DXBC program's reflection), and a root
//! signature's places for them (a root view, or a descriptor of a table) as it is made. While a list is recorded
//! with a capture armed, its pipeline, root signatures and root arguments are followed, and at each draw or dispatch
//! every register its programs declare is looked up where the root signature puts it: the root view's buffer, or the
//! descriptor its table's slot holds then. A buffer is copied whole; a texture, the subresources its view covers.
//! An ExecuteIndirect's command signature can set root parameters per record: those come from its records, not
//! from what the list set, and what a root UAV in them names is found after the frame (see [`super::indirect`]).
use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::descriptors::Description;

/// UAV registers as (space, first, last), `u32::MAX` the last of an unbounded array.
type Registers = Arc<Vec<(u32, u32, u32)>>;

fn lock<T>(m: &'static OnceLock<Mutex<T>>) -> MutexGuard<'static, T>
where
    T: Default,
{
    m.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// By program (its content key): its UAV registers, or None where neither its `PSV0` part nor reflection says.
fn programs() -> MutexGuard<'static, HashMap<String, Option<Registers>>> {
    static PROGRAMS: OnceLock<Mutex<HashMap<String, Option<Registers>>>> = OnceLock::new();
    lock(&PROGRAMS)
}

/// A pipeline's programs: each one's stage and registers.
type Stages = Arc<Vec<(&'static str, Option<Registers>)>>;

/// By pipeline: its programs.
fn pipelines() -> MutexGuard<'static, HashMap<u64, Stages>> {
    static PIPELINES: OnceLock<Mutex<HashMap<u64, Stages>>> = OnceLock::new();
    lock(&PIPELINES)
}

/// A place a root signature gives UAV registers: a root view (parameter) or the descriptors of a table range.
#[derive(Clone, Copy)]
struct Place {
    parameter: u32,
    /// D3D12_SHADER_VISIBILITY.
    visibility: u32,
    space: u32,
    first: u32,
    /// The last register, `u32::MAX` for an unbounded range.
    last: u32,
    /// The descriptor of `first` from the table's start; None for a root view.
    offset: Option<u32>,
}

fn roots() -> MutexGuard<'static, HashMap<u64, Arc<Vec<Place>>>> {
    static ROOTS: OnceLock<Mutex<HashMap<u64, Arc<Vec<Place>>>>> = OnceLock::new();
    lock(&ROOTS)
}

/// A program's bytes as a pipeline is made: its UAV registers, read once per program.
pub fn program(key: &str, code: &[u8]) {
    let mut p = programs();
    if !p.contains_key(key) {
        p.insert(
            key.to_string(),
            locum_capture::rdef::uav_registers(code).map(Arc::new),
        );
    }
}

/// A pipeline made, from its description: its programs, by stage.
pub fn pipeline(id: u64, description: &Value) {
    let stages: Vec<(&'static str, Option<Registers>)> = {
        let p = programs();
        description["shaders"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let stage = match s["stage"].as_str()? {
                    "vs" => "vs",
                    "hs" => "hs",
                    "ds" => "ds",
                    "gs" => "gs",
                    "ps" => "ps",
                    "cs" => "cs",
                    "as" => "as",
                    "ms" => "ms",
                    _ => return None,
                };
                Some((stage, p.get(s["blob"].as_str()?).cloned().flatten()))
            })
            .collect()
    };
    pipelines().insert(id, Arc::new(stages));
}

/// A root signature made, from its decoded description: where it puts UAV registers.
pub fn root(id: u64, decoded: &Value) {
    let n = |v: &Value, key: &str| v[key].as_u64().unwrap_or(0) as u32;
    let mut places = Vec::new();
    for (i, p) in decoded["parameters"].as_array().into_iter().flatten().enumerate() {
        let visibility = n(p, "visibility");
        match p["type"].as_u64() {
            // D3D12_ROOT_PARAMETER_TYPE_UAV
            Some(4) => {
                let r = n(&p["value"], "ShaderRegister");
                places.push(Place {
                    parameter: i as u32,
                    visibility,
                    space: n(&p["value"], "RegisterSpace"),
                    first: r,
                    last: r,
                    offset: None,
                });
            }
            // D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE
            Some(0) => {
                let mut next = 0u32;
                for r in p["value"].as_array().into_iter().flatten() {
                    let count = n(r, "NumDescriptors");
                    let at = match n(r, "OffsetInDescriptorsFromTableStart") {
                        u32::MAX => next,
                        o => o,
                    };
                    next = at.saturating_add(count);
                    // D3D12_DESCRIPTOR_RANGE_TYPE_UAV
                    if n(r, "RangeType") == 1 {
                        let first = n(r, "BaseShaderRegister");
                        places.push(Place {
                            parameter: i as u32,
                            visibility,
                            space: n(r, "RegisterSpace"),
                            first,
                            last: if count == u32::MAX {
                                u32::MAX
                            } else {
                                first.saturating_add(count.max(1) - 1)
                            },
                            offset: Some(at),
                        });
                    }
                }
            }
            _ => {}
        }
    }
    roots().insert(id, Arc::new(places));
}

/// By command signature: the root parameters its records set.
fn signatures() -> MutexGuard<'static, HashMap<u64, Arc<Vec<u32>>>> {
    static SIGNATURES: OnceLock<Mutex<HashMap<u64, Arc<Vec<u32>>>>> = OnceLock::new();
    lock(&SIGNATURES)
}

/// A command signature made, from its description.
pub fn signature(id: u64, description: &Value) {
    let set = locum_capture::d3d12::indirect::root_parameters(description);
    signatures().insert(id, Arc::new(set));
}

pub fn destroyed(id: u64) {
    pipelines().remove(&id);
    roots().remove(&id);
    signatures().remove(&id);
}

/// One subresource (or a whole buffer, `sub` None) a command wrote.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Written {
    pub resource: u64,
    pub sub: Option<u32>,
}

/// What a list being recorded has bound, as its commands set it: [graphics, compute].
#[derive(Default)]
pub struct Bound {
    pso: u64,
    roots: [u64; 2],
    args: [HashMap<u32, Value>; 2],
}

/// Registers resolved for one draw beyond this many are not copied: an array a program indexes is not a list of
/// outputs.
const MOST_REGISTERS: u32 = 64;

impl Bound {
    /// Follows a state command.
    pub fn apply(&mut self, call: &str, a: &Value) {
        let id = |v: &Value| v.as_u64().unwrap_or(0);
        match call {
            "SetPipelineState" => self.pso = id(&a[0]),
            "SetGraphicsRootSignature" | "SetComputeRootSignature" => {
                let k = usize::from(call == "SetComputeRootSignature");
                // A root signature set anew drops the arguments, even the same one.
                self.roots[k] = id(&a[0]);
                self.args[k].clear();
            }
            "ClearState" => {
                *self = Bound {
                    pso: id(&a[0]),
                    ..Bound::default()
                }
            }
            // A bundle can set any of it; what it leaves is not followed.
            "ExecuteBundle" => *self = Bound::default(),
            _ => {
                let (k, kind) = if let Some(k) = call.strip_prefix("SetGraphicsRoot") {
                    (0, k)
                } else if let Some(k) = call.strip_prefix("SetComputeRoot") {
                    (1, k)
                } else {
                    return;
                };
                if matches!(kind, "DescriptorTable" | "UnorderedAccessView") {
                    self.args[k].insert(id(&a[0]) as u32, a[1].clone());
                }
            }
        }
    }

    /// The initial pipeline a Reset names.
    pub fn reset(pso: u64) -> Bound {
        Bound {
            pso,
            ..Bound::default()
        }
    }

    /// What the command `call` with arguments `a` writes through UAVs, each subresource once; and why some it may
    /// write cannot be named. Empty for commands that write none.
    pub fn written(&self, call: &str, a: &Value) -> (Vec<Written>, Vec<String>) {
        let mut out = Vec::new();
        let mut gaps = Vec::new();
        match call {
            "ClearUnorderedAccessViewUint" | "ClearUnorderedAccessViewFloat" => {
                let resource = a[2].as_u64().unwrap_or(0);
                // The view as the CPU handle's slot defines it; failing that, the GPU handle's slot, which holds the
                // same view.
                let view = match a[1]["definition"].get("description") {
                    Some(d) => described(d),
                    None => match (a[0]["heap"].as_u64(), a[0]["slot"].as_u64()) {
                        (Some(heap), Some(slot)) => super::slots::uav(heap, slot as usize)
                            .filter(|(r, _, _)| *r == resource)
                            .map(|(_, _, view)| Some(view))
                            .ok_or_else(|| "its view is not known".to_string()),
                        _ => Err("its view is not known".into()),
                    },
                };
                match covered(resource, view) {
                    Ok(subs) => out.extend(subs),
                    Err(why) => gaps.push(why),
                }
            }
            "Dispatch" | "DispatchMesh" | "DrawInstanced" | "DrawIndexedInstanced" | "ExecuteIndirect" => {
                // An indirect command's signature says whether it draws or dispatches; both root signatures are
                // followed, so either is found.
                let kinds: &[usize] = match call {
                    "Dispatch" => &[1],
                    "ExecuteIndirect" => &[0, 1],
                    _ => &[0],
                };
                let Some(stages) = pipelines().get(&self.pso).cloned() else {
                    gaps.push("its pipeline's programs are not known".into());
                    return (out, gaps);
                };
                // What an ExecuteIndirect's records set comes from them.
                let from_records = match call {
                    "ExecuteIndirect" => signatures()
                        .get(&a[0].as_u64().unwrap_or(0))
                        .cloned()
                        .unwrap_or_default(),
                    _ => Arc::default(),
                };
                for &k in kinds {
                    let compute = k == 1;
                    let Some(places) = roots().get(&self.roots[k]).cloned() else {
                        continue;
                    };
                    for (stage, registers) in stages.iter().filter(|(s, _)| (*s == "cs") == compute) {
                        let Some(registers) = registers else {
                            gaps.push(format!("its {stage} program says nothing of its UAVs"));
                            continue;
                        };
                        for &(space, first, last) in registers.iter() {
                            if last == u32::MAX || last - first >= MOST_REGISTERS {
                                gaps.push(
                                    "a UAV array of unbounded or large size: what it writes is not named"
                                        .into(),
                                );
                                continue;
                            }
                            for register in first..=last {
                                match self.resolve(&places, k, stage, space, register, &from_records) {
                                    Ok(Some(w)) => out.extend(w),
                                    Ok(None) => {}
                                    Err(why) => gaps.push(why),
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        let mut seen = std::collections::HashSet::new();
        out.retain(|w| seen.insert(w.clone()));
        (out, gaps)
    }

    /// The subresources register `u{register}` of `space` names for `stage`, by root signature `places` and the
    /// arguments of kind `k`; None where nothing is bound there (a null view), or where the root parameter is one of
    /// `from_records`, which an ExecuteIndirect's records set.
    fn resolve(
        &self,
        places: &[Place],
        k: usize,
        stage: &str,
        space: u32,
        register: u32,
        from_records: &[u32],
    ) -> Result<Option<Vec<Written>>, String> {
        // D3D12_SHADER_VISIBILITY: all, vertex, hull, domain, geometry, pixel, amplification, mesh.
        let visible = match stage {
            "vs" => 1,
            "hs" => 2,
            "ds" => 3,
            "gs" => 4,
            "ps" => 5,
            "as" => 6,
            "ms" => 7,
            _ => 0,
        };
        let place = places
            .iter()
            .find(|p| {
                (p.visibility == 0 || p.visibility == visible)
                    && p.space == space
                    && (p.first..=p.last).contains(&register)
            })
            .ok_or_else(|| format!("u{register} space {space} is in no root parameter"))?;
        if from_records.contains(&place.parameter) {
            return Ok(None);
        }
        let Some(arg) = self.args[k].get(&place.parameter) else {
            return Err(format!(
                "root parameter {} was not set in this list",
                place.parameter
            ));
        };
        match place.offset {
            None => {
                if arg.get("candidates").is_some() {
                    return Err("a root UAV's address names more than one resource".into());
                }
                let resource = arg["resource"].as_u64().unwrap_or(0);
                Ok((resource != 0).then(|| vec![Written { resource, sub: None }]))
            }
            Some(offset) => {
                let (Some(heap), Some(slot)) = (arg["heap"].as_u64(), arg["slot"].as_u64()) else {
                    return Err("a table's descriptor heap is not known".into());
                };
                let slot = slot as usize + offset as usize + (register - place.first) as usize;
                let Some((resource, counter, view)) = super::slots::uav(heap, slot) else {
                    return Ok(None);
                };
                let mut written = covered(resource, Ok(Some(view)))?;
                // An append or consume buffer's count is written too.
                if counter != 0 {
                    written.push(Written {
                        resource: counter,
                        sub: None,
                    });
                }
                Ok(Some(written))
            }
        }
    }
}

/// A UAV definition's view as recorded (the JSON of [`Description::json`]), as far as [`covered`] reads it.
fn described(v: &Value) -> Result<Option<Description>, String> {
    if v["default"] == true {
        return Ok(Some(Description::DefaultView));
    }
    let n = |key: &str| v["payload"][key].as_u64().unwrap_or(0) as u32;
    let mut d = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT(v["format"].as_i64().unwrap_or(0) as i32),
        ViewDimension: D3D12_UAV_DIMENSION(v["dimension"].as_i64().unwrap_or(0) as i32),
        ..Default::default()
    };
    match d.ViewDimension {
        D3D12_UAV_DIMENSION_BUFFER => {}
        D3D12_UAV_DIMENSION_TEXTURE1D => {
            d.Anonymous.Texture1D = D3D12_TEX1D_UAV {
                MipSlice: n("MipSlice"),
            }
        }
        D3D12_UAV_DIMENSION_TEXTURE1DARRAY => {
            d.Anonymous.Texture1DArray = D3D12_TEX1D_ARRAY_UAV {
                MipSlice: n("MipSlice"),
                FirstArraySlice: n("FirstArraySlice"),
                ArraySize: n("ArraySize"),
            }
        }
        D3D12_UAV_DIMENSION_TEXTURE2D => {
            d.Anonymous.Texture2D = D3D12_TEX2D_UAV {
                MipSlice: n("MipSlice"),
                PlaneSlice: n("PlaneSlice"),
            }
        }
        D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
            d.Anonymous.Texture2DArray = D3D12_TEX2D_ARRAY_UAV {
                MipSlice: n("MipSlice"),
                FirstArraySlice: n("FirstArraySlice"),
                ArraySize: n("ArraySize"),
                PlaneSlice: n("PlaneSlice"),
            }
        }
        D3D12_UAV_DIMENSION_TEXTURE3D => {
            d.Anonymous.Texture3D = D3D12_TEX3D_UAV {
                MipSlice: n("MipSlice"),
                FirstWSlice: n("FirstWSlice"),
                WSize: n("WSize"),
            }
        }
        other => return Err(format!("a UAV of dimension {} is not copied", other.0)),
    }
    Ok(Some(Description::Uav(d)))
}

/// The subresources of `resource` a UAV view covers: the whole of a buffer; a texture's mip in each array slice of
/// the view (a volume's whole mip).
fn covered(resource: u64, view: Result<Option<Description>, String>) -> Result<Vec<Written>, String> {
    if resource == 0 {
        return Ok(Vec::new());
    }
    let real = super::real(resource)
        .and_then(|r| r.cast::<ID3D12Resource>().ok())
        .ok_or("destroyed before it was written")?;
    let desc = unsafe { real.GetDesc() };
    if desc.Dimension == D3D12_RESOURCE_DIMENSION_BUFFER {
        return Ok(vec![Written { resource, sub: None }]);
    }
    if desc.SampleDesc.Count > 1 {
        return Err("a multisampled UAV is not copied".into());
    }
    let mips = u32::from(desc.MipLevels.max(1));
    let slices = if desc.Dimension == D3D12_RESOURCE_DIMENSION_TEXTURE3D {
        1
    } else {
        u32::from(desc.DepthOrArraySize.max(1))
    };
    let span = |first: u32, size: u32| {
        first
            ..first
                .saturating_add(if size == u32::MAX { slices } else { size })
                .min(slices)
    };
    let (mip, range, plane) = match view? {
        None | Some(Description::DefaultView) => (0, 0..slices, 0),
        Some(Description::Uav(d)) => unsafe {
            match d.ViewDimension {
                D3D12_UAV_DIMENSION_TEXTURE1D => (d.Anonymous.Texture1D.MipSlice, 0..1, 0),
                D3D12_UAV_DIMENSION_TEXTURE1DARRAY => {
                    let a = d.Anonymous.Texture1DArray;
                    (a.MipSlice, span(a.FirstArraySlice, a.ArraySize), 0)
                }
                D3D12_UAV_DIMENSION_TEXTURE2D => (
                    d.Anonymous.Texture2D.MipSlice,
                    0..1,
                    d.Anonymous.Texture2D.PlaneSlice,
                ),
                D3D12_UAV_DIMENSION_TEXTURE2DARRAY => {
                    let a = d.Anonymous.Texture2DArray;
                    (a.MipSlice, span(a.FirstArraySlice, a.ArraySize), a.PlaneSlice)
                }
                D3D12_UAV_DIMENSION_TEXTURE3D => (d.Anonymous.Texture3D.MipSlice, 0..1, 0),
                // A buffer view of a texture is not one D3D12 makes.
                other => return Err(format!("a UAV of dimension {} is not copied", other.0)),
            }
        },
        Some(_) => return Err("the slot holds no UAV".into()),
    };
    if mip >= mips {
        return Err(format!("its view names mip {mip} of {mips}"));
    }
    Ok(range
        .map(|slice| Written {
            resource,
            sub: Some(mip + slice * mips + plane * mips * slices),
        })
        .collect())
}
