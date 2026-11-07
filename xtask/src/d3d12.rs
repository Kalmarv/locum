//! D3D12-specific forwarding policy. ABI signatures still come from windows metadata.
use super::{Class, Vtbl, chain};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;

pub fn registry(classes: &[Class], vtbls: &HashMap<String, Vtbl>) -> String {
    let mut s = String::from(
        "//! Generated D3D12 interface dispatch.\nuse windows::core::{GUID, Interface};\nuse windows::Win32::Graphics::Direct3D12::*;\nuse core::ffi::c_void;\npub fn table(iid: &GUID) -> Option<(*const c_void, &'static str)> {\n",
    );
    let mut seen = BTreeSet::new();
    for c in classes.iter().filter(|c| c.module.starts_with("d12_")) {
        writeln!(
            s,
            "static {}: {}_Vtbl = super::{}::vtable::<crate::d3d12::Object>();",
            c.module.to_uppercase(),
            c.interface,
            c.module
        )
        .unwrap();
        for (n, _) in chain(c.interface, vtbls) {
            let iface = n.trim_end_matches("_Vtbl");
            if seen.insert(iface.to_string()) {
                writeln!(s,"if *iid == {iface}::IID {{ return Some((&{} as *const _ as *const c_void, \"{iface}\")); }}",c.module.to_uppercase()).unwrap();
            }
        }
    }
    s.push_str("None\n}\n");
    s
}

pub fn implementation(c: &Class, levels: &[(String, &Vtbl)]) -> String {
    let mut s = format!(
        "\n#[allow(clippy::let_and_return)] // Uniform post-call interception in generated methods.\nunsafe impl {} for crate::d3d12::Object {{\nfn inner(&self) -> *mut c_void {{ self.real() }}\nunsafe fn QueryInterface(&self, iid: *const windows::core::GUID, out: *mut *mut c_void) -> windows::core::HRESULT {{ unsafe {{ self.query(iid,out) }} }}\nunsafe fn AddRef(&self) -> u32 {{ self.add_ref() }}\nunsafe fn Release(&self) -> u32 {{ unsafe {{ self.release() }} }}\n",
        c.trait_name
    );
    for (_, v) in levels {
        for e in &v.entries {
            let params: Vec<_> = e
                .params
                .iter()
                .enumerate()
                .map(|(i, t)| format!("a{i}: {t}"))
                .collect();
            let mut args: Vec<_> = (0..params.len()).map(|i| format!("a{i}")).collect();
            let ret = e.ret.as_ref().map(|r| format!(" -> {r}")).unwrap_or_default();
            writeln!(
                s,
                "unsafe fn {}(&self{}{}){ret} {{ unsafe {{",
                e.name,
                if params.is_empty() { "" } else { ", " },
                params.join(", ")
            )
            .unwrap();
            // These raw mutable pointers are data, not COM interfaces.
            let data = matches!(
                e.name.as_str(),
                "CheckFeatureSupport"
                    | "GetPrivateData"
                    | "Serialize"
                    | "ReadFromSubresource"
                    | "GetMessage"
                    | "GetStorageFilter"
                    | "GetRetrievalFilter"
            );
            for (i, t) in e.params.iter().enumerate() {
                let compact = t.replace(' ', "");
                if compact == "*mutcore::ffi::c_void" && !data {
                    args[i] = format!("crate::d3d12::unwrap(a{i})");
                }
                if compact == "*const*mutcore::ffi::c_void" {
                    let count = match e.name.as_str() {
                        "SetEventOnMultipleFenceCompletion" => 2,
                        "EnqueueMakeResident" => 1,
                        _ => i.checked_sub(1).expect("array count must be explicit"),
                    };
                    writeln!(
                        s,
                        "let mapped{i} = crate::d3d12::inputs::objects(a{i}, a{count});"
                    )
                    .unwrap();
                    args[i] = format!("mapped{i}.ptr()");
                }
                let helper = match compact.as_str() {
                    "*constwindows::Win32::Graphics::Direct3D12::D3D12_GRAPHICS_PIPELINE_STATE_DESC" => {
                        Some(("graphics", None))
                    }
                    "*constwindows::Win32::Graphics::Direct3D12::D3D12_COMPUTE_PIPELINE_STATE_DESC" => {
                        Some(("compute", None))
                    }
                    "*constwindows::Win32::Graphics::Direct3D12::D3D12_PIPELINE_STATE_STREAM_DESC" => {
                        Some(("stream", None))
                    }
                    "*constwindows::Win32::Graphics::Direct3D12::D3D12_TEXTURE_COPY_LOCATION" => {
                        Some(("texture_copy", None))
                    }
                    "*constwindows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER" => {
                        Some(("barriers", Some(0)))
                    }
                    "*constwindows::Win32::Graphics::Direct3D12::D3D12_RENDER_PASS_RENDER_TARGET_DESC" => {
                        Some(("render_targets", Some(0)))
                    }
                    "*constwindows::Win32::Graphics::Direct3D12::D3D12_RENDER_PASS_DEPTH_STENCIL_DESC" => {
                        Some(("depth_stencil", None))
                    }
                    "*constwindows::Win32::Graphics::Direct3D12::D3D12_STATE_OBJECT_DESC" => {
                        Some(("state_object", None))
                    }
                    "*constwindows::Win32::Graphics::Direct3D12::D3D12_BARRIER_GROUP" => {
                        Some(("barrier_groups", Some(0)))
                    }
                    _ => None,
                };
                if let Some((helper, count)) = helper {
                    writeln!(
                        s,
                        "let mapped{i} = crate::d3d12::inputs::{helper}(a{i}{});",
                        count.map(|n| format!(", a{n}")).unwrap_or_default()
                    )
                    .unwrap();
                    args[i] = format!("mapped{i}.ptr()");
                }
            }

            if matches!(
                e.name.as_str(),
                "ExecuteCommandLists"
                    | "Close"
                    | "Reset"
                    | "DrawInstanced"
                    | "DrawIndexedInstanced"
                    | "Dispatch"
                    | "ExecuteIndirect"
                    | "DispatchRays"
                    | "CreateStateObject"
                    | "AddToStateObject"
                    | "CreatePipelineLibrary"
                    | "LoadPipeline"
                    | "LoadGraphicsPipeline"
                    | "LoadComputePipeline"
            ) {
                writeln!(s, "self.called(\"{}\");", e.name).unwrap();
            }
            let has_ret = e.ret.is_some();
            s.push_str(&super::d3d12_capture::before(c, e));
            if c.module == "d12_queue" && e.name == "ExecuteCommandLists" {
                // The array `before` arranged: the application's lists with locum's.
                args = vec!["count".into(), "lists".into()];
            }
            writeln!(
                s,
                "{}real::{}(self.real(){}{});",
                if has_ret { "let result = " } else { "" },
                e.name,
                if args.is_empty() { "" } else { ", " },
                args.join(", ")
            )
            .unwrap();
            if has_ret && e.ret.as_ref().is_some_and(|r| r.contains("HRESULT")) {
                if e.name == "SetPrivateDataInterface" {
                    s.push_str("if result.is_ok() { self.private_interface(a0, !a1.is_null()); }\n");
                }
                if e.name == "SetPrivateData" {
                    s.push_str("if result.is_ok() { self.private_interface(a0, false); }\n");
                }
                for i in 1..e.params.len() {
                    if e.params[i].replace(' ', "") == "*mut*mutcore::ffi::c_void"
                        && e.params[i - 1].contains("GUID")
                    {
                        writeln!(s, "crate::d3d12::wrap_output(result, a{}, a{i});", i - 1).unwrap();
                    }
                }
                if e.name == "GetPrivateData" {
                    s.push_str("self.private_output(result, a0, a1, a2);\n");
                }
            }
            s.push_str(&super::d3d12_capture::after(c, e));
            if has_ret {
                s.push_str("result\n");
            }
            s.push_str("} }\n");
        }
    }
    s.push_str("}\n");
    s
}

