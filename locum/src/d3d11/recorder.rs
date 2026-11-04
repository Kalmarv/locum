//! Deferred lists recorded from their start: every call written to a tape (see [`tape`]), replayed when a captured
//! frame executes the list.
mod state;
mod tape;
use core::ffi::c_void;
pub use tape::{Reader, Tape};
use windows::core::{IUnknown, Interface};

/// An object an upload names, with a reference held.
#[derive(Clone)]
pub struct Object(Option<IUnknown>);
impl Object {
    /// # Safety
    /// A non-null pointer is a live COM interface supplied to the recorded call.
    pub unsafe fn copy(raw: *mut c_void) -> Self {
        Self(unsafe { IUnknown::from_raw_borrowed(&raw) }.cloned())
    }
    pub fn raw(&self) -> *mut c_void {
        self.0.as_ref().map_or(core::ptr::null_mut(), Interface::as_raw)
    }
}

use crate::vtables::recorded_context::tag;
use std::sync::{Arc, Mutex};
use windows::Win32::Graphics::Direct3D11::*;
mod private;
mod uploads;
mod watch;
use uploads::Mapped;

#[derive(Default)]
pub struct Recorder {
    current: Mutex<Current>,
}
#[derive(Default)]
struct Current {
    initial: Option<Tape>,
    inherited: bool,
    commands: Tape,
    /// The stream-output targets the list set last, which a list inheriting its state starts with.
    so: Option<Tape>,
    gaps: Vec<&'static str>,
    maps: std::collections::HashMap<(usize, u32), Mapped>,
    shadows: std::collections::HashMap<(usize, u32), watch::Shadow>,
}
/// A finished list: the state it inherited, as calls, and its own calls. Plans are immutable; the caller serializes
/// access to replay's context.
pub struct Plan {
    pub frame: u64,
    initial: Tape,
    commands: Tape,
    pub gaps: Vec<&'static str>,
}
impl Recorder {
    /// # Safety
    /// Context is the live deferred context owned by this recorder; its caller serializes calls.
    pub unsafe fn begin(&self, context: *mut c_void) {
        let mut s = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if s.initial.is_none() {
            s.initial = Some(if s.inherited {
                unsafe { state::snapshot(context, s.so.as_ref()) }
            } else {
                Tape::default()
            });
            if s.inherited && s.so.is_some() {
                s.gaps.push("inherited stream-output cursor is not observable");
            }
        }
    }
    /// Records the call `write` writes to a tape, after the real context made it.
    pub fn record(&self, write: impl Fn(&mut Tape)) {
        let mut s = self.current.lock().unwrap_or_else(|e| e.into_inner());
        write(&mut s.commands);
        match s.commands.last() {
            Some(tag::SOSetTargets) => {
                let mut so = Tape::default();
                write(&mut so);
                s.so = Some(so);
            }
            Some(tag::ClearState) => s.so = None,
            Some(tag::ExecuteCommandList) => s
                .gaps
                .push("nested command-list readbacks cannot be reconstructed on this runtime"),
            _ => {}
        }
    }
    pub fn gap(&self, reason: &'static str) {
        let mut s = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if !s.gaps.contains(&reason) {
            s.gaps.push(reason);
        }
    }
    /// # Safety
    /// Called after successful FinishCommandList with its result (possibly null).
    pub unsafe fn finish(&self, list: *mut c_void, restore: bool) {
        let plan = {
            let mut s = self.current.lock().unwrap_or_else(|e| e.into_inner());
            if !s.maps.is_empty() {
                s.gaps.push("FinishCommandList with an outstanding map");
            }
            // Every list must start each resource's maps with DISCARD. Active maps own their
            // allocation separately, so an invalid outstanding map never receives a dangling pointer.
            s.shadows.clear();
            let plan = Plan {
                frame: crate::dxgi::frame(),
                initial: s.initial.take().unwrap_or_default(),
                commands: {
                    // The next list is likely as long as this one.
                    let next = Tape::sized_like(&s.commands);
                    std::mem::replace(&mut s.commands, next)
                },
                gaps: std::mem::take(&mut s.gaps),
            };
            s.inherited = restore;
            if !restore {
                s.so = None;
            }
            Arc::new(plan)
        };
        unsafe { private::attach(list, plan) };
    }
}
impl Plan {
    /// # Safety
    /// The context is an empty private deferred context with recursive recording disabled.
    pub unsafe fn replay(&self, context: *mut c_void) -> windows::core::Result<()> {
        unsafe { self.initial.replay(context) }?;
        unsafe { self.commands.replay(context) }
    }
}
/// # Safety
/// `list` is the application's live command list.
pub unsafe fn plan(list: *mut c_void) -> Option<Arc<Plan>> {
    unsafe { private::get(list) }
}
