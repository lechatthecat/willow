use super::*;

/// Snapshot this thread's live root object pointers (as addresses) from its
/// thread-local stack. Reads only this thread's TLS, so it is race-free.
pub(super) fn snapshot_local_roots() -> Vec<usize> {
    let mut result = Vec::new();
    crate::generated_context::visit_roots(|slots| {
        result.extend(slots.iter().filter_map(|&slot| {
            RootSlot::from_raw(slot)
                .and_then(RootSlot::load)
                .map(|payload| payload.as_ptr() as usize)
        }));
    });
    result
}

std::thread_local! {
    static SATB_BUFFER: std::cell::RefCell<Option<Arc<satb::MutatorBuffer>>> = const { std::cell::RefCell::new(None) };
    /// The registering runtime's epoch cell, or null when this thread is not
    /// registered, and the epoch it held at registration. Only the owning
    /// thread inserts or removes its own registry entry, so this cache answers
    /// `current_thread_is_registered` without the coordination lock that every
    /// root push/pop would otherwise contend on. The cell address identifies
    /// the owner; it is compared by address before any dereference, and a
    /// registration is retired before its runtime can be dropped.
    static REGISTERED_EPOCH_CELL: std::cell::Cell<*const std::sync::atomic::AtomicU64> = const { std::cell::Cell::new(std::ptr::null()) };
    static REGISTERED_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// `REGISTERED_EPOCH_CELL` when the registration belongs to the runtime
    /// this thread serves, else null. Selected runtimes are `'static` and a
    /// registered thread cannot change selection, so a non-null value stays
    /// dereferenceable and keeps identifying `runtime()` while registered.
    static SERVED_EPOCH_CELL: std::cell::Cell<*const std::sync::atomic::AtomicU64> = const { std::cell::Cell::new(std::ptr::null()) };
    /// Whether every native frame on this thread that holds a managed pointer
    /// keeps it only in registered root slots and reloads it after a GC point.
    /// Generated code does; runtime Rust frames may keep raw copies, so the
    /// default is false and only generated-code GC entry points set it.
    static RELOCATION_SAFE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks a runtime entry point called directly by generated code at a GC
/// point (allocation slow path, poll, collection builtins). Within its extent
/// this thread's root-slot referents may move; elsewhere a park publishes them
/// as values that stay in place. Runtime frames that call back
/// into generated code are scheduler/main trampolines holding no raw pointers.
pub(crate) struct RelocationSafeScope(bool);

impl RelocationSafeScope {
    // Keep TLS address computation on its side of a migrating suspension.
    #[inline(never)]
    pub(crate) fn enter() -> Self {
        Self(RELOCATION_SAFE.replace(true))
    }
}

impl Drop for RelocationSafeScope {
    #[inline(never)]
    fn drop(&mut self) {
        RELOCATION_SAFE.set(self.0);
    }
}

/// Safepoint for a runtime loop whose own frame, and every runtime frame
/// between it and generated code, holds no managed pointer outside registered
/// root slots: scheduler drivers between task polls, and spin-waits entered
/// directly from generated code. Roots published here are relocatable.
pub(crate) fn relocatable_safepoint() {
    relocatable_safepoint_in(crate::runtime::current());
}

pub(crate) fn relocatable_safepoint_in(owner: &crate::runtime::Runtime) {
    let _relocation = RelocationSafeScope::enter();
    safepoint_in(owner);
}

pub(super) fn relocation_safe() -> bool {
    RELOCATION_SAFE.get()
}

/// Native stack switches carry the flag with the stack: a parked stack keeps
/// the value it had, and the scheduler context that takes over is unsafe.
pub(crate) fn set_relocation_safe(value: bool) -> bool {
    RELOCATION_SAFE.replace(value)
}

/// Root slots published by a parked mutator or a parked native stack.
/// `relocatable` records whether the owner may observe moved referents; when
/// false the collector keeps every referent in place.
#[derive(Default)]
pub(super) struct PublishedRoots {
    pub(super) slots: Vec<usize>,
    /// Context whose slots the next stop reads into `slots`. Entering a safe
    /// region stays independent of root depth, and a region no stop reaches
    /// never walks its roots.
    deferred: Option<QuiescentContext>,
    pub(super) relocatable: bool,
    pub(super) satb_buffer: Option<Arc<satb::MutatorBuffer>>,
}

/// Active context of a thread inside a safe region.
struct QuiescentContext(*const crate::generated_context::Context);

// SAFETY: only a stopped collector dereferences this, under the coordination
// lock, while the owner stays in its region. Leaving the region clears it.
unsafe impl Send for QuiescentContext {}

impl PublishedRoots {
    pub(super) fn current(slots: Vec<usize>) -> Self {
        Self {
            slots,
            deferred: None,
            relocatable: relocation_safe(),
            satb_buffer: current_satb_buffer(),
        }
    }

    /// Publication for a safe region: O(1), whatever the root depth.
    fn deferred() -> Self {
        Self {
            slots: Vec::new(),
            deferred: Some(QuiescentContext(crate::generated_context::active())),
            relocatable: relocation_safe(),
            satb_buffer: current_satb_buffer(),
        }
    }

    /// Read deferred slots once; later stops of the same region reuse them.
    fn materialize(&mut self) {
        let Some(QuiescentContext(context)) = self.deferred.take() else {
            return;
        };
        // SAFETY: the world is stopped and the owner cannot leave its region
        // without the coordination lock this collector holds.
        unsafe {
            crate::generated_context::visit_quiescent_roots(context, |slots| {
                append_live_root_slots(slots, &mut self.slots);
            });
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
    let mut result = Vec::new();
    crate::generated_context::visit_roots(|slots| append_live_root_slots(slots, &mut result));
    result
}

fn append_live_root_slots(slots: &[*mut *mut u8], result: &mut Vec<usize>) {
    result.extend(
        slots
            .iter()
            .copied()
            .filter(|&slot| RootSlot::from_raw(slot).and_then(RootSlot::load).is_some())
            .map(|slot| slot as usize),
    );
}

/// Registered mutators each legitimately own their own thread-local root stack;
/// cross-thread safety is handled by stop-the-world scanning, so they bypass the
/// legacy single-mutator `runtime().root_stack_owner` guard.
///
/// A thread may hold a registration with a runtime it does not serve (shared
/// runtime workers and drivers start without selecting their owner), so only
/// a registration with `runtime()` counts. Registration records that case in
/// `SERVED_EPOCH_CELL`, which avoids a runtime lookup on root push/pop.
pub(super) fn current_thread_is_registered() -> bool {
    let cell = SERVED_EPOCH_CELL.get();
    // SAFETY: non-null only while registered with the selected runtime, which
    // is `'static` and cannot be deselected while this thread is registered.
    !cell.is_null() && REGISTERED_EPOCH.get() == unsafe { &*cell }.load(Ordering::Acquire)
}

pub(super) fn current_thread_is_registered_in(owner: &GcRuntime) -> bool {
    std::ptr::eq(REGISTERED_EPOCH_CELL.get(), &owner.mutator_epoch)
        && REGISTERED_EPOCH.get() == owner.mutator_epoch.load(Ordering::Acquire)
}

/// Whether this thread may start serving `next`: a thread that still holds a
/// mutator registration must keep serving the runtime it registered with.
pub(crate) fn registration_permits_selection(next: &crate::runtime::Runtime) -> bool {
    let cell = REGISTERED_EPOCH_CELL.get();
    cell.is_null() || std::ptr::eq(cell, &next.gc.mutator_epoch)
}

/// Re-derive `SERVED_EPOCH_CELL` after this thread starts serving `next`.
/// Selection is permitted only when unregistered or registered with `next`
/// (see [`registration_permits_selection`]); the latter case must start
/// counting as served because `next` is now `runtime()` and `'static`.
pub(crate) fn refresh_served_registration(next: &'static crate::runtime::Runtime) {
    let cell = REGISTERED_EPOCH_CELL.get();
    SERVED_EPOCH_CELL.set(if std::ptr::eq(cell, &next.gc.mutator_epoch) {
        cell
    } else {
        std::ptr::null()
    });
}

pub(super) fn current_satb_buffer() -> Option<Arc<satb::MutatorBuffer>> {
    current_satb_buffer_in(runtime())
}

pub(super) fn current_satb_buffer_in(owner: &GcRuntime) -> Option<Arc<satb::MutatorBuffer>> {
    let epoch = REGISTERED_EPOCH.try_with(|epoch| epoch.get()).ok()?;
    if !std::ptr::eq(
        REGISTERED_EPOCH_CELL.try_with(|cell| cell.get()).ok()?,
        &owner.mutator_epoch,
    ) || epoch != owner.mutator_epoch.load(Ordering::Acquire)
    {
        return None;
    }
    SATB_BUFFER
        .try_with(|local| local.borrow().clone())
        .ok()
        .flatten()
}

/// Called with the coordination lock held after the registry is cleared.
pub(super) fn invalidate_mutator_registrations() {
    crate::generated_context::revoke_barrier_buffers();
    runtime().mutator_epoch.fetch_add(1, Ordering::AcqRel);
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

/// Register the current thread as a GC mutator. A mutator that
/// can allocate or hold GC references on worker threads must register so a
/// stop-the-world collection scans its roots.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_register_mutator() {
    register_mutator_in(crate::runtime::current());
}

fn register_mutator_in(owner: &crate::runtime::Runtime) {
    assert!(
        registration_permits_selection(owner),
        "a thread must retire its previous mutator before changing runtime"
    );
    crate::gc_telemetry::observations::register_for_runtime(owner);
    let (lock, _) = &owner.gc.coord;
    {
        let mut coord = lock.lock().unwrap();
        let id = std::thread::current().id();
        let record = coord.mutators.entry(id).or_default();
        let buffer = record.satb_buffer.get_or_insert_with(Default::default);
        SATB_BUFFER.with(|local| *local.borrow_mut() = Some(Arc::clone(buffer)));
        REGISTERED_EPOCH_CELL.set(&owner.gc.mutator_epoch);
        SERVED_EPOCH_CELL.set(if std::ptr::eq(owner, crate::runtime::current()) {
            &owner.gc.mutator_epoch
        } else {
            std::ptr::null()
        });
        REGISTERED_EPOCH.set(owner.gc.mutator_epoch.load(Ordering::Acquire));
        if coord.stop_requested {
            coord.stw_pending.insert(id);
        }
    }
    // Registration can race with a collection that has already requested a
    // stop. Join that stop before executing any mutator work so the collector
    // never waits on a newly registered thread that has not published roots.
    safepoint_in(owner);
}

/// Unregister the current thread as a GC mutator. Must be called before the
/// thread stops allocating/holding GC references (e.g. at worker shutdown).
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_unregister_mutator() {
    unregister_mutator_in(crate::runtime::current());
}

fn unregister_mutator_in(owner: &crate::runtime::Runtime) {
    assert!(
        registration_permits_selection(owner),
        "cannot retire a mutator belonging to another runtime"
    );
    assist::reset();
    barrier::flush_satb_current_in(owner, true);
    crate::generated_context::revoke_barrier_buffers();
    let id = std::thread::current().id();
    let (lock, cv) = &owner.gc.coord;
    let mut coord = lock.lock().unwrap();
    // A previously registered stack must not disappear from both registries.
    // Empty-stack shutdown remains legal during a collection. Nonempty raw
    // ownership cannot leave a running epoch's barrier/handshake protocol, so
    // finish a foreign epoch while still registered (its roots stay published
    // through the safepoint protocol), then take legacy ownership under the
    // same lock that elects the next collector.
    if crate::generated_context::depth() != 0 || crate::generated_context::has_active_bridge() {
        while coord
            .cycle_collector
            .is_some_and(|collector| collector != id)
        {
            let generation = coord.cycle_generation;
            super::cycle_wait::wait_for_cycle_in(owner, coord, generation);
            coord = lock.lock().unwrap();
        }
        root::claim_legacy_owner_locked_in(&owner.gc, &coord, false);
    }
    crate::gc_telemetry::observations::unregister_for_runtime(owner);
    coord.mutators.remove(&id);
    coord.stw_pending.remove(&id);
    REGISTERED_EPOCH.set(0);
    REGISTERED_EPOCH_CELL.set(std::ptr::null());
    SERVED_EPOCH_CELL.set(std::ptr::null());
    SATB_BUFFER.with(|local| *local.borrow_mut() = None);
    coord.parked.remove(&id);
    // A legacy owner can register, then empty its stack while registered.
    // Retire that ownership before releasing coord, using coord -> owner order.
    // Nonempty legacy stacks must still block foreign collections.
    root::clear_root_stack_owner_if_empty_in(&owner.gc);
    // Keep the coordination lock while retiring TLS state: collectors acquire
    // coord then heap, so this preserves lock ordering and prevents a collector
    // from missing this thread while it still mutates its chunk metadata.
    old_lab::retire_private_current_in(owner);
    tlab::retire_tlabs_for_thread_in(owner, id);
    // A collector may be waiting for this thread to park; it no longer needs to.
    cv.notify_all();
}

/// Registers the calling thread as a GC mutator until the guard is dropped.
///
/// A thread that stops while still registered leaves its id in
/// `GcCoord::mutators`, and every later collection then waits for an
/// acknowledgement that can never arrive. An explicit
/// register/unregister pair is skipped by an unwind, so one panic on a worker
/// wedges the whole process; the guard runs the unregistration on the unwind
/// path as well. Prefer it over the bare pair wherever the registered region
/// can panic.
#[must_use = "dropping the guard unregisters the mutator immediately"]
pub struct MutatorRegistration<'a> {
    owner: &'a crate::runtime::Runtime,
    owned: bool,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl MutatorRegistration<'static> {
    /// Register the calling thread, joining a collection that has already
    /// requested a stop exactly as `willow_gc_register_mutator` does.
    pub fn new() -> Self {
        Self::new_in(crate::runtime::current())
    }
}

impl<'a> MutatorRegistration<'a> {
    pub(crate) fn new_in(owner: &'a crate::runtime::Runtime) -> Self {
        let owned = !current_thread_is_registered_in(&owner.gc);
        if owned {
            register_mutator_in(owner);
        }
        Self {
            owner,
            owned,
            _thread: std::marker::PhantomData,
        }
    }
}

impl Default for MutatorRegistration<'static> {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MutatorRegistration<'_> {
    fn drop(&mut self) {
        if self.owned {
            unregister_mutator_in(self.owner);
        }
    }
}

/// Temporary native/FFI mutator registration. Enter BEFORE acquiring managed
/// references; poll `willow_gc_safepoint` during long-running managed work.
/// Roots pushed within this lexical scope are removed on return, cancellation
/// (Drop), or Rust unwind. Nested scopes preserve the caller's registration and
/// root prefix. Do not pop caller-owned roots or let raw references escape.
///
/// Native calls with no heap references need no scope. Native code that blocks
/// without polling must instead retain already-published stable handles or
/// PinLease address loans and must not manipulate unprotected managed values.
/// Generated FFI calls already on a mutator use explicit safepoints and their
/// existing handle/PinLease bridge; this scope does not unregister that caller.
#[must_use]
pub struct NativeGcScope {
    depth: usize,
    _registration: MutatorRegistration<'static>,
}

impl NativeGcScope {
    pub fn enter() -> Self {
        let registration = MutatorRegistration::new();
        Self {
            depth: crate::generated_context::depth(),
            _registration: registration,
        }
    }
}

impl Drop for NativeGcScope {
    fn drop(&mut self) {
        ROOT_STACK.with(|roots| {
            let mut roots = roots.borrow_mut();
            roots.truncate(self.depth - crate::generated_context::parent_depth());
        });
        clear_root_stack_owner_if_empty();
    }
}

/// The configured bound, or `None` to wait indefinitely.
///
/// An acknowledgement that never arrives means a registered thread stopped
/// without unregistering. The wait then cannot end, and the process produces
/// no further output at all — the stalled GC regression is a Windows CI job that spent its
/// whole 90-minute budget inside one such wait. All builds give up after the
/// bound and name the threads instead; release builds must not silently wait
/// forever for a vanished mutator.
/// `WILLOW_GC_HANDSHAKE_TIMEOUT_SECS=0` disables the bound everywhere.
///
/// Read once: `std::env::var` allocates for the key on Windows.
fn handshake_timeout() -> Option<std::time::Duration> {
    *crate::runtime::current().diagnostics.gc_handshake_timeout
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
    cv: &GcCondvar,
    coord: std::sync::MutexGuard<'a, GcCoord>,
    phase: &str,
    ready: impl Fn(&GcCoord) -> bool,
    outstanding: impl Fn(&GcCoord) -> Vec<ThreadId>,
) -> std::sync::MutexGuard<'a, GcCoord> {
    wait_for_mutators_with_timeout(handshake_timeout(), cv, coord, phase, ready, outstanding)
}

fn wait_for_mutators_with_timeout<'a>(
    timeout: Option<std::time::Duration>,
    cv: &GcCondvar,
    mut coord: std::sync::MutexGuard<'a, GcCoord>,
    phase: &str,
    ready: impl Fn(&GcCoord) -> bool,
    outstanding: impl Fn(&GcCoord) -> Vec<ThreadId>,
) -> std::sync::MutexGuard<'a, GcCoord> {
    let deadline = timeout.map(|limit| std::time::Instant::now() + limit);
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
                timeout.unwrap_or_default().as_secs(),
                stuck.len(),
            );
            crate::failure::fatal_invariant("registered mutator handshake timed out");
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
    stop_flag_in(runtime())
}

