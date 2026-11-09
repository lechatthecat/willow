//! Cranelift code generation from operand-only LIR.
//!
//! User bodies, lifted lambdas, and cleanup regions are basic-block graphs.
//! Instructions consume local IDs or immediate values; the backend never
//! evaluates a HIR expression tree. Lowering owns ternary, match, short-circuit,
//! propagation, select, and nested-await control flow.
//!
//! Every local is bound at function entry, independently of block emission
//! order. GC values use rooted storage; async values live in the LIR-computed
//! frame when they survive suspension. ClearScopeRoots ends temporary lifetimes
//! without growing the root stack on loop iterations.
//!
//! Reference operands capture their place before later arguments run. Array
//! references retain the original buffer in an opaque GC-owner local, so resize,
//! collection, and suspension cannot redirect the reference to a new buffer.
//! The address is materialized from the rooted owner at the call.
//!
//! Calls and stores validate signatures and layouts before emission. Explicit
//! coercions preserve argument evaluation order, and heap reference writes use
//! the GC barrier. Option representation follows the shared ABI metadata.
//!
//! Suspension terminators implement the cooperative poll protocol. Prepared
//! method frames and reference diagnostic scopes follow CFG edges and are
//! restored on resume. Defer scopes and recovery edges likewise come from LIR;
//! cleanup graphs are replayed with their captured locals and fresh region state.

use super::class_view::ClassTables;
use super::emit_option_result::ResultSide;
use super::function_index::FunctionMap;
use super::{FlatReferenceDebug, ModuleSymbols};
use crate::qualified_name::QualifiedName;
use crate::semantic::ids::SemanticType as Type;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use willow_abi::runtime_symbols::names as rt;

use cranelift_codegen::ir::{
    AbiParam, AtomicRmwOp, InstBuilder, MemFlagsData, StackSlotData, StackSlotKind,
    condcodes::FloatCC, condcodes::IntCC, types,
};
use cranelift_module::Module;

use crate::diagnostics::span::Span;
#[cfg(test)]
use crate::ir::dump::binop_str;
use crate::ir::lowered::{
    BlockId, LirBlock, LirFunction, LirInst, LirLocalId, LirLockSlots, LirSelectOp,
    LirSelectWaitOp, SuspendOp, Terminator,
};
#[cfg(test)]
use crate::ir::typed_ast::{
    HirExpr, HirExprKind, HirMatchArm, HirPattern, HirSelectCase, HirSelectCaseKind, HirStmt,
};
use crate::parser::ast::{BinOp, ExprId, ParamMode, UnaryOp};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::ids::{FunctionId, TypeId};
#[cfg(test)]
use crate::semantic::intrinsics;
use crate::semantic::intrinsics::Intrinsic;
#[cfg(test)]
use crate::semantic::type_checker::types::await_output_type;
use crate::semantic::type_checker::types::type_name;

use super::emit_interface::{collection_elem_kind, is_self_or_descendant};
use super::gc_codegen::{GcLayoutMetadata, GcObjectKind, GcStoreDestination};
use super::option_repr::{OptionRepr, is_scalar_pair};
use super::symbols::{class_method_symbol_name, class_name_for_object_type, module_item_symbol};
use super::type_helpers::{
    builtin_call_runtime_name, clif_type, is_gc_managed, reference_type,
    runtime_stat_builtin_runtime_name,
};
use super::{
    CoopSuspendPoints, FRAME_SLOT_TASK_ID, FuncGen, VarStorage, array_element_type,
    async_frame_slot_offset, channel_runtime_symbol,
};

mod cleanup_emission;
mod type_support;
pub(super) use type_support::*;
mod validation;
pub(super) use validation::*;
mod expression_validation;
use expression_validation::*;
mod intrinsic;
mod rvalue;
mod terminator;

/// Immutable function layout shared by a block's emission.
struct LirBlockLayout<'a> {
    function: &'a LirFunction,
    blocks: &'a [cranelift_codegen::ir::Block],
    return_type: &'a Type,
}

#[cfg(test)]
mod tests;

/// Physical handles for one immutable LIR cleanup state. All three heads share
/// their immutable ancestors; installing a block state is constant time.
#[derive(Clone, Default)]
struct LirCleanupState {
    entries: super::DeferStack,
    panic: super::Chain<super::PanicScope>,
    locks: super::Chain<super::CoopLockScope>,
}

/// A normal flush can share code only with the same lexical registrations,
/// panic destinations and root/protocol depth. Return values remain in their
/// already-bound LIR locals; a block parameter selects the continuation.
#[derive(Clone, Hash, PartialEq, Eq)]
struct NormalCleanupKey {
    region: Option<crate::ir::lowered::cleanup::CleanupRegionId>,
    entries: usize,
    depth: usize,
    roots: usize,
    panic_target: Option<cranelift_codegen::ir::Block>,
    panic_depth: usize,
    recover_depth: usize,
    call_depth: usize,
    native_active: bool,
}
struct NormalCleanup {
    entry: cranelift_codegen::ir::Block,
    end: Option<cranelift_codegen::ir::Block>,
    selector: cranelift_codegen::ir::Value,
    continuations: Vec<cranelift_codegen::ir::Block>,
}

struct LirCleanupBindings {
    initial: LirCleanupState,
    states: Vec<LirCleanupState>,
    scopes: Vec<super::PanicScope>,
    panic_states: Vec<LirCleanupState>,
    normal: HashMap<NormalCleanupKey, NormalCleanup>,
    roots: usize,
}
struct LirBlockDeferCtx<'a> {
    block_index: usize,
    cleanup: &'a crate::ir::lowered::cleanup::CleanupGraph,
    bindings: &'a mut LirCleanupBindings,
}

/// Whether every LIR block of a SYNCHRONOUS function can be emitted under the
/// defer-scope stack the emitter will actually be holding.
///
/// `emit_lir_function_inner` snapshots the Rust-side scope state at block
/// boundaries. That is sound exactly when the open scopes are a property of
/// the block rather than of the path taken to it: every ordinary predecessor
/// and every recovered-panic edge must agree on the block's entry stack.
///
/// A scope opened and closed inside one block satisfies this trivially, which
/// is every shape that was eligible before this check replaced a per-block
/// balance test. The interesting new one is a scope opened in the entry block
/// and left by a `return` in a later block: the stack is `[scope]` at every
/// block after the first. A scope opened in one arm of a branch and still open
/// at the join remains ambiguous and is refused.
///
/// Scopes are identified by where they were opened, so a stack comparison
/// distinguishes "the same scope is still open" from "one closed and another
/// opened", which a depth alone cannot.
/// The LIR blocks control can reach directly from `block`.
pub(super) fn lir_block_successors(block: &LirBlock) -> Vec<usize> {
    match &block.terminator {
        Terminator::Jump(target) => vec![target.0],
        Terminator::Branch {
            then_block,
            else_block,
            ..
        } => vec![then_block.0, else_block.0],
        Terminator::Suspend { resume, .. } => vec![resume.0],
        Terminator::Return(_) | Terminator::CleanupReturn => Vec::new(),
    }
}

