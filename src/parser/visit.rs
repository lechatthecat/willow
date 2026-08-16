//! One structural traversal of the AST, shared by every read-only whole-program
//! analysis (willow-uqzx.1.1, catalog item 3).
//!
//! Before this module the compiler carried nine hand-written whole-body
//! traversals, each with its own exhaustive `match` over [`Stmt`] and [`Expr`]:
//!
//! * the backend's panic-effect visitor and its local-name collector,
//! * the concurrency checker's helper call collector and loop detector,
//! * the debug source map's await-point and reference-call collectors,
//! * the type checker's lambda capture scan,
//! * the type checker's constructor self-field and super-init scans.
//!
//! Adding a syntax node meant remembering all nine; forgetting one produced a
//! silently incomplete analysis rather than a compile error. The `walk_*`
//! functions below hold the only copy of that match, so a new node has exactly
//! one place to be threaded through.
//!
//! # What this module is not
//!
//! It is a *structural* traversal only. It knows the shape of the tree and
//! nothing about types, symbols, effects, or lowering. Passes that rewrite or
//! produce a tree (`ast_passes`, `coop_anf`, `ir::lower`, the emitters) keep
//! their own traversals: they need to build a new node per arm, which a
//! borrow-only visitor cannot express.
//!
//! # Capabilities
//!
//! A visitor overrides `visit_*` and decides where the children go:
//!
//! * work **before** children — do the work, then call `walk_*`;
//! * work **after** children — call `walk_*`, then do the work;
//! * **skip** a subtree — override and never call `walk_*`. [`AstVisitor::visit_lambda`]
//!   exists as its own hook because the effect, call-graph and capture analyses
//!   deliberately stop at a lambda body, which is analyzed as a separate
//!   callable. The source map's collectors, which describe the source text
//!   rather than a callable, keep the default and walk into it.
//!
//! # Lexical scopes
//!
//! Scope tracking is opt-in: [`AstVisitor::enter_scope`], [`AstVisitor::exit_scope`]
//! and [`AstVisitor::bind`] default to nothing, so a visitor that does not care
//! pays nothing. A visitor that does care gets one consistent shadowing rule for
//! free, because the walker calls the hooks at the points where a name actually
//! becomes visible:
//!
//! * a block opens and closes a scope;
//! * a `let` binds **after** its initializer is walked, so `let f = f();` still
//!   sees the outer `f` in the initializer;
//! * `for`, `lock`, a lambda, a `match` arm and a `select` receive/join case each
//!   open a scope holding their binding, around their body.

use super::ast::*;

/// A read-only visitor over the AST. Every method has a default that delegates
/// to the matching free `walk_*` function, so an implementor overrides only the
/// nodes it cares about.
pub trait AstVisitor {
    fn visit_block(&mut self, block: &Block) {
        walk_block(self, block);
    }

    fn visit_stmt(&mut self, stmt: &Stmt) {
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &Expr) {
        walk_expr(self, expr);
    }

    /// A lambda body is a separate callable in every analysis that exists
    /// today, so this hook is the documented place to stop. Override with an
    /// empty body to skip the body while keeping the rest of the traversal.
    fn visit_lambda(&mut self, lambda: &LambdaExpr) {
        walk_lambda(self, lambda);
    }

    /// Patterns contribute bindings, never sub-expressions.
    fn visit_pattern(&mut self, pattern: &Pattern) {
        walk_pattern(self, pattern);
    }

    /// A new lexical scope opens. Paired with [`Self::exit_scope`].
    fn enter_scope(&mut self) {}

    /// The innermost lexical scope closes.
    fn exit_scope(&mut self) {}

    /// `name` becomes visible in the innermost open scope.
    fn bind(&mut self, _name: &str) {}
}

/// Walk a block: one lexical scope around its statements.
pub fn walk_block<V: AstVisitor + ?Sized>(visitor: &mut V, block: &Block) {
    visitor.enter_scope();
    for stmt in &block.stmts {
        visitor.visit_stmt(stmt);
    }
    visitor.exit_scope();
}

