//! `diff`: two captures, draw by draw. Draws are matched by an identity that survives a relaunch of the game (resource
//! numbers do not): their shaders, the call and its counts (not its offsets into shared buffers), what they read by
//! its upload hash (or its shape, for what has none: targets, dynamic buffers), the shapes of their targets, and how
//! many draws with that identity came before. Matched draws are compared constant by constant (named from the
//! shaders), texture by texture and state by state.

use std::collections::{BTreeMap, HashMap};

use locum_capture::hash::KeyHasher;

use crate::describe::{Reflection, float, shape};
use crate::draws::shader_of;
use crate::open::Capture;
use crate::out;
use crate::states;

/// A draw's identity, readable.
fn identity(c: &Capture, n: i64) -> String {
    let (call, args, shaders): (String, String, Vec<Option<String>>) = c
        .row(
            "SELECT call, args, vs, hs, ds, gs, ps, cs FROM draws WHERE n = ?1",
            [n],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    (2..8).map(|i| r.get(i)).collect::<rusqlite::Result<_>>()?,
                ))
            },
        )
        .unwrap_or_default();
    let args: Vec<i64> = serde_json::from_str(&args).unwrap_or_default();
    // The counts, not the offsets: vertices or indices, instances, thread groups.
    let counts: Vec<i64> = match call.as_str() {
        "Draw" | "DrawIndexed" => args.iter().take(1).copied().collect(),
        "DrawInstanced" | "DrawIndexedInstanced" => args.iter().take(2).copied().collect(),
        "Dispatch" => args.clone(),
        _ => Vec::new(),
    };
    let mut parts = vec![format!("{call} {counts:?}")];
    for (s, h) in ["vs", "hs", "ds", "gs", "ps", "cs"].iter().zip(&shaders) {
        if let Some(h) = h {
            parts.push(format!("{s}={h}"));
        }
    }
    type Bound = (String, String, i64, Option<i64>, Option<String>, bool);
    let bound: Vec<Bound> = c.rows(
        "SELECT b.stage, b.kind, b.slot, b.resource, r.upload, coalesce(r.written, 0) FROM bindings b \
         LEFT JOIN resources r ON r.id = b.resource WHERE b.draw = ?1 AND b.kind IN ('srv', 'vb', 'ib', 'rtv', 'dsv') \
         ORDER BY b.stage, b.kind, b.slot",
        [n],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
    );
    for (stage, kind, slot, resource, upload, written) in bound {
        let what = match (upload, written, kind.as_str()) {
            (Some(u), false, "srv" | "vb" | "ib") => u,
            _ => resource
                .and_then(|r| shape(c, r))
                .map(|s| {
                    let text = s.to_string();
                    text.split_once(' ')
                        .map_or(text.clone(), |(_, rest)| rest.to_string())
                })
                .unwrap_or_default(),
        };
        parts.push(format!("{stage}.{kind}{slot}={what}"));
    }
    parts.join(" ")
}

/// Every draw keyed by its identity and how many draws with that identity came before it.
fn keyed(c: &Capture) -> Vec<(String, i64)> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    c.rows("SELECT n FROM draws ORDER BY n", [], |r| r.get::<_, i64>(0))
        .into_iter()
        .map(|n| {
            let id = identity(c, n);
            let mut h = KeyHasher::new();
            h.update(id.as_bytes());
            let k = seen.entry(id).or_default();
            *k += 1;
            (format!("{}#{}", h.finish(), *k - 1), n)
        })
        .collect()
}

