use super::*;

thread_local! {
    #[cfg(test)]
    static OWNER_SLOW_PATHS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_push_root(slot: *mut *mut u8) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    claim_root_stack_owner();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        stack.push(slot);
    });
}

/// Unregister the most recently pushed root slot.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_pop_root() {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        stack.pop();
    });
    release_root_stack_owner_if_empty();
}

/// Unregister `count` root slots from the top of the root stack.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_pop_roots(count: i32) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        let remove = (count as usize).min(stack.len());
        let new_len = stack.len() - remove;
        stack.truncate(new_len);
    });
    release_root_stack_owner_if_empty();
}

/// Current generated-code root depth relative to the active task stack (or
/// the worker stack outside native tasks). Panic cleanup
/// records a lexical scope's entry depth and restores it on every shared
/// unwind edge, where the number of roots pushed before the panic is otherwise
/// path-dependent (willow-s9ej.3).
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
    if GC_MARK_PHASE.load(Ordering::Acquire) == 0 {
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
/// moved. Registration remembers its initial children; subsequent reference
/// stores must use the GC write barrier. Minors scan remembered owners rather
/// than enumerating persistent address roots. Registering a young object is fatal. A holder of a
/// possibly young object registers its storage with
/// [`willow_gc_add_runtime_root_slot`] instead.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_add_runtime_root(object: *mut u8) {
    if object.is_null() {
        return;
    }

    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // Registry publication cannot safepoint before the insertion finishes.
    // The activation handshake crosses it before taking runtime roots, just
    // as it crosses the corresponding phase-gated SATB deletion below.
    let marking = GC_MARK_PHASE.load(Ordering::Acquire) != 0;
    if marking || runtime().tlab_ever_allocated.load(Ordering::Acquire) {
        let mut state = runtime().heap.lock().unwrap();
        if tlab_payload_generation(&state, object as usize) == Some(GC_GENERATION_YOUNG) {
            drop(state);
            crate::failure::fatal_invariant(
                "young object registered as an address runtime root; \
                 use willow_gc_add_runtime_root_slot",
            );
        }
        if marking && let Some(cycle) = state.concurrent_cycle.as_ref() {
            cycle.enqueue(object);
        }
        if runtime().tlab_ever_allocated.load(Ordering::Acquire) {
            // Debug root diagnostics belong to collection. Do not dereference
            // an invalid root while remembering it merely because a TLAB exists.
            #[cfg(debug_assertions)]
            let valid = validate_payload_pointer_locked(&state, object).is_ok();
            #[cfg(not(debug_assertions))]
            let valid = true;
            if valid {
                barrier::remember_owner(&mut state, object as usize);
            }
        }
    }
    runtime().runtime_roots.add(object);
}

/// Remove a persistent runtime root when the owning runtime structure no
/// longer needs to retain the object.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_remove_runtime_root(object: *mut u8) {
    if object.is_null() {
        return;
    }

    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    satb_delete(object);
    runtime().runtime_roots.remove(object);
}

/// Keep the object stored in `*slot` alive through runtime-owned storage that
/// a moving collection may rewrite (willow-9tls.9). A minor collection
/// evacuates a young referent and stores its new address in `*slot`; the
/// holder must reload the slot after any GC point instead of keeping a copy.
/// `slot` must stay valid and pointer-aligned until the matching
/// [`willow_gc_remove_runtime_root_slot`]. While registered, only the
/// collector writes `*slot`. Registration is counted per slot address.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_add_runtime_root_slot(slot: *mut *mut u8) {
    if slot.is_null() {
        return;
    }
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // As for address roots: publication cannot cross a safepoint, and an
    // active cycle must see the referent even if the holder drops it later.
    if GC_MARK_PHASE.load(Ordering::Acquire) != 0
        && let Some(cycle) = runtime().heap.lock().unwrap().concurrent_cycle.as_ref()
    {
        // SAFETY: the caller guarantees a valid slot.
        cycle.enqueue(unsafe { *slot });
    }
    runtime().runtime_root_slots.add(slot.cast());
}

/// Stop retaining the referent of a slot registered with
/// [`willow_gc_add_runtime_root_slot`].
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_remove_runtime_root_slot(slot: *mut *mut u8) {
    if slot.is_null() {
        return;
    }
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // SAFETY: the slot stays valid until this removal returns.
    satb_delete(unsafe { *slot });
    runtime().runtime_root_slots.remove(slot.cast());
}

pub(crate) fn generated_roots_admitted() -> bool {
    current_thread_is_registered()
        || OWNER_EPOCH.get() == safepoint::MUTATOR_EPOCH.load(Ordering::Acquire)
}
pub(crate) fn leave_generated_roots() {
    release_root_stack_owner_if_empty();
}

pub(super) fn claim_root_stack_owner() {
    // Registered mutators are coordinated via the registry + STW, not the
    // single-owner guard (willow-6fv.5.6).
    if current_thread_is_registered() {
        return;
    }
    // Only the owning thread clears its own claim, so an existing claim stays
    // valid without coord; collectors already defer to it.
    if OWNER_EPOCH.get() == safepoint::MUTATOR_EPOCH.load(Ordering::Acquire) {
        return;
    }
    let coord = runtime().coord.0.lock().unwrap();
    claim_legacy_owner_locked(&coord, true);
}

