//! Largest available-span selection. Span keys are bounded by one regular
//! region; region identities within a bucket use expected-constant swap removal.
use crate::id_hash::IdMap;
use std::collections::BTreeMap;

#[derive(Default)]
pub(super) struct RegionCandidates {
    buckets: BTreeMap<usize, Vec<usize>>,
    // Runtime-assigned region indices need no SipHash (see `id_hash`).
    entries: IdMap<usize, ((usize, usize), usize)>,
    largest: Option<(usize, usize)>,
}

impl RegionCandidates {
    fn refresh(&mut self) {
        self.largest = self
            .buckets
            .last_key_value()
            .map(|(&bytes, indices)| (bytes, *indices.last().unwrap()));
    }

    pub(super) fn insert(&mut self, key: (usize, usize)) -> bool {
        let (bytes, index) = key;
        assert!(bytes <= super::GC_OLD_REGION_SIZE);
        if let Some(&(existing, _)) = self.entries.get(&index) {
            assert_eq!(existing, key, "candidate must be removed before resizing");
            return false;
        }
        let bucket = self.buckets.entry(bytes).or_default();
        self.entries.insert(index, (key, bucket.len()));
        bucket.push(index);
        self.refresh();
        true
    }

    pub(super) fn remove(&mut self, key: &(usize, usize)) -> bool {
        let Some(&(existing, position)) = self.entries.get(&key.1) else {
            return false;
        };
        if existing != *key {
            return false;
        }
        self.entries.remove(&key.1);
        let bucket = self.buckets.get_mut(&key.0).unwrap();
        bucket.swap_remove(position);
        if let Some(&moved) = bucket.get(position) {
            self.entries.get_mut(&moved).unwrap().1 = position;
        }
        if bucket.is_empty() {
            self.buckets.remove(&key.0);
        }
        self.refresh();
        true
    }

    pub(super) fn last(&self) -> Option<&(usize, usize)> {
        self.largest.as_ref()
    }
    pub(super) fn pop_last(&mut self) -> Option<(usize, usize)> {
        let key = self.largest?;
        assert!(self.remove(&key));
        Some(key)
    }
    pub(super) fn iter(&self) -> impl Iterator<Item = &(usize, usize)> {
        self.entries.values().map(|(key, _)| key)
    }
    pub(super) fn len(&self) -> usize {
        self.entries.len()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

impl FromIterator<(usize, usize)> for RegionCandidates {
    fn from_iter<T: IntoIterator<Item = (usize, usize)>>(iter: T) -> Self {
        let mut result = Self::default();
        for key in iter {
            result.insert(key);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn equal_span_regions_have_constant_membership_work() {
        for count in [1_000, 10_000, 100_000] {
            let mut candidates = RegionCandidates::default();
            for index in 0..count {
                assert!(candidates.insert((128, index)));
            }
            assert_eq!(candidates.buckets.len(), 1);
            for index in (0..count).step_by(2) {
                assert!(!candidates.remove(&(64, index)));
                assert!(candidates.remove(&(128, index)));
                assert!(candidates.insert((64, index)));
            }
            let mut previous = usize::MAX;
            let mut popped = 0;
            while let Some((bytes, _)) = candidates.pop_last() {
                assert!(bytes <= previous);
                previous = bytes;
                popped += 1;
            }
            assert_eq!(popped, count);
            assert!(candidates.entries.is_empty());
            assert!(candidates.buckets.is_empty());
            eprintln!(
                "candidate regions={count} inserts={} removes={} pops={count} max_buckets=2",
                count + count / 2,
                count / 2
            );
        }
    }
}
