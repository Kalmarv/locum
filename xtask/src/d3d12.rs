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

pub fn smoke(root: &std::path::Path) {
    use std::process::Command;
    let status = Command::new(env!("CARGO"))
        .current_dir(root)
        .args([
            "build",
            "--release",
            "-p",
            "locum",
            "-p",
            "locum-shim-dxgi",
            "-p",
            "locum-shim-d3d12",
            "-p",
            "locum-shim-nvapi64",
            "-p",
            "locum-smoke-d3d12",
            "-p",
            "locum-cli",
        ])
        .status()
        .expect("build D3D12 smoke");
    assert!(status.success(), "D3D12 smoke build failed");
    let stage = root.join("target/smoke-d3d12");
    std::fs::create_dir_all(&stage).unwrap();
    for file in [
        "locum.dll",
        "dxgi.dll",
        "d3d12.dll",
        "nvapi64.dll",
        "locum-smoke-d3d12.exe",
        "locum-cli.exe",
    ] {
        std::fs::copy(root.join("target/release").join(file), stage.join(file)).unwrap();
    }
    let log = stage.join("log");
    std::fs::create_dir_all(&log).unwrap();
    let old_log = log.join("locum.log");
    if old_log.is_file() {
        std::fs::remove_file(&old_log).unwrap();
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
    let log = std::fs::read_to_string(old_log).unwrap();
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
    replay_checks(&stage, pid);
}

/// Replays the fixture's captures with locum-cli on WARP: verify, a rule, and contents at an event.
fn replay_checks(stage: &std::path::Path, pid: u32) {
    use std::process::Command;
    let log = stage.join("log");
    let capture = |name: &str| {
        log.join("captures")
            .join(format!("{name}-{pid}"))
            .display()
            .to_string()
    };
    let run = |args: &[&str]| {
        let output = Command::new(stage.join("locum-cli.exe"))
            .args(args)
            .env("LOCUM_DIR", &log)
            .output()
            .unwrap_or_else(|e| panic!("locum-cli {args:?}: {e}"));
        let text =
            String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
        println!("--- locum-cli {}\n{text}", args.join(" "));
        (output.status.success(), text)
    };
    let sql = |name: &str, query: &str| -> String {
        let (ok, text) = run(&["sql", &capture(name), query]);
        assert!(ok, "locum-cli sql {query}");
        text.lines().nth(1).unwrap_or_default().trim().to_string()
    };
    // Every capture replays without an event, argument or descriptor kind it skips.
    for name in [
        "journal",
        "graphics",
        "modern",
        "contents",
        "passes",
        "compute-first",
        "cb-copied",
        "results",
        "reserved",
    ] {
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
    let first: i64 = sql(
        "reserved",
        &format!("SELECT min(event) FROM event_resources WHERE resource = {texture}"),
    )
    .parse()
    .expect("the reserved texture's first event");
    let raw = log.join("reserved.raw");
    let (ok, text) = run(&[
        "export",
        &capture("reserved"),
        &texture,
        "--at",
        &first.to_string(),
        "--warp",
        "-o",
        &raw.display().to_string(),
    ]);
    assert!(ok, "export a reserved texture");
    assert!(
        text.contains("reserved resources were made committed")
            && text.contains("2 changes to their tile mappings"),
        "replay says what it does with a reserved resource: {text}"
    );
    // reserved.rs fills mip 0's byte i with 77 + i and leaves tile 2 (x 0..128, y 128..256) unmapped.
    let bytes = std::fs::read(&raw).unwrap();
    let mip0 = |x: usize, y: usize| bytes[y * 1024 + x * 4];
    assert!(
        bytes.len() >= 256 * 1024
            && (0..128).all(|y| (0..256).all(|x| mip0(x, y) == (77 + y * 1024 + x * 4) as u8))
            && (128..256).all(|y| (0..128).all(|x| mip0(x, y) == 0)),
        "export: the reserved texture as its first use read it, zero where a tile was unmapped"
    );
    println!(
        "ok   D3D12 reserved: tiles per mip, the frame's mapping changes, and replay's contents as first used"
    );
}
