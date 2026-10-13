use std::ffi::c_void;
use std::time::Instant;
use willow_runtime::{async_frame, gc, preempt, scheduler};

// This exact workload is also compiled against the pinned historical runtime.
// Two runnable tasks exercise poll, quantum setup, yield, requeue and completion.
unsafe extern "C" fn poll(frame: *mut c_void) -> i32 {
    let remaining = unsafe {
        &mut *frame
            .cast::<u8>()
            .add(async_frame::async_frame_slot_offset(0))
            .cast::<u64>()
    };
    if *remaining == 0 {
        return 1;
    }
    *remaining -= 1;
    if preempt::willow_preempt_check() != 0 {
        3
    } else {
        2
    }
}

pub fn run(iterations: u64) -> u128 {
    let start = Instant::now();
    let mut left = async_frame::willow_async_frame_alloc(1, 0).cast::<u8>();
    gc::willow_push_root(&raw mut left);
    let mut right = async_frame::willow_async_frame_alloc(1, 0).cast::<u8>();
    gc::willow_push_root(&raw mut right);
    let offset = async_frame::async_frame_slot_offset(0);
    unsafe {
        *left.add(offset).cast::<u64>() = iterations;
        *right.add(offset).cast::<u64>() = iterations;
    }
    scheduler::willow_sched_spawn_cooperative(poll, left.cast());
    scheduler::willow_sched_spawn_cooperative(poll, right.cast());
    scheduler::willow_sched_run();
    unsafe {
        assert_eq!(
            (
                *left.add(offset).cast::<u64>(),
                *right.add(offset).cast::<u64>()
            ),
            (0, 0)
        );
    }
    gc::willow_pop_root();
    gc::willow_pop_root();
    start.elapsed().as_nanos()
}
