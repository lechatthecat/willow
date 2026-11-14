//! Grace periods for retiring terminal task-frame runtime roots.
//!
//! A task frame is a runtime GC root while its task is live. Once the task
//! is terminal the frame stays alive only through whoever still holds the
//! `Task` handle, but code that was already executing when the task finished
//! may hold the frame pointer somewhere the collector does not scan: the
//! spawner between `willow_sched_spawn*` and storing the handle, a poll that
//! keeps an awaitee frame in a register across `willow_frame_await`, or a poll
//! paused inside a nested scheduler drive. The root therefore outlives the
//! terminal transition until every such execution has ended.
//!
//! The previous rule waited for the outermost drive to return, which a
//! long-running `async main` never does, so every finished frame stayed rooted
//! forever. This module bounds the wait by live work instead, using
//! quiescent-state reclamation:
//!
//! - An [`ExecutionUnit`] brackets each stretch of runtime/generated code that
//!   can obtain a frame pointer while tasks finish concurrently: one task poll
//!   or cancellation cleanup in the run loop, and every spawn. Entering the
//!   outermost unit on a thread publishes the current retire epoch in that
//!   thread's slot; leaving it marks the slot idle. Nested units (a spawn or a
//!   nested drive inside a poll) leave the outer epoch in place.
//! - Retiring a frame takes a tag from the same epoch counter.
//! - A retired frame may be unrooted once its tag is below every active slot.
//!
//! Every unit that can hold an unrooted pointer to a frame began before that
//! frame was retired (the spawner precedes publication, a poll that reads
//! the frame from task state precedes its completion), so it published an
//! epoch no greater than the frame's tag and keeps the root alive until it
//! exits. A unit that begins later can only reach the frame through a scanned
//! root, which keeps the frame alive without the runtime root.

use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Slot value of a thread outside every execution unit.
const IDLE: u64 = u64::MAX;

/// One runtime's frame grace periods. Retained scheduler storage shares the
/// domain, while participants borrow it for their execution lifetime.
#[derive(Debug)]
pub(crate) struct Domain {
    epoch: AtomicU64,
    slots: Mutex<Vec<Arc<EpochSlot>>>,
}

impl Domain {
    pub(crate) const fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            slots: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn participant(&self) -> Participant<'_> {
        Participant {
            unit: ThreadUnit::register(self),
            _thread: PhantomData,
        }
    }

    pub(crate) fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub(crate) fn retire_tag(&self) -> u64 {
        self.epoch.fetch_add(1, Ordering::AcqRel)
    }

    pub(crate) fn release_bound(&self) -> u64 {
        let epoch = self.current_epoch();
        let slots = self
            .slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        slots
            .iter()
            .map(|slot| slot.epoch.load(Ordering::Acquire))
            .fold(epoch, u64::min)
    }
}

pub(crate) struct Participant<'a> {
    unit: ThreadUnit<'a>,
    _thread: PhantomData<Rc<()>>,
}

impl Participant<'_> {
    pub(crate) fn enter(&self) -> ParticipantUnit<'_> {
        self.unit.enter();
        ParticipantUnit { participant: self }
    }
}

#[must_use = "dropping the unit ends it immediately"]
pub(crate) struct ParticipantUnit<'a> {
    participant: &'a Participant<'a>,
}

impl Drop for ParticipantUnit<'_> {
    fn drop(&mut self) {
        self.participant.unit.leave();
    }
}

fn default_domain() -> &'static Domain {
    &crate::runtime::current().frame_reclaim
}

/// Padded so one worker's per-poll store does not share a cache line with
/// another worker's slot.
#[derive(Debug)]
#[repr(align(128))]
struct EpochSlot {
    epoch: AtomicU64,
    index: AtomicUsize,
}

struct ThreadUnit<'a> {
    domain: &'a Domain,
    depth: Cell<usize>,
    slot: Arc<EpochSlot>,
}

impl<'a> ThreadUnit<'a> {
    fn register(domain: &'a Domain) -> Self {
        let mut slots = domain
            .slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let slot = Arc::new(EpochSlot {
            epoch: AtomicU64::new(IDLE),
            index: AtomicUsize::new(slots.len()),
        });
        slots.push(Arc::clone(&slot));
        Self {
            domain,
            depth: Cell::new(0),
            slot,
        }
    }

    fn enter(&self) {
        let depth = self.depth.get();
        if depth == 0 {
            self.slot
                .epoch
                .store(self.domain.current_epoch(), Ordering::Release);
        }
        self.depth.set(depth + 1);
    }

    fn leave(&self) {
        let depth = self.depth.get() - 1;
        self.depth.set(depth);
        if depth == 0 {
            self.slot.epoch.store(IDLE, Ordering::Release);
        }
    }
}

impl Drop for ThreadUnit<'_> {
    fn drop(&mut self) {
        self.slot.epoch.store(IDLE, Ordering::Release);
        let mut slots = self
            .domain
            .slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let index = self.slot.index.load(Ordering::Relaxed);
        slots.swap_remove(index);
        if let Some(moved) = slots.get(index) {
            moved.index.store(index, Ordering::Relaxed);
        }
    }
}

thread_local! {
    static THREAD_UNIT: ThreadUnit<'static> = ThreadUnit::register(default_domain());
}

/// RAII bracket for one execution unit. See the module documentation.
#[must_use = "dropping the unit ends it immediately"]
pub(crate) struct ExecutionUnit {
    entered: bool,
    _thread: PhantomData<Rc<()>>,
}

impl ExecutionUnit {
    pub(crate) fn enter() -> Self {
        // `try_with` fails only while this thread's TLS is being destroyed;
        // such a thread runs no further polls or spawns.
        let entered = THREAD_UNIT.try_with(|unit| unit.enter()).is_ok();
        Self {
            entered,
            _thread: PhantomData,
        }
    }
}

