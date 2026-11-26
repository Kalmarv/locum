//! The sidecar a tool beside the game may write (see [`locum_capture::sidecar`]), read on the writer thread as a
//! capture is written: its rows go into the capture as they are, and each region is copied from this process's memory
//! then, after the frame. A region is stored as a pieced blob: runs of pages of at most a readback chunk, and its pages
//! of zeros, or that could not be read, as zeros stored nowhere.
use locum_capture::blobs::{Blobs, ZEROS};
use locum_capture::sidecar::{Captured, Copied, Region};
use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows::Win32::System::Threading::GetCurrentProcess;

use crate::log::log;

const PAGE: u64 = 64 << 10;

/// The sidecar the settings name, its regions copied into `blobs`, and its file; None when there is none.
pub fn captured(blobs: &Blobs) -> Option<(String, Captured)> {
    let s = crate::settings::settings();
    let file = &s.sidecar;
    let text = std::fs::read_to_string(file).ok()?;
    let (sidecar, warnings) = locum_capture::sidecar::parse(&text);
    for w in warnings.iter().take(10) {
        log!("{}: {w}", file.display());
    }
    let mut left = s.sidecar_regions;
    let copied = sidecar
        .regions
        .iter()
        .map(|r| {
            if r.bytes > left {
                return Copied {
                    content: None,
                    note: Some(format!(
                        "not copied: past sidecar-region-mb ({} MiB for all regions)",
                        s.sidecar_regions >> 20
                    )),
                };
            }
            left -= r.bytes;
            copy(r, blobs, s.readback_chunk)
        })
        .collect();
    log!(
        "sidecar {}: {} regions, {} visible ranges, {} descriptors",
        file.display(),
        sidecar.regions.len(),
        sidecar.visible.len(),
        sidecar.descriptors.len()
    );
    Some((file.display().to_string(), Captured { sidecar, copied }))
}

/// Region `r` read from this process, in pieces of at most `chunk` bytes.
fn copy(r: &Region, blobs: &Blobs, chunk: u64) -> Copied {
    let mut pieces: Vec<(String, u64)> = Vec::new();
    let mut run: Vec<u8> = Vec::new();
    let mut unreadable = 0u64;
    let mut failed = None;
    let mut flush = |run: &mut Vec<u8>, pieces: &mut Vec<(String, u64)>| {
        if run.is_empty() {
            return;
        }
        let key = locum_capture::hash::key(run);
        if let Err(e) = blobs.put(&key, run) {
            failed.get_or_insert(e.to_string());
        }
        pieces.push((key, run.len() as u64));
        run.clear();
    };
    let zeros = |pieces: &mut Vec<(String, u64)>, n: u64| match pieces.last_mut() {
        Some((k, size)) if k == ZEROS => *size += n,
        _ => pieces.push((ZEROS.into(), n)),
    };
    let mut page = vec![0u8; PAGE as usize];
    let mut at = 0;
    while at < r.bytes {
        let n = PAGE.min(r.bytes - at);
        let buf = &mut page[..n as usize];
        let mut read = 0usize;
        let ok = unsafe {
            ReadProcessMemory(
                GetCurrentProcess(),
                (r.address + at) as *const core::ffi::c_void,
                buf.as_mut_ptr().cast(),
                buf.len(),
                Some(&mut read),
            )
        }
        .is_ok()
            && read == buf.len();
        if !ok {
            unreadable += n;
        }
        if !ok || buf.iter().all(|&b| b == 0) {
            flush(&mut run, &mut pieces);
            zeros(&mut pieces, n);
        } else {
            if run.len() as u64 + n > chunk {
                flush(&mut run, &mut pieces);
            }
            run.extend_from_slice(buf);
        }
        at += n;
    }
    flush(&mut run, &mut pieces);
    if let Some(e) = failed {
        return Copied {
            content: None,
            note: Some(format!("storing its bytes: {e}")),
        };
    }
    let key = locum_capture::blobs::pieces_key(&pieces);
    if let Err(e) = blobs.put_pieces(&key, &pieces) {
        return Copied {
            content: None,
            note: Some(format!("storing the list of its {} pieces: {e}", pieces.len())),
        };
    }
    let mut note = "read as the capture was written, after the frame".to_string();
    if unreadable > 0 {
        note += &format!(
            "; {} of its {} pages could not be read (zeros here)",
            unreadable.div_ceil(PAGE),
            r.bytes.div_ceil(PAGE)
        );
    }
    Copied {
        content: Some(key),
        note: Some(note),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_copy_as_pieces_with_unreadable_pages_as_zeros() {
        use windows::Win32::System::Memory::*;
        let dir = std::env::temp_dir().join(format!("locum-sidecar-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let blobs = Blobs::new(&dir);
        // Five pages reserved: 0 and 1 with data, 2 zeros, 3 not committed, 4 with data.
        let base = unsafe { VirtualAlloc(None, 5 * PAGE as usize, MEM_RESERVE, PAGE_NOACCESS) } as *mut u8;
        assert!(!base.is_null());
        for p in [0u64, 1, 2, 4] {
            let at = unsafe { base.add((p * PAGE) as usize) };
            let got = unsafe { VirtualAlloc(Some(at.cast()), PAGE as usize, MEM_COMMIT, PAGE_READWRITE) };
            assert!(!got.is_null());
            if p != 2 {
                unsafe { core::ptr::write_bytes(at, p as u8 + 1, PAGE as usize) };
            }
        }
        let r = Region {
            address: base as u64,
            bytes: 5 * PAGE,
            name: "test".into(),
        };
        // A chunk of one page: the first two data pages are two pieces.
        let c = copy(&r, &blobs, PAGE);
        let key = c.content.expect("copied");
        assert!(c.note.unwrap().contains("1 of its 5 pages could not be read"));
        let pieces = blobs.pieces(&key).unwrap().unwrap();
        let shape: Vec<(bool, u64)> = pieces.iter().map(|(k, n)| (k == ZEROS, n / PAGE)).collect();
        assert_eq!(shape, [(false, 1), (false, 1), (true, 2), (false, 1)]);
        let bytes = blobs.get(&key).unwrap();
        let expect: Vec<u8> = [1u8, 2, 0, 0, 5]
            .iter()
            .flat_map(|&v| std::iter::repeat_n(v, PAGE as usize))
            .collect();
        assert!(bytes == expect);
        unsafe { VirtualFree(base.cast(), 0, MEM_RELEASE) }.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
