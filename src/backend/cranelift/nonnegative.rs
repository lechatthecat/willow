//! Conservative induction proof (willow-ijui.12).
//!
//! A local is nonnegative at every read when every definition is a nonnegative
//! literal or a positive increment protected by a strict signed upper bound.
//! Only a body's sole incoming true edge is used; joins and longer dominance
//! paths deliberately keep the checks. For step=1 any i64 bound suffices:
//! i < bound <= MAX implies i + 1 <= MAX. Larger steps require a literal bound.
//! No fixed point over ranges or walk per loop: two instruction passes, one
//! edge pass, and a worklist visiting each copy dependency once. Bounded copy
//! peeling (eight links) keeps even adversarial copy chains linear.

use crate::ir::lowered::{
    LirFunction, LirInst, LirLocalId, LirOperand as O, LirRvalue as V, Terminator,
};
use crate::parser::ast::BinOp;
use crate::semantic::ids::SemanticType as Type;
use std::collections::HashMap;

#[derive(Clone, Copy)]
struct Def<'a> {
    block: usize,
    at: usize,
    value: &'a V,
}

#[derive(Default)]
pub(super) struct Proof {
    pub(super) locals: Vec<bool>,
    /// Deterministic work units, including operand/copy/edge visits.
    pub(super) work: usize,
}

impl Proof {
    pub(super) fn contains(&self, operand: &O) -> bool {
        match operand {
            O::Int(n) => *n >= 0,
            O::Local(id) => self.locals.get(id.0 as usize).copied().unwrap_or(false),
            _ => false,
        }
    }
}

pub(super) fn analyze(f: &LirFunction) -> Proof {
    let n = f.locals.len();
    let mut out = Proof {
        locals: vec![false; n],
        work: n,
    };
    // Cleanup/resume can introduce entries not described by normal CFG edges.
    if f.is_async
        || !f.async_frame.locals.is_empty()
        || !f.captures.is_empty()
        || f.blocks.iter().any(|b| {
            !b.recovery.is_empty()
                || b.instrs
                    .iter()
                    .any(|i| matches!(i, LirInst::Defer { .. } | LirInst::EnterDeferScope { .. }))
        })
    {
        return out;
    }
    let mut escaped = vec![false; n];
    let mut defs = vec![None; n];
    let mut counts = vec![0usize; n];
    let mut writes = HashMap::<(usize, LirLocalId), usize>::new();
    let mut preds = vec![None; f.blocks.len()];
    let mut edges = vec![0usize; f.blocks.len()];
    let mut allowed: Vec<_> = f
        .locals
        .iter()
        .map(|l| l.ty == Type::I64 && !l.parameter && !l.is_gc_owner())
        .collect();
    for (b, block) in f.blocks.iter().enumerate() {
        for (at, inst) in block.instrs.iter().enumerate() {
            out.work += 1;
            match inst {
                LirInst::Compute { local, value, .. } => {
                    counts[local.0 as usize] += 1;
                    defs[local.0 as usize] = Some(Def {
                        block: b,
                        at,
                        value,
                    });
                    *writes.entry((b, *local)).or_default() += 1;
                }
                LirInst::Let { local, .. } | LirInst::Assign { local, .. } => {
                    counts[local.0 as usize] += 1;
                    *writes.entry((b, *local)).or_default() += 1;
                }
                _ => {
                    for id in super::transient_roots::inst_locals(inst) {
                        out.work += 1;
                        allowed[id.0 as usize] = false;
                        escaped[id.0 as usize] = true;
                    }
                }
            }
        }
        for target in super::lir_gen::lir_block_successors(block) {
            out.work += 1;
            edges[target] += 1;
            preds[target] = Some(b);
        }
    }
    for (id, def) in defs.iter_mut().enumerate() {
        if counts[id] != 1 || !f.locals[id].synthetic {
            *def = None;
        }
    }
    // Peel only same-block, earlier, single-definition copies. A snapshot of
    // a mutable local is interchangeable with it only when that block does
    // not write it (header), or writes it just once at the increment (body).
    let peel = |mut operand: O, block: usize, mut before: usize, work: &mut usize| {
        for _ in 0..8 {
            *work += 1;
            let O::Local(id) = operand else {
                break;
            };
            let Some(def) = defs[id.0 as usize] else {
                break;
            };
            if def.block != block || def.at >= before {
                break;
            }
            let V::Use(next) = def.value else {
                break;
            };
            operand = next.clone();
            before = def.at;
        }
        operand
    };
    let mut guard = vec![None; f.blocks.len()];
    for (b, pred) in preds.iter().enumerate() {
        if edges[b] != 1 || b == 0 {
            continue;
        }
        let Some(p) = *pred else {
            continue;
        };
        let Terminator::Branch {
            cond: O::Local(cond),
            then_block,
            else_block,
        } = &f.blocks[p].terminator
        else {
            continue;
        };
        if then_block.0 != b || else_block.0 == b {
            continue;
        }
        let Some(def) = defs[cond.0 as usize] else {
            continue;
        };
        if def.block != p {
            continue;
        }
        let V::Binary {
            op: BinOp::Lt,
            lhs,
            rhs,
            operand_ty: Type::I64,
        } = def.value
        else {
            continue;
        };
        let O::Local(id) = peel(lhs.clone(), p, def.at, &mut out.work) else {
            continue;
        };
        if writes.contains_key(&(p, id)) {
            continue;
        }
        let bound = match peel(rhs.clone(), p, def.at, &mut out.work) {
            O::Int(n) => n,
            _ => i64::MAX,
        };
        guard[b] = Some((id, bound));
    }
    let mut initialized = vec![false; n];
    let mut copies = vec![Vec::new(); n];
    for (b, block) in f.blocks.iter().enumerate() {
        for (at, inst) in block.instrs.iter().enumerate() {
            out.work += 1;
            match inst {
                LirInst::Compute { local, value, .. } => {
                    // Every synthetic copy is a dependency; propagate once
                    // after all original locals have been accepted/rejected.
                    allowed[local.0 as usize] = false;
                    if counts[local.0 as usize] == 1 && f.locals[local.0 as usize].synthetic {
                        match value {
                            V::Use(O::Local(source)) => copies[source.0 as usize].push(*local),
                            V::Use(O::Int(n)) if *n >= 0 => out.locals[local.0 as usize] = true,
                            _ => {}
                        }
                    }
                    let mut operands = value.operands();
                    if let V::ReferenceDebug { argument, .. } = value {
                        operands.push(argument);
                    }
                    for operand in operands {
                        out.work += 1;
                        if matches!(operand, O::Reference { .. })
                            || matches!(value, V::Closure { .. })
                        {
                            for id in operand.locals() {
                                allowed[id.0 as usize] = false;
                                escaped[id.0 as usize] = true;
                                // Captured snapshots reject the originating induction local too.
                                if let O::Local(source) = peel(O::Local(id), b, at, &mut out.work) {
                                    allowed[source.0 as usize] = false;
                                    escaped[source.0 as usize] = true;
                                }
                            }
                        }
                    }
                }
                LirInst::Let { local, value, .. } | LirInst::Assign { local, value, .. } => {
                    let id = local.0 as usize;
                    if matches!(value, O::Int(n) if *n >= 0) {
                        initialized[id] = true;
                        continue;
                    }
                    let valid = (|| {
                        if writes.get(&(b, *local)) != Some(&1) {
                            return false;
                        }
                        let Some((guarded, bound)) = guard[b] else {
                            return false;
                        };
                        if guarded != *local {
                            return false;
                        }
                        let O::Local(temp) = value else {
                            return false;
                        };
                        let Some(def) = defs[temp.0 as usize] else {
                            return false;
                        };
                        if def.block != b || def.at >= at {
                            return false;
                        }
                        let V::Binary {
                            op: BinOp::Add,
                            lhs,
                            rhs: O::Int(step),
                            operand_ty: Type::I64,
                        } = def.value
                        else {
                            return false;
                        };
                        *step > 0
                            && bound <= i64::MAX - (*step - 1)
                            && peel(lhs.clone(), b, def.at, &mut out.work) == O::Local(*local)
                    })();
                    if !valid {
                        allowed[id] = false;
                    }
                }
                _ => {}
            }
        }
    }
    for id in 0..n {
        out.locals[id] = (out.locals[id] || allowed[id] && initialized[id]) && !escaped[id];
    }
    let mut pending: Vec<_> = (0..n).filter(|&id| out.locals[id]).collect();
    while let Some(id) = pending.pop() {
        out.work += 1;
        for copy in &copies[id] {
            out.work += 1;
            if !escaped[copy.0 as usize] && !out.locals[copy.0 as usize] {
                out.locals[copy.0 as usize] = true;
                pending.push(copy.0 as usize);
            }
        }
    }
    out
}

