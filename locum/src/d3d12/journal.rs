//! Continuous list history and per-submission capture. No COM ownership is kept in
//! the portable history; object IDs survive pointer and descriptor-slot reuse.
use core::ffi::c_void;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use super::cost::Counter;
use locum_capture::{d3d12 as model, db, writer};
use serde_json::{Value, json};
use windows::core::HRESULT;

use crate::log::{folder, log};

pub static RECORDING_COST: Counter = Counter::new(0);
pub static SUBMISSION_COST: Counter = Counter::new(1);
pub static ADDRESS_COST: Counter = Counter::new(2);
pub static DESCRIPTOR_COST: Counter = Counter::new(3);
pub static DESTROY_COST: Counter = Counter::new(4);
pub static UPLOAD_COST: Counter = Counter::new(5);
pub static RESET_COST: Counter = Counter::new(6);

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("LOCUM_D3D12_JOURNAL").as_deref() != Ok("off"))
}

pub struct Data {
    pub id: u64,
    pub recording: Mutex<Option<model::Recording>>,
    /// Held through the real queue operation so the recorded order is its actual CPU call order.
    pub queue_seq: Mutex<u64>,
    /// A direct list's passes while it is recorded with a capture armed (see [`super::passes`]).
    pub pass: Mutex<Option<super::passes::Pass>>,
}

impl Data {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            recording: Mutex::new(None),
            queue_seq: Mutex::new(0),
            pass: Mutex::new(None),
        }
    }
    /// Records a command before it is forwarded to the real list `real`; an instrumented list first records the
    /// copies of a pass the command ends.
    pub fn record(&self, real: *mut c_void, call: &'static str, build: impl FnOnce() -> Value) {
        if !enabled() {
            return;
        }
        let start = Instant::now();
        let args = build();
        let mut recording = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(recording) = recording.as_mut() {
            self.checkpoints(recording, |pass, at| unsafe {
                pass.before(real, call, &args, at)
            });
            recording.record(model::Command {
                call,
                args,
                bundle: None,
                diagnostic: None,
            });
        }
        RECORDING_COST.add(start, 0);
    }
    pub fn bundle(&self, real: *mut c_void, pointer: *mut c_void) {
        if !enabled() {
            return;
        }
        let bundle = super::data(pointer).and_then(|d| d.snapshot());
        let mut recording = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(recording) = recording.as_mut() {
            self.checkpoints(recording, |pass, at| unsafe {
                pass.before(real, "ExecuteBundle", &Value::Null, at)
            });
            let diagnostic = bundle
                .is_none()
                .then(|| "bundle has no recorded generation".into());
            recording.record(model::Command {
                call: "ExecuteBundle",
                args: json!({"list":super::id(pointer)}),
                bundle,
                diagnostic,
            });
        }
    }
    pub fn snapshot(&self) -> Option<Arc<model::List>> {
        self.recording
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|r| r.snapshot())
    }
    /// The checkpoints `make` records into an instrumented list, at its next command, and the outputs it copies.
    fn checkpoints(
        &self,
        recording: &mut model::Recording,
        make: impl FnOnce(&mut super::passes::Pass, usize) -> Vec<model::Checkpoint>,
    ) {
        let mut pass = self.pass.lock().unwrap_or_else(|e| e.into_inner());
        let Some(pass) = pass.as_mut() else {
            return;
        };
        let checkpoints = make(pass, recording.len());
        self.keep(recording, checkpoints, pass.outputs());
    }
    /// Assigns copies made in the list to its generation.
    fn keep(
        &self,
        recording: &mut model::Recording,
        checkpoints: Vec<model::Checkpoint>,
        outputs: Vec<model::Output>,
    ) {
        super::passes::belong(
            self.id,
            checkpoints
                .iter()
                .map(|c| c.id)
                .chain(outputs.iter().map(|o| o.id)),
        );
        for c in checkpoints {
            recording.checkpoint(c);
        }
        for o in outputs {
            recording.output(o);
        }
    }
    /// Before Close is forwarded to the real list `real`: what its last command wrote is copied, a pass still open
    /// is pending, and the generation keeps what its pass tracking learned of the targets it continues and the
    /// resources it names.
    pub fn closing(&self, real: *mut c_void) {
        if !enabled() {
            return;
        }
        let mut recording = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        let pass = self.pass.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let (Some(recording), Some(mut pass)) = (recording.as_mut(), pass) {
            let checkpoints = unsafe { pass.closing(real, recording.len()) };
            self.keep(recording, checkpoints, pass.outputs());
            let (continues, written) = pass.touches();
            recording.touches(continues, written);
        }
    }
    pub fn close(&self, hr: HRESULT) {
        if hr.is_ok()
            && let Some(r) = self.recording.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            r.close();
        }
    }
    /// After Reset of the real list `real`: the previous generation's copies are retired, and the new one is
    /// instrumented while a capture is armed.
    pub fn reset(&self, real: *mut c_void, hr: HRESULT, allocator: *mut c_void, pso: *mut c_void) {
        let start = Instant::now();
        let (allocator, pso) = (super::id(allocator), super::id(pso));
        super::passes::retire(self.id);
        if hr.is_ok()
            && let Some(r) = self.recording.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            r.reset(allocator, pso);
            self.instrument(r, real, pso);
        }
        RESET_COST.add(start, 0);
    }
    /// Instruments a generation opened with pipeline `pso` while a capture is armed, if the list is a direct one.
    fn instrument(&self, recording: &mut model::Recording, real: *mut c_void, pso: u64) {
        let pass = if super::passes::ARMED.load(std::sync::atomic::Ordering::Acquire) {
            unsafe { super::passes::Pass::new(real, pso) }
        } else {
            None
        };
        if pass.is_some() {
            recording.instrument();
        }
        *self.pass.lock().unwrap_or_else(|e| e.into_inner()) = pass;
    }
}

