//! `cargo xtask gen`: writes `locum/src/vtables/*.rs`, the forwarding vtables of the interfaces locum wraps.
//!
//! The `windows` crate publishes every COM interface's vtable as a `#[repr(C)]` struct of `extern "system"` function
//! pointers with the exact ABI (`ID3D11DeviceContext_Vtbl` and so on). For each wrapped interface this reads that
//! struct and its bases from the crate's source and generates:
//! - a trait with one method per vtable entry. Every method but the three of IUnknown has a default that calls the
//!   wrapped object's own entry, so a wrapper forwards everything it does not override;
//! - one `extern "system"` thunk per entry, which turns the COM `this` back into the wrapper and calls the trait method;
//! - a `const fn` that builds the whole vtable from those thunks for a given wrapper type.
//!
//! Regenerate after changing the `windows` version. The generated files are checked in.

mod d3d11;
mod d3d12;
mod d3d12_capture;

use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use quote::ToTokens;
use syn::{Fields, Item, Type};

/// A wrapped interface: the generated module's name, the trait's name, the most derived interface and the module
/// (under `windows::`) whose source defines it.
struct Class {
    module: &'static str,
    trait_name: &'static str,
    interface: &'static str,
    source: &'static [&'static str],
}

const CLASSES: &[Class] = &[
    Class {
        module: "d12_debug_base",
        trait_name: "DebugBase12Methods",
        interface: "ID3D12Debug6",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_device",
        trait_name: "Device12Methods",
        interface: "ID3D12Device14",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_queue",
        trait_name: "Queue12Methods",
        interface: "ID3D12CommandQueue",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_list",
        trait_name: "List12Methods",
        interface: "ID3D12GraphicsCommandList10",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_allocator",
        trait_name: "Allocator12Methods",
        interface: "ID3D12CommandAllocator",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_resource",
        trait_name: "Resource12Methods",
        interface: "ID3D12Resource2",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_heap",
        trait_name: "Heap12Methods",
        interface: "ID3D12Heap1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_fence",
        trait_name: "Fence12Methods",
        interface: "ID3D12Fence1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_descriptors",
        trait_name: "Descriptors12Methods",
        interface: "ID3D12DescriptorHeap",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_root",
        trait_name: "Root12Methods",
        interface: "ID3D12RootSignature",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_pso",
        trait_name: "Pso12Methods",
        interface: "ID3D12PipelineState",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_signature",
        trait_name: "Signature12Methods",
        interface: "ID3D12CommandSignature",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_query",
        trait_name: "Query12Methods",
        interface: "ID3D12QueryHeap",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_library",
        trait_name: "Library12Methods",
        interface: "ID3D12PipelineLibrary1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_state",
        trait_name: "State12Methods",
        interface: "ID3D12StateObject",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_properties",
        trait_name: "Properties12Methods",
        interface: "ID3D12StateObjectProperties1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_debug",
        trait_name: "Debug12Methods",
        interface: "ID3D12Debug1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_info",
        trait_name: "Info12Methods",
        interface: "ID3D12InfoQueue1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_protected",
        trait_name: "Protected12Methods",
        interface: "ID3D12ProtectedResourceSession1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_meta",
        trait_name: "Meta12Methods",
        interface: "ID3D12MetaCommand",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_cache",
        trait_name: "Cache12Methods",
        interface: "ID3D12ShaderCacheSession",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_work_graph",
        trait_name: "WorkGraph12Methods",
        interface: "ID3D12WorkGraphProperties",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_debug2",
        trait_name: "Debug2_12Methods",
        interface: "ID3D12Debug2",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_debug_device",
        trait_name: "DebugDevice12Methods",
        interface: "ID3D12DebugDevice2",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_debug_device1",
        trait_name: "DebugDevice1_12Methods",
        interface: "ID3D12DebugDevice1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_debug_queue",
        trait_name: "DebugQueue12Methods",
        interface: "ID3D12DebugCommandQueue1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_debug_list",
        trait_name: "DebugList12Methods",
        interface: "ID3D12DebugCommandList3",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_debug_list1",
        trait_name: "DebugList1_12Methods",
        interface: "ID3D12DebugCommandList1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_dred",
        trait_name: "Dred12Methods",
        interface: "ID3D12DeviceRemovedExtendedData2",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_dred_settings",
        trait_name: "DredSettings12Methods",
        interface: "ID3D12DeviceRemovedExtendedDataSettings2",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_sdk",
        trait_name: "Sdk12Methods",
        interface: "ID3D12SDKConfiguration1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_factory",
        trait_name: "Factory12Methods",
        interface: "ID3D12DeviceFactory",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_configuration",
        trait_name: "Configuration12Methods",
        interface: "ID3D12DeviceConfiguration1",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_tools",
        trait_name: "Tools12Methods",
        interface: "ID3D12Tools",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_sharing",
        trait_name: "Sharing12Methods",
        interface: "ID3D12SharingContract",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_gbv",
        trait_name: "Gbv12Methods",
        interface: "ID3D12GBVDiagnostics",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_write_tracking",
        trait_name: "WriteTracking12Methods",
        interface: "ID3D12ManualWriteTrackingResource",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_assistant",
        trait_name: "Assistant12Methods",
        interface: "ID3D12SwapChainAssistant",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_guest",
        trait_name: "Guest12Methods",
        interface: "ID3D12VirtualizationGuestDevice",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_lifetime_owner",
        trait_name: "LifetimeOwner12Methods",
        interface: "ID3D12LifetimeOwner",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "d12_lifetime",
        trait_name: "Lifetime12Methods",
        interface: "ID3D12LifetimeTracker",
        source: &["Win32", "Graphics", "Direct3D12"],
    },
    Class {
        module: "device",
        trait_name: "DeviceMethods",
        interface: "ID3D11Device5",
        source: &["Win32", "Graphics", "Direct3D11"],
    },
    Class {
        module: "context",
        trait_name: "ContextMethods",
        interface: "ID3D11DeviceContext4",
        source: &["Win32", "Graphics", "Direct3D11"],
    },
    Class {
        module: "factory",
        trait_name: "FactoryMethods",
        interface: "IDXGIFactory7",
        source: &["Win32", "Graphics", "Dxgi"],
    },
    Class {
        module: "swapchain",
        trait_name: "SwapChainMethods",
        interface: "IDXGISwapChain4",
        source: &["Win32", "Graphics", "Dxgi"],
    },
    Class {
        module: "dxgi_device",
        trait_name: "DxgiDeviceMethods",
        interface: "IDXGIDevice4",
        source: &["Win32", "Graphics", "Dxgi"],
    },
    Class {
        module: "adapter",
        trait_name: "AdapterMethods",
        interface: "IDXGIAdapter4",
        source: &["Win32", "Graphics", "Dxgi"],
    },
];

/// Source modules whose vtable structs are read (bases can live in another module than the interface).
const MODULES: &[&[&str]] = &[
    &["Win32", "Graphics", "Direct3D12"],
    &["Win32", "Graphics", "Direct3D11"],
    &["Win32", "Graphics", "Dxgi"],
];

/// One vtable entry: its name, its parameters after `this`, and its return type (absolute paths).
struct Entry {
    name: String,
    params: Vec<String>,
    ret: Option<String>,
}

/// A vtable struct: the base it extends (`None` for IUnknown) and its own entries.
struct Vtbl {
    base: Option<String>,
    entries: Vec<Entry>,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .to_path_buf();
    // `--dlls DIR`: the smoke runs the DLLs in DIR (a release's, say) instead of building them.
    let dlls = args.windows(2).find(|p| p[0] == "--dlls").map(|p| {
        let dir = PathBuf::from(&p[1]);
        dir.canonicalize()
            .unwrap_or_else(|e| panic!("--dlls {}: {e}", dir.display()))
    });
    match args.first().map(String::as_str) {
        Some("gen") => generate_all(&root),
        Some("smoke") if args.windows(2).any(|p| p[0] == "--api" && p[1] == "d3d12") => d3d12::smoke(
            &root,
            args.iter().any(|a| a == "--hardware"),
            args.iter().any(|a| a == "--debug"),
            dlls.as_deref(),
        ),
        Some("smoke") => smoke(&root, args.iter().any(|a| a == "--debug"), dlls.as_deref()),
        _ => {
            eprintln!(
                "usage: cargo xtask gen | smoke [--debug] [--dlls DIR] | smoke --api d3d12 [--hardware] [--debug] \
                 [--dlls DIR]"
            );
            std::process::exit(2);
        }
    }
}

