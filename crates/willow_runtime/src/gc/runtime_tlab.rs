use super::*;

// ---------------------------------------------------------------------------
// Runtime-owned TLAB for string and array results
// ---------------------------------------------------------------------------
//
// Runtime allocations normally go to the old generation under the heap lock.
// String results of operations called directly from generated code
// (concatenation, `i64.to_string`), array literals and grown push buffers are
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

/// Array buffers: scalar elements, and reference elements
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

/// Two-word enum objects (type id 0) indexed by inline reference mask:
/// array pair copies and runtime-built `tag + one word` enum variants.
static ENUM_PAIR_DESCRIPTORS: [willow_abi::GcLayoutDescriptor; 4] = {
    let mut table = [willow_abi::GcLayoutDescriptor {
        type_id: 0,
        layout_id: 0,
        gc_ref_mask: 0,
        size: GC_HEADER_SIZE as u64 + 16,
    }; 4];
    let mut mask = 0;
    while mask < 4 {
        table[mask].layout_id = willow_abi::gc_layout_id(GcObjectKind::Enum, 16, 0, mask as u64);
        table[mask].gc_ref_mask = mask as u64;
        mask += 1;
    }
    table
};

/// Pair copies keep the same enum layout and independent storage. Only the
/// first word can be a managed reference; the second is scalar/vtable data.
pub(crate) fn array_pair_layout(mask: u64) -> Option<YoungLayout> {
    (mask < 2).then_some(())?;
    enum_pair_layout(mask)
}

/// Two-word type-id-0 enum object with inline reference `mask` (bits 0..2).
pub(crate) fn enum_pair_layout(mask: u64) -> Option<YoungLayout> {
    Some(YoungLayout {
        descriptor: ENUM_PAIR_DESCRIPTORS.get(usize::try_from(mask).ok()?)?,
        kind: GcObjectKind::Enum,
    })
}

pub(crate) fn array_handle_layout() -> YoungLayout {
    YoungLayout {
        descriptor: &ARRAY_HANDLE_DESCRIPTOR,
        kind: GcObjectKind::ArrayHandle,
    }
}

/// Build a buffer descriptor from its capacity and storage flags. The shared
/// array layout includes the traced prefix even when capacity is zero.
pub(crate) fn array_buffer_layout(cap: i64, flags: i64) -> Option<YoungLayout> {
    let payload = usize::try_from(willow_abi::array_layout::buffer_bytes(cap, flags)?).ok()?;
    let is_ref = willow_abi::array_layout::is_reference(flags);
    if is_ref {
        crate::array::ensure_trace_registered();
    }
    let total = object_size(payload)?;
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
                    shared: 0,
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
    // A TLAB-sized object uses the aligned payload of its fast-path
    // descriptor. Larger objects go to the old heap, which keeps the exact
    // payload size, matching old allocation from threads without a TLAB.
    let payload = if total <= GC_TLAB_MAX_OBJECT_SIZE {
        total - GC_HEADER_SIZE
    } else {
        payload_size
    };
    // SAFETY: The requested young object uses the size, type and pointer map selected by the
    // enclosing constructor.
    unsafe { alloc_young_slow(GcObjectKind::String, 0, payload as i64, 0) }
}

/// Allocation miss path for [`try_alloc_young`] of one object, with the same
/// contract as [`alloc_string_payload_slow`]. May collect.
pub(crate) fn alloc_young_layout_slow(layout: YoungLayout) -> *mut u8 {
    let descriptor = layout.descriptor;
    // SAFETY: The requested young object uses the size, type and pointer map selected by the
    // enclosing constructor.
    unsafe {
        alloc_young_slow(
            layout.kind,
            descriptor.type_id as u32,
            (descriptor.size as usize - GC_HEADER_SIZE) as i64,
            descriptor.gc_ref_mask,
        )
    }
}

