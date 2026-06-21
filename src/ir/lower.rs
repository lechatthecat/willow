//! Lowering: type-checked AST → typed HIR ([`super::typed_ast`]) — willow-mb5.
//!
//! Slice 1 lowers the MVP-core constructs. Type information flows in through a
//! [`LowerCtx`] (parameter/`let` bindings and free-function return types) and is
//! attached to every [`HirExpr`], so a downstream consumer never has to
//! re-derive a type from the AST. Constructs outside slice 1 (classes, methods,
//! async, collections, `for`, field/index assignment, …) return a diagnostic
//! rather than silently dropping work, so later slices can extend coverage
//! incrementally without changing behavior.

use std::collections::HashMap;

use crate::diagnostics::{Diagnostic, ErrorCode, Severity, Span};
use crate::parser::ast::{
    BinOp, Block, CallArgMode, Expr, FunctionDecl, Item, Program, Stmt, Type, UnaryOp,
};

use super::typed_ast::{HirExpr, HirExprKind, HirFunction, HirParam, HirProgram, HirStmt};

/// Lower a whole program's free functions to typed HIR. Non-function items and
/// constructs outside slice 1 are reported as diagnostics; the functions that
/// do lower cleanly are still returned, so callers can make progress.
pub fn lower_program(program: &Program) -> (HirProgram, Vec<Diagnostic>) {
    let mut fn_returns: HashMap<String, Type> = HashMap::new();
    for item in &program.items {
        if let Item::Function(f) = item {
            fn_returns.insert(f.name.clone(), f.return_type.clone());
        }
    }

    let mut functions = Vec::new();
    let mut diagnostics = Vec::new();
    for item in &program.items {
        if let Item::Function(f) = item {
            match lower_function(f, &fn_returns) {
                Ok(func) => functions.push(func),
                Err(d) => diagnostics.push(d),
            }
        }
    }
    (HirProgram { functions }, diagnostics)
}

/// Lower a single free function against the program's function signatures.
pub fn lower_function(
    f: &FunctionDecl,
    fn_returns: &HashMap<String, Type>,
) -> Result<HirFunction, Diagnostic> {
    let mut ctx = LowerCtx::new(fn_returns);
    let mut params = Vec::with_capacity(f.params.len());
    for p in &f.params {
        ctx.bind(p.name.clone(), p.ty.clone());
        params.push(HirParam {
            name: p.name.clone(),
            ty: p.ty.clone(),
            span: p.span,
        });
    }
    let body = lower_block(&f.body, &mut ctx)?;
    Ok(HirFunction {
        name: f.name.clone(),
        params,
        return_type: f.return_type.clone(),
        body,
        span: f.span,
    })
}

/// Lowering scope: variable types (innermost-last) plus the free-function
/// return types used to type `Call` expressions.
struct LowerCtx<'a> {
    scopes: Vec<HashMap<String, Type>>,
    fn_returns: &'a HashMap<String, Type>,
}

