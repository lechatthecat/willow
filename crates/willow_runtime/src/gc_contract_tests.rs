//! Premises of the concurrent-mark contract.
//!
//! `requirements/willow_apex_gc_concurrent_mark_contract.md` fixes the first old
//! cycle as **non-moving SATB mark-sweep**, and several of its decisions are only
//! sound because of properties the collector already has today. Those properties
//! are prose in the contract and code here: if one of them regresses, a decision
//! in the contract silently becomes wrong, and the SATB implementation built on
//! top of it (concurrent collection and later) inherits the hole.
//!
//! Each test names the contract clause it pins (`C1`..`C7` of §13). These are
//! *premises*, not the concurrent collector — nothing here marks concurrently.
//!
//! | Clause | Premise | Decision that needs it |
//! |---|---|---|
//! | C1 | an old object's address survives a major collection | D1: no relocation, so mark work can hold raw addresses |
//! | C2 | an old object's address survives a minor collection and promotion | D1/D3: only young addresses need the D3 remap step |
//! | C3 | a reclaimed object's drop hook runs exactly once | D7 |
//! | C4 | a surviving object's drop hook does not run | D7 |
//! | C5 | a later cycle does not re-run a hook already run | D7 |
//! | C7 | `OldRegion::mark_bitmap` is a live object-start map, not reachability | D5: concurrent mark needs its own bitmap |

use super::*;
use std::sync::Mutex;

/// Payload addresses passed to the test drop hook, in call order. The hook runs
/// with the heap lock held, so this must stay a leaf lock: never touch GC state
/// from inside it.
static DROP_LOG: crate::runtime::PerRuntime<Mutex<Vec<usize>>> =
    crate::runtime::PerRuntime::new(|| Mutex::new(Vec::new()));

/// Type id whose drop hook records the reclamation. Distinct from the ids the
/// runtime registers for its own containers.
const CONTRACT_TYPE_ID: i64 = 0x5A_7B_01;

unsafe fn record_drop(payload: *mut u8) {
    DROP_LOG.lock().unwrap().push(payload as usize);
}

fn reset_gc() {
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal() };
    DROP_LOG.lock().unwrap().clear();
}

/// Registers the recording hook. Must run after `reset_gc`, which clears the
/// registry.
fn arm_drop_recorder() {
    // SAFETY: This fixture reserves the type ID for the payload consumed by this finalizer
    // and initializes each instance before collection.
    unsafe { willow_register_drop(CONTRACT_TYPE_ID as u32, record_drop) };
}

fn drops_of(payload: *mut u8) -> usize {
    DROP_LOG
        .lock()
        .unwrap()
        .iter()
        .filter(|&&addr| addr == payload as usize)
        .count()
}

fn new_tlab_state() -> TestTlab {
    tlab_state_for_test()
}

/// Offset of `payload`'s header within its old region, plus whether that region
/// currently records it as a live object start.
fn region_start_bit(payload: *mut u8) -> bool {
    let state = runtime().heap.lock().unwrap();
    let header = payload_to_header(payload) as usize;
    let region = state
        .old_regions
        .iter()
        .find(|region| region.contains(header))
        .expect("an old-generation object belongs to a region");
    region.mark_bitmap.is_marked(header - region.start())
}

// C1. A major collection must not move a surviving old object. The whole
//     first-slice design rests on this: mark work items, SATB buffer entries,
//     and remembered-set owners are all raw payload addresses, and none of them
//     is remapped for old objects. Sweeping a dead neighbour out of the middle
//     of the region is the interesting case — a compacting sweep would slide the
//     survivor down into the hole.
#[test]
fn contract_c1_old_object_address_survives_a_major_collection() {
    let _guard = isolated_runtime();
    reset_gc();

    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let low = unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 24) };
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let dead = unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 24) };
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let high = unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 24) };
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    unsafe {
        *(low as *mut i64) = 0x1111;
        *(high as *mut i64) = 0x3333;
    }
    let (low_before, high_before) = (low, high);
    assert_ne!(dead, low_before);

    let mut low_root = low;
    let mut high_root = high;
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe {
        willow_push_root(&raw mut low_root);
        willow_push_root(&raw mut high_root);
    }
    willow_gc_collect();

    assert_eq!(
        low_root, low_before,
        "C1: a major collection must not move a surviving old object"
    );
    assert_eq!(
        high_root, high_before,
        "C1: the survivor above the swept hole must not slide down into it"
    );
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    assert_eq!(unsafe { *(low_root as *mut i64) }, 0x1111);
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    assert_eq!(unsafe { *(high_root as *mut i64) }, 0x3333);

    // A second cycle over the same objects: stability is not a one-shot property.
    willow_gc_collect();
    assert_eq!(low_root, low_before, "C1: stable across repeated cycles");
    assert_eq!(high_root, high_before, "C1: stable across repeated cycles");

    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe {
        willow_pop_root();
        willow_pop_root();
    }
    reset_gc();
}

