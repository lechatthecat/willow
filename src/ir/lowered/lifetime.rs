//! End root ownership at last use. Async source bindings must also release
//! their frame slots before suspending after their final read.
use super::async_liveness::{instruction_use_def, terminator_uses};
use super::{BlockId, LirLocalId, SourceBlock, SourceFunction, SourceInst, SourceTerminator};
use std::collections::{HashMap, HashSet};

pub(super) fn clear_dead_temporaries(function: &mut SourceFunction) {
    let mut functions = vec![function];
    while let Some(function) = functions.pop() {
        clear_function(function);
        for block in &mut function.blocks {
            for instruction in &mut block.instrs {
                if let SourceInst::Defer { body, .. } = instruction {
                    functions.push(body.function.as_mut());
                }
            }
        }
    }
}

fn clear_function(function: &mut SourceFunction) {
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
    let clearable = |id: &LirLocalId| {
        function.locals.get(id.0 as usize).is_some_and(|local| {
            (local.synthetic || function.is_async)
                && !local.parameter
                && !captured.contains(id)
                && (local.storage_kind == super::LirStorageKind::GcOwner
                    || matches!(
                        local.ty,
                        crate::semantic::ids::SemanticType::String
                            | crate::semantic::ids::SemanticType::Array(_)
                            | crate::semantic::ids::SemanticType::Named(_)
                            | crate::semantic::ids::SemanticType::Generic(..)
                            | crate::semantic::ids::SemanticType::Closure(..)
                    ))
        })
    };
    let mut uses = vec![HashSet::new(); function.blocks.len()];
    let mut defs = uses.clone();
    let mut successors = vec![Vec::new(); function.blocks.len()];
    for (index, block) in function.blocks.iter().enumerate() {
        for instruction in &block.instrs {
            instruction_use_def(instruction, &names, &mut uses[index], &mut defs[index]);
        }
        terminator_uses(&block.terminator, &names, &mut uses[index], &defs[index]);
        successors[index] = match block.terminator {
            SourceTerminator::Jump(target) | SourceTerminator::Suspend { resume: target, .. } => {
                vec![target.0]
            }
            SourceTerminator::Branch {
                then_block,
                else_block,
                ..
            } => vec![then_block.0, else_block.0],
            _ => Vec::new(),
        };
        successors[index].extend(block.recovery.iter().map(|target| target.0));
    }
    let mut live_in = uses.clone();
    let mut live_out = vec![HashSet::new(); function.blocks.len()];
    loop {
        let mut changed = false;
        for index in (0..function.blocks.len()).rev() {
            let out: HashSet<_> = successors[index]
                .iter()
                .flat_map(|&target| live_in[target].iter().copied())
                .collect();
            let mut input = uses[index].clone();
            input.extend(out.difference(&defs[index]).copied());
            // A panic may transfer before any definition in this block. A
            // later assignment cannot kill the value that recovery would see.
            for target in &function.blocks[index].recovery {
                input.extend(&live_in[target.0]);
            }
            changed |= input != live_in[index] || out != live_out[index];
            live_in[index] = input;
            live_out[index] = out;
        }
        if !changed {
            break;
        }
    }
    let mut edges = Vec::new();
    let original_blocks = function.blocks.len();
    for (index, block) in function.blocks.iter_mut().enumerate() {
        let mut live = live_out[index].clone();
        let recovery_live: HashSet<_> = block
            .recovery
            .iter()
            .flat_map(|target| live_in[target.0].iter().copied())
            .collect();
        terminator_uses(&block.terminator, &names, &mut live, &HashSet::new());
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
                let mut cleared: Vec<_> = live
                    .difference(&live_in[target.0])
                    .filter(|id| clearable(id))
                    .copied()
                    .collect();
                cleared.sort_by_key(|id| id.0);
                if !cleared.is_empty() {
                    let id = BlockId(original_blocks + edges.len());
                    edges.push(SourceBlock {
                        id,
                        instrs: vec![SourceInst::ClearScopeRoots { locals: cleared }],
                        terminator: SourceTerminator::Jump(*target),
                        recovery: Vec::new(),
                    });
                    *target = id;
                }
            }
        }
        let mut reversed = Vec::with_capacity(block.instrs.len());
        for instruction in std::mem::take(&mut block.instrs).into_iter().rev() {
            let mut read = HashSet::new();
            let mut written = HashSet::new();
            instruction_use_def(&instruction, &names, &mut read, &mut written);
            let mut clear: Vec<_> = read
                .union(&written)
                .filter(|id| clearable(id) && !live.contains(id))
                .copied()
                .collect();
            clear.sort_by_key(|id| id.0);
            if !clear.is_empty() {
                reversed.push(SourceInst::ClearScopeRoots { locals: clear });
            }
            live.retain(|id| !written.contains(id));
            live.extend(&recovery_live);
            live.extend(read);
            reversed.push(instruction);
        }
        reversed.reverse();
        block.instrs = reversed;
    }
    function.blocks.extend(edges);
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
