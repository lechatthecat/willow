//! Backend-independent, stack-bounded constant folding for lowered bodies.
//!
//! Only pure scalar subtrees are replaced. Faulting arithmetic stays in the IR
//! so runtime panic/recovery behavior and source locations remain intact.
use std::collections::HashMap;

use super::lowered::{SourceBlock, SourceInst, SourceTerminator};
use super::typed_ast::{HirExpr, HirExprKind};
use crate::parser::ast::{BinOp, UnaryOp};
use crate::semantic::ids::SemanticType as Type;

#[derive(Clone, Copy)]
enum Constant {
    Int(i64),
    Bool(bool),
}

impl Constant {
    fn kind(self) -> HirExprKind {
        match self {
            Self::Int(value) => HirExprKind::Int(value),
            Self::Bool(value) => HirExprKind::Bool(value),
        }
    }
}

fn binary(op: &BinOp, lhs: Constant, rhs: Constant) -> Option<Constant> {
    use BinOp::*;
    use Constant::*;
    Some(match (lhs, rhs) {
        (Int(a), Int(b)) => match op {
            Add => Int(a.wrapping_add(b)),
            Sub => Int(a.wrapping_sub(b)),
            Mul => Int(a.wrapping_mul(b)),
            Div => Int(a.checked_div(b)?),
            Rem => Int(a.checked_rem(b)?),
            Pow if b >= 0 => {
                // Bounded by the exponent's bit width, including i64::MAX.
                let (mut base, mut exponent, mut result) = (a, b as u64, 1i64);
                while exponent != 0 {
                    if exponent & 1 != 0 {
                        result = result.wrapping_mul(base);
                    }
                    exponent >>= 1;
                    base = base.wrapping_mul(base);
                }
                Int(result)
            }
            Eq => Bool(a == b),
            Ne => Bool(a != b),
            Lt => Bool(a < b),
            Le => Bool(a <= b),
            Gt => Bool(a > b),
            Ge => Bool(a >= b),
            _ => return None,
        },
        (Bool(a), Bool(b)) => match op {
            And => Bool(a && b),
            Or => Bool(a || b),
            Eq => Bool(a == b),
            Ne => Bool(a != b),
            _ => return None,
        },
        _ => return None,
    })
}

/// Compute facts in postorder, then replace maximal constant subtrees in
/// preorder. Addresses are opaque keys only: no raw pointer is dereferenced,
/// and replacing a parent prunes every child whose address it invalidates.
/// Both traversals use explicit worklists, including match/defer/lambda bodies.
pub(crate) fn fold_expr(expr: &mut HirExpr) {
    let mut constants = HashMap::new();
    for node in expr.walk_postorder(true) {
        let get = |child: &HirExpr| constants.get(&std::ptr::from_ref(child)).copied();
        let value = match &node.kind {
            HirExprKind::Int(n) if node.ty == Type::I64 => Some(Constant::Int(*n)),
            HirExprKind::Bool(b) => Some(Constant::Bool(*b)),
            HirExprKind::Unary { op, operand } => match (op, get(operand)) {
                (UnaryOp::Neg, Some(Constant::Int(n))) => Some(Constant::Int(n.wrapping_neg())),
                (UnaryOp::Not, Some(Constant::Bool(b))) => Some(Constant::Bool(!b)),
                _ => None,
            },
            HirExprKind::Binary { op, lhs, rhs } => match (get(lhs), get(rhs)) {
                (Some(a), Some(b)) => binary(op, a, b),
                _ => None,
            },
            // Preserve conditional subtrees, calls, allocation, and reads.
            // No algebraic identities such as effectful_call() * 0 => 0.
            _ => None,
        };
        if let Some(value) = value {
            constants.insert(std::ptr::from_ref(node), value);
        }
    }
    expr.visit_mut_preorder(true, |node| {
        if let Some(value) = constants.get(&std::ptr::from_ref(node)) {
            node.kind = value.kind();
            false
        } else {
            true
        }
    });
}

/// Facts never cross block boundaries or an operation that may mutate locals.
/// Only complete scalar expression trees qualify: reference/address operands,
/// calls, closures, and scoped expressions are deliberately opaque.
fn propagate_scalar(expr: &mut HirExpr, facts: &mut HashMap<String, Constant>) {
    let scalar = expr.walk_postorder(false).all(|node| {
        matches!(node.ty, Type::I64 | Type::Bool)
            && matches!(
                node.kind,
                HirExprKind::Int(_)
                    | HirExprKind::Bool(_)
                    | HirExprKind::Var(_)
                    | HirExprKind::Unary { .. }
                    | HirExprKind::Binary { .. }
            )
    });
    if scalar {
        expr.visit_mut_preorder(false, |node| {
            if let HirExprKind::Var(name) = &node.kind
                && let Some(value) = facts.get(name)
            {
                node.kind = value.kind();
            }
            true
        });
    } else {
        facts.clear();
    }
    fold_expr(expr);
}

