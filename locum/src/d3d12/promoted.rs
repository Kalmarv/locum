//! Buffers locum's copies read by implicit promotion. A copy inside an application's list (an acceleration
//! structure build's inputs, an ExecuteIndirect's argument records, a DispatchRays' shader tables) reads a buffer
//! the list's own barriers have not moved as it is: a buffer begins each ExecuteCommandLists in COMMON, and the
//! list's uses of it promote it to read states, which the copy's COPY_SOURCE joins. The application does not know
//! of that COPY_SOURCE, so where it next moves the buffer it names the before-state without it, which the runtime
//! rejects. Locum moves it back first:
//!
//! - inside the list, right before the application's next transition of it: from that transition's before-state
//!   with COPY_SOURCE to the before-state;
//! - where the list leaves it so (no transition of its own after the copy), in a list of locum's submitted before
//!   the next list of the same ExecuteCommandLists that moves it, likewise: buffers do not decay to COMMON between
//!   the lists of one call.
//!
//! A buffer an earlier list of the same call left in a state of its own (by a transition) is not promoted: the copy
//! needs COPY_SOURCE joined to that state, which a list of locum's submitted before the copying list does (and the
//! copying list's later transition from that state, or the next list's, is settled as above).
//!
//! An upload buffer is always in GENERIC_READ, which holds COPY_SOURCE: it needs none of this.
use core::ffi::c_void;
use std::collections::{HashMap, HashSet};
use std::mem::ManuallyDrop;

use locum_capture::d3d12 as model;
use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::states::{COMMON, COPY_SOURCE};

/// D3D12_RESOURCE_BARRIER_FLAG_END_ONLY: the second half of a split barrier, whose first half already moved it.
const END_ONLY: u64 = 2;
/// D3D12_RESOURCE_BARRIER_FLAG_BEGIN_ONLY.
const BEGIN_ONLY: u64 = 1;

/// A transition of a buffer locum makes: `resource`, `subresource`, from `before` to `after`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    pub resource: u64,
    pub subresource: u32,
    pub before: i32,
    pub after: i32,
}

/// The transitions (resource, subresource, before, after) of recorded ResourceBarrier arguments `a`, the second
/// halves of split barriers left out.
fn transitions(a: &Value) -> impl Iterator<Item = (u64, u32, i32, i32, bool)> + '_ {
    a[0].as_array().into_iter().flatten().filter_map(|b| {
        let p = &b["payload"];
        let flags = b["flags"].as_u64().unwrap_or(0);
        (b["type"] == 0 && flags & END_ONLY == 0).then(|| {
            (
                p["resource"].as_u64().unwrap_or(0),
                p["subresource"].as_u64().unwrap_or(0) as u32,
                p["before"].as_i64().unwrap_or(0) as i32,
                p["after"].as_i64().unwrap_or(0) as i32,
                flags & BEGIN_ONLY != 0,
            )
        })
    })
}

/// What locum moves right before ResourceBarrier `a` is recorded into a list recorded as `recording`: each promoted
/// buffer the barrier first moves, from its before-state with COPY_SOURCE to it. Each is settled.
pub fn settling(recording: &mut model::Recording, a: &Value) -> Vec<Move> {
    let mut out = Vec::new();
    for (resource, subresource, before, _, _) in transitions(a) {
        if recording.settle(resource) && before & COPY_SOURCE == 0 {
            out.push(Move {
                resource,
                subresource,
                before: before | COPY_SOURCE,
                after: before,
            });
        }
    }
    out
}

