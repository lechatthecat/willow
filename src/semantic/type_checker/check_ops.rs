use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;

use super::*;

#[willow_continuations::checker]
impl TypeChecker {
    pub(super) fn check_object_literal(&mut self, literal: &ObjectLiteralExpr) -> Type {
        for field in &literal.fields {
            self.check_expr(&field.value);
        }
        let mut diagnostic = Diagnostic::new(
            Severity::Error,
            ErrorCode::E0847,
            format!(
                "object literal construction for `{}` is no longer supported",
                literal.class
            ),
        )
        .with_label(Label::primary(literal.span, "object literal used here"))
        .with_help(format!(
            "use `new {}(...)` and pass fields in constructor order",
            literal.class
        ));
        if let Some(field) = literal.fields.first() {
            diagnostic = diagnostic.with_label(Label::secondary(
                field.span,
                "named field syntax is part of the old construction form",
            ));
        }
        self.push(diagnostic);
        Type::Named(literal.class.clone())
    }

    /// Traverse contiguous unary/binary trees on the heap. Non-operator leaves
    /// still use normal checking so contextual and callable state is preserved.
    pub(super) fn check_operator_tree(&mut self, expr: &Expr) -> Type {
        enum Work<'a> {
            Enter(&'a Expr),
            AfterLeft(&'a BinaryExpr),
            FinishBinary(&'a BinaryExpr, Type),
            AfterRightForBareNoneLhs(&'a BinaryExpr),
            FinishUnary(&'a UnaryExpr),
        }
        let mut work = vec![Work::Enter(expr)];
        let mut value = Type::Void;
        while let Some(next) = work.pop() {
            match next {
                Work::Enter(Expr::Binary(binary)) => {
                    if matches!(binary.op, BinOp::Eq | BinOp::Ne)
                        && self.is_unshadowed_bare_none(&binary.lhs)
                    {
                        // A bare None on the left derives its Option context
                        // from the right, retaining the original evaluation order.
                        work.push(Work::AfterRightForBareNoneLhs(binary));
                        work.push(Work::Enter(&binary.rhs));
                    } else {
                        work.push(Work::AfterLeft(binary));
                        work.push(Work::Enter(&binary.lhs));
                    }
                }
                Work::Enter(Expr::Unary(unary)) => {
                    work.push(Work::FinishUnary(unary));
                    work.push(Work::Enter(&unary.expr));
                }
                Work::Enter(leaf) => value = self.check_expr(leaf),
                Work::AfterLeft(binary) => {
                    let left = value;
                    if matches!(binary.op, BinOp::Eq | BinOp::Ne)
                        && is_option_type(&left)
                        && self.is_unshadowed_bare_none(&binary.rhs)
                    {
                        let right = self.check_expr_expecting(&binary.rhs, &left);
                        value = self.finish_binary(binary, left, right);
                        self.expr_types.insert(binary.id, value.clone());
                    } else {
                        work.push(Work::FinishBinary(binary, left));
                        work.push(Work::Enter(&binary.rhs));
                        value = Type::Void;
                    }
                }
                Work::FinishBinary(binary, left) => {
                    value = self.finish_binary(binary, left, value);
                    self.expr_types.insert(binary.id, value.clone());
                }
                Work::AfterRightForBareNoneLhs(binary) => {
                    let right = value;
                    let left = if is_option_type(&right) {
                        self.check_expr_expecting(&binary.lhs, &right)
                    } else {
                        self.check_expr(&binary.lhs)
                    };
                    value = self.finish_binary(binary, left, right);
                    self.expr_types.insert(binary.id, value.clone());
                }
                Work::FinishUnary(unary) => {
                    value = self.finish_unary(unary, value);
                    self.expr_types.insert(unary.id, value.clone());
                }
            }
        }
        value
    }

    fn finish_binary(&mut self, b: &BinaryExpr, lty: Type, rty: Type) -> Type {
        match &b.op {
            // `**` is deliberately narrower than the other arithmetic operators:
            // only `i64 ** i64 -> i64` and `f64 ** f64 -> f64` are defined
            // (willow-n5yv.2).  There is no mixed-type form, so `2 ** 0.5` is an
            // error rather than an implicit widening, and `String ** String` gets
            // an exponent-specific message instead of the concatenation help text.
            BinOp::Pow => {
                if lty == Type::I64 && rty == Type::I64 {
                    // `i64 ** i64` is lowered natively (willow-n5yv.3). A
                    // literal negative exponent has no integer result, so it is
                    // a compile error rather than a guaranteed runtime panic.
                    if let Some(span) = negative_exponent_literal_span(&b.rhs) {
                        self.push(negative_exponent_literal(span));
                    }
                    return Type::I64;
                }
                if lty == Type::F64 && rty == Type::F64 {
                    return Type::F64;
                }

                let mut diagnostic = Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0202,
                    format!(
                        "cannot raise `{}` to the power of `{}`",
                        type_name(&lty),
                        type_name(&rty)
                    ),
                )
                .with_label(Label::primary(
                    b.span,
                    format!(
                        "`**` is defined for `i64 ** i64` and `f64 ** f64`, not `{} ** {}`",
                        type_name(&lty),
                        type_name(&rty)
                    ),
                ));
                if (lty == Type::I64 && rty == Type::F64) || (lty == Type::F64 && rty == Type::I64)
                {
                    diagnostic = diagnostic.with_help(
                        "`**` does not mix `i64` and `f64`; make both operands the same type",
                    );
                }
                self.push(diagnostic);

                // Recover with the base type when it is numeric so a single bad
                // exponent does not cascade into unrelated errors downstream.
                if lty == Type::I64 || lty == Type::F64 {
                    lty
                } else {
                    Type::I64
                }
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                if b.op == BinOp::Add && lty == Type::String && rty == Type::String {
                    return Type::String;
                }

                // String concatenation is strongly typed: `String + non-String`
                // (or the reverse) is rejected with a `toString()` suggestion
                // rather than an implicit stringify (willow-fvfc).
                if b.op == BinOp::Add && (lty == Type::String || rty == Type::String) {
                    let (non_str, side) = if lty == Type::String {
                        (&rty, "right")
                    } else {
                        (&lty, "left")
                    };
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "cannot concatenate `String` with `{}`",
                                type_name(non_str)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!("the {side} operand is `{}`, not `String`", type_name(non_str)),
                        ))
                        .with_help(
                            "convert explicitly with `.toString()`, e.g. `\"x = \" + value.toString()`",
                        ),
                    );
                    return Type::String;
                }

                if (lty != Type::I64 && lty != Type::F64) || lty != rty {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "cannot apply operator `{}` to `{}` and `{}`",
                                b.op.symbol(),
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!(
                                "`{}` not defined for `{}` and `{}`",
                                b.op.symbol(),
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )),
                    );
                }
                lty
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if (lty != Type::I64 && lty != Type::F64) || lty != rty {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "cannot compare `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!(
                                "comparison not defined for `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )),
                    );
                }
                Type::Bool
            }
            BinOp::Eq | BinOp::Ne => {
                if is_option_type(&lty) || is_option_type(&rty) {
                    let operator = b.op.symbol();
                    let mut diagnostic = Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("`Option<T>` does not support `{operator}`"),
                    )
                    .with_label(Label::primary(
                        b.span,
                        "Option values do not have structural equality",
                    ));
                    if let Some(receiver) = self.option_none_comparison_receiver(b, &lty, &rty) {
                        let predicate = if b.op == BinOp::Eq {
                            "is_none"
                        } else {
                            "is_some"
                        };
                        diagnostic =
                            diagnostic.with_help(format!("use `{receiver}.{predicate}()`"));
                    } else {
                        diagnostic =
                            diagnostic.with_help("use `is_none()`, `is_some()`, or `match`");
                    }
                    self.push(diagnostic);
                    return Type::Bool;
                }
                if !self.types_compatible(&lty, &rty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "mismatched types: `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(
                            b.span,
                            format!(
                                "cannot compare `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )),
                    );
                }
                Type::Bool
            }
            BinOp::And | BinOp::Or => {
                if lty != Type::Bool || rty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!(
                                "logical operator requires `bool` operands, found `{}` and `{}`",
                                type_name(&lty),
                                type_name(&rty)
                            ),
                        )
                        .with_label(Label::primary(b.span, "operands must be `bool`")),
                    );
                }
                Type::Bool
            }
        }
    }

    fn is_unshadowed_bare_none(&self, expr: &Expr) -> bool {
        matches!(expr, Expr::Var(name, _, _) if name == "None" && self.prelude_variant_name_is_unshadowed(name))
    }

    fn option_none_comparison_receiver<'a>(
        &self,
        binary: &'a BinaryExpr,
        left_ty: &Type,
        right_ty: &Type,
    ) -> Option<&'a str> {
        if is_option_none_expr(self, &binary.rhs)
            && is_option_type(left_ty)
            && let Expr::Var(name, _, _) = &binary.lhs
        {
            return Some(name);
        }
        if is_option_none_expr(self, &binary.lhs)
            && is_option_type(right_ty)
            && let Expr::Var(name, _, _) = &binary.rhs
        {
            return Some(name);
        }
        None
    }

    fn finish_unary(&mut self, u: &UnaryExpr, ty: Type) -> Type {
        match &u.op {
            UnaryOp::Neg => {
                if ty != Type::I64 && ty != Type::F64 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!("unary `-` cannot be applied to `{}`", type_name(&ty)),
                        )
                        .with_label(Label::primary(
                            u.span,
                            format!("requires `i64` or `f64`, found `{}`", type_name(&ty)),
                        )),
                    );
                }
                ty
            }
            UnaryOp::Not => {
                if ty != Type::Bool {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0202,
                            format!("unary `!` cannot be applied to `{}`", type_name(&ty)),
                        )
                        .with_label(Label::primary(
                            u.span,
                            format!("requires `bool`, found `{}`", type_name(&ty)),
                        )),
                    );
                }
                Type::Bool
            }
        }
    }

    /// Type-check `ClassName::property = value` (willow-qsqf §5/§13.4): the
    /// property must be `static mut`, the value must match its type, and
    /// visibility must allow the write.
    pub(super) fn check_static_field_assign(&mut self, s: &StaticFieldAssignStmt) {
        let Some(resolved) = self.resolve_static_call_class_name(&s.class, s.span) else {
            self.check_expr(&s.value);
            return;
        };
        let Some((owner, info)) = self.lookup_static_prop_in_hierarchy(&resolved, &s.field) else {
            self.check_expr(&s.value);
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0502,
                    format!("no static property `{}::{}`", resolved, s.field),
                )
                .with_label(Label::primary(s.span, "static property not found")),
            );
            return;
        };
        let val_ty = self.check_expr_expecting(&s.value, &info.ty);
        if !info.is_mut {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0832,
                    format!(
                        "cannot assign to immutable static property `{}::{}`",
                        owner, s.field
                    ),
                )
                .with_label(Label::primary(s.span, "cannot assign to immutable static"))
                .with_help("declare it as `static mut` if shared mutation is intended"),
            );
            return;
        }
        // Visibility: a private/protected static can only be written from inside.
        if !info.public {
            let allowed = if info.protected {
                self.can_access_protected_member(&owner)
            } else {
                self.can_access_private_member(&owner)
            };
            if !allowed {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0419,
                        format!("static property `{}::{}` is private", owner, s.field),
                    )
                    .with_label(Label::primary(s.span, "private static property")),
                );
            }
        }
        if info.ty != Type::Void && !self.types_compatible(&info.ty, &val_ty) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    self.type_mismatch_error_code(&info.ty, &val_ty),
                    format!(
                        "mismatched types: expected `{}`, found `{}`",
                        type_name(&info.ty),
                        type_name(&val_ty)
                    ),
                )
                .with_label(Label::primary(
                    s.span,
                    format!("expected `{}`", type_name(&info.ty)),
                )),
            );
        }
    }

    /// Reject a module-qualified reference to a non-`pub` type (class, interface,
    /// or enum) from another module (willow-7ihl). A module-qualified name
    /// contains `::`; same-module references are unqualified and never checked.
    pub(super) fn check_type_visibility(&mut self, name: &str, span: Span) {
        let Some((namespace, _)) = name.rsplit_once("::") else {
            return;
        };
        // A module's own declarations carry its canonical identity now
        // (willow-itcw), so a private enum a module uses in its OWN body reaches
        // here fully qualified. It is at home; only the units that had to import
        // it are asking about visibility.
        if self.module_path.as_deref() == Some(namespace) {
            return;
        }
        let (is_private, kind) = if let Some(c) = self.symbols.lookup_class(name) {
            (!c.public, "class")
        } else if let Some(i) = self.symbols.lookup_interface(name) {
            (!i.public, "interface")
        } else if let Some(e) = self.symbols.lookup_enum(name) {
            (!e.public, "enum")
        } else {
            return;
        };
        if is_private {
            let simple = name.rsplit("::").next().unwrap_or(name);
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0419,
                    format!("{kind} `{name}` is private to its module"),
                )
                .with_label(Label::primary(
                    span,
                    "private type accessed from another module",
                ))
                .with_help(format!(
                    "mark it `pub {kind} {simple}` to use it outside its module"
                )),
            );
        }
    }
}

