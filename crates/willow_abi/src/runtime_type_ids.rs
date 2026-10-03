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

/// How the collector discovers references. Custom callbacks expose mutable
/// slots; Bitmap uses the descriptor referenced by gc_ref_mask, not a callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceClass {
    Leaf,
    Custom,
    Bitmap,
}

/// Native-resource ownership, independent of the GC's dynamic owned-root bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnershipClass {
    GcOnly,
    NativeFinalizer,
}

/// Payload representation. Rust-private aggregates deliberately have no stable
/// size or field offsets: generated code may only pass their handles to the ABI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeLayout {
    OpaqueNative,
    /// A logical length word followed by 64-bit reference slots; capacity may
    /// exceed length. See array_layout for the shared physical constants.
    ReferenceArrayBuffer,
    /// Variable generated payload; gc_ref_mask addresses [count, bitmap...].
    BitmapPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeType {
    pub name: &'static str,
    pub type_id: u32,
    pub layout: TypeLayout,
    pub trace: TraceClass,
    pub ownership: OwnershipClass,
}

impl RuntimeType {
    /// Pin actual native hooks to the schema. Bitmap is collector-special and
    /// cannot be registered as a native callback type.
    pub const fn accepts_native_hooks(self, trace: bool, finalizer: bool) -> bool {
        let trace_matches = match self.trace {
            TraceClass::Leaf => !trace,
            TraceClass::Custom => trace,
            TraceClass::Bitmap => return false,
        };
        let ownership_matches = match self.ownership {
            OwnershipClass::GcOnly => !finalizer,
            OwnershipClass::NativeFinalizer => finalizer,
        };
        trace_matches && ownership_matches
    }
}

// Declare both the exported constants and the inventory from the same rows,
// so every added ID participates in the uniqueness/range contract tests.
macro_rules! runtime_type_ids {
    ($($(#[$meta:meta])* $name:ident = $value:expr => ($layout:ident, $trace:ident, $ownership:ident);)+) => {
        $($(#[$meta])* pub const $name: u32 = $value;)+
        pub const RUNTIME_TYPE_IDS: &[(&str, u32)] = &[
            $((stringify!($name), $name),)+
        ];
        pub const RUNTIME_TYPES: &[RuntimeType] = &[
            $(RuntimeType { name: stringify!($name), type_id: $name,
                layout: TypeLayout::$layout, trace: TraceClass::$trace,
                ownership: OwnershipClass::$ownership },)+
        ];
        /// Generated match lookup; native descriptors evaluate this at compile time.
        pub const fn runtime_type(type_id: u32) -> Option<RuntimeType> {
            match type_id {
                $($name => Some(RuntimeType { name: stringify!($name), type_id: $name,
                    layout: TypeLayout::$layout, trace: TraceClass::$trace,
                    ownership: OwnershipClass::$ownership }),)+
                _ => None,
            }
        }
    };
}

runtime_type_ids! {
    ARRAY_REF_TYPE_ID = 0xA22A_0001 => (ReferenceArrayBuffer, Custom, GcOnly);
    MAP_TYPE_ID = 0xA22A_0002 => (OpaqueNative, Custom, NativeFinalizer);
    CHANNEL_TYPE_ID = 0xC4A2_0001 => (OpaqueNative, Custom, NativeFinalizer);
    ASYNC_MUTEX_TYPE_ID = 0x10C4_0001 => (OpaqueNative, Custom, NativeFinalizer);
    ASYNC_RWLOCK_TYPE_ID = 0x10C4_0002 => (OpaqueNative, Custom, NativeFinalizer);
    BLOCKING_CELL_TYPE_ID = 0x10C4_0003 => (OpaqueNative, Custom, NativeFinalizer);
    BLOCKING_RW_CELL_TYPE_ID = 0x10C4_0004 => (OpaqueNative, Custom, NativeFinalizer);
    CANCELLATION_TOKEN_TYPE_ID = 0x4341_4E01 => (OpaqueNative, Leaf, NativeFinalizer);
    TASK_SCOPE_TYPE_ID = 0x5343_5001 => (OpaqueNative, Custom, NativeFinalizer);
    NETWORK_HANDLE_TYPE_ID = 0x4E45_5401 => (OpaqueNative, Leaf, NativeFinalizer);
    /// Reserved tracing type: gc_ref_mask points to a static descriptor of
    /// [bitmap_word_count, bitmap_word_0, ...]; layout_id remains a fingerprint.
    GC_BITMAP_TYPE_ID = 0xB17B_17B1 => (BitmapPayload, Bitmap, GcOnly);
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
