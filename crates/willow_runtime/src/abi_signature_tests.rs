//! Pin the Rust side of the runtime ABI against the shared schema.
//!
//! Each assignment is checked at compilation: handles and callbacks must remain
//! native pointers while task IDs, lock tokens, counters, and generic payload
//! words retain their declared scalar widths. No runtime entry point is called.
//! This does not establish that storage layouts support 32-bit execution.
//!
//! The Rust types alone cannot tell a generic payload word from a fixed-width
//! integer: both are `i64`, and both lower to the same Cranelift type on every
//! supported target. The semantic tests below therefore pin the
//! [`AbiTy::Word`] / [`AbiTy::I64`] positions of each payload-carrying row in
//! [`willow_abi::RUNTIME_SYMBOLS`] against the order the runtime declares, so a
//! transposed row (willow-9tls.7) fails here instead of working by
//! coincidence until the two representations diverge.

use std::collections::BTreeSet;
use std::ffi::c_void;

use willow_abi::AbiTy::{self, F64, I8, I32, I64, Ptr, Word};
use willow_abi::{RUNTIME_SYMBOLS, runtime_symbol};

use crate::{
    array, async_frame, async_mutex, async_rwlock, channel, future, gc, lock, map, parallel,
    scheduler, string, task,
};

/// The semantic parameter and return kinds the shared schema declares for
/// `name`, as the compiler will lower them.
fn schema(name: &str) -> (&'static [AbiTy], Option<AbiTy>) {
    let symbol = runtime_symbol(name).unwrap_or_else(|| panic!("{name} is not in RUNTIME_SYMBOLS"));
    (symbol.params, symbol.ret)
}

/// Assert one row of the shared schema, so a failure names the symbol.
#[track_caller]
fn assert_schema(name: &str, params: &[AbiTy], ret: Option<AbiTy>) {
    assert_eq!(schema(name), (params, ret), "{name}");
}

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
    let _: extern "C" fn(u64, task::RuntimeCancelFn, i32) =
        scheduler::willow_sched_set_cancel_fn_cooperative;
    let _: extern "C" fn(u64) = scheduler::willow_sched_wake;
    let _: unsafe extern "C" fn(*const u64, usize) = scheduler::willow_sched_wake_many;
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
    let _: extern "C" fn(*mut u8, *mut u8, *mut u8, i64) = gc::willow_gc_write_barrier;
    let _: extern "C" fn() -> i64 = gc::willow_gc_allocated_bytes;
    let _: extern "C" fn(u64) -> u64 = gc::willow_gc_set_memory_limit;
    assert_schema("willow_gc_set_memory_limit", &[I64], Some(I64));
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

// --- semantic kinds: generic payload `Word` vs fixed-width `I64` ---
//
// Every payload-carrying row is stated twice: the Rust `extern "C"` type,
// which the compiler checks, and the `AbiTy` row the schema must carry, which
// the Rust type cannot express because `Word` and `I64` are both `i64`.

/// The semantic row of every ABI symbol that moves a generic payload word, in
/// the order the runtime declares it. This is the single list the per-row and
/// exhaustiveness checks both read, so a row cannot be counted as pinned
/// without its kinds being asserted.
const WORD_PINS: &[(&str, &[AbiTy], Option<AbiTy>)] = &[
    // (arr, index) -> word; (arr, index, word); (arr, word); (arr) -> word.
    ("willow_array_get", &[Ptr, I64], Some(Word)),
    ("willow_array_set", &[Ptr, I64, Word], None),
    ("willow_array_push", &[Ptr, Word], None),
    ("willow_array_pop", &[Ptr], Some(Word)),
    // (map, key_word, key_is_ref, val_word, val_is_ref): the flag for a word
    // follows that word. The old schema `[Ptr, Word, Word, I64, I64]` only
    // linked because both kinds lower to I64 (willow-9tls.7).
    ("willow_map_insert", &[Ptr, Word, I64, Word, I64], None),
    // (map, key_word, key_is_ref, use_niche) and (map, key_word, key_is_ref).
    ("willow_map_get", &[Ptr, Word, I64, I64], Some(Ptr)),
    ("willow_map_contains", &[Ptr, Word, I64], Some(I64)),
    // Cells: new(value_word, is_ref); get/read(handle) -> word;
    // set/write(handle, word).
    ("willow_blocking_cell_new", &[Word, I64], Some(Ptr)),
    ("willow_blocking_cell_get", &[Ptr], Some(Word)),
    ("willow_blocking_cell_set", &[Ptr, Word], None),
    ("willow_blocking_rw_cell_new", &[Word, I64], Some(Ptr)),
    ("willow_blocking_rw_cell_read", &[Ptr], Some(Word)),
    ("willow_blocking_rw_cell_write", &[Ptr, Word], None),
    // Scheduler locks: new(value_word, is_ref); load(handle, token) -> word;
    // commit(handle, token, word).
    ("willow_async_mutex_new", &[Word, I64], Some(Ptr)),
    ("willow_async_mutex_load", &[Ptr, I64], Some(Word)),
    ("willow_async_mutex_commit", &[Ptr, I64, Word], Some(I32)),
    ("willow_async_rwlock_new", &[Word, I64], Some(Ptr)),
    ("willow_async_rwlock_load", &[Ptr, I64], Some(Word)),
    ("willow_async_rwlock_commit", &[Ptr, I64, Word], Some(I32)),
];

