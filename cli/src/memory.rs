//! The CPU side: `dump` writes a minidump of a process from a snapshot of it (run by locum at a capture), and
//! `memfind` and `memdiff` search such dumps: for values the GPU saw (a constant from a draw), floats, integers or
//! bytes, and for what changed between two dumps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use memmap2::Mmap;
use windows::Win32::Foundation::{CloseHandle, HANDLE, S_FALSE};
use windows::Win32::System::Diagnostics::Debug::{
    MINIDUMP_CALLBACK_INFORMATION, MINIDUMP_CALLBACK_INPUT, MINIDUMP_CALLBACK_OUTPUT, MINIDUMP_TYPE,
    MiniDumpIgnoreInaccessibleMemory, MiniDumpWithFullMemory, MiniDumpWithFullMemoryInfo,
    MiniDumpWithThreadInfo, MiniDumpWithUnloadedModules, MiniDumpWriteDump,
};
use windows::Win32::System::Diagnostics::ProcessSnapshotting::*;
use windows::Win32::System::Threading::{
    EVENT_MODIFY_STATE, OpenEventW, OpenProcess, PROCESS_ALL_ACCESS, SetEvent,
};
use windows::core::{BOOL, HSTRING};

use crate::describe::{Reflection, float, mib, register_name};
use crate::open::Capture;
use crate::out;

/// `IsProcessSnapshotCallback`: the dump is of a snapshot, not of the process.
const IS_PROCESS_SNAPSHOT_CALLBACK: u32 = 16;
/// CONTEXT_ALL on x64.
const CONTEXT_ALL: u32 = 0x0010_001f;

unsafe extern "system" fn snapshot_callback(
    _param: *mut core::ffi::c_void,
    input: *const MINIDUMP_CALLBACK_INPUT,
    output: *mut MINIDUMP_CALLBACK_OUTPUT,
) -> BOOL {
    let kind = unsafe { core::ptr::addr_of!((*input).CallbackType).read_unaligned() };
    if kind == IS_PROCESS_SNAPSHOT_CALLBACK {
        unsafe { core::ptr::addr_of_mut!((*output).Anonymous.Status).write_unaligned(S_FALSE) };
    }
    BOOL(1)
}

/// Writes a full-memory minidump of process `pid` to `out` (as `out.part` until complete), from a snapshot; signals
/// the event `signal` once the snapshot is taken, so the process can run on.
pub fn dump(pid: u32, out: &Path, signal: Option<&str>) -> Result<(), String> {
    let started = std::time::Instant::now();
    let process = unsafe { OpenProcess(PROCESS_ALL_ACCESS, false, pid) }
        .map_err(|e| format!("open process {pid}: {e}"))?;
    let mut snapshot = HPSS::default();
    // The set Microsoft's documentation gives for MiniDumpWriteDump on a snapshot.
    let flags = PSS_CAPTURE_VA_CLONE
        | PSS_CAPTURE_HANDLES
        | PSS_CAPTURE_HANDLE_NAME_INFORMATION
        | PSS_CAPTURE_HANDLE_BASIC_INFORMATION
        | PSS_CAPTURE_HANDLE_TYPE_SPECIFIC_INFORMATION
        | PSS_CAPTURE_HANDLE_TRACE
        | PSS_CAPTURE_THREADS
        | PSS_CAPTURE_THREAD_CONTEXT
        | PSS_CAPTURE_THREAD_CONTEXT_EXTENDED
        | PSS_CREATE_BREAKAWAY
        | PSS_CREATE_BREAKAWAY_OPTIONAL
        | PSS_CREATE_USE_VM_ALLOCATIONS
        | PSS_CREATE_RELEASE_SECTION;
    let err = unsafe { PssCaptureSnapshot(process, flags, Some(CONTEXT_ALL), &mut snapshot) };
    let signal_now = || {
        if let Some(name) = signal
            && let Ok(e) = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, &HSTRING::from(name)) }
        {
            let _ = unsafe { SetEvent(e) };
            let _ = unsafe { CloseHandle(e) };
        }
    };
    // Let the process go whatever happened: it waits for this.
    signal_now();
    if err != 0 {
        let _ = unsafe { CloseHandle(process) };
        return Err(format!("PssCaptureSnapshot: error {err}"));
    }
    let snapshot_ms = started.elapsed().as_millis();
    let part = PathBuf::from(format!("{}.part", out.display()));
    let file = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let callback = MINIDUMP_CALLBACK_INFORMATION {
        CallbackRoutine: Some(snapshot_callback),
        CallbackParam: core::ptr::null_mut(),
    };
    let kind = MINIDUMP_TYPE(
        MiniDumpWithFullMemory.0
            | MiniDumpWithFullMemoryInfo.0
            | MiniDumpWithThreadInfo.0
            | MiniDumpWithUnloadedModules.0
            | MiniDumpIgnoreInaccessibleMemory.0,
    );
    let written = unsafe {
        MiniDumpWriteDump(
            HANDLE(snapshot.0),
            pid,
            HANDLE(std::os::windows::io::AsRawHandle::as_raw_handle(&file)),
            kind,
            None,
            None,
            Some(&callback),
        )
    };
    drop(file);
    unsafe {
        PssFreeSnapshot(process, snapshot);
        let _ = CloseHandle(process);
    }
    written.map_err(|e| format!("MiniDumpWriteDump: {e}"))?;
    std::fs::rename(&part, out).map_err(|e| format!("{}: {e}", out.display()))?;
    let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    out!(
        "{}: {}; snapshot in {snapshot_ms} ms, written in {:.1} s",
        out.display(),
        mib(size),
        started.elapsed().as_secs_f64()
    );
    Ok(())
}

