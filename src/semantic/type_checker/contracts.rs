use super::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::semantic::symbols::VarInfo;

#[willow_continuations::checker]
impl TypeChecker {
    fn contract_error(&mut self, span: Span, message: &str) {
        self.push(
            Diagnostic::new(Severity::Error, ErrorCode::E0204, message)
                .with_label(Label::primary(span, "invalid contract expression")),
        );
    }

    pub(super) fn check_loop_contracts(&mut self, invariants: &[Expr], variant: Option<&Expr>) {
        let mut immutable = HashMap::new();
        for (expr, expected) in invariants
            .iter()
            .map(|e| (e, Type::Bool))
            .chain(variant.into_iter().map(|e| (e, Type::I64)))
        {
            let ty = self.check_expr(expr);
            self.check_proof_expression(expr, None, false, &mut immutable);
            if ty != expected {
                self.contract_error(
                    expr.span(),
                    if expected == Type::Bool {
                        "loop invariant must have type bool"
                    } else {
                        "loop variant must have type i64"
                    },
                );
            }
        }
    }

    fn check_proof_expression(
        &mut self,
        expression: &Expr,
        params: Option<&HashMap<&str, &Param>>,
        post: bool,
        immutable: &mut HashMap<Type, bool>,
    ) {
        let mut bindings =
            crate::semantic::scopes::Scopes::with_frame(std::iter::empty::<(String, bool)>());
        let mut walk = AstWalk::new(AstEvent::Expr(expression));
        while let Some(event) = walk.next() {
            match event {
                AstEvent::EnterScope => bindings.push(),
                AstEvent::ExitScope => bindings.pop(),
                AstEvent::Bind(name) => bindings.insert(name.to_owned(), true),
                AstEvent::Expr(expr) => {
                    let allowed = match expr {
                        Expr::Integer(..)
                        | Expr::Float(..)
                        | Expr::Bool(..)
                        | Expr::Binary(_)
                        | Expr::Unary(_)
                        | Expr::Ternary(_)
                        | Expr::Index(..)
                        | Expr::FieldAccess(..) => true,
                        // Statement arms can mutate state or return from the enclosing
                        // function; runtime contracts permit expression arms only.
                        Expr::Match(m) => m
                            .arms
                            .iter()
                            .all(|arm| matches!(arm.body, MatchBody::Expr(_))),
                        Expr::MethodCall(c) => {
                            c.method == "len"
                                && c.args.is_empty()
                                && self.expr_types.get(&c.object.id()).is_some_and(|ty| {
                                    builtin_types::resolve(ty)
                                        .is_some_and(|b| b.id == B::FrozenArray)
                                        && self.verification_immutable_type(ty, immutable)
                                })
                        }
                        Expr::Call(c) if matches!(c.callee.as_str(), "forall" | "exists") => true,
                        Expr::Var(name, ..) => {
                            if bindings.get(name).copied().unwrap_or(false) {
                                true
                            } else if name == "result" && params.is_some() && !post {
                                false
                            } else {
                                let parameter = params.is_none_or(|params| {
                                    name == "result"
                                        || params
                                            .get(name.as_str())
                                            .is_some_and(|p| p.mode == ParamMode::Value)
                                });
                                (parameter
                                    && self.symbols.lookup_var(name).is_some_and(|v| {
                                        scalar(&v.ty)
                                            || self.verification_immutable_type(&v.ty, immutable)
                                    }))
                                    || self.symbols.lookup_func(name).is_some_and(|f| {
                                        f.constant.is_some() && scalar(&f.return_type)
                                    })
                            }
                        }
                        Expr::StaticField(s) => self
                            .symbols
                            .lookup_module_func(&s.class, &s.field)
                            .is_some_and(|f| f.constant.is_some() && scalar(&f.return_type)),
                        Expr::StaticCall(s) if s.args.is_empty() && s.type_args.is_empty() => self
                            .symbols
                            .lookup_module_func(&s.class, &s.method)
                            .is_some_and(|f| f.constant.is_some() && scalar(&f.return_type)),
                        _ => false,
                    };
                    if !allowed {
                        self.push(
                            Diagnostic::new(
                                Severity::Error,
                                ErrorCode::E2631,
                                "unsupported or mutable contract expression",
                            )
                            .with_label(Label::primary(
                                expr.span(),
                                "immutable proof expression required",
                            )),
                        );
                        walk.skip_children();
                    }
                }
                _ => {}
            }
        }
    }

    pub(super) fn check_contracts(&mut self, f: &FunctionDecl, return_type: &Type) {
        if f.requires.is_empty() && f.ensures.is_empty() {
            return;
        }
        let params: HashMap<_, _> = f.params.iter().map(|p| (p.name.as_str(), p)).collect();
        if params.contains_key("result") {
            self.contract_error(f.span, "result is reserved in functions with contracts");
        }
        // Keep the cache within this callable's dependency-observation epoch;
        // sharing it across checked bodies would hide their symbol reads.
        let mut immutable = HashMap::new();
        for (post, clauses) in [(false, &f.requires), (true, &f.ensures)] {
            self.symbols.push_scope();
            if post {
                self.define_var(
                    "result".into(),
                    VarInfo {
                        ty: return_type.clone(),
                        mutable: false,
                        is_param: false,
                        declaration_span: f.span,
                    },
                );
            }
            for clause in clauses {
                let ty = self.check_expr(clause);
                self.check_proof_expression(clause, Some(&params), post, &mut immutable);
                if ty != Type::Bool {
                    self.contract_error(
                        clause.span(),
                        "each requires/ensures clause must have type bool",
                    );
                }
            }
            self.symbols.pop_scope();
        }
    }
}

fn scalar(ty: &Type) -> bool {
    matches!(ty, Type::I64 | Type::Bool | Type::F64)
}
