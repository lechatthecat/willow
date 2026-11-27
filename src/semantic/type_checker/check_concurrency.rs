use crate::diagnostics::{Diagnostic, ErrorCode, Label, Severity};
use crate::parser::ast::*;
use crate::semantic::builtin_types::{self, BuiltinTypeId as B};
use crate::semantic::symbols::*;

use super::*;

#[willow_continuations::checker]
impl TypeChecker {
    pub(super) fn check_select(&mut self, s: &SelectExpr) {
        let mut default_count = 0;
        for case in &s.cases {
            self.symbols.push_scope();
            match &case.kind {
                SelectCaseKind::Recv {
                    binding,
                    channel,
                    closed_aware,
                } => {
                    let ch_ty = self.check_expr(channel);
                    let elem = self.select_channel_elem(&ch_ty, channel.span());
                    if binding != "_" {
                        let elem = elem.into_owned();
                        // Closed channels bind None in a close-aware receive.
                        let elem = if *closed_aware && elem != Type::Void {
                            B::Option.apply(vec![elem])
                        } else {
                            elem
                        };
                        // Include the received binding in the task's Send check,
                        // independently of other bindings at the same source span.
                        if self.local.current_async_context {
                            self.record_async_local(
                                elem.clone(),
                                binding,
                                case.span,
                                super::async_retention::Key::binding(binding),
                            );
                        }
                        self.define_var(
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
                    // Match ordinary send: the channel supplies the expected
                    // type for shorthand variants such as Some/None.
                    let v_ty = self.check_expr_expecting(value, &elem);
                    if *elem != Type::Void
                        && v_ty != *elem
                        && !self.recovery_types_compatible(&elem, &v_ty)
                    {
                        let diagnostic = Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0201,
                            format!(
                                "select send value type `{}` does not match channel element `{}`",
                                self.diagnostic_type_name(&v_ty),
                                self.diagnostic_type_name(&elem)
                            ),
                        )
                        .with_label(Label::primary(value.span(), "wrong value type"));
                        self.push(self.with_callable_kind_help(diagnostic, &elem, &v_ty));
                    }
                    // A bounded send can suspend while holding the value,
                    // so its type participates in the task's Send check.
                    if self.local.current_async_context && *elem != Type::Void {
                        self.record_async_local(
                            elem.into_owned(),
                            "select send value",
                            value.span(),
                            super::async_retention::Key::Value(value.id()),
                        );
                    }
                }
                SelectCaseKind::Timeout { millis } => {
                    let ms_ty = self.check_expr(millis);
                    if ms_ty != Type::I64 && !Self::contains_error_type(&ms_ty) {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "select `sleep` timeout must be `i64` milliseconds, found `{}`",
                                    self.diagnostic_type_name(&ms_ty)
                                ),
                            )
                            .with_label(Label::primary(millis.span(), "expected `i64`")),
                        );
                    }
                }
                SelectCaseKind::Join { binding, task } => {
                    // A task case IS an `await`, so it obeys the same rule as
                    // one written outside a select: waiting on a task is only
                    // possible in an async fn. Channel and
                    // `sleep` cases stay legal in a sync select.
                    if !self.local.current_async_context {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0801,
                                "`await` can only be used inside an async function",
                            )
                            .with_label(Label::primary(
                                case.span,
                                "`await` used in a non-async function",
                            ))
                            .with_help("make the enclosing function `async`"),
                        );
                    }
                    let task_ty = self.check_expr(task);
                    // Cancellation-awareness follows the awaited TYPE, not the
                    // syntax. An inline `t.result()`, a held `TaskResult<T>`,
                    // and a user-defined `result()` returning `Task<T>` are
                    // therefore classified consistently.
                    let (output_ty, cancel_aware) = match awaitable_task_type(&task_ty) {
                        Some(shape) => shape,
                        None if Self::contains_error_type(&task_ty) => (Self::error_type(), false),
                        None => {
                            self.push(
                                Diagnostic::new(
                                    Severity::Error,
                                    ErrorCode::E0805,
                                    format!(
                                        "cannot await `{}` in a select case",
                                        self.diagnostic_type_name(&task_ty)
                                    ),
                                )
                                .with_label(Label::primary(task.span(), "expected a task")),
                            );
                            (Type::I64, false)
                        }
                    };
                    let result_ty = if cancel_aware {
                        B::Result.apply(vec![output_ty, Type::Named("Cancelled".to_string())])
                    } else {
                        output_ty
                    };
                    if binding != "_" {
                        // Include the join binding just like a received binding.
                        if self.local.current_async_context {
                            self.record_async_local(
                                result_ty.clone(),
                                binding,
                                case.span,
                                super::async_retention::Key::binding(binding),
                            );
                        }
                        self.define_var(
                            binding.clone(),
                            VarInfo {
                                ty: result_ty,
                                mutable: false,
                                is_param: false,
                                declaration_span: case.span,
                            },
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
    /// Callback parameter and return types depend on the receiver's payloads.
    pub(super) fn check_option_result_method_call(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
        spelling: &str,
        method: AdtMethod,
    ) -> Type {
        use super::builtin_methods::{OptionAction, TransformResult, adt_arity, option_action};
        let builtin = builtin_types::resolve(obj_ty).expect("checked Option or Result");
        let arity = adt_arity(method);
        let action = option_action(builtin.id, method);
        let owner = builtin.id.source_name();
        static VOID: Type = Type::Void;
        let inner = builtin.args.first().unwrap_or(&VOID);
        let error = builtin.args.get(1).unwrap_or(&VOID);
        if call.args.len() != arity.count {
            self.check_discarded_builtin_args(&call.args);
            self.check_builtin_arity(arity, owner, spelling, call.args.len(), call.span);
            return match action {
                OptionAction::Bool => Type::Bool,
                OptionAction::ErrorPayload => error.clone(),
                OptionAction::Transform { .. } => obj_ty.clone(),
                _ => inner.clone(),
            };
        }
        match action {
            OptionAction::Bool => Type::Bool,
            OptionAction::Payload => inner.clone(),
            OptionAction::ErrorPayload => error.clone(),
            OptionAction::Message | OptionAction::Default => {
                let got = self.check_expr(&call.args[0].expr);
                let rule = if matches!(action, OptionAction::Message) {
                    BuiltinArgument::Message
                } else {
                    BuiltinArgument::Default
                };
                self.check_builtin_argument(
                    rule,
                    owner,
                    spelling,
                    inner,
                    &got,
                    call.args[0].expr.span(),
                );
                inner.clone()
            }
            OptionAction::Transform {
                error_input,
                no_input,
                result,
            } => {
                let input = if error_input { error } else { inner };
                let params = if no_input {
                    &[][..]
                } else {
                    std::slice::from_ref(input)
                };
                let mut f_ty =
                    self.check_fn_arg_with_param_context(&call.args[0].expr, params, false);
                match f_ty {
                    Type::Fn(ref parameters, ref mut ret)
                    | Type::Closure(ref parameters, ref mut ret)
                        if parameters.len() == params.len() =>
                    {
                        if !no_input {
                            self.check_builtin_argument(
                                BuiltinArgument::Closure,
                                owner,
                                spelling,
                                input,
                                &parameters[0],
                                call.args[0].expr.span(),
                            );
                        }
                        let output = std::mem::replace(ret.as_mut(), Type::Void);
                        match result {
                            TransformResult::Option => B::Option.apply(vec![output]),
                            TransformResult::ResultOk => {
                                B::Result.apply(vec![output, error.clone()])
                            }
                            TransformResult::ResultErr => {
                                B::Result.apply(vec![inner.clone(), output])
                            }
                            TransformResult::Direct => output,
                        }
                    }
                    _ if !matches!(f_ty, Type::Fn(..) | Type::Closure(..))
                        && Self::contains_error_type(&f_ty) =>
                    {
                        Self::error_type()
                    }
                    _ => {
                        let signature = match (builtin.id, method) {
                            (_, AdtMethod::Map) => {
                                format!("fn({}) -> U", self.diagnostic_type_name(input))
                            }
                            (B::Result, AdtMethod::MapErr) => {
                                format!("fn({}) -> F", self.diagnostic_type_name(input))
                            }
                            (B::Option, AdtMethod::AndThen) => {
                                format!("fn({}) -> Option<U>", self.diagnostic_type_name(input))
                            }
                            (B::Option, AdtMethod::OrElse) => {
                                format!("fn() -> Option<{}>", self.diagnostic_type_name(input))
                            }
                            (B::Result, AdtMethod::AndThen) => {
                                format!("fn({}) -> Result<U, E>", self.diagnostic_type_name(input))
                            }
                            (B::Result, AdtMethod::OrElse) => {
                                format!("fn({}) -> Result<T, F>", self.diagnostic_type_name(input))
                            }
                            _ => unreachable!(),
                        };
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E0201,
                                format!(
                                    "`{owner}::{}` expects a function `{signature}`, found `{}`",
                                    spelling,
                                    self.diagnostic_type_name(&f_ty)
                                ),
                            )
                            .with_label(Label::primary(
                                call.args[0].expr.span(),
                                "expected function",
                            )),
                        );
                        obj_ty.clone()
                    }
                }
            }
        }
    }

