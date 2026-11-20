use super::*;

fn parse(source: &str) -> Program {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}\n{source}");
    program
}
fn options() -> Options {
    Options {
        artifacts: None,
        ..Options::default()
    }
}
fn prove(source: &str) -> Verification {
    verify(&parse(source), &options(), false)
}
fn range(bounds: &str, annotations: &str, body: &str, post: &str) -> String {
    format!(
        "@pure @verified fn f()->i64 ensures {post} {{for i in {bounds} {annotations} {{{body}}} return 0;}}"
    )
}

// Each named case is an independent acceptance perspective, including false
// claims at empty, reversed, extreme, jump and nested-loop boundaries.
#[test]
fn inferred_loop_proof_perspectives() {
    use Status::*;
    let cases = [
        ("range without annotations", range("0..3", "", "", "result==0"), Proven),
        ("user invariant plus inferred bounds", "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures result==n {let mut x=0;for i in 0..n invariant x==i {x=x+1;}return x;}".into(), Proven),
        ("array iteration", "@pure @verified fn f(a:FrozenArray<i64>)->i64 ensures result==a.len() {let mut x=0;for v in a invariant x==iteration {x=x+1;}return x;}".into(), Proven),
        ("array no annotations", "@pure @verified fn f(a:FrozenArray<i64>)->i64 {for v in a {} return 0;}".into(), Proven),
        ("user variant without user invariant", range("0..3", "variant 3-i", "", "result==0"), Proven),
        ("user constant variant wins", range("0..3", "variant 1", "", "result==0"), Counterexample),
        ("user negative variant wins", range("0..3", "variant -1", "", "result==0"), Counterexample),
        ("user invariant checked at entry", range("0..3", "invariant i>0", "", "true"), Counterexample),
        ("user invariant checked on backedge", range("0..3", "invariant i==0", "", "true"), Counterexample),
        ("empty range", range("2..2", "", "return 1/0;", "result==0"), Proven),
        ("empty false post", range("2..2", "", "return 1;", "result==1"), Counterexample),
        ("reversed range", range("3..0", "", "return 1/0;", "result==0"), Proven),
        ("reversed false post", range("3..0", "", "return 1;", "result==1"), Counterexample),
        ("reversed extreme bounds", range("9223372036854775807..(-9223372036854775807-1)", "", "return 1/0;", "result==0"), Proven),
        ("reversed extreme false post", range("9223372036854775807..(-9223372036854775807-1)", "", "", "result==1"), Counterexample),
        ("maximum boundary", range("9223372036854775806..9223372036854775807", "", "let x=i+1;", "result==0"), Proven),
        ("maximum body overflow", range("9223372036854775806..9223372036854775807", "", "let x=i+2;", "true"), Counterexample),
        ("minimum boundary", range("(-9223372036854775807-1)..(-9223372036854775806)", "", "let x=i+1;", "result==0"), Proven),
        ("minimum body overflow", range("(-9223372036854775807-1)..(-9223372036854775806)", "", "let x=i-1;", "true"), Counterexample),
        ("default rank overflow fails closed", range("(-9223372036854775807-1)..9223372036854775807", "", "", "true"), Counterexample),
        ("explicit rank can avoid default overflow", range("(-9223372036854775807-1)..9223372036854775807", "variant 0", "break;", "result==0"), Proven),
        ("empty maximum", range("9223372036854775807..9223372036854775807", "", "return 1/0;", "result==0"), Proven),
        ("empty minimum", range("(-9223372036854775807-1)..(-9223372036854775807-1)", "", "return 1/0;", "result==0"), Proven),
        ("continue increments before preservation", "@pure @verified fn f()->i64 ensures result==3 {let mut x=0;for i in 0..3 invariant x==i {x=x+1;continue;x=99;}return x;}".into(), Proven),
        ("continue does not skip preservation", "@pure @verified fn f()->i64 {let mut x=0;for i in 0..3 invariant x==i {continue;x=x+1;}return x;}".into(), Counterexample),
        ("break actual state", "@pure @verified fn f()->i64 ensures result==7 {let mut x=0;for i in 0..3 invariant x==0 {x=7;break;}return x;}".into(), Counterexample),
        // Induction need not infer that a break-only loop runs. The disjunction
        // states exactly what its supplied head invariant allows at exit.
        ("break exit mutation", "@pure @verified fn f()->i64 ensures result==0 || result==7 {let mut x=0;for i in 0..3 invariant x==0 {x=7;break;}return x;}".into(), Proven),
        ("break false exit", "@pure @verified fn f()->i64 ensures result==0 {let mut x=0;for i in 0..3 invariant x==0 {x=7;break;}return x;}".into(), Counterexample),
        ("nested range jumps", range("0..3", "", "for j in 0..i {if j==1 {break;} continue;} continue;", "result==0"), Proven),
        ("nested false post", range("0..3", "", "for j in 0..i {if j==1 {break;} continue;}", "result==1"), Counterexample),
        ("while still requires annotations", "@pure @verified fn f()->i64 {while false {} return 0;}".into(), Unknown),
        ("while still requires invariant", "@pure @verified fn f()->i64 {while false variant 0 {} return 0;}".into(), Unknown),
        ("while still requires variant", "@pure @verified fn f()->i64 {while false invariant true {} return 0;}".into(), Unknown),
        ("explicit range annotations compatible", "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures result==n {let mut x=0;for i in 0..n invariant i>=0 && i<=n invariant x==i variant n-i {x=x+1;}return x;}".into(), Proven),
        ("array continue", "@pure @verified fn f(a:FrozenArray<i64>)->i64 ensures result==a.len() {let mut x=0;for v in a invariant x==iteration {x=x+1;continue;}return x;}".into(), Proven),
        ("array false claim", "@pure @verified fn f(a:FrozenArray<i64>)->i64 requires a.len()>0 ensures result==0 {let mut x=0;for v in a invariant x==iteration {x=x+1;}return x;}".into(), Counterexample),
        ("array user variant wins", "@pure @verified fn f(a:FrozenArray<i64>)->i64 {for v in a variant 1 {} return 0;}".into(), Counterexample),
        ("empty array", "@pure @verified fn f(a:FrozenArray<i64>)->i64 requires a.len()==0 ensures result==0 {for v in a {return 1/0;} return 0;}".into(), Proven),
        ("empty array false claim", "@pure @verified fn f(a:FrozenArray<i64>)->i64 requires a.len()==0 ensures result==1 {for v in a {return 1;} return 0;}".into(), Counterexample),
        ("nested arrays", "@pure @verified fn f(a:FrozenArray<i64>)->i64 {for v in a {for w in a {continue;} break;} return 0;}".into(), Proven),
        ("array exhausted element unavailable", "@pure @verified fn f(a:FrozenArray<i64>)->i64 {for v in a invariant v==0 {} return 0;}".into(), Unknown),
        ("user arithmetic is checked", range("0..3", "invariant 1/(i-i)>0", "", "true"), Counterexample),
        ("captured range bound survives assignment", "@pure @verified fn f()->i64 ensures result==3 {let mut n=3;let mut x=0;for i in 0..n invariant x==i {n=0;x=x+1;}return x;}".into(), Proven),
        ("captured start survives assignment", "@pure @verified fn f()->i64 ensures result==5 {let mut n=2;let mut x=2;for i in n..5 invariant x==i {n=100;x=x+1;}return x;}".into(), Proven),
        ("bound overflow is checked", range("0..(9223372036854775807+1)", "", "", "true"), Counterexample),
        ("earlier safety cannot disappear", "@pure @verified fn f()->i64 {let x=1/0;for i in 3..0 {}return 0;}".into(), Counterexample),
        ("unreachable loop", "@pure @verified fn f()->i64 ensures result==0 {return 0;for i in 0..3 invariant false {return 1/0;}}".into(), Proven),
        ("counter shadow restores outer binding", "@pure @verified fn f()->i64 ensures result==9 {let i=9;for i in 0..3 {}return i;}".into(), Proven),
        ("array iteration does not capture body binding", "@pure @verified fn f(a:FrozenArray<i64>)->i64 requires a.len()<=100 ensures result==a.len()*9 {let iteration=9;let mut x=0;for v in a invariant x==iteration*9 {x=x+iteration;}return x;}".into(), Proven),
    ];
    assert!(cases.len() >= 20);
    for (name, source, expected) in cases {
        let v = prove(&source);
        assert_eq!(
            v.reports.last().unwrap().status,
            expected,
            "{name}: {source}\n{:?}",
            v.reports
        );
    }
}

