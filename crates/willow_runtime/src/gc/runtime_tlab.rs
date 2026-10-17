use super::*;

// ---------------------------------------------------------------------------
// Runtime-owned TLAB for string and array results (willow-8hq4.29, jz15.94)
// ---------------------------------------------------------------------------
//
// Runtime allocations normally go to the old generation under the heap lock.
// String results of operations called directly from generated code
// (concatenation, `i64.toString`), array literals and grown push buffers are
// short-lived and frequent, so they
// bump-allocate young objects from a per-thread TLS block exactly as the
// generated fast path does: same header bytes, same start-bit publication,
// same slow path. Only threads that run generated code may use one: the
// thread inside `runtime_start` and registered mutators. Either way the block
// retires itself before its thread-local storage goes away.

/// The runtime's TLS allocation block. Its destructor forgets the block's
/// registration so no TLAB record outlives the thread.
struct RuntimeTlab(GcTlabState);

impl RuntimeTlab {
    fn address(&self) -> usize {
        &self.0 as *const GcTlabState as usize
    }
}

impl Drop for RuntimeTlab {
    fn drop(&mut self) {
        unregister_tlab_state(self.address());
    }
}

std::thread_local! {
    static RUNTIME_TLAB: RuntimeTlab = const { RuntimeTlab(GcTlabState::new()) };
    /// Set while `runtime_start` runs synchronous top-level code on an
    /// unregistered thread.
    static RUNTIME_TLAB_OWNER: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Acquire the current worker's allocation block. The generated execution
/// context refreshes this pointer at native-stack resume and runtime entry.
/// Allocation sites load it through that stable context, never cached TLS.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub extern "C" fn willow_gc_current_tlab() -> *mut GcTlabState {
    RUNTIME_TLAB.with(|tls| tls.address() as *mut GcTlabState)
}

/// Largest object with a static size-indexed descriptor; larger ones are not
/// TLAB allocations anyway.
const STATIC_DESCRIPTOR_SLOTS: usize =
    GC_TLAB_MAX_OBJECT_SIZE / std::mem::align_of::<GcHeader>() + 1;

type DescriptorTable = [willow_abi::GcLayoutDescriptor; STATIC_DESCRIPTOR_SLOTS];

/// Static descriptors of one object shape indexed by total object size / 8.
/// Like generated descriptors they are never released, so headers point at
/// them unowned.
const fn descriptor_table(kind: GcObjectKind, type_id: u32) -> DescriptorTable {
    let mut table = [willow_abi::GcLayoutDescriptor {
        type_id: 0,
        layout_id: 0,
        gc_ref_mask: 0,
        size: 0,
    }; STATIC_DESCRIPTOR_SLOTS];
    let mut slot = GC_HEADER_SIZE / 8;
    while slot < STATIC_DESCRIPTOR_SLOTS {
        let total = slot * 8;
        table[slot] = willow_abi::GcLayoutDescriptor {
            type_id: type_id as u64,
            layout_id: willow_abi::gc_layout_id(
                kind,
                (total - GC_HEADER_SIZE) as i64,
                type_id as i64,
                0,
            ),
            gc_ref_mask: 0,
            size: total as u64,
        };
        slot += 1;
    }
    table
}

static STRING_DESCRIPTORS: DescriptorTable = descriptor_table(GcObjectKind::String, 0);

/// Array buffers (willow-jz15.94): scalar elements, and reference elements
/// traced by the registered `ARRAY_REF_TYPE_ID` hook.
static ARRAY_BUFFER_DESCRIPTORS: DescriptorTable = descriptor_table(GcObjectKind::ArrayBuffer, 0);
static REF_ARRAY_BUFFER_DESCRIPTORS: DescriptorTable = descriptor_table(
    GcObjectKind::ArrayBuffer,
    willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID,
);

const ARRAY_HANDLE_PAYLOAD: i64 =
    willow_abi::array_layout::HANDLE_WORDS * willow_abi::array_layout::WORD_BYTES as i64;

