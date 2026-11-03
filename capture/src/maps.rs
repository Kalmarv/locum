//! What a map wrote. While a capture records, a map hands the game memory locum owns (a shadow), copied whole to the
//! driver's at the unmap, and only the bytes the game wrote are recorded: those that differ from what the shadow
//! started as. A discarding map's shadow starts as [`fill`]'s pattern, so the GPU sees that pattern wherever the game
//! wrote nothing, and a replay of the map writes the same pattern before the bytes recorded.

/// The pattern's 4-byte word at word `i`.
fn pattern(i: usize) -> u32 {
    (i as u32).wrapping_mul(0x9e37_79b9) ^ 0x5bd1_e995
}

/// Fills `data` with the pattern a discarding map starts as.
pub fn fill(data: &mut [u8]) {
    for (i, chunk) in data.chunks_mut(4).enumerate() {
        let p = pattern(i).to_le_bytes();
        chunk.copy_from_slice(&p[..chunk.len()]);
    }
}

/// The byte ranges (start, end) of `data` that differ from `reference` (or from the pattern), merged across gaps of
/// 64 bytes: what a map records.
pub fn written(data: &[u8], reference: Option<&[u8]>) -> Vec<(usize, usize)> {
    differing(data, reference, 64)
}

/// The byte ranges of `data` that differ from `reference` (or from the pattern), word by word with nothing between
/// them: what an unmap copies back, so it writes no byte the game did not write.
pub fn changed(data: &[u8], reference: Option<&[u8]>) -> Vec<(usize, usize)> {
    differing(data, reference, 0)
}

fn differing(data: &[u8], reference: Option<&[u8]>, gap: usize) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for (i, chunk) in data.chunks(4).enumerate() {
        let differs = match reference {
            Some(r) => chunk != &r[i * 4..i * 4 + chunk.len()],
            None => chunk != &pattern(i).to_le_bytes()[..chunk.len()],
        };
        if !differs {
            continue;
        }
        let (start, end) = (i * 4, i * 4 + chunk.len());
        match ranges.last_mut() {
            Some(last) if start <= last.1 + gap => last.1 = end,
            _ => ranges.push((start, end)),
        }
    }
    ranges
}

/// The bytes a discarding map of `size` bytes left, given the ranges it wrote (offset, length) and their bytes one
/// after another.
pub fn discarded(size: usize, ranges: &[(usize, usize)], bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; size];
    fill(&mut out);
    let mut at = 0;
    for &(offset, len) in ranges {
        if let (Some(dst), Some(src)) = (out.get_mut(offset..offset + len), bytes.get(at..at + len)) {
            dst.copy_from_slice(src);
        }
        at += len;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_discarding_map_replays_to_the_bytes_the_gpu_saw() {
        let mut shadow = vec![0u8; 1000];
        fill(&mut shadow);
        shadow[8..88].fill(7);
        shadow[600..604].copy_from_slice(&1.5f32.to_le_bytes());
        let ranges = written(&shadow, None);
        assert_eq!(ranges, vec![(8, 88), (600, 604)]);
        // An unmap copies back only the words written.
        let mut reference = shadow.clone();
        reference[40..44].copy_from_slice(&[9, 9, 9, 9]);
        reference[52..56].copy_from_slice(&[9, 9, 9, 9]);
        assert_eq!(changed(&shadow, Some(&reference)), vec![(40, 44), (52, 56)]);
        assert_eq!(written(&shadow, Some(&reference)), vec![(40, 56)]);
        let bytes: Vec<u8> = ranges.iter().flat_map(|&(a, b)| shadow[a..b].to_vec()).collect();
        let spans: Vec<(usize, usize)> = ranges.iter().map(|&(a, b)| (a, b - a)).collect();
        assert_eq!(discarded(1000, &spans, &bytes), shadow);
        // Nothing written: the GPU saw the pattern alone.
        let mut empty = vec![0u8; 64];
        fill(&mut empty);
        assert!(written(&empty, None).is_empty());
        assert_eq!(discarded(64, &[], &[]), empty);
    }
}
