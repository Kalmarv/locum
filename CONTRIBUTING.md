# Contributing

## Before pushing

Besides `cargo fmt`, `cargo clippy --workspace --all-targets` and `cargo test --workspace`, run the slim shader set
through locum's shader reading, with `GAME_DATA` naming a real-data configuration whose `[run] require = 'gate'`:
`cargo test --locked -p locum-capture slim -- --nocapture`. It reads primase-testdata's `oracles.corpus.general/slim`
(8,287 programs; tests/inputs/README.md in `capture`) and prints how many programs read by each path; quote those
counts in the push report. Without `GAME_DATA` it skips, which is what GitHub's CI runs.

## Releasing

1. Raise the version in the workspace and in the crates' dependencies on each other (a patch, 0.x.Y, for fixes and data
   or manifest additions; a minor, 0.X.0, for new API or any breaking change before 1.0; then `cargo check`, so
   `Cargo.lock` follows) and put the change's CHANGELOG lines under a `## <version> (<date>)` heading, in one commit.
2. Push it to main. Once every check has passed, release-plz publishes each crate whose version crates.io does not have
   yet (trusted publishing: no token is stored anywhere), tags the commit `v<version>` and creates the GitHub Release
   with that CHANGELOG section as its notes. Then dist (`.github/workflows/release.yml`) builds the binaries and
   attaches them to the Release, with their checksums.

A commit that raises no version only runs CI. Never release by hand. If a run fails after the tag was made, start
`release.yml` by hand with that tag.

A new crate's first version can't come from trusted publishing: the token holder publishes it by hand, then adds its
trusted-publishing config (workflow `ci.yml`).

To test the DLLs as they will ship, run `dist build --artifacts=local --target x86_64-pc-windows-msvc` (dist 0.34.0)
on Windows: it writes `target/distrib/locum-x86_64-pc-windows-msvc.zip`. Unpack it and run `cargo xtask smoke --dlls
DIR`, `cargo xtask smoke --api d3d12 --dlls DIR` and, on a GPU, `--hardware`. After the release, the same on the
Release's own zip checks what CI built. The zip's DLLs are `locum`'s own and the shims' (`[package.metadata.dist]` in
`locum/Cargo.toml`).
