//! Card-granular remembering of large reference-array buffers: a minor scans
//! only the cards written since the previous minor, never the whole buffer.

use super::super::remembered::{ARRAY_CARD_MIN_SLOTS, ARRAY_CARD_SLOTS, card_array_capacity};
use crate::array::{new_old_array, willow_array_get, willow_array_push, willow_array_set};
use crate::gc::*;
use willow_abi::array_layout::{BUFFER_HEADER_WORDS, H_BUF};

fn quiet_heap() {
    reset_internal_for_test();
    let mut state = runtime().heap.lock().unwrap();
    state.threshold_bytes = usize::MAX;
    state.pacer_trigger = u64::MAX;
    state.pacer.disable_for_test();
    state.nursery_threshold_bytes = usize::MAX;
}

fn minor() -> crate::gc_telemetry::MarkWork {
    with_stw(
        crate::gc_telemetry::stops::StopReason::Minor,
        super::collect_stopped,
    )
    .expect("minor collection ran")
    .2
}

fn buffer_of(array: *mut u8) -> usize {
    unsafe { *array.cast::<usize>().add(H_BUF) }
}

fn slot_of(buffer: usize, index: usize) -> *mut u8 {
    (buffer + (BUFFER_HEADER_WORDS + index) * GC_STORAGE_WORD_BYTES) as *mut u8
}

fn young(tls: &mut GcTlabState, value: u64) -> *mut u8 {
    let object = willow_gc_alloc_slow(tls, 1, 0, 8, 0);
    unsafe { *object.cast::<u64>() = value };
    object
}

fn element_value(array: *mut u8, index: i64) -> u64 {
    let child = willow_array_get(array, index) as *const u64;
    assert!(!child.is_null());
    unsafe { *child }
}

fn remembered_flag(buffer: usize) -> u8 {
    unsafe { (*payload_to_header(buffer as *mut u8)).remembered }
}

fn generation(object: usize) -> u8 {
    unsafe { (*payload_to_header(object as *mut u8)).generation }
}

fn recorded_cards(buffer: usize) -> Option<Vec<usize>> {
    let state = runtime().heap.lock().unwrap();
    state.remembered_set.owner_cards(buffer).map(|cards| {
        let mut cards: Vec<_> = cards.iter().copied().collect();
        cards.sort_unstable();
        cards
    })
}

fn verify() -> Result<(), String> {
    let state = runtime().heap.lock().unwrap();
    let registry = type_registry().lock().unwrap();
    verify_remembered_set(&state, &registry)
}

fn hits() -> u64 {
    runtime().heap.lock().unwrap().write_barrier_hits
}

#[test]
fn card_threshold_selects_only_large_reference_buffers() {
    let _guard = runtime_test_guard();
    quiet_heap();
    for (len, expected) in [
        (0, false),
        (ARRAY_CARD_SLOTS as i64, false),
        (ARRAY_CARD_MIN_SLOTS as i64 - 1, false),
        (ARRAY_CARD_MIN_SLOTS as i64, true),
        (100_000, true),
    ] {
        let array = new_old_array(len, 1);
        let buffer = buffer_of(array);
        let object = HeapObject::from_payload(GcPayload::from_raw(buffer as *mut u8).unwrap());
        assert_eq!(card_array_capacity(object).is_some(), expected, "len={len}");
    }
    // Scalar buffers never contain references.
    let scalar = new_old_array(100_000, 0);
    let object =
        HeapObject::from_payload(GcPayload::from_raw(buffer_of(scalar) as *mut u8).unwrap());
    assert!(card_array_capacity(object).is_none());
    reset_internal_for_test();
}

