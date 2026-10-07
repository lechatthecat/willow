//! Array temporaries that need no shadow-stack root (willow-nzsg).
//!
//! Lowering copies a method receiver into a synthetic temporary, so
//! `while i < values.len()` evaluates `t = values; t.len()` on every trip.
//! A GC-managed temporary normally gets a rooted stack slot that is stored,
//! reloaded and nulled again, and those stores also keep Cranelift from
//! forwarding the owner and length loads to the loop body.
//!
//! A root is only needed while a collection can move the referent, that is,
//! across a GC point. A temporary qualifies for a plain SSA value when:
//!
//! * it is synthetic, typed `Array<T>`/`FrozenArray<T>`, and written by exactly
//!   one `Compute`;
//! * every read is a later instruction of the same block that only inspects
//!   the array (`len`, a scalar-element index, or a scalar-element store);
//! * every instruction from the write up to its last read is GC-free on all
//!   paths that continue: scalar arithmetic, copies, scalar array reads,
//!   scalar array stores and lengths, and root clears. Faults in them
//!   (bounds, null, overflow, zero divisor) raise and never rejoin, so a
//!   recovered panic resumes in another block, where the temporary is dead.
//!
//! The block-entry safepoint precedes every instruction of the block. Only
//! synchronous bodies use this: a cooperative body may suspend at a task
//! boundary inside a block.
//!
//! The same GC-free runs let codegen reuse what an earlier instruction of
//! the run loaded ([`LirReuse`]): no collection can move an array or change
//! its header inside the run, so the owner read from a root slot, the length
//! and the buffer pointer stay valid until a slot store or the run ends. A
//! run continues into a block whose only predecessor was just emitted and
//! that has no entry poll, so a loop header's poll always starts a new one.
//!
//! Cost: one pass over instructions and operands plus one prefix count per
//! block, so O(instructions + operands) per function.

use crate::ir::lowered::{LirFunction, LirInst, LirLocalId, LirOperand, LirRvalue, Terminator};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::ids::SemanticType as Type;
use crate::semantic::intrinsics::Intrinsic;
use cranelift_codegen::ir::{StackSlot, Value};
use std::collections::{HashMap, HashSet};

/// Facts established by the current GC-free run (see the module docs).
#[derive(Clone, Default)]
pub(super) struct LirReuse {
    /// Array owner last loaded from a root slot; a store to the slot drops it.
    pub(super) slots: HashMap<StackSlot, Value>,
    /// Header of a non-null array handle.
    pub(super) arrays: HashMap<Value, ArrayFacts>,
}

impl LirReuse {
    /// Facts carried to a successor are copied once per edge, so only a
    /// bounded set travels: a long GC-free chain cannot make the copies
    /// quadratic.
    pub(super) fn carriable(&self) -> bool {
        const CARRY_LIMIT: usize = 16;
        self.slots.len() + self.arrays.len() <= CARRY_LIMIT
    }
}

#[derive(Clone, Copy)]
pub(super) struct ArrayFacts {
    /// Validated as not negative.
    pub(super) len: Value,
    pub(super) buffer: Option<Value>,
}

#[derive(Clone, Copy)]
enum State {
    Unseen,
    Defined {
        block: usize,
        at: usize,
        last_read: usize,
    },
    Rejected,
}