/// Where cargo builds this workspace: `CARGO_TARGET_DIR` (from `root` if relative), else `root/target`.
fn target_dir(root: &Path) -> PathBuf {
    target_dir_from(root, std::env::var_os("CARGO_TARGET_DIR"))
}

fn target_dir_from(root: &Path, set: Option<std::ffi::OsString>) -> PathBuf {
    match set {
        Some(dir) if !dir.is_empty() => root.join(dir),
        _ => root.join("target"),
    }
}

/// Builds `packages` for release, leaving out the DLLs' where `dlls` gives them.
fn build(root: &Path, packages: &[&str], dlls: Option<&Path>) {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(root).args(["build", "--release"]);
    for p in packages {
        if dlls.is_none() || !(*p == "locum" || p.starts_with("locum-shim-")) {
            command.args(["-p", p]);
        }
    }
    let status = command.status().expect("run cargo build");
    assert!(status.success(), "build failed");
}

/// Copies `files` from the release build (the one [`build`] made, in [`target_dir`]) into `stage`; the DLLs from `dlls`
/// where it is given.
fn stage_files(root: &Path, stage: &Path, files: &[&str], dlls: Option<&Path>) {
    let release = target_dir(root).join("release");
    for file in files {
        let from = match dlls {
            Some(dir) if file.ends_with(".dll") => dir.join(file),
            _ => release.join(file),
        };
        std::fs::copy(&from, stage.join(file)).unwrap_or_else(|e| panic!("copy {}: {e}", from.display()));
        if dlls.is_some() && file.ends_with(".dll") {
            println!("smoke: {file} from {}", from.display());
        }
    }
}

