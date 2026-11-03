//! Every resource the game creates, from its creation to its destruction: a number, its description, the stack that
//! created it, and the hash of the data it was created with (written to the blob store with `uploads store`). Updates
//! that cover a whole subresource are hashed the same way, except, outside a capture, those of per-frame data: a
//! constant buffer, or a resource already updated in this frame or the last. Those are rewritten many times a second
//! (Sekiro: thousands of updates a frame), their bytes match no file, and a capture records its own. A capture lists
//! every live resource from here, and takes the contents of the immutable ones it uses from their upload instead of
//! reading them back.
//!
//! Nothing here holds a reference to a resource: entries are keyed by number and removed by the runtime's
//! destruction callback, so the registry never keeps a resource alive or touches a dead one.

use core::ffi::c_void;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use locum_capture::cost::Cost;
use locum_capture::hash::key;
use locum_capture::layout::{self, Sub};
use locum_capture::{stacks, writer};
use windows::Win32::Graphics::Direct3D::ID3DDestructionNotifier;
use windows::Win32::Graphics::Direct3D11::*;
use windows::core::{GUID, Interface};

use crate::log::log;
use crate::settings::{Uploads, settings};

/// The private-data key of the number locum gives each object, with the number of the capture that last described
/// it: `[id, capture]`.
pub const ID_KEY: GUID = GUID::from_u128(0x6c6f_6375_6d00_4a5e_9d1b_7a2f_8c3e_0002);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// What a resource is, from its description.
#[derive(Clone, Debug, Default)]
pub struct Dims {
    pub kind: &'static str,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mips: u32,
    pub array: u32,
    pub format: u32,
    pub samples: u32,
    pub stride: u32,
    pub usage: u32,
    pub bind: u32,
    pub cpu: u32,
    pub misc: u32,
    /// The API's description struct.
    pub desc: Vec<u8>,
}

impl Dims {
    /// The subresources of its contents in the canonical layout (a multisampled texture's as resolved), or None
    /// for a format without one.
    pub fn subs(&self) -> Option<Vec<Sub>> {
        if self.kind == "buffer" {
            return Some(layout::buffer(self.width));
        }
        layout::texture(
            self.format,
            self.width,
            self.height,
            self.depth,
            self.mips,
            self.array,
        )
    }

    pub fn immutable(&self) -> bool {
        self.usage == D3D11_USAGE_IMMUTABLE.0 as u32
    }
}

fn bytes_of<T>(t: &T) -> Vec<u8> {
    unsafe { core::slice::from_raw_parts((t as *const T).cast::<u8>(), size_of::<T>()) }.to_vec()
}

pub fn buffer_dims(d: &D3D11_BUFFER_DESC) -> Dims {
    Dims {
        kind: "buffer",
        width: d.ByteWidth,
        height: 1,
        depth: 1,
        mips: 1,
        array: 1,
        stride: d.StructureByteStride,
        usage: d.Usage.0 as u32,
        bind: d.BindFlags,
        cpu: d.CPUAccessFlags,
        misc: d.MiscFlags,
        desc: bytes_of(d),
        ..Default::default()
    }
}

pub fn texture1d_dims(d: &D3D11_TEXTURE1D_DESC) -> Dims {
    Dims {
        kind: "texture1d",
        width: d.Width,
        height: 1,
        depth: 1,
        mips: d.MipLevels,
        array: d.ArraySize,
        format: d.Format.0 as u32,
        samples: 1,
        usage: d.Usage.0 as u32,
        bind: d.BindFlags,
        cpu: d.CPUAccessFlags,
        misc: d.MiscFlags,
        desc: bytes_of(d),
        ..Default::default()
    }
}

pub fn texture2d_dims(d: &D3D11_TEXTURE2D_DESC) -> Dims {
    Dims {
        kind: "texture2d",
        width: d.Width,
        height: d.Height,
        depth: 1,
        mips: d.MipLevels,
        array: d.ArraySize,
        format: d.Format.0 as u32,
        samples: d.SampleDesc.Count,
        usage: d.Usage.0 as u32,
        bind: d.BindFlags,
        cpu: d.CPUAccessFlags,
        misc: d.MiscFlags,
        desc: bytes_of(d),
        ..Default::default()
    }
}

pub fn texture3d_dims(d: &D3D11_TEXTURE3D_DESC) -> Dims {
    Dims {
        kind: "texture3d",
        width: d.Width,
        height: d.Height,
        depth: d.Depth,
        mips: d.MipLevels,
        array: 1,
        format: d.Format.0 as u32,
        samples: 1,
        usage: d.Usage.0 as u32,
        bind: d.BindFlags,
        cpu: d.CPUAccessFlags,
        misc: d.MiscFlags,
        desc: bytes_of(d),
        ..Default::default()
    }
}

