use super::*;

/// Snapshot this thread's live root object pointers (as addresses) from its
/// thread-local stack. Reads only this thread's TLS, so it is race-free.
pub(super) fn snapshot_local_roots() -> Vec<usize> {
    ROOT_STACK.with(|rs| {
        rs.borrow()
            .iter()
            .filter(|&&slot| !slot.is_null())
            .filter_map(|&slot| {
                RootSlot::from_raw(slot)
                    .and_then(RootSlot::load)
                    .map(|payload| payload.as_ptr() as usize)
            })
            .collect()
    })
}

/// Publish live root locations rather than object values. Null-valued slots
/// need no rewrite; omitting them avoids retaining a snapshot entry for every
/// dead local. The owning stack cannot mutate until STW ends.
pub(super) fn snapshot_local_root_slots() -> Vec<usize> {
    ROOT_STACK.with(|roots| {
        roots
            .borrow()
            .iter()
            .copied()
            .filter(|&slot| RootSlot::from_raw(slot).and_then(RootSlot::load).is_some())
            .map(|slot| slot as usize)
            .collect()
    })
}

/// True when at least one mutator OTHER than the current thread is registered,
/// so a collection must stop the world rather than scan only the local stack.
#[cfg(test)]
pub(super) fn multi_mutator_active() -> bool {
    let current = std::thread::current().id();
    let (lock, _) = &runtime().coord;
    lock.lock()
        .unwrap()
        .mutators
        .keys()
        .any(|&id| id != current)
}

/// Register the current thread as a GC mutator (willow-6fv.5.6). A mutator that
/// can allocate or hold GC references on worker threads must register so a
/// stop-the-world collection scans its roots.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_register_mutator() {
    let (lock, _) = &runtime().coord;
    {
        let mut coord = lock.lock().unwrap();
        let id = std::thread::current().id();
        coord.mutators.entry(id).or_default();
        if let Some(handshake) = coord.handshake.as_mut() {
            handshake.pending.insert(id);
        }
    }
    // Registration can race with a collection that has already requested a
    // stop. Join that stop before executing any mutator work so the collector
    // never waits on a newly registered thread that has not published roots.
    willow_gc_safepoint();
}

/// Unregister the current thread as a GC mutator. Must be called before the
/// thread stops allocating/holding GC references (e.g. at worker shutdown).
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_unregister_mutator() {
    assist::reset();
    flush_satb_current(true);
    let id = std::thread::current().id();
    let (lock, cv) = &runtime().coord;
    let mut coord = lock.lock().unwrap();
    root_handshake::publish_current(&mut coord);
    coord.mutators.remove(&id);
    coord.parked.remove(&id);
    // A legacy owner can register, then empty its stack while registered.
    // Retire that ownership before releasing coord, using coord -> owner order.
    // Nonempty legacy stacks must still block foreign collections.
    clear_root_stack_owner_if_empty();
    // Keep the coordination lock while retiring TLS state: collectors acquire
    // coord then heap, so this preserves lock ordering and prevents a collector
    // from missing this thread while it still mutates its chunk metadata.
    retire_tlabs_for_thread(id);
    // A collector may be waiting for this thread to park; it no longer needs to.
    cv.notify_all();
}

/// Process-lifetime address of the GC poll gate for generated atomic byte loads.
/// A set gate requests either independent root publication or a global stop.
/// Generated code must reload this flag at every poll, with acquire ordering or
/// stronger; caching the flag value would prevent a collector from stopping it.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_stop_flag() -> *const u8 {
    runtime().poll_requested.as_ptr().cast::<u8>()
}

