//! Task-frame diagnostics. Counters never own GC references. Optional events
//! retain only integer identities in a bounded ring, so diagnostics cannot
//! manufacture a permanent root or grow with completed tasks.
use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameEventKind {
    Created,
    SchedulerRootAdded,
    PollEntered,
    PollExited,
    Terminal,
    Quiescent,
    SchedulerRootReleased,
    ScopeOwnerReleased,
    LogicallyDead,
    MetadataRemoved,
    DropPending,
    DropCompleted,
    SweepCompleted,
}

#[derive(Debug, Clone, Copy)]
pub struct FrameEvent {
    pub kind: FrameEventKind,
    /// Retire epoch, not a GC mark epoch. Equal tags relate lifetime events.
    pub epoch: u64,
    /// Major collection number for sweep events; zero for scheduler events.
    pub sweep_epoch: u64,
    /// Integer diagnostic identity only; never dereference an event address.
    pub frame: usize,
    pub bytes: u64,
}

const EVENT_LIMIT: usize = 512;
static CAPTURE: AtomicBool = AtomicBool::new(false);
static EVENTS: Mutex<VecDeque<FrameEvent>> = Mutex::new(VecDeque::new());

macro_rules! counters {
    ($($field:ident),+ $(,)?) => {
        $(static $field: AtomicU64 = AtomicU64::new(0);)+
        #[derive(Debug, Clone, Copy, Default)]
        pub struct FrameLifetimeSnapshot {
            $(pub $field: u64,)+
            pub task_frames_running: u64,
            pub poll_exits: u64,
            pub task_frames_rooted: u64,
            pub task_frames_retired: u64,
            pub task_frames_terminal: u64,
        }
        /// Atomic counters can span adjacent events during concurrent activity.
        /// Exact accounting assertions must take snapshots at a rendezvous.
        pub fn snapshot() -> FrameLifetimeSnapshot {
            let mut s = FrameLifetimeSnapshot {
                $($field: $field.load(Ordering::Acquire),)+
                ..Default::default()
            };
            let (entered, exited) = poll_totals();
            s.task_frames_running = entered.saturating_sub(exited);
            s.poll_exits = exited;
            s.task_frames_rooted = s.scheduler_roots_added.saturating_sub(s.scheduler_roots_released);
            s.task_frames_retired = s.terminal_events.saturating_sub(s.scheduler_roots_released);
            s.task_frames_terminal = s.terminal_events.saturating_sub(s.terminal_frames_reclaimed);
            s
        }
        #[cfg(test)]
        #[test]
        fn maximum_counter_diagnostic_is_bounded() {
            let counters = FrameLifetimeSnapshot {
                $($field: u64::MAX,)+
                task_frames_running: u64::MAX,
                poll_exits: u64::MAX,
                task_frames_rooted: u64::MAX,
                task_frames_retired: u64::MAX,
                task_frames_terminal: u64::MAX,
            };
            let text = diagnostic_text(counters);
            assert!(text.len() <= 2048, "{} bytes", text.len());
            assert_eq!(text.lines().count(), 1);
            assert!(text.contains("allocated_frame_bytes: 18446744073709551615"));
            assert!(text.contains("reclaimed_frame_bytes: 18446744073709551615"));
        }
        #[cfg(test)]
        pub(crate) fn reset() {
            $($field.store(0, Ordering::Release);)+
            for shard in &POLL_SHARDS {
                shard.entered.store(0, Ordering::Release);
                shard.exited.store(0, Ordering::Release);
            }
            EVENTS.lock().unwrap_or_else(|p| p.into_inner()).clear();
            CAPTURE.store(false, Ordering::Release);
        }
    };
}

/// Poll entry/exit counts run on every task poll of every worker, so they are
/// striped per thread instead of contending on two process-global words
/// (willow-a6o0). Snapshots sum the stripes: running = entered - exited.
const POLL_SHARD_COUNT: usize = 64;

#[repr(align(128))]
struct PollShard {
    entered: AtomicU64,
    exited: AtomicU64,
}

