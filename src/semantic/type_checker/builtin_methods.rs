//! Builtin call contracts. Keep legacy diagnostic spelling and evaluation
//! policy separate: an arity error must not change which expressions are checked.
use super::*;

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

    fn label(mut self, label: &'static str) -> Self {
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
            ArityText::Display => "`toString()` takes no arguments".into(),
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

/// A match table avoids a scan or allocation per call. Receiver and method
/// names have a fixed maximum length in the recognized set.
pub(super) fn option_method(owner: B, method: &str) -> Option<(BuiltinArity, OptionAction)> {
    use OptionAction::*;
    let action = match (owner, method) {
        (B::Option, "is_some" | "is_none") | (B::Result, "is_ok" | "is_err") => Bool,
        (B::Option | B::Result, "unwrap") => Payload,
        (B::Result, "unwrap_err") => ErrorPayload,
        (B::Option | B::Result, "expect") => Message,
        (B::Option | B::Result, "unwrap_or") => Default,
        (B::Option, "map") => Transform {
            error_input: false,
            no_input: false,
            result: TransformResult::Option,
        },
        (B::Result, "map") => Transform {
            error_input: false,
            no_input: false,
            result: TransformResult::ResultOk,
        },
        (B::Result, "map_err") => Transform {
            error_input: true,
            no_input: false,
            result: TransformResult::ResultErr,
        },
        (B::Option | B::Result, "and_then") => Transform {
            error_input: false,
            no_input: false,
            result: TransformResult::Direct,
        },
        (B::Option, "or_else") => Transform {
            error_input: false,
            no_input: true,
            result: TransformResult::Direct,
        },
        (B::Result, "or_else") => Transform {
            error_input: true,
            no_input: false,
            result: TransformResult::Direct,
        },
        _ => return None,
    };
    let (count, suffix) = match action {
        Bool | Payload | ErrorPayload => (0, ""),
        Message => (1, " (message)"),
        Default => (1, ""),
        Transform { .. } => (1, " (fn)"),
    };
    let arity = BuiltinArity::new(count, ArityText::Method(suffix)).label(if count == 0 {
        "unexpected arguments"
    } else {
        "wrong number of arguments"
    });
    Some((arity, action))
}

pub(super) fn method_arity(owner: &str, method: &str) -> BuiltinArity {
    use ArityText::*;
    match (owner, method) {
        ("Map", _) => map_method(method).expect("known Map method").0,
        ("Channel", "send") => BuiltinArity::new(1, Plain),
        ("Channel", "recv" | "recv_opt" | "close")
        | ("Task", "result" | "cancel" | "is_cancelled") => BuiltinArity::new(0, Plain),
        ("CancellationToken" | "TaskScope", "attach" | "add") => BuiltinArity::new(1, Method("")),
        ("CancellationToken" | "TaskScope", "cancel" | "is_cancelled" | "child" | "finish") => {
            BuiltinArity::new(0, QualifiedCount)
        }
        ("primitive", "toString") => BuiltinArity::new(0, Display).label("unexpected arguments"),
        _ => unreachable!("missing builtin arity: {owner}::{method}"),
    }
}

pub(super) fn constructor_arity(owner: &str, method: &str) -> BuiltinArity {
    if owner == "Map" {
        return BuiltinArity::new(0, ArityText::Method("")).label("unexpected arguments");
    }
    if matches!(owner, "CancellationToken" | "TaskScope") {
        return BuiltinArity::new(0, ArityText::Constructor).label("wrong constructor arguments");
    }
    if owner == "Channel" && method == "new" {
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
    /// Used only on paths that already evaluated all arguments without context.
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
        self.push(
            Diagnostic::new(Severity::Error, code, message).with_label(Label::primary(span, label)),
        );
        false
    }
}

/// (arity, checks key, checks value). Incorrect arity intentionally checks neither.
pub(super) fn map_method(method: &str) -> Option<(BuiltinArity, bool, bool)> {
    use ArityText::Method;
    Some(match method {
        "insert" => (
            BuiltinArity::new(2, Method("")).label("expected `insert(key, value)`"),
            true,
            true,
        ),
        "get" => (
            BuiltinArity::new(1, Method("")).label("expected `get(key)`"),
            true,
            false,
        ),
        "remove" => (
            BuiltinArity::new(1, Method("")).label("expected `remove(key)`"),
            true,
            false,
        ),
        "contains" => (
            BuiltinArity::new(1, Method("")).label("expected `contains(key)`"),
            true,
            false,
        ),
        "keys" | "values" | "entries" | "len" | "freeze" | "toString" => (
            BuiltinArity::new(0, Method("")).label("unexpected arguments"),
            false,
            false,
        ),
        _ => return None,
    })
}
