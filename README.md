# locum

[![crates.io](https://img.shields.io/crates/v/locum)](https://crates.io/crates/locum) [![docs.rs](https://img.shields.io/docsrs/locum)](https://docs.rs/locum) [![CI](https://github.com/Kalmarv/locum/actions/workflow/status/Kalmarv/locum/ci.yml?branch=main&label=CI)](https://github.com/Kalmarv/locum/actions/workflows/ci.yml)

A Windows Direct3D 11 / Direct3D 12 frame capture, replay, and memory diagnostics toolkit.

locum pairs **in-process capture DLLs** (recording draws, shaders, resources, and GPU timings into a SQLite database and shared LZ4 blob store) with **`locum-mem`** (out-of-process, zero-suspend Windows virtual memory, D3DKMT, and `DxgKrnl` ETW diagnostics), all inspected from `locum-cli` and a local web viewer.

---

## Capabilities

### Frame Capture, Replay & Verification
- **Full GPU-order capture:** Records every draw, dispatch, clear, copy, update, and map across up to 64 consecutive frames per request, along with bound shaders, state, and constant buffers named from DXBC/DXIL reflection
- **Resource & target snapshots:** Stores initial resource contents in a deduplicated XXH3-128 / LZ4 blob store and saves render/depth targets after each pass
- **Offline replay & layer conformance:** Replays single draws or whole frames on hardware GPU or WARP, diffs two captures draw by draw, and compares DXVK / vkd3d-proton against native Direct3D (`conform`)
- **Timing & shader provenance:** Records per-draw/dispatch GPU times (`--times`), traces output lanes back to inputs (`trace`), and labels resources/shaders from game files (`--index`)
- **Live rules & memory setup (D3D11):** Applies draw overrides (`skip`, solid color, replacement shader, constant) and reads/writes game values via `table.txt`
- **Local web viewer:** Browses captures by timeline, draw, resource, shader, and plain SQL

### Memory, ETW & Dispatch Diagnostics (`locum-cli mem`)
- **Zero-suspend memory ground truth:** Reconciles Win32 virtual memory, D3DKMT allocations, and `DxgKrnl` / kernel HardFaults ETW traces without pausing the target process (`inspect`, `trace`, `etl`, `diff`)
- **VidMm residency & eviction tracking:** Groups allocations by placement and owner (`ALLOC_HIST`), looks up exact-size allocations (`--alloc-size`), and surfaces demoted/evicted handles with priorities and create stacks (`DEMOTED_ALLOC`, `EVICTED_ALLOC`)
- **QPC wait & paging timeline (`timeline`):** Correlates GPU queue activity, fence waits, VidMm paging/demotion throughput, and out-of-process symbolized CPU stacks from live runs or `.etl` files
- **Crash & stall flight recorder (`flight`):** Keeps a 10-second in-memory ETW ring (zero disk writes while armed) that dumps automatically on device removal (`0x887a0006` / TDR) or a 2 s submit stall
- **Compute dispatch probe (`probe`):** Follows indexed raw-buffer descriptors in D3D12 captures (paired with opt-in `dispatch-probe` in `settings.txt`) to compare GPU bytes against CPU aliases

---

## Installation

### Prebuilt Releases (Windows x86_64)
Each [release](https://github.com/Kalmarv/locum/releases/latest) provides two archives (with `.sha256` and `sha256.sum`):

- [`locum-x86_64-pc-windows-msvc.zip`](https://github.com/Kalmarv/locum/releases/latest/download/locum-x86_64-pc-windows-msvc.zip): `locum.dll`, `d3d11.dll`, `dxgi.dll`, `d3d12.dll`, `nvapi64.dll`
- [`locum-cli-x86_64-pc-windows-msvc.zip`](https://github.com/Kalmarv/locum/releases/latest/download/locum-cli-x86_64-pc-windows-msvc.zip): `locum-cli.exe` (or `cargo install locum-cli`)

Libraries are also published on crates.io (`cargo add locum-capture`).

### Building from Source
**Requirements:**
- Windows (64-bit Direct3D 11 / 12 target)
- Rust 1.99.0 (`rust-toolchain.toml`) with MSVC build tools (`lib.exe` for DLL shims)
- [Bun](https://bun.sh) for the web viewer
- `dxcompiler.dll` on `PATH`, in the Windows SDK, or via `LOCUM_DXCOMPILER` (for DXIL tools/replay rules)

```powershell
cargo build --release   # outputs DLLs and locum-cli.exe to target/release/
```

### Placing the Capture DLLs
Copy the DLLs next to the target executable (e.g. `Binaries/Win64` for Unreal games), never into a Windows system directory:

- **Direct3D 11:** `locum.dll`, `d3d11.dll`, `dxgi.dll`
- **Direct3D 12:** `locum.dll`, `d3d12.dll`, `dxgi.dll`, `nvapi64.dll`

Set aside any existing DLLs with those names first, and remove them when finished. **Do not use locum on games with anti-cheat.** (`locum-cli mem` works out-of-process without placing DLLs unless using `mem probe` / `dispatch-probe`.)

---

## Workspace Layout

| Package | Role |
|---|---|
| `locum` | `locum.dll`: Direct3D / DXGI capture hooks and `dispatch-probe` |
| `locum-shim-d3d11`, `locum-shim-dxgi`, `locum-shim-d3d12` | `d3d11.dll`, `dxgi.dll`, `d3d12.dll` export forwarders to `locum.dll` |
| `locum-shim-nvapi64` | `nvapi64.dll` stub for Direct3D 12 targets |
| `locum-capture` | Capture format, LZ4 blob store, reflection, and rule language |
| `locum-d3d11` | Direct3D 11 readback shared by capture and replay |
| `locum-mem` | Out-of-process Windows memory, D3DKMT, and `DxgKrnl` ETW diagnostics |
| `locum-cli` | CLI for capture inspection, replay, verification, diffing, and `mem` diagnostics |
| `viewer` | Bun + Preact web UI |
| `locum-smoke`, `locum-smoke-d3d12`, `xtask` | Smoke test targets and `cargo xtask` automation |

---

## Quick Start

### 1. Capture & Inspect a Frame
locum writes logs, shaders, and captures to `%LOCALAPPDATA%\locum` (or `LOCUM_DIR`). Trigger a capture while the app is running by creating an empty `capture` file:

```powershell
New-Item "$env:LOCALAPPDATA\locum\capture"

locum-cli info last                          # capture summary and missing data
locum-cli draws last --ps 9f2c               # filter draws by pixel shader
locum-cli draw last 120                      # bindings, named constants, and targets for draw 120
locum-cli target last 120 rt0 -o rt0.png     # export render target after pass
locum-cli verify last                        # replay frame and compare targets against capture
```

Common analysis commands:
- `locum-cli shader last 9f2c -d` — constant buffers, bindings, and disassembly (`--wgsl` for WGSL)
- `locum-cli trace last 120 rt1.z` — trace an output lane back to its inputs
- `locum-cli export last 812 -o tex.dds` — export a resource's initial contents
- `locum-cli replay last 120 --rule "skip ps=9f2c"` — replay a draw with a live rule (`--times times.tsv` for D3D12 per-draw/dispatch GPU microseconds)
- `locum-cli diff before after` — draw-by-draw diff of two captures
- `locum-cli conform last --layer C:\dxvk\x64` — compare a translation layer against native D3D and the capture
- `locum-cli --index files.db join last` / `locum-cli sql last "SELECT ..."` — asset naming and raw SQL queries

### 2. Open the Web Viewer
```powershell
cd viewer
bun install
bun run viewer --db "$env:LOCALAPPDATA\locum\captures\frame-<n>"
```

### 3. Memory, ETW & Timeline Diagnostics (`locum-cli mem`)
```powershell
locum-cli mem etw-caps                         # check non-admin ETW provider/stack capabilities
locum-cli mem inspect --pid <pid>              # zero-suspend Win32 + D3DKMT snapshot
locum-cli mem trace --pid <pid> --duration 10  # live DxgKrnl + HardFaults trace & reconciliation
locum-cli mem timeline --pid <pid> --duration 10 # QPC GPU/fence waits, VidMm paging & CPU stacks
locum-cli mem flight --pid <pid>               # 10 s in-memory ETW ring (dumps on TDR / 2 s stall)
locum-cli mem diff before.json after.json      # diff placement, owners, and size classes
locum-cli mem probe last --shader <hash>       # D3D12 indexed compute buffer probe
```

*(Note: Live override rules, process memory dumps, and `skip`/`max_mb` capture options are D3D11-only. `DxgKrnl` kernel ETW is system-wide; pass `--allocs-only` and/or `--max-etl-mb` to bound `.etl` size beside timed runs.)*

---

## Testing & Docs

```powershell
cargo xtask smoke               # D3D11 capture/replay smoke tests (GPU + WARP)
cargo xtask smoke --api d3d12   # D3D12 capture/replay smoke tests
bun test                        # viewer tests
```

- Full CLI, `settings.txt`, `overrides.txt`, and `table.txt` reference: [docs/README.md](docs/README.md)
- Viewer details: [viewer/README.md](viewer/README.md)
- Releases & contributing: [CONTRIBUTING.md](CONTRIBUTING.md) • Licensed under [MIT](LICENSE)
