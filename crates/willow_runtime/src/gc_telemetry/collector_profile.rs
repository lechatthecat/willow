//! Opt-in bounded collector lock, worker, and graph profiling. No I/O and no GC locks under this mutex.
//! Histograms use nanoseconds; bucket i bounds observations by 2^i ns.
use std::sync::{LazyLock, Mutex, MutexGuard, TryLockError};
use std::time::Instant;

pub const KINDS: usize = 5;
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Kind {
    SatbRegistry,
    SatbBuffer,
    SatbSeal,
    SweepWait,
    SweepHold,
}
#[derive(Clone, Copy, Debug)]
pub struct Histogram {
    pub count: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    pub buckets: [u64; 65],
}
impl Default for Histogram {
    fn default() -> Self {
        Self {
            count: 0,
            total_ns: 0,
            max_ns: 0,
            buckets: [0; 65],
        }
    }
}
impl Histogram {
    fn add(&mut self, ns: u64) {
        self.count = self.count.saturating_add(1);
        self.total_ns = self.total_ns.saturating_add(ns);
        self.max_ns = self.max_ns.max(ns);
        let i = if ns == 0 {
            0
        } else {
            (64 - (ns - 1).leading_zeros()) as usize
        };
        self.buckets[i] = self.buckets[i].saturating_add(1);
    }
    pub fn percentile_upper_ns(&self, percent: u64) -> Option<u64> {
        if self.count == 0 || percent == 0 || percent > 100 {
            return None;
        }
        let target = (u128::from(self.count) * u128::from(percent)).div_ceil(100);
        let mut count = 0u128;
        for (i, &n) in self.buckets.iter().enumerate() {
            count += u128::from(n);
            if count >= target {
                return Some(1u64.checked_shl(i as u32).unwrap_or(u64::MAX));
            }
        }
        None
    }
}
#[derive(Clone, Copy, Debug, Default)]
pub struct Snapshot {
    pub intervals: [Histogram; KINDS],
    pub collisions: [u64; KINDS],
}
pub(crate) struct CollectorProfile {
    stats: LazyLock<Mutex<Snapshot>>,
    workers: Mutex<[Worker; 64]>,
}
impl CollectorProfile {
    pub(crate) const fn new() -> Self {
        Self {
            stats: LazyLock::new(|| Mutex::new(Snapshot::default())),
            workers: Mutex::new(
                [Worker {
                    jobs: 0,
                    objects: 0,
                    scanned_bytes: 0,
                    duplicate_claims: 0,
                    steal_attempts: 0,
                    steals: 0,
                    max_queue_depth: 0,
                    cpu_ns: 0,
                    cpu_unavailable: 0,
                    wall_ns: 0,
                    idle_ns: 0,
                }; 64],
            ),
        }
    }
    fn snapshot(&self) -> Snapshot {
        *self.stats.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn record(&self, kind: Kind, ns: u64) {
        self.stats
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .intervals[kind as usize]
            .add(ns);
    }
    fn workers(&self) -> [Worker; 64] {
        *self.workers.lock().unwrap_or_else(|e| e.into_inner())
    }
}
fn default_owner() -> &'static CollectorProfile {
    &super::telemetry().collector_profile
}

pub fn enabled() -> bool {
    crate::runtime::current().diagnostics.gc_profile.one
}
pub fn snapshot() -> Snapshot {
    default_owner().snapshot()
}
pub(crate) fn record(kind: Kind, ns: u64) {
    record_in(crate::runtime::current(), kind, ns);
}

