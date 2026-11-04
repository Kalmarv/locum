//! The dx and iridia commits this build links, from the workspace's Cargo.lock, for `shader-file --evidence`; and
//! an 8 MiB main-thread stack: clap's parser for this many commands needs more than Windows' default 1 MiB in debug
//! builds.
fn main() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/STACK:8388608");
    }
    let lock = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.lock");
    println!("cargo:rerun-if-changed={}", lock.display());
    let text = std::fs::read_to_string(&lock).expect("read the workspace Cargo.lock");
    for (package, variable) in [
        ("dxil-reader", "LOCUM_DX_REV"),
        ("iridia-dxil", "LOCUM_IRIDIA_REV"),
    ] {
        let commit = text
            .split("[[package]]")
            .find(|entry| entry.lines().any(|line| line == format!("name = \"{package}\"")))
            .and_then(|entry| {
                entry
                    .lines()
                    .find_map(|line| line.strip_prefix("source = \"git+"))
            })
            .and_then(|source| source.trim_end_matches('"').rsplit_once('#'))
            .map(|(_, commit)| commit.to_owned())
            .unwrap_or_else(|| panic!("Cargo.lock has no git source for {package}"));
        println!("cargo:rustc-env={variable}={commit}");
    }
}