fn is_option_type(ty: &Type) -> bool {
    crate::semantic::builtin_types::unary_arg(
        ty,
        crate::semantic::builtin_types::BuiltinTypeId::Option,
    )
    .is_some()
}

fn is_option_none_expr(checker: &TypeChecker, expr: &Expr) -> bool {
    checker.is_unshadowed_bare_none(expr)
        || matches!(expr,
            Expr::StaticCall(call)
                if call.class == "Option" && call.method == "None" && call.args.is_empty())
}

/// The span of a syntactically negative exponent literal (`x ** -3`), if that
/// is what `exponent` is.
fn negative_exponent_literal_span(exponent: &Expr) -> Option<Span> {
    let Expr::Unary(unary) = exponent else {
        return None;
    };
    if unary.op != UnaryOp::Neg {
        return None;
    }
    // `- 0` is still zero, and `x ** 0` is 1, so only a non-zero magnitude is
    // an error.
    match unary.expr {
        Expr::Integer(0, _, _) => None,
        Expr::Integer(_, _, _) => Some(unary.span),
        _ => None,
    }
}

/// `i64 ** i64` has no fractional result, so a negative exponent can only ever
/// panic. When the exponent is written as a literal the compiler can say so
/// before the program runs (willow-n5yv.3).
pub(crate) fn negative_exponent_literal(span: Span) -> Diagnostic {
    Diagnostic::new(
        Severity::Error,
        ErrorCode::E0204,
        "negative exponent in an integer `**`",
    )
    .with_label(Label::primary(
        span,
        "integer exponentiation has no fractional result",
    ))
    .with_help("use `f64` operands (`2.0 ** -3.0`), or compute `1 / (x ** 3)` explicitly")
}