// C2. A minor collection moves young objects and promotes survivors into the old
//     generation. It must not move objects that are already old. D3 tells minor
//     GC to remap outstanding mark work; that step is bounded to *young*
//     addresses only because of this.
#[test]
fn contract_c2_old_object_address_survives_minor_collection_and_promotion() {
    let _guard = isolated_runtime();
    reset_gc();

    // gc_ref_mask 0b1: payload word 0 is a GC reference.
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let parent = unsafe { willow_gc_alloc_layout(0x9001, CONTRACT_TYPE_ID, 8, 0b1) };
    let parent_before = parent;

    let tls = new_tlab_state();
    // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size and
    // tracing metadata describe the object initialized here.
    let young = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 0x9002, CONTRACT_TYPE_ID, 8, 0) };
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    unsafe { *(young as *mut i64) = 0x2222 };
    assert_eq!(
        // SAFETY: The current GC payload follows its aligned header; its owner keeps it live here.
        unsafe { (*payload_to_header(young)).generation },
        GC_GENERATION_YOUNG,
        "TLAB allocations start young"
    );

    // SAFETY: The destination describes this reference update. Any asserted header belongs to
    // its live GC payload.
    unsafe {
        willow_gc_write_barrier(
            parent,
            std::ptr::null_mut(),
            young,
            GcStoreDestination::ObjectField as i64,
        )
    };
    // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the accessed
    // words.
    unsafe { *(parent as *mut *mut u8) = young };
    let mut parent_root = parent;
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut parent_root) };

    crate::gc::minor_collect_internal();

    assert_eq!(
        parent_root, parent_before,
        "C2: promotion must not move the old object that points at the survivor"
    );
    // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the accessed
    // words.
    let survivor = unsafe { *(parent_root as *mut *mut u8) };
    // SAFETY: The current GC payload follows its aligned header; its owner keeps it live here.
    assert_eq!(unsafe { (*payload_to_header(survivor)).age }, 1);
    assert_eq!(
        // SAFETY: The current GC payload follows its aligned header; its owner keeps it live here.
        unsafe { (*payload_to_header(survivor)).generation },
        GC_GENERATION_YOUNG
    );
    crate::gc::minor_collect_internal();
    assert_eq!(parent_root, parent_before);
    // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the accessed
    // words.
    let promoted = unsafe { *(parent_root as *mut *mut u8) };
    assert_ne!(
        promoted, young,
        "the young child is expected to move — that is what makes C2 meaningful"
    );
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    assert_eq!(unsafe { *(promoted as *mut i64) }, 0x2222);
    assert_eq!(
        // SAFETY: The current GC payload follows its aligned header; its owner keeps it live here.
        unsafe { (*payload_to_header(promoted)).generation },
        GC_GENERATION_OLD,
        "a second-minor survivor is promoted"
    );

    // Now that the child is old too, a third minor collection must move neither.
    let promoted_before = promoted;
    crate::gc::minor_collect_internal();
    assert_eq!(parent_root, parent_before, "C2: old parent still stable");
    assert_eq!(
        // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the accessed
        // words.
        unsafe { *(parent_root as *mut *mut u8) },
        promoted_before,
        "C2: a promoted object is old and never moves again"
    );

    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_root() };
    reset_gc();
}

// C3. D7 promises exactly one drop-hook invocation per object the sweep actually
//     reclaims. One object dies, one survives, in the same cycle.
#[test]
fn contract_c3_drop_hook_runs_exactly_once_per_reclaimed_object() {
    let _guard = isolated_runtime();
    reset_gc();
    arm_drop_recorder();

    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let kept = unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 16) };
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let doomed = unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 16) };
    let mut kept_root = kept;
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut kept_root) };

    willow_gc_collect();

    assert_eq!(
        drops_of(doomed),
        1,
        "C3: the reclaimed object's hook runs exactly once"
    );
    assert_eq!(
        DROP_LOG.lock().unwrap().len(),
        1,
        "C3: no hook ran for any other object in this cycle"
    );

    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_root() };
    reset_gc();
}