/// The description of a live resource, with a mip count of 0 resolved to the full chain the runtime made.
pub fn dims_of(resource: &ID3D11Resource) -> Option<Dims> {
    unsafe {
        if let Ok(t) = resource.cast::<ID3D11Texture2D>() {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            t.GetDesc(&mut d);
            return Some(texture2d_dims(&d));
        }
        if let Ok(b) = resource.cast::<ID3D11Buffer>() {
            let mut d = D3D11_BUFFER_DESC::default();
            b.GetDesc(&mut d);
            return Some(buffer_dims(&d));
        }
        if let Ok(t) = resource.cast::<ID3D11Texture3D>() {
            let mut d = D3D11_TEXTURE3D_DESC::default();
            t.GetDesc(&mut d);
            return Some(texture3d_dims(&d));
        }
        if let Ok(t) = resource.cast::<ID3D11Texture1D>() {
            let mut d = D3D11_TEXTURE1D_DESC::default();
            t.GetDesc(&mut d);
            return Some(texture1d_dims(&d));
        }
    }
    None
}

#[derive(Clone, Debug, Default)]
pub struct Entry {
    pub dims: Dims,
    /// Its subresources in the canonical layout (none for a format without one).
    pub subs: Vec<Sub>,
    pub created_frame: Option<u64>,
    pub created_stack: Vec<u64>,
    /// Key of the data it was created with, and whether that data is in the blob store.
    pub upload: Option<String>,
    pub upload_stored: bool,
    /// Key of each subresource's bytes as last uploaded whole (by creation or an update), by subresource index.
    pub sub_hashes: Vec<Option<String>>,
    pub updated_frame: Option<u64>,
    pub updated_stack: Vec<u64>,
    pub update_count: u64,
}

static ENTRIES: Mutex<Option<HashMap<u64, Entry>>> = Mutex::new(None);

fn entries() -> std::sync::MutexGuard<'static, Option<HashMap<u64, Entry>>> {
    ENTRIES.lock().unwrap_or_else(|e| e.into_inner())
}

/// The entry of the resource numbered `id`, if it is alive.
pub fn entry(id: u64) -> Option<Entry> {
    entries().as_ref()?.get(&id).cloned()
}

/// What the resource numbered `id` is (`Dims::kind`) and its subresource `sub` (None for a format without a
/// canonical layout), without copying its entry.
pub fn sub_of(id: u64, sub: u32) -> Option<(&'static str, Option<Sub>)> {
    let entries = entries();
    let e = entries.as_ref()?.get(&id)?;
    Some((e.dims.kind, e.subs.get(sub as usize).copied()))
}

/// Every live resource's number.
pub fn live() -> Vec<u64> {
    entries()
        .as_ref()
        .map_or_else(Vec::new, |e| e.keys().copied().collect())
}

unsafe extern "system" fn destroyed(data: *mut c_void) {
    if let Some(e) = entries().as_mut() {
        e.remove(&(data as u64));
    }
}

/// The number on `object`, if it has one.
pub fn id_on(object: &ID3D11DeviceChild) -> Option<(u64, u64)> {
    let mut data = [0u64; 2];
    let mut size = 16u32;
    let ok = unsafe { object.GetPrivateData(&ID_KEY, &mut size, Some(data.as_mut_ptr().cast())) }.is_ok();
    (ok && size == 16).then_some((data[0], data[1]))
}

pub fn set_id(object: &ID3D11DeviceChild, id: u64, capture: u64) {
    let data = [id, capture];
    let _ = unsafe { object.SetPrivateData(&ID_KEY, 16, Some(data.as_ptr().cast())) };
}

/// Registers a resource: gives it a number and an entry, and watches for its destruction. `initial` is the data it
/// was created with (one per subresource), if any.
/// Resources registered (with the initial data hashed and handed to the writer), and updates seen, on the game's
/// threads.
pub static REGISTERED: Cost = Cost::new();
pub static UPDATES: Cost = Cost::new();

