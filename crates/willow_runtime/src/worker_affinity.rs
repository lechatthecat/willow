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

/// An instance supplies observation without changing affinity synchronization.
/// The unit observer has no state or work and is erased by monomorphization.
pub(crate) trait AffinityObservation {
    fn query(&self) {}
    fn update(&self) {}
    fn scan(&self) {}
}

impl AffinityObservation for () {}

#[cfg(not(test))]
pub(crate) type RuntimeObservation = ();
#[cfg(test)]
pub(crate) type RuntimeObservation = tests::Counters;

/// Counts live native-stack and cooperative-segment leases. Idle cached stacks
/// and tasks suspended at a source await do not pin workers.
///
/// The packed bound has two handshake bits, not a blocking writer lock. Only
/// one releaser scans; other releasers never wait for it. Every acquire/release
/// invalidates an in-flight scan. No guard or borrow crosses a context switch.
/// A contested scan leaves a conservative bound while leases remain, and its
/// final handshake (or the last releaser) clears the bound when all leases end.
#[derive(Debug, Default)]
pub(crate) struct WorkerAffinities<O: AffinityObservation = ()> {
    counts: WorkerCounts,
    bound: AtomicUsize,
    live: AtomicUsize,
    observation: O,
}

impl<O: AffinityObservation> WorkerAffinities<O> {
    pub(crate) fn acquire(&self, worker: usize) -> WorkerAffinity<'_, O> {
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
        self.observation.update();
        WorkerAffinity {
            registry: RegistryLease::borrowed(self),
            worker,
            count: count as *const AtomicUsize,
        }
    }

    pub(crate) fn required_workers(&self) -> usize {
        self.observation.query();
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
            self.observation.scan();
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
        self.observation.update();
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
pub(crate) struct WorkerAffinity<'a, O: AffinityObservation = RuntimeObservation> {
    registry: RegistryLease<'a, O>,
    worker: usize,
    count: *const AtomicUsize,
}

impl<O: AffinityObservation> Drop for WorkerAffinity<'_, O> {
    fn drop(&mut self) {
        self.registry.get().release(self.worker, self.count());
    }
}

/// A borrowed registry, or one retaining a strong `Arc` count (low tag bit).
/// One tagged word keeps the lease, and the task slot holding it, as small as
/// a plain borrow, and lets `Option<WorkerAffinity>` use the pointer niche.
#[derive(Debug)]
struct RegistryLease<'a, O: AffinityObservation> {
    tagged: std::ptr::NonNull<WorkerAffinities<O>>,
    _registry: std::marker::PhantomData<&'a WorkerAffinities<O>>,
}

const SHARED_LEASE: usize = 1;

impl<'a, O: AffinityObservation> RegistryLease<'a, O> {
    fn borrowed(registry: &'a WorkerAffinities<O>) -> Self {
        Self {
            tagged: std::ptr::NonNull::from(registry),
            _registry: std::marker::PhantomData,
        }
    }

    fn shared(registry: std::sync::Arc<WorkerAffinities<O>>) -> Self {
        // `Arc` allocations are word aligned, leaving the low bit free.
        let raw = std::sync::Arc::into_raw(registry).cast_mut();
        Self {
            tagged: std::ptr::NonNull::new(raw.map_addr(|addr| addr | SHARED_LEASE))
                .expect("Arc pointers are non-null"),
            _registry: std::marker::PhantomData,
        }
    }

    #[inline]
    fn get(&self) -> &WorkerAffinities<O> {
        // SAFETY: a borrowed registry outlives 'a; a shared one is retained
        // by this lease until drop.
        unsafe { &*self.tagged.as_ptr().map_addr(|addr| addr & !SHARED_LEASE) }
    }
}

impl<O: AffinityObservation> Drop for RegistryLease<'_, O> {
    #[inline]
    fn drop(&mut self) {
        if self.tagged.as_ptr().addr() & SHARED_LEASE != 0 {
            // SAFETY: `shared` produced this pointer from `Arc::into_raw`.
            drop(unsafe { std::sync::Arc::from_raw(self.get()) });
        }
    }
}

