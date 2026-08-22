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
use crate::parser::ast::Type;

use super::typed_ast::{
    HirDeferBody, HirExpr, HirExprKind, HirFunction, HirParam, HirProgram, HirStmt,
};

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
    pub params: Vec<HirParam>,
    pub return_type: Type,
    pub blocks: Vec<LirBlock>,
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
        sites: Vec<Span>,
    },
    /// Close the scope opened by the matching [`LirInst::EnterDeferScope`]:
    /// run its registrations (newest first) and pop it. This is the
    /// FALLTHROUGH exit — an early exit uses [`LirInst::FlushDefers`] and
    /// leaves the scope structure in place for the paths that did not take it.
    LeaveDeferScope,
    /// Run the registrations of the innermost `scopes` defer scopes, newest
    /// first, without popping them (willow-0g8j.2.3). Emitted immediately
    /// before a `return`, `break` or `continue` that leaves those scopes.
    FlushDefers {
        scopes: usize,
    },
    /// `defer` registration (willow-vynv.2). `span` is the `defer` statement's
    /// own span — the key its scope's cleanup flag is registered under, so it
    /// must match the entry in the enclosing `EnterDeferScope::sites`.
    Defer {
        body: LirDeferBody,
        span: Span,
    },
    Let {
        name: String,
        mutable: bool,
        /// The type the name is bound with — the annotation when the source
        /// wrote one, otherwise `value.ty`. A consumer must size and type the
        /// variable's storage from this, because `let a: Animal = new Dog();`
        /// binds `a` as the interface while the initialiser is the class
        /// (willow-0g8j.5).
        ty: Type,
        value: HirExpr,
    },
    Assign {
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
        let mut b = Builder::new();
        b.lower_stmts(body);
        out.push(LirLambda {
            span: expr.span,
            function: LirFunction {
                name: lambda_placeholder_name(expr.span),
                params: params.clone(),
                return_type: (**ret).clone(),
                blocks: b.finish(),
            },
        });
    }
}

/// The name a lifted lambda carries until the backend renames it to the
/// `$lambda.N` symbol it declared. Derived from the span so `--emit-lir` output
/// is stable and two lambdas never collide.
pub fn lambda_placeholder_name(span: Span) -> String {
    format!("$lambda@{}:{}", span.file_id.0, span.start)
}

/// Lower one function's statement tree into a block graph.
fn lower_function(f: &HirFunction, class: Option<&str>) -> LirFunction {
    let mut b = Builder::new();
    b.lower_scope(&f.body);
    // The fall-through end of a function is an implicit `return;` (the type
    // checker has already guaranteed value-returning paths return).
    let blocks = b.finish();
    let name = match class {
        Some(class) => format!("{class}::{}", f.name),
        None => f.name.clone(),
    };
    LirFunction {
        name,
        params: f.params.clone(),
        return_type: f.return_type.clone(),
        blocks,
    }
}

/// A `let` the LIR lowering synthesizes itself (the `for` desugaring's
/// induction variable, bound, array and element bindings). These never came
/// from an annotated source `let`, so the binding type IS the initialiser's
/// type (willow-0g8j.5).
fn synth_let(name: &str, mutable: bool, value: HirExpr) -> LirInst {
    LirInst::Let {
        name: name.to_string(),
        mutable,
        ty: value.ty.clone(),
        value,
    }
}

/// Block-graph builder: appends instructions to a current block and seals
/// blocks with terminators as control flow branches and rejoins.
struct Builder {
    blocks: Vec<(Vec<LirInst>, Option<Terminator>)>,
    current: usize,
    /// Counter for synthesized `for` induction variables, unique per function
    /// so nested loops do not collide.
    for_counter: usize,
    /// Innermost-first (exit, continue_target, defer_depth_at_entry) loop
    /// context for break/continue lowering (willow-kzka). The depth is what
    /// tells `break`/`continue` how many defer scopes they are leaving
    /// (willow-0g8j.2.3) — the ones opened inside the loop body, not the ones
    /// that were already open when the loop started.
    loop_stack: Vec<(BlockId, BlockId, usize)>,
    /// How many defer scopes are currently open. `return` flushes all of them.
    defer_depth: usize,
}

impl Builder {
    fn new() -> Self {
        Self {
            blocks: vec![(Vec::new(), None)],
            current: 0,
            for_counter: 0,
            loop_stack: Vec::new(),
            defer_depth: 0,
        }
    }

