use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use std::collections::HashMap;

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
        let lty = self.check_expr(&b.lhs);
        let rty = self.check_expr(&b.rhs);

        match &b.op {
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
                if lty == Type::Nil || rty == Type::Nil {
                    self.check_nil_comparison(&lty, &rty, b.span);
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

    pub(super) fn check_block_with_narrowing(
        &mut self,
        block: &Block,
        narrowing: &NilCheckNarrowing,
    ) {
        self.narrowed_vars.push(HashMap::new());
        self.add_narrowing_to_current_scope(narrowing);
        self.check_block(block);
        self.narrowed_vars.pop();
    }

    pub(super) fn check_nil_comparison(&mut self, lty: &Type, rty: &Type, span: Span) {
        match (lty, rty) {
            (Type::Nullable(_), Type::Nil) | (Type::Nil, Type::Nullable(_)) => {}
            (Type::Nil, Type::Nil) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        "cannot compare `nil` with `nil` without a nullable type context",
                    )
                    .with_label(Label::primary(span, "both sides are `nil`"))
                    .with_help("compare a nullable value with `nil` instead"),
                );
            }
            (Type::Nil, other) | (other, Type::Nil) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "cannot compare non-nullable type `{}` with `nil`",
                            type_name(other)
                        ),
                    )
                    .with_label(Label::primary(
                        span,
                        "only nullable values can be compared with `nil`",
                    ))
                    .with_help("make the value nullable with `?` or remove the `nil` comparison"),
                );
            }
            _ => {}
        }
    }

    /// Reject a module-qualified reference to a non-`pub` type (class, interface,
    /// or enum) from another module (willow-7ihl). A module-qualified name
    /// contains `::`; same-module references are unqualified and never checked.
    pub(super) fn check_type_visibility(&mut self, name: &str, span: Span) {
        if !name.contains("::") {
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
