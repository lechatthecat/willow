//! V3 groups: time in ns, memory in bytes, counts dimensionless. No heap pointers.
use super::{MarkWork, elapsed_ns};
use std::sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
};
use std::time::Instant;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct GcMemoryV3 {
    pub managed_bytes: u64,
    pub external_bytes: u64,
    pub old_reserved_bytes: u64,
    pub young_reserved_bytes: u64,
    pub pinned_bytes: u64,
    pub fragmentation_bytes: u64,
    /// Live-byte metadata in pending regions/chunks, including live candidates.
    /// Approximate during a partial sweep; never an estimate of dead bytes.
    pub pending_sweep_bytes: u64,
    /// Free old-region holes and bump tails still quarantined, unavailable
    /// to allocation. Does not include quarantined native/frame storage.
    pub quarantined_bytes: u64,
    /// Old-region free holes and bump tails available to the shared allocator.
    /// Excludes quarantined storage, large-object tails and owned LAB capacity.
    pub reusable_bytes: u64,
    /// Previous whole-heap major retention sample, not current occupied bytes.
    pub last_major_retained_bytes: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MeasurementsV3 {
    pub root_scan_ns: u64,
    pub root_scan_samples: u64,
    /// Minor tracing/copying are fused; this measures the fused traversal.
    pub evacuation_ns: u64,
    pub evacuation_samples: u64,
    /// Callback return through gate clear/notification/coordination release.
    pub resume_ns: u64,
    pub resume_samples: u64,
    pub mark_cpu_ns: u64,
    pub mark_cpu_missing_cycles: u64,
    /// Assist intervals overlap mark CPU and include wrapper overhead.
    /// Do not add this value to mark CPU.
    pub assist_cpu_ns: u64,
    pub assist_cpu_missing_samples: u64,
    pub assist_samples: u64,
    pub reclaimed_bytes: u64,
    pub worker_threads: u64,
    pub worker_busy: u64,
    pub worker_idle: u64,
}
pub(crate) struct Measurements {
    stats: Mutex<MeasurementsV3>,
    threads: AtomicU64,
    busy: AtomicU64,
}
impl Measurements {
    pub(crate) const fn new() -> Self {
        Self {
            stats: Mutex::new(MeasurementsV3 {
                root_scan_ns: 0,
                root_scan_samples: 0,
                evacuation_ns: 0,
                evacuation_samples: 0,
                resume_ns: 0,
                resume_samples: 0,
                mark_cpu_ns: 0,
                mark_cpu_missing_cycles: 0,
                assist_cpu_ns: 0,
                assist_cpu_missing_samples: 0,
                assist_samples: 0,
                reclaimed_bytes: 0,
                worker_threads: 0,
                worker_busy: 0,
                worker_idle: 0,
            }),
            threads: AtomicU64::new(0),
            busy: AtomicU64::new(0),
        }
    }
    pub(crate) fn snapshot(&self) -> MeasurementsV3 {
        let mut stats = *self.stats.lock().unwrap_or_else(|p| p.into_inner());
        stats.worker_threads = self.threads.load(Ordering::Acquire);
        stats.worker_busy = self.busy.load(Ordering::Acquire);
        stats.worker_idle = stats.worker_threads.saturating_sub(stats.worker_busy);
        stats
    }
    pub(crate) fn completed(&self, work: MarkWork, reclaimed: u64) {
        let mut s = self.stats.lock().unwrap_or_else(|p| p.into_inner());
        s.mark_cpu_ns = s.mark_cpu_ns.saturating_add(work.cpu_ns);
        s.mark_cpu_missing_cycles = s
            .mark_cpu_missing_cycles
            .saturating_add(u64::from(work.cpu_incomplete));
        s.reclaimed_bytes = s.reclaimed_bytes.saturating_add(reclaimed);
    }
    pub(crate) fn reset(&self) {
        *self.stats.lock().unwrap_or_else(|p| p.into_inner()) = MeasurementsV3::default();
    }
}
fn default_owner() -> &'static Measurements {
    &super::telemetry().measurements
}
pub fn snapshot() -> MeasurementsV3 {
    default_owner().snapshot()
}
pub(crate) fn reset() {
    default_owner().reset();
}
pub(crate) enum Phase {
    Roots,
    Evacuation,
    Resume,
    Assist,
}
pub(crate) struct Span<'a> {
    owner: &'a Measurements,
    phase: Phase,
    start: Instant,
    cpu: Option<u64>,
    _interval: super::observations::Span<'a>,
    _exclusive: Option<super::pause::Span<'a>>,
}
impl Span<'static> {
    pub(crate) fn new(phase: Phase) -> Self {
        Self::for_runtime(crate::runtime::current(), phase)
    }
}
impl<'a> Span<'a> {
    pub(crate) fn for_runtime(runtime: &'a crate::runtime::Runtime, phase: Phase) -> Self {
        Self::start_for_runtime(runtime, phase, true)
    }
    /// V3 fused root rewriting does not attribute evacuation to exclusive roots.
    pub(crate) fn fused_roots_in(runtime: &'a crate::runtime::Runtime) -> Self {
        Self::start_for_runtime(runtime, Phase::Roots, false)
    }
    fn start_for_runtime(
        runtime: &'a crate::runtime::Runtime,
        phase: Phase,
        exclusive: bool,
    ) -> Self {
        Self::start_in(
            &runtime.telemetry.measurements,
            &runtime.telemetry.pause,
            phase,
            exclusive,
            |kind| super::observations::Span::for_runtime(runtime, kind),
        )
    }
    fn start_in(
        owner: &'a Measurements,
        pause: &'a super::pause::PauseTelemetry,
        phase: Phase,
        exclusive: bool,
        interval: impl FnOnce(super::observations::Kind) -> super::observations::Span<'a>,
    ) -> Self {
        let cpu = matches!(phase, Phase::Assist)
            .then(super::workers::thread_cpu_ns)
            .flatten();
        let kind = match phase {
            Phase::Roots => super::observations::Kind::Roots,
            Phase::Evacuation => super::observations::Kind::Evacuation,
            Phase::Resume => super::observations::Kind::Resume,
            Phase::Assist => super::observations::Kind::Assist,
        };
        Self {
            owner,
            _interval: interval(kind),
            _exclusive: match phase {
                Phase::Roots if exclusive => Some(super::pause::Span::new(
                    pause,
                    super::pause::Component::Roots,
                )),
                Phase::Evacuation => Some(super::pause::Span::new(
                    pause,
                    super::pause::Component::Evacuation,
                )),
                _ => None,
            },
            phase,
            start: Instant::now(),
            cpu,
        }
    }
}
impl Drop for Span<'_> {
    fn drop(&mut self) {
        let ns = elapsed_ns(self.start);
        let cpu = if matches!(self.phase, Phase::Assist) {
            self.cpu
                .zip(super::workers::thread_cpu_ns())
                .and_then(|(a, b)| b.checked_sub(a))
        } else {
            None
        };
        let mut s = self.owner.stats.lock().unwrap_or_else(|p| p.into_inner());
        match self.phase {
            Phase::Roots => {
                s.root_scan_ns = s.root_scan_ns.saturating_add(ns);
                s.root_scan_samples = s.root_scan_samples.saturating_add(1);
            }
            Phase::Evacuation => {
                s.evacuation_ns = s.evacuation_ns.saturating_add(ns);
                s.evacuation_samples = s.evacuation_samples.saturating_add(1);
            }
            Phase::Resume => {
                s.resume_ns = s.resume_ns.saturating_add(ns);
                s.resume_samples = s.resume_samples.saturating_add(1);
            }
            Phase::Assist => {
                s.assist_cpu_ns = s.assist_cpu_ns.saturating_add(cpu.unwrap_or(0));
                s.assist_cpu_missing_samples = s
                    .assist_cpu_missing_samples
                    .saturating_add(u64::from(cpu.is_none()));
                s.assist_samples = s.assist_samples.saturating_add(1);
            }
        }
    }
}
pub(crate) struct WorkerLifetime<'a>(&'a Measurements);
impl WorkerLifetime<'static> {
    pub(crate) fn new() -> Self {
        Self::new_in(default_owner())
    }
}
impl<'a> WorkerLifetime<'a> {
    pub(crate) fn new_in(owner: &'a Measurements) -> Self {
        owner.threads.fetch_add(1, Ordering::AcqRel);
        Self(owner)
    }
}
impl Drop for WorkerLifetime<'_> {
    fn drop(&mut self) {
        self.0.threads.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(crate) struct BusyWorker<'a>(&'a Measurements);
