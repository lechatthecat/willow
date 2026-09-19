//! Blocking compatibility cells (willow-dgwo.3, willow-38w.1.4/.1.5).
//!
//! `BlockingCell<T>` and `BlockingRwCell<T>` preserve the old single-operation
//! API after public `Mutex<T>`/`RwLock<T>` became scheduler-aware lexical
//! locks. Each holds the inner value as a single 64-bit word (scalars by value,
//! GC values as their pointer; the compiler coerces). A real `std::sync` lock
//! guards the word, so these explicit compatibility types may block a worker
//! and should not be confused with scheduler-aware locks.
//!
//! GC (willow-9tls.5): a cell is an ordinary GC-managed object, allocated
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
const BLOCKING_CELL_TYPE_ID: u32 = 0x10C4_0003;
const BLOCKING_RW_CELL_TYPE_ID: u32 = 0x10C4_0004;

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
unsafe fn snapshot_blocking_cell(payload: *mut u8, children: &mut Vec<*mut u8>) {
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
    unsafe { std::ptr::drop_in_place(payload as *mut WillowBlockingCell) };
    #[cfg(test)]
    BLOCKING_CELL_DROP_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// # Safety
/// `payload` must be a [`WillowBlockingRwCell`] allocated by
/// [`willow_blocking_rw_cell_new`].
unsafe fn trace_blocking_rw_cell(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
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
unsafe fn snapshot_blocking_rw_cell(payload: *mut u8, children: &mut Vec<*mut u8>) {
    let cell = unsafe { &*(payload as *const WillowBlockingRwCell) };
    if cell.is_ref {
        let word = cell.value.read().unwrap_or_else(PoisonError::into_inner);
        children.push(*word as *mut u8);
    }
}

/// # Safety
/// `payload` must point to an initialized [`WillowBlockingRwCell`].
unsafe fn drop_blocking_rw_cell(payload: *mut u8) {
    unsafe { std::ptr::drop_in_place(payload as *mut WillowBlockingRwCell) };
    #[cfg(test)]
    BLOCKING_RW_CELL_DROP_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(test)]
static BLOCKING_CELL_DROP_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static BLOCKING_RW_CELL_DROP_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
static BLOCKING_CELL_REGISTRATION_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

static BLOCKING_CELL_REGISTRATION: NativeGcRegistration = NativeGcRegistration::new();
const BLOCKING_CELL_GC_TYPES: &[NativeGcType] = &[
    NativeGcType::new(
        BLOCKING_CELL_TYPE_ID,
        Some(trace_blocking_cell),
        Some(drop_blocking_cell),
    )
    .with_concurrent_trace(snapshot_blocking_cell),
    NativeGcType::new(
        BLOCKING_RW_CELL_TYPE_ID,
        Some(trace_blocking_rw_cell),
        Some(drop_blocking_rw_cell),
    )
    .with_concurrent_trace(snapshot_blocking_rw_cell),
];

/// Install both cells' GC hooks once per registry generation; the common
/// allocation path pays one atomic load.
fn ensure_blocking_cells_registered() {
    if BLOCKING_CELL_REGISTRATION.ensure(BLOCKING_CELL_GC_TYPES) {
        #[cfg(test)]
        BLOCKING_CELL_REGISTRATION_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Allocate one cell payload with `value` kept alive across the allocation.
///
/// The initial word arrives by value. When it is a GC reference the caller may
/// hold it nowhere else, and the allocation below can collect: a minor
/// collection would free (or, if it is reachable only through a heap slot,
/// move) the object. Rooting the local copy makes it a direct root, which the
/// collector pins in place, so the word written into the payload afterwards is
/// still the object's address. Returns null when the allocation fails.
fn alloc_cell_payload(type_id: u32, payload_size: usize, value: &mut i64, is_ref: bool) -> *mut u8 {
    ensure_blocking_cells_registered();
    let rooted = is_ref && *value != 0;
    if rooted {
        crate::gc::willow_push_root((value as *mut i64).cast::<*mut u8>());
    }
    let payload = crate::gc::willow_alloc_with_layout(
        GcObjectKind::LockHandle,
        type_id,
        payload_size as i64,
        0,
    );
    if rooted {
        crate::gc::willow_pop_roots(1);
    }
    payload
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_cell_new(value: i64, is_ref: i64) -> *mut c_void {
    let is_ref = is_ref != 0;
    let mut value = value;
    let payload = alloc_cell_payload(
        BLOCKING_CELL_TYPE_ID,
        std::mem::size_of::<WillowBlockingCell>(),
        &mut value,
        is_ref,
    );
    if payload.is_null() {
        return std::ptr::null_mut();
    }
    // Placement-init into GC memory; `drop_blocking_cell` runs the destructor
    // during sweep.
    unsafe {
        (payload as *mut WillowBlockingCell).write(WillowBlockingCell {
            value: StdMutex::new(value),
            is_ref,
        });
    }
    if is_ref {
        crate::gc::willow_gc_write_barrier(
            payload,
            value as *mut u8,
            GcStoreDestination::BlockingCell as i64,
        );
    }
    payload as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_cell_get(raw: *mut c_void) -> i64 {
    let m = unsafe { &*(raw as *const WillowBlockingCell) };
    *m.value.lock().expect("mutex poisoned")
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_cell_set(raw: *mut c_void, value: i64) {
    let m = unsafe { &*(raw as *const WillowBlockingCell) };
    if m.is_ref {
        // The barrier is non-safepointing and never allocates, so taking the
        // cell lock afterwards cannot deadlock against a collector; publishing
        // the edge before the store is the concurrent-mark contract.
        crate::gc::willow_gc_write_barrier(
            raw as *mut u8,
            value as *mut u8,
            GcStoreDestination::BlockingCell as i64,
        );
    }
    *m.value.lock().expect("mutex poisoned") = value;
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_rw_cell_new(value: i64, is_ref: i64) -> *mut c_void {
    let is_ref = is_ref != 0;
    let mut value = value;
    let payload = alloc_cell_payload(
        BLOCKING_RW_CELL_TYPE_ID,
        std::mem::size_of::<WillowBlockingRwCell>(),
        &mut value,
        is_ref,
    );
    if payload.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        (payload as *mut WillowBlockingRwCell).write(WillowBlockingRwCell {
            value: StdRwLock::new(value),
            is_ref,
        });
    }
    if is_ref {
        crate::gc::willow_gc_write_barrier(
            payload,
            value as *mut u8,
            GcStoreDestination::BlockingRwCell as i64,
        );
    }
    payload as *mut c_void
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_rw_cell_read(raw: *mut c_void) -> i64 {
    let r = unsafe { &*(raw as *const WillowBlockingRwCell) };
    *r.value.read().expect("rwlock poisoned")
}

#[unsafe(no_mangle)]
pub extern "C" fn willow_blocking_rw_cell_write(raw: *mut c_void, value: i64) {
    let r = unsafe { &*(raw as *const WillowBlockingRwCell) };
    if r.is_ref {
        crate::gc::willow_gc_write_barrier(
            raw as *mut u8,
            value as *mut u8,
            GcStoreDestination::BlockingRwCell as i64,
        );
    }
    *r.value.write().expect("rwlock poisoned") = value;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{
        reset_internal_for_test, runtime_test_guard, total_frees_for_test, willow_alloc_typed,
        willow_gc_allocated_bytes, willow_gc_collect, willow_pop_roots, willow_push_root,
    };
    use std::sync::atomic::Ordering;

    /// A GC object holding one scalar word, allocated in the old generation.
    fn old_object(word: i64) -> *mut u8 {
        let object = willow_alloc_typed(8, 0);
        assert!(!object.is_null());
        unsafe { *(object as *mut i64) = word };
        object
    }

    // ── Perspective 1-2: single-operation accessors keep their semantics ──

    #[test]
    fn mutex_get_set() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let m = willow_blocking_cell_new(7, 0);
        assert_eq!(willow_blocking_cell_get(m), 7);
        willow_blocking_cell_set(m, 42);
        assert_eq!(willow_blocking_cell_get(m), 42);
    }

    #[test]
    fn rwlock_read_write() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let r = willow_blocking_rw_cell_new(1, 0);
        assert_eq!(willow_blocking_rw_cell_read(r), 1);
        willow_blocking_rw_cell_write(r, 100);
        assert_eq!(willow_blocking_rw_cell_read(r), 100);
    }

    // ── Perspective 3-4: unreachable cells are reclaimed; nothing grows ──

    #[test]
    fn unreachable_cells_are_reclaimed_and_the_heap_returns_to_baseline() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let before = willow_gc_allocated_bytes();
        for i in 0..1000 {
            let child = old_object(i);
            assert!(!willow_blocking_cell_new(child as i64, 1).is_null());
            assert!(!willow_blocking_rw_cell_new(child as i64, 1).is_null());
            assert!(!willow_blocking_cell_new(i, 0).is_null());
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
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let cells_before = BLOCKING_CELL_DROP_COUNT.load(Ordering::SeqCst);
        let rw_before = BLOCKING_RW_CELL_DROP_COUNT.load(Ordering::SeqCst);
        const CELLS: usize = 256;
        for i in 0..CELLS {
            willow_blocking_cell_new(i as i64, 0);
            willow_blocking_rw_cell_new(i as i64, 0);
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
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let kept = old_object(0x5EED);
        let mut cell = willow_blocking_cell_new(kept as i64, 1) as *mut u8;
        willow_push_root(&mut cell as *mut *mut u8);
        // A dead cell with a value only it references: exactly two frees.
        willow_blocking_cell_new(old_object(1) as i64, 1);
        let frees = total_frees_for_test();
        willow_gc_collect();
        assert_eq!(
            total_frees_for_test() - frees,
            2,
            "only the dead pair is swept"
        );
        assert_eq!(willow_blocking_cell_get(cell as *mut c_void), kept as i64);
        assert_eq!(unsafe { *(kept as *const i64) }, 0x5EED);
        willow_pop_roots(1);
    }

    #[test]
    fn rooted_rw_cell_keeps_its_reference_value_alive() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let kept = old_object(0x2EAD);
        let mut cell = willow_blocking_rw_cell_new(kept as i64, 1) as *mut u8;
        willow_push_root(&mut cell as *mut *mut u8);
        willow_blocking_rw_cell_new(old_object(1) as i64, 1);
        let frees = total_frees_for_test();
        willow_gc_collect();
        assert_eq!(total_frees_for_test() - frees, 2);
        assert_eq!(
            willow_blocking_rw_cell_read(cell as *mut c_void),
            kept as i64
        );
        assert_eq!(unsafe { *(kept as *const i64) }, 0x2EAD);
        willow_pop_roots(1);
    }

    // ── Perspective 7: a value stored later is traced too ──

    #[test]
    fn value_stored_with_set_survives_collection() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let mut cell = willow_blocking_cell_new(0, 1) as *mut u8;
        willow_push_root(&mut cell as *mut *mut u8);
        let stored = old_object(11);
        willow_blocking_cell_set(cell as *mut c_void, stored as i64);
        let mut rw = willow_blocking_rw_cell_new(0, 1) as *mut u8;
        willow_push_root(&mut rw as *mut *mut u8);
        let written = old_object(22);
        willow_blocking_rw_cell_write(rw as *mut c_void, written as i64);
        let frees = total_frees_for_test();
        willow_gc_collect();
        assert_eq!(total_frees_for_test(), frees, "nothing reachable is freed");
        assert_eq!(unsafe { *(stored as *const i64) }, 11);
        assert_eq!(unsafe { *(written as *const i64) }, 22);
        willow_pop_roots(2);
    }

    // ── Perspective 8-9: the trace hooks expose exactly the reference word ──

    #[test]
    fn scalar_cells_expose_no_gc_slots() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let cell = willow_blocking_cell_new(0x1234, 0) as *mut u8;
        let rw = willow_blocking_rw_cell_new(0x5678, 0) as *mut u8;
        let mut slots = Vec::new();
        unsafe {
            trace_blocking_cell(cell, &mut slots);
            trace_blocking_rw_cell(rw, &mut slots);
        }
        assert!(slots.is_empty(), "a scalar word is never a GC edge");
        let mut children = Vec::new();
        unsafe {
            snapshot_blocking_cell(cell, &mut children);
            snapshot_blocking_rw_cell(rw, &mut children);
        }
        assert!(children.is_empty());
    }

    #[test]
    fn reference_cells_expose_their_word_as_a_rewritable_slot() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let first = old_object(1);
        let second = old_object(2);
        let cell = willow_blocking_cell_new(first as i64, 1) as *mut u8;
        let rw = willow_blocking_rw_cell_new(second as i64, 1) as *mut u8;
        let mut slots = Vec::new();
        unsafe {
            trace_blocking_cell(cell, &mut slots);
            trace_blocking_rw_cell(rw, &mut slots);
        }
        assert_eq!(slots.len(), 2);
        assert_eq!(unsafe { *slots[0] }, first);
        assert_eq!(unsafe { *slots[1] }, second);
        // The collector rewrites a moved child through the slot it was given.
        let moved = old_object(3);
        unsafe { *slots[0] = moved };
        assert_eq!(willow_blocking_cell_get(cell as *mut c_void), moved as i64);
        let mut children = Vec::new();
        unsafe {
            snapshot_blocking_cell(cell, &mut children);
            snapshot_blocking_rw_cell(rw, &mut children);
        }
        assert_eq!(children, vec![moved, second]);
    }

    // ── Perspective 10: a null reference is not reported ──

    #[test]
    fn null_reference_word_is_not_a_slot() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let cell = willow_blocking_cell_new(0, 1) as *mut u8;
        let rw = willow_blocking_rw_cell_new(0, 1) as *mut u8;
        let mut slots = Vec::new();
        unsafe {
            trace_blocking_cell(cell, &mut slots);
            trace_blocking_rw_cell(rw, &mut slots);
        }
        assert!(slots.is_empty());
    }

    // ── Perspective 11: hooks register once per registry generation ──

    #[test]
    fn gc_hooks_register_once_per_registry_generation() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        ensure_blocking_cells_registered();
        let generation = crate::gc::registry_generation();
        let registrations = BLOCKING_CELL_REGISTRATION_COUNT.load(Ordering::SeqCst);
        for i in 0..10_000 {
            willow_blocking_cell_new(i, 0);
        }
        assert_eq!(
            BLOCKING_CELL_REGISTRATION_COUNT.load(Ordering::SeqCst),
            registrations,
            "same-generation cell creation stays on the atomic fast path"
        );
        reset_internal_for_test();
        assert_ne!(crate::gc::registry_generation(), generation);
        willow_blocking_rw_cell_new(1, 0);
        assert_eq!(
            BLOCKING_CELL_REGISTRATION_COUNT.load(Ordering::SeqCst),
            registrations + 1,
            "the first cell after a GC reset reinstalls the hooks once"
        );
    }

    // ── Perspective 12-13: stores into an old cell take the write barrier ──

    #[test]
    fn set_of_a_young_value_remembers_the_cell() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        crate::gc::set_gc_stress_for_test(Some(""));
        let mut tls = crate::gc::tlab_state_for_test();
        let mut cell = willow_blocking_cell_new(0, 1) as *mut u8;
        willow_push_root(&mut cell as *mut *mut u8);
        let young = crate::gc::willow_gc_alloc_slow(&mut tls, 42, 0, 8, 0);
        assert!(!young.is_null());
        unsafe { *(young as *mut i64) = 77 };
        let remembered = crate::gc::willow_gc_remembered_set_size();
        willow_blocking_cell_set(cell as *mut c_void, young as i64);
        assert_eq!(
            crate::gc::willow_gc_remembered_set_size(),
            remembered + 1,
            "an old cell holding a young value joins the remembered set"
        );
        crate::gc::willow_gc_minor_collect();
        let relocated = willow_blocking_cell_get(cell as *mut c_void) as *const i64;
        assert_ne!(
            relocated, young as *const i64,
            "the young value was evacuated"
        );
        assert_eq!(
            unsafe { *relocated },
            77,
            "the cell follows the moved value"
        );
        crate::gc::set_gc_stress_for_test(None);
        willow_pop_roots(1);
    }

    #[test]
    fn write_of_a_young_value_remembers_the_rw_cell() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        crate::gc::set_gc_stress_for_test(Some(""));
        let mut tls = crate::gc::tlab_state_for_test();
        let mut cell = willow_blocking_rw_cell_new(0, 1) as *mut u8;
        willow_push_root(&mut cell as *mut *mut u8);
        let young = crate::gc::willow_gc_alloc_slow(&mut tls, 42, 0, 8, 0);
        assert!(!young.is_null());
        unsafe { *(young as *mut i64) = 88 };
        let remembered = crate::gc::willow_gc_remembered_set_size();
        willow_blocking_rw_cell_write(cell as *mut c_void, young as i64);
        assert_eq!(crate::gc::willow_gc_remembered_set_size(), remembered + 1);
        crate::gc::willow_gc_minor_collect();
        let relocated = willow_blocking_rw_cell_read(cell as *mut c_void) as *const i64;
        assert_ne!(relocated, young as *const i64);
        assert_eq!(unsafe { *relocated }, 88);
        crate::gc::set_gc_stress_for_test(None);
        willow_pop_roots(1);
    }

    // ── Perspective 14: a scalar store never touches the barrier ──

    #[test]
    fn scalar_stores_skip_the_write_barrier() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let cell = willow_blocking_cell_new(0, 0);
        let rw = willow_blocking_rw_cell_new(0, 0);
        let calls = crate::gc::telemetry_heap_snapshot().0.barrier_calls;
        willow_blocking_cell_set(cell, 5);
        willow_blocking_rw_cell_write(rw, 6);
        assert_eq!(crate::gc::telemetry_heap_snapshot().0.barrier_calls, calls);
        assert_eq!(willow_blocking_cell_get(cell), 5);
        assert_eq!(willow_blocking_rw_cell_read(rw), 6);
    }

    // ── Perspective 15: the initial value survives a collection inside `new` ──

    /// The constructor's own allocation can collect. The initial word is a
    /// young object the caller may hold nowhere else, so the runtime roots it
    /// across the allocation; as a direct root it is promoted IN PLACE and the
    /// word written into the fresh payload is still its address.
    ///
    /// Deterministic ordering follows the map-copy recipe (willow-9tls.8): this
    /// thread registers as a mutator so the collector cannot proceed until it
    /// parks; `alloc` stress makes the payload allocation reach
    /// `collect_internal`, whose first act under a pending stop is to park at
    /// the safepoint. The collection therefore runs exactly inside `new`.
    #[test]
    fn initial_young_value_survives_a_collection_inside_new() {
        use std::time::{Duration, Instant};
        let _guard = runtime_test_guard();
        crate::gc::willow_gc_init();
        crate::gc::willow_gc_register_mutator();
        crate::gc::set_gc_stress_for_test(Some(""));
        let mut tls = crate::gc::tlab_state_for_test();
        let young = crate::gc::willow_gc_alloc_slow(&mut tls, 42, 0, 8, 0);
        assert!(!young.is_null());
        unsafe { *(young as *mut i64) = 4242 };
        let promoted_before = crate::gc::willow_gc_promoted_objects();

        let collector = std::thread::spawn(|| crate::gc::willow_gc_minor_collect());
        let deadline = Instant::now() + Duration::from_secs(30);
        while !crate::gc::stop_pending_for_test() {
            assert!(
                Instant::now() < deadline,
                "the collector never requested a stop"
            );
            std::thread::yield_now();
        }
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        let mut cell = willow_blocking_cell_new(young as i64, 1) as *mut u8;
        crate::gc::set_gc_stress_for_test(None);
        collector.join().unwrap();
        willow_push_root(&mut cell as *mut *mut u8);

        assert!(
            crate::gc::willow_gc_promoted_objects() > promoted_before,
            "the collection ran while `new` was allocating"
        );
        let held = willow_blocking_cell_get(cell as *mut c_void) as *const i64;
        assert_eq!(
            held, young as *const i64,
            "a direct root is pinned in place"
        );
        assert_eq!(unsafe { *held }, 4242, "the pinned value is intact");
        willow_pop_roots(1);
        crate::gc::willow_gc_unregister_mutator();
    }

    // ── Perspective 16: `new` leaves the root stack balanced ──

    #[test]
    fn new_leaves_the_root_stack_balanced() {
        let _guard = runtime_test_guard();
        reset_internal_for_test();
        let depth = crate::gc::willow_root_depth();
        let child = old_object(9);
        willow_blocking_cell_new(child as i64, 1);
        willow_blocking_rw_cell_new(child as i64, 1);
        willow_blocking_cell_new(0, 1);
        willow_blocking_cell_new(5, 0);
        assert_eq!(crate::gc::willow_root_depth(), depth);
    }
}