#[cfg(test)]
mod operator_tree_tests {
    use super::*;

    #[test]
    fn operator_types_and_errors_cover_twenty_four_perspectives() {
        let cases = [
            ("1 + 2", Type::I64, 0),
            ("3 - 1", Type::I64, 0),
            ("2 * 4", Type::I64, 0),
            ("8 / 2", Type::I64, 0),
            ("9 % 2", Type::I64, 0),
            ("2 ** 3", Type::I64, 0),
            ("1.0 + 2.0", Type::F64, 0),
            ("2.0 ** 3.0", Type::F64, 0),
            ("1 < 2", Type::Bool, 0),
            ("1 <= 2", Type::Bool, 0),
            ("2 > 1", Type::Bool, 0),
            ("2 >= 1", Type::Bool, 0),
            ("1 == 2", Type::Bool, 0),
            ("1 != 2", Type::Bool, 0),
            ("true && false", Type::Bool, 0),
            ("true || false", Type::Bool, 0),
            ("-1", Type::I64, 0),
            ("-1.0", Type::F64, 0),
            ("!false", Type::Bool, 0),
            ("-(1 + 2) * -3", Type::I64, 0),
            ("1 + true", Type::I64, 1),
            ("-false", Type::Bool, 1),
            ("!1", Type::Bool, 1),
            ("2 ** -1", Type::I64, 1),
        ];
        for (expression, expected, error_count) in cases {
            let source = format!("fn probe() {{ let value = {expression}; }}");
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (program, errors) = crate::parser::Parser::new(tokens).parse();
            assert!(errors.is_empty(), "{expression}: {errors:?}");
            let Item::Function(function) = &program.items[0] else {
                unreachable!()
            };
            let Stmt::Let(binding) = &function.body.stmts[0] else {
                unreachable!()
            };
            let mut checker = TypeChecker::new();
            assert_eq!(checker.check_expr(&binding.init), expected, "{expression}");
            assert_eq!(
                checker.errors.len(),
                error_count,
                "{expression}: {:?}",
                checker.errors
            );
            for event in crate::parser::iter::AstWalk::new(crate::parser::iter::AstEvent::Expr(
                &binding.init,
            )) {
                if let crate::parser::iter::AstEvent::Expr(expr) = event {
                    assert!(checker.expr_types.contains_key(&expr.id()), "{expression}");
                }
            }
        }
    }

    #[test]
    fn mixed_operator_tree_uses_a_one_megabyte_stack() {
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let mut checker = TypeChecker::new();
                let span = Span::new(0, 0, 1, 1);
                let mut expr = Expr::Integer(1, span, ExprId::fresh());
                for index in 0..50_000 {
                    expr = if index % 2 == 0 {
                        Expr::Unary(Box::new(UnaryExpr {
                            id: ExprId::fresh(),
                            op: UnaryOp::Neg,
                            expr,
                            span,
                        }))
                    } else {
                        Expr::Binary(Box::new(BinaryExpr {
                            id: ExprId::fresh(),
                            op: BinOp::Add,
                            lhs: expr,
                            rhs: Expr::Integer(1, span, ExprId::fresh()),
                            span,
                        }))
                    };
                }
                let result = checker.check_expr(&expr);
                let type_count = checker.expr_types.len();
                let errors = checker.errors;
                drop(expr);
                assert_eq!(result, Type::I64);
                assert_eq!(type_count, 75_001);
                assert!(errors.is_empty(), "{errors:?}");
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
