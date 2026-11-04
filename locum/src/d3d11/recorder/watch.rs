//! A reusable, lazily backed map shadow. Only dirty pages are inspected at Unmap.
use std::{collections::HashMap, ffi::c_void, ptr::NonNull, sync::OnceLock};
use windows::Win32::System::{
    Memory::{
        GetWriteWatch, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, ResetWriteWatch,
        VIRTUAL_ALLOCATION_TYPE, VirtualAlloc, VirtualFree,
    },
    SystemInformation::{GetSystemInfo, SYSTEM_INFO},
    SystemServices::{MEM_WRITE_WATCH, WRITE_WATCH_FLAG_RESET},
};

pub(super) struct Write {
    pub offset: usize,
    pub bytes: Vec<u8>,
}
impl Write {
    pub fn push(writes: &mut Vec<Self>, offset: usize, bytes: &[u8]) {
        if let Some(last) = writes.last_mut().filter(|w| w.offset + w.bytes.len() == offset) {
            last.bytes.extend_from_slice(bytes);
        } else {
            writes.push(Self {
                offset,
                bytes: bytes.into(),
            });
        }
    }
}

pub(super) struct Shadow {
    pointer: NonNull<c_void>,
    pub size: usize,
    page: usize,
    addresses: Vec<*mut c_void>,
    /// Last committed bytes of pages written since DISCARD. Never clone the whole resource.
    previous: HashMap<usize, Box<[u8]>>,
}
impl Shadow {
    pub fn new(size: usize) -> Result<Self, &'static str> {
        static PAGE: OnceLock<usize> = OnceLock::new();
        let page = *PAGE.get_or_init(|| {
            let mut info = SYSTEM_INFO::default();
            unsafe { GetSystemInfo(&mut info) };
            info.dwPageSize as usize
        });
        if size == 0 || page == 0 {
            return Err("invalid write-watch shadow size");
        }
        let pointer = NonNull::new(unsafe {
            VirtualAlloc(
                None,
                size,
                MEM_COMMIT | MEM_RESERVE | VIRTUAL_ALLOCATION_TYPE(MEM_WRITE_WATCH),
                PAGE_READWRITE,
            )
        })
        .ok_or("VirtualAlloc write-watch shadow failed")?;
        Ok(Self {
            pointer,
            size,
            page,
            addresses: vec![std::ptr::null_mut(); size.div_ceil(page)],
            previous: HashMap::new(),
        })
    }
    pub fn pointer(&self) -> *mut u8 {
        self.pointer.as_ptr().cast()
    }
    pub fn discard(&mut self) {
        // Bytes not written after DISCARD are undefined. Reuse the allocation without clearing or
        // touching them. Only the pages actually written become defined in the new driver allocation.
        self.previous.clear();
    }

    /// # Safety
    /// No application thread may access the mapped pointer while Unmap collects its writes.
    pub unsafe fn changes(&mut self) -> (Vec<Write>, bool) {
        let mut count = self.addresses.len();
        let mut granularity = 0;
        let result = unsafe {
            GetWriteWatch(
                WRITE_WATCH_FLAG_RESET,
                self.pointer.as_ptr(),
                self.size,
                Some(self.addresses.as_mut_ptr()),
                Some(&mut count),
                Some(&mut granularity),
            )
        };
        if result == 0 && count == 0 {
            return (Vec::new(), false);
        }
        let base = self.pointer.as_ptr() as usize;
        let valid = result == 0
            && count <= self.addresses.len()
            && granularity as usize == self.page
            && self.addresses[..count].iter().all(|&p| {
                (p as usize)
                    .checked_sub(base)
                    .is_some_and(|n| n < self.size && n.is_multiple_of(self.page))
            });
        let mut offsets: Vec<_> = if valid {
            self.addresses[..count]
                .iter()
                .map(|&p| p as usize - base)
                .collect()
        } else {
            // Preserve the application's writes even if the observation API fails. This rare path
            // scans the shadow, emits a reconstruction gap, and keeps the original command list.
            let _ = unsafe { ResetWriteWatch(self.pointer.as_ptr(), self.size) };
            (0..self.size).step_by(self.page).collect()
        };
        offsets.sort_unstable();
        let mut writes = Vec::new();
        for offset in offsets {
            let len = self.page.min(self.size - offset);
            let bytes = unsafe { std::slice::from_raw_parts(self.pointer().add(offset), len) };
            if let Some(previous) = self.previous.get(&offset) {
                // Page dirtiness is not a license to rewrite the page: NO_OVERWRITE may append
                // beside GPU-referenced bytes on that same page. Copy only changed byte runs.
                let mut i = 0;
                while i < len {
                    if bytes[i] == previous[i] {
                        i += 1;
                        continue;
                    }
                    let start = i;
                    while i < len && bytes[i] != previous[i] {
                        i += 1;
                    }
                    Write::push(&mut writes, offset + start, &bytes[start..i]);
                }
            } else {
                Write::push(&mut writes, offset, bytes);
            }
            if let Some(previous) = self.previous.get_mut(&offset) {
                previous.copy_from_slice(bytes);
            } else {
                self.previous.insert(offset, bytes.into());
            }
        }
        (writes, !valid)
    }
}
impl Drop for Shadow {
    fn drop(&mut self) {
        let _ = unsafe { VirtualFree(self.pointer.as_ptr(), 0, MEM_RELEASE) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_writes_and_same_page_append_keep_old_bytes_out_of_the_copy() {
        let mut shadow = Shadow::new(4 << 20).unwrap();
        unsafe { std::ptr::write_bytes(shadow.pointer().add(32), 7, 64) };
        let (first, failed) = unsafe { shadow.changes() };
        assert!(!failed);
        assert_eq!(first.iter().map(|w| w.bytes.len()).sum::<usize>(), shadow.page);
        unsafe { std::ptr::write_bytes(shadow.pointer().add(96), 9, 64) };
        let (next, failed) = unsafe { shadow.changes() };
        assert!(!failed);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].offset, 96);
        assert_eq!(next[0].bytes, vec![9; 64]);
        assert!(unsafe { shadow.changes() }.0.is_empty());
        shadow.discard();
        unsafe { std::ptr::write_bytes(shadow.pointer().add(shadow.page * 31 + 11), 3, 64) };
        let (discard, failed) = unsafe { shadow.changes() };
        assert!(!failed);
        assert_eq!(discard.len(), 1);
        assert_eq!(discard[0].offset, shadow.page * 31);
        assert_eq!(discard[0].bytes.len(), shadow.page);
    }
}
