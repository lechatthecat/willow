//! Opt-in A8 runtime microbenchmarks; run through tests/performance/preemption_gate.py.
//! The loop models emit_sync_safepoint's countdown/GC gates. This is not a
//! measurement of complete compiler-generated programs (see the accompanying audit).
#![no_main]

#[path = "preemption_gate/async_probe.rs"]
mod async_probe;

use std::ffi::c_void;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use willow_runtime::{async_frame, gc, preempt, scheduler};

struct Work {
    iterations: u64,
    checksum: u64,
    start: Option<Instant>,
    latency: u128,
    peer_ran: bool,
    peer_parked: bool,
    peer_id: u64,
    slow_calls: u64,
}

fn frame_for(state: *mut Work) -> *mut c_void {
    let frame = async_frame::willow_async_frame_alloc(1, 0);
    // SAFETY: The GC word-aligns these allocated task frames and the ABI locates the reserved slot
    // at a word offset.
    unsafe {
        *frame
            .cast::<u8>()
            .add(async_frame::async_frame_slot_offset(0))
            .cast::<*mut Work>() = state;
    }
    frame
}

unsafe fn state_in(frame: *mut c_void) -> *mut Work {
    // SAFETY: The GC word-aligns these allocated task frames and the ABI locates the reserved slot
    // at a word offset.
    unsafe {
        *frame
            .cast::<u8>()
            .add(async_frame::async_frame_slot_offset(0))
            .cast::<*mut Work>()
    }
}

// The baseline retains the GC gate and identical arithmetic. Volatile counter
// accesses prevent LLVM eliminating/hoisting the countdown which Cranelift emits.
#[inline(never)]
fn work<const INSTRUMENTED: bool>(state: *mut Work, wake: bool) {
    let mut spare = i32::MAX;
    let active = if INSTRUMENTED {
        preempt::willow_sync_poll_counter()
    } else {
        std::ptr::null_mut()
    };
    let counter = if active.is_null() {
        &raw mut spare
    } else {
        active
    };
    // SAFETY: the byte-pointer ABI exposes the current GcRuntime's AtomicBool.
    // Its owner outlives this probe and all accesses to the gate are atomic.
    let stop = unsafe { &*gc::willow_gc_stop_flag().cast::<AtomicBool>() };
    if wake {
        // Handshake outside the timed interval: this is an actual Pending ->
        // runnable wake, not a newly spawned peer or a parallel worker probe.
        loop {
            // SAFETY: The single-worker probe retains this typed state through both tasks.
            // Accesses finish before a task yields.
            if unsafe { (*state).peer_parked } {
                break;
            }
            // The peer mutates the flag while this call suspends our stack.
            assert_eq!(preempt::willow_sync_safepoint(), 0);
        }
        // SAFETY: The single-worker probe retains this typed state through both tasks. Accesses
        // finish before a task yields.
        unsafe { (*state).start = Some(Instant::now()) };
        // SAFETY: The single-worker probe retains this typed state through both tasks. Accesses
        // finish before a task yields.
        scheduler::willow_sched_wake(unsafe { (*state).peer_id });
    }
    let mut value = 1_u64;
    // SAFETY: the driver retains Work until the single-worker probe finishes.
    for i in 0..unsafe { (*state).iterations } {
        value = black_box(value).wrapping_mul(1664525).wrapping_add(i);
        let gc_stop = stop.load(Ordering::Acquire);
        let expired = if INSTRUMENTED {
            // SAFETY: counter is this activation's spare or the active stack's live countdown.
            unsafe {
                let left = counter.read_volatile() - 1;
                counter.write_volatile(left);
                left <= 0
            }
        } else {
            false
        };
        if gc_stop || expired {
            spare = i32::MAX;
            black_box(&spare);
            // SAFETY: The single-worker probe retains this typed state through both tasks.
            // Accesses finish before a task yields.
            unsafe { (*state).slow_calls += 1 };
            assert_eq!(preempt::willow_sync_safepoint(), 0);
        }
        // SAFETY: The single-worker probe retains this typed state through both tasks. Accesses
        // finish before a task yields.
        if wake && unsafe { (*state).peer_ran } {
            break;
        }
    }
    // SAFETY: The single-worker probe retains this typed state through both tasks. Accesses finish
    // before a task yields.
    unsafe { (*state).checksum = value };
    black_box(value);
    if wake {
        assert!(
            // SAFETY: The single-worker probe retains this typed state through both tasks.
            // Accesses finish before a task yields.
            unsafe { (*state).peer_ran },
            "helper finished before peer ran"
        );
    }
}

