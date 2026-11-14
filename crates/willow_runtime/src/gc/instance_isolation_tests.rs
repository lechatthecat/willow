//! Two runtime instances driven at the same time on separate threads keep
//! independent heaps, collection counts, descriptor counts and barrier gates.
//! Each side uses a different object count, so any shared state shows up as
//! the sum of both sides or as a cross-runtime stop that never completes.
use super::*;
use std::sync::Barrier;

const ROUNDS: u64 = 3;
const KEY: willow_abi::GcLayoutDescriptor = willow_abi::GcLayoutDescriptor {
    type_id: 0,
    layout_id: 0x1505_1A7E,
    gc_ref_mask: 0,
    size: (GC_HEADER_SIZE + 8) as u64,
};

#[derive(Debug, PartialEq)]
struct Gates {
    runtime: usize,
    mark_phase: usize,
    card_epoch: usize,
    card_epoch_value: u64,
}

fn gates() -> Gates {
    let owner = crate::runtime::current();
    Gates {
        runtime: std::ptr::from_ref(owner) as usize,
        mark_phase: std::ptr::from_ref(owner.mark_phase()) as usize,
        card_epoch: std::ptr::from_ref(owner.card_epoch()) as usize,
        card_epoch_value: owner.card_epoch().load(Ordering::Acquire),
    }
}

/// Every phase is separated by a barrier, so each runtime allocates, stores
/// and collects while the other one is doing the same.
fn drive(live: usize, garbage: usize, stores: usize, rendezvous: &Barrier) -> Gates {
    let _runtime = isolated_runtime();
    reset_internal_for_test();
    runtime().heap.lock().unwrap().threshold_bytes = usize::MAX;
    invalidate_runtime_policy_budgets();
    let _mutator = MutatorRegistration::new();
    let mut parent = willow_alloc_typed(8, 1);
    willow_push_root(&mut parent);
    let mut roots = Vec::with_capacity(live);
    for _ in 0..live {
        roots.push(willow_gc_alloc_layout(KEY.layout_id, 0, 8, 0));
    }
    for slot in &mut roots {
        willow_push_root(slot);
    }
    rendezvous.wait();
    for round in 1..=ROUNDS {
        for _ in 0..garbage {
            willow_gc_alloc_layout(KEY.layout_id, 0, 8, 0);
        }
        let barrier_calls = runtime().write_barrier_calls.load(Ordering::Relaxed);
        rendezvous.wait();
        for index in 0..stores {
            let value = roots[index % live];
            willow_gc_write_barrier(
                parent,
                std::ptr::null_mut(),
                value,
                GcStoreDestination::ObjectField as i64,
            );
        }
        // Both collections overlap; a shared stop would wait forever for the
        // other runtime's mutator, which is blocked in this barrier.
        willow_gc_collect();
        rendezvous.wait();
        assert_eq!(
            runtime().write_barrier_calls.load(Ordering::Relaxed) - barrier_calls,
            stores as u64,
            "live={live}"
        );
        assert_eq!(layouts::references(KEY), live, "live={live}");
        assert_eq!(
            willow_gc_allocated_bytes(),
            ((live + 1) * (GC_HEADER_SIZE + 8)) as i64,
            "live={live}"
        );
        assert_eq!(
            runtime().heap.lock().unwrap().major_collections,
            round,
            "live={live}"
        );
        rendezvous.wait();
    }
    let observed = gates();
    willow_pop_roots(live as i32 + 1);
    drop(_mutator);
    reset_internal_for_test();
    assert_eq!(layouts::references(KEY), 0);
    observed
}

#[test]
fn concurrent_runtime_instances_collect_without_shared_heap_or_gates() {
    let rendezvous = Barrier::new(2);
    let (first, second) = std::thread::scope(|scope| {
        let first = scope.spawn(|| drive(3, 100, 5, &rendezvous));
        let second = scope.spawn(|| drive(40, 7, 50, &rendezvous));
        (first.join().unwrap(), second.join().unwrap())
    });
    assert_ne!(first.runtime, second.runtime);
    assert_ne!(first.mark_phase, second.mark_phase);
    assert_ne!(first.card_epoch, second.card_epoch);
    // Thread card caches outlive a switch of the served runtime, so epoch
    // values of different runtimes never coincide either.
    assert_ne!(first.card_epoch_value, second.card_epoch_value);
    // Neither fixture instance uses the exported default-runtime gates.
    let default = crate::runtime::process_default();
    for side in [&first, &second] {
        assert_ne!(side.runtime, std::ptr::from_ref(default) as usize);
        assert_ne!(
            side.card_epoch,
            std::ptr::from_ref(default.card_epoch()) as usize
        );
        assert_ne!(
            side.card_epoch_value,
            default.card_epoch().load(Ordering::Acquire)
        );
    }
}
