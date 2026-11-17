//! Mutator-private edge and root-slot logs: lock-free normal paths, bounded
//! storage, exact application at every stop, and negative checks proving the
//! verifier catches entries a broken drain would lose.

use super::*;
use crate::gc::barrier::write_barrier_object_owner;
use crate::gc::root::{add_runtime_root_slot_in, remove_runtime_root_slot_in};
use crate::gc_telemetry::pause::Component;
use crate::gc_telemetry::stops::StopReason;
use crate::runtime::isolated_runtime;
use std::time::Duration;

const OBJECT_FIELD: i64 = GcStoreDestination::ObjectField as i64;

fn quiet_heap() {
    reset_internal_for_test();
    let mut state = runtime().heap.lock().unwrap();
    state.threshold_bytes = usize::MAX;
    state.pacer_trigger = u64::MAX;
    state.pacer.disable_for_test();
    state.nursery_threshold_bytes = usize::MAX;
}

fn owner() -> &'static crate::runtime::Runtime {
    crate::runtime::current()
}

/// An old object with one reference slot at payload offset 0.
fn old_object() -> *mut u8 {
    willow_gc_alloc_layout(0, 61, 8, 0b1)
}

fn young_object(tls: &mut GcTlabState, value: u64) -> *mut u8 {
    let object = willow_gc_alloc_slow(tls, 0, 62, 8, 0);
    unsafe { *object.cast::<u64>() = value };
    object
}

/// Runtime object store: barrier, then the store, as runtime containers do.
fn store(owner: *mut u8, value: *mut u8) {
    let slot = owner.cast::<*mut u8>();
    write_barrier_object_owner(owner, unsafe { *slot }, value, OBJECT_FIELD);
    unsafe { *slot = value };
}

fn shared_locks() -> u64 {
    shared_locks_for_test()
}

fn pending_edges() -> usize {
    bound_in(&owner().gc).map_or(0, |log| unsafe { log.edges() }.len())
}

fn pending_roots() -> Vec<(usize, isize)> {
    bound_in(&owner().gc).map_or_else(Vec::new, |log| unsafe { log.roots() }.clone())
}

/// A stop that only drains: no collection runs.
fn stop() {
    safepoint::with_stw(StopReason::Minor, |_, _| ());
}

fn minor() {
    super::super::minor::minor_collect_in(owner());
}

fn verify() -> Result<(), String> {
    let state = runtime().heap.lock().unwrap();
    let registry = type_registry().lock().unwrap();
    verify_remembered_set(&state, &registry)
}

fn remembered(owner: *mut u8) -> bool {
    runtime()
        .heap
        .lock()
        .unwrap()
        .remembered_set
        .contains(&(owner as usize))
}

fn remembered_len() -> usize {
    runtime().heap.lock().unwrap().remembered_set.len()
}

fn root_slots() -> Vec<*mut *mut u8> {
    super::super::root::runtime_root_slots(runtime())
}

#[test]
fn logged_edge_is_private_until_the_next_stop() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    let child = young_object(&mut tls, 7);
    let locks = shared_locks();
    store(parent, child);
    assert_eq!(shared_locks(), locks, "registered store took a shared lock");
    assert_eq!(pending_edges(), 1);
    assert!(!remembered(parent));
    // Negative: a collector reading without the drain misses the edge.
    assert!(
        verify()
            .expect_err("an undrained log must fail verification")
            .contains("remembered")
    );
    stop();
    assert_eq!(pending_edges(), 0);
    assert!(remembered(parent));
    verify().unwrap();
    reset_internal_for_test();
}

#[test]
fn a_dropped_log_is_detected_by_verification() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    store(parent, young_object(&mut tls, 1));
    // Simulate a drain that forgot this mutator.
    unsafe { bound_in(&owner().gc).unwrap().edges() }.clear();
    stop();
    assert!(verify().is_err(), "lost log entry went undetected");
    reset_internal_for_test();
}

