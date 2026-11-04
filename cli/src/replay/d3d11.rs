//! Replay on Direct3D 11: resources, views, states and shaders made from the capture's descriptions, each event run
//! as recorded. A draw is run from the full state the capture took at it (after `ClearState`), so events need no
//! state from one another; constant buffers are made from the bytes the GPU read at the draw.

use std::collections::{BTreeSet, HashMap};
use std::ffi::CString;

use locum_capture::layout;
use locum_capture::rules::{self, Action, Rule};
use serde_json::Value;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::{Interface, PCSTR};

use crate::describe::shape;
use crate::open::Capture;

use super::Backend;
mod tiles;

/// Description structs, from the bytes the capture holds (shorter blobs leave the rest at its default).
fn from_bytes<T: Default>(bytes: &[u8]) -> T {
    let mut t = T::default();
    let n = bytes.len().min(size_of::<T>());
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), (&mut t as *mut T).cast::<u8>(), n) };
    t
}

/// Misc flags kept when a resource is made again: mip generation, cube, indirect arguments, raw and structured
/// buffers, tiled resources and tile pools. Sharing and protection flags are dropped.
const KEPT_MISC: u32 = 0x1
    | 0x4
    | 0x10
    | 0x20
    | 0x40
    | D3D11_RESOURCE_MISC_TILED.0 as u32
    | D3D11_RESOURCE_MISC_TILE_POOL.0 as u32;

enum View {
    Srv(ID3D11ShaderResourceView),
    Rtv(ID3D11RenderTargetView),
    Dsv(ID3D11DepthStencilView),
    Uav(ID3D11UnorderedAccessView),
}

pub struct D3d11 {
    device: ID3D11Device3,
    ctx: ID3D11DeviceContext1,
    resources: HashMap<i64, ID3D11Resource>,
    views: HashMap<i64, Option<View>>,
    /// Textures made in their family's typeless format because their views use more than one format of it: their
    /// own format, for views that name none.
    typed: HashMap<i64, u32>,
    states: HashMap<i64, Option<ID3D11DeviceChild>>,
    shaders: HashMap<(String, usize), Option<ID3D11DeviceChild>>,
    replaced: HashMap<String, Option<ID3D11DeviceChild>>,
    warnings: BTreeSet<String>,
    debug: Option<ID3D11InfoQueue>,
}

