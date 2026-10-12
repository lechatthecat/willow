use super::*;

/// Stable mutable GC-reference slot owned by a [`GcRootArena`].
///
/// The collector receives the address of `value` and may rewrite it when a
/// young object moves. `Arc` ownership makes that address independent of arena
/// vector growth and keeps it alive after a logical root is released.
pub(super) struct GcRootCell(std::sync::atomic::AtomicPtr<u8>);

impl GcRootCell {
    /// Log before publishing the replacement. A failed CAS only retains an
    /// extra snapshot edge; retrying logs the actual value we next overwrite.
    /// This preserves atomic replacement when handles are shared by readers
    /// and writers, without exposing an unlogged deletion to the marker.
    fn replace(&self, value: *mut u8, mut before_store: impl FnMut(*mut u8)) {
        let mut old = self.0.load(Ordering::Acquire);
        loop {
            before_store(old);
            match self
                .0
                .compare_exchange_weak(old, value, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => return,
                Err(actual) => old = actual,
            }
        }
    }
}

// Atomic publication permits concurrent marking; the Arc allocation keeps
// slot addresses stable until owner finalization. Relocation writes stay STW.

/// One stable mutable reference slot. A scoped handle unregisters and frees
/// its cell on Drop. An arena handle nulls its cell on Drop; the arena retains
/// the physical slot until owner finalization, preserving trace-slot lifetimes.
pub(crate) struct GcRootHandle {
    cell: Arc<GcRootCell>,
    registered: bool,
}

impl GcRootHandle {
    /// A scoped native root, using the same stable cell as arena-owned roots.
    /// This operation does not safepoint. Reload after every GC point.
    pub(crate) fn new(value: *mut u8) -> Self {
        let cell = Arc::new(GcRootCell(std::sync::atomic::AtomicPtr::new(value)));
        willow_gc_add_runtime_root_slot(cell.0.as_ptr());
        Self {
            cell,
            registered: true,
        }
    }

    pub(crate) fn load(&self) -> *mut u8 {
        // SAFETY: see `GcRootCell`'s synchronization contract.
        self.cell.0.load(Ordering::Acquire)
    }

    pub(crate) fn store(&self, value: *mut u8) {
        let _no_preempt = crate::preempt::NoPreemptGuard::enter();
        self.cell.replace(value, |old| {
            willow_gc_write_barrier(
                std::ptr::null_mut(),
                old,
                value,
                GcStoreDestination::GlobalStatic as i64,
            );
        });
    }

    pub(crate) fn release(&self) {
        // SAFETY: see `GcRootCell`'s synchronization contract. Null is the GC
        // root protocol's explicit inactive-slot value.
        self.store(std::ptr::null_mut());
    }
}

impl Drop for GcRootHandle {
    fn drop(&mut self) {
        self.release();
        if self.registered {
            willow_gc_remove_runtime_root_slot(self.cell.0.as_ptr());
        }
    }
}

/// A scoped address loan. Unlike a relocatable handle, the leased object must
/// remain at its current address until Drop, including through park or unwind.
/// T records the kind of loan; the lease owns its root and cannot be cloned.
pub(crate) struct PinLease<T> {
    root: GcRootHandle,
    kind: std::marker::PhantomData<T>,
}

pub(crate) enum ForeignReference {}
pub(crate) enum InteriorPointer {}

impl PinLease<InteriorPointer> {
    /// Retain the containing allocation when a native byte slice borrows GC
    /// storage. Native addresses need no lease. Uses the existing region index;
    /// only an unindexed TLAB header prefix needs a bounded bitmap scan.
    pub(crate) fn for_address(address: *const u8) -> Option<Self> {
        let _no_preempt = crate::preempt::NoPreemptGuard::enter();
        let owner = {
            let state = runtime().heap.lock().unwrap();
            find_old_region_object(&state, address as usize, true)
                .or_else(|| {
                    find_tlab_chunk(&state, address as usize).and_then(|chunk| {
                        if chunk.owner_state.is_none() {
                            return object_in_retired_chunk(chunk, address as usize, true);
                        }
                        let relative =
                            (address as usize).checked_sub(chunk.base as usize + GC_HEADER_SIZE)?;
                        let offset = chunk.mark_bitmap.last_start_in(0, relative)?;
                        // The caller borrows an already published allocation;
                        // start bits only select headers at/before that address.
                        let object =
                            HeapObject::from_raw(unsafe { chunk.base.add(offset) }.cast())?;
                        (object.allocated()
                            && address as usize >= object.payload().as_ptr() as usize
                            && (address as usize) < object.as_ptr() as usize + object.size())
                        .then_some(object)
                    })
                })
                .map(|object| object.payload().as_ptr())
        };
        owner.map(Self::new)
    }
}

