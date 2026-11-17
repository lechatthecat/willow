//! Task-owned young regions and publication: per-point publication and its
//! negative tests, in-place promotion, republish cost, owner segments, the
//! verifier and parking mid-way at a requested stop.

use super::*;
use crate::runtime::Runtime;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicPtr};

const TASK_A: u64 = 41;
const TASK_B: u64 = 42;

/// Words of a test node; word 0..`words` may hold references per `mask`.
fn layout(words: usize, mask: u64) -> u64 {
    willow_abi::gc_layout_id(GcObjectKind::Class, (words * 8) as i64, 0, mask)
}

fn object(payload: *mut u8) -> HeapObject {
    HeapObject::from_payload(GcPayload::from_raw(payload).expect("non-null payload"))
}

/// One young object in this thread's TLAB with `words` zeroed words, the
/// first `refs` of them references. Mirrors the generated bump path, falling
/// back to the slow path when the chunk is full.
fn young(refs: usize, words: usize) -> *mut u8 {
    let mask = if refs == 64 {
        u64::MAX
    } else {
        (1u64 << refs) - 1
    };
    let layout_id = layout(words, mask);
    let bytes = GC_HEADER_SIZE + words * 8;
    let tls = runtime_tlab::willow_gc_current_tlab();
    // SAFETY: this thread's TLS allocation block.
    let state = unsafe { &*tls };
    let cursor = state.cursor.load(Ordering::Acquire);
    if cursor != 0 && cursor + bytes <= state.limit.load(Ordering::Acquire) {
        initialize_generated_object_at_for_test(cursor as *mut u8, bytes, 0, layout_id, mask)
            .unwrap();
        publish_tlab_start_for_test(state, cursor as *mut u8);
        state.cursor.store(cursor + bytes, Ordering::Release);
        let payload = (cursor + GC_HEADER_SIZE) as *mut u8;
        // SAFETY: the payload was just bump-allocated by this thread.
        unsafe { std::ptr::write_bytes(payload, 0, words * 8) };
        return payload;
    }
    let payload = willow_gc_alloc_slow(tls, layout_id, 0, (words * 8) as i64, mask);
    assert!(!payload.is_null());
    payload
}

/// A private (unshared) old object with `refs` reference words.
fn old(refs: usize) -> *mut u8 {
    let mask = (1u64 << refs) - 1;
    let payload = allocate_old(layout(refs.max(1), mask), 0, (refs.max(1) * 8) as i64, mask);
    assert!(!payload.is_null());
    payload
}

fn set(owner: *mut u8, index: usize, value: *mut u8) {
    // SAFETY: tests bound `index` by the allocation's reference words.
    unsafe { owner.cast::<*mut u8>().add(index).write(value) };
}

fn get(owner: *mut u8, index: usize) -> *mut u8 {
    // SAFETY: as in `set`.
    unsafe { owner.cast::<*mut u8>().add(index).read() }
}

fn shared(payload: *mut u8) -> bool {
    object(payload).shared()
}

fn is_old(payload: *mut u8) -> bool {
    object(payload).generation() == GC_GENERATION_OLD
}

fn run_as(owner: &Runtime, task: u64) {
    switch_task_owner_in(owner, task_owner(task));
}

/// Apply this thread's pending owner/promotion entries and run the
/// publication verifier exactly as a minor stop does.
fn verify_now(owner: &Runtime) -> Result<(), String> {
    let snapshot = registry_snapshot(owner);
    let mut state = owner.gc.heap.lock().unwrap();
    if let Some(log) = mutator_log::bound_in(&owner.gc) {
        // SAFETY: this thread owns its bound log.
        unsafe { log.publication() }.apply(&mut state, &owner.gc.publication);
    }
    // As at a minor stop: active cursors are sealed (start bits published).
    super::tlab::retire_all_tlabs_unindexed_in(owner, &mut state);
    let handed_over = owner
        .gc
        .publication
        .handed_over_owners
        .lock()
        .unwrap()
        .clone();
    validation::verify_publication(
        &state,
        &snapshot.trace,
        |address| owner.gc.publication.parked_contains(address),
        &handed_over,
    )
}

/// Fixture: a fresh heap, this thread registered and running task A.
struct Fixture {
    owner: &'static Runtime,
    /// Set when the runtime is reference counted (owner-held roots).
    arc: Option<&'static std::sync::Arc<Runtime>>,
}

impl Fixture {
    fn new() -> Self {
        Self::start(isolated_runtime().runtime, None)
    }

    /// A reference-counted runtime this thread serves (leaked: a selected
    /// runtime must outlive the thread).
    fn owned() -> &'static std::sync::Arc<Runtime> {
        let arc = Box::leak(Box::new(std::sync::Arc::new(Runtime::new(
            crate::scheduler::environment_worker_config,
        ))));
        let runtime: &'static Runtime = arc;
        runtime.select_for_thread();
        arc
    }

    fn on(arc: &'static std::sync::Arc<Runtime>) -> Self {
        Self::start(arc, Some(arc))
    }

    fn start(owner: &'static Runtime, arc: Option<&'static std::sync::Arc<Runtime>>) -> Self {
        reset_internal();
        willow_gc_register_mutator();
        run_as(owner, TASK_A);
        Self { owner, arc }
    }

    fn stats(&self) -> PublicationStats {
        stats_in(self.owner)
    }

    fn skip(&self, point: PublicationPoint, skipped: bool) {
        self.owner
            .gc
            .publication
            .set_point_skipped_for_test(point, skipped);
    }

    fn verifier_on(&self, on: bool) {
        *self.owner.diagnostics.stress_override.lock().unwrap() =
            on.then(|| vec!["scheduler".into()]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for point in PublicationPoint::ALL {
            self.skip(point, false);
        }
        *self.owner.diagnostics.stress_override.lock().unwrap() = None;
        switch_task_owner_in(self.owner, 0);
        willow_gc_unregister_mutator();
        reset_internal();
    }
}

/// Task B keeps `value` in its own young object, as a receiver would.
fn read_from_task_b(fixture: &Fixture, value: *mut u8) -> *mut u8 {
    run_as(fixture.owner, TASK_B);
    let holder = young(1, 1);
    set(holder, 0, value);
    run_as(fixture.owner, TASK_A);
    holder
}

