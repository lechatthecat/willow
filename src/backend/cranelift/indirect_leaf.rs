//! Guarded expansion of one small scalar target per function-value signature.
//! The address guard is essential: imports, stored/reassigned values and unknown
//! targets still use the ordinary checked indirect call. No fn-value ABI change.
use super::*;
use crate::diagnostics::Span;
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

#[derive(Clone)]
pub(super) struct Leaf {
    target: FuncId,
    params: Vec<Type>,
    param_locals: Vec<LirLocalId>,
    result_ty: Type,
    result: LirOperand,
    operations: Vec<(LirLocalId, LirRvalue)>,
    locals: usize,
}

impl Leaf {
    pub(super) fn analyze(function: &LirFunction, target: FuncId) -> Option<Self> {
        let scalar = |ty: &Type| matches!(ty, Type::I64 | Type::F64 | Type::Bool);
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
                    match value {
                        LirRvalue::Use(_) | LirRvalue::Unary { .. } => {}
                        LirRvalue::Binary { op, .. }
                            if !matches!(
                                op,
                                BinOp::Div | BinOp::Rem | BinOp::Pow | BinOp::And | BinOp::Or
                            ) => {}
                        _ => return None,
                    }
                    operations.push((*local, value.clone()));
                }
                LirInst::Let { local, value, .. } | LirInst::Assign { local, value, .. } => {
                    operations.push((*local, LirRvalue::Use(value.clone())))
                }
                LirInst::ClearScopeRoots { .. } => {}
                _ => return None,
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
            LirOperand::Int(n) => self.builder.ins().iconst(types::I64, *n),
            LirOperand::Float(n) => self.builder.ins().f64const(*n),
            LirOperand::Bool(b) => self.builder.ins().iconst(types::I8, i64::from(*b)),
            _ => unreachable!("scalar leaf operand"),
        }
    }

    // Independent call signature, operands and diagnostic context.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn emit_promoted_fn_call(
        &mut self,
        signature: cranelift_codegen::ir::SigRef,
        target: Value,
        args: &[Value],
        params: &[Type],
        result: &Type,
        name: &str,
        span: Span,
    ) -> Value {
        // Cranelift can rematerialize dependent scalar operands on both guard
        // paths. Bound expansion across the entire body, not merely each site:
        // chains of C calls must not produce O(C²) machine code. The common
        // one-site hot loop still benefits; later sites retain one shared call.
        if self.indirect_leaf_promoted
            || self.indirect_leaves.0.is_empty()
            || params.len() > 8
            || !matches!(result, Type::I64 | Type::F64 | Type::Bool)
            || params
                .iter()
                .any(|ty| !matches!(ty, Type::I64 | Type::F64 | Type::Bool))
        {
            return self.emit_lir_indirect_call(signature, target, args, name, span, None);
        }
        let key = Type::Fn(params.to_vec(), Box::new(result.clone()));
        let leaf = self
            .indirect_leaves
            .0
            .get(&key)
            .and_then(Option::as_ref)
            .cloned();
        let Some(leaf) = leaf else {
            return self.emit_lir_indirect_call(signature, target, args, name, span, None);
        };
        let fref = self
            .module
            .declare_func_in_func(leaf.target, self.builder.func);
        let target_sig = self.builder.func.dfg.ext_funcs[fref].signature;
        if self.builder.func.dfg.signatures[target_sig]
            != self.builder.func.dfg.signatures[signature]
        {
            return self.emit_lir_indirect_call(signature, target, args, name, span, None);
        }
        self.indirect_leaf_promoted = true;
        if std::env::var_os("WILLOW_PANIC_DEPTH_LOAD_LOG").is_some() {
            eprintln!("[indirect-leaf-expand]");
        }
        let addr = self
            .builder
            .ins()
            .func_addr(reference_type(self.module.target_config()), fref);
        let matches = self.builder.ins().icmp(IntCC::Equal, target, addr);
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
                        UnaryOp::Not if *ty == Type::I64 => self.builder.ins().bnot(v),
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
                    self.emit_lir_binop(op, l, r, *operand_ty == Type::F64)
                }
                _ => unreachable!("validated scalar leaf operation"),
            };
            locals[id.0 as usize] = Some(value);
        }
        let value = self.scalar_leaf_operand(&leaf.result, &locals);
        self.builder.ins().jump(merge, &[value.into()]);
        self.builder.switch_to_block(fallback);
        self.builder.seal_block(fallback);
        let value = self.emit_lir_indirect_call(signature, target, args, name, span, None);
        self.builder.ins().jump(merge, &[value.into()]);
        self.builder.switch_to_block(merge);
        self.builder.seal_block(merge);
        merged
    }
}
