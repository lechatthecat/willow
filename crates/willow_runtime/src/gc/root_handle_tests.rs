//! GC-02 regressions. Included beneath gc::tests to share its isolated fixtures.
use super::*;

fn young_word(tls: &TestTlab, word: i64) -> *mut u8 {
    // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size and
    // tracing metadata describe the object initialized here.
    let value = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 901, 0, 8, 0) };
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    unsafe { value.cast::<i64>().write(word) };
    value
}

#[test]
fn normal_handle_move_park_resume_cancel_and_unwind() {
    let _guard = isolated_runtime();
    for exit in ["resume", "cancel", "panic"] {
        reset_gc();
        let tls = new_tlab_state();
        let before = runtime_root_count();
        let original = young_word(&tls, 73);
        let root = GcRootHandle::new(original);
        assert_eq!(runtime_root_count(), before + 1);
        let mut empty_slot = std::ptr::null_mut();
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut empty_slot) };
        let scope = RelocationSafeScope::enter();
        // SAFETY: The owning thread parks its live shadow stack at a valid depth and keeps the
        // slots alive until discharge.
        let token = unsafe { park_current_roots(0) };
        drop(scope);
        minor_collect_internal();
        assert_ne!(root.load(), original, "handle must reload after movement");
        // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
        assert_eq!(unsafe { root.load().cast::<i64>().read() }, 73);
        assert_eq!(deferred_minor_collections_for_test(), 0);
        assert_eq!(willow_gc_pinned_promotions(), 0);
        if exit == "resume" {
            // SAFETY: The matching park token still owns these live slots and is resumed by their
            // owner.
            unsafe { resume_parked_roots(token) };
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_root() };
            // Restoring the parked flag is tested separately from its guard.
            safepoint::set_relocation_safe(false);
            drop(root);
        } else {
            // SAFETY: This token belongs to the parked stack and its slots have finished serving
            // as roots.
            unsafe { discard_parked_roots(token) };
            if exit == "panic" {
                assert!(
                    std::panic::catch_unwind(|| {
                        let _root = root;
                        panic!("scoped root unwind");
                    })
                    .is_err()
                );
            } else {
                drop(root);
            }
        }
        assert_eq!(runtime_root_count(), before);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }
}