/// Builds the release DLLs (or takes those in `dlls`) and the smoke program, puts them in `smoke/` in the target
/// directory and runs it there, with locum's log in `smoke/log/`.
fn smoke(root: &Path, debug: bool, dlls: Option<&Path>) {
    build(
        root,
        &[
            "locum",
            "locum-shim-d3d11",
            "locum-shim-dxgi",
            "locum-smoke",
            "locum-cli",
        ],
        dlls,
    );
    let stage = target_dir(root).join("smoke");
    let log_dir = stage.join("log");
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&log_dir).expect("create the smoke folder");
    stage_files(
        root,
        &stage,
        &[
            "locum.dll",
            "d3d11.dll",
            "dxgi.dll",
            "locum-smoke.exe",
            "locum-smoke-late.exe",
            "locum-smoke-overlay.exe",
            "locum-cli.exe",
        ],
        dlls,
    );
    // Captures dump the process's memory with locum-cli, into log/dumps.
    std::fs::write(
        log_dir.join("settings.txt"),
        format!(
            "dumper {}\ndumps {}\n",
            stage.join("locum-cli.exe").display(),
            log_dir.join("dumps").display()
        ),
    )
    .expect("write settings.txt");
    let status = Command::new(stage.join("locum-smoke.exe"))
        .current_dir(&stage)
        .env("LOCUM_DIR", &log_dir)
        .status()
        .expect("run locum-smoke");
    println!("--- {}", log_dir.join("locum.log").display());
    print!(
        "{}",
        std::fs::read_to_string(log_dir.join("locum.log")).unwrap_or_else(|e| format!("(no log: {e})\n"))
    );
    // Run CLI evidence checks even when a GPU/database assertion failed, so every adversarial case reports.
    // A program that loads d3d11.dll by name only after locum started (as games that delay-load it do), in a log
    // folder of its own.
    let late_dir = log_dir.join("late");
    std::fs::create_dir_all(&late_dir).expect("create the late log folder");
    let late = Command::new(stage.join("locum-smoke-late.exe"))
        .current_dir(&stage)
        .env("LOCUM_DIR", &late_dir)
        .status()
        .expect("run locum-smoke-late");
    // A program whose overlay redirects the system d3d11.dll's D3D11CreateDevice export to code that calls locum's
    // (as Epic Online Services' overlay does), in a log folder of its own.
    let overlay_dir = log_dir.join("overlay");
    std::fs::create_dir_all(&overlay_dir).expect("create the overlay log folder");
    let overlay = Command::new(stage.join("locum-smoke-overlay.exe"))
        .current_dir(&stage)
        .env("LOCUM_DIR", &overlay_dir)
        .status()
        .expect("run locum-smoke-overlay");
    let overlay_logged = std::fs::read_to_string(overlay_dir.join("locum.log"))
        .is_ok_and(|l| l.contains("D3D11CreateDevice of ") && l.contains("called at its own address"));
    println!(
        "{} overlay: locum's log names the redirected export",
        if overlay_logged { "ok  " } else { "FAIL" }
    );
    let cli = stage.join("locum-cli.exe");
    let cli_ok = cli_checks(&cli, &log_dir);
    let (adversarial_ok, adversarial) = verified(
        &cli,
        &log_dir,
        "adversarial",
        false,
        debug,
        "adversarial rows6/7: all recorded checkpoints match replay",
    );
    // Where the capture was made and verified, as `info` and verify say, and the verification kept in it.
    let mut environment_ok = true;
    let on = |args: &[&str]| -> String {
        let out = Command::new(&cli)
            .arg(args[0])
            .arg(log_dir.join("captures").join("adversarial"))
            .args(&args[1..])
            .env("LOCUM_DIR", &log_dir)
            .output()
            .expect("run locum-cli");
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        println!("--- locum-cli {}\n{text}", args.join(" "));
        text
    };
    for (passed, what) in environment_checks(&on, &adversarial, "d3d11") {
        println!("{}  adversarial: {what}", if passed { "ok  " } else { "FAIL" });
        environment_ok &= passed;
    }
    // locum's stand-ins as a translation layer, conform held to the adversarial capture's three draws, all of one
    // pixel shader. Its reusable list draws red, then green; a rule draws white instead, a finding at each of the two.
    // The third draw's target is read after the clear that follows it, which no rule changes.
    let ps = on(&["sql", "SELECT hash FROM shaders WHERE stage = 'ps'"])
        .lines()
        .nth(1)
        .unwrap_or_default()
        .trim()
        .to_string();
    let rule = format!("solid ps={ps} 1 1 1 1\n");
    let mut conform_ok = true;
    let captures = log_dir.join("captures");
    for (passed, what) in conform_checks(
        &cli,
        &stage,
        &log_dir.join("conform"),
        &Conformance {
            capture: &captures.join("adversarial"),
            nothing: &captures.join("series-1"),
            dlls: &["d3d11.dll", "dxgi.dll", "locum.dll"],
            needed: 2,
            rule: Some((&rule, 2)),
            warp: false,
        },
    ) {
        println!("{} {what}", if passed { "ok  " } else { "FAIL" });
        conform_ok &= passed;
    }
    // The ordered capture's dispatch writes its records in whatever order the GPU runs its threads; WARP runs them
    // in another order than the GPU did. Both replays must still be bit for bit the game's.
    let mut ordered_ok = true;
    for warp in [false, true] {
        let device = if warp { "WARP" } else { "the GPU" };
        let (ok, text) = verified(
            &cli,
            &log_dir,
            "ordered",
            warp,
            debug,
            &format!(
                "ordered: replayed on {device}, the draw after an order-dependent dispatch is the game's"
            ),
        );
        // "order-dependent shaders' outputs: A identical to the game's, B the same records in another order, C
        // different": all three of the dispatch's UAVs compared.
        let settled = text.lines().any(|l| {
            let Some(rest) = l.strip_prefix("order-dependent shaders' outputs: ") else {
                return false;
            };
            let counts: Vec<u32> = rest
                .split(", ")
                .filter_map(|p| p.split(' ').next()?.parse().ok())
                .collect();
            counts.len() == 3 && counts.iter().sum::<u32>() == 3
        });
        // Replayed on WARP, the GPU's capture: both ends named, and that they differ.
        let ends = text.contains("captured on ")
            && text.contains("; replayed on Microsoft Basic Render Driver (1414:008c)")
            && text.contains("(another adapter or driver than the capture's)");
        if warp {
            println!(
                "{} ordered: replayed on WARP, verify says the capture was made on another adapter",
                if ends { "ok  " } else { "FAIL" }
            );
            ordered_ok &= ends;
        }
        println!(
            "{} ordered: on {device}, each of the dispatch's three outputs was compared with the game's",
            if settled { "ok  " } else { "FAIL" }
        );
        let plain = text.lines().any(|l| {
            l == "what draws and dispatches wrote through UAVs: 1 identical to the game's, 0 different"
        });
        println!(
            "{} outputs: on {device}, the order-independent dispatch's output is compared and identical",
            if plain { "ok  " } else { "FAIL" }
        );
        // The same capture with that output's record off by one word: verify fails, naming the dispatch.
        let mut command = Command::new(&cli);
        command
            .arg("verify")
            .arg(log_dir.join("captures/ordered-tampered"));
        if warp {
            command.arg("--warp");
        }
        let output = command
            .env("LOCUM_DIR", &log_dir)
            .output()
            .expect("verify the tampered capture");
        let tampered =
            String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
        let named = !output.status.success()
            && tampered
                .lines()
                .any(|l| l.starts_with("Dispatch 1 ") && l.contains("wrote other bytes than the game's"))
            && tampered.contains("1 of 1 outputs of draws and dispatches differ");
        println!("--- verify ordered-tampered (WARP={warp})\n{tampered}");
        println!(
            "{} outputs: on {device}, verify fails on a dispatch whose output is not the game's, and names it",
            if named { "ok  " } else { "FAIL" }
        );
        ordered_ok &= ok && settled && plain && named;
    }
    let mut rows_ok = verified(&cli, &log_dir, "row16", false, debug, "row16: bit-exact replay").0;
    for label in ["tiles-gpu", "tiles-warp"] {
        if !log_dir.join(format!("captures/{label}/capture.db")).exists() {
            continue;
        }
        for warp in [false, true] {
            let mut cmd = Command::new(&cli);
            cmd.arg("verify").arg(log_dir.join(format!("captures/{label}")));
            if warp {
                cmd.arg("--warp");
            }
            let output = cmd.output().expect("verify native tiled replay");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let ok = output.status.success() && text.contains("6 of 6 targets identical");
            println!("--- verify {label} WARP={warp}\n{text}");
            println!(
                "{} tiles: native mapping and shared-pool replay is bit-exact",
                if ok { "ok  " } else { "FAIL" }
            );
            rows_ok &= ok;
            let mut command = Command::new(&cli);
            command
                .arg("replay")
                .arg(log_dir.join(format!("captures/{label}")))
                .args(["last", "--mode", "slice", "--out"])
                .arg(log_dir.join(format!("{label}-slice-{warp}")));
            if warp {
                command.arg("--warp");
            }
            if debug {
                command.arg("--debug");
            }
            let output = command.output().expect("replay tiled alias slice");
            let text = String::from_utf8_lossy(&output.stdout).into_owned()
                + &String::from_utf8_lossy(&output.stderr);
            let ok = output.status.success()
                && text.contains("against the game's: identical")
                && !text.contains("warning:");
            println!("--- {label} slice WARP={warp}\n{text}");
            println!(
                "{} tiles: slice retains writes through pool aliases",
                if ok { "ok  " } else { "FAIL" }
            );
            rows_ok &= ok;
        }
    }

    for driver in ["gpu", "warp"] {
        for kind in ["", "bc1-"] {
            let packed_label = format!("tiles-packed-{kind}{driver}");
            if log_dir
                .join(format!("captures/{packed_label}/capture.db"))
                .exists()
            {
                for warp in [false, true] {
                    let mut cmd = Command::new(&cli);
                    cmd.arg("verify")
                        .arg(log_dir.join(format!("captures/{packed_label}")));
                    if warp {
                        cmd.arg("--warp");
                    }
                    if debug {
                        cmd.arg("--debug");
                    }
                    let output = cmd.output().expect("verify packed mip seeds");
                    let text = String::from_utf8_lossy(&output.stdout).into_owned()
                        + &String::from_utf8_lossy(&output.stderr);
                    let incompatible = !output.status.success()
                        && warp != (driver == "warp")
                        && text.contains("device tiling differs");
                    let ok = incompatible
                        || (output.status.success() && text.contains("7 of 7 targets identical"));
                    println!("--- {packed_label} WARP={warp}\n{text}");
                    println!(
                        "{} packed mips: {}",
                        if ok { "ok  " } else { "FAIL" },
                        if incompatible {
                            "incompatible device layout explicitly rejected"
                        } else {
                            "all mips and array slices verify"
                        }
                    );
                    rows_ok &= ok;
                }
            }
        }
        for (label, expected) in [
            (format!("tiles-{driver}-omit-alias"), "targets differ"),
            (format!("tiles-packed-{driver}-zero-seeds"), "targets differ"),
            (format!("tiles-packed-bc1-{driver}-zero-seeds"), "targets differ"),
            (
                format!("tiles-packed-bc1-alias-{driver}"),
                "packed mip pool tiles have an unverified alias",
            ),
            (
                format!("tiles-packed-alias-{driver}"),
                "packed mip pool tiles have an unverified alias",
            ),
        ] {
            if !log_dir.join(format!("captures/{label}/capture.db")).exists() {
                continue;
            }
            for warp in [false, true] {
                let mut cmd = Command::new(&cli);
                cmd.arg("verify").arg(log_dir.join(format!("captures/{label}")));
                if warp {
                    cmd.arg("--warp");
                }
                let output = cmd.output().expect("verify tiled negative control");
                let text = String::from_utf8_lossy(&output.stdout).into_owned()
                    + &String::from_utf8_lossy(&output.stderr);
                let ok = !output.status.success()
                    && (text.contains(expected)
                        || (label.starts_with("tiles-packed-")
                            && warp != (driver == "warp")
                            && text.contains("device tiling differs")));
                println!("--- {label} WARP={warp}\n{text}");
                println!(
                    "{} tiles: rejects missing alias writes or packed seeds",
                    if ok { "ok  " } else { "FAIL" }
                );
                rows_ok &= ok;
            }
        }
    }

    for label in [
        "list-prefix",
        "list-bindings",
        "list-reset",
        "list-unexecuted",
        "list-armed",
        "list-generation",
        "list-inherited",
        "list-reuse-update",
        "list-reuse-map",
        "list-owned-update",
        "list-owned-map",
        "list-sparse-maps",
        "list-nested",
    ] {
        for warp in [false, true] {
            if label == "list-nested" {
                let mut cmd = Command::new(&cli);
                cmd.arg("verify").arg(log_dir.join(format!("captures/{label}")));
                if warp {
                    cmd.arg("--warp");
                }
                let output = cmd.output().expect("verify nested fallback");
                let text = String::from_utf8_lossy(&output.stdout).into_owned()
                    + &String::from_utf8_lossy(&output.stderr);
                let ok =
                    !output.status.success() && text.contains("deferred_recorder") && text.contains("nested");
                println!(
                    "{} list-nested: explicit fallback gap WARP={warp}\n{text}",
                    if ok { "ok  " } else { "FAIL" }
                );
                rows_ok &= ok;
            } else {
                rows_ok &= verified(
                    &cli,
                    &log_dir,
                    label,
                    warp,
                    debug,
                    &format!("{label}: complete retained list verifies"),
                )
                .0;
            }
        }
    }

    for label in ["row16-warp-raw", "row16-warp-adjusted", "row16-warp-modern"] {
        for warp in [false, true] {
            rows_ok &= verified(
                &cli,
                &log_dir,
                label,
                warp,
                debug,
                &format!("{label}: bit-exact replay"),
            )
            .0;
        }
    }
    for label in [
        "device-write-gpu-2d",
        "device-write-gpu-map-first",
        "device-write-warp-map-first",
        "device-write-gpu-3d",
        "device-write-warp-2d",
        "device-write-warp-3d",
    ] {
        if log_dir.join(format!("captures/{label}/capture.db")).exists() {
            for warp in [false, true] {
                rows_ok &= verified(
                    &cli,
                    &log_dir,
                    label,
                    warp,
                    debug,
                    &format!("{label}: bit-exact replay"),
                )
                .0;
            }
        }
    }
    for label in ["device-write-gpu-prearmed", "device-write-warp-prearmed"] {
        if !log_dir.join(format!("captures/{label}/capture.db")).exists() {
            continue;
        }
        for warp in [false, true] {
            let mut cmd = Command::new(&cli);
            cmd.arg("verify").arg(log_dir.join(format!("captures/{label}")));
            if warp {
                cmd.arg("--warp");
            }
            let output = cmd.output().expect("verify prearmed default map");
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let ok = !output.status.success() && text.contains("default_map");
            println!("--- verify {label} WARP={warp}\n{text}");
            println!(
                "{} {label}: rejects missing pre-map contents",
                if ok { "ok  " } else { "FAIL" }
            );
            rows_ok &= ok;
        }
    }
    if log_dir.join("captures/row17/capture.db").exists() {
        for warp in [false, true] {
            rows_ok &= verified(&cli, &log_dir, "row17", warp, debug, "row17: bit-exact replay").0;
        }
    }
    for warp in [false, true] {
        rows_ok &= verified(&cli, &log_dir, "row18", warp, debug, "row18: bit-exact replay").0;
    }
    for warp in [false, true] {
        rows_ok &= verified(&cli, &log_dir, "row21", warp, debug, "row21: bit-exact replay").0;
        let mut command = Command::new(&cli);
        command
            .arg("replay")
            .arg(log_dir.join("captures/row21"))
            .args(["last", "--mode", "slice", "--out"])
            .arg(log_dir.join(if warp {
                "row21-slice-warp"
            } else {
                "row21-slice-gpu"
            }));
        if warp {
            command.arg("--warp");
        }
        if debug {
            command.arg("--debug");
        }
        let output = command
            .env("LOCUM_DIR", &log_dir)
            .output()
            .expect("replay counter slice");
        let text =
            String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
        let ok = output.status.success()
            && text.contains("against the game's: identical")
            && !text.contains("warning:");
        println!("--- row21 slice replay (WARP={warp})\n{text}");
        println!(
            "{} row21: slice retains counter resets",
            if ok { "ok  " } else { "FAIL" }
        );
        rows_ok &= ok;
    }
    rows_ok &= layer_checks(&cli, &log_dir, debug);
    for warp in [false, true] {
        for label in [
            "live-skip",
            "live-solid",
            "live-constant",
            "live-deferred",
            "live-unmatched",
            "live-unexecuted",
        ] {
            let mut cmd = Command::new(&cli);
            cmd.arg("verify")
                .arg(log_dir.join("captures").join(label))
                .env("LOCUM_DIR", &log_dir);
            if warp {
                cmd.arg("--warp");
            }
            if debug {
                cmd.arg("--debug");
            }
            let output = cmd.output().expect("verify live-rule capture");
            let text = String::from_utf8_lossy(&output.stdout).into_owned()
                + &String::from_utf8_lossy(&output.stderr);
            let passed = output.status.success();
            println!("--- {label} (WARP={warp})\n{text}");
            let ok = if matches!(label, "live-unmatched" | "live-unexecuted") {
                passed
            } else if label == "live-deferred" {
                !passed && text.contains("capture gap: deferred_recorder") && text.contains("live override")
            } else {
                !passed
                    && text.contains("capture gap: overrides")
                    && text.contains("event 0")
                    && text.contains("1 of 1 targets identical")
            };
            println!(
                "{} {label}: live-rule completeness gate",
                if ok { "ok  " } else { "FAIL" }
            );
            rows_ok &= ok;
        }
    }
    if !status.success()
        || !late.success()
        || !overlay.success()
        || !overlay_logged
        || !cli_ok
        || !adversarial_ok
        || !environment_ok
        || !conform_ok
        || !ordered_ok
        || !rows_ok
    {
        std::process::exit(1);
    }
}

