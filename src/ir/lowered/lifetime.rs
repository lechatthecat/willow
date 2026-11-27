//! End root ownership at last use. Async source bindings must also release
//! their frame slots before suspending after their final read.
use super::async_liveness::{clear_local_scratch, instruction_use_def, terminator_uses};
use super::liveness::{self, Liveness};
use super::{
    BlockId, LirOperand, LirRvalue, SourceBlock, SourceFunction, SourceInst, SourceTerminator,
};
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
                match instruction {
                    SourceInst::Defer { body, .. } => {
                        clear_function(body.function.as_mut());
                        functions.push(body.function.as_mut());
                    }
                    ir_variants!(
                        SourceInst;
                        Compute, Let, Assign, EnterDeferScope, LeaveDeferScope, FlushDefers, RestoreCallPreparation,
                        ClearScopeRoots, FieldAssign, IndexAssign, StaticFieldAssign, SuperInit,
                        SelectInit, SelectProbe, SelectPick, SelectUnregister, SelectCommit,
                        ReleaseLock, MatchTest, MatchBind, Expr
                    ) => {}
                }
            }
        }
    }
    root
}

/// Arguments whose referenced slots can still be read by a child task after
/// the instruction returns. A synchronous call finishes borrowing at the call.
fn task_arguments(value: &LirRvalue) -> &[LirOperand] {
    match value {
        LirRvalue::StartTask { args, .. }
        | LirRvalue::MethodCall {
            args,
            is_async: true,
            ..
        }
        | LirRvalue::StaticCall {
            args,
            is_async: true,
            ..
        } => args,
        LirRvalue::MethodCall {
            is_async: false, ..
        }
        | LirRvalue::StaticCall {
            is_async: false, ..
        }
        | ir_variants!(
            LirRvalue;
            BeginReferenceCall, Use, AwaitFuture, SelectIdleWait, Coerce, ArrayAlloc,
            CaptureArrayOwner, ArrayStore, Index, ObjectAlloc, FieldLoad, FieldStore,
            StaticField, StaticStore, EnumAlloc, EnumPayloadStore, ConstructorCall,
            Range, EnumMethod, BuiltinCall, FormatScalar, Panic, Recover, ReferenceDebug,
            RebindResultError, IntoError, PrepareMethod, StringLiteral, FunctionRef,
            Closure, Print, IntrinsicCall, DirectCall, IndirectCall, Unary, Binary, StringConcat
        ) => &[],
    }
}

