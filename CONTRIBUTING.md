# Contributing

## Releasing

A release is a commit that changes a version. Once every CI job has passed on main, CI's release job
(`.github/release.py`) publishes each crate whose version crates.io does not have yet, in dependency order (trusted
publishing: no token is stored anywhere), tags the commit `v<version>` (`<package>-v<version>` for a package with a
version of its own) and creates the GitHub Release. Its notes are the version's `CHANGELOG.md` section (else the
commits since the previous release), and the downloads `RELEASE_BINS` in `ci.yml` lists are attached with
`SHA256SUMS`: `locum-cli-<version>-<target>.zip` and `locum-dlls-<version>-<target>.zip`, the DLLs that go beside a
game. A word of `RELEASE_BINS` is one binary or library target (`locum-cli`), or an archive of several
(`locum-dlls=locum+d3d11+dxgi+d3d12+nvapi64`); `:windows` ships it only for Windows targets. The archive's targets
are built by one `cargo build`. A commit that changes no version only runs CI.

1. Bump the version when a change is worth releasing: a patch (0.x.Y) for fixes and data or manifest additions, a minor
   (0.X.0) for new API or any breaking change before 1.0, in the workspace and in the crates' dependencies on each
   other. Run `cargo check` so `Cargo.lock` follows, and add the version's section to `CHANGELOG.md`.
2. Optionally, test the DLLs as they will ship: `python .github/release.py build x86_64-pc-windows-msvc` with a
   `PLAN` naming the downloads writes the archives to `dist/`; unpack `locum-dlls` and run `cargo xtask smoke --dlls
   DIR`, `cargo xtask smoke --api d3d12 --dlls DIR` and, on a GPU, `--hardware`. After the release, the same on the
   Release's own zip checks what CI built.
3. Push to main. Never publish by hand; a new crate's first release is the one exception (the token holder makes it,
   then the crate gets its trusted-publishing config).
