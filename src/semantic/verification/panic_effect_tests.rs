//! Value proofs must refine declaration contracts without weakening runtime facts.
use super::*;
use crate::semantic::{TypeChecker, ids::FunctionId};

fn checked(source: &str, options: Options) -> TypeChecker {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (program, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    let mut checker = TypeChecker::new();
    checker.verification_options = Some(options);
    checker.check_module_program(&program);
    checker
}
fn options() -> Options {
    Options {
        artifacts: None,
        ..Options::default()
    }
}
fn rejects(checker: &TypeChecker) -> bool {
    checker.errors.iter().any(|d| d.severity == Severity::Error)
}

#[test]
fn proof_no_panic_acceptance_matrix() {
    let cases = [
        (
            "increment",
            "x:i64",
            "requires x>=0 && x<100 ensures result==x+1",
            "return x+1;",
            true,
        ),
        ("subtraction", "x:i64", "requires x>0", "return x-1;", true),
        (
            "multiplication",
            "x:i64",
            "requires x>=0 && x<100",
            "return x*2;",
            true,
        ),
        ("negation", "x:i64", "requires x>=0", "return -x;", true),
        ("division", "x:i64", "requires x>0", "return 100/x;", true),
        ("remainder", "x:i64", "requires x>0", "return 100%x;", true),
        (
            "frozen indexing",
            "xs:FrozenArray<i64>, i:i64",
            "requires i>=0 && i<xs.len()",
            "return xs[i];",
            true,
        ),
        (
            "short circuit requires",
            "x:i64",
            "requires x<100 && x+1>0",
            "return x+1;",
            true,
        ),
        (
            "ensures arithmetic",
            "x:i64",
            "requires x>=0 && x<100 ensures result==x+1",
            "return x+1;",
            true,
        ),
        (
            "branch safety",
            "x:i64",
            "",
            "if x<100 { return x+1; } return x;",
            true,
        ),
        ("overflow", "x:i64", "", "return x+1;", false),
        ("zero divisor", "x:i64", "", "return 100/x;", false),
        (
            "negative remainder overflow",
            "x:i64",
            "",
            "return x%(-1);",
            false,
        ),
        (
            "unbounded index",
            "xs:FrozenArray<i64>, i:i64",
            "",
            "return xs[i];",
            false,
        ),
        (
            "unsafe pre evaluation",
            "x:i64",
            "requires x+1>0",
            "return x;",
            false,
        ),
        (
            "unsafe post evaluation",
            "x:i64",
            "ensures result+1>x",
            "return x;",
            false,
        ),
        (
            "false postcondition",
            "x:i64",
            "ensures result>x",
            "return x;",
            false,
        ),
        ("vacuous", "x:i64", "requires false", "return x;", false),
        (
            "explicit panic",
            "x:i64",
            "",
            "panic(\"bad\"); return x;",
            false,
        ),
        ("unwrap", "x:Option<i64>", "", "return x.unwrap();", false),
        (
            "mutable array",
            "xs:Array<i64>",
            "requires xs.len()>0",
            "return xs[0];",
            false,
        ),
        ("recursion", "x:i64", "", "return f(x);", false),
        ("plain scalar", "x:i64", "", "return x;", true),
        (
            "minimum division overflow",
            "x:i64",
            "",
            "return x/(-1);",
            false,
        ),
    ];
    for (name, params, contract, body, accepted) in cases {
        let checker = checked(
            &format!(
                "@effects(no_panic) @pure @verified fn f({params})->i64 {contract} {{ {body} }}"
            ),
            options(),
        );
        assert_eq!(!rejects(&checker), accepted, "{name}: {:?}", checker.errors);
        if !accepted {
            assert!(
                checker
                    .errors
                    .iter()
                    .any(|d| matches!(d.code, ErrorCode::E2610 | ErrorCode::E2631)),
                "{name}: {:?}",
                checker.errors
            );
        }
        let evidence = &checker.panic_evidence[&FunctionId::free("f")];
        if accepted {
            assert_eq!(evidence.status.as_str(), "PROVEN", "{name}");
            assert_eq!(
                evidence.basis.as_str(),
                if name == "plain scalar" {
                    "effect-analysis"
                } else {
                    "solver-proof"
                },
                "{name}"
            );
        }
    }
}

#[test]
fn proof_no_panic_callers_and_other_effects() {
    let callee = "@pure @verified @effects(no_panic) fn inc(x:i64)->i64 requires x>=0 && x<100 ensures result==x+1 {return x+1;}";
    for (attrs, requires, accepted) in [
        ("@pure @verified", "requires x>=0 && x<100", true),
        ("@pure", "requires x>=0 && x<100", false),
        ("@pure @verified", "", false),
    ] {
        let checker = checked(
            &format!(
                "{callee} {attrs} @effects(no_panic) fn f(x:i64)->i64 {requires} {{return inc(x);}}"
            ),
            options(),
        );
        assert_eq!(!rejects(&checker), accepted, "{:?}", checker.errors);
    }
    for attrs in ["", "@pure @verified"] {
        let checker = checked(
            &format!(
                "{attrs} @effects(no_alloc) fn f(x:i64)->i64 requires x>=0 && x<100 {{return x+1;}}"
            ),
            options(),
        );
        assert!(!rejects(&checker), "{:?}", checker.errors);
        let allocation = checked(
            &format!("{attrs} @effects(no_alloc) fn f()->i64 {{let xs=[1]; return 1;}}"),
            options(),
        );
        assert!(
            allocation.errors.iter().any(|d| d.code == ErrorCode::E2610),
            "{:?}",
            allocation.errors
        );
    }
    let checker = checked(
        "@pure @verified @effects(no_panic,no_alloc,no_io,no_wait) fn f(x:i64)->i64 {return x;}",
        options(),
    );
    assert!(!rejects(&checker), "{:?}", checker.errors);
}

#[test]
fn proof_no_panic_unknown_is_not_evidence() {
    for options in [
        Options {
            resource_limit: 1,
            ..options()
        },
        Options {
            timeout_ms: 0,
            ..options()
        },
        Options {
            solver: "missing-proof-solver".into(),
            ..options()
        },
    ] {
        let checker = checked(
            "@pure @verified @effects(no_panic) fn f(x:i64)->i64 requires x>=0 && x<100 {return x+1;}",
            options,
        );
        assert!(
            checker.errors.iter().any(|d| d.code == ErrorCode::E2610),
            "{:?}",
            checker.errors
        );
        assert_eq!(
            checker.panic_evidence[&FunctionId::free("f")]
                .status
                .as_str(),
            "UNPROVEN"
        );
    }
}

#[test]
fn proof_no_panic_solver_work_counts() {
    for n in [1, 4, 16] {
        for m in [0, 4, 16] {
            for verified in [false, true] {
                let mut source = String::new();
                let attrs = if verified {
                    "@pure @verified @effects(no_panic)"
                } else {
                    "@pure"
                };
                for i in 0..n {
                    source += &format!(
                        "{attrs} fn leaf{i}(x:i64)->i64 requires x>=0 && x<100 {{return x+1;}}\n"
                    );
                }
                for j in 0..m {
                    source += &format!(
                        "{attrs} fn caller{j}(x:i64)->i64 requires x>=0 && x<100 {{return leaf{}(x);}}\n",
                        j % n
                    );
                }
                SOLVER_STARTS.set(0);
                PROOF_RUNS.set(0);
                let checker = checked(&source, options());
                assert!(!rejects(&checker), "{:?}", checker.errors);
                assert_eq!(SOLVER_STARTS.get(), usize::from(verified));
                assert_eq!(PROOF_RUNS.get(), if verified { n + m } else { 0 });
                println!(
                    "proof-count N={n} M={m} verified={verified} sessions={} functions={}",
                    SOLVER_STARTS.get(),
                    PROOF_RUNS.get()
                );
            }
        }
    }
}

#[test]
fn proof_no_panic_contract_and_data_changes_invalidate() {
    fn parse(source: &str) -> Program {
        let (program, errors) =
            crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap())
                .parse();
        assert!(errors.is_empty(), "{errors:?}");
        program
    }
    let source = "@pure @verified fn inc(x:i64)->i64 requires x>=0 && x<100 {return x+1;} @pure @verified @effects(no_panic) fn f(x:i64)->i64 requires x>=0 && x<100 {return inc(x);}";
    let mut cache = ProofCache::default();
    let first = comparison::capture(parse(source), &options(), &mut cache);
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    assert_eq!(
        first.lock.functions["f"]
            .no_panic
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .basis
            .as_str(),
        "solver-proof"
    );
    let repeated = comparison::capture(parse(source), &options(), &mut cache);
    assert_eq!(first.lock, repeated.lock);
    assert_eq!(cache.hits, 2);
    let changed = comparison::capture(
        parse(&source.replacen("x<100", "x<50", 1)),
        &options(),
        &mut cache,
    );
    assert!(
        changed.errors.iter().any(|e| e.contains("no_panic")),
        "{:?}",
        changed.errors
    );
    assert_eq!(
        changed.lock.functions["f"]
            .no_panic
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .status
            .as_str(),
        "UNPROVEN"
    );
    assert_ne!(
        first.lock.functions["f"].semantics,
        changed.lock.functions["f"].semantics
    );
    let source = "class Box { pub value:i64; } @pure @verified @effects(no_panic) fn f(b:Box)->i64 requires b.value>=0 && b.value<100 {return b.value+1;}";
    let first = comparison::capture(parse(source), &options(), &mut cache);
    assert!(first.errors.is_empty(), "{:?}", first.errors);
    let hits = cache.hits;
    let changed = comparison::capture(
        parse(&source.replace("pub value:i64;", "pub value:i64; pub other:i64;")),
        &options(),
        &mut cache,
    );
    assert!(changed.errors.is_empty(), "{:?}", changed.errors);
    assert_eq!(cache.hits, hits);
    assert_ne!(first.lock.functions["f"], changed.lock.functions["f"]);
    let mutable = comparison::capture(
        parse(&source.replace(
            "pub value:i64;",
            "pub value:i64; pub fn set(self) {self.value=0;}",
        )),
        &options(),
        &mut cache,
    );
    assert!(
        mutable
            .verification
            .reports
            .iter()
            .any(|r| r.status != Status::Proven)
    );
    assert_eq!(
        mutable.lock.functions["f"]
            .no_panic
            .as_ref()
            .unwrap()
            .as_ref()
            .unwrap()
            .status
            .as_str(),
        "UNPROVEN"
    );
}

