//! End root ownership at last use. Async source bindings must also release
//! their frame slots before suspending after their final read.
use super::async_liveness::{instruction_use_def, terminator_uses};
use super::liveness::{self, Liveness};
use super::{BlockId, SourceBlock, SourceFunction, SourceInst, SourceTerminator};
use std::collections::{HashMap, HashSet};

/// Insert last-use root clears into `function` and its defer bodies. Returns
/// the root body's liveness, updated for split edges, so async frame planning
/// reuses it instead of solving the same equations again.
pub(super) fn clear_dead_temporaries(function: &mut SourceFunction) -> Liveness {
    let root = clear_function(function);
    let mut functions = vec![function];
    while let Some(function) = functions.pop() {
        for block in &mut function.blocks {
            for instruction in &mut block.instrs {
                if let SourceInst::Defer { body, .. } = instruction {
                    clear_function(body.function.as_mut());
                    functions.push(body.function.as_mut());
                }
            }
        }
    }
    root
}

fn clear_function(function: &mut SourceFunction) -> Liveness {
    let names: HashMap<_, _> = function
        .locals
        .iter()
        .map(|local| (local.name.as_str(), local.id))
        .collect();
    // Cleanup reads captured slots even when cancellation has no normal CFG
    // exit (for example, an infinite loop while holding a lock).
    let mut captured: HashSet<_> = function
        .blocks
        .iter()
        .flat_map(|block| &block.instrs)
        .filter_map(|inst| {
            if let SourceInst::Defer { body, .. } = inst {
                Some(&body.captures)
            } else {
                None
            }
        })
        .flatten()
        .copied()
        .collect();
    for instruction in function.blocks.iter().flat_map(|block| &block.instrs) {
        if let SourceInst::EnterDeferScope {
            lock: Some(slots), ..
        } = instruction
        {
            captured.extend([slots.handle, slots.token, slots.phase, slots.binding]);
        }
    }
    let mut clearable = vec![0u64; function.locals.len().div_ceil(64)];
    for local in &function.locals {
        if (local.synthetic || function.is_async)
            && !local.parameter
            && !captured.contains(&local.id)
            && (local.storage_kind == super::LirStorageKind::GcOwner
                || matches!(
                    local.ty,
                    crate::semantic::ids::SemanticType::String
                        | crate::semantic::ids::SemanticType::Array(_)
                        | crate::semantic::ids::SemanticType::Named(_)
                        | crate::semantic::ids::SemanticType::Generic(..)
                        | crate::semantic::ids::SemanticType::Closure(..)
                ))
        {
            liveness::insert(&mut clearable, local.id);
        }
    }
    let mut live_sets = Liveness::compute_named(&function.blocks, function.locals.len(), &names);
    let mut edges = Vec::new();
    let original_blocks = function.blocks.len();
    let mut read = HashSet::new();
    let mut written = HashSet::new();
    let mut recovery_live = vec![0u64; clearable.len()];
    for (index, block) in function.blocks.iter_mut().enumerate() {
        let mut live = live_sets.live_out.row(index).to_vec();
        // Recovery targets are successors, so `live` already contains their
        // live-in sets; the instruction walk below never removes them.
        recovery_live.fill(0);
        for target in &block.recovery {
            for (word, bits) in recovery_live
                .iter_mut()
                .zip(live_sets.live_in.row(target.0))
            {
                *word |= bits;
            }
        }
        read.clear();
        terminator_uses(&block.terminator, &names, &mut read, &HashSet::new());
        read.iter().for_each(|&id| liveness::insert(&mut live, id));
        // A value used only on one branch dies on the other edge, even when
        // that edge contains no read at which to insert a last-use clear.
        // Split only edges that need stores; normal and recovery CFG edges
        // otherwise keep their original identity. Suspend clears execute on
        // resume, after the scheduler has finished consuming its operands.
        if function.is_async {
            let targets: Vec<&mut BlockId> = match &mut block.terminator {
                SourceTerminator::Jump(target)
                | SourceTerminator::Suspend { resume: target, .. } => vec![target],
                SourceTerminator::Branch {
                    then_block,
                    else_block,
                    ..
                } => {
                    vec![then_block, else_block]
                }
                _ => Vec::new(),
            };
            for target in targets {
                let dead: Vec<u64> = live
                    .iter()
                    .zip(live_sets.live_in.row(target.0))
                    .zip(&clearable)
                    .map(|((live, next), clearable)| live & !next & clearable)
                    .collect();
                let cleared: Vec<_> = liveness::ones(&dead).collect();
                if !cleared.is_empty() {
                    let id = BlockId(original_blocks + edges.len());
                    edges.push(SourceBlock {
                        id,
                        instrs: vec![SourceInst::ClearScopeRoots { locals: cleared }],
                        terminator: SourceTerminator::Jump(*target),
                        recovery: Vec::new(),
                    });
                    live_sets.push_forwarding_block(target.0);
                    *target = id;
                }
            }
        }
        let mut reversed = Vec::with_capacity(block.instrs.len());
        for instruction in std::mem::take(&mut block.instrs).into_iter().rev() {
            read.clear();
            written.clear();
            instruction_use_def(&instruction, &names, &mut read, &mut written);
            let mut clear: Vec<_> = read
                .union(&written)
                .filter(|&&id| liveness::contains(&clearable, id) && !liveness::contains(&live, id))
                .copied()
                .collect();
            clear.sort_by_key(|id| id.0);
            if !clear.is_empty() {
                reversed.push(SourceInst::ClearScopeRoots { locals: clear });
            }
            for &id in &written {
                if !liveness::contains(&recovery_live, id) {
                    liveness::remove(&mut live, id);
                }
            }
            read.iter().for_each(|&id| liveness::insert(&mut live, id));
            reversed.push(instruction);
        }
        reversed.reverse();
        block.instrs = reversed;
    }
    function.blocks.extend(edges);
    live_sets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn async_source_root_clear_count_scales_with_last_uses() {
        for count in [1, 8, 64, 256] {
            let mut source = String::from("class Payload { pub value: i64; } async fn main() {");
            for i in 0..count {
                source.push_str(&format!(
                    "let p{i} = new Payload({i}); await yield(); println(p{i}.value);"
                ));
            }
            source.push_str("await yield(); }");
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (ast, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty(), "{errors:?}");
            let (hir, errors) = crate::ir::lower::lower_program(&ast);
            assert!(errors.is_empty(), "{errors:?}");
            let program = super::super::lower_source_program(&hir);
            let main = program
                .functions
                .iter()
                .find(|f| f.name.to_string() == "main")
                .unwrap();
            let source_roots: HashSet<_> = main
                .locals
                .iter()
                .filter(|local| !local.synthetic && local.name.starts_with('p'))
                .map(|local| local.id)
                .collect();
            assert_eq!(source_roots.len(), count);
            let clears: usize = main
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .filter_map(|inst| match inst {
                    SourceInst::ClearScopeRoots { locals } => {
                        Some(locals.iter().filter(|id| source_roots.contains(id)).count())
                    }
                    _ => None,
                })
                .sum();
            // One last-use clear and the existing lexical close per binding.
            assert_eq!(clears, 2 * count);
            println!("async source roots={count} clear stores={clears}");
        }
    }
}