#[derive(Default)]
struct State {
    objects: BTreeMap<u64, model::Object>,
    uploads: BTreeMap<u64, Upload>,
    cpu_seq: u64,
    next_submission: u64,
    /// The armed frame, projected one submission at a time.
    capture: Option<Live>,
    /// The descriptor versions the armed frame's submissions have reported.
    descriptors: super::slots::Known,
    /// The armed frame's constant-buffer views as of its latest submission: (heap, slot) to (resource, begin, end).
    constant_buffers: BTreeMap<(u64, u32), (u64, u64, u64)>,
    /// Every texture's state as the next submission will find it.
    states: super::states::Tracker,
    /// The series the armed frame belongs to (`frames N` in the request): the next frame is armed as it closes.
    series: Option<Series>,
    /// Fence values each queue has been told to wait for and no submitted Signal is known to reach, by queue.
    waits: HashMap<u64, Vec<(u64, u64)>>,
    /// The highest value submitted to signal each fence, by fence.
    signals: HashMap<u64, u64>,
}

/// Consecutive frames one request asked for, each a capture of its own.
#[derive(Clone, Debug)]
struct Series {
    /// The request's label: the captures are `<label>-1` to `<label>-N`; `frame-<n>` each without one.
    label: Option<String>,
    /// The armed frame's place in the series, from 1.
    index: u32,
    frames: u32,
}

/// The most frames one request captures.
const MAX_FRAMES: u32 = 64;

