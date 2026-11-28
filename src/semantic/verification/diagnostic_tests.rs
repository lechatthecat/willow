const VALUE_CASES: [(&str, &str, &str, &str); 25] = [
    (
        "negative",
        "@pure @verified fn f(x:i64)->i64 requires x == -1 ensures result>=0 {return x;}",
        "result>=0",
        "x = -1; result = -1",
    ),
    (
        "boolean false",
        "@pure @verified fn f(flag:bool)->bool requires !flag ensures result {return flag;}",
        "result",
        "flag = false; result = false",
    ),
    (
        "boolean true",
        "@pure @verified fn f(flag:bool)->bool requires flag ensures !result {return flag;}",
        "!result",
        "flag = true; result = true",
    ),
    (
        "only second ensures",
        "@pure @verified fn f(x:i64)->i64 requires x == -1 ensures result==x ensures result>=0 ensures result<10 {return x;}",
        "result>=0",
        "x = -1",
    ),
    (
        "only first ensures",
        "@pure @verified fn f(x:i64)->i64 requires x == -1 ensures result>=0 ensures result==x {return x;}",
        "result>=0",
        "x = -1",
    ),
    (
        "negative array",
        "@pure @verified fn f(xs:FrozenArray<i64>)->i64 requires xs.len()==1 requires xs[0]==-3 ensures result>=0 {return xs[0];}",
        "result>=0",
        "xs = [-3]",
    ),
    (
        "empty array",
        "@pure @verified fn f(xs:FrozenArray<bool>)->bool requires xs.len()==0 ensures false {return true;}",
        "false",
        "xs = []",
    ),
    (
        "bool array",
        "@pure @verified fn f(xs:FrozenArray<bool>)->bool requires xs.len()==2 requires xs[0] requires !xs[1] ensures false {return true;}",
        "false",
        "xs = [true, false]",
    ),
    (
        "record",
        "class Point {pub x:i64;} @pure @verified fn f(p:Point)->i64 requires p.x == -4 ensures result>=0 {return p.x;}",
        "result>=0",
        "p = new Point(-4)",
    ),
    (
        "nested record",
        "class Point {pub x:i64;} class Box {pub p:Point;} @pure @verified fn f(b:Box)->i64 requires b.p.x == -5 ensures result>=0 {return b.p.x;}",
        "result>=0",
        "b = new Box(new Point(-5))",
    ),
    (
        "enum payload",
        "enum Choice {Yes(i64),No} @pure @verified fn f(c:Choice)->i64 requires match c {Choice::Yes(n)=>n == -6,Choice::No=>false} ensures result>=0 {return match c {Choice::Yes(n)=>n,Choice::No=>0};}",
        "result>=0",
        "c = Choice::Yes(-6)",
    ),
    (
        "enum empty",
        "enum Choice {Yes(i64),No} @pure @verified fn f(c:Choice)->i64 requires match c {Choice::No=>true,_=>false} ensures result>0 {return 0;}",
        "result>0",
        "c = Choice::No",
    ),
    (
        "same named variant",
        "enum Choice {Choice,Other} @pure @verified fn f(c:Choice)->i64 requires match c {Choice::Choice=>true,_=>false} ensures false {return 0;}",
        "false",
        "c = Choice::Choice",
    ),
    (
        "requires safety",
        "@pure @verified fn f(x:i64)->i64 requires 10/x>0 {return 1;}",
        "10/x",
        "x = 0",
    ),
    (
        "body division",
        "@pure @verified fn f(x:i64)->i64 requires x==0 {return 10/x;}",
        "10/x",
        "x = 0",
    ),
    (
        "overflow",
        "@pure @verified fn f(x:i64)->i64 requires x==9223372036854775807 {return x+1;}",
        "x+1",
        "x = 9223372036854775807",
    ),
    (
        "index",
        "@pure @verified fn f(xs:FrozenArray<i64>)->i64 requires xs.len()==0 {return xs[0];}",
        "xs[0]",
        "xs = []",
    ),
    (
        "ensures safety",
        "@pure @verified fn f(x:i64)->i64 requires x==0 ensures 1/result>0 {return x;}",
        "1/result",
        "x = 0",
    ),
    (
        "call requires",
        "@pure fn positive(x:i64)->i64 requires x>0 {return x;} @pure @verified fn f(x:i64)->i64 requires x==0 {return positive(x);}",
        "positive(x)",
        "x = 0",
    ),
    (
        "invariant entry",
        "@pure @verified fn f(x:i64)->i64 requires x == -1 {let mut n=x; while n>0 invariant n>=0 variant n {n=n-1;}return n;}",
        "n>=0",
        "x = -1",
    ),
    (
        "second invariant",
        "@pure @verified fn f(x:i64)->i64 requires x == -1 {let mut n=x; while n>0 invariant n<=10 invariant n>=0 variant n {n=n-1;}return n;}",
        "n>=0",
        "x = -1",
    ),
    (
        "invariant preservation",
        "@pure @verified fn f(x:i64)->i64 requires x==2 {let mut n=x; while n>0 invariant n>=0 variant n {n=n-2;}return n;}",
        "n>=0",
        "x = 2",
    ),
    (
        "variant",
        "@pure @verified fn f(x:i64)->i64 requires x==1 {let mut n=x; while n>0 invariant n>=0 variant -n {n=n-1;}return n;}",
        "-n",
        "x = 1",
    ),
    (
        "no inputs",
        "@pure @verified fn f()->i64 ensures result>0 {return -1;}",
        "result>0",
        "result = -1",
    ),
    (
        "unreachable safety",
        "@pure @verified fn f(x:i64)->i64 requires x == -1 ensures result>=0 {if false {return 1/0;} return x;}",
        "result>=0",
        "x = -1",
    ),
];
use super::tests::{options, parse};
use super::*;

