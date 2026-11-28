//! Both constructor spellings share borrowed arguments and one checking path.
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
    pub(super) fn is_constructor_call(&self, expr: &Expr) -> bool {
        self.constructor_calls.contains(&expr.id())
    }

    pub(super) fn construction<'a>(&self, expr: &'a Expr) -> Option<Construction<'a>> {
        let (class_name, type_args, args) = match expr {
            Expr::New(n) => (
                n.class_name.clone(),
                n.type_args.as_slice(),
                n.args.as_slice(),
            ),
            Expr::Call(c)
                if self.symbols.lookup_var(&c.callee).is_none()
                    && self.symbols.lookup_func(&c.callee).is_none()
                    && !self.generics.functions.contains_key(&c.callee) =>
            {
                (c.callee.clone(), c.type_args.as_slice(), c.args.as_slice())
            }
            Expr::StaticCall(c) if !c.is_bare_member() => {
                let name = QualifiedName::join(&c.class, &c.method);
                // Use the same module/alias and generic indexes as ordinary
                // static calls. A function keeps precedence even when a class
                // shares its spelling, or its arguments will be diagnosed.
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
        if !matches!(expr, Expr::New(_))
            && !self.generics.classes.contains_key(&class_name)
            && self
                .resolve_static_call_class_quiet(&class_name)
                .is_none_or(|name| self.symbols.lookup_class(&name).is_none())
        {
            return None;
        }
        Some(Construction {
            class_name,
            type_args,
            args,
            id: expr.id(),
            span: expr.span(),
        })
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
            assert!(checker.construction(&statement.expr).is_some());
            let function = checker
                .generics
                .registry
                .borrow_mut()
                .intern_function(function);
            checker.generics.functions.insert(name, function);
            assert!(checker.construction(&statement.expr).is_none());
        }
    }

    #[test]
    fn concise_constructor_inference_is_linear_and_shared() {
        for legacy in [false, true] {
            let mut previous = None;
            for count in [8, 32, 128] {
                let prefix = if legacy { "new " } else { "" };
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
                assert!(checker.errors.is_empty(), "{:?}", checker.errors);
                let work = super::super::generics::GENERIC_NEW_INFERENCES.with(|n| n.get());
                if let Some((old_count, old_work)) = previous {
                    assert_eq!(work * old_count, old_work * count);
                }
                assert!(work >= count);
                eprintln!(
                    "legacy={legacy} sites={} inference_checks={work}",
                    count * 2
                );
                previous = Some((count, work));
            }
        }
    }
}
