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
//! Keys may be `i64` or `String` (compared by content). Values are stored as
//! raw 64-bit words; `.get` returns a Willow `Option<V>` built directly here.

use crate::gc::{
    willow_alloc_object, willow_alloc_typed, willow_register_drop, willow_register_type,
};
use crate::string::willow_string_as_str;
use std::collections::HashMap;

/// `type_id` for maps. Distinct from the array type id and well above the
/// small, sequentially-assigned class type ids.
const MAP_TYPE_ID: u32 = 0xA22A_0002;

/// A key copied out of the Willow heap so the map owns it independently of the
/// GC. String keys compare by content (not pointer identity), which is what
/// `Map<String, V>` lookups require.
#[derive(PartialEq, Eq, Hash)]
enum MapKey {
    Int(i64),
    Str(String),
}

#[derive(Default)]
struct MapData {
    /// Whether values are GC references (recorded on insert, used by tracing
    /// and by `Some` construction in `get`).
    val_is_ref: bool,
    /// Key -> raw 64-bit value word.
    entries: HashMap<MapKey, i64>,
}

/// Build an owned key from a raw key word.
///
/// # Safety
/// When `key_is_ref` is nonzero, `word` must be a valid WillowString pointer.
unsafe fn key_from_word(word: i64, key_is_ref: i64) -> MapKey {
    if key_is_ref != 0 {
        let s = unsafe { willow_string_as_str(word as *const u8) };
        MapKey::Str(s.to_string())
    } else {
        MapKey::Int(word)
    }
}

/// Borrow the boxed `MapData` behind a map payload pointer.
///
/// # Safety
/// `map` must be a non-null map payload produced by [`willow_map_new`].
unsafe fn map_data<'a>(map: *mut u8) -> &'a mut MapData {
    let boxed = unsafe { *(map as *mut *mut MapData) };
    unsafe { &mut *boxed }
}

/// Trace hook: report reference-typed values as GC children.
unsafe fn trace_map(payload: *mut u8, children: &mut Vec<*mut u8>) {
    let data = unsafe { map_data(payload) };
    if data.val_is_ref {
        for &v in data.entries.values() {
            let p = v as *mut u8;
            if !p.is_null() {
                children.push(p);
            }
        }
    }
}

/// Finalizer hook: free the boxed `MapData` when the map is swept.
unsafe fn drop_map(payload: *mut u8) {
    let boxed = unsafe { *(payload as *mut *mut MapData) };
    if !boxed.is_null() {
        drop(unsafe { Box::from_raw(boxed) });
    }
}

/// Register the map trace and finalizer. Called on every `willow_map_new`
/// (idempotent): `willow_gc_init` clears the type registry, so a process-global
/// `Once` would fail to re-register after the first reset (e.g. in multi-init
/// test runs). Real programs init once, so the repeated insert is harmless.
fn ensure_registered() {
    willow_register_type(MAP_TYPE_ID, trace_map);
    willow_register_drop(MAP_TYPE_ID, drop_map);
}

/// Allocate an empty map. The payload is a single word holding the boxed
/// `MapData` pointer; `gc_ref_mask` is 0 because that word is a Rust pointer,
/// not a GC pointer (tracing happens through `trace_map`).
#[unsafe(no_mangle)]
pub extern "C" fn willow_map_new() -> *mut u8 {
    ensure_registered();
    let data = Box::into_raw(Box::new(MapData::default()));
    let map = willow_alloc_object(MAP_TYPE_ID as i64, 8);
    if map.is_null() {
        // Reclaim the box rather than leaking it.
        drop(unsafe { Box::from_raw(data) });
        return std::ptr::null_mut();
    }
    unsafe { *(map as *mut *mut MapData) = data };
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
    let data = unsafe { map_data(map) };
    data.val_is_ref = val_is_ref != 0;
    let key = unsafe { key_from_word(key_word, key_is_ref) };
    data.entries.insert(key, val_word);
}

/// Look up `key`, returning a Willow `Option<V>` (`Some(value)` or `None`).
#[unsafe(no_mangle)]
pub extern "C" fn willow_map_get(map: *mut u8, key_word: i64, key_is_ref: i64) -> *mut u8 {
    if map.is_null() {
        return alloc_none();
    }
    let data = unsafe { map_data(map) };
    let key = unsafe { key_from_word(key_word, key_is_ref) };
    match data.entries.get(&key) {
        Some(&v) => alloc_some(v, data.val_is_ref),
        None => alloc_none(),
    }
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
    i64::from(data.entries.contains_key(&key))
}