/// A region of the process's memory the dump holds.
struct Range {
    start: u64,
    size: u64,
    /// Where its bytes are in the file.
    offset: u64,
}

struct Module {
    base: u64,
    size: u64,
    name: String,
}

struct Info {
    base: u64,
    allocation: u64,
    size: u64,
    protect: u32,
    kind: u32,
}

/// A minidump, mapped.
pub struct Dump {
    map: Mmap,
    ranges: Vec<Range>,
    modules: Vec<Module>,
    info: Vec<Info>,
    stacks: Vec<(u32, u64, u64)>,
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

impl Dump {
    pub fn open(path: &Path) -> Result<Dump, String> {
        let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let map = unsafe { Mmap::map(&file) }.map_err(|e| format!("{}: {e}", path.display()))?;
        let b = &map[..];
        if b.get(..4) != Some(b"MDMP") {
            return Err(format!("{} is not a minidump", path.display()));
        }
        let bad = || format!("{}: damaged minidump", path.display());
        let streams = u32_at(b, 8).ok_or_else(bad)? as usize;
        let directory = u32_at(b, 12).ok_or_else(bad)? as usize;
        let (mut ranges, mut modules, mut info, mut stacks) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        for i in 0..streams {
            let e = directory + i * 12;
            let (kind, rva) = (
                u32_at(b, e).ok_or_else(bad)?,
                u32_at(b, e + 8).ok_or_else(bad)? as usize,
            );
            match kind {
                // Memory64ListStream
                9 => {
                    let n = u64_at(b, rva).ok_or_else(bad)? as usize;
                    let mut offset = u64_at(b, rva + 8).ok_or_else(bad)?;
                    for k in 0..n {
                        let d = rva + 16 + k * 16;
                        let (start, size) =
                            (u64_at(b, d).ok_or_else(bad)?, u64_at(b, d + 8).ok_or_else(bad)?);
                        ranges.push(Range { start, size, offset });
                        offset += size;
                    }
                }
                // ModuleListStream
                4 => {
                    let n = u32_at(b, rva).ok_or_else(bad)? as usize;
                    for k in 0..n {
                        let m = rva + 4 + k * 108;
                        let name_at = u32_at(b, m + 20).ok_or_else(bad)? as usize;
                        let len = u32_at(b, name_at).unwrap_or(0) as usize;
                        let units: Vec<u16> = b
                            .get(name_at + 4..name_at + 4 + len)
                            .unwrap_or(&[])
                            .as_chunks::<2>()
                            .0
                            .iter()
                            .map(|c| u16::from_le_bytes(*c))
                            .collect();
                        let path = String::from_utf16_lossy(&units);
                        modules.push(Module {
                            base: u64_at(b, m).ok_or_else(bad)?,
                            size: u64::from(u32_at(b, m + 8).ok_or_else(bad)?),
                            name: path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string(),
                        });
                    }
                }
                // MemoryInfoListStream
                16 => {
                    let header = u32_at(b, rva).ok_or_else(bad)? as usize;
                    let entry = u32_at(b, rva + 4).ok_or_else(bad)? as usize;
                    let n = u64_at(b, rva + 8).ok_or_else(bad)? as usize;
                    for k in 0..n {
                        let m = rva + header + k * entry;
                        info.push(Info {
                            base: u64_at(b, m).ok_or_else(bad)?,
                            allocation: u64_at(b, m + 8).ok_or_else(bad)?,
                            size: u64_at(b, m + 24).ok_or_else(bad)?,
                            protect: u32_at(b, m + 36).ok_or_else(bad)?,
                            kind: u32_at(b, m + 40).ok_or_else(bad)?,
                        });
                    }
                }
                // ThreadListStream
                3 => {
                    let n = u32_at(b, rva).ok_or_else(bad)? as usize;
                    for k in 0..n {
                        let t = rva + 4 + k * 48;
                        stacks.push((
                            u32_at(b, t).ok_or_else(bad)?,
                            u64_at(b, t + 24).ok_or_else(bad)?,
                            u64::from(u32_at(b, t + 32).ok_or_else(bad)?),
                        ));
                    }
                }
                _ => {}
            }
        }
        if ranges.is_empty() {
            return Err(format!("{} holds no full memory (Memory64List)", path.display()));
        }
        ranges.sort_by_key(|r| r.start);
        info.sort_by_key(|i| i.base);
        Ok(Dump {
            map,
            ranges,
            modules,
            info,
            stacks,
        })
    }

