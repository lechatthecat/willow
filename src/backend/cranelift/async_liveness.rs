//! Live-across-await analysis for eager async frames (willow-lpn.10).
//!
//! An `async fn` frame-backs bindings so they stay reachable while the task is
//! parked. Before this analysis the backend framed EVERY GC-managed binding,
//! which is sound but over-allocates: a local that is dead by the time the
//! function reaches its first `await` never needs a frame slot at all, and a
//! function with no `await` needs no frame.
//!
//! [`live_across_await`] answers which binding VALUES are live at a suspension
//! point. A non-GC binding outside that set can fall back to an ordinary SSA
//! value or stack slot. GC-managed bindings remain frame-backed regardless of
//! liveness until willow-p42j teaches cooperative poll functions to unwind and
//! restore their shadow-stack roots on every exit/resume edge.
//!
//! # Why a textual rule would be wrong
//!
//! `while c { use(x); await t; }` uses `x` textually BEFORE the await, but the
//! back edge means that use executes AFTER the await on every iteration but the
//! last — so `x` is live across it. Any rule that reads the body once, in
//! source order, gets this wrong in the unsafe direction. The pass therefore
//! runs a real backward dataflow with a fixpoint over loop bodies.
//!
//! # Why an incomplete traversal cannot go wrong
//!
//! Under-approximating liveness drops a GC root and produces a use-after-free
//! that only shows up under collection pressure, so the pass is built so that
//! every way of being wrong over-approximates instead:
//!
//! * The `Stmt` and `Expr` matches are EXHAUSTIVE — no `_` arm. A new AST
//!   variant is a compile error here, not a silently missed use.
//! * Constructs whose control flow or capture set this pass does not model
//!   (`lambda`, `select`, `defer`) lower to [`Lower::opaque`], which marks every
//!   binding currently in scope as both used and live across a suspension. That
//!   is exactly the pre-willow-lpn.10 behaviour, applied locally.
//! * Bindings are keyed by the SPAN of their declaration, never by name, so a
//!   shadowing inner `let` gets its own slot (willow-lpn.11) and can never be
//!   confused with the outer binding it hides.
//!
//! # What counts as a suspension point
//!
//! Not just `await`. A cooperative poll fn returns to the scheduler at:
//!
//! * `await <task>` and `await sleep(n)`,
//! * `ch.recv()` on an empty channel and a bare `ch.send(v)` on a full one,
//! * `select`,
//! * and — the one that is easy to miss — a PREEMPTION SAFEPOINT, which
//!   `emit_coop_stmts` plants before EVERY statement and which returns
//!   `COOP_POLL_PREEMPTED` when the check trips.
//!
//! The last of those dominates the result: with a safepoint at every statement
//! boundary, a local is live across a suspension unless it is never read after
//! the statement that declares it. Narrowing is therefore nearly vacuous on the
//! cooperative path today, and would only pay off if safepoints were placed at
//! loop back edges and calls instead of at every statement. The analysis models
//! them anyway, because leaving them out is a use-after-free.
//!
//! Setting `WILLOW_ASYNC_FRAME_ALL=1` restores the old frame-everything
//! behaviour; the caller checks it, and it exists so a suspected rooting bug can
//! be bisected against this pass without rebuilding an older compiler.

use std::collections::{HashMap, HashSet};

use crate::diagnostics::Span;
use crate::parser::ast::{
    Block, DeferBody, Expr, LambdaBody, MatchBody, Param, Pattern, SelectCaseKind, Stmt,
};

/// One node of the simplified control-flow program the backward pass walks.
///
/// Lowering the AST to this handful of shapes keeps the dataflow itself small
/// enough to check by eye; everything subtle lives in [`Lower`], which decides
/// what each AST construct becomes.
#[derive(Debug)]
enum Node {
    /// The binding declared at this span is read here.
    Use(Span),
    /// The binding declared at this span is (re)defined here, killing whatever
    /// it held before.
    Def(Span),
    /// A suspension point: everything live here needs a frame slot.
    Await,
    /// Jump to the exit of the innermost enclosing loop.
    Break,
    /// Jump to the head of the innermost enclosing loop.
    Continue,
    /// Leave the function; nothing after this point executes.
    Return,
    Seq(Vec<Node>),
    /// Alternative paths, e.g. `if`/`else` arms. Liveness merges by union, and
    /// a fall-through path is represented by an empty branch.
    Branch(Vec<Vec<Node>>),
    /// A loop body, including its condition. Solved by fixpoint so that the back
    /// edge is accounted for.
    Loop(Vec<Node>),
}