// Counter buckets remain at stable addresses until the borrowed/shared registry
// dies. The lease retains that registry, and only atomic operations access count.
unsafe impl<O: AffinityObservation + Send + Sync> Send for WorkerAffinity<'_, O> {}
unsafe impl<O: AffinityObservation + Send + Sync> Sync for WorkerAffinity<'_, O> {}

/// A scheduler retains private registries without adding reference-count work
/// to the process-default stack switch path.
#[derive(Clone, Debug)]
pub(crate) enum Registry {
    Default,
    Private(std::sync::Arc<WorkerAffinities<RuntimeObservation>>),
}

impl Registry {
    pub(crate) fn for_runtime(owner: &crate::runtime::Runtime) -> Self {
        if std::ptr::eq(owner, crate::runtime::current()) {
            Self::Default
        } else {
            Self::Private(std::sync::Arc::clone(&owner.affinities))
        }
    }

    pub(crate) fn acquire(&self, worker: usize) -> WorkerAffinity<'static> {
        match self {
            Self::Default => crate::runtime::current().affinities.acquire(worker),
            Self::Private(registry) => acquire_from(registry, worker),
        }
    }
}

pub(crate) fn acquire_in(
    owner: &crate::runtime::Runtime,
    worker: usize,
) -> WorkerAffinity<'static> {
    if std::ptr::eq(owner, crate::runtime::current()) {
        crate::runtime::current().affinities.acquire(worker)
    } else {
        acquire_from(&owner.affinities, worker)
    }
}

pub(crate) fn acquire_from(
    registry: &std::sync::Arc<WorkerAffinities<RuntimeObservation>>,
    worker: usize,
) -> WorkerAffinity<'static> {
    let retained = std::sync::Arc::clone(registry);
    let lease = registry.acquire(worker);
    let count = lease.count;
    // Transfer the live acquisition to a lease retaining the private registry.
    std::mem::forget(lease);
    WorkerAffinity {
        registry: RegistryLease::shared(retained),
        worker,
        count,
    }
}

/// The outer drive owns the persistent-worker lock, so no previous drive can
/// still create a stack when it reads this bound. Running/nested drives retain
/// their leases; completion and cancellation release them through recycle/drop.
#[cfg(test)]
pub(crate) fn required_workers() -> usize {
    crate::runtime::current().affinities.required_workers()
}

#[cfg(test)]
pub(crate) use tests::affinity_counts_for_test;