static GLOBAL_CELL: AtomicPtr<u8> = AtomicPtr::new(std::ptr::null_mut());

unsafe extern "C" fn never_polled(_: *mut c_void) -> i32 {
    0
}

/// Drive `value` through the real call path of `point`.
fn exercise(fixture: &Fixture, point: PublicationPoint, value: *mut u8) {
    let owner = fixture.owner;
    match point {
        PublicationPoint::ChannelSend => {
            let channel = crate::channel::willow_channel_new_bounded(1, 4);
            assert_eq!(
                crate::channel::willow_channel_try_send_ptr(channel, value.cast()),
                1,
                "sent"
            );
        }
        PublicationPoint::Spawn => {
            crate::scheduler::reset_global_scheduler_for_test();
            let frame = crate::async_frame::willow_async_frame_alloc(1, 1).cast::<u8>();
            let offset = crate::async_frame::async_frame_slot_offset(0);
            // SAFETY: slot 0 of a one-slot frame.
            unsafe { frame.add(offset).cast::<*mut u8>().write(value) };
            crate::scheduler::willow_sched_spawn(never_polled, frame.cast());
            assert!(!shared(frame), "a spawned frame stays private");
        }
        PublicationPoint::TaskReturn => {
            let frame = crate::async_frame::willow_async_frame_alloc(2, 0b11).cast::<u8>();
            for slot in 0..2 {
                let offset = crate::async_frame::async_frame_slot_offset(slot);
                // SAFETY: both slots of a two-slot frame.
                unsafe { frame.add(offset).cast::<*mut u8>().write(value) };
            }
            // The handle escaped (sent to another task) before the task
            // finished, so its result is published, not handed over.
            assert!(!crate::async_frame::frame_mark_result_escaped(frame.cast()));
            finish_task_result_in(owner, TASK_A, frame);
            assert!(!shared(frame));
        }
        PublicationPoint::LockedState => {
            let cell = old(1);
            barrier::write_barrier_in(
                owner,
                cell,
                cell,
                std::ptr::null_mut(),
                value,
                GcStoreDestination::AsyncMutexCell as i64,
            );
            set(cell, 0, value);
        }
        PublicationPoint::Global => {
            barrier::write_barrier_in(
                owner,
                GLOBAL_CELL.as_ptr().cast(),
                GLOBAL_CELL.as_ptr().cast(),
                std::ptr::null_mut(),
                value,
                GcStoreDestination::GlobalStatic as i64,
            );
            GLOBAL_CELL.store(value, Ordering::Release);
        }
        PublicationPoint::RuntimeRoot => {
            // An owner-held root, readable by threads outside the tasks.
            let arc = fixture.arc.expect("runtime roots need an owned runtime");
            let handle =
                root_arena::GcRootHandle::new_in(std::sync::Arc::clone(arc), std::ptr::null_mut());
            handle.store(value);
            assert_eq!(handle.load(), value);
            std::mem::forget(handle);
        }
        PublicationPoint::NativeBoundary => {
            let lease = root_arena::PinLease::<root_arena::ForeignReference>::new(value);
            assert_eq!(lease.load(), value);
            drop(lease);
        }
        PublicationPoint::SharedStore => {
            let target = old(1);
            publish_in(owner, target, PublicationPoint::Global, false);
            barrier::write_barrier_in(
                owner,
                target,
                target,
                std::ptr::null_mut(),
                value,
                GcStoreDestination::ObjectField as i64 | willow_abi::GC_STORE_OWNER_HAS_HEADER,
            );
            set(target, 0, value);
        }
    }
}

#[test]
fn every_publication_point_shares_its_value_and_the_verifier_accepts_it() {
    let arc = Fixture::owned();
    for point in PublicationPoint::ALL {
        let fixture = Fixture::on(arc);
        let child = young(0, 1);
        let value = young(1, 1);
        set(value, 0, child);
        let before = fixture.stats();
        exercise(&fixture, point, value);
        assert!(shared(value) && shared(child), "{point:?}");
        assert!(
            !is_old(value) && !is_old(child),
            "{point:?}: published young objects stay young until a minor"
        );
        let after = fixture.stats();
        assert!(
            after.per_point[point as usize] > before.per_point[point as usize],
            "{point:?}"
        );
        read_from_task_b(&fixture, value);
        verify_now(fixture.owner).unwrap_or_else(|error| panic!("{point:?}: {error}"));
    }
}

#[test]
fn every_skipped_publication_point_is_caught_by_the_verifier() {
    let arc = Fixture::owned();
    for point in PublicationPoint::ALL {
        let fixture = Fixture::on(arc);
        // Owner tracking (task attribution of young chunks) is recorded only
        // while the verifier is on.
        fixture.verifier_on(true);
        fixture.skip(point, true);
        let value = young(0, 1);
        exercise(&fixture, point, value);
        assert!(!shared(value), "{point:?} was skipped");
        read_from_task_b(&fixture, value);
        let error = verify_now(fixture.owner).expect_err(&format!("{point:?}"));
        assert!(
            error.contains("of task owner") || error.contains("references unshared"),
            "{point:?}: {error}"
        );
    }
}

#[test]
fn skipped_shared_store_leaves_a_shared_to_unshared_edge() {
    let fixture = Fixture::new();
    fixture.skip(PublicationPoint::SharedStore, true);
    let value = young(0, 1);
    exercise(&fixture, PublicationPoint::SharedStore, value);
    let error = verify_now(fixture.owner).unwrap_err();
    assert!(error.contains("references unshared"), "{error}");
}

