use super::*;

thread_local! {
    static OWNER_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static OWNER_EPOCH: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static ROOT_BASE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Generated cleanup depths are relative to the active task stack, independent
/// of how many roots the destination worker's caller has published.
pub(crate) fn replace_root_base(base: usize) -> usize {
    ROOT_BASE.replace(base)
}

/// Cold generated-code capacity path. It cannot collect or suspend: callers
/// may still hold unregistered incoming references.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_reserve_roots(count: usize) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    claim_root_stack_owner();
    ROOT_STACK.with(|roots| {
        let mut roots = roots.borrow_mut();
        if roots
            .len()
            .checked_add(count)
            .is_none_or(|n| n > i32::MAX as usize)
        {
            crate::failure::fatal_invariant("generated-code root depth overflow");
        }
        roots.reserve(count);
    });
}

/// Register a root slot.  `slot` must point to a stack location that holds
/// a GC-managed pointer.  The slot must remain valid until the matching pop.
/// Generated code reloads the slot after every GC point, so a minor
/// collection may move its referent and rewrite the slot.
///
/// # Safety
/// Supply an initialized, pointer-aligned slot containing null or a current managed payload
/// in this runtime. Keep the slot writable at the same address through its matching pop,
/// including while parked. Permit collector rewrites at safepoints and reload afterward.
///
/// ```compile_fail,E0133
/// let mut slot = std::ptr::null_mut::<u8>();
/// willow_runtime::gc::willow_push_root(&raw mut slot);
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_push_root(slot: *mut *mut u8) {
    // SAFETY: Forwarded caller contract.
    unsafe { push_root(slot) };
}

/// [`willow_push_root`] for runtime callers, which already run inside an FFI
/// boundary; pair it with [`pop_roots`].
///
/// # Safety
/// Same contract as [`willow_push_root`].
pub(crate) unsafe fn push_root(slot: *mut *mut u8) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    claim_root_stack_owner();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        stack.push(slot);
    });
}

/// Unregister the most recently pushed root slot.
///
/// # Safety
/// Every slot removed must belong to the caller. Preserve any roots still needed by
/// enclosing frames or guards. The slot storage must remain live through this call.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_pop_root() {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        stack.pop();
    });
    release_root_stack_owner_if_empty();
}

/// Unregister `count` root slots from the top of the root stack.
///
/// # Safety
/// Every slot removed must belong to the caller. Preserve any roots still needed by
/// enclosing frames or guards. The slot storage must remain live through this call.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_pop_roots(count: i32) {
    pop_roots(count as usize);
}

/// Runtime guards count slots at the native width of the shadow stack.
pub(crate) fn pop_roots(count: usize) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        let new_len = depth_after_pop(stack.len(), count);
        stack.truncate(new_len);
    });
    release_root_stack_owner_if_empty();
}

fn depth_after_pop(depth: usize, count: usize) -> usize {
    depth.saturating_sub(count)
}

/// Current generated-code root depth relative to the active task stack (or
/// the worker stack outside native tasks). Panic cleanup
/// records a lexical scope's entry depth and restores it on every shared
/// unwind edge, where the number of roots pushed before the panic is otherwise
/// path-dependent.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_root_depth() -> i32 {
    i32::try_from(crate::generated_context::depth() - ROOT_BASE.get()).unwrap_or_else(|_| {
        crate::failure::fatal_invariant("generated-code root depth overflow");
    })
}

/// Number of shadow roots on the running native stack.
pub(crate) fn gc_thread_root_depth() -> usize {
    crate::generated_context::depth()
}

// The caller owns live local slots or holds the suspended-stack registry lock.
// Publication neither traces nor safepoints, so stack transfer stays indivisible
// with respect to this thread's root handshake.
pub(super) fn retain_transferred_roots(slots: impl IntoIterator<Item = *mut *mut u8>) {
    if crate::runtime::current()
        .mark_phase()
        .load(Ordering::Acquire)
        == 0
    {
        return;
    }
    let state = runtime().heap.lock().unwrap();
    if let Some(cycle) = &state.concurrent_cycle {
        for slot in slots {
            if let Some(value) = RootSlot::from_raw(slot).and_then(RootSlot::load) {
                cycle.enqueue(value.as_ptr());
            }
        }
    }
}