impl<O: AffinityObservation> WorkerAffinity<'_, O> {
    fn count(&self) -> &AtomicUsize {
        // SAFETY: registry ownership retains the published counter bucket.
        unsafe { &*self.count }
    }

    pub(crate) fn worker(&self) -> usize {
        self.worker
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    pub(crate) struct Counters {
        queries: AtomicUsize,
        updates: AtomicUsize,
        scans: AtomicUsize,
    }

    impl AffinityObservation for Counters {
        fn query(&self) {
            self.queries.fetch_add(1, Ordering::Relaxed);
        }

        fn update(&self) {
            self.updates.fetch_add(1, Ordering::Relaxed);
        }

        fn scan(&self) {
            self.scans.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn affinity_counts_for_test() -> (usize, usize) {
        (
            crate::runtime::current()
                .affinities
                .observation
                .queries
                .load(Ordering::Relaxed),
            crate::runtime::current()
                .affinities
                .observation
                .updates
                .load(Ordering::Relaxed),
        )
    }

    #[test]
    fn private_affinity_leases_retain_only_their_registry() {
        let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let weak = std::sync::Arc::downgrade(&first.affinities);
        let lease = acquire_from(&first.affinities, 7);
        let peer = acquire_from(&second.affinities, 1);
        assert_eq!(first.affinities.required_workers(), 8);
        assert_eq!(second.affinities.required_workers(), 2);
        drop(first);
        assert!(weak.upgrade().is_some());
        drop(lease);
        assert!(weak.upgrade().is_none());
        assert_eq!(second.affinities.required_workers(), 2);
        drop(peer);
        assert_eq!(second.affinities.required_workers(), 0);
    }

    #[test]
    fn no_op_and_injected_observation_preserve_affinity_lifetimes() {
        for worker in [0, 7, 1023] {
            let plain = WorkerAffinities::<()>::default();
            let observed = WorkerAffinities::<Counters>::default();
            let first = plain.acquire(worker);
            let second = observed.acquire(worker);
            assert_eq!(plain.required_workers(), worker + 1);
            assert_eq!(observed.required_workers(), worker + 1);
            assert_eq!(observed.observation.updates.load(Ordering::Relaxed), 1);
            drop(first);
            // Releasing a different instance cannot release this lease.
            assert_eq!(plain.required_workers(), 0);
            assert_eq!(observed.required_workers(), worker + 1);
            drop(second);
            assert_eq!(observed.required_workers(), 0);
            assert_eq!(observed.observation.updates.load(Ordering::Relaxed), 2);
        }
    }

    #[test]
    fn affinity_bound_counts_shared_and_sparse_workers() {
        let registry = WorkerAffinities::<Counters>::default();
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
                let registry = WorkerAffinities::<Counters>::default();
                let leases: Vec<_> = (0..count).map(|i| registry.acquire(i % workers)).collect();
                let queries = count * 4;
                for _ in 0..queries {
                    assert_eq!(registry.required_workers(), workers);
                }
                assert_eq!(
                    registry.observation.queries.load(Ordering::Relaxed),
                    queries
                );
                assert_eq!(registry.observation.updates.load(Ordering::Relaxed), count);
                assert_eq!(registry.observation.scans.load(Ordering::Relaxed), 0);
                drop(leases);
                assert_eq!(
                    registry.observation.updates.load(Ordering::Relaxed),
                    count * 2
                );
                assert_eq!(registry.required_workers(), 0);
                assert_eq!(registry.live.load(Ordering::SeqCst), 0);
            }
        }
    }

    #[test]
    fn affinity_scan_rejects_same_bound_and_lower_worker_acquisition() {
        for new_worker in [1, 7, 15] {
            let registry = WorkerAffinities::<Counters>::default();
            let low = registry.acquire(0);
            let high = registry.acquire(7);
            // Split the final high-worker release at its production scan/CAS
            // boundary to force the otherwise rare stale-scan interleaving.
            assert_eq!(high.count().fetch_sub(1, Ordering::SeqCst), 1);
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
        let registry = WorkerAffinities::<Counters>::default();
        let low = registry.acquire(0);
        let high = registry.acquire(7);
        high.count().fetch_sub(1, Ordering::SeqCst);
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
        let registry = WorkerAffinities::<Counters>::default();
        let old = registry.acquire(7);
        let previous = old.count().fetch_sub(1, Ordering::SeqCst);
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
                let registry = WorkerAffinities::<Counters>::default();
                let anchors: Vec<_> = (0..workers)
                    .map(|worker| registry.acquire(worker))
                    .collect();
                for task in 0..tasks {
                    drop(registry.acquire(task % workers));
                }
                assert_eq!(
                    registry.observation.updates.load(Ordering::Relaxed),
                    workers + tasks * 2
                );
                assert_eq!(registry.observation.scans.load(Ordering::Relaxed), 0);
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
            let registry = WorkerAffinities::<Counters>::default();
            std::thread::scope(|scope| {
                for worker in 0..workers {
                    let registry = &registry;
                    scope.spawn(crate::runtime::serving(move || {
                        for _ in 0..10_000 {
                            let first = registry.acquire(worker);
                            let second = registry.acquire(worker);
                            assert!(registry.required_workers() > worker);
                            drop(first);
                            assert!(registry.required_workers() > worker);
                            drop(second);
                        }
                    }));
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