impl<T> PinLease<T> {
    pub(crate) fn new(value: *mut u8) -> Self {
        let _no_preempt = crate::preempt::NoPreemptGuard::enter();
        let root = GcRootHandle::new(value);
        runtime().pin_lease_slots.add(root.cell.0.as_ptr().cast());
        Self {
            root,
            kind: std::marker::PhantomData,
        }
    }

    #[cfg(test)]
    pub(crate) fn load(&self) -> *mut u8 {
        self.root.load()
    }
}

impl<T> Drop for PinLease<T> {
    fn drop(&mut self) {
        let _no_preempt = crate::preempt::NoPreemptGuard::enter();
        runtime()
            .pin_lease_slots
            .remove(self.root.cell.0.as_ptr().cast());
        // The handle's Drop performs the deletion barrier and unregisters.
    }
}

/// Stable arena for runtime-owned GC root slots.
///
/// Slots are retained until the arena itself is dropped, so a collector that
/// obtained an address from a trace callback never observes freed Rust heap
/// storage. Logical release nulls the slot immediately and task metadata may be
/// reclaimed; physical slot reclamation is synchronized with owner finalization.
pub(crate) struct GcRootArena {
    cells: Mutex<Vec<Arc<GcRootCell>>>,
}

impl Default for GcRootArena {
    fn default() -> Self {
        let cells = Mutex::new(Vec::new());
        // Some platforms lazily allocate native mutex storage on first use,
        // including try_lock(). Pay that cost before publishing the arena so
        // even its first, empty snapshot remains allocation-free.
        drop(cells.lock().unwrap_or_else(|error| error.into_inner()));
        Self { cells }
    }
}

impl GcRootArena {
    pub(crate) fn insert(&self, owner: *mut u8, value: *mut u8) -> GcRootHandle {
        willow_gc_write_barrier(
            owner,
            std::ptr::null_mut(),
            value,
            GcStoreDestination::ContainerInternal as i64,
        );
        let cell = Arc::new(GcRootCell(std::sync::atomic::AtomicPtr::new(value)));
        self.cells
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::clone(&cell));
        GcRootHandle {
            cell,
            registered: false,
        }
    }

    pub(crate) fn trace_slots(&self, slots: &mut Vec<*mut *mut u8>) {
        let cells = self
            .cells
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        slots.extend(cells.iter().map(|cell| cell.0.as_ptr()));
    }

    /// Copy a bounded prefix of stable cells without waiting on an arena writer.
    /// `bound` fixes the initial extent; insertions publish new edges separately.
    pub(crate) fn snapshot_slice(
        &self,
        cursor: usize,
        limit: usize,
        bound: usize,
        children: &mut Vec<*mut u8>,
    ) -> Option<(usize, usize)> {
        let cells = match self.cells.try_lock() {
            Ok(cells) => cells,
            Err(std::sync::TryLockError::WouldBlock) => return None,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let bound = bound.min(cells.len());
        let end = cursor.saturating_add(limit).min(bound);
        children.extend(
            cells[cursor.min(bound)..end]
                .iter()
                .map(|cell| cell.0.load(Ordering::Acquire)),
        );
        Some((end, bound))
    }

    #[cfg(test)]
    pub(crate) fn slot_count(&self) -> usize {
        self.cells
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len()
    }
}

/// Publish an edge stored outside GC-managed storage. Does not safepoint.
pub(crate) fn publish_native_reference(value: *mut u8) {
    if let Some(cycle) = runtime().heap.lock().unwrap().concurrent_cycle.as_ref() {
        cycle.enqueue(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_cell_logs_before_publication_and_retries_changed_values() {
        // Synthetic addresses: replacement never dereferences the referents.
        let first = 8usize as *mut u8;
        let competing = 16usize as *mut u8;
        let replacement = 24usize as *mut u8;
        for value in [std::ptr::null_mut(), replacement] {
            let cell = GcRootCell(std::sync::atomic::AtomicPtr::new(first));
            let mut logged = Vec::new();
            cell.replace(value, |old| {
                assert_eq!(cell.0.load(Ordering::Acquire), old);
                logged.push(old);
                if logged.len() == 1 {
                    // Force a competing writer between the pre-write and CAS.
                    cell.0.store(competing, Ordering::Release);
                }
            });
            assert_eq!(logged[0], first);
            assert!(logged[1..].iter().all(|&old| old == competing));
            assert_eq!(cell.0.load(Ordering::Acquire), value);
        }
    }
}
