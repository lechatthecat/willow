//! Blocking compatibility cells (blocking cells, mutex and read/write lock lowering).
//!
//! `BlockingCell<T>` and `BlockingRwCell<T>` preserve the old single-operation
//! API after public `Mutex<T>`/`RwLock<T>` became scheduler-aware lexical
//! locks. Each holds the inner value as a single 64-bit word (scalars by value,
//! GC values as their pointer; the compiler coerces). A real `std::sync` lock
//! guards the word, so these explicit compatibility types may block a worker
//! and should not be confused with scheduler-aware locks.
//!
//! GC: a cell is an ordinary GC-managed object, allocated
//! through `willow_alloc_with_layout` like a channel or a scheduler-aware lock
//! handle. The collector traces the protected word of a reference cell through
//! the registered trace hook, so a live cell keeps its value alive and an
//! unreachable cell is reclaimed by sweep together with anything only it
//! referenced. The previous design leaked every cell for the program's lifetime
//! and recorded reference cells in a never-pruned global registry that the
//! collector scanned as roots; both are gone, so native memory and root-scan
//! cost no longer grow with the number of cells ever created.
//!
//! The std lock is held only across a single load or store inside one runtime
//! call. No safepoint exists inside that region, and stop-the-world collection
//! parks mutators only at safepoints, so the trace hook can take the same lock
//! without deadlocking against a stopped mutator.

use std::os::raw::c_void;
use std::sync::Mutex as StdMutex;
use std::sync::PoisonError;
use std::sync::RwLock as StdRwLock;

use crate::gc::{GcObjectKind, GcStoreDestination, NativeGcRegistration, NativeGcType};

struct WillowBlockingCell {
    value: StdMutex<i64>,
    is_ref: bool,
}

struct WillowBlockingRwCell {
    value: StdRwLock<i64>,
    is_ref: bool,
}

/// GC type ids for the two cell payloads. They share the lock-handle family
/// with the scheduler-aware locks (`0x10C4_0001` / `0x10C4_0002`).
use willow_abi::runtime_type_ids::BLOCKING_CELL_TYPE_ID;
use willow_abi::runtime_type_ids::BLOCKING_RW_CELL_TYPE_ID;

/// Stop-the-world trace: the protected word of a reference cell is the one
/// mutable GC slot the payload owns. The slot address stays valid after the
/// guard drops because it points into the GC payload, and a moving minor
/// collection may rewrite it in place.
///
/// The hooks read through a poisoned lock rather than skipping it: a poison
/// flag only records that some thread panicked while holding the guard, and
/// dropping the edge would leave a dangling word in a live cell.
///
/// # Safety
/// `payload` must be a [`WillowBlockingCell`] allocated by
/// [`willow_blocking_cell_new`].
unsafe fn trace_blocking_cell(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
    // size.
    let cell = unsafe { &*(payload as *const WillowBlockingCell) };
    if !cell.is_ref {
        return;
    }
    let mut word = cell.value.lock().unwrap_or_else(PoisonError::into_inner);
    if *word != 0 {
        slots.push((&mut *word as *mut i64).cast::<*mut u8>());
    }
}

