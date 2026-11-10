use super::*;

fn parse(source: &str) -> Program {
    let (program, errors) =
        crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty(), "{errors:?}\n{source}");
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

#[test]
fn verified_data_proof_perspectives() {
    use Status::*;
    let cases = [
        (
            "array length",
            "@pure @verified fn f(a:FrozenArray<i64>)->i64 ensures result>=0 {return a.len();}",
            Proven,
        ),
        (
            "array read",
            "@pure @verified fn f(a:FrozenArray<i64>,i:i64)->i64 requires i>=0 && i<a.len() ensures result==a[i] {return a[i];}",
            Proven,
        ),
        (
            "bad array contract",
            "@pure @verified fn f(a:FrozenArray<i64>)->i64 requires a.len()>0 ensures result==7 {return a[0];}",
            Counterexample,
        ),
        (
            "missing array bounds",
            "@pure @verified fn f(a:FrozenArray<i64>)->i64 {return a[0];}",
            Counterexample,
        ),
        (
            "negative index",
            "@pure @verified fn f(a:FrozenArray<i64>)->i64 {return a[-1];}",
            Counterexample,
        ),
        (
            "boolean elements",
            "@pure @verified fn f(a:FrozenArray<bool>)->bool requires a.len()>0 ensures result==a[0] {return a[0];}",
            Proven,
        ),
        (
            "mutable array",
            "@pure @verified fn f(a:Array<i64>)->i64 {return a[0];}",
            Unknown,
        ),
        (
            "record field",
            "class Point {pub x:i64;} @pure @verified fn f(p:Point)->i64 ensures result==p.x {return p.x;}",
            Proven,
        ),
        (
            "record false contract",
            "class Point {pub x:i64;} @pure @verified fn f(p:Point)->i64 ensures result==0 {return p.x;}",
            Counterexample,
        ),
        (
            "nested record",
            "class Point {pub x:i64;} class Boxed {pub p:Point;} @pure @verified fn f(p:Boxed)->i64 ensures result==p.p.x {return p.p.x;}",
            Proven,
        ),
        (
            "mutable record",
            "class Point {pub x:i64;} fn change(p:Point){p.x=1;} @pure @verified fn f(p:Point)->i64 {return p.x;}",
            Unknown,
        ),
        (
            "enum payload",
            "enum Choice { Yes(i64), No } @pure @verified fn f(c:Choice)->i64 ensures result==match c {Choice::Yes(x)=>x, Choice::No=>0} {return match c {Choice::Yes(x)=>x,Choice::No=>0};}",
            Proven,
        ),
        (
            "enum false",
            "enum Choice { Yes(i64), No } @pure @verified fn f(c:Choice)->i64 ensures result==0 {return match c {Choice::Yes(x)=>x,Choice::No=>0};}",
            Counterexample,
        ),
        (
            "universal elements",
            "@pure @verified fn f(a:FrozenArray<i64>,i:i64)->i64 requires forall(0,a.len(),|j:i64| a[j]>=0) requires i>=0 && i<a.len() ensures result>=0 {return a[i];}",
            Proven,
        ),
        (
            "existential",
            "@pure @verified fn f(a:FrozenArray<i64>)->bool requires a.len()>0 ensures exists(0,a.len(),|j:i64| a[j]==a[0]) {return true;}",
            Proven,
        ),
        (
            "empty universal",
            "@pure @verified fn f()->bool ensures result {return forall(0,0,|j:i64| false);}",
            Proven,
        ),
        (
            "empty existential",
            "@pure @verified fn f()->bool ensures !result {return exists(3,0,|j:i64| true);}",
            Proven,
        ),
        (
            "quantifier false",
            "@pure @verified fn f()->bool ensures result {return forall(0,3,|j:i64| j<2);}",
            Counterexample,
        ),
        (
            "quantifier safety",
            "@pure @verified fn f()->bool {return forall(0,3,|j:i64| 1/j>0);}",
            Counterexample,
        ),
        (
            "nested quantifier",
            "@pure @verified fn f()->bool ensures result {return forall(0,3,|i:i64| exists(0,3,|j:i64| i==j));}",
            Proven,
        ),
        (
            "for induction",
            "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures result==n {let mut x=0;for i in 0..n invariant i>=0 && i<=n invariant x==i variant n-i {x=x+1;}return x;}",
            Proven,
        ),
        (
            "for continue",
            "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures result==n {let mut x=0;for i in 0..n invariant i>=0 && i<=n invariant x==i variant n-i {x=x+1;continue;}return x;}",
            Proven,
        ),
        (
            "while break",
            "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures result==n {let mut x=n;while x>0 invariant x==n variant x {break;}return x;}",
            Proven,
        ),
        (
            "while continue decreases",
            "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures result==0 {let mut x=n;while x>0 invariant x>=0 variant x {x=x-1;continue;}return x;}",
            Proven,
        ),
        (
            "continue stutters",
            "@pure @verified fn f(n:i64)->i64 requires n>=0 {let mut x=n;while x>0 invariant x>=0 variant x {continue;}return x;}",
            Counterexample,
        ),
        (
            "frozen array for",
            "@pure @verified fn f(a:FrozenArray<i64>)->i64 ensures result==a.len() {let mut count=0; for value in a invariant count==iteration invariant count>=0 && count<=a.len() variant a.len()-count {count=count+1;continue;} return count;}",
            Proven,
        ),
        (
            "for missing annotations",
            "@pure @verified fn f()->i64 {for i in 0..1 {}return 0;}",
            Unknown,
        ),
    ];
    for (name, source, expected) in cases {
        let v = prove(source);
        let r = v.reports.last().unwrap();
        assert_eq!(r.status, expected, "{name}: {:?}\n{}", v.reports, v.smt);
        if expected == Counterexample && !r.input_names.is_empty() {
            assert!(
                r.obligations
                    .iter()
                    .any(|o| o.counterexample_candidate.is_some()),
                "{name}"
            );
        }
        if expected == Unknown {
            assert_eq!(r.diagnostic_code, ErrorCode::E2631, "{name}");
        }
    }
}