/// Records `moves` into `list`.
///
/// # Safety
/// `list` is a real list open for recording; the moved resources are live.
pub unsafe fn record(list: &ID3D12GraphicsCommandList, moves: &[Move]) {
    // Held while the barriers borrow them.
    let held: Vec<(super::Real<ID3D12Resource>, &Move)> = moves
        .iter()
        .filter_map(|m| super::real::<ID3D12Resource>(m.resource).map(|r| (r, m)))
        .collect();
    let barriers: Vec<D3D12_RESOURCE_BARRIER> = held
        .iter()
        .map(|(r, m)| D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: super::passes::borrowed(r),
                    Subresource: m.subresource,
                    StateBefore: D3D12_RESOURCE_STATES(m.before),
                    StateAfter: D3D12_RESOURCE_STATES(m.after),
                }),
            },
        })
        .collect();
    if !barriers.is_empty() {
        unsafe { list.ResourceBarrier(&barriers) };
    }
}

/// Before ResourceBarrier `a` is forwarded to the real list `real` (recorded as `recording`): see the module's
/// documentation.
///
/// # Safety
/// `real` is the application's real list, open for recording.
pub unsafe fn settle(recording: &mut model::Recording, real: *mut c_void, a: &Value) {
    if recording.promoted().iter().all(|p| p.settled) {
        return;
    }
    let moves = settling(recording, a);
    if let Some(list) = unsafe { ID3D12GraphicsCommandList::from_raw_borrowed(&real) } {
        unsafe { record(list, &moves) };
    }
}

