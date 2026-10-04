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

/// Registration epoch; `reset_internal` advances it when it clears the
/// mutator registry, invalidating every thread's cached registration.
static MUTATOR_EPOCH: AtomicU64 = AtomicU64::new(1);

std::thread_local! {
    /// `MUTATOR_EPOCH` at this thread's registration, or zero when it is not
    /// registered. Only the owning thread inserts or removes its own registry
    /// entry, so this cache answers `current_thread_is_registered` without the
    /// coordination lock that every root push/pop would otherwise contend on.
    static REGISTERED_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Whether every native frame on this thread that holds a managed pointer
    /// keeps it only in registered root slots and reloads it after a GC point.
    /// Generated code does; runtime Rust frames may keep raw copies, so the
    /// default is false and only generated-code GC entry points set it.
    static RELOCATION_SAFE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks a runtime entry point called directly by generated code at a GC
/// point (allocation slow path, poll, collection builtins). Within its extent
/// this thread's root-slot referents may move; elsewhere a park publishes them
/// as values that stay in place (willow-9tls.9). Runtime frames that call back
/// into generated code are scheduler/main trampolines holding no raw pointers.
pub(crate) struct RelocationSafeScope(bool);

impl RelocationSafeScope {
    pub(crate) fn enter() -> Self {
        Self(RELOCATION_SAFE.replace(true))
    }
}

impl Drop for RelocationSafeScope {
    fn drop(&mut self) {
        RELOCATION_SAFE.set(self.0);
    }
}

/// Safepoint for a runtime loop whose own frame, and every runtime frame
/// between it and generated code, holds no managed pointer outside registered
/// root slots: scheduler drivers between task polls, and spin-waits entered
/// directly from generated code. Roots published here are relocatable.
pub(crate) fn relocatable_safepoint() {
    let _relocation = RelocationSafeScope::enter();
    willow_gc_safepoint();
}

pub(super) fn relocation_safe() -> bool {
    RELOCATION_SAFE.get()
}

/// Native stack switches carry the flag with the stack: a parked stack keeps
/// the value it had, and the scheduler context that takes over is unsafe.
pub(super) fn set_relocation_safe(value: bool) -> bool {
    RELOCATION_SAFE.replace(value)
}

/// Root slots published by a parked mutator or a parked native stack.
/// `relocatable` records whether the owner may observe moved referents; when
/// false the collector keeps every referent in place.
#[derive(Default)]
pub(super) struct PublishedRoots {
    pub(super) slots: Vec<usize>,
    pub(super) relocatable: bool,
}

impl PublishedRoots {
    pub(super) fn current(slots: Vec<usize>) -> Self {
        Self {
            slots,
            relocatable: relocation_safe(),
        }
    }

    /// Add these roots to `roots` as rewritable slots or as in-place values.
    pub(super) fn append_to(&self, roots: &mut MinorRoots) {
        for slot in self.slots.iter().map(|&slot| slot as *mut *mut u8) {
            if self.relocatable {
                roots.slots.push(slot);
            } else if let Some(value) = RootSlot::from_raw(slot).and_then(RootSlot::load) {
                roots
                    .values
                    .push((value.as_ptr(), PinSource::RuntimeCodeStack));
            }
        }
    }
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

/// Registered mutators each legitimately own their own thread-local root stack;
/// cross-thread safety is handled by stop-the-world scanning, so they bypass the
/// legacy single-mutator `runtime().root_stack_owner` guard.
pub(super) fn current_thread_is_registered() -> bool {
    let epoch = REGISTERED_EPOCH.get();
    epoch != 0 && epoch == MUTATOR_EPOCH.load(Ordering::Acquire)
}

/// Called with the coordination lock held after the registry is cleared.
pub(super) fn invalidate_mutator_registrations() {
    MUTATOR_EPOCH.fetch_add(1, Ordering::AcqRel);
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
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_register_mutator() {
    let (lock, _) = &runtime().coord;
    {
        let mut coord = lock.lock().unwrap();
        let id = std::thread::current().id();
        coord.mutators.entry(id).or_default();
        REGISTERED_EPOCH.set(MUTATOR_EPOCH.load(Ordering::Acquire));
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
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_unregister_mutator() {
    assist::reset();
    flush_satb_current(true);
    let id = std::thread::current().id();
    let (lock, cv) = &runtime().coord;
    let coord = lock.lock().unwrap();
    let mut coord = root_handshake::wait_for_publication_round(cv, coord);
    root_handshake::publish_current(&mut coord);
    coord.mutators.remove(&id);
    REGISTERED_EPOCH.set(0);
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

/// Registers the calling thread as a GC mutator until the guard is dropped.
///
/// A thread that stops while still registered leaves its id in
/// `GcCoord::mutators`, and every later collection then waits for an
/// acknowledgement that can never arrive (willow-utqy). An explicit
/// register/unregister pair is skipped by an unwind, so one panic on a worker
/// wedges the whole process; the guard runs the unregistration on the unwind
/// path as well. Prefer it over the bare pair wherever the registered region
/// can panic.
#[must_use = "dropping the guard unregisters the mutator immediately"]
pub struct MutatorRegistration(());

impl MutatorRegistration {
    /// Register the calling thread, joining a collection that has already
    /// requested a stop exactly as `willow_gc_register_mutator` does.
    pub fn new() -> Self {
        willow_gc_register_mutator();
        Self(())
    }
}

impl Default for MutatorRegistration {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MutatorRegistration {
    fn drop(&mut self) {
        willow_gc_unregister_mutator();
    }
}

/// How long a collector waits for registered mutators to acknowledge a
/// handshake or park before treating the wait as wedged.
const DEFAULT_HANDSHAKE_TIMEOUT_SECS: u64 = 120;

/// The configured bound, or `None` to wait indefinitely.
///
/// An acknowledgement that never arrives means a registered thread stopped
/// without unregistering. The wait then cannot end, and the process produces
/// no further output at all — willow-utqy is a Windows CI job that spent its
/// whole 90-minute budget inside one such wait. Debug builds, which is every
/// test binary, give up after the bound and name the threads instead. Release
/// builds keep the unbounded wait unless the environment asks otherwise.
/// `WILLOW_GC_HANDSHAKE_TIMEOUT_SECS=0` disables the bound everywhere.
///
/// Read once: `std::env::var` allocates for the key on Windows (willow-ssl7.12).
fn handshake_timeout() -> Option<std::time::Duration> {
    static TIMEOUT: LazyLock<Option<std::time::Duration>> = LazyLock::new(|| {
        let seconds = match std::env::var("WILLOW_GC_HANDSHAKE_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
        {
            Some(seconds) => seconds,
            None if cfg!(debug_assertions) => DEFAULT_HANDSHAKE_TIMEOUT_SECS,
            None => 0,
        };
        (seconds != 0).then(|| std::time::Duration::from_secs(seconds))
    });
    *TIMEOUT
}

/// Wait on the coordination condvar until `ready` holds.
///
/// `outstanding` names the mutators still being waited on. It is consulted
/// only once the wait has exceeded `handshake_timeout`, which means a
/// registered mutator can no longer respond. There is no safe recovery from
/// that: the collector cannot scan a stack whose owner is gone, and dropping
/// the id from the registry would let a merely slow thread run unscanned. So
/// report the ids and abort rather than block with no diagnostic.
pub(super) fn wait_for_mutators<'a>(
    cv: &Condvar,
    mut coord: std::sync::MutexGuard<'a, GcCoord>,
    phase: &str,
    ready: impl Fn(&GcCoord) -> bool,
    outstanding: impl Fn(&GcCoord) -> Vec<ThreadId>,
) -> std::sync::MutexGuard<'a, GcCoord> {
    let deadline = handshake_timeout().map(|limit| std::time::Instant::now() + limit);
    while !ready(&coord) {
        let Some(deadline) = deadline else {
            coord = cv.wait(coord).unwrap_or_else(|poison| poison.into_inner());
            continue;
        };
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            let stuck = outstanding(&coord);
            eprintln!(
                "[gc] {phase} did not complete within {}s: {} registered mutator(s) never \
                 acknowledged: {stuck:?}. A thread that stops while registered must go \
                 through gc::MutatorRegistration, which unregisters on the unwind path too. \
                 Raise or disable the bound with WILLOW_GC_HANDSHAKE_TIMEOUT_SECS (0 waits \
                 indefinitely) if a mutator legitimately needs longer to reach a safepoint.",
                handshake_timeout().unwrap_or_default().as_secs(),
                stuck.len(),
            );
            std::process::abort();
        };
        coord = cv
            .wait_timeout(coord, remaining)
            .unwrap_or_else(|poison| poison.into_inner())
            .0;
    }
    coord
}

/// Process-lifetime address of the GC poll gate for generated atomic byte loads.
/// A set gate requests either independent root publication or a global stop.
/// Generated code must reload this flag at every poll, with acquire ordering or
/// stronger; caching the flag value would prevent a collector from stopping it.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_stop_flag() -> *const u8 {
    runtime().poll_requested.as_ptr().cast::<u8>()
}

/// A cooperative GC safepoint (willow-6fv.5.6). Cheap when no collection is
/// pending. When a stop-the-world collection is in progress, the calling mutator
/// publishes a snapshot of its roots and parks here until the collector resumes
/// it. The scheduler polls this between task polls; future compiler-inserted
/// safepoints can add loop-backedge coverage.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
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
    let roots = PublishedRoots::current(snapshot_local_root_slots());
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
    coord = wait_for_mutators(
        cv,
        coord,
        "stop-the-world park",
        |coord| {
            coord
                .mutators
                .keys()
                .filter(|&&id| id != me)
                .all(|id| coord.parked.contains(id))
        },
        |coord| {
            coord
                .mutators
                .keys()
                .copied()
                .filter(|&id| id != me && !coord.parked.contains(&id))
                .collect()
        },
    );
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
        roots.extend(published.slots.iter().map(|&a| a as *mut *mut u8));
    }
    roots
}

/// Stack roots for a relocating minor collection: rewritable slots from
/// relocation-safe owners, in-place values from the rest.
pub(super) fn minor_stack_roots(coord: &GcCoord) -> MinorRoots {
    let me = std::thread::current().id();
    let mut roots = MinorRoots::default();
    PublishedRoots::current(snapshot_local_root_slots()).append_to(&mut roots);
    for (&id, published) in coord.mutators.iter() {
        if id != me {
            published.append_to(&mut roots);
        }
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