#[test]
fn minor_card_slots_stay_flat_as_old_array_grows() {
    let _guard = runtime_test_guard();
    let mut table = Vec::new();
    for len in [1024usize, 16_384, 262_144] {
        quiet_heap();
        let mut array = new_old_array(len as i64, 1);
        willow_push_root(&mut array);
        let buffer = buffer_of(array);
        assert_eq!(generation(buffer), GC_GENERATION_OLD);
        let mut tls = tlab_state_for_test();
        let indices = [0usize, len / 2, len - 1];
        let calls_before = runtime()
            .write_barrier_calls
            .load(std::sync::atomic::Ordering::Relaxed);
        for (n, &index) in indices.iter().enumerate() {
            willow_array_set(array, index as i64, young(&mut tls, 100 + n as u64) as i64);
        }
        // Every young store reaches the barrier (no whole-owner flag), but
        // the owner enters the remembered set once.
        assert_eq!(
            runtime()
                .write_barrier_calls
                .load(std::sync::atomic::Ordering::Relaxed)
                - calls_before,
            3
        );
        assert_eq!(hits(), 1);
        assert_eq!(willow_gc_remembered_set_size(), 1);
        assert_eq!(remembered_flag(buffer), 0);
        let cards: Vec<_> = indices.iter().map(|i| i / ARRAY_CARD_SLOTS).collect();
        assert_eq!(recorded_cards(buffer).unwrap(), cards);
        verify().unwrap();

        let work = minor();
        assert_eq!(work.remembered_card_slots, (3 * ARRAY_CARD_SLOTS) as u64);
        assert_eq!(work.remembered_owners, 1);
        for (n, &index) in indices.iter().enumerate() {
            assert_eq!(element_value(array, index as i64), 100 + n as u64);
        }
        // Survivors stay young: the same cards are recorded again.
        verify().unwrap();
        let mut minors = 1;
        while recorded_cards(buffer).is_some() {
            assert_eq!(recorded_cards(buffer).unwrap(), cards);
            let work = minor();
            assert!(work.remembered_card_slots <= (3 * ARRAY_CARD_SLOTS) as u64);
            minors += 1;
            assert!(minors < 32, "young children never tenured");
        }
        // Tenured children leave nothing to scan.
        assert_eq!(willow_gc_remembered_set_size(), 0);
        assert_eq!(minor().remembered_card_slots, 0);
        for (n, &index) in indices.iter().enumerate() {
            assert_eq!(element_value(array, index as i64), 100 + n as u64);
            let child = willow_array_get(array, index as i64) as usize;
            assert_eq!(generation(child), GC_GENERATION_OLD);
        }
        table.push((len, 3 * ARRAY_CARD_SLOTS, minors));
        willow_pop_root();
    }
    for (len, slots, minors) in &table {
        eprintln!(
            "card_remembered len={len} scanned_slots_per_minor={slots} minors_to_tenure={minors}"
        );
    }
    assert!(table.iter().all(|row| row.1 == table[0].1));
    reset_internal_for_test();
}