#[test]
fn unresolved_store_is_classified_when_applied() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    let other = old_object();
    let child = young_object(&mut tls, 3);
    // No header proofs: the log keeps the raw store for later classification.
    unsafe { *parent.cast::<*mut u8>() = child };
    write_barrier_in(
        owner(),
        parent,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        child,
        OBJECT_FIELD,
    );
    // Old value into an old owner: classified away, never remembered.
    write_barrier_in(
        owner(),
        other,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        parent,
        OBJECT_FIELD,
    );
    assert_eq!(pending_edges(), 2);
    stop();
    assert!(remembered(parent));
    assert!(!remembered(other));
    verify().unwrap();
    reset_internal_for_test();
}

#[test]
fn repeated_stores_into_one_owner_log_once() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    for value in 0..1000 {
        store(parent, young_object(&mut tls, value));
    }
    // The claimed remembered byte filters every later store.
    assert_eq!(pending_edges(), 1);
    stop();
    assert_eq!(remembered_len(), 1);
    verify().unwrap();
    reset_internal_for_test();
}

/// Sends into an old channel are remembered through the channel's header:
/// one owner entry per minor, whatever the number of sends, and no send
/// takes the heap lock. The queued values survive the minor intact.
#[test]
fn channel_sends_into_an_old_channel_log_one_owner_edge() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let mut channel = crate::channel::willow_channel_new(1).cast::<u8>();
    willow_push_root(&mut channel);
    let header = HeapObject::from_payload(GcPayload::from_raw(channel).unwrap());
    assert_eq!(header.generation(), GC_GENERATION_OLD, "channels start old");
    let sends = 4 * EDGE_LOG_CAPACITY as u64;
    let locks = shared_locks();
    for value in 0..sends {
        assert_eq!(
            crate::channel::willow_channel_try_send_ptr(
                channel.cast(),
                young_object(&mut tls, value).cast()
            ),
            1
        );
    }
    assert_eq!(shared_locks(), locks, "a send took the heap lock");
    assert_eq!(pending_edges(), 1, "one owner entry covers every send");
    stop();
    assert!(remembered(channel));
    verify().unwrap();
    minor();
    for value in 0..sends {
        let received = crate::channel::willow_channel_recv_ptr(channel.cast()).cast::<u64>();
        assert_eq!(unsafe { *received }, value);
    }
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn write_lock_count_is_zero_for_any_number_of_stop_separated_batches() {
    let _guard = isolated_runtime();
    for batches in [1usize, 10, 100] {
        quiet_heap();
        let registration = MutatorRegistration::new();
        let mut tls = tlab_state_for_test();
        let locks = shared_locks();
        for _ in 0..batches {
            for value in 0..EDGE_LOG_CAPACITY as u64 {
                store(old_object(), young_object(&mut tls, value));
            }
            stop();
        }
        assert_eq!(shared_locks(), locks, "batches={batches}");
        drop(registration);
        reset_internal_for_test();
    }
}

#[test]
fn full_edge_log_flushes_once_per_capacity() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let locks = shared_locks();
    let total = 3 * EDGE_LOG_CAPACITY + 1;
    for value in 0..total as u64 {
        store(old_object(), young_object(&mut tls, value));
        assert!(
            pending_edges() <= EDGE_LOG_CAPACITY,
            "log exceeded its bound"
        );
    }
    assert_eq!(shared_locks() - locks, 3);
    assert_eq!(remembered_len(), 3 * EDGE_LOG_CAPACITY);
    assert_eq!(pending_edges(), 1);
    stop();
    assert_eq!(remembered_len(), total);
    verify().unwrap();
    reset_internal_for_test();
}