/// The D3D12 smoke on WARP, or with `hardware` what only a real GPU shows, on the default adapter; with the DLLs in
/// `dlls` where it is given.
pub fn smoke(root: &std::path::Path, hardware: bool, debug: bool, dlls: Option<&std::path::Path>) {
    use std::process::Command;
    super::build(
        root,
        &[
            "locum",
            "locum-shim-dxgi",
            "locum-shim-d3d12",
            "locum-shim-nvapi64",
            "locum-smoke-d3d12",
            "locum-cli",
        ],
        dlls,
    );
    let stage = super::target_dir(root).join("smoke-d3d12");
    std::fs::create_dir_all(&stage).unwrap();
    super::stage_files(
        root,
        &stage,
        &[
            "locum.dll",
            "dxgi.dll",
            "d3d12.dll",
            "nvapi64.dll",
            "locum-smoke-d3d12.exe",
            "locum-cli.exe",
        ],
        dlls,
    );
    // Each run's log, captures and blobs, not every earlier run's.
    let log = stage.join("log");
    let _ = std::fs::remove_dir_all(&log);
    std::fs::create_dir_all(&log).unwrap();
    if hardware {
        return hardware_smoke(&stage, &log);
    }
    // The fixture labels its captures with its process ID.
    let child = Command::new(stage.join("locum-smoke-d3d12.exe"))
        .current_dir(&stage)
        .env("LOCUM_DIR", &log)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run D3D12 WARP smoke");
    let pid = child.id();
    let result = child.wait_with_output().expect("run D3D12 WARP smoke");
    let stdout = String::from_utf8_lossy(&result.stdout);
    let stderr = String::from_utf8_lossy(&result.stderr);
    print!("{stdout}");
    eprint!("{stderr}");
    std::fs::write(stage.join("stdout.txt"), result.stdout).unwrap();
    std::fs::write(stage.join("stderr.txt"), result.stderr).unwrap();
    assert!(
        result.status.success(),
        "D3D12 WARP smoke failed: {}",
        result.status
    );
    let log = std::fs::read_to_string(log.join("locum.log")).unwrap();
    assert!(
        !log.contains("D3D12 object "),
        "per-object logging must not return"
    );
    for method in ["Close", "Reset", "ExecuteCommandLists", "Dispatch"] {
        assert_eq!(
            log.lines()
                .filter(|l| l.ends_with(&format!("D3D12 first {method}")))
                .count(),
            1,
            "first method logged once: {method}"
        );
    }
    assert!(
        log.contains("D3D12 totals:"),
        "capture must include aggregate counts"
    );
    for proof in [
        "wrapped ID3D12Device",
        "wrapped ID3D12CommandQueue",
        "wrapped ID3D12Resource",
        "first ExecuteCommandLists",
        "wrapped swap chain",
        "D3D12 first proxied Present",
    ] {
        assert!(log.contains(proof), "missing wrapper proof: {proof}");
    }
    println!("D3D12 log proves proxied device, queue, resources, list execution and swapchain");
    replay_checks(&stage, pid, debug);
}

