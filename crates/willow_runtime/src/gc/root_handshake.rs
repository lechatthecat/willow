//! Initial snapshot and phase activation under the STW coordination boundary.
use super::*;
/// Collection entry policy and initial-handshake observation belong to the
/// collector. The production policy is constant and never resolves the observer.
pub(super) trait HandshakePolicy {
    fn check_early<'a>(_owner: impl FnOnce() -> &'a Self) -> bool
    where
        Self: 'a,
    {
        true
    }

    fn participants<'a>(_owner: impl FnOnce() -> &'a Self, _count: usize)
    where
        Self: 'a,
    {
    }
}
impl HandshakePolicy for () {}
#[cfg(not(test))]
pub(super) type RuntimeHandshakePolicy = ();
#[cfg(test)]
pub(super) type RuntimeHandshakePolicy = tests::Observation;

/// Start a concurrent mark epoch, or return `None` when a legacy root owner
/// outside the registry holds a stack no handshake can publish.
pub(super) fn begin() -> Option<(u64, Arc<ConcurrentCycle>)> {
    begin_in(crate::runtime::current())
}

pub(super) fn begin_in(owner: &crate::runtime::Runtime) -> Option<(u64, Arc<ConcurrentCycle>)> {
    // Collector serialization excludes every previous epoch reader and minor
    // collection. With phase inactive, mutators publish starts but never color;
    // new extents are born zeroed. Reset pinned epoch metadata before requesting
    // a stop, so bitmap zeroing is not hidden inside initial stopped work.
    let (snapshot, locked_at) = {
        let state = owner.gc.heap.lock().unwrap();
        let locked_at = crate::gc_telemetry::timestamp_ns();
        assert_eq!(owner.gc.mark_phase().load(Ordering::Acquire), 0);
        (epoch_index::RegionIndex::snapshot(&state), locked_at)
    };
    let heap_hold_ns = crate::gc_telemetry::timestamp_ns().saturating_sub(locked_at);
    let (prepared, words) = snapshot.build(true);
    let (regions, entries) = prepared.publication_counts();
    old_cycle::record_preparation(&owner.gc, words, regions, entries, heap_hold_ns);
    let identity = ConcurrentCycle::reserve_epoch();
    let result = safepoint::with_stw_in(
        owner,
        crate::gc_telemetry::stops::StopReason::InitialMark,
        |coord, stop_work| {
            if root::foreign_root_stack_owner_active_locked_in(&owner.gc, coord) {
                return None;
            }
            RuntimeHandshakePolicy::participants(
                || &owner.gc.handshake_policy,
                coord.mutators.len(),
            );
            let (before, cycle, late_chunks) = {
                let mut state = owner.gc.heap.lock().unwrap();
                assert!(
                    state.satb.is_empty(),
                    "root handshake found undrained SATB work"
                );
                tlab::retire_all_tlabs_unindexed_for_mark_in(owner, &mut state);
                // Chunks appended after this stop receive only post-snapshot,
                // uncolored TLAB allocations: they stay implicitly black.
                let late_chunks = state.tlab_chunks.len();
                let before = state.allocated_bytes as u64;
                // Generated allocations publish start bits after header initialization.
                // A publication racing initialization is retried after the handshake.
                let objects = epoch_index::EpochIndex::regions(prepared);
                let (legacy, traces, slices) = registry::trace_registry_snapshot_in(&owner.gc);
                let mut cycle =
                    ConcurrentCycle::with_epoch_index(objects, legacy, traces, 0, identity, owner);
                cycle.late_published = std::sync::atomic::AtomicBool::new(false);
                let inputs = memory_inputs_in(&owner.gc, &state);
                let goal = state.soft_memory.decide(inputs).goal;
                // Reservations, not allocated bytes, reach the limit: partly
                // filled regions and TLAB tails count before they hold objects.
                // TLAB chunks reserved after this point enter the pacing only
                // when refilled, so it lags by at most one chunk per allocator.
                let runway = goal.saturating_sub(inputs.pressure_bytes.max(before));
                cycle.slices = slices;
                cycle.assist_pacing = if !old_cycle::concurrent_enabled()
                    || std::env::var("WILLOW_GC_ASSIST").as_deref() == Ok("0")
                {
                    None
                } else {
                    let decision = state.pacer.decision(pacer_inputs_in(owner, &state));
                    Some(
                        assist::Pacing::new(
                            decision
                                .expected_work
                                .max(state.last_major_mark_work)
                                .max(before),
                            runway,
                        )
                        .with_background(decision.assist_work_per_byte_ppm),
                    )
                };
                cycle
                    .tracing_enabled
                    .store(old_cycle::concurrent_enabled(), Ordering::Relaxed);
                let cycle = Arc::new(cycle);
                marking::set_concurrent_cycle_in(&owner.gc, &mut state, Some(cycle.clone()));
                owner.gc.mark_phase().store(1, Ordering::Release);
                (before, cycle, late_chunks)
            };
            let root_scan = crate::gc_telemetry::measurement::Span::for_runtime(
                owner,
                crate::gc_telemetry::measurement::Phase::Roots,
            );
            let root_job = cycle.termination.enter();
            let mut roots = all_registered_stack_roots(coord);
            roots.extend(root::runtime_roots_snapshot_in(&owner.gc));
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
                    let state = owner.gc.heap.lock().unwrap();
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
            Some((before, cycle, late_chunks))
        },
    );
    if let Some((_, cycle, late_chunks)) = &result {
        sweep::release_quarantine_in(&owner.gc, cycle.assist_epoch);
        // Late extents may be arbitrarily numerous after collector descheduling.
        // Publish them after resume, before starting workers; captured roots and
        // SATB claims retain unresolved values in owned unindexed work meanwhile.
        let state = owner.gc.heap.lock().unwrap();
        let late = cycle.objects.snapshot_late(&state, *late_chunks);
        drop(state);
        cycle.publish_late(late);
    }
    result.map(|(before, cycle, _)| (before, cycle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[derive(Default)]
    pub(crate) struct Observation {
        pub(crate) handshake_only: AtomicBool,
        pub(crate) participants: AtomicUsize,
    }

    impl HandshakePolicy for Observation {
        fn check_early<'a>(owner: impl FnOnce() -> &'a Self) -> bool {
            !owner().handshake_only.load(Ordering::Relaxed)
        }

        fn participants<'a>(owner: impl FnOnce() -> &'a Self, count: usize) {
            owner().participants.fetch_add(count, Ordering::Relaxed);
        }
    }

    impl Observation {
        pub(crate) fn with_handshake_only(&self, enabled: bool) -> Restore<'_> {
            Restore(self, self.handshake_only.swap(enabled, Ordering::Relaxed))
        }
    }

    pub(crate) struct Restore<'a>(&'a Observation, bool);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            self.0.handshake_only.store(self.1, Ordering::Relaxed);
        }
    }

    #[test]
    fn handshake_observation_is_owned_and_scoped() {
        let first = GcRuntime::default();
        let second = GcRuntime::default();
        std::thread::scope(|scope| {
            let a = scope.spawn(crate::runtime::serving(|| {
                let _override = first.handshake_policy.with_handshake_only(true);
                for _ in 0..64 {
                    assert!(!Observation::check_early(|| &first.handshake_policy));
                    Observation::participants(|| &first.handshake_policy, 3);
                }
            }));
            let b = scope.spawn(crate::runtime::serving(|| {
                for _ in 0..64 {
                    assert!(Observation::check_early(|| &second.handshake_policy));
                    Observation::participants(|| &second.handshake_policy, 1);
                }
            }));
            a.join().unwrap();
            b.join().unwrap();
        });
        assert_eq!(
            first.handshake_policy.participants.load(Ordering::Relaxed),
            192
        );
        assert_eq!(
            second.handshake_policy.participants.load(Ordering::Relaxed),
            64
        );
        assert!(Observation::check_early(|| &first.handshake_policy));
        let panic = std::panic::catch_unwind(|| {
            let _outer = first.handshake_policy.with_handshake_only(true);
            {
                let _inner = first.handshake_policy.with_handshake_only(false);
                assert!(Observation::check_early(|| &first.handshake_policy));
            }
            assert!(!Observation::check_early(|| &first.handshake_policy));
            panic!("exercise scoped restoration");
        });
        assert!(panic.is_err());
        assert!(Observation::check_early(|| &first.handshake_policy));
        assert!(<()>::check_early(|| panic!(
            "production policy looked up owner"
        )));
        <()>::participants(|| panic!("production observer looked up owner"), 1);
    }
}