#[test]
fn normal_handle_aliases_null_store_release_and_arena_drop() {
    let _guard = isolated_runtime();
    reset_gc();
    let tls = new_tlab_state();
    let before = runtime_root_count();
    let original = young_word(&tls, 19);
    let first = GcRootHandle::new(original);
    let second = GcRootHandle::new(std::ptr::null_mut());
    second.store(original);
    minor_collect_internal();
    assert_ne!(first.load(), original);
    assert_eq!(first.load(), second.load());
    assert_eq!(willow_gc_moved_objects(), 1);
    first.release();
    first.release();
    assert!(first.load().is_null());
    drop(first);
    assert_eq!(runtime_root_count(), before + 1);
    minor_collect_internal();
    assert_eq!(
        // SAFETY: The current GC payload follows its aligned header; its owner keeps it live here.
        unsafe { (*payload_to_header(second.load())).generation },
        GC_GENERATION_OLD
    );
    drop(second);
    let owner = willow_alloc(8);
    let arena = GcRootArena::default();
    // SAFETY: This test allocated the non-null arena inputs as managed objects and retains
    // them through insertion.
    let root = unsafe { arena.insert(owner, owner) };
    let mut slots = Vec::new();
    arena.trace_slots(&mut slots);
    assert_eq!(slots.len(), 1);
    drop(root);
    // SAFETY: The owner of this registered or traced slot keeps its pointer storage live for this
    // read or write.
    assert!(unsafe { (*slots[0]).is_null() });
    assert_eq!(runtime_root_count(), before);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
fn scoped_loan_park_resume_cancel_panic_and_stack_alias() {
    let _guard = isolated_runtime();
    for exit in ["resume", "cancel", "panic"] {
        for kind in ["interior", "ffi"] {
            reset_gc();
            let tls = new_tlab_state();
            let original = young_word(&tls, 37);
            let before = runtime_root_count();
            let mut stack_alias = original;
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut stack_alias) };
            // SAFETY: The owning thread parks its live shadow stack at a valid depth and keeps the
            // slots alive until discharge.
            let token = unsafe { park_current_roots(0) };
            // Both loans pin their young owner in place; a foreign (native
            // boundary) loan also publishes it first.
            let foreign = kind == "ffi";
            let run = |loan_address: *mut u8| {
                minor_collect_internal();
                assert_eq!(loan_address, original);
                assert_eq!(stack_alias, original);
                // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar
                // value.
                assert_eq!(unsafe { original.cast::<i64>().read() }, 37);
                let object = HeapObject::from_payload(GcPayload::from_raw(original).unwrap());
                assert_eq!(object.shared(), foreign);
                assert_eq!(object.generation(), GC_GENERATION_OLD);
                assert_eq!(willow_gc_pinned_promotions(), 1);
                let pins = crate::gc_telemetry::pin_snapshot();
                assert_eq!(pins.len(), 1);
                assert_eq!(pins[0].reason, "scoped fixed-address loan");
                assert_eq!(pins[0].owner_allocation, original as usize);
                assert_eq!(deferred_minor_collections_for_test(), 0);
                if exit == "panic" {
                    panic!("scoped loan unwind");
                }
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if kind == "interior" {
                    // SAFETY: The byte offset lies within the live allocation, including its
                    // one-past address when forming a pointer.
                    let loan = PinLease::<InteriorPointer>::for_address(unsafe { original.add(3) })
                        .unwrap();
                    assert_eq!(runtime().pin_lease_slots.len(), 1);
                    run(loan.load());
                } else {
                    let loan = PinLease::<ForeignReference>::new(original);
                    run(loan.load());
                }
            }));
            assert_eq!(result.is_err(), exit == "panic");
            assert_eq!(runtime().pin_lease_slots.len(), 0);
            assert_eq!(runtime_root_count(), before);
            if exit == "resume" {
                // SAFETY: The matching park token still owns these live slots and is resumed by
                // their owner.
                unsafe { resume_parked_roots(token) };
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { willow_pop_root() };
            } else {
                // SAFETY: This token belongs to the parked stack and its slots have finished
                // serving as roots.
                unsafe { discard_parked_roots(token) };
            }
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
            let stats = crate::gc_telemetry::survivor_snapshot();
            assert_eq!(stats.active_pins, 0);
            assert_eq!(stats.active_pin_bytes, 0);
            assert_eq!(stats.released_pins, 1);
            reset_gc();
        }
    }
}

#[test]
fn scoped_loan_native_old_retired_and_nested_lifetimes() {
    let _guard = isolated_runtime();
    reset_gc();
    let native = [1u8; 16];
    assert!(PinLease::<InteriorPointer>::for_address(native.as_ptr()).is_none());
    assert!(PinLease::<InteriorPointer>::for_address(std::ptr::null()).is_none());
    let tls = new_tlab_state();
    let original = young_word(&tls, 23);
    retire_tlabs_for_thread(std::thread::current().id());
    // SAFETY: The byte offset lies within the live allocation, including its one-past address when
    // forming a pointer.
    let first = PinLease::<InteriorPointer>::for_address(unsafe { original.add(1) }).unwrap();
    minor_collect_internal();
    assert_eq!(first.load(), original);
    assert_eq!(willow_gc_pinned_promotions(), 1);
    // A foreign loan of the now pinned-old owner publishes it without a
    // second promotion; both loans keep the address.
    let second = PinLease::<ForeignReference>::new(original);
    minor_collect_internal();
    assert_eq!(first.load(), original);
    assert_eq!(second.load(), original);
    assert!(HeapObject::from_payload(GcPayload::from_raw(original).unwrap()).shared());
    assert_eq!(willow_gc_pinned_promotions(), 1);
    drop(first);
    assert_eq!(runtime().pin_lease_slots.len(), 1);
    willow_gc_collect();
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    assert_eq!(unsafe { second.load().cast::<i64>().read() }, 23);
    drop(second);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    let old = willow_alloc(16);
    // SAFETY: The byte offset lies within the live allocation, including its one-past address when
    // forming a pointer.
    let loan = PinLease::<InteriorPointer>::for_address(unsafe { old.add(8) }).unwrap();
    assert_eq!(loan.load(), old);
    minor_collect_internal();
    drop(loan);
    willow_gc_collect();
    assert_eq!(runtime_root_count(), 0);
    reset_gc();
}

