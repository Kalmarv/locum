//! Pass selection for capture-side completeness (`complete-pass <selector>` in `settings.txt` or `capture` request).
//!
//! Matching submissions capture without silent budget drops: exact bindless descriptor versions, referenced buffer
//! ranges (upload and default/placed/reserved buffers), resident tile bytes/mappings (including post-remap tiles),
//! ExecuteIndirect argument/count buffers, and producer queue/fence state.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use locum_capture::d3d12 as model;
use serde_json::Value;

/// Active capture-time flag: copy ExecuteIndirect argument/count records even when a signature holds no GPU addresses.
static CAPTURE_INDIRECT_ARGS: AtomicBool = AtomicBool::new(false);
/// Per-output / per-indirect-record readback cap while a capture is armed (default 64 MiB; raised for complete-pass).
static OUTPUT_CAP: AtomicU64 = AtomicU64::new(64 << 20);
/// Aggregate output readback budget while a capture is armed (default 2 GiB; raised for complete-pass).
static OUTPUT_BUDGET_CAP: AtomicU64 = AtomicU64::new(2 << 30);

pub fn capture_indirect_args() -> bool {
    CAPTURE_INDIRECT_ARGS.load(Ordering::Acquire)
}

pub fn output_cap() -> u64 {
    OUTPUT_CAP.load(Ordering::Acquire)
}

pub fn output_budget() -> u64 {
    OUTPUT_BUDGET_CAP.load(Ordering::Acquire)
}

/// Updates the lock-free capture flags when a frame is armed or closed.
pub fn configure(indirect_args: bool, complete_active: bool, complete_output_bytes: u64) {
    CAPTURE_INDIRECT_ARGS.store(indirect_args || complete_active, Ordering::Release);
    if complete_active {
        let cap = complete_output_bytes.max(64 << 20);
        OUTPUT_CAP.store(cap, Ordering::Release);
        OUTPUT_BUDGET_CAP.store(cap.saturating_mul(4).max(2 << 30), Ordering::Release);
    } else {
        OUTPUT_CAP.store(64 << 20, Ordering::Release);
        OUTPUT_BUDGET_CAP.store(2 << 30, Ordering::Release);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PassSelector {
    All,
    Shader(String),
    Pso(u64),
    Resource(u64),
    DrawRange(u64, u64),
    PassRange(u64, u64),
    Marker(String),
}

impl PassSelector {
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        let Some((key, val)) = spec.split_once('=') else {
            return Err(format!(
                "expected `all` or key=value (shader, pso, resource, target, draw, pass, marker), got {spec:?}"
            ));
        };
        let key = key.trim().to_ascii_lowercase();
        let val = val.trim();
        if val.is_empty() {
            return Err(format!("missing value in {spec:?}"));
        }
        match key.as_str() {
            "shader" => {
                let hex = val.to_ascii_lowercase();
                if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(format!("shader={val:?} is not hex"));
                }
                Ok(Self::Shader(hex))
            }
            "pso" => val
                .parse::<u64>()
                .map(Self::Pso)
                .map_err(|_| format!("pso={val:?} is not an integer")),
            "resource" | "target" => val
                .trim_start_matches('#')
                .parse::<u64>()
                .map(Self::Resource)
                .map_err(|_| format!("{key}={val:?} is not a resource id")),
            "draw" => parse_range(val)
                .map(|(a, b)| Self::DrawRange(a, b))
                .map_err(|e| format!("draw={val:?}: {e}")),
            "pass" => parse_range(val)
                .map(|(a, b)| Self::PassRange(a, b))
                .map_err(|e| format!("pass={val:?}: {e}")),
            "marker" => Ok(Self::Marker(val.to_ascii_lowercase())),
            _ => Err(format!("unknown selector key {key:?} in {spec:?}")),
        }
    }
}

fn parse_range(s: &str) -> Result<(u64, u64), String> {
    if let Some((a, b)) = s.split_once("..=") {
        let a = a.trim().parse::<u64>().map_err(|_| "invalid start")?;
        let b = b.trim().parse::<u64>().map_err(|_| "invalid end")?;
        Ok((a, b))
    } else if let Some((a, b)) = s.split_once("..") {
        let a = a.trim().parse::<u64>().map_err(|_| "invalid start")?;
        let b = b.trim().parse::<u64>().map_err(|_| "invalid end")?;
        Ok((a, b))
    } else {
        let n = s.trim().parse::<u64>().map_err(|_| "invalid number")?;
        Ok((n, n))
    }
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    pub selectors: Vec<PassSelector>,
}

