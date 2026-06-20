use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};
use crate::parser::ast::*;
use crate::semantic::symbols::*;

use super::*;

impl TypeChecker {
    pub(super) fn check_select(&mut self, s: &SelectExpr) {
        let mut default_count = 0;
        for case in &s.cases {
            self.symbols.push_scope();
            match &case.kind {
                SelectCaseKind::Recv { binding, channel } => {
                    let ch_ty = self.check_expr(channel);
                    let elem = self.select_channel_elem(&ch_ty, channel.span());
                    if binding != "_" {
                        // Record the binding type keyed by the case span so the
                        // cooperative async lowering can frame-back it (willow-7aj),
                        // mirroring how `let`/`for` locals are recorded.
                        self.async_local_types.insert(case.span, elem.clone());
                        self.symbols.define_var(
                            binding.clone(),
                            VarInfo {
                                ty: elem,
                                mutable: false,
                                is_param: false,
                                declaration_span: case.span,
                            },
                        );
                    }
                }
                SelectCaseKind::Send { channel, value } => {
                    let ch_ty = self.check_expr(channel);
                    let elem = self.select_channel_elem(&ch_ty, channel.span());
                    let v_ty = self.check_expr(value);
                    if elem != Type::Void && v_ty != elem {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "select send value type `{}` does not match channel element `{}`",
                                    type_name(&v_ty),
                                    type_name(&elem)
                                ),
                            )
                            .with_label(Label::primary(value.span(), "wrong value type")),
                        );
                    }
                }
                SelectCaseKind::Default => default_count += 1,
            }
            self.check_block(&case.body);
            self.symbols.pop_scope();
        }
        if default_count > 1 {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0807,
                    "select may have at most one `default` case",
                )
                .with_label(Label::primary(s.span, "multiple `default` cases")),
            );
        }
    }

    /// Type-check method calls on `Option<T>` and `Result<T,E>`.
    /// Returns `Some(return_type)` if the call was handled, `None` to fall through.
    pub(super) fn check_option_result_method_call(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
    ) -> Option<Type> {
        match obj_ty {
            Type::Generic(name, args) if name == "Option" => {
                let inner = args.first().cloned().unwrap_or(Type::Void);
                match call.method.as_str() {
                    "is_some" | "is_none" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!("`Option::{}` takes no arguments", call.method),
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(Type::Bool)
                    }
                    "unwrap" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    "`Option::unwrap` takes no arguments",
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(inner)
                    }
                    "expect" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::expect` expects 1 argument (message), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let msg_ty = self.check_expr(&call.args[0].expr);
                            if msg_ty != Type::String {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "expect message must be `String`, found `{}`",
                                            type_name(&msg_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            "expected `String`",
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(inner)
                    }
                    "unwrap_or" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::unwrap_or` expects 1 argument, got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let default_ty = self.check_expr(&call.args[0].expr);
                            if !self.types_compatible(&inner, &default_ty) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "mismatched types: expected `{}`, found `{}`",
                                            type_name(&inner),
                                            type_name(&default_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            format!("expected `{}`", type_name(&inner)),
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(inner)
                    }
                    "map" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::map` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_fn_arg_with_param_context(
                                &call.args[0].expr,
                                std::slice::from_ref(&inner),
                            );
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&inner, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Option::map` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&inner), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(Type::Generic("Option".to_string(), vec![*ret.clone()]))
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Option::map` expects a function `fn({}) -> U`, found `{}`",
                                            type_name(&inner), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "and_then" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::and_then` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_fn_arg_with_param_context(
                                &call.args[0].expr,
                                std::slice::from_ref(&inner),
                            );
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&inner, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Option::and_then` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&inner), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Option::and_then` expects a function `fn({}) -> Option<U>`, found `{}`",
                                            type_name(&inner), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "or_else" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Option::or_else` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty =
                                self.check_fn_arg_with_param_context(&call.args[0].expr, &[]);
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.is_empty() => {
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Option::or_else` expects a function `fn() -> Option<{}>`, found `{}`",
                                            type_name(&inner), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    _ => None,
                }
            }
            Type::Generic(name, args) if name == "Result" => {
                let ok_ty = args.first().cloned().unwrap_or(Type::Void);
                let err_ty = args.get(1).cloned().unwrap_or(Type::Void);
                match call.method.as_str() {
                    "is_ok" | "is_err" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!("`Result::{}` takes no arguments", call.method),
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(Type::Bool)
                    }
                    "unwrap" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    "`Result::unwrap` takes no arguments",
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(ok_ty)
                    }
                    "expect" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::expect` expects 1 argument (message), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let msg_ty = self.check_expr(&call.args[0].expr);
                            if msg_ty != Type::String {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "expect message must be `String`, found `{}`",
                                            type_name(&msg_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            "expected `String`",
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(ok_ty)
                    }
                    "unwrap_or" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::unwrap_or` expects 1 argument, got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                        } else {
                            let default_ty = self.check_expr(&call.args[0].expr);
                            if !self.types_compatible(&ok_ty, &default_ty) {
                                self.push(
                                    Diagnostic::new(
                                        Severity::Error,
                                        ErrorCode::E0201,
                                        format!(
                                            "mismatched types: expected `{}`, found `{}`",
                                            type_name(&ok_ty),
                                            type_name(&default_ty)
                                        ),
                                    )
                                    .with_label(
                                        Label::primary(
                                            call.args[0].expr.span(),
                                            format!("expected `{}`", type_name(&ok_ty)),
                                        ),
                                    ),
                                );
                            }
                        }
                        Some(ok_ty)
                    }
                    "unwrap_err" => {
                        if !call.args.is_empty() {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    "`Result::unwrap_err` takes no arguments",
                                )
                                .with_label(Label::primary(call.span, "unexpected arguments")),
                            );
                        }
                        Some(err_ty)
                    }
                    "map" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::map` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_fn_arg_with_param_context(
                                &call.args[0].expr,
                                std::slice::from_ref(&ok_ty),
                            );
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&ok_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::map` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&ok_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(Type::Generic(
                                        "Result".to_string(),
                                        vec![*ret.clone(), err_ty],
                                    ))
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::map` expects a function `fn({}) -> U`, found `{}`",
                                            type_name(&ok_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "map_err" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::map_err` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_fn_arg_with_param_context(
                                &call.args[0].expr,
                                std::slice::from_ref(&err_ty),
                            );
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&err_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::map_err` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&err_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(Type::Generic(
                                        "Result".to_string(),
                                        vec![ok_ty, *ret.clone()],
                                    ))
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::map_err` expects a function `fn({}) -> F`, found `{}`",
                                            type_name(&err_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "and_then" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::and_then` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_fn_arg_with_param_context(
                                &call.args[0].expr,
                                std::slice::from_ref(&ok_ty),
                            );
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&ok_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::and_then` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&ok_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::and_then` expects a function `fn({}) -> Result<U, E>`, found `{}`",
                                            type_name(&ok_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    "or_else" => {
                        if call.args.len() != 1 {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0201,
                                    format!(
                                        "`Result::or_else` expects 1 argument (fn), got {}",
                                        call.args.len()
                                    ),
                                )
                                .with_label(Label::primary(call.span, "wrong number of arguments")),
                            );
                            Some(obj_ty.clone())
                        } else {
                            let f_ty = self.check_fn_arg_with_param_context(
                                &call.args[0].expr,
                                std::slice::from_ref(&err_ty),
                            );
                            match f_ty {
                                Type::Fn(ref params, ref ret) if params.len() == 1 => {
                                    if !self.types_compatible(&err_ty, &params[0]) {
                                        self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                            format!("`Result::or_else` closure argument type mismatch: expected `{}`, found `{}`",
                                                type_name(&err_ty), type_name(&params[0])))
                                            .with_label(Label::primary(call.args[0].expr.span(), "type mismatch")));
                                    }
                                    Some(*ret.clone())
                                }
                                _ => {
                                    self.push(Diagnostic::new(Severity::Error, ErrorCode::E0201,
                                        format!("`Result::or_else` expects a function `fn({}) -> Result<T, F>`, found `{}`",
                                            type_name(&err_ty), type_name(&f_ty)))
                                        .with_label(Label::primary(call.args[0].expr.span(), "expected function")));
                                    Some(obj_ty.clone())
                                }
                            }
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Type-check a method on `Mutex<T>` (`get`/`set`) or `RwLock<T>`
    /// (`read`/`write`) (willow-dgwo.3).
    pub(super) fn check_lock_method_call(
        &mut self,
        lock: &str,
        elem: &Type,
        call: &MethodCallExpr,
    ) -> Type {
        // (expected arg type, return type) per method.
        let sig: Option<(Option<Type>, Type)> = match (lock, call.method.as_str()) {
            ("Mutex", "get") | ("RwLock", "read") => Some((None, elem.clone())),
            ("Mutex", "set") | ("RwLock", "write") => Some((Some(elem.clone()), Type::Void)),
            _ => None,
        };
        let Some((arg_ty, ret)) = sig else {
            for arg in &call.args {
                self.check_expr(&arg.expr);
            }
            let methods = if lock == "Mutex" {
                "get/set"
            } else {
                "read/write"
            };
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0806,
                    format!("`{lock}<T>` has no method `{}`", call.method),
                )
                .with_label(Label::primary(call.span, "unknown lock method"))
                .with_help(format!("`{lock}` supports {methods}")),
            );
            return Type::Void;
        };
        let expected_argc = usize::from(arg_ty.is_some());
        if call.args.len() != expected_argc {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "`{lock}::{}` expects {expected_argc} argument(s), got {}",
                        call.method,
                        call.args.len()
                    ),
                )
                .with_label(Label::primary(call.span, "wrong number of arguments")),
            );
        }
        if let (Some(expected), Some(arg)) = (arg_ty, call.args.first()) {
            let got = self.check_expr(&arg.expr);
            if got != expected && got != Type::Never {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "`{lock}::{}` expects `{}`, got `{}`",
                            call.method,
                            type_name(&expected),
                            type_name(&got)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong argument type")),
                );
            }
        }
        ret
    }

    /// Type-check a method on `AtomicI64` / `AtomicBool` (willow-dgwo.3).
    /// `load() -> T`, `store(T)`, `add(T)/sub(T) -> T` (i64 only), `swap(T) -> T`.
    pub(super) fn check_atomic_method_call(&mut self, atomic: &str, call: &MethodCallExpr) -> Type {
        let elem = if atomic == "AtomicI64" {
            Type::I64
        } else {
            Type::Bool
        };
        // (arg count, expected arg type, return type)
        let sig: Option<(usize, Option<Type>, Type)> = match call.method.as_str() {
            "load" => Some((0, None, elem.clone())),
            "store" => Some((1, Some(elem.clone()), Type::Void)),
            "swap" => Some((1, Some(elem.clone()), elem.clone())),
            "add" | "sub" if atomic == "AtomicI64" => Some((1, Some(Type::I64), Type::I64)),
            _ => None,
        };
        let Some((argc, arg_ty, ret)) = sig else {
            for arg in &call.args {
                self.check_expr(&arg.expr);
            }
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0806,
                    format!("`{atomic}` has no method `{}`", call.method),
                )
                .with_label(Label::primary(call.span, "unknown atomic method"))
                .with_help("`AtomicI64`: load/store/add/sub/swap; `AtomicBool`: load/store/swap"),
            );
            return Type::Void;
        };
        if call.args.len() != argc {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0201,
                    format!(
                        "`{atomic}::{}` expects {argc} argument(s), got {}",
                        call.method,
                        call.args.len()
                    ),
                )
                .with_label(Label::primary(call.span, "wrong number of arguments")),
            );
        }
        if let (Some(expected), Some(arg)) = (arg_ty, call.args.first()) {
            let got = self.check_expr(&arg.expr);
            if got != expected && got != Type::Never {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!(
                            "`{atomic}::{}` expects `{}`, got `{}`",
                            call.method,
                            type_name(&expected),
                            type_name(&got)
                        ),
                    )
                    .with_label(Label::primary(arg.expr.span(), "wrong argument type")),
                );
            }
        }
        ret
    }

    pub(super) fn check_concurrency_method_call(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
    ) -> Option<Type> {
        // Atomic primitives (willow-dgwo.3).
        if let Type::Named(n) = obj_ty {
            if n == "AtomicI64" || n == "AtomicBool" {
                return Some(self.check_atomic_method_call(n, call));
            }
        }
        // Lock primitives (willow-dgwo.3): Mutex<T>.get/set, RwLock<T>.read/write.
        if let Type::Generic(n, args) = obj_ty {
            if (n == "Mutex" || n == "RwLock") && args.len() == 1 {
                return Some(self.check_lock_method_call(n, &args[0].clone(), call));
            }
        }
        match call.method.as_str() {
            "join" => {
                if !call.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("join expects 0 arguments, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                match obj_ty {
                    // Both `JoinHandle<T>` (spawn) and `Task<T>` (an async fn
                    // call result) are joinable: an async call schedules an
                    // eager task, so `.join()` waits for it (willow-h2vf).
                    Type::Generic(name, args)
                        if (name == "JoinHandle" || name == "Task") && args.len() == 1 =>
                    {
                        Some(args[0].clone())
                    }
                    _ => {
                        for arg in &call.args {
                            self.check_expr(&arg.expr);
                        }
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0805,
                                format!("cannot call `join` on `{}`", type_name(obj_ty)),
                            )
                            .with_label(Label::primary(call.span, "expected a task")),
                        );
                        Some(Type::Void)
                    }
                }
            }
            "send" => {
                let channel_type = channel_element_type(obj_ty);
                if channel_type.is_none() {
                    for arg in &call.args {
                        self.check_expr(&arg.expr);
                    }
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0806,
                            format!("cannot call `send` on `{}`", type_name(obj_ty)),
                        )
                        .with_label(Label::primary(call.span, "expected `Channel<T>`")),
                    );
                    return Some(Type::Void);
                }
                let element_ty = channel_type.unwrap();
                // A sent value crosses task/worker boundaries, so the channel
                // item type must be Send (willow-dgwo.6, spec §10; gated like the
                // other data-race checks).
                if self.enforce_send_sync && element_ty != Type::Void && !self.is_send(&element_ty)
                {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E2403,
                            format!(
                                "channel item type `{}` must be `Send`",
                                type_name(&element_ty)
                            ),
                        )
                        .with_label(Label::primary(
                            call.span,
                            "this value crosses a task boundary",
                        ))
                        .with_help("send Send values (scalars, String, frozen/Sync types) through channels"),
                    );
                }
                if call.args.len() != 1 {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("send expects 1 argument, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                if let Some(arg) = call.args.first() {
                    let arg_ty = self.check_expr(&arg.expr);
                    if matches!(arg.mode, CallArgMode::Reference { .. }) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E1703,
                                "unexpected reference argument",
                            )
                            .with_label(Label::primary(
                                arg.span,
                                format!(
                                    "send expects `{}`, not `& {}`",
                                    type_name(&element_ty),
                                    type_name(&arg_ty)
                                ),
                            )),
                        );
                    } else if !self.types_compatible(&element_ty, &arg_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0802,
                                format!(
                                    "cannot send `{}` into `Channel<{}>`",
                                    type_name(&arg_ty),
                                    type_name(&element_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                arg.expr.span(),
                                format!(
                                    "expected `{}`, found `{}`",
                                    type_name(&element_ty),
                                    type_name(&arg_ty)
                                ),
                            )),
                        );
                    }
                }
                Some(Type::Void)
            }
            "recv" => {
                if !call.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("recv expects 0 arguments, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                match channel_element_type(obj_ty) {
                    Some(element_ty) => Some(element_ty),
                    None => {
                        for arg in &call.args {
                            self.check_expr(&arg.expr);
                        }
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0806,
                                format!("cannot call `recv` on `{}`", type_name(obj_ty)),
                            )
                            .with_label(Label::primary(call.span, "expected `Channel<T>`")),
                        );
                        Some(Type::Void)
                    }
                }
            }
            "close" => {
                if !call.args.is_empty() {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!("close expects 0 arguments, got {}", call.args.len()),
                        )
                        .with_label(Label::primary(call.span, "wrong number of arguments")),
                    );
                }
                if channel_element_type(obj_ty).is_none() {
                    for arg in &call.args {
                        self.check_expr(&arg.expr);
                    }
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0806,
                            format!("cannot call `close` on `{}`", type_name(obj_ty)),
                        )
                        .with_label(Label::primary(call.span, "expected `Channel<T>`")),
                    );
                }
                Some(Type::Void)
            }
            _ => None,
        }
    }
}
