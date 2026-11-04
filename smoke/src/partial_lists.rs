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
        "list-inherited",
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
        if label == "list-inherited" {
            sh.bind(&d, &rtv, &cb);
            let mut prior = None;
            unsafe {
                d.FinishCommandList(true, Some(&mut prior))?;
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
            if label != "list-inherited" {
                sh.bind(&d, &rtv, &cb);
            }
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
            gaps == 0,
            &format!("{label}: full list is retained across capture arming"),
        );
    }
    unsafe {
        c.ClearState();
    }
    repeated(checks, device, c, swap)?;
    sparse_maps(checks, device, c, swap)?;
    Ok(())
}

/// Same-page appends, a new dirty page, and a second DISCARD in a large allocation.
/// Compare only initialized texels: unwritten DISCARD bytes have no defined value.
fn sparse_maps(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
) -> Result<()> {
    let sh = Shaders::new(device)?;
    let cb = constant_buffer(device, false)?;
    let intermediate = contents_buffer(device, 16, false, 0)?;
    let mut source = None;
    unsafe {
        device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: 4 << 20,
                Usage: D3D11_USAGE_DYNAMIC,
                BindFlags: D3D11_BIND_VERTEX_BUFFER.0 as u32,
                CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut source),
        )
    }?;
    let source = source.unwrap();
    let mut d = None;
    unsafe { device.CreateDeferredContext(0, Some(&mut d)) }?;
    let d = d.unwrap();
    let cases = [
        (
            D3D11_MAP_WRITE_DISCARD,
            0u32,
            [2000.0f32, 0., 0., 2000.],
            [0, 0, 255, 255],
        ),
        (
            D3D11_MAP_WRITE_NO_OVERWRITE,
            64,
            [0., 2000., 0., 2000.],
            [0, 255, 0, 255],
        ),
        (
            D3D11_MAP_WRITE_NO_OVERWRITE,
            4160,
            [0., 0., 2000., 2000.],
            [255, 0, 0, 255],
        ),
        (
            D3D11_MAP_WRITE_DISCARD,
            0,
            [2000., 2000., 0., 2000.],
            [0, 255, 255, 255],
        ),
    ];
    let mut targets = Vec::new();
    for (kind, offset, color, _) in cases {
        let bytes: Vec<_> = color.iter().flat_map(|f| f.to_le_bytes()).collect();
        map_bytes(&d, &source, kind, offset as usize, &bytes)?;
        let (texture, view) = target(device)?;
        let region = D3D11_BOX {
            left: offset,
            right: offset + 16,
            bottom: 1,
            back: 1,
            ..Default::default()
        };
        unsafe {
            d.CopySubresourceRegion(&intermediate, 0, 0, 0, 0, &source, 0, Some(&region));
            d.CopyResource(&cb, &intermediate);
        }
        sh.bind(&d, &view, &cb);
        unsafe {
            d.Draw(3, 0);
            d.OMSetRenderTargets(None, None);
        }
        targets.push(texture);
    }
    let mut list = None;
    unsafe { d.FinishCommandList(false, Some(&mut list)) }?;
    let list = list.unwrap();
    let label = "list-sparse-maps";
    // Both original and reconstructed executions must agree with the initialized source bytes.
    for armed in [false, true] {
        let dir = if armed {
            Some(replay_rows::begin(swap, label)?)
        } else {
            None
        };
        unsafe { c.ExecuteCommandList(&list, false) };
        for (i, texture) in targets.iter().enumerate() {
            let pixel = first_pixel(device, c, texture)?;
            checks.check(
                pixel == cases[i].3,
                &format!(
                    "{label}: armed={armed} target {i} expected {:?}, got {pixel:?}",
                    cases[i].3
                ),
            );
        }
        if let Some(dir) = dir {
            replay_rows::finish(checks, swap, &dir, label)?;
            let db = rusqlite::Connection::open(dir.join(format!("captures/{label}/capture.db"))).unwrap();
            for (i, (_, _, color, _)) in cases.iter().enumerate() {
                let actual: Option<Vec<u8>> = db.query_row(
                    "SELECT c.bytes FROM bindings b JOIN cb_data c ON c.hash=b.content WHERE b.draw=?1 AND b.stage='ps' AND b.slot=3 AND b.kind='cb'",
                    [i as i64], |r| r.get(0)).ok();
                let expected: Vec<_> = color.iter().flat_map(|f| f.to_le_bytes()).collect();
                checks.check(
                    actual == Some(expected),
                    &format!("{label}: draw {i} captures sparse-map constants"),
                );
            }
        }
    }
    Ok(())
}

