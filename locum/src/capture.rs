//! One-frame capture: every draw and dispatch of a frame with the full state it ran with.
//!
//! Requested by creating the file `capture` in locum's folder; it is removed when seen. The capture then runs over
//! three presents:
//! - **pre-recording** (from the present that sees the file): deferred contexts record their draws, so command lists
//!   recorded now and executed in the captured frame are complete;
//! - **recording** (the next frame): draws on the immediate context, and command lists as they are executed, form the
//!   frame's timeline;
//! - **finish** (the present ending that frame): the constant-buffer copies are read back and everything is written
//!   to `captures/frame-<n>/draws.jsonl`.
//!
//! The state of each draw is read from the real context with its getters (the runtime tracks it; locum tracks nothing
//! while not capturing). Constant buffers are copied by the GPU at the draw, on the draw's own context, into staging
//! buffers: a deferred context's copy runs when its command list runs, so it sees exactly what the draw saw.

use core::ffi::c_void;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT;
use windows::core::{GUID, Interface};

use crate::log::{folder, log};
use crate::shaders;

/// True from pre-recording to the end of the captured frame: the only check on the draw path otherwise.
static ARMED: AtomicBool = AtomicBool::new(false);

pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// The private-data key of the sequential id locum gives each resource, view and state object it records.
const ID_KEY: GUID = GUID::from_u128(0x6c6f_6375_6d00_4a5e_9d1b_7a2f_8c3e_0002);

/// Staging buffers for constant-buffer copies: 32 MiB each, at most 16 (512 MiB) per capture.
const CHUNK: u32 = 32 << 20;
const MAX_CHUNKS: usize = 16;
/// The most of one bound constant-buffer range copied (the whole D3D11 limit is 64 KiB: 4096 constants).
const MAX_CB_COPY: u32 = 65536;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    PreRecording,
    Recording,
}

/// A constant buffer bound at a draw, and where its copy went.
struct CbBind {
    stage: &'static str,
    slot: u32,
    resource: u64,
    first: u32,
    count: u32,
    /// The bound buffer, until its copy is recorded.
    source: Option<ID3D11Buffer>,
    copy: Option<(u32, u32, u32)>, // chunk, offset, bytes
}

