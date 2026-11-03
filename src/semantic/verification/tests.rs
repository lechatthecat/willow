use super::*;
fn parse(source: &str) -> Program {
    let tokens = crate::lexer::Lexer::new(source).tokenize().unwrap();
    let (p, errors) = crate::parser::Parser::new(tokens).parse();
    assert!(errors.is_empty(), "{errors:?}");
    p
}
fn options() -> Options {
    Options {
        artifacts: None,
        ..Options::default()
    }
}
fn verify_source(source: &str) -> Verification {
    verify(&parse(source), &options(), false)
}
fn status(source: &str, expected: Status) {
    let v = verify_source(source);
    assert_eq!(
        v.reports.last().unwrap().status,
        expected,
        "{}",
        serde_json::to_string_pretty(&v.reports).unwrap()
    );
}
#[test]
fn verification_acceptance_cases() {
    use Status::*;
    for (name, source, want) in [
        (
            "larger",
            "@pure @verified fn f(a:i64,b:i64)->i64 ensures result>=a ensures result>=b ensures result==a || result==b {if a>=b {return a;} return b;}",
            Proven,
        ),
        (
            "wrong post",
            "@pure @verified fn f(x:i64)->i64 ensures result>x {return x;}",
            Counterexample,
        ),
        (
            "negative quotient",
            "@pure @verified fn f()->i64 ensures result == -3 {return -7/2;}",
            Proven,
        ),
        (
            "negative remainder",
            "@pure @verified fn f()->i64 ensures result == -1 {return -7%2;}",
            Proven,
        ),
        (
            "negative divisor",
            "@pure @verified fn f()->i64 ensures result == -3 {return 7/-2;}",
            Proven,
        ),
        (
            "negative divisor rem",
            "@pure @verified fn f()->i64 ensures result == 1 {return 7%-2;}",
            Proven,
        ),
        (
            "min div",
            "@pure @verified fn f()->i64 {return (-9223372036854775807-1)/-1;}",
            Counterexample,
        ),
        (
            "min rem",
            "@pure @verified fn f()->i64 {return (-9223372036854775807-1)%-1;}",
            Counterexample,
        ),
        (
            "price",
            "@pure @verified fn f(price:i64)->i64 ensures result==price*9/10 {return price;}",
            Counterexample,
        ),
        (
            "false pre",
            "@pure @verified fn f()->i64 requires false {return 0;}",
            VacuousPrecondition,
        ),
        (
            "empty interval",
            "@pure @verified fn f(x:i64)->i64 requires x>10 requires x<0 {return x;}",
            VacuousPrecondition,
        ),
        (
            "overflow",
            "@pure @verified fn f(x:i64)->i64 {return x+1;}",
            Counterexample,
        ),
        (
            "zero div",
            "@pure @verified fn f(x:i64)->i64 {return 10/x;}",
            Counterexample,
        ),
        (
            "loop",
            "@pure @verified fn f()->i64 {while false {} return 0;}",
            Unknown,
        ),
        (
            "recursion",
            "@pure @verified fn f(x:i64)->i64 {return f(x);}",
            Unknown,
        ),
        (
            "mutual recursion",
            "@pure fn g(x:i64)->i64 {return f(x);} @pure @verified fn f(x:i64)->i64 {return g(x);}",
            Unknown,
        ),
        (
            "helper",
            "@pure fn g(x:i64)->i64 {return x;} @pure @verified fn f(x:i64)->i64 ensures result==x {return g(x);}",
            Proven,
        ),
        (
            "helper false ensures",
            "@pure fn g(x:i64)->i64 ensures result>x {return x;} @pure @verified fn f(x:i64)->i64 {return g(x);}",
            Counterexample,
        ),
        (
            "missing body",
            "@pure @verified fn f(x:i64)->i64 {return missing(x);}",
            Unknown,
        ),
        (
            "verified alone",
            "@verified fn f()->i64 {return 1;}",
            Unknown,
        ),
        (
            "requires safety",
            "@pure @verified fn f(x:i64)->i64 requires 10/x>0 {return 1;}",
            Counterexample,
        ),
        (
            "short circuit and",
            "@pure @verified fn f(x:i64)->i64 requires x!=0 && 10/x>0 {return 1;}",
            Proven,
        ),
        (
            "short circuit or",
            "@pure @verified fn f(x:i64)->i64 requires x==0 || 10/x>0 {return 1;}",
            Proven,
        ),
        (
            "bool",
            "@pure @verified fn f(x:bool)->bool ensures result!=x {return !x;}",
            Proven,
        ),
        (
            "branch assignment",
            "@pure @verified fn f(x:bool)->i64 ensures result==1 || result==2 {let mut y=0; if x {y=1;} else {y=2;} return y;}",
            Proven,
        ),
        (
            "early return",
            "@pure @verified fn f(x:bool)->i64 ensures result==1 || result==2 {if x {return 1;} return 2;}",
            Proven,
        ),
        (
            "unreachable arithmetic",
            "@pure @verified fn f()->i64 ensures result==1 {return 1; let x=1/0;}",
            Proven,
        ),
        (
            "generic",
            "@pure @verified fn f<T>(x:i64)->i64 {return x;}",
            Unknown,
        ),
        (
            "heap",
            "@pure @verified fn f()->i64 {let a=[1];return 1;}",
            Unknown,
        ),
        (
            "closure",
            "@pure @verified fn f()->i64 {let a=|| 1;return 1;}",
            Unknown,
        ),
        (
            "neg overflow",
            "@pure @verified fn f(x:i64)->i64 {return -x;}",
            Counterexample,
        ),
        (
            "mul overflow",
            "@pure @verified fn f(x:i64)->i64 {return x*2;}",
            Counterexample,
        ),
        (
            "sub overflow",
            "@pure @verified fn f(x:i64)->i64 {return x-1;}",
            Counterexample,
        ),
        (
            "ternary",
            "@pure @verified fn f(x:i64)->i64 ensures result>=0 {return x<0 ? 0 : x;}",
            Proven,
        ),
        (
            "constant",
            "const N:i64=4; @pure @verified fn f()->i64 ensures result==4 {return N;}",
            Proven,
        ),
    ] {
        eprintln!("perspective: {name}");
        status(source, want);
    }
}
#[test]
fn verification_verified_call_contracts() {
    for (requires, want) in [
        ("requires x<9223372036854775807", Status::Proven),
        ("", Status::Counterexample),
    ] {
        status(
            &format!(
                "@pure @verified fn g(x:i64)->i64 requires x<9223372036854775807 ensures result==x+1 {{return x+1;}} @pure @verified fn f(x:i64)->i64 {requires} ensures result==x+1 {{return g(x);}}"
            ),
            want,
        );
    }
    // A weak callee contract must not expose its body's stronger guarantee.
    status(
        "@pure @verified fn g(x:i64)->i64 ensures result>=x {return x;} @pure @verified fn f(x:i64)->i64 ensures result==x {return g(x);}",
        Status::Counterexample,
    );
}
#[test]
fn verification_determinism_and_replay() {
    let source = "@pure @verified fn f(x:i64)->i64 ensures result==x {return x;}";
    let a = verify_source(source);
    let b = verify_source(source);
    assert_eq!(a.smt, b.smt);
    assert_eq!(
        serde_json::to_string(&a.reports).unwrap(),
        serde_json::to_string(&b.reports).unwrap()
    );
    let mut s = solver::Solver::new(&options().solver, 5000).unwrap();
    assert!(!s.exchange(&a.smt).unwrap().contains("unknown"));
}
#[test]
fn verification_missing_solver_and_prior_error_fail_closed() {
    let p = parse("@pure @verified fn f()->i64 {return 1;}");
    let v = verify(
        &p,
        &Options {
            solver: "/definitely/missing/z3".into(),
            ..options()
        },
        false,
    );
    assert_eq!(v.reports[0].status, Status::Unknown);
    let v = verify(&p, &options(), true);
    assert_eq!(v.reports[0].status, Status::Unknown);
}
#[test]
fn verification_shared_helper_scaling() {
    for n in [8, 16, 32] {
        let mut source = String::from("@pure fn g(x:i64)->i64 {return x;}");
        source.push_str("@pure @verified fn f(x:i64)->i64 ensures result==x {let mut y=x;");
        for _ in 0..n {
            source.push_str("y=g(y);");
        }
        source.push_str("return y;}");
        let v = verify_source(&source);
        assert_eq!(v.reports[0].status, Status::Proven);
        assert_eq!(v.smt.matches("(define-fun f0_value ").count(), 1);
        assert!(v.reports[0].vc_nodes < n * 12 + 20);
        eprintln!(
            "calls={n} nodes={} bytes={}",
            v.reports[0].vc_nodes,
            v.smt.len()
        );
    }
}