/// The stop flag generated polls of `owner`'s mutators read. Its address is
/// fixed for the runtime's life.
pub(crate) fn stop_flag_in(owner: &GcRuntime) -> *const u8 {
    owner.poll_requested.as_ptr().cast::<u8>()
}

/// A cooperative GC safepoint. Cheap when no collection is
/// pending. When a stop-the-world collection is in progress, the calling mutator
/// publishes a snapshot of its roots and parks here until the collector resumes
/// it. The scheduler polls this between task polls; future compiler-inserted
/// safepoints can add loop-backedge coverage.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_safepoint() {
    safepoint_in(crate::runtime::current());
}

pub(super) fn safepoint_in(owner: &crate::runtime::Runtime) {
    crate::gc_telemetry::observations::progress_for_runtime(owner);
    barrier::flush_satb_current_in(owner, false);
    // Hot-path: a single relaxed atomic load. No collection pending → return
    // immediately without touching the coordination lock.
    if !owner
        .gc
        .poll_requested
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return;
    }
    // Past the fast path this thread is about to park. Parking while holding a
    // mark-queue lock deadlocks the collector: the world
    // cannot restart until this thread runs, and this thread cannot run until
    // the world restarts. Checking here makes that rule enforced rather than
    // merely documented, and costs one TLS read on the already-slow path.
    crate::gc_mark_queue::assert_no_queue_lock_held("willow_gc_safepoint");
    let _interval = crate::gc_telemetry::observations::Span::for_runtime(
        owner,
        crate::gc_telemetry::observations::Kind::Handshake,
    );
    let (lock, cv) = &owner.gc.coord;
    let mut coord = lock.lock().unwrap();
    if !coord.stop_requested {
        return;
    }
    let id = std::thread::current().id();
    // Publish our roots so the collector can scan them while we are parked, then
    // park until the world resumes.
    let mut roots = PublishedRoots::current(snapshot_local_root_slots());
    roots.satb_buffer = current_satb_buffer_in(&owner.gc);
    if let Some(slot) = coord.mutators.get_mut(&id) {
        *slot = roots;
    }
    coord.parked.insert(id);
    coord.stw_pending.remove(&id);
    if coord.stw_pending.is_empty() {
        cv.notify_all();
    }
    while coord.stop_requested {
        coord = cv.wait(coord).unwrap();
    }
    coord.parked.remove(&id);
}

