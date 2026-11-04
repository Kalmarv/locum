//! Immutable command arguments retained from the start of a deferred list.
mod state;
use core::ffi::c_void;
use windows::core::{IUnknown, Interface};
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
#[derive(Clone)]
pub struct Objects(Option<Box<[Option<IUnknown>]>>);
impl Objects {
    /// # Safety
    /// The array contains `count` live/null COM interface pointers, or is null.
    pub unsafe fn copy(raw: *const *mut c_void, count: u32) -> Self {
        Self((!raw.is_null()).then(|| {
            if count == 0 {
                Box::default()
            } else {
                unsafe { core::slice::from_raw_parts(raw, count as usize) }
                    .iter()
                    .map(|p| unsafe { IUnknown::from_raw_borrowed(p) }.cloned())
                    .collect()
            }
        }))
    }
    pub fn raw(&self) -> *const *mut c_void {
        self.0.as_ref().map_or(core::ptr::null(), |v| v.as_ptr().cast())
    }
}
#[derive(Clone)]
pub struct Array<T>(Option<Box<[T]>>);
impl<T: Copy> Array<T> {
    /// # Safety
    /// The pointer contains `count` input elements, or is null.
    pub unsafe fn copy(raw: *const T, count: u32) -> Self {
        Self((!raw.is_null()).then(|| {
            if count == 0 {
                Box::default()
            } else {
                unsafe { core::slice::from_raw_parts(raw, count as usize) }.into()
            }
        }))
    }
    pub fn raw(&self) -> *const T {
        self.0.as_ref().map_or(core::ptr::null(), |v| v.as_ptr())
    }
}

use crate::vtables::recorded_context::Command;
use std::sync::{Arc, Mutex};
use windows::Win32::Graphics::Direct3D11::*;
mod private;
mod uploads;
mod watch;
use uploads::{Mapped, Upload};

#[derive(Default)]
pub struct Recorder {
    current: Mutex<Current>,
}
#[derive(Default)]
struct Current {
    initial: Option<Vec<Command>>,
    inherited: bool,
    commands: Vec<Recorded>,
    so: Option<Command>,
    gaps: Vec<&'static str>,
    maps: std::collections::HashMap<(usize, u32), Mapped>,
    shadows: std::collections::HashMap<(usize, u32), watch::Shadow>,
}
pub struct Plan {
    pub frame: u64,
    initial: Vec<Command>,
    commands: Vec<Recorded>,
    pub gaps: Vec<&'static str>,
}
// Plans are immutable. Their COM pointers are D3D11 device-child interfaces, whose references may
// move from a recording thread to an execution thread. The caller serializes access to replay's context.
unsafe impl Send for Plan {}
unsafe impl Sync for Plan {}
enum Recorded {
    Call(Command),
    Upload(Box<Upload>),
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
                Vec::new()
            });
            if s.inherited && s.so.is_some() {
                s.gaps.push("inherited stream-output cursor is not observable");
            }
        }
    }
    pub fn push(&self, command: Command) {
        let mut s = self.current.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(command, Command::SOSetTargets(_)) {
            s.so = Some(command.clone());
        }
        if matches!(command, Command::ClearState(_)) {
            s.so = None;
        }
        if matches!(command, Command::ExecuteCommandList(_)) {
            s.gaps
                .push("nested command-list readbacks cannot be reconstructed on this runtime");
        }
        s.commands.push(Recorded::Call(command));
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
                commands: std::mem::take(&mut s.commands),
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
        for c in &self.initial {
            unsafe { c.replay(context) }?;
        }
        for c in &self.commands {
            match c {
                Recorded::Call(c) => unsafe { c.replay(context) }?,
                Recorded::Upload(u) => unsafe { u.replay(context) }?,
            }
        }
        Ok(())
    }
}
/// # Safety
/// `list` is the application's live command list.
pub unsafe fn plan(list: *mut c_void) -> Option<Arc<Plan>> {
    unsafe { private::get(list) }
}
