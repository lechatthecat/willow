//! Fail-closed, whole-function confinement proof for scalar maps.
//! No alias graph is needed: every map source must be a fresh constructor or
//! local copy, and no handle may leave the function. Unknown uses keep locks.
use crate::ir::ir_variants;
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
            is_map(&local.ty).is_some_and(|(key, value)| {
                local.parameter || !(scalar(key) || matches!(key, Type::String)) || !scalar(value)
            })
        })
    {
        return false;
    }
    let map_operand = |operand: &LirOperand| match operand {
        LirOperand::Local(id) => maps[id.0 as usize],
        LirOperand::Int(_)
        | LirOperand::Integer { .. }
        | LirOperand::Float(_)
        | LirOperand::Bool(_)
        | LirOperand::Reference { .. } => false,
    };
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
                                crate::semantic::builtin_types::resolve_name(class)
                                    == Some(crate::semantic::builtin_types::BuiltinTypeId::Map)
                                    && method == "new"
                                    && args.is_empty()
                            }
                            ir_variants!(
                                LirRvalue;
                                BeginReferenceCall, AwaitFuture, StartTask, SelectIdleWait,
                                Coerce, ArrayAlloc, CaptureArrayOwner, ArrayStore, Index,
                                ObjectAlloc, FieldLoad, FieldStore, StaticField, StaticStore,
                                EnumAlloc, EnumPayloadStore, ConstructorCall, Range, EnumMethod,
                                BuiltinCall, FormatScalar, Panic, Recover, ReferenceDebug,
                                RebindResultError, IntoError, MethodCall, StringLiteral,
                                FunctionRef, Closure, Print, IntrinsicCall, DirectCall,
                                IndirectCall, Unary, Binary, StringConcat
                            ) => false,
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
                                            | Intrinsic::MapRemove
                                            | Intrinsic::MapKeys
                                            | Intrinsic::MapValues
                                            | Intrinsic::MapEntries
                                            | Intrinsic::MapContains
                                            | Intrinsic::MapLen
                                            | Intrinsic::MapToString
                                            | Intrinsic::MapFreeze
                                    )
                            }
                            ir_variants!(
                                LirRvalue;
                                BeginReferenceCall, AwaitFuture, StartTask, SelectIdleWait,
                                Coerce, ArrayAlloc, CaptureArrayOwner, ArrayStore, Index,
                                ObjectAlloc, FieldLoad, FieldStore, StaticField, StaticStore,
                                StaticCall, EnumAlloc, EnumPayloadStore, ConstructorCall, Range,
                                EnumMethod, BuiltinCall, FormatScalar, Panic, Recover,
                                ReferenceDebug, RebindResultError, IntoError, PrepareMethod, MethodCall,
                                StringLiteral, FunctionRef, Closure, Print, DirectCall,
                                IndirectCall, Unary, Binary, StringConcat
                            ) => false,
                        };
                    valid_source
                        && valid_use
                        && operands.into_iter().all(LirOperand::is_plain_value)
                        && !matches!(value, LirRvalue::ReferenceDebug { .. })
                }
                LirInst::Let { local, value, .. } | LirInst::Assign { local, value, .. } => {
                    value.is_plain_value() && maps[local.0 as usize] == map_operand(value)
                }
                LirInst::ClearScopeRoots { .. }
                | LirInst::LeaveDeferScope { .. }
                | LirInst::RestoreCallPreparation { .. }
                | LirInst::FlushDefers { .. } => true,
                LirInst::EnterDeferScope { sites, lock, .. } => sites.is_empty() && lock.is_none(),
                LirInst::MatchTest { scrutinee, .. } => !maps[scrutinee.0 as usize],
                LirInst::MatchBind {
                    scrutinee,
                    bindings,
                    ..
                } => !maps[scrutinee.0 as usize] && bindings.iter().all(|id| !maps[id.0 as usize]),
                // In particular, reject cleanup captures and suspension/locks.
                ir_variants!(
                    LirInst;
                    Defer, SelectInit, SelectProbe, SelectPick, SelectUnregister, SelectCommit,
                    ReleaseLock, Unsupported
                ) => false,
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
                cond.is_plain_value() && !map_operand(cond)
            }
            Terminator::Suspend { .. } | Terminator::CleanupReturn => false,
        };
        if !allowed {
            return false;
        }
    }
    true
}