// C4. The other half of D7: a hook must never run for an object the sweep did
//     not reclaim. A hook that fires early frees a Rust payload out from under a
//     live object.
#[test]
fn contract_c4_drop_hook_does_not_run_for_a_survivor() {
    let _guard = isolated_runtime();
    reset_gc();
    arm_drop_recorder();

    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let kept = unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 16) };
    let mut kept_root = kept;
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut kept_root) };

    willow_gc_collect();
    willow_gc_collect();
    assert_eq!(
        drops_of(kept),
        0,
        "C4: a rooted object's hook must not run while it is live"
    );

    // Releasing the root makes the next cycle reclaim it — exactly once.
    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_root() };
    willow_gc_collect();
    assert_eq!(
        drops_of(kept),
        1,
        "C4: the hook runs on the cycle that actually reclaims the object"
    );

    reset_gc();
}

// C5. Reclamation is not repeatable. Once an object is swept, later cycles must
//     not find its header again and run the hook a second time — a double free
//     of whatever Rust payload the hook owns.
#[test]
fn contract_c5_drop_hook_is_not_repeated_by_later_cycles() {
    let _guard = isolated_runtime();
    reset_gc();
    arm_drop_recorder();

    // An anchor keeps the region alive so later cycles still walk this storage.
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let anchor = unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 16) };
    let mut anchor_root = anchor;
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut anchor_root) };
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let doomed = unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 16) };

    willow_gc_collect();
    assert_eq!(drops_of(doomed), 1, "reclaimed on the first cycle");

    for _ in 0..3 {
        willow_gc_collect();
    }
    assert_eq!(
        drops_of(doomed),
        1,
        "C5: repeated cycles must not run a hook for already-reclaimed storage"
    );
    assert_eq!(
        drops_of(anchor),
        0,
        "C5: the anchor is still rooted and must not be finalized"
    );

    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_root() };
    reset_gc();
}

// C7. `OldRegion::mark_bitmap` is the allocator's live object-start index: set at
//     allocation, cleared on release, rebuilt by sweep. It is NOT a reachability
//     bitmap, so D5 gives concurrent marking a separate one. If this test ever
//     starts failing because a fresh allocation is unmarked, the two roles have
//     been merged and D5 needs rewriting before the mark engine lands.
#[test]
fn contract_c7_region_bitmap_tracks_object_starts_not_reachability() {
    let _guard = isolated_runtime();
    reset_gc();

    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let anchor = unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 16) };
    let mut anchor_root = anchor;
    // SAFETY: The initialized slot stays at this address until its registration is popped;
    // collection may rewrite it there.
    unsafe { willow_push_root(&raw mut anchor_root) };
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    let doomed = unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 16) };

    assert!(
        region_start_bit(doomed),
        "C7: allocation sets the object-start bit, before anything has traced it"
    );
    assert!(region_start_bit(anchor), "C7: the anchor is recorded too");
    assert!(
        // SAFETY: The current GC payload follows its aligned header; its owner keeps it live here.
        !unsafe { (*payload_to_header(doomed)).marked },
        "C7: reachability lives in the header mark bit, which is still clear"
    );

    let doomed_header = payload_to_header(doomed) as usize;
    willow_gc_collect();

    let anchor_header = payload_to_header(anchor_root) as usize;
    let state = runtime().heap.lock().unwrap();
    let region = state
        .old_regions
        .iter()
        .find(|region| region.contains(anchor_header))
        .expect("the anchor keeps its region alive");
    assert!(
        region.mark_bitmap.is_marked(anchor_header - region.start()),
        "C7: sweep re-records surviving object starts"
    );
    assert!(
        !region.mark_bitmap.is_marked(doomed_header - region.start()),
        "C7: releasing an object clears its start bit"
    );
    drop(state);

    // SAFETY: The removed shadow-stack entries belong to this operation and their slot
    // storage is still live.
    unsafe { willow_pop_root() };
    reset_gc();
}

unsafe fn record_then_panic_drop(payload: *mut u8) {
    DROP_LOG.lock().unwrap().push(payload as usize);
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    if unsafe { *(payload as *const u64) } == 1 {
        panic!("injected native drop failure");
    }
}