fn layer_checks(cli: &Path, log_dir: &Path, debug: bool) -> bool {
    let mut ok = true;
    for warp in [false, true] {
        for (label, expected) in [
            ("layers-array", "4 of 4 targets identical"),
            ("layers-volume", "2 of 2 targets identical"),
            ("layers-legacy", "4 of 4 targets identical"),
            (
                "layers-deferred",
                "clear outputs: 5 identical to the game's, 0 different",
            ),
            ("layers-budget-array", "1 of 1 targets identical"),
            ("layers-budget-volume", "1 of 1 targets identical"),
            (
                "layers-budget-shared",
                "clear outputs: 1 identical to the game's, 0 different",
            ),
            (
                "layers-clear",
                "clear outputs: 6 identical to the game's, 0 different",
            ),
        ] {
            for bad in [false, true] {
                if bad && !matches!(label, "layers-array" | "layers-volume" | "layers-clear") {
                    continue;
                }
                let name = if bad {
                    format!("{label}-tampered")
                } else {
                    label.into()
                };
                let mut cmd = Command::new(cli);
                cmd.arg("verify")
                    .arg(log_dir.join("captures").join(&name))
                    .env("LOCUM_DIR", log_dir);
                if warp {
                    cmd.arg("--warp");
                }
                if debug {
                    cmd.arg("--debug");
                }
                let output = cmd.output().expect("layers verify");
                let text = String::from_utf8_lossy(&output.stdout).into_owned()
                    + &String::from_utf8_lossy(&output.stderr);
                let pass = if bad {
                    !output.status.success()
                        && text.contains("(event ")
                        && if label == "layers-clear" {
                            text.contains("ClearRenderTargetView (event 0)")
                                && text.contains("1 clear outputs differ")
                        } else {
                            text.contains("1 of ")
                                && text.contains("targets differ")
                                && text.contains(if label == "layers-volume" {
                                    "rt0[z2]"
                                } else {
                                    "rt0[slice2]"
                                })
                        }
                } else {
                    output.status.success()
                        && text.contains(expected)
                        && (!label.contains("budget") || text.contains("max_mb"))
                };
                println!(
                    "--- verify {name} (WARP={warp})\n{text}\n{} layers: {name}",
                    if pass { "ok  " } else { "FAIL" }
                );
                ok &= pass;
            }
        }
        for (label, names) in [
            (
                "layers-array",
                vec!["rt0[slice1]", "rt0[slice2]", "depth[slice1]", "depth[slice2]"],
            ),
            ("layers-volume", vec!["rt0[z1]", "rt0[z2]"]),
        ] {
            let dest = log_dir.join(format!("{label}-preview-{warp}"));
            let mut cmd = Command::new(cli);
            cmd.arg("replay")
                .arg(log_dir.join("captures").join(label))
                .arg("0")
                .args(["--mode", "frame"])
                .arg("-o")
                .arg(&dest)
                .env("LOCUM_DIR", log_dir);
            if warp {
                cmd.arg("--warp");
            }
            if debug {
                cmd.arg("--debug");
            }
            let output = cmd.output().expect("layers preview");
            let text = String::from_utf8_lossy(&output.stdout).into_owned()
                + &String::from_utf8_lossy(&output.stderr);
            let pass = output.status.success()
                && text.matches("against the game's: identical").count() == names.len()
                && names
                    .iter()
                    .all(|name| dest.join(format!("{name}.png")).is_file());
            println!(
                "--- replay {label} (WARP={warp})\n{text}\n{} layers: every layer previewed",
                if pass { "ok  " } else { "FAIL" }
            );
            ok &= pass;
        }
    }
    ok
}

/// Runs `locum-cli verify` on the smoke test's capture `label` (on WARP if `warp`): whether it passed, with every
/// target compared identical and at least one compared, and what it said.
fn verified(cli: &Path, log_dir: &Path, label: &str, warp: bool, debug: bool, what: &str) -> (bool, String) {
    let mut command = Command::new(cli);
    command.arg("verify").arg(log_dir.join("captures").join(label));
    if debug {
        command.arg("--debug");
    }
    if warp {
        command.arg("--warp");
    }
    let output = command
        .env("LOCUM_DIR", log_dir)
        .output()
        .unwrap_or_else(|e| panic!("verify the {label} capture: {e}"));
    let text =
        String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    println!(
        "--- verify {label}{} (exit {:?})\n{text}",
        if warp { " --warp" } else { "" },
        output.status.code()
    );
    let all_same = text.lines().any(|l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        w.len() > 4
            && w[1] == "of"
            && w[3] == "targets"
            && w[4] == "identical;"
            && w[0] == w[2]
            && w[0] != "0"
    });
    let ok = output.status.success() && all_same;
    println!("{} {what}", if ok { "ok  " } else { "FAIL" });
    (ok, text)
}

