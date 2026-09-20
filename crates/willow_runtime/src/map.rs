//! GC-managed hash map `Map<K, V>`.
//!
//! The map stores a `Mutex<MapData>` directly in its non-moving GC payload.
//! Two GC hooks keep it correct:
//!
//! * a trace function reports reference-typed *values* so they stay alive while
//!   the map is reachable (keys are copied out of the Willow heap, so they need
//!   no tracing — see [`MapKey`]);
//! * a finalizer drops the inline state when the map is swept, releasing the
//!   hash table and its owned string keys.
//!
//! Keys are one 64-bit word — an `i64`, the bits of an `f64`, or a `bool` — or
//! a `String` compared by content. Values are stored as raw 64-bit words; `.get`
//! returns a Willow `Option<V>` built directly here.

use crate::gc::{
    GcObjectKind, GcStoreDestination, willow_alloc_with_layout, willow_gc_write_barrier,
};
use crate::string::willow_string_as_str;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};

use willow_abi::runtime_type_ids::MAP_TYPE_ID;

/// A key copied out of the Willow heap so the map owns it independently of the
/// GC. String keys compare by content (not pointer identity), which is what
/// `Map<String, V>` lookups require. Every other admitted key is one word, so
/// `Word` holds an `i64`, a `bool` as 0/1, or canonical `f64` bits. Floating
/// keys reject NaN and normalize both signed zeros to +0.0, so admitted keys
/// have the same equality as the language's numerical `==`.
#[derive(PartialEq, Eq, Clone)]
enum MapKey {
    Word(i64),
    Str(String),
}

#[derive(Clone, Copy)]
struct MapLayout {
    key_kind: i64,
    value_kind: i64,
    value_is_ref: bool,
}

struct MapData {
    /// Fixed at construction from Map<K, V>; inserts never change GC metadata.
    layout: MapLayout,
    /// Hash buckets may move on growth; dense value indices never do. This
    /// permits bounded GC slices without restarting/skipping a hash iterator.
    entries: HashMap<MapKey, usize>,
    values: Vec<i64>,
    /// Major cycles are serialized. Appends after the first slice are covered
    /// by insertion barriers and must not extend this epoch's finite scan.
    scan_limit: usize,
}

impl MapData {
    fn get(&self, key: &dyn KeyView) -> Option<i64> {
        self.entries.get(key).map(|&index| self.values[index])
    }
}

/// The owned and borrowed keys share exactly the same hash/equality encoding.
#[derive(PartialEq, Eq, Hash, Clone, Copy)]
enum KeyRef<'a> {
    Word(i64),
    Str(&'a str),
}

trait KeyView {
    fn key_ref(&self) -> KeyRef<'_>;
}

impl KeyView for MapKey {
    fn key_ref(&self) -> KeyRef<'_> {
        match self {
            Self::Word(word) => KeyRef::Word(*word),
            Self::Str(text) => KeyRef::Str(text),
        }
    }
}

impl KeyView for KeyRef<'_> {
    fn key_ref(&self) -> KeyRef<'_> {
        *self
    }
}

impl Hash for MapKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key_ref().hash(state);
    }
}

impl Hash for dyn KeyView + '_ {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key_ref().hash(state);
    }
}

impl PartialEq for dyn KeyView + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.key_ref() == other.key_ref()
    }
}
impl Eq for dyn KeyView + '_ {}

impl<'a> Borrow<dyn KeyView + 'a> for MapKey {
    fn borrow(&self) -> &(dyn KeyView + 'a) {
        self
    }
}

/// Borrow a key only during the lookup; no Willow pointer is retained in the map.
///
/// # Safety
/// When `key_kind` is 3, `word` must be a valid WillowString pointer
/// for the returned borrow's lifetime. Do not allocate in the GC while borrowed.
/// Returns None for NaN; callers must release the map lock before raising.
unsafe fn key_from_word<'a>(word: i64, key_kind: i64) -> Option<KeyRef<'a>> {
    match key_kind {
        3 => Some(KeyRef::Str(unsafe {
            willow_string_as_str(word as *const u8)
        })),
        1 => {
            // Classify bits without floating-point arithmetic, preserving
            // subnormals and rejecting both quiet and signaling NaNs.
            let magnitude = (word as u64) & 0x7fff_ffff_ffff_ffff;
            if magnitude > 0x7ff0_0000_0000_0000 {
                None
            } else {
                Some(KeyRef::Word(if magnitude == 0 { 0 } else { word }))
            }
        }
        _ => Some(KeyRef::Word(word)),
    }
}

fn raise_nan_key() {
    crate::panic_context::raise_language_message("NaN cannot be used as a Map key");
}

/// Lock the inline `MapData` at a map payload pointer.
///
/// # Safety
/// `map` must be a non-null map payload produced by [`willow_map_new`].
unsafe fn map_data<'a>(map: *mut u8) -> MutexGuard<'a, MapData> {
    unsafe { &*map.cast::<Mutex<MapData>>() }.lock().unwrap()
}

/// Trace hook: report reference-typed values as GC children.
unsafe fn trace_map(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    let mut data = unsafe { map_data(payload) };
    if data.layout.value_is_ref {
        for value in &mut data.values {
            slots.push((value as *mut i64).cast::<*mut u8>());
        }
    }
}