#[test]
fn publication_shares_young_objects_in_place_and_counts_bytes() {
    let fixture = Fixture::new();
    let a = young(1, 2);
    let b = young(1, 1);
    let c = young(0, 3);
    set(a, 0, b);
    set(b, 0, c);
    let before = fixture.stats();
    publish_in(fixture.owner, a, PublicationPoint::ChannelSend, false);
    let after = fixture.stats();
    for value in [a, b, c] {
        assert!(shared(value) && !is_old(value));
        assert!(object(value).promoted_by_publication());
    }
    assert_eq!(get(a, 0), b, "publication moves nothing");
    assert_eq!(get(b, 0), c);
    assert_eq!(after.calls - before.calls, 1);
    assert_eq!(after.marked_objects - before.marked_objects, 3);
    assert_eq!(after.promoted_objects - before.promoted_objects, 3);
    assert_eq!(
        after.promoted_bytes - before.promoted_bytes,
        (3 * GC_HEADER_SIZE + 6 * 8) as u64
    );
}

#[test]
fn republishing_is_constant_time_and_scans_nothing() {
    let fixture = Fixture::new();
    let mut head = std::ptr::null_mut();
    for _ in 0..64 {
        let node = young(1, 1);
        set(node, 0, head);
        head = node;
    }
    publish_in(fixture.owner, head, PublicationPoint::ChannelSend, false);
    let first = fixture.stats();
    for point in PublicationPoint::ALL {
        publish_in(fixture.owner, head, point, false);
    }
    let again = fixture.stats();
    assert_eq!(again.scanned_slots, first.scanned_slots);
    assert_eq!(again.marked_objects, first.marked_objects);
    assert_eq!(again.calls, first.calls);
    assert_eq!(again.republished - first.republished, 8);
}

#[test]
fn publication_is_linear_in_newly_marked_objects() {
    let fixture = Fixture::new();
    // A shared prefix is not rescanned when a longer list reaching it is
    // published later.
    let mut head = std::ptr::null_mut();
    for _ in 0..100 {
        let node = young(1, 1);
        set(node, 0, head);
        head = node;
    }
    publish_in(fixture.owner, head, PublicationPoint::Spawn, false);
    let before = fixture.stats();
    let mut longer = head;
    for _ in 0..10 {
        let node = young(1, 1);
        set(node, 0, longer);
        longer = node;
    }
    publish_in(fixture.owner, longer, PublicationPoint::Spawn, false);
    let after = fixture.stats();
    assert_eq!(after.marked_objects - before.marked_objects, 10);
    assert_eq!(after.scanned_slots - before.scanned_slots, 10);
}

#[test]
fn first_publication_of_a_private_old_object_marks_its_successors() {
    let fixture = Fixture::new();
    let container = old(2);
    let young_child = young(0, 1);
    let old_child = old(0);
    // A young value stored into a private old object: logged, not promoted.
    barrier::write_barrier_in(
        fixture.owner,
        container,
        container,
        std::ptr::null_mut(),
        young_child,
        GcStoreDestination::ObjectField as i64 | willow_abi::GC_STORE_OWNER_HAS_HEADER,
    );
    set(container, 0, young_child);
    set(container, 1, old_child);
    assert!(!shared(young_child) && !is_old(young_child));
    assert!(object(container).remembered(), "remembered, not promoted");
    publish_in(fixture.owner, container, PublicationPoint::Global, false);
    for value in [container, young_child, old_child] {
        assert!(shared(value));
    }
    assert!(is_old(container) && is_old(old_child) && !is_old(young_child));
    assert!(!object(container).promoted_by_publication());
    assert!(object(young_child).promoted_by_publication());
    verify_now(fixture.owner).unwrap();
}

#[test]
fn private_long_lived_container_promotes_nothing() {
    let fixture = Fixture::new();
    fixture.verifier_on(true);
    let container = old(8);
    let before = fixture.stats();
    for round in 0..2000 {
        let value = young(0, 1);
        barrier::write_barrier_in(
            fixture.owner,
            container,
            get(container, 0).cast(),
            get(container, round % 8),
            value,
            GcStoreDestination::ObjectField as i64 | willow_abi::GC_STORE_OWNER_HAS_HEADER,
        );
        set(container, round % 8, value);
        if round % 500 == 499 {
            let mut root = container;
            willow_push_root(&mut root);
            willow_gc_minor_collect();
            willow_pop_root();
        }
    }
    let after = fixture.stats();
    assert_eq!(after.promoted_bytes, before.promoted_bytes);
    assert_eq!(after.calls, before.calls);
    assert!(!shared(container));
}

#[test]
fn async_frames_are_never_shared_but_inline_type_zero_objects_are() {
    let fixture = Fixture::new();
    let frame = crate::async_frame::willow_async_frame_alloc(1, 1).cast::<u8>();
    assert!(publish::is_frame(object(frame)));
    publish_in(fixture.owner, frame, PublicationPoint::ChannelSend, false);
    assert!(!shared(frame));
    // Generated inline-traced objects also use type id zero.
    let inline = young(1, 1);
    assert_eq!(object(inline).type_id(), 0);
    assert!(!publish::is_frame(object(inline)));
    let holder = young(1, 1);
    set(holder, 0, frame);
    set(inline, 0, holder);
    publish_in(fixture.owner, inline, PublicationPoint::ChannelSend, false);
    assert!(shared(inline) && shared(holder));
    assert!(
        !shared(frame),
        "a frame reached through a value stays private"
    );
}

#[test]
fn spawn_publishes_every_frame_slot_and_task_return_only_slot_zero() {
    let fixture = Fixture::new();
    let frame = crate::async_frame::willow_async_frame_alloc(2, 0b11).cast::<u8>();
    let first = young(0, 1);
    let second = young(0, 1);
    let write = |slot: usize, value: *mut u8| {
        let offset = crate::async_frame::async_frame_slot_offset(slot);
        // SAFETY: both slots of a two-slot frame.
        unsafe { frame.add(offset).cast::<*mut u8>().write(value) };
    };
    write(0, first);
    write(1, second);
    publish_task_result_in(fixture.owner, frame);
    assert!(shared(first) && !shared(second));
    publish_frame_slots_in(fixture.owner, frame, None, PublicationPoint::Spawn, true);
    assert!(shared(second));
    assert!(!shared(frame));
}

