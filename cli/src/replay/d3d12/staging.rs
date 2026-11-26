//! The pieces a resource's contents move in between replay and its staging buffers. A staging buffer is one
//! committed buffer on an upload or readback heap, mapped, and kept until the list that copies through it has run: a
//! resource's contents larger than [`LIMIT`] (a game's 4 GiB buffer, say) cannot be one, so they go in pieces of at
//! most that, each staged, copied and run before the next. A piece of a buffer is a run of its bytes; of a texture,
//! whole subresources where they fit, else bands of a subresource's slices or of its rows (whole rows of blocks).
//! A reserved resource moves only what its mapped tiles hold: what is not mapped holds no memory.
use std::collections::BTreeSet;

use locum_capture::layout::Footprint;
use locum_capture::tiles::{Layout, TILE_BYTES};
use windows::Win32::Graphics::Direct3D12::{D3D12_PLACED_SUBRESOURCE_FOOTPRINT, D3D12_SUBRESOURCE_FOOTPRINT};

/// The most bytes one staging buffer holds.
pub(super) const LIMIT: u64 = 256 << 20;

/// D3D12_TEXTURE_DATA_PLACEMENT_ALIGNMENT: where a footprint may start in a buffer.
const PLACEMENT: u64 = 512;

/// Bytes `from..from + len` of a resource's copy layout (its bytes for a buffer, the footprints
/// [`super::D3d12::put`] lays a texture's out in), staged in one buffer, and the copies between it and the resource.
#[derive(Debug, PartialEq)]
pub(super) struct Piece {
    pub from: u64,
    pub len: u64,
    pub copies: Vec<Copy>,
}

#[derive(Debug, PartialEq)]
pub(super) enum Copy {
    /// The whole piece, to or from the buffer's bytes `at` on.
    Buffer { at: u64 },
    /// Subresource `sub` from row `y` and slice `z` (in texels) on, as much as `footprint` holds; its offset is in the
    /// piece. `whole`: all of the subresource.
    Texture {
        sub: u32,
        footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT,
        y: u32,
        z: u32,
        whole: bool,
    },
}

/// The tiles of a reserved resource that are mapped, against its tile layout.
pub(super) struct Mapped {
    pub layout: Layout,
    pub tiles: BTreeSet<u32>,
}

/// The pieces of a buffer `width` bytes long.
pub(super) fn buffer(width: u64, mapped: Option<&Mapped>, limit: u64) -> Vec<Piece> {
    let mut runs: Vec<(u64, u64)> = Vec::new();
    match mapped {
        None => runs.push((0, width)),
        Some(m) => {
            for &tile in &m.tiles {
                let (begin, end) = (u64::from(tile) * TILE_BYTES, (u64::from(tile) + 1) * TILE_BYTES);
                let end = end.min(width);
                if begin >= end {
                    continue;
                }
                match runs.last_mut() {
                    Some(last) if last.1 == begin => last.1 = end,
                    _ => runs.push((begin, end)),
                }
            }
        }
    }
    let limit = limit.max(1);
    let mut out = Vec::new();
    for (begin, end) in runs {
        let mut at = begin;
        while at < end {
            let len = (end - at).min(limit);
            out.push(Piece {
                from: at,
                len,
                copies: vec![Copy::Buffer { at }],
            });
            at += len;
        }
    }
    out
}

/// One box of a subresource to move: slices `z0..z1` and rows of blocks `r0..r1`.
struct Band {
    sub: u32,
    z0: u32,
    z1: u32,
    r0: u32,
    r1: u32,
}

