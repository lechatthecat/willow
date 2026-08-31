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
//! Expressions stay as typed [`HirExpr`] trees inside instructions; lowering
//! expression-level control flow (ternary, `match`, short-circuit operators)
//! into blocks is the backend slice's job. The backend consumes this IR for
//! every function whose lowering stays inside its supported subset
//! (`backend::cranelift::lir_gen`, willow-0g8j) and falls back to the AST
//! walker for the rest; `--emit-lir` renders it either way.

use crate::diagnostics::Span;
use crate::parser::ast::{LockMode, Type};
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::type_checker::types::{await_output_type, awaitable_task_type};

use super::typed_ast::{
    HirDeferBody, HirExpr, HirExprKind, HirFunction, HirParam, HirPattern, HirProgram, HirStmt,
};

pub mod async_liveness;

use async_liveness::LirAsyncFrameLayout;

/// A whole program in lowered IR.
#[derive(Debug, Clone, PartialEq)]
pub struct LirProgram {
    pub functions: Vec<LirFunction>,
    /// Lambda bodies, lifted out of the expressions that contain them
    /// (willow-0g8j.2.2). They are kept apart from `functions` because the LIR
    /// cannot name them: the backend assigns each lambda its `$lambda.N`
    /// symbol, so the pairing is by span and the name is filled in there.
    pub lambdas: Vec<LirLambda>,
}

/// One lifted lambda body, keyed by the span of the lambda expression it came
/// from — the same key the backend's own lambda table uses.
#[derive(Debug, Clone, PartialEq)]
pub struct LirLambda {
    pub span: Span,
    pub function: LirFunction,
}

/// One function as a basic-block graph. `blocks[0]` is the entry block.
#[derive(Debug, Clone, PartialEq)]
pub struct LirFunction {
    pub name: String,
    pub is_async: bool,
    pub params: Vec<HirParam>,
    pub return_type: Type,
    pub blocks: Vec<LirBlock>,
    /// Stable identities for parameters, source locals, and LIR-synthesized
    /// temporaries. Source spans are diagnostic metadata only.
    pub locals: Vec<LirLocal>,
    /// Frame ownership belongs to LIR: this is computed from the final CFG,
    /// after synthetic locals and explicit suspension edges exist.
    pub async_frame: LirAsyncFrameLayout,
}