pub(super) fn unrooted_array_temps(f: &LirFunction) -> HashSet<LirLocalId> {
    let mut states: Vec<State> = f
        .locals
        .iter()
        .map(|local| {
            if local.synthetic && !local.parameter && !local.is_gc_owner() && is_array(&local.ty) {
                State::Unseen
            } else {
                State::Rejected
            }
        })
        .collect();
    let reject = |states: &mut Vec<State>, local: LirLocalId| {
        if let Some(state) = states.get_mut(local.0 as usize) {
            *state = State::Rejected;
        }
    };
    // Prefix counts of instructions that may reach a GC point, per block.
    let mut gc_points: Vec<Vec<u32>> = Vec::with_capacity(f.blocks.len());
    for (block_index, block) in f.blocks.iter().enumerate() {
        let mut prefix = Vec::with_capacity(block.instrs.len() + 1);
        let mut count = 0;
        prefix.push(count);
        for (at, inst) in block.instrs.iter().enumerate() {
            count += u32::from(!gc_free(f, inst));
            prefix.push(count);
            if let LirInst::Compute { local, value, .. } = inst {
                let inspected = inspected_array(value);
                for operand in value.operands() {
                    for id in operand.locals() {
                        let direct = matches!(operand, LirOperand::Local(_));
                        match states.get(id.0 as usize).copied() {
                            Some(State::Defined { block, at: def, .. })
                                if direct && block == block_index && inspected == Some(id) =>
                            {
                                states[id.0 as usize] = State::Defined {
                                    block,
                                    at: def,
                                    last_read: at,
                                };
                            }
                            _ => reject(&mut states, id),
                        }
                    }
                }
                states[local.0 as usize] = match states[local.0 as usize] {
                    State::Unseen => State::Defined {
                        block: block_index,
                        at,
                        last_read: at,
                    },
                    _ => State::Rejected,
                };
                continue;
            }
            for id in inst_locals(inst) {
                reject(&mut states, id);
            }
        }
        for id in terminator_locals(&block.terminator) {
            reject(&mut states, id);
        }
        gc_points.push(prefix);
    }
    states
        .iter()
        .enumerate()
        .filter_map(|(index, state)| match *state {
            State::Defined {
                block,
                at,
                last_read,
            } if gc_points[block][last_read + 1] == gc_points[block][at + 1] => {
                Some(f.locals[index].id)
            }
            _ => None,
        })
        .collect()
}

/// The predecessor a block can inherit a run from: its only incoming edge,
/// when that edge is a plain `Jump`/`Branch` edge. Recovery and resume edges,
/// and the cleanup continuations of a function with defers, enter blocks
/// outside the LIR terminators, so such functions inherit nothing. O(N + E).
pub(super) fn sole_predecessors(f: &LirFunction) -> Vec<Option<usize>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Pred {
        Zero,
        One(usize),
        Many,
    }
    let mut preds = vec![Pred::Zero; f.blocks.len()];
    let defers = f.blocks.iter().any(|block| {
        !block.recovery.is_empty()
            || block.instrs.iter().any(|inst| {
                matches!(
                    inst,
                    LirInst::EnterDeferScope { .. }
                        | LirInst::LeaveDeferScope { .. }
                        | LirInst::Defer { .. }
                        | LirInst::FlushDefers { .. }
                )
            })
    });
    if !defers {
        for (index, block) in f.blocks.iter().enumerate() {
            for target in super::lir_gen::lir_block_successors(block) {
                preds[target] = match preds[target] {
                    Pred::Zero => Pred::One(index),
                    _ => Pred::Many,
                };
            }
        }
    }
    preds
        .into_iter()
        .map(|pred| match pred {
            Pred::One(index) => Some(index),
            Pred::Zero | Pred::Many => None,
        })
        .collect()
}

pub(super) fn is_array(ty: &Type) -> bool {
    matches!(ty, Type::Array(_)) || builtin_types::unary_arg(ty, B::FrozenArray).is_some()
}

fn scalar(ty: &Type) -> bool {
    matches!(ty, Type::I64 | Type::F64 | Type::Bool)
}

/// The array an rvalue accesses only through the inline, nonallocating
/// access path of `emit_array_access`: a length, a scalar-element read, or a
/// scalar-element store (willow-ijui.10).
fn inspected_array(value: &LirRvalue) -> Option<LirLocalId> {
    match value {
        LirRvalue::IntrinsicCall {
            intrinsic: Intrinsic::ArrayLen | Intrinsic::FrozenArrayLen,
            receiver: LirOperand::Local(array),
            ..
        } => Some(*array),
        LirRvalue::Index {
            array: LirOperand::Local(array),
            element,
            ..
        } if scalar(element) => Some(*array),
        // A scalar element store writes one buffer word: no barrier, no
        // allocation, and the header (owner, length, buffer) is unchanged.
        LirRvalue::ArrayStore {
            array: LirOperand::Local(array),
            element,
            ..
        } if scalar(element) => Some(*array),
        _ => None,
    }
}

