//! Constant-stack reductions over enum traversals.
//!
//! A single self call followed only by an unchecked i64 addition and return
//! forwarding can carry that addition in an accumulator. Modulo 2^64 addition
//! is associative and commutative; checked arithmetic and floats are not.
//! Keep this proof narrow: an acyclic, read-only enum-matching body, no other
//! calls, references, cleanup, allocation or faulting operations. Prefixes still
//! execute in source order. The new backedge polls on every traversal step and
//! ordinary LIR storage keeps the current parameters rooted across that poll.
//!
//! One body scan, one CFG traversal and one return-suffix walk (there is exactly
//! one recursive site): O(blocks + instructions + locals + operand occurrences).
//! Added IR is O(parameters + locals + returns); neither code nor roots multiply
//! with recursion depth. Runtime retains one native frame and O(1) root slots
//! with respect to traversal depth, without a heap continuation stack.
use crate::diagnostics::Span;
use crate::driver::OverflowChecks;
use crate::ir::lowered::{
    BlockId, LirLocal, LirLocalId, LirOperand, LirRvalue, LirStorageKind, SourceBlock,
    SourceFunction, SourceInst, SourceTerminator,
};
use crate::ir::typed_ast::{HirExpr, HirExprKind, HirPattern};
use crate::parser::ast::BinOp;
use crate::semantic::ids::SemanticType as Type;
use std::collections::{HashMap, HashSet};

struct Reduction {
    block: BlockId,
    at: usize,
    args: Vec<LirOperand>,
    span: Span,
    addend: LirOperand,
}

fn operand(expr: &HirExpr, names: &HashMap<String, LirLocalId>) -> Option<LirOperand> {
    LirOperand::from_flat_expr(expr, |name| names.get(name).copied())
        .filter(LirOperand::is_plain_value)
}

fn pattern_is_read_only(pattern: &HirPattern) -> bool {
    matches!(
        pattern,
        HirPattern::EnumVariant { .. }
            | HirPattern::EnumVariantTuple { .. }
            | HirPattern::Wildcard
            | HirPattern::Binding { .. }
            | HirPattern::LiteralBool(_)
            | HirPattern::LiteralInt(_)
    )
}

/// Validate the entire function before changing any instruction.
fn candidate(
    f: &SourceFunction,
    checks: OverflowChecks,
    names: &HashMap<String, LirLocalId>,
) -> Option<Reduction> {
    let mut site = None;
    let mut enum_binding = false;
    let mut incoming = vec![0usize; f.blocks.len()];
    let mut edges = vec![Vec::new(); f.blocks.len()];
    for block in &f.blocks {
        if !block.recovery.is_empty() {
            return None;
        }
        match &block.terminator {
            SourceTerminator::Jump(target) => edges[block.id.0].push(target.0),
            SourceTerminator::Branch {
                cond,
                then_block,
                else_block,
            } => {
                operand(cond, names)?;
                edges[block.id.0].extend([then_block.0, else_block.0]);
            }
            SourceTerminator::Return(Some(value)) => {
                operand(value, names)?;
            }
            // Exhaustive match statements can leave an unreachable fallthrough
            // return. The synchronous i64 ABI's neutral return is zero.
            SourceTerminator::Return(None) => {}
            _ => return None,
        }
        for &target in &edges[block.id.0] {
            incoming[target] += 1;
        }
        for (at, inst) in block.instrs.iter().enumerate() {
            match inst {
                SourceInst::Compute { local, value, span } => {
                    if value.operands().iter().any(|o| !o.is_plain_value())
                        || value.has_overflow_check_with(checks)
                    {
                        return None;
                    }
                    match value {
                        LirRvalue::Use(_) => {}
                        LirRvalue::Unary { ty, .. }
                            if ty.scalar_layout().is_some() || *ty == Type::Bool => {}
                        LirRvalue::Binary { op, operand_ty, .. }
                            if (operand_ty.scalar_layout().is_some()
                                || *operand_ty == Type::Bool)
                                && !matches!(op, BinOp::Div | BinOp::Rem | BinOp::Pow) => {}
                        LirRvalue::DirectCall {
                            callee,
                            args,
                            params,
                            result,
                        } if *callee == f.name
                            && args.len() == f.params.len()
                            && *result == Type::I64
                            && params.iter().eq(f.params.iter().map(|p| &p.ty)) =>
                        {
                            if site.is_some() {
                                return None;
                            }
                            site = Some((block.id, at, *local, args.clone(), *span));
                        }
                        _ => return None,
                    }
                }
                SourceInst::Let { value, ty, .. } if &value.ty == ty => {
                    operand(value, names)?;
                }
                SourceInst::Assign { local, value, .. }
                    if value.ty == f.locals[local.0 as usize].ty =>
                {
                    operand(value, names)?;
                }
                SourceInst::MatchTest { pattern, .. } | SourceInst::MatchBind { pattern, .. }
                    if pattern_is_read_only(pattern) =>
                {
                    enum_binding |= matches!(
                        inst,
                        SourceInst::MatchBind {
                            pattern: HirPattern::EnumVariantTuple { .. },
                            ..
                        }
                    );
                }
                SourceInst::ClearScopeRoots { .. } => {}
                _ => return None,
            }
        }
    }
    if !enum_binding || incoming[0] != 0 {
        return None;
    }
    // An iterative topological walk rejects loops and bounds the suffix walk.
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
    if visited != f.blocks.len() {
        return None;
    }
    let (block, at, result, args, span) = site?;
    let addend = return_addend(f, block, at, result, names)?;
    Some(Reduction {
        block,
        at,
        args,
        span,
        addend,
    })
}

