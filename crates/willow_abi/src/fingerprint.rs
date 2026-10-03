//! Versioned, deterministic ABI encoding. No Rust memory representation,
//! randomized Hasher, addresses, or host endianness enter the fingerprint.
//! FNV-1a is a compatibility checksum, not a security/authenticity mechanism.
//! Add new shared semantic constants here; opaque runtime-private fields are
//! intentionally excluded. Bump CONTRACT_REVISION for ABI rules not represented
//! by schema data (e.g. enum tag interpretation or native callback semantics).
use crate::schema::RuntimeAbiSchema;
use crate::*;

pub const CONTRACT_REVISION: u64 = 1;

/// All currently supported executable targets use 64-bit pointers.
pub const ABI_HASH_64: u64 = abi_hash(&RUNTIME_ABI, 8, CONTRACT_CONSTANTS);

const CONTRACT_CONSTANTS: &[u64] = &[
    CONTRACT_REVISION,
    OBJECT_LAYOUT_REVISION as u64,
    GC_REF_MASK_BITS as u64,
    EnumVariantLayout::TAG_WORDS as u64,
    storage_word_bytes(8) as u64,
    array_layout::WORD_BYTES as u64,
    array_layout::HANDLE_MASK,
    dispatch_layout::CLASS_ID_BYTES as u64,
    dispatch_layout::INTERFACE_GC_REF_MASK,
    gc_header::MARKED_OFFSET as u64,
    gc_header::ALLOCATED_OFFSET as u64,
    gc_header::GENERATION_OFFSET as u64,
    gc_header::AGE_OFFSET as u64,
    gc_header::OWNED_OFFSET as u64,
    gc_header::REMEMBERED_OFFSET as u64,
    gc_header::DESCRIPTOR_OFFSET as u64,
    gc_header::GENERATION_YOUNG as u64,
    gc_header::GENERATION_OLD as u64,
    tlab::MAX_OBJECT_SIZE as u64,
    tlab::CHUNK_SIZE as u64,
    tlab::MARK_GRANULE_BYTES as u64,
    frame_status::TERMINAL_MASK as u64,
    frame_status::CANCEL_REQUESTED as u64,
    runtime_type_ids::NO_TYPE_ID as u64,
    runtime_type_ids::GENERATED_TYPE_ID_MIN as u64,
    runtime_type_ids::GENERATED_TYPE_ID_MAX as u64,
    runtime_type_ids::RUNTIME_TYPE_ID_MIN as u64,
    RuntimePollResult::Pending as u64,
    RuntimePollResult::Ready as u64,
    RuntimePollResult::Yield as u64,
    RuntimePollResult::Preempted as u64,
    RuntimePollResult::Panicked as u64,
    RuntimePollResult::BlockedSyscall as u64,
    FrameTerminalStatus::Pending as u64,
    FrameTerminalStatus::Completed as u64,
    FrameTerminalStatus::Cancelled as u64,
    FrameTerminalStatus::Panicked as u64,
    LockAcquireStatus::Cancelled as u64,
    LockAcquireStatus::Lost as u64,
    LockAcquireStatus::Recursive as u64,
    LockAcquireStatus::Pending as u64,
    LockAcquireStatus::Acquired as u64,
    LockStatusPhase::Acquire as u64,
    LockStatusPhase::Poll as u64,
    GcObjectKind::Class as u64,
    GcObjectKind::Enum as u64,
    GcObjectKind::InterfaceBox as u64,
    GcObjectKind::Range as u64,
    GcObjectKind::AsyncFrame as u64,
    GcObjectKind::ArrayHandle as u64,
    GcObjectKind::ArrayBuffer as u64,
    GcObjectKind::Map as u64,
    GcObjectKind::String as u64,
    GcObjectKind::Channel as u64,
    GcObjectKind::AtomicCell as u64,
    GcObjectKind::LockHandle as u64,
    GcObjectKind::Closure as u64,
    GcStoreDestination::ObjectField as u64,
    GcStoreDestination::ArrayElement as u64,
    GcStoreDestination::MapValue as u64,
    GcStoreDestination::EnumPayload as u64,
    GcStoreDestination::InterfaceObject as u64,
    GcStoreDestination::AsyncFrameSlot as u64,
    GcStoreDestination::IndirectReference as u64,
    GcStoreDestination::GlobalStatic as u64,
    GcStoreDestination::ContainerInternal as u64,
    GcStoreDestination::AsyncMutexCell as u64,
    GcStoreDestination::AsyncRwLockCell as u64,
    GcStoreDestination::BlockingCell as u64,
    GcStoreDestination::BlockingRwCell as u64,
];