/// Function entry covers recursion; DFS backedge targets cover every cycle,
/// including irreducible control flow and recovery edges. Polling ordinary
/// forward blocks adds overhead without covering additional unbounded work.
fn lir_sync_poll_blocks(f: &LirFunction) -> Vec<bool> {
    let mut poll = vec![false; f.blocks.len()];
    if let Some(entry) = poll.first_mut() {
        *entry = true;
    }
    let edges: Vec<Vec<usize>> = f
        .blocks
        .iter()
        .map(|block| {
            let mut edges = lir_block_successors(block);
            edges.extend(block.recovery.iter().map(|target| target.0));
            edges
        })
        .collect();
    let mut color = vec![0u8; f.blocks.len()];
    for root in 0..f.blocks.len() {
        if color[root] != 0 {
            continue;
        }
        color[root] = 1;
        let mut pending = vec![(root, 0)];
        while let Some((block, next)) = pending.last_mut() {
            if *next == edges[*block].len() {
                color[*block] = 2;
                pending.pop();
                continue;
            }
            let target = edges[*block][*next];
            *next += 1;
            match color[target] {
                0 => {
                    color[target] = 1;
                    pending.push((target, 0));
                }
                1 => poll[target] = true,
                _ => {}
            }
        }
    }
    poll
}

/// A bounded scalar early return needs no safepoint. Move the entry poll to
/// the other branch, including its runtime address/context lookups. Restrict
/// this to acyclic bodies so loop guards keep their invocation-cached inputs.
/// Like a division guard, a debug overflow check has a panic path.
fn lir_defer_entry_poll(f: &LirFunction, poll: &mut [bool], overflow_checks: bool) -> bool {
    use crate::ir::lowered::LirRvalue as V;
    let scalar = |ty: &Type| matches!(ty, Type::I64 | Type::F64 | Type::Bool);
    if f.is_async
        || f.params.iter().any(|p| p.by_reference)
        || f.locals.iter().any(|l| l.is_gc_owner() || !scalar(&l.ty))
        || poll.iter().skip(1).any(|p| *p)
        || f.blocks
            .iter()
            .any(|b| !b.recovery.is_empty() || lir_block_successors(b).contains(&0))
    {
        return false;
    }
    let Some(entry) = f.blocks.first() else {
        return false;
    };
    let Terminator::Branch {
        then_block,
        else_block,
        ..
    } = &entry.terminator
    else {
        return false;
    };
    let bounded = |block: &LirBlock| {
        block.instrs.len() <= 8
            && block.instrs.iter().all(|inst| match inst {
                LirInst::Compute { value, .. } => {
                    !(overflow_checks && value.has_overflow_check())
                        && match value {
                            V::Use(_) | V::Unary { .. } => true,
                            V::Binary { op, .. } => {
                                !matches!(op, BinOp::Div | BinOp::Rem | BinOp::Pow)
                            }
                            _ => false,
                        }
                }
                LirInst::Let { .. } | LirInst::Assign { .. } | LirInst::ClearScopeRoots { .. } => {
                    true
                }
                _ => false,
            })
    };
    let returns = |id: BlockId| {
        f.blocks
            .get(id.0)
            .is_some_and(|b| matches!(b.terminator, Terminator::Return(_)) && bounded(b))
    };
    if !bounded(entry) || (!returns(*then_block) && !returns(*else_block)) {
        return false;
    }
    poll[0] = false;
    for target in [*then_block, *else_block] {
        poll[target.0] = !returns(target);
    }
    true
}

/// Argument evaluation may cross basic blocks or a cooperative suspension.
/// Validate the matching preparations and retain the source frames at each
/// entry, independently of the order in which machine blocks are emitted.
fn lir_call_frame_entries(f: &LirFunction) -> Option<Vec<Vec<(String, Span)>>> {
    use crate::ir::lowered::{LirOperand, LirRvalue};
    type Stack = Vec<(crate::ir::lowered::LirLocalId, String, Span)>;
    let mut entries: Vec<Option<Stack>> = vec![None; f.blocks.len()];
    if entries.is_empty() {
        return Some(Vec::new());
    }
    entries[0] = Some(Vec::new());
    let mut work = vec![0];
    while let Some(index) = work.pop() {
        let mut stack = entries.get(index)?.clone()?;
        let mut edges = Vec::new();
        let mut diverged = false;
        for instruction in &f.blocks[index].instrs {
            match instruction {
                LirInst::Compute {
                    local,
                    value: LirRvalue::PrepareMethod { method, .. },
                    span,
                } => stack.push((*local, method.clone(), *span)),
                LirInst::Compute {
                    value:
                        LirRvalue::MethodCall {
                            receiver, method, ..
                        },
                    ..
                } => {
                    let (prepared, expected, _) = stack.pop()?;
                    if *receiver != LirOperand::Local(prepared) || *method != expected {
                        return None;
                    }
                }
                // Panic unwinds preparations at runtime and recovery restores
                // the state captured on scope entry. The syntactic terminator
                // is unreachable; forwarding its state invents a normal edge.
                LirInst::Compute {
                    value: LirRvalue::Panic { .. },
                    ..
                } => {
                    diverged = true;
                    break;
                }
                LirInst::EnterDeferScope {
                    resume: Some(resume),
                    ..
                } => edges.push((resume.0, stack.clone())),
                _ => {}
            }
        }
        if !diverged {
            edges.extend(
                lir_block_successors(&f.blocks[index])
                    .into_iter()
                    .map(|target| (target, stack.clone())),
            );
        }
        for (target, state) in edges {
            match entries.get_mut(target)? {
                Some(existing) if *existing != state => return None,
                Some(_) => {}
                entry @ None => {
                    *entry = Some(state);
                    work.push(target);
                }
            }
        }
    }
    Some(
        entries
            .into_iter()
            .map(|entry| {
                entry
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(_, name, span)| (name, span))
                    .collect()
            })
            .collect(),
    )
}

