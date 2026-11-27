//! Lowered IR (LIR): the typed HIR with control flow made explicit as basic
//! blocks — typed HIR lowering, the `Lowered IR` stage of the pipeline mandated by the
//! project conventions (`AST → Typed AST → Lowered IR → Cranelift IR`).
//!
//! Statement-level control flow becomes blocks and terminators:
//!
//! ```text
//! if    → condition branch + then block + else block + merge block
//! while → loop header + loop body + loop exit
//! for   → desugared to a while-shaped header/body/exit with an induction
//!         variable (index-based for arrays, bound-based for ranges)
//! ```
//!
//! Expression control flow, including nested ternaries, matches, short-circuit
//! operators and propagation, becomes explicit CFG before backend emission.
//! A private `Source*` construction graph temporarily retains typed expressions
//! while the worklist schedules operands, captures, coercions and allocations.
//! The public executable graph in `final_ir` contains only local/immediate
//! operands and shallow rvalues; lambdas are lifted functions and defers own
//! reusable cleanup CFG regions. No HIR expression reaches backend emission.
//! Every user body, including static initializers, uses this graph.
//! Unsupported lowering is diagnosed; `--emit-lir` renders the final program.

mod expressions;
mod statements;

use crate::diagnostics::Span;
use crate::driver::OverflowChecks;
use crate::parser::ast::{ExprId, LockMode};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::ids::{FunctionId, SemanticType as Type, TypeId};
use crate::semantic::intrinsics::{AdtMethod, BuiltinMethod, Intrinsic};
use crate::semantic::type_checker::types::{await_output_type, awaitable_task_type};
use std::borrow::Cow;
use std::collections::HashMap;

use super::typed_ast::{
    HirCallBinding, HirCapture, HirDeferBody, HirDeferId, HirExpr, HirExprKind, HirFunction,
    HirGuard, HirParam, HirPattern, HirProgram, HirStmt,
};

pub mod async_liveness;
pub mod cleanup;
mod lifetime;
mod liveness;
pub(crate) use liveness::normal_successors;
mod declarations;
mod defer_captures;
mod dispatch;
use declarations::LoweringDeclarations;
mod nested_cfg;
pub mod value;
pub use value::{LirOperand, LirPlace, LirRvalue};

use async_liveness::LirAsyncFrameLayout;

/// Borrowed instance-call operands shared by receiver preparation and call emission.
#[derive(Clone, Copy)]
struct MethodOperands<'a> {
    object: &'a HirExpr,
    method: &'a crate::parser::ast::MethodSpelling,
    args: &'a [HirExpr],
    super_class: Option<TypeId>,
}

/// A whole program in lowered IR.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceProgram {
    pub functions: Vec<SourceFunction>,
    /// Lambda bodies, lifted out of the expressions that contain them. They are kept apart
    /// from `functions` because the LIR
    /// cannot name them: the backend assigns each lambda its `$lambda.N`
    /// symbol, so the pairing is by span and the name is filled in there.
    pub lambdas: Vec<SourceLambda>,
}

/// One lifted lambda body, keyed by the span of the lambda expression it came
/// from — the same key the backend's own lambda table uses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceLambda {
    pub id: ExprId,
    pub span: Span,
    pub function: SourceFunction,
}

/// One function as a basic-block graph. `blocks[0]` is the entry block.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceFunction {
    pub name: FunctionId,
    pub is_async: bool,
    pub params: Vec<HirParam>,
    pub return_type: Type,
    pub blocks: Vec<SourceBlock>,
    /// Stable identities for parameters, source locals, and LIR-synthesized
    /// temporaries. Source spans are diagnostic metadata only.
    pub locals: Vec<LirLocal>,
    /// Frame ownership belongs to LIR: this is computed from the final CFG,
    /// after synthetic locals and explicit suspension edges exist.
    pub async_frame: LirAsyncFrameLayout,
    /// The captured environment this body is entered with, in slot order. Empty for every
    /// function that is not a lifted
    /// capturing lambda. Each entry is also a `parameter` local, because the
    /// backend binds it at entry exactly like a parameter — the difference is
    /// only where the value comes from: environment word `i + 1` instead of an
    /// argument register.
    pub captures: Vec<LirCapture>,
}

/// One slot of a lifted lambda's closure environment.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LirCapture {
    /// The local this slot binds inside the lifted body.
    pub name: String,
    pub ty: Type,
    /// The name the value is read from in the ENCLOSING function when the
    /// environment is built.
    pub source: String,
}

impl SourceFunction {
    /// Visit executable expression roots, including deferred statement bodies.
    /// Consumers decide whether to descend into each expression's operands.
    pub(crate) fn visit_expr_roots_mut(&mut self, mut visit: impl FnMut(&mut HirExpr)) {
        let mut functions = vec![self];
        while let Some(function) = functions.pop() {
            for block in &mut function.blocks {
                for instruction in &mut block.instrs {
                    match instruction {
                        SourceInst::Let { value, .. }
                        | SourceInst::Assign { value, .. }
                        | SourceInst::StaticFieldAssign { value, .. }
                        | SourceInst::Expr(value) => visit(value),
                        SourceInst::FieldAssign { object, value, .. } => {
                            visit(object);
                            visit(value);
                        }
                        SourceInst::IndexAssign {
                            array,
                            index,
                            value,
                        } => {
                            visit(array);
                            visit(index);
                            visit(value);
                        }
                        SourceInst::SuperInit { args, .. } => args.iter_mut().for_each(&mut visit),
                        SourceInst::Defer { body, .. } => functions.push(body.function.as_mut()),
                        SourceInst::Compute { .. }
                        | SourceInst::EnterDeferScope { .. }
                        | SourceInst::LeaveDeferScope { .. }
                        | SourceInst::FlushDefers { .. }
                        | SourceInst::ClearScopeRoots { .. }
                        | SourceInst::ReleaseLock(_)
                        | SourceInst::MatchTest { .. }
                        | SourceInst::MatchBind { .. }
                        | SourceInst::SelectInit { .. }
                        | SourceInst::SelectProbe { .. }
                        | SourceInst::SelectPick { .. }
                        | SourceInst::SelectUnregister { .. }
                        | SourceInst::SelectCommit { .. } => {}
                    }
                }
                match &mut block.terminator {
                    SourceTerminator::Branch { cond, .. }
                    | SourceTerminator::Return(Some(cond)) => visit(cond),
                    SourceTerminator::Jump(_)
                    | SourceTerminator::Suspend { .. }
                    | SourceTerminator::Return(None)
                    | SourceTerminator::CleanupReturn => {}
                }
            }
        }
    }

    /// Local names are unique within a function, so resolving a
    /// `HirExprKind::Var` by name yields exactly one [`LirLocalId`].
    ///
    /// Two independent things rely on this: `Builder::local_by_name` resolves
    /// assignment targets without a scope stack, and `async_liveness` maps
    /// names back to ids when it decides what the async frame must hold. If a
    /// name ever covered two locals, the second would win in both maps and the
    /// first local's frame slot would be aliased — a value of one type read
    /// through another's slot, including through the GC trace mask.
    ///
    /// Source shadowing does not break it because `LowerCtx::bind` already
    /// alpha-renames shadowing bindings to `name$n`, and LIR's own temporaries
    /// come from `Builder::synthetic_name`, which is counter-driven. This is a
    /// debug assertion rather than a type-level guarantee because the fix, if
    /// it ever fires, belongs in whichever of those two allocators regressed.
    fn assert_unique_local_names(&self) {
        if !cfg!(debug_assertions) {
            return;
        }
        let mut seen = std::collections::HashSet::with_capacity(self.locals.len());
        for local in &self.locals {
            assert!(
                seen.insert(local.name.as_str()),
                "LIR function `{}` has two locals named `{}` ({:?} and a later one); \
                 HIR alpha-renaming or synthetic-name allocation regressed",
                self.name,
                local.name,
                local.id,
            );
        }
    }
}

/// Function-local variable identity. Unlike a source span it also exists for
/// compiler-generated bindings and cannot alias another declaration.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct LirLocalId(pub u32);

/// Stable identity of one source `defer` registration site. Its source span
/// is diagnostic metadata; frame flags and cancellation cleanup use this id.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct LirDeferId(pub u32);

/// Physical storage is independent of source-language types. GcOwner holds
/// an opaque managed allocation base used only by lowered reference places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum LirStorageKind {
    #[default]
    Value,
    GcOwner,
    /// An async borrowed parameter stores a traced owner and scalar offset.
    ReferenceCell,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LirLocal {
    pub storage_kind: LirStorageKind,
    pub id: LirLocalId,
    pub name: String,
    pub ty: Type,
    pub source_span: Option<Span>,
    pub synthetic: bool,
    pub parameter: bool,
}

impl LirLocal {
    pub fn is_gc_owner(&self) -> bool {
        self.storage_kind == LirStorageKind::GcOwner
    }
}

/// A basic-block index into [`SourceFunction::blocks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BlockId(pub usize);

/// A straight-line run of instructions ended by exactly one terminator.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceBlock {
    pub id: BlockId,
    pub instrs: Vec<SourceInst>,
    pub terminator: SourceTerminator,
    /// Where a panic raised in this block can continue: the resume block of
    /// every enclosing recovery-capable `defer` scope, innermost first.
    ///
    ///
    /// This edge exists in no terminator because a panic can leave from ANY
    /// point in the block — a call, an index, a division — and the cleanup CFG
    /// that runs the defers is built by the backend. It still belongs in the
    /// LIR graph: a local whose only live use after the panic is on this edge
    /// is live across every suspension in the scope, and the async frame
    /// layout is computed from these edges. Without it such a local looks dead
    /// across the suspension, gets no frame slot, and reads back as zero after
    /// the recovered poll re-enters.
    ///
    /// Empty for a synchronous function: its recovery resumes inside one
    /// native frame, so nothing has to survive a poll return.
    pub recovery: Vec<BlockId>,
}