/// The fixture's hardware scenarios on the default adapter, then their captures replayed there.
fn hardware_smoke(stage: &std::path::Path, log: &std::path::Path) {
    use std::process::Command;
    let child = Command::new(stage.join("locum-smoke-d3d12.exe"))
        .arg("--hardware")
        .current_dir(stage)
        .env("LOCUM_DIR", log)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("run D3D12 hardware smoke");
    let pid = child.id();
    let result = child.wait_with_output().expect("run D3D12 hardware smoke");
    print!("{}", String::from_utf8_lossy(&result.stdout));
    eprint!("{}", String::from_utf8_lossy(&result.stderr));
    assert!(
        result.status.success(),
        "D3D12 hardware smoke failed: {}",
        result.status
    );
    // Each capture verified on the adapter it was made on.
    let capture = |name: &str| {
        log.join("captures")
            .join(format!("{name}-{pid}"))
            .display()
            .to_string()
    };
    let (ok, text) = cli(stage, log, &["verify", &capture("planes")]);
    assert!(
        ok && text.contains("2 of 2 targets identical"),
        "planes: both draws replay as the fixture drew them"
    );
    println!(
        "ok   D3D12 hardware: a split-plane depth-stencil copied between two passes holds what the first left"
    );
    let (ok, text) = cli(stage, log, &["verify", &capture("indirect")]);
    indirect_verified(ok, &text, targets(stage, log, &capture("indirect")));
    println!("ok   D3D12 hardware: ExecuteIndirect with GPU addresses in its records replays as the game's");
    let said = String::from_utf8_lossy(&result.stdout);
    rays_verified(stage, log, &capture, &said, "hardware");
    samples_verified(stage, log, &capture("samples"), "hardware");
}

/// What verify says of the fixture's `samples` capture: each multisampled texture's contents written into every
/// sample, the frame replayed, and its pass ends (two slices of a colour, a slice of each other texture the device
/// renders to) read back sample by sample, each the game's bit for bit.
fn samples_verified(stage: &std::path::Path, log: &std::path::Path, capture: &str, on: &str) {
    let mut args = vec!["verify"];
    if on == "verify" {
        args.push("--warp");
    }
    args.push(capture);
    let (ok, text) = cli(stage, log, &args);
    let targets = crate::multisampled_targets(std::path::Path::new(capture));
    assert!(
        ok && targets >= 7
            && text.contains(&format!("{targets} of {targets} targets identical"))
            && !text.contains("multisampled"),
        "samples: every multisampled target ({targets}) replays as the game's, every sample"
    );
    println!(
        "ok   D3D12 {on}: multisampled textures' samples written and read back bit for bit, depth and stencil too"
    );
    if on != "verify" {
        return;
    }
    let id = |format: u32| -> String {
        let query = format!("SELECT id FROM resources WHERE samples = 4 AND format = {format}");
        let (ok, text) = cli(stage, log, &["sql", capture, &query]);
        assert!(ok, "locum-cli sql {query}");
        text.lines().nth(1).unwrap_or_default().trim().to_string()
    };
    // The D24S8's texel (0, 0) where its pass ended: each sample's depth and stencil, sample 3 as the frame redrew it.
    let (ok, text) = cli(stage, log, &["pixel", capture, &format!("#{}", id(44)), "0", "0"]);
    assert!(
        ok && text.contains("4 samples")
            && text.contains("sample 0: depth 0 stencil 0")
            && text.contains("sample 2: depth 1 stencil 255")
            && text.contains("stencil 60"),
        "samples: pixel shows each sample of a multisampled depth-stencil"
    );
    // The colour's second slice: its 4 samples' 8x4 images of 4 bytes a texel.
    let out = log.join("samples-colour-slice1.raw");
    let (ok, text) = cli(
        stage,
        log,
        &[
            "export",
            capture,
            &id(27),
            "-o",
            &out.display().to_string(),
            "--sub",
            "1",
        ],
    );
    let bytes = std::fs::metadata(&out).map_or(0, |m| m.len());
    assert!(
        ok && bytes == 512 && text.contains("its 4 samples stacked"),
        "samples: export --sub of a multisampled texture gives that subresource's samples ({bytes} bytes)"
    );
    println!("ok   D3D12 {on}: pixel and export take a multisampled texture's samples");
}

/// The fixture's raytracing captures, made where the device has raytracing tier 1.1, and the outputs their
/// DispatchRays and inline traces write: `indirect-rays` (an ExecuteIndirect's record runs one) and `rays` (two direct
/// ones, a collection and a state object grown by AddToStateObject, with local root arguments; a scene built before
/// the frame and one in it; and two inline traces, the scene a view in a table and a root SRV).
const RAYS: [(&str, usize); 2] = [("indirect-rays", 1), ("rays", 4)];

