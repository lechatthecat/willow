use super::*;
use crate::diagnostics::ErrorCode;

fn check(body: &str, expected_missing: &[&str]) {
    check_items("", body, expected_missing);
}

/// `check` with extra top-level items (helpers, classes) beside `C`.
fn check_items(items: &str, body: &str, expected_missing: &[&str]) {
    let source = format!(
        "{items} class C {{ x: i64; y: i64; init(self, flag: bool) {{ {body} }} }} fn main() {{}}"
    );
    let errors = check_source(&source);
    let mut missing = Vec::new();
    for error in &errors {
        if body.contains("let f =") && error.code == ErrorCode::E1002 {
            continue;
        }
        if body.contains("defer println(match") && error.code == ErrorCode::E0905 {
            continue;
        }
        assert_eq!(error.code, ErrorCode::E0842, "{body}: {errors:?}");
        for name in ["x", "y"] {
            if error.message.contains(&format!("field `{name}`")) {
                missing.push(name);
            }
        }
    }
    assert_eq!(missing, expected_missing, "{body}: {errors:?}");
}

#[test]
fn constructor_flow_branch_and_exit_perspectives() {
    let cases: &[(&str, &[&str])] = &[
        ("self.x = 1; self.y = 2;", &[]),
        ("if flag { self.x = 1; } self.y = 2;", &["x"]),
        (
            "if flag { self.x = 1; } else { self.x = 2; } self.y = 2;",
            &[],
        ),
        ("if flag { self.x = 1; } else { self.y = 2; }", &["x", "y"]),
        (
            "self.x = 1; if flag { self.y = 2; } else { self.y = 3; }",
            &[],
        ),
        ("if flag { if flag { self.x = 1; } } self.y = 2;", &["x"]),
        ("if flag { return; } self.x = 1; self.y = 2;", &["x", "y"]),
        ("self.x = 1; if flag { return; } self.y = 2;", &["y"]),
        (
            "if flag { self.x = 1; self.y = 2; return; } self.x = 3; self.y = 4;",
            &[],
        ),
        ("if flag { panic(\"bad\"); } self.x = 1; self.y = 2;", &[]),
        (
            "if flag { self.x = 1; self.y = 2; } else { panic(\"bad\"); }",
            &[],
        ),
        ("panic(\"bad\");", &[]),
        ("return; self.x = 1; self.y = 2;", &["x", "y"]),
        ("defer { self.x = 1; } self.y = 2;", &["x"]),
        ("let f = || { self.x = 1; }; self.y = 2;", &["x"]),
        ("if true { self.x = 1; self.y = 2; }", &[]),
        ("if false { self.x = 1; self.y = 2; }", &["x", "y"]),
        ("self.x = 1; self.x = 2; self.y = 3;", &[]),
        (
            "if flag { panic(\"bad\"); } else { return; } self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        (
            "self.x = match flag { true => { return; }, false => 1 }; self.y = 2;",
            &["x", "y"],
        ),
    ];
    for (body, missing) in cases {
        check(body, missing);
    }
}

