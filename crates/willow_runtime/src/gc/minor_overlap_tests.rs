//! Forced old-epoch/minor overlap, without relying on scheduler timing.
use super::*;
use crate::gc::*;

fn end(cycle: &ConcurrentCycle) {
    crate::runtime::current()
        .mark_phase()
        .store(0, Ordering::Release);
    marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
    cycle.queue.end_epoch();
}

#[test]
fn overlap_graph_copy_promotion_and_snapshot_only_reachability() {
    let _guard = isolated_runtime();
    for shape in ["chain", "cycle", "fanout", "shared"] {
        for count in [1usize, 16, 64, 256] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { reset_internal_for_test() };
            let tls = tlab_state_for_test();
            let nodes: Vec<_> = (0..count)
                // SAFETY: The fixture owns the stable TLAB through unregistration. The
                // requested size and tracing metadata describe the object initialized here.
                .map(|_| unsafe { willow_gc_alloc_slow(tls.as_ptr(), 1, 0, 24, 3) })
                .collect();
            for (i, &node) in nodes.iter().enumerate() {
                let children = match shape {
                    "cycle" => [(i + 1) % count, count],
                    "fanout" => [2 * i + 1, 2 * i + 2],
                    "shared" => [i + 1, i + 1],
                    _ => [i + 1, count],
                };
                // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the
                // accessed words.
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
                // A drain publishes its partial branch batch on return. A
                // fan-out graph may therefore need more than one drain to
                // establish this fixture's fully scanned snapshot.
                for _ in 0..count {
                    if cycle.queue.snapshot().is_drained() {
                        break;
                    }
                    cycle.drain(usize::MAX);
                }
                assert!(cycle.queue.snapshot().is_drained());
                assert_eq!(cycle.objects.marked_count(), count);
            }
            for pass in 0..2 {
                let before = runtime().heap.lock().unwrap().moved_objects;
                collect_owned();
                assert_eq!(
                    cycle.queue.snapshot().outstanding,
                    u64::from(count == 1 && pass == 0),
                    "only the previously queued root retains a trace job"
                );
                let mut queued = std::collections::HashSet::new();
                cycle.queue.remap_stopped(|object| {
                    assert!(
                        queued.insert(object.addr()),
                        "duplicate survivor publication"
                    );
                    Some(object)
                });
                assert_eq!(queued.len(), usize::from(count == 1 && pass == 0));
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
                    // SAFETY: The GC allocator word-aligns this payload and the fixture reserves
                    // the accessed words.
                    assert!(unsafe { *value.cast::<usize>().add(2) } < count);
                    for index in 0..2 {
                        // SAFETY: The GC allocator word-aligns this payload and the fixture
                        // reserves the accessed words.
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
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn overlap_containers_and_async_frame_preserve_rewritten_edges() {
    let _guard = isolated_runtime();
    for kind in ["array", "map", "channel", "frame"] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let mut owner = match kind {
            // SAFETY: Flag 1 allocates full-width reference slots.
            "array" => unsafe { crate::array::willow_array_new(1, 1) },
            "map" => crate::map::willow_map_new(0, 3, 1),
            "channel" => crate::channel::willow_channel_new(1).cast::<u8>(),
            // SAFETY: The requested layout describes the initialized words of this object and
            // its type’s tracing or finalization contract.
            _ => unsafe { willow_gc_alloc_layout(2, 0, 8, 1) },
        };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut owner) };
        let tls = tlab_state_for_test();
        // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size
        // and tracing metadata describe the object initialized here.
        let young = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 1, 0, 8, 0) };
        // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
        unsafe {
            *young.cast::<i64>() = 987;
        }
        match kind {
            // SAFETY: The destination is the live fixture array; the element word matches its
            // declared storage and tracing layout.
            "array" => unsafe { crate::array::willow_array_set(owner, 0, young as i64) },
            "map" => {
                // SAFETY: The constructor and reference flags agree. Each pointer word comes
                // from a live fixture object at the call.
                unsafe { crate::map::willow_map_insert(owner, 7, 0, young as i64, 1) };
            }
            "channel" => {
                // SAFETY: The constructor selected this channel element representation and
                // the channel remains live during the send.
                unsafe { crate::channel::willow_channel_send_ptr(owner.cast(), young.cast()) };
            }
            _ => {
                // SAFETY: The destination describes this reference update. Any asserted
                // header belongs to its live GC payload.
                unsafe {
                    willow_gc_write_barrier(
                        owner,
                        std::ptr::null_mut(),
                        young,
                        GcStoreDestination::ObjectField as i64,
                    )
                };
                // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the
                // accessed words.
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
            // SAFETY: The fixture retains the array allocated for these accesses.
            "array" => (unsafe { crate::array::willow_array_get(owner, 0) }) as *mut u8,
            "map" => {
                // SAFETY: The live map has scalar keys and this lookup borrows no pointer-
                // valued key.
                let option = unsafe { crate::map::willow_map_get(owner, 7, 0, 0) };
                // SAFETY: The fixture allocates these fields in word-aligned GC storage at
                // whole-word offsets.
                unsafe { *option.cast::<*mut u8>().add(1) }
            }
            "channel" => {
                // SAFETY: The handle is null or the live channel retained for this operation.
                unsafe { crate::channel::willow_channel_recv_ptr(owner.cast()) }.cast::<u8>()
            }
            // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the
            // accessed words.
            _ => unsafe { *owner.cast::<*mut u8>() },
        };
        assert_ne!(value, young);
        if kind == "channel" {
            // A send publishes its young value; the minor then promotes the
            // copy, which keeps the shared mark.
            assert!(HeapObject::from_payload(GcPayload::from_raw(value).unwrap()).shared());
        }
        // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
        assert_eq!(unsafe { *value.cast::<i64>() }, 987);
        assert!(cycle.is_marked(value as usize));
        assert_eq!(
            HeapObject::from_payload(GcPayload::from_raw(value).unwrap()).generation(),
            GC_GENERATION_OLD
        );
        assert!(runtime().heap.lock().unwrap().remembered_set.is_empty());
        end(&cycle);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_root() };
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn overlap_minor_requests_coalesce_under_major_owner_and_registration_changes() {
    let _guard = isolated_runtime();
    for participants in [1, 4, 16] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        runtime()
            .minor_pending
            .store(false, std::sync::atomic::Ordering::Release);
        let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
        let (_, cycle) = root_handshake::begin().unwrap();
        let threads: Vec<_> = (0..participants)
            .map(|_| {
                std::thread::spawn(crate::runtime::serving(|| {
                    let _mutator = MutatorRegistration::new();
                    minor_collect_internal();
                }))
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(runtime().heap.lock().unwrap().minor_collections, 0);
        assert!(
            runtime()
                .minor_pending
                .load(std::sync::atomic::Ordering::Acquire)
        );
        service_pending();
        assert_eq!(runtime().heap.lock().unwrap().minor_collections, 1);
        assert!(
            !runtime()
                .minor_pending
                .load(std::sync::atomic::Ordering::Acquire)
        );
        assert_eq!(cycle.queue.current_epoch(), cycle.mark_epoch);
        assert!(!cycle.termination.request(cycle.termination.sample(), false));
        end(&cycle);
        drop(elected);
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn overlap_satb_deletions_parked_mutators_and_old_descendants_survive_sweep() {
    let _guard = isolated_runtime();
    for participants in [1usize, 4, 16] {
        for closing in [false, true] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { reset_internal_for_test() };
            runtime()
                .minor_pending
                .store(false, std::sync::atomic::Ordering::Release);
            let tls = tlab_state_for_test();
            let mut owners = Vec::new();
            let mut leaves = Vec::new();
            for index in 0..participants {
                // SAFETY: The requested layout describes the initialized words of this object
                // and its type’s tracing or finalization contract.
                let leaf = unsafe { willow_gc_alloc_layout(1, 0, 8, 0) };
                // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar
                // value.
                unsafe {
                    *leaf.cast::<usize>() = index + 1000;
                }
                // SAFETY: The fixture owns the stable TLAB through unregistration. The
                // requested size and tracing metadata describe the object initialized here.
                let young = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 2, 0, 8, 1) };
                // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the
                // accessed words.
                unsafe {
                    *young.cast::<*mut u8>() = leaf;
                }
                // SAFETY: The requested layout describes the initialized words of this object
                // and its type’s tracing or finalization contract.
                let owner = unsafe { willow_gc_alloc_layout(3, 0, 8, 1) };
                // SAFETY: The destination describes this reference update. Any asserted
                // header belongs to its live GC payload.
                unsafe {
                    willow_gc_write_barrier(
                        owner,
                        std::ptr::null_mut(),
                        young,
                        GcStoreDestination::ObjectField as i64,
                    )
                };
                // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the
                // accessed words.
                unsafe {
                    *owner.cast::<*mut u8>() = young;
                }
                owners.push(owner);
                leaves.push(leaf);
            }
            for owner in &mut owners {
                // SAFETY: The slot storage stays live and stationary through the matching
                // pop, including during collection.
                unsafe { willow_push_root(owner) };
            }
            let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
            let (_, cycle) = root_handshake::begin().unwrap();
            if closing {
                cycle.close();
            }
            let ready = Arc::new(AtomicUsize::new(0));
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let threads: Vec<_> = owners
                .iter()
                .map(|&owner| {
                    let address = owner as usize;
                    let ready = ready.clone();
                    let done = done.clone();
                    std::thread::spawn(crate::runtime::serving(move || {
                        let _mutator = MutatorRegistration::new();
                        let owner = address as *mut u8;
                        // SAFETY: The GC allocator word-aligns this payload and the fixture
                        // reserves the accessed words.
                        let young = unsafe { *owner.cast::<*mut u8>() };
                        // SAFETY: The destination describes this reference update. Any
                        // asserted header belongs to its live GC payload.
                        unsafe {
                            willow_gc_write_barrier(
                                owner,
                                young,
                                std::ptr::null_mut(),
                                GcStoreDestination::ObjectField as i64,
                            )
                        };
                        // SAFETY: The GC allocator word-aligns this payload and the fixture
                        // reserves the accessed words.
                        unsafe {
                            *owner.cast::<*mut u8>() = std::ptr::null_mut();
                        }
                        minor_collect_internal();
                        ready.fetch_add(1, Ordering::Release);
                        while !done.load(Ordering::Acquire) {
                            willow_gc_safepoint();
                            std::thread::yield_now();
                        }
                    }))
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
            crate::gc::preemption_tolerant_remark_budget_for_test();
            let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
            sweep::concurrent(plan);
            for (index, leaf) in leaves.into_iter().enumerate() {
                // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar
                // value.
                assert_eq!(unsafe { *leaf.cast::<usize>() }, index + 1000);
                assert!(HeapObject::from_payload(GcPayload::from_raw(leaf).unwrap()).allocated());
            }
            assert!(cycle.queue.snapshot().is_drained());
            assert_eq!(cycle.queue.snapshot().counter_underflows, 0);
            assert!(runtime().heap.lock().unwrap().concurrent_cycle.is_none());
            drop(elected);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(participants as i32) };
            eprintln!(
                "overlap SATB parked_mutators={participants} closing={closing} old_descendants={participants} sweep=retained"
            );
        }
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
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
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    let frame = NativeTaskFrame::<Frame>::allocate().unwrap();
    let tls = tlab_state_for_test();
    // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size and
    // tracing metadata describe the object initialized here.
    let young = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 1, 0, 8, 0) };
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    unsafe {
        *young.cast::<i64>() = 456;
    }
    frame.store_gc(0, young);
    let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
    let (_, cycle) = root_handshake::begin().unwrap();
    let pool = mark_workers::Pool::new(2).unwrap();
    for _ in 0..2 {
        runtime()
            .minor_pending
            .store(true, std::sync::atomic::Ordering::Release);
        pool.run(crate::runtime::current(), &cycle, 1024);
        assert_eq!(cycle.active_drains.load(Ordering::Acquire), 0);
        assert_eq!(cycle.queue.snapshot().active_consumers, 0);
        service_pending();
        assert_eq!(cycle.queue.current_epoch(), cycle.mark_epoch);
        pool.run(crate::runtime::current(), &cycle, 1024);
        assert!(cycle.queue.snapshot().is_drained());
        assert!(cycle.is_marked(frame.load_gc(0) as usize));
        // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
        assert_eq!(unsafe { *frame.load_gc(0).cast::<i64>() }, 456);
    }
    assert_ne!(frame.load_gc(0), young);
    frame.store_gc(0, std::ptr::null_mut());
    collect_owned();
    crate::gc::preemption_tolerant_remark_budget_for_test();
    let closed = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
    sweep::concurrent(closed.1);
    drop(pool);
    drop(elected);
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn overlap_phase_requests_and_remark_election_keep_epoch_ownership_distinct() {
    let _guard = isolated_runtime();
    for phase in ["initial", "concurrent", "remark_request", "remark", "sweep"] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
        // Initial/remark/sweep stops cannot recursively collect; the same owner
        // services a coalesced request only at its following non-sweep boundary.
        if phase == "initial" {
            runtime().minor_pending.store(true, Ordering::Release);
        }
        let (_, cycle) = root_handshake::begin().unwrap();
        if phase == "remark_request" {
            assert!(cycle.termination.request(cycle.termination.sample(), true));
            assert!(cycle.termination.requested());
        }
        if phase == "remark" {
            cycle.close();
        }
        if phase == "sweep" {
            crate::gc::preemption_tolerant_remark_budget_for_test();
            let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
            assert!(runtime().heap.lock().unwrap().sweeping.is_some());
            runtime().minor_pending.store(true, Ordering::Release);
            assert_eq!(runtime().heap.lock().unwrap().minor_collections, 0);
            sweep::concurrent(plan);
            service_pending();
            assert!(cycle.queue.current_epoch().is_idle());
        } else {
            runtime().minor_pending.store(true, Ordering::Release);
            service_pending();
            assert_eq!(cycle.queue.current_epoch(), cycle.mark_epoch);
            assert!(!cycle.termination.requested());
            assert_eq!(cycle.satb_shards.is_drained(), phase == "remark");
            end(&cycle);
        }
        assert_eq!(runtime().heap.lock().unwrap().minor_collections, 1);
        assert!(!runtime().minor_pending.load(Ordering::Acquire));
        drop(elected);
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn overlap_post_snapshot_tlab_black_allocation_survives_minor_and_old_sweep() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
    let (_, cycle) = root_handshake::begin().unwrap();
    let tls = tlab_state_for_test();
    // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size and
    // tracing metadata describe the object initialized here.
    let young = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 1, 0, 8, 0) };
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
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
    crate::gc::preemption_tolerant_remark_budget_for_test();
    let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
    sweep::concurrent(plan);
    // SAFETY: This GC allocation reserves an aligned word for the fixture's scalar value.
    assert_eq!(unsafe { *survivor.cast::<i64>() }, 12345);
    drop(elected);
    // Black ownership expires with the old epoch; the next minor may collect it.
    minor_collect_internal();
    assert_eq!(runtime().heap.lock().unwrap().allocated_bytes, 0);
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn concurrent_minor_snapshot_and_relocation_scale_linearly() {
    let _guard = isolated_runtime();
    for count in [1_000usize, 10_000, 100_000] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.pacer_trigger = u64::MAX;
            state.pacer.disable_for_test();
            state.nursery_threshold_bytes = usize::MAX;
        }
        let tls = tlab_state_for_test();
        let mut nodes = Vec::with_capacity(count);
        let bytes = GC_HEADER_SIZE + size_of::<usize>();
        for _ in 0..count {
            let cursor = tls.cursor.load(Ordering::Acquire);
            let limit = tls.limit.load(Ordering::Acquire);
            let node = if cursor == 0 || limit - cursor < bytes {
                // SAFETY: The fixture owns the stable TLAB through unregistration. The
                // requested size and tracing metadata describe the object initialized here.
                unsafe { willow_gc_alloc_slow(tls.as_ptr(), 1, 0, size_of::<usize>() as i64, 1) }
            } else {
                // SAFETY: The allocation cursor reserves this aligned header and its complete
                // payload. The supplied type and mask describe the initialized words.
                unsafe {
                    initialize_generated_object_at_for_test(cursor as *mut u8, bytes, 0, 1, 1)
                }
                .unwrap();
                publish_tlab_start_for_test(&tls, cursor as *mut u8);
                tls.cursor.store(cursor + bytes, Ordering::Release);
                (cursor + GC_HEADER_SIZE) as *mut u8
            };
            nodes.push(node);
        }
        for pair in nodes.windows(2) {
            // SAFETY: The fixture allocates these fields in word-aligned GC storage at whole-word
            // offsets.
            unsafe {
                *pair[0].cast::<*mut u8>() = pair[1];
            }
        }
        let mut root = nodes[0];
        let slot = (&mut root as *mut *mut u8).cast();
        runtime().runtime_root_slots.add(slot);
        let (_, cycle) = root_handshake::begin().unwrap();
        runtime().runtime_root_slots.remove(slot);
        cycle.drain(usize::MAX);
        for pass in 0..2 {
            runtime()
                .heap
                .lock()
                .unwrap()
                .minor_observation
                .page_queries
                .set(0);
            let before = runtime().heap.lock().unwrap().moved_objects;
            let before_injected = cycle.queue.snapshot().injected;
            let before_bytes = cycle.work.lock().unwrap().marked_bytes;
            collect_owned();
            assert_eq!(
                runtime()
                    .heap
                    .lock()
                    .unwrap()
                    .minor_observation
                    .snapshot_dedup_keys
                    .get(),
                0,
                "bitmap roots need no hash entries"
            );
            let publications = cycle.queue.snapshot().injected - before_injected;
            assert_eq!(
                publications, 0,
                "minor already scanned generated inline layouts"
            );
            assert_eq!(
                cycle.work.lock().unwrap().marked_bytes - before_bytes,
                (count * bytes) as u64
            );
            let queries = runtime()
                .heap
                .lock()
                .unwrap()
                .minor_observation
                .page_queries
                .get();
            assert!(queries <= 16 * count, "queries={queries}, nodes={count}");
            cycle.drain(usize::MAX);
            let state = runtime().heap.lock().unwrap();
            assert_eq!(state.moved_objects - before, count as u64);
            assert_eq!(cycle.objects.marked_count(), count);
            assert!(cycle.queue.snapshot().is_drained());
            verify_old_region_metadata(&state).unwrap();
            eprintln!(
                "concurrent-minor nodes={count} pass={pass} page_queries={queries} copies={count} live_marks={count} publications={publications}"
            );
        }
        end(&cycle);
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal() };
}