#[test]
fn word_carrying_rows_keep_i64_payload_types_in_rust() {
    let _: extern "C" fn(*mut u8, i64) -> i64 = array::willow_array_get;
    let _: extern "C" fn(*mut u8, i64, i64) = array::willow_array_set;
    let _: extern "C" fn(*mut u8, i64) = array::willow_array_push;
    let _: extern "C" fn(*mut u8) -> i64 = array::willow_array_pop;
    let _: extern "C" fn(*mut u8, i64, i64, i64, i64) = map::willow_map_insert;
    let _: extern "C" fn(*mut u8, i64, i64, i64) -> *mut u8 = map::willow_map_get;
    let _: extern "C" fn(*mut u8, i64, i64) -> i64 = map::willow_map_contains;
    let _: extern "C" fn(i64, i64) -> *mut c_void = lock::willow_blocking_cell_new;
    let _: extern "C" fn(*mut c_void) -> i64 = lock::willow_blocking_cell_get;
    let _: extern "C" fn(*mut c_void, i64) = lock::willow_blocking_cell_set;
    let _: extern "C" fn(i64, i64) -> *mut c_void = lock::willow_blocking_rw_cell_new;
    let _: extern "C" fn(*mut c_void) -> i64 = lock::willow_blocking_rw_cell_read;
    let _: extern "C" fn(*mut c_void, i64) = lock::willow_blocking_rw_cell_write;
    let _: extern "C" fn(i64, i64) -> *mut c_void = async_mutex::willow_async_mutex_new;
    let _: extern "C" fn(*mut c_void, i64) -> i64 = async_mutex::willow_async_mutex_load;
    let _: extern "C" fn(*mut c_void, i64, i64) -> i32 = async_mutex::willow_async_mutex_commit;
    let _: extern "C" fn(i64, i64) -> *mut c_void = async_rwlock::willow_async_rwlock_new;
    let _: extern "C" fn(*mut c_void, i64) -> i64 = async_rwlock::willow_async_rwlock_load;
    let _: extern "C" fn(*mut c_void, i64, i64) -> i32 = async_rwlock::willow_async_rwlock_commit;
}

#[test]
fn word_pins_match_the_shared_schema() {
    assert_ne!(Word, I64, "the semantic kinds must stay distinguishable");
    for (name, params, ret) in WORD_PINS {
        assert!(
            params.contains(&Word) || *ret == Some(Word),
            "{name} is pinned as payload-carrying but states no Word"
        );
        assert_schema(name, params, *ret);
    }
}

#[test]
fn every_word_carrying_row_is_pinned_by_the_runtime() {
    // Exhaustive: a new row that moves a generic payload word across the ABI
    // must be added to WORD_PINS (and so have its kinds asserted), so the
    // Word/I64 order is stated by the side that defines it.
    let pinned: BTreeSet<&str> = WORD_PINS.iter().map(|(name, _, _)| *name).collect();
    assert_eq!(pinned.len(), WORD_PINS.len(), "WORD_PINS repeats a symbol");
    let declared: BTreeSet<&str> = RUNTIME_SYMBOLS
        .iter()
        .filter(|symbol| symbol.params.contains(&Word) || symbol.ret == Some(Word))
        .map(|symbol| symbol.name)
        .collect();
    assert_eq!(
        declared, pinned,
        "rows carrying AbiTy::Word drifted from the runtime's semantic pins"
    );
}

#[test]
fn payload_free_collection_and_lock_rows_stay_i64() {
    // Constructors, lengths, indexes and lock tokens next to the Word rows
    // above carry kind tags, flags and ids only, never a payload word.
    let _: extern "C" fn(i64, i64) -> *mut u8 = array::willow_array_new;
    let _: extern "C" fn(*mut u8, i64) -> *mut u8 = array::willow_array_element_addr;
    let _: extern "C" fn(i64, i64, i64) -> *mut u8 = map::willow_map_new;
    assert_schema("willow_array_new", &[I64, I64], Some(Ptr));
    assert_schema("willow_array_len", &[Ptr], Some(I64));
    assert_schema("willow_array_element_addr", &[Ptr, I64], Some(Ptr));
    assert_schema("willow_map_new", &[I64, I64, I64], Some(Ptr));
    for lock in ["mutex", "rwlock"] {
        assert_schema(&format!("willow_async_{lock}_poll"), &[Ptr, I64], Some(I32));
        assert_schema(
            &format!("willow_async_{lock}_release"),
            &[Ptr, I64],
            Some(I32),
        );
    }
}

