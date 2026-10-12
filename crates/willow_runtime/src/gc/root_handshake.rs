//! Initial snapshot and phase activation under the STW coordination boundary.
use super::*;
#[cfg(test)]
pub(super) static INITIAL_PARTICIPANTS: AtomicUsize = AtomicUsize::new(0);

/// Start a concurrent mark epoch, or return `None` when a legacy root owner
/// outside the registry holds a stack no handshake can publish.
pub(super) fn begin() -> Option<(u64, Arc<ConcurrentCycle>)> {
    with_stw(
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
                retire_tlabs_with_work(&mut state, stop_work);
                sync_tlab_accounting(&mut state);
                let before = state.allocated_bytes as u64;
                // Generated allocations publish start bits after header initialization.
                // A publication racing initialization is retried after the handshake.
                let objects =
                    epoch_index::EpochIndex::regions(epoch_index::RegionIndex::capture(&state));
                let mut cycle = ConcurrentCycle::with_index(
                    objects,
                    type_registry().lock().unwrap().keys().copied().collect(),
                    runtime().concurrent_trace_registry.lock().unwrap().clone(),
                    0,
                );
                let goal = state.soft_memory.decide(memory_inputs(&state)).goal;
                cycle.slices = runtime().concurrent_slice_registry.lock().unwrap().clone();
                cycle.assist_rate = if std::env::var("WILLOW_GC_ASSIST").as_deref() == Ok("0") {
                    0
                } else {
                    assist::rate(
                        state.last_major_mark_work.max(before),
                        goal.saturating_sub(before),
                    )
                };
                cycle.tracing_enabled.store(true, Ordering::Relaxed);
                let cycle = Arc::new(cycle);
                marking::set_concurrent_cycle(&mut state, Some(cycle.clone()));
                GC_MARK_PHASE.store(1, Ordering::Release);
                (before, cycle)
            };
            let root_job = cycle.termination.enter();
            let mut roots = all_registered_stack_roots(coord);
            roots.extend(runtime_roots_snapshot());
            stop_work.root_values += roots.len() as u64;
            for root in &roots {
                cycle.enqueue(*root);
            }
            {
                let mut work = cycle.work.lock().unwrap();
                work.root_scan_bytes = work
                    .root_scan_bytes
                    .saturating_add(roots.len() as u64 * std::mem::size_of::<usize>() as u64);
            }
            cycle.recheck_unindexed();
            drop(root_job);
            Some((before, cycle))
        },
    )
}
