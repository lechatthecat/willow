//! Grace periods for retiring terminal task-frame runtime roots (willow-vjaf).
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Slot value of a thread outside every execution unit.
const IDLE: u64 = u64::MAX;

/// Monotonic retire epoch. Units read it on entry; retirements advance it.
static RETIRE_EPOCH: AtomicU64 = AtomicU64::new(0);

/// One slot per thread that has ever entered a unit. A slot whose thread has
/// exited is only referenced from here and is pruned during the next scan.
static SLOTS: Mutex<Vec<Arc<EpochSlot>>> = Mutex::new(Vec::new());

/// Padded so one worker's per-poll store does not share a cache line with
/// another worker's slot.
#[repr(align(128))]
struct EpochSlot(AtomicU64);

struct ThreadUnit {
    depth: Cell<usize>,
    slot: Arc<EpochSlot>,
}

impl ThreadUnit {
    fn register() -> Self {
        let slot = Arc::new(EpochSlot(AtomicU64::new(IDLE)));
        SLOTS
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(Arc::clone(&slot));
        Self {
            depth: Cell::new(0),
            slot,
        }
    }
}

impl Drop for ThreadUnit {
    fn drop(&mut self) {
        self.slot.0.store(IDLE, Ordering::Release);
    }
}

thread_local! {
    static THREAD_UNIT: ThreadUnit = ThreadUnit::register();
}

/// RAII bracket for one execution unit. See the module documentation.
#[must_use = "dropping the unit ends it immediately"]
pub(crate) struct ExecutionUnit {
    entered: bool,
}

impl ExecutionUnit {
    pub(crate) fn enter() -> Self {
        // `try_with` fails only while this thread's TLS is being destroyed;
        // such a thread runs no further polls or spawns.
        let entered = THREAD_UNIT
            .try_with(|unit| {
                let depth = unit.depth.get();
                if depth == 0 {
                    // Published before the unit can observe any frame pointer.
                    // Every frame this unit can reach unrooted is retired after
                    // this store, by a retirement ordered after it.
                    unit.slot
                        .0
                        .store(RETIRE_EPOCH.load(Ordering::Acquire), Ordering::Release);
                }
                unit.depth.set(depth + 1);
            })
            .is_ok();
        Self { entered }
    }
}

impl Drop for ExecutionUnit {
    fn drop(&mut self) {
        if !self.entered {
            return;
        }
        let _ = THREAD_UNIT.try_with(|unit| {
            let depth = unit.depth.get() - 1;
            unit.depth.set(depth);
            if depth == 0 {
                // Release: the unit's last use of any frame happens-before a
                // scan that observes the slot idle.
                unit.slot.0.store(IDLE, Ordering::Release);
            }
        });
    }
}

/// Tag for a frame retired now. Callers retire under the scheduler lock, so
/// tags are monotonic in retirement order.
pub(crate) fn retire_tag() -> u64 {
    RETIRE_EPOCH.fetch_add(1, Ordering::AcqRel)
}

/// Frames retired with a tag below the returned bound have no unit left that
/// started before their retirement.
///
/// The epoch is read before the slots: a frame tagged below that snapshot was
/// retired before the read, so any unit holding it published its slot before
/// this scan looks at it. Without the snapshot, a scan that saw every slot
/// idle would also release frames retired after it, under units that began
/// after it. O(threads that ever entered a unit); dead threads are pruned.
pub(crate) fn release_bound() -> u64 {
    let epoch = RETIRE_EPOCH.load(Ordering::Acquire);
    let mut slots = SLOTS.lock().unwrap_or_else(|poison| poison.into_inner());
    slots.retain(|slot| Arc::strong_count(slot) > 1);
    slots
        .iter()
        .map(|slot| slot.0.load(Ordering::Acquire))
        .fold(epoch, u64::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::runtime_test_guard;
    use std::sync::Barrier;

    // Perspectives (willow-vjaf): an idle process bounds nothing it has not
    // tagged yet; a unit holds back exactly the frames retired after it began;
    // nesting keeps the outer epoch; another thread's unit counts; a thread
    // that dies inside a unit cannot wedge reclamation.

    #[test]
    fn idle_bound_covers_every_frame_already_retired() {
        let _guard = runtime_test_guard();
        let tag = retire_tag();
        assert!(release_bound() > tag);
    }

    #[test]
    fn retire_tags_are_strictly_increasing() {
        let _guard = runtime_test_guard();
        let first = retire_tag();
        let second = retire_tag();
        assert!(second > first);
    }

    #[test]
    fn active_unit_holds_back_frames_retired_during_it() {
        let _guard = runtime_test_guard();
        let before = retire_tag();
        let unit = ExecutionUnit::enter();
        let during = retire_tag();
        let bound = release_bound();
        assert!(bound > before, "a frame retired before the unit is free");
        assert!(bound <= during, "a frame retired during the unit is held");
        drop(unit);
        assert!(release_bound() > during, "leaving the unit releases it");
    }

    #[test]
    fn nested_unit_keeps_the_outer_epoch() {
        let _guard = runtime_test_guard();
        let outer = ExecutionUnit::enter();
        let first = retire_tag();
        let inner = ExecutionUnit::enter();
        let second = retire_tag();
        drop(inner);
        assert!(
            release_bound() <= first,
            "ending a nested spawn or drive must not end the enclosing poll"
        );
        drop(outer);
        assert!(release_bound() > second);
    }

    #[test]
    fn unit_on_another_thread_holds_back_release() {
        let _guard = runtime_test_guard();
        let entered = Arc::new(Barrier::new(2));
        let leave = Arc::new(Barrier::new(2));
        let worker = {
            let (entered, leave) = (Arc::clone(&entered), Arc::clone(&leave));
            std::thread::spawn(move || {
                let unit = ExecutionUnit::enter();
                entered.wait();
                leave.wait();
                drop(unit);
            })
        };
        entered.wait();
        let tag = retire_tag();
        assert!(release_bound() <= tag);
        leave.wait();
        worker.join().unwrap();
        assert!(release_bound() > tag);
    }

    #[test]
    fn thread_exiting_inside_a_unit_does_not_wedge_release() {
        let _guard = runtime_test_guard();
        let tag = std::thread::spawn(|| {
            std::mem::forget(ExecutionUnit::enter());
            retire_tag()
        })
        .join()
        .unwrap();
        assert!(release_bound() > tag);
    }
}