impl Selection {
    pub fn from_specs(specs: &[String]) -> (Self, Vec<String>) {
        let mut selectors = Vec::new();
        let mut errors = Vec::new();
        for raw in specs {
            for token in raw.split_whitespace() {
                match PassSelector::parse(token) {
                    Ok(sel) => selectors.push(sel),
                    Err(e) => errors.push(e),
                }
            }
        }
        (Self { selectors }, errors)
    }

    pub fn is_empty(&self) -> bool {
        self.selectors.is_empty()
    }

    /// Resource ids explicitly pinned by `resource=<id>` / `target=<id>` selectors.
    pub fn explicit_resources(&self) -> Vec<u64> {
        let mut out = BTreeSet::new();
        for sel in &self.selectors {
            if let PassSelector::Resource(id) = *sel {
                out.insert(id);
            }
        }
        out.into_iter().collect()
    }

    /// Whether resource `id` is explicitly pinned by a selector.
    pub fn pins_resource(&self, id: u64) -> bool {
        self.selectors
            .iter()
            .any(|s| matches!(s, PassSelector::Resource(r) if *r == id))
    }

    /// Checks whether a submission (with its `lists`, newly added draw numbers `draw_start..=draw_end`, pass count
    /// `pass_start..=pass_end`, and `touched` resources) matches any selector; returns a concise reason string.
    pub fn matches_submission(
        &self,
        lists: &[Arc<model::List>],
        objects: &BTreeMap<u64, model::Object>,
        touched: &[u64],
        draws: Option<(u64, u64)>,
        passes: Option<(u64, u64)>,
    ) -> Option<String> {
        if self.selectors.is_empty() {
            return None;
        }
        for sel in &self.selectors {
            match sel {
                PassSelector::All => return Some("all".into()),
                PassSelector::Resource(id) => {
                    if touched.contains(id) {
                        return Some(format!("resource=#{id}"));
                    }
                }
                PassSelector::DrawRange(lo, hi) => {
                    if let Some((a, b)) = draws
                        && a <= *hi
                        && b >= *lo
                    {
                        return Some(format!("draw={lo}..={hi}"));
                    }
                }
                PassSelector::PassRange(lo, hi) => {
                    if let Some((a, b)) = passes
                        && a <= *hi
                        && b >= *lo
                    {
                        return Some(format!("pass={lo}..={hi}"));
                    }
                }
                PassSelector::Pso(target_pso) => {
                    if lists_use_pso(lists, *target_pso) {
                        return Some(format!("pso={target_pso}"));
                    }
                }
                PassSelector::Shader(prefix) => {
                    if lists_use_shader(lists, objects, prefix) {
                        return Some(format!("shader={prefix}"));
                    }
                }
                PassSelector::Marker(needle) => {
                    if lists_have_marker(lists, needle) {
                        return Some(format!("marker={needle}"));
                    }
                }
            }
        }
        None
    }
}

fn walk_lists(lists: &[Arc<model::List>], f: &mut impl FnMut(&model::List) -> bool) -> bool {
    for l in lists {
        if f(l) {
            return true;
        }
        for c in &l.commands {
            if let Some(b) = &c.bundle
                && f(b)
            {
                return true;
            }
        }
    }
    false
}

fn lists_use_pso(lists: &[Arc<model::List>], target: u64) -> bool {
    walk_lists(lists, &mut |l| {
        l.initial_pso == target
            || l.commands.iter().any(|c| {
                matches!(c.call, "SetPipelineState" | "SetPipelineState1" | "ClearState")
                    && c.args[0].as_u64() == Some(target)
            })
    })
}

fn pso_has_shader(objects: &BTreeMap<u64, model::Object>, pso_id: u64, prefix: &str) -> bool {
    let Some(obj) = objects.get(&pso_id) else {
        return false;
    };
    obj.description["shaders"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s["blob"].as_str())
        .any(|h| h.to_ascii_lowercase().starts_with(prefix))
}

fn lists_use_shader(
    lists: &[Arc<model::List>],
    objects: &BTreeMap<u64, model::Object>,
    prefix: &str,
) -> bool {
    walk_lists(lists, &mut |l| {
        if l.initial_pso != 0 && pso_has_shader(objects, l.initial_pso, prefix) {
            return true;
        }
        l.commands.iter().any(|c| {
            matches!(c.call, "SetPipelineState" | "ClearState")
                && c.args[0]
                    .as_u64()
                    .is_some_and(|id| pso_has_shader(objects, id, prefix))
        })
    })
}

