use super::*;
fn parse(s: &str) -> Program {
    let (p, e) =
        crate::parser::Parser::new(crate::lexer::Lexer::new(s).tokenize().unwrap()).parse();
    assert!(e.is_empty(), "{e:?}");
    p
}
fn options() -> Options {
    Options {
        artifacts: None,
        ..Default::default()
    }
}
fn snapshot(s: &str) -> Snapshot {
    capture(parse(s), &options(), &mut ProofCache::default())
}
fn result(a: &str, b: &str) -> Json {
    compare(&snapshot(a), &snapshot(b), &options())
}
fn function(pre: &str, post: &str, body: &str) -> String {
    format!("@pure fn f(x:i64)->i64 {pre} {post} {{return {body};}}")
}
#[test]
fn contract_comparison_implication_perspectives() {
    for (name, ap, aq, bp, bq, want) in [
        (
            "same",
            "",
            "ensures result==x",
            "",
            "ensures result==x",
            "PROVEN",
        ),
        (
            "weaker post",
            "",
            "ensures result==x",
            "",
            "ensures result>=0",
            "NOT_PRESERVED",
        ),
        (
            "stronger pre",
            "requires x>=0",
            "",
            "requires x>=10",
            "",
            "NOT_PRESERVED",
        ),
        (
            "weaker pre",
            "requires x>=10",
            "",
            "requires x>=0",
            "",
            "PROVEN",
        ),
        (
            "stronger post",
            "",
            "ensures result>=0",
            "",
            "ensures result==0",
            "PROVEN",
        ),
        ("remove pre", "requires x>=0", "", "", "", "PROVEN"),
        ("add pre", "", "", "requires x>=0", "", "NOT_PRESERVED"),
        (
            "remove post",
            "",
            "ensures result==x",
            "",
            "",
            "NOT_PRESERVED",
        ),
        ("add post", "", "", "", "ensures result==x", "PROVEN"),
        (
            "old pre constrains post",
            "requires x>=0",
            "ensures result>=0",
            "requires x>=0",
            "ensures result==x",
            "PROVEN",
        ),
        (
            "pre div zero",
            "",
            "",
            "requires 1/x>0",
            "",
            "NOT_PRESERVED",
        ),
        (
            "pre overflow",
            "",
            "",
            "requires x+1>x",
            "",
            "NOT_PRESERVED",
        ),
        (
            "guarded pre division",
            "requires x!=0 && 1/x==0",
            "",
            "requires x!=0 && 1/x==0",
            "",
            "PROVEN",
        ),
        (
            "post domain",
            "",
            "",
            "",
            "ensures 1/result>0",
            "NOT_PRESERVED",
        ),
        (
            "guarded post domain",
            "",
            "ensures result!=0 && 1/result==0",
            "",
            "ensures result!=0 && 1/result==0",
            "PROVEN",
        ),
    ] {
        let r = result(&function(ap, aq, "x"), &function(bp, bq, "x"));
        assert_eq!(r["changes"][0]["contract_preserved"], want, "{name}: {r}");
    }
}
#[test]
fn contract_comparison_identity_purity_and_verification() {
    let base = function("", "ensures result>=0", "0");
    for (name, target, want) in [
        ("pure removal", base.replace("@pure ", ""), "NOT_PRESERVED"),
        ("rename", base.replace("fn f", "fn g"), "NOT_PRESERVED"),
        (
            "parameter rename",
            base.replace("x:i64", "y:i64"),
            "NOT_PRESERVED",
        ),
        (
            "type change",
            "@pure fn f(x:bool)->i64 ensures result>=0 {return 0;}".into(),
            "NOT_PRESERVED",
        ),
        (
            "weak contract body change",
            base.replace("return 0", "return 1"),
            "PROVEN",
        ),
        (
            "verified addition",
            base.replace("@pure", "@pure @verified"),
            "PROVEN",
        ),
    ] {
        let r = result(&base, &target);
        assert_eq!(r["changes"][0]["contract_preserved"], want, "{name}: {r}");
        assert_eq!(r["changes"][0]["behavior_equivalent"], "UNSUPPORTED");
    }
    let r = result(&base, &base.replace("@pure", "@pure @verified"));
    assert_eq!(r["changes"][0]["implementation_verified"], true);
    let r = result(&base.replace("@pure", "@pure @verified"), &base);
    assert_eq!(r["changes"][0]["verification"]["declaration_changed"], true);
    assert_eq!(r["changes"][0]["implementation_verified"], false);
}
#[test]
fn contract_comparison_normalization_dependencies_and_determinism() {
    let a = snapshot(
        "const LIMIT:i64=1; @pure fn f(x:i64)->i64 ensures (result>=LIMIT) {return LIMIT;}",
    );
    let b = snapshot(
        "const LIMIT:i64=2; @pure fn f(x:i64)->i64 ensures (result>=LIMIT) {return LIMIT;}",
    );
    assert_eq!(
        a.lock.functions["f"].contract,
        b.lock.functions["f"].contract
    );
    assert_ne!(
        a.lock.functions["f"].semantics,
        b.lock.functions["f"].semantics
    );
    let source = "@pure fn g(x:i64)->i64 {return x;} @pure fn f(x:i64)->i64 ensures result==x {return g(x);}";
    let a = snapshot(source);
    let b = snapshot(&source.replace("return x;", "return 0;"));
    assert_ne!(
        a.lock.functions["f"].semantics,
        b.lock.functions["f"].semantics
    );
    let spaced = snapshot(&format!("// comment\n\n{source}"));
    assert_eq!(a.lock, spaced.lock);
    assert_eq!(
        compare(&a, &a, &options()),
        compare(&spaced, &spaced, &options())
    );
}
#[test]
fn contract_comparison_unknown_is_not_preserved() {
    let a = snapshot(&function("", "", "x"));
    let r = compare(
        &a,
        &a,
        &Options {
            solver: "/missing/solver".into(),
            ..options()
        },
    );
    assert_eq!(r["changes"][0]["contract_preserved"], "UNKNOWN");
    let bad = snapshot("@pure fn f(x:i64)->i64 {println(x);return x;}");
    assert!(!bad.purity_losses.is_empty());
    assert_ne!(
        compare(&a, &bad, &options())["changes"][0]["contract_preserved"],
        "PROVEN"
    );
}
#[test]
fn contract_comparison_merkle_work_scales() {
    for n in [8, 16, 32] {
        for chain in [true, false] {
            let mut source = String::from("@pure fn g0(x:i64)->i64 {return x;}");
            for i in 1..n {
                source += &format!(
                    "@pure fn g{i}(x:i64)->i64 {{return g{}(x);}}",
                    if chain { i - 1 } else { 0 }
                );
            }
            let a = snapshot(&source);
            let comparison = compare(&a, &a, &options());
            assert_eq!(comparison["work"]["comparison_queries"], 4 * n);
            assert!(comparison["work"]["predicate_bytes"].as_u64().unwrap() < (n * 2000) as u64);
            eprintln!("comparison n={n} work={}", comparison["work"]);
            assert_eq!(a.dependency_edges, n - 1);
            assert!(a.canonical_bytes < n * 2000);
            eprintln!(
                "contract normalization shape={} n={n} edges={} bytes={}",
                if chain { "chain" } else { "fanout" },
                a.dependency_edges,
                a.canonical_bytes
            );
        }
    }
}
#[test]
fn contract_proof_cache_reuse_and_invalidation() {
    let source = "@pure fn g(x:i64)->i64 {return x;} @pure @verified fn f(x:i64)->i64 ensures result==x {return g(x);}";
    let mut cache = ProofCache::default();
    let p = parse(source);
    let a = verify_cached(&p, &options(), false, &mut cache);
    assert_eq!(a.reports[0].status, Status::Proven);
    let b = verify_cached(&p, &options(), false, &mut cache);
    assert_eq!(b.reports[0].status, Status::Proven);
    assert_eq!(cache.hits, 1);
    let c = verify_cached(
        &parse(&source.replace("return x;", "return 0;")),
        &options(),
        false,
        &mut cache,
    );
    assert_eq!(c.reports[0].status, Status::Counterexample);
    assert_eq!(cache.hits, 1);
    let c = verify_cached(
        &p,
        &Options {
            resource_limit: 1,
            ..options()
        },
        false,
        &mut cache,
    );
    assert_eq!(c.reports[0].status, Status::Unknown);
    assert_eq!(cache.hits, 1);
    let c = verify_cached(
        &p,
        &Options {
            solver: "/missing/solver".into(),
            ..options()
        },
        false,
        &mut cache,
    );
    assert_eq!(c.reports[0].status, Status::Unknown);
    assert_eq!(cache.hits, 1);
    let c = verify_cached(&p, &options(), true, &mut cache);
    assert_eq!(c.reports[0].status, Status::Unknown);
    assert_eq!(cache.hits, 1);
    let changed = parse(&source.replace(
        "@pure fn g(x:i64)->i64",
        "@pure fn g(x:i64)->i64 requires x>0",
    ));
    let c = verify_cached(&changed, &options(), false, &mut cache);
    assert_ne!(c.reports[0].status, Status::Proven);
}

