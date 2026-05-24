// GC Runtime — Stage 1: skeleton + Stage 2: stop-the-world mark-and-sweep
//
// Object layout in memory:
//   [ GcHeader | payload bytes ... ]
//
// The GcHeader is immediately before the payload.  `willow_alloc_object`
// returns a pointer to the payload start, just like malloc.
//
// Root stack: a thread-local Vec of *mut *mut u8.  Each entry points to a
// stack slot that holds a GC-managed pointer.  Generated code pushes a slot
// on entry and pops it on exit.  The mark phase reads through each slot to
// reach the live object.
//
// Heap list: a singly-linked list through GcHeader::next.  The head is
// GC_STATE.heap_head.  All objects are on this list; unreachable ones are
// freed during sweep.

use std::alloc::{Layout, alloc, dealloc};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Object header
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct GcHeader {
    /// Mark bit used during mark phase.
    pub marked: bool,
    /// Runtime type identifier (0 = unknown/opaque for now).
    pub type_id: u32,
    /// Total allocation size in bytes (header + payload).
    pub size: usize,
    /// Next object in the heap linked list.
    pub next: *mut GcHeader,
}

// SAFETY: we guard all access through Mutex<GcState>.
unsafe impl Send for GcHeader {}
unsafe impl Sync for GcHeader {}

// ---------------------------------------------------------------------------
// GC state
// ---------------------------------------------------------------------------

struct GcState {
    /// Head of the heap linked list.
    heap_head: *mut GcHeader,
    /// Total bytes currently allocated (header + payload).
    allocated_bytes: usize,
    /// Trigger a collection when allocated_bytes exceeds this threshold.
    threshold_bytes: usize,
    /// Total objects allocated lifetime.
    total_allocs: u64,
    /// Total objects freed lifetime.
    total_frees: u64,
}

// SAFETY: we always access through Mutex.
unsafe impl Send for GcState {}

static GC_STATE: Mutex<GcState> = Mutex::new(GcState {
    heap_head: std::ptr::null_mut(),
    allocated_bytes: 0,
    threshold_bytes: 1024 * 1024, // 1 MiB initial threshold
    total_allocs: 0,
    total_frees: 0,
});

// Root stack — per-thread explicit shadow stack.
std::thread_local! {
    static ROOT_STACK: std::cell::RefCell<Vec<*mut *mut u8>> =
        std::cell::RefCell::new(Vec::new());
}

// ---------------------------------------------------------------------------
// Public runtime API
// ---------------------------------------------------------------------------

/// Initialize the GC.  Must be called once before any allocation.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_init() {
    // Nothing to initialize beyond the lazy statics for now.
    // Kept as an explicit call so the compiler can emit it at program start.
    let _guard = GC_STATE.lock().unwrap();
}

/// Register a root slot.  `slot` must point to a stack location that holds
/// a GC-managed pointer.  The slot must remain valid until the matching pop.
#[unsafe(no_mangle)]
pub extern "C" fn willow_push_root(slot: *mut *mut u8) {
    ROOT_STACK.with(|rs| rs.borrow_mut().push(slot));
}

/// Unregister the most recently pushed root slot.
#[unsafe(no_mangle)]
pub extern "C" fn willow_pop_root() {
    ROOT_STACK.with(|rs| {
        rs.borrow_mut().pop();
    });
}

/// Unregister `count` root slots from the top of the root stack.
#[unsafe(no_mangle)]
pub extern "C" fn willow_pop_roots(count: i64) {
    ROOT_STACK.with(|rs| {
        let mut stack = rs.borrow_mut();
        let remove = (count as usize).min(stack.len());
        let new_len = stack.len() - remove;
        stack.truncate(new_len);
    });
}

