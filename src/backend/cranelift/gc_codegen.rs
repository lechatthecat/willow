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
pub(super) use willow_abi::{GcObjectKind, GcStoreDestination};

use super::*;

const GC_HEADER_MARKED_OFFSET: i32 = willow_abi::gc_header::MARKED_OFFSET as i32;
const GC_HEADER_ALLOCATED_OFFSET: i32 = willow_abi::gc_header::ALLOCATED_OFFSET as i32;
const GC_HEADER_GENERATION_OFFSET: i32 = willow_abi::gc_header::GENERATION_OFFSET as i32;
const GC_HEADER_AGE_OFFSET: i32 = willow_abi::gc_header::AGE_OFFSET as i32;
const GC_HEADER_OWNED_OFFSET: i32 = willow_abi::gc_header::OWNED_OFFSET as i32;
const GC_HEADER_DESCRIPTOR_OFFSET: i32 = willow_abi::gc_header::DESCRIPTOR_OFFSET as i32;
const GC_TLAB_STATE_SIZE: u64 = willow_abi::tlab::STATE_SIZE as u64;
const GC_TLAB_MAX_OBJECT_SIZE: i64 = willow_abi::tlab::MAX_OBJECT_SIZE as i64;

/// Compiler-owned layout metadata for one allocation site.
///
/// `layout_id` is a stable fingerprint of the current shape, not a registry
/// index. The runtime treats it as opaque today; a future layout registry may
/// replace the fingerprint without changing allocation call sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct GcLayoutMetadata {
    pub(super) kind: GcObjectKind,
    pub(super) payload_size: i64,
    pub(super) runtime_type_id: i64,
    pub(super) gc_ref_mask: u64,
    pub(super) layout_id: u64,
    pub(super) bitmap: Vec<u64>,
}

impl GcLayoutMetadata {
    pub(super) fn new(
        kind: GcObjectKind,
        payload_size: i64,
        runtime_type_id: i64,
        gc_ref_mask: u64,
    ) -> Self {
        debug_assert!(payload_size >= 0);
        let hash = willow_abi::gc_layout_id(kind, payload_size, runtime_type_id, gc_ref_mask);
        Self {
            kind,
            payload_size,
            runtime_type_id,
            gc_ref_mask,
            layout_id: hash,
            bitmap: Vec::new(),
        }
    }

    pub(super) fn class(
        class_name: &str,
        runtime_type_id: i64,
        fields: &[(String, Type)],
        enum_infos: &TypeMap<EnumInfo>,
        pointer_bytes: u32,
    ) -> Self {
        let gc_ref_mask = gc_ref_mask_for_layout(class_name, fields, enum_infos);
        let mut layout = Self::new(
            GcObjectKind::Class,
            (fields.len() as i64 + 1) * willow_abi::storage_word_bytes(pointer_bytes) as i64,
            runtime_type_id,
            gc_ref_mask,
        );
        if fields
            .iter()
            .enumerate()
            .any(|(i, (_, ty))| i + 1 >= 64 && is_gc_managed(ty, enum_infos))
        {
            layout.bitmap = vec![0; (fields.len() + 1).div_ceil(64)];
            for (i, (_, ty)) in fields.iter().enumerate() {
                if is_gc_managed(ty, enum_infos) {
                    layout.bitmap[(i + 1) / 64] |= 1 << ((i + 1) % 64);
                }
            }
        }
        layout
    }
}

/// Intern exact trace descriptors within the object module. A layout fingerprint
/// is insufficient as a cache key because hashes can collide. Equal bitmaps can safely
/// share data even when allocation size or runtime type differ. Cache their
/// content digest here; combine it with size/type once per allocation site.
fn bitmap_descriptor(
    module: &mut ObjectModule,
    descriptors: &mut HashMap<Vec<u64>, (DataId, u64)>,
    bitmap: &[u64],
) -> (DataId, u64) {
    if let Some(&id) = descriptors.get(bitmap) {
        return id;
    }
    let data_id = module
        .declare_anonymous_data(false, false)
        .expect("GC bitmap data");
    let mut data = DataDescription::new();
    let bytes: Vec<_> = std::iter::once(bitmap.len() as u64)
        .chain(bitmap.iter().copied())
        .flat_map(u64::to_ne_bytes)
        .collect();
    data.define(bytes.into_boxed_slice());
    data.set_align(8);
    module
        .define_data(data_id, &data)
        .expect("GC bitmap definition");
    let descriptor = (data_id, willow_abi::gc_bitmap_fingerprint(bitmap));
    descriptors.insert(bitmap.to_vec(), descriptor);
    descriptor
}

