//! Ordered allocation-range starts. Payload membership is validated by the
//! selected region; gaps must never be mistaken for an allocation.
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Address(usize);
impl PartialOrd for Address {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Address {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        #[cfg(test)]
        COMPARISONS.with(|count| count.set(count.get() + 1));
        self.0.cmp(&other.0)
    }
}

#[derive(Default)]
pub(super) struct AddressIndex {
    starts: BTreeMap<Address, usize>,
}
impl AddressIndex {
    pub(super) fn matches(&self, count: usize, start_at: impl Fn(usize) -> Option<usize>) -> bool {
        self.starts.len() == count
            && self
                .starts
                .iter()
                .all(|(start, &index)| start_at(index) == Some(start.0))
    }
    pub(super) fn insert(&mut self, start: usize, index: usize) {
        assert!(
            self.starts.insert(Address(start), index).is_none(),
            "duplicate heap region start"
        );
    }
    pub(super) fn candidate(&self, address: usize) -> Option<usize> {
        self.starts
            .range(..=Address(address))
            .next_back()
            .map(|(_, &index)| index)
    }
    pub(super) fn exact(&self, start: usize) -> Option<usize> {
        self.starts.get(&Address(start)).copied()
    }
    /// Bulk sweep already visits all regions. Update vector positions in one
    /// ordered traversal instead of performing a tree lookup per survivor.
    pub(super) fn remap(&mut self, positions: &[Option<usize>]) {
        self.starts.retain(|_, index| {
            if let Some(next) = positions[*index] {
                *index = next;
                true
            } else {
                false
            }
        });
    }
    pub(super) fn clear(&mut self) {
        self.starts.clear();
    }
}

#[cfg(test)]
thread_local! { static COMPARISONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
pub(super) fn take_comparisons() -> usize {
    COMPARISONS.replace(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lookup_comparisons_scale_with_tree_height_and_survive_bulk_compaction() {
        for count in [16usize, 64, 256, 1024, 4096] {
            let mut index = AddressIndex::default();
            for i in 0..count {
                index.insert(1024 + i * 128, i);
            }
            assert_eq!(index.candidate(1023), None);
            COMPARISONS.set(0);
            for i in 0..count {
                assert_eq!(index.candidate(1024 + i * 128 + 127), Some(i));
            }
            let comparisons = COMPARISONS.get();
            assert!(comparisons <= count * 12 * (count.ilog2() as usize + 1));
            println!("region-index regions={count} queries={count} comparisons={comparisons}");
            let positions: Vec<_> = (0..count).map(|i| (i % 2 == 1).then_some(i / 2)).collect();
            index.remap(&positions);
            for i in (1..count).step_by(2) {
                assert_eq!(index.exact(1024 + i * 128), Some(i / 2));
            }
            for i in (0..count).step_by(2) {
                assert_eq!(index.exact(1024 + i * 128), None);
            }
            index.clear();
            assert_eq!(index.candidate(usize::MAX), None);
        }
    }
}