struct Draw {
    context: u32,
    deferred: bool,
    call: &'static str,
    args: Vec<i64>,
    shaders: Vec<(&'static str, Option<u64>)>,
    cbs: Vec<CbBind>,
    /// stage, slot, view id, resource id
    srvs: Vec<(&'static str, u32, u64, u64)>,
    /// slot, view id, resource id
    rtvs: Vec<(u32, u64, u64)>,
    dsv: Option<(u64, u64)>,
    topology: i32,
    layout: u64,
    /// slot, buffer id, stride, offset
    vbs: Vec<(u32, u64, u32, u32)>,
    ib: Option<(u64, i32, u32)>,
    raster: u64,
    blend: u64,
    depth: u64,
}

struct State {
    phase: Phase,
    frame: u64,
    /// Draws recorded on a deferred context since its last FinishCommandList, by context id.
    pending: BTreeMap<u32, Vec<Draw>>,
    /// Draws of finished command lists not yet executed, by command list address.
    lists: BTreeMap<usize, Vec<Draw>>,
    timeline: Vec<Draw>,
    lists_spliced: u32,
    lists_unrecorded: u32,
    staging: Vec<ID3D11Buffer>,
    used: u32,
    copy_failures: u32,
    next_id: u64,
    /// Captures requested so far: the current capture's number.
    captures: u64,
    /// JSON lines describing each object the first time it is recorded.
    objects: Vec<String>,
}

static STATE: Mutex<State> = Mutex::new(State {
    phase: Phase::Idle,
    frame: 0,
    pending: BTreeMap::new(),
    lists: BTreeMap::new(),
    timeline: Vec::new(),
    lists_spliced: 0,
    lists_unrecorded: 0,
    staging: Vec::new(),
    used: 0,
    copy_failures: 0,
    next_id: 1,
    captures: 0,
    objects: Vec::new(),
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ids for contexts, handed out as they are wrapped.
pub fn next_context_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// At each present, before the real one. Ends a frame being recorded.
pub fn before_present() -> bool {
    let mut s = state();
    s.frame += 1;
    if s.phase == Phase::Recording {
        ARMED.store(false, Ordering::Relaxed);
        s.phase = Phase::Idle;
        return true;
    }
    false
}

/// At each present, after the real one. `finished` is what [`before_present`] returned; `immediate` is the real
/// immediate context (for reading the copies back).
pub fn after_present(finished: bool, immediate: &ID3D11DeviceContext4) {
    if finished {
        finish(immediate);
        return;
    }
    let mut s = state();
    match s.phase {
        Phase::Idle => {
            // Look for a request every 30 frames.
            if !s.frame.is_multiple_of(30) {
                return;
            }
            let Some(request) = folder().map(|d| d.join("capture")) else {
                return;
            };
            if std::fs::remove_file(&request).is_ok() {
                log!("capture requested at frame {}: pre-recording", s.frame);
                s.phase = Phase::PreRecording;
                s.captures += 1;
                s.objects.clear();
                ARMED.store(true, Ordering::Relaxed);
            }
        }
        Phase::PreRecording => {
            log!("capture: recording frame {}", s.frame + 1);
            s.phase = Phase::Recording;
            s.timeline.clear();
        }
        Phase::Recording => {}
    }
}

/// A draw or dispatch on a context, while armed. `context` is the real context it runs on.
pub fn on_draw(context: &ID3D11DeviceContext4, id: u32, deferred: bool, call: &'static str, args: &[i64]) {
    {
        let s = state();
        if s.phase == Phase::Idle || (!deferred && s.phase != Phase::Recording) {
            return;
        }
    }
    let compute = call.starts_with("Dispatch");
    let mut draw = Draw {
        context: id,
        deferred,
        call,
        args: args.to_vec(),
        shaders: Vec::new(),
        cbs: Vec::new(),
        srvs: Vec::new(),
        rtvs: Vec::new(),
        dsv: None,
        topology: 0,
        layout: 0,
        vbs: Vec::new(),
        ib: None,
        raster: 0,
        blend: 0,
        depth: 0,
    };
    unsafe { snapshot(context, compute, &mut draw) };
    // Constant-buffer copies, recorded on this context right now.
    for cb in &mut draw.cbs {
        cb.copy = unsafe { copy_cb(context, &mut *cb) };
    }
    let mut s = state();
    if deferred {
        s.pending.entry(id).or_default().push(draw);
    } else if s.phase == Phase::Recording {
        s.timeline.push(draw);
    }
}

/// A deferred context's FinishCommandList: its pending draws (possibly none) belong to the list.
pub fn on_finish_list(id: u32, list: *mut c_void) {
    let mut s = state();
    let draws = s.pending.remove(&id).unwrap_or_default();
    if !list.is_null() {
        s.lists.insert(list as usize, draws);
    }
}

/// ExecuteCommandList. On the immediate context the list's draws join the timeline; on a deferred context (a list
/// run from inside another) they join that context's pending draws. A list can be executed more than once, so its
/// recording is kept until the capture ends.
pub fn on_execute_list(id: u32, deferred: bool, list: *mut c_void) {
    let mut s = state();
    if !deferred && s.phase != Phase::Recording {
        return;
    }
    let Some(draws) = s
        .lists
        .get(&(list as usize))
        .map(|d| d.iter().map(Draw::again).collect::<Vec<_>>())
    else {
        if !deferred {
            s.lists_unrecorded += 1;
        }
        return;
    };
    if deferred {
        s.pending.entry(id).or_default().extend(draws);
    } else {
        s.lists_spliced += 1;
        s.timeline.extend(draws);
    }
}

impl Draw {
    /// A copy for a second execution of the same command list (the buffer sources are gone by then).
    fn again(&self) -> Draw {
        Draw {
            context: self.context,
            deferred: self.deferred,
            call: self.call,
            args: self.args.clone(),
            shaders: self.shaders.clone(),
            cbs: self
                .cbs
                .iter()
                .map(|c| CbBind {
                    stage: c.stage,
                    slot: c.slot,
                    resource: c.resource,
                    first: c.first,
                    count: c.count,
                    source: None,
                    copy: c.copy,
                })
                .collect(),
            srvs: self.srvs.clone(),
            rtvs: self.rtvs.clone(),
            dsv: self.dsv,
            topology: self.topology,
            layout: self.layout,
            vbs: self.vbs.clone(),
            ib: self.ib,
            raster: self.raster,
            blend: self.blend,
            depth: self.depth,
        }
    }
}

/// The id of a buffer passed to an indirect draw (0 for null).
///
/// # Safety
/// `buffer` must be null or a live `ID3D11Buffer`.
pub unsafe fn buffer_id(buffer: *mut c_void) -> u64 {
    let Some(buffer) = (unsafe { ID3D11Resource::from_raw_borrowed(&buffer) }) else {
        return 0;
    };
    unsafe { resource_id(buffer) }
}

/// The id locum gave `object`, giving it one the first time. An object is described (a line in the capture's file)
/// the first time each capture records it, so every capture's file stands alone.
unsafe fn id_of(object: &ID3D11DeviceChild, describe: impl FnOnce(u64) -> String) -> u64 {
    // [id, number of the capture that last described it]
    let mut data = [0u64; 2];
    let mut size = 16u32;
    let known = unsafe { object.GetPrivateData(&ID_KEY, &mut size, Some(data.as_mut_ptr().cast())) }.is_ok()
        && size == 16;
    let mut s = state();
    if known && data[1] == s.captures {
        return data[0];
    }
    let id = if known {
        data[0]
    } else {
        s.next_id += 1;
        s.next_id - 1
    };
    let data = [id, s.captures];
    drop(s);
    let _ = unsafe { object.SetPrivateData(&ID_KEY, 16, Some(data.as_ptr().cast())) };
    let line = describe(id);
    state().objects.push(line);
    id
}

unsafe fn resource_id(resource: &ID3D11Resource) -> u64 {
    let child: ID3D11DeviceChild = resource.cast().expect("a resource is a device child");
    unsafe { id_of(&child, |id| describe_resource(id, resource)) }
}

fn describe_resource(id: u64, resource: &ID3D11Resource) -> String {
    if let Ok(t) = resource.cast::<ID3D11Texture2D>() {
        let mut d = D3D11_TEXTURE2D_DESC::default();
        unsafe { t.GetDesc(&mut d) };
        return format!(
            "{{\"e\":\"resource\",\"id\":{id},\"kind\":\"texture2d\",\"width\":{},\"height\":{},\"mips\":{},\"array\":{},\"format\":{},\"samples\":{},\"usage\":{},\"bind\":{},\"misc\":{}}}",
            d.Width,
            d.Height,
            d.MipLevels,
            d.ArraySize,
            d.Format.0,
            d.SampleDesc.Count,
            d.Usage.0,
            d.BindFlags,
            d.MiscFlags
        );
    }
    if let Ok(b) = resource.cast::<ID3D11Buffer>() {
        let mut d = D3D11_BUFFER_DESC::default();
        unsafe { b.GetDesc(&mut d) };
        return format!(
            "{{\"e\":\"resource\",\"id\":{id},\"kind\":\"buffer\",\"bytes\":{},\"usage\":{},\"bind\":{},\"misc\":{},\"stride\":{}}}",
            d.ByteWidth, d.Usage.0, d.BindFlags, d.MiscFlags, d.StructureByteStride
        );
    }
    if let Ok(t) = resource.cast::<ID3D11Texture3D>() {
        let mut d = D3D11_TEXTURE3D_DESC::default();
        unsafe { t.GetDesc(&mut d) };
        return format!(
            "{{\"e\":\"resource\",\"id\":{id},\"kind\":\"texture3d\",\"width\":{},\"height\":{},\"depth\":{},\"mips\":{},\"format\":{}}}",
            d.Width, d.Height, d.Depth, d.MipLevels, d.Format.0
        );
    }
    if let Ok(t) = resource.cast::<ID3D11Texture1D>() {
        let mut d = D3D11_TEXTURE1D_DESC::default();
        unsafe { t.GetDesc(&mut d) };
        return format!(
            "{{\"e\":\"resource\",\"id\":{id},\"kind\":\"texture1d\",\"width\":{},\"mips\":{},\"array\":{},\"format\":{}}}",
            d.Width, d.MipLevels, d.ArraySize, d.Format.0
        );
    }
    format!("{{\"e\":\"resource\",\"id\":{id},\"kind\":\"unknown\"}}")
}

/// A view's id, and its resource's id.
unsafe fn view_ids(view: &ID3D11View, kind: &str, format: DXGI_FORMAT, dimension: i32) -> (u64, u64) {
    let resource = unsafe { view.GetResource() }.ok();
    let resource_id = resource.as_ref().map_or(0, |r| unsafe { resource_id(r) });
    let child: ID3D11DeviceChild = view.cast().expect("a view is a device child");
    let id = unsafe {
        id_of(&child, |id| {
            format!(
                "{{\"e\":\"view\",\"id\":{id},\"kind\":\"{kind}\",\"resource\":{resource_id},\"format\":{},\"dimension\":{dimension}}}",
                format.0
            )
        })
    };
    (id, resource_id)
}

unsafe fn state_id(object: Option<ID3D11DeviceChild>, kind: &str) -> u64 {
    let Some(object) = object else { return 0 };
    unsafe {
        id_of(&object, |id| {
            format!("{{\"e\":\"state\",\"id\":{id},\"kind\":\"{kind}\"}}")
        })
    }
}

const GRAPHICS_STAGES: [&str; 5] = ["vs", "hs", "ds", "gs", "ps"];

/// Reads the state a draw runs with from the real context.
unsafe fn snapshot(c: &ID3D11DeviceContext4, compute: bool, draw: &mut Draw) {
    unsafe {
        let stages: &[&str] = if compute { &["cs"] } else { &GRAPHICS_STAGES };
        for &stage in stages {
            let shader: Option<ID3D11DeviceChild> = match stage {
                "vs" => {
                    let mut s = None;
                    c.VSGetShader(&mut s, None, None);
                    s.map(|s| s.cast().expect("child"))
                }
                "hs" => {
                    let mut s = None;
                    c.HSGetShader(&mut s, None, None);
                    s.map(|s| s.cast().expect("child"))
                }
                "ds" => {
                    let mut s = None;
                    c.DSGetShader(&mut s, None, None);
                    s.map(|s| s.cast().expect("child"))
                }
                "gs" => {
                    let mut s = None;
                    c.GSGetShader(&mut s, None, None);
                    s.map(|s| s.cast().expect("child"))
                }
                "ps" => {
                    let mut s = None;
                    c.PSGetShader(&mut s, None, None);
                    s.map(|s| s.cast().expect("child"))
                }
                _ => {
                    let mut s = None;
                    c.CSGetShader(&mut s, None, None);
                    s.map(|s| s.cast().expect("child"))
                }
            };
            let Some(shader) = shader else { continue };
            draw.shaders.push((stage, shaders::hash_of(shader.as_raw())));

            let mut buffers: [Option<ID3D11Buffer>; 14] = Default::default();
            let mut firsts = [0u32; 14];
            let mut counts = [0u32; 14];
            let (b, f, n) = (buffers.as_mut_ptr(), firsts.as_mut_ptr(), counts.as_mut_ptr());
            match stage {
                "vs" => c.VSGetConstantBuffers1(0, 14, Some(b), Some(f), Some(n)),
                "hs" => c.HSGetConstantBuffers1(0, 14, Some(b), Some(f), Some(n)),
                "ds" => c.DSGetConstantBuffers1(0, 14, Some(b), Some(f), Some(n)),
                "gs" => c.GSGetConstantBuffers1(0, 14, Some(b), Some(f), Some(n)),
                "ps" => c.PSGetConstantBuffers1(0, 14, Some(b), Some(f), Some(n)),
                _ => c.CSGetConstantBuffers1(0, 14, Some(b), Some(f), Some(n)),
            }
            for (slot, buffer) in buffers.iter().enumerate() {
                let Some(buffer) = buffer else { continue };
                let resource: ID3D11Resource = buffer.cast().expect("resource");
                draw.cbs.push(CbBind {
                    stage,
                    slot: slot as u32,
                    resource: resource_id(&resource),
                    first: firsts[slot],
                    count: counts[slot],
                    source: Some(buffer.clone()),
                    copy: None,
                });
            }

            let mut views: [Option<ID3D11ShaderResourceView>; 128] = [const { None }; 128];
            match stage {
                "vs" => c.VSGetShaderResources(0, Some(&mut views)),
                "hs" => c.HSGetShaderResources(0, Some(&mut views)),
                "ds" => c.DSGetShaderResources(0, Some(&mut views)),
                "gs" => c.GSGetShaderResources(0, Some(&mut views)),
                "ps" => c.PSGetShaderResources(0, Some(&mut views)),
                _ => c.CSGetShaderResources(0, Some(&mut views)),
            }
            for (slot, view) in views.iter().enumerate() {
                let Some(view) = view else { continue };
                let mut d = D3D11_SHADER_RESOURCE_VIEW_DESC::default();
                view.GetDesc(&mut d);
                let v: ID3D11View = view.cast().expect("view");
                let (id, resource) = view_ids(&v, "srv", d.Format, d.ViewDimension.0);
                draw.srvs.push((stage, slot as u32, id, resource));
            }
        }
        if compute {
            return;
        }

        let mut rtvs: [Option<ID3D11RenderTargetView>; 8] = Default::default();
        let mut dsv = None;
        c.OMGetRenderTargets(Some(&mut rtvs), Some(&mut dsv));
        for (slot, view) in rtvs.iter().enumerate() {
            let Some(view) = view else { continue };
            let mut d = D3D11_RENDER_TARGET_VIEW_DESC::default();
            view.GetDesc(&mut d);
            let v: ID3D11View = view.cast().expect("view");
            let (id, resource) = view_ids(&v, "rtv", d.Format, d.ViewDimension.0);
            draw.rtvs.push((slot as u32, id, resource));
        }
        if let Some(view) = dsv {
            let mut d = D3D11_DEPTH_STENCIL_VIEW_DESC::default();
            view.GetDesc(&mut d);
            let v: ID3D11View = view.cast().expect("view");
            draw.dsv = Some(view_ids(&v, "dsv", d.Format, d.ViewDimension.0));
        }

        let D3D_PRIMITIVE_TOPOLOGY(t) = c.IAGetPrimitiveTopology();
        draw.topology = t;
        draw.layout = state_id(
            c.IAGetInputLayout().ok().map(|l| l.cast().expect("child")),
            "input_layout",
        );
        let mut vbs: [Option<ID3D11Buffer>; 16] = Default::default();
        let mut strides = [0u32; 16];
        let mut offsets = [0u32; 16];
        c.IAGetVertexBuffers(
            0,
            16,
            Some(vbs.as_mut_ptr()),
            Some(strides.as_mut_ptr()),
            Some(offsets.as_mut_ptr()),
        );
        for (slot, vb) in vbs.iter().enumerate() {
            let Some(vb) = vb else { continue };
            let resource: ID3D11Resource = vb.cast().expect("resource");
            draw.vbs
                .push((slot as u32, resource_id(&resource), strides[slot], offsets[slot]));
        }
        let mut ib = None;
        let mut ib_format = DXGI_FORMAT::default();
        let mut ib_offset = 0u32;
        c.IAGetIndexBuffer(Some(&mut ib), Some(&mut ib_format), Some(&mut ib_offset));
        if let Some(ib) = ib {
            let resource: ID3D11Resource = ib.cast().expect("resource");
            draw.ib = Some((resource_id(&resource), ib_format.0, ib_offset));
        }

        draw.raster = state_id(
            c.RSGetState().ok().map(|s| s.cast().expect("child")),
            "rasterizer",
        );
        let mut blend = None;
        c.OMGetBlendState(Some(&mut blend), None, None);
        draw.blend = state_id(blend.map(|s| s.cast().expect("child")), "blend");
        let mut depth = None;
        c.OMGetDepthStencilState(Some(&mut depth), None);
        draw.depth = state_id(depth.map(|s| s.cast().expect("child")), "depth_stencil");
    }
}

/// Records a GPU copy of the bound range of a constant buffer into the staging pool, on context `c`.
unsafe fn copy_cb(c: &ID3D11DeviceContext4, cb: &mut CbBind) -> Option<(u32, u32, u32)> {
    let source = cb.source.take()?;
    let mut desc = D3D11_BUFFER_DESC::default();
    unsafe { source.GetDesc(&mut desc) };
    let start = cb.first.saturating_mul(16).min(desc.ByteWidth);
    let wanted = if cb.count == 0 {
        desc.ByteWidth
    } else {
        cb.count.saturating_mul(16)
    };
    let bytes = wanted.min(desc.ByteWidth - start).min(MAX_CB_COPY);
    if bytes == 0 {
        return None;
    }
    let (chunk, offset, staging) = {
        let mut s = state();
        let aligned = (bytes + 15) & !15;
        if s.staging.is_empty() || s.used + aligned > CHUNK {
            if s.staging.len() >= MAX_CHUNKS {
                s.copy_failures += 1;
                return None;
            }
            let device = unsafe { c.GetDevice() }.ok()?;
            let d = D3D11_BUFFER_DESC {
                ByteWidth: CHUNK,
                Usage: D3D11_USAGE_STAGING,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..Default::default()
            };
            let mut buffer = None;
            if unsafe { device.CreateBuffer(&d, None, Some(&mut buffer)) }.is_err() {
                s.copy_failures += 1;
                return None;
            }
            s.staging.push(buffer?);
            s.used = 0;
        }
        let chunk = s.staging.len() as u32 - 1;
        let offset = s.used;
        s.used += aligned;
        (chunk, offset, s.staging[chunk as usize].clone())
    };
    let source_box = D3D11_BOX {
        left: start,
        top: 0,
        front: 0,
        right: start + bytes,
        bottom: 1,
        back: 1,
    };
    unsafe { c.CopySubresourceRegion(&staging, 0, offset, 0, 0, &source, 0, Some(&source_box)) };
    Some((chunk, offset, bytes))
}

/// Reads the copies back and writes the frame.
fn finish(immediate: &ID3D11DeviceContext4) {
    let mut s = state();
    let frame = s.frame;
    let Some(dir) = folder().map(|d| d.join("captures").join(format!("frame-{frame}"))) else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);

    // Map every staging chunk (waits for the GPU to finish the frame's copies).
    let mut chunks: Vec<Vec<u8>> = Vec::new();
    for (i, staging) in s.staging.iter().enumerate() {
        let len = if i + 1 == s.staging.len() { s.used } else { CHUNK } as usize;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        if unsafe { immediate.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.is_ok() {
            let bytes = unsafe { core::slice::from_raw_parts(mapped.pData as *const u8, len) }.to_vec();
            unsafe { immediate.Unmap(staging, 0) };
            chunks.push(bytes);
        } else {
            chunks.push(Vec::new());
        }
    }

    let mut out = String::new();
    let timeline = std::mem::take(&mut s.timeline);
    let _ = writeln!(
        out,
        "{{\"e\":\"frame\",\"frame\":{frame},\"draws\":{},\"lists_spliced\":{},\"lists_unrecorded\":{},\"copy_failures\":{}}}",
        timeline.len(),
        s.lists_spliced,
        s.lists_unrecorded,
        s.copy_failures
    );
    for line in s.objects.drain(..) {
        out.push_str(&line);
        out.push('\n');
    }
    let mut contents: HashSet<u64> = HashSet::new();
    for (index, d) in timeline.iter().enumerate() {
        let mut cbs = String::new();
        for cb in &d.cbs {
            let data = cb.copy.and_then(|(chunk, offset, bytes)| {
                let c = chunks.get(chunk as usize)?;
                c.get(offset as usize..(offset + bytes) as usize)
            });
            let content = data.map(|bytes| {
                let h = shaders::hash(bytes);
                if contents.insert(h) {
                    let _ = writeln!(
                        out,
                        "{{\"e\":\"cb\",\"hash\":\"{h:016x}\",\"bytes\":{},\"hex\":\"{}\"}}",
                        bytes.len(),
                        hex(bytes)
                    );
                }
                h
            });
            let _ = write!(
                cbs,
                "{}{{\"stage\":\"{}\",\"slot\":{},\"buffer\":{},\"first\":{},\"count\":{},\"content\":{}}}",
                if cbs.is_empty() { "" } else { "," },
                cb.stage,
                cb.slot,
                cb.resource,
                cb.first,
                cb.count,
                content.map_or("null".into(), |h| format!("\"{h:016x}\""))
            );
        }
        let shaders_json = d
            .shaders
            .iter()
            .map(|(stage, h)| {
                format!(
                    "\"{stage}\":{}",
                    h.map_or("null".into(), |h| format!("\"{h:016x}\""))
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let srvs = d
            .srvs
            .iter()
            .map(|(st, slot, v, r)| format!("[\"{st}\",{slot},{v},{r}]"))
            .collect::<Vec<_>>()
            .join(",");
        let rtvs = d
            .rtvs
            .iter()
            .map(|(slot, v, r)| format!("[{slot},{v},{r}]"))
            .collect::<Vec<_>>()
            .join(",");
        let vbs = d
            .vbs
            .iter()
            .map(|(slot, b, st, o)| format!("[{slot},{b},{st},{o}]"))
            .collect::<Vec<_>>()
            .join(",");
        let args = d.args.iter().map(|a| a.to_string()).collect::<Vec<_>>().join(",");
        let _ = writeln!(
            out,
            "{{\"e\":\"draw\",\"i\":{index},\"ctx\":{},\"deferred\":{},\"call\":\"{}\",\"args\":[{args}],\"shaders\":{{{shaders_json}}},\"cbs\":[{cbs}],\"srvs\":[{srvs}],\"rtvs\":[{rtvs}],\"dsv\":{},\"topology\":{},\"layout\":{},\"vbs\":[{vbs}],\"ib\":{},\"raster\":{},\"blend\":{},\"depth\":{}}}",
            d.context,
            d.deferred,
            d.call,
            d.dsv.map_or("null".into(), |(v, r)| format!("[{v},{r}]")),
            d.topology,
            d.layout,
            d.ib.map_or("null".into(), |(b, f, o)| format!("[{b},{f},{o}]")),
            d.raster,
            d.blend,
            d.depth,
        );
    }
    let path: PathBuf = dir.join("draws.jsonl");
    let written = std::fs::write(&path, out).is_ok();
    log!(
        "capture of frame {frame}: {} draws, {} command lists spliced, {} executed without a recording, {} staging chunks, {} copy failures -> {} ({})",
        timeline.len(),
        s.lists_spliced,
        s.lists_unrecorded,
        s.staging.len(),
        s.copy_failures,
        path.display(),
        if written { "written" } else { "NOT written" }
    );
    // Reset for the next capture (ids stay: they are on the objects).
    s.pending.clear();
    s.lists.clear();
    s.lists_spliced = 0;
    s.lists_unrecorded = 0;
    s.staging.clear();
    s.used = 0;
    s.copy_failures = 0;
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}