impl<'a> LowerCtx<'a> {
    fn new(fn_returns: &'a HashMap<String, Type>) -> Self {
        Self {
            scopes: vec![HashMap::new()],
            fn_returns,
        }
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn bind(&mut self, name: String, ty: Type) {
        self.scopes
            .last_mut()
            .expect("at least one scope")
            .insert(name, ty);
    }

    fn lookup_var(&self, name: &str) -> Option<Type> {
        self.scopes.iter().rev().find_map(|s| s.get(name).cloned())
    }
}

fn lower_block(block: &Block, ctx: &mut LowerCtx) -> Result<Vec<HirStmt>, Diagnostic> {
    ctx.push_scope();
    let mut out = Vec::with_capacity(block.stmts.len());
    for stmt in &block.stmts {
        out.push(lower_stmt(stmt, ctx)?);
    }
    ctx.pop_scope();
    Ok(out)
}

fn lower_stmt(stmt: &Stmt, ctx: &mut LowerCtx) -> Result<HirStmt, Diagnostic> {
    match stmt {
        Stmt::Let(l) => {
            let value = lower_expr(&l.init, ctx)?;
            // A `let x: T = ..` annotation pins the binding type; otherwise the
            // type flows from the value expression.
            let binding_ty = l.ty.clone().unwrap_or_else(|| value.ty.clone());
            ctx.bind(l.name.clone(), binding_ty);
            Ok(HirStmt::Let {
                name: l.name.clone(),
                mutable: l.mutable,
                value,
                span: l.span,
            })
        }
        Stmt::Assign(a) => {
            let value = lower_expr(&a.value, ctx)?;
            Ok(HirStmt::Assign {
                name: a.name.clone(),
                value,
                span: a.span,
            })
        }
        Stmt::If(i) => {
            let cond = lower_expr(&i.cond, ctx)?;
            let then_branch = lower_block(&i.then_block, ctx)?;
            let else_branch = match &i.else_block {
                Some(b) => Some(lower_block(b, ctx)?),
                None => None,
            };
            Ok(HirStmt::If {
                cond,
                then_branch,
                else_branch,
                span: i.span,
            })
        }
        Stmt::While(w) => {
            let cond = lower_expr(&w.cond, ctx)?;
            let body = lower_block(&w.body, ctx)?;
            Ok(HirStmt::While {
                cond,
                body,
                span: w.span,
            })
        }
        Stmt::Return(r) => {
            let value = match &r.value {
                Some(e) => Some(lower_expr(e, ctx)?),
                None => None,
            };
            Ok(HirStmt::Return {
                value,
                span: r.span,
            })
        }
        Stmt::Expr(e) => Ok(HirStmt::Expr(lower_expr(&e.expr, ctx)?)),
        Stmt::FieldAssign(s) => Err(unsupported(s.span, "field assignment")),
        Stmt::SuperInit(s) => Err(unsupported(s.span, "super.init")),
        Stmt::StaticFieldAssign(s) => Err(unsupported(s.span, "static field assignment")),
        Stmt::IndexAssign(s) => Err(unsupported(s.span, "index assignment")),
        Stmt::For(s) => Err(unsupported(s.span, "for loop")),
    }
}

fn lower_expr(expr: &Expr, ctx: &mut LowerCtx) -> Result<HirExpr, Diagnostic> {
    match expr {
        Expr::Integer(n, span) => Ok(lit(HirExprKind::Int(*n), Type::I64, *span)),
        Expr::Float(f, span) => Ok(lit(HirExprKind::Float(*f), Type::F64, *span)),
        Expr::Bool(b, span) => Ok(lit(HirExprKind::Bool(*b), Type::Bool, *span)),
        Expr::String(s, span) => Ok(lit(HirExprKind::Str(s.clone()), Type::String, *span)),
        Expr::Var(name, span) => {
            let ty = ctx.lookup_var(name).ok_or_else(|| {
                internal(
                    *span,
                    format!("unbound variable `{name}` reached HIR lowering"),
                )
            })?;
            Ok(HirExpr {
                kind: HirExprKind::Var(name.clone()),
                ty,
                span: *span,
            })
        }
        Expr::Binary(b) => {
            let lhs = lower_expr(&b.lhs, ctx)?;
            let rhs = lower_expr(&b.rhs, ctx)?;
            let ty = binary_result_type(&b.op, &lhs.ty);
            Ok(HirExpr {
                kind: HirExprKind::Binary {
                    op: b.op.clone(),
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                },
                ty,
                span: b.span,
            })
        }
        Expr::Unary(u) => {
            let operand = lower_expr(&u.expr, ctx)?;
            let ty = match u.op {
                UnaryOp::Neg => operand.ty.clone(),
                UnaryOp::Not => Type::Bool,
            };
            Ok(HirExpr {
                kind: HirExprKind::Unary {
                    op: u.op.clone(),
                    operand: Box::new(operand),
                },
                ty,
                span: u.span,
            })
        }
        Expr::Call(c) => {
            let mut args = Vec::with_capacity(c.args.len());
            for arg in &c.args {
                if arg.mode != CallArgMode::Value {
                    return Err(unsupported(arg.span, "reference call argument"));
                }
                args.push(lower_expr(&arg.expr, ctx)?);
            }
            let ty = ctx.fn_returns.get(&c.callee).cloned().ok_or_else(|| {
                internal(
                    c.span,
                    format!(
                        "call to unknown function `{}` reached HIR lowering",
                        c.callee
                    ),
                )
            })?;
            Ok(HirExpr {
                kind: HirExprKind::Call {
                    callee: c.callee.clone(),
                    args,
                },
                ty,
                span: c.span,
            })
        }
        Expr::Print(inner, newline, span) => {
            let value = lower_expr(inner, ctx)?;
            Ok(HirExpr {
                kind: HirExprKind::Print {
                    value: Box::new(value),
                    newline: *newline,
                },
                ty: Type::Void,
                span: *span,
            })
        }
        other => Err(unsupported(other.span(), "expression form")),
    }
}

fn binary_result_type(op: &BinOp, lhs_ty: &Type) -> Type {
    match op {
        // Arithmetic preserves the (already type-checked) operand type.
        BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => lhs_ty.clone(),
        // Comparisons and logical operators always produce `Bool`.
        BinOp::Eq
        | BinOp::Ne
        | BinOp::Lt
        | BinOp::Le
        | BinOp::Gt
        | BinOp::Ge
        | BinOp::And
        | BinOp::Or => Type::Bool,
    }
}

fn lit(kind: HirExprKind, ty: Type, span: Span) -> HirExpr {
    HirExpr { kind, ty, span }
}

fn unsupported(span: Span, what: &str) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        ErrorCode::E0800,
        format!("HIR lowering does not yet support {what} (willow-mb5 slice 1)"),
    )
    .with_label(crate::diagnostics::Label::primary(span, "here"))
}

