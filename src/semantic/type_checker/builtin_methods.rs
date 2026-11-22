//! Builtin call contracts. Keep legacy diagnostic spelling and evaluation
//! policy separate: an arity error must not change which expressions are checked.
use super::*;
use crate::semantic::builtin_types::BuiltinTypeId as Builtin;

#[cfg(test)]
thread_local! {
    pub(super) static BUILTIN_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}

#[derive(Clone, Copy)]
pub(super) enum ArityText {
    Method(&'static str),
    Plain,
    QualifiedCount,
    Function,
    FunctionPlural,
    ArgumentS,
    ValueCall,
    Constructor,
    Display,
}

#[derive(Clone, Copy)]
pub(super) struct BuiltinArity {
    pub count: usize,
    text: ArityText,
    label: &'static str,
    help: Option<&'static str>,
}

impl BuiltinArity {
    pub const fn new(count: usize, text: ArityText) -> Self {
        Self {
            count,
            text,
            label: "wrong number of arguments",
            help: None,
        }
    }

    pub(super) fn label(mut self, label: &'static str) -> Self {
        self.label = label;
        self
    }

    pub fn diagnostic(self, owner: &str, method: &str, given: usize, span: Span) -> Diagnostic {
        let count = self.count;
        let message = match self.text {
            ArityText::Method(_) if count == 0 => format!("`{owner}::{method}` takes no arguments"),
            ArityText::Method(suffix) => format!(
                "`{owner}::{method}` expects {count} argument{}{suffix}, got {given}",
                if count == 1 { "" } else { "s" }
            ),
            ArityText::QualifiedCount => {
                format!("`{owner}::{method}` expects {count} arguments, got {given}")
            }
            ArityText::Plain => format!(
                "{method} expects {count} argument{}, got {given}",
                if count == 1 { "" } else { "s" }
            ),
            ArityText::Function => format!(
                "function `{owner}::{method}` expects {count} argument{}, got {given}",
                if count == 1 { "" } else { "s" }
            ),
            ArityText::FunctionPlural => {
                format!("function `{owner}::{method}` expects {count} arguments, got {given}")
            }
            ArityText::ArgumentS => {
                format!("`{owner}::{method}` expects {count} argument(s), got {given}")
            }
            ArityText::ValueCall => {
                format!("{method} takes {count} argument(s) but {given} were supplied")
            }
            ArityText::Constructor => format!("function `{owner}::{method}` expects no arguments"),
            ArityText::Display => "`to_string()` takes no arguments".into(),
        };
        let mut diagnostic = Diagnostic::new(Severity::Error, ErrorCode::E0201, message)
            .with_label(Label::primary(span, self.label));
        if let Some(help) = self.help {
            diagnostic = diagnostic.with_help(help);
        }
        diagnostic
    }
}

#[derive(Clone, Copy)]
pub(super) enum OptionAction {
    Bool,
    Payload,
    ErrorPayload,
    Message,
    Default,
    Transform {
        error_input: bool,
        no_input: bool,
        result: TransformResult,
    },
}

#[derive(Clone, Copy)]
pub(super) enum TransformResult {
    Option,
    ResultOk,
    ResultErr,
    Direct,
}

pub(super) fn option_action(owner: B, method: AdtMethod) -> OptionAction {
    use OptionAction::*;
    match (owner, method) {
        (B::Option, AdtMethod::IsSome | AdtMethod::IsNone)
        | (B::Result, AdtMethod::IsOk | AdtMethod::IsErr) => Bool,
        (B::Option | B::Result, AdtMethod::Unwrap) => Payload,
        (B::Result, AdtMethod::UnwrapErr) => ErrorPayload,
        (B::Option | B::Result, AdtMethod::Expect) => Message,
        (B::Option | B::Result, AdtMethod::UnwrapOr) => Default,
        (B::Option, AdtMethod::Map) => Transform {
            error_input: false,
            no_input: false,
            result: TransformResult::Option,
        },
        (B::Result, AdtMethod::Map) => Transform {
            error_input: false,
            no_input: false,
            result: TransformResult::ResultOk,
        },
        (B::Result, AdtMethod::MapErr) => Transform {
            error_input: true,
            no_input: false,
            result: TransformResult::ResultErr,
        },
        (B::Option | B::Result, AdtMethod::AndThen) => Transform {
            error_input: false,
            no_input: false,
            result: TransformResult::Direct,
        },
        (B::Option, AdtMethod::OrElse) => Transform {
            error_input: false,
            no_input: true,
            result: TransformResult::Direct,
        },
        (B::Result, AdtMethod::OrElse) => Transform {
            error_input: true,
            no_input: false,
            result: TransformResult::Direct,
        },
        _ => unreachable!("receiver-checked Option/Result method"),
    }
}

pub(super) fn adt_arity(method: AdtMethod) -> BuiltinArity {
    let count = method.arity();
    let suffix = if method.callback() {
        " (fn)"
    } else if method == AdtMethod::Expect {
        " (message)"
    } else {
        ""
    };
    BuiltinArity::new(count, ArityText::Method(suffix)).label(if count == 0 {
        "unexpected arguments"
    } else {
        "wrong number of arguments"
    })
}

pub(super) fn constructor_arity(owner: &str, method: &str) -> BuiltinArity {
    let owner_id = Builtin::from_name(owner);
    if owner_id == Some(Builtin::Map) {
        return BuiltinArity::new(0, ArityText::Method("")).label("unexpected arguments");
    }
    if matches!(
        owner_id,
        Some(Builtin::CancellationToken | Builtin::TaskScope)
    ) {
        return BuiltinArity::new(0, ArityText::Constructor).label("wrong constructor arguments");
    }
    if owner_id == Some(Builtin::Channel) && method == "new" {
        let mut arity = BuiltinArity::new(0, ArityText::Function);
        arity.help = Some("use `Channel::with_capacity(n)` to create a bounded channel");
        return arity;
    }
    BuiltinArity::new(1, ArityText::Function)
}

impl TypeChecker {
    pub(super) fn check_builtin_arity(
        &mut self,
        arity: BuiltinArity,
        owner: &str,
        method: &str,
        supplied: usize,
        span: Span,
    ) -> bool {
        #[cfg(test)]
        BUILTIN_WORK.with(|work| {
            let (calls, args) = work.get();
            work.set((calls + 1, args));
        });
        if supplied == arity.count {
            return true;
        }
        self.push(arity.diagnostic(owner, method, supplied, span));
        false
    }
}

#[willow_continuations::checker]
impl TypeChecker {
    /// Missing members normally report the receiver first. The retired waits
    /// historically report independent argument errors before the receiver.
    pub(super) fn report_missing_method_with_args(
        &mut self,
        diagnostic: Diagnostic,
        args: &[CallArg],
        args_first: bool,
    ) {
        if args_first {
            self.check_discarded_builtin_args(args);
            self.push(diagnostic);
        } else {
            self.push(diagnostic);
            self.check_discarded_builtin_args(args);
        }
    }

