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
pub(crate) struct FrameLifetime {
    counts: counts::Counters,
    capture: AtomicBool,
    events: Mutex<VecDeque<FrameEvent>>,
}
impl FrameLifetime {
    pub(crate) const fn new() -> Self {
        Self {
            counts: counts::Counters::new(),
            capture: AtomicBool::new(false),
            events: Mutex::new(VecDeque::new()),
        }
    }
}
fn runtime() -> &'static FrameLifetime {
    &crate::runtime::current().frame_lifetime
}

macro_rules! counters {
    (striped: [$($hot:ident),+ $(,)?], global: [$($field:ident),+ $(,)?] $(,)?) => {
        /// Per-Task events run on every worker, so each thread adds to its own
        /// stripe and snapshots pay for the sum.
        #[repr(align(128))]
        struct CounterShard {
            entered: AtomicU64,
            exited: AtomicU64,
            $($hot: AtomicU64,)+
        }
        pub(super) struct Counters {
            $($field: AtomicU64,)+
            shards: [CounterShard; COUNTER_SHARD_COUNT],
        }
        impl Counters {
            pub(super) const fn new() -> Self {
                Self {
                    $($field: AtomicU64::new(0),)+
                    shards: [const {
                        CounterShard {
                            entered: AtomicU64::new(0),
                            exited: AtomicU64::new(0),
                            $($hot: AtomicU64::new(0),)+
                        }
                    }; COUNTER_SHARD_COUNT],
                }
            }
        }
        #[derive(Debug, Clone, Copy, Default)]
        #[repr(C)]
        pub struct FrameLifetimeSnapshot {
            $(pub $hot: u64,)+
            $(pub $field: u64,)+
            pub task_frames_running: u64,
            pub poll_exits: u64,
            pub task_frames_rooted: u64,
            pub task_frames_retired: u64,
            pub task_frames_terminal: u64,
        }
        /// Atomic counters can span adjacent events during concurrent activity.
        /// Exact accounting assertions must take snapshots at a rendezvous.
        impl Counters {
            pub(super) fn snapshot(&self) -> FrameLifetimeSnapshot {
                let mut s = FrameLifetimeSnapshot {
                    $($hot: self.shards
                        .iter()
                        .map(|shard| shard.$hot.load(Ordering::Acquire))
                        .sum(),)+
                    $($field: self.$field.load(Ordering::Acquire),)+
                    ..Default::default()
                };
                let (entered, exited) = self.poll_totals();
                s.task_frames_running = entered.saturating_sub(exited);
                s.poll_exits = exited;
                s.task_frames_rooted = s.scheduler_roots_added.saturating_sub(s.scheduler_roots_released);
                s.task_frames_retired = s.terminal_events.saturating_sub(s.scheduler_roots_released);
                s.task_frames_terminal = s.terminal_events.saturating_sub(s.terminal_frames_reclaimed);
                s
            }
        }
        #[cfg(test)]
        #[test]
        fn maximum_counter_diagnostic_is_bounded() {
            let counters = FrameLifetimeSnapshot {
                $($hot: u64::MAX,)+
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
        impl Counters {
            pub(super) fn reset(&self) {
                $(self.$field.store(0, Ordering::Release);)+
                for shard in &self.shards {
                    shard.entered.store(0, Ordering::Release);
                    shard.exited.store(0, Ordering::Release);
                    $(shard.$hot.store(0, Ordering::Release);)+
                }
            }
        }
    };
}

const COUNTER_SHARD_COUNT: usize = 64;
static NEXT_COUNTER_SHARD: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static COUNTER_SHARD: Cell<usize> = const { Cell::new(usize::MAX) };
}

fn counter_shard_index() -> usize {
    COUNTER_SHARD.with(|slot| {
        let mut index = slot.get();
        if index == usize::MAX {
            index = NEXT_COUNTER_SHARD.fetch_add(1, Ordering::Relaxed) % COUNTER_SHARD_COUNT;
            slot.set(index);
        }
        index
    })
}

mod counts {
    use super::*;
    counters! {
        striped: [
            frames_created, allocated_frame_bytes,
            terminal_events,
            scheduler_roots_added, scheduler_roots_released,
            quiescent_events, scope_owner_releases,
        ],
        global: [
            terminal_frames_reclaimed,
            metadata_removed_frames, metadata_removed_bytes,
            quarantined_frames, quarantined_bytes, pending_drops,
            task_frames_reclaimed, reclaimed_frame_bytes, sweep_completions,
            quarantined_terminal_frames,
        ],
    }
    /// Sum exits before entries: an exit is counted only after its entry, so a
    /// concurrent snapshot never observes more exits than entries.
    impl Counters {
        fn poll_totals(&self) -> (u64, u64) {
            let exited = self
                .shards
                .iter()
                .map(|shard| shard.exited.load(Ordering::Acquire))
                .sum::<u64>();
            let entered = self
                .shards
                .iter()
                .map(|shard| shard.entered.load(Ordering::Acquire))
                .sum::<u64>();
            (entered, exited)
        }
        pub(super) fn record(&self, kind: FrameEventKind, bytes: u64) {
            let shard = || &self.shards[counter_shard_index()];
            let increment = |counter: &AtomicU64| {
                counter.fetch_add(1, Ordering::AcqRel);
            };
            match kind {
                FrameEventKind::Created => {
                    let shard = shard();
                    increment(&shard.frames_created);
                    shard
                        .allocated_frame_bytes
                        .fetch_add(bytes, Ordering::AcqRel);
                }
                FrameEventKind::SchedulerRootAdded => increment(&shard().scheduler_roots_added),
                FrameEventKind::PollEntered => {
                    shard().entered.fetch_add(1, Ordering::Release);
                }
                FrameEventKind::PollExited => {
                    shard().exited.fetch_add(1, Ordering::Release);
                }
                FrameEventKind::Terminal => increment(&shard().terminal_events),
                FrameEventKind::Quiescent => increment(&shard().quiescent_events),
                FrameEventKind::SchedulerRootReleased => {
                    increment(&shard().scheduler_roots_released)
                }
                FrameEventKind::ScopeOwnerReleased => increment(&shard().scope_owner_releases),
                FrameEventKind::LogicallyDead => {}
                FrameEventKind::DropPending => increment(&self.pending_drops),
                FrameEventKind::MetadataRemoved => {
                    increment(&self.metadata_removed_frames);
                    self.metadata_removed_bytes
                        .fetch_add(bytes, Ordering::AcqRel);
                    increment(&self.quarantined_frames);
                    self.quarantined_bytes.fetch_add(bytes, Ordering::AcqRel);
                }
                FrameEventKind::DropCompleted => {
                    self.pending_drops.fetch_sub(1, Ordering::AcqRel);
                }
                FrameEventKind::SweepCompleted => {
                    let frames = self.quarantined_frames.swap(0, Ordering::AcqRel);
                    let bytes = self.quarantined_bytes.swap(0, Ordering::AcqRel);
                    self.task_frames_reclaimed
                        .fetch_add(frames, Ordering::AcqRel);
                    self.reclaimed_frame_bytes
                        .fetch_add(bytes, Ordering::AcqRel);
                    self.terminal_frames_reclaimed.fetch_add(
                        self.quarantined_terminal_frames.swap(0, Ordering::AcqRel),
                        Ordering::AcqRel,
                    );
                    increment(&self.sweep_completions);
                }
            }
        }
        pub(super) fn terminal_reclaimed(&self) {
            self.quarantined_terminal_frames
                .fetch_add(1, Ordering::AcqRel);
        }
    }
}
pub use counts::FrameLifetimeSnapshot;
pub fn snapshot() -> FrameLifetimeSnapshot {
    runtime().snapshot()
}

impl FrameLifetime {
    pub(crate) fn snapshot(&self) -> FrameLifetimeSnapshot {
        self.counts.snapshot()
    }
}
#[cfg(test)]
pub(crate) fn reset() {
    runtime().counts.reset();
    runtime()
        .events
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    runtime().capture.store(false, Ordering::Release);
}

/// Enable bounded event capture. Counters remain available when disabled.
pub fn capture_events(enabled: bool) {
    runtime().capture_events(enabled);
}
impl FrameLifetime {
    fn capture_events(&self, enabled: bool) {
        if enabled {
            // Reserve before publishing capture: native destructor diagnostics
            // must never allocate while the drop hook owns quarantined storage.
            let mut events = self.events.lock().unwrap_or_else(|p| p.into_inner());
            let additional = EVENT_LIMIT.saturating_sub(events.capacity());
            events.reserve_exact(additional);
            self.capture.store(true, Ordering::Release);
        } else {
            self.capture.store(false, Ordering::Release);
            self.events
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clear();
        }
    }
    fn events(&self) -> Vec<FrameEvent> {
        self.events
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .copied()
            .collect()
    }
}
pub fn events() -> Vec<FrameEvent> {
    runtime().events()
}
pub(crate) fn record(kind: FrameEventKind, frame: usize, epoch: u64, bytes: u64) {
    runtime().record_with_sweep(kind, frame, epoch, 0, bytes);
}
pub(crate) fn record_sweep(kind: FrameEventKind, frame: usize, sweep_epoch: u64, bytes: u64) {
    runtime().record_sweep(kind, frame, sweep_epoch, bytes);
}
impl FrameLifetime {
    pub(crate) fn record_sweep(
        &self,
        kind: FrameEventKind,
        frame: usize,
        sweep_epoch: u64,
        bytes: u64,
    ) {
        self.record_with_sweep(
            kind,
            frame,
            crate::frame_reclaim::current_epoch(),
            sweep_epoch,
            bytes,
        );
    }

    pub(crate) fn record_with_sweep(
        &self,
        kind: FrameEventKind,
        frame: usize,
        epoch: u64,
        sweep_epoch: u64,
        bytes: u64,
    ) {
        self.counts.record(kind, bytes);
        if self.capture.load(Ordering::Acquire) {
            self.push_event(FrameEvent {
                kind,
                epoch,
                sweep_epoch,
                frame,
                bytes,
            });
        }
    }
    fn push_event(&self, event: FrameEvent) {
        let mut events = self.events.lock().unwrap_or_else(|p| p.into_inner());
        if events.len() == EVENT_LIMIT {
            events.pop_front();
        }
        events.push_back(event);
    }
}
pub(crate) fn metadata_removed(frame: usize, bytes: usize, terminal: bool, sweep_epoch: u64) {
    runtime().metadata_removed(frame, bytes, terminal, sweep_epoch);
}
pub(crate) fn pending_drop(frame: usize, sweep_epoch: u64) {
    runtime().pending_drop(frame, sweep_epoch);
}
impl FrameLifetime {
    pub(crate) fn metadata_removed(
        &self,
        frame: usize,
        bytes: usize,
        terminal: bool,
        sweep_epoch: u64,
    ) {
        #[cfg(feature = "borrow-migration-probe")]
        crate::borrow_migration_probe::removed(frame);
        if terminal {
            self.counts.terminal_reclaimed();
        }
        self.record_sweep(
            FrameEventKind::LogicallyDead,
            frame,
            sweep_epoch,
            bytes as u64,
        );
        self.record_sweep(
            FrameEventKind::MetadataRemoved,
            frame,
            sweep_epoch,
            bytes as u64,
        );
    }
    pub(crate) fn pending_drop(&self, frame: usize, sweep_epoch: u64) {
        self.record_sweep(FrameEventKind::DropPending, frame, sweep_epoch, 0);
    }
}

pub(crate) struct PollLifetime<'a> {
    frame: usize,
    owner: &'a FrameLifetime,
}
impl PollLifetime<'static> {
    pub(crate) fn enter(frame: usize) -> Self {
        Self::enter_in(runtime(), frame)
    }
}
impl<'a> PollLifetime<'a> {
    pub(crate) fn enter_in(owner: &'a FrameLifetime, frame: usize) -> Self {
        if frame != 0 {
            owner.record_poll(FrameEventKind::PollEntered, frame);
        }
        Self { frame, owner }
    }
}
impl Drop for PollLifetime<'_> {
    #[inline]
    fn drop(&mut self) {
        if self.frame != 0 {
            self.owner
                .record_poll(FrameEventKind::PollExited, self.frame);
        }
    }
}

