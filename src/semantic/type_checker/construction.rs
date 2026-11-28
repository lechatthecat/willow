//! Objects are constructed only with `new`; the call form `Class(args)` is
//! rejected with a fix that keeps the written qualifier and type arguments.
use super::*;

pub(super) struct Construction<'a> {
    pub class_name: String,
    pub type_args: &'a [Type],
    pub args: &'a [CallArg],
    pub id: ExprId,
    pub span: Span,
}

#[willow_continuations::checker]
impl TypeChecker {
    pub(super) fn construction<'a>(&self, expr: &'a Expr) -> Option<Construction<'a>> {
        let Expr::New(n) = expr else {
            return None;
        };
        Some(Construction {
            class_name: n.class_name.clone(),
            type_args: &n.type_args,
            args: &n.args,
            id: n.id,
            span: n.span,
        })
    }

    /// A call spelled like a constructor without `new` (`User(x)`,
    /// `model::User(x)`, `Box<i64>(x)`). Variables, functions and generic
    /// functions keep precedence, exactly as for ordinary calls, so only a
    /// callee that resolves to nothing but a class is reported.
    pub(super) fn call_form_class<'a>(
        &self,
        expr: &'a Expr,
    ) -> Option<(String, &'a [Type], &'a [CallArg])> {
        let (name, type_args, args) = match expr {
            Expr::Call(c)
                if self.symbols.lookup_var(&c.callee).is_none()
                    && self.symbols.lookup_func(&c.callee).is_none()
                    && !self.generics.functions.contains_key(&c.callee) =>
            {
                (c.callee.clone(), c.type_args.as_slice(), c.args.as_slice())
            }
            Expr::StaticCall(c) if !c.is_bare_member() => {
                let name = QualifiedName::join(&c.class, &c.method);
                if self
                    .symbols
                    .lookup_module_func(&c.class, &c.method)
                    .is_some()
                    || self.generics.functions.contains_key(&name)
                {
                    return None;
                }
                (name, c.type_args.as_slice(), c.args.as_slice())
            }
            _ => return None,
        };
        let class = self.generics.classes.contains_key(&name)
            || self
                .resolve_static_call_class_quiet(&name)
                .is_some_and(|resolved| self.symbols.lookup_class(&resolved).is_some());
        class.then_some((name, type_args, args))
    }

    /// Reject `Class(args)`: objects are constructed only with `new`. The
    /// arguments still go through the constructor check, so their own errors
    /// (arity, types, lambda parameter context) surface exactly as they would
    /// with `new`; the result is the error type so the missing `new` does not
    /// cascade into later uses.
    pub(super) fn reject_call_form_construction(
        &mut self,
        expr: &Expr,
        name: &str,
        type_args: &[Type],
        args: &[CallArg],
    ) -> Type {
        self.check_constructor_call(&Construction {
            class_name: name.to_owned(),
            type_args,
            args,
            id: expr.id(),
            span: expr.span(),
        });
        let written = if type_args.is_empty() {
            name.to_owned()
        } else {
            format!(
                "{name}<{}>",
                type_args
                    .iter()
                    .map(|ty| self.diagnostic_type_name(ty))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        self.push(
            Self::error_at(
                ErrorCode::E0843,
                format!("class `{name}` can only be constructed with `new`"),
                expr.span(),
                "`new` is missing",
            )
            .with_help(format!("write `new {written}(...)` instead")),
        );
        Self::error_type()
    }

    pub(super) fn check_constructor_call(&mut self, n: &Construction<'_>) -> Type {
        if n.type_args.is_empty()
            && let Some(ty) = self.check_inferred_generic_new(n)
        {
            return ty;
        }
        if let Some(ty) = self.instantiate_class(&n.class_name, n.type_args, n.span) {
            if let Type::Named(name) = &ty {
                self.generics.objects.insert(n.id, name.clone());
                return self.check_new_with_effects(n, name, false);
            }
            return ty;
        }
        if !n.type_args.is_empty() {
            self.generic_error(
                n.span,
                format!(
                    "class `{}` is not generic and does not accept type arguments",
                    n.class_name
                ),
            );
        }
        self.check_new_with_effects(n, &n.class_name, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualified_generic_function_precedence_uses_registered_name_indexes() {
        // Generic duplicate declarations are rejected in source. Exercise the
        // dispatcher independently so a registered function wins even when a
        // class lookup is also available under a qualified/alias spelling.
        for owner in ["model", "alias"] {
            let source = format!(
                "class C<T> {{ pub value: T; }} fn identity<T>(value: T) -> T {{ value }} fn main() {{ {owner}::C<i64>(1); }}"
            );
            let (program, errors) =
                crate::parser::Parser::new(crate::lexer::Lexer::new(&source).tokenize().unwrap())
                    .parse();
            assert!(errors.is_empty(), "{errors:?}");
            let (Item::Class(class), Item::Function(function), Item::Function(main)) =
                (&program.items[0], &program.items[1], &program.items[2])
            else {
                unreachable!()
            };
            let Stmt::Expr(statement) = &main.body.stmts[0] else {
                unreachable!()
            };
            let mut checker = TypeChecker::new();
            let name = QualifiedName::join(owner, "C");
            let class = checker.generics.registry.borrow_mut().intern_class(class);
            checker.generics.classes.insert(name.clone(), class);
            assert!(checker.call_form_class(&statement.expr).is_some());
            let function = checker
                .generics
                .registry
                .borrow_mut()
                .intern_function(function);
            checker.generics.functions.insert(name, function);
            assert!(checker.call_form_class(&statement.expr).is_none());
        }
    }

    #[test]
    fn constructor_inference_is_linear_and_shared() {
        // Valid `new` sites and rejected call-form sites share one inference
        // path; neither grows faster than the number of sites.
        for call_form in [false, true] {
            let mut previous = None;
            for count in [8, 32, 128] {
                let prefix = if call_form { "" } else { "new " };
                let source = format!(
                    "class Box<T> {{ pub value: T; }} fn main() {{ {} }}",
                    (0..count)
                        .map(|i| format!("let value{i} = {prefix}Box({prefix}Box(1));"))
                        .collect::<String>()
                );
                let (program, errors) = crate::parser::Parser::new(
                    crate::lexer::Lexer::new(&source).tokenize().unwrap(),
                )
                .parse();
                assert!(errors.is_empty(), "{errors:?}");
                super::super::generics::GENERIC_NEW_INFERENCES.with(|n| n.set(0));
                let mut checker = TypeChecker::new();
                checker.check_program(&program);
                let rejected = checker
                    .errors
                    .iter()
                    .filter(|e| e.code == ErrorCode::E0843)
                    .count();
                assert_eq!(rejected, if call_form { 2 * count } else { 0 });
                assert_eq!(checker.errors.len(), rejected, "{:?}", checker.errors);
                let work = super::super::generics::GENERIC_NEW_INFERENCES.with(|n| n.get());
                if let Some((old_count, old_work)) = previous {
                    assert_eq!(work * old_count, old_work * count);
                }
                assert!(work >= count);
                eprintln!(
                    "call_form={call_form} sites={} inference_checks={work}",
                    count * 2
                );
                previous = Some((count, work));
            }
        }
    }
}
