//! Per-type native tracing diagnostics, separate from stable C V1/V2 layouts.
//! Cumulative since GC initialization; snapshots have unspecified row order.
//! Deferred objects publish at bounded drain completion, stopped calls at the
//! end of a stopped major walk (including unwind). Minor relocation is excluded.
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NativeTraceStats {
    pub type_id: u32,
    /// Unique object discoveries with StopOnly capability, summed over epochs.
    pub deferred_objects: u64,
    /// Stopped major callback invocations, including safe hooks retraced on fallback.
    pub stopped_calls: u64,
    /// Subset of stopped_calls whose registered capability was StopOnly.
    /// Includes descendants first discovered during stopped tracing.
    pub stop_only_calls: u64,
}

pub(crate) struct NativeTrace {
    stats: Mutex<Option<HashMap<u32, NativeTraceStats>>>,
    generation: AtomicU64,
}
impl NativeTrace {
    pub(crate) const fn new() -> Self {
        Self {
            stats: Mutex::new(None),
            generation: AtomicU64::new(0),
        }
    }
    pub(crate) fn snapshot(&self) -> Vec<NativeTraceStats> {
        self.stats
            .lock()
            .unwrap()
            .as_ref()
            .map(|rows| rows.values().copied().collect())
            .unwrap_or_default()
    }
    pub(crate) fn reset(&self) {
        let mut stats = self.stats.lock().unwrap();
        self.generation.fetch_add(1, Ordering::Relaxed);
        if let Some(rows) = stats.as_mut() {
            rows.clear();
        }
    }
    pub(crate) fn counts(&self) -> Counts<'_> {
        Counts {
            owner: self,
            generation: self.generation.load(Ordering::Relaxed),
            rows: HashMap::new(),
        }
    }
}
fn default_owner() -> &'static NativeTrace {
    &super::telemetry().native_trace
}

/// Coherent cumulative counters. Zero-activity types are omitted; registration
/// inventory is available separately via gc::native_trace_modes().
pub fn snapshot() -> Vec<NativeTraceStats> {
    default_owner().snapshot()
}

/// Local aggregation avoids a telemetry lock/atomic per graph object or edge.
/// Drop publishes even if a callback unwinds; calls count attempts, not success.
pub(crate) struct Counts<'a> {
    owner: &'a NativeTrace,
    generation: u64,
    rows: HashMap<u32, NativeTraceStats>,
}

impl Default for Counts<'static> {
    fn default() -> Self {
        default_owner().counts()
    }
}

impl Counts<'_> {
    fn row(&mut self, type_id: u32) -> &mut NativeTraceStats {
        self.rows.entry(type_id).or_insert(NativeTraceStats {
            type_id,
            ..Default::default()
        })
    }

    pub(crate) fn deferred(&mut self, type_id: u32) {
        let row = self.row(type_id);
        row.deferred_objects = row.deferred_objects.saturating_add(1);
    }

    pub(crate) fn stopped(&mut self, type_id: u32, stop_only: bool) {
        let row = self.row(type_id);
        row.stopped_calls = row.stopped_calls.saturating_add(1);
        row.stop_only_calls = row.stop_only_calls.saturating_add(u64::from(stop_only));
    }
}

impl Drop for Counts<'_> {
    fn drop(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let mut stats = self.owner.stats.lock().unwrap();
        if self.generation != self.owner.generation.load(Ordering::Relaxed) {
            return;
        }
        let stats = stats.get_or_insert_with(HashMap::new);
        for (&type_id, row) in &self.rows {
            let total = stats.entry(type_id).or_insert(NativeTraceStats {
                type_id,
                ..Default::default()
            });
            total.deferred_objects = total.deferred_objects.saturating_add(row.deferred_objects);
            total.stopped_calls = total.stopped_calls.saturating_add(row.stopped_calls);
            total.stop_only_calls = total.stop_only_calls.saturating_add(row.stop_only_calls);
        }
    }
}

pub(crate) fn reset() {
    default_owner().reset();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_trace_owners_isolate_same_type_generation_reset_and_unwind() {
        let first = NativeTrace::new();
        let second = NativeTrace::new();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let handles: Vec<_> = [(&first, 1), (&second, 3)]
                .into_iter()
                .map(|(owner, count)| {
                    let barrier = &barrier;
                    scope.spawn(crate::runtime::serving(move || {
                        for _ in 0..64 {
                            let mut batch = owner.counts();
                            for _ in 0..count {
                                batch.deferred(7);
                                batch.stopped(7, true);
                            }
                            barrier.wait();
                            drop(batch);
                            barrier.wait();
                        }
                    }))
                })
                .collect();
            for handle in handles {
                handle.join().unwrap();
            }
        });
        assert_eq!(first.snapshot()[0].deferred_objects, 64);
        assert_eq!(second.snapshot()[0].deferred_objects, 192);
        let mut stale = first.counts();
        stale.stopped(7, true);
        let mut unaffected = second.counts();
        unaffected.deferred(7);
        first.reset();
        drop(stale);
        drop(unaffected);
        assert!(first.snapshot().is_empty());
        let unwind = std::panic::catch_unwind(|| {
            let mut batch = first.counts();
            batch.stopped(7, false);
            panic!("native accounting unwind");
        });
        assert!(unwind.is_err());
        assert_eq!(
            first.snapshot()[0],
            NativeTraceStats {
                type_id: 7,
                stopped_calls: 1,
                ..NativeTraceStats::default()
            }
        );
        assert_eq!(
            second.snapshot()[0],
            NativeTraceStats {
                type_id: 7,
                deferred_objects: 193,
                stopped_calls: 192,
                stop_only_calls: 192
            }
        );
    }
}
