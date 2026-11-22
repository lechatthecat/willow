use super::*;
use crate::lexer::Lexer;
use crate::parser::Parser;
use crate::semantic::scopes::Scopes;

fn parse(source: &str) -> Program {
    let tokens = Lexer::new(source).tokenize().expect("lex");
    let (program, errors) = Parser::new(tokens).parse();
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    program
}

/// The body of the named function or method.
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

/// A missing child slot appears as a missing variable name.
fn block_vars(body: &Block) -> Vec<String> {
    AstWalk::new(AstEvent::Block(body))
        .filter_map(|event| match event {
            AstEvent::Expr(Expr::Var(name, ..)) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

fn vars(source: &str) -> Vec<String> {
    block_vars(&body_of(source, "f"))
}

#[derive(Default)]
struct ScopeTrace {
    /// The innermost frame containing the name at each variable read.
    resolved: Vec<(String, Option<usize>)>,
    max_depth: usize,
}

fn scope_trace(source: &str) -> ScopeTrace {
    let body = body_of(source, "f");
    let mut trace = ScopeTrace::default();
    let mut scopes = Scopes::default();
    for event in AstWalk::new(AstEvent::Block(&body)) {
        match event {
            AstEvent::EnterScope => {
                scopes.push();
                trace.max_depth = trace.max_depth.max(scopes.len());
            }
            AstEvent::ExitScope => {
                assert!(scopes.len() > 0, "unmatched scope exit: {source}");
                scopes.pop();
            }
            AstEvent::Bind(name) => {
                assert_ne!(name, "_", "wildcards cannot bind");
                let depth = scopes.len().checked_sub(1).expect("an open scope");
                scopes.insert(name.to_owned(), depth);
            }
            AstEvent::Expr(Expr::Var(name, ..)) => {
                trace
                    .resolved
                    .push((name.clone(), scopes.get(name).copied()));
            }
            _ => {}
        }
    }
    assert_eq!(scopes.len(), 0, "unclosed scope: {source}");
    trace
}

/// True when `name` resolved to some open scope at every read.
fn always_bound(trace: &ScopeTrace, name: &str) -> bool {
    let mut reads = trace
        .resolved
        .iter()
        .filter(|(read, _)| read == name)
        .peekable();
    reads.peek().is_some() && reads.all(|(_, depth)| depth.is_some())
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
    let Item::Class(class) = parse(source).items.into_iter().next().expect("item") else {
        panic!("expected a class");
    };
    assert_eq!(block_vars(&class.constructors[0].body), ["first", "second"]);
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
//     its own; a consumer can prune the Defer statement.
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

// ── Perspective 29-33: lambda events ──

// 29. by default a lambda body IS walked, so the walker stays structurally
//     total; skipping is an explicit consumer decision.
#[test]
fn p29_lambda_body_is_walked_by_default() {
    assert_eq!(
        vars("fn f() { let g = |x| captured + x; }"),
        ["captured", "x"]
    );
}

// 30. pruning a lambda skips its body and continues with its siblings.
#[test]
fn p30_pruning_a_lambda_skips_only_the_body() {
    let body = body_of("fn f() { before; let g = |x| inside_lambda; after; }", "f");
    let mut walk = AstWalk::new(AstEvent::Block(&body));
    let mut seen = Vec::new();
    while let Some(event) = walk.next() {
        match event {
            AstEvent::Lambda(_) => walk.skip_children(),
            AstEvent::Expr(Expr::Var(name, ..)) => seen.push(name.as_str()),
            _ => {}
        }
    }
    assert_eq!(seen, ["before", "after"]);
}

// 31. block-bodied lambdas also expose their children.
#[test]
fn p31_block_bodied_lambda_exposes_its_body() {
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

// ── Perspective 34-42: lexical scope events ──

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
    assert_eq!(trace.max_depth, 3);
}

#[test]
fn scopes_and_bindings_follow_source_order() {
    let body = body_of(
        "fn f() { let a = |b| { let c = b; }; for x in a { x; } }",
        "f",
    );
    let events: Vec<_> = AstWalk::new(AstEvent::Block(&body))
        .filter_map(|event| match event {
            AstEvent::EnterScope => Some("{"),
            AstEvent::ExitScope => Some("}"),
            AstEvent::Bind(name) => Some(name),
            _ => None,
        })
        .collect();
    assert_eq!(
        events,
        [
            "{", "{", "b", "{", "c", "}", "}", "a", "{", "x", "{", "}", "}", "}"
        ]
    );
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
    let trace = scope_trace("async fn f() { lock target as cell { let a = cell; } let b = cell; }");
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
    let trace =
        scope_trace("fn f() { let a = match s { Shape::Circle(radius) => radius, _ => radius }; }");
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

fn expr_label(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Binary(_) => Some("binary"),
        Expr::Var(name, ..) => Some(name),
        _ => None,
    }
}

#[test]
fn p43_pre_order_work_runs_before_children() {
    let body = body_of("fn f() { let a = lhs + rhs; }", "f");
    let order: Vec<_> = AstWalk::new(AstEvent::Block(&body))
        .filter_map(|event| match event {
            AstEvent::Expr(expr) => expr_label(expr),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["binary", "lhs", "rhs"]);
}

#[test]
fn p44_post_order_work_runs_after_children() {
    let body = body_of("fn f() { let a = lhs + rhs; }", "f");
    let order: Vec<_> = AstWalk::new(AstEvent::Block(&body))
        .filter_map(|event| match event {
            AstEvent::ExitExpr(expr) => expr_label(expr),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["lhs", "rhs", "binary"]);
}

#[test]
fn p45_a_subtree_can_be_pruned() {
    let body = body_of("fn f() { if cond { kept; } else { pruned; } after; }", "f");
    let mut walk = AstWalk::new(AstEvent::Block(&body));
    let mut seen = Vec::new();
    while let Some(event) = walk.next() {
        match event {
            AstEvent::Stmt(Stmt::If(branch)) => {
                walk.skip_children();
                walk.push(AstEvent::Block(&branch.then_block));
                walk.push(AstEvent::Expr(&branch.cond));
            }
            AstEvent::Expr(Expr::Var(name, ..)) => seen.push(name.as_str()),
            _ => {}
        }
    }
    assert_eq!(seen, ["cond", "kept", "after"]);
}

#[test]
fn p46_walk_terminates_with_a_no_op_consumer() {
    let body = body_of(
        "fn f() { while a { if b { for c in d { let e = |g| { h(i[j]); }; } } } }",
        "f",
    );
    let mut walk = AstWalk::new(AstEvent::Block(&body));
    walk.by_ref().for_each(|_| {});
    assert!(walk.next().is_none());
}

#[test]
fn p47_walker_works_through_a_trait_object() {
    let body = body_of("fn f() { let a = lhs + rhs; }", "f");
    let boxed: Box<dyn Iterator<Item = AstEvent<'_>>> =
        Box::new(AstWalk::new(AstEvent::Block(&body)));
    assert_eq!(
        boxed
            .filter(|event| matches!(event, AstEvent::Expr(_)))
            .count(),
        3
    );
}

// ── Perspective 48-50: totality against future syntax ──

// 48. one program touching every statement and expression form the parser
//     can produce, with a distinct marker in every child slot. A child slot
//     omitted by the walker fails here even though the
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
}
