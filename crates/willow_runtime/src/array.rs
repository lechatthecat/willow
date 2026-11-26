//! GC-managed growable arrays.
//!
//! An array value is a small fixed **handle** that points at a separately
//! allocated **buffer**, so the array can grow (`push`) without changing the
//! handle pointer that user code holds:
//!
//! ```text
//!   handle payload:  [ len(i64), cap(i64), is_ref(i64), buffer ]   (gc_ref_mask = 0b1000)
//!   buffer payload:  [ traced_prefix(atomic i64), elem0, elem1, ... elem_{cap-1} ]
//! ```
//!
//! Integer and character elements use their native 1/2/4/8-byte widths.
//! Other scalars and reference elements keep the 64-bit word representation.
//! The handle flags retain the scalar width independently of the GC trace bit.
//! The handle traces `buffer` through `gc_ref_mask`; a reference buffer uses a
//! dedicated `type_id` + trace function that scans its published prefix.
//! Ordinary arrays trace their logical length and clear removed slots. Pair
//! arrays retain scalar or interface boxes as stable slot storage, even after pop;
//! their traced prefix is the initialized high-water mark, bounded by capacity.
//! Capacity is kept in the handle. Generated code roots captured buffer owners.
//!
//! Index access is bounds-checked against `len`; out-of-range aborts.
//!
//! NOTE: `willow_array_element_addr` returns a pointer **into the current
//! buffer**. A `push` that grows the array reallocates the buffer, so any
//! element address taken before such a `push` is invalidated.

use std::sync::atomic::{AtomicI64, Ordering};

use crate::gc::{
    GcObjectKind, GcStoreDestination, willow_alloc_with_layout, write_barrier_array_slot,
    write_barrier_object_owner,
};

use willow_abi::runtime_type_ids::ARRAY_REF_TYPE_ID;

use willow_abi::array_layout::{
    BUFFER_HEADER_WORDS, H_BUF, H_CAP, H_IS_REF, H_LEN, HANDLE_MASK, HANDLE_WORDS,
    PAIR_STORAGE_EXPOSED, WORD_BYTES, buffer_bytes, element_width, is_reference,
};
const WORD: i64 = WORD_BYTES as i64;

/// Read the published tracing prefix; the buffer remains independently traceable
/// when a captured element reference keeps it alive after handle growth.
unsafe fn buffer_len(buffer: *mut u8) -> i64 {
    // SAFETY: The GC word-aligns the buffer. Its first word is reserved for the length and all
    // concurrent accesses are atomic.
    unsafe { AtomicI64::from_ptr(buffer.cast()).load(Ordering::Acquire) }
}

/// Publish the traced prefix after initializing slots or clearing ordinary slots.
/// The concurrent marker may observe an older prefix; insertion barriers retain
/// every newly stored reference even when its slot is outside that snapshot.
unsafe fn set_buffer_len(buffer: *mut u8, len: i64) {
    // SAFETY: The GC word-aligns the buffer. Its first word is reserved for the length and all
    // concurrent accesses are atomic.
    unsafe { AtomicI64::from_ptr(buffer.cast()).store(len, Ordering::Release) };
}

/// Trace the published prefix (logical length, or initialized pair storage).
///
/// # Safety
/// `payload` must point at a buffer allocated by [`alloc_buffer`].
unsafe fn trace_array_ref(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    // SAFETY: The trace hook receives a live reference buffer. Its published length bounds the
    // initialized word slots.
    let len = unsafe { buffer_len(payload) };
    for index in 0..len {
        // SAFETY: The trace hook receives a live reference buffer. Its published length bounds the
        // initialized word slots.
        slots.push(unsafe { buf_slot(payload, index).cast() });
    }
}

/// Visit the built-in array tracer's contiguous slots without allocating a
/// slot vector. Other trace hooks and any inline mask are handled by the caller.
///
/// # Safety
/// For a matching type and hook, `payload` must be a live, word-aligned
/// reference buffer. Its acquire-loaded length covers initialized reference
/// slots. Keep that storage live at the same address until the range has been
/// consumed. Concurrent readers must use atomic slot loads. Rewriting the
/// slots during evacuation requires exclusive access.
pub(crate) unsafe fn minor_reference_range(
    type_id: u32,
    trace: Option<crate::gc::TraceFn>,
    payload: *mut u8,
) -> Option<(*mut *mut u8, usize)> {
    if type_id != ARRAY_REF_TYPE_ID
        || !trace
            .is_some_and(|hook| std::ptr::fn_addr_eq(hook, trace_array_ref as crate::gc::TraceFn))
    {
        return None;
    }
    // SAFETY: The checked layout has a length word even when capacity is zero.
    let len = unsafe { buffer_len(payload) }.max(0) as usize;
    // SAFETY: Index zero addresses the first slot or the end of a header-only
    // buffer. Only indices below the returned length can be visited.
    Some((unsafe { buf_slot(payload, 0).cast() }, len))
}

/// Both length and reference slots use atomic publication for concurrent GC.
unsafe fn snapshot_array_ref(payload: *mut u8, children: &mut Vec<*mut u8>) {
    // SAFETY: The trace hook receives a live reference buffer. Its published length bounds the
    // initialized word slots.
    let len = unsafe { buffer_len(payload) };
    for index in 0..len {
        // SAFETY: The trace hook receives a live reference buffer. Its published length bounds the
        // initialized word slots.
        children.push(unsafe { crate::gc::load_gc_reference(buf_slot(payload, index).cast()) });
    }
}

unsafe fn snapshot_array_ref_slice(
    payload: *mut u8,
    cursor: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> crate::gc::TraceSliceProgress {
    // SAFETY: The trace hook receives a live reference buffer. Its published length bounds the
    // initialized word slots.
    let len = unsafe { buffer_len(payload) }.max(0) as usize;
    let end = cursor.saturating_add(limit).min(len);
    for index in cursor..end {
        children
            // SAFETY: The trace hook receives a live reference buffer. Its published length bounds
            // the initialized word slots.
            .push(unsafe { crate::gc::load_gc_reference(buf_slot(payload, index as i64).cast()) });
    }
    if end < len {
        crate::gc::TraceSliceProgress::Continue(end)
    } else {
        crate::gc::TraceSliceProgress::Done
    }
}

/// Register the ref-buffer trace once per owning collector's registry generation.
/// `willow_gc_init` invalidates registration, so the next allocation reinstalls
/// the hook after a reset; warmed allocations only check the generation.
const ARRAY_REGISTRATION: crate::gc::NativeGcRegistration = crate::gc::NativeGcRegistration::Array;
const ARRAY_GC_TYPES: &[crate::gc::NativeGcType] = &[
    // SAFETY: The buffer ABI initializes capacity and aligned reference slots for this ID.
    // Stopped tracing exposes the slots; snapshots atomically load their values over a
    // bounded index range.
    unsafe {
        crate::gc::NativeGcType::new(ARRAY_REF_TYPE_ID, Some(trace_array_ref), None)
            .with_concurrent_trace(snapshot_array_ref)
            .with_concurrent_slice(snapshot_array_ref_slice)
    },
];

pub(crate) fn ensure_trace_registered() {
    // SAFETY: This is the complete descriptor set for the reference-array buffer registration
    // group and its constructors.
    unsafe { ARRAY_REGISTRATION.ensure(ARRAY_GC_TYPES) };
}

/// Allocate an empty, zero-initialized buffer of `cap` slots (`[len=0, e0..]`).
fn alloc_buffer(cap: i64, flags: i64) -> *mut u8 {
    let payload = match buffer_bytes(cap, flags) {
        Some(p) => p,
        None => {
            raise_with(&format!("array capacity too large: {cap}"));
            return std::ptr::null_mut();
        }
    };
    let buf = if is_reference(flags) {
        ensure_trace_registered();
        // SAFETY: The constructor selects the matching payload layout and initializes its
        // fields before exposing it or reaching a GC point.
        unsafe {
            willow_alloc_with_layout(GcObjectKind::ArrayBuffer, ARRAY_REF_TYPE_ID, payload, 0)
        }
    } else {
        // SAFETY: The constructor selects the matching payload layout and initializes its
        // fields before exposing it or reaching a GC point.
        unsafe { willow_alloc_with_layout(GcObjectKind::ArrayBuffer, 0, payload, 0) }
    };
    if buf.is_null() {
        raise_with("array buffer allocation failed");
        return buf;
    }
    // SAFETY: The freshly allocated buffer has a word-aligned length field, even at zero capacity.
    unsafe { set_buffer_len(buf, 0) };
    buf
}

/// Address of element slot `index` in a buffer (unchecked).
///
/// # Safety
/// `buffer` must be a live word-slot buffer and `0 <= index <= capacity`.
/// At capacity the pointer is one past the allocation and cannot be dereferenced.
unsafe fn buf_slot(buffer: *mut u8, index: i64) -> *mut i64 {
    // SAFETY: The caller bounds the index by capacity. The GC word-aligned base and header
    // preserve the element stride's alignment.
    unsafe { (buffer as *mut i64).add(BUFFER_HEADER_WORDS + index as usize) }
}

/// Compute a slot address using the declared width of the live buffer.
///
/// # Safety
/// The flags must match the allocation and `0 <= index <= capacity`.
/// At capacity the result is only an empty range boundary and must not be dereferenced.
unsafe fn element_slot(buffer: *mut u8, index: i64, flags: i64) -> *mut u8 {
    // SAFETY: The caller bounds the index by capacity. The GC word-aligned base and header
    // preserve the element stride's alignment.
    unsafe {
        buffer
            .add(BUFFER_HEADER_WORDS * WORD_BYTES as usize + index as usize * element_width(flags))
    }
}

/// Preserve scalar bits; the compiler sign-extends signed narrow values.
unsafe fn load_buffer_slot(buffer: *mut u8, index: i64, flags: i64) -> i64 {
    // SAFETY: The caller bounds the index by capacity. The GC word-aligned base and header
    // preserve the element stride's alignment.
    let slot = unsafe { element_slot(buffer, index, flags) };
    // SAFETY: The GC word-aligns the buffer and its header. Each in-bounds element offset is a
    // multiple of the selected width.
    unsafe {
        match element_width(flags) {
            1 => i64::from(slot.read()),
            2 => i64::from(slot.cast::<u16>().read()),
            4 => i64::from(slot.cast::<u32>().read()),
            _ => slot.cast::<i64>().read(),
        }
    }
}

unsafe fn store_buffer_slot(buffer: *mut u8, index: i64, value: i64, flags: i64) {
    let is_ref = is_reference(flags);
    if is_ref {
        // SAFETY: The caller bounds the index by capacity. The GC word-aligned base and header
        // preserve the element stride's alignment.
        let slot = unsafe { buf_slot(buffer, index) }.cast::<u8>();
        // SAFETY: The element slot belongs to this live reference buffer and the old and
        // incoming words are its managed references.
        unsafe {
            write_barrier_array_slot(
                buffer,
                slot,
                crate::gc::load_gc_reference(slot.cast()),
                value as *mut u8,
            )
        };
    }
    if is_ref {
        // SAFETY: Reference elements occupy full aligned words; the barrier above precedes
        // publication.
        unsafe { crate::gc::store_gc_reference(buf_slot(buffer, index).cast(), value as *mut u8) };
    } else {
        // SAFETY: The caller bounds the index by capacity. The GC word-aligned base and header
        // preserve the element stride's alignment.
        let slot = unsafe { element_slot(buffer, index, flags) };
        // SAFETY: The GC word-aligns the buffer and its header. Each in-bounds element offset is a
        // multiple of the selected width.
        unsafe {
            match element_width(flags) {
                1 => slot.write(value as u8),
                2 => slot.cast::<u16>().write(value as u16),
                4 => slot.cast::<u32>().write(value as u32),
                _ => slot.cast::<i64>().write(value),
            }
        }
    }
}

unsafe fn handle_word(arr: *mut u8, w: usize) -> i64 {
    // SAFETY: The GC word-aligns the handle and the field index selects one of its reserved words.
    unsafe { *((arr as *const i64).add(w)) }
}
unsafe fn set_handle_word(arr: *mut u8, w: usize, v: i64) {
    if w == H_BUF {
        // SAFETY: The GC word-aligns the handle and the field index selects one of its reserved
        // words.
        unsafe { crate::gc::store_gc_reference((arr as *mut i64).add(w).cast(), v as *mut u8) };
    } else {
        // SAFETY: The GC word-aligns the handle and the field index selects one of its reserved
        // words.
        unsafe { *((arr as *mut i64).add(w)) = v };
    }
}
unsafe fn handle_buffer(arr: *mut u8) -> *mut u8 {
    // SAFETY: The GC word-aligns the handle and the field index selects one of its reserved words.
    unsafe { handle_word(arr, H_BUF) as *mut u8 }
}

/// Unchecked scalar slot for runtime-owned parallel chunks. The caller proves
/// the index is in range, the array contains scalar words, and writes are
/// exclusive to this index. Do not retain the pointer across a mapper call or
/// any safepoint; reload the rooted handle first after such a call.
#[inline]
pub(crate) unsafe fn scalar_slot_unchecked(arr: *mut u8, index: i64) -> *mut i64 {
    // SAFETY: The caller supplies a live word array and an index within its buffer.
    unsafe { buf_slot(handle_buffer(arr), index) }
}

/// Allocate an array of `len` elements (all zero). `elem_is_ref` contains the
/// shared storage flags; legacy 0/1 still select scalar/reference word storage.
/// Raises a language panic on a negative length.
///
/// A handle and a buffer that fit a TLAB are bump-allocated young together, with no lock,
/// root registration or old-region work. Only a TLAB miss
/// reaches a GC point, which may move this thread's root-slot referents like
/// any generated allocation; generated code reloads its values after the call.
///
/// # Safety
/// The storage flags must describe a supported array layout. Reference elements
/// always require full pointer-width slots, including when the buffer starts zeroed.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_new(len: i64, elem_is_ref: i64) -> *mut u8 {
    if len < 0 {
        return raise_negative_length(len);
    }
    allocate_array(len, elem_is_ref)
}

