//! Shared worker-lifetime leases for native stacks and cooperative segments.
use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

const SHRINKING: usize = 1;
const INVALIDATED: usize = 2;
const FLAGS: usize = SHRINKING | INVALIDATED;
const BOUND_SHIFT: u32 = 2;

#[derive(Debug, Default)]
#[repr(align(64))]
struct WorkerCount(AtomicUsize);

/// Stable, geometrically sized counter arrays. Existing workers require one
/// indexed pointer load; arrays are published once and reclaimed with the registry.
#[derive(Debug)]
struct WorkerCounts([AtomicPtr<WorkerCount>; usize::BITS as usize]);

impl Default for WorkerCounts {
    fn default() -> Self {
        Self(std::array::from_fn(|_| AtomicPtr::default()))
    }
}

impl WorkerCounts {
    fn location(worker: usize) -> (usize, usize) {
        let ordinal = worker.checked_add(1).expect("worker index overflow");
        let bucket = ordinal.ilog2() as usize;
        (bucket, ordinal - (1usize << bucket))
    }

    fn existing(&self, worker: usize) -> Option<&AtomicUsize> {
        let (bucket, offset) = Self::location(worker);
        let ptr = self.0[bucket].load(Ordering::Acquire);
        if ptr.is_null() {
            None
        } else {
            // SAFETY: publication initializes the entire bucket. Buckets never
            // move or disappear while this registry (and the returned borrow) lives.
            Some(unsafe { &(*ptr.add(offset)).0 })
        }
    }

    fn get(&self, worker: usize) -> &AtomicUsize {
        if let Some(count) = self.existing(worker) {
            return count;
        }
        let (bucket, _) = Self::location(worker);
        let len = 1usize << bucket;
        let allocation = std::iter::repeat_with(WorkerCount::default)
            .take(len)
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let ptr = Box::into_raw(allocation).cast::<WorkerCount>();
        if self.0[bucket]
            .compare_exchange(
                std::ptr::null_mut(),
                ptr,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_err()
        {
            // SAFETY: a competing publisher won. No reader could obtain our
            // unpublished allocation; restore its original slice layout.
            unsafe { drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(ptr, len))) };
        }
        self.existing(worker).expect("published worker counter")
    }
}

impl Drop for WorkerCounts {
    fn drop(&mut self) {
        for (bucket, slot) in self.0.iter_mut().enumerate() {
            let ptr = *slot.get_mut();
            if !ptr.is_null() {
                // SAFETY: exclusive ownership excludes all counter borrowers.
                unsafe {
                    drop(Box::from_raw(std::ptr::slice_from_raw_parts_mut(
                        ptr,
                        1usize << bucket,
                    )))
                };
            }
        }
    }
}

/// Counts live native-stack and cooperative-segment leases. Idle cached stacks
/// and tasks suspended at a source await do not pin workers.
///
/// The packed bound has two handshake bits, not a blocking writer lock. Only
/// one releaser scans; other releasers never wait for it. Every acquire/release
/// invalidates an in-flight scan. No guard or borrow crosses a context switch.
/// A contested scan leaves a conservative bound while leases remain, and its
/// final handshake (or the last releaser) clears the bound when all leases end.
#[derive(Debug, Default)]
pub(crate) struct WorkerAffinities {
    counts: WorkerCounts,
    bound: AtomicUsize,
    live: AtomicUsize,
    #[cfg(test)]
    queries: AtomicUsize,
    #[cfg(test)]
    updates: AtomicUsize,
    #[cfg(test)]
    scans: AtomicUsize,
}

