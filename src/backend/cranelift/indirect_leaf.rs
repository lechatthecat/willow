//! Guarded expansion of one small scalar target per function-value signature.
//! The code-address guard is essential: imports, stored/reassigned values and
//! unknown targets still use the ordinary checked indirect call.
use super::*;
use crate::diagnostics::Span;
use crate::ir::ir_variants;
use crate::ir::lowered::{LirFunction, LirInst, LirLocalId, LirOperand, LirRvalue, Terminator};
use cranelift_codegen::ir::{InstBuilder, Value, condcodes::IntCC, types};
use cranelift_module::Module;

#[derive(Default)]
pub(super) struct Candidates(HashMap<Type, Option<Leaf>>);

impl Candidates {
    pub(super) fn insert(&mut self, leaf: Leaf) {
        let signature = Type::Fn(leaf.params.clone(), Box::new(leaf.result_ty.clone()));
        // More than one candidate retains the existing dispatch. This caps
        // generated guards and avoids guessing a hot target without a profile.
        self.0
            .entry(signature)
            .and_modify(|old| *old = None)
            .or_insert(Some(leaf));
    }
}

/// The body a leaf was analyzed from, which decides whether its function
/// value carries the may-panic tag (see `LirRvalue::FunctionRef`/`Closure`).
#[derive(Clone)]
pub(super) enum LeafOwner {
    Function(FunctionId),
    Lambda(ExprId, FunctionId),
}

#[derive(Clone)]
pub(super) struct Leaf {
    target: FuncId,
    owner: LeafOwner,
    params: Vec<Type>,
    param_locals: Vec<LirLocalId>,
    result_ty: Type,
    result: LirOperand,
    operations: Vec<(LirLocalId, LirRvalue)>,
    locals: usize,
}

impl Leaf {
    pub(super) fn analyze(
        function: &LirFunction,
        target: FuncId,
        owner: LeafOwner,
        overflow_checks: crate::driver::OverflowChecks,
    ) -> Option<Self> {
        let scalar = |ty: &Type| {
            matches!(
                ty,
                Type::I8
                    | Type::I16
                    | Type::I32
                    | Type::U16
                    | Type::U32
                    | Type::U64
                    | Type::Char
                    | Type::U8
                    | Type::I64
                    | Type::F64
                    | Type::Bool
            )
        };
        if function.is_async
            || !function.captures.is_empty()
            || function.blocks.len() != 1
            || function.locals.len() > 32
            || function.params.len() > 8
            || !scalar(&function.return_type)
            || function
                .params
                .iter()
                .any(|p| p.by_reference || !scalar(&p.ty))
            || function
                .locals
                .iter()
                .any(|l| l.is_gc_owner() || !scalar(&l.ty))
        {
            return None;
        }
        let block = &function.blocks[0];
        if !block.recovery.is_empty() || block.instrs.len() > 8 {
            return None;
        }
        let Terminator::Return(Some(result)) = &block.terminator else {
            return None;
        };
        let mut operations = Vec::new();
        for instruction in &block.instrs {
            match instruction {
                LirInst::Compute { local, value, .. } => {
                    // Expansion emits plain scalar operators without a panic
                    // frame. Keep enabled overflow faults in the original body.
                    if value.has_overflow_check_with(overflow_checks) {
                        return None;
                    }
                    match value {
                        LirRvalue::Use(_) | LirRvalue::Unary { .. } => {}
                        LirRvalue::Binary { op, .. }
                            if !matches!(
                                op,
                                BinOp::Div | BinOp::Rem | BinOp::Pow | BinOp::And | BinOp::Or
                            ) => {}
                        ir_variants!(
                            LirRvalue;
                            BeginReferenceCall, AwaitFuture, StartTask, SelectIdleWait, Coerce,
                            ArrayAlloc, CaptureArrayOwner, ArrayStore, Index, ObjectAlloc,
                            FieldLoad, FieldStore, StaticField, StaticStore, StaticCall,
                            EnumAlloc, EnumPayloadStore, ConstructorCall, Range, EnumMethod,
                            BuiltinCall, FormatScalar, Panic, Recover, ReferenceDebug,
                            RebindResultError, IntoError, PrepareMethod, MethodCall,
                            StringLiteral, FunctionRef, Closure, Print, IntrinsicCall, DirectCall,
                            IndirectCall, Binary, StringConcat
                        ) => return None,
                    }
                    operations.push((*local, value.clone()));
                }
                LirInst::Let { local, value, .. } | LirInst::Assign { local, value, .. } => {
                    operations.push((*local, LirRvalue::Use(value.clone())))
                }
                LirInst::ClearScopeRoots { .. } => {}
                ir_variants!(
                    LirInst;
                    EnterDeferScope, LeaveDeferScope, FlushDefers, RestoreCallPreparation, Defer, SelectInit, SelectProbe,
                    SelectPick, SelectUnregister, SelectCommit, ReleaseLock, MatchTest, MatchBind,
                    Unsupported
                ) => return None,
            }
        }
        let param_locals = function
            .params
            .iter()
            .map(|p| {
                function
                    .locals
                    .iter()
                    .find(|l| l.parameter && l.name == p.name)
                    .map(|l| l.id)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            target,
            owner,
            params: function.params.iter().map(|p| p.ty.clone()).collect(),
            param_locals,
            result_ty: function.return_type.clone(),
            result: result.clone(),
            operations,
            locals: function.locals.len(),
        })
    }
}