static ARRAY_HANDLE_DESCRIPTOR: willow_abi::GcLayoutDescriptor = willow_abi::GcLayoutDescriptor {
    type_id: 0,
    layout_id: willow_abi::gc_layout_id(
        GcObjectKind::ArrayHandle,
        ARRAY_HANDLE_PAYLOAD,
        0,
        willow_abi::array_layout::HANDLE_MASK,
    ),
    gc_ref_mask: willow_abi::array_layout::HANDLE_MASK,
    size: GC_HEADER_SIZE as u64 + ARRAY_HANDLE_PAYLOAD as u64,
};

/// Shape of a runtime object a TLAB may hold: its static descriptor plus the
/// object kind the slow path needs to rebuild the same layout id.
#[derive(Clone, Copy)]
pub(crate) struct YoungLayout {
    descriptor: &'static willow_abi::GcLayoutDescriptor,
    kind: GcObjectKind,
}

pub(crate) fn array_handle_layout() -> YoungLayout {
    YoungLayout {
        descriptor: &ARRAY_HANDLE_DESCRIPTOR,
        kind: GcObjectKind::ArrayHandle,
    }
}

/// Descriptor of an array buffer with `payload_size` bytes, or `None` when
/// the object is too large for a TLAB. Reference buffers require the array
/// trace hook to be registered before allocation.
pub(crate) fn array_buffer_layout(payload_size: usize, is_ref: bool) -> Option<YoungLayout> {
    let total = object_size(payload_size)?;
    if total > GC_TLAB_MAX_OBJECT_SIZE {
        return None;
    }
    let table = if is_ref {
        &REF_ARRAY_BUFFER_DESCRIPTORS
    } else {
        &ARRAY_BUFFER_DESCRIPTORS
    };
    Some(YoungLayout {
        descriptor: &table[total / 8],
        kind: GcObjectKind::ArrayBuffer,
    })
}