impl WorkerAffinities {
    pub(crate) fn acquire(&self, worker: usize) -> WorkerAffinity<'_> {
        // Even one counter for every index below this limit would exceed the
        // addressable allocation size; this does not limit realizable pools.
        let bound = worker.checked_add(1).expect("worker index overflow");
        assert!(bound <= usize::MAX >> BOUND_SHIFT, "worker array too large");
        let count = self.counts.get(worker);
        count.fetch_add(1, Ordering::SeqCst);
        self.live.fetch_add(1, Ordering::SeqCst);
        // Logical fetch_max, also preserving/invalidation-marking a scan.
        // A plain fetch_max cannot invalidate acquisition at the SAME bound:
        // an old scan could then lower the bound past this newly live lease.
        self.bound
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |state| {
                Some(
                    ((state >> BOUND_SHIFT).max(bound) << BOUND_SHIFT)
                        | (state & SHRINKING)
                        | INVALIDATED,
                )
            })
            .unwrap();
        #[cfg(test)]
        self.updates.fetch_add(1, Ordering::Relaxed);
        WorkerAffinity {
            registry: self,
            worker,
            count,
        }
    }

    pub(crate) fn required_workers(&self) -> usize {
        #[cfg(test)]
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.bound.load(Ordering::SeqCst) >> BOUND_SHIFT
    }

    fn begin_shrink(&self, worker: usize, last: bool) -> Option<usize> {
        let state = self.bound.load(Ordering::SeqCst);
        if state & SHRINKING != 0 || (!last && state >> BOUND_SHIFT != worker + 1) {
            return None;
        }
        let marked = (state & !FLAGS) | SHRINKING;
        self.bound
            .compare_exchange(state, marked, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| marked)
    }

    fn scan(&self, bound: usize) -> usize {
        for worker in (0..bound).rev() {
            #[cfg(test)]
            self.scans.fetch_add(1, Ordering::Relaxed);
            if self
                .counts
                .existing(worker)
                .is_some_and(|count| count.load(Ordering::SeqCst) != 0)
            {
                return worker + 1;
            }
        }
        0
    }

    fn finish_shrink(&self, marked: usize, bound: usize) {
        if self
            .bound
            .compare_exchange(
                marked,
                bound << BOUND_SHIFT,
                Ordering::SeqCst,
                Ordering::SeqCst,
            )
            .is_ok()
        {
            return;
        }
        // Scan invalidated: do not rescan O(W) under contention. Keep the
        // conservative maximum, except when all leases have ended. Both kinds
        // of writer invalidate after updating `live`, so a last release either
        // makes this CAS retry or sees SHRINKING cleared and clears it itself.
        // CAS retries are contention-dependent; each retry does O(1) work.
        loop {
            let state = self.bound.load(Ordering::SeqCst);
            let marked = state & !INVALIDATED;
            if self
                .bound
                .compare_exchange(state, marked, Ordering::SeqCst, Ordering::SeqCst)
                .is_err()
            {
                continue;
            }
            let next = if self.live.load(Ordering::SeqCst) == 0 {
                0
            } else {
                marked & !FLAGS
            };
            if self
                .bound
                .compare_exchange(marked, next, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return;
            }
        }
    }

    fn release(&self, worker: usize, count: &AtomicUsize) {
        let previous = count.fetch_sub(1, Ordering::SeqCst);
        assert_ne!(previous, 0, "missing native stack affinity");
        let last = self.live.fetch_sub(1, Ordering::SeqCst) == 1;
        self.publish_release(worker, previous, last);
        #[cfg(test)]
        self.updates.fetch_add(1, Ordering::Relaxed);
    }

    fn publish_release(&self, worker: usize, previous: usize, last: bool) {
        self.bound.fetch_or(INVALIDATED, Ordering::SeqCst);
        if (previous == 1 || last)
            && let Some(marked) = self.begin_shrink(worker, last)
        {
            let bound = if last {
                if self.live.load(Ordering::SeqCst) == 0 {
                    0
                } else {
                    marked >> BOUND_SHIFT
                }
            } else {
                self.scan(marked >> BOUND_SHIFT)
            };
            // A last-release candidate is valid only if no acquire invalidated
            // the handshake; an acquire still publishing raises its bound
            // before returning the lease to its caller.
            self.finish_shrink(marked, bound);
        }
    }
}

#[derive(Debug)]
pub(crate) struct WorkerAffinity<'a> {
    registry: &'a WorkerAffinities,
    worker: usize,
    count: &'a AtomicUsize,
}

impl Drop for WorkerAffinity<'_> {
    fn drop(&mut self) {
        self.registry.release(self.worker, self.count);
    }
}

static WORKER_AFFINITIES: std::sync::LazyLock<WorkerAffinities> =
    std::sync::LazyLock::new(WorkerAffinities::default);

/// The outer drive owns the persistent-worker lock, so no previous drive can
/// still create a stack when it reads this bound. Running/nested drives retain
/// their leases; completion and cancellation release them through recycle/drop.
pub(crate) fn required_workers() -> usize {
    WORKER_AFFINITIES.required_workers()
}

#[cfg(test)]
pub(crate) fn affinity_counts_for_test() -> (usize, usize) {
    (
        WORKER_AFFINITIES.queries.load(Ordering::Relaxed),
        WORKER_AFFINITIES.updates.load(Ordering::Relaxed),
    )
}

pub(crate) fn acquire(worker: usize) -> WorkerAffinity<'static> {
    WORKER_AFFINITIES.acquire(worker)
}

