//! A deferred list's recorded calls laid end to end in 8-byte cells: each call's tag, then its arguments, arrays
//! inline. Object pointers are written inline too, so a replayed call reads its object arrays where they lie, and an
//! owner list holds a reference to each. Once the buffers have grown, recording a call allocates nothing, and
//! dropping a list's tape frees a few buffers rather than one allocation per call.
use super::uploads::Upload;
use core::ffi::c_void;
use core::mem::size_of;
use windows::core::{IUnknown, Interface};

/// The tag of an upload between recorded calls (see [`super::uploads`]).
const UPLOAD: u32 = u32::MAX;
/// An array's or object array's header for a null pointer.
const NULL: u64 = u64::MAX;

struct Held([usize; 64]);
impl Default for Held {
    fn default() -> Held {
        Held([0; 64])
    }
}

#[derive(Default)]
pub struct Tape {
    cells: Vec<u64>,
    /// A reference to every object the cells name, kept while the tape lives.
    owners: Vec<IUnknown>,
    /// Objects recently given an owner, by a hash of their address: a call naming one again needs no reference of
    /// its own. An address cannot be reused while the tape holds its object.
    held: Held,
    uploads: Vec<Upload>,
    /// The tag of the last call written.
    last: Option<u32>,
}

// The objects are D3D11 device children, free-threaded; the caller serializes replay on its context.
unsafe impl Send for Tape {}
unsafe impl Sync for Tape {}

impl Tape {
    /// An empty tape with room for what `previous`, the list recorded before it, held.
    pub fn sized_like(previous: &Tape) -> Tape {
        Tape {
            cells: Vec::with_capacity(previous.cells.len()),
            owners: Vec::with_capacity(previous.owners.len()),
            ..Tape::default()
        }
    }

    pub fn last(&self) -> Option<u32> {
        self.last
    }

    pub(crate) fn tag(&mut self, tag: u32) {
        self.cells.push(u64::from(tag));
        self.last = Some(tag);
    }

    /// Writes `count` values of `T` from `p` into whole cells.
    unsafe fn bytes<T: Copy>(&mut self, p: *const T, count: usize) {
        let bytes = count * size_of::<T>();
        let at = self.cells.len();
        self.cells.resize(at + bytes.div_ceil(8), 0);
        unsafe {
            core::ptr::copy_nonoverlapping(p.cast::<u8>(), self.cells[at..].as_mut_ptr().cast::<u8>(), bytes)
        };
    }

    pub(crate) fn put<T: Copy>(&mut self, value: T) {
        unsafe { self.bytes(&value, 1) };
    }

    /// # Safety
    /// `p` is null or points at `count` values.
    pub(crate) unsafe fn array<T: Copy>(&mut self, p: *const T, count: u32) {
        if p.is_null() {
            self.cells.push(NULL);
        } else {
            self.cells.push(u64::from(count));
            unsafe { self.bytes(p, count as usize) };
        }
    }

    /// # Safety
    /// `p` is null or a live COM interface.
    pub(crate) unsafe fn object(&mut self, p: *mut c_void) {
        self.cells.push(p as u64);
        let slot = &mut self.held.0[(p as usize >> 4) % 64];
        if *slot == p as usize {
            return;
        }
        if let Some(o) = unsafe { IUnknown::from_raw_borrowed(&p) } {
            self.owners.push(o.clone());
            *slot = p as usize;
        }
    }

    /// # Safety
    /// `p` is null or points at `count` live or null COM interface pointers.
    pub(crate) unsafe fn objects(&mut self, p: *const *mut c_void, count: u32) {
        if p.is_null() {
            self.cells.push(NULL);
            return;
        }
        self.cells.push(u64::from(count));
        let objects = unsafe { core::slice::from_raw_parts(p, count as usize) };
        for &o in objects {
            unsafe { self.object(o) };
        }
    }

    pub(super) fn upload(&mut self, upload: Upload) {
        self.cells.push(u64::from(UPLOAD));
        self.cells.push(self.uploads.len() as u64);
        self.uploads.push(upload);
        self.last = Some(UPLOAD);
    }

    /// Appends a tape of calls, without uploads, after this one's.
    pub fn append(&mut self, other: &Tape) {
        debug_assert!(other.uploads.is_empty(), "appending a tape with uploads");
        self.cells.extend_from_slice(&other.cells);
        self.owners.extend(other.owners.iter().cloned());
        self.held = Held::default();
        self.last = other.last.or(self.last);
    }

    /// # Safety
    /// The context is an empty private deferred context with recursive recording disabled.
    pub unsafe fn replay(&self, context: *mut c_void) -> windows::core::Result<()> {
        let mut r = Reader {
            cells: &self.cells,
            at: 0,
        };
        while r.at < self.cells.len() {
            let tag: u32 = r.get();
            if tag == UPLOAD {
                let index: u64 = r.get();
                unsafe { self.uploads[index as usize].replay(context) }?;
            } else {
                unsafe { crate::vtables::recorded_context::call(tag, &mut r, context) }?;
            }
        }
        Ok(())
    }
}

/// Reads a tape's calls back, in the order they were written.
pub struct Reader<'a> {
    cells: &'a [u64],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, bytes: usize) -> *const u64 {
        let p = self.cells[self.at..].as_ptr();
        self.at += bytes.div_ceil(8);
        p
    }

    pub(crate) fn get<T: Copy>(&mut self) -> T {
        // Cells are 8-byte aligned and every argument type aligns to at most 8.
        unsafe { self.take(size_of::<T>()).cast::<T>().read() }
    }

    pub(crate) fn array<T: Copy>(&mut self) -> *const T {
        match self.get::<u64>() {
            NULL => core::ptr::null(),
            count => self.take(count as usize * size_of::<T>()).cast(),
        }
    }

    pub(crate) fn object(&mut self) -> *mut c_void {
        self.get::<u64>() as *mut c_void
    }

    pub(crate) fn objects(&mut self) -> *const *mut c_void {
        match self.get::<u64>() {
            NULL => core::ptr::null(),
            count => self.take(count as usize * 8).cast(),
        }
    }
}
