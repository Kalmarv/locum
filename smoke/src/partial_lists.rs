//! A recorded suffix must not masquerade as a complete deferred command list.
use super::*;

pub(super) fn run(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
) -> Result<()> {
    let sh = Shaders::new(device)?;
    let cb = constant_buffer(device, false)?;
    let (_, rtv) = target(device)?;
    unsafe {
        c.UpdateSubresource(&cb, 0, None, [0.0f32; 4].as_ptr().cast(), 0, 0);
    }
    for label in [
        "list-prefix",
        "list-bindings",
        "list-reset",
        "list-unexecuted",
        "list-armed",
        "list-generation",
    ] {
        let mut d = None;
        unsafe {
            device.CreateDeferredContext(0, Some(&mut d))?;
            c.ClearState();
        }
        let d = d.unwrap();
        if label == "list-generation" {
            let dir = replay_rows::begin(swap, "list-generation-seed")?;
            unsafe {
                d.ClearRenderTargetView(&rtv, &[1.0, 0.0, 0.0, 1.0]);
                c.ClearRenderTargetView(&rtv, &[0.0; 4]);
            }
            replay_rows::finish(checks, swap, &dir, "list-generation-seed")?;
        } else if label == "list-bindings" {
            sh.bind(&d, &rtv, &cb);
        } else if label != "list-armed" {
            unsafe {
                d.ClearRenderTargetView(&rtv, &[1.0, 0.0, 0.0, 1.0]);
            }
        }
        if label == "list-reset" {
            let mut discarded = None;
            unsafe {
                d.FinishCommandList(false, Some(&mut discarded))?;
            }
        }
        let dir = replay_rows::begin_config(swap, label, "", || {
            if label == "list-armed" {
                unsafe {
                    d.ClearRenderTargetView(&rtv, &[1.0, 0.0, 0.0, 1.0]);
                }
            }
            Ok(())
        })?;
        let mut list = None;
        unsafe {
            sh.bind(&d, &rtv, &cb);
            d.Draw(3, 0);
            d.OMSetRenderTargets(None, None);
            d.FinishCommandList(false, Some(&mut list))?;
            if label == "list-unexecuted" {
                sh.bind(c, &rtv, &cb);
                c.Draw(3, 0);
                c.OMSetRenderTargets(None, None);
            } else {
                c.ExecuteCommandList(list.as_ref().unwrap(), false);
            }
        }
        replay_rows::finish(checks, swap, &dir, label)?;
        let db = rusqlite::Connection::open(dir.join(format!("captures/{label}/capture.db"))).unwrap();
        let gaps: u32 = db
            .query_row("SELECT count(*) FROM gaps WHERE what='partial_list'", [], |r| {
                r.get(0)
            })
            .unwrap();
        checks.check(
            (gaps > 0) == matches!(label, "list-prefix" | "list-generation"),
            &format!("{label}: only an executed partial list marks the capture incomplete"),
        );
    }
    unsafe {
        c.ClearState();
    }
    Ok(())
}