impl WorkerAffinity<'_> {
    pub(crate) fn worker(&self) -> usize {
        self.worker
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn affinity_bound_counts_shared_and_sparse_workers() {
        let registry = WorkerAffinities::default();
        assert_eq!(registry.required_workers(), 0);
        let low = registry.acquire(1);
        let high = registry.acquire(1_000_000);
        let shared = registry.acquire(1_000_000);
        assert_eq!(registry.required_workers(), 1_000_001);
        drop(high);
        assert_eq!(registry.required_workers(), 1_000_001);
        drop(shared);
        assert_eq!(registry.required_workers(), 2);
        drop(low);
        assert_eq!(registry.required_workers(), 0);
    }

    #[test]
    fn affinity_queries_do_not_visit_stacks_or_update_counts() {
        for count in [16, 128, 1024] {
            for workers in [1, 8, count] {
                let registry = WorkerAffinities::default();
                let leases: Vec<_> = (0..count).map(|i| registry.acquire(i % workers)).collect();
                let queries = count * 4;
                for _ in 0..queries {
                    assert_eq!(registry.required_workers(), workers);
                }
                assert_eq!(registry.queries.load(Ordering::Relaxed), queries);
                assert_eq!(registry.updates.load(Ordering::Relaxed), count);
                assert_eq!(registry.scans.load(Ordering::Relaxed), 0);
                drop(leases);
                assert_eq!(registry.updates.load(Ordering::Relaxed), count * 2);
                assert_eq!(registry.required_workers(), 0);
                assert_eq!(registry.live.load(Ordering::SeqCst), 0);
            }
        }
    }

    #[test]
    fn affinity_scan_rejects_same_bound_and_lower_worker_acquisition() {
        for new_worker in [1, 7, 15] {
            let registry = WorkerAffinities::default();
            let low = registry.acquire(0);
            let high = registry.acquire(7);
            // Split the final high-worker release at its production scan/CAS
            // boundary to force the otherwise rare stale-scan interleaving.
            assert_eq!(high.count.fetch_sub(1, Ordering::SeqCst), 1);
            registry.live.fetch_sub(1, Ordering::SeqCst);
            registry.bound.fetch_or(INVALIDATED, Ordering::SeqCst);
            std::mem::forget(high);
            let marked = registry.begin_shrink(7, false).unwrap();
            let candidate = registry.scan(marked >> BOUND_SHIFT);
            assert_eq!(candidate, 1);
            let newcomer = registry.acquire(new_worker);
            registry.finish_shrink(marked, candidate);
            assert!(registry.required_workers() > new_worker);
            drop(newcomer);
            drop(low);
            assert_eq!(registry.required_workers(), 0);
        }
    }

    #[test]
    fn affinity_invalidated_scan_clears_when_last_lease_ends() {
        let registry = WorkerAffinities::default();
        let low = registry.acquire(0);
        let high = registry.acquire(7);
        high.count.fetch_sub(1, Ordering::SeqCst);
        registry.live.fetch_sub(1, Ordering::SeqCst);
        registry.bound.fetch_or(INVALIDATED, Ordering::SeqCst);
        std::mem::forget(high);
        let marked = registry.begin_shrink(7, false).unwrap();
        let candidate = registry.scan(marked >> BOUND_SHIFT);
        let newcomer = registry.acquire(3);
        drop(newcomer);
        drop(low); // Cannot scan: the first releaser still owns the handshake.
        registry.finish_shrink(marked, candidate);
        assert_eq!(registry.live.load(Ordering::SeqCst), 0);
        assert_eq!(registry.required_workers(), 0);
    }

    #[test]
    fn affinity_last_release_rechecks_new_acquisition_before_shrink() {
        let registry = WorkerAffinities::default();
        let old = registry.acquire(7);
        let previous = old.count.fetch_sub(1, Ordering::SeqCst);
        let last = registry.live.fetch_sub(1, Ordering::SeqCst) == 1;
        std::mem::forget(old);
        assert!(last);
        let new = registry.acquire(3);
        registry.publish_release(7, previous, last);
        assert!(registry.required_workers() > 3);
        drop(new);
        assert_eq!(registry.required_workers(), 0);
    }

    #[test]
    fn affinity_switch_updates_do_not_scale_with_workers_or_tasks() {
        for workers in [1, 4, 16] {
            for tasks in [100, 1_000, 10_000] {
                let registry = WorkerAffinities::default();
                let anchors: Vec<_> = (0..workers)
                    .map(|worker| registry.acquire(worker))
                    .collect();
                for task in 0..tasks {
                    drop(registry.acquire(task % workers));
                }
                assert_eq!(
                    registry.updates.load(Ordering::Relaxed),
                    workers + tasks * 2
                );
                assert_eq!(registry.scans.load(Ordering::Relaxed), 0);
                assert_eq!(registry.required_workers(), workers);
                drop(anchors);
                assert_eq!(registry.live.load(Ordering::SeqCst), 0);
                assert_eq!(registry.required_workers(), 0);
            }
        }
    }

    #[test]
    fn affinity_concurrent_acquire_release_preserves_live_bound() {
        for workers in [1, 4, 16] {
            let registry = WorkerAffinities::default();
            std::thread::scope(|scope| {
                for worker in 0..workers {
                    let registry = &registry;
                    scope.spawn(move || {
                        for _ in 0..10_000 {
                            let first = registry.acquire(worker);
                            let second = registry.acquire(worker);
                            assert!(registry.required_workers() > worker);
                            drop(first);
                            assert!(registry.required_workers() > worker);
                            drop(second);
                        }
                    });
                }
            });
            assert_eq!(registry.live.load(Ordering::SeqCst), 0);
            assert_eq!(registry.required_workers(), 0);
            for worker in 0..workers {
                assert_eq!(
                    registry
                        .counts
                        .existing(worker)
                        .unwrap()
                        .load(Ordering::SeqCst),
                    0
                );
            }
        }
    }
}