/// What verify says of the fixture's raytracing captures (`said`: what the smoke printed, which says why where it made
/// none), on the adapter they were made on and, where `on` is the WARP stage, on the default one too: every
/// DispatchRays replays, its tables replay's copies with replay's identifiers and local arguments, its scenes built
/// again from what capture copied, and each output it wrote is the game's bit for bit.
fn rays_verified(
    stage: &std::path::Path,
    log: &std::path::Path,
    capture: &dyn Fn(&str) -> String,
    said: &str,
    on: &str,
) {
    for (name, outputs) in RAYS {
        if let Some(why) = said
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name} SKIPPED: ")))
        {
            println!("SKIP D3D12 {on}: {name} not captured: {why}");
            continue;
        }
        let path = capture(name);
        assert!(
            std::path::Path::new(&path).is_dir(),
            "{name}: the smoke neither made the capture nor said why"
        );
        let adapters: &[&[&str]] = if on == "verify" {
            &[&["--warp"], &[]]
        } else {
            &[&[]]
        };
        for adapter in adapters {
            let mut args = vec!["verify"];
            args.extend_from_slice(adapter);
            args.push(&path);
            let (ok, text) = cli(stage, log, &args);
            let compared = format!(
                "what draws and dispatches wrote through UAVs: {outputs} identical to the game's, 0 different"
            );
            assert!(
                ok && text.contains(&compared) && !text.contains("incomplete"),
                "{name}: every DispatchRays replays and writes what the game's did ({})",
                adapter.join(" ")
            );
        }
        println!(
            "ok   D3D12 {on}: {name}: every DispatchRays replayed through replay's tables and scenes, its {outputs} \
             outputs the game's"
        );
    }
}

/// Runs locum-cli with `args`, printing what it says: whether it succeeded, and what it said.
fn cli(stage: &std::path::Path, log: &std::path::Path, args: &[&str]) -> (bool, String) {
    let output = std::process::Command::new(stage.join("locum-cli.exe"))
        .args(args)
        .env("LOCUM_DIR", log)
        .output()
        .unwrap_or_else(|e| panic!("locum-cli {args:?}: {e}"));
    let text =
        String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
    println!("--- locum-cli {}\n{text}", args.join(" "));
    (output.status.success(), text)
}

/// How many targets capture `capture` read back.
fn targets(stage: &std::path::Path, log: &std::path::Path, capture: &str) -> usize {
    let (ok, text) = cli(stage, log, &["sql", capture, "SELECT count(*) FROM targets"]);
    assert!(ok, "locum-cli sql {capture}");
    text.lines()
        .nth(1)
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or_else(|| panic!("{capture}: a count of targets: {text}"))
}

/// What verify must say of the fixture's `indirect` capture (`targets` of them: 4, and one each for the incrementing
/// constant and the mesh dispatches where the device has them): each target the game's, the copier's two outputs and what the root UAVs in records wrote the game's, nothing missing.
fn indirect_verified(ok: bool, text: &str, targets: usize) {
    assert!(targets >= 4, "indirect: {targets} targets read back");
    assert!(
        text.contains(&format!("{targets} of {targets} targets identical")),
        "indirect: every ExecuteIndirect's target replays as the game's"
    );
    assert!(
        text.contains("what draws and dispatches wrote through UAVs: 3 identical to the game's, 0 different"),
        "indirect: what the root UAVs in records wrote is compared, and the game's"
    );
    assert!(
        ok && text.contains("verified: every target and output compared is the game's"),
        "indirect: the capture is complete"
    );
}

