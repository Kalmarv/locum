//! GPU time of each replayed draw and dispatch, when `--times` names a file: a timestamp before and after
//! the call (inside the barriers replay puts around it), resolved when the open list runs, and a tab-separated row per
//! op appended to the file: event, draw, call, its arguments, its program (the compute shader's, else the pixel or
//! vertex shader's), and the microseconds between the two timestamps. A pair recorded past the heap's room is not
//! timed (counted on stderr).

use std::io::Write;
use std::path::PathBuf;
use std::sync::OnceLock;

use windows::Win32::Graphics::Direct3D12::*;

/// The file `--times` names, for every replay this process makes.
static TIMES: OnceLock<PathBuf> = OnceLock::new();

/// Each replay after this appends its ops' times to `path`.
pub fn time_to(path: PathBuf) {
    let _ = TIMES.set(path);
}

/// Timestamp pairs one list holds.
const PAIRS: u32 = 1 << 16;

pub(super) struct Timing {
    heap: ID3D12QueryHeap,
    readback: ID3D12Resource,
    /// ticks per second of the queue's clock
    frequency: u64,
    /// the open list's pairs: what each times
    marks: Vec<String>,
    open: bool,
    dropped: u64,
    out: std::fs::File,
}

impl Timing {
    /// None unless `--times` named a file; an error when it did and the file or queries cannot be made.
    pub(super) fn new(device: &ID3D12Device2, queue: &ID3D12CommandQueue) -> Result<Option<Timing>, String> {
        let Some(path) = TIMES.get() else {
            return Ok(None);
        };
        let out = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("--times {}: {e}", path.display()))?;
        let mut heap: Option<ID3D12QueryHeap> = None;
        let mut readback: Option<ID3D12Resource> = None;
        let frequency = unsafe {
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 2 * PAIRS,
                        NodeMask: 0,
                    },
                    &mut heap,
                )
                .map_err(|e| format!("timestamp heap: {e}"))?;
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_READBACK,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: 16 * PAIRS as u64,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .map_err(|e| format!("timestamp readback: {e}"))?;
            queue
                .GetTimestampFrequency()
                .map_err(|e| format!("GetTimestampFrequency: {e}"))?
        };
        Ok(Some(Timing {
            heap: heap.ok_or("no timestamp heap")?,
            readback: readback.ok_or("no timestamp readback")?,
            frequency,
            marks: Vec::new(),
            open: false,
            dropped: 0,
            out,
        }))
    }

    /// Before the call: what it is, for its row.
    pub(super) fn begin(&mut self, list: &ID3D12GraphicsCommandList, what: String) {
        if self.marks.len() as u32 >= PAIRS {
            self.dropped += 1;
            return;
        }
        unsafe {
            list.EndQuery(
                &self.heap,
                D3D12_QUERY_TYPE_TIMESTAMP,
                2 * self.marks.len() as u32,
            )
        };
        self.marks.push(what);
        self.open = true;
    }

    /// After the call (nothing when its begin was not timed).
    pub(super) fn end(&mut self, list: &ID3D12GraphicsCommandList) {
        if !std::mem::take(&mut self.open) {
            return;
        }
        unsafe {
            list.EndQuery(
                &self.heap,
                D3D12_QUERY_TYPE_TIMESTAMP,
                2 * self.marks.len() as u32 - 1,
            )
        };
    }

    /// Before the open list closes: its pairs resolved into the readback buffer.
    pub(super) fn resolve(&mut self, list: &ID3D12GraphicsCommandList) {
        if !self.marks.is_empty() {
            unsafe {
                list.ResolveQueryData(
                    &self.heap,
                    D3D12_QUERY_TYPE_TIMESTAMP,
                    0,
                    2 * self.marks.len() as u32,
                    &self.readback,
                    0,
                )
            };
        }
    }

    /// Once the list has run (`ran`): a row per pair; the pairs are gone either way.
    pub(super) fn write(&mut self, ran: bool) -> Result<(), String> {
        let marks = std::mem::take(&mut self.marks);
        self.open = false;
        if marks.is_empty() || !ran {
            return Ok(());
        }
        let mut p = core::ptr::null_mut();
        let range = D3D12_RANGE {
            Begin: 0,
            End: 16 * marks.len(),
        };
        let ticks: Vec<u64> = unsafe {
            self.readback
                .Map(0, Some(&range), Some(&mut p))
                .map_err(|e| format!("timestamp readback Map: {e}"))?;
            let t = std::slice::from_raw_parts(p.cast::<u64>(), 2 * marks.len()).to_vec();
            self.readback.Unmap(0, Some(&D3D12_RANGE::default()));
            t
        };
        let mut rows = String::new();
        for (i, what) in marks.iter().enumerate() {
            let us = ticks[2 * i + 1].saturating_sub(ticks[2 * i]) as f64 * 1e6 / self.frequency as f64;
            rows += &format!("{what}\t{us:.1}\n");
        }
        self.out
            .write_all(rows.as_bytes())
            .map_err(|e| format!("--times: {e}"))
    }
}

impl Drop for Timing {
    fn drop(&mut self) {
        if self.dropped > 0 {
            eprintln!(
                "--times: {} op(s) past {PAIRS} in one list were not timed",
                self.dropped
            );
        }
    }
}
