//! Continuous list history and per-submission capture. No COM ownership is kept in
//! the portable history; object IDs survive pointer and descriptor-slot reuse.
use core::ffi::c_void;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::Instant;

use super::cost::Counter;
use locum_capture::{d3d12 as model, db, writer};
use serde_json::{Value, json};
use windows::Win32::Graphics::Direct3D12::{D3D12_RANGE, ID3D12Resource};
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
}

impl Data {
    pub fn new(id: u64) -> Self {
        Self {
            id,
            recording: Mutex::new(None),
            queue_seq: Mutex::new(0),
        }
    }
    pub fn record(&self, call: &'static str, build: impl FnOnce() -> Value) {
        if !enabled() {
            return;
        }
        let start = Instant::now();
        let args = build();
        let mut recording = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(recording) = recording.as_mut() {
            recording.record(model::Command {
                call,
                args,
                bundle: None,
                diagnostic: None,
            });
        }
        RECORDING_COST.add(start, 0);
    }
    pub fn bundle(&self, pointer: *mut c_void) {
        if !enabled() {
            return;
        }
        let bundle = super::data(pointer).and_then(|d| d.snapshot());
        let mut recording = self.recording.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(recording) = recording.as_mut() {
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
    pub fn close(&self, hr: HRESULT) {
        if hr.is_ok()
            && let Some(r) = self.recording.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            r.close();
        }
    }
    pub fn reset(&self, hr: HRESULT, allocator: *mut c_void, pso: *mut c_void) {
        let start = Instant::now();
        let (allocator, pso) = (super::id(allocator), super::id(pso));
        if hr.is_ok()
            && let Some(r) = self.recording.lock().unwrap_or_else(|e| e.into_inner()).as_mut()
        {
            r.reset(allocator, pso);
        }
        RESET_COST.add(start, 0);
    }
}

#[derive(Default)]
struct State {
    objects: BTreeMap<u64, model::Object>,
    uploads: BTreeMap<u64, Upload>,
    cpu_seq: u64,
    next_submission: u64,
    capture: Option<(PathBuf, model::Frame)>,
    /// The descriptor versions the armed frame's submissions have reported.
    descriptors: super::slots::Known,
    /// The armed frame's constant-buffer views as of its latest submission: (heap, slot) to (resource, begin, end).
    constant_buffers: BTreeMap<(u64, u32), (u64, u64, u64)>,
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
        if let Some((_, frame)) = s.capture.as_mut() {
            frame.objects.retain(|o| o.id != id);
            frame.objects.push(object);
        }
    }
}
pub fn owner_device(id: u64) -> u64 {
    state().objects.get(&id).map_or(0, |o| o.device)
}
pub fn destroy(id: u64) {
    let start = Instant::now();
    {
        let mut s = state();
        s.objects.remove(&id);
        s.uploads.remove(&id);
    }
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
        *data.recording.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(model::Recording::new(data.id, kind, allocator, pso, closed));
        describe(
            data.id,
            device,
            json!({"type":kind,"allocator":allocator,"initial_pso":pso}),
        );
    }
}

/// Called while the queue lock is held, immediately before forwarding ExecuteCommandLists.
pub unsafe fn submit(queue: &Data, sequence: u64, count: u32, pointers: *const *mut c_void) {
    if !enabled() {
        return;
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
    let mut lists = Vec::new();
    let mut diagnostics = Vec::new();
    if armed {
        for (ordinal, pointer) in unsafe { slice(pointers, count as usize) }.iter().enumerate() {
            if let Some(data) = super::data(*pointer) {
                lists.push(data.snapshot().unwrap_or_else(|| {
                    Arc::new(model::List {
                        id: data.id,
                        ..model::List::default()
                    })
                }));
            } else {
                diagnostics.push(format!(
                    "submission {id} queue {}: list {ordinal} of {count} has no registered identity",
                    queue.id
                ));
            }
        }
    }
    let uploads = match s.capture.as_ref() {
        Some((_, frame)) => unsafe {
            upload_contents(
                &s.uploads,
                frame.submissions.is_empty(),
                &lists,
                s.constant_buffers.values(),
            )
        },
        None => Vec::new(),
    };
    if let Some((_, frame)) = s.capture.as_mut() {
        frame.diagnostics.extend(diagnostics);
        frame.submissions.push(model::Submission {
            id,
            queue: queue.id,
            queue_seq: sequence,
            cpu_seq,
            lists,
            descriptors,
            uploads,
        });
    }
    SUBMISSION_COST.add(start, 0);
}

/// D3D12's largest constant buffer view: 4096 constants of 16 bytes.
const CONSTANT_BUFFER_BYTES: u64 = 65536;

/// The upload bytes an armed submission's work reads: every upload buffer whole at the frame's first submission
/// (marked `whole`, the contents its resources began the frame with), then at each submission only the
/// constant-buffer ranges its lists' root CBVs and the descriptor heaps' CBVs name, merged into runs per buffer.
/// Copying every upload buffer whole at every submission cost seconds per submission on Elden Ring's persistent
/// 323 MB ring, so the armed frame never reached its closing Present. Copying each range on its own cost as much:
/// Elden Ring's 128k CBVs left 23.7k separate runs of ~540 bytes per submission, 36 s of per-copy overhead in one
/// frame; runs closer than [`CONSTANT_BUFFER_BYTES`] are copied as one (81 copies for that frame, 629 MiB).
///
/// # Safety
/// Mapped pointers in `uploads` are the application's live persistent maps.
unsafe fn upload_contents<'a>(
    uploads: &BTreeMap<u64, Upload>,
    first: bool,
    lists: &[Arc<model::List>],
    constant_buffers: impl Iterator<Item = &'a (u64, u64, u64)>,
) -> Vec<(u64, Value)> {
    let mut out = Vec::new();
    if first {
        for (&resource, u) in uploads {
            let mut content = unsafe { upload_range(resource, u, 0, u.bytes) };
            if let Some(o) = content.as_object_mut() {
                o.insert("whole".into(), json!(true));
            }
            out.push((resource, content));
        }
    }
    let mut ranges = Vec::new();
    for list in lists {
        constant_ranges(list, uploads, &mut ranges);
    }
    ranges.extend(constant_buffers.filter(|(resource, _, _)| uploads.contains_key(resource)));
    for (resource, begin, end) in runs(ranges, CONSTANT_BUFFER_BYTES) {
        let u = &uploads[&resource];
        let end = end.min(u.bytes as u64);
        if begin < end {
            out.push((resource, unsafe {
                upload_range(resource, u, begin as usize, (end - begin) as usize)
            }));
        }
    }
    out
}

