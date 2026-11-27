//! Bounded expansion of small, pure self-recursive scalar functions.
//! Calls still execute in source order; no memoization or reassociation occurs.
use crate::driver::OverflowChecks;
use std::collections::{HashMap, HashSet};

use crate::ir::lowered::{
    BlockId, LirLocalId, LirOperand, LirRvalue, SourceBlock, SourceFunction, SourceInst,
    SourceTerminator,
};
use crate::ir::typed_ast::{HirExpr, HirExprKind};
use crate::parser::ast::BinOp;

/// With arithmetic checks enabled, checked integer arithmetic is a
/// fault whose call stack must keep every recursive frame.
pub(crate) fn inline_scalar_recursion(
    function: &mut SourceFunction,
    overflow_checks: OverflowChecks,
) {
    let depth = scalar_recursion_depth(function, overflow_checks);
    if depth == 0 {
        return;
    }
    let original = function.clone();
    let names: HashMap<_, _> = original
        .locals
        .iter()
        .map(|l| (l.name.as_str(), l.id))
        .collect();
    let Some(params) = original
        .params
        .iter()
        .map(|p| names.get(p.name.as_str()).copied())
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    let mut used_names: HashSet<_> = original.locals.iter().map(|l| l.name.clone()).collect();
    for _ in 0..depth {
        let mut sites = Vec::new();
        for block in &function.blocks {
            for (index, inst) in block.instrs.iter().enumerate() {
                if matches!(inst, SourceInst::Compute { value: LirRvalue::DirectCall { callee, .. }, .. } if *callee == function.name)
                {
                    sites.push((block.id, index));
                }
            }
        }
        // Splitting later calls first preserves the earlier instruction indices.
        for (block_id, index) in sites.into_iter().rev() {
            let local_base = function.locals.len() as u32;
            let remap_id = |id: LirLocalId| LirLocalId(local_base + id.0);
            for local in &original.locals {
                let mut copy = local.clone();
                copy.id = remap_id(local.id);
                copy.name = format!("__lir_recursive_{}", copy.id.0);
                while !used_names.insert(copy.name.clone()) {
                    copy.name.push('_');
                }
                copy.parameter = false;
                copy.synthetic = true;
                function.locals.push(copy);
            }
            let remap_operand = |operand: &LirOperand| match operand {
                LirOperand::Local(id) => LirOperand::Local(remap_id(*id)),
                LirOperand::Int(_)
                | LirOperand::Integer { .. }
                | LirOperand::Float(_)
                | LirOperand::Bool(_)
                | LirOperand::Reference { .. } => operand.clone(),
            };
            let remap_expr = |expr: &HirExpr| {
                let mut copy = expr.clone();
                if let HirExprKind::Var(name) = &mut copy.kind {
                    *name = function.locals[remap_id(names[name.as_str()]).0 as usize]
                        .name
                        .clone();
                }
                copy
            };
            let continuation = BlockId(function.blocks.len());
            let first = BlockId(continuation.0 + 1);
            let remap_block = |id: BlockId| BlockId(first.0 + id.0);
            let block = &mut function.blocks[block_id.0];
            let suffix = block.instrs.split_off(index + 1);
            let SourceInst::Compute {
                local: destination,
                value: LirRvalue::DirectCall { args, .. },
                span,
            } = block.instrs.pop().unwrap()
            else {
                unreachable!()
            };
            let terminator =
                std::mem::replace(&mut block.terminator, SourceTerminator::Jump(first));
            for (param, arg) in params.iter().zip(args) {
                block.instrs.push(SourceInst::Compute {
                    local: remap_id(*param),
                    value: LirRvalue::Use(arg),
                    span,
                });
            }
            function.blocks.push(SourceBlock {
                id: continuation,
                instrs: suffix,
                terminator,
                recovery: vec![],
            });
            for source in &original.blocks {
                let mut instrs = Vec::new();
                for inst in &source.instrs {
                    let SourceInst::Compute { local, value, span } = inst else {
                        continue;
                    };
                    let value = match value {
                        LirRvalue::Use(op) => LirRvalue::Use(remap_operand(op)),
                        LirRvalue::Unary { op, operand, ty } => LirRvalue::Unary {
                            op: op.clone(),
                            operand: remap_operand(operand),
                            ty: ty.clone(),
                        },
                        LirRvalue::Binary {
                            op,
                            lhs,
                            rhs,
                            operand_ty,
                        } => LirRvalue::Binary {
                            op: op.clone(),
                            lhs: remap_operand(lhs),
                            rhs: remap_operand(rhs),
                            operand_ty: operand_ty.clone(),
                        },
                        LirRvalue::DirectCall {
                            callee,
                            args,
                            params,
                            result,
                        } => LirRvalue::DirectCall {
                            callee: *callee,
                            args: args.iter().map(&remap_operand).collect(),
                            params: params.clone(),
                            result: result.clone(),
                        },
                        ir_variants!(
                            LirRvalue;
                            BeginReferenceCall, AwaitFuture, StartTask, SelectIdleWait, Coerce,
                            ArrayAlloc, CaptureArrayOwner, ArrayStore, Index, ObjectAlloc,
                            FieldLoad, FieldStore, StaticField, StaticStore, StaticCall,
                            EnumAlloc, EnumPayloadStore, ConstructorCall, Range, EnumMethod,
                            BuiltinCall, FormatScalar, Panic, Recover, ReferenceDebug,
                            RebindResultError, IntoError, PrepareMethod, MethodCall,
                            StringLiteral, FunctionRef, Closure, Print, IntrinsicCall,
                            IndirectCall, StringConcat
                        ) => unreachable!("candidate operations validated"),
                    };
                    instrs.push(SourceInst::Compute {
                        local: remap_id(*local),
                        value,
                        span: *span,
                    });
                }
                let terminator = match &source.terminator {
                    SourceTerminator::Jump(id) => SourceTerminator::Jump(remap_block(*id)),
                    SourceTerminator::Branch {
                        cond,
                        then_block,
                        else_block,
                    } => SourceTerminator::Branch {
                        cond: remap_expr(cond),
                        then_block: remap_block(*then_block),
                        else_block: remap_block(*else_block),
                    },
                    SourceTerminator::Return(Some(value)) => {
                        let operand =
                            LirOperand::from_flat_expr(value, |name| Some(remap_id(names[name])))
                                .unwrap_or_else(|| unreachable!());
                        instrs.push(SourceInst::Compute {
                            local: destination,
                            value: LirRvalue::Use(operand),
                            span: value.span,
                        });
                        SourceTerminator::Jump(continuation)
                    }
                    SourceTerminator::Suspend { .. }
                    | SourceTerminator::Return(_)
                    | SourceTerminator::CleanupReturn => {
                        unreachable!("candidate terminators validated")
                    }
                };
                function.blocks.push(SourceBlock {
                    id: remap_block(source.id),
                    instrs,
                    terminator,
                    recovery: vec![],
                });
            }
        }
    }
}