fn checked(source: &str) -> (Vec<Diagnostic>, Verification) {
    let program = parse(source);
    let writes = crate::semantic::field_writes::FieldWrites::from_program(&program);
    check_with_data(
        &program,
        &[],
        &options(),
        &mut ProofCache::default(),
        &DataTypes::new(&program, &writes),
        None,
    )
}

#[test]
fn source_diagnostics_values_and_locations() {
    // Each row is a separate acceptance perspective, with an exact source span
    // and a model constrained enough to make the displayed value deterministic.
    for (name, source, failed, value) in VALUE_CASES {
        let (errors, verification) = checked(source);
        let report = verification
            .reports
            .iter()
            .find(|r| r.function == "f")
            .unwrap();
        assert_eq!(report.status, Status::Counterexample, "{name}: {errors:?}");
        let display = report
            .obligations
            .last()
            .unwrap()
            .presentation
            .as_ref()
            .unwrap();
        assert_eq!(display.incomplete, None, "{name}: {display:?}");
        assert_eq!(display.evidence, "CANDIDATE");
        assert!(display.text().contains(value), "{name}: {}", display.text());
        let error = errors.iter().find(|d| d.code == ErrorCode::E2630).unwrap();
        assert_eq!(error.labels.len(), 1, "{name}");
        let span = error.labels[0].span;
        let start = source.rfind(failed).unwrap();
        assert!(
            span.start >= start && span.end <= start + failed.len(),
            "{name}: {error:?}; expected within {failed}"
        );
        assert!(error.message.contains("candidate inputs"), "{name}");
        if failed != "result>=0" && report.obligations.last().unwrap().kind != "ensures truth" {
            assert!(display.result.is_none(), "unsafe result: {name}");
        }
    }
}