/// Which sources of suspension the lowering models.
///
/// Language-level suspensions (`await`, `recv`/`send`, `select`) are always
/// modelled. Preemption safepoints are a property of the CODE GENERATOR, not of
/// the language, so they are a separate switch: the backend always turns them on
/// because `emit_coop_stmts` always emits them, and the unit tests turn them off
/// to exercise the dataflow core at full resolution. If safepoint placement is
/// ever narrowed to loop back edges and calls, this is the knob that follows it.
#[derive(Clone, Copy)]
pub(crate) struct SuspendModel {
    /// A preemption check before every statement and at every loop back edge,
    /// each of which can return `COOP_POLL_PREEMPTED` from the poll fn.
    statement_safepoints: bool,
}

impl SuspendModel {
    /// What the cooperative backend actually emits.
    pub(crate) const COOPERATIVE: Self = Self {
        statement_safepoints: true,
    };

    /// Only the suspensions the source program spells out.
    #[cfg(test)]
    pub(crate) const EXPLICIT_ONLY: Self = Self {
        statement_safepoints: false,
    };
}

/// What the backend needs to know to decide which bindings get a frame slot.
#[derive(Default)]
pub(crate) struct FrameAnalysis {
    /// Bindings that are live across at least one suspension: their VALUE has to
    /// survive, so they must be frame-backed.
    pub(crate) live: HashSet<Span>,
    /// Bindings that were already declared when some suspension executed, i.e.
    /// whose lexical scope contains a suspension point.
    ///
    /// This is a weaker condition than [`Self::live`] and records the exact
    /// suspension/scope relationship for diagnostics and the future
    /// willow-p42j root-restoration work. It is NOT sufficient to decide that a
    /// GC local may leave the frame: cooperative codegen also fails to unwind
    /// roots at inner-scope exit and at the terminal Ready return. Current
    /// backend policy therefore frame-backs every GC-managed local regardless
    /// of this set.
    // Production keeps this fact ready for willow-p42j, while the current safe
    // policy frame-backs every GC local before it needs to consult the set.
    // Unit tests read it directly to pin the scope analysis.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) scoped_over_suspend: HashSet<Span>,
}

/// Run both analyses over one function body in a single lowering.
pub(crate) fn analyze(params: &[Param], body: &Block) -> FrameAnalysis {
    analyze_with(params, body, SuspendModel::COOPERATIVE)
}

pub(crate) fn analyze_with(params: &[Param], body: &Block, model: SuspendModel) -> FrameAnalysis {
    let mut lower = Lower::new(params, model);
    let mut nodes = Vec::new();
    lower.block(body, &mut nodes);

    let mut backward = Backward {
        framed: lower.forced.clone(),
    };
    let mut live = HashSet::new();
    backward.run(&nodes, &mut live, &LoopCtx::default());
    FrameAnalysis {
        live: backward.framed,
        scoped_over_suspend: lower.scoped_over_suspend,
    }
}

/// Spans of the bindings that are live across at least one `await`.
///
/// The result is a superset of the true answer; see the module docs for the
/// places where it deliberately over-approximates. Spans of things that are not
/// frame slots (loop variables, `match` arm bindings, `select` bindings) can
/// appear in it — callers intersect with their own slot list.
///
/// The backend calls [`analyze`] directly (it needs both sets); this projection
/// exists so the unit tests can talk about liveness on its own.
#[cfg(test)]
pub(crate) fn live_across_await(
    params: &[Param],
    body: &Block,
    model: SuspendModel,
) -> HashSet<Span> {
    analyze_with(params, body, model).live
}