unsafe fn snapshot_map(payload: *mut u8, children: &mut Vec<*mut u8>) {
    let data = unsafe { map_data(payload) };
    if data.layout.value_is_ref {
        children.extend(data.values.iter().map(|value| *value as *mut u8));
    }
}

unsafe fn snapshot_map_slice(
    payload: *mut u8,
    cursor: usize,
    limit: usize,
    children: &mut Vec<*mut u8>,
) -> crate::gc::TraceSliceProgress {
    use crate::gc::TraceSliceProgress;
    let mut data = match unsafe { &*payload.cast::<Mutex<MapData>>() }.try_lock() {
        Ok(data) => data,
        Err(std::sync::TryLockError::WouldBlock) => return TraceSliceProgress::Retry,
        Err(std::sync::TryLockError::Poisoned(_)) => panic!("map trace found poisoned storage"),
    };
    if !data.layout.value_is_ref {
        return TraceSliceProgress::Done;
    }
    if cursor == 0 {
        data.scan_limit = data.values.len();
    }
    let end = cursor.saturating_add(limit).min(data.scan_limit);
    children.extend(
        data.values[cursor.min(end)..end]
            .iter()
            .map(|&value| value as *mut u8),
    );
    if end < data.scan_limit {
        TraceSliceProgress::Continue(end)
    } else {
        TraceSliceProgress::Done
    }
}

/// Finalizer hook: release the hash table and owned keys when the map is swept.
unsafe fn drop_map(payload: *mut u8) {
    unsafe { std::ptr::drop_in_place(payload.cast::<Mutex<MapData>>()) };
}

/// Register the map trace and finalizer. Called on every `willow_map_new`
/// (idempotent): `willow_gc_init` clears the type registry, so a process-global
/// `Once` would fail to re-register after the first reset (e.g. in multi-init
/// test runs). Real programs init once, so the repeated insert is harmless.
static MAP_REGISTRATION: crate::gc::NativeGcRegistration = crate::gc::NativeGcRegistration::new();
const MAP_GC_TYPES: &[crate::gc::NativeGcType] =
    &[
        crate::gc::NativeGcType::new(MAP_TYPE_ID, Some(trace_map), Some(drop_map))
            .with_concurrent_trace(snapshot_map)
            .with_concurrent_slice(snapshot_map_slice),
    ];

fn ensure_registered() {
    MAP_REGISTRATION.ensure(MAP_GC_TYPES);
}

/// Allocate an empty map with inline state. `gc_ref_mask` is zero because
/// reference values are traced through `trace_map`, not the native state words.
#[unsafe(no_mangle)]
pub extern "C" fn willow_map_new(key_kind: i64, value_kind: i64, value_is_ref: i64) -> *mut u8 {
    // Old-generation payloads never move and provide header alignment, which
    // must also satisfy the inline mutex on every supported target.
    const { assert!(align_of::<Mutex<MapData>>() <= align_of::<crate::gc::GcHeader>()) };
    ensure_registered();
    let map = willow_alloc_with_layout(
        GcObjectKind::Map,
        MAP_TYPE_ID,
        size_of::<Mutex<MapData>>() as i64,
        0,
    );
    if map.is_null() {
        return std::ptr::null_mut();
    }
    // No GC allocation occurs between reserving the payload and initializing it.
    unsafe {
        map.cast::<Mutex<MapData>>().write(Mutex::new(MapData {
            layout: MapLayout {
                key_kind,
                value_kind,
                value_is_ref: value_is_ref != 0,
            },
            entries: HashMap::new(),
            values: Vec::new(),
            scan_limit: 0,
        }));
    }
    map
}

/// Insert or update `key -> value`. `key_is_ref`/`val_is_ref` describe whether
/// the words are WillowString/GC pointers.
#[unsafe(no_mangle)]
pub extern "C" fn willow_map_insert(
    map: *mut u8,
    key_word: i64,
    key_is_ref: i64,
    val_word: i64,
    val_is_ref: i64,
) {
    if map.is_null() {
        return;
    }
    let mut data = unsafe { map_data(map) };
    debug_assert_eq!(data.layout.value_is_ref, val_is_ref != 0);
    debug_assert_eq!(data.layout.key_kind == 3, key_is_ref != 0);
    let Some(key) = (unsafe { key_from_word(key_word, data.layout.key_kind) }) else {
        drop(data);
        raise_nan_key();
        return;
    };
    let owned_key = match key {
        KeyRef::Word(word) => MapKey::Word(word),
        KeyRef::Str(text) => MapKey::Str(text.to_owned()),
    };
    let is_ref = data.layout.value_is_ref;
    let MapData {
        entries, values, ..
    } = &mut *data;
    let entry = entries.entry(owned_key);
    if is_ref {
        let old = match &entry {
            std::collections::hash_map::Entry::Occupied(slot) => values[*slot.get()],
            std::collections::hash_map::Entry::Vacant(_) => 0,
        };
        willow_gc_write_barrier(
            map,
            old as *mut u8,
            val_word as *mut u8,
            GcStoreDestination::MapValue as i64,
        );
    }
    match entry {
        std::collections::hash_map::Entry::Occupied(slot) => values[*slot.get()] = val_word,
        std::collections::hash_map::Entry::Vacant(slot) => {
            let index = values.len();
            values.push(val_word);
            slot.insert(index);
        }
    }
}

