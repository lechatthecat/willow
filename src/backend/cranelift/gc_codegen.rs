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

use cranelift_codegen::ir::{FuncRef, MemFlagsData, Value};
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

    /// Mixed-width fields following non-reference header words. Use the same
    /// bitmap allocation path as wide classes when references exceed bit 63.
    pub(super) fn aggregate(
        kind: GcObjectKind,
        header_words: u32,
        slots: &[willow_abi::SlotKind],
        pointer_bytes: u32,
    ) -> Self {
        let words = header_words + willow_abi::WordLayout::new(slots).word_count();
        let mut mask = 0u64;
        let mut bitmap = Vec::new();
        let mut word = header_words as usize;
        for slot in slots {
            if slot.traces_first_word() {
                if word < 64 {
                    mask |= 1 << word;
                } else {
                    if bitmap.is_empty() {
                        bitmap = vec![0; (words as usize).div_ceil(64)];
                        bitmap[0] = mask;
                    }
                    bitmap[word / 64] |= 1 << (word % 64);
                }
            }
            word += slot.word_count() as usize;
        }
        let mut layout = Self::new(
            kind,
            i64::from(words) * i64::from(willow_abi::storage_word_bytes(pointer_bytes)),
            0,
            mask,
        );
        layout.bitmap = bitmap;
        layout
    }

    pub(super) fn class(
        runtime_type_id: i64,
        object: &crate::compiler_db::layout::ObjectLayout,
        enum_infos: &TypeMap<EnumInfo>,
    ) -> Self {
        let fallback;
        let trace = match object.gc_trace() {
            Some(trace) => trace,
            None => {
                // Standalone layout tests and unregistered builtin layouts.
                fallback =
                    crate::compiler_db::layout::GcTraceLayout::from_fields(object.fields(), |ty| {
                        Ok(is_gc_managed(ty, enum_infos))
                    })
                    .expect("GC classification");
                &fallback
            }
        };
        let mut layout = Self::new(
            GcObjectKind::Class,
            object.size_bytes(),
            runtime_type_id,
            trace.mask,
        );
        layout.bitmap.clone_from(&trace.bitmap);
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
        if builder.func.dfg.value_type(value) == types::I128 {
            // A traced inline pair contains an object followed by an untraced
            // vtable. The barrier ABI and atomic publication concern word zero.
            let (object, vtable) = builder.ins().isplit(value);
            emit_gc_heap_store_raw(
                builder,
                Some(barrier),
                owner,
                offset,
                object,
                destination,
                flags,
            );
            builder.ins().store(flags, vtable, owner, offset + 8);
            return;
        }
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

/// Publish one initialized header into the TLAB's persistent start map, after
/// all header writes and before the payload escapes. Only the owner writes an
/// active chunk's start words, and other threads read them only after
/// synchronizing with the owner, so a plain read-modify-write suffices: no
/// fence and no locked RMW (willow-8hq4.16).
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
        .load(ptr_ty, MemFlagsData::trusted(), bitmap_slot, 0);
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
    let bits = builder
        .ins()
        .load(types::I64, MemFlagsData::trusted(), word, 0);
    let bits = builder.ins().bor(bits, mask);
    builder.ins().store(MemFlagsData::trusted(), bits, word, 0);
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
        debug_assert_eq!(GC_TLAB_STATE_SIZE, 24, "compiler/runtime TLAB ABI changed");
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
        // Owner-only state: plain accesses, see willow_abi::tlab.
        let cursor = self
            .builder
            .ins()
            .load(ptr_ty, MemFlagsData::trusted(), tlab, 0);
        let limit = self
            .builder
            .ins()
            .load(ptr_ty, MemFlagsData::trusted(), limit_addr, 0);
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
            .store(MemFlagsData::trusted(), new_cursor, tlab, 0);

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
        emit_tlab_start_publication(self.builder, tlab, cursor, limit, pointer_bytes);
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
        if self.is_inline_pair(value_ty)
            && destination == GcStoreDestination::AsyncFrameSlot
            && self.coop_result_offset == Some(offset)
        {
            // The runtime task-id word remains at data slot 1. Pair results
            // reserve data slot 2 for their payload, leaving this shared ABI intact.
            let (first, payload) = self.builder.ins().isplit(value);
            self.emit_gc_heap_store_classified(
                owner,
                offset,
                first,
                self.is_interface_pair(value_ty),
                destination,
            );
            let payload_offset = super::async_frame_slot_offset(
                2,
                reference_type(self.module.target_config()).bytes(),
            );
            self.builder
                .ins()
                .store(MemFlagsData::new(), payload, owner, payload_offset);
            return;
        }
        let is_reference = is_gc_managed(value_ty, self.enum_infos);
        let value_has_header = self.is_class_pointer(value_ty)
            || option_repr::option_inner(value_ty).is_some_and(|inner| {
                self.option_repr(value_ty) == Some(option_repr::OptionRepr::NullableGcPointer)
                    && self.is_class_pointer(inner)
            });
        self.emit_gc_heap_store_inner(
            owner,
            offset,
            value,
            is_reference,
            value_has_header,
            destination,
        );
    }

    /// Class instances are always GC heap payloads with a header; a class
    /// value word is never a tag, box-free pair, or foreign pointer.
    fn is_class_pointer(&self, ty: &Type) -> bool {
        matches!(ty, Type::Named(n) | Type::Generic(n, _) if self.classes.is_class(n))
    }

    /// Variant for values whose source-level type has already been erased to a
    /// raw word or a traced inline interface pair (runtime payloads).
    pub(super) fn emit_gc_heap_store_classified(
        &mut self,
        owner: Value,
        offset: i32,
        value: Value,
        is_reference: bool,
        destination: GcStoreDestination,
    ) {
        self.emit_gc_heap_store_inner(owner, offset, value, is_reference, false, destination);
    }

    /// `value_has_header`: a non-null `value` is known to be a GC payload whose
    /// header generation byte may be read (class instances).
    fn emit_gc_heap_store_inner(
        &mut self,
        owner: Value,
        offset: i32,
        value: Value,
        is_reference: bool,
        value_has_header: bool,
        destination: GcStoreDestination,
    ) {
        if is_reference && owner_is_object_payload(destination) {
            let flags = MemFlagsData::new();
            // Only word zero of an inline interface pair is traced.
            let (word, vtable) = if self.builder.func.dfg.value_type(value) == types::I128 {
                let (object, vtable) = self.builder.ins().isplit(value);
                (object, Some(vtable))
            } else {
                (value, None)
            };
            let slot = if offset == 0 {
                owner
            } else {
                self.builder.ins().iadd_imm_s(owner, i64::from(offset))
            };
            let refinement = if value_has_header && vtable.is_none() {
                BarrierRefinement::OldValueSkips
            } else {
                BarrierRefinement::None
            };
            self.emit_filtered_reference_store(owner, slot, word, destination, refinement, flags);
            if let Some(vtable) = vtable {
                self.builder.ins().store(flags, vtable, owner, offset + 8);
            }
            return;
        }
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

    /// Store a reference `word` into `slot` inside the GC object whose payload
    /// starts at `owner` (willow-8hq4.15, 8hq4.22). The fused
    /// `willow_gc_write_barrier` call is skipped when it is a no-op: no SATB
    /// epoch is active and the word is null, the owner is not old, or the
    /// owner is already remembered (a header flag the runtime keeps equal to
    /// remembered-set membership). Otherwise a cold block captures the old
    /// value, calls the barrier, and publishes atomically; see
    /// [`BarrierRefinement`] for the optional extra conditions.
    /// Nothing between the phase load and the store reaches a safepoint,
    /// matching the runtime barrier/store pair. Leaves the builder in the
    /// continuation block.
    pub(super) fn emit_filtered_reference_store(
        &mut self,
        owner: Value,
        slot: Value,
        word: Value,
        destination: GcStoreDestination,
        refinement: BarrierRefinement,
        flags: MemFlagsData,
    ) {
        use willow_abi::gc_header;
        let ptr_ty = reference_type(self.module.target_config());
        let plain = self.builder.create_block();
        let barrier = self.builder.create_block();
        self.builder.set_cold_block(barrier);
        let done = self.builder.create_block();

        let phase_data = self
            .module
            .declare_data(
                willow_abi::GC_MARK_PHASE_SYMBOL,
                cranelift_module::Linkage::Import,
                false,
                false,
            )
            .expect("GC mark phase symbol");
        let phase_global = self
            .module
            .declare_data_in_func(phase_data, self.builder.func);
        let phase_addr = self.builder.ins().symbol_value(ptr_ty, phase_global);
        let phase = self
            .builder
            .ins()
            .atomic_load(types::I8, MemFlagsData::trusted(), phase_addr);
        let marking = self.builder.ins().icmp_imm_s(IntCC::NotEqual, phase, 0);
        let header = -(gc_header::size(ptr_ty.bytes()) as i32);
        let generation = self.builder.ins().load(
            types::I8,
            flags,
            owner,
            header + gc_header::GENERATION_OFFSET as i32,
        );
        let remembered = self.builder.ins().load(
            types::I8,
            flags,
            owner,
            header + gc_header::REMEMBERED_OFFSET as i32,
        );
        let old = self.builder.ins().icmp_imm_s(
            IntCC::Equal,
            generation,
            i64::from(gc_header::GENERATION_OLD),
        );
        let unremembered = self.builder.ins().icmp_imm_s(IntCC::Equal, remembered, 0);
        let nonnull = self.builder.ins().icmp_imm_s(IntCC::NotEqual, word, 0);
        let new_edge = self.builder.ins().band(old, unremembered);
        let new_edge = self.builder.ins().band(new_edge, nonnull);
        let needs_barrier = self.builder.ins().bor(marking, new_edge);
        let needs_barrier = match refinement {
            BarrierRefinement::BufferIsRef(is_ref) => {
                self.builder.ins().band(is_ref, needs_barrier)
            }
            _ => needs_barrier,
        };
        if refinement == BarrierRefinement::OldValueSkips {
            // Reached only when marking or when `word` is non-null.
            let check_marking = self.builder.create_block();
            let check_value = self.builder.create_block();
            self.builder
                .ins()
                .brif(needs_barrier, check_marking, &[], plain, &[]);
            self.builder.switch_to_block(check_marking);
            self.builder.seal_block(check_marking);
            self.builder
                .ins()
                .brif(marking, barrier, &[], check_value, &[]);
            self.builder.switch_to_block(check_value);
            self.builder.seal_block(check_value);
            let value_generation = self.builder.ins().load(
                types::I8,
                MemFlagsData::trusted(),
                word,
                header + gc_header::GENERATION_OFFSET as i32,
            );
            let value_old = self.builder.ins().icmp_imm_s(
                IntCC::Equal,
                value_generation,
                i64::from(gc_header::GENERATION_OLD),
            );
            self.builder.ins().brif(value_old, plain, &[], barrier, &[]);
        } else {
            self.builder
                .ins()
                .brif(needs_barrier, barrier, &[], plain, &[]);
        }

        self.builder.switch_to_block(plain);
        self.builder.seal_block(plain);
        self.builder.ins().store(flags, word, slot, 0);
        self.builder.ins().jump(done, &[]);

        self.builder.switch_to_block(barrier);
        self.builder.seal_block(barrier);
        let word_ty = self.builder.func.dfg.value_type(word);
        let previous = self.builder.ins().atomic_load(word_ty, flags, slot);
        let destination = self.builder.ins().iconst(types::I64, destination as i64);
        let barrier_id = self.func_id("willow_gc_write_barrier");
        let barrier_ref = self
            .module
            .declare_func_in_func(barrier_id, self.builder.func);
        self.builder
            .ins()
            .call(barrier_ref, &[owner, previous, word, destination]);
        self.builder.ins().atomic_store(flags, word, slot);
        self.builder.ins().jump(done, &[]);

        self.builder.switch_to_block(done);
        self.builder.seal_block(done);
    }
}