/// Transfer a native task stack's roots to the collector before suspending it.
///
/// # Safety
/// Every slot in the suffix must remain allocated and unchanged until resume
/// or discard. The caller must switch stacks without a intervening safepoint.
pub(crate) unsafe fn park_current_roots(depth: usize) -> u64 {
    let token = runtime().next_parked_stack.fetch_add(1, Ordering::Relaxed);
    assert_ne!(token, 0, "parked native stack token exhausted");
    let mut parked = runtime().parked_stack_roots.lock().unwrap();
    let depth = depth
        .checked_sub(crate::generated_context::parent_depth())
        .expect("root depth below parent context");
    ROOT_STACK.with(|roots| {
        let mut roots = roots.borrow_mut();
        assert!(depth <= roots.len(), "native stack root depth mismatch");
        retain_transferred_roots(roots[depth..].iter().copied());
        parked.insert(
            token,
            PublishedRoots::current(roots[depth..].iter().map(|slot| *slot as usize).collect()),
        );
        // The scheduler context that takes over this thread is runtime code.
        set_relocation_safe(false);
        roots.truncate(depth);
    });
    release_root_stack_owner_if_empty();
    token
}

/// Reattach a suspended stack's shadow roots immediately before resuming it.
///
/// # Safety
/// `token` must own a still-live suspended native stack. The caller must resume
/// that stack without executing a safepoint against the wrong stack's slots.
pub(crate) unsafe fn resume_parked_roots(token: u64) {
    // Legacy admission takes coord; never acquire it while holding parked,
    // because stopped root snapshots take coord -> parked. Token ownership
    // keeps the entry stable across these two lock holds.
    if !current_thread_is_registered() {
        let has_roots = !runtime()
            .parked_stack_roots
            .lock()
            .unwrap()
            .get(&token)
            .expect("unknown parked native stack")
            .slots
            .is_empty();
        if has_roots {
            claim_root_stack_owner();
        }
    }
    let mut parked = runtime().parked_stack_roots.lock().unwrap();
    let PublishedRoots {
        slots, relocatable, ..
    } = parked.remove(&token).expect("unknown parked native stack");
    set_relocation_safe(relocatable);
    retain_transferred_roots(slots.iter().map(|&slot| slot as *mut *mut u8));
    ROOT_STACK.with(|roots| {
        let mut roots = roots.borrow_mut();
        roots.extend(slots.into_iter().map(|slot| slot as *mut *mut u8));
    });
}

#[cfg(test)]
/// Release roots only after the corresponding suspended stack was unwound.
///
/// # Safety
/// No live Willow frame may still depend on any root belonging to `token`.
pub(crate) unsafe fn discard_parked_roots(token: u64) {
    runtime()
        .parked_stack_roots
        .lock()
        .unwrap()
        .remove(&token)
        .expect("unknown parked native stack");
}

/// Keep a GC-managed object alive through a runtime-owned structure such as a
/// scheduler task, future frame, task handle, or wait queue.
///
/// The holder keeps the raw address, which a moving collection cannot rewrite,
/// so `object` must not be young: register only runtime allocations (always
/// old) or objects that already survived promotion. Old objects are never
/// moved. Registration queues its initial children for the next minor scan; subsequent reference
/// stores must use the GC write barrier. Minors scan remembered owners rather
/// than enumerating persistent address roots. Registering a young object is fatal. A holder of a
/// possibly young object registers its storage with
/// [`willow_gc_add_runtime_root_slot`] instead.
///
/// # Safety
/// A non-null object must be a live managed payload in the current runtime. A detected young
/// allocation triggers the fatal invariant before registration. Keep each accepted, nonmoving
/// address registered until its owner releases it exactly once.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_gc_add_runtime_root(object: *mut u8) {
    if object.is_null() {
        return;
    }
    add_address_root_in(runtime(), object, |_| true);
}

pub(crate) fn add_task_frame_root_in(owner: &crate::runtime::Runtime, frame: *mut u8) {
    add_address_root_in(&owner.gc, frame, frame_has_initial_references);
}

