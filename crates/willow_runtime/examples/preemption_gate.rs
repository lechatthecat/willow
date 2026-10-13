//! Opt-in A8 runtime microbenchmarks; run through tests/performance/preemption_gate.py.
//! The loop models emit_sync_safepoint's countdown/GC gates. This is not a
//! measurement of complete compiler-generated programs (see the accompanying audit).
#![no_main]

#[path = "preemption_gate/async_probe.rs"]
mod async_probe;

use std::ffi::c_void;
use std::hint::black_box;
use std::sync::atomic::{AtomicU8, Ordering};
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
    unsafe {
        *frame
            .cast::<u8>()
            .add(async_frame::async_frame_slot_offset(0))
            .cast::<*mut Work>() = state;
    }
    frame
}

unsafe fn state_in(frame: *mut c_void) -> *mut Work {
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
    let stop = gc::willow_gc_stop_flag().cast::<AtomicU8>();
    if wake {
        // Handshake outside the timed interval: this is an actual Pending ->
        // runnable wake, not a newly spawned peer or a parallel worker probe.
        loop {
            if unsafe { (*state).peer_parked } {
                break;
            }
            // The peer mutates the flag while this call suspends our stack.
            assert_eq!(preempt::willow_sync_safepoint(), 0);
        }
        unsafe { (*state).start = Some(Instant::now()) };
        scheduler::willow_sched_wake(unsafe { (*state).peer_id });
    }
    let mut value = 1_u64;
    for i in 0..unsafe { (*state).iterations } {
        value = black_box(value).wrapping_mul(1664525).wrapping_add(i);
        let gc_stop = unsafe { (*stop).load(Ordering::Relaxed) } != 0;
        let expired = if INSTRUMENTED {
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
            unsafe { (*state).slow_calls += 1 };
            assert_eq!(preempt::willow_sync_safepoint(), 0);
        }
        if wake && unsafe { (*state).peer_ran } {
            break;
        }
    }
    unsafe { (*state).checksum = value };
    black_box(value);
    if wake {
        assert!(
            unsafe { (*state).peer_ran },
            "helper finished before peer ran"
        );
    }
}

unsafe extern "C" fn peer(frame: *mut c_void) -> i32 {
    let state = unsafe { state_in(frame) };
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
    work::<true>(unsafe { state_in(frame) }, false);
    1
}

unsafe extern "C" fn wake_helper(frame: *mut c_void) -> i32 {
    work::<true>(unsafe { state_in(frame) }, true);
    1
}

unsafe extern "C" fn task_helper(frame: *mut c_void) -> i32 {
    let result = scheduler::willow_task_stack_enter(helper, frame);
    scheduler::willow_task_stack_leave();
    result
}

unsafe extern "C" fn task_wake(frame: *mut c_void) -> i32 {
    let result = scheduler::willow_task_stack_enter(wake_helper, frame);
    scheduler::willow_task_stack_leave();
    result
}

unsafe extern "C" fn task_control(frame: *mut c_void) -> i32 {
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
    gc::willow_gc_init();
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
                    scheduler::willow_sched_spawn_cooperative(peer, frame_for(&raw mut state))
                        as u64;
            }
            let poll = match mode.as_str() {
                "task-control" => task_control,
                "task-helper" => task_helper,
                _ => task_wake,
            };
            scheduler::willow_sched_spawn_cooperative(poll, frame_for(&raw mut state));
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