/// Walk a statement's children in evaluation order.
///
/// The `match` is exhaustive and has no catch-all arm, so a new [`Stmt`] variant
/// is a compile error here rather than a silently dropped subtree.
pub fn walk_stmt<V: AstVisitor + ?Sized>(visitor: &mut V, stmt: &Stmt) {
    match stmt {
        Stmt::Let(stmt) => {
            // The initializer is evaluated before the binding exists.
            visitor.visit_expr(&stmt.init);
            visitor.bind(&stmt.name);
        }
        Stmt::Assign(stmt) => visitor.visit_expr(&stmt.value),
        Stmt::FieldAssign(stmt) => {
            visitor.visit_expr(&stmt.object);
            visitor.visit_expr(&stmt.value);
        }
        Stmt::SuperInit(stmt) => walk_args(visitor, &stmt.args),
        Stmt::StaticFieldAssign(stmt) => visitor.visit_expr(&stmt.value),
        Stmt::IndexAssign(stmt) => {
            visitor.visit_expr(&stmt.array);
            visitor.visit_expr(&stmt.index);
            visitor.visit_expr(&stmt.value);
        }
        Stmt::If(stmt) => {
            visitor.visit_expr(&stmt.cond);
            visitor.visit_block(&stmt.then_block);
            if let Some(block) = &stmt.else_block {
                visitor.visit_block(block);
            }
        }
        Stmt::While(stmt) => {
            visitor.visit_expr(&stmt.cond);
            visitor.visit_block(&stmt.body);
        }
        Stmt::Break(_) | Stmt::Continue(_) => {}
        Stmt::Defer(stmt) => match &stmt.body {
            DeferBody::Expr(expr) => visitor.visit_expr(expr),
            DeferBody::Block(block) => visitor.visit_block(block),
        },
        Stmt::Lock(stmt) => {
            visitor.visit_expr(&stmt.target);
            visitor.enter_scope();
            visitor.bind(&stmt.binding);
            visitor.visit_block(&stmt.body);
            visitor.exit_scope();
        }
        Stmt::For(stmt) => {
            visitor.visit_expr(&stmt.iterable);
            visitor.enter_scope();
            visitor.bind(&stmt.name);
            visitor.visit_block(&stmt.body);
            visitor.exit_scope();
        }
        Stmt::Return(stmt) => {
            if let Some(value) = &stmt.value {
                visitor.visit_expr(value);
            }
        }
        Stmt::Expr(stmt) => visitor.visit_expr(&stmt.expr),
    }
}

/// Walk an expression's children in evaluation order.
///
/// The `match` is exhaustive and has no catch-all arm, so a new [`Expr`] variant
/// is a compile error here rather than a silently dropped subtree.
pub fn walk_expr<V: AstVisitor + ?Sized>(visitor: &mut V, expr: &Expr) {
    match expr {
        Expr::Integer(..)
        | Expr::Float(..)
        | Expr::Bool(..)
        | Expr::String(..)
        | Expr::Var(..)
        | Expr::StaticField(_) => {}
        Expr::Binary(expr) => {
            visitor.visit_expr(&expr.lhs);
            visitor.visit_expr(&expr.rhs);
        }
        Expr::Unary(expr) => visitor.visit_expr(&expr.expr),
        Expr::Call(call) => walk_args(visitor, &call.args),
        Expr::FieldAccess(object, ..) => visitor.visit_expr(object),
        Expr::MethodCall(call) => {
            visitor.visit_expr(&call.object);
            walk_args(visitor, &call.args);
        }
        Expr::StaticCall(call) => walk_args(visitor, &call.args),
        Expr::New(new) => walk_args(visitor, &new.args),
        Expr::ObjectLiteral(object) => {
            for field in &object.fields {
                visitor.visit_expr(&field.value);
            }
        }
        Expr::Await(awaited) => visitor.visit_expr(&awaited.expr),
        Expr::Select(select) => {
            for case in &select.cases {
                visitor.enter_scope();
                match &case.kind {
                    SelectCaseKind::Recv { binding, channel } => {
                        visitor.visit_expr(channel);
                        visitor.bind(binding);
                    }
                    SelectCaseKind::Send { channel, value } => {
                        visitor.visit_expr(channel);
                        visitor.visit_expr(value);
                    }
                    SelectCaseKind::Timeout { millis } => visitor.visit_expr(millis),
                    SelectCaseKind::Join { binding, task } => {
                        visitor.visit_expr(task);
                        visitor.bind(binding);
                    }
                    SelectCaseKind::Default => {}
                }
                visitor.visit_block(&case.body);
                visitor.exit_scope();
            }
        }
        Expr::Print(value, ..) => visitor.visit_expr(value),
        Expr::Ternary(expr) => {
            visitor.visit_expr(&expr.condition);
            visitor.visit_expr(&expr.then_expr);
            visitor.visit_expr(&expr.else_expr);
        }
        Expr::Range(expr) => {
            visitor.visit_expr(&expr.start);
            visitor.visit_expr(&expr.end);
        }
        Expr::Lambda(lambda) => visitor.visit_lambda(lambda),
        Expr::Match(expr) => {
            visitor.visit_expr(&expr.scrutinee);
            for arm in &expr.arms {
                visitor.enter_scope();
                visitor.visit_pattern(&arm.pattern);
                match &arm.body {
                    MatchBody::Expr(body) => visitor.visit_expr(body),
                    MatchBody::Block(body) => visitor.visit_block(body),
                }
                visitor.exit_scope();
            }
        }
        Expr::TryPropagate(inner, _) => visitor.visit_expr(inner),
        Expr::ArrayLiteral(elements, _) => {
            for element in elements {
                visitor.visit_expr(element);
            }
        }
        Expr::Index(array, index, _) => {
            visitor.visit_expr(array);
            visitor.visit_expr(index);
        }
    }
}