#[test]
fn scoped_loan_bridge_deduplicates_and_preserves_foreign_string_data() {
    use crate::rust_bridge_abi::aggregate::*;
    let _guard = isolated_runtime();
    reset_gc();
    let tls = new_tlab_state();
    // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size and
    // tracing metadata describe the object initialized here.
    let string = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 902, 0, 16, 0) };
    // SAFETY: The test allocated a young string with space for this length, the copied bytes and
    // the NUL.
    unsafe {
        crate::string::initialize_length(string, 3);
        std::ptr::copy_nonoverlapping(c"abc".as_ptr().cast(), string.add(8), 4);
    }
    let frame = willow_rust_bridge_frame_new();
    for _ in 0..256 {
        // SAFETY: The adapter retains exclusive ownership of the frame and passes the current
        // payload for pinning.
        unsafe { willow_rust_bridge_root(frame, string as u64) };
    }
    assert_eq!(runtime_root_count(), 1);
    assert_eq!(runtime().pin_lease_slots.len(), 1);
    let mut data = willow_abi::ffi::WillowSliceU8 {
        ptr: std::ptr::null(),
        len: 0,
    };
    // SAFETY: The test pins the live string in its bridge frame and supplies a writable local slice
    // descriptor.
    unsafe { willow_rust_bridge_string_data(string as u64, &mut data) };
    willow_gc_minor_collect();
    assert_eq!(
        // SAFETY: The bridge frame pins the source and the returned length bounds the borrowed
        // bytes.
        unsafe { std::slice::from_raw_parts(data.ptr, data.len) },
        b"abc"
    );
    willow_gc_collect();
    assert_eq!(
        // SAFETY: The bridge frame pins the source and the returned length bounds the borrowed
        // bytes.
        unsafe { std::slice::from_raw_parts(data.ptr, data.len) },
        b"abc"
    );
    // SAFETY: The fixture owns the original frame and has finished using every borrowed
    // payload.
    unsafe { willow_rust_bridge_frame_drop(frame) };
    assert_eq!(runtime_root_count(), 0);
    assert_eq!(runtime().pin_lease_slots.len(), 0);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

// Deterministically stop this mutator inside the operation's allocation, so
// tests inspect actual movement, rather than assuming stress triggered it.
fn during_minor<R>(operation: impl FnOnce() -> R) -> R {
    let _registration = MutatorRegistration::new();
    let collector = std::thread::spawn(crate::runtime::serving(|| willow_gc_minor_collect()));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !runtime().poll_requested.load(Ordering::Acquire) {
        assert!(
            std::time::Instant::now() < deadline,
            "collector did not request a stop"
        );
        std::thread::yield_now();
    }
    set_gc_stress_for_test(Some("alloc"));
    let result = operation();
    set_gc_stress_for_test(Some(""));
    collector.join().unwrap();
    result
}