/// Type id of frames from `willow_async_frame_alloc`; no trace hook can claim it.
pub(super) const ASYNC_FRAME_TYPE_ID: u32 = 0;

/// Inline-traced frames are decided from their reference mask. Bitmap and
/// runtime-traced layouts keep the conservative scan obligation.
fn frame_has_initial_references(frame: *mut u8) -> bool {
    let Some(payload) = GcPayload::from_raw(frame) else {
        return false;
    };
    let object = HeapObject::from_payload(payload);
    let metadata = object.trace_metadata();
    metadata.type_id != ASYNC_FRAME_TYPE_ID
        || inline_reference_slots(object, metadata)
            // SAFETY: the mask bounds each slot within the live frame payload,
            // and this thread wrote them before any other thread can see it.
            .any(|slot| !unsafe { *slot }.is_null())
}

fn add_address_root_in(
    owner: &GcRuntime,
    object: *mut u8,
    owes_initial_scan: impl FnOnce(*mut u8) -> bool,
) {
    if object.is_null() {
        return;
    }

    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // Registry publication cannot safepoint before the insertion finishes.
    // The activation handshake crosses it before taking runtime roots, just
    // as it crosses the corresponding phase-gated SATB deletion below.
    // Only unregistered private service callers need explicit coordination.
    // Enrolled mutators are crossed by their owner's stop handshake.
    let _coord = (!owner.serves_current_thread()
        && !safepoint::current_thread_is_registered_in(owner))
    .then(|| owner.coord.0.lock().unwrap());
    let marking = owner.mark_phase().load(Ordering::Acquire) != 0;
    if owner.root_tlabs.generation(object as usize) == Some(GC_GENERATION_YOUNG) {
        crate::failure::fatal_invariant(
            "young object registered as an address runtime root; \
             use willow_gc_add_runtime_root_slot",
        );
    }
    if marking && let Some(cycle) = owner.heap.lock().unwrap().concurrent_cycle.as_ref() {
        cycle.enqueue(object);
    }
    if owner.tlab_ever_allocated.load(Ordering::Acquire) && owes_initial_scan(object) {
        owner.runtime_roots.add_pending(object);
    } else {
        owner.runtime_roots.add(object);
    }
}

/// Remove a persistent runtime root when the owning runtime structure no
/// longer needs to retain the object.
///
/// # Safety
/// A non-null object must be a live, nonmoving managed payload in the owning runtime. Pair
/// each registration with one removal while the address is still valid.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_gc_remove_runtime_root(object: *mut u8) {
    remove_runtime_root_in(crate::runtime::current(), object);
}

pub(crate) fn remove_runtime_root_in(owner: &crate::runtime::Runtime, object: *mut u8) {
    if object.is_null() {
        return;
    }

    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let _coord = (!std::ptr::eq(owner, crate::runtime::current())
        && !safepoint::current_thread_is_registered_in(&owner.gc))
    .then(|| owner.gc.coord.0.lock().unwrap());
    barrier::satb_delete_in(owner, object);
    owner.gc.runtime_roots.remove(object);
}

/// Keep the object stored in `*slot` alive through runtime-owned storage that
/// a moving collection may rewrite. A minor collection
/// evacuates a young referent and stores its new address in `*slot`; the
/// holder must reload the slot after any GC point instead of keeping a copy.
/// `slot` must stay valid and pointer-aligned until the matching
/// [`willow_gc_remove_runtime_root_slot`]. While registered, mutator replacements must
/// synchronize with collection and apply the root write barrier. Registration is counted per slot address.
///
/// # Safety
/// A non-null slot must be initialized to null or a live managed payload in the owning
/// runtime. Keep its aligned storage live at a stable address through matching removal for
/// every registration. Synchronize access with relocation; mutator replacements must use the root
/// write barrier.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_gc_add_runtime_root_slot(slot: *mut *mut u8) {
    // SAFETY: The ABI caller retains this registered slot in the current runtime through
    // removal.
    unsafe { add_runtime_root_slot_in(crate::runtime::current(), slot) };
}

