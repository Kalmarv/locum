# locum reference

How locum works, the files it writes and reads, what a capture holds, and every `locum-cli` command. The
[README](../README.md) has the short version.

## How it works

- `d3d11.dll` and `dxgi.dll` contain no code: each export forwards to `locum.dll` (the same names and ordinals as the
  system DLLs, the list DXVK exports).
- `locum.dll` loads its own two DLLs, then the real `d3d11.dll` and `dxgi.dll` from the system folder, and wraps the
  device, its contexts, the DXGI factory and the swap chains in objects of its own. Loading its own first keeps a
  game that loads `d3d11.dll` late, by name, on locum's.
- The wrappers are generated. The `windows` crate publishes each interface's vtable as a `#[repr(C)]` struct with the
  exact ABI; `cargo xtask gen` reads those structs and writes `locum/src/vtables/`: one forwarding thunk per method
  and a trait whose default methods call the real object. A wrapper overrides only the methods it cares about.

## Files and settings

locum writes to the folder named by `LOCUM_DIR`, or `%LOCALAPPDATA%\locum`:

- `locum.log`: what was wrapped, the frame rate every 600 frames, and each interface the game asked for that locum
  passes to the real object unwrapped.
- `shaders/<hash>.<stage>.dxbc`: the bytecode of every shader the game creates, once per distinct shader. The hash
  is FNV-1a 64 of the bytecode.
- `captures/<name>/capture.db`: a captured frame (below). Create a file named `capture` in the folder to request one;
  locum removes it, records the next frames and writes the capture. Each line of the file is an option, and an empty
  file captures everything:
  - `label NAME`: the capture's folder name (default `frame-<n>`);
  - `skip targets`: no render and depth targets read back at pass ends, and no clear-output checkpoints;
  - `skip contents`: no resource contents (draws, state and constants only);
  - `max_mb N`: the most memory the copies of targets, clear outputs and what draws and dispatches wrote through UAVs may use
    (default 4096); copies past it are skipped and counted.
- `blobs/`: the blob store, shared by every capture: resource contents, targets, the bytes each update and map wrote,
  and shader bytecode, each under the xxh3-128 hash of its bytes. They are appended to `packs/*.pack` (one per process
  writing), and `index.db` (table `blobs(key, pack, offset, size)`) says where each one is; stores from before packs
  hold one file per blob, `<2 hex>/<32 hex>`, and are still read. Contents are in a canonical layout: subresources in
  order (mip within array slice), rows packed tightly, blocks for compressed formats.
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

  Hashes are the whole 16 hex digits here. Rules given to `locum-cli replay` can use the start of a hash instead,
  as long as it names exactly one shader of that stage in the capture.

  Rules apply at the draw, on its own context, and are undone after it. A changed constant is written into a GPU copy
  of the bound buffer, so draws on deferred contexts get theirs when their command list runs. `locum.log` reports how
  often each rule applied every 600 frames.

locum numbers every resource as it is created and keeps its description, creation stack and upload hash, so a
capture lists all the resources alive, not only those the frame touched. The capture runs over two frames: the one
before records command lists ahead of time (a list recorded then may be executed in the captured frame), and during
the captured frame every call is recorded.

Each resource's contents as the frame began are copied on the immediate context just before the first thing on the
GPU's timeline that touches it: before an immediate-context call that reads or writes it, or before the
`ExecuteCommandList` whose list touches it first. Nothing has touched it since the frame began, so the copy is exact
whatever the frame does to it later, on whichever context, and however many times a list runs; a list recorded but
never executed changes nothing. Immutable resources whose upload was stored are not copied: the upload is their
contents. locum's own copies run whatever predicate the game has set.

`capture.db` has these tables (`capture/src/db.rs` has the schema with comments):

- `meta`: the API, frame, process, request and counts; `gaps`: what could not be recorded, and why;
- `resources` and `subresources`: every live resource with its description, upload hash, whether the frame used or
  wrote it, and the key of its contents as the frame began with where they came from (`first-use` or `upload`) and
  any doubt about them;