#[test]
fn card_buffer_stores_log_one_card_and_hit_the_cache() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let mut array = crate::array::new_old_array(4096, 1);
    willow_push_root(&mut array);
    let buffer = unsafe { *array.cast::<usize>().add(willow_abi::array_layout::H_BUF) };
    for index in 0..8 {
        crate::array::willow_array_set(array, index, young_object(&mut tls, 5) as i64);
    }
    assert_eq!(pending_edges(), 1, "cache must deduplicate one card");
    stop();
    let cards = runtime()
        .heap
        .lock()
        .unwrap()
        .remembered_set
        .owner_cards(buffer)
        .map(|cards| cards.iter().copied().collect::<Vec<_>>());
    assert_eq!(cards, Some(vec![0]));
    verify().unwrap();
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn minor_after_logged_edge_keeps_the_young_child() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let mut parent = old_object();
    willow_push_root(&mut parent);
    store(parent, young_object(&mut tls, 4242));
    minor();
    let child = unsafe { *parent.cast::<*mut u8>() };
    assert_eq!(unsafe { *child.cast::<u64>() }, 4242);
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn second_thread_store_into_claimed_owner_logs_nothing() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    store(parent, young_object(&mut tls, 1));
    let parent_address = parent as usize;
    // Idle, this thread's log is drained by the other thread's stop.
    let idle = unsafe { safepoint::IdleMutator::enter() };
    std::thread::scope(|scope| {
        scope
            .spawn(crate::runtime::serving(move || {
                let _registration = MutatorRegistration::new();
                let mut tls = tlab_state_for_test();
                let value = young_object(&mut tls, 2);
                store(parent_address as *mut u8, value);
                assert_eq!(pending_edges(), 0);
                // Keep the other store's value reachable through the owner.
                store(parent_address as *mut u8, value);
                stop();
            }))
            .join()
            .unwrap();
    });
    drop(idle);
    assert!(remembered(parent));
    verify().unwrap();
    reset_internal_for_test();
}

#[test]
fn registered_paths_complete_while_shared_locks_are_held() {
    let _guard = isolated_runtime();
    quiet_heap();
    let parent = old_object();
    let parent_address = parent as usize;
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (start_tx, start_rx) = std::sync::mpsc::channel::<()>();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        scope.spawn(crate::runtime::serving(move || {
            let registration = MutatorRegistration::new();
            let mut tls = tlab_state_for_test();
            let value = young_object(&mut tls, 9);
            let mut slot: *mut u8 = std::ptr::null_mut();
            ready_tx.send(()).unwrap();
            start_rx.recv().unwrap();
            store(parent_address as *mut u8, value);
            add_runtime_root_slot_in(owner(), &raw mut slot);
            remove_runtime_root_slot_in(owner(), &raw mut slot);
            done_tx.send(()).unwrap();
            drop(registration);
        }));
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let heap = runtime().heap.lock().unwrap();
        let shards = runtime().runtime_root_slots.hold_all_for_test();
        start_tx.send(()).unwrap();
        let completed = done_rx.recv_timeout(Duration::from_secs(5));
        drop(shards);
        drop(heap);
        assert!(
            completed.is_ok(),
            "registered path waited for a shared lock"
        );
    });
    // Unregistration flushed the worker's log.
    assert!(remembered(parent));
    assert!(root_slots().is_empty());
    reset_internal_for_test();
}

#[test]
fn unregistered_thread_keeps_the_locked_paths() {
    let _guard = isolated_runtime();
    quiet_heap();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    let mut slot: *mut u8 = std::ptr::null_mut();
    let locks = shared_locks();
    store(parent, young_object(&mut tls, 1));
    add_runtime_root_slot_in(owner(), &raw mut slot);
    assert_eq!(shared_locks() - locks, 2);
    assert!(remembered(parent));
    assert_eq!(root_slots(), vec![&raw mut slot]);
    remove_runtime_root_slot_in(owner(), &raw mut slot);
    assert!(root_slots().is_empty());
    reset_internal_for_test();
}

#[test]
fn root_pairs_take_no_lock_for_any_n() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    for pairs in [1usize, 100, 10_000] {
        let locks = shared_locks();
        let mut slots = [std::ptr::null_mut::<u8>(); 4];
        for index in 0..pairs {
            let slot = &raw mut slots[index % 4];
            add_runtime_root_slot_in(owner(), slot);
            remove_runtime_root_slot_in(owner(), slot);
        }
        // Nested LIFO and FIFO handle lifetimes cancel inside the window.
        for index in 0..pairs {
            let (first, second) = (&raw mut slots[0], &raw mut slots[1 + index % 3]);
            add_runtime_root_slot_in(owner(), first);
            add_runtime_root_slot_in(owner(), second);
            remove_runtime_root_slot_in(owner(), first);
            remove_runtime_root_slot_in(owner(), second);
        }
        assert_eq!(shared_locks(), locks, "pairs={pairs}");
        assert!(pending_roots().is_empty());
        assert_eq!(super::super::root::runtime_root_count_in(&owner().gc), 0);
    }
    reset_internal_for_test();
}

