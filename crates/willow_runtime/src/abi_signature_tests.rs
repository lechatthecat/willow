//! Pin the Rust side of the runtime ABI independently of the compiler table.
//!
//! Each assignment is checked at compilation: handles and callbacks must remain
//! native pointers while task IDs, lock tokens, counters, and generic payload
//! words retain their declared scalar widths. No runtime entry point is called.
//! This does not establish that storage layouts support 32-bit execution.

use std::ffi::c_void;

use crate::{
    array, async_frame, async_mutex, async_rwlock, channel, future, gc, lock, map, parallel,
    scheduler, string, task,
};

#[test]
fn scheduler_callbacks_and_frame_handles_are_native_pointers() {
    // Bidirectional assignments pin aliases, including their unsafe C ABI.
    fn poll_alias(callback: task::RuntimePollFn) -> unsafe extern "C" fn(*mut c_void) -> i32 {
        callback
    }
    fn cancel_alias(callback: task::RuntimeCancelFn) -> unsafe extern "C" fn(*mut c_void) {
        callback
    }
    let _: fn(unsafe extern "C" fn(*mut c_void) -> i32) -> task::RuntimePollFn = poll_alias;
    let _: fn(unsafe extern "C" fn(*mut c_void)) -> task::RuntimeCancelFn = cancel_alias;
    let _: extern "C" fn(task::RuntimePollFn, *mut c_void) -> u64 = scheduler::willow_sched_spawn;
    let _: extern "C" fn(u64, task::RuntimeCancelFn) = scheduler::willow_sched_set_cancel_fn;
    let _: extern "C" fn(u64) = scheduler::willow_sched_wake;
    let _: extern "C" fn() -> u64 = scheduler::willow_sched_current_task;
    let _: extern "C" fn() -> u64 = scheduler::willow_sched_active_workers;
    let _: extern "C" fn(*mut c_void, u64) -> i32 = scheduler::willow_frame_await;
    let _: extern "C" fn(i64, u64) -> *mut c_void = async_frame::willow_async_frame_alloc;
}

#[test]
fn parallel_mapper_is_a_nullable_native_function_pointer() {
    fn mapper_alias(callback: parallel::I64Mapper) -> unsafe extern "C" fn(i64) -> i64 {
        callback
    }
    let _: fn(unsafe extern "C" fn(i64) -> i64) -> parallel::I64Mapper = mapper_alias;
    let _: extern "C" fn(*mut u8, Option<parallel::I64Mapper>) -> *mut c_void =
        parallel::willow_parallel_map_i64;
}

#[test]
fn gc_objects_metadata_and_root_slots_are_pointers() {
    let _: extern "C" fn(i64) -> *mut u8 = gc::willow_alloc;
    let _: extern "C" fn(i64, i64) -> *mut u8 = gc::willow_alloc_object;
    let _: extern "C" fn(i64, u64) -> *mut u8 = gc::willow_alloc_typed;
    let _: extern "C" fn(i64, i64, *const u64) -> *mut u8 = gc::willow_gc_alloc_bitmap;
    let _: extern "C" fn(*mut *mut u8) = gc::willow_push_root;
    let _: extern "C" fn(*mut u8) = gc::willow_gc_add_runtime_root;
    let _: extern "C" fn(*mut u8) = gc::willow_gc_remove_runtime_root;
    let _: extern "C" fn(*mut u8, *mut u8, i64) = gc::willow_gc_write_barrier;
    let _: extern "C" fn() -> i64 = gc::willow_gc_allocated_bytes;
}

#[test]
fn collection_handles_are_pointers_and_payload_words_remain_i64() {
    let _: extern "C" fn(*const u8, i64) -> *mut u8 = string::willow_string_alloc;
    let _: extern "C" fn(*const u8, *const u8) -> *mut u8 = string::willow_string_concat;
    let _: extern "C" fn(*const u8, *const u8) -> i64 = string::willow_string_eq;
    let _: extern "C" fn(i64, i64) -> *mut u8 = array::willow_array_new;
    let _: extern "C" fn(*mut u8) -> i64 = array::willow_array_len;
    let _: extern "C" fn(*mut u8, i64) -> i64 = array::willow_array_get;
    let _: extern "C" fn(*mut u8, i64, i64) = array::willow_array_set;
    let _: extern "C" fn(*mut u8, i64) -> *mut u8 = array::willow_array_element_addr;
    let _: extern "C" fn(i64, i64, i64) -> *mut u8 = map::willow_map_new;
    let _: extern "C" fn(*mut u8, i64, i64, i64, i64) = map::willow_map_insert;
    let _: extern "C" fn(*mut u8, i64, i64, i64) -> *mut u8 = map::willow_map_get;
}