/// Decodes a PIX/D3D12 SetMarker/BeginEvent byte payload (UTF-16LE or UTF-8) into a lowercase string.
fn decode_marker(args: &Value) -> Option<String> {
    let bytes: Vec<u8> = args[1]
        .as_array()?
        .iter()
        .filter_map(|v| v.as_u64().map(|n| n as u8))
        .collect();
    if bytes.is_empty() {
        return None;
    }
    // UTF-16LE when even length and every odd byte of the first ASCII chars is 0.
    if bytes.len() >= 2 && bytes.len().is_multiple_of(2) && bytes[1] == 0 {
        let u16s: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .take_while(|&w| w != 0)
            .collect();
        return Some(String::from_utf16_lossy(&u16s).to_ascii_lowercase());
    }
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Some(String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase())
}

fn lists_have_marker(lists: &[Arc<model::List>], needle: &str) -> bool {
    walk_lists(lists, &mut |l| {
        l.commands.iter().any(|c| {
            matches!(c.call, "SetMarker" | "BeginEvent")
                && decode_marker(&c.args).is_some_and(|text| text.contains(needle))
        })
    })
}

/// Collects shader-visible descriptor heap ids bound by `SetDescriptorHeaps` across `lists`.
pub fn bound_descriptor_heaps(lists: &[Arc<model::List>]) -> Vec<u64> {
    let mut heaps = BTreeSet::new();
    let _ = walk_lists(lists, &mut |l| {
        for c in &l.commands {
            if c.call == "SetDescriptorHeaps" {
                for id in c.args[0]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_u64)
                {
                    heaps.insert(id);
                }
            }
        }
        false
    });
    heaps.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_all_selector_forms_and_rejects_invalid() {
        let (sel, errs) = Selection::from_specs(&[
            "all shader=0C29154C pso=11873 resource=#18 target=10130 draw=1590..1596 pass=438 marker=Sky"
                .into(),
        ]);
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(
            sel.selectors,
            vec![
                PassSelector::All,
                PassSelector::Shader("0c29154c".into()),
                PassSelector::Pso(11873),
                PassSelector::Resource(18),
                PassSelector::Resource(10130),
                PassSelector::DrawRange(1590, 1596),
                PassSelector::PassRange(438, 438),
                PassSelector::Marker("sky".into()),
            ]
        );
        assert_eq!(sel.explicit_resources(), vec![18, 10130]);
        assert!(PassSelector::parse("shader=zzzz").is_err());
        assert!(PassSelector::parse("bogus=1").is_err());
        assert!(PassSelector::parse("resource=").is_err());
    }

    #[test]
    fn matches_shader_pso_marker_resource_and_draw_ranges() {
        let mut objects = BTreeMap::new();
        objects.insert(
            11873,
            model::Object {
                id: 11873,
                kind: "ID3D12PipelineState",
                device: 1,
                description: json!({
                    "shaders": [
                        {"stage": "vs", "blob": "b587a7de45db628db5c0ed83c9e40384"},
                        null, null, null,
                        {"stage": "ps", "blob": "0c29154cc96988643f81d4e865e840dd"},
                        null
                    ]
                }),
            },
        );
        let mut rec = model::Recording::new(1, 0, 2, 0, false);
        rec.record(model::Command {
            call: "SetDescriptorHeaps",
            args: json!([[72, 726]]),
            bundle: None,
            diagnostic: None,
        });
        rec.record(model::Command {
            call: "SetPipelineState",
            args: json!([11873]),
            bundle: None,
            diagnostic: None,
        });
        // UTF-16LE "xcl sky"
        let utf16: Vec<u8> = "xcl sky\0".encode_utf16().flat_map(u16::to_le_bytes).collect();
        rec.record(model::Command {
            call: "SetMarker",
            args: json!([0, utf16]),
            bundle: None,
            diagnostic: None,
        });
        let lists = vec![rec.snapshot()];
        assert_eq!(bound_descriptor_heaps(&lists), vec![72, 726]);

        let (by_shader, _) = Selection::from_specs(&["shader=0c29154c".into()]);
        assert_eq!(
            by_shader
                .matches_submission(&lists, &objects, &[], None, None)
                .as_deref(),
            Some("shader=0c29154c")
        );
        let (by_marker, _) = Selection::from_specs(&["marker=sky".into()]);
        assert_eq!(
            by_marker
                .matches_submission(&lists, &objects, &[], None, None)
                .as_deref(),
            Some("marker=sky")
        );
        let (by_res, _) = Selection::from_specs(&["resource=18".into()]);
        assert!(
            by_res
                .matches_submission(&lists, &objects, &[9, 24], None, None)
                .is_none()
        );
        assert_eq!(
            by_res
                .matches_submission(&lists, &objects, &[9, 18], None, None)
                .as_deref(),
            Some("resource=#18")
        );
        let (by_draw, _) = Selection::from_specs(&["draw=1590..1596".into()]);
        assert_eq!(
            by_draw
                .matches_submission(&lists, &objects, &[], Some((1580, 1595)), None)
                .as_deref(),
            Some("draw=1590..=1596")
        );
    }
}
