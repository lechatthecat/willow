//! Runtime intrinsic and builtin emission.

use super::*;
use willow_abi::runtime_symbols::names as rt;

impl<'a, 'b> FuncGen<'a, 'b> {
    fn emit_atomic_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        receiver: cranelift_codegen::ir::Value,
        receiver_ty: &Type,
        args: &[cranelift_codegen::ir::Value],
    ) -> cranelift_codegen::ir::Value {
        use Intrinsic::*;
        let cell = atomic_cell(receiver_ty).expect("validated atomic intrinsic");
        // Cranelift atomics are SeqCst, including failed strong CAS.
        // The handle already points to the payload; no runtime check or
        // safepoint existed for these ABI operations. Keep operand roots
        // and evaluation order established by LIR.
        let address = self
            .builder
            .ins()
            .iadd_imm_s(receiver, i64::from(willow_abi::atomic_layout::VALUE_OFFSET));
        let flags = MemFlagsData::new();
        let ty = if cell.is_i64 { types::I64 } else { types::I8 };
        let normalize = |this: &mut Self, value| {
            if cell.is_i64 {
                value
            } else {
                this.builder.ins().icmp_imm_s(IntCC::NotEqual, value, 0)
            }
        };
        match intrinsic {
            AtomicLoad => self.builder.ins().atomic_load(ty, flags, address),
            AtomicStore => {
                let value = normalize(self, args[0]);
                if self.module.isa().triple().architecture == target_lexicon::Architecture::X86_64 {
                    // x86 SeqCst store is an exchange with its old value unused.
                    // Cranelift's atomic_store emits mov + mfence, substantially
                    // slower here than xchg (also used by the Rust runtime).
                    self.builder
                        .ins()
                        .atomic_rmw(ty, flags, AtomicRmwOp::Xchg, address, value);
                } else {
                    self.builder.ins().atomic_store(flags, value, address);
                }
                self.builder.ins().iconst(types::I8, 0)
            }
            AtomicCompareExchange => {
                let expected = normalize(self, args[0]);
                let desired = normalize(self, args[1]);
                let old = self
                    .builder
                    .ins()
                    .atomic_cas(flags, address, expected, desired);
                self.builder.ins().icmp(IntCC::Equal, old, expected)
            }
            AtomicSwap | AtomicAdd | AtomicSub => {
                let op = match intrinsic {
                    AtomicSwap => AtomicRmwOp::Xchg,
                    AtomicAdd => AtomicRmwOp::Add,
                    AtomicSub => AtomicRmwOp::Sub,
                    _ => unreachable!(),
                };
                let value = normalize(self, args[0]);
                self.builder.ins().atomic_rmw(ty, flags, op, address, value)
            }
            _ => unreachable!(),
        }
    }
}

/// Relocatable root slots of an intrinsic's GC-managed operands.
pub(super) struct OperandRoots {
    receiver: Option<cranelift_codegen::ir::StackSlot>,
    args: Vec<Option<cranelift_codegen::ir::StackSlot>>,
}

impl<'a, 'b> FuncGen<'a, 'b> {
    /// Reload a possibly rooted operand after a GC point; unrooted (scalar)
    /// operands are returned unchanged.
    fn emit_reload_operand(
        &mut self,
        slot: Option<cranelift_codegen::ir::StackSlot>,
        value: cranelift_codegen::ir::Value,
    ) -> cranelift_codegen::ir::Value {
        match slot {
            Some(slot) => self.emit_reload_root(slot, value),
            None => value,
        }
    }

