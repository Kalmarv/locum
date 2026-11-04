//! Each texture's state (or layout) per subresource as the GPU finds it when the next submission begins: from its
//! creation, then every submitted list's barriers in submission order. A copy made before an application's lists
//! transitions from the state a texture is truly in to COPY_SOURCE and back.
//!
//! Buffers and simultaneous-access textures are not tracked: D3D12 decays them to COMMON whenever an
//! ExecuteCommandLists completes (and creates buffers in COMMON whatever state is asked for), so a submission always
//! begins with them in COMMON, from which a copy promotes them implicitly. A non-simultaneous texture in COMMON is
//! promoted implicitly too; a promotion to a read state decays, one to COPY_DEST (a copy into it) persists.
//!
//! Each list's first barrier on a subresource names the state the application expects it in. One that differs from
//! the tracked state (other than from COMMON, which a list may have promoted) is counted as a mismatch, and the
//! application's word is taken: the count says how far the tracking can be trusted.
use std::collections::HashMap;

use locum_capture::d3d12 as model;
use serde_json::{Value, json};

/// A subresource's state: a legacy resource state, or an enhanced-barrier layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    State(i32),
    Layout(i32),
}

impl Access {
    pub fn value(self) -> Value {
        match self {
            Access::State(s) => json!({"state":s}),
            Access::Layout(l) => json!({"layout":l}),
        }
    }
}

pub const COMMON: i32 = 0;
pub const COPY_DEST: i32 = 0x400;
pub const COPY_SOURCE: i32 = 0x800;
/// D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY: the first half of a split barrier, completed by an END_ONLY one.
const BEGIN_ONLY: u64 = 1;
const ALL_SUBRESOURCES: u64 = 0xFFFF_FFFF;
/// D3D12_BARRIER_LAYOUT_UNDEFINED: a barrier from it discards, whatever the layout was.
pub const UNDEFINED: i32 = -1;
const COPY_QUEUE: i32 = 3;

struct Texture {
    subs: Vec<Access>,
    mips: u32,
    array: u32,
}

#[derive(Default)]
pub struct Tracker {
    textures: HashMap<u64, Texture>,
    /// Barriers applied, and those whose before-state differed from the tracked one.
    pub transitions: u64,
    pub mismatches: u64,
}

impl Tracker {
    /// A resource made in `initial` (a state, or a layout from the enhanced-barrier creators), described as
    /// `journal::describe` records it, with `planes` planes per subresource.
    pub fn create(&mut self, id: u64, d: &Value, initial: Access, planes: u32) {
        let n = |key: &str| d[key].as_u64().unwrap_or(0) as u32;
        // D3D12_RESOURCE_FLAG_ALLOW_SIMULTANEOUS_ACCESS.
        if n("dimension") <= 1 || n("flags") & 0x20 != 0 {
            return;
        }
        let mips = n("mips").max(1);
        let array = if n("dimension") == 4 {
            1
        } else {
            n("depth_or_array_size").max(1)
        };
        let count = (mips * array * planes.max(1)) as usize;
        self.textures.insert(
            id,
            Texture {
                subs: vec![initial; count],
                mips,
                array,
            },
        );
    }

    pub fn destroy(&mut self, id: u64) {
        self.textures.remove(&id);
    }

    pub fn count(&self) -> usize {
        self.textures.len()
    }

    /// Whether a resource's states are tracked (a non-simultaneous texture made by a creator locum saw).
    pub fn tracked(&self, id: u64) -> bool {
        self.textures.contains_key(&id)
    }

    /// Each subresource's state, in D3D12 subresource order (`mip + slice * mips + plane * mips * slices`).
    pub fn states(&self, id: u64) -> Option<&[Access]> {
        self.textures.get(&id).map(|t| t.subs.as_slice())
    }

    /// Applies a submission's lists, in order, on a queue of `queue_type`.
    pub fn execute(&mut self, lists: &[std::sync::Arc<model::List>], queue_type: i32) {
        let mut touched = std::collections::HashSet::new();
        for list in lists {
            let mut seen = std::collections::HashSet::new();
            for c in &list.commands {
                self.command(c, &mut seen, &mut touched);
            }
        }
        // Everything a copy queue's lists touched decays to COMMON when their ExecuteCommandLists completes.
        if queue_type == COPY_QUEUE {
            for id in touched {
                if let Some(t) = self.textures.get_mut(&id) {
                    for s in &mut t.subs {
                        if matches!(s, Access::State(_)) {
                            *s = Access::State(COMMON);
                        }
                    }
                }
            }
        }
    }