impl D3d11 {
    pub fn new(warp: bool, debug: bool) -> Result<D3d11, String> {
        let (mut device, mut ctx) = (None, None);
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        unsafe {
            D3D11CreateDevice(
                None,
                if warp {
                    D3D_DRIVER_TYPE_WARP
                } else {
                    D3D_DRIVER_TYPE_HARDWARE
                },
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT
                    | if debug {
                        D3D11_CREATE_DEVICE_DEBUG
                    } else {
                        D3D11_CREATE_DEVICE_FLAG(0)
                    },
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut ctx),
            )
        }
        .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
        let device: ID3D11Device3 = device
            .ok_or("no device")?
            .cast()
            .map_err(|e| format!("ID3D11Device3: {e}"))?;
        let ctx: ID3D11DeviceContext1 = ctx
            .ok_or("no context")?
            .cast()
            .map_err(|e| format!("ID3D11DeviceContext1: {e}"))?;
        let debug = if debug {
            Some(
                device
                    .cast::<ID3D11InfoQueue>()
                    .map_err(|e| format!("ID3D11InfoQueue: {e}"))?,
            )
        } else {
            None
        };
        Ok(D3d11 {
            device,
            ctx,
            resources: HashMap::new(),
            views: HashMap::new(),
            typed: HashMap::new(),
            states: HashMap::new(),
            shaders: HashMap::new(),
            replaced: HashMap::new(),
            warnings: BTreeSet::new(),
            debug,
        })
    }

    fn warn(&mut self, w: impl Into<String>) {
        self.warnings.insert(w.into());
    }

    fn resource(&self, id: i64) -> Result<&ID3D11Resource, String> {
        self.resources
            .get(&id)
            .ok_or_else(|| format!("resource #{id} was not made"))
    }

    fn buffer(&self, id: i64) -> Result<ID3D11Buffer, String> {
        self.resource(id)?
            .cast()
            .map_err(|_| format!("#{id} is not a buffer"))
    }

    fn view(&mut self, c: &Capture, id: i64) -> Result<Option<&View>, String> {
        if !self.views.contains_key(&id) {
            let row: Option<(String, Option<i64>, Option<Vec<u8>>)> = c.row(
                "SELECT kind, resource, desc FROM views WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            );
            let made = match row {
                Some((kind, Some(resource), desc)) => {
                    let r = self.resource(resource)?.clone();
                    let mut desc = desc.unwrap_or_default();
                    // Every view description starts with its format; none (0) means the resource's own.
                    if let Some(&format) = self.typed.get(&resource)
                        && desc.get(..4) == Some(&[0; 4])
                    {
                        desc[..4].copy_from_slice(&format.to_le_bytes());
                    }
                    match self.make_view(&kind, &r, &desc) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            self.warn(format!(
                                "view #{id} ({kind} of #{resource}) could not be made: {e}"
                            ));
                            None
                        }
                    }
                }
                _ => {
                    self.warn(format!("view #{id} is not described in the capture"));
                    None
                }
            };
            if let Some(View::Uav(view)) = &made {
                self.initial_counter(c, id, view);
            }
            self.views.insert(id, made);
        }
        Ok(self.views[&id].as_ref())
    }

    fn make_view(&self, kind: &str, r: &ID3D11Resource, desc: &[u8]) -> windows::core::Result<View> {
        unsafe {
            match kind {
                "srv" => {
                    let d: D3D11_SHADER_RESOURCE_VIEW_DESC = from_bytes(desc);
                    let mut v = None;
                    self.device.CreateShaderResourceView(r, Some(&d), Some(&mut v))?;
                    Ok(View::Srv(v.unwrap()))
                }
                "rtv" => {
                    let d: D3D11_RENDER_TARGET_VIEW_DESC = from_bytes(desc);
                    let mut v = None;
                    self.device.CreateRenderTargetView(r, Some(&d), Some(&mut v))?;
                    Ok(View::Rtv(v.unwrap()))
                }
                "dsv" => {
                    let d: D3D11_DEPTH_STENCIL_VIEW_DESC = from_bytes(desc);
                    let mut v = None;
                    self.device.CreateDepthStencilView(r, Some(&d), Some(&mut v))?;
                    Ok(View::Dsv(v.unwrap()))
                }
                _ => {
                    let d: D3D11_UNORDERED_ACCESS_VIEW_DESC = from_bytes(desc);
                    let mut v = None;
                    self.device.CreateUnorderedAccessView(r, Some(&d), Some(&mut v))?;
                    Ok(View::Uav(v.unwrap()))
                }
            }
        }
    }

    /// Restore a view's counter once, before any event can use it, including CopyStructureCount.
    fn initial_counter(&mut self, c: &Capture, id: i64, view: &ID3D11UnorderedAccessView) {
        let mut desc = D3D11_UNORDERED_ACCESS_VIEW_DESC::default();
        unsafe { view.GetDesc(&mut desc) };
        if desc.ViewDimension != D3D11_UAV_DIMENSION_BUFFER
            || unsafe { desc.Anonymous.Buffer.Flags }
                & (D3D11_BUFFER_UAV_FLAG_APPEND.0 | D3D11_BUFFER_UAV_FLAG_COUNTER.0) as u32
                == 0
        {
            return;
        }
        let count: Option<u32> = c.row("SELECT count FROM uav_counters WHERE view = ?1", [id], |r| {
            r.get(0)
        });
        let Some(count) = count else {
            self.warn(format!("view #{id}: initial UAV counter was not captured"));
            return;
        };
        if count == u32::MAX {
            self.warn(format!(
                "view #{id}: initial UAV counter UINT_MAX cannot be set by a binding"
            ));
            return;
        }
        // Every draw rebuilds its bindings; this temporary bind also initializes views used only by copies.
        unsafe {
            self.ctx
                .CSSetUnorderedAccessViews(0, 1, Some(&Some(view.clone())), Some(&count));
            self.ctx.CSSetUnorderedAccessViews(0, 1, Some(&None), None);
        }
    }

    fn srv(&mut self, c: &Capture, id: Option<i64>) -> Result<Option<ID3D11ShaderResourceView>, String> {
        Ok(match id {
            Some(id) => match self.view(c, id)? {
                Some(View::Srv(v)) => Some(v.clone()),
                _ => None,
            },
            None => None,
        })
    }

    fn rtv(&mut self, c: &Capture, id: i64) -> Result<Option<ID3D11RenderTargetView>, String> {
        Ok(match self.view(c, id)? {
            Some(View::Rtv(v)) => Some(v.clone()),
            _ => None,
        })
    }

    fn dsv(&mut self, c: &Capture, id: i64) -> Result<Option<ID3D11DepthStencilView>, String> {
        Ok(match self.view(c, id)? {
            Some(View::Dsv(v)) => Some(v.clone()),
            _ => None,
        })
    }

    fn uav(&mut self, c: &Capture, id: i64) -> Result<Option<ID3D11UnorderedAccessView>, String> {
        Ok(match self.view(c, id)? {
            Some(View::Uav(v)) => Some(v.clone()),
            _ => None,
        })
    }

    fn state(&mut self, c: &Capture, id: Option<i64>) -> Option<ID3D11DeviceChild> {
        let id = id?;
        if !self.states.contains_key(&id) {
            type StateRow = (String, Option<Vec<u8>>, Option<String>, Option<String>);
            let row: Option<StateRow> = c.row(
                "SELECT kind, desc, json, signature FROM states WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            );
            let made = row.and_then(|(kind, desc, json, signature)| {
                let desc = desc.unwrap_or_default();
                match self.make_state(c, &kind, &desc, json.as_deref(), signature.as_deref()) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        self.warn(format!("{kind} state #{id} could not be made: {e}"));
                        None
                    }
                }
            });
            self.states.insert(id, made);
        }
        self.states[&id].clone()
    }

    fn make_state(
        &self,
        c: &Capture,
        kind: &str,
        desc: &[u8],
        json: Option<&str>,
        signature: Option<&str>,
    ) -> Result<ID3D11DeviceChild, String> {
        let child = |r: windows::core::Result<()>, made: Option<ID3D11DeviceChild>| {
            r.map_err(|e| e.to_string())?;
            made.ok_or_else(|| "nothing made".to_string())
        };
        unsafe {
            match kind {
                "rasterizer" if desc.len() >= size_of::<D3D11_RASTERIZER_DESC2>() => {
                    let d: D3D11_RASTERIZER_DESC2 = from_bytes(desc);
                    let mut s = None;
                    let r = self.device.CreateRasterizerState2(&d, Some(&mut s));
                    child(r, s.and_then(|s| s.cast().ok()))
                }
                "rasterizer" => {
                    let d: D3D11_RASTERIZER_DESC = from_bytes(desc);
                    let mut s = None;
                    let r = self.device.CreateRasterizerState(&d, Some(&mut s));
                    child(r, s.and_then(|s| s.cast().ok()))
                }
                "blend" if desc.len() >= size_of::<D3D11_BLEND_DESC1>() => {
                    let d: D3D11_BLEND_DESC1 = from_bytes(desc);
                    let mut s = None;
                    let r = self.device.CreateBlendState1(&d, Some(&mut s));
                    child(r, s.and_then(|s| s.cast().ok()))
                }
                "blend" => {
                    let d: D3D11_BLEND_DESC = from_bytes(desc);
                    let mut s = None;
                    let r = self.device.CreateBlendState(&d, Some(&mut s));
                    child(r, s.and_then(|s| s.cast().ok()))
                }
                "depth_stencil" => {
                    let d: D3D11_DEPTH_STENCIL_DESC = from_bytes(desc);
                    let mut s = None;
                    let r = self.device.CreateDepthStencilState(&d, Some(&mut s));
                    child(r, s.and_then(|s| s.cast().ok()))
                }
                "sampler" => {
                    let d: D3D11_SAMPLER_DESC = from_bytes(desc);
                    let mut s = None;
                    let r = self.device.CreateSamplerState(&d, Some(&mut s));
                    child(r, s.and_then(|s| s.cast().ok()))
                }
                "input_layout" => {
                    let elements: Vec<Value> = json
                        .and_then(|j| serde_json::from_str(j).ok())
                        .ok_or("no elements recorded")?;
                    let code = c.blob(signature.ok_or("no signature recorded")?)?;
                    let names: Vec<CString> = elements
                        .iter()
                        .map(|e| CString::new(e["semantic"].as_str().unwrap_or("")).unwrap_or_default())
                        .collect();
                    let descs: Vec<D3D11_INPUT_ELEMENT_DESC> = elements
                        .iter()
                        .zip(&names)
                        .map(|(e, name)| D3D11_INPUT_ELEMENT_DESC {
                            SemanticName: PCSTR(name.as_ptr().cast()),
                            SemanticIndex: e["index"].as_u64().unwrap_or(0) as u32,
                            Format: DXGI_FORMAT(e["format"].as_i64().unwrap_or(0) as i32),
                            InputSlot: e["slot"].as_u64().unwrap_or(0) as u32,
                            AlignedByteOffset: e["offset"].as_u64().unwrap_or(0) as u32,
                            InputSlotClass: if e["per_instance"] == true {
                                D3D11_INPUT_PER_INSTANCE_DATA
                            } else {
                                D3D11_INPUT_PER_VERTEX_DATA
                            },
                            InstanceDataStepRate: e["step"].as_u64().unwrap_or(0) as u32,
                        })
                        .collect();
                    let mut s = None;
                    let r = self.device.CreateInputLayout(&descs, &code, Some(&mut s));
                    child(r, s.and_then(|s| s.cast().ok()))
                }
                other => Err(format!("unknown state kind {other}")),
            }
        }
    }

    fn make_shader(&self, stage: usize, code: &[u8]) -> Result<ID3D11DeviceChild, String> {
        unsafe {
            let made: windows::core::Result<ID3D11DeviceChild> = match stage {
                0 => {
                    let mut s = None;
                    self.device
                        .CreateVertexShader(code, None, Some(&mut s))
                        .and_then(|()| s.unwrap().cast())
                }
                1 => {
                    let mut s = None;
                    self.device
                        .CreateHullShader(code, None, Some(&mut s))
                        .and_then(|()| s.unwrap().cast())
                }
                2 => {
                    let mut s = None;
                    self.device
                        .CreateDomainShader(code, None, Some(&mut s))
                        .and_then(|()| s.unwrap().cast())
                }
                3 => {
                    let mut s = None;
                    self.device
                        .CreateGeometryShader(code, None, Some(&mut s))
                        .and_then(|()| s.unwrap().cast())
                }
                4 => {
                    let mut s = None;
                    self.device
                        .CreatePixelShader(code, None, Some(&mut s))
                        .and_then(|()| s.unwrap().cast())
                }
                _ => {
                    let mut s = None;
                    self.device
                        .CreateComputeShader(code, None, Some(&mut s))
                        .and_then(|()| s.unwrap().cast())
                }
            };
            made.map_err(|e| e.to_string())
        }
    }

    fn shader(&mut self, c: &Capture, hash: &str, stage: usize) -> Option<ID3D11DeviceChild> {
        let k = (hash.to_string(), stage);
        if !self.shaders.contains_key(&k) {
            let key: Option<String> = c
                .row("SELECT blob FROM shaders WHERE hash = ?1", [hash], |r| r.get(0))
                .flatten();
            let made = match key.map(|k| c.blob(&k)) {
                Some(Ok(code)) => match self.make_shader(stage, &code) {
                    Ok(s) => Some(s),
                    Err(e) => {
                        self.warn(format!("{} {hash} could not be made: {e}", rules::STAGES[stage]));
                        None
                    }
                },
                _ => {
                    self.warn(format!(
                        "{} {hash}: no bytecode in the capture",
                        rules::STAGES[stage]
                    ));
                    None
                }
            };
            self.shaders.insert(k.clone(), made);
        }
        self.shaders[&k].clone()
    }

    /// The shader a rule puts in place.
    fn replacement(&mut self, rule: &Rule) -> Option<ID3D11DeviceChild> {
        if !self.replaced.contains_key(&rule.text) {
            let code = match &rule.action {
                Action::Solid(colour) => crate::shaders::compile(&rules::solid_source(colour), c"ps_5_0"),
                Action::Shader(code) => Ok(code.clone()),
                _ => Err("not a shader rule".into()),
            };
            let made = match code.and_then(|code| self.make_shader(rule.stage, &code)) {
                Ok(s) => Some(s),
                Err(e) => {
                    self.warn(format!("rule {:?}: {e}", rule.text));
                    None
                }
            };
            self.replaced.insert(rule.text.clone(), made);
        }
        self.replaced[&rule.text].clone()
    }

    fn constant_buffer(&self, bytes: &[u8]) -> Result<ID3D11Buffer, String> {
        let size = bytes.len().max(16).div_ceil(16) * 16;
        let mut data = bytes.to_vec();
        data.resize(size, 0);
        let desc = D3D11_BUFFER_DESC {
            ByteWidth: size as u32,
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
            ..Default::default()
        };
        let init = D3D11_SUBRESOURCE_DATA {
            pSysMem: data.as_ptr().cast(),
            ..Default::default()
        };
        let mut b = None;
        unsafe { self.device.CreateBuffer(&desc, Some(&init), Some(&mut b)) }
            .map_err(|e| format!("constant buffer of {size} bytes: {e}"))?;
        b.ok_or_else(|| "no constant buffer".into())
    }

    /// Runs a draw or dispatch from the state the capture took at it.
    fn draw(
        &mut self,
        c: &Capture,
        seq: i64,
        call: &str,
        args: &[i64],
        rules: &[Rule],
    ) -> Result<(), String> {
        let n: i64 = c
            .row("SELECT n FROM draws WHERE event = ?1", [seq], |r| r.get(0))
            .ok_or("no draw row")?;
        type Row = (
            [Option<String>; 6],
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<String>,
            Option<i64>,
            Option<i64>,
        );
        let row: Row = c
            .row(
                "SELECT vs, hs, ds, gs, ps, cs, topology, layout, rasterizer, blend, depth_stencil, blend_factor, \
                 sample_mask, stencil_ref FROM draws WHERE n = ?1",
                [n],
                |r| {
                    Ok((
                        [r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?],
                        r.get(6)?,
                        r.get(7)?,
                        r.get(8)?,
                        r.get(9)?,
                        r.get(10)?,
                        r.get(11)?,
                        r.get(12)?,
                        r.get(13)?,
                    ))
                },
            )
            .ok_or("no draw row")?;
        let (hashes, topology, layout_id, raster, blend, depth, factor, mask, stencil_ref) = row;
        let compute = call.starts_with("Dispatch");
        let bound = |stage: usize| -> Option<u128> {
            hashes[stage]
                .as_deref()
                .and_then(|h| u128::from_str_radix(h, 16).ok())
        };
        let applying: Vec<&Rule> = rules
            .iter()
            .filter(|r| r.applies(compute) && r.holds(bound))
            .collect();
        if applying.iter().any(|r| r.action == Action::Skip) {
            return Ok(());
        }

        let ctx = self.ctx.clone();
        unsafe { ctx.ClearState() };

        // Shaders, with replacements.
        let mut shaders: [Option<ID3D11DeviceChild>; 6] = Default::default();
        for (stage, h) in hashes.iter().enumerate() {
            if let Some(h) = h {
                shaders[stage] = self.shader(c, h, stage);
            }
        }
        for r in &applying {
            if matches!(r.action, Action::Solid(_) | Action::Shader(_))
                && let Some(s) = self.replacement(r)
            {
                shaders[r.stage] = Some(s);
            }
        }

        type BindRow = (
            String,
            String,
            u32,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<String>,
        );
        let binds: Vec<BindRow> = c.rows(
            "SELECT stage, kind, slot, view, resource, state, offset, stride, format, content FROM bindings \
             WHERE draw = ?1",
            [n],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get::<_, i64>(2)? as u32,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                    r.get(7)?,
                    r.get(8)?,
                    r.get(9)?,
                ))
            },
        );

        let mut cbs: [[Option<ID3D11Buffer>; 14]; 6] = Default::default();
        let mut srvs: [Vec<Option<ID3D11ShaderResourceView>>; 6] = Default::default();
        let mut samplers: [[Option<ID3D11SamplerState>; 16]; 6] = Default::default();
        let mut rtvs: [Option<ID3D11RenderTargetView>; 8] = Default::default();
        let mut dsv: Option<ID3D11DepthStencilView> = None;
        let mut om_uavs: Vec<(u32, Option<ID3D11UnorderedAccessView>)> = Vec::new();
        let mut cs_uavs: Vec<(u32, Option<ID3D11UnorderedAccessView>)> = Vec::new();
        let mut vbs: Vec<(u32, ID3D11Buffer, u32, u32)> = Vec::new();
        let mut ib: Option<(ID3D11Buffer, u32, u32)> = None;
        let mut so: Vec<(u32, ID3D11Buffer, u32)> = Vec::new();
        for (stage_name, kind, slot, view, resource, state, offset, stride, fmt, content) in binds {
            let stage = rules::stage_named(&stage_name);
            match (kind.as_str(), stage) {
                ("cb", Some(st)) if (slot as usize) < 14 => {
                    let mut bytes: Vec<u8> = match (content, resource) {
                        (Some(key), _) => c
                            .row("SELECT bytes FROM cb_data WHERE hash = ?1", [&key], |r| r.get(0))
                            .ok_or("constant buffer contents missing")?,
                        // Not captured (a command list executed more than once kept only its last execution's):
                        // the replayed buffer holds them, its contents being the game's at this point.
                        (None, Some(r)) => {
                            let size: Option<i64> = c
                                .row(
                                    "SELECT size FROM bindings WHERE draw = ?1 AND stage = ?2 AND kind = 'cb' \
                                     AND slot = ?3",
                                    rusqlite::params![n, stage_name, slot],
                                    |r| r.get(0),
                                )
                                .flatten();
                            if size == Some(0) {
                                continue;
                            }
                            let all = self.read(r)?;
                            let start = (offset.unwrap_or(0) as usize).min(all.len());
                            let end = match size {
                                Some(s) if s >= 0 => (start + s as usize).min(all.len()),
                                _ => all.len().min(start + 65536),
                            };
                            all[start..end].to_vec()
                        }
                        (None, None) => {
                            self.warn(format!(
                                "{stage_name} b{slot}: contents not captured; left unbound"
                            ));
                            continue;
                        }
                    };
                    // An empty window reads zeros at every register, like an unbound buffer.
                    if bytes.is_empty() {
                        continue;
                    }
                    for r in &applying {
                        if let Action::Constant { slot: s, index, bits } = r.action
                            && r.stage == st
                            && s == slot
                        {
                            let at = index as usize * 16;
                            if at + 16 <= bytes.len() {
                                for (k, b) in bits.iter().enumerate() {
                                    bytes[at + k * 4..at + k * 4 + 4].copy_from_slice(&b.to_le_bytes());
                                }
                            } else {
                                self.warn(format!("rule {:?}: c{index} is past the bound range", r.text));
                            }
                        }
                    }
                    cbs[st][slot as usize] = Some(self.constant_buffer(&bytes)?);
                }
                ("srv", Some(st)) => {
                    let v = self.srv(c, view)?;
                    let list = &mut srvs[st];
                    if list.len() <= slot as usize {
                        list.resize(slot as usize + 1, None);
                    }
                    list[slot as usize] = v;
                }
                ("sampler", Some(st)) if (slot as usize) < 16 => {
                    samplers[st][slot as usize] = self.state(c, state).and_then(|s| s.cast().ok());
                }
                ("uav", Some(5)) => cs_uavs.push((slot, view.map(|v| self.uav(c, v)).transpose()?.flatten())),
                ("uav", _) => om_uavs.push((slot, view.map(|v| self.uav(c, v)).transpose()?.flatten())),
                ("rtv", _) if slot < 8 => {
                    if let Some(v) = view {
                        rtvs[slot as usize] = self.rtv(c, v)?;
                    }
                }
                ("dsv", _) => {
                    if let Some(v) = view {
                        dsv = self.dsv(c, v)?;
                    }
                }
                ("vb", _) => {
                    if let Some(r) = resource {
                        vbs.push((
                            slot,
                            self.buffer(r)?,
                            stride.unwrap_or(0) as u32,
                            offset.unwrap_or(0) as u32,
                        ));
                    }
                }
                ("ib", _) => {
                    if let Some(r) = resource {
                        ib = Some((
                            self.buffer(r)?,
                            fmt.unwrap_or(0) as u32,
                            offset.unwrap_or(0) as u32,
                        ));
                    }
                }
                ("so", _) => {
                    if let Some(r) = resource {
                        so.push((slot, self.buffer(r)?, offset.unwrap_or(0) as u32));
                    }
                }
                _ => {}
            }
        }

        unsafe {
            // Input assembly.
            if let Some(t) = topology {
                ctx.IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY(t as i32));
            }
            if let Some(l) = self.state(c, layout_id) {
                ctx.IASetInputLayout(&l.cast::<ID3D11InputLayout>().map_err(|e| e.to_string())?);
            }
            for (slot, b, stride, offset) in &vbs {
                ctx.IASetVertexBuffers(*slot, 1, Some(&Some(b.clone())), Some(stride), Some(offset));
            }
            if let Some((b, f, offset)) = &ib {
                ctx.IASetIndexBuffer(b, DXGI_FORMAT(*f as i32), *offset);
            }
            // Shader stages.
            for stage in 0..6 {
                set_stage(
                    &ctx,
                    stage,
                    shaders[stage].as_ref(),
                    &cbs[stage],
                    &srvs[stage],
                    &samplers[stage],
                );
            }
            for (slot, uav) in &cs_uavs {
                ctx.CSSetUnorderedAccessViews(*slot, 1, Some(uav), None);
            }
            if !so.is_empty() {
                let count = so.iter().map(|s| s.0 + 1).max().unwrap_or(0) as usize;
                let mut buffers: Vec<Option<ID3D11Buffer>> = vec![None; count];
                let mut offsets = vec![0u32; count];
                for (slot, b, offset) in &so {
                    buffers[*slot as usize] = Some(b.clone());
                    offsets[*slot as usize] = *offset;
                }
                ctx.SOSetTargets(count as u32, Some(buffers.as_ptr()), Some(offsets.as_ptr()));
            }
            // Rasterizer.
            if let Some(s) = self.state(c, raster) {
                ctx.RSSetState(&s.cast::<ID3D11RasterizerState>().map_err(|e| e.to_string())?);
            }
            let viewports: Vec<D3D11_VIEWPORT> = c.rows(
                "SELECT x, y, width, height, min_depth, max_depth FROM viewports WHERE draw = ?1 ORDER BY i",
                [n],
                |r| {
                    Ok(D3D11_VIEWPORT {
                        TopLeftX: r.get::<_, f64>(0)? as f32,
                        TopLeftY: r.get::<_, f64>(1)? as f32,
                        Width: r.get::<_, f64>(2)? as f32,
                        Height: r.get::<_, f64>(3)? as f32,
                        MinDepth: r.get::<_, f64>(4)? as f32,
                        MaxDepth: r.get::<_, f64>(5)? as f32,
                    })
                },
            );
            if !viewports.is_empty() {
                ctx.RSSetViewports(Some(&viewports));
            }
            let scissors: Vec<RECT> = c.rows(
                "SELECT left, top, right, bottom FROM scissors WHERE draw = ?1 ORDER BY i",
                [n],
                |r| {
                    Ok(RECT {
                        left: r.get::<_, i64>(0)? as i32,
                        top: r.get::<_, i64>(1)? as i32,
                        right: r.get::<_, i64>(2)? as i32,
                        bottom: r.get::<_, i64>(3)? as i32,
                    })
                },
            );
            if !scissors.is_empty() {
                ctx.RSSetScissorRects(Some(&scissors));
            }
            // Output merger.
            if !compute {
                let factor: [f32; 4] = factor
                    .and_then(|f| serde_json::from_str(&f).ok())
                    .unwrap_or([1.0; 4]);
                let b = self
                    .state(c, blend)
                    .and_then(|s| s.cast::<ID3D11BlendState>().ok());
                ctx.OMSetBlendState(b.as_ref(), Some(&factor), mask.unwrap_or(-1) as u32);
                let d = self
                    .state(c, depth)
                    .and_then(|s| s.cast::<ID3D11DepthStencilState>().ok());
                ctx.OMSetDepthStencilState(d.as_ref(), stencil_ref.unwrap_or(0) as u32);
                let count = rtvs.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
                if om_uavs.is_empty() {
                    ctx.OMSetRenderTargets(Some(&rtvs[..count]), dsv.as_ref());
                } else {
                    let start = om_uavs.iter().map(|u| u.0).min().unwrap_or(0);
                    let end = om_uavs.iter().map(|u| u.0 + 1).max().unwrap_or(0);
                    let mut uavs: Vec<Option<ID3D11UnorderedAccessView>> = vec![None; (end - start) as usize];
                    for (slot, u) in &om_uavs {
                        uavs[(slot - start) as usize] = u.clone();
                    }
                    let keep = vec![u32::MAX; uavs.len()];
                    ctx.OMSetRenderTargetsAndUnorderedAccessViews(
                        Some(&rtvs[..count]),
                        dsv.as_ref(),
                        start,
                        uavs.len() as u32,
                        Some(uavs.as_ptr()),
                        Some(keep.as_ptr()),
                    );
                }
            }
            let a = |i: usize| args.get(i).copied().unwrap_or(0);
            match call {
                "Draw" => ctx.Draw(a(0) as u32, a(1) as u32),
                "DrawIndexed" => ctx.DrawIndexed(a(0) as u32, a(1) as u32, a(2) as i32),
                "DrawInstanced" => ctx.DrawInstanced(a(0) as u32, a(1) as u32, a(2) as u32, a(3) as u32),
                "DrawIndexedInstanced" => {
                    ctx.DrawIndexedInstanced(a(0) as u32, a(1) as u32, a(2) as u32, a(3) as i32, a(4) as u32)
                }
                "DrawAuto" => ctx.DrawAuto(),
                "DrawInstancedIndirect" => ctx.DrawInstancedIndirect(&self.buffer(a(0))?, a(1) as u32),
                "DrawIndexedInstancedIndirect" => {
                    ctx.DrawIndexedInstancedIndirect(&self.buffer(a(0))?, a(1) as u32)
                }
                "Dispatch" => ctx.Dispatch(a(0) as u32, a(1) as u32, a(2) as u32),
                "DispatchIndirect" => ctx.DispatchIndirect(&self.buffer(a(0))?, a(1) as u32),
                other => return Err(format!("unknown call {other}")),
            }
        }
        Ok(())
    }
}

