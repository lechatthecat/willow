//! Conservative, intraprocedural proof for synchronous root-free bodies.
//!
//! The live emission-path root count is not a function maximum. Inspect the
//! complete LIR before binding parameters or emitting the entry snapshot instead.
use crate::ir::lowered::{LirFunction, LirInst, LirOperand, LirRvalue, Terminator};
use crate::parser::ast::BinOp;
use crate::semantic::ids::SemanticType as Type;

fn immediate(ty: &Type) -> bool {
    matches!(
        ty,
        Type::I64 | Type::F64 | Type::Bool | Type::Void | Type::Never
    )
}

/// Unknown operations/types fail closed. This deliberately excludes even
/// immediate-looking reference parameters, indirect calls and cleanup regions.
/// Callees own their roots and restore them before returning, including on
/// panic/cancellation; their effects need not flow into a scalar caller.
pub(super) fn may_push_gc_roots(function: &LirFunction) -> bool {
    if function.is_async
        || !function.captures.is_empty()
        || !immediate(&function.return_type)
        || function
            .params
            .iter()
            .any(|p| p.by_reference || !immediate(&p.ty))
        || function
            .locals
            .iter()
            .any(|l| l.is_gc_owner() || !immediate(&l.ty))
    {
        return true;
    }
    let operand_is_immediate = |operand: &LirOperand| {
        !matches!(operand, LirOperand::Reference { .. })
            && operand
                .ty(&function.locals)
                .is_some_and(|ty| immediate(&ty))
    };
    for block in &function.blocks {
        for instruction in &block.instrs {
            let safe = match instruction {
                LirInst::Compute { value, .. } => {
                    let operation_is_safe = match value {
                        LirRvalue::Use(_) => true,
                        LirRvalue::Unary { ty, .. } | LirRvalue::Print { ty, .. } => immediate(ty),
                        LirRvalue::Binary { operand_ty, .. } => immediate(operand_ty),
                        LirRvalue::DirectCall { params, result, .. } => {
                            params.iter().all(immediate) && immediate(result)
                        }
                        LirRvalue::StaticCall {
                            arg_types, result, ..
                        } => arg_types.iter().all(immediate) && immediate(result),
                        _ => false,
                    };
                    operation_is_safe && value.operands().into_iter().all(&operand_is_immediate)
                }
                LirInst::Let { value, .. } | LirInst::Assign { value, .. } => {
                    operand_is_immediate(value)
                }
                LirInst::ClearScopeRoots { .. } => true,
                _ => false,
            };
            if !safe {
                return true;
            }
        }
        let safe = match &block.terminator {
            Terminator::Jump(_) | Terminator::Return(None) => true,
            Terminator::Branch { cond, .. } => operand_is_immediate(cond),
            Terminator::Return(Some(value)) => operand_is_immediate(value),
            _ => false,
        };
        if !safe {
            return true;
        }
    }
    false
}

/// Whether a synchronous body can never reach a GC safepoint: every path is
/// bounded (acyclic, no calls) and every operation is an inline immediate
/// computation or a plain heap load. Such a body needs no entry poll — the
/// caller's own polls bound the work around it — and no GC value it reads can
/// move or die before it returns, so an instance method need not root its
/// receiver (willow-8hq4.14). The same instruction set also has no panic
/// path, so nothing jumps to the panic-return block either.
///
/// Unknown operations, terminators and parameter modes fail closed.
pub(super) fn is_safepoint_free_leaf(function: &LirFunction) -> bool {
    if function.is_async
        || !function.captures.is_empty()
        || function.params.iter().any(|p| p.by_reference)
    {
        return false;
    }
    let operand_is_value = |operand: &LirOperand| !matches!(operand, LirOperand::Reference { .. });
    // Stores and returns may implicitly allocate an interface box. Without
    // class metadata, accept only identity conversions.
    let identity = |value: &LirOperand, target: &Type| {
        operand_is_value(value) && value.ty(&function.locals).as_ref() == Some(target)
    };
    for block in &function.blocks {
        if !block.recovery.is_empty() {
            return false;
        }
        for instruction in &block.instrs {
            let safe = match instruction {
                LirInst::Compute { value, .. } => {
                    let operation_is_safe = match value {
                        LirRvalue::Use(_) | LirRvalue::FieldLoad { .. } => true,
                        LirRvalue::Unary { ty, .. } => immediate(ty),
                        // Exponentiation and integer division keep their
                        // runtime helpers and guards; a literal divisor other
                        // than 0 and -1 can trip neither guard.
                        LirRvalue::Binary {
                            op,
                            rhs,
                            operand_ty,
                            ..
                        } => {
                            immediate(operand_ty)
                                && match op {
                                    BinOp::Pow => false,
                                    BinOp::Div | BinOp::Rem => {
                                        *operand_ty == Type::F64
                                            || matches!(rhs, LirOperand::Int(divisor) if *divisor != 0 && *divisor != -1)
                                    }
                                    _ => true,
                                }
                        }
                        _ => false,
                    };
                    operation_is_safe && value.operands().into_iter().all(&operand_is_value)
                }
                LirInst::Let { value, ty, .. } => identity(value, ty),
                LirInst::Assign { local, value, .. } => {
                    identity(value, &function.locals[local.0 as usize].ty)
                }
                LirInst::ClearScopeRoots { .. } => true,
                _ => false,
            };
            if !safe {
                return false;
            }
        }
        let safe = match &block.terminator {
            Terminator::Jump(_) | Terminator::Return(None) => true,
            Terminator::Branch { cond, .. } => operand_is_value(cond),
            Terminator::Return(Some(value)) => identity(value, &function.return_type),
            _ => false,
        };
        if !safe {
            return false;
        }
    }
    is_acyclic(function)
}

