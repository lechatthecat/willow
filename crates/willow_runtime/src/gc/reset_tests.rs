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
                } else if kind != "fragmented" || index % 2 == 0 {
                    willow_gc_add_runtime_root(payload);
                }
            }
            if matches!(kind, "survivor" | "tenured") {
                willow_gc_minor_collect();
                assert_eq!(willow_gc_moved_objects(), count as i64);
                if kind == "tenured" {
                    willow_gc_minor_collect();
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
            reset_internal();
            assert_eq!(LIVE.load(Ordering::Relaxed), 0);
            eprintln!("reset storage={kind} owners={count} live_owners=0");
        }
    }
}

#[test]
fn reset_contains_native_drop_panics_and_continues_cleanup() {
    static CALLS: AtomicUsize = AtomicUsize::new(0);
    unsafe fn panic_drop(_: *mut u8) {
        CALLS.fetch_add(1, Ordering::Relaxed);
        panic!("reset drop hook test");
    }
    let _guard = runtime_test_guard();
    reset_internal();
    CALLS.store(0, Ordering::Relaxed);
    willow_register_drop(OWNER_TYPE, panic_drop);
    let mut tls = tlab_state_for_test();
    for _ in 0..4 {
        assert!(!willow_alloc_with_layout(GcObjectKind::Class, OWNER_TYPE, 8, 0).is_null());
        assert!(!willow_gc_alloc_slow(&mut tls, 1, OWNER_TYPE as i64, 8, 0).is_null());
    }
    reset_internal();
    assert_eq!(CALLS.load(Ordering::Relaxed), 8);
    reset_internal();
    assert_eq!(CALLS.load(Ordering::Relaxed), 8);
}