/// Walk a lambda: its parameters bind inside one scope around its body.
pub fn walk_lambda<V: AstVisitor + ?Sized>(visitor: &mut V, lambda: &LambdaExpr) {
    visitor.enter_scope();
    for param in &lambda.params {
        visitor.bind(&param.name);
    }
    match &lambda.body {
        LambdaBody::Expr(body) => visitor.visit_expr(body),
        LambdaBody::Block(body) => visitor.visit_block(body),
    }
    visitor.exit_scope();
}

/// Bind every name a pattern introduces. `_` is a hole, not a name.
pub fn walk_pattern<V: AstVisitor + ?Sized>(visitor: &mut V, pattern: &Pattern) {
    match pattern {
        Pattern::Wildcard(_) | Pattern::LiteralBool(..) | Pattern::LiteralInt(..) => {}
        Pattern::Binding { name, .. } => visitor.bind(name),
        Pattern::EnumVariant { .. } => {}
        Pattern::EnumVariantTuple { bindings, .. } => {
            for binding in bindings {
                if binding != "_" {
                    visitor.bind(binding);
                }
            }
        }
        Pattern::ClassDowncast { binding, .. } => {
            if binding != "_" {
                visitor.bind(binding);
            }
        }
    }
}

/// Walk call arguments. `&x` and `x` differ only in the ABI, not in shape.
pub fn walk_args<V: AstVisitor + ?Sized>(visitor: &mut V, args: &[CallArg]) {
    for arg in args {
        visitor.visit_expr(&arg.expr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::Lexer;
    use crate::parser::Parser;

    fn parse(source: &str) -> Program {
        let tokens = Lexer::new(source).tokenize().expect("lex");
        let (program, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        program
    }

    /// The body of the first free function in `source`.
    fn body_of(source: &str, name: &str) -> Block {
        for item in parse(source).items {
            match item {
                Item::Function(function) if function.name == name => return function.body,
                Item::Class(class) => {
                    for method in class.methods {
                        if method.name == name {
                            return method.body;
                        }
                    }
                }
                _ => {}
            }
        }
        panic!("no callable named `{name}`");
    }

    /// Records every `Var` name the walker reaches, in traversal order. A child
    /// slot that the walker forgets shows up as a missing marker.
    #[derive(Default)]
    struct VarTrace {
        seen: Vec<String>,
        skip_lambda_bodies: bool,
    }

    impl AstVisitor for VarTrace {
        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Var(name, _) = expr {
                self.seen.push(name.clone());
            }
            walk_expr(self, expr);
        }

        fn visit_lambda(&mut self, lambda: &LambdaExpr) {
            if !self.skip_lambda_bodies {
                walk_lambda(self, lambda);
            }
        }
    }

    fn vars(source: &str) -> Vec<String> {
        let mut trace = VarTrace::default();
        trace.visit_block(&body_of(source, "f"));
        trace.seen
    }

    /// Records the scope stack as names are bound, so shadowing can be asserted
    /// without any analysis attached.
    #[derive(Default)]
    struct ScopeTrace {
        stack: Vec<Vec<String>>,
        /// `(name, depth-of-scope-holding-it)` for every resolved `Var` read.
        resolved: Vec<(String, Option<usize>)>,
        max_depth: usize,
    }

    impl AstVisitor for ScopeTrace {
        fn enter_scope(&mut self) {
            self.stack.push(Vec::new());
            self.max_depth = self.max_depth.max(self.stack.len());
        }

        fn exit_scope(&mut self) {
            self.stack.pop().expect("balanced scopes");
        }

        fn bind(&mut self, name: &str) {
            self.stack
                .last_mut()
                .expect("a binding needs an open scope")
                .push(name.to_string());
        }

        fn visit_expr(&mut self, expr: &Expr) {
            if let Expr::Var(name, _) = expr {
                let depth = self
                    .stack
                    .iter()
                    .rposition(|scope| scope.iter().any(|bound| bound == name));
                self.resolved.push((name.clone(), depth));
            }
            walk_expr(self, expr);
        }
    }

    fn scope_trace(source: &str) -> ScopeTrace {
        let mut trace = ScopeTrace::default();
        trace.visit_block(&body_of(source, "f"));
        assert!(trace.stack.is_empty(), "scopes must be balanced");
        trace
    }

    /// True when `name` resolved to some open scope at every read.
    fn always_bound(trace: &ScopeTrace, name: &str) -> bool {
        trace
            .resolved
            .iter()
            .filter(|(read, _)| read == name)
            .all(|(_, depth)| depth.is_some())
    }

    // ── Perspective 1-14: every child slot of every `Stmt` is reached ──

    // 1. `let` initializer.
    #[test]
    fn p01_let_initializer_is_visited() {
        assert_eq!(vars("fn f() { let a = init; }"), ["init"]);
    }

    // 2. plain assignment value (the assigned name is not an expression).
    #[test]
    fn p02_assign_value_is_visited() {
        assert_eq!(vars("fn f() { a = value; }"), ["value"]);
    }

    // 3. field assignment visits receiver then value.
    #[test]
    fn p03_field_assign_visits_object_then_value() {
        assert_eq!(
            vars("fn f() { object.field = value; }"),
            ["object", "value"]
        );
    }

    // 4. `super.init(...)` arguments.
    #[test]
    fn p04_super_init_arguments_are_visited() {
        let source = "class C { init(self, x: i64) { super.init(first, second); } }";
        let mut trace = VarTrace::default();
        let Item::Class(class) = parse(source).items.into_iter().next().expect("item") else {
            panic!("expected a class");
        };
        trace.visit_block(&class.constructors[0].body);
        assert_eq!(trace.seen, ["first", "second"]);
    }

    // 5. static property assignment visits only the value.
    #[test]
    fn p05_static_field_assign_visits_value() {
        assert_eq!(vars("fn f() { C::prop = value; }"), ["value"]);
    }

    // 6. index assignment visits array, index, value — in that order.
    #[test]
    fn p06_index_assign_visits_all_three_children() {
        assert_eq!(
            vars("fn f() { array[index] = value; }"),
            ["array", "index", "value"]
        );
    }

    // 7. `if` visits condition, then-block, and the optional else-block.
    #[test]
    fn p07_if_visits_condition_and_both_branches() {
        assert_eq!(
            vars("fn f() { if cond { then_side; } else { else_side; } }"),
            ["cond", "then_side", "else_side"]
        );
    }

    // 8. `while` visits condition and body.
    #[test]
    fn p08_while_visits_condition_and_body() {
        assert_eq!(vars("fn f() { while cond { body; } }"), ["cond", "body"]);
    }

    // 9. `break`/`continue` have no children and must not disturb the walk.
    #[test]
    fn p09_break_and_continue_have_no_children() {
        assert_eq!(vars("fn f() { while cond { break; continue; } }"), ["cond"]);
    }

    // 10. both `defer` bodies are reached — the walker never skips a subtree on
    //     its own; a consumer that wants to skip overrides `visit_stmt`.
    #[test]
    fn p10_defer_expression_and_block_bodies_are_visited() {
        assert_eq!(vars("fn f() { defer g(direct); }"), ["direct"]);
        assert_eq!(vars("fn f() { defer { inside; } }"), ["inside"]);
    }

    // 11. `lock` visits its target and its body.
    #[test]
    fn p11_lock_visits_target_and_body() {
        assert_eq!(
            vars("async fn f() { lock target as cell { body; } }"),
            ["target", "body"]
        );
    }

    // 12. `for` visits the iterable and the body.
    #[test]
    fn p12_for_visits_iterable_and_body() {
        assert_eq!(
            vars("fn f() { for x in iterable { body; } }"),
            ["iterable", "body"]
        );
    }

    // 13. `return` with and without a value.
    #[test]
    fn p13_return_value_is_visited_when_present() {
        assert_eq!(vars("fn f() { return value; }"), ["value"]);
        assert_eq!(vars("fn f() { return; }"), Vec::<String>::new());
    }

    // 14. an expression statement.
    #[test]
    fn p14_expression_statement_is_visited() {
        assert_eq!(vars("fn f() { standalone; }"), ["standalone"]);
    }

    // ── Perspective 15-28: every child slot of every `Expr` is reached ──

    // 15. binary operands, in source order.
    #[test]
    fn p15_binary_visits_both_operands() {
        assert_eq!(vars("fn f() { let a = lhs + rhs; }"), ["lhs", "rhs"]);
    }

    // 16. unary operand.
    #[test]
    fn p16_unary_visits_its_operand() {
        assert_eq!(vars("fn f() { let a = -operand; }"), ["operand"]);
    }

    // 17. free-call arguments, including a `&`-mode argument, which differs only
    //     in ABI and must still be walked.
    #[test]
    fn p17_call_arguments_including_reference_mode_are_visited() {
        assert_eq!(vars("fn f() { g(first, &second); }"), ["first", "second"]);
    }

    // 18. field access visits the receiver.
    #[test]
    fn p18_field_access_visits_its_object() {
        assert_eq!(vars("fn f() { let a = object.field; }"), ["object"]);
    }

    // 19. method call visits receiver then arguments.
    #[test]
    fn p19_method_call_visits_receiver_then_arguments() {
        assert_eq!(
            vars("fn f() { receiver.m(argument); }"),
            ["receiver", "argument"]
        );
    }

    // 20. static call and `new` visit their arguments; the class name is not an
    //     expression in either.
    #[test]
    fn p20_static_call_and_new_visit_arguments() {
        assert_eq!(vars("fn f() { C::m(static_arg); }"), ["static_arg"]);
        assert_eq!(vars("fn f() { let a = new C(new_arg); }"), ["new_arg"]);
    }

    // 21. object literal field values.
    #[test]
    fn p21_object_literal_field_values_are_visited() {
        assert_eq!(
            vars("fn f() { let a = C { x: first, y: second }; }"),
            ["first", "second"]
        );
    }

    // 22. `await` operand.
    #[test]
    fn p22_await_visits_its_operand() {
        assert_eq!(vars("async fn f() { let a = await task; }"), ["task"]);
    }

    // 23. every `select` case kind exposes its expressions AND its body. The
    //     channel/task/timeout expressions are the slots the old hand-written
    //     loop detector dropped.
    #[test]
    fn p23_select_visits_case_expressions_and_bodies() {
        let seen = vars(
            "async fn f() { select { \
                 let v = recv_chan.recv() => { recv_body; } \
                 send_chan.send(sent) => { send_body; } \
                 sleep(delay) => { timeout_body; } \
                 let j = await joined => { join_body; } \
                 default => { default_body; } \
             } }",
        );
        assert_eq!(
            seen,
            [
                "recv_chan",
                "recv_body",
                "send_chan",
                "sent",
                "send_body",
                "delay",
                "timeout_body",
                "joined",
                "join_body",
                "default_body"
            ]
        );
    }

    // 24. `print`/`println` operand.
    #[test]
    fn p24_print_visits_its_operand() {
        assert_eq!(vars("fn f() { println(printed); }"), ["printed"]);
    }

    // 25. ternary visits condition, then, else.
    #[test]
    fn p25_ternary_visits_all_three_arms() {
        assert_eq!(
            vars("fn f() { let a = cond ? yes : no; }"),
            ["cond", "yes", "no"]
        );
    }

    // 26. range endpoints.
    #[test]
    fn p26_range_visits_both_endpoints() {
        assert_eq!(vars("fn f() { for i in start..end { } }"), ["start", "end"]);
    }

    // 27. `match` visits the scrutinee and both body shapes.
    #[test]
    fn p27_match_visits_scrutinee_and_every_arm_body() {
        assert_eq!(
            vars("fn f() { let a = match scrutinee { 1 => expr_body, _ => { block_body; } }; }"),
            ["scrutinee", "expr_body", "block_body"]
        );
    }

    // 28. `?`, array literal, and index all expose their children.
    #[test]
    fn p28_try_array_literal_and_index_visit_their_children() {
        assert_eq!(vars("fn f() { let a = fallible?; }"), ["fallible"]);
        assert_eq!(
            vars("fn f() { let a = [first, second]; }"),
            ["first", "second"]
        );
        assert_eq!(vars("fn f() { let a = array[index]; }"), ["array", "index"]);
    }

    // ── Perspective 29-33: the lambda hook ──

    // 29. by default a lambda body IS walked, so the walker stays structurally
    //     total; skipping is an explicit consumer decision.
    #[test]
    fn p29_lambda_body_is_walked_by_default() {
        assert_eq!(
            vars("fn f() { let g = |x| captured + x; }"),
            ["captured", "x"]
        );
    }

    // 30. overriding `visit_lambda` to do nothing skips exactly the body and
    //     nothing else.
    #[test]
    fn p30_visit_lambda_override_skips_only_the_body() {
        let mut trace = VarTrace {
            skip_lambda_bodies: true,
            ..VarTrace::default()
        };
        trace.visit_block(&body_of(
            "fn f() { before; let g = |x| inside_lambda; after; }",
            "f",
        ));
        assert_eq!(trace.seen, ["before", "after"]);
    }

    // 31. a block-bodied lambda is walked through the same hook.
    #[test]
    fn p31_block_bodied_lambda_uses_the_same_hook() {
        assert_eq!(vars("fn f() { let g = |x| { inside; }; }"), ["inside"]);
    }

    // 32. lambda parameters bind inside the lambda's own scope.
    #[test]
    fn p32_lambda_parameters_bind_inside_the_lambda_scope() {
        let trace = scope_trace("fn f() { let g = |p| p + 1; }");
        assert!(always_bound(&trace, "p"));
    }

    // 33. a lambda parameter is not visible after the lambda ends.
    #[test]
    fn p33_lambda_parameter_does_not_escape_the_lambda() {
        let trace = scope_trace("fn f() { let g = |p| p; let h = p; }");
        let reads: Vec<_> = trace
            .resolved
            .iter()
            .filter(|(name, _)| name == "p")
            .map(|(_, depth)| depth.is_some())
            .collect();
        assert_eq!(reads, [true, false]);
    }

    // ── Perspective 34-42: lexical scope hooks ──

    // 34. scopes are balanced across every construct in one program.
    #[test]
    fn p34_scopes_are_balanced() {
        let trace = scope_trace(
            "async fn f() { \
                 let a = 1; \
                 if a > 0 { let b = 2; } else { let c = 3; } \
                 while a > 0 { let d = 4; } \
                 for e in [1] { let g = 5; } \
                 lock a as cell { let h = 6; } \
                 let i = |j| j; \
                 let k = match a { 1 => 2, _ => 3 }; \
             }",
        );
        // `scope_trace` already asserts the stack drained; this pins that the
        // walk really nested rather than staying flat.
        assert!(trace.max_depth >= 3, "depth was {}", trace.max_depth);
    }

    // 35. a `let` binds only after its own initializer, so a same-named outer
    //     value is still visible on the right-hand side.
    #[test]
    fn p35_let_binds_after_its_initializer() {
        let trace = scope_trace("fn f() { let shadow = shadow + 1; let after = shadow; }");
        let reads: Vec<_> = trace
            .resolved
            .iter()
            .filter(|(name, _)| name == "shadow")
            .map(|(_, depth)| depth.is_some())
            .collect();
        assert_eq!(reads, [false, true]);
    }

    // 36. a `for` binding is visible in the body and gone afterwards.
    #[test]
    fn p36_for_binding_is_scoped_to_its_body() {
        let trace = scope_trace("fn f() { for item in [1] { let a = item; } let b = item; }");
        let reads: Vec<_> = trace
            .resolved
            .iter()
            .filter(|(name, _)| name == "item")
            .map(|(_, depth)| depth.is_some())
            .collect();
        assert_eq!(reads, [true, false]);
    }

    // 37. a `for` binding is not visible in its own iterable.
    #[test]
    fn p37_for_binding_is_not_visible_in_the_iterable() {
        let trace = scope_trace("fn f() { for item in item { } }");
        assert_eq!(trace.resolved, [("item".to_string(), None)]);
    }

    // 38. a `lock` binding is scoped to the critical section.
    #[test]
    fn p38_lock_binding_is_scoped_to_the_body() {
        let trace =
            scope_trace("async fn f() { lock target as cell { let a = cell; } let b = cell; }");
        let reads: Vec<_> = trace
            .resolved
            .iter()
            .filter(|(name, _)| name == "cell")
            .map(|(_, depth)| depth.is_some())
            .collect();
        assert_eq!(reads, [true, false]);
    }

    // 39. a block-local binding does not leak to its parent.
    #[test]
    fn p39_block_local_binding_does_not_leak() {
        let trace = scope_trace("fn f() { if c { let inner = 1; let a = inner; } let b = inner; }");
        let reads: Vec<_> = trace
            .resolved
            .iter()
            .filter(|(name, _)| name == "inner")
            .map(|(_, depth)| depth.is_some())
            .collect();
        assert_eq!(reads, [true, false]);
    }

    // 40. a `match` arm's tuple bindings are visible in that arm only.
    #[test]
    fn p40_match_tuple_bindings_are_scoped_to_their_arm() {
        let trace = scope_trace(
            "fn f() { let a = match s { Shape::Circle(radius) => radius, _ => radius }; }",
        );
        let reads: Vec<_> = trace
            .resolved
            .iter()
            .filter(|(name, _)| name == "radius")
            .map(|(_, depth)| depth.is_some())
            .collect();
        assert_eq!(reads, [true, false]);
    }

    // 41. a class-downcast pattern binds its name; `_` binds nothing.
    #[test]
    fn p41_class_downcast_binds_its_name_but_not_underscore() {
        let bound = scope_trace("fn f() { let a = match s { Dog(d) => d, _ => 0 }; }");
        assert!(always_bound(&bound, "d"));

        let hole = scope_trace("fn f() { let a = match s { Dog(_) => outer, _ => 0 }; }");
        assert_eq!(
            hole.resolved,
            [("s".to_string(), None), ("outer".to_string(), None)]
        );
    }

    // 42. `select` receive and join bindings are scoped to their case body.
    #[test]
    fn p42_select_case_bindings_are_scoped_to_their_case() {
        let trace = scope_trace(
            "async fn f() { \
                 select { \
                     let v = ch.recv() => { let a = v; } \
                     let j = await t => { let b = j; } \
                     default => { let c = v; } \
                 } \
             }",
        );
        let v_reads: Vec<_> = trace
            .resolved
            .iter()
            .filter(|(name, _)| name == "v")
            .map(|(_, depth)| depth.is_some())
            .collect();
        assert_eq!(v_reads, [true, false]);
        assert!(always_bound(&trace, "j"));
    }

    // ── Perspective 43-47: the traversal contract itself ──

    // 43. work can run strictly before children.
    #[test]
    fn p43_pre_order_work_runs_before_children() {
        #[derive(Default)]
        struct PreOrder(Vec<String>);
        impl AstVisitor for PreOrder {
            fn visit_expr(&mut self, expr: &Expr) {
                match expr {
                    Expr::Binary(_) => self.0.push("binary".to_string()),
                    Expr::Var(name, _) => self.0.push(name.clone()),
                    _ => {}
                }
                walk_expr(self, expr);
            }
        }
        let mut visitor = PreOrder::default();
        visitor.visit_block(&body_of("fn f() { let a = lhs + rhs; }", "f"));
        assert_eq!(visitor.0, ["binary", "lhs", "rhs"]);
    }

    // 44. work can run strictly after children — the shape the panic-effect
    //     analysis needs, since it classifies a node only once its operands
    //     have been accounted for.
    #[test]
    fn p44_post_order_work_runs_after_children() {
        #[derive(Default)]
        struct PostOrder(Vec<String>);
        impl AstVisitor for PostOrder {
            fn visit_expr(&mut self, expr: &Expr) {
                walk_expr(self, expr);
                match expr {
                    Expr::Binary(_) => self.0.push("binary".to_string()),
                    Expr::Var(name, _) => self.0.push(name.clone()),
                    _ => {}
                }
            }
        }
        let mut visitor = PostOrder::default();
        visitor.visit_block(&body_of("fn f() { let a = lhs + rhs; }", "f"));
        assert_eq!(visitor.0, ["lhs", "rhs", "binary"]);
    }

    // 45. a consumer can prune any subtree by not calling `walk_*`.
    #[test]
    fn p45_a_subtree_can_be_pruned() {
        #[derive(Default)]
        struct SkipElse(Vec<String>);
        impl AstVisitor for SkipElse {
            fn visit_stmt(&mut self, stmt: &Stmt) {
                if let Stmt::If(if_stmt) = stmt {
                    self.visit_expr(&if_stmt.cond);
                    self.visit_block(&if_stmt.then_block);
                    return;
                }
                walk_stmt(self, stmt);
            }
            fn visit_expr(&mut self, expr: &Expr) {
                if let Expr::Var(name, _) = expr {
                    self.0.push(name.clone());
                }
                walk_expr(self, expr);
            }
        }
        let mut visitor = SkipElse::default();
        visitor.visit_block(&body_of(
            "fn f() { if cond { kept; } else { pruned; } }",
            "f",
        ));
        assert_eq!(visitor.0, ["cond", "kept"]);
    }

    // 46. a visitor that overrides nothing is a no-op that still terminates over
    //     deeply nested syntax.
    #[test]
    fn p46_default_visitor_is_a_terminating_no_op() {
        struct Nothing;
        impl AstVisitor for Nothing {}
        Nothing.visit_block(&body_of(
            "fn f() { while a { if b { for c in d { let e = |g| { h(i[j]); }; } } } }",
            "f",
        ));
    }

    // 47. the walker is usable behind a trait object, so a consumer can hold a
    //     boxed visitor without duplicating the traversal per concrete type.
    #[test]
    fn p47_walker_works_through_a_trait_object() {
        #[derive(Default)]
        struct Counter(usize);
        impl AstVisitor for Counter {
            fn visit_expr(&mut self, expr: &Expr) {
                self.0 += 1;
                walk_expr(self, expr);
            }
        }
        let mut boxed: Box<dyn AstVisitor> = Box::new(Counter::default());
        boxed.visit_block(&body_of("fn f() { let a = lhs + rhs; }", "f"));
    }

    // ── Perspective 48-50: totality against future syntax ──

    // 48. one program touching every statement and expression form the parser
    //     can produce, with a distinct marker in every child slot. A child slot
    //     dropped from `walk_stmt`/`walk_expr` fails here even though the
    //     `match` arms still compile.
    #[test]
    fn p48_every_child_slot_of_every_node_is_reached() {
        let source = "\
open class Marker {
    field: i64;
    fn m(self, p: i64) -> i64 { return p; }
}

async fn f(param: i64) -> i64 {
    let let_init = m_let;
    assigned = m_assign;
    obj.field = m_field_value;
    C::prop = m_static_value;
    arr[m_index] = m_index_value;
    if m_if_cond { m_then; } else { m_else; }
    while m_while_cond { m_while_body; break; continue; }
    for iterated in m_iterable { m_for_body; }
    defer g(m_defer_expr);
    defer { m_defer_block; }
    lock m_lock_target as cell { m_lock_body; }
    let binary = m_lhs + m_rhs;
    let unary = -m_unary;
    call(m_call_arg, &m_ref_arg);
    let field_access = m_access_object.field;
    m_receiver.method(m_method_arg);
    C::stat(m_static_arg);
    let created = new C(m_new_arg);
    let literal = C { x: m_literal_value };
    let awaited = await m_awaited;
    select {
        let v = m_recv_channel.recv() => { m_recv_body; }
        m_send_channel.send(m_sent) => { m_send_body; }
        sleep(m_delay) => { m_timeout_body; }
        let j = await m_joined => { m_join_body; }
        default => { m_default_body; }
    }
    println(m_printed);
    let ternary = m_cond ? m_then_expr : m_else_expr;
    let ranged = m_range_start..m_range_end;
    let lambda = |lp| m_lambda_body;
    let matched = match m_scrutinee { 1 => m_arm_expr, _ => { m_arm_block; } };
    let propagated = m_fallible?;
    let array = [m_element];
    let indexed = m_index_array[m_index_index];
    return m_returned;
}
";
        let seen = vars(source);
        let expected = [
            "m_let",
            "m_assign",
            "obj",
            "m_field_value",
            "m_static_value",
            "arr",
            "m_index",
            "m_index_value",
            "m_if_cond",
            "m_then",
            "m_else",
            "m_while_cond",
            "m_while_body",
            "m_iterable",
            "m_for_body",
            "m_defer_expr",
            "m_defer_block",
            "m_lock_target",
            "m_lock_body",
            "m_lhs",
            "m_rhs",
            "m_unary",
            "m_call_arg",
            "m_ref_arg",
            "m_access_object",
            "m_receiver",
            "m_method_arg",
            "m_static_arg",
            "m_new_arg",
            "m_literal_value",
            "m_awaited",
            "m_recv_channel",
            "m_recv_body",
            "m_send_channel",
            "m_sent",
            "m_send_body",
            "m_delay",
            "m_timeout_body",
            "m_joined",
            "m_join_body",
            "m_default_body",
            "m_printed",
            "m_cond",
            "m_then_expr",
            "m_else_expr",
            "m_range_start",
            "m_range_end",
            "m_lambda_body",
            "m_scrutinee",
            "m_arm_expr",
            "m_arm_block",
            "m_fallible",
            "m_element",
            "m_index_array",
            "m_index_index",
            "m_returned",
        ];
        assert_eq!(seen, expected);
    }

    // 49. the same program leaves the scope stack empty, so no construct opens a
    //     scope it forgets to close.
    #[test]
    fn p49_a_program_using_every_construct_balances_its_scopes() {
        let trace = scope_trace(
            "async fn f() { \
                 for a in [1] { let b = a; } \
                 lock t as c { let d = c; } \
                 let e = |g| g; \
                 let h = match x { Dog(i) => i, Shape::Circle(j) => j, _ => 0 }; \
                 select { let v = ch.recv() => { let k = v; } default => { } } \
             }",
        );
        assert!(trace.stack.is_empty());
        for name in ["a", "c", "g", "i", "j", "v"] {
            assert!(always_bound(&trace, name), "`{name}` never resolved");
        }
    }

    // 50. an empty body and an empty `select` are handled without panicking or
    //     unbalancing the scope stack.
    #[test]
    fn p50_empty_bodies_are_handled() {
        assert_eq!(vars("fn f() { }"), Vec::<String>::new());
        let trace = scope_trace("async fn f() { select { } }");
        assert!(trace.resolved.is_empty());
        assert!(trace.stack.is_empty());
    }
}