/// # Safety
/// The type, tracing metadata and payload size must satisfy the allocation contract of
/// willow_gc_alloc_layout, including reserved bitmap descriptors and native initialization.
unsafe fn alloc_young_slow(
    kind: GcObjectKind,
    type_id: u32,
    payload: i64,
    gc_ref_mask: u64,
) -> *mut u8 {
    if !runtime_tlab_enabled() {
        // SAFETY: The requested layout describes the initialized words of this object and its
        // type’s tracing or finalization contract.
        return unsafe { willow_alloc_with_layout(kind, type_id, payload, gc_ref_mask) };
    }
    let _relocation = RelocationSafeScope::enter();
    let layout_id = gc_layout_id(kind, payload, type_id as i64, gc_ref_mask);
    RUNTIME_TLAB.with(|tls| {
        // SAFETY: The runtime owns this thread-local block until unregistration, and its
        // layout table selects the matching payload and mask.
        unsafe {
            tlab_alloc_slow(
                tls.address(),
                layout_id,
                type_id as i64,
                payload,
                gc_ref_mask,
            )
        }
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
    fn array_literal_inline_trace_is_ready_after_every_gc_init() {
        let _guard = isolated_runtime();
        for _ in 0..2 {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { willow_gc_init() };
            let _owner = RuntimeTlabOwner::enter();
            prime_runtime_tlab();
            // Model generated allocation: no array runtime entry registers a hook.
            let [mut handle, buffer] = try_alloc_young([
                array_handle_layout(),
                YoungLayout {
                    descriptor: &REF_ARRAY_BUFFER_DESCRIPTORS
                        [(GC_HEADER_SIZE + 16) / GC_STORAGE_WORD_BYTES],
                    kind: GcObjectKind::ArrayBuffer,
                },
            ])
            .unwrap();
            let [child] = try_alloc_young([array_pair_layout(0).unwrap()]).unwrap();
            // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the
            // accessed words.
            unsafe {
                child.cast::<i64>().write(73);
                handle.cast::<i64>().write(1);
                handle.cast::<i64>().add(1).write(1);
                handle.cast::<i64>().add(2).write(1);
                handle.cast::<*mut u8>().add(3).write(buffer);
                buffer.cast::<i64>().write(1);
                buffer.cast::<*mut u8>().add(1).write(child);
            }
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut handle) };
            let old = handle;
            willow_gc_minor_collect();
            assert_ne!(handle, old);
            // SAFETY: The fixture retains the array allocated for these accesses.
            let moved_child = unsafe { willow_array_get(handle, 0) } as *mut i64;
            assert_ne!(moved_child.cast::<u8>(), child);
            // SAFETY: The fixture allocates these fields in word-aligned GC storage at whole-word
            // offsets.
            assert_eq!(unsafe { moved_child.read() }, 73);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
    }

    #[test]
    fn unregistered_threads_never_use_the_runtime_tlab() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
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
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
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
                // SAFETY: This fresh word-aligned string payload has a zeroed length word and
                // terminator.
                assert_eq!(unsafe { crate::string::willow_string_len(next) }, 0);
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
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
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

    /// String words must be current addresses in this runtime or null.
    unsafe fn concat(parts: &mut [u64]) -> *mut u8 {
        // SAFETY: The exclusive slice covers the pairs; the fixture supplies current pointers for
        // string parts.
        unsafe {
            crate::string::willow_string_concat_n(parts.as_mut_ptr(), (parts.len() / 2) as i64)
        }
    }

    fn text(ptr: *mut u8) -> String {
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        unsafe { crate::string::willow_string_as_str(ptr) }.to_owned()
    }

    #[test]
    fn concatenation_reloads_parts_moved_by_its_own_collection() {
        use willow_abi::string_concat::{PART_I64, PART_STRING};
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        let _owner = RuntimeTlabOwner::enter();
        // One object per TLAB: every runtime string allocation collects.
        let _stress = StressModes::set("relocate");
        let accent = crate::string::willow_string_from_str("é水");
        let slots = [const { std::cell::Cell::new(std::ptr::null_mut::<u8>()) }; 3];
        slots[0].set(accent);
        // SAFETY: The initialized cell provides a stable writable slot through the matching
        // pop.
        unsafe { willow_push_root(slots[0].as_ptr()) };
        // SAFETY: Only integers or an empty list are passed.
        slots[1].set(unsafe { concat(&mut [PART_I64, 12345]) });
        // SAFETY: The initialized cell provides a stable writable slot through the matching
        // pop.
        unsafe { willow_push_root(slots[1].as_ptr()) };
        // SAFETY: The string words come from the current rooted slots or fresh strings, with no
        // intervening GC point.
        slots[2].set(unsafe {
            concat(&mut [PART_STRING, slots[0].get() as u64, PART_I64, (-7i64) as u64])
        });
        // SAFETY: The initialized cell provides a stable writable slot through the matching
        // pop.
        unsafe { willow_push_root(slots[2].as_ptr()) };
        assert_eq!(
            tlab_payload_generation(&runtime().heap.lock().unwrap(), slots[2].get() as usize),
            Some(GC_GENERATION_YOUNG)
        );
        let young = [slots[1].get() as u64, slots[2].get() as u64];
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
        let caller_slots = ROOT_STACK.with(|stack| stack.borrow().to_vec());
        // SAFETY: The string words come from the current rooted slots or fresh strings, with no
        // intervening GC point.
        let joined = unsafe { concat(&mut parts) };
        ROOT_STACK.with(|stack| assert_eq!(stack.borrow().as_slice(), caller_slots));
        assert!(runtime().heap.lock().unwrap().minor_collections > minor_before);
        assert_ne!(
            [parts[1], parts[7]],
            young,
            "the collection moved the young parts and rewrote their slots"
        );
        assert_eq!(text(joined), format!("12345{}é水-7", i64::MIN));
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(3) };
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
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
        // SAFETY: The fixture allocates these fields in word-aligned GC storage at whole-word
        // offsets.
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
        let _guard = isolated_runtime();
        let handle = array_handle_layout().descriptor;
        assert_eq!(
            handle.layout_id,
            gc_layout_id(GcObjectKind::ArrayHandle, 32, 0, HANDLE_MASK)
        );
        assert_eq!(handle.gc_ref_mask, HANDLE_MASK);
        assert_eq!(handle.size, (GC_HEADER_SIZE + 32) as u64);
        let ref_type = willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID;
        for (width, is_ref, type_id) in [
            (1, false, 0),
            (2, false, 0),
            (4, false, 0),
            (8, false, 0),
            (8, true, ref_type),
        ] {
            let flags = willow_abi::array_layout::storage_flags(width, is_ref);
            assert!(array_buffer_layout(-1, flags).is_none());
            assert!(array_buffer_layout(i64::MAX, flags).is_none());
            let max_cap = ((GC_TLAB_MAX_OBJECT_SIZE - GC_HEADER_SIZE - GC_STORAGE_WORD_BYTES)
                / width as usize) as i64;
            for cap in [0, 1, 8, 17, max_cap] {
                let payload = GC_STORAGE_WORD_BYTES + cap as usize * width as usize;
                let total = object_size(payload).unwrap();
                let descriptor = array_buffer_layout(cap, flags).unwrap().descriptor;
                assert_eq!(descriptor.size, total as u64);
                assert_eq!(descriptor.type_id, u64::from(type_id));
                assert_eq!(descriptor.gc_ref_mask, 0);
                assert_eq!(
                    descriptor.layout_id,
                    gc_layout_id(
                        GcObjectKind::ArrayBuffer,
                        (total - GC_HEADER_SIZE) as i64,
                        i64::from(type_id),
                        0
                    )
                );
            }
            assert!(array_buffer_layout(max_cap + 1, flags).is_none());
        }
    }

    #[test]
    fn unregistered_threads_allocate_array_literals_old() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        let fast_before = willow_gc_tlab_fast_allocations();
        // SAFETY: Flag 0 selects untraced word storage.
        let arr = unsafe { crate::array::willow_array_new_literal(3, 0) };
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_len(arr) }, 3);
        assert_eq!(
            generation(arr),
            None,
            "an unregistered thread allocates old"
        );
        assert_eq!(generation(buffer(arr)), None);
        assert_eq!(willow_gc_tlab_fast_allocations(), fast_before);
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
    }

    #[test]
    fn owner_bump_allocates_array_literal_handle_and_buffer_together() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        let _owner = RuntimeTlabOwner::enter();
        prime_runtime_tlab();
        let fast_before = willow_gc_tlab_fast_allocations();
        for (len, is_ref) in [(3i64, 0i64), (0, 0), (2, 1), (64, 0)] {
            // SAFETY: The fixture selects only word-sized scalar or reference storage.
            let arr = unsafe { crate::array::willow_array_new_literal(len, is_ref) };
            let buf = buffer(arr);
            assert_eq!(generation(arr), Some(GC_GENERATION_YOUNG));
            assert_eq!(generation(buf), Some(GC_GENERATION_YOUNG));
            // One bump: the buffer follows the handle object directly.
            assert_eq!(buf as usize, arr as usize + 32 + GC_HEADER_SIZE);
            // SAFETY: The fixture retains the array allocated for these accesses.
            assert_eq!(unsafe { willow_array_len(arr) }, len);
            assert_eq!(handle_word(arr, H_CAP), len);
            for i in 0..len {
                // SAFETY: The fixture retains the array allocated for these accesses.
                assert_eq!(unsafe { willow_array_get(arr, i) }, 0, "zeroed elements");
            }
        }
        assert_eq!(willow_gc_tlab_fast_allocations() - fast_before, 8);
        // A buffer too large for a TLAB takes the old path.
        // SAFETY: Flag 0 selects untraced word storage.
        let big = unsafe { crate::array::willow_array_new_literal(1000, 0) };
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_len(big) }, 1000);
        assert_eq!(generation(big), None);
        assert_eq!(generation(buffer(big)), None);
        // Runtime-internal callers always allocate old.
        let internal = new_old_array(2, 1);
        assert_eq!(generation(internal), None);
        assert_eq!(generation(buffer(internal)), None);
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
    }

    #[test]
    fn array_literal_miss_path_rewrites_rooted_caller_values() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        let _owner = RuntimeTlabOwner::enter();
        // One object per TLAB: every literal reaches the collecting miss path.
        let _stress = StressModes::set("relocate");
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut outer = unsafe { crate::array::willow_array_new_literal(2, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut outer) };
        // Outer's collections are all high-survival evidence. Forget it so
        // inner survives its own allocation young and must move below.
        runtime().heap.lock().unwrap().tenuring = Default::default();
        // SAFETY: Flag 0 selects untraced word storage.
        let mut inner = unsafe { crate::array::willow_array_new_literal(1, 0) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut inner) };
        // SAFETY: The fixture retains the array and stores a word matching its element layout.
        // The destination is the live fixture array; the element word matches its declared
        // storage and tracing layout.
        unsafe {
            willow_array_set(inner, 0, 99);
            willow_array_set(outer, 0, inner as i64);
        }
        let before = [outer, inner];
        let minor_before = runtime().heap.lock().unwrap().minor_collections;
        // SAFETY: Flag 1 allocates full-width reference slots.
        let fresh = unsafe { crate::array::willow_array_new_literal(4, 1) };
        assert!(runtime().heap.lock().unwrap().minor_collections > minor_before);
        assert_ne!(
            [outer, inner],
            before,
            "the literal's collection moved its roots"
        );
        // SAFETY: The fixture retains the array allocated for these accesses.
        unsafe {
            assert_eq!(willow_array_len(fresh), 4);
            assert_eq!(willow_array_get(fresh, 3), 0);
        }
        // SAFETY: The fixture retains the array allocated for these accesses.
        let element = unsafe { willow_array_get(outer, 0) } as *mut u8;
        assert_eq!(element, inner, "the moved element edge follows the copy");
        // SAFETY: The fixture retains the array allocated for these accesses.
        unsafe {
            assert_eq!(willow_array_get(element, 0), 99);
            assert_eq!(willow_array_get(outer, 1), 0);
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
    }

    #[test]
    fn push_growth_gives_old_handles_young_buffers_kept_by_the_barrier() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        let _owner = RuntimeTlabOwner::enter();
        let mut arr = new_old_array(0, 1);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        let old_handle = arr;
        prime_runtime_tlab();
        for i in 0..20 {
            // SAFETY: The constructor selects scalar word storage. This fixture uses the
            // resulting handle before any collection.
            unsafe {
                let element = willow_array_new(1, 0);
                willow_array_set(element, 0, i);
                willow_array_push(arr, element as i64);
            }
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
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_len(arr) }, 20);
        for i in 0..20 {
            assert_eq!(
                // SAFETY: The fixture retains the array allocated for these accesses.
                unsafe { willow_array_get(willow_array_get(arr, i) as *mut u8, 0) },
                i
            );
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
    }

    #[test]
    fn resize_reloads_handle_after_collection_during_first_growth() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        let _registration = MutatorRegistration::new();
        let _owner = RuntimeTlabOwner::enter();
        // SAFETY: Flag 0 selects untraced word storage.
        let array = GcRootHandle::new(unsafe { willow_array_new(256, 0) });
        let original = array.load();
        assert!(young(original));
        // SAFETY: The fixture retains the array and stores a word matching its element
        // layout.
        unsafe { willow_array_set(original, 255, 73) };

        // The first append doubles the buffer to 512 words, so the large
        // allocation polls the pending stop. The next two appends fit.
        let minors = willow_gc_minor_collections();
        let collector = std::thread::spawn(crate::runtime::serving(|| willow_gc_minor_collect()));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !runtime().stop_requested.load(Ordering::Acquire) {
            assert!(std::time::Instant::now() < deadline, "stop never requested");
            std::thread::yield_now();
        }
        {
            // Model a relocatable caller: the handle is rooted and the old
            // address is used only for comparison once resize returns.
            let _relocation = RelocationSafeScope::enter();
            // SAFETY: The live array has the storage layout selected by the resize mask.
            unsafe { crate::array::willow_array_resize(original, 259, -1) };
        }
        assert_eq!(willow_gc_minor_collections(), minors + 1);
        let current = array.load();
        assert_ne!(current, original);
        // SAFETY: The fixture retains the array allocated for these accesses.
        unsafe {
            assert_eq!(willow_array_len(current), 259);
            assert_eq!(willow_array_get(current, 255), 73);
        }
        for index in 256..259 {
            // SAFETY: The fixture retains the array allocated for these accesses.
            assert_eq!(unsafe { willow_array_get(current, index) }, 0);
        }
        collector.join().unwrap();
    }

    #[test]
    fn push_growth_on_a_tlab_miss_keeps_buffers_young() {
        let _guard = isolated_runtime();
        // Up to 250 elements every grown buffer still fits a TLAB object;
        // larger buffers are large objects and are allocated old by design.
        for (flags, pushes) in [(0, 9i64), (0, 64), (0, 250), (1, 9), (1, 64), (1, 250)] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { reset_internal() };
            let _owner = RuntimeTlabOwner::enter();
            // SAFETY: Flags 0 and 1 select word and reference storage.
            let mut arr = unsafe { willow_array_new(0, flags) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut arr) };
            let (old_before, _) = path_counts();
            let mut misses = 0;
            for i in 0..pushes {
                if word(arr, 0) == word(arr, 1) {
                    // Fill the TLAB so this growth misses it and takes the
                    // allocation slow path.
                    while try_alloc_young([array_pair_layout(0).unwrap()]).is_some() {}
                    misses += 1;
                }
                // A reference array holds null elements, which need no tracing.
                let value = if flags == 0 { i } else { 0 };
                // SAFETY: The fixture retains the array and stores a word matching its
                // element layout.
                unsafe { willow_array_push(arr, value) };
            }
            assert!(young(buffer(arr)), "flags {flags}, pushes {pushes}");
            assert!(misses >= 1, "flags {flags}, pushes {pushes}");
            assert_eq!(
                path_counts().0,
                old_before,
                "flags {flags}, pushes {pushes}"
            );
            // SAFETY: The fixture retains the array allocated for these accesses.
            assert_eq!(unsafe { willow_array_len(arr) }, pushes);
            // SAFETY: The removed shadow-stack entries belong to this operation and their slot
            // storage is still live.
            unsafe { willow_pop_roots(1) };
        }
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
    }

    #[test]
    fn push_growth_under_relocation_keeps_every_element() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        let _owner = RuntimeTlabOwner::enter();
        let _stress = StressModes::set("relocate");
        // SAFETY: Flag 0 selects untraced word storage.
        let mut arr = unsafe { willow_array_new(0, 0) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        for i in 0..300 {
            // SAFETY: The fixture retains the array and stores a word matching its element
            // layout.
            unsafe { willow_array_push(arr, i * 3) };
        }
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_len(arr) }, 300);
        for i in 0..300 {
            // SAFETY: The fixture retains the array allocated for these accesses.
            assert_eq!(unsafe { willow_array_get(arr, i) }, i * 3);
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
    }

    // -- runtime-created method results and parse Results ------------------

    fn ws(text: &str) -> *mut u8 {
        crate::string::willow_string_from_str(text)
    }

    fn word(ptr: *mut u8, index: usize) -> i64 {
        // SAFETY: The GC allocator word-aligns this payload and the fixture reserves the accessed
        // words.
        unsafe { *(ptr as *const i64).add(index) }
    }

    /// `(old allocations, runtime-root slot registrations)` made by this thread.
    fn path_counts() -> (usize, usize) {
        runtime().path_observation.current()
    }

    fn young(ptr: *mut u8) -> bool {
        generation(ptr) == Some(GC_GENERATION_YOUNG)
    }

    /// Text of a `Result<_, ParseError>::Err(Invalid(message))`.
    fn parse_error_message(result: *mut u8) -> String {
        assert_eq!(word(result, 0), 1, "Err tag");
        let parse_error = word(result, 1) as *mut u8;
        assert_eq!(word(parse_error, 0), 0, "Invalid tag");
        text(word(parse_error, 1) as *mut u8)
    }

    #[test]
    fn runtime_method_results_bump_allocate_young_without_root_traffic() {
        use crate::string::*;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        let _owner = RuntimeTlabOwner::enter();
        let (source, space, comma, bad) = (ws("  lend 123456 45 6 "), ws(" "), ws(","), ws("12x"));
        let float = ws("2.5");
        prime_runtime_tlab();
        let (old_before, roots_before) = path_counts();
        let fast_before = willow_gc_tlab_fast_allocations();

        // SAFETY: The operands are initialized in this isolated heap and the primed TLAB covers the
        // intervening allocations.
        let trimmed = unsafe { willow_string_trim(source) };
        assert_eq!(text(trimmed), "lend 123456 45 6");
        // SAFETY: The operands are initialized in this isolated heap and the primed TLAB covers the
        // intervening allocations.
        let digits = unsafe { willow_string_substring(trimmed, 5, 11) };
        assert_eq!(text(digits), "123456");
        // SAFETY: The operands are initialized in this isolated heap and the primed TLAB covers the
        // intervening allocations.
        let parts = unsafe { willow_string_split(trimmed, space) };
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_len(parts) }, 4);
        let fields: Vec<_> = (0..4)
            // SAFETY: The fixture retains the array allocated for these accesses.
            .map(|i| unsafe { willow_array_get(parts, i) } as *mut u8)
            .collect();
        assert_eq!(
            fields.iter().map(|&part| text(part)).collect::<Vec<_>>(),
            ["lend", "123456", "45", "6"]
        );
        // SAFETY: The operands are initialized in this isolated heap and the primed TLAB covers the
        // intervening allocations.
        let repeated = unsafe { willow_string_repeat(digits, 3) };
        assert_eq!(text(repeated), "123456123456123456");
        // SAFETY: The operands are initialized in this isolated heap and the primed TLAB covers the
        // intervening allocations.
        let joined = unsafe { willow_array_join(parts, comma) };
        assert_eq!(text(joined), "lend,123456,45,6");
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let ok = unsafe { crate::math::willow_i64_parse(digits) };
        assert_eq!([word(ok, 0), word(ok, 1)], [0, 123456]);
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let err = unsafe { crate::math::willow_i64_parse(bad) };
        assert!(parse_error_message(err).starts_with("invalid integer"));
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let float_ok = unsafe { crate::math::willow_f64_parse(float) };
        assert_eq!(
            [word(float_ok, 0), word(float_ok, 1)],
            [0, 2.5f64.to_bits() as i64]
        );
        let float_text = crate::math::willow_f64_to_string(2.5);
        assert_eq!(text(float_text), "2.5");
        let bool_text = crate::math::willow_bool_to_string(1);
        assert_eq!(text(bool_text), "true");
        // SAFETY: The operands are initialized in this isolated heap and the primed TLAB covers the
        // intervening allocations.
        let debug = unsafe { willow_string_debug(digits) };
        assert_eq!(text(debug), "\"123456\"");

        let mut results = vec![
            trimmed, digits, parts, repeated, joined, ok, err, float_ok, float_text, bool_text,
            debug,
        ];
        results.extend(&fields);
        results.push(word(err, 1) as *mut u8);
        results.push(word(word(err, 1) as *mut u8, 1) as *mut u8);
        for result in results {
            assert!(young(result), "runtime result {result:p} is young");
        }
        // trim, substring, split (handle + buffer + 4 parts), repeat, join,
        // Ok, Err (message + ParseError + Result), f64 Ok, two to_strings, debug.
        assert_eq!(willow_gc_tlab_fast_allocations() - fast_before, 18);
        assert_eq!(
            path_counts(),
            (old_before, roots_before),
            "no old allocation or runtime-root registration"
        );
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
    }

    #[test]
    fn split_reloads_partially_filled_array_after_moving_collection() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        let _owner = RuntimeTlabOwner::enter();
        let large = "z".repeat(GC_TLAB_MAX_OBJECT_SIZE);
        let fields = ["é\0水", "", large.as_str()];
        let source = GcRootHandle::new(ws(&fields.join(",")));
        let separator = GcRootHandle::new(ws(","));
        prime_runtime_tlab();
        let _stress = StressModes::set("minor");

        // The fresh TLAB serves the handle, its buffer and the first two fields.
        // The oversized last field forces a collecting miss after both stores.
        let original =
            RUNTIME_TLAB.with(|tls| tls.0.cursor.load(Ordering::Relaxed)) + GC_HEADER_SIZE;
        let fast = willow_gc_tlab_fast_allocations();
        let minors = willow_gc_minor_collections();
        // SAFETY: Both handles are reloaded after preparing the pending collection.
        let result = unsafe { crate::string::willow_string_split(source.load(), separator.load()) };
        assert_eq!(willow_gc_tlab_fast_allocations() - fast, 4);
        assert_eq!(willow_gc_minor_collections() - minors, 1);
        assert!(!result.is_null());
        assert_ne!(result as usize, original);
        // SAFETY: Split returned a populated Array<String>; this traversal reads it before
        // another GC point.
        assert_eq!(unsafe { willow_array_len(result) }, fields.len() as i64);
        for (index, expected) in fields.iter().enumerate() {
            // SAFETY: Split returned a populated Array<String>; this traversal reads it
            // before another GC point.
            let field = unsafe { willow_array_get(result, index as i64) } as *mut u8;
            assert!(!field.is_null());
            assert_eq!(text(field), *expected);
        }
    }

    #[test]
    fn runtime_method_results_survive_relocating_misses() {
        use crate::string::*;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        let _owner = RuntimeTlabOwner::enter();
        // One object per TLAB: every allocation below misses and collects.
        let _stress = StressModes::set("relocate");
        let slots = [const { std::cell::Cell::new(std::ptr::null_mut::<u8>()) }; 3];
        for slot in &slots {
            // SAFETY: The initialized cell provides a stable writable slot through the
            // matching pop.
            unsafe { willow_push_root(slot.as_ptr()) };
        }
        // SAFETY: The receiver comes directly from the string constructor.
        slots[1].set(unsafe { willow_string_trim(ws(", ")) });
        // SAFETY: The receiver comes directly from the string constructor.
        slots[2].set(unsafe { willow_string_trim(ws("4x")) });
        // Earlier collections are all high-survival evidence. Forget it so the
        // receiver survives its first collections young and must move.
        runtime().heap.lock().unwrap().tenuring = Default::default();
        let left = GcRootHandle::new(ws(" é水 "));
        let right = ws(" b ");
        // SAFETY: The left handle survives allocation of the right operand.
        // Both addresses are current when passed in the exclusive pair array.
        slots[0].set(unsafe {
            concat(&mut [
                willow_abi::string_concat::PART_STRING,
                left.load() as u64,
                willow_abi::string_concat::PART_I64,
                42,
                willow_abi::string_concat::PART_STRING,
                right as u64,
            ])
        });
        drop(left);
        assert!(young(slots[0].get()));
        let minor_before = runtime().heap.lock().unwrap().minor_collections;
        let roots_before = path_counts().1;
        let mut source = slots[0].get();

        // SAFETY: The registered slot supplies the current string immediately before the call.
        let trimmed = unsafe { willow_string_trim(slots[0].get()) };
        assert_ne!(
            slots[0].get(),
            source,
            "the receiver moved during its own result"
        );
        assert_eq!(text(trimmed), "é水 42 b");
        source = slots[0].get();
        assert_eq!(
            // SAFETY: The registered slot supplies the current string immediately before the call.
            text(unsafe { willow_string_substring(slots[0].get(), 1, 6) }),
            "é水"
        );
        assert_ne!(slots[0].get(), source);
        let space = ws(" ");
        // SAFETY: The registered slot supplies the current string immediately before the call.
        let parts = unsafe { willow_string_split(slots[0].get(), space) };
        // SAFETY: The fixture retains the array allocated for these accesses.
        let fields: Vec<_> = (0..unsafe { willow_array_len(parts) })
            // SAFETY: The fixture retains the array allocated for these accesses.
            .map(|i| text(unsafe { willow_array_get(parts, i) } as *mut u8))
            .collect();
        assert_eq!(fields, ["", "é水", "42", "b", ""]);
        let parts = std::cell::Cell::new(parts);
        // SAFETY: The initialized cell provides a stable writable slot through the matching
        // pop.
        unsafe { willow_push_root(parts.as_ptr()) };
        assert_eq!(
            // SAFETY: Split populated the rooted array with strings; the second root supplies its
            // current separator.
            text(unsafe { willow_array_join(parts.get(), slots[1].get()) }),
            ",é水,42,b,"
        );
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        assert_eq!(
            // SAFETY: The registered slot supplies the current string immediately before the call.
            text(unsafe { willow_string_repeat(slots[1].get(), 3) }),
            ",,,"
        );
        // SAFETY: The receiver comes directly from the string constructor.
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let ok = unsafe { crate::math::willow_i64_parse(willow_string_trim(ws(" -17 "))) };
        assert_eq!([word(ok, 0), word(ok, 1)], [0, -17]);
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let err = unsafe { crate::math::willow_i64_parse(slots[2].get()) };
        assert!(parse_error_message(err).starts_with("invalid integer"));
        assert_eq!(
            // SAFETY: The registered slot supplies the current string immediately before the call.
            text(unsafe { willow_string_debug(slots[2].get()) }),
            "\"4x\""
        );

        assert!(runtime().heap.lock().unwrap().minor_collections > minor_before);
        assert_eq!(
            path_counts().1,
            roots_before,
            "misses root only shadow-stack slots"
        );
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(3) };
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
    }

    #[test]
    fn unregistered_threads_build_runtime_method_results_old() {
        use crate::string::*;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
        assert!(!runtime_tlab_enabled());
        let source = ws(" a b ");
        let (old_before, _) = path_counts();
        let space = ws(" ");
        // SAFETY: The source and trimmed string are live in the isolated old heap; the separator is
        // a fresh string.
        let parts = unsafe { willow_string_split(willow_string_trim(source), space) };
        // SAFETY: The fixture retains the array allocated for these accesses.
        unsafe {
            assert_eq!(willow_array_len(parts), 2);
            assert_eq!(text(willow_array_get(parts, 1) as *mut u8), "b");
        }
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let ok = unsafe { crate::math::willow_i64_parse(ws("9")) };
        assert_eq!([word(ok, 0), word(ok, 1)], [0, 9]);
        // SAFETY: Each input is null or a live Willow string whose bytes remain immutable
        // during this call.
        let err = unsafe { crate::math::willow_i64_parse(ws("")) };
        assert!(parse_error_message(err).starts_with("invalid integer"));
        // SAFETY: The fixture retains the array allocated for these accesses.
        for result in [parts, ok, err, unsafe { willow_array_get(parts, 0) }
            as *mut u8]
        {
            assert_eq!(
                generation(result),
                None,
                "an unregistered thread allocates old"
            );
        }
        // trim, two strings, split (handle, buffer, two parts), Ok, Err (3).
        assert!(path_counts().0 - old_before >= 11);
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { reset_internal() };
    }
}