/// Does this node list contain a suspension anywhere, including inside nested
/// branches and loops?
fn contains_await(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| match node {
        Node::Await => true,
        Node::Use(_) | Node::Def(_) | Node::Break | Node::Continue | Node::Return => false,
        Node::Seq(inner) | Node::Loop(inner) => contains_await(inner),
        Node::Branch(arms) => arms.iter().any(|arm| contains_await(arm)),
    })
}

/// Every binding span the lowering can see, live or not.
///
/// This is what `WILLOW_ASYNC_FRAME_ALL=1` substitutes for
/// [`live_across_await`], so the escape hatch reuses the same traversal and
/// cannot drift away from it.
pub(crate) fn all_binding_spans(params: &[Param], body: &Block) -> HashSet<Span> {
    let mut lower = Lower::new(params, SuspendModel::COOPERATIVE);
    let mut nodes = Vec::new();
    lower.block(body, &mut nodes);
    let mut out: HashSet<Span> = params.iter().map(|p| p.span).collect();
    collect_spans(&nodes, &mut out);
    out
}

// ── phase A: AST -> Node ───────────────────────────────────────────────────

struct Lower {
    /// Name → declaring span, innermost scope last. Uses resolve through this,
    /// so a shadowed outer binding is unreachable by name exactly as it is at
    /// run time.
    scopes: Vec<HashMap<String, Span>>,
    /// Bindings that must be framed regardless of liveness because the code
    /// generator writes them through a frame offset (see [`Lower::stmt`]).
    forced: HashSet<Span>,
    /// Every binding that was in scope at some suspension point; see
    /// [`FrameAnalysis::scoped_over_suspend`].
    scoped_over_suspend: HashSet<Span>,
    model: SuspendModel,
}

impl Lower {
    fn new(params: &[Param], model: SuspendModel) -> Self {
        let mut scope = HashMap::new();
        for p in params {
            scope.insert(p.name.clone(), p.span);
        }
        Self {
            scopes: vec![scope],
            forced: HashSet::new(),
            scoped_over_suspend: HashSet::new(),
            model,
        }
    }

