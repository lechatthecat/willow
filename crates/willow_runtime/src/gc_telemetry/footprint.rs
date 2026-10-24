//! Process-lifetime high-water counters, independent of periodic snapshots.
use std::sync::atomic::{AtomicU64, Ordering};
struct Counters {
    reserved: AtomicU64,
    tail_waste: AtomicU64,
    #[cfg(feature = "gc-acceptance-faults")]
    window: AtomicU64,
}
impl Counters {
    const fn new() -> Self {
        Self {
            reserved: AtomicU64::new(0),
            tail_waste: AtomicU64::new(0),
            #[cfg(feature = "gc-acceptance-faults")]
            window: AtomicU64::new(0),
        }
    }
    fn reservation(&self, bytes: u64) {
        self.reserved.fetch_max(bytes, Ordering::Relaxed);
        #[cfg(feature = "gc-acceptance-faults")]
        let _ = self
            .window
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n != 0).then_some(n.max(bytes))
            });
    }
    fn retired_tail(&self, bytes: u64) {
        let _ = self
            .tail_waste
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(bytes))
            });
    }
    fn snapshot(&self) -> (u64, u64) {
        (
            self.reserved.load(Ordering::Relaxed),
            self.tail_waste.load(Ordering::Relaxed),
        )
    }
}
static COUNTERS: Counters = Counters::new();
#[cfg(feature = "gc-acceptance-faults")]
static MARK_WINDOW_RESULT: AtomicU64 = AtomicU64::new(0);
pub(crate) fn reservation(bytes: u64) {
    COUNTERS.reservation(bytes);
}
#[cfg(feature = "gc-acceptance-faults")]
pub(crate) fn window_open() -> bool {
    COUNTERS.window.load(Ordering::Relaxed) != 0
}
#[cfg(feature = "gc-acceptance-faults")]
/// Returns false when a previous window was left armed or unread.
pub(crate) fn window_begin(bytes: u64) -> bool {
    let stale = MARK_WINDOW_RESULT.swap(0, Ordering::Relaxed);
    let open = COUNTERS.window.swap(bytes.max(1), Ordering::Relaxed);
    stale == 0 && open == 0
}
#[cfg(feature = "gc-acceptance-faults")]
pub(crate) fn window_end() -> u64 {
    // Disarm before checking so a failed window cannot poison the next begin.
    let open = COUNTERS.window.swap(0, Ordering::Relaxed);
    let bytes = MARK_WINDOW_RESULT.swap(0, Ordering::Relaxed);
    assert_eq!(open, 0, "mark window never completed");
    assert_ne!(bytes, 0);
    bytes
}
#[cfg(feature = "gc-acceptance-faults")]
/// Called only by both major sweep preparations: a nested minor's MarkComplete
/// must not close a window scoped to the major mark an explicit request joins.
pub(crate) fn mark_window_complete() {
    // Atomic exchange is the cutoff: a racing reservation either precedes it,
    // or retries against zero and cannot add post-mark growth to this window.
    let bytes = COUNTERS.window.swap(0, Ordering::Relaxed);
    if bytes != 0 {
        MARK_WINDOW_RESULT.store(bytes, Ordering::Relaxed);
    }
}

/// Reset discards all reservations, so the high water restarts from zero.
pub(crate) fn reset() {
    COUNTERS.reserved.store(0, Ordering::Relaxed);
    COUNTERS.tail_waste.store(0, Ordering::Relaxed);
    #[cfg(feature = "gc-acceptance-faults")]
    {
        COUNTERS.window.store(0, Ordering::Relaxed);
        MARK_WINDOW_RESULT.store(0, Ordering::Relaxed);
    }
}
pub(crate) fn retired_tail(bytes: u64) {
    COUNTERS.retired_tail(bytes);
}
/// Managed committed uses the same reservation convention as V1; it excludes
/// native/OS commitment. Waste is cumulative unused nursery tail at retirement.
pub fn snapshot() -> (u64, u64) {
    COUNTERS.snapshot()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn high_water_keeps_transient_growth_and_retirement_saturates() {
        let c = Counters::new();
        for value in [64, 256, 32, 1024, 8] {
            c.reservation(value);
        }
        assert_eq!(c.snapshot().0, 1024);
        c.retired_tail(u64::MAX - 2);
        c.retired_tail(4);
        assert_eq!(c.snapshot().1, u64::MAX);
    }
    #[test]
    #[cfg(feature = "gc-acceptance-faults")]
    fn scoped_peak_excludes_previous_windows_without_resetting_global_peak() {
        let c = Counters::new();
        c.reservation(1024);
        c.window.store(64, Ordering::Relaxed);
        for n in [128, 32, 256, 16] {
            c.reservation(n);
        }
        assert_eq!(c.window.swap(0, Ordering::Relaxed), 256);
        c.reservation(512);
        assert_eq!(c.window.load(Ordering::Relaxed), 0);
        assert_eq!(c.snapshot().0, 1024);
    }
    #[test]
    #[cfg(feature = "gc-acceptance-faults")]
    fn mark_window_cutoff_excludes_later_reservation_growth() {
        let c = Counters::new();
        c.window.store(64, Ordering::Relaxed);
        c.reservation(128);
        let mark_peak = c.window.swap(0, Ordering::Relaxed);
        c.reservation(4096);
        assert_eq!(mark_peak, 128);
        assert_eq!(c.window.load(Ordering::Relaxed), 0);
        assert_eq!(c.snapshot().0, 4096);
    }
    #[test]
    fn high_water_concurrent_reservation_and_tail_totals() {
        let c = Counters::new();
        std::thread::scope(|scope| {
            for thread in 1..=8 {
                let c = &c;
                scope.spawn(move || {
                    for i in 1..=128 {
                        c.reservation(thread * i);
                        c.retired_tail(3);
                    }
                });
            }
        });
        assert_eq!(c.snapshot(), (1024, 8 * 128 * 3));
    }
}