#[test]
fn panicking_drop_terminates_every_storage_kind() {
    use super::failure_policy_tests::{GC_FATAL_CASE, assert_fatal_child};
    if let Ok(shape) = std::env::var(GC_FATAL_CASE) {
        reset_gc();
        // SAFETY: This fixture reserves the type ID for the payload consumed by this
        // finalizer and initializes each instance before collection.
        unsafe { willow_register_drop(CONTRACT_TYPE_ID as u32, record_then_panic_drop) };
        let tls = new_tlab_state();
        let allocate = || match shape.as_str() {
            "large" => {
                // SAFETY: The requested layout describes the initialized words of this object
                // and its type’s tracing or finalization contract.
                unsafe {
                    willow_alloc_object(CONTRACT_TYPE_ID, (GC_LARGE_OBJECT_THRESHOLD + 8) as i64)
                }
            }
            "tlab_major" | "tlab_minor" => {
                // SAFETY: The fixture owns the stable TLAB through unregistration. The
                // requested size and tracing metadata describe the object initialized here.
                unsafe { willow_gc_alloc_slow(tls.as_ptr(), 0, CONTRACT_TYPE_ID, 8, 0) }
            }
            // SAFETY: The requested layout describes the initialized words of this object and
            // its type’s tracing or finalization contract.
            _ => unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 8) },
        };
        let dead = allocate();
        let _healthy_dead = allocate();
        let mut survivor = allocate();
        // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
        unsafe { *(dead as *mut u64) = 1 };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut survivor) };
        if shape == "tlab_minor" {
            crate::gc::minor_collect_pinning_for_test();
        } else {
            willow_gc_collect();
        }
        eprintln!("GC_PANIC_RETURNED");
        return;
    }
    for shape in ["old", "large", "tlab_major", "tlab_minor"] {
        let stderr = assert_fatal_child(
            "gc::contract_viewpoint_tests::panicking_drop_terminates_every_storage_kind",
            shape,
            "runtime fatal: Rust panic in GC native destructor",
        );
        assert!(stderr.contains("injected native drop failure"));
    }
}

#[test]
fn panicking_payload_is_not_dropped_after_native_hook_panic() {
    use super::failure_policy_tests::{GC_FATAL_CASE, assert_fatal_child};
    if std::env::var_os(GC_FATAL_CASE).is_some() {
        struct BadPayload;
        impl Drop for BadPayload {
            fn drop(&mut self) {
                panic!("GC_PAYLOAD_DROP_RAN");
            }
        }
        unsafe fn hook(_: *mut u8) {
            std::panic::panic_any(BadPayload);
        }
        // SAFETY: The registered hook matches the allocation's type and runs while the collector
        // retains its payload.
        unsafe {
            run_drop_hook(
                runtime(),
                hook,
                std::ptr::null_mut(),
                crate::observation::Value::new(|| 0),
            )
        };
        eprintln!("GC_PANIC_RETURNED");
        return;
    }
    let stderr = assert_fatal_child(
        "gc::contract_viewpoint_tests::panicking_payload_is_not_dropped_after_native_hook_panic",
        "payload",
        "runtime fatal: Rust panic in GC native destructor",
    );
    assert!(!stderr.contains("GC_PAYLOAD_DROP_RAN"));
}

static DROP_WITHOUT_HEAP_LOCK: crate::runtime::PerRuntime<std::sync::atomic::AtomicBool> =
    crate::runtime::PerRuntime::new(|| std::sync::atomic::AtomicBool::new(false));

unsafe fn verify_concurrent_drop_context(_: *mut u8) {
    assert!(!runtime().stop_requested.load(Ordering::Acquire));
    let _heap = runtime()
        .heap
        .try_lock()
        .expect("old drop held the heap mutex");
    DROP_WITHOUT_HEAP_LOCK.store(true, Ordering::Release);
}

#[test]
fn normal_old_drop_runs_after_world_resume_without_heap_mutex() {
    let _guard = isolated_runtime();
    reset_gc();
    DROP_WITHOUT_HEAP_LOCK.store(false, Ordering::Relaxed);
    // SAFETY: This fixture reserves the type ID for the payload consumed by this finalizer
    // and initializes each instance before collection.
    unsafe { willow_register_drop(CONTRACT_TYPE_ID as u32, verify_concurrent_drop_context) };
    // SAFETY: The requested layout describes the initialized words of this object and its
    // type’s tracing or finalization contract.
    unsafe { willow_alloc_object(CONTRACT_TYPE_ID, 8) };
    let before = crate::gc_telemetry::workers::snapshot().drop_hook_panics;
    willow_gc_collect();
    assert!(DROP_WITHOUT_HEAP_LOCK.load(Ordering::Acquire));
    assert_eq!(
        crate::gc_telemetry::workers::snapshot().drop_hook_panics,
        before
    );
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_gc();
}