/// A cooperative GC safepoint (willow-6fv.5.6). Cheap when no collection is
/// pending. When a stop-the-world collection is in progress, the calling mutator
/// publishes a snapshot of its roots and parks here until the collector resumes
/// it. The scheduler polls this between task polls; future compiler-inserted
/// safepoints can add loop-backedge coverage.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_safepoint() {
    flush_satb_current(false);
    // Hot-path: a single relaxed atomic load. No collection pending → return
    // immediately without touching the coordination lock.
    if !runtime()
        .poll_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    // Past the fast path this thread is about to park. Parking while holding a
    // mark-queue lock deadlocks the collector (willow-6fv.5.6.1): the world
    // cannot restart until this thread runs, and this thread cannot run until
    // the world restarts. Checking here makes that rule enforced rather than
    // merely documented, and costs one TLS read on the already-slow path.
    crate::gc_mark_queue::assert_no_queue_lock_held("willow_gc_safepoint");
    let (lock, cv) = &runtime().coord;
    let mut coord = lock.lock().unwrap();
    root_handshake::publish_current(&mut coord);
    if !coord.stop_requested {
        return;
    }
    let id = std::thread::current().id();
    // Publish our roots so the collector can scan them while we are parked, then
    // park until the world resumes.
    let roots = snapshot_local_root_slots();
    if let Some(slot) = coord.mutators.get_mut(&id) {
        *slot = roots;
    }
    coord.parked.insert(id);
    cv.notify_all(); // wake the collector waiting for everyone to park
    while coord.stop_requested {
        coord = cv.wait(coord).unwrap();
    }
    coord.parked.remove(&id);
}

/// Run `collect` with the world stopped: request a safepoint, wait until every
/// other registered mutator has parked, then run `collect` (which scans all
/// published roots), then resume the world (willow-6fv.5.6).
pub(super) fn with_stw<R>(
    reason: crate::gc_telemetry::stops::StopReason,
    collect: impl FnOnce(&GcCoord, &mut crate::gc_telemetry::stops::StopWorkV2) -> R,
) -> R {
    let mut measurement = crate::gc_telemetry::stops::StopMeasurement::begin(reason);
    let (lock, cv) = &runtime().coord;
    let me = std::thread::current().id();
    // Publish the stop request on the lock-free gate first so mutators on the
    // hot path observe it at their next safepoint.
    runtime()
        .stop_requested
        .store(true, std::sync::atomic::Ordering::Release);
    runtime().poll_requested.store(true, Ordering::Release);
    let mut coord = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    coord.stop_requested = true;
    loop {
        let all_parked = coord
            .mutators
            .keys()
            .filter(|&&id| id != me)
            .all(|id| coord.parked.contains(id));
        if all_parked {
            break;
        }
        coord = cv.wait(coord).unwrap_or_else(|poison| poison.into_inner());
    }
    // A collection can panic: the debug pointer validation aborts the cycle on
    // a corrupt root, and callers catch that. The world has to be resumed
    // either way — an unwind that leaves `stop_requested` set parks every other
    // mutator forever, and one that unwinds out of the guard poisons the
    // registry, taking every later collection down with it (willow-v6k0).
    measurement.stopped();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        collect(&coord, measurement.work())
    }));
    coord.stop_requested = false;
    runtime()
        .stop_requested
        .store(false, std::sync::atomic::Ordering::Release);
    runtime().poll_requested.store(false, Ordering::Release);
    cv.notify_all();
    drop(coord);
    measurement.finish(result.is_err());
    match result {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// All roots to scan under stop-the-world: this (collector) thread's LIVE
/// thread-local roots plus every OTHER registered mutator's published snapshot.
pub(super) fn all_registered_stack_root_slots(coord: &GcCoord) -> Vec<*mut *mut u8> {
    let me = std::thread::current().id();
    let mut roots: Vec<*mut *mut u8> = snapshot_local_root_slots()
        .into_iter()
        .map(|a| a as *mut *mut u8)
        .collect();
    for (&id, published) in coord.mutators.iter() {
        if id == me {
            continue; // self uses the live snapshot above, not a stale publish
        }
        roots.extend(published.iter().map(|&a| a as *mut *mut u8));
    }
    roots
}

/// Value view for the existing nonrelocating root consumers. Slot publication
/// is distinct from the concurrent mark handshake's value snapshots: those
/// snapshots must never expose stack locations after their owners resume.
pub(super) fn all_registered_stack_roots(coord: &GcCoord) -> Vec<*mut u8> {
    all_registered_stack_root_slots(coord)
        .into_iter()
        .filter_map(|slot| RootSlot::from_raw(slot).and_then(RootSlot::load))
        .map(GcPayload::as_ptr)
        .collect()
}
