# locum

A stand-in `d3d11.dll` and `dxgi.dll` for looking inside a Direct3D 11 game. The game asks for a device and gets
locum's; every call it makes arrives at locum first, which passes it on to the real runtime. Nothing in the system's
DLLs is patched, and the game keeps rendering on its own driver.

Status: early. The wrappers forward everything, shaders are hashed and dumped, and one frame can be captured whole:
every draw and every call that changed a resource in GPU order, each draw's full state, the contents of every
resource the frame used as the frame began, and the render targets at the end of each pass, in one SQLite file and a
store of blobs. Rules in a file change what is drawn: skip draws, swap shaders, change constants.

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
- `captures/<name>/capture.db`: a captured frame (below). Create a file named `capture` in the folder to request one;
  locum removes it, records the next frames and writes the capture. Each line of the file is an option, and an empty
  file captures everything:
  - `label NAME`: the capture's folder name (default `frame-<n>`);
  - `skip targets`: no render and depth targets read back at the end of each pass;
  - `skip contents`: no resource contents (draws, state and constants only);
  - `max_mb N`: the most memory the target copies may use (default 4096); targets past it are skipped and counted.
- `blobs/<2 hex>/<32 hex>`: the blob store, shared by every capture: resource contents, targets, the bytes each update
  and map wrote, and shader bytecode, each under the xxh3-128 hash of its bytes. Contents are in a canonical layout:
  subresources in order (mip within array slice), rows packed tightly, blocks for compressed formats.
- `settings.txt` (written by you), one per line:
  - `uploads store` (default), `hash` or `off`: whether the data each resource is created with, and each update
    covering a whole subresource, is hashed and stored as it happens. Stored uploads are the game's own textures and
    meshes without a readback, and their hashes match the files they were loaded from when those hold the same bytes;
  - `blobs DIR`: where the blob store is (the variable `LOCUM_BLOBS` overrides it);
  - `dumper PATH`: `locum-cli.exe`, which writes a memory dump of the game at each capture (unless the request says
    `skip memory`). The game waits in Present only while it takes a snapshot of the process (tens of milliseconds);
    the dump is written from the snapshot while the game runs on;
  - `dumps DIR`: where the dumps go (default the capture's folder). They hold all of the game's memory: keep them
    private.
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

locum numbers every resource as it is created and keeps its description, creation stack and upload hash, so a
capture lists all the resources alive, not only those the frame touched. The capture runs over three frames: the one
before records command lists ahead of time and notes which resources are written; at the frame's start those are
copied; during it every call is recorded, and any other resource is copied on its own context just before its first
write. At the end, resources the frame only read are taken from their upload (immutable ones) or read back.

`capture.db` has these tables (`capture/src/db.rs` has the schema with comments):

- `meta`: the API, frame, process, request and counts; `gaps`: what could not be recorded, and why;
- `resources` and `subresources`: every live resource with its description, upload hash, whether the frame used or
  wrote it, and the key of its contents as the frame began with where they came from (`upload`, `start`,
  `first-write`, `end`) and any doubt about them;
- `views` and `states` (rasterizer, blend, depth-stencil, sampler, input layout), with their descriptions;
- `events`: everything on the GPU's timeline in order: draws, dispatches, clears, copies, updates and maps (with the
  key of the bytes written), resolves and mip generation, each with its context, command list and call stack;
- `draws`, `bindings`, `viewports`, `scissors`: each draw's shaders and state, and everything bound to it by stage,
  kind (`cb`, `srv`, `uav`, `sampler`, `rtv`, `dsv`, `vb`, `ib`, `so`) and slot, constant buffers with the bytes the
  GPU read (`cb_data`);
- `passes` and `targets`: runs of draws with the same targets, and the targets read back after each one's last draw
  (in the format the pass drew in; depth as the colour format of the same bits, D24S8 as R24_UNORM_X8);
- `shaders`, `modules`, `stacks`: bytecode keys, and what the call stacks need to be read.

While a capture is recording, the game's writes through `Map` go to memory locum owns and are copied to the driver's
at `Unmap`, so a map records only the bytes the game wrote.

