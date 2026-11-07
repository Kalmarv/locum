# Changelog

## Unreleased

- A wrapper is another face of the real object: its `AddRef` and `Release` are the object's, one wrapper is made per
  object, and it is freed when the object is destroyed. A game sees the counts it would see without locum: a child
  holds its device, the immediate context's first reference holds the device, a back buffer holds its swap chain, an
  adapter its factory. Before, each wrapper counted on its own, and a Direct3D 12 heap a placed resource lived in was
  freed when the game released it, so a later call on it crashed (vkd3d's `test_create_placed_resource`).
- `GetDevice` on what a Direct3D 11 device made (a buffer, a texture's DXGI surface, a class instance) answers with
  locum's device, and every way to the adapter and factory (`IDXGIDevice::GetAdapter`, `IDXGIAdapter::GetParent`, an
  output's `GetParent`) with locum's wrappers. Before, it handed out the real device, and what a game made with that
  passed locum by.
- A wrapper answers only to the interfaces its real object answers to: a factory from `CreateDXGIFactory` refuses
  `IDXGIFactory1`, as the system's does.
- `D3D11CreateDevice` and `D3D11CreateDeviceAndSwapChain` clear the out pointers they are given and make nothing when
  given none to fill, as the system's do.
- The Agility SDK's `D3D12Core.dll` and `d3d12SDKLayers.dll`, and `d3d10warp.dll`, in a game's folder are the
  runtime: the factories they ask DXGI for are the real ones.
- What a Direct3D 12 capture makes beside a game's queue (a helper queue, fences) goes when the queue is destroyed.
  Before, it lived as long as the process and held the device.
- A command list run on a deferred context is not held by that context's recording. Before, the recording held it
  (and so its device) for as long as the list being recorded, or the context, lived.
- The first present that reports the device removed (or reset) logs the device's reason.
- `cargo xtask smoke` runs what cargo built where `CARGO_TARGET_DIR` points, and stages there (`smoke/`,
  `smoke-d3d12/`). Before, it built there but copied `target/release`'s DLLs, so a smoke could pass on old ones.
- `locum-cli conform` (and `--layer`) loads what a layer's DLLs forward their exports to from the layer's folder:
  locum's stand-ins forward to `locum.dll`, which was otherwise the first of that name beside locum-cli or on PATH
  (cargo's target folders under `cargo xtask smoke`), or none, and the layer replay failed to load the system's
  `d3d11.dll`. The machine line names those modules too.
- A copy of locum's from a buffer the game's list had not moved by a barrier (a build's inputs, armed or not;
  ExecuteIndirect's records and DispatchRays' tables while a capture is armed) no longer leaves the buffer in a state
  the game does not know. The copy promoted it to COPY_SOURCE as well, so the game's next barrier from the state it
  expected was an error the debug layer reports, and the runtime's tracking of the buffer went wrong. Locum now moves
  it back right before that barrier, or between the lists of one ExecuteCommandLists, and joins COPY_SOURCE first
  where an earlier list of the call left the buffer in a state of its own.
- A top-level build in the frame whose instances are an array of pointers reads the buffers its pointers point into,
  so a plan makes them and they get first-use copies. Before, replay left out every instance only a pointer named.
  Such a build before the frame is still not captured (see the docs for why): replay builds it empty, and verify says
  so.
- An ExecuteIndirect whose command signature runs DispatchRays has its records read, as other records with addresses
  are: the shader tables they name are among its reads, so a plan makes them, and a table nothing else in the frame
  names is copied after it. Before, its records were a `d3d12-indirect-arguments` gap.
- Raytracing replays. A DispatchRays, direct or through ExecuteIndirect, runs with its state object made again
  (collections and AddToStateObject included) on copies of its shader tables in which each record's identifier and
  local root arguments are replay's; acceleration structures the frame began with are built again from the inputs the
  capture copied at their builds, and the frame's builds, updates, copies and inline traces (RayQuery) run as it ran
  them. Capture reads what each record's local root arguments name (the buffers and descriptors a dispatch reads and
  writes through its tables alone), at the Present and, for a direct DispatchRays, as its submission is made, so what
  they name gets a first-use copy; copies what a DispatchRays writes through UAVs, as a dispatch's outputs; keeps each
  state object's subobjects and the identifiers the game asked for; and names each acceleration structure view's
  buffer. Before, replay ran no DispatchRays and verify said so. Verified on WARP and NVIDIA hardware, a capture made
  on one replayed on the other, through callable shaders, compacted, updated and cloned structures, and instances by
  pointer.
- Multisampled textures are captured sample by sample, bit for bit, depth and stencil included: their contents and
  their pass-end targets on both APIs. Before, Direct3D 11 captured their resolve, and Direct3D 12 none of them (a
  gap). Draws read the samples, since no copy can: a twin of the texture's typeless family, and a pixel shader per
  sample loading its raw bits (a depth as a float turned back into its 24 or 32 bits) into a one-sample image.
- Replay writes every sample of a multisampled texture's contents the same way (a draw a sample, depth through
  SV_Depth, stencil a bit at a time), and reads its targets back sample by sample, so verify compares them. Before,
  they started empty, and Direct3D 12 replay could not read them back. A capture from 0.1.4 or earlier holds their
  resolve: replay starts them empty and says so.
- A multisampled texture's subresource is its samples' images in sample order. Its `subresources` row, a target read
  from it, `export` (PNG, DDS or raw), the viewer and `pixel --draws` (which shows each sample) take them as one image
  of the samples stacked top to bottom; a whole multisampled texture exports to DDS with each slice's samples as array
  slices.
- The smokes' multisampled textures include every colour format whose samples are read through a float view
  (B8G8R8A8, B8G8R8X8, B5G6R5, B5G5R5A1, B4G4R4A4, A8, R10G10B10_XR_BIAS) where the device renders it with 4 samples,
  each sample different, and name those it cannot.

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