/// Allocation-only entry for compiler-proven constant-length array literals.
/// Invalid ABI lengths are fatal; allocation exhaustion uses the runtime's
/// non-recoverable resource failure policy. No language panic can return.
///
/// # Safety
/// The storage flags must describe a supported array layout. Reference elements
/// always require full pointer-width slots, including when the buffer starts zeroed.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_new_literal(len: i64, elem_is_ref: i64) -> *mut u8 {
    if buffer_bytes(len, elem_is_ref).is_none() {
        crate::failure::fatal_invariant("invalid constant array literal length");
    }
    let array = allocate_array(len, elem_is_ref);
    if array.is_null() {
        crate::failure::resource_exhausted(format_args!("array literal allocation failed"));
    }
    array
}

// Shared allocation/GC/rooting path. Both callers validate non-negative length;
// the literal entry also proves the old buffer payload cannot overflow.
fn allocate_array(len: i64, elem_is_ref: i64) -> *mut u8 {
    let Some(buffer_layout) = crate::gc::array_buffer_layout(len, elem_is_ref) else {
        return new_old_array(len, elem_is_ref);
    };
    let handle_layout = crate::gc::array_handle_layout();
    let (handle, buffer) = match crate::gc::try_alloc_young([handle_layout, buffer_layout]) {
        Some([handle, buffer]) => (handle, buffer),
        None => {
            let mut handle = crate::gc::alloc_young_layout_slow(handle_layout);
            if handle.is_null() {
                return handle;
            }
            // The handle's slot is null, so a collection here traces it safely.
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { crate::gc::push_root(&raw mut handle) };
            let buffer = match crate::gc::try_alloc_young([buffer_layout]) {
                Some([buffer]) => buffer,
                None => crate::gc::alloc_young_layout_slow(buffer_layout),
            };
            // The removed shadow-stack entry belongs to this operation.
            crate::gc::pop_roots(1);
            if buffer.is_null() {
                return buffer;
            }
            (handle, buffer)
        }
    };
    // SAFETY: Both fresh allocations are word-aligned and sized for the handle and the requested
    // buffer capacity.
    unsafe { init_array(handle, buffer, len, elem_is_ref) };
    handle
}

fn raise_negative_length(len: i64) -> *mut u8 {
    raise_with(&format!(
        "cannot create an array with negative length {len}"
    ));
    std::ptr::null_mut()
}

/// Allocate an array in the old generation. Runtime-internal callers use this:
/// it never moves root-slot referents, so their raw managed pointers stay valid.
pub(crate) fn new_old_array(len: i64, elem_is_ref: i64) -> *mut u8 {
    if len < 0 {
        return raise_negative_length(len);
    }
    // Allocate the handle first (zero-filled, buffer slot null), root it, then
    // allocate the buffer — so a collection during the buffer allocation cannot
    // free the handle, and the still-null buffer slot traces safely.
    // SAFETY: The constructor selects the matching payload layout and initializes its fields
    // before exposing it or reaching a GC point.
    let handle = unsafe {
        willow_alloc_with_layout(
            GcObjectKind::ArrayHandle,
            0,
            HANDLE_WORDS * WORD,
            HANDLE_MASK,
        )
    };
    if handle.is_null() {
        return std::ptr::null_mut();
    }
    let handle_root = crate::gc::GcRootHandle::new(handle);
    let buffer = alloc_buffer(len, elem_is_ref);
    if buffer.is_null() {
        return std::ptr::null_mut();
    }
    let handle = handle_root.load();
    // SAFETY: Both fresh allocations are word-aligned and sized for the handle and the requested
    // buffer capacity.
    unsafe { init_array(handle, buffer, len, elem_is_ref) };
    handle
}

/// Borrow packed byte storage without a safepoint or an element-wise conversion.
///
/// # Safety
/// `array` is null or a live packed Array<u8>. The borrow must end before any GC safepoint,
/// array mutation, or foreign call; native allocation alone is safe.
pub(crate) unsafe fn packed_bytes<'a>(array: *mut u8) -> &'a [u8] {
    if array.is_null() {
        return &[];
    }
    // SAFETY: The caller retains the packed byte array through this borrow. Its length bounds the
    // bytes after the buffer header.
    let flags = unsafe { handle_word(array, H_IS_REF) };
    if flags != willow_abi::array_layout::storage_flags(1, false) {
        crate::failure::fatal_invariant("expected packed Array<u8>");
    }
    // SAFETY: The caller retains the packed byte array through this borrow. Its length bounds the
    // bytes after the buffer header.
    let len = unsafe { handle_word(array, H_LEN) } as usize;
    // SAFETY: The caller retains the packed byte array through this borrow. Its length bounds the
    // bytes after the buffer header.
    unsafe { std::slice::from_raw_parts(element_slot(handle_buffer(array), 0, flags), len) }
}

/// Copy native bytes into packed GC storage in one bulk transfer. The input
/// must be native storage, not an unrooted slice into the moving GC heap.
pub(crate) fn from_native_bytes(bytes: &[u8]) -> *mut u8 {
    let length = i64::try_from(bytes.len()).expect("byte buffer length exceeds i64");
    let flags = willow_abi::array_layout::storage_flags(1, false);
    let array = new_old_array(length, flags);
    if !bytes.is_empty() {
        // SAFETY: The new packed array has space for the entire source slice in a distinct
        // allocation.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                element_slot(handle_buffer(array), 0, flags),
                bytes.len(),
            );
        }
        crate::observation::record(crate::observation::Event::OutputCopies, 1);
    }
    array
}

/// Fill a fresh zeroed handle with its length, capacity and zeroed buffer.
///
/// # Safety
/// `handle` and `buffer` are fresh allocations with `len` buffer slots.
unsafe fn init_array(handle: *mut u8, buffer: *mut u8, len: i64, elem_is_ref: i64) {
    // SAFETY: Both fresh allocations are word-aligned and sized for the handle and the requested
    // buffer capacity.
    unsafe {
        set_buffer_len(buffer, len);
        set_handle_word(handle, H_LEN, len);
        set_handle_word(handle, H_CAP, len);
        set_handle_word(handle, H_IS_REF, elem_is_ref & !PAIR_STORAGE_EXPOSED);
        write_barrier_object_owner(
            handle,
            std::ptr::null_mut(),
            buffer,
            GcStoreDestination::ContainerInternal as i64,
        );
        set_handle_word(handle, H_BUF, buffer as i64);
    }
}

/// Allocate an independent copy of `arr` (same length, element ref-ness, and
/// element words). Backs `Array<T>::freeze()` -> `FrozenArray<T>`:
/// the copy has no mutation API and shares no buffer with the original, so it is
/// safe to treat as immutable. Shallow per the element word (ref elements share
/// their — Sync — referents).
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_copy(arr: *mut u8) -> *mut u8 {
    if arr.is_null() {
        raise_with("cannot freeze a null array");
        return std::ptr::null_mut();
    }
    // SAFETY: The caller supplied the current source handle and null was rejected.
    let len = unsafe { array_len(arr) };
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let is_ref = unsafe { handle_word(arr, H_IS_REF) };
    let arr = crate::gc::GcRootHandle::new(arr);
    let copy = new_old_array(len, is_ref);
    if copy.is_null() {
        return copy;
    }
    let copy = crate::gc::GcRootHandle::new(copy);
    let mut i = 0;
    while i < len {
        // SAFETY: Both handles come from the source contract and the fresh copy of the same
        // layout; each root is reloaded for this in-bounds transfer.
        unsafe { willow_array_set(copy.load(), i, willow_array_get(arr.load(), i)) };
        i += 1;
    }
    copy.load()
}

/// Freeze an array whose element words point to scalar or interface pair boxes. Copy the boxes too: mutable element references must not
/// change the frozen snapshot. Both arrays remain rooted across every allocation.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// The reference buffer must store GC pair boxes with two aligned, initialized words
/// and the element type's tracing layout. Zero slots are allowed.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_copy_scalar_pairs(arr: *mut u8) -> *mut u8 {
    if arr.is_null() {
        raise_with("cannot freeze a null array");
        return std::ptr::null_mut();
    }
    // SAFETY: The source handle is live by the caller contract and passed the null check.
    let len = unsafe { array_len(arr) };
    let arr = crate::gc::GcRootHandle::new(arr);
    let copy = new_old_array(len, 1);
    if copy.is_null() {
        return copy;
    }
    let copy = crate::gc::GcRootHandle::new(copy);
    for index in 0..len {
        // SAFETY: The source holds the declared pair slots and both arrays are reloaded from
        // roots. The new box is stored before another allocation.
        if unsafe { willow_array_get(arr.load(), index) } == 0 {
            continue;
        }
        // SAFETY: The managed pair reserves two words at the GC allocator's word alignment.
        let pair = unsafe { copy_scalar_pair(willow_array_get(arr.load(), index) as *const i64) };
        if pair.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: The source holds the declared pair slots and both arrays are reloaded from
        // roots. The new box is stored before another allocation.
        unsafe { willow_array_set(copy.load(), index, pair as i64) };
    }
    copy.load()
}

/// Read immutable tracing metadata without borrowing mutable header bytes.
/// Pair storage has either no references or a traced first word.
unsafe fn pair_ref_mask(pair: *const i64) -> u64 {
    // SAFETY: the source is a live, unforwarded managed pair.
    unsafe { crate::gc::GcHeader::live_descriptor(pair.cast()).gc_ref_mask }
}

/// Publish the object word through the same SATB/generational barrier used by
/// ordinary array elements; the vtable word is never interpreted as a pointer.
unsafe fn store_pair(pair: *mut i64, first: i64, second: i64, mask: u64) {
    if mask & 1 != 0 {
        // SAFETY: The initialized pair is a GC payload and its reference word and replacement
        // match the pair layout.
        unsafe {
            write_barrier_object_owner(
                pair.cast(),
                crate::gc::load_gc_reference(pair.cast()),
                first as *mut u8,
                GcStoreDestination::ArrayElement as i64,
            )
        };
        // SAFETY: The pair allocation contains two aligned words. Its layout selects reference
        // access for the traced first word.
        unsafe { crate::gc::store_gc_reference(pair.cast(), first as *mut u8) };
    } else {
        // SAFETY: The pair allocation contains two aligned words. Its layout selects reference
        // access for the traced first word.
        unsafe { pair.write(first) };
    }
    // SAFETY: The pair allocation contains two aligned words. Its layout selects reference access
    // for the traced first word.
    unsafe { pair.add(1).write(second) };
}

/// Copy both words and preserve whether word zero is traced. Root an interface
/// object across allocation so a moving collection updates the copied pointer.
/// The caller must publish or root the returned box before another safepoint.
pub(crate) unsafe fn copy_scalar_pair(source: *const i64) -> *mut u8 {
    // SAFETY: The pair allocation contains two aligned words. Its layout selects reference access
    // for the traced first word.
    let mask = unsafe { pair_ref_mask(source) };
    // SAFETY: The pair allocation contains two aligned words. Its layout selects reference access
    // for the traced first word.
    let (first, second) = unsafe { (source.read(), source.add(1).read()) };
    // A successful bump allocation cannot collect: both source words remain
    // valid, and no root registration/old-region indexing is needed. The
    // collecting fallback below retains its existing roots and reloads.
    if let Some([pair]) =
        crate::gc::array_pair_layout(mask).and_then(|layout| crate::gc::try_alloc_young([layout]))
    {
        // SAFETY: The pair allocation contains two aligned words. Its layout selects reference
        // access for the traced first word.
        unsafe { store_pair(pair.cast(), first, second, mask) };
        return pair;
    }
    let object = (mask & 1 != 0).then(|| crate::gc::GcRootHandle::new(first as *mut u8));
    let pair = crate::gc::array_pair_layout(mask).map_or_else(
        // SAFETY: The constructor selects the matching payload layout and initializes its
        // fields before exposing it or reaching a GC point.
        || unsafe { willow_alloc_with_layout(GcObjectKind::Enum, 0, 16, mask) },
        crate::gc::alloc_young_layout_slow,
    );
    if !pair.is_null() {
        // SAFETY: The pair allocation contains two aligned words. Its layout selects reference
        // access for the traced first word.
        unsafe {
            store_pair(
                pair.cast(),
                object.as_ref().map_or(first, |root| root.load() as i64),
                second,
                mask,
            )
        };
    }
    pair
}