#[test]
fn store_into_shared_object_through_indirect_reference_publishes() {
    let fixture = Fixture::new();
    let target = old(1);
    publish_in(fixture.owner, target, PublicationPoint::Global, false);
    let value = young(0, 1);
    barrier::write_barrier_in(
        fixture.owner,
        target,
        target,
        std::ptr::null_mut(),
        value,
        GcStoreDestination::IndirectReference as i64,
    );
    assert!(shared(value));
    // Without a slot the owner is not a proven payload start: no publication.
    let private = young(0, 1);
    barrier::write_barrier_in(
        fixture.owner,
        target,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        private,
        GcStoreDestination::IndirectReference as i64,
    );
    assert!(!shared(private));
}

#[test]
fn every_lock_cell_destination_publishes() {
    for destination in [
        GcStoreDestination::AsyncMutexCell,
        GcStoreDestination::AsyncRwLockCell,
        GcStoreDestination::BlockingCell,
        GcStoreDestination::BlockingRwCell,
    ] {
        let fixture = Fixture::new();
        let cell = old(1);
        let value = young(0, 1);
        barrier::write_barrier_in(
            fixture.owner,
            cell,
            cell,
            std::ptr::null_mut(),
            value,
            destination as i64,
        );
        assert!(shared(value), "{destination:?}");
        assert!(
            !shared(cell),
            "{destination:?}: the cell itself needs no mark"
        );
    }
}

#[test]
fn stores_into_private_or_young_owners_never_publish() {
    let fixture = Fixture::new();
    let before = fixture.stats().calls;
    let private_old = old(1);
    let young_owner = young(1, 1);
    for owner in [private_old, young_owner] {
        let value = young(0, 1);
        barrier::write_barrier_in(
            fixture.owner,
            owner,
            owner,
            std::ptr::null_mut(),
            value,
            GcStoreDestination::ObjectField as i64 | willow_abi::GC_STORE_OWNER_HAS_HEADER,
        );
        assert!(!shared(value));
    }
    // A null global owner (a static's initialization) is no publication.
    let value = young(0, 1);
    barrier::write_barrier_in(
        fixture.owner,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        value,
        GcStoreDestination::GlobalStatic as i64,
    );
    assert!(!shared(value));
    assert_eq!(fixture.stats().calls, before);
}

#[test]
fn owner_segments_follow_task_switches_migration_and_end() {
    let fixture = Fixture::new();
    // Only the verifier reads chunk ownership, so only it records owners.
    fixture.verifier_on(true);
    run_as(fixture.owner, 0);
    run_as(fixture.owner, TASK_A);
    let a = young(0, 1);
    run_as(fixture.owner, TASK_B);
    let b = young(0, 1);
    // Task A resumes here after running elsewhere (migration): its earlier
    // object keeps owner A, its new one is owned by A too.
    run_as(fixture.owner, TASK_A);
    let a2 = young(0, 1);
    // The task ends, is cancelled or panics: the scheduler installs no task.
    switch_task_owner_in(fixture.owner, 0);
    let none = young(0, 1);
    run_as(fixture.owner, TASK_A);
    verify_now(fixture.owner).unwrap();
    let state = fixture.owner.gc.heap.lock().unwrap();
    let owner_of = |payload: *mut u8| {
        let base = find_tlab_chunk(&state, payload as usize).unwrap().base as usize;
        state.chunk_owners[&base].owner_at(object(payload).as_ptr() as usize - base)
    };
    assert_eq!(owner_of(a), task_owner(TASK_A));
    assert_eq!(owner_of(b), task_owner(TASK_B));
    assert_eq!(owner_of(a2), task_owner(TASK_A));
    assert_eq!(owner_of(none), 0);
}

#[test]
fn owner_survives_a_minor_copy_and_a_cross_task_edge_is_still_caught() {
    let fixture = Fixture::new();
    fixture.verifier_on(true);
    run_as(fixture.owner, TASK_B);
    let mut b_value = young(0, 1);
    run_as(fixture.owner, TASK_A);
    let mut a_holder = young(1, 1);
    willow_push_root(&mut b_value);
    willow_push_root(&mut a_holder);
    willow_gc_minor_collect();
    assert!(!is_old(a_holder), "survivors stay young");
    set(a_holder, 0, b_value);
    let error = verify_now(fixture.owner).unwrap_err();
    assert!(error.contains("of task owner"), "{error}");
    set(a_holder, 0, std::ptr::null_mut());
    willow_pop_root();
    willow_pop_root();
}

#[test]
fn a_task_that_never_allocates_adds_no_owner_entries() {
    let fixture = Fixture::new();
    young(0, 1);
    verify_now(fixture.owner).unwrap();
    let count = || {
        let state = fixture.owner.gc.heap.lock().unwrap();
        state
            .chunk_owners
            .values()
            .map(|segments| segments.0.len())
            .sum::<usize>()
    };
    let before = count();
    for task in 0..10_000 {
        run_as(fixture.owner, 1000 + task);
    }
    run_as(fixture.owner, TASK_A);
    verify_now(fixture.owner).unwrap();
    assert!(
        count() <= before + 1,
        "switches without allocation overwrite"
    );
}

#[test]
fn verifier_accepts_shared_young_objects_and_parked_pending_objects() {
    let fixture = Fixture::new();
    let value = young(0, 1);
    object(value).set_shared(true);
    verify_now(fixture.owner).unwrap();
    let parent = old(1);
    let child = young(0, 1);
    set(parent, 0, child);
    object(parent).set_shared(false);
    assert!(verify_now(fixture.owner).is_err());
    fixture.owner.gc.publication.parked.lock().unwrap().insert(
        std::thread::current().id(),
        publish::Parked {
            root: parent as usize,
            pending: vec![parent as usize],
        },
    );
    let accepted = verify_now(fixture.owner);
    fixture.owner.gc.publication.parked.lock().unwrap().clear();
    accepted.unwrap();
    set(parent, 0, std::ptr::null_mut());
}

