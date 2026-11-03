//! Captures must say when a live rule changed or skipped the recorded draw.
use super::*;

pub(super) fn run(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    deferred: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
) -> Result<()> {
    let dir = std::path::PathBuf::from(std::env::var_os("LOCUM_DIR").unwrap());
    let sh = Shaders::new(device)?;
    let cb = constant_buffer(device, false)?;
    let (_texture, rtv) = target(device)?;
    // A matching replacement can produce the same pixels. Verification must still report the changed execution.
    unsafe {
        c.UpdateSubresource(&cb, 0, None, [0.0f32; 4].as_ptr().cast(), 0, 0);
    }
    for (label, rule, matched, on_deferred) in [
        ("live-skip", format!("skip ps={}\n", sh.ps_hash), true, false),
        (
            "live-solid",
            format!("solid ps={} 0 0 0 0\n", sh.ps_hash),
            true,
            false,
        ),
        ("live-constant", "constant ps b3 c0 0 0 0 0\n".into(), true, false),
        ("live-deferred", format!("skip ps={}\n", sh.ps_hash), true, true),
        (
            "live-unexecuted",
            format!("skip ps={}\n", sh.ps_hash),
            false,
            true,
        ),
        (
            "live-unmatched",
            "skip ps=0000000000000000\n".into(),
            false,
            false,
        ),
    ] {
        unsafe {
            c.ClearState();
            deferred.ClearState();
            c.ClearRenderTargetView(&rtv, &[0.0; 4]);
        }
        set_overrides(&dir, swap, Some(&rule))?;
        replay_rows::begin(swap, label)?;
        let ctx = if on_deferred { deferred } else { c };
        unsafe {
            sh.bind(ctx, &rtv, &cb);
            ctx.Draw(3, 0);
            ctx.OMSetRenderTargets(None, None);
            if on_deferred {
                let mut list = None;
                deferred.FinishCommandList(false, Some(&mut list))?;
                if label == "live-unexecuted" {
                    c.ClearRenderTargetView(&rtv, &[0.0; 4]);
                } else {
                    c.ExecuteCommandList(list.as_ref().unwrap(), false);
                }
            }
        }
        replay_rows::finish(checks, swap, &dir, label)?;
        let db = rusqlite::Connection::open(dir.join(format!("captures/{label}/capture.db"))).unwrap();
        let gaps: u32 = db
            .query_row(
                "SELECT count(*) FROM gaps WHERE what='overrides' AND detail LIKE '%event 0%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        checks.check(
            (gaps > 0) == matched,
            &format!("{label}: only an applied live rule marks the capture incomplete"),
        );
    }
    set_overrides(&dir, swap, None)?;
    Ok(())
}
