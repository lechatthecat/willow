//! Async frame planning over the final LIR control-flow graph.
//!
//! This deliberately knows nothing about parser AST nodes or source spans.
//! Bindings are identified by [`LirLocalId`], including locals synthesized by
//! LIR lowering, and spans remain optional diagnostic metadata on `LirLocal`.

use std::collections::{BTreeMap, HashMap, HashSet};

use crate::ir::typed_ast::{HirExpr, HirExprKind};

use super::liveness::{self, Liveness};
use super::{
    LirInst, LirLocal, LirLocalId, LirSelectOp, SourceBlock, SourceInst, SourceTerminator,
};

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

#[cfg(test)]
thread_local! {
    pub(crate) static SCRATCH_CAPACITY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SLOT_PROOF_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SLOT_BLOCK_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static BORROW_OPERAND_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
        match &block.terminator {
            SourceTerminator::Suspend { operation, .. } => {
                framed.extend(liveness.live_out.iter(block.id.0));
                operation.for_each_local(|id| {
                    pinned.insert(id);
                });
            }
            SourceTerminator::Jump(_)
            | SourceTerminator::Branch { .. }
            | SourceTerminator::Return(_)
            | SourceTerminator::CleanupReturn => {}
        }
        for inst in &block.instrs {
            match inst {
                SourceInst::Compute { value, .. } => {
                    // A reference is an address use, not just a value use.
                    // Its borrower can outlive the last direct read of the
                    // local. Give it an exclusive slot for the entire frame
                    // lifetime; never rematerialize or coalesce that storage.
                    for operand in value.iter_operands() {
                        #[cfg(test)]
                        BORROW_OPERAND_VISITS.with(|visits| visits.set(visits.get() + 1));
                        if let Some(local) = operand.borrowed_local() {
                            pinned.insert(local);
                        }
                    }
                }
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
                        LirSelectOp::Send { .. }
                        | LirSelectOp::Timeout { .. }
                        | LirSelectOp::Default => {}
                    }
                }
                SourceInst::Defer { body, .. } => pinned.extend(body.captures.iter().copied()),
                ir_variants!(
                    SourceInst;
                    EnterDeferScope, LeaveDeferScope, FlushDefers, ClearScopeRoots, Let, Assign,
                    FieldAssign, IndexAssign, StaticFieldAssign, SuperInit, ReleaseLock,
                    MatchTest, MatchBind, Expr
                ) => {}
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
                SourceTerminator::Jump(_)
                | SourceTerminator::Suspend { .. }
                | SourceTerminator::Return(_)
                | SourceTerminator::CleanupReturn => {}
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
    let mut mapping = HashMap::new();
    let mut by_type = HashMap::new();
    for local in locals.iter().filter(|local| framed.contains(&local.id)) {
        let index = if candidate(local) {
            let group = by_type
                .entry(&local.ty)
                .or_insert_with(ReusableSlots::default);
            group.assign(&regions[local.id.0 as usize], local.id, &mut slots)
        } else {
            slots.push(local.id);
            slots.len() - 1
        };
        mapping.insert(local.id, FrameSlot { index });
    }
    LirAsyncFrameLayout {
        locals: mapping,
        slots,
    }
}

/// Validate the non-moving local-slot invariant before async code generation.
/// The runtime frame is non-moving; exclusive logical slots ensure that its
/// fixed offsets keep denoting the same local across every poll/resume. Do not
/// allow the backend's conservative frame-all fallback to hide a missing proof.
pub(crate) fn stable_borrow_rejection(function: &super::LirFunction) -> Option<String> {
    let frame = &function.async_frame;
    let mut occupants = vec![0usize; frame.slots.len()];
    for (local, slot) in &frame.locals {
        if function.locals.get(local.0 as usize).is_none() || slot.index >= occupants.len() {
            return Some("invalid async frame local or slot".into());
        }
        occupants[slot.index] += 1;
    }
    let check_local = |id: LirLocalId| {
        let Some(local) = function.locals.get(id.0 as usize) else {
            return Some("borrowed local has no storage identity".into());
        };
        let stable = frame
            .slot(id)
            .is_some_and(|slot| occupants[slot.index] == 1 && frame.slots[slot.index] == id);
        (!stable).then(|| {
            format!(
                "borrowed local `{}` has no exclusive stable async frame slot",
                local.name
            )
        })
    };
    for block in &function.blocks {
        for instruction in &block.instrs {
            match instruction {
                super::LirInst::Compute { value, .. } => {
                    for operand in value.iter_operands() {
                        if let Some(id) = operand.borrowed_local()
                            && let Some(reason) = check_local(id)
                        {
                            return Some(reason);
                        }
                    }
                }
                // Cleanup runs in a separate region but accesses the parent's
                // storage. Its captures are pinned by the same frame planner.
                super::LirInst::Defer { body, .. } => {
                    for id in &body.captures {
                        if let Some(reason) = check_local(*id) {
                            return Some(reason);
                        }
                    }
                }
                ir_variants!(
                    LirInst;
                    Let, Assign, EnterDeferScope, LeaveDeferScope, FlushDefers, ClearScopeRoots,
                    SelectInit, SelectProbe, SelectPick, SelectUnregister, SelectCommit,
                    ReleaseLock, MatchTest, MatchBind, Unsupported
                ) => {}
            }
        }
    }
    None
}

