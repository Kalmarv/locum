//! Tiled (D3D11) and reserved (D3D12) resource addressing and page tables, shared by capture and replay. Both APIs
//! address 64 KiB tiles alike: a resource's tiles are numbered through its subresources in order, each standard mip a
//! box of tiles, and each array slice's packed mips a run of tiles after its standard ones. The device reports the
//! numbering ([`Layout`]); the API crates read it.
use std::collections::BTreeMap;

use windows::Win32::Graphics::Direct3D12::{
    D3D12_PACKED_MIP_INFO, D3D12_RESOURCE_DIMENSION_BUFFER, D3D12_RESOURCE_DIMENSION_TEXTURE3D,
    D3D12_SUBRESOURCE_TILING, D3D12_TILE_SHAPE, ID3D12Device, ID3D12Resource,
};

/// The bytes of one tile, for every format and both APIs.
pub const TILE_BYTES: u64 = 65536;

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
    pub fn is_packed(&self, subresource: u32) -> bool {
        self.mips != 0 && self.packed[1] != 0 && subresource % self.mips >= self.packed[0]
    }

    /// The flat virtual tile range backing one array slice's complete packed mip group.
    pub fn packed_tiles(&self, slice: u32) -> Result<std::ops::Range<u32>, String> {
        if self.array == 0 || slice >= self.array || !self.total.is_multiple_of(self.array) {
            return Err("invalid packed mip array layout".into());
        }
        let per_slice = self.total / self.array;
        let start = slice
            .checked_mul(per_slice)
            .and_then(|n| n.checked_add(self.packed[3]))
            .ok_or("packed mip tile range overflow")?;
        let end = start
            .checked_add(self.packed[2])
            .filter(|&n| n <= (slice + 1) * per_slice)
            .ok_or("packed mip tile range outside array slice")?;
        Ok(start..end)
    }

    /// The layout as captures record it.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({"total": self.total, "mips": self.mips, "array": self.array, "packed": self.packed,
            "shape": self.shape, "subs": self.subs})
    }

    /// A layout [`Layout::json`] recorded.
    pub fn from_json(v: &serde_json::Value) -> Option<Self> {
        let n = |v: &serde_json::Value| v.as_u64().and_then(|n| u32::try_from(n).ok());
        let row = |v: &serde_json::Value| -> Option<[u32; 4]> {
            Some([n(&v[0])?, n(&v[1])?, n(&v[2])?, n(&v[3])?])
        };
        Some(Self {
            total: n(&v["total"])?,
            mips: n(&v["mips"])?,
            array: n(&v["array"])?,
            packed: row(&v["packed"])?,
            shape: [n(&v["shape"][0])?, n(&v["shape"][1])?, n(&v["shape"][2])?],
            subs: v["subs"].as_array()?.iter().map(row).collect::<Option<_>>()?,
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

/// Where one mapped tile of a D3D12 reserved resource is: a heap's id and the tile in it.
pub type Page = (u64, u32);

/// A D3D12 reserved resource's page table. Unlike a D3D11 tiled resource, which maps one tile pool at a time, it can
/// map tiles of several heaps: a call naming a heap changes only the tiles it names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pages {
    pub tiles: BTreeMap<u32, Page>,
}

impl Pages {
    /// An UpdateTileMappings of `locations` (the resource's tiles its regions name, in order) by `ranges` into `heap`;
    /// nothing changes when the call is invalid.
    pub fn update(&mut self, heap: Option<u64>, locations: &[u32], ranges: &[Range]) -> Result<(), String> {
        if ranges.iter().map(|r| u64::from(r.count)).sum::<u64>() != locations.len() as u64 {
            return Err("tile ranges and resource regions differ in length".into());
        }
        for r in ranges {
            match r.flag {
                // NONE, REUSE_SINGLE_TILE
                0 | 4 if r.count != 0 && heap.is_none() => {
                    return Err("a tile range maps heap tiles but the call names no heap".into());
                }
                0 if r.count != 0 && r.offset.checked_add(r.count - 1).is_none() => {
                    return Err("heap tile overflow".into());
                }
                0 | 1 | 2 | 4 => {}
                _ => return Err(format!("unknown tile range flag {}", r.flag)),
            }
        }
        let mut locations = locations.iter();
        for r in ranges {
            for i in 0..r.count {
                let location = *locations.next().expect("lengths checked");
                match r.flag {
                    1 => {
                        self.tiles.remove(&location);
                    }
                    2 => {}
                    flag => {
                        let tile = if flag == 4 { r.offset } else { r.offset + i };
                        self.tiles.insert(location, (heap.expect("checked"), tile));
                    }
                }
            }
        }
        Ok(())
    }

    /// The pages `locations` map to, `None` where they are unmapped: what [`Pages::copy`] copies.
    pub fn at(&self, locations: &[u32]) -> Vec<Option<Page>> {
        locations.iter().map(|i| self.tiles.get(i).copied()).collect()
    }

    /// A CopyTileMappings onto `dst` of the source's pages `values` (from [`Pages::at`], read before this so an
    /// overlapping copy within one resource works).
    pub fn copy(&mut self, dst: &[u32], values: &[Option<Page>]) -> Result<(), String> {
        if dst.len() != values.len() {
            return Err("tile copy regions differ in length".into());
        }
        for (&dst, value) in dst.iter().zip(values) {
            match value {
                Some(page) => self.tiles.insert(dst, *page),
                None => self.tiles.remove(&dst),
            };
        }
        Ok(())
    }

    /// The page table as captures record it: runs `[first tile, tiles, heap, first heap tile]` of consecutive tiles
    /// mapped to consecutive tiles of one heap.
    pub fn runs(&self) -> Vec<[u64; 4]> {
        let mut runs: Vec<[u64; 4]> = Vec::new();
        for (&tile, &(heap, page)) in &self.tiles {
            match runs.last_mut() {
                Some(r)
                    if r[0] + r[1] == u64::from(tile) && r[2] == heap && r[3] + r[1] == u64::from(page) =>
                {
                    r[1] += 1
                }
                _ => runs.push([u64::from(tile), 1, heap, u64::from(page)]),
            }
        }
        runs
    }

    /// A page table [`Pages::runs`] recorded.
    pub fn from_runs(v: &serde_json::Value) -> Option<Self> {
        let mut tiles = BTreeMap::new();
        for run in v.as_array()? {
            let n = |i: usize| run[i].as_u64();
            let (first, count, heap, page) = (n(0)?, n(1)?, n(2)?, n(3)?);
            for i in 0..count {
                tiles.insert(
                    u32::try_from(first + i).ok()?,
                    (heap, u32::try_from(page + i).ok()?),
                );
            }
        }
        Some(Self { tiles })
    }
}

/// The tile numbering a reserved resource's device gives it (Direct3D 12): the capture's and replay's alike.
pub fn d3d12(resource: &ID3D12Resource) -> Result<Layout, String> {
    let d = unsafe { resource.GetDesc() };
    let (mips, array) = match d.Dimension {
        D3D12_RESOURCE_DIMENSION_BUFFER => (1, 1),
        D3D12_RESOURCE_DIMENSION_TEXTURE3D => (u32::from(d.MipLevels), 1),
        _ => (u32::from(d.MipLevels), u32::from(d.DepthOrArraySize)),
    };
    if mips == 0 || array == 0 {
        return Err("the resource reports no subresources".into());
    }
    let mut device: Option<ID3D12Device> = None;
    unsafe { resource.GetDevice(&mut device) }.map_err(|e| format!("GetDevice: {e}"))?;
    let device = device.ok_or("GetDevice gave no device")?;
    let mut count = mips.checked_mul(array).ok_or("subresource count overflow")?;
    let mut subs = vec![D3D12_SUBRESOURCE_TILING::default(); count as usize];
    let mut total = 0;
    let mut packed = D3D12_PACKED_MIP_INFO::default();
    let mut shape = D3D12_TILE_SHAPE::default();
    unsafe {
        device.GetResourceTiling(
            resource,
            Some(&mut total),
            Some(&mut packed),
            Some(&mut shape),
            Some(&mut count),
            0,
            subs.as_mut_ptr(),
        )
    };
    subs.truncate(count as usize);
    if total == 0 {
        return Err("the device reports no tiles for the resource".into());
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reserved_pages_map_several_heaps_and_round_trip_as_runs() {
        let mut p = Pages::default();
        let r = |flag, offset, count| Range { flag, offset, count };
        p.update(Some(7), &[0, 1, 2, 3], &[r(0, 10, 4)]).unwrap();
        // Another heap for two tiles leaves the rest on the first: no pool switch as in D3D11.
        p.update(Some(8), &[2, 3], &[r(4, 5, 2)]).unwrap();
        assert_eq!(
            p.tiles,
            BTreeMap::from([(0, (7, 10)), (1, (7, 11)), (2, (8, 5)), (3, (8, 5))])
        );
        assert_eq!(p.runs(), [[0, 2, 7, 10], [2, 1, 8, 5], [3, 1, 8, 5]]);
        assert_eq!(Pages::from_runs(&serde_json::json!(p.runs())).unwrap(), p);
        // NULL and SKIP need no heap; a mapping range does, and a refused call changes nothing.
        p.update(None, &[0, 1], &[r(1, 0, 1), r(2, 0, 1)]).unwrap();
        assert_eq!(p.at(&[0, 1]), [None, Some((7, 11))]);
        let before = p.clone();
        assert!(p.update(None, &[1], &[r(0, 3, 1)]).is_err());
        assert!(p.update(Some(7), &[1, 2], &[r(0, 3, 1)]).is_err());
        assert_eq!(p, before);
        // A copy within one resource reads its source first.
        let values = p.at(&[1, 2]);
        p.copy(&[2, 3], &values).unwrap();
        assert_eq!(p.at(&[1, 2, 3]), [Some((7, 11)), Some((7, 11)), Some((8, 5))]);
    }
    #[test]
    fn a_layout_round_trips_through_its_json() {
        let l = layout();
        assert_eq!(Layout::from_json(&l.json()), Some(l));
    }
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
        assert_eq!(l.packed_tiles(0).unwrap(), 4..6);
        assert_eq!(l.packed_tiles(1).unwrap(), 10..12);
        assert!(!l.is_packed(0));
        assert!(l.is_packed(1));
        assert!(!l.is_packed(2));
        assert!(l.is_packed(3));
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
    fn packed_ranges_reject_invalid_array_and_pool_bounds() {
        let mut l = layout();
        assert!(l.packed_tiles(2).is_err());
        l.array = 0;
        assert!(l.packed_tiles(0).is_err());
        l.array = 2;
        l.total = 11;
        assert!(l.packed_tiles(0).is_err());
        l.total = 12;
        l.packed[2] = 3; // The tail would cross into the next array slice.
        assert!(l.packed_tiles(0).is_err());
        l.packed[2] = 2;
        l.packed[3] = u32::MAX; // Addition must be checked before indexing a page table.
        assert!(l.packed_tiles(0).is_err());
        assert!(l.packed_tiles(1).is_err());
        l.mips = 0;
        assert!(!l.is_packed(0));
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
