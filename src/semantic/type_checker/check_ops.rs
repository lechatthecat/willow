use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;

use super::*;

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

    pub(super) fn check_binary(&mut self, b: &BinaryExpr) -> Type {
        let (lty, rty) = if matches!(b.op, BinOp::Eq | BinOp::Ne) {
            // Give a bare `None` the other operand's Option context so it is
            // diagnosed as the forbidden Option equality, not as an unknown
            // variable. A user declaration named `None` shadows this path.
            if self.is_unshadowed_bare_none(&b.lhs) {
                let rty = self.check_expr(&b.rhs);
                let lty = if is_option_type(&rty) {
                    self.check_expr_expecting(&b.lhs, &rty)
                } else {
                    self.check_expr(&b.lhs)
                };
                (lty, rty)
            } else {
                let lty = self.check_expr(&b.lhs);
                let rty = if is_option_type(&lty) && self.is_unshadowed_bare_none(&b.rhs) {
                    self.check_expr_expecting(&b.rhs, &lty)
                } else {
                    self.check_expr(&b.rhs)
                };
                (lty, rty)
            }
        } else {
            (self.check_expr(&b.lhs), self.check_expr(&b.rhs))
        };

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
        matches!(expr, Expr::Var(name, _) if name == "None" && self.prelude_variant_name_is_unshadowed(name))
    }

    fn option_none_comparison_receiver<'a>(
        &self,
        binary: &'a BinaryExpr,
        left_ty: &Type,
        right_ty: &Type,
    ) -> Option<&'a str> {
        if is_option_none_expr(self, &binary.rhs)
            && is_option_type(left_ty)
            && let Expr::Var(name, _) = &binary.lhs
        {
            return Some(name);
        }
        if is_option_none_expr(self, &binary.lhs)
            && is_option_type(right_ty)
            && let Expr::Var(name, _) = &binary.rhs
        {
            return Some(name);
        }
        None
    }

    pub(super) fn check_unary(&mut self, u: &UnaryExpr) -> Type {
        let ty = self.check_expr(&u.expr);
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
        Expr::Integer(0, _) => None,
        Expr::Integer(_, _) => Some(unary.span),
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
