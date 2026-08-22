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
            for successor in successors(&block.terminator) {
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
    for block in blocks {
        if let Terminator::Suspend { operation, .. } = &block.terminator {
            framed.extend(live_out[block.id.0].iter().copied());
            operation.collect_locals(&mut framed);
        }
        for inst in &block.instrs {
            match inst {
                LirInst::SelectInit { operations } => {
                    for operation in operations {
                        collect_select_locals(operation, &mut framed);
                        if let LirSelectOp::Timeout { deadline, .. } = operation {
                            framed.insert(*deadline);
                        }
                    }
                }
                LirInst::SelectProbe { operations, ready } => {
                    for operation in operations {
                        collect_select_locals(operation, &mut framed);
                    }
                    framed.extend(ready.iter().flatten().copied());
                }
                LirInst::SelectPick { chosen, .. } => {
                    framed.insert(*chosen);
                }
                LirInst::SelectUnregister { operations } => {
                    for operation in operations {
                        collect_select_locals(operation, &mut framed);
                    }
                }
                LirInst::SelectCommit { operation, success } => {
                    collect_select_locals(operation, &mut framed);
                    framed.insert(*success);
                    match operation {
                        LirSelectOp::Recv { binding, .. } | LirSelectOp::Join { binding, .. } => {
                            framed.extend(binding.iter().copied());
                        }
                        _ => {}
                    }
                }
                LirInst::Defer { body, .. } => {
                    let no_defs = HashSet::new();
                    match body {
                        super::LirDeferBody::Expr(expr) => {
                            collect_expr_uses(expr, &names, &mut framed, &no_defs)
                        }
                        super::LirDeferBody::Block(stmts) => {
                            for expr in stmts.iter().flat_map(|stmt| stmt.child_exprs()) {
                                collect_expr_uses(expr, &names, &mut framed, &no_defs);
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
                    collect_expr_uses(value, &names, &mut framed, &no_defs);
                }
                _ => {}
            }
        }
    }
    let slots: Vec<_> = locals
        .iter()
        .filter_map(|local| framed.contains(&local.id).then_some(local.id))
        .collect();
    let locals = slots
        .iter()
        .enumerate()
        .map(|(index, local)| (*local, FrameSlot { index }))
        .collect();
    LirAsyncFrameLayout { locals, slots }
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
            LirInst::SuperInit { args } => {
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
            LirInst::EnterDeferScope { .. }
            | LirInst::LeaveDeferScope { .. }
            | LirInst::FlushDefers { .. } => {}
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
    if let HirExprKind::Var(name) = &expr.kind
        && let Some(local) = names.get(name.as_str())
        && !defs.contains(local)
    {
        uses.insert(*local);
    }
    if !matches!(expr.kind, HirExprKind::Lambda { .. }) {
        for child in expr.children() {
            collect_expr_uses(child, names, uses, defs);
        }
    }
}

fn successors(terminator: &Terminator) -> Vec<BlockId> {
    match terminator {
        Terminator::Jump(target) => vec![*target],
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![*then_block, *else_block],
        Terminator::Suspend { resume, .. } => vec![*resume],
        Terminator::Return(_) => Vec::new(),
    }
}
