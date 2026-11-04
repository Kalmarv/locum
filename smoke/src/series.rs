//! A series (`frames N`): consecutive frames, each a capture of its own, with nothing between them. A command list
//! recorded in the first frame and executed in the second is whole in the second's capture, with the constants the
//! second frame gave it; a buffer the first frame wrote begins the second with those bytes.
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
    let written = contents_buffer(device, 16, false, 0)?;
    let mut d = None;
    unsafe {
        device.CreateDeferredContext(0, Some(&mut d))?;
        c.ClearState();
        c.UpdateSubresource(&cb, 0, None, [0.0f32; 4].as_ptr().cast(), 0, 0);
    }
    let d = d.unwrap();
    let dir = replay_rows::begin_config(swap, "series", "frames 2", || Ok(()))?;
    // The first frame: a list recorded and not executed, and a buffer written.
    let mut list = None;
    unsafe {
        sh.bind(&d, &rtv, &cb);
        d.Draw(3, 0);
        d.OMSetRenderTargets(None, None);
        d.FinishCommandList(false, Some(&mut list))?;
        c.UpdateSubresource(&written, 0, None, [1.0f32; 4].as_ptr().cast(), 0, 0);
        swap.Present(0, DXGI_PRESENT(0)).ok()?;
    }
    // The second: the list executed, with constants this frame wrote; the buffer written again.
    unsafe {
        c.UpdateSubresource(&cb, 0, None, [2.0f32; 4].as_ptr().cast(), 0, 0);
        c.ExecuteCommandList(list.as_ref().unwrap(), false);
        c.UpdateSubresource(&written, 0, None, [3.0f32; 4].as_ptr().cast(), 0, 0);
    }
    replay_rows::finish(checks, swap, &dir, "series-2")?;
    let first = dir.join("captures/series-1/capture.db");
    checks.check(wait_for(&first), "series-1: capture written");
    let open = |p: &std::path::Path| rusqlite::Connection::open(p).unwrap();
    let (one, two) = (open(&first), open(&dir.join("captures/series-2/capture.db")));
    let series = |db: &rusqlite::Connection| -> String {
        db.query_row("SELECT value FROM meta WHERE key = 'series'", [], |r| r.get(0))
            .unwrap_or_default()
    };
    checks.check(
        series(&one).contains("\"index\":1") && series(&two).contains("\"index\":2"),
        "series: each capture says its place in the series",
    );
    let frame = |db: &rusqlite::Connection| -> i64 {
        db.query_row("SELECT value FROM meta WHERE key = 'frame'", [], |r| {
            r.get::<_, String>(0)
        })
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(-1)
    };
    checks.check(
        frame(&two) == frame(&one) + 1,
        "series: the second capture is the frame after the first",
    );
    let count =
        |db: &rusqlite::Connection, sql: &str| -> i64 { db.query_row(sql, [], |r| r.get(0)).unwrap_or(-1) };
    checks.check(
        count(&one, "SELECT count(*) FROM draws") == 0 && count(&two, "SELECT count(*) FROM draws") == 1,
        "series: the list recorded in the first frame is drawn in the second",
    );
    checks.check(
        count(&two, "SELECT count(*) FROM gaps WHERE what = 'partial_list'") == 0,
        "series-2: the list recorded across the frames is whole",
    );
    let constants: Option<Vec<u8>> = two
        .query_row(
            "SELECT c.bytes FROM bindings b JOIN cb_data c ON c.hash = b.content \
             WHERE b.stage = 'ps' AND b.slot = 3 AND b.kind = 'cb'",
            [],
            |r| r.get(0),
        )
        .ok();
    let bytes = |v: f32| -> Vec<u8> { [v; 4].iter().flat_map(|f| f.to_le_bytes()).collect() };
    checks.check(
        constants == Some(bytes(2.0)),
        "series-2: the list's draw has the constants the second frame gave it",
    );
    // The buffer's contents as the second frame began are what the first wrote.
    let begun: Option<i64> = two
        .query_row(
            "SELECT id FROM resources WHERE content = ?1",
            [locum_capture::hash::key(&bytes(1.0))],
            |r| r.get(0),
        )
        .ok();
    let before: Option<String> = begun.and_then(|id| {
        one.query_row("SELECT content FROM resources WHERE id = ?1", [id], |r| r.get(0))
            .ok()
    });
    checks.check(
        begun.is_some() && before == Some(locum_capture::hash::key(&bytes(0.0))),
        "series: a buffer begins the second frame with what the first wrote to it",
    );
    unsafe {
        c.ClearState();
    }
    Ok(())
}