impl Drop for ExecutionUnit {
    fn drop(&mut self) {
        if !self.entered {
            return;
        }
        let _ = THREAD_UNIT.try_with(ThreadUnit::leave);
    }
}

/// Current retire epoch for diagnostics; reading it does not retire a frame.
pub(crate) fn current_epoch() -> u64 {
    default_domain().current_epoch()
}

/// Tag for a frame retired now. Callers retire under their queue's lock, so
/// each queue stays ordered by tag.
pub(crate) fn retire_tag() -> u64 {
    default_domain().retire_tag()
}

/// Frames retired with a tag below the returned bound have no unit left that
/// started before their retirement.
///
/// The epoch is read before the slots: a frame tagged below that snapshot was
/// retired before the read, so any unit holding it published its slot before
/// this scan looks at it. Without the snapshot, a scan that saw every slot
/// idle would also release frames retired after it, under units that began
/// after it. O(live participants); departing participants unregister in O(1).
pub(crate) fn release_bound() -> u64 {
    default_domain().release_bound()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::isolated_runtime;
    use std::sync::Barrier;

    // Perspectives: an idle process bounds nothing it has not
    // tagged yet; a unit holds back exactly the frames retired after it began;
    // nesting keeps the outer epoch; another thread's unit counts; a thread
    // that dies inside a unit cannot wedge reclamation.

    #[test]
    fn idle_bound_covers_every_frame_already_retired() {
        let domain = Domain::new();
        let tag = domain.retire_tag();
        assert!(domain.release_bound() > tag);
    }

    #[test]
    fn retire_tags_are_strictly_increasing() {
        let domain = Domain::new();
        let first = domain.retire_tag();
        assert!(domain.retire_tag() > first);
    }

    #[test]
    fn active_unit_holds_back_frames_retired_during_it() {
        let domain = Domain::new();
        let participant = domain.participant();
        let before = domain.retire_tag();
        let unit = participant.enter();
        let during = domain.retire_tag();
        let bound = domain.release_bound();
        assert!(bound > before);
        assert!(bound <= during);
        drop(unit);
        assert!(domain.release_bound() > during);
    }

    #[test]
    fn nested_unit_keeps_the_outer_epoch() {
        let domain = Domain::new();
        let participant = domain.participant();
        let outer = participant.enter();
        let first = domain.retire_tag();
        let inner = participant.enter();
        let second = domain.retire_tag();
        drop(inner);
        assert!(domain.release_bound() <= first);
        drop(outer);
        assert!(domain.release_bound() > second);
    }

    #[test]
    fn unit_on_another_thread_holds_back_release() {
        let domain = Domain::new();
        let entered = Barrier::new(2);
        let leave = Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(crate::runtime::serving(|| {
                let participant = domain.participant();
                let _unit = participant.enter();
                entered.wait();
                leave.wait();
            }));
            entered.wait();
            let tag = domain.retire_tag();
            let held = domain.release_bound() <= tag;
            leave.wait();
            assert!(held);
        });
        assert!(domain.slots.lock().unwrap().is_empty());
        assert_eq!(domain.release_bound(), domain.current_epoch());
    }

    #[test]
    fn private_frame_domains_ignore_peer_units_and_survive_peer_teardown() {
        let first = Domain::new();
        let second = Domain::new();
        let entered = Barrier::new(2);
        let leave = Barrier::new(2);
        std::thread::scope(|scope| {
            scope.spawn(crate::runtime::serving(|| {
                let participant = first.participant();
                let _unit = participant.enter();
                entered.wait();
                leave.wait();
            }));
            entered.wait();
            let own_tag = first.retire_tag();
            let peer_tag = second.retire_tag();
            let correct = first.release_bound() <= own_tag && second.release_bound() > peer_tag;
            leave.wait();
            assert!(correct);
        });
        drop(first);
        let participant = second.participant();
        let unit = participant.enter();
        let tag = second.retire_tag();
        assert!(second.release_bound() <= tag);
        drop(unit);
        assert!(second.release_bound() > tag);
    }

    #[test]
    fn frame_domain_unregister_repairs_moved_indices_and_bounds_storage() {
        for count in [1, 16, 256] {
            let domain = Domain::new();
            let mut participants: std::collections::VecDeque<_> =
                (0..count).map(|_| domain.participant()).collect();
            assert_eq!(domain.slots.lock().unwrap().len(), count);
            while !participants.is_empty() {
                drop(participants.pop_front());
                assert_eq!(domain.slots.lock().unwrap().len(), participants.len());
            }
            for _ in 0..count {
                let participant = domain.participant();
                let _unit = participant.enter();
                assert_eq!(domain.slots.lock().unwrap().len(), 1);
            }
            assert!(domain.slots.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn frame_domain_unwind_releases_active_and_nested_units() {
        let domain = Domain::new();
        let result = std::panic::catch_unwind(|| {
            let participant = domain.participant();
            let _outer = participant.enter();
            let _inner = participant.enter();
            domain.retire_tag();
            panic!("participant unwind");
        });
        assert!(result.is_err());
        assert!(domain.slots.lock().unwrap().is_empty());
        assert_eq!(domain.release_bound(), domain.current_epoch());
    }

    #[test]
    fn thread_exiting_inside_a_unit_does_not_wedge_release() {
        let _guard = isolated_runtime();
        let tag = std::thread::spawn(crate::runtime::serving(|| {
            std::mem::forget(ExecutionUnit::enter());
            retire_tag()
        }))
        .join()
        .unwrap();
        assert!(release_bound() > tag);
    }
}
