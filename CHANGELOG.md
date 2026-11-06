# Changelog

## Unreleased

- `cargo xtask smoke` runs what cargo built where `CARGO_TARGET_DIR` points, and stages there (`smoke/`,
  `smoke-d3d12/`). Before, it built there but copied `target/release`'s DLLs, so a smoke could pass on old ones.
- `locum-cli conform` (and `--layer`) loads what a layer's DLLs forward their exports to from the layer's folder:
  locum's stand-ins forward to `locum.dll`, which was otherwise the first of that name beside locum-cli or on PATH
  (cargo's target folders under `cargo xtask smoke`), or none, and the layer replay failed to load the system's
  `d3d11.dll`. The machine line names those modules too.

## 0.1.4 (2026-11-06)

- Releases are made by release-plz and dist. The downloads are now `locum-x86_64-pc-windows-msvc.zip` (`locum.dll`,
  `d3d11.dll`, `dxgi.dll`, `d3d12.dll`, `nvapi64.dll`: the DLLs that go beside a game; was `locum-dlls-<version>-…`)
  and `locum-cli-x86_64-pc-windows-msvc.zip`, each with a `.sha256`, and `sha256.sum` for both.
- Direct3D 12 replay on a device that takes more of a heap for a placed resource than the game's GPU did, or another
  alignment, lays the heap out again instead of making the resource committed: what shared memory in the game still
  does, reserved tiles and the frame's tile remaps included. vkd3d-proton on an RTX 5070 Ti sizes 94 of an Elden Ring
  frame's placed resources so; before, they no longer shared memory and verify called the frame incomplete.
- A placed resource this device will not take at the small alignment the game placed it with is placed at the default.

## 0.1.3 (2026-11-06)

- `locum-cli conform CAPTURE --layer DIR` holds a translation layer (DXVK, vkd3d-proton) to a captured frame: it
  verifies the capture on the system's Direct3D and through the layer's DLLs, and names each comparison the system's
  replay passes and the layer's fails, what the layer's never reached, and what only it could not make. The docs'
  "Translation layers" section says what it found in DXVK 3.1.1 and vkd3d-proton 3.1.
- `--layer DIR` on any command replays through the Direct3D DLLs in DIR, and a replay's machine line names them.
- Replays run on the system's Direct3D, not on DLLs beside locum-cli. Before, a locum-cli in `target/release` (or
  beside a game) loaded locum's own stand-ins there and replayed through locum's wrappers.
- `verify --report FILE` writes every comparison it made as JSON.
- `overrides.txt` is read when a device is made, so a program that never presents gets its rules.
- `cargo xtask smoke --api d3d12` starts each run with an empty log folder; captures of every earlier run had piled
  up there (1 GB here).

## 0.1.2 (2026-11-06)

- Each release now has the DLLs that go beside a game as a download, `locum-dlls-<version>-x86_64-pc-windows-msvc.zip`
  (`locum.dll`, `d3d11.dll`, `dxgi.dll`, `d3d12.dll`, `nvapi64.dll`), beside `locum-cli`. The README's Installation
  section says which go where.
- Direct3D 12: ExecuteIndirect with an incrementing constant (ExecuteIndirect tier 1.1) is captured and replayed.
  Before, capture called its records unreadable and replay skipped it.
- `cargo xtask smoke --api d3d12 --debug` verifies every capture of the run under GPU-based validation and the debug
  layer, not only two of them.
- `cargo xtask smoke --dlls DIR` (and `--api d3d12`) runs the smokes on the DLLs in DIR, such as a release's, instead
  of building them.