#[test]
fn small_reference_buffers_keep_whole_owner_remembering() {
    let _guard = runtime_test_guard();
    quiet_heap();
    let mut array = new_old_array(ARRAY_CARD_SLOTS as i64, 1);
    willow_push_root(&mut array);
    let buffer = buffer_of(array);
    let mut tls = tlab_state_for_test();
    willow_array_set(array, 3, young(&mut tls, 7) as i64);
    assert_eq!(remembered_flag(buffer), 1);
    assert!(recorded_cards(buffer).is_none());
    assert_eq!(willow_gc_remembered_set_size(), 1);
    let calls = runtime()
        .write_barrier_calls
        .load(std::sync::atomic::Ordering::Relaxed);
    willow_array_set(array, 4, young(&mut tls, 8) as i64);
    assert_eq!(
        runtime()
            .write_barrier_calls
            .load(std::sync::atomic::Ordering::Relaxed),
        calls,
        "a whole-remembered owner skips the barrier"
    );
    let work = minor();
    assert_eq!(work.remembered_card_slots, 0);
    assert_eq!(element_value(array, 3), 7);
    assert_eq!(element_value(array, 4), 8);
    verify().unwrap();
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn repeated_stores_into_one_card_record_it_once() {
    let _guard = runtime_test_guard();
    quiet_heap();
    let mut array = new_old_array(4096, 1);
    willow_push_root(&mut array);
    let buffer = buffer_of(array);
    let mut tls = tlab_state_for_test();
    for index in 0..ARRAY_CARD_SLOTS {
        willow_array_set(array, index as i64, young(&mut tls, index as u64) as i64);
    }
    assert_eq!(recorded_cards(buffer).unwrap(), vec![0]);
    assert_eq!(hits(), 1);
    // Another card of the same owner adds a card, not an owner.
    willow_array_set(array, ARRAY_CARD_SLOTS as i64, young(&mut tls, 99) as i64);
    assert_eq!(recorded_cards(buffer).unwrap(), vec![0, 1]);
    assert_eq!(hits(), 1);
    assert_eq!(minor().remembered_card_slots, (2 * ARRAY_CARD_SLOTS) as u64);
    for index in 0..ARRAY_CARD_SLOTS {
        assert_eq!(element_value(array, index as i64), index as u64);
    }
    assert_eq!(element_value(array, ARRAY_CARD_SLOTS as i64), 99);
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn barrier_dedup_cache_does_not_survive_a_minor_that_dropped_the_card() {
    let _guard = runtime_test_guard();
    quiet_heap();
    let mut array = new_old_array(4096, 1);
    willow_push_root(&mut array);
    let buffer = buffer_of(array);
    let mut tls = tlab_state_for_test();
    let old_value = willow_alloc_typed(8, 0);
    willow_array_set(array, 5, young(&mut tls, 1) as i64);
    // The young child is overwritten by an old value; the minor finds no
    // young child in card 0 and forgets it.
    willow_array_set(array, 5, old_value as i64);
    minor();
    assert!(recorded_cards(buffer).is_none());
    assert_eq!(willow_gc_remembered_set_size(), 0);
    // This thread cached (buffer, card 0) before the minor. The new epoch
    // must force the store back to the locked path.
    willow_array_set(array, 6, young(&mut tls, 66) as i64);
    assert_eq!(recorded_cards(buffer).unwrap(), vec![0]);
    verify().unwrap();
    minor();
    assert_eq!(element_value(array, 6), 66);
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn growth_into_large_old_buffer_records_every_young_card() {
    let _guard = runtime_test_guard();
    quiet_heap();
    let mut array = crate::array::willow_array_new(0, 1);
    willow_push_root(&mut array);
    let mut tls = tlab_state_for_test();
    let count = 1000usize;
    for index in 0..count {
        willow_array_push(array, young(&mut tls, index as u64) as i64);
    }
    let buffer = buffer_of(array);
    assert_eq!(generation(buffer), GC_GENERATION_OLD);
    assert_eq!(remembered_flag(buffer), 0);
    assert_eq!(
        recorded_cards(buffer).unwrap(),
        (0..count.div_ceil(ARRAY_CARD_SLOTS)).collect::<Vec<_>>()
    );
    verify().unwrap();
    // Earlier old buffers abandoned by growth (256 and 512 slots, fully
    // written) keep their cards until a major reclaims them; together they
    // stay below the current buffer's size.
    let work = minor();
    assert_eq!(work.remembered_card_slots, (256 + 512 + count) as u64);
    for index in 0..count {
        assert_eq!(element_value(array, index as i64), index as u64);
    }
    verify().unwrap();
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn slotless_barrier_remembers_whole_buffer_until_minor_converts_it() {
    let _guard = runtime_test_guard();
    quiet_heap();
    let mut array = new_old_array(4096, 1);
    willow_push_root(&mut array);
    let buffer = buffer_of(array);
    let mut tls = tlab_state_for_test();
    let child = young(&mut tls, 41);
    let flags = GcStoreDestination::ArrayElement as i64
        | willow_abi::GC_STORE_VALUE_HAS_HEADER
        | willow_abi::GC_STORE_OWNER_HAS_HEADER;
    willow_gc_write_barrier(buffer as *mut u8, std::ptr::null_mut(), child, flags);
    unsafe { store_gc_reference(slot_of(buffer, 3000).cast(), child) };
    assert_eq!(remembered_flag(buffer), 1);
    assert!(recorded_cards(buffer).is_none());
    verify().unwrap();
    // The whole scan converts the buffer to card remembering.
    let work = minor();
    assert_eq!(work.remembered_card_slots, 0);
    assert_eq!(remembered_flag(buffer), 0);
    assert_eq!(
        recorded_cards(buffer).unwrap(),
        vec![3000 / ARRAY_CARD_SLOTS]
    );
    assert_eq!(element_value(array, 3000), 41);
    verify().unwrap();
    assert_eq!(minor().remembered_card_slots, ARRAY_CARD_SLOTS as u64);
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn array_barrier_with_foreign_slot_falls_back_to_whole_owner() {
    let _guard = runtime_test_guard();
    quiet_heap();
    let mut array = new_old_array(4096, 1);
    willow_push_root(&mut array);
    let buffer = buffer_of(array);
    let mut tls = tlab_state_for_test();
    let flags = GcStoreDestination::ArrayElement as i64
        | willow_abi::GC_STORE_VALUE_HAS_HEADER
        | willow_abi::GC_STORE_OWNER_HAS_HEADER;
    // The length word and a slot past capacity are not element slots.
    for slot in [buffer as *mut u8, slot_of(buffer, 1 << 20)] {
        willow_gc_array_write_barrier(
            buffer as *mut u8,
            slot,
            std::ptr::null_mut(),
            young(&mut tls, 1),
            flags,
        );
        assert_eq!(remembered_flag(buffer), 1);
        assert!(recorded_cards(buffer).is_none());
    }
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn generated_array_barrier_records_slot_card() {
    let _guard = runtime_test_guard();
    quiet_heap();
    let mut array = new_old_array(4096, 1);
    willow_push_root(&mut array);
    let buffer = buffer_of(array);
    let mut tls = tlab_state_for_test();
    let child = young(&mut tls, 12);
    let slot = slot_of(buffer, 1234);
    willow_gc_array_write_barrier(
        buffer as *mut u8,
        slot,
        std::ptr::null_mut(),
        child,
        GcStoreDestination::ArrayElement as i64
            | willow_abi::GC_STORE_VALUE_HAS_HEADER
            | willow_abi::GC_STORE_OWNER_HAS_HEADER,
    );
    unsafe { store_gc_reference(slot.cast(), child) };
    assert_eq!(
        recorded_cards(buffer).unwrap(),
        vec![1234 / ARRAY_CARD_SLOTS]
    );
    assert_eq!(remembered_flag(buffer), 0);
    minor();
    assert_eq!(element_value(array, 1234), 12);
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn verifier_rejects_young_child_in_unrecorded_card() {
    let _guard = runtime_test_guard();
    quiet_heap();
    let mut array = new_old_array(4096, 1);
    willow_push_root(&mut array);
    let buffer = buffer_of(array);
    let mut tls = tlab_state_for_test();
    willow_array_set(array, 0, young(&mut tls, 1) as i64);
    verify().unwrap();
    // A store that bypassed the barrier leaves card 3 unrecorded.
    let stray = young(&mut tls, 2);
    unsafe { store_gc_reference(slot_of(buffer, 200).cast(), stray) };
    let error = verify().unwrap_err();
    assert!(error.contains("unrecorded card 3"), "{error}");
    unsafe { store_gc_reference(slot_of(buffer, 200).cast(), std::ptr::null_mut()) };
    verify().unwrap();
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn compiled_array_stores_without_value_header_hit_card_cache() {
    use super::super::barrier::CARD_CACHE_HITS;
    let _guard = runtime_test_guard();
    quiet_heap();
    let mut array = new_old_array(4096, 1);
    willow_push_root(&mut array);
    let buffer = buffer_of(array);
    let mut tls = tlab_state_for_test();
    // Generated `xs[i] = v` passes only the owner-header proof.
    let flags = GcStoreDestination::ArrayElement as i64 | willow_abi::GC_STORE_OWNER_HAS_HEADER;
    let store = |tls: &mut GcTlabState, index: usize, value: u64| {
        let child = young(tls, value);
        let slot = slot_of(buffer, index);
        willow_gc_array_write_barrier(buffer as *mut u8, slot, std::ptr::null_mut(), child, flags);
        unsafe { store_gc_reference(slot.cast(), child) };
    };
    let hits_before = CARD_CACHE_HITS.get();
    store(&mut tls, 2 * ARRAY_CARD_SLOTS, 0);
    assert_eq!(
        CARD_CACHE_HITS.get(),
        hits_before,
        "first store takes the lock"
    );
    assert_eq!(hits(), 1);
    let repeats = ARRAY_CARD_SLOTS - 1;
    for offset in 1..ARRAY_CARD_SLOTS {
        store(&mut tls, 2 * ARRAY_CARD_SLOTS + offset, offset as u64);
    }
    assert_eq!(
        CARD_CACHE_HITS.get() - hits_before,
        repeats as u64,
        "repeat stores into the recorded card skip the heap lock"
    );
    assert_eq!(hits(), 1);
    assert_eq!(recorded_cards(buffer).unwrap(), vec![2]);
    verify().unwrap();
    // A minor advances the epoch: the next store relocks and re-records.
    minor();
    let after_minor = CARD_CACHE_HITS.get();
    store(&mut tls, 2 * ARRAY_CARD_SLOTS, 99);
    assert_eq!(CARD_CACHE_HITS.get(), after_minor);
    assert_eq!(recorded_cards(buffer).unwrap(), vec![2]);
    verify().unwrap();
    minor();
    assert_eq!(element_value(array, 2 * ARRAY_CARD_SLOTS as i64), 99);
    for offset in 1..ARRAY_CARD_SLOTS {
        assert_eq!(
            element_value(array, (2 * ARRAY_CARD_SLOTS + offset) as i64),
            offset as u64
        );
    }
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn runtime_array_card_filter_counts_cards_instead_of_stores() {
    use super::super::barrier::CARD_CACHE_HITS;
    let _guard = runtime_test_guard();
    for count in [64usize, 128, 512, 1024] {
        quiet_heap();
        let mut array = new_old_array(4096, 1);
        willow_push_root(&mut array);
        let mut tls = tlab_state_for_test();
        let child = young(&mut tls, 42);
        let before = runtime().write_barrier_calls.load(Ordering::Relaxed);
        let cached = CARD_CACHE_HITS.get();
        for index in 0..count {
            willow_array_set(array, index as i64, child as i64);
        }
        let cards = count.div_ceil(ARRAY_CARD_SLOTS) as u64;
        assert_eq!(
            runtime().write_barrier_calls.load(Ordering::Relaxed) - before,
            cards
        );
        assert_eq!(CARD_CACHE_HITS.get() - cached, count as u64 - cards);
        verify().unwrap();
        assert_eq!(minor().remembered_card_slots, count as u64);
        for index in 0..count {
            assert_eq!(element_value(array, index as i64), 42);
        }
        willow_pop_root();
    }
    reset_internal_for_test();
}

#[test]
fn runtime_cached_card_still_publishes_during_active_marking() {
    let _guard = runtime_test_guard();
    quiet_heap();
    let mut array = new_old_array(4096, 1);
    willow_push_root(&mut array);
    let mut tls = tlab_state_for_test();
    let child = young(&mut tls, 42);
    willow_array_set(array, 0, child as i64);
    let (_, cycle) = root_handshake::begin().expect("no foreign root owner");
    let before = runtime().write_barrier_calls.load(Ordering::Relaxed);
    willow_array_set(array, 0, 0);
    assert_eq!(
        runtime().write_barrier_calls.load(Ordering::Relaxed),
        before + 1
    );
    let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now())
        .expect("native array tracing closes concurrently");
    assert!(cycle.is_marked(child as usize));
    sweep::concurrent(plan);
    assert_eq!(willow_array_get(array, 0), 0);
    willow_pop_root();
    reset_internal_for_test();
}

#[test]
fn reference_array_growth_barriers_scale_with_cards() {
    let _guard = runtime_test_guard();
    for count in [512usize, 1024, 2048, 4096] {
        quiet_heap();
        let mut array = new_old_array(count as i64, 1);
        willow_push_root(&mut array);
        let mut tls = tlab_state_for_test();
        let child = young(&mut tls, 42);
        for index in 0..count {
            willow_array_set(array, index as i64, child as i64);
        }
        let before = runtime().write_barrier_calls.load(Ordering::Relaxed);
        willow_array_push(array, child as i64);
        let calls = runtime().write_barrier_calls.load(Ordering::Relaxed) - before;
        assert!(calls <= count.div_ceil(ARRAY_CARD_SLOTS) as u64 + 2);
        verify().unwrap();
        minor();
        for index in 0..=count {
            assert_eq!(element_value(array, index as i64), 42);
        }
        willow_pop_root();
    }
    reset_internal_for_test();
}
