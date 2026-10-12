//! Per-type native tracing diagnostics, separate from stable C V1/V2 layouts.
//! Cumulative since GC initialization; snapshots have unspecified row order.
//! Deferred objects publish at bounded drain completion, stopped calls at the
//! end of a stopped major walk (including unwind). Minor relocation is excluded.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

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

static STATS: LazyLock<Mutex<HashMap<u32, NativeTraceStats>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Coherent cumulative counters. Zero-activity types are omitted; registration
/// inventory is available separately via gc::native_trace_modes().
pub fn snapshot() -> Vec<NativeTraceStats> {
    STATS.lock().unwrap().values().copied().collect()
}

/// Local aggregation avoids a telemetry lock/atomic per graph object or edge.
/// Drop publishes even if a callback unwinds; calls count attempts, not success.
pub(crate) struct Counts {
    generation: u64,
    rows: HashMap<u32, NativeTraceStats>,
}

impl Default for Counts {
    fn default() -> Self {
        Self {
            generation: GENERATION.load(Ordering::Relaxed),
            rows: HashMap::new(),
        }
    }
}

impl Counts {
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

impl Drop for Counts {
    fn drop(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let mut stats = STATS.lock().unwrap();
        if self.generation != GENERATION.load(Ordering::Relaxed) {
            return;
        }
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
    let mut stats = STATS.lock().unwrap();
    GENERATION.fetch_add(1, Ordering::Relaxed);
    stats.clear();
}
