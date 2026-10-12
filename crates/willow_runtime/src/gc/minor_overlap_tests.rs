//! Forced old-epoch/minor overlap, without relying on scheduler timing.
use super::*;
use crate::gc::*;

fn end(cycle: &ConcurrentCycle) {
    GC_MARK_PHASE.store(0, Ordering::Release);
    marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
    cycle.queue.end_epoch();
}

#[test]
fn overlap_graph_copy_promotion_and_snapshot_only_reachability() {
    let _guard = runtime_test_guard();
    for shape in ["chain", "cycle", "fanout", "shared"] {
        for count in [1usize, 16, 64, 256] {
            reset_internal_for_test();
            let mut tls = tlab_state_for_test();
            let nodes: Vec<_> = (0..count)
                .map(|_| willow_gc_alloc_slow(&mut tls, 1, 0, 24, 3))
                .collect();
            for (i, &node) in nodes.iter().enumerate() {
                let children = match shape {
                    "cycle" => [(i + 1) % count, count],
                    "fanout" => [2 * i + 1, 2 * i + 2],
                    "shared" => [i + 1, i + 1],
                    _ => [i + 1, count],
                };
                unsafe {
                    for (slot, child) in children.into_iter().enumerate() {
                        *node.cast::<*mut u8>().add(slot) =
                            nodes.get(child).copied().unwrap_or_default();
                    }
                    *node.cast::<usize>().add(2) = i;
                }
            }
            // A relocation-safe native root exists only for initial marking.
            let mut root = nodes[0];
            let slot = (&mut root as *mut *mut u8).cast::<u8>();
            runtime().runtime_root_slots.add(slot);
            let (_, cycle) = root_handshake::begin().unwrap();
            runtime().runtime_root_slots.remove(slot);
            // The mark queue alone now owns snapshot reachability. Alternate
            // unscanned and fully scanned young graphs before minor relocation.
            if count % 2 == 0 {
                cycle.drain(usize::MAX);
            }
            for pass in 0..2 {
                let before = runtime().heap.lock().unwrap().moved_objects;
                collect_owned();
                assert_eq!(
                    cycle.queue.snapshot().outstanding,
                    count as u64,
                    "each survivor needs exactly one scan job"
                );
                cycle.drain(usize::MAX);
                let state = runtime().heap.lock().unwrap();
                assert_eq!(state.moved_objects - before, count as u64);
                assert_eq!(cycle.objects.marked_count(), count);
                assert!(cycle.queue.snapshot().is_drained());
                assert_eq!(cycle.queue.current_epoch(), cycle.mark_epoch);
                assert_eq!(cycle.queue.snapshot().counter_underflows, 0);
                assert_eq!(state.young_allocated_bytes == 0, pass == 1);
                verify_old_region_metadata(&state).unwrap();
                for object in epoch_objects(&state, &mut Default::default()) {
                    let value = object.payload().as_ptr();
                    assert!(cycle.is_marked(value as usize));
                    assert!(unsafe { *value.cast::<usize>().add(2) } < count);
                    for index in 0..2 {
                        let child = unsafe { *value.cast::<*mut u8>().add(index) };
                        assert!(child.is_null() || cycle.objects.contains(child as usize));
                    }
                }
                eprintln!(
                    "overlap shape={shape} nodes={count} pass={pass} copies={count} live_marks={count}"
                );
            }
            end(&cycle);
        }
    }
    reset_internal_for_test();
}

#[test]
fn overlap_containers_and_async_frame_preserve_rewritten_edges() {
    let _guard = runtime_test_guard();
    for kind in ["array", "map", "channel", "frame"] {
        reset_internal_for_test();
        let mut owner = match kind {
            "array" => crate::array::willow_array_new(1, 1),
            "map" => crate::map::willow_map_new(0, 3, 1),
            "channel" => crate::channel::willow_channel_new(1).cast::<u8>(),
            _ => willow_gc_alloc_layout(2, 0, 8, 1),
        };
        willow_push_root(&mut owner);
        let mut tls = tlab_state_for_test();
        let young = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
        unsafe {
            *young.cast::<i64>() = 987;
        }
        match kind {
            "array" => crate::array::willow_array_set(owner, 0, young as i64),
            "map" => {
                crate::map::willow_map_insert(owner, 7, 0, young as i64, 1);
            }
            "channel" => {
                crate::channel::willow_channel_send_ptr(owner.cast(), young.cast());
            }
            _ => {
                willow_gc_write_barrier(
                    owner,
                    std::ptr::null_mut(),
                    young,
                    GcStoreDestination::ObjectField as i64,
                );
                unsafe {
                    *owner.cast::<*mut u8>() = young;
                }
            }
        }
        let (_, cycle) = root_handshake::begin().unwrap();
        for _ in 0..2 {
            collect_owned();
            cycle.drain(usize::MAX);
        }
        let value = match kind {
            "array" => crate::array::willow_array_get(owner, 0) as *mut u8,
            "map" => {
                let option = crate::map::willow_map_get(owner, 7, 0, 0);
                unsafe { *option.cast::<*mut u8>().add(1) }
            }
            "channel" => crate::channel::willow_channel_recv_ptr(owner.cast()).cast::<u8>(),
            _ => unsafe { *owner.cast::<*mut u8>() },
        };
        assert_ne!(value, young);
        assert_eq!(unsafe { *value.cast::<i64>() }, 987);
        assert!(cycle.is_marked(value as usize));
        assert_eq!(
            HeapObject::from_payload(GcPayload::from_raw(value).unwrap()).generation(),
            GC_GENERATION_OLD
        );
        assert!(runtime().heap.lock().unwrap().remembered_set.is_empty());
        end(&cycle);
        willow_pop_root();
    }
    reset_internal_for_test();
}