/// A scheduler-visible operation. Its operands are stable LIR locals, never
/// source spans or backend-created Cranelift values.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum SuspendOp {
    Sleep {
        /// Dedicated synthetic operand slot; codegen reuses it for the sleep
        /// deadline across wakes. Never aliases a source-level local.
        millis: LirLocalId,
    },
    Yield,
    AwaitTask {
        task: LirLocalId,
        result: Option<LirLocalId>,
        result_ty: Type,
        /// The operand was a `TaskResult<T>` (`await t.result()`), so a
        /// cancelled task yields `Err(Cancelled)` instead of raising. Recorded
        /// here for the same reason [`SuspendOp::ChannelSend::elem_ty`] is:
        /// `Task<T>` and `TaskResult<T>` are the SAME frame pointer, so the
        /// backend cannot recover the distinction from the value it loads.
        cancel_aware: bool,
    },
    ChannelSend {
        channel: LirLocalId,
        value: LirLocalId,
        elem_ty: Type,
    },
    ChannelRecv {
        channel: LirLocalId,
        result: Option<LirLocalId>,
        elem_ty: Type,
        /// `recv_opt()`: `result` is `Option<elem_ty>` and a
        /// closed, drained channel yields `None` instead of raising.
        closed_aware: bool,
    },
    SelectWait {
        operations: Vec<LirSelectWaitOp>,
    },
    /// Acquire the critical section of a `lock`/`read`/`write` statement.
    ///
    ///
    /// A contended acquisition parks, so this is a suspension like any other —
    /// but unlike the rest it is a suspension the RESUME does not simply follow:
    /// the resumed poll re-polls the acquisition and parks again while the lock
    /// is still held elsewhere. `resume` is the critical section's first block,
    /// reached only once the section is owned.
    ///
    /// All four operands are LIR locals so that liveness gives each one an async
    /// frame slot: the evaluated handle (a GC object, traced), this
    /// acquisition's registration token and phase (plain words), and the
    /// binding the protected value is loaded into. Nothing here may live on the
    /// native stack — a park returns out of the poll function entirely.
    LockAcquire {
        slots: LirLockSlots,
        /// The `lock` keyword's location, for the reentrancy panic.
        span: Span,
    },
    Preempt,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LirSelectWaitOp {
    Recv {
        channel: LirLocalId,
    },
    Send {
        channel: LirLocalId,
        value: LirLocalId,
    },
    Join {
        task: LirLocalId,
    },
    Timeout {
        deadline: LirLocalId,
    },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum LirSelectOp {
    Recv {
        channel: LirLocalId,
        binding: Option<LirLocalId>,
        elem_ty: Type,
        /// `let v = ch.recv_opt() =>`: `binding` is `Option<elem_ty>`, `None`
        /// when the case was selected because the channel closed.
        closed_aware: bool,
    },
    Send {
        channel: LirLocalId,
        value: LirLocalId,
        elem_ty: Type,
    },
    Join {
        task: LirLocalId,
        binding: Option<LirLocalId>,
        result_ty: Type,
        /// See [`SuspendOp::AwaitTask::cancel_aware`]: a `TaskResult<T>` case
        /// binds `Result<T, Cancelled>` and never raises on cancellation.
        cancel_aware: bool,
    },
    Timeout {
        millis: LirLocalId,
        deadline: LirLocalId,
    },
    Default,
}

impl LirSelectOp {
    fn wait_op(&self) -> Option<LirSelectWaitOp> {
        match self {
            LirSelectOp::Recv { channel, .. } => Some(LirSelectWaitOp::Recv { channel: *channel }),
            LirSelectOp::Send { channel, value, .. } => Some(LirSelectWaitOp::Send {
                channel: *channel,
                value: *value,
            }),
            LirSelectOp::Join { task, .. } => Some(LirSelectWaitOp::Join { task: *task }),
            LirSelectOp::Timeout { deadline, .. } => Some(LirSelectWaitOp::Timeout {
                deadline: *deadline,
            }),
            LirSelectOp::Default => None,
        }
    }
}

impl SuspendOp {
    pub(crate) fn for_each_local(&self, mut insert: impl FnMut(LirLocalId)) {
        match self {
            SuspendOp::Sleep { millis } => insert(*millis),
            SuspendOp::Yield | SuspendOp::Preempt => {}
            SuspendOp::AwaitTask { task, result, .. }
            | SuspendOp::ChannelRecv {
                channel: task,
                result,
                ..
            } => {
                insert(*task);
                if let Some(result) = result {
                    insert(*result);
                }
            }
            SuspendOp::ChannelSend { channel, value, .. } => {
                insert(*channel);
                insert(*value);
            }
            SuspendOp::LockAcquire { slots, .. } => {
                for local in slots.all_locals() {
                    insert(local);
                }
            }
            SuspendOp::SelectWait { operations } => {
                for operation in operations {
                    match operation {
                        LirSelectWaitOp::Recv { channel }
                        | LirSelectWaitOp::Join { task: channel } => insert(*channel),
                        LirSelectWaitOp::Send { channel, value } => {
                            insert(*channel);
                            insert(*value);
                        }
                        LirSelectWaitOp::Timeout { deadline } => insert(*deadline),
                    }
                }
            }
        }
    }
}

/// A cleanup CFG replayed at each exit where this registration is live.
/// Captures name enclosing locals; region parameter locals reuse their names
/// and storage. Region-owned locals receive fresh storage at each replay.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceDeferBody {
    pub function: Box<SourceFunction>,
    pub captures: Vec<LirLocalId>,
    pub recovery_capable: bool,
}

/// The frame-backed slots one critical section owns.
///
/// The same four locals appear at the acquisition, at every release, and on the
/// scope that owns the section's panic cleanup, so they travel together: a
/// consumer that gets them from one of the three is looking at the same lock.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct LirLockSlots {
    /// Source-order members of one runtime-ordered acquisition group.
    pub members: Vec<LirLockSlots>,
    /// Which state machine the runtime calls — `Mutex` and `RwLock` have
    /// separate acquire/poll/load/release entry points.
    pub mode: LockMode,
    /// The evaluated lock handle. A GC object, so its slot is traced.
    pub handle: LirLocalId,
    /// This acquisition's registration token, proving ownership at release.
    pub token: LirLocalId,
    /// 0 before the protected value is loaded, 1 after — what tells a release
    /// whether there is anything to commit.
    pub phase: LirLocalId,
    /// The `as` binding the protected value is loaded into.
    pub binding: LirLocalId,
    /// The protected type — the element of the target's `Mutex<T>`/`RwLock<T>`.
    /// Carried because the runtime hands the value back as a word and only this
    /// says how to read it.
    pub value_ty: Type,
}

impl LirLockSlots {
    /// The four locals, in the order the runtime hooks take them.
    pub fn locals(&self) -> [LirLocalId; 4] {
        [self.handle, self.token, self.phase, self.binding]
    }
    pub fn all_locals(&self) -> impl Iterator<Item = LirLocalId> + '_ {
        self.locals()
            .into_iter()
            .chain(self.members.iter().flat_map(|m| m.locals()))
    }
}

/// A non-branching instruction. Values are typed HIR expression trees.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SourceInst {
    /// One flat computation with explicit scalar operands.
    Compute {
        local: LirLocalId,
        value: LirRvalue,
        span: Span,
    },
    /// Open a lexical scope that owns `sites` defer registrations.
    ///
    ///
    /// `defer` is a LEXICAL construct — a defer in a loop body runs once per
    /// iteration — but the LIR is a flat block graph with no scopes of its
    /// own, so the boundaries are instructions. Every scope opened here is
    /// closed by exactly one [`SourceInst::LeaveDeferScope`] on the fallthrough
    /// path, and every early exit out of it is preceded by a
    /// [`SourceInst::FlushDefers`].
    ///
    /// `sites` are the spans of the `defer` statements this scope contains, in
    /// source order. A consumer needs them BEFORE the first registration runs:
    /// a panic can leave the scope from a point where only some of the defers
    /// have registered, so each site needs a cleared flag at scope entry.
    EnterDeferScope {
        sites: Vec<(LirDeferId, Span)>,
        /// Recovery resumes at the first block after this lexical scope.
        /// Async scopes always carry it because their state machine needs an
        /// explicit continuation; synchronous scopes carry it when one of
        /// their defers can recover a panic.
        resume: Option<BlockId>,
        /// Set when this scope IS a `lock` body. The scope
        /// is what gives the critical section a panic cleanup block, so the
        /// section is identified here rather than at the acquisition: a
        /// consumer walks blocks in index order, which a suspension split can
        /// reorder relative to the source, and only this pairing survives that.
        lock: Option<LirLockSlots>,
    },
    /// Close the scope opened by the matching [`SourceInst::EnterDeferScope`]:
    /// run its registrations (newest first) and pop it. This is the
    /// FALLTHROUGH exit — an early exit uses [`SourceInst::FlushDefers`] and
    /// leaves the scope structure in place for the paths that did not take it.
    LeaveDeferScope {
        sites: Vec<LirDeferId>,
    },
    /// Run the named registrations newest first, without changing lexical
    /// scope metadata. Emitted immediately before a
    /// `return`, `break` or `continue` that leaves their scopes.
    FlushDefers {
        sites: Vec<LirDeferId>,
        /// Innermost open scopes this exit leaves. Explicit because a `lock`
        /// scope has no sites, so the sites alone cannot say where it stops.
        scopes: usize,
    },
    /// The end of the lexical scope that declared `locals`.
    ///
    /// A GC-managed local gets ONE slot for the whole function and that slot is
    /// its root, so nothing stops a loop body's binding from keeping the last
    /// iteration's object reachable until the function returns — where the AST
    /// emitter drops that root when the scope ends. This marks the boundary so
    /// the emitter can clear the slots back to null.
    ///
    /// Every SOURCE local the scope declared is listed, nested scopes included:
    /// which of them actually hold a GC root is a back-end question — it needs
    /// the enum table — and clearing an already-cleared slot is a dead store.
    /// Compiler-generated temporaries are deliberately left out; some of them
    /// carry a value out of the scope that declared them.
    ///
    /// Fallthrough only, exactly like [`SourceInst::LeaveDeferScope`]: a `break`,
    /// `continue` or `return` leaves without passing this instruction, and a
    /// `return` pops every root anyway.
    ClearScopeRoots {
        locals: Vec<LirLocalId>,
    },
    /// `defer` registration. `span` is the `defer` statement's
    /// own span — the key its scope's cleanup flag is registered under, so it
    /// must match the entry in the enclosing `EnterDeferScope::sites`.
    Defer {
        id: LirDeferId,
        body: SourceDeferBody,
        span: Span,
    },
    Let {
        local: LirLocalId,
        name: String,
        mutable: bool,
        /// Declaration identity used by async frame layout/liveness.
        span: Span,
        /// The type the name is bound with — the annotation when the source
        /// wrote one, otherwise `value.ty`. A consumer must size and type the
        /// variable's storage from this, because `let a: Animal = new Dog();`
        /// binds `a` as the interface while the initialiser is the class.
        ///
        ty: Type,
        value: HirExpr,
    },
    Assign {
        local: LirLocalId,
        name: String,
        value: HirExpr,
    },
    FieldAssign {
        object: HirExpr,
        field: String,
        value: HirExpr,
    },
    IndexAssign {
        array: HirExpr,
        index: HirExpr,
        value: HirExpr,
    },
    StaticFieldAssign {
        class: TypeId,
        field: String,
        value: HirExpr,
    },
    SuperInit {
        args: Vec<HirExpr>,
        /// The `super.init(...)` statement's own span. A zero-argument call has
        /// no expression to borrow a position from, and the emitted call still
        /// needs one for its panic call-chain frame.
        span: Span,
    },
    SelectInit {
        operations: Vec<LirSelectOp>,
    },
    SelectProbe {
        operations: Vec<LirSelectOp>,
        ready: Vec<Option<LirLocalId>>,
    },
    SelectPick {
        ready: Vec<Option<LirLocalId>>,
        chosen: LirLocalId,
    },
    SelectUnregister {
        operations: Vec<LirSelectOp>,
        winner: LirLocalId,
    },
    SelectCommit {
        operation: LirSelectOp,
        success: LirLocalId,
    },
    /// Commit the protected value and release the critical section opened by a
    /// [`SuspendOp::LockAcquire`].
    ///
    /// Emitted on every exit that LEAVES the section under its own power:
    /// fallthrough off the end, and a `return`/`break`/`continue` that jumps
    /// past it. The section's own `defer`s run first — still holding the lock —
    /// and the enclosing scopes' `defer`s run after, which is what a
    /// [`SourceInst::FlushDefers`] on either side of this instruction expresses.
    ///
    /// The panic path is deliberately absent: an unwind releases the lock from
    /// the section's cleanup block instead, so it needs no instruction of its
    /// own. Running twice is harmless — the release is guarded by the handle
    /// slot, which it clears.
    ReleaseLock(LirLockSlots),
    /// Does the scrutinee match one arm's pattern?
    ///
    /// Emitted only when lowering has split a `match` into blocks because an
    /// arm suspends. Nonsuspending matches use the same explicit pattern
    /// instructions and branch structure.
    ///
    /// The scrutinee is a local rather than an expression because every arm
    /// tests the SAME value: the source evaluates it once, and the tests are
    /// spread over a chain of dispatch blocks. `result` is a `Bool` local, not
    /// a value, for the same reason [`SourceTerminator::Branch`] reads one — the
    /// branch that consumes it is the block's terminator.
    MatchTest {
        scrutinee: LirLocalId,
        pattern: HirPattern,
        result: LirLocalId,
        span: Span,
    },
    /// Bring one arm's pattern bindings into their own LIR locals.
    ///
    ///
    /// `bindings` are positional: one local per binding the pattern names, in
    /// the order [`HirPattern`] lists them. They are LIR locals rather than
    /// backend-scoped variables because an arm body may suspend after reading
    /// one, and only a local can be given a frame slot.
    ///
    /// Emitted at the top of the arm's own block, where the test has already
    /// proved the pattern applies.
    MatchBind {
        scrutinee: LirLocalId,
        pattern: HirPattern,
        bindings: Vec<LirLocalId>,
        span: Span,
    },
    /// A bare expression evaluated for its effect.
    Expr(HirExpr),
}

