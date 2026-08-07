//! Premises of the concurrent-mark contract (willow-6fv.5.8.1).
//!
//! `requirements/willow_apex_gc_concurrent_mark_contract.md` fixes the first old
//! cycle as **non-moving SATB mark-sweep**, and several of its decisions are only
//! sound because of properties the collector already has today. Those properties
//! are prose in the contract and code here: if one of them regresses, a decision
//! in the contract silently becomes wrong, and the SATB implementation built on
//! top of it (willow-6fv.5.6.2 and later) inherits the hole.
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
static DROP_LOG: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Type id whose drop hook records the reclamation. Distinct from the ids the
/// runtime registers for its own containers.
const CONTRACT_TYPE_ID: i64 = 0x5A_7B_01;

unsafe fn record_drop(payload: *mut u8) {
    DROP_LOG.lock().unwrap().push(payload as usize);
}

fn reset_gc() {
    reset_internal();
    DROP_LOG.lock().unwrap().clear();
}

fn global_gc_guard() -> std::sync::MutexGuard<'static, ()> {
    runtime_test_guard()
}

/// Registers the recording hook. Must run after `reset_gc`, which clears the
/// registry.
fn arm_drop_recorder() {
    willow_register_drop(CONTRACT_TYPE_ID as u32, record_drop);
}

fn drops_of(payload: *mut u8) -> usize {
    DROP_LOG
        .lock()
        .unwrap()
        .iter()
        .filter(|&&addr| addr == payload as usize)
        .count()
}

fn new_tlab_state() -> GcTlabState {
    GcTlabState {
        cursor: AtomicUsize::new(0),
        limit: AtomicUsize::new(0),
        fast_allocations: AtomicU64::new(0),
        fast_allocated_bytes: AtomicU64::new(0),
    }
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
    let _guard = global_gc_guard();
    reset_gc();

    let low = willow_alloc_object(CONTRACT_TYPE_ID, 24);
    let dead = willow_alloc_object(CONTRACT_TYPE_ID, 24);
    let high = willow_alloc_object(CONTRACT_TYPE_ID, 24);
    unsafe {
        *(low as *mut i64) = 0x1111;
        *(high as *mut i64) = 0x3333;
    }
    let (low_before, high_before) = (low, high);
    assert_ne!(dead, low_before);

    let mut low_root = low;
    let mut high_root = high;
    willow_push_root(&mut low_root as *mut *mut u8);
    willow_push_root(&mut high_root as *mut *mut u8);
    willow_gc_collect();

    assert_eq!(
        low_root, low_before,
        "C1: a major collection must not move a surviving old object"
    );
    assert_eq!(
        high_root, high_before,
        "C1: the survivor above the swept hole must not slide down into it"
    );
    assert_eq!(unsafe { *(low_root as *mut i64) }, 0x1111);
    assert_eq!(unsafe { *(high_root as *mut i64) }, 0x3333);

    // A second cycle over the same objects: stability is not a one-shot property.
    willow_gc_collect();
    assert_eq!(low_root, low_before, "C1: stable across repeated cycles");
    assert_eq!(high_root, high_before, "C1: stable across repeated cycles");

    willow_pop_root();
    willow_pop_root();
    reset_gc();
}