fn lir_reference_scope_entries(
    f: &LirFunction,
) -> Option<Vec<Vec<Vec<super::FlatReferenceDebug>>>> {
    use crate::ir::lowered::{LirOperand, LirRvalue as V};
    type Scopes = Vec<Vec<super::FlatReferenceDebug>>;
    let mut entries: Vec<Option<Scopes>> = vec![None; f.blocks.len()];
    if entries.is_empty() {
        return Some(Vec::new());
    }
    entries[0] = Some(Vec::new());
    let mut work = vec![0];
    while let Some(index) = work.pop() {
        let mut scopes = entries.get(index)?.clone()?;
        let mut edges = Vec::new();
        let mut diverged = false;
        for instruction in &f.blocks[index].instrs {
            match instruction {
                LirInst::Compute {
                    value: V::BeginReferenceCall,
                    ..
                } => scopes.push(Vec::new()),
                LirInst::Compute {
                    value:
                        V::ReferenceDebug {
                            argument,
                            callee,
                            index,
                        },
                    ..
                } => scopes.last_mut()?.push(super::FlatReferenceDebug {
                    argument: argument.clone(),
                    callee: *callee,
                    index: *index,
                }),
                LirInst::Compute {
                    value:
                        V::DirectCall { args, .. }
                        | V::StartTask { args, .. }
                        | V::StaticCall { args, .. }
                        | V::MethodCall { args, .. }
                        | V::ConstructorCall { args, .. },
                    ..
                } if args
                    .iter()
                    .any(|arg| matches!(arg, LirOperand::Reference { .. })) =>
                {
                    scopes.pop()?;
                }
                // Panic unwinds preparations at runtime and recovery restores
                // the state captured on scope entry. The syntactic terminator
                // is unreachable; forwarding its state invents a normal edge.
                LirInst::Compute {
                    value: V::Panic { .. },
                    ..
                } => {
                    diverged = true;
                    break;
                }
                LirInst::EnterDeferScope {
                    resume: Some(resume),
                    ..
                } => edges.push((resume.0, scopes.clone())),
                _ => {}
            }
        }
        if !diverged {
            edges.extend(
                lir_block_successors(&f.blocks[index])
                    .into_iter()
                    .map(|target| (target, scopes.clone())),
            );
        }
        for (target, state) in edges {
            match entries.get_mut(target)? {
                Some(existing) if *existing != state => return None,
                Some(_) => {}
                entry @ None => {
                    *entry = Some(state);
                    work.push(target);
                }
            }
        }
    }
    Some(entries.into_iter().map(Option::unwrap_or_default).collect())
}

/// Where a LIR instruction came from, for the debug-build fault site. LIR
/// instructions carry no span of their own, so this takes the span of the
/// sub-expression that actually runs the fault-capable code: the indexed array
/// for an element store, the stored value otherwise.
fn lir_inst_span(inst: &LirInst) -> Option<Span> {
    match inst {
        LirInst::Compute { span, .. }
        | LirInst::Let { span, .. }
        | LirInst::Defer { span, .. }
        | LirInst::MatchTest { span, .. }
        | LirInst::MatchBind { span, .. }
        | LirInst::Unsupported { span, .. } => Some(*span),
        _ => None,
    }
}

/// The edges of `f` that close a loop: the target DOMINATES the source, so
/// every path that reaches the source has already run the target.
///
/// A preemption safepoint belongs on a real back edge — a CPU-bound loop has to
/// stay preemptible — but nowhere else. The AST liveness pass models suspension
/// at exactly "loop backedges and statements that execute a call", so a
/// safepoint on any OTHER edge can park with a local the pass left in an SSA
/// value, and the resumed poll re-enters past that definition.
///
/// Neither block ids nor plain reachability can pick these out. Lowering
/// numbers an `if`'s join block before its `else` arm, so the else arm's jump
/// to the join runs backwards by id while closing no loop; and inside a loop
/// every block reaches every other, so "the target reaches the source" calls
/// that same join edge a loop. Dominance is the definition that separates them
///.
#[cfg(test)]
fn lir_back_edges(f: &LirFunction) -> std::collections::HashSet<(usize, usize)> {
    fn successors(block: &LirBlock) -> Vec<usize> {
        match &block.terminator {
            Terminator::Jump(target) => vec![target.0],
            Terminator::Branch {
                then_block,
                else_block,
                ..
            } => vec![then_block.0, else_block.0],
            Terminator::Suspend { resume, .. } => vec![resume.0],
            Terminator::Return(_) | Terminator::CleanupReturn => Vec::new(),
        }
    }

    let n = f.blocks.len();
    let mut predecessors = vec![Vec::new(); n];
    for block in &f.blocks {
        for target in successors(block) {
            predecessors[target].push(block.id.0);
        }
    }

    // Iterative dominators over the block set. The entry is dominated only by
    // itself; every other block starts dominated by everything and shrinks to
    // its own fixpoint, which also leaves an unreachable block (no
    // predecessors) dominated by all of them — it emits no code that matters.
    let mut dominators: Vec<Vec<bool>> = vec![vec![true; n]; n];
    dominators[0] = (0..n).map(|i| i == 0).collect();
    let mut changed = true;
    while changed {
        changed = false;
        for at in 1..n {
            let mut next = vec![false; n];
            let mut first = true;
            for &pred in &predecessors[at] {
                if first {
                    next.copy_from_slice(&dominators[pred]);
                    first = false;
                } else {
                    for (slot, dominates) in next.iter_mut().zip(&dominators[pred]) {
                        *slot &= *dominates;
                    }
                }
            }
            if first {
                continue;
            }
            next[at] = true;
            if next != dominators[at] {
                dominators[at] = next;
                changed = true;
            }
        }
    }

    let mut edges = std::collections::HashSet::new();
    for block in &f.blocks {
        for target in successors(block) {
            if dominators[block.id.0][target] {
                edges.insert((block.id.0, target));
            }
        }
    }
    edges
}

#[cfg(test)]
fn lir_terminator_needs_preempt_safepoint(
    block: &LirBlock,
    back_edges: &std::collections::HashSet<(usize, usize)>,
) -> bool {
    let closes_loop = |target: &BlockId| back_edges.contains(&(block.id.0, target.0));
    match &block.terminator {
        Terminator::Jump(target) => closes_loop(target),
        Terminator::Branch {
            cond: _,
            then_block,
            else_block,
        } => closes_loop(then_block) || closes_loop(else_block),
        Terminator::Return(Some(_)) => false,
        Terminator::Suspend { .. } | Terminator::Return(None) | Terminator::CleanupReturn => false,
    }
}

#[willow_continuations::methods(
    emit_deferred_action,
    emit_flush_defers_from,
    emit_sync_try_defer_flush
)]
impl<'a, 'b> FuncGen<'a, 'b> {
    /// Emit a whole function body by walking its LIR block graph. The entry
    /// block's instructions land in the already-created Cranelift entry block
    /// (parameters are bound there); every other LIR block gets its own.
    /// All paths are terminated by the LIR, so the caller must skip its
    /// implicit-return epilogue.
    pub(super) fn emit_lir_function(&mut self, f: &LirFunction) {
        // A proven inline-only, acyclic leaf cannot consume more stack after
        // its probed prologue. Leave the caller's name in place: a prologue
        // fault occurs before any callee marker could execute in either case.
        if !super::root_effect::is_safepoint_free_leaf(
            f,
            self.diagnostics.build_mode == crate::BuildMode::Debug,
        ) {
            self.emit_stack_helper();
            // For proven runtime-only bodies, every path is dominated by this
            // publication and no body operation changes the helper marker.
            self.diagnostics.stack_helper_established =
                super::root_effect::preserves_stack_helper(f);
        }
        self.emit_lir_function_inner(f, None);
    }