#[test]
fn contract_proof_cache_shared_graph_work_scales() {
    for n in [8, 16, 32] {
        let mut source = String::from("@pure fn helper(x:i64)->i64 {return x;}");
        for i in 0..n {
            source += &format!(
                "@pure @verified fn f{i}(x:i64)->i64 ensures result==x {{return helper(x);}}"
            );
        }
        let p = parse(&source);
        let mut cache = ProofCache::default();
        let first = verify_cached(&p, &options(), false, &mut cache);
        assert!(first.reports.iter().all(|r| r.status == Status::Proven));
        assert_eq!(cache.misses, n);
        let second = verify_cached(&p, &options(), false, &mut cache);
        assert!(second.reports.iter().all(|r| r.status == Status::Proven));
        assert_eq!(cache.hits, n);
        assert_eq!(cache.misses, n);
        assert_eq!(second.smt.matches("(define-fun f0_value ").count(), 1);
        eprintln!(
            "cache fanout n={n} misses={} hits={} bytes={}",
            cache.misses,
            cache.hits,
            second.smt.len()
        );
    }
}

#[test]
fn contract_comparison_invalid_budgets_and_unsupported_calls_fail_closed() {
    let a = snapshot(&function("", "", "x"));
    for o in [
        Options {
            timeout_ms: 0,
            ..options()
        },
        Options {
            resource_limit: 0,
            ..options()
        },
        Options {
            timeout_ms: 60001,
            ..options()
        },
    ] {
        assert_eq!(
            compare(&a, &a, &o)["changes"][0]["contract_preserved"],
            "UNKNOWN"
        );
    }
    let a = snapshot(
        "@pure fn g(x:i64)->i64 {return x;} @pure fn f(x:i64)->i64 ensures result==g(x) {return x;}",
    );
    let c = compare(&a, &a, &options());
    assert_eq!(
        c["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["symbol"] == "f")
            .unwrap()["contract_preserved"],
        "UNKNOWN"
    );
}

#[test]
fn contract_comparison_body_identity_is_separate_from_specification_dependencies() {
    let a = snapshot(&function("", "ensures result>=0", "0"));
    let b = snapshot(&function("", "ensures result>=0", "1"));
    assert_eq!(a.lock, b.lock);
    let c = compare(&a, &b, &options());
    assert_eq!(c["changes"][0]["contract_preserved"], "PROVEN");
    assert_eq!(c["changes"][0]["proof_dependencies_changed"], false);
    assert_eq!(c["changes"][0]["semantic_identity_changed"], true);
}