/// One fixed-size record per distinct allocation shape, shared across sites.
fn layout_descriptor(
    module: &mut ObjectModule,
    descriptors: &mut HashMap<willow_abi::GcLayoutDescriptor, DataId>,
    layout: willow_abi::GcLayoutDescriptor,
) -> DataId {
    *descriptors.entry(layout).or_insert_with(|| {
        let id = module
            .declare_anonymous_data(false, false)
            .expect("GC layout data");
        let mut data = DataDescription::new();
        let bytes: Vec<_> = [
            layout.type_id,
            layout.layout_id,
            layout.gc_ref_mask,
            layout.size,
        ]
        .into_iter()
        .flat_map(u64::to_ne_bytes)
        .collect();
        data.define(bytes.into_boxed_slice());
        data.set_align(8);
        module.define_data(id, &data).expect("GC layout definition");
        id
    })
}

/// Shared allocation path for wide class objects and async frames.
pub(super) fn emit_bitmap_alloc(
    module: &mut ObjectModule,
    descriptors: &mut HashMap<Vec<u64>, (DataId, u64)>,
    builder: &mut FunctionBuilder<'_>,
    alloc_id: FuncId,
    type_id: i64,
    payload_size: i64,
    bitmap: &[u64],
) -> Value {
    let (data_id, digest) = bitmap_descriptor(module, descriptors, bitmap);
    let global = module.declare_data_in_func(data_id, builder.func);
    let pointer = builder
        .ins()
        .symbol_value(reference_type(module.target_config()), global);
    let fingerprint = willow_abi::gc_bitmap_layout_id(payload_size, type_id, digest);
    let layout_id = builder.ins().iconst(types::I64, fingerprint as i64);
    let size = builder.ins().iconst(types::I64, payload_size);
    let alloc = module.declare_func_in_func(alloc_id, builder.func);
    let call = builder.ins().call(alloc, &[layout_id, size, pointer]);
    builder.inst_results(call)[0]
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
        let slot = if offset == 0 {
            owner
        } else {
            builder.ins().iadd_imm_s(owner, i64::from(offset))
        };
        let value_type = builder.func.dfg.value_type(value);
        let old = builder.ins().atomic_load(value_type, flags, slot);
        builder
            .ins()
            .call(barrier, &[owner, old, value, destination]);
        // Capture and publish the old reference before mutation, even when the
        // replacement is null. Atomic accesses also protect concurrent tracing.
        builder.ins().atomic_store(flags, value, slot);
    } else {
        builder.ins().store(flags, value, owner, offset);
    }
}