static POLL_SHARDS: [PollShard; POLL_SHARD_COUNT] = [const {
    PollShard {
        entered: AtomicU64::new(0),
        exited: AtomicU64::new(0),
    }
}; POLL_SHARD_COUNT];
static NEXT_POLL_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static POLL_SHARD: Cell<usize> = const { Cell::new(usize::MAX) };
}

fn poll_shard() -> &'static PollShard {
    let index = POLL_SHARD.with(|slot| {
        let mut index = slot.get();
        if index == usize::MAX {
            index = NEXT_POLL_SHARD.fetch_add(1, Ordering::Relaxed) % POLL_SHARD_COUNT;
            slot.set(index);
        }
        index
    });
    &POLL_SHARDS[index]
}

/// Sum exits before entries: an exit is counted only after its entry, so a
/// concurrent snapshot never observes more exits than entries.
fn poll_totals() -> (u64, u64) {
    let exited = POLL_SHARDS
        .iter()
        .map(|shard| shard.exited.load(Ordering::Acquire))
        .sum::<u64>();
    let entered = POLL_SHARDS
        .iter()
        .map(|shard| shard.entered.load(Ordering::Acquire))
        .sum::<u64>();
    (entered, exited)
}

// Lowercase names deliberately match the public snapshot fields.
#[allow(non_upper_case_globals)]
mod counts {
    use super::*;
    counters! {
        frames_created, allocated_frame_bytes,
        terminal_events, terminal_frames_reclaimed,
        scheduler_roots_added, scheduler_roots_released,
        quiescent_events, scope_owner_releases,
        metadata_removed_frames, metadata_removed_bytes,
        quarantined_frames, quarantined_bytes, pending_drops,
        task_frames_reclaimed, reclaimed_frame_bytes, sweep_completions,
        quarantined_terminal_frames,
    }
    pub(super) fn record(kind: FrameEventKind, bytes: u64) {
        let increment = |counter: &AtomicU64| {
            counter.fetch_add(1, Ordering::AcqRel);
        };
        match kind {
            FrameEventKind::Created => {
                increment(&frames_created);
                allocated_frame_bytes.fetch_add(bytes, Ordering::AcqRel);
            }
            FrameEventKind::SchedulerRootAdded => increment(&scheduler_roots_added),
            FrameEventKind::PollEntered => {
                poll_shard().entered.fetch_add(1, Ordering::Release);
            }
            FrameEventKind::PollExited => {
                poll_shard().exited.fetch_add(1, Ordering::Release);
            }
            FrameEventKind::Terminal => increment(&terminal_events),
            FrameEventKind::Quiescent => increment(&quiescent_events),
            FrameEventKind::SchedulerRootReleased => increment(&scheduler_roots_released),
            FrameEventKind::ScopeOwnerReleased => increment(&scope_owner_releases),
            FrameEventKind::LogicallyDead => {}
            FrameEventKind::DropPending => increment(&pending_drops),
            FrameEventKind::MetadataRemoved => {
                increment(&metadata_removed_frames);
                metadata_removed_bytes.fetch_add(bytes, Ordering::AcqRel);
                increment(&quarantined_frames);
                quarantined_bytes.fetch_add(bytes, Ordering::AcqRel);
            }
            FrameEventKind::DropCompleted => {
                pending_drops.fetch_sub(1, Ordering::AcqRel);
            }
            FrameEventKind::SweepCompleted => {
                let frames = quarantined_frames.swap(0, Ordering::AcqRel);
                let bytes = quarantined_bytes.swap(0, Ordering::AcqRel);
                task_frames_reclaimed.fetch_add(frames, Ordering::AcqRel);
                reclaimed_frame_bytes.fetch_add(bytes, Ordering::AcqRel);
                terminal_frames_reclaimed.fetch_add(
                    quarantined_terminal_frames.swap(0, Ordering::AcqRel),
                    Ordering::AcqRel,
                );
                increment(&sweep_completions);
            }
        }
    }
    pub(super) fn terminal_reclaimed() {
        quarantined_terminal_frames.fetch_add(1, Ordering::AcqRel);
    }
}
#[cfg(test)]
pub(crate) use counts::reset;
pub use counts::{FrameLifetimeSnapshot, snapshot};

