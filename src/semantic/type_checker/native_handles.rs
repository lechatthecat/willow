//! Native method resolution and conservative local ownership diagnostics.
use super::*;
use crate::parser::iter::{AstEvent, AstWalk};
use crate::semantic::symbols::FuncInfo;

struct LocalHandle {
    name: String,
    span: Span,
    owned: bool,
    escaped: bool,
    uncertain: bool,
    closed: bool,
    abandoned: bool,
    consumed: bool,
    invalid_reads: Vec<Span>,
}

#[derive(Default)]
struct Locals {
    values: Vec<LocalHandle>,
    names: HashMap<String, Vec<usize>>,
    scopes: Vec<Vec<String>>,
}
impl Locals {
    fn get(&self, name: &str) -> Option<usize> {
        self.names.get(name)?.last().copied()
    }
    fn bind(&mut self, name: &str, owned: bool, span: Span) {
        let index = self.values.len();
        self.values.push(LocalHandle {
            name: name.into(),
            owned,
            span,
            escaped: false,
            uncertain: false,
            closed: false,
            abandoned: false,
            consumed: false,
            invalid_reads: Vec::new(),
        });
        self.names.entry(name.into()).or_default().push(index);
        self.scopes.last_mut().unwrap().push(name.into());
    }
    fn leave(&mut self) {
        for name in self.scopes.pop().unwrap() {
            self.names.get_mut(&name).unwrap().pop();
        }
    }
}

// Only direct native receivers preserve local eligibility. Every other use
// can introduce an alias, including a copy which stays in the same scope.
#[derive(Clone, Copy)]
struct ReceiverUse {
    consumes: bool,
    automatic: bool,
}

impl TypeChecker {
    fn native_call_info(&self, expr: &Expr) -> Option<&FuncInfo> {
        let name = match self.analysis_calls.get(&expr.id()) {
            Some(target) => target.as_ref()?.to_string(),
            None => match expr {
                Expr::Call(c) => c.callee.clone(),
                Expr::StaticCall(c) => format!("{}::{}", c.class, c.method),
                _ => return None,
            },
        };
        self.symbols.native_function(&name)
    }

    fn owned_native_result(&self, expr: &Expr) -> bool {
        let mut expr = expr;
        loop {
            expr = match expr {
                Expr::TryPropagate(inner, ..) => inner,
                Expr::Await(awaited) => &awaited.expr,
                _ => break,
            };
        }
        self.native_call_info(expr)
            .is_some_and(|info| info.native.is_some())
    }

    fn opaque_expr(&self, expr: &Expr) -> bool {
        let Some(Type::Named(name)) = self.expr_types.get(&expr.id()) else {
            return false;
        };
        self.symbols
            .lookup_enum(name)
            .is_some_and(|info| info.variants.len() == 1 && info.variants[0].name == "$rust_handle")
    }

    /// One structural walk and one final binding pass. No alias provenance,
    /// branch snapshots, container projections or repeated deferred traversal.
    pub(super) fn check_native_ownership(&mut self, root: AstEvent<'_>) {
        let mut locals = Locals::default();
        locals.scopes.push(Vec::new());
        let mut statements = Vec::new();
        let mut receivers = HashMap::<ExprId, ReceiverUse>::new();
        let mut calls = HashMap::<ExprId, (usize, ReceiverUse)>::new();
        let mut control = 0usize;
        let mut walk = AstWalk::new(root);
        while let Some(event) = walk.next() {
            #[cfg(test)]
            NATIVE_LOCAL_VISITS.with(|visits| visits.set(visits.get() + 1));
            match event {
                AstEvent::EnterScope => locals.scopes.push(Vec::new()),
                AstEvent::ExitScope => locals.leave(),
                AstEvent::Lambda(lambda) => {
                    if let Some(names) = self.local.native_captures.get(&lambda.id) {
                        for name in names {
                            if let Some(id) = locals.get(name) {
                                locals.values[id].escaped = true;
                            }
                        }
                    }
                    walk.skip_children();
                }
                AstEvent::Stmt(stmt) => {
                    if control_statement(stmt) {
                        control += 1;
                    }
                    if let Stmt::Assign(assign) = stmt
                        && let Some(id) = locals.get(&assign.name)
                    {
                        // A rebind can replace a generation or be observed by
                        // a captured reference. Leave the entire local dynamic.
                        locals.values[id].uncertain = true;
                    }
                    statements.push(stmt);
                }
                AstEvent::ExitStmt(stmt) => {
                    if let Stmt::Assign(assign) = stmt
                        && let Some(id) = locals.get(&assign.name)
                    {
                        let fresh = self.opaque_expr(&assign.value)
                            && self.owned_native_result(&assign.value);
                        let local = &mut locals.values[id];
                        if fresh {
                            local.abandoned |= local.owned && !local.closed;
                            local.owned = true;
                            local.closed = false;
                        } else {
                            local.escaped = true;
                        }
                    }
                    if control_statement(stmt) {
                        control -= 1;
                    }
                    statements.pop();
                }
                AstEvent::Bind(name) => {
                    let (owned, span) = match statements.last() {
                        Some(Stmt::Let(binding)) if binding.name == name => (
                            self.opaque_expr(&binding.init)
                                && self.owned_native_result(&binding.init),
                            binding.span,
                        ),
                        _ => (false, Span::dummy()),
                    };
                    locals.bind(name, owned, span);
                }
                AstEvent::Expr(expr) => {
                    if control_expression(expr) {
                        control += 1;
                    }
                    if let Expr::MethodCall(call) = expr
                        && let Some(native) = self
                            .native_call_info(expr)
                            .and_then(|info| info.native.as_ref())
                        && native.receiver
                        && let Expr::Var(name, _, id) = &call.object
                    {
                        let usage = ReceiverUse {
                            consumes: native.closes || native.consumes.contains(&0),
                            automatic: native.automatic,
                        };
                        receivers.insert(*id, usage);
                        if let Some(local) = locals.get(name) {
                            calls.insert(call.id, (local, usage));
                        }
                    }
                    if let Expr::Var(name, span, id) = expr
                        && let Some(index) = locals.get(name)
                    {
                        let local = &mut locals.values[index];
                        let usage = receivers.get(id);
                        local.escaped |= usage.is_none();
                        local.uncertain |= control > 0 && !usage.is_some_and(|u| u.automatic);
                        if !usage.is_some_and(|u| u.automatic) && local.consumed {
                            local.invalid_reads.push(*span);
                        }
                    }
                }
                AstEvent::ExitExpr(expr) => {
                    if let Some((index, usage)) = calls.remove(&expr.id()) {
                        let local = &mut locals.values[index];
                        if usage.consumes {
                            local.closed = true;
                            if !usage.automatic {
                                local.consumed = true;
                            }
                        }
                    }
                    if control_expression(expr) {
                        control -= 1;
                    }
                }
                _ => {}
            }
        }
        for local in locals.values {
            if local.escaped {
                continue;
            }
            if !local.uncertain {
                for span in local.invalid_reads {
                    self.push(
                        Diagnostic::new(
                            Severity::Error,
                            ErrorCode::E0906,
                            format!("Rust handle `{}` used after consumption", local.name),
                        )
                        .with_label(Label::primary(span, "this handle was already consumed")),
                    );
                }
            }
            if (local.abandoned || (local.owned && !local.closed)) && !self.template_precheck {
                self.push(
                    Diagnostic::new(
                        Severity::Warning,
                        ErrorCode::W0906,
                        format!("owned Rust handle `{}` is not closed", local.name),
                    )
                    .with_label(Label::primary(
                        local.span,
                        "use `use` or close this handle explicitly",
                    )),
                );
            }
        }
    }
}

