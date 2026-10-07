//! Async frame planning over the final LIR control-flow graph.
//!
//! This deliberately knows nothing about parser AST nodes or source spans.
//! Bindings are identified by [`LirLocalId`], including locals synthesized by
//! LIR lowering, and spans remain optional diagnostic metadata on `LirLocal`.

use std::collections::{HashMap, HashSet};

use crate::ir::typed_ast::{HirExpr, HirExprKind};

use super::liveness::{self, Liveness};
use super::{LirLocal, LirLocalId, LirSelectOp, SourceBlock, SourceInst, SourceTerminator};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FrameSlot {
    pub index: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
#[cfg(test)]
pub(crate) fn analyze(blocks: &[SourceBlock], locals: &[LirLocal]) -> LirAsyncFrameLayout {
    analyze_with(blocks, locals, &Liveness::compute(blocks, locals))
}

/// Plan the frame from liveness already solved for exactly `blocks`.
pub(crate) fn analyze_with(
    blocks: &[SourceBlock],
    locals: &[LirLocal],
    liveness: &Liveness,
) -> LirAsyncFrameLayout {
    debug_assert_eq!(liveness.live_in.rows(), blocks.len());
    let names: HashMap<&str, LirLocalId> = locals
        .iter()
        .map(|local| (local.name.as_str(), local.id))
        .collect();
    let mut framed = HashSet::new();
    let mut pinned = HashSet::new();
    for block in blocks {
        if let SourceTerminator::Suspend { operation, .. } = &block.terminator {
            framed.extend(liveness.live_out.iter(block.id.0));
            operation.collect_locals(&mut pinned);
        }
        for inst in &block.instrs {
            match inst {
                SourceInst::SelectInit { operations } => {
                    for operation in operations {
                        collect_select_locals(operation, &mut pinned);
                        if let LirSelectOp::Timeout { deadline, .. } = operation {
                            pinned.insert(*deadline);
                        }
                    }
                }
                SourceInst::SelectProbe { operations, ready } => {
                    for operation in operations {
                        collect_select_locals(operation, &mut pinned);
                    }
                    pinned.extend(ready.iter().flatten().copied());
                }
                SourceInst::SelectPick { chosen, .. } => {
                    pinned.insert(*chosen);
                }
                SourceInst::SelectUnregister { operations, winner } => {
                    pinned.insert(*winner);
                    for operation in operations {
                        collect_select_locals(operation, &mut pinned);
                    }
                }
                SourceInst::SelectCommit { operation, success } => {
                    collect_select_locals(operation, &mut pinned);
                    pinned.insert(*success);
                    match operation {
                        LirSelectOp::Recv { binding, .. } | LirSelectOp::Join { binding, .. } => {
                            pinned.extend(binding.iter().copied());
                        }
                        _ => {}
                    }
                }
                SourceInst::Defer { body, .. } => pinned.extend(body.captures.iter().copied()),
                _ => {}
            }
        }
        if block
            .instrs
            .iter()
            .any(|inst| matches!(inst, SourceInst::FlushDefers { .. }))
        {
            let no_defs = HashSet::new();
            match &block.terminator {
                SourceTerminator::Return(Some(value))
                | SourceTerminator::Branch { cond: value, .. } => {
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
    // A block is an indivisible interference region. Include writes even when
    // ordinary liveness does not consider them reads: they can overwrite
    // another logical local's physical frame slot. Only framed scalars that
    // may share a slot need regions, and root clears never touch them:
    // primitive locals have no root, so the backend emits no store for a
    // scalar named in a scope-exit `ClearScopeRoots` list.
    let candidate = |local: &LirLocal| {
        framed.contains(&local.id) && !pinned.contains(&local.id) && reusable_scalar(local)
    };
    let mut candidates = vec![0u64; locals.len().div_ceil(64)];
    for local in locals.iter().filter(|local| candidate(local)) {
        liveness::insert(&mut candidates, local.id);
    }
    let mut regions = vec![Vec::new(); locals.len()];
    let mut touched = vec![0u64; candidates.len()];
    for block in blocks {
        let index = block.id.0;
        for (word, touched) in touched.iter_mut().enumerate() {
            *touched = candidates[word]
                & (liveness.live_in.row(index)[word]
                    | liveness.live_out.row(index)[word]
                    | liveness.uses.row(index)[word]
                    | liveness.defs.row(index)[word]);
        }
        for local in liveness::ones(&touched) {
            regions[local.0 as usize].push(index);
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
                            && reusable_scalar(other)
                            && other.ty == local.ty
                            && disjoint(
                                &regions[local.id.0 as usize],
                                &regions[other.id.0 as usize],
                            )
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
    !local.is_gc_owner()
        && matches!(
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

pub(crate) fn instruction_use_def(
    inst: &SourceInst,
    names: &HashMap<&str, LirLocalId>,
    uses: &mut HashSet<LirLocalId>,
    defs: &mut HashSet<LirLocalId>,
) {
    macro_rules! read {
        ($expr:expr) => {
            collect_expr_uses($expr, names, uses, defs)
        };
    }
    match inst {
        SourceInst::Compute { local, value, .. } => {
            for operand in value.operands() {
                for id in operand.locals() {
                    if !defs.contains(&id) {
                        uses.insert(id);
                    }
                }
            }
            defs.insert(*local);
        }
        SourceInst::Let { local, value, .. } => {
            read!(value);
            defs.insert(*local);
        }
        SourceInst::Assign { local, value, .. } => {
            read!(value);
            defs.insert(*local);
        }
        SourceInst::FieldAssign { object, value, .. } => {
            read!(object);
            read!(value);
        }
        SourceInst::IndexAssign {
            array,
            index,
            value,
        } => {
            read!(array);
            read!(index);
            read!(value);
        }
        SourceInst::StaticFieldAssign { value, .. } | SourceInst::Expr(value) => read!(value),
        SourceInst::SuperInit { args, .. } => {
            for arg in args {
                read!(arg);
            }
        }
        SourceInst::Defer { body, .. } => {
            for capture in &body.captures {
                if !defs.contains(capture) {
                    uses.insert(*capture);
                }
            }
        }
        SourceInst::SelectInit { operations } => {
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
        SourceInst::SelectProbe { operations, ready } => {
            for operation in operations {
                select_uses(operation, uses, defs);
            }
            defs.extend(ready.iter().flatten().copied());
        }
        SourceInst::SelectPick { ready, chosen } => {
            for local in ready.iter().flatten() {
                if !defs.contains(local) {
                    uses.insert(*local);
                }
            }
            defs.insert(*chosen);
        }
        SourceInst::SelectUnregister { operations, winner } => {
            if !defs.contains(winner) {
                uses.insert(*winner);
            }
            for operation in operations {
                select_uses(operation, uses, defs);
            }
        }
        SourceInst::SelectCommit { operation, success } => {
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
        SourceInst::ReleaseLock(slots)
        | SourceInst::EnterDeferScope {
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
        SourceInst::MatchTest {
            scrutinee, result, ..
        } => {
            if !defs.contains(scrutinee) {
                uses.insert(*scrutinee);
            }
            defs.insert(*result);
        }
        SourceInst::MatchBind {
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
        SourceInst::EnterDeferScope { .. }
        | SourceInst::LeaveDeferScope { .. }
        | SourceInst::FlushDefers { .. }
        | SourceInst::ClearScopeRoots { .. } => {}
    }
}

pub(crate) fn terminator_uses(
    terminator: &SourceTerminator,
    names: &HashMap<&str, LirLocalId>,
    uses: &mut HashSet<LirLocalId>,
    defs: &HashSet<LirLocalId>,
) {
    macro_rules! read {
        ($expr:expr) => {
            collect_expr_uses($expr, names, uses, defs)
        };
    }
    match terminator {
        SourceTerminator::Branch { cond, .. } => read!(cond),
        SourceTerminator::Return(Some(value)) => read!(value),
        SourceTerminator::Suspend { operation, .. } => operation.collect_locals(uses),
        SourceTerminator::Jump(_)
        | SourceTerminator::Return(None)
        | SourceTerminator::CleanupReturn => {}
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

/// Whether two ascending block-id lists share no block.
fn disjoint(left: &[usize], right: &[usize]) -> bool {
    let (mut l, mut r) = (0, 0);
    while l < left.len() && r < right.len() {
        match left[l].cmp(&right[r]) {
            std::cmp::Ordering::Less => l += 1,
            std::cmp::Ordering::Greater => r += 1,
            std::cmp::Ordering::Equal => return false,
        }
    }
    true
}

#[cfg(test)]
mod coalescing_tests {
    use super::*;
    use crate::semantic::ids::SemanticType as Type;

    #[test]
    fn async_frame_artifact_preserves_local_slot_mapping() {
        let frame = LirAsyncFrameLayout {
            locals: HashMap::from([
                (LirLocalId(3), FrameSlot { index: 0 }),
                (LirLocalId(9), FrameSlot { index: 1 }),
                (LirLocalId(17), FrameSlot { index: 0 }),
            ]),
            slots: vec![LirLocalId(3), LirLocalId(9)],
        };
        let wire = serde_json::to_vec(&frame).unwrap();
        let restored: LirAsyncFrameLayout = serde_json::from_slice(&wire).unwrap();
        assert_eq!(restored, frame);
    }

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
        let mut program = super::super::lower_source_program(&hir);
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
    fn scalar_cannot_reuse_an_opaque_gc_owner_slot() {
        let source = "async fn f() { if true { let a = 1; await sleep(0); print(a); } await sleep(0); if true { let b = 2; await sleep(0); print(b); } }";
        let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
        let (ast, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let (hir, errors) = crate::ir::lower::lower_program(&ast);
        assert!(errors.is_empty());
        let mut program = super::super::lower_source_program(&hir);
        let mut f = program.functions.remove(0);
        let a = f.locals.iter().position(|local| local.name == "a").unwrap();
        let b = f.locals.iter().position(|local| local.name == "b").unwrap();
        f.locals[a].storage_kind = super::super::LirStorageKind::GcOwner;
        let layout = analyze(&f.blocks, &f.locals);
        assert_ne!(layout.slot(f.locals[a].id), layout.slot(f.locals[b].id));
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
