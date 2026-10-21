//! Shared worker-lifetime leases for native stacks and cooperative segments.
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Counts live native-stack and cooperative-segment leases. Idle cached stacks
/// and tasks suspended at a source await do not pin workers.
/// Writers never hold this lock across a context switch or acquire task locks.
#[derive(Debug, Default)]
pub(crate) struct WorkerAffinities {
    counts: Mutex<BTreeMap<usize, usize>>,
    bound: AtomicUsize,
    #[cfg(test)]
    queries: AtomicUsize,
    #[cfg(test)]
    updates: AtomicUsize,
}

impl WorkerAffinities {
    pub(crate) fn acquire(&self, worker: usize) -> WorkerAffinity<'_> {
        let mut counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        *counts.entry(worker).or_default() += 1;
        self.bound.fetch_max(worker + 1, Ordering::Release);
        #[cfg(test)]
        self.updates.fetch_add(1, Ordering::Relaxed);
        WorkerAffinity {
            registry: self,
            worker,
        }
    }

    pub(crate) fn required_workers(&self) -> usize {
        #[cfg(test)]
        self.queries.fetch_add(1, Ordering::Relaxed);
        self.bound.load(Ordering::Acquire)
    }
}

#[derive(Debug)]
pub(crate) struct WorkerAffinity<'a> {
    registry: &'a WorkerAffinities,
    worker: usize,
}

impl Drop for WorkerAffinity<'_> {
    fn drop(&mut self) {
        let mut counts = self
            .registry
            .counts
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let count = counts
            .get_mut(&self.worker)
            .expect("missing native stack affinity");
        *count -= 1;
        if *count == 0 {
            counts.remove(&self.worker);
            if self.registry.bound.load(Ordering::Relaxed) == self.worker + 1 {
                self.registry.bound.store(
                    counts.last_key_value().map_or(0, |(&worker, _)| worker + 1),
                    Ordering::Release,
                );
            }
        }
        #[cfg(test)]
        self.registry.updates.fetch_add(1, Ordering::Relaxed);
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
        assert_eq!(registry.counts.lock().unwrap().len(), 2);
        drop(high);
        assert_eq!(registry.required_workers(), 1_000_001);
        drop(shared);
        assert_eq!(registry.required_workers(), 2);
        drop(low);
        assert_eq!(registry.required_workers(), 0);
    }

    #[test]
    fn affinity_queries_do_not_visit_stacks_or_update_counts() {
        println!("stacks,workers,queries,updates");
        for count in [16, 128, 1024] {
            for workers in [1, 8, count] {
                let registry = WorkerAffinities::default();
                let leases: Vec<_> = (0..count).map(|i| registry.acquire(i % workers)).collect();
                let queries = count * 4;
                // Holding the writer lock also proves queries cannot inspect
                // the counts map, regardless of its size or shape.
                let counts = registry.counts.lock().unwrap();
                for _ in 0..queries {
                    assert_eq!(registry.required_workers(), workers);
                }
                drop(counts);
                assert_eq!(registry.queries.load(Ordering::Relaxed), queries);
                assert_eq!(registry.updates.load(Ordering::Relaxed), count);
                drop(leases);
                assert_eq!(registry.updates.load(Ordering::Relaxed), count * 2);
                assert_eq!(registry.bound.load(Ordering::Relaxed), 0);
                println!("{count},{workers},{queries},{}", count * 2);
            }
        }
    }
}
