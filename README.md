# locum

See everything a Direct3D 11 game draws in a frame: every draw and dispatch, its shaders, constants, textures,
buffers and state, and the render targets after each pass, in one SQLite file you can browse, query and replay.
Like RenderDoc, without injecting anything: three DLLs go in the game's folder.

- **Capture** a frame with everything the GPU did and read.
- **Browse** it in a local web viewer or with `locum-cli`, or query it with SQL.
- **Trace** textures and buffers to the files they came from, and constants to the game's memory.
- **Change** a draw (skip it, swap a shader, set a constant), live in the game or offline.
- **Replay** the frame offline, checking every render target against the game's bit for bit.

## Quick start

Requires Windows, a 64-bit Direct3D 11 game, Rust, and [Bun](https://bun.sh) for the viewer.

```bash
cargo build --release
```

1. Copy `locum.dll`, `d3d11.dll` and `dxgi.dll` from `target/release/` next to the game's `.exe` (for Unreal games,
   the one under `Binaries/Win64`). For a Direct3D 12 game, copy `locum.dll`, `d3d12.dll`, `dxgi.dll` and
   `nvapi64.dll`.
2. Run the game. To capture a frame, create an empty file named `capture` in `%LOCALAPPDATA%\locum`. A few seconds
   later the frame is in `captures/frame-<n>/`.
3. Open it:

   ```bash
   cd viewer && bun install && bun run viewer --db "%LOCALAPPDATA%\locum\captures\frame-<n>"
   ```

4. Delete the three DLLs from the game's folder when done.

## Usage

`locum-cli` reads captures; `last` names the newest.

| Command | What it shows |
|---|---|
| `locum-cli draws last` | every draw and dispatch, in GPU order |
| `locum-cli draw last 120` | one draw: state, everything bound, constants by name, targets after it |
| `locum-cli shader last 9f2c -d` | a shader's constant buffers and disassembly |
| `locum-cli trace last 120 rt1.z` | what a target lane is made of: textures by channel, constants by name and value, vertex attributes |
| `locum-cli resource last 812` | a resource: where it was made, its contents, what the frame did with it |
| `locum-cli export last 812 -o tex.dds` | a texture or buffer as the frame began |
| `locum-cli pixel last 120:rt0 960 540` | one texel in every readback of a target: which passes changed it |
| `locum-cli find last 0.35` | which constant holds a value, and the draws that saw it |
| `locum-cli sql last "SELECT ..."` | anything else |
| `locum-cli replay last 120 --rule "skip ps=9f2c"` | the draw replayed with a change, before and after |
| `locum-cli verify last` | the whole frame replayed and compared with the game's |

`locum-cli --help` lists the rest (diff two captures, memory dumps, file indexes).

## Configuration

| Variable | Default | Description |
|---|---|---|
| `LOCUM_DIR` | `%LOCALAPPDATA%\locum` | where locum writes its log, shaders and captures |
| `LOCUM_BLOBS` | `blobs/` in that folder | the store of resource contents shared by captures |
| `LOCUM_INDEX` | none | indexes of the game's files, for naming resources (`;`-separated) |

`settings.txt` and `overrides.txt` in that folder set capture options and change draws live; see
[docs/README.md](docs/README.md#files-and-settings).

## Limits

- 64-bit only. Direct3D 12 captures record every command, state, shader and constant, the contents each resource
  had when the frame first used it, the targets each pass ended with and what draws and dispatches wrote through
  UAVs, and replay; they do not yet record the contents of reserved (tiled) resources. Vulkan games run as normal,
  unseen.
- Don't use it on games with anti-cheat.

## Development

```bash
cargo build --release
cargo xtask smoke   # runs small Direct3D programs through locum and verifies their captures on the GPU and WARP
cargo xtask gen     # regenerates the COM wrappers after a windows crate update
```

[docs/README.md](docs/README.md) covers how locum works, every file it writes, the capture's tables, and
replay. [viewer/README.md](viewer/README.md) covers the viewer.

## License

[GPL-3.0-or-later](LICENSE)