    fn declare(&mut self, name: &str, span: Span) {
        // `_` is the wildcard binding: evaluated, never readable.
        if name == "_" {
            return;
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_string(), span);
        }
    }

    fn resolve(&self, name: &str) -> Option<Span> {
        self.scopes.iter().rev().find_map(|s| s.get(name).copied())
    }

    /// Every binding currently in scope, including ones hidden by shadowing.
    /// Only used by [`Self::opaque`], where being generous is the point.
    fn all_visible(&self) -> Vec<Span> {
        self.scopes
            .iter()
            .flat_map(|s| s.values().copied())
            .collect()
    }

    /// Give up on modelling a construct: treat it as reading every binding in
    /// scope and as being able to suspend. Bracketing the `Await` with the uses
    /// makes the bindings live on both sides, so they survive the backward pass
    /// regardless of what follows.
    fn opaque(&mut self, out: &mut Vec<Node>) {
        let visible = self.all_visible();
        out.extend(visible.iter().map(|s| Node::Use(*s)));
        self.mark_suspend();
        out.push(Node::Await);
        out.extend(visible.into_iter().map(Node::Use));
    }

    /// Record that a suspension executes here, so every binding currently in
    /// scope is holding a shadow-stack root across a poll-fn return. Called at
    /// every `Node::Await` emission site.
    fn mark_suspend(&mut self) {
        let visible = self.all_visible();
        self.scoped_over_suspend.extend(visible);
    }

    /// Emit a suspension whose OPERANDS are read on both sides of it.
    ///
    /// `await t` does not just evaluate `t` and stop: after the resume the poll
    /// fn reads the awaited task again to pull its result out, and `ch.recv()`
    /// re-reads the channel when it wakes. Modelling that as a plain
    /// `uses; Await` would let a binding whose LAST source-level mention is the
    /// awaited expression fall out of the frame, and the resume would then read
    /// a poll-fn stack slot that no longer holds anything. Repeating the uses
    /// after the `Await` keeps every operand live across it.
    fn suspend_with_operands(&mut self, mut operands: Vec<Node>, out: &mut Vec<Node>) {
        let mut reread = HashSet::new();
        collect_uses(&operands, &mut reread);
        out.append(&mut operands);
        self.mark_suspend();
        out.push(Node::Await);
        out.extend(reread.into_iter().map(Node::Use));
    }

    /// A preemption safepoint: the poll fn can return to the scheduler here
    /// without reading or writing anything. Unlike [`Self::opaque`] it adds no
    /// uses — it only ends a poll segment.
    fn safepoint(&mut self, out: &mut Vec<Node>) {
        if !self.model.statement_safepoints {
            return;
        }
        self.mark_suspend();
        out.push(Node::Await);
    }

    fn block(&mut self, block: &Block, out: &mut Vec<Node>) {
        self.scopes.push(HashMap::new());
        for stmt in &block.stmts {
            // `emit_coop_stmts` plants a preemption safepoint before every
            // statement, so mirror it here or the pass under-approximates.
            self.safepoint(out);
            self.stmt(stmt, out);
        }
        self.scopes.pop();
    }

    /// Lower a block into a fresh node list (for branch arms and loop bodies).
    fn block_nodes(&mut self, block: &Block) -> Vec<Node> {
        let mut out = Vec::new();
        self.block(block, &mut out);
        out
    }

    fn stmt(&mut self, stmt: &Stmt, out: &mut Vec<Node>) {
        match stmt {
            Stmt::Let(l) => {
                // The initializer is evaluated in the OUTER scope: `let x = x;`
                // reads the previous `x`.
                let mut init = Vec::new();
                self.expr(&l.init, &mut init);
                // `let x = await f();` writes the awaited result into `x` AFTER
                // the resume, so the poll fn stores it through a frame offset
                // and the binding has to have one — whether or not `x` outlives
                // the next suspension. Same for `let x = ch.recv();`.
                //
                // This holds for `let _ = await t;` too: the wildcard is
                // unreadable, but the code generator still looks the offset up
                // unconditionally, so the slot has to exist.
                if contains_await(&init) {
                    self.forced.insert(l.span);
                }
                out.append(&mut init);
                self.declare(&l.name, l.span);
                if l.name != "_" {
                    out.push(Node::Def(l.span));
                }
            }
            Stmt::Assign(a) => {
                self.expr(&a.value, out);
                // A whole-variable overwrite kills the previous value: if the
                // old contents are never read again, they need no frame slot.
                if let Some(span) = self.resolve(&a.name) {
                    out.push(Node::Def(span));
                }
            }
            Stmt::FieldAssign(f) => {
                // The cooperative await lowering evaluates the RHS first, then
                // reloads the destination object after resume before storing.
                // Preserve the source operand walk, but repeat its USES after a
                // suspending RHS so they are live across the actual codegen edge.
                let mut destination = Vec::new();
                self.expr(&f.object, &mut destination);
                let mut value = Vec::new();
                self.expr(&f.value, &mut value);
                Self::assignment_operands(destination, value, out);
            }
            Stmt::SuperInit(s) => {
                for arg in &s.args {
                    self.expr(&arg.expr, out);
                }
            }
            Stmt::StaticFieldAssign(s) => self.expr(&s.value, out),
            Stmt::IndexAssign(i) => {
                // As with field assignment, the cooperative emitter reloads
                // BOTH the array and index after an awaited RHS completes.
                let mut destination = Vec::new();
                self.expr(&i.array, &mut destination);
                self.expr(&i.index, &mut destination);
                let mut value = Vec::new();
                self.expr(&i.value, &mut value);
                Self::assignment_operands(destination, value, out);
            }
            Stmt::If(s) => {
                self.expr(&s.cond, out);
                let then_nodes = self.block_nodes(&s.then_block);
                let else_nodes = match &s.else_block {
                    Some(b) => self.block_nodes(b),
                    // No `else` still has a path that skips the `then` block.
                    None => Vec::new(),
                };
                out.push(Node::Branch(vec![then_nodes, else_nodes]));
            }
            Stmt::While(s) => {
                // The condition is re-evaluated every iteration, so it belongs
                // INSIDE the loop node.
                let mut body = Vec::new();
                // The back edge carries its own safepoint (willow-lpn.10):
                // `emit_coop_stmts` reaches the loop header through
                // `emit_coop_safepoint_to`, so a value that only crosses the
                // back edge still crosses a suspension.
                self.safepoint(&mut body);
                self.expr(&s.cond, &mut body);
                self.block(&s.body, &mut body);
                out.push(Node::Loop(body));
            }
            Stmt::For(s) => {
                self.expr(&s.iterable, out);
                self.scopes.push(HashMap::new());
                self.declare(&s.name, s.name_span);
                let mut body = Vec::new();
                // Back-edge safepoint, as for `while`.
                self.safepoint(&mut body);
                // The loop variable is rebound at the top of every iteration.
                if s.name != "_" {
                    body.push(Node::Def(s.name_span));
                }
                self.block(&s.body, &mut body);
                self.scopes.pop();
                out.push(Node::Loop(body));
            }
            Stmt::Break(_) => out.push(Node::Break),
            Stmt::Continue(_) => out.push(Node::Continue),
            Stmt::Defer(d) => {
                // A deferred body runs at scope exit, which can be after any
                // number of awaits, and `?`/`break`/`return` all reach it. Its
                // effective position is not modelled, so nothing in scope may be
                // narrowed away.
                match &d.body {
                    DeferBody::Expr(e) => self.expr(e, out),
                    DeferBody::Block(b) => {
                        let nodes = self.block_nodes(b);
                        out.push(Node::Seq(nodes));
                    }
                }
                self.opaque(out);
            }
            Stmt::Return(r) => {
                if let Some(v) = &r.value {
                    self.expr(v, out);
                }
                out.push(Node::Return);
            }
            Stmt::Expr(e) => self.expr(&e.expr, out),
        }
    }

    /// Append an assignment destination and value, preserving the extra
    /// destination reads performed by cooperative codegen after a suspending
    /// RHS resumes (`object.field = await task`, `array[index] = await task`).
    fn assignment_operands(mut destination: Vec<Node>, mut value: Vec<Node>, out: &mut Vec<Node>) {
        let mut reread = HashSet::new();
        if contains_await(&value) {
            collect_uses(&destination, &mut reread);
        }
        out.append(&mut destination);
        out.append(&mut value);
        out.extend(reread.into_iter().map(Node::Use));
    }

    fn expr(&mut self, expr: &Expr, out: &mut Vec<Node>) {
        match expr {
            Expr::Integer(..)
            | Expr::Float(..)
            | Expr::Bool(..)
            | Expr::Nil(..)
            | Expr::String(..)
            | Expr::StaticField(_) => {}
            Expr::Var(name, _) => {
                if let Some(span) = self.resolve(name) {
                    out.push(Node::Use(span));
                }
            }
            Expr::Binary(b) => {
                self.expr(&b.lhs, out);
                self.expr(&b.rhs, out);
            }
            Expr::Unary(u) => self.expr(&u.expr, out),
            Expr::Call(c) => {
                for arg in &c.args {
                    self.expr(&arg.expr, out);
                }
            }
            Expr::FieldAccess(obj, _, _) => self.expr(obj, out),
            Expr::MethodCall(m) => {
                let mut operands = Vec::new();
                self.expr(&m.object, &mut operands);
                for arg in &m.args {
                    self.expr(&arg.expr, &mut operands);
                }
                // `ch.recv()` parks on an empty channel and `ch.send(v)` parks
                // on a full bounded one, so both suspend without an `await`
                // (see `direct_suspend_type` / `is_channel_send`). Matching on
                // the METHOD NAME alone counts a same-named method on some
                // unrelated class as a suspension too, which only over-frames.
                if m.method == "recv" || m.method == "send" {
                    self.suspend_with_operands(operands, out);
                } else {
                    out.append(&mut operands);
                }
            }
            Expr::StaticCall(s) => {
                for arg in &s.args {
                    self.expr(&arg.expr, out);
                }
            }
            Expr::New(n) => {
                for arg in &n.args {
                    self.expr(&arg.expr, out);
                }
            }
            Expr::ObjectLiteral(o) => {
                for f in &o.fields {
                    self.expr(&f.value, out);
                }
            }
            Expr::Await(a) => {
                let mut operands = Vec::new();
                self.expr(&a.expr, &mut operands);
                self.suspend_with_operands(operands, out);
            }
            Expr::Select(s) => {
                // `select` picks one ready case at run time and each case body
                // is reached through scheduler machinery this pass does not
                // model, so the whole construct is opaque. The per-case
                // expressions and bodies are still lowered, which keeps their
                // own bindings resolving correctly.
                self.opaque(out);
                let mut arms = Vec::new();
                for case in &s.cases {
                    let mut arm = Vec::new();
                    self.scopes.push(HashMap::new());
                    match &case.kind {
                        SelectCaseKind::Recv { binding, channel } => {
                            self.expr(channel, &mut arm);
                            self.declare(binding, case.span);
                        }
                        SelectCaseKind::Send { channel, value } => {
                            self.expr(channel, &mut arm);
                            self.expr(value, &mut arm);
                        }
                        SelectCaseKind::Timeout { millis } => self.expr(millis, &mut arm),
                        SelectCaseKind::Join { binding, task } => {
                            self.expr(task, &mut arm);
                            self.declare(binding, case.span);
                        }
                        SelectCaseKind::Default => {}
                    }
                    self.block(&case.body, &mut arm);
                    self.scopes.pop();
                    arms.push(arm);
                }
                out.push(Node::Branch(arms));
            }
            Expr::Print(inner, _, _) => self.expr(inner, out),
            Expr::Ternary(t) => {
                self.expr(&t.condition, out);
                let mut then_nodes = Vec::new();
                self.expr(&t.then_expr, &mut then_nodes);
                let mut else_nodes = Vec::new();
                self.expr(&t.else_expr, &mut else_nodes);
                out.push(Node::Branch(vec![then_nodes, else_nodes]));
            }
            Expr::Range(r) => {
                self.expr(&r.start, out);
                self.expr(&r.end, out);
            }
            Expr::Lambda(l) => {
                // A lambda body runs at an unknown later point and may capture
                // anything in scope, so nothing in scope may be narrowed away.
                self.opaque(out);
                self.scopes.push(HashMap::new());
                for p in &l.params {
                    self.declare(&p.name, p.span);
                }
                match &l.body {
                    LambdaBody::Expr(e) => self.expr(e, out),
                    LambdaBody::Block(b) => {
                        let nodes = self.block_nodes(b);
                        out.push(Node::Seq(nodes));
                    }
                }
                self.scopes.pop();
            }
            Expr::Match(m) => {
                self.expr(&m.scrutinee, out);
                let mut arms = Vec::new();
                for arm in &m.arms {
                    self.scopes.push(HashMap::new());
                    self.declare_pattern(&arm.pattern);
                    let mut nodes = Vec::new();
                    match &arm.body {
                        MatchBody::Expr(e) => self.expr(e, &mut nodes),
                        MatchBody::Block(b) => self.block(b, &mut nodes),
                    }
                    self.scopes.pop();
                    arms.push(nodes);
                }
                out.push(Node::Branch(arms));
            }
            Expr::TryPropagate(inner, _) => {
                // `?` can return early. An early exit only REMOVES later uses,
                // so treating it as fall-through over-approximates liveness.
                self.expr(inner, out);
            }
            Expr::ArrayLiteral(elements, _) => {
                for e in elements {
                    self.expr(e, out);
                }
            }
            Expr::Index(array, index, _) => {
                self.expr(array, out);
                self.expr(index, out);
            }
        }
    }

    /// Bring a `match` arm's bindings into scope. Their spans are not frame
    /// slots; declaring them stops a use inside the arm from resolving to an
    /// outer binding of the same name.
    fn declare_pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Wildcard(_) | Pattern::LiteralBool(..) | Pattern::LiteralInt(..) => {}
            Pattern::Binding { name, span } => self.declare(name, *span),
            Pattern::EnumVariant { .. } => {}
            Pattern::EnumVariantTuple { bindings, span, .. } => {
                for b in bindings {
                    self.declare(b, *span);
                }
            }
            Pattern::ClassDowncast { binding, span, .. } => self.declare(binding, *span),
        }
    }
}

