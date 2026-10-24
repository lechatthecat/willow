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
static STATS: Mutex<MeasurementsV3> = Mutex::new(MeasurementsV3 {
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
});
static THREADS: AtomicU64 = AtomicU64::new(0);
static BUSY: AtomicU64 = AtomicU64::new(0);

pub fn snapshot() -> MeasurementsV3 {
    let mut stats = *STATS.lock().unwrap_or_else(|p| p.into_inner());
    stats.worker_threads = THREADS.load(Ordering::Acquire);
    stats.worker_busy = BUSY.load(Ordering::Acquire);
    stats.worker_idle = stats.worker_threads.saturating_sub(stats.worker_busy);
    stats
}
pub(crate) fn completed(work: MarkWork, reclaimed: u64) {
    let mut s = STATS.lock().unwrap_or_else(|p| p.into_inner());
    s.mark_cpu_ns = s.mark_cpu_ns.saturating_add(work.cpu_ns);
    s.mark_cpu_missing_cycles = s
        .mark_cpu_missing_cycles
        .saturating_add(u64::from(work.cpu_incomplete));
    s.reclaimed_bytes = s.reclaimed_bytes.saturating_add(reclaimed);
}
pub(crate) fn reset() {
    *STATS.lock().unwrap_or_else(|p| p.into_inner()) = MeasurementsV3::default();
}
pub(crate) enum Phase {
    Roots,
    Evacuation,
    Resume,
    Assist,
}
pub(crate) struct Span {
    phase: Phase,
    start: Instant,
    cpu: Option<u64>,
    _interval: super::observations::Span,
    _exclusive: Option<super::pause::Span>,
}
impl Span {
    pub(crate) fn new(phase: Phase) -> Self {
        Self::start(phase, true)
    }
    /// Preserve V3's fused root-rewrite timing without attributing copying to
    /// root enumeration in the exclusive component histograms.
    pub(crate) fn fused_roots() -> Self {
        Self::start(Phase::Roots, false)
    }
    fn start(phase: Phase, exclusive: bool) -> Self {
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
            _interval: super::observations::Span::new(kind),
            _exclusive: match phase {
                Phase::Roots if exclusive => {
                    Some(super::pause::Span::new(super::pause::Component::Roots))
                }
                Phase::Evacuation => {
                    Some(super::pause::Span::new(super::pause::Component::Evacuation))
                }
                _ => None,
            },
            phase,
            start: Instant::now(),
            cpu,
        }
    }
}
impl Drop for Span {
    fn drop(&mut self) {
        let ns = elapsed_ns(self.start);
        let cpu = if matches!(self.phase, Phase::Assist) {
            self.cpu
                .zip(super::workers::thread_cpu_ns())
                .and_then(|(a, b)| b.checked_sub(a))
        } else {
            None
        };
        let mut s = STATS.lock().unwrap_or_else(|p| p.into_inner());
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
pub(crate) struct WorkerLifetime;
impl WorkerLifetime {
    pub(crate) fn new() -> Self {
        THREADS.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for WorkerLifetime {
    fn drop(&mut self) {
        THREADS.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(crate) fn worker_busy() {
    BUSY.fetch_add(1, Ordering::AcqRel);
}
pub(crate) fn worker_idle() {
    BUSY.fetch_sub(1, Ordering::AcqRel);
}