#[test]
fn normal_handle_runtime_string_and_enum_paths_move_without_pin() {
    let _guard = isolated_runtime();
    for operation in ["concat", "substring", "trim", "repeat", "split", "enum"] {
        reset_gc();
        set_gc_stress_for_test(Some(""));
        let tls = new_tlab_state();
        // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size
        // and tracing metadata describe the object initialized here.
        let original = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 903, 0, 24, 0) };
        // SAFETY: The test allocated a young string with space for this length, the copied bytes
        // and the NUL.
        unsafe {
            crate::string::initialize_length(original, 5);
            std::ptr::copy_nonoverlapping(c" a,b ".as_ptr().cast(), original.add(8), 6);
        }
        let source = GcRootHandle::new(original);
        let result = during_minor(|| match operation {
            // SAFETY: The handle is reloaded on entry; the fixture initialized the length, text and
            // terminator.
            "concat" => unsafe {
                crate::string::willow_string_concat(source.load(), source.load())
            },
            // SAFETY: The handle is reloaded on entry; the fixture initialized the length, text and
            // terminator.
            "substring" => unsafe { crate::string::willow_string_substring(source.load(), 1, 4) },
            // SAFETY: The handle is reloaded on entry; the fixture initialized the length, text and
            // terminator.
            "trim" => unsafe { crate::string::willow_string_trim(source.load()) },
            // SAFETY: The handle is reloaded on entry; the fixture initialized the length, text and
            // terminator.
            "repeat" => unsafe { crate::string::willow_string_repeat(source.load(), 2) },
            "split" => {
                // Separator is native/runtime-old; allocate before the pending
                // stop so only the operation itself consumes the handshake.
                // SAFETY: The handle is reloaded on entry; the fixture initialized the length, text
                // and terminator.
                unsafe { crate::string::willow_string_split(source.load(), std::ptr::null()) }
            }
            // SAFETY: The variant layout selects its initialized word or reference payload,
            // including the traced low word for a pair.
            _ => unsafe {
                willow_alloc_enum_variant(
                    0,
                    willow_abi::EnumVariantLayout::new(0, &[willow_abi::SlotKind::GcRef]),
                    &[source.load() as i64],
                )
            },
        });
        let result = GcRootHandle::new(result);
        assert_ne!(
            source.load(),
            original,
            "{operation} did not relocate its input"
        );
        assert_eq!(willow_gc_pinned_promotions(), 0, "{operation}");
        assert_eq!(deferred_minor_collections_for_test(), 0, "{operation}");
        if operation == "split" {
            // SAFETY: The root supplies the current live array for this access.
            assert_eq!(unsafe { crate::array::willow_array_len(result.load()) }, 7);
        } else if operation == "enum" {
            assert_eq!(
                // SAFETY: The fixture allocates these fields in word-aligned GC storage at
                // whole-word offsets.
                unsafe { result.load().add(8).cast::<*mut u8>().read() },
                source.load()
            );
        } else {
            let expected = match operation {
                "concat" => " a,b  a,b ",
                "substring" | "trim" => "a,b",
                _ => " a,b  a,b ",
            };
            assert_eq!(
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                unsafe { crate::string::willow_string_as_str(result.load()) },
                expected
            );
        }
        drop(result);
        drop(source);
        assert_eq!(runtime_root_count(), 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        set_gc_stress_for_test(None);
        reset_gc();
    }
}

#[test]
fn normal_handle_panic_inputs_move_and_remain_remembered_after_transfer() {
    use crate::panic_context::*;
    let _guard = isolated_runtime();
    reset_gc();
    struct Restore(Option<std::sync::Arc<PanicContext>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            replace_current_context(self.0.take());
        }
    }
    let _restore = Restore(replace_current_context(Some(std::sync::Arc::new(
        PanicContext::new(909),
    ))));
    set_gc_stress_for_test(Some(""));
    let tls = new_tlab_state();
    // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size and
    // tracing metadata describe the object initialized here.
    let original = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 905, 0, 16, 0) };
    // SAFETY: The test allocated a young string with space for this length, the copied bytes and
    // the NUL.
    unsafe {
        crate::string::initialize_length(original, 3);
        std::ptr::copy_nonoverlapping(c"abc".as_ptr().cast(), original.add(8), 4);
    }
    let source = GcRootHandle::new(original);
    // SAFETY: The optional source file is null or a current Willow string and the panic path
    // roots its string inputs before allocation.
    during_minor(|| unsafe { willow_panic_raise(source.load(), source.load(), 7, 9) });
    assert_ne!(source.load(), original);
    assert_eq!(willow_gc_pinned_promotions(), 0);
    assert_eq!(deferred_minor_collections_for_test(), 0);
    willow_panic_enter_defer();
    let info = willow_panic_recover();
    let info_root = GcRootHandle::new(info);
    assert!(HeapObject::from_payload(GcPayload::from_raw(info).unwrap()).remembered());
    willow_panic_release_recovered(info);
    willow_panic_leave_defer();
    drop(source);
    let parent = willow_alloc_typed(8, 1);
    // SAFETY: The live object's trace layout reserves these aligned pointer slots.
    unsafe { store_gc_reference(parent.cast(), info) };
    let parent_root = GcRootHandle::new(parent);
    drop(info_root);
    minor_collect_internal();
    // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
    assert_eq!(unsafe { panic_info_message(info) }, "abc");
    // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
    assert_eq!(unsafe { panic_info_file(info) }, "abc");
    // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
    assert_eq!(unsafe { panic_info_line(info) }, 7);
    // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
    assert_eq!(unsafe { panic_info_column(info) }, 9);
    drop(parent_root);
    assert_eq!(runtime_root_count(), 0);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    set_gc_stress_for_test(None);
    reset_gc();
}