    fn new_block(&mut self) -> BlockId {
        self.blocks.push((Vec::new(), None));
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

    fn finish(self) -> Vec<LirBlock> {
        let blocks: Vec<LirBlock> = self
            .blocks
            .into_iter()
            .enumerate()
            .map(|(i, (instrs, terminator))| LirBlock {
                id: BlockId(i),
                instrs,
                terminator: terminator.unwrap_or(Terminator::Return(None)),
            })
            .collect();
        prune_unreachable(blocks)
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
    /// Scopes without a `defer` get no markers at all — nothing would run at
    /// their exit, and the flush counts stay small.
    fn lower_scope(&mut self, stmts: &[HirStmt]) {
        let sites: Vec<Span> = stmts
            .iter()
            .filter_map(|s| match s {
                HirStmt::Defer { span, .. } => Some(*span),
                _ => None,
            })
            .collect();
        if sites.is_empty() {
            self.lower_stmts(stmts);
            return;
        }
        self.push(LirInst::EnterDeferScope { sites });
        self.defer_depth += 1;
        self.lower_stmts(stmts);
        self.defer_depth -= 1;
        // The fallthrough close. If the scope ended in a `return`, this lands
        // in the dead block `terminate` switched to and is pruned.
        self.push(LirInst::LeaveDeferScope);
    }

    /// Flush every defer scope an early exit is about to leave.
    fn flush_defers_down_to(&mut self, depth: usize) {
        let scopes = self.defer_depth - depth;
        if scopes > 0 {
            self.push(LirInst::FlushDefers { scopes });
        }
    }

    fn lower_stmt(&mut self, stmt: &HirStmt) {
        match stmt {
            HirStmt::Let {
                name,
                mutable,
                ty,
                value,
                ..
            } => self.push(LirInst::Let {
                name: name.clone(),
                mutable: *mutable,
                ty: ty.clone(),
                value: value.clone(),
            }),
            HirStmt::Assign { name, value, .. } => self.push(LirInst::Assign {
                name: name.clone(),
                value: value.clone(),
            }),
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
            HirStmt::SuperInit { args, .. } => self.push(LirInst::SuperInit { args: args.clone() }),
            HirStmt::Expr(e) => self.push(LirInst::Expr(e.clone())),
            HirStmt::Return { value, .. } => {
                // The returned value is computed BEFORE the defers run: a
                // deferred body can mutate what the expression reads.
                self.flush_defers_down_to(0);
                self.terminate(Terminator::Return(value.clone()));
                // Anything after a return is unreachable; give it a fresh
                // predecessor-less block rather than corrupting this one.
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::Break { .. } => {
                let (exit, _, depth) = *self.loop_stack.last().expect("break outside loop");
                self.flush_defers_down_to(depth);
                self.terminate(Terminator::Jump(exit));
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::Defer { body, span } => {
                let body = match body {
                    HirDeferBody::Expr(e) => LirDeferBody::Expr(e.clone()),
                    HirDeferBody::Block(stmts) => LirDeferBody::Block(stmts.clone()),
                };
                self.push(LirInst::Defer { body, span: *span });
            }
            HirStmt::Continue { .. } => {
                let (_, cont, depth) = *self.loop_stack.last().expect("continue outside loop");
                self.flush_defers_down_to(depth);
                self.terminate(Terminator::Jump(cont));
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
                let then_block = self.new_block();
                let merge_block = self.new_block();
                let else_block = match else_branch {
                    Some(_) => self.new_block(),
                    None => merge_block,
                };
                self.terminate(Terminator::Branch {
                    cond: cond.clone(),
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
                let exit = self.new_block();

                self.terminate(Terminator::Jump(header));
                self.switch_to(header);
                self.terminate(Terminator::Branch {
                    cond: cond.clone(),
                    then_block: body_block,
                    else_block: exit,
                });

                self.switch_to(body_block);
                self.loop_stack.push((exit, header, self.defer_depth));
                self.lower_scope(body);
                self.loop_stack.pop();
                self.terminate(Terminator::Jump(header));

                self.switch_to(exit);
            }
            HirStmt::For {
                name,
                iterable,
                body,
                span,
            } => self.lower_for(name, iterable, body, *span),
            HirStmt::Lock { body, .. } => self.lower_scope(body),
        }
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
                self.push(synth_let(&i_name, true, (**start).clone()));
                self.push(synth_let(&bound_name, false, (**end).clone()));
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
                self.push(synth_let(&arr_name, false, iterable.clone()));
                self.push(synth_let(
                    &i_name,
                    true,
                    HirExpr {
                        kind: HirExprKind::Int(0),
                        ty: Type::I64,
                        span,
                    },
                ));
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
                self.push(synth_let(&range_name, false, iterable.clone()));
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
                self.push(synth_let(&i_name, true, bound("start")));
                self.push(synth_let(&bound_name, false, bound("end")));
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
        self.push(synth_let(name, false, element_binding));
        self.loop_stack.push((exit, inc_block, self.defer_depth));
        self.lower_scope(body);
        self.loop_stack.pop();
        self.terminate(Terminator::Jump(inc_block));

        self.switch_to(inc_block);
        self.push(LirInst::Assign {
            name: i_name.clone(),
            value: plus_one(i64_var(&i_name)),
        });
        self.terminate(Terminator::Jump(header));

        self.switch_to(exit);
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
        LirInst::EnterDeferScope { sites } => {
            format!("enter defer scope ({} sites);", sites.len())
        }
        LirInst::LeaveDeferScope => "leave defer scope;".to_string(),
        LirInst::FlushDefers { scopes } => format!("flush defers ({scopes});"),
        LirInst::Defer { body, .. } => match body {
            LirDeferBody::Expr(call) => format!("defer {};", e(call)),
            LirDeferBody::Block(stmts) => format!("defer {{ .. }} ({} stmts);", stmts.len()),
        },
        LirInst::Let {
            name,
            mutable,
            ty,
            value,
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
        LirInst::Assign { name, value } => format!("{name} = {};", e(value)),
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
        LirInst::SuperInit { args } => {
            let args = args.iter().map(e).collect::<Vec<_>>().join(", ");
            format!("super.init({args});")
        }
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
        assert_eq!(f.blocks[0].instrs.len(), 2);
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
                Terminator::Jump(_) | Terminator::Branch { .. } | Terminator::Return(_) => {}
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
                _ => "other".to_string(),
            })
            .collect();
        assert_eq!(kinds, ["let a", "let b", "expr"]);
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