/// Binds one stage's shader, constant buffers, shader resources and samplers.
unsafe fn set_stage(
    ctx: &ID3D11DeviceContext1,
    stage: usize,
    shader: Option<&ID3D11DeviceChild>,
    cbs: &[Option<ID3D11Buffer>; 14],
    srvs: &[Option<ID3D11ShaderResourceView>],
    samplers: &[Option<ID3D11SamplerState>; 16],
) {
    let cb_count = cbs.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
    let sampler_count = samplers.iter().rposition(Option::is_some).map_or(0, |i| i + 1);
    let cbs = (cb_count > 0).then(|| &cbs[..cb_count]);
    let srvs = (!srvs.is_empty()).then_some(srvs);
    let samplers = (sampler_count > 0).then(|| &samplers[..sampler_count]);
    unsafe {
        match stage {
            0 => {
                ctx.VSSetShader(shader.and_then(|s| s.cast().ok()).as_ref(), None);
                ctx.VSSetConstantBuffers(0, cbs);
                ctx.VSSetShaderResources(0, srvs);
                ctx.VSSetSamplers(0, samplers);
            }
            1 => {
                ctx.HSSetShader(shader.and_then(|s| s.cast().ok()).as_ref(), None);
                ctx.HSSetConstantBuffers(0, cbs);
                ctx.HSSetShaderResources(0, srvs);
                ctx.HSSetSamplers(0, samplers);
            }
            2 => {
                ctx.DSSetShader(shader.and_then(|s| s.cast().ok()).as_ref(), None);
                ctx.DSSetConstantBuffers(0, cbs);
                ctx.DSSetShaderResources(0, srvs);
                ctx.DSSetSamplers(0, samplers);
            }
            3 => {
                ctx.GSSetShader(shader.and_then(|s| s.cast().ok()).as_ref(), None);
                ctx.GSSetConstantBuffers(0, cbs);
                ctx.GSSetShaderResources(0, srvs);
                ctx.GSSetSamplers(0, samplers);
            }
            4 => {
                ctx.PSSetShader(shader.and_then(|s| s.cast().ok()).as_ref(), None);
                ctx.PSSetConstantBuffers(0, cbs);
                ctx.PSSetShaderResources(0, srvs);
                ctx.PSSetSamplers(0, samplers);
            }
            _ => {
                ctx.CSSetShader(shader.and_then(|s| s.cast().ok()).as_ref(), None);
                ctx.CSSetConstantBuffers(0, cbs);
                ctx.CSSetShaderResources(0, srvs);
                ctx.CSSetSamplers(0, samplers);
            }
        }
    }
}