    fn bytes(&self, r: &Range) -> &[u8] {
        &self.map[r.offset as usize..(r.offset + r.size) as usize]
    }

    pub fn total(&self) -> u64 {
        self.ranges.iter().map(|r| r.size).sum()
    }

    /// `len` bytes at `address`, if the dump holds them.
    fn read(&self, address: u64, len: usize) -> Option<&[u8]> {
        let i = self
            .ranges
            .partition_point(|r| r.start <= address)
            .checked_sub(1)?;
        let r = &self.ranges[i];
        let at = (address - r.start) as usize;
        self.bytes(r).get(at..at + len)
    }

    /// What an address is: in a module's image, a thread's stack, or other memory, with its protection.
    fn describe(&self, address: u64) -> String {
        if let Some(m) = self
            .modules
            .iter()
            .find(|m| (m.base..m.base + m.size).contains(&address))
        {
            return format!("{}+0x{:x}", m.name, address - m.base);
        }
        if let Some((id, start, size)) = self
            .stacks
            .iter()
            .find(|(_, s, n)| (*s..s + n).contains(&address))
        {
            return format!("stack of thread {id} +0x{:x} of 0x{size:x}", address - start);
        }
        let i = self.info.partition_point(|i| i.base <= address);
        match i.checked_sub(1).map(|i| &self.info[i]) {
            Some(r) if address < r.base + r.size => {
                let kind = match r.kind {
                    0x100_0000 => "image",
                    0x4_0000 => "mapped",
                    _ => "private",
                };
                let protect = match r.protect & 0xff {
                    0x02 => "r",
                    0x04 => "rw",
                    0x08 => "rw (copy)",
                    0x10 => "x",
                    0x20 => "rx",
                    0x40 => "rwx",
                    _ => "?",
                };
                format!(
                    "{kind} {protect}, allocation 0x{:x} +0x{:x}",
                    r.allocation,
                    address - r.allocation
                )
            }
            _ => "unknown memory".into(),
        }
    }

