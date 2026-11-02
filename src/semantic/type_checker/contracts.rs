use super::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::semantic::symbols::VarInfo;

impl TypeChecker {
    fn contract_error(&mut self, span: Span, message: &str) {
        self.push(
            Diagnostic::new(Severity::Error, ErrorCode::E0204, message)
                .with_label(Label::primary(span, "invalid contract expression")),
        );
    }

    pub(super) fn check_contracts(&mut self, f: &FunctionDecl, return_type: &Type) {
        if f.requires.is_empty() && f.ensures.is_empty() {
            return;
        }
        let params: HashMap<_, _> = f.params.iter().map(|p| (p.name.as_str(), p)).collect();
        if params.contains_key("result") {
            self.contract_error(f.span, "result is reserved in functions with contracts");
        }
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
                let mut walk = AstWalk::new(AstEvent::Expr(clause));
                while let Some(event) = walk.next() {
                    let AstEvent::Expr(expr) = event else {
                        continue;
                    };
                    let allowed = match expr {
                        Expr::Integer(..)
                        | Expr::Float(..)
                        | Expr::Bool(..)
                        | Expr::Binary(_)
                        | Expr::Unary(_)
                        | Expr::Ternary(_) => true,
                        Expr::Var(name, ..) if name == "result" => post && scalar(return_type),
                        Expr::Var(name, ..) => {
                            if let Some(param) = params.get(name.as_str()) {
                                matches!(param.mode, ParamMode::Value)
                                    && self.symbols.lookup_var(name).is_some_and(|v| scalar(&v.ty))
                            } else {
                                self.symbols
                                    .lookup_func(name)
                                    .is_some_and(|f| f.constant.is_some() && scalar(&f.return_type))
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
                        self.contract_error(expr.span(), "contracts may reference only scalar value parameters, constants, pure operators, and result in ensures");
                        walk.skip_children();
                    }
                }
                let ty = self.check_expr(clause);
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