#[test]
fn source_diagnostics_prior_errors_and_solver_unknown() {
    for source in [
        "@pure @verified fn f()->i64 {return true;}",
        "@pure @verified fn f()->i64 {println(1);return 1;}",
        "@effects(no_alloc) @pure @verified fn f()->i64 {let xs=[1];return xs[0];}",
    ] {
        let mut checker = crate::semantic::TypeChecker::new();
        checker.verification_options = Some(options());
        checker.check_program(&parse(source));
        assert!(checker.errors.iter().any(|d| d.severity == Severity::Error));
        assert!(
            !checker.errors.iter().any(|d| d.code == ErrorCode::E2631),
            "{:?}",
            checker.errors
        );
        assert!(
            checker
                .verification
                .reports
                .iter()
                .all(|r| r.status != Status::Proven)
        );
    }
    let (errors, _) = checked("@pure @verified fn f()->i64 {while false {} return 0;}");
    assert!(errors.iter().any(|d| d.code == ErrorCode::E2631));
    let program = parse(
        "@pure @verified fn f(x:i64)->i64 requires x>=0 && x<100 ensures result==x*x {return x*x;}",
    );
    let writes = crate::semantic::field_writes::FieldWrites::from_program(&program);
    let (errors, result) = check_with_data(
        &program,
        &[],
        &Options {
            resource_limit: 1,
            ..options()
        },
        &mut ProofCache::default(),
        &DataTypes::new(&program, &writes),
        None,
    );
    assert_eq!(result.reports[0].status, Status::Unknown);
    assert!(errors.iter().any(|d| d.code == ErrorCode::E2632));
}

#[test]
fn source_diagnostics_json_and_moved_source_cache() {
    let source = "@pure @verified fn f(x:i64)->i64 requires x == -1 ensures result>=0 {return x;}";
    let (_, result) = checked(source);
    let json = serde_json::to_value(&result.reports[0]).unwrap();
    assert_eq!(json["schema"], "willow.verification.v1");
    assert_eq!(json["status"], "COUNTEREXAMPLE");
    let obligation = json["obligations"].as_array().unwrap().last().unwrap();
    for key in [
        "kind",
        "solver_result",
        "expected_solver_result",
        "counterexample_candidate",
    ] {
        assert!(obligation.get(key).is_some(), "{key}");
    }
    assert!(
        obligation["counterexample_candidate"]
            .as_str()
            .unwrap()
            .contains("(- 1)")
    );
    assert_eq!(
        obligation["presentation"]["schema"],
        "willow.verification.presentation.v1"
    );
    assert_eq!(obligation["presentation"]["inputs"][0]["name"], "x");
    assert_eq!(obligation["presentation"]["inputs"][0]["value"], "-1");
    let mut cache = ProofCache::default();
    let good = source.replace("result>=0", "result==x");
    assert_eq!(
        verify_cached(&parse(&good), &options(), false, &mut cache).reports[0].status,
        Status::Proven
    );
    let shifted = format!("\n\n{good}");
    assert_eq!(
        verify_cached(&parse(&shifted), &options(), false, &mut cache).reports[0].status,
        Status::Proven
    );
    assert_eq!(cache.hits, 1);
    let bad = format!("\n\n{source}");
    let result = verify_cached(&parse(&bad), &options(), false, &mut cache);
    assert_eq!(
        result.reports[0]
            .obligations
            .last()
            .unwrap()
            .presentation
            .as_ref()
            .unwrap()
            .location
            .unwrap()
            .line,
        3
    );
}

#[test]
fn source_diagnostics_linear_selector_counts() {
    let mut previous = 0;
    for n in [8, 16, 32, 64] {
        let source = format!(
            "@pure @verified fn f(x:i64)->i64 {} {{return x;}}",
            "ensures result==x ".repeat(n)
        );
        let program = parse(&source);
        let Item::Function(f) = &program.items[0] else {
            panic!()
        };
        let summary = encode(0, &[f], &HashMap::from([("f", 0)]), &DataTypes::default()).unwrap();
        assert_eq!(
            summary.sites.iter().filter(|s| s.kind == "ensures").count(),
            n
        );
        assert_eq!(summary.definitions.matches("_failure_").count(), 4);
        assert!(summary.definitions.len() < 5000 * n + 10000);
        if previous > 0 {
            assert!(summary.definitions.len() < previous * 3);
        }
        previous = summary.definitions.len();
        eprintln!(
            "ensures={n} sites={} nodes={} bytes={}",
            summary.sites.len(),
            summary.nodes,
            summary.definitions.len()
        );
    }
}