#[test]
fn unpaired_root_adds_flush_once_per_capacity() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let total = 2 * ROOT_LOG_CAPACITY + 1;
    let mut slots = vec![std::ptr::null_mut::<u8>(); total];
    let locks = shared_locks();
    for slot in &mut slots {
        add_runtime_root_slot_in(owner(), slot);
        assert!(pending_roots().len() <= ROOT_LOG_CAPACITY);
    }
    assert_eq!(shared_locks() - locks, 2);
    assert_eq!(
        super::super::root::runtime_root_count_in(&owner().gc),
        total,
        "count includes pending registrations"
    );
    stop();
    assert_eq!(root_slots().len(), total);
    for slot in &mut slots {
        remove_runtime_root_slot_in(owner(), slot);
    }
    stop();
    assert!(root_slots().is_empty());
    reset_internal_for_test();
}

#[test]
fn root_removed_after_task_migration_is_exact_in_either_order() {
    let _guard = isolated_runtime();
    for remove_flushes_first in [false, true] {
        quiet_heap();
        let _registration = MutatorRegistration::new();
        let mut slot: *mut u8 = std::ptr::null_mut();
        let slot_address = &raw mut slot as usize;
        add_runtime_root_slot_in(owner(), &raw mut slot);
        // Idle, this thread's pending add is drained by the other's stop.
        let idle = unsafe { safepoint::IdleMutator::enter() };
        std::thread::scope(|scope| {
            scope
                .spawn(crate::runtime::serving(move || {
                    // The handle's task resumed on another worker.
                    let _registration = MutatorRegistration::new();
                    remove_runtime_root_slot_in(owner(), slot_address as *mut *mut u8);
                    assert_eq!(pending_roots(), vec![(slot_address, -1)]);
                    if remove_flushes_first {
                        flush_current_in(owner());
                        // The removal reached the shared set before the add.
                        assert!(root_slots().is_empty());
                    } else {
                        stop();
                    }
                }))
                .join()
                .unwrap();
        });
        drop(idle);
        stop();
        assert!(
            root_slots().is_empty(),
            "flush first={remove_flushes_first}"
        );
        assert_eq!(runtime().runtime_root_slots.net(), 0);
        reset_internal_for_test();
    }
}

#[test]
fn signed_root_counts_commute() {
    let roots = RuntimeRootSet::default();
    let slot = 0x4000 as *mut u8;
    roots.adjust(slot, -1);
    assert!(roots.snapshot().is_empty(), "negative count is not a root");
    assert_eq!(roots.net(), -1);
    roots.adjust(slot, 1);
    assert!(roots.snapshot().is_empty());
    assert_eq!(roots.net(), 0);
    roots.adjust(slot, 2);
    roots.adjust(slot, -1);
    assert_eq!(roots.snapshot(), vec![slot]);
    roots.adjust(slot, -1);
    assert!(roots.snapshot().is_empty());
}

#[test]
fn unregistration_flushes_pending_entries() {
    let _guard = isolated_runtime();
    quiet_heap();
    let parent = old_object();
    let parent_address = parent as usize;
    let mut slot: *mut u8 = std::ptr::null_mut();
    let slot_address = &raw mut slot as usize;
    std::thread::scope(|scope| {
        scope
            .spawn(crate::runtime::serving(move || {
                let registration = MutatorRegistration::new();
                let mut tls = tlab_state_for_test();
                store(parent_address as *mut u8, young_object(&mut tls, 1));
                add_runtime_root_slot_in(owner(), slot_address as *mut *mut u8);
                drop(registration);
                assert!(bound_in(&owner().gc).is_none());
            }))
            .join()
            .unwrap();
    });
    assert!(remembered(parent));
    assert_eq!(root_slots(), vec![slot_address as *mut *mut u8]);
    remove_runtime_root_slot_in(owner(), &raw mut slot);
    reset_internal_for_test();
}

