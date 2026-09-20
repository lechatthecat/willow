//! Concurrent mark ownership, separate from the allocator's object-start map
//! and the header's stop-the-world mark bit.
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) struct ConcurrentMarkBits {
    words: Box<[AtomicU64]>,
}

impl ConcurrentMarkBits {
    pub(super) fn new(objects: usize) -> Self {
        Self {
            words: (0..objects.div_ceil(64))
                .map(|_| AtomicU64::new(0))
                .collect(),
        }
    }
    pub(super) fn words_ptr(&self) -> *mut AtomicU64 {
        self.words.as_ptr().cast_mut()
    }
    /// Only the first claimant may publish this object's trace job.
    pub(super) fn claim(&self, index: usize) -> bool {
        let bit = 1u64 << (index % 64);
        let word = &self.words[index / 64];
        if word.load(Ordering::Acquire) & bit != 0 {
            return false;
        }
        word.fetch_or(bit, Ordering::AcqRel) & bit == 0
    }
    pub(super) fn contains(&self, index: usize) -> bool {
        self.words
            .get(index / 64)
            .is_some_and(|word| word.load(Ordering::Acquire) & (1u64 << (index % 64)) != 0)
    }
    pub(super) fn clear(&self) {
        for word in &self.words {
            word.store(0, Ordering::Release);
        }
    }
    pub(super) fn set(&self, index: usize) {
        if let Some(word) = self.words.get(index / 64) {
            word.fetch_or(1u64 << (index % 64), Ordering::Release);
        }
    }
    #[cfg(test)]
    pub(super) fn unset(&self, index: usize) {
        if let Some(word) = self.words.get(index / 64) {
            word.fetch_and(!(1u64 << (index % 64)), Ordering::Release);
        }
    }
    #[cfg(test)]
    pub(super) fn count(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.load(Ordering::Acquire).count_ones() as usize)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn concurrent_claims_have_exactly_one_owner_across_word_boundaries() {
        for objects in [1, 63, 64, 65, 4096] {
            let bits = Arc::new(ConcurrentMarkBits::new(objects));
            let start = Arc::new(Barrier::new(8));
            let workers: Vec<_> = (0..8)
                .map(|_| {
                    let (bits, start) = (bits.clone(), start.clone());
                    std::thread::spawn(move || {
                        start.wait();
                        (0..objects).filter(|&index| bits.claim(index)).count()
                    })
                })
                .collect();
            assert_eq!(
                workers
                    .into_iter()
                    .map(|h| h.join().unwrap())
                    .sum::<usize>(),
                objects
            );
            assert_eq!(bits.count(), objects);
            assert_eq!(bits.words.len(), objects.div_ceil(64));
            for index in 0..objects {
                assert!(bits.contains(index));
                assert!(!bits.claim(index));
            }
        }
    }
}