impl FrameLifetime {
    /// Poll events read the retire epoch only when events are being captured.
    #[inline]
    fn record_poll(&self, kind: FrameEventKind, frame: usize) {
        self.counts.record(kind, 0);
        if self.capture.load(Ordering::Acquire) {
            self.capture_poll(kind, frame);
        }
    }

    #[cold]
    #[inline(never)]
    fn capture_poll(&self, kind: FrameEventKind, frame: usize) {
        self.push_event(FrameEvent {
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

#[cfg(test)]
mod owner_tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn frame_lifetime_owners_isolate_concurrent_counts_and_bounded_events() {
        let barrier = Barrier::new(2);
        std::thread::scope(|scope| {
            for bytes in [16, 48] {
                let barrier = &barrier;
                scope.spawn(crate::runtime::serving(move || {
                    let owner = FrameLifetime::new();
                    owner.capture_events(true);
                    barrier.wait();
                    for frame in 1..=EVENT_LIMIT + 17 {
                        owner.record_with_sweep(FrameEventKind::Created, frame, 7, 0, bytes);
                        owner.record_with_sweep(FrameEventKind::PollEntered, frame, 7, 0, 0);
                        owner.record_with_sweep(FrameEventKind::PollExited, frame, 7, 0, 0);
                    }
                    barrier.wait();
                    let snapshot = owner.counts.snapshot();
                    assert_eq!(snapshot.frames_created, (EVENT_LIMIT + 17) as u64);
                    assert_eq!(
                        snapshot.allocated_frame_bytes,
                        snapshot.frames_created * bytes
                    );
                    assert_eq!(snapshot.task_frames_running, 0);
                    assert_eq!(snapshot.poll_exits, snapshot.frames_created);
                    let events = owner.events();
                    assert_eq!(events.len(), EVENT_LIMIT);
                    assert_eq!(events.last().unwrap().frame, EVENT_LIMIT + 17);
                    assert!(
                        events
                            .iter()
                            .filter(|event| event.kind == FrameEventKind::Created)
                            .all(|event| event.bytes == bytes)
                    );
                    owner.capture_events(false);
                    assert!(owner.events().is_empty());
                    assert_eq!(
                        owner.counts.snapshot().frames_created,
                        snapshot.frames_created
                    );
                }));
            }
        });
    }

    #[test]
    fn frame_lifetime_poll_guards_retain_owner_across_nesting_and_unwind() {
        let first = FrameLifetime::new();
        let second = FrameLifetime::new();
        first.capture_events(true);
        second.capture_events(true);
        let outer = PollLifetime::enter_in(&first, 11);
        {
            let _other = PollLifetime::enter_in(&second, 22);
            let _nested = PollLifetime::enter_in(&first, 33);
            assert_eq!(first.counts.snapshot().task_frames_running, 2);
            assert_eq!(second.counts.snapshot().task_frames_running, 1);
        }
        let unwind = std::panic::catch_unwind(|| {
            let _poll = PollLifetime::enter_in(&second, 44);
            panic!("poll unwind");
        });
        assert!(unwind.is_err());
        drop(PollLifetime::enter_in(&first, 0));
        assert_eq!(first.counts.snapshot().task_frames_running, 1);
        drop(outer);
        assert_eq!(first.counts.snapshot().task_frames_running, 0);
        assert_eq!(second.counts.snapshot().task_frames_running, 0);
        assert_eq!(first.counts.snapshot().poll_exits, 2);
        assert_eq!(second.counts.snapshot().poll_exits, 2);
        assert!(
            first
                .events()
                .iter()
                .all(|event| [11, 33].contains(&event.frame))
        );
        assert!(
            second
                .events()
                .iter()
                .all(|event| [22, 44].contains(&event.frame))
        );
    }

    #[test]
    fn frame_lifetime_owner_sweep_drains_only_its_quarantine() {
        let owners = [FrameLifetime::new(), FrameLifetime::new()];
        for (index, owner) in owners.iter().enumerate() {
            owner.record_with_sweep(FrameEventKind::MetadataRemoved, index, 2, 3, 32);
            owner.counts.terminal_reclaimed();
            owner.record_with_sweep(FrameEventKind::DropPending, index, 2, 3, 0);
        }
        owners[0].record_with_sweep(FrameEventKind::DropCompleted, 0, 2, 3, 0);
        owners[0].record_with_sweep(FrameEventKind::SweepCompleted, 0, 2, 3, 0);
        let first = owners[0].counts.snapshot();
        let second = owners[1].counts.snapshot();
        assert_eq!((first.quarantined_frames, first.pending_drops), (0, 0));
        assert_eq!(
            (first.task_frames_reclaimed, first.terminal_frames_reclaimed),
            (1, 1)
        );
        assert_eq!(first.reclaimed_frame_bytes, 32);
        assert_eq!((second.quarantined_frames, second.pending_drops), (1, 1));
        assert_eq!(
            (
                second.task_frames_reclaimed,
                second.terminal_frames_reclaimed
            ),
            (0, 0)
        );
        assert_eq!(second.sweep_completions, 0);
    }
}