/// The pieces of a texture whose subresources' copies lie as `placed` and `footprints` say (as
/// GetCopyableFootprints gives them).
pub(super) fn texture(
    placed: &[D3D12_PLACED_SUBRESOURCE_FOOTPRINT],
    footprints: &[Footprint],
    mapped: Option<&Mapped>,
    limit: u64,
) -> Vec<Piece> {
    let mut bands = Vec::new();
    for (i, (p, f)) in placed.iter().zip(footprints).enumerate() {
        let sub = i as u32;
        let (rows, depth) = (f.rows, p.Footprint.Depth.max(1));
        if rows == 0 {
            continue;
        }
        let whole = Band {
            sub,
            z0: 0,
            z1: depth,
            r0: 0,
            r1: rows,
        };
        let Some(m) = mapped else {
            bands.push(whole);
            continue;
        };
        let layout = &m.layout;
        if layout.is_packed(sub) {
            let slice = sub / layout.mips.max(1);
            if layout
                .packed_tiles(slice)
                .is_ok_and(|tiles| tiles.into_iter().any(|t| m.tiles.contains(&t)))
            {
                bands.push(whole);
            }
            continue;
        }
        // The rows and slices of tiles mapped, each a band of the tile's height and depth.
        let block = block_height(p, rows);
        let [_, height, deep] = layout.shape;
        let mut boxes = BTreeSet::new();
        for &tile in &m.tiles {
            if let Ok([_, y, z, s]) = layout.coordinate(tile)
                && s == sub
            {
                boxes.insert((z, y));
            }
        }
        for (z, y) in boxes {
            let (z0, z1) = (z * deep.max(1), ((z + 1) * deep.max(1)).min(depth));
            let r0 = (y * height.max(1)) / block;
            let r1 = ((y + 1) * height.max(1)).div_ceil(block).min(rows);
            if z0 >= z1 || r0 >= r1 {
                continue;
            }
            match bands.last_mut() {
                // The next tile row of the same slices continues the band.
                Some(b) if b.sub == sub && b.z0 == z0 && b.z1 == z1 && b.r1 == r0 => b.r1 = r1,
                _ => bands.push(Band { sub, z0, z1, r0, r1 }),
            }
        }
    }
    // Bands larger than the limit split by slices, then by rows.
    let mut split = Vec::new();
    for b in bands {
        let f = &footprints[b.sub as usize];
        let (pitch, slice) = (u64::from(f.row_pitch), u64::from(f.row_pitch) * u64::from(f.rows));
        let bytes = |b: &Band| extent(f, slice, b);
        if bytes(&b) <= limit {
            split.push(b);
        } else if b.r0 == 0
            && b.r1 == f.rows
            && b.z1 - b.z0 > 1
            && extent(f, slice, &Band { z1: b.z0 + 1, ..b }) <= limit
        {
            let per = (limit / slice.max(1)).max(1) as u32;
            let mut z = b.z0;
            while z < b.z1 {
                let z1 = (z + per).min(b.z1);
                split.push(Band { z0: z, z1, ..b });
                z = z1;
            }
        } else {
            let per = (limit / pitch.max(1)).max(1) as u32;
            for z in b.z0..b.z1 {
                let mut r = b.r0;
                while r < b.r1 {
                    let r1 = (r + per).min(b.r1);
                    split.push(Band {
                        sub: b.sub,
                        z0: z,
                        z1: z + 1,
                        r0: r,
                        r1,
                    });
                    r = r1;
                }
            }
        }
    }
    // Into pieces: a band joins the piece before where it starts at an aligned place in it, after its end, and the
    // piece stays within the limit.
    let mut out: Vec<Piece> = Vec::new();
    for b in split {
        let (p, f) = (&placed[b.sub as usize], &footprints[b.sub as usize]);
        let slice = u64::from(f.row_pitch) * u64::from(f.rows);
        let at = f.offset + u64::from(b.z0) * slice + u64::from(b.r0) * u64::from(f.row_pitch);
        let end = at + extent(f, slice, &b);
        let block = block_height(p, f.rows);
        let height = if b.r1 == f.rows {
            p.Footprint.Height - b.r0 * block
        } else {
            (b.r1 - b.r0) * block
        };
        let copy = |from: u64| Copy::Texture {
            sub: b.sub,
            footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: at - from,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: p.Footprint.Format,
                    Width: p.Footprint.Width,
                    Height: height,
                    Depth: b.z1 - b.z0,
                    RowPitch: f.row_pitch,
                },
            },
            y: b.r0 * block,
            z: b.z0,
            whole: b.r0 == 0 && b.r1 == f.rows && b.z0 == 0 && b.z1 == p.Footprint.Depth.max(1),
        };
        match out.last_mut() {
            Some(piece)
                if at >= piece.from + piece.len
                    && (at - piece.from).is_multiple_of(PLACEMENT)
                    && end - piece.from <= limit =>
            {
                piece.copies.push(copy(piece.from));
                piece.len = end - piece.from;
            }
            _ => out.push(Piece {
                from: at,
                len: end - at,
                copies: vec![copy(at)],
            }),
        }
    }
    out
}