    /// Replay a cleanup graph with fresh region storage and the enclosing
    /// panic/defer state. CleanupReturn rejoins this replay's continuation.
    pub(super) fn emit_lir_cleanup_region(&mut self, region: &LirFunction) {
        let vars = self.locals.vars.clone();
        let roots = self.roots.gc_root_count;
        let active_roots = self
            .roots
            .coop_shadow_roots
            .as_ref()
            .map(|roots| roots.active.clone());
        let defers = self.cleanup.defer_stack.clone();
        let panic_scopes = self.cleanup.panic_scopes.clone();
        let flags = self.cleanup.sync_defer_flags.clone();
        let frame_offsets = std::mem::take(&mut self.suspension.lir_frame_offsets);
        let before_exit = self.cleanup.lir_cleanup_exit;
        let exit = self.builder.create_block();
        self.cleanup.lir_cleanup_exit = Some((exit, roots, false));
        // Region temporaries can share a spelling with a subsequently declared
        // outer temporary; their lifetimes and storage are independent.
        for local in region.locals.iter().filter(|local| !local.parameter) {
            self.locals.vars.remove(&local.name);
        }
        self.emit_lir_function_inner(region, None);
        let reached = self
            .cleanup
            .lir_cleanup_exit
            .expect("cleanup exit installed")
            .2;
        self.cleanup.lir_cleanup_exit = before_exit;
        self.locals.vars = vars;
        self.roots.gc_root_count = roots;
        self.cleanup.defer_stack = defers;
        self.cleanup.panic_scopes = panic_scopes;
        self.cleanup.sync_defer_flags = flags;
        self.suspension.lir_frame_offsets = frame_offsets;
        if let (Some(active), Some(state)) = (active_roots, self.roots.coop_shadow_roots.as_mut()) {
            state.active = active;
        }
        if reached {
            self.builder.switch_to_block(exit);
            self.builder.seal_block(exit);
        }
        self.locals.terminated = !reached;
    }

    /// Emit an async poll body from LIR while retaining the established
    /// cooperative ABI. Each instruction boundary gets a cancellable
    /// preemption transition; locals selected
    /// by async liveness are read from their heap-frame slots after resume.
    pub(super) fn emit_coop_lir_function(
        &mut self,
        f: &LirFunction,
        suspends: &mut CoopSuspendPoints,
        frame: cranelift_codegen::ir::Value,
    ) {
        self.emit_lir_function_inner(f, Some((suspends, frame)));
    }

    fn emit_lir_function_inner(
        &mut self,
        f: &LirFunction,
        mut coop: Option<(&mut CoopSuspendPoints, cranelift_codegen::ir::Value)>,
    ) {
        let proof = if coop.is_none() && self.suspension.lir_frame_offsets.is_empty() {
            super::nonnegative::analyze(f)
        } else {
            Default::default()
        };
        let outer_nonnegative = std::mem::replace(&mut self.reuse.lir_nonnegative, proof);
        let outer_confined_maps = self.suspension.lir_confined_maps;
        self.suspension.lir_confined_maps = super::local_maps::confined_scalar_maps(f);
        let incoming_frames =
            lir_call_frame_entries(f).expect("validated method preparation frames");
        let incoming_references =
            lir_reference_scope_entries(f).expect("validated reference argument scopes");
        let outer_references = self.diagnostics.lir_reference_scopes.clone();
        let outer_frames = self.diagnostics.lir_call_frames.clone();
        let outer_frame_depth = self.diagnostics.callstack_frame_depth;
        let entry = self.builder.current_block().expect("entry block active");
        let mut poll_blocks = lir_sync_poll_blocks(f);
        let loop_depths = super::block_layout::lir_loop_depths(f);
        // A bounded body that cannot reach a safepoint needs no poll at all;
        // the caller's polls already bound the work around it.
        let unpolled =
            coop.is_none() && super::root_effect::is_safepoint_free_leaf(f, self.overflow_checks());
        if unpolled {
            poll_blocks.fill(false);
        }
        let deferred_poll = coop.is_none()
            && !unpolled
            && lir_defer_entry_poll(f, &mut poll_blocks, self.overflow_checks());
        // SSA carries the delayed activity lookup through later joins and
        // inlined recursive bodies. The zero entry value is used only on the
        // bounded return path; every path containing calls crosses a poll.
        let deferred_active = deferred_poll.then(|| {
            let ptr_ty = reference_type(self.module.target_config());
            let var = self.builder.declare_var(ptr_ty);
            let zero = self.builder.ins().iconst(ptr_ty, 0);
            self.builder.def_var(var, zero);
            var
        });
        let loop_polls = (0..f.blocks.len()).any(|i| poll_blocks[i] && loop_depths.all[i] > 0);
        let poll_spare =
            (coop.is_none() && !unpolled && loop_polls).then(|| self.emit_sync_poll_spare());
        let sync_poll = (coop.is_none() && !deferred_poll && !unpolled)
            .then(|| self.emit_sync_poll_gates(poll_spare));
        let unrooted = if coop.is_none() {
            super::transient_roots::unrooted_array_temps(f)
        } else {
            HashSet::new()
        };
        if coop.is_some() {
            self.bind_coop_lir_locals(f);
            // GC locals that are dead at every suspension deliberately stay
            // out of the heap frame, but still need a native shadow-stack
            // root while the current poll invocation can allocate.
            self.bind_lir_gc_locals(f);
        } else {
            self.bind_lir_reused_roots(f, &unrooted);
            self.bind_lir_gc_locals(f);
        }
        self.bind_lir_locals(f, &unrooted);
        let mut blocks = vec![entry];
        for _ in 1..f.blocks.len() {
            blocks.push(self.builder.create_block());
        }

        let cleanup_graph = crate::ir::lowered::cleanup::CleanupGraph::lower(f)
            .expect("validated cleanup control-flow graph");
        let mut cleanup_bindings =
            self.prepare_lir_cleanup(f, &cleanup_graph, &blocks, coop.is_some());
        let reuse_preds = super::transient_roots::sole_predecessors(f);
        let mut reuse_exits: Vec<Option<super::transient_roots::LirReuse>> =
            (0..f.blocks.len()).map(|_| None).collect();
        let mut order = cleanup_graph.order.clone();
        match std::env::var("WILLOW_TEST_LIR_BLOCK_ORDER").as_deref() {
            Ok("reverse") => {
                order = (1..f.blocks.len()).rev().collect();
                order.insert(0, 0);
            }
            Ok("index") => {
                order = (0..f.blocks.len()).collect();
            }
            _ => {}
        }
        for i in order {
            let block = &f.blocks[i];
            if i > 0 {
                self.builder.switch_to_block(blocks[i]);
            }
            self.install_lir_cleanup(cleanup_bindings.state(cleanup_graph.incoming_states[i]));
            self.roots.gc_root_count = cleanup_bindings.roots;
            // Each LIR block starts a fresh Cranelift block, so whatever the
            // previous one ended with (a `return`, a diverging statement) says
            // nothing about this one.
            self.locals.terminated = false;
            self.diagnostics.lir_call_frames = outer_frames.clone();
            self.diagnostics.lir_reference_scopes = outer_references.clone();
            self.diagnostics
                .lir_reference_scopes
                .extend(incoming_references[i].clone());
            let frames: Vec<_> = incoming_frames[i]
                .iter()
                .filter(|(method, _)| self.flat_method_frame_enabled(method))
                .cloned()
                .collect();
            self.diagnostics.lir_call_frames.extend(frames);
            self.diagnostics.callstack_frame_depth =
                outer_frame_depth + self.diagnostics.lir_call_frames.len() - outer_frames.len();
            let block_coop = coop
                .as_mut()
                .map(|(suspends, frame)| (&mut **suspends, *frame));
            let mut defer_ctx = LirBlockDeferCtx {
                block_index: i,
                cleanup: &cleanup_graph,
                bindings: &mut cleanup_bindings,
            };
            let block_poll = if deferred_poll && poll_blocks[i] {
                let spare = poll_spare.filter(|_| loop_depths.all[i] > 0);
                let poll = self.emit_sync_poll_gates(spare);
                self.builder
                    .def_var(deferred_active.expect("delayed poll activity"), poll.active);
                Some(poll)
            } else {
                sync_poll.filter(|_| poll_blocks[i])
            };
            let outer_active = self.cleanup.sync_native_active;
            self.cleanup.sync_native_active = block_poll
                .or(sync_poll)
                .map(|poll| poll.active)
                .or_else(|| deferred_active.map(|var| self.builder.use_var(var)));
            // Continue the predecessor's GC-free run only along its edge into
            // a block without an entry poll. The predecessor's
            // exit dominates this block whatever order emits the two.
            self.reuse.lir_reuse = match reuse_preds[i] {
                Some(pred) if !poll_blocks[i] => reuse_exits[pred].clone(),
                _ => None,
            };
            if let Some(reuse) = &mut self.reuse.lir_reuse {
                if let Some((target, index, len)) = reuse.branch_bound.take()
                    && target == i
                {
                    reuse.bound = Some((index, len));
                }
                reuse.comparison = None;
            }
            self.emit_lir_block(
                LirBlockLayout {
                    function: f,
                    blocks: &blocks,
                    return_type: &f.return_type,
                },
                block,
                block_coop,
                block_poll.map(|poll| (poll, loop_depths.all[i] > 0)),
                super::block_layout::prefer_then_fallthrough(f, &loop_depths, i),
                &mut defer_ctx,
            );
            self.cleanup.sync_native_active = outer_active;
            let facts = self.reuse.lir_reuse.take();
            if !self.locals.terminated
                && matches!(
                    block.terminator,
                    Terminator::Jump(_) | Terminator::Branch { .. }
                )
            {
                reuse_exits[i] = facts.filter(|facts| facts.carriable());
            }
        }
        self.diagnostics.lir_call_frames = outer_frames;
        self.diagnostics.lir_reference_scopes = outer_references;
        self.diagnostics.callstack_frame_depth = outer_frame_depth;
        self.locals.terminated = true;
        // Panic CFG resources exist before any source block is emitted. Every
        // lexical program is emitted once, independently of which exit ran last.
        for index in (0..cleanup_bindings.scopes.len()).rev() {
            self.install_lir_cleanup(&cleanup_bindings.panic_states[index]);
            self.roots.gc_root_count = cleanup_bindings.roots;
            self.emit_shared_panic_cleanup(&cleanup_bindings.scopes[index]);
        }
        for scope in &cleanup_bindings.scopes {
            self.builder.seal_block(scope.cleanup);
        }
        self.install_lir_cleanup(&cleanup_bindings.initial);
        for cleanup in cleanup_bindings.normal.into_values() {
            if let Some(end) = cleanup.end {
                self.builder.switch_to_block(end);
                let mut switch = cranelift_frontend::Switch::new();
                for (index, &continuation) in cleanup.continuations.iter().enumerate() {
                    switch.set_entry(index as u128, continuation);
                }
                switch.emit(self.builder, cleanup.selector, cleanup.continuations[0]);
                for continuation in cleanup.continuations {
                    self.builder.seal_block(continuation);
                }
            }
            self.builder.seal_block(cleanup.entry);
        }
        // The enclosing function compiler may append shared panic-return CFG
        // after the LIR body. It seals all blocks once that ABI edge exists
        //.
        self.locals.terminated = true;
        self.suspension.lir_confined_maps = outer_confined_maps;
        self.reuse.lir_nonnegative = outer_nonnegative;
    }

