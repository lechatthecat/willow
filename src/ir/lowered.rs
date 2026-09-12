//! Lowered IR (LIR): the typed HIR with control flow made explicit as basic
//! blocks — willow-mb5, the `Lowered IR` stage of the pipeline mandated by the
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

use crate::diagnostics::Span;
use crate::parser::ast::{ExprId, LockMode};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::ids::{FunctionId, SemanticType as Type, TypeId};
use crate::semantic::type_checker::types::{await_output_type, awaitable_task_type};

use super::typed_ast::{
    HirCapture, HirDeferBody, HirDeferId, HirExpr, HirExprKind, HirFunction, HirParam, HirPattern,
    HirProgram, HirStmt,
};

pub mod async_liveness;
mod lifetime;
pub mod value;
pub use value::{LirOperand, LirPlace, LirRvalue};

use async_liveness::LirAsyncFrameLayout;

/// A whole program in lowered IR.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceProgram {
    pub functions: Vec<SourceFunction>,
    /// Lambda bodies, lifted out of the expressions that contain them
    /// (willow-0g8j.2.2). They are kept apart from `functions` because the LIR
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
    /// The captured environment this body is entered with, in slot order
    /// (willow-0g8j.2.12). Empty for every function that is not a lifted
    /// capturing lambda. Each entry is also a `parameter` local, because the
    /// backend binds it at entry exactly like a parameter — the difference is
    /// only where the value comes from: environment word `i + 1` instead of an
    /// argument register.
    pub captures: Vec<LirCapture>,
}

/// One slot of a lifted lambda's closure environment.
#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LirLocalId(pub u32);

/// Stable identity of one source `defer` registration site. Its source span
/// is diagnostic metadata; frame flags and cancellation cleanup use this id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LirDeferId(pub u32);

/// Physical storage is independent of source-language types. GcOwner holds
/// an opaque managed allocation base used only by lowered reference places.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LirStorageKind {
    #[default]
    Value,
    GcOwner,
}

#[derive(Debug, Clone, PartialEq)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockId(pub usize);