- `views` and `states` (rasterizer, blend, depth-stencil, sampler, input layout), with their descriptions;
- `events`: everything on the GPU's timeline in order: draws, dispatches, clears, copies, updates and maps (with the
  key of the bytes written), resolves and mip generation, each with its context, command list and call stack;
- `draws`, `bindings`, `viewports`, `scissors`: each draw's shaders and state, and everything bound to it by stage,
  kind (`cb`, `srv`, `uav`, `sampler`, `rtv`, `dsv`, `vb`, `ib`, `so`) and slot, constant buffers with the bytes the
  GPU read (`cb_data`);
- `passes` and `targets`: runs of draws with the same targets, and the targets read back after each one's last draw
  (in the format the pass drew in; depth as the colour format of the same bits, D24S8 as R24_UNORM_X8). Every
  array slice and 3D depth plane covered by the view has a row: `sub` identifies its subresource, `z` its depth
  plane (zero for 2D). Layered names are `rt0[slice2]`, `depth[slice2]`, or `rt0[z2]`;
- `results`: resource contents immediately after each draw/dispatch writes its UAVs or a clear writes its view,
  and whether its shader is order-dependent (below). Clears have `order_dependent=0`;
- `shaders`, `modules`, `stacks`: bytecode keys, and what the call stacks need to be read.

Some shaders write different bytes depending on the order the GPU happens to run their threads: a thread that takes
the next free slot of a buffer with an atomic add (particles, light lists) puts its record wherever it lands, and the
GPU never runs the threads in the same order twice. locum decodes every shader's bytecode as it is made (with dx's
`dxbc-reader`) and flags those that use an atomic that returns a value (`imm_atomic_*`, `atomic_cmp_store`;
`locum_capture::dxbc`), and the log names them, and any program it cannot decode with the word where reading
stopped. During a capture, every draw or dispatch has everything bound to it as a UAV copied right after it runs,
into `results`, flagged when its shader is order-dependent.

A shader can also depend on the order without an atomic, when its thread groups write the same records: whichever
group runs last wins, and D3D doesn't order groups. In a Motor Town frame, groups 0 and 1 of one dispatch (compute shader
`d01ca9db50d0ef89`) both write records 12212 to 12272; the game's own output holds group 0's values in the first 36 and group 1's in the rest,
and verify's replay holds group 1's in all of them. The bytecode can't show this (which records a group writes comes
from its input buffers), so such a dispatch isn't flagged, and verify reports it as differing.

While a capture is recording, the game's writes through `Map` go to memory locum owns and are copied to the driver's
at `Unmap`, so a map records only the bytes the game wrote. A discarding map starts as a fixed pattern
(`locum_capture::maps`), which is what the GPU reads wherever the game wrote nothing; replay writes the same pattern
under the recorded bytes, so a shader reading past what the game filled in sees what it saw in the game. Any other map
starts as the resource's memory and gets back only the words the game changed, so what the game left alone stays as
it was (a staging resource the GPU copied into keeps that copy). A swap chain resize during a capture gives the
capture up (it holds references to the back buffers): request another.

`WriteToSubresource` (Direct3D 11.3: the CPU writing straight into a default texture) is recorded as an `update`
event with its box, subresource and bytes, and replay makes the same write. Such a texture is mapped first (`Map`
with no pointer back); its contents are copied before that map. One mapped already when the capture began has no
such copy, and a `default_map` gap marks the capture incomplete.

Tiled resources and tile pools replay as themselves. locum follows every successful mapping call, armed or not, in a
model of each resource's page table (which pool tile each of its tiles maps to), so a capture knows the mappings as
the frame began. The first time the frame uses a tiled resource, a `tile_bootstrap` event records its page table, and
`tile_seed` events record the contents of each pool tile it maps that no other resource seeded yet, read through its
mapping as 64 KiB tiles; a resource mapped again during the frame has its newly mapped pool tiles seeded the next time
it is used. A pool's size includes resizes made before the capture. Every event on a tiled resource also reads or
writes its pool, so a sliced replay keeps the writes made through another resource mapping the same tiles. Replay
checks the device's tiling against the capture's and refuses one that differs.

