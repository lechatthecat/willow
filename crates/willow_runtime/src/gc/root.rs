use super::*;

/// Register a root slot.  `slot` must point to a stack location that holds
/// a GC-managed pointer.  The slot must remain valid until the matching pop.
#[unsafe(no_mangle)]
pub extern "C" fn willow_push_root(slot: *mut *mut u8) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    claim_root_stack_owner();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        stack.push(slot);
        ROOT_DEPTH.set(stack.len());
    });
}

/// Unregister the most recently pushed root slot.
#[unsafe(no_mangle)]
pub extern "C" fn willow_pop_root() {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        stack.pop();
        ROOT_DEPTH.set(stack.len());
    });
    release_root_stack_owner_if_empty();
}

/// Unregister `count` root slots from the top of the root stack.
#[unsafe(no_mangle)]
pub extern "C" fn willow_pop_roots(count: i32) {
    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        let remove = (count as usize).min(stack.len());
        let new_len = stack.len() - remove;
        stack.truncate(new_len);
        ROOT_DEPTH.set(stack.len());
    });
    release_root_stack_owner_if_empty();
}

/// Current generated-code shadow-root depth for this mutator. Panic cleanup
/// records a lexical scope's entry depth and restores it on every shared
/// unwind edge, where the number of roots pushed before the panic is otherwise
/// path-dependent (willow-s9ej.3).
#[unsafe(no_mangle)]
pub extern "C" fn willow_root_depth() -> i32 {
    i32::try_from(ROOT_DEPTH.get()).unwrap_or_else(|_| {
        eprintln!("runtime fatal: generated-code root depth overflow");
        std::process::abort();
    })
}

/// Number of shadow roots on the running native stack.
pub(crate) fn gc_thread_root_depth() -> usize {
    ROOT_DEPTH.get()
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
    ROOT_STACK.with(|roots| {
        let mut roots = roots.borrow_mut();
        assert!(depth <= roots.len(), "native stack root depth mismatch");
        retain_transferred_roots(roots[depth..].iter().copied());
        parked.insert(
            token,
            roots[depth..].iter().map(|slot| *slot as usize).collect(),
        );
        roots.truncate(depth);
        ROOT_DEPTH.set(roots.len());
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
    let mut parked = runtime().parked_stack_roots.lock().unwrap();
    let slots = parked.remove(&token).expect("unknown parked native stack");
    retain_transferred_roots(slots.iter().map(|&slot| slot as *mut *mut u8));
    if !slots.is_empty() {
        claim_root_stack_owner();
    }
    ROOT_STACK.with(|roots| {
        let mut roots = roots.borrow_mut();
        roots.extend(slots.into_iter().map(|slot| slot as *mut *mut u8));
        ROOT_DEPTH.set(roots.len());
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
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_add_runtime_root(object: *mut u8) {
    if object.is_null() {
        return;
    }

    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    // Registry publication cannot safepoint before the insertion finishes.
    // The activation handshake crosses it before taking runtime roots, just
    // as it crosses the corresponding phase-gated SATB deletion below.
    if GC_MARK_PHASE.load(Ordering::Acquire) != 0
        && let Some(cycle) = runtime().heap.lock().unwrap().concurrent_cycle.as_ref()
    {
        cycle.enqueue(object);
    }
    runtime().runtime_roots.add(object);
}

/// Remove a persistent runtime root when the owning runtime structure no
/// longer needs to retain the object.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_remove_runtime_root(object: *mut u8) {
    if object.is_null() {
        return;
    }

    let _no_preempt = crate::preempt::NoPreemptGuard::enter();
    satb_delete(object);
    runtime().runtime_roots.remove(object);
}

/// Registered mutators each legitimately own their own thread-local root stack;
/// cross-thread safety is handled by stop-the-world scanning, so they bypass the
/// legacy single-mutator `runtime().root_stack_owner` guard below.
pub(super) fn current_thread_is_registered() -> bool {
    let current = std::thread::current().id();
    let (lock, _) = &runtime().coord;
    lock.lock().unwrap().mutators.contains_key(&current)
}

pub(super) fn claim_root_stack_owner() {
    // Registered mutators are coordinated via the registry + STW, not the
    // single-owner guard (willow-6fv.5.6).
    if current_thread_is_registered() {
        return;
    }
    let current = std::thread::current().id();
    let mut owner = runtime().root_stack_owner.lock().unwrap();
    match *owner {
        Some(existing) if existing != current => {
            eprintln!("willow gc: explicit root stacks are single-mutator in the current runtime");
            std::process::abort();
        }
        _ => *owner = Some(current),
    }
}

pub(super) fn release_root_stack_owner_if_empty() {
    if current_thread_is_registered() {
        return;
    }
    clear_root_stack_owner_if_empty();
}

// Does not reacquire coord: unregister calls this while holding that lock.
pub(super) fn clear_root_stack_owner_if_empty() {
    let is_empty = ROOT_STACK.with(|rs| rs.borrow().is_empty());
    if !is_empty {
        return;
    }
    let current = std::thread::current().id();
    let mut owner = runtime().root_stack_owner.lock().unwrap();
    if owner.as_ref().is_some_and(|existing| *existing == current) {
        *owner = None;
    }
}

pub(super) fn foreign_root_stack_owner_active() -> bool {
    let current = std::thread::current().id();
    let coord = runtime().coord.0.lock().unwrap();
    runtime()
        .root_stack_owner
        .lock()
        .unwrap()
        .as_ref()
        .is_some_and(|owner| *owner != current && !coord.mutators.contains_key(owner))
}

/// Number of distinct runtime-rooted objects. Acceptance tests use this to
/// prove that panic/recover releases every root it took, instead of only
/// checking that the program printed the right text (willow-s9ej.7).
pub fn runtime_root_count() -> usize {
    runtime().runtime_roots.len()
}

pub(super) fn runtime_roots_snapshot() -> Vec<*mut u8> {
    let mut roots = runtime().runtime_roots.snapshot();
    let parked = runtime().parked_stack_roots.lock().unwrap();
    for slots in parked.values() {
        for &slot in slots {
            // SAFETY: the park contract retains these immutable stack slots;
            // active stack transitions and collection are serialized by STW.
            if slot == 0 {
                continue;
            }
            let value = unsafe { *(slot as *mut *mut u8) };
            if !value.is_null() {
                roots.push(value);
            }
        }
    }
    roots
}