    /// One command's effect. `seen` holds the (resource, subresource) pairs this list has already barriered, so only
    /// a list's first barrier on each is checked against the tracked state; `touched`, the textures it changed.
    fn command(
        &mut self,
        c: &model::Command,
        seen: &mut std::collections::HashSet<(u64, u32)>,
        touched: &mut std::collections::HashSet<u64>,
    ) {
        let a = &c.args;
        match c.call {
            "ResourceBarrier" => {
                for b in a[0].as_array().into_iter().flatten() {
                    // Transitions only; the second half of a split barrier is where its state takes effect.
                    if b["type"] != 0 || b["flags"].as_u64().unwrap_or(0) & BEGIN_ONLY != 0 {
                        continue;
                    }
                    let p = &b["payload"];
                    let (Some(id), Some(sub), Some(before), Some(after)) = (
                        p["resource"].as_u64(),
                        p["subresource"].as_u64(),
                        p["before"].as_i64(),
                        p["after"].as_i64(),
                    ) else {
                        continue;
                    };
                    let Some(t) = self.textures.get_mut(&id) else {
                        continue;
                    };
                    touched.insert(id);
                    let range: Vec<u32> = if sub == ALL_SUBRESOURCES {
                        (0..t.subs.len() as u32).collect()
                    } else {
                        vec![sub as u32]
                    };
                    for i in range {
                        let Some(s) = t.subs.get_mut(i as usize) else {
                            continue;
                        };
                        self.transitions += 1;
                        let before = Access::State(before as i32);
                        if seen.insert((id, i)) && *s != before && *s != Access::State(COMMON) {
                            self.mismatches += 1;
                        }
                        *s = Access::State(after as i32);
                    }
                }
            }
            "Barrier" => {
                // Texture barrier groups (D3D12_BARRIER_TYPE_TEXTURE).
                for g in a[0].as_array().into_iter().flatten().filter(|g| g["type"] == 1) {
                    for b in g["barriers"].as_array().into_iter().flatten() {
                        let Some(id) = b["resource"].as_u64() else {
                            continue;
                        };
                        let (Some(before), Some(after)) = (b["layout"][0].as_i64(), b["layout"][1].as_i64())
                        else {
                            continue;
                        };
                        let Some(t) = self.textures.get_mut(&id) else {
                            continue;
                        };
                        touched.insert(id);
                        for i in subresources(&b["subresources"], t) {
                            let Some(s) = t.subs.get_mut(i as usize) else {
                                continue;
                            };
                            self.transitions += 1;
                            let before = before as i32;
                            if seen.insert((id, i))
                                && before != UNDEFINED
                                && matches!(*s, Access::Layout(l) if l != before)
                            {
                                self.mismatches += 1;
                            }
                            *s = Access::Layout(after as i32);
                        }
                    }
                }
            }
            // Copies into a texture in COMMON promote it to COPY_DEST, which persists.
            "CopyTextureRegion" => {
                if let (Some(id), Some(sub)) = (a[0]["resource"].as_u64(), a[0]["subresource"].as_u64()) {
                    self.promote(id, Some(sub as u32));
                    touched.insert(id);
                }
            }
            "CopyResource" => {
                if let Some(id) = a[0].as_u64() {
                    self.promote(id, None);
                    touched.insert(id);
                }
            }
            _ => {}
        }
    }

    fn promote(&mut self, id: u64, sub: Option<u32>) {
        let Some(t) = self.textures.get_mut(&id) else {
            return;
        };
        for (i, s) in t.subs.iter_mut().enumerate() {
            if sub.is_none_or(|sub| sub as usize == i) && *s == Access::State(COMMON) {
                *s = Access::State(COPY_DEST);
            }
        }
    }
}