#[test]
fn proof_no_panic_deep_shared_call_counts_and_diagnostics() {
    for n in [8, 32, 128] {
        let mut source =
            "@pure @verified @effects(no_panic) fn chain0(x:i64)->i64 requires x>=0 {return x+0;}"
                .to_string();
        for i in 1..n {
            source += &format!(
                "@pure @verified @effects(no_panic) fn chain{i}(x:i64)->i64 requires x>=0 {{chain{}(x); return chain{}(x);}}",
                i - 1,
                i - 1
            );
        }
        for i in 0..n {
            source += &format!("fn ordinary{i}(x:i64)->i64 {{return chain{}(x);}}", n - 1);
        }
        SOLVER_STARTS.set(0);
        PROOF_RUNS.set(0);
        let checker = checked(&source, options());
        assert!(!rejects(&checker), "{:?}", checker.errors);
        assert_eq!(SOLVER_STARTS.get(), 1);
        assert_eq!(PROOF_RUNS.get(), n);
        println!(
            "proof-chain depth={n} repeated-calls={} ordinary-callers={n} sessions=1 proofs={n}",
            2 * (n - 1)
        );
    }
    let checker = checked(
        "class Box {pub value:i64;} @pure @verified @effects(no_panic,no_alloc) fn f()->i64 {let b=new Box(1); return b.value;}",
        options(),
    );
    assert_eq!(
        checker.panic_evidence[&FunctionId::free("f")]
            .status
            .as_str(),
        "PROVEN",
        "{:?}",
        checker.errors
    );
    assert!(
        checker.errors.iter().any(|d| d.code == ErrorCode::E2610
            && d.notes
                .iter()
                .any(|n| n.contains("no_panic evidence: PROVEN (effect-analysis)"))),
        "{:?}",
        checker.errors
    );
}