#[test]
fn verification_resource_exhaustion_is_unknown() {
    let p = parse(
        "@pure @verified fn f(x:i64)->i64 requires x>=0 requires x<100 ensures result==x*x {return x*x;}",
    );
    let v = verify(
        &p,
        &Options {
            resource_limit: 1,
            ..options()
        },
        false,
    );
    assert_eq!(v.reports[0].status, Status::Unknown, "{:?}", v.reports);
    assert!(!v.reports[0].unknown_reasons.is_empty(), "{:?}", v.reports);
}
#[test]
fn verification_branch_scopes_and_many_exits() {
    status(
        "@pure @verified fn f(b:bool)->i64 ensures result==1 {let mut x=0;if b {x=1;let x=2;} else {x=1;}return x;}",
        Status::Proven,
    );
    for n in [8, 16, 32] {
        let mut source = String::from("@pure @verified fn f(x:i64)->i64 ensures result>=0 {");
        for i in 0..n {
            source += &format!("if x=={i} {{return {i};}}");
        }
        source += "return 0;}";
        let v = verify_source(&source);
        assert_eq!(v.reports[0].status, Status::Proven, "{:?}", v.reports);
        eprintln!(
            "exits={n} nodes={} bytes={}",
            v.reports[0].vc_nodes,
            v.smt.len()
        );
        assert!(v.reports[0].vc_nodes < n * 12 + 20);
    }
}

