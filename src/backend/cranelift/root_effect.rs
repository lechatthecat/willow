//! Conservative, intraprocedural proof for synchronous root-free bodies.
//!
//! The live emission-path root count is not a function maximum. Inspect the
//! complete LIR before binding parameters or emitting the entry snapshot instead.
use crate::ir::lowered::{LirFunction, LirInst, LirOperand, LirRvalue, Terminator};
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