/// Publish one initialized header into the TLAB's persistent start map. The
/// atomic OR is after all header writes and before the payload escapes.
fn emit_tlab_start_publication(
    builder: &mut FunctionBuilder<'_>,
    tlab: Value,
    header: Value,
    limit: Value,
    pointer_bytes: u32,
) {
    let ptr_ty = builder.func.dfg.value_type(tlab);
    let bitmap_slot = builder.ins().iadd_imm_s(
        tlab,
        willow_abi::tlab::start_bits_offset(pointer_bytes) as i64,
    );
    let bitmap = builder
        .ins()
        .atomic_load(ptr_ty, MemFlagsData::trusted(), bitmap_slot);
    let base = builder
        .ins()
        .iadd_imm_s(limit, -(willow_abi::tlab::CHUNK_SIZE as i64));
    let offset = builder.ins().isub(header, base);
    let granule = builder.ins().ushr_imm_s(
        offset,
        willow_abi::tlab::MARK_GRANULE_BYTES.trailing_zeros() as i64,
    );
    let word_index = builder.ins().ushr_imm_s(granule, 6);
    let word_offset = builder.ins().ishl_imm_s(word_index, 3);
    let word = builder.ins().iadd(bitmap, word_offset);
    let one = builder.ins().iconst(types::I64, 1);
    // CLIF integer shifts mask the count to the value width (64 here).
    let mask = builder.ins().ishl(one, granule);
    builder.ins().atomic_rmw(
        types::I64,
        MemFlagsData::trusted(),
        AtomicRmwOp::Or,
        word,
        mask,
    );
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit the single compiler/runtime allocation abstraction.
    ///
    /// Small objects use the generated executable's TLS cursor/limit directly.
    /// Only empty/exhausted TLABs, large objects, and stress-mode allocations
    /// call the runtime slow path.
    pub(super) fn emit_gc_alloc(&mut self, layout: GcLayoutMetadata) -> Value {
        if !layout.bitmap.is_empty() {
            let alloc = self.func_id("willow_gc_alloc_bitmap");
            return emit_bitmap_alloc(
                self.module,
                self.gc_bitmap_descriptors,
                self.builder,
                alloc,
                layout.runtime_type_id,
                layout.payload_size,
                &layout.bitmap,
            );
        }
        debug_assert_eq!(GC_TLAB_STATE_SIZE, 40, "compiler/runtime TLAB ABI changed");
        let pointer_bytes = reference_type(self.module.target_config()).bytes();
        let header_size = willow_abi::gc_header::size(pointer_bytes) as i64;
        let alignment = willow_abi::storage_word_bytes(pointer_bytes) as i64;
        let total_size = (header_size + layout.payload_size + alignment - 1) & !(alignment - 1);
        if total_size > GC_TLAB_MAX_OBJECT_SIZE {
            return self.emit_gc_alloc_slow(layout);
        }

        let ptr_ty = reference_type(self.module.target_config());
        let tls_global = self
            .module
            .declare_data_in_func(self.gc_tlab_state, self.builder.func);
        let tlab = self.builder.ins().tls_value(ptr_ty, tls_global);
        let limit_addr = self
            .builder
            .ins()
            .iadd_imm_s(tlab, willow_abi::tlab::limit_offset(pointer_bytes) as i64);
        let cursor = self
            .builder
            .ins()
            .atomic_load(ptr_ty, MemFlagsData::trusted(), tlab);
        let limit = self
            .builder
            .ins()
            .atomic_load(ptr_ty, MemFlagsData::trusted(), limit_addr);
        let new_cursor = self.builder.ins().iadd_imm_s(cursor, total_size);
        let nonempty = self.builder.ins().icmp_imm_s(IntCC::NotEqual, cursor, 0);
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
        self.builder.ins().store(
            MemFlagsData::trusted(),
            zero8,
            cursor,
            GC_HEADER_OWNED_OFFSET,
        );
        let descriptor = layout_descriptor(
            self.module,
            self.gc_layout_descriptors,
            willow_abi::GcLayoutDescriptor {
                type_id: layout.runtime_type_id as u32 as u64,
                layout_id: layout.layout_id,
                gc_ref_mask: layout.gc_ref_mask,
                size: total_size as u64,
            },
        );
        let global = self
            .module
            .declare_data_in_func(descriptor, self.builder.func);
        let address = self.builder.ins().symbol_value(ptr_ty, global);
        self.builder.ins().store(
            MemFlagsData::trusted(),
            address,
            cursor,
            GC_HEADER_DESCRIPTOR_OFFSET,
        );
        let total_size_value = self.builder.ins().iconst(types::I64, total_size);

        emit_tlab_start_publication(self.builder, tlab, cursor, limit, pointer_bytes);
        let one64 = self.builder.ins().iconst(types::I64, 1);
        let fast_allocs_addr = self.builder.ins().iadd_imm_s(
            tlab,
            willow_abi::tlab::fast_allocations_offset(pointer_bytes) as i64,
        );
        self.builder.ins().atomic_rmw(
            types::I64,
            MemFlagsData::trusted(),
            AtomicRmwOp::Add,
            fast_allocs_addr,
            one64,
        );
        let fast_bytes_addr = self.builder.ins().iadd_imm_s(
            tlab,
            willow_abi::tlab::fast_bytes_offset(pointer_bytes) as i64,
        );
        self.builder.ins().atomic_rmw(
            types::I64,
            MemFlagsData::trusted(),
            AtomicRmwOp::Add,
            fast_bytes_addr,
            total_size_value,
        );
        let payload = self.builder.ins().iadd_imm_s(cursor, header_size);
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
        let ptr_ty = reference_type(self.module.target_config());
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
    fn satb_codegen_loads_old_before_each_reference_store_in_linear_code() {
        use cranelift_codegen::ir::Opcode;
        for stores in [1, 16, 256] {
            let mut codegen = Codegen::new(&CompilerOptions::debug()).unwrap();
            codegen.declare_runtime().unwrap();
            let mut ctx = codegen.module.make_context();
            let mut signature = codegen.module.make_signature();
            signature
                .params
                .extend([AbiParam::new(types::I64), AbiParam::new(types::I64)]);
            ctx.func.signature = signature;
            let mut fn_ctx = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            builder.seal_block(entry);
            let owner = builder.block_params(entry)[0];
            let value = builder.block_params(entry)[1];
            let barrier_id = codegen.func_id("willow_gc_write_barrier");
            let barrier = codegen
                .module
                .declare_func_in_func(barrier_id, builder.func);
            for i in 0..stores {
                emit_gc_heap_store_raw(
                    &mut builder,
                    Some(barrier),
                    owner,
                    i * 8,
                    value,
                    GcStoreDestination::ObjectField,
                    MemFlagsData::new(),
                );
            }
            builder.ins().return_(&[]);
            builder.finalize(codegen.module.target_config());
            let effects: Vec<_> = ctx
                .func
                .layout
                .block_insts(entry)
                .map(|i| ctx.func.dfg.insts[i].opcode())
                .filter(|op| matches!(op, Opcode::AtomicLoad | Opcode::Call | Opcode::AtomicStore))
                .collect();
            assert_eq!(
                effects,
                [Opcode::AtomicLoad, Opcode::Call, Opcode::AtomicStore].repeat(stores as usize)
            );
            cranelift_codegen::verify_function(&ctx.func, codegen.module.isa()).unwrap();
            println!(
                "reference_stores={stores} old_loads={stores} barrier_calls={stores} atomic_stores={stores}"
            );
        }
    }

    #[test]
    fn compact_descriptors_scale_with_shapes_not_sites() {
        for sites in [1, 16, 256, 4096] {
            let mut codegen = Codegen::new(&CompilerOptions::debug()).unwrap();
            let before = codegen.module.declarations().get_data_objects().count();
            let key = willow_abi::GcLayoutDescriptor {
                type_id: 2,
                layout_id: 7,
                gc_ref_mask: 1,
                size: 24,
            };
            let first =
                layout_descriptor(&mut codegen.module, &mut codegen.gc_layout_descriptors, key);
            for _ in 1..sites {
                assert_eq!(
                    layout_descriptor(&mut codegen.module, &mut codegen.gc_layout_descriptors, key),
                    first
                );
            }
            assert_eq!(
                codegen.module.declarations().get_data_objects().count() - before,
                1
            );
            for other in [
                willow_abi::GcLayoutDescriptor { type_id: 3, ..key },
                willow_abi::GcLayoutDescriptor {
                    gc_ref_mask: 2,
                    ..key
                },
                willow_abi::GcLayoutDescriptor { size: 32, ..key },
            ] {
                assert_ne!(
                    layout_descriptor(
                        &mut codegen.module,
                        &mut codegen.gc_layout_descriptors,
                        other
                    ),
                    first
                );
            }
            assert_eq!(codegen.gc_layout_descriptors.len(), 4);
            println!(
                "sites={sites} repeated_shape_records=1 distinct_shape_records=4 descriptor_bytes=128"
            );
        }
    }

    #[test]
    fn bitmap_descriptors_scale_with_unique_contents_not_sites() {
        for words in [2, 8, 64] {
            for sites in [1, 16, 256] {
                let mut codegen = Codegen::new(&CompilerOptions::debug()).unwrap();
                let before = codegen.module.declarations().get_data_objects().count();
                let mut bitmap = vec![0; words];
                bitmap[words - 1] = 1;
                let first = bitmap_descriptor(
                    &mut codegen.module,
                    &mut codegen.gc_bitmap_descriptors,
                    &bitmap,
                );
                for _ in 1..sites {
                    assert_eq!(
                        bitmap_descriptor(
                            &mut codegen.module,
                            &mut codegen.gc_bitmap_descriptors,
                            &bitmap,
                        ),
                        first,
                    );
                }
                assert_eq!(
                    codegen.module.declarations().get_data_objects().count() - before,
                    1
                );
                // The low mask and bitmap length are identical; only a high
                // reference bit differs. A layout-fingerprint key would collide.
                bitmap[words - 1] = 2;
                let other = bitmap_descriptor(
                    &mut codegen.module,
                    &mut codegen.gc_bitmap_descriptors,
                    &bitmap,
                );
                assert_ne!(first.0, other.0);
                assert_ne!(first.1, other.1);
                assert_eq!(
                    codegen.module.declarations().get_data_objects().count() - before,
                    2
                );
                assert_eq!(codegen.gc_bitmap_descriptors.len(), 2);
            }
        }
    }

    #[test]
    fn tlab_start_publication_codegen_has_constant_work_per_site() {
        use cranelift_codegen::ir::Opcode;
        for sites in [1, 16, 256] {
            let codegen = Codegen::new(&CompilerOptions::debug()).unwrap();
            let mut ctx = codegen.module.make_context();
            ctx.func
                .signature
                .params
                .extend([AbiParam::new(types::I64); 3]);
            let mut fn_ctx = FunctionBuilderContext::new();
            let mut builder = FunctionBuilder::new(&mut ctx.func, &mut fn_ctx);
            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);
            builder.seal_block(entry);
            let params = builder.block_params(entry).to_vec();
            for _ in 0..sites {
                emit_tlab_start_publication(&mut builder, params[0], params[1], params[2], 8);
            }
            builder.ins().return_(&[]);
            builder.finalize(codegen.module.target_config());
            let instructions: Vec<_> = ctx
                .func
                .layout
                .block_insts(entry)
                .map(|inst| ctx.func.dfg.insts[inst].opcode())
                .collect();
            assert_eq!(
                instructions
                    .iter()
                    .filter(|&&op| op == Opcode::AtomicRmw)
                    .count(),
                sites
            );
            assert_eq!(
                instructions
                    .iter()
                    .filter(|&&op| op == Opcode::AtomicLoad)
                    .count(),
                sites
            );
            assert_eq!(instructions.len(), 16 * sites + 1);
            cranelift_codegen::verify_function(&ctx.func, codegen.module.isa()).unwrap();
            println!(
                "tlab_sites={sites} publication_instructions={} atomic_or={sites}",
                instructions.len() - 1
            );
        }
    }

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
        assert_eq!(willow_abi::gc_header::size(8), 16);
        assert_eq!(GC_HEADER_ALLOCATED_OFFSET, 1);
        assert_eq!(GC_HEADER_GENERATION_OFFSET, 2);
        assert_eq!(GC_HEADER_AGE_OFFSET, 3);
        assert_eq!(GC_HEADER_OWNED_OFFSET, 4);
        assert_eq!(GC_HEADER_DESCRIPTOR_OFFSET, 8);
        assert_eq!(GC_TLAB_STATE_SIZE, 40);
        assert_eq!(GC_TLAB_MAX_OBJECT_SIZE, 4096);
        assert_eq!(willow_abi::tlab::start_bits_offset(8), 32);
        assert_eq!(willow_abi::tlab::CHUNK_SIZE, 32768);
        assert_eq!(willow_abi::tlab::MARK_GRANULE_BYTES, 8);
    }

    #[test]
    fn class_layout_carries_size_type_and_reference_mask() {
        let fields = vec![
            ("count".to_string(), Type::I64),
            ("name".to_string(), Type::String),
        ];
        let layout = GcLayoutMetadata::class("Node", 17, &fields, &TypeMap::new(), 8);
        assert_eq!(layout.kind, GcObjectKind::Class);
        assert_eq!(layout.payload_size, 24);
        assert_eq!(layout.runtime_type_id, 17);
        assert_eq!(layout.gc_ref_mask, 0b100);
    }
}