Fully mapped packed-mip groups are seeded as canonical texels, one `tile_packed_seed` per mip. Each readback copies
just that resident subresource into an ordinary one-mip staging texture; BC staging dimensions round up to whole
blocks while the readback retains the mip's own row layout. A group's events are appended only after every mip's
copy succeeds; a failure leaves no partial seed prefix. Replay restores the mips with UpdateSubresource.
This preserves texels for a matching layout, not the driver's opaque pool bytes. Partial groups, overlapping aliases,
reinterpreted seed mappings, and CopyTiles/UpdateTiles on packed coordinates retain explicit `tiled` gaps. A standard
alias of a formerly packed pool tile also stays a gap, including CopyTiles reads after the packed resource was unmapped.
This packed-use history lasts until the pool is destroyed. Hardware smoke covers RGBA8 and BC1 groups, each with
seven patterned mips down to 1x1;
the tested WARP device reports no packed mips, and cross-driver incompatible layouts are explicitly rejected.

Calls on tiled resources and tile pools are recorded in order as `tile_*` events with all their arguments:
UpdateTileMappings (an array the game passed as null stays null), CopyTileMappings, CopyTiles with its direction and
byte offset, UpdateTiles with the bytes it wrote, ResizeTilePool and TiledResourceBarrier. A call that fails is not
recorded. Replay spells out the documented one tile per range where several ranges have null counts (WARP crashes on
the null pointer). An upload on a deferred context runs where its command list is executed, and so does a mapping
change, but a mapping change in a command list is a `tiled` gap: the tiles it maps aren't seeded yet. Tiles of a
shrunken pool still mapped beyond its new size are also a `tiled` gap. Tiled
render and depth targets and tiled UAV outputs aren't read back to compare. No readback copies a whole tiled resource
(on tier 1 hardware it could touch an unmapped tile). A capture from before this, with no mappings or seeds, is
refused rather than replayed as ordinary resources.

A pass ends where the targets in effect change on the GPU's timeline: an `OMSetRenderTargets` that binds other
targets, a command list whose first draw uses other targets than the draw before it, or the end of the frame. Its
targets are read there, and the capture records the event they were read after (the pass's last draw, or a clear, copy
or dispatch after it), which is where verify compares them. Each captured execution of a retained command list gets
fresh constant-buffer, target and output copies. Updating or mapping its resources between executions therefore
produces a separate checkpoint for each execution.

Deferred contexts retain command arguments, referenced D3D11 objects, and CPU upload/map bytes from the start of each
list, including before capture arming. No GPU copies are issued by this recorder while idle. Metadata belongs to the
finished command list and is released with it. During capture, locum constructs an instrumented list for each
execution, keeping the original Finish/Execute boundaries. A list begun before arming or during an older capture can
therefore be captured in full. A list that never executes contributes no events or gaps.

Deferred write maps use a reusable write-watch allocation per resource/subresource in the current list. Unmap
examines only dirty pages and retains their changed byte ranges, with pitch padding removed. NO_OVERWRITE compares
those pages against their last retained values so appending on a page does not rewrite earlier GPU-referenced bytes.
MEM_COMMIT charges commit for the whole subresource, even though untouched pages consume no physical RAM.
DISCARD starts a new logical allocation without clearing untouched bytes; those bytes remain undefined. The first
written page can therefore retain stale pre-DISCARD bytes outside the application's writes in the recorded upload. Finished
lists own the sparse uploads, while map shadows are released at FinishCommandList. If write-watch observation fails,
the original list is retained with a reconstruction gap.

