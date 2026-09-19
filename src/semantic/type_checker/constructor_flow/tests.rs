use super::*;
use crate::diagnostics::ErrorCode;

fn check(body: &str, expected_missing: &[&str]) {
    let source =
        format!("class C {{ x: i64; y: i64; init(self, flag: bool) {{ {body} }} }} fn main() {{}}");
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
    let body = parse_body(source);
    let fields = names
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    let types = HashMap::new();
    let mut graph = Graph::new(&fields, &types);
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
                stmts: vec![],
                span,
            };
            for _ in 0..50_000 {
                body = Block {
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
                uninitialized_fields(&body, ["x"].into_iter(), &types),
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
