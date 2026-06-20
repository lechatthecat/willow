use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;

use super::*;

impl TypeChecker {
    /// Type-check an array literal `[e0, e1, ...]`. The element type is inferred
    /// from the first element; all elements must agree. An empty literal yields
    /// `Array<Void>`, an unresolved placeholder that a type annotation resolves
    /// (e.g. `let xs: Array<i64> = [];`).
    pub(super) fn check_array_literal(&mut self, elements: &[Expr], span: Span) -> Type {
        self.check_array_literal_expecting(elements, span, None)
    }

    /// Type-check an array literal. When `expected_elem` is given (e.g. from a
    /// `let xs: Array<Animal> = [...]` annotation), each element is checked
    /// against it — this allows a heterogeneous literal of classes that all
    /// implement the same interface, and the literal takes the expected type.
    pub(super) fn check_array_literal_expecting(
        &mut self,
        elements: &[Expr],
        _span: Span,
        expected_elem: Option<&Type>,
    ) -> Type {
        if let Some(expected) = expected_elem {
            for el in elements {
                let ty = self.check_expr_expecting(el, expected);
                if !self.types_compatible(expected, &ty) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            self.type_mismatch_error_code(expected, &ty),
                            format!(
                                "array element expects `{}`, found `{}`",
                                type_name(expected),
                                type_name(&ty)
                            ),
                        )
                        .with_label(Label::primary(el.span(), "mismatched element type")),
                    );
                }
            }
            return Type::Array(Box::new(expected.clone()));
        }

        if elements.is_empty() {
            return Type::Array(Box::new(Type::Void));
        }
        let first_ty = self.check_expr(&elements[0]);
        for el in elements.iter().skip(1) {
            let ty = self.check_expr(el);
            if !self.types_compatible(&first_ty, &ty) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "array elements must have the same type: expected `{}`, found `{}`",
                            type_name(&first_ty),
                            type_name(&ty)
                        ),
                    )
                    .with_label(Label::primary(el.span(), "mismatched element type")),
                );
            }
        }
        Type::Array(Box::new(first_ty))
    }

    /// Type-check an index expression `arr[index]`. `arr` must be `Array<T>` and
    /// `index` must be `i64`; the result type is `T`.
    pub(super) fn check_index(&mut self, arr: &Expr, index: &Expr, span: Span) -> Type {
        let arr_ty = self.check_expr(arr);
        let idx_ty = self.check_expr(index);
        if !matches!(idx_ty, Type::I64) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!("array index must be `i64`, found `{}`", type_name(&idx_ty)),
                )
                .with_label(Label::primary(index.span(), "index is not an `i64`")),
            );
        }
        match &arr_ty {
            Type::Array(elem) => (**elem).clone(),
            // Read-only indexing of an immutable `FrozenArray<T>` (willow-dgwo.7).
            Type::Generic(name, args) if name == "FrozenArray" && args.len() == 1 => {
                args[0].clone()
            }
            // Recover quietly from an earlier error that produced Void.
            Type::Void => Type::Void,
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("cannot index a value of type `{}`", type_name(other)),
                    )
                    .with_label(Label::primary(span, "not an array"))
                    .with_help("indexing with `[..]` requires an `Array<T>` or `FrozenArray<T>`"),
                );
                Type::Void
            }
        }
    }

    /// Builtin methods on `Array<T>`. Returns `Some(ret)` when `obj_ty` is an
    /// array (handling the method or reporting an unknown one), `None` otherwise.
    pub(super) fn check_array_method_call(
        &mut self,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<Type> {
        let Type::Array(elem) = obj_ty else {
            return None;
        };
        match m.method.as_str() {
            "len" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Array::len` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some(Type::I64)
            }
            "push" => {
                let elem_ty = (**elem).clone();
                if m.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Array::push` expects 1 argument, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `push(value)`")),
                    );
                } else {
                    let v = self.check_expr(&m.args[0].expr);
                    if !self.types_compatible(&elem_ty, &v) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "cannot push `{}` to `Array<{}>`",
                                    type_name(&v),
                                    type_name(&elem_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                m.args[0].expr.span(),
                                "wrong element type",
                            )),
                        );
                    }
                }
                Some(Type::Void)
            }
            "pop" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Array::pop` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some((**elem).clone())
            }
            // `.freeze()` -> an immutable `FrozenArray<T>` copy that is Sync when
            // T is Sync, so it can be shared across tasks (willow-dgwo.7).
            "freeze" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Array::freeze` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some(Type::Generic(
                    "FrozenArray".to_string(),
                    vec![(**elem).clone()],
                ))
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("no method `{}` on `Array<{}>`", other, type_name(elem)),
                    )
                    .with_label(Label::primary(m.span, "unknown array method"))
                    .with_help(
                        "arrays support `.len()`, `.push(v)`, `.pop()`, `.freeze()`, and indexing `arr[i]`",
                    ),
                );
                Some(Type::Void)
            }
        }
    }

    /// Builtin methods on the immutable `FrozenArray<T>` (willow-dgwo.7): only
    /// `.len()` plus read-only indexing `fa[i]`; mutation methods are rejected.
    pub(super) fn check_frozen_array_method_call(
        &mut self,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<Type> {
        let Type::Generic(name, args) = obj_ty else {
            return None;
        };
        if name != "FrozenArray" || args.len() != 1 {
            return None;
        }
        match m.method.as_str() {
            "len" => Some(Type::I64),
            "push" | "pop" | "set" => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("`FrozenArray` is immutable; `{}` is not allowed", m.method),
                    )
                    .with_label(Label::primary(m.span, "frozen arrays cannot be mutated"))
                    .with_help(
                        "freeze a copy of a mutable `Array<T>`; read it with `[i]` / `.len()`",
                    ),
                );
                Some(Type::Void)
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{other}` on `FrozenArray<{}>`",
                            type_name(&args[0])
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown method"))
                    .with_help("`FrozenArray` supports `.len()` and indexing `fa[i]`"),
                );
                Some(Type::Void)
            }
        }
    }

    /// Builtin methods on `Map<K, V>`: `insert(k, v)`, `get(k) -> Option<V>`,
    /// `contains(k) -> bool`, `len() -> i64`. Returns `Some(ret)` when `obj_ty`
    /// is a map, `None` otherwise.
    pub(super) fn check_map_method_call(
        &mut self,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<Type> {
        let Type::Generic(name, args) = obj_ty else {
            return None;
        };
        if name != "Map" || args.len() != 2 {
            return None;
        }
        let key_ty = args[0].clone();
        let val_ty = args[1].clone();

        let check_key = |checker: &mut Self, arg: &CallArg| {
            let k = checker.check_expr(&arg.expr);
            if key_ty != Type::Void && !checker.types_compatible(&key_ty, &k) {
                checker.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "map key type mismatch: expected `{}`, found `{}`",
                            type_name(&key_ty),
                            type_name(&k)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong key type")),
                );
            }
        };

        match m.method.as_str() {
            "insert" => {
                if m.args.len() != 2 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Map::insert` expects 2 arguments, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `insert(key, value)`")),
                    );
                } else {
                    check_key(self, &m.args[0]);
                    let v = self.check_expr(&m.args[1].expr);
                    if val_ty != Type::Void && !self.types_compatible(&val_ty, &v) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "map value type mismatch: expected `{}`, found `{}`",
                                    type_name(&val_ty),
                                    type_name(&v)
                                ),
                            )
                            .with_label(Label::primary(m.args[1].expr.span(), "wrong value type")),
                        );
                    }
                }
                Some(Type::Void)
            }
            "get" => {
                if m.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Map::get` expects 1 argument, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `get(key)`")),
                    );
                } else {
                    check_key(self, &m.args[0]);
                }
                Some(Type::Generic("Option".to_string(), vec![val_ty]))
            }
            "contains" => {
                if m.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("`Map::contains` expects 1 argument, got {}", m.args.len()),
                        )
                        .with_label(Label::primary(m.span, "expected `contains(key)`")),
                    );
                } else {
                    check_key(self, &m.args[0]);
                }
                Some(Type::Bool)
            }
            "len" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Map::len` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some(Type::I64)
            }
            // `.freeze()` -> an immutable `FrozenMap<K,V>` copy, Sync when K,V are
            // Sync, so it can be shared across tasks (willow-dgwo.10).
            "freeze" => {
                if !m.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            "`Map::freeze` takes no arguments",
                        )
                        .with_label(Label::primary(m.span, "unexpected arguments")),
                    );
                }
                Some(Type::Generic("FrozenMap".to_string(), vec![key_ty, val_ty]))
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{}` on `Map<{}, {}>`",
                            other,
                            type_name(&key_ty),
                            type_name(&val_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown map method"))
                    .with_help(
                        "maps support `.insert(k, v)`, `.get(k)`, `.contains(k)`, `.len()`, `.freeze()`",
                    ),
                );
                Some(Type::Void)
            }
        }
    }

    /// Builtin methods on the immutable `FrozenMap<K, V>` (willow-dgwo.10):
    /// read-only `.get(k) -> Option<V>`, `.contains(k) -> bool`, `.len() -> i64`;
    /// `insert`/`remove` are rejected.
    pub(super) fn check_frozen_map_method_call(
        &mut self,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<Type> {
        let Type::Generic(name, args) = obj_ty else {
            return None;
        };
        if name != "FrozenMap" || args.len() != 2 {
            return None;
        }
        let key_ty = args[0].clone();
        let val_ty = args[1].clone();
        if let Some(arg) = m.args.first() {
            let k = self.check_expr(&arg.expr);
            if matches!(m.method.as_str(), "get" | "contains")
                && key_ty != Type::Void
                && !self.types_compatible(&key_ty, &k)
            {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "map key type mismatch: expected `{}`, found `{}`",
                            type_name(&key_ty),
                            type_name(&k)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong key type")),
                );
            }
        }
        match m.method.as_str() {
            "get" => Some(Type::Generic("Option".to_string(), vec![val_ty])),
            "contains" => Some(Type::Bool),
            "len" => Some(Type::I64),
            "insert" | "remove" => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("`FrozenMap` is immutable; `{}` is not allowed", m.method),
                    )
                    .with_label(Label::primary(m.span, "frozen maps cannot be mutated"))
                    .with_help("freeze a copy of a mutable `Map<K, V>`; read it with `.get`/`.contains`/`.len`"),
                );
                Some(Type::Void)
            }
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{other}` on `FrozenMap<{}, {}>`",
                            type_name(&key_ty),
                            type_name(&val_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown method"))
                    .with_help("`FrozenMap` supports `.get(k)`, `.contains(k)`, `.len()`"),
                );
                Some(Type::Void)
            }
        }
    }
}