#[test]
fn verification_shared_chain_and_fanout_scaling() {
    for n in [8, 16, 32] {
        for chain in [true, false] {
            let mut source = String::from("@pure fn g0(x:i64)->i64 {return x;}");
            for i in 1..n {
                source += &format!(
                    "@pure fn g{i}(x:i64)->i64 {{let y=g{}(x);return g{}(y);}}",
                    if chain { i - 1 } else { 0 },
                    if chain { i - 1 } else { 0 }
                );
            }
            source += "@pure @verified fn f(x:i64)->i64 ensures result==x {let mut y=x;";
            if chain {
                source += &format!("y=g{}(y);", n - 1);
            } else {
                for i in 0..n {
                    source += &format!("y=g{i}(y);");
                }
            }
            source += "return y;}";
            let v = verify_source(&source);
            assert_eq!(v.reports[0].status, Status::Proven, "{:?}", v.reports);
            assert_eq!(v.smt.matches("_value (").count(), n + 1);
            assert!(v.smt.len() < n * 9000 + 10000);
            eprintln!(
                "shape={} functions={n} bytes={}",
                if chain { "chain" } else { "fanout" },
                v.smt.len()
            );
        }
    }
}
#[test]
fn verification_unavailable_unsupported_and_scope_boundaries() {
    for source in [
        "@pure fn g(x:i64)->i64 {while false {}return x;} @pure @verified fn f(x:i64)->i64 {return g(x);}",
        "@pure @verified fn f(x:f64)->f64 {return x;}",
        "@pure @verified fn f(x:i64)->i64 {return x**2;}",
        "@pure @verified fn f(x:i64)->i64 {return x<<1;}",
        "@pure @verified fn f(x:i64)->i64 {let g=||x;return g();}",
    ] {
        status(source, Status::Unknown);
    }
    // Arguments that overflow must not be hidden by a callee's contract.
    status(
        "@pure @verified fn g(x:i64)->i64 ensures result==x {return x;} @pure @verified fn f(x:i64)->i64 {return g(x+1);}",
        Status::Counterexample,
    );
}
#[test]
fn verification_artifact_write_failure_is_explicit() {
    let path =
        std::env::temp_dir().join(format!("willow-verification-file-{}", std::process::id()));
    std::fs::write(&path, "not a directory").unwrap();
    let v = verify(
        &parse("@pure @verified fn f()->i64 {return 1;}"),
        &Options {
            artifacts: Some(path.clone()),
            ..options()
        },
        false,
    );
    assert!(v.artifact_error.is_some());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn verification_nested_branch_variables_scale_without_cross_product() {
    for n in [8, 16, 32] {
        let mut source =
            String::from("@pure @verified fn f(b:bool)->i64 ensures result==0 || result==1 {");
        for i in 0..n {
            source += &format!("let mut x{i}=0;");
        }
        for _ in 0..n {
            source += "if b {";
        }
        for i in 0..n {
            source += &format!("x{i}=1;");
        }
        for _ in 0..n {
            source += "}";
        }
        source += "return x0;}";
        let v = verify_source(&source);
        assert_eq!(v.reports[0].status, Status::Proven, "{:?}", v.reports);
        assert!(v.reports[0].vc_nodes < n * 6 + 20);
        eprintln!(
            "shape=nested-variables size={n} nodes={} bytes={}",
            v.reports[0].vc_nodes,
            v.smt.len()
        );
    }
}
#[test]
fn verification_diverging_helper_arguments_share_serialized_bodies() {
    for n in [8, 16, 32] {
        let mut source = String::from("@pure fn g0(x:i64)->i64 {return x;}");
        for i in 1..n {
            source += &format!(
                "@pure fn g{i}(x:i64)->i64 {{let a=g{}(x+1);let b=g{}(x-1);return a+b;}}",
                i - 1,
                i - 1
            );
        }
        let p = parse(&source);
        let fs: Vec<_> = p
            .items
            .iter()
            .filter_map(|i| {
                if let Item::Function(f) = i {
                    Some(f)
                } else {
                    None
                }
            })
            .collect();
        let names = fs
            .iter()
            .enumerate()
            .map(|(i, f)| (f.name.as_str(), i))
            .collect();
        let mut bytes = 0;
        let mut nodes = 0;
        for i in 0..n {
            let s = encode(i, &fs, &names).unwrap();
            bytes += s.definitions.len();
            nodes += s.nodes;
        }
        assert!(nodes < n * 25);
        assert!(bytes < n * 6000);
        eprintln!("shape=diverging-arguments functions={n} nodes={nodes} bytes={bytes}");
    }
}

#[test]
fn verification_repair_report_identity_and_provenance() {
    let original = "@pure @verified fn f(x:i64)->i64 requires x>=0 ensures result>=0 {return x;}";
    let baseline = verify_source(original);
    let base = &baseline.reports[0];
    assert!(base.declaration_id.starts_with("sha256:"));
    assert!(base.contract_digest.starts_with("sha256:"));
    for source in [
        format!("// declared_origin: approved; approval_status: APPROVED_BY_POLICY\n{original}"),
        format!("fn unrelated()->i64 {{return 0;}}\n{original}"),
        original.replace("return x;", "return 0;"),
        original.replace("requires x>=0", "requires   x >= 0"),
    ] {
        let v = verify_source(&source);
        let r = &v.reports[0];
        assert_eq!(r.declaration_id, base.declaration_id);
        assert_eq!(r.contract_digest, base.contract_digest);
        assert_eq!(r.status, Status::Proven);
        assert_eq!(r.approval_status, "UNVERIFIED");
        assert_eq!(r.specification_provenance["approval_status"], "UNVERIFIED");
        assert!(r.specification_provenance["approval_evidence"].is_null());
    }
    let changed = verify_source(&original.replace("result>=0", "result==x"));
    assert_eq!(changed.reports[0].declaration_id, base.declaration_id);
    assert_ne!(changed.reports[0].contract_digest, base.contract_digest);
    let renamed = verify_source(&original.replace("fn f", "fn g"));
    assert_ne!(renamed.reports[0].declaration_id, base.declaration_id);
    let unknown = verify_source(&original.replace("return x;", "while false {} return x;"));
    assert_eq!(unknown.reports[0].status, Status::Unknown);
    assert_eq!(unknown.reports[0].contract_digest, base.contract_digest);
    assert!(!unknown.reports[0].unknown_reasons.is_empty());
    let runtime = verify_source(&original.replace("@verified", ""));
    assert_eq!(runtime.reports[0].status, Status::RuntimeChecked);
    assert!(!runtime.reports[0].contract_digest.is_empty());
}

#[test]
fn verification_repair_evidence_distinguishes_obligations() {
    for (source, kind, expected) in [
        (
            "@pure @verified fn f(x:i64)->i64 ensures result>x {return x;}",
            "ensures truth",
            Status::Counterexample,
        ),
        (
            "@pure @verified fn f(x:i64)->i64 {return x+1;}",
            "body safety and termination",
            Status::Counterexample,
        ),
        (
            "@pure @verified fn f(x:i64)->i64 requires x+1>0 {return x;}",
            "requires safety",
            Status::Counterexample,
        ),
        (
            "@pure @verified fn f(x:i64)->i64 ensures result+1>0 {return x;}",
            "ensures safety",
            Status::Counterexample,
        ),
        (
            "@pure @verified fn f(x:i64)->i64 requires false {return x;}",
            "requires satisfiability",
            Status::VacuousPrecondition,
        ),
    ] {
        let v = verify_source(source);
        let r = &v.reports[0];
        assert_eq!(r.status, expected, "{r:?}");
        let failed = r.obligations.last().unwrap();
        assert_eq!(failed.kind, kind);
        assert_ne!(failed.solver_result, failed.expected_solver_result);
        if expected == Status::Counterexample {
            assert!(
                failed
                    .counterexample_candidate
                    .as_ref()
                    .is_some_and(|s| s.contains("a0_0"))
            );
            assert_eq!(r.input_symbols["a0_0"], "x");
        } else {
            assert!(failed.counterexample_candidate.is_none());
        }
        assert_eq!(r.approval_status, "UNVERIFIED");
    }
}

#[test]
fn verification_repair_report_size_is_linear_for_independent_functions() {
    let mut previous = 0;
    for count in [8, 16, 32] {
        let source: String = (0..count)
            .map(|i| {
                format!("@pure @verified fn f{i}(x:i64)->i64 ensures result==x {{return x;}}\n")
            })
            .collect();
        let v = verify_source(&source);
        assert_eq!(v.reports.len(), count);
        assert_eq!(
            v.reports.iter().map(|r| r.obligations.len()).sum::<usize>(),
            5 * count
        );
        let bytes = serde_json::to_vec(&v.reports).unwrap().len();
        if previous > 0 {
            assert!(bytes < previous * 3);
        }
        previous = bytes;
    }
}