#[test]
fn minor_collection_runs_the_verifier_when_enabled() {
    let fixture = Fixture::new();
    fixture.verifier_on(true);
    let mut value = young(1, 1);
    let child = young(0, 1);
    set(value, 0, child);
    publish_in(fixture.owner, value, PublicationPoint::ChannelSend, false);
    willow_push_root(&mut value);
    // Live published young objects move like any young object; the copies
    // keep the SHARED mark and the rooted slot is rewritten.
    willow_gc_minor_collect();
    let moved_child = get(value, 0);
    assert!(shared(value) && shared(moved_child));
    assert_ne!(moved_child, child, "the child moved too");
    verify_now(fixture.owner).unwrap();
    willow_pop_root();
}

#[test]
fn publishing_a_card_capable_buffer_bumps_the_card_epoch() {
    let fixture = Fixture::new();
    let slots = 4096;
    let words = willow_abi::array_layout::BUFFER_HEADER_WORDS + slots;
    let buffer = allocate_old(
        willow_abi::gc_layout_id(
            GcObjectKind::ArrayBuffer,
            (words * 8) as i64,
            willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID as i64,
            0,
        ),
        willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID as u32,
        (words * 8) as i64,
        0,
    );
    assert!(remembered::card_array_capacity(object(buffer)).is_some());
    let epoch = fixture.owner.card_epoch().load(Ordering::Acquire);
    publish_in(fixture.owner, buffer, PublicationPoint::ChannelSend, false);
    assert!(shared(buffer));
    assert!(fixture.owner.card_epoch().load(Ordering::Acquire) > epoch);
    // A plain object publication leaves the epoch alone.
    let epoch = fixture.owner.card_epoch().load(Ordering::Acquire);
    publish_in(fixture.owner, old(1), PublicationPoint::ChannelSend, false);
    assert_eq!(fixture.owner.card_epoch().load(Ordering::Acquire), epoch);
}

#[test]
fn card_cache_proves_a_private_buffer_and_never_a_shared_one() {
    let fixture = Fixture::new();
    let slots = 4096;
    let words = willow_abi::array_layout::BUFFER_HEADER_WORDS + slots;
    let card_buffer = || {
        allocate_old(
            willow_abi::gc_layout_id(
                GcObjectKind::ArrayBuffer,
                (words * 8) as i64,
                willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID as i64,
                0,
            ),
            willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID as u32,
            (words * 8) as i64,
            0,
        )
    };
    let private = card_buffer();
    barrier::card_cache_fill(private as usize, 3);
    assert!(barrier::card_cache_hit(private as usize, 3).is_some());
    // Publishing the cached buffer invalidates the proof (generated array
    // stores skip publication on a hit).
    publish_in(fixture.owner, private, PublicationPoint::ChannelSend, false);
    assert!(barrier::card_cache_hit(private as usize, 3).is_none());
    // A shared buffer never gets a privacy proof (its entry is tagged
    // shared), so generated stores always reach the publication check.
    barrier::card_cache_fill(private as usize, 3);
    assert!(barrier::card_cache_hit(private as usize, 3).is_none());
    let other = card_buffer();
    publish_in(fixture.owner, other, PublicationPoint::Global, false);
    barrier::card_cache_fill(other as usize, 5);
    assert!(barrier::card_cache_hit(other as usize, 5).is_none());
}

#[test]
fn wide_tree_publication_marks_every_level() {
    let fixture = Fixture::new();
    let root = young(64, 64);
    for index in 0..64 {
        let inner = young(64, 64);
        for leaf in 0..64 {
            set(inner, leaf, young(0, 1));
        }
        set(root, index, inner);
    }
    let before = fixture.stats();
    publish_in(fixture.owner, root, PublicationPoint::Spawn, true);
    let after = fixture.stats();
    assert_eq!(
        after.marked_objects - before.marked_objects,
        1 + 64 + 64 * 64
    );
    assert_eq!(after.scanned_slots - before.scanned_slots, 64 + 64 * 64);
    assert!(shared(get(get(root, 63), 63)));
    verify_now(fixture.owner).unwrap();
}

/// Build a young list of `length` nodes rooted at `*head`, a registered
/// root slot. A raw pointer, read volatile: a slow-path allocation may
/// collect and rewrite the slot, which a `&mut` borrow would let the
/// optimizer assume cannot happen (the stale head then dangles).
fn build_list(head: *mut *mut u8, length: usize) {
    for _ in 0..length {
        let node = young(1, 1);
        // SAFETY: `head` is a live registered root slot of this thread.
        set(node, 0, unsafe { head.read_volatile() });
        // SAFETY: as above.
        unsafe { head.write_volatile(node) };
    }
}

#[test]
fn deep_list_publication_parks_mid_way_for_a_requested_stop() {
    const LENGTH: usize = 200_000;
    let fixture = Fixture::new();
    fixture.verifier_on(true);
    let mut head = std::ptr::null_mut();
    willow_push_root(&mut head);
    build_list(&raw mut head, LENGTH);
    let collected = std::sync::Arc::new(AtomicBool::new(false));
    let collector = std::thread::spawn(crate::runtime::serving({
        let collected = std::sync::Arc::clone(&collected);
        move || {
            willow_gc_minor_collect();
            collected.store(true, Ordering::Release);
        }
    }));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !fixture.owner.gc.stop_requested.load(Ordering::Acquire) {
        assert!(std::time::Instant::now() < deadline, "stop never requested");
        std::thread::yield_now();
    }
    let before = fixture.stats();
    publish_in(fixture.owner, head, PublicationPoint::ChannelSend, true);
    let after = fixture.stats();
    collector.join().unwrap();
    assert!(collected.load(Ordering::Acquire));
    assert!(after.parks > before.parks, "parked mid-way");
    assert_eq!(after.marked_objects - before.marked_objects, LENGTH as u64);
    let mut node = head;
    let mut count = 0;
    while !node.is_null() {
        assert!(shared(node));
        node = get(node, 0);
        count += 1;
    }
    assert_eq!(count, LENGTH);
    assert!(
        fixture
            .owner
            .gc
            .publication
            .parked
            .lock()
            .unwrap()
            .is_empty()
    );
    verify_now(fixture.owner).unwrap();
    willow_pop_root();
}