#[derive(Clone)]
enum SuffixValue {
    // An operand's value at the call site, before any suffix assignments.
    Prefix(LirOperand),
    Recursive,
    Sum,
}

fn return_addend(
    f: &SourceFunction,
    block: BlockId,
    at: usize,
    result: LirLocalId,
    names: &HashMap<String, LirLocalId>,
) -> Option<LirOperand> {
    let mut current = block;
    let mut start = at + 1;
    let mut addend = None;
    let mut values = HashMap::from([(result, SuffixValue::Recursive)]);
    loop {
        for inst in &f.blocks[current.0].instrs[start..] {
            let read = |operand: &LirOperand| match operand {
                LirOperand::Local(id) => values
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| SuffixValue::Prefix(operand.clone())),
                _ => SuffixValue::Prefix(operand.clone()),
            };
            let (local, value) = match inst {
                SourceInst::ClearScopeRoots { .. } => continue,
                SourceInst::Compute {
                    local,
                    value: LirRvalue::Use(value),
                    ..
                } => (*local, read(value)),
                SourceInst::Let { local, value, .. } | SourceInst::Assign { local, value, .. } => {
                    (*local, read(&operand(value, names)?))
                }
                SourceInst::Compute {
                    local,
                    value:
                        LirRvalue::Binary {
                            op: BinOp::Add,
                            lhs,
                            rhs,
                            operand_ty: Type::I64,
                        },
                    ..
                } if addend.is_none() => {
                    let other = match (read(lhs), read(rhs)) {
                        (SuffixValue::Recursive, SuffixValue::Prefix(other))
                        | (SuffixValue::Prefix(other), SuffixValue::Recursive) => other,
                        _ => return None,
                    };
                    addend = Some(other);
                    (*local, SuffixValue::Sum)
                }
                _ => return None,
            };
            // Only scalar copies may move from after the call to before it.
            if f.locals[local.0 as usize].ty != Type::I64 {
                return None;
            }
            values.insert(local, value);
        }
        match &f.blocks[current.0].terminator {
            SourceTerminator::Jump(target) => {
                current = *target;
                start = 0;
            }
            SourceTerminator::Return(Some(value)) => {
                let LirOperand::Local(id) = operand(value, names)? else {
                    return None;
                };
                return matches!(values.get(&id), Some(SuffixValue::Sum)).then_some(addend?);
            }
            _ => return None,
        }
    }
}

fn new_local(
    f: &mut SourceFunction,
    names: &mut HashSet<String>,
    ty: Type,
    span: Span,
) -> LirLocalId {
    let id = LirLocalId(f.locals.len() as u32);
    let mut name = format!("__lir_reduce_{}", id.0);
    while !names.insert(name.clone()) {
        name.push('_');
    }
    f.locals.push(LirLocal {
        id,
        name,
        ty,
        source_span: Some(span),
        synthetic: true,
        parameter: false,
        storage_kind: LirStorageKind::Value,
    });
    id
}