/// # Safety
/// A non-null slot must be initialized to null or a live managed payload in the owning
/// runtime. Keep its aligned storage live at a stable address through matching removal for
/// every registration. Synchronize access with relocation; mutator replacements must use the root
/// write barrier.
pub(crate) unsafe fn add_runtime_root_slot_in(owner: &crate::runtime::Runtime, slot: *mut *mut u8) {
    if slot.is_null() {
        return;
    }
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let default_owner = std::ptr::eq(owner, crate::runtime::current());
    let _coord = (!default_owner && !safepoint::current_thread_is_registered_in(&owner.gc))
        .then(|| owner.gc.coord.0.lock().unwrap());
    // As for address roots: publication cannot cross a safepoint, and an
    // active cycle must see the referent even if the holder drops it later.
    let phase = owner.mark_phase();
    if phase.load(Ordering::Acquire) != 0
        && let Some(cycle) = owner.gc.heap.lock().unwrap().concurrent_cycle.as_ref()
    {
        // SAFETY: the caller guarantees a valid slot.
        cycle.enqueue(unsafe { *slot });
    }
    RuntimePathObservation::root_slot_registration(|| &owner.gc.path_observation);
    // A registered mutator logs the registration without a shared lock; the
    // next stop applies it before any root snapshot or slot rewrite.
    if !mutator_log::record_root_in(owner, slot as usize, 1) {
        mutator_log::count_shared_lock();
        owner.gc.runtime_root_slots.adjust(slot.cast(), 1);
    }
}

/// Stop retaining the referent of a slot registered with
/// [`willow_gc_add_runtime_root_slot`].
///
/// # Safety
/// A non-null slot must be initialized to null or a live managed payload in the owning
/// runtime. Keep its aligned storage live at a stable address through matching removal for
/// every registration. Synchronize access with relocation; mutator replacements must use the root
/// write barrier.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_gc_remove_runtime_root_slot(slot: *mut *mut u8) {
    // SAFETY: The ABI caller retains this registered slot in the current runtime through
    // removal.
    unsafe { remove_runtime_root_slot_in(crate::runtime::current(), slot) };
}

/// # Safety
/// A non-null slot must be initialized to null or a live managed payload in the owning
/// runtime. Keep its aligned storage live at a stable address through matching removal for
/// every registration. Synchronize access with relocation; mutator replacements must use the root
/// write barrier.
pub(crate) unsafe fn remove_runtime_root_slot_in(
    owner: &crate::runtime::Runtime,
    slot: *mut *mut u8,
) {
    if slot.is_null() {
        return;
    }
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    let default_owner = std::ptr::eq(owner, crate::runtime::current());
    let _coord = (!default_owner && !safepoint::current_thread_is_registered_in(&owner.gc))
        .then(|| owner.gc.coord.0.lock().unwrap());
    // SAFETY: the slot stays valid until this removal returns.
    barrier::satb_delete_in(owner, unsafe { *slot });
    // The registration may sit in another worker's log after its task moved;
    // signed deltas let the two halves meet in the shared set in any order.
    if !mutator_log::record_root_in(owner, slot as usize, -1) {
        mutator_log::count_shared_lock();
        owner.gc.runtime_root_slots.adjust(slot.cast(), -1);
    }
}

fn legacy_owner_is_current(gc: &GcRuntime) -> bool {
    OWNER_ID.get() == gc.policy_identity
        && OWNER_EPOCH.get() == gc.mutator_epoch.load(Ordering::Acquire)
}

pub(crate) fn generated_roots_admitted() -> bool {
    current_thread_is_registered() || legacy_owner_is_current(runtime())
}
pub(crate) fn leave_generated_roots() {
    release_root_stack_owner_if_empty();
}

pub(super) fn claim_root_stack_owner() {
    // Registered mutators are coordinated via the registry + STW, not the
    // single-owner guard.
    if current_thread_is_registered() {
        return;
    }
    // Only the owning thread clears its own claim, so an existing claim stays
    // valid without coord; collectors already defer to it.
    if legacy_owner_is_current(runtime()) {
        return;
    }
    let coord = runtime().coord.0.lock().unwrap();
    claim_legacy_owner_locked(&coord, true);
}

