//! The dx and iridia builds this build links, from the Cargo.lock it resolves with, for `shader-file --evidence`; an
//! 8 MiB main-thread stack: clap's parser for this many commands needs more than Windows' default 1 MiB in debug
//! builds; and Direct3D's DLLs delay-loaded.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
        // Loaded by full path before their first call (src/runtime.rs), so neither locum's stand-ins nor anything
        // else of those names beside the executable is used: the tests too, which replay.
        for dll in ["d3d11.dll", "dxgi.dll", "d3d12.dll"] {
            println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
        }
        println!("cargo:rustc-link-arg=delayimp.lib");
        // A target that calls none of one (the unit tests, whose replays the linker drops) has nothing to delay.
        println!("cargo:rustc-link-arg=/IGNORE:4199");
    }
    // the workspace's lock in the repository, the package's own once published (a package with binaries carries one)
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = [manifest.join("Cargo.lock"), manifest.join("../Cargo.lock")]
        .into_iter()
        .find(|path| path.is_file())
        .expect("a Cargo.lock beside the package or the workspace");
    println!("cargo:rerun-if-changed={}", lock.display());
    let text = std::fs::read_to_string(&lock).expect("read Cargo.lock");
    for (package, variable) in [("dxil-reader", "LOCUM_DX"), ("iridia-dxil", "LOCUM_IRIDIA")] {
        let entry = text
            .split("[[package]]")
            .find(|entry| entry.lines().any(|line| line == format!("name = \"{package}\"")))
            .unwrap_or_else(|| panic!("Cargo.lock has no {package}"));
        let field = |name: &str| {
            entry
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{name} = \"")))
                .map(|value| value.trim_end_matches('"').to_owned())
        };
        let version = field("version").unwrap_or_else(|| panic!("Cargo.lock has no version of {package}"));
        // a crates.io build is named by its version, a git build by its commit
        let build = match field("source") {
            Some(source) if source.starts_with("registry+") => format!("{package} {version}"),
            Some(source) if source.starts_with("git+") => {
                let (_, commit) = source
                    .rsplit_once('#')
                    .unwrap_or_else(|| panic!("{package}: no commit in {source}"));
                format!("{package} {version} at {commit}")
            }
            other => panic!("{package}: a source this build cannot name ({other:?})"),
        };
        println!("cargo:rustc-env={variable}={build}");
    }
}