/// What differs between draw `a` of `ca` and draw `b` of `cb`, a line each, keyed by what it is about.
fn differences(ca: &Capture, a: i64, cb: &Capture, b: i64, ra: &mut Reflection) -> Vec<(String, String)> {
    let mut out = Vec::new();
    // Constant buffers, register by register.
    type Cbs = BTreeMap<(String, i64), (Option<Vec<u8>>, i64)>;
    let cbs = |c: &Capture, n: i64| -> Cbs {
        c.rows(
            "SELECT b.stage, b.slot, d.bytes, b.space FROM bindings b LEFT JOIN cb_data d ON d.hash = b.content \
             WHERE b.draw = ?1 AND b.kind = 'cb'",
            [n],
            |r| Ok(((r.get(0)?, r.get(1)?), (r.get(2)?, r.get(3)?))),
        )
        .into_iter()
        .collect()
    };
    let (xa, xb) = (cbs(ca, a), cbs(cb, b));
    for (k, (bytes_a, space)) in &xa {
        let Some((bytes_b, _)) = xb.get(k) else {
            out.push((format!("{} b{}", k.0, k.1), "bound only in the first".into()));
            continue;
        };
        let (Some(p), Some(q)) = (bytes_a, bytes_b) else {
            continue;
        };
        if p == q {
            continue;
        }
        let shader = shader_of(ca, a, &k.0);
        let refl = shader
            .as_deref()
            .and_then(|s| ra.cb(ca, s, k.1 as u32, *space as u32));
        for (i, (x, y)) in p
            .as_chunks::<4>()
            .0
            .iter()
            .zip(q.as_chunks::<4>().0.iter())
            .enumerate()
        {
            if x == y {
                continue;
            }
            let at = i as u32 * 4;
            let name = refl
                .and_then(|r| r.name_at(at))
                .unwrap_or_else(|| format!("c{}.{}", at / 16, ["x", "y", "z", "w"][(at % 16 / 4) as usize]));
            out.push((
                format!("{} b{} {name}", k.0, k.1),
                format!(
                    "{} -> {}",
                    float(f32::from_le_bytes(*x)),
                    float(f32::from_le_bytes(*y))
                ),
            ));
        }
        if p.len() != q.len() {
            out.push((
                format!("{} b{}", k.0, k.1),
                format!("{} bytes -> {} bytes", p.len(), q.len()),
            ));
        }
    }
    // Textures and buffers read, by their contents as the frame began.
    let inputs = |c: &Capture, n: i64| -> BTreeMap<(String, String, i64), Option<String>> {
        c.rows(
            "SELECT b.stage, b.kind, b.slot, r.content FROM bindings b LEFT JOIN resources r ON r.id = b.resource \
             WHERE b.draw = ?1 AND b.kind IN ('srv', 'vb', 'ib') AND coalesce(r.written, 0) = 0",
            [n],
            |r| Ok(((r.get(0)?, r.get(1)?, r.get(2)?), r.get(3)?)),
        )
        .into_iter()
        .collect()
    };
    let (ia, ib) = (inputs(ca, a), inputs(cb, b));
    for (k, x) in &ia {
        if let Some(y) = ib.get(k)
            && x != y
        {
            out.push((
                format!("{} {}{}", k.0, k.1, k.2),
                format!(
                    "contents {} -> {}",
                    x.as_deref().unwrap_or("-"),
                    y.as_deref().unwrap_or("-")
                ),
            ));
        }
    }
    // Fixed-function state, kind by kind: different when the descriptions differ (shown with their enums named); a
    // kind one draw has no state object for is the API's default.
    let state = |c: &Capture, n: i64| -> BTreeMap<String, String> {
        c.rows(
            "SELECT s.kind, s.json FROM draws d JOIN states s ON s.id IN (d.rasterizer, d.blend, d.depth_stencil)              WHERE d.n = ?1",
            [n],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default())),
        )
        .into_iter()
        .collect()
    };
    let (sa, sb) = (state(ca, a), state(cb, b));
    let kinds: std::collections::BTreeSet<&String> = sa.keys().chain(sb.keys()).collect();
    for kind in kinds {
        let (x, y) = (sa.get(kind), sb.get(kind));
        if x != y {
            let text = |j: Option<&String>| j.map_or("default".into(), |j| states::describe(kind, Some(j)));
            out.push((kind.clone(), format!("{} -> {}", text(x), text(y))));
        }
    }
    let viewport = |c: &Capture, n: i64| -> Vec<String> {
        c.rows(
            "SELECT x || ',' || y || ' ' || width || 'x' || height FROM viewports WHERE draw = ?1 ORDER BY i",
            [n],
            |r| r.get(0),
        )
    };
    let (va, vb) = (viewport(ca, a), viewport(cb, b));
    if va != vb {
        out.push((
            "viewport".into(),
            format!("{} -> {}", va.join("; "), vb.join("; ")),
        ));
    }
    out
}

pub fn diff(ca: &Capture, cb: &Capture, draws: bool, limit: usize) -> Result<(), String> {
    let ka = keyed(ca);
    let kb = keyed(cb);
    let index_b: HashMap<&str, i64> = kb.iter().map(|(k, n)| (k.as_str(), *n)).collect();
    let index_a: HashMap<&str, i64> = ka.iter().map(|(k, n)| (k.as_str(), *n)).collect();
    let only_a: Vec<i64> = ka
        .iter()
        .filter(|(k, _)| !index_b.contains_key(k.as_str()))
        .map(|x| x.1)
        .collect();
    let only_b: Vec<i64> = kb
        .iter()
        .filter(|(k, _)| !index_a.contains_key(k.as_str()))
        .map(|x| x.1)
        .collect();
    let pairs: Vec<(i64, i64)> = ka
        .iter()
        .filter_map(|(k, a)| index_b.get(k.as_str()).map(|b| (*a, *b)))
        .collect();
    out!(
        "{} draws in the first, {} in the second: {} matched, {} only in the first, {} only in the second",
        ka.len(),
        kb.len(),
        pairs.len(),
        only_a.len(),
        only_b.len()
    );
    let mut reflection = Reflection::default();
    // What differs, and in which draws.
    let mut by_what: BTreeMap<String, (usize, (i64, i64, String))> = BTreeMap::new();
    let mut changed = 0;
    for &(a, b) in &pairs {
        let d = differences(ca, a, cb, b, &mut reflection);
        if d.is_empty() {
            continue;
        }
        changed += 1;
        if draws {
            out!(
                "draw {a} -> {b}: {}",
                identity(ca, a).split(' ').take(3).collect::<Vec<_>>().join(" ")
            );
            for (what, how) in d.iter().take(limit) {
                out!("    {what}: {how}");
            }
            if d.len() > limit {
                out!("    ... {} more", d.len() - limit);
            }
        }
        for (what, how) in d {
            let e = by_what.entry(what).or_insert((0, (a, b, how)));
            e.0 += 1;
        }
    }
    out!("{changed} matched draws differ");
    if !by_what.is_empty() {
        out!("what differs, in how many draws (with the first):");
        let mut rows: Vec<_> = by_what.into_iter().collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.1.0));
        for (what, (count, (a, b, how))) in rows.iter().take(limit) {
            out!("  {count:>6}  {what}: {how} (draw {a} -> {b})");
        }
        if rows.len() > limit {
            out!("  ... {} more (--limit)", rows.len() - limit);
        }
    }
    let list = |label: &str, c: &Capture, ns: &[i64]| {
        if ns.is_empty() {
            return;
        }
        out!("only in the {label}:");
        for n in ns.iter().take(limit) {
            out!(
                "  draw {n}: {}",
                identity(c, *n).split(' ').take(4).collect::<Vec<_>>().join(" ")
            );
        }
        if ns.len() > limit {
            out!("  ... {} more", ns.len() - limit);
        }
    };
    list("first", ca, &only_a);
    list("second", cb, &only_b);
    Ok(())
}
