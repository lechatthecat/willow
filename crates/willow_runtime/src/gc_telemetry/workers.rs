//! Coherent cumulative worker diagnostics. One update per completed worker job,
//! never a shared telemetry lock per traced object. CPU time is measured on the
//! worker itself; wall time is elapsed job time, not a claim of CPU consumption.
use std::sync::Mutex;
use std::time::Instant;

pub const COUNTERS_VALID: u64 = 1;
pub const CPU_COMPLETE: u64 = 2;
pub const SATURATED: u64 = 4;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MarkWorkerStatsV2 {
    pub flags: u64,
    pub completed_jobs: u64,
    pub wall_ns: u64,
    pub cpu_ns: u64,
    pub cpu_samples: u64,
    pub cpu_unavailable: u64,
    pub marker_panics: u64,
    pub spawn_failures: u64,
    pub join_timeouts: u64,
    pub fallback_cycles: u64,
    pub drop_hook_panics: u64,
}

impl Default for MarkWorkerStatsV2 {
    fn default() -> Self {
        Self {
            flags: COUNTERS_VALID | CPU_COMPLETE,
            completed_jobs: 0,
            wall_ns: 0,
            cpu_ns: 0,
            cpu_samples: 0,
            cpu_unavailable: 0,
            marker_panics: 0,
            spawn_failures: 0,
            join_timeouts: 0,
            fallback_cycles: 0,
            drop_hook_panics: 0,
        }
    }
}

pub(crate) struct WorkerTelemetry {
    stats: std::sync::LazyLock<Mutex<MarkWorkerStatsV2>>,
    discoveries: Mutex<MarkDiscoveryStats>,
}
impl WorkerTelemetry {
    pub(crate) const fn new() -> Self {
        Self {
            stats: std::sync::LazyLock::new(|| Mutex::new(MarkWorkerStatsV2::default())),
            discoveries: Mutex::new(MarkDiscoveryStats {
                pointer_checks: 0,
                duplicates: 0,
                unindexed: 0,
            }),
        }
    }
    pub(crate) fn snapshot(&self) -> MarkWorkerStatsV2 {
        *self.stats.lock().unwrap_or_else(|p| p.into_inner())
    }
}
fn default_owner() -> &'static WorkerTelemetry {
    &super::telemetry().workers
}
pub fn snapshot() -> MarkWorkerStatsV2 {
    default_owner().snapshot()
}

/// Cumulative discovery diagnostics, without changing either versioned C ABI.
/// Unindexed candidates include legitimate post-snapshot allocations; they are
/// not a count of corrupt pointers. Checked builds validate them at remark.
#[derive(Clone, Copy, Debug, Default)]
pub struct MarkDiscoveryStats {
    pub pointer_checks: u64,
    pub duplicates: u64,
    pub unindexed: u64,
}
impl WorkerTelemetry {
    pub(crate) fn discovery_snapshot(&self) -> MarkDiscoveryStats {
        *self.discoveries.lock().unwrap_or_else(|p| p.into_inner())
    }
    pub(crate) fn record_discoveries(&self, checks: u64, duplicates: u64, unindexed: u64) {
        let mut stats = self.discoveries.lock().unwrap_or_else(|p| p.into_inner());
        stats.pointer_checks = stats.pointer_checks.saturating_add(checks);
        stats.duplicates = stats.duplicates.saturating_add(duplicates);
        stats.unindexed = stats.unindexed.saturating_add(unindexed);
    }
}
pub fn discovery_snapshot() -> MarkDiscoveryStats {
    default_owner().discovery_snapshot()
}

impl MarkWorkerStatsV2 {
    /// Measured CPU / active job wall time, in parts per million. This is
    /// active worker duty, excluding parked time; unavailable CPU is explicit.
    pub fn active_duty_ppm(&self) -> Option<u64> {
        if self.flags & CPU_COMPLETE == 0 || self.wall_ns == 0 {
            return None;
        }
        Some(
            ((u128::from(self.cpu_ns) * 1_000_000 / u128::from(self.wall_ns)).min(1_000_000))
                as u64,
        )
    }
}

fn add(field: &mut u64, increment: u64) -> bool {
    match field.checked_add(increment) {
        Some(value) => {
            *field = value;
            false
        }
        None => {
            *field = u64::MAX;
            true
        }
    }
}

pub(crate) enum Failure {
    MarkerPanic,
    Spawn,
    JoinTimeout,
    Fallback,
    DropPanic,
}

impl WorkerTelemetry {
    pub(crate) fn record_failure(&self, failure: Failure) {
        let mut stats = self.stats.lock().unwrap_or_else(|p| p.into_inner());
        let field = match failure {
            Failure::MarkerPanic => &mut stats.marker_panics,
            Failure::Spawn => &mut stats.spawn_failures,
            Failure::JoinTimeout => &mut stats.join_timeouts,
            Failure::Fallback => &mut stats.fallback_cycles,
            Failure::DropPanic => &mut stats.drop_hook_panics,
        };
        if add(field, 1) {
            stats.flags |= SATURATED;
        }
    }
}
pub(crate) fn record_failure(failure: Failure) {
    default_owner().record_failure(failure);
}