#[test]
fn verified_data_checked_frontend_and_comparison() {
    let source = "class Point {pub x:i64;} enum Choice {Yes(i64), No} @pure @verified fn f(a:FrozenArray<i64>,p:Point,c:Choice)->i64 requires a.len()>0 requires forall(0,a.len(),|i:i64| a[i]>=p.x) ensures result>=p.x {return a[0];}";
    let options = Options {
        artifacts: None,
        ..Options::default()
    };
    let mut cache = ProofCache::default();
    let base = comparison::capture(parse(source), &options, &mut cache);
    assert!(base.errors.is_empty(), "{:?}", base.errors);
    assert_eq!(
        base.verification.reports.last().unwrap().status,
        Status::Proven,
        "{:?}",
        base.verification.reports
    );
    let same = comparison::capture(parse(&format!("\n{source}")), &options, &mut cache);
    assert_eq!(base.lock, same.lock);
    let preserved = comparison::compare(&base, &same, &options);
    assert_eq!(
        preserved["changes"][0]["contract_preserved"], "PROVEN",
        "{preserved}"
    );
    let weak = comparison::capture(
        parse(&source.replace("result>=p.x", "true")),
        &options,
        &mut cache,
    );
    let changed = comparison::compare(&base, &weak, &options);
    assert_eq!(
        changed["changes"][0]["contract_preserved"], "NOT_PRESERVED",
        "{changed}"
    );
    let record = comparison::capture(
        parse(&source.replace("pub x:i64;", "pub x:i64; pub y:bool;")),
        &options,
        &mut cache,
    );
    assert_ne!(base.lock, record.lock);
    let changed = comparison::compare(&base, &record, &options);
    assert_eq!(
        changed["changes"][0]["contract_preserved"], "NOT_PRESERVED",
        "{changed}"
    );
}

#[test]
fn verified_data_unknown_and_quantifier_scopes() {
    let source = "@pure @verified fn f(a:FrozenArray<i64>)->bool ensures result {return forall(0,a.len(),|i:i64| exists(0,a.len(),|j:i64| i==j));}";
    let options = Options {
        artifacts: None,
        ..Options::default()
    };
    let v = verify(
        &parse(source),
        &Options {
            resource_limit: 1,
            ..options
        },
        false,
    );
    assert_eq!(v.reports[0].status, Status::Unknown);
    assert_eq!(v.reports[0].diagnostic_code, ErrorCode::E2632);
    for source in [
        "@pure @verified fn f()->bool ensures result {return forall(0,3,|i:i64| forall(0,3,|j:i64| i==j));}",
        "@pure @verified fn f()->bool ensures result {return forall(0,3,|i:i64| exists(0,1,|j:i64| i==j));}",
    ] {
        assert_eq!(prove(source).reports[0].status, Status::Counterexample);
    }
}

#[test]
fn verified_data_scaling_counts() {
    for size in [8, 16, 32] {
        let mut source =
            String::from("@pure @verified fn f(a:FrozenArray<i64>)->i64 requires a.len()>0 ");
        for _ in 0..size {
            source.push_str("requires forall(0,a.len(),|i:i64| a[i]>=0) ");
        }
        source.push_str("ensures result>=0 {return a[0];}");
        let p = parse(&source);
        let model = DataTypes::new(
            &p,
            &crate::semantic::field_writes::FieldWrites::from_program(&p),
        );
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
        let names = HashMap::from([("f", 0)]);
        let summary = encode(0, &fs, &names, &model).unwrap();
        assert_eq!(model.visits, 1);
        assert!(summary.nodes < 30 * size + 50, "{}", summary.nodes);
        assert!(
            summary.definitions.len() < 20_000 * size + 20_000,
            "{}",
            summary.definitions.len()
        );
        eprintln!(
            "quantifiers={size} types={} nodes={} bytes={}",
            model.visits,
            summary.nodes,
            summary.definitions.len()
        );
        let mut predicate = "true".to_owned();
        for index in 0..size {
            predicate = format!("forall(0,2,|i{index}:i64| {predicate})");
        }
        let p = parse(&format!(
            "@pure @verified fn f()->bool {{return {predicate};}}"
        ));
        let Item::Function(f) = &p.items[0] else {
            panic!()
        };
        let summary = encode(0, &[f], &names, &DataTypes::default()).unwrap();
        assert!(
            summary.definitions.len() < 5_000 * size + 10_000,
            "nested {size}: {}",
            summary.definitions.len()
        );
        assert!(summary.assembly_bytes < 1_000 * size + 5_000);
        eprintln!(
            "quantifier depth={size} nodes={} bytes={} assembly_bytes={}",
            summary.nodes,
            summary.definitions.len(),
            summary.assembly_bytes,
        );
    }
}