#[test]
fn barrier_publication_never_parks() {
    let fixture = Fixture::new();
    let mut head = std::ptr::null_mut();
    willow_push_root(&mut head);
    build_list(&raw mut head, 4096);
    let target = old(1);
    publish_in(fixture.owner, target, PublicationPoint::Global, false);
    fixture
        .owner
        .gc
        .stop_requested
        .store(true, Ordering::Release);
    barrier::write_barrier_in(
        fixture.owner,
        target,
        target,
        std::ptr::null_mut(),
        head,
        GcStoreDestination::ObjectField as i64 | willow_abi::GC_STORE_OWNER_HAS_HEADER,
    );
    fixture
        .owner
        .gc
        .stop_requested
        .store(false, Ordering::Release);
    assert_eq!(fixture.stats().parks, 0);
    assert!(shared(head));
    willow_pop_root();
}

#[test]
fn published_young_objects_that_die_young_are_accounted_by_the_minor() {
    let fixture = Fixture::new();
    let before = fixture.stats();
    let value = young(0, 4);
    // A private old object published as well: shared, but not young when
    // published, so its death is no promoted-then-died byte.
    let private_old = old(4);
    publish_in(fixture.owner, value, PublicationPoint::ChannelSend, false);
    publish_in(
        fixture.owner,
        private_old,
        PublicationPoint::ChannelSend,
        false,
    );
    let promoted = fixture.stats().promoted_bytes - before.promoted_bytes;
    assert_eq!(promoted, (GC_HEADER_SIZE + 32) as u64);
    assert!(object(value).promoted_by_publication());
    assert!(shared(private_old) && !object(private_old).promoted_by_publication());
    // Nothing roots the young value: the minor reclaims it young.
    willow_gc_minor_collect();
    assert_eq!(
        fixture.stats().promoted_died_bytes - before.promoted_died_bytes,
        promoted
    );
    // A later full collection frees the old one without counting it.
    willow_gc_collect();
    assert_eq!(
        fixture.stats().promoted_died_bytes - before.promoted_died_bytes,
        promoted
    );
}

#[test]
fn tenured_published_objects_that_die_old_are_accounted_by_a_major_collection() {
    let fixture = Fixture::new();
    let mut value = young(1, 1);
    let child = young(0, 1);
    set(value, 0, child);
    publish_in(fixture.owner, value, PublicationPoint::ChannelSend, false);
    let before = fixture.stats();
    willow_push_root(&mut value);
    // Survivor copies stay counted as published-young until they tenure.
    let mut minors = 0;
    while !is_old(value) {
        willow_gc_minor_collect();
        minors += 1;
        assert!(minors <= 8, "a rooted survivor tenures");
    }
    willow_pop_root();
    let child = get(value, 0);
    assert!(is_old(child) && shared(value) && shared(child));
    assert!(object(value).promoted_by_publication());
    assert_eq!(
        fixture.stats().promoted_died_bytes,
        before.promoted_died_bytes,
        "live at every minor: nothing died"
    );
    // Unrooted now: the full collection frees both as old objects.
    willow_gc_collect();
    assert_eq!(
        fixture.stats().promoted_died_bytes - before.promoted_died_bytes,
        (2 * GC_HEADER_SIZE + 16) as u64
    );
}

#[test]
fn unregistered_threads_publish_under_the_heap_lock() {
    let fixture = Fixture::new();
    let value = old(1);
    let child = old(0);
    set(value, 0, child);
    let address = value as usize;
    std::thread::spawn(crate::runtime::serving(move || {
        publish_in(
            crate::runtime::current(),
            address as *mut u8,
            PublicationPoint::NativeBoundary,
            false,
        );
    }))
    .join()
    .unwrap();
    assert!(shared(value) && shared(child));
    assert_eq!(
        fixture.stats().per_point[PublicationPoint::NativeBoundary as usize],
        1
    );
}

#[test]
fn owner_held_root_handle_publishes_on_creation() {
    let arc = Fixture::owned();
    let fixture = Fixture::on(arc);
    let value = young(0, 1);
    let handle = root_arena::GcRootHandle::new_in(std::sync::Arc::clone(arc), value);
    assert!(shared(value));
    assert_eq!(
        fixture.stats().per_point[PublicationPoint::RuntimeRoot as usize],
        1
    );
    // A scoped task-stack handle is not a publication.
    let scoped_value = young(0, 1);
    let scoped = root_arena::GcRootHandle::new(scoped_value);
    assert!(!shared(scoped_value));
    drop(scoped);
    drop(handle);
}

#[test]
fn a_shared_young_value_stored_into_an_old_owner_is_remembered() {
    let fixture = Fixture::new();
    fixture.verifier_on(true);
    let mut value = young(0, 1);
    publish_in(fixture.owner, value, PublicationPoint::ChannelSend, false);
    assert!(shared(value) && !is_old(value));
    for owner_shared in [false, true] {
        let mut owner = old(1);
        if owner_shared {
            publish_in(fixture.owner, owner, PublicationPoint::Global, false);
        }
        barrier::write_barrier_in(
            fixture.owner,
            owner,
            owner,
            std::ptr::null_mut(),
            value,
            GcStoreDestination::ObjectField as i64 | willow_abi::GC_STORE_OWNER_HAS_HEADER,
        );
        set(owner, 0, value);
        assert!(
            object(owner).remembered(),
            "owner shared {owner_shared}: the old-to-young edge is remembered"
        );
        // Only the owner roots the value: the minor finds it through the
        // remembered set and rewrites the owner's slot to the moved copy.
        willow_push_root(&mut owner);
        willow_gc_minor_collect();
        willow_pop_root();
        let moved = get(owner, 0);
        assert!(!moved.is_null() && shared(moved));
        assert_eq!(object(moved).size(), GC_HEADER_SIZE + 8);
        set(owner, 0, std::ptr::null_mut());
        value = young(0, 1);
        publish_in(fixture.owner, value, PublicationPoint::ChannelSend, false);
    }
}