pub(crate) struct JobMeasurement<'a> {
    owner: &'a WorkerTelemetry,
    busy: Option<super::measurement::BusyWorker<'a>>,
    // Creation and completion must sample the same thread CPU clock.
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
    start: Instant,
    cpu: Option<u64>,
}
impl JobMeasurement<'static> {
    pub(crate) fn begin() -> Self {
        Self::begin_in(super::telemetry())
    }
}
impl<'a> JobMeasurement<'a> {
    pub(crate) fn begin_in(owner: &'a super::Telemetry) -> Self {
        let busy = owner.measurements.worker_busy();
        Self {
            owner: &owner.workers,
            busy: Some(busy),
            _thread: std::marker::PhantomData,
            start: Instant::now(),
            cpu: thread_cpu_ns(),
        }
    }
}
impl Drop for JobMeasurement<'_> {
    fn drop(&mut self) {
        let cpu = self
            .cpu
            .zip(thread_cpu_ns())
            .and_then(|(a, b)| b.checked_sub(a));
        let wall = self.start.elapsed().as_nanos();
        let mut stats = self.owner.stats.lock().unwrap_or_else(|p| p.into_inner());
        let mut saturated = wall > u64::MAX as u128;
        saturated |= add(&mut stats.completed_jobs, 1);
        saturated |= add(&mut stats.wall_ns, wall.min(u64::MAX as u128) as u64);
        if let Some(cpu) = cpu {
            saturated |= add(&mut stats.cpu_ns, cpu);
            saturated |= add(&mut stats.cpu_samples, 1);
        } else {
            stats.flags &= !CPU_COMPLETE;
            saturated |= add(&mut stats.cpu_unavailable, 1);
        }
        if saturated {
            stats.flags |= SATURATED;
        }
        drop(self.busy.take());
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn thread_cpu_ns() -> Option<u64> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: valid output storage and a calling-thread clock selector.
    if unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) } != 0 {
        return None;
    }
    u64::try_from(time.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u64::try_from(time.tv_nsec).ok()?)
}

#[cfg(target_os = "windows")]
pub fn thread_cpu_ns() -> Option<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    impl FileTime {
        fn ticks(&self) -> u64 {
            (u64::from(self.high) << 32) | u64::from(self.low)
        }
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> *mut std::ffi::c_void;
        fn GetThreadTimes(
            thread: *mut std::ffi::c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }
    let (mut creation, mut exit, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // SAFETY: the pseudo-handle refers to this thread; all FILETIME outputs are
    // initialized and have the Windows ABI's two-u32 layout.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    kernel.ticks().checked_add(user.ticks())?.checked_mul(100)
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn thread_cpu_ns() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_job_owners_isolate_completion_failure_discoveries_and_unwind() {
        let first = super::super::Telemetry::new();
        let second = super::super::Telemetry::new();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [(&first, 1), (&second, 3)]
                .into_iter()
                .map(|(owner, stride)| {
                    let barrier = &barrier;
                    scope.spawn(crate::runtime::serving(move || {
                        for round in 1..=64 {
                            let job = JobMeasurement::begin_in(owner);
                            owner.workers.record_discoveries(stride, 1, 0);
                            owner.workers.record_failure(Failure::Fallback);
                            barrier.wait();
                            assert_eq!(owner.measurements.snapshot().worker_busy, 1);
                            drop(job);
                            barrier.wait();
                            assert_eq!(owner.measurements.snapshot().worker_busy, 0);
                            assert_eq!(owner.workers.snapshot().completed_jobs, round);
                            assert_eq!(owner.workers.snapshot().fallback_cycles, round);
                            assert_eq!(
                                owner.workers.discovery_snapshot().pointer_checks,
                                round * stride
                            );
                        }
                    }))
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _job = JobMeasurement::begin_in(&first);
            panic!("owned worker job unwind");
        }));
        assert!(unwind.is_err());
        assert_eq!(first.measurements.snapshot().worker_busy, 0);
        assert_eq!(first.workers.snapshot().completed_jobs, 65);
        assert_eq!(second.workers.snapshot().completed_jobs, 64);
        assert_eq!(first.workers.discovery_snapshot().pointer_checks, 64);
        assert_eq!(second.workers.discovery_snapshot().pointer_checks, 192);
        first.workers.stats.lock().unwrap().spawn_failures = u64::MAX;
        first.workers.record_failure(Failure::Spawn);
        assert_ne!(first.workers.snapshot().flags & SATURATED, 0);
        assert_eq!(second.workers.snapshot().flags & SATURATED, 0);
    }

    #[test]
    fn thread_cpu_measurement_is_monotonic_when_available() {
        let before = thread_cpu_ns();
        let mut value = 1u64;
        for _ in 0..10000 {
            value = std::hint::black_box(value.wrapping_mul(3));
        }
        std::hint::black_box(value);
        let after = thread_cpu_ns();
        if let (Some(before), Some(after)) = (before, after) {
            assert!(after >= before);
        }
    }
    #[test]
    fn worker_duty_requires_complete_cpu_and_nonzero_wall_time() {
        let mut stats = MarkWorkerStatsV2::default();
        assert_eq!(stats.active_duty_ppm(), None);
        stats.wall_ns = 100;
        stats.cpu_ns = 25;
        assert_eq!(stats.active_duty_ppm(), Some(250_000));
        stats.cpu_ns = 200;
        assert_eq!(stats.active_duty_ppm(), Some(1_000_000));
        stats.flags &= !CPU_COMPLETE;
        assert_eq!(stats.active_duty_ppm(), None);
    }
    #[test]
    fn overflow_is_saturated_and_reported() {
        let mut value = u64::MAX - 1;
        assert!(!add(&mut value, 1));
        assert!(add(&mut value, 1));
        assert_eq!(value, u64::MAX);
    }
}