/// What `info` and `verify` must say of where a capture of `api` (`d3d11`, `d3d12`) was made and where `verify` (what
/// its last run said) replayed it, the same machine: every part of both described, no probe failed, and the
/// verification a row of the capture's `verifications`. `run` runs locum-cli on the capture (its command, then the
/// capture, then the rest) and gives what it said. Each check, with what it shows.
fn environment_checks(run: &dyn Fn(&[&str]) -> String, verify: &str, api: &str) -> Vec<(bool, &'static str)> {
    let info = run(&["info"]);
    let line = |prefix: &str| {
        info.lines()
            .find(|l| l.trim_start().starts_with(prefix))
            .unwrap_or_default()
            .to_string()
    };
    let adapter = line("adapter: ");
    // The lines of the machine: from the adapter's to the verifications'.
    let environment: String = info
        .lines()
        .skip_while(|l| !l.starts_with("  adapter: "))
        .take_while(|l| !l.starts_with("  verified"))
        .collect();
    let sql = |query: &str| {
        run(&["sql", query])
            .lines()
            .nth(1)
            .unwrap_or_default()
            .trim()
            .to_string()
    };
    let last = sql(
        "SELECT ok || ' ' || json_extract(environment, '$.adapter.description') FROM verifications ORDER BY rowid \
         DESC LIMIT 1",
    );
    let described = adapter
        .strip_prefix("  adapter: ")
        .and_then(|a| a.split(" (").next())
        .unwrap_or_default()
        .to_string();
    vec![
        (
            adapter.contains(", driver ")
                && adapter.contains(", LUID ")
                && line("user-mode driver: ").contains('.')
                && line("Windows ").starts_with("  Windows 10.")
                && line("exe: ").contains("sha256 ")
                && (api == "d3d12") == line("Direct3D 12: ").contains("D3D12Core 10."),
            "info describes the adapter, its driver and user-mode driver, Windows, the exe and (D3D12) the runtime",
        ),
        (
            !environment.contains("not recorded")
                && sql("SELECT count(*) FROM gaps WHERE what = 'environment'") == "0",
            "no probe of the machine failed: no part not recorded, no environment gap",
        ),
        (
            !described.is_empty()
                && verify.contains(&format!("captured and replayed on {described} ("))
                && verify.contains(
                    "verified: every target and output compared is the game's (captured and replayed on",
                ),
            "verify names the adapter both ends ran on, before it compares and with its result",
        ),
        (
            last == format!("1 {described}")
                && info
                    .lines()
                    .any(|l| l.contains(" UTC, locum-cli ") && l.contains(": verified: every target")),
            "the verification is kept in the capture with the adapter it replayed on, and info lists it",
        ),
        (
            verify.contains("replayed on ") && !verify.contains(", through "),
            "verify replayed on the system's Direct3D, not on the DLLs beside locum-cli",
        ),
    ]
}

/// A capture to hold `locum-cli conform` to, replayed through locum's own stand-ins as a translation layer.
struct Conformance<'a> {
    /// A capture whose verify compares targets, and one whose compares none.
    capture: &'a Path,
    nothing: &'a Path,
    /// The stand-ins: the API's DLLs (the first `needed`), then what they load.
    dlls: &'a [&'a str],
    needed: usize,
    /// An overrides.txt line changing every draw `capture` compares, and how many draws that is.
    rule: Option<(&'a str, usize)>,
    /// Both replays on WARP.
    warp: bool,
}

/// What `locum-cli conform` must say of `c`'s captures through its stand-ins, copied from `stage` into a folder of
/// their own under `out`. With no rule the layer draws the game's frame and is named by its DLLs and folder, and the
/// system's replay by none; with the rule each draw it changes is a finding and conform fails; a layer without one of
/// the API's DLLs, or a capture with nothing to compare, is refused. Each check, with what it shows.
fn conform_checks(cli: &Path, stage: &Path, out: &Path, c: &Conformance) -> Vec<(bool, String)> {
    let Conformance {
        capture,
        nothing,
        dlls,
        needed,
        rule,
        warp,
    } = *c;
    let _ = std::fs::remove_dir_all(out);
    let layer = out.join("layer");
    let lacking = out.join("lacking");
    std::fs::create_dir_all(&layer).expect("create the layer folder");
    std::fs::create_dir_all(&lacking).expect("create the lacking layer folder");
    for (i, dll) in dlls.iter().enumerate() {
        std::fs::copy(stage.join(dll), layer.join(dll)).unwrap_or_else(|e| panic!("copy {dll}: {e}"));
        // The lacking layer has all but the first.
        if i > 0 {
            std::fs::copy(stage.join(dll), lacking.join(dll)).unwrap_or_else(|e| panic!("copy {dll}: {e}"));
        }
    }
    // Each run in a folder of its own, with locum's settings, log and overrides.txt (`rules`) in it.
    let run = |name: &str, capture: &Path, layer: &Path, rules: &str| -> (bool, String) {
        let dir = out.join(name);
        let locum_dir = dir.join("locum");
        std::fs::create_dir_all(&locum_dir).expect("create locum's folder");
        if !rules.is_empty() {
            std::fs::write(locum_dir.join("overrides.txt"), rules).expect("write overrides.txt");
        }
        let output = Command::new(cli)
            .arg("conform")
            .arg(capture)
            .arg("--layer")
            .arg(layer)
            .arg("-o")
            .arg(&dir)
            .args(warp.then_some("--warp"))
            .env("LOCUM_DIR", &locum_dir)
            .output()
            .expect("run locum-cli conform");
        let text =
            String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
        println!(
            "--- locum-cli conform {} ({name}, exit {:?})\n{text}",
            capture.display(),
            output.status.code()
        );
        (output.status.success(), text)
    };
    let mut checks = Vec::new();
    let (ok, text) = run("plain", capture, &layer, "");
    // "...; N findings about the layer (system: <the system's end>; layer: <the layer's end>)"
    let summary = text
        .lines()
        .find(|l| l.contains(" comparisons ("))
        .unwrap_or_default();
    let (system, theirs) = summary
        .split_once("(system: ")
        .and_then(|(_, ends)| ends.split_once("; layer: "))
        .unwrap_or_default();
    // conform names the layer's folder as Windows makes its path whole.
    let folder = std::path::absolute(&layer).expect("the layer's path");
    let named =
        dlls[..needed].iter().all(|d| theirs.contains(d)) && theirs.contains(&*folder.display().to_string());
    // "N comparisons (...): N the game's in both replays, ..."
    let compared = summary.split(' ').next().unwrap_or_default();
    checks.push((
        ok && summary.contains("; 0 findings about the layer")
            && compared != "0"
            && summary.contains(&format!("): {compared} the game's in both replays, "))
            && !system.is_empty()
            && !system.contains(", through ")
            && theirs.contains(", through ")
            && named,
        format!(
            "conform through locum's {}: the game's frame, the layer named by its DLLs and folder, the system's \
             replay on the system's Direct3D",
            dlls[..needed].join(" and ")
        ),
    ));
    if let Some((rule, draws)) = rule {
        let (ok, text) = run("rule", capture, &layer, rule);
        let differs = text
            .lines()
            .filter(|l| l.starts_with("LAYER DIFFERS draw "))
            .count();
        checks.push((
            !ok && differs == draws && text.contains(&format!("{draws} findings about the layer")),
            format!(
                "conform through locum's stand-ins with `{}`: each of the {draws} draws it changes a finding",
                rule.trim()
            ),
        ));
    }
    let (ok, text) = run("lacking", capture, &lacking, "");
    checks.push((
        !ok && text.contains(&format!("has no {}", dlls[0])),
        format!("conform refuses a layer without {}", dlls[0]),
    ));
    let (ok, text) = run("nothing", nothing, &layer, "");
    checks.push((
        !ok && text.contains("the system's replay compared nothing with the game's"),
        "conform refuses a capture whose replay compares nothing".into(),
    ));
    checks
}