#[test]
fn a_published_survivor_that_dies_young_later_is_counted_once() {
    let fixture = Fixture::new();
    let mut value = young(0, 2);
    publish_in(fixture.owner, value, PublicationPoint::ChannelSend, false);
    let before = fixture.stats();
    willow_push_root(&mut value);
    willow_gc_minor_collect();
    willow_pop_root();
    assert!(
        shared(value) && !is_old(value),
        "copied to a young survivor"
    );
    assert_eq!(
        fixture.stats().promoted_died_bytes,
        before.promoted_died_bytes
    );
    // Unrooted: the next minor reclaims the survivor young.
    willow_gc_minor_collect();
    let died = fixture.stats().promoted_died_bytes - before.promoted_died_bytes;
    assert_eq!(died, (GC_HEADER_SIZE + 16) as u64);
    willow_gc_minor_collect();
    assert_eq!(
        fixture.stats().promoted_died_bytes - before.promoted_died_bytes,
        died,
        "counted once"
    );
}

/// A private old object holding one data word.
fn old_data(value: usize) -> *mut u8 {
    let payload = allocate_old(layout(1, 0), 0, 8, 0);
    assert!(!payload.is_null());
    // SAFETY: one data word.
    unsafe { payload.cast::<usize>().write(value) };
    payload
}

fn data(payload: *mut u8, word: usize) -> usize {
    // SAFETY: tests read data words inside the allocation.
    unsafe { payload.cast::<usize>().add(word).read() }
}

/// A generated store: barrier first, then the slot write.
fn store(owner: &Runtime, target: *mut u8, index: usize, value: *mut u8) {
    barrier::write_barrier_in(
        owner,
        target,
        target,
        get(target, index),
        value,
        GcStoreDestination::ObjectField as i64
            | willow_abi::GC_STORE_OWNER_HAS_HEADER
            | willow_abi::GC_STORE_VALUE_HAS_HEADER,
    );
    set(target, index, value);
}

fn assert_live_shared(payload: *mut u8, what: &str) {
    assert!(!payload.is_null(), "{what}: reachable");
    assert!(object(payload).allocated(), "{what}: not swept");
    assert!(shared(payload), "{what}: shared");
}

/// Publications into a shared owner while a concurrent major cycle marks,
/// closes and sweeps: every published object (young ones moved by minors,
/// private old ones the snapshot reached only through a private holder that
/// drops them) survives with its data and stays shared.
#[test]
fn publication_during_concurrent_mark_close_and_sweep_loses_nothing() {
    const CHAIN: usize = 300;
    let fixture = Fixture::new();
    fixture.verifier_on(true);
    let owner = fixture.owner;
    let mut shared_owner = old(4);
    publish_in(owner, shared_owner, PublicationPoint::Global, false);
    let mut holder = old(3);
    let tail = old_data(7777);
    let late = old(1);
    let swept = old_data(4242);
    set(holder, 0, tail);
    set(holder, 1, late);
    set(holder, 2, swept);
    willow_push_root(&mut shared_owner);
    willow_push_root(&mut holder);
    let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
    let (_, cycle) = root_handshake::begin().unwrap();

    // Marking: a young chain allocated after the snapshot ends at an old
    // object the snapshot reached only through `holder`; published, then
    // dropped from the holder.
    let mut head = tail;
    willow_push_root(&mut head);
    for index in 0..CHAIN {
        let node = young(1, 2);
        set(node, 0, head);
        // SAFETY: word 1 of a two-word node is data.
        unsafe { node.cast::<usize>().add(1).write(index) };
        head = node;
    }
    store(owner, shared_owner, 0, head);
    willow_pop_root();
    store(owner, holder, 0, std::ptr::null_mut());
    minor::collect_owned();
    while !cycle.queue.snapshot().is_drained() {
        cycle.drain(usize::MAX);
    }

    // Closing: a private old object with a fresh young child.
    cycle.close();
    let leaf = young(0, 1);
    // SAFETY: one data word.
    unsafe { leaf.cast::<usize>().write(99) };
    store(owner, late, 0, leaf);
    store(owner, shared_owner, 1, late);
    store(owner, holder, 1, std::ptr::null_mut());
    minor::collect_owned();

    crate::gc::preemption_tolerant_remark_budget_for_test();
    let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
    // Sweeping: an old object marked through the holder, and a young one
    // allocated now.
    store(owner, shared_owner, 2, swept);
    store(owner, holder, 2, std::ptr::null_mut());
    let fresh = young(0, 1);
    // SAFETY: one data word.
    unsafe { fresh.cast::<usize>().write(55) };
    store(owner, shared_owner, 3, fresh);
    sweep::concurrent(plan);
    assert!(cycle.queue.snapshot().is_drained());
    drop(elected);

    let check = |round: &str| {
        let mut node = get(shared_owner, 0);
        for index in (0..CHAIN).rev() {
            assert_live_shared(node, &format!("{round} chain node {index}"));
            assert_eq!(data(node, 1), index, "{round} chain node data");
            node = get(node, 0);
        }
        assert_eq!(node, tail, "old tail is never moved");
        assert_live_shared(node, &format!("{round} tail"));
        assert_eq!(data(node, 0), 7777);
        let late = get(shared_owner, 1);
        assert_live_shared(late, &format!("{round} late"));
        let leaf = get(late, 0);
        assert_live_shared(leaf, &format!("{round} late leaf"));
        assert_eq!(data(leaf, 0), 99);
        let swept = get(shared_owner, 2);
        assert_live_shared(swept, &format!("{round} swept"));
        assert_eq!(data(swept, 0), 4242);
        let fresh = get(shared_owner, 3);
        assert_live_shared(fresh, &format!("{round} fresh"));
        assert_eq!(data(fresh, 0), 55);
    };
    check("after sweep");
    verify_now(owner).unwrap();
    // The next minor moves the young survivors; the next full collection
    // marks the shared graph from the owner alone.
    willow_gc_minor_collect();
    check("after minor");
    willow_gc_collect();
    check("after full collection");
    verify_now(owner).unwrap();
    willow_pop_roots(2);
}

/// A one-slot task frame (slot 0 a reference) holding `result`.
fn task_frame(result: *mut u8) -> *mut u8 {
    let frame = crate::async_frame::willow_async_frame_alloc(1, 1).cast::<u8>();
    let offset = crate::async_frame::async_frame_slot_offset(0);
    // SAFETY: slot 0 of a one-slot frame.
    unsafe { frame.add(offset).cast::<*mut u8>().write(result) };
    frame
}