// Admission and collector election share coord. A raw, unpublished reference
// cannot safely wait across a collection; reject misuse before publishing it.
pub(super) fn claim_legacy_owner_locked(coord: &GcCoord, already_unregistered: bool) {
    claim_legacy_owner_locked_in(runtime(), coord, already_unregistered);
}

pub(super) fn claim_legacy_owner_locked_in(
    gc: &GcRuntime,
    coord: &GcCoord,
    already_unregistered: bool,
) {
    RuntimePathObservation::root_owner_slow(|| &gc.path_observation);
    let current = std::thread::current().id();
    let mut owner = gc.root_stack_owner.lock().unwrap();
    // Existing legacy ownership already requires a foreign collector to defer.
    if already_unregistered && *owner == Some(current) {
        OWNER_ID.set(gc.policy_identity);
        OWNER_EPOCH.set(gc.mutator_epoch.load(Ordering::Acquire));
        return;
    }
    if coord
        .cycle_collector
        .is_some_and(|collector| collector != current)
    {
        crate::failure::fatal_invariant(
            "unregistered root ownership during GC; enter NativeGcScope before acquiring references",
        );
    }
    match *owner {
        Some(existing) if existing != current => {
            eprintln!("willow gc: explicit root stacks are single-mutator in the current runtime");
            crate::failure::fatal_invariant("multiple unregistered root-stack owners");
        }
        _ => *owner = Some(current),
    }
    OWNER_ID.set(gc.policy_identity);
    OWNER_EPOCH.set(gc.mutator_epoch.load(Ordering::Acquire));
}

pub(super) fn release_root_stack_owner_if_empty() {
    if current_thread_is_registered() {
        return;
    }
    clear_root_stack_owner_if_empty();
}

// Does not reacquire coord: unregister calls this while holding that lock.
pub(super) fn clear_root_stack_owner_if_empty() {
    clear_root_stack_owner_if_empty_in(runtime());
}

pub(super) fn clear_root_stack_owner_if_empty_in(gc: &GcRuntime) {
    if crate::generated_context::has_active_bridge()
        || gc_thread_root_depth() != 0
        || !legacy_owner_is_current(gc)
    {
        return;
    }
    RuntimePathObservation::root_owner_slow(|| &gc.path_observation);
    let current = std::thread::current().id();
    let mut owner = gc.root_stack_owner.lock().unwrap();
    if owner.as_ref().is_some_and(|existing| *existing == current) {
        *owner = None;
    }
    OWNER_ID.set(0);
    OWNER_EPOCH.set(0);
}

#[cfg(test)]
pub(super) fn foreign_root_stack_owner_active() -> bool {
    foreign_root_stack_owner_active_in(runtime())
}

pub(super) fn foreign_root_stack_owner_active_in(owner: &GcRuntime) -> bool {
    foreign_root_stack_owner_active_locked_in(owner, &owner.coord.0.lock().unwrap())
}

pub(super) fn foreign_root_stack_owner_active_locked_in(
    owner: &GcRuntime,
    coord: &GcCoord,
) -> bool {
    let current = std::thread::current().id();
    owner
        .root_stack_owner
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|owner| *owner != current && !coord.mutators.contains_key(owner))
}

/// Number of distinct runtime-rooted objects and registered runtime root
/// slots. Acceptance tests use this to prove that panic/recover releases every
/// root it took, instead of only checking that the program printed the right
/// text.
pub fn runtime_root_count() -> usize {
    runtime_root_count_in(runtime())
}

/// Address roots plus live slot registrations, including registrations still
/// held in mutator logs. Exact when no mutator is concurrently adding or
/// removing slots; otherwise in-flight log deltas make the slot part an
/// approximation (see `net_root_slots`), clamped at zero because a read can
/// see a migrated remove without its add.
pub(crate) fn runtime_root_count_in(owner: &GcRuntime) -> usize {
    let slots = mutator_log::net_root_slots(owner);
    owner.runtime_roots.len() + slots.max(0).unsigned_abs()
}

