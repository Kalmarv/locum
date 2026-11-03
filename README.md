# locum

A stand-in `d3d11.dll` and `dxgi.dll` for looking inside a Direct3D 11 game. The game asks for a device and gets
locum's; every call it makes arrives at locum first, which passes it on to the real runtime. Nothing in the system's
DLLs is patched, and the game keeps rendering on its own driver.

Status: early. The wrappers forward everything, shaders are hashed and dumped, and one frame can be captured with
each draw's state, its constant-buffer contents and the render targets at the end of each pass. Rules in a file
change what is drawn: skip draws, swap shaders, change constants.

## What it is for

- **Watching**: every draw with its full state: shaders, constant buffers, textures, render targets, vertex and index
  buffers, blend and raster state.
- **Reading back**: copying a texture, render target or buffer to memory at the moment it is used.
- **Changing**: overriding a constant, skipping a draw, swapping a shader or texture by hash, then looking at the
  screen.

## How it works

- `d3d11.dll` and `dxgi.dll` go beside the game's executable. They contain no code: each export forwards to
  `locum.dll` (the same names and ordinals as the system DLLs, the list DXVK exports).
- `locum.dll` loads the real `d3d11.dll` and `dxgi.dll` from the system folder and wraps the device, its contexts,
  the DXGI factory and the swap chain in objects of its own.
- The wrappers are generated. The `windows` crate publishes each interface's vtable as a `#[repr(C)]` struct with the
  exact ABI; `cargo xtask gen` reads those structs and writes `locum/src/vtables/`: one forwarding thunk per method
  and a trait whose default methods call the real object. A wrapper overrides only the methods it cares about.

## Using it

Copy `locum.dll`, `d3d11.dll` and `dxgi.dll` beside the game's executable. locum writes to the folder named by
`LOCUM_DIR`, or `%LOCALAPPDATA%\locum`:

- `locum.log`: what was wrapped, the frame rate every 600 frames, and each interface the game asked for that locum
  passes to the real object unwrapped.
- `shaders/<hash>.<stage>.dxbc`: the bytecode of every shader the game creates, once per distinct shader. The hash
  is FNV-1a 64 of the bytecode.
- `captures/frame-<n>/draws.jsonl`: a captured frame. Create a file named `capture` in the folder to request one;
  locum removes it, records the next frames and writes the capture. Each line of the file is an option:
  - `targets`: also read back the render and depth targets at the end of each pass, to
    `captures/frame-<n>/targets/<draw>-<target>.dds` (the pass's last draw; `rt0` to `rt7` or `depth`);
  - `max_mb N`: the most memory the target copies may use (default 4096); targets past it are skipped and counted.
- `overrides.txt` (written by you): rules that change what is drawn, read again when the file changes and dropped when
  it is removed. One per line:
  - `skip vs=<hash>`: draws with that shader bound are not drawn (`skip cs=<hash>`: dispatches are not run);
  - `solid vs=<hash> R G B A`: those draws get a pixel shader writing that colour to every target;
  - `shader ps=<hash> FILE`: that shader is replaced by the bytecode in FILE (relative to the folder);
  - `constant ps b3 c0 X Y Z W`: the draw sees constant 0 of its ps b3 range as X Y Z W (floats, or `0x` and 8 hex
    digits for raw bits);
  - any rule can end with `if vs=<hash> ps=<hash> ...`; every stage can be named (vs hs ds gs ps cs).

  Rules apply at the draw, on its own context, and are undone after it. A changed constant is written into a GPU copy
  of the bound buffer, so draws on deferred contexts get theirs when their command list runs. `locum.log` reports how
  often each rule applied every 600 frames.

A capture is one JSON object per line: a `frame` line (draw count, command lists spliced in, copy failures), a
`resource`, `view` or `state` line for each object the frame used (with a number the draws refer to), a `cb` line for
each distinct constant-buffer content (hex), and a `draw` line for each draw and dispatch in the order the GPU runs
them: the call and its arguments, each stage's shader hash, constant buffers (slot, buffer, range, content), shader
resource views, render targets, depth target, input assembly and the blend, raster and depth-stencil states, and with
`targets` the files read back after it.

A pass ends where the targets in effect change on the GPU's timeline: an `OMSetRenderTargets` that binds other
targets, a command list whose first draw uses other targets than the draw before it, or the end of the frame. Its
targets are copied then and kept with its last draw.

Target files are DDS in the format the pass drew in; depth is written as the colour format of the same bits (D24S8 as
R24_UNORM_X8, D32 as R32_FLOAT) and typeless formats as their plain typed format. Multisampled colour targets are
resolved first.

Draws recorded on deferred contexts are placed where their command list is executed. Constant buffers and targets
are copied by the GPU on the draw's own context, so each draw's constants and each pass's targets are the ones the
GPU saw at that point of the frame.

## Building

```
cargo build --release
```

The DLLs are `target/release/locum.dll`, `d3d11.dll` and `dxgi.dll`. Regenerate the vtables after changing the
`windows` version with `cargo xtask gen`. `cargo xtask smoke` builds everything and runs a small Direct3D program
through locum's DLLs, checking identity, deferred contexts and a capture.

## References

DXVK and RenderDoc are both open source and solve the same problems (object identity, DXGI, deferred contexts); when
in doubt, locum does what they do.

## Licence

GPL-3.0-or-later.