/// Look up `key`, returning a Willow `Option<V>` (`Some(value)` or `None`).
/// `use_niche` is selected by the compiler's central `OptionRepr`: when set,
/// `V` is a guaranteed-non-null GC reference and the result is the value word
/// itself for `Some`, or zero for `None`. Other payloads use the boxed tagged
/// enum layout below.
#[unsafe(no_mangle)]
pub extern "C" fn willow_map_get(
    map: *mut u8,
    key_word: i64,
    key_is_ref: i64,
    use_niche: i64,
) -> *mut u8 {
    if map.is_null() {
        return if use_niche != 0 {
            std::ptr::null_mut()
        } else {
            alloc_none()
        };
    }
    let data = unsafe { map_data(map) };
    debug_assert_eq!(data.layout.key_kind == 3, key_is_ref != 0);
    let Some(key) = (unsafe { key_from_word(key_word, data.layout.key_kind) }) else {
        drop(data);
        raise_nan_key();
        return std::ptr::null_mut();
    };
    let value = data.get(&key);
    let is_ref = data.layout.value_is_ref;
    drop(data);
    match value {
        Some(v) if use_niche != 0 => v as *mut u8,
        Some(v) => alloc_some(v, is_ref),
        None if use_niche != 0 => std::ptr::null_mut(),
        None => alloc_none(),
    }
}

/// Allocate an independent copy of `map` (same entries + value ref-ness). Backs
/// `Map<K,V>::freeze()` -> `FrozenMap<K,V>` (willow-dgwo.10): the copy shares no
/// `MapData` with the original, so it is safe to treat as immutable / Sync.
#[unsafe(no_mangle)]
pub extern "C" fn willow_map_copy(map: *mut u8) -> *mut u8 {
    if map.is_null() {
        return willow_map_new(0, 0, 0);
    }
    let layout = unsafe { map_data(map) }.layout;
    // Allocate the destination BEFORE reading any value word out of the source
    // (willow-9tls.8). `willow_map_new` can collect, and a moving minor
    // collection evacuates young values reachable only through the rooted
    // source map, rewriting the source's slots through `trace_map`. A native
    // snapshot taken before that allocation would still hold the pre-move
    // addresses and put dangling pointers into the copy. Once the destination
    // exists, nothing below GC-allocates or polls a safepoint (the key clones
    // are Rust-heap allocations), so the words read from the source are the
    // current ones.
    let copy = willow_map_new(
        layout.key_kind,
        layout.value_kind,
        i64::from(layout.value_is_ref),
    );
    if copy.is_null() {
        return std::ptr::null_mut();
    }
    let src = unsafe { map_data(map) };
    let mut dst = unsafe { map_data(copy) };
    dst.entries.reserve(src.entries.len());
    dst.values.reserve(src.values.len());
    for &v in &src.values {
        if layout.value_is_ref {
            willow_gc_write_barrier(
                copy,
                std::ptr::null_mut(),
                v as *mut u8,
                GcStoreDestination::MapValue as i64,
            );
        }
        dst.values.push(v);
    }
    for (key, &index) in &src.entries {
        dst.entries.insert(key.clone(), index);
    }
    copy
}

/// Number of entries.
#[unsafe(no_mangle)]
pub extern "C" fn willow_map_len(map: *mut u8) -> i64 {
    if map.is_null() {
        return 0;
    }
    unsafe { map_data(map) }.entries.len() as i64
}

/// Whether `key` is present (1) or not (0).
#[unsafe(no_mangle)]
pub extern "C" fn willow_map_contains(map: *mut u8, key_word: i64, key_is_ref: i64) -> i64 {
    if map.is_null() {
        return 0;
    }
    let data = unsafe { map_data(map) };
    debug_assert_eq!(data.layout.key_kind == 3, key_is_ref != 0);
    let Some(key) = (unsafe { key_from_word(key_word, data.layout.key_kind) }) else {
        drop(data);
        raise_nan_key();
        return 0;
    };
    i64::from(data.entries.contains_key(&key as &dyn KeyView))
}

// Willow `Option` layout (must match the compiler's enum lowering):
//   Some(v) -> 2-word object [tag = 0, payload]
//   None    -> 1-word object [tag = 1]
// Variant tags follow declaration order in src/prelude.rs (`Some`, then `None`).

fn alloc_some(value_word: i64, val_is_ref: bool) -> *mut u8 {
    const WORD: &[willow_abi::SlotKind] = &[willow_abi::SlotKind::Word];
    const REF: &[willow_abi::SlotKind] = &[willow_abi::SlotKind::GcRef];
    crate::gc::willow_alloc_enum_variant(
        0,
        willow_abi::EnumVariantLayout::new(0, if val_is_ref { REF } else { WORD }),
        &[value_word],
    )
}