fn scalar_recursion_depth(function: &SourceFunction, overflow_checks: OverflowChecks) -> u32 {
    const MAX_DEPTH: u32 = 3;
    const MAX_INSTRUCTIONS: usize = 256;
    const MAX_BLOCKS: usize = 64;
    if function.is_async
        || !function.captures.is_empty()
        || !super::inline_scalar_type(&function.return_type)
        || function.params.iter().any(|p| p.by_reference)
        || function.locals.len() > 32
        || function
            .locals
            .iter()
            .any(|l| l.is_gc_owner() || !super::inline_scalar_type(&l.ty))
        || function.blocks.is_empty()
        || function.blocks.len() > 6
    {
        return 0;
    }
    let leaf = |e: &HirExpr| {
        super::inline_scalar_type(&e.ty)
            && matches!(
                e.kind,
                HirExprKind::Var(_)
                    | HirExprKind::Int(_)
                    | HirExprKind::Float(_)
                    | HirExprKind::Bool(_)
            )
    };
    let mut calls = 0usize;
    let mut instructions = 0;
    let mut returns = 0;
    let mut edges = vec![Vec::new(); function.blocks.len()];
    let mut incoming = vec![0usize; function.blocks.len()];
    for block in &function.blocks {
        if !block.recovery.is_empty() {
            return 0;
        }
        for inst in &block.instrs {
            instructions += 1;
            match inst {
                SourceInst::Compute { value, .. } => {
                    if value.operands().iter().any(|op| !op.is_plain_value()) {
                        return 0;
                    }
                    if value.has_overflow_check_with(overflow_checks) {
                        return 0;
                    }
                    match value {
                        LirRvalue::Use(_) | LirRvalue::Unary { .. } => {}
                        LirRvalue::Binary { op, .. }
                            if !matches!(op, BinOp::Div | BinOp::Rem | BinOp::Pow) => {}
                        LirRvalue::DirectCall { callee, args, .. }
                            if *callee == function.name && args.len() == function.params.len() =>
                        {
                            calls += 1
                        }
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
                        ) => return 0,
                    }
                }
                SourceInst::ClearScopeRoots { .. } => {} // all locals proved scalar
                ir_variants!(
                    SourceInst;
                    EnterDeferScope, LeaveDeferScope, FlushDefers, RestoreCallPreparation, Defer, Let, Assign,
                    FieldAssign, IndexAssign, StaticFieldAssign, SuperInit, SelectInit,
                    SelectProbe, SelectPick, SelectUnregister, SelectCommit, ReleaseLock,
                    MatchTest, MatchBind, Expr
                ) => return 0,
            }
        }
        match &block.terminator {
            SourceTerminator::Jump(target) => edges[block.id.0].push(target.0),
            SourceTerminator::Branch {
                cond,
                then_block,
                else_block,
            } if leaf(cond) => {
                edges[block.id.0].extend([then_block.0, else_block.0]);
            }
            SourceTerminator::Return(Some(value)) if leaf(value) => returns += 1,
            SourceTerminator::Branch { .. }
            | SourceTerminator::Suspend { .. }
            | SourceTerminator::Return(_)
            | SourceTerminator::CleanupReturn => return 0,
        }
        for &target in &edges[block.id.0] {
            incoming[target] += 1;
        }
    }
    if !(1..=2).contains(&calls) || instructions > 16 {
        return 0;
    }
    // Acyclic intraprocedural CFG only. Remaining recursive calls provide the
    // safepoints; at most four source invocation levels are combined per expansion.
    let mut ready: Vec<_> = incoming
        .iter()
        .enumerate()
        .filter_map(|(i, &n)| (n == 0).then_some(i))
        .collect();
    let mut visited = 0;
    while let Some(block) = ready.pop() {
        visited += 1;
        for &target in &edges[block] {
            incoming[target] -= 1;
            if incoming[target] == 0 {
                ready.push(target);
            }
        }
    }
    if visited != function.blocks.len() {
        return 0;
    }
    (1..=MAX_DEPTH)
        .take_while(|&depth| {
            let copies: usize = (1..=depth).map(|level| calls.pow(level)).sum();
            instructions + copies * (instructions + function.params.len() + returns)
                <= MAX_INSTRUCTIONS
                && function.blocks.len() + copies * (function.blocks.len() + 1) <= MAX_BLOCKS
        })
        .last()
        .unwrap_or(0)
}
