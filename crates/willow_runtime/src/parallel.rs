//! Bounded parallel collection mapping on the M:N scheduler (willow-2s3.4).
//!
//! `parallel::map` accepts the runtime representation of a
//! `FrozenArray<i64>` and a non-capturing `fn(i64) -> i64`. It creates at most
//! one chunk Task per active scheduler worker, never one OS thread or Task per
//! element. Chunks write disjoint output indices, which preserves input order
//! independently of execution order. Each poll processes a bounded quantum so
//! cancellation and unrelated Tasks remain responsive.

use std::ffi::c_void;

use crate::gc::{willow_pop_roots, willow_push_root};
use crate::native_frame::{NativeFrameSpec, NativeTaskFrame};

const CHUNK_RESULT_SLOT: usize = 0;
const CHUNK_TASK_ID_SLOT: usize = 1;
const CHUNK_INPUT_SLOT: usize = 2;
const CHUNK_OUTPUT_SLOT: usize = 3;
const CHUNK_STATE_SLOT: usize = 4;
const COORD_RESULT_SLOT: usize = 0;
const COORD_TASK_ID_SLOT: usize = 1;
const COORD_INPUT_SLOT: usize = 2;
const COORD_OUTPUT_SLOT: usize = 3;
const COORD_STATE_SLOT: usize = 4;

struct ChunkFrame;

impl NativeFrameSpec for ChunkFrame {
    const LAYOUT: willow_abi::NativeFrameLayout<'static> = willow_abi::NativeFrameLayout::new(&[
        willow_abi::SlotKind::Word,
        willow_abi::SlotKind::Word,
        willow_abi::SlotKind::GcRef,
        willow_abi::SlotKind::GcRef,
        willow_abi::SlotKind::NativePtr,
    ]);
    const NAME: &'static str = "parallel map chunk";
}

struct CoordinatorFrame;

impl NativeFrameSpec for CoordinatorFrame {
    const LAYOUT: willow_abi::NativeFrameLayout<'static> = willow_abi::NativeFrameLayout::new(&[
        willow_abi::SlotKind::GcRef,
        willow_abi::SlotKind::Word,
        willow_abi::SlotKind::GcRef,
        willow_abi::SlotKind::GcRef,
        willow_abi::SlotKind::NativePtr,
    ]);
    const NAME: &'static str = "parallel map coordinator";
}

unsafe fn chunk_frame(frame: *mut c_void) -> NativeTaskFrame<ChunkFrame> {
    unsafe { NativeTaskFrame::from_raw(frame) }
}

unsafe fn coordinator_frame(frame: *mut c_void) -> NativeTaskFrame<CoordinatorFrame> {
    unsafe { NativeTaskFrame::from_raw(frame) }
}
/// Hard upper bound for one native poll. Scheduler budget/time checks normally
/// stop the chunk earlier; this cap remains as a fail-safe if no quantum is
/// active or a future preemption policy is accidentally disabled.
const MAX_ELEMENTS_PER_POLL: i64 = 256;

type I64Mapper = unsafe extern "C" fn(i64) -> i64;

struct ChunkState {
    mapper: I64Mapper,
    next: i64,
    end: i64,
}

struct CoordinatorState {
    children: Vec<u64>,
    waiters_registered: bool,
}

fn chunk_ranges(len: i64, workers: usize) -> Vec<(i64, i64)> {
    if len <= 0 {
        return Vec::new();
    }
    let chunk_count = usize::try_from(len)
        .unwrap_or(usize::MAX)
        .min(workers.max(1));
    let chunk_count_i64 = chunk_count as i64;
    // `i64::div_ceil` is not available on Willow's current MSRV. Quotient +
    // remainder is overflow-free for these validated positive operands, unlike
    // the former `(len + n - 1) / n` spelling.
    let chunk_size = len / chunk_count_i64 + i64::from(len % chunk_count_i64 != 0);
    (0..chunk_count)
        .map(|chunk| {
            let start = (chunk as i64) * chunk_size;
            let end = (start + chunk_size).min(len);
            (start, end)
        })
        .take_while(|(start, _)| *start < len)
        .collect()
}