/// A dedicated marker has no mutator roots or allocation buffers. Its queue
/// consumer is retired before polling; enrollment exists solely for stop rounds.
pub(super) struct MarkerRegistration<'a> {
    owner: &'a crate::runtime::Runtime,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
impl<'a> MarkerRegistration<'a> {
    pub(super) fn new(owner: &'a crate::runtime::Runtime) -> Self {
        crate::gc_telemetry::observations::register_for_runtime(owner);
        let id = std::thread::current().id();
        {
            let mut coord = owner.gc.coord.0.lock().unwrap();
            assert!(
                coord
                    .mutators
                    .insert(id, PublishedRoots::default())
                    .is_none()
            );
            if coord.stop_requested {
                coord.stw_pending.insert(id);
            }
        }
        let registration = Self {
            owner,
            _thread: std::marker::PhantomData,
        };
        registration.poll();
        registration
    }

    pub(super) fn poll(&self) {
        crate::gc_telemetry::observations::progress_for_runtime(self.owner);
        if !self.owner.gc.poll_requested.load(Ordering::Acquire) {
            return;
        }
        crate::gc_mark_queue::assert_no_queue_lock_held("marker safepoint");
        let (lock, cv) = &self.owner.gc.coord;
        let mut coord = lock.lock().unwrap();
        if !coord.stop_requested {
            return;
        }
        let id = std::thread::current().id();
        coord.parked.insert(id);
        coord.stw_pending.remove(&id);
        if coord.stw_pending.is_empty() {
            cv.notify_all();
        }
        while coord.stop_requested {
            coord = cv.wait(coord).unwrap();
        }
        coord.parked.remove(&id);
    }
}
impl Drop for MarkerRegistration<'_> {
    fn drop(&mut self) {
        crate::gc_telemetry::observations::unregister_for_runtime(self.owner);
        let (lock, cv) = &self.owner.gc.coord;
        let mut coord = lock.lock().unwrap_or_else(|p| p.into_inner());
        let id = std::thread::current().id();
        coord.mutators.remove(&id);
        coord.stw_pending.remove(&id);
        coord.parked.remove(&id);
        cv.notify_all();
    }
}