#[test]
fn normal_handle_lock_constructors_reload_reference_arguments() {
    let _guard = isolated_runtime();
    for kind in [
        "blocking_mutex",
        "blocking_rwlock",
        "async_mutex",
        "async_rwlock",
    ] {
        reset_gc();
        set_gc_stress_for_test(Some(""));
        let tls = new_tlab_state();
        let original = young_word(&tls, 91);
        let source = GcRootHandle::new(original);
        let payload = during_minor(|| match kind {
            // SAFETY: The initial reference comes from a live managed allocation and is
            // current on entry.
            "blocking_mutex" => unsafe {
                crate::lock::willow_blocking_cell_new(source.load() as i64, 1)
            },
            // SAFETY: The initial reference comes from a live managed allocation and is
            // current on entry.
            "blocking_rwlock" => unsafe {
                crate::lock::willow_blocking_rw_cell_new(source.load() as i64, 1)
            },
            // SAFETY: The initial word matches the constructor’s reference flag.
            "async_mutex" => unsafe {
                crate::async_mutex::willow_async_mutex_new(source.load() as i64, 1)
            },
            // SAFETY: The initial word matches the constructor’s reference flag.
            _ => unsafe { crate::async_rwlock::willow_async_rwlock_new(source.load() as i64, 1) },
        });
        assert_ne!(source.load(), original, "{kind} did not move the source");
        let object = HeapObject::from_payload(GcPayload::from_raw(payload.cast()).unwrap());
        let slots = object_reference_slots(object, &type_registry().lock().unwrap());
        assert_eq!(slots.len(), 1);
        assert_eq!(
            // SAFETY: The owner of this registered or traced slot keeps its pointer storage live
            // for this read or write.
            unsafe { *slots[0] },
            source.load(),
            "{kind} stored a stale value"
        );
        assert_eq!(willow_gc_pinned_promotions(), 0);
        assert_eq!(deferred_minor_collections_for_test(), 0);
        drop(source);
        assert_eq!(runtime_root_count(), 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        set_gc_stress_for_test(None);
        reset_gc();
    }
}

#[test]
fn scoped_loan_string_interior_copy_survives_a_pending_minor() {
    let _guard = isolated_runtime();
    reset_gc();
    set_gc_stress_for_test(Some(""));
    let tls = new_tlab_state();
    // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size and
    // tracing metadata describe the object initialized here.
    let original = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 904, 0, 16, 0) };
    // SAFETY: The test allocated a young string with space for this length, the copied bytes and
    // the NUL.
    unsafe {
        crate::string::initialize_length(original, 3);
        std::ptr::copy_nonoverlapping(c"abc".as_ptr().cast(), original.add(8), 4);
    }
    // SAFETY: The byte offset lies within the live allocation, including its one-past address when
    // forming a pointer.
    // SAFETY: The initialized payload covers the three bytes at offset eight; the allocator pins
    // them for the copy.
    let result = during_minor(|| unsafe { crate::string::willow_string_alloc(original.add(8), 3) });
    assert_eq!(
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        unsafe { crate::string::willow_string_as_str(result) },
        "abc"
    );
    assert_eq!(willow_gc_pinned_promotions(), 1);
    assert_eq!(deferred_minor_collections_for_test(), 0);
    assert_eq!(runtime_root_count(), 0);
    assert_eq!(runtime().pin_lease_slots.len(), 0);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    set_gc_stress_for_test(None);
    reset_gc();
}

