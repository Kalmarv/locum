# locum

A stand-in `d3d11.dll` and `dxgi.dll` for looking inside a Direct3D 11 game. The game asks for a device and gets
locum's; every call it makes arrives at locum first, which passes it on to the real runtime. Nothing in the system's
DLLs is patched, and the game keeps rendering on its own driver.

Status: early. The wrappers forward everything; recording, readback and overrides are being built.

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

## Building

```
cargo build --release
```

The DLLs are `target/release/locum.dll`, `d3d11.dll` and `dxgi.dll`. Regenerate the vtables after changing the
`windows` version with `cargo xtask gen`.

## References

DXVK and RenderDoc are both open source and solve the same problems (object identity, DXGI, deferred contexts); when
in doubt, locum does what they do.

## Licence

GPL-3.0-or-later.