/// Keeps a waiting thread visible to collectors without waking it for every
/// stop. The owner publishes its context in O(1); the first stop reads the
/// slots, and later stops reuse them until Drop takes the same coordination
/// lock and observes the world running again.
pub(crate) struct IdleMutator<'a> {
    owner: &'a crate::runtime::Runtime,
    registered: bool,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl IdleMutator<'static> {
    /// # Safety
    /// Until this guard is dropped, the caller must not access managed objects,
    /// root slots, TLAB state, or SATB buffers, or call back into managed code.
    /// It may only wait on native synchronization or OS calls. Relocation safety
    /// is inherited from the caller; raw runtime references remain pinned.
    pub(crate) unsafe fn enter() -> Self {
        unsafe { Self::enter_in(crate::runtime::current()) }
    }
}

impl<'a> IdleMutator<'a> {
    /// The same exclusion contract as `enter`, for the selected runtime.
    pub(crate) unsafe fn enter_in(owner: &'a crate::runtime::Runtime) -> Self {
        let registered = current_thread_is_registered_in(&owner.gc);
        if registered {
            barrier::flush_satb_current_in(owner, false);
            crate::gc_mark_queue::assert_no_queue_lock_held("idle mutator");
            let (lock, cv) = &owner.gc.coord;
            let mut coord = lock.lock().unwrap();
            let id = std::thread::current().id();
            *coord
                .mutators
                .get_mut(&id)
                .expect("registered idle mutator") = PublishedRoots {
                satb_buffer: current_satb_buffer_in(&owner.gc),
                ..PublishedRoots::deferred()
            };
            assert!(coord.parked.insert(id), "nested idle mutator guard");
            coord.stw_pending.remove(&id);
            if coord.stop_requested && coord.stw_pending.is_empty() {
                cv.notify_all();
            }
        }
        Self {
            owner,
            registered,
            _thread: std::marker::PhantomData,
        }
    }
}