    /// Operands are already evaluated, typed, and rooted by LIR. This emitter
    /// performs one resolved operation; it never walks executable syntax.
    // Keep explicit emission operands aligned with the LIR/runtime ABI.
    pub(super) fn emit_flat_intrinsic(
        &mut self,
        intrinsic: Intrinsic,
        (receiver, receiver_ty): (cranelift_codegen::ir::Value, &Type),
        args: &[cranelift_codegen::ir::Value],
        arg_types: &[Type],
        _result: &Type,
        span: Span,
    ) -> cranelift_codegen::ir::Value {
        // A plain metadata read has no safepoint and needs no transient roots.
        if intrinsic == Intrinsic::StringLen {
            return self.emit_string_length(receiver);
        }
        // Length has no allocating/coercing successful path. Its only runtime
        // fallback receives null, so no transient receiver root is needed.
        if matches!(intrinsic, Intrinsic::ArrayLen | Intrinsic::FrozenArrayLen) {
            return self.emit_array_access(receiver, None, None);
        }
        if intrinsic == Intrinsic::ArrayResize
            && matches!(
                array_element_type(receiver_ty),
                Type::I64 | Type::F64 | Type::Bool
            )
        {
            return self.emit_scalar_array_resize(receiver, args[0]);
        }
        // Arguments have already been evaluated and rooted by LIR. Atomic
        // instructions cannot allocate, block, or safepoint, so they need no
        // additional transient receiver root or runtime cleanup scaffolding.
        if matches!(
            intrinsic,
            Intrinsic::AtomicLoad
                | Intrinsic::AtomicStore
                | Intrinsic::AtomicSwap
                | Intrinsic::AtomicCompareExchange
                | Intrinsic::AtomicAdd
                | Intrinsic::AtomicSub
        ) {
            return self.emit_atomic_intrinsic(intrinsic, receiver, receiver_ty, args);
        }
        // Frame-backed references can otherwise move during a blocking runtime
        // call. The operand roots are relocatable: an arm that reaches an
        // allocation before its runtime call reloads the operands it still
        // needs from these slots. `coerce_to_target` never allocates (it
        // builds inline interface pairs), so the only such allocation is
        // boxing an inline pair into its storage word.
        let roots_before = self.roots.gc_root_count;
        let roots = OperandRoots {
            receiver: is_gc_managed(receiver_ty, self.enum_infos)
                .then(|| self.emit_push_relocatable_root(receiver)),
            args: args
                .iter()
                .zip(arg_types)
                .map(|(&value, ty)| {
                    is_gc_managed(ty, self.enum_infos)
                        .then(|| self.emit_push_relocatable_root(value))
                })
                .collect(),
        };
        let value = self.emit_flat_intrinsic_inner(
            intrinsic,
            (receiver, receiver_ty),
            args,
            arg_types,
            &roots,
            span,
        );
        self.emit_pop_roots_n(self.roots.gc_root_count - roots_before);
        self.roots.gc_root_count = roots_before;
        value
    }