/// Root constant-buffer views on upload buffers, in this list and the bundles it executes, as (resource, begin, end)
/// at the largest size a constant buffer can have: the bytes a shader reads are not known until reflection.
fn constant_ranges(list: &model::List, uploads: &BTreeMap<u64, Upload>, ranges: &mut Vec<(u64, u64, u64)>) {
    for c in &list.commands {
        if let Some(bundle) = &c.bundle {
            constant_ranges(bundle, uploads, ranges);
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

/// `len` bytes at `offset` of an upload buffer, through the application's persistent map or a map of our own.
///
/// # Safety
/// A non-zero `u.pointer` is the application's live map of the resource.
unsafe fn upload_range(resource: u64, u: &Upload, offset: usize, len: usize) -> Value {
    let start = Instant::now();
    let mut content = if u.pointer != 0 {
        unsafe { super::pipelines::bytes((u.pointer + offset) as *const c_void, len) }
    } else {
        // Re-map an unmapped CPU-visible resource through its real interface, reading only the range.
        match super::real(resource).and_then(|r| r.cast::<ID3D12Resource>().ok()) {
            Some(r) => {
                let mut p = std::ptr::null_mut();
                let range = D3D12_RANGE {
                    Begin: offset,
                    End: offset + len,
                };
                match unsafe { r.Map(0, Some(&range), Some(&mut p)) } {
                    Ok(()) => {
                        let content =
                            unsafe { super::pipelines::bytes(p.cast::<u8>().add(offset).cast(), len) };
                        unsafe { r.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 })) };
                        content
                    }
                    Err(e) => json!({"diagnostic":format!("upload snapshot Map: {e}")}),
                }
            }
            None => json!({"diagnostic":"upload resource identity expired before submission"}),
        }
    };
    if let Some(o) = content.as_object_mut() {
        o.insert("offset".into(), json!(offset));
    }
    UPLOAD_COST.add(start, len as u64);
    content
}

pub fn sync(queue: &Data, sequence: u64, kind: &'static str, fence: *mut c_void, value: u64, hr: HRESULT) {
    if !enabled() || hr.is_err() {
        return;
    }
    let fence = super::id(fence);
    let mut s = state();
    s.cpu_seq += 1;
    let cpu_seq = s.cpu_seq;
    if let Some((_, frame)) = s.capture.as_mut() {
        frame.sync.push(model::Sync {
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

pub fn present(frame_number: u64) {
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
    }
    drop(last);
    if !enabled() {
        return;
    }
    let Some(root) = folder() else {
        return;
    };
    let finished = state().capture.take();
    if let Some((path, frame)) = finished {
        super::log_counts();
        let mut capture = db::Capture::default();
        capture.meta("api", "d3d12");
        capture.meta("name", path.file_name().unwrap_or_default().to_string_lossy());
        capture.meta("locum", env!("CARGO_PKG_VERSION"));
        capture.meta("frame", frame_number);
        capture.meta("pid", std::process::id());
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
            "command history; resource checkpoints not yet implemented",
        );
        let prepared = frame.prepare(&mut capture);
        capture.gap(
            "d3d12-capture-development",
            "GPU first-use/pass-end checkpoints and full pipeline inspection are in development",
            1,
        );
        // The same "capture written" / "capture NOT written" lines as Direct3D 11's: runners wait for them.
        writer::run(move || {
            let t = std::time::Instant::now();
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
        });
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
        let label = if label.is_empty() {
            format!("frame-{}", frame_number + 1)
        } else {
            label
        };
        let path = root.join("captures").join(label);
        if path.exists() {
            log!("D3D12 capture destination already exists: {}", path.display());
            return;
        }
        let mut s = state();
        s.descriptors.clear();
        s.constant_buffers.clear();
        s.capture = Some((
            path,
            model::Frame {
                objects: s.objects.values().cloned().collect(),
                ..model::Frame::default()
            },
        ));
        log!("D3D12 capture armed at Present {frame_number}; cached list history retained");
    }
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