/// Number of elements in `arr`. Raises for a null array and for a malformed
/// negative length: generated length reads guard both on their fast path and
/// call here only to raise, so the call must not return normally for either.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_len(arr: *mut u8) -> i64 {
    // SAFETY: Forwarded caller contract.
    unsafe { array_len(arr) }
}

/// [`willow_array_len`] for runtime callers, which are already inside an FFI
/// boundary: entering a nested one per length read would repeat the
/// native-frame bookkeeping and unwind guard on every call.
///
/// # Safety
/// Same contract as [`willow_array_len`].
pub(crate) unsafe fn array_len(arr: *mut u8) -> i64 {
    if arr.is_null() {
        raise_with("cannot take the length of a null array");
        return 0;
    }
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let len = unsafe { handle_word(arr, H_LEN) };
    if len < 0 {
        raise_with(&format!("malformed array: negative length {len}"));
        return 0;
    }
    len
}

/// Read scalar bits at `index`, zero-extending narrow storage. Callers interpret
/// the bits according to the element type, including signed narrow integers.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// For reference elements this operation also writes the handle's exposure flags.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_get(arr: *mut u8, index: i64) -> i64 {
    if !check_bounds(arr, index) {
        return 0;
    }
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    unsafe { expose_pair_storage(arr) };
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    unsafe { load_buffer_slot(handle_buffer(arr), index, handle_word(arr, H_IS_REF)) }
}

/// Write the low element-width bits of `value` at `index`.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// For reference elements, `value` must be null or a current GC payload of the element type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_set(arr: *mut u8, index: i64, value: i64) {
    if !check_bounds(arr, index) {
        return;
    }
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    unsafe { expose_pair_storage(arr) };
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let flags = unsafe { handle_word(arr, H_IS_REF) };
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let buffer = unsafe { handle_buffer(arr) };
    // SAFETY: The GC word-aligns this live buffer. The index and element width select storage
    // within its capacity.
    unsafe { store_buffer_slot(buffer, index, value, flags) };
}

/// Return the address of the element-width slot at `index`.
///
/// Used by compiler-generated `&xs[i]` / `&mut xs[i]` reference calls. NOTE: the
/// returned address points into the current buffer; a `push` that grows the
/// array reallocates the buffer and invalidates any address taken earlier.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// Use the result only at the element width while the same buffer stays live and stationary.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_element_addr(arr: *mut u8, index: i64) -> *mut u8 {
    if !check_bounds(arr, index) {
        return std::ptr::null_mut();
    }
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    unsafe { expose_pair_storage(arr) };
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    unsafe { element_slot(handle_buffer(arr), index, handle_word(arr, H_IS_REF)) }
}

/// Capture the allocation that currently owns an indexed reference. Unlike
/// the array handle, this allocation remains the same owner after a resize.
/// Generated code roots the returned base and keeps the original index, then
/// recomputes the interior address after any moving collection or suspension.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// Root the returned owner and reload it after collection before computing a slot address.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_reference_owner(arr: *mut u8, index: i64) -> *mut u8 {
    if !check_bounds(arr, index) {
        return std::ptr::null_mut();
    }
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    unsafe { expose_pair_storage(arr) };
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    unsafe { handle_buffer(arr) }
}

/// Exposing a reference slot or accepting externally owned storage forbids
/// transferring pair boxes on the next growth. It does not affect scalar arrays.
unsafe fn expose_pair_storage(arr: *mut u8) {
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let flags = unsafe { handle_word(arr, H_IS_REF) };
    if is_reference(flags) {
        // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
        unsafe { set_handle_word(arr, H_IS_REF, flags | PAIR_STORAGE_EXPOSED) };
    }
}

/// Change allocated logical storage while retaining the stable array handle.
/// `pair_mask` is -1 for word elements, or the GC mask of a two-word slot.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// A nonnegative pair mask requires pair storage with that mask and live boxes in
/// the slots to be popped. Use -1 for ordinary elements.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_resize(arr: *mut u8, length: i64, pair_mask: i64) {
    if arr.is_null() || length < 0 || !(-1..=1).contains(&pair_mask) {
        raise_with("invalid array storage resize");
        return;
    }
    // `Array.push` lowers here once per element: a stack root keeps that path
    // free of the root registry and of a heap-allocated handle.
    let mut root = arr;
    let slot = &raw mut root;
    // SAFETY: The slot storage stays live and stationary through the matching pop, including
    // during collection.
    unsafe { crate::gc::push_root(slot) };
    // SAFETY: This initialized slot stays registered and live until the pop below.
    unsafe { resize_rooted(slot, length, pair_mask) };
    // The removed shadow-stack entry belongs to this operation.
    crate::gc::pop_roots(1);
}

/// # Safety
/// `root` must point to a registered, writable slot holding a live array
/// handle throughout this call. A moving collector can rewrite it at GC points.
unsafe fn resize_rooted(root: *mut *mut u8, length: i64, pair_mask: i64) {
    // SAFETY: The slot stays initialized. Each load runs while the mutator
    // owns the slot and reads the handle left by the most recent collection.
    let arr = || unsafe { std::ptr::read_volatile(root) };
    // Reject impossible requests before incremental growth can allocate or
    // mutate the array. Generated scalar in-capacity resizes stay inline;
    // this check belongs only to their runtime fallback.
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    if buffer_bytes(length, unsafe { handle_word(arr(), H_IS_REF) }).is_none() {
        raise_with(&format!("array capacity too large: {length}"));
        return;
    }
    // SAFETY: Each call reads the current handle from the registered root slot.
    let current_len = || unsafe { array_len(arr()) };
    while current_len() < length {
        let value = if pair_mask < 0 {
            0
        } else {
            // SAFETY: The constructor selects the matching payload layout and initializes its
            // fields before exposing it or reaching a GC point.
            let pair =
                unsafe { willow_alloc_with_layout(GcObjectKind::Enum, 0, 16, pair_mask as u64) };
            if pair.is_null() {
                return;
            }
            pair as i64
        };
        let previous = current_len();
        array_push(arr(), value, pair_mask >= 0, true);
        if current_len() == previous {
            return;
        }
    }
    while current_len() > length {
        if pair_mask < 0 {
            // SAFETY: arr() reloads the registered handle after any collection. The resize
            // caller supplies the matching element representation.
            unsafe { willow_array_pop(arr()) };
        } else {
            let mut words = [0_i64; 2];
            // SAFETY: The root supplies the current pair array; words is a disjoint two-i64
            // stack array.
            unsafe { willow_array_pop_scalar_pair_into(arr(), words.as_mut_ptr()) };
        }
    }
}

/// Append `value`, growing the buffer (doubling, min 4) when full.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// For reference elements, `value` must be null or a current GC payload of the element type.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_push(arr: *mut u8, value: i64) {
    array_push(arr, value, false, false);
}

/// Pair boxes are mutable element storage, not shared referents. A captured
/// element reference belongs to the old buffer, so growth copies each pair's
/// bits into independent storage in the new buffer.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// The reference buffer must store GC pair boxes with two aligned, initialized words
/// and the element type's tracing layout. The incoming word must point to such a box.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_push_scalar_pairs(arr: *mut u8, value: i64) {
    array_push(arr, value, true, false);
}

/// Compiler-only ownership transfer: `value` is a freshly boxed inline pair
/// with no storage alias outside this call. Captured element references still
/// force independent copies on growth; the ordinary public entry remains conservative.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// The reference buffer must store GC pair boxes with two aligned, initialized words
/// and the element type's tracing layout. The incoming word must transfer a freshly allocated
/// box of that layout with no other storage aliases.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_push_owned_pair(arr: *mut u8, value: i64) {
    array_push(arr, value, true, true);
}

/// Allocate the grown buffer of `array_push` on the allocation slow path,
/// which may collect. A TLAB-sized buffer stays young like the fast path:
/// a TLAB miss is not survival evidence, and allocating it old would leave
/// only a major cycle to reclaim it. Returns the reloaded handle and value
/// with the buffer, or `None` after raising.
fn grow_rooted(
    mut arr: *mut u8,
    mut value: i64,
    len: i64,
    new_cap: i64,
    flags: i64,
    copy_pairs: bool,
) -> Option<(*mut u8, i64, *mut u8)> {
    // Root the handle and the (possibly reference) value across the buffer
    // allocation, which may trigger a collection. The old buffer stays
    // reachable through the rooted handle. Only root the pushed value when
    // it is a GC pointer — rooting a scalar word (e.g. an i64 like 42) would
    // make the collector treat it as an object pointer and crash. Use the
    // updated root slots after allocation in case the collector moves them.
    let arr_root = crate::gc::GcRootHandle::new(arr);
    let value_root = is_reference(flags).then(|| crate::gc::GcRootHandle::new(value as *mut u8));
    let mut new_buf = match crate::gc::array_buffer_layout(new_cap, flags) {
        Some(layout) => {
            let buffer = crate::gc::alloc_young_layout_slow(layout);
            if buffer.is_null() {
                raise_with("array buffer allocation failed");
            } else {
                // SAFETY: The fresh buffer has a word-aligned length field.
                unsafe { set_buffer_len(buffer, 0) };
            }
            buffer
        }
        None => alloc_buffer(new_cap, flags),
    };
    if new_buf.is_null() {
        return None;
    }
    arr = arr_root.load();
    if copy_pairs {
        let buffer_root = crate::gc::GcRootHandle::new(new_buf);
        for index in 0..len {
            // Reload through the rooted handle on every iteration: cloning
            // can move both buffers and all still-live boxes.
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            let source = unsafe { *buf_slot(handle_buffer(arr), index) };
            let pair = if source == 0 {
                std::ptr::null_mut()
            } else {
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                let pair = unsafe { copy_scalar_pair(source as *const i64) };
                if pair.is_null() {
                    return None;
                }
                pair
            };
            arr = arr_root.load();
            new_buf = buffer_root.load();
            // SAFETY: The rooted handle owns the source buffer. The destination has capacity for
            // the live elements and the appended slot.
            unsafe {
                store_buffer_slot(new_buf, index, pair as i64, 1);
                // Keep each initialized pair visible to GC during the next
                // allocation, while leaving the old buffer unchanged.
                set_buffer_len(new_buf, index + 1);
            }
        }
    }
    if let Some(root) = value_root {
        value = root.load() as i64;
    }
    Some((arr, value, new_buf))
}

