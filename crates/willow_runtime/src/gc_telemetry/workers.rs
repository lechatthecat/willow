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

static STATS: std::sync::LazyLock<Mutex<MarkWorkerStatsV2>> =
    std::sync::LazyLock::new(|| Mutex::new(MarkWorkerStatsV2::default()));

pub fn snapshot() -> MarkWorkerStatsV2 {
    *STATS.lock().unwrap_or_else(|p| p.into_inner())
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

pub(crate) fn record_failure(failure: Failure) {
    let mut stats = STATS.lock().unwrap_or_else(|p| p.into_inner());
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

pub(crate) struct JobMeasurement {
    start: Instant,
    cpu: Option<u64>,
}
impl JobMeasurement {
    pub(crate) fn begin() -> Self {
        Self {
            start: Instant::now(),
            cpu: thread_cpu_ns(),
        }
    }
}
impl Drop for JobMeasurement {
    fn drop(&mut self) {
        let cpu = self
            .cpu
            .zip(thread_cpu_ns())
            .and_then(|(a, b)| b.checked_sub(a));
        let wall = self.start.elapsed().as_nanos();
        let mut stats = STATS.lock().unwrap_or_else(|p| p.into_inner());
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
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn thread_cpu_ns() -> Option<u64> {
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
pub(crate) fn thread_cpu_ns() -> Option<u64> {
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
pub(crate) fn thread_cpu_ns() -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn overflow_is_saturated_and_reported() {
        let mut value = u64::MAX - 1;
        assert!(!add(&mut value, 1));
        assert!(add(&mut value, 1));
        assert_eq!(value, u64::MAX);
    }
}