pub(crate) fn fold_blocks(blocks: &mut [SourceBlock]) {
    let mut regions = vec![blocks];
    while let Some(blocks) = regions.pop() {
        for block in blocks {
            let mut facts = HashMap::new();
            for inst in &mut block.instrs {
                let assigns = matches!(inst, SourceInst::Assign { .. });
                match inst {
                    SourceInst::Let { name, value, .. }
                    | SourceInst::Assign { name, value, .. } => {
                        propagate_scalar(value, &mut facts);
                        // Reference parameters can alias. An assignment may
                        // change another named local through the same address.
                        if assigns {
                            facts.clear();
                        }
                        facts.remove(name);
                        let constant = match value.kind {
                            HirExprKind::Int(n) if value.ty == Type::I64 => Some(Constant::Int(n)),
                            HirExprKind::Bool(value) => Some(Constant::Bool(value)),
                            _ => None,
                        };
                        if let Some(constant) = constant {
                            facts.insert(name.clone(), constant);
                        }
                    }
                    SourceInst::Expr(value) => propagate_scalar(value, &mut facts),
                    _ => facts.clear(),
                }
                match inst {
                    SourceInst::Compute { .. } => {}
                    SourceInst::Let { .. } | SourceInst::Assign { .. } | SourceInst::Expr(_) => {}
                    SourceInst::StaticFieldAssign { value, .. } => fold_expr(value),
                    SourceInst::FieldAssign { object, value, .. } => {
                        fold_expr(object);
                        fold_expr(value);
                    }
                    SourceInst::IndexAssign {
                        array,
                        index,
                        value,
                    } => {
                        fold_expr(array);
                        fold_expr(index);
                        fold_expr(value);
                    }
                    SourceInst::SuperInit { args, .. } => {
                        for arg in args {
                            fold_expr(arg);
                        }
                    }
                    SourceInst::Defer { body, .. } => {
                        regions.push(body.function.blocks.as_mut_slice())
                    }
                    SourceInst::EnterDeferScope { .. }
                    | SourceInst::LeaveDeferScope { .. }
                    | SourceInst::FlushDefers { .. }
                    | SourceInst::ClearScopeRoots { .. }
                    | SourceInst::ReleaseLock(_)
                    | SourceInst::MatchTest { .. }
                    | SourceInst::MatchBind { .. }
                    | SourceInst::SelectInit { .. }
                    | SourceInst::SelectProbe { .. }
                    | SourceInst::SelectPick { .. }
                    | SourceInst::SelectUnregister { .. }
                    | SourceInst::SelectCommit { .. } => {}
                }
            }
            match &mut block.terminator {
                SourceTerminator::Branch { cond, .. } => propagate_scalar(cond, &mut facts),
                SourceTerminator::Return(Some(value)) => propagate_scalar(value, &mut facts),
                SourceTerminator::Jump(_)
                | SourceTerminator::Suspend { .. }
                | SourceTerminator::Return(None)
                | SourceTerminator::CleanupReturn => {}
            }
        }
    }
}