#[test]
fn inferred_loop_reports_cache_and_replay() {
    let source = "@pure @verified fn f(a:FrozenArray<i64>)->i64 {for i in 0..3 {for v in a {} } for j in 0..2 variant 2-j {} return 0;}";
    let program = parse(source);
    let mut cache = ProofCache::default();
    let first = verify_cached(&program, &options(), false, &mut cache);
    let second = verify_cached(&program, &options(), false, &mut cache);
    assert_eq!(cache.hits, 1);
    assert_eq!(first.reports[0].status, Status::Proven);
    assert_eq!(first.smt, prove(source).smt);
    let json = serde_json::to_value(&first.reports[0]).unwrap();
    assert_eq!(
        json["inferred_loop_annotations"],
        serde_json::to_value(&second.reports[0]).unwrap()["inferred_loop_annotations"]
    );
    let annotations = json["inferred_loop_annotations"].as_array().unwrap();
    assert_eq!(annotations.len(), 3);
    assert_eq!(annotations[0]["kind"], "range");
    assert_eq!(annotations[0]["counter"], "i");
    assert_eq!(annotations[1]["counter"], "iteration");
    assert_eq!(annotations[1]["kind"], "frozen_array");
    assert_eq!(annotations[0]["variant"], "captured_end - counter");
    assert!(annotations[2]["variant"].is_null());
    assert!(
        annotations[0]["invariant"]
            .as_str()
            .unwrap()
            .contains("max(captured_start, captured_end)")
    );
    let mut solver = solver::Solver::new(&options().solver, 5000).unwrap();
    let replay = solver.exchange(&first.smt).unwrap();
    assert!(
        !replay.contains("unknown") && !replay.contains("error"),
        "{replay}"
    );
    let changed = parse(&source.replace("variant 2-j", "variant 1"));
    assert_eq!(
        verify_cached(&changed, &options(), false, &mut cache).reports[0].status,
        Status::Counterexample
    );
}