#[test]
fn typed_scalar_rows_never_carry_a_word() {
    // Scalar-typed channel, atomic and future entry points are monomorphic
    // `i64` payloads with `_ptr`/`_bool`/`_f64` siblings; they are `I64`, not
    // `Word`, and their pointer siblings are `Ptr`.
    let _: extern "C" fn(*mut c_void, i64) = channel::willow_channel_send_i64;
    let _: extern "C" fn(*mut c_void, *mut c_void) = channel::willow_channel_send_ptr;
    assert_schema("willow_channel_send_i64", &[Ptr, I64], None);
    assert_schema("willow_channel_send_ptr", &[Ptr, Ptr], None);
    assert_schema("willow_channel_recv_i64", &[Ptr], Some(I64));
    assert_schema("willow_channel_recv_ptr", &[Ptr], Some(Ptr));
    assert_schema("willow_channel_send_bool", &[Ptr, I8], None);
    assert_schema("willow_channel_send_f64", &[Ptr, F64], None);
    assert_schema("willow_atomic_i64_new", &[I64], Some(Ptr));
    assert_schema("willow_atomic_i64_swap", &[Ptr, I64], Some(I64));
    assert_schema("willow_future_ready_i64", &[I64], Some(Ptr));
    assert_schema("willow_future_ready_ptr", &[Ptr], Some(Ptr));
    assert_schema("willow_future_await_i64", &[Ptr], Some(I64));
}

#[test]
fn flags_ids_and_sizes_are_i64_next_to_native_pointers() {
    // Rows that mix pointers with fixed-width integers, pinned so a pointer
    // and an integer cannot swap places behind identical 64-bit lowering.
    assert_schema(
        "willow_gc_alloc_slow",
        &[Ptr, I64, I64, I64, I64],
        Some(Ptr),
    );
    assert_schema("willow_gc_alloc_bitmap", &[I64, I64, Ptr], Some(Ptr));
    assert_schema("willow_gc_write_barrier", &[Ptr, Ptr, Ptr, I64], None);
    assert_schema("willow_gc_stats_snapshot", &[I64, Ptr, I64], Some(I64));
    assert_schema("willow_string_alloc", &[Ptr, I64], Some(Ptr));
    assert_schema("willow_sched_spawn", &[Ptr, Ptr], Some(I64));
    assert_schema("willow_sched_set_spawn_site", &[I64, Ptr, I64], None);
    assert_schema(
        "willow_sched_set_cancel_fn_cooperative",
        &[I64, Ptr, I32],
        None,
    );
    assert_schema("willow_frame_await", &[Ptr, I64], Some(I32));
    assert_schema(
        "willow_callstack_push",
        &[Ptr, I64, Ptr, I64, I32, I32],
        None,
    );
    assert_schema("willow_panic_raise", &[Ptr, Ptr, I64, I64], None);
    assert_schema("willow_async_frame_alloc", &[I64, I64], Some(Ptr));
    assert_schema("willow_parallel_map_i64", &[Ptr, Ptr], Some(Ptr));
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

#[test]
fn interface_dispatch_layout_matches_native_pointer_aggregate() {
    #[repr(C)]
    struct InterfaceBox {
        object: *mut c_void,
        vtable: *const c_void,
    }
    let pointer_bytes = std::mem::size_of::<*const c_void>() as u32;
    assert_eq!(
        willow_abi::dispatch_layout::interface_bytes(pointer_bytes) as usize,
        std::mem::size_of::<InterfaceBox>()
    );
    assert_eq!(
        willow_abi::dispatch_layout::OBJECT_OFFSET as usize,
        std::mem::offset_of!(InterfaceBox, object)
    );
    assert_eq!(
        willow_abi::dispatch_layout::vtable_offset(pointer_bytes) as usize,
        std::mem::offset_of!(InterfaceBox, vtable)
    );
}

#[test]
fn lazy_task_stack_entrypoints_keep_pointer_callback_abi() {
    let _: extern "C" fn(crate::task::RuntimePollFn, *mut c_void) -> i64 =
        crate::scheduler::willow_sched_spawn_cooperative;
    let _: extern "C" fn(crate::task::RuntimePollFn, *mut c_void) -> i32 =
        crate::scheduler::willow_task_stack_enter;
    let _: extern "C" fn() = crate::scheduler::willow_task_stack_leave;
}

#[test]
fn tlab_storage_size_matches_target_contract() {
    let width = std::mem::size_of::<usize>() as u32;
    assert_eq!(
        willow_abi::tlab::state_size(width) as usize,
        std::mem::size_of::<crate::gc::GcTlabState>()
    );
}
