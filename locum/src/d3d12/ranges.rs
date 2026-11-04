//! Dynamic interval index. Overlapping placed resources remain separate candidates;
//! a lookup visits one segment, not every live allocation.
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default)]
pub(super) struct Ranges {
    segments: BTreeMap<u64, BTreeSet<u64>>,
    objects: BTreeMap<u64, (u64, u64)>,
}
impl Ranges {
    fn boundary(&mut self, at: u64) {
        if !self.segments.contains_key(&at) {
            let previous = self
                .segments
                .range(..at)
                .next_back()
                .map(|(_, v)| v.clone())
                .unwrap_or_default();
            self.segments.insert(at, previous);
        }
    }
    pub fn insert(&mut self, id: u64, start: u64, size: u64) {
        self.remove(id);
        let Some(end) = start.checked_add(size).filter(|end| *end > start) else {
            return;
        };
        self.boundary(start);
        self.boundary(end);
        for (_, ids) in self.segments.range_mut(start..end) {
            ids.insert(id);
        }
        self.objects.insert(id, (start, end));
    }
    pub fn remove(&mut self, id: u64) {
        let Some((start, end)) = self.objects.remove(&id) else {
            return;
        };
        self.boundary(start);
        self.boundary(end);
        for (_, ids) in self.segments.range_mut(start..end) {
            ids.remove(&id);
        }
        let points: Vec<_> = self.segments.range(start..=end).map(|(&at, _)| at).collect();
        for at in points {
            let same = self.segments.range(..at).next_back().map_or_else(
                || self.segments[&at].is_empty(),
                |(_, previous)| *previous == self.segments[&at],
            );
            if same {
                self.segments.remove(&at);
            }
        }
    }
    pub fn get(&self, address: u64) -> impl Iterator<Item = u64> + '_ {
        self.segments
            .range(..=address)
            .next_back()
            .into_iter()
            .flat_map(|(_, ids)| ids.iter().copied())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn overlaps_reuse_and_removal_match_an_exhaustive_oracle() {
        let mut index = Ranges::default();
        let mut oracle = BTreeMap::new();
        for n in 0..500u64 {
            let id = n % 31;
            if n % 5 == 0 {
                index.remove(id);
                oracle.remove(&id);
            } else {
                let (start, size) = ((n * 37) % 257, 1 + (n * 19) % 87);
                index.insert(id, start, size);
                oracle.insert(id, (start, size));
            }
            for address in 0..350 {
                let expected: Vec<_> = oracle
                    .iter()
                    .filter(|(_, (start, size))| address >= *start && address - start < *size)
                    .map(|(&id, _)| id)
                    .collect();
                assert_eq!(
                    index.get(address).collect::<Vec<_>>(),
                    expected,
                    "step {n}, address {address}"
                );
            }
        }
        for id in 0..31 {
            index.remove(id);
        }
        assert!(index.segments.is_empty());
    }
    #[test]
    fn large_disjoint_population_has_one_candidate_per_lookup() {
        let mut index = Ranges::default();
        for id in 0..60_000 {
            index.insert(id, id * 4096, 256);
        }
        let start = std::time::Instant::now();
        for n in 0..1_000_000 {
            let id = n % 60_000;
            assert_eq!(index.get(id * 4096 + 128).collect::<Vec<_>>(), vec![id]);
        }
        eprintln!(
            "1,000,000 lookups across 60,000 allocations: {:?}",
            start.elapsed()
        );
    }
}
