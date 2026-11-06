# Changelog

## 0.1.2

- Each release now has the DLLs that go beside a game as a download, `locum-dlls-<version>-x86_64-pc-windows-msvc.zip`
  (`locum.dll`, `d3d11.dll`, `dxgi.dll`, `d3d12.dll`, `nvapi64.dll`), beside `locum-cli`. The README's Installation
  section says which go where.
- Direct3D 12: ExecuteIndirect with an incrementing constant (ExecuteIndirect tier 1.1) is captured and replayed.
  Before, capture called its records unreadable and replay skipped it.
- `cargo xtask smoke --api d3d12 --debug` verifies every capture of the run under GPU-based validation and the debug
  layer, not only two of them.
- `cargo xtask smoke --dlls DIR` (and `--api d3d12`) runs the smokes on the DLLs in DIR, such as a release's, instead
  of building them.