    /// Pre-bind the locals selected by LIR liveness to their LIR-owned frame
    /// slots. Source spans are intentionally absent from this lookup.
    fn bind_coop_lir_locals(&mut self, f: &LirFunction) {
        for local in &f.locals {
            if let Some(offset) = self.suspension.lir_frame_offsets.get(&local.id).copied() {
                self.locals
                    .vars
                    .insert(local.name.clone(), VarStorage::async_local(local, offset));
            }
        }
    }

    pub(super) fn load_lir_local(
        &mut self,
        function: &LirFunction,
        local: LirLocalId,
    ) -> cranelift_codegen::ir::Value {
        let name = &function.locals[local.0 as usize].name;
        let storage = self
            .locals
            .vars
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("LIR local `{name}` has no storage"));
        // Inside a GC-free run an array root slot still holds the owner the
        // run last read from it.
        let reusable = match storage {
            VarStorage::Stack { slot, .. }
                if self.reuse.lir_reuse.is_some()
                    && super::transient_roots::is_array(&function.locals[local.0 as usize].ty) =>
            {
                Some(slot)
            }
            _ => None,
        };
        if let Some(slot) = reusable {
            let reuse = self.reuse.lir_reuse.as_ref().expect("checked above");
            if let Some(&owner) = reuse.slots.get(&slot) {
                return owner;
            }
            let owner = self.stack_load(reference_type(self.module.target_config()), slot);
            self.reuse
                .lir_reuse
                .as_mut()
                .expect("checked above")
                .slots
                .insert(slot, owner);
            return owner;
        }
        if function.locals[local.0 as usize].is_gc_owner() {
            let ptr_ty = reference_type(self.module.target_config());
            return match storage {
                VarStorage::Stack { slot, .. } => self.stack_load(ptr_ty, slot),
                VarStorage::Frame { offset, .. } => self.builder.ins().load(
                    ptr_ty,
                    MemFlagsData::new(),
                    self.suspension.async_frame.expect("GC owner frame"),
                    offset,
                ),
                _ => panic!("opaque GC owner must occupy rooted storage"),
            };
        }
        let scalar = matches!(storage, VarStorage::Value { .. })
            && self
                .reuse
                .lir_nonnegative
                .contains(&crate::ir::lowered::LirOperand::Local(local));
        if scalar
            && let Some(value) = self
                .reuse
                .lir_reuse
                .as_ref()
                .and_then(|r| r.scalars.get(&local))
        {
            return *value;
        }
        let value = self.load_var(&storage);
        if scalar && let Some(reuse) = &mut self.reuse.lir_reuse {
            reuse.scalars.insert(local, value);
        }
        value
    }

    fn store_lir_local(
        &mut self,
        function: &LirFunction,
        local: LirLocalId,
        value: cranelift_codegen::ir::Value,
    ) {
        if let Some(reuse) = &mut self.reuse.lir_reuse {
            reuse.scalars.remove(&local);
        }
        let name = &function.locals[local.0 as usize].name;
        let storage = self
            .locals
            .vars
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("LIR local `{name}` has no storage"));
        if function.locals[local.0 as usize].is_gc_owner() {
            match storage {
                VarStorage::Stack { slot, .. } => self.stack_store(value, slot),
                VarStorage::Frame { offset, .. } => self.emit_gc_owner_frame_store(
                    self.suspension.async_frame.expect("GC owner frame"),
                    offset,
                    value,
                ),
                _ => panic!("opaque GC owner must occupy rooted storage"),
            }
            return;
        }
        self.store_var(&storage, value);
    }

    /// Share entry roots only when all accesses, including clears, fit in a
    /// single non-overlapping block interval. Other locals keep unique slots.
    fn bind_lir_reused_roots(&mut self, f: &LirFunction, unrooted: &HashSet<LirLocalId>) {
        let widths: Vec<_> = f
            .locals
            .iter()
            .map(|local| {
                (!local.parameter
                    && !unrooted.contains(&local.id)
                    && !self.locals.vars.contains_key(local.name.as_str())
                    && !self.locals.address_taken.contains(local.name.as_str())
                    && (local.is_gc_owner() || is_gc_managed(&local.ty, self.enum_infos)))
                .then(|| self.clif_type(&local.ty).bytes().max(8))
            })
            .collect();
        let mut null = None;
        for group in super::stack_slots::groups(f, &widths) {
            let first = &f.locals[group[0].0 as usize];
            self.bind_lir_rooted_slot(&first.name, &first.ty, &mut null);
            let VarStorage::Stack { slot, .. } = self.locals.vars[&first.name] else {
                unreachable!("new rooted slot");
            };
            for id in &group[1..] {
                let local = &f.locals[id.0 as usize];
                self.locals.vars.insert(
                    local.name.clone(),
                    VarStorage::Stack {
                        slot,
                        ty: local.ty.clone(),
                    },
                );
            }
        }
    }

    /// Give every GC-managed `let` of this function one entry-allocated, rooted
    /// stack slot (see the module docs). The slot is null-initialized so a
    /// collection that happens before the `let` executes reads an empty root
    /// rather than uninitialized stack memory. GC-managed *parameters* already
    /// got the same treatment from `bind_param`, so they are skipped here.
    fn bind_lir_gc_locals(&mut self, f: &LirFunction) {
        let mut null = None;
        for block in &f.blocks {
            for inst in &block.instrs {
                let LirInst::Let { name, ty, .. } = inst else {
                    continue;
                };
                if !is_gc_managed(ty, self.enum_infos) {
                    continue;
                }
                if self.locals.vars.contains_key(name) {
                    continue;
                }
                self.bind_lir_rooted_slot(name, ty, &mut null);
            }
        }
    }

    /// One entry-allocated, null-initialized, rooted stack slot for a
    /// GC-managed binding. `null` memoizes the zero across the slots of one
    /// function, so the entry block gets a single constant however many
    /// bindings need seeding.
    fn bind_lir_rooted_slot(
        &mut self,
        name: &str,
        ty: &Type,
        null: &mut Option<cranelift_codegen::ir::Value>,
    ) {
        let ptr_ty = reference_type(self.module.target_config());
        let slot = self.builder.create_sized_stack_slot(StackSlotData::new(
            StackSlotKind::ExplicitSlot,
            self.clif_type(ty).bytes().max(8),
            0,
        ));
        let zero = *null.get_or_insert_with(|| self.builder.ins().iconst(ptr_ty, 0));
        self.stack_store(zero, slot);
        if self.is_interface_pair(ty) {
            self.builder.ins().stack_store(ptr_ty, zero, slot, 8);
        }
        self.emit_push_root_slot(slot);
        self.track_coop_binding_root(slot);
        self.locals.vars.insert(
            name.to_string(),
            VarStorage::Stack {
                slot,
                ty: ty.clone(),
            },
        );
    }

    /// Drop the GC roots a lexical scope owned, now that it has ended
    ///.
    ///
    /// A GC-managed local's slot is allocated and rooted once at function entry
    /// (see the module docs), so without this the last value a scope bound
    /// stays reachable until the function returns: a loop body's binding pins
    /// one object per call. Nulling the slot at scope exit lets the collector
    /// reclaim it while leaving the root registered and empty, as at entry.
    ///
    /// Two kinds of slot are cleared. A stack slot is the shadow-stack root
    /// itself. A [`VarStorage::Frame`] local lives in the heap async frame,
    /// traced by the frame's own map rather than by the shadow stack, and it
    /// needs the same treatment for the same reason: the frame outlives every
    /// scope in the function and is traced until the task completes, so a local
    /// that was live across an `await` would otherwise be held by a scope that
    /// ended. A [`VarStorage::Value`] local holds no root at all, and a
    /// [`VarStorage::ReferencePtr`] one points into storage the callee does not
    /// own. The `is_gc_managed` test is what separates a rooted slot from the
    /// plain stack slot an address-taken local gets: clearing that one would
    /// zero a live variable.
    fn emit_lir_clear_scope_roots(&mut self, function: &LirFunction, locals: &[LirLocalId]) {
        enum Root {
            Slot(cranelift_codegen::ir::StackSlot),
            Frame(i32),
        }
        let roots: Vec<Root> = locals
            .iter()
            .filter_map(|local| function.locals.get(local.0 as usize))
            .filter_map(|local| match self.locals.vars.get(local.name.as_str()) {
                Some(VarStorage::Stack { slot, ty })
                    if local.is_gc_owner() || is_gc_managed(ty, self.enum_infos) =>
                {
                    Some(Root::Slot(*slot))
                }
                Some(VarStorage::Frame { offset, ty })
                    if local.is_gc_owner() || is_gc_managed(ty, self.enum_infos) =>
                {
                    Some(Root::Frame(*offset))
                }
                _ => None,
            })
            .collect();
        if roots.is_empty() {
            return;
        }
        let ptr_ty = reference_type(self.module.target_config());
        let zero = self.builder.ins().iconst(ptr_ty, 0);
        let frame_base = self.suspension.async_frame;
        for root in roots {
            match root {
                Root::Slot(slot) => self.stack_store(zero, slot),
                // Null removes an existing heap edge: SATB must retain its old
                // value even though the new value creates no edge.
                Root::Frame(offset) => {
                    if let Some(base) = frame_base {
                        self.emit_gc_heap_store_classified(
                            base,
                            offset,
                            zero,
                            true,
                            GcStoreDestination::AsyncFrameSlot,
                        );
                    }
                }
            }
        }
    }

    /// Give storage to every LIR local that nothing else binds.
    ///
    /// Two kinds of local need it. Lowering declares locals that carry a value
    /// ACROSS blocks — a conditional's merge result, a `match` scrutinee, a
    /// `match` arm's pattern bindings. They are written by
    /// [`LirInst::Assign`] or by [`LirInst::MatchBind`] and never by a
    /// [`LirInst::Let`], so neither [`FuncGen::bind_lir_gc_locals`] nor
    /// [`FuncGen::bind_coop_lir_locals`] sees them.
    ///
    /// The rest are ordinary `let`s whose [`LirInst::Let`] is EMITTED after a
    /// block that names them. A block index is not a position in any order
    /// control flows in, and an async body is emitted in index order, so a
    /// `lock` body and its continuation can both come out before the block
    /// holding the `let` they read: `let mut got = 0; lock m as v { got = v; }`
    /// lowers the two lock blocks ahead of the block that binds `got`. Binding
    /// at the `Let` alone is therefore too late for them.
    ///
    /// Either way the consequence is the same: with no entry binding the write
    /// is silently discarded and the read reaches codegen unbound. So every
    /// local gets its storage here, and [`LirInst::Let`] stores into whatever
    /// this left it — a binding point that does not depend on emission order.
    ///
    /// A local async liveness put in the heap frame is already bound and is
    /// skipped here — and that set is exactly the one that has to survive a
    /// poll return, so a Cranelift variable is sound for everything left.
    fn bind_lir_locals(&mut self, f: &LirFunction, unrooted: &HashSet<LirLocalId>) {
        for block in &f.blocks {
            for inst in &block.instrs {
                if let LirInst::Compute { value, .. } = inst {
                    for operand in value.operands() {
                        if let crate::ir::lowered::LirOperand::Reference {
                            place: crate::ir::lowered::LirPlace::Local(id),
                            ..
                        } = operand
                        {
                            self.locals
                                .address_taken
                                .insert(f.locals[id.0 as usize].name.clone());
                        }
                    }
                }
            }
        }
        let mut null = None;
        for local in &f.locals {
            if local.parameter || self.locals.vars.contains_key(local.name.as_str()) {
                continue;
            }
            // See `transient_roots`: no GC point can move these referents
            // while they are live, so they stay plain SSA values.
            if (local.is_gc_owner() || is_gc_managed(&local.ty, self.enum_infos))
                && !unrooted.contains(&local.id)
            {
                self.bind_lir_rooted_slot(&local.name, &local.ty, &mut null);
                continue;
            }
            if self.locals.address_taken.contains(local.name.as_str()) {
                // Give an address-taken local its definitive slot at function
                // entry so no `&` use inserts path-local promotion, and so the
                // one slot is the same one whichever block writes it first.
                let clif = self.clif_type(&local.ty);
                let zero = if clif == types::F64 {
                    self.builder.ins().f64const(0.0)
                } else {
                    self.emit_zero(clif)
                };
                let storage = self.create_local_stack_slot(&local.ty, zero);
                self.locals.vars.insert(local.name.clone(), storage);
                continue;
            }
            // Seeded so a path that reads the local without having run its
            // write — a `match` whose arms all diverge, a merge reached from a
            // branch that never assigned — still has a reaching definition,
            // including a match merge whose other arm diverges.
            let clif = self.clif_type(&local.ty);
            let var = self.builder.declare_var(clif);
            let zero = match clif {
                types::F64 => self.builder.ins().f64const(0.0),
                ty => self.emit_zero(ty),
            };
            self.builder.def_var(var, zero);
            self.locals
                .vars
                .insert(local.name.clone(), VarStorage::Value { var });
        }
    }
}