unsafe extern "C" fn poll_chunk(frame: *mut c_void) -> i32 {
    let frame_view = unsafe { chunk_frame(frame) };
    let input = frame_view.load_gc(CHUNK_INPUT_SLOT);
    let output = frame_view.load_gc(CHUNK_OUTPUT_SLOT);
    let state_ptr = frame_view.load_native::<ChunkState>(CHUNK_STATE_SLOT);
    let Some(state) = (unsafe { state_ptr.as_mut() }) else {
        return crate::task::RUNTIME_POLL_READY;
    };

    let stop = state
        .next
        .saturating_add(MAX_ELEMENTS_PER_POLL)
        .min(state.end);
    while state.next < stop {
        let input_value = crate::array::willow_array_get(input, state.next);
        if crate::panic_context::willow_panic_active() != 0 {
            unsafe { drop_chunk_state(frame) };
            return crate::task::RUNTIME_POLL_PANICKED;
        }
        let mapped = unsafe { (state.mapper)(input_value) };
        // A mapper panic follows the ordinary Task policy. Stop immediately so
        // neutral ABI return values are never committed as successful output.
        if crate::panic_context::willow_panic_active() != 0 {
            unsafe { drop_chunk_state(frame) };
            return crate::task::RUNTIME_POLL_PANICKED;
        }
        crate::array::willow_array_set(output, state.next, mapped);
        if crate::panic_context::willow_panic_active() != 0 {
            unsafe { drop_chunk_state(frame) };
            return crate::task::RUNTIME_POLL_PANICKED;
        }
        state.next += 1;
        if state.next < state.end && crate::preempt::willow_preempt_check() != 0 {
            return crate::task::RUNTIME_POLL_PREEMPTED;
        }
    }
    if state.next < state.end {
        return crate::task::RUNTIME_POLL_PREEMPTED;
    }

    unsafe { drop_chunk_state(frame) };
    crate::task::RUNTIME_POLL_READY
}

unsafe fn drop_chunk_state(frame: *mut c_void) {
    unsafe {
        let raw = chunk_frame(frame).take_native::<ChunkState>(CHUNK_STATE_SLOT);
        if !raw.is_null() {
            drop(Box::from_raw(raw));
        }
    }
}

unsafe extern "C" fn cancel_chunk(frame: *mut c_void) {
    unsafe { drop_chunk_state(frame) };
}

fn spawn_chunk(input: *mut u8, output: *mut u8, mapper: I64Mapper, start: i64, end: i64) -> u64 {
    let Some(frame) = NativeTaskFrame::<ChunkFrame>::allocate() else {
        return 0;
    };
    frame.store_gc(CHUNK_INPUT_SLOT, input);
    frame.store_gc(CHUNK_OUTPUT_SLOT, output);
    frame.store_native(
        CHUNK_STATE_SLOT,
        Box::into_raw(Box::new(ChunkState {
            mapper,
            next: start,
            end,
        })),
    );
    let raw = frame.as_raw();
    crate::scheduler::spawn_global_task_initialized(
        poll_chunk,
        raw,
        Some(cancel_chunk),
        |task_id| frame.store_word(CHUNK_TASK_ID_SLOT, task_id as i64),
    )
}

unsafe extern "C" fn poll_coordinator(frame: *mut c_void) -> i32 {
    let frame_view = unsafe { coordinator_frame(frame) };
    let state_ptr = frame_view.load_native::<CoordinatorState>(COORD_STATE_SLOT);
    let Some(state) = (unsafe { state_ptr.as_mut() }) else {
        return crate::task::RUNTIME_POLL_READY;
    };
    let pending = if state.waiters_registered {
        state
            .children
            .iter()
            .any(|child| crate::scheduler::willow_sched_task_state(*child) != -1)
    } else {
        let mut pending = false;
        for child in &state.children {
            if crate::scheduler::willow_sched_await(*child) == 0 {
                pending = true;
            }
        }
        // A woken coordinator owns all live registrations from this point on;
        // subsequent polls inspect terminal state instead of re-registering on
        // every still-running child.
        state.waiters_registered = true;
        pending
    };
    if pending {
        return crate::task::RUNTIME_POLL_PENDING;
    }

    let output = frame_view.load_gc(COORD_OUTPUT_SLOT);
    unsafe {
        frame_view.store_gc(COORD_RESULT_SLOT, output);
        frame_view.store_native::<CoordinatorState>(COORD_STATE_SLOT, std::ptr::null_mut());
        drop(Box::from_raw(state_ptr));
    }
    crate::task::RUNTIME_POLL_READY
}

unsafe extern "C" fn cancel_coordinator(frame: *mut c_void) {
    unsafe {
        let raw = coordinator_frame(frame).take_native::<CoordinatorState>(COORD_STATE_SLOT);
        if !raw.is_null() {
            let state = Box::from_raw(raw);
            for child in &state.children {
                crate::scheduler::willow_sched_cancel(*child);
            }
            drop(state);
        }
    }
}