#[test]
fn verify_no_pin_rejects_an_actual_scoped_young_loan() {
    const CHILD: &str = "WILLOW_GC02_VERIFY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "gc::tests::root_handle_tests::verify_no_pin_rejects_an_actual_scoped_young_loan",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("WILLOW_GC_VERIFY_NO_PIN", "1")
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("WILLOW_GC_VERIFY_NO_PIN"), "{stderr}");
        assert!(stderr.contains("scoped fixed-address loan"), "{stderr}");
        return;
    }
    let _guard = isolated_runtime();
    reset_gc();
    let tls = new_tlab_state();
    let _loan = PinLease::<ForeignReference>::new(young_word(&tls, 1));
    minor_collect_internal();
}

#[test]
fn normal_handle_concurrent_snapshot_churn_uses_live_atomic_slots() {
    let owner = Arc::new(crate::runtime::Runtime::new(
        crate::scheduler::environment_worker_config,
    ));
    // SAFETY: This fixture selects the payload size and tracing layout for its object and
    // initializes any native fields before collection.
    let old = unsafe { allocate_old_in(&owner, 0, 0, 8, 0) };
    root::add_task_frame_root_in(&owner, old);
    let address = old as usize;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(5));
    let mut threads = Vec::new();
    for _ in 0..4 {
        let barrier = std::sync::Arc::clone(&barrier);
        let owner = Arc::clone(&owner);
        threads.push(std::thread::spawn(crate::runtime::serving(move || {
            barrier.wait();
            for _ in 0..4096 {
                let root = GcRootHandle::new_in(Arc::clone(&owner), address as *mut u8);
                root.release();
                root.store(address as *mut u8);
                std::thread::yield_now();
            }
        })));
    }
    barrier.wait();
    for _ in 0..4096 {
        let values = runtime_roots_snapshot_in(&owner.gc);
        assert!(values.iter().all(|&value| value == old));
        assert!(!values.is_empty());
    }
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(
        (owner.gc.runtime_roots.len() + owner.gc.runtime_root_slots.len()),
        1
    );
    root::remove_runtime_root_in(&owner, old);
    assert_eq!(
        try_collect_internal_in(&owner),
        CollectionAttempt::Completed
    );
    assert_eq!(
        (owner.gc.runtime_roots.len() + owner.gc.runtime_root_slots.len()),
        0
    );
    assert_eq!(owner.gc.heap.lock().unwrap().allocated_bytes, 0);
}