/// Whether every continuing path of `inst` is free of GC points. A copy
/// into an interface-typed local boxes, so only scalar and array copies count.
pub(super) fn gc_free(f: &LirFunction, inst: &LirInst) -> bool {
    let plain = |operand: &LirOperand| !matches!(operand, LirOperand::Reference { .. });
    let copyable = |ty: &Type| scalar(ty) || is_array(ty);
    match inst {
        LirInst::Compute { value, .. } => match value {
            LirRvalue::Use(operand) => plain(operand),
            LirRvalue::Binary { operand_ty, .. } => scalar(operand_ty),
            value => inspected_array(value).is_some(),
        },
        LirInst::Let { ty, value, .. } => plain(value) && copyable(ty),
        LirInst::Assign { local, value, .. } => {
            plain(value) && copyable(&f.locals[local.0 as usize].ty)
        }
        LirInst::ClearScopeRoots { .. } => true,
        _ => false,
    }
}

/// Every local an instruction other than `Compute` names, except the
/// root clears, which only null a slot.
fn inst_locals(inst: &LirInst) -> Vec<LirLocalId> {
    use crate::ir::lowered::LirSelectOp as Op;
    let select = |op: &Op, out: &mut Vec<LirLocalId>| match op {
        Op::Recv {
            channel, binding, ..
        } => out.extend(std::iter::once(*channel).chain(*binding)),
        Op::Send { channel, value, .. } => out.extend([*channel, *value]),
        Op::Join { task, binding, .. } => out.extend(std::iter::once(*task).chain(*binding)),
        Op::Timeout { millis, deadline } => out.extend([*millis, *deadline]),
        Op::Default => {}
    };
    let mut out = Vec::new();
    match inst {
        LirInst::Compute { .. } | LirInst::ClearScopeRoots { .. } => {}
        LirInst::Let { local, value, .. } | LirInst::Assign { local, value, .. } => {
            out.push(*local);
            out.extend(value.locals());
        }
        LirInst::EnterDeferScope { lock, .. } => {
            out.extend(lock.iter().flat_map(|slots| slots.locals()));
        }
        LirInst::ReleaseLock(slots) => out.extend(slots.locals()),
        LirInst::Defer { body, .. } => out.extend(body.captures.iter().copied()),
        LirInst::SelectInit { operations } | LirInst::SelectProbe { operations, .. } => {
            for op in operations {
                select(op, &mut out);
            }
            if let LirInst::SelectProbe { ready, .. } = inst {
                out.extend(ready.iter().flatten().copied());
            }
        }
        LirInst::SelectPick { ready, chosen } => {
            out.extend(ready.iter().flatten().copied());
            out.push(*chosen);
        }
        LirInst::SelectUnregister { operations, winner } => {
            for op in operations {
                select(op, &mut out);
            }
            out.push(*winner);
        }
        LirInst::SelectCommit { operation, success } => {
            select(operation, &mut out);
            out.push(*success);
        }
        LirInst::MatchTest {
            scrutinee, result, ..
        } => out.extend([*scrutinee, *result]),
        LirInst::MatchBind {
            scrutinee,
            bindings,
            ..
        } => {
            out.push(*scrutinee);
            out.extend(bindings.iter().copied());
        }
        LirInst::LeaveDeferScope { .. }
        | LirInst::FlushDefers { .. }
        | LirInst::Unsupported { .. } => {}
    }
    out
}

fn terminator_locals(terminator: &Terminator) -> Vec<LirLocalId> {
    match terminator {
        Terminator::Branch { cond, .. } | Terminator::Return(Some(cond)) => cond.locals(),
        Terminator::Suspend { operation, .. } => {
            let mut out = HashSet::new();
            operation.collect_locals(&mut out);
            out.into_iter().collect()
        }
        Terminator::Jump(_) | Terminator::Return(None) | Terminator::CleanupReturn => Vec::new(),
    }
}
