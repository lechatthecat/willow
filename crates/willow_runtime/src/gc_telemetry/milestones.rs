//! Bounded, process-lifetime cycle identities. No heap traversal or I/O here.
use super::timestamp_ns;
use crate::gc::GcCondvar;
use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

const CAPACITY: usize = 64;

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GcStage {
    MarkComplete = 1,
    SweepComplete = 2,
    Quiescent = 3,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct CycleMilestones {
    pub epoch: u64,
    pub mark_complete_ns: u64,
    pub sweep_complete_ns: u64,
    pub quiescent_ns: u64,
    /// Stage reached, independent of timestamp zero being a valid clock value.
    pub stage: u32,
    pub aborted: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GcTicket(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitError {
    Timeout,
    Aborted,
    Expired,
    /// Blocking a registered mutator could prevent root publication/parking.
    RegisteredMutator,
}

/// `active` holds unfinished cycles (at most a major plus its nested minor).
/// A finished top-level cycle waits in `finished` for quiescent publication at
/// election release, so repeated owned cycles in one election cannot nest.
#[derive(Default)]
struct State {
    active: Vec<u64>,
    finished: Option<u64>,
    records: VecDeque<CycleMilestones>,
}
static STATE: LazyLock<(Mutex<State>, GcCondvar)> =
    LazyLock::new(|| (Mutex::new(State::default()), GcCondvar::new()));
#[cfg(test)]
static WAITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
thread_local! {
    static REQUEST: std::cell::Cell<Option<GcTicket>> = const { std::cell::Cell::new(None) };
}

/// Synchronously attempt an explicit major collection and return its own
/// identity. The existing explicit request cooperates with collector election;
/// foreign-owner rejection or collector reentry can return None. The request
/// itself is synchronous and has no timeout; only observer waits are bounded.
pub fn request_gc_for_test() -> Option<GcTicket> {
    REQUEST.set(None);
    crate::gc::willow_gc_collect();
    REQUEST.take()
}

pub(crate) fn begin(epoch: u64) {
    let mut state = STATE.0.lock().unwrap_or_else(|p| p.into_inner());
    assert!(
        state.active.len() < 2,
        "collector nesting exceeds major plus minor"
    );
    if state.records.len() == CAPACITY {
        let retired = state
            .records
            .iter()
            .position(|r| !state.active.contains(&r.epoch) && state.finished != Some(r.epoch))
            .expect("bounded collector nesting leaves a retired record");
        state.records.remove(retired);
    }
    state.records.push_back(CycleMilestones {
        epoch,
        ..Default::default()
    });
    state.active.push(epoch);
    STATE.1.notify_all();
}

pub(crate) fn reached(stage: GcStage) {
    let mut state = STATE.0.lock().unwrap_or_else(|p| p.into_inner());
    let Some(epoch) = state.active.last().copied() else {
        return;
    };
    let record = state
        .records
        .iter_mut()
        .find(|r| r.epoch == epoch)
        .expect("active cycle has a record");
    debug_assert_eq!(record.epoch, epoch);
    if record.stage + 1 != stage as u32 {
        return;
    }
    set_stage(record, stage);
    STATE.1.notify_all();
}

fn set_stage(record: &mut CycleMilestones, stage: GcStage) {
    let now = timestamp_ns();
    match stage {
        GcStage::MarkComplete => record.mark_complete_ns = now,
        GcStage::SweepComplete => record.sweep_complete_ns = now,
        GcStage::Quiescent => record.quiescent_ns = now,
    }
    record.stage = stage as u32;
}

pub(crate) fn abort(epoch: u64) {
    let mut state = STATE.0.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(index) = state.active.iter().position(|&active| active == epoch) {
        if let Some(record) = state.records.iter_mut().find(|r| r.epoch == epoch) {
            record.aborted = 1;
        }
        state.active.remove(index);
        STATE.1.notify_all();
    }
}

/// The quiescent runtime reset ends every cycle without publication; it also
/// resets the collector phase, so no record may remain active across it.
pub(crate) fn reset() {
    let mut state = STATE.0.lock().unwrap_or_else(|p| p.into_inner());
    let State {
        active,
        finished,
        records,
    } = &mut *state;
    for epoch in active.drain(..).chain(finished.take()) {
        if let Some(record) = records.iter_mut().find(|r| r.epoch == epoch) {
            record.aborted = 1;
        }
    }
    STATE.1.notify_all();
}

/// A completed cycle leaves the active stack. Nested cycles have no election of
/// their own and publish now; a top-level cycle publishes at election release.
/// An unpublished predecessor in the same election is quiescent once a later
/// cycle in that election completes.
pub(crate) fn finished(epoch: u64, nested: bool) {
    let mut state = STATE.0.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(index) = state.active.iter().position(|&active| active == epoch) {
        state.active.remove(index);
    }
    let publish = if nested {
        Some(epoch)
    } else {
        state.finished.replace(epoch)
    };
    if let Some(epoch) = publish {
        publish_quiescent(&mut state, epoch);
    }
    STATE.1.notify_all();
}

fn publish_quiescent(state: &mut State, epoch: u64) {
    // Bounded history may already have retired an unpublished record.
    let Some(record) = state.records.iter_mut().find(|r| r.epoch == epoch) else {
        return;
    };
    if record.stage == GcStage::SweepComplete as u32 {
        set_stage(record, GcStage::Quiescent);
        REQUEST.set(Some(GcTicket(record.epoch)));
    } else {
        record.aborted = 1;
    }
}

/// Publication happens while releasing collector election, before a later
/// collector can begin. Never attach quiescence to a newer epoch.
pub(crate) fn quiescent() {
    let mut state = STATE.0.lock().unwrap_or_else(|p| p.into_inner());
    // An unfinished cycle still active at release is published by its stage.
    let unfinished = state.active.pop();
    let finished = state.finished.take();
    if unfinished.is_none() && finished.is_none() {
        return;
    }
    for epoch in unfinished.into_iter().chain(finished) {
        publish_quiescent(&mut state, epoch);
    }
    STATE.1.notify_all();
}

pub fn snapshot() -> CycleMilestones {
    STATE
        .0
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .records
        .back()
        .copied()
        .unwrap_or_default()
}

pub(crate) fn try_snapshot() -> Option<CycleMilestones> {
    match STATE.0.try_lock() {
        Ok(state) => state.records.back().copied(),
        Err(std::sync::TryLockError::Poisoned(p)) => p.into_inner().records.back().copied(),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

pub(crate) fn record(epoch: u64) -> Option<CycleMilestones> {
    STATE
        .0
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .records
        .iter()
        .find(|r| r.epoch == epoch)
        .copied()
}

/// Opt-in observer API. Call only from an unregistered test/control thread.
/// Tickets older than the bounded 64-cycle history report Expired, not success.
pub fn wait_gc_for_test(
    ticket: GcTicket,
    stage: GcStage,
    timeout: Duration,
) -> Result<CycleMilestones, WaitError> {
    if crate::gc::telemetry_wait_is_mutator() {
        return Err(WaitError::RegisteredMutator);
    }
    let start = Instant::now();
    let mut state = STATE.0.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        if let Some(record) = state.records.iter().find(|r| r.epoch == ticket.0) {
            if record.stage >= stage as u32 {
                return Ok(*record);
            }
            if record.aborted != 0 {
                return Err(WaitError::Aborted);
            }
        } else if ticket.0 == 0 || state.records.back().is_some_and(|r| r.epoch >= ticket.0) {
            return Err(WaitError::Expired);
        }
        let remaining = timeout.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return Err(WaitError::Timeout);
        }
        #[cfg(test)]
        WAITS.fetch_add(1, std::sync::atomic::Ordering::Release);
        state = STATE
            .1
            .wait_timeout(state, remaining)
            .unwrap_or_else(|p| p.into_inner())
            .0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::*;

    #[test]
    fn milestone_unobserved_cycles_emit_no_wake_calls() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let before = STATE.1.notifications();
        for cycles in [1, 8, 64] {
            for _ in 0..cycles {
                let cycle = super::super::Cycle::begin(super::super::CycleKind::Minor);
                reached(GcStage::MarkComplete);
                reached(GcStage::SweepComplete);
                cycle.finish(0, 0, Default::default());
                quiescent();
            }
            assert_eq!(STATE.1.notifications(), before);
            assert_eq!(snapshot().stage, GcStage::Quiescent as u32);
        }
        reset_internal_for_test();
    }

    #[test]
    fn milestone_repeated_owned_cycles_in_one_election_do_not_nest() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        // Owned top-level cycles completing before election release (or with no
        // election at all) must not accumulate as active nesting.
        let mut epochs = Vec::new();
        for _ in 0..4 {
            let minor = super::super::Cycle::begin(super::super::CycleKind::Minor);
            epochs.push(snapshot().epoch);
            reached(GcStage::MarkComplete);
            reached(GcStage::SweepComplete);
            minor.finish(0, 0, Default::default());
            assert!(STATE.0.lock().unwrap().active.is_empty());
        }
        let last = epochs.pop().unwrap();
        for &epoch in &epochs {
            assert_eq!(record(epoch).unwrap().stage, GcStage::Quiescent as u32);
        }
        assert_eq!(record(last).unwrap().stage, GcStage::SweepComplete as u32);
        quiescent();
        assert_eq!(record(last).unwrap().stage, GcStage::Quiescent as u32);
        reset_internal_for_test();
    }

    #[test]
    fn milestone_runtime_reset_retires_unpublished_cycles() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let major = super::super::Cycle::begin(super::super::CycleKind::Major);
        let unfinished = snapshot().epoch;
        std::mem::forget(major);
        let minor = super::super::Cycle::begin(super::super::CycleKind::Minor);
        let finished = snapshot().epoch;
        minor.finish(0, 0, Default::default());
        reset_internal_for_test();
        let state = STATE.0.lock().unwrap();
        assert!(state.active.is_empty() && state.finished.is_none());
        drop(state);
        for epoch in [unfinished, finished] {
            assert_eq!(record(epoch).unwrap().aborted, 1);
        }
        drop(super::super::Cycle::begin(super::super::CycleKind::Major));
        reset_internal_for_test();
    }

    #[test]
    fn milestone_nested_minor_history_preserves_major_and_abort_restores_parent() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let major = super::super::Cycle::begin(super::super::CycleKind::Major);
        let parent = snapshot().epoch;
        for _ in 0..CAPACITY * 2 {
            let minor = super::super::Cycle::begin(super::super::CycleKind::Minor);
            let child = snapshot().epoch;
            reached(GcStage::MarkComplete);
            reached(GcStage::SweepComplete);
            minor.finish(0, 0, Default::default());
            assert_eq!(record(child).unwrap().stage, 3);
            assert_eq!(record(parent).unwrap().stage, 0);
            assert!(STATE.0.lock().unwrap().records.len() <= CAPACITY);
        }
        let minor = super::super::Cycle::begin(super::super::CycleKind::Minor);
        let aborted = snapshot().epoch;
        drop(minor);
        assert_eq!(record(aborted).unwrap().aborted, 1);
        reached(GcStage::MarkComplete);
        reached(GcStage::SweepComplete);
        major.finish_concurrent(0, 0, Default::default(), 0, 0);
        quiescent();
        assert_eq!(record(parent).unwrap().stage, 3);
        assert!(wait_gc_for_test(GcTicket(parent), GcStage::Quiescent, Duration::ZERO).is_ok());
    }

    #[test]
    fn m0_c_abi_layout_evidence() {
        use super::super::*;
        use std::mem::{offset_of, size_of};
        eprintln!(
            "m0_abi V1={} V2={} V3={} milestones={} memory={} survivors={} frames={} measurements={} running={} terminal_events={} pin_reasons={}",
            size_of::<WillowGcStatsV1>(),
            size_of::<WillowGcStatsV2>(),
            size_of::<WillowGcStatsV3>(),
            offset_of!(WillowGcStatsV3, milestones),
            offset_of!(WillowGcStatsV3, memory),
            offset_of!(WillowGcStatsV3, survivors),
            offset_of!(WillowGcStatsV3, frames),
            offset_of!(WillowGcStatsV3, measurements),
            offset_of!(
                crate::frame_lifetime::FrameLifetimeSnapshot,
                task_frames_running
            ),
            offset_of!(
                crate::frame_lifetime::FrameLifetimeSnapshot,
                terminal_events
            ),
            offset_of!(GcSurvivorStats, pin_reasons)
        );
    }

    #[test]
    fn milestone_future_wait_ignores_spurious_notifications_and_wakes_on_completion() {
        use std::sync::atomic::Ordering;
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let previous = request_gc_for_test().unwrap();
        let next = GcTicket(previous.0 + 1);
        WAITS.store(0, Ordering::Relaxed);
        let observer = std::thread::spawn(move || {
            wait_gc_for_test(next, GcStage::Quiescent, Duration::from_secs(10))
        });
        let await_wait = |count| {
            let start = Instant::now();
            while WAITS.load(Ordering::Acquire) < count {
                assert!(start.elapsed() < Duration::from_secs(10));
                std::thread::yield_now();
            }
            // The count is recorded under the predicate mutex. Acquiring it
            // proves the observer crossed the atomic condvar sleep boundary.
            drop(STATE.0.lock().unwrap());
        };
        await_wait(1);
        STATE.1.notify_all();
        await_wait(2);
        assert!(!observer.is_finished());
        assert_eq!(request_gc_for_test(), Some(next));
        assert_eq!(observer.join().unwrap().unwrap().epoch, next.0);
        reset_internal_for_test();
    }

    #[test]
    fn milestone_major_empty_and_rooted_cycles_are_ordered() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let mut previous = 0;
        for rooted in [false, true] {
            let mut root = willow_alloc(8);
            if rooted {
                willow_push_root(&mut root);
            }
            let ticket = request_gc_for_test().expect("own major cycle completed");
            assert!(ticket.0 > previous);
            previous = ticket.0;
            for stage in [
                GcStage::MarkComplete,
                GcStage::SweepComplete,
                GcStage::Quiescent,
            ] {
                let record = wait_gc_for_test(ticket, stage, Duration::ZERO).unwrap();
                assert_eq!(record.epoch, ticket.0);
                assert_eq!(record.stage, 3);
                assert_eq!(record.aborted, 0);
                assert!(record.mark_complete_ns <= record.sweep_complete_ns);
                assert!(record.sweep_complete_ns <= record.quiescent_ns);
            }
            assert_eq!(willow_gc_allocated_bytes() != 0, rooted);
            if rooted {
                willow_pop_root();
            }
        }
        reset_internal_for_test();
    }

    #[test]
    fn milestone_minor_completion_and_reset_preserve_identity() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        willow_gc_minor_collect();
        let before = snapshot();
        assert_eq!(before.stage, 3);
        assert_eq!(crate::gc_telemetry::snapshot().last_cycle.kind, 1);
        reset_internal_for_test();
        let ticket = request_gc_for_test().unwrap();
        assert!(ticket.0 > before.epoch);
        assert_eq!(
            wait_gc_for_test(GcTicket(before.epoch), GcStage::Quiescent, Duration::ZERO)
                .unwrap()
                .epoch,
            before.epoch
        );
        reset_internal_for_test();
    }

    #[test]
    fn milestone_wait_timeout_invalid_and_registered_observers() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        assert_eq!(
            wait_gc_for_test(GcTicket(0), GcStage::Quiescent, Duration::ZERO).unwrap_err(),
            WaitError::Expired
        );
        for stage in [
            GcStage::MarkComplete,
            GcStage::SweepComplete,
            GcStage::Quiescent,
        ] {
            assert_eq!(
                wait_gc_for_test(GcTicket(u64::MAX), stage, Duration::ZERO).unwrap_err(),
                WaitError::Timeout
            );
        }
        let ticket = request_gc_for_test().unwrap();
        {
            let _mutator = MutatorRegistration::new();
            assert_eq!(
                wait_gc_for_test(ticket, GcStage::Quiescent, Duration::ZERO).unwrap_err(),
                WaitError::RegisteredMutator
            );
        }
        assert!(wait_gc_for_test(ticket, GcStage::Quiescent, Duration::ZERO).is_ok());
        reset_internal_for_test();
    }

    #[test]
    fn milestone_abort_never_fabricates_completion() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let epoch;
        {
            let _cycle = super::super::Cycle::begin(super::super::CycleKind::Major);
            epoch = snapshot().epoch;
            reached(GcStage::SweepComplete); // Out-of-order publication rejected.
            assert_eq!(snapshot().stage, 0);
            reached(GcStage::MarkComplete);
            reached(GcStage::MarkComplete); // Duplicate is idempotent.
            assert_eq!(snapshot().stage, 1);
        }
        assert_eq!(snapshot().aborted, 1);
        assert!(wait_gc_for_test(GcTicket(epoch), GcStage::MarkComplete, Duration::ZERO).is_ok());
        for stage in [GcStage::SweepComplete, GcStage::Quiescent] {
            assert_eq!(
                wait_gc_for_test(GcTicket(epoch), stage, Duration::ZERO).unwrap_err(),
                WaitError::Aborted
            );
        }
        assert!(request_gc_for_test().is_some());
        reset_internal_for_test();
    }

    #[test]
    fn milestone_history_is_bounded_and_old_tickets_expire() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let first = request_gc_for_test().unwrap();
        for _ in 0..CAPACITY {
            request_gc_for_test().unwrap();
        }
        assert_eq!(STATE.0.lock().unwrap().records.len(), CAPACITY);
        assert_eq!(
            wait_gc_for_test(first, GcStage::Quiescent, Duration::ZERO).unwrap_err(),
            WaitError::Expired
        );
        reset_internal_for_test();
    }

    #[test]
    fn milestone_v3_negotiation_is_unaligned_and_preserves_nested_versions() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let ticket = request_gc_for_test().unwrap();
        let size = std::mem::size_of::<super::super::WillowGcStatsV3>();
        assert_eq!(super::super::willow_gc_stats_size(3), size as i64);
        let mut bytes = vec![0xa5; size + 2];
        let out = unsafe { bytes.as_mut_ptr().add(1) };
        assert_eq!(
            super::super::willow_gc_stats_snapshot(3, out, size as i64 - 1),
            0
        );
        assert!(bytes.iter().all(|&b| b == 0xa5));
        assert_eq!(
            super::super::willow_gc_stats_snapshot(3, out, size as i64),
            size as i64
        );
        let stats = unsafe { out.cast::<super::super::WillowGcStatsV3>().read_unaligned() };
        assert_eq!((stats.version, stats.struct_size), (3, size as u32));
        assert_eq!(stats.baseline.version, 2);
        assert_eq!(
            stats.baseline.struct_size as usize,
            std::mem::size_of::<super::super::WillowGcStatsV2>()
        );
        assert_eq!(stats.baseline.baseline.version, 1);
        assert_eq!(stats.milestones.epoch, ticket.0);
        assert_eq!(stats.milestones.stage, 3);
        assert_eq!((bytes[0], bytes[size + 1]), (0xa5, 0xa5));
        reset_internal_for_test();
    }

    #[test]
    fn milestone_json_has_distinct_epoch_tagged_events() {
        let mut bytes = Vec::new();
        let record = CycleMilestones {
            epoch: 17,
            mark_complete_ns: 10,
            sweep_complete_ns: 20,
            quiescent_ns: 30,
            stage: 3,
            aborted: 0,
        };
        super::super::write_milestones(&mut bytes, record).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.lines().count(), 3);
        for (line, (name, time)) in text.lines().zip([
            ("MarkComplete", 10),
            ("SweepComplete", 20),
            ("Quiescent", 30),
        ]) {
            assert_eq!(
                line,
                format!(
                    "{{\"version\":3,\"event\":\"{name}\",\"epoch\":17,\"timestamp_ns\":{time}}}"
                )
            );
        }
    }
}