#[test]
fn inferred_loop_constant_obligations_and_scaling() {
    for nested in [false, true] {
        for count in [8, 16, 32] {
            let mut source = String::from("@pure @verified fn f()->i64 {");
            for i in 0..count {
                source += &format!("for i{i} in 0..9223372036854775807 {{");
                if !nested {
                    source += "}";
                }
            }
            if nested {
                source += &"}".repeat(count);
            }
            source += "return 0;}";
            let program = parse(&source);
            let Item::Function(f) = &program.items[0] else {
                panic!()
            };
            let summary =
                encode(0, &[f], &HashMap::from([("f", 0)]), &DataTypes::default()).unwrap();
            let safe = summary
                .definitions
                .lines()
                .find(|s| s.starts_with("(define-fun f0_safe "))
                .unwrap();
            // Entry/preservation, rank bounds twice, nonnegativity, increment
            // safety and strict decrease: seven guards, independent of trip
            // count and nesting. One final function fallthrough guard remains.
            let obligations = safe.matches("(=> ").count();
            assert_eq!(obligations, count * 7 + 1);
            assert_eq!(summary.inferred_loop_annotations.len(), count);
            assert!(summary.nodes <= count * 40 + 20);
            eprintln!(
                "inferred nested={nested} loops={count} guards={obligations} nodes={} bytes={}",
                summary.nodes,
                summary.definitions.len()
            );
        }
    }
}

#[test]
fn inferred_loop_array_and_exit_obligation_counts() {
    for count in [8, 16, 32] {
        for shape in ["arrays", "break", "continue", "return"] {
            let mut source = String::from("@pure @verified fn f(a:FrozenArray<i64>)->i64 {");
            if shape == "arrays" {
                for i in 0..count {
                    source += &format!("for v{i} in a {{");
                }
                source += &"}".repeat(count);
            } else {
                source += "for i in 0..100 {";
                for i in 0..count {
                    let jump = if shape == "return" { "return 0" } else { shape };
                    source += &format!("if i=={i} {{{jump};}}");
                }
                source += "}";
            }
            source += "return 0;}";
            let program = parse(&source);
            let writes = crate::semantic::field_writes::FieldWrites::from_program(&program);
            let data = DataTypes::new(&program, &writes);
            let Item::Function(f) = &program.items[0] else {
                panic!()
            };
            let summary = encode(0, &[f], &HashMap::from([("f", 0)]), &data).unwrap();
            let safe = summary
                .definitions
                .lines()
                .find(|s| s.starts_with("(define-fun f0_safe "))
                .unwrap();
            let loops = if shape == "arrays" { count } else { 1 };
            let guards = safe.matches("(=> ").count();
            assert_eq!(guards, loops * 7 + 1);
            assert_eq!(summary.inferred_loop_annotations.len(), loops);
            assert!(summary.nodes < count * 40 + 50);
            eprintln!(
                "inferred shape={shape} size={count} guards={guards} nodes={} bytes={}",
                summary.nodes,
                summary.definitions.len()
            );
        }
    }
}
