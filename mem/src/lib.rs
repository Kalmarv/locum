//! Out-of-process, zero-suspend memory ground-truth inspection (`locum-mem`).
//!
//! Combines three independent OS sources without injecting code into or suspending the target process:
//! 1. Win32 virtual-memory and working-set inspection (`VirtualQueryEx`, `QueryWorkingSetEx`,
//!    `GetProcessMemoryInfo`) splitting every committed region into RAM-resident (private/shared),
//!    paged-out/trimmed, and untouched demand-zero pages, plus out-of-process `D3DKMTQueryVideoMemoryInfo`.
//! 2. `Microsoft-Windows-DxgKrnl` + kernel `SystemMemory` ETW (live trace via `StartTraceW`/`EnableTraceEx2`
//!    with rundown `EVENT_CONTROL_CODE_CAPTURE_STATE`, or offline `.etl` decoding via cached TDH schemas)
//!    for true `VidMm` segment placement (`Local` VRAM vs `NonLocal` host vs demoted), `UpdateTileMappings`
//!    (`UpdateGpuVirtualAddress`), paging/eviction, and kernel `HardFaults`.
//! 3. Generic `--labels <ranges.tsv>` owner attribution and optional read-only join against a `locum`
//!    `capture.db`'s `d12_draw_descriptors` table.

pub mod capture_join;
pub mod diff;
pub mod etw;
pub mod labels;
pub mod reconcile;
pub mod win32;

/// Number of target process thread suspends performed by `locum-mem` (guaranteed zero).
pub const THREAD_SUSPENDS: u64 = 0;
