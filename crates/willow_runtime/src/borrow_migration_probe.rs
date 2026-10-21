//! Opt-in acceptance harness for generated async scalar borrows (B7).
//!
//! The fixture's first argument is MARKER, followed by ONE scalar reference.
//! Thus slot 2 is the marker and slots 3/4 are the compiler's {base, offset}
//! cell. Inspect actual generated frames, not a Rust reconstruction of them.
//! No exports, environment switches or overhead exist in ordinary builds.
use crate::async_frame::async_frame_slot_offset;
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::thread::ThreadId;

const MARKER: i64 = 731_207_731;
const BASE_SLOT: usize = 3;

#[derive(Clone, Copy)]
struct Probe {
    worker: usize,
    thread: ThreadId,
    address: usize,
    polls: usize,
    parked: bool,
}

struct Allocation {
    bytes: usize,
    mask: u64,
    probe: Option<Probe>,
}

static FRAMES: LazyLock<Mutex<HashMap<usize, Allocation>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

pub(crate) fn allocated(frame: usize, bytes: usize, mask: u64) {
    FRAMES.lock().unwrap().insert(
        frame,
        Allocation {
            bytes,
            mask,
            probe: None,
        },
    );
}

pub(crate) fn removed(frame: usize) {
    FRAMES.lock().unwrap().remove(&frame);
}

fn is_fixture(frames: &HashMap<usize, Allocation>, frame: usize) -> bool {
    frames.get(&frame).is_some_and(|allocation| {
        allocation.bytes >= async_frame_slot_offset(BASE_SLOT + 2)
            // SAFETY: called only for a scheduler-owned frame whose generated
            // code is not executing;
            // the allocation index proves the marker word is in bounds.
            && unsafe { *((frame + async_frame_slot_offset(2)) as *const i64) } == MARKER
    })
}

pub(crate) fn route(frame: usize, worker: usize, workers: usize) -> Option<usize> {
    let frames = FRAMES.lock().unwrap();
    if !is_fixture(&frames, frame) {
        return None;
    }
    assert!(
        workers >= 2,
        "migration probe requires at least two workers"
    );
    frames[&frame].probe.and_then(|previous| {
        let next = (previous.worker + 1) % workers;
        (worker != next).then_some(next)
    })
}

fn owned_slot(frames: &HashMap<usize, Allocation>, base: usize, offset: usize) -> Option<usize> {
    let owner = frames.get(&base)?;
    // Positive allocation membership, not a distance-from-stack heuristic.
    // Both the cell and its resolved slot must belong to live async frames.
    (offset >= async_frame_slot_offset(0)
        && offset.is_multiple_of(8)
        && offset.checked_add(8)? <= owner.bytes)
        .then(|| base.checked_add(offset))
        .flatten()
}

pub(crate) fn claimed(frame: usize, worker: usize, task: u64, native: bool, pinned: bool) {
    let mut frames = FRAMES.lock().unwrap();
    if !is_fixture(&frames, frame) {
        return;
    }
    let allocation = &frames[&frame];
    assert_ne!(
        allocation.mask & (1 << BASE_SLOT),
        0,
        "untraced borrow owner"
    );
    assert_eq!(allocation.mask & (1 << (BASE_SLOT + 1)), 0);
    // SAFETY: is_fixture established bounds for both words. The scheduler
    // owns this poll exclusively; no generated code is mutating the cell.
    let (base, offset) = unsafe {
        let cell = (frame + async_frame_slot_offset(BASE_SLOT)) as *const usize;
        (*cell, *cell.add(1))
    };
    let address = owned_slot(&frames, base, offset).unwrap_or_else(|| {
        panic!("borrowed slot is not inside a live heap async frame (worker stack or invalid cell): offset={offset}, owner_bytes={:?}", frames.get(&base).map(|a| a.bytes))
    });
    let thread = std::thread::current().id();
    let previous = allocation.probe;
    if pinned {
        let previous = previous.expect("pinned continuation must follow an initial claim");
        assert_eq!(
            previous.worker, worker,
            "implicit preemption migrated workers"
        );
        assert_eq!(
            previous.thread, thread,
            "implicit preemption migrated OS threads"
        );
        assert_eq!(previous.address, address, "borrow changed caller slot");
        if native {
            eprintln!("native-stack-preemption task={task} worker={worker} checked=1");
        } else {
            eprintln!("cooperative-preemption task={task} worker={worker} checked=1");
        }
        return;
    }
    if let Some(previous) = previous {
        assert_ne!(previous.worker, worker, "task did not migrate");
        assert_ne!(
            previous.thread, thread,
            "worker IDs hid reuse of one OS thread"
        );
        assert_eq!(previous.address, address, "borrow changed caller slot");
    }
    if native {
        assert!(
            previous.is_some(),
            "native migration must follow an initial claim"
        );
        eprintln!("native-stack-migration task={task} worker={worker} checked=1");
    }
    let polls = previous.map_or(1, |p| p.polls + 1);
    eprintln!(
        "borrow-migration task={task} poll={polls} parked={} checked=1",
        usize::from(previous.is_some_and(|p| p.parked)),
    );
    frames.get_mut(&frame).unwrap().probe = Some(Probe {
        worker,
        thread,
        address,
        polls,
        parked: false,
    });
}

pub(crate) fn parked(frame: usize) {
    if let Some(probe) = FRAMES
        .lock()
        .unwrap()
        .get_mut(&frame)
        .and_then(|allocation| allocation.probe.as_mut())
    {
        probe.parked = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn borrowed_slot_probe_rejects_stack_and_invalid_offsets() {
        let stack_word = 0_u64;
        let stack = &stack_word as *const u64 as usize;
        let frames = HashMap::from([(
            0x1000,
            Allocation {
                bytes: 80,
                mask: 0,
                probe: None,
            },
        )]);
        assert_eq!(owned_slot(&frames, 0x1000, 40), Some(0x1028));
        assert_eq!(owned_slot(&frames, 0, stack), None);
        assert_eq!(owned_slot(&frames, stack, 40), None);
        for offset in [0, 16, 39, 41, 73, 80, usize::MAX] {
            assert_eq!(owned_slot(&frames, 0x1000, offset), None);
        }
    }
}