    // Keep explicit emission operands aligned with the LIR/runtime ABI.
    fn emit_flat_intrinsic_inner(
        &mut self,
        intrinsic: Intrinsic,
        (receiver, receiver_ty): (cranelift_codegen::ir::Value, &Type),
        args: &[cranelift_codegen::ir::Value],
        arg_types: &[Type],
        roots: &OperandRoots,
        _span: Span,
    ) -> cranelift_codegen::ir::Value {
        use Intrinsic::*;
        match intrinsic {
            StringToString | TaskResult => receiver,
            // Explicit overflow behavior ignores the build mode.
            I64WrappingAdd => self.builder.ins().iadd(receiver, args[0]),
            I64WrappingSub => self.builder.ins().isub(receiver, args[0]),
            I64WrappingMul => self.builder.ins().imul(receiver, args[0]),
            I64WrappingNeg => self.builder.ins().ineg(receiver),
            I64CheckedAdd | I64CheckedSub | I64CheckedMul | I64CheckedNeg => {
                let (value, overflowed) = match intrinsic {
                    I64CheckedAdd => self.emit_overflowing_binop(&BinOp::Add, receiver, args[0]),
                    I64CheckedSub => self.emit_overflowing_binop(&BinOp::Sub, receiver, args[0]),
                    I64CheckedMul => self.emit_overflowing_binop(&BinOp::Mul, receiver, args[0]),
                    _ => self.emit_overflowing_neg(receiver),
                };
                // `Option<i64>` is a scalar pair: tag 0 `Some`, 1 `None` with
                // a zero payload, as `emit_alloc_option_none` builds it.
                let tag = self.builder.ins().uextend(types::I64, overflowed);
                let zero = self.builder.ins().iconst(types::I64, 0);
                let payload = self.builder.ins().select(overflowed, zero, value);
                self.emit_pair(tag, payload)
            }
            StringConcatArray => {
                self.emit_value_runtime_call(rt::willow_string_concat_array, &[receiver])
            }
            StringLen => self.emit_string_length(receiver),
            StringByteAt => {
                self.emit_value_runtime_call(rt::willow_string_byte_at, &[receiver, args[0]])
            }
            StringSubstring => self.emit_value_runtime_call(
                rt::willow_string_substring,
                &[receiver, args[0], args[1]],
            ),
            StringSplit => {
                self.emit_value_runtime_call(rt::willow_string_split, &[receiver, args[0]])
            }
            StringFind => {
                self.emit_value_runtime_call(rt::willow_string_find, &[receiver, args[0]])
            }
            StringRepeat => {
                self.emit_value_runtime_call(rt::willow_string_repeat, &[receiver, args[0]])
            }
            StringReplace => self
                .emit_value_runtime_call(rt::willow_string_replace, &[receiver, args[0], args[1]]),
            StringTrimStart => {
                self.emit_value_runtime_call(rt::willow_string_trim_start, &[receiver])
            }
            StringTrimEnd => self.emit_value_runtime_call(rt::willow_string_trim_end, &[receiver]),
            StringToLower => self.emit_value_runtime_call(rt::willow_string_to_lower, &[receiver]),
            StringToUpper => self.emit_value_runtime_call(rt::willow_string_to_upper, &[receiver]),
            StringTrim => self.emit_value_runtime_call(rt::willow_string_trim, &[receiver]),
            StringContains | StringStartsWith | StringEndsWith => {
                let symbol = if intrinsic == StringContains {
                    rt::willow_string_contains
                } else if intrinsic == StringEndsWith {
                    rt::willow_string_ends_with
                } else {
                    rt::willow_string_starts_with
                };
                let value = self.emit_value_runtime_call(symbol, &[receiver, args[0]]);
                self.builder.ins().ireduce(types::I8, value)
            }

            I64ToString => self.emit_value_runtime_call(rt::willow_i64_to_string, &[receiver]),
            F64ToString => self.emit_value_runtime_call(rt::willow_f64_to_string, &[receiver]),
            BoolToString => self.emit_value_runtime_call(rt::willow_bool_to_string, &[receiver]),
            TaskCancel => {
                let id = self.builder.ins().load(
                    types::I64,
                    MemFlagsData::new(),
                    receiver,
                    async_frame_slot_offset(
                        FRAME_SLOT_TASK_ID,
                        reference_type(self.module.target_config()).bytes(),
                    ),
                );
                self.emit_void_runtime_call(rt::willow_sched_cancel, &[id]);
                self.builder.ins().iconst(types::I8, 0)
            }
            TaskIsCancelled => {
                let raw = self.emit_value_runtime_call(rt::willow_frame_is_cancelled, &[receiver]);
                self.builder.ins().ireduce(types::I8, raw)
            }
            TokenIsCancelled | ScopeIsCancelled | TokenCancel | ScopeCancel | TokenChild
            | ScopeChild | TokenAttach | ScopeAdd | ScopeFinish => {
                cancellation_handle(receiver_ty).expect("validated cancellation intrinsic");
                let symbol = match intrinsic {
                    TokenIsCancelled => rt::willow_cancellation_token_is_cancelled,
                    ScopeIsCancelled => rt::willow_task_scope_is_cancelled,
                    TokenCancel => rt::willow_cancellation_token_cancel,
                    ScopeCancel => rt::willow_task_scope_cancel,
                    TokenChild => rt::willow_cancellation_token_child,
                    ScopeChild => rt::willow_task_scope_child,
                    TokenAttach => rt::willow_cancellation_token_attach,
                    ScopeAdd => rt::willow_task_scope_add,
                    ScopeFinish => rt::willow_task_scope_finish,
                    _ => unreachable!(),
                };
                let mut values = vec![receiver];
                values.extend_from_slice(args);
                let value = self.emit_runtime_call_with_cleanup(symbol, &values, |_| {});
                match intrinsic {
                    TokenIsCancelled | ScopeIsCancelled => {
                        self.builder.ins().ireduce(types::I8, value.unwrap())
                    }
                    _ => value.unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0)),
                }
            }
            AtomicLoad
            | AtomicStore
            | AtomicSwap
            | AtomicCompareExchange
            | AtomicAdd
            | AtomicSub => {
                unreachable!("atomic operations use the non-allocating fast path")
            }
            CellGet | CellSet | RwCellRead | RwCellWrite => {
                let (_, element) = blocking_cell(receiver_ty).expect("validated cell intrinsic");
                let symbol = match intrinsic {
                    CellGet => rt::willow_blocking_cell_get,
                    CellSet => rt::willow_blocking_cell_set,
                    RwCellRead => rt::willow_blocking_rw_cell_read,
                    RwCellWrite => rt::willow_blocking_rw_cell_write,
                    _ => unreachable!(),
                };
                let mut values = vec![receiver];
                if let Some(&value) = args.first() {
                    let word = self.emit_to_storage_word(value, element);
                    if self.is_inline_pair(element) {
                        self.emit_push_call_root(word);
                        values[0] = self.emit_reload_operand(roots.receiver, receiver);
                    }
                    values.push(word);
                }
                match self.emit_runtime_call_with_cleanup(symbol, &values, |_| {}) {
                    Some(value) => self.emit_from_storage_word(value, element),
                    None => self.builder.ins().iconst(types::I8, 0),
                }
            }
            ChannelRecv | ChannelRecvOpt => {
                let element = builtin_types::unary_arg(receiver_ty, B::Channel)
                    .expect("validated channel intrinsic");
                self.emit_channel_recv(receiver, element, intrinsic == ChannelRecvOpt)
            }
            ChannelSend | ChannelClose => {
                let element = builtin_types::unary_arg(receiver_ty, B::Channel)
                    .expect("validated channel intrinsic");
                let symbol = if intrinsic == ChannelClose {
                    rt::willow_channel_close
                } else {
                    channel_runtime_symbol(
                        element,
                        [
                            rt::willow_channel_send_i64,
                            rt::willow_channel_send_bool,
                            rt::willow_channel_send_f64,
                            rt::willow_channel_send_ptr,
                        ],
                    )
                };
                let mut values = vec![receiver];
                if let Some(&value) = args.first() {
                    let value = self.coerce_to_target(value, &arg_types[0], element);
                    let value = if self.is_inline_pair(element) {
                        let word = self.emit_to_storage_word(value, element);
                        self.emit_push_call_root(word);
                        values[0] = self.emit_reload_operand(roots.receiver, receiver);
                        word
                    } else {
                        value
                    };
                    values.push(value);
                }
                self.emit_runtime_call_with_cleanup(symbol, &values, |_| {});
                self.builder.ins().iconst(types::I8, 0)
            }
            ArrayLen | FrozenArrayLen => self.emit_array_access(receiver, None, None),
            ArrayResize => {
                let element = array_element_type(receiver_ty);
                let mask = if self.is_interface_pair(&element) {
                    1
                } else if self.is_inline_pair(&element) {
                    0
                } else {
                    -1
                };
                let mask = self.builder.ins().iconst(types::I64, mask);
                self.emit_void_runtime_call(rt::willow_array_resize, &[receiver, args[0], mask]);
                self.builder.ins().iconst(types::I8, 0)
            }
            StringDebug => self.emit_value_runtime_call(rt::willow_string_debug, &[receiver]),
            ArrayCopy => {
                let runtime = if self.is_inline_pair(&array_element_type(receiver_ty)) {
                    rt::willow_array_copy_scalar_pairs
                } else {
                    rt::willow_array_copy
                };
                self.emit_value_runtime_call(runtime, &[receiver])
            }
            MapLen | FrozenMapLen => self.emit_value_runtime_call(rt::willow_map_len, &[receiver]),
            MapToString => self.emit_value_runtime_call(rt::willow_map_to_string, &[receiver]),
            MapFreeze => self.emit_value_runtime_call(rt::willow_map_copy, &[receiver]),
            MapKeys | MapValues | MapEntries => {
                let mode = self.builder.ins().iconst(
                    types::I64,
                    match intrinsic {
                        MapKeys => 0,
                        MapValues => 1,
                        _ => 2,
                    },
                );
                let (_, parameters) = lir_collection(receiver_ty).expect("validated map intrinsic");
                let pair_kind = if self.is_interface_pair(&parameters[1]) {
                    2
                } else {
                    i64::from(self.is_inline_pair(&parameters[1]))
                };
                let pair = self.builder.ins().iconst(types::I64, pair_kind);
                self.emit_value_runtime_call(rt::willow_map_snapshot, &[receiver, mode, pair])
            }
            MapContains | FrozenMapContains | MapGet | FrozenMapGet | MapRemove | MapInsert => {
                let (_, parameters) = lir_collection(receiver_ty).expect("validated map intrinsic");
                let (key_ty, value_ty) = (&parameters[0], &parameters[1]);
                // Boxing a pair key or value allocates, so every operand
                // used after one is reloaded from its root.
                let key = self.emit_to_storage_word(args[0], key_ty);
                let key_slot = (self.is_inline_pair(key_ty) && intrinsic == MapInsert)
                    .then(|| self.emit_push_relocatable_root(key));
                let receiver = self.emit_reload_operand(roots.receiver, receiver);
                let key_ref = self.map_is_ref_flag(key_ty);
                match intrinsic {
                    MapContains | FrozenMapContains => {
                        let value = self.emit_value_runtime_call(
                            rt::willow_map_contains,
                            &[receiver, key, key_ref],
                        );
                        self.builder.ins().ireduce(types::I8, value)
                    }
                    MapGet | FrozenMapGet | MapRemove => self.emit_map_get_value(
                        receiver,
                        key,
                        key_ref,
                        value_ty,
                        intrinsic == MapRemove,
                    ),
                    MapInsert => {
                        // Only boxing an inline-pair value allocates; the
                        // coercion and a non-pair storage word do not.
                        let value = self.emit_reload_operand(roots.args[1], args[1]);
                        let value = self.coerce_to_target(value, &arg_types[1], value_ty);
                        let word = self.emit_to_storage_word(value, value_ty);
                        let (receiver, key) = if self.is_inline_pair(value_ty) {
                            self.emit_push_call_root(word);
                            (
                                self.emit_reload_operand(roots.receiver, receiver),
                                match key_slot {
                                    Some(slot) => self.emit_reload_root(slot, key),
                                    None => self.emit_reload_operand(roots.args[0], key),
                                },
                            )
                        } else {
                            (receiver, key)
                        };
                        let value_ref = self.map_is_ref_flag(value_ty);
                        self.emit_void_runtime_call(
                            rt::willow_map_insert,
                            &[receiver, key, key_ref, word, value_ref],
                        );
                        self.builder.ins().iconst(types::I8, 0)
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    /// Resolved enum combinators consume already-evaluated, typed operands.
    pub(super) fn emit_flat_enum_method(
        &mut self,
        receiver: cranelift_codegen::ir::Value,
        receiver_ty: &Type,
        method: &str,
        args: &[cranelift_codegen::ir::Value],
        arg_types: &[Type],
        span: Span,
    ) -> cranelift_codegen::ir::Value {
        const OK_TAG: i64 = 0;
        const ERR_TAG: i64 = 1;
        let resolved = builtin_types::resolve(receiver_ty)
            .expect("Option/Result receiver vetted by eligibility");
        let id = resolved.id;
        let payload = |i: usize| resolved.args.get(i).cloned().unwrap_or(Type::Void);
        let (ok_ty, err_ty) = (payload(0), payload(1));
        let recv = receiver;
        let roots_before = self.roots.gc_root_count;
        // The operands stay rooted for the whole method. Every combinator
        // reads the receiver and its arguments before its first GC point (the
        // callback call, an allocation or a panic), never after one, so the
        // roots may relocate without reloads. A combinator that needs an
        // operand after a GC point must reload it from a relocatable root;
        // `option_result_combinators_read_receivers_before_gc_points` runs
        // every combinator under moving, poisoning stress.
        for (&value, ty) in args.iter().zip(arg_types) {
            if is_gc_managed(ty, self.enum_infos) {
                self.emit_push_relocatable_root(value);
            }
        }
        if is_gc_managed(receiver_ty, self.enum_infos) {
            self.emit_push_relocatable_root(recv);
        }
        let value = match (id, method) {
            (B::Option, "is_some") => self.emit_option_is_some(recv, &ok_ty),
            (B::Option, "is_none") => {
                let some = self.emit_option_is_some(recv, &ok_ty);
                let zero = self.builder.ins().iconst(types::I8, 0);
                self.builder.ins().icmp(IntCC::Equal, some, zero)
            }
            (B::Option, "unwrap") => {
                let msg = self.emit_string_literal("called `Option::unwrap()` on a `None` value");
                self.emit_option_unwrap(recv, &ok_ty, msg, Some(span))
            }
            (B::Option, "expect") => {
                let msg = args[0];
                self.emit_option_unwrap(recv, &ok_ty, msg, Some(span))
            }
            (B::Option, "unwrap_or") => {
                let default_val = args[0];
                self.emit_option_unwrap_or(recv, &ok_ty, default_val)
            }
            (B::Result, "is_ok") | (B::Result, "is_err") => {
                let tag = self.emit_load_enum_tag(recv);
                let want = self
                    .builder
                    .ins()
                    .iconst(types::I64, if method == "is_ok" { OK_TAG } else { ERR_TAG });
                self.builder.ins().icmp(IntCC::Equal, tag, want)
            }
            (B::Result, "unwrap") => {
                let msg = self.emit_string_literal("called `Result::unwrap()` on an `Err` value");
                self.emit_enum_unwrap(recv, &ok_ty, OK_TAG, msg, Some(span))
            }
            (B::Result, "unwrap_err") => {
                let msg =
                    self.emit_string_literal("called `Result::unwrap_err()` on an `Ok` value");
                self.emit_enum_unwrap(recv, &err_ty, ERR_TAG, msg, Some(span))
            }
            (B::Result, "expect") => {
                let msg = args[0];
                self.emit_enum_unwrap(recv, &ok_ty, OK_TAG, msg, Some(span))
            }
            (B::Result, "unwrap_or") => {
                let default_val = args[0];
                self.emit_enum_unwrap_or(recv, &ok_ty, OK_TAG, default_val)
            }
            // The callable-taking combinators. The receiver
            // is already rooted above, which is what makes calling an arbitrary
            // function — and allocating the new enum around its result — safe
            // here. Shared helpers enforce tag layout, the pointer niche,
            // and the indirect-call ABI.
            (B::Option, "map") => {
                let (f_val, f_ty) = (args[0], arg_types[0].clone());
                let produced = fn_return_type(&f_ty);
                self.emit_option_map(recv, &ok_ty, &produced, f_val, &f_ty)
            }
            (B::Option, "and_then") => {
                let (f_val, f_ty) = (args[0], arg_types[0].clone());
                self.emit_option_and_then(recv, &ok_ty, f_val, &f_ty)
            }
            (B::Option, "or_else") => {
                let (f_val, f_ty) = (args[0], arg_types[0].clone());
                self.emit_option_or_else(recv, &ok_ty, f_val, &f_ty)
            }
            (B::Result, "map") => {
                let (f_val, f_ty) = (args[0], &arg_types[0]);
                self.emit_result_map_side(ResultSide::Ok, recv, &ok_ty, &err_ty, f_val, f_ty)
            }
            (B::Result, "map_err") => {
                let (f_val, f_ty) = (args[0], &arg_types[0]);
                self.emit_result_map_side(ResultSide::Err, recv, &ok_ty, &err_ty, f_val, f_ty)
            }
            (B::Result, "and_then") => {
                let (f_val, f_ty) = (args[0], &arg_types[0]);
                self.emit_result_chain_side(ResultSide::Ok, recv, &ok_ty, &err_ty, f_val, f_ty)
            }
            (B::Result, "or_else") => {
                let (f_val, f_ty) = (args[0], &arg_types[0]);
                self.emit_result_chain_side(ResultSide::Err, recv, &ok_ty, &err_ty, f_val, f_ty)
            }
            _ => unreachable!("unsupported `{method}` on an Option/Result passed eligibility"),
        };
        self.emit_pop_roots_n(self.roots.gc_root_count - roots_before);
        self.roots.gc_root_count = roots_before;
        value
    }

    pub(super) fn emit_flat_format_scalar(
        &mut self,
        value: cranelift_codegen::ir::Value,
        ty: &Type,
        format: Option<crate::interpolate::F64Format>,
    ) -> cranelift_codegen::ir::Value {
        if let Some(format) = format {
            assert_eq!(*ty, Type::F64, "validated f64 format operand");
            return self.emit_value_runtime_call(format.runtime_symbol(), &[value]);
        }
        match ty {
            Type::String => value,
            Type::I64 => self.emit_value_runtime_call(rt::willow_i64_to_string, &[value]),
            Type::F64 => self.emit_value_runtime_call(rt::willow_f64_to_string, &[value]),
            Type::Bool => self.emit_value_runtime_call(rt::willow_bool_to_string, &[value]),
            _ => unreachable!("validated display format operand"),
        }
    }

    pub(super) fn emit_flat_builtin_call(
        &mut self,
        callee: &crate::semantic::ids::FunctionId,
        args: &[cranelift_codegen::ir::Value],
    ) -> cranelift_codegen::ir::Value {
        let runtime = builtin_call_runtime_name(callee.unqualified_name().as_deref().unwrap_or(""))
            .expect("resolved builtin function");
        self.emit_runtime_call_with_cleanup(runtime, args, |_| {})
            .unwrap_or_else(|| self.builder.ins().iconst(types::I8, 0))
    }
}
