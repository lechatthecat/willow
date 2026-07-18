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
//! into blocks is the backend slice's job. The backend is not yet wired to
//! consume the LIR, so behavior is unchanged (`--emit-lir` renders it).

use crate::parser::ast::Type;

use super::typed_ast::{HirExpr, HirExprKind, HirFunction, HirParam, HirProgram, HirStmt};

/// A whole program in lowered IR.
#[derive(Debug, Clone, PartialEq)]
pub struct LirProgram {
    pub functions: Vec<LirFunction>,
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

/// A non-branching instruction. Values are typed HIR expression trees.
#[derive(Debug, Clone, PartialEq)]
pub enum LirInst {
    /// `defer` registration (willow-vynv.2). The LIR backend does not emit
    /// defers yet — its eligibility check rejects any function containing one
    /// (falls back to the AST path).
    Defer(HirExpr),
    Let {
        name: String,
        mutable: bool,
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
    for f in &program.functions {
        functions.push(lower_function(f, None));
    }
    for c in &program.classes {
        for m in &c.methods {
            functions.push(lower_function(m, Some(&c.name)));
        }
    }
    LirProgram { functions }
}

/// Lower one function's statement tree into a block graph.
fn lower_function(f: &HirFunction, class: Option<&str>) -> LirFunction {
    let mut b = Builder::new();
    b.lower_stmts(&f.body);
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

/// Block-graph builder: appends instructions to a current block and seals
/// blocks with terminators as control flow branches and rejoins.
struct Builder {
    blocks: Vec<(Vec<LirInst>, Option<Terminator>)>,
    current: usize,
    /// Counter for synthesized `for` induction variables, unique per function
    /// so nested loops do not collide.
    for_counter: usize,
    /// Innermost-first (exit, continue_target) loop context for
    /// break/continue lowering (willow-kzka).
    loop_stack: Vec<(BlockId, BlockId)>,
}

impl Builder {
    fn new() -> Self {
        Self {
            blocks: vec![(Vec::new(), None)],
            current: 0,
            for_counter: 0,
            loop_stack: Vec::new(),
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

    fn lower_stmt(&mut self, stmt: &HirStmt) {
        match stmt {
            HirStmt::Let {
                name,
                mutable,
                value,
                ..
            } => self.push(LirInst::Let {
                name: name.clone(),
                mutable: *mutable,
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
                self.terminate(Terminator::Return(value.clone()));
                // Anything after a return is unreachable; give it a fresh
                // predecessor-less block rather than corrupting this one.
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::Break { .. } => {
                let (exit, _) = *self.loop_stack.last().expect("break outside loop");
                self.terminate(Terminator::Jump(exit));
                let dead = self.new_block();
                self.switch_to(dead);
            }
            HirStmt::Defer { call, .. } => {
                self.push(LirInst::Defer(call.clone()));
            }
            HirStmt::Continue { .. } => {
                let (_, cont) = *self.loop_stack.last().expect("continue outside loop");
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
                self.lower_stmts(then_branch);
                self.terminate(Terminator::Jump(merge_block));

                if let Some(else_branch) = else_branch {
                    self.switch_to(else_block);
                    self.lower_stmts(else_branch);
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
                self.loop_stack.push((exit, header));
                self.lower_stmts(body);
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

        // Entry instructions + the loop-variable binding for the body.
        let bound_name: String;
        let element_binding: HirExpr;
        match (&iterable.kind, &iterable.ty) {
            // for x in start..end  →  i = start; while i < end { x = i; .. }
            (HirExprKind::Range { start, end }, _) => {
                bound_name = format!("__for{n}_end");
                self.push(LirInst::Let {
                    name: i_name.clone(),
                    mutable: true,
                    value: (**start).clone(),
                });
                self.push(LirInst::Let {
                    name: bound_name.clone(),
                    mutable: false,
                    value: (**end).clone(),
                });
                element_binding = i64_var(&i_name);
            }
            // for x in arr  →  a = arr; i = 0; while i < a.len() { x = a[i]; .. }
            (_, Type::Array(elem)) => {
                let arr_name = format!("__for{n}_arr");
                bound_name = format!("__for{n}_len");
                let arr_var = HirExpr {
                    kind: HirExprKind::Var(arr_name.clone()),
                    ty: iterable.ty.clone(),
                    span,
                };
                self.push(LirInst::Let {
                    name: arr_name.clone(),
                    mutable: false,
                    value: iterable.clone(),
                });
                self.push(LirInst::Let {
                    name: i_name.clone(),
                    mutable: true,
                    value: HirExpr {
                        kind: HirExprKind::Int(0),
                        ty: Type::I64,
                        span,
                    },
                });
                self.push(LirInst::Let {
                    name: bound_name.clone(),
                    mutable: false,
                    value: HirExpr {
                        kind: HirExprKind::MethodCall {
                            object: Box::new(arr_var.clone()),
                            method: "len".to_string(),
                            args: vec![],
                        },
                        ty: Type::I64,
                        span,
                    },
                });
                element_binding = HirExpr {
                    kind: HirExprKind::Index {
                        array: Box::new(arr_var),
                        index: Box::new(i64_var(&i_name)),
                    },
                    ty: (**elem).clone(),
                    span,
                };
            }
            // Ranges bound to a variable (`let r = 0..3; for x in r`).
            (_, Type::Generic(g, args)) if g == "Range" && args.first() == Some(&Type::I64) => {
                // The range VALUE's bounds are runtime state; without a field
                // projection in the HIR, materialize via while over the value
                // is not expressible yet — treated by the HIR lowering as an
                // array-like unsupported case before reaching here. Fall back
                // to binding the whole value; the backend slice will finish it.
                bound_name = format!("__for{n}_end");
                self.push(LirInst::Let {
                    name: i_name.clone(),
                    mutable: true,
                    value: HirExpr {
                        kind: HirExprKind::Int(0),
                        ty: Type::I64,
                        span,
                    },
                });
                self.push(LirInst::Let {
                    name: bound_name.clone(),
                    mutable: false,
                    value: iterable.clone(),
                });
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
        let bound_expr = HirExpr {
            kind: HirExprKind::Var(bound_name.clone()),
            ty: Type::I64,
            span,
        };
        self.terminate(Terminator::Branch {
            cond: lt(i64_var(&i_name), bound_expr),
            then_block: body_block,
            else_block: exit,
        });

        self.switch_to(body_block);
        self.push(LirInst::Let {
            name: name.to_string(),
            mutable: false,
            value: element_binding,
        });
        self.loop_stack.push((exit, inc_block));
        self.lower_stmts(body);
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
    for f in &program.functions {
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
        LirInst::Defer(call) => format!("defer {};", e(call)),
        LirInst::Let {
            name,
            mutable,
            value,
        } => {
            let kw = if *mutable { "let mut" } else { "let" };
            format!("{kw} {name} = {};", e(value))
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

    // 12. array-for desugars to arr/index/len lets and an indexed element bind
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
        assert!(names.contains(&"__for0_len"), "{names:?}");
        let Terminator::Jump(header) = f.blocks[0].terminator else {
            unreachable!()
        };
        let Terminator::Branch {
            then_block: body, ..
        } = &f.blocks[header.0].terminator
        else {
            panic!("header must branch");
        };
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
