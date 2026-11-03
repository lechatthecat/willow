use super::*;

fn parse(source: &str) -> Program {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    program
}
fn prove(source: &str) -> Verification {
    verify(
        &parse(source),
        &Options {
            artifacts: None,
            ..Options::default()
        },
        false,
    )
}
fn check(source: &str, expected: Status) {
    let v = prove(source);
    assert_eq!(
        v.reports.last().unwrap().status,
        expected,
        "{source}\n{:?}",
        v.reports
    );
}
fn countdown(body: &str, annotations: &str, post: &str) -> String {
    format!(
        "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures {post} {{let mut x=n; while x>0 {annotations} {{{body}}} return x;}}"
    )
}

#[test]
fn verified_loop_proof_perspectives() {
    use Status::*;
    for (name, source, expected) in [
        (
            "countdown",
            countdown("x=x-1;", "invariant x>=0 variant x", "result==0"),
            Proven,
        ),
        (
            "multiple invariants",
            countdown(
                "x=x-1;",
                "invariant x>=0 invariant x<=n variant x",
                "result==0",
            ),
            Proven,
        ),
        (
            "parentheses",
            countdown("x=x-1;", "invariant(x>=0) variant(x)", "result==0"),
            Proven,
        ),
        (
            "annotation order",
            countdown("x=x-1;", "variant x invariant x>=0", "result==0"),
            Proven,
        ),
        (
            "false entry",
            countdown("x=x-1;", "invariant x>0 variant x", "result==0"),
            Counterexample,
        ),
        (
            "preservation",
            countdown("x=x-2;", "invariant x>=0 variant x", "result==0"),
            Counterexample,
        ),
        (
            "negative rank",
            countdown("x=x-1;", "invariant x>=0 variant -x", "result==0"),
            Counterexample,
        ),
        (
            "constant rank",
            countdown("x=x-1;", "invariant x>=0 variant 1", "result==0"),
            Counterexample,
        ),
        (
            "stuttering",
            countdown("x=x;", "invariant x>=0 variant x", "result==0"),
            Counterexample,
        ),
        (
            "increasing",
            countdown("x=x+1;", "invariant x>=0 variant x", "result==0"),
            Counterexample,
        ),
        (
            "missing both",
            countdown("x=x-1;", "", "result==0"),
            Unknown,
        ),
        (
            "missing invariant",
            countdown("x=x-1;", "variant x", "result==0"),
            Unknown,
        ),
        (
            "missing variant",
            countdown("x=x-1;", "invariant x>=0", "result==0"),
            Unknown,
        ),
        (
            "wrong post",
            countdown("x=x-1;", "invariant x>=0 variant x", "result==1"),
            Counterexample,
        ),
        (
            "body arithmetic",
            countdown("x=x/0;", "invariant x>=0 variant x", "result==0"),
            Counterexample,
        ),
        (
            "invariant arithmetic",
            countdown("x=x-1;", "invariant 1/(x-x)>0 variant x", "result==0"),
            Counterexample,
        ),
        (
            "rank arithmetic",
            countdown("x=x-1;", "invariant x>=0 variant x+1", "result==0"),
            Counterexample,
        ),
        (
            "shadowed body local",
            countdown("let mut x=1; x=0;", "invariant x>=0 variant x", "result==0"),
            Counterexample,
        ),
        (
            "early return",
            countdown("return 0;", "invariant x>=0 variant x", "result==0"),
            Proven,
        ),
        (
            "wrong early return",
            countdown("return 1;", "invariant x>=0 variant x", "result==0"),
            Counterexample,
        ),
        (
            "conditional early return",
            countdown(
                "if x==2 {return 0;} x=x-1;",
                "invariant x>=0 variant x",
                "result==0",
            ),
            Proven,
        ),
        (
            "branch decrease",
            countdown(
                "if x>1 {x=x-2;} else {x=x-1;}",
                "invariant x>=0 variant x",
                "result==0",
            ),
            Proven,
        ),
        (
            "branch stutter",
            countdown("if x>1 {x=x-1;}", "invariant x>=0 variant x", "result==0"),
            Counterexample,
        ),
        (
            "break unsupported",
            countdown("break;", "invariant x>=0 variant x", "result==0"),
            Unknown,
        ),
        (
            "continue unsupported",
            countdown("continue;", "invariant x>=0 variant x", "result==0"),
            Unknown,
        ),
        (
            "nested loop",
            countdown(
                "let mut y=x; while y>0 invariant y>=0 variant y {y=y-1;} x=x-1;",
                "invariant x>=0 variant x",
                "result==0",
            ),
            Proven,
        ),
        (
            "nested missing annotation",
            countdown(
                "while false {} x=x-1;",
                "invariant x>=0 variant x",
                "result==0",
            ),
            Unknown,
        ),
        (
            "nested invalid variant",
            countdown(
                "let mut y=x; while y>0 invariant y>=0 variant 1 {y=y-1;} x=x-1;",
                "invariant x>=0 variant x",
                "result==0",
            ),
            Counterexample,
        ),
    ] {
        eprintln!("perspective: {name}");
        check(&source, expected);
    }
}