#[test]
fn verified_data_adverse_shapes_and_exit_counts() {
    for n in [8, 16, 32] {
        let mut source = String::from("class Leaf {pub x:i64;} ");
        for i in 0..n {
            let child = if i == 0 {
                "Leaf".into()
            } else {
                format!("Node{}", i - 1)
            };
            source += &format!("class Node{i} {{pub child:{child};}}");
        }
        for i in 0..n {
            source += &format!(
                "@pure @verified fn f{i}(x:Node{})->i64 ensures result==0 {{return 0;}}",
                n - 1
            );
        }
        let p = parse(&source);
        let model = DataTypes::new(
            &p,
            &crate::semantic::field_writes::FieldWrites::from_program(&p),
        );
        assert_eq!(model.visits, n + 1);
        assert_eq!(model.constructors.len(), n + 1);
        eprintln!(
            "record chain={n} callers={n} type_visits={} data_bytes={}",
            model.visits,
            model.definitions.len()
        );
        let mut source = String::from("enum Choice {");
        for i in 0..n {
            source += &format!("V{i}(i64),");
        }
        source += "} @pure @verified fn f(x:Choice)->i64 {return match x {";
        for i in 0..n {
            source += &format!("Choice::V{i}(v)=>v,");
        }
        source += "};}";
        let p = parse(&source);
        let model = DataTypes::new(
            &p,
            &crate::semantic::field_writes::FieldWrites::from_program(&p),
        );
        let Item::Function(f) = &p.items[1] else {
            panic!()
        };
        let summary = encode(0, &[f], &HashMap::from([("f", 0)]), &model).unwrap();
        assert!(summary.nodes < n * 6 + 20);
        assert!(summary.definitions.len() < n * 9_000 + 10_000);
        eprintln!(
            "enum variants={n} nodes={} bytes={}",
            summary.nodes,
            summary.definitions.len()
        );
        let mut source = String::from(
            "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures result>=0 {let mut x=n;while x>0 invariant x>=0 variant x {",
        );
        for i in 0..n {
            source += &format!("if x=={} {{break;}}", i + 1);
        }
        source += "x=x-1;continue;}return x;}";
        let p = parse(&source);
        let Item::Function(f) = &p.items[0] else {
            panic!()
        };
        let summary = encode(0, &[f], &HashMap::from([("f", 0)]), &DataTypes::default()).unwrap();
        assert!(summary.nodes < n * 12 + 100);
        eprintln!(
            "loop breaks={n} nodes={} bytes={}",
            summary.nodes,
            summary.definitions.len()
        );
    }
}

#[test]
fn verified_data_iteration_binding_and_nested_breaks() {
    for source in [
        "@pure @verified fn f(a:FrozenArray<i64>)->i64 requires a.len()<=100 ensures result==a.len()*9 {let iteration=9;let mut total=0;for x in a invariant total==iteration*9 invariant iteration>=0 && iteration<=a.len() variant a.len()-iteration {total=total+iteration;}return total;}",
        "@pure @verified fn f(n:i64)->i64 requires n>=0 ensures result==0 {let mut x=n;while x>0 invariant x>=0 variant x {let mut y=x;while y>0 invariant y==x variant y {break;}x=x-1;continue;}return x;}",
    ] {
        let v = prove(source);
        assert_eq!(v.reports[0].status, Status::Proven, "{:?}", v.reports);
    }
    let wrong = "@pure @verified fn f(a:FrozenArray<i64>)->i64 requires a.len()>0 requires a.len()<=100 ensures result==0 {let iteration=9;let mut total=0;for x in a invariant total==iteration*9 invariant iteration>=0 && iteration<=a.len() variant a.len()-iteration {total=total+iteration;}return total;}";
    assert_eq!(prove(wrong).reports[0].status, Status::Counterexample);
}

#[test]
fn verified_data_replay_determinism() {
    let source = "@pure @verified fn f(a:FrozenArray<i64>)->i64 ensures result==a.len() {let mut count=0;for value in a invariant count==iteration invariant count>=0 && count<=a.len() variant a.len()-count {count=count+1;}return count;}";
    let first = prove(source);
    let second = prove(source);
    assert_eq!(first.reports[0].status, Status::Proven);
    assert_eq!(first.smt, second.smt);
    assert_eq!(
        first.reports[0].input_symbols,
        second.reports[0].input_symbols
    );
    let mut solver = solver::Solver::new(&Options::default().solver, 5000).unwrap();
    assert!(!solver.exchange(&first.smt).unwrap().contains("unknown"));
}