/// What locum moves between the lists of one ExecuteCommandLists (`lists`, each with its place in the
/// application's array, in order), by the place of the list it is moved before: see the module's documentation.
pub fn between(lists: &[(usize, &model::List)]) -> Vec<(usize, Vec<Move>)> {
    // The state the call's lists so far left each buffer in by a transition of their own, and the buffers they left
    // promoted with COPY_SOURCE.
    let mut explicit: HashMap<u64, (u32, i32)> = HashMap::new();
    let mut joined: HashSet<u64> = HashSet::new();
    let mut out = Vec::new();
    for &(at, list) in lists {
        let mut moves = Vec::new();
        let promoted: HashSet<u64> = list.promoted.iter().map(|p| p.resource).collect();
        // Copied where an earlier list left it in a state without COPY_SOURCE: joined first.
        for &r in &promoted {
            if let Some(&(subresource, state)) = explicit.get(&r)
                && state != COMMON
                && state & COPY_SOURCE == 0
            {
                moves.push(Move {
                    resource: r,
                    subresource,
                    before: state,
                    after: state | COPY_SOURCE,
                });
                explicit.insert(r, (subresource, state | COPY_SOURCE));
            }
        }
        // Left promoted with COPY_SOURCE by an earlier list, and moved by this one before any copy of its own
        // (which settles it in the list).
        let mut first = HashSet::new();
        for c in list.commands.iter().filter(|c| c.call == "ResourceBarrier") {
            for (r, subresource, before, _, _) in transitions(&c.args) {
                if first.insert(r)
                    && joined.contains(&r)
                    && !promoted.contains(&r)
                    && before & COPY_SOURCE == 0
                {
                    moves.push(Move {
                        resource: r,
                        subresource,
                        before: before | COPY_SOURCE,
                        after: before,
                    });
                }
            }
        }
        if !moves.is_empty() {
            out.push((at, moves));
        }
        // What the list leaves.
        for c in list.commands.iter().filter(|c| c.call == "ResourceBarrier") {
            for (r, subresource, _, after, begin) in transitions(&c.args) {
                joined.remove(&r);
                if begin {
                    explicit.remove(&r);
                } else {
                    explicit.insert(r, (subresource, after));
                }
            }
        }
        for p in list.promoted.iter().filter(|p| !p.settled) {
            joined.insert(p.resource);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NPSR: i32 = 0x40;
    const COPY_DEST: i32 = 0x400;

    fn barrier(resource: u64, before: i32, after: i32) -> model::Command {
        model::Command {
            call: "ResourceBarrier",
            args: json!([[{"type": 0, "flags": 0, "payload":
                {"resource": resource, "subresource": 0xFFFF_FFFFu32, "before": before, "after": after}}]]),
            bundle: None,
            diagnostic: None,
        }
    }

    fn list(id: u64, commands: Vec<model::Command>, promoted: &[(u64, bool)]) -> model::List {
        model::List {
            id,
            commands,
            promoted: promoted
                .iter()
                .map(|&(resource, settled)| model::Promoted { resource, settled })
                .collect(),
            ..model::List::default()
        }
    }

    fn moved(resource: u64, before: i32, after: i32) -> Move {
        Move {
            resource,
            subresource: 0xFFFF_FFFF,
            before,
            after,
        }
    }

    #[test]
    fn a_promoted_buffer_is_moved_back_before_the_lists_next_transition_of_it() {
        let mut r = model::Recording::new(1, 0, 0, 0, false);
        r.promote(5);
        r.promote(5);
        assert_eq!(r.snapshot().promoted.len(), 1);
        // A transition of another buffer settles nothing.
        assert!(settling(&mut r, &barrier(6, NPSR, COPY_DEST).args).is_empty());
        assert_eq!(
            settling(&mut r, &barrier(5, NPSR, COPY_DEST).args),
            vec![moved(5, NPSR | COPY_SOURCE, NPSR)]
        );
        assert!(r.snapshot().promoted[0].settled);
        // Once.
        assert!(settling(&mut r, &barrier(5, COPY_DEST, NPSR).args).is_empty());
        // A before-state that holds COPY_SOURCE already settles it with nothing to move.
        r.promote(7);
        assert!(settling(&mut r, &barrier(7, 0xAC3, COPY_DEST).args).is_empty());
        assert!(r.snapshot().promoted.iter().all(|p| p.settled));
    }

    #[test]
    fn buffers_are_moved_between_the_lists_of_one_call() {
        // 0 moves #1 into NPSR; 1 copies from it (and from #2, from COMMON), leaving both; 2 moves both.
        let lists = [
            list(10, vec![barrier(1, COPY_DEST, NPSR)], &[]),
            list(11, vec![], &[(1, false), (2, false)]),
            list(
                12,
                vec![barrier(1, NPSR, COPY_DEST), barrier(2, NPSR, COPY_DEST)],
                &[],
            ),
        ];
        let at: Vec<(usize, &model::List)> = lists.iter().enumerate().collect();
        assert_eq!(
            between(&at),
            vec![
                (1, vec![moved(1, NPSR, NPSR | COPY_SOURCE)]),
                (
                    2,
                    vec![
                        moved(1, NPSR | COPY_SOURCE, NPSR),
                        moved(2, NPSR | COPY_SOURCE, NPSR)
                    ]
                ),
            ]
        );
    }

    #[test]
    fn what_a_list_settles_itself_is_not_moved_again() {
        // 1 copies from #1 and moves it itself (settled inside it); 2 copies from it again before moving it.
        let lists = [
            list(10, vec![], &[(1, false)]),
            list(11, vec![barrier(1, NPSR, COPY_DEST)], &[(3, true)]),
            list(12, vec![], &[(4, false)]),
            list(13, vec![barrier(4, NPSR, COPY_DEST)], &[(4, true)]),
        ];
        let at: Vec<(usize, &model::List)> = lists.iter().enumerate().collect();
        // #1: 0 left it promoted, 1 moves it first: moved back before 1. #4: 2 left it promoted, and 3 copies from
        // it before moving it, which 3 settles itself.
        assert_eq!(between(&at), vec![(1, vec![moved(1, NPSR | COPY_SOURCE, NPSR)])]);
    }

    #[test]
    fn nothing_moves_where_buffers_begin_the_call_in_common_and_stay_so() {
        let lists = [
            list(10, vec![], &[(1, false)]),
            list(11, vec![barrier(2, COMMON, NPSR)], &[(3, true)]),
        ];
        let at: Vec<(usize, &model::List)> = lists.iter().enumerate().collect();
        assert!(between(&at).is_empty());
    }
}