#[test]
fn verified_loop_paths_and_frames() {
    use Status::*;
    for (source, expected) in [
        (
            "@pure @verified fn f()->i64 ensures result==3 {let mut x=3; while false invariant x==3 variant 0 {} return x;}",
            Proven,
        ),
        (
            "@pure @verified fn f()->i64 ensures result==0 {let mut x=0; while x<3 invariant x>=0 && x<=3 variant 3-x {x=x+1;} return x;}",
            Counterexample,
        ),
        (
            "@pure @verified fn f()->i64 ensures result==3 {let mut x=0; while x<3 invariant x>=0 && x<=3 variant 3-x {x=x+1;} return x;}",
            Proven,
        ),
        (
            "@pure @verified fn f(b:bool)->i64 ensures result==0 || result==7 {let mut x=7;if b {while x>0 invariant x>=0 variant x {x=x-1;}}return x;}",
            Proven,
        ),
        (
            "@pure @verified fn f()->i64 ensures result==7 {let mut x=7;while false invariant true variant 0 {let mut x=0;x=1;} return x;}",
            Proven,
        ),
        (
            "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures result==n {let mut x=n;while x>0 invariant x>=0 variant x {x=x-1;}return n;}",
            Proven,
        ),
        (
            "@pure @verified fn f()->i64 ensures result==5 {let mut x=0;while x<3 invariant x>=0 && x<=3 variant 3-x {x=x+1;}while x<5 invariant x>=3 && x<=5 variant 5-x {x=x+1;}return x;}",
            Proven,
        ),
        (
            "@pure @verified fn f()->i64 ensures result==1 {while true invariant true variant 0 {return 1;}return 2;}",
            Proven,
        ),
        (
            "@pure @verified fn f()->i64 ensures result==1 {while true invariant true variant 0 {} return 1;}",
            Counterexample,
        ),
        (
            "@pure @verified fn f()->i64 {let mut x=0;while x<2 invariant true variant 2-x {x=x+1;}return 1;}",
            Counterexample,
        ),
        // An impossible later loop must not discharge an earlier safety error.
        (
            "@pure @verified fn f()->i64 {let y=1/0;while false invariant false variant 0 {}return 0;}",
            Counterexample,
        ),
        (
            "@pure @verified fn f()->i64 {while false invariant false variant 0 {}return 0;}",
            Counterexample,
        ),
        (
            "@pure @verified fn f()->i64 {return 0;while false invariant false variant 1/0 {}}",
            Proven,
        ),
        (
            "@pure @verified fn f()->i64 ensures result==0 {let mut x=true;while x invariant true variant (x ? 1 : 0) {x=false;}return 0;}",
            Proven,
        ),
    ] {
        check(source, expected);
    }
}

#[test]
fn verified_loop_call_boundaries() {
    let helper = "@pure @verified fn g(n:i64)->i64 requires n>=0 ensures result==0 {let mut x=n;while x>0 invariant x>=0 variant x {x=x-1;}return x;}";
    check(
        &format!("{helper} @pure @verified fn f()->i64 ensures result==0 {{return g(5);}}"),
        Status::Proven,
    );
    check(
        &format!("{helper} @pure @verified fn f()->i64 {{return g(-1);}}"),
        Status::Counterexample,
    );
    check(
        &format!(
            "{} @pure @verified fn f()->i64 {{return g(5);}}",
            helper.replace("@verified", "")
        ),
        Status::Unknown,
    );
    let step = "@pure @verified fn step(x:i64)->i64 requires x>0 ensures result==x-1 {return x-1;}";
    check(
        &format!(
            "{step} {}",
            countdown("x=step(x);", "invariant x>=0 variant x", "result==0")
        ),
        Status::Proven,
    );
}

#[test]
fn verified_loop_determinism_cache_and_replay() {
    let source = countdown("x=x-1;", "invariant x>=0 variant x", "result==0");
    let a = prove(&source);
    let b = prove(&source);
    assert_eq!(a.smt, b.smt);
    assert_eq!(a.reports[0].status, Status::Proven);
    let mut solver = solver::Solver::new(&Options::default().solver, 5000).unwrap();
    assert!(!solver.exchange(&a.smt).unwrap().contains("unknown"));
    let mut cache = ProofCache::default();
    let options = Options {
        artifacts: None,
        ..Options::default()
    };
    let p = parse(&source);
    verify_cached(&p, &options, false, &mut cache);
    verify_cached(&p, &options, false, &mut cache);
    assert_eq!(cache.hits, 1);
    let changed = parse(&source.replace("variant x", "variant 1"));
    let failed = verify_cached(&changed, &options, false, &mut cache);
    assert_eq!(failed.reports[0].status, Status::Counterexample);
    let exhausted = verify(
        &p,
        &Options {
            resource_limit: 1,
            ..options
        },
        false,
    );
    assert_eq!(exhausted.reports[0].status, Status::Unknown);
}