/// Iterative three-color DFS over the block graph: O(blocks + edges). Only
/// called after every terminator is known to be `Jump`, `Branch` or `Return`.
fn is_acyclic(function: &LirFunction) -> bool {
    let successors = |block: usize| match &function.blocks[block].terminator {
        Terminator::Jump(target) => [Some(target.0), None],
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => [Some(then_block.0), Some(else_block.0)],
        _ => [None, None],
    };
    // 0 = unvisited, 1 = on the DFS stack, 2 = finished.
    let mut color = vec![0u8; function.blocks.len()];
    if color.is_empty() {
        return true;
    }
    color[0] = 1;
    let mut pending = vec![(0usize, 0usize)];
    while let Some((block, next)) = pending.last_mut() {
        let Some(target) = successors(*block).get(*next).copied() else {
            color[*block] = 2;
            pending.pop();
            continue;
        };
        *next += 1;
        let Some(target) = target else {
            continue;
        };
        match color.get(target) {
            Some(0) => {
                color[target] = 1;
                pending.push((target, 0));
            }
            Some(1) | None => return false,
            _ => {}
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::lowered::{BlockId, LirBlock};

    #[test]
    fn leaf_cfg_proof_handles_deep_chains_and_rejects_cycles() {
        for n in [1, 128, 8192] {
            let mut function = LirFunction::empty_artifact_region();
            function.blocks = (0..n)
                .map(|i| LirBlock {
                    id: BlockId(i),
                    instrs: Vec::new(),
                    recovery: Vec::new(),
                    terminator: if i + 1 == n {
                        Terminator::Return(None)
                    } else {
                        Terminator::Jump(BlockId(i + 1))
                    },
                })
                .collect();
            assert!(is_safepoint_free_leaf(&function), "blocks={n}");
            function.blocks[n - 1].terminator = Terminator::Jump(BlockId(0));
            assert!(!is_safepoint_free_leaf(&function), "cycle blocks={n}");
            function.blocks[n - 1].terminator = Terminator::Jump(BlockId(n));
            assert!(
                !is_safepoint_free_leaf(&function),
                "invalid target blocks={n}"
            );
        }
    }

    #[test]
    fn leaf_cfg_proof_handles_shared_successors_and_many_exits() {
        for n in [2, 128, 8192] {
            let mut function = LirFunction::empty_artifact_region();
            function.blocks = (0..n)
                .map(|i| LirBlock {
                    id: BlockId(i),
                    instrs: Vec::new(),
                    recovery: Vec::new(),
                    terminator: if i + 1 == n {
                        Terminator::Return(None)
                    } else {
                        Terminator::Branch {
                            cond: LirOperand::Bool(true),
                            then_block: BlockId(i + 1),
                            else_block: BlockId(n - 1),
                        }
                    },
                })
                .collect();
            assert!(is_safepoint_free_leaf(&function), "shared exit blocks={n}");
            for i in (1..n).step_by(2) {
                function.blocks[i].terminator = Terminator::Return(None);
            }
            assert!(is_safepoint_free_leaf(&function), "many exits blocks={n}");
        }
    }
}
