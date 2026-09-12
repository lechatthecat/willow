//! Bounded expansion of small, pure self-recursive scalar functions.
//! Calls still execute in source order; no memoization or reassociation occurs.
use std::collections::{HashMap, HashSet};

use crate::ir::lowered::{
    BlockId, LirLocalId, LirOperand, LirRvalue, SourceBlock, SourceFunction, SourceInst,
    SourceTerminator,
};
use crate::ir::typed_ast::{HirExpr, HirExprKind};
use crate::parser::ast::BinOp;
use crate::semantic::ids::SemanticType as Type;

pub(crate) fn inline_scalar_recursion(function: &mut SourceFunction) {
    const MAX_DEPTH: u32 = 3;
    const MAX_INSTRUCTIONS: usize = 256;
    const MAX_BLOCKS: usize = 64;
    let scalar = |ty: &Type| matches!(ty, Type::I64 | Type::F64 | Type::Bool);
    if function.is_async
        || !function.captures.is_empty()
        || !scalar(&function.return_type)
        || function.params.iter().any(|p| p.by_reference)
        || function.locals.len() > 32
        || function
            .locals
            .iter()
            .any(|l| l.is_gc_owner() || !scalar(&l.ty))
        || function.blocks.is_empty()
        || function.blocks.len() > 6
    {
        return;
    }
    let leaf = |e: &HirExpr| {
        scalar(&e.ty)
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
            return;
        }
        for inst in &block.instrs {
            instructions += 1;
            match inst {
                SourceInst::Compute { value, .. } => {
                    if value
                        .operands()
                        .iter()
                        .any(|op| matches!(op, LirOperand::Reference { .. }))
                    {
                        return;
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
                        _ => return,
                    }
                }
                SourceInst::ClearScopeRoots { .. } => {} // all locals proved scalar
                _ => return,
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
            _ => return,
        }
        for &target in &edges[block.id.0] {
            incoming[target] += 1;
        }
    }
    if !(1..=2).contains(&calls) || instructions > 16 {
        return;
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
        return;
    }
    let depth = (1..=MAX_DEPTH)
        .take_while(|&depth| {
            let copies: usize = (1..=depth).map(|level| calls.pow(level)).sum();
            instructions + copies * (instructions + function.params.len() + returns)
                <= MAX_INSTRUCTIONS
                && function.blocks.len() + copies * (function.blocks.len() + 1) <= MAX_BLOCKS
        })
        .last()
        .unwrap_or(0);
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
                _ => operand.clone(),
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
                        _ => unreachable!("candidate operations validated"),
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
                        let operand = match &value.kind {
                            HirExprKind::Var(name) => {
                                LirOperand::Local(remap_id(names[name.as_str()]))
                            }
                            HirExprKind::Int(n) => LirOperand::Int(*n),
                            HirExprKind::Float(n) => LirOperand::Float(*n),
                            HirExprKind::Bool(b) => LirOperand::Bool(*b),
                            _ => unreachable!(),
                        };
                        instrs.push(SourceInst::Compute {
                            local: destination,
                            value: LirRvalue::Use(operand),
                            span: value.span,
                        });
                        SourceTerminator::Jump(continuation)
                    }
                    _ => unreachable!("candidate terminators validated"),
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