/// Runs locum-cli on the smoke test's capture and checks what it says about it.
fn cli_checks(cli: &Path, log_dir: &Path) -> bool {
    let capture = log_dir.join("captures").join("frame-32");
    let run = |args: &[&str]| -> String {
        let out = Command::new(cli)
            .arg(args[0])
            .arg(&capture)
            .args(&args[1..])
            .env("LOCUM_DIR", log_dir)
            .output()
            .expect("run locum-cli");
        let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
        println!("--- locum-cli {}\n{text}", args.join(" "));
        text
    };
    let checks = [
        (
            run(&["info"]).contains("3 draws and dispatches in 3 passes"),
            "locum-cli info: three draws in three passes",
        ),
        (
            {
                let bytes = |draw: &str| {
                    Command::new(cli)
                        .arg("target")
                        .arg(&capture)
                        .args([draw, "rt0", "-o", "-"])
                        .env("LOCUM_DIR", log_dir)
                        .output()
                        .expect("run locum-cli")
                        .stdout
                };
                let presented = bytes("presented");
                run(&["info"]).contains("presented: #")
                    && run(&["info"]).contains("read back (`target presented`)")
                    && !presented.is_empty()
                    && presented == bytes("1")
            },
            "locum-cli info names the back buffer the frame presented, and `target presented` gives its \
             readback after draw 1, where the frame draws into it",
        ),
        (
            {
                let t = run(&["draw", "0"]);
                t.contains("Frame.value")
                    && t.contains("1032")
                    && t.contains("(upload)")
                    && t.contains("(first-use)")
                    && t.contains("topology: triangle list (4)")
                    && t.contains("(1 bound register the shaders do not use; --all shows it)")
                    && !t.contains("ps srv0")
                    && run(&["draw", "0", "--all"]).contains("ps srv0 (the shader does not use it)")
            },
            "locum-cli draw 0: ps b3 named Frame.value holding 1032; upload and first-use contents; topology named; \
             the texture bound at t0 that the shader does not read is left out and counted, and --all names it",
        ),
        (
            {
                let t = run(&["resource", "9"]);
                t.starts_with("#9 ") && t.contains("events that wrote it") && !t.contains("panicked")
            },
            "locum-cli resource 9: a Direct3D 11 capture's resource, with the events that wrote it (no Direct3D 12 \
             tile tables asked of it)",
        ),
        (
            {
                let other = log_dir.join("captures").join("device-write-gpu-2d");
                let out = Command::new(cli)
                    .arg("draw")
                    .arg(&other)
                    .arg("0")
                    .output()
                    .expect("run locum-cli draw");
                let t = String::from_utf8_lossy(&out.stdout).into_owned();
                println!("--- locum-cli draw device-write-gpu-2d 0\n{t}");
                t.contains("ps srv0 t view")
            },
            "locum-cli draw: a texture slot the shader reads is named from its reflection (t)",
        ),
        (
            {
                // #9 is updated at events 5, 6 and 7 with two contents in turn: each event sees the one before it.
                let dir = log_dir.join("export-at");
                let _ = std::fs::create_dir_all(&dir);
                let file = |name: &str| dir.join(name).display().to_string();
                let saw = |event: &str| -> Vec<u8> {
                    run(&[
                        "export",
                        "9",
                        "--at",
                        event,
                        "-o",
                        &file(&format!("at{event}.bin")),
                    ]);
                    std::fs::read(file(&format!("at{event}.bin"))).unwrap_or_default()
                };
                let wrote = |event: &str| -> Vec<u8> {
                    let data = run(&["sql", &format!("SELECT data FROM events WHERE seq = {event}")]);
                    let key = data.lines().nth(1).unwrap_or("").trim().to_string();
                    run(&["blob", &key, "-o", &file(&format!("data{event}.bin"))]);
                    std::fs::read(file(&format!("data{event}.bin"))).unwrap_or_default()
                };
                let (six, seven) = (wrote("6"), wrote("7"));
                six.len() == 80 && six != seven && saw("7") == six && saw("8") == seven
            },
            "locum-cli export --at: a buffer updated three times holds, at each event, what the update before it wrote",
        ),
        (
            {
                let word = r#""name":"Frame.value.x","value":1032.0"#;
                let one = run(&["draw", "0", "--json"]);
                let every = run(&["draw", "all", "--json"]);
                let cb = run(&["cb", "0", "ps", "3", "--json"]);
                one.contains(word)
                    && one.contains(r#""named_args":"count=3 start=0""#)
                    && every.lines().filter(|l| l.starts_with(r#"{"draw":"#)).count() == 3
                    && cb.contains(r#""name":"Frame""#)
                    && cb.contains(word)
            },
            "locum-cli draw/cb --json: draw 0, every draw one per line, and ps b3, with Frame.value.x = 1032",
        ),
        (
            run(&["find", "32"]).contains("Frame.value.x"),
            "locum-cli find 32: the command list's constant, by name",
        ),
        (
            run(&["events", "--kind", "map"]).contains("Unmap"),
            "locum-cli events --kind map: the map in the command list",
        ),
        (
            {
                // The dump is written after the capture, by its own process: wait for it.
                let dump = log_dir.join("dumps").join("frame-32.dmp");
                for _ in 0..600 {
                    if dump.is_file() {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                let t = run(&["memfind", "--cb", "0", "ps", "3"]);
                t.contains("Frame.value: ") && !t.contains("Frame.value: 0 found")
            },
            "locum-cli memfind --cb 0 ps 3: the constant draw 0 saw, found in the process's memory",
        ),
        (
            run(&["verify"]).contains("3 of 3 targets identical"),
            "locum-cli verify: the replayed frame's three targets are the game's, bit for bit",
        ),
        (
            {
                let out = log_dir.join("replay").display().to_string();
                let t = run(&[
                    "replay",
                    "0",
                    "--rule",
                    "constant ps b3 c0 500 500 500 500",
                    "-o",
                    &out,
                ]);
                t.contains("against the game's: identical") && t.contains("4096 of 4096 texels differ")
            },
            "locum-cli replay 0 with a constant rule: as the game drew it before, every texel changed after",
        ),
        (
            {
                let t = run(&["diff", &capture.display().to_string()]);
                t.contains("3 matched, 0 only in the first, 0 only in the second")
                    && t.contains("0 matched draws differ")
            },
            "locum-cli diff: a capture against itself matches every draw and finds nothing different",
        ),
        (
            {
                let index = |folder: &str| {
                    let out = log_dir.join("files").join(format!("{folder}.db"));
                    let _ = std::fs::remove_file(&out);
                    let made = Command::new(cli)
                        .args(["index", "-o"])
                        .arg(&out)
                        .arg(log_dir.join("files").join(folder))
                        .output()
                        .expect("run locum-cli index");
                    println!(
                        "--- locum-cli index {folder}
{}",
                        String::from_utf8_lossy(&made.stdout)
                    );
                    out.display().to_string()
                };
                let (whole, part) = (index("whole"), index("from-mip-1"));
                let t = run(&["join", "--index", &whole]);
                let u = run(&["join", "--index", &part]);
                t.contains("texture.dds (upload)")
                    && t.contains("textures the frame used: 1 of 1 named")
                    && u.contains("texture.dds (2 of 3 subresources: mips 1..=2 are the file's mips 0..=1)")
            },
            "locum-cli join: the texture named by its file whole, and by a file holding its mips from the second on",
        ),
    ];
    let mut ok = true;
    for (passed, what) in checks {
        println!("{}  {what}", if passed { "ok  " } else { "FAIL" });
        ok &= passed;
    }
    ok
}

fn generate_all(root: &Path) {
    let root = root.to_path_buf();
    let windows = windows_source(&root);
    let mut vtbls = HashMap::new();
    let mut features = BTreeSet::new();
    for module in MODULES {
        let file = windows.join("src/Windows").join(module.join("/")).join("mod.rs");
        read_vtbls(&file, module, &mut vtbls, &mut features);
    }
    let out = root.join("locum/src/vtables");
    std::fs::create_dir_all(&out).expect("create locum/src/vtables");
    let mut mods = String::from("//! Generated by `cargo xtask gen`; do not edit.\n\n");
    for class in CLASSES {
        let code = generate(class, &vtbls);
        let path = out.join(format!("{}.rs", class.module));
        std::fs::write(&path, code).expect("write generated module");
        rustfmt(&path);
        writeln!(mods, "pub mod {};", class.module).unwrap();
    }
    let recorded = out.join("recorded_context.rs");
    std::fs::write(&recorded, d3d11::commands(&vtbls)).unwrap();
    rustfmt(&recorded);
    mods.push_str("pub mod recorded_context;\n");
    let d12 = out.join("d12_registry.rs");
    std::fs::write(&d12, d3d12::registry(CLASSES, &vtbls)).unwrap();
    rustfmt(&d12);
    mods.push_str("pub mod d12_registry;\n");
    let mod_rs = out.join("mod.rs");
    std::fs::write(&mod_rs, mods).expect("write vtables/mod.rs");
    rustfmt(&mod_rs);
    let portable = root.join("locum/src/d3d12/portable.rs");
    std::fs::write(&portable, d3d12_capture::portable(&windows)).unwrap();
    rustfmt(&portable);
    println!("windows features the generated vtables need:");
    for feature in features {
        println!("  {feature}");
    }
}

/// The source folder of the `windows` crate this workspace resolves, from `cargo metadata`.
fn windows_source(root: &Path) -> PathBuf {
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("run cargo metadata");
    assert!(output.status.success(), "cargo metadata failed");
    let text = String::from_utf8(output.stdout).expect("cargo metadata output is UTF-8");
    // The package entry reads `"name":"windows","version":"…",…,"manifest_path":"…"`; find it without a JSON parser.
    let at = text
        .find("\"name\":\"windows\",\"version\":")
        .expect("the windows package is in the dependency graph");
    let rest = &text[at..];
    let key = "\"manifest_path\":\"";
    let start = rest.find(key).expect("manifest_path") + key.len();
    let end = start + rest[start..].find('"').expect("end of manifest_path");
    let manifest = rest[start..end].replace("\\\\", "\\");
    Path::new(&manifest).parent().expect("crate folder").to_path_buf()
}

fn read_vtbls(
    file: &Path,
    module: &[&str],
    vtbls: &mut HashMap<String, Vtbl>,
    features: &mut BTreeSet<String>,
) {
    let text = std::fs::read_to_string(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
    let parsed = syn::parse_file(&text).unwrap_or_else(|e| panic!("parse {}: {e}", file.display()));
    for item in parsed.items {
        let Item::Struct(s) = item else { continue };
        let name = s.ident.to_string();
        if !name.ends_with("_Vtbl") {
            continue;
        }
        let Fields::Named(fields) = s.fields else { continue };
        let mut base = None;
        let mut entries = Vec::new();
        for field in fields.named {
            let field_name = field.ident.as_ref().expect("named field").to_string();
            match cfg_of(&field.attrs) {
                Cfg::Not => continue,
                Cfg::Feature(f) => {
                    features.insert(f);
                }
                Cfg::None => {}
            }
            if field_name == "base__" {
                base = Some(last_ident(&field.ty));
                continue;
            }
            let Type::BareFn(f) = &field.ty else {
                panic!("{name}.{field_name} is not a function pointer")
            };
            let mut inputs = f.inputs.iter();
            inputs.next().expect("this");
            let params = inputs.map(|a| absolute(&a.ty, module)).collect();
            let ret = match &f.output {
                syn::ReturnType::Default => None,
                syn::ReturnType::Type(_, t) => Some(absolute(t, module)),
            };
            entries.push(Entry {
                name: field_name,
                params,
                ret,
            });
        }
        let base = base.filter(|b| b != "IUnknown_Vtbl");
        vtbls.insert(name, Vtbl { base, entries });
    }
}

enum Cfg {
    None,
    Feature(String),
    Not,
}

/// A field's `#[cfg(feature = "…")]` (kept, the feature is recorded) or `#[cfg(not(…))]` (the placeholder, dropped).
fn cfg_of(attrs: &[syn::Attribute]) -> Cfg {
    for attr in attrs {
        if !attr.path().is_ident("cfg") {
            continue;
        }
        let tokens = attr.meta.to_token_stream().to_string();
        if tokens.contains("not (") || tokens.contains("not(") {
            return Cfg::Not;
        }
        let start = tokens.find('"').expect("feature name") + 1;
        let end = start + tokens[start..].find('"').expect("end of feature name");
        return Cfg::Feature(tokens[start..end].to_string());
    }
    Cfg::None
}

fn last_ident(ty: &Type) -> String {
    let Type::Path(p) = ty else {
        panic!("base__ is not a path")
    };
    p.path.segments.last().expect("segment").ident.to_string()
}

/// A type from the source module, written with absolute paths: `windows_core::X` becomes `windows::core::X`,
/// `super::super::Foundation::X` and bare names are resolved against the module, primitives and `core::` stay.
fn absolute(ty: &Type, module: &[&str]) -> String {
    let mut ty = ty.clone();
    rewrite(&mut ty, module);
    ty.to_token_stream().to_string()
}

fn rewrite(ty: &mut Type, module: &[&str]) {
    match ty {
        Type::Ptr(p) => rewrite(&mut p.elem, module),
        Type::Reference(r) => rewrite(&mut r.elem, module),
        Type::Array(a) => rewrite(&mut a.elem, module),
        Type::Slice(s) => rewrite(&mut s.elem, module),
        Type::Paren(p) => rewrite(&mut p.elem, module),
        Type::Tuple(t) => t.elems.iter_mut().for_each(|e| rewrite(e, module)),
        Type::Path(p) => {
            for seg in p.path.segments.iter_mut() {
                if let syn::PathArguments::AngleBracketed(args) = &mut seg.arguments {
                    for arg in args.args.iter_mut() {
                        if let syn::GenericArgument::Type(t) = arg {
                            rewrite(t, module);
                        }
                    }
                }
            }
            let segs: Vec<String> = p.path.segments.iter().map(|s| s.ident.to_string()).collect();
            const PRIMITIVES: &[&str] = &[
                "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
                "f32", "f64", "bool",
            ];
            if segs[0] == "core" || (segs.len() == 1 && PRIMITIVES.contains(&segs[0].as_str())) {
                return;
            }
            let full: Vec<String> = if segs[0] == "windows_core" {
                // windows_core::X -> windows::core::X
                ["windows", "core"]
                    .iter()
                    .map(|s| s.to_string())
                    .chain(segs[1..].iter().cloned())
                    .collect()
            } else {
                // super::super::Foundation::X, Common::X or X: relative to the source module
                let ups = segs.iter().take_while(|s| *s == "super").count();
                std::iter::once("windows".to_string())
                    .chain(module[..module.len() - ups].iter().map(|s| s.to_string()))
                    .chain(segs[ups..].iter().cloned())
                    .collect()
            };
            let last_args = p.path.segments.last().expect("segment").arguments.clone();
            let mut path: syn::Path = syn::parse_str(&full.join("::")).expect("path");
            path.segments.last_mut().expect("segment").arguments = last_args;
            p.path = path;
        }
        _ => {}
    }
}

/// The interface's vtable chain, most basic first (the struct after IUnknown), with each struct's name.
fn chain<'a>(top: &str, vtbls: &'a HashMap<String, Vtbl>) -> Vec<(String, &'a Vtbl)> {
    let mut out = Vec::new();
    let mut name = format!("{top}_Vtbl");
    loop {
        let v = vtbls
            .get(&name)
            .unwrap_or_else(|| panic!("no vtable struct {name}"));
        out.push((name.clone(), v));
        match &v.base {
            Some(b) => name = b.clone(),
            None => break,
        }
    }
    out.reverse();
    out
}

fn generate(class: &Class, vtbls: &HashMap<String, Vtbl>) -> String {
    let levels = chain(class.interface, vtbls);
    let top = format!("windows::{}::{}_Vtbl", class.source.join("::"), class.interface);
    let module_of = |vtbl: &str| -> String {
        // Every struct of the chain lives in one of MODULES; find which by looking the name up in each source.
        for m in MODULES {
            let candidate = format!("windows::{}::{vtbl}", m.join("::"));
            if vtbl.starts_with("ID3D12") && m.ends_with(&["Direct3D12"]) {
                return candidate;
            }
            if vtbl.starts_with("ID3D11") && m.ends_with(&["Direct3D11"]) {
                return candidate;
            }
            if vtbl.starts_with("IDXGI") && m.ends_with(&["Dxgi"]) {
                return candidate;
            }
        }
        panic!("no module for {vtbl}");
    };

    let mut seen = BTreeSet::new();
    let mut out = String::new();
    writeln!(
        out,
        "//! Generated by `cargo xtask gen` from the `windows` crate's `{}_Vtbl`; do not edit.\n",
        class.interface
    )
    .unwrap();
    out.push_str(
        "#![allow(non_snake_case, clippy::too_many_arguments, clippy::missing_safety_doc, dead_code)]\n\n",
    );
    out.push_str("use core::ffi::c_void;\n\n");
    writeln!(
        out,
        "/// Every method of `{iface}` in vtable order. The defaults forward to the wrapped object [`{t}::inner`].\n\
         ///\n/// # Safety\n/// The implementing type must be `#[repr(C)]` with a pointer to [`vtable`]'s table as its first field,\n\
         /// and `inner` must return a live `{iface}`.",
        iface = class.interface,
        t = class.trait_name
    )
    .unwrap();
    writeln!(out, "pub unsafe trait {}: Sized {{", class.trait_name).unwrap();
    writeln!(out, "    /// The wrapped object, an `{}`.", class.interface).unwrap();
    out.push_str("    fn inner(&self) -> *mut c_void;\n");
    if class.trait_name == "ContextMethods" {
        out.push_str("    /// Called before commands that perform work, including calls with a manual override.\n    fn before_command(&self) {}\n    fn recorder(&self) -> Option<&crate::d3d11::recorder::Recorder> { None }\n");
    }
    out.push_str("    unsafe fn QueryInterface(&self, iid: *const windows::core::GUID, out: *mut *mut c_void) -> windows::core::HRESULT;\n");
    out.push_str("    unsafe fn AddRef(&self) -> u32;\n");
    out.push_str("    unsafe fn Release(&self) -> u32;\n");
    let mut thunks = String::new();
    let mut real = String::from(
        "/// Each method called on the real (wrapped) object: the trait's defaults, and overrides that still want the\n\
         /// real behaviour.\npub mod real {\n    use core::ffi::c_void;\n\n",
    );
    for (depth, (_, v)) in levels.iter().enumerate() {
        let path = "base__.".repeat(levels.len() - 1 - depth);
        for e in &v.entries {
            assert!(
                seen.insert(e.name.clone()),
                "{} appears twice in {}",
                e.name,
                class.interface
            );
            let params: Vec<String> = e
                .params
                .iter()
                .enumerate()
                .map(|(i, t)| format!("a{i}: {t}"))
                .collect();
            let args: Vec<String> = (0..e.params.len()).map(|i| format!("a{i}")).collect();
            let ret = e.ret.as_ref().map(|r| format!(" -> {r}")).unwrap_or_default();
            let comma = if params.is_empty() { "" } else { ", " };
            writeln!(
                out,
                "    unsafe fn {name}(&self{comma}{params}){ret} {{\n        unsafe {{ real::{name}(self.inner(){comma}{args}) }}\n    }}",
                name = e.name,
                params = params.join(", "),
                args = args.join(", "),
            )
            .unwrap();
            writeln!(
                real,
                "    /// `{name}` on the real object.\n    pub unsafe fn {name}(inner: *mut c_void{comma}{params}){ret} {{\n        unsafe {{ ((*(*(inner as *const *const {top}))).{path}{name})(inner{comma}{args}) }}\n    }}",
                name = e.name,
                params = params.join(", "),
                args = args.join(", "),
            )
            .unwrap();
            // State setters are read back in the draw snapshot. Commands with effects on resources or query
            // state cannot be recovered that way: track their list's capture coverage even while unarmed.
            let work = class.trait_name == "ContextMethods"
                && ([
                    "Draw",
                    "Dispatch",
                    "Copy",
                    "Update",
                    "ClearRenderTarget",
                    "ClearDepthStencil",
                    "ClearUnorderedAccess",
                    "ClearView",
                    "Discard",
                    "Resolve",
                    "GenerateMips",
                    "ResizeTilePool",
                    "TiledResourceBarrier",
                ]
                .iter()
                .any(|prefix| e.name.starts_with(prefix))
                    || matches!(
                        e.name.as_str(),
                        "Map" | "Unmap" | "Begin" | "End" | "ExecuteCommandList" | "SetResourceMinLOD"
                    ));
            let observe = if work {
                "    unsafe { (*(this as *const T)).before_command() };\n"
            } else {
                ""
            };
            let owning = class.trait_name == "ContextMethods" && d3d11::recorded(&e.name);
            let special = class.trait_name == "ContextMethods"
                && matches!(
                    e.name.as_str(),
                    "Map" | "UpdateSubresource" | "UpdateSubresource1" | "UpdateTiles"
                );
            let unsupported = class.trait_name == "ContextMethods"
                && matches!(
                    e.name.as_str(),
                    "SwapDeviceContextState"
                        | "SetHardwareProtectionState"
                        | "Signal"
                        | "Wait"
                        | "SetResourceMinLOD"
                );
            let mut before = observe.to_string();
            if owning || special || unsupported {
                before.push_str("    let object = unsafe { &*(this as *const T) };\n    let recorder = object.recorder();\n    if let Some(r) = recorder { unsafe { r.begin(object.inner()) }; }\n");
            }
            let condition = if e.ret.is_some() { "result.is_ok() && " } else { "" };
            let mut after = String::new();
            if unsupported {
                writeln!(after,"    if {condition}let Some(r) = recorder {{ r.gap(\"{} cannot be reconstructed exactly\"); }}",e.name).unwrap();
            }
            if owning {
                writeln!(after,"    if {condition}let Some(r) = recorder {{ r.record(|tape| unsafe {{ tape.{}({}) }}); }}", e.name, args.join(", ")).unwrap();
            }
            let call = format!(
                "unsafe {{ (*(this as *const T)).{}({}) }}",
                e.name,
                args.join(", ")
            );
            let body = if after.is_empty() {
                format!("{before}    {call}")
            } else if e.ret.is_none() {
                format!("{before}    {call};\n{after}")
            } else {
                format!("{before}    let result = {call};\n{after}    result")
            };
            writeln!(thunks,
                "unsafe extern \"system\" fn {name}<T: {t}>(this: *mut c_void{comma}{params}){ret} {{\n{body}\n}}",
                name=e.name, t=class.trait_name, comma=if params.is_empty(){""}else{", "},
                params=params.join(", ")).unwrap();
        }
    }
    out.push_str("}\n\n");
    real.push_str("}\n\n");
    out.push_str(&real);
    for (name, ret) in [
        (
            "QueryInterface",
            "iid: *const windows::core::GUID, out: *mut *mut c_void) -> windows::core::HRESULT",
        ),
        ("AddRef", ") -> u32"),
        ("Release", ") -> u32"),
    ] {
        let (params, args) = if name == "QueryInterface" {
            (format!(", {ret}"), "iid, out")
        } else {
            (ret.to_string(), "")
        };
        writeln!(
            out,
            "unsafe extern \"system\" fn {name}<T: {t}>(this: *mut c_void{params} {{\n    unsafe {{ (*(this as *const T)).{name}({args}) }}\n}}",
            t = class.trait_name,
        )
        .unwrap();
    }
    out.push_str(&thunks);
    if class.module.starts_with("d12_") {
        out.push_str(&d3d12::implementation(class, &levels));
    }

    // The vtable literal, nested from the most derived struct down to IUnknown's.
    let mut literal = String::from(
        "windows::core::IUnknown_Vtbl { QueryInterface: QueryInterface::<T>, AddRef: AddRef::<T>, Release: Release::<T> }",
    );
    for (name, v) in &levels {
        let mut s = format!("{} {{ base__: {literal}", module_of(name));
        for e in &v.entries {
            write!(s, ", {0}: {0}::<T>", e.name).unwrap();
        }
        s.push_str(" }");
        literal = s;
    }
    writeln!(
        out,
        "\n/// The `{iface}` vtable whose entries call `T`'s methods.\npub const fn vtable<T: {t}>() -> {top} {{\n    {literal}\n}}",
        iface = class.interface,
        t = class.trait_name,
    )
    .unwrap();
    out
}

fn rustfmt(path: &Path) {
    let status = Command::new("rustfmt")
        .args(["--edition", "2024"])
        .arg(path)
        .status()
        .expect("run rustfmt");
    assert!(status.success(), "rustfmt failed on {}", path.display());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smokes stage what cargo built: where `CARGO_TARGET_DIR` says, absolute or from the workspace.
    #[test]
    fn the_target_directory_is_cargos() {
        let root = Path::new(r"C:\work\locum");
        assert_eq!(target_dir_from(root, None), root.join("target"));
        assert_eq!(target_dir_from(root, Some("".into())), root.join("target"));
        assert_eq!(target_dir_from(root, Some("out".into())), root.join("out"));
        assert_eq!(
            target_dir_from(root, Some(r"D:\scratch".into())),
            PathBuf::from(r"D:\scratch")
        );
    }
}