unsafe fn register(
    resource: &ID3D11Resource,
    dims: Dims,
    initial: *const D3D11_SUBRESOURCE_DATA,
    created: bool,
) -> u64 {
    let child: ID3D11DeviceChild = resource.cast().expect("a resource is a device child");
    if let Some((id, _)) = id_on(&child) {
        return id;
    }
    let started = std::time::Instant::now();
    let mut hashed = 0u64;
    let id = next_id();
    set_id(&child, id, 0);
    let mut entry = Entry {
        created_frame: created.then(crate::dxgi::frame),
        created_stack: if created { stacks::capture() } else { Vec::new() },
        ..Default::default()
    };
    if let Some(subs) = dims.subs() {
        entry.sub_hashes = vec![None; subs.len()];
        if !initial.is_null() && settings().uploads != Uploads::Off {
            let (bytes, hashes) = unsafe { pack_initial(&dims, &subs, initial) };
            hashed = bytes.len() as u64;
            let whole = key(&bytes);
            entry.sub_hashes = hashes.into_iter().map(Some).collect();
            entry.upload_stored = settings().uploads == Uploads::Store
                && writer::blob(&settings().blobs, whole.clone(), bytes, false);
            entry.upload = Some(whole);
        }
        entry.subs = subs;
    }
    entry.dims = dims;
    entries().get_or_insert_with(HashMap::new).insert(id, entry);
    match resource.cast::<ID3DDestructionNotifier>() {
        Ok(n) => {
            let _ = unsafe { n.RegisterDestructionCallback(Some(destroyed), id as *const c_void) };
        }
        Err(e) => {
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| log!("resources have no ID3DDestructionNotifier ({e}): dead ones stay listed"));
        }
    }
    REGISTERED.add(started, hashed);
    id
}

/// The canonical bytes of initial data, and each subresource's key.
unsafe fn pack_initial(
    dims: &Dims,
    subs: &[Sub],
    initial: *const D3D11_SUBRESOURCE_DATA,
) -> (Vec<u8>, Vec<String>) {
    let mut bytes = Vec::with_capacity(layout::total(subs) as usize);
    let mut hashes = Vec::with_capacity(subs.len());
    for (i, sub) in subs.iter().enumerate() {
        let d = unsafe { &*initial.add(i) };
        let start = bytes.len();
        let (row, slice) = if dims.kind == "buffer" || dims.kind == "texture1d" {
            (sub.row_bytes as usize, sub.slice_bytes() as usize)
        } else {
            (d.SysMemPitch as usize, d.SysMemSlicePitch as usize)
        };
        unsafe { layout::pack(sub, d.pSysMem.cast(), row, slice, &mut bytes) };
        hashes.push(key(&bytes[start..]));
    }
    (bytes, hashes)
}

/// After a `Create*` of a resource: registers it if it was made.
///
/// # Safety
/// `out` is the call's out pointer; `initial` is the call's initial data (null or one per subresource).
pub unsafe fn created(
    hr: windows::core::HRESULT,
    out: *mut *mut c_void,
    initial: *const D3D11_SUBRESOURCE_DATA,
) {
    if hr.is_err() || out.is_null() || unsafe { (*out).is_null() } {
        return;
    }
    let Some(resource) = (unsafe { ID3D11Resource::from_raw_borrowed(&*out) }) else {
        return;
    };
    // The description from the object, so a mip count of 0 is the chain the runtime made.
    if let Some(dims) = dims_of(resource) {
        unsafe { register(resource, dims, initial, true) };
    }
}

/// The number of a resource locum did not see created (a swap chain's buffers, a shared resource), registering it.
pub fn id_of(resource: &ID3D11Resource) -> u64 {
    let child: ID3D11DeviceChild = resource.cast().expect("a resource is a device child");
    if let Some((id, _)) = id_on(&child) {
        return id;
    }
    match dims_of(resource) {
        Some(dims) => unsafe { register(resource, dims, core::ptr::null(), false) },
        None => {
            let id = next_id();
            set_id(&child, id, 0);
            id
        }
    }
}

/// Extra source offset of legacy UpdateSubresource on an emulated deferred context.
/// See https://learn.microsoft.com/en-us/windows/win32/api/d3d11/nf-d3d11-id3d11devicecontext-updatesubresource
pub(super) fn source_offset(dims: &Dims, region: &D3D11_BOX, row: u32, depth: u32) -> Option<usize> {
    if dims.kind == "buffer" {
        return Some(region.left as usize);
    }
    let format = locum_capture::format::layout(dims.format)?;
    let x = (region.left / format.block_width) as usize * format.block_bytes as usize;
    let y = (region.top / format.block_height) as usize;
    (region.front as usize)
        .checked_mul(depth as usize)?
        .checked_add(y.checked_mul(row as usize)?)?
        .checked_add(x)
}

/// An `UpdateSubresource`: counted on the resource's entry, and hashed (and stored) when it covers a whole
/// subresource. Returns the canonical bytes of the region and their key, for a capture.
///
/// # Safety
/// `data` must hold the region `region` (or the whole subresource) with the given pitches.
pub unsafe fn updated(
    resource: &ID3D11Resource,
    sub: u32,
    region: Option<&D3D11_BOX>,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
    keep: bool,
) -> Option<(Vec<u8>, String)> {
    let started = std::time::Instant::now();
    let (out, hashed) = unsafe { update_entry(resource, sub, region, data, row_pitch, depth_pitch, keep) };
    UPDATES.add(started, hashed);
    out
}