#[test]
fn log_storage_is_charged_while_registered() {
    let _guard = isolated_runtime();
    quiet_heap();
    let native = || owner().gc.native_accounting.bytes.load(Ordering::Relaxed);
    let before = native();
    let registration = MutatorRegistration::new();
    assert_eq!(native(), before + MutatorLog::BYTES);
    // Re-entrant registration shares the log and its charge.
    let nested = MutatorRegistration::new();
    assert_eq!(native(), before + MutatorLog::BYTES);
    drop(nested);
    drop(registration);
    assert_eq!(native(), before);
    const { assert!(MutatorLog::BYTES <= 8 * 1024, "per-mutator bound grew") };
    reset_internal_for_test();
}

#[test]
fn reset_releases_log_storage() {
    let _guard = isolated_runtime();
    quiet_heap();
    let native = || owner().gc.native_accounting.bytes.load(Ordering::Relaxed);
    let before = native();
    let registration = MutatorRegistration::new();
    assert_eq!(native(), before + MutatorLog::BYTES);
    reset_internal_for_test();
    assert_eq!(native(), before);
    assert!(owner().gc.coord.0.lock().unwrap().mutator_logs.is_empty());
    // The invalidated registration releases nothing twice.
    drop(registration);
    assert_eq!(native(), before);
}

#[test]
fn drain_time_is_a_pause_component_and_counted() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let drained = || {
        owner()
            .gc
            .mutator_log_drained_entries
            .load(Ordering::Relaxed)
    };
    let before = drained();
    let pause_before = crate::gc_telemetry::pause::snapshot().by_reason
        [StopReason::Minor as usize - 1][Component::RememberedLog as usize]
        .count;
    store(old_object(), young_object(&mut tls, 1));
    let mut slot: *mut u8 = std::ptr::null_mut();
    add_runtime_root_slot_in(owner(), &raw mut slot);
    stop();
    assert_eq!(drained() - before, 2);
    let stats = crate::gc_telemetry::pause::snapshot();
    assert!(
        stats.by_reason[StopReason::Minor as usize - 1][Component::RememberedLog as usize].count
            > pause_before
    );
    remove_runtime_root_slot_in(owner(), &raw mut slot);
    reset_internal_for_test();
}

#[test]
fn telemetry_readers_see_their_own_logged_edges() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    store(old_object(), young_object(&mut tls, 1));
    assert_eq!(willow_gc_remembered_set_size(), 1);
    assert_eq!(willow_gc_write_barrier_hits(), 1);
    assert_eq!(pending_edges(), 0);
    reset_internal_for_test();
}

#[test]
fn stores_during_marking_are_logged_and_drained_by_the_remark_stop() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    let cycle = Arc::new(ConcurrentCycle::new(
        std::iter::empty(),
        Default::default(),
        Default::default(),
        0,
    ));
    marking::set_concurrent_cycle_in(
        &owner().gc,
        &mut runtime().heap.lock().unwrap(),
        Some(cycle.clone()),
    );
    owner().mark_phase().store(1, Ordering::Release);
    store(parent, young_object(&mut tls, 1));
    assert!(!remembered(parent));
    safepoint::with_stw(StopReason::Remark, |_, _| {
        owner().mark_phase().store(0, Ordering::Release)
    });
    assert!(remembered(parent));
    verify().unwrap();
    marking::set_concurrent_cycle_in(&owner().gc, &mut runtime().heap.lock().unwrap(), None);
    reset_internal_for_test();
}

#[test]
fn global_static_and_null_owner_stores_never_log() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let child = young_object(&mut tls, 1);
    let mut global: *mut u8 = std::ptr::null_mut();
    let locks = shared_locks();
    write_barrier_in(
        owner(),
        (&raw mut global).cast(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        child,
        GcStoreDestination::GlobalStatic as i64,
    );
    write_barrier_in(
        owner(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        child,
        OBJECT_FIELD,
    );
    assert_eq!(pending_edges(), 0);
    assert_eq!(shared_locks(), locks);
    reset_internal_for_test();
}

/// Generated frame-style store: the owner header is known, the value is not.
fn store_unproven_value(owner_object: *mut u8, value: *mut u8) {
    unsafe { *owner_object.cast::<*mut u8>() = value };
    write_barrier_in(
        owner(),
        owner_object,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        value,
        OBJECT_FIELD | willow_abi::GC_STORE_OWNER_HAS_HEADER,
    );
}

#[test]
fn owner_proven_stores_with_unproven_values_log_once_per_owner() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    let locks = shared_locks();
    for value in 0..(4 * EDGE_LOG_CAPACITY as u64) {
        store_unproven_value(parent, young_object(&mut tls, value));
    }
    // The first store claims the owner's byte; later ones are filtered out.
    assert_eq!(pending_edges(), 1);
    assert_eq!(shared_locks(), locks, "frame-style stores flushed the log");
    stop();
    assert!(remembered(parent));
    verify().unwrap();
    minor();
    assert!(remembered(parent), "the young child must keep the owner");
    verify().unwrap();
    reset_internal_for_test();
}