/// `parallel::map(values, mapper) -> Task<Array<i64>>`.
///
/// The input must be a `FrozenArray<i64>` at the language boundary. The mapper
/// is a plain non-capturing function pointer; captured lambdas are rejected by
/// the compiler before this ABI is reached.
#[unsafe(no_mangle)]
pub extern "C" fn willow_parallel_map_i64(input: *mut u8, mapper: i64) -> *mut c_void {
    if input.is_null() || mapper == 0 {
        crate::panic_context::raise_language_message(
            "parallel::map requires a FrozenArray<i64> and a valid mapper",
        );
        return std::ptr::null_mut();
    }
    let len = crate::array::willow_array_len(input);
    if crate::panic_context::willow_panic_active() != 0 {
        return std::ptr::null_mut();
    }
    let output = crate::array::willow_array_new(len, 0);
    if output.is_null() {
        return std::ptr::null_mut();
    }

    // `input` remains rooted by generated argument-rooting, but the newly
    // allocated output has no owner until it is stored in the coordinator.
    let mut rooted_output = output;
    willow_push_root(&mut rooted_output as *mut *mut u8);

    let Some(frame) = NativeTaskFrame::<CoordinatorFrame>::allocate() else {
        willow_pop_roots(1);
        return std::ptr::null_mut();
    };
    frame.store_gc(COORD_INPUT_SLOT, input);
    frame.store_gc(COORD_OUTPUT_SLOT, output);

    // The coordinator is not scheduler-rooted until all children are published.
    // Root it explicitly while child-frame allocations may collect.
    let mut rooted_frame = frame.as_raw().cast::<u8>();
    willow_push_root(&mut rooted_frame as *mut *mut u8);
    let mapper: I64Mapper = unsafe { std::mem::transmute(mapper as usize) };
    let worker_count = crate::scheduler::willow_sched_active_workers().max(1) as usize;
    let ranges = chunk_ranges(len, worker_count);
    let mut children = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        let child = spawn_chunk(input, output, mapper, start, end);
        if child == 0 {
            for spawned in children {
                crate::scheduler::willow_sched_cancel(spawned);
            }
            willow_pop_roots(2);
            if crate::panic_context::willow_panic_active() == 0 {
                crate::panic_context::raise_language_message(
                    "parallel::map could not allocate a worker Task",
                );
            }
            return std::ptr::null_mut();
        }
        children.push(child);
    }
    frame.store_native(
        COORD_STATE_SLOT,
        Box::into_raw(Box::new(CoordinatorState {
            children,
            waiters_registered: false,
        })),
    );
    let raw = frame.as_raw();
    crate::scheduler::spawn_global_task_initialized(
        poll_coordinator,
        raw,
        Some(cancel_coordinator),
        |task_id| frame.store_word(COORD_TASK_ID_SLOT, task_id as i64),
    );
    willow_pop_roots(2);
    raw
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parallel_01_empty_input_creates_no_chunks() {
        assert!(chunk_ranges(0, 5).is_empty());
        assert!(chunk_ranges(-1, 5).is_empty());
    }

    #[test]
    fn parallel_02_chunk_count_is_bounded_by_elements_and_workers() {
        for len in 1..128 {
            for workers in 1..32 {
                let ranges = chunk_ranges(len, workers);
                assert!(ranges.len() <= workers);
                assert!(ranges.len() <= len as usize);
            }
        }
    }

    #[test]
    fn parallel_03_chunks_cover_input_once_in_source_order() {
        for len in 1..128 {
            for workers in 1..32 {
                let ranges = chunk_ranges(len, workers);
                assert_eq!(ranges.first().map(|range| range.0), Some(0));
                assert_eq!(ranges.last().map(|range| range.1), Some(len));
                for pair in ranges.windows(2) {
                    assert_eq!(pair[0].1, pair[1].0);
                }
                assert!(ranges.iter().all(|(start, end)| start < end));
            }
        }
    }

    #[test]
    fn parallel_04_zero_workers_is_treated_as_one() {
        assert_eq!(chunk_ranges(10, 0), vec![(0, 10)]);
    }

    unsafe extern "C" fn pending_forever(_frame: *mut c_void) -> i32 {
        crate::task::RUNTIME_POLL_PENDING
    }

    #[test]
    fn parallel_05_coordinator_registers_each_child_waiter_once() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        crate::scheduler::reset_global_scheduler_for_test();

        let children = (0..4)
            .map(|_| crate::scheduler::willow_sched_spawn(pending_forever, std::ptr::null_mut()))
            .collect::<Vec<_>>();
        let frame = NativeTaskFrame::<CoordinatorFrame>::allocate().expect("coordinator frame");
        frame.store_native(
            COORD_STATE_SLOT,
            Box::into_raw(Box::new(CoordinatorState {
                children: children.clone(),
                waiters_registered: false,
            })),
        );
        let raw = frame.as_raw();
        let coordinator = crate::scheduler::spawn_global_task_initialized(
            poll_coordinator,
            raw,
            Some(cancel_coordinator),
            |task_id| frame.store_word(COORD_TASK_ID_SLOT, task_id as i64),
        );

        crate::scheduler::willow_sched_run();
        for child in &children {
            assert_eq!(crate::scheduler::task_waiter_count_for_test(*child), 1);
        }
        for _ in 0..16 {
            crate::scheduler::willow_sched_wake(coordinator);
            crate::scheduler::willow_sched_run();
            for child in &children {
                assert_eq!(crate::scheduler::task_waiter_count_for_test(*child), 1);
            }
        }

        crate::scheduler::willow_sched_cancel(coordinator);
        crate::scheduler::willow_sched_run();
        crate::scheduler::reset_global_scheduler_for_test();
        crate::gc::reset_internal_for_test();
    }
}
