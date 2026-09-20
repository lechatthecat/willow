//! Stop requests are counted at the coordination boundary, including empty
//! heaps and failed collections. V1 cycle totals cannot prove zero STW: an
//! aborted cycle never publishes a V1 sample, and fallback/recovery stops must remain visible.

use super::{GcLatencyV1, WillowGcStatsV1, timestamp_ns};
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

pub const STOP_KINDS: usize = 4;
const EVENT_CAPACITY: usize = 64;
pub const STOP_COUNTERS_VALID: u64 = 1;
pub const STOP_EVENTS_LOST: u64 = 2;
pub const STOP_COUNTERS_SATURATED: u64 = 4;
pub const STOP_PROTOCOL_ERROR: u64 = 8;

#[repr(u32)]
#[derive(Clone, Copy, Debug)]
pub(crate) enum StopReason {
    InitialMark = 1,
    Remark = 2,
    Minor = 3,
    Recovery = 4,
}

/// Counts actual operations, including repeated metadata walks. Counts are
/// accumulated on the collector stack, not through a per-object shared lock.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct StopWorkV2 {
    pub metadata_objects: u64,
    pub metadata_bytes: u64,
    pub root_values: u64,
    pub swept_objects: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct StopEventV2 {
    pub sequence: u64,
    pub reset_generation: u64,
    pub reason: u32,
    /// 0 = returned normally, 1 = callback unwound, 2 = incomplete protocol.
    pub outcome: u32,
    pub requested_ns: u64,
    pub stopped_ns: u64,
    /// Stop gate was cleared and the coordination lock released. This is NOT
    /// when every resumed thread was next scheduled by the operating system.
    pub released_ns: u64,
    pub work: StopWorkV2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct StopTotalsV2 {
    pub requests: u64,
    pub completed: u64,
    pub aborted: u64,
    /// Request publication through all registered peers acknowledging.
    pub rendezvous: GcLatencyV1,
    /// All peers acknowledged through stop-gate release and lock release.
    pub stopped: GcLatencyV1,
    pub work: StopWorkV2,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct StopStatsV2 {
    pub flags: u64,
    pub reset_generation: u64,
    /// Monotonic process-lifetime identity; reset does not reuse identities.
    pub last_sequence: u64,
    /// Nonzero until that request has actually released the world.
    pub active_sequence: u64,
    pub events_dropped: u64,
    pub events_pending: u64,
    /// Initial mark, remark, minor, recovery, in that order.
    pub by_reason: [StopTotalsV2; STOP_KINDS],
    pub last_event: StopEventV2,
}

/// V2 adds a separately coherent coordination group. V1's complete ABI and
/// symbol remain unchanged. Neither group claims simultaneous heap sampling.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WillowGcStatsV2 {
    pub version: u32,
    pub struct_size: u32,
    pub timestamp_ns: u64,
    pub baseline: WillowGcStatsV1,
    pub stops: StopStatsV2,
    pub workers: super::workers::MarkWorkerStatsV2,
}

struct StopTelemetry {
    stats: StopStatsV2,
    pending: VecDeque<StopEventV2>,
}

impl Default for StopTelemetry {
    fn default() -> Self {
        Self {
            stats: StopStatsV2 {
                flags: STOP_COUNTERS_VALID,
                ..Default::default()
            },
            pending: VecDeque::with_capacity(EVENT_CAPACITY),
        }
    }
}

fn add(value: &mut u64, increment: u64, flags: &mut u64) {
    match value.checked_add(increment) {
        Some(total) => *value = total,
        None => {
            *value = u64::MAX;
            *flags |= STOP_COUNTERS_SATURATED;
        }
    }
}

impl StopTelemetry {
    fn request(&mut self, reason: StopReason, now: u64) -> StopEventV2 {
        let s = &mut self.stats;
        if s.active_sequence != 0 {
            s.flags |= STOP_PROTOCOL_ERROR;
        }
        add(&mut s.last_sequence, 1, &mut s.flags);
        s.active_sequence = s.last_sequence;
        add(
            &mut s.by_reason[reason as usize - 1].requests,
            1,
            &mut s.flags,
        );
        StopEventV2 {
            sequence: s.last_sequence,
            reset_generation: s.reset_generation,
            reason: reason as u32,
            requested_ns: now,
            ..Default::default()
        }
    }

    fn complete(&mut self, event: StopEventV2) {
        let s = &mut self.stats;
        if s.active_sequence != event.sequence
            || s.reset_generation != event.reset_generation
            || event.stopped_ns < event.requested_ns
            || event.released_ns < event.stopped_ns
            || event.outcome == 2
        {
            s.flags |= STOP_PROTOCOL_ERROR;
        }
        if s.active_sequence == event.sequence {
            s.active_sequence = 0;
        }
        let totals = &mut s.by_reason[event.reason as usize - 1];
        add(&mut totals.completed, 1, &mut s.flags);
        if event.outcome != 0 {
            add(&mut totals.aborted, 1, &mut s.flags);
        }
        let rendezvous = event.stopped_ns.saturating_sub(event.requested_ns);
        let stopped = event.released_ns.saturating_sub(event.stopped_ns);
        for (histogram, ns) in [
            (&mut totals.rendezvous, rendezvous),
            (&mut totals.stopped, stopped),
        ] {
            if histogram.count == u64::MAX || histogram.total_ns.checked_add(ns).is_none() {
                s.flags |= STOP_COUNTERS_SATURATED;
            }
            histogram.record(ns);
        }
        add(
            &mut totals.work.metadata_objects,
            event.work.metadata_objects,
            &mut s.flags,
        );
        add(
            &mut totals.work.metadata_bytes,
            event.work.metadata_bytes,
            &mut s.flags,
        );
        add(
            &mut totals.work.root_values,
            event.work.root_values,
            &mut s.flags,
        );
        add(
            &mut totals.work.swept_objects,
            event.work.swept_objects,
            &mut s.flags,
        );
        s.last_event = event;
        if self.pending.len() == EVENT_CAPACITY {
            self.pending.pop_front();
            add(&mut s.events_dropped, 1, &mut s.flags);
            s.flags |= STOP_EVENTS_LOST;
        }
        self.pending.push_back(event);
        s.events_pending = self.pending.len() as u64;
    }
}

static STOPS: LazyLock<Mutex<StopTelemetry>> =
    LazyLock::new(|| Mutex::new(StopTelemetry::default()));

pub(crate) struct StopMeasurement {
    event: StopEventV2,
    finished: bool,
}

impl StopMeasurement {
    pub(crate) fn begin(reason: StopReason) -> Self {
        let event = STOPS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .request(reason, timestamp_ns());
        Self {
            event,
            finished: false,
        }
    }
    pub(crate) fn stopped(&mut self) {
        self.event.stopped_ns = timestamp_ns();
    }
    pub(crate) fn work(&mut self) -> &mut StopWorkV2 {
        &mut self.event.work
    }
    pub(crate) fn finish(mut self, aborted: bool) {
        self.event.outcome = u32::from(aborted);
        self.event.released_ns = timestamp_ns();
        STOPS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .complete(self.event);
        self.finished = true;
    }
}

impl Drop for StopMeasurement {
    fn drop(&mut self) {
        if !self.finished {
            self.event.outcome = 2;
            self.event.released_ns = timestamp_ns();
            STOPS
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .complete(self.event);
        }
    }
}

pub fn snapshot_stops() -> StopStatsV2 {
    STOPS.lock().unwrap_or_else(|p| p.into_inner()).stats
}

/// Drain under a short telemetry-only lock. Formatting and I/O occur after the
/// lock is released, and only after collector election is released by caller.
pub(super) fn take_events() -> Vec<StopEventV2> {
    let mut telemetry = STOPS.lock().unwrap_or_else(|p| p.into_inner());
    let events = telemetry.pending.drain(..).collect();
    telemetry.stats.events_pending = 0;
    events
}

pub(super) fn discard_events() {
    let mut telemetry = STOPS.lock().unwrap_or_else(|p| p.into_inner());
    telemetry.pending.clear();
    telemetry.stats.events_pending = 0;
}

pub(super) fn reset_for_test() {
    let mut telemetry = STOPS.lock().unwrap_or_else(|p| p.into_inner());
    let sequence = telemetry.stats.last_sequence;
    let generation = telemetry.stats.reset_generation.saturating_add(1);
    *telemetry = StopTelemetry::default();
    telemetry.stats.last_sequence = sequence;
    telemetry.stats.reset_generation = generation;
}

#[cfg(test)]
#[path = "stops_tests.rs"]
mod tests;
