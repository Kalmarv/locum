//! Tile-pool aliasing must survive mapping changes and writes through another resource.
use super::*;
use windows::Win32::Graphics::Direct3D::D3D_SRV_DIMENSION_TEXTURE2D;

pub(super) fn run(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
    driver: &str,
) -> Result<()> {
    let mut options = D3D11_FEATURE_DATA_D3D11_OPTIONS1::default();
    unsafe {
        device.CheckFeatureSupport(
            D3D11_FEATURE_D3D11_OPTIONS1,
            (&mut options as *mut D3D11_FEATURE_DATA_D3D11_OPTIONS1).cast(),
            size_of_val(&options) as u32,
        )?;
    }
    println!("tiles: tier {}", options.TiledResourcesTier.0);
    if options.TiledResourcesTier == D3D11_TILED_RESOURCES_NOT_SUPPORTED {
        println!("skip tiles: unsupported");
        return Ok(());
    }
    let label = format!("tiles-{driver}");
    let c2: ID3D11DeviceContext2 = c.cast()?;
    let mut pool = None;
    let bd = D3D11_BUFFER_DESC {
        ByteWidth: 3 * 65536,
        Usage: D3D11_USAGE_DEFAULT,
        MiscFlags: D3D11_RESOURCE_MISC_TILE_POOL.0 as u32,
        ..Default::default()
    };
    unsafe {
        device.CreateBuffer(&bd, None, Some(&mut pool))?;
    }
    let pool = pool.unwrap();
    // The initial pool description must follow resizes made before capture arming.
    unsafe {
        c2.ResizeTilePool(&pool, 5 * 65536)?;
    }
    let td = D3D11_TEXTURE2D_DESC {
        Width: 256,
        Height: 128,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_TILED.0 as u32,
        ..Default::default()
    };
    let (mut a, mut b) = (None, None);
    unsafe {
        device.CreateTexture2D(&td, None, Some(&mut a))?;
        device.CreateTexture2D(&td, None, Some(&mut b))?;
    }
    let (a, b) = (a.unwrap(), b.unwrap());
    let mut srv = None;
    let sd = D3D11_SHADER_RESOURCE_VIEW_DESC {
        Format: td.Format,
        ViewDimension: D3D_SRV_DIMENSION_TEXTURE2D,
        Anonymous: D3D11_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D11_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
            },
        },
    };
    unsafe {
        device.CreateShaderResourceView(&b, Some(&sd), Some(&mut srv))?;
    }
    let zero = D3D11_TILED_RESOURCE_COORDINATE::default();
    let one = D3D11_TILE_REGION_SIZE {
        NumTiles: 1,
        bUseBox: false.into(),
        ..Default::default()
    };
    let second = D3D11_TILED_RESOURCE_COORDINATE { X: 1, ..zero };
    let red = [255u8, 0, 0, 255].repeat(16384);
    let green = [0u8, 255, 0, 255].repeat(16384);
    let blue = [0u8, 0, 255, 255].repeat(16384);
    let mut yellow = [255u8, 255, 0, 255].repeat(16384);
    yellow.extend_from_slice(&green);
    let magenta = [255u8, 0, 255, 255].repeat(16384);
    let mut upload = None;
    let ud = D3D11_BUFFER_DESC {
        ByteWidth: 65536,
        Usage: D3D11_USAGE_DEFAULT,
        ..Default::default()
    };
    let initial = D3D11_SUBRESOURCE_DATA {
        pSysMem: magenta.as_ptr().cast(),
        ..Default::default()
    };
    unsafe {
        device.CreateBuffer(&ud, Some(&initial), Some(&mut upload))?;
    }
    let upload = upload.unwrap();
    let mut download = None;
    let dd = D3D11_BUFFER_DESC {
        ByteWidth: 131072,
        Usage: D3D11_USAGE_DEFAULT,
        ..Default::default()
    };
    unsafe {
        device.CreateBuffer(&dd, None, Some(&mut download))?;
    }
    let download = download.unwrap();
    unsafe {
        c.ClearState();
        if driver == "warp" {
            println!(
                "tiles: explicit one-tile counts avoid the independently reproduced WARP null-counts AV"
            );
        }
        c2.UpdateTileMappings(
            &a,
            2,
            Some([zero, second].as_ptr()),
            None,
            &pool,
            2,
            None,
            Some([0, 1].as_ptr()),
            if driver == "warp" {
                Some([1, 1].as_ptr())
            } else {
                None
            },
            0,
        )?;
        c2.UpdateTileMappings(
            &b,
            1,
            Some(&zero),
            Some(&one),
            &pool,
            1,
            None,
            Some(&2),
            Some(&1),
            0,
        )?;
        c2.UpdateTileMappings(
            &b,
            1,
            Some(&second),
            Some(&one),
            &pool,
            1,
            None,
            Some(&1),
            Some(&1),
            0,
        )?;
        c2.UpdateTiles(&a, &zero, &one, red.as_ptr().cast(), 0);
        c2.UpdateTiles(&a, &second, &one, green.as_ptr().cast(), 0);
        c2.UpdateTiles(&b, &zero, &one, blue.as_ptr().cast(), 0);
        c2.TiledResourceBarrier(None::<&ID3D11DeviceChild>, None::<&ID3D11DeviceChild>);
    }
    let sh = Shaders::new(device)?;
    let cb = constant_buffer(device, false)?;
    let (texture, rtv) = target(device)?;
    let mut ps = None;
    unsafe {
        device.CreatePixelShader(&compile("Texture2D<float4> t:register(t0); float4 main(float4 p:SV_Position):SV_Target {return t.Load(int3(uint2(p.xy)*2,0));}",s!("ps_5_0"))?,None,Some(&mut ps))?;
    }
    let draw = || unsafe {
        sh.bind(c, &rtv, &cb);
        c.PSSetShader(ps.as_ref(), None);
        c.PSSetShaderResources(0, Some(&[srv.clone()]));
        c.Draw(3, 0);
        c.OMSetRenderTargets(None, None);
        c.PSSetShaderResources(0, Some(&[None]));
    };
    let dir = replay_rows::begin(swap, &label)?;
    draw();
    unsafe {
        c2.CopyTileMappings(&b, &zero, &a, &zero, &one, 0)?;
        c2.TiledResourceBarrier(&a, &b);
    }
    draw();
    unsafe {
        c2.UpdateTiles(
            &a,
            &zero,
            &D3D11_TILE_REGION_SIZE {
                NumTiles: 2,
                bUseBox: true.into(),
                Width: 2,
                Height: 1,
                Depth: 1,
            },
            yellow.as_ptr().cast(),
            0,
        );
        c2.TiledResourceBarrier(&a, &b);
    }
    draw();
    unsafe {
        c2.CopyTiles(
            &a,
            &zero,
            &one,
            &upload,
            0,
            D3D11_TILE_COPY_LINEAR_BUFFER_TO_SWIZZLED_TILED_RESOURCE.0 as u32,
        );
        c2.TiledResourceBarrier(&a, &b);
    }
    draw();
    unsafe {
        c2.CopyTiles(
            &b,
            &zero,
            &one,
            &download,
            65536,
            D3D11_TILE_COPY_SWIZZLED_TILED_RESOURCE_TO_LINEAR_BUFFER.0 as u32,
        );
        c2.ResizeTilePool(&pool, 6 * 65536)?;
        c2.UpdateTileMappings(
            &b,
            2,
            Some([zero, second].as_ptr()),
            None,
            &pool,
            2,
            None,
            Some([2, 1].as_ptr()),
            if driver == "warp" {
                Some([1, 1].as_ptr())
            } else {
                None
            },
            0,
        )?;
        c2.TiledResourceBarrier(None::<&ID3D11DeviceChild>, None::<&ID3D11DeviceChild>);
    }
    draw();
    let mut d = None;
    unsafe {
        device.CreateDeferredContext(0, Some(&mut d))?;
    }
    let d: ID3D11DeviceContext2 = d.unwrap().cast()?;
    unsafe {
        if let Err(e) = d.UpdateTileMappings(
            &b,
            1,
            Some(&zero),
            Some(&one),
            &pool,
            1,
            None,
            Some(&0),
            Some(&1),
            0,
        ) {
            if e.code() != windows::Win32::Foundation::E_INVALIDARG {
                return Err(e);
            }
            println!(
                "skip tiles deferred mapping: runtime returned {e}; testing deferred upload with an immediate mapping"
            );
            c2.UpdateTileMappings(
                &b,
                1,
                Some(&zero),
                Some(&one),
                &pool,
                1,
                None,
                Some(&0),
                Some(&1),
                0,
            )?;
        }
        d.UpdateTiles(&a, &zero, &one, green.as_ptr().cast(), 0);
        d.TiledResourceBarrier(&a, &b);
        let mut list = None;
        d.FinishCommandList(false, Some(&mut list))?;
        c.ExecuteCommandList(list.as_ref().unwrap(), false);
    }
    draw();
    replay_rows::finish(checks, swap, &dir, &label)?;
    checks.check(
        first_pixel(device, c, &texture)? == [0, 255, 0, 255],
        "tiles: deferred write through A changes B to green",
    );
    let mut staging = None;
    let staging_desc = D3D11_BUFFER_DESC {
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..dd
    };
    unsafe {
        device.CreateBuffer(&staging_desc, None, Some(&mut staging))?;
    }
    let staging = staging.unwrap();
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe {
        c.CopyResource(&staging, &download);
        c.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
    }
    let bytes = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>().add(65536), 65536) };
    checks.check(
        bytes == magenta,
        "tiles: CopyTiles to buffer used the byte offset and canonical tile bytes",
    );
    unsafe {
        c.Unmap(&staging, 0);
        c.ClearState();
    }
    let db = rusqlite::Connection::open(dir.join(format!("captures/{label}/capture.db"))).unwrap();
    for (call, count) in [
        ("UpdateTileMappings", 2),
        ("CopyTileMappings", 1),
        ("CopyTiles", 2),
        ("UpdateTiles", 2),
        ("ResizeTilePool", 1),
        ("TiledResourceBarrier", 5),
    ] {
        let n: u32 = db
            .query_row("SELECT count(*) FROM events WHERE call=?1", [call], |r| r.get(0))
            .unwrap();
        checks.check(
            n == count,
            &format!("{label}: recorded {count} successful {call} calls"),
        );
    }
    let blobs = locum_capture::blobs::Blobs::new(dir.join("blobs"));
    let uploads: Vec<String> = db
        .prepare("SELECT data FROM events WHERE call='UpdateTiles' ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    checks.check(
        uploads.len() == 2
            && blobs.get(&uploads[0]).is_ok_and(|v| v == yellow)
            && blobs.get(&uploads[1]).is_ok_and(|v| v == green),
        &format!("{label}: exact boxed/immediate and linear/deferred tile upload bytes"),
    );
    let deferred: u32 = db
        .query_row(
            "SELECT count(*) FROM events WHERE call='UpdateTiles' AND deferred=1 AND list IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    checks.check(
        deferred == 1,
        &format!("{label}: deferred upload appears at list execution"),
    );
    let initial_pool: u32 = db
        .query_row(
            "SELECT width FROM resources WHERE misc & 131072 != 0 AND used=1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    checks.check(
        initial_pool == 5 * 65536,
        "tiles: pool size includes resize before arming",
    );
    let seeds: Vec<String> = db
        .prepare("SELECT data FROM events WHERE kind='tile_seed' ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let seeded: Vec<_> = seeds
        .iter()
        .flat_map(|k| {
            blobs
                .get(k)
                .unwrap()
                .as_chunks::<65536>()
                .0
                .iter()
                .map(|v| v.to_vec())
                .collect::<Vec<_>>()
        })
        .collect();
    checks.check(
        seeded.len() == 3 && seeded.contains(&red) && seeded.contains(&green) && seeded.contains(&blue),
        "tiles: each aliased physical tile seeded once with its initial canonical bytes",
    );
    let gaps: u32 = db
        .query_row("SELECT count(*) FROM gaps WHERE what='tiled'", [], |r| r.get(0))
        .unwrap();
    checks.check(gaps == 0, "tiles: native mapping/pool capture has no gaps");
    let tampered = dir.join(format!("captures/{label}-omit-alias"));
    std::fs::create_dir_all(&tampered).unwrap();
    std::fs::copy(
        dir.join(format!("captures/{label}/capture.db")),
        tampered.join("capture.db"),
    )
    .unwrap();
    let bad = rusqlite::Connection::open(tampered.join("capture.db")).unwrap();
    bad.execute("UPDATE events SET kind='discard' WHERE kind='tile_update'", [])
        .unwrap();
    drop(bad);
    packed(checks, device, c, swap, driver)?;
    Ok(())
}

