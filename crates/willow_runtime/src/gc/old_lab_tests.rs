//! Focused old-LAB invariants (willow-jz15.53): unindexed lookups, flushing,
//! allocation across mark closure and a pending sweep, and pruning of buffers
//! owned by exited threads.
use super::*;
use std::time::Instant;

/// Enables buffers for one test and restores the default even on panic.
struct LabsOn;

impl LabsOn {
    fn new() -> Self {
        OLD_LABS_FOR_TEST.store(true, Ordering::Release);
        LabsOn
    }
}

impl Drop for LabsOn {
    fn drop(&mut self) {
        OLD_LABS_FOR_TEST.store(false, Ordering::Release);
    }
}

fn indexed(state: &GcState, payload: *mut u8) -> bool {
    let object = find_old_region_object(state, payload as usize, false).expect("live object");
    let address = object.as_ptr() as usize;
    state.old_regions.iter().any(|region| {
        region.contains(address) && region.allocations.contains_key(&(address - region.start()))
    })
}

#[test]
fn pending_objects_are_found_by_exact_and_interior_lookup_until_flushed() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let _labs = LabsOn::new();
    let objects: Vec<_> = [8i64, 40, 200]
        .iter()
        .map(|&size| willow_alloc(size))
        .collect();
    assert_eq!(local_pending_for_test(), objects.len());
    {
        let state = runtime().heap.lock().unwrap();
        for &payload in &objects {
            assert!(!indexed(&state, payload), "still buffered");
            let exact = find_old_region_object(&state, payload as usize, false).unwrap();
            assert_eq!(exact.payload().as_ptr(), payload);
        }
        // The last object lies past every indexed one: the interior lookup
        // must fall back to the start bits.
        let last = *objects.last().unwrap();
        let interior = find_old_region_object(&state, last as usize + 100, true).unwrap();
        assert_eq!(interior.payload().as_ptr(), last);
        let first = find_old_region_object(&state, objects[0] as usize + 4, true).unwrap();
        assert_eq!(first.payload().as_ptr(), objects[0]);
    }
    {
        let mut state = runtime().heap.lock().unwrap();
        old_lab::flush_all(&mut state);
        for &payload in &objects {
            assert!(indexed(&state, payload));
        }
        let region = state
            .old_regions
            .iter()
            .find(|region| region.contains(objects[0] as usize))
            .unwrap();
        assert_eq!(region.active_labs, 1, "flush keeps the buffer active");
    }
    assert_eq!(local_pending_for_test(), 0);
    let expected: usize = [8usize, 40, 200]
        .iter()
        .map(|size| size + GC_HEADER_SIZE)
        .sum();
    assert_eq!(willow_gc_allocated_bytes() as usize, expected);
    // A following allocation still bumps in the same buffer.
    let next = willow_alloc(8);
    assert_eq!(local_pending_for_test(), 1);
    let (_, last_span) = span_for(200).unwrap();
    assert_eq!(next as usize, objects[2] as usize + last_span);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn buffer_allocations_survive_mark_closure_and_a_pending_sweep() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let _labs = LabsOn::new();
    let mut rooted = willow_alloc(16);
    willow_push_root(&mut rooted);
    let (_, cycle) = root_handshake::begin().expect("no foreign root owner");
    // Allocated black while marking, from the buffer carved before the cycle.
    let during_mark = willow_alloc(24);
    assert!(local_pending_for_test() > 0);
    let mut consumer = cycle.queue.register_assist();
    while cycle.drain_worker(64, &mut Vec::new(), &mut consumer).0 > 0 {}
    drop(consumer);
    let (_, plan) = mark_closure::finish(&cycle, Instant::now()).expect("closes concurrently");
    assert_eq!(local_pending_for_test(), 0, "closure retires every buffer");
    assert!(cycle.is_marked(during_mark as usize));
    // The sweep is pending: a refilled buffer copies `sweep_pending`, and the
    // region visit must index this unmarked, unrooted object before freeing.
    let during_sweep = willow_alloc(32);
    assert_eq!(local_pending_for_test(), 1);
    {
        let state = runtime().heap.lock().unwrap();
        assert!(state.sweeping.is_some());
        assert!(!indexed(&state, during_sweep));
    }
    sweep::concurrent(plan);
    {
        let state = runtime().heap.lock().unwrap();
        for payload in [rooted, during_mark, during_sweep] {
            assert!(indexed(&state, payload), "survived its allocation epoch");
        }
        assert!(
            state
                .old_regions
                .iter()
                .all(|region| region.active_labs == 0)
        );
    }
    let expected = [16usize, 24, 32]
        .iter()
        .map(|size| size + GC_HEADER_SIZE)
        .sum::<usize>();
    assert_eq!(willow_gc_allocated_bytes() as usize, expected);
    // The next cycle frees the two unrooted objects without accounting drift.
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes() as usize, 16 + GC_HEADER_SIZE);
    willow_pop_root();
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}

#[test]
fn exited_thread_buffers_are_indexed_and_pruned() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let _labs = LabsOn::new();
    let mine = willow_alloc(8);
    let theirs = std::thread::spawn(|| willow_alloc(48) as usize)
        .join()
        .unwrap() as *mut u8;
    {
        let mut state = runtime().heap.lock().unwrap();
        assert_eq!(state.old_labs.len(), 2);
        assert!(!indexed(&state, theirs));
        let active: usize = state.old_regions.iter().map(|r| r.active_labs).sum();
        assert_eq!(active, 2);
        old_lab::flush_all(&mut state);
        assert_eq!(state.old_labs.len(), 1, "exited owner's record is dropped");
        assert!(indexed(&state, theirs));
        assert!(indexed(&state, mine));
        let active: usize = state.old_regions.iter().map(|r| r.active_labs).sum();
        assert_eq!(active, 1, "only the live owner's buffer stays active");
    }
    assert_eq!(
        willow_gc_allocated_bytes() as usize,
        8 + 48 + 2 * GC_HEADER_SIZE
    );
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    reset_internal_for_test();
}