If exact reconstruction is unavailable, locum executes the original list and emits a `deferred_recorder` gap.
This includes applied live overrides, unobservable inherited stream-output cursors, nested lists (whose instrumented
readbacks hung on the tested hardware runtime), unknown upload layouts, context-state swaps, hardware protection and
fence calls. Successful deferred tile-mapping calls retain their separate `tiled` gap until execution-time seeding is
verified. The D3D11 specification permits deferred mapping; the installed hardware and WARP runtimes rejected the
probed mapping calls with `E_INVALIDARG`, so this is an observed runtime limitation, not an API-wide prohibition.

`locum-smoke --record-bench` measures the real unarmed recorder: 100 retained lists, each with 500 draws and 500 CPU
uploads, six trials, with elapsed time and process-private memory. Run the same executable beside baseline/new DLLs
(and with `--warp`) to compare recorder cost without submitting any of those lists to the GPU.
Add `--maps` for four trials of 4/16 MiB dynamic buffers: one DISCARD followed by 255 small NO_OVERWRITE writes,
both within pages and across pages, plus repeated 16 MiB DISCARD maps with 64-byte writes. It reports us/map,
FinishCommandList time, and private-memory deltas before and after Finish. The old full-shadow implementation can
retain roughly 4 GiB in the 16 MiB ring case; run comparisons sequentially when the machine is free.

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
locum-cli shader last 9f2c -d              # constant buffers, textures and samplers by register, and disassembly
locum-cli draw last all --json > d.jsonl   # every draw as JSON, one per line (also: draw N --json, cb --json,
                                           # shader --json with every constant by its full name)
locum-cli export last 812 -o tex.dds       # contents as the frame began: .dds (all of it), .png (one mip), or raw
locum-cli export last 812 --at 3301 -o t.dds  # as event 3301 saw them, replayed with the events that wrote them
locum-cli target last 120 rt0 -o gbuf0.png # a target read back after a pass
locum-cli sql last "SELECT ..."            # anything else
```

Replay runs a captured draw again offline, on the GPU or on WARP (`--warp`), with or without rules in the
`overrides.txt` language changing it:

```
locum-cli verify last                      # replay the frame; compare every target read back with the game's
locum-cli replay last 812                  # the draw and the earlier events it depends on; writes its targets
locum-cli replay last last --rule "constant vs b11 c0 8 8 0 0"   # as recorded, with the rule, and a diff picture
locum-cli replay last 812 --rules mine.txt --mode frame
```

`verify` exits with success only when every target and every draw, dispatch or clear output compared is identical and
nothing the replay needs is missing (contents, command lists, constants); checks the capture could not make (targets
or outputs skipped, a repeated list's earlier executions, the memory dump) are listed as not compared. With
`--out <dir>` it writes the replay's and the game's bytes of everything that differs there
(`event-<seq>-<resource>.{replay,game}.bin`, `draw-<n>-<target>.{replay,game}.bin`), and for a target in a format it
can decode, both pictures and one of where they differ (`.replay.png`, `.game.png`, `.diff.png`).

A capture taken while a rule in `overrides.txt` changed or skipped one of its draws records the draw as the game
asked for it, not as the GPU ran it: an `overrides` gap names the draw, the event and the rule, and verify fails even
if the pixels happen to match. A rule that matched nothing, or a draw in a command list that never ran, doesn't count.

After each draw or dispatch, replay compares what it wrote through UAVs with what the game's GPU wrote (`results`).
Each clear is also compared immediately, including unbound views and clears with no following draw. A frame with
only clears can therefore be verified. These checkpoints copy the whole resource, including bytes outside the
cleared view; like pass targets and UAV outputs, they spend the shared `max_mb` budget and report gaps when it runs
out. Older captures without `targets.z` remain readable (depth plane zero).
Where it differs, `verify` names the event (`Dispatch 218 (event 567) #4770: wrote other bytes than the game's`),
fails, and goes on from the game's bytes, so a difference further on is that later event's own: the first such line
is where the replay and the game part. `replay` with rules puts back only order-dependent outputs, so a rule's
effect on a dispatch stays.