/// Concurrent snapshot for the marker: copy the current value under the lock.
///
/// # Safety
/// `payload` must be a [`WillowBlockingCell`] allocated by
/// [`willow_blocking_cell_new`].
unsafe fn snapshot_blocking_cell_slice(
    payload: *mut u8,
    _cursor: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> crate::gc::TraceSliceProgress {
    use crate::gc::TraceSliceProgress;
    assert!(
        limit > 0,
        "native trace slice requires positive work credit"
    );
    // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
    // size.
    let cell = unsafe { &*payload.cast::<WillowBlockingCell>() };
    if cell.is_ref {
        let word = match cell.value.try_lock() {
            Ok(word) => word,
            Err(std::sync::TryLockError::WouldBlock) => return TraceSliceProgress::Retry,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        };
        children.push(*word as *mut u8);
    }
    TraceSliceProgress::Done
}

unsafe fn snapshot_blocking_cell(payload: *mut u8, children: &mut Vec<*mut u8>) {
    // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
    // size.
    let cell = unsafe { &*(payload as *const WillowBlockingCell) };
    if cell.is_ref {
        let word = cell.value.lock().unwrap_or_else(PoisonError::into_inner);
        children.push(*word as *mut u8);
    }
}

/// Finalizer: run the `std::sync::Mutex` destructor before sweep releases the
/// GC block. Some platforms back the std lock with a lazily boxed native
/// primitive, which only the destructor frees.
///
/// # Safety
/// `payload` must point to an initialized [`WillowBlockingCell`].
unsafe fn drop_blocking_cell(payload: *mut u8) {
    // SAFETY: The collector invokes the matching destructor on the word-aligned initialized cell
    // exactly once.
    unsafe { std::ptr::drop_in_place(payload as *mut WillowBlockingCell) };
}

/// # Safety
/// `payload` must be a [`WillowBlockingRwCell`] allocated by
/// [`willow_blocking_rw_cell_new`].
unsafe fn trace_blocking_rw_cell(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
    // size.
    let cell = unsafe { &*(payload as *const WillowBlockingRwCell) };
    if !cell.is_ref {
        return;
    }
    let mut word = cell.value.write().unwrap_or_else(PoisonError::into_inner);
    if *word != 0 {
        slots.push((&mut *word as *mut i64).cast::<*mut u8>());
    }
}

/// # Safety
/// `payload` must be a [`WillowBlockingRwCell`] allocated by
/// [`willow_blocking_rw_cell_new`].
unsafe fn snapshot_blocking_rw_cell_slice(
    payload: *mut u8,
    _cursor: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> crate::gc::TraceSliceProgress {
    use crate::gc::TraceSliceProgress;
    assert!(
        limit > 0,
        "native trace slice requires positive work credit"
    );
    // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
    // size.
    let cell = unsafe { &*payload.cast::<WillowBlockingRwCell>() };
    if cell.is_ref {
        let word = match cell.value.try_read() {
            Ok(word) => word,
            Err(std::sync::TryLockError::WouldBlock) => return TraceSliceProgress::Retry,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        };
        children.push(*word as *mut u8);
    }
    TraceSliceProgress::Done
}

unsafe fn snapshot_blocking_rw_cell(payload: *mut u8, children: &mut Vec<*mut u8>) {
    // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
    // size.
    let cell = unsafe { &*(payload as *const WillowBlockingRwCell) };
    if cell.is_ref {
        let word = cell.value.read().unwrap_or_else(PoisonError::into_inner);
        children.push(*word as *mut u8);
    }
}

/// # Safety
/// `payload` must point to an initialized [`WillowBlockingRwCell`].
unsafe fn drop_blocking_rw_cell(payload: *mut u8) {
    // SAFETY: The collector invokes the matching destructor on the word-aligned initialized cell
    // exactly once.
    unsafe { std::ptr::drop_in_place(payload as *mut WillowBlockingRwCell) };
}

#[cfg(test)]
const BLOCKING_CELL_DROP_COUNT: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::BlockingCellDropCount);
#[cfg(test)]
const BLOCKING_RW_CELL_DROP_COUNT: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::BlockingRwCellDropCount);
#[cfg(test)]
const BLOCKING_CELL_REGISTRATION_COUNT: crate::observation::Counter =
    crate::observation::Counter(crate::observation::Event::BlockingCellRegistrationCount);

const BLOCKING_CELL_REGISTRATION: NativeGcRegistration = NativeGcRegistration::BlockingCell;
const BLOCKING_CELL_GC_TYPES: &[NativeGcType] = &[
    // SAFETY: Each cell constructor pairs its ID with the corresponding mutex or
    // read/write-lock representation. The stopped hook exposes its reference slot,
    // concurrent snapshots synchronize with writers, and the finalizer drops that cell.
    unsafe {
        NativeGcType::new(
            BLOCKING_CELL_TYPE_ID,
            Some(trace_blocking_cell),
            Some(drop_blocking_cell),
        )
        .with_concurrent_trace(snapshot_blocking_cell)
        .with_concurrent_slice(snapshot_blocking_cell_slice)
    },
    // SAFETY: Each cell constructor pairs its ID with the corresponding mutex or
    // read/write-lock representation. The stopped hook exposes its reference slot,
    // concurrent snapshots synchronize with writers, and the finalizer drops that cell.
    unsafe {
        NativeGcType::new(
            BLOCKING_RW_CELL_TYPE_ID,
            Some(trace_blocking_rw_cell),
            Some(drop_blocking_rw_cell),
        )
        .with_concurrent_trace(snapshot_blocking_rw_cell)
        .with_concurrent_slice(snapshot_blocking_rw_cell_slice)
    },
];

/// Install both cells' GC hooks once per registry generation; the common
/// allocation path pays two atomic loads.
fn ensure_blocking_cells_registered() {
    // SAFETY: This is the complete descriptor set for the blocking cell registration group
    // and its constructors.
    if unsafe { BLOCKING_CELL_REGISTRATION.ensure(BLOCKING_CELL_GC_TYPES) } {
        crate::observation::record(crate::observation::Event::BlockingCellRegistrationCount, 1);
    }
}