pub(super) fn runtime_roots_snapshot_in(owner: &GcRuntime) -> Vec<*mut u8> {
    let mut roots = owner.runtime_roots.snapshot();
    // SAFETY: runtime slot registration keeps each aligned slot alive until
    // removal; snapshot_slot_values holds the registry locks through atomic
    // reads so a concurrent handle Drop cannot free an enumerated cell.
    roots.extend(unsafe { owner.runtime_root_slots.snapshot_slot_values() });
    let parked = owner.parked_stack_roots.lock().unwrap();
    roots.extend(
        parked
            .values()
            .flat_map(|published| published.slots.iter())
            .filter_map(|&slot| RootSlot::from_raw(slot as *mut *mut u8).and_then(RootSlot::load))
            .map(GcPayload::as_ptr),
    );
    roots
}

/// Slots registered with [`willow_gc_add_runtime_root_slot`]. A relocating
/// collector rewrites them; address-registered roots are never young and are
/// only scanned in place.
pub(super) fn runtime_root_slots(owner: &GcRuntime) -> Vec<*mut *mut u8> {
    owner
        .runtime_root_slots
        .snapshot()
        .into_iter()
        .map(|slot| slot.cast())
        .collect()
}

/// Roots of parked native stacks. The park contract keeps each slot allocated
/// until its stack resumes or is discarded, and the stack cannot run while a
/// stop-the-world collection rewrites it.
pub(super) fn append_parked_roots(owner: &GcRuntime, roots: &mut MinorRoots) {
    let parked = owner.parked_stack_roots.lock().unwrap();
    for published in parked.values() {
        published.append_to(roots);
    }
}

#[cfg(test)]
pub(crate) fn parked_root_counts_for_test() -> (usize, usize) {
    let roots = runtime().parked_stack_roots.lock().unwrap();
    (
        roots.len(),
        roots.values().map(|roots| roots.slots.len()).sum(),
    )
}

#[cfg(test)]
mod inline_root_tests {
    use super::*;

    #[test]
    fn native_root_pop_preserves_prefix_at_wide_counts() {
        let signed_limit = i32::MAX as usize;
        let largest_part_count = isize::MAX as usize / size_of::<[u64; 2]>();
        for count in [
            0,
            1,
            signed_limit,
            signed_limit + 1,
            1 << 32,
            largest_part_count,
        ] {
            for caller_depth in [0, 1, 13] {
                assert_eq!(depth_after_pop(caller_depth + count, count), caller_depth);
            }
        }
    }

    #[test]
    fn private_root_slot_publication_and_removal_use_selected_cycle() {
        for phase in [0, 1, 2] {
            let first = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
            let second = crate::runtime::Runtime::new(crate::scheduler::environment_worker_config);
            let mut shared = std::ptr::null_mut();
            let shared_slot = &raw mut shared;
            // SAFETY: Both runtimes may retain this stable null slot. Neither can relocate null.
            unsafe {
                add_runtime_root_slot_in(&first, shared_slot);
                add_runtime_root_slot_in(&second, shared_slot);
                remove_runtime_root_slot_in(&first, shared_slot);
            }
            assert!(runtime_root_slots(&first.gc).is_empty());
            assert_eq!(runtime_root_slots(&second.gc), vec![shared_slot]);

            // SAFETY: The allocation path forwards the validated payload extent, type and
            // tracing layout; its caller initializes native fields before collection.
            let mut inserted = unsafe { allocate_old_in(&first, 0, 0, 8, 0) };
            // SAFETY: The allocation path forwards the validated payload extent, type and
            // tracing layout; its caller initializes native fields before collection.
            let mut deleted = unsafe { allocate_old_in(&first, 0, 0, 8, 0) };
            let insertion_slot = &raw mut inserted;
            let deletion_slot = &raw mut deleted;
            // SAFETY: These slots hold distinct live objects of first. Register the deletion
            // edge before marking so only its removal can publish it to the cycle.
            unsafe { add_runtime_root_slot_in(&first, deletion_slot) };
            let cycle = Arc::new(ConcurrentCycle::new(
                std::iter::empty(),
                Default::default(),
                Default::default(),
                0,
            ));
            cycle.late_published.store(false, Ordering::Release);
            marking::set_concurrent_cycle_in(
                &first.gc,
                &mut first.gc.heap.lock().unwrap(),
                Some(cycle.clone()),
            );
            first.gc.mark_phase().store(phase, Ordering::Release);
            // SAFETY: Both local slots stay live in first until their respective removals.
            unsafe { add_runtime_root_slot_in(&first, insertion_slot) };
            assert_eq!(
                cycle.unindexed.lock().unwrap().len(),
                usize::from(phase != 0)
            );
            // SAFETY: Remove the root that was registered before the cycle began.
            unsafe { remove_runtime_root_slot_in(&first, deletion_slot) };
            assert_eq!(
                cycle.unindexed.lock().unwrap().len(),
                2 * usize::from(phase != 0)
            );
            // SAFETY: Each owner releases its own remaining slot before storage leaves scope.
            unsafe {
                remove_runtime_root_slot_in(&first, insertion_slot);
                remove_runtime_root_slot_in(&second, shared_slot);
            }
            assert!(runtime_root_slots(&first.gc).is_empty());
            assert!(runtime_root_slots(&second.gc).is_empty());
            first.gc.mark_phase().store(0, Ordering::Release);
            marking::set_concurrent_cycle_in(&first.gc, &mut first.gc.heap.lock().unwrap(), None);
        }
    }