#[test]
fn constructor_flow_loop_perspectives() {
    let cases: &[(&str, &[&str])] = &[
        ("while flag { self.x = 1; } self.y = 2;", &["x"]),
        ("for i in 0..1 { self.x = i; } self.y = 2;", &["x"]),
        ("while true { self.x = 1; self.y = 2; break; }", &[]),
        (
            "while true { if flag { break; } self.x = 1; self.y = 2; }",
            &["x", "y"],
        ),
        (
            "while true { if flag { self.x = 1; self.y = 2; break; } continue; }",
            &[],
        ),
        ("while true { continue; self.x = 1; }", &[]),
        ("while true { }", &[]),
        (
            "while true { while true { break; } self.x = 1; self.y = 2; break; }",
            &[],
        ),
        (
            "while flag { return; } self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        ("while false { return; } self.x = 1; self.y = 2;", &[]),
        (
            "for i in 0..1 { if flag { return; } } self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        (
            "while true { match flag { true => { break; }, false => { self.x = 1; } }; } self.y = 2;",
            &["x"],
        ),
        (
            "while true { self.x = 1; while flag { continue; } self.y = 2; break; }",
            &[],
        ),
    ];
    for (body, missing) in cases {
        check(body, missing);
    }
}

#[test]
fn constructor_flow_expression_perspectives() {
    let cases: &[(&str, &[&str])] = &[
        (
            "match flag { true => { self.x = 1; }, false => { self.x = 2; } }; self.y = 2;",
            &[],
        ),
        (
            "match flag { true => { self.x = 1; }, false => {} }; self.y = 2;",
            &["x"],
        ),
        (
            "match flag { true => { self.x = 1; self.y = 2; }, false => { panic(\"bad\"); } };",
            &[],
        ),
        (
            "let a = flag && match flag { true => { self.x = 1; return; }, false => false }; self.x = 2; self.y = 2;",
            &["y"],
        ),
        (
            "let a = false && match flag { true => { return; }, false => false }; self.x = 2; self.y = 2;",
            &[],
        ),
        (
            "let a = true || match flag { true => { return; }, false => false }; self.x = 2; self.y = 2;",
            &[],
        ),
        ("self.x = flag ? 1 : 2; self.y = 2;", &[]),
        (
            "println(match flag { true => { self.x = 1; return; }, false => 1 }); self.x = 2; self.y = 2;",
            &["y"],
        ),
        (
            "defer println(match flag { true => { return; }, false => 1 }); self.x = 2; self.y = 2;",
            &["x", "y"],
        ),
        ("defer panic(\"later\");", &["x", "y"]),
        ("self.x = panic(\"bad\");", &[]),
    ];
    for (body, missing) in cases {
        check(body, missing);
    }
}

#[test]
fn constructor_flow_try_propagation_stays_invalid() {
    for (value, expected) in [
        ("Option::Some(1)", ErrorCode::E1807),
        ("Result::Ok(1)", ErrorCode::E1807),
    ] {
        let source =
            format!("class C {{ x: i64; init(self) {{ self.x = {value}?; }} }} fn main() {{}}");
        let errors = check_source(&source);
        assert!(errors.iter().any(|e| e.code == expected), "{errors:?}");
    }
}

#[test]
fn constructor_flow_other_objects_static_fields_and_overloads() {
    let sources = [
        (
            "class C { pub x: i64; init(self, other: C) { other.x = 1; } } fn main() {}",
            1,
        ),
        (
            "class C { static x: i64 = 1; init(self) {} } fn main() {}",
            0,
        ),
        (
            "class C { x: i64; init(self) { self.x = 1; } init(self, flag: bool) { if flag { self.x = 2; } } } fn main() {}",
            1,
        ),
        (
            "fn panic(s: String) {} class C { x: i64; init(self) { panic(\"returns\"); } } fn main() {}",
            1,
        ),
    ];
    for (source, expected) in sources {
        let errors = check_source(source);
        assert_eq!(
            errors.iter().filter(|e| e.code == ErrorCode::E0842).count(),
            expected,
            "{errors:?}"
        );
    }
}

fn check_source(source: &str) -> Vec<crate::diagnostics::Diagnostic> {
    let tokens = crate::lexer::Lexer::new(source).tokenize().expect("lex");
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = crate::semantic::TypeChecker::new();
    crate::register_prelude(&mut checker).expect("prelude");
    checker.check_program(&program);
    checker.errors
}

fn parse_body(source: &str) -> Block {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    for item in program.items {
        if let Item::Class(c) = item {
            return c.constructors.into_iter().next().unwrap().body;
        }
    }
    panic!("missing constructor")
}

fn measure(source: &str, names: &[String]) -> (Vec<bool>, Counts) {
    measure_with(source, names, &HashMap::new(), &every_call_may_panic)
}

fn measure_with(
    source: &str,
    names: &[String],
    types: &HashMap<ExprId, Type>,
    call_may_panic: CallMayPanic<'_>,
) -> (Vec<bool>, Counts) {
    let body = parse_body(source);
    let fields = names
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    let mut graph = Graph::new(&fields, types, call_may_panic);
    let entry = graph.schedule(Input::Block(&body.stmts), EXIT, Flow::ROOT);
    graph.build();
    let (state, counts) = graph.solve(entry);
    let mut missing = vec![false; names.len()];
    if let Some(state) = state {
        state.write_missing(&mut missing);
    }
    (missing, counts)
}

#[test]
fn constructor_flow_scaling_counts() {
    println!("shape,size,fields,nodes,edges,assignment_nodes,join_nodes,allocated_nodes");
    for size in [64_usize, 256, 1024] {
        let names: Vec<_> = (0..size).map(|i| format!("field_{i}")).collect();
        let declarations = names
            .iter()
            .map(|s| format!("{s}: i64;"))
            .collect::<String>();
        for shape in [
            "linear", "branches", "exits", "fanout", "repeated", "deep", "sparse",
        ] {
            let mut body = String::new();
            match shape {
                "linear" => {
                    for name in &names {
                        body.push_str(&format!("self.{name} = 1;"));
                    }
                }
                "branches" => {
                    for name in &names {
                        body.push_str(&format!(
                            "if flag {{ self.{name} = 1; }} else {{ self.{name} = 2; }}"
                        ));
                    }
                }
                "exits" => {
                    for name in &names {
                        body.push_str(&format!("if flag {{ self.{name} = 1; return; }}"));
                    }
                }
                "fanout" => {
                    body.push_str("match n {");
                    for (i, name) in names.iter().enumerate() {
                        body.push_str(&format!("{i} => {{ self.{name} = 1; }},"));
                    }
                    body.push_str("_ => {} };");
                }
                "repeated" => {
                    for name in &names {
                        body.push_str(&format!("self.{name} = 1;"));
                    }
                    for _ in 0..size {
                        body.push_str("if flag { self.field_0 = 2; } else { self.field_0 = 3; }");
                    }
                }
                "deep" => {
                    for _ in 0..size {
                        body.push_str("if flag {");
                    }
                    body.push_str("self.field_0 = 1;");
                    for _ in 0..size {
                        body.push('}');
                    }
                }
                "sparse" => {
                    for _ in 0..64 {
                        body.push_str("if flag { self.field_0 = 1; } else { self.field_0 = 2; }");
                    }
                }
                _ => unreachable!(),
            }
            let source =
                format!("class C {{ {declarations} init(self, flag: bool, n: i64) {{ {body} }} }}");
            let (missing, c) = measure(&source, &names);
            let expected = matches!(shape, "exits" | "fanout");
            if shape == "deep" {
                assert!(missing.iter().all(|&m| m));
            } else if shape == "sparse" {
                assert!(!missing[0]);
                assert!(missing[1..].iter().all(|&m| m));
            } else {
                assert!(missing.iter().all(|&m| m == expected));
            }
            let log = size.ilog2() as usize + 1;
            assert!(c.nodes <= 35 * size + 10, "{shape}: {c:?}");
            assert!(c.assignment_nodes <= 5 * size * log, "{shape}: {c:?}");
            assert!(c.join_nodes <= 8 * size * log, "{shape}: {c:?}");
            assert!(c.allocated_nodes <= 8 * size * log, "{shape}: {c:?}");
            println!(
                "{shape},{size},{size},{},{},{},{},{}",
                c.nodes, c.edges, c.assignment_nodes, c.join_nodes, c.allocated_nodes
            );
        }
    }
}

#[test]
fn constructor_flow_deep_branches_use_small_native_stack() {
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(|| {
            let span = crate::diagnostics::Span::new(0, 0, 1, 1);
            let mut body = Block {
                id: crate::parser::ast::BodyId::fresh(),
                stmts: vec![],
                span,
            };
            for _ in 0..50_000 {
                body = Block {
                    id: crate::parser::ast::BodyId::fresh(),
                    stmts: vec![Stmt::If(IfStmt {
                        cond: Expr::Var("flag".into(), span, ExprId::fresh()),
                        then_block: body,
                        else_block: None,
                        span,
                    })],
                    span,
                };
            }
            let types = HashMap::new();
            assert_eq!(
                uninitialized_fields(&body, ["x"].into_iter(), &types, &every_call_may_panic),
                [true]
            );
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn constructor_flow_persistent_state_matches_dense_sets() {
    // Deterministic independent dense oracle, including odd-sized field trees,
    // shared states, idempotent assignments and many non-identical joins.
    for fields in [1, 3, 63, 64, 65, 257] {
        let mut states = vec![(State::Missing, vec![true; fields])];
        let mut seed = 17_u64;
        let mut counts = Counts::default();
        for step in 0..1200 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let a = seed as usize % states.len();
            let (mut state, mut dense) = states[a].clone();
            let field = (seed >> 32) as usize % fields;
            state.assign(field, fields, &mut counts);
            dense[field] = false;
            if step % 3 == 0 {
                let b = (seed >> 16) as usize % states.len();
                state.join(states[b].0.clone(), &mut counts);
                for (x, y) in dense.iter_mut().zip(&states[b].1) {
                    *x |= *y;
                }
            }
            let mut actual = vec![false; fields];
            state.write_missing(&mut actual);
            assert_eq!(actual, dense);
            states.push((state, dense));
        }
    }
}

#[test]
fn constructor_flow_recovery_exits_require_initialization() {
    let recovery = "defer match recover() { Some(_) => {}, None => {} };";
    let cases: &[(&str, &[&str])] = &[
        ("RECOVER panic(\"bad\");", &["x", "y"]),
        ("self.x = 1; RECOVER panic(\"bad\");", &["y"]),
        ("self.x = 1; self.y = 2; RECOVER panic(\"bad\");", &[]),
        (
            "if flag { RECOVER panic(\"bad\"); } self.x = 1; self.y = 2;",
            &[],
        ),
        (
            "RECOVER if flag { panic(\"bad\"); } self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        ("panic(\"before registration\"); RECOVER", &[]),
        ("RECOVER self.x = 1; self.y = 2;", &[]),
        (
            "RECOVER let n = 1 / 0; self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        ("RECOVER while true { break; } self.x = 1; self.y = 2;", &[]),
    ];
    for (body, expected) in cases {
        check(&body.replace("RECOVER", recovery), expected);
    }
}

/// willow-ssl7.19: calls and cleanups consult checker-owned panic facts, and
/// only a direct `recover()` creates a recovery exit.
#[test]
fn constructor_flow_effect_refinement_perspectives() {
    let recovery = "defer match recover() { Some(_) => {}, None => {} };";
    let helpers = r#"
fn pure(n: i64) -> bool { return n > 0; }
fn tidy() { println("tidy"); }
fn calm(n: i64) -> i64 { return n; }
fn boom() { panic("boom"); }
fn chain_ok() { chain_mid(); }
fn chain_mid() { let n = calm(1); }
fn chain_bad() { chain_bad_mid(); }
fn chain_bad_mid() { boom(); }
fn ping(go: bool) { if go { pong(false); } }
fn pong(go: bool) { if go { ping(false); } }
fn spin(go: bool) { if go { spun(false); } }
fn spun(go: bool) { if go { spin(false); } else { boom(); } }
fn rescue() { defer match recover() { Some(_) => {}, None => {} }; }
fn grow(n: i64) -> i64 { return n + 1; }
class Fixed { pub init(self) {} pub fn ok(self) -> bool { return true; } pub fn bad(self) { panic("bad"); } pub static fn make() -> bool { return true; } }
open class Base { pub init(self) {} pub open fn ok(self) -> bool { return true; } pub fn sealed(self) -> bool { return true; } }
interface Probe { fn ok(self) -> bool; }
"#;
    let cases: &[(&str, &[&str])] = &[
        // 1-3: no direct recover() means no recovery exit at all.
        ("defer println(\"cleanup\"); panic(\"stop\");", &[]),
        ("defer tidy(); panic(\"stop\");", &[]),
        ("defer boom(); panic(\"stop\");", &[]),
        // 4: a helper's recover() is never eligible for this unwinding.
        ("defer rescue(); panic(\"stop\");", &[]),
        // 5: a non-recovering block cleanup can still not publish fields.
        ("defer { tidy(); println(1); } panic(\"stop\");", &[]),
        // 6: direct recovery keeps rejecting a resumable panic.
        ("RECOVER panic(\"stop\");", &["x", "y"]),
        // 7-8: proven pure calls do not reach the recovery exit.
        ("RECOVER let ok = pure(1); self.x = 1; self.y = 2;", &[]),
        ("RECOVER let n = calm(1); self.x = 1; self.y = 2;", &[]),
        // Helper display shares the solver's conservative Print summary.
        ("RECOVER tidy(); self.x = 1; self.y = 2;", &["x", "y"]),
        // 9-10: panicking helpers and checked arithmetic still do.
        ("RECOVER boom(); self.x = 1; self.y = 2;", &["x", "y"]),
        (
            "RECOVER let n = grow(1); self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        // 11-12: transitive call chains.
        ("RECOVER chain_ok(); self.x = 1; self.y = 2;", &[]),
        ("RECOVER chain_bad(); self.x = 1; self.y = 2;", &["x", "y"]),
        // 13-14: recursive SCCs, pure and panicking.
        ("RECOVER ping(true); self.x = 1; self.y = 2;", &[]),
        ("RECOVER spin(true); self.x = 1; self.y = 2;", &["x", "y"]),
        // 15-17: methods of a non-open class and static methods resolve.
        (
            "let f = new Fixed(); RECOVER let ok = f.ok(); self.x = 1; self.y = 2;",
            &[],
        ),
        (
            "let f = new Fixed(); RECOVER f.bad(); self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        (
            "RECOVER let ok = Fixed::make(); self.x = 1; self.y = 2;",
            &[],
        ),
        // 18: constructor calls use the callee's init facts.
        ("RECOVER let f = new Fixed(); self.x = 1; self.y = 2;", &[]),
        // 19-20: overridable dispatch stays conservative; a non-open method
        // of an open class cannot be overridden.
        (
            "let b = new Base(); RECOVER let ok = b.ok(); self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        (
            "let b = new Base(); RECOVER let ok = b.sealed(); self.x = 1; self.y = 2;",
            &[],
        ),
        // 21: indirect lambda calls stay conservative.
        (
            "let g = || { }; RECOVER g(); self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        // 22-23: scalar display is not a hazard (E1402 rejects any other
        // operand); a panicking operand still is.
        (
            "RECOVER println(1); println(\"s\"); println(true); println(1.5); self.x = 1; self.y = 2;",
            &[],
        ),
        (
            "RECOVER println(grow(1)); self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        // 24-25: an inner cleanup panic reaches the outer recovery exit.
        (
            "RECOVER if flag { defer boom(); } self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        (
            "RECOVER if flag { defer calm(1); } self.x = 1; self.y = 2;",
            &[],
        ),
        // 26-27: break runs a panicking cleanup on the way out of the loop.
        (
            "RECOVER while flag { defer boom(); break; } self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        (
            "RECOVER while flag { defer calm(1); break; } self.x = 1; self.y = 2;",
            &[],
        ),
        // 28: inner recovery resumes after its scope, then initializes. With an
        // outer recovery too, a lexical recover() may be conditional, so the
        // outer exit stays reachable.
        ("if flag { RECOVER boom(); } self.x = 1; self.y = 2;", &[]),
        (
            "RECOVER if flag { RECOVER boom(); } self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
        // 29: a recovering cleanup that itself panics leaves to the outer scope.
        (
            "RECOVER if flag { defer { match recover() { Some(_) => {}, None => {} }; boom(); }; } self.x = 1; self.y = 2;",
            &["x", "y"],
        ),
    ];
    for (body, expected) in cases {
        check_items(helpers, &body.replace("RECOVER", recovery), expected);
    }
    // 30: interface dispatch stays conservative (its implementations are open).
    check_items(
        &format!(
            "{helpers} class Impl implements Probe {{ pub init(self) {{}} pub fn ok(self) -> bool {{ return true; }} }}"
        ),
        &format!("let p: Probe = new Impl(); {recovery} let ok = p.ok(); self.x = 1; self.y = 2;"),
        &["x", "y"],
    );
}

/// willow-ssl7.19 scaling: helper chains, recursive SCCs, fan-out and repeated
/// calls. The checker accepts each shape through solved facts, and one flow
/// run asks the call oracle exactly once per call expression.
#[test]
fn constructor_flow_effect_query_counts() {
    let recovery = "defer match recover() { Some(_) => {}, None => {} };";
    println!("shape,size,call_sites,oracle_queries,nodes,edges");
    for size in [64_usize, 256, 1024] {
        for shape in ["chain", "scc", "fanout", "repeated"] {
            let mut items = String::new();
            let mut calls = String::new();
            match shape {
                "chain" => {
                    for i in 0..size {
                        let next = if i + 1 < size {
                            format!("h{}(go);", i + 1)
                        } else {
                            String::new()
                        };
                        items.push_str(&format!("fn h{i}(go: bool) {{ if go {{ {next} }} }}"));
                    }
                    calls.push_str("h0(false);");
                }
                "scc" => {
                    for i in 0..size {
                        let next = (i + 1) % size;
                        items.push_str(&format!(
                            "fn h{i}(go: bool) {{ if go {{ h{next}(false); }} }}"
                        ));
                    }
                    calls.push_str("h0(false);");
                }
                "fanout" => {
                    for i in 0..size {
                        items.push_str(&format!("fn h{i}(go: bool) {{ }}"));
                        calls.push_str(&format!("h{i}(false);"));
                    }
                }
                "repeated" => {
                    items.push_str("fn h0(go: bool) { }");
                    for _ in 0..size {
                        calls.push_str("h0(false);");
                    }
                }
                _ => unreachable!(),
            }
            // End to end: facts prove every call panic-free.
            check_items(
                &items,
                &format!("{recovery} {calls} self.x = 1; self.y = 2;"),
                &[],
            );
            // One flow run: oracle queries are linear in call expressions.
            let call_sites = calls.matches('(').count();
            let queries = std::cell::Cell::new(0_usize);
            let oracle = |_: &Expr| {
                queries.set(queries.get() + 1);
                false
            };
            let source = format!(
                "class C {{ x: i64; y: i64; init(self) {{ {recovery} {calls} self.x = 1; self.y = 2; }} }}"
            );
            let names = ["x".to_string(), "y".to_string()];
            let (missing, c) = measure_with(&source, &names, &HashMap::new(), &oracle);
            assert_eq!(missing, [false, false], "{shape}");
            // The recovery defer's recover() call is the one extra query.
            assert_eq!(queries.get(), call_sites + 1, "{shape}");
            assert!(c.nodes <= 8 * call_sites + 40, "{shape}: {c:?}");
            println!(
                "{shape},{size},{call_sites},{},{},{}",
                queries.get(),
                c.nodes,
                c.edges
            );
        }
    }
}