fn json_u32(v: &Value, key: &str) -> u32 {
    v[key].as_u64().unwrap_or(0) as u32
}

fn json_box(v: &Value) -> Option<D3D11_BOX> {
    let b = v.as_array()?;
    let n = |i: usize| b.get(i).and_then(Value::as_u64).unwrap_or(0) as u32;
    Some(D3D11_BOX {
        left: n(0),
        top: n(1),
        front: n(2),
        right: n(3),
        bottom: n(4),
        back: n(5),
    })
}

impl Backend for D3d11 {
    fn create(&mut self, c: &Capture, id: i64, contents: Option<&[u8]>) -> Result<(), String> {
        let s = shape(c, id).ok_or("not described in the capture")?;
        let desc: Vec<u8> = c
            .row("SELECT desc FROM resources WHERE id = ?1", [id], |r| {
                r.get::<_, Option<Vec<u8>>>(0)
            })
            .flatten()
            .ok_or("its description was not recorded")?;
        let misc: u32 = c
            .row("SELECT misc FROM resources WHERE id=?1", [id], |r| r.get(0))
            .unwrap_or(0);
        if misc & (D3D11_RESOURCE_MISC_TILED.0 as u32 | D3D11_RESOURCE_MISC_TILE_POOL.0 as u32) != 0 {
            let native: bool = c
                .row(
                    "SELECT content_source = 'tile-events' FROM resources WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if !native {
                return Err("tiled resource has no recorded native mapping/pool state".into());
            }
        }
        let subs = s.subs();
        let contents = contents.filter(|b| b.len() as u64 == layout::total(&subs));
        let msaa = s.samples > 1;
        if msaa && contents.is_some() {
            self.warn("multisampled textures start empty (their contents were read back resolved)");
        }
        let init: Vec<D3D11_SUBRESOURCE_DATA> = match contents {
            Some(bytes) if !msaa => subs
                .iter()
                .map(|x| D3D11_SUBRESOURCE_DATA {
                    pSysMem: bytes[x.offset as usize..].as_ptr().cast(),
                    SysMemPitch: x.row_bytes,
                    SysMemSlicePitch: x.row_bytes * x.rows,
                })
                .collect(),
            _ => Vec::new(),
        };
        let init_ptr = (!init.is_empty()).then_some(init.as_ptr());
        // A texture viewed as more than one format of its family (a swap chain's back buffer drawn to through an sRGB
        // view: DXGI allows that of its own buffers) is made in the family's typeless format, which takes them all.
        let mut format = s.format;
        if s.kind != "buffer" {
            let viewed: Vec<u32> = c.rows(
                "SELECT DISTINCT format FROM views WHERE resource = ?1 AND format IS NOT NULL AND format <> 0",
                [id],
                |r| r.get(0),
            );
            if viewed.iter().any(|&f| f != s.format) && locum_capture::format::typeless(s.format) != s.format
            {
                format = locum_capture::format::typeless(s.format);
                self.typed.insert(id, s.format);
            }
        }
        let device = self.device.clone();
        let make = |with: Option<*const D3D11_SUBRESOURCE_DATA>| -> windows::core::Result<ID3D11Resource> {
            unsafe {
                match s.kind.as_str() {
                    "buffer" => {
                        let mut d: D3D11_BUFFER_DESC = from_bytes(&desc);
                        d.Usage = D3D11_USAGE_DEFAULT;
                        d.CPUAccessFlags = 0;
                        d.MiscFlags &= KEPT_MISC;
                        let mut b = None;
                        device.CreateBuffer(&d, with, Some(&mut b))?;
                        b.unwrap().cast()
                    }
                    "texture1d" => {
                        let mut d: D3D11_TEXTURE1D_DESC = from_bytes(&desc);
                        d.Format = DXGI_FORMAT(format as i32);
                        d.MipLevels = s.mips;
                        d.Usage = D3D11_USAGE_DEFAULT;
                        d.CPUAccessFlags = 0;
                        d.MiscFlags &= KEPT_MISC;
                        let mut t = None;
                        device.CreateTexture1D(&d, with, Some(&mut t))?;
                        t.unwrap().cast()
                    }
                    "texture3d" => {
                        let mut d: D3D11_TEXTURE3D_DESC = from_bytes(&desc);
                        d.Format = DXGI_FORMAT(format as i32);
                        d.MipLevels = s.mips;
                        d.Usage = D3D11_USAGE_DEFAULT;
                        d.CPUAccessFlags = 0;
                        d.MiscFlags &= KEPT_MISC;
                        let mut t = None;
                        device.CreateTexture3D(&d, with, Some(&mut t))?;
                        t.unwrap().cast()
                    }
                    _ => {
                        let mut d: D3D11_TEXTURE2D_DESC = from_bytes(&desc);
                        d.Format = DXGI_FORMAT(format as i32);
                        d.MipLevels = s.mips;
                        d.Usage = D3D11_USAGE_DEFAULT;
                        d.CPUAccessFlags = 0;
                        d.MiscFlags &= KEPT_MISC;
                        let mut t = None;
                        device.CreateTexture2D(&d, with, Some(&mut t))?;
                        t.unwrap().cast()
                    }
                }
            }
        };
        let made = match make(init_ptr) {
            Ok(r) => r,
            Err(e) if init_ptr.is_some() => {
                let r = make(None).map_err(|e| e.to_string())?;
                self.warn(format!(
                    "#{id} could not be made with its contents ({e}); it starts empty"
                ));
                r
            }
            Err(e) => return Err(e.to_string()),
        };
        self.resources.insert(id, made);
        Ok(())
    }