/// How a block ends.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum SourceTerminator {
    /// Unconditional jump.
    Jump(BlockId),
    /// Two-way branch on a `Bool` condition.
    Branch {
        cond: HirExpr,
        then_block: BlockId,
        else_block: BlockId,
    },
    /// Return control to the scheduler and re-enter through `resume`.
    Suspend {
        operation: SuspendOp,
        resume: BlockId,
    },
    /// Function return.
    Return(Option<HirExpr>),
    /// Exit a replayed cleanup region, not the enclosing user function.
    CleanupReturn,
}

#[cfg(test)]
thread_local! {
    static FINAL_ASYNC_ANALYSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Lower every function (free functions and class methods, flattened as
/// `Class::method`) of a typed-HIR program to basic blocks, with debug-build
/// overflow checks (the conservative choice for callers without a mode).
pub(crate) fn lower_source_program(program: &HirProgram) -> SourceProgram {
    lower_source_program_with(program, true.into())
}

/// The resolved policy determines which integer arithmetic is a fault that
/// leaf/recursive inlining must not move across a call-stack frame.
pub(crate) fn lower_source_program_with(
    program: &HirProgram,
    overflow_checks: OverflowChecks,
) -> SourceProgram {
    let resolution = LoweringDeclarations::new(&program.resolution);
    let mut functions = Vec::with_capacity(program.functions.len());
    let mut lambdas = Vec::new();
    for f in &program.functions {
        functions.push(lower_function(f, None, &resolution));
        collect_lambdas(&f.body, &mut lambdas, &resolution);
    }
    for c in &program.classes {
        for m in &c.methods {
            functions.push(lower_function(m, Some(&c.name), &resolution));
            collect_lambdas(&m.body, &mut lambdas, &resolution);
        }
    }
    value::lower_calls(&mut functions, &mut lambdas, &resolution);
    // Calls through a value that can only be one named function become
    // direct calls before leaf inlining selects its call sites.
    let direct = synchronous_function_indices(&functions);
    for function in functions
        .iter_mut()
        .chain(lambdas.iter_mut().map(|lambda| &mut lambda.function))
    {
        super::optimize::devirtualize_function_values(function, &direct);
    }
    super::optimize::inline_scalar_leaves(&mut functions, overflow_checks);
    // The lifted graph is the sole owner of each executable lambda body.
    // Enclosing expressions retain only closure construction metadata.
    for function in functions
        .iter_mut()
        .chain(lambdas.iter_mut().map(|lambda| &mut lambda.function))
    {
        super::optimize::eliminate_tail_recursion(function);
        super::optimize::inline_scalar_recursion(function, overflow_checks);
        super::optimize::unroll_scalar_loops(function);
    }
    super::optimize::fuse_program(&mut functions, &mut lambdas, &direct);
    drop(direct);
    for function in functions
        .iter_mut()
        .chain(lambdas.iter_mut().map(|lambda| &mut lambda.function))
    {
        let liveness = lifetime::clear_dead_temporaries(function);
        function.async_frame = if function.is_async {
            #[cfg(test)]
            FINAL_ASYNC_ANALYSES.with(|count| count.set(count.get() + 1));
            async_liveness::analyze_with(&function.blocks, &function.locals, &liveness)
        } else {
            LirAsyncFrameLayout::default()
        };
        function.visit_expr_roots_mut(|expr| {
            expr.visit_mut_preorder(true, |node| {
                if let HirExprKind::Lambda { body, .. } = &mut node.kind {
                    body.clear();
                    false
                } else {
                    true
                }
            })
        });
    }
    SourceProgram { functions, lambdas }
}

#[cfg(test)]
thread_local! {
    pub(in crate::ir) static DIRECT_INDEX_WORK: std::cell::Cell<(usize,usize)> = const { std::cell::Cell::new((0,0)) };
}

/// Both devirtualization and effect lookup use the function order established by lowering.
pub(in crate::ir) fn synchronous_function_indices(
    functions: &[SourceFunction],
) -> HashMap<FunctionId, usize> {
    let index: HashMap<_, _> = functions
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.is_async)
        .map(|(i, f)| (f.name, i))
        .collect();
    #[cfg(test)]
    DIRECT_INDEX_WORK.set((DIRECT_INDEX_WORK.get().0 + 1, index.len()));
    index
}

/// Lift every lambda in a statement body, innermost first, into its own block
/// graph.
///
/// A lambda body is not part of the enclosing function's control flow — it is a
/// separate function the backend compiles under its own symbol — so lowering it
/// inline would put its blocks in the wrong graph. The walk goes through
/// [`HirExpr::children`], whose `Lambda` case yields the body's expressions, so
/// a lambda nested inside another lambda is reached the same way as one nested
/// in a call argument.
fn collect_lambdas<'r>(
    body: &[HirStmt],
    out: &mut Vec<SourceLambda>,
    resolution: &'r LoweringDeclarations<'r>,
) {
    for stmt in body {
        for expr in stmt.child_exprs() {
            collect_lambdas_in_expr(expr, out, resolution);
        }
    }
}

fn collect_lambdas_in_expr<'r>(
    expr: &HirExpr,
    out: &mut Vec<SourceLambda>,
    resolution: &'r LoweringDeclarations<'r>,
) {
    for expr in expr.walk_postorder(true) {
        collect_lambda(expr, out, resolution);
    }
}

fn collect_lambda<'r>(
    expr: &HirExpr,
    out: &mut Vec<SourceLambda>,
    resolution: &'r LoweringDeclarations<'r>,
) {
    if let HirExprKind::Lambda {
        id,
        params,
        captures,
        body,
    } = &expr.kind
    {
        // The callable type the checker gave the lambda expression is the only
        // place the return type lives: the HIR params carry their own types,
        // but a lambda has no declared return type node of its own.
        let (Type::Fn(_, ret) | Type::Closure(_, ret)) = &expr.ty else {
            return;
        };
        let mut b = Builder::new_lambda(params, captures, resolution);
        b.return_type = (**ret).clone();
        // A lifted lambda owns a function scope just like a named function.
        // Lowering only its statements would leave top-level defer sites
        // without the scope metadata that assigns their stable identities.
        b.lower_scope(body);
        let (blocks, locals) = b.finish();
        let function = SourceFunction {
            name: FunctionId::free(lambda_placeholder_name(*id)),
            is_async: false,
            params: params.clone(),
            return_type: (**ret).clone(),
            blocks,
            locals,
            async_frame: LirAsyncFrameLayout::default(),
            captures: captures
                .iter()
                .map(|c| LirCapture {
                    name: c.name.clone(),
                    ty: c.ty.clone(),
                    source: c.source.clone(),
                })
                .collect(),
        };
        function.assert_unique_local_names();
        out.push(SourceLambda {
            id: *id,
            span: expr.span,
            function,
        });
    }
}

/// The name a lifted lambda carries until the backend renames it to the
/// `$lambda.N` symbol it declared. Derived from the span so `--emit-lir` output
/// is stable and two lambdas never collide.
pub fn lambda_placeholder_name(id: ExprId) -> String {
    format!("$lambda@{id}")
}

pub(crate) fn reference_place_name(place: &HirExpr) -> String {
    let mut current = place;
    let mut suffixes = Vec::new();
    let mut out = loop {
        match &current.kind {
            HirExprKind::Var(name) => break name.clone(),
            HirExprKind::FieldAccess { object, field } => {
                suffixes.push((Some(field.as_str()), None));
                current = object;
            }
            HirExprKind::Index { array, index } => {
                suffixes.push((None, Some(index.as_ref())));
                current = array;
            }
            ir_variants!(
                HirExprKind;
                Int, Float, Bool, Str, FnRef, Binary, Unary, Call, Invoke, Print, Array, Ternary,
                New, MethodCall, ObjectLiteral, StaticField, StaticCall, ReferenceArg, Range,
                Await, TryPropagate, Lambda, Match, Select
            ) => break "<expression>".into(),
        }
    };
    for (field, index) in suffixes.into_iter().rev() {
        if let Some(field) = field {
            out.push('.');
            out.push_str(field);
        } else if let Some(index) = index {
            out.push('[');
            match &index.kind {
                HirExprKind::Int(value) => out.push_str(&value.to_string()),
                HirExprKind::Var(name) => out.push_str(name),
                ir_variants!(
                    HirExprKind;
                    Float, Bool, Str, FnRef, Binary, Unary, Call, Invoke, Print, Array, Index,
                    Ternary, New, FieldAccess, MethodCall, ObjectLiteral, StaticField, StaticCall,
                    ReferenceArg, Range, Await, TryPropagate, Lambda, Match, Select
                ) => out.push_str("<expr>"),
            }
            out.push(']');
        }
    }
    out
}

fn format_template(args: &[HirExpr]) -> Option<&str> {
    let HirExprKind::Str(spec) = &args.first()?.kind else {
        return None;
    };
    Some(spec)
}

#[cfg(test)]
std::thread_local! {
    static LIVE_FORMAT_DESCRIPTOR_CAPACITY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static PEAK_FORMAT_DESCRIPTOR_CAPACITY: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn check_format(
    args: &[HirExpr],
    mut record: impl FnMut(crate::interpolate::SourceSegment),
) -> Option<crate::interpolate::CheckedTemplate<'_>> {
    use crate::interpolate::SourceSegment as Segment;
    let template = format_template(args)?;
    let mut operands = args[1..].iter();
    let mut valid = true;
    let parsed = crate::interpolate::visit_ranges(template, |segment| {
        valid &= match segment {
            Segment::Literal(_) => true,
            Segment::Display => operands.next().is_some_and(|arg| {
                matches!(
                    arg.ty,
                    Type::I8
                        | Type::I16
                        | Type::I32
                        | Type::I64
                        | Type::U8
                        | Type::U16
                        | Type::U32
                        | Type::U64
                        | Type::Char
                        | Type::F64
                        | Type::Bool
                        | Type::String
                )
            }),
            Segment::F64(_) => operands.next().is_some_and(|arg| arg.ty == Type::F64),
        };
        if valid {
            record(segment);
        }
    });
    parsed.ok().filter(|_| valid && operands.next().is_none())
}