/// Reciprocal for a positive, non-power-of-two divisor and a 63-bit dividend.
/// With s=floor(log2(d)), m=ceil(2^(64+s)/d), the reciprocal error e satisfies
/// 0 < e < d < 2^(s+1). Thus n*e/2^(64+s) < 1 for n < 2^63:
/// floor(n*m/2^(64+s)) == floor(n/d). Full-width udiv needs a correction;
/// this restricted dividend does not. All construction arithmetic fits u128.
pub(super) fn reciprocal(divisor: u64) -> (u64, u32) {
    debug_assert!(divisor > 1 && divisor < 1 << 63 && !divisor.is_power_of_two());
    let shift = divisor.ilog2();
    let numerator = 1u128 << (64 + shift);
    (numerator.div_ceil(u128::from(divisor)) as u64, shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonnegative_reciprocal_matches_integer_division() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let divisors = (3..10000).chain([i64::MAX as u64, (1 << 62) + 1, (1 << 32) - 1]);
        for d in divisors.filter(|d| !d.is_power_of_two()) {
            let (m, shift) = reciprocal(d);
            let qmax = i64::MAX as u64 / d;
            let boundary = qmax * d;
            let mut inputs = vec![
                0,
                1,
                d - 1,
                d,
                d + 1,
                boundary - 1,
                boundary,
                i64::MAX as u64,
            ];
            for _ in 0..64 {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                inputs.push(seed >> 1);
            }
            for n in inputs.into_iter().filter(|n| *n <= i64::MAX as u64) {
                let q = ((u128::from(n) * u128::from(m)) >> (64 + shift)) as u64;
                assert_eq!(q, n / d, "n={n} d={d} m={m} shift={shift}");
                assert_eq!(n - q * d, n % d);
            }
        }
    }
}