fn alloc_none() -> *mut u8 {
    crate::gc::willow_alloc_enum_variant(0, willow_abi::EnumVariantLayout::new(1, &[]), &[])
}

/// Debug display of a whole map: `{a: 1, b: 2}` — entries sorted by rendered key
/// for deterministic output (HashMap iteration order is not stable). Kinds follow
/// `willow_array_to_string` (0=i64, 1=f64, 2=bool, 3=String); string KEYS are
/// printed bare (they are identifiers-like), string VALUES are quoted
/// (willow-vwn6). `key_kind` is what tells a word key apart from the `f64` and
/// `bool` that share its representation, so `Map<f64, V>` prints `1.5` and not
/// the bit pattern. Returns a newly allocated WillowString.
#[unsafe(no_mangle)]
pub extern "C" fn willow_map_to_string(map: *mut u8) -> *mut u8 {
    let (key_kind, val_kind) = if map.is_null() {
        (0, 0)
    } else {
        let layout = unsafe { map_data(map) }.layout;
        (layout.key_kind, layout.value_kind)
    };
    let mut entries: Vec<(String, i64)> = if map.is_null() {
        Vec::new()
    } else {
        let data = unsafe { map_data(map) };
        data.entries
            .iter()
            .map(|(k, &index)| {
                let key = match k {
                    MapKey::Word(n) => crate::array::element_word_to_string(*n, key_kind),
                    MapKey::Str(s) => s.clone(),
                };
                (key, data.values[index])
            })
            .collect()
    };
    entries.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("{");
    for (i, (key, word)) in entries.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(key);
        out.push_str(": ");
        out.push_str(&crate::array::element_word_to_string(*word, val_kind));
    }
    out.push('}');
    crate::string::willow_string_from_str(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_slice_retries_a_busy_map_without_waiting_or_partial_children() {
        let _guard = crate::gc::runtime_test_guard();
        crate::gc::willow_gc_init();
        let mut map = willow_map_new(0, 0, 1);
        crate::gc::willow_push_root(&mut map);
        let child = crate::gc::willow_alloc(8);
        willow_map_insert(map, 0, 0, child as i64, 1);
        let guard = unsafe { map_data(map) };
        let mut children = Vec::new();
        assert!(matches!(
            unsafe { snapshot_map_slice(map, 0, 512, &mut children) },
            crate::gc::TraceSliceProgress::Retry
        ));
        assert!(children.is_empty());
        drop(guard);
        assert!(matches!(
            unsafe { snapshot_map_slice(map, 0, 512, &mut children) },
            crate::gc::TraceSliceProgress::Done
        ));
        assert_eq!(children, [child]);
        crate::gc::willow_pop_root();
        crate::gc::willow_gc_collect();
    }
    use crate::gc::{
        runtime_test_guard, willow_gc_collect, willow_gc_init, willow_pop_roots, willow_push_root,
    };
    use crate::string::{willow_string_as_str, willow_string_from_str};

    fn opt_tag(opt: *mut u8) -> i64 {
        unsafe { *(opt as *const i64) }
    }
    fn opt_payload(opt: *mut u8) -> i64 {
        unsafe { *((opt as *const i64).add(1)) }
    }

    #[test]
    fn map_unit_01_new_is_empty() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let m = willow_map_new(0, 0, 0);
        assert!(!m.is_null());
        assert_eq!(willow_map_len(m), 0);
    }

    #[test]
    fn inline_maps_remain_stable_and_are_reclaimed_at_increasing_counts() {
        use crate::gc::{GC_HEADER_SIZE, willow_gc_allocated_bytes};

        let _guard = runtime_test_guard();
        for count in [1, 16, 128] {
            willow_gc_init();
            // Reserve every root slot before registering its address.
            let mut maps = vec![std::ptr::null_mut(); count];
            for slot in &mut maps {
                *slot = willow_map_new(3, 3, 1);
                assert!(!slot.is_null());
                assert!((*slot as usize).is_multiple_of(align_of::<Mutex<MapData>>()));
                willow_push_root(slot);
            }
            let payload_bytes = size_of::<Mutex<MapData>>();
            assert_eq!(
                willow_gc_allocated_bytes() as usize,
                count * (GC_HEADER_SIZE + payload_bytes)
            );
            let addresses = maps.clone();
            for &map in &maps {
                let mut key = willow_string_from_str("owned-key");
                willow_push_root(&mut key);
                let value = willow_string_from_str("traced-value");
                willow_map_insert(map, key as i64, 1, value as i64, 1);
                willow_pop_roots(1);
            }
            willow_gc_collect();
            assert_eq!(maps, addresses, "inline mutexes must never move");
            for &map in &maps {
                let key = willow_string_from_str("owned-key");
                let value = willow_map_get(map, key as i64, 1, 1);
                assert_eq!(unsafe { willow_string_as_str(value) }, "traced-value");
            }
            willow_pop_roots(count as i32);
            willow_gc_collect();
            assert_eq!(willow_gc_allocated_bytes(), 0);
        }
    }

    #[test]
    fn map_unit_02_int_key_insert_get() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let m = willow_map_new(0, 0, 0);
        willow_map_insert(m, 7, 0, 100, 0);
        willow_map_insert(m, 8, 0, 200, 0);
        assert_eq!(willow_map_len(m), 2);
        let g = willow_map_get(m, 7, 0, 0);
        assert_eq!(opt_tag(g), 0); // Some
        assert_eq!(opt_payload(g), 100);
        assert_eq!(opt_tag(willow_map_get(m, 99, 0, 0)), 1); // None
    }

    #[test]
    fn map_unit_03_insert_overwrites() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let m = willow_map_new(0, 0, 0);
        willow_map_insert(m, 1, 0, 10, 0);
        willow_map_insert(m, 1, 0, 20, 0);
        assert_eq!(willow_map_len(m), 1);
        assert_eq!(opt_payload(willow_map_get(m, 1, 0, 0)), 20);
    }

    #[test]
    fn map_unit_04_string_keys_compare_by_content() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let m = willow_map_new(3, 0, 0);
        let alice = willow_string_from_str("Alice");
        willow_map_insert(m, alice as i64, 1, 30, 0);
        // A *different* string object with the same content must hit.
        let alice2 = willow_string_from_str("Alice");
        let g = willow_map_get(m, alice2 as i64, 1, 0);
        assert_eq!(opt_tag(g), 0);
        assert_eq!(opt_payload(g), 30);
        let bob = willow_string_from_str("Bob");
        assert_eq!(opt_tag(willow_map_get(m, bob as i64, 1, 0)), 1); // None
    }

    #[test]
    fn map_unit_05_contains() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let m = willow_map_new(0, 0, 0);
        willow_map_insert(m, 5, 0, 50, 0);
        assert_eq!(willow_map_contains(m, 5, 0), 1);
        assert_eq!(willow_map_contains(m, 6, 0), 0);
    }

    #[test]
    fn map_unit_06_reference_values_survive_collection() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut m = willow_map_new(0, 3, 1);
        willow_push_root(&mut m as *mut *mut u8);
        let v = willow_string_from_str("kept-value");
        willow_map_insert(m, 1, 0, v as i64, 1);
        willow_gc_collect();
        let g = willow_map_get(m, 1, 0, 0);
        assert_eq!(opt_tag(g), 0);
        let got = opt_payload(g) as *mut u8;
        assert_eq!(unsafe { willow_string_as_str(got) }, "kept-value");
        willow_pop_roots(1);
    }

    #[test]
    fn map_unit_07_reference_option_uses_nullable_pointer_niche() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let m = willow_map_new(0, 3, 1);
        let value = willow_string_from_str("niche-value");
        willow_map_insert(m, 1, 0, value as i64, 1);
        assert_eq!(willow_map_get(m, 1, 0, 1), value);
        assert!(willow_map_get(m, 2, 0, 1).is_null());
    }
    #[test]
    fn map_layout_survives_freeze_and_drives_word_display() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let map = willow_map_new(1, 2, 0);
        willow_map_insert(map, 1.5f64.to_bits() as i64, 0, 1, 0);
        let copy = willow_map_copy(map);
        let text = willow_map_to_string(copy);
        assert_eq!(unsafe { willow_string_as_str(text) }, "{1.5: true}");
        let layout = unsafe { map_data(copy) }.layout;
        assert_eq!(
            (layout.key_kind, layout.value_kind, layout.value_is_ref),
            (1, 2, false)
        );
    }
    #[test]
    fn borrowed_string_keys_survive_copy_update_and_collection() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut map = willow_map_new(3, 0, 0);
        willow_push_root(&mut map);
        for (i, text) in ["", "日本語🦀", "ordinary"].into_iter().enumerate() {
            let key = willow_string_from_str(text);
            willow_map_insert(map, key as i64, 1, i as i64, 0);
        }
        let mut copy = willow_map_copy(map);
        willow_push_root(&mut copy);
        // No source key is rooted: map keys own their bytes independently.
        willow_gc_collect();
        for (i, text) in ["", "日本語🦀", "ordinary"].into_iter().enumerate() {
            let mut key = willow_string_from_str(text);
            willow_push_root(&mut key);
            assert_eq!(willow_map_contains(map, key as i64, 1), 1);
            assert_eq!(
                opt_payload(willow_map_get(copy, key as i64, 1, 0)),
                i as i64
            );
            willow_map_insert(map, key as i64, 1, 99, 0);
            assert_eq!(
                opt_payload(willow_map_get(copy, key as i64, 1, 0)),
                i as i64
            );
            assert_eq!(opt_payload(willow_map_get(map, key as i64, 1, 0)), 99);
            willow_pop_roots(1);
        }
        assert_eq!(willow_map_len(map), 3);
        willow_pop_roots(2);
    }

    #[test]
    fn borrowed_word_keys_preserve_all_bits() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let map = willow_map_new(0, 0, 0);
        let words = [
            0,
            i64::MIN,
            0x7ff8_0000_0000_0001,
            0x7ff8_0000_0000_0002,
            -1,
        ];
        for (i, word) in words.into_iter().enumerate() {
            willow_map_insert(map, word, 0, i as i64, 0);
        }
        for (i, word) in words.into_iter().enumerate() {
            assert_eq!(willow_map_contains(map, word, 0), 1);
            assert_eq!(opt_payload(willow_map_get(map, word, 0, 0)), i as i64);
        }
        assert_eq!(willow_map_contains(map, 1, 0), 0);
        assert_eq!(willow_map_len(map), words.len() as i64);
    }

    #[test]
    fn float_keys_normalize_zero_preserve_numbers_and_copy() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut map = willow_map_new(1, 0, 0);
        willow_push_root(&mut map);
        for zeros in [[0.0_f64, -0.0], [-0.0_f64, 0.0]] {
            for (index, key) in zeros.into_iter().enumerate() {
                willow_map_insert(map, key.to_bits() as i64, 0, index as i64 + 10, 0);
            }
            assert_eq!(willow_map_len(map), 1);
            for key in zeros {
                assert_eq!(willow_map_contains(map, key.to_bits() as i64, 0), 1);
                assert_eq!(
                    opt_payload(willow_map_get(map, key.to_bits() as i64, 0, 0)),
                    11
                );
            }
        }
        let values = [
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::MAX,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            -f64::from_bits(1),
            1.5,
            -1.5,
        ];
        for (index, value) in values.into_iter().enumerate() {
            willow_map_insert(map, value.to_bits() as i64, 0, index as i64, 0);
        }
        let mut copy = willow_map_copy(map);
        willow_push_root(&mut copy);
        assert_eq!(willow_map_len(copy), 9);
        for (index, value) in values.into_iter().enumerate() {
            assert_eq!(
                opt_payload(willow_map_get(copy, value.to_bits() as i64, 0, 0)),
                index as i64
            );
        }
        assert_eq!(opt_payload(willow_map_get(copy, i64::MIN, 0, 0)), 11);
        assert!(!unsafe { willow_string_as_str(willow_map_to_string(copy)) }.contains("-0.0:"));
        willow_pop_roots(2);
    }

    #[test]
    fn float_keys_reject_all_nan_forms_without_mutating_or_holding_lock() {
        use crate::panic_context::*;
        let _guard = runtime_test_guard();
        willow_gc_init();
        let previous = replace_current_context(Some(std::sync::Arc::new(PanicContext::new(903))));
        let mut map = willow_map_new(1, 0, 0);
        willow_push_root(&mut map);
        willow_map_insert(map, 0, 0, 42, 0);
        for bits in [
            0x7ff0_0000_0000_0001u64,
            0x7ff8_0000_0000_0000,
            0x7fff_ffff_ffff_ffff,
        ] {
            for sign in [0, 1u64 << 63] {
                for operation in 0..4 {
                    let word = (bits | sign) as i64;
                    match operation {
                        0 => willow_map_insert(map, word, 0, 99, 0),
                        1 => assert_eq!(willow_map_contains(map, word, 0), 0),
                        _ => assert!(willow_map_get(map, word, 0, operation - 2).is_null()),
                    }
                    assert_eq!(willow_panic_depth(), 1);
                    willow_panic_enter_defer();
                    let info = willow_panic_recover();
                    willow_panic_leave_defer();
                    assert_eq!(
                        unsafe { panic_info_message(info) },
                        "NaN cannot be used as a Map key"
                    );
                    willow_panic_release_recovered(info);
                    assert_eq!(willow_map_len(map), 1);
                    assert_eq!(opt_payload(willow_map_get(map, 0, 0, 0)), 42);
                }
            }
        }
        willow_pop_roots(1);
        replace_current_context(previous);
    }

    #[test]
    fn float_key_normalization_scales_without_allocations() {
        use crate::scheduler::scaling_measurements::counting_allocator as counter;
        for count in [16usize, 128, 1024, 8192] {
            let allocations = counter::thread_allocations();
            for index in 0..count {
                let word = (index as f64).to_bits() as i64;
                assert!(
                    matches!(unsafe { key_from_word(word, 1) }, Some(KeyRef::Word(got)) if got == word)
                );
                // Integer keys that happen to encode NaN or -0.0 remain integers.
                assert!(matches!(
                    unsafe { key_from_word(i64::MIN, 0) },
                    Some(KeyRef::Word(i64::MIN))
                ));
                assert!(matches!(
                    unsafe { key_from_word(f64::NAN.to_bits() as i64, 0) },
                    Some(KeyRef::Word(_))
                ));
            }
            assert_eq!(counter::thread_allocations(), allocations);
            eprintln!("normalization calls={}, allocations=0", 3 * count);
        }
    }

    /// willow-9tls.8: `freeze()` copies the value words a moving collection
    /// leaves in the source, not the ones it read before that collection.
    ///
    /// The scenario is a worker that freezes a map while another thread stops
    /// the world for a minor collection. The rooted source map is pinned, but
    /// its young values are reachable only through its slots, so they are
    /// COPIED to the old generation and the source's slots are rewritten. A
    /// native snapshot of those slots taken before `willow_map_copy`'s own
    /// allocation would carry the pre-move addresses into the copy.
    ///
    /// Deterministic ordering: this thread registers as a mutator, so the
    /// collector cannot proceed until it parks; `alloc` stress makes the
    /// destination allocation reach `collect_internal`, whose first act under a
    /// pending stop is to park at the safepoint. The collection therefore runs
    /// exactly between the copy's allocation and its return.
    #[test]
    fn copy_stores_values_as_relocated_by_a_collection_inside_its_allocation() {
        use std::time::{Duration, Instant};
        const VALUES: i64 = 8;

        let _guard = runtime_test_guard();
        willow_gc_init();
        crate::gc::willow_gc_register_mutator();
        // Environment stress must not promote the values before the copy: the
        // point is that they are still young when the collection runs.
        crate::gc::set_gc_stress_for_test(Some(""));
        let mut tls = crate::gc::tlab_state_for_test();
        let mut map = willow_map_new(0, 4, 1);
        willow_push_root(&mut map);
        let mut young = Vec::new();
        for i in 0..VALUES {
            let value = crate::gc::willow_gc_alloc_slow(&mut tls, 42, 0, 8, 0);
            assert!(!value.is_null());
            unsafe { *(value as *mut i64) = 1000 + i };
            willow_map_insert(map, i, 0, value as i64, 1);
            young.push(value);
        }
        let moved_before = crate::gc::willow_gc_moved_objects();

        let collector = std::thread::spawn(|| crate::gc::willow_gc_minor_collect());
        // Wait for the COORDINATION flag, not the lock-free gate: the gate is
        // published first, and a safepoint reached between the two returns
        // without parking, which would leave the collector waiting forever.
        let deadline = Instant::now() + Duration::from_secs(30);
        while !crate::gc::stop_pending_for_test() {
            assert!(
                Instant::now() < deadline,
                "the collector never requested a stop"
            );
            std::thread::yield_now();
        }
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        let mut copy = willow_map_copy(map);
        crate::gc::set_gc_stress_for_test(None);
        collector.join().unwrap();
        willow_push_root(&mut copy);

        assert_eq!(
            crate::gc::willow_gc_moved_objects() - moved_before,
            VALUES,
            "every value was evacuated while the copy was in progress"
        );
        for i in 0..VALUES {
            let source = willow_map_get(map, i, 0, 1);
            let copied = willow_map_get(copy, i, 0, 1);
            assert_ne!(source, young[i as usize], "value {i} moved");
            assert_eq!(copied, source, "copy holds the relocated value {i}");
            assert_eq!(unsafe { *(copied as *const i64) }, 1000 + i);
        }
        assert_eq!(willow_map_len(copy), VALUES);
        willow_pop_roots(2);
        crate::gc::willow_gc_unregister_mutator();
    }

    /// The copy's own slots take the write barrier: a young value copied into
    /// the old destination puts the DESTINATION in the remembered set (the
    /// source already is), so a later minor collection rewrites the copy as
    /// well as the source. The copy is a direct root here, which would get it
    /// traced anyway; the remembered-set count is what shows the barrier ran.
    #[test]
    fn copy_of_young_values_is_updated_by_the_next_minor_collection() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        crate::gc::set_gc_stress_for_test(Some(""));
        let mut tls = crate::gc::tlab_state_for_test();
        let mut map = willow_map_new(0, 4, 1);
        willow_push_root(&mut map);
        let young = crate::gc::willow_gc_alloc_slow(&mut tls, 42, 0, 8, 0);
        unsafe { *(young as *mut i64) = 77 };
        willow_map_insert(map, 1, 0, young as i64, 1);
        let remembered = crate::gc::willow_gc_remembered_set_size();
        let mut copy = willow_map_copy(map);
        willow_push_root(&mut copy);
        crate::gc::set_gc_stress_for_test(None);
        assert_eq!(willow_map_get(copy, 1, 0, 1), young);
        assert_eq!(
            crate::gc::willow_gc_remembered_set_size(),
            remembered + 1,
            "the copy joined the remembered set"
        );

        crate::gc::willow_gc_minor_collect();

        let moved = willow_map_get(map, 1, 0, 1);
        assert_ne!(moved, young, "the young value was evacuated");
        assert_eq!(willow_map_get(copy, 1, 0, 1), moved);
        assert_eq!(unsafe { *(moved as *const i64) }, 77);
        // The copy is independent: dropping the source keeps the value alive
        // through the copy alone.
        willow_pop_roots(2);
        willow_push_root(&mut copy);
        crate::gc::willow_gc_collect();
        assert_eq!(
            unsafe { *(willow_map_get(copy, 1, 0, 1) as *const i64) },
            77
        );
        willow_pop_roots(1);
        // `tls` lives on this stack and is registered with the heap; drop the
        // registration before it goes out of scope.
        willow_gc_init();
    }

    /// A full collection inside the copy's allocation (non-moving) keeps every
    /// value: the only thing keeping them alive at that moment is the rooted
    /// source map.
    #[test]
    fn copy_keeps_string_values_across_a_full_collection_in_its_allocation() {
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut map = willow_map_new(3, 3, 1);
        willow_push_root(&mut map);
        for (key, value) in [("a", "alpha"), ("b", "beta"), ("c", "gamma")] {
            let mut key = willow_string_from_str(key);
            willow_push_root(&mut key);
            let value = willow_string_from_str(value);
            willow_map_insert(map, key as i64, 1, value as i64, 1);
            willow_pop_roots(1);
        }
        let majors = crate::gc::willow_gc_major_collections();
        crate::gc::set_gc_stress_for_test(Some("alloc"));
        let mut copy = willow_map_copy(map);
        crate::gc::set_gc_stress_for_test(None);
        willow_push_root(&mut copy);
        assert!(crate::gc::willow_gc_major_collections() > majors);
        willow_pop_roots(2);
        willow_push_root(&mut copy);
        crate::gc::willow_gc_collect();
        for (key, value) in [("a", "alpha"), ("b", "beta"), ("c", "gamma")] {
            let mut key = willow_string_from_str(key);
            willow_push_root(&mut key);
            let got = willow_map_get(copy, key as i64, 1, 1);
            assert_eq!(unsafe { willow_string_as_str(got) }, value);
            willow_pop_roots(1);
        }
        assert_eq!(willow_map_len(copy), 3);
        willow_pop_roots(1);
    }

    /// willow-ssl7.7: a read-only lookup must not copy the key. Counted per
    /// thread through the test binary's counting global allocator, after all
    /// setup allocations, so the numbers are this loop's own Rust allocation
    /// traffic and nothing else's.
    #[test]
    fn read_only_lookups_never_copy_the_key() {
        use crate::scheduler::scaling_measurements::counting_allocator as counter;
        const CALLS: usize = 1_000;
        let _guard = runtime_test_guard();
        willow_gc_init();
        let mut map = willow_map_new(3, 0, 0);
        willow_push_root(&mut map);
        let mut get_bytes_by_len: Vec<(usize, usize)> = Vec::new();
        for len in [8usize, 64, 1024] {
            let mut hit = willow_string_from_str(&"k".repeat(len));
            willow_push_root(&mut hit);
            willow_map_insert(map, hit as i64, 1, len as i64, 0);
            let mut miss = willow_string_from_str(&"m".repeat(len));
            willow_push_root(&mut miss);

            // `contains` returns a scalar, so the whole call is the lookup:
            // hits and misses alike must allocate nothing at any key length.
            let allocs = counter::thread_allocations();
            let bytes = counter::thread_bytes();
            for _ in 0..CALLS {
                assert_eq!(willow_map_contains(map, hit as i64, 1), 1);
                assert_eq!(willow_map_contains(map, miss as i64, 1), 0);
            }
            assert_eq!(
                (
                    counter::thread_allocations() - allocs,
                    counter::thread_bytes() - bytes
                ),
                (0, 0),
                "contains allocated at key length {len}"
            );

            // The same for `get`'s key lookup with its required `Option`
            // output excluded: borrow the key, probe the table, drop it.
            let data = unsafe { map_data(map) };
            let allocs = counter::thread_allocations();
            let bytes = counter::thread_bytes();
            for _ in 0..CALLS {
                let key = unsafe { key_from_word(hit as i64, 3) }.unwrap();
                assert_eq!(data.get(&key), Some(len as i64));
                let key = unsafe { key_from_word(miss as i64, 3) }.unwrap();
                assert_eq!(data.get(&key), None);
            }
            assert_eq!(
                (
                    counter::thread_allocations() - allocs,
                    counter::thread_bytes() - bytes
                ),
                (0, 0),
                "get key lookup allocated at key length {len}"
            );
            drop(data);

            // The public `get` still allocates its `Option` in the GC heap,
            // which in turn asks Rust for space. That stays under one Rust
            // allocation per call, so no per-call owned key hides in it.
            let allocs = counter::thread_allocations();
            let bytes = counter::thread_bytes();
            for _ in 0..CALLS {
                assert_eq!(
                    opt_payload(willow_map_get(map, hit as i64, 1, 0)),
                    len as i64
                );
                assert_eq!(opt_tag(willow_map_get(map, miss as i64, 1, 0)), 1);
            }
            let get_allocs = counter::thread_allocations() - allocs;
            let get_bytes = counter::thread_bytes() - bytes;
            assert!(
                get_allocs < 2 * CALLS,
                "get allocated {get_allocs} times for {} calls at key length {len}",
                2 * CALLS
            );
            get_bytes_by_len.push((len, get_bytes));
            willow_pop_roots(2);
        }
        // Copying each looked-up key would put `calls * len` bytes through the
        // allocator; the GC's own bookkeeping grows with the live heap instead,
        // so the totals stay far below that and the growth between the
        // shortest and longest key does too.
        let (long_len, long_bytes) = *get_bytes_by_len.last().expect("one length");
        assert!(
            long_bytes < 2 * CALLS * long_len,
            "{long_bytes} bytes for {} lookups of a {long_len}-byte key",
            2 * CALLS
        );
        let (short_len, short_bytes) = get_bytes_by_len[0];
        // Compare without subtracting: GC bookkeeping can make the short run
        // allocate slightly more than the long one.
        assert!(
            long_bytes < short_bytes + CALLS * (long_len - short_len),
            "get bytes went {short_bytes} -> {long_bytes} from key length {short_len} to {long_len}"
        );
        willow_pop_roots(1);
    }
}