/// Tag bit in a closure's code word: the lifted body cannot raise, so a call
/// through it skips the panic-depth protocol. Function code
/// is at least 64-byte aligned (`log2_min_function_alignment`), so the bit is
/// never part of the address; callers mask it off before the call.
const CLOSURE_CODE_NO_PANIC: i64 = 1;

/// User calls can run synchronous safepoints. Builtin scalar/collection
/// operations and task creation remain on the ordinary poll stack.
/// Frame slots consumed by one callback, including reference owners/indices.
/// The physical layout guarantees parent slots for these locals. Print-only
/// boundaries retain liveness narrowing and add just missing operand slots.
pub(super) fn task_boundary_locals(
    function: &LirFunction,
    result: LirLocalId,
    value: &crate::ir::lowered::LirRvalue,
) -> Vec<LirLocalId> {
    let mut seen = HashSet::new();
    value
        .operands()
        .into_iter()
        .flat_map(|operand| operand.locals())
        .chain(std::iter::once(result))
        .filter(|id| {
            let local = &function.locals[id.0 as usize];
            (local.is_gc_owner() || !matches!(local.ty, Type::Void | Type::Never))
                && seen.insert(*id)
        })
        .collect()
}

pub(super) fn task_stack_boundary(value: &crate::ir::lowered::LirRvalue) -> bool {
    use crate::ir::lowered::LirRvalue as R;
    let params = match value {
        R::StaticCall { arg_types, .. }
        | R::ConstructorCall { arg_types, .. }
        | R::MethodCall { arg_types, .. }
        | R::EnumMethod { arg_types, .. }
        | R::IntrinsicCall { arg_types, .. } => arg_types.as_slice(),
        R::BuiltinCall { params, .. } => params.as_slice(),
        _ => &[],
    };
    if params
        .iter()
        .any(|ty| matches!(ty, Type::Fn(..) | Type::Closure(..)))
    {
        return true;
    }
    // Locks and ranges retain ordinary call boundaries even though their
    // nominal heads now have builtin identities.
    match value {
        R::DirectCall { .. }
        | R::IndirectCall { .. }
        | R::IntoError { .. }
        | R::Print { .. }
        | R::AwaitFuture { .. } => true,
        R::StaticCall { class, .. } | R::ConstructorCall { class, .. } => {
            crate::semantic::builtin_types::resolve(&Type::Named(*class))
                .is_none_or(|ty| matches!(ty.id, B::Mutex | B::RwLock | B::Range))
        }
        R::MethodCall { receiver_ty, .. } | R::EnumMethod { receiver_ty, .. } => {
            crate::semantic::builtin_types::resolve(receiver_ty)
                .is_none_or(|ty| matches!(ty.id, B::Mutex | B::RwLock | B::Range))
        }
        _ => false,
    }
}