    /// Every address where `needle` is, up to `limit`.
    fn find(&self, needle: &[u8], limit: usize) -> (Vec<u64>, usize) {
        let finder = memchr::memmem::Finder::new(needle);
        let (mut found, mut count) = (Vec::new(), 0);
        for r in &self.ranges {
            for at in finder.find_iter(self.bytes(r)) {
                count += 1;
                if found.len() < limit {
                    found.push(r.start + at as u64);
                }
            }
        }
        (found, count)
    }

    /// Every aligned 4-byte float within `tolerance` (relative) of `target`, up to `limit`.
    fn find_float(&self, target: f32, tolerance: f32, limit: usize) -> (Vec<u64>, usize) {
        let (mut found, mut count) = (Vec::new(), 0);
        let span = tolerance * target.abs().max(f32::MIN_POSITIVE);
        for r in &self.ranges {
            let skip = ((4 - (r.start % 4)) % 4) as usize;
            let bytes = self.bytes(r).get(skip..).unwrap_or(&[]);
            for (k, w) in bytes.as_chunks::<4>().0.iter().enumerate() {
                let v = f32::from_le_bytes(*w);
                if (v - target).abs() <= span {
                    count += 1;
                    if found.len() < limit {
                        found.push(r.start + skip as u64 + k as u64 * 4);
                    }
                }
            }
        }
        (found, count)
    }