/// Remove unused, non-faulting flat definitions by tracing dependencies from
/// effectful instructions and terminators. Each candidate must define a fresh
/// compiler temporary exactly once; aliased source assignments are never DCE'd.
pub(crate) fn eliminate_dead_values(
    blocks: &mut [SourceBlock],
    locals: &[super::lowered::LirLocal],
) {
    // Effects live in Compute. Discarding its typed temporary is inert even
    // for Void/Never; keep unknown or retyped reads for validation to reject.
    let synthetic: HashMap<_, _> = locals
        .iter()
        .filter(|local| local.synthetic)
        .map(|local| (local.name.as_str(), &local.ty))
        .collect();
    for block in blocks.iter_mut() {
        block.instrs.retain(|inst| !matches!(inst, SourceInst::Expr(HirExpr { kind: HirExprKind::Var(name), ty, .. }) if synthetic.get(name.as_str()).is_some_and(|declared| *declared == ty)));
    }
    scalar_replace_objects(blocks, locals);
    use super::lowered::async_liveness::{instruction_use_def, terminator_uses};
    use super::lowered::{LirLocalId, LirOperand, LirRvalue};
    use std::collections::HashSet;
    let names = locals
        .iter()
        .map(|local| (local.name.as_str(), local.id))
        .collect();
    let pure = |value: &LirRvalue| match value {
        LirRvalue::Use(_) | LirRvalue::Unary { .. } => true,
        LirRvalue::FunctionRef { .. } => true,
        LirRvalue::Binary {
            operand_ty: Type::String,
            ..
        } => false,
        LirRvalue::Binary {
            op,
            rhs,
            operand_ty,
            ..
        } => match op {
            BinOp::Div | BinOp::Rem => *operand_ty == Type::F64,
            BinOp::Pow => matches!(rhs, LirOperand::Int(exponent) if *exponent >= 0),
            _ => true,
        },
        // New operations remain effectful until their contract is proven.
        _ => false,
    };
    let mut definitions = HashMap::<LirLocalId, usize>::new();
    let mut dependencies = HashMap::new();
    let mut ignored = HashSet::new();
    let mut defs = HashSet::new();
    for block in blocks.iter() {
        for inst in &block.instrs {
            defs.clear();
            instruction_use_def(inst, &names, &mut ignored, &mut defs);
            for &local in &defs {
                *definitions.entry(local).or_default() += 1;
            }
            if let SourceInst::Compute { local, value, .. } = inst
                && locals[local.0 as usize].synthetic
                && pure(value)
            {
                dependencies.insert(
                    *local,
                    value
                        .operands()
                        .into_iter()
                        .filter_map(|operand| match operand {
                            LirOperand::Local(id) => Some(*id),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                );
            }
        }
    }
    dependencies.retain(|local, _| definitions.get(local) == Some(&1));
    let mut live = HashSet::new();
    for block in blocks.iter_mut() {
        // Reading a scalar value just to discard it has no runtime effect.
        // A faulting producer remains an effectful instruction below.
        block.instrs.retain(|inst| !matches!(inst, SourceInst::Expr(expr)
            if matches!(expr.ty, Type::I64 | Type::F64 | Type::Bool)
                && matches!(expr.kind, HirExprKind::Int(_) | HirExprKind::Float(_) | HirExprKind::Bool(_) | HirExprKind::Var(_))));
        for inst in &block.instrs {
            if matches!(inst, SourceInst::Compute { local, .. } if dependencies.contains_key(local))
            {
                continue;
            }
            defs.clear();
            instruction_use_def(inst, &names, &mut live, &mut defs);
        }
        terminator_uses(&block.terminator, &names, &mut live, &HashSet::new());
    }
    let mut pending: Vec<_> = live.iter().copied().collect();
    while let Some(local) = pending.pop() {
        if let Some(inputs) = dependencies.get(&local) {
            for &input in inputs {
                if live.insert(input) {
                    pending.push(input);
                }
            }
        }
    }
    for block in blocks {
        block.instrs.retain(|inst| !matches!(inst,
            SourceInst::Compute { local, .. } if dependencies.contains_key(local) && !live.contains(local)));
    }
}

/// Fold constant branch decisions. Unreachable blocks are pruned by the
/// lowerer's existing renumbering pass, including its panic-recovery edges.
pub(crate) fn simplify_cfg(blocks: &mut [SourceBlock]) {
    for block in blocks {
        if let SourceTerminator::Branch {
            cond,
            then_block,
            else_block,
        } = &block.terminator
            && let HirExprKind::Bool(value) = cond.kind
        {
            block.terminator =
                SourceTerminator::Jump(if value { *then_block } else { *else_block });
        }
    }
}

/// Unroll small, straight-line scalar loops without changing arithmetic order.
/// Every copy retains its condition, including the final partial group. The
/// resulting cycle spans at most four original iterations between polls.
/// Calls, faults, references and cleanup protocol operations are excluded.
pub(crate) fn unroll_scalar_loops(function: &mut super::lowered::SourceFunction) {
    use super::lowered::{BlockId, LirOperand, LirRvalue};
    const FACTOR: usize = 4;
    const MAX_LOOP_INSTRUCTIONS: usize = 24;
    const MAX_ADDED_INSTRUCTIONS: usize = 192;
    if function.params.iter().any(|param| param.by_reference) {
        return;
    }
    let scalar = |ty: &Type| matches!(ty, Type::I64 | Type::F64 | Type::Bool);
    let operand = |value: &LirOperand| match value {
        LirOperand::Local(id) => function
            .locals
            .get(id.0 as usize)
            .is_some_and(|local| !local.is_gc_owner() && scalar(&local.ty)),
        LirOperand::Int(_) | LirOperand::Float(_) | LirOperand::Bool(_) => true,
        LirOperand::Reference { .. } => false,
    };
    let leaf = |value: &HirExpr| {
        scalar(&value.ty)
            && matches!(
                value.kind,
                HirExprKind::Var(_)
                    | HirExprKind::Int(_)
                    | HirExprKind::Float(_)
                    | HirExprKind::Bool(_)
            )
    };
    let instruction = |instruction: &SourceInst| match instruction {
        SourceInst::Compute { value, .. } => match value {
            LirRvalue::Use(value) => operand(value),
            LirRvalue::Unary {
                operand: value, ty, ..
            } => scalar(ty) && operand(value),
            LirRvalue::Binary {
                op,
                lhs,
                rhs,
                operand_ty,
            } => {
                scalar(operand_ty)
                    && operand(lhs)
                    && operand(rhs)
                    && !matches!(op, BinOp::Pow | BinOp::Rem | BinOp::And | BinOp::Or)
                    && (!matches!(op, BinOp::Div) || *operand_ty == Type::F64)
            }
            _ => false,
        },
        SourceInst::Let { value, ty, .. } => scalar(ty) && leaf(value),
        SourceInst::Assign { value, .. } => leaf(value),
        _ => false,
    };
    let original_len = function.blocks.len();
    let mut predecessors = vec![0usize; original_len];
    for block in &function.blocks {
        let mut edge = |target: BlockId| {
            if let Some(count) = predecessors.get_mut(target.0) {
                *count += 1;
            }
        };
        match &block.terminator {
            SourceTerminator::Jump(target) => edge(*target),
            SourceTerminator::Branch {
                then_block,
                else_block,
                ..
            } => {
                edge(*then_block);
                edge(*else_block);
            }
            SourceTerminator::Suspend { resume, .. } => edge(*resume),
            _ => {}
        }
        for target in &block.recovery {
            edge(*target);
        }
    }
    let mut candidates = Vec::new();
    let mut added = 0;
    for header in &function.blocks {
        let SourceTerminator::Branch {
            cond,
            then_block: body_id,
            else_block: exit,
        } = &header.terminator
        else {
            continue;
        };
        let Some(body) = function.blocks.get(body_id.0) else {
            continue;
        };
        let count = header.instrs.len() + body.instrs.len();
        if header.id.0 == 0
            || *body_id == header.id
            || *exit == header.id
            || *exit == *body_id
            || predecessors[body_id.0] != 1
            || !matches!(body.terminator, SourceTerminator::Jump(target) if target == header.id)
            || !header.recovery.is_empty()
            || !body.recovery.is_empty()
            || !leaf(cond)
            || cond.ty != Type::Bool
            || count == 0
            || count > MAX_LOOP_INSTRUCTIONS
            || !header.instrs.iter().chain(&body.instrs).all(&instruction)
            || added + (FACTOR - 1) * count > MAX_ADDED_INSTRUCTIONS
        {
            continue;
        }
        added += (FACTOR - 1) * count;
        candidates.push((header.id, *body_id, *exit));
    }
    for (header_id, body_id, exit) in candidates {
        let header = function.blocks[header_id.0].clone();
        let body = function.blocks[body_id.0].clone();
        let first_copy = BlockId(function.blocks.len());
        function.blocks[body_id.0].terminator = SourceTerminator::Jump(first_copy);
        for copy in 0..FACTOR - 1 {
            let mut copied_header = header.clone();
            let mut copied_body = body.clone();
            copied_header.id = BlockId(function.blocks.len());
            copied_body.id = BlockId(function.blocks.len() + 1);
            let SourceTerminator::Branch {
                then_block,
                else_block,
                ..
            } = &mut copied_header.terminator
            else {
                unreachable!()
            };
            *then_block = copied_body.id;
            *else_block = exit;
            copied_body.terminator = SourceTerminator::Jump(if copy == FACTOR - 2 {
                header_id
            } else {
                BlockId(function.blocks.len() + 2)
            });
            // LIR locals are mutable slots, not SSA definitions. Reusing the
            // original temporary IDs preserves intra-iteration dependencies;
            // Cranelift builds SSA for the expanded control-flow graph later.
            function.blocks.push(copied_header);
            function.blocks.push(copied_body);
        }
    }
}

/// Inline only bounded, single-block scalar leaves. No allocation, calls,
/// faults, loops, references, or cleanup can cross this transformation.
/// Keeping the expansion budget small also bounds work between safepoints.
pub(crate) fn inline_scalar_leaves(functions: &mut [super::lowered::SourceFunction]) {
    // Whole-program resolution flattened aggregate constructors after the
    // per-function pass. Eliminate those before selecting scalar callees.
    for function in functions.iter_mut() {
        eliminate_dead_values(&mut function.blocks, &function.locals);
    }
    use super::lowered::{LirLocalId, LirOperand, LirRvalue};
    use crate::semantic::ids::FunctionId;
    #[derive(Clone)]
    struct Leaf {
        params: Vec<LirLocalId>,
        operations: Vec<(LirLocalId, LirRvalue, Type, crate::diagnostics::Span)>,
        result: LirOperand,
    }
    fn operand(expr: &HirExpr, names: &HashMap<&str, LirLocalId>) -> Option<LirOperand> {
        Some(match &expr.kind {
            HirExprKind::Int(value) => LirOperand::Int(*value),
            HirExprKind::Float(value) => LirOperand::Float(*value),
            HirExprKind::Bool(value) => LirOperand::Bool(*value),
            HirExprKind::Var(name) => LirOperand::Local(*names.get(name.as_str())?),
            _ => return None,
        })
    }
    fn candidate(function: &super::lowered::SourceFunction) -> Option<Leaf> {
        if function.is_async
            || function.blocks.len() != 1
            || !function.captures.is_empty()
            || function.params.iter().any(|param| param.by_reference)
            || !matches!(function.return_type, Type::I64 | Type::F64 | Type::Bool)
            || function
                .locals
                .iter()
                .any(|local| !matches!(local.ty, Type::I64 | Type::F64 | Type::Bool))
        {
            return None;
        }
        let block = &function.blocks[0];
        if !block.recovery.is_empty() || block.instrs.len() > 8 {
            return None;
        }
        let names: HashMap<_, _> = function
            .locals
            .iter()
            .map(|local| (local.name.as_str(), local.id))
            .collect();
        let mut operations = Vec::new();
        for instruction in &block.instrs {
            match instruction {
                SourceInst::Compute { local, value, span } => {
                    match value {
                        LirRvalue::Use(_) | LirRvalue::Unary { .. } => {}
                        LirRvalue::Binary { op, .. }
                            if !matches!(op, BinOp::Div | BinOp::Rem | BinOp::Pow) => {}
                        _ => return None,
                    }
                    operations.push((
                        *local,
                        value.clone(),
                        function.locals[local.0 as usize].ty.clone(),
                        *span,
                    ));
                }
                SourceInst::ClearScopeRoots { .. } => {} // all locals proved scalar
                _ => return None,
            }
        }
        let SourceTerminator::Return(Some(result)) = &block.terminator else {
            return None;
        };
        Some(Leaf {
            params: function
                .params
                .iter()
                .map(|param| names.get(param.name.as_str()).copied())
                .collect::<Option<_>>()?,
            operations,
            result: operand(result, &names)?,
        })
    }
    fn remap(operand: &LirOperand, locals: &HashMap<LirLocalId, LirOperand>) -> LirOperand {
        match operand {
            LirOperand::Local(id) => locals[id].clone(),
            _ => operand.clone(),
        }
    }
    let leaves: HashMap<FunctionId, Leaf> = functions
        .iter()
        .filter_map(|function| candidate(function).map(|leaf| (function.name, leaf)))
        .collect();
    for function in functions {
        let mut names: std::collections::HashSet<_> = function
            .locals
            .iter()
            .map(|local| local.name.clone())
            .collect();
        for block in &mut function.blocks {
            let mut instructions = Vec::new();
            for instruction in std::mem::take(&mut block.instrs) {
                if let SourceInst::Compute {
                    local,
                    value: LirRvalue::DirectCall { callee, args, .. },
                    span,
                } = &instruction
                    && let Some(leaf) = leaves.get(callee)
                    && leaf.params.len() == args.len()
                {
                    let mut mapping: HashMap<_, _> = leaf
                        .params
                        .iter()
                        .copied()
                        .zip(args.iter().cloned())
                        .collect();
                    for (source, value, ty, source_span) in &leaf.operations {
                        let value = match value {
                            LirRvalue::Use(input) => LirRvalue::Use(remap(input, &mapping)),
                            LirRvalue::Unary { op, operand, ty } => LirRvalue::Unary {
                                op: op.clone(),
                                operand: remap(operand, &mapping),
                                ty: ty.clone(),
                            },
                            LirRvalue::Binary {
                                op,
                                lhs,
                                rhs,
                                operand_ty,
                            } => LirRvalue::Binary {
                                op: op.clone(),
                                lhs: remap(lhs, &mapping),
                                rhs: remap(rhs, &mapping),
                                operand_ty: operand_ty.clone(),
                            },
                            _ => unreachable!("leaf operations validated"),
                        };
                        let id = LirLocalId(function.locals.len() as u32);
                        let mut name = format!("__lir_inline_{}", id.0);
                        while !names.insert(name.clone()) {
                            name.push('_');
                        }
                        function.locals.push(super::lowered::LirLocal {
                            storage_kind: super::lowered::LirStorageKind::Value,
                            id,
                            name,
                            ty: ty.clone(),
                            source_span: None,
                            synthetic: true,
                            parameter: false,
                        });
                        instructions.push(SourceInst::Compute {
                            local: id,
                            value,
                            span: *source_span,
                        });
                        mapping.insert(*source, LirOperand::Local(id));
                    }
                    instructions.push(SourceInst::Compute {
                        local: *local,
                        value: LirRvalue::Use(remap(&leaf.result, &mapping)),
                        span: *span,
                    });
                } else {
                    instructions.push(instruction);
                }
            }
            block.instrs = instructions;
        }
        eliminate_dead_values(&mut function.blocks, &function.locals);
        if function.is_async {
            function.async_frame =
                super::lowered::async_liveness::analyze(&function.blocks, &function.locals);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Span;

    fn lowered(source: &str) -> super::super::lowered::SourceProgram {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let (hir, errors) = crate::ir::lower::lower_program(&ast);
        assert!(errors.is_empty(), "{errors:?}");
        super::super::lowered::lower_source_program(&hir)
    }

    #[test]
    fn scalar_unrolling_excludes_calls_and_faulting_arithmetic() {
        use super::super::lowered::LirRvalue;
        let program = lowered(
            r#"
fn effect(n: i64) -> i64 { print(n); return n + 1; }
fn pure(n: i64) -> i64 { let mut i = 0; while i < n { i = i + 1; } return i; }
fn faulting(n: i64) -> i64 { let mut i = 0; while i < n { i = i + 100 / n; } return i; }
fn calling(n: i64) -> i64 { let mut i = 0; while i < n { i = effect(i); } return i; }
"#,
        );
        let comparisons = |name: &str| {
            program
                .functions
                .iter()
                .find(|function| function.name.is_free_named(name))
                .unwrap()
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .filter(|instruction| {
                    matches!(
                        instruction,
                        SourceInst::Compute {
                            value: LirRvalue::Binary { op: BinOp::Lt, .. },
                            ..
                        }
                    )
                })
                .count()
        };
        assert_eq!(comparisons("pure"), 4, "pure scalar loop is expanded");
        assert_eq!(
            comparisons("faulting"),
            1,
            "integer division remains behind every poll"
        );
        assert_eq!(
            comparisons("calling"),
            1,
            "observable calls remain behind every poll"
        );
    }

    fn scalar_object_fixture() -> (Vec<SourceBlock>, Vec<super::super::lowered::LirLocal>) {
        use super::super::lowered::{
            BlockId, LirLocal, LirLocalId as Id, LirOperand as O, LirRvalue as V,
        };
        let class = crate::semantic::ids::TypeId::local("ScalarPair");
        let object_ty = Type::Named(class);
        let locals = [
            object_ty.clone(),
            object_ty.clone(),
            Type::Void,
            Type::I64,
            Type::I64,
        ]
        .into_iter()
        .enumerate()
        .map(|(id, ty)| LirLocal {
            storage_kind: super::super::lowered::LirStorageKind::Value,
            id: Id(id as u32),
            name: format!("v{id}"),
            ty,
            source_span: None,
            synthetic: true,
            parameter: false,
        })
        .collect();
        let compute = |id, value| SourceInst::Compute {
            local: Id(id),
            value,
            span: Span::dummy(),
        };
        let mut program = lowered("fn f() -> i64 { return 0; }");
        let mut block = program.functions.remove(0).blocks.remove(0);
        block.id = BlockId(0);
        block.instrs = vec![
            compute(0, V::ObjectAlloc { class }),
            compute(1, V::Use(O::Local(Id(0)))),
            compute(
                2,
                V::FieldStore {
                    object: O::Local(Id(1)),
                    object_ty: object_ty.clone(),
                    field: "x".into(),
                    value: O::Int(17),
                },
            ),
            compute(
                3,
                V::FieldLoad {
                    object: O::Local(Id(1)),
                    object_ty,
                    field: "x".into(),
                    result: Type::I64,
                },
            ),
            compute(
                4,
                V::Binary {
                    op: BinOp::Add,
                    lhs: O::Local(Id(3)),
                    rhs: O::Int(25),
                    operand_ty: Type::I64,
                },
            ),
        ];
        block.terminator = SourceTerminator::Return(Some(var("v4")));
        (vec![block], locals)
    }

    #[test]
    fn scalar_replacement_eliminates_aliases_and_preserves_computed_value() {
        use super::super::lowered::LirRvalue as V;
        let (mut blocks, locals) = scalar_object_fixture();
        scalar_replace_objects(&mut blocks, &locals);
        assert_eq!(blocks[0].instrs.len(), 2);
        assert!(matches!(
            &blocks[0].instrs[0],
            SourceInst::Compute {
                value: V::Use(super::super::lowered::LirOperand::Int(17)),
                ..
            }
        ));
        // Execute the surviving scalar dataflow, independently of the field
        // replacement algorithm: the observable returned value remains 42.
        let mut values = HashMap::new();
        for inst in &blocks[0].instrs {
            let SourceInst::Compute { local, value, .. } = inst else {
                panic!("expected scalar computation")
            };
            let read = |operand: &super::super::lowered::LirOperand| match operand {
                super::super::lowered::LirOperand::Int(value) => *value,
                super::super::lowered::LirOperand::Local(local) => values[local],
                _ => panic!("expected integer"),
            };
            let result = match value {
                V::Use(value) => read(value),
                V::Binary {
                    op: BinOp::Add,
                    lhs,
                    rhs,
                    ..
                } => read(lhs) + read(rhs),
                _ => panic!("unexpected operation"),
            };
            values.insert(*local, result);
        }
        assert_eq!(values[&super::super::lowered::LirLocalId(4)], 42);
    }

    #[test]
    fn scalar_replacement_runs_after_resolved_constructor_lowering() {
        use super::super::lowered::LirRvalue as V;
        let program = lowered(
            "class Pair { pub x: i64; pub y: i64; } fn sum(x: i64) -> i64 { let p = new Pair(x, 25); return p.x + p.y; }",
        );
        let function = program
            .functions
            .iter()
            .find(|function| function.name == "sum".into())
            .unwrap();
        assert!(
            !function
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(
                    inst,
                    SourceInst::Compute {
                        value: V::ObjectAlloc { .. } | V::FieldStore { .. } | V::FieldLoad { .. },
                        ..
                    }
                ))
        );
    }

    #[test]
    fn scalar_replacement_tracks_named_aliases() {
        let (mut blocks, locals) = scalar_object_fixture();
        blocks[0].instrs[1] = SourceInst::Let {
            local: locals[1].id,
            name: "v1".into(),
            mutable: false,
            span: Span::dummy(),
            ty: locals[1].ty.clone(),
            value: HirExpr {
                kind: HirExprKind::Var("v0".into()),
                ty: locals[0].ty.clone(),
                span: Span::dummy(),
            },
        };
        scalar_replace_objects(&mut blocks, &locals);
        assert_eq!(blocks[0].instrs.len(), 2);
    }

    #[test]
    fn scalar_replacement_keeps_escaping_uninitialized_and_observed_objects() {
        use super::super::lowered::{LirLocalId as Id, LirRvalue as V};
        for scenario in 0..3 {
            let (mut blocks, locals) = scalar_object_fixture();
            match scenario {
                0 => {
                    blocks[0].terminator = SourceTerminator::Return(Some(HirExpr {
                        kind: HirExprKind::Var("v1".into()),
                        ty: locals[1].ty.clone(),
                        span: Span::dummy(),
                    }))
                }
                1 => {
                    blocks[0].instrs.remove(2);
                }
                _ => blocks[0].instrs.insert(
                    3,
                    SourceInst::Compute {
                        local: Id(2),
                        value: V::BuiltinCall {
                            callee: crate::semantic::ids::FunctionId::free("gc_allocated_bytes"),
                            args: vec![],
                            params: vec![],
                            result: Type::I64,
                        },
                        span: Span::dummy(),
                    },
                ),
            }
            let before = blocks.clone();
            scalar_replace_objects(&mut blocks, &locals);
            assert_eq!(blocks, before, "scenario {scenario}");
        }
    }

    #[test]
    fn scalar_replacement_does_not_re_read_a_mutated_stored_operand() {
        use super::super::lowered::{LirLocalId as Id, LirOperand as O, LirRvalue as V};
        let (mut blocks, locals) = scalar_object_fixture();
        if let SourceInst::Compute {
            value: V::FieldStore { value, .. },
            ..
        } = &mut blocks[0].instrs[2]
        {
            *value = O::Local(Id(4));
        }
        blocks[0].instrs.insert(
            2,
            SourceInst::Compute {
                local: Id(4),
                value: V::Use(O::Int(17)),
                span: Span::dummy(),
            },
        );
        let before = blocks.clone();
        scalar_replace_objects(&mut blocks, &locals);
        assert_eq!(blocks, before);
    }

    #[test]
    fn inlines_small_scalar_leaves_but_preserves_faulting_calls() {
        use super::super::lowered::LirRvalue;
        let program = lowered(
            "fn twice(x: i64) -> i64 { return x * 2; } fn divide(x: i64, y: i64) -> i64 { return x / y; } fn f(x: i64, y: i64) -> i64 { return twice(x) + divide(x, y); }",
        );
        let function = program
            .functions
            .iter()
            .find(|f| f.name == "f".into())
            .unwrap();
        let callees: Vec<_> = function
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| {
                if let SourceInst::Compute {
                    value: LirRvalue::DirectCall { callee, .. },
                    ..
                } = inst
                {
                    Some(callee.to_string())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(callees, ["divide"]);
    }

    #[test]
    fn dce_removes_dead_scalar_dependency_chains() {
        let program = lowered("fn f(x: i64) { (x * 2) + 1; }");
        assert!(
            !program.functions[0]
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(inst, SourceInst::Compute { .. }))
        );
    }

    #[test]
    fn dce_preserves_faults_and_their_live_inputs() {
        let program = lowered("fn f(x: i64, y: i64) { (x + 1) / y; }");
        let operations: Vec<_> = program.functions[0]
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| {
                if let SourceInst::Compute { value, .. } = inst {
                    Some(value)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(operations.len(), 2, "{operations:?}");
    }

    #[test]
    fn cfg_removes_only_the_unchosen_constant_arm() {
        let program = lowered(
            "fn effect() {} fn f() { if 1 + 2 == 3 { effect(); } else { effect(); effect(); } }",
        );
        let function = program
            .functions
            .iter()
            .find(|f| f.name == "f".into())
            .unwrap();
        let calls = function
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter(|inst| {
                matches!(
                    inst,
                    SourceInst::Expr(HirExpr {
                        kind: HirExprKind::Call { .. },
                        ..
                    }) | SourceInst::Compute {
                        value: super::super::lowered::LirRvalue::DirectCall { .. },
                        ..
                    }
                )
            })
            .count();
        assert_eq!(calls, 1);
    }

    fn int(n: i64) -> HirExpr {
        HirExpr {
            kind: HirExprKind::Int(n),
            ty: Type::I64,
            span: Span::dummy(),
        }
    }
    fn bin(op: BinOp, lhs: HirExpr, rhs: HirExpr) -> HirExpr {
        HirExpr {
            kind: HirExprKind::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            ty: Type::I64,
            span: Span::dummy(),
        }
    }

    #[test]
    fn folds_constant_exponent_but_preserves_dynamic_base() {
        let base = HirExpr {
            kind: HirExprKind::Call {
                callee: "effect".into(),
                args: vec![],
            },
            ty: Type::I64,
            span: Span::dummy(),
        };
        let mut expr = bin(BinOp::Pow, base.clone(), bin(BinOp::Add, int(1), int(2)));
        fold_expr(&mut expr);
        let HirExprKind::Binary { lhs, rhs, .. } = &expr.kind else {
            panic!("lost call")
        };
        assert_eq!(**lhs, base);
        assert_eq!(**rhs, int(3));
        let mut zero = bin(BinOp::Mul, base.clone(), int(0));
        fold_expr(&mut zero);
        assert!(matches!(zero.kind, HirExprKind::Binary { .. }));
        let mut pow_zero = bin(BinOp::Pow, base, int(0));
        fold_expr(&mut pow_zero);
        assert!(matches!(pow_zero.kind, HirExprKind::Binary { .. }));
    }

    #[test]
    fn preserves_arithmetic_faults_and_wraps_successful_operations() {
        for (op, a, b) in [
            (BinOp::Div, 1, 0),
            (BinOp::Rem, 1, 0),
            (BinOp::Div, i64::MIN, -1),
            (BinOp::Rem, i64::MIN, -1),
            (BinOp::Pow, 2, -1),
        ] {
            let mut expr = bin(op, int(a), int(b));
            let original = expr.clone();
            fold_expr(&mut expr);
            assert_eq!(expr, original);
        }
        for (op, a, b, expected) in [
            (BinOp::Add, i64::MAX, 1, i64::MIN),
            (BinOp::Sub, i64::MIN, 1, i64::MAX),
            (BinOp::Mul, i64::MAX, 2, -2),
            (BinOp::Pow, 2, 63, i64::MIN),
            (BinOp::Pow, -1, i64::MAX, -1),
            (BinOp::Div, -7, 3, -2),
            (BinOp::Rem, -7, 3, -1),
        ] {
            let mut expr = bin(op, int(a), int(b));
            fold_expr(&mut expr);
            assert_eq!(expr, int(expected));
        }
    }

    #[test]
    fn lowered_program_exposes_folded_exponent() {
        let tokens = crate::lexer::Lexer::new(
            "fn f(x: i64) -> i64 { let exponent = 1 + 2; return x ** exponent; }",
        )
        .tokenize()
        .unwrap();
        let (ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let (hir, errors) = crate::ir::lower::lower_program(&ast);
        assert!(errors.is_empty(), "{errors:?}");
        let lir = crate::ir::lowered::lower_source_program(&hir);
        assert!(
            lir.functions[0]
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(
                    inst,
                    SourceInst::Compute {
                        value: super::super::lowered::LirRvalue::Binary {
                            op: BinOp::Pow,
                            rhs: super::super::lowered::LirOperand::Int(3),
                            ..
                        },
                        ..
                    }
                ))
        );
    }

    fn var(name: &str) -> HirExpr {
        HirExpr {
            kind: HirExprKind::Var(name.into()),
            ty: Type::I64,
            span: Span::dummy(),
        }
    }

    fn assign(name: &str, value: HirExpr) -> SourceInst {
        SourceInst::Assign {
            local: super::super::lowered::LirLocalId(0),
            name: name.into(),
            value,
        }
    }

    fn optimize_return(instrs: Vec<SourceInst>, value: HirExpr) -> HirExpr {
        let mut blocks = vec![SourceBlock {
            id: super::super::lowered::BlockId(0),
            instrs,
            terminator: SourceTerminator::Return(Some(value)),
            recovery: vec![],
        }];
        fold_blocks(&mut blocks);
        let SourceTerminator::Return(Some(value)) = &blocks[0].terminator else {
            panic!()
        };
        value.clone()
    }

    #[test]
    fn propagates_local_exponents_and_reassignments() {
        let value = optimize_return(
            vec![assign("exponent", int(2))],
            bin(
                BinOp::Pow,
                var("base"),
                bin(BinOp::Add, var("exponent"), int(1)),
            ),
        );
        let HirExprKind::Binary { lhs, rhs, .. } = &value.kind else {
            panic!()
        };
        assert_eq!(**lhs, var("base"));
        assert_eq!(**rhs, int(3));
        assert_eq!(
            optimize_return(
                vec![
                    assign("x", int(2)),
                    assign("x", bin(BinOp::Add, var("x"), int(4)))
                ],
                var("x")
            ),
            int(6)
        );
        assert_eq!(
            optimize_return(
                vec![assign("x", int(2)), assign("x", var("unknown"))],
                var("x")
            ),
            var("x")
        );
    }

    #[test]
    fn calls_and_reference_arguments_invalidate_facts() {
        for args in [
            vec![],
            vec![HirExpr {
                kind: HirExprKind::ReferenceArg {
                    place: Box::new(var("x")),
                },
                ty: Type::I64,
                span: Span::dummy(),
            }],
        ] {
            let call = HirExpr {
                kind: HirExprKind::Call {
                    callee: "mutate".into(),
                    args,
                },
                ty: Type::I64,
                span: Span::dummy(),
            };
            let mut blocks = vec![SourceBlock {
                id: super::super::lowered::BlockId(0),
                instrs: vec![assign("x", int(2)), SourceInst::Expr(call.clone())],
                terminator: SourceTerminator::Return(Some(var("x"))),
                recovery: vec![],
            }];
            fold_blocks(&mut blocks);
            assert_eq!(blocks[0].instrs[1], SourceInst::Expr(call));
            assert_eq!(
                blocks[0].terminator,
                SourceTerminator::Return(Some(var("x")))
            );
        }
    }

    #[test]
    fn assignment_invalidates_potentially_aliased_local() {
        assert_eq!(
            optimize_return(vec![assign("x", int(2)), assign("alias", int(9))], var("x")),
            var("x")
        );
    }

    #[test]
    fn propagation_preserves_faulting_arithmetic() {
        let result = optimize_return(
            vec![assign("zero", int(0))],
            bin(BinOp::Div, int(1), var("zero")),
        );
        assert_eq!(result, bin(BinOp::Div, int(1), int(0)));
    }

    #[test]
    fn fifty_thousand_levels_fold_on_a_small_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let mut expr = int(0);
                for _ in 0..50_000 {
                    expr = bin(BinOp::Add, expr, int(1));
                }
                fold_expr(&mut expr);
                assert_eq!(expr, int(50_000));
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

/// Eliminate one nonescaping scalar aggregate in a pure single-block body.
/// No call, allocation, fault, or GC observation may cross the replacement.
/// Stored operands must remain stable: replacing a store with a later read of
/// a mutable variable would otherwise observe the wrong version of its value.
pub(crate) fn scalar_replace_objects(
    blocks: &mut [SourceBlock],
    locals: &[super::lowered::LirLocal],
) {
    use super::lowered::async_liveness::{instruction_use_def, terminator_uses};
    use super::lowered::{LirLocalId, LirOperand as O, LirRvalue as V};
    use std::collections::HashSet;
    let [block] = blocks else {
        return;
    };
    if !block.recovery.is_empty() {
        return;
    }
    let scalar = |ty: &Type| matches!(ty, Type::I64 | Type::F64 | Type::Bool);
    let atom = |expr: &HirExpr| {
        scalar(&expr.ty)
            && matches!(
                expr.kind,
                HirExprKind::Int(_)
                    | HirExprKind::Float(_)
                    | HirExprKind::Bool(_)
                    | HirExprKind::Var(_)
            )
    };
    if !matches!(&block.terminator, SourceTerminator::Return(None))
        && !matches!(&block.terminator, SourceTerminator::Return(Some(value)) if atom(value))
    {
        return;
    }
    let names: HashMap<_, _> = locals
        .iter()
        .map(|local| (local.name.as_str(), local.id))
        .collect();
    let mut last_def = HashMap::new();
    let mut counts = HashMap::<LirLocalId, usize>::new();
    let mut uses = HashSet::new();
    let mut defs = HashSet::new();
    let mut candidate = None;
    for (index, inst) in block.instrs.iter().enumerate() {
        uses.clear();
        defs.clear();
        instruction_use_def(inst, &names, &mut uses, &mut defs);
        for &id in &defs {
            last_def.insert(id, index);
            *counts.entry(id).or_default() += 1;
        }
        if let SourceInst::Compute {
            local,
            value: V::ObjectAlloc { class },
            ..
        } = inst
        {
            if candidate.replace((*local, *class, index)).is_some() {
                return;
            }
        }
    }
    let Some((object, class, allocated_at)) = candidate else {
        return;
    };
    if counts.get(&object) != Some(&1) {
        return;
    }
    let mut aliases = HashSet::from([object]);
    let mut fields = HashMap::<String, O>::new();
    let mut remove = HashSet::from([allocated_at]);
    let mut replacements = HashMap::new();
    let mut discard_defs = HashSet::new();
    for (index, inst) in block.instrs.iter().enumerate() {
        if index == allocated_at {
            continue;
        }
        let alias_source = match inst {
            SourceInst::Compute {
                local,
                value: V::Use(O::Local(source)),
                ..
            } if aliases.contains(source) => Some((*local, *source)),
            SourceInst::Let {
                name,
                value:
                    HirExpr {
                        kind: HirExprKind::Var(source),
                        ..
                    },
                ..
            } => names
                .get(source.as_str())
                .filter(|source| aliases.contains(source))
                .and_then(|source| names.get(name.as_str()).map(|target| (*target, *source))),
            _ => None,
        };
        if let Some((target, _)) = alias_source {
            if index < allocated_at {
                return;
            }
            if counts.get(&target) != Some(&1) {
                return;
            }
            aliases.insert(target);
            remove.insert(index);
            continue;
        }
        match inst {
            SourceInst::Compute {
                local,
                value:
                    V::FieldStore {
                        object: O::Local(owner),
                        object_ty,
                        field,
                        value,
                    },
                ..
            } if aliases.contains(owner) => {
                if !matches!(object_ty, Type::Named(name) if *name == class)
                    || !value.ty(locals).is_some_and(|ty| scalar(&ty))
                {
                    return;
                }
                if let O::Local(source) = value {
                    if aliases.contains(source)
                        || last_def.get(source).is_some_and(|defined| *defined > index)
                    {
                        return;
                    }
                }
                fields.insert(field.clone(), value.clone());
                discard_defs.insert(*local);
                remove.insert(index);
                continue;
            }
            SourceInst::Compute {
                value:
                    V::FieldLoad {
                        object: O::Local(owner),
                        object_ty,
                        field,
                        result,
                    },
                ..
            } if aliases.contains(owner) => {
                let Some(value) = fields.get(field) else {
                    return;
                };
                if !matches!(object_ty, Type::Named(name) if *name == class)
                    || !scalar(result)
                    || value.ty(locals).as_ref() != Some(result)
                {
                    return;
                }
                replacements.insert(index, value.clone());
                continue;
            }
            SourceInst::Compute { value, .. } => match value {
                V::Use(operand) if operand.ty(locals).is_some_and(|ty| scalar(&ty)) => {}
                V::Unary { ty, .. } if scalar(ty) => {}
                V::Binary { op, operand_ty, .. }
                    if scalar(operand_ty)
                        && !matches!(op, BinOp::Div | BinOp::Rem | BinOp::Pow) => {}
                _ => return,
            },
            SourceInst::Let { value, .. }
            | SourceInst::Assign { value, .. }
            | SourceInst::Expr(value)
                if atom(value) => {}
            SourceInst::ClearScopeRoots { .. } => continue,
            _ => return,
        }
        uses.clear();
        defs.clear();
        instruction_use_def(inst, &names, &mut uses, &mut defs);
        if !uses.is_disjoint(&aliases) || !defs.is_disjoint(&aliases) {
            return;
        }
    }
    // Removed store instructions produce only unused void temporaries. Any
    // unusual consumer of that result makes this candidate ineligible.
    for (index, inst) in block.instrs.iter().enumerate() {
        if remove.contains(&index) || matches!(inst, SourceInst::ClearScopeRoots { .. }) {
            continue;
        }
        uses.clear();
        defs.clear();
        instruction_use_def(inst, &names, &mut uses, &mut defs);
        if !uses.is_disjoint(&discard_defs) {
            return;
        }
    }
    uses.clear();
    terminator_uses(&block.terminator, &names, &mut uses, &HashSet::new());
    if !uses.is_disjoint(&aliases) || !uses.is_disjoint(&discard_defs) {
        return;
    }
    let mut index = 0;
    block.instrs.retain_mut(|inst| {
        let at = index;
        index += 1;
        if remove.contains(&at) {
            return false;
        }
        if let Some(operand) = replacements.remove(&at) {
            if let SourceInst::Compute { value, .. } = inst {
                *value = V::Use(operand);
            }
        }
        true
    });
}