fn array_push(mut arr: *mut u8, mut value: i64, copy_pairs: bool, owned_pair: bool) {
    if arr.is_null() {
        raise_with("cannot push to a null array");
        return;
    }
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let len = unsafe { handle_word(arr, H_LEN) };
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let cap = unsafe { handle_word(arr, H_CAP) };
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let flags = unsafe { handle_word(arr, H_IS_REF) };
    let is_ref = is_reference(flags);

    if len == cap {
        let clone_pairs =
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            copy_pairs && unsafe { handle_word(arr, H_IS_REF) } & PAIR_STORAGE_EXPOSED != 0;
        let new_cap = if cap == 0 { 4 } else { cap.saturating_mul(2) };
        // A buffer that fits the TLAB is bump-allocated young without a GC
        // point, so `arr` and `value` stay valid and need no roots. Cloning
        // exposed pairs can allocate per element and requires the rooted path.
        let young = (!clone_pairs)
            .then(|| crate::gc::array_buffer_layout(new_cap, flags))
            .flatten()
            .and_then(|layout| crate::gc::try_alloc_young([layout]));
        let new_buf = match young {
            Some([buffer]) => buffer,
            None => {
                let Some((moved_arr, moved_value, buffer)) =
                    grow_rooted(arr, value, len, new_cap, flags, clone_pairs)
                else {
                    return;
                };
                (arr, value) = (moved_arr, moved_value);
                buffer
            }
        };
        // SAFETY: The handle owns the old buffer and is current after either allocation path.
        // The new buffer has the same storage flags and room for the old elements plus one.
        unsafe {
            let old_buf = handle_buffer(arr);
            if is_ref && !clone_pairs {
                // The source buffer remains owned by arr throughout this
                // non-safepoint copy. The handle barrier below publishes its
                // deletion to SATB, retaining every copied old edge. A fresh
                // destination has no deleted edges of its own; one young edge
                // per remembered card suffices: a small destination is
                // remembered whole, a large old one per card.
                let mut young_card = None;
                for i in 0..len {
                    let value = crate::gc::load_gc_reference(buf_slot(old_buf, i).cast());
                    crate::gc::store_gc_reference(buf_slot(new_buf, i).cast(), value);
                    let card = i as usize / crate::gc::ARRAY_CARD_SLOTS;
                    if young_card != Some(card) && !value.is_null() {
                        // The loaded edge points to a live object; this copy cannot safepoint.
                        if crate::gc::GcHeader::live_generation(value)
                            == willow_abi::gc_header::GENERATION_YOUNG
                        {
                            young_card = Some(card);
                            write_barrier_array_slot(
                                new_buf,
                                buf_slot(new_buf, i).cast(),
                                std::ptr::null_mut(),
                                value,
                            );
                        }
                    }
                }
            } else if !is_ref && len != 0 {
                // Both buffers have a first element here. Copy the initialized scalar bytes
                // between distinct allocations, with no safepoint before the copy finishes.
                std::ptr::copy_nonoverlapping(
                    element_slot(old_buf, 0, flags),
                    element_slot(new_buf, 0, flags),
                    len as usize * element_width(flags),
                );
            }
            set_buffer_len(new_buf, len);
            write_barrier_object_owner(
                arr,
                old_buf,
                new_buf,
                GcStoreDestination::ContainerInternal as i64,
            );
            set_handle_word(arr, H_BUF, new_buf as i64);
            set_handle_word(arr, H_CAP, new_cap);
            // The old captured buffer keeps its own cloned storage. No view
            // has observed the new buffer yet.
            set_handle_word(arr, H_IS_REF, flags & !PAIR_STORAGE_EXPOSED);
        }
    }
    // SAFETY: Spare capacity and successful young growth reach no safepoint.
    // grow_rooted reloads the handle and any reference value after its last allocation.
    // The current buffer has room at len, and the barriers and stores below cannot safepoint.
    unsafe {
        if !owned_pair {
            expose_pair_storage(arr);
        }
        let buffer = handle_buffer(arr);
        let existing = if copy_pairs {
            *buf_slot(buffer, len)
        } else {
            0
        };
        if existing != 0 {
            // Pair pushes supply a GC-aligned box with two initialized words
            // (resize supplies a zeroed two-word box). Pair pop clears both words
            // but keeps the box in the buffer's traced prefix. Thus the nonzero
            // slot still owns live, aligned storage of the same pair layout.
            // Reusing it preserves outstanding element references.
            let source = value as *const i64;
            let target = existing as *mut i64;
            let mask = pair_ref_mask(target);
            debug_assert_eq!(
                mask,
                pair_ref_mask(source),
                "array pair layout must be stable"
            );
            store_pair(target, source.read(), source.add(1).read(), mask);
        } else {
            store_buffer_slot(buffer, len, value, flags);
        }
        // The traced prefix is a high-water mark (see `willow_array_pop`).
        set_buffer_len(buffer, buffer_len(buffer).max(len + 1));
        set_handle_word(arr, H_LEN, len + 1);
    }
}

/// Remove and return the last element. Aborts on an empty array. The freed slot
/// is nulled so a popped reference can be reclaimed.
///
/// The buffer's traced prefix is not shrunk: it is a high-water mark of
/// initialized slots. A `&mut` element reference keeps the buffer alive as its
/// cell base and may store into the slot after the pop; the
/// store's barrier only remembers the buffer, so the slot must stay inside the
/// traced prefix for its referent to be kept and relocated. Tracing costs
/// O(high-water) <= capacity, and a value stored through such a reference is
/// retained until the slot is overwritten or the buffer dies.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_pop(arr: *mut u8) -> i64 {
    if arr.is_null() {
        raise_with("cannot pop from a null array");
        return 0;
    }
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let len = unsafe { handle_word(arr, H_LEN) };
    if len == 0 {
        raise_with("cannot pop from an empty array");
        return 0;
    }
    let last = len - 1;
    // SAFETY: The checked positive length selects an initialized last slot in the handle's live
    // buffer.
    unsafe {
        let buffer = handle_buffer(arr);
        let flags = handle_word(arr, H_IS_REF);
        let value = load_buffer_slot(buffer, last, flags);
        store_buffer_slot(buffer, last, 0, flags); // allow the GC to reclaim it
        set_handle_word(arr, H_LEN, last);
        value
    }
}

/// Pop pair words into caller-owned storage without returning an alias to
/// mutable element storage. The box stays in its buffer slot and remains traced
/// until that buffer dies, so references captured before later arguments (or
/// already held by a callee) survive removal and observe a subsequent push.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// The reference buffer must store GC pair boxes with two aligned, initialized words
/// and the element type's tracing layout. A populated last slot must contain a non-null box.
/// Even for an empty or null array, `out` must exclusively address two consecutive,
/// aligned writable i64 words disjoint from the array and its elements.
///
/// ```compile_fail,E0133
/// let mut words = [0_i64; 2];
/// willow_runtime::array::willow_array_pop_scalar_pair_into(std::ptr::null_mut(), words.as_mut_ptr());
/// ```
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_pop_scalar_pair_into(arr: *mut u8, out: *mut i64) {
    // SAFETY: The caller lends two consecutive writable i64 words even on the error path.
    unsafe {
        out.write(0);
        out.add(1).write(0);
    }
    if arr.is_null() {
        raise_with("cannot pop from a null array");
        return;
    }
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let len = unsafe { handle_word(arr, H_LEN) };
    if len == 0 {
        raise_with("cannot pop from an empty array");
        return;
    }
    // SAFETY: The last slot owns a live two-word GC pair of the declared layout. Both
    // initialized words can be copied to the disjoint output before the box is cleared.
    unsafe {
        let pair = *buf_slot(handle_buffer(arr), len - 1) as *mut i64;
        out.write(pair.read());
        out.add(1).write(pair.add(1).read());
        // Match ordinary slot clearing, while retaining the slot's allocation.
        store_pair(pair, 0, 0, pair_ref_mask(pair));
        set_handle_word(arr, H_LEN, len - 1);
    }
}

fn check_bounds(arr: *mut u8, index: i64) -> bool {
    if arr.is_null() {
        raise_with("cannot index a null array");
        return false;
    }
    // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
    let len = unsafe { handle_word(arr, H_LEN) };
    if index < 0 || index >= len {
        raise_with(&format!(
            "array index out of bounds: the length is {len} but the index is {index}"
        ));
        return false;
    }
    true
}

/// Raise a recoverable language fault and let the generated caller branch
/// before observing the neutral return value.
fn raise_with(message: &str) {
    crate::panic_context::raise_language_message(message);
}

/// # Safety
/// For the string kind, a nonzero word must denote a current immutable WillowString.
pub(crate) unsafe fn element_word_to_string(word: i64, kind: i64) -> String {
    use willow_abi::collection_element as element;
    match kind {
        element::I8 => (word as i8).to_string(),
        element::I16 => (word as i16).to_string(),
        element::I32 => (word as i32).to_string(),
        element::U8 => (word as u8).to_string(),
        element::U16 => (word as u16).to_string(),
        element::U32 => (word as u32).to_string(),
        element::U64 => (word as u64).to_string(),
        element::CHAR => u32::try_from(word)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or_else(|| crate::failure::fatal_invariant("invalid collection character"))
            .to_string(),
        element::F64 => crate::math::format_f64_shortest(f64::from_bits(word as u64)),
        element::BOOL => if word != 0 { "true" } else { "false" }.to_string(),
        element::STRING => {
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            let s = unsafe { crate::string::willow_string_as_str(word as *const u8) };
            format!("{s:?}")
        }
        _ => word.to_string(),
    }
}

/// Debug display of a whole array: `[1, 2, 3]` (strings quoted, f64 shortest
/// round-trip). `elem_kind` uses the shared `collection_element` tags.
/// Returns a newly allocated WillowString.
///
/// # Safety
/// A non-null `arr` must be a current, live Willow array handle with its initialized
/// backing buffer. Exclude conflicting accesses for the duration of this operation.
/// `elem_kind` must match the stored words. String elements must be null or live WillowStrings.
#[unsafe(no_mangle)]
#[willow_runtime_macros::ffi_boundary]
pub unsafe extern "C" fn willow_array_to_string(arr: *mut u8, elem_kind: i64) -> *mut u8 {
    if arr.is_null() {
        raise_with("cannot convert a null array to String");
        return std::ptr::null_mut();
    }
    // SAFETY: The caller supplies a live array with this element kind.
    let len = unsafe { array_len(arr) };
    let mut out = String::from("[");
    for index in 0..len {
        if index > 0 {
            out.push_str(", ");
        }
        // SAFETY: No GC point intervenes while formatting. The word read at this
        // index has the declared kind, including a live or null string pointer.
        unsafe {
            out.push_str(&element_word_to_string(
                willow_array_get(arr, index),
                elem_kind,
            ))
        };
    }
    out.push(']');
    crate::string::willow_string_from_str(&out)
}