pub(crate) fn record_in(owner: &crate::runtime::Runtime, kind: Kind, ns: u64) {
    if owner.diagnostics.gc_profile.one {
        owner.telemetry.collector_profile.record(kind, ns);
    }
}
pub(crate) struct Span(Kind, Option<(&'static CollectorProfile, Instant)>);
impl Span {
    pub(crate) fn new(kind: Kind) -> Self {
        Self(kind, enabled().then(|| (default_owner(), Instant::now())))
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if let Some((owner, start)) = self.1 {
            owner.record(
                self.0,
                start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            );
        }
    }
}
/// Collision is an observed WouldBlock, not an inferred long acquisition.
/// Poison recovery matches the SATB registries' existing behavior.
pub(crate) fn lock<T>(mutex: &Mutex<T>, kind: Kind) -> MutexGuard<'_, T> {
    if !enabled() {
        return mutex.lock().unwrap_or_else(|e| e.into_inner());
    }
    let started = Instant::now();
    let (guard, collision) = match mutex.try_lock() {
        Ok(guard) => (guard, false),
        Err(TryLockError::Poisoned(e)) => (e.into_inner(), false),
        Err(TryLockError::WouldBlock) => (mutex.lock().unwrap_or_else(|e| e.into_inner()), true),
    };
    // Stop the acquisition timer before taking the statistics lock. Otherwise
    // observer contention would be mislabeled as SATB lock acquisition time.
    let ns = started.elapsed().as_nanos().min(u64::MAX as u128) as u64;
    let mut s = default_owner()
        .stats
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    s.intervals[kind as usize].add(ns);
    if collision {
        s.collisions[kind as usize] = s.collisions[kind as usize].saturating_add(1);
    }
    guard
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collector_profile_owners_isolate_workers_histograms_and_nested_unwind() {
        let first = CollectorProfile::new();
        let second = CollectorProfile::new();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [(&first, 1), (&second, 3)]
                .into_iter()
                .map(|(owner, ns)| {
                    let barrier = &barrier;
                    scope.spawn(crate::runtime::serving(move || {
                        for round in 1..=64 {
                            let job = WorkerJob::begin_in(owner, 0, ns);
                            owner.record(Kind::SweepHold, ns);
                            barrier.wait();
                            drop(job);
                            barrier.wait();
                            assert_eq!(owner.workers()[0].jobs, round);
                            assert_eq!(owner.workers()[0].idle_ns, round * ns);
                            assert_eq!(
                                owner.snapshot().intervals[Kind::SweepHold as usize].total_ns,
                                round * ns
                            );
                        }
                        assert!(LOCAL.get().is_none());
                    }))
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        steal(true); // No active job: no profile to update.
        let outer = WorkerJob::begin_in(&first, 1, 11);
        steal(false);
        graph_in(
            &first,
            super::super::MarkWork {
                objects: 1,
                ..Default::default()
            },
            0,
            1,
        );
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _inner = WorkerJob::begin_in(&second, 2, 22);
            steal(true);
            graph_in(
                &first,
                super::super::MarkWork {
                    objects: 100,
                    ..Default::default()
                },
                0,
                100,
            );
            graph_in(
                &second,
                super::super::MarkWork {
                    objects: 3,
                    ..Default::default()
                },
                0,
                3,
            );
            panic!("nested profile job unwind");
        }));
        assert!(unwind.is_err());
        assert_eq!(LOCAL.get().unwrap().0, 1);
        assert_eq!(LOCAL.get().unwrap().1.idle_ns, 11);
        steal(false);
        drop(outer);
        assert!(LOCAL.get().is_none());
        assert_eq!(first.workers()[1].jobs, 1);
        assert_eq!(first.workers()[2].jobs, 0);
        assert_eq!(second.workers()[1].jobs, 0);
        assert_eq!(second.workers()[2].jobs, 1);
        assert_eq!(first.workers()[1].objects, 1);
        assert_eq!(second.workers()[2].objects, 3);
        assert_eq!(first.workers()[1].steal_attempts, 2);
        assert_eq!(first.workers()[1].steals, 0);
        assert_eq!(second.workers()[2].steal_attempts, 0);
        assert_eq!(second.workers()[2].steals, 1);
    }

    #[test]
    fn m3_histogram_bounds_zero_powers_extremes_and_percentiles() {
        let mut h = Histogram::default();
        assert_eq!(h.percentile_upper_ns(99), None);
        for n in [0, 1, 2, 3, 4, 7, 8, 9, u64::MAX] {
            h.add(n);
        }
        assert_eq!(h.count, 9);
        assert_eq!(h.total_ns, u64::MAX);
        assert_eq!(h.percentile_upper_ns(50), Some(4));
        assert_eq!(h.percentile_upper_ns(99), Some(u64::MAX));
        assert_eq!(h.percentile_upper_ns(101), None);
        for n in 0..4096 {
            let mut h = Histogram::default();
            h.add(n);
            assert!(h.percentile_upper_ns(99).unwrap() >= n);
        }
    }
}