pub(super) fn task_boundary_symbol(
    function: &LirFunction,
    block: usize,
    instruction: usize,
) -> String {
    format!("{}$task_boundary.{block}.{instruction}", function.name)
}

/// A bounded cleanup can run directly on the cooperative poll stack. Calls
/// into user code and cycles need a resumable native stack, including those
/// nested inside another deferred region.
pub(super) fn cleanup_needs_task_stack(function: &LirFunction) -> bool {
    let mut pending = vec![function];
    while let Some(region) = pending.pop() {
        if lir_sync_poll_blocks(region)
            .iter()
            .skip(1)
            .any(|poll| *poll)
            || region.blocks.iter().any(|block| {
                lir_block_successors(block).contains(&0)
                    || block.recovery.iter().any(|target| target.0 == 0)
            })
        {
            return true;
        }
        for instruction in region.blocks.iter().flat_map(|block| &block.instrs) {
            match instruction {
                LirInst::Compute { value, .. } if task_stack_boundary(value) => return true,
                LirInst::Defer { body, .. } => pending.push(&body.function),
                _ => {}
            }
        }
    }
    false
}

pub(super) fn task_cleanup_symbol(poll: u32, flag: i32, recovery: bool) -> String {
    format!("__willow_task_cleanup.{poll}.{flag}.{}", u8::from(recovery))
}