// The descriptors belong to this call. Scheduling can borrow their ranges
// while pushing work; the template slices in that work borrow the HIR.
fn with_format_segments<'e>(
    args: &'e [HirExpr],
    schedule: impl FnOnce(&'e str, &[crate::interpolate::SourceSegment]),
) -> bool {
    let mut segments = Vec::new();
    let template = check_format(args, |segment| segments.push(segment));
    #[cfg(test)]
    {
        let live = LIVE_FORMAT_DESCRIPTOR_CAPACITY.get() + segments.capacity();
        LIVE_FORMAT_DESCRIPTOR_CAPACITY.set(live);
        PEAK_FORMAT_DESCRIPTOR_CAPACITY.set(PEAK_FORMAT_DESCRIPTOR_CAPACITY.get().max(live));
    }
    if let Some(spec) = template {
        schedule(spec.as_str(), &segments);
    }
    #[cfg(test)]
    LIVE_FORMAT_DESCRIPTOR_CAPACITY
        .set(LIVE_FORMAT_DESCRIPTOR_CAPACITY.get() - segments.capacity());
    template.is_some()
}

fn object_literal_supported(
    resolution: &LoweringDeclarations<'_>,
    class: &TypeId,
    fields: &[(String, HirExpr)],
) -> bool {
    resolution.fields_rev(class).is_some_and(|mut declared| {
        if declared.len() != fields.len() {
            return false;
        }
        let names: std::collections::HashSet<_> = fields.iter().map(|(name, _)| name).collect();
        names.len() == fields.len() && declared.all(|(name, _)| names.contains(name))
    })
}

fn arguments_compatible<'a>(
    resolution: &super::typed_ast::HirResolution,
    params: impl IntoIterator<Item = &'a Type, IntoIter: ExactSizeIterator>,
    args: &[HirExpr],
) -> bool {
    let params = params.into_iter();
    params.len() == args.len()
        && params.zip(args).all(|(target, argument)| {
            if matches!(argument.kind, HirExprKind::ReferenceArg { .. }) {
                *target == argument.ty
            } else {
                resolution.can_coerce(&argument.ty, target)
            }
        })
}

/// Class and module signatures can borrow the declaration. Interface dispatch
/// may specialize Self and type parameters for the receiver.
struct ResolvedSignature<'a, 'r> {
    declaration: &'a super::typed_ast::HirSignature,
    interface: Option<InterfaceReceiver<'a, 'r>>,
}

struct InterfaceReceiver<'a, 'r> {
    class: TypeId,
    type_params: &'a super::typed_ast::HirTypeParameters,
    args: Option<&'r [Type]>,
}

#[derive(Clone, Copy)]
struct InterfaceBindings<'a> {
    class: TypeId,
    args: Option<&'a [Type]>,
    type_params: &'a super::typed_ast::HirTypeParameters,
    self_id: TypeId,
}

impl<'a, 'r> InterfaceReceiver<'a, 'r> {
    fn bindings<'b>(&self) -> InterfaceBindings<'b>
    where
        'a: 'b,
        'r: 'b,
    {
        InterfaceBindings {
            class: self.class,
            args: self.args,
            type_params: self.type_params,
            self_id: TypeId::local("Self"),
        }
    }
}

impl InterfaceBindings<'_> {
    fn matches(&self, param: &Type, actual: &Type) -> bool {
        self.compare(param, actual, false)
    }

    fn compare(&self, param: &Type, actual: &Type, ignore_callable_kind: bool) -> bool {
        match_bound_parameter(
            param,
            actual,
            ignore_callable_kind,
            self.type_params,
            self.args.unwrap_or_default(),
            |id, ty| {
                if *id != self.self_id {
                    return None;
                }
                Some(match (self.args, ty) {
                    (None, Type::Named(id)) => self.class == *id,
                    (Some(args), Type::Generic(id, actual)) => {
                        self.class == *id
                            && args.len() == actual.len()
                            && args.iter().zip(actual).all(|(a, b)| {
                                if ignore_callable_kind {
                                    a.same_modulo_callable_kind(b)
                                } else {
                                    a == b
                                }
                            })
                    }
                    _ => false,
                })
            },
        )
    }

    fn specialize(&self, param: &Type) -> Type {
        #[cfg(test)]
        INTERFACE_SPECIALIZATIONS.set(INTERFACE_SPECIALIZATIONS.get() + 1);
        param.substitute_names(|id| {
            if let Some(i) = self.type_params.position(id) {
                Some(self.args.unwrap_or_default()[i].clone())
            } else if *id == self.self_id {
                Some(match self.args {
                    Some(args) => Type::Generic(self.class, args.to_vec()),
                    None => Type::Named(self.class),
                })
            } else {
                None
            }
        })
    }
}

fn match_bound_parameter(
    param: &Type,
    actual: &Type,
    ignore_callable_kind: bool,
    names: &super::typed_ast::HirTypeParameters,
    args: &[Type],
    match_self: impl Fn(&TypeId, &Type) -> Option<bool>,
) -> bool {
    let bound = |id: &TypeId, ty: &Type| match names.position(id) {
        Some(i) if ignore_callable_kind => Some(args[i].same_modulo_callable_kind(ty)),
        Some(i) => Some(args[i] == *ty),
        None => match_self(id, ty),
    };
    if ignore_callable_kind {
        param.callable_storage_with_bindings(actual, bound)
    } else {
        param.equals_with_bindings(actual, bound)
    }
}

/// A conversion may use an interface parameter whose concrete shape depends
/// on the receiver. Both parts borrow immutable declaration or HIR data.
#[derive(Clone, Copy)]
struct ProjectedParameter<'e> {
    param: &'e Type,
    bindings: InterfaceBindings<'e>,
}

impl ProjectedParameter<'_> {
    // Reference arguments require an exact match. The caller has already
    // handled them; here only a value can use callable storage or an upcast.
    fn accepts_value(&self, source: &Type, resolution: &super::typed_ast::HirResolution) -> bool {
        resolution.can_coerce_matching(source, |ty, relax| {
            self.bindings.compare(self.param, ty, relax)
        })
    }
}

#[derive(Clone, Copy)]
enum ParameterType<'e> {
    Declared(&'e Type),
    Projected(ProjectedParameter<'e>),
}

impl<'e> ResolvedSignature<'e, 'e> {
    fn check_conversions(
        &self,
        args: &[HirExpr],
        exits_before_conversion: impl Fn(&HirExpr) -> bool,
        mut check: impl FnMut(usize, ParameterType<'e>, &HirExpr) -> bool,
    ) -> bool {
        let params = &self.declaration.params;
        if args.len() != params.len() {
            return false;
        }
        let mut bindings = None;
        for (index, (param, arg)) in params.iter().zip(args).enumerate() {
            if exits_before_conversion(arg) {
                continue;
            }
            let target = if let Some(interface) = &self.interface {
                let bindings = *bindings.get_or_insert_with(|| interface.bindings());
                if bindings.matches(param, &arg.ty) {
                    continue;
                }
                ParameterType::Projected(ProjectedParameter { param, bindings })
            } else {
                if param == &arg.ty {
                    continue;
                }
                ParameterType::Declared(param)
            };
            if !check(index, target, arg) {
                return false;
            }
        }
        true
    }
}

impl<'a> ResolvedSignature<'a, '_> {
    fn return_type(&self) -> Cow<'a, Type> {
        let result = &self.declaration.return_type;
        match &self.interface {
            Some(interface) => Cow::Owned(interface.bindings().specialize(result)),
            None => Cow::Borrowed(result),
        }
    }
}

fn static_signature<'a>(
    resolution: &'a LoweringDeclarations<'a>,
    class: &TypeId,
    method: &str,
) -> Option<ResolvedSignature<'a, 'static>> {
    resolution
        .modules
        .get(class)
        .and_then(|functions| functions.get(method))
        .map(|declaration| ResolvedSignature {
            declaration,
            interface: None,
        })
        .or_else(|| lookup_method_signature(resolution, *class, None, method))
}

#[cfg(test)]
std::thread_local! {
    static INTERFACE_SPECIALIZATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static CLASS_FIELD_ENUMERATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static DECLARED_METHOD_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn method_is_async(resolution: &LoweringDeclarations<'_>, receiver: &Type, method: &str) -> bool {
    let (Type::Named(class) | Type::Generic(class, _)) = receiver else {
        return false;
    };
    resolution
        .declared_method(*class, method)
        .is_some_and(|decl| decl.signature.is_async)
}

fn method_signature<'a, 'r>(
    resolution: &LoweringDeclarations<'a>,
    receiver: &'r Type,
    method: &str,
) -> Option<ResolvedSignature<'a, 'r>> {
    let (class, args) = match receiver {
        Type::Named(class) => (*class, None),
        Type::Generic(class, args) => (*class, Some(args.as_slice())),
        _ => return None,
    };
    lookup_method_signature(resolution, class, args, method)
}

fn lookup_method_signature<'a, 'r>(
    resolution: &LoweringDeclarations<'a>,
    receiver_class: TypeId,
    args: Option<&'r [Type]>,
    method: &str,
) -> Option<ResolvedSignature<'a, 'r>> {
    #[cfg(test)]
    DECLARED_METHOD_LOOKUPS.set(DECLARED_METHOD_LOOKUPS.get() + 1);
    let method = resolution.declared_method(receiver_class, method)?;
    let interface = match method.interface_params {
        Some(type_params) => {
            if type_params.as_slice().len() != args.unwrap_or_default().len() {
                return None;
            }
            Some(InterfaceReceiver {
                class: receiver_class,
                type_params,
                args,
            })
        }
        None => None,
    };
    Some(ResolvedSignature {
        declaration: method.signature,
        interface,
    })
}

/// A missing arm yields the condition's boolean value on that edge (&& or ||).
struct ConditionalOperands<'a> {
    condition: &'a HirExpr,
    then_expr: Option<&'a HirExpr>,
    else_expr: Option<&'a HirExpr>,
}

impl<'a> ConditionalOperands<'a> {
    fn from_kind(kind: &'a HirExprKind) -> Option<Self> {
        use crate::parser::ast::BinOp;
        let (condition, then_expr, else_expr) = match kind {
            HirExprKind::Ternary {
                condition,
                then_expr,
                else_expr,
            } => (
                condition.as_ref(),
                Some(then_expr.as_ref()),
                Some(else_expr.as_ref()),
            ),
            HirExprKind::Binary {
                op: BinOp::And,
                lhs,
                rhs,
            } => (lhs.as_ref(), Some(rhs.as_ref()), None),
            HirExprKind::Binary {
                op: BinOp::Or,
                lhs,
                rhs,
            } => (lhs.as_ref(), None, Some(rhs.as_ref())),
            _ => return None,
        };
        Some(Self {
            condition,
            then_expr,
            else_expr,
        })
    }