#[test]
fn overlap_minor_requests_coalesce_under_major_owner_and_registration_changes() {
    let _guard = runtime_test_guard();
    for participants in [1, 4, 16] {
        reset_internal_for_test();
        PENDING.store(false, std::sync::atomic::Ordering::Release);
        let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
        let (_, cycle) = root_handshake::begin().unwrap();
        let threads: Vec<_> = (0..participants)
            .map(|_| {
                std::thread::spawn(|| {
                    let _mutator = MutatorRegistration::new();
                    minor_collect_internal();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(runtime().heap.lock().unwrap().minor_collections, 0);
        assert!(PENDING.load(std::sync::atomic::Ordering::Acquire));
        service_pending();
        assert_eq!(runtime().heap.lock().unwrap().minor_collections, 1);
        assert!(!PENDING.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(cycle.queue.current_epoch(), cycle.mark_epoch);
        assert!(!cycle.termination.request(cycle.termination.sample(), false));
        end(&cycle);
        drop(elected);
    }
    reset_internal_for_test();
}

#[test]
fn overlap_satb_deletions_parked_mutators_and_old_descendants_survive_sweep() {
    let _guard = runtime_test_guard();
    for participants in [1usize, 4, 16] {
        for closing in [false, true] {
            reset_internal_for_test();
            PENDING.store(false, std::sync::atomic::Ordering::Release);
            let mut tls = tlab_state_for_test();
            let mut owners = Vec::new();
            let mut leaves = Vec::new();
            for index in 0..participants {
                let leaf = willow_gc_alloc_layout(1, 0, 8, 0);
                unsafe {
                    *leaf.cast::<usize>() = index + 1000;
                }
                let young = willow_gc_alloc_slow(&mut tls, 2, 0, 8, 1);
                unsafe {
                    *young.cast::<*mut u8>() = leaf;
                }
                let owner = willow_gc_alloc_layout(3, 0, 8, 1);
                willow_gc_write_barrier(
                    owner,
                    std::ptr::null_mut(),
                    young,
                    GcStoreDestination::ObjectField as i64,
                );
                unsafe {
                    *owner.cast::<*mut u8>() = young;
                }
                owners.push(owner);
                leaves.push(leaf);
            }
            for owner in &mut owners {
                willow_push_root(owner);
            }
            let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
            let (_, cycle) = root_handshake::begin().unwrap();
            cycle.closing.store(closing, Ordering::Release);
            let ready = Arc::new(AtomicUsize::new(0));
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let threads: Vec<_> = owners
                .iter()
                .map(|&owner| {
                    let address = owner as usize;
                    let ready = ready.clone();
                    let done = done.clone();
                    std::thread::spawn(move || {
                        let _mutator = MutatorRegistration::new();
                        let owner = address as *mut u8;
                        let young = unsafe { *owner.cast::<*mut u8>() };
                        willow_gc_write_barrier(
                            owner,
                            young,
                            std::ptr::null_mut(),
                            GcStoreDestination::ObjectField as i64,
                        );
                        unsafe {
                            *owner.cast::<*mut u8>() = std::ptr::null_mut();
                        }
                        minor_collect_internal();
                        ready.fetch_add(1, Ordering::Release);
                        while !done.load(Ordering::Acquire) {
                            willow_gc_safepoint();
                            std::thread::yield_now();
                        }
                    })
                })
                .collect();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while ready.load(Ordering::Acquire) != participants {
                assert!(
                    std::time::Instant::now() < deadline,
                    "minor request was lost"
                );
                std::thread::yield_now();
            }
            service_pending();
            collect_owned();
            cycle.drain(usize::MAX);
            done.store(true, Ordering::Release);
            for thread in threads {
                thread.join().unwrap();
            }
            let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
            sweep::concurrent(plan);
            for (index, leaf) in leaves.into_iter().enumerate() {
                assert_eq!(unsafe { *leaf.cast::<usize>() }, index + 1000);
                assert!(HeapObject::from_payload(GcPayload::from_raw(leaf).unwrap()).allocated());
            }
            assert!(cycle.queue.snapshot().is_drained());
            assert_eq!(cycle.queue.snapshot().counter_underflows, 0);
            assert!(runtime().heap.lock().unwrap().concurrent_cycle.is_none());
            drop(elected);
            willow_pop_roots(participants as i32);
            eprintln!(
                "overlap SATB parked_mutators={participants} closing={closing} old_descendants={participants} sweep=retained"
            );
        }
    }
    reset_internal_for_test();
}

#[test]
fn overlap_native_async_frame_and_marker_pool_pause_resume() {
    use crate::native_frame::{NativeFrameSpec, NativeTaskFrame};
    struct Frame;
    impl NativeFrameSpec for Frame {
        const LAYOUT: willow_abi::NativeFrameLayout<'static> =
            willow_abi::NativeFrameLayout::new(&[willow_abi::SlotKind::GcRef]);
        const NAME: &'static str = "minor overlap";
    }
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let frame = NativeTaskFrame::<Frame>::allocate().unwrap();
    let mut tls = tlab_state_for_test();
    let young = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    unsafe {
        *young.cast::<i64>() = 456;
    }
    frame.store_gc(0, young);
    let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
    let (_, cycle) = root_handshake::begin().unwrap();
    let pool = mark_workers::Pool::new(2).unwrap();
    for _ in 0..2 {
        PENDING.store(true, std::sync::atomic::Ordering::Release);
        pool.run(&cycle, 1024);
        assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
        assert_eq!(cycle.queue.snapshot().active_consumers, 0);
        service_pending();
        assert_eq!(cycle.queue.current_epoch(), cycle.mark_epoch);
        pool.run(&cycle, 1024);
        assert!(cycle.queue.snapshot().is_drained());
        assert!(cycle.is_marked(frame.load_gc(0) as usize));
        assert_eq!(unsafe { *frame.load_gc(0).cast::<i64>() }, 456);
    }
    assert_ne!(frame.load_gc(0), young);
    frame.store_gc(0, std::ptr::null_mut());
    collect_owned();
    let closed = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
    sweep::concurrent(closed.1);
    drop(pool);
    drop(elected);
    reset_internal_for_test();
}

#[test]
fn overlap_phase_requests_and_remark_election_keep_epoch_ownership_distinct() {
    let _guard = runtime_test_guard();
    for phase in ["initial", "concurrent", "remark_request", "remark", "sweep"] {
        reset_internal_for_test();
        let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
        // Initial/remark/sweep stops cannot recursively collect; the same owner
        // services a coalesced request only at its following non-sweep boundary.
        if phase == "initial" {
            PENDING.store(true, Ordering::Release);
        }
        let (_, cycle) = root_handshake::begin().unwrap();
        if phase == "remark_request" {
            assert!(cycle.termination.request(cycle.termination.sample(), true));
            assert!(cycle.termination.requested());
        }
        if phase == "remark" {
            cycle.closing.store(true, Ordering::Release);
        }
        if phase == "sweep" {
            let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
            assert!(runtime().heap.lock().unwrap().sweeping.is_some());
            PENDING.store(true, Ordering::Release);
            assert_eq!(runtime().heap.lock().unwrap().minor_collections, 0);
            sweep::concurrent(plan);
            service_pending();
            assert!(cycle.queue.current_epoch().is_idle());
        } else {
            PENDING.store(true, Ordering::Release);
            service_pending();
            assert_eq!(cycle.queue.current_epoch(), cycle.mark_epoch);
            assert!(!cycle.termination.requested());
            assert_eq!(cycle.satb_shards.is_drained(), phase == "remark");
            end(&cycle);
        }
        assert_eq!(runtime().heap.lock().unwrap().minor_collections, 1);
        assert!(!PENDING.load(Ordering::Acquire));
        drop(elected);
    }
    reset_internal_for_test();
}

#[test]
fn overlap_post_snapshot_tlab_black_allocation_survives_minor_and_old_sweep() {
    let _guard = runtime_test_guard();
    reset_internal_for_test();
    let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
    let (_, cycle) = root_handshake::begin().unwrap();
    let mut tls = tlab_state_for_test();
    let young = willow_gc_alloc_slow(&mut tls, 1, 0, 8, 0);
    unsafe {
        *young.cast::<i64>() = 12345;
    }
    // No user root or insertion barrier: the new TLAB's implicit black color
    // alone must survive its translation into the captured epoch's bitmap.
    collect_owned();
    cycle.drain(usize::MAX);
    let survivor = {
        let state = runtime().heap.lock().unwrap();
        let objects = epoch_objects(&state, &mut Default::default());
        assert_eq!(objects.len(), 1);
        objects[0].payload().as_ptr()
    };
    assert_ne!(survivor, young);
    assert!(cycle.is_marked(survivor as usize));
    let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
    sweep::concurrent(plan);
    assert_eq!(unsafe { *survivor.cast::<i64>() }, 12345);
    drop(elected);
    // Black ownership expires with the old epoch; the next minor may collect it.
    minor_collect_internal();
    assert_eq!(runtime().heap.lock().unwrap().allocated_bytes, 0);
    reset_internal_for_test();
}