#[test]
fn scoped_handle_emergency_tenure_budget_keeps_the_slot_valid() {
    let _guard = isolated_runtime();
    reset_gc();
    let tls = new_tlab_state();
    let root = GcRootHandle::new(young_word(&tls, 53));
    minor_collect_internal();
    let survivor = root.load();
    {
        let mut state = runtime().heap.lock().unwrap();
        assert!(state.old_regions.is_empty());
        state.memory_limit_bytes = Some(state.tlab_reserved_bytes);
    }
    minor_collect_internal();
    assert_eq!(root.load(), survivor);
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    assert_eq!(unsafe { survivor.cast::<i64>().read() }, 53);
    assert_eq!(
        // SAFETY: The current GC payload follows its aligned header; its owner keeps it live here.
        unsafe { (*payload_to_header(survivor)).generation },
        GC_GENERATION_OLD
    );
    assert_eq!(willow_gc_pinned_promotions(), 1);
    assert_eq!(deferred_minor_collections_for_test(), 0);
    drop(root);
    willow_gc_collect();
    assert_eq!(runtime_root_count(), 0);
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
fn normal_handle_overhead_and_fragmented_registration_scaling() {
    use crate::scheduler::scaling_measurements::counting_allocator as counter;
    let _guard = isolated_runtime();
    reset_gc();
    // Measure cold/growing registry costs too; the handle vector is prepared
    // outside the measured region. Null values isolate root-management work.
    for count in [16, 64, 256, 1024] {
        let mut legacy_slots = vec![std::ptr::null_mut(); count];
        let legacy_allocations = counter::thread_allocations();
        let legacy_started = std::time::Instant::now();
        for slot in &mut legacy_slots {
            // SAFETY: The slot storage stays live and stationary through the matching pop,
            // including during collection.
            unsafe { willow_push_root(slot) };
        }
        let legacy_ns = legacy_started.elapsed().as_nanos();
        let legacy_allocated = counter::thread_allocations() - legacy_allocations;
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(count as i32) };
        let mut handles = Vec::with_capacity(count);
        let allocations = counter::thread_allocations();
        let bytes = counter::thread_bytes();
        let started = std::time::Instant::now();
        for _ in 0..count {
            handles.push(GcRootHandle::new(std::ptr::null_mut()));
        }
        let allocated = counter::thread_allocations() - allocations;
        let allocated_bytes = counter::thread_bytes() - bytes;
        let elapsed = started.elapsed().as_nanos();
        assert_eq!(runtime_root_slots(runtime()).len(), count);
        assert!(allocated >= count && allocated <= count + 128 + count / 2);
        let load_allocations = counter::thread_allocations();
        let load_started = std::time::Instant::now();
        let mut loads = 0;
        for handle in &handles {
            for _ in 0..64 {
                std::hint::black_box(handle.load());
                loads += 1;
            }
        }
        let load_ns = load_started.elapsed().as_nanos();
        assert_eq!(loads, count * 64);
        assert_eq!(counter::thread_allocations(), load_allocations);
        let mut kept = Vec::new();
        for (index, handle) in handles.into_iter().enumerate() {
            if index % 16 == 0 {
                kept.push(handle);
            }
        }
        assert_eq!(runtime_root_slots(runtime()).len(), count / 16);
        eprintln!(
            "handles={count} slot_visits={count} fragmented_visits={} handle_bytes={} cell_bytes={} allocated={allocated} allocated_bytes={allocated_bytes} create_ns={elapsed} legacy_allocated={legacy_allocated} legacy_create_ns={legacy_ns} loads={loads} load_ns={load_ns}",
            count / 16,
            std::mem::size_of::<GcRootHandle>(),
            std::mem::size_of::<GcRootCell>()
        );
        drop(kept);
        assert_eq!(runtime_root_count(), 0);
    }
    reset_gc();
}

#[test]
fn private_scoped_handles_keep_their_heap_alive_through_release_and_unwind() {
    for unwind in [false, true] {
        let owner = Arc::new(crate::runtime::Runtime::new(
            crate::scheduler::environment_worker_config,
        ));
        let peer = Arc::new(crate::runtime::Runtime::new(
            crate::scheduler::environment_worker_config,
        ));
        let weak = Arc::downgrade(&owner);
        // SAFETY: This fixture selects the payload size and tracing layout for its object and
        // initializes any native fields before collection.
        let old = unsafe { allocate_old_in(&owner, 0, 0, 8, 0) };
        // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
        unsafe { old.cast::<u64>().write(73) };
        let root = GcRootHandle::new_in(Arc::clone(&owner), old);
        let alias = GcRootHandle::new_in(Arc::clone(&owner), std::ptr::null_mut());
        alias.store(root.load());
        assert_eq!(owner.gc.runtime_root_slots.len(), 2);
        assert_eq!(peer.gc.runtime_root_slots.len(), 0);
        root.release();
        root.release();
        drop(root);
        assert_eq!(owner.gc.runtime_root_slots.len(), 1);
        assert_eq!(
            try_collect_internal_in(&owner),
            CollectionAttempt::Completed
        );
        // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
        assert_eq!(unsafe { alias.load().cast::<u64>().read() }, 73);
        drop(owner);
        assert!(weak.upgrade().is_some());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _alias = alias;
            if unwind {
                panic!("private scoped root unwind");
            }
        }));
        assert_eq!(outcome.is_err(), unwind);
        assert!(weak.upgrade().is_none());
        assert_eq!(peer.gc.runtime_root_slots.len(), 0);
    }
}