/// An armed frame: its history so far, projected as it is submitted, and the copies its submissions made.
struct Live {
    /// Its place in its series, when the request asked for more than one frame.
    series: Option<Series>,
    path: PathBuf,
    frame: model::Frame,
    projection: model::Projection,
    capture: db::Capture,
    results: super::readback::Results,
    /// The resources a submission of the frame has used: their first-use copies are made, or given up.
    used: HashSet<u64>,
    /// The list generations executed in the frame: a generation's first execution has its checkpoints' bytes.
    executed: HashSet<(u64, u64)>,
    /// The reserved resources whose page tables as the frame found them are recorded (see [`super::tiles`]).
    paged: HashSet<u64>,
}
#[derive(Default)]
struct Upload {
    pointer: usize,
    maps: u32,
    bytes: usize,
}
fn state() -> MutexGuard<'static, State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE
        .get_or_init(|| Mutex::new(State::default()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

pub fn register(id: u64, kind: &'static str) {
    if !enabled() {
        return;
    }
    state().objects.entry(id).or_insert_with(|| model::Object {
        id,
        kind,
        device: 0,
        description: json!({}),
    });
}
pub fn describe(id: u64, device: u64, description: Value) {
    let mut s = state();
    if let Some(object) = s.objects.get_mut(&id) {
        object.device = device;
        object.description = description;
        let object = object.clone();
        if let Some(live) = s.capture.as_mut() {
            live.projection.describe(id, &object.description);
            live.frame.objects.retain(|o| o.id != id);
            live.frame.objects.push(object);
        }
    }
}
/// A resource's states from its creation in `initial` (see [`super::states`]).
pub fn track(id: u64, description: &Value, initial: super::states::Access) {
    if !enabled() {
        return;
    }
    let format = description["format"].as_u64().unwrap_or(0) as u32;
    let planes = if locum_capture::format::is_depth_stencil(format) {
        2
    } else {
        1
    };
    state().states.create(id, description, initial, planes);
}
pub fn owner_device(id: u64) -> u64 {
    state().objects.get(&id).map_or(0, |o| o.device)
}
pub fn destroy(id: u64) {
    let start = Instant::now();
    super::passes::destroyed(id);
    super::outputs::destroyed(id);
    {
        let mut s = state();
        s.objects.remove(&id);
        s.uploads.remove(&id);
        s.states.destroy(id);
    }
    super::tiles::destroyed(id);
    super::slots::destroy(id);
    DESTROY_COST.add(start, 0);
}
pub fn resource_address(id: u64, address: u64, bytes: u64) {
    super::slots::register_address(id, address, bytes);
}
pub fn descriptor_addresses(id: u64, cpu: u64, gpu: u64, count: u32, stride: u32) {
    super::slots::register_heap(id, cpu, gpu, count, stride);
}
pub fn address(value: u64) -> Value {
    if value == 0 {
        return Value::Null;
    }
    let start = Instant::now();
    let result = super::slots::address(value);
    ADDRESS_COST.add(start, 0);
    result
}
pub fn constant_buffer(address: u64, bytes: u32) -> super::descriptors::Description {
    let start = Instant::now();
    let result = if let Some((resource, offset)) = super::slots::unique_address(address) {
        super::descriptors::Description::Cb {
            resource,
            offset,
            bytes,
        }
    } else {
        let buffer = if address == 0 {
            Value::Null
        } else {
            super::slots::address(address)
        };
        super::descriptors::Description::Json(json!({"buffer":buffer,"bytes":bytes}))
    };
    ADDRESS_COST.add(start, 0);
    result
}
pub fn descriptor(value: u64, gpu: bool) -> Value {
    if value == 0 {
        return Value::Null;
    }
    let start = Instant::now();
    let result = super::slots::descriptor(value, gpu);
    DESCRIPTOR_COST.add(start, 0);
    result
}
pub fn set_descriptor(
    handle: u64,
    kind: &'static str,
    resource: u64,
    counter: u64,
    description: super::descriptors::Description,
) {
    if !enabled() {
        return;
    }
    let start = Instant::now();
    super::slots::set(handle, kind, resource, counter, description);
    DESCRIPTOR_COST.add(start, 0);
}
pub fn copy_descriptors(destinations: &[u64], sources: &[u64]) {
    if !enabled() {
        return;
    }
    let start = Instant::now();
    super::slots::copy(destinations, sources);
    DESCRIPTOR_COST.add(start, 0);
}
pub fn copy_descriptors_simple(count: u32, destination: u64, source: u64) {
    if !enabled() {
        return;
    }
    let start = Instant::now();
    super::slots::copy_simple(count, destination, source);
    DESCRIPTOR_COST.add(start, 0);
}
pub fn descriptor_stride(handle: u64) -> Option<u32> {
    super::slots::stride(handle)
}
pub fn render_targets(
    pointer: *const windows::Win32::Graphics::Direct3D12::D3D12_CPU_DESCRIPTOR_HANDLE,
    count: u32,
    contiguous: bool,
) -> Value {
    if count == 0 {
        return json!([]);
    }
    if contiguous {
        let Some(first) = (unsafe { pointer.as_ref() }) else {
            return json!([]);
        };
        let Some(stride) = descriptor_stride(first.ptr as u64) else {
            return json!({"diagnostic":"contiguous RTV heap is not registered","count":count});
        };
        json!(
            (0..count)
                .map(|i| descriptor(first.ptr as u64 + u64::from(i) * u64::from(stride), false))
                .collect::<Vec<_>>()
        )
    } else {
        json!(
            unsafe { slice(pointer, count as usize) }
                .iter()
                .map(|h| descriptor(h.ptr as u64, false))
                .collect::<Vec<_>>()
        )
    }
}

pub unsafe fn mapped(resource: u64, sub: u32, out: *mut *mut c_void) {
    if !enabled() || sub != 0 {
        return;
    }
    let mut s = state();
    let Some(o) = s.objects.get(&resource) else {
        return;
    };
    if o.description["dimension"] != 1 || o.description["heap_properties"]["type"] != 2 {
        return;
    }
    let bytes = o.description["width"].as_u64().unwrap_or(0) as usize;
    let u = s.uploads.entry(resource).or_insert_with(|| Upload {
        bytes,
        ..Upload::default()
    });
    u.maps += 1;
    if !out.is_null() {
        u.pointer = unsafe { *out } as usize;
    }
}
pub fn unmap(resource: u64, sub: u32) {
    if !enabled() || sub != 0 {
        return;
    }
    let mut s = state();
    if let Some(u) = s.uploads.get_mut(&resource) {
        u.maps = u.maps.saturating_sub(1);
        if u.maps == 0 {
            u.pointer = 0;
        }
    }
}

pub unsafe fn created_list(
    device: u64,
    out: *mut *mut c_void,
    kind: i32,
    allocator: *mut c_void,
    pso: *mut c_void,
    closed: bool,
) {
    if !enabled() || out.is_null() {
        return;
    }
    if let Some(data) = super::data(unsafe { *out }) {
        let (allocator, pso) = (super::id(allocator), super::id(pso));
        let mut recording = model::Recording::new(data.id, kind, allocator, pso, closed);
        if !closed && let Some(real) = super::real(data.id) {
            data.instrument(&mut recording, windows::core::Interface::as_raw(&real), pso);
        }
        *data.recording.lock().unwrap_or_else(|e| e.into_inner()) = Some(recording);
        describe(
            data.id,
            device,
            json!({"type":kind,"allocator":allocator,"initial_pso":pso}),
        );
    }
}

/// Called while the queue lock is held, immediately before forwarding ExecuteCommandLists to `real`, the real queue.
/// Returns the checkpoints its instrumented lists copy, for [`submitted`].
pub unsafe fn submit(
    queue: &Data,
    real: *mut c_void,
    sequence: u64,
    count: u32,
    pointers: *const *mut c_void,
) -> super::passes::Submitted {
    if !enabled() {
        return Default::default();
    }
    let start = Instant::now();
    let mut s = state();
    s.cpu_seq += 1;
    s.next_submission += 1;
    let (cpu_seq, id) = (s.cpu_seq, s.next_submission);
    let armed = s.capture.is_some();
    let descriptors = if armed {
        let changes = super::slots::changes(&mut s.descriptors);
        for d in &changes {
            let range = (
                d.kind,
                d.description["buffer"]["offset"].as_u64(),
                d.description["bytes"].as_u64(),
            );
            match range {
                ("cb", Some(offset), Some(bytes)) => s.constant_buffers.insert(
                    (d.heap, d.slot),
                    (d.resource, offset, offset.saturating_add(bytes)),
                ),
                _ => s.constant_buffers.remove(&(d.heap, d.slot)),
            };
        }
        changes
    } else {
        Vec::new()
    };
    // Every submission's lists, armed or not: the state tracker follows them all.
    let mut lists = Vec::new();
    let mut ordinals = Vec::new();
    let mut diagnostics = Vec::new();
    for (ordinal, pointer) in unsafe { slice(pointers, count as usize) }.iter().enumerate() {
        if let Some(data) = super::data(*pointer) {
            ordinals.push(ordinal);
            lists.push(data.snapshot().unwrap_or_else(|| {
                Arc::new(model::List {
                    id: data.id,
                    ..model::List::default()
                })
            }));
        } else if armed {
            diagnostics.push(format!(
                "submission {id} queue {}: list {ordinal} of {count} has no registered identity",
                queue.id
            ));
        }
    }
    let queue_type = s
        .objects
        .get(&queue.id)
        .and_then(|o| o.description["type"].as_i64())
        .unwrap_or(0) as i32;
    let State {
        capture,
        objects,
        uploads,
        constant_buffers,
        states,
        waits,
        signals,
        ..
    } = &mut *s;
    if let Some(live) = capture.as_mut() {
        let start = Instant::now();
        let first = live.frame.submissions.is_empty();
        live.frame.diagnostics.extend(diagnostics);
        live.frame.submissions.push(model::Submission {
            id,
            queue: queue.id,
            queue_seq: sequence,
            cpu_seq,
            lists: lists.clone(),
            descriptors,
            // Filled from the GPU's copies when the capture is written.
            uploads: Vec::new(),
        });
        let submission = live.frame.submissions.last().expect("pushed");
        let touched = live.projection.submit(&mut live.capture, submission);
        let unknown = unsafe { unknown_wait(waits.entry(queue.id).or_default(), signals) };
        let snapshots = first_uses(live, objects, states, touched, unknown.as_deref(), cpu_seq);
        let copies = upload_copies(uploads, first, &lists, constant_buffers.values());
        if !copies.is_empty() || !snapshots.is_empty() {
            unsafe { super::readback::submit(real, id, copies, snapshots, &live.results) };
        }
        UPLOAD_COST.add(start, 0);
    }
    // The copies run before these lists, which change states for the next submission.
    states.execute(&lists, queue_type);
    let mut submitted = super::passes::Submitted::new(id);
    let mut executed = Vec::new();
    for (at, list) in ordinals.into_iter().zip(&lists).filter(|(_, l)| l.instrumented) {
        let copied = list.checkpoints.iter().filter(|c| c.gap.is_none()).map(|c| c.id);
        let copied = copied.chain(list.outputs.iter().filter(|o| o.gap.is_none()).map(|o| o.id));
        submitted.all.extend(copied.clone());
        let first = capture
            .as_mut()
            .is_some_and(|live| live.executed.insert((list.id, list.generation)));
        if first {
            submitted.read.extend(copied);
        }
        executed.push(super::passes::Executed { at, list, first });
    }
    let continued = unsafe { super::passes::ends(&mut submitted, &executed) };
    if let Some(live) = capture.as_ref() {
        if submitted.reads() {
            submitted.results = Some(live.results.clone());
        }
        if !continued.is_empty() {
            live.results
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(super::readback::Batch {
                    submission: id,
                    continued,
                    ..Default::default()
                });
        }
    }
    SUBMISSION_COST.add(start, 0);
    submitted
}

/// Called while the queue lock is held, immediately after forwarding ExecuteCommandLists to `real`, the real queue.
pub unsafe fn submitted(queue: &Data, real: *mut c_void, submission: super::passes::Submitted) {
    unsafe { super::passes::submitted(queue.id, real, submission) };
}

/// The first wait this queue has been told of that no submitted Signal and no completed fence value is known to
/// reach, as the reason its submission's first-use copies cannot be made: the GPU will run work submitted later
/// first, so the states and contents the copies would find are not yet known. Waits found reached are forgotten.
unsafe fn unknown_wait(waits: &mut Vec<(u64, u64)>, signals: &HashMap<u64, u64>) -> Option<String> {
    unsafe { prune(waits, signals) };
    waits.first().map(|(fence, value)| {
        format!(
            "the queue waits for fence {fence} to reach {value}, which nothing submitted yet signals: the GPU runs later work first"
        )
    })
}

/// Forgets the waits a submitted Signal or the fence's completed value reaches.
unsafe fn prune(waits: &mut Vec<(u64, u64)>, signals: &HashMap<u64, u64>) {
    use windows::Win32::Graphics::Direct3D12::ID3D12Fence;
    use windows::core::Interface;
    waits.retain(|&(fence, value)| {
        let signalled = signals.get(&fence).is_some_and(|&v| v >= value);
        let completed = super::real(fence)
            .and_then(|f| f.cast::<ID3D12Fence>().ok())
            .is_none_or(|f| unsafe { f.GetCompletedValue() } >= value);
        !(signalled || completed)
    });
}

/// The snapshots to copy before a submission's lists: each resource it touches that no earlier submission of the
/// frame used, unless its contents come another way (upload buffers, copied whole at the first submission;
/// readback buffers, which only the GPU writes) or cannot be copied, which is recorded with the reason.
fn first_uses(
    live: &mut Live,
    objects: &BTreeMap<u64, model::Object>,
    states: &super::states::Tracker,
    touched: Vec<u64>,
    unknown: Option<&str>,
    cpu_seq: u64,
) -> Vec<super::readback::Snapshot> {
    let submission = live.frame.submissions.last().map_or(0, |s| s.id);
    let mut snapshots = Vec::new();
    let mut failed = Vec::new();
    for resource in touched {
        if !live.used.insert(resource) {
            continue;
        }
        // Every resource an event names gets a copy or a reason: none is skipped unsaid.
        let d = match objects.get(&resource).map(|o| &o.description) {
            Some(d) if d["dimension"].is_u64() => d,
            found => {
                let why = if found.is_none() {
                    // A descriptor table can keep naming a resource destroyed since its view was written; a shader
                    // that read it would be reading freed memory, so this is most likely a slot it never reads.
                    "destroyed before the submission that names it (a stale descriptor)"
                } else {
                    "no description: locum did not see its creation"
                };
                failed.push((
                    resource,
                    model::FirstUse {
                        submission,
                        states: json!([]),
                        content: Err(why.to_string()),
                    },
                ));
                continue;
            }
        };
        let n = |key: &str| d[key].as_u64().unwrap_or(0) as u32;
        // Upload and readback heaps.
        if matches!(d["heap_properties"]["type"].as_u64(), Some(2 | 3)) {
            continue;
        }
        let texture = n("dimension") > 1;
        let simultaneous = n("flags") & 0x20 != 0;
        let format = n("format");
        let layout = model::layout(d);
        let reserved = d["heap_properties"].is_null();
        if reserved {
            page(live, resource, cpu_seq);
        }
        let why = if let Some(why) = unknown {
            Some(why.to_string())
        } else if let Some(why) = reserved.then(|| super::tiles::unreadable(resource)).flatten() {
            Some(why)
        } else if n("sample_count") > 1 {
            Some("multisampled: copying samples out is not supported yet".to_string())
        } else if layout.is_none() {
            Some(format!("format {format} has no canonical layout"))
        } else if texture && !simultaneous && !states.tracked(resource) {
            Some("its state is unknown: no creation locum saw".to_string())
        } else {
            None
        };
        if let Some(why) = why {
            failed.push((
                resource,
                model::FirstUse {
                    submission,
                    states: json!([]),
                    content: Err(why),
                },
            ));
            continue;
        }
        let states = states.states(resource).map(<[_]>::to_vec).unwrap_or_default();
        let planes = if locum_capture::format::is_depth_stencil(format) {
            2
        } else {
            1
        };
        let layout = layout.expect("checked");
        snapshots.push(super::readback::Snapshot {
            resource,
            subresources: if texture { layout.len() as u32 * planes } else { 1 },
            joined: (planes == 2).then(|| locum_capture::format::layout(format).map_or(4, |f| f.block_bytes)),
            states,
            layout,
        });
    }
    if !failed.is_empty() {
        live.results
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(super::readback::Batch {
                submission,
                contents: failed,
                ..Default::default()
            });
    }
    snapshots
}

/// D3D12's largest constant buffer view: 4096 constants of 16 bytes.
const CONSTANT_BUFFER_BYTES: u64 = 65536;

/// The upload bytes an armed submission's work reads: every upload buffer whole at the frame's first submission
/// (marked `whole`, the contents its resources began the frame with), then at each submission only the
/// constant-buffer ranges its lists' root CBVs and the descriptor heaps' CBVs name, and the ranges its buffer copies
/// read (a constant buffer in a default heap is filled that way), merged into runs per buffer.
/// The GPU copies them (see [`super::readback`]). Runs closer than [`CONSTANT_BUFFER_BYTES`] are copied as one:
/// Elden Ring's 128k CBVs left 23.7k runs of ~540 bytes per submission, which merge into about three.
fn upload_copies<'a>(
    uploads: &BTreeMap<u64, Upload>,
    first: bool,
    lists: &[Arc<model::List>],
    constant_buffers: impl Iterator<Item = &'a (u64, u64, u64)>,
) -> Vec<super::readback::Copy> {
    let mut out = Vec::new();
    if first {
        for (&resource, u) in uploads {
            out.push(super::readback::Copy {
                resource,
                offset: 0,
                len: u.bytes as u64,
                whole: true,
            });
        }
    }
    let mut ranges = Vec::new();
    for list in lists {
        read_ranges(list, uploads, &mut ranges);
    }
    ranges.extend(constant_buffers.filter(|(resource, _, _)| uploads.contains_key(resource)));
    for (resource, begin, end) in runs(ranges, CONSTANT_BUFFER_BYTES) {
        let end = end.min(uploads[&resource].bytes as u64);
        if begin < end {
            out.push(super::readback::Copy {
                resource,
                offset: begin,
                len: end - begin,
                whole: false,
            });
        }
    }
    out
}

/// The upload bytes this list and the bundles it executes read, as (resource, begin, end): root constant-buffer
/// views at the largest size a constant buffer can have (the bytes a shader reads are not known until reflection), and
/// the sources of buffer copies.
fn read_ranges(list: &model::List, uploads: &BTreeMap<u64, Upload>, ranges: &mut Vec<(u64, u64, u64)>) {
    for c in &list.commands {
        if let Some(bundle) = &c.bundle {
            read_ranges(bundle, uploads, ranges);
        }
        let upload = |v: &Value| v.as_u64().filter(|r| uploads.contains_key(r));
        match c.call {
            "CopyBufferRegion" => {
                if let (Some(resource), Some(offset), Some(bytes)) =
                    (upload(&c.args[2]), c.args[3].as_u64(), c.args[4].as_u64())
                {
                    ranges.push((resource, offset, offset.saturating_add(bytes)));
                }
            }
            "CopyResource" => {
                if let Some(resource) = upload(&c.args[1]) {
                    ranges.push((resource, 0, uploads[&resource].bytes as u64));
                }
            }
            _ => {}
        }
        if matches!(
            c.call,
            "SetGraphicsRootConstantBufferView" | "SetComputeRootConstantBufferView"
        ) {
            let at = &c.args[1];
            if let (Some(resource), Some(offset)) = (at["resource"].as_u64(), at["offset"].as_u64())
                && uploads.contains_key(&resource)
            {
                ranges.push((resource, offset, offset.saturating_add(CONSTANT_BUFFER_BYTES)));
            }
        }
    }
}

/// Ranges of the same resource, merged where they overlap or are fewer than `gap` bytes apart: one copy of the bytes
/// between costs less than another copy's overhead.
fn runs(mut ranges: Vec<(u64, u64, u64)>, gap: u64) -> Vec<(u64, u64, u64)> {
    ranges.sort_unstable();
    let mut merged: Vec<(u64, u64, u64)> = Vec::new();
    for (resource, begin, end) in ranges {
        match merged.last_mut() {
            Some(last) if last.0 == resource && begin <= last.2.saturating_add(gap) => {
                last.2 = last.2.max(end)
            }
            _ => merged.push((resource, begin, end)),
        }
    }
    merged
}

/// Records reserved resource `resource`'s page table as the armed frame finds it, once per frame.
fn page(live: &mut Live, resource: u64, cpu_seq: u64) {
    if resource != 0 && live.paged.insert(resource) {
        live.frame.tile_pages.push(model::TilePages {
            resource,
            cpu_seq,
            pages: super::tiles::pages(resource),
        });
    }
}

/// Before a change to the tile mappings of `resources`: an armed frame records their page tables as it found them.
pub fn tiles_changing(resources: &[u64]) {
    let mut s = state();
    let cpu_seq = s.cpu_seq;
    if let Some(live) = s.capture.as_mut() {
        for &resource in resources {
            page(live, resource, cpu_seq);
        }
    }
}

/// A change to tile mappings, the `queue_seq`th operation of queue `queue`: an armed frame records it.
pub fn tile_mapping(
    queue: u64,
    queue_seq: u64,
    call: &'static str,
    resource: u64,
    source: Option<u64>,
    args: Value,
    after: Result<Value, String>,
) {
    let mut s = state();
    s.cpu_seq += 1;
    let cpu_seq = s.cpu_seq;
    if let Some(live) = s.capture.as_mut() {
        live.frame.tile_mappings.push(model::TileMapping {
            queue,
            queue_seq,
            cpu_seq,
            call,
            resource,
            source,
            args,
            after,
        });
    }
}

pub fn sync(queue: &Data, sequence: u64, kind: &'static str, fence: *mut c_void, value: u64, hr: HRESULT) {
    if !enabled() || hr.is_err() {
        return;
    }
    let fence = super::id(fence);
    let mut s = state();
    s.cpu_seq += 1;
    let cpu_seq = s.cpu_seq;
    match kind {
        "Signal" => {
            let v = s.signals.entry(fence).or_default();
            *v = (*v).max(value);
            for waits in s.waits.values_mut() {
                waits.retain(|&(f, v)| f != fence || v > value);
            }
        }
        "Wait" => {
            let State { waits, signals, .. } = &mut *s;
            let waits = waits.entry(queue.id).or_default();
            waits.push((fence, value));
            // Fences the CPU signals are only seen reached by asking them.
            if waits.len() > 64 {
                unsafe { prune(waits, signals) };
            }
        }
        _ => {}
    }
    if let Some(live) = s.capture.as_mut() {
        live.frame.sync.push(model::Sync {
            queue: queue.id,
            queue_seq: sequence,
            cpu_seq,
            kind,
            fence,
            value,
        });
    }
}

/// No slices are constructed from null, including the API's conventional (null, 0).
pub unsafe fn slice<'a, T>(pointer: *const T, len: usize) -> &'a [T] {
    if pointer.is_null() || len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(pointer, len) }
    }
}