    fn literal_condition(&self) -> Option<bool> {
        match self.condition.kind {
            HirExprKind::Bool(value) => Some(value),
            _ => None,
        }
    }

    fn operand(&self, truth: bool) -> Option<&'a HirExpr> {
        if truth {
            self.then_expr
        } else {
            self.else_expr
        }
    }
}

struct ConstantIfBranches<'a> {
    taken: Option<&'a [HirStmt]>,
    skipped: &'a [HirStmt],
}

/// Both CFG construction and lexical capture analysis elide the same statement arm.
fn constant_if_branches<'a>(
    cond: &HirExpr,
    then_branch: &'a [HirStmt],
    else_branch: Option<&'a [HirStmt]>,
) -> Option<ConstantIfBranches<'a>> {
    match cond.kind {
        HirExprKind::Bool(true) => Some(ConstantIfBranches {
            taken: Some(then_branch),
            skipped: else_branch.unwrap_or_default(),
        }),
        HirExprKind::Bool(false) => Some(ConstantIfBranches {
            taken: else_branch,
            skipped: then_branch,
        }),
        _ => None,
    }
}

fn while_body_is_reachable(cond: &HirExpr) -> bool {
    !matches!(cond.kind, HirExprKind::Bool(false))
}

/// Whether running `body` can END a panic rather than just clean up after one:
/// it calls `recover()` somewhere outside a lambda.
///
/// The lambda exclusion is the same one the backend applies — a `recover()`
/// written inside a lambda body runs when that lambda is CALLED, which is not
/// this defer's unwinding.
pub(crate) fn defer_body_contains_recover(body: &HirDeferBody) -> bool {
    match body {
        HirDeferBody::Expr(expr) => expr_has_recover(expr),
        HirDeferBody::Block(stmts) => block_has_recover(stmts),
    }
}

fn expr_has_recover(expr: &HirExpr) -> bool {
    expr.walk_postorder(false)
        .any(|expr| matches!(&expr.kind, HirExprKind::Call { callee, binding: HirCallBinding::Function, .. } if callee.is_free_named("recover")))
}

fn block_has_recover(stmts: &[HirStmt]) -> bool {
    stmts
        .iter()
        .flat_map(HirStmt::child_exprs)
        .any(expr_has_recover)
}

#[cfg(test)]
std::thread_local! {
    static CAPTURE_DISCOVERY_STORAGE: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
    static CAPTURE_DISCOVERY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn cleanup_captures(
    inputs: &[LirLocalId],
    blocks: &[SourceBlock],
    locals: &[LirLocal],
) -> Vec<LirLocalId> {
    if inputs.is_empty() {
        return Vec::new();
    }
    // Region-local IDs in the input prefix map directly to the caller's ordered IDs.
    let names = locals[..inputs.len()]
        .iter()
        .map(|local| {
            #[cfg(test)]
            CAPTURE_DISCOVERY_VISITS.set(CAPTURE_DISCOVERY_VISITS.get() + 1);
            (local.name.as_str(), local.id)
        })
        .collect::<HashMap<_, _>>();
    let mut seen = vec![0; inputs.len().div_ceil(64)];
    #[cfg(test)]
    CAPTURE_DISCOVERY_STORAGE.set((names.len(), seen.len()));

    let mut mark = |id: LirLocalId| {
        if (id.0 as usize) < inputs.len() && !liveness::contains(&seen, id) {
            liveness::insert(&mut seen, id);
            true
        } else {
            false
        }
    };
    let mut remaining = inputs.len();
    for block in blocks {
        for inst in &block.instrs {
            #[cfg(test)]
            CAPTURE_DISCOVERY_VISITS.set(CAPTURE_DISCOVERY_VISITS.get() + 1);
            async_liveness::visit_instruction_locals(inst, &names, |_, id| {
                remaining -= usize::from(mark(id));
            });
            if remaining == 0 {
                return inputs.to_vec();
            }
        }
        #[cfg(test)]
        CAPTURE_DISCOVERY_VISITS.set(CAPTURE_DISCOVERY_VISITS.get() + 1);
        async_liveness::visit_terminator_uses(&block.terminator, &names, |id| {
            remaining -= usize::from(mark(id));
        });
        if remaining == 0 {
            return inputs.to_vec();
        }
    }
    liveness::ones(&seen)
        .map(|id| inputs[id.0 as usize])
        .collect()
}

/// Lower one function's statement tree into a block graph.
fn lower_function<'r>(
    f: &HirFunction,
    class: Option<&TypeId>,
    resolution: &'r LoweringDeclarations<'r>,
) -> SourceFunction {
    let mut b = Builder::new(&f.params, f.is_async, resolution);
    b.return_type = f.return_type.clone();
    for clause in &f.requires {
        b.lower_stmt(&contract_check(clause.clone(), false));
    }
    b.lower_scope(&f.body);
    if !f.ensures.is_empty() {
        b.contract_epilogue(f);
    }
    b.materialize_preemption_safepoints();
    // The fall-through end of a function is an implicit `return;` (the type
    // checker has already guaranteed value-returning paths return).
    let (blocks, locals) = b.finish();
    let name = match class {
        Some(class) => FunctionId::method(*class, f.name.name()),
        None => f.name,
    };
    let function = SourceFunction {
        name,
        is_async: f.is_async,
        params: f.params.clone(),
        return_type: f.return_type.clone(),
        // Planned once over the final graph after optimization and root
        // clearing; every earlier layout would be discarded unread.
        async_frame: LirAsyncFrameLayout::default(),
        blocks,
        locals,
        captures: Vec::new(),
    };
    function.assert_unique_local_names();
    function
}

/// One enclosing loop, for the early exits that jump out of it: where `break`
/// and `continue` go, and how much nesting they leave on the way. The two
/// depths are what an early exit needs and a fallthrough does not: how many
/// defer scopes to flush and how many lexical scopes' GC
/// roots to drop — the ones opened inside the loop body, not
/// the ones that were already open when the loop started.
#[derive(Debug, Clone, Copy)]
struct LirLoopFrame {
    exit: BlockId,
    next: BlockId,
    defer_depth: usize,
    scope_depth: usize,
}

/// One open lexical scope, for the GC-root close at its end.
#[derive(Debug, Clone)]
struct LirScopeMark {
    /// Where this scope's locals begin in [`Builder::locals`]. Locals are handed
    /// out from one growing table, so its own bindings — and those of every
    /// scope nested in it — are exactly the entries from here to the end.
    first_local: usize,
    /// Start in the active root stack. Popping a child truncates that stack,
    /// so ordinary exits never revisit locals belonging to closed children.
    first_root: usize,
}

impl LirScopeMark {
    fn opening_at(first_local: usize, first_root: usize) -> Self {
        Self {
            first_local,
            first_root,
        }
    }
}

/// Classification flags for a local declaration.
struct LocalOptions {
    synthetic: bool,
    parameter: bool,
}

/// Block-graph builder: appends instructions to a current block and seals
/// blocks with terminators as control flow branches and rejoins.
struct Builder<'r> {
    return_type: Type,
    resolution: &'r LoweringDeclarations<'r>,
    blocks: Vec<(Vec<SourceInst>, Option<SourceTerminator>)>,
    /// Per-block [`SourceBlock::recovery`], filled in by `lower_scope` once it
    /// knows the scope's resume block.
    block_recovery: Vec<Vec<BlockId>>,
    current: usize,
    /// Counter for synthesized `for` induction variables, unique per function
    /// so nested loops do not collide.
    for_counter: usize,
    /// Innermost-first loop context for break/continue lowering.
    ///
    loop_stack: Vec<LirLoopFrame>,
    /// How many defer scopes are currently open. `return` flushes all of them.
    defer_depth: usize,
    defer_counter: u32,
    defer_scopes: Vec<std::collections::HashMap<HirDeferId, LirDeferId>>,
    is_async: bool,
    suspend_counter: usize,
    locals: Vec<LirLocal>,
    /// Free names of descendant sites in the lexical cleanup currently being lowered.
    nested_defer_access: defer_captures::NestedAccess,
    cleanup_name: Option<FunctionId>,
    /// The currently open lexical scopes, outermost first.
    scope_starts: Vec<LirScopeMark>,
    scope_roots: Vec<LirLocalId>,
    /// Flat rather than a scope stack, because HIR names are already unique
    /// within a function: `LowerCtx::bind` alpha-renames every shadowing
    /// binding to `name$n` before lowering runs. That invariant is what lets a
    /// `HirExprKind::Var` node resolve to exactly one [`LirLocalId`] here and
    /// in `async_liveness`; without it two frame slots would silently alias.
    /// [`SourceFunction::assert_unique_local_names`] pins it.
    local_by_name: std::collections::HashMap<String, LirLocalId>,
    /// Open critical sections, outermost first. Early exits release every
    /// section they leave after running that section's own deferred cleanup.
    active_locks: Vec<ActiveLock>,
}

/// One critical section in [`Builder::active_locks`].
#[derive(Clone)]
struct ActiveLock {
    slots: LirLockSlots,
    /// The defer depth the section's own scope occupies. An exit unwinding to a
    /// depth at or below this one leaves the section; one unwinding to a deeper
    /// scope — a `break` out of a loop written INSIDE the section — does not.
    defer_depth: usize,
}

fn expr_suspends_here(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Await { inner }
            if builtin_types::unary_arg(&inner.ty, B::Future).is_some()
                && !matches!(&inner.kind, HirExprKind::Call { callee, binding: HirCallBinding::Function, .. } if matches!(callee.name().as_ref(), "sleep" | "yield")) =>
        {
            false
        }
        HirExprKind::Await { .. } | HirExprKind::Select { .. } => true,
        HirExprKind::MethodCall { builtin, .. } => {
            builtin.is_some_and(BuiltinMethod::is_suspension_point)
        }
        ir_variants!(
            HirExprKind;
            Int, Float, Bool, Str, Var, FnRef, Binary, Unary, Call, Invoke, Print, Array, Index,
            Ternary, New, FieldAccess, ObjectLiteral, StaticField, StaticCall, ReferenceArg,
            Range, TryPropagate, Lambda, Match
        ) => false,
    }
}

fn collect_suspensions<'a>(expr: &'a HirExpr, out: &mut Vec<&'a HirExpr>) {
    out.extend(
        expr.walk_postorder(false)
            .filter(|expr| expr_suspends_here(expr)),
    );
}

/// Does a suspension live anywhere inside this expression?
fn suspends_anywhere(expr: &HirExpr) -> bool {
    expr.walk_postorder(false).any(expr_suspends_here)
}

/// The names a pattern binds, with the type each is bound at, in the order the
/// emitter destructures them.
///
/// A variant whose payloads are ALL `void` carries no word, so its bindings
/// name values that do not exist. They are deliberately absent here: nothing
/// declares a local for them, so an arm body that reads one finds no binding
/// and is rejected by LIR validation, matching tree-shaped `match` validation.
fn pattern_bindings(pattern: &HirPattern) -> Vec<(String, Type)> {
    match pattern {
        HirPattern::Wildcard
        | HirPattern::LiteralBool(_)
        | HirPattern::LiteralInt(_)
        | HirPattern::EnumVariant { .. } => Vec::new(),
        HirPattern::Binding { name, ty } => vec![(name.clone(), ty.clone())],
        HirPattern::EnumVariantTuple { bindings, .. } => {
            if bindings.iter().all(|(_, ty)| matches!(ty, Type::Void)) {
                return Vec::new();
            }
            bindings.clone()
        }
        HirPattern::ClassDowncast {
            binding,
            binding_ty,
            ..
        } => vec![(binding.clone(), binding_ty.clone())],
    }
}