/// Allocate a GC-managed object of `payload_size` bytes with the given
/// `type_id`.  Returns a pointer to the **payload** (past the header), or
/// null on allocation failure.
///
/// This function may trigger a collection if the heap threshold is exceeded.
#[unsafe(no_mangle)]
pub extern "C" fn willow_alloc_object(type_id: i64, payload_size: i64) -> *mut u8 {
    let payload_size = payload_size as usize;
    let total = std::mem::size_of::<GcHeader>() + payload_size;

    // Trigger collection if above threshold (before allocating more).
    {
        let state = GC_STATE.lock().unwrap();
        if state.allocated_bytes >= state.threshold_bytes {
            drop(state);
            collect_internal();
        }
    }

    // SAFETY: Layout::from_size_align is safe with valid alignment.
    let layout = match Layout::from_size_align(total, std::mem::align_of::<GcHeader>()) {
        Ok(l) => l,
        Err(_) => return std::ptr::null_mut(),
    };
    // SAFETY: alloc returns null on failure; we check below.
    let raw = unsafe { alloc(layout) };
    if raw.is_null() {
        return std::ptr::null_mut();
    }

    // Initialize the header.
    let header = raw as *mut GcHeader;
    unsafe {
        (*header).marked = false;
        (*header).type_id = type_id as u32;
        (*header).size = total;
        (*header).next = std::ptr::null_mut();
    }

    // Prepend to the heap linked list.
    {
        let mut state = GC_STATE.lock().unwrap();
        unsafe { (*header).next = state.heap_head };
        state.heap_head = header;
        state.allocated_bytes += total;
        state.total_allocs += 1;

        // Double the threshold when we fill it, so amortized O(1) collections.
        if state.allocated_bytes >= state.threshold_bytes {
            state.threshold_bytes *= 2;
        }
    }

    // Return the payload pointer.
    unsafe { raw.add(std::mem::size_of::<GcHeader>()) }
}

/// Trigger a full stop-the-world mark-and-sweep collection.
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_collect() {
    collect_internal();
}

/// Return the total bytes currently on the GC heap (header + payload).
#[unsafe(no_mangle)]
pub extern "C" fn willow_gc_allocated_bytes() -> i64 {
    GC_STATE.lock().unwrap().allocated_bytes as i64
}

// ---------------------------------------------------------------------------
// Internal collection
// ---------------------------------------------------------------------------

fn collect_internal() {
    let gc_log = std::env::var("WILLOW_GC_LOG").is_ok();

    let heap_before;
    {
        let state = GC_STATE.lock().unwrap();
        heap_before = state.allocated_bytes;
    }

    // ---- Mark phase --------------------------------------------------------
    // Walk every root slot and mark the object it points to.
    ROOT_STACK.with(|rs| {
        let stack = rs.borrow();
        for &slot in stack.iter() {
            if slot.is_null() {
                continue;
            }
            // SAFETY: slot is a valid stack location pushed by willow_push_root.
            let obj_ptr = unsafe { *slot };
            if !obj_ptr.is_null() {
                mark_object(obj_ptr);
            }
        }
    });

    // ---- Sweep phase -------------------------------------------------------
    let freed = sweep();

    if gc_log {
        let state = GC_STATE.lock().unwrap();
        eprintln!(
            "gc: heap_before={}B freed={}B heap_after={}B total_allocs={} total_frees={}",
            heap_before, freed, state.allocated_bytes, state.total_allocs, state.total_frees,
        );
    }
}

/// Mark the object whose **payload** pointer is `obj_ptr`.
fn mark_object(obj_ptr: *mut u8) {
    if obj_ptr.is_null() {
        return;
    }
    let header = payload_to_header(obj_ptr);
    // SAFETY: header was written by willow_alloc_object; memory is still valid.
    unsafe {
        if (*header).marked {
            return; // already visited
        }
        (*header).marked = true;
        // Tracing GC references inside the object would go here once we have
        // TypeInfo tables.  For now, opaque objects have no interior pointers.
    }
}

