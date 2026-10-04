//! Reset must destroy native owners exactly once, independently of storage.
use super::*;

const OWNER_TYPE: u32 = 98762;
static LIVE: AtomicUsize = AtomicUsize::new(0);

unsafe fn drop_owner(payload: *mut u8) {
    unsafe { drop(Box::from_raw(*payload.cast::<*mut usize>())) };
    LIVE.fetch_sub(1, Ordering::Relaxed);
}

fn initialize_owner(payload: *mut u8) {
    assert!(!payload.is_null());
    unsafe { *payload.cast::<*mut usize>() = Box::into_raw(Box::new(42)) };
    LIVE.fetch_add(1, Ordering::Relaxed);
}

#[test]
fn reset_finalizes_native_owners_in_every_storage_kind() {
    let _guard = runtime_test_guard();
    reset_internal();
    for kind in ["old", "large", "young", "survivor", "tenured", "fragmented"] {
        for count in [1usize, 8, 32] {
            willow_register_drop(OWNER_TYPE, drop_owner);
            let mut tls = tlab_state_for_test();
            // Young referents need rewritable slots; full capacity keeps the
            // registered slot addresses stable.
            let mut young_slots = Vec::<*mut u8>::with_capacity(count);
            for index in 0..count {
                let payload = if matches!(kind, "young" | "survivor" | "tenured") {
                    willow_gc_alloc_slow(&mut tls, 1, OWNER_TYPE as i64, 8, 0)
                } else {
                    let size = if kind == "large" {
                        GC_LARGE_OBJECT_THRESHOLD
                    } else {
                        8
                    };
                    willow_alloc_with_layout(GcObjectKind::Class, OWNER_TYPE, size as i64, 0)
                };
                initialize_owner(payload);
                if matches!(kind, "survivor" | "tenured") {
                    let owner = willow_alloc_typed(8, 1);
                    unsafe { *owner.cast::<*mut u8>() = payload };
                    willow_gc_add_runtime_root(owner);
                } else if kind == "young" {
                    young_slots.push(payload);
                    willow_gc_add_runtime_root_slot(young_slots.last_mut().unwrap());
                } else if kind != "fragmented" || index % 2 == 0 {
                    willow_gc_add_runtime_root(payload);
                }
            }
            if matches!(kind, "survivor" | "tenured") {
                crate::gc::minor_collect_internal();
                assert_eq!(willow_gc_moved_objects(), count as i64);
                if kind == "tenured" {
                    crate::gc::minor_collect_internal();
                    assert_eq!(willow_gc_moved_objects(), 2 * count as i64);
                }
            }
            if kind == "fragmented" {
                willow_gc_collect();
                assert_eq!(LIVE.load(Ordering::Relaxed), count.div_ceil(2));
            } else {
                assert_eq!(LIVE.load(Ordering::Relaxed), count);
            }
            // Reset also owns objects still present in the runtime root table.
            reset_internal();
            assert_eq!(
                LIVE.load(Ordering::Relaxed),
                0,
                "{kind}: leaked native owner"
            );
            assert_eq!(willow_gc_allocated_bytes(), 0);
            drop(young_slots);
            reset_internal();
            assert_eq!(LIVE.load(Ordering::Relaxed), 0);
            eprintln!("reset storage={kind} owners={count} live_owners=0");
        }
    }
}

#[test]
fn native_drop_panics_terminate_reset_and_collection() {
    use super::failure_policy_tests::{GC_FATAL_CASE, assert_fatal_child};
    if let Ok(case) = std::env::var(GC_FATAL_CASE) {
        unsafe fn panic_drop(_: *mut u8) {
            eprintln!("DROP_HOOK_ENTERED");
            panic!("injected native destructor panic");
        }
        reset_internal();
        willow_register_drop(OWNER_TYPE, panic_drop);
        let mut tls = tlab_state_for_test();
        for _ in 0..4 {
            willow_alloc_with_layout(GcObjectKind::Class, OWNER_TYPE, 8, 0);
            willow_gc_alloc_slow(&mut tls, 1, OWNER_TYPE as i64, 8, 0);
        }
        if case == "reset" {
            reset_internal();
        } else {
            willow_gc_collect();
        }
        eprintln!("GC_PANIC_RETURNED");
        return;
    }
    for case in ["reset", "collection"] {
        let stderr = assert_fatal_child(
            "gc::reset_tests::native_drop_panics_terminate_reset_and_collection",
            case,
            "runtime fatal: Rust panic in GC native destructor",
        );
        assert_eq!(stderr.matches("DROP_HOOK_ENTERED").count(), 1);
        assert!(stderr.contains("injected native destructor panic"));
    }
}
