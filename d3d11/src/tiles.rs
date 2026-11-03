//! D3D11 tiled-resource addressing and page-table updates shared by capture and native replay.
use std::collections::BTreeMap;
use windows::{Win32::Graphics::Direct3D11::*, core::Interface};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    pub total: u32,
    pub mips: u32,
    pub array: u32,
    /// Standard mips, packed mips, packed tiles per array slice, first packed tile in slice zero.
    pub packed: [u32; 4],
    pub shape: [u32; 3],
    /// Width, height, depth in tiles, overall start tile (MAX for packed mips).
    pub subs: Vec<[u32; 4]>,
}

impl Layout {
    pub fn query(resource: &ID3D11Resource) -> Result<Self, String> {
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
        Ok(Self {
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

    pub fn index(&self, c: [u32; 4]) -> Result<u32, String> {
        let [x, y, z, sub] = c;
        let [w, h, d, start] = *self
            .subs
            .get(sub as usize)
            .ok_or("tile subresource out of range")?;
        if start == u32::MAX {
            if y != 0 || z != 0 || x >= self.packed[2] {
                return Err("packed tile coordinate out of range".into());
            }
            return Ok((sub / self.mips) * (self.total / self.array) + self.packed[3] + x);
        }
        if x >= w || y >= h || z >= d {
            return Err("tile coordinate out of range".into());
        }
        let index = u64::from(start)
            + u64::from(x)
            + u64::from(y) * u64::from(w)
            + u64::from(z) * u64::from(w) * u64::from(h);
        u32::try_from(index).map_err(|_| "tile index overflow".into())
    }

    /// A canonical coordinate for an overall tile, using the first packed mip for a packed tile.
    pub fn coordinate(&self, index: u32) -> Result<[u32; 4], String> {
        if index >= self.total {
            return Err("tile index out of range".into());
        }
        for (sub, &[w, h, d, start]) in self.subs.iter().enumerate() {
            if start == u32::MAX || w == 0 || h == 0 {
                continue;
            }
            let n = u64::from(w) * u64::from(h) * u64::from(d);
            if index >= start && u64::from(index - start) < n {
                let offset = index - start;
                return Ok([offset % w, (offset / w) % h, offset / (w * h), sub as u32]);
            }
        }
        let per_slice = self.total / self.array;
        let slice = index / per_slice;
        let local = index % per_slice;
        if self.packed[1] != 0 && local >= self.packed[3] && local - self.packed[3] < self.packed[2] {
            return Ok([local - self.packed[3], 0, 0, slice * self.mips + self.packed[0]]);
        }
        Err("tile is outside the reported subresources and packed mip groups".into())
    }

    /// None/None describes the entire resource; a supplied coordinate with no size describes one tile.
    pub fn region(&self, start: Option<[u32; 4]>, size: Option<Region>) -> Result<Vec<u32>, String> {
        if start.is_none() && size.is_none() {
            return Ok((0..self.total).collect());
        }
        let at = start.unwrap_or([0; 4]);
        let first = self.index(at)?;
        let Some(size) = size else {
            return Ok(vec![first]);
        };
        if !size.boxed {
            let end = first
                .checked_add(size.count)
                .filter(|&end| end <= self.total)
                .ok_or("linear tile range out of bounds")?;
            return Ok((first..end).collect());
        }
        if self.subs[at[3] as usize][3] == u32::MAX {
            return Err("box regions cannot address packed mip tiles".into());
        }
        let mut out = Vec::new();
        for z in 0..size.depth {
            for y in 0..size.height {
                for x in 0..size.width {
                    out.push(self.index([
                        at[0].checked_add(x).ok_or("tile X overflow")?,
                        at[1].checked_add(y).ok_or("tile Y overflow")?,
                        at[2].checked_add(z).ok_or("tile Z overflow")?,
                        at[3],
                    ])?);
                }
            }
        }
        Ok(out)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Region {
    pub count: u32,
    pub boxed: bool,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
}
impl From<D3D11_TILE_REGION_SIZE> for Region {
    fn from(v: D3D11_TILE_REGION_SIZE) -> Self {
        Self {
            count: v.NumTiles,
            boxed: v.bUseBox.as_bool(),
            width: v.Width,
            height: u32::from(v.Height),
            depth: u32::from(v.Depth),
        }
    }
}

/// Only non-null mappings occupy memory. A D3D11 resource refers to one tile pool at a time.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mappings {
    pub pool: Option<u64>,
    pub tiles: BTreeMap<u32, u32>,
}
#[derive(Clone, Copy, Debug)]
pub struct Range {
    pub flag: u32,
    pub offset: u32,
    pub count: u32,
}
impl Mappings {
    pub fn update(&mut self, pool: Option<u64>, locations: &[u32], ranges: &[Range]) -> Result<(), String> {
        if ranges.iter().map(|r| u64::from(r.count)).sum::<u64>() != locations.len() as u64 {
            return Err("tile ranges and resource regions differ in length".into());
        }
        if ranges.iter().any(|r| !matches!(r.flag, 0 | 1 | 2 | 4)) {
            return Err("unknown tile range flag".into());
        }
        for r in ranges {
            if r.count != 0 && matches!(r.flag, 0 | 4) && pool.or(self.pool).is_none() {
                return Err("non-null tile mapping has no pool".into());
            }
            if r.flag == 0 && r.count != 0 && r.offset.checked_add(r.count - 1).is_none() {
                return Err("pool tile overflow".into());
            }
        }
        if let Some(pool) = pool
            && self.pool != Some(pool)
        {
            self.tiles.clear();
            self.pool = Some(pool);
        }
        let mut index = 0;
        for range in ranges {
            for offset in 0..range.count {
                let location = locations[index];
                index += 1;
                match range.flag {
                    1 => {
                        self.tiles.remove(&location);
                    } // NULL
                    2 => {} // SKIP
                    flag => {
                        if self.pool.is_none() {
                            return Err("non-null tile mapping has no pool".into());
                        }
                        let tile = if flag == 4 {
                            range.offset
                        } else {
                            range.offset.checked_add(offset).ok_or("pool tile overflow")?
                        };
                        self.tiles.insert(location, tile);
                    }
                }
            }
        }
        Ok(())
    }
    /// The pool tiles `locations` map to, `None` where they are unmapped: what [`Mappings::copy`] copies.
    pub fn at(&self, locations: &[u32]) -> Vec<Option<u32>> {
        locations.iter().map(|i| self.tiles.get(i).copied()).collect()
    }

    /// Maps `dst` as the source's locations were (`values`, from [`Mappings::at`], read before this so an overlapping
    /// copy within one resource works), in the source's `pool`.
    pub fn copy(&mut self, dst: &[u32], pool: Option<u64>, values: &[Option<u32>]) -> Result<(), String> {
        if dst.len() != values.len() {
            return Err("tile copy regions differ in length".into());
        }
        if self.pool != pool {
            self.tiles.clear();
            self.pool = pool;
        }
        for (&dst, &value) in dst.iter().zip(values) {
            match value {
                Some(tile) => {
                    self.tiles.insert(dst, tile);
                }
                None => {
                    self.tiles.remove(&dst);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn layout() -> Layout {
        Layout {
            total: 12,
            mips: 2,
            array: 2,
            packed: [1, 1, 2, 4],
            shape: [128, 128, 1],
            subs: vec![
                [2, 2, 1, 0],
                [0, 0, 0, u32::MAX],
                [2, 2, 1, 6],
                [0, 0, 0, u32::MAX],
            ],
        }
    }
    #[test]
    fn array_and_packed_addresses() {
        let l = layout();
        assert_eq!(
            l.region(
                Some([1, 0, 0, 0]),
                Some(Region {
                    count: 0,
                    boxed: true,
                    width: 1,
                    height: 2,
                    depth: 1
                })
            )
            .unwrap(),
            [1, 3]
        );
        assert_eq!(
            l.region(
                Some([1, 1, 0, 0]),
                Some(Region {
                    count: 5,
                    boxed: false,
                    width: 0,
                    height: 0,
                    depth: 0
                })
            )
            .unwrap(),
            [3, 4, 5, 6, 7]
        );
        assert_eq!(l.coordinate(11).unwrap(), [1, 0, 0, 3]);
        for i in 0..12 {
            assert_eq!(l.index(l.coordinate(i).unwrap()).unwrap(), i);
        }
        assert!(l.index([2, 0, 0, 3]).is_err());
    }
    #[test]
    fn flags_and_pool_switch() {
        let mut m = Mappings::default();
        m.update(
            Some(10),
            &[0, 1, 2, 3],
            &[Range {
                flag: 0,
                offset: 20,
                count: 4,
            }],
        )
        .unwrap();
        m.update(
            None,
            &[0, 1, 2, 3],
            &[
                Range {
                    flag: 1,
                    offset: 0,
                    count: 1,
                },
                Range {
                    flag: 2,
                    offset: 0,
                    count: 1,
                },
                Range {
                    flag: 4,
                    offset: 7,
                    count: 2,
                },
            ],
        )
        .unwrap();
        assert_eq!(m.tiles, BTreeMap::from([(1, 21), (2, 7), (3, 7)]));
        m.update(
            Some(11),
            &[2],
            &[Range {
                flag: 0,
                offset: 5,
                count: 1,
            }],
        )
        .unwrap();
        assert_eq!(m.tiles, BTreeMap::from([(2, 5)]));
    }
    #[test]
    fn rejected_update_preserves_the_previous_page_table() {
        let mut m = Mappings {
            pool: Some(7),
            tiles: BTreeMap::from([(0, 4)]),
        };
        let before = m.clone();
        assert!(
            m.update(
                Some(8),
                &[0, 1],
                &[Range {
                    flag: 0,
                    offset: u32::MAX,
                    count: 2
                }]
            )
            .is_err()
        );
        assert_eq!(m, before);
    }
    #[test]
    fn copied_mappings_preserve_holes_and_overlap() {
        let source = Mappings {
            pool: Some(9),
            tiles: BTreeMap::from([(0, 10), (1, 11), (3, 13)]),
        };
        // Within one resource, onto locations the copy reads.
        let mut m = source.clone();
        let values = m.at(&[0, 1, 2]);
        m.copy(&[1, 2, 3], m.pool, &values).unwrap();
        assert_eq!(m.tiles, BTreeMap::from([(0, 10), (1, 10), (2, 11)]));
        let mut other = Mappings {
            pool: Some(2),
            tiles: BTreeMap::from([(99, 1)]),
        };
        other.copy(&[7], source.pool, &source.at(&[3])).unwrap();
        assert_eq!(other.pool, Some(9));
        assert_eq!(other.tiles, BTreeMap::from([(7, 13)]));
    }
}
