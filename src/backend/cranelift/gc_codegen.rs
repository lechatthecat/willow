//! Central compiler-side GC allocation and reference-store lowering.
//!
//! Stage 4 combines the inlined TLS bump-allocation fast path with a copying
//! young generation and an enabled old-to-young write barrier:
//!
//! - every generated GC allocation goes through [`FuncGen::emit_gc_alloc`];
//! - every generated GC-reference heap store goes through
//!   [`FuncGen::emit_gc_heap_store`].
//!
//! Later stages can replace chunk/refill/card policy here without
//! redistributing collector policy through expression-specific emitters.

use cranelift_codegen::ir::{AtomicRmwOp, FuncRef, MemFlagsData, Value};
use cranelift_frontend::FunctionBuilder;
use cranelift_module::Module;

use super::*;

const GC_HEADER_SIZE: i64 = 40;
const GC_HEADER_MARKED_OFFSET: i32 = 0;
const GC_HEADER_ALLOCATED_OFFSET: i32 = 1;
const GC_HEADER_GENERATION_OFFSET: i32 = 2;
const GC_HEADER_AGE_OFFSET: i32 = 3;
const GC_HEADER_TYPE_ID_OFFSET: i32 = 4;
const GC_HEADER_LAYOUT_ID_OFFSET: i32 = 8;
const GC_HEADER_REF_MASK_OFFSET: i32 = 16;
const GC_HEADER_SIZE_OFFSET: i32 = 24;
const GC_HEADER_NEXT_OFFSET: i32 = 32;
const GC_TLAB_LIMIT_OFFSET: i64 = 8;
const GC_TLAB_FAST_ALLOCS_OFFSET: i64 = 16;
const GC_TLAB_FAST_BYTES_OFFSET: i64 = 24;
const GC_TLAB_STATE_SIZE: u64 = 32;
const GC_TLAB_MAX_OBJECT_SIZE: i64 = 4 * 1024;

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
/// The generational barrier uses these values to distinguish heap owners from
/// permanent global/static roots. A future concurrent collector can also use
/// them to refine destination-specific barrier semantics.
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
    barrier: Option<FuncRef>,
    owner: Value,
    offset: i32,
    value: Value,
    destination: GcStoreDestination,
    flags: MemFlagsData,
) {
    if let Some(barrier) = barrier {
        let destination = builder.ins().iconst(types::I64, destination as i64);
        builder.ins().call(barrier, &[owner, value, destination]);
    }
    builder.ins().store(flags, value, owner, offset);
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit the single compiler/runtime allocation abstraction.
    ///
    /// Small objects use the generated executable's TLS cursor/limit directly.
    /// Only empty/exhausted TLABs, large objects, and stress-mode allocations
    /// call the runtime slow path.
    pub(super) fn emit_gc_alloc(&mut self, layout: GcLayoutMetadata) -> Value {
        debug_assert_eq!(GC_TLAB_STATE_SIZE, 32, "compiler/runtime TLAB ABI changed");
        let total_size = (GC_HEADER_SIZE + layout.payload_size + 7) & !7;
        if total_size > GC_TLAB_MAX_OBJECT_SIZE {
            return self.emit_gc_alloc_slow(layout);
        }

        let ptr_ty = self.module.target_config().pointer_type();
        let tls_global = self
            .module
            .declare_data_in_func(self.gc_tlab_state, self.builder.func);
        let tlab = self.builder.ins().tls_value(ptr_ty, tls_global);
        let limit_addr = self.builder.ins().iadd_imm(tlab, GC_TLAB_LIMIT_OFFSET);
        let cursor = self
            .builder
            .ins()
            .atomic_load(types::I64, MemFlagsData::trusted(), tlab);
        let limit = self
            .builder
            .ins()
            .atomic_load(types::I64, MemFlagsData::trusted(), limit_addr);
        let new_cursor = self.builder.ins().iadd_imm(cursor, total_size);
        let nonempty = self.builder.ins().icmp_imm(IntCC::NotEqual, cursor, 0);
        let no_overflow =
            self.builder
                .ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, new_cursor, cursor);
        let fits = self
            .builder
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, new_cursor, limit);
        let usable = self.builder.ins().band(nonempty, no_overflow);
        let usable = self.builder.ins().band(usable, fits);

        let fast_block = self.builder.create_block();
        let slow_block = self.builder.create_block();
        let done_block = self.builder.create_block();
        self.builder.append_block_param(done_block, ptr_ty);
        self.builder
            .ins()
            .brif(usable, fast_block, &[], slow_block, &[]);

        self.builder.switch_to_block(fast_block);
        self.builder.seal_block(fast_block);
        self.builder
            .ins()
            .atomic_store(MemFlagsData::trusted(), new_cursor, tlab);

        // Fresh TLAB chunks are zero-filled and never reuse swept holes. Write
        // every nonzero/semantic header field before the payload is exposed.
        let zero8 = self.builder.ins().iconst(types::I8, 0);
        let one8 = self.builder.ins().iconst(types::I8, 1);
        self.builder.ins().store(
            MemFlagsData::trusted(),
            zero8,
            cursor,
            GC_HEADER_MARKED_OFFSET,
        );
        self.builder.ins().store(
            MemFlagsData::trusted(),
            one8,
            cursor,
            GC_HEADER_ALLOCATED_OFFSET,
        );
        self.builder.ins().store(
            MemFlagsData::trusted(),
            zero8,
            cursor,
            GC_HEADER_GENERATION_OFFSET,
        );
        self.builder
            .ins()
            .store(MemFlagsData::trusted(), zero8, cursor, GC_HEADER_AGE_OFFSET);
        let layout_id = self
            .builder
            .ins()
            .iconst(types::I64, layout.layout_id as i64);
        let type_id = self
            .builder
            .ins()
            .iconst(types::I64, layout.runtime_type_id);
        let type_id32 = self.builder.ins().ireduce(types::I32, type_id);
        self.builder.ins().store(
            MemFlagsData::trusted(),
            type_id32,
            cursor,
            GC_HEADER_TYPE_ID_OFFSET,
        );
        self.builder.ins().store(
            MemFlagsData::trusted(),
            layout_id,
            cursor,
            GC_HEADER_LAYOUT_ID_OFFSET,
        );
        let mask = self
            .builder
            .ins()
            .iconst(types::I64, layout.gc_ref_mask as i64);
        self.builder.ins().store(
            MemFlagsData::trusted(),
            mask,
            cursor,
            GC_HEADER_REF_MASK_OFFSET,
        );
        let total_size_value = self.builder.ins().iconst(types::I64, total_size);
        self.builder.ins().store(
            MemFlagsData::trusted(),
            total_size_value,
            cursor,
            GC_HEADER_SIZE_OFFSET,
        );
        let zero64 = self.builder.ins().iconst(types::I64, 0);
        self.builder.ins().store(
            MemFlagsData::trusted(),
            zero64,
            cursor,
            GC_HEADER_NEXT_OFFSET,
        );

        let one64 = self.builder.ins().iconst(types::I64, 1);
        let fast_allocs_addr = self
            .builder
            .ins()
            .iadd_imm(tlab, GC_TLAB_FAST_ALLOCS_OFFSET);
        self.builder.ins().atomic_rmw(
            types::I64,
            MemFlagsData::trusted(),
            AtomicRmwOp::Add,
            fast_allocs_addr,
            one64,
        );
        let fast_bytes_addr = self.builder.ins().iadd_imm(tlab, GC_TLAB_FAST_BYTES_OFFSET);
        self.builder.ins().atomic_rmw(
            types::I64,
            MemFlagsData::trusted(),
            AtomicRmwOp::Add,
            fast_bytes_addr,
            total_size_value,
        );
        let payload = self.builder.ins().iadd_imm(cursor, GC_HEADER_SIZE);
        self.builder.ins().jump(done_block, &[payload.into()]);

        self.builder.switch_to_block(slow_block);
        self.builder.seal_block(slow_block);
        let slow_payload = self.emit_gc_alloc_slow(layout);
        self.builder.ins().jump(done_block, &[slow_payload.into()]);

        self.builder.switch_to_block(done_block);
        self.builder.seal_block(done_block);
        self.builder.block_params(done_block)[0]
    }

    fn emit_gc_alloc_slow(&mut self, layout: GcLayoutMetadata) -> Value {
        let ptr_ty = self.module.target_config().pointer_type();
        let tls_global = self
            .module
            .declare_data_in_func(self.gc_tlab_state, self.builder.func);
        let tlab = self.builder.ins().tls_value(ptr_ty, tls_global);
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
        let alloc_id = self.func_id("willow_gc_alloc_slow");
        let alloc_ref = self
            .module
            .declare_func_in_func(alloc_id, self.builder.func);
        let call = self
            .builder
            .ins()
            .call(alloc_ref, &[tlab, layout_id, type_id, size, mask]);
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
        let barrier_id = self.func_id("willow_gc_write_barrier");
        let barrier = self
            .module
            .declare_func_in_func(barrier_id, self.builder.func);
        emit_gc_heap_store_raw(
            self.builder,
            is_reference.then_some(barrier),
            owner,
            offset,
            value,
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
    fn generated_header_and_tlab_layout_contract_is_stable() {
        assert_eq!(GC_HEADER_SIZE, 40);
        assert_eq!(GC_HEADER_ALLOCATED_OFFSET, 1);
        assert_eq!(GC_HEADER_GENERATION_OFFSET, 2);
        assert_eq!(GC_HEADER_AGE_OFFSET, 3);
        assert_eq!(GC_HEADER_TYPE_ID_OFFSET, 4);
        assert_eq!(GC_HEADER_LAYOUT_ID_OFFSET, 8);
        assert_eq!(GC_HEADER_REF_MASK_OFFSET, 16);
        assert_eq!(GC_HEADER_SIZE_OFFSET, 24);
        assert_eq!(GC_HEADER_NEXT_OFFSET, 32);
        assert_eq!(GC_TLAB_STATE_SIZE, 32);
        assert_eq!(GC_TLAB_MAX_OBJECT_SIZE, 4096);
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