/// A young two-node graph built by the running task.
fn young_pair() -> (*mut u8, *mut u8) {
    let child = young(0, 1);
    let value = young(1, 1);
    set(value, 0, child);
    (value, child)
}

#[test]
fn finished_task_hands_a_private_handle_its_result_without_publishing() {
    let fixture = Fixture::new();
    fixture.verifier_on(true);
    let (value, child) = young_pair();
    let frame = task_frame(value);
    let before = fixture.stats();
    finish_task_result_in(fixture.owner, TASK_A, frame);
    let after = fixture.stats();
    assert!(!shared(value) && !shared(child), "handed over unpublished");
    assert_eq!(after.handed_over, before.handed_over + 1);
    assert_eq!(after.calls, before.calls, "no traversal");
    assert_eq!(
        after.per_point[PublicationPoint::TaskReturn as usize],
        before.per_point[PublicationPoint::TaskReturn as usize]
    );
    // The holder (task B) keeps the result in its own young objects: the
    // finished task's young objects are now B's.
    read_from_task_b(&fixture, value);
    verify_now(fixture.owner).unwrap();
}

#[test]
fn handle_escaping_after_its_task_finished_publishes_the_result_once() {
    let fixture = Fixture::new();
    let (value, child) = young_pair();
    let frame = task_frame(value);
    finish_task_result_in(fixture.owner, TASK_A, frame);
    assert!(!shared(value));
    exercise(&fixture, PublicationPoint::ChannelSend, frame);
    assert!(
        shared(value) && shared(child),
        "the escape publishes the result"
    );
    assert!(!shared(frame), "the frame itself stays private");
    let marked = fixture.stats().marked_objects;
    // A second escape of the same handle is O(1): nothing is traversed.
    exercise(&fixture, PublicationPoint::ChannelSend, frame);
    publish_in(fixture.owner, frame, PublicationPoint::Global, false);
    assert_eq!(fixture.stats().marked_objects, marked);
    read_from_task_b(&fixture, value);
    verify_now(fixture.owner).unwrap();
}

#[test]
fn handle_escaping_before_its_task_finished_publishes_at_task_return() {
    let fixture = Fixture::new();
    let (value, child) = young_pair();
    let frame = task_frame(value);
    exercise(&fixture, PublicationPoint::ChannelSend, frame);
    assert!(!shared(value), "a running task's result is not read yet");
    let before = fixture.stats();
    finish_task_result_in(fixture.owner, TASK_A, frame);
    let after = fixture.stats();
    assert!(shared(value) && shared(child));
    assert_eq!(after.handed_over, before.handed_over);
    assert_eq!(
        after.per_point[PublicationPoint::TaskReturn as usize],
        before.per_point[PublicationPoint::TaskReturn as usize] + 1
    );
}

#[test]
fn handle_reached_inside_a_published_graph_publishes_its_handed_over_result() {
    let fixture = Fixture::new();
    let (value, child) = young_pair();
    let frame = task_frame(value);
    finish_task_result_in(fixture.owner, TASK_A, frame);
    // A message whose field holds the handle (the traversal's slot path).
    let message = young(2, 2);
    set(message, 0, frame);
    publish_in(fixture.owner, message, PublicationPoint::ChannelSend, false);
    assert!(shared(message) && shared(value) && shared(child));
    assert!(!shared(frame));
    verify_now(fixture.owner).unwrap();
}

#[test]
fn chains_of_handed_over_handles_publish_iteratively() {
    let fixture = Fixture::new();
    // Task k returned the handle of task k + 1; the last returned a list.
    let (value, child) = young_pair();
    let mut handle = task_frame(value);
    finish_task_result_in(fixture.owner, TASK_A, handle);
    let mut frames = vec![handle];
    for _ in 0..2_000 {
        handle = task_frame(handle);
        finish_task_result_in(fixture.owner, TASK_A, handle);
        frames.push(handle);
    }
    frames.shrink_to_fit();
    for frame in frames.iter_mut() {
        willow_push_root(frame);
    }
    // Directly and through a slot: both walk the chain without recursion.
    let message = young(1, 1);
    set(message, 0, handle);
    publish_in(fixture.owner, message, PublicationPoint::ChannelSend, false);
    assert!(shared(value) && shared(child));
    assert!(frames.iter().all(|frame| !shared(*frame)));
    willow_pop_roots(frames.len() as i32);
}

#[test]
fn escaped_handle_with_skipped_task_return_is_caught_by_the_verifier() {
    let fixture = Fixture::new();
    fixture.verifier_on(true);
    fixture.skip(PublicationPoint::TaskReturn, true);
    let value = young(0, 1);
    let frame = task_frame(value);
    exercise(&fixture, PublicationPoint::ChannelSend, frame);
    finish_task_result_in(fixture.owner, TASK_A, frame);
    assert!(!shared(value));
    // Another task awaits the escaped handle and keeps the result.
    read_from_task_b(&fixture, value);
    let error = verify_now(fixture.owner).unwrap_err();
    assert!(error.contains("of task owner"), "{error}");
}

#[test]
fn handed_over_owners_reset_at_the_next_minor() {
    let fixture = Fixture::new();
    fixture.verifier_on(true);
    let value = young(0, 1);
    let frame = task_frame(value);
    let mut root = frame;
    willow_push_root(&mut root);
    finish_task_result_in(fixture.owner, TASK_A, frame);
    assert!(
        fixture
            .owner
            .gc
            .publication
            .handed_over_owners
            .lock()
            .unwrap()
            .contains(&task_owner(TASK_A))
    );
    willow_gc_minor_collect();
    assert!(
        fixture
            .owner
            .gc
            .publication
            .handed_over_owners
            .lock()
            .unwrap()
            .is_empty()
    );
    // The survivor belongs to no task now: the holder may keep it.
    let value = get(
        root.wrapping_add(crate::async_frame::async_frame_slot_offset(0)),
        0,
    );
    read_from_task_b(&fixture, value);
    verify_now(fixture.owner).unwrap();
    willow_pop_roots(1);
}