#[cfg(test)]
mod destination_abi_tests {
    use super::GcStoreDestination;

    /// Perspective 5: `destination_kind` reaches `willow_gc_write_barrier` as a
    /// bare integer, so the compiler's enum and the runtime's must agree
    /// discriminant for discriminant. The lock-cell destinations were added on
    /// both sides in willow-38w.1.4/.1.5 and the blocking-cell ones in
    /// willow-9tls.5; the rest are pinned so a renumbering shows up here
    /// rather than as a mis-classified barrier at runtime.
    #[test]
    fn store_destinations_are_the_shared_runtime_type() {
        let values: &[willow_abi::GcStoreDestination] = &[
            GcStoreDestination::ObjectField,
            GcStoreDestination::EnumPayload,
            GcStoreDestination::InterfaceObject,
            GcStoreDestination::AsyncFrameSlot,
            GcStoreDestination::IndirectReference,
            GcStoreDestination::GlobalStatic,
            GcStoreDestination::AsyncMutexCell,
            GcStoreDestination::AsyncRwLockCell,
            GcStoreDestination::BlockingCell,
            GcStoreDestination::BlockingRwCell,
        ];
        assert_eq!(values.len(), 10);
    }

    /// Perspective 6: the mutex cell is NOT a global static. The runtime's
    /// barrier short-circuits on `GlobalStatic`, so tagging a commit that way
    /// would skip the generational barrier for a heap value published out of a
    /// critical section.
    #[test]
    fn async_lock_cells_are_not_global_statics() {
        for destination in [
            GcStoreDestination::AsyncMutexCell,
            GcStoreDestination::AsyncRwLockCell,
            GcStoreDestination::BlockingCell,
            GcStoreDestination::BlockingRwCell,
        ] {
            assert_ne!(destination as i64, GcStoreDestination::GlobalStatic as i64);
        }
    }
}