/// Replays the fixture's captures with locum-cli on WARP: verify, a rule, and contents at an event.
fn replay_checks(stage: &std::path::Path, pid: u32, debug: bool) {
    let log = stage.join("log");
    let capture = |name: &str| {
        log.join("captures")
            .join(format!("{name}-{pid}"))
            .display()
            .to_string()
    };
    let run = |args: &[&str]| cli(stage, &log, args);
    let replayed = [
        "journal",
        "graphics",
        "modern",
        "contents",
        "passes",
        "compute-first",
        "cb-copied",
        "results",
        "reserved",
        "planes",
        "queries",
        "indirect",
        "samples",
    ];
    let sql = |name: &str, query: &str| -> String {
        let (ok, text) = run(&["sql", &capture(name), query]);
        assert!(ok, "locum-cli sql {query}");
        text.lines().nth(1).unwrap_or_default().trim().to_string()
    };
    // Every capture replays without an event, argument or descriptor kind it skips.
    for name in replayed {
        let (_, text) = run(&["verify", "--warp", &capture(name)]);
        assert!(
            !text.contains(" are not replayed"),
            "{name}: verify skipped something it should replay"
        );
        assert!(!text.contains("locum-cli: event "), "{name}: replay failed");
    }
    // What each dispatch and the clear wrote through UAVs, as replay makes it: the game's bytes, and the tickets the
    // same records whatever order the GPU took them in.
    let (_, text) = run(&["verify", "--warp", &capture("results")]);
    assert!(
        text.contains("what draws and dispatches wrote through UAVs: 9 identical to the game's, 0 different"),
        "results: every dispatch's outputs identical on replay"
    );
    assert!(
        text.contains("clear outputs: 1 identical to the game's, 0 different"),
        "results: the clear's output identical on replay"
    );
    let ordered = text
        .lines()
        .find_map(|l| l.strip_prefix("order-dependent shaders' outputs: "))
        .expect("results: the tickets' outputs compared");
    let n: Vec<u32> = ordered
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|w| w.parse().ok())
        .collect();
    assert!(
        n.len() == 3 && n[0] + n[1] == 2 && n[2] == 0,
        "results: the tickets' outputs: {ordered}"
    );
    println!("ok   D3D12 verify: what dispatches and a clear wrote through UAVs replays as the game's");
    // ExecuteIndirect records naming buffers by GPU address (vertex and index buffer views, root CBVs, SRVs and
    // UAVs), with root constants, written by the CPU, by a dispatch with a count, and run by a bundle: replay runs them
    // on its own buffers' addresses, the buffers only the records name have the contents the capture copied after the
    // frame, and what a root UAV in them wrote is compared.
    let indirect_targets = targets(stage, &log, &capture("indirect"));
    let (ok, text) = run(&["verify", "--warp", &capture("indirect")]);
    indirect_verified(ok, &text, indirect_targets);
    // Where it was made and verified, as `info` and verify say, and the verification kept in it.
    let on = |args: &[&str]| {
        let indirect = capture("indirect");
        let mut given = vec![args[0], indirect.as_str()];
        given.extend_from_slice(&args[1..]);
        run(&given).1
    };
    for (passed, what) in super::environment_checks(&on, &text, "d3d12") {
        assert!(passed, "indirect: {what}");
        println!("ok   D3D12 verify: {what}");
    }
    // locum's stand-ins as a translation layer, on WARP as the captures were made: conform held to the indirect
    // capture's draws and outputs, and refusing the graphics capture, whose replay compares nothing.
    let (indirect, graphics) = (capture("indirect"), capture("graphics"));
    for (passed, what) in super::conform_checks(
        &stage.join("locum-cli.exe"),
        stage,
        &log.join("conform"),
        &super::Conformance {
            capture: std::path::Path::new(&indirect),
            nothing: std::path::Path::new(&graphics),
            dlls: &["d3d12.dll", "dxgi.dll", "locum.dll", "nvapi64.dll"],
            needed: 1,
            rule: None,
            warp: true,
        },
    ) {
        assert!(passed, "conform: {what}");
        println!("ok   D3D12 {what}");
    }
    println!(
        "ok   D3D12 verify: ExecuteIndirect with GPU addresses in its records replays as the game's ({indirect_targets} \
         targets)"
    );
    // What cannot be proven is said: an uninstrumented list's records are a gap, and a buffer only a root UAV names
    // has no contents as the frame began, though what it was written is compared.
    let (ok, text) = run(&["verify", "--warp", &capture("indirect-gaps")]);
    assert!(!ok, "indirect-gaps: verify must not call the capture complete");
    for said in [
        "incomplete: capture gap: d3d12-indirect-arguments its list was not instrumented",
        "resources the frame used have no contents",
        "what draws and dispatches wrote through UAVs: 1 identical to the game's, 0 different",
    ] {
        assert!(text.contains(said), "indirect-gaps: verify says {said:?}");
    }
    println!(
        "ok   D3D12 verify: ExecuteIndirect records it could not read, and contents it could not know, are gaps"
    );
    let said = std::fs::read_to_string(stage.join("stdout.txt")).expect("the WARP smoke's output");
    rays_verified(stage, &log, &capture, &said, "verify");
    samples_verified(stage, &log, &capture("samples"), "verify");
    // Under the debug layer, with a pass under GPU-based validation of its own: the dispatches write a texture made
    // for simultaneous access as a UAV, which that validation takes for an error the spec does not make it.
    if debug {
        let (ok, text) = run(&["verify", "--warp", "--debug", &capture("results")]);
        assert!(
            text.lines()
                .any(|l| l.starts_with("validated in ") && l.ends_with(": 0 errors")),
            "results: GPU-based validation found errors"
        );
        assert!(
            !text.contains("D3D12 validation"),
            "results: the debug layer found errors"
        );
        assert!(
            ok && text.contains(
                "what draws and dispatches wrote through UAVs: 9 identical to the game's, 0 different"
            ),
            "results: verify --debug compares as verify does"
        );
        println!("ok   D3D12 verify --debug: no validation errors, and the outputs compared as without it");
        let (ok, text) = run(&["verify", "--warp", "--debug", &capture("indirect")]);
        assert!(
            text.lines()
                .any(|l| l.starts_with("validated in ") && l.ends_with(": 0 errors")),
            "indirect: GPU-based validation found errors"
        );
        assert!(
            !text.contains("D3D12 validation"),
            "indirect: the debug layer found errors"
        );
        indirect_verified(ok, &text, indirect_targets);
        println!(
            "ok   D3D12 verify --debug: ExecuteIndirect with addresses in its records, no validation errors"
        );
        // Every other capture too: its frame replays under both without an error, and compares as without them.
        let rays = RAYS.map(|(name, _)| name);
        let made = rays
            .iter()
            .filter(|name| std::path::Path::new(&capture(name)).is_dir());
        for name in replayed.iter().chain(&["indirect-gaps"]).chain(made) {
            let (plain_ok, plain) = run(&["verify", "--warp", &capture(name)]);
            let (ok, text) = run(&["verify", "--warp", "--debug", &capture(name)]);
            assert!(
                text.lines()
                    .any(|l| l.starts_with("validated in ") && l.ends_with(": 0 errors")),
                "{name}: GPU-based validation found errors"
            );
            assert!(
                !text.contains("D3D12 validation"),
                "{name}: the debug layer found errors"
            );
            let last = |t: &str| {
                t.lines()
                    .rfind(|l| !l.trim().is_empty())
                    .unwrap_or_default()
                    .to_string()
            };
            assert_eq!(
                (ok, last(&text)),
                (plain_ok, last(&plain)),
                "{name}: verify --debug compares as verify does"
            );
        }
        println!("ok   D3D12 verify --debug: every capture, no validation errors, compared as without it");
    }
    // Passes' targets, drawn from root constants across lists, are the fixture's bit for bit.
    let (_, text) = run(&["verify", "--warp", &capture("passes")]);
    let identical = text.lines().find_map(|l| {
        let w: Vec<&str> = l.split_whitespace().collect();
        (w.len() > 4
            && w[1] == "of"
            && w[3] == "targets"
            && w[4] == "identical;"
            && w[0] == w[2]
            && w[0] != "0")
            .then(|| w[0].to_string())
    });
    let targets = identical.expect("passes: every target identical on replay");
    println!("ok   D3D12 verify: passes' {targets} targets replay bit for bit");
    let (_, text) = run(&["verify", "--warp", &capture("planes")]);
    assert!(
        text.contains("2 of 2 targets identical"),
        "planes: both draws' depth-stencil replay as copied"
    );
    println!("ok   D3D12 verify: a split-plane depth-stencil's two passes replay as copied");

    // A constant rule on a root constants register: the fixture's colour as recorded, the rule's after.
    let out = log.join("replay-rule");
    let (_, text) = run(&[
        "replay",
        "--warp",
        &capture("passes"),
        "6",
        "--rule",
        "constant ps b0 c0 0 1 0 1",
        "-o",
        &out.display().to_string(),
    ]);
    assert!(
        text.contains("against the game's: identical") && text.contains("64 of 64 texels differ"),
        "replay with a root-constant rule"
    );
    println!("ok   D3D12 replay: a rule on root constants changes every texel; without it, the game's");

    // What draw 6 left in its target, from the replay of the frame up to the event after it.
    let event: i64 = sql("passes", "SELECT event FROM draws WHERE n = 6")
        .parse()
        .expect("draw 6's event");
    let target = sql(
        "passes",
        "SELECT resource FROM bindings WHERE draw = 6 AND kind = 'rtv'",
    );
    let raw = log.join("at.raw");
    let (ok, _) = run(&[
        "export",
        &capture("passes"),
        &target,
        "--at",
        &(event + 1).to_string(),
        "--warp",
        "-o",
        &raw.display().to_string(),
    ]);
    assert!(ok, "export --at");
    // passes.rs draws its sixth triangle, over the whole 8x8 target, in cyan.
    let bytes = std::fs::read(&raw).unwrap();
    assert!(
        bytes.len() == 8 * 8 * 4 && bytes.chunks(4).all(|t| t == [0, 255, 255, 255]),
        "export --at: draw 6's cyan in every texel"
    );
    println!("ok   D3D12 export --at: a target as the event after a draw saw it");

    // A reserved texture: its tiles as the frame found them, and replay's copy holding what its first use read.
    let texture = sql(
        "reserved",
        "SELECT id FROM resources WHERE width = 256 AND height = 256 AND mips = 2",
    );
    let (ok, text) = run(&["resource", &capture("reserved"), &texture]);
    assert!(
        ok && text.contains("  mip 0: 3 of 4 tiles mapped")
            && text.contains("the frame's UpdateTileMappings")
            && text.contains("the frame's CopyTileMappings"),
        "resource: a reserved texture's tiles mapped per mip, and the frame's changes"
    );
    let events = |id: &str| -> (i64, i64) {
        let text = sql(
            "reserved",
            &format!("SELECT min(event) || ' ' || max(event) FROM event_resources WHERE resource = {id}"),
        );
        let mut n = text.split(' ').map(|n| n.parse().expect("an event"));
        (n.next().unwrap(), n.next().unwrap())
    };
    let export = |id: &str, at: i64, name: &str| -> Vec<u8> {
        let raw = log.join(name);
        let (ok, text) = run(&[
            "export",
            &capture("reserved"),
            id,
            "--at",
            &at.to_string(),
            "--warp",
            "-o",
            &raw.display().to_string(),
        ]);
        assert!(ok, "export #{id} at event {at}");
        assert!(
            !text.contains("made committed") && !text.contains("are not run"),
            "replay makes the reserved texture and the placed buffers in the game's heaps: {text}"
        );
        std::fs::read(&raw).unwrap()
    };
    let (first, last) = events(&texture);
    assert!(first < last, "the reserved texture read twice");
    // reserved.rs fills mip 0's byte i with 77 + i and leaves tile 2 (x 0..128, y 128..256) unmapped; in the frame,
    // after the first read, CopyTileMappings maps tile 2 where tile 0 is.
    let bytes = export(&texture, first, "reserved.raw");
    let mip0 = |b: &[u8], x: usize, y: usize| b[y * 1024 + x * 4];
    let filled = |x: usize, y: usize| (77 + y * 1024 + x * 4) as u8;
    assert!(
        bytes.len() >= 256 * 1024
            && (0..128).all(|y| (0..256).all(|x| mip0(&bytes, x, y) == filled(x, y)))
            && (128..256).all(|y| (0..128).all(|x| mip0(&bytes, x, y) == 0)),
        "export: the reserved texture as its first use read it, zero where a tile was unmapped"
    );
    let bytes = export(&texture, last, "reserved-remapped.raw");
    assert!(
        (128..256).all(|y| (0..128).all(|x| mip0(&bytes, x, y) == filled(x, y - 128)))
            && (128..256).all(|y| (128..256).all(|x| mip0(&bytes, x, y) == filled(x, y))),
        "export: after the frame's remaps, tile 2 holds what tile 0 does"
    );
    // The placed buffer the frame read, holding what it wrote through the other at the same offset.
    let aliased = sql(
        "reserved",
        "SELECT max(id) FROM d12_objects WHERE kind LIKE 'ID3D12Resource%' AND json_extract(description, '$.heap') = \
         (SELECT id FROM d12_objects WHERE kind LIKE 'ID3D12Heap%' AND json_extract(description, '$.bytes') = 327680)",
    );
    let bytes = export(&aliased, events(&aliased).1, "aliased.raw");
    assert!(
        bytes.len() == 65536 && bytes.iter().enumerate().all(|(i, &b)| b == (i * 7 + 3) as u8),
        "export: a placed buffer reads what the frame wrote through another at its offset"
    );
    println!(
        "ok   D3D12 reserved and placed: tiles per mip, contents as first used, the frame's remaps run, and memory \
         two placed buffers share"
    );
    // The same capture as one from before heaps were described has it: the heaps' descriptions empty and no placed
    // resource's size. Replay makes the heaps from what the placed buffers record and still shares their memory.
    let old = log.join("captures").join(format!("undescribed-{pid}"));
    std::fs::create_dir_all(&old).unwrap();
    let db = old.join("capture.db");
    std::fs::copy(
        log.join("captures")
            .join(format!("reserved-{pid}"))
            .join("capture.db"),
        &db,
    )
    .expect("copy the reserved capture");
    let blanked = rusqlite::Connection::open(&db)
        .and_then(|db| {
            let heaps = db.execute(
                "UPDATE d12_objects SET description = '{}' WHERE kind LIKE 'ID3D12Heap%'",
                [],
            )?;
            db.execute(
                "UPDATE d12_objects SET description = json_remove(description, '$.bytes') \
                 WHERE kind LIKE 'ID3D12Resource%' AND json_extract(description, '$.heap') != 0",
                [],
            )?;
            Ok(heaps)
        })
        .expect("blank the copy's heap descriptions");
    assert!(blanked > 0, "the reserved capture has heaps");
    let raw = log.join("undescribed.raw");
    let (ok, text) = run(&[
        "export",
        &old.display().to_string(),
        &aliased,
        "--at",
        &events(&aliased).1.to_string(),
        "--warp",
        "-o",
        &raw.display().to_string(),
    ]);
    assert!(ok, "export from the undescribed copy");
    assert!(
        text.contains("heaps the capture does not describe") && !text.contains("made committed"),
        "replay infers the heaps the copy does not describe and places the buffers in them: {text}"
    );
    let bytes = std::fs::read(&raw).unwrap();
    assert!(
        bytes.len() == 65536 && bytes.iter().enumerate().all(|(i, &b)| b == (i * 7 + 3) as u8),
        "export: in an inferred heap, a placed buffer reads what the frame wrote through another at its offset"
    );
    println!(
        "ok   D3D12 heaps a capture does not describe: made from their placed resources, their memory shared"
    );
    // The same capture as one from a GPU that took less of its heaps than this device takes: the placed buffers
    // took half a tile there, at a 4 KiB alignment this device does not take for a buffer, and a texture the frame
    // does not use took 4 KiB at the start of each heap the reserved texture's tiles are in. Replay lays those heaps out again, the tiles after that texture at other offsets, and
    // the buffers still share their memory, the texture its tiles, before and after the frame's remaps.
    let moved = log.join("captures").join(format!("relocated-{pid}"));
    std::fs::create_dir_all(&moved).unwrap();
    let db = moved.join("capture.db");
    std::fs::copy(
        log.join("captures")
            .join(format!("reserved-{pid}"))
            .join("capture.db"),
        &db,
    )
    .expect("copy the reserved capture");
    let (buffers, tile_heaps) = rusqlite::Connection::open(&db)
        .and_then(|db| {
            let buffers = db.execute(
                "UPDATE d12_objects SET description = json_set(description, '$.bytes', 32768, '$.alignment', 4096) \
                 WHERE kind LIKE 'ID3D12Resource%' AND json_extract(description, '$.heap') = \
                 (SELECT id FROM d12_objects WHERE kind LIKE 'ID3D12Heap%' \
                  AND json_extract(description, '$.bytes') = 327680)",
                [],
            )?;
            // D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES; a 128x128 R8G8B8A8_UNORM texture takes 64 KiB.
            let tile_heaps = db.execute(
                "INSERT INTO d12_objects(id, kind, device, desc_version, description) \
                 SELECT (SELECT max(id) FROM d12_objects) + row_number() OVER (ORDER BY id), 'ID3D12Resource', \
                 device, desc_version, json_object('dimension', 3, 'alignment', 0, 'width', 128, 'height', 128, \
                 'depth_or_array_size', 1, 'mips', 1, 'format', 28, 'sample_count', 1, 'sample_quality', 0, \
                 'layout', 0, 'flags', 0, 'heap', id, 'heap_offset', 0, 'bytes', 4096) \
                 FROM d12_objects WHERE kind LIKE 'ID3D12Heap%' AND json_extract(description, '$.flags') = 68",
                [],
            )?;
            Ok((buffers, tile_heaps))
        })
        .expect("shrink what the copy's placed resources took");
    assert!(
        buffers == 2 && tile_heaps > 0,
        "the reserved capture has two placed buffers ({buffers}) and heaps of tiles ({tile_heaps})"
    );
    let from_moved = |id: &str, at: i64, name: &str| -> Vec<u8> {
        let raw = log.join(name);
        let (ok, text) = run(&[
            "export",
            &moved.display().to_string(),
            id,
            "--at",
            &at.to_string(),
            "--warp",
            "-o",
            &raw.display().to_string(),
        ]);
        assert!(ok, "export #{id} at event {at} from the relocated copy");
        assert!(
            text.contains("heaps were laid out again for this device")
                && !text.contains("made committed")
                && !text.contains("are not run")
                && !text.contains("do not here"),
            "replay lays the copy's heaps out again and shares all their memory: {text}"
        );
        std::fs::read(&raw).unwrap()
    };
    let bytes = from_moved(&texture, first, "relocated.raw");
    assert!(
        (0..128).all(|y| (0..256).all(|x| mip0(&bytes, x, y) == filled(x, y)))
            && (128..256).all(|y| (0..128).all(|x| mip0(&bytes, x, y) == 0)),
        "export: in heaps laid out again, the reserved texture as its first use read it"
    );
    let bytes = from_moved(&texture, last, "relocated-remapped.raw");
    assert!(
        (128..256).all(|y| (0..128).all(|x| mip0(&bytes, x, y) == filled(x, y - 128)))
            && (128..256).all(|y| (128..256).all(|x| mip0(&bytes, x, y) == filled(x, y))),
        "export: in heaps laid out again, after the frame's remaps, tile 2 holds what tile 0 does"
    );
    let bytes = from_moved(&aliased, events(&aliased).1, "relocated-aliased.raw");
    assert!(
        bytes.len() == 65536 && bytes.iter().enumerate().all(|(i, &b)| b == (i * 7 + 3) as u8),
        "export: in a heap laid out again, a placed buffer reads what the frame wrote through another"
    );
    println!(
        "ok   D3D12 heaps this device takes more of than the game's GPU did: laid out again, every tile and buffer \
         sharing what it shared"
    );
    // Queries: what replay resolves is what the GPU gave the fixture (queries.rs), but for the timestamps, which are
    // the replay's clock. Through `export --at`, so the plan must take the draws the queries counted.
    let expected = std::fs::read(log.join("queries-expected.bin")).expect("queries-expected.bin");
    let dst = |filter: &str| {
        sql(
            "queries",
            &format!(
                "SELECT json_extract(args, '$[4]') FROM events WHERE call = 'ResolveQueryData' AND {filter}"
            ),
        )
    };
    let readback = dst("json_extract(args, '$[1]') = 2");
    let resolved = dst(&format!("json_extract(args, '$[4]') <> {readback}"));
    let last: i64 = sql(
        "queries",
        "SELECT max(seq) FROM events WHERE call = 'ResolveQueryData'",
    )
    .parse()
    .expect("the last resolve");
    let export = |id: &str, name: &str| -> Vec<u8> {
        let raw = log.join(name);
        let (ok, text) = run(&[
            "export",
            &capture("queries"),
            id,
            "--at",
            &(last + 1).to_string(),
            "--warp",
            "-o",
            &raw.display().to_string(),
        ]);
        assert!(ok, "export #{id} after the resolves");
        assert!(
            !text.contains("are not replayed"),
            "queries: replay runs them all: {text}"
        );
        std::fs::read(&raw).unwrap()
    };
    let replayed = export(&readback, "queries-readback.raw");
    let word = |b: &[u8], at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap());
    assert_eq!(
        (word(&replayed, 0), word(&replayed, 8), word(&replayed, 16)),
        (word(&expected, 0), word(&expected, 8), word(&expected, 16)),
        "queries: occlusion and binary occlusion as the GPU counted them in the fixture"
    );
    assert_eq!(
        replayed[64..152],
        expected[64..152],
        "queries: pipeline statistics as the GPU counted them in the fixture"
    );
    let (t0, t1) = (word(&replayed, 160), word(&replayed, 168));
    assert!(
        t0 > 0 && t1 >= t0,
        "queries: the replay's timestamps, in order: {t0} {t1}"
    );
    let other = export(&resolved, "queries-resolved.raw");
    assert_eq!(
        word(&other, 0),
        word(&expected, 0),
        "queries: the occlusion result resolved into a default-heap buffer"
    );
    println!(
        "ok   D3D12 queries: occlusion {}, binary {} and {}, pipeline statistics as the GPU counted them, timestamps \
         the replay's own",
        word(&replayed, 0),
        word(&replayed, 8),
        word(&replayed, 16)
    );
}