    #[test]
    fn private_foreign_root_admission_uses_only_its_owner_registry() {
        let first = GcRuntime::default();
        let second = GcRuntime::default();
        let foreign = std::thread::spawn(crate::runtime::serving(|| std::thread::current().id()))
            .join()
            .unwrap();
        *first.root_stack_owner.lock().unwrap() = Some(foreign);
        assert!(foreign_root_stack_owner_active_in(&first));
        assert!(!foreign_root_stack_owner_active_in(&second));
        second
            .coord
            .0
            .lock()
            .unwrap()
            .mutators
            .insert(foreign, Default::default());
        assert!(foreign_root_stack_owner_active_in(&first));
        first
            .coord
            .0
            .lock()
            .unwrap()
            .mutators
            .insert(foreign, Default::default());
        assert!(!foreign_root_stack_owner_active_in(&first));
        first.coord.0.lock().unwrap().mutators.remove(&foreign);
        assert!(foreign_root_stack_owner_active_in(&first));
        *first.root_stack_owner.lock().unwrap() = Some(std::thread::current().id());
        assert!(!foreign_root_stack_owner_active_in(&first));
    }

    #[test]
    fn private_runtime_root_snapshots_keep_owner_slots_separate() {
        let first = GcRuntime::default();
        let second = GcRuntime::default();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            for owner in [&first, &second] {
                let barrier = &barrier;
                scope.spawn(crate::runtime::serving(move || {
                    let mut payload = 0u8;
                    let payload = &raw mut payload;
                    let mut value = payload;
                    let slot = &raw mut value;
                    owner.runtime_roots.add(payload);
                    owner.runtime_root_slots.add(slot.cast());
                    barrier.wait();
                    assert_eq!(runtime_root_slots(owner), vec![slot]);
                    assert_eq!(runtime_roots_snapshot_in(owner), vec![payload, payload]);
                    barrier.wait();
                    owner.runtime_root_slots.remove(slot.cast());
                    owner.runtime_roots.remove(payload);
                    assert!(runtime_root_slots(owner).is_empty());
                    assert!(runtime_roots_snapshot_in(owner).is_empty());
                }));
            }
        });
    }

    #[test]
    fn runtime_root_registration_publishes_during_marking() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let object = willow_alloc_typed(8, 0);
        let index = epoch_index::RegionIndex::capture(&runtime().heap.lock().unwrap());
        let cycle = Arc::new(ConcurrentCycle::with_index(
            epoch_index::EpochIndex::regions(index),
            IdSet::default(),
            IdMap::default(),
            0,
        ));
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), Some(cycle.clone()));
        crate::runtime::current()
            .mark_phase()
            .store(1, Ordering::Release);
        assert!(!cycle.is_marked(object as usize));
        // SAFETY: The runtime keeps this nonmoving payload at its registered address through
        // removal.
        unsafe { willow_gc_add_runtime_root(object) };
        cycle.drain(usize::MAX);
        assert!(cycle.is_marked(object as usize));
        // SAFETY: The runtime keeps this nonmoving payload at its registered address through
        // removal.
        unsafe { willow_gc_remove_runtime_root(object) };
        crate::runtime::current()
            .mark_phase()
            .store(0, Ordering::Release);
        marking::set_concurrent_cycle(&mut runtime().heap.lock().unwrap(), None);
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
    }

    #[test]
    fn runtime_root_registration_with_nursery_does_not_lock_heap() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let tls = tlab_state_for_test();
        // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size
        // and tracing metadata describe the object initialized here.
        unsafe { willow_gc_alloc_slow(tls.as_ptr(), 0, 0, 8, 0) };
        let object = willow_alloc_typed(8, 0);
        assert_eq!(
            crate::runtime::current()
                .mark_phase()
                .load(Ordering::Acquire),
            0
        );
        // A recursive heap acquisition would deadlock: registration/removal
        // must finish while this caller already owns the mutex.
        let state = runtime().heap.lock().unwrap();
        for _ in 0..1024 {
            // SAFETY: The runtime keeps this nonmoving payload at its registered address
            // through removal.
            unsafe { willow_gc_add_runtime_root(object) };
        }
        assert_eq!(runtime_root_count(), 1);
        for _ in 0..1024 {
            // SAFETY: The runtime keeps this nonmoving payload at its registered address
            // through removal.
            unsafe { willow_gc_remove_runtime_root(object) };
        }
        assert_eq!(runtime_root_count(), 0);
        drop(state);
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
    }

    #[test]
    fn owner_cached_push_pop_has_two_slow_paths_per_live_stack() {
        let _guard = isolated_runtime();
        for count in [1, 16, 256, 4096] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { reset_internal_for_test() };
            runtime().path_observation.set_root_owner_slow_paths(0);
            let mut slots = vec![std::ptr::null_mut(); count];
            for slot in &mut slots {
                // SAFETY: The slot storage stays live and stationary through the matching
                // pop, including during collection.
                unsafe { willow_push_root(slot) };
            }
            assert_eq!(
                runtime().path_observation.root_owner_slow_paths(),
                1,
                "first claim only: {count}"
            );
            for _ in 1..count {
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { willow_pop_root() };
            }
            assert_eq!(
                runtime().path_observation.root_owner_slow_paths(),
                1,
                "nonfinal pops: {count}"
            );
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_root() };
            assert_eq!(
                runtime().path_observation.root_owner_slow_paths(),
                2,
                "final release: {count}"
            );
        }
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
    }

    unsafe extern "C" fn empty_scope_loop(_: *mut std::ffi::c_void) -> i32 {
        let mut slot = std::ptr::null_mut();
        for _ in 0..4096 {
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut slot) };
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_root() };
            assert_eq!(runtime().path_observation.root_owner_slow_paths(), 1);
        }
        1
    }

    #[test]
    fn owner_bridge_retains_one_claim_across_empty_scopes() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        runtime().path_observation.set_root_owner_slow_paths(0);
        assert_eq!(
            // SAFETY: The test entry has the bridge callback signature and ignores its null frame.
            unsafe {
                crate::generated_context::invoke(
                    empty_scope_loop as *const (),
                    std::ptr::null_mut(),
                )
            },
            1
        );
        assert_eq!(runtime().path_observation.root_owner_slow_paths(), 2);
        assert!(!generated_roots_admitted());
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
    }

    #[test]
    fn owner_registered_push_pop_never_enters_owner_slow_path() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let _registration = MutatorRegistration::new();
        runtime().path_observation.set_root_owner_slow_paths(0);
        let mut slot = std::ptr::null_mut();
        for _ in 0..4096 {
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut slot) };
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_root() };
        }
        assert_eq!(runtime().path_observation.root_owner_slow_paths(), 0);
    }
}