#[test]
fn post_snapshot_extent_publications_do_not_build_per_object_pending_sets() {
    let _guard = isolated_runtime();
    for count in [100usize, 1_000, 10_000] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.pacer_trigger = u64::MAX;
            state.pacer.disable_for_test();
            state.nursery_threshold_bytes = usize::MAX;
        }
        let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
        let (_, cycle) = root_handshake::begin().unwrap();
        let tls = tlab_state_for_test();
        for _ in 0..count {
            // SAFETY: The fixture owns the stable TLAB through unregistration. The requested
            // size and tracing metadata describe the object initialized here.
            let young = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 1, 0, 8, 0) };
            cycle.enqueue(young);
        }
        assert!(cycle.unindexed.lock().unwrap().is_empty());
        assert!(cycle.queue.snapshot().is_drained());
        collect_owned();
        assert!(
            cycle.queue.snapshot().is_drained(),
            "leaf copies need no trace job"
        );
        assert_eq!(
            cycle.work.lock().unwrap().marked_bytes,
            (count * (GC_HEADER_SIZE + 8)) as u64
        );
        cycle.drain(usize::MAX);
        assert_eq!(cycle.objects.marked_count(), count);
        crate::gc::preemption_tolerant_remark_budget_for_test();
        let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
        sweep::concurrent(plan);
        {
            let state = runtime().heap.lock().unwrap();
            assert_eq!(state.total_allocs - state.total_frees, count as u64);
        }
        drop(elected);
        minor_collect_internal();
        assert_eq!(runtime().heap.lock().unwrap().allocated_bytes, 0);
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn overlap_minor_inline_credit_publishes_old_descendants() {
    let _guard = isolated_runtime();
    for count in [1usize, 100, 10_000] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        {
            let mut state = runtime().heap.lock().unwrap();
            state.threshold_bytes = usize::MAX;
            state.pacer_trigger = u64::MAX;
            state.pacer.disable_for_test();
            state.nursery_threshold_bytes = usize::MAX;
        }
        let old_leaf = willow_alloc(8);
        // SAFETY: The requested layout describes the initialized words of this object and its
        // type’s tracing or finalization contract.
        let old_parent = unsafe { willow_gc_alloc_layout(2, 0, 8, 1) };
        // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the accessed
        // words.
        unsafe {
            *old_parent.cast::<*mut u8>() = old_leaf;
        }
        let elected = cycle_wait::CollectorGuard::try_acquire().unwrap();
        let (_, cycle) = root_handshake::begin().unwrap();
        assert!(!cycle.is_marked(old_parent as usize));
        assert!(!cycle.is_marked(old_leaf as usize));
        let tls = tlab_state_for_test();
        let mut previous = old_parent;
        for _ in 0..count {
            // SAFETY: The fixture owns the stable TLAB through unregistration. The requested
            // size and tracing metadata describe the object initialized here.
            let node = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 1, 0, 16, 3) };
            // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the
            // accessed words.
            unsafe {
                *node.cast::<*mut u8>() = previous;
                *node.cast::<*mut u8>().add(1) = old_parent;
            }
            previous = node;
        }
        collect_owned();
        // Repeated old edges still produce one claim/job, and tracing that
        // old parent must keep its otherwise-unrooted old descendant alive.
        assert!(cycle.is_marked(old_parent as usize));
        cycle.drain(usize::MAX);
        assert!(cycle.is_marked(old_leaf as usize));
        assert!(cycle.queue.snapshot().is_drained());
        crate::gc::preemption_tolerant_remark_budget_for_test();
        let (_, plan) = mark_closure::finish(&cycle, std::time::Instant::now()).unwrap();
        sweep::concurrent(plan);
        {
            let state = runtime().heap.lock().unwrap();
            assert_eq!(state.total_allocs - state.total_frees, count as u64 + 2);
        }
        drop(elected);
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