// ── phase B: backward dataflow ─────────────────────────────────────────────

/// Where `break` and `continue` land, as live sets.
#[derive(Default, Clone)]
struct LoopCtx {
    /// Live set at the loop's exit.
    exit: HashSet<Span>,
    /// Live set at the loop's head, i.e. the body's live-in from the last
    /// fixpoint round.
    head: HashSet<Span>,
}

struct Backward {
    framed: HashSet<Span>,
}

impl Backward {
    fn run(&mut self, nodes: &[Node], live: &mut HashSet<Span>, ctx: &LoopCtx) {
        for node in nodes.iter().rev() {
            match node {
                Node::Use(span) => {
                    live.insert(*span);
                }
                Node::Def(span) => {
                    live.remove(span);
                }
                Node::Await => self.framed.extend(live.iter().copied()),
                // Outside any loop these are unreachable, and `LoopCtx::default`
                // leaves both sets empty, which is the same as `Return`.
                Node::Break => *live = ctx.exit.clone(),
                Node::Continue => *live = ctx.head.clone(),
                Node::Return => live.clear(),
                Node::Seq(inner) => self.run(inner, live, ctx),
                Node::Branch(arms) => {
                    let mut merged = HashSet::new();
                    for arm in arms {
                        let mut arm_live = live.clone();
                        self.run(arm, &mut arm_live, ctx);
                        merged.extend(arm_live);
                    }
                    *live = merged;
                }
                Node::Loop(body) => self.run_loop(body, live),
            }
        }
    }