An order-dependent draw or dispatch's output is compared the same way, and described: identical; the same records in
another order (a structured buffer's structures, otherwise 4-byte words); or different in more than order, as when
the shader writes what its atomics returned (Sekiro's light tiles each hold the offset into the light list their
group was given). Then it puts the game's bytes in place and goes on from them, so everything after follows the
game's order and can be compared exactly. Neither is a failure. Outputs that differ in
more than order are listed as not compared: what verify proves is that every target is the game's given what those
events wrote in the game. Rules do not change what such an event writes.

A draw is replayed with what it depends on: walking back from it, each earlier event that wrote a resource it (or an
event already taken) reads or writes is taken too, and every resource starts with its contents as the frame began.
Each draw runs from the full state captured at it, with its constant buffers made from the bytes the GPU read. With
rules, the draw is replayed twice, and its targets are written as recorded (`rt0.recorded.png`) and with the rules
(`rt0.rules.png`), and `rt0.diff.png` shows the texels that changed in magenta over the unchanged ones in grey.

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

Two captures, of the same game run twice or of a scene before and after a change, are compared draw by draw:

```
locum-cli diff before after                # how many draws match, and what differs in how many of them
locum-cli diff before after --draws        # each matched draw that differs, line by line
```

Draws are matched by what stays the same across a relaunch: their shaders, the call and its counts, the upload hashes
of the textures and buffers they read (or their shapes, for what the frame wrote), the shapes of their targets, and
how many draws like them came before. Matched draws are compared constant by constant (named from the shaders),
input by input and state by state.

A capture's textures and buffers are named from the files they were made from with an index of those files, keyed
the way captures key contents (a texture's canonical layout is a DDS file's order, so a DDS file is keyed whole and
mip by mip as it is):

```
locum-cli index files.db DIR...            # key every DDS file under each DIR
locum-cli --index files.db join last       # what each texture and buffer the frame used was made from
locum-cli --index files.db draw last 120   # bound resources are shown with their file's name
```

A resource is matched by its upload, its contents, or subresource by subresource (a texture made from fewer mips
than its file holds, or from one face). `LOCUM_INDEX` (paths separated by `;`) stands in for `--index`. A game's own
extractor can add what it reads from its archives through `locum_capture::index`.

Constants are named from each shader's reflection (its `RDEF` part as `dxbc-reader` reads it, Shader Model 4.0 to 5.1,
register spaces included), as `cbuffer.variable[index].component`. PNGs of depth and other one-channel images are
stretched from their smallest to largest value, float colour is tone-mapped, and `--range LO:HI` sets the mapping.

## Direct3D 12 (in progress)

A third DLL, `d3d12.dll`, passes every Direct3D 12 call through to the system's, as `d3d11.dll` does for Direct3D 11;
it keeps the system DLL's exports by name and ordinal (Elden Ring imports `D3D12CreateDevice` by ordinal 101). Devices,
queues, command lists and every other object a game gets are wrapped. The log records the first use of each interface
and method, with periodic counts and journal timings. Object identities remain in memory and in captures.

The inspection foundation records command lists continuously, so lists and bundles recorded before a capture request
retain their original generations. A request arms at the next non-test Present and closes at the following Present.
Captures use schema 2 with `api=d3d12`: queue submissions, list executions, fence values, descriptor versions, root
signatures, root arguments and PSO inputs are authoritative records. The CLI and viewer project these into their
draw, resource, binding and pipeline views. Display order across queues is not a GPU execution order; queue order
and fence operations are retained separately. CPU-consumed descriptor definitions are frozen when recorded, while
GPU descriptor tables use the versions present at submission.

This milestone remains explicitly incomplete: GPU first-use snapshots, pass-end/output checkpoints and D3D12 replay
are subsequent work. Upload buffers are copied at armed submissions; these CPU observations assume the application's
host writes obey D3D12 synchronization and are not a substitute for GPU checkpoints. Captures retain the original
shader bytes regardless of whether reflection succeeds. Resource reachability comes from API state and root
signatures, not a single shader parser's conclusions.