impl IdleMutator<'static> {
    /// Hand the process-default region to a caller across an ABI boundary.
    pub(crate) fn into_raw(self) -> bool {
        assert!(std::ptr::eq(self.owner, crate::runtime::current()));
        let registered = self.registered;
        std::mem::forget(self);
        registered
    }

    /// # Safety
    /// `registered` must come from `into_raw` on this thread, exactly once.
    pub(crate) unsafe fn from_raw(registered: bool) -> Self {
        Self {
            owner: crate::runtime::current(),
            registered,
            _thread: std::marker::PhantomData,
        }
    }
}

impl Drop for IdleMutator<'_> {
    fn drop(&mut self) {
        if !self.registered {
            return;
        }
        // This also runs while foreign code unwinds; a second panic would abort.
        let (lock, cv) = &self.owner.gc.coord;
        let mut coord = lock.lock().unwrap_or_else(|poison| poison.into_inner());
        while coord.stop_requested {
            coord = cv.wait(coord).unwrap_or_else(|poison| poison.into_inner());
        }
        let id = std::thread::current().id();
        if let Some(published) = coord.mutators.get_mut(&id) {
            published.deferred = None;
        }
        coord.parked.remove(&id);
    }
}

/// Run `work` in a GC safe region: collections proceed without this thread,
/// which parks on return if a stop is still active.
///
/// # Safety
/// `work` must meet the [`IdleMutator::enter`] contract. Callers must not read
/// a raw managed pointer after this returns unless a pin keeps it in place.
pub(crate) unsafe fn run_blocking<T>(work: impl FnOnce() -> T) -> T {
    let _idle = unsafe { IdleMutator::enter() };
    work()
}

