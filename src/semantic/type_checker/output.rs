//! Console output uses ordinary names and the existing typed output intrinsic.
use super::*;

#[willow_continuations::checker]
impl TypeChecker {
    pub(super) fn output_function(&self, expr: &Expr) -> Option<bool> {
        match expr {
            Expr::Call(call) => {
                if self.symbols.lookup_var(&call.callee).is_some()
                    || self.symbols.lookup_func(&call.callee).is_some()
                    || self.generics.functions.contains_key(&call.callee)
                {
                    return None;
                }
                self.imported_output_functions
                    .get(&call.callee)
                    .copied()
                    .or(match call.callee.as_str() {
                        "print" => Some(false),
                        "println" => Some(true),
                        _ => None,
                    })
            }
            Expr::StaticCall(call)
                if call.class == "std::io"
                    || self
                        .imported_std_modules
                        .get(&call.class)
                        .is_some_and(|m| m.module == "io") =>
            {
                match call.method.as_str() {
                    "print" => Some(false),
                    "println" => Some(true),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    pub(super) fn check_output_call(&mut self, expr: &Expr, newline: bool) -> Type {
        let (args, types) = match expr {
            Expr::Call(call) => (&call.args, &call.type_args),
            Expr::StaticCall(call) => (&call.args, &call.type_args),
            _ => unreachable!(),
        };
        let name = if newline { "println" } else { "print" };
        self.check_call_argument_count(name, 1, args.len(), expr.span());
        if !types.is_empty() {
            self.generic_error(
                expr.span(),
                "output functions infer their argument type; remove the type arguments",
            );
        }
        for arg in args {
            if !matches!(arg.mode, CallArgMode::Value) {
                self.push(
                    Diagnostic::new(
                        Severity::Error,
                        ErrorCode::E0201,
                        "output functions take a value, not a reference argument",
                    )
                    .with_label(Label::primary(arg.span, "remove `&`")),
                );
            }
            self.check_print_expr(&arg.expr, &newline);
        }
        self.analysis_calls.insert(
            expr.id(),
            Some(FunctionId::method(TypeId::local("$output"), name)),
        );
        Type::Void
    }
}