#[allow(clippy::too_many_arguments)]
unsafe fn update_entry(
    resource: &ID3D11Resource,
    sub: u32,
    region: Option<&D3D11_BOX>,
    data: *const c_void,
    row_pitch: u32,
    depth_pitch: u32,
    keep: bool,
) -> (Option<(Vec<u8>, String)>, u64) {
    let id = id_of(resource);
    // Counted, and what the rest needs taken, under one lock.
    let (kind, format, whole, per_frame) = {
        let mut entries = entries();
        let Some(e) = entries.as_mut().and_then(|e| e.get_mut(&id)) else {
            return (None, 0);
        };
        let frame = crate::dxgi::frame();
        let per_frame = e.dims.bind & D3D11_BIND_CONSTANT_BUFFER.0 as u32 != 0
            || e.updated_frame.is_some_and(|f| f + 1 >= frame);
        e.update_count += 1;
        e.updated_frame = Some(frame);
        if per_frame && !keep {
            // Its bytes are unknown from here; a capture reads them back.
            if let Some(slot) = e.sub_hashes.get_mut(sub as usize) {
                *slot = None;
            }
            return (None, 0);
        }
        let Some(&whole) = e.subs.get(sub as usize) else {
            return (None, 0);
        };
        (e.dims.kind, e.dims.format, whole, per_frame)
    };
    let f = locum_capture::format::layout(format).unwrap_or(locum_capture::format::Layout {
        block_width: 1,
        block_height: 1,
        block_bytes: 1,
    });
    // The region as a subresource of its own.
    let part = match region {
        None => whole,
        Some(b) if kind == "buffer" => Sub {
            width: b.right.saturating_sub(b.left),
            row_bytes: b.right.saturating_sub(b.left),
            ..whole
        },
        Some(b) => {
            let (w, h, d) = (
                b.right.saturating_sub(b.left),
                b.bottom.saturating_sub(b.top),
                b.back.saturating_sub(b.front),
            );
            Sub {
                width: w,
                height: h,
                depth: d,
                row_bytes: f.row_bytes(w),
                rows: f.rows(h),
                size: u64::from(f.row_bytes(w)) * u64::from(f.rows(h)) * u64::from(d),
                ..whole
            }
        }
    };
    let covers = match region {
        None => true,
        Some(b) => {
            b.left == 0
                && b.top == 0
                && b.front == 0
                && part.row_bytes == whole.row_bytes
                && part.rows == whole.rows
                && part.depth == whole.depth
        }
    };
    let (row, slice) = if kind == "buffer" || kind == "texture1d" {
        (part.row_bytes as usize, part.slice_bytes() as usize)
    } else {
        (row_pitch as usize, depth_pitch as usize)
    };
    // Stored for the join with files: whole uploads of what is not per-frame data.
    let store = covers && !per_frame && settings().uploads == Uploads::Store;
    let hashing = covers && settings().uploads != Uploads::Off;
    let mut out = None;
    let mut new_hash = None;
    let mut hashed = 0;
    if keep || hashing {
        let mut bytes = Vec::with_capacity(part.size as usize);
        unsafe { layout::pack(&part, data.cast(), row, slice, &mut bytes) };
        hashed = bytes.len() as u64;
        let k = key(&bytes);
        if covers {
            new_hash = Some(k.clone());
        }
        if keep {
            if store {
                writer::blob(&settings().blobs, k.clone(), bytes.clone(), false);
            }
            out = Some((bytes, k));
        } else if store {
            writer::blob(&settings().blobs, k, bytes, false);
        }
    }
    // Per-frame data's updates are a capture's events, each with its own stack.
    let stack = (!per_frame).then(stacks::capture);
    if let Some(e) = entries().as_mut().and_then(|e| e.get_mut(&id)) {
        if let Some(stack) = stack {
            e.updated_stack = stack;
        }
        if let Some(slot) = e.sub_hashes.get_mut(sub as usize) {
            // A partial update leaves the subresource's bytes unknown.
            *slot = new_hash;
        }
    }
    (out, hashed)
}

#[cfg(test)]
mod source_offset_tests {
    use super::*;

    #[test]
    fn offsets_use_bytes_blocks_and_volume_pitches() {
        let region = D3D11_BOX {
            left: 4,
            top: 8,
            front: 2,
            right: 8,
            bottom: 12,
            back: 3,
        };
        let dims = |kind, format| Dims {
            kind,
            format,
            ..Default::default()
        };
        assert_eq!(source_offset(&dims("buffer", 0), &region, 99, 999), Some(4));
        assert_eq!(
            source_offset(&dims("texture3d", 28), &region, 64, 1024),
            Some(2576)
        );
        assert_eq!(source_offset(&dims("texture3d", 71), &region, 32, 256), Some(584));
        assert_eq!(
            source_offset(&dims("texture3d", 77), &region, 64, 512),
            Some(1168)
        );
        assert_eq!(source_offset(&dims("texture2d", 103), &region, 64, 0), None);
    }
}