#[test]
fn verified_loop_scaling_counts() {
    for n in [8, 16, 32] {
        let mut source = String::from("@pure @verified fn f()->i64 ensures result==0 {");
        for i in 0..n {
            source += &format!(
                "let mut x{i}=2;while x{i}>0 invariant x{i}>=0 variant x{i} {{x{i}=x{i}-1;}}"
            );
        }
        source += "return 0;}";
        let v = prove(&source);
        assert_eq!(v.reports[0].status, Status::Proven, "{:?}", v.reports);
        assert_eq!(v.smt.matches("(declare-const h").count(), n);
        assert!(v.reports[0].vc_nodes < n * 45 + 20);
        assert!(v.smt.len() < n * 15000 + 10000);
        eprintln!(
            "loops={n} nodes={} bytes={}",
            v.reports[0].vc_nodes,
            v.smt.len()
        );
    }
}

#[test]
fn verified_loop_syntax_roundtrip_and_rejections() {
    let source = countdown(
        "x=x-1;",
        "invariant(x>=0) invariant(x<=n) variant(x)",
        "result==0",
    );
    let p = parse(&source);
    let cloned = p.clone();
    let serialized = serde_json::to_string(&cloned).unwrap();
    let restored: Program = serde_json::from_str(&serialized).unwrap();
    let result = verify(
        &restored,
        &Options {
            artifacts: None,
            ..Options::default()
        },
        false,
    );
    assert_eq!(result.reports[0].status, Status::Proven);
    for annotations in [
        "invariant variant x",
        "invariant true variant",
        "invariant true variant x variant x",
        "invariant () variant x",
    ] {
        let source = countdown("x=x-1;", annotations, "result==0");
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (_, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(!errors.is_empty(), "accepted {source}");
    }
}

#[test]
fn verified_loop_nested_write_and_exit_scaling() {
    for n in [8, 16, 32] {
        for shared in [false, true] {
            let mut source = String::from("@pure @verified fn f()->i64 {");
            if shared {
                for i in 0..n {
                    source += &format!("let mut x{i}=2;");
                }
            }
            for i in 0..n {
                if !shared {
                    source += &format!("let mut x{i}=2;");
                }
                source += &format!("while x{i}>0 invariant x{i}>=0 variant x{i} {{");
            }
            for i in 0..n {
                source += &format!("x{i}=x{i}-1;");
            }
            for _ in 0..n {
                source += "}";
            }
            source += "return 0;}";
            let p = parse(&source);
            let Item::Function(f) = &p.items[0] else {
                panic!()
            };
            let fs = [f];
            let names = HashMap::from([(f.name.as_str(), 0)]);
            let summary = encode(0, &fs, &names).unwrap();
            let heads = if shared { n * n } else { n * (n + 1) / 2 };
            // Assigning outer bindings from inner bodies is visible at all
            // intervening heads; body-local declarations do not escape outward.
            assert_eq!(summary.loop_symbols.len(), heads);
            assert!(summary.nodes < heads * 4 + n * 45 + 20);
            eprintln!(
                "nested shared={shared} depth={n} heads={heads} nodes={} bytes={}",
                summary.nodes,
                summary.definitions.len()
            );
        }
        let mut source = String::from(
            "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures result==0 {let mut x=n;while x>0 invariant x>=0 variant x {",
        );
        for i in 0..n {
            source += &format!("if x=={} {{return 0;}}", i + 1);
        }
        source += "x=x-1;}return 0;}";
        let v = prove(&source);
        assert_eq!(v.reports[0].status, Status::Proven);
        assert!(v.reports[0].vc_nodes < n * 10 + 60);
        eprintln!(
            "loop exits={n} nodes={} bytes={}",
            v.reports[0].vc_nodes,
            v.smt.len()
        );
    }
}

#[test]
fn verified_loop_counterexample_names_arbitrary_head() {
    let v = prove(&countdown("x=x;", "invariant x>=0 variant x", "result==0"));
    let r = &v.reports[0];
    assert_eq!(r.status, Status::Counterexample);
    assert!(
        r.input_symbols
            .values()
            .any(|v| v.starts_with("loop@") && v.ends_with("::x"))
    );
    assert!(
        r.obligations
            .last()
            .unwrap()
            .counterexample_candidate
            .as_ref()
            .unwrap()
            .contains("h0_0")
    );
}

#[test]
fn verified_loop_annotations_reach_shared_walkers() {
    use crate::parser::iter::{AstEvent, AstWalk};
    use crate::parser::visit::{AstVisitor, walk_expr};
    let p = parse("fn f(){let mut x=1;while x>0 invariant x>=0 variant x {x=x-1;}}");
    let Item::Function(f) = &p.items[0] else {
        panic!()
    };
    let events = AstWalk::new(AstEvent::Block(&f.body))
        .filter(|e| matches!(e, AstEvent::Expr(Expr::Var(n, ..)) if n=="x"))
        .count();
    struct Count(usize);
    impl AstVisitor for Count {
        fn visit_expr(&mut self, e: &Expr) {
            if matches!(e, Expr::Var(n, ..) if n=="x") {
                self.0 += 1;
            }
            walk_expr(self, e);
        }
    }
    let mut visitor = Count(0);
    visitor.visit_block(&f.body);
    assert_eq!(events, 4);
    assert_eq!(visitor.0, events);
}