    fn run(&mut self, c: &Capture, seq: i64, rules: &[Rule]) -> Result<(), String> {
        let (kind, call, args, data): (String, String, Option<String>, Option<String>) = c
            .row(
                "SELECT kind, call, args, data FROM events WHERE seq = ?1",
                [seq],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .ok_or("no such event")?;
        let a: Value = args
            .as_deref()
            .and_then(|a| serde_json::from_str(a).ok())
            .unwrap_or_default();
        let ctx = self.ctx.clone();
        if kind.starts_with("tile_") {
            return tiles::run(self, c, &kind, &a, data.as_deref());
        }
        match kind.as_str() {
            "draw" | "dispatch" => {
                let list: Vec<i64> = a
                    .as_array()
                    .map(|v| v.iter().map(|x| x.as_i64().unwrap_or(0)).collect())
                    .unwrap_or_default();
                self.draw(c, seq, &call, &list, rules)?;
            }
            "update" => {
                let Some(key) = data else {
                    self.warn("an update's bytes were not recorded (its resource is not numbered)");
                    return Ok(());
                };
                let bytes = c.blob(&key)?;
                let dst = json_u32(&a, "dst") as i64;
                let sub = json_u32(&a, "sub");
                let s = shape(c, dst).ok_or("update of an undescribed resource")?;
                let whole = s
                    .subs()
                    .into_iter()
                    .find(|x| x.index == sub)
                    .ok_or("no such subresource")?;
                let region = json_box(&a["box"]);
                let (row, depth) = match (&region, s.kind.as_str()) {
                    (_, "buffer") => (0, 0),
                    (Some(b), _) => {
                        let l = locum_capture::format::layout(s.format).ok_or("format without layout")?;
                        let row = l.row_bytes(b.right - b.left);
                        (row, row * l.rows(b.bottom - b.top))
                    }
                    (None, _) => (whole.row_bytes, whole.row_bytes * whole.rows),
                };
                let r = self.resource(dst)?.clone();
                unsafe {
                    ctx.UpdateSubresource(
                        &r,
                        sub,
                        region.as_ref().map(|b| b as *const _),
                        bytes.as_ptr().cast(),
                        row,
                        depth,
                    )
                };
            }
            "map" => {
                let Some(key) = data else { return Ok(()) };
                let bytes = c.blob(&key)?;
                let id = json_u32(&a, "resource") as i64;
                let sub = json_u32(&a, "sub");
                let r = self.resource(id)?.clone();
                if a["whole"] == true {
                    let s = shape(c, id).ok_or("map of an undescribed resource")?;
                    let x = s
                        .subs()
                        .into_iter()
                        .find(|x| x.index == sub)
                        .ok_or("no such subresource")?;
                    unsafe {
                        ctx.UpdateSubresource(
                            &r,
                            sub,
                            None,
                            bytes.as_ptr().cast(),
                            x.row_bytes,
                            x.row_bytes * x.rows,
                        )
                    };
                } else if a["type"] == D3D11_MAP_WRITE_DISCARD.0 {
                    // What the GPU saw: the pattern the capture's shadow started as, under the bytes the game wrote.
                    let s = shape(c, id).ok_or("map of an undescribed resource")?;
                    let size = s
                        .subs()
                        .into_iter()
                        .find(|x| x.index == sub)
                        .ok_or("no such subresource")?
                        .size as usize;
                    let ranges: Vec<(usize, usize)> = a["ranges"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|r| {
                            (
                                r[0].as_u64().unwrap_or(0) as usize,
                                r[1].as_u64().unwrap_or(0) as usize,
                            )
                        })
                        .collect();
                    let whole = locum_capture::maps::discarded(size, &ranges, &bytes);
                    unsafe { ctx.UpdateSubresource(&r, sub, None, whole.as_ptr().cast(), 0, 0) };
                } else {
                    let mut at = 0usize;
                    for range in a["ranges"].as_array().into_iter().flatten() {
                        let (offset, len) = (
                            range[0].as_u64().unwrap_or(0) as u32,
                            range[1].as_u64().unwrap_or(0) as usize,
                        );
                        let b = D3D11_BOX {
                            left: offset,
                            top: 0,
                            front: 0,
                            right: offset + len as u32,
                            bottom: 1,
                            back: 1,
                        };
                        if at + len <= bytes.len() {
                            unsafe {
                                ctx.UpdateSubresource(&r, 0, Some(&b), bytes[at..].as_ptr().cast(), 0, 0)
                            };
                        }
                        at += len;
                    }
                }
            }
            "copy" => {
                let (d, s) = (
                    self.resource(json_u32(&a, "dst") as i64)?.clone(),
                    self.resource(json_u32(&a, "src") as i64)?.clone(),
                );
                unsafe { ctx.CopyResource(&d, &s) };
            }
            "copy_region" => {
                let (d, s) = (
                    self.resource(json_u32(&a, "dst") as i64)?.clone(),
                    self.resource(json_u32(&a, "src") as i64)?.clone(),
                );
                let at = &a["at"];
                let n = |i: usize| at.get(i).and_then(Value::as_u64).unwrap_or(0) as u32;
                let region = json_box(&a["box"]);
                unsafe {
                    ctx.CopySubresourceRegion(
                        &d,
                        json_u32(&a, "dst_sub"),
                        n(0),
                        n(1),
                        n(2),
                        &s,
                        json_u32(&a, "src_sub"),
                        region.as_ref().map(|b| b as *const _),
                    )
                };
            }
            "clear" => {
                let view = json_u32(&a, "view") as i64;
                let floats = |k: &str| -> [f32; 4] {
                    let v = &a[k];
                    [0, 1, 2, 3].map(|i| v.get(i).and_then(Value::as_f64).unwrap_or(0.0) as f32)
                };
                match call.as_str() {
                    "ClearRenderTargetView" => {
                        if let Some(v) = self.rtv(c, view)? {
                            unsafe { ctx.ClearRenderTargetView(&v, &floats("color")) };
                        }
                    }
                    "ClearDepthStencilView" => {
                        if let Some(v) = self.dsv(c, view)? {
                            unsafe {
                                ctx.ClearDepthStencilView(
                                    &v,
                                    json_u32(&a, "flags"),
                                    a["depth"].as_f64().unwrap_or(0.0) as f32,
                                    json_u32(&a, "stencil") as u8,
                                )
                            };
                        }
                    }
                    "ClearUnorderedAccessViewUint" => {
                        if let Some(v) = self.uav(c, view)? {
                            let vals = &a["values"];
                            let u =
                                [0, 1, 2, 3].map(|i| vals.get(i).and_then(Value::as_u64).unwrap_or(0) as u32);
                            unsafe { ctx.ClearUnorderedAccessViewUint(&v, &u) };
                        }
                    }
                    "ClearUnorderedAccessViewFloat" => {
                        if let Some(v) = self.uav(c, view)? {
                            unsafe { ctx.ClearUnorderedAccessViewFloat(&v, &floats("values")) };
                        }
                    }
                    _ => {
                        let rects: Vec<RECT> = a["rects"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|r| {
                                let n = |i: usize| r.get(i).and_then(Value::as_i64).unwrap_or(0) as i32;
                                RECT {
                                    left: n(0),
                                    top: n(1),
                                    right: n(2),
                                    bottom: n(3),
                                }
                            })
                            .collect();
                        let target: Option<ID3D11View> = match self.view(c, view)? {
                            Some(View::Rtv(v)) => v.cast().ok(),
                            Some(View::Dsv(v)) => v.cast().ok(),
                            Some(View::Uav(v)) => v.cast().ok(),
                            Some(View::Srv(v)) => v.cast().ok(),
                            None => None,
                        };
                        if let Some(v) = target {
                            let rects = (!rects.is_empty()).then_some(&rects[..]);
                            unsafe { ctx.ClearView(&v, &floats("color"), rects) };
                        }
                    }
                }
            }
            "resolve" => {
                let (d, s) = (
                    self.resource(json_u32(&a, "dst") as i64)?.clone(),
                    self.resource(json_u32(&a, "src") as i64)?.clone(),
                );
                unsafe {
                    ctx.ResolveSubresource(
                        &d,
                        json_u32(&a, "dst_sub"),
                        &s,
                        json_u32(&a, "src_sub"),
                        DXGI_FORMAT(json_u32(&a, "format") as i32),
                    )
                };
            }
            "mips" => {
                if let Some(v) = self.srv(c, Some(json_u32(&a, "view") as i64))? {
                    unsafe { ctx.GenerateMips(&v) };
                }
            }
            "copy_count" => {
                let d = self.buffer(json_u32(&a, "dst") as i64)?;
                if let Some(v) = self.uav(c, json_u32(&a, "view") as i64)? {
                    unsafe { ctx.CopyStructureCount(&d, json_u32(&a, "offset"), &v) };
                }
            }
            "uav_counter" => {
                for s in a["set"].as_array().into_iter().flatten() {
                    let Some(v) = self.uav(c, json_u32(s, "view") as i64)? else {
                        continue;
                    };
                    let count = json_u32(s, "count");
                    let slot = json_u32(s, "slot");
                    unsafe {
                        if call.starts_with("CS") {
                            ctx.CSSetUnorderedAccessViews(slot, 1, Some(&Some(v)), Some(&count));
                        } else {
                            // The next draw binds its own targets again; the counter stays with the view.
                            ctx.OMSetRenderTargetsAndUnorderedAccessViews(
                                None,
                                None,
                                slot,
                                1,
                                Some(&Some(v)),
                                Some(&count),
                            );
                        }
                    }
                }
            }
            // Discards leave contents undefined; keeping them is one valid outcome. Map starts and reads change
            // nothing on the GPU.
            "discard" | "map_begin" | "map_read" => {}
            other => self.warn(format!("events of kind {other} are not replayed")),
        }
        Ok(())
    }

    fn read(&mut self, id: i64) -> Result<Vec<u8>, String> {
        let r = self.resource(id)?.clone();
        let ctx: ID3D11DeviceContext = self.ctx.cast().map_err(|e| e.to_string())?;
        let rb = unsafe { locum_d3d11::readback::issue(&ctx, &r) }?;
        unsafe { rb.read(&ctx) }
    }

    fn write(&mut self, c: &Capture, id: i64, contents: &[u8]) -> Result<(), String> {
        let s = shape(c, id).ok_or("not described in the capture")?;
        let subs = s.subs();
        if contents.len() as u64 != layout::total(&subs) {
            return Err(format!(
                "#{id}: {} bytes do not fit its {} bytes of subresources",
                contents.len(),
                layout::total(&subs)
            ));
        }
        let r = self.resource(id)?.clone();
        for x in &subs {
            unsafe {
                self.ctx.UpdateSubresource(
                    &r,
                    x.index,
                    None,
                    contents[x.offset as usize..].as_ptr().cast(),
                    x.row_bytes,
                    x.row_bytes * x.rows,
                )
            };
        }
        Ok(())
    }

    fn warnings(&self) -> Vec<String> {
        let mut warnings = self.warnings.clone();
        if let Some(queue) = &self.debug {
            unsafe {
                if queue.GetNumMessagesDiscardedByMessageCountLimit() != 0 {
                    warnings.insert("D3D11 validation messages were lost to the queue limit".into());
                }
                for i in 0..queue.GetNumStoredMessagesAllowedByRetrievalFilter() {
                    let mut size = 0;
                    if let Err(e) = queue.GetMessage(i, None, &mut size) {
                        warnings.insert(format!("D3D11 validation message {i}: {e}"));
                        continue;
                    }
                    // The queue writes a D3D11_MESSAGE followed by its text; keep the header aligned.
                    let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
                    let message = storage.as_mut_ptr().cast::<D3D11_MESSAGE>();
                    if let Err(e) = queue.GetMessage(i, Some(message), &mut size) {
                        warnings.insert(format!("D3D11 validation message {i}: {e}"));
                        continue;
                    }
                    let message = &*message;
                    if matches!(
                        message.Severity,
                        D3D11_MESSAGE_SEVERITY_ERROR | D3D11_MESSAGE_SEVERITY_CORRUPTION
                    ) {
                        let text =
                            std::slice::from_raw_parts(message.pDescription, message.DescriptionByteLength);
                        warnings.insert(format!(
                            "D3D11 validation: {}",
                            String::from_utf8_lossy(text).trim_end_matches('\0')
                        ));
                    }
                }
            }
        }
        warnings.into_iter().collect()
    }
}