unsafe extern "C" fn peer(frame: *mut c_void) -> i32 {
    // SAFETY: The GC word-aligns these allocated task frames and the ABI locates the reserved slot
    // at a word offset.
    let state = unsafe { state_in(frame) };
    // SAFETY: The single-worker probe retains this typed state through both tasks. Accesses finish
    // before a task yields.
    unsafe {
        if !(*state).peer_parked {
            (*state).peer_parked = true;
            return 0;
        }
        (*state).latency = (*state).start.unwrap().elapsed().as_nanos();
        (*state).peer_ran = true;
    }
    1
}

unsafe extern "C" fn helper(frame: *mut c_void) -> i32 {
    // SAFETY: The GC word-aligns these allocated task frames and the ABI locates the reserved slot
    // at a word offset.
    work::<true>(unsafe { state_in(frame) }, false);
    1
}

unsafe extern "C" fn wake_helper(frame: *mut c_void) -> i32 {
    // SAFETY: The GC word-aligns these allocated task frames and the ABI locates the reserved slot
    // at a word offset.
    work::<true>(unsafe { state_in(frame) }, true);
    1
}

unsafe extern "C" fn task_helper(frame: *mut c_void) -> i32 {
    // SAFETY: The supplied callback matches this frame layout; the frame storage remains live
    // across scheduling and callback execution.
    let result = unsafe { scheduler::willow_task_stack_enter(helper, frame) };
    scheduler::willow_task_stack_leave();
    result
}

unsafe extern "C" fn task_wake(frame: *mut c_void) -> i32 {
    // SAFETY: The supplied callback matches this frame layout; the frame storage remains live
    // across scheduling and callback execution.
    let result = unsafe { scheduler::willow_task_stack_enter(wake_helper, frame) };
    scheduler::willow_task_stack_leave();
    result
}

unsafe extern "C" fn task_control(frame: *mut c_void) -> i32 {
    // SAFETY: The GC word-aligns these allocated task frames and the ABI locates the reserved slot
    // at a word offset.
    work::<false>(unsafe { state_in(frame) }, false);
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_user_main() {
    assert_eq!(std::env::var("WILLOW_WORKERS").as_deref(), Ok("1"));
    let args: Vec<_> = std::env::args().collect();
    let mode = args.get(1).expect("mode");
    let iterations: u64 = args.get(2).expect("iterations").parse().unwrap();
    assert!(iterations > 0);
    // SAFETY: The standalone driver exclusively controls this heap between workloads; no
    // mutator or borrowed payload crosses reset.
    unsafe { gc::willow_gc_init() };
    if mode == "async" {
        println!(
            "{{\"elapsed_ns\":{},\"operations\":{}}}",
            async_probe::run(iterations),
            2 * (iterations + 1)
        );
        return;
    }
    let mut state = Work {
        iterations,
        checksum: 0,
        start: None,
        latency: 0,
        peer_ran: false,
        peer_parked: false,
        peer_id: 0,
        slow_calls: 0,
    };
    let start = Instant::now();
    match mode.as_str() {
        "control" => work::<false>(&raw mut state, false),
        "never" => work::<true>(&raw mut state, false),
        "task-control" | "task-helper" | "wake" => {
            if mode == "wake" {
                state.peer_id =
                    // SAFETY: The supplied callback matches this frame layout; the frame
                    // storage remains live across scheduling and callback execution.
                    (unsafe { scheduler::willow_sched_spawn_cooperative(peer, frame_for(&raw mut state)) })
                        as u64;
            }
            let poll = match mode.as_str() {
                "task-control" => task_control,
                "task-helper" => task_helper,
                _ => task_wake,
            };
            // SAFETY: The supplied callback matches this frame layout; the frame storage
            // remains live across scheduling and callback execution.
            unsafe { scheduler::willow_sched_spawn_cooperative(poll, frame_for(&raw mut state)) };
            scheduler::willow_sched_run();
        }
        _ => panic!("unknown mode"),
    }
    let elapsed = start.elapsed().as_nanos();
    if mode == "never" {
        assert_eq!(state.slow_calls, 0, "never-task entered slow path");
    }
    if mode == "task-helper" || mode == "wake" {
        assert!(state.slow_calls > 0, "helper did not exercise preemption");
    }
    println!(
        "{{\"elapsed_ns\":{elapsed},\"operations\":{iterations},\"checksum\":{},\"slow_calls\":{},\"latency_ns\":{}}}",
        state.checksum, state.slow_calls, state.latency
    );
}

#[unsafe(no_mangle)]
pub extern "C" fn __willow_static_init() {}