#[cfg(test)]
#[path = "array/packed_tests.rs"]
mod packed_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{
        isolated_runtime, willow_gc_collect, willow_gc_init, willow_pop_roots, willow_push_root,
    };
    use crate::string::{willow_string_as_str, willow_string_from_str};

    #[test]
    fn array_width_collection_display_and_snapshot_layout() {
        use willow_abi::collection_element as element;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for (kind, value, width, expected) in [
            (element::I8, 128, 1, "-128"),
            (element::I16, 32768, 2, "-32768"),
            (element::I32, 2147483648, 4, "-2147483648"),
            (element::I64, i64::MIN, 8, "-9223372036854775808"),
            (element::U8, -1, 1, "255"),
            (element::U16, -1, 2, "65535"),
            (element::U32, -1, 4, "4294967295"),
            (element::U64, -1, 8, "18446744073709551615"),
            (element::CHAR, '界' as i64, 4, "界"),
            (element::CHAR, '🦀' as i64, 4, "🦀"),
            (element::F64, 1.5_f64.to_bits() as i64, 8, "1.5"),
            (element::BOOL, 1, 8, "true"),
        ] {
            let flags = element::storage_flags(kind, false);
            assert_eq!(element_width(flags), width);
            assert!(!is_reference(flags));
            // SAFETY: The fixture supplies scalar kind tags for these numeric words.
            assert_eq!(unsafe { element_word_to_string(value, kind) }, expected);
            // SAFETY: The width flags here describe untraced scalar storage.
            let array = unsafe { willow_array_new(1, flags) };
            // SAFETY: The fixture retains the array and stores a word matching its element
            // layout.
            unsafe { willow_array_set(array, 0, value) };
            // SAFETY: The array was just populated using the same element kind and storage
            // flags.
            let text = unsafe { willow_array_to_string(array, kind) };
            assert_eq!(
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                unsafe { willow_string_as_str(text) },
                format!("[{expected}]")
            );
        }
        for kind in [element::STRING, element::OBJECT] {
            let flags = element::storage_flags(kind, true);
            assert_eq!(flags, 1);
            assert_eq!(element_width(flags), 8);
            assert!(is_reference(flags));
        }
        let text = willow_string_from_str("quoted");
        assert_eq!(
            // SAFETY: text is the string just allocated above.
            unsafe { element_word_to_string(text as i64, element::STRING) },
            "\"quoted\""
        );
    }

    #[test]
    fn array_width_storage_operations_and_captured_owner() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for width in [1, 2, 4, 8] {
            let flags = willow_abi::array_layout::storage_flags(width, false);
            let mask = if width == 8 {
                -1
            } else {
                (1_i64 << (width * 8)) - 1
            };
            // SAFETY: The width flags here describe untraced scalar storage.
            let mut array = unsafe { willow_array_new(3, flags) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut array) };
            for (index, value) in [-1, i64::MIN, 42].into_iter().enumerate() {
                // SAFETY: The fixture retains the array and stores a word matching its
                // element layout.
                unsafe { willow_array_set(array, index as i64, value) };
                assert_eq!(
                    // SAFETY: The fixture retains the array allocated for these accesses.
                    unsafe { willow_array_get(array, index as i64) },
                    value & mask
                );
            }
            // SAFETY: The index selects initialized storage in the current array; the
            // returned address is used while its buffer is live.
            let first = unsafe { willow_array_element_addr(array, 0) };
            // SAFETY: The index selects initialized storage in the current array; the
            // returned address is used while its buffer is live.
            let second = unsafe { willow_array_element_addr(array, 1) };
            // SAFETY: Both pointers denote slots in the same allocated buffer, so their byte
            // distance is defined.
            assert_eq!(unsafe { second.byte_offset_from(first) }, width as isize);
            // SAFETY: The index selects initialized storage in the current array; the
            // returned address is used while its buffer is live.
            let mut owner = unsafe { willow_array_reference_owner(array, 2) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut owner) };
            // SAFETY: The fixture retains the array and stores a word matching its element
            // layout.
            unsafe { willow_array_push(array, -1) };
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            assert_ne!(unsafe { handle_buffer(array) }, owner);
            // SAFETY: The GC word-aligns this live buffer. The index and element width select
            // storage within its capacity.
            assert_eq!(unsafe { load_buffer_slot(owner, 2, flags) }, 42);
            // SAFETY: The fixture retains the array allocated for these accesses. The live
            // array has the storage layout selected by the resize mask.
            unsafe {
                assert_eq!(willow_array_pop(array), mask);
                assert_eq!(willow_array_len(array), 3);
                willow_array_resize(array, 33, -1);
                assert_eq!(willow_array_get(array, 2), 42);
            }
            for index in 3..33 {
                // SAFETY: The fixture retains the array allocated for these accesses.
                assert_eq!(unsafe { willow_array_get(array, index) }, 0);
            }
            // SAFETY: The fixture retains the array and stores a word matching its element
            // layout.
            unsafe { willow_array_set(array, 32, -1) };
            // SAFETY: The fixture retains the array allocated for these accesses.
            let mut copy = unsafe { willow_array_copy(array) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut copy) };
            // SAFETY: The fixture retains the array and stores a word matching its element
            // layout.
            unsafe { willow_array_set(array, 32, 7) };
            willow_gc_collect();
            // SAFETY: The fixture retains the array allocated for these accesses.
            unsafe {
                assert_eq!(willow_array_get(copy, 32), mask);
                assert_eq!(willow_array_get(array, 32), 7);
            }
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            assert_eq!(unsafe { handle_word(copy, H_IS_REF) }, flags);
            // SAFETY: The GC word-aligns this live buffer. The index and element width select
            // storage within its capacity.
            assert_eq!(unsafe { load_buffer_slot(owner, 2, flags) }, 42);
            // SAFETY: The live array has the storage layout selected by the resize mask.
            unsafe {
                willow_array_resize(array, 0, -1);
                willow_array_resize(array, 33, -1);
            }
            for index in 0..33 {
                // SAFETY: The fixture retains the array allocated for these accesses.
                assert_eq!(unsafe { willow_array_get(array, index) }, 0);
            }
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(3) };
        }
    }

    #[test]
    fn array_width_allocation_bytes_and_alignment() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for width in [1, 2, 4, 8] {
            let flags = willow_abi::array_layout::storage_flags(width, false);
            for length in [0, 1, 3, 1000, 100_000] {
                let logical = 8 + length * i64::from(width);
                assert_eq!(buffer_bytes(length, flags), Some(logical));
                // SAFETY: The width flags here describe untraced scalar storage.
                let array = unsafe { willow_array_new_literal(length, flags) };
                // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
                unsafe {
                    let buffer = handle_buffer(array);
                    let descriptor = crate::gc::GcHeader::live_descriptor(buffer);
                    let expected = (crate::gc::GC_HEADER_SIZE + logical as usize)
                        .next_multiple_of(std::mem::align_of::<crate::gc::GcHeader>());
                    assert_eq!(descriptor.size, expected as u64);
                    assert_eq!(descriptor.type_id, 0);
                    assert_eq!(descriptor.gc_ref_mask, 0);
                    assert_eq!(buffer as usize % 8, 0);
                    eprintln!(
                        "array-width bytes width={width} n={length} logical={logical} allocated={expected}"
                    );
                }
            }
            assert_eq!(buffer_bytes(-1, flags), None);
            assert_eq!(buffer_bytes(i64::MAX, flags), None);
        }
    }

    #[test]
    fn array_width_scalar_bytes_survive_collections_and_growth() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for width in [1, 2, 4] {
            let flags = willow_abi::array_layout::storage_flags(width, false);
            let pointer = willow_string_from_str("untraced scalar bits") as usize;
            // SAFETY: The width flags here describe untraced scalar storage.
            let mut array = unsafe { willow_array_new(0, flags) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut array) };
            let depth = crate::gc::willow_root_depth();
            let mask = (1_u64 << (width * 8)) - 1;
            let values: Vec<_> = (0..32)
                .map(|index| ((pointer as u64).rotate_right(index * 8) & mask) as i64)
                .collect();
            crate::gc::set_gc_stress_for_test(Some("alloc"));
            for &value in &values {
                // SAFETY: The fixture retains the array and stores a word matching its
                // element layout.
                unsafe { willow_array_push(array, value) };
            }
            crate::gc::set_gc_stress_for_test(None);
            crate::gc::willow_gc_minor_collect();
            willow_gc_collect();
            assert_eq!(crate::gc::willow_root_depth(), depth);
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            assert_eq!(unsafe { handle_word(array, H_IS_REF) }, flags);
            for (index, &value) in values.iter().enumerate() {
                // SAFETY: The fixture retains the array allocated for these accesses.
                assert_eq!(unsafe { willow_array_get(array, index as i64) }, value);
            }
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
    }

    #[test]
    fn array_literal_invalid_lengths_and_exhaustion_never_return() {
        const KEY: &str = "WILLOW_TEST_LITERAL_FAILURE";
        if let Ok(case) = std::env::var(KEY) {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { willow_gc_init() };
            let len = match case.as_str() {
                "negative" => -1,
                "overflow" => i64::MAX,
                "exhaustion" => 1000,
                _ => unreachable!(),
            };
            // SAFETY: Flag 0 selects untraced word storage.
            unsafe { willow_array_new_literal(len, 0) };
            eprintln!("LITERAL_FAILURE_RETURNED");
            return;
        }
        for case in ["negative", "overflow", "exhaustion"] {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "array::tests::array_literal_invalid_lengths_and_exhaustion_never_return",
                    "--nocapture",
                ])
                .env(KEY, case)
                .env("WILLOW_GC_MEMORY_LIMIT", "1")
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{case}: {stderr}");
            assert!(!stderr.contains("LITERAL_FAILURE_RETURNED"), "{stderr}");
            assert!(
                stderr.contains(if case == "exhaustion" {
                    "runtime resource exhausted:"
                } else {
                    "invalid constant array literal length"
                }),
                "{case}: {stderr}"
            );
        }
    }

    /// `Array.push` lowers to a one-element resize. Every reallocation copies
    /// the current elements, so the copies summed over n pushes measure the
    /// amortized cost: doubling keeps them below 2n for every n, scalar or
    /// reference elements, so each push is amortized O(1).
    #[test]
    fn push_growth_copies_stay_linear_in_pushes() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for elem_is_ref in [0, 1] {
            for count in [1_i64, 5, 64, 1000, 4096, 100_000] {
                // SAFETY: The fixture selects only word-sized scalar or reference storage.
                let mut array = unsafe { willow_array_new(0, elem_is_ref) };
                // SAFETY: The initialized slot stays at this address until its registration
                // is popped; collection may rewrite it there.
                unsafe { willow_push_root(&raw mut array) };
                let mut copied = 0;
                let mut reallocations = 0;
                for i in 0..count {
                    let (len, cap) =
                        // SAFETY: The live handle owns its buffer and the ABI fixes these word
                        // offsets.
                        unsafe { (handle_word(array, H_LEN), handle_word(array, H_CAP)) };
                    assert_eq!(len, i);
                    if len == cap {
                        copied += len;
                        reallocations += 1;
                    }
                    let value = if elem_is_ref == 0 { i } else { 0 };
                    // SAFETY: The live array has the storage layout selected by the resize
                    // mask.
                    unsafe { willow_array_resize(array, i + 1, -1) };
                    if elem_is_ref == 0 {
                        // SAFETY: The fixture retains the array and stores a word matching
                        // its element layout.
                        unsafe { willow_array_set(array, i, value) };
                    }
                }
                // SAFETY: The fixture retains the array allocated for these accesses.
                assert_eq!(unsafe { willow_array_len(array) }, count);
                assert!(copied < 2 * count, "count={count} copied={copied}");
                // Geometric growth: one reallocation per doubling.
                assert!(
                    reallocations <= 2 + (count as f64).log2().ceil() as i64,
                    "count={count} reallocations={reallocations}"
                );
                eprintln!(
                    "array push elem_is_ref={elem_is_ref} count={count} copied={copied} reallocations={reallocations}"
                );
                // SAFETY: The removed shadow-stack entries belong to this operation and their
                // slot storage is still live.
                unsafe { willow_pop_roots(1) };
            }
        }
    }

    #[test]
    fn array_shared_layout_matches_allocated_payload() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 0 selects untraced word storage.
        let array = unsafe { willow_array_new(3, 0) };
        // SAFETY: The fixture retains the array and stores a word matching its element
        // layout.
        unsafe { willow_array_push(array, 42) };
        // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
        unsafe {
            assert_eq!(WORD_BYTES as usize, std::mem::size_of::<i64>());
            assert_eq!(handle_word(array, H_LEN), 4);
            assert!(handle_word(array, H_CAP) > 4);
            assert_eq!(handle_word(array, H_IS_REF), 0);
            let buffer = handle_buffer(array);
            assert_eq!(buffer_len(buffer), 4);
            assert_eq!(buf_slot(buffer, 0).byte_offset_from(buffer), 8);
            assert_eq!(*buf_slot(buffer, 3), 42);
            assert_eq!(HANDLE_MASK, 1 << H_BUF);
            use willow_abi::schema::SharedLayout;
            assert_eq!(SharedLayout::ArrayHandle.size(8), 32);
            for (field, value) in [4, handle_word(array, H_CAP), 0, buffer as i64]
                .into_iter()
                .enumerate()
            {
                let offset = SharedLayout::ArrayHandle.field_offset(field, 8).unwrap();
                // SAFETY: The ABI offsets are word multiples in the GC word-aligned array handle.
                assert_eq!(*array.add(offset as usize).cast::<i64>(), value);
            }
            assert_eq!(
                SharedLayout::ArrayBufferHeader.size(8),
                buf_slot(buffer, 0).byte_offset_from(buffer) as u32
            );
        }
    }

    #[test]
    fn array_unit_01_new_sets_length() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 0 selects untraced word storage.
        let arr = unsafe { willow_array_new(3, 0) };
        assert!(!arr.is_null());
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_len(arr) }, 3);
    }

    #[test]
    fn array_unit_02_zero_length_array() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: The constructor selects scalar word storage. This fixture uses the resulting
        // handle before any collection.
        unsafe {
            let arr = willow_array_new(0, 0);
            assert_eq!(willow_array_len(arr), 0);
        }
    }

    #[test]
    fn array_unit_03_scalar_get_set_roundtrip() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 0 selects untraced word storage.
        let arr = unsafe { willow_array_new(4, 0) };
        // SAFETY: The fixture retains the array and stores a word matching its element layout.
        // The fixture retains the array allocated for these accesses.
        unsafe {
            willow_array_set(arr, 0, 10);
            willow_array_set(arr, 1, -20);
            willow_array_set(arr, 3, 99);
            assert_eq!(willow_array_get(arr, 0), 10);
            assert_eq!(willow_array_get(arr, 1), -20);
        }
        // Slot 2 was never written — zero-initialized.
        // SAFETY: The fixture retains the array allocated for these accesses.
        unsafe {
            assert_eq!(willow_array_get(arr, 2), 0);
            assert_eq!(willow_array_get(arr, 3), 99);
        }
    }

    #[test]
    fn array_unit_04_reference_elements_roundtrip() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // Root the array as generated code would (Array is GC-managed): the
        // string allocation below can trigger a collection under GC stress.
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut arr = unsafe { willow_array_new(2, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        let s = willow_string_from_str("hello");
        // SAFETY: The destination is the live fixture array; the element word matches its
        // declared storage and tracing layout.
        unsafe { willow_array_set(arr, 0, s as i64) };
        // SAFETY: The fixture retains the array allocated for these accesses.
        let got = unsafe { willow_array_get(arr, 0) } as *mut u8;
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(got) }, "hello");
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    #[test]
    fn array_unit_05_reference_elements_survive_collection_when_rooted() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut arr = unsafe { willow_array_new(1, 1) };
        // Root the array slot so the collector can reach it.
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        let s = willow_string_from_str("kept");
        // SAFETY: The destination is the live fixture array; the element word matches its
        // declared storage and tracing layout.
        unsafe { willow_array_set(arr, 0, s as i64) };
        willow_gc_collect();
        // SAFETY: The fixture retains the array allocated for these accesses.
        let got = unsafe { willow_array_get(arr, 0) } as *mut u8;
        assert!(
            !got.is_null(),
            "element string must survive GC via array trace"
        );
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(got) }, "kept");
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    #[test]
    fn array_unit_06_large_reference_array_traces_all_elements() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // More than 64 elements: exercises the trace function, not gc_ref_mask.
        let n = 100i64;
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut arr = unsafe { willow_array_new(n, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        for i in 0..n {
            let s = willow_string_from_str(&format!("e{i}"));
            // SAFETY: The destination is the live fixture array; the element word matches its
            // declared storage and tracing layout.
            unsafe { willow_array_set(arr, i, s as i64) };
        }
        willow_gc_collect();
        assert_eq!(
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            unsafe { willow_string_as_str(willow_array_get(arr, 99) as *mut u8) },
            "e99"
        );
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    #[test]
    fn array_unit_07_push_grows_and_reads() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 0 selects untraced word storage.
        let arr = unsafe { willow_array_new(0, 0) };
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_len(arr) }, 0);
        for i in 0..10 {
            // SAFETY: The fixture retains the array and stores a word matching its element
            // layout.
            unsafe { willow_array_push(arr, i * 100) };
        }
        // SAFETY: The fixture retains the array allocated for these accesses.
        unsafe {
            assert_eq!(willow_array_len(arr), 10);
            assert_eq!(willow_array_get(arr, 0), 0);
            assert_eq!(willow_array_get(arr, 9), 900);
        }
    }

    #[test]
    fn array_unit_08_pop_returns_last_and_shrinks() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: The constructor selects scalar word storage. This fixture uses the resulting
        // handle before any collection.
        unsafe {
            let arr = willow_array_new(0, 0);
            willow_array_push(arr, 1);
            willow_array_push(arr, 2);
            willow_array_push(arr, 3);
            assert_eq!(willow_array_pop(arr), 3);
            assert_eq!(willow_array_pop(arr), 2);
            assert_eq!(willow_array_len(arr), 1);
            assert_eq!(willow_array_get(arr, 0), 1);
        }
    }

    #[test]
    fn array_unit_09_pushed_reference_values_survive_gc_across_growth() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut arr = unsafe { willow_array_new(0, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        // Push past several growth points; each push may reallocate the buffer.
        for i in 0..20 {
            let s = willow_string_from_str(&format!("v{i}"));
            // SAFETY: The destination is the live fixture array; the element word matches its
            // declared storage and tracing layout.
            unsafe { willow_array_push(arr, s as i64) };
        }
        willow_gc_collect();
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_len(arr) }, 20);
        assert_eq!(
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            unsafe { willow_string_as_str(willow_array_get(arr, 0) as *mut u8) },
            "v0"
        );
        assert_eq!(
            // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
            unsafe { willow_string_as_str(willow_array_get(arr, 19) as *mut u8) },
            "v19"
        );
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    // Deterministic slot counts isolate tracing cost from allocation and GC timing.
    // The traced prefix is the buffer's high-water mark, bounded by capacity;
    // popped slots stay traced but hold null.
    #[test]
    fn array_trace_work_is_bounded_by_high_water_after_pop_and_regrowth() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for capacity in [16, 256, 4096] {
            // SAFETY: Flag 1 allocates full-width reference slots.
            let mut arr = unsafe { willow_array_new(capacity, 1) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut arr) };
            let value = willow_string_from_str("retained");
            for index in 0..capacity {
                // SAFETY: The destination is the live fixture array; the element word matches
                // its declared storage and tracing layout.
                unsafe { willow_array_set(arr, index, value as i64) };
            }
            for _ in 3..capacity {
                // SAFETY: The fixture retains the array allocated for these accesses.
                assert_eq!(unsafe { willow_array_pop(arr) }, value as i64);
            }
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            let buffer = unsafe { handle_buffer(arr) };
            for length in [3, 0, 1, 8, 16] {
                // SAFETY: The fixture retains the array allocated for these accesses.
                while unsafe { willow_array_len(arr) } > length {
                    // SAFETY: The fixture retains the array allocated for these accesses.
                    unsafe { willow_array_pop(arr) };
                }
                // SAFETY: The fixture retains the array allocated for these accesses.
                while unsafe { willow_array_len(arr) } < length {
                    // SAFETY: The destination is the live fixture array; the element word
                    // matches its declared storage and tracing layout.
                    unsafe { willow_array_push(arr, value as i64) };
                }
                let mut slots = Vec::new();
                let mut children = Vec::new();
                // SAFETY: The live reference buffer and its initialized length meet the registered
                // array tracer's contract.
                unsafe {
                    trace_array_ref(buffer, &mut slots);
                    snapshot_array_ref(buffer, &mut children);
                }
                assert_eq!(slots.len(), capacity as usize, "capacity={capacity}");
                let (live, popped) = children.split_at(length as usize);
                assert_eq!(live, vec![value; length as usize]);
                assert!(popped.iter().all(|child| child.is_null()));
                println!(
                    "capacity={capacity} length={length} trace_slots={} snapshot_children={}",
                    slots.len(),
                    children.len()
                );
                for (index, slot) in slots.into_iter().enumerate() {
                    // SAFETY: The GC word-aligns this live buffer. The index and element width
                    // select storage within its capacity.
                    assert_eq!(slot, unsafe { buf_slot(buffer, index as i64).cast() });
                }
            }
            willow_gc_collect();
            assert_eq!(
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                unsafe { willow_string_as_str(willow_array_get(arr, 0) as *mut u8) },
                "retained"
            );
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
    }

    #[test]
    fn array_trace_preserves_captured_reference_buffer_after_growth() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut arr = unsafe { willow_array_new(1, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        let value = willow_string_from_str("captured");
        // SAFETY: The destination is the live fixture array; the element word matches its
        // declared storage and tracing layout.
        unsafe { willow_array_set(arr, 0, value as i64) };
        // SAFETY: The index selects initialized storage in the current array; the returned
        // address is used while its buffer is live.
        let mut captured = unsafe { willow_array_reference_owner(arr, 0) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut captured) };
        // SAFETY: The destination is the live fixture array; the element word matches its
        // declared storage and tracing layout.
        unsafe { willow_array_push(arr, value as i64) };
        // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
        assert_ne!(captured, unsafe { handle_buffer(arr) });
        // SAFETY: The fixture retains the array allocated for these accesses.
        unsafe {
            willow_array_pop(arr);
            willow_array_pop(arr);
        }
        willow_gc_collect();
        let mut children = Vec::new();
        // SAFETY: The live reference buffer and its initialized length meet the registered array
        // tracer's contract.
        unsafe { snapshot_array_ref(captured, &mut children) };
        assert_eq!(children.len(), 1);
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(children[0]) }, "captured");
        children.clear();
        // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
        unsafe { snapshot_array_ref(handle_buffer(arr), &mut children) };
        assert!(children.iter().all(|child| child.is_null()));
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
    }

    #[test]
    fn array_snapshot_length_publication_during_push_pop() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut arr = unsafe { willow_array_new(256, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        let mut value = willow_string_from_str("published");
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut value) };
        for _ in 0..256 {
            // SAFETY: The fixture retains the array allocated for these accesses.
            unsafe { willow_array_pop(arr) };
        }
        // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
        let buffer = unsafe { handle_buffer(arr) } as usize;
        let expected = value as usize;
        let start = std::sync::Barrier::new(2);
        // No allocations/collections while the reader borrows this buffer.
        std::thread::scope(|scope| {
            let reader = scope.spawn(crate::runtime::serving(|| {
                start.wait();
                let mut children = Vec::new();
                for _ in 0..4096 {
                    children.clear();
                    // SAFETY: The live reference buffer and its initialized length meet the
                    // registered array tracer's contract.
                    unsafe { snapshot_array_ref(buffer as *mut u8, &mut children) };
                    assert!(children.len() <= 256);
                    assert!(
                        children
                            .iter()
                            .all(|child| child.is_null() || *child as usize == expected)
                    );
                }
            }));
            start.wait();
            for _ in 0..16 {
                for _ in 0..256 {
                    // SAFETY: The destination is the live fixture array; the element word
                    // matches its declared storage and tracing layout.
                    unsafe { willow_array_push(arr, value as i64) };
                }
                for _ in 0..256 {
                    // SAFETY: The fixture retains the array allocated for these accesses.
                    unsafe { willow_array_pop(arr) };
                }
            }
            reader.join().unwrap();
        });
        // Pops clear slots but keep the high-water prefix.
        // SAFETY: The retained GC buffer begins with its aligned atomic length word.
        assert_eq!(unsafe { buffer_len(buffer as *mut u8) }, 256);
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
    }

    // Regression: pushing scalars while a collection runs during buffer growth
    // must not root the scalar word as an object pointer (previously SIGSEGV'd
    // under GC stress). Stress mode forces a collection on every allocation.
    #[test]
    fn array_unit_10_scalar_push_grow_under_gc_stress() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 0 selects untraced word storage.
        let mut arr = unsafe { willow_array_new(0, 0) }; // scalar (non-reference) array
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        // Guarded because the stress override is global to the test binary.
        let collections = crate::gc::willow_gc_major_collections();
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        for i in 0..12 {
            // SAFETY: The fixture retains the array and stores a word matching its element
            // layout.
            unsafe { willow_array_push(arr, i * 7) }; // crosses several growth points
        }
        crate::gc::set_gc_stress_for_test(None);
        assert!(crate::gc::willow_gc_major_collections() > collections);
        // SAFETY: The fixture retains the array allocated for these accesses.
        unsafe {
            assert_eq!(willow_array_len(arr), 12);
            assert_eq!(willow_array_get(arr, 0), 0);
            assert_eq!(willow_array_get(arr, 11), 77);
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    #[test]
    fn array_copy_and_reference_growth_keep_temporary_roots_under_gc_stress() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut arr = unsafe { willow_array_new(0, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        let depth = crate::gc::willow_root_depth();
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        for i in 0..12 {
            let value = willow_string_from_str(&format!("temporary-{i}"));
            // SAFETY: The destination is the live fixture array; the element word matches its
            // declared storage and tracing layout.
            unsafe { willow_array_push(arr, value as i64) };
            assert_eq!(crate::gc::willow_root_depth(), depth);
        }
        // Leave source liveness to array_copy's own root during its allocations.
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        // SAFETY: The fixture retains the array allocated for these accesses.
        let mut copy = unsafe { willow_array_copy(arr) };
        crate::gc::set_gc_stress_for_test(None);
        assert_eq!(crate::gc::willow_root_depth(), depth - 1);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut copy) };
        willow_gc_collect();
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_len(copy) }, 12);
        for i in 0..12 {
            assert_eq!(
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                unsafe { willow_string_as_str(willow_array_get(copy, i) as *mut u8) },
                format!("temporary-{i}")
            );
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    fn interface_pair_for_test(text: &str, vtable: i64) -> *mut u8 {
        let mut object = willow_string_from_str(text);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut object) };
        // SAFETY: The constructor selects the matching payload layout and initializes its
        // fields before exposing it or reaching a GC point.
        let pair = unsafe { willow_alloc_with_layout(GcObjectKind::Enum, 0, 16, 1) };
        // SAFETY: The managed pair reserves two words at the GC allocator's word alignment.
        unsafe { store_pair(pair.cast(), object as i64, vtable, 1) };
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
        pair
    }

    #[test]
    fn minor_contiguous_range_requires_the_builtin_trace_registration() {
        unsafe fn replacement(_: *mut u8, _: &mut Vec<*mut *mut u8>) {}
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for count in [0, 1, 32, 10_000] {
            // SAFETY: Flag 1 allocates full-width reference slots.
            let mut array = unsafe { willow_array_new(count, 1) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut array) };
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            let buffer = unsafe { handle_buffer(array) };
            let range =
                // SAFETY: The live reference buffer and its initialized length meet the registered
                // array tracer's contract.
                unsafe { minor_reference_range(ARRAY_REF_TYPE_ID, Some(trace_array_ref), buffer) }
                    .unwrap();
            // SAFETY: Every buffer contains the full header. With zero capacity
            // its end is one past the allocation and is only compared here.
            let start = unsafe { buffer.add(BUFFER_HEADER_WORDS * WORD_BYTES as usize) };
            assert_eq!(range.0, start.cast());
            assert_eq!(range.1, count as usize);
            assert!(
                // SAFETY: The live reference buffer and its initialized length meet the registered
                // array tracer's contract.
                unsafe { minor_reference_range(ARRAY_REF_TYPE_ID, Some(replacement), buffer) }
                    .is_none()
            );
            // SAFETY: The live reference buffer and its initialized length meet the registered
            // array tracer's contract.
            assert!(unsafe { minor_reference_range(ARRAY_REF_TYPE_ID, None, buffer) }.is_none());
            // SAFETY: The live reference buffer and its initialized length meet the registered
            // array tracer's contract.
            assert!(unsafe { minor_reference_range(0, Some(trace_array_ref), buffer) }.is_none());
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
    }

    #[test]
    fn owned_pair_growth_transfers_only_unexposed_storage() {
        let _guard = isolated_runtime();
        for count in [4i64, 16, 1024] {
            // SAFETY: The test controls its isolated runtime at this reset boundary; no other
            // mutator or subsequent access relies on the discarded heap.
            unsafe { willow_gc_init() };
            // SAFETY: Flag 1 allocates full-width reference slots.
            let mut arr = unsafe { willow_array_new(0, 1) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut arr) };
            for i in 0..count {
                let pair = interface_pair_for_test("private", i);
                // SAFETY: This is a fresh, unaliased two-word box with the reference array’s
                // declared pair layout.
                unsafe { willow_array_push_owned_pair(arr, pair as i64) };
            }
            // Private test inspection does not expose storage through the ABI.
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            let first = unsafe { *buf_slot(handle_buffer(arr), 0) };
            let incoming = interface_pair_for_test("new", 99);
            let before = crate::gc::telemetry_heap_snapshot().0.allocation_count;
            // SAFETY: This is a fresh, unaliased two-word box with the reference array’s
            // declared pair layout.
            unsafe { willow_array_push_owned_pair(arr, incoming as i64) };
            assert_eq!(
                crate::gc::telemetry_heap_snapshot().0.allocation_count - before,
                1
            );
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            assert_eq!(unsafe { *buf_slot(handle_buffer(arr), 0) }, first);
            for _ in count + 1..2 * count {
                let pair = interface_pair_for_test("private", 5);
                // SAFETY: This is a fresh, unaliased two-word box with the reference array’s
                // declared pair layout.
                unsafe { willow_array_push_owned_pair(arr, pair as i64) };
            }
            // SAFETY: The index selects initialized storage in the current array; the
            // returned address is used while its buffer is live.
            let mut captured = unsafe { willow_array_reference_owner(arr, 0) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut captured) };
            let incoming = interface_pair_for_test("new", 100);
            // SAFETY: This is a fresh, unaliased two-word box with the reference array’s
            // declared pair layout.
            unsafe { willow_array_push_owned_pair(arr, incoming as i64) };
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            let fresh = unsafe { *buf_slot(handle_buffer(arr), 0) };
            // SAFETY: The GC word-aligns this live buffer. The index and element width select
            // storage within its capacity.
            let old = unsafe { *buf_slot(captured, 0) };
            assert_ne!(fresh, old);
            // SAFETY: The managed pair reserves two words at the GC allocator's word alignment.
            unsafe { store_pair(old as *mut i64, 0, 777, 1) };
            assert_eq!(
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                unsafe { willow_string_as_str(*(fresh as *const *mut u8)) },
                "private"
            );
            for _ in 2 * count + 1..4 * count {
                let pair = interface_pair_for_test("private", 5);
                // SAFETY: This is a fresh, unaliased two-word box with the reference array’s
                // declared pair layout.
                unsafe { willow_array_push_owned_pair(arr, pair as i64) };
            }
            let incoming = interface_pair_for_test("new", 101);
            // SAFETY: This is a fresh, unaliased two-word box with the reference array’s
            // declared pair layout.
            unsafe { willow_array_push_owned_pair(arr, incoming as i64) };
            // Capturing the obsolete buffer does not expose a later buffer.
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            assert_eq!(unsafe { *buf_slot(handle_buffer(arr), 0) }, fresh);
            assert_eq!(
                // SAFETY: The GC word-aligns this live buffer. The index and element width select
                // storage within its capacity.
                unsafe { *((*buf_slot(captured, 0) as *const i64).add(1)) },
                777
            );
            willow_gc_collect();
            // SAFETY: The fixture retains the array allocated for these accesses.
            assert_eq!(unsafe { willow_array_len(arr) }, 4 * count + 1);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(2) };
        }
    }

    #[test]
    fn interface_pair_copies_use_tlab_and_preserve_moving_references() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let _owner = crate::gc::RuntimeTlabOwner::enter();
        let mut source = interface_pair_for_test("retained", 1234);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut source) };
        for count in [1, 100, 10_000] {
            // SAFETY: Flag 1 allocates full-width reference slots.
            let mut array = unsafe { willow_array_new(count, 1) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut array) };
            for index in 0..count {
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                let pair = unsafe { copy_scalar_pair(source.cast()) };
                assert!(!pair.is_null());
                assert_ne!(pair, source);
                // Both slow refill and fast copies on a TLAB-owning thread
                // allocate young. The source object survives any refill GC.
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                let generation = unsafe { crate::gc::GcHeader::live_generation(pair) };
                assert_eq!(generation, willow_abi::gc_header::GENERATION_YOUNG);
                // SAFETY: The destination is the live fixture array; the element word matches
                // its declared storage and tracing layout.
                unsafe { willow_array_set(array, index, pair as i64) };
            }
            crate::gc::minor_collect_internal();
            for index in 0..count {
                // SAFETY: The fixture retains the array allocated for these accesses.
                let pair = unsafe { willow_array_get(array, index) } as *mut i64;
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                unsafe {
                    assert_eq!(pair_ref_mask(pair), 1);
                    assert_eq!(willow_string_as_str(pair.read() as *mut u8), "retained");
                    assert_eq!(pair.add(1).read(), 1234);
                }
            }
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(1) };
    }

    #[test]
    fn interface_pair_copy_and_growth_preserve_tracing_and_independent_storage() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for count in [1, 16, 64] {
            // SAFETY: Flag 1 allocates full-width reference slots.
            let mut arr = unsafe { willow_array_new(count, 1) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut arr) };
            for index in 0..count {
                let pair = interface_pair_for_test(&format!("object-{index}"), 1234);
                // SAFETY: The destination is the live fixture array; the element word matches
                // its declared storage and tracing layout.
                unsafe { willow_array_set(arr, index, pair as i64) };
            }
            crate::gc::set_gc_stress_for_test(Some("alloc"));
            // SAFETY: The live reference array holds zero or the initialized two-word boxes
            // allocated by this fixture.
            let mut copy = unsafe { willow_array_copy_scalar_pairs(arr) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut copy) };
            let incoming = interface_pair_for_test("new", 5678);
            // SAFETY: The incoming box and stored boxes have the same initialized two-word
            // layout in this reference array.
            unsafe { willow_array_push_scalar_pairs(arr, incoming as i64) };
            crate::gc::set_gc_stress_for_test(None);
            willow_gc_collect();
            for index in 0..count {
                // SAFETY: The fixture retains the array allocated for these accesses.
                let source = unsafe { willow_array_get(arr, index) } as *mut i64;
                // SAFETY: The fixture retains the array allocated for these accesses.
                let frozen = unsafe { willow_array_get(copy, index) } as *mut i64;
                assert_ne!(source, frozen);
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                unsafe {
                    assert_eq!(pair_ref_mask(source), 1);
                    assert_eq!(pair_ref_mask(frozen), 1);
                    assert_eq!(
                        willow_string_as_str(source.read() as *mut u8),
                        format!("object-{index}")
                    );
                    assert_eq!(
                        willow_string_as_str(frozen.read() as *mut u8),
                        format!("object-{index}")
                    );
                    assert_eq!(frozen.add(1).read(), 1234);
                    store_pair(source, 0, 9999, 1);
                    assert_ne!(frozen.read(), 0);
                    assert_eq!(frozen.add(1).read(), 1234);
                }
            }
            willow_gc_collect();
            // SAFETY: The fixture retains the array allocated for these accesses.
            let frozen = unsafe { willow_array_get(copy, 0) } as *mut i64;
            assert_eq!(
                // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this
                // borrow.
                unsafe { willow_string_as_str(frozen.read() as *mut u8) },
                "object-0"
            );
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(2) };
        }
    }

    #[test]
    fn interface_pair_pop_reuse_clears_edges_and_preserves_storage() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut arr = unsafe { willow_array_new(1, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        let pair = interface_pair_for_test("first", 1234);
        // SAFETY: The destination is the live fixture array; the element word matches its
        // declared storage and tracing layout.
        unsafe { willow_array_set(arr, 0, pair as i64) };
        willow_gc_collect();
        // SAFETY: The fixture retains the array allocated for these accesses.
        let mut retained = unsafe { willow_array_get(arr, 0) } as *mut u8;
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut retained) };
        let mut out = [0i64; 2];
        let before = crate::gc::telemetry_heap_snapshot().0.allocation_count;
        // SAFETY: The last element is an initialized two-word pair; out provides two disjoint
        // writable i64 words.
        unsafe { willow_array_pop_scalar_pair_into(arr, out.as_mut_ptr()) };
        assert_eq!(
            crate::gc::telemetry_heap_snapshot().0.allocation_count,
            before
        );
        assert_eq!(out[1], 1234);
        // SAFETY: The string ABI permits null or a live UTF-8 payload throughout this borrow.
        assert_eq!(unsafe { willow_string_as_str(out[0] as *mut u8) }, "first");
        // SAFETY: The managed pair reserves two words at the GC allocator's word alignment.
        unsafe {
            assert_eq!(pair_ref_mask(retained.cast()), 1);
            assert_eq!(
                [
                    retained.cast::<i64>().read(),
                    retained.cast::<i64>().add(1).read()
                ],
                [0, 0]
            );
        }
        let incoming = interface_pair_for_test("replacement", 5678);
        let before = crate::gc::telemetry_heap_snapshot().0.allocation_count;
        // SAFETY: The incoming box and stored boxes have the same initialized two-word layout
        // in this reference array.
        unsafe { willow_array_push_scalar_pairs(arr, incoming as i64) };
        assert_eq!(
            crate::gc::telemetry_heap_snapshot().0.allocation_count,
            before
        );
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_get(arr, 0) }, retained as i64);
        willow_gc_collect();
        // SAFETY: The managed pair reserves two words at the GC allocator's word alignment.
        unsafe {
            let words = retained.cast::<i64>();
            assert_eq!(pair_ref_mask(words), 1);
            assert_eq!(willow_string_as_str(words.read() as *mut u8), "replacement");
            assert_eq!(words.add(1).read(), 5678);
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
    }

    #[test]
    fn scalar_pair_freeze_copies_boxes_and_survives_gc_stress() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        // SAFETY: Flag 1 allocates full-width reference slots.
        let mut arr = unsafe { willow_array_new(4, 1) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut arr) };
        let words = [(0, 0), (1, i64::MIN), (0, f64::NAN.to_bits() as i64)];
        for (index, &(tag, payload)) in words.iter().enumerate() {
            // SAFETY: The constructor selects the matching payload layout and initializes its
            // fields before exposing it or reaching a GC point.
            let pair = unsafe { willow_alloc_with_layout(GcObjectKind::Enum, 0, 16, 0) };
            // SAFETY: The managed pair reserves two words at the GC allocator's word alignment.
            unsafe {
                (pair as *mut i64).write(tag);
                (pair as *mut i64).add(1).write(payload);
            }
            // SAFETY: The destination is the live fixture array; the element word matches its
            // declared storage and tracing layout.
            unsafe { willow_array_set(arr, index as i64, pair as i64) };
        }
        let depth = crate::gc::willow_root_depth();
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        // SAFETY: The live reference array holds zero or the initialized two-word boxes
        // allocated by this fixture.
        let mut copy = unsafe { willow_array_copy_scalar_pairs(arr) };
        crate::gc::set_gc_stress_for_test(None);
        assert_eq!(crate::gc::willow_root_depth(), depth);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut copy) };
        willow_gc_collect();
        // SAFETY: The fixture retains the array allocated for these accesses.
        assert_eq!(unsafe { willow_array_get(copy, 3) }, 0);
        for (index, &(tag, payload)) in words.iter().enumerate() {
            // SAFETY: The fixture retains the array allocated for these accesses.
            let source = unsafe { willow_array_get(arr, index as i64) } as *mut i64;
            // SAFETY: The fixture retains the array allocated for these accesses.
            let frozen = unsafe { willow_array_get(copy, index as i64) } as *const i64;
            assert_ne!(source as *const i64, frozen);
            // SAFETY: The managed pair reserves two words at the GC allocator's word alignment.
            unsafe {
                source.write(99);
                source.add(1).write(123);
                assert_eq!(frozen.read(), tag);
                assert_eq!(frozen.add(1).read(), payload);
            }
        }
        // SAFETY: The removed shadow-stack entries belong to this operation and their slot
        // storage is still live.
        unsafe { willow_pop_roots(2) };
    }

    #[test]
    fn scalar_pair_growth_copies_each_element_once_and_preserves_old_buffer() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for count in [1, 16, 128] {
            // SAFETY: Flag 1 allocates full-width reference slots.
            let mut arr = unsafe { willow_array_new(count, 1) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut arr) };
            for index in 0..count {
                // SAFETY: The constructor selects the matching payload layout and initializes
                // its fields before exposing it or reaching a GC point.
                let pair = unsafe { willow_alloc_with_layout(GcObjectKind::Enum, 0, 16, 0) };
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                unsafe {
                    pair.cast::<i64>().write(index % 2);
                    pair.cast::<i64>().add(1).write(i64::MIN + index);
                }
                // SAFETY: The destination is the live fixture array; the element word matches
                // its declared storage and tracing layout.
                unsafe { willow_array_set(arr, index, pair as i64) };
            }
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            let mut old_buffer = unsafe { handle_buffer(arr) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut old_buffer) };
            // SAFETY: The constructor selects the matching payload layout and initializes its
            // fields before exposing it or reaching a GC point.
            let pushed = unsafe { willow_alloc_with_layout(GcObjectKind::Enum, 0, 16, 0) };
            // SAFETY: The managed pair reserves two words at the GC allocator's word alignment.
            unsafe { pushed.cast::<i64>().add(1).write(777) };
            let depth = crate::gc::willow_root_depth();
            let before = crate::gc::telemetry_heap_snapshot().0.allocation_count;
            crate::gc::set_gc_stress_for_test(Some("alloc"));
            // SAFETY: The incoming box and stored boxes have the same initialized two-word
            // layout in this reference array.
            unsafe { willow_array_push_scalar_pairs(arr, pushed as i64) };
            crate::gc::set_gc_stress_for_test(None);
            let allocations = crate::gc::telemetry_heap_snapshot().0.allocation_count - before;
            // One new buffer and exactly one box per live copied element.
            assert_eq!(allocations, count as u64 + 1);
            eprintln!("pair-growth count={count} allocations={allocations}");
            assert_eq!(crate::gc::willow_root_depth(), depth);
            willow_gc_collect();
            // SAFETY: The fixture retains the array allocated for these accesses.
            assert_eq!(unsafe { willow_array_len(arr) }, count + 1);
            // SAFETY: The retained GC buffer begins with its aligned atomic length word.
            assert_eq!(unsafe { buffer_len(old_buffer) }, count);
            let mut unique = std::collections::HashSet::new();
            for index in 0..count {
                // SAFETY: The GC word-aligns this live buffer. The index and element width select
                // storage within its capacity.
                let old_pair = unsafe { *buf_slot(old_buffer, index) } as *mut i64;
                // SAFETY: The fixture retains the array allocated for these accesses.
                let new_pair = unsafe { willow_array_get(arr, index) } as *const i64;
                assert_ne!(old_pair as *const i64, new_pair);
                assert!(unique.insert(new_pair));
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                unsafe {
                    assert_eq!(new_pair.read(), index % 2);
                    assert_eq!(new_pair.add(1).read(), i64::MIN + index);
                    old_pair.add(1).write(123);
                    assert_eq!(new_pair.add(1).read(), i64::MIN + index);
                }
            }
            assert_eq!(unique.len(), count as usize);
            assert_eq!(
                // SAFETY: The test inserted a two-word GC pair at this index. Its second word is
                // aligned and initialized.
                unsafe { (willow_array_get(arr, count) as *const i64).add(1).read() },
                777
            );
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(2) };
        }
    }

    #[test]
    fn scalar_pair_pop_reuse_preserves_storage_and_tracing() {
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        for count in [1, 16, 128] {
            // SAFETY: Flag 1 allocates full-width reference slots.
            let mut arr = unsafe { willow_array_new(count, 1) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut arr) };
            for index in 0..count {
                // SAFETY: The constructor selects the matching payload layout and initializes
                // its fields before exposing it or reaching a GC point.
                let pair = unsafe { willow_alloc_with_layout(GcObjectKind::Enum, 0, 16, 0) };
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                unsafe {
                    pair.cast::<i64>().write(index % 2);
                    pair.cast::<i64>().add(1).write(i64::MIN + index);
                }
                // SAFETY: The destination is the live fixture array; the element word matches
                // its declared storage and tracing layout.
                unsafe { willow_array_set(arr, index, pair as i64) };
            }
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            let mut owner = unsafe { handle_buffer(arr) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut owner) };
            let depth = crate::gc::willow_root_depth();
            let before = crate::gc::telemetry_heap_snapshot().0.allocation_count;
            for index in (0..count).rev() {
                let mut out = [-1; 2];
                // SAFETY: The last element is an initialized two-word pair; out provides two
                // disjoint writable i64 words.
                unsafe { willow_array_pop_scalar_pair_into(arr, out.as_mut_ptr()) };
                assert_eq!(out, [index % 2, i64::MIN + index]);
                // SAFETY: The GC word-aligns this live buffer. The index and element width select
                // storage within its capacity.
                let pair = unsafe { *buf_slot(owner, index) } as *const i64;
                assert!(!pair.is_null());
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                assert_eq!(unsafe { [pair.read(), pair.add(1).read()] }, [0, 0]);
            }
            assert_eq!(
                crate::gc::telemetry_heap_snapshot().0.allocation_count,
                before
            );
            // SAFETY: The fixture retains the array allocated for these accesses.
            assert_eq!(unsafe { willow_array_len(arr) }, 0);
            // SAFETY: The retained GC buffer begins with its aligned atomic length word.
            assert_eq!(unsafe { buffer_len(owner) }, count);
            let mut slots = Vec::new();
            // SAFETY: The live reference buffer and its initialized length meet the registered
            // array tracer's contract.
            unsafe { trace_array_ref(owner, &mut slots) };
            assert_eq!(slots.len(), count as usize);
            willow_gc_collect();
            crate::gc::set_gc_stress_for_test(Some("alloc"));
            for index in 0..count {
                // SAFETY: The constructor selects the matching payload layout and initializes
                // its fields before exposing it or reaching a GC point.
                let incoming = unsafe { willow_alloc_with_layout(GcObjectKind::Enum, 0, 16, 0) };
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                unsafe { incoming.cast::<i64>().add(1).write(index + 99) };
                // Read the old identity only after allocation, which may move it.
                // SAFETY: The GC word-aligns this live buffer. The index and element width select
                // storage within its capacity.
                let old_pair = unsafe { *buf_slot(owner, index) } as *mut i64;
                let before = crate::gc::telemetry_heap_snapshot().0.allocation_count;
                // SAFETY: The incoming box and stored boxes have the same initialized two-
                // word layout in this reference array.
                unsafe { willow_array_push_scalar_pairs(arr, incoming as i64) };
                assert_eq!(
                    crate::gc::telemetry_heap_snapshot().0.allocation_count,
                    before
                );
                // SAFETY: The fixture retains the array allocated for these accesses.
                assert_eq!(unsafe { willow_array_get(arr, index) }, old_pair as i64);
                // SAFETY: The managed pair reserves two words at the GC allocator's word
                // alignment.
                unsafe {
                    assert_eq!(old_pair.add(1).read(), index + 99);
                    old_pair.add(1).write(777);
                }
                assert_eq!(
                    // SAFETY: The test inserted a two-word GC pair at this index. Its second word
                    // is aligned and initialized.
                    unsafe { (willow_array_get(arr, index) as *const i64).add(1).read() },
                    777
                );
                // SAFETY: The retained GC buffer begins with its aligned atomic length word.
                assert_eq!(unsafe { buffer_len(owner) }, count);
            }
            crate::gc::set_gc_stress_for_test(None);
            assert_eq!(crate::gc::willow_root_depth(), depth);
            eprintln!(
                "pair-pop-reuse count={count} retained-slots={} pop-allocations=0 reuse-runtime-allocations=0",
                slots.len()
            );
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(2) };
        }
    }

    #[test]
    fn scalar_pair_pop_errors_initialize_output_without_losing_roots() {
        use crate::panic_context::*;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(905))));
        let depth = crate::gc::willow_root_depth();
        // SAFETY: Flag 1 allocates full-width reference slots.
        let empty = unsafe { willow_array_new(0, 1) };
        for (arr, message) in [
            (empty, "cannot pop from an empty array"),
            (std::ptr::null_mut(), "cannot pop from a null array"),
        ] {
            let mut out = [-11, -1, -1, -22];
            // SAFETY: arr is null or the empty reference array; out provides two aligned, disjoint output
            // words beginning at index one, including on both error paths.
            unsafe { willow_array_pop_scalar_pair_into(arr, out[1..3].as_mut_ptr()) };
            assert_eq!(out, [-11, 0, 0, -22]);
            assert_eq!(crate::gc::willow_root_depth(), depth);
            assert_eq!(willow_panic_depth(), 1);
            willow_panic_enter_defer();
            let info = willow_panic_recover();
            willow_panic_leave_defer();
            // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
            assert_eq!(unsafe { panic_info_message(info) }, message);
            willow_panic_release_recovered(info);
        }
        replace_current_context(previous);
    }

    #[test]
    fn scalar_pair_freeze_null_preserves_root_depth_and_raises() {
        use crate::panic_context::*;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(904))));
        let depth = crate::gc::willow_root_depth();
        // SAFETY: A null array is accepted by this error path.
        assert!(unsafe { willow_array_copy_scalar_pairs(std::ptr::null_mut()) }.is_null());
        assert_eq!(crate::gc::willow_root_depth(), depth);
        assert_eq!(willow_panic_depth(), 1);
        willow_panic_enter_defer();
        let info = willow_panic_recover();
        willow_panic_leave_defer();
        assert_eq!(
            // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
            unsafe { panic_info_message(info) },
            "cannot freeze a null array"
        );
        willow_panic_release_recovered(info);
        replace_current_context(previous);
    }

    #[test]
    fn array_len_raises_for_null_and_negative_lengths() {
        use crate::panic_context::*;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(906))));
        // SAFETY: Flag 0 selects untraced word storage.
        let arr = crate::gc::GcRootHandle::new(unsafe { willow_array_new(2, 0) });
        // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
        unsafe { set_handle_word(arr.load(), H_LEN, -3) };
        for (null, message) in [
            (true, "cannot take the length of a null array"),
            (false, "malformed array: negative length -3"),
        ] {
            let target = if null {
                std::ptr::null_mut()
            } else {
                arr.load()
            };
            // SAFETY: The operand is null or the allocated handle with an initialized length
            // word.
            assert_eq!(unsafe { willow_array_len(target) }, 0);
            assert_eq!(willow_panic_depth(), 1);
            willow_panic_enter_defer();
            let info = willow_panic_recover();
            willow_panic_leave_defer();
            // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
            assert_eq!(unsafe { panic_info_message(info) }, message);
            willow_panic_release_recovered(info);
        }
        // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
        unsafe { set_handle_word(arr.load(), H_LEN, 2) };
        // SAFETY: The operand is null or the allocated handle with an initialized length
        // word.
        assert_eq!(unsafe { willow_array_len(arr.load()) }, 2);
        assert_eq!(willow_panic_depth(), 0);
        replace_current_context(previous);
    }

    #[test]
    fn array_null_push_preserves_error_and_root_depth() {
        use crate::panic_context::*;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(903))));
        let depth = crate::gc::willow_root_depth();
        // SAFETY: A null array is accepted by this error path.
        unsafe { willow_array_push(std::ptr::null_mut(), 42) };
        assert_eq!(crate::gc::willow_root_depth(), depth);
        assert_eq!(willow_panic_depth(), 1);
        willow_panic_enter_defer();
        let info = willow_panic_recover();
        willow_panic_leave_defer();
        assert_eq!(
            // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
            unsafe { panic_info_message(info) },
            "cannot push to a null array"
        );
        willow_panic_release_recovered(info);
        replace_current_context(previous);
    }

    // Synthetic handle lengths force checked overflow without huge allocations.
    // The real buffer retains its valid logical length for GC tracing throughout.
    #[test]
    fn array_allocation_overflow_preserves_state_and_roots() {
        use crate::panic_context::*;
        let _guard = isolated_runtime();
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(902))));
        for (is_ref, nonnull_value) in [(false, false), (true, false), (true, true)] {
            // SAFETY: The fixture selects only word-sized scalar or reference storage.
            let mut arr = unsafe { willow_array_new(1, i64::from(is_ref)) };
            // SAFETY: The initialized slot stays at this address until its registration is
            // popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut arr) };
            let value = if nonnull_value {
                willow_string_from_str("rooted") as i64
            } else {
                0
            };
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            let buffer = unsafe { handle_buffer(arr) };
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            unsafe {
                set_handle_word(arr, H_LEN, i64::MAX);
                set_handle_word(arr, H_CAP, i64::MAX);
            }
            let depth = crate::gc::willow_root_depth();
            // SAFETY: The live handle has an overflowing requested capacity, rejected before
            // any element access. Its real buffer remains initialized.
            unsafe { willow_array_push(arr, value) };
            assert_eq!(crate::gc::willow_root_depth(), depth);
            assert_eq!(willow_panic_depth(), 1);
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            assert_eq!(unsafe { handle_buffer(arr) }, buffer);
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            assert_eq!(unsafe { handle_word(arr, H_LEN) }, i64::MAX);
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            assert_eq!(unsafe { handle_word(arr, H_CAP) }, i64::MAX);
            willow_panic_enter_defer();
            let info = willow_panic_recover();
            willow_panic_leave_defer();
            // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
            assert!(unsafe { panic_info_message(info) }.starts_with("array capacity too large:"));
            willow_panic_release_recovered(info);

            // SAFETY: The live handle has an overflowing requested capacity, rejected before
            // any element access. Its real buffer remains initialized.
            assert!(unsafe { willow_array_copy(arr) }.is_null());
            assert_eq!(crate::gc::willow_root_depth(), depth);
            assert_eq!(willow_panic_depth(), 1);
            willow_panic_enter_defer();
            let info = willow_panic_recover();
            willow_panic_leave_defer();
            // SAFETY: The retained PanicInfo keeps its initialized fields live through this read.
            assert!(unsafe { panic_info_message(info) }.starts_with("array capacity too large:"));
            willow_panic_release_recovered(info);
            // SAFETY: The live handle owns its buffer and the ABI fixes these word offsets.
            unsafe {
                set_handle_word(arr, H_LEN, 1);
                set_handle_word(arr, H_CAP, 1);
            }
            // SAFETY: The fixture retains the array allocated for these accesses.
            assert_eq!(unsafe { willow_array_get(arr, 0) }, 0);
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_roots(1) };
        }
        replace_current_context(previous);
    }
}