fn control_statement(stmt: &Stmt) -> bool {
    matches!(
        stmt,
        Stmt::If(_) | Stmt::While(_) | Stmt::For(_) | Stmt::Defer(_)
    )
}
fn control_expression(expr: &Expr) -> bool {
    matches!(expr, Expr::Ternary(_) | Expr::Match(_) | Expr::Select(_))
        || matches!(expr, Expr::Binary(binary) if matches!(binary.op, BinOp::And | BinOp::Or))
}

#[cfg(test)]
thread_local! {
    static NATIVE_LOCAL_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[willow_continuations::checker]
impl TypeChecker {
    pub(super) fn native_member(
        &self,
        owner: &str,
        method: &str,
        receiver: bool,
    ) -> Option<(String, FuncInfo)> {
        let alternate_target = match method {
            "close" => Some("close$method"),
            m if receiver && (m == "dispose" || m.starts_with("dispose_")) => Some("close"),
            _ => None,
        };
        if let Some(suffix) = alternate_target {
            let name = format!("{owner}_{suffix}");
            if let Some(info) = self.symbols.native_function(&name)
                && let Some(native) = info.native.as_ref()
                && native.method.as_deref() == Some(method)
                && native.receiver == receiver
            {
                return Some((name, info.clone()));
            }
        }
        let suffix = if method == "$use_close" {
            "close$scope"
        } else {
            method
        };
        let name = format!("{owner}_{suffix}");
        let info = self.symbols.native_function(&name)?;
        let native = info.native.as_ref()?;
        if native.method.as_deref() != Some(method) || native.receiver != receiver {
            return None;
        }
        Some((name, info.clone()))
    }

    pub(super) fn check_native_member(
        &mut self,
        id: ExprId,
        span: Span,
        name: String,
        info: FuncInfo,
        args: &[CallArg],
        receiver: bool,
    ) -> Type {
        let params = &info.param_infos[usize::from(receiver)..];
        self.check_call_argument_count(&name, params.len(), args.len(), span);
        self.check_call_args_against_param_infos(params, args, false);
        for arg in args.iter().skip(params.len()) {
            self.check_expr(&arg.expr);
        }
        self.check_async_call_captures(params, args, info.is_async);
        if info.foreign {
            self.check_foreign_call(span, info.is_async);
        }
        let target = FunctionId::free_from_source_name(&name);
        self.analysis_calls.insert(id, Some(target));
        self.record_lock_effect_call(target, span);
        function_call_return_type(&info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_local_analysis_visits_scale_additively() {
        let mut samples = HashMap::new();
        for locals in [1, 16, 128] {
            for reads in [1, 16, 128] {
                let bindings = (0..locals)
                    .map(|i| format!("let h{i}=native::Handle::new(1);"))
                    .collect::<String>();
                let source = format!("fn work() {{ {bindings} {} }}", "h0.read();".repeat(reads));
                let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
                let (program, errors) = crate::parser::Parser::new(tokens).parse();
                assert!(errors.is_empty());
                let Item::Function(function) = &program.items[0] else {
                    panic!()
                };
                NATIVE_LOCAL_VISITS.set(0);
                TypeChecker::new().check_native_ownership(AstEvent::Block(&function.body));
                let visits = NATIVE_LOCAL_VISITS.get();
                assert_eq!(visits, 8 * locals + 7 * reads + 3);
                samples.insert((locals, reads), visits);
                println!("locals={locals} reads={reads} visits={visits}");
            }
        }
        assert_eq!(
            samples[&(128, 128)] - samples[&(128, 1)],
            samples[&(1, 128)] - samples[&(1, 1)]
        );
    }
}
