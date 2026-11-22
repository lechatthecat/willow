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
        self.check_array_literal_expecting(elements, None, false)
    }

    /// Type-check an array literal. When `expected_elem` is given (e.g. from a
    /// `let xs: Array<Animal> = [...]` annotation), each element is checked
    /// against it — this allows a heterogeneous literal of classes that all
    /// implement the same interface, and the literal takes the expected type.
    pub(super) fn check_array_literal_expecting(
        &mut self,
        elements: &[Expr],
        expected_elem: Option<&Type>,
        checked: bool,
    ) -> Type {
        // Both callers return through check_expr/check_expr_expecting, which
        // record the authoritative literal type once for downstream lowering.
        #[cfg(test)]
        ARRAY_ELEMENT_CHECKS.with(|count| count.set(count.get() + elements.len()));
        if let Some(expected) = expected_elem {
            for el in elements {
                let ty = self.check_argument_expecting(el, expected, checked);
                if !self.types_compatible(expected, &ty) {
                    self.push(
                        self.with_callable_kind_help(
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
                            expected,
                            &ty,
                        ),
                    );
                }
            }
            return Type::Array(Box::new(expected.clone()));
        }

        if elements.is_empty() {
            return Type::Array(Box::new(Type::Void));
        }
        // An array is mutable storage, so its element type is the one any
        // later store must fit: a function value element takes the type a
        // written `Array<fn(...)>` names, which holds every function value.
        // The shareable kind would make the array invariant at a type no
        // annotation spells, so neither a capturing lambda pushed later nor
        // a `fn(...)` parameter could accept it.
        let mut first_ty = self.check_expr(&elements[0]);
        if let Type::Fn(params, result) = &mut first_ty {
            first_ty = Type::Closure(
                std::mem::take(params),
                std::mem::replace(result, Box::new(Type::Void)),
            );
        }
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
            // Read-only indexing of an immutable `FrozenArray<T>`.
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

    /// Resolve a source method or a private storage operation on an array.
    pub(super) fn check_array_method_call(
        &mut self,
        obj_ty: &Type,
        elem: &Type,
        m: &MethodCallExpr,
        spelling: &str,
        lookup: MethodLookup<String>,
    ) -> Type {
        if matches!(obj_ty, Type::FixedArray(..))
            && matches!(
                lookup,
                MethodLookup::Missing(
                    MethodName::Push | MethodName::Pop | MethodName::Insert | MethodName::Remove
                )
            )
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
            return Self::error_type();
        }
        if !spelling.starts_with("__abi_") {
            let bundled = "$std::collections::Array";
            let template = self
                .generics
                .classes
                .get(bundled)
                .map(|class| (bundled, class))
                .or_else(|| {
                    let local = B::Array.source_name();
                    self.generics
                        .classes
                        .get(local)
                        .filter(|class| class.abi_storage.is_some())
                        .map(|class| (local, class))
                })
                .map(|(access, class)| (access, std::rc::Rc::clone(class)));
            let builtin = match lookup {
                MethodLookup::Missing(name) => Some((name, None)),
                _ => None,
            };
            if template.is_none()
                && let Some(owner) = self.array_storage_method_class(obj_ty).cloned()
            {
                self.record_array_method_owner(m, spelling, &owner);
                return self.resolve_method(&Type::Named(owner), m, spelling, false, builtin);
            }
            if let Some((access, template)) = template {
                let Some(declared) = template
                    .methods
                    .iter()
                    .find(|method| method.name == spelling)
                else {
                    let methods = template
                        .methods
                        .iter()
                        .filter(|method| method.public() && !method.is_static())
                        .map(|method| method.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let mut diagnostic = Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{}` on type `{}`",
                            spelling,
                            self.diagnostic_type_name(obj_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown method"))
                    .with_help(format!("available methods: {methods}"));
                    self.label_receiver_call_origin(&mut diagnostic, &m.object, obj_ty);
                    self.report_missing_method_with_args(
                        diagnostic,
                        &m.args,
                        lookup.is_retired_wait(),
                    );
                    return Self::error_type();
                };
                if *elem == Type::Void {
                    let depends = |ty: &Type| {
                        super::generics::mentions(ty, |name| {
                            template
                                .type_params
                                .iter()
                                .any(|parameter| parameter == name)
                        })
                    };
                    if declared.constraint.is_some()
                        || depends(&declared.return_type)
                        || declared
                            .params
                            .iter()
                            .any(|parameter| depends(&parameter.ty))
                    {
                        if lookup.is_retired_wait() {
                            self.check_discarded_builtin_args(&m.args);
                        }
                        self.generic_error(m.span, "array method requires a known element type");
                        return Self::error_type();
                    }
                    // An empty literal has no element representation yet. Source
                    // signatures independent of that element can still be checked;
                    // executable storage continues to require a concrete type.
                    self.check_call_argument_count(
                        spelling,
                        declared.params.len(),
                        m.args.len(),
                        m.span,
                    );
                    self.check_value_call_args(
                        &declared
                            .params
                            .iter()
                            .map(|parameter| parameter.ty.clone())
                            .collect::<Vec<_>>(),
                        &m.args,
                    );
                    return declared.return_type.clone();
                }
                if let Some(bound) = &declared.constraint
                    && !bound.alternatives.contains(elem)
                {
                    let diagnostic = Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "method `{}` is unavailable for `{}`",
                            spelling,
                            self.diagnostic_type_name(obj_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "method constraint is not satisfied"));
                    self.report_missing_method_with_args(
                        diagnostic,
                        &m.args,
                        lookup.is_retired_wait(),
                    );
                    return Self::error_type();
                }
                if let Some(receiver) =
                    self.instantiate_class(access, std::slice::from_ref(elem), m.span)
                {
                    if let Type::Named(owner) = &receiver
                        && !Self::is_error_name(owner)
                    {
                        self.record_array_method_owner(m, spelling, owner);
                    }
                    return self.resolve_method(&receiver, m, spelling, false, builtin);
                }
                self.generic_error(
                    template.span,
                    "bundled Array declaration could not be instantiated",
                );
            } else {
                self.generic_error(m.span, "bundled Array declaration is unavailable");
            }
            return Self::error_type();
        }
        if !self.array_abi_files.contains(&m.span.file_id) {
            self.generic_error(
                m.span,
                "array ABI operations are private to the bundled declaration",
            );
            return Self::error_type();
        }
        let MethodLookup::Intrinsic(sig) = lookup else {
            self.generic_error(m.span, "unknown array storage operation");
            return Self::error_type();
        };
        let params: &[Type] = if sig.intrinsic == Intrinsic::ArrayResize {
            &[Type::I64]
        } else {
            &[]
        };
        self.check_call_argument_count(spelling, params.len(), m.args.len(), m.span);
        self.check_value_call_args(params, &m.args);
        sig.return_type(|_| None)
    }

    fn record_array_method_owner(&mut self, call: &MethodCallExpr, spelling: &str, owner: &str) {
        let owner = TypeId::from_source_name(owner);
        self.static_call_classes.insert(call.id, owner);
        self.record_lock_effect_call(FunctionId::method(owner, spelling), call.span);
    }

    pub(super) fn check_frozen_array_method_call(
        &mut self,
        elem_ty: &Type,
        m: &MethodCallExpr,
        spelling: &str,
        lookup: MethodLookup<String>,
    ) -> Type {
        if let MethodLookup::Intrinsic(sig) = lookup {
            return sig.return_type(|_| None);
        }
        match lookup {
            MethodLookup::Missing(MethodName::Push | MethodName::Pop | MethodName::Set) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("`FrozenArray` is immutable; `{}` is not allowed", spelling),
                    )
                    .with_label(Label::primary(m.span, "frozen arrays cannot be mutated"))
                    .with_help(
                        "freeze a copy of a mutable `Array<T>`; read it with `[i]` / `.len()`",
                    ),
                );
                Type::Void
            }
            _ => {
                if lookup.is_retired_wait() {
                    self.check_discarded_builtin_args(&m.args);
                }
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{spelling}` on `FrozenArray<{}>`",
                            self.diagnostic_type_name(elem_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown method"))
                    .with_help("`FrozenArray` supports `.len()` and indexing `fa[i]`"),
                );
                Type::Void
            }
        }
    }

    /// Builtin methods on `Map<K, V>`: `insert(k, v)`, `get(k) -> Option<V>`,
    /// `contains(k) -> bool`, `len() -> i64`.
    pub(super) fn check_map_method_call(
        &mut self,
        obj_ty: &Type,
        payloads: (&Type, &Type),
        m: &MethodCallExpr,
        spelling: &str,
        lookup: MethodLookup<String>,
    ) -> Type {
        let (key_ty, val_ty) = payloads;
        let retired_wait = lookup.is_retired_wait();
        let signature = lookup.into_signature();
        let intrinsic = signature.as_ref().map(|s| s.intrinsic);

        if matches!(key_ty, Type::Void)
            && matches!(val_ty, Type::Void)
            && intrinsic == Some(Intrinsic::MapInsert)
            && m.args.len() == 2
            && let Expr::Var(name, _, _) = &m.object
            && let Some(declaration) = self.symbols.lookup_var(name).map(|v| v.declaration_span)
            && self.local.inferred_maps.contains_key(&declaration)
        {
            let key = self.check_expr(&m.args[0].expr);
            let value = self.check_expr(&m.args[1].expr);
            if key != Type::Void && value != Type::Void {
                let ty = B::Map.apply(vec![key, value]);
                let (uses, async_slot) = self.local.inferred_maps.remove(&declaration).unwrap();
                for id in uses {
                    self.local.pending_map_uses.remove(&id);
                    self.expr_types.insert(id, ty.clone());
                }
                if let Some(slot) = async_slot {
                    self.local.async_local_types[slot] = ty.clone();
                }
                self.symbols.lookup_var_mut(name).unwrap().ty = ty;
            }
            return Type::Void;
        }

        if (matches!(key_ty, Type::Void) || matches!(val_ty, Type::Void))
            && matches!(
                intrinsic,
                Some(
                    Intrinsic::MapGet
                        | Intrinsic::MapContains
                        | Intrinsic::MapFreeze
                        | Intrinsic::MapToString
                )
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

        if let Some(sig) = &signature
            && self.check_builtin_arity(
                super::builtin_methods::map_arity(sig),
                B::Map.source_name(),
                spelling,
                m.args.len(),
                m.span,
            )
        {
            if let Some(arg) = m.args.first() {
                let got = self.check_expr_expecting(&arg.expr, key_ty);
                self.check_builtin_argument(
                    BuiltinArgument::MapKey,
                    B::Map.source_name(),
                    spelling,
                    key_ty,
                    &got,
                    arg.expr.span(),
                );
            }
            if let Some(arg) = m.args.get(1) {
                let got = self.check_expr_expecting(&arg.expr, val_ty);
                self.check_builtin_argument(
                    BuiltinArgument::MapValue,
                    B::Map.source_name(),
                    spelling,
                    val_ty,
                    &got,
                    arg.expr.span(),
                );
            }
        }

        match signature {
            Some(sig) if sig.intrinsic == Intrinsic::MapToString => {
                if !matches!(
                    val_ty,
                    Type::I8
                        | Type::I16
                        | Type::I32
                        | Type::U16
                        | Type::U32
                        | Type::U64
                        | Type::Char
                        | Type::U8
                        | Type::I64
                        | Type::F64
                        | Type::Bool
                        | Type::String
                ) {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E1402,
                            format!(
                                "cannot to_string a map with `{}` values",
                                self.diagnostic_type_name(val_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            m.span,
                            "value types supported by to_string are i64, f64, bool, and String",
                        )),
                    );
                }
                Type::String
            }
            Some(sig)
                if sig.intrinsic == Intrinsic::MapFreeze
                    && (matches!(key_ty, Type::Void) || matches!(val_ty, Type::Void)) =>
            {
                Self::error_type()
            }
            Some(sig) => sig.return_type(|_| None),
            None => {
                if retired_wait {
                    self.check_discarded_builtin_args(&m.args);
                }
                let other = spelling;
                let mut diagnostic = Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{}` on type `Map<{}, {}>`",
                            other,
                            self.diagnostic_type_name(key_ty),
                            self.diagnostic_type_name(val_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown map method"))
                    .with_help(
                        "maps support `.insert(k, v)`, `.get(k)`, `.remove(k)`, `.keys()`, `.values()`, `.entries()`, `.contains(k)`, `.len()`, `.freeze()`, `.to_string()`",
                    );
                self.label_receiver_call_origin(&mut diagnostic, &m.object, obj_ty);
                self.push(diagnostic);
                Type::Void
            }
        }
    }

    /// Builtin methods on the immutable `FrozenMap<K, V>`:
    /// read-only `.get(k) -> Option<V>`, `.contains(k) -> bool`, `.len() -> i64`;
    /// `insert`/`remove` are rejected.
    pub(super) fn check_frozen_map_method_call(
        &mut self,
        payloads: (&Type, &Type),
        m: &MethodCallExpr,
        spelling: &str,
        lookup: MethodLookup<String>,
    ) -> Type {
        let (key_ty, val_ty) = payloads;
        if lookup.is_retired_wait() {
            self.check_discarded_builtin_args(&m.args);
        }
        if let Some(arg) = m.args.first() {
            let k = self.check_argument_expecting(&arg.expr, key_ty, lookup.is_retired_wait());
            if matches!(
                &lookup,
                MethodLookup::Intrinsic(ResolvedMethod {
                    intrinsic: Intrinsic::FrozenMapGet | Intrinsic::FrozenMapContains,
                    ..
                })
            ) && !matches!(key_ty, Type::Void)
                && !self.types_compatible(key_ty, &k)
            {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "map key type mismatch: expected `{}`, found `{}`",
                            self.diagnostic_type_name(key_ty),
                            self.diagnostic_type_name(&k)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong key type")),
                );
            }
        }
        if let MethodLookup::Intrinsic(sig) = lookup {
            return sig.return_type(|_| None);
        }
        match lookup {
            MethodLookup::Missing(MethodName::Insert | MethodName::Remove) => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("`FrozenMap` is immutable; `{}` is not allowed", spelling),
                    )
                    .with_label(Label::primary(m.span, "frozen maps cannot be mutated"))
                    .with_help("freeze a copy of a mutable `Map<K, V>`; read it with `.get`/`.contains`/`.len`"),
                );
                Type::Void
            }
            _ => {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "no method `{spelling}` on `FrozenMap<{}, {}>`",
                            self.diagnostic_type_name(key_ty),
                            self.diagnostic_type_name(val_ty)
                        ),
                    )
                    .with_label(Label::primary(m.span, "unknown method"))
                    .with_help("`FrozenMap` supports `.get(k)`, `.contains(k)`, `.len()`"),
                );
                Type::Void
            }
        }
    }
}
