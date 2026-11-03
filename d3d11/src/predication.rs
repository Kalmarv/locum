//! locum's own copies on a game's context (contents, constants, targets) must run whatever predicate the game set:
//! copies and resolves are predicated, and one skipped by the game's predicate would leave a capture's copy empty.

use windows::Win32::Graphics::Direct3D11::*;
use windows::core::BOOL;

/// Runs `f` with predication off on `c`, then sets the game's predicate back.
///
/// # Safety
/// `c` must be a live context that nothing else uses meanwhile.
pub unsafe fn unpredicated<R>(c: &ID3D11DeviceContext, f: impl FnOnce() -> R) -> R {
    let mut predicate = None;
    let mut value = BOOL(0);
    unsafe { c.GetPredication(Some(&mut predicate), Some(&mut value)) };
    let Some(predicate) = predicate else {
        return f();
    };
    unsafe { c.SetPredication(None, false) };
    let r = f();
    unsafe { c.SetPredication(&predicate, value.as_bool()) };
    r
}