// C2. A minor collection moves young objects and promotes survivors into the old
//     generation. It must not move objects that are already old. D3 tells minor
//     GC to remap outstanding mark work; that step is bounded to *young*
//     addresses only because of this.
#[test]
fn contract_c2_old_object_address_survives_minor_collection_and_promotion() {
    let _guard = global_gc_guard();
    reset_gc();

    // gc_ref_mask 0b1: payload word 0 is a GC reference.
    let parent = willow_gc_alloc_layout(0x9001, CONTRACT_TYPE_ID, 8, 0b1);
    let parent_before = parent;

    let mut tls = new_tlab_state();
    let young = willow_gc_alloc_slow(&mut tls, 0x9002, CONTRACT_TYPE_ID, 8, 0);
    unsafe { *(young as *mut i64) = 0x2222 };
    assert_eq!(
        unsafe { (*payload_to_header(young)).generation },
        GC_GENERATION_YOUNG,
        "TLAB allocations start young"
    );

    willow_gc_write_barrier(parent, young, GcStoreDestination::ObjectField as i64);
    unsafe { *(parent as *mut *mut u8) = young };
    let mut parent_root = parent;
    willow_push_root(&mut parent_root as *mut *mut u8);

    willow_gc_minor_collect();

    assert_eq!(
        parent_root, parent_before,
        "C2: promotion must not move the old object that points at the survivor"
    );
    let promoted = unsafe { *(parent_root as *mut *mut u8) };
    assert_ne!(
        promoted, young,
        "the young child is expected to move — that is what makes C2 meaningful"
    );
    assert_eq!(unsafe { *(promoted as *mut i64) }, 0x2222);
    assert_eq!(
        unsafe { (*payload_to_header(promoted)).generation },
        GC_GENERATION_OLD,
        "a minor survivor is promoted"
    );

    // Now that the child is old too, a second minor collection must move neither.
    let promoted_before = promoted;
    willow_gc_minor_collect();
    assert_eq!(parent_root, parent_before, "C2: old parent still stable");
    assert_eq!(
        unsafe { *(parent_root as *mut *mut u8) },
        promoted_before,
        "C2: a promoted object is old and never moves again"
    );

    willow_pop_root();
    reset_gc();
}

// C3. D7 promises exactly one drop-hook invocation per object the sweep actually
//     reclaims. One object dies, one survives, in the same cycle.
#[test]
fn contract_c3_drop_hook_runs_exactly_once_per_reclaimed_object() {
    let _guard = global_gc_guard();
    reset_gc();
    arm_drop_recorder();

    let kept = willow_alloc_object(CONTRACT_TYPE_ID, 16);
    let doomed = willow_alloc_object(CONTRACT_TYPE_ID, 16);
    let mut kept_root = kept;
    willow_push_root(&mut kept_root as *mut *mut u8);

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

    willow_pop_root();
    reset_gc();
}

// C4. The other half of D7: a hook must never run for an object the sweep did
//     not reclaim. A hook that fires early frees a Rust payload out from under a
//     live object.
#[test]
fn contract_c4_drop_hook_does_not_run_for_a_survivor() {
    let _guard = global_gc_guard();
    reset_gc();
    arm_drop_recorder();

    let kept = willow_alloc_object(CONTRACT_TYPE_ID, 16);
    let mut kept_root = kept;
    willow_push_root(&mut kept_root as *mut *mut u8);

    willow_gc_collect();
    willow_gc_collect();
    assert_eq!(
        drops_of(kept),
        0,
        "C4: a rooted object's hook must not run while it is live"
    );

    // Releasing the root makes the next cycle reclaim it — exactly once.
    willow_pop_root();
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
    let _guard = global_gc_guard();
    reset_gc();
    arm_drop_recorder();

    // An anchor keeps the region alive so later cycles still walk this storage.
    let anchor = willow_alloc_object(CONTRACT_TYPE_ID, 16);
    let mut anchor_root = anchor;
    willow_push_root(&mut anchor_root as *mut *mut u8);
    let doomed = willow_alloc_object(CONTRACT_TYPE_ID, 16);

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

    willow_pop_root();
    reset_gc();
}

// C7. `OldRegion::mark_bitmap` is the allocator's live object-start index: set at
//     allocation, cleared on release, rebuilt by sweep. It is NOT a reachability
//     bitmap, so D5 gives concurrent marking a separate one. If this test ever
//     starts failing because a fresh allocation is unmarked, the two roles have
//     been merged and D5 needs rewriting before the mark engine lands.
#[test]
fn contract_c7_region_bitmap_tracks_object_starts_not_reachability() {
    let _guard = global_gc_guard();
    reset_gc();

    let anchor = willow_alloc_object(CONTRACT_TYPE_ID, 16);
    let mut anchor_root = anchor;
    willow_push_root(&mut anchor_root as *mut *mut u8);
    let doomed = willow_alloc_object(CONTRACT_TYPE_ID, 16);

    assert!(
        region_start_bit(doomed),
        "C7: allocation sets the object-start bit, before anything has traced it"
    );
    assert!(region_start_bit(anchor), "C7: the anchor is recorded too");
    assert!(
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

    willow_pop_root();
    reset_gc();
}