#[test]
fn overlap_native_recovery_retry_history_follows_minor_relocation() {
    static RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    unsafe fn stopped(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
        slots.push(payload.cast());
    }
    unsafe fn slice(
        payload: *mut u8,
        _: usize,
        _: usize,
        children: &mut Vec<*mut u8>,
    ) -> TraceSliceProgress {
        if RELEASE.load(Ordering::Acquire) {
            // SAFETY: The live object's trace layout reserves these aligned pointer slots.
            children.push(unsafe { load_gc_reference(payload.cast()) });
            TraceSliceProgress::Done
        } else {
            TraceSliceProgress::Retry
        }
    }
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
    RELEASE.store(false, Ordering::Release);
    const TYPE: u32 = 0xFA97;
    // SAFETY: The fixture controls every instance of this ID and installs the matching
    // stopped tracer and synchronized snapshot capability, if present.
    unsafe {
        willow_register_type_capability(
            TYPE,
            stopped,
            NativeTraceCapability::SliceConcurrentSafe(slice),
        )
    };
    let tls = tlab_state_for_test();
    // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size and
    // tracing metadata describe the object initialized here.
    let mut root = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 0, TYPE as i64, 8, 0) };
    // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size and
    // tracing metadata describe the object initialized here.
    let child = unsafe { willow_gc_alloc_slow(tls.as_ptr(), 0, 0, 8, 0) };
    // SAFETY: The live object's trace layout reserves these aligned pointer slots.
    unsafe { store_gc_reference(root.cast(), child) };
    let slot = (&mut root as *mut *mut u8).cast();
    runtime().runtime_root_slots.add(slot);
    let (_, cycle) = root_handshake::begin().unwrap();
    cycle.recovering.store(true, Ordering::Release);
    cycle.drain_checked(1);
    assert_eq!(cycle.recovery_native_retry_count(root as usize), 1);
    for expected in [2, 3] {
        let before = root as usize;
        collect_owned();
        assert_ne!(root as usize, before);
        assert_eq!(cycle.recovery_native_retry_count(before), 0);
        assert_eq!(
            cycle.recovery_native_retry_count(root as usize),
            expected - 1
        );
        cycle.drain_checked(1);
        assert_eq!(cycle.recovery_native_retry_count(root as usize), expected);
    }
    RELEASE.store(true, Ordering::Release);
    let (_, plan, _) = mark_closure::finish_stopped(&cycle, std::time::Instant::now());
    sweep::concurrent(plan.unwrap());
    assert_eq!(cycle.recovery_native_retry_count(root as usize), 0);
    // SAFETY: The live object's trace layout reserves these aligned pointer slots.
    assert!(!unsafe { load_gc_reference(root.cast()) }.is_null());
    runtime().runtime_root_slots.remove(slot);
    willow_gc_collect();
    assert_eq!(willow_gc_allocated_bytes(), 0);
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}

