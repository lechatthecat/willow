//! GC-managed hash map `Map<K, V>`.
//!
//! The map is a thin GC object whose single payload word holds a raw pointer to
//! a boxed [`MapData`] (a Rust `HashMap`). Two GC hooks keep it correct:
//!
//! * a trace function reports reference-typed *values* so they stay alive while
//!   the map is reachable (keys are copied out of the Willow heap, so they need
//!   no tracing — see [`MapKey`]);
//! * a finalizer frees the boxed `MapData` when the map is swept, so the Rust
//!   allocation does not leak.
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

/// `type_id` for maps. Distinct from the array type id and well above the
/// small, sequentially-assigned class type ids.
const MAP_TYPE_ID: u32 = 0xA22A_0002;

/// A key copied out of the Willow heap so the map owns it independently of the
/// GC. String keys compare by content (not pointer identity), which is what
/// `Map<String, V>` lookups require. Every other admitted key is one word, so
/// `Word` holds it verbatim: an `i64`, a `bool` as 0/1, or the BITS of an `f64`
/// (which is why `Map<f64, V>` matches keys bit-for-bit, and so distinguishes
/// `0.0` from `-0.0`).
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
    entries: HashMap<MapKey, i64>,
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
/// When `key_is_ref` is nonzero, `word` must be a valid WillowString pointer
/// for the returned borrow's lifetime. Do not allocate in the GC while borrowed.
unsafe fn key_from_word<'a>(word: i64, key_is_ref: i64) -> KeyRef<'a> {
    if key_is_ref != 0 {
        KeyRef::Str(unsafe { willow_string_as_str(word as *const u8) })
    } else {
        KeyRef::Word(word)
    }
}

/// Borrow the boxed `MapData` behind a map payload pointer.
///
/// # Safety
/// `map` must be a non-null map payload produced by [`willow_map_new`].
unsafe fn map_data<'a>(map: *mut u8) -> MutexGuard<'a, MapData> {
    let boxed = unsafe { *(map as *mut *mut Mutex<MapData>) };
    unsafe { &*boxed }.lock().unwrap()
}

/// Trace hook: report reference-typed values as GC children.
unsafe fn trace_map(payload: *mut u8, slots: &mut Vec<*mut *mut u8>) {
    let mut data = unsafe { map_data(payload) };
    if data.layout.value_is_ref {
        for value in data.entries.values_mut() {
            slots.push((value as *mut i64).cast::<*mut u8>());
        }
    }
}

unsafe fn snapshot_map(payload: *mut u8, children: &mut Vec<*mut u8>) {
    let data = unsafe { map_data(payload) };
    if data.layout.value_is_ref {
        children.extend(data.entries.values().map(|value| *value as *mut u8));
    }
}

/// Finalizer hook: free the boxed `MapData` when the map is swept.
unsafe fn drop_map(payload: *mut u8) {
    let boxed = unsafe { *(payload as *mut *mut Mutex<MapData>) };
    if !boxed.is_null() {
        drop(unsafe { Box::from_raw(boxed) });
    }
}

/// Register the map trace and finalizer. Called on every `willow_map_new`
/// (idempotent): `willow_gc_init` clears the type registry, so a process-global
/// `Once` would fail to re-register after the first reset (e.g. in multi-init
/// test runs). Real programs init once, so the repeated insert is harmless.
static MAP_REGISTRATION: crate::gc::NativeGcRegistration = crate::gc::NativeGcRegistration::new();
const MAP_GC_TYPES: &[crate::gc::NativeGcType] =
    &[
        crate::gc::NativeGcType::new(MAP_TYPE_ID, Some(trace_map), Some(drop_map))
            .with_concurrent_trace(snapshot_map),
    ];

fn ensure_registered() {
    MAP_REGISTRATION.ensure(MAP_GC_TYPES);
}

/// Allocate an empty map. The payload is a single word holding the boxed
/// `MapData` pointer; `gc_ref_mask` is 0 because that word is a Rust pointer,
/// not a GC pointer (tracing happens through `trace_map`).
#[unsafe(no_mangle)]
pub extern "C" fn willow_map_new(key_kind: i64, value_kind: i64, value_is_ref: i64) -> *mut u8 {
    ensure_registered();
    let data = Box::into_raw(Box::new(Mutex::new(MapData {
        layout: MapLayout {
            key_kind,
            value_kind,
            value_is_ref: value_is_ref != 0,
        },
        entries: HashMap::new(),
    })));
    let map = willow_alloc_with_layout(GcObjectKind::Map, MAP_TYPE_ID, 8, 0);
    if map.is_null() {
        // Reclaim the box rather than leaking it.
        drop(unsafe { Box::from_raw(data) });
        return std::ptr::null_mut();
    }
    unsafe { *(map as *mut *mut Mutex<MapData>) = data };
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
    let key = unsafe { key_from_word(key_word, key_is_ref) };
    let owned_key = match key {
        KeyRef::Word(word) => MapKey::Word(word),
        KeyRef::Str(text) => MapKey::Str(text.to_owned()),
    };
    if data.layout.value_is_ref {
        willow_gc_write_barrier(
            map,
            val_word as *mut u8,
            GcStoreDestination::MapValue as i64,
        );
    }
    data.entries.insert(owned_key, val_word);
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
    let key = unsafe { key_from_word(key_word, key_is_ref) };
    let value = data.entries.get(&key as &dyn KeyView).copied();
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
    // Snapshot the source entries into owned Rust data first; the value words are
    // kept alive by the still-rooted source map across the `willow_map_new`
    // allocation below.
    let (layout, entries): (MapLayout, Vec<(MapKey, i64)>) = {
        let src = unsafe { map_data(map) };
        (
            src.layout,
            src.entries.iter().map(|(k, &v)| (k.clone(), v)).collect(),
        )
    };
    let copy = willow_map_new(
        layout.key_kind,
        layout.value_kind,
        i64::from(layout.value_is_ref),
    );
    if copy.is_null() {
        return std::ptr::null_mut();
    }
    let mut dst = unsafe { map_data(copy) };
    for (k, v) in entries {
        if layout.value_is_ref {
            willow_gc_write_barrier(copy, v as *mut u8, GcStoreDestination::MapValue as i64);
        }
        dst.entries.insert(k, v);
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
    let key = unsafe { key_from_word(key_word, key_is_ref) };
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

/// Debug display of a whole map: `{a: 1, b: 2}` — entries sorted by key for
/// deterministic output (HashMap iteration order is not stable). Kinds follow
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
        unsafe { map_data(map) }
            .entries
            .iter()
            .map(|(k, &v)| {
                let key = match k {
                    MapKey::Word(n) => crate::array::element_word_to_string(*n, key_kind),
                    MapKey::Str(s) => s.clone(),
                };
                (key, v)
            })
            .collect()
    };
    entries.sort_by(|a, b| a.0.cmp(&b.0));
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
        let map = willow_map_new(1, 0, 0);
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
                let key = unsafe { key_from_word(hit as i64, 1) };
                assert_eq!(
                    data.entries.get(&key as &dyn KeyView).copied(),
                    Some(len as i64)
                );
                let key = unsafe { key_from_word(miss as i64, 1) };
                assert_eq!(data.entries.get(&key as &dyn KeyView).copied(), None);
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