fn clear_function(function: &mut SourceFunction) -> Liveness {
    let names: HashMap<_, _> = function
        .locals
        .iter()
        .map(|local| (local.name.as_str(), local.id))
        .collect();
    // Cleanup reads captured slots even when cancellation has no normal CFG
    // exit (for example, an infinite loop while holding a lock).
    let mut captured = HashSet::new();
    for instruction in function.blocks.iter().flat_map(|block| &block.instrs) {
        match instruction {
            SourceInst::Defer { body, .. } => captured.extend(body.captures.iter().copied()),
            SourceInst::EnterDeferScope {
                lock: Some(slots), ..
            } => {
                captured.extend(slots.all_locals());
            }
            SourceInst::Compute { value, .. } => {
                // Task-borrowed slots retain their value until lexical scope exit.
                captured.extend(
                    task_arguments(value)
                        .iter()
                        .filter_map(LirOperand::borrowed_local),
                );
            }
            SourceInst::EnterDeferScope { lock: None, .. }
            | ir_variants!(
                SourceInst;
                Let, Assign, LeaveDeferScope, FlushDefers, RestoreCallPreparation, ClearScopeRoots, FieldAssign,
                IndexAssign, StaticFieldAssign, SuperInit, SelectInit, SelectProbe, SelectPick,
                SelectUnregister, SelectCommit, ReleaseLock, MatchTest, MatchBind, Expr
            ) => {}
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
        clear_local_scratch(&mut read);
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
                SourceTerminator::Return(_) | SourceTerminator::CleanupReturn => Vec::new(),
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
            clear_local_scratch(&mut read);
            clear_local_scratch(&mut written);
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
    fn wide_selects_bound_scratch_work_across_narrow_instructions() {
        use super::super::async_liveness::SCRATCH_CAPACITY;
        use super::super::{LirLocal, LirLocalId, LirSelectOp, LirStorageKind};
        use crate::ir::optimize::{
            devirtualize_function_values, eliminate_dead_values, scalar_replace_objects,
        };
        use crate::semantic::ids::SemanticType as Type;

        let tokens = crate::lexer::Lexer::new("fn main() {}").tokenize().unwrap();
        let (ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let (hir, errors) = crate::ir::lower::lower_program(&ast);
        assert!(errors.is_empty());
        let mut function = super::super::lower_source_program(&hir)
            .functions
            .into_iter()
            .find(|f| f.name.is_free_named("main"))
            .unwrap();
        for (width, narrow) in [(32, 128), (256, 128), (256, 1024), (1024, 2048)] {
            let timer = LirLocalId(width);
            let chosen = LirLocalId(width + 1);
            function.locals = (0..=chosen.0)
                .map(|i| LirLocal {
                    id: LirLocalId(i),
                    name: format!("l{i}"),
                    ty: if i < width { Type::Bool } else { Type::I64 },
                    storage_kind: LirStorageKind::Value,
                    synthetic: false,
                    parameter: i == width,
                    source_span: None,
                })
                .collect();
            let ready: Vec<_> = (0..width).map(|i| Some(LirLocalId(i))).collect();
            // Forward passes see the wide definition first. The reverse
            // lifetime walk sees the wide read before the same narrow run.
            let mut instrs = vec![SourceInst::SelectProbe {
                operations: vec![
                    LirSelectOp::Timeout {
                        millis: timer,
                        deadline: timer
                    };
                    width as usize
                ],
                ready: ready.clone(),
            }];
            instrs.extend((0..narrow).map(|_| SourceInst::SelectPick {
                ready: vec![Some(LirLocalId(0))],
                chosen,
            }));
            instrs.push(SourceInst::SelectPick { ready, chosen });
            function.blocks = vec![SourceBlock {
                id: BlockId(0),
                instrs,
                terminator: SourceTerminator::Return(None),
                recovery: vec![],
            }];
            // Sum the actual capacities of populated scratch tables as they
            // are reset. Every pass makes a fixed number of scans per table.
            let check = |pass, run: &mut dyn FnMut()| {
                SCRATCH_CAPACITY.set(0);
                run();
                let capacity = SCRATCH_CAPACITY.get();
                assert!(capacity >= narrow, "{pass} did not measure the narrow run");
                assert!(
                    capacity <= 96 * (width as usize + narrow),
                    "{pass}: {capacity}"
                );
                println!("{pass}, width={width}, narrow={narrow}, capacity={capacity}");
            };
            check("scalar replacement", &mut || {
                scalar_replace_objects(&mut function.blocks, &function.locals)
            });
            check("DCE including scalar replacement", &mut || {
                eliminate_dead_values(&mut function.blocks, &function.locals)
            });
            check("devirtualization", &mut || {
                devirtualize_function_values(&mut function, &HashMap::new())
            });
            check("last use including block liveness", &mut || {
                clear_function(&mut function);
            });
            assert_eq!(function.blocks[0].instrs.len(), narrow + 2);
        }
    }

    #[test]
    fn async_method_effect_cache_scales_with_inherited_calls() {
        use super::super::{
            LoweringDeclarations,
            declarations::{METHOD_ANCESTRY_PROBES, METHOD_CACHE_PROBES},
        };
        use crate::ir::typed_ast::{HirClassInfo, HirResolution, HirSignature};
        use crate::semantic::ids::{SemanticType as Type, TypeId};
        for depth in [1, 8, 64, 128] {
            let mut resolution = HirResolution::default();
            let classes: Vec<_> = (0..depth).map(|i| TypeId::local(format!("C{i}"))).collect();
            for (i, &class) in classes.iter().enumerate() {
                let mut info = HirClassInfo::default();
                if i == 0 {
                    info.methods.insert(
                        "read".into(),
                        HirSignature {
                            params: vec![],
                            param_modes: vec![],
                            return_type: Type::String,
                            is_static: false,
                            is_async: true,
                        },
                    );
                } else {
                    info.base = Some(classes[i - 1]);
                }
                resolution.classes.insert(class, info);
            }
            let declarations = LoweringDeclarations::new(&resolution);
            METHOD_ANCESTRY_PROBES.set(0);
            METHOD_CACHE_PROBES.set(0);
            assert!(super::super::method_is_async(
                &declarations,
                &Type::Named(classes[depth - 1]),
                "read",
            ));
            for _ in 0..8 {
                for &class in &classes {
                    assert!(super::super::method_is_async(
                        &declarations,
                        &Type::Named(class),
                        "read"
                    ));
                }
            }
            assert_eq!(METHOD_ANCESTRY_PROBES.get(), depth);
            let probes = METHOD_CACHE_PROBES.get();
            let cached = declarations.methodless_chain_entries();
            assert_eq!(cached, depth + 1);
            assert_eq!(probes, 9 * depth);
            println!(
                "inheritance depth={depth} repeated calls={} method index entries={cached} probes={probes}",
                8 * depth
            );
            for fan_out in [false, true] {
                for async_leaf in [false, true] {
                    let mut resolution = resolution.clone();
                    resolution
                        .classes
                        .get_mut(&classes[0])
                        .unwrap()
                        .methods
                        .get_mut("read")
                        .unwrap()
                        .is_async = false;
                    if async_leaf {
                        let mut signature = resolution.classes[&classes[0]].methods["read"].clone();
                        signature.is_async = true;
                        resolution
                            .classes
                            .get_mut(&classes[depth - 1])
                            .unwrap()
                            .methods
                            .insert("read".into(), signature);
                    }
                    if fan_out {
                        for &child in &classes[1..] {
                            resolution.classes.get_mut(&child).unwrap().base = Some(classes[0]);
                        }
                    }
                    let declarations = LoweringDeclarations::new(&resolution);
                    let mut effects = super::super::dispatch::DispatchEffects::default();
                    for _ in 0..8 {
                        for (i, &class) in classes.iter().enumerate() {
                            let result = super::super::method_is_async(
                                &declarations,
                                &Type::Named(class),
                                "read",
                            ) || effects.may_start_task(&declarations, class, "read");
                            assert_eq!(
                                result,
                                async_leaf && (!fan_out || i == 0 || i == depth - 1)
                            );
                        }
                    }
                    // An exact async hit on a singleton needs no descendant query.
                    assert_eq!(
                        effects.visits,
                        if depth == 1 && async_leaf { 0 } else { depth }
                    );
                }
            }
        }
    }

    #[test]
    fn async_method_gc_borrow_clear_counts() {
        for flavor in [
            "sync",
            "async",
            "sync_task",
            "public_closed_task",
            "public_open_task",
        ] {
            let asynchronous = flavor == "async";
            for count in [1, 8, 64, 128] {
                let modifier = if asynchronous { "async " } else { "" };
                let return_ty = if flavor.ends_with("_task") {
                    "Task<String>"
                } else {
                    "String"
                };
                let returned = if flavor.ends_with("_task") {
                    "value(x)"
                } else {
                    "x"
                };
                let class_modifier = match flavor {
                    "public_closed_task" => "pub ",
                    "public_open_task" => "pub open ",
                    _ => "",
                };
                let mut source = format!(
                    "async fn value(x: String) -> String {{ return x; }} {class_modifier}class Reader {{ pub {modifier}fn read(self, x: &String) -> {return_ty} {{ return {returned}; }} }} async fn main() {{ let reader = new Reader();"
                );
                for i in 0..count {
                    source.push_str(&format!(
                        "let p{i}: String = \"value\"; let t{i} = reader.read(&p{i});"
                    ));
                }
                for i in 0..count {
                    let await_op = if flavor != "sync" { "await " } else { "" };
                    source.push_str(&format!("let r{i} = {await_op}t{i}; println(r{i});"));
                }
                source.push('}');
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
                let slots: HashSet<_> = main
                    .locals
                    .iter()
                    .filter(|local| !local.synthetic && local.name.starts_with('p'))
                    .map(|local| local.id)
                    .collect();
                assert_eq!(slots.len(), count);
                let mut clears = HashMap::new();
                for inst in main.blocks.iter().flat_map(|block| &block.instrs) {
                    if let SourceInst::ClearScopeRoots { locals } = inst {
                        for local in locals.iter().filter(|id| slots.contains(id)) {
                            *clears.entry(*local).or_insert(0) += 1;
                        }
                    }
                }
                assert_eq!(clears.len(), count);
                let expected = if asynchronous || flavor == "public_open_task" {
                    1
                } else {
                    2
                };
                assert!(
                    clears.values().all(|&n| n == expected),
                    "async={asynchronous}: {clears:?}"
                );
                println!(
                    "method={flavor} borrowed slots={count} clear stores={}",
                    count * expected
                );
            }
        }
    }

    #[test]
    fn async_borrowed_gc_slots_clear_only_at_lexical_exit() {
        for count in [1, 8, 64, 128] {
            let mut source = String::from(
                "async fn read(x: &String) -> String { await sleep(1); return x; } async fn main() {",
            );
            for i in 0..count {
                source.push_str(&format!(
                    "let p{i}: String = \"value\"; let t{i} = read(&p{i});"
                ));
            }
            for i in 0..count {
                source.push_str(&format!("let r{i} = await t{i}; println(r{i});"));
            }
            source.push('}');
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
            let slots: HashSet<_> = main
                .locals
                .iter()
                .filter(|local| !local.synthetic && local.name.starts_with('p'))
                .map(|local| local.id)
                .collect();
            assert_eq!(slots.len(), count);
            let mut clears = HashMap::new();
            for inst in main.blocks.iter().flat_map(|block| &block.instrs) {
                if let SourceInst::ClearScopeRoots { locals } = inst {
                    for local in locals.iter().filter(|id| slots.contains(id)) {
                        *clears.entry(*local).or_insert(0) += 1;
                    }
                }
            }
            assert_eq!(clears.len(), count);
            assert!(clears.values().all(|&count| count == 1), "{clears:?}");
            println!("borrowed GC slots={count} lexical clears={count} premature clears=0");
        }
    }

    #[test]
    fn synchronous_gc_borrow_keeps_last_use_clearing() {
        let source = "fn read(x: &String) -> String { return x; } async fn main() { let p: String = \"value\"; let r = read(&p); println(r); await sleep(1); }";
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
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
        let p = main
            .locals
            .iter()
            .find(|local| local.name == "p")
            .unwrap()
            .id;
        let clears = main.blocks.iter().flat_map(|block| &block.instrs).filter(|inst| matches!(inst, SourceInst::ClearScopeRoots { locals } if locals.contains(&p))).count();
        assert_eq!(clears, 2, "synchronous call must not extend root lifetime");
    }

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