/// Allocate one cell payload with a possibly moving initial reference held in
/// a stable handle. Reload into the caller's word after the allocation.
fn alloc_cell_payload(type_id: u32, payload_size: usize, value: &mut i64, is_ref: bool) -> *mut u8 {
    ensure_blocking_cells_registered();
    let root = is_ref.then(|| crate::gc::GcRootHandle::new(*value as *mut u8));
    // SAFETY: The constructor selects the matching payload layout and initializes its fields
    // before exposing it or reaching a GC point.
    let payload = unsafe {
        crate::gc::willow_alloc_with_layout(
            GcObjectKind::LockHandle,
            type_id,
            payload_size as i64,
            0,
        )
    };
    if let Some(root) = root {
        *value = root.load() as i64;
    }
    payload
}

/// Allocate and initialize a cell before publishing its initial GC edge.
/// # Safety
/// The type ID must be registered for exactly T, including its trace and drop hooks.
/// For a reference cell the initial word must be null or a current managed payload.
unsafe fn new_blocking_cell<T>(
    mut value: i64,
    is_ref: i64,
    type_id: u32,
    destination: GcStoreDestination,
    init: impl FnOnce(i64, bool) -> T,
) -> *mut c_void {
    const { assert!(align_of::<T>() <= align_of::<crate::gc::GcHeader>()) };
    let is_ref = is_ref != 0;
    let payload = alloc_cell_payload(type_id, std::mem::size_of::<T>(), &mut value, is_ref);
    if payload.is_null() {
        return std::ptr::null_mut();
    }
    // Placement-init into GC memory; the registered hook drops T during sweep.
    // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
    // size.
    unsafe {
        payload.cast::<T>().write(init(value, is_ref));
    }
    if is_ref {
        // SAFETY: The reference cell supplies its previous word and incoming value. This
        // destination asserts no header bits.
        unsafe {
            crate::gc::willow_gc_write_barrier(
                payload,
                std::ptr::null_mut(),
                value as *mut u8,
                destination as i64,
            )
        };
    }
    payload.cast()
}

/// # Safety
/// When `is_ref` is nonzero, `value` must be null or the current address of a live
/// managed payload so the allocator can root and trace it.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_blocking_cell_new(value: i64, is_ref: i64) -> *mut c_void {
    // SAFETY: The selected type ID and constructor initialize the exact blocking cell
    // representation consumed by its hooks.
    unsafe {
        new_blocking_cell(
            value,
            is_ref,
            BLOCKING_CELL_TYPE_ID,
            GcStoreDestination::BlockingCell,
            |value, is_ref| WillowBlockingCell {
                value: StdMutex::new(value),
                is_ref,
            },
        )
    }
}

/// # Safety
/// The caller must keep a non-null cell returned by `willow_blocking_cell_new`
/// live in this runtime throughout the call, including any wait for its lock.
///
/// ```compile_fail,E0133
/// fn read(cell: *mut std::ffi::c_void) -> i64 {
///     willow_runtime::lock::willow_blocking_cell_get(cell)
/// }
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_blocking_cell_get(raw: *mut c_void) -> i64 {
    // SAFETY: The caller keeps the matching constructor’s initialized WillowBlockingCell live.
    let m = unsafe { &*raw.cast::<WillowBlockingCell>() };
    *m.value.lock().expect("mutex poisoned")
}

/// # Safety
/// The caller must keep a non-null cell returned by `willow_blocking_cell_new`
/// live in this runtime throughout the call, including any wait for its lock.
/// If constructed as a reference cell, the new value must be null or a current live GC payload.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_blocking_cell_set(raw: *mut c_void, value: i64) {
    // SAFETY: The caller keeps the matching constructor’s initialized WillowBlockingCell live.
    let m = unsafe { &*raw.cast::<WillowBlockingCell>() };
    let mut slot = m.value.lock().expect("mutex poisoned");
    if m.is_ref {
        // Keep the old-value load and the store in the same critical section.
        // The barrier only publishes work; it never traces or reaches a safepoint.
        // SAFETY: The reference cell supplies its previous word and incoming value. This
        // destination asserts no header bits.
        unsafe {
            crate::gc::willow_gc_write_barrier(
                raw as *mut u8,
                *slot as *mut u8,
                value as *mut u8,
                GcStoreDestination::BlockingCell as i64,
            )
        };
    }
    *slot = value;
}

/// # Safety
/// When `is_ref` is nonzero, `value` must be null or the current address of a live
/// managed payload so the allocator can root and trace it.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_blocking_rw_cell_new(value: i64, is_ref: i64) -> *mut c_void {
    // SAFETY: The selected type ID and constructor initialize the exact blocking cell
    // representation consumed by its hooks.
    unsafe {
        new_blocking_cell(
            value,
            is_ref,
            BLOCKING_RW_CELL_TYPE_ID,
            GcStoreDestination::BlockingRwCell,
            |value, is_ref| WillowBlockingRwCell {
                value: StdRwLock::new(value),
                is_ref,
            },
        )
    }
}