impl FuncGen<'_, '_> {
    fn scalar_leaf_operand(&mut self, operand: &LirOperand, locals: &[Option<Value>]) -> Value {
        match operand {
            LirOperand::Local(id) => locals[id.0 as usize].expect("validated scalar leaf local"),
            LirOperand::Integer { value, ty } => {
                let ty = self.clif_type(ty);
                self.builder.ins().iconst(ty, *value)
            }
            LirOperand::Int(n) => self.builder.ins().iconst(types::I64, *n),
            LirOperand::Float(n) => self.builder.ins().f64const(*n),
            LirOperand::Bool(b) => self.builder.ins().iconst(types::I8, i64::from(*b)),
            LirOperand::Reference { .. } => unreachable!("scalar leaf operand"),
        }
    }

    // Independent call signature, operands and diagnostic context. `word` is
    // the function value's (tagged) code word.
    pub(super) fn emit_promoted_fn_call(
        &mut self,
        signature: cranelift_codegen::ir::SigRef,
        word: Value,
        args: &[Value],
        (params, result): (&[Type], &Type),
        name: &str,
        span: Span,
    ) -> Value {
        // Cranelift can rematerialize dependent scalar operands on both guard
        // paths. Bound expansion across the entire body, not merely each site:
        // chains of C calls must not produce O(C²) machine code. The common
        // one-site hot loop still benefits; later sites retain one shared call.
        if self.reuse.indirect_leaf_promoted
            || self.indirect_leaves.0.is_empty()
            || params.len() > 8
            || !matches!(
                result,
                Type::I8
                    | Type::I16
                    | Type::I32
                    | Type::U16
                    | Type::U32
                    | Type::U64
                    | Type::Char
                    | Type::U8
                    | Type::I64
                    | Type::F64
                    | Type::Bool
            )
            || params.iter().any(|ty| {
                !matches!(
                    ty,
                    Type::I8
                        | Type::I16
                        | Type::I32
                        | Type::U16
                        | Type::U32
                        | Type::U64
                        | Type::Char
                        | Type::U8
                        | Type::I64
                        | Type::F64
                        | Type::Bool
                )
            })
        {
            return self.emit_lir_indirect_call(signature, word, args, name, span);
        }
        let key = Type::Fn(params.to_vec(), Box::new(result.clone()));
        let leaf = self
            .indirect_leaves
            .0
            .get(&key)
            .and_then(Option::as_ref)
            .cloned();
        let Some(leaf) = leaf else {
            return self.emit_lir_indirect_call(signature, word, args, name, span);
        };
        let fref = self
            .module
            .declare_func_in_func(leaf.target, self.builder.func);
        let target_sig =
            &self.builder.func.dfg.signatures[self.builder.func.dfg.ext_funcs[fref].signature];
        let call_sig = &self.builder.func.dfg.signatures[signature];
        // The call passes the value itself last; a named target ignores it and
        // a lambda target declares it, so either prefix of the call's
        // parameters is the same ABI.
        let declared = target_sig.params.len();
        if target_sig.call_conv != call_sig.call_conv
            || target_sig.returns != call_sig.returns
            || !(declared + 1 == call_sig.params.len() || declared == call_sig.params.len())
            || target_sig.params[..] != call_sig.params[..declared]
        {
            return self.emit_lir_indirect_call(signature, word, args, name, span);
        }
        self.reuse.indirect_leaf_promoted = true;
        if *crate::diagnostic_settings::PANIC_DEPTH_LOAD_LOG {
            eprintln!("[indirect-leaf-expand]");
        }
        // Every value of this unit that names the leaf carries the same tag,
        // so one comparison of the raw word checks the address and the tag; a
        // value from another unit with a different tag only takes the
        // ordinary call.
        let may_panic = match &leaf.owner {
            LeafOwner::Function(id) => self.user_function_may_panic_id(id),
            LeafOwner::Lambda(expr, id) => self.lambda_may_panic(expr, id),
        };
        let addr = self
            .builder
            .ins()
            .func_addr(reference_type(self.module.target_config()), fref);
        let expected = if may_panic {
            self.builder
                .ins()
                .bor_imm_s(addr, willow_abi::FUNCTION_CODE_MAY_PANIC as i64)
        } else {
            addr
        };
        let matches = self.builder.ins().icmp(IntCC::Equal, word, expected);
        let direct = self.builder.create_block();
        let fallback = self.builder.create_block();
        let merge = self.builder.create_block();
        let result_ty = self.clif_type(result);
        let merged = self.builder.append_block_param(merge, result_ty);
        self.builder.ins().brif(matches, direct, &[], fallback, &[]);
        self.builder.switch_to_block(direct);
        self.builder.seal_block(direct);
        let mut locals = vec![None; leaf.locals];
        for (id, value) in leaf.param_locals.iter().zip(args) {
            locals[id.0 as usize] = Some(*value);
        }
        for (id, operation) in &leaf.operations {
            let value = match operation {
                LirRvalue::Use(input) => self.scalar_leaf_operand(input, &locals),
                LirRvalue::Unary { op, operand, ty } => {
                    let v = self.scalar_leaf_operand(operand, &locals);
                    match op {
                        UnaryOp::Neg if *ty == Type::F64 => self.builder.ins().fneg(v),
                        UnaryOp::Neg => self.builder.ins().ineg(v),
                        UnaryOp::Not if ty.integer_layout().is_some() => self.builder.ins().bnot(v),
                        UnaryOp::Not => self.builder.ins().bxor_imm_s(v, 1),
                    }
                }
                LirRvalue::Binary {
                    op,
                    lhs,
                    rhs,
                    operand_ty,
                } => {
                    let l = self.scalar_leaf_operand(lhs, &locals);
                    let r = self.scalar_leaf_operand(rhs, &locals);
                    self.emit_lir_binop(op, l, r, operand_ty)
                }
                ir_variants!(
                    LirRvalue;
                    BeginReferenceCall, AwaitFuture, StartTask, SelectIdleWait, Coerce,
                    ArrayAlloc, CaptureArrayOwner, ArrayStore, Index, ObjectAlloc, FieldLoad,
                    FieldStore, StaticField, StaticStore, StaticCall, EnumAlloc, EnumPayloadStore,
                    ConstructorCall, Range, EnumMethod, BuiltinCall, FormatScalar, Panic, Recover,
                    ReferenceDebug, RebindResultError, IntoError, PrepareMethod, MethodCall,
                    StringLiteral, FunctionRef, Closure, Print, IntrinsicCall, DirectCall,
                    IndirectCall, StringConcat
                ) => unreachable!("validated scalar leaf operation"),
            };
            locals[id.0 as usize] = Some(value);
        }
        let value = self.scalar_leaf_operand(&leaf.result, &locals);
        self.builder.ins().jump(merge, &[value.into()]);
        self.builder.switch_to_block(fallback);
        self.builder.seal_block(fallback);
        let value = self.emit_lir_indirect_call(signature, word, args, name, span);
        self.builder.ins().jump(merge, &[value.into()]);
        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        merged
    }
}
