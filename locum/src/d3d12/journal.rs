//! Continuous list history and per-submission capture. No COM ownership is kept in
//! the portable history; object IDs survive pointer and descriptor-slot reuse.
use core::ffi::c_void;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::cost::Counter;
use locum_capture::{d3d12 as model, db, environment, writer};
use serde_json::{Value, json};
use windows::Win32::Graphics::Direct3D12::{
    D3D12_COMMAND_LIST_TYPE, ID3D12CommandQueue, ID3D12Device, ID3D12GraphicsCommandList,
};
use windows::core::{HRESULT, Interface};

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
            if call == "ResourceBarrier" {
                unsafe { super::promoted::settle(recording, real, &args) };
            }
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
        self.keep(recording, checkpoints, pass);
    }
    /// Assigns copies made in the list (`checkpoints`, and what `pass` copied since last asked) to its generation.
    fn keep(
        &self,
        recording: &mut model::Recording,
        checkpoints: Vec<model::Checkpoint>,
        pass: &mut super::passes::Pass,
    ) {
        let (outputs, arguments) = (pass.outputs(), pass.arguments());
        for resource in pass.promoted() {
            recording.promote(resource);
        }
        super::passes::belong(
            self.id,
            checkpoints
                .iter()
                .map(|c| c.id)
                .chain(outputs.iter().map(|o| o.id))
                .chain(arguments.iter().map(|a| a.id)),
        );
        for c in checkpoints {
            recording.checkpoint(c);
        }
        for o in outputs {
            recording.output(o);
        }
        for a in arguments {
            recording.arguments(a);
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
            self.keep(recording, checkpoints, &mut pass);
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
        super::rays::retire(self.id);
        if hr.is_ok()
            && let Some(r) = self.recording.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            r.reset(allocator, pso);
            if let Some(list) = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&real) } {
                self.instrument(r, list, pso);
            }
        }
        RESET_COST.add(start, 0);
    }
    /// Instruments a generation opened with pipeline `pso` while a capture is armed, if the list is a direct one.
    fn instrument(&self, recording: &mut model::Recording, list: &ID3D12GraphicsCommandList, pso: u64) {
        let pass = if super::passes::instrumenting() {
            unsafe { super::passes::Pass::new(list, pso) }
        } else {
            None
        };
        if pass.is_some() {
            recording.instrument();
        }
        *self.pass.lock().unwrap_or_else(|e| e.into_inner()) = pass;
    }
}

/// Last known queue signal that produced a fence value.
#[derive(Clone, Copy, Debug)]
struct ProducerSignal {
    queue: u64,
    submission: Option<u64>,
    cpu_seq: u64,
    value: u64,
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
    /// The armed frame's buffer SRV/UAV views pointing into upload buffers: (heap, slot) to (resource, begin, end).
    upload_views: BTreeMap<(u64, u32), (u64, u64, u64)>,
    /// Every texture's state as the next submission will find it.
    states: super::states::Tracker,
    /// The series the armed frame belongs to (`frames N` in the request): the next frame is armed as it closes.
    series: Option<Series>,
    /// A request in its lead (`early N`): the Presents left before its first frame is armed, and its series.
    lead: Option<(u32, Series)>,
    /// Fence values each queue has been told to wait for and no submitted Signal is known to reach, by queue.
    waits: HashMap<u64, Vec<(u64, u64)>>,
    /// The captures armed so far: each armed frame's number.
    armings: u64,
    /// The armed frame's submissions between [`submit`] and [`submitted`], which let the state go while they make
    /// their copies: the Present that closes the capture waits for them.
    submitting: u32,
    /// The highest value submitted to signal each fence, by fence.
    signals: HashMap<u64, u64>,
    /// Last producer queue/submission known to signal each fence.
    producers: HashMap<u64, ProducerSignal>,
    /// Most recent submission id on each queue.
    last_submission_on_queue: HashMap<u64, u64>,
}

/// Consecutive frames one request asked for, each a capture of its own.
#[derive(Clone, Debug)]
struct Series {
    /// The request's label: the captures are `<label>-1` to `<label>-N`; `frame-<n>` each without one.
    label: Option<String>,
    /// The armed frame's place in the series, from 1.
    index: u32,
    frames: u32,
    /// Per-request `complete-pass` selectors merged with `settings.txt`.
    complete_passes: Vec<String>,
    /// Per-request `indirect-args` override.
    indirect_args: Option<bool>,
}

/// The most frames one request captures.
const MAX_FRAMES: u32 = 64;
/// The most Presents a request's lead (`early N`) runs before its first frame is armed.
const MAX_EARLY: u32 = 64;
/// How long the Present that closes a capture waits for its submissions still making their copies.
const SUBMITTING: Duration = Duration::from_secs(10);

