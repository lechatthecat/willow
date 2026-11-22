//! Direct calls through function values whose target is statically known.
//!
//! `let f = add1; f(x)` calls through a function value, but `f` can only ever
//! hold `add1`'s shared value: it is defined once, by an immutable binding or
//! a compiler temporary, from a function reference or from another such
//! local. A call through it is then a direct call to `add1`, which the leaf
//! inliner and the backend treat exactly like a call written `add1(x)`. A
//! named function never reads the trailing environment argument of a call
//! through a value, so dropping it changes nothing the callee can observe.
//!
//! A binding left unused by the rewrite (`f` above, and every copy chain
//! feeding only rewritten calls) is removed, so the direct call does not
//! still load the function's shared value on every call of the body.
//!
//! Cost: one pass over the instructions to count definitions and uses, one
//! memoized resolution per local, one pass to rewrite the calls, and one
//! worklist over the removed bindings (each local enters it at most once),
//! so O(instructions + operands + locals) per body.

use std::collections::{HashMap, HashSet};

use super::super::lowered::{LirLocalId, LirOperand, LirRvalue, SourceFunction, SourceInst};
use super::super::typed_ast::HirExprKind;
use crate::semantic::ids::FunctionId;

#[cfg(test)]
thread_local! {
    pub(super) static REMOVAL_VISITS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

#[derive(Clone, Copy)]
enum Def {
    Function(FunctionId),
    Copy(LirLocalId),
    Opaque,
}

/// Losing the last use of a resolved binding makes its definition removable.
fn remove_use(local: LirLocalId, use_counts: &mut [u32], dead: &mut Vec<LirLocalId>) {
    let remaining = &mut use_counts[local.0 as usize];
    *remaining -= 1;
    if *remaining == 0 {
        dead.push(local);
    }
}

/// Rewrite calls through locals that always hold one synchronous named
/// function's value into direct calls. `direct` lists the functions a direct
/// call may name (an `async` target would need its task protocol instead).
pub(crate) fn devirtualize_function_values(
    function: &mut SourceFunction,
    direct: &HashMap<FunctionId, usize>,
) {
    use super::super::lowered::async_liveness::{clear_local_scratch, instruction_use_def};
    let names: HashMap<_, _> = function
        .locals
        .iter()
        .map(|local| (local.name.as_str(), local.id))
        .collect();
    use super::super::lowered::async_liveness::terminator_uses;
    let mut definitions = vec![0u32; function.locals.len()];
    let mut sources = vec![Def::Opaque; function.locals.len()];
    // Instructions and terminators reading each local, and where each
    // function-or-copy binding is defined.
    let mut use_counts = vec![0u32; function.locals.len()];
    let mut def_blocks = vec![None; function.locals.len()];
    let mut uses = HashSet::new();
    let mut defs = HashSet::new();
    let mut any_call = false;
    for (block_index, block) in function.blocks.iter().enumerate() {
        for inst in &block.instrs {
            clear_local_scratch(&mut defs);
            clear_local_scratch(&mut uses);
            instruction_use_def(inst, &names, &mut uses, &mut defs);
            for local in &defs {
                definitions[local.0 as usize] += 1;
            }
            for local in &uses {
                use_counts[local.0 as usize] += 1;
            }
            let (local, def) = match inst {
                SourceInst::Compute { local, value, .. } => {
                    any_call |= matches!(value, LirRvalue::IndirectCall { .. });
                    // A reference argument may write its place.
                    for operand in value.operands() {
                        match operand {
                            LirOperand::Reference { place, .. } => {
                                for id in place.locals() {
                                    definitions[id.0 as usize] += 2;
                                }
                            }
                            ir_variants!(LirOperand; Local, Int, Integer, Float, Bool) => {}
                        }
                    }
                    let def = match value {
                        LirRvalue::FunctionRef { function, .. } => Def::Function(*function),
                        LirRvalue::Use(LirOperand::Local(source)) => Def::Copy(*source),
                        ir_variants!(
                            LirRvalue;
                            BeginReferenceCall, Use, AwaitFuture, StartTask, SelectIdleWait,
                            Coerce, ArrayAlloc, CaptureArrayOwner, ArrayStore, Index, ObjectAlloc,
                            FieldLoad, FieldStore, StaticField, StaticStore, StaticCall,
                            EnumAlloc, EnumPayloadStore, ConstructorCall, Range, EnumMethod,
                            BuiltinCall, FormatScalar, Panic, Recover, ReferenceDebug,
                            RebindResultError, IntoError, PrepareMethod, MethodCall,
                            StringLiteral, Closure, Print, IntrinsicCall, DirectCall,
                            IndirectCall, Unary, Binary, StringConcat
                        ) => Def::Opaque,
                    };
                    // Only a compiler temporary is defined solely here; a
                    // source variable is bound by `Let`.
                    if !function.locals[local.0 as usize].synthetic {
                        continue;
                    }
                    (*local, def)
                }
                SourceInst::Let {
                    local,
                    mutable: false,
                    value,
                    ..
                } => {
                    let def = match &value.kind {
                        HirExprKind::FnRef(function) => Def::Function(*function),
                        HirExprKind::Var(name) => names
                            .get(name.as_str())
                            .map_or(Def::Opaque, |source| Def::Copy(*source)),
                        ir_variants!(
                            HirExprKind;
                            Int, Float, Bool, Str, Binary, Unary, Call, Invoke, Print, Array,
                            Index, Ternary, New, FieldAccess, MethodCall, ObjectLiteral,
                            StaticField, StaticCall, ReferenceArg, Range, Await, TryPropagate,
                            Lambda, Match, Select
                        ) => Def::Opaque,
                    };
                    (*local, def)
                }
                ir_variants!(
                    SourceInst;
                    EnterDeferScope, LeaveDeferScope, FlushDefers, ClearScopeRoots, Defer, Let,
                    Assign, FieldAssign, IndexAssign, StaticFieldAssign, SuperInit, SelectInit,
                    SelectProbe, SelectPick, SelectUnregister, SelectCommit, ReleaseLock,
                    MatchTest, MatchBind, Expr
                ) => continue,
            };
            sources[local.0 as usize] = def;
            def_blocks[local.0 as usize] = Some(block_index);
        }
        clear_local_scratch(&mut uses);
        terminator_uses(&block.terminator, &names, &mut uses, &HashSet::new());
        for local in &uses {
            use_counts[local.0 as usize] += 1;
        }
    }
    if !any_call {
        return;
    }
    #[derive(Clone, Copy)]
    enum Memo {
        Unvisited,
        Visiting,
        Done(Option<FunctionId>),
    }
    let mut memo = vec![Memo::Unvisited; function.locals.len()];
    let mut resolve = |start: LirLocalId| -> Option<FunctionId> {
        let mut chain = Vec::new();
        let mut current = start;
        let found = loop {
            match memo[current.0 as usize] {
                Memo::Done(found) => break found,
                Memo::Visiting => break None,
                Memo::Unvisited => {}
            }
            memo[current.0 as usize] = Memo::Visiting;
            chain.push(current);
            if definitions[current.0 as usize] != 1 || function.locals[current.0 as usize].parameter
            {
                break None;
            }
            match sources[current.0 as usize] {
                Def::Function(target) => break direct.contains_key(&target).then_some(target),
                Def::Copy(source) => current = source,
                Def::Opaque => break None,
            }
        };
        for id in chain {
            memo[id.0 as usize] = Memo::Done(found);
        }
        found
    };
    let mut dead = Vec::new();
    for block in &mut function.blocks {
        for inst in &mut block.instrs {
            let SourceInst::Compute { value, .. } = inst else {
                continue;
            };
            let LirRvalue::IndirectCall {
                callee: LirOperand::Local(callee),
                args,
                params,
                result,
                ..
            } = value
            else {
                continue;
            };
            let Some(target) = resolve(*callee) else {
                continue;
            };
            remove_use(*callee, &mut use_counts, &mut dead);
            *value = LirRvalue::DirectCall {
                callee: target,
                args: std::mem::take(args),
                params: std::mem::take(params),
                result: result.clone(),
            };
        }
    }
    // Remove bindings the rewrite left unread. Each resolved local has
    // exactly one definition, a function reference or a copy; removing a
    // copy may leave its source unread in turn.
    if dead.is_empty() {
        return;
    }
    let mut removed = vec![false; function.locals.len()];
    let mut changed_blocks = vec![false; function.blocks.len()];
    while let Some(local) = dead.pop() {
        let id = local.0 as usize;
        // Only resolved locals enter this list, when their last use disappears.
        // Each has exactly one defining instruction, identified by its local.
        let block = def_blocks[id].expect("resolved callable has a definition");
        removed[id] = true;
        changed_blocks[block] = true;
        #[cfg(test)]
        REMOVAL_VISITS.with(|visits| visits.update(|(marks, scans)| (marks + 1, scans)));
        if let Def::Copy(source) = sources[id] {
            remove_use(source, &mut use_counts, &mut dead);
        }
    }
    for (block, changed) in function.blocks.iter_mut().zip(changed_blocks) {
        if changed {
            block.instrs.retain(|inst| {
                #[cfg(test)]
                REMOVAL_VISITS.with(|visits| visits.update(|(marks, scans)| (marks, scans + 1)));
                match inst {
                    SourceInst::Compute { local, .. } | SourceInst::Let { local, .. } => {
                        !removed[local.0 as usize]
                    }
                    // Only Compute and immutable Let populate the definition table.
                    ir_variants!(
                        SourceInst;
                        Assign, EnterDeferScope, LeaveDeferScope, FlushDefers, ClearScopeRoots,
                        Defer, FieldAssign, IndexAssign, StaticFieldAssign, SuperInit, SelectInit,
                        SelectProbe, SelectPick, SelectUnregister, SelectCommit, ReleaseLock,
                        MatchTest, MatchBind, Expr
                    ) => true,
                }
            });
        }
    }
}
