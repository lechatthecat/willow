//! Fail-closed, whole-function confinement proof for scalar maps.
//! No alias graph is needed: every map source must be a fresh constructor or
//! local copy, and no handle may leave the function. Unknown uses keep locks.
use crate::ir::lowered::{LirFunction, LirInst, LirOperand, LirRvalue, Terminator};
use crate::semantic::builtin_types::{self, BuiltinTypeId};
use crate::semantic::ids::SemanticType as Type;
use crate::semantic::intrinsics::Intrinsic;

#[cfg(test)]
thread_local! { static VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
pub(super) fn take_visits() -> usize {
    VISITS.with(|visits| visits.replace(0))
}

pub(super) fn confined_scalar_maps(f: &LirFunction) -> bool {
    fn is_map(ty: &Type) -> Option<(&Type, &Type)> {
        builtin_types::binary_args(ty, BuiltinTypeId::Map)
    }
    let scalar = |ty: &Type| matches!(ty, Type::I64 | Type::F64 | Type::Bool);
    if f.is_async
        || !f.captures.is_empty()
        || is_map(&f.return_type).is_some()
        || f.params.iter().any(|param| is_map(&param.ty).is_some())
    {
        return false;
    }
    let maps: Vec<_> = f
        .locals
        .iter()
        .map(|local| is_map(&local.ty).is_some())
        .collect();
    if !maps.iter().any(|&map| map)
        || f.locals.iter().any(|local| {
            is_map(&local.ty)
                .is_some_and(|(key, value)| local.parameter || !scalar(key) || !scalar(value))
        })
    {
        return false;
    }
    let map_operand = |operand: &LirOperand| match operand {
        LirOperand::Local(id) => maps[id.0 as usize],
        _ => false,
    };
    let plain = |operand: &LirOperand| !matches!(operand, LirOperand::Reference { .. });
    for block in &f.blocks {
        for instruction in &block.instrs {
            #[cfg(test)]
            VISITS.with(|visits| visits.set(visits.get() + 1));
            let allowed = match instruction {
                LirInst::Compute { local, value, .. } => {
                    let produces_map = maps[local.0 as usize];
                    let valid_source = !produces_map
                        || match value {
                            LirRvalue::Use(value) => map_operand(value),
                            LirRvalue::PrepareMethod { receiver, .. } => map_operand(receiver),
                            LirRvalue::StaticCall {
                                class,
                                method,
                                args,
                                ..
                            } => {
                                *class == crate::semantic::ids::TypeId::local("Map")
                                    && method == "new"
                                    && args.is_empty()
                            }
                            _ => false,
                        };
                    // ReferenceDebug intentionally omits its diagnostic operand
                    // from operands(); reject it explicitly rather than miss it.
                    let operands = value.operands();
                    let map_uses = operands
                        .iter()
                        .filter(|operand| map_operand(operand))
                        .count();
                    let valid_use = map_uses == 0
                        || match value {
                            LirRvalue::Use(_) => produces_map,
                            LirRvalue::PrepareMethod {
                                receiver, method, ..
                            } => {
                                map_uses == 1
                                    && map_operand(receiver)
                                    && matches!(
                                        method.as_str(),
                                        "insert"
                                            | "get"
                                            | "contains"
                                            | "len"
                                            | "toString"
                                            | "freeze"
                                    )
                            }
                            LirRvalue::IntrinsicCall {
                                intrinsic,
                                receiver,
                                ..
                            } => {
                                map_uses == 1
                                    && map_operand(receiver)
                                    && matches!(
                                        intrinsic,
                                        Intrinsic::MapInsert
                                            | Intrinsic::MapGet
                                            | Intrinsic::MapContains
                                            | Intrinsic::MapLen
                                            | Intrinsic::MapToString
                                            | Intrinsic::MapFreeze
                                    )
                            }
                            _ => false,
                        };
                    valid_source
                        && valid_use
                        && operands.into_iter().all(plain)
                        && !matches!(value, LirRvalue::ReferenceDebug { .. })
                }
                LirInst::Let { local, value, .. } | LirInst::Assign { local, value, .. } => {
                    plain(value) && maps[local.0 as usize] == map_operand(value)
                }
                LirInst::ClearScopeRoots { .. }
                | LirInst::LeaveDeferScope { .. }
                | LirInst::FlushDefers { .. } => true,
                LirInst::EnterDeferScope { sites, lock, .. } => sites.is_empty() && lock.is_none(),
                LirInst::MatchTest { scrutinee, .. } => !maps[scrutinee.0 as usize],
                LirInst::MatchBind {
                    scrutinee,
                    bindings,
                    ..
                } => !maps[scrutinee.0 as usize] && bindings.iter().all(|id| !maps[id.0 as usize]),
                // In particular, reject cleanup captures and suspension/locks.
                _ => false,
            };
            if !allowed {
                return false;
            }
        }
        #[cfg(test)]
        VISITS.with(|visits| visits.set(visits.get() + 1));
        let allowed = match &block.terminator {
            Terminator::Jump(_) | Terminator::Return(None) => true,
            Terminator::Branch { cond, .. } | Terminator::Return(Some(cond)) => {
                plain(cond) && !map_operand(cond)
            }
            _ => false,
        };
        if !allowed {
            return false;
        }
    }
    true
}