impl Measurements {
    pub(crate) fn worker_busy(&self) -> BusyWorker<'_> {
        self.busy.fetch_add(1, Ordering::AcqRel);
        BusyWorker(self)
    }
}
impl Drop for BusyWorker<'_> {
    fn drop(&mut self) {
        self.0.busy.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurement_owners_keep_worker_lifetimes_and_completed_work_independent() {
        let first_runtime =
            crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let second_runtime =
            crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
        let first = &first_runtime.telemetry.measurements;
        let second = &second_runtime.telemetry.measurements;
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [(&first_runtime, 1), (&second_runtime, 3)]
                .into_iter()
                .map(|(runtime, stride)| {
                    let barrier = &barrier;
                    scope.spawn(crate::runtime::serving(move || {
                        let owner = &runtime.telemetry.measurements;
                        let _lifetime = WorkerLifetime::new_in(owner);
                        for round in 1..=64 {
                            let busy = owner.worker_busy();
                            owner.completed(
                                MarkWork {
                                    cpu_ns: stride,
                                    ..MarkWork::default()
                                },
                                2 * stride,
                            );
                            drop(Span::for_runtime(runtime, Phase::Resume));
                            barrier.wait();
                            let stats = owner.snapshot();
                            assert_eq!(stats.worker_threads, 1);
                            assert_eq!(stats.worker_busy, 1);
                            assert_eq!(stats.worker_idle, 0);
                            assert_eq!(stats.mark_cpu_ns, round * stride);
                            assert_eq!(stats.reclaimed_bytes, round * 2 * stride);
                            assert_eq!(stats.resume_samples, round);
                            drop(busy);
                            barrier.wait();
                        }
                    }))
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        for owner in [first, second] {
            assert_eq!(owner.snapshot().worker_threads, 0);
            assert_eq!(owner.snapshot().worker_busy, 0);
        }
        let panic = std::panic::catch_unwind(|| {
            let _lifetime = WorkerLifetime::new_in(first);
            let _busy = first.worker_busy();
            first.reset();
            assert_eq!(first.snapshot().worker_threads, 1);
            assert_eq!(first.snapshot().worker_busy, 1);
            panic!("owned measurement unwind");
        });
        assert!(panic.is_err());
        assert_eq!(first.snapshot().worker_threads, 0);
        assert_eq!(first.snapshot().worker_busy, 0);
        assert_eq!(first.snapshot().mark_cpu_ns, 0);
        assert_eq!(second.snapshot().mark_cpu_ns, 192);
    }
}