fn reusable_scalar(local: &LirLocal) -> bool {
    !local.is_gc_owner()
        && matches!(
            local.ty,
            crate::parser::ast::Type::I8
                | crate::parser::ast::Type::I16
                | crate::parser::ast::Type::I32
                | crate::parser::ast::Type::U16
                | crate::parser::ast::Type::U32
                | crate::parser::ast::Type::U64
                | crate::parser::ast::Type::Char
                | crate::parser::ast::Type::U8
                | crate::parser::ast::Type::I64
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

/// Reuse a scratch table while its capacity tracks the last populated set.
/// Iteration and clearing can both scan buckets. After a wide-to-narrow
/// transition, charge one sparse scan to the wide set and release the table.
pub(crate) fn clear_local_scratch(set: &mut HashSet<LirLocalId>) {
    #[cfg(test)]
    SCRATCH_CAPACITY.with(|total| total.set(total.get() + set.capacity()));
    if crate::ir::sparse_scratch_table(set.capacity(), set.len()) {
        *set = HashSet::new();
    } else {
        set.clear();
    }
}

pub(super) enum LocalAccess {
    Read,
    Write,
}

pub(crate) fn instruction_use_def(
    inst: &SourceInst,
    names: &HashMap<&str, LirLocalId>,
    uses: &mut HashSet<LirLocalId>,
    defs: &mut HashSet<LirLocalId>,
) {
    visit_instruction_locals(inst, names, |access, id| match access {
        LocalAccess::Read => {
            if !defs.contains(&id) {
                uses.insert(id);
            }
        }
        LocalAccess::Write => {
            defs.insert(id);
        }
    });
}

/// Report reads and writes in execution order, including reads preceding a write
/// within one instruction. Each consumer chooses which IDs it needs to retain.
pub(super) fn visit_instruction_locals(
    inst: &SourceInst,
    names: &HashMap<&str, LirLocalId>,
    mut visit: impl FnMut(LocalAccess, LirLocalId),
) {
    use LocalAccess::{Read, Write};
    let mut read_expr = |expr: &HirExpr| visit_expr_uses(expr, names, |id| visit(Read, id));
    match inst {
        SourceInst::Compute { local, value, .. } => {
            for operand in value.iter_operands() {
                for id in operand.iter_locals() {
                    visit(Read, id);
                }
            }
            visit(Write, *local);
        }
        SourceInst::Let { local, value, .. } | SourceInst::Assign { local, value, .. } => {
            read_expr(value);
            visit(Write, *local);
        }
        SourceInst::FieldAssign { object, value, .. } => {
            read_expr(object);
            read_expr(value);
        }
        SourceInst::IndexAssign {
            array,
            index,
            value,
        } => {
            read_expr(array);
            read_expr(index);
            read_expr(value);
        }
        SourceInst::StaticFieldAssign { value, .. } | SourceInst::Expr(value) => read_expr(value),
        SourceInst::SuperInit { args, .. } => {
            for arg in args {
                read_expr(arg);
            }
        }
        SourceInst::Defer { body, .. } => {
            for capture in &body.captures {
                visit(Read, *capture);
            }
        }
        SourceInst::SelectInit { operations } => {
            for operation in operations {
                match operation {
                    LirSelectOp::Timeout { millis, deadline } => {
                        visit(Read, *millis);
                        visit(Write, *deadline);
                    }
                    LirSelectOp::Recv { .. }
                    | LirSelectOp::Send { .. }
                    | LirSelectOp::Join { .. }
                    | LirSelectOp::Default => visit_select_uses(operation, |id| visit(Read, id)),
                }
            }
        }
        SourceInst::SelectProbe { operations, ready } => {
            for operation in operations {
                visit_select_uses(operation, |id| visit(Read, id));
            }
            for local in ready.iter().flatten() {
                visit(Write, *local);
            }
        }
        SourceInst::SelectPick { ready, chosen } => {
            for local in ready.iter().flatten() {
                visit(Read, *local);
            }
            visit(Write, *chosen);
        }
        SourceInst::SelectUnregister { operations, winner } => {
            visit(Read, *winner);
            for operation in operations {
                visit_select_uses(operation, |id| visit(Read, id));
            }
        }
        SourceInst::SelectCommit { operation, success } => {
            visit_select_uses(operation, |id| visit(Read, id));
            match operation {
                LirSelectOp::Recv {
                    binding: Some(id), ..
                }
                | LirSelectOp::Join {
                    binding: Some(id), ..
                } => visit(Write, *id),
                LirSelectOp::Recv { binding: None, .. }
                | LirSelectOp::Join { binding: None, .. }
                | LirSelectOp::Send { .. }
                | LirSelectOp::Timeout { .. }
                | LirSelectOp::Default => {}
            }
            visit(Write, *success);
        }
        // Releasing reads all four of the acquisition's frame slots, so
        // each one stays live from the `lock` down to every exit that
        // leaves the section. A `lock` body's scope
        // reads them too: its panic cleanup is where an unwind releases.
        SourceInst::ReleaseLock(slots)
        | SourceInst::EnterDeferScope {
            lock: Some(slots), ..
        } => {
            for local in slots.all_locals() {
                visit(Read, local);
            }
        }
        // The scrutinee is READ by every dispatch block and by the bind at
        // the top of each arm; the bindings are DEFINED there. That is what
        // puts a binding an arm reads after suspending into the frame, and
        // keeps a scrutinee nothing reads again out of it.
        //
        SourceInst::MatchTest {
            scrutinee, result, ..
        } => {
            visit(Read, *scrutinee);
            visit(Write, *result);
        }
        SourceInst::MatchBind {
            scrutinee,
            bindings,
            ..
        } => {
            visit(Read, *scrutinee);
            for local in bindings {
                visit(Write, *local);
            }
        }
        // Naming a local does not read it: the instruction drops the GC
        // root of a scope that ended, so nothing it names is live past it
        // and nothing it names is redefined either.
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
    visit_terminator_uses(terminator, names, |id| {
        if !defs.contains(&id) {
            uses.insert(id);
        }
    });
}

pub(super) fn visit_terminator_uses(
    terminator: &SourceTerminator,
    names: &HashMap<&str, LirLocalId>,
    read: impl FnMut(LirLocalId),
) {
    match terminator {
        SourceTerminator::Branch { cond: expr, .. } | SourceTerminator::Return(Some(expr)) => {
            visit_expr_uses(expr, names, read);
        }
        SourceTerminator::Suspend { operation, .. } => operation.for_each_local(read),
        SourceTerminator::Jump(_)
        | SourceTerminator::Return(None)
        | SourceTerminator::CleanupReturn => {}
    }
}

fn visit_select_uses(operation: &LirSelectOp, mut read: impl FnMut(LirLocalId)) {
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
    visit_expr_uses(expr, names, |id| {
        if !defs.contains(&id) {
            uses.insert(id);
        }
    });
}

fn visit_expr_uses(
    mut expr: &HirExpr,
    names: &HashMap<&str, LirLocalId>,
    mut visit: impl FnMut(LirLocalId),
) {
    let mut read = |name: &str| {
        if let Some(&id) = names.get(name) {
            visit(id);
        }
    };
    let mut pending = Vec::new();
    loop {
        match &expr.kind {
            HirExprKind::Var(name) => read(name),
            HirExprKind::Call {
                callee,
                binding,
                args,
                ..
            } => {
                if binding.is_local() {
                    read(&callee.name());
                }
                pending.extend(args);
            }
            HirExprKind::Lambda { captures, .. } => {
                for capture in captures {
                    read(&capture.source);
                }
                // Constructing the closure reads its environment. Its body runs later.
            }
            ir_variants!(
                HirExprKind;
                Int, Float, Bool, Str, FnRef, Binary, Unary, Invoke, Print, Array,
                Index, Ternary, New, FieldAccess, MethodCall, ObjectLiteral, StaticField,
                StaticCall, ReferenceArg, Range, Await, TryPropagate, Match, Select
            ) => pending.extend(expr.children()),
        }
        let Some(next) = pending.pop() else {
            break;
        };
        expr = next;
    }
}

/// Disjoint half-open runs of same-type slot ordinals occupied by one block.
#[derive(Default)]
struct OccupiedSlots {
    prefix: usize,
    // Runs above the first hole. A dense prefix needs no tree allocation.
    runs: BTreeMap<usize, usize>,
}

impl OccupiedSlots {
    fn first_free_from(&self, slot: usize) -> usize {
        if slot <= self.prefix {
            return self.prefix;
        }
        self.runs
            .range(..=slot)
            .next_back()
            .map_or(slot, |(_, &end)| end.max(slot))
    }

    fn occupy(&mut self, slot: usize) {
        let end = self.runs.remove(&(slot + 1)).unwrap_or(slot + 1);
        if slot == self.prefix {
            self.prefix = end;
        } else if let Some((_, previous_end)) = self.runs.range_mut(..slot).next_back()
            && *previous_end == slot
        {
            *previous_end = end;
        } else {
            self.runs.insert(slot, end);
        }
    }
}

/// All ordinals below `end` conflict with at least one of these blocks.
struct ConflictPrefix {
    blocks: Vec<usize>,
    end: usize,
}

#[derive(Default)]
struct WitnessNode {
    children: HashMap<usize, usize>,
    proof: Option<ConflictPrefix>,
}

/// Trie of ordered witness sets. At each matched prefix, inspect the smaller
/// of its outgoing edges and the remaining blocks of the request.
#[derive(Default)]
struct ConflictPrefixes {
    nodes: Vec<WitnessNode>,
    positions: HashMap<usize, usize>,
    pending: Vec<(usize, usize)>,
}

impl ConflictPrefixes {
    fn insert(&mut self, blocks: Vec<usize>, end: usize) {
        if self.nodes.is_empty() {
            self.nodes.push(WitnessNode::default());
        }
        let mut node = 0;
        for &block in &blocks {
            self.positions.entry(block).or_insert(0);
            node = if let Some(&child) = self.nodes[node].children.get(&block) {
                child
            } else {
                let child = self.nodes.len();
                self.nodes[node].children.insert(block, child);
                self.nodes.push(WitnessNode::default());
                child
            };
        }
        self.nodes[node].proof = Some(ConflictPrefix { blocks, end });
    }

    fn best(&mut self, region: &[usize], mut bound: usize) -> Option<&mut ConflictPrefix> {
        if self.nodes.is_empty() {
            return None;
        }
        for (i, block) in region.iter().enumerate() {
            if let Some(pos) = self.positions.get_mut(block) {
                *pos = i;
            }
        }
        self.pending.push((0, 0));
        let mut best = None;
        while let Some((node, start)) = self.pending.pop() {
            if let Some(proof) = &self.nodes[node].proof
                && proof.end > bound
            {
                best = Some(node);
                bound = proof.end;
            }
            let children = &self.nodes[node].children;
            let remaining = &region[start..];
            #[cfg(test)]
            SLOT_PROOF_PROBES.set(SLOT_PROOF_PROBES.get() + children.len().min(remaining.len()));
            if remaining.len() < children.len() {
                for (offset, b) in remaining.iter().enumerate() {
                    if let Some(&child) = children.get(b) {
                        self.pending.push((child, start + offset + 1));
                    }
                }
            } else {
                for (b, &child) in children {
                    let pos = self.positions[b];
                    // An absent label can retain a position from an earlier request.
                    if pos >= start && region.get(pos) == Some(b) {
                        self.pending.push((child, pos + 1));
                    }
                }
            }
        }
        self.nodes[best?].proof.as_mut()
    }

    #[cfg(test)]
    fn proofs(&self) -> impl Iterator<Item = &ConflictPrefix> {
        self.nodes.iter().filter_map(|n| n.proof.as_ref())
    }
}

/// First-fit assigns monotonically occupied slots in creation order.
#[derive(Default)]
struct ReusableSlots<'region> {
    slots: Vec<usize>,
    by_block: HashMap<usize, OccupiedSlots>,
    // Remember fragmented searches as well, including interleaved regions.
    region_prefix: HashMap<&'region [usize], usize>,
    shared: ConflictPrefixes,
}

impl<'region> ReusableSlots<'region> {
    fn assign(
        &mut self,
        region: &'region [usize],
        local: LirLocalId,
        slots: &mut Vec<LirLocalId>,
    ) -> usize {
        let (prefix_block, prefix) = region
            .iter()
            .enumerate()
            .filter_map(|(i, b)| self.by_block.get(b).map(|o| (i, o.prefix)))
            .max_by_key(|&(_, end)| end)
            .unwrap_or((0, 0));
        let exact = self.region_prefix.get(region).copied().unwrap_or(0);
        let mut ordinal = prefix.max(exact);
        let mut shared = if ordinal < self.slots.len() && region.len() > 1 {
            self.shared.best(region, ordinal)
        } else {
            None
        };
        if let Some(proof) = shared.as_deref() {
            ordinal = proof.end;
        }
        let start = ordinal;
        let mut fragmented = false;
        let mut witnesses = Vec::new();
        while ordinal < self.slots.len() {
            let mut next = ordinal;
            let mut witness = 0;
            for (i, block) in region.iter().enumerate() {
                #[cfg(test)]
                SLOT_BLOCK_PROBES.set(SLOT_BLOCK_PROBES.get() + 1);
                if let Some(occupied) = self.by_block.get(block) {
                    let end = occupied.first_free_from(ordinal);
                    if end > next {
                        next = end;
                        witness = i;
                    }
                }
            }
            if next == ordinal {
                break;
            }
            fragmented |= ordinal != start || next == ordinal + 1;
            if start == prefix && region.len() > 1 {
                if witnesses.is_empty() {
                    witnesses.resize(region.len(), false);
                    if prefix > 0 {
                        witnesses[prefix_block] = true;
                    }
                }
                witnesses[witness] = true;
            }
            if let Some(proof) = shared.as_deref_mut()
                && proof.end == ordinal
                && proof.blocks.contains(&region[witness])
            {
                proof.end = next;
            }
            ordinal = next;
        }
        if ordinal == self.slots.len() {
            self.slots.push(slots.len());
            slots.push(local);
        }
        if let Some(proof) = shared
            && ordinal == proof.end
        {
            proof.end = ordinal + 1;
        } else if fragmented && witnesses.contains(&false) {
            let blocks: Vec<_> = region
                .iter()
                .zip(witnesses)
                .filter_map(|(&b, used)| used.then_some(b))
                .collect();
            self.shared.insert(blocks, ordinal + 1);
        } else if exact > 0 || (fragmented && region.len() > 1) {
            self.region_prefix.insert(region, ordinal + 1);
        }
        for &block in region {
            self.by_block.entry(block).or_default().occupy(ordinal);
        }
        self.slots[ordinal]
    }
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
    fn local_call_reads_its_callee_and_arguments_before_definition() {
        use crate::diagnostics::Span;
        use crate::ir::typed_ast::HirCallBinding;
        use crate::semantic::ids::FunctionId;
        let names = HashMap::from([("f", LirLocalId(0)), ("arg", LirLocalId(1))]);
        let call = |name, binding, args| HirExpr {
            kind: HirExprKind::Call {
                callee: FunctionId::free_from_source_name(name),
                binding,
                args,
            },
            ty: Type::I64,
            span: Span::dummy(),
        };
        let mut uses = HashSet::new();
        let f = call("f", HirCallBinding::Local { may_change: false }, vec![]);
        collect_expr_uses(&f, &names, &mut uses, &HashSet::new());
        assert_eq!(uses, HashSet::from([LirLocalId(0)]));
        uses.clear();
        let arg = HirExpr {
            kind: HirExprKind::Var("arg".into()),
            ty: Type::I64,
            span: Span::dummy(),
        };
        collect_expr_uses(
            &call("f", HirCallBinding::Local { may_change: false }, vec![arg]),
            &names,
            &mut uses,
            &HashSet::from([LirLocalId(0)]),
        );
        assert_eq!(uses, HashSet::from([LirLocalId(1)]));
        uses.clear();
        collect_expr_uses(
            &call("module::f", HirCallBinding::Function, vec![]),
            &names,
            &mut uses,
            &HashSet::new(),
        );
        assert!(uses.is_empty());
        collect_expr_uses(
            &call("f", HirCallBinding::Function, vec![]),
            &names,
            &mut uses,
            &HashSet::new(),
        );
        assert!(uses.is_empty());
    }

    #[test]
    fn lambda_reads_capture_sources_without_entering_its_body() {
        use crate::diagnostics::Span;
        use crate::ir::typed_ast::{HirCapture, HirStmt};
        use crate::parser::ast::ExprId;
        let names = HashMap::from([("outer", LirLocalId(0)), ("inner", LirLocalId(1))]);
        let lambda = HirExpr {
            kind: HirExprKind::Lambda {
                id: ExprId::fresh(),
                params: vec![],
                captures: vec![HirCapture {
                    source: "outer".into(),
                    name: "inner".into(),
                    ty: Type::I64,
                }],
                body: vec![HirStmt::Expr(HirExpr {
                    kind: HirExprKind::Var("inner".into()),
                    ty: Type::I64,
                    span: Span::dummy(),
                })],
            },
            ty: Type::Fn(vec![], Box::new(Type::Void)),
            span: Span::dummy(),
        };
        let mut uses = HashSet::new();
        collect_expr_uses(&lambda, &names, &mut uses, &HashSet::new());
        assert_eq!(uses, HashSet::from([LirLocalId(0)]));
        uses.clear();
        collect_expr_uses(&lambda, &names, &mut uses, &HashSet::from([LirLocalId(0)]));
        assert!(uses.is_empty());
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
    fn select_reads_respect_earlier_writes_in_the_same_instruction() {
        let [source, first, second] = [0, 1, 2].map(LirLocalId);
        let inst = SourceInst::SelectInit {
            operations: vec![
                LirSelectOp::Timeout {
                    millis: source,
                    deadline: first,
                },
                LirSelectOp::Timeout {
                    millis: first,
                    deadline: second,
                },
            ],
        };
        let mut uses = HashSet::new();
        let mut defs = HashSet::new();
        instruction_use_def(&inst, &HashMap::new(), &mut uses, &mut defs);
        assert_eq!(uses, HashSet::from([source]));
        assert_eq!(defs, HashSet::from([first, second]));
    }

    #[test]
    fn suspension_reads_only_upward_exposed_values_on_block_entry() {
        use super::super::{BlockId, LirOperand, LirRvalue, SuspendOp};
        use crate::diagnostics::Span;
        let id = LirLocalId(0);
        let locals = [LirLocal {
            id,
            name: "millis".into(),
            ty: Type::I64,
            storage_kind: Default::default(),
            synthetic: false,
            parameter: false,
            source_span: None,
        }];
        let mut blocks = [
            SourceBlock {
                id: BlockId(0),
                instrs: vec![SourceInst::Compute {
                    local: id,
                    value: LirRvalue::Use(LirOperand::Int(1)),
                    span: Span::dummy(),
                }],
                terminator: SourceTerminator::Suspend {
                    operation: SuspendOp::Sleep { millis: id },
                    resume: BlockId(1),
                },
                recovery: vec![],
            },
            SourceBlock {
                id: BlockId(1),
                instrs: vec![],
                terminator: SourceTerminator::Return(None),
                recovery: vec![],
            },
        ];
        assert!(
            Liveness::compute(&blocks, &locals)
                .live_in
                .iter(0)
                .next()
                .is_none()
        );
        blocks[0].instrs.insert(
            0,
            SourceInst::Expr(HirExpr {
                kind: HirExprKind::Var("millis".into()),
                ty: Type::I64,
                span: Span::dummy(),
            }),
        );
        assert_eq!(
            Liveness::compute(&blocks, &locals)
                .live_in
                .iter(0)
                .collect::<Vec<_>>(),
            [id]
        );
    }

    #[test]
    fn first_fit_indexes_disjoint_regions_and_skips_conflicting_prefixes() {
        for n in [8, 64, 512, 2048] {
            let mut group = ReusableSlots::default();
            let mut slots = vec![];
            SLOT_BLOCK_PROBES.set(0);
            let regions: Vec<_> = (0..n).map(|i| [i]).collect();
            for (i, region) in regions.iter().enumerate() {
                assert_eq!(group.assign(region, LirLocalId(i as u32), &mut slots), 0);
            }
            assert_eq!(SLOT_BLOCK_PROBES.get(), n - 1);
            assert_eq!(slots, [LirLocalId(0)]);
            // Each local now interferes in block zero. Its first possible
            // slot advances by one without searching previous occupants.
            SLOT_BLOCK_PROBES.set(0);
            for i in 1..=n {
                assert_eq!(
                    group.assign(&[0], LirLocalId((n + i) as u32), &mut slots),
                    i
                );
            }
            assert_eq!(SLOT_BLOCK_PROBES.get(), 0);
            // A free earlier slot for a different region still wins.
            assert_eq!(
                group.assign(&[1, n], LirLocalId((2 * n + 1) as u32), &mut slots),
                1
            );
        }
    }

    #[test]
    fn repeated_mixed_regions_resume_after_their_first_fit() {
        for n in [8, 64, 512, 2048] {
            let mut group = ReusableSlots::default();
            let mut slots = Vec::new();
            assert_eq!(group.assign(&[0, 2], LirLocalId(0), &mut slots), 0);
            assert_eq!(group.assign(&[1, 2], LirLocalId(1), &mut slots), 1);
            SLOT_BLOCK_PROBES.set(0);
            for i in 2..n + 2 {
                assert_eq!(group.assign(&[0, 1], LirLocalId(i as u32), &mut slots), i);
            }
            // Only the first mixed search inspects slot one (A absent, B present).
            assert_eq!(SLOT_BLOCK_PROBES.get(), 2);
            assert_eq!(slots.len(), n + 2);
            assert_eq!(group.region_prefix.len(), 1);
        }
    }

    #[test]
    fn mixed_regions_skip_shared_conflicts_with_distinct_extra_blocks() {
        for n in [8, 64, 512, 2048] {
            let regions: Vec<_> = (0..n).map(|i| [0, 1, 3 + i]).collect();
            let mut group = ReusableSlots::default();
            let mut slots = Vec::new();
            assert_eq!(group.assign(&[0, 2], LirLocalId(0), &mut slots), 0);
            assert_eq!(group.assign(&[1, 2], LirLocalId(1), &mut slots), 1);
            SLOT_BLOCK_PROBES.set(0);
            for (i, region) in regions.iter().enumerate() {
                assert_eq!(
                    group.assign(region, LirLocalId(i as u32 + 2), &mut slots),
                    i + 2
                );
            }
            assert_eq!(SLOT_BLOCK_PROBES.get(), 3);
            assert_eq!(group.shared.proofs().count(), 1);
            assert_eq!(slots.len(), n + 2);
        }
    }

    #[test]
    fn alternating_slot_conflicts_share_their_bound_across_wider_regions() {
        for n in [8, 64, 512, 2048] {
            // Extra blocks lie between A and B in block traversal order.
            let b = n + 1;
            let c = n + 2;
            let pairs = [[0, c], [b, c]];
            let regions: Vec<_> = (1..=n).map(|d| [0, d, b]).collect();
            let mut group = ReusableSlots::default();
            let mut slots = Vec::new();
            SLOT_BLOCK_PROBES.set(0);
            for i in 0..2 * n {
                assert_eq!(
                    group.assign(&pairs[i % 2], LirLocalId(i as u32), &mut slots),
                    i
                );
            }
            for (i, region) in regions.iter().enumerate() {
                let ordinal = 2 * n + i;
                assert_eq!(
                    group.assign(region, LirLocalId(ordinal as u32), &mut slots),
                    ordinal
                );
            }
            assert_eq!(SLOT_BLOCK_PROBES.get(), 3 * (2 * n - 1));
            assert_eq!(group.shared.proofs().count(), 1);
            let proof = group.shared.proofs().next().unwrap();
            assert_eq!(proof.blocks, [0, b]);
            assert_eq!(proof.end, 3 * n);
            // Omitting B makes slot one available even after the prefix grew.
            assert_eq!(
                group.assign(&[0], LirLocalId((3 * n) as u32), &mut slots),
                1
            );
        }
    }

    #[test]
    fn alternating_proofs_with_one_anchor_keep_both_first_fit_bounds() {
        for n in [8, 64, 512, 2048] {
            let mut group = ReusableSlots::default();
            let mut slots = Vec::new();
            let regions: Vec<_> = (0..2 * n).map(|i| [0, 1 + i % 2, 4 + i]).collect();
            for i in 0..2 * n {
                let pair: &[usize] = if i % 2 == 0 { &[0, 3] } else { &[1, 2, 3] };
                assert_eq!(group.assign(pair, LirLocalId(i as u32), &mut slots), i);
            }
            SLOT_BLOCK_PROBES.set(0);
            SLOT_PROOF_PROBES.set(0);
            for (i, region) in regions.iter().enumerate() {
                let expected = 2 * n + i;
                assert_eq!(
                    group.assign(region, LirLocalId(expected as u32), &mut slots),
                    expected
                );
            }
            assert_eq!(group.shared.proofs().count(), 2);
            assert_eq!(SLOT_BLOCK_PROBES.get(), 18 * n - 9);
            assert_eq!(SLOT_PROOF_PROBES.get(), 6 * n - 4);
        }
    }

    #[test]
    fn fragmented_block_runs_match_first_fit_for_every_region() {
        let mut group = ReusableSlots::default();
        let mut slots = Vec::new();
        let mut occupants: Vec<usize> = Vec::new();
        let regions: Vec<Vec<_>> = (0..256)
            .map(|i| (0..7).filter(|b| (i * 37 % 128) & (1 << b) != 0).collect())
            .collect();
        for (i, region) in regions.iter().enumerate() {
            let mask = region.iter().fold(0, |bits, b| bits | (1 << b));
            let expected = occupants
                .iter()
                .position(|bits| bits & mask == 0)
                .unwrap_or(occupants.len());
            if expected == occupants.len() {
                occupants.push(0);
            }
            occupants[expected] |= mask;
            assert_eq!(
                group.assign(region, LirLocalId(i as u32), &mut slots),
                expected
            );
        }
    }

    #[test]
    fn closing_a_hole_skips_the_entire_occupied_run() {
        for n in [8, 64, 512, 2048] {
            let mut group = ReusableSlots::default();
            let mut slots = vec![];
            SLOT_BLOCK_PROBES.set(0);
            assert_eq!(group.assign(&[0], LirLocalId(0), &mut slots), 0);
            for i in 1..=n {
                assert_eq!(group.assign(&[0, 1], LirLocalId(i as u32), &mut slots), i);
            }
            for i in 0..n {
                let local = LirLocalId((n + 1 + i) as u32);
                let expected = if i == 0 { 0 } else { n + i };
                assert_eq!(group.assign(&[1], local, &mut slots), expected);
            }
            // Filling slot zero merges the entire adjacent run in one operation.
            assert_eq!(SLOT_BLOCK_PROBES.get(), 1);
            assert_eq!(slots.len(), 2 * n);
        }
    }

    #[test]
    fn separate_suspension_regions_share_the_first_frame_slot() {
        use super::super::{BlockId, SuspendOp};
        use crate::diagnostics::Span;
        for n in [8, 64, 512] {
            // One self-resuming region per scalar, with no intervening definition.
            let locals: Vec<_> = (0..n)
                .map(|id| LirLocal {
                    id: LirLocalId(id),
                    name: format!("v{id}"),
                    ty: Type::I64,
                    storage_kind: Default::default(),
                    parameter: false,
                    synthetic: true,
                    source_span: None,
                })
                .collect();
            let blocks: Vec<_> = locals
                .iter()
                .map(|local| {
                    let block = BlockId(local.id.0 as usize);
                    SourceBlock {
                        id: block,
                        instrs: vec![SourceInst::Expr(HirExpr {
                            kind: HirExprKind::Var(local.name.clone()),
                            ty: Type::I64,
                            span: Span::dummy(),
                        })],
                        terminator: SourceTerminator::Suspend {
                            operation: SuspendOp::Yield,
                            resume: block,
                        },
                        recovery: vec![],
                    }
                })
                .collect();
            SLOT_BLOCK_PROBES.set(0);
            let frame = analyze(&blocks, &locals);
            assert_eq!(frame.slots, [LirLocalId(0)]);
            assert!(
                locals
                    .iter()
                    .all(|local| frame.slot(local.id) == Some(FrameSlot { index: 0 }))
            );
            assert_eq!(SLOT_BLOCK_PROBES.get(), n as usize - 1);
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

#[cfg(test)]
#[path = "stable_borrow_tests.rs"]
mod stable_borrow_tests;