/// Packed mip groups retain a gap until canonical packed seeding is implemented.
fn packed(
    checks: &mut Checks,
    device: &ID3D11Device,
    c: &ID3D11DeviceContext,
    swap: &IDXGISwapChain1,
    driver: &str,
) -> Result<()> {
    let label = format!("tiles-packed-{driver}");
    let c2: ID3D11DeviceContext2 = c.cast()?;
    let mut texture = None;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: 64,
        Height: 64,
        MipLevels: 7,
        ArraySize: 1,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        MiscFlags: D3D11_RESOURCE_MISC_TILED.0 as u32,
        ..Default::default()
    };
    unsafe {
        device.CreateTexture2D(&desc, None, Some(&mut texture))?;
    }
    let texture = texture.unwrap();
    let d2: ID3D11Device2 = device.cast()?;
    let mut total = 0;
    let mut packed = D3D11_PACKED_MIP_DESC::default();
    unsafe {
        d2.GetResourceTiling(
            &texture,
            Some(&mut total),
            Some(&mut packed),
            None,
            None,
            0,
            std::ptr::null_mut(),
        );
    }
    if packed.NumPackedMips == 0 {
        println!("skip tiles-packed-{driver}: runtime reports no packed mips for 64x64/7 mips");
        return Ok(());
    }
    checks.check(total != 0, "tiles: packed mip fixture has packed tiles");
    let mut pool = None;
    unsafe {
        device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: total * 65536,
                Usage: D3D11_USAGE_DEFAULT,
                MiscFlags: D3D11_RESOURCE_MISC_TILE_POOL.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut pool),
        )?;
    }
    let pool = pool.unwrap();
    unsafe {
        c2.UpdateTileMappings(&texture, 1, None, None, &pool, 1, None, Some(&0), None, 0)?;
    }
    for mip in 0..7 {
        let size = (64u32 >> mip).max(1);
        let data = [17u8, 67, 139, 255].repeat((size * size) as usize);
        unsafe {
            c.UpdateSubresource(&texture, mip, None, data.as_ptr().cast(), size * 4, 0);
        }
    }
    let mut srv = None;
    unsafe {
        device.CreateShaderResourceView(&texture, None, Some(&mut srv))?;
    }
    let sh = Shaders::new(device)?;
    let cb = constant_buffer(device, false)?;
    let (_, rtv) = target(device)?;
    let mut ps = None;
    unsafe {
        device.CreatePixelShader(&compile("Texture2D<float4> t:register(t0); float4 main(float4 p:SV_Position):SV_Target { return t.Load(int3(uint2(p.xy),0)); }", s!("ps_5_0"))?, None, Some(&mut ps))?;
    }
    let dir = replay_rows::begin(swap, &label)?;
    unsafe {
        sh.bind(c, &rtv, &cb);
        c.PSSetShader(ps.as_ref(), None);
        c.PSSetShaderResources(0, Some(&[srv]));
        c.Draw(3, 0);
        c.ClearState();
    }
    replay_rows::finish(checks, swap, &dir, &label)?;
    let db = rusqlite::Connection::open(dir.join(format!("captures/{label}/capture.db"))).unwrap();
    let gaps: u32 = db
        .query_row(
            "SELECT count(*) FROM gaps WHERE what='tiled' AND detail LIKE '%packed%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    checks.check(gaps != 0, "tiles: packed initial contents retain an explicit gap");
    Ok(())
}