    /// Type-check a method on the explicitly native-blocking compatibility
    /// cells. Scheduler-aware `Mutex<T>` / `RwLock<T>` have no accessors.
    ///
    /// `Mutex<T>` has NO accessor methods. A `get`/`set` pair
    /// around a read-modify-write drops concurrent updates, which is exactly the
    /// bug a mutex exists to prevent, so exclusive access is only reachable
    /// through `lock <mutex> as [mut] value { .. }`.
    pub(super) fn check_lock_method_call(
        &mut self,
        lock: &str,
        elem: &Type,
        call: &MethodCallExpr,
        spelling: &str,
        signature: Option<ResolvedMethod>,
    ) -> Type {
        let Some(sig) = signature else {
            self.check_discarded_builtin_args(&call.args);
            let mut diagnostic = Diagnostic::new(
                Severity::Error,
                ErrorCode::E0806,
                format!("`{lock}<T>` has no method `{}`", spelling),
            )
            .with_label(Label::primary(call.span, "unknown lock method"));
            diagnostic = if matches!(B::from_name(lock), Some(B::Mutex | B::RwLock)) {
                // The removed `get`/`set` are the likely reach, so name the
                // replacement rather than an empty method list.
                diagnostic.with_help(
                    if B::from_name(lock) == Some(B::Mutex) {
                        "`Mutex<T>` has no accessor methods: a get/set read-modify-write can lose updates. Use `lock <mutex> as [mut] value { .. }` inside an `async fn`, or `BlockingCell<T>` for native-blocking compatibility"
                    } else {
                        "`RwLock<T>` has no accessor methods: separate read/write calls can lose a read-modify-write update. Use `lock read <rwlock> as value { .. }` or `lock write <rwlock> as [mut] value { .. }` inside an `async fn`, or `BlockingRwCell<T>` for native-blocking compatibility"
                    },
                )
            } else {
                let methods = if B::from_name(lock) == Some(B::BlockingCell) {
                    "get/set"
                } else {
                    "read/write"
                };
                diagnostic.with_help(format!("`{lock}` supports {methods}"))
            };
            self.push(diagnostic);
            return Type::Void;
        };
        let expected_argc = sig.arity;
        self.check_builtin_arity(
            BuiltinArity::new(expected_argc, ArityText::ArgumentS),
            lock,
            spelling,
            call.args.len(),
            call.span,
        );
        if sig.arity == 1
            && let Some(arg) = call.args.first()
        {
            let got = self.check_expr(&arg.expr);
            self.check_builtin_argument(
                BuiltinArgument::Cell,
                lock,
                spelling,
                elem,
                &got,
                arg.expr.span(),
            );
        }
        sig.return_type(|_| None)
    }