#[test]
fn future_pointer_and_scalar_payloads_have_distinct_signatures() {
    let _: extern "C" fn(*mut c_void) -> *mut c_void = future::willow_future_ready_ptr;
    let _: extern "C" fn(*mut c_void) -> *mut c_void = future::willow_future_await_ptr;
    let _: extern "C" fn() -> *mut c_void = future::willow_future_pending_ptr;
    let _: extern "C" fn(*mut c_void) -> u8 = future::willow_future_is_ready_ptr;
    let _: extern "C" fn(i64) -> *mut c_void = future::willow_future_ready_i64;
    let _: extern "C" fn(*mut c_void) -> i64 = future::willow_future_await_i64;
    let _: extern "C" fn(u8) -> *mut c_void = future::willow_future_ready_bool;
    let _: extern "C" fn(*mut c_void) -> u8 = future::willow_future_await_bool;
    let _: extern "C" fn(f64) -> *mut c_void = future::willow_future_ready_f64;
    let _: extern "C" fn(*mut c_void) -> f64 = future::willow_future_await_f64;
}

#[test]
fn channel_pointer_and_scalar_payloads_have_distinct_signatures() {
    let _: extern "C" fn(i64, i64) -> *mut c_void = channel::willow_channel_new_bounded;
    let _: extern "C" fn(*mut c_void, *mut c_void) -> i32 = channel::willow_channel_try_send_ptr;
    let _: extern "C" fn(*mut c_void, *mut c_void) = channel::willow_channel_send_ptr;
    let _: extern "C" fn(*mut c_void) -> *mut c_void = channel::willow_channel_recv_ptr;
    let _: extern "C" fn(*mut c_void, i64) = channel::willow_channel_send_i64;
    let _: extern "C" fn(*mut c_void) -> i64 = channel::willow_channel_recv_i64;
    let _: extern "C" fn(*mut c_void, u8) = channel::willow_channel_send_bool;
    let _: extern "C" fn(*mut c_void) -> u8 = channel::willow_channel_recv_bool;
    let _: extern "C" fn(*mut c_void, f64) = channel::willow_channel_send_f64;
    let _: extern "C" fn(*mut c_void) -> f64 = channel::willow_channel_recv_f64;
}

#[test]
fn lock_handles_are_pointers_and_tokens_and_payloads_remain_i64() {
    let _: extern "C" fn(i64, i64) -> *mut c_void = lock::willow_blocking_cell_new;
    let _: extern "C" fn(*mut c_void) -> i64 = lock::willow_blocking_cell_get;
    let _: extern "C" fn(*mut c_void, i64) = lock::willow_blocking_cell_set;
    let _: extern "C" fn(i64, i64) -> *mut c_void = async_mutex::willow_async_mutex_new;
    let _: extern "C" fn(*mut c_void, *mut i64) -> i32 = async_mutex::willow_async_mutex_acquire;
    let _: extern "C" fn(*mut c_void, i64) -> i32 = async_mutex::willow_async_mutex_poll;
    let _: extern "C" fn(*mut c_void, i64) -> i64 = async_mutex::willow_async_mutex_load;
    let _: extern "C" fn(*mut c_void, i64, i64) -> i32 = async_mutex::willow_async_mutex_commit;
    let _: extern "C" fn(i64, i64) -> *mut c_void = async_rwlock::willow_async_rwlock_new;
    let _: extern "C" fn(*mut c_void, i32, *mut i64) -> i32 =
        async_rwlock::willow_async_rwlock_acquire;
    let _: extern "C" fn(*mut c_void, i64) -> i32 = async_rwlock::willow_async_rwlock_poll;
    let _: extern "C" fn(*mut c_void, i64) -> i64 = async_rwlock::willow_async_rwlock_load;
    let _: extern "C" fn(*mut c_void, i64, i64) -> i32 = async_rwlock::willow_async_rwlock_commit;
}