`LOCUM_D3D12_JOURNAL=off` disables the journal for cost comparisons using the same DLL. The default is on. Disarmed
runs retain command/descriptor history but do not copy or hash upload-buffer contents. Ten-second log summaries
measure recording, submission, lookup, resource destruction, upload and reset costs; nested lookup timings overlap
recording and must not be added to it. Compare native, off and on on the same route, including game CPU time: a
vsync-limited frame rate alone cannot establish CPU overhead.

DXIL inspection uses pinned `dxcontainer`, `dxil-reader` and `iridia-dxil`, cross-checked with Microsoft's shader or
library reflection. `LOCUM_DXCOMPILER` selects a `dxcompiler.dll`; otherwise the usual DLL search applies. The CLI
records disagreements with the file, shader hash, crate revisions and both answers. Native disassembly and optional
WGSL (iridia lifts DXIL and DXBC alike) are available without allowing display analysis to narrow capture coverage:

```
locum-cli shader last HASH -d --wgsl
locum-cli shader-file shader.dxil --evidence comparison.json
locum-cli cb last 120 ps 3 --space 7 --raw
```

Shader archival is independent of the journal. Successful graphics, compute and stream PSO creates and library loads
save their original shader bytes under `shaders/<XXH3-128>.<stage>.dxbc` in `LOCUM_DIR`; this includes DXIL containers,
mesh/amplification shaders and whole DXR libraries (`lib.dxbc`) from `CreateStateObject` and `AddToStateObject`.
Reflection never filters these bytes. Opaque inputs to pipeline-library creation use `<hash>.pipeline-library.bin`;
cached PSO inputs and `GetCachedBlob` outputs use `<hash>.cached-pso.bin`. Each distinct opaque blob logs its hash,
size and path once per process. Serialize logs only the library identity and caller capacity; it does not inspect or archive the output.
First observations write synchronously from borrowed bytes, with atomic file publication and retries after errors;
repeated content only hashes and checks the in-memory index. Large new caches therefore incur first-seen hashing and
disk I/O without an additional in-memory copy or unbounded writer queue.
An unrecognized PSO stream type logs its first occurrence once per type per process.

`cargo xtask smoke --api d3d12` runs a program through it on WARP with the debug layer on (copies, dispatches, fences,
pipeline states and libraries, placed resources, barriers, swap chain present and resize); `cargo xtask smoke` stays
the Direct3D 11 suite. The wrappers are generated: `cargo xtask gen` (policy in `xtask/src/d3d12.rs`).

## Crash investigation

For a targeted crash investigation, `settings.txt` can opt into a full-memory dump at listed addresses relative to the exe:

```text
dumper C:/path/to/locum-cli.exe
dumps C:/path/to/research/dumps
crash-av-rva 0x1f64f4d, 0X1F64F56
```

The comma-separated list accepts hexadecimal RVAs with `0x`, `0X`, or no prefix. With this setting absent, no exception
handlers or crash helper are installed. When enabled, locum prepares a hidden CLI helper and named events, then installs
a vectored exception handler. The first access violation at a listed RVA requests a snapshot, even if the application
subsequently handles that exception. Replacing the application's unhandled filter does not remove this observer.
An unhandled-filter fallback also observes fatal access violations at any address, including Windows and driver DLLs,
provided the application's exception chain reaches it. Listed first-chance faults and fallback faults share one dump
request per process; after a handled fault consumes it, later faults do not produce another dump in that process.

The faulting thread publishes its original exception pointers and waits up to 120 seconds for the snapshot. The vectored
handler always continues exception search; the fallback chains the previous filter or continues search. Neither callback
allocates, logs, or changes the exception context. The CLI uses the same PSS full-memory dump path as captures, with
the original exception stream for `.ecxr`. Normal exit releases the waiting helper without a dump. An earlier vectored
handler or debugger that consumes the exception can prevent observation; CPU fixtures do not establish game pickup.

## References

DXVK and RenderDoc are both open source and solve the same problems (object identity, DXGI, deferred contexts); when
in doubt, locum does what they do.