    /// The floats around an address, for context.
    fn around(&self, address: u64) -> String {
        let start = address.saturating_sub(16) & !3;
        let Some(bytes) = self.read(start, 48) else {
            return String::new();
        };
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .enumerate()
            .map(|(i, w)| {
                let v = float(f32::from_le_bytes(*w));
                if start + i as u64 * 4 == address {
                    format!("[{v}]")
                } else {
                    v
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// The dump a capture names, or `arg` itself when it is a dump.
pub fn open(arg: &str, blobs: Option<&Path>) -> Result<(Dump, Option<Capture>), String> {
    let p = PathBuf::from(arg);
    if p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dmp")) {
        return Ok((Dump::open(&p)?, None));
    }
    let c = crate::open::open(arg, blobs)?;
    let memory = c.meta_json("memory");
    let path = memory["path"]
        .as_str()
        .ok_or_else(|| match memory["error"].as_str() {
            Some(e) => format!("the capture has no memory dump: {e}"),
            None => "the capture has no memory dump".to_string(),
        })?;
    let path = PathBuf::from(path);
    if !path.is_file() {
        let part = PathBuf::from(format!("{}.part", path.display()));
        return Err(if part.is_file() {
            format!("{} is still being written (or its writer died)", path.display())
        } else {
            format!("{} is gone", path.display())
        });
    }
    Ok((Dump::open(&path)?, Some(c)))
}

pub enum Needle {
    Float(f32, f32),
    Int(i64),
    Bytes(Vec<u8>),
    /// The registers of a constant buffer a draw had bound: draw, stage, slot, and one register or all.
    Cb(i64, String, u32, Option<u32>),
}

pub fn memfind(d: &Dump, c: Option<&Capture>, needle: &Needle, limit: usize) -> Result<(), String> {
    out!(
        "{} regions, {:.2} GiB",
        d.ranges.len(),
        d.total() as f64 / (1u64 << 30) as f64
    );
    let show = |what: &str, (found, count): (Vec<u64>, usize)| {
        out!("{what}: {count} found");
        for a in found {
            out!("  0x{a:016x}  {:<48} {}", d.describe(a), d.around(a));
        }
    };
    match needle {
        Needle::Float(v, tol) => show(&format!("float {v}"), d.find_float(*v, *tol, limit)),
        Needle::Int(v) => {
            if let Ok(x) = i32::try_from(*v) {
                show(&format!("int32 {v}"), d.find(&x.to_le_bytes(), limit));
            } else if let Ok(x) = u32::try_from(*v) {
                show(&format!("uint32 {v}"), d.find(&x.to_le_bytes(), limit));
            }
            show(&format!("int64 {v}"), d.find(&v.to_le_bytes(), limit));
        }
        Needle::Bytes(b) => show(&format!("{} bytes", b.len()), d.find(b, limit)),
        Needle::Cb(draw, stage, slot, register) => {
            let c = c.ok_or("--cb needs a capture, not a dump file")?;
            let (key, space): (String, i64) = c
                .row(
                    "SELECT content, space FROM bindings WHERE draw = ?1 AND stage = ?2 AND kind = 'cb' AND slot = ?3",
                    rusqlite::params![draw, stage, slot],
                    |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get(1)?)),
                )
                .ok_or_else(|| format!("draw {draw} has no {stage} b{slot}"))?;
            let bytes: Vec<u8> = c
                .row("SELECT bytes FROM cb_data WHERE hash = ?1", [&key], |r| r.get(0))
                .ok_or("its contents were not captured")?;
            let shader = crate::draws::shader_of(c, *draw, stage);
            let mut reflection = Reflection::default();
            let cb = shader
                .as_deref()
                .and_then(|s| reflection.cb(c, s, *slot, space as u32));
            let mut by_count: HashMap<usize, usize> = HashMap::new();
            for (r, reg) in bytes.as_chunks::<16>().0.iter().enumerate() {
                if register.is_some_and(|x| x as usize != r) || reg.iter().all(|&b| b == 0) {
                    continue;
                }
                let names: [Option<String>; 4] =
                    [0, 1, 2, 3].map(|i| cb.and_then(|cb| cb.name_at(r as u32 * 16 + i * 4)));
                let values: Vec<String> = reg
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|w| float(f32::from_le_bytes(*w)))
                    .collect();
                let (found, count) = d.find(reg, limit);
                *by_count.entry(count).or_default() += 1;
                out!(
                    "c{r} ({}) {}: {count} found",
                    values.join(" "),
                    register_name(&names)
                );
                for a in found {
                    out!("  0x{a:016x}  {}", d.describe(a));
                }
            }
            if by_count.is_empty() {
                out!("no non-zero register to look for");
            }
        }
    }
    Ok(())
}

/// Words that differ between two dumps at the same address: those that went from `from` to `to` (floats within
/// `tolerance`), or with neither, how many words changed in each allocation.
pub fn memdiff(
    a: &Dump,
    b: &Dump,
    from: Option<f32>,
    to: Option<f32>,
    tolerance: f32,
    limit: usize,
) -> Result<(), String> {
    let near = |v: f32, t: Option<f32>| {
        t.is_none_or(|t| (v - t).abs() <= tolerance * t.abs().max(f32::MIN_POSITIVE))
    };
    let (mut shown, mut count) = (0usize, 0usize);
    let mut by_allocation: HashMap<String, usize> = HashMap::new();
    let summary = from.is_none() && to.is_none();
    for r in &a.ranges {
        let Some(other) = b.read(r.start, r.size as usize) else {
            continue;
        };
        let mine = a.bytes(r);
        let skip = ((4 - (r.start % 4)) % 4) as usize;
        let (x, y) = (mine.get(skip..).unwrap_or(&[]), other.get(skip..).unwrap_or(&[]));
        if x == y {
            continue;
        }
        for (k, (p, q)) in x
            .as_chunks::<4>()
            .0
            .iter()
            .zip(y.as_chunks::<4>().0.iter())
            .enumerate()
        {
            if p == q {
                continue;
            }
            let (u, v) = (f32::from_le_bytes(*p), f32::from_le_bytes(*q));
            if !near(u, from) || !near(v, to) {
                continue;
            }
            let address = r.start + skip as u64 + k as u64 * 4;
            count += 1;
            if summary {
                let where_ = a.describe(address);
                let key = where_.split(" +0x").next().unwrap_or(&where_).to_string();
                *by_allocation.entry(key).or_default() += 1;
            } else if shown < limit {
                shown += 1;
                out!(
                    "  0x{address:016x}  {} -> {}  {}",
                    float(u),
                    float(v),
                    a.describe(address)
                );
            }
        }
    }
    if summary {
        let mut rows: Vec<(String, usize)> = by_allocation.into_iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1));
        for (place, n) in rows.iter().take(limit) {
            out!("{n:>10}  {place}");
        }
    }
    out!("{count} words changed");
    Ok(())
}
