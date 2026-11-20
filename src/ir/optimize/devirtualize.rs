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

#[derive(Clone, Copy)]
enum Def {
    Function(FunctionId),
    Copy(LirLocalId),
    Opaque,
}

/// Rewrite calls through locals that always hold one synchronous named
/// function's value into direct calls. `direct` lists the functions a direct
/// call may name (an `async` target would need its task protocol instead).
pub(crate) fn devirtualize_function_values(
    function: &mut SourceFunction,
    direct: &HashSet<FunctionId>,
) {
    use super::super::lowered::async_liveness::instruction_use_def;
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
    let mut def_sites = vec![None; function.locals.len()];
    let mut uses = HashSet::new();
    let mut defs = HashSet::new();
    let mut any_call = false;
    for (block_index, block) in function.blocks.iter().enumerate() {
        for (inst_index, inst) in block.instrs.iter().enumerate() {
            defs.clear();
            uses.clear();
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
                        if let LirOperand::Reference { place, .. } = operand {
                            for id in place.locals() {
                                definitions[id.0 as usize] += 2;
                            }
                        }
                    }
                    let def = match value {
                        LirRvalue::FunctionRef { function, .. } => Def::Function(*function),
                        LirRvalue::Use(LirOperand::Local(source)) => Def::Copy(*source),
                        _ => Def::Opaque,
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
                        _ => Def::Opaque,
                    };
                    (*local, def)
                }
                _ => continue,
            };
            sources[local.0 as usize] = def;
            def_sites[local.0 as usize] = Some((block_index, inst_index));
        }
        uses.clear();
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
                Def::Function(target) => break direct.contains(&target).then_some(target),
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
            use_counts[callee.0 as usize] -= 1;
            dead.push(*callee);
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
    let mut removed = vec![false; function.locals.len()];
    let mut sites: Vec<Vec<usize>> = vec![Vec::new(); function.blocks.len()];
    while let Some(local) = dead.pop() {
        let id = local.0 as usize;
        if removed[id] || use_counts[id] != 0 || resolve(local).is_none() {
            continue;
        }
        let Some((block, inst)) = def_sites[id] else {
            continue;
        };
        removed[id] = true;
        sites[block].push(inst);
        if let Def::Copy(source) = sources[id] {
            use_counts[source.0 as usize] -= 1;
            dead.push(source);
        }
    }
    for (block, mut drop) in function.blocks.iter_mut().zip(sites) {
        if drop.is_empty() {
            continue;
        }
        drop.sort_unstable();
        let mut next = drop.into_iter().peekable();
        let mut index = 0;
        block.instrs.retain(|_| {
            let keep = next.peek() != Some(&index);
            if !keep {
                next.next();
            }
            index += 1;
            keep
        });
    }
}
