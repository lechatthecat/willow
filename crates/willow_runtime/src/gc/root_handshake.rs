//! Initial snapshot and phase activation under the STW coordination boundary.
use super::*;
#[cfg(test)]
pub(super) static INITIAL_PARTICIPANTS: AtomicUsize = AtomicUsize::new(0);

/// Start a concurrent mark epoch, or return `None` when a legacy root owner
/// outside the registry holds a stack no handshake can publish.
pub(super) fn begin() -> Option<(u64, Arc<ConcurrentCycle>)> {
    // Collector serialization excludes every previous epoch reader and minor
    // collection. With phase inactive, mutators publish starts but never color;
    // new extents are born zeroed. Reset pinned epoch metadata before requesting
    // a stop, so bitmap zeroing is not hidden inside initial stopped work.
    let (snapshot, locked_at) = {
        let state = runtime().heap.lock().unwrap();
        let locked_at = crate::gc_telemetry::timestamp_ns();
        assert_eq!(GC_MARK_PHASE.load(Ordering::Acquire), 0);
        (epoch_index::RegionIndex::snapshot(&state), locked_at)
    };
    let heap_hold_ns = crate::gc_telemetry::timestamp_ns().saturating_sub(locked_at);
    let (prepared, words) = snapshot.build(true);
    let (regions, entries) = prepared.publication_counts();
    old_cycle::record_preparation(words, regions, entries, heap_hold_ns);
    let identity = ConcurrentCycle::reserve_epoch();
    let result = with_stw(
        crate::gc_telemetry::stops::StopReason::InitialMark,
        |coord, stop_work| {
            if foreign_root_stack_owner_active_locked(coord) {
                return None;
            }
            #[cfg(test)]
            INITIAL_PARTICIPANTS.fetch_add(coord.mutators.len(), Ordering::Relaxed);
            let (before, cycle) = {
                let mut state = runtime().heap.lock().unwrap();
                assert!(
                    state.satb.is_empty(),
                    "root handshake found undrained SATB work"
                );
                retire_all_tlabs_unindexed_for_mark(&mut state);
                sync_tlab_accounting_unindexed(&mut state);
                let before = state.allocated_bytes as u64;
                // Generated allocations publish start bits after header initialization.
                // A publication racing initialization is retried after the handshake.
                let objects = epoch_index::EpochIndex::regions(prepared);
                let (legacy, traces, slices) = trace_registry_snapshot();
                let mut cycle =
                    ConcurrentCycle::with_epoch_index(objects, legacy, traces, 0, identity);
                let goal = state.soft_memory.decide(memory_inputs(&state)).goal;
                cycle.slices = slices;
                cycle.assist_rate = if !old_cycle::concurrent_enabled()
                    || std::env::var("WILLOW_GC_ASSIST").as_deref() == Ok("0")
                {
                    0
                } else {
                    assist::rate(
                        state.last_major_mark_work.max(before),
                        goal.saturating_sub(before),
                    )
                };
                cycle
                    .tracing_enabled
                    .store(old_cycle::concurrent_enabled(), Ordering::Relaxed);
                let cycle = Arc::new(cycle);
                marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
                GC_MARK_PHASE.store(1, Ordering::Release);
                (before, cycle)
            };
            let root_scan = crate::gc_telemetry::measurement::Span::new(
                crate::gc_telemetry::measurement::Phase::Roots,
            );
            let root_job = cycle.termination.enter();
            let mut roots = all_registered_stack_roots(coord);
            roots.extend(runtime_roots_snapshot());
            stop_work.root_values += roots.len() as u64;
            for root in &roots {
                cycle.enqueue(*root);
            }
            // Enqueue drops addresses outside every captured extent. Roots are
            // all captured at this stop, so validate exactly those here.
            #[cfg(debug_assertions)]
            {
                let reader = cycle.objects.reader();
                let mut outside = roots.iter().copied().filter(|&root| {
                    !root.is_null()
                        && !cycle.objects.contains(root as usize)
                        && !reader.needs_start_publication(root as usize)
                });
                if let Some(first) = outside.next() {
                    let state = runtime().heap.lock().unwrap();
                    *cycle.invalid_root.lock().unwrap() = std::iter::once(first)
                        .chain(outside)
                        .find_map(|root| validate_payload_pointer_locked(&state, root).err());
                }
            }
            {
                let mut work = cycle.work.lock().unwrap();
                work.root_scan_bytes = work
                    .root_scan_bytes
                    .saturating_add(roots.len() as u64 * std::mem::size_of::<usize>() as u64);
            }
            drop(root_job);
            drop(root_scan);
            cycle
                .phase
                .lock()
                .unwrap()
                .transition(old_cycle::Phase::ConcurrentMark);
            Some((before, cycle))
        },
    );
    if let Some((_, cycle)) = &result {
        sweep::release_quarantine(cycle.assist_epoch);
        // Late extents may be arbitrarily numerous after collector descheduling.
        // Publish them after resume, before starting workers; captured roots and
        // SATB claims retain unresolved values in owned unindexed work meanwhile.
        let state = runtime().heap.lock().unwrap();
        let late = cycle.objects.snapshot_late(&state);
        drop(state);
        cycle.objects.publish_late(late);
        cycle.recheck_unindexed();
    }
    result
}
