//! GC-header type-ID namespace shared by generated code and native payloads.
//!
//! Zero means no specialized type (inline-mask tracing / no base class).
//! Generated classes occupy 1..=GENERATED_TYPE_ID_MAX. All higher values are
//! reserved for runtime contracts, including future native payloads. Keep
//! existing values stable: type IDs also participate in layout fingerprints.

pub const NO_TYPE_ID: u32 = 0;
pub const GENERATED_TYPE_ID_MIN: u32 = 1;
pub const GENERATED_TYPE_ID_MAX: u32 = 0x0FFF_FFFF;
pub const RUNTIME_TYPE_ID_MIN: u32 = 0x1000_0000;

// Declare both the exported constants and the inventory from the same rows,
// so every added ID participates in the uniqueness/range contract tests.
macro_rules! runtime_type_ids {
    ($($(#[$meta:meta])* $name:ident = $value:expr;)+) => {
        $($(#[$meta])* pub const $name: u32 = $value;)+
        pub const RUNTIME_TYPE_IDS: &[(&str, u32)] = &[
            $((stringify!($name), $name),)+
        ];
    };
}

runtime_type_ids! {
    ARRAY_REF_TYPE_ID = 0xA22A_0001;
    MAP_TYPE_ID = 0xA22A_0002;
    CHANNEL_TYPE_ID = 0xC4A2_0001;
    ASYNC_MUTEX_TYPE_ID = 0x10C4_0001;
    ASYNC_RWLOCK_TYPE_ID = 0x10C4_0002;
    BLOCKING_CELL_TYPE_ID = 0x10C4_0003;
    BLOCKING_RW_CELL_TYPE_ID = 0x10C4_0004;
    CANCELLATION_TOKEN_TYPE_ID = 0x4341_4E01;
    TASK_SCOPE_TYPE_ID = 0x5343_5001;
    NETWORK_HANDLE_TYPE_ID = 0x4E45_5401;
    /// Reserved tracing type: gc_ref_mask points to a static descriptor of
    /// [bitmap_word_count, bitmap_word_0, ...]; layout_id remains a fingerprint.
    GC_BITMAP_TYPE_ID = 0xB17B_17B1;
}

/// Convert a zero-based class ordinal without truncation or reserved-ID reuse.
/// Constant work and no allocation, including at namespace exhaustion.
pub const fn generated_type_id(ordinal: usize) -> Option<u32> {
    if ordinal < GENERATED_TYPE_ID_MAX as usize {
        Some(ordinal as u32 + GENERATED_TYPE_ID_MIN)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_ids_are_unique_and_outside_the_generated_range() {
        let mut seen = std::collections::HashSet::new();
        assert_eq!(NO_TYPE_ID, 0);
        assert_eq!(GENERATED_TYPE_ID_MIN, NO_TYPE_ID + 1);
        assert_eq!(GENERATED_TYPE_ID_MAX + 1, RUNTIME_TYPE_ID_MIN);
        for &(name, id) in RUNTIME_TYPE_IDS {
            assert!(id >= RUNTIME_TYPE_ID_MIN, "{name} overlaps generated IDs");
            assert!(seen.insert(id), "{name} duplicates a native ID");
        }
    }

    #[test]
    fn generated_ids_reject_reserved_values_and_integer_overflow() {
        assert_eq!(generated_type_id(0), Some(1));
        assert_eq!(
            generated_type_id(GENERATED_TYPE_ID_MAX as usize - 1),
            Some(GENERATED_TYPE_ID_MAX),
        );
        for ordinal in [
            GENERATED_TYPE_ID_MAX as usize,
            RUNTIME_TYPE_ID_MIN as usize,
            u32::MAX as usize,
            usize::MAX,
        ] {
            assert_eq!(generated_type_id(ordinal), None);
        }
        for &(_, id) in RUNTIME_TYPE_IDS {
            assert_eq!(generated_type_id(id as usize - 1), None);
        }
    }
}