/// Enable bounded event capture. Counters remain available when disabled.
pub fn capture_events(enabled: bool) {
    if enabled {
        // Reserve before publishing capture: native destructor diagnostics
        // must never allocate while the drop hook owns quarantined storage.
        let mut events = EVENTS.lock().unwrap_or_else(|p| p.into_inner());
        let additional = EVENT_LIMIT.saturating_sub(events.capacity());
        events.reserve_exact(additional);
        CAPTURE.store(true, Ordering::Release);
    } else {
        CAPTURE.store(false, Ordering::Release);
        EVENTS.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}
pub fn events() -> Vec<FrameEvent> {
    EVENTS
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .iter()
        .copied()
        .collect()
}
pub(crate) fn record(kind: FrameEventKind, frame: usize, epoch: u64, bytes: u64) {
    record_with_sweep(kind, frame, epoch, 0, bytes);
}
pub(crate) fn record_sweep(kind: FrameEventKind, frame: usize, sweep_epoch: u64, bytes: u64) {
    record_with_sweep(
        kind,
        frame,
        crate::frame_reclaim::current_epoch(),
        sweep_epoch,
        bytes,
    );
}
fn record_with_sweep(kind: FrameEventKind, frame: usize, epoch: u64, sweep_epoch: u64, bytes: u64) {
    counts::record(kind, bytes);
    if CAPTURE.load(Ordering::Acquire) {
        push_event(FrameEvent {
            kind,
            epoch,
            sweep_epoch,
            frame,
            bytes,
        });
    }
}
fn push_event(event: FrameEvent) {
    let mut events = EVENTS.lock().unwrap_or_else(|p| p.into_inner());
    if events.len() == EVENT_LIMIT {
        events.pop_front();
    }
    events.push_back(event);
}
pub(crate) fn metadata_removed(frame: usize, bytes: usize, terminal: bool, sweep_epoch: u64) {
    if terminal {
        counts::terminal_reclaimed();
    }
    record_sweep(
        FrameEventKind::LogicallyDead,
        frame,
        sweep_epoch,
        bytes as u64,
    );
    record_sweep(
        FrameEventKind::MetadataRemoved,
        frame,
        sweep_epoch,
        bytes as u64,
    );
}
pub(crate) fn pending_drop(frame: usize, sweep_epoch: u64) {
    record_sweep(FrameEventKind::DropPending, frame, sweep_epoch, 0);
}

pub(crate) struct PollLifetime(usize);
impl PollLifetime {
    pub(crate) fn enter(frame: usize) -> Self {
        if frame != 0 {
            record_poll(FrameEventKind::PollEntered, frame);
        }
        Self(frame)
    }
}
impl Drop for PollLifetime {
    fn drop(&mut self) {
        if self.0 != 0 {
            record_poll(FrameEventKind::PollExited, self.0);
        }
    }
}

/// Poll events read the retire epoch only when events are being captured.
fn record_poll(kind: FrameEventKind, frame: usize) {
    counts::record(kind, 0);
    if CAPTURE.load(Ordering::Acquire) {
        push_event(FrameEvent {
            kind,
            epoch: crate::frame_reclaim::current_epoch(),
            sweep_epoch: 0,
            frame,
            bytes: 0,
        });
    }
}

/// Emit one bounded, process-local lifecycle snapshot for reclamation failures.
/// Atomic updates may span adjacent events; this neither collects nor captures
/// event history. All fields are fixed-size u64 counters (at most 20 digits).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_task_frame_diagnostics() {
    let counters = snapshot();
    crate::print::write_stdout(diagnostic_text(counters));
}

fn diagnostic_text(counters: FrameLifetimeSnapshot) -> String {
    format!("task_frame_lifetime_v1 {counters:?}\n")
}
