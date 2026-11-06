# locum

[![crates.io](https://img.shields.io/crates/v/locum)](https://crates.io/crates/locum) [![docs.rs](https://img.shields.io/docsrs/locum)](https://docs.rs/locum) [![CI](https://img.shields.io/github/actions/workflow/status/Kalmarv/locum/ci.yml?branch=main&label=CI)](https://github.com/Kalmarv/locum/actions/workflows/ci.yml)

A frame capture and replay tool for Direct3D 11 and Direct3D 12 games on Windows. It runs as stand-in system DLLs in
the game's folder and writes each captured frame to a SQLite database, which a command-line tool and a local web
viewer read.

## Features

- Records every draw, dispatch, clear, copy, update and map of a frame in GPU order, with each draw's shaders, state and bindings
- Records the constant-buffer bytes each draw read, with constants named from the shaders' DXBC or DXIL reflection
- Stores each used resource's contents as the frame began and the render and depth targets at the end of each pass
- Captures up to 64 consecutive frames from one request
- Keeps resource contents in a blob store shared by all captures of a game, keyed by XXH3-128 and LZ4-compressed
- Lists, filters, exports (DDS, PNG, raw) and queries captures with `locum-cli`, including plain SQL
- Traces a target's output lane back to the texture channels, constants and vertex attributes it came from
- Replays a draw or a whole frame offline on the GPU or on WARP and verifies every target against the game's
- Holds a translation layer (DXVK, vkd3d-proton) to a captured frame: replays it on the system's Direct3D and through the
  layer, and names what only the layer draws differently or cannot do
- Changes draws with rules (skip, solid colour, replacement shader, constant), live in Direct3D 11 games or in replay
- Names textures, buffers and shader programs after the game's own files through indexes of DDS files, shaders and key lists
- Diffs two captures draw by draw
- Writes a process memory dump at capture time and searches it for the values a draw saw (Direct3D 11)
- Reads, sets and calls game values from a `table.txt` to set up a scene before a capture
- Browses captures in a local web viewer with timeline, draw, resource, shader and SQL pages
- Compares the output of shaders whose results depend on GPU thread order by records rather than bytes

## Getting Started

### Requirements

- Windows, and a 64-bit game that uses Direct3D 11 or Direct3D 12
- Rust 1.99.0 (pinned in `rust-toolchain.toml`) with the MSVC build tools; the DLL shims are linked with `lib.exe`
- [Bun](https://bun.sh) for the viewer
- `dxcompiler.dll` for DXIL tools and DXIL replay rules: found on the DLL search path, in the Windows SDK, or at `LOCUM_DXCOMPILER`

Do not use locum on games with anti-cheat. Live override rules, memory dumps and the `skip`/`max_mb` capture options
are implemented for Direct3D 11 only.

### Installation

Each [release](https://github.com/Kalmarv/locum/releases/latest) has two downloads for Windows (x86_64), with
`SHA256SUMS`:

- `locum-dlls-<version>-x86_64-pc-windows-msvc.zip`: `locum.dll`, `d3d11.dll`, `dxgi.dll`, `d3d12.dll` and
  `nvapi64.dll`, the DLLs that go beside a game
- `locum-cli-<version>-x86_64-pc-windows-msvc.zip`: `locum-cli.exe` (also `cargo install locum-cli`)

The libraries are on crates.io (`cargo add locum-capture`). To build the DLLs yourself instead, run
`cargo build --release` in the workspace; they are written to `target/release/`.

| Package | What it is |
|---|---|
| `locum` | `locum.dll`: wraps the Direct3D and DXGI objects and records captures |
| `locum-shim-d3d11`, `locum-shim-dxgi`, `locum-shim-d3d12` | `d3d11.dll`, `dxgi.dll`, `d3d12.dll`: forward every export to `locum.dll` |
| `locum-shim-nvapi64` | `nvapi64.dll`: an empty NVAPI for Direct3D 12 games |
| `locum-capture` | The capture format, blob store, formats, reflection and rule language |
| `locum-d3d11` | Direct3D 11 readback shared by capture and replay |
| `locum-cli` | Reads, replays and verifies captures |
| `viewer` | Bun and Preact web viewer for captures |
| `locum-smoke`, `locum-smoke-d3d12`, `xtask` | Test programs and the `cargo xtask` build tasks |

Copy the DLLs next to the game's executable (for Unreal games, the one under `Binaries/Win64`), not into a system
folder:

- Direct3D 11: `locum.dll`, `d3d11.dll`, `dxgi.dll`
- Direct3D 12: `locum.dll`, `d3d12.dll`, `dxgi.dll`, `nvapi64.dll`

If the game's folder already has a file of one of these names (another tool's `dxgi.dll`, say), set it aside first.
Remove the DLLs again when you are done.

### Usage

locum writes its log, shaders and captures to `%LOCALAPPDATA%\locum` (or `LOCUM_DIR`). With the game running, create
an empty file named `capture` there. locum removes the file and writes the next frame to `captures\frame-<n>\`.

```powershell
New-Item "$env:LOCALAPPDATA\locum\capture"

locum-cli info last                          # what the capture holds and what it is missing
locum-cli draws last --ps 9f2c               # draws that used a pixel shader
locum-cli draw last 120                      # one draw: state, bindings, named constants, targets after it
locum-cli target last 120 rt0 -o rt0.png     # a render target read back after its pass
locum-cli verify last                        # replay the frame and compare every target with the game's
```

To open the capture in the viewer, which prints its address (`http://127.0.0.1:5170/...`):

```powershell
cd viewer
bun install
bun run viewer --db "$env:LOCALAPPDATA\locum\captures\frame-<n>"
```

Other commands:

- `locum-cli shader last 9f2c -d`: a shader's constant buffers, bindings and disassembly (`--wgsl` for WGSL)
- `locum-cli trace last 120 rt1.z`: what one lane of a target is made of
- `locum-cli export last 812 -o tex.dds`: a resource's contents as the frame began
- `locum-cli replay last 120 --rule "skip ps=9f2c"`: a draw replayed with a rule, before and after
- `locum-cli diff before after`: two captures compared draw by draw
- `locum-cli conform last --layer C:\dxvk\x64`: the frame through a translation layer's DLLs, compared with the
  system's replay and the game's
- `locum-cli --index files.db join last`: what each resource was made from
- `locum-cli sql last "SELECT ..."`: any query on the capture

`locum-cli --help` lists every command. `settings.txt`, `overrides.txt`, `table.txt` and the capture request options
are described in [docs/README.md](docs/README.md).

`cargo xtask smoke` runs the Direct3D 11 test programs through locum and verifies their captures on the GPU and WARP.
`cargo xtask smoke --api d3d12` does the same for Direct3D 12, and `bun test` runs the viewer's tests.
[docs/README.md](docs/README.md) is the full reference, and [viewer/README.md](viewer/README.md) covers the viewer.
locum is licensed under [GPL-3.0-or-later](LICENSE). [CONTRIBUTING.md](CONTRIBUTING.md) describes how releases are made.