    /// Solve one loop by fixpoint.
    ///
    /// `live` on entry is the loop's live-OUT (the exit set); on return it holds
    /// the loop's live-IN. The back edge is what makes a use textually before an
    /// `await` still count as live across it, so the body is re-run with a
    /// growing head set until it stops growing.
    fn run_loop(&mut self, body: &[Node], live: &mut HashSet<Span>) {
        let exit = live.clone();
        // Seed the back edge with the exit set: a loop that runs zero times
        // flows straight from head to exit.
        let mut head = exit.clone();
        // Each round can only grow `head` (every transfer is monotone in its
        // input and `exit` is unioned back in), and the set is bounded by the
        // bindings in the function, so this settles. The cap only bounds the
        // damage if a future `Node` shape breaks that monotonicity.
        let mut settled = false;
        for _ in 0..MAX_LOOP_ROUNDS {
            let mut body_live = head.clone();
            let ctx = LoopCtx {
                exit: exit.clone(),
                head: head.clone(),
            };
            self.run(body, &mut body_live, &ctx);
            body_live.extend(exit.iter().copied());
            if body_live.is_subset(&head) {
                settled = true;
                break;
            }
            head = body_live;
        }
        if !settled {
            // No usable answer, so fall back to the pre-narrowing behaviour for
            // this loop: everything the body touches is live and framed.
            debug_assert!(settled, "live-across-await fixpoint did not converge");
            collect_spans(body, &mut head);
            self.framed.extend(head.iter().copied());
        }
        *live = head;
    }
}

