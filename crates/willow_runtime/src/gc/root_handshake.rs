//! Two-round publication without parking mutators: first cross every in-flight
//! pre-activation barrier/store, then publish roots. No payload tracing may run
//! before the activation cut. Early acknowledgers resume in both rounds.
use super::*;

#[cfg(test)]
pub(super) static ACTIVATION_ACKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
pub(super) static ROOT_ACKS: AtomicUsize = AtomicUsize::new(0);
#[cfg(test)]
pub(super) static ACK_NOTIFICATIONS: AtomicUsize = AtomicUsize::new(0);

pub(super) struct Handshake {
    pub(super) pending: HashSet<ThreadId>,
    activating: bool,
}

fn notify_round_complete(handshake: &Handshake) {
    if handshake.pending.is_empty() {
        #[cfg(test)]
        ACK_NOTIFICATIONS.fetch_add(1, Ordering::Relaxed);
        runtime().coord.1.notify_all();
    }
}

pub(super) fn publish_current(coord: &mut GcCoord) {
    let id = std::thread::current().id();
    let Some(handshake) = coord.handshake.as_mut() else {
        return;
    };
    if !handshake.pending.remove(&id) {
        return;
    }
    if handshake.activating {
        #[cfg(test)]
        ACTIVATION_ACKS.fetch_add(1, Ordering::Relaxed);
        // The coord lock observes phase publication. This safepoint is after
        // the caller's preceding barrier AND store (neither may safepoint).
        // Taking roots here would recreate the pre-activation deletion race.
        notify_round_complete(handshake);
        return;
    }
    #[cfg(test)]
    ROOT_ACKS.fetch_add(1, Ordering::Relaxed);
    let roots = snapshot_local_roots();
    let cycle = {
        let mut state = runtime().heap.lock().unwrap();
        retire_owned_tlabs_locked(&mut state, id, false);
        flush_satb_all_for_current(&mut state);
        state
            .concurrent_cycle
            .clone()
            .expect("root handshake has a mark epoch")
    };
    for &root in &roots {
        cycle.enqueue(root as *mut u8);
    }
    let mut work = cycle.work.lock().unwrap();
    work.root_scan_bytes = work
        .root_scan_bytes
        .saturating_add(roots.len() as u64 * std::mem::size_of::<usize>() as u64);
    notify_round_complete(handshake);
}

/// Keep a leaving mutator in the registry until the publication round opens.
///
/// A participant that already acknowledged activation is absent from the
/// activation `pending` set, and the publication round is rebuilt from the
/// registry. Leaving in between would drop its root stack from the snapshot
/// even though it keeps using those references, so it waits for the rebuild
/// and then publishes like any other participant. The collector explicitly
/// notifies `cv` when it opens the publication round after the rebuild.
pub(super) fn wait_for_publication_round<'a>(
    cv: &Condvar,
    mut coord: std::sync::MutexGuard<'a, GcCoord>,
) -> std::sync::MutexGuard<'a, GcCoord> {
    while coord
        .handshake
        .as_ref()
        .is_some_and(|handshake| handshake.activating)
    {
        publish_current(&mut coord);
        coord = cv.wait(coord).unwrap_or_else(|poison| poison.into_inner());
    }
    coord
}

fn flush_satb_all_for_current(state: &mut GcState) {
    let cycle = state
        .concurrent_cycle
        .as_ref()
        .expect("active root handshake");
    state
        .satb
        .flush_thread(std::thread::current().id(), false, |values| {
            cycle.enqueue_satb_batch(values)
        });
}

/// Wait until every participant of the current round has acknowledged.
///
/// Bounded by `wait_for_mutators`: a participant that stopped without
/// unregistering can never acknowledge, and an unbounded wait there hides the
/// cause behind a silent hang (willow-utqy).
fn wait_pending<'a>(
    cv: &Condvar,
    coord: std::sync::MutexGuard<'a, GcCoord>,
    phase: &str,
) -> std::sync::MutexGuard<'a, GcCoord> {
    wait_for_mutators(
        cv,
        coord,
        phase,
        |coord| {
            coord
                .handshake
                .as_ref()
                .is_none_or(|handshake| handshake.pending.is_empty())
        },
        |coord| {
            coord
                .handshake
                .as_ref()
                .map(|handshake| handshake.pending.iter().copied().collect())
                .unwrap_or_default()
        },
    )
}

/// Start a concurrent mark epoch, or return `None` when a legacy root owner
/// outside the registry holds a stack no handshake can publish.
pub(super) fn begin() -> Option<(u64, Arc<ConcurrentCycle>)> {
    let (lock, cv) = &runtime().coord;
    let mut coord = lock.lock().unwrap();
    assert!(!coord.stop_requested && coord.handshake.is_none());
    // The caller's earlier check released coord; an owner may have unregistered
    // with a nonempty stack since. Deciding under this hold closes that gap.
    if foreign_root_stack_owner_active_locked(&coord) {
        return None;
    }
    let (before, cycle) = {
        let mut state = runtime().heap.lock().unwrap();
        assert!(
            state.satb.is_empty(),
            "root handshake found undrained SATB work"
        );
        sync_tlab_accounting(&mut state);
        let before = state.allocated_bytes as u64;
        // Generated allocations publish start bits after header initialization.
        // A publication racing initialization is retried after the handshake.
        let objects = epoch_index::EpochIndex::Regions(epoch_index::RegionIndex::capture(&state));
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
        cycle.tracing_enabled.store(false, Ordering::Relaxed);
        let cycle = Arc::new(cycle);
        state.concurrent_cycle = Some(cycle.clone());
        GC_MARK_PHASE.store(1, Ordering::Release);
        (before, cycle)
    };
    let mut pending: HashSet<_> = coord.mutators.keys().copied().collect();
    pending.insert(std::thread::current().id());
    coord.handshake = Some(Handshake {
        pending,
        activating: true,
    });
    runtime().poll_requested.store(true, Ordering::Release);
    publish_current(&mut coord);
    coord = wait_pending(cv, coord, "root handshake activation round");
    // All pre-activation stores are now complete. Only now can root snapshots
    // and tracing establish a consistent SATB cut. Rebuild participants once:
    // new registrations join this round. Unregistration waits out activation
    // (`wait_for_publication_round`), so no participant leaves unpublished.
    let mut pending: HashSet<_> = coord.mutators.keys().copied().collect();
    pending.insert(std::thread::current().id());
    coord.handshake = Some(Handshake {
        pending,
        activating: false,
    });
    cycle.tracing_enabled.store(true, Ordering::Release);
    // Leaving mutators wait for this transition, not just round completion.
    cv.notify_all();
    publish_current(&mut coord);
    coord = wait_pending(cv, coord, "root handshake publication round");
    coord.handshake = None;
    runtime().poll_requested.store(false, Ordering::Release);
    drop(coord);
    let roots = runtime_roots_snapshot();
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
    Some((before, cycle))
}