#[test]
fn owner_proven_store_of_an_old_value_is_dropped_by_the_next_minor() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    // A published nursery makes young edges possible at all.
    young_object(&mut tls, 0);
    let parent = old_object();
    let other = old_object();
    store_unproven_value(parent, other);
    stop();
    // Conservative: the unproven value is old, yet the owner is remembered.
    assert!(remembered(parent));
    verify().unwrap();
    minor();
    assert!(
        !remembered(parent),
        "a minor keeps only owners of young children"
    );
    verify().unwrap();
    reset_internal_for_test();
}

#[test]
fn unresolved_stores_of_distinct_values_into_one_owner_all_survive() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    let old_value = old_object();
    let child = young_object(&mut tls, 5);
    // No slot and no header proofs, like an async frame slot: the old value
    // logged last must not hide the young value logged first.
    for value in [child, old_value] {
        write_barrier_in(
            owner(),
            parent,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            value,
            GcStoreDestination::AsyncFrameSlot as i64,
        );
    }
    unsafe { *parent.cast::<*mut u8>() = child };
    assert_eq!(pending_edges(), 2);
    stop();
    assert!(remembered(parent));
    verify().unwrap();
    reset_internal_for_test();
}

#[test]
fn exact_duplicate_unresolved_stores_log_once() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    let first = young_object(&mut tls, 1);
    let second = young_object(&mut tls, 2);
    for _ in 0..10 {
        for value in [first, second] {
            write_barrier_in(
                owner(),
                parent,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                value,
                OBJECT_FIELD,
            );
        }
    }
    assert_eq!(
        pending_edges(),
        2,
        "alternating repeats are exact duplicates"
    );
    unsafe { *parent.cast::<*mut u8>() = second };
    stop();
    assert!(remembered(parent));
    verify().unwrap();
    reset_internal_for_test();
}

#[test]
fn root_count_is_a_consistent_snapshot_while_logs_flush() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let _guard = isolated_runtime();
    quiet_heap();
    const SLOTS: usize = 3 * ROOT_LOG_CAPACITY + 7;
    const ROUNDS: usize = 200;
    let mut slots = vec![std::ptr::null_mut::<u8>(); SLOTS];
    let addresses: Vec<usize> = slots
        .iter_mut()
        .map(|slot| slot as *mut _ as usize)
        .collect();
    let added = std::sync::Barrier::new(2);
    let removed = std::sync::Barrier::new(2);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (addresses, added, removed) = (&addresses, &added, &removed);
        // Adds on one worker and removes on another, as a task migrating
        // between them would: each side's full log flushes concurrently with
        // the other's pending deltas of the same slots.
        let adder = scope.spawn(crate::runtime::serving(move || {
            let _registration = MutatorRegistration::new();
            for _ in 0..ROUNDS {
                for &slot in addresses {
                    add_runtime_root_slot_in(owner(), slot as *mut *mut u8);
                }
                added.wait();
                removed.wait();
            }
            flush_current_in(owner());
        }));
        let remover = scope.spawn(crate::runtime::serving(move || {
            let _registration = MutatorRegistration::new();
            for _ in 0..ROUNDS {
                added.wait();
                for &slot in addresses {
                    remove_runtime_root_slot_in(owner(), slot as *mut *mut u8);
                }
                removed.wait();
            }
            flush_current_in(owner());
        }));
        let reader = scope.spawn(crate::runtime::serving(|| {
            let mut reads = 0usize;
            while !done.load(Ordering::Acquire) {
                // Before the single snapshot, a flush between the shard sum
                // and the log sum could count a delta twice or not at all,
                // and a split pair could go negative and panic here.
                let count = super::super::root::runtime_root_count_in(&owner().gc);
                assert!(count <= SLOTS, "count {count} exceeds registrations");
                reads += 1;
            }
            reads
        }));
        adder.join().unwrap();
        remover.join().unwrap();
        done.store(true, Ordering::Release);
        assert!(reader.join().unwrap() > 0);
    });
    assert_eq!(super::super::root::runtime_root_count_in(&owner().gc), 0);
    assert!(root_slots().is_empty());
    reset_internal_for_test();
}