/// Every binding span READ anywhere in `nodes`, including nested ones.
fn collect_uses(nodes: &[Node], out: &mut HashSet<Span>) {
    for node in nodes {
        match node {
            Node::Use(span) => {
                out.insert(*span);
            }
            Node::Def(_) | Node::Await | Node::Break | Node::Continue | Node::Return => {}
            Node::Seq(inner) | Node::Loop(inner) => collect_uses(inner, out),
            Node::Branch(arms) => {
                for arm in arms {
                    collect_uses(arm, out);
                }
            }
        }
    }
}

/// Every binding span mentioned anywhere in `nodes`, including nested ones.
fn collect_spans(nodes: &[Node], out: &mut HashSet<Span>) {
    for node in nodes {
        match node {
            Node::Use(span) | Node::Def(span) => {
                out.insert(*span);
            }
            Node::Await | Node::Break | Node::Continue | Node::Return => {}
            Node::Seq(inner) | Node::Loop(inner) => collect_spans(inner, out),
            Node::Branch(arms) => {
                for arm in arms {
                    collect_spans(arm, out);
                }
            }
        }
    }
}

/// Fixpoint round cap. Each round strictly grows the live set, so the true
/// bound is the number of distinct bindings in the function.
const MAX_LOOP_ROUNDS: usize = 10_000;

#[cfg(test)]
mod tests;