/// At each Present; `presented` is the back buffer it shows, looked up while a capture is armed.
pub fn present(frame_number: u64, presented: Option<u64>) {
    static REPORT: OnceLock<Mutex<Instant>> = OnceLock::new();
    let mut last = REPORT
        .get_or_init(|| Mutex::new(Instant::now()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if last.elapsed().as_secs() >= 10 {
        *last = Instant::now();
        super::log_counts();
        let [
            recording,
            submission,
            address,
            descriptor,
            destruction,
            upload,
            reset,
        ] = super::cost::take();
        log!(
            "D3D12 journal CPU: recording {}, submission {}",
            recording,
            submission
        );
        log!(
            "D3D12 journal detail: VA {}, descriptor {}, destruction {}, upload {}, reset {}; frame {frame_number}",
            address,
            descriptor,
            destruction,
            upload,
            reset
        );
        let s = state();
        let (ranges, heaps, slots) = super::slots::counts();
        log!(
            "D3D12 journal live: objects {}, buffer ranges {}, descriptor heaps {}, descriptor slots {}, upload buffers {}",
            s.objects.len(),
            ranges,
            heaps,
            slots,
            s.uploads.len()
        );
        log!(
            "D3D12 states: {} textures tracked, {} transitions, {} not from the tracked state",
            s.states.count(),
            s.states.transitions,
            s.states.mismatches
        );
    }
    drop(last);
    if !enabled() {
        return;
    }
    let Some(root) = folder() else {
        return;
    };
    super::passes::sweep();
    let (finished, series) = {
        let mut s = state();
        (s.capture.take(), s.series.take())
    };
    if finished.is_some() {
        super::passes::ARMED.store(false, std::sync::atomic::Ordering::Release);
        log!(
            "D3D12 capture closing: {} copies recorded so far",
            super::passes::recorded()
        );
    }
    if let Some(Live {
        path,
        frame,
        projection,
        mut capture,
        results,
        series: of,
        ..
    }) = finished
    {
        super::log_counts();
        capture.meta("api", "d3d12");
        if let Some(of) = &of {
            capture.meta(
                "series",
                serde_json::json!({"label": of.label, "index": of.index, "frames": of.frames}).to_string(),
            );
        }
        capture.meta("name", path.file_name().unwrap_or_default().to_string_lossy());
        capture.meta("locum", env!("CARGO_PKG_VERSION"));
        capture.meta("frame", frame_number);
        capture.meta("pid", std::process::id());
        if let Some(id) = presented {
            capture.meta("presented", id);
        }
        capture.meta(
            "exe",
            std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        capture.meta("blobs", crate::settings::settings().blobs.root().display());
        capture.meta("capture_status", "incomplete");
        capture.meta(
            "verification_scope",
            "command history, first-use contents, pass-end targets and what draws, dispatches and clears wrote through UAVs",
        );
        let mut prepared = frame.finish(&mut capture, projection);
        let t = std::time::Instant::now();
        // The copies finish on the GPU soon after the frame and the collector reads them in order: the capture is
        // written after the last, off the game's threads. The same "capture written" / "capture NOT written" lines as
        // Direct3D 11's: runners wait for them.
        super::readback::then(move || {
            writer::run(move || {
                for batch in std::mem::take(&mut *results.lock().unwrap_or_else(|e| e.into_inner())) {
                    prepared.add_uploads(batch.submission, batch.uploads);
                    for (resource, first) in batch.contents {
                        prepared.set_first_use(resource, first);
                    }
                    for (id, content) in batch.checkpoints {
                        prepared.set_checkpoint(id, content);
                    }
                    for id in batch.continued {
                        prepared.set_continued(id);
                    }
                }
                prepared.contents(&mut capture, &crate::settings::settings().blobs);
                capture.d3d12 = Some(prepared);
                let file = path.join("capture.db");
                let result = std::fs::create_dir_all(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|()| db::write(&file, &capture).map_err(|e| e.to_string()));
                match result {
                    Ok(()) => log!(
                        "capture written: {} ({} events, {} draws, {} gaps; {:.1} s after the frame)",
                        file.display(),
                        capture.events.len(),
                        capture.draws.len(),
                        capture.gaps.len(),
                        t.elapsed().as_secs_f64()
                    ),
                    Err(e) => log!("capture NOT written to {}: {e}", file.display()),
                }
            })
        });
    }
    // The next frame of a series is armed at the present that closed the one before it: nothing comes between them.
    if let Some(series) = series.filter(|s| s.index < s.frames) {
        arm(
            &root,
            frame_number,
            Series {
                index: series.index + 1,
                ..series
            },
        );
        return;
    }
    let request = root.join("capture");
    if let Ok(text) = std::fs::read_to_string(&request) {
        if let Err(e) = std::fs::remove_file(&request) {
            log!("D3D12 capture request: {e}");
            return;
        }
        let label = text.lines().find_map(|l| l.strip_prefix("label ")).unwrap_or("");
        let label: String = label
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c))
            .collect();
        let frames = match text.lines().find_map(|l| l.strip_prefix("frames ")) {
            None => 1,
            Some(n) => match n.trim().parse::<u32>() {
                Ok(n @ 1..=MAX_FRAMES) => n,
                _ => {
                    log!("D3D12 capture request: frames {n:?} is not 1 to {MAX_FRAMES}; one frame");
                    1
                }
            },
        };
        arm(
            &root,
            frame_number,
            Series {
                label: (!label.is_empty()).then_some(label),
                index: 1,
                frames,
            },
        );
    }
}

/// Arms the capture of the frame after Present `frame_number`, frame `series.index` of its series: written to
/// `captures/<label>` (one frame), `captures/<label>-<index>` (a series) or `captures/frame-<n>` (no label).
fn arm(root: &std::path::Path, frame_number: u64, series: Series) {
    let name = match &series.label {
        Some(label) if series.frames > 1 => format!("{label}-{}", series.index),
        Some(label) => label.clone(),
        None => format!("frame-{}", frame_number + 1),
    };
    let path = root.join("captures").join(name);
    if path.exists() {
        log!("D3D12 capture destination already exists: {}", path.display());
        return;
    }
    let mut s = state();
    s.descriptors.clear();
    s.constant_buffers.clear();
    let objects: Vec<_> = s.objects.values().cloned().collect();
    s.capture = Some(Live {
        series: (series.frames > 1).then(|| series.clone()),
        path,
        projection: model::Projection::new(&objects),
        frame: model::Frame {
            objects,
            ..model::Frame::default()
        },
        capture: db::Capture::default(),
        results: Default::default(),
        used: HashSet::new(),
        executed: HashSet::new(),
        paged: HashSet::new(),
    });
    s.series = Some(series.clone());
    super::passes::ARMED.store(true, std::sync::atomic::Ordering::Release);
    log!(
        "D3D12 capture armed at Present {frame_number} (frame {} of {}); cached list history retained; {} copies recorded so far",
        series.index,
        series.frames,
        super::passes::recorded()
    );
}

#[cfg(test)]
mod tests {
    use super::runs;
    #[test]
    fn ranges_merge_across_gaps_smaller_than_the_threshold_and_never_across_resources() {
        let ranges = vec![
            (7, 1000, 1256),
            (7, 0, 256),
            (7, 100, 300),
            (7, 5000, 5100),
            (8, 300, 400),
        ];
        assert_eq!(
            runs(ranges.clone(), 0),
            vec![(7, 0, 300), (7, 1000, 1256), (7, 5000, 5100), (8, 300, 400)]
        );
        assert_eq!(
            runs(ranges.clone(), 700),
            vec![(7, 0, 1256), (7, 5000, 5100), (8, 300, 400)]
        );
        assert_eq!(runs(ranges, 1 << 20), vec![(7, 0, 5100), (8, 300, 400)]);
    }
}