fn rematerializable(expr: &HirExpr) -> bool {
    matches!(
        expr.kind,
        HirExprKind::Int(_)
            | HirExprKind::Float(_)
            | HirExprKind::Bool(_)
            | HirExprKind::Str(_)
            | HirExprKind::Var(_)
    )
}

#[cfg(test)]
fn contains_suspend_span(node: &HirExpr, target_span: Span) -> bool {
    node.walk_postorder(true)
        .any(|child| child.span == target_span && expr_suspends_here(child))
}

fn hoistable_around(node: &HirExpr, target: &HirExpr, seen: &mut bool) -> bool {
    let Some(path) = node.path_to(target) else {
        return *seen || rematerializable(node);
    };
    let ancestors: std::collections::HashSet<*const HirExpr> =
        path.into_iter().map(std::ptr::from_ref).collect();
    let mut pending = vec![node];
    while let Some(node) = pending.pop() {
        if std::ptr::eq(node, target) {
            *seen = true;
            continue;
        }
        if !ancestors.contains(&std::ptr::from_ref(node)) {
            if !*seen && !rematerializable(node) {
                return false;
            }
            continue;
        }
        // Only unconditional operands can be pulled ahead of their parent.
        let conditional = match &node.kind {
            HirExprKind::Ternary { condition, .. } => {
                !ancestors.contains(&std::ptr::from_ref(&**condition))
            }
            HirExprKind::Match { scrutinee, .. } => {
                !ancestors.contains(&std::ptr::from_ref(&**scrutinee))
            }
            HirExprKind::Binary {
                op: crate::parser::ast::BinOp::And | crate::parser::ast::BinOp::Or,
                lhs,
                ..
            } => !ancestors.contains(&std::ptr::from_ref(&**lhs)),
            HirExprKind::Select { .. } | HirExprKind::Lambda { .. } => true,
            ir_variants!(
                HirExprKind;
                Int, Float, Bool, Str, Var, FnRef, Binary, Unary, Call, Invoke, Print, Array,
                Index, New, FieldAccess, MethodCall, ObjectLiteral, StaticField, StaticCall,
                ReferenceArg, Range, Await, TryPropagate
            ) => false,
        };
        if conditional {
            return false;
        }
        pending.extend(node.children().into_iter().rev());
    }
    true
}

fn replace_suspension(expr: &HirExpr, target_span: Span, replacement: &HirExpr) -> HirExpr {
    let mut out = expr.clone();
    out.visit_mut_preorder(false, |node| {
        if node.span == target_span && expr_suspends_here(node) {
            *node = replacement.clone();
            return false;
        }
        !matches!(
            node.kind,
            HirExprKind::Lambda { .. } | HirExprKind::Select { .. }
        )
    });
    out
}

fn expression_needs_cfg(expr: &HirExpr) -> bool {
    expr.walk_postorder(false).any(|node| {
        matches!(
            node.kind,
            HirExprKind::TryPropagate { .. }
                | HirExprKind::Ternary { .. }
                | HirExprKind::Match { .. }
                | HirExprKind::Binary {
                    op: crate::parser::ast::BinOp::And | crate::parser::ast::BinOp::Or,
                    ..
                }
        )
    })
}

fn expression_executes_call(expr: &HirExpr) -> bool {
    expr.walk_postorder(false).any(|expr| {
        // Array reads/length are bounded runtime operations, not scheduling
        // boundaries. Still visit their operands: f()[g()] must retain the
        // call boundary. Loop backedges already provide the required poll.
        if let HirExprKind::Index { array, .. } = &expr.kind
            && (matches!(array.ty, Type::Array(_))
                || builtin_types::unary_arg(&array.ty, B::FrozenArray).is_some())
        {
            return false;
        }
        if let HirExprKind::MethodCall {
            builtin: Some(BuiltinMethod::Intrinsic(Intrinsic::ArrayLen | Intrinsic::FrozenArrayLen)),
            ..
        } = &expr.kind
        {
            return false;
        }
        matches!(
            expr.kind,
            HirExprKind::Call { .. }
                | HirExprKind::MethodCall { .. }
                | HirExprKind::StaticCall { .. }
                | HirExprKind::New { .. }
                | HirExprKind::ObjectLiteral { .. }
                | HirExprKind::Print { .. }
                | HirExprKind::Array { .. }
                | HirExprKind::Index { .. }
                | HirExprKind::Select { .. }
        )
    })
}

fn instruction_executes_call(inst: &SourceInst) -> bool {
    match inst {
        SourceInst::Let { value, .. }
        | SourceInst::Assign { value, .. }
        | SourceInst::StaticFieldAssign { value, .. }
        | SourceInst::Expr(value) => expression_executes_call(value),
        SourceInst::FieldAssign { object, value, .. } => {
            expression_executes_call(object) || expression_executes_call(value)
        }
        SourceInst::IndexAssign {
            array,
            index,
            value,
        } => {
            expression_executes_call(array)
                || expression_executes_call(index)
                || expression_executes_call(value)
        }
        SourceInst::SuperInit { .. } => true,
        SourceInst::Defer { .. } => false,
        // The release calls the runtime, but it is compiler-owned bookkeeping
        // rather than a user statement, and a preemption edge in front of it
        // would park the task holding a lock it is one instruction from giving
        // back. Cleanup unwinding also emits release without a safepoint.
        // A pattern test and its bindings are loads and integer compares on a
        // value the enclosing block already holds. No call, so no safepoint.
        SourceInst::Compute { .. }
        | SourceInst::ReleaseLock { .. }
        | SourceInst::EnterDeferScope { .. }
        | SourceInst::LeaveDeferScope { .. }
        | SourceInst::FlushDefers { .. }
        | SourceInst::ClearScopeRoots { .. }
        | SourceInst::MatchTest { .. }
        | SourceInst::MatchBind { .. }
        | SourceInst::SelectInit { .. }
        | SourceInst::SelectProbe { .. }
        | SourceInst::SelectPick { .. }
        | SourceInst::SelectUnregister { .. }
        | SourceInst::SelectCommit { .. } => false,
    }
}

#[willow_continuations::methods(
    lower_assign_operands,
    lower_condition,
    lower_conditional_suspend,
    lower_for,
    lower_lock,
    lower_defer_region,
    lower_match_arm_body,
    lower_match_arms_cfg,
    lower_nested_suspend,
    lower_nested_cfg,
    lower_operand_before_suspend,
    lower_root_suspend,
    lower_scope,
    lower_scope_inner,
    lower_select,
    lower_super_init_values,
    lower_stmt,
    lower_stmts,
    lower_suspending_operand,
    lower_value_into
)]
impl<'r> Builder<'r> {
    fn new(params: &[HirParam], is_async: bool, resolution: &'r LoweringDeclarations<'r>) -> Self {
        let mut builder = Self {
            return_type: Type::Void,
            resolution,
            blocks: vec![(Vec::new(), None)],
            block_recovery: vec![Vec::new()],
            current: 0,
            for_counter: 0,
            loop_stack: Vec::new(),
            defer_depth: 0,
            active_locks: Vec::new(),
            defer_counter: 0,
            defer_scopes: Vec::new(),
            is_async,
            suspend_counter: 0,
            locals: Vec::new(),
            nested_defer_access: None,
            cleanup_name: None,
            scope_starts: Vec::new(),
            scope_roots: Vec::new(),
            local_by_name: std::collections::HashMap::new(),
        };
        for param in params {
            builder.declare_local(
                param.name.clone(),
                param.ty.clone(),
                Some(param.span),
                LocalOptions {
                    synthetic: false,
                    parameter: true,
                },
            );
            if is_async && param.by_reference {
                builder.locals.last_mut().unwrap().storage_kind = LirStorageKind::ReferenceCell;
            }
        }
        builder
    }

    /// A lifted lambda body, whose entry binds the captured environment before
    /// the declared parameters.
    ///
    /// Captures come first because that is the order lowering bound them in,
    /// and they are `parameter` locals for the same reason a parameter is: the
    /// backend gives them their value at entry, so nothing in the body may
    /// clear or re-slot them on a scope exit.
    fn new_lambda(
        params: &[HirParam],
        captures: &[HirCapture],
        resolution: &'r LoweringDeclarations<'r>,
    ) -> Self {
        let mut builder = Self::new(&[], false, resolution);
        for capture in captures {
            builder.declare_local(
                capture.name.clone(),
                capture.ty.clone(),
                None,
                LocalOptions {
                    synthetic: false,
                    parameter: true,
                },
            );
        }
        for param in params {
            builder.declare_local(
                param.name.clone(),
                param.ty.clone(),
                Some(param.span),
                LocalOptions {
                    synthetic: false,
                    parameter: true,
                },
            );
        }
        builder
    }

    fn lower_defer_region(
        &mut self,
        stmts: &[HirStmt],
        recovery_capable: bool,
        inputs: &[LirLocalId],
        nested_access: defer_captures::NestedAccess,
    ) -> SourceDeferBody {
        let mut region = Self::new(&[], false, self.resolution);
        let name = self.cleanup_name.unwrap_or_else(|| {
            if let Some(receiver) = self.local_by_name.get("self")
                && let Type::Named(owner) = self.locals[receiver.0 as usize].ty
            {
                FunctionId::method(owner, "$defer")
            } else {
                FunctionId::free("$defer")
            }
        });
        region.cleanup_name = Some(name);
        region.nested_defer_access = nested_access;
        region.locals = inputs
            .iter()
            .map(|id| self.locals[id.0 as usize].clone())
            .collect();
        region.local_by_name = region
            .locals
            .iter_mut()
            .enumerate()
            .map(|(index, local)| {
                local.id = LirLocalId(index as u32);
                local.parameter = true;
                (local.name.clone(), local.id)
            })
            .collect();
        region.suspend_counter = self.suspend_counter;
        region.for_counter = self.for_counter;
        region.lower_scope(stmts);
        for (_, terminator) in &mut region.blocks {
            if terminator.is_none() {
                *terminator = Some(SourceTerminator::CleanupReturn);
            }
        }
        let (blocks, locals) = region.finish();
        let captures = cleanup_captures(inputs, &blocks, &locals);
        SourceDeferBody {
            function: Box::new(SourceFunction {
                name,
                is_async: false,
                params: Vec::new(),
                return_type: Type::Void,
                blocks,
                locals,
                async_frame: LirAsyncFrameLayout::default(),
                captures: Vec::new(),
            }),
            captures,
            recovery_capable,
        }
    }