#[test]
fn root_count_never_goes_negative_while_migrating_pairs_overlap_reads() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let _guard = isolated_runtime();
    quiet_heap();
    const SLOTS: usize = 3 * ROOT_LOG_CAPACITY + 7;
    const ROUNDS: usize = 1500;
    let mut slots = vec![std::ptr::null_mut::<u8>(); SLOTS];
    let addresses: Vec<usize> = slots
        .iter_mut()
        .map(|slot| slot as *mut _ as usize)
        .collect();
    // Slots added so far, over all rounds; the remover trails it by one slot
    // so adds and removes of the same task's slots overlap on two workers.
    let added = AtomicUsize::new(0);
    let removed = AtomicUsize::new(0);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (addresses, added, removed, done) = (&addresses, &added, &removed, &done);
        let adder = scope.spawn(crate::runtime::serving(move || {
            let _registration = MutatorRegistration::new();
            for round in 0..ROUNDS {
                for &slot in addresses {
                    add_runtime_root_slot_in(owner(), slot as *mut *mut u8);
                    added.fetch_add(1, Ordering::Release);
                }
                // A slot is re-added only after the previous round removed it.
                while removed.load(Ordering::Acquire) < (round + 1) * SLOTS {
                    std::hint::spin_loop();
                }
            }
            flush_current_in(owner());
        }));
        let remover = scope.spawn(crate::runtime::serving(move || {
            let _registration = MutatorRegistration::new();
            for index in 0..ROUNDS * SLOTS {
                while added.load(Ordering::Acquire) <= index {
                    std::hint::spin_loop();
                }
                remove_runtime_root_slot_in(owner(), addresses[index % SLOTS] as *mut *mut u8);
                removed.fetch_add(1, Ordering::Release);
            }
            flush_current_in(owner());
        }));
        // Two readers widen the window in which a read straddles a
        // migrating add/remove pair.
        let reader = move || {
            scope.spawn(crate::runtime::serving(move || {
                let mut reads = 0usize;
                while !done.load(Ordering::Acquire) {
                    // Running logs are read without excluding their appends, so
                    // a read can miss an add whose matching remove it sees (it
                    // must clamp instead of panicking), or see adds whose removes
                    // land after it passed the remover's log. Those removes are
                    // bounded by one log: a full log's flush waits for the shard
                    // locks this read holds.
                    let count = super::super::root::runtime_root_count_in(&owner().gc);
                    assert!(
                        count <= SLOTS + ROOT_LOG_CAPACITY,
                        "count {count} exceeds registrations"
                    );
                    reads += 1;
                }
                reads
            }))
        };
        let readers = [reader(), reader()];
        adder.join().unwrap();
        remover.join().unwrap();
        done.store(true, Ordering::Release);
        for reader in readers {
            assert!(reader.join().unwrap() > 0);
        }
    });
    // Quiescent: every log is flushed, so the count is exact again.
    assert_eq!(super::super::root::runtime_root_count_in(&owner().gc), 0);
    assert!(root_slots().is_empty());
    reset_internal_for_test();
}