impl FuncGen<'_, '_> {
    pub(super) fn emit_task_cleanup_callback(&mut self, region: &LirFunction, recovery: bool) {
        self.cleanup.recover_eligible_depth = usize::from(recovery);
        let panic_exit = self.builder.create_block();
        self.cleanup.panic_return_block = Some(panic_exit);
        self.emit_lir_cleanup_region(region);
        if !self.locals.terminated {
            let ready = self.builder.ins().iconst(types::I32, 1);
            self.builder.ins().return_(&[ready]);
        }
        self.builder.switch_to_block(panic_exit);
        let depth = self.emit_value_runtime_call(rt::willow_root_depth, &[]);
        let baseline = self
            .cleanup
            .panic_function_root_depth
            .expect("cleanup root baseline");
        let count = self.builder.ins().isub(depth, baseline);
        self.emit_void_runtime_call(rt::willow_pop_roots, &[count]);
        let panicked = self
            .builder
            .ins()
            .iconst(types::I32, willow_abi::RuntimePollResult::Panicked as i64);
        self.builder.ins().return_(&[panicked]);
    }

    pub(super) fn emit_task_boundary_callback(
        &mut self,
        function: &LirFunction,
        local: LirLocalId,
        value: &crate::ir::lowered::LirRvalue,
        span: Span,
    ) {
        use crate::ir::lowered::{LirOperand, LirRvalue as R};
        for id in task_boundary_locals(function, local, value) {
            let binding = &function.locals[id.0 as usize];
            let offset = self.suspension.lir_frame_offsets[&id];
            self.locals.vars.insert(
                binding.name.clone(),
                VarStorage::async_local(binding, offset),
            );
            #[cfg(test)]
            super::async_codegen::count_callback_binding();
        }
        let panic_exit = self.builder.create_block();
        self.cleanup.panic_return_block = Some(panic_exit);
        if let R::MethodCall { method, .. } = value
            && self.flat_method_frame_enabled(method)
        {
            self.diagnostics
                .lir_call_frames
                .push((method.clone(), span));
            self.diagnostics.callstack_frame_depth = 1;
        }
        if value
            .operands()
            .iter()
            .any(|arg| matches!(arg, LirOperand::Reference { .. }))
        {
            self.diagnostics.lir_reference_scopes.push(Vec::new());
        }
        self.diagnostics.fault_site_span = Some(span);
        let result = self.emit_lir_rvalue(function, value, span);
        if !self.locals.terminated {
            if function.locals[local.0 as usize].is_gc_owner()
                || !matches!(
                    function.locals[local.0 as usize].ty,
                    Type::Void | Type::Never
                )
            {
                self.store_lir_local(function, local, result);
            }
            let ready = self.builder.ins().iconst(types::I32, 1);
            self.builder.ins().return_(&[ready]);
        }
        self.builder.switch_to_block(panic_exit);
        let depth = self.emit_value_runtime_call(rt::willow_root_depth, &[]);
        let baseline = self
            .cleanup
            .panic_function_root_depth
            .expect("boundary root baseline");
        let count = self.builder.ins().isub(depth, baseline);
        self.emit_void_runtime_call(rt::willow_pop_roots, &[count]);
        let ready = self
            .builder
            .ins()
            .iconst(types::I32, willow_abi::RuntimePollResult::Panicked as i64);
        self.builder.ins().return_(&[ready]);
    }

    pub(super) fn emit_task_boundary(
        &mut self,
        value: Option<&crate::ir::lowered::LirRvalue>,
        symbol: &str,
        suspends: &mut CoopSuspendPoints,
        frame: cranelift_codegen::ir::Value,
    ) {
        use crate::ir::lowered::{LirOperand, LirRvalue as R};
        let resume = self.builder.create_block();
        self.builder.ins().jump(resume, &[]);
        self.builder.switch_to_block(resume);
        let callback = self.func_ids[symbol];
        let callback = self
            .module
            .declare_func_in_func(callback, self.builder.func);
        let address = self
            .builder
            .ins()
            .func_addr(reference_type(self.module.target_config()), callback);
        // Explicitly handle suspension before panic propagation or observing
        // the result slot. Runtime-call wrappers cannot express this split.
        self.emit_fault_site();
        let enter = self.func_ids[rt::willow_task_stack_enter_generated];
        let enter = self.module.declare_func_in_func(enter, self.builder.func);
        let call = self.builder.ins().call(enter, &[address, frame]);
        let status = self.builder.inst_results(call)[0];
        self.emit_void_runtime_call(rt::willow_task_stack_leave, &[]);
        let ready = self.builder.create_block();
        let pending = self.builder.create_block();
        let ready_status = self.builder.ins().icmp_imm_s(IntCC::Equal, status, 1);
        let panic_status = self.builder.ins().icmp_imm_s(
            IntCC::Equal,
            status,
            willow_abi::RuntimePollResult::Panicked as i64,
        );
        let done = self.builder.ins().bor(ready_status, panic_status);
        self.builder.ins().brif(done, ready, &[], pending, &[]);
        self.builder.switch_to_block(pending);
        let state = self
            .builder
            .ins()
            .iconst(types::I64, (suspends.len() + 1) as i64);
        self.builder
            .ins()
            .store(MemFlagsData::new(), state, frame, 0);
        self.emit_coop_unwind_poll_roots();
        self.builder.ins().return_(&[status]);
        self.record_coop_suspend(suspends, resume);
        self.builder.switch_to_block(ready);
        if let Some(R::MethodCall { method, .. }) = value
            && self.flat_method_frame_enabled(method)
        {
            self.diagnostics
                .lir_call_frames
                .pop()
                .expect("prepared boundary method");
            self.emit_callstack_pop();
        }
        if value.is_some_and(|value| {
            value
                .operands()
                .iter()
                .any(|arg| matches!(arg, LirOperand::Reference { .. }))
        }) {
            self.emit_flat_reference_call_end();
        }
        // The callback reports an unwind explicitly. Comparing TLS depth
        // here would lose a panic raised before an earlier suspension.
        let panicked = self.builder.create_block();
        let normal = self.builder.create_block();
        self.builder
            .ins()
            .brif(panic_status, panicked, &[], normal, &[]);
        self.builder.switch_to_block(panicked);
        self.emit_sync_panic_unwind();
        self.builder.switch_to_block(normal);
        self.locals.terminated = false;
        self.cleanup.panic_depth_snapshot = None;
    }
}

#[cfg(test)]
#[path = "assignable_repr_tests.rs"]
mod assignable_repr_tests;