A pass ends where the targets in effect change on the GPU's timeline: an `OMSetRenderTargets` that binds other
targets, a command list whose first draw uses other targets than the draw before it, or the end of the frame.

Draws recorded on deferred contexts are placed where their command list is executed. Constant buffers and targets
are copied by the GPU on the draw's own context, so each draw's constants and each pass's targets are the ones the
GPU saw at that point of the frame.

## Reading a capture

`locum-cli` reads captures. A capture is named by its `capture.db`, its folder, its name under locum's folder, or
`last`:

```
locum-cli captures                         # the captures in locum's folder, newest first
locum-cli info last                        # what it holds, and what it is missing
locum-cli draws last --ps 9f2c --pass 3    # draws, filtered by shader, pass, --uses/--target RESOURCE, --content KEY
locum-cli draw last 120                    # one draw: state, everything bound, constants by name, targets after it
locum-cli cb last 120 ps 3                 # a constant buffer, every register, named from the shader's reflection
locum-cli find last 0.35                   # which constant holds 0.35, its name, and the draws that saw it
locum-cli passes last
locum-cli resources last --used --format BC7_UNORM
locum-cli resource last 812                # where it was made (call stack), hashes, views, draws, events
locum-cli events last --kind copy          # the timeline; --resource N for what touched one resource
locum-cli shader last 9f2c -d              # constant buffers and disassembly
locum-cli export last 812 -o tex.dds       # contents as the frame began: .dds (all of it), .png (one mip), or raw
locum-cli target last 120 rt0 -o gbuf0.png # a target read back after a pass
locum-cli sql last "SELECT ..."            # anything else
```

Replay runs a captured draw again offline, on the GPU or on WARP (`--warp`), with or without rules in the
`overrides.txt` language changing it:

```
locum-cli verify last                      # replay the frame; compare every target read back with the game's
locum-cli replay last 812                  # the draw and the earlier events it depends on; writes its targets
locum-cli replay last last --rule "constant vs b11 c0 8 8 0 0"   # before, after and a diff picture of the change
locum-cli replay last 812 --rules mine.txt --mode frame
```

A draw is replayed with what it depends on: walking back from it, each earlier event that wrote a resource it (or an
event already taken) reads or writes is taken too, and every resource starts with its contents as the frame began.
Each draw runs from the full state captured at it, with its constant buffers made from the bytes the GPU read. With
rules, the targets are written before (`rt0.before.png`) and after (`rt0.after.png`) them, and `rt0.diff.png` shows
the texels that changed in magenta over the unchanged ones in grey.

The memory dump ties what the GPU saw back to the game's memory:

```
locum-cli memfind last --cb 812 ps 3       # where each register of that constant buffer sits in memory
locum-cli memfind last --float 0.35        # or --int N, --bytes 0000803f
locum-cli memdiff before.dmp after.dmp --from 0.35 --to 0.5   # what changed between two dumps
locum-cli stack last 17                    # a call stack recorded at a map or update, as module+offset
```

Each hit is named: a module and offset, a thread's stack, or a heap allocation and the offset into it. Material
parameters copied straight into constant buffers are found this way; values the game computes each frame (matrices)
usually are not in memory as the GPU saw them.

Constants are named from each shader's reflection, as `cbuffer.variable[index].component`. PNGs of depth and other
one-channel images are stretched from their smallest to largest value, float colour is tone-mapped, and `--range
LO:HI` sets the mapping.

## Building

```
cargo build --release
```

The DLLs are `target/release/locum.dll`, `d3d11.dll` and `dxgi.dll`, and the reader `target/release/locum-cli.exe`. Regenerate the vtables after changing the
`windows` version with `cargo xtask gen`. `cargo xtask smoke` builds everything and runs a small Direct3D program
through locum's DLLs, checking identity, deferred contexts, a capture and overrides, then reads the capture with
`locum-cli`.

## References

DXVK and RenderDoc are both open source and solve the same problems (object identity, DXGI, deferred contexts); when
in doubt, locum does what they do.

## Licence

GPL-3.0-or-later.