/// The bytes band `b` of a subresource copied as `f` (`slice` bytes a slice) spans: its last row only as long as a
/// row of blocks.
fn extent(f: &Footprint, slice: u64, b: &Band) -> u64 {
    u64::from(b.z1 - b.z0 - 1) * slice + u64::from(b.r1 - b.r0 - 1) * u64::from(f.row_pitch) + f.row_bytes
}

/// The texels a row of blocks of a footprint `rows` rows of blocks high is high: 4 for block-compressed formats.
fn block_height(p: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT, rows: u32) -> u32 {
    match locum_capture::format::layout(p.Footprint.Format.0 as u32) {
        Some(l) => l.block_height.max(1),
        None => p.Footprint.Height.div_ceil(rows.max(1)).max(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT, DXGI_FORMAT_BC1_UNORM, DXGI_FORMAT_R8G8B8A8_UNORM,
    };

    const K: u64 = TILE_BYTES;

    fn at(pieces: &[Piece]) -> Vec<(u64, u64)> {
        pieces.iter().map(|p| (p.from, p.len)).collect()
    }

    #[test]
    fn a_buffer_moves_in_pieces_of_at_most_the_limit() {
        let pieces = buffer(10, None, 4);
        assert_eq!(at(&pieces), [(0, 4), (4, 4), (8, 2)]);
        assert_eq!(pieces[1].copies, [Copy::Buffer { at: 4 }]);
        assert_eq!(at(&buffer(10, None, 64)), [(0, 10)]);
    }

    #[test]
    fn a_reserved_buffer_moves_only_its_mapped_tiles() {
        let layout = Layout {
            total: 6,
            mips: 1,
            array: 1,
            packed: [1, 0, 0, 0],
            shape: [65536, 1, 1],
            subs: vec![[6, 1, 1, 0]],
        };
        let mapped = Mapped {
            layout,
            tiles: [0, 1, 5].into(),
        };
        // The last tile is cut at the buffer's end; consecutive tiles are one run, split at the limit.
        let width = 5 * K + 100;
        assert_eq!(
            at(&buffer(width, Some(&mapped), 64 * K)),
            [(0, 2 * K), (5 * K, 100)]
        );
        assert_eq!(
            at(&buffer(width, Some(&mapped), K)),
            [(0, K), (K, K), (5 * K, 100)]
        );
        let none = Mapped {
            tiles: BTreeSet::new(),
            ..mapped
        };
        assert!(buffer(width, Some(&none), K).is_empty());
    }

    /// A 2D subresource's copy: `width` x `height` texels of `format`, `bytes` a row of blocks, at `offset`.
    fn sub(
        offset: u64,
        format: DXGI_FORMAT,
        width: u32,
        height: u32,
        rows: u32,
        bytes: u32,
    ) -> (D3D12_PLACED_SUBRESOURCE_FOOTPRINT, Footprint) {
        let pitch = bytes.next_multiple_of(256);
        (
            D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                Offset: offset,
                Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                    Format: format,
                    Width: width,
                    Height: height,
                    Depth: 1,
                    RowPitch: pitch,
                },
            },
            Footprint {
                offset,
                row_pitch: pitch,
                depth: 1,
                rows,
                row_bytes: u64::from(bytes),
            },
        )
    }

    fn rows(pieces: &[Piece]) -> Vec<(u32, u32, u32, bool)> {
        pieces
            .iter()
            .flat_map(|p| &p.copies)
            .map(|c| match c {
                Copy::Texture {
                    sub,
                    footprint,
                    y,
                    whole,
                    ..
                } => (*sub, *y, footprint.Footprint.Height, *whole),
                Copy::Buffer { .. } => panic!("a buffer copy of a texture"),
            })
            .collect()
    }

    #[test]
    fn subresources_that_fit_share_a_piece_and_one_that_does_not_moves_in_bands_of_whole_block_rows() {
        // BC1 64x40: 10 rows of 16 blocks of 8 bytes (rows pitched at 256), then its 32x20 mip at 2560 (5 rows).
        let (p0, f0) = sub(0, DXGI_FORMAT_BC1_UNORM, 64, 40, 10, 128);
        let (p1, f1) = sub(2560, DXGI_FORMAT_BC1_UNORM, 32, 20, 5, 64);
        let (placed, footprints) = ([p0, p1], [f0, f1]);
        let pieces = texture(&placed, &footprints, None, 1 << 20);
        assert_eq!(at(&pieces), [(0, 2560 + 4 * 256 + 64)]);
        assert_eq!(rows(&pieces), [(0, 0, 40, true), (1, 0, 20, true)]);
        let Copy::Texture { footprint, .. } = &pieces[0].copies[1] else {
            unreachable!()
        };
        assert_eq!(footprint.Offset, 2560);

        // 1024 bytes a piece: four rows of blocks, 16 texels, each; the last band to the subresource's end.
        let pieces = texture(&placed, &footprints, None, 1024);
        assert_eq!(
            rows(&pieces),
            [
                (0, 0, 16, false),
                (0, 16, 16, false),
                (0, 32, 8, false),
                (1, 0, 16, false),
                (1, 16, 4, false)
            ]
        );
        assert!(pieces.iter().all(|p| p.len <= 1024));
        for p in &pieces {
            for c in &p.copies {
                let Copy::Texture { footprint, .. } = c else {
                    unreachable!()
                };
                assert!(footprint.Offset.is_multiple_of(PLACEMENT));
            }
        }
        // The bands start where their rows are in the copy layout.
        assert_eq!(pieces[1].from, 4 * 256);
        assert_eq!(pieces[3].from, 2560);
    }

    #[test]
    fn a_volume_moves_in_bands_of_slices() {
        let (mut p, mut f) = sub(0, DXGI_FORMAT_R8G8B8A8_UNORM, 64, 4, 4, 256);
        p.Footprint.Depth = 8;
        f.depth = 8;
        // A slice is 1 KiB: three a piece.
        let pieces = texture(&[p], &[f], None, 3 * 1024);
        let slices: Vec<(u32, u32)> = pieces
            .iter()
            .flat_map(|p| &p.copies)
            .map(|c| match c {
                Copy::Texture { z, footprint, .. } => (*z, footprint.Footprint.Depth),
                Copy::Buffer { .. } => unreachable!(),
            })
            .collect();
        assert_eq!(slices, [(0, 3), (3, 3), (6, 2)]);
    }

    #[test]
    fn a_reserved_texture_moves_the_rows_of_its_mapped_tiles_and_its_packed_mips_if_mapped() {
        // 256x256 RGBA8 in 128x128 tiles (2x2 tiles), its second mip packed with the rest.
        let layout = Layout {
            total: 5,
            mips: 2,
            array: 1,
            packed: [1, 1, 1, 4],
            shape: [128, 128, 1],
            subs: vec![[2, 2, 1, 0], [0, 0, 0, u32::MAX]],
        };
        let (p0, f0) = sub(0, DXGI_FORMAT_R8G8B8A8_UNORM, 256, 256, 256, 1024);
        let (p1, f1) = sub(256 * 1024, DXGI_FORMAT_R8G8B8A8_UNORM, 128, 128, 128, 512);
        let (placed, footprints) = ([p0, p1], [f0, f1]);
        // Tile (1, 1) only: the second row of tiles, all its width; the packed mips are not mapped.
        let mapped = Mapped {
            layout: layout.clone(),
            tiles: [3].into(),
        };
        assert_eq!(
            rows(&texture(&placed, &footprints, Some(&mapped), 1 << 30)),
            [(0, 128, 128, false)]
        );
        let mapped = Mapped {
            layout,
            tiles: [0, 1, 4].into(),
        };
        assert_eq!(
            rows(&texture(&placed, &footprints, Some(&mapped), 1 << 30)),
            [(0, 0, 128, false), (1, 0, 128, true)]
        );
    }
}