fn compute(local: LirLocalId, value: LirRvalue, span: Span) -> SourceInst {
    SourceInst::Compute { local, value, span }
}
fn add(lhs: LirOperand, rhs: LirOperand) -> LirRvalue {
    LirRvalue::Binary {
        op: BinOp::Add,
        lhs,
        rhs,
        operand_ty: Type::I64,
    }
}

pub(crate) fn reduce_enum_recursion(f: &mut SourceFunction, checks: OverflowChecks) {
    if f.is_async
        || !f.captures.is_empty()
        || f.return_type != Type::I64
        || checks.i64_arithmetic
        || f.params.iter().any(|p| p.by_reference)
        || f.locals.iter().any(LirLocal::is_gc_owner)
        || f.blocks.is_empty()
    {
        return;
    }

    let names: HashMap<_, _> = f.locals.iter().map(|l| (l.name.clone(), l.id)).collect();
    let Some(plan) = candidate(f, checks, &names) else {
        return;
    };
    let Some(params) = f
        .params
        .iter()
        .map(|p| names.get(&p.name).copied())
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    let mut used_names = names.keys().cloned().collect();
    let acc = new_local(f, &mut used_names, Type::I64, plan.span);
    let header = BlockId(f.blocks.len());
    let mut step = vec![compute(
        acc,
        add(LirOperand::Local(acc), plan.addend),
        plan.span,
    )];
    // Lowering normally already snapshots arguments into non-parameter locals.
    // Reuse those snapshots. Only direct parameter operands need another
    // parallel-copy group, e.g. a swap; redundant managed copies cost root slots.
    let needs_copies = plan.args.iter().any(|arg| {
        arg.locals()
            .iter()
            .any(|id| f.locals[id.0 as usize].parameter)
    });
    let mut copies = Vec::new();
    for (arg, param) in plan.args.into_iter().zip(&params) {
        if needs_copies {
            let id = new_local(
                f,
                &mut used_names,
                f.locals[param.0 as usize].ty.clone(),
                plan.span,
            );
            step.push(compute(id, LirRvalue::Use(arg), plan.span));
            copies.push(LirOperand::Local(id));
        } else {
            copies.push(arg);
        }
    }
    for (&param, copy) in params.iter().zip(copies) {
        step.push(compute(param, LirRvalue::Use(copy), plan.span));
    }
    // End the previous invocation's roots before the next header poll. The
    // copied parameters are now the owners; no safepoint occurs during copying.
    step.push(SourceInst::ClearScopeRoots {
        locals: f
            .locals
            .iter()
            .filter(|l| !l.parameter)
            .map(|l| l.id)
            .collect(),
    });
    f.blocks[plan.block.0].instrs.truncate(plan.at);
    f.blocks[plan.block.0].instrs.extend(step);
    f.blocks[plan.block.0].terminator = SourceTerminator::Jump(header);
    // Every remaining return is a base path (the recursive suffix is bypassed).
    // Shared match joins need the addition too; this does not clone their CFG.
    for index in 0..f.blocks.len() {
        let SourceTerminator::Return(value) = &f.blocks[index].terminator else {
            continue;
        };
        let value = value.as_ref().map_or(LirOperand::Int(0), |value| {
            operand(value, &names).expect("validated return")
        });
        let result = new_local(f, &mut used_names, Type::I64, plan.span);
        f.blocks[index].instrs.push(compute(
            result,
            add(LirOperand::Local(acc), value),
            plan.span,
        ));
        f.blocks[index].terminator = SourceTerminator::Return(Some(HirExpr {
            kind: HirExprKind::Var(f.locals[result.0 as usize].name.clone()),
            ty: Type::I64,
            span: plan.span,
        }));
    }
    // The acyclic original entry has no reachable incoming edges. Replacing it
    // preserves ABI initialization and initializes the accumulator exactly once.
    let mut body = std::mem::replace(
        &mut f.blocks[0],
        SourceBlock {
            id: BlockId(0),
            instrs: vec![compute(acc, LirRvalue::Use(LirOperand::Int(0)), plan.span)],
            terminator: SourceTerminator::Jump(header),
            recovery: vec![],
        },
    );
    body.id = header;
    f.blocks.push(body);
}

#[cfg(test)]
mod tests;
