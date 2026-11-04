//! Native tiled APIs and capture bootstrap/seed events.
use super::*;
use serde_json::json;
fn n(v: &Value) -> u32 {
    v.as_u64().unwrap_or(0) as u32
}
fn coordinate(v: &Value) -> D3D11_TILED_RESOURCE_COORDINATE {
    D3D11_TILED_RESOURCE_COORDINATE {
        X: n(&v[0]),
        Y: n(&v[1]),
        Z: n(&v[2]),
        Subresource: n(&v[3]),
    }
}
fn size(v: &Value) -> D3D11_TILE_REGION_SIZE {
    D3D11_TILE_REGION_SIZE {
        NumTiles: n(&v[0]),
        bUseBox: v[1].as_bool().unwrap_or(false).into(),
        Width: n(&v[2]),
        Height: n(&v[3]) as u16,
        Depth: n(&v[4]) as u16,
    }
}
fn array<T>(v: &Value, f: impl Fn(&Value) -> T) -> Option<Vec<T>> {
    v.as_array().map(|v| v.iter().map(f).collect())
}
fn pointer<T>(a: &Option<Vec<T>>) -> Option<*const T> {
    a.as_ref().map(|a| a.as_ptr())
}
pub(super) fn run(
    b: &mut D3d11,
    c: &Capture,
    kind: &str,
    a: &Value,
    data: Option<&str>,
) -> Result<(), String> {
    let ctx: ID3D11DeviceContext2 = b.ctx.cast().map_err(|e| e.to_string())?;
    let resource = |name: &str| b.resource(a[name].as_i64().unwrap_or(0));
    let pool = |name: &str| -> Result<Option<ID3D11Buffer>, String> {
        let id = a[name].as_i64().unwrap_or(0);
        if id == 0 { Ok(None) } else { b.buffer(id).map(Some) }
    };
    unsafe {
        match kind {
            "tile_bootstrap" => {
                let dst = resource("dst")?;
                let l = locum_d3d11::tiles::Layout::query(dst)?;
                let actual = json!({"total": l.total, "mips": l.mips, "array": l.array, "packed": l.packed, "shape": l.shape, "subs": l.subs});
                if a["layout"] != actual {
                    return Err(format!(
                        "device tiling differs from captured layout: recorded {}, device {}",
                        a["layout"], actual
                    ));
                }
                let pool = pool("pool")?;
                let tiles = a["tiles"].as_array().ok_or("no initial mappings")?;
                let mut starts = Vec::new();
                let mut offsets = Vec::new();
                for pair in tiles {
                    let at = l.coordinate(n(&pair[0]))?;
                    starts.push(coordinate(&json!(at)));
                    offsets.push(n(&pair[1]));
                }
                if !starts.is_empty() {
                    let counts = vec![1; starts.len()];
                    ctx.UpdateTileMappings(
                        dst,
                        starts.len() as u32,
                        Some(starts.as_ptr()),
                        None,
                        pool.as_ref(),
                        starts.len() as u32,
                        None,
                        Some(offsets.as_ptr()),
                        Some(counts.as_ptr()),
                        0,
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            "tile_seed" => {
                let dst = resource("dst")?;
                let bytes = c.blob(data.ok_or("tile seed bytes missing")?)?;
                let coordinates = a["coordinates"]
                    .as_array()
                    .ok_or("tile seed coordinates missing")?;
                if coordinates.len().checked_mul(65536) != Some(bytes.len()) {
                    return Err("tile seed byte count differs".into());
                }
                let size = D3D11_TILE_REGION_SIZE {
                    NumTiles: 1,
                    ..Default::default()
                };
                for (i, at) in coordinates.iter().enumerate() {
                    ctx.UpdateTiles(dst, &coordinate(at), &size, bytes[i * 65536..].as_ptr().cast(), 0);
                }
                ctx.TiledResourceBarrier(dst, dst);
            }
            "tile_packed_seed" => {
                let dst = resource("dst")?;
                let sub = n(&a["sub"]);
                let layout = locum_d3d11::tiles::Layout::query(dst)?;
                if !layout.is_packed(sub) {
                    return Err("packed mip seed does not address a packed mip on this device".into());
                }
                let subs = locum_d3d11::readback::subs(dst)?;
                let sub_layout = subs
                    .get(sub as usize)
                    .ok_or("packed mip seed subresource out of range")?;
                let bytes = c.blob(data.ok_or("packed mip seed bytes missing")?)?;
                if u64::try_from(bytes.len()).ok() != Some(sub_layout.size) {
                    return Err("packed mip seed byte count differs".into());
                }
                let depth_pitch = u32::try_from(sub_layout.slice_bytes())
                    .map_err(|_| "packed mip seed depth pitch overflow")?;
                ctx.UpdateSubresource(
                    dst,
                    sub,
                    None,
                    bytes.as_ptr().cast(),
                    sub_layout.row_bytes,
                    depth_pitch,
                );
                ctx.TiledResourceBarrier(dst, dst);
            }
            "tile_map" => {
                let starts = array(&a["starts"], coordinate);
                let sizes = array(&a["sizes"], size);
                let flags = array(&a["range_flags"], n);
                let offsets = array(&a["offsets"], n);
                // Multiple null range counts mean one tile per range. WARP crashes on the null pointer;
                // spell out the documented defaults while retaining the original arguments in the capture.
                let counts = array(&a["counts"], n)
                    .or_else(|| (n(&a["ranges"]) > 1).then(|| vec![1; n(&a["ranges"]) as usize]));
                ctx.UpdateTileMappings(
                    resource("dst")?,
                    n(&a["regions"]),
                    pointer(&starts),
                    pointer(&sizes),
                    pool("pool")?.as_ref(),
                    n(&a["ranges"]),
                    pointer(&flags),
                    pointer(&offsets),
                    pointer(&counts),
                    n(&a["flags"]),
                )
                .map_err(|e| e.to_string())?;
            }
            "tile_copy_map" => {
                ctx.CopyTileMappings(
                    resource("dst")?,
                    &coordinate(&a["dst_at"]),
                    resource("src")?,
                    &coordinate(&a["src_at"]),
                    &size(&a["size"]),
                    n(&a["flags"]),
                )
                .map_err(|e| e.to_string())?;
            }
            "tile_resize" => {
                ctx.ResizeTilePool(
                    pool("pool")?.as_ref(),
                    a["bytes"].as_u64().ok_or("pool size missing")?,
                )
                .map_err(|e| e.to_string())?;
            }
            "tile_copy" => {
                ctx.CopyTiles(
                    resource("resource")?,
                    &coordinate(&a["at"]),
                    &size(&a["size"]),
                    &b.buffer(a["buffer"].as_i64().unwrap_or(0))?,
                    a["offset"].as_u64().unwrap_or(0),
                    n(&a["flags"]),
                );
            }
            "tile_update" => {
                let bytes = c.blob(data.ok_or("UpdateTiles bytes missing")?)?;
                let size = size(&a["size"]);
                let tiles = if size.bUseBox.as_bool() {
                    u64::from(size.Width) * u64::from(size.Height) * u64::from(size.Depth)
                } else {
                    u64::from(size.NumTiles)
                };
                if tiles.checked_mul(65536) != Some(bytes.len() as u64) {
                    return Err("UpdateTiles byte count differs".into());
                }
                ctx.UpdateTiles(
                    resource("dst")?,
                    &coordinate(&a["at"]),
                    &size,
                    bytes.as_ptr().cast(),
                    n(&a["flags"]),
                );
            }
            "tile_barrier" => {
                let before = a["before"]
                    .as_i64()
                    .filter(|&id| id != 0)
                    .map(|id| b.resource(id))
                    .transpose()?;
                let after = a["after"]
                    .as_i64()
                    .filter(|&id| id != 0)
                    .map(|id| b.resource(id))
                    .transpose()?;
                ctx.TiledResourceBarrier(before.map(|r| &**r), after.map(|r| &**r));
            }
            _ => return Err(format!("unknown tiled event {kind}")),
        }
    }
    Ok(())
}