impl LirFunction {
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

#[derive(Debug, Clone, PartialEq)]
pub struct LirLocal {
    pub id: LirLocalId,
    pub name: String,
    pub ty: Type,
    pub source_span: Option<Span>,
    pub synthetic: bool,
    pub parameter: bool,
}

/// A basic-block index into [`LirFunction::blocks`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockId(pub usize);

/// A straight-line run of instructions ended by exactly one terminator.
#[derive(Debug, Clone, PartialEq)]
pub struct LirBlock {
    pub id: BlockId,
    pub instrs: Vec<LirInst>,
    pub terminator: Terminator,
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

/// What a registered `defer` runs, carried through unchanged from the HIR
/// (willow-0g8j.2.3).
///
/// A deferred body is NOT lowered into blocks of its own: it runs at scope
/// exit, and every exit path — fallthrough, `return`, panic unwinding — has to
/// splice it in at a different place. Keeping it as a statement tree lets each
/// of those paths emit it where it belongs.
#[derive(Debug, Clone, PartialEq)]
pub enum LirDeferBody {
    Expr(HirExpr),
    Block(Vec<HirStmt>),
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
pub enum LirInst {
    /// Open a lexical scope that owns `sites` defer registrations
    /// (willow-0g8j.2.3).
    ///
    /// `defer` is a LEXICAL construct — a defer in a loop body runs once per
    /// iteration — but the LIR is a flat block graph with no scopes of its
    /// own, so the boundaries are instructions. Every scope opened here is
    /// closed by exactly one [`LirInst::LeaveDeferScope`] on the fallthrough
    /// path, and every early exit out of it is preceded by a
    /// [`LirInst::FlushDefers`].
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
    /// Close the scope opened by the matching [`LirInst::EnterDeferScope`]:
    /// run its registrations (newest first) and pop it. This is the
    /// FALLTHROUGH exit — an early exit uses [`LirInst::FlushDefers`] and
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
    /// Fallthrough only, exactly like [`LirInst::LeaveDeferScope`]: a `break`,
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
        body: LirDeferBody,
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
        class: String,
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
    /// [`LirInst::FlushDefers`] on either side of this instruction expresses.
    ///
    /// The panic path is deliberately absent: an unwind releases the lock from
    /// the section's cleanup block instead, so it needs no instruction of its
    /// own. Running twice is harmless — the release is guarded by the handle
    /// slot, which it clears.
    ReleaseLock(LirLockSlots),
    /// Does the scrutinee match one arm's pattern? (willow-0g8j.2.11.1)
    ///
    /// Emitted only when lowering has split a `match` into blocks because an
    /// arm suspends. A `match` whose arms all run to completion stays an
    /// [`HirExprKind::Match`] tree, so this is never the only way a pattern
    /// test reaches the backend.
    ///
    /// The scrutinee is a local rather than an expression because every arm
    /// tests the SAME value: the source evaluates it once, and the tests are
    /// spread over a chain of dispatch blocks. `result` is a `Bool` local, not
    /// a value, for the same reason [`Terminator::Branch`] reads one — the
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
pub enum Terminator {
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
}

/// Lower every function (free functions and class methods, flattened as
/// `Class::method`) of a typed-HIR program to basic blocks.
pub fn lower_program(program: &HirProgram) -> LirProgram {
    let mut functions = Vec::with_capacity(program.functions.len());
    let mut lambdas = Vec::new();
    for f in &program.functions {
        functions.push(lower_function(f, None));
        collect_lambdas(&f.body, &mut lambdas);
    }
    for c in &program.classes {
        for m in &c.methods {
            functions.push(lower_function(m, Some(&c.name)));
            collect_lambdas(&m.body, &mut lambdas);
        }
    }
    LirProgram { functions, lambdas }
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
fn collect_lambdas(body: &[HirStmt], out: &mut Vec<LirLambda>) {
    for stmt in body {
        for expr in stmt.child_exprs() {
            collect_lambdas_in_expr(expr, out);
        }
    }
}

fn collect_lambdas_in_expr(expr: &HirExpr, out: &mut Vec<LirLambda>) {
    for child in expr.children() {
        collect_lambdas_in_expr(child, out);
    }
    if let HirExprKind::Lambda { params, body } = &expr.kind {
        // The `fn(...) -> R` the checker gave the lambda expression is the only
        // place the return type lives: the HIR params carry their own types,
        // but a lambda has no declared return type node of its own.
        let Type::Fn(_, ret) = &expr.ty else {
            return;
        };
        let mut b = Builder::new(params, false);
        // A lifted lambda owns a function scope just like a named function.
        // Lowering only its statements would leave top-level defer sites
        // without the scope metadata that assigns their stable identities.
        b.lower_scope(body);
        let (blocks, locals) = b.finish();
        let function = LirFunction {
            name: lambda_placeholder_name(expr.span),
            is_async: false,
            params: params.clone(),
            return_type: (**ret).clone(),
            blocks,
            locals,
            async_frame: LirAsyncFrameLayout::default(),
        };
        function.assert_unique_local_names();
        out.push(LirLambda {
            span: expr.span,
            function,
        });
    }
}

/// The name a lifted lambda carries until the backend renames it to the
/// `$lambda.N` symbol it declared. Derived from the span so `--emit-lir` output
/// is stable and two lambdas never collide.
pub fn lambda_placeholder_name(span: Span) -> String {
    format!("$lambda@{}:{}", span.file_id.0, span.start)
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

impl LirDeferBody {
    /// The lowered form of the same question [`defer_body_contains_recover`]
    /// asks of the HIR. The backend has only this form by the time it emits
    /// the cleanup CFG, and the two must agree: lowering records the panic
    /// edge for a scope exactly when the backend arms recovery in it.
    pub fn contains_recover(&self) -> bool {
        match self {
            LirDeferBody::Expr(expr) => expr_has_recover(expr),
            LirDeferBody::Block(stmts) => block_has_recover(stmts),
        }
    }
}

fn expr_has_recover(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Call { callee, .. } if callee == "recover" => true,
        HirExprKind::Lambda { .. } => false,
        _ => expr.children().into_iter().any(expr_has_recover),
    }
}

fn block_has_recover(stmts: &[HirStmt]) -> bool {
    stmts
        .iter()
        .flat_map(HirStmt::child_exprs)
        .any(expr_has_recover)
}

/// Lower one function's statement tree into a block graph.
fn lower_function(f: &HirFunction, class: Option<&str>) -> LirFunction {
    let mut b = Builder::new(&f.params, f.is_async);
    b.lower_scope(&f.body);
    b.materialize_preemption_safepoints();
    // The fall-through end of a function is an implicit `return;` (the type
    // checker has already guaranteed value-returning paths return).
    let (blocks, locals) = b.finish();
    let name = match class {
        Some(class) => format!("{class}::{}", f.name),
        None => f.name.clone(),
    };
    let function = LirFunction {
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
    blocks: Vec<(Vec<LirInst>, Option<Terminator>)>,
    /// Per-block [`LirBlock::recovery`], filled in by `lower_scope` once it
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
    defer_scopes: Vec<std::collections::HashMap<Span, LirDeferId>>,
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
    /// [`LirFunction::assert_unique_local_names`] pins it.
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
        HirExprKind::Await { .. } | HirExprKind::Select { .. } => true,
        HirExprKind::MethodCall { object, method, .. } => {
            builtin_types::unary_arg(&object.ty, B::Channel).is_some()
                && matches!(method.as_str(), "send" | "recv")
        }
        _ => false,
    }
}

fn collect_suspensions<'a>(expr: &'a HirExpr, out: &mut Vec<&'a HirExpr>) {
    if matches!(expr.kind, HirExprKind::Lambda { .. }) {
        return;
    }
    for child in expr.children() {
        collect_suspensions(child, out);
    }
    // Children are evaluated before their enclosing expression. In
    // particular `await f(ch.recv())` must split the recv before the await.
    if expr_suspends_here(expr) {
        out.push(expr);
    }
}

/// Does any statement of this body suspend? Used to decide whether a `match`
/// has to become blocks: an arm that never returns control to the scheduler is
/// emittable as part of an expression tree, and staying a tree keeps the
/// existing `match` emission (willow-0g8j.2.11.1).
fn body_suspends(body: &[HirStmt]) -> bool {
    body.iter().flat_map(HirStmt::child_exprs).any(|expr| {
        let mut found = Vec::new();
        collect_suspensions(expr, &mut found);
        !found.is_empty()
    })
}

/// Does this body contain a statement that cannot remain inside a match arm's
/// HIR island? Lowering the enclosing match into the LIR graph lets the normal
/// statement path own place stores and defer scopes as well as loop blocks,
/// resolving `break`/`continue` against the loop stack active at the match site
/// (willow-o3xi).
fn body_needs_match_cfg(body: &[HirStmt]) -> bool {
    body.iter().any(stmt_needs_match_cfg)
}

fn stmt_needs_match_cfg(stmt: &HirStmt) -> bool {
    match stmt {
        HirStmt::While { .. }
        | HirStmt::For { .. }
        | HirStmt::Break { .. }
        | HirStmt::Continue { .. }
        | HirStmt::Defer { .. }
        | HirStmt::IndexAssign { .. }
        | HirStmt::StaticFieldAssign { .. } => true,
        HirStmt::If {
            then_branch,
            else_branch,
            ..
        } => {
            body_needs_match_cfg(then_branch)
                || else_branch.as_deref().is_some_and(body_needs_match_cfg)
        }
        HirStmt::Lock { body, .. } => body_needs_match_cfg(body),
        _ => stmt.child_exprs().into_iter().any(expr_needs_match_cfg),
    }
}

fn expr_needs_match_cfg(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Match { scrutinee, arms } => {
            expr_needs_match_cfg(scrutinee)
                || arms.iter().any(|arm| body_needs_match_cfg(&arm.body))
        }
        HirExprKind::Lambda { .. } => false,
        _ => expr.children().into_iter().any(expr_needs_match_cfg),
    }
}

/// The names a pattern binds, with the type each is bound at, in the order the
/// emitter destructures them (willow-0g8j.2.11.1).
///
/// A variant whose payloads are ALL `void` carries no word, so its bindings
/// name values that do not exist. They are deliberately absent here: nothing
/// declares a local for them, so an arm body that reads one finds no binding
/// and takes the function back to the AST emitter — the same outcome the
/// tree-shaped `match` already produces.
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

fn contains_expr(node: &HirExpr, target: &HirExpr) -> bool {
    std::ptr::eq(node, target)
        || node
            .children()
            .into_iter()
            .any(|child| contains_expr(child, target))
}

fn contains_suspend_span(node: &HirExpr, target_span: Span) -> bool {
    (node.span == target_span && expr_suspends_here(node))
        || node
            .children()
            .into_iter()
            .any(|child| contains_suspend_span(child, target_span))
}

fn hoistable_around(node: &HirExpr, target: &HirExpr, seen: &mut bool) -> bool {
    if std::ptr::eq(node, target) {
        *seen = true;
        return true;
    }
    if !contains_expr(node, target) {
        return *seen || rematerializable(node);
    }
    // A conditionally evaluated child cannot be pulled in front of the whole
    // expression, but the part that IS evaluated unconditionally can: a
    // ternary's condition, a `match` scrutinee, the left operand of `&&`/`||`.
    match &node.kind {
        HirExprKind::Ternary { condition, .. } if !contains_expr(condition, target) => {
            return false;
        }
        HirExprKind::Match { scrutinee, .. } if !contains_expr(scrutinee, target) => {
            return false;
        }
        HirExprKind::Binary {
            op: crate::parser::ast::BinOp::And | crate::parser::ast::BinOp::Or,
            lhs,
            ..
        } if !contains_expr(lhs, target) => {
            return false;
        }
        HirExprKind::Select { .. } | HirExprKind::Lambda { .. } => return false,
        _ => {}
    }
    node.children()
        .into_iter()
        .all(|child| hoistable_around(child, target, seen))
}

fn replace_suspension(expr: &HirExpr, target_span: Span, replacement: &HirExpr) -> HirExpr {
    if expr.span == target_span && expr_suspends_here(expr) {
        return replacement.clone();
    }
    let mut out = expr.clone();
    out.kind = match &expr.kind {
        HirExprKind::Binary { op, lhs, rhs } => HirExprKind::Binary {
            op: op.clone(),
            lhs: Box::new(replace_suspension(lhs, target_span, replacement)),
            rhs: Box::new(replace_suspension(rhs, target_span, replacement)),
        },
        HirExprKind::Unary { op, operand } => HirExprKind::Unary {
            op: op.clone(),
            operand: Box::new(replace_suspension(operand, target_span, replacement)),
        },
        HirExprKind::Call { callee, args } => HirExprKind::Call {
            callee: callee.clone(),
            args: args
                .iter()
                .map(|arg| replace_suspension(arg, target_span, replacement))
                .collect(),
        },
        HirExprKind::Print { value, newline } => HirExprKind::Print {
            value: Box::new(replace_suspension(value, target_span, replacement)),
            newline: *newline,
        },
        HirExprKind::Array { elements } => HirExprKind::Array {
            elements: elements
                .iter()
                .map(|element| replace_suspension(element, target_span, replacement))
                .collect(),
        },
        HirExprKind::Index { array, index } => HirExprKind::Index {
            array: Box::new(replace_suspension(array, target_span, replacement)),
            index: Box::new(replace_suspension(index, target_span, replacement)),
        },
        HirExprKind::Ternary {
            condition,
            then_expr,
            else_expr,
        } => HirExprKind::Ternary {
            condition: Box::new(replace_suspension(condition, target_span, replacement)),
            then_expr: Box::new(replace_suspension(then_expr, target_span, replacement)),
            else_expr: Box::new(replace_suspension(else_expr, target_span, replacement)),
        },
        HirExprKind::New { class, args } => HirExprKind::New {
            class: class.clone(),
            args: args
                .iter()
                .map(|arg| replace_suspension(arg, target_span, replacement))
                .collect(),
        },
        HirExprKind::FieldAccess { object, field } => HirExprKind::FieldAccess {
            object: Box::new(replace_suspension(object, target_span, replacement)),
            field: field.clone(),
        },
        HirExprKind::MethodCall {
            object,
            method,
            args,
        } => HirExprKind::MethodCall {
            object: Box::new(replace_suspension(object, target_span, replacement)),
            method: method.clone(),
            args: args
                .iter()
                .map(|arg| replace_suspension(arg, target_span, replacement))
                .collect(),
        },
        HirExprKind::ObjectLiteral { class, fields } => HirExprKind::ObjectLiteral {
            class: class.clone(),
            fields: fields
                .iter()
                .map(|(name, value)| {
                    (
                        name.clone(),
                        replace_suspension(value, target_span, replacement),
                    )
                })
                .collect(),
        },
        HirExprKind::StaticCall {
            class,
            method,
            args,
        } => HirExprKind::StaticCall {
            class: class.clone(),
            method: method.clone(),
            args: args
                .iter()
                .map(|arg| replace_suspension(arg, target_span, replacement))
                .collect(),
        },
        HirExprKind::ReferenceArg { place } => HirExprKind::ReferenceArg {
            place: Box::new(replace_suspension(place, target_span, replacement)),
        },
        HirExprKind::Range { start, end } => HirExprKind::Range {
            start: Box::new(replace_suspension(start, target_span, replacement)),
            end: Box::new(replace_suspension(end, target_span, replacement)),
        },
        HirExprKind::Await { inner } => HirExprKind::Await {
            inner: Box::new(replace_suspension(inner, target_span, replacement)),
        },
        HirExprKind::TryPropagate { inner } => HirExprKind::TryPropagate {
            inner: Box::new(replace_suspension(inner, target_span, replacement)),
        },
        HirExprKind::Match { scrutinee, arms } => HirExprKind::Match {
            scrutinee: Box::new(replace_suspension(scrutinee, target_span, replacement)),
            arms: arms.clone(),
        },
        _ => return out,
    };
    out
}

fn expression_executes_call(expr: &HirExpr) -> bool {
    match &expr.kind {
        HirExprKind::Call { .. }
        | HirExprKind::MethodCall { .. }
        | HirExprKind::StaticCall { .. }
        | HirExprKind::New { .. }
        | HirExprKind::ObjectLiteral { .. }
        | HirExprKind::Print { .. }
        | HirExprKind::Array { .. }
        | HirExprKind::Index { .. }
        | HirExprKind::Select { .. } => true,
        HirExprKind::Lambda { .. } => false,
        _ => expr.children().into_iter().any(expression_executes_call),
    }
}

fn instruction_executes_call(inst: &LirInst) -> bool {
    match inst {
        LirInst::Let { value, .. }
        | LirInst::Assign { value, .. }
        | LirInst::StaticFieldAssign { value, .. }
        | LirInst::Expr(value) => expression_executes_call(value),
        LirInst::FieldAssign { object, value, .. } => {
            expression_executes_call(object) || expression_executes_call(value)
        }
        LirInst::IndexAssign {
            array,
            index,
            value,
        } => {
            expression_executes_call(array)
                || expression_executes_call(index)
                || expression_executes_call(value)
        }
        LirInst::SuperInit { .. } => true,
        LirInst::Defer { body, .. } => match body {
            LirDeferBody::Expr(expr) => expression_executes_call(expr),
            LirDeferBody::Block(stmts) => stmts
                .iter()
                .flat_map(HirStmt::child_exprs)
                .any(expression_executes_call),
        },
        // The release calls the runtime, but it is compiler-owned bookkeeping
        // rather than a user statement, and a preemption edge in front of it
        // would park the task holding a lock it is one instruction from giving
        // back. The AST path emits its release from the defer unwinder, which
        // has no safepoint either.
        // A pattern test and its bindings are loads and integer compares on a
        // value the enclosing block already holds. No call, so no safepoint.
        LirInst::ReleaseLock { .. }
        | LirInst::EnterDeferScope { .. }
        | LirInst::LeaveDeferScope { .. }
        | LirInst::FlushDefers { .. }
        | LirInst::ClearScopeRoots { .. }
        | LirInst::MatchTest { .. }
        | LirInst::MatchBind { .. }
        | LirInst::SelectInit { .. }
        | LirInst::SelectProbe { .. }
        | LirInst::SelectPick { .. }
        | LirInst::SelectUnregister { .. }
        | LirInst::SelectCommit { .. } => false,
    }
}

impl Builder {
    fn new(params: &[HirParam], is_async: bool) -> Self {
        let mut builder = Self {
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
        self.push(LirInst::Let {
            local,
            name,
            mutable,
            span: source_span.unwrap_or(value.span),
            ty,
            value,
        });
    }

    fn push_synth_let(&mut self, name: &str, mutable: bool, value: HirExpr) -> LirLocalId {
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
                callee: callee.clone(),
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

    /// Split a root suspension into an explicit CFG edge. The operand is
    /// evaluated once into a synthetic local before parking; the optional
    /// result local is populated by the resume transition.
    fn lower_root_suspend(
        &mut self,
        value: &HirExpr,
        destination: Option<LirLocalId>,
    ) -> Option<Option<HirExpr>> {
        if let Some(lowered) = self.lower_match_arms_cfg(value, destination) {
            return Some(lowered);
        }
        if !self.is_async {
            return None;
        }
        let operation = match &value.kind {
            HirExprKind::Await { inner } => {
                if let HirExprKind::Call { callee, args } = &inner.kind {
                    match (callee.as_str(), args.as_slice()) {
                        ("sleep", [millis]) if value.ty == Type::Void => {
                            let name = self.synthetic_name("sleep_millis");
                            let millis = self.push_synth_let(&name, false, millis.clone());
                            SuspendOp::Sleep { millis }
                        }
                        ("yield", []) if value.ty == Type::Void => SuspendOp::Yield,
                        _ => {
                            let name = self.synthetic_name("task");
                            let task = self.push_synth_let(&name, false, (**inner).clone());
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
                    let name = self.synthetic_name("task");
                    let task = self.push_synth_let(&name, false, (**inner).clone());
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
                let channel_name = self.synthetic_name("channel");
                let channel = self.push_synth_let(&channel_name, false, (**object).clone());
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
                        let value_name = self.synthetic_name("send_value");
                        let sent = self.push_synth_let(&value_name, false, sent.clone());
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
        self.terminate(Terminator::Suspend { operation, resume });
        self.switch_to(resume);
        Some(destination.map(|local| self.local_expr(local, value.span)))
    }

    /// Split a `match` whose arm suspends or needs structural statement
    /// lowering into an explicit dispatch chain (willow-0g8j.2.11.1,
    /// willow-o3xi).
    ///
    /// `match` is an HIR EXPRESSION and stays a tree through lowering, so an
    /// `await`, a channel operation or a `select` written inside an arm would
    /// never become a [`Terminator::Suspend`]. This turns the whole `match`
    /// into blocks — scrutinee, one dispatch block per testable arm, one block
    /// per arm body, and a merge — so the arm body is lowered by the ordinary
    /// statement path and its suspension splits like any other.
    ///
    /// Deliberately narrow: a `match` whose arms neither suspend nor need this
    /// structural path stays a tree and keeps the existing emission. This is
    /// the same shape [`Builder::lower_conditional_branches`] gives a ternary,
    /// one arm wider.
    ///
    /// `None` when this is not such a `match`; otherwise the value the caller
    /// should use, which is `None` for a `void` match.
    fn lower_match_arms_cfg(
        &mut self,
        value: &HirExpr,
        destination: Option<LirLocalId>,
    ) -> Option<Option<HirExpr>> {
        let HirExprKind::Match { scrutinee, arms } = &value.kind else {
            return None;
        };
        if !arms.iter().any(|arm| {
            body_needs_match_cfg(&arm.body) || (self.is_async && body_suspends(&arm.body))
        }) {
            return None;
        }

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
                self.terminate(Terminator::Jump(arm_block));
            } else {
                let test_name = self.synthetic_name("match_test");
                let test = self.declare_local(test_name, Type::Bool, None, true, false);
                self.push(LirInst::MatchTest {
                    scrutinee: scrutinee_local,
                    pattern: arm.pattern.clone(),
                    result: test,
                    span: arm.span,
                });
                // The last arm falling through means the scrutinee matched
                // nothing; the merge keeps the result at its seeded value,
                // exactly as the tree-shaped `match` does.
                self.terminate(Terminator::Branch {
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
                .map(|(name, ty)| self.declare_local(name, ty, Some(arm.span), false, false))
                .collect();
            if !bindings.is_empty() {
                self.push(LirInst::MatchBind {
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
            self.terminate(Terminator::Jump(merge));

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
                if !body.iter().any(|s| matches!(s, HirStmt::Defer { .. })) =>
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

    fn lower_nested_suspend(&mut self, value: &HirExpr) -> Option<HirExpr> {
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
        let mut out = value.clone();
        out.kind = match &value.kind {
            HirExprKind::Print { value, newline } => HirExprKind::Print {
                value: Box::new(self.freeze_binary_prefix(value, target_span)?),
                newline: *newline,
            },
            HirExprKind::Binary { op, lhs, rhs } if contains_suspend_span(lhs, target_span) => {
                HirExprKind::Binary {
                    op: op.clone(),
                    lhs: Box::new(self.freeze_binary_prefix(lhs, target_span)?),
                    rhs: rhs.clone(),
                }
            }
            HirExprKind::Binary { op, lhs, rhs } if contains_suspend_span(rhs, target_span) => {
                let lhs = if rematerializable(lhs) {
                    (**lhs).clone()
                } else {
                    let name = self.synthetic_name("prefix");
                    let local = self.push_synth_let(&name, false, (**lhs).clone());
                    self.local_expr(local, lhs.span)
                };
                let rhs = if rhs.span == target_span && expr_suspends_here(rhs) {
                    (**rhs).clone()
                } else {
                    self.freeze_binary_prefix(rhs, target_span)
                        .unwrap_or_else(|| (**rhs).clone())
                };
                HirExprKind::Binary {
                    op: op.clone(),
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                }
            }
            _ => return None,
        };
        Some(out)
    }

    fn lower_conditional_suspend(&mut self, value: &HirExpr) -> Option<HirExpr> {
        let mut suspensions = Vec::new();
        collect_suspensions(value, &mut suspensions);
        if suspensions.is_empty() {
            return None;
        }

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
        self.terminate(Terminator::Branch {
            cond: condition,
            then_block,
            else_block,
        });

        self.switch_to(then_block);
        self.lower_value_into(result, then_expr);
        self.terminate(Terminator::Jump(merge));

        self.switch_to(else_block);
        self.lower_value_into(result, else_expr);
        self.terminate(Terminator::Jump(merge));

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
        self.push(LirInst::Assign {
            local: destination,
            name: local.name.clone(),
            value,
        });
    }

    fn lower_condition(&mut self, condition: &HirExpr) -> HirExpr {
        if !self.is_async {
            return condition.clone();
        }
        let name = self.synthetic_name("condition");
        let local = self.declare_local(name, Type::Bool, None, true, false);
        match self.lower_root_suspend(condition, Some(local)) {
            Some(Some(value)) => value,
            _ => self
                .lower_nested_suspend(condition)
                .unwrap_or_else(|| condition.clone()),
        }
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
                    let (result_ty, cancel_aware) =
                        awaitable_task_type(&task.ty).unwrap_or((Type::I64, false));
                    let binding_ty = await_output_type(&task.ty).unwrap_or(Type::I64);
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
            operations.push(operation);
        }

        self.push(LirInst::SelectInit {
            operations: operations.clone(),
        });
        let probe = self.new_block();
        let idle = self.new_block();
        let dispatch = self.new_block();
        let done = self.new_block();
        let case_blocks: Vec<_> = cases.iter().map(|_| self.new_block()).collect();
        self.terminate(Terminator::Jump(probe));

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
        self.push(LirInst::SelectProbe {
            operations: operations.clone(),
            ready: ready.clone(),
        });
        self.push(LirInst::SelectPick { ready, chosen });
        let chosen_expr = self.local_expr(chosen, span);
        self.terminate(Terminator::Branch {
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
            self.terminate(Terminator::Jump(case_blocks[default]));
        } else {
            self.terminate(Terminator::Suspend {
                operation: SuspendOp::SelectWait {
                    operations: operations.iter().filter_map(LirSelectOp::wait_op).collect(),
                },
                resume: probe,
            });
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
            self.terminate(Terminator::Branch {
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
            self.push(LirInst::SelectUnregister {
                operations: operations.clone(),
            });
            let success_name = self.synthetic_name("select_success");
            let success = self.declare_local(success_name, Type::Bool, None, true, false);
            self.push(LirInst::SelectCommit {
                operation: operations[index].clone(),
                success,
            });
            if matches!(operations[index], LirSelectOp::Send { .. }) {
                let body = self.new_block();
                self.terminate(Terminator::Branch {
                    cond: self.local_expr(success, case.span),
                    then_block: body,
                    else_block: probe,
                });
                self.switch_to(body);
            }
            self.lower_scope(&case.body);
            self.terminate(Terminator::Jump(done));
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

    fn push(&mut self, inst: LirInst) {
        self.blocks[self.current].0.push(inst);
    }

    /// Seal the current block. A block already sealed by an inner `return`
    /// keeps its first terminator (trailing unreachable code was appended to a
    /// fresh block by `terminate`).
    fn terminate(&mut self, terminator: Terminator) {
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
                    self.blocks[current].1 = Some(Terminator::Suspend {
                        operation: SuspendOp::Preempt,
                        resume,
                    });
                    current = resume.0;
                }
                self.blocks[current].0.push(inst);
            }
            let terminator = terminator.unwrap_or(Terminator::Return(None));
            let terminator_calls = match &terminator {
                Terminator::Branch { cond, .. } => expression_executes_call(cond),
                Terminator::Return(Some(value)) => expression_executes_call(value),
                _ => false,
            };
            if terminator_calls {
                let resume = self.new_block();
                self.block_recovery[resume.0] = recovery.clone();
                self.blocks[current].1 = Some(Terminator::Suspend {
                    operation: SuspendOp::Preempt,
                    resume,
                });
                current = resume.0;
            }
            self.blocks[current].1 = Some(terminator);
        }
    }

    fn finish(self) -> (Vec<LirBlock>, Vec<LirLocal>) {
        let locals = self.locals;
        let mut recovery = self.block_recovery;
        let blocks: Vec<LirBlock> = self
            .blocks
            .into_iter()
            .enumerate()
            .map(|(i, (instrs, terminator))| LirBlock {
                id: BlockId(i),
                instrs,
                terminator: terminator.unwrap_or(Terminator::Return(None)),
                recovery: std::mem::take(&mut recovery[i]),
            })
            .collect();
        (prune_unreachable(blocks), locals)
    }

    fn lower_stmts(&mut self, stmts: &[HirStmt]) {
        for stmt in stmts {
            self.lower_stmt(stmt);
        }
    }

    /// Lower a statement list as a lexical scope: if it registers any `defer`,
    /// bracket it with [`LirInst::EnterDeferScope`]/[`LirInst::LeaveDeferScope`]
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
        let sites: Vec<(LirDeferId, Span)> = stmts
            .iter()
            .filter_map(|s| match s {
                HirStmt::Defer { span, .. } => {
                    let id = LirDeferId(self.defer_counter);
                    self.defer_counter += 1;
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
        let scope = sites.iter().map(|(id, span)| (*span, *id)).collect();
        let enter_block = self.current;
        let enter_index = self.blocks[enter_block].0.len();
        // Only a scope that can actually swallow a panic continues at its
        // resume block; a scope whose defers merely run cleanup lets the panic
        // through, so it adds no edge.
        let recovers = stmts.iter().any(
            |stmt| matches!(stmt, HirStmt::Defer { body, .. } if defer_body_contains_recover(body)),
        );
        self.push(LirInst::EnterDeferScope {
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
        self.push(LirInst::LeaveDeferScope { sites });
        if self.is_async || recovers {
            // Recovery must branch to a real LIR continuation, not a backend-
            // invented block after the whole poll body has been emitted.
            let resume = self.new_block();
            self.terminate(Terminator::Jump(resume));
            let LirInst::EnterDeferScope {
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
            self.push(LirInst::ClearScopeRoots { locals });
        }
    }

    /// Drop the GC roots of every lexical scope an early exit is about to leave
    /// (willow-0g8j.3.3).
    ///
    /// `break` and `continue` jump out without passing the fallthrough close,
    /// so the boundary that close marks has to be re-stated here — the same
    /// reason [`LirInst::FlushDefers`] exists beside
    /// [`LirInst::LeaveDeferScope`]. `return` needs nothing, because the emitter
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
                self.push(LirInst::ReleaseLock(lock.slots));
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
            self.push(LirInst::FlushDefers { sites });
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
                    self.push(LirInst::Assign {
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
            } => self.push(LirInst::FieldAssign {
                object: object.clone(),
                field: field.clone(),
                value: value.clone(),
            }),
            HirStmt::IndexAssign {
                array,
                index,
                value,
                ..
            } => self.push(LirInst::IndexAssign {
                array: array.clone(),
                index: index.clone(),
                value: value.clone(),
            }),
            HirStmt::StaticFieldAssign {
                class,
                field,
                value,
                ..
            } => self.push(LirInst::StaticFieldAssign {
                class: class.clone(),
                field: field.clone(),
                value: value.clone(),
            }),
            HirStmt::SuperInit { args, span } => self.push(LirInst::SuperInit {
                args: args.clone(),
                span: *span,
            }),
            HirStmt::Expr(e) => {
                if let HirExprKind::Select { cases } = &e.kind
                    && self.is_async
                {
                    self.lower_select(cases, e.span);
                } else if self.lower_root_suspend(e, None).is_none() {
                    let value = self.lower_nested_suspend(e).unwrap_or_else(|| e.clone());
                    self.push(LirInst::Expr(value));
                }
            }
            HirStmt::Return { value, .. } => {
                // The returned value is computed BEFORE the defers run: a
                // deferred body can mutate what the expression reads.
                let value = if self.defer_depth == 0 {
                    value.as_ref().and_then(|value| {
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
                    })
                } else {
                    value.as_ref().and_then(|value| {
                        if value.ty == Type::Void {
                            if self.lower_root_suspend(value, None).is_none() {
                                let value = self
                                    .lower_nested_suspend(value)
                                    .unwrap_or_else(|| value.clone());
                                self.push(LirInst::Expr(value));
                            }
                            return None;
                        }

                        let name = self.synthetic_name("return");
                        let destination =
                            self.declare_local(name.clone(), value.ty.clone(), None, true, false);
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
                    })
                };
                self.flush_defers_down_to(0);
                self.terminate(Terminator::Return(value));
                // Anything after a return is unreachable; give it a fresh
                // predecessor-less block rather than corrupting this one.
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::Break { .. } => {
                let frame = *self.loop_stack.last().expect("break outside loop");
                self.flush_defers_down_to(frame.defer_depth);
                self.clear_scope_roots_down_to(frame.scope_depth);
                self.terminate(Terminator::Jump(frame.exit));
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::Defer { body, span } => {
                let id = self
                    .defer_scopes
                    .last()
                    .and_then(|scope| scope.get(span))
                    .copied()
                    .expect("defer outside its LIR scope");
                let body = match body {
                    HirDeferBody::Expr(e) => LirDeferBody::Expr(self.capture_defer_expr(id, e)),
                    HirDeferBody::Block(stmts) => LirDeferBody::Block(stmts.clone()),
                };
                self.push(LirInst::Defer {
                    id,
                    body,
                    span: *span,
                });
            }
            HirStmt::Continue { .. } => {
                let frame = *self.loop_stack.last().expect("continue outside loop");
                self.flush_defers_down_to(frame.defer_depth);
                self.clear_scope_roots_down_to(frame.scope_depth);
                self.terminate(Terminator::Jump(frame.next));
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
                self.terminate(Terminator::Branch {
                    cond,
                    then_block,
                    else_block,
                });

                self.switch_to(then_block);
                self.lower_scope(then_branch);
                self.terminate(Terminator::Jump(merge_block));

                if let Some(else_branch) = else_branch {
                    self.switch_to(else_block);
                    self.lower_scope(else_branch);
                    self.terminate(Terminator::Jump(merge_block));
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

                self.terminate(Terminator::Jump(header));
                self.switch_to(header);
                let cond = self.lower_condition(cond);
                self.terminate(Terminator::Branch {
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
                self.terminate(Terminator::Jump(backedge));

                if self.is_async {
                    self.switch_to(backedge);
                    self.terminate(Terminator::Suspend {
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
    /// [`LirInst::ReleaseLock`]: the fallthrough one is emitted here, and
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
        self.terminate(Terminator::Suspend {
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
        self.push(LirInst::ReleaseLock(slots));
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
                // body that grows or shrinks the array is observed, exactly as
                // on the AST path.
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
            // Both bounds are read ONCE, before the loop, exactly as
            // `emit_range_for_value` reads them on the AST path: a `Range<i64>`
            // is immutable, so re-reading them per iteration could only cost
            // loads, and hoisting the value itself keeps a call in the iterable
            // position from running twice.
            (_, Type::Generic(g, args)) if g == "Range" && args.first() == Some(&Type::I64) => {
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

        self.terminate(Terminator::Jump(header));
        self.switch_to(header);
        self.terminate(Terminator::Branch {
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
        self.terminate(Terminator::Jump(inc_block));

        self.switch_to(inc_block);
        let local = self.local_by_name[&i_name];
        self.push(LirInst::Assign {
            local,
            name: i_name.clone(),
            value: plus_one(i64_var(&i_name)),
        });
        if self.is_async {
            self.terminate(Terminator::Suspend {
                operation: SuspendOp::Preempt,
                resume: header,
            });
        } else {
            self.terminate(Terminator::Jump(header));
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
fn prune_unreachable(blocks: Vec<LirBlock>) -> Vec<LirBlock> {
    let mut reachable = vec![false; blocks.len()];
    let mut stack = vec![0usize];
    while let Some(i) = stack.pop() {
        if std::mem::replace(&mut reachable[i], true) {
            continue;
        }
        for inst in &blocks[i].instrs {
            if let LirInst::EnterDeferScope {
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
            Terminator::Jump(b) => stack.push(b.0),
            Terminator::Branch {
                then_block,
                else_block,
                ..
            } => {
                stack.push(then_block.0);
                stack.push(else_block.0);
            }
            Terminator::Suspend { resume, .. } => stack.push(resume.0),
            Terminator::Return(_) => {}
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
                if let LirInst::EnterDeferScope {
                    resume: Some(resume),
                    ..
                } = inst
                {
                    *resume = BlockId(remap[resume.0]);
                }
            }
            block.terminator = match block.terminator {
                Terminator::Jump(b) => Terminator::Jump(BlockId(remap[b.0])),
                Terminator::Branch {
                    cond,
                    then_block,
                    else_block,
                } => Terminator::Branch {
                    cond,
                    then_block: BlockId(remap[then_block.0]),
                    else_block: BlockId(remap[else_block.0]),
                },
                Terminator::Suspend { operation, resume } => Terminator::Suspend {
                    operation,
                    resume: BlockId(remap[resume.0]),
                },
                ret @ Terminator::Return(_) => ret,
            };
            block
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Text rendering (`--emit-lir`)
// ---------------------------------------------------------------------------

/// Render a lowered program as labeled basic blocks.
pub fn format_program(program: &LirProgram) -> String {
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

fn format_inst(inst: &LirInst) -> String {
    let e = super::dump::expr_text;
    match inst {
        LirInst::EnterDeferScope { sites, lock, .. } => {
            let owns = match lock {
                Some(slots) => format!(", holds {}", slots.mode.keyword()),
                None => String::new(),
            };
            format!("enter defer scope ({} sites{owns});", sites.len())
        }
        LirInst::LeaveDeferScope { sites } => {
            format!("leave defer scope ({} sites);", sites.len())
        }
        LirInst::FlushDefers { sites } => format!("flush defers ({} sites);", sites.len()),
        LirInst::ClearScopeRoots { locals } => {
            let names = locals
                .iter()
                .map(|l| format!("l{}", l.0))
                .collect::<Vec<_>>()
                .join(", ");
            format!("clear scope roots ({names});")
        }
        LirInst::ReleaseLock(slots) => format!("release {};", slots.mode.keyword()),
        LirInst::Defer { body, .. } => match body {
            LirDeferBody::Expr(call) => format!("defer {};", e(call)),
            LirDeferBody::Block(stmts) => format!("defer {{ .. }} ({} stmts);", stmts.len()),
        },
        LirInst::Let {
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
        LirInst::Assign { name, value, .. } => format!("{name} = {};", e(value)),
        LirInst::FieldAssign {
            object,
            field,
            value,
        } => format!("{}.{field} = {};", e(object), e(value)),
        LirInst::IndexAssign {
            array,
            index,
            value,
        } => format!("{}[{}] = {};", e(array), e(index), e(value)),
        LirInst::StaticFieldAssign {
            class,
            field,
            value,
        } => format!("{class}::{field} = {};", e(value)),
        LirInst::SuperInit { args, .. } => {
            let args = args.iter().map(e).collect::<Vec<_>>().join(", ");
            format!("super.init({args});")
        }
        LirInst::MatchTest {
            scrutinee, result, ..
        } => format!("l{} = match.test l{};", result.0, scrutinee.0),
        LirInst::MatchBind {
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
        LirInst::SelectInit { .. } => "select.init;".to_string(),
        LirInst::SelectProbe { .. } => "select.probe;".to_string(),
        LirInst::SelectPick { .. } => "select.pick;".to_string(),
        LirInst::SelectUnregister { .. } => "select.unregister;".to_string(),
        LirInst::SelectCommit { .. } => "select.commit;".to_string(),
        LirInst::Expr(expr) => format!("{};", e(expr)),
    }
}

fn format_terminator(t: &Terminator) -> String {
    let e = super::dump::expr_text;
    match t {
        Terminator::Jump(b) => format!("jump bb{}", b.0),
        Terminator::Branch {
            cond,
            then_block,
            else_block,
        } => format!("branch {} bb{} bb{}", e(cond), then_block.0, else_block.0),
        Terminator::Suspend { operation, resume } => {
            format!("suspend {operation:?} -> bb{}", resume.0)
        }
        Terminator::Return(Some(v)) => format!("return {}", e(v)),
        Terminator::Return(None) => "return".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    /// Parse + HIR-lower + LIR-lower; assert no HIR diagnostics.
    fn lir(src: &str) -> LirProgram {
        let tokens = Lexer::new(src).tokenize().expect("lexing failed");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        let (hir, diags) = super::super::lower::lower_program(&program);
        assert!(diags.is_empty(), "HIR diagnostics: {diags:?}");
        lower_program(&hir)
    }

    fn func<'a>(p: &'a LirProgram, name: &str) -> &'a LirFunction {
        p.functions
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("no function {name}"))
    }

    // 1. a straight-line body is a single entry block
    #[test]
    fn l01_straight_line_single_block() {
        let p = lir("fn f() { let a = 1; print(a); }");
        let f = func(&p, "f");
        // The two statements, plus the body scope's root close
        // (willow-0g8j.3.3).
        assert_eq!(f.blocks[0].instrs.len(), 3);
        assert_eq!(f.blocks[0].terminator, Terminator::Return(None));
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
            Terminator::Return(Some(v)) if matches!(v.kind, HirExprKind::Int(7))
        ));
    }

    // 4. an empty function still gets an implicit `return`
    #[test]
    fn l04_empty_fn_implicit_return() {
        let p = lir("fn f() { }");
        assert_eq!(func(&p, "f").blocks[0].terminator, Terminator::Return(None));
    }

    // 5. `if` without else: entry branches then/merge, then jumps to merge
    #[test]
    fn l05_if_without_else_shape() {
        let p = lir("fn f(c: bool) { if c { print(1); } print(2); }");
        let f = func(&p, "f");
        let Terminator::Branch {
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
            Terminator::Jump(*else_block)
        );
        // The merge block holds the trailing statement.
        assert_eq!(f.blocks[else_block.0].instrs.len(), 1);
    }

    // 6. `if`/`else`: both arms jump to the same merge block
    #[test]
    fn l06_if_else_merges() {
        let p = lir("fn f(c: bool) { if c { print(1); } else { print(2); } print(3); }");
        let f = func(&p, "f");
        let Terminator::Branch {
            then_block,
            else_block,
            ..
        } = &f.blocks[0].terminator
        else {
            panic!("entry must branch");
        };
        let Terminator::Jump(merge_a) = f.blocks[then_block.0].terminator else {
            panic!("then must jump to merge");
        };
        let Terminator::Jump(merge_b) = f.blocks[else_block.0].terminator else {
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
        let Terminator::Branch { cond, .. } = &f.blocks[0].terminator else {
            panic!("entry must branch");
        };
        assert_eq!(cond.ty, Type::Bool);
    }

    // 8. `while`: entry jumps to a header that branches body/exit
    #[test]
    fn l08_while_header_shape() {
        let p = lir("fn f(c: bool) { while c { print(1); } }");
        let f = func(&p, "f");
        let Terminator::Jump(header) = f.blocks[0].terminator else {
            panic!("entry must jump to the loop header");
        };
        let Terminator::Branch {
            then_block: body, ..
        } = &f.blocks[header.0].terminator
        else {
            panic!("header must branch");
        };
        // The body jumps back to the header (the loop backedge).
        assert_eq!(f.blocks[body.0].terminator, Terminator::Jump(header));
    }

    // 9. the `while` condition lives in the header, not the entry block
    #[test]
    fn l09_while_cond_in_header() {
        let p = lir("fn f(a: i64) { while a > 0 { print(1); } }");
        let f = func(&p, "f");
        assert!(matches!(f.blocks[0].terminator, Terminator::Jump(_)));
        let Terminator::Jump(header) = f.blocks[0].terminator else {
            unreachable!()
        };
        assert!(matches!(
            f.blocks[header.0].terminator,
            Terminator::Branch { .. }
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
                LirInst::Let { name, .. } => Some(name.as_str()),
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
        let Terminator::Jump(header) = f.blocks[0].terminator else {
            unreachable!()
        };
        let Terminator::Branch {
            then_block: body, ..
        } = &f.blocks[header.0].terminator
        else {
            panic!("header must branch");
        };
        let body = &f.blocks[body.0];
        assert!(matches!(
            &body.instrs[0],
            LirInst::Let { name, .. } if name == "i"
        ));
        // The increment lives in a dedicated block (the `continue` target,
        // willow-kzka): body jumps to it, and it assigns the induction var.
        let Terminator::Jump(inc) = body.terminator else {
            panic!("body must jump to the increment block");
        };
        let inc = &f.blocks[inc.0];
        assert!(matches!(
            inc.instrs.last(),
            Some(LirInst::Assign { name, .. }) if name == "__for0_i"
        ));
        assert!(matches!(inc.terminator, Terminator::Jump(h) if h == header));
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
                LirInst::Let { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"__for0_arr"), "{names:?}");
        assert!(names.contains(&"__for0_i"), "{names:?}");
        // The length is NOT hoisted into a `let`.
        assert!(!names.contains(&"__for0_len"), "{names:?}");
        let Terminator::Jump(header) = f.blocks[0].terminator else {
            unreachable!()
        };
        let Terminator::Branch {
            cond,
            then_block: body,
            ..
        } = &f.blocks[header.0].terminator
        else {
            panic!("header must branch");
        };
        // The bound is a fresh `__for0_arr.len()` call in the header itself.
        let HirExprKind::Binary { rhs, .. } = &cond.kind else {
            panic!("header condition must be `i < bound`");
        };
        assert!(
            matches!(&rhs.kind, HirExprKind::MethodCall { method, .. } if method == "len"),
            "{:?}",
            rhs.kind
        );
        // v = __for0_arr[__for0_i], typed with the element type.
        let LirInst::Let { name, value, .. } = &f.blocks[body.0].instrs[0] else {
            panic!("body must bind the loop variable first");
        };
        assert_eq!(name, "v");
        assert!(matches!(value.kind, HirExprKind::Index { .. }));
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
                LirInst::Let { name, .. } => Some(name.clone()),
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
                Terminator::Jump(_)
                | Terminator::Branch { .. }
                | Terminator::Suspend { .. }
                | Terminator::Return(_) => {}
            }
        }
        // The then-arm's return survives as a Return terminator.
        let Terminator::Branch { then_block, .. } = &f.blocks[0].terminator else {
            panic!("entry must branch");
        };
        assert!(matches!(
            f.blocks[then_block.0].terminator,
            Terminator::Return(Some(_))
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
                LirInst::Let { name, .. } => format!("let {name}"),
                LirInst::Expr(_) => "expr".to_string(),
                LirInst::ClearScopeRoots { .. } => "clear roots".to_string(),
                _ => "other".to_string(),
            })
            .collect();
        assert_eq!(kinds, ["let a", "let b", "expr", "clear roots"]);
    }

    // 16. field/index/static assignments lower to their instructions
    #[test]
    fn l16_assignment_instructions() {
        let p = lir("class C { x: i64; static mut t: i64 = 0; } \
             fn f() { let p = new C(1); p.x = 2; let xs = [1]; xs[0] = 9; C::t = 5; }");
        let f = func(&p, "f");
        let instrs = &f.blocks[0].instrs;
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, LirInst::FieldAssign { .. }))
        );
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, LirInst::IndexAssign { .. }))
        );
        assert!(
            instrs
                .iter()
                .any(|i| matches!(i, LirInst::StaticFieldAssign { .. }))
        );
    }

    // 17. class methods are flattened as `Class::method`
    #[test]
    fn l17_class_methods_flattened() {
        let p = lir("class Box { pub v: i64; pub fn get(self) -> i64 { return self.v; } }");
        assert!(p.functions.iter().any(|f| f.name == "Box::get"));
    }

    // 18. a constructor flattens as `Class::init` and keeps super.init
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
                .any(|i| matches!(i, LirInst::SuperInit { .. }))
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
        let Terminator::Jump(header) = f.blocks[0].terminator else {
            unreachable!()
        };
        // Some block jumps back to the header — the loop backedge survives the
        // nested if's merge.
        let backedges = f
            .blocks
            .iter()
            .filter(|b| b.id != BlockId(0) && b.terminator == Terminator::Jump(header))
            .count();
        assert!(backedges >= 1);
    }

    // 21. the LIR text dump renders labeled blocks and terminators
    #[test]
    fn l21_text_dump_shape() {
        let p = lir("fn f(c: bool) -> i64 { if c { return 1; } return 2; }");
        let text = format_program(&p);
        assert!(text.contains("bb0:"), "{text}");
        assert!(text.contains("branch c: bool bb"), "{text}");
        assert!(text.contains("return 1: i64"), "{text}");
    }

    // 22. expression-level control flow (ternary/match) stays in instructions
    #[test]
    fn l22_expression_control_flow_stays_in_tree() {
        let p = lir("fn f(c: bool) -> i64 { return c ? 1 : 2; }");
        let f = func(&p, "f");
        // A single block: the ternary is a value inside the return, not blocks.
        assert!(matches!(
            &f.blocks[0].terminator,
            Terminator::Return(Some(v)) if matches!(v.kind, HirExprKind::Ternary { .. })
        ));
    }

    #[test]
    fn l26_async_await_is_an_explicit_suspend_edge() {
        let p = lir("async fn f() { await sleep(1); print(2); }");
        let f = func(&p, "f");
        assert!(f.is_async);
        assert!(f.blocks.iter().any(|block| matches!(
            block.terminator,
            Terminator::Suspend {
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
                .any(|inst| matches!(inst, LirInst::SelectProbe { .. }))
        }));
        assert!(f.blocks.iter().any(|block| {
            block
                .instrs
                .iter()
                .any(|inst| matches!(inst, LirInst::SelectPick { .. }))
        }));
        assert!(f.blocks.iter().any(|block| {
            block
                .instrs
                .iter()
                .any(|inst| matches!(inst, LirInst::SelectUnregister { .. }))
        }));
        assert!(f.blocks.iter().any(|block| {
            block
                .instrs
                .iter()
                .any(|inst| matches!(inst, LirInst::SelectCommit { .. }))
        }));
        assert!(f.blocks.iter().any(|block| matches!(
            block.terminator,
            Terminator::Suspend {
                operation: SuspendOp::SelectWait { .. },
                ..
            }
        )));
        assert!(f.blocks.iter().any(|block| matches!(
            block.terminator,
            Terminator::Suspend {
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
                    .any(|inst| matches!(inst, LirInst::FlushDefers { .. }))
            })
            .expect("return block must flush its defer");
        let flush_index = block
            .instrs
            .iter()
            .position(|inst| matches!(inst, LirInst::FlushDefers { .. }))
            .unwrap();
        let (return_local, return_name) = block.instrs[..flush_index]
            .iter()
            .find_map(|inst| match inst {
                LirInst::Let {
                    local, name, value, ..
                } if name.starts_with("__async_return_")
                    && matches!(value.kind, HirExprKind::Binary { .. }) =>
                {
                    Some((*local, name.as_str()))
                }
                _ => None,
            })
            .expect("the complete return expression must be stored before flushing defers");
        assert!(matches!(
            &block.terminator,
            Terminator::Return(Some(HirExpr {
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
                .filter(|inst| matches!(inst, LirInst::MatchTest { .. }))
                .count(),
            1
        );
        assert!(
            f.blocks
                .iter()
                .any(|block| matches!(block.terminator, Terminator::Branch { .. }))
        );
        // Both arms suspend, so each one ends up behind its own suspend edge.
        assert_eq!(
            f.blocks
                .iter()
                .filter(|block| matches!(
                    block.terminator,
                    Terminator::Suspend {
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
                LirInst::MatchTest { scrutinee, .. } => Some(*scrutinee),
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
                .any(|inst| matches!(inst, LirInst::MatchTest { .. }))
        );
        assert_eq!(f.blocks[0].terminator, Terminator::Jump(BlockId(1)));
        // The catch-all still binds the whole scrutinee.
        assert!(
            f.blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(inst, LirInst::MatchBind { .. }))
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
                LirInst::MatchBind { bindings, .. } => Some(bindings.clone()),
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

    /// A `match` no arm of which suspends keeps its arms in the HIR tree. The
    /// split exists to give a suspension a resume point, and paying for blocks
    /// where nothing suspends would cost the walker its cheaper shape.
    #[test]
    fn l35_non_suspending_match_is_not_split() {
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
            !f.blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(inst, LirInst::MatchTest { .. } | LirInst::MatchBind { .. }))
        );
    }

    /// A synchronous function never suspends, so the split never applies to
    /// one however its arms are written.
    #[test]
    fn l36_sync_match_is_never_split() {
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
            !f.blocks
                .iter()
                .flat_map(|block| &block.instrs)
                .any(|inst| matches!(inst, LirInst::MatchTest { .. } | LirInst::MatchBind { .. }))
        );
    }
}

#[cfg(test)]
mod prune_and_corpus_tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn lir(src: &str) -> LirProgram {
        let tokens = Lexer::new(src).tokenize().expect("lexing failed");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "parse errors: {errs:?}");
        let (hir, diags) = super::super::lower::lower_program(&program);
        assert!(diags.is_empty(), "HIR diagnostics: {diags:?}");
        lower_program(&hir)
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
                Terminator::Jump(t) => assert!(t.0 < f.blocks.len()),
                Terminator::Branch {
                    then_block,
                    else_block,
                    ..
                } => {
                    assert!(then_block.0 < f.blocks.len());
                    assert!(else_block.0 < f.blocks.len());
                }
                Terminator::Suspend { resume, .. } => assert!(resume.0 < f.blocks.len()),
                Terminator::Return(_) => {}
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
            let _ = lower_program(&hir); // must not panic
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