    /// Type-check a method on `AtomicI64` / `AtomicBool`.
    /// `compare_exchange(T, T) -> bool`, `load() -> T`, `store(T)`, `add(T)/sub(T) -> T` (i64 only), `swap(T) -> T`.
    pub(super) fn check_atomic_method_call(
        &mut self,
        atomic: &str,
        call: &MethodCallExpr,
        spelling: &str,
        signature: Option<ResolvedMethod>,
    ) -> Type {
        let elem = if B::from_name(atomic) == Some(B::AtomicI64) {
            Type::I64
        } else {
            Type::Bool
        };
        let Some(sig) = signature else {
            self.check_discarded_builtin_args(&call.args);
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E0806,
                    format!("`{atomic}` has no method `{}`", spelling),
                )
                .with_label(Label::primary(call.span, "unknown atomic method"))
                .with_help("`AtomicI64`: load/store/add/sub/swap/compare_exchange; `AtomicBool`: load/store/swap/compare_exchange"),
            );
            return Type::Void;
        };
        self.check_builtin_arity(
            BuiltinArity::new(sig.arity, ArityText::ArgumentS),
            atomic,
            spelling,
            call.args.len(),
            call.span,
        );
        for arg in &call.args {
            if sig.arity == 0 {
                self.check_expr(&arg.expr);
                continue;
            };
            let got = self.check_expr(&arg.expr);
            self.check_builtin_argument(
                BuiltinArgument::Cell,
                atomic,
                spelling,
                &elem,
                &got,
                arg.expr.span(),
            );
        }
        sig.return_type(|_| None)
    }

    pub(super) fn check_task_control_method_call(
        &mut self,
        kind: &str,
        call: &MethodCallExpr,
        spelling: &str,
        lookup: MethodLookup<String>,
    ) -> Type {
        for arg in &call.args {
            if matches!(arg.mode, CallArgMode::Reference { .. }) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E1703,
                        "unexpected reference argument",
                    )
                    .with_label(Label::primary(
                        arg.span,
                        format!("`{kind}::{}` takes Task values by value", spelling),
                    )),
                );
            }
        }
        match lookup {
            MethodLookup::Intrinsic(sig)
                if matches!(sig.intrinsic, Intrinsic::TokenAttach | Intrinsic::ScopeAdd) =>
            {
                if call.args.len() != sig.arity {
                    self.check_discarded_builtin_args(&call.args);
                    self.check_builtin_arity(
                        BuiltinArity::new(sig.arity, ArityText::Method("")),
                        kind,
                        spelling,
                        call.args.len(),
                        call.span,
                    );
                    Type::Void
                } else {
                    let task_ty = self.check_expr(&call.args[0].expr);
                    if !is_task_handle_type(&task_ty) && Self::contains_error_type(&task_ty) {
                        Self::error_type()
                    } else if !self.check_builtin_argument(
                        BuiltinArgument::Task,
                        kind,
                        spelling,
                        &Type::Void,
                        &task_ty,
                        call.args[0].expr.span(),
                    ) {
                        Type::Void
                    } else {
                        task_ty
                    }
                }
            }
            MethodLookup::Intrinsic(sig) => {
                if call.args.len() != sig.arity {
                    self.check_discarded_builtin_args(&call.args);
                    self.check_builtin_arity(
                        BuiltinArity::new(sig.arity, ArityText::QualifiedCount),
                        kind,
                        spelling,
                        call.args.len(),
                        call.span,
                    );
                }
                sig.return_type(|_| None)
            }
            MethodLookup::Missing(MethodName::Finish) => {
                if !call.args.is_empty() {
                    self.check_discarded_builtin_args(&call.args);
                    self.check_builtin_arity(
                        BuiltinArity::new(0, ArityText::QualifiedCount),
                        kind,
                        spelling,
                        call.args.len(),
                        call.span,
                    );
                }
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        "`CancellationToken` has no `finish` method",
                    )
                    .with_label(Label::primary(call.span, "unknown token method")),
                );
                Type::Void
            }
            MethodLookup::Missing(MethodName::Attach | MethodName::Add) => {
                self.check_discarded_builtin_args(&call.args);
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("`{kind}` has no `{}` method", spelling),
                    )
                    .with_label(Label::primary(call.span, "unknown method")),
                );
                Type::Void
            }
            _ => {
                self.check_discarded_builtin_args(&call.args);
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        format!("unknown `{kind}` method `{}`", spelling),
                    )
                    .with_label(Label::primary(call.span, "unknown method")),
                );
                Type::Void
            }
        }
    }
    pub(super) fn check_retired_task_wait(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
        spelling: &str,
        name: MethodName,
    ) -> Type {
        self.check_discarded_builtin_args(&call.args);
        let is_try_join = name == MethodName::TryJoin;
        let code = if is_try_join {
            ErrorCode::E0813
        } else {
            ErrorCode::E0812
        };
        self.push(
            Diagnostic::new(
                Severity::Error,
                code,
                format!(
                    "`{}::{}` has been removed",
                    self.diagnostic_type_name(obj_ty),
                    if is_try_join { "try_join()" } else { "join()" }
                ),
            )
            .with_label(Label::primary(
                call.span,
                format!("`{}` no longer exists", spelling),
            ))
            .with_help("use `await task`")
            .with_help("make the containing function `async` if necessary")
            .with_help("to handle cancellation as a value, use `await task.result()`"),
        );
        let Type::Generic(_, args) = obj_ty else {
            unreachable!()
        };
        if is_try_join {
            Type::Void
        } else {
            args[0].clone()
        }
    }

    pub(super) fn check_invalid_channel_receiver(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
        spelling: &str,
        attempted: Intrinsic,
    ) -> Type {
        if attempted != Intrinsic::ChannelSend {
            self.check_builtin_arity(
                BuiltinArity::new(0, ArityText::Plain),
                "Channel",
                spelling,
                call.args.len(),
                call.span,
            );
        }
        self.check_discarded_builtin_args(&call.args);
        self.push(
            Diagnostic::new(
                Severity::Error,
                ErrorCode::E0806,
                format!(
                    "cannot call `{}` on `{}`",
                    spelling,
                    self.diagnostic_type_name(obj_ty)
                ),
            )
            .with_label(Label::primary(call.span, "expected `Channel<T>`")),
        );
        Type::Void
    }

    pub(super) fn check_channel_send(
        &mut self,
        obj_ty: &Type,
        call: &MethodCallExpr,
        spelling: &str,
        arity: usize,
    ) -> Type {
        let element_ty = channel_element_type(obj_ty).expect("checked channel receiver");
        // A sent value crosses task/worker boundaries, so the channel
        // item type must be Send (parallel-map Send checking, spec §10; gated like the
        // other data-race checks).
        if self.enforce_send_sync && *element_ty != Type::Void && !self.is_send(element_ty) {
            self.push(
                Diagnostic::new(
                    Severity::Error,
                    ErrorCode::E2403,
                    format!(
                        "channel item type `{}` must be `Send`",
                        self.diagnostic_type_name(element_ty)
                    ),
                )
                .with_label(Label::primary(
                    call.span,
                    "this value crosses a task boundary",
                ))
                .with_help(
                    "send Send values (scalars, String, frozen/Sync types) through channels",
                ),
            );
        }
        self.check_builtin_arity(
            BuiltinArity::new(arity, ArityText::Plain),
            "Channel",
            spelling,
            call.args.len(),
            call.span,
        );
        if let Some(arg) = call.args.first() {
            let arg_ty = self.check_expr_expecting(&arg.expr, element_ty);
            // Record each suspended value independently: a
            // cooperative `send` on a bounded channel is a suspend
            // point, so the lowering frame-backs channel + value and
            // carries the value into the task.
            if self.local.current_async_context && *element_ty != Type::Void {
                self.record_async_local(
                    element_ty.clone(),
                    "channel send value",
                    arg.expr.span(),
                    super::async_retention::Key::Value(arg.expr.id()),
                );
            }
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
                            self.diagnostic_type_name(element_ty),
                            self.diagnostic_type_name(&arg_ty)
                        ),
                    )),
                );
            } else {
                self.check_builtin_argument(
                    BuiltinArgument::Channel,
                    "Channel",
                    spelling,
                    element_ty,
                    &arg_ty,
                    arg.expr.span(),
                );
            }
        }
        Type::Void
    }
}
