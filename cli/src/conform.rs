//! `locum-cli conform`: how a translation layer (DXVK, vkd3d-proton) draws a game's frame. The capture is replayed
//! twice, each replay compared with the game's targets and outputs as `verify` does: once through the system's
//! Direct3D, once through the layer's DLLs. Each is a locum-cli process of its own (a process has one `d3d11.dll`),
//! the layer's run with `--layer`, which loads its DLLs in place of the system's before Direct3D is called (see
//! [`crate::runtime`]).
//!
//! A comparison the system's replay passes and the layer's fails is the layer's to explain: the same capture, events
//! and resources gave the game's bytes through the system's runtime and other bytes through the layer. One both fail
//! is not a finding about the layer (the capture, replay or driver differs from the game already); whether the two
//! replays' bytes are the same is said. What the layer's replay could not do that the system's did (a command
//! signature it could not make, an event it stopped at) is a finding too.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::open::Capture;
use crate::out;
use crate::replay::{Checked, Report};

/// The DLLs a layer must have for a capture of `api`: without them its replay would run on the system's runtime.
fn needed(api: &str) -> &'static [&'static str] {
    if api == "d3d12" {
        &["d3d12.dll"]
    } else {
        &["d3d11.dll", "dxgi.dll"]
    }
}

/// Replays `c` through the system's runtime and through the DLLs in `layer`, writing both replays' reports, outputs
/// and the pictures of what differs under `out`; fails if the layer's replay differs from the game's where the
/// system's does not, or could not do what the system's did.
pub fn conform(
    c: &Capture,
    layer: &Path,
    out_dir: &Path,
    limit: Option<i64>,
    warp: bool,
    blobs: Option<&Path>,
) -> Result<(), String> {
    let api = c.meta("api").unwrap_or_else(|| "d3d11".into());
    let dlls: Vec<PathBuf> = std::fs::read_dir(layer)
        .map_err(|e| format!("{}: {e}", layer.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")))
        .collect();
    let has = |name: &str| {
        dlls.iter()
            .any(|p| p.file_name().is_some_and(|f| f.eq_ignore_ascii_case(name)))
    };
    let lacking: Vec<&str> = needed(&api).iter().copied().filter(|n| !has(n)).collect();
    if !lacking.is_empty() {
        return Err(format!(
            "{} has no {}: a {api} capture's replay would run on the system's runtime",
            layer.display(),
            lacking.join(" or ")
        ));
    }
    let absolute = |p: &Path| std::path::absolute(p).map_err(|e| format!("{}: {e}", p.display()));
    let (out_dir, layer) = (&absolute(out_dir)?, &absolute(layer)?);
    std::fs::create_dir_all(out_dir).map_err(|e| format!("{}: {e}", out_dir.display()))?;
    let me = std::env::current_exe().map_err(|e| format!("locum-cli's path: {e}"))?;
    let capture = absolute(&c.path)?.display().to_string();
    let replay = |through: Option<&Path>, side: &str| -> Result<Report, String> {
        let report = out_dir.join(format!("{side}.json"));
        let _ = std::fs::remove_file(&report);
        let mut cmd = Command::new(&me);
        // The replay runs in another folder.
        if let Some(b) = blobs {
            cmd.arg("--blobs").arg(absolute(b)?);
        }
        if let Some(dir) = through {
            cmd.arg("--layer").arg(dir);
        }
        cmd.args(["verify", &capture, "--report"])
            .arg(&report)
            .arg("--out")
            .arg(out_dir.join(side));
        if let Some(l) = limit {
            cmd.args(["--limit", &l.to_string()]);
        }
        if warp {
            cmd.arg("--warp");
        }
        // What a layer writes to its working folder (DXVK's logs, vkd3d-proton's pipeline cache) goes with the side's
        // output, which an earlier run's is not left in.
        let dir = out_dir.join(side);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let output = cmd
            .current_dir(&dir)
            .output()
            .map_err(|e| format!("run {}: {e}", me.display()))?;
        let text =
            String::from_utf8_lossy(&output.stdout).into_owned() + &String::from_utf8_lossy(&output.stderr);
        let log = out_dir.join(format!("{side}.txt"));
        std::fs::write(&log, &text).map_err(|e| format!("{}: {e}", log.display()))?;
        match std::fs::read_to_string(&report) {
            Ok(json) => serde_json::from_str(&json).map_err(|e| format!("{}: {e}", report.display())),
            // A replay that crashed writes no report: what it printed last says where.
            Err(_) => Err(format!(
                "the {side} replay ended ({}) without a report; it printed last:\n{}",
                output.status,
                text.lines()
                    .rev()
                    .take(8)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join("\n")
            )),
        }
    };
    out!("replaying {capture} through the system's runtime");
    let system = replay(None, "system")?;
    out!("{}", system.result);
    // Nothing compared, nothing the layer could be found to do differently.
    if system.checks.is_empty() {
        return Err(format!(
            "the system's replay compared nothing with the game's, so the layer's has nothing to be held to: {}",
            system.result
        ));
    }
    if let Some(e) = system.environment.as_ref().filter(|e| !e.layers.is_empty()) {
        return Err(format!(
            "the system's replay loaded Direct3D DLLs from outside the system folder: {}",
            e.layers
                .values()
                .map(|l| l.path.clone())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out!("replaying it through {}", layer.display());
    let theirs = replay(Some(layer), "layer")?;
    out!("{}", theirs.result);
    // The layer's replay must have run on the layer: its DLLs loaded.
    let loaded = theirs
        .environment
        .as_ref()
        .map(|e| e.layers.clone())
        .unwrap_or_default();
    let unused: Vec<&str> = needed(&api)
        .iter()
        .copied()
        .filter(|n| !loaded.keys().any(|k| k.eq_ignore_ascii_case(n)))
        .collect();
    if !unused.is_empty() && theirs.environment.is_some() {
        return Err(format!(
            "the layer's replay did not load its {} (loaded from outside the system folder: {})",
            unused.join(" or "),
            if loaded.is_empty() {
                "nothing".into()
            } else {
                loaded.keys().cloned().collect::<Vec<_>>().join(", ")
            }
        ));
    }
    let verdict = compare(&system, &theirs);
    for line in &verdict.lines {
        out!("{line}");
    }
    out!("{}", verdict.summary);
    out!(
        "reports: {} and {}; what differs from the game's: {} and {}",
        out_dir.join("system.json").display(),
        out_dir.join("layer.json").display(),
        out_dir.join("system").display(),
        out_dir.join("layer").display()
    );
    if verdict.failures > 0 {
        Err(format!(
            "{} findings about the layer ({})",
            verdict.failures, verdict.ends
        ))
    } else {
        Ok(())
    }
}

/// What [`compare`] found: a line per finding and per comparison both replays failed, the counts, and how many are
/// the layer's to explain.
#[derive(Debug, Default, PartialEq)]
pub struct Verdict {
    pub lines: Vec<String>,
    pub summary: String,
    pub failures: usize,
    pub ends: String,
}

/// Compares the system's replay (`system`) and the layer's (`layer`) of one capture, comparison by comparison.
pub fn compare(system: &Report, layer: &Report) -> Verdict {
    let theirs: HashMap<&str, &Checked> = layer.checks.iter().map(|c| (c.key.as_str(), c)).collect();
    let mut v = Verdict::default();
    let (mut agree, mut both_same, mut both_other, mut only_layer_passes) = (0, 0, 0, 0);
    let mut missed = Vec::new();
    for s in &system.checks {
        let Some(l) = theirs.get(s.key.as_str()) else {
            missed.push(s);
            continue;
        };
        match (s.identical, l.identical) {
            (true, true) => agree += 1,
            (true, false) => {
                v.failures += 1;
                v.lines.push(format!(
                    "LAYER DIFFERS {} (event {}): the system's replay is the game's; through the layer {}",
                    s.what, s.event, l.detail
                ));
            }
            (false, true) => {
                only_layer_passes += 1;
                v.lines.push(format!(
                    "note: {} (event {}): the game's through the layer only; the system's replay {}",
                    s.what, s.event, s.detail
                ));
            }
            (false, false) if s.replay == l.replay => {
                both_same += 1;
                v.lines.push(format!(
                    "note: {} (event {}): both replays differ from the game's, with the same bytes ({})",
                    s.what, s.event, s.detail
                ));
            }
            (false, false) => {
                both_other += 1;
                v.lines.push(format!(
                    "note: {} (event {}): both replays differ from the game's, and from each other (system: {}; \
                     layer: {})",
                    s.what, s.event, s.detail, l.detail
                ));
            }
        }
    }
    if !missed.is_empty() {
        v.failures += missed.len();
        let first = missed[0];
        v.lines.push(format!(
            "LAYER STOPPED: {} comparisons the system's replay made, from {} (event {}), the layer's never reached: {}",
            missed.len(),
            first.what,
            first.event,
            layer.result
        ));
    }
    // What only the layer's replay could not do.
    for i in &layer.incomplete {
        if !system.incomplete.contains(i) {
            v.failures += 1;
            v.lines.push(format!("LAYER COULD NOT: {i}"));
        }
    }
    let mut kinds = BTreeMap::<&str, usize>::new();
    for c in &system.checks {
        *kinds.entry(c.kind.as_str()).or_default() += 1;
    }
    let kinds: Vec<String> = kinds.iter().map(|(k, n)| format!("{n} {k}")).collect();
    v.ends = match (&system.environment, &layer.environment) {
        (Some(s), Some(l)) => format!("system: {}; layer: {}", s.line(), l.line()),
        _ => "an end made no device".into(),
    };
    v.summary = format!(
        "{} comparisons ({}): {agree} the game's in both replays, {} the game's on the system's runtime only, \
         {only_layer_passes} through the layer only, {} differ in both ({both_same} with the same bytes); {} \
         findings about the layer ({})",
        system.checks.len(),
        kinds.join(", "),
        v.lines.iter().filter(|l| l.starts_with("LAYER DIFFERS")).count(),
        both_same + both_other,
        v.failures,
        v.ends
    );
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(key: &str, identical: bool, replay: &str) -> Checked {
        Checked {
            key: key.into(),
            kind: "target".into(),
            event: 1,
            what: key.into(),
            identical,
            detail: if identical {
                "identical".into()
            } else {
                "64 of 64 texels differ".into()
            },
            replay: replay.into(),
        }
    }

    #[test]
    fn only_the_layers_own_differences_are_findings() {
        let system = Report {
            checks: vec![
                check("a", true, "1"),
                check("b", true, "2"),
                check("c", false, "3"),
                check("d", false, "4"),
                check("e", false, "5"),
                check("f", true, "6"),
            ],
            incomplete: vec!["capture gap: x".into()],
            ..Default::default()
        };
        let layer = Report {
            checks: vec![
                check("a", true, "1"),
                check("b", false, "9"),
                check("c", false, "3"),
                check("d", false, "8"),
                check("e", true, "5"),
            ],
            incomplete: vec![
                "capture gap: x".into(),
                "replay: command signature #1 could not be made".into(),
            ],
            result: "event 7: CreateCommandSignature failed".into(),
            ..Default::default()
        };
        let v = compare(&system, &layer);
        // b differs through the layer only; f was never reached; the signature only the layer could not make.
        assert_eq!(v.failures, 3, "{v:#?}");
        assert!(v.lines[0].starts_with("LAYER DIFFERS b"), "{v:#?}");
        assert!(v.lines.iter().any(|l| l.contains("with the same bytes")));
        assert!(v.lines.iter().any(|l| l.contains("and from each other")));
        assert!(
            v.lines
                .iter()
                .any(|l| l.starts_with("note: e") && l.contains("through the layer only"))
        );
        assert!(
            v.lines
                .iter()
                .any(|l| l.starts_with("LAYER STOPPED: 1 comparisons") && l.contains("event 7"))
        );
        assert!(
            v.lines
                .iter()
                .any(|l| l == "LAYER COULD NOT: replay: command signature #1 could not be made")
        );
        assert!(v.summary.starts_with(
            "6 comparisons (6 target): 1 the game's in both replays, 1 the game's on the system's runtime only, 1 \
             through the layer only, 2 differ in both (1 with the same bytes); 3 findings"
        ), "{}", v.summary);
        // The same replay twice: nothing to say of the layer.
        assert_eq!(compare(&system, &system).failures, 0);
    }
}
