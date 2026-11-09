use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity, Span};
use crate::parser::ast::*;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};

use super::*;

#[cfg(test)]
thread_local! {
    pub(super) static ARRAY_ELEMENT_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[willow_continuations::checker]
impl TypeChecker {
    /// Type-check an array literal `[e0, e1, ...]`. The element type is inferred
    /// from the first element; all elements must agree. An empty literal yields
    /// `Array<Void>`, an unresolved placeholder that a type annotation resolves
    /// (e.g. `let xs: Array<i64> = [];`).
    pub(super) fn check_array_literal(&mut self, elements: &[Expr]) -> Type {
        self.check_array_literal_expecting(elements, None)
    }

    /// Type-check an array literal. When `expected_elem` is given (e.g. from a
    /// `let xs: Array<Animal> = [...]` annotation), each element is checked
    /// against it — this allows a heterogeneous literal of classes that all
    /// implement the same interface, and the literal takes the expected type.
    pub(super) fn check_array_literal_expecting(
        &mut self,
        elements: &[Expr],
        expected_elem: Option<&Type>,
    ) -> Type {
        // Both callers return through check_expr/check_expr_expecting, which
        // record the authoritative literal type once for downstream lowering.
        #[cfg(test)]
        ARRAY_ELEMENT_CHECKS.with(|count| count.set(count.get() + elements.len()));
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
                                self.diagnostic_type_name(expected),
                                self.diagnostic_type_name(&ty)
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
                            self.diagnostic_type_name(&first_ty),
                            self.diagnostic_type_name(&ty)
                        ),
                    )
                    .with_label(Label::primary(el.span(), "mismatched element type"))
                    .with_help("if the elements implement a common interface, annotate the array as `Array<InterfaceName>`"),
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
        if !matches!(idx_ty, Type::I64) && !Self::contains_error_type(&idx_ty) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "array index must be `i64`, found `{}`",
                        self.diagnostic_type_name(&idx_ty)
                    ),
                )
                .with_label(Label::primary(index.span(), "index is not an `i64`")),
            );
        }
        if Self::contains_error_type(&arr_ty) {
            return Self::error_type();
        }
        match &arr_ty {
            Type::Array(elem) | Type::FixedArray(elem, _) => (**elem).clone(),
            // Read-only indexing of an immutable `FrozenArray<T>` (willow-dgwo.7).
            Type::Generic(_, _)
                if let Some(elem) = builtin_types::unary_arg(&arr_ty, B::FrozenArray) =>
            {
                elem.clone()
            }
            // Recover quietly from an earlier error that produced Void.
            Type::Void => Type::Void,
            other => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "cannot index a value of type `{}`",
                            self.diagnostic_type_name(other)
                        ),
                    )
                    .with_label(Label::primary(span, "not an array"))
                    .with_help("indexing with `[..]` requires an `Array<T>` or `FrozenArray<T>`"),
                );
                Type::Void
            }
        }
    }

    /// Source methods on array storage. Returns `Some(ret)` when `obj_ty` is an
    /// array (handling the method or reporting an unknown one), `None` otherwise.
    pub(super) fn check_array_method_call(
        &mut self,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<Type> {
        let (Type::Array(elem) | Type::FixedArray(elem, _)) = obj_ty else {
            return None;
        };
        if matches!(obj_ty, Type::FixedArray(..))
            && matches!(m.method.as_str(), "push" | "pop" | "insert" | "remove")
        {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    "fixed arrays cannot be resized",
                )
                .with_label(Label::primary(m.span, "resizing a fixed array")),
            );
            self.check_discarded_builtin_args(&m.args);
            return Some(Self::error_type());
        }
        let template = self
            .generics
            .classes
            .get("$std::collections::Array")
            .cloned()
            .or_else(|| {
                self.generics
                    .classes
                    .get(B::Array.source_name())
                    .filter(|c| c.abi_storage.is_some())
                    .cloned()
            });
        if !m.method.starts_with("__abi_") {
            let storage = Type::Array(elem.clone());
            if template.is_none()
                && let Some(owner) = self.array_class(&storage).cloned()
            {
                self.static_call_classes
                    .insert(m.id, TypeId::from_source_name(&owner));
                self.record_lock_effect_call(
                    FunctionId::method(TypeId::from_source_name(&owner), &m.method),
                    m.span,
                );
                return Some(self.resolve_method(&Type::Named(owner), m, false));
            }
            if let Some(template) = template {
                let declared = template
                    .methods
                    .iter()
                    .find(|method| method.name == m.method);
                if declared.is_none() {
                    let methods = template
                        .methods
                        .iter()
                        .filter(|method| method.public && !method.is_static)
                        .map(|method| method.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let mut diagnostic = Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{}` on type `{}`",
                            m.method,
                            self.diagnostic_type_name(obj_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown method"))
                    .with_help(format!("available methods: {methods}"));
                    self.label_receiver_call_origin(&mut diagnostic, &m.object, obj_ty);
                    self.push(diagnostic);
                    self.check_discarded_builtin_args(&m.args);
                    return Some(Self::error_type());
                }
                if **elem == Type::Void {
                    let method = declared.expect("checked source declaration");
                    let depends = |ty: &Type| {
                        super::generics::mentions(ty, |name| {
                            template
                                .type_params
                                .iter()
                                .any(|parameter| parameter == name)
                        })
                    };
                    if method.constraint.is_some()
                        || depends(&method.return_type)
                        || method.params.iter().any(|parameter| depends(&parameter.ty))
                    {
                        self.generic_error(m.span, "array method requires a known element type");
                        return Some(Self::error_type());
                    }
                    // An empty literal has no element representation yet. Source
                    // signatures independent of that element can still be checked;
                    // executable storage continues to require a concrete type.
                    self.check_call_argument_count(
                        &m.method,
                        method.params.len(),
                        m.args.len(),
                        m.span,
                    );
                    self.check_value_call_args(
                        &method
                            .params
                            .iter()
                            .map(|parameter| parameter.ty.clone())
                            .collect::<Vec<_>>(),
                        &m.args,
                    );
                    return Some(method.return_type.clone());
                }
                if let Some(bound) = template
                    .methods
                    .iter()
                    .find(|method| method.name == m.method)
                    .and_then(|method| method.constraint.as_ref())
                    && !bound.alternatives.contains(elem.as_ref())
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "method `{}` is unavailable for `{}`",
                                m.method,
                                self.diagnostic_type_name(obj_ty)
                            ),
                        )
                        .with_label(Label::primary(m.span, "method constraint is not satisfied")),
                    );
                    self.check_discarded_builtin_args(&m.args);
                    return Some(Self::error_type());
                }
                let access = if self
                    .generics
                    .classes
                    .contains_key("$std::collections::Array")
                {
                    "$std::collections::Array"
                } else {
                    B::Array.source_name()
                };
                if let Some(receiver) =
                    self.instantiate_class(access, &[elem.as_ref().clone()], m.span)
                {
                    if let Type::Named(owner) = &receiver {
                        self.static_call_classes
                            .insert(m.id, TypeId::from_source_name(owner));
                    }
                    for callee in self.method_effect_ids(&Type::Array(elem.clone()), &m.method) {
                        self.record_lock_effect_call(callee, m.span);
                    }
                    return Some(self.resolve_method(&receiver, m, false));
                }
                self.generic_error(
                    template.span,
                    "bundled Array declaration could not be instantiated",
                );
            } else {
                self.generic_error(m.span, "bundled Array declaration is unavailable");
            }
            return Some(Self::error_type());
        }
        if !self.array_abi_files.contains(&m.span.file_id) {
            self.generic_error(
                m.span,
                "array ABI operations are private to the bundled declaration",
            );
            return Some(Self::error_type());
        }
        let (params, result) = match m.method.as_str() {
            "__abi_len" => (vec![], Type::I64),
            "__abi_resize" => (vec![Type::I64], Type::Void),
            "__abi_copy" => (
                vec![],
                Type::Generic("FrozenArray".into(), vec![elem.as_ref().clone()]),
            ),
            "__abi_concat" if **elem == Type::String => (vec![], Type::String),
            _ => {
                self.generic_error(m.span, "unknown array storage operation");
                return Some(Self::error_type());
            }
        };
        self.check_call_argument_count(&m.method, params.len(), m.args.len(), m.span);
        self.check_value_call_args(&params, &m.args);
        Some(result)
    }

    pub(super) fn check_frozen_array_method_call(
        &mut self,
        obj_ty: &Type,
        m: &MethodCallExpr,
    ) -> Option<Type> {
        let elem_ty = builtin_types::unary_arg(obj_ty, B::FrozenArray)?;
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
                            self.diagnostic_type_name(elem_ty)
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
        let (key_ty, val_ty) = builtin_types::binary_args(obj_ty, B::Map)?;
        let key_ty = key_ty.clone();
        let val_ty = val_ty.clone();

        if key_ty == Type::Void
            && val_ty == Type::Void
            && m.method == "insert"
            && m.args.len() == 2
            && let Expr::Var(name, _, _) = &m.object
            && let Some(info) = self.symbols.lookup_var(name).cloned()
            && self
                .local
                .inferred_maps
                .contains_key(&info.declaration_span)
        {
            let key = self.check_expr(&m.args[0].expr);
            let value = self.check_expr(&m.args[1].expr);
            if key != Type::Void && value != Type::Void {
                let ty = B::Map.apply(vec![key, value]);
                let (uses, async_slot) = self
                    .local
                    .inferred_maps
                    .remove(&info.declaration_span)
                    .unwrap();
                for id in uses {
                    self.local.pending_map_uses.remove(&id);
                    self.expr_types.insert(id, ty.clone());
                }
                if let Some(slot) = async_slot {
                    self.local.async_local_types[slot] = ty.clone();
                }
                self.symbols.lookup_var_mut(name).unwrap().ty = ty;
            }
            return Some(Type::Void);
        }

        if (key_ty == Type::Void || val_ty == Type::Void)
            && matches!(
                m.method.as_str(),
                "get" | "contains" | "freeze" | "toString"
            )
        {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    "cannot use map contents before key and value types are known",
                )
                .with_label(Label::primary(m.span, "map needs type information"))
                .with_help("insert a key and value first, or add a Map<K, V> annotation"),
            );
        }

        if let Some((arity, key, value)) = super::builtin_methods::map_method(&m.method)
            && self.check_builtin_arity(
                arity,
                B::Map.source_name(),
                &m.method,
                m.args.len(),
                m.span,
            )
        {
            if key {
                let got = self.check_expr(&m.args[0].expr);
                self.check_builtin_argument(
                    BuiltinArgument::MapKey,
                    B::Map.source_name(),
                    &m.method,
                    &key_ty,
                    &got,
                    m.args[0].expr.span(),
                );
            }
            if value {
                let got = self.check_expr_expecting(&m.args[1].expr, &val_ty);
                self.check_builtin_argument(
                    BuiltinArgument::MapValue,
                    B::Map.source_name(),
                    &m.method,
                    &val_ty,
                    &got,
                    m.args[1].expr.span(),
                );
            }
        }

        match m.method.as_str() {
            "insert" => Some(Type::Void),
            "keys" | "values" | "entries" => {
                let element = match m.method.as_str() {
                    "keys" => key_ty,
                    "values" => val_ty,
                    _ => Type::Generic("$Tuple2".into(), vec![key_ty, val_ty]),
                };
                Some(Type::Array(Box::new(element)))
            }
            "get" | "remove" => Some(B::Option.apply(vec![val_ty])),
            "contains" => Some(Type::Bool),
            "len" => Some(Type::I64),
            // `.freeze()` -> an immutable `FrozenMap<K,V>` copy, Sync when K,V are
            // Sync, so it can be shared across tasks (willow-dgwo.10).
            "toString" => {
                if !matches!(val_ty, Type::I64 | Type::F64 | Type::Bool | Type::String) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1402,
                            format!(
                                "cannot toString a map with `{}` values",
                                self.diagnostic_type_name(&val_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            m.span,
                            "value types supported by toString are i64, f64, bool, and String",
                        )),
                    );
                }
                Some(Type::String)
            }
            "freeze" => {
                if key_ty == Type::Void || val_ty == Type::Void {
                    return Some(Self::error_type());
                }
                Some(B::FrozenMap.apply(vec![key_ty, val_ty]))
            }
            other => {
                let mut diagnostic = Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{}` on type `Map<{}, {}>`",
                            other,
                            self.diagnostic_type_name(&key_ty),
                            self.diagnostic_type_name(&val_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown map method"))
                    .with_help(
                        "maps support `.insert(k, v)`, `.get(k)`, `.remove(k)`, `.keys()`, `.values()`, `.entries()`, `.contains(k)`, `.len()`, `.freeze()`, `.toString()`",
                    );
                self.label_receiver_call_origin(&mut diagnostic, &m.object, obj_ty);
                self.push(diagnostic);
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
        let (key_ty, val_ty) = builtin_types::binary_args(obj_ty, B::FrozenMap)?;
        let key_ty = key_ty.clone();
        let val_ty = val_ty.clone();
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
                            self.diagnostic_type_name(&key_ty),
                            self.diagnostic_type_name(&k)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong key type")),
                );
            }
        }
        match m.method.as_str() {
            "get" => Some(B::Option.apply(vec![val_ty])),
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
                            self.diagnostic_type_name(&key_ty),
                            self.diagnostic_type_name(&val_ty)
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