/// Allocate a young chain of `count` nodes and return its head.
fn young_chain(count: usize) -> *mut u8 {
    let tls = tlab_state_for_test();
    let nodes: Vec<_> = (0..count)
        // SAFETY: The fixture owns the stable TLAB through unregistration. The
        // requested size and tracing metadata describe the object initialized here.
        .map(|_| unsafe { willow_gc_alloc_slow(tls.as_ptr(), 1, 0, 24, 3) })
        .collect();
    for (i, &node) in nodes.iter().enumerate() {
        // SAFETY: The GC allocator word-aligns this payload and the fixture
        // reserves the accessed words.
        unsafe {
            *node.cast::<*mut u8>() = nodes.get(i + 1).copied().unwrap_or_default();
            *node.cast::<*mut u8>().add(1) = std::ptr::null_mut();
            *node.cast::<usize>().add(2) = i;
        }
    }
    nodes[0]
}

fn sizing(state: &GcState) -> (usize, u8) {
    (state.nursery_threshold_bytes, state.tenuring.age())
}

#[test]
fn minors_during_a_mark_keep_nursery_sizing() {
    let _guard = isolated_runtime();
    for count in [64usize, 256, 1024] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        // Snapshot-only survivors: the mark queue roots a chain that died
        // after the mark began, so every copy is floating garbage.
        let mut root = young_chain(count);
        let slot = (&mut root as *mut *mut u8).cast::<u8>();
        runtime().runtime_root_slots.add(slot);
        let (_, cycle) = root_handshake::begin().unwrap();
        runtime().runtime_root_slots.remove(slot);
        let before = sizing(&runtime().heap.lock().unwrap());
        for _ in 0..3 {
            let moved = runtime().heap.lock().unwrap().moved_objects;
            young_chain(count);
            collect_owned();
            let state = runtime().heap.lock().unwrap();
            assert!(
                state.moved_objects > moved,
                "count {count}: nothing survived"
            );
            assert_eq!(sizing(&state), before, "count {count}");
        }
        end(&cycle);

        // The same survival outside a mark is real evidence and tenures.
        // SAFETY: The test controls its isolated runtime at this reset boundary.
        unsafe { reset_internal_for_test() };
        let before = sizing(&runtime().heap.lock().unwrap());
        let mut root = std::ptr::null_mut();
        let slot = (&mut root as *mut *mut u8).cast::<u8>();
        runtime().runtime_root_slots.add(slot);
        for _ in 0..3 {
            root = young_chain(count);
            collect_owned();
        }
        runtime().runtime_root_slots.remove(slot);
        let after = sizing(&runtime().heap.lock().unwrap());
        assert_ne!(
            after, before,
            "count {count}: survival never reached the policy"
        );
    }
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other mutator
    // or subsequent access relies on the discarded heap.
    unsafe { reset_internal_for_test() };
}