/// An enhanced barrier's subresource range as indices: `mips` 0 means `index_or_first_mip` is one subresource index
/// (all of them at 0xFFFFFFFF); otherwise the range of mips, slices and planes.
fn subresources(r: &Value, t: &Texture) -> Vec<u32> {
    let n = |key: &str| r[key].as_u64().unwrap_or(0) as u32;
    let first = r["index_or_first_mip"].as_u64().unwrap_or(0);
    if n("mips") == 0 {
        return if first == ALL_SUBRESOURCES {
            (0..t.subs.len() as u32).collect()
        } else {
            vec![first as u32]
        };
    }
    let mut out = Vec::new();
    for plane in n("first_plane")..n("first_plane") + n("planes").max(1) {
        for slice in n("first_slice")..n("first_slice") + n("slices").max(1) {
            for mip in first as u32..first as u32 + n("mips") {
                out.push(mip + slice * t.mips + plane * t.mips * t.array);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn list(commands: Vec<(&'static str, Value)>) -> Arc<model::List> {
        Arc::new(model::List {
            commands: commands
                .into_iter()
                .map(|(call, args)| model::Command {
                    call,
                    args,
                    bundle: None,
                    diagnostic: None,
                })
                .collect(),
            ..model::List::default()
        })
    }

    fn transition(resource: u64, sub: u64, before: i32, after: i32) -> Value {
        json!({"type":0,"flags":0,"payload":{"resource":resource,"subresource":sub,"before":before,"after":after}})
    }

    #[test]
    fn transitions_promotions_and_mismatches() {
        let mut t = Tracker::default();
        // A 2-mip, 2-slice texture, a simultaneous-access texture and a buffer.
        t.create(
            1,
            &json!({"dimension":3,"mips":2,"depth_or_array_size":2,"flags":0}),
            Access::State(0x80),
            1,
        );
        t.create(
            2,
            &json!({"dimension":3,"mips":1,"depth_or_array_size":1,"flags":0x20}),
            Access::State(0),
            1,
        );
        t.create(3, &json!({"dimension":1,"width":64}), Access::State(0), 1);
        assert!(t.tracked(1) && !t.tracked(2) && !t.tracked(3));
        t.execute(
            &[list(vec![(
                "ResourceBarrier",
                json!([[transition(1, 3, 0x80, 0x4), transition(1, 0, 0x80, 0x8)]]),
            )])],
            0,
        );
        assert_eq!(
            t.states(1).unwrap(),
            [
                Access::State(0x8),
                Access::State(0x80),
                Access::State(0x80),
                Access::State(0x4)
            ]
        );
        assert_eq!(t.mismatches, 0);
        // A split barrier's begin is no change; the application's before-state wins over a wrong tracked one.
        let split =
            json!({"type":0,"flags":1,"payload":{"resource":1,"subresource":1,"before":0x80,"after":0x4}});
        t.execute(
            &[list(vec![(
                "ResourceBarrier",
                json!([[split, transition(1, 2, 0x40, 0x800)]]),
            )])],
            0,
        );
        assert_eq!(t.states(1).unwrap()[1], Access::State(0x80));
        assert_eq!(t.states(1).unwrap()[2], Access::State(0x800));
        assert_eq!(t.mismatches, 1);
        // All subresources at once, then a copy into a COMMON texture promotes it to COPY_DEST.
        t.execute(
            &[list(vec![
                (
                    "ResourceBarrier",
                    json!([[transition(1, ALL_SUBRESOURCES, 0x80, 0)]]),
                ),
                (
                    "CopyTextureRegion",
                    json!([{"resource":1,"subresource":3},0,0,0,{"resource":9},null]),
                ),
            ])],
            0,
        );
        assert_eq!(
            t.states(1).unwrap(),
            [
                Access::State(0),
                Access::State(0),
                Access::State(0),
                Access::State(COPY_DEST)
            ]
        );
        // Mismatches: subresources 0 (UNORDERED_ACCESS), 2 (COPY_SOURCE) and 3 (RENDER_TARGET) were not in
        // PIXEL_SHADER_RESOURCE.
        assert_eq!(t.mismatches, 4);
    }

    #[test]
    fn copy_queues_decay_and_layout_ranges() {
        let mut t = Tracker::default();
        t.create(
            1,
            &json!({"dimension":3,"mips":3,"depth_or_array_size":2,"flags":0}),
            Access::Layout(0),
            2,
        );
        assert_eq!(t.states(1).unwrap().len(), 12);
        let barrier = |range: Value, before: i32, after: i32| json!([[{"type":1,"barriers":[{"resource":1,"layout":[before,after],"subresources":range}]}]]);
        // Mip 1, slice 1, plane 1: index 1 + 1*3 + 1*3*2 = 10.
        let range =
            json!({"index_or_first_mip":1,"mips":1,"first_slice":1,"slices":1,"first_plane":1,"planes":1});
        t.execute(&[list(vec![("Barrier", barrier(range, 0, 7))])], 0);
        let states = t.states(1).unwrap();
        assert_eq!(states[10], Access::Layout(7));
        assert_eq!(states.iter().filter(|s| **s == Access::Layout(0)).count(), 11);
        t.execute(
            &[list(vec![(
                "Barrier",
                barrier(
                    json!({"index_or_first_mip":ALL_SUBRESOURCES,"mips":0}),
                    UNDEFINED,
                    3,
                ),
            )])],
            0,
        );
        assert!(t.states(1).unwrap().iter().all(|s| *s == Access::Layout(3)));
        assert_eq!(t.mismatches, 0);
        // Legacy states on a copy queue decay; layouts do not.
        t.create(
            2,
            &json!({"dimension":3,"mips":1,"depth_or_array_size":1,"flags":0}),
            Access::State(0),
            1,
        );
        t.execute(
            &[list(vec![(
                "ResourceBarrier",
                json!([[transition(2, 0, 0, 0x400)]]),
            )])],
            COPY_QUEUE,
        );
        assert_eq!(t.states(2).unwrap(), [Access::State(0)]);
    }
}
