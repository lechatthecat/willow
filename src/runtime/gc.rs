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
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

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
// TypeInfo registry
// ---------------------------------------------------------------------------

/// Trace function: given a payload pointer, push the GC-managed child pointers
/// it contains into `children`.  Called by the mark phase for interior tracing.
pub type TraceFn = unsafe fn(payload: *mut u8, children: &mut Vec<*mut u8>);

static TYPE_REGISTRY: OnceLock<Mutex<HashMap<u32, TraceFn>>> = OnceLock::new();

fn type_registry() -> &'static Mutex<HashMap<u32, TraceFn>> {
    TYPE_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register a trace function for `type_id`.  Call once per class at startup.
pub fn willow_register_type(type_id: u32, trace: TraceFn) {
    type_registry().lock().unwrap().insert(type_id, trace);
}

/// Unregister the trace function for `type_id`.
pub fn willow_unregister_type(type_id: u32) {
    type_registry().lock().unwrap().remove(&type_id);
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
    // Collect roots, then do worklist-based marking with interior pointer
    // tracing via the TypeInfo registry.
    ROOT_STACK.with(|rs| {
        let mut worklist: Vec<*mut u8> = {
            let stack = rs.borrow();
            stack
                .iter()
                .filter(|&&slot| !slot.is_null())
                .filter_map(|&slot| {
                    // SAFETY: slot is a valid stack location from willow_push_root.
                    let p = unsafe { *slot };
                    if p.is_null() { None } else { Some(p) }
                })
                .collect()
        };

        while let Some(obj_ptr) = worklist.pop() {
            let header = payload_to_header(obj_ptr);
            // SAFETY: header was written by willow_alloc_object.
            unsafe {
                if (*header).marked {
                    continue; // already visited — handles cycles
                }
                (*header).marked = true;
            }
            let type_id = unsafe { (*header).type_id };
            let trace_fn = type_registry().lock().unwrap().get(&type_id).copied();
            if let Some(trace) = trace_fn {
                let mut children: Vec<*mut u8> = Vec::new();
                // SAFETY: trace is the registered function for this type_id.
                unsafe { trace(obj_ptr, &mut children) };
                worklist.extend(children.into_iter().filter(|&p| !p.is_null()));
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
        type_registry().lock().unwrap().clear();
    }

    fn set_threshold(bytes: usize) {
        GC_STATE.lock().unwrap().threshold_bytes = bytes;
    }

    fn total_allocs() -> u64 {
        GC_STATE.lock().unwrap().total_allocs
    }

    fn total_frees() -> u64 {
        GC_STATE.lock().unwrap().total_frees
    }

    fn header_size() -> usize {
        std::mem::size_of::<GcHeader>()
    }

    fn obj_size(payload: usize) -> i64 {
        (header_size() + payload) as i64
    }

    // -------------------------------------------------------------------------
    // 基本: alloc
    // -------------------------------------------------------------------------

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

    /// allocated_bytes はヘッダ込みのサイズを追跡していること
    #[test]
    fn test_gc_allocated_bytes_includes_header_overhead() {
        let _guard = gc_test_guard();
        reset_gc();
        let payload: i64 = 40;
        willow_alloc_object(1, payload);
        let expected = (header_size() as i64) + payload;
        assert_eq!(
            willow_gc_allocated_bytes(),
            expected,
            "allocated_bytes must include GcHeader overhead"
        );
        reset_gc();
    }

    /// total_allocs カウンタが増える
    #[test]
    fn test_gc_total_allocs_counter() {
        let _guard = gc_test_guard();
        reset_gc();
        let before = total_allocs();
        willow_alloc_object(1, 8);
        willow_alloc_object(1, 8);
        willow_alloc_object(1, 8);
        assert_eq!(total_allocs(), before + 3);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 回収: unreachable objects
    // -------------------------------------------------------------------------

    #[test]
    fn test_gc_collect_frees_unreachable_objects() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 128);
        let before = willow_gc_allocated_bytes();
        assert!(before > 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0, "unrooted object should be freed");
        reset_gc();
    }

    /// collect 後に total_frees が増えること
    #[test]
    fn test_gc_total_frees_counter_after_collect() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 16);
        willow_alloc_object(1, 16);
        let before = total_frees();
        willow_gc_collect();
        assert_eq!(total_frees(), before + 2, "two unrooted objects should be freed");
        reset_gc();
    }

    /// collect を複数回呼んでも壊れない (2回目以降は何も回収しない)
    #[test]
    fn test_gc_collect_idempotent_on_empty_heap() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_gc_collect();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    /// collect 後に生き残ったオブジェクトの mark bit がリセットされること
    /// (リセットされないと次サイクルで全部生存扱いになる)
    #[test]
    fn test_gc_survivor_mark_bit_cleared_after_collection() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 16);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);

        // 1回目のGC: 生き残る
        willow_gc_collect();
        // mark bit が false に戻っているかヘッダで確認
        let hdr = payload_to_header(ptr);
        assert!(!unsafe { (*hdr).marked }, "mark bit must be cleared after collection");

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // ルート管理
    // -------------------------------------------------------------------------

    #[test]
    fn test_gc_collect_preserves_rooted_objects() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(2, 32);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert!(willow_gc_allocated_bytes() > 0, "rooted object must survive");
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0, "unrooted object freed after pop");
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

    /// pop_roots(0) はスタックを変えない
    #[test]
    fn test_gc_pop_roots_zero_is_noop() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slot: *mut u8 = std::ptr::null_mut();
        willow_push_root(&mut slot as *mut *mut u8);
        willow_pop_roots(0);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 1, "pop_roots(0) must not change stack"));
        willow_pop_root();
        reset_gc();
    }

    /// pop_roots(n > stack size) はアンダーフローしない
    #[test]
    fn test_gc_pop_roots_excess_clamps_to_zero() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slot: *mut u8 = std::ptr::null_mut();
        willow_push_root(&mut slot as *mut *mut u8);
        // スタックに1つしかないのに100個pop → 0になるだけ、クラッシュしない
        willow_pop_roots(100);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0, "stack must clamp to 0"));
        reset_gc();
    }

    /// スロットの値が null のルートがあっても GC はクラッシュしない
    #[test]
    fn test_gc_null_root_value_does_not_crash() {
        let _guard = gc_test_guard();
        reset_gc();
        // slot は非null だが、slot の指す先 (ポインタ値) が null
        let mut slot: *mut u8 = std::ptr::null_mut();
        willow_push_root(&mut slot as *mut *mut u8);
        // GC はこの null ポインタをスキップするだけ、クラッシュしない
        willow_gc_collect();
        willow_pop_root();
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 複合シナリオ
    // -------------------------------------------------------------------------

    #[test]
    fn test_gc_multiple_allocs_and_collect() {
        let _guard = gc_test_guard();
        reset_gc();
        for _ in 0..9 {
            willow_alloc_object(1, 16);
        }
        let last_ptr = willow_alloc_object(1, 16);
        let mut slot = last_ptr;
        willow_push_root(&mut slot as *mut *mut u8);

        willow_gc_collect();

        let expected = (header_size() + 16) as i64;
        assert_eq!(willow_gc_allocated_bytes(), expected, "exactly one object must survive");
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    /// 100個確保して1つだけルートを張る → collect後にちょうど1個残る
    #[test]
    fn test_gc_large_population_single_survivor() {
        let _guard = gc_test_guard();
        reset_gc();
        for _ in 0..99 {
            willow_alloc_object(1, 8);
        }
        let survivor = willow_alloc_object(2, 8);
        let mut slot = survivor;
        willow_push_root(&mut slot as *mut *mut u8);

        willow_gc_collect();

        let expected = (header_size() + 8) as i64;
        assert_eq!(
            willow_gc_allocated_bytes(),
            expected,
            "100 objects allocated, only the rooted one should survive"
        );
        assert_eq!(total_frees(), 99, "99 unreachable objects should be freed");
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    /// collect後にまた確保できること (ヒープが壊れていないこと)
    #[test]
    fn test_gc_reallocation_after_collection() {
        let _guard = gc_test_guard();
        reset_gc();
        // 1回目の確保・回収サイクル
        willow_alloc_object(1, 32);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);

        // 2回目: 回収後も新規確保できること
        let ptr = willow_alloc_object(1, 32);
        assert!(!ptr.is_null(), "allocation after collection must succeed");
        assert_eq!(willow_gc_allocated_bytes(), (header_size() + 32) as i64);

        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    /// しきい値を超えた確保が自動でGCをトリガーすること
    #[test]
    fn test_gc_auto_trigger_by_threshold() {
        let _guard = gc_test_guard();
        reset_gc();

        // まず1つ確保してしきい値をそのオブジェクトのサイズより小さく設定
        let obj1 = willow_alloc_object(1, 16);
        assert!(!obj1.is_null());
        let bytes_after_first = willow_gc_allocated_bytes();

        // しきい値を「現在の確保量より小さい値」に設定
        // → 次の willow_alloc_object の冒頭で auto-collect が走る
        set_threshold(1); // 1 バイト → 確実に超えている

        let frees_before = total_frees();

        // obj1 はルートなし → 自動GCで回収されるはず
        let obj2 = willow_alloc_object(1, 16);
        assert!(!obj2.is_null());

        // obj1 (unrooted) が回収されて obj2 だけ残っているはず
        let expected = (header_size() + 16) as i64;
        assert_eq!(
            willow_gc_allocated_bytes(),
            expected,
            "auto-triggered GC should have freed obj1"
        );
        assert!(total_frees() > frees_before, "auto-triggered GC should have incremented total_frees");
        let _ = bytes_after_first;

        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    /// 複数ルートのうち1つだけ外すと、外したものだけ回収される
    #[test]
    fn test_gc_partial_roots_partial_collection() {
        let _guard = gc_test_guard();
        reset_gc();

        let ptr_a = willow_alloc_object(1, 16);
        let ptr_b = willow_alloc_object(2, 16);
        let mut slot_a: *mut u8 = ptr_a;
        let mut slot_b: *mut u8 = ptr_b;
        willow_push_root(&mut slot_a as *mut *mut u8); // A をルート
        willow_push_root(&mut slot_b as *mut *mut u8); // B をルート

        // B のルートを外す
        willow_pop_root();

        willow_gc_collect();

        // A だけ生き残っているはず
        let expected = (header_size() + 16) as i64;
        assert_eq!(
            willow_gc_allocated_bytes(),
            expected,
            "only A (still rooted) should survive"
        );

        willow_pop_root(); // A のルートを外す
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点1: 大きなペイロード (1MB) の確保が成功する
    // =========================================================================
    #[test]
    fn test_gc_large_payload_alloc_succeeds() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 1024 * 1024);
        assert!(!ptr.is_null(), "1 MiB payload allocation must succeed");
        reset_gc();
    }

    // 観点2: 連続確保でヒープリストが更新される
    #[test]
    fn test_gc_consecutive_allocs_update_heap_head() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 8);
        let head1 = GC_STATE.lock().unwrap().heap_head;
        willow_alloc_object(2, 8);
        let head2 = GC_STATE.lock().unwrap().heap_head;
        assert_ne!(head1, head2, "heap_head must change after each alloc");
        reset_gc();
    }

    // 観点3: 確保したペイロード領域に読み書きできる
    #[test]
    fn test_gc_payload_is_readable_writable() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8) as *mut i64;
        unsafe {
            *ptr = 0x0DEADBEEF_i64;
            assert_eq!(*ptr, 0x0DEADBEEF_i64);
        }
        reset_gc();
    }

    // 観点4: 複数の type_id を持つオブジェクトが混在して確保できる
    #[test]
    fn test_gc_multiple_type_ids_work() {
        let _guard = gc_test_guard();
        reset_gc();
        let p1 = willow_alloc_object(1, 8);
        let p2 = willow_alloc_object(2, 8);
        let p3 = willow_alloc_object(99, 8);
        assert!(!p1.is_null() && !p2.is_null() && !p3.is_null());
        unsafe {
            assert_eq!((*payload_to_header(p1)).type_id, 1);
            assert_eq!((*payload_to_header(p2)).type_id, 2);
            assert_eq!((*payload_to_header(p3)).type_id, 99);
        }
        reset_gc();
    }

    // 観点5: 確保直後の GcHeader.marked が false
    #[test]
    fn test_gc_header_marked_false_after_alloc() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8);
        assert!(!unsafe { (*payload_to_header(ptr)).marked });
        reset_gc();
    }

    // 観点6: 確保直後の GcHeader.type_id が指定値と一致
    #[test]
    fn test_gc_header_type_id_matches() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(42, 8);
        assert_eq!(unsafe { (*payload_to_header(ptr)).type_id }, 42);
        reset_gc();
    }

    // 観点7: GcHeader.size がヘッダ+ペイロードと一致
    #[test]
    fn test_gc_header_size_field_matches_total() {
        let _guard = gc_test_guard();
        reset_gc();
        let payload: i64 = 24;
        let ptr = willow_alloc_object(1, payload);
        let expected = header_size() + payload as usize;
        assert_eq!(unsafe { (*payload_to_header(ptr)).size }, expected);
        reset_gc();
    }

    // 観点8: ヒープリストのリンク順序が正しい
    #[test]
    fn test_gc_heap_list_link_order() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr1 = willow_alloc_object(1, 8);
        let hdr1 = payload_to_header(ptr1);
        let ptr2 = willow_alloc_object(2, 8);
        let hdr2 = payload_to_header(ptr2);
        let head = GC_STATE.lock().unwrap().heap_head;
        assert_eq!(head, hdr2, "most recent alloc must be heap_head");
        assert_eq!(unsafe { (*hdr2).next }, hdr1, "hdr2.next must point to hdr1");
        assert!(unsafe { (*hdr1).next }.is_null(), "hdr1.next must be null");
        reset_gc();
    }

    // =========================================================================
    // 観点9: push_root n回でスタックが n 増える
    // =========================================================================
    #[test]
    fn test_gc_push_root_n_times_increases_stack_by_n() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slots = [std::ptr::null_mut::<u8>(); 5];
        for s in slots.iter_mut() {
            willow_push_root(s as *mut *mut u8);
        }
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 5));
        willow_pop_roots(5);
        reset_gc();
    }

    // 観点10: pop_roots(n) でちょうど n 個減る
    #[test]
    fn test_gc_pop_roots_n_decreases_by_exactly_n() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slot = std::ptr::null_mut::<u8>();
        for _ in 0..6 {
            willow_push_root(&mut slot as *mut *mut u8);
        }
        willow_pop_roots(4);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 2));
        willow_pop_roots(2);
        reset_gc();
    }

    // 観点11: push→pop→push のスロット再利用
    #[test]
    fn test_gc_push_pop_push_slot_reuse() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr1 = willow_alloc_object(1, 8);
        let mut slot: *mut u8 = ptr1;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect(); // ptr1 survives
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8));
        willow_pop_root();

        // 新しいオブジェクトを確保してスロットを再利用
        let ptr2 = willow_alloc_object(2, 8);
        slot = ptr2;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect(); // ptr1 freed, ptr2 survives
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8));

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点12: 同じスロットを2回 push するとスタックに2エントリ入る
    #[test]
    fn test_gc_same_slot_pushed_twice_creates_two_entries() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut slot = std::ptr::null_mut::<u8>();
        willow_push_root(&mut slot as *mut *mut u8);
        willow_push_root(&mut slot as *mut *mut u8);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 2));
        willow_pop_roots(2);
        reset_gc();
    }

    // 観点13: 空スタックで pop_root を呼んでもクラッシュしない
    #[test]
    fn test_gc_pop_root_on_empty_stack_no_crash() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_pop_root();
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
        reset_gc();
    }

    // 観点14: 空スタックで pop_roots(n) を呼んでもクラッシュしない
    #[test]
    fn test_gc_pop_roots_on_empty_stack_no_crash() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_pop_roots(5);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
        reset_gc();
    }

    // 観点15: 4000個近くまで push_root できる
    #[test]
    fn test_gc_push_root_near_max_capacity() {
        let _guard = gc_test_guard();
        reset_gc();
        const N: usize = 4000;
        let mut slots = vec![std::ptr::null_mut::<u8>(); N];
        for s in slots.iter_mut() {
            willow_push_root(s as *mut *mut u8);
        }
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), N));
        willow_pop_roots(N as i64);
        ROOT_STACK.with(|rs| assert_eq!(rs.borrow().len(), 0));
        reset_gc();
    }

    // =========================================================================
    // 観点16-17: マークフェーズ
    // =========================================================================

    // 観点16: 同じオブジェクトを2つのルートが指しても二重マークでクラッシュしない
    #[test]
    fn test_gc_two_roots_same_object_no_double_mark_crash() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8);
        let mut s1: *mut u8 = ptr;
        let mut s2: *mut u8 = ptr;
        willow_push_root(&mut s1 as *mut *mut u8);
        willow_push_root(&mut s2 as *mut *mut u8);
        willow_gc_collect(); // must not crash or double-free
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8), "object must survive");
        willow_pop_roots(2);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点21-25: スイープフェーズ
    // =========================================================================

    // 観点21: ヒープ先頭 (heap_head) が unreachable でも正しく回収
    #[test]
    fn test_gc_sweep_head_object_unreachable() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr_a = willow_alloc_object(1, 8); // first alloc → becomes tail
        let _ptr_b = willow_alloc_object(2, 8); // second alloc → becomes head (unreachable)
        let mut slot_a: *mut u8 = ptr_a;
        willow_push_root(&mut slot_a as *mut *mut u8); // only A rooted
        willow_gc_collect();
        // B (head) freed, A (tail→new head) survives
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8));
        assert_eq!(GC_STATE.lock().unwrap().heap_head, payload_to_header(ptr_a));
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点22: ヒープ末尾が unreachable でも正しく回収
    #[test]
    fn test_gc_sweep_tail_object_unreachable() {
        let _guard = gc_test_guard();
        reset_gc();
        let _ptr_a = willow_alloc_object(1, 8); // first → tail (unreachable)
        let ptr_b = willow_alloc_object(2, 8);  // second → head (rooted)
        let mut slot_b: *mut u8 = ptr_b;
        willow_push_root(&mut slot_b as *mut *mut u8);
        willow_gc_collect();
        // A (tail) freed, B (head) survives
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8));
        assert_eq!(GC_STATE.lock().unwrap().heap_head, payload_to_header(ptr_b));
        assert!(unsafe { (*payload_to_header(ptr_b)).next }.is_null());
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点23: ヒープ中間のオブジェクトだけ unreachable でも正しく回収
    #[test]
    fn test_gc_sweep_middle_object_unreachable() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr_a = willow_alloc_object(1, 8); // tail
        let _ptr_b = willow_alloc_object(2, 8); // middle (unreachable)
        let ptr_c = willow_alloc_object(3, 8); // head
        let mut sa: *mut u8 = ptr_a;
        let mut sc: *mut u8 = ptr_c;
        willow_push_root(&mut sa as *mut *mut u8);
        willow_push_root(&mut sc as *mut *mut u8);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8) * 2, "A and C survive, B freed");
        willow_pop_roots(2);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点24: sweep後にヒープリストが null で終端される
    #[test]
    fn test_gc_heap_null_terminated_after_sweep() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert!(unsafe { (*payload_to_header(ptr)).next }.is_null());
        willow_pop_root();
        willow_gc_collect();
        reset_gc();
    }

    // 観点25: 全員生き残ったときリンクリストが壊れていない
    #[test]
    fn test_gc_survivors_remain_linked_after_sweep() {
        let _guard = gc_test_guard();
        reset_gc();
        let pa = willow_alloc_object(1, 8);
        let pb = willow_alloc_object(2, 8);
        let pc = willow_alloc_object(3, 8);
        let mut sa: *mut u8 = pa;
        let mut sb: *mut u8 = pb;
        let mut sc: *mut u8 = pc;
        willow_push_root(&mut sa as *mut *mut u8);
        willow_push_root(&mut sb as *mut *mut u8);
        willow_push_root(&mut sc as *mut *mut u8);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8) * 3);
        // ヒープリストをたどって3個確認
        let mut count = 0;
        let mut cur = GC_STATE.lock().unwrap().heap_head;
        while !cur.is_null() {
            count += 1;
            cur = unsafe { (*cur).next };
        }
        assert_eq!(count, 3, "all 3 survivors must be linked");
        willow_pop_roots(3);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点26-29: allocated_bytes の正確性
    // =========================================================================

    // 観点26: 確保するたびに正確に (header+payload) ずつ増える
    #[test]
    fn test_gc_allocated_bytes_grows_precisely_per_alloc() {
        let _guard = gc_test_guard();
        reset_gc();
        let step = obj_size(16);
        willow_alloc_object(1, 16);
        assert_eq!(willow_gc_allocated_bytes(), step);
        willow_alloc_object(1, 16);
        assert_eq!(willow_gc_allocated_bytes(), step * 2);
        willow_alloc_object(1, 16);
        assert_eq!(willow_gc_allocated_bytes(), step * 3);
        reset_gc();
    }

    // 観点28: partial collect で回収分だけ減り、生存分は保持される
    #[test]
    fn test_gc_partial_collect_bytes_accurate() {
        let _guard = gc_test_guard();
        reset_gc();
        let pa = willow_alloc_object(1, 8); // freed
        let pb = willow_alloc_object(2, 8); // survives
        let _pc = willow_alloc_object(3, 8); // freed
        let mut sb: *mut u8 = pb;
        willow_push_root(&mut sb as *mut *mut u8);
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8) * 3);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8), "only B survives");
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        let _ = pa;
        reset_gc();
    }

    // 観点29: 5つルートを張った5個全員の合計バイトが正確
    #[test]
    fn test_gc_five_roots_five_survivors_bytes() {
        let _guard = gc_test_guard();
        reset_gc();
        let mut ptrs: Vec<*mut u8> = (0..5).map(|i| willow_alloc_object(i, 8)).collect();
        for p in ptrs.iter_mut() {
            willow_push_root(p as *mut *mut u8);
        }
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), obj_size(8) * 5);
        willow_pop_roots(5);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点30-34: threshold / 自動トリガー
    // =========================================================================

    // 観点30: threshold 未満では auto-collect が走らない
    #[test]
    fn test_gc_no_auto_trigger_below_threshold() {
        let _guard = gc_test_guard();
        reset_gc();
        set_threshold(usize::MAX);
        let before = total_frees();
        for _ in 0..10 {
            willow_alloc_object(1, 8);
        }
        assert_eq!(total_frees(), before, "no auto-collect should have run");
        reset_gc();
    }

    // 観点31: auto-collect 後に threshold が2倍になる
    #[test]
    fn test_gc_threshold_doubles_after_auto_trigger() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 8); // allocated_bytes = header+8
        set_threshold(1); // 現在の allocated_bytes より小さく設定
        willow_alloc_object(1, 8); // 先頭で auto-collect、その後確保
        let new_threshold = GC_STATE.lock().unwrap().threshold_bytes;
        assert!(new_threshold >= 2, "threshold must have at least doubled from 1");
        reset_gc();
    }

    // 観点32: auto-collect 後に新規確保が正常にできる
    #[test]
    fn test_gc_realloc_after_auto_trigger_works() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 16);
        set_threshold(1);
        let ptr = willow_alloc_object(1, 16);
        assert!(!ptr.is_null());
        assert!(willow_gc_allocated_bytes() > 0);
        reset_gc();
    }

    // 観点33: threshold=1 で毎回 auto-collect がトリガーされる
    #[test]
    fn test_gc_threshold_one_every_alloc_triggers_collect() {
        let _guard = gc_test_guard();
        reset_gc();
        set_threshold(1);
        let before = total_frees();
        for _ in 0..5 {
            willow_alloc_object(1, 8);
        }
        assert!(total_frees() > before, "auto-collect must fire at least once");
        reset_gc();
    }

    // 観点34: 100回 auto-collect サイクルを繰り返してもメモリリークなし
    #[test]
    fn test_gc_100_auto_trigger_cycles_no_leak() {
        let _guard = gc_test_guard();
        reset_gc();
        set_threshold(1);
        for _ in 0..100 {
            willow_alloc_object(1, 8);
        }
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点35-39: ライフサイクル
    // =========================================================================

    // 観点35: alloc→root→collect(生存)→unroot→collect(回収) の完全1サイクル
    #[test]
    fn test_gc_full_lifecycle() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 16);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert!(willow_gc_allocated_bytes() > 0, "rooted object must survive");
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0, "unrooted object must be freed");
        reset_gc();
    }

    // 観点36: rooted のまま collect を 10回繰り返しても毎回生き残る
    #[test]
    fn test_gc_repeated_collect_rooted_object_survives() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        for i in 0..10 {
            willow_gc_collect();
            assert!(
                willow_gc_allocated_bytes() > 0,
                "object must survive collection #{i}"
            );
        }
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点37: alloc→collect を 100サイクル繰り返してもリークなし
    #[test]
    fn test_gc_100_alloc_collect_cycles_no_leak() {
        let _guard = gc_test_guard();
        reset_gc();
        for _ in 0..100 {
            willow_alloc_object(1, 16);
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
        reset_gc();
    }

    // 観点38: collect 後も生き残ったオブジェクトのペイロード値が変化していない
    #[test]
    fn test_gc_payload_unchanged_after_collection() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8) as *mut i64;
        unsafe { *ptr = 0xCAFEBABE_i64; }
        let mut slot: *mut u8 = ptr as *mut u8;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert_eq!(
            unsafe { *ptr },
            0xCAFEBABE_i64,
            "GC must not corrupt payload data"
        );
        willow_pop_root();
        reset_gc();
    }

    // 観点39: unroot 直後の collect でオブジェクトが即座に回収される
    #[test]
    fn test_gc_collect_immediately_after_unroot() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 8);
        let mut slot: *mut u8 = ptr;
        willow_push_root(&mut slot as *mut *mut u8);
        willow_gc_collect();
        assert!(willow_gc_allocated_bytes() > 0);
        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0, "must be freed immediately after unroot");
        reset_gc();
    }

    // =========================================================================
    // 観点40-42: カウンタの正確性
    // =========================================================================

    // 観点40: reset 後に total_allocs が 0 から始まる
    #[test]
    fn test_gc_total_allocs_starts_at_zero_after_reset() {
        let _guard = gc_test_guard();
        reset_gc();
        assert_eq!(total_allocs(), 0);
        reset_gc();
    }

    // 観点42: partial collect で total_frees が回収個数分だけ増える
    #[test]
    fn test_gc_partial_collect_total_frees_count() {
        let _guard = gc_test_guard();
        reset_gc();
        for _ in 0..5 {
            willow_alloc_object(1, 8);
        }
        let survivor = willow_alloc_object(1, 8);
        let mut slot: *mut u8 = survivor;
        willow_push_root(&mut slot as *mut *mut u8);
        let before = total_frees();
        willow_gc_collect();
        assert_eq!(total_frees(), before + 5, "exactly 5 unrooted objects freed");
        willow_pop_root();
        reset_gc();
    }

    // =========================================================================
    // 観点43-46: エッジケース / 境界値
    // =========================================================================

    // 観点43: ペイロード 1 バイトの確保が成功する
    #[test]
    fn test_gc_alloc_payload_size_one() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(1, 1);
        assert!(!ptr.is_null());
        reset_gc();
    }

    // 観点44: 奇数ペイロードサイズでも正しく動く
    #[test]
    fn test_gc_alloc_odd_payload_sizes() {
        let _guard = gc_test_guard();
        reset_gc();
        for &size in &[1i64, 3, 5, 7, 9, 11] {
            let ptr = willow_alloc_object(1, size);
            assert!(!ptr.is_null(), "alloc of {size} bytes must succeed");
        }
        reset_gc();
    }

    // 観点45: 10,000個確保して全部回収される
    #[test]
    fn test_gc_ten_thousand_allocs_all_freed() {
        let _guard = gc_test_guard();
        reset_gc();
        set_threshold(usize::MAX); // 自動トリガー無効
        for _ in 0..10_000 {
            willow_alloc_object(1, 8);
        }
        assert!(willow_gc_allocated_bytes() > 0);
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // 観点46: 確保→回収サイクルを 20回繰り返した後にヒープが空
    #[test]
    fn test_gc_empty_heap_after_many_cycles() {
        let _guard = gc_test_guard();
        reset_gc();
        for _ in 0..20 {
            for _ in 0..10 {
                willow_alloc_object(1, 8);
            }
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0, "heap must be empty after each cycle");
        }
        reset_gc();
    }

    // =========================================================================
    // 観点47-48: payload_to_header ヘルパー
    // =========================================================================

    // 観点47: alloc から返ったポインタを payload_to_header に渡すと元のヘッダが返る
    #[test]
    fn test_gc_payload_to_header_roundtrip() {
        let _guard = gc_test_guard();
        reset_gc();
        let ptr = willow_alloc_object(7, 16);
        let hdr = payload_to_header(ptr);
        assert_eq!(unsafe { (*hdr).type_id }, 7);
        let expected_payload = unsafe { (hdr as *mut u8).add(header_size()) };
        assert_eq!(ptr, expected_payload, "payload pointer must be header + header_size");
        reset_gc();
    }

    // 観点48: 2個確保したとき、それぞれ payload_to_header が別のヘッダを返す
    #[test]
    fn test_gc_payload_to_header_two_objects_distinct() {
        let _guard = gc_test_guard();
        reset_gc();
        let p1 = willow_alloc_object(1, 8);
        let p2 = willow_alloc_object(2, 8);
        let h1 = payload_to_header(p1);
        let h2 = payload_to_header(p2);
        assert_ne!(h1, h2, "two allocations must have distinct headers");
        assert_eq!(unsafe { (*h1).type_id }, 1);
        assert_eq!(unsafe { (*h2).type_id }, 2);
        reset_gc();
    }

    // =========================================================================
    // 観点49: WILLOW_GC_LOG 環境変数があってもパニックしない
    // =========================================================================
    #[test]
    fn test_gc_log_env_var_does_not_panic() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_alloc_object(1, 8);
        // WILLOW_GC_LOG が設定されていても collect はクラッシュしない
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // 観点50: 複数スレッドからの同時 alloc でパニックしない (Mutex保護の確認)
    // =========================================================================
    #[test]
    fn test_gc_concurrent_alloc_no_panic() {
        let _guard = gc_test_guard();
        reset_gc();

        let handles: Vec<_> = (0..4)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..100 {
                        let _ptr = willow_alloc_object(1, 16);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().expect("concurrent alloc thread must not panic");
        }

        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // =========================================================================
    // TypeInfo / オブジェクトグラフ トレーステスト
    // =========================================================================
    //
    // テスト用 type_id 定数
    const TYPE_NODE: u32 = 200;  // payload = [child: *mut u8]              (8 bytes)
    const TYPE_NODE2: u32 = 201; // payload = [child0: *mut u8, child1: *mut u8] (16 bytes)
    const TYPE_LEAF: u32 = 202;  // 内部ポインタなし — TraceFn 未登録
    const TYPE_CLASS: u32 = 203; // payload = [i64_field: 8, gc_ptr: 8]    (16 bytes)
    const TYPE_ARRAY: u32 = 204; // payload = [len: i64, ptr0, ptr1, ...]
    const TYPE_MSG: u32 = 210;   // enum: [tag: i64, data: i64|*mut u8]    (16 bytes)

    // テスト用 trace 関数 (naked unsafe fn → TraceFn として使用)

    unsafe fn trace_node(payload: *mut u8, children: &mut Vec<*mut u8>) {
        // payload[0..8] = child pointer
        let child = unsafe { *(payload as *mut *mut u8) };
        children.push(child);
    }

    unsafe fn trace_node2(payload: *mut u8, children: &mut Vec<*mut u8>) {
        // payload[0..8] = child0, payload[8..16] = child1
        let c0 = unsafe { *(payload as *mut *mut u8) };
        let c1 = unsafe { *((payload as *mut *mut u8).add(1)) };
        children.push(c0);
        children.push(c1);
    }

    unsafe fn trace_class(payload: *mut u8, children: &mut Vec<*mut u8>) {
        // payload[0..8] = i64 field (not a pointer), payload[8..16] = gc_ptr
        let gc_ptr = unsafe { *((payload.add(8)) as *mut *mut u8) };
        children.push(gc_ptr);
    }

    unsafe fn trace_array(payload: *mut u8, children: &mut Vec<*mut u8>) {
        // payload[0..8] = len: i64, payload[8 + i*8] = ptr_i
        let len = unsafe { *(payload as *mut i64) } as usize;
        for i in 0..len {
            let elem = unsafe { *((payload.add(8 + i * 8)) as *mut *mut u8) };
            children.push(elem);
        }
    }

    unsafe fn trace_msg(payload: *mut u8, children: &mut Vec<*mut u8>) {
        // payload[0..8] = tag: i64
        // tag == 0 (Text)  → payload[8..16] is a GC pointer
        // tag == 1 (Number) → payload[8..16] is an i64, must NOT be traced
        let tag = unsafe { *(payload as *mut i64) };
        if tag == 0 {
            let ptr = unsafe { *((payload.add(8)) as *mut *mut u8) };
            children.push(ptr);
        }
    }

    // -------------------------------------------------------------------------
    // 観点T1: root → child が生き残る
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_root_child_survives() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE, trace_node);

        let child = willow_alloc_object(TYPE_LEAF as i64, 8);
        let parent = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe { *(parent as *mut *mut u8) = child; }

        let mut root_slot: *mut u8 = parent;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(8) * 2,
            "parent and child must both survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T2: root → child → grandchild が生き残る
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_root_child_grandchild_survives() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE, trace_node);

        let grandchild = willow_alloc_object(TYPE_LEAF as i64, 8);
        let child = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe { *(child as *mut *mut u8) = grandchild; }
        let parent = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe { *(parent as *mut *mut u8) = child; }

        let mut root_slot: *mut u8 = parent;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(8) * 3,
            "parent, child, and grandchild must all survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T3: root なしの cycle は回収される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_rootless_cycle_collected() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE, trace_node);

        let a = willow_alloc_object(TYPE_NODE as i64, 8);
        let b = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe {
            *(a as *mut *mut u8) = b; // A → B
            *(b as *mut *mut u8) = a; // B → A
        }

        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0, "rootless cycle must be collected");
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T4: root ありの cycle は生き残る
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_rooted_cycle_survives() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE, trace_node);

        let a = willow_alloc_object(TYPE_NODE as i64, 8);
        let b = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe {
            *(a as *mut *mut u8) = b;
            *(b as *mut *mut u8) = a;
        }

        let mut root_slot: *mut u8 = a;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(8) * 2,
            "rooted cycle must survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T5: class の GC フィールドが trace される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_class_field_traced() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_CLASS, trace_class);

        let field_obj = willow_alloc_object(TYPE_LEAF as i64, 8);
        // payload: [i64_field: 8, gc_ptr: 8] = 16 bytes
        let instance = willow_alloc_object(TYPE_CLASS as i64, 16);
        unsafe {
            *(instance as *mut i64) = 42i64;
            *((instance.add(8)) as *mut *mut u8) = field_obj;
        }

        let mut root_slot: *mut u8 = instance;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(16) + obj_size(8),
            "class instance and its GC field must both survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T6: 2子 (TYPE_NODE2) が両方 trace される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_two_children_traced() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE2, trace_node2);

        let child0 = willow_alloc_object(TYPE_LEAF as i64, 8);
        let child1 = willow_alloc_object(TYPE_LEAF as i64, 8);
        // payload: [child0_ptr: 8, child1_ptr: 8] = 16 bytes
        let parent = willow_alloc_object(TYPE_NODE2 as i64, 16);
        unsafe {
            *(parent as *mut *mut u8) = child0;
            *((parent as *mut *mut u8).add(1)) = child1;
        }

        let mut root_slot: *mut u8 = parent;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(16) + obj_size(8) * 2,
            "parent and both children must survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T7: array の全要素が trace される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_array_elements_traced() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_ARRAY, trace_array);

        const N: usize = 4;
        let elems: Vec<*mut u8> = (0..N)
            .map(|_| willow_alloc_object(TYPE_LEAF as i64, 8))
            .collect();

        // payload: [len: i64, ptr0, ptr1, ptr2, ptr3] = 8 + 4*8 = 40 bytes
        let array_payload: usize = 8 + N * 8;
        let array = willow_alloc_object(TYPE_ARRAY as i64, array_payload as i64);
        unsafe {
            *(array as *mut i64) = N as i64;
            for (i, &ep) in elems.iter().enumerate() {
                *((array.add(8 + i * 8)) as *mut *mut u8) = ep;
            }
        }

        let mut root_slot: *mut u8 = array;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(array_payload) + obj_size(8) * (N as i64),
            "array and all elements must survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T8: enum Text バリアント — 内部 GC ポインタが trace される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_enum_text_variant_traced() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_MSG, trace_msg);

        let text_obj = willow_alloc_object(TYPE_LEAF as i64, 8);
        // payload: [tag=0: i64, ptr: *mut u8] = 16 bytes
        let msg = willow_alloc_object(TYPE_MSG as i64, 16);
        unsafe {
            *(msg as *mut i64) = 0i64; // tag = Text
            *((msg.add(8)) as *mut *mut u8) = text_obj;
        }

        let mut root_slot: *mut u8 = msg;
        willow_push_root(&mut root_slot as *mut *mut u8);

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(16) + obj_size(8),
            "Message::Text and its string payload must both survive"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T9: enum Number バリアント — i64 フィールドをポインタとして trace しない
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_enum_number_variant_not_traced() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_MSG, trace_msg);

        // payload: [tag=1: i64, data=12345: i64] = 16 bytes
        let msg = willow_alloc_object(TYPE_MSG as i64, 16);
        unsafe {
            *(msg as *mut i64) = 1i64;            // tag = Number
            *((msg.add(8)) as *mut i64) = 12345i64; // numeric data, NOT a pointer
        }

        let mut root_slot: *mut u8 = msg;
        willow_push_root(&mut root_slot as *mut *mut u8);

        // GC must not crash treating 12345 as a pointer
        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            obj_size(16),
            "only msg survives; no child was traced"
        );

        willow_pop_root();
        willow_gc_collect();
        assert_eq!(willow_gc_allocated_bytes(), 0);
        reset_gc();
    }

    // -------------------------------------------------------------------------
    // 観点T10: root なしの parent+child は両方回収される
    // -------------------------------------------------------------------------
    #[test]
    fn test_gc_typeinfo_unrooted_parent_child_both_collected() {
        let _guard = gc_test_guard();
        reset_gc();
        willow_register_type(TYPE_NODE, trace_node);

        let child = willow_alloc_object(TYPE_LEAF as i64, 8);
        let parent = willow_alloc_object(TYPE_NODE as i64, 8);
        unsafe { *(parent as *mut *mut u8) = child; }

        willow_gc_collect();
        assert_eq!(
            willow_gc_allocated_bytes(),
            0,
            "unrooted parent and child must both be collected"
        );
        reset_gc();
    }
}
