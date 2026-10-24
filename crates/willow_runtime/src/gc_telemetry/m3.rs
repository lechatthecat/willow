//! Opt-in bounded M3 diagnostics. No I/O and no GC locks under this mutex.
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
static ON: LazyLock<bool> =
    LazyLock::new(|| std::env::var("WILLOW_GC_PROFILE").as_deref() == Ok("1"));
static STATS: LazyLock<Mutex<Snapshot>> = LazyLock::new(|| Mutex::new(Snapshot::default()));
pub fn enabled() -> bool {
    *ON
}
pub fn snapshot() -> Snapshot {
    *STATS.lock().unwrap_or_else(|e| e.into_inner())
}
pub(crate) fn record(kind: Kind, ns: u64) {
    if enabled() {
        STATS.lock().unwrap_or_else(|e| e.into_inner()).intervals[kind as usize].add(ns);
    }
}
pub(crate) struct Span(Kind, Option<Instant>);
impl Span {
    pub(crate) fn new(kind: Kind) -> Self {
        Self(kind, enabled().then(Instant::now))
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        if let Some(start) = self.1 {
            record(
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
    let mut s = STATS.lock().unwrap_or_else(|e| e.into_inner());
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
static WORKERS: Mutex<[Worker; 64]> = Mutex::new(
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
);
thread_local! { static LOCAL: std::cell::Cell<Option<(usize, Worker)>> = const { std::cell::Cell::new(None) }; }
pub fn workers() -> [Worker; 64] {
    *WORKERS.lock().unwrap_or_else(|e| e.into_inner())
}
pub(crate) struct WorkerJob {
    start: Instant,
    cpu: Option<u64>,
}
impl WorkerJob {
    pub(crate) fn begin(index: usize, idle: u64) -> Option<Self> {
        if !enabled() {
            return None;
        }
        LOCAL.set(Some((
            index,
            Worker {
                jobs: 1,
                idle_ns: idle,
                ..Default::default()
            },
        )));
        Some(Self {
            start: Instant::now(),
            cpu: super::workers::thread_cpu_ns(),
        })
    }
}
impl Drop for WorkerJob {
    fn drop(&mut self) {
        let (index, mut w) = LOCAL.take().expect("worker profile ownership");
        w.wall_ns = self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64;
        match self
            .cpu
            .zip(super::workers::thread_cpu_ns())
            .and_then(|(a, b)| b.checked_sub(a))
        {
            Some(cpu) => w.cpu_ns = cpu,
            None => w.cpu_unavailable = 1,
        }
        let mut workers = WORKERS.lock().unwrap_or_else(|e| e.into_inner());
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
    if !enabled() {
        return;
    }
    if let Some((index, mut w)) = LOCAL.get() {
        w.objects = w.objects.saturating_add(work.objects);
        w.scanned_bytes = w.scanned_bytes.saturating_add(work.scanned_bytes);
        w.duplicate_claims = w.duplicate_claims.saturating_add(duplicates);
        w.max_queue_depth = w.max_queue_depth.max(depth);
        LOCAL.set(Some((index, w)));
    }
}
pub(crate) fn steal(success: bool) {
    if !enabled() {
        return;
    }
    if let Some((index, mut w)) = LOCAL.get() {
        if success {
            w.steals = w.steals.saturating_add(1);
        } else {
            w.steal_attempts = w.steal_attempts.saturating_add(1);
        }
        LOCAL.set(Some((index, w)));
    }
}