// Admission and collector election share coord. A raw, unpublished reference
// cannot safely wait across a collection; reject misuse before publishing it.
pub(super) fn claim_legacy_owner_locked(coord: &GcCoord, already_unregistered: bool) {
    #[cfg(test)]
    OWNER_SLOW_PATHS.set(OWNER_SLOW_PATHS.get() + 1);
    let current = std::thread::current().id();
    let mut owner = runtime().root_stack_owner.lock().unwrap();
    // Existing legacy ownership already requires a foreign collector to defer.
    if already_unregistered && *owner == Some(current) {
        OWNER_EPOCH.set(safepoint::MUTATOR_EPOCH.load(Ordering::Acquire));
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
    OWNER_EPOCH.set(safepoint::MUTATOR_EPOCH.load(Ordering::Acquire));
}

pub(super) fn release_root_stack_owner_if_empty() {
    if current_thread_is_registered() {
        return;
    }
    clear_root_stack_owner_if_empty();
}

// Does not reacquire coord: unregister calls this while holding that lock.
pub(super) fn clear_root_stack_owner_if_empty() {
    if crate::generated_context::has_active_bridge()
        || gc_thread_root_depth() != 0
        || OWNER_EPOCH.get() != safepoint::MUTATOR_EPOCH.load(Ordering::Acquire)
    {
        return;
    }
    #[cfg(test)]
    OWNER_SLOW_PATHS.set(OWNER_SLOW_PATHS.get() + 1);
    let current = std::thread::current().id();
    let mut owner = runtime().root_stack_owner.lock().unwrap();
    if owner.as_ref().is_some_and(|existing| *existing == current) {
        *owner = None;
    }
    OWNER_EPOCH.set(0);
}

pub(super) fn foreign_root_stack_owner_active() -> bool {
    foreign_root_stack_owner_active_locked(&runtime().coord.0.lock().unwrap())
}

/// Unregistration also runs under coord, so a collector that decides under the
/// same lock hold cannot miss a legacy owner that leaves the registry in between.
pub(super) fn foreign_root_stack_owner_active_locked(coord: &GcCoord) -> bool {
    let current = std::thread::current().id();
    runtime()
        .root_stack_owner
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|owner| *owner != current && !coord.mutators.contains_key(owner))
}

/// Number of distinct runtime-rooted objects and registered runtime root
/// slots. Acceptance tests use this to prove that panic/recover releases every
/// root it took, instead of only checking that the program printed the right
/// text (willow-s9ej.7).
pub fn runtime_root_count() -> usize {
    runtime().runtime_roots.len() + runtime().runtime_root_slots.len()
}

/// Values of every runtime-held root: address-registered runtime roots,
/// registered runtime root slots, and the slots of parked native stacks.
/// Marking consumers only need values.
pub(super) fn runtime_roots_snapshot() -> Vec<*mut u8> {
    let mut roots = runtime().runtime_roots.snapshot();
    // SAFETY: runtime slot registration keeps each aligned slot alive until
    // removal; snapshot_slot_values holds the registry locks through atomic
    // reads so a concurrent handle Drop cannot free an enumerated cell.
    roots.extend(unsafe { runtime().runtime_root_slots.snapshot_slot_values() });
    let parked = runtime().parked_stack_roots.lock().unwrap();
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
pub(super) fn runtime_root_slots() -> Vec<*mut *mut u8> {
    runtime()
        .runtime_root_slots
        .snapshot()
        .into_iter()
        .map(|slot| slot.cast())
        .collect()
}

/// Roots of parked native stacks. The park contract keeps each slot allocated
/// until its stack resumes or is discarded, and the stack cannot run while a
/// stop-the-world collection rewrites it.
pub(super) fn append_parked_roots(roots: &mut MinorRoots) {
    let parked = runtime().parked_stack_roots.lock().unwrap();
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
    fn owner_cached_push_pop_has_two_slow_paths_per_live_stack() {
        let _guard = runtime_test_guard();
        for count in [1, 16, 256, 4096] {
            reset_internal_for_test();
            OWNER_SLOW_PATHS.set(0);
            let mut slots = vec![std::ptr::null_mut(); count];
            for slot in &mut slots {
                willow_push_root(slot);
            }
            assert_eq!(OWNER_SLOW_PATHS.get(), 1, "first claim only: {count}");
            for _ in 1..count {
                willow_pop_root();
            }
            assert_eq!(OWNER_SLOW_PATHS.get(), 1, "nonfinal pops: {count}");
            willow_pop_root();
            assert_eq!(OWNER_SLOW_PATHS.get(), 2, "final release: {count}");
        }
        reset_internal_for_test();
    }

    unsafe extern "C" fn empty_scope_loop(_: *mut std::ffi::c_void) -> i32 {
        let mut slot = std::ptr::null_mut();
        for _ in 0..4096 {
            willow_push_root(&mut slot);
            willow_pop_root();
            assert_eq!(OWNER_SLOW_PATHS.get(), 1);
        }
        1
    }

    #[test]
    fn owner_bridge_retains_one_claim_across_empty_scopes() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        OWNER_SLOW_PATHS.set(0);
        assert_eq!(
            unsafe {
                crate::generated_context::invoke(
                    empty_scope_loop as *const (),
                    std::ptr::null_mut(),
                )
            },
            1
        );
        assert_eq!(OWNER_SLOW_PATHS.get(), 2);
        assert!(!generated_roots_admitted());
        reset_internal_for_test();
    }

    #[test]
    fn owner_registered_push_pop_never_enters_owner_slow_path() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let _registration = MutatorRegistration::new();
        OWNER_SLOW_PATHS.set(0);
        let mut slot = std::ptr::null_mut();
        for _ in 0..4096 {
            willow_push_root(&mut slot);
            willow_pop_root();
        }
        assert_eq!(OWNER_SLOW_PATHS.get(), 0);
    }
}
