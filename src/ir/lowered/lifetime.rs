//! End the ownership of flat temporaries at their last use. A temporary root
//! must not retain (or pin) an object after its source binding has taken over.
use std::collections::{HashMap, HashSet};
use super::{SourceFunction, SourceInst, LirLocalId, SourceTerminator};
use super::async_liveness::{instruction_use_def, terminator_uses};

pub(super) fn clear_dead_temporaries(function: &mut SourceFunction) {
    let mut functions = vec![function];
    while let Some(function) = functions.pop() {
        clear_function(function);
        for block in &mut function.blocks {
            for instruction in &mut block.instrs {
                if let SourceInst::Defer { body, .. } = instruction { functions.push(body.function.as_mut()); }
            }
        }
    }
}

fn clear_function(function: &mut SourceFunction) {
    let names: HashMap<_, _> = function.locals.iter().map(|local| (local.name.as_str(), local.id)).collect();
    // Deferred regions read their captured slots at cleanup, after registration.
    let captured: HashSet<_> = function.blocks.iter().flat_map(|block| &block.instrs).filter_map(|inst|
        if let SourceInst::Defer { body, .. } = inst { Some(&body.captures) } else { None }).flatten().copied().collect();
    let temporary = |id: &LirLocalId| function.locals.get(id.0 as usize).is_some_and(|local|
        local.synthetic && !local.parameter && !captured.contains(id)
            && (local.storage_kind == super::LirStorageKind::GcOwner
                || matches!(local.ty, crate::semantic::ids::SemanticType::String | crate::semantic::ids::SemanticType::Array(_) | crate::semantic::ids::SemanticType::Named(_) | crate::semantic::ids::SemanticType::Generic(..) | crate::semantic::ids::SemanticType::Closure(..))));
    let mut uses = vec![HashSet::new(); function.blocks.len()];
    let mut defs = uses.clone();
    let mut successors = vec![Vec::new(); function.blocks.len()];
    for (index, block) in function.blocks.iter().enumerate() {
        for instruction in &block.instrs { instruction_use_def(instruction, &names, &mut uses[index], &mut defs[index]); }
        terminator_uses(&block.terminator, &names, &mut uses[index], &defs[index]);
        successors[index] = match block.terminator {
            SourceTerminator::Jump(target) | SourceTerminator::Suspend { resume: target, .. } => vec![target.0],
            SourceTerminator::Branch { then_block, else_block, .. } => vec![then_block.0, else_block.0],
            _ => Vec::new(),
        };
        successors[index].extend(block.recovery.iter().map(|target| target.0));
    }
    let mut live_in = uses.clone();
    let mut live_out = vec![HashSet::new(); function.blocks.len()];
    loop {
        let mut changed = false;
        for index in (0..function.blocks.len()).rev() {
            let out: HashSet<_> = successors[index].iter().flat_map(|&target| live_in[target].iter().copied()).collect();
            let mut input = uses[index].clone();
            input.extend(out.difference(&defs[index]).copied());
            changed |= input != live_in[index] || out != live_out[index];
            live_in[index] = input; live_out[index] = out;
        }
        if !changed { break; }
    }
    for (index, block) in function.blocks.iter_mut().enumerate() {
        let mut live = live_out[index].clone();
        terminator_uses(&block.terminator, &names, &mut live, &HashSet::new());
        let mut reversed = Vec::with_capacity(block.instrs.len());
        for instruction in std::mem::take(&mut block.instrs).into_iter().rev() {
            let mut read = HashSet::new();
            let mut written = HashSet::new();
            instruction_use_def(&instruction, &names, &mut read, &mut written);
            let mut clear: Vec<_> = read.union(&written).filter(|id| temporary(id) && !live.contains(id)).copied().collect();
            clear.sort_by_key(|id| id.0);
            if !clear.is_empty() { reversed.push(SourceInst::ClearScopeRoots { locals: clear }); }
            live.retain(|id| !written.contains(id));
            live.extend(read);
            reversed.push(instruction);
        }
        reversed.reverse(); block.instrs = reversed;
    }
}
