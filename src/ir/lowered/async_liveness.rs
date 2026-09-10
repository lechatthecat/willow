//! Async frame planning over the final LIR control-flow graph.
//!
//! This deliberately knows nothing about parser AST nodes or source spans.
//! Bindings are identified by [`LirLocalId`], including locals synthesized by
//! LIR lowering, and spans remain optional diagnostic metadata on `LirLocal`.

use std::collections::{HashMap, HashSet};

use crate::ir::typed_ast::{HirExpr, HirExprKind};

use super::{BlockId, LirBlock, LirInst, LirLocal, LirLocalId, LirSelectOp, Terminator};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSlot {
    pub index: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LirAsyncFrameLayout {
    pub locals: HashMap<LirLocalId, FrameSlot>,
    pub slots: Vec<LirLocalId>,
}

impl LirAsyncFrameLayout {
    pub fn slot(&self, local: LirLocalId) -> Option<FrameSlot> {
        self.locals.get(&local).copied()
    }
}

/// Compute the exact set of locals live across explicit LIR suspension edges.
pub fn analyze(blocks: &[LirBlock], locals: &[LirLocal]) -> LirAsyncFrameLayout {
    let names: HashMap<&str, LirLocalId> = locals
        .iter()
        .map(|local| (local.name.as_str(), local.id))
        .collect();
    let mut uses = vec![HashSet::new(); blocks.len()];
    let mut defs = vec![HashSet::new(); blocks.len()];
    for block in blocks {
        block_use_def(block, &names, &mut uses[block.id.0], &mut defs[block.id.0]);
    }

    let mut live_in = vec![HashSet::new(); blocks.len()];
    let mut live_out = vec![HashSet::new(); blocks.len()];
    loop {
        let mut changed = false;
        for block in blocks.iter().rev() {
            let mut out = HashSet::new();
            for successor in successors(block) {
                out.extend(live_in[successor.0].iter().copied());
            }
            let mut input = uses[block.id.0].clone();
            input.extend(out.difference(&defs[block.id.0]).copied());
            changed |= out != live_out[block.id.0] || input != live_in[block.id.0];
            live_out[block.id.0] = out;
            live_in[block.id.0] = input;
        }
        if !changed {
            break;
        }
    }

    let mut framed = HashSet::new();
    let mut pinned = HashSet::new();
    for block in blocks {
        if let Terminator::Suspend { operation, .. } = &block.terminator {
            framed.extend(live_out[block.id.0].iter().copied());
            operation.collect_locals(&mut pinned);
        }
        for inst in &block.instrs {
            match inst {
                LirInst::SelectInit { operations } => {
                    for operation in operations {
                        collect_select_locals(operation, &mut pinned);
                        if let LirSelectOp::Timeout { deadline, .. } = operation {
                            pinned.insert(*deadline);
                        }
                    }
                }
                LirInst::SelectProbe { operations, ready } => {
                    for operation in operations {
                        collect_select_locals(operation, &mut pinned);
                    }
                    pinned.extend(ready.iter().flatten().copied());
                }
                LirInst::SelectPick { chosen, .. } => {
                    pinned.insert(*chosen);
                }
                LirInst::SelectUnregister { operations } => {
                    for operation in operations {
                        collect_select_locals(operation, &mut pinned);
                    }
                }
                LirInst::SelectCommit { operation, success } => {
                    collect_select_locals(operation, &mut pinned);
                    pinned.insert(*success);
                    match operation {
                        LirSelectOp::Recv { binding, .. } | LirSelectOp::Join { binding, .. } => {
                            pinned.extend(binding.iter().copied());
                        }
                        _ => {}
                    }
                }
                LirInst::Defer { body, .. } => {
                    let no_defs = HashSet::new();
                    match body {
                        super::LirDeferBody::Expr(expr) => {
                            collect_expr_uses(expr, &names, &mut pinned, &no_defs)
                        }
                        super::LirDeferBody::Block(stmts) => {
                            for expr in stmts.iter().flat_map(|stmt| stmt.child_exprs()) {
                                collect_expr_uses(expr, &names, &mut pinned, &no_defs);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if block
            .instrs
            .iter()
            .any(|inst| matches!(inst, LirInst::FlushDefers { .. }))
        {
            let no_defs = HashSet::new();
            match &block.terminator {
                Terminator::Return(Some(value)) | Terminator::Branch { cond: value, .. } => {
                    collect_expr_uses(value, &names, &mut pinned, &no_defs);
                }
                _ => {}
            }
        }
    }
    framed.extend(pinned.iter().copied());
    pinned.extend(
        locals
            .iter()
            .filter(|local| local.parameter)
            .map(|local| local.id),
    );
    // A block is an indivisible interference region. Include writes and root
    // clears even when ordinary liveness does not consider them reads: either
    // can overwrite another logical local's physical frame slot.
    let mut regions = vec![HashSet::new(); locals.len()];
    for block in blocks {
        let mut touched = live_in[block.id.0].clone();
        touched.extend(&live_out[block.id.0]);
        touched.extend(&uses[block.id.0]);
        touched.extend(&defs[block.id.0]);
        for inst in &block.instrs {
            if let LirInst::ClearScopeRoots { locals: cleared } = inst {
                // Primitive locals have no root to clear; the backend emits
                // no store for them. Scope-exit lists include all source
                // locals, including scalars whose lifetimes already ended.
                touched.extend(
                    cleared
                        .iter()
                        .filter(|id| !reusable_scalar(&locals[id.0 as usize])),
                );
            }
        }
        for local in touched {
            regions[local.0 as usize].insert(block.id.0);
        }
    }
    let mut slots = Vec::new();
    let mut occupants: Vec<Vec<&LirLocal>> = Vec::new();
    let mut mapping = HashMap::new();
    for local in locals.iter().filter(|local| framed.contains(&local.id)) {
        // GC values and protocol/defer slots retain exclusive ownership. This
        // first tier only reuses primitive scalars with identical types.
        let reusable = !pinned.contains(&local.id) && reusable_scalar(local);
        let index = reusable
            .then(|| {
                occupants.iter().position(|members| {
                    members.iter().all(|other| {
                        !pinned.contains(&other.id)
                            && other.ty == local.ty
                            && regions[local.id.0 as usize]
                                .is_disjoint(&regions[other.id.0 as usize])
                    })
                })
            })
            .flatten()
            .unwrap_or_else(|| {
                slots.push(local.id);
                occupants.push(Vec::new());
                slots.len() - 1
            });
        occupants[index].push(local);
        mapping.insert(local.id, FrameSlot { index });
    }
    LirAsyncFrameLayout {
        locals: mapping,
        slots,
    }
}

fn reusable_scalar(local: &LirLocal) -> bool {
    matches!(
        local.ty,
        crate::parser::ast::Type::I64
            | crate::parser::ast::Type::F64
            | crate::parser::ast::Type::Bool
    )
}

fn collect_select_locals(operation: &LirSelectOp, out: &mut HashSet<LirLocalId>) {
    match operation {
        LirSelectOp::Recv {
            channel, binding, ..
        } => {
            out.insert(*channel);
            out.extend(binding.iter().copied());
        }
        LirSelectOp::Send { channel, value, .. } => {
            out.insert(*channel);
            out.insert(*value);
        }
        LirSelectOp::Join { task, binding, .. } => {
            out.insert(*task);
            out.extend(binding.iter().copied());
        }
        LirSelectOp::Timeout { millis, deadline } => {
            out.insert(*millis);
            out.insert(*deadline);
        }
        LirSelectOp::Default => {}
    }
}

fn block_use_def(
    block: &LirBlock,
    names: &HashMap<&str, LirLocalId>,
    uses: &mut HashSet<LirLocalId>,
    defs: &mut HashSet<LirLocalId>,
) {
    macro_rules! read {
        ($expr:expr) => {
            collect_expr_uses($expr, names, uses, defs)
        };
    }
    for inst in &block.instrs {
        match inst {
            LirInst::Let { local, value, .. } => {
                read!(value);
                defs.insert(*local);
            }
            LirInst::Assign { local, value, .. } => {
                read!(value);
                defs.insert(*local);
            }
            LirInst::FieldAssign { object, value, .. } => {
                read!(object);
                read!(value);
            }
            LirInst::IndexAssign {
                array,
                index,
                value,
            } => {
                read!(array);
                read!(index);
                read!(value);
            }
            LirInst::StaticFieldAssign { value, .. } | LirInst::Expr(value) => read!(value),
            LirInst::SuperInit { args, .. } => {
                for arg in args {
                    read!(arg);
                }
            }
            LirInst::Defer { body, .. } => match body {
                super::LirDeferBody::Expr(expr) => read!(expr),
                super::LirDeferBody::Block(stmts) => {
                    for expr in stmts.iter().flat_map(|stmt| stmt.child_exprs()) {
                        read!(expr);
                    }
                }
            },
            LirInst::SelectInit { operations } => {
                for operation in operations {
                    match operation {
                        LirSelectOp::Timeout { millis, deadline } => {
                            if !defs.contains(millis) {
                                uses.insert(*millis);
                            }
                            defs.insert(*deadline);
                        }
                        _ => select_uses(operation, uses, defs),
                    }
                }
            }
            LirInst::SelectProbe { operations, ready } => {
                for operation in operations {
                    select_uses(operation, uses, defs);
                }
                defs.extend(ready.iter().flatten().copied());
            }
            LirInst::SelectPick { ready, chosen } => {
                for local in ready.iter().flatten() {
                    if !defs.contains(local) {
                        uses.insert(*local);
                    }
                }
                defs.insert(*chosen);
            }
            LirInst::SelectUnregister { operations } => {
                for operation in operations {
                    select_uses(operation, uses, defs);
                }
            }
            LirInst::SelectCommit { operation, success } => {
                select_uses(operation, uses, defs);
                match operation {
                    LirSelectOp::Recv { binding, .. } | LirSelectOp::Join { binding, .. } => {
                        defs.extend(binding.iter().copied());
                    }
                    _ => {}
                }
                defs.insert(*success);
            }
            // Releasing reads all four of the acquisition's frame slots, so
            // each one stays live from the `lock` down to every exit that
            // leaves the section (willow-0g8j.2.13). A `lock` body's scope
            // reads them too: its panic cleanup is where an unwind releases.
            LirInst::ReleaseLock(slots)
            | LirInst::EnterDeferScope {
                lock: Some(slots), ..
            } => {
                for local in slots.locals() {
                    if !defs.contains(&local) {
                        uses.insert(local);
                    }
                }
            }
            // The scrutinee is READ by every dispatch block and by the bind at
            // the top of each arm; the bindings are DEFINED there. That is what
            // puts a binding an arm reads after suspending into the frame, and
            // keeps a scrutinee nothing reads again out of it
            // (willow-0g8j.2.11.1).
            LirInst::MatchTest {
                scrutinee, result, ..
            } => {
                if !defs.contains(scrutinee) {
                    uses.insert(*scrutinee);
                }
                defs.insert(*result);
            }
            LirInst::MatchBind {
                scrutinee,
                bindings,
                ..
            } => {
                if !defs.contains(scrutinee) {
                    uses.insert(*scrutinee);
                }
                defs.extend(bindings.iter().copied());
            }
            // Naming a local does not read it: the instruction drops the GC
            // root of a scope that ended, so nothing it names is live past it
            // and nothing it names is redefined either (willow-0g8j.3.3).
            LirInst::EnterDeferScope { .. }
            | LirInst::LeaveDeferScope { .. }
            | LirInst::FlushDefers { .. }
            | LirInst::ClearScopeRoots { .. } => {}
        }
    }
    match &block.terminator {
        Terminator::Branch { cond, .. } => read!(cond),
        Terminator::Return(Some(value)) => read!(value),
        Terminator::Suspend { operation, .. } => operation.collect_locals(uses),
        Terminator::Jump(_) | Terminator::Return(None) => {}
    }
}

fn select_uses(
    operation: &LirSelectOp,
    uses: &mut HashSet<LirLocalId>,
    defs: &HashSet<LirLocalId>,
) {
    let mut read = |local| {
        if !defs.contains(&local) {
            uses.insert(local);
        }
    };
    match operation {
        LirSelectOp::Recv { channel, .. } => read(*channel),
        LirSelectOp::Send { channel, value, .. } => {
            read(*channel);
            read(*value);
        }
        LirSelectOp::Join { task, .. } => read(*task),
        LirSelectOp::Timeout { deadline, .. } => read(*deadline),
        LirSelectOp::Default => {}
    }
}

fn collect_expr_uses(
    expr: &HirExpr,
    names: &HashMap<&str, LirLocalId>,
    uses: &mut HashSet<LirLocalId>,
    defs: &HashSet<LirLocalId>,
) {
    for expr in expr.walk_postorder(false) {
        if let HirExprKind::Var(name) = &expr.kind
            && let Some(local) = names.get(name.as_str())
            && !defs.contains(local)
        {
            uses.insert(*local);
        }
    }
}

/// Control-flow successors INCLUDING the panic edges: a value whose only later
/// use is after a recovered panic is still live here, and the whole point of
/// this pass is to decide what must survive a poll return.
fn successors(block: &LirBlock) -> Vec<BlockId> {
    let mut out = match &block.terminator {
        Terminator::Jump(target) => vec![*target],
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![*then_block, *else_block],
        Terminator::Suspend { resume, .. } => vec![*resume],
        Terminator::Return(_) => Vec::new(),
    };
    out.extend(block.recovery.iter().copied());
    out
}

#[cfg(test)]
mod coalescing_tests {
    use super::*;
    use crate::semantic::ids::SemanticType as Type;

    #[test]
    fn deep_local_use_collection_uses_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = crate::diagnostics::Span::new(0, 0, 1, 1);
                let mut expr = HirExpr {
                    kind: HirExprKind::Var("value".into()),
                    ty: Type::I64,
                    span,
                };
                for _ in 0..50_000 {
                    expr = HirExpr {
                        kind: HirExprKind::TryPropagate {
                            inner: Box::new(expr),
                        },
                        ty: Type::I64,
                        span,
                    };
                }
                let id = LirLocalId(0);
                let names = HashMap::from([("value", id)]);
                let mut uses = HashSet::new();
                collect_expr_uses(&expr, &names, &mut uses, &HashSet::new());
                assert_eq!(uses, HashSet::from([id]));
                uses.clear();
                collect_expr_uses(&expr, &names, &mut uses, &HashSet::from([id]));
                assert!(uses.is_empty());
                drop(expr);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn layout(source: &str) -> (LirAsyncFrameLayout, Vec<LirLocal>) {
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        let (hir, errors) = crate::ir::lower::lower_program(&ast);
        assert!(errors.is_empty(), "{errors:?}");
        let mut program = super::super::lower_program(&hir);
        let f = program.functions.remove(0);
        (f.async_frame, f.locals)
    }

    fn compare(source: &str, shared: bool) {
        let (layout, locals) = layout(source);
        let slot = |name: &str| {
            let local = locals.iter().find(|local| local.name == name).unwrap();
            layout
                .slot(local.id)
                .unwrap_or_else(|| panic!("missing frame local {name}: {source}"))
        };
        assert_eq!(slot("a") == slot("b"), shared, "{source}");
        for slot in layout.locals.values() {
            assert!(slot.index < layout.slots.len());
        }
    }

    #[test]
    fn scalar_lifetime_matrix() {
        // 21 perspectives: each primitive type exercises separated lifetimes,
        // simultaneous values, writes in a shared block, loop backedges,
        // parameter ownership, defer ownership, and deterministic planning.
        for (ty, a, b) in [
            ("i64", "1", "2"),
            ("f64", "1.0", "2.0"),
            ("bool", "true", "false"),
        ] {
            let separated = format!(
                "async fn f() {{ if true {{ let a: {ty} = {a}; await sleep(0); print(a); }} await sleep(0); if true {{ let b: {ty} = {b}; await sleep(0); print(b); }} }}"
            );
            compare(&separated, true);
            compare(
                &format!(
                    "async fn f() {{ let a: {ty} = {a}; let b: {ty} = {b}; await sleep(0); print(a); print(b); }}"
                ),
                false,
            );
            compare(
                &format!(
                    "async fn f() {{ let a: {ty} = {a}; await sleep(0); print(a); let b: {ty} = {b}; await sleep(0); print(b); }}"
                ),
                false,
            );
            compare(
                &format!(
                    "async fn f() {{ let a: {ty} = {a}; while true {{ let b: {ty} = {b}; await sleep(0); print(a); print(b); }} }}"
                ),
                false,
            );
            compare(
                &format!(
                    "async fn f(a: {ty}) {{ await sleep(0); print(a); await sleep(0); let b: {ty} = {b}; await sleep(0); print(b); }}"
                ),
                false,
            );
            compare(
                &format!(
                    "async fn f() {{ let a: {ty} = {a}; defer print(a); await sleep(0); print(a); let b: {ty} = {b}; await sleep(0); print(b); }}"
                ),
                false,
            );
            assert_eq!(layout(&separated).0, layout(&separated).0);
        }
    }

    #[test]
    fn different_types_and_gc_values_do_not_share() {
        compare(
            "async fn f() { if true { let a = 1; await sleep(0); print(a); } await sleep(0); if true { let b = true; await sleep(0); print(b); } }",
            false,
        );
        compare(
            "async fn f() { if true { let a = \"first\"; await sleep(0); print(a); } await sleep(0); if true { let b = \"second\"; await sleep(0); print(b); } }",
            false,
        );
    }
}