fn repeated(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
) -> Result<()> {
    let sh = Shaders::new(device)?;
    for (label, dynamic, upload_inside, nested) in [
        ("list-reuse-update", false, false, false),
        ("list-reuse-map", true, false, false),
        ("list-owned-update", false, true, false),
        ("list-owned-map", true, true, false),
        ("list-nested", false, false, true),
    ] {
        let cb = constant_buffer(device, dynamic)?;
        let (texture, view) = target(device)?;
        let mut deferred = None;
        unsafe { device.CreateDeferredContext(0, Some(&mut deferred)) }?;
        let d = deferred.unwrap();
        let colors = [[2000.0f32, 0., 0., 2000.], [0., 2000., 0., 2000.]];
        if upload_inside {
            let bytes: Vec<u8> = colors[0].iter().flat_map(|f| f.to_le_bytes()).collect();
            if dynamic {
                map_bytes(&d, &cb, D3D11_MAP_WRITE_DISCARD, 0, &bytes)?;
            } else {
                unsafe { d.UpdateSubresource(&cb, 0, None, bytes.as_ptr().cast(), 0, 0) };
            }
            // The application's upload storage is gone before arming.
        }
        sh.bind(&d, &view, &cb);
        unsafe {
            d.Draw(3, 0);
            d.OMSetRenderTargets(None, None)
        };
        let mut list = None;
        unsafe { d.FinishCommandList(false, Some(&mut list)) }?;
        let mut list = list.unwrap();
        if nested {
            unsafe { d.ExecuteCommandList(&list, false) };
            let mut parent = None;
            unsafe { d.FinishCommandList(false, Some(&mut parent)) }?;
            list = parent.unwrap();
        }
        let dir = replay_rows::begin(swap, label)?;
        let mut expected = Vec::new();
        for color in colors {
            let bytes: Vec<u8> = color.iter().flat_map(|f| f.to_le_bytes()).collect();
            if dynamic {
                map_bytes(c, &cb, D3D11_MAP_WRITE_DISCARD, 0, &bytes)?;
            } else {
                unsafe { c.UpdateSubresource(&cb, 0, None, bytes.as_ptr().cast(), 0, 0) };
            }
            unsafe { c.ExecuteCommandList(&list, false) };
            expected.push(if upload_inside { colors[0] } else { color });
            let pixel = first_pixel(device, c, &texture)?;
            let want = if upload_inside || color == colors[0] {
                [0, 0, 255, 255]
            } else {
                [0, 255, 0, 255]
            };
            checks.check(
                pixel == want,
                &format!("{label}: actual execution sees {want:?}, got {pixel:?}"),
            );
        }
        drop(list);
        replay_rows::finish(checks, swap, &dir, label)?;
        let db = rusqlite::Connection::open(dir.join(format!("captures/{label}/capture.db"))).unwrap();
        if nested {
            let gaps: u32 = db
                .query_row(
                    "SELECT count(*) FROM gaps WHERE what='deferred_recorder' AND detail LIKE '%nested%'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            checks.check(
                gaps > 0,
                "list-nested: original execution retained with explicit reconstruction gap",
            );
            continue;
        }
        for (i, expected) in expected.iter().enumerate() {
            let bytes:Option<Vec<u8>>=db.query_row("SELECT c.bytes FROM bindings b JOIN cb_data c ON c.hash=b.content WHERE b.draw=?1 AND b.stage='ps' AND b.slot=3 AND b.kind='cb'",[i as i64],|r|r.get(0)).ok();
            let want: Vec<u8> = expected.iter().flat_map(|f| f.to_le_bytes()).collect();
            checks.check(
                bytes == Some(want),
                &format!("{label}: execution {i} retains its own constants"),
            );
        }
    }
    Ok(())
}