    /// Check independent argument errors when there is no parameter context.
    pub(super) fn check_discarded_builtin_args(&mut self, args: &[CallArg]) {
        #[cfg(test)]
        BUILTIN_WORK.with(|work| {
            let (calls, count) = work.get();
            work.set((calls, count + args.len()));
        });
        for arg in args {
            self.check_expr(&arg.expr);
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum BuiltinArgument {
    Message,
    Default,
    Closure,
    MapKey,
    MapValue,
    Cell,
    Channel,
    Task,
    Constructor,
    LockConstructor,
    Capacity,
}

impl TypeChecker {
    pub(super) fn check_builtin_argument(
        &mut self,
        rule: BuiltinArgument,
        owner: &str,
        method: &str,
        expected: &Type,
        got: &Type,
        span: Span,
    ) -> bool {
        use BuiltinArgument::*;
        let compatible = match rule {
            Task => is_task_handle_type(got),
            Constructor | LockConstructor => got == expected || *got == Type::Never,
            Capacity => *got == Type::I64,
            Message => *got == Type::String || Self::contains_error_type(got),
            Default | Closure | Channel => self.types_compatible(expected, got),
            MapKey | MapValue => *expected == Type::Void || self.types_compatible(expected, got),
            Cell => {
                got == expected
                    || *got == Type::Never
                    || self.recovery_types_compatible(expected, got)
            }
        };
        if compatible {
            return true;
        }
        let found = self.diagnostic_type_name(got);
        let expected_type = expected;
        let expected = if matches!(rule, Message | Task | Capacity) {
            String::new()
        } else {
            self.diagnostic_type_name(expected)
        };
        let (code, message, label) = match rule {
            Task => (
                ErrorCode::E0201,
                format!("`{owner}::{method}` expects `Task<T>`, found `{found}`"),
                "not a Task handle".into(),
            ),
            Constructor => (
                ErrorCode::E0201,
                format!("`{owner}::{method}` expects `{expected}`, got `{found}`"),
                "wrong argument type".into(),
            ),
            LockConstructor => (
                ErrorCode::E0201,
                format!("`{owner}<{expected}>::{method}` expects `{expected}`, got `{found}`"),
                "wrong argument type".into(),
            ),
            Capacity => (
                ErrorCode::E0201,
                format!("channel capacity must be `i64`, found `{found}`"),
                "expected `i64`".into(),
            ),
            Message => (
                ErrorCode::E0201,
                format!("expect message must be `String`, found `{found}`"),
                "expected `String`".into(),
            ),
            Default => (
                ErrorCode::E0201,
                format!("mismatched types: expected `{expected}`, found `{found}`"),
                format!("expected `{expected}`"),
            ),
            Closure => (
                ErrorCode::E0201,
                format!(
                    "`{owner}::{method}` closure argument type mismatch: expected `{expected}`, found `{found}`"
                ),
                "type mismatch".into(),
            ),
            MapKey => (
                ErrorCode::E0201,
                format!("map key type mismatch: expected `{expected}`, found `{found}`"),
                "wrong key type".into(),
            ),
            MapValue => (
                ErrorCode::E0201,
                format!("map value type mismatch: expected `{expected}`, found `{found}`"),
                "wrong value type".into(),
            ),
            Cell => (
                ErrorCode::E0201,
                format!("`{owner}::{method}` expects `{expected}`, got `{found}`"),
                "wrong argument type".into(),
            ),
            Channel => (
                ErrorCode::E0802,
                format!("cannot send `{found}` into `Channel<{expected}>`"),
                format!("expected `{expected}`, found `{found}`"),
            ),
        };
        let diagnostic =
            Diagnostic::new(Severity::Error, code, message).with_label(Label::primary(span, label));
        self.push(self.with_callable_kind_help(diagnostic, expected_type, got));
        false
    }
}

/// Arity wording for Map methods. Argument roles follow the signature:
/// argument zero is a key and argument one (insert only) is a value.
pub(super) fn map_arity(sig: &ResolvedMethod) -> BuiltinArity {
    let label = match sig.intrinsic {
        Intrinsic::MapInsert => "expected `insert(key, value)`",
        Intrinsic::MapGet => "expected `get(key)`",
        Intrinsic::MapRemove => "expected `remove(key)`",
        Intrinsic::MapContains => "expected `contains(key)`",
        _ => "unexpected arguments",
    };
    BuiltinArity::new(sig.arity, ArityText::Method("")).label(label)
}
