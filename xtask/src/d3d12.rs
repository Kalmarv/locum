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
    ] {
        std::fs::copy(root.join("target/release").join(file), stage.join(file)).unwrap();
    }
    let log = stage.join("log");
    std::fs::create_dir_all(&log).unwrap();
    let old_log = log.join("locum.log");
    if old_log.is_file() {
        std::fs::remove_file(&old_log).unwrap();
    }
    let result = Command::new(stage.join("locum-smoke-d3d12.exe"))
        .current_dir(&stage)
        .env("LOCUM_DIR", &log)
        .output()
        .expect("run D3D12 WARP smoke");
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
}
