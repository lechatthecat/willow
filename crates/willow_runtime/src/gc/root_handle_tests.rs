//! GC-02 regressions. Included beneath gc::tests to share its isolated fixtures.
use super::*;

fn young_word(tls: &mut GcTlabState, word: i64) -> *mut u8 {
    let value = willow_gc_alloc_slow(tls, 901, 0, 8, 0);
    unsafe { value.cast::<i64>().write(word) };
    value
}

#[test]
fn normal_handle_move_park_resume_cancel_and_unwind() {
    let _guard = gc_test_guard();
    for exit in ["resume", "cancel", "panic"] {
        reset_gc();
        let mut tls = new_tlab_state();
        let before = runtime_root_count();
        let original = young_word(&mut tls, 73);
        let root = GcRootHandle::new(original);
        assert_eq!(runtime_root_count(), before + 1);
        let mut empty_slot = std::ptr::null_mut();
        willow_push_root(&mut empty_slot);
        let scope = RelocationSafeScope::enter();
        let token = unsafe { park_current_roots(0) };
        drop(scope);
        minor_collect_internal();
        assert_ne!(root.load(), original, "handle must reload after movement");
        assert_eq!(unsafe { root.load().cast::<i64>().read() }, 73);
        assert_eq!(deferred_minor_collections_for_test(), 0);
        assert_eq!(willow_gc_pinned_promotions(), 0);
        if exit == "resume" {
            unsafe { resume_parked_roots(token) };
            willow_pop_root();
            // Restoring the parked flag is tested separately from its guard.
            safepoint::set_relocation_safe(false);
            drop(root);
        } else {
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
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let before = runtime_root_count();
    let original = young_word(&mut tls, 19);
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
        unsafe { (*payload_to_header(second.load())).generation },
        GC_GENERATION_OLD
    );
    drop(second);
    let owner = willow_alloc(8);
    let arena = GcRootArena::default();
    let root = arena.insert(owner, owner);
    let mut slots = Vec::new();
    arena.trace_slots(&mut slots);
    assert_eq!(slots.len(), 1);
    drop(root);
    assert!(unsafe { (*slots[0]).is_null() });
    assert_eq!(runtime_root_count(), before);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
fn scoped_loan_park_resume_cancel_panic_and_stack_alias() {
    let _guard = gc_test_guard();
    for exit in ["resume", "cancel", "panic"] {
        for kind in ["interior", "ffi"] {
            reset_gc();
            let mut tls = new_tlab_state();
            let original = young_word(&mut tls, 37);
            let before = runtime_root_count();
            let mut stack_alias = original;
            willow_push_root(&mut stack_alias);
            let token = unsafe { park_current_roots(0) };
            let run = |loan_address: *mut u8| {
                minor_collect_internal();
                assert_eq!(loan_address, original);
                assert_eq!(stack_alias, original);
                assert_eq!(unsafe { original.cast::<i64>().read() }, 37);
                assert_eq!(willow_gc_pinned_promotions(), 1);
                assert_eq!(deferred_minor_collections_for_test(), 0);
                if exit == "panic" {
                    panic!("scoped loan unwind");
                }
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if kind == "interior" {
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
                unsafe { resume_parked_roots(token) };
                willow_pop_root();
            } else {
                unsafe { discard_parked_roots(token) };
            }
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
            reset_gc();
        }
    }
}

#[test]
fn scoped_loan_native_old_retired_and_nested_lifetimes() {
    let _guard = gc_test_guard();
    reset_gc();
    let native = [1u8; 16];
    assert!(PinLease::<InteriorPointer>::for_address(native.as_ptr()).is_none());
    assert!(PinLease::<InteriorPointer>::for_address(std::ptr::null()).is_none());
    let mut tls = new_tlab_state();
    let original = young_word(&mut tls, 23);
    retire_tlabs_for_thread(std::thread::current().id());
    let first = PinLease::<InteriorPointer>::for_address(unsafe { original.add(1) }).unwrap();
    let second = PinLease::<ForeignReference>::new(original);
    minor_collect_internal();
    assert_eq!(first.load(), original);
    assert_eq!(second.load(), original);
    assert_eq!(willow_gc_pinned_promotions(), 1);
    drop(first);
    assert_eq!(runtime().pin_lease_slots.len(), 1);
    willow_gc_collect();
    assert_eq!(unsafe { second.load().cast::<i64>().read() }, 23);
    drop(second);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    let old = willow_alloc(16);
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
    use crate::rust_bridge::aggregate::*;
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let string = willow_gc_alloc_slow(&mut tls, 902, 0, 16, 0);
    unsafe {
        string.cast::<i64>().write(3);
        std::ptr::copy_nonoverlapping(c"abc".as_ptr().cast(), string.add(8), 4);
    }
    let frame = willow_rust_bridge_frame_new();
    for _ in 0..256 {
        willow_rust_bridge_root(frame, string as u64);
    }
    assert_eq!(runtime_root_count(), 1);
    assert_eq!(runtime().pin_lease_slots.len(), 1);
    let mut data = willow_abi::ffi::WillowSliceU8 {
        ptr: std::ptr::null(),
        len: 0,
    };
    willow_rust_bridge_string_data(string as u64, &mut data);
    willow_gc_minor_collect();
    assert_eq!(
        unsafe { std::slice::from_raw_parts(data.ptr, data.len) },
        b"abc"
    );
    willow_gc_collect();
    assert_eq!(
        unsafe { std::slice::from_raw_parts(data.ptr, data.len) },
        b"abc"
    );
    willow_rust_bridge_frame_drop(frame);
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
    let collector = std::thread::spawn(|| willow_gc_minor_collect());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while unsafe { *willow_gc_stop_flag() } == 0 {
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
    let _guard = gc_test_guard();
    for operation in ["concat", "substring", "trim", "repeat", "split", "enum"] {
        reset_gc();
        set_gc_stress_for_test(Some(""));
        let mut tls = new_tlab_state();
        let original = willow_gc_alloc_slow(&mut tls, 903, 0, 24, 0);
        unsafe {
            original.cast::<i64>().write(5);
            std::ptr::copy_nonoverlapping(c" a,b ".as_ptr().cast(), original.add(8), 6);
        }
        let source = GcRootHandle::new(original);
        let result = during_minor(|| match operation {
            "concat" => crate::string::willow_string_concat(source.load(), source.load()),
            "substring" => crate::string::willow_string_substring(source.load(), 1, 4),
            "trim" => crate::string::willow_string_trim(source.load()),
            "repeat" => crate::string::willow_string_repeat(source.load(), 2),
            "split" => {
                // Separator is native/runtime-old; allocate before the pending
                // stop so only the operation itself consumes the handshake.
                crate::string::willow_string_split(source.load(), std::ptr::null())
            }
            _ => willow_alloc_enum_variant(
                0,
                willow_abi::EnumVariantLayout::new(0, &[willow_abi::SlotKind::GcRef]),
                &[source.load() as i64],
            ),
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
            assert_eq!(crate::array::willow_array_len(result.load()), 7);
        } else if operation == "enum" {
            assert_eq!(
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
    let _guard = gc_test_guard();
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
    let mut tls = new_tlab_state();
    let original = willow_gc_alloc_slow(&mut tls, 905, 0, 16, 0);
    unsafe {
        original.cast::<i64>().write(3);
        std::ptr::copy_nonoverlapping(c"abc".as_ptr().cast(), original.add(8), 4);
    }
    let source = GcRootHandle::new(original);
    during_minor(|| willow_panic_raise(source.load(), source.load(), 7, 9));
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
    unsafe { store_gc_reference(parent.cast(), info) };
    let parent_root = GcRootHandle::new(parent);
    drop(info_root);
    minor_collect_internal();
    assert_eq!(unsafe { panic_info_message(info) }, "abc");
    assert_eq!(unsafe { panic_info_file(info) }, "abc");
    assert_eq!(unsafe { panic_info_line(info) }, 7);
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
    let _guard = gc_test_guard();
    for kind in [
        "blocking_mutex",
        "blocking_rwlock",
        "async_mutex",
        "async_rwlock",
    ] {
        reset_gc();
        set_gc_stress_for_test(Some(""));
        let mut tls = new_tlab_state();
        let original = young_word(&mut tls, 91);
        let source = GcRootHandle::new(original);
        let payload = during_minor(|| match kind {
            "blocking_mutex" => crate::lock::willow_blocking_cell_new(source.load() as i64, 1),
            "blocking_rwlock" => crate::lock::willow_blocking_rw_cell_new(source.load() as i64, 1),
            "async_mutex" => crate::async_mutex::willow_async_mutex_new(source.load() as i64, 1),
            _ => crate::async_rwlock::willow_async_rwlock_new(source.load() as i64, 1),
        });
        assert_ne!(source.load(), original, "{kind} did not move the source");
        let object = HeapObject::from_payload(GcPayload::from_raw(payload.cast()).unwrap());
        let slots = object_reference_slots(object, &type_registry().lock().unwrap());
        assert_eq!(slots.len(), 1);
        assert_eq!(
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
    let _guard = gc_test_guard();
    reset_gc();
    set_gc_stress_for_test(Some(""));
    let mut tls = new_tlab_state();
    let original = willow_gc_alloc_slow(&mut tls, 904, 0, 16, 0);
    unsafe {
        original.cast::<i64>().write(3);
        std::ptr::copy_nonoverlapping(c"abc".as_ptr().cast(), original.add(8), 4);
    }
    let result = during_minor(|| crate::string::willow_string_alloc(unsafe { original.add(8) }, 3));
    assert_eq!(
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
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let _loan = PinLease::<ForeignReference>::new(young_word(&mut tls, 1));
    minor_collect_internal();
}

#[test]
fn normal_handle_concurrent_snapshot_churn_uses_live_atomic_slots() {
    let _guard = gc_test_guard();
    reset_gc();
    let old = willow_alloc(8);
    willow_gc_add_runtime_root(old);
    let address = old as usize;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(5));
    let mut threads = Vec::new();
    for _ in 0..4 {
        let barrier = std::sync::Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            for _ in 0..4096 {
                let root = GcRootHandle::new(address as *mut u8);
                root.release();
                root.store(address as *mut u8);
                std::thread::yield_now();
            }
        }));
    }
    barrier.wait();
    for _ in 0..4096 {
        let values = runtime_roots_snapshot();
        assert!(values.iter().all(|&value| value == old));
        assert!(!values.is_empty());
    }
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(runtime_root_count(), 1);
    willow_gc_remove_runtime_root(old);
    willow_gc_collect();
    assert_eq!(runtime_root_count(), 0);
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}

#[test]
fn scoped_handle_emergency_tenure_budget_keeps_the_slot_valid() {
    let _guard = gc_test_guard();
    reset_gc();
    let mut tls = new_tlab_state();
    let root = GcRootHandle::new(young_word(&mut tls, 53));
    minor_collect_internal();
    let survivor = root.load();
    {
        let mut state = runtime().heap.lock().unwrap();
        assert!(state.old_regions.is_empty());
        state.memory_limit_bytes = Some(state.tlab_reserved_bytes);
    }
    minor_collect_internal();
    assert_eq!(root.load(), survivor);
    assert_eq!(unsafe { survivor.cast::<i64>().read() }, 53);
    assert_eq!(
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
    let _guard = gc_test_guard();
    reset_gc();
    // Measure cold/growing registry costs too; the handle vector is prepared
    // outside the measured region. Null values isolate root-management work.
    for count in [16, 64, 256, 1024] {
        let mut legacy_slots = vec![std::ptr::null_mut(); count];
        let legacy_allocations = counter::thread_allocations();
        let legacy_started = std::time::Instant::now();
        for slot in &mut legacy_slots {
            willow_push_root(slot);
        }
        let legacy_ns = legacy_started.elapsed().as_nanos();
        let legacy_allocated = counter::thread_allocations() - legacy_allocations;
        willow_pop_roots(count as i32);
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
        assert_eq!(runtime_root_slots().len(), count);
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
        assert_eq!(runtime_root_slots().len(), count / 16);
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
