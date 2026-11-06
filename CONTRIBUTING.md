# Contributing

## Releasing

A release is a commit that changes a version. Once every CI job has passed on main, CI's release job
(`.github/release.py`) publishes each crate whose version crates.io does not have yet, in dependency order (trusted
publishing: no token is stored anywhere), tags the commit `v<version>` (`<package>-v<version>` for a package with a
version of its own) and creates the GitHub Release with the commits since the previous release as its notes and the binaries attached (`<bin>-<version>-<target>.zip` / `.tar.gz` with `SHA256SUMS`).
A commit that changes no version only runs CI.

1. Bump the version when a change is worth releasing: a patch (0.x.Y) for fixes and data or manifest additions, a minor
   (0.X.0) for new API or any breaking change before 1.0. Run `cargo check` so `Cargo.lock` follows.
2. Push to main. Never publish by hand; a new crate's first release is the one exception (the token holder makes it,
   then the crate gets its trusted-publishing config).