fn internal(span: Span, msg: String) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        ErrorCode::E0800,
        format!("internal compiler error: {msg}"),
    )
    .with_label(crate::diagnostics::Label::primary(span, "here"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn lower_src(src: &str) -> (HirProgram, Vec<Diagnostic>) {
        let tokens = Lexer::new(src).tokenize().expect("lexing failed");
        let (program, errs) = Parser::new(tokens).parse();
        assert!(errs.is_empty(), "unexpected parse errors: {errs:?}");
        lower_program(&program)
    }

    /// Lower a program expected to be wholly within slice 1; assert no
    /// diagnostics and return the first function's body.
    fn lower_body(src: &str) -> Vec<HirStmt> {
        let (hir, diags) = lower_src(src);
        assert!(
            diags.is_empty(),
            "unexpected lowering diagnostics: {diags:?}"
        );
        hir.functions
            .into_iter()
            .next()
            .expect("at least one function")
            .body
    }

    /// Extract the value expression of the first `return` statement in a body.
    fn first_return(body: &[HirStmt]) -> &HirExpr {
        body.iter()
            .find_map(|s| match s {
                HirStmt::Return { value: Some(v), .. } => Some(v),
                _ => None,
            })
            .expect("a return with a value")
    }

    fn return_ty(src: &str) -> Type {
        let body = lower_body(src);
        first_return(&body).ty.clone()
    }

    // 1. integer literal → I64
    #[test]
    fn p01_integer_literal_is_i64() {
        assert_eq!(return_ty("fn f() -> i64 { return 7; }"), Type::I64);
    }

    // 2. float literal → F64
    #[test]
    fn p02_float_literal_is_f64() {
        assert_eq!(return_ty("fn f() -> f64 { return 1.5; }"), Type::F64);
    }

    // 3. bool literal → Bool
    #[test]
    fn p03_bool_literal_is_bool() {
        assert_eq!(return_ty("fn f() -> bool { return true; }"), Type::Bool);
    }

    // 4. string literal → String
    #[test]
    fn p04_string_literal_is_string() {
        assert_eq!(
            return_ty("fn f() -> String { return \"hi\"; }"),
            Type::String
        );
    }

    // 5. parameter variable read carries its declared type
    #[test]
    fn p05_param_var_has_declared_type() {
        assert_eq!(return_ty("fn f(a: i64) -> i64 { return a; }"), Type::I64);
    }

    // 6. f64 parameter variable read
    #[test]
    fn p06_param_var_f64() {
        assert_eq!(return_ty("fn f(a: f64) -> f64 { return a; }"), Type::F64);
    }

    // 7. integer addition → I64
    #[test]
    fn p07_i64_add_is_i64() {
        assert_eq!(return_ty("fn f() -> i64 { return 1 + 2; }"), Type::I64);
    }

    // 8. float addition → F64
    #[test]
    fn p08_f64_add_is_f64() {
        assert_eq!(return_ty("fn f() -> f64 { return 1.0 + 2.0; }"), Type::F64);
    }

    // 9. subtraction/multiplication/division/remainder preserve operand type
    #[test]
    fn p09_arithmetic_preserves_operand_type() {
        for op in ["-", "*", "/", "%"] {
            let src = format!("fn f() -> i64 {{ return 6 {op} 3; }}");
            assert_eq!(return_ty(&src), Type::I64, "op {op}");
        }
    }

    // 10. equality → Bool
    #[test]
    fn p10_eq_is_bool() {
        assert_eq!(return_ty("fn f() -> bool { return 1 == 2; }"), Type::Bool);
    }

    // 11. relational comparisons → Bool
    #[test]
    fn p11_relational_is_bool() {
        for op in ["<", "<=", ">", ">=", "!="] {
            let src = format!("fn f() -> bool {{ return 1 {op} 2; }}");
            assert_eq!(return_ty(&src), Type::Bool, "op {op}");
        }
    }

    // 12. logical and/or → Bool
    #[test]
    fn p12_logical_is_bool() {
        assert_eq!(
            return_ty("fn f() -> bool { return true && false; }"),
            Type::Bool
        );
        assert_eq!(
            return_ty("fn f() -> bool { return true || false; }"),
            Type::Bool
        );
    }

    // 13. unary negation preserves operand type
    #[test]
    fn p13_unary_neg_preserves_type() {
        assert_eq!(return_ty("fn f(a: i64) -> i64 { return -a; }"), Type::I64);
    }

    // 14. unary not → Bool
    #[test]
    fn p14_unary_not_is_bool() {
        assert_eq!(
            return_ty("fn f(a: bool) -> bool { return !a; }"),
            Type::Bool
        );
    }

    // 15. nested binary propagates the operand type outward
    #[test]
    fn p15_nested_binary_type() {
        assert_eq!(
            return_ty("fn f() -> i64 { return (1 + 2) * 3 - 4; }"),
            Type::I64
        );
    }

    // 16. comparison of arithmetic sub-expressions is still Bool
    #[test]
    fn p16_compare_of_arithmetic_is_bool() {
        assert_eq!(
            return_ty("fn f() -> bool { return 1 + 2 < 3 * 4; }"),
            Type::Bool
        );
    }

    // 17. free-function call carries the callee's return type
    #[test]
    fn p17_call_has_callee_return_type() {
        let ty = return_ty("fn g() -> i64 { return 1; } fn f() -> i64 { return g(); }");
        assert_eq!(ty, Type::I64);
    }

    // 18. call with arguments lowers each argument
    #[test]
    fn p18_call_with_args() {
        let (hir, diags) = lower_src(
            "fn add(a: i64, b: i64) -> i64 { return a + b; } \
             fn f() -> i64 { return add(1, 2); }",
        );
        assert!(diags.is_empty(), "{diags:?}");
        let f = hir
            .functions
            .iter()
            .find(|fun| fun.name == "f")
            .expect("function f");
        match &first_return(&f.body).kind {
            HirExprKind::Call { callee, args } => {
                assert_eq!(callee, "add");
                assert_eq!(args.len(), 2);
                assert_eq!(args[0].ty, Type::I64);
            }
            other => panic!("expected call, got {other:?}"),
        }
    }

    // 19. print expression is Void
    #[test]
    fn p19_print_is_void() {
        let body = lower_body("fn f() { print(1); }");
        match &body[0] {
            HirStmt::Expr(e) => {
                assert_eq!(e.ty, Type::Void);
                assert!(matches!(e.kind, HirExprKind::Print { newline: false, .. }));
            }
            other => panic!("expected expr stmt, got {other:?}"),
        }
    }

    // 20. println sets the newline flag
    #[test]
    fn p20_println_newline_flag() {
        let body = lower_body("fn f() { println(1); }");
        match &body[0] {
            HirStmt::Expr(e) => {
                assert!(matches!(e.kind, HirExprKind::Print { newline: true, .. }));
            }
            other => panic!("expected expr stmt, got {other:?}"),
        }
    }

    // 21. let binds the inferred value type into scope
    #[test]
    fn p21_let_binds_inferred_type() {
        let ty = return_ty("fn f() -> i64 { let x = 5; return x; }");
        assert_eq!(ty, Type::I64);
    }

    // 22. let with an explicit annotation pins the binding type
    #[test]
    fn p22_let_annotation_pins_type() {
        let body = lower_body("fn f() { let x: f64 = 2.0; }");
        match &body[0] {
            HirStmt::Let { mutable, value, .. } => {
                assert!(!mutable);
                assert_eq!(value.ty, Type::F64);
            }
            other => panic!("expected let, got {other:?}"),
        }
    }

    // 23. let mut records mutability
    #[test]
    fn p23_let_mut_records_mutability() {
        let body = lower_body("fn f() { let mut x = 1; }");
        assert!(matches!(body[0], HirStmt::Let { mutable: true, .. }));
    }

    // 24. assignment lowers its value expression
    #[test]
    fn p24_assign_lowers_value() {
        let body = lower_body("fn f() { let mut x = 1; x = 2; }");
        match &body[1] {
            HirStmt::Assign { name, value, .. } => {
                assert_eq!(name, "x");
                assert_eq!(value.ty, Type::I64);
            }
            other => panic!("expected assign, got {other:?}"),
        }
    }

    // 25. if lowers a Bool condition and both branches
    #[test]
    fn p25_if_cond_is_bool_with_branches() {
        let body = lower_body("fn f(a: i64) { if a > 0 { print(1); } else { print(2); } }");
        match &body[0] {
            HirStmt::If {
                cond,
                then_branch,
                else_branch,
                ..
            } => {
                assert_eq!(cond.ty, Type::Bool);
                assert_eq!(then_branch.len(), 1);
                assert_eq!(else_branch.as_ref().map(|b| b.len()), Some(1));
            }
            other => panic!("expected if, got {other:?}"),
        }
    }

    // 26. if without else has no else branch
    #[test]
    fn p26_if_without_else() {
        let body = lower_body("fn f(a: i64) { if a > 0 { print(1); } }");
        assert!(matches!(
            &body[0],
            HirStmt::If {
                else_branch: None,
                ..
            }
        ));
    }

    // 27. while lowers a Bool condition and a body
    #[test]
    fn p27_while_cond_is_bool() {
        let body = lower_body("fn f(a: bool) { while a { print(1); } }");
        match &body[0] {
            HirStmt::While { cond, body, .. } => {
                assert_eq!(cond.ty, Type::Bool);
                assert_eq!(body.len(), 1);
            }
            other => panic!("expected while, got {other:?}"),
        }
    }

    // 28. return without a value lowers to None
    #[test]
    fn p28_bare_return() {
        let body = lower_body("fn f() { return; }");
        assert!(matches!(body[0], HirStmt::Return { value: None, .. }));
    }

    // 29. function parameters and return type are carried on HirFunction
    #[test]
    fn p29_function_signature_carried() {
        let (hir, diags) = lower_src("fn f(a: i64, b: bool) -> i64 { return a; }");
        assert!(diags.is_empty(), "{diags:?}");
        let f = &hir.functions[0];
        assert_eq!(f.name, "f");
        assert_eq!(f.params.len(), 2);
        assert_eq!(f.params[0].ty, Type::I64);
        assert_eq!(f.params[1].ty, Type::Bool);
        assert_eq!(f.return_type, Type::I64);
    }

    // 30. a block-scoped binding does not leak to an outer scope
    #[test]
    fn p30_block_scope_does_not_leak() {
        // `x` is declared inside the `if` block; reading `x` after the block
        // would be an unbound-variable internal error if scopes leaked. Here the
        // outer `return y` only sees the outer binding, so lowering succeeds.
        let (_, diags) = lower_src(
            "fn f(c: bool) -> i64 { let y = 1; if c { let x = 2; print(x); } return y; }",
        );
        assert!(diags.is_empty(), "{diags:?}");
    }

    // 31. an out-of-slice construct (for loop) is reported, not panicked on
    #[test]
    fn p31_unsupported_construct_reports_diagnostic() {
        let (_, diags) = lower_src(
            "import std::collections::Array; \
             fn f(xs: Array<i64>) { for v in xs { print(v); } }",
        );
        assert!(
            diags
                .iter()
                .any(|d| d.message.contains("does not yet support")),
            "expected an unsupported-construct diagnostic, got {diags:?}"
        );
    }

    // 32. multi-statement function lowers every statement in order
    #[test]
    fn p32_multi_statement_function() {
        let body = lower_body("fn f() -> i64 { let a = 1; let b = 2; let c = a + b; return c; }");
        assert_eq!(body.len(), 4);
        assert!(matches!(body[0], HirStmt::Let { .. }));
        assert!(matches!(body[3], HirStmt::Return { .. }));
    }
}
