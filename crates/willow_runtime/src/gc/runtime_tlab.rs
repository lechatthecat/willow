use super::*;

// ---------------------------------------------------------------------------
// Runtime-owned TLAB for string results (willow-8hq4.29)
// ---------------------------------------------------------------------------
//
// Runtime allocations normally go to the old generation under the heap lock.
// String results of operations called directly from generated code
// (concatenation, `i64.toString`) are short-lived and frequent, so they
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

/// Largest String object with a static descriptor; larger ones are not TLAB
/// allocations anyway.
const STRING_DESCRIPTOR_SLOTS: usize =
    GC_TLAB_MAX_OBJECT_SIZE / std::mem::align_of::<GcHeader>() + 1;

/// Static String descriptors indexed by total object size / 8. Like generated
/// descriptors they are never released, so headers point at them unowned.
static STRING_DESCRIPTORS: [willow_abi::GcLayoutDescriptor; STRING_DESCRIPTOR_SLOTS] = {
    let mut table = [willow_abi::GcLayoutDescriptor {
        type_id: 0,
        layout_id: 0,
        gc_ref_mask: 0,
        size: 0,
    }; STRING_DESCRIPTOR_SLOTS];
    let mut slot = GC_HEADER_SIZE / 8;
    while slot < STRING_DESCRIPTOR_SLOTS {
        let total = slot * 8;
        table[slot] = willow_abi::GcLayoutDescriptor {
            type_id: 0,
            layout_id: willow_abi::gc_layout_id(
                GcObjectKind::String,
                (total - GC_HEADER_SIZE) as i64,
                0,
                0,
            ),
            gc_ref_mask: 0,
            size: total as u64,
        };
        slot += 1;
    }
    table
};

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

/// Total object size of a String payload, rounded to header alignment.
fn string_object_size(payload_size: usize) -> Option<usize> {
    GC_HEADER_SIZE
        .checked_add(payload_size)?
        .checked_next_multiple_of(std::mem::align_of::<GcHeader>())
}

/// Bump-allocate a young String payload of at least `payload_size` bytes from
/// this thread's runtime TLAB. Never reaches a GC point. `None` on a TLAB miss,
/// for objects too large for a TLAB, or on a thread that may not own one.
/// The payload is zeroed: TLAB chunks are fresh zeroed storage.
pub(crate) fn try_alloc_young_string_payload(payload_size: usize) -> Option<*mut u8> {
    let total = string_object_size(payload_size)?;
    if total > GC_TLAB_MAX_OBJECT_SIZE || !runtime_tlab_enabled() {
        return None;
    }
    let descriptor = &STRING_DESCRIPTORS[total / 8];
    RUNTIME_TLAB.with(|RuntimeTlab(tls)| {
        let cursor = tls.cursor.load(Ordering::Relaxed);
        let limit = tls.limit.load(Ordering::Relaxed);
        if cursor == 0 || cursor.checked_add(total)? > limit {
            return None;
        }
        let header = cursor as *mut GcHeader;
        // SAFETY: [cursor, cursor + total) lies in this thread's active chunk,
        // which only this thread writes until a collection retires it, and a
        // collection cannot run between this check and the cursor store.
        unsafe {
            header.write(GcHeader {
                marked: false,
                allocated: true,
                generation: GC_GENERATION_YOUNG,
                age: 0,
                descriptor_owned: false,
                remembered: 0,
                descriptor: descriptor as *const willow_abi::GcLayoutDescriptor as usize,
            });
        }
        // Publish the start bit after the header, as generated code does. The
        // owner is the only writer of the active chunk's start words.
        let base = limit - GC_TLAB_CHUNK_SIZE;
        let granule = (cursor - base) / GC_REGION_MARK_GRANULE;
        let words = tls.start_bits.load(Ordering::Relaxed) as *const AtomicU64;
        // SAFETY: the active chunk's start words cover every granule below
        // its limit and stay alive while the chunk is active.
        let word = unsafe { &*words.add(granule / 64) };
        word.store(
            word.load(Ordering::Relaxed) | (1u64 << (granule % 64)),
            Ordering::Release,
        );
        tls.cursor.store(cursor + total, Ordering::Release);
        Some((cursor + GC_HEADER_SIZE) as *mut u8)
    })
}

/// Allocation miss path for [`try_alloc_young_string_payload`]. May collect.
///
/// Only runtime entry points called directly by generated code may use it:
/// on a TLAB-owning thread a minor collection here may move this thread's
/// root-slot referents, so the caller must keep its own managed pointers in
/// pushed root slots and reload them afterwards. Other threads allocate old.
pub(crate) fn alloc_string_payload_slow(payload_size: usize) -> *mut u8 {
    let Some(total) = string_object_size(payload_size) else {
        crate::failure::resource_exhausted(format_args!("string allocation size overflow"));
    };
    let payload = (total - GC_HEADER_SIZE) as i64;
    if !runtime_tlab_enabled() {
        return willow_alloc_with_layout(GcObjectKind::String, 0, payload, 0);
    }
    let _relocation = RelocationSafeScope::enter();
    let layout_id = gc_layout_id(GcObjectKind::String, payload, 0, 0);
    RUNTIME_TLAB.with(|tls| tlab_alloc_slow(tls.address(), layout_id, 0, payload, 0))
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
            let total = string_object_size(payload).unwrap();
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
}