/// Walk the heap linked list, free unmarked objects, clear marks on survivors.
/// Returns total bytes freed.
fn sweep() -> usize {
    let mut freed_bytes = 0usize;
    let mut freed_count = 0u64;

    let mut state = GC_STATE.lock().unwrap();
    let mut prev_next: *mut *mut GcHeader = &mut state.heap_head as *mut *mut GcHeader;

    let mut current = state.heap_head;
    while !current.is_null() {
        // SAFETY: current is a valid allocation from willow_alloc_object.
        let (marked, size, next) = unsafe { ((*current).marked, (*current).size, (*current).next) };

        if marked {
            // Survivor: clear the mark and advance.
            unsafe { (*current).marked = false };
            unsafe { *prev_next = current };
            prev_next = unsafe { &mut (*current).next as *mut *mut GcHeader };
            current = next;
        } else {
            // Unreachable: unlink and free.
            unsafe { *prev_next = next };
            // SAFETY: layout matches the one used in willow_alloc_object.
            let layout = Layout::from_size_align(size, std::mem::align_of::<GcHeader>()).unwrap();
            unsafe { dealloc(current as *mut u8, layout) };
            freed_bytes += size;
            freed_count += 1;
            state.allocated_bytes = state.allocated_bytes.saturating_sub(size);
            state.total_frees += 1;
            current = next;
        }
    }
    // Terminate the list properly.
    unsafe { *prev_next = std::ptr::null_mut() };

    let _ = freed_count;
    freed_bytes
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Given a payload pointer, return the GcHeader pointer just before it.
fn payload_to_header(payload: *mut u8) -> *mut GcHeader {
    let header_size = std::mem::size_of::<GcHeader>();
    unsafe { (payload as *mut u8).sub(header_size) as *mut GcHeader }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn gc_test_guard() -> MutexGuard<'static, ()> {
        TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn reset_gc() {
        let mut state = GC_STATE.lock().unwrap();
        // Free all objects to start clean.
        let mut current = state.heap_head;
        while !current.is_null() {
            let (size, next) = unsafe { ((*current).size, (*current).next) };
            let layout = Layout::from_size_align(size, std::mem::align_of::<GcHeader>()).unwrap();
            unsafe { dealloc(current as *mut u8, layout) };
            current = next;
        }
        state.heap_head = std::ptr::null_mut();
        state.allocated_bytes = 0;
        state.threshold_bytes = 1024 * 1024;
        state.total_allocs = 0;
        state.total_frees = 0;
        ROOT_STACK.with(|rs| rs.borrow_mut().clear());
    }

    #[test]
    fn test_gc_alloc_returns_non_null() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 16);
        assert!(!ptr.is_null());
        reset_gc();
    }

    #[test]
    fn test_gc_alloc_zero_size_object() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(0, 0);
        assert!(!ptr.is_null(), "zero-payload allocation should succeed");
        reset_gc();
    }

    #[test]
    fn test_gc_allocated_bytes_increases() {
        let _guard = gc_test_guard();
        reset_gc();
        let before = willow_gc_allocated_bytes();
        willow_alloc_object(1, 64);
        let after = willow_gc_allocated_bytes();
        assert!(after > before);
        reset_gc();
    }

    #[test]
    fn test_gc_collect_frees_unreachable_objects() {
        let _guard = gc_test_guard();
        reset_gc();
        // Allocate an object but don't root it.
        willow_alloc_object(1, 128);
        let before = willow_gc_allocated_bytes();
        assert!(before > 0);
        // Collection should reclaim it.
        willow_gc_collect();
        let after = willow_gc_allocated_bytes();
        assert_eq!(after, 0, "unrooted object should be freed");
        reset_gc();
    }

    #[test]
    fn test_gc_collect_preserves_rooted_objects() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(2, 32);
        // Root the object via its slot.
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        let after = willow_gc_allocated_bytes();
        assert!(after > 0, "rooted object must survive collection");
        willow_pop_root();
        // After unrooting, collection should free it.
        willow_gc_collect();
        let final_bytes = willow_gc_allocated_bytes();
        assert_eq!(final_bytes, 0, "unrooted object should be freed after pop");
        reset_gc();
    }

    #[test]
    fn test_gc_root_push_pop_symmetry() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slot1: *mut u8 = std::ptr::null_mut();
        let mut slot2: *mut u8 = std::ptr::null_mut();
        willow_push_root(&mut slot1 as *mut *mut u8);
        willow_push_root(&mut slot2 as *mut *mut u8);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 2));
        willow_pop_root();
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 1));
        willow_pop_roots(1);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
        reset_gc();
    }

    #[test]
    fn test_gc_multiple_allocs_and_collect() {
        let _guard = gc_test_guard();
        reset_gc();
        // Allocate 10 objects; root only the last one.
        for _ in 0..9 {
            willow_alloc_object(1, 16);
        }
        let last_ptr = willow_alloc_object(1, 16);
        let mut slot = last_ptr;
        willow_push_root(&mut slot as *mut *mut u8);

        willow_gc_collect();

        // Only the rooted object should survive.
        let header_size = std::mem::size_of::<GcHeader>();
        let expected = (header_size + 16) as i64;
        assert_eq!(
            willow_gc_allocated_bytes(),
            expected,
            "exactly one object should survive"
        );
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }
}
