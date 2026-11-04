# locum viewer

A local website over a capture: what the frame did, in the order the GPU did it, and every value a draw saw.

```
cd viewer
bun install
bun run viewer --db path/to/captures/NAME          # a capture.db, or the folder holding one
```

It prints the address (`http://127.0.0.1:5170/#/c/<id>`); open it in a browser. Without `--db` it starts on the open
page, which lists the captures under a folder, opens one by path, or takes a capture.db from the browser (drop it
on the page, or choose it: the File System Access API where there is one, localhost being a secure context).

Pictures, disassembly and verify come from `locum-cli`: `--cli PATH`, else `LOCUM_CLI` (a `viewer/.env` line works,
Bun reads it), else `PATH`, else `../target/release`. The blob store is `--blobs DIR`, else `LOCUM_BLOBS`, else the
one the capture names in its meta, so a capture.db sent from the browser still finds its pictures.

| option | |
|---|---|
| `--db PATH` | the capture to open at start |
| `--blobs DIR` | the blob store |
| `--dir DIR` | the folder the open page lists (default: the `--db`'s captures folder, `LOCUM_DIR`, or `%LOCALAPPDATA%/locum`) |
| `--port N` | 5170, or the next free one |
| `--host H` | 127.0.0.1; the API reads any capture.db the server can see, so keep it local |

Ctrl+C stops it; so does ending the `bun run` that started it (the server exits when its parent does).

Every page is in the address, so links and reloads work:

| address | |
|---|---|
| `#/c/<id>` or `#/c/<id>/info` | the capture's summary |
| `#/c/<id>/pipeline/<seq>` | the timeline at event `seq` (the `events` table's numbering) |
| `#/c/<id>/pipeline/draw/<n>` | the timeline at draw `n`, the number `locum-cli draws` and `draw` use |
| `#/c/<id>/resources/<id>` | one resource |
| `#/c/<id>/shaders/<hash>` | one shader |
| `#/c/<id>/sql` | a SQL console |

## What it shows

- **info**: what the capture holds and lacks (as `locum-cli info` says it), its meta, and `locum-cli verify` on request.
- **pipeline**: every event in GPU order, grouped by pass, filtered by kind, shader, pass, resource or text; `j`/`k`
  step events, `J`/`K` draws, `[`/`]` passes. A draw shows its shaders, state, viewports, every binding, constants by
  name (from the shader's reflection) as float, int, uint or hex, its pass's targets read back after the pass, and
  the textures it reads. A draw or dispatch whose shader's writes depend on thread order shows what the game's GPU
  wrote through its UAVs (the `results` table).
- **pictures**: zoom (wheel), pan (drag), `f` fit, `1` actual size; hover or click for a texel's value as stored
  (raw bytes and decoded channels); rgb (alpha dropped, the default), rgba or one channel; value range auto, clamp,
  tonemap or lo:hi. A target can be shown before its pass (`b`) and as a diff of the bytes that changed (`d`).
- **resources**: description, creation stack, the events that read or wrote it, the draws it was bound to, its
  contents (pictures for textures, bytes for buffers) and its read-backs.
- **shaders**: reflection, the draws that ran it, disassembly.
- **sql**: read-only queries over the capture.db, with its schema beside.

The capture.db is opened read-only and only while being read (closed 1.5 s after the last query), so the viewer never
stops a new capture from replacing it. Outputs of locum-cli are cached in `viewer/.cache` per capture file and
locum-cli build.

## Development

`bun run dev` serves the front end through vite with hot reload (the API on 5171); `bun run check` type-checks;
`bun test` runs the tests. `test/cdp.ts` drives a headless Chrome or Edge (no window, its profile in `.cache`) for
checks of the pages.