/// An armed frame: its history so far, projected as it is submitted, and the copies its submissions made.
struct Live {
    /// Which capture of the process's it is (see [`State::armings`]).
    number: u64,
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
    /// Every acceleration structure's latest build as the frame began (see [`super::rays`]).
    rays: Vec<(super::rays::Key, Arc<super::rays::Build>)>,
    /// What the frame's first-use copies take, and those given up for room.
    room: Room,
    /// Active selected-pass completeness filters for this capture.
    selection: super::select::Selection,
    /// Reserved buffers whose tile mappings changed in-frame since their last resident-tile snapshot.
    remapped_since_copy: HashSet<u64>,
}
/// What a capture leaves free on the blob store's drive: its first-use copies stop short of it.
const DISK_KEPT: u64 = 8 << 30;
/// Room for a frame's first-use copies: a resource only a directly indexed descriptor heap names within
/// `heap-indexed-mb` (`heap`, None for `heap-indexed off`), and every copy within the bytes free on the blob store's
/// drive when the capture was armed (`disk`, None if it could not be asked), less [`DISK_KEPT`].
#[derive(Default)]
struct Room {
    heap: Option<u64>,
    disk: Option<u64>,
    /// The bytes planned so far, and those of them for resources only such a heap names.
    planned: u64,
    heap_bytes: u64,
    /// The copies given up, by what ran out (`heap-indexed off`, `heap-indexed-mb`, `disk`): how many, and their bytes.
    uncopied: BTreeMap<&'static str, (u64, u64)>,
}
impl Room {
    /// Takes room for a copy of `bytes` (of a resource only a directly indexed heap names, if `heap_only`), or says
    /// why there is none.
    fn take(&mut self, bytes: u64, heap_only: bool) -> Result<(), String> {
        let why = match (heap_only, self.heap, self.disk) {
            (true, None, _) => Err(("heap-indexed off", "`heap-indexed off`".to_string())),
            (true, Some(cap), _) if self.heap_bytes + bytes > cap => Err((
                "heap-indexed-mb",
                format!(
                    "past `heap-indexed-mb` {}: {} MiB of the frame's resources only such a heap names came first",
                    cap >> 20,
                    self.heap_bytes >> 20
                ),
            )),
            (_, _, Some(free)) if self.planned + bytes + DISK_KEPT > free => Err((
                "disk",
                format!(
                    "disk: the blob store's drive had {} MiB free when the capture was armed, the frame's copies \
                     before it take {} MiB, and {} MiB are kept free",
                    free >> 20,
                    self.planned >> 20,
                    DISK_KEPT >> 20
                ),
            )),
            _ => Ok(()),
        };
        match why {
            Ok(()) => {
                self.planned += bytes;
                if heap_only {
                    self.heap_bytes += bytes;
                }
                Ok(())
            }
            Err((what, why)) => {
                let lost = self.uncopied.entry(what).or_default();
                *lost = (lost.0 + 1, lost.1 + bytes);
                Err(why)
            }
        }
    }
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
/// Changes object `id`'s description in place with `change`, which says whether it changed anything: what the
/// object reports after its creation (a state object's shader identifiers).
pub fn amend(id: u64, change: impl FnOnce(&mut Value) -> bool) {
    let mut s = state();
    let Some(object) = s.objects.get_mut(&id) else {
        return;
    };
    if !change(&mut object.description) {
        return;
    }
    let object = object.clone();
    if let Some(live) = s.capture.as_mut() {
        live.projection.describe(id, &object.description);
        live.frame.objects.retain(|o| o.id != id);
        live.frame.objects.push(object);
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
    super::rays::retire(id);
    super::rays::destroyed(id);
    {
        let mut s = state();
        s.objects.remove(&id);
        s.uploads.remove(&id);
        s.states.destroy(id);
    }
    super::tiles::destroyed(id);
    super::slots::destroy(id);
    super::readback::destroyed(id);
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
        super::descriptors::Description::Json(json!({"buffer":buffer,"address":address,"bytes":bytes}))
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
        if !closed && let Some(list) = super::real::<ID3D12GraphicsCommandList>(data.id) {
            data.instrument(&mut recording, &list, pso);
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
            if let Some((begin, end)) = buffer_view_range(d) {
                s.upload_views.insert((d.heap, d.slot), (d.resource, begin, end));
            } else {
                s.upload_views.remove(&(d.heap, d.slot));
            }
        }
        changes
    } else {
        Vec::new()
    };
    s.last_submission_on_queue.insert(queue.id, id);
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
    // The armed frame this submission belongs to. The state is let go while locum asks the real device for something
    // (see below) and taken again after: it is still this submission's frame if the number is the same.
    let number = s.capture.as_ref().map(|live| live.number);
    // The first-use and upload copies to make before these lists, and where their results go.
    let mut before = None;
    let upload_start = Instant::now();
    if armed {
        s.submitting += 1;
        let State {
            capture,
            objects,
            waits,
            signals,
            producers,
            ..
        } = &mut *s;
        let live = capture.as_mut().expect("armed");
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
        let from = live.projection.indirect().len();
        let draws_before = live.capture.draws.len() as u64;
        let mut touched = live.projection.submit(&mut live.capture, submission);
        let draws_after = live.capture.draws.len() as u64;
        // What its programs may read through a heap they index directly, besides what its events name.
        let mut heap = HashSet::new();
        let named = live.projection.heap_indexed();
        if !named.is_empty() {
            let events: HashSet<u64> = touched.iter().copied().collect();
            for r in named.into_iter().filter(|r| !events.contains(r)) {
                heap.insert(r);
                touched.push(r);
            }
        }
        // Any resource explicitly pinned by `complete-pass resource=<id>` is pulled in on the first submission while
        // live so it is never missed when referenced only indirectly.
        for pinned in live.selection.explicit_resources() {
            if !live.used.contains(&pinned) && objects.contains_key(&pinned) && !touched.contains(&pinned) {
                touched.push(pinned);
            }
        }
        let draw_range = (draws_after > draws_before).then_some((draws_before + 1, draws_after));
        let selected_reason = live
            .selection
            .matches_submission(&lists, objects, &touched, draw_range, None);
        let selected = selected_reason.is_some();
        if let Some(reason) = selected_reason {
            let bindless_heaps = super::select::bound_descriptor_heaps(&lists);
            let (bindless_max_version, _) = super::slots::heap_versions(&bindless_heaps);
            live.frame.selected_passes.push(model::SelectedPass {
                submission: id,
                queue: queue.id,
                cpu_seq,
                reason,
                bindless_heaps: json!(bindless_heaps),
                bindless_max_version,
                resources: json!(touched),
            });
        }
        // Snapshot producer queue/fence wait state before pruning so both satisfied and blocking waits are recorded.
        let queue_waits = waits.entry(queue.id).or_default();
        for &(fence, wait_value) in queue_waits.iter() {
            let completed_value = super::real::<windows::Win32::Graphics::Direct3D12::ID3D12Fence>(fence)
                .map(|f| unsafe { f.GetCompletedValue() });
            let signalled = signals.get(&fence).is_some_and(|&v| v >= wait_value);
            let satisfied = signalled || completed_value.is_some_and(|c| c >= wait_value);
            let prod = producers.get(&fence).copied();
            live.frame.submission_syncs.push(model::SubmissionSync {
                submission: id,
                queue: queue.id,
                cpu_seq,
                fence,
                wait_value,
                completed_value,
                producer_queue: prod.map(|p| p.queue),
                producer_submission: prod.and_then(|p| p.submission),
                producer_value: prod.map(|p| p.value),
                satisfied,
            });
        }
        let unknown = unsafe { unknown_wait(queue_waits, signals) };
        // What its DispatchRays' shader tables name, read now: the queue runs everything submitted before first.
        let real_queue = unsafe { ID3D12CommandQueue::from_raw_borrowed(&real) };
        let mut reads = match (&unknown, real_queue) {
            (None, Some(_)) => super::indirect::at_submission(&live.projection, &live.capture.events, from),
            _ => Vec::new(),
        };
        // Reading the tables records and runs a list on the real device, and a reserved resource's page table is
        // asked of it: whose methods a translation layer may have taken over, and one that holds a lock of its own
        // while it calls a wrapped object, which takes this state, would wait for this thread while this thread waits
        // for it. So they are made without the state, and the tables read name more resources (reserved ones, too).
        let mut tiles = HashMap::new();
        loop {
            let pages: Vec<u64> = match &s.capture {
                Some(live) if Some(live.number) == number => touched
                    .iter()
                    .copied()
                    .filter(|r| {
                        !tiles.contains_key(r)
                            && (!live.used.contains(r) || (selected && live.remapped_since_copy.contains(r)))
                    })
                    .filter(|r| {
                        s.objects.get(r).is_some_and(|o| {
                            o.description["dimension"].is_u64() && o.description["heap_properties"].is_null()
                        })
                    })
                    .collect(),
                _ => break,
            };
            if reads.is_empty() && pages.is_empty() {
                break;
            }
            drop(s);
            if let Some(queue) = real_queue {
                unsafe { super::indirect::read_at_submission(&mut reads, queue) };
            }
            for resource in pages {
                let (pages, mapped_buffer_tiles) = super::tiles::pages_and_mapped_buffer_tiles(resource);
                tiles.insert(
                    resource,
                    Tiles {
                        pages,
                        unreadable: super::tiles::unreadable(resource),
                        mapped_buffer_tiles,
                    },
                );
            }
            s = state();
            if let Some(live) = s.capture.as_ref().filter(|live| Some(live.number) == number) {
                let named =
                    super::indirect::named_at_submission(&live.projection, std::mem::take(&mut reads));
                for r in named {
                    if !touched.contains(&r) {
                        touched.push(r);
                    }
                }
            }
        }
        let State {
            capture,
            objects,
            uploads,
            constant_buffers,
            upload_views,
            states,
            ..
        } = &mut *s;
        if let Some(live) = capture.as_mut().filter(|live| Some(live.number) == number) {
            let snapshots = first_uses(
                live,
                id,
                objects,
                states,
                touched,
                &heap,
                unknown.as_deref(),
                cpu_seq,
                &tiles,
                selected,
            );
            let copies = upload_copies(
                uploads,
                first,
                &lists,
                constant_buffers.values().chain(upload_views.values()),
                selected,
                &live.selection,
            );
            if !copies.is_empty() || !snapshots.is_empty() {
                before = Some((copies, snapshots, live.results.clone()));
            }
        }
    }
    let State { capture, states, .. } = &mut *s;
    // A capture closed while the state was let go (after waiting long enough for this submission) is not this one's.
    let mut other = None;
    let capture = if capture.as_ref().map(|live| live.number) == number {
        capture
    } else {
        &mut other
    };
    // The copies run before these lists, which change states for the next submission.
    states.execute(&lists, queue_type);
    let mut submitted = super::passes::Submitted::new(id, D3D12_COMMAND_LIST_TYPE(queue_type));
    submitted.in_flight = armed;
    submitted.rays = super::rays::executing(&lists.iter().map(|l| l.id).collect::<Vec<_>>());
    let mut executed = Vec::new();
    let ordinals_of = ordinals.clone();
    for (at, list) in ordinals.into_iter().zip(&lists).filter(|(_, l)| l.instrumented) {
        let copied = list.checkpoints.iter().filter(|c| c.gap.is_none()).map(|c| c.id);
        let copied = copied.chain(list.outputs.iter().filter(|o| o.gap.is_none()).map(|o| o.id));
        submitted.all.extend(copied.clone());
        if capture.is_some() {
            let arguments = list.arguments.iter().filter(|a| a.gap.is_none()).map(|a| a.id);
            submitted.all.extend(arguments.clone());
            submitted.arguments.extend(arguments);
        }
        let first = capture
            .as_mut()
            .is_some_and(|live| live.executed.insert((list.id, list.generation)));
        if first {
            submitted.read.extend(copied);
        }
        executed.push(super::passes::Executed {
            at,
            list,
            armed: capture.is_some(),
            first,
        });
    }
    let results = capture.as_ref().map(|live| live.results.clone());
    // Everything from here on records locum's own lists on the real device, or runs them on the real queue: without
    // the state, as above. The Present that closes the capture waits for this submission (see [`present`]).
    drop(s);
    if let Some((copies, snapshots, results)) = before {
        unsafe { super::readback::submit(queue.id, real, id, copies, snapshots, &results) };
    }
    if armed {
        UPLOAD_COST.add(upload_start, 0);
    }
    let continued = unsafe { super::passes::ends(&mut submitted, &executed) };
    // Buffers locum's copies promoted, moved between the lists (see [`super::promoted`]).
    let placed: Vec<(usize, &model::List)> = ordinals_of
        .iter()
        .copied()
        .zip(lists.iter().map(|l| &**l))
        .collect();
    // Instances given as arrays of pointers, gathered right after their lists (see [`super::gather`]), before
    // anything locum moves before the next list.
    let gathers = super::rays::gathers(&placed);
    let moves = super::promoted::between(&placed);
    if !gathers.is_empty() || !moves.is_empty() {
        let mut device: Option<ID3D12Device> = None;
        let made = unsafe { ID3D12CommandQueue::from_raw_borrowed(&real) }
            .ok_or_else(|| "no queue".to_string())
            .and_then(|q| unsafe { q.GetDevice(&mut device) }.map_err(|e| e.to_string()))
            .and_then(|()| device.ok_or_else(|| "no device".to_string()));
        for (at, builds) in gathers {
            let gathered = made
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|d| unsafe { submitted.gather_after(d, at, &builds) });
            if let Err(why) = &gathered {
                crate::log::log!(
                    "D3D12 acceleration structures: gather after list {at} of submission {id}: {why}"
                );
            }
            super::rays::gathered_at_submission(&builds, gathered.err().as_deref());
        }
        for (at, m) in moves {
            if let Err(why) = made
                .as_ref()
                .map_err(Clone::clone)
                .and_then(|d| unsafe { submitted.moves_before(d, at, &m) })
            {
                crate::log::log!(
                    "D3D12: submission {id}: the buffers locum's copies promoted are not moved back before list {at}: {why}"
                );
            }
        }
    }
    if let Some(results) = results {
        if submitted.reads() {
            submitted.results = Some(results.clone());
        }
        if !continued.is_empty() {
            results
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
pub unsafe fn submitted(queue: &Data, real: *mut c_void, mut submission: super::passes::Submitted) {
    let builds = std::mem::take(&mut submission.rays);
    let in_flight = submission.in_flight;
    unsafe { super::passes::submitted(queue.id, real, submission) };
    unsafe { super::rays::submitted(queue.id, real, builds) };
    if in_flight {
        let mut s = state();
        s.submitting = s.submitting.saturating_sub(1);
    }
}

/// What a reserved resource's page table holds, and why it cannot be copied if it cannot, asked of the real device
/// before a submission's first-use copies are decided (see [`submit`]).
struct Tiles {
    pages: Result<(Value, Value), String>,
    unreadable: Option<String>,
    /// Mapped 64 KiB tiles of a reserved buffer, captured under the same lock as `pages`.
    mapped_buffer_tiles: Option<Vec<u32>>,
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
    waits.retain(|&(fence, value)| {
        let signalled = signals.get(&fence).is_some_and(|&v| v >= value);
        let completed =
            super::real::<ID3D12Fence>(fence).is_none_or(|f| unsafe { f.GetCompletedValue() } >= value);
        !(signalled || completed)
    });
}

/// The snapshots to copy before a submission's lists: each resource it touches that no earlier submission of the
/// frame used, unless its contents come another way (upload buffers, copied whole at the first submission;
/// readback buffers, which only the GPU writes) or cannot be copied, which is recorded with the reason.
#[allow(clippy::too_many_arguments)]
fn first_uses(
    live: &mut Live,
    submission: u64,
    objects: &BTreeMap<u64, model::Object>,
    states: &super::states::Tracker,
    touched: Vec<u64>,
    heap: &HashSet<u64>,
    unknown: Option<&str>,
    cpu_seq: u64,
    tiles: &HashMap<u64, Tiles>,
    selected: bool,
) -> Vec<super::readback::Snapshot> {
    let mut snapshots = Vec::new();
    let mut failed = Vec::new();
    for resource in touched {
        let pinned = selected || live.selection.pins_resource(resource);
        let first_time = live.used.insert(resource);
        if !first_time {
            // For a selected pass, if a reserved buffer's tile mappings changed since its last copy, capture a fresh
            // resident-tile snapshot of its current mapped tiles without dropping for budget.
            if pinned
                && live.remapped_since_copy.remove(&resource)
                && let Some(d) = objects.get(&resource).map(|o| &o.description)
                && d["dimension"] == 1
                && d["heap_properties"].is_null()
                && let Some(layout) = model::layout(d)
                && let Some(tile) = tiles.get(&resource)
                && tile.unreadable.is_none()
            {
                let pages_json = tile.pages.as_ref().ok().map(|(_, p)| p.to_string());
                snapshots.push(super::readback::Snapshot {
                    resource,
                    subresources: 1,
                    joined: None,
                    states: Vec::new(),
                    layout,
                    mapped_tiles: tile.mapped_buffer_tiles.clone(),
                    note: None,
                    guaranteed: true,
                    tile_snapshot: Some((cpu_seq, pages_json)),
                });
            }
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
                        note: None,
                    },
                ));
                continue;
            }
        };
        let n = |key: &str| d[key].as_u64().unwrap_or(0) as u32;
        // Upload and readback heaps; and acceleration structures, which a frame begins with as they were built
        // (see `rays`).
        if matches!(d["heap_properties"]["type"].as_u64(), Some(2 | 3)) || model::acceleration_structures(d) {
            continue;
        }
        let texture = n("dimension") > 1;
        let simultaneous = n("flags") & 0x20 != 0;
        let format = n("format");
        let layout = model::layout(d);
        let reserved = d["heap_properties"].is_null();
        let tile = tiles.get(&resource);
        if reserved {
            let pages = tile.map_or_else(
                || Err("its page table was not asked for".to_string()),
                |t| t.pages.clone(),
            );
            page(live, resource, cpu_seq, pages);
        }
        // Behind a wait nothing submitted yet signals, a buffer is still copied: the copy runs on this queue past the
        // wait, just before the lists, so it finds what they will; a texture's state there is not known.
        let blocked = texture && !simultaneous;
        let why = if let Some(why) = unknown.filter(|_| blocked) {
            Some(why.to_string())
        } else if let Some(why) = reserved
            .then(|| tile.and_then(|t| t.unreadable.clone()))
            .flatten()
        {
            Some(why)
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
                    note: None,
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
        let heap_only = heap.contains(&resource);
        let mapped_tiles = if reserved && !texture {
            tile.and_then(|t| t.mapped_buffer_tiles.clone())
        } else {
            None
        };
        let planned_bytes = if texture {
            locum_capture::layout::total(&layout)
        } else {
            super::readback::buffer_copy_bytes(locum_capture::layout::total(&layout), mapped_tiles.as_deref())
        };
        // Selected complete passes bypass the heap-indexed cap (still honoring the disk safety reserve).
        let charge_heap = heap_only && !pinned;
        if let Err(why) = live.room.take(planned_bytes, charge_heap) {
            let named = if heap_only {
                ", named only by a descriptor heap its root signature indexes directly"
            } else {
                ""
            };
            failed.push((
                resource,
                model::FirstUse {
                    submission,
                    states: json!([]),
                    content: Err(format!("not copied{named}: {why}")),
                    note: None,
                },
            ));
            continue;
        }
        // A multisampled texture's layout has an image a sample; its samples are drawn out joined (see
        // `locum_capture::d3d12::samples`).
        let samples = n("sample_count").max(1);
        live.remapped_since_copy.remove(&resource);
        snapshots.push(super::readback::Snapshot {
            resource,
            subresources: if texture {
                layout.len() as u32 / samples * planes
            } else {
                1
            },
            joined: (planes == 2 && samples == 1)
                .then(|| locum_capture::format::layout(format).map_or(4, |f| f.block_bytes)),
            states,
            layout,
            mapped_tiles,
            guaranteed: pinned,
            tile_snapshot: None,
            note: unknown
                .map(|why| {
                    format!(
                        "{why}: copied when the queue got past the wait, as this submission found it (work submitted \
                         later that the GPU ran first may have changed it before)"
                    )
                })
                .or_else(|| {
                    heap_only.then(|| {
                        "named only by a descriptor heap its root signature indexes directly: copied as the slots \
                         its programs may read hold it"
                            .to_string()
                    })
                }),
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

/// Byte range `[begin, end)` covered by a buffer SRV or UAV descriptor, if described.
fn buffer_view_range(d: &model::Descriptor) -> Option<(u64, u64)> {
    if !matches!(d.kind, "srv" | "uav") || d.resource == 0 || d.description["dimension"] != 1 {
        return None;
    }
    let p = &d.description["payload"];
    let first = p["FirstElement"].as_u64()?;
    let count = p["NumElements"].as_u64()?;
    if count == 0 {
        return None;
    }
    let stride = p["StructureByteStride"].as_u64().unwrap_or(0);
    let elem_bytes = if stride > 0 {
        stride
    } else if p["Flags"].as_u64().unwrap_or(0) & 1 != 0 {
        4
    } else {
        let fmt = d.description["format"].as_u64().unwrap_or(0) as u32;
        u64::from(locum_capture::format::layout(fmt).map_or(4, |l| l.block_bytes))
    };
    let begin = first.saturating_mul(elem_bytes);
    let end = begin.saturating_add(count.saturating_mul(elem_bytes));
    Some((begin, end))
}

/// D3D12's largest constant buffer view: 4096 constants of 16 bytes.
const CONSTANT_BUFFER_BYTES: u64 = 65536;

/// The upload bytes an armed submission's work reads: every upload buffer whole at the frame's first submission
/// (marked `whole`, the contents its resources began the frame with), then at each submission all referenced
/// ranges: constant buffers, buffer SRVs/UAVs, vertex/index buffers, indirect args/counts, root SRVs/UAVs, and
/// buffer/texture copy sources, merged into runs per buffer.
fn upload_copies<'a>(
    uploads: &BTreeMap<u64, Upload>,
    first: bool,
    lists: &[Arc<model::List>],
    view_ranges: impl Iterator<Item = &'a (u64, u64, u64)>,
    selected: bool,
    selection: &super::select::Selection,
) -> Vec<super::readback::Copy> {
    let mut out = Vec::new();
    if first {
        for (&resource, u) in uploads {
            out.push(super::readback::Copy {
                resource,
                offset: 0,
                len: u.bytes as u64,
                whole: true,
                guaranteed: selected || selection.pins_resource(resource),
            });
        }
    }
    let mut ranges = Vec::new();
    for list in lists {
        read_ranges(list, uploads, &mut ranges);
    }
    ranges.extend(view_ranges.filter(|(resource, _, _)| uploads.contains_key(resource)));
    for (resource, begin, end) in runs(ranges, CONSTANT_BUFFER_BYTES) {
        let end = end.min(uploads[&resource].bytes as u64);
        if begin < end {
            out.push(super::readback::Copy {
                resource,
                offset: begin,
                len: end - begin,
                whole: false,
                guaranteed: selected || selection.pins_resource(resource),
            });
        }
    }
    out
}

/// The upload bytes this list and the bundles it executes read, as (resource, begin, end): copy sources, root
/// CBV/SRV/UAV views, vertex/index buffer views, and ExecuteIndirect argument/count buffers.
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
            "CopyTextureRegion" => {
                let src = &c.args[4];
                if let Some(resource) = upload(&src["resource"]) {
                    let f = &src["footprint"];
                    let offset = f["offset"].as_u64().unwrap_or(0);
                    let row_pitch = f["row_pitch"].as_u64().unwrap_or(0);
                    let height = f["height"].as_u64().unwrap_or(1);
                    let depth = f["depth"].as_u64().unwrap_or(1);
                    let est = row_pitch.saturating_mul(height).saturating_mul(depth);
                    let end = if est > 0 {
                        offset.saturating_add(est)
                    } else {
                        uploads[&resource].bytes as u64
                    };
                    ranges.push((resource, offset, end));
                }
            }
            "IASetVertexBuffers" => {
                for vb in c.args[1].as_array().into_iter().flatten() {
                    if let (Some(resource), Some(offset), Some(bytes)) = (
                        upload(&vb["buffer"]["resource"]),
                        vb["buffer"]["offset"].as_u64(),
                        vb["bytes"].as_u64(),
                    ) {
                        ranges.push((resource, offset, offset.saturating_add(bytes)));
                    }
                }
            }
            "IASetIndexBuffer" => {
                let ib = &c.args[0];
                if let (Some(resource), Some(offset), Some(bytes)) = (
                    upload(&ib["buffer"]["resource"]),
                    ib["buffer"]["offset"].as_u64(),
                    ib["bytes"].as_u64(),
                ) {
                    ranges.push((resource, offset, offset.saturating_add(bytes)));
                }
            }
            "ExecuteIndirect" => {
                if let (Some(resource), Some(offset)) = (upload(&c.args[2]), c.args[3].as_u64()) {
                    ranges.push((resource, offset, uploads[&resource].bytes as u64));
                }
                if let (Some(resource), Some(offset)) = (upload(&c.args[4]), c.args[5].as_u64()) {
                    ranges.push((resource, offset, offset.saturating_add(4)));
                }
            }
            "SetGraphicsRootConstantBufferView" | "SetComputeRootConstantBufferView" => {
                let at = &c.args[1];
                if let (Some(resource), Some(offset)) = (at["resource"].as_u64(), at["offset"].as_u64())
                    && uploads.contains_key(&resource)
                {
                    ranges.push((resource, offset, offset.saturating_add(CONSTANT_BUFFER_BYTES)));
                }
            }
            "SetGraphicsRootShaderResourceView"
            | "SetComputeRootShaderResourceView"
            | "SetGraphicsRootUnorderedAccessView"
            | "SetComputeRootUnorderedAccessView" => {
                let at = &c.args[1];
                if let (Some(resource), Some(offset)) = (at["resource"].as_u64(), at["offset"].as_u64())
                    && uploads.contains_key(&resource)
                {
                    // Root SRV/UAV has no explicit size in D3D12: cover from offset to the upload buffer's end.
                    ranges.push((resource, offset, uploads[&resource].bytes as u64));
                }
            }
            _ => {}
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

/// Records reserved resource `resource`'s page table as the armed frame finds it, `pages` ([`super::tiles::pages`]),
/// once per frame.
fn page(live: &mut Live, resource: u64, cpu_seq: u64, pages: Result<(Value, Value), String>) {
    if resource != 0 && live.paged.insert(resource) {
        live.frame.tile_pages.push(model::TilePages {
            resource,
            cpu_seq,
            pages,
        });
    }
}

/// Before a change to the tile mappings of `resources`: an armed frame records their page tables as it found them.
/// They are asked for before the state is taken: a page table never asked for is asked of the real device (see
/// [`submit`]).
pub fn tiles_changing(resources: &[u64]) {
    if !super::passes::ARMED.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    let pages: Vec<_> = resources.iter().map(|&r| (r, super::tiles::pages(r))).collect();
    let mut s = state();
    let cpu_seq = s.cpu_seq;
    if let Some(live) = s.capture.as_mut() {
        for (resource, pages) in pages {
            page(live, resource, cpu_seq, pages);
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
        live.remapped_since_copy.insert(resource);
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
    let last_sub = s.last_submission_on_queue.get(&queue.id).copied();
    match kind {
        "Signal" => {
            let v = s.signals.entry(fence).or_default();
            *v = (*v).max(value);
            s.producers.insert(
                fence,
                ProducerSignal {
                    queue: queue.id,
                    submission: last_sub,
                    cpu_seq,
                    value,
                },
            );
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

/// The adapter a captured frame ran on: the device's that made the back buffer it presented, else the first
/// device's; or why it is not known. Asked of the real DXGI, so the game's factories see nothing.
fn adapter(presented: Option<u64>) -> Result<environment::Adapter, String> {
    use windows::Win32::Graphics::Direct3D12::ID3D12Device;
    use windows::Win32::Graphics::Dxgi::{IDXGIAdapter, IDXGIFactory4};
    use windows::core::{GUID, Interface};
    let device = presented
        .map(owner_device)
        .filter(|&d| d != 0)
        .or_else(|| {
            let s = state();
            let mut devices: Vec<u64> = s
                .objects
                .values()
                .filter(|o| o.kind.starts_with("ID3D12Device"))
                .map(|o| o.id)
                .collect();
            devices.sort_unstable();
            devices.first().copied()
        })
        .ok_or("the process has no D3D12 device")?;
    let device =
        super::real::<ID3D12Device>(device).ok_or_else(|| format!("device #{device} has no real device"))?;
    let luid = unsafe { device.GetAdapterLuid() };
    type Factory = unsafe extern "system" fn(u32, *const GUID, *mut *mut c_void) -> HRESULT;
    let create =
        unsafe { crate::system::export::<Factory>(crate::system::system().dxgi, c"CreateDXGIFactory2") }
            .ok_or("the system's dxgi.dll has no CreateDXGIFactory2")?;
    let mut raw = std::ptr::null_mut();
    unsafe { create(0, &IDXGIFactory4::IID, &mut raw) }
        .ok()
        .map_err(|e| format!("CreateDXGIFactory2: {e}"))?;
    let factory = unsafe { IDXGIFactory4::from_raw(raw) };
    let adapter: IDXGIAdapter = unsafe { factory.EnumAdapterByLuid(luid) }.map_err(|e| {
        format!(
            "EnumAdapterByLuid({:08x}:{:08x}): {e}",
            luid.HighPart, luid.LowPart
        )
    })?;
    environment::adapter(&adapter, environment::Api::D3d12)
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
        // A submission of the frame still making its copies adds their results after: the capture waits for it, as
        // long as a copy may take (it may be waiting on what this thread holds).
        let waiting = Instant::now();
        while s.capture.is_some() && s.submitting > 0 && waiting.elapsed() < SUBMITTING {
            drop(s);
            std::thread::sleep(Duration::from_millis(1));
            s = state();
        }
        if s.capture.is_some() && s.submitting > 0 {
            log!(
                "D3D12 capture closing with {} submissions still making their copies after {:?}: what they copy is not in it",
                s.submitting,
                SUBMITTING
            );
        }
        (s.capture.take(), s.series.take())
    };
    if finished.is_some() {
        // The series' next frame is armed at the end of this Present, which writes this one first: the lists other
        // threads reset meanwhile, the next frame's, are instrumented as in a lead.
        if series.as_ref().is_some_and(|s| s.index < s.frames) {
            super::passes::EARLY.store(true, std::sync::atomic::Ordering::Release);
        } else {
            let settings = crate::settings::settings();
            super::select::configure(
                settings.indirect_args,
                !settings.complete_passes.is_empty(),
                settings.complete_output_bytes,
            );
        }
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
        used,
        rays,
        room,
        ..
    }) = finished
    {
        super::log_counts();
        for (what, (count, bytes)) in room.uncopied {
            let gap = if what == "disk" {
                "d3d12-disk"
            } else {
                "d3d12-heap-indexed"
            };
            capture.gap(
                gap,
                format!("{count} first-use copies ({} MiB) not made: {what}", bytes >> 20),
                count,
            );
        }
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
        // The adapter the frame was drawn on; the executable is hashed on the writer thread, below.
        let adapter = adapter(presented);
        capture.meta("capture_status", "incomplete");
        capture.meta(
            "verification_scope",
            "command history, first-use contents, pass-end targets and what draws, dispatches and clears wrote through UAVs",
        );
        let mut prepared = frame.finish(&mut capture, projection);
        // Before the game's next submission on this thread: what argument records name is copied now, its results
        // apart from the frame's own copies.
        let after_frame = super::readback::Results::default();
        unsafe { super::indirect::after_frame(&mut prepared, &mut capture, &used, &after_frame) };
        let t = std::time::Instant::now();
        // The copies finish on the GPU soon after the frame and the collector reads them in order: the capture is
        // written after the last, off the game's threads. The same "capture written" / "capture NOT written" lines as
        // Direct3D 11's: runners wait for them.
        super::readback::then(move || {
            writer::run(move || {
                capture.environment(&environment::Environment::probe(
                    adapter,
                    environment::Api::D3d12,
                    true,
                ));
                for batch in std::mem::take(&mut *results.lock().unwrap_or_else(|e| e.into_inner())) {
                    prepared.add_uploads(batch.submission, batch.uploads);
                    for (resource, first) in batch.contents {
                        prepared.set_first_use(resource, first);
                    }
                    for snap in batch.tile_snapshots {
                        prepared.add_tile_snapshot(snap);
                    }
                    for (id, content) in batch.checkpoints {
                        prepared.set_checkpoint(id, content);
                    }
                    for id in batch.continued {
                        prepared.set_continued(id);
                    }
                }
                for batch in std::mem::take(&mut *after_frame.lock().unwrap_or_else(|e| e.into_inner())) {
                    for (resource, copy) in batch.contents {
                        prepared.set_after_frame(resource, copy);
                    }
                }
                for (key, b) in &rays {
                    prepared.add_acceleration_structure(super::rays::record(*key, b));
                }
                prepared.contents(&mut capture, &crate::settings::settings().blobs);
                capture.d3d12 = Some(prepared);
                if let Some((from, sidecar)) = crate::sidecar::captured(&crate::settings::settings().blobs) {
                    capture.meta("sidecar", from);
                    capture.sidecar = Some(sidecar);
                }
                let file = path.join("capture.db");
                let result = std::fs::create_dir_all(&path)
                    .map_err(|e| e.to_string())
                    .and_then(|()| db::write(&file, &capture).map_err(|e| e.to_string()));
                match result {
                    Ok(()) => {
                        let probes = &crate::settings::settings().dispatch_probes;
                        if !probes.is_empty() {
                            record_dispatch_probes(&file, probes);
                        }
                        log!(
                            "capture written: {} ({} events, {} draws, {} gaps; {:.1} s after the frame)",
                            file.display(),
                            capture.events.len(),
                            capture.draws.len(),
                            capture.gaps.len(),
                            t.elapsed().as_secs_f64()
                        );
                    }
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
    // A request's lead: lists reset since the Present that read it are instrumented, and its first frame is armed
    // `early` Presents after that one.
    let lead = state().lead.take();
    if let Some((left, series)) = lead {
        if left > 1 {
            state().lead = Some((left - 1, series));
        } else {
            arm(&root, frame_number, series);
        }
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
        let complete_passes: Vec<String> = text
            .lines()
            .filter_map(|l| l.strip_prefix("complete-pass "))
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .collect();
        let indirect_args = text
            .lines()
            .find_map(|l| l.strip_prefix("indirect-args "))
            .and_then(|v| match v.trim() {
                "on" => Some(true),
                "off" => Some(false),
                _ => None,
            });
        let series = Series {
            label: (!label.is_empty()).then_some(label),
            index: 1,
            frames,
            complete_passes,
            indirect_args,
        };
        let early = match text.lines().find_map(|l| l.strip_prefix("early ")) {
            None => 0,
            Some(n) => match n.trim().parse::<u32>() {
                Ok(n @ 0..=MAX_EARLY) => n,
                _ => {
                    log!("D3D12 capture request: early {n:?} is not 0 to {MAX_EARLY}; no lead");
                    0
                }
            },
        };
        if early == 0 {
            arm(&root, frame_number, series);
            return;
        }
        state().lead = Some((early, series));
        super::passes::EARLY.store(true, std::sync::atomic::Ordering::Release);
        log!(
            "D3D12 capture request read at Present {frame_number}: lists reset from now on are instrumented, the capture armed {early} Presents later"
        );
    }
}

/// The bytes free to this process on the drive that holds `dir` (or its nearest folder that exists).
fn free_bytes(dir: &std::path::Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let dir = dir.ancestors().find(|d| d.exists())?;
    let wide: Vec<u16> = dir.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0u64;
    unsafe {
        windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            windows::core::PCWSTR(wide.as_ptr()),
            Some(&mut free),
            None,
            None,
        )
    }
    .ok()
    .map(|()| free)
}

/// Arms the capture of the frame after Present `frame_number`, frame `series.index` of its series: written to
/// `captures/<label>` (one frame), `captures/<label>-<index>` (a series) or `captures/frame-<n>` (no label).
fn arm(root: &std::path::Path, frame_number: u64, series: Series) {
    // A request's lead, or the Present between two frames of a series, ends here: an armed capture instruments
    // lists itself. Lists are instrumented throughout: the lead ends once the capture is armed.
    let lead_ends = || super::passes::EARLY.store(false, std::sync::atomic::Ordering::Release);
    let name = match &series.label {
        Some(label) if series.frames > 1 => format!("{label}-{}", series.index),
        Some(label) => label.clone(),
        None => format!("frame-{}", frame_number + 1),
    };
    let path = root.join("captures").join(name);
    if path.exists() {
        log!("D3D12 capture destination already exists: {}", path.display());
        lead_ends();
        return;
    }
    // Asked before the state is taken: a drive can be slow to answer.
    let settings = crate::settings::settings();
    let disk = free_bytes(settings.blobs.root());
    let mut specs = settings.complete_passes.clone();
    specs.extend(series.complete_passes.iter().cloned());
    let (selection, sel_errors) = super::select::Selection::from_specs(&specs);
    for err in sel_errors {
        log!("D3D12 complete-pass selector error: {err}");
    }
    let indirect_args = series.indirect_args.unwrap_or(settings.indirect_args);
    super::select::configure(
        indirect_args,
        !selection.is_empty(),
        settings.complete_output_bytes,
    );
    let mut s = state();
    s.descriptors.clear();
    s.constant_buffers.clear();
    s.upload_views.clear();
    // Snapshot live fence state at arming (every fence with pending waits or known signals).
    let mut fence_ids: std::collections::BTreeSet<u64> = s.signals.keys().copied().collect();
    fence_ids.extend(s.producers.keys().copied());
    for waits in s.waits.values() {
        for &(f, _) in waits {
            fence_ids.insert(f);
        }
    }
    let arm_cpu_seq = s.cpu_seq;
    let fence_states: Vec<model::FenceState> = fence_ids
        .into_iter()
        .filter_map(|fence| {
            let real = super::real::<windows::Win32::Graphics::Direct3D12::ID3D12Fence>(fence)?;
            let completed = unsafe { real.GetCompletedValue() };
            let prod = s.producers.get(&fence).copied();
            Some(model::FenceState {
                phase: "arm",
                submission: None,
                cpu_seq: arm_cpu_seq,
                fence,
                completed,
                producer_queue: prod.map(|p| p.queue),
                producer_submission: prod.and_then(|p| p.submission),
                producer_value: prod.map(|p| p.value),
                producer_cpu_seq: prod.map(|p| p.cpu_seq),
            })
        })
        .collect();
    let objects: Vec<_> = s.objects.values().cloned().collect();
    s.armings += 1;
    s.capture = Some(Live {
        number: s.armings,
        series: (series.frames > 1).then(|| series.clone()),
        path,
        projection: model::Projection::new(&objects),
        frame: model::Frame {
            objects,
            fence_states,
            ..model::Frame::default()
        },
        capture: db::Capture::default(),
        results: Default::default(),
        used: HashSet::new(),
        executed: HashSet::new(),
        paged: HashSet::new(),
        rays: super::rays::latest(),
        room: Room {
            heap: settings.heap_indexed.then_some(settings.heap_indexed_bytes),
            disk,
            ..Room::default()
        },
        selection,
        remapped_since_copy: HashSet::new(),
    });
    s.series = Some(series.clone());
    super::passes::ARMED.store(true, std::sync::atomic::Ordering::Release);
    lead_ends();
    log!(
        "D3D12 capture armed at Present {frame_number} (frame {} of {}); cached list history retained; {} copies recorded so far",
        series.index,
        series.frames,
        super::passes::recorded()
    );
}

/// Evaluates opt-in `dispatch-probe` settings against `db_path` in-process (reading CPU alias bytes
/// via `ReadProcessMemory(GetCurrentProcess())`) and persists rows into `d12_dispatch_probes`.
fn record_dispatch_probes(db_path: &std::path::Path, specs: &[String]) {
    use locum_capture::probe::{
        AliasMap, DispatchProbeRecord, append_table, parse_settings_probe, probe_capture,
    };
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Threading::GetCurrentProcess;

    let blobs = &crate::settings::settings().blobs;
    let mut rows = Vec::new();
    for spec in specs {
        let (query, alias_path) = match parse_settings_probe(spec) {
            Ok(v) => v,
            Err(e) => {
                log!("dispatch-probe {spec:?}: {e}");
                continue;
            }
        };
        let alias_map = match alias_path.as_deref() {
            Some(p) => match AliasMap::from_file(p) {
                Ok(m) => m,
                Err(e) => {
                    log!("dispatch-probe alias-map {}: {e}", p.display());
                    AliasMap::default()
                }
            },
            None => AliasMap::default(),
        };
        let mut captured_cpu: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
        let results = match probe_capture(
            db_path,
            &query,
            &alias_map,
            Some(|va: u64, len: usize| {
                let mut buf = vec![0u8; len];
                let mut read = 0usize;
                unsafe {
                    ReadProcessMemory(
                        GetCurrentProcess(),
                        va as *const core::ffi::c_void,
                        buf.as_mut_ptr().cast(),
                        len,
                        Some(&mut read),
                    )
                }
                .map_err(|e| format!("ReadProcessMemory({va:#x}): {e}"))?;
                buf.truncate(read);
                let key = locum_capture::hash::key(&buf);
                let _ = blobs.put(&key, &buf);
                captured_cpu.insert(va, key);
                Ok(buf)
            }),
        ) {
            Ok(r) => r,
            Err(e) => {
                log!("dispatch-probe {spec:?}: {e}");
                continue;
            }
        };
        for r in results {
            let raw_desc = r
                .decoded
                .as_ref()
                .map(|d| {
                    (0..d.raw_hex.len())
                        .step_by(2)
                        .filter_map(|i| u8::from_str_radix(&d.raw_hex[i..i + 2], 16).ok())
                        .collect()
                })
                .unwrap_or_default();
            let cpu_blob = r.cpu_alias_va.and_then(|va| captured_cpu.get(&va).cloned());
            let status = r
                .comparison
                .as_ref()
                .map(|c| c.status.clone())
                .unwrap_or_else(|| {
                    if r.gpu_unavailable_reason.is_some() {
                        "gpu-unavailable".into()
                    } else if r.cpu_unavailable_reason.is_some() {
                        "cpu-unavailable".into()
                    } else {
                        "unavailable".into()
                    }
                });
            rows.push(DispatchProbeRecord {
                dispatch_draw: r.dispatch.draw,
                event: r.dispatch.event,
                shader: r.dispatch.cs_hash,
                sample_point: r.gpu_sample_point,
                descriptor_source: r.descriptor_source,
                raw_descriptor: raw_desc,
                resolved_gpu_va: r.target_gpu_va,
                resolved_resource: r.target_resource,
                resolved_offset: r.target_offset,
                resolved_size: r.decoded.as_ref().and_then(|d| d.decoded_size),
                gpu_bytes_blob: None,
                cpu_alias_va: r.cpu_alias_va,
                cpu_bytes_blob: cpu_blob,
                compare_bytes: query.compare_bytes as u64,
                mismatch_count: r.comparison.as_ref().map(|c| c.mismatch_count as u64),
                first_mismatch_offset: r
                    .comparison
                    .as_ref()
                    .and_then(|c| c.first_mismatch_offset.map(|o| o as u64)),
                status,
                note: r.gpu_unavailable_reason.or(r.cpu_unavailable_reason),
            });
        }
    }
    if let Err(e) = append_table(db_path, &rows) {
        log!("dispatch-probe write failed: {e}");
    } else if !rows.is_empty() {
        log!(
            "dispatch-probe: recorded {} row(s) in d12_dispatch_probes",
            rows.len()
        );
    }
}

/// Arms the capture of the next frame into `root` at once, the first of `frames`, as a request read at a Present
/// would.
#[cfg(test)]
pub(super) fn arm_now(root: &std::path::Path, frames: u32) {
    arm(
        root,
        0,
        Series {
            label: None,
            index: 1,
            frames,
            complete_passes: Vec::new(),
            indirect_args: None,
        },
    );
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::path::PathBuf;

    use locum_capture::{d3d12 as model, db};
    use serde_json::json;

    use super::{DISK_KEPT, Live, Room, Tiles, first_uses, runs};

    fn reserved_buffer(id: u64, width: u64) -> model::Object {
        model::Object {
            id,
            kind: "ID3D12Resource",
            device: 1,
            description: json!({
                "dimension": 1,
                "width": width,
                "height": 1,
                "depth_or_array": 1,
                "mips": 1,
                "format": 0,
                "sample_count": 1,
                "flags": 0,
                "heap_properties": null,
            }),
        }
    }

    fn test_live(objects: &BTreeMap<u64, model::Object>, room: Room) -> Live {
        let list: Vec<_> = objects.values().cloned().collect();
        Live {
            number: 1,
            series: None,
            path: PathBuf::new(),
            projection: model::Projection::new(&list),
            frame: model::Frame {
                objects: list,
                ..model::Frame::default()
            },
            capture: db::Capture::default(),
            results: Default::default(),
            used: HashSet::new(),
            executed: HashSet::new(),
            paged: HashSet::new(),
            rays: Vec::new(),
            room,
            selection: super::super::select::Selection::default(),
            remapped_since_copy: HashSet::new(),
        }
    }

    #[test]
    fn first_use_copies_stop_at_the_heap_cap_and_short_of_the_disk() {
        const MIB: u64 = 1 << 20;
        // 100 MiB for resources only a heap names; the drive has 150 MiB past what is kept free.
        let mut room = Room {
            heap: Some(100 * MIB),
            disk: Some(DISK_KEPT + 150 * MIB),
            ..Room::default()
        };
        assert!(room.take(60 * MIB, true).is_ok());
        let past = room.take(50 * MIB, true).unwrap_err();
        assert!(past.contains("past `heap-indexed-mb` 100: 60 MiB"), "{past}");
        // A resource an event names is not held to the heap's cap, only to the disk.
        assert!(room.take(80 * MIB, false).is_ok());
        let disk = room.take(20 * MIB, false).unwrap_err();
        assert!(
            disk.starts_with("disk: ") && disk.contains("take 140 MiB"),
            "{disk}"
        );
        assert!(room.take(10 * MIB, false).is_ok());
        assert_eq!((room.planned, room.heap_bytes), (150 * MIB, 60 * MIB));
        assert_eq!(
            room.uncopied.into_iter().collect::<Vec<_>>(),
            [("disk", (1, 20 * MIB)), ("heap-indexed-mb", (1, 50 * MIB))]
        );
        // `heap-indexed off`, and a drive that could not be asked.
        let mut off = Room::default();
        assert_eq!(off.take(1, true).unwrap_err(), "`heap-indexed off`");
        assert!(off.take(u64::MAX / 2, false).is_ok());
    }
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

    #[test]
    fn reserved_buffers_charge_mapped_tiles_not_virtual_size_and_keep_disk_reserve() {
        const GIB4: u64 = 1 << 32;
        const MIB: u64 = 1 << 20;
        const TILE: u64 = locum_capture::tiles::TILE_BYTES;
        const CHUNK: u64 = 256 << 20;

        // 4 GiB reserved buffer with one mapped tile (id=10, heap-only) and a genuinely empty 4 GiB one (id=11),
        // with only 1 MiB above the 8 GiB disk reserve and 1 MiB heap-indexed cap.
        let mut objects = BTreeMap::new();
        objects.insert(10, reserved_buffer(10, GIB4));
        objects.insert(11, reserved_buffer(11, GIB4));
        objects.insert(12, reserved_buffer(12, GIB4));
        let states = super::super::states::Tracker::default();
        let mut tiles = HashMap::new();
        tiles.insert(
            10,
            Tiles {
                pages: Ok((json!({}), json!([[17, 1, 99, 0]]))),
                unreadable: None,
                mapped_buffer_tiles: Some(vec![17]),
            },
        );
        tiles.insert(
            11,
            Tiles {
                pages: Ok((json!({}), json!([]))),
                unreadable: None,
                mapped_buffer_tiles: Some(Vec::new()),
            },
        );
        // Unknown page table falls back to charging the full virtual size.
        tiles.insert(
            12,
            Tiles {
                pages: Err("page table tracking failed".into()),
                unreadable: None,
                mapped_buffer_tiles: None,
            },
        );
        let mut live = test_live(
            &objects,
            Room {
                heap: Some(MIB),
                disk: Some(DISK_KEPT + MIB),
                ..Room::default()
            },
        );
        let heap = HashSet::from([10u64]);
        let snapshots = first_uses(
            &mut live,
            1,
            &objects,
            &states,
            vec![10, 11, 12],
            &heap,
            None,
            1,
            &tiles,
            false,
        );
        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].resource, 10);
        assert_eq!(snapshots[0].mapped_tiles.as_deref(), Some(&[17][..]));
        assert_eq!(snapshots[1].resource, 11);
        assert_eq!(snapshots[1].mapped_tiles.as_deref(), Some(&[][..]));

        // Room charge matches the exact bytes readback::segments copies for both snapshots.
        let copied_by_reader: u64 = snapshots
            .iter()
            .map(|s| {
                super::super::readback::segments(
                    locum_capture::layout::total(&s.layout),
                    CHUNK,
                    s.mapped_tiles.as_deref(),
                )
                .into_iter()
                .filter(|seg| seg.2)
                .map(|seg| seg.1)
                .sum::<u64>()
            })
            .sum();
        assert_eq!(copied_by_reader, TILE);
        assert_eq!(live.room.planned, TILE);
        assert_eq!(live.room.heap_bytes, TILE);
        // Resource 12 (unknown page table) was conservatively charged 4 GiB and rejected for disk room.
        assert_eq!(live.room.uncopied.get("disk"), Some(&(1, GIB4)));

        // Nine 4 GiB reserved windows (36 GiB virtual) with 1 MiB mapped each fit in 10 MiB above DISK_KEPT.
        let mut nine_objects = BTreeMap::new();
        let mut nine_tiles = HashMap::new();
        let mut nine_ids = Vec::new();
        for id in 100..109u64 {
            nine_objects.insert(id, reserved_buffer(id, GIB4));
            nine_tiles.insert(
                id,
                Tiles {
                    pages: Ok((json!({}), json!([[0, 16, 1, 0]]))),
                    unreadable: None,
                    mapped_buffer_tiles: Some((0..16).collect()),
                },
            );
            nine_ids.push(id);
        }
        let mut nine_live = test_live(
            &nine_objects,
            Room {
                heap: Some(10 * MIB),
                disk: Some(DISK_KEPT + 10 * MIB),
                ..Room::default()
            },
        );
        let nine_heap: HashSet<u64> = nine_ids.iter().copied().collect();
        let nine_snaps = first_uses(
            &mut nine_live,
            2,
            &nine_objects,
            &states,
            nine_ids,
            &nine_heap,
            None,
            2,
            &nine_tiles,
            false,
        );
        assert_eq!(nine_snaps.len(), 9);
        assert_eq!(nine_live.room.planned, 9 * MIB);
        assert_eq!(nine_live.room.heap_bytes, 9 * MIB);

        // The 8 GiB reserve is still enforced: below DISK_KEPT rejects even an empty resource; DISK_KEPT + 32 KiB
        // rejects a single 64 KiB mapped tile.
        let mut below_reserve = test_live(
            &objects,
            Room {
                heap: Some(MIB),
                disk: Some(DISK_KEPT - 1),
                ..Room::default()
            },
        );
        assert!(
            first_uses(
                &mut below_reserve,
                3,
                &objects,
                &states,
                vec![11],
                &HashSet::new(),
                None,
                3,
                &tiles,
                false,
            )
            .is_empty()
        );
        assert_eq!(below_reserve.room.uncopied.get("disk"), Some(&(1, 0)));

        let mut tight_reserve = test_live(
            &objects,
            Room {
                heap: Some(MIB),
                disk: Some(DISK_KEPT + TILE / 2),
                ..Room::default()
            },
        );
        assert!(
            first_uses(
                &mut tight_reserve,
                4,
                &objects,
                &states,
                vec![10],
                &HashSet::new(),
                None,
                4,
                &tiles,
                false,
            )
            .is_empty()
        );
        assert_eq!(tight_reserve.room.uncopied.get("disk"), Some(&(1, TILE)));
    }

    #[test]
    fn selected_pass_bypasses_heap_cap_and_snapshots_remapped_tiles() {
        const TILE: u64 = 65536;
        const MIB: u64 = 1 << 20;
        let mut objects = BTreeMap::new();
        objects.insert(18, reserved_buffer(18, 4 * MIB));
        let states = super::super::states::Tracker::default();
        let mut tiles = HashMap::new();
        tiles.insert(
            18,
            Tiles {
                pages: Ok((json!({}), json!([[0, 4, 1, 0]]))),
                unreadable: None,
                mapped_buffer_tiles: Some(vec![0, 1, 2, 3]),
            },
        );
        // Tight heap cap (only 64 KiB) would drop a 256 KiB heap-only resource normally, but selected=true bypasses it.
        let mut live = test_live(
            &objects,
            Room {
                heap: Some(TILE),
                disk: Some(DISK_KEPT + 16 * MIB),
                ..Room::default()
            },
        );
        let heap = HashSet::from([18u64]);
        let snaps = first_uses(
            &mut live,
            1,
            &objects,
            &states,
            vec![18],
            &heap,
            None,
            10,
            &tiles,
            true,
        );
        assert_eq!(snaps.len(), 1);
        assert_eq!(snaps[0].resource, 18);
        assert!(snaps[0].guaranteed);
        assert!(snaps[0].tile_snapshot.is_none());
        assert_eq!(live.room.heap_bytes, 0);

        // After a tile remap on #18, a subsequent selected submission takes a post-remap tile snapshot.
        live.remapped_since_copy.insert(18);
        tiles.insert(
            18,
            Tiles {
                pages: Ok((json!({}), json!([[4, 2, 2, 10]]))),
                unreadable: None,
                mapped_buffer_tiles: Some(vec![4, 5]),
            },
        );
        let snaps2 = first_uses(
            &mut live,
            2,
            &objects,
            &states,
            vec![18],
            &heap,
            None,
            20,
            &tiles,
            true,
        );
        assert_eq!(snaps2.len(), 1);
        assert_eq!(snaps2[0].resource, 18);
        assert!(snaps2[0].guaranteed);
        assert_eq!(snaps2[0].mapped_tiles.as_deref(), Some(&[4, 5][..]));
        assert!(snaps2[0].tile_snapshot.is_some());
        assert!(!live.remapped_since_copy.contains(&18));
    }

    #[test]
    fn expanded_upload_ranges_cover_vb_ib_root_indirect_and_srv() {
        use super::{Upload, buffer_view_range, read_ranges};
        let mut uploads = BTreeMap::new();
        uploads.insert(
            1548,
            Upload {
                pointer: 0,
                maps: 0,
                bytes: 64 << 20,
            },
        );
        let list = model::List {
            id: 1,
            generation: 1,
            commands: vec![
                model::Command {
                    call: "IASetVertexBuffers",
                    args: json!([0, [{"buffer": {"resource": 1548, "offset": 1024}, "bytes": 512, "stride": 32}]]),
                    bundle: None,
                    diagnostic: None,
                },
                model::Command {
                    call: "IASetIndexBuffer",
                    args: json!([{"buffer": {"resource": 1548, "offset": 4096}, "bytes": 256, "format": 42}]),
                    bundle: None,
                    diagnostic: None,
                },
                model::Command {
                    call: "SetComputeRootShaderResourceView",
                    args: json!([0, {"resource": 1548, "offset": 8192}]),
                    bundle: None,
                    diagnostic: None,
                },
                model::Command {
                    call: "ExecuteIndirect",
                    args: json!([10, 4, 1548, 16384, 1548, 32768]),
                    bundle: None,
                    diagnostic: None,
                },
            ],
            ..model::List::default()
        };
        let mut ranges = Vec::new();
        read_ranges(&list, &uploads, &mut ranges);
        assert!(ranges.contains(&(1548, 1024, 1536)), "{ranges:?}");
        assert!(ranges.contains(&(1548, 4096, 4352)), "{ranges:?}");
        assert!(
            ranges.iter().any(|&(r, b, e)| r == 1548 && b == 8192 && e > b),
            "{ranges:?}"
        );
        assert!(
            ranges.iter().any(|&(r, b, e)| r == 1548 && b == 16384 && e > b),
            "{ranges:?}"
        );
        assert!(ranges.contains(&(1548, 32768, 32772)), "{ranges:?}");

        let srv = model::Descriptor {
            id: 1,
            heap: 5,
            slot: 7,
            kind: "srv",
            resource: 1548,
            counter: 0,
            description: json!({
                "dimension": 1,
                "format": 0,
                "payload": {"FirstElement": 10, "NumElements": 20, "StructureByteStride": 64, "Flags": 0}
            }),
        };
        assert_eq!(buffer_view_range(&srv), Some((640, 1920)));
    }
}