    fn declare_local(
        &mut self,
        name: String,
        ty: Type,
        source_span: Option<Span>,
        options: LocalOptions,
    ) -> LirLocalId {
        let LocalOptions {
            synthetic,
            parameter,
        } = options;
        let id = LirLocalId(self.locals.len() as u32);
        self.locals.push(LirLocal {
            storage_kind: LirStorageKind::Value,
            id,
            name: name.clone(),
            ty,
            source_span,
            synthetic,
            parameter,
        });
        if !synthetic && !parameter && !self.scope_starts.is_empty() {
            self.scope_roots.push(id);
        }
        self.local_by_name.insert(name, id);
        id
    }

    fn push_let(
        &mut self,
        name: String,
        mutable: bool,
        ty: Type,
        value: HirExpr,
        source_span: Option<Span>,
        synthetic: bool,
    ) -> LirLocalId {
        let local = self.declare_local(
            name.clone(),
            ty.clone(),
            source_span,
            LocalOptions {
                synthetic,
                parameter: false,
            },
        );
        self.push_existing_let(local, name, mutable, ty, value, source_span);
        local
    }

    fn push_existing_let(
        &mut self,
        local: LirLocalId,
        name: String,
        mutable: bool,
        ty: Type,
        value: HirExpr,
        source_span: Option<Span>,
    ) {
        self.push(SourceInst::Let {
            local,
            name,
            mutable,
            span: source_span.unwrap_or(value.span),
            ty,
            value,
        });
    }

    fn push_synth_let(&mut self, name: &str, mutable: bool, value: HirExpr) -> LirLocalId {
        if expression_needs_cfg(&value) || (self.is_async && suspends_anywhere(&value)) {
            let local = self.declare_local(
                name.to_string(),
                value.ty.clone(),
                None,
                LocalOptions {
                    synthetic: true,
                    parameter: false,
                },
            );
            self.lower_value_into(local, &value);
            return local;
        }
        self.push_let(
            name.to_string(),
            mutable,
            value.ty.clone(),
            value,
            None,
            true,
        )
    }

    // A terminal '$' is private to CFG storage. Source shadow suffixes end in a
    // number. Final LIR chooses the printable spelling once all locals exist.
    fn synthetic_name(&mut self, role: &str) -> String {
        let n = self.suspend_counter;
        self.suspend_counter += 1;
        format!("__async_{role}_{n}$")
    }

    fn local_expr(&self, local: LirLocalId, span: Span) -> HirExpr {
        let local = &self.locals[local.0 as usize];
        HirExpr {
            kind: HirExprKind::Var(local.name.clone()),
            ty: local.ty.clone(),
            span,
        }
    }

    /// Freeze the operands whose defer semantics are registration-time. Block
    /// and match bodies deliberately remain trees and read lexical locals when
    /// cleanup actually runs.
    fn capture_defer_expr(
        &mut self,
        id: LirDeferId,
        expr: &HirExpr,
    ) -> (HirExpr, Option<Vec<LirLocalId>>) {
        let capture =
            |this: &mut Self, role: &str, value: HirExpr, inputs: &mut Vec<LirLocalId>| {
                let mut name = format!("__defer{}_{}$", id.0, role);
                // Each cleanup region restarts its site IDs but inherits its
                // parent's locals. Distinguish a new capture of the same role.
                if this.local_by_name.contains_key(&name) {
                    use std::fmt::Write;
                    write!(name, "{}$", this.locals.len()).expect("writing to a String");
                }
                let span = value.span;
                let local = this.push_synth_let(&name, false, value);
                inputs.push(local);
                this.local_expr(local, span)
            };
        let capture_args = |this: &mut Self, args: &[HirExpr], inputs: &mut Vec<LirLocalId>| {
            args.iter()
                .enumerate()
                .map(|(index, arg)| capture(this, &format!("arg{index}"), arg.clone(), inputs))
                .collect()
        };
        let invoke =
            |this: &mut Self, name, callee, args: &[HirExpr], inputs: &mut Vec<LirLocalId>| {
                // An argument may replace the callable. Read it before the first
                // argument and keep that value in the cleanup region's captures.
                let callee = Box::new(capture(this, "callee", callee, inputs));
                HirExprKind::Invoke {
                    name,
                    callee,
                    args: capture_args(this, args, inputs),
                }
            };
        let mut inputs = Vec::new();
        let kind = match &expr.kind {
            HirExprKind::Invoke { name, callee, args } => {
                invoke(self, *name, (**callee).clone(), args, &mut inputs)
            }
            HirExprKind::Call {
                callee,
                binding: HirCallBinding::Local { may_change: true },
                args,
            } => {
                let slot = self.local_by_name[callee.name().as_ref()];
                invoke(
                    self,
                    *callee,
                    self.local_expr(slot, expr.span),
                    args,
                    &mut inputs,
                )
            }
            HirExprKind::Call {
                callee,
                binding,
                args,
            } => {
                if binding.is_local() {
                    inputs.push(self.local_by_name[callee.name().as_ref()]);
                }
                HirExprKind::Call {
                    callee: *callee,
                    binding: *binding,
                    args: capture_args(self, args, &mut inputs),
                }
            }
            HirExprKind::MethodCall {
                object,
                method,
                args,
                super_class,
                builtin,
            } => HirExprKind::MethodCall {
                builtin: *builtin,
                object: Box::new(capture(self, "self", (**object).clone(), &mut inputs)),
                method: method.clone(),
                args: capture_args(self, args, &mut inputs),
                super_class: *super_class,
            },
            HirExprKind::StaticCall {
                class,
                method,
                args,
            } => HirExprKind::StaticCall {
                class: *class,
                method: method.clone(),
                args: capture_args(self, args, &mut inputs),
            },
            HirExprKind::Print { value, newline } => HirExprKind::Print {
                value: Box::new(capture(self, "print", (**value).clone(), &mut inputs)),
                newline: *newline,
            },
            ir_variants!(
                HirExprKind;
                Int, Float, Bool, Str, Var, FnRef, Binary, Unary, Array, Index, Ternary, New,
                FieldAccess, ObjectLiteral, StaticField, ReferenceArg, Range, Await,
                TryPropagate, Lambda, Match, Select
            ) => return (expr.clone(), None),
        };
        (
            HirExpr {
                kind,
                ty: expr.ty.clone(),
                span: expr.span,
            },
            Some(inputs),
        )
    }

    fn new_block(&mut self) -> BlockId {
        self.blocks.push((Vec::new(), None));
        self.block_recovery.push(Vec::new());
        BlockId(self.blocks.len() - 1)
    }

    fn switch_to(&mut self, block: BlockId) {
        self.current = block.0;
    }

    fn push(&mut self, inst: SourceInst) {
        self.blocks[self.current].0.push(inst);
    }

    /// Seal the current block. A block already sealed by an inner `return`
    /// keeps its first terminator (trailing unreachable code was appended to a
    /// fresh block by `terminate`).
    fn terminate(&mut self, terminator: SourceTerminator) {
        let slot = &mut self.blocks[self.current].1;
        if slot.is_none() {
            *slot = Some(terminator);
        }
    }

    /// Make conditional scheduler preemption part of the CFG before liveness
    /// runs. This mirrors the established placement (before call-bearing
    /// instructions/terminators); the backend no longer invents these edges.
    fn materialize_preemption_safepoints(&mut self) {
        if !self.is_async {
            return;
        }
        let original_blocks = self.blocks.len();
        for index in 0..original_blocks {
            let (instrs, terminator) = std::mem::take(&mut self.blocks[index]);
            // Splitting a block does not move it out of its `defer` scopes, so
            // every piece keeps the panic edges the whole block had.
            let recovery = self.block_recovery[index].clone();
            let mut current = index;
            for inst in instrs {
                if instruction_executes_call(&inst) {
                    let resume = self.new_block();
                    self.block_recovery[resume.0] = recovery.clone();
                    self.blocks[current].1 = Some(SourceTerminator::Suspend {
                        operation: SuspendOp::Preempt,
                        resume,
                    });
                    current = resume.0;
                }
                self.blocks[current].0.push(inst);
            }
            let terminator = terminator.unwrap_or(SourceTerminator::Return(None));
            let terminator_calls = match &terminator {
                SourceTerminator::Branch { cond, .. } => expression_executes_call(cond),
                SourceTerminator::Return(Some(value)) => expression_executes_call(value),
                SourceTerminator::Jump(_)
                | SourceTerminator::Suspend { .. }
                | SourceTerminator::Return(_)
                | SourceTerminator::CleanupReturn => false,
            };
            if terminator_calls {
                let resume = self.new_block();
                self.block_recovery[resume.0] = recovery.clone();
                self.blocks[current].1 = Some(SourceTerminator::Suspend {
                    operation: SuspendOp::Preempt,
                    resume,
                });
                current = resume.0;
            }
            self.blocks[current].1 = Some(terminator);
        }
    }

    fn finish(self) -> (Vec<SourceBlock>, Vec<LirLocal>) {
        let mut locals = self.locals;
        let mut recovery = self.block_recovery;
        let mut blocks: Vec<SourceBlock> = self
            .blocks
            .into_iter()
            .enumerate()
            .map(|(i, (instrs, terminator))| SourceBlock {
                id: BlockId(i),
                instrs,
                terminator: terminator.unwrap_or(SourceTerminator::Return(None)),
                recovery: std::mem::take(&mut recovery[i]),
            })
            .collect();
        super::optimize::fold_blocks(&mut blocks);
        value::lower_blocks(&mut blocks, &mut locals, self.is_async);
        super::optimize::simplify_cfg(&mut blocks);
        let mut blocks = prune_unreachable(blocks);
        super::optimize::eliminate_dead_values(&mut blocks, &locals);
        (blocks, locals)
    }
}