/// Cumulative per-pool-slot diagnostics; slots are reused only after pool shutdown.
#[derive(Clone, Copy, Debug, Default)]
pub struct Worker {
    pub jobs: u64,
    pub objects: u64,
    pub scanned_bytes: u64,
    pub duplicate_claims: u64,
    pub steal_attempts: u64,
    pub steals: u64,
    pub max_queue_depth: u64,
    pub cpu_ns: u64,
    pub cpu_unavailable: u64,
    pub wall_ns: u64,
    pub idle_ns: u64,
}
thread_local! { static LOCAL: std::cell::Cell<Option<(usize, Worker, *const CollectorProfile)>> = const { std::cell::Cell::new(None) }; }
pub fn workers() -> [Worker; 64] {
    default_owner().workers()
}
pub(crate) struct WorkerJob<'a> {
    owner: &'a CollectorProfile,
    previous: Option<(usize, Worker, *const CollectorProfile)>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
    start: Instant,
    cpu: Option<u64>,
}
impl WorkerJob<'static> {
    pub(crate) fn begin(index: usize, idle: u64) -> Option<Self> {
        if !enabled() {
            return None;
        }
        Some(Self::begin_in(default_owner(), index, idle))
    }
}
impl<'a> WorkerJob<'a> {
    pub(crate) fn begin_in(owner: &'a CollectorProfile, index: usize, idle: u64) -> Self {
        let previous = LOCAL.replace(Some((
            index,
            Worker {
                jobs: 1,
                idle_ns: idle,
                ..Default::default()
            },
            owner,
        )));
        Self {
            owner,
            previous,
            _thread: std::marker::PhantomData,
            start: Instant::now(),
            cpu: super::workers::thread_cpu_ns(),
        }
    }
}
impl Drop for WorkerJob<'_> {
    fn drop(&mut self) {
        let (index, mut w, _) = LOCAL
            .replace(self.previous)
            .expect("worker profile ownership");
        w.wall_ns = self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        match self
            .cpu
            .zip(super::workers::thread_cpu_ns())
            .and_then(|(a, b)| b.checked_sub(a))
        {
            Some(cpu) => w.cpu_ns = cpu,
            None => w.cpu_unavailable = 1,
        }
        let mut workers = self.owner.workers.lock().unwrap_or_else(|e| e.into_inner());
        let total = &mut workers[index];
        macro_rules! add { ($($f:ident),*) => { $(total.$f = total.$f.saturating_add(w.$f);)* }; }
        add!(
            jobs,
            objects,
            scanned_bytes,
            duplicate_claims,
            steal_attempts,
            steals,
            cpu_ns,
            cpu_unavailable,
            wall_ns,
            idle_ns
        );
        total.max_queue_depth = total.max_queue_depth.max(w.max_queue_depth);
    }
}
pub(crate) fn graph(work: super::MarkWork, duplicates: u64, depth: u64) {
    graph_for_runtime(crate::runtime::current(), work, duplicates, depth);
}

pub(crate) fn graph_for_runtime(
    runtime: &crate::runtime::Runtime,
    work: super::MarkWork,
    duplicates: u64,
    depth: u64,
) {
    if runtime.diagnostics.gc_profile.one {
        graph_in(
            &runtime.telemetry.collector_profile,
            work,
            duplicates,
            depth,
        );
    }
}

fn graph_in(owner: &CollectorProfile, work: super::MarkWork, duplicates: u64, depth: u64) {
    if let Some((index, mut w, registered)) = LOCAL.get()
        && std::ptr::eq(registered, owner)
    {
        w.objects = w.objects.saturating_add(work.objects);
        w.scanned_bytes = w.scanned_bytes.saturating_add(work.scanned_bytes);
        w.duplicate_claims = w.duplicate_claims.saturating_add(duplicates);
        w.max_queue_depth = w.max_queue_depth.max(depth);
        LOCAL.set(Some((index, w, registered)));
    }
}

pub(crate) fn steal(success: bool) {
    // Job creation gates profiling. Its borrowed binding selects the owner,
    // including nested jobs, without consulting the process default.
    if let Some((index, mut w, registered)) = LOCAL.get() {
        if success {
            w.steals = w.steals.saturating_add(1);
        } else {
            w.steal_attempts = w.steal_attempts.saturating_add(1);
        }
        LOCAL.set(Some((index, w, registered)));
    }
}