/// Lets the synchronous top-level thread use the runtime TLAB while it is not
/// a registered mutator. Dropping it retires that TLAB.
#[must_use]
pub(crate) struct RuntimeTlabOwner {
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl RuntimeTlabOwner {
    pub(crate) fn enter() -> Self {
        RUNTIME_TLAB_OWNER.set(true);
        Self {
            _thread: std::marker::PhantomData,
        }
    }
}

impl Drop for RuntimeTlabOwner {
    fn drop(&mut self) {
        RUNTIME_TLAB_OWNER.set(false);
        // A registered thread could keep it, but the next use simply
        // re-registers the block on its slow path.
        let _ = RUNTIME_TLAB.try_with(|tls| unregister_tlab_state(tls.address()));
    }
}

fn runtime_tlab_enabled() -> bool {
    RUNTIME_TLAB_OWNER.get() || current_thread_is_registered()
}

/// Total object size of a payload, rounded to header alignment.
fn object_size(payload_size: usize) -> Option<usize> {
    GC_HEADER_SIZE
        .checked_add(payload_size)?
        .checked_next_multiple_of(std::mem::align_of::<GcHeader>())
}

/// Bump-allocate a young String payload of at least `payload_size` bytes from
/// this thread's runtime TLAB. Never reaches a GC point. `None` on a TLAB miss,
/// for objects too large for a TLAB, or on a thread that may not own one.
/// The payload is zeroed: TLAB chunks are fresh zeroed storage.
pub(crate) fn try_alloc_young_string_payload(payload_size: usize) -> Option<*mut u8> {
    let total = object_size(payload_size)?;
    if total > GC_TLAB_MAX_OBJECT_SIZE {
        return None;
    }
    try_alloc_young([YoungLayout {
        descriptor: &STRING_DESCRIPTORS[total / 8],
        kind: GcObjectKind::String,
    }])
    .map(|[payload]| payload)
}

/// Bump-allocate consecutive young objects with one TLAB bound check. Never
/// reaches a GC point; `None` on a TLAB miss or on a thread that may not own
/// a runtime TLAB. Payloads are zeroed: TLAB chunks are fresh zeroed storage.
pub(crate) fn try_alloc_young<const N: usize>(layouts: [YoungLayout; N]) -> Option<[*mut u8; N]> {
    if !runtime_tlab_enabled() {
        return None;
    }
    RUNTIME_TLAB.with(|RuntimeTlab(tls)| {
        let start = tls.cursor.load(Ordering::Relaxed);
        let limit = tls.limit.load(Ordering::Relaxed);
        let mut end = start;
        for layout in layouts {
            end = end.checked_add(layout.descriptor.size as usize)?;
        }
        if start == 0 || end > limit {
            return None;
        }
        let base = limit - GC_TLAB_CHUNK_SIZE;
        let words = tls.start_bits.load(Ordering::Relaxed) as *const AtomicU64;
        let mut cursor = start;
        let mut payloads = [std::ptr::null_mut(); N];
        for (layout, payload) in layouts.into_iter().zip(&mut payloads) {
            let header = cursor as *mut GcHeader;
            // SAFETY: [start, end) lies in this thread's active chunk, which
            // only this thread writes until a collection retires it, and a
            // collection cannot run before the cursor store below.
            unsafe {
                header.write(GcHeader {
                    marked: false,
                    allocated: true,
                    generation: GC_GENERATION_YOUNG,
                    age: 0,
                    descriptor_owned: false,
                    remembered: 0,
                    descriptor: layout.descriptor as *const willow_abi::GcLayoutDescriptor as usize,
                });
            }
            // Publish the start bit after the header, as generated code does.
            // The owner is the only writer of the active chunk's start words.
            let granule = (cursor - base) / GC_REGION_MARK_GRANULE;
            // SAFETY: the active chunk's start words cover every granule below
            // its limit and stay alive while the chunk is active.
            let word = unsafe { &*words.add(granule / 64) };
            word.store(
                word.load(Ordering::Relaxed) | (1u64 << (granule % 64)),
                Ordering::Release,
            );
            *payload = (cursor + GC_HEADER_SIZE) as *mut u8;
            cursor += layout.descriptor.size as usize;
        }
        tls.cursor.store(end, Ordering::Release);
        Some(payloads)
    })
}

/// Allocation miss path for [`try_alloc_young_string_payload`]. May collect.
///
/// Only runtime entry points called directly by generated code may use it:
/// on a TLAB-owning thread a minor collection here may move this thread's
/// root-slot referents, so the caller must keep its own managed pointers in
/// pushed root slots and reload them afterwards. Other threads allocate old.
pub(crate) fn alloc_string_payload_slow(payload_size: usize) -> *mut u8 {
    let Some(total) = object_size(payload_size) else {
        crate::failure::resource_exhausted(format_args!("string allocation size overflow"));
    };
    let payload = (total - GC_HEADER_SIZE) as i64;
    alloc_young_slow(GcObjectKind::String, 0, payload, 0)
}

/// Allocation miss path for [`try_alloc_young`] of one object, with the same
/// contract as [`alloc_string_payload_slow`]. May collect.
pub(crate) fn alloc_young_layout_slow(layout: YoungLayout) -> *mut u8 {
    let descriptor = layout.descriptor;
    alloc_young_slow(
        layout.kind,
        descriptor.type_id as u32,
        (descriptor.size as usize - GC_HEADER_SIZE) as i64,
        descriptor.gc_ref_mask,
    )
}

fn alloc_young_slow(kind: GcObjectKind, type_id: u32, payload: i64, gc_ref_mask: u64) -> *mut u8 {
    if !runtime_tlab_enabled() {
        return willow_alloc_with_layout(kind, type_id, payload, gc_ref_mask);
    }
    let _relocation = RelocationSafeScope::enter();
    let layout_id = gc_layout_id(kind, payload, type_id as i64, gc_ref_mask);
    RUNTIME_TLAB.with(|tls| {
        tlab_alloc_slow(
            tls.address(),
            layout_id,
            type_id as i64,
            payload,
            gc_ref_mask,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_string_descriptors_match_runtime_layout_ids() {
        for payload in [
            9usize,
            10,
            16,
            17,
            100,
            GC_TLAB_MAX_OBJECT_SIZE - GC_HEADER_SIZE,
        ] {
            let total = object_size(payload).unwrap();
            let descriptor = STRING_DESCRIPTORS[total / 8];
            assert_eq!(descriptor.size, total as u64);
            assert_eq!(
                descriptor.layout_id,
                gc_layout_id(GcObjectKind::String, (total - GC_HEADER_SIZE) as i64, 0, 0)
            );
            assert_eq!(descriptor.gc_ref_mask, 0);
        }
    }

    #[test]
    fn unregistered_threads_never_use_the_runtime_tlab() {
        let _guard = runtime_test_guard();
        reset_internal();
        assert!(!runtime_tlab_enabled());
        assert_eq!(try_alloc_young_string_payload(9), None);
        let payload = alloc_string_payload_slow(9);
        assert!(!payload.is_null());
        assert_eq!(
            tlab_payload_generation(&runtime().heap.lock().unwrap(), payload as usize),
            None,
            "an unregistered thread allocates old"
        );
    }

    #[test]
    fn owner_allocates_young_strings_and_retires_on_drop() {
        let _guard = runtime_test_guard();
        reset_internal();
        let fast_before = willow_gc_tlab_fast_allocations();
        {
            let _owner = RuntimeTlabOwner::enter();
            // A fresh TLS block misses first; the slow path refills it.
            assert_eq!(try_alloc_young_string_payload(9), None);
            let first = alloc_string_payload_slow(9);
            assert!(!first.is_null());
            let mut previous = first as usize;
            for payload in [9usize, 10, 17, 255] {
                let next = try_alloc_young_string_payload(payload).expect("TLAB hit");
                assert!(next as usize > previous);
                // Zeroed payload: the length word reads as an empty string.
                assert_eq!(unsafe { *(next as *const i64) }, 0);
                assert_eq!(
                    tlab_payload_generation(&runtime().heap.lock().unwrap(), next as usize),
                    Some(GC_GENERATION_YOUNG)
                );
                previous = next as usize;
            }
            // Too large for a TLAB object: never bump-allocated.
            assert_eq!(
                try_alloc_young_string_payload(GC_TLAB_MAX_OBJECT_SIZE),
                None
            );
        }
        assert!(!runtime_tlab_enabled());
        RUNTIME_TLAB.with(|RuntimeTlab(tls)| {
            assert_eq!(tls.cursor.load(Ordering::Acquire), 0, "retired on drop");
            assert_eq!(tls.limit.load(Ordering::Acquire), 0);
        });
        assert!(
            !runtime()
                .heap
                .lock()
                .unwrap()
                .tlab_owners
                .contains_key(&std::thread::current().id())
        );
        assert_eq!(willow_gc_tlab_fast_allocations() - fast_before, 4);
        reset_internal();
    }

    /// Restores the environment's stress modes when a test ends.
    struct StressModes;

    impl StressModes {
        fn set(modes: &str) -> Self {
            set_gc_stress_for_test(Some(modes));
            Self
        }
    }

    impl Drop for StressModes {
        fn drop(&mut self) {
            set_gc_stress_for_test(None);
        }
    }

    fn concat(parts: &mut [u64]) -> *mut u8 {
        crate::string::willow_string_concat_n(parts.as_mut_ptr(), (parts.len() / 2) as i64)
    }

    fn text(ptr: *mut u8) -> String {
        unsafe { crate::string::willow_string_as_str(ptr) }.to_owned()
    }

    #[test]
    fn concatenation_reloads_parts_moved_by_its_own_collection() {
        use willow_abi::string_concat::{PART_I64, PART_STRING};
        let _guard = runtime_test_guard();
        reset_internal();
        let _owner = RuntimeTlabOwner::enter();
        // One object per TLAB: every runtime string allocation collects.
        let _stress = StressModes::set("relocate");
        let accent = crate::string::willow_string_from_str("é水");
        let mut slots = [std::ptr::null_mut::<u8>(); 3];
        slots[0] = accent;
        willow_push_root((&mut slots[0] as *mut *mut u8).cast());
        slots[1] = concat(&mut [PART_I64, 12345]);
        willow_push_root((&mut slots[1] as *mut *mut u8).cast());
        slots[2] = concat(&mut [PART_STRING, slots[0] as u64, PART_I64, (-7i64) as u64]);
        willow_push_root((&mut slots[2] as *mut *mut u8).cast());
        assert_eq!(
            tlab_payload_generation(&runtime().heap.lock().unwrap(), slots[2] as usize),
            Some(GC_GENERATION_YOUNG)
        );
        let young = [slots[1] as u64, slots[2] as u64];
        let minor_before = runtime().heap.lock().unwrap().minor_collections;
        let mut parts = [
            PART_STRING,
            young[0],
            PART_STRING,
            0, // null string reads as empty
            PART_I64,
            i64::MIN as u64,
            PART_STRING,
            young[1],
        ];
        let joined = concat(&mut parts);
        assert!(runtime().heap.lock().unwrap().minor_collections > minor_before);
        assert_ne!(
            [parts[1], parts[7]],
            young,
            "the collection moved the young parts and rewrote their slots"
        );
        assert_eq!(text(joined), format!("12345{}é水-7", i64::MIN));
        willow_pop_roots(3);
        reset_internal();
    }

    use crate::array::{
        new_old_array, willow_array_get, willow_array_len, willow_array_new, willow_array_push,
        willow_array_set,
    };
    use willow_abi::array_layout::{H_BUF, H_CAP, HANDLE_MASK};

    fn generation(ptr: *mut u8) -> Option<u8> {
        tlab_payload_generation(&runtime().heap.lock().unwrap(), ptr as usize)
    }

    fn handle_word(arr: *mut u8, word: usize) -> i64 {
        unsafe { *(arr as *const i64).add(word) }
    }

    fn buffer(arr: *mut u8) -> *mut u8 {
        handle_word(arr, H_BUF) as *mut u8
    }

    /// Prime the thread's runtime TLAB so the next allocations are fast hits.
    fn prime_runtime_tlab() {
        assert!(!alloc_young_layout_slow(array_handle_layout()).is_null());
    }

    #[test]
    fn static_array_descriptors_match_runtime_layout_ids() {
        let handle = array_handle_layout().descriptor;
        assert_eq!(
            handle.layout_id,
            gc_layout_id(GcObjectKind::ArrayHandle, 32, 0, HANDLE_MASK)
        );
        assert_eq!(handle.gc_ref_mask, HANDLE_MASK);
        assert_eq!(handle.size, (GC_HEADER_SIZE + 32) as u64);
        let ref_type = willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID;
        for payload in [
            8usize,
            16,
            32,
            100,
            GC_TLAB_MAX_OBJECT_SIZE - GC_HEADER_SIZE,
        ] {
            let total = object_size(payload).unwrap();
            for (is_ref, type_id) in [(false, 0), (true, ref_type)] {
                let layout = array_buffer_layout(payload, is_ref).unwrap();
                let descriptor = layout.descriptor;
                assert_eq!(descriptor.size, total as u64);
                assert_eq!(descriptor.type_id, type_id as u64);
                assert_eq!(descriptor.gc_ref_mask, 0);
                assert_eq!(
                    descriptor.layout_id,
                    gc_layout_id(
                        GcObjectKind::ArrayBuffer,
                        (total - GC_HEADER_SIZE) as i64,
                        type_id as i64,
                        0
                    )
                );
            }
        }
        // Larger buffers are not TLAB objects.
        assert!(array_buffer_layout(GC_TLAB_MAX_OBJECT_SIZE - GC_HEADER_SIZE + 1, false).is_none());
        assert!(array_buffer_layout(usize::MAX, true).is_none());
    }

    #[test]
    fn unregistered_threads_allocate_array_literals_old() {
        let _guard = runtime_test_guard();
        reset_internal();
        let fast_before = willow_gc_tlab_fast_allocations();
        let arr = willow_array_new(3, 0);
        assert_eq!(willow_array_len(arr), 3);
        assert_eq!(
            generation(arr),
            None,
            "an unregistered thread allocates old"
        );
        assert_eq!(generation(buffer(arr)), None);
        assert_eq!(willow_gc_tlab_fast_allocations(), fast_before);
        reset_internal();
    }

    #[test]
    fn owner_bump_allocates_array_literal_handle_and_buffer_together() {
        let _guard = runtime_test_guard();
        reset_internal();
        let _owner = RuntimeTlabOwner::enter();
        prime_runtime_tlab();
        let fast_before = willow_gc_tlab_fast_allocations();
        for (len, is_ref) in [(3i64, 0i64), (0, 0), (2, 1), (64, 0)] {
            let arr = willow_array_new(len, is_ref);
            let buf = buffer(arr);
            assert_eq!(generation(arr), Some(GC_GENERATION_YOUNG));
            assert_eq!(generation(buf), Some(GC_GENERATION_YOUNG));
            // One bump: the buffer follows the handle object directly.
            assert_eq!(buf as usize, arr as usize + 32 + GC_HEADER_SIZE);
            assert_eq!(willow_array_len(arr), len);
            assert_eq!(handle_word(arr, H_CAP), len);
            for i in 0..len {
                assert_eq!(willow_array_get(arr, i), 0, "zeroed elements");
            }
        }
        assert_eq!(willow_gc_tlab_fast_allocations() - fast_before, 8);
        // A buffer too large for a TLAB takes the old path.
        let big = willow_array_new(1000, 0);
        assert_eq!(willow_array_len(big), 1000);
        assert_eq!(generation(big), None);
        assert_eq!(generation(buffer(big)), None);
        // Runtime-internal callers always allocate old.
        let internal = new_old_array(2, 1);
        assert_eq!(generation(internal), None);
        assert_eq!(generation(buffer(internal)), None);
        reset_internal();
    }

    #[test]
    fn array_literal_miss_path_rewrites_rooted_caller_values() {
        let _guard = runtime_test_guard();
        reset_internal();
        let _owner = RuntimeTlabOwner::enter();
        // One object per TLAB: every literal reaches the collecting miss path.
        let _stress = StressModes::set("relocate");
        let mut outer = willow_array_new(2, 1);
        willow_push_root(&mut outer);
        let mut inner = willow_array_new(1, 0);
        willow_push_root(&mut inner);
        willow_array_set(inner, 0, 99);
        willow_array_set(outer, 0, inner as i64);
        let before = [outer, inner];
        let minor_before = runtime().heap.lock().unwrap().minor_collections;
        let fresh = willow_array_new(4, 1);
        assert!(runtime().heap.lock().unwrap().minor_collections > minor_before);
        assert_ne!(
            [outer, inner],
            before,
            "the literal's collection moved its roots"
        );
        assert_eq!(willow_array_len(fresh), 4);
        assert_eq!(willow_array_get(fresh, 3), 0);
        let element = willow_array_get(outer, 0) as *mut u8;
        assert_eq!(element, inner, "the moved element edge follows the copy");
        assert_eq!(willow_array_get(element, 0), 99);
        assert_eq!(willow_array_get(outer, 1), 0);
        willow_pop_roots(2);
        reset_internal();
    }

    #[test]
    fn push_growth_gives_old_handles_young_buffers_kept_by_the_barrier() {
        let _guard = runtime_test_guard();
        reset_internal();
        let _owner = RuntimeTlabOwner::enter();
        let mut arr = new_old_array(0, 1);
        willow_push_root(&mut arr);
        let old_handle = arr;
        prime_runtime_tlab();
        for i in 0..20 {
            let element = willow_array_new(1, 0);
            willow_array_set(element, 0, i);
            willow_array_push(arr, element as i64);
        }
        assert_eq!(arr, old_handle, "no collection ran");
        assert_eq!(generation(arr), None);
        let young_buffer = buffer(arr);
        assert_eq!(generation(young_buffer), Some(GC_GENERATION_YOUNG));
        // Only the old handle's barrier-recorded edge keeps the young buffer
        // and its young elements alive across this collection.
        willow_gc_minor_collect();
        assert_eq!(arr, old_handle, "old handles never move");
        assert_ne!(buffer(arr), young_buffer, "the young buffer was evacuated");
        assert_eq!(willow_array_len(arr), 20);
        for i in 0..20 {
            assert_eq!(willow_array_get(willow_array_get(arr, i) as *mut u8, 0), i);
        }
        willow_pop_roots(1);
        reset_internal();
    }

    #[test]
    fn push_growth_under_relocation_keeps_every_element() {
        let _guard = runtime_test_guard();
        reset_internal();
        let _owner = RuntimeTlabOwner::enter();
        let _stress = StressModes::set("relocate");
        let mut arr = willow_array_new(0, 0);
        willow_push_root(&mut arr);
        for i in 0..300 {
            willow_array_push(arr, i * 3);
        }
        assert_eq!(willow_array_len(arr), 300);
        for i in 0..300 {
            assert_eq!(willow_array_get(arr, i), i * 3);
        }
        willow_pop_roots(1);
        reset_internal();
    }
}
