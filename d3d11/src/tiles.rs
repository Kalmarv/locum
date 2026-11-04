//! D3D11 tiled resources: the device's tile numbering of a resource, read for [`locum_capture::tiles`].
pub use locum_capture::tiles::{Layout, Mappings, Range, Region};
use windows::{Win32::Graphics::Direct3D11::*, core::Interface};

/// The device's tile numbering of a tiled resource.
pub fn layout(resource: &ID3D11Resource) -> Result<Layout, String> {
    let (mips, array) = unsafe {
        if let Ok(t) = resource.cast::<ID3D11Texture2D>() {
            let mut d = D3D11_TEXTURE2D_DESC::default();
            t.GetDesc(&mut d);
            (d.MipLevels, d.ArraySize)
        } else if let Ok(t) = resource.cast::<ID3D11Texture3D>() {
            let mut d = D3D11_TEXTURE3D_DESC::default();
            t.GetDesc(&mut d);
            (d.MipLevels, 1)
        } else if resource.cast::<ID3D11Buffer>().is_ok() {
            (1, 1)
        } else {
            return Err("not a tiled buffer, 2D or 3D texture".into());
        }
    };
    let device: ID3D11Device2 = unsafe { resource.GetDevice() }
        .and_then(|d| d.cast())
        .map_err(|e| e.to_string())?;
    let mut count = mips.checked_mul(array).ok_or("subresource count overflow")?;
    let mut subs = vec![D3D11_SUBRESOURCE_TILING::default(); count as usize];
    let mut total = 0;
    let mut packed = D3D11_PACKED_MIP_DESC::default();
    let mut shape = D3D11_TILE_SHAPE::default();
    unsafe {
        device.GetResourceTiling(
            resource,
            Some(&mut total),
            Some(&mut packed),
            Some(&mut shape),
            Some(&mut count),
            0,
            subs.as_mut_ptr(),
        );
    }
    subs.truncate(count as usize);
    if total == 0 || mips == 0 || array == 0 {
        return Err("device returned no tiling for resource".into());
    }
    Ok(Layout {
        total,
        mips,
        array,
        packed: [
            u32::from(packed.NumStandardMips),
            u32::from(packed.NumPackedMips),
            packed.NumTilesForPackedMips,
            packed.StartTileIndexInOverallResource,
        ],
        shape: [shape.WidthInTexels, shape.HeightInTexels, shape.DepthInTexels],
        subs: subs
            .iter()
            .map(|s| {
                [
                    s.WidthInTiles,
                    u32::from(s.HeightInTiles),
                    u32::from(s.DepthInTiles),
                    s.StartTileIndexInOverallResource,
                ]
            })
            .collect(),
    })
}

pub fn region(v: D3D11_TILE_REGION_SIZE) -> Region {
    Region {
        count: v.NumTiles,
        boxed: v.bUseBox.as_bool(),
        width: v.Width,
        height: u32::from(v.Height),
        depth: u32::from(v.Depth),
    }
}