#[test]
fn root_count_clamps_a_read_that_misses_a_migrated_add() {
    use std::sync::mpsc::{Receiver, Sender, channel};
    let _guard = isolated_runtime();
    quiet_heap();
    let slot = Box::into_raw(Box::new(std::ptr::null_mut::<u8>())) as usize;
    // A worker owns a log and adds or removes `slot` on command, without
    // taking the coordination or shard locks the reader holds.
    let worker = |commands: Receiver<isize>, acks: Sender<usize>| {
        crate::runtime::serving(move || {
            let _registration = MutatorRegistration::new();
            acks.send(bound_log_address_for_test(&owner().gc).unwrap())
                .unwrap();
            while let Ok(delta) = commands.recv() {
                match delta {
                    1 => add_runtime_root_slot_in(owner(), slot as *mut *mut u8),
                    -1 => remove_runtime_root_slot_in(owner(), slot as *mut *mut u8),
                    _ => flush_current_in(owner()),
                }
                acks.send(0).unwrap();
            }
        })
    };
    std::thread::scope(|scope| {
        let (to_a, a_commands) = channel();
        let (a_acks, from_a) = channel();
        let (to_b, b_commands) = channel();
        let (b_acks, from_b) = channel();
        scope.spawn(worker(a_commands, a_acks));
        scope.spawn(worker(b_commands, b_acks));
        let (log_a, log_b) = (from_a.recv().unwrap(), from_b.recv().unwrap());
        let reader = scope.spawn(crate::runtime::serving(move || {
            let workers = std::rc::Rc::new(RefCell::new(Some([(to_a, from_a), (to_b, from_b)])));
            let straddled = std::rc::Rc::new(Cell::new(false));
            let (hook_workers, hook_straddled) = (workers.clone(), straddled.clone());
            // After the reader passed one worker's log, a task adds the slot
            // on that worker, migrates and removes it on the other: the read
            // misses the add and sees the remove.
            set_after_log_read_for_test(Some(Box::new(move |log| {
                if hook_straddled.get() || (log != log_a && log != log_b) {
                    return;
                }
                hook_straddled.set(true);
                let mut workers = hook_workers.borrow_mut();
                let [a, b] = workers.as_mut().unwrap();
                let (passed, next) = if log == log_a { (a, b) } else { (b, a) };
                passed.0.send(1).unwrap();
                passed.1.recv().unwrap();
                next.0.send(-1).unwrap();
                next.1.recv().unwrap();
            })));
            let straddling = super::super::root::runtime_root_count_in(&owner().gc);
            set_after_log_read_for_test(None);
            assert!(straddled.get(), "the hook placed the migrated pair");
            // Quiescent logs (+1 on one worker, -1 on the other) are exact.
            let quiescent = net_root_slots(&owner().gc);
            for (commands, acks) in workers.borrow_mut().take().unwrap() {
                commands.send(0).unwrap();
                acks.recv().unwrap();
            }
            (straddling, quiescent, net_root_slots(&owner().gc))
        }));
        let (straddling, quiescent, flushed) = reader.join().unwrap();
        assert_eq!(
            straddling, 0,
            "a straddling read clamps instead of panicking"
        );
        assert_eq!(quiescent, 0);
        assert_eq!(flushed, 0);
        assert!(root_slots().is_empty());
    });
    reset_internal_for_test();
    // SAFETY: allocated above; the workers have exited.
    drop(unsafe { Box::from_raw(slot as *mut *mut u8) });
}

#[test]
fn telemetry_snapshot_flushes_pending_edges_after_a_heap_poisoning_panic() {
    let _guard = isolated_runtime();
    quiet_heap();
    let _registration = MutatorRegistration::new();
    let mut tls = tlab_state_for_test();
    let parent = old_object();
    store(parent, young_object(&mut tls, 9));
    assert_eq!(pending_edges(), 1);
    let gc = &owner().gc;
    std::thread::scope(|scope| {
        let panicked = scope
            .spawn(crate::runtime::serving(|| {
                let _heap = gc.heap.lock().unwrap();
                panic!("injected panic while heap is locked");
            }))
            .join();
        assert!(panicked.is_err());
    });
    assert!(gc.heap.is_poisoned());
    // The diagnostic flush applies the edge instead of panicking again.
    let (_, heap) = crate::gc::heap_snapshot::telemetry_heap_snapshot();
    let _ = heap;
    assert_eq!(pending_edges(), 0);
    gc.heap.clear_poison();
    assert!(remembered(parent));
    verify().unwrap();
    reset_internal_for_test();
}
