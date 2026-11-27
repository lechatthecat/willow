use super::*;
use crate::gc::*;

#[test]
fn string_keys_growth_updates_copy_telemetry_and_finalizers() {
    let _guard = isolated_runtime();
    for count in [1, 32, 1024] {
        // SAFETY: The test controls its isolated runtime at this reset boundary; no other
        // mutator or subsequent access relies on the discarded heap.
        unsafe { willow_gc_init() };
        let baseline = crate::gc_telemetry::external_bytes();
        let mut map = willow_map_new(3, 0, 0);
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut map) };
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
        let before = crate::gc_telemetry::snapshot();
        let mut key_bytes = 0;
        for index in 0..count {
            let text = format!("{index:08}-{}", "x".repeat(256));
            key_bytes += text.len() as u64;
            let mut key = crate::string::willow_string_from_str(&text);
            // SAFETY: The initialized slot stays at this address until its registration
            // is popped; collection may rewrite it there.
            unsafe { willow_push_root(&raw mut key) };
            // SAFETY: The constructor and reference flags agree. Each pointer word comes
            // from a live fixture object at the call.
            unsafe { willow_map_insert(map, key as i64, 1, index, 0) };
            let charged = crate::gc_telemetry::external_bytes();
            // SAFETY: The constructor and reference flags agree. Each pointer word comes
            // from a live fixture object at the call.
            unsafe { willow_map_insert(map, key as i64, 1, index + 1, 0) };
            assert_eq!(
                crate::gc_telemetry::external_bytes(),
                charged,
                "replacement grew native storage"
            );
            // SAFETY: The removed shadow-stack entries belong to this operation and their
            // slot storage is still live.
            unsafe { willow_pop_root() };
        }
        let bytes = crate::gc_telemetry::external_bytes() - baseline;
        assert!(bytes >= key_bytes);
        let stats = crate::gc_telemetry::snapshot();
        assert!(stats.heap.reserved_bytes >= before.heap.reserved_bytes);
        assert_eq!(
            crate::gc_telemetry::native_memory_snapshot().requested_bytes,
            baseline + bytes
        );
        assert_eq!(
            stats.heap.reserved_bytes,
            stats.heap.old_reserved_bytes + stats.heap.nursery_reserved_bytes
        );
        willow_gc_collect();
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline + bytes);
        // SAFETY: The source is a live map from the constructor and remains reachable
        // while copied.
        let mut copy = unsafe { willow_map_copy(map) };
        // SAFETY: The initialized slot stays at this address until its registration is
        // popped; collection may rewrite it there.
        unsafe { willow_push_root(&raw mut copy) };
        assert!(crate::gc_telemetry::external_bytes() >= baseline + bytes + key_bytes);
        // SAFETY: The handle is a live map from this fixture or its frozen copy.
        assert_eq!(unsafe { willow_map_len(copy) }, count);
        // SAFETY: The removed shadow-stack entries belong to this operation and their
        // slot storage is still live.
        unsafe { willow_pop_roots(2) };
        willow_gc_collect();
        assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
        println!("native map entries={count} owned_key_bytes={key_bytes} charged_bytes={bytes}");
    }
}

#[test]
fn reset_releases_native_map_storage() {
    let _guard = isolated_runtime();
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    let baseline = crate::gc_telemetry::external_bytes();
    let map = willow_map_new(0, 0, 0);
    for key in 0..128 {
        // SAFETY: This live map was constructed with scalar key and value slots.
        unsafe { willow_map_insert(map, key, 0, key, 0) };
    }
    assert!(crate::gc_telemetry::external_bytes() > baseline);
    // SAFETY: The test controls its isolated runtime at this reset boundary; no other
    // mutator or subsequent access relies on the discarded heap.
    unsafe { willow_gc_init() };
    assert_eq!(crate::gc_telemetry::external_bytes(), baseline);
}