#[test]
fn source_diagnostics_vacuity_and_bounded_display() {
    let source = "@pure @verified fn f(x:i64)->i64 requires x>0 requires x<0 {return x;}";
    let (errors, result) = checked(source);
    assert_eq!(result.reports[0].status, Status::VacuousPrecondition);
    assert!(!errors[0].message.contains("candidate inputs"));
    assert_eq!(
        &source[errors[0].labels[0].span.start..errors[0].labels[0].span.end],
        "x>0 requires x<0"
    );
    let source = "@pure @verified fn f(xs:FrozenArray<i64>)->i64 requires xs.len()==40 ensures false {return 0;}";
    let (_, result) = checked(source);
    let display = result.reports[0]
        .obligations
        .last()
        .unwrap()
        .presentation
        .as_ref()
        .unwrap();
    assert!(display.inputs[0].value.ends_with("… (length = 40)]"));
    assert_eq!(display.evidence, "CANDIDATE");
    let source = "@pure @verified fn f()->bool {return forall(0,3,|i:i64| 1/i>0);}";
    let (errors, result) = checked(source);
    assert_eq!(result.reports[0].status, Status::Counterexample);
    let span = errors[0].labels[0].span;
    assert!(source[span.start..span.end].contains("1/i"), "{errors:?}");
}

#[test]
fn source_diagnostics_datatype_projection_counts() {
    for n in [8, 16, 32] {
        let mut source = String::from("enum Choice {");
        for i in 0..n {
            source += &format!("C{i},");
        }
        source += "} @pure @verified fn f(";
        for i in 0..n {
            if i > 0 {
                source.push(',');
            }
            source += &format!("p{i}:Choice");
        }
        source += ")->i64 ensures false {return 0;}";
        let (_, result) = checked(&source);
        assert_eq!(result.reports[0].status, Status::Counterexample);
        let display = result.reports[0]
            .obligations
            .last()
            .unwrap()
            .presentation
            .as_ref()
            .unwrap();
        assert_eq!(display.inputs.len(), n);
        assert!(display.incomplete.is_none());
        assert_eq!(result.smt.matches("(define-fun tag_").count(), 1);
        assert_eq!(result.smt.matches("(get-value ((tag_").count(), n);
        eprintln!(
            "enum variants={n} inputs={n} tag_definitions=1 tag_queries={n} bytes={}",
            result.smt.len()
        );
    }
}

#[test]
fn source_diagnostics_empty_module_artifact_failure() {
    // A file cannot serve as an artifact directory, on any supported OS.
    let options = Options {
        artifacts: Some(std::env::current_exe().unwrap()),
        ..options()
    };
    let (errors, result) = check_with_data(
        &parse(""),
        &[],
        &options,
        &mut ProofCache::default(),
        &DataTypes::default(),
        Some(&mut modular::World::default()),
    );
    assert!(result.artifact_error.is_some());
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].code, ErrorCode::E2632);
    assert_eq!(errors[0].labels[0].span, Span::dummy());
}

