//! Bounded parallel collection mapping on the M:N scheduler (willow-2s3.4).
//!
//! `parallel::map` accepts the runtime representation of a
//! `FrozenArray<i64>` and a non-capturing `fn(i64) -> i64`. It creates at most
//! one chunk Task per active scheduler worker, never one OS thread or Task per
//! element. Chunks write disjoint output indices, which preserves input order
//! independently of execution order. Each poll processes a bounded quantum so
//! cancellation and unrelated Tasks remain responsive.

use std::ffi::c_void;

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
// A native scalar loop has one safepoint per bounded batch, like the amortized
// checks in generated loops. The mapper's own safepoints still consume the
// shared quantum. Do not make one cheap scalar operation a scheduler quantum:
// requeue/claim contention would otherwise dominate eight-worker mapping.
const ELEMENTS_PER_SAFEPOINT: i64 = 64;
/// Hard upper bound for one native poll, even without an active quantum.
// Keep the native batch bound independent of production time-based scheduling.
const MAX_ELEMENTS_PER_POLL: i64 = 65_536;

pub type I64Mapper = unsafe extern "C" fn(i64) -> i64;

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

#[willow_runtime_macros::ffi_boundary]
unsafe extern "C" fn poll_chunk(frame: *mut c_void) -> i32 {
    let frame_view = unsafe { chunk_frame(frame) };
    let state_ptr = frame_view.load_native::<ChunkState>(CHUNK_STATE_SLOT);
    let Some(state) = (unsafe { state_ptr.as_mut() }) else {
        return crate::task::RUNTIME_POLL_READY;
    };

    let stop = state
        .next
        .saturating_add(MAX_ELEMENTS_PER_POLL)
        .min(state.end);
    let bridge = crate::generated_context::ScalarBridge::enter();
    let mapper = state.mapper;
    let end = state.end;
    let mut next = state.next;
    let mut until_check = ELEMENTS_PER_SAFEPOINT;
    while next < stop {
        // Chunk construction proves bounds and scalar element type. Reload
        // the rooted handles: a mapper may allocate and collect between reads.
        let input_value = unsafe {
            crate::array::scalar_slot_unchecked(frame_view.load_gc(CHUNK_INPUT_SLOT), next).read()
        };
        // This native frame can repurpose the pinned register even when its
        // scheduler entry installed one. Re-establish it at the mapper boundary.
        let mapped = unsafe { bridge.call(mapper, input_value) };
        // A mapper panic follows the ordinary Task policy. Stop immediately so
        // neutral ABI return values are never committed as successful output.
        if bridge.panicked() {
            unsafe { drop_chunk_state(frame) };
            return crate::task::RUNTIME_POLL_PANICKED;
        }
        unsafe {
            crate::array::scalar_slot_unchecked(frame_view.load_gc(CHUNK_OUTPUT_SLOT), next)
                .write(mapped);
        }
        next += 1;
        until_check -= 1;
        if next < end && until_check == 0 {
            // Also settles pending safepoint debt from generated helpers.
            // Flags and time are sampled every batch, not every quantum.
            if crate::preempt::willow_preempt_check() != 0 {
                state.next = next;
                return crate::task::RUNTIME_POLL_PREEMPTED;
            }
            until_check = ELEMENTS_PER_SAFEPOINT;
        }
    }
    state.next = next;
    if next < end {
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

#[willow_runtime_macros::ffi_boundary]
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

#[willow_runtime_macros::ffi_boundary]
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

#[willow_runtime_macros::ffi_boundary]
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
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_parallel_map_i64(
    input: *mut u8,
    mapper: Option<I64Mapper>,
) -> *mut c_void {
    let Some(mapper) = mapper else {
        crate::panic_context::raise_language_message(
            "parallel::map requires a FrozenArray<i64> and a valid mapper",
        );
        return std::ptr::null_mut();
    };
    if input.is_null() {
        crate::panic_context::raise_language_message(
            "parallel::map requires a FrozenArray<i64> and a valid mapper",
        );
        return std::ptr::null_mut();
    }
    let len = crate::array::willow_array_len(input);
    if crate::panic_context::willow_panic_active() != 0 {
        return std::ptr::null_mut();
    }
    let input = crate::gc::GcRootHandle::new(input);
    let output = crate::array::new_old_array(len, 0);
    if output.is_null() {
        return std::ptr::null_mut();
    }

    // `input` remains rooted by generated argument-rooting, but the newly
    // allocated output has no owner until it is stored in the coordinator.
    let rooted_output = crate::gc::GcRootHandle::new(output);

    let Some(frame) = NativeTaskFrame::<CoordinatorFrame>::allocate() else {
        return std::ptr::null_mut();
    };
    frame.store_gc(COORD_INPUT_SLOT, input.load());
    frame.store_gc(COORD_OUTPUT_SLOT, rooted_output.load());

    // The coordinator is not scheduler-rooted until all children are published.
    // Root it explicitly while child-frame allocations may collect.
    let _rooted_frame = crate::gc::GcRootHandle::new(frame.as_raw().cast::<u8>());
    let worker_count = crate::scheduler::willow_sched_active_workers().max(1) as usize;
    let ranges = chunk_ranges(len, worker_count);
    let mut children = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        let child = spawn_chunk(input.load(), rooted_output.load(), mapper, start, end);
        if child == 0 {
            for spawned in children {
                crate::scheduler::willow_sched_cancel(spawned);
            }
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

    thread_local! {
        static MAPPED_ELEMENTS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
    }

    unsafe extern "C" fn increment(value: i64) -> i64 {
        MAPPED_ELEMENTS.set(MAPPED_ELEMENTS.get() + 1);
        value.wrapping_add(1)
    }

    #[test]
    fn parallel_06_scalar_loop_exact_work_and_poll_bounds() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        // Growing, non-aligned lengths include both countdown and poll edges.
        for len in [
            0, 1, 2, 63, 64, 65, 255, 256, 257, 1023, 1024, 1025, 4097, 65535, 65536, 65537, 131073,
        ] {
            for budget in [1, 2, 64, 4096] {
                MAPPED_ELEMENTS.set(0);
                let input = crate::array::new_old_array(len, 0);
                let _input_root = crate::gc::GcRootHandle::new(input);
                let output = crate::array::new_old_array(len, 0);
                let _output_root = crate::gc::GcRootHandle::new(output);
                for index in 0..len {
                    crate::array::willow_array_set(input, index, index);
                }
                let frame = NativeTaskFrame::<ChunkFrame>::allocate().unwrap();
                frame.store_gc(CHUNK_INPUT_SLOT, input);
                frame.store_gc(CHUNK_OUTPUT_SLOT, output);
                frame.store_native(
                    CHUNK_STATE_SLOT,
                    Box::into_raw(Box::new(ChunkState {
                        mapper: increment,
                        next: 0,
                        end: len,
                    })),
                );
                let mut processed = 0;
                let mut polls = 0;
                loop {
                    crate::preempt::begin_quantum(
                        crate::preempt::PreemptConfig::from_env_values(
                            Some(&budget.to_string()),
                            Some("100000"),
                        ),
                        std::ptr::null(),
                    );
                    let result = unsafe { poll_chunk(frame.as_raw()) };
                    crate::preempt::willow_preempt_end();
                    polls += 1;
                    if result == crate::task::RUNTIME_POLL_READY {
                        assert!(frame.load_native::<ChunkState>(CHUNK_STATE_SLOT).is_null());
                        break;
                    }
                    assert_eq!(result, crate::task::RUNTIME_POLL_PREEMPTED);
                    let next = unsafe { (*frame.load_native::<ChunkState>(CHUNK_STATE_SLOT)).next };
                    assert_eq!(
                        next - processed,
                        (budget * ELEMENTS_PER_SAFEPOINT).min(MAX_ELEMENTS_PER_POLL)
                    );
                    processed = next;
                }
                let quantum = (budget * ELEMENTS_PER_SAFEPOINT).min(MAX_ELEMENTS_PER_POLL);
                assert_eq!(polls, ((len + quantum - 1) / quantum).max(1));
                assert_eq!(MAPPED_ELEMENTS.get(), len);
                for index in 0..len {
                    assert_eq!(crate::array::willow_array_get(output, index), index + 1);
                }
                println!("len={len} budget={budget} mapped={len} polls={polls}");
            }
        }
        crate::gc::reset_internal_for_test();
    }

    #[test]
    fn parallel_07_pending_preemption_stops_within_one_batch() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::reset_internal_for_test();
        MAPPED_ELEMENTS.set(0);
        let input = crate::array::new_old_array(1024, 0);
        let _input = crate::gc::GcRootHandle::new(input);
        let output = crate::array::new_old_array(1024, 0);
        let _output = crate::gc::GcRootHandle::new(output);
        let frame = NativeTaskFrame::<ChunkFrame>::allocate().unwrap();
        frame.store_gc(CHUNK_INPUT_SLOT, input);
        frame.store_gc(CHUNK_OUTPUT_SLOT, output);
        frame.store_native(
            CHUNK_STATE_SLOT,
            Box::into_raw(Box::new(ChunkState {
                mapper: increment,
                next: 0,
                end: 1024,
            })),
        );
        let flag = std::sync::atomic::AtomicBool::new(true);
        crate::preempt::begin_quantum(
            crate::preempt::PreemptConfig::from_env_values(Some("4096"), Some("100000")),
            (&raw const flag).cast(),
        );
        // Beginning a quantum clears stale requests; simulate a request that
        // arrives after the scheduler has installed this quantum.
        flag.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            unsafe { poll_chunk(frame.as_raw()) },
            crate::task::RUNTIME_POLL_PREEMPTED
        );
        crate::preempt::willow_preempt_end();
        assert_eq!(MAPPED_ELEMENTS.get(), ELEMENTS_PER_SAFEPOINT);
        unsafe { cancel_chunk(frame.as_raw()) };
        assert!(frame.load_native::<ChunkState>(CHUNK_STATE_SLOT).is_null());
        // Cleanup is idempotent and cannot map another element or publish success.
        unsafe { cancel_chunk(frame.as_raw()) };
        assert_eq!(MAPPED_ELEMENTS.get(), ELEMENTS_PER_SAFEPOINT);
        drop(_input);
        drop(_output);
        crate::gc::reset_internal_for_test();
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
