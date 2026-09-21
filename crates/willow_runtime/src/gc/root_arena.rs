use super::*;

/// Stable mutable GC-reference slot owned by a [`GcRootArena`].
///
/// The collector receives the address of `value` and may rewrite it when a
/// young object moves. `Arc` ownership makes that address independent of arena
/// vector growth and keeps it alive after a logical root is released.
pub(super) struct GcRootCell(std::sync::atomic::AtomicPtr<u8>);

// Atomic publication permits concurrent marking; the Arc allocation keeps
// slot addresses stable until owner finalization. Relocation writes stay STW.

/// Handle for one slot in a [`GcRootArena`]. Dropping the handle does not free
/// the slot because the arena retains its own Arc until the owning runtime
/// object is finalized.
pub(crate) struct GcRootHandle {
    cell: Arc<GcRootCell>,
}

impl GcRootHandle {
    pub(crate) fn load(&self) -> *mut u8 {
        // SAFETY: see `GcRootCell`'s synchronization contract.
        self.cell.0.load(Ordering::Acquire)
    }

    pub(crate) fn release(&self) {
        // SAFETY: see `GcRootCell`'s synchronization contract. Null is the GC
        // root protocol's explicit inactive-slot value.
        satb_delete(self.cell.0.load(Ordering::Acquire));
        self.cell.0.store(std::ptr::null_mut(), Ordering::Release);
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
        GcRootHandle { cell }
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