/// A straight-line run of instructions ended by exactly one terminator.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SourceBlock {
    pub id: BlockId,
    pub instrs: Vec<SourceInst>,
    pub terminator: SourceTerminator,
    /// Where a panic raised in this block can continue: the resume block of
    /// every enclosing recovery-capable `defer` scope, innermost first
    /// (willow-0g8j.2.11).
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
#[derive(Debug, Clone, PartialEq)]
pub enum SuspendOp {
    Sleep {
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
        result_ty: Type,
    },
    SelectWait {
        operations: Vec<LirSelectWaitOp>,
    },
    /// Acquire the critical section of a `lock`/`read`/`write` statement
    /// (willow-0g8j.2.13).
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

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub enum LirSelectOp {
    Recv {
        channel: LirLocalId,
        binding: Option<LirLocalId>,
        elem_ty: Type,
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
    pub(crate) fn collect_locals(&self, out: &mut std::collections::HashSet<LirLocalId>) {
        let mut insert = |local| {
            out.insert(local);
        };
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
                for local in slots.locals() {
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

/// The frame-backed slots one critical section owns (willow-0g8j.2.13).
///
/// The same four locals appear at the acquisition, at every release, and on the
/// scope that owns the section's panic cleanup, so they travel together: a
/// consumer that gets them from one of the three is looking at the same lock.
#[derive(Debug, Clone, PartialEq)]
pub struct LirLockSlots {
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
    /// Open a lexical scope that owns `sites` defer registrations
    /// (willow-0g8j.2.3).
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
        /// Set when this scope IS a `lock` body (willow-0g8j.2.13). The scope
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
    /// scope metadata (willow-0g8j.2.3). Emitted immediately before a
    /// `return`, `break` or `continue` that leaves their scopes.
    FlushDefers {
        sites: Vec<LirDeferId>,
    },
    /// The end of the lexical scope that declared `locals` (willow-0g8j.3.3).
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
    /// `defer` registration (willow-vynv.2). `span` is the `defer` statement's
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
        /// binds `a` as the interface while the initialiser is the class
        /// (willow-0g8j.5).
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
        /// needs one for its panic call-chain frame (willow-0g8j.2.18).
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
        winner: usize,
    },
    SelectCommit {
        operation: LirSelectOp,
        success: LirLocalId,
    },
    /// Commit the protected value and release the critical section opened by a
    /// [`SuspendOp::LockAcquire`] (willow-0g8j.2.13).
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
    /// Does the scrutinee match one arm's pattern? (willow-0g8j.2.11.1)
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
    /// Bring one arm's pattern bindings into their own LIR locals
    /// (willow-0g8j.2.11.1).
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

/// Lower every function (free functions and class methods, flattened as
/// `Class::method`) of a typed-HIR program to basic blocks.
pub(crate) fn lower_source_program(program: &HirProgram) -> SourceProgram {
    let resolution = std::rc::Rc::new(program.resolution.clone());
    let mut functions = Vec::with_capacity(program.functions.len());
    let mut lambdas = Vec::new();
    for f in &program.functions {
        functions.push(lower_function(f, None, resolution.clone()));
        collect_lambdas(&f.body, &mut lambdas, &resolution);
    }
    for c in &program.classes {
        for m in &c.methods {
            functions.push(lower_function(m, Some(&c.name), resolution.clone()));
            collect_lambdas(&m.body, &mut lambdas, &resolution);
        }
    }
    value::lower_calls(&mut functions, &mut lambdas, &program.resolution);
    super::optimize::inline_scalar_leaves(&mut functions);
    // The lifted graph is the sole owner of each executable lambda body.
    // Enclosing expressions retain only closure construction metadata.
    for function in functions
        .iter_mut()
        .chain(lambdas.iter_mut().map(|lambda| &mut lambda.function))
    {
        super::optimize::eliminate_tail_recursion(function);
        super::optimize::inline_scalar_recursion(function);
        super::optimize::unroll_scalar_loops(function);
        lifetime::clear_dead_temporaries(function);
        function.async_frame = async_liveness::analyze(&function.blocks, &function.locals);
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

/// Lift every lambda in a statement body, innermost first, into its own block
/// graph (willow-0g8j.2.2).
///
/// A lambda body is not part of the enclosing function's control flow — it is a
/// separate function the backend compiles under its own symbol — so lowering it
/// inline would put its blocks in the wrong graph. The walk goes through
/// [`HirExpr::children`], whose `Lambda` case yields the body's expressions, so
/// a lambda nested inside another lambda is reached the same way as one nested
/// in a call argument.
fn collect_lambdas(
    body: &[HirStmt],
    out: &mut Vec<SourceLambda>,
    resolution: &std::rc::Rc<super::typed_ast::HirResolution>,
) {
    for stmt in body {
        for expr in stmt.child_exprs() {
            collect_lambdas_in_expr(expr, out, resolution);
        }
    }
}

fn collect_lambdas_in_expr(
    expr: &HirExpr,
    out: &mut Vec<SourceLambda>,
    resolution: &std::rc::Rc<super::typed_ast::HirResolution>,
) {
    for expr in expr.walk_postorder(true) {
        collect_lambda(expr, out, resolution);
    }
}

fn collect_lambda(
    expr: &HirExpr,
    out: &mut Vec<SourceLambda>,
    resolution: &std::rc::Rc<super::typed_ast::HirResolution>,
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
        let mut b = Builder::new_lambda(params, captures);
        b.resolution = resolution.clone();
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
            _ => break "<expression>".into(),
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
                _ => out.push_str("<expr>"),
            }
            out.push(']');
        }
    }
    out
}

fn format_segments(args: &[HirExpr]) -> Option<Vec<crate::interpolate::Segment>> {
    use crate::interpolate::Segment;
    let HirExprKind::Str(spec) = &args.first()?.kind else {
        return None;
    };
    let segments = crate::interpolate::parse_spec(spec).ok()?;
    let mut operands = args[1..].iter();
    for segment in &segments {
        match segment {
            Segment::Literal(_) => {}
            Segment::Display => {
                if !matches!(
                    operands.next()?.ty,
                    Type::I64 | Type::F64 | Type::Bool | Type::String
                ) {
                    return None;
                }
            }
            Segment::F64(_) => {
                if operands.next()?.ty != Type::F64 {
                    return None;
                }
            }
        }
    }
    operands.next().is_none().then_some(segments)
}

fn class_fields(
    resolution: &super::typed_ast::HirResolution,
    class: &TypeId,
) -> Option<Vec<(String, Type)>> {
    let mut chain = Vec::new();
    let mut next = Some(*class);
    let mut seen = std::collections::HashSet::new();
    while let Some(class) = next {
        if !seen.insert(class) {
            return None;
        }
        let info = resolution.classes.get(&class)?;
        chain.push(info);
        next = info.base;
    }
    let mut fields = Vec::new();
    let mut names = std::collections::HashSet::new();
    for info in chain.into_iter().rev() {
        for (name, ty) in &info.fields {
            if names.insert(name.clone()) {
                fields.push((name.clone(), ty.clone()));
            }
        }
    }
    Some(fields)
}

fn arguments_compatible(
    resolution: &super::typed_ast::HirResolution,
    params: &[Type],
    args: &[HirExpr],
) -> bool {
    params.len() == args.len()
        && params.iter().zip(args).all(|(target, argument)| {
            if matches!(argument.kind, HirExprKind::ReferenceArg { .. }) {
                *target == argument.ty
            } else {
                resolution.can_coerce(&argument.ty, target)
            }
        })
}

fn static_signature(
    resolution: &super::typed_ast::HirResolution,
    class: &TypeId,
    method: &str,
) -> Option<super::typed_ast::HirSignature> {
    resolution
        .modules
        .get(class)
        .and_then(|functions| functions.get(method))
        .cloned()
        .or_else(|| method_signature(resolution, &Type::Named(*class), method))
}

fn method_signature(
    resolution: &super::typed_ast::HirResolution,
    receiver: &Type,
    method: &str,
) -> Option<super::typed_ast::HirSignature> {
    let (Type::Named(class) | Type::Generic(class, _)) = receiver else {
        return None;
    };
    let mut next = Some(*class);
    let mut seen = std::collections::HashSet::new();
    while let Some(class) = next {
        if !seen.insert(class) {
            return None;
        }
        if let Some(interface) = resolution.interfaces.get(&class) {
            let mut signature = interface.methods.get(method)?.clone();
            let mut substitutions =
                std::collections::HashMap::from([(TypeId::local("Self"), receiver.clone())]);
            if let Type::Generic(_, args) = receiver {
                if interface.type_params.len() != args.len() {
                    return None;
                }
                substitutions.extend(
                    interface
                        .type_params
                        .iter()
                        .copied()
                        .zip(args.iter().cloned()),
                );
            } else if !interface.type_params.is_empty() {
                return None;
            }
            signature.params = signature
                .params
                .iter()
                .map(|ty| crate::semantic::symbols::substitute_type(ty, &substitutions))
                .collect();
            signature.return_type =
                crate::semantic::symbols::substitute_type(&signature.return_type, &substitutions);
            return Some(signature);
        }
        let info = resolution.classes.get(&class)?;
        if let Some(signature) = info.methods.get(method) {
            return Some(signature.clone());
        }
        next = info.base;
    }
    None
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
        .any(|expr| matches!(&expr.kind, HirExprKind::Call { callee, .. } if callee.is_free_named("recover")))
}

fn block_has_recover(stmts: &[HirStmt]) -> bool {
    stmts
        .iter()
        .flat_map(HirStmt::child_exprs)
        .any(expr_has_recover)
}

/// Lower one function's statement tree into a block graph.
fn lower_function(
    f: &HirFunction,
    class: Option<&TypeId>,
    resolution: std::rc::Rc<super::typed_ast::HirResolution>,
) -> SourceFunction {
    let mut b = Builder::new(&f.params, f.is_async);
    b.resolution = resolution;
    b.return_type = f.return_type.clone();
    b.lower_scope(&f.body);
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
        async_frame: if f.is_async {
            async_liveness::analyze(&blocks, &locals)
        } else {
            LirAsyncFrameLayout::default()
        },
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
/// defer scopes to flush (willow-0g8j.2.3) and how many lexical scopes' GC
/// roots to drop (willow-0g8j.3.3) — the ones opened inside the loop body, not
/// the ones that were already open when the loop started.
#[derive(Debug, Clone, Copy)]
struct LirLoopFrame {
    exit: BlockId,
    next: BlockId,
    defer_depth: usize,
    scope_depth: usize,
}

/// One open lexical scope, for the GC-root close at its end (willow-0g8j.3.3).
#[derive(Debug, Clone)]
struct LirScopeMark {
    /// Where this scope's locals begin in [`Builder::locals`]. Locals are handed
    /// out from one growing table, so its own bindings — and those of every
    /// scope nested in it — are exactly the entries from here to the end.
    first_local: usize,
    /// Locals the scope owns that the sweep from `first_local` skips. That sweep
    /// drops synthetic locals, because lowering declares them for values that
    /// cross block boundaries and are read after the scope that declared them
    /// ends; a `for` loop's element binding is flagged synthetic for a different
    /// reason — lowering synthesizes its `let` from the iteration protocol — and
    /// it really does end with the body.
    adopted: Vec<LirLocalId>,
}

impl LirScopeMark {
    fn opening_at(first_local: usize) -> Self {
        Self {
            first_local,
            adopted: Vec::new(),
        }
    }
}

/// Block-graph builder: appends instructions to a current block and seals
/// blocks with terminators as control flow branches and rejoins.
struct Builder {
    return_type: Type,
    resolution: std::rc::Rc<super::typed_ast::HirResolution>,
    blocks: Vec<(Vec<SourceInst>, Option<SourceTerminator>)>,
    /// Per-block [`SourceBlock::recovery`], filled in by `lower_scope` once it
    /// knows the scope's resume block.
    block_recovery: Vec<Vec<BlockId>>,
    current: usize,
    /// Counter for synthesized `for` induction variables, unique per function
    /// so nested loops do not collide.
    for_counter: usize,
    /// Innermost-first loop context for break/continue lowering
    /// (willow-kzka).
    loop_stack: Vec<LirLoopFrame>,
    /// How many defer scopes are currently open. `return` flushes all of them.
    defer_depth: usize,
    defer_counter: u32,
    defer_scopes: Vec<std::collections::HashMap<HirDeferId, LirDeferId>>,
    is_async: bool,
    suspend_counter: usize,
    locals: Vec<LirLocal>,
    /// The currently open lexical scopes, outermost first (willow-0g8j.3.3).
    scope_starts: Vec<LirScopeMark>,
    /// Flat rather than a scope stack, because HIR names are already unique
    /// within a function: `LowerCtx::bind` alpha-renames every shadowing
    /// binding to `name$n` before lowering runs. That invariant is what lets a
    /// `HirExprKind::Var` node resolve to exactly one [`LirLocalId`] here and
    /// in `async_liveness`; without it two frame slots would silently alias.
    /// [`SourceFunction::assert_unique_local_names`] pins it.
    local_by_name: std::collections::HashMap<String, LirLocalId>,
    /// The critical section currently being lowered, if any (willow-0g8j.2.13).
    ///
    /// At most one can be open: E2605 rejects a `lock` nested inside another
    /// one. It is what tells an early exit whether it is leaving the section
    /// and therefore has to release it.
    active_lock: Option<ActiveLock>,
}

/// The critical section [`Builder::active_lock`] is inside.
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
                && !matches!(&inner.kind, HirExprKind::Call { callee, .. } if matches!(callee.name(), "sleep" | "yield")) =>
        {
            false
        }
        HirExprKind::Await { .. } | HirExprKind::Select { .. } => true,
        HirExprKind::MethodCall { object, method, .. } => {
            builtin_types::unary_arg(&object.ty, B::Channel).is_some()
                && matches!(method.as_str(), "send" | "recv")
        }
        _ => false,
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
/// emitter destructures them (willow-0g8j.2.11.1).
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
            _ => false,
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
    lower_conditional_branches,
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
impl Builder {
    fn new(params: &[HirParam], is_async: bool) -> Self {
        let mut builder = Self {
            return_type: Type::Void,
            resolution: Default::default(),
            blocks: vec![(Vec::new(), None)],
            block_recovery: vec![Vec::new()],
            current: 0,
            for_counter: 0,
            loop_stack: Vec::new(),
            defer_depth: 0,
            active_lock: None,
            defer_counter: 0,
            defer_scopes: Vec::new(),
            is_async,
            suspend_counter: 0,
            locals: Vec::new(),
            scope_starts: Vec::new(),
            local_by_name: std::collections::HashMap::new(),
        };
        for param in params {
            builder.declare_local(
                param.name.clone(),
                param.ty.clone(),
                Some(param.span),
                false,
                true,
            );
        }
        builder
    }

    /// A lifted lambda body, whose entry binds the captured environment before
    /// the declared parameters (willow-0g8j.2.12).
    ///
    /// Captures come first because that is the order lowering bound them in,
    /// and they are `parameter` locals for the same reason a parameter is: the
    /// backend gives them their value at entry, so nothing in the body may
    /// clear or re-slot them on a scope exit.
    fn new_lambda(params: &[HirParam], captures: &[HirCapture]) -> Self {
        let mut builder = Self::new(&[], false);
        for capture in captures {
            builder.declare_local(capture.name.clone(), capture.ty.clone(), None, false, true);
        }
        for param in params {
            builder.declare_local(
                param.name.clone(),
                param.ty.clone(),
                Some(param.span),
                false,
                true,
            );
        }
        builder
    }

    fn lower_defer_region(&mut self, stmts: &[HirStmt], recovery_capable: bool) -> SourceDeferBody {
        let mut region = Self::new(&[], false);
        region.resolution = self.resolution.clone();
        region.locals = self.locals.clone();
        region
            .locals
            .iter_mut()
            .for_each(|local| local.parameter = true);
        region.local_by_name = self.local_by_name.clone();
        region.suspend_counter = self.suspend_counter;
        region.for_counter = self.for_counter;
        region.lower_scope(stmts);
        for (_, terminator) in &mut region.blocks {
            if terminator.is_none() {
                *terminator = Some(SourceTerminator::CleanupReturn);
            }
        }
        let (blocks, locals) = region.finish();
        let names = locals
            .iter()
            .map(|local| (local.name.as_str(), local.id))
            .collect();
        let mut referenced = std::collections::HashSet::new();
        for block in &blocks {
            for inst in &block.instrs {
                let mut writes = std::collections::HashSet::new();
                async_liveness::instruction_use_def(inst, &names, &mut referenced, &mut writes);
                referenced.extend(writes);
            }
            async_liveness::terminator_uses(
                &block.terminator,
                &names,
                &mut referenced,
                &std::collections::HashSet::new(),
            );
        }
        let mut captures: Vec<_> = referenced
            .into_iter()
            .filter(|local| (local.0 as usize) < self.locals.len())
            .collect();
        captures.sort_by_key(|local| local.0);
        let name = self
            .locals
            .iter()
            .find(|local| local.name == "self")
            .and_then(|local| {
                if let Type::Named(owner) = &local.ty {
                    Some(FunctionId::method(*owner, "$defer"))
                } else {
                    None
                }
            })
            .unwrap_or_else(|| FunctionId::free("$defer"));
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
        synthetic: bool,
        parameter: bool,
    ) -> LirLocalId {
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
        let local = self.declare_local(name.clone(), ty.clone(), source_span, synthetic, false);
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
            let local = self.declare_local(name.to_string(), value.ty.clone(), None, true, false);
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

    fn synthetic_name(&mut self, role: &str) -> String {
        let n = self.suspend_counter;
        self.suspend_counter += 1;
        format!("__async_{role}_{n}")
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
    fn capture_defer_expr(&mut self, id: LirDeferId, expr: &HirExpr) -> HirExpr {
        let capture = |this: &mut Self, role: &str, value: &HirExpr| {
            let name = format!("__defer{}_{}", id.0, role);
            let local = this.push_synth_let(&name, false, value.clone());
            this.local_expr(local, value.span)
        };
        let mut out = expr.clone();
        out.kind = match &expr.kind {
            HirExprKind::Call { callee, args } => HirExprKind::Call {
                callee: *callee,
                args: args
                    .iter()
                    .enumerate()
                    .map(|(index, arg)| capture(self, &format!("arg{index}"), arg))
                    .collect(),
            },
            HirExprKind::MethodCall {
                object,
                method,
                args,
            } => HirExprKind::MethodCall {
                object: Box::new(capture(self, "self", object)),
                method: method.clone(),
                args: args
                    .iter()
                    .enumerate()
                    .map(|(index, arg)| capture(self, &format!("arg{index}"), arg))
                    .collect(),
            },
            HirExprKind::Print { value, newline } => HirExprKind::Print {
                value: Box::new(capture(self, "print", value)),
                newline: *newline,
            },
            _ => return expr.clone(),
        };
        out
    }

    /// Lower root expression control flow or a scheduler suspension.
    /// Suspension operands are evaluated once before parking; an optional
    /// destination receives the selected branch value or resumed result.
    fn lower_root_suspend(
        &mut self,
        value: &HirExpr,
        destination: Option<LirLocalId>,
    ) -> Option<Option<HirExpr>> {
        if let Some(lowered) = self.lower_try_propagate(value, destination) {
            return Some(lowered);
        }
        if let Some(lowered) = self.lower_match_arms_cfg(value, destination) {
            return Some(lowered);
        }
        if let Some(lowered) = self.lower_conditional_suspend(value) {
            if let Some(destination) = destination {
                let local = &self.locals[destination.0 as usize];
                self.push(SourceInst::Assign {
                    local: destination,
                    name: local.name.clone(),
                    value: lowered,
                });
                return Some(Some(self.local_expr(destination, value.span)));
            }
            return Some(None);
        }
        if !self.is_async {
            return None;
        }
        let operation = match &value.kind {
            // Awaiting a `Future<T>` VALUE is a BLOCKING runtime call
            // (`willow_future_await_*`), not a scheduler suspension: there is
            // no async frame behind that pointer to park on. Splitting it into
            // a `Suspend` would hand `SuspendOp::AwaitTask` a raw future
            // pointer to read frame slots out of, so this refuses the SPLIT and
            // leaves the await as an ordinary expression for the walker to emit
            // as that call (willow-0g8j.3) — the function itself still compiles
            // from LIR. `await sleep(..)` / `await yield()` are not this case:
            // their type is `Future<void>` too, but they are recognised by
            // their CALL shape below and become real suspensions.
            HirExprKind::Await { inner }
                if awaitable_task_type(&inner.ty).is_none()
                    && builtin_types::unary_arg(&inner.ty, B::Future).is_some()
                    && !matches!(&inner.kind, HirExprKind::Call { callee, args }
                        if (callee.is_free_named("sleep") && args.len() == 1)
                            || (callee.is_free_named("yield") && args.is_empty())) =>
            {
                return None;
            }
            HirExprKind::Await { inner } => {
                if let HirExprKind::Call { callee, args } = &inner.kind {
                    match (callee.name(), args.as_slice()) {
                        ("sleep", [millis]) if value.ty == Type::Void => {
                            let millis = self.lower_operand_before_suspend(millis)?;
                            let name = self.synthetic_name("sleep_millis");
                            let millis = self.push_synth_let(&name, false, millis);
                            SuspendOp::Sleep { millis }
                        }
                        ("yield", []) if value.ty == Type::Void => SuspendOp::Yield,
                        _ => {
                            let awaited = self.lower_operand_before_suspend(inner)?;
                            let name = self.synthetic_name("task");
                            let task = self.push_synth_let(&name, false, awaited);
                            let (result_ty, cancel_aware) = awaitable_task_type(&inner.ty)
                                .unwrap_or_else(|| (value.ty.clone(), false));
                            SuspendOp::AwaitTask {
                                task,
                                result: destination,
                                result_ty,
                                cancel_aware,
                            }
                        }
                    }
                } else {
                    let awaited = self.lower_operand_before_suspend(inner)?;
                    let name = self.synthetic_name("task");
                    let task = self.push_synth_let(&name, false, awaited);
                    let (result_ty, cancel_aware) =
                        awaitable_task_type(&inner.ty).unwrap_or_else(|| (value.ty.clone(), false));
                    SuspendOp::AwaitTask {
                        task,
                        result: destination,
                        result_ty,
                        cancel_aware,
                    }
                }
            }
            HirExprKind::MethodCall {
                object,
                method,
                args,
            } if builtin_types::unary_arg(&object.ty, B::Channel).is_some() => {
                let receiver = self.lower_operand_before_suspend(object)?;
                let channel_name = self.synthetic_name("channel");
                let channel = self.push_synth_let(&channel_name, false, receiver);
                match (method.as_str(), args.as_slice()) {
                    ("recv", []) => SuspendOp::ChannelRecv {
                        channel,
                        result: destination,
                        result_ty: value.ty.clone(),
                    },
                    ("send", [sent]) => {
                        let Type::Generic(_, type_args) = &object.ty else {
                            unreachable!("channel method receiver has a generic channel type");
                        };
                        let elem_ty = type_args
                            .first()
                            .expect("Channel has an element type")
                            .clone();
                        let sent = self.lower_operand_before_suspend(sent)?;
                        let value_name = self.synthetic_name("send_value");
                        let sent = self.push_synth_let(&value_name, false, sent);
                        SuspendOp::ChannelSend {
                            channel,
                            value: sent,
                            elem_ty,
                        }
                    }
                    _ => return None,
                }
            }
            _ => return None,
        };
        let resume = self.new_block();
        self.terminate(SourceTerminator::Suspend { operation, resume });
        self.switch_to(resume);
        Some(destination.map(|local| self.local_expr(local, value.span)))
    }

    /// Split every root `match` into a dispatch chain with scoped arm bodies.
    /// The scrutinee is evaluated once and an optional destination receives
    /// the selected arm's value. Suspensions use the ordinary statement path.
    fn lower_match_arms_cfg(
        &mut self,
        value: &HirExpr,
        destination: Option<LirLocalId>,
    ) -> Option<Option<HirExpr>> {
        let HirExprKind::Match { scrutinee, arms } = &value.kind else {
            return None;
        };

        // Evaluated once, before any test. A suspension in the scrutinee itself
        // splits here, in front of the whole dispatch.
        // The whole `match` is a scope of its own (willow-0g8j.3.3). The
        // scrutinee is held in a temp that outlives every arm and that no source
        // scope declared, so — like a `for` loop's hoisted iterable — the
        // construct has to drop its root itself, at the merge every arm reaches.
        let construct = self.scope_starts.len();
        self.scope_starts
            .push(LirScopeMark::opening_at(self.locals.len()));
        let scrutinee_name = self.synthetic_name("match_scrutinee");
        let scrutinee_local =
            self.declare_local(scrutinee_name, scrutinee.ty.clone(), None, true, false);
        self.scope_starts[construct].adopted.push(scrutinee_local);
        self.lower_value_into(scrutinee_local, scrutinee);

        let merge = self.new_block();
        for (index, arm) in arms.iter().enumerate() {
            // A wildcard or a whole-value binding always applies, so it needs
            // no test and nothing after it is reachable.
            let always_matches = matches!(
                arm.pattern,
                HirPattern::Wildcard | HirPattern::Binding { .. }
            );
            let is_last = index + 1 == arms.len();
            let arm_block = self.new_block();
            let next = (!always_matches && !is_last).then(|| self.new_block());
            if always_matches {
                self.terminate(SourceTerminator::Jump(arm_block));
            } else {
                let test_name = self.synthetic_name("match_test");
                let test = self.declare_local(test_name, Type::Bool, None, true, false);
                self.push(SourceInst::MatchTest {
                    scrutinee: scrutinee_local,
                    pattern: arm.pattern.clone(),
                    result: test,
                    span: arm.span,
                });
                // The last arm falling through means the scrutinee matched
                // nothing; the merge keeps the result at its seeded value,
                // exactly as the tree-shaped `match` does.
                self.terminate(SourceTerminator::Branch {
                    cond: self.local_expr(test, arm.span),
                    then_block: arm_block,
                    else_block: next.unwrap_or(merge),
                });
            }

            self.switch_to(arm_block);
            // The arm is a scope, and its pattern bindings belong to it: they
            // are declared HERE, ahead of the body's own scope, so the body's
            // close does not reach them (willow-0g8j.3.3).
            let arm_scope = self.scope_starts.len();
            self.scope_starts
                .push(LirScopeMark::opening_at(self.locals.len()));
            let bindings: Vec<_> = pattern_bindings(&arm.pattern)
                .into_iter()
                .map(|(name, ty)| {
                    // Discarded payloads still occupy binding positions but
                    // must not alias another arm's discarded payload local.
                    let name = if name == "_" {
                        self.synthetic_name("discard")
                    } else {
                        name
                    };
                    self.declare_local(name, ty, Some(arm.span), false, false)
                })
                .collect();
            if !bindings.is_empty() {
                self.push(SourceInst::MatchBind {
                    scrutinee: scrutinee_local,
                    pattern: arm.pattern.clone(),
                    bindings,
                    span: arm.span,
                });
            }
            self.lower_match_arm_body(&arm.body, destination);
            // After the body: an arm that produces a value has already copied it
            // into `destination`, which has a rooted slot of its own.
            self.push_scope_root_clears(arm_scope);
            self.scope_starts.pop();
            self.terminate(SourceTerminator::Jump(merge));

            if let Some(next) = next {
                self.switch_to(next);
            }
            if always_matches {
                break;
            }
        }

        self.switch_to(merge);
        self.push_scope_root_clears(construct);
        self.scope_starts.pop();
        Some(destination.map(|local| self.local_expr(local, value.span)))
    }

    fn push_compute(&mut self, value: LirRvalue, ty: Type, span: Span, label: &str) -> LirLocalId {
        let name = self.synthetic_name(label);
        let local = self.declare_local(name, ty, None, true, false);
        self.push(SourceInst::Compute { local, value, span });
        local
    }

    fn lower_try_propagate(
        &mut self,
        expression: &HirExpr,
        destination: Option<LirLocalId>,
    ) -> Option<Option<HirExpr>> {
        let HirExprKind::TryPropagate { inner } = &expression.kind else {
            return None;
        };
        let resolved = builtin_types::resolve(&inner.ty)?;
        let returning = builtin_types::resolve(&self.return_type)?;
        if !matches!(resolved.id, B::Option | B::Result) || resolved.id != returning.id {
            return None;
        }
        let is_option = resolved.id == B::Option;
        let source_error = (!is_option)
            .then(|| resolved.args.get(1).cloned())
            .flatten();
        let target_error = (!is_option)
            .then(|| returning.args.get(1).cloned())
            .flatten();
        let name = self.synthetic_name("try_operand");
        let operand = self.declare_local(name, inner.ty.clone(), None, true, false);
        self.lower_value_into(operand, inner);
        let receiver = LirOperand::Local(operand);
        let condition = self.push_compute(
            LirRvalue::EnumMethod {
                receiver: receiver.clone(),
                receiver_ty: inner.ty.clone(),
                method: if is_option { "is_some" } else { "is_ok" }.into(),
                args: Vec::new(),
                arg_types: Vec::new(),
                result: Type::Bool,
            },
            Type::Bool,
            expression.span,
            "try_success",
        );
        let success = self.new_block();
        let failure = self.new_block();
        self.terminate(SourceTerminator::Branch {
            cond: self.local_expr(condition, expression.span),
            then_block: success,
            else_block: failure,
        });
        self.switch_to(failure);
        let return_type = self.return_type.clone();
        let failure_value = if is_option {
            self.push_compute(
                LirRvalue::EnumAlloc {
                    class: TypeId::local("Option"),
                    variant: "None".into(),
                    enum_ty: return_type.clone(),
                },
                return_type.clone(),
                expression.span,
                "try_none",
            )
        } else if let (Some(source_error @ Type::Named(_)), Some(target_error)) =
            (&source_error, &target_error)
        {
            if source_error != target_error && *target_error != Type::Void {
                let error = self.push_compute(
                    LirRvalue::EnumMethod {
                        receiver: receiver.clone(),
                        receiver_ty: inner.ty.clone(),
                        method: "unwrap_err".into(),
                        args: Vec::new(),
                        arg_types: Vec::new(),
                        result: source_error.clone(),
                    },
                    source_error.clone(),
                    expression.span,
                    "try_error",
                );
                let converted = self.push_compute(
                    LirRvalue::IntoError {
                        value: LirOperand::Local(error),
                        source: source_error.clone(),
                        target: target_error.clone(),
                    },
                    target_error.clone(),
                    expression.span,
                    "try_converted_error",
                );
                let result = self.push_compute(
                    LirRvalue::EnumAlloc {
                        class: TypeId::local("Result"),
                        variant: "Err".into(),
                        enum_ty: return_type.clone(),
                    },
                    return_type.clone(),
                    expression.span,
                    "try_failure",
                );
                self.push(SourceInst::Compute {
                    local: result,
                    value: LirRvalue::EnumPayloadStore {
                        object: LirOperand::Local(result),
                        class: TypeId::local("Result"),
                        variant: "Err".into(),
                        index: 0,
                        value: LirOperand::Local(converted),
                        source: target_error.clone(),
                        enum_ty: return_type.clone(),
                    },
                    span: expression.span,
                });
                result
            } else {
                self.push_compute(
                    LirRvalue::RebindResultError {
                        value: receiver.clone(),
                        source: inner.ty.clone(),
                        target: return_type.clone(),
                    },
                    return_type.clone(),
                    expression.span,
                    "try_failure",
                )
            }
        } else {
            self.push_compute(
                LirRvalue::RebindResultError {
                    value: receiver.clone(),
                    source: inner.ty.clone(),
                    target: return_type.clone(),
                },
                return_type.clone(),
                expression.span,
                "try_failure",
            )
        };
        self.lower_stmt(&HirStmt::Return {
            value: Some(self.local_expr(failure_value, expression.span)),
            span: expression.span,
        });
        self.switch_to(success);
        if let Some(local) = destination
            && expression.ty != Type::Void
        {
            self.push(SourceInst::Compute {
                local,
                value: LirRvalue::EnumMethod {
                    receiver,
                    receiver_ty: inner.ty.clone(),
                    method: "unwrap".into(),
                    args: Vec::new(),
                    arg_types: Vec::new(),
                    result: expression.ty.clone(),
                },
                span: expression.span,
            });
            return Some(Some(self.local_expr(local, expression.span)));
        }
        Some(None)
    }

    /// Lower one arm's body into the block already switched to.
    ///
    /// An arm that produces a value ends in an expression statement, and that
    /// statement is what writes the match's result; everything before it is an
    /// effect. An arm that produces nothing — a block arm, or one that
    /// `return`s or panics — is lowered as an ordinary scope, and the result
    /// keeps whatever the merge was seeded with.
    fn lower_match_arm_body(&mut self, body: &[HirStmt], destination: Option<LirLocalId>) {
        let value_tail = destination.and_then(|destination| match body.split_last() {
            // A `defer` anywhere in the body needs the scope brackets
            // `lower_scope` puts around it, so those arms take the plain path.
            Some((HirStmt::Expr(value), rest))
                if value.ty != Type::Never
                    && !body.iter().any(|s| matches!(s, HirStmt::Defer { .. })) =>
            {
                Some((destination, value, rest))
            }
            _ => None,
        });
        match value_tail {
            Some((destination, value, rest)) => {
                self.lower_stmts(rest);
                self.lower_value_into(destination, value);
            }
            None => self.lower_scope(body),
        }
    }

    /// Evaluate one operand into a local of its own, splitting the suspension it
    /// holds out of the expression that reads it (willow-0g8j.3).
    ///
    /// `None` when the suspension sits somewhere no split reaches; the operand
    /// then keeps its suspension and the enclosing statement is refused as a
    /// whole.
    fn lower_suspending_operand(&mut self, operand: &HirExpr) -> Option<HirExpr> {
        let name = self.synthetic_name("operand");
        let local = self.declare_local(name.clone(), operand.ty.clone(), None, true, false);
        if self.lower_root_suspend(operand, Some(local)).is_none() {
            let value = self.lower_nested_suspend(operand)?;
            self.push_existing_let(local, name, false, operand.ty.clone(), value, None);
        }
        Some(self.local_expr(local, operand.span))
    }

    /// An operand of a statement that is ITSELF about to park: anything it
    /// suspends on runs first, in its own split, so the park that follows holds
    /// no nested suspension (willow-0g8j.3). `done.send(work.recv())` parks on
    /// the recv and sends the value it resumed with.
    fn lower_operand_before_suspend(&mut self, operand: &HirExpr) -> Option<HirExpr> {
        if expression_needs_cfg(operand) {
            return self.lower_nested_cfg(operand);
        }
        if !suspends_anywhere(operand) {
            return Some(operand.clone());
        }
        self.lower_suspending_operand(operand)
    }

    /// Split a suspension out of an assignment's operands (willow-0g8j.3).
    ///
    /// `obj.field = await task;` and `xs[i] = await task;` park in the middle of
    /// a store, so their operands are ANF'd here the way a `let`'s initialiser
    /// is: the suspension becomes a [`SourceTerminator::Suspend`] of its own and the
    /// store reads the local the resume filled. Operands are evaluated left to
    /// right, so every operand before the parking one is frozen into a local
    /// first — a resume must not re-run `obj` or `i`.
    ///
    /// `None` when nothing suspends, or when a suspension sits somewhere this
    /// cannot hoist it out of. The caller then pushes the operands as written,
    /// and [`lir_async_rejection_reason`](crate::backend::cranelift) turns the
    /// remaining suspension into an AST fallback — which is also why the
    /// synthetic locals a partial split already pushed are harmless: the whole
    /// lowered function is discarded with it.
    fn lower_assign_operands(&mut self, operands: &[&HirExpr]) -> Option<Vec<HirExpr>> {
        let last = operands.iter().rposition(|operand| {
            expression_needs_cfg(operand) || (self.is_async && suspends_anywhere(operand))
        })?;
        let mut out = Vec::with_capacity(operands.len());
        for (index, operand) in operands.iter().enumerate() {
            let lowered = if index <= last
                && !matches!(
                    operand.kind,
                    HirExprKind::Int(_)
                        | HirExprKind::Float(_)
                        | HirExprKind::Bool(_)
                        | HirExprKind::Str(_)
                        | HirExprKind::FnRef(_)
                ) {
                let name = self.synthetic_name("operand");
                let local = self.declare_local(name, operand.ty.clone(), None, true, false);
                self.lower_value_into(local, operand);
                self.local_expr(local, operand.span)
            } else {
                (*operand).clone()
            };
            out.push(lowered);
        }
        Some(out)
    }

    /// Evaluate eager operands in order around nested source control flow.
    /// The worklist keeps eager expression depth off the native stack. Place
    /// operands retain their address identity; only their receiver/index values
    /// are materialized, never the referenced slot's contents.
    fn lower_nested_cfg(&mut self, value: &HirExpr) -> Option<HirExpr> {
        if !(expression_needs_cfg(value) || self.is_async && suspends_anywhere(value)) {
            return None;
        }
        enum Work<'a> {
            Eval(&'a HirExpr, bool, bool), // expression, place, root
            Finish(&'a HirExpr, usize, bool, bool),
            ArrayStore(LirLocalId, usize, Type, Span),
            FieldStore(LirLocalId, Type, String, Span),
            EnumStore(LirLocalId, TypeId, String, usize, Type, Span),
            Constructor(LirLocalId, &'a HirExpr),
            PrepareMethod(&'a HirExpr),
            MethodCall(&'a HirExpr, LirLocalId),
            IndirectCall(&'a HirExpr, LirLocalId, Vec<Type>, Type),
            DirectCall(&'a HirExpr, FunctionId, Vec<Type>, Type),
            StaticCall(&'a HirExpr),
            CaptureReference(&'a HirExpr, FunctionId, usize),
            FinishReference(&'a HirExpr, LirOperand),
            CoerceArgument(Type),
            FormatLiteral(LirLocalId, String, bool, Span),
            FormatValue(
                LirLocalId,
                Option<crate::interpolate::F64Format>,
                bool,
                Span,
            ),
        }
        let mut control_flow = std::collections::HashSet::new();
        for node in value.walk_postorder(false) {
            if matches!(
                node.kind,
                HirExprKind::TryPropagate { .. }
                    | HirExprKind::Ternary { .. }
                    | HirExprKind::Match { .. }
                    | HirExprKind::Binary {
                        op: crate::parser::ast::BinOp::And | crate::parser::ast::BinOp::Or,
                        ..
                    }
            ) || (self.is_async && expr_suspends_here(node))
                || (!matches!(node.kind, HirExprKind::Lambda { .. })
                    && node
                        .children()
                        .iter()
                        .any(|child| control_flow.contains(&std::ptr::from_ref(*child))))
            {
                control_flow.insert(std::ptr::from_ref(node));
            }
        }
        fn arguments<'a>(
            callee: FunctionId,
            args: &'a [HirExpr],
            params: &[Type],
        ) -> Vec<Work<'a>> {
            let mut pending = Vec::new();
            for (index, arg) in args.iter().enumerate() {
                if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) {
                    pending.push(Work::CaptureReference(arg, callee, index));
                } else {
                    pending.push(Work::Eval(arg, false, false));
                    if let Some(target) = params.get(index)
                        && *target != arg.ty
                    {
                        pending.push(Work::CoerceArgument(target.clone()));
                    }
                }
            }
            pending
        }
        let mut pending = vec![Work::Eval(value, false, true)];
        let mut values = Vec::new();
        let mut references: std::collections::HashMap<*const HirExpr, LirOperand> =
            std::collections::HashMap::new();
        while let Some(work) = pending.pop() {
            match work {
                Work::Eval(expr, place, root) => {
                    if !place && control_flow.contains(&std::ptr::from_ref(expr)) {
                        match &expr.kind {
                            HirExprKind::Call { callee, args }
                                if !self.local_by_name.contains_key(callee.unqualified_name())
                                    && self.resolution.functions.get(callee).is_some_and(
                                        |signature| {
                                            !signature.is_async
                                                && arguments_compatible(
                                                    &self.resolution,
                                                    &signature.params,
                                                    args,
                                                )
                                        },
                                    ) =>
                            {
                                let signature = self.resolution.functions[callee].clone();
                                if args
                                    .iter()
                                    .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
                                {
                                    self.push_compute(
                                        LirRvalue::BeginReferenceCall,
                                        Type::Void,
                                        expr.span,
                                        "reference_call",
                                    );
                                }
                                pending.push(Work::DirectCall(
                                    expr,
                                    *callee,
                                    signature.params.clone(),
                                    signature.return_type,
                                ));
                                pending.extend(
                                    arguments(*callee, args, &signature.params)
                                        .into_iter()
                                        .rev(),
                                );
                                continue;
                            }
                            HirExprKind::StaticCall {
                                class,
                                method,
                                args,
                            } if static_signature(&self.resolution, class, method).is_some_and(
                                |signature| {
                                    signature.is_static
                                        && arguments_compatible(
                                            &self.resolution,
                                            &signature.params,
                                            args,
                                        )
                                },
                            ) =>
                            {
                                if args
                                    .iter()
                                    .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
                                {
                                    self.push_compute(
                                        LirRvalue::BeginReferenceCall,
                                        Type::Void,
                                        expr.span,
                                        "reference_call",
                                    );
                                }
                                let signature = static_signature(&self.resolution, class, method)
                                    .expect("static signature");
                                pending.push(Work::StaticCall(expr));
                                pending.extend(
                                    arguments(
                                        FunctionId::method(*class, method),
                                        args,
                                        &signature.params,
                                    )
                                    .into_iter()
                                    .rev(),
                                );
                                continue;
                            }
                            HirExprKind::Call { callee, args }
                                if callee.is_free_named("format")
                                    && !self.local_by_name.contains_key("format")
                                    && format_segments(args).is_some() =>
                            {
                                let mut segments = format_segments(args).expect("validated format");
                                if segments.is_empty() {
                                    segments
                                        .push(crate::interpolate::Segment::Literal(String::new()));
                                }
                                let name = self.synthetic_name("format_result");
                                let local =
                                    self.declare_local(name, Type::String, None, true, false);
                                values.push(self.local_expr(local, expr.span));
                                let mut children = args[1..].iter();
                                let mut actions = Vec::new();
                                for (index, segment) in segments.into_iter().enumerate() {
                                    match segment {
                                        crate::interpolate::Segment::Literal(text) => actions.push(
                                            Work::FormatLiteral(local, text, index == 0, expr.span),
                                        ),
                                        segment => {
                                            let child = children.next().expect("format operand");
                                            let format = match segment {
                                                crate::interpolate::Segment::F64(format) => {
                                                    Some(format)
                                                }
                                                _ => None,
                                            };
                                            actions.push(Work::Eval(child, false, false));
                                            actions.push(Work::FormatValue(
                                                local,
                                                format,
                                                index == 0,
                                                child.span,
                                            ));
                                        }
                                    }
                                }
                                pending.extend(actions.into_iter().rev());
                                continue;
                            }
                            HirExprKind::Call { callee, args }
                                if self.local_by_name.contains_key(callee.unqualified_name()) =>
                            {
                                let source = self.local_by_name[callee.unqualified_name()];
                                let ty = self.locals[source.0 as usize].ty.clone();
                                if let Type::Fn(params, result) | Type::Closure(params, result) =
                                    &ty
                                    && !args.iter().any(|arg| {
                                        matches!(arg.kind, HirExprKind::ReferenceArg { .. })
                                    }) && arguments_compatible(&self.resolution, params, args)
                                    {
                                        let name = self.synthetic_name("callable_snapshot");
                                        let local =
                                            self.declare_local(name, ty.clone(), None, true, false);
                                        self.push(SourceInst::Compute {
                                            local,
                                            value: LirRvalue::Use(LirOperand::Local(source)),
                                            span: expr.span,
                                        });
                                        pending.push(Work::IndirectCall(
                                            expr,
                                            local,
                                            params.clone(),
                                            (**result).clone(),
                                        ));
                                        pending.extend(
                                            arguments(*callee, args, params).into_iter().rev(),
                                        );
                                        continue;
                                    }
                            }
                            HirExprKind::MethodCall {
                                object,
                                method,
                                args,
                            } if method_signature(&self.resolution, &object.ty, method)
                                .is_some_and(|signature| {
                                    (!signature.is_static || matches!(&object.ty, Type::Named(name) | Type::Generic(name, _) if self.resolution.interfaces.contains_key(name)))
                                        && arguments_compatible(
                                            &self.resolution,
                                            &signature.params,
                                            args,
                                        )
                                }) =>
                            {
                                pending.push(Work::PrepareMethod(expr));
                                pending.push(Work::Eval(object, false, false));
                                continue;
                            }
                            HirExprKind::Array { elements } => {
                                let Type::Array(element) = &expr.ty else {
                                    return None;
                                };
                                let name = self.synthetic_name("array_shell");
                                let local =
                                    self.declare_local(name, expr.ty.clone(), None, true, false);
                                self.push(SourceInst::Compute {
                                    local,
                                    value: LirRvalue::ArrayAlloc {
                                        length: elements.len(),
                                        element: (**element).clone(),
                                    },
                                    span: expr.span,
                                });
                                values.push(self.local_expr(local, expr.span));
                                for (index, child) in elements.iter().enumerate().rev() {
                                    pending.push(Work::ArrayStore(
                                        local,
                                        index,
                                        (**element).clone(),
                                        child.span,
                                    ));
                                    pending.push(Work::Eval(child, false, false));
                                }
                                continue;
                            }
                            HirExprKind::StaticCall {
                                class,
                                method,
                                args,
                            } if self.resolution.enums.get(class).is_some_and(|info| {
                                info.variants.iter().any(|variant| {
                                    variant.name == *method && variant.payloads.len() == args.len()
                                })
                            }) =>
                            {
                                let name = self.synthetic_name("enum_shell");
                                let local =
                                    self.declare_local(name, expr.ty.clone(), None, true, false);
                                self.push(SourceInst::Compute {
                                    local,
                                    value: LirRvalue::EnumAlloc {
                                        class: *class,
                                        variant: method.clone(),
                                        enum_ty: expr.ty.clone(),
                                    },
                                    span: expr.span,
                                });
                                values.push(self.local_expr(local, expr.span));
                                for (index, child) in args.iter().enumerate().rev() {
                                    pending.push(Work::EnumStore(
                                        local,
                                        *class,
                                        method.clone(),
                                        index,
                                        expr.ty.clone(),
                                        child.span,
                                    ));
                                    pending.push(Work::Eval(child, false, false));
                                }
                                continue;
                            }
                            HirExprKind::ObjectLiteral { class, fields }
                                if class_fields(&self.resolution, class).is_some_and(
                                    |declared| {
                                        declared.len() == fields.len()
                                            && declared.iter().all(|(name, _)| {
                                                fields
                                                    .iter()
                                                    .filter(|(field, _)| field == name)
                                                    .count()
                                                    == 1
                                            })
                                    },
                                ) =>
                            {
                                let name = self.synthetic_name("object_shell");
                                let local =
                                    self.declare_local(name, expr.ty.clone(), None, true, false);
                                self.push(SourceInst::Compute {
                                    local,
                                    value: LirRvalue::ObjectAlloc { class: *class },
                                    span: expr.span,
                                });
                                values.push(self.local_expr(local, expr.span));
                                for (field, child) in fields.iter().rev() {
                                    pending.push(Work::FieldStore(
                                        local,
                                        expr.ty.clone(),
                                        field.clone(),
                                        child.span,
                                    ));
                                    pending.push(Work::Eval(child, false, false));
                                }
                                continue;
                            }
                            HirExprKind::New { class, args }
                                if self.resolution.classes.contains_key(class) =>
                            {
                                let info = self.resolution.classes[class].clone();
                                if info.constructor.as_ref().is_some_and(|signature| {
                                    !arguments_compatible(&self.resolution, &signature.params, args)
                                }) {
                                    return None;
                                }
                                let fields = if info.constructor.is_none() {
                                    class_fields(&self.resolution, class)
                                } else {
                                    Some(Vec::new())
                                }?;
                                if info.constructor.is_none() && fields.len() != args.len() {
                                    return None;
                                }
                                let name = self.synthetic_name("object_shell");
                                let local =
                                    self.declare_local(name, expr.ty.clone(), None, true, false);
                                self.push(SourceInst::Compute {
                                    local,
                                    value: LirRvalue::ObjectAlloc { class: *class },
                                    span: expr.span,
                                });
                                values.push(self.local_expr(local, expr.span));
                                if info.constructor.is_some() {
                                    if args.iter().any(|arg| {
                                        matches!(arg.kind, HirExprKind::ReferenceArg { .. })
                                    }) {
                                        self.push_compute(
                                            LirRvalue::BeginReferenceCall,
                                            Type::Void,
                                            expr.span,
                                            "reference_call",
                                        );
                                    }
                                    pending.push(Work::Constructor(local, expr));
                                    pending.extend(
                                        arguments(
                                            FunctionId::method(*class, "init"),
                                            args,
                                            &info
                                                .constructor
                                                .as_ref()
                                                .expect("explicit constructor")
                                                .params,
                                        )
                                        .into_iter()
                                        .rev(),
                                    );
                                } else {
                                    for ((field, _), child) in fields.into_iter().zip(args).rev() {
                                        pending.push(Work::FieldStore(
                                            local,
                                            expr.ty.clone(),
                                            field,
                                            child.span,
                                        ));
                                        pending.push(Work::Eval(child, false, false));
                                    }
                                }
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if matches!(
                        expr.kind,
                        HirExprKind::TryPropagate { .. }
                            | HirExprKind::Ternary { .. }
                            | HirExprKind::Match { .. }
                            | HirExprKind::Binary {
                                op: crate::parser::ast::BinOp::And | crate::parser::ast::BinOp::Or,
                                ..
                            }
                    ) || (self.is_async && expr_suspends_here(expr))
                    {
                        let name = self.synthetic_name("expression");
                        let result = self.declare_local(name, expr.ty.clone(), None, true, false);
                        self.lower_root_suspend(expr, Some(result))?;
                        values.push(self.local_expr(result, expr.span));
                    } else if matches!(
                        expr.kind,
                        HirExprKind::Lambda { .. } | HirExprKind::Select { .. }
                    ) || !(control_flow.contains(&std::ptr::from_ref(expr))
                        || matches!(expr.kind, HirExprKind::ReferenceArg { .. })
                        || (place
                            && matches!(
                                expr.kind,
                                HirExprKind::Index { .. } | HirExprKind::FieldAccess { .. }
                            )))
                    {
                        if root
                            || place
                            || matches!(
                                expr.kind,
                                HirExprKind::Int(_)
                                    | HirExprKind::Float(_)
                                    | HirExprKind::Bool(_)
                                    | HirExprKind::Str(_)
                                    | HirExprKind::FnRef(_)
                                    | HirExprKind::ReferenceArg { .. }
                            )
                        {
                            values.push(expr.clone());
                        } else {
                            let name = self.synthetic_name("expression_operand");
                            let local =
                                self.declare_local(name, expr.ty.clone(), None, true, false);
                            self.lower_value_into(local, expr);
                            values.push(self.local_expr(local, expr.span));
                        }
                    } else {
                        let operands = expr.children();
                        pending.push(Work::Finish(expr, operands.len(), place, root));
                        let reference = matches!(expr.kind, HirExprKind::ReferenceArg { .. });
                        pending.extend(
                            operands
                                .into_iter()
                                .rev()
                                .map(|operand| Work::Eval(operand, reference, false)),
                        );
                    }
                }
                Work::CoerceArgument(target) => {
                    let source = values.pop().expect("argument value");
                    let value = self.flat_operand(&source);
                    let local = self.push_compute(
                        LirRvalue::Coerce {
                            value,
                            source: source.ty.clone(),
                            target: target.clone(),
                        },
                        target,
                        source.span,
                        "coerced_argument",
                    );
                    values.push(self.local_expr(local, source.span));
                }
                Work::CaptureReference(expr, callee, index) => {
                    let HirExprKind::ReferenceArg { place } = &expr.kind else {
                        unreachable!()
                    };
                    let captured = match &place.kind {
                        HirExprKind::Var(name) => LirPlace::Local(*self.local_by_name.get(name)?),
                        HirExprKind::FieldAccess { object, field } => {
                            let name = self.synthetic_name("reference_object");
                            LirPlace::Field {
                                object: self.declare_local(
                                    name,
                                    object.ty.clone(),
                                    None,
                                    true,
                                    false,
                                ),
                                object_ty: object.ty.clone(),
                                field: field.clone(),
                                ty: place.ty.clone(),
                            }
                        }
                        HirExprKind::Index { .. } => {
                            let name = self.synthetic_name("reference_owner");
                            let owner = self.declare_local(name, Type::Void, None, true, false);
                            self.locals[owner.0 as usize].storage_kind = LirStorageKind::GcOwner;
                            let name = self.synthetic_name("reference_index");
                            LirPlace::ArrayElement {
                                owner,
                                index: self.declare_local(name, Type::I64, None, true, false),
                                element: place.ty.clone(),
                            }
                        }
                        _ => return None,
                    };
                    let argument = LirOperand::Reference {
                        place: captured,
                        span: expr.span,
                        display: reference_place_name(place),
                    };
                    self.push_compute(
                        LirRvalue::ReferenceDebug {
                            argument: argument.clone(),
                            callee,
                            index,
                        },
                        Type::Void,
                        expr.span,
                        "reference_debug",
                    );
                    pending.push(Work::FinishReference(expr, argument));
                    match &place.kind {
                        HirExprKind::FieldAccess { object, .. } => {
                            pending.push(Work::Eval(object, false, false))
                        }
                        HirExprKind::Index { array, index } => {
                            pending.push(Work::Eval(index, false, false));
                            pending.push(Work::Eval(array, false, false));
                        }
                        _ => {}
                    }
                }
                Work::FinishReference(expr, argument) => {
                    let HirExprKind::ReferenceArg { place } = &expr.kind else {
                        unreachable!()
                    };
                    let LirOperand::Reference {
                        place: captured, ..
                    } = &argument
                    else {
                        unreachable!()
                    };
                    match (&place.kind, captured) {
                        (HirExprKind::Var(_), LirPlace::Local(_)) => {}
                        (HirExprKind::FieldAccess { .. }, LirPlace::Field { object, .. }) => {
                            let child = values.pop().expect("reference receiver");
                            let value = self.flat_operand(&child);
                            self.push(SourceInst::Compute {
                                local: *object,
                                value: LirRvalue::Use(value),
                                span: place.span,
                            });
                        }
                        (
                            HirExprKind::Index { .. },
                            LirPlace::ArrayElement { owner, index, .. },
                        ) => {
                            let index_expr = values.pop().expect("reference index");
                            let array_expr = values.pop().expect("reference array");
                            let index_value = self.flat_operand(&index_expr);
                            let array_value = self.flat_operand(&array_expr);
                            self.push(SourceInst::Compute {
                                local: *index,
                                value: LirRvalue::Use(index_value.clone()),
                                span: place.span,
                            });
                            self.push(SourceInst::Compute {
                                local: *owner,
                                value: LirRvalue::CaptureArrayOwner {
                                    array: array_value,
                                    index: LirOperand::Local(*index),
                                },
                                span: place.span,
                            });
                        }
                        _ => unreachable!(),
                    }
                    references.insert(std::ptr::from_ref(expr), argument);
                    values.push(expr.clone());
                }
                Work::DirectCall(expr, callee, params, result) => {
                    let HirExprKind::Call { args, .. } = &expr.kind else {
                        unreachable!()
                    };
                    let children = values.split_off(values.len() - args.len());
                    let operands = args
                        .iter()
                        .zip(&children)
                        .map(|(original, child)| {
                            references
                                .remove(&std::ptr::from_ref(original))
                                .unwrap_or_else(|| self.flat_operand(child))
                        })
                        .collect();
                    let local = self.push_compute(
                        LirRvalue::DirectCall {
                            callee,
                            args: operands,
                            params,
                            result: result.clone(),
                        },
                        result,
                        expr.span,
                        "call_result",
                    );
                    values.push(self.local_expr(local, expr.span));
                }
                Work::StaticCall(expr) => {
                    let HirExprKind::StaticCall {
                        class,
                        method,
                        args,
                    } = &expr.kind
                    else {
                        unreachable!()
                    };
                    let children = values.split_off(values.len() - args.len());
                    let operands = args
                        .iter()
                        .zip(&children)
                        .map(|(original, child)| {
                            references
                                .remove(&std::ptr::from_ref(original))
                                .unwrap_or_else(|| self.flat_operand(child))
                        })
                        .collect();
                    let local = self.push_compute(
                        LirRvalue::StaticCall {
                            class: *class,
                            method: method.clone(),
                            args: operands,
                            arg_types: children.iter().map(|arg| arg.ty.clone()).collect(),
                            result: expr.ty.clone(),
                        },
                        expr.ty.clone(),
                        expr.span,
                        "static_result",
                    );
                    values.push(self.local_expr(local, expr.span));
                }
                Work::FormatLiteral(local, text, first, span) => {
                    let name = self.synthetic_name("format_literal");
                    let piece = self.declare_local(name, Type::String, None, true, false);
                    self.push(SourceInst::Compute {
                        local: piece,
                        value: LirRvalue::StringLiteral(text),
                        span,
                    });
                    self.append_format_piece(local, LirOperand::Local(piece), first, span);
                }
                Work::FormatValue(local, format, first, span) => {
                    let child = values.pop().expect("format operand");
                    let value = self.flat_operand(&child);
                    let name = self.synthetic_name("format_piece");
                    let piece = self.declare_local(name, Type::String, None, true, false);
                    self.push(SourceInst::Compute {
                        local: piece,
                        value: LirRvalue::FormatScalar {
                            value,
                            ty: child.ty.clone(),
                            format,
                        },
                        span,
                    });
                    self.append_format_piece(local, LirOperand::Local(piece), first, span);
                }
                Work::IndirectCall(expr, callee, params, result) => {
                    let HirExprKind::Call { callee: name, args } = &expr.kind else {
                        unreachable!()
                    };
                    let children = values.split_off(values.len() - args.len());
                    let args = children
                        .iter()
                        .map(|child| self.flat_operand(child))
                        .collect();
                    let local_name = self.synthetic_name("indirect_result");
                    let local = self.declare_local(local_name, result.clone(), None, true, false);
                    self.push(SourceInst::Compute {
                        local,
                        value: LirRvalue::IndirectCall {
                            callee: LirOperand::Local(callee),
                            name: *name,
                            args,
                            params,
                            result,
                        },
                        span: expr.span,
                    });
                    values.push(self.local_expr(local, expr.span));
                }
                Work::PrepareMethod(expr) => {
                    let HirExprKind::MethodCall {
                        object,
                        method,
                        args,
                    } = &expr.kind
                    else {
                        unreachable!()
                    };
                    let receiver = values.pop().expect("method receiver");
                    let receiver = self.flat_operand(&receiver);
                    let name = self.synthetic_name("method_receiver");
                    let local = self.declare_local(name, object.ty.clone(), None, true, false);
                    self.push(SourceInst::Compute {
                        local,
                        value: LirRvalue::PrepareMethod {
                            receiver,
                            receiver_ty: object.ty.clone(),
                            method: method.clone(),
                        },
                        span: expr.span,
                    });
                    if args
                        .iter()
                        .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
                    {
                        self.push_compute(
                            LirRvalue::BeginReferenceCall,
                            Type::Void,
                            expr.span,
                            "reference_call",
                        );
                    }
                    pending.push(Work::MethodCall(expr, local));
                    let (Type::Named(owner) | Type::Generic(owner, _)) = &object.ty else {
                        unreachable!()
                    };
                    pending.extend(
                        arguments(
                            FunctionId::method(*owner, method),
                            args,
                            &method_signature(&self.resolution, &object.ty, method)
                                .expect("method signature")
                                .params,
                        )
                        .into_iter()
                        .rev(),
                    );
                }
                Work::MethodCall(expr, receiver) => {
                    let HirExprKind::MethodCall {
                        object,
                        method,
                        args,
                    } = &expr.kind
                    else {
                        unreachable!()
                    };
                    let children = values.split_off(values.len() - args.len());
                    let args = args
                        .iter()
                        .zip(&children)
                        .map(|(original, child)| {
                            references
                                .remove(&std::ptr::from_ref(original))
                                .unwrap_or_else(|| self.flat_operand(child))
                        })
                        .collect();
                    let arg_types = children.iter().map(|child| child.ty.clone()).collect();
                    let name = self.synthetic_name("method_result");
                    let local = self.declare_local(name, expr.ty.clone(), None, true, false);
                    self.push(SourceInst::Compute {
                        local,
                        value: LirRvalue::MethodCall {
                            receiver: LirOperand::Local(receiver),
                            receiver_ty: object.ty.clone(),
                            method: method.clone(),
                            args,
                            arg_types,
                            result: expr.ty.clone(),
                        },
                        span: expr.span,
                    });
                    values.push(self.local_expr(local, expr.span));
                }
                Work::ArrayStore(array, index, element, span) => {
                    let child = values.pop().expect("array element");
                    let value = self.flat_operand(&child);
                    let name = self.synthetic_name("array_store");
                    let local = self.declare_local(name, Type::Void, None, true, false);
                    self.push(SourceInst::Compute {
                        local,
                        value: LirRvalue::ArrayStore {
                            array: LirOperand::Local(array),
                            index: LirOperand::Int(index as i64),
                            value,
                            element,
                        },
                        span,
                    });
                }
                Work::FieldStore(object, object_ty, field, span) => {
                    let child = values.pop().expect("field value");
                    let value = self.flat_operand(&child);
                    let name = self.synthetic_name("field_store");
                    let local = self.declare_local(name, Type::Void, None, true, false);
                    self.push(SourceInst::Compute {
                        local,
                        value: LirRvalue::FieldStore {
                            object: LirOperand::Local(object),
                            object_ty,
                            field,
                            value,
                        },
                        span,
                    });
                }
                Work::EnumStore(local, class, variant, index, enum_ty, span) => {
                    let child = values.pop().expect("enum payload");
                    let value = self.flat_operand(&child);
                    self.push(SourceInst::Compute {
                        local,
                        value: LirRvalue::EnumPayloadStore {
                            object: LirOperand::Local(local),
                            class,
                            variant,
                            index,
                            value,
                            source: child.ty.clone(),
                            enum_ty,
                        },
                        span,
                    });
                }
                Work::Constructor(object, expr) => {
                    let HirExprKind::New { class, args } = &expr.kind else {
                        unreachable!()
                    };
                    let children = values.split_off(values.len() - args.len());
                    let operands = args
                        .iter()
                        .zip(&children)
                        .map(|(original, child)| {
                            references
                                .remove(&std::ptr::from_ref(original))
                                .unwrap_or_else(|| self.flat_operand(child))
                        })
                        .collect();
                    let arg_types = children.iter().map(|child| child.ty.clone()).collect();
                    self.push_compute(
                        LirRvalue::ConstructorCall {
                            object: LirOperand::Local(object),
                            class: *class,
                            args: operands,
                            arg_types,
                        },
                        Type::Void,
                        expr.span,
                        "constructor_call",
                    );
                }
                Work::Finish(expr, count, place, root) => {
                    let operands = values.split_off(values.len() - count);
                    let lowered = expr.with_eager_operands(operands);
                    if root || place || matches!(lowered.kind, HirExprKind::ReferenceArg { .. }) {
                        values.push(lowered);
                    } else {
                        let name = self.synthetic_name("expression_operand");
                        let local = self.declare_local(name, lowered.ty.clone(), None, true, false);
                        self.lower_value_into(local, &lowered);
                        values.push(self.local_expr(local, expr.span));
                    }
                }
            }
        }
        values.pop()
    }

    fn append_format_piece(
        &mut self,
        local: LirLocalId,
        piece: LirOperand,
        first: bool,
        span: Span,
    ) {
        let value = if first {
            LirRvalue::Use(piece)
        } else {
            LirRvalue::Binary {
                op: crate::parser::ast::BinOp::Add,
                lhs: LirOperand::Local(local),
                rhs: piece,
                operand_ty: Type::String,
            }
        };
        self.push(SourceInst::Compute { local, value, span });
    }

    fn flat_operand(&mut self, value: &HirExpr) -> LirOperand {
        match &value.kind {
            HirExprKind::Int(value) => LirOperand::Int(*value),
            HirExprKind::Float(value) => LirOperand::Float(*value),
            HirExprKind::Bool(value) => LirOperand::Bool(*value),
            HirExprKind::Var(name) if self.local_by_name.contains_key(name) => {
                LirOperand::Local(self.local_by_name[name])
            }
            _ => {
                let name = self.synthetic_name("operand");
                let local = self.declare_local(name, value.ty.clone(), None, true, false);
                self.lower_value_into(local, value);
                LirOperand::Local(local)
            }
        }
    }

    fn lower_nested_suspend(&mut self, value: &HirExpr) -> Option<HirExpr> {
        if expression_needs_cfg(value)
            || (self.is_async && suspends_anywhere(value) && !expr_suspends_here(value))
        {
            return self.lower_nested_cfg(value);
        }
        if !self.is_async || expr_suspends_here(value) {
            return None;
        }
        if let Some(value) = self.lower_conditional_suspend(value) {
            return Some(value);
        }
        let mut lowered = value.clone();
        let mut changed = false;
        loop {
            let mut suspensions = Vec::new();
            collect_suspensions(&lowered, &mut suspensions);
            let Some(target) = suspensions.first() else {
                return changed.then_some(lowered);
            };
            if target.ty == Type::Void {
                return None;
            }
            let mut seen = false;
            let initially_hoistable = hoistable_around(&lowered, target, &mut seen);
            let target = (*target).clone();
            if !initially_hoistable {
                lowered = self.freeze_binary_prefix(&lowered, target.span)?;
                let mut prepared_suspensions = Vec::new();
                collect_suspensions(&lowered, &mut prepared_suspensions);
                let prepared_target = prepared_suspensions.first()?;
                let mut seen = false;
                if !hoistable_around(&lowered, prepared_target, &mut seen) {
                    return None;
                }
            }
            let name = self.synthetic_name("result");
            let result = self.declare_local(name, target.ty.clone(), None, true, false);
            self.lower_root_suspend(&target, Some(result))?;
            let replacement = self.local_expr(result, target.span);
            lowered = replace_suspension(&lowered, target.span, &replacement);
            changed = true;
        }
    }

    /// Preserve a non-repeatable left operand before parking on a suspension
    /// in the right operand. This is the minimal ANF step needed for expressions
    /// such as `count() + await task`: `count()` runs once, before the park.
    fn freeze_binary_prefix(&mut self, value: &HirExpr, target_span: Span) -> Option<HirExpr> {
        let target = value
            .walk_postorder(true)
            .find(|node| node.span == target_span && expr_suspends_here(node))?;
        let ancestors: std::collections::HashSet<*const HirExpr> = value
            .path_to(target)?
            .into_iter()
            .map(std::ptr::from_ref)
            .collect();
        let mut out = value.clone();
        let mut source = value;
        let mut current = &mut out;
        let mut can_fall_back = false;
        loop {
            match &source.kind {
                HirExprKind::Print { value: child, .. } => {
                    let HirExprKind::Print { value, .. } = &mut current.kind else {
                        unreachable!()
                    };
                    source = child;
                    current = value;
                }
                HirExprKind::Binary { lhs, rhs, .. } => {
                    let HirExprKind::Binary {
                        lhs: target_lhs,
                        rhs: target_rhs,
                        ..
                    } = &mut current.kind
                    else {
                        unreachable!()
                    };
                    if ancestors.contains(&std::ptr::from_ref(&**lhs)) {
                        source = lhs;
                        current = target_lhs;
                        continue;
                    }
                    if !ancestors.contains(&std::ptr::from_ref(&**rhs)) {
                        if can_fall_back {
                            break;
                        } else {
                            return None;
                        }
                    }
                    if !rematerializable(lhs) {
                        let name = self.synthetic_name("prefix");
                        let old = std::mem::replace(
                            &mut **target_lhs,
                            HirExpr {
                                kind: HirExprKind::Int(0),
                                ty: Type::Void,
                                span: lhs.span,
                            },
                        );
                        let local = self.push_synth_let(&name, false, old);
                        **target_lhs = self.local_expr(local, lhs.span);
                    }
                    can_fall_back = true;
                    if rhs.span == target_span && expr_suspends_here(rhs) {
                        break;
                    }
                    source = rhs;
                    current = target_rhs;
                }
                _ if can_fall_back => break,
                _ => return None,
            }
        }
        Some(out)
    }

    fn lower_conditional_suspend(&mut self, value: &HirExpr) -> Option<HirExpr> {
        let (condition, then_expr, else_expr) = match &value.kind {
            HirExprKind::Ternary {
                condition,
                then_expr,
                else_expr,
            } => (&**condition, &**then_expr, &**else_expr),
            HirExprKind::Binary { op, lhs, rhs }
                if matches!(
                    op,
                    crate::parser::ast::BinOp::And | crate::parser::ast::BinOp::Or
                ) =>
            {
                let constant = HirExpr {
                    kind: HirExprKind::Bool(matches!(op, crate::parser::ast::BinOp::Or)),
                    ty: Type::Bool,
                    span: value.span,
                };
                if matches!(op, crate::parser::ast::BinOp::And) {
                    return self.lower_conditional_branches(value, lhs, rhs, &constant);
                }
                return self.lower_conditional_branches(value, lhs, &constant, rhs);
            }
            _ => return None,
        };
        self.lower_conditional_branches(value, condition, then_expr, else_expr)
    }

    fn lower_conditional_branches(
        &mut self,
        whole: &HirExpr,
        condition: &HirExpr,
        then_expr: &HirExpr,
        else_expr: &HirExpr,
    ) -> Option<HirExpr> {
        let condition = self.lower_condition(condition);
        let result_name = self.synthetic_name("conditional");
        let result = self.declare_local(result_name, whole.ty.clone(), None, true, false);
        let then_block = self.new_block();
        let else_block = self.new_block();
        let merge = self.new_block();
        self.terminate(SourceTerminator::Branch {
            cond: condition,
            then_block,
            else_block,
        });

        self.switch_to(then_block);
        self.lower_value_into(result, then_expr);
        self.terminate(SourceTerminator::Jump(merge));

        self.switch_to(else_block);
        self.lower_value_into(result, else_expr);
        self.terminate(SourceTerminator::Jump(merge));

        self.switch_to(merge);
        Some(self.local_expr(result, whole.span))
    }

    fn lower_value_into(&mut self, destination: LirLocalId, value: &HirExpr) {
        if self.lower_root_suspend(value, Some(destination)).is_some() {
            return;
        }
        let value = self
            .lower_nested_suspend(value)
            .unwrap_or_else(|| value.clone());
        let local = &self.locals[destination.0 as usize];
        self.push(SourceInst::Assign {
            local: destination,
            name: local.name.clone(),
            value,
        });
    }

    fn lower_condition(&mut self, condition: &HirExpr) -> HirExpr {
        let name = self.synthetic_name("condition");
        let local = self.declare_local(name, Type::Bool, None, true, false);
        match self.lower_root_suspend(condition, Some(local)) {
            Some(Some(value)) => value,
            _ => self
                .lower_nested_suspend(condition)
                .unwrap_or_else(|| condition.clone()),
        }
    }

    /// Capture a reference at its source evaluation point. Array references retain
    /// the backing buffer, so later arguments may resize the array safely.
    fn lower_reference_argument(
        &mut self,
        expr: &HirExpr,
        callee: FunctionId,
        index: usize,
    ) -> Option<LirOperand> {
        let HirExprKind::ReferenceArg { place } = &expr.kind else {
            return None;
        };
        let captured = match &place.kind {
            HirExprKind::Var(name) => LirPlace::Local(*self.local_by_name.get(name)?),
            HirExprKind::FieldAccess { object, field } => {
                let name = self.synthetic_name("reference_object");
                LirPlace::Field {
                    object: self.declare_local(name, object.ty.clone(), None, true, false),
                    object_ty: object.ty.clone(),
                    field: field.clone(),
                    ty: place.ty.clone(),
                }
            }
            HirExprKind::Index { .. } => {
                let name = self.synthetic_name("reference_owner");
                let owner = self.declare_local(name, Type::Void, None, true, false);
                self.locals[owner.0 as usize].storage_kind = LirStorageKind::GcOwner;
                let name = self.synthetic_name("reference_index");
                LirPlace::ArrayElement {
                    owner,
                    index: self.declare_local(name, Type::I64, None, true, false),
                    element: place.ty.clone(),
                }
            }
            _ => return None,
        };
        let argument = LirOperand::Reference {
            place: captured.clone(),
            span: expr.span,
            display: reference_place_name(place),
        };
        self.push_compute(
            LirRvalue::ReferenceDebug {
                argument: argument.clone(),
                callee,
                index,
            },
            Type::Void,
            expr.span,
            "reference_debug",
        );
        match (&place.kind, captured) {
            (
                HirExprKind::FieldAccess { object, .. },
                LirPlace::Field {
                    object: destination,
                    ..
                },
            ) => self.lower_value_into(destination, object),
            (
                HirExprKind::Index { array, index },
                LirPlace::ArrayElement {
                    owner,
                    index: saved_index,
                    ..
                },
            ) => {
                let name = self.synthetic_name("reference_array");
                let array = self.push_synth_let(&name, false, (**array).clone());
                self.lower_value_into(saved_index, index);
                self.push(SourceInst::Compute {
                    local: owner,
                    value: LirRvalue::CaptureArrayOwner {
                        array: LirOperand::Local(array),
                        index: LirOperand::Local(saved_index),
                    },
                    span: place.span,
                });
            }
            (HirExprKind::Var(_), LirPlace::Local(_)) => {}
            _ => unreachable!(),
        }
        Some(argument)
    }

    fn lower_super_init_values(&mut self, args: &[HirExpr], span: Span) -> bool {
        let Some(&receiver) = self.local_by_name.get("self") else {
            return false;
        };
        let receiver_ty = self.locals[receiver.0 as usize].ty.clone();
        let Type::Named(owner) = &receiver_ty else {
            return false;
        };
        let Some(base) = self
            .resolution
            .classes
            .get(owner)
            .and_then(|info| info.base)
        else {
            return false;
        };
        let Some(info) = self.resolution.classes.get(&base).cloned() else {
            return false;
        };
        let explicit = info.constructor.is_some();
        let mut fields = Vec::new();
        let params = if let Some(signature) = &info.constructor {
            signature.params.clone()
        } else {
            let Some(inherited) = class_fields(&self.resolution, &base) else {
                return false;
            };
            fields = inherited;
            fields.iter().map(|(_, ty)| ty.clone()).collect()
        };
        if !arguments_compatible(&self.resolution, &params, args) {
            return false;
        }
        let name = self.synthetic_name("base_receiver");
        let base_ty = Type::Named(base);
        let base_local = self.declare_local(name, base_ty.clone(), None, true, false);
        self.push(SourceInst::Compute {
            local: base_local,
            value: LirRvalue::Coerce {
                value: LirOperand::Local(receiver),
                source: receiver_ty,
                target: base_ty.clone(),
            },
            span,
        });
        let mut operands = Vec::new();
        if args
            .iter()
            .any(|arg| matches!(arg.kind, HirExprKind::ReferenceArg { .. }))
        {
            self.push_compute(
                LirRvalue::BeginReferenceCall,
                Type::Void,
                span,
                "reference_call",
            );
        }
        for (index, (arg, target)) in args.iter().zip(&params).enumerate() {
            if matches!(arg.kind, HirExprKind::ReferenceArg { .. }) {
                let Some(reference) =
                    self.lower_reference_argument(arg, FunctionId::method(base, "init"), index)
                else {
                    return false;
                };
                operands.push(reference);
                continue;
            }
            let value = self
                .lower_nested_suspend(arg)
                .unwrap_or_else(|| arg.clone());
            let name = self.synthetic_name("base_argument");
            let source = self.push_synth_let(&name, false, value);
            let name = self.synthetic_name("base_coerced");
            let coerced = self.declare_local(name, target.clone(), None, true, false);
            self.push(SourceInst::Compute {
                local: coerced,
                value: LirRvalue::Coerce {
                    value: LirOperand::Local(source),
                    source: arg.ty.clone(),
                    target: target.clone(),
                },
                span: arg.span,
            });
            if explicit {
                operands.push(LirOperand::Local(coerced));
            } else {
                let name = self.synthetic_name("base_store");
                let local = self.declare_local(name, Type::Void, None, true, false);
                self.push(SourceInst::Compute {
                    local,
                    value: LirRvalue::FieldStore {
                        object: LirOperand::Local(base_local),
                        object_ty: base_ty.clone(),
                        field: fields[index].0.clone(),
                        value: LirOperand::Local(coerced),
                    },
                    span: arg.span,
                });
            }
        }
        if explicit {
            let name = self.synthetic_name("base_init");
            let local = self.declare_local(name, Type::Void, None, true, false);
            self.push(SourceInst::Compute {
                local,
                value: LirRvalue::ConstructorCall {
                    object: LirOperand::Local(base_local),
                    class: base,
                    args: operands,
                    arg_types: params,
                },
                span,
            });
        }
        true
    }

    fn lower_select(&mut self, cases: &[super::typed_ast::HirSelectCase], span: Span) {
        use super::typed_ast::HirSelectCaseKind;

        let mut operations = Vec::with_capacity(cases.len());
        for case in cases {
            let operation = match &case.kind {
                HirSelectCaseKind::Recv { binding, channel } => {
                    let name = self.synthetic_name("select_channel");
                    let channel_local = self.push_synth_let(&name, false, channel.clone());
                    let elem_ty = builtin_types::unary_arg(&channel.ty, B::Channel)
                        .expect("select recv channel was type checked")
                        .clone();
                    let binding = (binding != "_").then(|| {
                        self.declare_local(
                            binding.clone(),
                            elem_ty.clone(),
                            Some(case.span),
                            false,
                            false,
                        )
                    });
                    LirSelectOp::Recv {
                        channel: channel_local,
                        binding,
                        elem_ty,
                    }
                }
                HirSelectCaseKind::Send { channel, value } => {
                    let channel_name = self.synthetic_name("select_channel");
                    let channel_local = self.push_synth_let(&channel_name, false, channel.clone());
                    let value_name = self.synthetic_name("select_value");
                    let value_local = self.push_synth_let(&value_name, false, value.clone());
                    let elem_ty = builtin_types::unary_arg(&channel.ty, B::Channel)
                        .expect("select send channel was type checked")
                        .clone();
                    let value_local = if self.is_async {
                        value_local
                    } else {
                        let name = self.synthetic_name("select_stored_value");
                        let stored = self.declare_local(name, elem_ty.clone(), None, true, false);
                        self.push(SourceInst::Compute {
                            local: stored,
                            value: LirRvalue::Coerce {
                                value: LirOperand::Local(value_local),
                                source: value.ty.clone(),
                                target: elem_ty.clone(),
                            },
                            span: value.span,
                        });
                        stored
                    };
                    LirSelectOp::Send {
                        channel: channel_local,
                        value: value_local,
                        elem_ty,
                    }
                }
                HirSelectCaseKind::Timeout { millis } => {
                    let millis_name = self.synthetic_name("select_millis");
                    let millis_local = self.push_synth_let(&millis_name, false, millis.clone());
                    let deadline_name = self.synthetic_name("select_deadline");
                    let deadline = self.declare_local(deadline_name, Type::I64, None, true, false);
                    LirSelectOp::Timeout {
                        millis: millis_local,
                        deadline,
                    }
                }
                HirSelectCaseKind::Join { binding, task } => {
                    let task_name = self.synthetic_name("select_task");
                    let task_local = self.push_synth_let(&task_name, false, task.clone());
                    // `result_ty` is the PAYLOAD the backend loads out of the
                    // terminal frame; the `Result<T, Cancelled>` wrapper that
                    // `await t.result()` binds is built by the emitter from it,
                    // so only the BINDING carries the wrapped type.
                    let (result_ty, cancel_aware) = awaitable_task_type(&task.ty)
                        .expect("checked select join must have a task type");
                    let binding_ty = await_output_type(&task.ty)
                        .expect("checked select join must have an output type");
                    let binding = (binding != "_").then(|| {
                        self.declare_local(
                            binding.clone(),
                            binding_ty,
                            Some(case.span),
                            false,
                            false,
                        )
                    });
                    LirSelectOp::Join {
                        task: task_local,
                        binding,
                        result_ty,
                        cancel_aware,
                    }
                }
                HirSelectCaseKind::Default => LirSelectOp::Default,
            };
            if !self.is_async && matches!(operation, LirSelectOp::Timeout { .. }) {
                self.push(SourceInst::SelectInit {
                    operations: vec![operation.clone()],
                });
            }
            operations.push(operation);
        }

        if self.is_async {
            self.push(SourceInst::SelectInit {
                operations: operations.clone(),
            });
        }
        let probe = self.new_block();
        let idle = self.new_block();
        let dispatch = self.new_block();
        let done = self.new_block();
        let case_blocks: Vec<_> = cases.iter().map(|_| self.new_block()).collect();
        self.terminate(SourceTerminator::Jump(probe));

        self.switch_to(probe);
        let ready: Vec<_> = operations
            .iter()
            .map(|operation| {
                (!matches!(operation, LirSelectOp::Default)).then(|| {
                    let name = self.synthetic_name("select_ready");
                    self.declare_local(name, Type::Bool, None, true, false)
                })
            })
            .collect();
        let chosen_name = self.synthetic_name("select_chosen");
        let chosen = self.declare_local(chosen_name, Type::I64, None, true, false);
        self.push(SourceInst::SelectProbe {
            operations: operations.clone(),
            ready: ready.clone(),
        });
        self.push(SourceInst::SelectPick { ready, chosen });
        let chosen_expr = self.local_expr(chosen, span);
        self.terminate(SourceTerminator::Branch {
            cond: HirExpr {
                kind: HirExprKind::Binary {
                    op: crate::parser::ast::BinOp::Ge,
                    lhs: Box::new(chosen_expr.clone()),
                    rhs: Box::new(HirExpr {
                        kind: HirExprKind::Int(0),
                        ty: Type::I64,
                        span,
                    }),
                },
                ty: Type::Bool,
                span,
            },
            then_block: dispatch,
            else_block: idle,
        });

        self.switch_to(idle);
        if let Some(default) = operations
            .iter()
            .position(|operation| matches!(operation, LirSelectOp::Default))
        {
            self.terminate(SourceTerminator::Jump(case_blocks[default]));
        } else if self.is_async {
            self.terminate(SourceTerminator::Suspend {
                operation: SuspendOp::SelectWait {
                    operations: operations.iter().filter_map(LirSelectOp::wait_op).collect(),
                },
                resume: probe,
            });
        } else {
            let name = self.synthetic_name("select_idle");
            let local = self.declare_local(name, Type::Void, None, true, false);
            let deadlines = operations
                .iter()
                .filter_map(|operation| match operation {
                    LirSelectOp::Timeout { deadline, .. } => Some(LirOperand::Local(*deadline)),
                    _ => None,
                })
                .collect();
            self.push(SourceInst::Compute {
                local,
                value: LirRvalue::SelectIdleWait { deadlines },
                span,
            });
            self.terminate(SourceTerminator::Jump(probe));
        }

        self.switch_to(dispatch);
        let selectable: Vec<_> = operations
            .iter()
            .enumerate()
            .filter(|(_, operation)| !matches!(operation, LirSelectOp::Default))
            .map(|(index, _)| index)
            .collect();
        for (position, index) in selectable.iter().enumerate() {
            let fallback = if position + 1 == selectable.len() {
                probe
            } else {
                self.new_block()
            };
            self.terminate(SourceTerminator::Branch {
                cond: HirExpr {
                    kind: HirExprKind::Binary {
                        op: crate::parser::ast::BinOp::Eq,
                        lhs: Box::new(chosen_expr.clone()),
                        rhs: Box::new(HirExpr {
                            kind: HirExprKind::Int(*index as i64),
                            ty: Type::I64,
                            span,
                        }),
                    },
                    ty: Type::Bool,
                    span,
                },
                then_block: case_blocks[*index],
                else_block: fallback,
            });
            if fallback != probe {
                self.switch_to(fallback);
            }
        }

        for (index, case) in cases.iter().enumerate() {
            self.switch_to(case_blocks[index]);
            self.push(SourceInst::SelectUnregister {
                operations: operations.clone(),
                winner: index,
            });
            let success_name = self.synthetic_name("select_success");
            let success = self.declare_local(success_name, Type::Bool, None, true, false);
            self.push(SourceInst::SelectCommit {
                operation: operations[index].clone(),
                success,
            });
            if matches!(operations[index], LirSelectOp::Send { .. }) {
                let body = self.new_block();
                self.terminate(SourceTerminator::Branch {
                    cond: self.local_expr(success, case.span),
                    then_block: body,
                    else_block: probe,
                });
                self.switch_to(body);
            }
            self.lower_scope(&case.body);
            self.terminate(SourceTerminator::Jump(done));
        }
        self.switch_to(done);
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
                _ => false,
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

    fn lower_stmts(&mut self, stmts: &[HirStmt]) {
        for stmt in stmts {
            self.lower_stmt(stmt);
        }
    }

    /// Lower a statement list as a lexical scope: if it registers any `defer`,
    /// bracket it with [`SourceInst::EnterDeferScope`]/[`SourceInst::LeaveDeferScope`]
    /// (willow-0g8j.2.3).
    ///
    /// Scopes without a `defer` get no markers at all because nothing would
    /// run at their exit.
    fn lower_scope(&mut self, stmts: &[HirStmt]) {
        self.lower_scope_inner(stmts, None);
    }

    /// [`Self::lower_scope`], but `lock` names the critical section this scope
    /// IS the body of, and opens the scope even when the body registers no
    /// `defer` at all.
    ///
    /// A `lock` body needs one regardless (willow-0g8j.2.13): the scope is what
    /// gives the critical section a cleanup block, and a panic inside the
    /// section has to release the lock on its way out even when the section
    /// defers nothing.
    fn lower_scope_inner(&mut self, stmts: &[HirStmt], lock: Option<LirLockSlots>) {
        // Locals are handed out from one growing table, so everything this
        // scope declares — its own bindings and those of any scope nested in it
        // — sits in the range that opens here (willow-0g8j.3.3).
        let lexical_scope = self.scope_starts.len();
        self.scope_starts
            .push(LirScopeMark::opening_at(self.locals.len()));
        let mut scope = std::collections::HashMap::new();
        let sites: Vec<(LirDeferId, Span)> = stmts
            .iter()
            .filter_map(|s| match s {
                HirStmt::Defer {
                    id: hir_id, span, ..
                } => {
                    let id = LirDeferId(self.defer_counter);
                    self.defer_counter += 1;
                    assert!(
                        scope.insert(*hir_id, id).is_none(),
                        "duplicate HIR defer identity"
                    );
                    Some((id, *span))
                }
                _ => None,
            })
            .collect();
        if sites.is_empty() && lock.is_none() {
            self.lower_stmts(stmts);
            self.push_scope_root_clears(lexical_scope);
            self.scope_starts.pop();
            return;
        }
        let enter_block = self.current;
        let enter_index = self.blocks[enter_block].0.len();
        // Only a scope that can actually swallow a panic continues at its
        // resume block; a scope whose defers merely run cleanup lets the panic
        // through, so it adds no edge.
        let recovers = stmts.iter().any(
            |stmt| matches!(stmt, HirStmt::Defer { body, .. } if defer_body_contains_recover(body)),
        );
        self.push(SourceInst::EnterDeferScope {
            sites: sites.clone(),
            resume: None,
            lock,
        });
        self.defer_scopes.push(scope);
        self.defer_depth += 1;
        let body_start = self.blocks.len();
        self.lower_stmts(stmts);
        self.defer_depth -= 1;
        let mut sites: Vec<_> = self
            .defer_scopes
            .pop()
            .expect("LIR defer scope stack")
            .into_values()
            .collect();
        sites.sort_unstable();
        // The fallthrough close. If the scope ended in a `return`, this lands
        // in the dead block `terminate` switched to and is pruned.
        self.push(SourceInst::LeaveDeferScope { sites });
        if self.is_async || recovers {
            // Recovery must branch to a real LIR continuation, not a backend-
            // invented block after the whole poll body has been emitted.
            let resume = self.new_block();
            self.terminate(SourceTerminator::Jump(resume));
            let SourceInst::EnterDeferScope {
                resume: entry_resume,
                ..
            } = &mut self.blocks[enter_block].0[enter_index]
            else {
                unreachable!("recorded defer entry instruction changed kind");
            };
            *entry_resume = Some(resume);
            if recovers {
                // Every block the scope's body occupies can raise the panic
                // this scope recovers from. `new_block` only ever appends, so
                // the body's blocks are exactly the ones created between the
                // entry and the resume, plus the entry block itself.
                //
                // Innermost first falls out of the nesting: an inner scope
                // records its edge while lowering the outer scope's body, so
                // the outer scope's edge is appended after it.
                self.block_recovery[enter_block].push(resume);
                for id in body_start..resume.0 {
                    self.block_recovery[id].push(resume);
                }
            }
            self.switch_to(resume);
            // In the resume block, not before the jump: a recovered panic
            // branches straight here from wherever it was raised, so a clear on
            // the fallthrough path alone would leave the scope's roots standing
            // on the recovery path (willow-0g8j.3.3). Both paths have run the
            // scope's `defer`s by the time they arrive, which is what the clear
            // has to come after — a deferred body may read the bindings.
            self.push_scope_root_clears(lexical_scope);
        } else {
            // After the scope's own `defer`s have run: they may read the
            // bindings this drops the roots of.
            self.push_scope_root_clears(lexical_scope);
        }
        self.scope_starts.pop();
    }

    /// Close the GC roots of the scope at `scope` and of every scope nested
    /// inside it (willow-0g8j.3.3): name the source locals they declared, so the
    /// emitter can null the slots that hold one.
    ///
    /// One instruction covers the whole nest, because the marks nest too: the
    /// outermost one's sweep already spans every local an inner scope declared.
    /// Only their adopted locals have to be gathered scope by scope.
    fn push_scope_root_clears(&mut self, scope: usize) {
        let Some(mark) = self.scope_starts.get(scope) else {
            return;
        };
        let mut locals: Vec<LirLocalId> = self.locals[mark.first_local..]
            .iter()
            .filter(|local| !local.synthetic && !local.parameter)
            .map(|local| local.id)
            .collect();
        for mark in &self.scope_starts[scope..] {
            locals.extend(mark.adopted.iter().copied());
        }
        locals.sort_unstable();
        locals.dedup();
        if !locals.is_empty() {
            self.push(SourceInst::ClearScopeRoots { locals });
        }
    }

    /// Drop the GC roots of every lexical scope an early exit is about to leave
    /// (willow-0g8j.3.3).
    ///
    /// `break` and `continue` jump out without passing the fallthrough close,
    /// so the boundary that close marks has to be re-stated here — the same
    /// reason [`SourceInst::FlushDefers`] exists beside
    /// [`SourceInst::LeaveDeferScope`]. `return` needs nothing, because the emitter
    /// pops every root there.
    fn clear_scope_roots_down_to(&mut self, depth: usize) {
        self.push_scope_root_clears(depth);
    }

    /// Flush every defer scope an early exit is about to leave, releasing the
    /// critical section it leaves on the way out (willow-0g8j.2.13).
    ///
    /// The release sits BETWEEN the two flushes rather than before or after
    /// both: the section's own `defer`s are part of the critical section and
    /// run while the lock is still held, and only then does the lock go back,
    /// before the enclosing scopes' `defer`s run outside it. That is the same
    /// order the AST unwinder produces by releasing as each defer frame
    /// finishes.
    fn flush_defers_down_to(&mut self, depth: usize) {
        match self.active_lock.clone() {
            Some(lock) if lock.defer_depth >= depth => {
                self.flush_defer_scopes(lock.defer_depth, self.defer_scopes.len());
                self.push(SourceInst::ReleaseLock(lock.slots));
                self.flush_defer_scopes(depth, lock.defer_depth);
            }
            _ => self.flush_defer_scopes(depth, self.defer_scopes.len()),
        }
    }

    /// Register one `FlushDefers` for the scopes in `from..to`, newest last.
    fn flush_defer_scopes(&mut self, from: usize, to: usize) {
        let mut sites = Vec::new();
        for scope in &self.defer_scopes[from..to] {
            let mut scope_sites: Vec<_> = scope.values().copied().collect();
            scope_sites.sort_unstable();
            sites.extend(scope_sites);
        }
        if !sites.is_empty() {
            self.push(SourceInst::FlushDefers { sites });
        }
    }

    fn lower_stmt(&mut self, stmt: &HirStmt) {
        match stmt {
            HirStmt::Let {
                name,
                mutable,
                ty,
                value,
                span,
            } => {
                let local = self.declare_local(name.clone(), ty.clone(), Some(*span), false, false);
                if self.lower_root_suspend(value, Some(local)).is_none() {
                    let value = self
                        .lower_nested_suspend(value)
                        .unwrap_or_else(|| value.clone());
                    self.push_existing_let(
                        local,
                        name.clone(),
                        *mutable,
                        ty.clone(),
                        value,
                        Some(*span),
                    );
                }
            }
            HirStmt::Assign { name, value, span } => {
                let local = self.local_by_name.get(name).copied().unwrap_or_else(|| {
                    // Every assignable name is declared before it is assigned:
                    // parameters in `Builder::new`, locals by `HirStmt::Let`,
                    // and a lambda cannot reach an enclosing function's binding
                    // (the checker rejects captures with E1002). Declaring one
                    // here keeps a release build emitting a well-formed graph,
                    // but the write would go to a local nothing else reads, so
                    // a debug build reports it instead of losing it silently.
                    debug_assert!(
                        false,
                        "assignment to `{name}` at line {} has no LIR local; \
                         a binding form is missing from lowering",
                        span.line,
                    );
                    self.declare_local(name.clone(), value.ty.clone(), Some(*span), false, false)
                });
                if self.lower_root_suspend(value, Some(local)).is_none() {
                    let value = self
                        .lower_nested_suspend(value)
                        .unwrap_or_else(|| value.clone());
                    self.push(SourceInst::Assign {
                        local,
                        name: name.clone(),
                        value,
                    });
                }
            }
            HirStmt::FieldAssign {
                object,
                field,
                value,
                ..
            } => {
                let operands = self
                    .lower_assign_operands(&[object, value])
                    .unwrap_or_else(|| vec![object.clone(), value.clone()]);
                let [object, value] = operands
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("two operands in, two operands out"));
                self.push(SourceInst::FieldAssign {
                    object,
                    field: field.clone(),
                    value,
                });
            }
            HirStmt::IndexAssign {
                array,
                index,
                value,
                ..
            } => {
                let operands = self
                    .lower_assign_operands(&[array, index, value])
                    .unwrap_or_else(|| vec![array.clone(), index.clone(), value.clone()]);
                let [array, index, value] = operands
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("three operands in, three operands out"));
                self.push(SourceInst::IndexAssign {
                    array,
                    index,
                    value,
                });
            }
            HirStmt::StaticFieldAssign {
                class,
                field,
                value,
                ..
            } => {
                let operands = self
                    .lower_assign_operands(&[value])
                    .unwrap_or_else(|| vec![value.clone()]);
                let [value] = operands
                    .try_into()
                    .unwrap_or_else(|_| unreachable!("one operand in, one operand out"));
                self.push(SourceInst::StaticFieldAssign {
                    class: *class,
                    field: field.clone(),
                    value,
                });
            }
            HirStmt::SuperInit { args, span } => {
                if !self.lower_super_init_values(args, *span) {
                    self.push(SourceInst::SuperInit {
                        args: args.clone(),
                        span: *span,
                    });
                }
            }
            HirStmt::Expr(e) => {
                if let HirExprKind::Select { cases } = &e.kind {
                    self.lower_select(cases, e.span);
                } else if self.lower_root_suspend(e, None).is_none() {
                    let value = self.lower_nested_suspend(e).unwrap_or_else(|| e.clone());
                    self.push(SourceInst::Expr(value));
                }
            }
            HirStmt::Return { value, .. } => {
                // The returned value is computed BEFORE the defers run: a
                // deferred body can mutate what the expression reads.
                let value = if self.defer_depth == 0 {
                    if let Some(value) = value.as_ref() {
                        {
                            let destination = if value.ty == Type::Void {
                                None
                            } else {
                                let name = self.synthetic_name("return");
                                Some(self.declare_local(name, value.ty.clone(), None, true, false))
                            };
                            match self.lower_root_suspend(value, destination) {
                                Some(Some(value)) => Some(value),
                                // A `void` operand that lowering already turned
                                // into blocks — `return await sleep(1);`, a `void`
                                // `match` — has nothing left to return. Re-reading
                                // the operand here would put the suspension back
                                // into the terminator.
                                Some(None) => None,
                                None => Some(
                                    self.lower_nested_suspend(value)
                                        .unwrap_or_else(|| value.clone()),
                                ),
                            }
                        }
                    } else {
                        None
                    }
                } else {
                    if let Some(value) = value.as_ref() {
                        'returned: {
                            if value.ty == Type::Void {
                                if self.lower_root_suspend(value, None).is_none() {
                                    let value = self
                                        .lower_nested_suspend(value)
                                        .unwrap_or_else(|| value.clone());
                                    self.push(SourceInst::Expr(value));
                                }
                                break 'returned None;
                            }

                            let name = self.synthetic_name("return");
                            let destination = self.declare_local(
                                name.clone(),
                                value.ty.clone(),
                                None,
                                true,
                                false,
                            );
                            if self.lower_root_suspend(value, Some(destination)).is_none() {
                                let evaluated = self
                                    .lower_nested_suspend(value)
                                    .unwrap_or_else(|| value.clone());
                                self.push_existing_let(
                                    destination,
                                    name,
                                    false,
                                    value.ty.clone(),
                                    evaluated,
                                    None,
                                );
                            }
                            Some(self.local_expr(destination, value.span))
                        }
                    } else {
                        None
                    }
                };
                self.flush_defers_down_to(0);
                self.terminate(SourceTerminator::Return(value));
                // Anything after a return is unreachable; give it a fresh
                // predecessor-less block rather than corrupting this one.
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::Break { .. } => {
                let frame = *self.loop_stack.last().expect("break outside loop");
                self.flush_defers_down_to(frame.defer_depth);
                self.clear_scope_roots_down_to(frame.scope_depth);
                self.terminate(SourceTerminator::Jump(frame.exit));
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::Defer {
                id: hir_id,
                body,
                span,
            } => {
                let id = self
                    .defer_scopes
                    .last()
                    .and_then(|scope| scope.get(hir_id))
                    .copied()
                    .expect("defer outside its LIR scope");
                let recovery_capable = defer_body_contains_recover(body);
                let stmts = match body {
                    HirDeferBody::Expr(expr) => {
                        vec![HirStmt::Expr(self.capture_defer_expr(id, expr))]
                    }
                    HirDeferBody::Block(stmts) => stmts.clone(),
                };
                let body = self.lower_defer_region(&stmts, recovery_capable);
                self.push(SourceInst::Defer {
                    id,
                    body,
                    span: *span,
                });
            }
            HirStmt::Continue { .. } => {
                let frame = *self.loop_stack.last().expect("continue outside loop");
                self.flush_defers_down_to(frame.defer_depth);
                self.clear_scope_roots_down_to(frame.scope_depth);
                self.terminate(SourceTerminator::Jump(frame.next));
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                if let HirExprKind::Bool(value) = cond.kind {
                    if value {
                        self.lower_scope(then_branch);
                    } else if let Some(else_branch) = else_branch {
                        self.lower_scope(else_branch);
                    }
                    return;
                }
                let cond = self.lower_condition(cond);
                let then_block = self.new_block();
                let merge_block = self.new_block();
                let else_block = match else_branch {
                    Some(_) => self.new_block(),
                    None => merge_block,
                };
                self.terminate(SourceTerminator::Branch {
                    cond,
                    then_block,
                    else_block,
                });

                self.switch_to(then_block);
                self.lower_scope(then_branch);
                self.terminate(SourceTerminator::Jump(merge_block));

                if let Some(else_branch) = else_branch {
                    self.switch_to(else_block);
                    self.lower_scope(else_branch);
                    self.terminate(SourceTerminator::Jump(merge_block));
                }

                self.switch_to(merge_block);
            }
            HirStmt::While { cond, body, .. } => {
                let header = self.new_block();
                let body_block = self.new_block();
                let backedge = if self.is_async {
                    self.new_block()
                } else {
                    header
                };
                let exit = self.new_block();

                self.terminate(SourceTerminator::Jump(header));
                self.switch_to(header);
                let cond = self.lower_condition(cond);
                self.terminate(SourceTerminator::Branch {
                    cond,
                    then_block: body_block,
                    else_block: exit,
                });

                self.switch_to(body_block);
                self.loop_stack.push(LirLoopFrame {
                    exit,
                    next: backedge,
                    defer_depth: self.defer_depth,
                    scope_depth: self.scope_starts.len(),
                });
                self.lower_scope(body);
                self.loop_stack.pop();
                self.terminate(SourceTerminator::Jump(backedge));

                if self.is_async {
                    self.switch_to(backedge);
                    self.terminate(SourceTerminator::Suspend {
                        operation: SuspendOp::Preempt,
                        resume: header,
                    });
                }

                self.switch_to(exit);
            }
            HirStmt::For {
                name,
                iterable,
                body,
                span,
            } => self.lower_for(name, iterable, body, *span),
            HirStmt::Lock {
                mode,
                target,
                binding,
                body,
                span,
                ..
            } => self.lower_lock(*mode, target, binding, body, *span),
        }
    }

    /// `lock <target> as [mut] <binding> { .. }` (willow-0g8j.2.13).
    ///
    /// The critical section becomes a suspension edge plus a forced defer
    /// scope:
    ///
    /// ```text
    ///   <handle> = target                  // evaluated exactly ONCE
    ///   suspend LockAcquire -> body        // parks and re-polls until owned
    ///   body: EnterDeferScope .. LeaveDeferScope
    ///   ReleaseLock                        // fallthrough exit
    /// ```
    ///
    /// The target is hoisted into its own local because a suspension's operands
    /// must be locals: a resumed poll re-enters at the acquisition and reloads
    /// the handle from the frame, so a side-effecting target expression must
    /// never be re-evaluated. The token and phase locals are compiler-owned
    /// state with no initialiser — the runtime writes the token through a
    /// pointer to its slot — and exist here only so that liveness gives each a
    /// frame slot of its own.
    ///
    /// The scope is forced open even for a section that defers nothing, because
    /// the section's cleanup block is what releases the lock when a panic
    /// leaves it. Every other exit carries an explicit
    /// [`SourceInst::ReleaseLock`]: the fallthrough one is emitted here, and
    /// `return`/`break`/`continue` get theirs from
    /// [`Self::flush_defers_down_to`].
    fn lower_lock(
        &mut self,
        mode: LockMode,
        target: &HirExpr,
        binding: &str,
        body: &[HirStmt],
        span: Span,
    ) {
        let value_ty = match &target.ty {
            Type::Generic(_, args) if args.len() == 1 => args[0].clone(),
            // The checker rejects any other lock target, so this only has to
            // keep lowering total.
            other => other.clone(),
        };
        let binding_local = self.declare_local(
            binding.to_string(),
            value_ty.clone(),
            Some(span),
            false,
            false,
        );
        if !self.is_async {
            // E2603 has already rejected this program; lower the body anyway so
            // the graph stays well formed for the diagnostics that follow.
            self.lower_scope(body);
            return;
        }

        let handle_name = self.synthetic_name("lock_handle");
        let handle = self.declare_local(handle_name.clone(), target.ty.clone(), None, true, false);
        if self.lower_root_suspend(target, Some(handle)).is_none() {
            let evaluated = self
                .lower_nested_suspend(target)
                .unwrap_or_else(|| target.clone());
            self.push_existing_let(
                handle,
                handle_name,
                false,
                target.ty.clone(),
                evaluated,
                None,
            );
        }
        let token_name = self.synthetic_name("lock_token");
        let token = self.declare_local(token_name, Type::I64, None, true, false);
        let phase_name = self.synthetic_name("lock_phase");
        let phase = self.declare_local(phase_name, Type::I64, None, true, false);

        let slots = LirLockSlots {
            mode,
            handle,
            token,
            phase,
            binding: binding_local,
            value_ty,
        };

        let entry = self.new_block();
        self.terminate(SourceTerminator::Suspend {
            operation: SuspendOp::LockAcquire {
                slots: slots.clone(),
                span,
            },
            resume: entry,
        });
        self.switch_to(entry);

        let outer = self.active_lock.replace(ActiveLock {
            slots: slots.clone(),
            defer_depth: self.defer_depth,
        });
        self.lower_scope_inner(body, Some(slots.clone()));
        self.active_lock = outer;
        self.push(SourceInst::ReleaseLock(slots));
    }

    /// Desugar `for` into a while-shaped header/body/exit with an induction
    /// variable: bound-based for ranges, index-based for arrays.
    fn lower_for(
        &mut self,
        name: &str,
        iterable: &HirExpr,
        body: &[HirStmt],
        span: crate::diagnostics::Span,
    ) {
        let n = self.for_counter;
        self.for_counter += 1;
        let i_name = format!("__for{n}_i");
        // The whole `for` construct is a scope of its own (willow-0g8j.3.3).
        // The desugaring hoists the iterable into a synthetic `let` that lives
        // as long as the loop does; nothing in the source scope holds it, so
        // without a close of its own that temp keeps the array — and everything
        // in it — reachable until the function returns.
        let construct = self.scope_starts.len();
        self.scope_starts
            .push(LirScopeMark::opening_at(self.locals.len()));
        let i64_var = |name: &str| HirExpr {
            kind: HirExprKind::Var(name.to_string()),
            ty: Type::I64,
            span,
        };
        let lt = |lhs: HirExpr, rhs: HirExpr| HirExpr {
            kind: HirExprKind::Binary {
                op: crate::parser::ast::BinOp::Lt,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            },
            ty: Type::Bool,
            span,
        };
        let plus_one = |var: HirExpr| HirExpr {
            kind: HirExprKind::Binary {
                op: crate::parser::ast::BinOp::Add,
                lhs: Box::new(var),
                rhs: Box::new(HirExpr {
                    kind: HirExprKind::Int(1),
                    ty: Type::I64,
                    span,
                }),
            },
            ty: Type::I64,
            span,
        };

        // Entry instructions, the loop-variable binding for the body, and the
        // header's upper bound. A range's bound is evaluated once up front; an
        // array's length is re-read on every header entry, so a `push`/`pop`
        // inside the body changes how far the walk goes (willow-0g8j.4).
        let bound_expr: HirExpr;
        let element_binding: HirExpr;
        match (&iterable.kind, &iterable.ty) {
            // for x in start..end  →  i = start; while i < end { x = i; .. }
            (HirExprKind::Range { start, end }, _) => {
                let bound_name = format!("__for{n}_end");
                self.push_synth_let(&i_name, true, (**start).clone());
                self.push_synth_let(&bound_name, false, (**end).clone());
                bound_expr = HirExpr {
                    kind: HirExprKind::Var(bound_name),
                    ty: Type::I64,
                    span,
                };
                element_binding = i64_var(&i_name);
            }
            // for x in arr  →  a = arr; i = 0; while i < a.len() { x = a[i]; .. }
            (_, Type::Array(elem)) => {
                let arr_name = format!("__for{n}_arr");
                let arr_var = HirExpr {
                    kind: HirExprKind::Var(arr_name.clone()),
                    ty: iterable.ty.clone(),
                    span,
                };
                let arr_local = self.push_synth_let(&arr_name, false, iterable.clone());
                self.scope_starts[construct].adopted.push(arr_local);
                self.push_synth_let(
                    &i_name,
                    true,
                    HirExpr {
                        kind: HirExprKind::Int(0),
                        ty: Type::I64,
                        span,
                    },
                );
                // Not hoisted into a `let`: the header re-evaluates it, so a
                // body that grows or shrinks the array is observed.
                bound_expr = HirExpr {
                    kind: HirExprKind::MethodCall {
                        object: Box::new(arr_var.clone()),
                        method: "len".to_string(),
                        args: vec![],
                    },
                    ty: Type::I64,
                    span,
                };
                element_binding = HirExpr {
                    kind: HirExprKind::Index {
                        array: Box::new(arr_var),
                        index: Box::new(i64_var(&i_name)),
                    },
                    ty: (**elem).clone(),
                    span,
                };
            }
            // A range held as a VALUE (`let r = 0..3; for x in r`, or a call
            // that returns one) — for x in r  →  rng = r; i = rng.start;
            // end = rng.end; while i < end { x = i; .. } (willow-0g8j.2.10).
            //
            // Both bounds are read once before the loop. A `Range<i64>` is
            // immutable, so re-reading them per iteration could only cost
            // loads, and hoisting the value itself keeps a call in the iterable
            // position from running twice.
            (_, Type::Generic(g, args))
                if g.namespace().is_none()
                    && g.name() == "Range"
                    && args.first() == Some(&Type::I64) =>
            {
                let range_name = format!("__for{n}_range");
                let bound_name = format!("__for{n}_end");
                let range_local = self.push_synth_let(&range_name, false, iterable.clone());
                self.scope_starts[construct].adopted.push(range_local);
                let bound = |field: &str| HirExpr {
                    kind: HirExprKind::FieldAccess {
                        object: Box::new(HirExpr {
                            kind: HirExprKind::Var(range_name.clone()),
                            ty: iterable.ty.clone(),
                            span,
                        }),
                        field: field.to_string(),
                    },
                    ty: Type::I64,
                    span,
                };
                self.push_synth_let(&i_name, true, bound("start"));
                self.push_synth_let(&bound_name, false, bound("end"));
                bound_expr = HirExpr {
                    kind: HirExprKind::Var(bound_name),
                    ty: Type::I64,
                    span,
                };
                element_binding = i64_var(&i_name);
            }
            _ => {
                // The HIR lowering only produces array/range iterables.
                unreachable!("for over unsupported iterable reached LIR lowering")
            }
        }

        let header = self.new_block();
        let body_block = self.new_block();
        // Dedicated increment block: `continue` jumps HERE so the induction
        // variable still advances (willow-kzka).
        let inc_block = self.new_block();
        let exit = self.new_block();

        self.terminate(SourceTerminator::Jump(header));
        self.switch_to(header);
        self.terminate(SourceTerminator::Branch {
            cond: lt(i64_var(&i_name), bound_expr),
            then_block: body_block,
            else_block: exit,
        });

        self.switch_to(body_block);
        // One iteration is a scope of its own, opened around the element
        // binding: lowering emits that `let` ahead of the body's own scope, and
        // flags it synthetic because it synthesized it from the iteration
        // protocol, but the binding is the source loop variable and its root
        // ends with the iteration like any other (willow-0g8j.3.3).
        let iteration = self.scope_starts.len();
        self.scope_starts
            .push(LirScopeMark::opening_at(self.locals.len()));
        let element = self.push_synth_let(name, false, element_binding);
        self.scope_starts[iteration].adopted.push(element);
        self.loop_stack.push(LirLoopFrame {
            exit,
            next: inc_block,
            defer_depth: self.defer_depth,
            scope_depth: iteration,
        });
        self.lower_scope(body);
        self.loop_stack.pop();
        self.push_scope_root_clears(iteration);
        self.scope_starts.pop();
        self.terminate(SourceTerminator::Jump(inc_block));

        self.switch_to(inc_block);
        let local = self.local_by_name[&i_name];
        self.push(SourceInst::Assign {
            local,
            name: i_name.clone(),
            value: plus_one(i64_var(&i_name)),
        });
        if self.is_async {
            self.terminate(SourceTerminator::Suspend {
                operation: SuspendOp::Preempt,
                resume: header,
            });
        } else {
            self.terminate(SourceTerminator::Jump(header));
        }

        self.switch_to(exit);
        // Both ways out of the loop land here — fallthrough from the header and
        // every `break` — so the iterable temp's root is dropped once, on the
        // one path that leaves the construct.
        self.push_scope_root_clears(construct);
        self.scope_starts.pop();
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
        SourceInst::FlushDefers { sites } => format!("flush defers ({} sites);", sites.len()),
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
            // is the case a reader cannot infer from the value (willow-0g8j.5).
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
mod tests {
    #[test]
    fn defer_identity_survives_twenty_shared_span_shapes() {
        for count in 1..=20 {
            let source = format!("fn f() {{ {} }}", "defer print(1);".repeat(count));
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (mut ast, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty());
            let crate::parser::ast::Item::Function(f) = &mut ast.items[0] else {
                unreachable!()
            };
            for stmt in &mut f.body.stmts {
                if let crate::parser::ast::Stmt::Defer(defer) = stmt {
                    defer.span = f.span;
                }
            }
            let (hir, errors) = crate::ir::lower::lower_program(&ast);
            assert!(errors.is_empty(), "{errors:?}");
            let lir = lower_source_program(&hir);
            let ids: std::collections::HashSet<_> = lir.functions[0]
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .filter_map(|inst| {
                    if let SourceInst::Defer { id, .. } = inst {
                        Some(*id)
                    } else {
                        None
                    }
                })
                .collect();
            assert_eq!(ids.len(), count, "same-span defer sites must stay distinct");
        }
    }

    #[test]
    fn deep_expression_cfg_lowering_uses_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = Span::dummy();
                let int = |n| HirExpr {
                    kind: HirExprKind::Int(n),
                    ty: Type::I64,
                    span,
                };
                let mut expr = HirExpr {
                    kind: HirExprKind::Ternary {
                        condition: Box::new(HirExpr {
                            kind: HirExprKind::Bool(true),
                            ty: Type::Bool,
                            span,
                        }),
                        then_expr: Box::new(int(1)),
                        else_expr: Box::new(int(2)),
                    },
                    ty: Type::I64,
                    span,
                };
                for _ in 0..50_000 {
                    expr = HirExpr {
                        kind: HirExprKind::Unary {
                            op: crate::parser::ast::UnaryOp::Neg,
                            operand: Box::new(expr),
                        },
                        ty: Type::I64,
                        span,
                    };
                }
                let mut builder = Builder::new(&[], false);
                let lowered = builder
                    .lower_nested_cfg(&expr)
                    .expect("nested control flow");
                assert!(!expression_needs_cfg(&lowered));
                assert!(
                    builder
                        .blocks
                        .iter()
                        .any(|(_, term)| matches!(term, Some(SourceTerminator::Branch { .. })))
                );
                assert!(
                    builder
                        .blocks
                        .iter()
                        .map(|(insts, _)| insts.len())
                        .sum::<usize>()
                        >= 50_000
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn deep_hir_predicates_use_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                for has_call in [false, true] {
                    let span = Span::new(0, 0, 1, 1);
                    let mut expr = HirExpr {
                        kind: if has_call {
                            HirExprKind::Call {
                                callee: "recover".into(),
                                args: Vec::new(),
                            }
                        } else {
                            HirExprKind::Int(1)
                        },
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
                    assert_eq!(expr_has_recover(&expr), has_call);
                    assert_eq!(expression_executes_call(&expr), has_call);
                    let mut lambdas = Vec::new();
                    collect_lambdas_in_expr(&expr, &mut lambdas, &Default::default());
                    assert!(lambdas.is_empty());
                    drop(expr);
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn deep_hir_suspension_collection_uses_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let span = Span::new(0, 0, 1, 1);
                let mut expr = HirExpr {
                    kind: HirExprKind::Int(1),
                    ty: Type::I64,
                    span,
                };
                expr = HirExpr {
                    kind: HirExprKind::Await {
                        inner: Box::new(expr),
                    },
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
                let mut found = Vec::new();
                collect_suspensions(&expr, &mut found);
                assert_eq!(found.len(), 1);
                assert!(suspends_anywhere(&expr));
                assert!(contains_suspend_span(&expr, span));
                drop(found);
                drop(expr);
            })
            .unwrap()
            .join()
            .unwrap();
    }

    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    /// Parse + HIR-lower + LIR-lower; assert no HIR diagnostics.
    fn lir(src: &str) -> SourceProgram {
        let tokens = Lexer::new(src).tokenize().expect("lexing failed");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        let (hir, diags) = super::super::lower::lower_program(&program);
        assert!(diags.is_empty(), "HIR diagnostics: {diags:?}");
        lower_source_program(&hir)
    }

    fn func<'a>(p: &'a SourceProgram, name: &str) -> &'a SourceFunction {
        p.functions
            .iter()
            .find(|f| f.name.to_string() == name)
            .unwrap_or_else(|| panic!("no function {name}"))
    }

    #[test]
    fn defer_cleanup_region_owns_cfg_and_captures_only_outer_reads_or_writes() {
        let program = lir(
            "fn f(flag: bool) { let mut used = 1; let unused = 2; defer { let own = 3; while flag { used = own; } } }",
        );
        let outer = func(&program, "f");
        let cleanup = outer
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .find_map(|inst| {
                if let SourceInst::Defer { body, .. } = inst {
                    Some(body)
                } else {
                    None
                }
            })
            .unwrap();
        let captures: Vec<_> = cleanup
            .captures
            .iter()
            .map(|id| outer.locals[id.0 as usize].name.as_str())
            .collect();
        assert_eq!(captures, ["flag", "used"]);
        assert!(
            cleanup
                .function
                .blocks
                .iter()
                .any(|block| matches!(block.terminator, SourceTerminator::Branch { .. }))
        );
        assert!(
            cleanup
                .function
                .blocks
                .iter()
                .any(|block| block.terminator == SourceTerminator::CleanupReturn)
        );
        assert!(
            !cleanup
                .function
                .blocks
                .iter()
                .any(|block| matches!(block.terminator, SourceTerminator::Return(_)))
        );
        assert!(
            cleanup
                .function
                .locals
                .iter()
                .any(|local| local.name == "own" && !local.parameter)
        );
        for inst in cleanup
            .function
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
        {
            if let SourceInst::ClearScopeRoots { locals } = inst {
                assert!(
                    locals
                        .iter()
                        .all(|id| !cleanup.function.locals[id.0 as usize].parameter)
                );
            }
        }
    }

    #[test]
    fn expression_cleanup_region_captures_registration_time_operand() {
        let program = lir("fn f() { let mut value = 1; defer println(value); value = 2; }");
        let outer = func(&program, "f");
        let cleanup = outer
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .find_map(|inst| {
                if let SourceInst::Defer { body, .. } = inst {
                    Some(body)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(cleanup.captures.len(), 1);
        let captured = &outer.locals[cleanup.captures[0].0 as usize];
        assert!(captured.synthetic);
        assert_ne!(captured.name, "value");
    }

    // 1. a straight-line body is a single entry block
    #[test]
    fn l01_straight_line_single_block() {
        let p = lir("fn f() { let a = 1; print(a); }");
        let f = func(&p, "f");
        assert_eq!(f.blocks[0].instrs.len(), 3);
        assert!(matches!(
            &f.blocks[0].instrs[1],
            SourceInst::Compute {
                value: LirRvalue::Print { .. },
                ..
            }
        ));
        assert_eq!(f.blocks[0].terminator, SourceTerminator::Return(None));
    }

    // 2. entry block is always id 0
    #[test]
    fn l02_entry_is_block_zero() {
        let p = lir("fn f() { }");
        assert_eq!(func(&p, "f").blocks[0].id, BlockId(0));
    }

    // 3. an explicit `return v;` becomes a Return terminator with the value
    #[test]
    fn l03_return_value_terminator() {
        let p = lir("fn f() -> i64 { return 7; }");
        let f = func(&p, "f");
        assert!(matches!(
            &f.blocks[0].terminator,
            SourceTerminator::Return(Some(v)) if matches!(v.kind, HirExprKind::Int(7))
        ));
    }

    // 4. an empty function still gets an implicit `return`
    #[test]
    fn l04_empty_fn_implicit_return() {
        let p = lir("fn f() { }");
        assert_eq!(
            func(&p, "f").blocks[0].terminator,
            SourceTerminator::Return(None)
        );
    }

    // 5. `if` without else: entry branches then/merge, then jumps to merge
    #[test]
    fn l05_if_without_else_shape() {
        let p = lir("fn f(c: bool) { if c { print(1); } print(2); }");
        let f = func(&p, "f");
        let SourceTerminator::Branch {
            then_block,
            else_block,
            ..
        } = &f.blocks[0].terminator
        else {
            panic!("entry must branch");
        };
        // No else → the false edge goes straight to the merge block.
        assert_eq!(
            f.blocks[then_block.0].terminator,
            SourceTerminator::Jump(*else_block)
        );
        // The merge block holds the trailing statement.
        assert!(f.blocks[else_block.0].instrs.iter().any(|inst| matches!(
            inst,
            SourceInst::Compute {
                value: LirRvalue::Print {
                    value: LirOperand::Int(2),
                    ..
                },
                ..
            }
        )));
    }

    // 6. `if`/`else`: both arms jump to the same merge block
    #[test]
    fn l06_if_else_merges() {
        let p = lir("fn f(c: bool) { if c { print(1); } else { print(2); } print(3); }");
        let f = func(&p, "f");
        let SourceTerminator::Branch {
            then_block,
            else_block,
            ..
        } = &f.blocks[0].terminator
        else {
            panic!("entry must branch");
        };
        let SourceTerminator::Jump(merge_a) = f.blocks[then_block.0].terminator else {
            panic!("then must jump to merge");
        };
        let SourceTerminator::Jump(merge_b) = f.blocks[else_block.0].terminator else {
            panic!("else must jump to merge");
        };
        assert_eq!(merge_a, merge_b);
        assert_ne!(merge_a, *then_block);
        assert_ne!(merge_a, *else_block);
    }

    // 7. the branch condition is the lowered Bool expression
    #[test]
    fn l07_branch_cond_is_bool() {
        let p = lir("fn f(a: i64) { if a > 0 { print(1); } }");
        let f = func(&p, "f");
        let SourceTerminator::Branch { cond, .. } = &f.blocks[0].terminator else {
            panic!("entry must branch");
        };
        assert_eq!(cond.ty, Type::Bool);
    }

    // 8. `while`: entry jumps to a header that branches body/exit
    #[test]
    fn l08_while_header_shape() {
        let p = lir("fn f(c: bool) { while c { print(1); } }");
        let f = func(&p, "f");
        let SourceTerminator::Jump(header) = f.blocks[0].terminator else {
            panic!("entry must jump to the loop header");
        };
        let SourceTerminator::Branch {
            then_block: body, ..
        } = &f.blocks[header.0].terminator
        else {
            panic!("header must branch");
        };
        // The body jumps back to the header (the loop backedge).
        assert_eq!(f.blocks[body.0].terminator, SourceTerminator::Jump(header));
    }

    // 9. the `while` condition lives in the header, not the entry block
    #[test]
    fn l09_while_cond_in_header() {
        let p = lir("fn f(a: i64) { while a > 0 { print(1); } }");
        let f = func(&p, "f");
        assert!(matches!(f.blocks[0].terminator, SourceTerminator::Jump(_)));
        let SourceTerminator::Jump(header) = f.blocks[0].terminator else {
            unreachable!()
        };
        assert!(matches!(
            f.blocks[header.0].terminator,
            SourceTerminator::Branch { .. }
        ));
    }

    // 10. range-for desugars to induction let + bound let + header branch
    #[test]
    fn l10_range_for_desugar() {
        let p = lir("fn f() { for i in 0..3 { print(i); } }");
        let f = func(&p, "f");
        let names: Vec<_> = f.blocks[0]
            .instrs
            .iter()
            .filter_map(|i| match i {
                SourceInst::Let { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"__for0_i"), "{names:?}");
        assert!(names.contains(&"__for0_end"), "{names:?}");
    }

    // 11. the range-for body rebinds the loop variable and increments
    #[test]
    fn l11_range_for_body_binding_and_increment() {
        let p = lir("fn f() { for i in 0..3 { print(i); } }");
        let f = func(&p, "f");
        let SourceTerminator::Jump(header) = f.blocks[0].terminator else {
            unreachable!()
        };
        let SourceTerminator::Branch {
            then_block: body, ..
        } = &f.blocks[header.0].terminator
        else {
            panic!("header must branch");
        };
        let body = &f.blocks[body.0];
        assert!(matches!(
            &body.instrs[0],
            SourceInst::Let { name, .. } if name == "i"
        ));
        // The increment lives in a dedicated block (the `continue` target,
        // willow-kzka): body jumps to it, and it assigns the induction var.
        let SourceTerminator::Jump(inc) = body.terminator else {
            panic!("body must jump to the increment block");
        };
        let inc = &f.blocks[inc.0];
        assert!(matches!(
            inc.instrs.last(),
            Some(SourceInst::Assign { name, .. }) if name == "__for0_i"
        ));
        assert!(matches!(inc.terminator, SourceTerminator::Jump(h) if h == header));
    }

    // 12. array-for desugars to arr/index lets, a header that RE-READS `len()`
    // (so growth/shrinkage inside the body is observed), and an indexed element
    // bind
    #[test]
    fn l12_array_for_desugar() {
        let p = lir("fn f() { let xs = [1, 2]; for v in xs { print(v); } }");
        let f = func(&p, "f");
        let names: Vec<_> = f.blocks[0]
            .instrs
            .iter()
            .filter_map(|i| match i {
                SourceInst::Let { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"__for0_arr"), "{names:?}");
        assert!(names.contains(&"__for0_i"), "{names:?}");
        // The length is NOT hoisted into a `let`.
        assert!(!names.contains(&"__for0_len"), "{names:?}");
        let SourceTerminator::Jump(header) = f.blocks[0].terminator else {
            unreachable!()
        };
        let SourceTerminator::Branch {
            cond,
            then_block: body,
            ..
        } = &f.blocks[header.0].terminator
        else {
            panic!("header must branch");
        };
        // The bound is a fresh `__for0_arr.len()` call in the header itself.
        let HirExprKind::Var(condition) = &cond.kind else {
            panic!("header condition must be a flat value");
        };
        let header_instructions = &f.blocks[header.0].instrs;
        assert!(header_instructions.iter().any(|inst| matches!(inst,
            SourceInst::Compute { value: LirRvalue::IntrinsicCall { method, .. }, .. } if method == "len")));
        assert!(header_instructions.iter().any(|inst| matches!(inst,
            SourceInst::Compute { local, value: LirRvalue::Binary { op: crate::parser::ast::BinOp::Lt, .. }, .. }
                if f.locals[local.0 as usize].name == *condition)));
        // v = __for0_arr[__for0_i], typed with the element type.
        let (name, value) = f.blocks[body.0]
            .instrs
            .iter()
            .find_map(|inst| match inst {
                SourceInst::Let { name, value, .. } if name == "v" => Some((name, value)),
                _ => None,
            })
            .expect("body binds the loop variable");
        assert_eq!(name, "v");
        let HirExprKind::Var(index_result) = &value.kind else {
            panic!("index must be a flat value");
        };
        assert!(f.blocks[body.0].instrs.iter().any(|inst| matches!(inst,
            SourceInst::Compute { local, value: LirRvalue::Index { .. }, .. } if f.locals[local.0 as usize].name == *index_result)));
        assert_eq!(value.ty, Type::I64);
    }

    // 13. nested `for` loops get distinct induction variables
    #[test]
    fn l13_nested_for_unique_induction_vars() {
        let p = lir("fn f() { for i in 0..2 { for j in 0..2 { print(i + j); } } }");
        let f = func(&p, "f");
        let all_lets: Vec<String> = f
            .blocks
            .iter()
            .flat_map(|b| &b.instrs)
            .filter_map(|i| match i {
                SourceInst::Let { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        assert!(all_lets.iter().any(|n| n == "__for0_i"), "{all_lets:?}");
        assert!(all_lets.iter().any(|n| n == "__for1_i"), "{all_lets:?}");
    }

    // 14. a return inside an if leaves both paths terminated
    #[test]
    fn l14_return_inside_if() {
        let p = lir("fn f(c: bool) -> i64 { if c { return 1; } return 2; }");
        let f = func(&p, "f");
        // Every block has a terminator (no panics, no fallthrough corruption).
        for b in &f.blocks {
            match &b.terminator {
                SourceTerminator::Jump(_)
                | SourceTerminator::Branch { .. }
                | SourceTerminator::Suspend { .. }
                | SourceTerminator::Return(_)
                | SourceTerminator::CleanupReturn => {}
            }
        }
        // The then-arm's return survives as a Return terminator.
        let SourceTerminator::Branch { then_block, .. } = &f.blocks[0].terminator else {
            panic!("entry must branch");
        };
        assert!(matches!(
            f.blocks[then_block.0].terminator,
            SourceTerminator::Return(Some(_))
        ));
    }

    // 15. statement order within a block is preserved
    #[test]
    fn l15_instr_order_preserved() {
        let p = lir("fn f() { let a = 1; let b = 2; print(a + b); }");
        let f = func(&p, "f");
        let kinds: Vec<_> = f.blocks[0]
            .instrs
            .iter()
            .map(|i| match i {
                SourceInst::Let { name, .. } => format!("let {name}"),
                SourceInst::Compute {
                    value: LirRvalue::Binary { .. },
                    ..
                } => "binary".to_string(),
                SourceInst::Compute {
                    value: LirRvalue::Print { .. },
                    ..
                } => "print".to_string(),
                SourceInst::Expr(_) => "expr".to_string(),
                SourceInst::ClearScopeRoots { .. } => "clear roots".to_string(),
                _ => "other".to_string(),
            })
            .collect();
        assert_eq!(kinds, ["let a", "let b", "binary", "print", "clear roots"]);
    }

    // 16. field/index/static assignments lower to their instructions
    #[test]
    fn l16_assignment_instructions() {
        let p = lir("class C { x: i64; static mut t: i64 = 0; } \
             fn f() { let p = new C(1); p.x = 2; let xs = [1]; xs[0] = 9; C::t = 5; }");
        let f = func(&p, "f");
        let instrs = &f.blocks[0].instrs;
        assert!(instrs.iter().any(|i| matches!(
            i,
            SourceInst::Compute {
                value: LirRvalue::FieldStore { .. },
                ..
            }
        )));
        assert!(instrs.iter().any(|i| matches!(
            i,
            SourceInst::Compute {
                value: LirRvalue::ArrayStore { .. },
                ..
            }
        )));
        assert!(instrs.iter().any(|i| matches!(
            i,
            SourceInst::Compute {
                value: LirRvalue::StaticStore { .. },
                ..
            }
        )));
    }

    // 17. class methods are flattened as `Class::method`
    #[test]
    fn l17_class_methods_flattened() {
        let p = lir("class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }");
        assert!(
            p.functions
                .iter()
                .any(|f| f.name == FunctionId::method(TypeId::local("Box"), "get"))
        );
    }

    // 18. super.init becomes an explicit base constructor call.
    #[test]
    fn l18_constructor_flattened_with_super_init() {
        let p = lir(
            "open class A { v: i64; init(self, v: i64) { self.v = v; } } \
             class B extends A { init(self, v: i64) { super.init(v); } }",
        );
        let init = func(&p, "B::init");
        assert!(
            init.blocks[0]
                .instrs
                .iter()
                .any(|i| matches!(i, SourceInst::Compute { value: LirRvalue::ConstructorCall { class, .. }, .. } if *class == TypeId::local("A")))
        );
    }

    // 19. params and return type are carried onto the LIR function
    #[test]
    fn l19_signature_carried() {
        let p = lir("fn f(a: i64, b: bool) -> i64 { return a; }");
        let f = func(&p, "f");
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.return_type, Type::I64);
    }

    // 20. nested if inside while keeps the loop backedge intact
    #[test]
    fn l20_if_inside_while() {
        let p = lir(
            "fn f(n: i64) { let mut i = 0; while i < n { if i > 2 { print(i); } i = i + 1; } }",
        );
        let f = func(&p, "f");
        let SourceTerminator::Jump(header) = f.blocks[0].terminator else {
            unreachable!()
        };
        // Some block jumps back to the header — the loop backedge survives the
        // nested if's merge.
        let backedges = f
            .blocks
            .iter()
            .filter(|b| b.id != BlockId(0) && b.terminator == SourceTerminator::Jump(header))
            .count();
        assert!(backedges >= 1);
    }

    // 21. the LIR text dump renders labeled blocks and terminators
    #[test]
    fn l21_text_dump_shape() {
        let p = lir("fn f(c: bool) -> i64 { if c { return 1; } return 2; }");
        let text = format_source_program(&p);
        assert!(text.contains("bb0:"), "{text}");
        assert!(text.contains("branch c: bool bb"), "{text}");
        assert!(text.contains("return 1: i64"), "{text}");
    }

    // 22. expression-level control flow (ternary/match) stays in instructions
    #[test]
    fn l22_expression_control_flow_becomes_blocks() {
        let p = lir("fn f(c: bool) -> i64 { return c ? 1 : 2; }");
        let f = func(&p, "f");
        assert!(matches!(
            &f.blocks[0].terminator,
            SourceTerminator::Branch { .. }
        ));
        assert!(f.blocks.iter().any(|block| matches!(&block.terminator,
            SourceTerminator::Return(Some(v)) if matches!(v.kind, HirExprKind::Var(_)))));
    }

    #[test]
    fn l26_async_await_is_an_explicit_suspend_edge() {
        let p = lir("async fn f() { await sleep(1); print(2); }");
        let f = func(&p, "f");
        assert!(f.is_async);
        assert!(f.blocks.iter().any(|block| matches!(
            block.terminator,
            SourceTerminator::Suspend {
                operation: SuspendOp::Sleep { .. },
                ..
            }
        )));
    }

    #[test]
    fn l27_synthetic_locals_have_identity_without_source_spans() {
        let p = lir("async fn f(xs: Array<i64>) { for x in xs { await yield(); print(x); } }");
        let f = func(&p, "f");
        let synthetic: Vec<_> = f.locals.iter().filter(|local| local.synthetic).collect();
        assert!(!synthetic.is_empty());
        assert!(synthetic.iter().all(|local| local.source_span.is_none()));
        let ids: std::collections::HashSet<_> = synthetic.iter().map(|local| local.id).collect();
        assert_eq!(ids.len(), synthetic.len());
        assert!(
            synthetic.iter().any(|local| {
                local.name.starts_with("__for") && f.async_frame.slot(local.id).is_some()
            }),
            "locals/frame: {:#?} / {:#?}",
            f.locals,
            f.async_frame
        );
    }

    #[test]
    fn l28_async_frame_is_keyed_by_local_id_not_span() {
        let p = lir("async fn f() { let keep = 1; await yield(); print(keep); await yield(); }");
        let f = func(&p, "f");
        let keep = f.locals.iter().find(|local| local.name == "keep").unwrap();
        assert!(f.async_frame.locals.contains_key(&keep.id));
        assert!(
            f.async_frame
                .locals
                .keys()
                .all(|id| f.locals.get(id.0 as usize).is_some())
        );
    }

    #[test]
    fn l29_async_select_is_cfg_plus_suspend() {
        let p = lir(r#"
async fn f(ch: Channel<i64>) {
    select {
        let v = ch.recv() => { await yield(); print(v); }
        sleep(1) => { print(0); }
    }
}
"#);
        let f = func(&p, "f");
        assert!(f.blocks.iter().any(|block| {
            block
                .instrs
                .iter()
                .any(|inst| matches!(inst, SourceInst::SelectProbe { .. }))
        }));
        assert!(f.blocks.iter().any(|block| {
            block
                .instrs
                .iter()
                .any(|inst| matches!(inst, SourceInst::SelectPick { .. }))
        }));
        assert!(f.blocks.iter().any(|block| {
            block
                .instrs
                .iter()
                .any(|inst| matches!(inst, SourceInst::SelectUnregister { .. }))
        }));
        assert!(f.blocks.iter().any(|block| {
            block
                .instrs
                .iter()
                .any(|inst| matches!(inst, SourceInst::SelectCommit { .. }))
        }));
        assert!(f.blocks.iter().any(|block| matches!(
            block.terminator,
            SourceTerminator::Suspend {
                operation: SuspendOp::SelectWait { .. },
                ..
            }
        )));
        assert!(f.blocks.iter().any(|block| matches!(
            block.terminator,
            SourceTerminator::Suspend {
                operation: SuspendOp::Yield,
                ..
            }
        )));
    }

    #[test]
    fn l30_nested_await_return_value_is_fixed_before_defer_flush() {
        let p = lir(r#"
async fn one() -> i64 { return 1; }
async fn f() -> i64 {
    let mut x = 1;
    defer { x = 9; }
    return (await one()) + x;
}
"#);
        let f = func(&p, "f");
        let block = f
            .blocks
            .iter()
            .find(|block| {
                block
                    .instrs
                    .iter()
                    .any(|inst| matches!(inst, SourceInst::FlushDefers { .. }))
            })
            .expect("return block must flush its defer");
        let flush_index = block
            .instrs
            .iter()
            .position(|inst| matches!(inst, SourceInst::FlushDefers { .. }))
            .unwrap();
        assert!(block.instrs[..flush_index].iter().any(|inst| matches!(
            inst,
            SourceInst::Compute {
                value: LirRvalue::Binary { .. },
                ..
            }
        )));
        let (return_local, return_name) = block.instrs[..flush_index]
            .iter()
            .find_map(|inst| match inst {
                SourceInst::Let {
                    local, name, value, ..
                } if name.starts_with("__async_return_")
                    && matches!(value.kind, HirExprKind::Var(_)) =>
                {
                    Some((*local, name.as_str()))
                }
                _ => None,
            })
            .expect("the complete return expression must be stored before flushing defers");
        assert!(matches!(
            &block.terminator,
            SourceTerminator::Return(Some(HirExpr {
                kind: HirExprKind::Var(name),
                ..
            })) if name == return_name
        ));
        assert_eq!(f.locals[return_local.0 as usize].name, return_name);
    }

    /// The blocks a `match` arm's suspension is cut into (willow-0g8j.2.11.1).
    /// A `match` whose arms do not suspend stays a single `Expr`/`Assign`
    /// instruction, so every assertion here is also a check that the split is
    /// taken only when an arm needs it.
    #[test]
    fn l31_suspending_match_arm_becomes_blocks() {
        let p = lir(r#"
async fn leaf(n: i64) -> i64 { return n; }
async fn f(which: i64) -> i64 {
    match which {
        1 => { return await leaf(10); }
        _ => { return await leaf(20); }
    }
}
"#);
        let f = func(&p, "f");
        // One test per non-catch-all arm, dispatched by a two-way branch.
        assert_eq!(
            f.blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .filter(|inst| matches!(inst, SourceInst::MatchTest { .. }))
                .count(),
            1
        );
        assert!(
            f.blocks
                .iter()
                .any(|block| matches!(block.terminator, SourceTerminator::Branch { .. }))
        );
        // Both arms suspend, so each one ends up behind its own suspend edge.
        assert_eq!(
            f.blocks
                .iter()
                .filter(|block| matches!(
                    block.terminator,
                    SourceTerminator::Suspend {
                        operation: SuspendOp::AwaitTask { .. },
                        ..
                    }
                ))
                .count(),
            2
        );
    }

    /// The dispatch chain reads one scrutinee local, evaluated once before the
    /// first test. Re-reading the source expression per arm would run its side
    /// effects once per test.
    #[test]
    fn l32_match_scrutinee_is_evaluated_once() {
        let p = lir(r#"
async fn leaf(n: i64) -> i64 { return n; }
async fn f(which: i64) -> i64 {
    match which + 1 {
        1 => { return await leaf(10); }
        2 => { return await leaf(20); }
        _ => { return 0; }
    }
}
"#);
        let f = func(&p, "f");
        let tests: Vec<_> = f
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| match inst {
                SourceInst::MatchTest { scrutinee, .. } => Some(*scrutinee),
                _ => None,
            })
            .collect();
        assert_eq!(tests.len(), 2);
        assert_eq!(tests[0], tests[1]);
        let scrutinee = f.locals[tests[0].0 as usize].clone();
        assert!(scrutinee.synthetic);
        assert!(scrutinee.name.starts_with("__async_match_scrutinee_"));
    }

    /// A catch-all arm cannot fail, so it is entered by a jump with no test at
    /// all -- and nothing is lowered after it, since no later arm is reachable.
    #[test]
    fn l33_catch_all_arm_needs_no_test() {
        let p = lir(r#"
async fn leaf(n: i64) -> i64 { return n; }
async fn f(which: i64) -> i64 {
    match which {
        bound => { return await leaf(bound); }
    }
}
"#);
        let f = func(&p, "f");
        assert!(
            !f.blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(inst, SourceInst::MatchTest { .. }))
        );
        assert_eq!(f.blocks[0].terminator, SourceTerminator::Jump(BlockId(1)));
        // The catch-all still binds the whole scrutinee.
        assert!(
            f.blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(inst, SourceInst::MatchBind { .. }))
        );
    }

    /// A payload binding is destructured inside the arm's own block, so it is
    /// defined on exactly the path that reads it -- which is what lets liveness
    /// frame it for one arm without framing it for the others.
    #[test]
    fn l34_arm_bindings_are_defined_in_the_arm_block() {
        let p = lir(r#"
enum Shape { Circle(i64), Rect(i64, i64), Empty }
async fn leaf(n: i64) -> i64 { return n; }
async fn f(shape: Shape) -> i64 {
    match shape {
        Shape::Circle(r) => { let n = r + 1; await yield(); return n; }
        Shape::Rect(w, h) => { let a = await leaf(w); return a + h; }
        Shape::Empty => { return 0; }
    }
}
"#);
        let f = func(&p, "f");
        let binds: Vec<_> = f
            .blocks
            .iter()
            .flat_map(|block| &block.instrs)
            .filter_map(|inst| match inst {
                SourceInst::MatchBind { bindings, .. } => Some(bindings.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(binds.len(), 2, "only the two payload arms bind");
        assert_eq!(binds[0].len(), 1);
        assert_eq!(binds[1].len(), 2);
        // `h` is read after its arm's suspension, so it must be framed; `r` is
        // dead by the time its arm suspends and must not be. Both bindings come
        // out of the same instruction shape, so what separates them is which
        // block each was defined in.
        let local = |name: &str| f.locals.iter().find(|l| l.name == name).unwrap().id;
        assert!(f.async_frame.slot(local("h")).is_some());
        assert!(f.async_frame.slot(local("r")).is_none());
    }

    #[test]
    fn synchronous_root_conditionals_are_cfg() {
        for source in [
            "fn f(c: bool) -> i64 { return c ? 10 : 20; }",
            "fn f(c: bool, d: bool) -> bool { return c && d; }",
            "fn f(c: bool, d: bool) -> bool { return c || d; }",
            "fn f(c: bool, d: bool) -> i64 { if c && d { return 10; } return 20; }",
            "fn f(c: bool) -> i64 { let v = c ? 10 : 20; return v; }",
        ] {
            let p = lir(source);
            let f = func(&p, "f");
            assert!(
                f.blocks
                    .iter()
                    .any(|block| matches!(block.terminator, SourceTerminator::Branch { .. })),
                "{source}"
            );
            for block in &f.blocks {
                if let SourceTerminator::Return(Some(value))
                | SourceTerminator::Branch { cond: value, .. } = &block.terminator
                {
                    assert!(
                        !matches!(
                            value.kind,
                            HirExprKind::Ternary { .. }
                                | HirExprKind::Binary {
                                    op: crate::parser::ast::BinOp::And
                                        | crate::parser::ast::BinOp::Or,
                                    ..
                                }
                        ),
                        "{source}"
                    );
                }
            }
        }
    }

    #[test]
    fn nested_conditionals_are_evaluated_before_eager_parents() {
        for source in [
            "fn f(c: bool, x: i64) -> i64 { return x + (c ? 10 : 20); }",
            "fn g(a: i64, b: i64) -> i64 { return a + b; } fn f(c: bool, x: i64) -> i64 { return g(x, c ? 10 : 20); }",
            "fn f(c: bool, xs: Array<i64>) -> i64 { return xs[c ? 0 : 1]; }",
            "fn f(c: bool, xs: Array<i64>) { xs[c ? 0 : 1] = c ? 10 : 20; }",
            "fn g(a: &mut i64, b: i64) { a = b; } fn f(c: bool, xs: Array<i64>) { g(&xs[c ? 0 : 1], c ? 10 : 20); }",
        ] {
            let p = lir(source);
            let f = func(&p, "f");
            assert!(
                f.blocks
                    .iter()
                    .any(|block| matches!(block.terminator, SourceTerminator::Branch { .. })),
                "{source}"
            );
            let check =
                |value: &HirExpr| assert!(!expression_needs_cfg(value), "{source}: {value:?}");
            for block in &f.blocks {
                for inst in &block.instrs {
                    match inst {
                        SourceInst::Let { value, .. }
                        | SourceInst::Assign { value, .. }
                        | SourceInst::Expr(value) => check(value),
                        SourceInst::IndexAssign {
                            array,
                            index,
                            value,
                        } => {
                            check(array);
                            check(index);
                            check(value);
                        }
                        _ => {}
                    }
                }
                match &block.terminator {
                    SourceTerminator::Return(Some(value))
                    | SourceTerminator::Branch { cond: value, .. } => check(value),
                    _ => {}
                }
            }
        }
    }

    #[test]
    fn lifted_lambdas_exclusively_own_their_bodies() {
        let mut program = lir(
            "fn f() -> i64 { let apply = |x: i64| -> i64 { let next = |y: i64| y + 1; return next(x); }; return apply(10); }",
        );
        assert_eq!(program.lambdas.len(), 2);
        let mut constructions = 0;
        for function in program.functions.iter_mut().chain(
            program
                .lambdas
                .iter_mut()
                .map(|lambda| &mut lambda.function),
        ) {
            constructions += function
                .blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .filter(|inst| {
                    matches!(
                        inst,
                        SourceInst::Compute {
                            value: LirRvalue::Closure { .. },
                            ..
                        }
                    )
                })
                .count();
            function.visit_expr_roots_mut(|expr| {
                for node in expr.walk_postorder(true) {
                    if let HirExprKind::Lambda { body, .. } = &node.kind {
                        constructions += 1;
                        assert!(body.is_empty());
                    }
                }
            });
        }
        assert_eq!(constructions, 2);
    }

    /// Ordinary match arms also belong to the explicit control-flow graph.
    #[test]
    fn l35_non_suspending_match_is_split() {
        let p = lir(r#"
async fn f(which: i64) -> i64 {
    let mut out = 0;
    match which {
        1 => { out = 10; }
        _ => { out = 20; }
    }
    await yield();
    return out;
}
"#);
        let f = func(&p, "f");
        assert!(
            f.blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(
                    inst,
                    SourceInst::MatchTest { .. } | SourceInst::MatchBind { .. }
                ))
        );
    }

    // 37. a `lock` body and its continuation are emitted BEFORE the block
    // that holds the `let` they read (willow-34su).
    //
    // This is the shape that made lazy binding at `SourceInst::Let` wrong: the
    // assignment in the section and the read after it both land in blocks of
    // lower index than the `let`, so codegen reaches them first. The backend
    // answers it by binding every local at function entry; this pins the
    // premise, so a lowering change that reorders the blocks shows up here
    // rather than as a silent dependency the backend no longer needs.
    #[test]
    fn l37_lock_body_is_emitted_before_the_let_it_reads() {
        // The call is what makes the shape: an async body is split at its
        // preemption points and each continuation is APPENDED, so the block
        // holding everything after the `gc_collect()` — the `let` included —
        // is allocated after the section's own blocks.
        let p = lir(r#"
async fn peek(m: Mutex<i64>) -> i64 {
    gc_collect();
    let mut got = 0;
    lock m as value { got = value; }
    return got;
}
"#);
        let f = func(&p, "peek");
        let block_of = |pick: &dyn Fn(&SourceInst) -> bool| {
            f.blocks
                .iter()
                .position(|block| block.instrs.iter().any(pick))
                .expect("no block holds the instruction")
        };
        let binds = block_of(&|inst| matches!(inst, SourceInst::Let { name, .. } if name == "got"));
        let writes =
            block_of(&|inst| matches!(inst, SourceInst::Assign { name, .. } if name == "got"));
        assert!(
            writes < binds,
            "the section writes `got` in bb{writes} and binds it in bb{binds}; \
             the binding is no longer the later block"
        );
    }

    // 38. the same in a synchronous body: `if/else` lowers the merge block
    // before the `else` arm that jumps to it, so a block index has never been
    // a position in an order control can flow in (willow-ht1h, willow-fvt4).
    #[test]
    fn l38_if_else_merge_is_emitted_before_its_predecessor() {
        let p = lir("fn f(c: bool) -> i64 { if c { return 1; } else { print(2); } return 3; }");
        let f = func(&p, "f");
        let SourceTerminator::Branch { else_block, .. } = &f.blocks[0].terminator else {
            panic!("entry does not branch");
        };
        let merge = f
            .blocks
            .iter()
            .position(|block| {
                matches!(&block.terminator, SourceTerminator::Return(Some(v))
                    if matches!(v.kind, HirExprKind::Int(3)))
            })
            .expect("no merge block");
        assert!(
            merge < else_block.0,
            "merge is bb{merge} and the else arm bb{}; index order no longer \
             puts a block before its predecessor",
            else_block.0
        );
    }

    /// Synchronous matches use the same dispatch graph as async matches.
    #[test]
    fn l36_sync_match_is_split() {
        let p = lir(r#"
fn f(which: i64) -> i64 {
    match which {
        1 => { return 10; }
        _ => { return 20; }
    }
}
"#);
        let f = func(&p, "f");
        assert!(
            f.blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(
                    inst,
                    SourceInst::MatchTest { .. } | SourceInst::MatchBind { .. }
                ))
        );
    }
}

#[cfg(test)]
mod prune_and_corpus_tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn lir(src: &str) -> SourceProgram {
        let tokens = Lexer::new(src).tokenize().expect("lexing failed");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        let (hir, diags) = super::super::lower::lower_program(&program);
        assert!(diags.is_empty(), "HIR diagnostics: {diags:?}");
        lower_source_program(&hir)
    }

    // 23. dead blocks after mid-block returns are pruned
    #[test]
    fn l23_dead_blocks_pruned() {
        let p = lir("fn f(c: bool) -> i64 { if c { return 1; } return 2; }");
        let f = &p.functions[0];
        // Reachable shape: entry(branch) + then(return) + merge(return) = 3.
        assert_eq!(f.blocks.len(), 3, "{f:#?}");
        // Every edge stays in range after renumbering.
        for b in &f.blocks {
            match &b.terminator {
                SourceTerminator::Jump(t) => assert!(t.0 < f.blocks.len()),
                SourceTerminator::Branch {
                    then_block,
                    else_block,
                    ..
                } => {
                    assert!(then_block.0 < f.blocks.len());
                    assert!(else_block.0 < f.blocks.len());
                }
                SourceTerminator::Suspend { resume, .. } => assert!(resume.0 < f.blocks.len()),
                SourceTerminator::Return(_) | SourceTerminator::CleanupReturn => {}
            }
        }
    }

    // 24. block ids stay dense and self-consistent after pruning
    #[test]
    fn l24_pruned_ids_dense() {
        let p =
            lir("fn f(n: i64) -> i64 { if n > 0 { return 1; } if n < 0 { return -1; } return 0; }");
        let f = &p.functions[0];
        for (i, b) in f.blocks.iter().enumerate() {
            assert_eq!(b.id.0, i, "ids must be dense positions");
        }
    }

    // 25. corpus: every example/*.wi parses and survives HIR→LIR lowering
    // without panicking (coverage diagnostics are allowed; crashes are not).
    #[test]
    fn l25_examples_corpus_lowers_without_panic() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("example");
        let mut sources = Vec::new();
        collect_wi_files(&root, &mut sources);
        assert!(
            sources.len() > 30,
            "expected a real corpus, got {sources:?}"
        );

        let mut fully_covered = 0usize;
        for path in &sources {
            let text = std::fs::read_to_string(path).unwrap();
            let Ok(tokens) = Lexer::new(&text).tokenize() else {
                continue; // lexer-error fixtures are out of scope here
            };
            let (program, parse_errors) = Parser::new(tokens).parse();
            if !parse_errors.is_empty() {
                continue;
            }
            // Measure with the checker's side tables, as production lowering
            // does (checker errors are fine — import-using files won't fully
            // resolve here, and panic-safety is the primary assertion).
            let mut checker = crate::semantic::TypeChecker::new();
            crate::register_prelude(&mut checker).expect("prelude registers");
            checker.check_program(&program);
            let tables = super::super::lower::CheckerTables::from_checker(&checker);
            let (hir, diags) = super::super::lower::lower_program_with(&program, &tables);
            let _ = lower_source_program(&hir); // must not panic
            if diags.is_empty() {
                fully_covered += 1;
            }
        }
        // A healthy majority of the real examples should lower with no
        // coverage diagnostics; regressions here mean the HIR lost ground.
        assert!(
            fully_covered * 2 >= sources.len(),
            "only {fully_covered}/{} examples fully lowered",
            sources.len()
        );
    }

    fn collect_wi_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_wi_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "wi") {
                out.push(path);
            }
        }
    }
}

mod final_ir;
pub use final_ir::{
    LirBlock, LirDeferBody, LirFunction, LirInst, LirLambda, LirParam, LirPattern, LirProgram,
    Terminator,
};

pub fn lower_program(program: &HirProgram) -> LirProgram {
    final_ir::finish_program(lower_source_program(program))
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