struct Digest(u64);
impl Digest {
    const fn byte(&mut self, byte: u8) {
        self.0 = (self.0 ^ byte as u64).wrapping_mul(0x100_0000_01b3);
    }
    const fn word(&mut self, word: u64) {
        let bytes = word.to_le_bytes();
        let mut i = 0;
        while i < bytes.len() {
            self.byte(bytes[i]);
            i += 1;
        }
    }
    const fn string(&mut self, value: &str) {
        self.word(value.len() as u64);
        let mut i = 0;
        while i < value.len() {
            self.byte(value.as_bytes()[i]);
            i += 1;
        }
    }
}

// Explicit wire codes: never depend on a Rust enum's implicit discriminants.
const fn ty_code(ty: AbiTy) -> u64 {
    match ty {
        AbiTy::Word => 1,
        AbiTy::I64 => 2,
        AbiTy::I32 => 3,
        AbiTy::I8 => 4,
        AbiTy::F64 => 5,
        AbiTy::Ptr => 6,
    }
}

const fn abi_hash(schema: &RuntimeAbiSchema, pointer_bytes: u32, constants: &[u64]) -> u64 {
    hash_contract(
        schema.symbols,
        schema.types,
        schema.layouts,
        pointer_bytes,
        constants,
    )
}

const fn hash_contract(
    symbols: &[RuntimeSymbol],
    types: &[runtime_type_ids::RuntimeType],
    layouts: &[schema::SharedLayout],
    pointer_bytes: u32,
    constants: &[u64],
) -> u64 {
    let mut hash = Digest(0xcbf2_9ce4_8422_2325);
    hash.string("Willow ABI fingerprint v1");
    hash.word(pointer_bytes as u64);
    hash.word(constants.len() as u64);
    let mut i = 0;
    while i < constants.len() {
        hash.word(constants[i]);
        i += 1;
    }
    hash.string(GC_MARK_PHASE_SYMBOL);
    hash.word(symbols.len() as u64);
    i = 0;
    while i < symbols.len() {
        let row = &symbols[i];
        hash.string(row.name);
        hash.word(row.params.len() as u64);
        let mut j = 0;
        while j < row.params.len() {
            hash.word(ty_code(row.params[j]));
            j += 1;
        }
        hash.word(match row.ret {
            Some(ty) => ty_code(ty),
            None => 0,
        });
        hash.word(row.effects.bits() as u64);
        i += 1;
    }
    hash.word(types.len() as u64);
    i = 0;
    while i < types.len() {
        use runtime_type_ids::{OwnershipClass, TraceClass, TypeLayout};
        let row = &types[i];
        hash.string(row.name);
        hash.word(row.type_id as u64);
        hash.word(match row.layout {
            TypeLayout::OpaqueNative => 1,
            TypeLayout::ReferenceArrayBuffer => 2,
            TypeLayout::BitmapPayload => 3,
        });
        hash.word(match row.trace {
            TraceClass::Leaf => 1,
            TraceClass::Custom => 2,
            TraceClass::Bitmap => 3,
        });
        hash.word(match row.ownership {
            OwnershipClass::GcOnly => 1,
            OwnershipClass::NativeFinalizer => 2,
        });
        i += 1;
    }
    hash.word(layouts.len() as u64);
    i = 0;
    while i < layouts.len() {
        let layout = layouts[i];
        hash.word(layout.size(pointer_bytes) as u64);
        hash.word(layout.fields().len() as u64);
        let mut field = 0;
        while field < layout.fields().len() {
            hash.string(layout.fields()[field]);
            hash.word(layout.field_offset(field, pointer_bytes).unwrap() as u64);
            field += 1;
        }
        i += 1;
    }
    hash.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_hash_covers_every_contract_constant() {
        for i in 0..CONTRACT_CONSTANTS.len() {
            let mut changed = CONTRACT_CONSTANTS.to_vec();
            changed[i] ^= 1;
            assert_ne!(
                ABI_HASH_64,
                abi_hash(&RUNTIME_ABI, 8, &changed),
                "constant {i}"
            );
        }
    }

    #[test]
    fn abi_hash_is_repeatable_and_target_sensitive() {
        assert_eq!(ABI_HASH_64, abi_hash(&RUNTIME_ABI, 8, CONTRACT_CONSTANTS));
        assert_ne!(ABI_HASH_64, abi_hash(&RUNTIME_ABI, 4, CONTRACT_CONSTANTS));
        // Fixed encoding vector, independent of platform and Rust Hash implementation.
        let mut digest = Digest(0xcbf2_9ce4_8422_2325);
        digest.string("hello");
        digest.word(0x0102030405060708);
        assert_eq!(digest.0, 0xe743c7e10836a280);
    }

    #[test]
    fn abi_hash_covers_each_signature_and_effect() {
        for i in 0..RUNTIME_ABI.symbols.len() {
            for change in 0..4 {
                let mut rows = RUNTIME_ABI.symbols.to_vec();
                match change {
                    0 => rows[i].name = "changed",
                    1 => rows[i].params = &[AbiTy::I8, AbiTy::F64, AbiTy::Ptr],
                    2 => {
                        rows[i].ret = if rows[i].ret.is_some() {
                            None
                        } else {
                            Some(AbiTy::Ptr)
                        }
                    }
                    _ => {
                        rows[i].effects = if rows[i].effects.is_empty() {
                            RuntimeEffects::ALL
                        } else {
                            RuntimeEffects::NONE
                        }
                    }
                }
                assert_ne!(
                    ABI_HASH_64,
                    hash_contract(
                        &rows,
                        RUNTIME_ABI.types,
                        RUNTIME_ABI.layouts,
                        8,
                        CONTRACT_CONSTANTS
                    ),
                    "symbol {i}, change {change}"
                );
            }
        }
    }

    #[test]
    fn abi_hash_covers_layout_inventory_and_type_metadata() {
        use runtime_type_ids::{OwnershipClass, TraceClass, TypeLayout};
        for i in 0..RUNTIME_ABI.types.len() {
            for change in 0..5 {
                let mut rows = RUNTIME_ABI.types.to_vec();
                match change {
                    0 => rows[i].type_id ^= 1,
                    1 => rows[i].name = "changed",
                    2 => {
                        rows[i].layout = if rows[i].layout == TypeLayout::OpaqueNative {
                            TypeLayout::BitmapPayload
                        } else {
                            TypeLayout::OpaqueNative
                        }
                    }
                    3 => {
                        rows[i].trace = if rows[i].trace == TraceClass::Leaf {
                            TraceClass::Custom
                        } else {
                            TraceClass::Leaf
                        }
                    }
                    _ => {
                        rows[i].ownership = if rows[i].ownership == OwnershipClass::GcOnly {
                            OwnershipClass::NativeFinalizer
                        } else {
                            OwnershipClass::GcOnly
                        }
                    }
                }
                assert_ne!(
                    ABI_HASH_64,
                    hash_contract(
                        RUNTIME_ABI.symbols,
                        &rows,
                        RUNTIME_ABI.layouts,
                        8,
                        CONTRACT_CONSTANTS
                    )
                );
            }
        }
        for i in 0..RUNTIME_ABI.layouts.len() {
            let mut layouts = RUNTIME_ABI.layouts.to_vec();
            layouts.remove(i);
            assert_ne!(
                ABI_HASH_64,
                hash_contract(
                    RUNTIME_ABI.symbols,
                    RUNTIME_ABI.types,
                    &layouts,
                    8,
                    CONTRACT_CONSTANTS
                )
            );
        }
    }

    #[test]
    fn abi_hash_distinguishes_parameter_order_and_semantic_kinds() {
        let baseline = RuntimeSymbol {
            name: "test",
            params: &[AbiTy::Word, AbiTy::Ptr],
            ret: None,
            effects: RuntimeEffects::NONE,
        };
        let fingerprint = |row| hash_contract(&[row], &[], &[], 8, &[]);
        for params in [
            &[AbiTy::Ptr, AbiTy::Word][..],
            &[AbiTy::I64, AbiTy::Ptr],
            &[AbiTy::Word],
            &[],
        ] {
            assert_ne!(
                fingerprint(baseline),
                fingerprint(RuntimeSymbol { params, ..baseline })
            );
        }
    }
}