/// Drop blocks unreachable from the entry (dead blocks created after
/// mid-block `return`s) and renumber the survivors densely.
fn prune_unreachable(blocks: Vec<SourceBlock>) -> Vec<SourceBlock> {
    let mut reachable = vec![false; blocks.len()];
    let mut stack = vec![0usize];
    while let Some(i) = stack.pop() {
        if std::mem::replace(&mut reachable[i], true) {
            continue;
        }
        for inst in &blocks[i].instrs {
            if let SourceInst::EnterDeferScope {
                resume: Some(resume),
                ..
            } = inst
            {
                // A recovered panic reaches this continuation even when the
                // normal source path returned before the lexical scope ended.
                stack.push(resume.0);
            }
        }
        match &blocks[i].terminator {
            SourceTerminator::Jump(b) => stack.push(b.0),
            SourceTerminator::Branch {
                then_block,
                else_block,
                ..
            } => {
                stack.push(then_block.0);
                stack.push(else_block.0);
            }
            SourceTerminator::Suspend { resume, .. } => stack.push(resume.0),
            SourceTerminator::Return(_) | SourceTerminator::CleanupReturn => {}
        }
    }

    // Old index → new dense index.
    let mut remap = vec![usize::MAX; blocks.len()];
    let mut next = 0usize;
    for (i, live) in reachable.iter().enumerate() {
        if *live {
            remap[i] = next;
            next += 1;
        }
    }

    blocks
        .into_iter()
        .enumerate()
        .filter(|(i, _)| reachable[*i])
        .map(|(i, mut block)| {
            block.id = BlockId(remap[i]);
            // A resume block is reachable whenever its scope is (see the
            // `EnterDeferScope` walk above), so a surviving edge always has a
            // surviving target; an edge recorded in a block the walk never
            // reached disappears with the block itself.
            block
                .recovery
                .retain(|target| reachable.get(target.0).copied().unwrap_or(false));
            for target in &mut block.recovery {
                *target = BlockId(remap[target.0]);
            }
            for inst in &mut block.instrs {
                if let SourceInst::EnterDeferScope {
                    resume: Some(resume),
                    ..
                } = inst
                {
                    *resume = BlockId(remap[resume.0]);
                }
            }
            block.terminator = match block.terminator {
                SourceTerminator::Jump(b) => SourceTerminator::Jump(BlockId(remap[b.0])),
                SourceTerminator::Branch {
                    cond,
                    then_block,
                    else_block,
                } => SourceTerminator::Branch {
                    cond,
                    then_block: BlockId(remap[then_block.0]),
                    else_block: BlockId(remap[else_block.0]),
                },
                SourceTerminator::Suspend { operation, resume } => SourceTerminator::Suspend {
                    operation,
                    resume: BlockId(remap[resume.0]),
                },
                ret @ (SourceTerminator::Return(_) | SourceTerminator::CleanupReturn) => ret,
            };
            block
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Text rendering (`--emit-lir`)
// ---------------------------------------------------------------------------

/// Render a lowered program as labeled basic blocks.
#[cfg(test)]
pub(crate) fn format_source_program(program: &SourceProgram) -> String {
    let mut out = String::new();
    for f in program
        .functions
        .iter()
        .chain(program.lambdas.iter().map(|l| &l.function))
    {
        let params = f
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, super::dump::type_text(&p.ty)))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "fn {}({}) -> {} {{\n",
            f.name,
            params,
            super::dump::type_text(&f.return_type)
        ));
        for block in &f.blocks {
            out.push_str(&format!("bb{}:\n", block.id.0));
            if !block.recovery.is_empty() {
                let targets = block
                    .recovery
                    .iter()
                    .map(|target| format!("bb{}", target.0))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("  ; panic may resume at {targets}\n"));
            }
            for inst in &block.instrs {
                out.push_str(&format!("  {}\n", format_inst(inst)));
            }
            out.push_str(&format!("  {}\n", format_terminator(&block.terminator)));
        }
        out.push_str("}\n");
    }
    out
}

#[cfg(test)]
fn format_inst(inst: &SourceInst) -> String {
    let e = super::dump::expr_text;
    match inst {
        SourceInst::Compute { local, value, .. } => format!("%{} = {value:?}", local.0),
        SourceInst::EnterDeferScope { sites, lock, .. } => {
            let owns = match lock {
                Some(slots) => format!(", holds {}", slots.mode.keyword()),
                None => String::new(),
            };
            format!("enter defer scope ({} sites{owns});", sites.len())
        }
        SourceInst::LeaveDeferScope { sites } => {
            format!("leave defer scope ({} sites);", sites.len())
        }
        SourceInst::FlushDefers { sites, scopes } => {
            format!("flush defers ({} sites, {scopes} scopes);", sites.len())
        }
        SourceInst::ClearScopeRoots { locals } => {
            let names = locals
                .iter()
                .map(|l| format!("l{}", l.0))
                .collect::<Vec<_>>()
                .join(", ");
            format!("clear scope roots ({names});")
        }
        SourceInst::ReleaseLock(slots) => format!("release {};", slots.mode.keyword()),
        SourceInst::Defer { body, .. } => {
            let captures = body
                .captures
                .iter()
                .map(|id| format!("l{}", id.0))
                .collect::<Vec<_>>()
                .join(", ");
            let mut text = format!("defer cleanup captures ({captures}) {{\n");
            for block in &body.function.blocks {
                text.push_str(&format!("    bb{}:\n", block.id.0));
                for inst in &block.instrs {
                    text.push_str(&format!("      {}\n", format_inst(inst)));
                }
                text.push_str(&format!("      {}\n", format_terminator(&block.terminator)));
            }
            text.push_str("  }");
            text
        }

        SourceInst::Let {
            name,
            mutable,
            ty,
            value,
            ..
        } => {
            let kw = if *mutable { "let mut" } else { "let" };
            // Only an annotation that WIDENS the initialiser is printed — that
            // is the case a reader cannot infer from the value.
            if *ty == value.ty {
                format!("{kw} {name} = {};", e(value))
            } else {
                format!(
                    "{kw} {name}: {} = {};",
                    super::dump::type_text(ty),
                    e(value)
                )
            }
        }
        SourceInst::Assign { name, value, .. } => format!("{name} = {};", e(value)),
        SourceInst::FieldAssign {
            object,
            field,
            value,
        } => format!("{}.{field} = {};", e(object), e(value)),
        SourceInst::IndexAssign {
            array,
            index,
            value,
        } => format!("{}[{}] = {};", e(array), e(index), e(value)),
        SourceInst::StaticFieldAssign {
            class,
            field,
            value,
        } => format!("{class}::{field} = {};", e(value)),
        SourceInst::SuperInit { args, .. } => {
            let args = args.iter().map(e).collect::<Vec<_>>().join(", ");
            format!("super.init({args});")
        }
        SourceInst::MatchTest {
            scrutinee, result, ..
        } => format!("l{} = match.test l{};", result.0, scrutinee.0),
        SourceInst::MatchBind {
            scrutinee,
            bindings,
            ..
        } => {
            let names = bindings
                .iter()
                .map(|b| format!("l{}", b.0))
                .collect::<Vec<_>>()
                .join(", ");
            format!("({names}) = match.bind l{};", scrutinee.0)
        }
        SourceInst::SelectInit { .. } => "select.init;".to_string(),
        SourceInst::SelectProbe { .. } => "select.probe;".to_string(),
        SourceInst::SelectPick { .. } => "select.pick;".to_string(),
        SourceInst::SelectUnregister { .. } => "select.unregister;".to_string(),
        SourceInst::SelectCommit { .. } => "select.commit;".to_string(),
        SourceInst::Expr(expr) => format!("{};", e(expr)),
    }
}

#[cfg(test)]
fn format_terminator(t: &SourceTerminator) -> String {
    let e = super::dump::expr_text;
    match t {
        SourceTerminator::Jump(b) => format!("jump bb{}", b.0),
        SourceTerminator::Branch {
            cond,
            then_block,
            else_block,
        } => format!("branch {} bb{} bb{}", e(cond), then_block.0, else_block.0),
        SourceTerminator::Suspend { operation, resume } => {
            format!("suspend {operation:?} -> bb{}", resume.0)
        }
        SourceTerminator::Return(Some(v)) => format!("return {}", e(v)),
        SourceTerminator::Return(None) => "return".to_string(),
        SourceTerminator::CleanupReturn => "cleanup return".to_string(),
    }
}

#[cfg(test)]
#[path = "lowered_tests.rs"]
mod tests;

#[cfg(test)]
mod prune_and_corpus_tests;

mod final_ir;
pub use final_ir::{
    LirBlock, LirDeferBody, LirFunction, LirInst, LirLambda, LirParam, LirPattern, LirProgram,
    Terminator,
};

pub fn lower_program(program: &HirProgram) -> LirProgram {
    final_ir::finish_program(lower_source_program(program))
}

/// Finish one independently stored body after the unit's shared optimization.
pub(crate) fn finish_body(body: SourceFunction) -> LirFunction {
    final_ir::finish_function(body)
}

pub fn format_program(program: &LirProgram) -> String {
    let mut output = String::new();
    let mut pending: std::collections::VecDeque<_> = program
        .functions
        .iter()
        .chain(program.lambdas.iter().map(|lambda| &lambda.function))
        .map(|function| (None, function))
        .collect();
    let mut next_region = 0usize;
    while let Some((region, function)) = pending.pop_front() {
        if let Some(region) = region {
            output.push_str(&format!("cleanup region {region}:\n"));
        }
        output.push_str(&format!(
            "fn {}({}) -> {} {{\n",
            function.name,
            function
                .params
                .iter()
                .map(|param| format!("{}: {}", param.name, super::dump::type_text(&param.ty)))
                .collect::<Vec<_>>()
                .join(", "),
            super::dump::type_text(&function.return_type)
        ));
        for local in &function.locals {
            if let Some(slot) = function.async_frame.slot(local.id) {
                output.push_str(&format!(
                    "  frame slot {}: local {} `{}`;\n",
                    slot.index, local.id.0, local.name
                ));
            }
        }
        for block in &function.blocks {
            output.push_str(&format!("bb{}:\n", block.id.0));
            for instruction in &block.instrs {
                if let LirInst::Defer { id, body, span } = instruction {
                    let region = next_region;
                    next_region += 1;
                    output.push_str(&format!("  Defer {{ id: {id:?}, region: {region}, captures: {:?}, recovery_capable: {}, span: {span:?} }}\n", body.captures, body.recovery_capable));
                    pending.push_back((Some(region), &body.function));
                } else {
                    output.push_str(&format!("  {instruction:?}\n"));
                }
            }
            output.push_str(&format!("  {:?}\n", block.terminator));
        }
        output.push_str("}\n");
    }
    output
}

fn contract_check(clause: HirExpr, post: bool) -> HirStmt {
    let span = clause.span;
    HirStmt::Expr(HirExpr {
        kind: HirExprKind::Call {
            binding: HirCallBinding::Function,
            callee: "$contract_check".into(),
            args: vec![
                clause,
                HirExpr {
                    kind: HirExprKind::Int(i64::from(post)),
                    ty: Type::I64,
                    span,
                },
            ],
        },
        ty: Type::Void,
        span,
    })
}

impl Builder<'_> {
    /// Share the postcondition CFG across all normal returns. Neither clause
    /// trees nor their arithmetic are copied per exit.
    fn contract_epilogue(&mut self, f: &HirFunction) {
        let result = (f.return_type != Type::Void).then(|| {
            self.declare_local(
                "$contract_result".into(),
                f.return_type.clone(),
                None,
                LocalOptions {
                    synthetic: true,
                    parameter: false,
                },
            )
        });
        let exit = self.new_block();
        for index in 0..exit.0 {
            let term = &mut self.blocks[index].1;
            let value = match term {
                Some(SourceTerminator::Return(None)) if f.return_type != Type::Void => continue,
                Some(SourceTerminator::Return(value)) => value.take(),
                None if f.return_type == Type::Void => None,
                Some(
                    SourceTerminator::Jump(_)
                    | SourceTerminator::Branch { .. }
                    | SourceTerminator::Suspend { .. }
                    | SourceTerminator::CleanupReturn,
                )
                | None => continue,
            };
            if let Some(value) = value {
                self.blocks[index].0.push(if let Some(local) = result {
                    SourceInst::Assign {
                        local,
                        name: "$contract_result".into(),
                        value,
                    }
                } else {
                    SourceInst::Expr(value)
                });
            }
            self.blocks[index].1 = Some(SourceTerminator::Jump(exit));
        }
        self.switch_to(exit);
        for clause in &f.ensures {
            let mut clause = clause.clone();
            clause.visit_mut_preorder(false, |node| {
                if let HirExprKind::Var(name) = &mut node.kind
                    && name == "result"
                {
                    *name = "$contract_result".into();
                }
                true
            });
            self.lower_stmt(&contract_check(clause, true));
        }
        self.terminate(SourceTerminator::Return(
            result.map(|local| self.local_expr(local, f.span)),
        ));
    }
}