/// Extra conditions of [`FuncGen::emit_filtered_reference_store`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BarrierRefinement {
    None,
    /// Dynamically typed buffers: the barrier applies only when this flag
    /// (the buffer's `H_IS_REF` word, compared non-zero) is set.
    BufferIsRef(Value),
    /// Non-null words are GC payloads (class instances): a would-be new edge
    /// outside marking reads the value's generation byte and skips the call
    /// for an old value, which records no edge either.
    OldValueSkips,
}

/// Destinations whose owner is always the payload start of a GC heap object,
/// so its header bytes can be read inline. Globals have no header and
/// `IndirectReference` owners are interior slots; both, like runtime-owned
/// cells and async frames, keep the unconditional barrier call.
fn owner_is_object_payload(destination: GcStoreDestination) -> bool {
    matches!(
        destination,
        GcStoreDestination::ObjectField
            | GcStoreDestination::EnumPayload
            | GcStoreDestination::InterfaceObject
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn satb_codegen_loads_old_before_each_reference_store_in_linear_code() {
        use cranelift_codegen::ir::Opcode;
        for stores in [1, 16, 256] {
            let mut codegen_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
            let mut codegen = codegen_build.declaration_context();
            codegen.declare_runtime().unwrap();
            let mut ctx = codegen.output.module.make_context();
            let mut signature = codegen.output.module.make_signature();
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
                .output
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
            builder.finalize(codegen.output.module.target_config());
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
            cranelift_codegen::verify_function(&ctx.func, codegen.output.module.isa()).unwrap();
            println!(
                "reference_stores={stores} old_loads={stores} barrier_calls={stores} atomic_stores={stores}"
            );
        }
    }

    #[test]
    fn interface_pair_stores_trace_only_object_word_in_linear_code() {
        use cranelift_codegen::ir::Opcode;
        for stores in [1, 16, 256] {
            let mut codegen_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
            let mut codegen = codegen_build.declaration_context();
            codegen.declare_runtime().unwrap();
            let mut ctx = codegen.output.module.make_context();
            let mut signature = codegen.output.module.make_signature();
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
            let object = builder.block_params(entry)[1];
            let vtable = builder.ins().iconst(types::I64, 1234);
            let value = builder.ins().iconcat(object, vtable);
            let barrier_id = codegen.func_id("willow_gc_write_barrier");
            let barrier = codegen
                .output
                .module
                .declare_func_in_func(barrier_id, builder.func);
            for i in 0..stores {
                emit_gc_heap_store_raw(
                    &mut builder,
                    Some(barrier),
                    owner,
                    i * 16,
                    value,
                    GcStoreDestination::ObjectField,
                    MemFlagsData::new(),
                );
            }
            builder.ins().return_(&[]);
            builder.finalize(codegen.output.module.target_config());
            let effects: Vec<_> = ctx
                .func
                .layout
                .block_insts(entry)
                .map(|i| ctx.func.dfg.insts[i].opcode())
                .filter(|op| {
                    matches!(
                        op,
                        Opcode::AtomicLoad | Opcode::Call | Opcode::AtomicStore | Opcode::Store
                    )
                })
                .collect();
            assert_eq!(
                effects,
                [
                    Opcode::AtomicLoad,
                    Opcode::Call,
                    Opcode::AtomicStore,
                    Opcode::Store
                ]
                .repeat(stores as usize)
            );
            cranelift_codegen::verify_function(&ctx.func, codegen.output.module.isa()).unwrap();
            println!(
                "interface_pair_stores={stores} old_loads={stores} barrier_calls={stores} atomic_stores={stores} vtable_stores={stores}"
            );
        }
    }

    #[test]
    fn compact_descriptors_scale_with_shapes_not_sites() {
        for sites in [1, 16, 256, 4096] {
            let mut codegen_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
            let codegen = codegen_build.declaration_context();
            let before = codegen
                .output
                .module
                .declarations()
                .get_data_objects()
                .count();
            let key = willow_abi::GcLayoutDescriptor {
                type_id: 2,
                layout_id: 7,
                gc_ref_mask: 1,
                size: 24,
            };
            let first = layout_descriptor(
                &mut codegen.output.module,
                &mut codegen.output.gc_layout_descriptors,
                key,
            );
            for _ in 1..sites {
                assert_eq!(
                    layout_descriptor(
                        &mut codegen.output.module,
                        &mut codegen.output.gc_layout_descriptors,
                        key
                    ),
                    first
                );
            }
            assert_eq!(
                codegen
                    .output
                    .module
                    .declarations()
                    .get_data_objects()
                    .count()
                    - before,
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
                        &mut codegen.output.module,
                        &mut codegen.output.gc_layout_descriptors,
                        other
                    ),
                    first
                );
            }
            assert_eq!(codegen.output.gc_layout_descriptors.len(), 4);
            println!(
                "sites={sites} repeated_shape_records=1 distinct_shape_records=4 descriptor_bytes=128"
            );
        }
    }

    #[test]
    fn bitmap_descriptors_scale_with_unique_contents_not_sites() {
        for words in [2, 8, 64] {
            for sites in [1, 16, 256] {
                let mut codegen_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
                let codegen = codegen_build.declaration_context();
                let before = codegen
                    .output
                    .module
                    .declarations()
                    .get_data_objects()
                    .count();
                let mut bitmap = vec![0; words];
                bitmap[words - 1] = 1;
                let first = bitmap_descriptor(
                    &mut codegen.output.module,
                    &mut codegen.output.gc_bitmap_descriptors,
                    &bitmap,
                );
                for _ in 1..sites {
                    assert_eq!(
                        bitmap_descriptor(
                            &mut codegen.output.module,
                            &mut codegen.output.gc_bitmap_descriptors,
                            &bitmap,
                        ),
                        first,
                    );
                }
                assert_eq!(
                    codegen
                        .output
                        .module
                        .declarations()
                        .get_data_objects()
                        .count()
                        - before,
                    1
                );
                // The low mask and bitmap length are identical; only a high
                // reference bit differs. A layout-fingerprint key would collide.
                bitmap[words - 1] = 2;
                let other = bitmap_descriptor(
                    &mut codegen.output.module,
                    &mut codegen.output.gc_bitmap_descriptors,
                    &bitmap,
                );
                assert_ne!(first.0, other.0);
                assert_ne!(first.1, other.1);
                assert_eq!(
                    codegen
                        .output
                        .module
                        .declarations()
                        .get_data_objects()
                        .count()
                        - before,
                    2
                );
                assert_eq!(codegen.output.gc_bitmap_descriptors.len(), 2);
            }
        }
    }

    #[test]
    fn tlab_start_publication_codegen_has_constant_work_per_site() {
        use cranelift_codegen::ir::Opcode;
        for sites in [1, 16, 256] {
            let mut codegen_build = Codegen::for_tests(&CompilerOptions::debug()).unwrap();
            let codegen = codegen_build.declaration_context();
            let mut ctx = codegen.output.module.make_context();
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
            builder.finalize(codegen.output.module.target_config());
            let instructions: Vec<_> = ctx
                .func
                .layout
                .block_insts(entry)
                .map(|inst| ctx.func.dfg.insts[inst].opcode())
                .collect();
            let count = |opcode| instructions.iter().filter(|&&op| op == opcode).count();
            // Owner-only publication: no fence, no locked RMW, no atomics.
            for atomic in [
                Opcode::AtomicRmw,
                Opcode::AtomicLoad,
                Opcode::AtomicStore,
                Opcode::Fence,
            ] {
                assert_eq!(count(atomic), 0, "{atomic:?}");
            }
            assert_eq!(count(Opcode::Load), 2 * sites);
            assert_eq!(count(Opcode::Store), sites);
            assert_eq!(instructions.len(), 18 * sites + 1);
            cranelift_codegen::verify_function(&ctx.func, codegen.output.module.isa()).unwrap();
            println!(
                "tlab_sites={sites} publication_instructions={} atomics=0",
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
        assert_eq!(GC_TLAB_STATE_SIZE, 24);
        assert_eq!(GC_TLAB_MAX_OBJECT_SIZE, 4096);
        assert_eq!(willow_abi::tlab::start_bits_offset(8), 16);
        assert_eq!(willow_abi::tlab::CHUNK_SIZE, 32768);
        assert_eq!(willow_abi::tlab::MARK_GRANULE_BYTES, 8);
    }

    #[test]
    fn scalar_pair_aggregate_trace_boundaries_and_linear_storage() {
        use willow_abi::SlotKind::{GcRef, ScalarPair, Word};
        for count in [1, 16, 30, 31, 32, 63, 64, 128] {
            for prefix_ref in [false, true] {
                let mut slots = Vec::new();
                if prefix_ref {
                    slots.push(GcRef);
                }
                slots.extend(std::iter::repeat_n(ScalarPair, count));
                slots.push(GcRef);
                slots.push(Word);
                let high_word = 1 + usize::from(prefix_ref) + 2 * count;
                for kind in [GcObjectKind::Closure, GcObjectKind::Enum] {
                    let layout = GcLayoutMetadata::aggregate(kind, 1, &slots, 8);
                    let words = high_word + 2;
                    assert_eq!(layout.payload_size, (words * 8) as i64);
                    let traced: Vec<_> = if layout.bitmap.is_empty() {
                        (0..64)
                            .filter(|bit| layout.gc_ref_mask & (1 << bit) != 0)
                            .collect()
                    } else {
                        assert_eq!(layout.bitmap.len(), words.div_ceil(64));
                        (0..words)
                            .filter(|bit| layout.bitmap[bit / 64] & (1 << (bit % 64)) != 0)
                            .collect()
                    };
                    let expected = if prefix_ref {
                        vec![1, high_word]
                    } else {
                        vec![high_word]
                    };
                    assert_eq!(traced, expected);
                    assert_eq!(layout.bitmap.is_empty(), high_word < 64);
                }
            }
        }
        // Widening non-reference storage alone never needs bitmap tracing.
        let slots = vec![ScalarPair; 128];
        let layout = GcLayoutMetadata::aggregate(GcObjectKind::Closure, 1, &slots, 8);
        assert_eq!(layout.payload_size, 257 * 8);
        assert_eq!(layout.gc_ref_mask, 0);
        assert!(layout.bitmap.is_empty());
    }

    #[test]
    fn interface_pair_aggregate_traces_objects_across_bitmap_boundaries() {
        use willow_abi::SlotKind::{GcRef, InterfacePair, ScalarPair, Word};
        for count in [1, 16, 30, 31, 32, 63, 64, 128] {
            let mut slots = vec![Word, ScalarPair];
            slots.extend(std::iter::repeat_n(InterfacePair, count));
            slots.push(GcRef);
            let words = 5 + 2 * count;
            for kind in [GcObjectKind::Closure, GcObjectKind::Enum] {
                let layout = GcLayoutMetadata::aggregate(kind, 1, &slots, 8);
                assert_eq!(layout.payload_size, (words * 8) as i64);
                let traced: Vec<_> = (0..words)
                    .filter(|bit| {
                        if layout.bitmap.is_empty() {
                            *bit < 64 && layout.gc_ref_mask & (1 << bit) != 0
                        } else {
                            layout.bitmap[bit / 64] & (1 << (bit % 64)) != 0
                        }
                    })
                    .collect();
                assert_eq!(traced, (0..=count).map(|i| 4 + 2 * i).collect::<Vec<_>>());
                assert_eq!(layout.bitmap.is_empty(), 4 + 2 * count < 64);
            }
        }
    }

    #[test]
    fn class_layout_carries_size_type_and_reference_mask() {
        let fields = vec![
            ("count".to_string(), Type::I64),
            ("name".to_string(), Type::String),
        ];
        let object = crate::compiler_db::layout::ObjectLayout::new(fields.into(), 8);
        let layout = GcLayoutMetadata::class(17, &object, &TypeMap::new());
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