/// Foreign code inside a region can still request a collection. It runs as an
/// ordinary mutator, since parking there would otherwise end the region, then
/// the region resumes for whatever the foreign code does next.
pub(super) struct RegionReentry(bool);

impl RegionReentry {
    pub(super) fn enter() -> Self {
        let id = std::thread::current().id();
        let coord = runtime()
            .coord
            .0
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let inside = coord.parked.contains(&id);
        drop(coord);
        if inside {
            // SAFETY: a running thread is parked only by its own region.
            drop(unsafe { IdleMutator::from_raw(true) });
        }
        Self(inside)
    }
}

impl Drop for RegionReentry {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: the foreign caller already holds the region contract.
            unsafe { IdleMutator::enter() }.into_raw();
        }
    }
}

/// Run `collect` with the world stopped: request a safepoint, wait until every
/// other registered mutator has parked, then run `collect` (which scans all
/// published roots), then resume the world.
pub(super) fn with_stw<R>(
    reason: crate::gc_telemetry::stops::StopReason,
    collect: impl FnOnce(&GcCoord, &mut crate::gc_telemetry::stops::StopWorkV2) -> R,
) -> R {
    with_stw_in(crate::runtime::current(), reason, collect)
}

pub(super) fn with_stw_in<R>(
    owner: &crate::runtime::Runtime,
    reason: crate::gc_telemetry::stops::StopReason,
    collect: impl FnOnce(&GcCoord, &mut crate::gc_telemetry::stops::StopWorkV2) -> R,
) -> R {
    let mut measurement = crate::gc_telemetry::stops::StopMeasurement::begin_in(owner, reason);
    let (lock, cv) = &owner.gc.coord;
    let me = std::thread::current().id();
    // Publish the stop request on the lock-free gate first so mutators on the
    // hot path observe it at their next safepoint.
    owner
        .gc
        .stop_requested
        .store(true, std::sync::atomic::Ordering::Release);
    owner.gc.poll_requested.store(true, Ordering::Release);
    let mut coord = lock.lock().unwrap_or_else(|poison| poison.into_inner());
    coord.stop_requested = true;
    coord.stw_pending = coord
        .mutators
        .keys()
        .copied()
        .filter(|id| *id != me && !coord.parked.contains(id))
        .collect();
    owner.gc.budget_cv.notify_all();
    coord = wait_for_mutators_with_timeout(
        *owner.diagnostics.gc_handshake_timeout,
        cv,
        coord,
        "stop-the-world park",
        |coord| coord.stw_pending.is_empty(),
        |coord| coord.stw_pending.iter().copied().collect(),
    );
    // A collection can panic: the debug pointer validation aborts the cycle on
    // a corrupt root, and callers catch that. The world has to be resumed
    // either way — an unwind that leaves `stop_requested` set parks every other
    // mutator forever, and one that unwinds out of the guard poisons the
    // registry, taking every later collection down with it.
    measurement.stopped();
    let collector_start = crate::gc_telemetry::timestamp_ns();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _phase = crate::gc_telemetry::observations::StopPhase::new(
            &owner.telemetry.observations,
            reason as usize - 1,
        );
        let _interval = crate::gc_telemetry::observations::Span::for_runtime(
            owner,
            crate::gc_telemetry::observations::Kind::Collector,
        );
        for published in coord.mutators.values_mut() {
            published.materialize();
        }
        collect(&coord, measurement.work())
    }));
    let collector_end = crate::gc_telemetry::timestamp_ns();
    owner
        .telemetry
        .pause
        .switch(crate::gc_telemetry::pause::Component::Wakeup);
    let resume = crate::gc_telemetry::measurement::Span::for_runtime(
        owner,
        crate::gc_telemetry::measurement::Phase::Resume,
    );
    coord.stop_requested = false;
    owner
        .gc
        .stop_requested
        .store(false, std::sync::atomic::Ordering::Release);
    owner.gc.poll_requested.store(false, Ordering::Release);
    cv.notify_all();
    drop(coord);
    let unlocked = crate::gc_telemetry::timestamp_ns();
    drop(resume);
    measurement.finish(result.is_err());
    old_cycle::record_stop(
        &owner.gc,
        reason as u32,
        collector_start,
        collector_end,
        unlocked,
    );
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

#[cfg(test)]
#[path = "idle_tests.rs"]
mod idle_tests;

#[cfg(test)]
#[path = "mutator_owner_tests.rs"]
mod mutator_owner_tests;