#[test]
fn source_diagnostics_explicit_constructors_are_field_values() {
    for (init, argument) in [
        ("pub init(self,b:bool) {self.x=-4;}", "true"),
        ("pub init(self,x:i64) {self.x=0;}", "7"),
        ("pub init(self,x:i64) {self.x=x;}", "-4"),
    ] {
        let declarations = format!("class Point {{pub x:i64; {init}}}");
        let source = format!(
            "{declarations} @pure @verified fn f(p:Point)->i64 requires p.x == -4 ensures result>=0 {{return p.x;}}"
        );
        let (errors, result) = checked(&source);
        assert_eq!(
            result.reports[0].status,
            Status::Counterexample,
            "{errors:?}"
        );
        let display = result.reports[0]
            .obligations
            .last()
            .unwrap()
            .presentation
            .as_ref()
            .unwrap();
        assert_eq!(display.inputs[0].value, "Point { x: -4 }");
        assert_eq!(display.evidence, "CANDIDATE");
        assert!(
            display
                .incomplete
                .as_ref()
                .unwrap()
                .contains("construction unavailable (explicit constructor has not been evaluated)")
        );
        assert!(errors[0].message.contains("p = Point { x: -4 }"));
        assert!(!errors[0].message.contains("new Point("));
        // Confirm the constructor actually has the stated source signature.
        let mut checker = crate::semantic::TypeChecker::new();
        checker.check_program(&parse(&format!(
            "{declarations} fn main() {{let p=new Point({argument});}}"
        )));
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
    }
    let source = "class Point {pub x:i64; pub init(self,b:bool) {self.x=-4;}} class Box {pub p:Point;} @pure @verified fn f(b:Box)->i64 requires b.p.x == -4 ensures result>=0 {return b.p.x;}";
    let (errors, result) = checked(source);
    let display = result.reports[0]
        .obligations
        .last()
        .unwrap()
        .presentation
        .as_ref()
        .unwrap();
    assert_eq!(display.inputs[0].value, "Box { p: Point { x: -4 } }");
    assert!(
        display
            .incomplete
            .as_ref()
            .unwrap()
            .contains("Box: construction unavailable")
    );
    assert!(!errors[0].message.contains("new Box("));
}

#[test]
fn source_diagnostics_implicit_constructor_values_typecheck() {
    for (declarations, parameter, requires, expected) in [
        (
            "class Point {pub x:i64; pub flag:bool;}",
            "Point",
            "p.x == -4 && p.flag",
            "new Point(-4, true)",
        ),
        (
            "class Values {pub xs:FrozenArray<i64>;}",
            "Values",
            "p.xs.len()==1 && p.xs[0]==-4",
            "new Values([-4].freeze())",
        ),
        (
            "class Values {pub xs:FrozenArray<FrozenArray<i64>>;}",
            "Values",
            "p.xs.len()==1 && p.xs[0].len()==1 && p.xs[0][0]==-4",
            "new Values([[-4].freeze()].freeze())",
        ),
    ] {
        let source = format!(
            "{declarations} @pure @verified fn f(p:{parameter})->i64 requires {requires} ensures false {{return 0;}}"
        );
        let (_, result) = checked(&source);
        assert_eq!(
            result.reports[0].status,
            Status::Counterexample,
            "{:?}",
            result.reports
        );
        let display = result.reports[0]
            .obligations
            .last()
            .unwrap()
            .presentation
            .as_ref()
            .unwrap();
        assert_eq!(display.inputs[0].value, expected);
        assert!(display.incomplete.is_none());
        let mut checker = crate::semantic::TypeChecker::new();
        checker.check_program(&parse(&format!(
            "{declarations} fn main() {{let p={expected};}}"
        )));
        assert!(checker.errors.is_empty(), "{:?}", checker.errors);
    }
}

#[test]
fn source_diagnostics_escaped_string_values_preserve_contracts() {
    let source =
        r#"@pure @verified fn f(s:String)->String requires s == "\e" ensures false {return s;}"#;
    let (_, result) = checked(source);
    let display = result.reports[0]
        .obligations
        .last()
        .unwrap()
        .presentation
        .as_ref()
        .unwrap();
    assert_eq!(display.inputs[0].value, r#""\e""#);
    let reproduced = format!(
        r#"@pure @verified fn f()->String ensures result == "\e" {{return {};}}"#,
        display.inputs[0].value
    );
    let (errors, result) = checked(&reproduced);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(result.reports[0].status, Status::Proven);
    for status in [
        Status::Proven,
        Status::Counterexample,
        Status::VacuousPrecondition,
        Status::Unknown,
        Status::RuntimeChecked,
    ] {
        assert_eq!(serde_json::to_value(&status).unwrap(), status.as_str());
    }
}