// Willow `Option` layout (must match the compiler's enum lowering):
//   Some(v) -> 2-word object [tag = 0, payload]
//   None    -> 1-word object [tag = 1]
// Variant tags follow declaration order in src/prelude.rs (`Some`, then `None`).

fn alloc_some(value_word: i64, val_is_ref: bool) -> *mut u8 {
    let mask: u64 = if val_is_ref { 0b10 } else { 0 };
    let opt = willow_alloc_typed(16, mask);
    if opt.is_null() {
        return std::ptr::null_mut();
    }
    unsafe {
        *(opt as *mut i64) = 0; // tag = Some
        *((opt as *mut i64).add(1)) = value_word; // payload
    }
    opt
}

fn alloc_none() -> *mut u8 {
    let opt = willow_alloc_typed(8, 0);
    if opt.is_null() {
        return std::ptr::null_mut();
    }
    unsafe { *(opt as *mut i64) = 1 }; // tag = None
    opt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gc::{willow_gc_collect, willow_gc_init, willow_pop_roots, willow_push_root};
    use crate::string::{willow_string_as_str, willow_string_from_str};

    fn opt_tag(opt: *mut u8) -> i64 {
        unsafe { *(opt as *const i64) }
    }
    fn opt_payload(opt: *mut u8) -> i64 {
        unsafe { *((opt as *const i64).add(1)) }
    }

    #[test]
    fn map_unit_01_new_is_empty() {
        unsafe { willow_gc_init() };
        let m = willow_map_new();
        assert!(!m.is_null());
        assert_eq!(willow_map_len(m), 0);
    }

    #[test]
    fn map_unit_02_int_key_insert_get() {
        unsafe { willow_gc_init() };
        let m = willow_map_new();
        willow_map_insert(m, 7, 0, 100, 0);
        willow_map_insert(m, 8, 0, 200, 0);
        assert_eq!(willow_map_len(m), 2);
        let g = willow_map_get(m, 7, 0);
        assert_eq!(opt_tag(g), 0); // Some
        assert_eq!(opt_payload(g), 100);
        assert_eq!(opt_tag(willow_map_get(m, 99, 0)), 1); // None
    }

    #[test]
    fn map_unit_03_insert_overwrites() {
        unsafe { willow_gc_init() };
        let m = willow_map_new();
        willow_map_insert(m, 1, 0, 10, 0);
        willow_map_insert(m, 1, 0, 20, 0);
        assert_eq!(willow_map_len(m), 1);
        assert_eq!(opt_payload(willow_map_get(m, 1, 0)), 20);
    }

    #[test]
    fn map_unit_04_string_keys_compare_by_content() {
        unsafe { willow_gc_init() };
        let m = willow_map_new();
        let alice = willow_string_from_str("Alice");
        willow_map_insert(m, alice as i64, 1, 30, 0);
        // A *different* string object with the same content must hit.
        let alice2 = willow_string_from_str("Alice");
        let g = willow_map_get(m, alice2 as i64, 1);
        assert_eq!(opt_tag(g), 0);
        assert_eq!(opt_payload(g), 30);
        let bob = willow_string_from_str("Bob");
        assert_eq!(opt_tag(willow_map_get(m, bob as i64, 1)), 1); // None
    }

    #[test]
    fn map_unit_05_contains() {
        unsafe { willow_gc_init() };
        let m = willow_map_new();
        willow_map_insert(m, 5, 0, 50, 0);
        assert_eq!(willow_map_contains(m, 5, 0), 1);
        assert_eq!(willow_map_contains(m, 6, 0), 0);
    }

    #[test]
    fn map_unit_06_reference_values_survive_collection() {
        unsafe { willow_gc_init() };
        let mut m = willow_map_new();
        willow_push_root(&mut m as *mut *mut u8);
        let v = willow_string_from_str("kept-value");
        willow_map_insert(m, 1, 0, v as i64, 1);
        willow_gc_collect();
        let g = willow_map_get(m, 1, 0);
        assert_eq!(opt_tag(g), 0);
        let got = opt_payload(g) as *mut u8;
        assert_eq!(unsafe { willow_string_as_str(got) }, "kept-value");
        willow_pop_roots(1);
    }
}