/// # Safety
/// The caller must keep a non-null cell returned by `willow_blocking_rw_cell_new`
/// live in this runtime throughout the call, including any wait for its lock.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_blocking_rw_cell_read(raw: *mut c_void) -> i64 {
    // SAFETY: The caller keeps the matching constructor’s initialized WillowBlockingRwCell live.
    let r = unsafe { &*raw.cast::<WillowBlockingRwCell>() };
    *r.value.read().expect("rwlock poisoned")
}

/// # Safety
/// The caller must keep a non-null cell returned by `willow_blocking_rw_cell_new`
/// live in this runtime throughout the call, including any wait for its lock.
/// If constructed as a reference cell, the new value must be null or a current live GC payload.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_blocking_rw_cell_write(raw: *mut c_void, value: i64) {
    // SAFETY: The caller keeps the matching constructor’s initialized WillowBlockingRwCell live.
    let r = unsafe { &*raw.cast::<WillowBlockingRwCell>() };
    let mut slot = r.value.write().expect("rwlock poisoned");
    if r.is_ref {
        // SAFETY: The reference cell supplies its previous word and incoming value. This
        // destination asserts no header bits.
        unsafe {
            crate::gc::willow_gc_write_barrier(
                raw as *mut u8,
                *slot as *mut u8,
                value as *mut u8,
                GcStoreDestination::BlockingRwCell as i64,
            )
        };
    }
    *slot = value;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{
        isolated_runtime, reset_internal_for_test, total_frees_for_test, willow_alloc_typed,
        willow_gc_allocated_bytes, willow_gc_collect, willow_pop_roots, willow_push_root,
    };
    use std::sync::atomic::Ordering;

    #[test]
    fn bounded_cell_snapshots_retry_busy_locks_without_waiting() {
        use crate::gc::{GcRootHandle, TraceSliceProgress};
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let first = old_object(17);
        // SAFETY: The freshly allocated scalar object is a current reference on entry.
        let cell = GcRootHandle::new(unsafe { willow_blocking_cell_new(first as i64, 1) }.cast());
        let second = old_object(19);
        // SAFETY: The second constructor receives its own current referent.
        let rw = GcRootHandle::new(unsafe { willow_blocking_rw_cell_new(second as i64, 1) }.cast());
        let cell_ptr = cell.load();
        let rw_ptr = rw.load();
        assert!(!cell_ptr.is_null() && !rw_ptr.is_null());
        let mut children = Vec::new();

        // SAFETY: Both roots retain the corresponding initialized lock payloads
        // and their old-space children. Only these snapshot hooks run while we
        // hold the guards, and neither hook reaches a GC point.
        unsafe {
            let held = (*cell_ptr.cast::<WillowBlockingCell>())
                .value
                .lock()
                .unwrap();
            assert!(matches!(
                snapshot_blocking_cell_slice(cell_ptr, 0, 1, &mut children),
                TraceSliceProgress::Retry
            ));
            assert!(children.is_empty());
            drop(held);
            let held = (*rw_ptr.cast::<WillowBlockingRwCell>())
                .value
                .write()
                .unwrap();
            assert!(matches!(
                snapshot_blocking_rw_cell_slice(rw_ptr, 0, 1, &mut children),
                TraceSliceProgress::Retry
            ));
            assert!(children.is_empty());
            drop(held);
            assert!(matches!(
                snapshot_blocking_cell_slice(cell_ptr, 0, 1, &mut children),
                TraceSliceProgress::Done
            ));
            assert_eq!(children, [first]);
            children.clear();
            assert!(matches!(
                snapshot_blocking_rw_cell_slice(rw_ptr, 0, 1, &mut children),
                TraceSliceProgress::Done
            ));
            assert_eq!(children, [second]);
        }
    }

    /// A GC object holding one scalar word, allocated in the old generation.
    fn old_object(word: i64) -> *mut u8 {
        let object = willow_alloc_typed(8, 0);
        assert!(!object.is_null());
        // SAFETY: The allocator provides eight bytes at GC word alignment. This object is fresh.
        unsafe { object.cast::<i64>().write(word) };
        object
    }

    // ── Perspective 1-2: single-operation accessors keep their semantics ──

    #[test]
    fn mutex_get_set() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        // SAFETY: The scalar mutex cell is used immediately by its creating thread.
        unsafe {
            let m = willow_blocking_cell_new(7, 0);
            assert_eq!(willow_blocking_cell_get(m), 7);
            willow_blocking_cell_set(m, 42);
            assert_eq!(willow_blocking_cell_get(m), 42);
        }
    }

    #[test]
    fn rwlock_read_write() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        // SAFETY: The scalar read/write cell is used immediately by its creating thread.
        unsafe {
            let r = willow_blocking_rw_cell_new(1, 0);
            assert_eq!(willow_blocking_rw_cell_read(r), 1);
            willow_blocking_rw_cell_write(r, 100);
            assert_eq!(willow_blocking_rw_cell_read(r), 100);
        }
    }

    // ── Perspective 3-4: unreachable cells are reclaimed; nothing grows ──

    #[test]
    fn unreachable_cells_are_reclaimed_and_the_heap_returns_to_baseline() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let before = willow_gc_allocated_bytes();
        for i in 0..1000 {
            let child = old_object(i);
            let _keep_child = crate::gc::GcRootHandle::new(child.cast());
            // SAFETY: The initial reference comes from a live managed allocation and is
            // current on entry. The zero reference flag makes the initial value a scalar word.
            unsafe {
                assert!(!willow_blocking_cell_new(child as i64, 1).is_null());
                assert!(!willow_blocking_rw_cell_new(child as i64, 1).is_null());
                assert!(!willow_blocking_cell_new(i, 0).is_null());
            }
        }
        assert!(willow_gc_allocated_bytes() > before);
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            before,
            "dead cells and the values only they held must be swept"
        );
    }

    #[test]
    fn sweep_runs_the_cell_destructors() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let cells_before = BLOCKING_CELL_DROP_COUNT.load(Ordering::SeqCst);
        let rw_before = BLOCKING_RW_CELL_DROP_COUNT.load(Ordering::SeqCst);
        const CELLS: usize = 256;
        for i in 0..CELLS {
            // SAFETY: The zero reference flag makes the initial value a scalar word.
            unsafe {
                willow_blocking_cell_new(i as i64, 0);
                willow_blocking_rw_cell_new(i as i64, 0);
            }
        }
        willow_gc_collect();
        assert!(
            BLOCKING_CELL_DROP_COUNT.load(Ordering::SeqCst) - cells_before >= CELLS,
            "every unreachable BlockingCell payload is dropped"
        );
        assert!(
            BLOCKING_RW_CELL_DROP_COUNT.load(Ordering::SeqCst) - rw_before >= CELLS,
            "every unreachable BlockingRwCell payload is dropped"
        );
    }

    // ── Perspective 5-6: a rooted cell keeps its value alive ──

    #[test]
    fn rooted_cell_keeps_its_reference_value_alive() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let kept = old_object(0x5EED);
        // SAFETY: The initial reference comes from a live managed allocation and is current
        // on entry.
        let mut cell = (unsafe { willow_blocking_cell_new(kept as i64, 1) }) as *mut u8;
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut cell) };
        // A dead cell with a value only it references: exactly two frees.
        // SAFETY: The initial reference comes from a live managed allocation and is current
        // on entry.
        unsafe { willow_blocking_cell_new(old_object(1) as i64, 1) };
        let frees = total_frees_for_test();
        willow_gc_collect();
        assert_eq!(
            total_frees_for_test() - frees,
            2,
            "only the dead pair is swept"
        );
        assert_eq!(
            // SAFETY: The matching mutex cell constructor initialized this live payload.
            unsafe { willow_blocking_cell_get(cell as *mut c_void) },
            kept as i64
        );
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        assert_eq!(unsafe { *(kept as *const i64) }, 0x5EED);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    #[test]
    fn rooted_rw_cell_keeps_its_reference_value_alive() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let kept = old_object(0x2EAD);
        // SAFETY: The initial reference comes from a live managed allocation and is current
        // on entry.
        let mut cell = (unsafe { willow_blocking_rw_cell_new(kept as i64, 1) }) as *mut u8;
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut cell) };
        // SAFETY: The initial reference comes from a live managed allocation and is current
        // on entry.
        unsafe { willow_blocking_rw_cell_new(old_object(1) as i64, 1) };
        let frees = total_frees_for_test();
        willow_gc_collect();
        assert_eq!(total_frees_for_test() - frees, 2);
        assert_eq!(
            // SAFETY: The matching read/write cell constructor initialized this live payload.
            unsafe { willow_blocking_rw_cell_read(cell as *mut c_void) },
            kept as i64
        );
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        assert_eq!(unsafe { *(kept as *const i64) }, 0x2EAD);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    // ── Perspective 7: a value stored later is traced too ──

    #[test]
    fn value_stored_with_set_survives_collection() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        // SAFETY: Zero initializes an empty reference slot.
        let mut cell = (unsafe { willow_blocking_cell_new(0, 1) }) as *mut u8;
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut cell) };
        let stored = old_object(11);
        // SAFETY: The fixture retains the matching mutex cell from its constructor and stores
        // a value of its declared type.
        unsafe { willow_blocking_cell_set(cell as *mut c_void, stored as i64) };
        // SAFETY: Zero initializes an empty reference slot.
        let mut rw = (unsafe { willow_blocking_rw_cell_new(0, 1) }) as *mut u8;
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut rw) };
        let written = old_object(22);
        // SAFETY: The fixture retains the matching read/write cell from its constructor and
        // stores a value of its declared type.
        unsafe { willow_blocking_rw_cell_write(rw as *mut c_void, written as i64) };
        let frees = total_frees_for_test();
        willow_gc_collect();
        assert_eq!(total_frees_for_test(), frees, "nothing reachable is freed");
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        assert_eq!(unsafe { *(stored as *const i64) }, 11);
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        assert_eq!(unsafe { *(written as *const i64) }, 22);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
    }

    // ── Perspective 8-9: the trace hooks expose exactly the reference word ──

    #[test]
    fn scalar_cells_expose_no_gc_slots() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        // SAFETY: The zero reference flag makes the initial value a scalar word.
        let cell = (unsafe { willow_blocking_cell_new(0x1234, 0) }) as *mut u8;
        let _keep_cell = crate::gc::GcRootHandle::new(cell.cast());
        // SAFETY: The zero reference flag makes the initial value a scalar word.
        let rw = (unsafe { willow_blocking_rw_cell_new(0x5678, 0) }) as *mut u8;
        let mut slots = Vec::new();
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        unsafe {
            trace_blocking_cell(cell, &mut slots);
            trace_blocking_rw_cell(rw, &mut slots);
        }
        assert!(slots.is_empty(), "a scalar word is never a GC edge");
        let mut children = Vec::new();
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        unsafe {
            snapshot_blocking_cell(cell, &mut children);
            snapshot_blocking_rw_cell(rw, &mut children);
        }
        assert!(children.is_empty());
    }

    #[test]
    fn reference_cells_expose_their_word_as_a_rewritable_slot() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let first = old_object(1);
        let _keep_first = crate::gc::GcRootHandle::new(first.cast());
        let second = old_object(2);
        let _keep_second = crate::gc::GcRootHandle::new(second.cast());
        // SAFETY: The initial reference comes from a live managed allocation and is current
        // on entry.
        let cell = (unsafe { willow_blocking_cell_new(first as i64, 1) }) as *mut u8;
        let _keep_cell = crate::gc::GcRootHandle::new(cell.cast());
        // SAFETY: The initial reference comes from a live managed allocation and is current
        // on entry.
        let rw = (unsafe { willow_blocking_rw_cell_new(second as i64, 1) }) as *mut u8;
        let _keep_rw = crate::gc::GcRootHandle::new(rw.cast());
        let mut slots = Vec::new();
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        unsafe {
            trace_blocking_cell(cell, &mut slots);
            trace_blocking_rw_cell(rw, &mut slots);
        }
        assert_eq!(slots.len(), 2);
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        assert_eq!(unsafe { *slots[0] }, first);
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        assert_eq!(unsafe { *slots[1] }, second);
        // The collector rewrites a moved child through the slot it was given.
        let moved = old_object(3);
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        unsafe { *slots[0] = moved };
        assert_eq!(
            // SAFETY: The matching mutex cell constructor initialized this live payload.
            unsafe { willow_blocking_cell_get(cell as *mut c_void) },
            moved as i64
        );
        let mut children = Vec::new();
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        unsafe {
            snapshot_blocking_cell(cell, &mut children);
            snapshot_blocking_rw_cell(rw, &mut children);
        }
        assert_eq!(children, vec![moved, second]);
    }

    // ── Perspective 10: a null reference is not reported ──

    #[test]
    fn null_reference_word_is_not_a_slot() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        // SAFETY: Zero initializes an empty reference slot.
        let cell = (unsafe { willow_blocking_cell_new(0, 1) }) as *mut u8;
        let _keep_cell = crate::gc::GcRootHandle::new(cell.cast());
        // SAFETY: Zero initializes an empty reference slot.
        let rw = (unsafe { willow_blocking_rw_cell_new(0, 1) }) as *mut u8;
        let mut slots = Vec::new();
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        unsafe {
            trace_blocking_cell(cell, &mut slots);
            trace_blocking_rw_cell(rw, &mut slots);
        }
        assert!(slots.is_empty());
    }

    // ── Perspective 11: hooks register once per registry generation ──

    #[test]
    fn gc_hooks_register_once_per_registry_generation() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        ensure_blocking_cells_registered();
        let generation = crate::gc::registry_generation();
        let registrations = BLOCKING_CELL_REGISTRATION_COUNT.load(Ordering::SeqCst);
        for i in 0..10_000 {
            // SAFETY: The zero reference flag makes the initial value a scalar word.
            unsafe { willow_blocking_cell_new(i, 0) };
        }
        assert_eq!(
            BLOCKING_CELL_REGISTRATION_COUNT.load(Ordering::SeqCst),
            registrations,
            "same-generation cell creation stays on the atomic fast path"
        );
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        assert_ne!(crate::gc::registry_generation(), generation);
        // SAFETY: The zero reference flag makes the initial value a scalar word.
        unsafe { willow_blocking_rw_cell_new(1, 0) };
        assert_eq!(
            BLOCKING_CELL_REGISTRATION_COUNT.load(Ordering::SeqCst),
            registrations + 1,
            "the first cell after a GC reset reinstalls the hooks once"
        );
    }

    // ── Perspective 12-13: stores into an old cell take the write barrier ──

    #[test]
    fn set_of_a_young_value_remembers_the_cell() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        crate::gc::set_gc_stress_for_test(Some(""));
        let tls = crate::gc::tlab_state_for_test();
        // SAFETY: Zero initializes an empty reference slot.
        let mut cell = (unsafe { willow_blocking_cell_new(0, 1) }) as *mut u8;
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut cell) };
        // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size
        // and tracing metadata describe the object initialized here.
        let young = unsafe { crate::gc::willow_gc_alloc_slow(tls.as_ptr(), 42, 0, 8, 0) };
        assert!(!young.is_null());
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        unsafe { *(young as *mut i64) = 77 };
        let remembered = crate::gc::willow_gc_remembered_set_size();
        // SAFETY: The fixture retains the matching mutex cell from its constructor and stores
        // a value of its declared type.
        unsafe { willow_blocking_cell_set(cell as *mut c_void, young as i64) };
        assert_eq!(
            crate::gc::willow_gc_remembered_set_size(),
            remembered + 1,
            "an old cell holding a young value joins the remembered set"
        );
        crate::gc::minor_collect_internal();
        // SAFETY: The matching mutex cell constructor initialized this live payload.
        let relocated = (unsafe { willow_blocking_cell_get(cell as *mut c_void) }) as *const i64;
        assert_ne!(
            // SAFETY: The cell constructor places this lock in word-aligned GC storage of the
            // matching size.
            relocated,
            young as *const i64,
            "the young value was evacuated"
        );
        assert_eq!(
            // SAFETY: The cell constructor places this lock in word-aligned GC storage of the
            // matching size.
            unsafe { *relocated },
            77,
            "the cell follows the moved value"
        );
        crate::gc::set_gc_stress_for_test(None);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        // `tls` lives on this stack and is registered with the heap; drop the
        // registration before it goes out of scope, or the next heap reset
        // writes into a dead thread's stack (an access violation on Windows).
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
    }

    #[test]
    fn write_of_a_young_value_remembers_the_rw_cell() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        crate::gc::set_gc_stress_for_test(Some(""));
        let tls = crate::gc::tlab_state_for_test();
        // SAFETY: Zero initializes an empty reference slot.
        let mut cell = (unsafe { willow_blocking_rw_cell_new(0, 1) }) as *mut u8;
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut cell) };
        // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size
        // and tracing metadata describe the object initialized here.
        let young = unsafe { crate::gc::willow_gc_alloc_slow(tls.as_ptr(), 42, 0, 8, 0) };
        assert!(!young.is_null());
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        unsafe { *(young as *mut i64) = 88 };
        let remembered = crate::gc::willow_gc_remembered_set_size();
        // SAFETY: The fixture retains the matching read/write cell from its constructor and
        // stores a value of its declared type.
        unsafe { willow_blocking_rw_cell_write(cell as *mut c_void, young as i64) };
        assert_eq!(crate::gc::willow_gc_remembered_set_size(), remembered + 1);
        crate::gc::minor_collect_internal();
        // SAFETY: The matching read/write cell constructor initialized this live payload.
        let relocated =
            (unsafe { willow_blocking_rw_cell_read(cell as *mut c_void) }) as *const i64;
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        assert_ne!(relocated, young as *const i64);
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        assert_eq!(unsafe { *relocated }, 88);
        crate::gc::set_gc_stress_for_test(None);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        // See `set_of_a_young_value_remembers_the_cell`: drop the stack `tls`
        // registration before it goes out of scope.
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
    }

    // ── Perspective 14: a scalar store never touches the barrier ──

    #[test]
    fn scalar_stores_skip_the_write_barrier() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        // SAFETY: The zero reference flag makes the initial value a scalar word.
        let cell = unsafe { willow_blocking_cell_new(0, 0) };
        let _keep_cell = crate::gc::GcRootHandle::new(cell.cast());
        // SAFETY: The zero reference flag makes the initial value a scalar word.
        let rw = unsafe { willow_blocking_rw_cell_new(0, 0) };
        let calls = crate::gc::telemetry_heap_snapshot().0.barrier_calls;
        // SAFETY: The fixture retains the matching mutex cell from its constructor and stores
        // a value of its declared type. The fixture retains the matching read/write cell from
        // its constructor and stores a value of its declared type.
        unsafe {
            willow_blocking_cell_set(cell, 5);
            willow_blocking_rw_cell_write(rw, 6);
        }
        assert_eq!(crate::gc::telemetry_heap_snapshot().0.barrier_calls, calls);
        // SAFETY: The matching mutex cell constructor initialized this live payload. The
        // matching read/write cell constructor initialized this live payload.
        unsafe {
            assert_eq!(willow_blocking_cell_get(cell), 5);
            assert_eq!(willow_blocking_rw_cell_read(rw), 6);
        }
    }

    // ── Perspective 15: the initial value survives a collection inside `new` ──

    /// The constructor's own allocation can collect. The initial word is a
    /// young object the caller may hold nowhere else, so the runtime roots it
    /// across the allocation; its stable root can be rewritten by copying,
    /// and the word written into the fresh payload must be the new address.
    ///
    /// Deterministic ordering follows the map-copy recipe: this
    /// thread registers as a mutator so the collector cannot proceed until it
    /// parks; `alloc` stress makes the payload allocation reach
    /// `collect_internal`, whose first act under a pending stop is to park at
    /// the safepoint. The collection therefore runs exactly inside `new`.
    #[test]
    fn initial_young_value_is_reloaded_after_collection_inside_new() {
        use std::time::{Duration, Instant};
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { crate::gc::willow_gc_init() };
        crate::gc::willow_gc_register_mutator();
        crate::gc::set_gc_stress_for_test(Some(""));
        let tls = crate::gc::tlab_state_for_test();
        // SAFETY: The fixture owns the stable TLAB through unregistration. The requested size
        // and tracing metadata describe the object initialized here.
        let young = unsafe { crate::gc::willow_gc_alloc_slow(tls.as_ptr(), 42, 0, 8, 0) };
        assert!(!young.is_null());
        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        unsafe { *(young as *mut i64) = 4242 };
        let minor_before = crate::gc::willow_gc_minor_collections();

        let collector =
            std::thread::spawn(crate::runtime::serving(crate::gc::minor_collect_internal));
        let deadline = Instant::now() + Duration::from_secs(30);
        while !crate::gc::stop_pending_for_test() {
            assert!(
                Instant::now() < deadline,
                "the collector never requested a stop"
            );
            std::thread::yield_now();
        }
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        // SAFETY: The initial reference comes from a live managed allocation and is current
        // on entry.
        let mut cell = (unsafe { willow_blocking_cell_new(young as i64, 1) }) as *mut u8;
        crate::gc::set_gc_stress_for_test(None);
        collector.join().unwrap();
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut cell) };

        // The constructor's stable root permits copying while it allocates;
        // it reloads the rewritten reference before initializing the payload.
        assert!(
            crate::gc::willow_gc_minor_collections() > minor_before,
            "the collection must complete inside `new`"
        );
        assert_eq!(crate::gc::willow_gc_pinned_region_count(), 0);
        // SAFETY: The matching mutex cell constructor initialized this live payload.
        let held = (unsafe { willow_blocking_cell_get(cell as *mut c_void) }) as *const i64;
        assert_ne!(
            held.cast::<u8>(),
            young.cast_const(),
            "the young value must move"
        );

        // SAFETY: The cell constructor places this lock in word-aligned GC storage of the matching
        // size.
        assert_eq!(unsafe { *held }, 4242, "the held value is intact");
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        crate::gc::willow_gc_unregister_mutator();
    }

    // ── Perspective 16: `new` leaves the root stack balanced ──

    #[test]
    fn new_leaves_the_root_stack_balanced() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal_for_test() };
        let depth = crate::gc::willow_root_depth();
        let child = old_object(9);
        let _keep_child = crate::gc::GcRootHandle::new(child.cast());
        // SAFETY: The initial reference comes from a live managed allocation and is current on
        // entry. Zero initializes an empty reference slot. The zero reference flag makes the
        // initial value a scalar word.
        unsafe {
            willow_blocking_cell_new(child as i64, 1);
            willow_blocking_rw_cell_new(child as i64, 1);
            willow_blocking_cell_new(0, 1);
            willow_blocking_cell_new(5, 0);
        }
        assert_eq!(crate::gc::willow_root_depth(), depth);
    }
}
