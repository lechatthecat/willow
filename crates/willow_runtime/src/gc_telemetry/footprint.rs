//! Runtime-lifetime high-water counters, independent of periodic snapshots.
use std::sync::atomic::{AtomicU64, Ordering};
pub(crate) struct Counters {
    reserved: AtomicU64,
    tail_waste: AtomicU64,
    #[cfg(feature = "gc-acceptance-faults")]
    window: AtomicU64,
    #[cfg(feature = "gc-acceptance-faults")]
    mark_window_result: AtomicU64,
}
impl Counters {
    pub(crate) const fn new() -> Self {
        Self {
            reserved: AtomicU64::new(0),
            tail_waste: AtomicU64::new(0),
            #[cfg(feature = "gc-acceptance-faults")]
            window: AtomicU64::new(0),
            #[cfg(feature = "gc-acceptance-faults")]
            mark_window_result: AtomicU64::new(0),
        }
    }
    pub(crate) fn reservation(&self, bytes: u64) {
        self.reserved.fetch_max(bytes, Ordering::Relaxed);
        #[cfg(feature = "gc-acceptance-faults")]
        let _ = self
            .window
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n != 0).then_some(n.max(bytes))
            });
    }
    pub(crate) fn retired_tail(&self, bytes: u64) {
        let _ = self
            .tail_waste
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some(n.saturating_add(bytes))
            });
    }
    pub(crate) fn snapshot(&self) -> (u64, u64) {
        (
            self.reserved.load(Ordering::Relaxed),
            self.tail_waste.load(Ordering::Relaxed),
        )
    }
}
impl Counters {
    #[cfg(feature = "gc-acceptance-faults")]
    pub(crate) fn window_open(&self) -> bool {
        self.window.load(Ordering::Relaxed) != 0
    }
    #[cfg(feature = "gc-acceptance-faults")]
    /// Returns false when a previous window was left armed or unread.
    pub(crate) fn window_begin(&self, bytes: u64) -> bool {
        let stale = self.mark_window_result.swap(0, Ordering::Relaxed);
        let open = self.window.swap(bytes.max(1), Ordering::Relaxed);
        stale == 0 && open == 0
    }
    #[cfg(feature = "gc-acceptance-faults")]
    pub(crate) fn window_end(&self) -> u64 {
        // Disarm before checking so a failed window cannot poison the next begin.
        let open = self.window.swap(0, Ordering::Relaxed);
        let bytes = self.mark_window_result.swap(0, Ordering::Relaxed);
        assert_eq!(open, 0, "mark window never completed");
        assert_ne!(bytes, 0);
        bytes
    }
    #[cfg(feature = "gc-acceptance-faults")]
    /// Called only by both major sweep preparations: a nested minor's MarkComplete
    /// must not close a window scoped to the major mark an explicit request joins.
    pub(crate) fn mark_window_complete(&self) {
        // Atomic exchange is the cutoff: a racing reservation either precedes it,
        // or retries against zero and cannot add post-mark growth to this window.
        let bytes = self.window.swap(0, Ordering::Relaxed);
        if bytes != 0 {
            self.mark_window_result.store(bytes, Ordering::Relaxed);
        }
    }

    /// Reset discards all reservations, so the high water restarts from zero.
    pub(crate) fn reset(&self) {
        self.reserved.store(0, Ordering::Relaxed);
        self.tail_waste.store(0, Ordering::Relaxed);
        #[cfg(feature = "gc-acceptance-faults")]
        {
            self.window.store(0, Ordering::Relaxed);
            self.mark_window_result.store(0, Ordering::Relaxed);
        }
    }
}
fn default_owner() -> &'static Counters {
    &super::telemetry().footprint
}
pub(crate) fn reservation(bytes: u64) {
    default_owner().reservation(bytes);
}
pub(crate) fn reset() {
    default_owner().reset();
}
/// Managed committed uses the same reservation convention as V1; it excludes
/// native/OS commitment. Waste is cumulative unused nursery tail at retirement.
pub fn snapshot() -> (u64, u64) {
    default_owner().snapshot()
}
#[cfg(feature = "gc-acceptance-faults")]
pub(crate) fn window_open() -> bool {
    default_owner().window_open()
}
#[cfg(feature = "gc-acceptance-faults")]
pub(crate) fn window_begin(bytes: u64) -> bool {
    default_owner().window_begin(bytes)
}
#[cfg(feature = "gc-acceptance-faults")]
pub(crate) fn window_end() -> u64 {
    default_owner().window_end()
}
#[cfg(feature = "gc-acceptance-faults")]
pub(crate) fn mark_window_complete() {
    default_owner().mark_window_complete();
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn footprint_owners_keep_parallel_high_water_and_reset_independent() {
        let first = Counters::new();
        let second = Counters::new();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [(&first, 1), (&second, 3)]
                .into_iter()
                .map(|(owner, stride)| {
                    let barrier = &barrier;
                    scope.spawn(crate::runtime::serving(move || {
                        for round in 1..=64 {
                            owner.reservation(round * stride);
                            owner.retired_tail(stride);
                            barrier.wait();
                            assert_eq!(owner.snapshot(), (round * stride, round * stride));
                            barrier.wait();
                        }
                    }))
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        first.reset();
        assert_eq!(first.snapshot(), (0, 0));
        assert_eq!(second.snapshot(), (192, 192));
    }

    #[test]
    #[cfg(feature = "gc-acceptance-faults")]
    fn footprint_owner_mark_windows_preserve_other_pending_and_completed_results() {
        let first = Counters::new();
        let second = Counters::new();
        assert!(first.window_begin(64));
        assert!(second.window_begin(128));
        first.reservation(256);
        second.reservation(512);
        first.mark_window_complete();
        assert!(!first.window_open());
        assert!(second.window_open());
        first.reservation(1024);
        assert_eq!(first.window_end(), 256);
        second.mark_window_complete();
        first.reset();
        assert_eq!(second.window_end(), 512);
        assert!(first.window_begin(1));
        assert!(!first.window_begin(2));
        first.mark_window_complete();
        assert!(!first.window_begin(3));
        first.reset();
        assert!(first.window_begin(4));
        first.mark_window_complete();
        assert_eq!(first.window_end(), 4);
        assert_eq!(second.snapshot().0, 512);
    }

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
                scope.spawn(crate::runtime::serving(move || {
                    for i in 1..=128 {
                        c.reservation(thread * i);
                        c.retired_tail(3);
                    }
                }));
            }
        });
        assert_eq!(c.snapshot(), (1024, 8 * 128 * 3));
    }
}
