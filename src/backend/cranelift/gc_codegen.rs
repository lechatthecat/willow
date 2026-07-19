//! Central compiler-side GC allocation and reference-store lowering.
//!
//! Stage 2 keeps the current non-moving mark-and-sweep semantics, but makes the
//! two mutation points future collectors need explicit:
//!
//! - every generated GC allocation goes through [`FuncGen::emit_gc_alloc`];
//! - every generated GC-reference heap store goes through
//!   [`FuncGen::emit_gc_heap_store`].
//!
//! Later stages can add a TLAB fast path and real write barriers here without
//! redistributing collector policy through expression-specific emitters.

use cranelift_codegen::ir::{MemFlagsData, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::Module;

use super::*;

/// Broad object shape used when deriving the opaque runtime layout id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u64)]
pub(super) enum GcObjectKind {
    Class = 1,
    Enum = 2,
    InterfaceBox = 3,
    Range = 4,
}

/// Compiler-owned layout metadata for one allocation site.
///
/// `layout_id` is a stable fingerprint of the current shape, not a registry
/// index. The runtime treats it as opaque today; a future layout registry may
/// replace the fingerprint without changing allocation call sites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct GcLayoutMetadata {
    pub(super) kind: GcObjectKind,
    pub(super) payload_size: i64,
    pub(super) runtime_type_id: i64,
    pub(super) gc_ref_mask: u64,
    pub(super) layout_id: u64,
}

impl GcLayoutMetadata {
    pub(super) fn new(
        kind: GcObjectKind,
        payload_size: i64,
        runtime_type_id: i64,
        gc_ref_mask: u64,
    ) -> Self {
        debug_assert!(payload_size >= 0);
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for word in [
            kind as u64,
            payload_size as u64,
            runtime_type_id as u64,
            gc_ref_mask,
        ] {
            hash ^= word;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        // Zero is reserved for legacy/opaque allocation wrappers.
        if hash == 0 {
            hash = 1;
        }
        Self {
            kind,
            payload_size,
            runtime_type_id,
            gc_ref_mask,
            layout_id: hash,
        }
    }

    pub(super) fn class(
        class_name: &str,
        runtime_type_id: i64,
        fields: &[(String, Type)],
        enum_infos: &HashMap<String, EnumInfo>,
    ) -> Self {
        let gc_ref_mask = gc_ref_mask_for_layout(class_name, fields, enum_infos);
        Self::new(
            GcObjectKind::Class,
            (fields.len() as i64 + 1) * 8,
            runtime_type_id,
            gc_ref_mask,
        )
    }
}

/// Heap destination categories retained by the central store path.
///
/// The current no-op barrier does not branch on these values. Generational and
/// concurrent collectors will use them to distinguish owner/card semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub(super) enum GcStoreDestination {
    ObjectField = 1,
    EnumPayload = 4,
    InterfaceObject = 5,
    AsyncFrameSlot = 6,
    IndirectReference = 7,
    GlobalStatic = 8,
}

/// Low-level centralized heap-store emitter for codegen paths that construct a
/// frame before a [`FuncGen`] exists.
pub(super) fn emit_gc_heap_store_raw(
    builder: &mut FunctionBuilder<'_>,
    owner: Value,
    offset: i32,
    value: Value,
    _is_reference: bool,
    _destination: GcStoreDestination,
    flags: MemFlagsData,
) {
    // The reference branch is deliberately a compile-time no-op in Stage 2.
    // Stage 4 can attach a runtime/card barrier here; keeping the classification
    // in the signature makes coverage explicit today.
    builder.ins().store(flags, value, owner, offset);
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit the single compiler/runtime allocation abstraction.
    ///
    /// Stage 2 always calls the runtime slow path. Stage 3 can insert a TLAB
    /// fast path here and retain this call as refill/large-object fallback.
    pub(super) fn emit_gc_alloc(&mut self, layout: GcLayoutMetadata) -> Value {
        let layout_id = self
            .builder
            .ins()
            .iconst(types::I64, layout.layout_id as i64);
        let type_id = self
            .builder
            .ins()
            .iconst(types::I64, layout.runtime_type_id);
        let size = self.builder.ins().iconst(types::I64, layout.payload_size);
        let mask = self
            .builder
            .ins()
            .iconst(types::I64, layout.gc_ref_mask as i64);
        let alloc_id = self.func_id("willow_gc_alloc_layout");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self
            .builder
            .ins()
            .call(alloc_ref, &[layout_id, type_id, size, mask]);
        self.builder.inst_results(call)[0]
    }

    /// Store a typed value into GC-managed heap memory.
    ///
    /// All reference values pass the centralized barrier hook. Scalars use the
    /// same store helper so object-layout code cannot accidentally grow a new
    /// direct reference-store path.
    pub(super) fn emit_gc_heap_store(
        &mut self,
        owner: Value,
        offset: i32,
        value: Value,
        value_ty: &Type,
        destination: GcStoreDestination,
    ) {
        let is_reference = is_gc_managed(value_ty, self.enum_infos);
        self.emit_gc_heap_store_classified(owner, offset, value, is_reference, destination);
    }

    /// Variant for values whose source-level type has already been erased to a
    /// raw word (runtime payloads and dynamic interface boxes).
    pub(super) fn emit_gc_heap_store_classified(
        &mut self,
        owner: Value,
        offset: i32,
        value: Value,
        is_reference: bool,
        destination: GcStoreDestination,
    ) {
        emit_gc_heap_store_raw(
            self.builder,
            owner,
            offset,
            value,
            is_reference,
            destination,
            MemFlagsData::new(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_id_is_stable_and_shape_sensitive() {
        let a = GcLayoutMetadata::new(GcObjectKind::Enum, 16, 0, 0b10);
        let b = GcLayoutMetadata::new(GcObjectKind::Enum, 16, 0, 0b10);
        let different_mask = GcLayoutMetadata::new(GcObjectKind::Enum, 16, 0, 0);
        let different_kind = GcLayoutMetadata::new(GcObjectKind::InterfaceBox, 16, 0, 0b10);
        assert_eq!(a.layout_id, b.layout_id);
        assert_eq!(a.layout_id, 0x17b2_8090_98b9_7b2d);
        assert_ne!(a.layout_id, 0);
        assert_ne!(a.layout_id, different_mask.layout_id);
        assert_ne!(a.layout_id, different_kind.layout_id);
    }

    #[test]
    fn class_layout_carries_size_type_and_reference_mask() {
        let fields = vec![
            ("count".to_string(), Type::I64),
            ("name".to_string(), Type::String),
        ];
        let layout = GcLayoutMetadata::class("Node", 17, &fields, &HashMap::new());
        assert_eq!(layout.kind, GcObjectKind::Class);
        assert_eq!(layout.payload_size, 24);
        assert_eq!(layout.runtime_type_id, 17);
        assert_eq!(layout.gc_ref_mask, 0b100);
    }
}
