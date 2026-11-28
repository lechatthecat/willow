use super::*;

fn parse(source: &str) -> Program {
    let (program, errors) =
        crate::parser::Parser::new(crate::lexer::Lexer::new(source).tokenize().unwrap()).parse();
    assert!(errors.is_empty(), "{errors:?}\n{source}");
    program
}
fn options() -> Options {
    Options {
        artifacts: None,
        ..Options::default()
    }
}

#[test]
fn verified_string_proof_perspectives() {
    use Status::*;
    let cases = [
        (
            "identity",
            r#"@pure @verified fn f(s:String)->String ensures result==s {return s;}"#,
            Proven,
        ),
        (
            "nonnegative bytes",
            r#"@pure @verified fn f(s:String)->i64 ensures result>=0 {return s.len();}"#,
            Proven,
        ),
        (
            "empty bytes",
            r#"@pure @verified fn f()->i64 ensures result==0 {return "".len();}"#,
            Proven,
        ),
        (
            "ascii bytes",
            r#"@pure @verified fn f()->i64 ensures result==3 {return "abc".len();}"#,
            Proven,
        ),
        (
            "unicode bytes",
            r#"@pure @verified fn f()->i64 ensures result==7 {return "雪🌲".len();}"#,
            Proven,
        ),
        (
            "nul bytes",
            r#"@pure @verified fn f()->i64 ensures result==3 {return "a\0b".len();}"#,
            Proven,
        ),
        (
            "escape bytes",
            r#"@pure @verified fn f()->i64 ensures result==3 {return "\"\\\n".len();}"#,
            Proven,
        ),
        (
            "equal lengths",
            r#"@pure @verified fn f(s:String,t:String)->bool requires s==t ensures result {return s.len()==t.len();}"#,
            Proven,
        ),
        (
            "empty equivalent",
            r#"@pure @verified fn f(s:String)->bool ensures result {return (s=="")== (s.len()==0);}"#,
            Proven,
        ),
        (
            "route match",
            r#"@pure @verified fn f(s:String)->bool ensures result==(s=="/health") {return s=="/health";}"#,
            Proven,
        ),
        (
            "inequality",
            r#"@pure @verified fn f(s:String)->bool requires s!="" ensures result {return s!="";}"#,
            Proven,
        ),
        (
            "fallback",
            r#"@pure @verified fn f(s:String)->String ensures result.len()>0 {return s=="" ? "unknown" : s;}"#,
            Proven,
        ),
        (
            "local rebinding",
            r#"@pure @verified fn f(s:String)->String ensures result=="ok" {let mut x=s;x="ok";return x;}"#,
            Proven,
        ),
        (
            "branch rebinding",
            r#"@pure @verified fn f(s:String,b:bool)->String ensures result==(b ? "ok" : s) {let mut x=s;if b {x="ok";}return x;}"#,
            Proven,
        ),
        (
            "immutable record",
            r#"class Label {pub text:String;} @pure @verified fn f(s:Label)->String ensures result==s.text {return s.text;}"#,
            Proven,
        ),
        (
            "enum payload",
            r#"enum Label {Text(String), Empty} @pure @verified fn f(s:Label)->String ensures result==match s {Label::Text(t)=>t,Label::Empty=>""} {return match s {Label::Text(t)=>t,Label::Empty=>""};}"#,
            Proven,
        ),
        (
            "frozen element",
            r#"@pure @verified fn f(a:FrozenArray<String>)->String requires a.len()>0 ensures result==a[0] {return a[0];}"#,
            Proven,
        ),
        (
            "quantified lengths",
            r#"@pure @verified fn f(a:FrozenArray<String>)->bool ensures result {return forall(0,a.len(),|i:i64|a[i].len()>=0);}"#,
            Proven,
        ),
        (
            "verified helper",
            r#"@pure @verified fn id(s:String)->String ensures result==s {return s;} @pure @verified fn f(s:String)->String ensures result==s {return id(s);}"#,
            Proven,
        ),
        (
            "literal-only return",
            r#"@pure @verified fn f()->String ensures result=="ok" {return "ok";}"#,
            Proven,
        ),
        (
            "loop string state",
            r#"@pure @verified fn f(n:i64,s:String)->String requires n>=0 ensures result==s {let mut text=s;for i in 0..n invariant text==s {text=s;}return text;}"#,
            Proven,
        ),
        (
            "length arithmetic",
            r#"@pure @verified fn f(s:String)->i64 requires s.len()>0 ensures result>=0 {return s.len()-1;}"#,
            Proven,
        ),
        (
            "false literal",
            r#"@pure @verified fn f()->String ensures result=="bad" {return "ok";}"#,
            Counterexample,
        ),
        (
            "false unicode length",
            r#"@pure @verified fn f()->i64 ensures result==2 {return "雪🌲".len();}"#,
            Counterexample,
        ),
        (
            "distinct same length",
            r#"@pure @verified fn f(s:String,t:String)->bool requires s.len()==t.len() ensures result {return s==t;}"#,
            Counterexample,
        ),
        (
            "nul not terminator",
            r#"@pure @verified fn f()->bool ensures result {return "a\0b"=="a";}"#,
            Counterexample,
        ),
        (
            "false identity",
            r#"@pure @verified fn f(s:String)->String requires s=="x" ensures result=="y" {return s;}"#,
            Counterexample,
        ),
        (
            "vacuous string",
            r#"@pure @verified fn f(s:String)->String requires s.len()<0 {return s;}"#,
            VacuousPrecondition,
        ),
        (
            "unsupported contains",
            r#"@pure @verified fn f(s:String)->bool {return s.contains("x");}"#,
            Unknown,
        ),
        (
            "unsupported order",
            r#"@pure @verified fn f(s:String,t:String)->bool {return s<t;}"#,
            Unknown,
        ),
        (
            "unsupported concat",
            r#"@pure @verified fn f(s:String)->String {return s+"!";}"#,
            Unknown,
        ),
        (
            "unsupported float",
            r#"@pure @verified fn f(s:f64)->f64 {return s;}"#,
            Unknown,
        ),
        (
            "mutable array",
            r#"@pure @verified fn f(a:Array<String>)->String {return a[0];}"#,
            Unknown,
        ),
        (
            "owned array",
            r#"@pure @verified fn f()->i64 {let a=[1];a[0]=2;return a[0];}"#,
            Unknown,
        ),
        (
            "mutable record",
            r#"class Label {pub text:String;} fn change(s:Label){s.text="bad";} @pure @verified fn f(s:Label)->String {return s.text;}"#,
            Unknown,
        ),
        (
            "shadowed length",
            r#"class Label {pub text:String;pub fn len(self)->i64{return 9;}} @pure @verified fn f(s:Label)->i64 {return s.len();}"#,
            Unknown,
        ),
    ];
    assert!(cases.len() >= 20);
    for (name, source, expected) in cases {
        assert_string_case(name, source, expected);
    }
}

fn assert_string_case(name: &str, source: &str, expected: Status) {
    use Status::*;
    let v = verify(&parse(source), &options(), false);
    let report = v.reports.last().unwrap();
    assert_eq!(report.status, expected, "{name}: {:?}", v.reports);
    if expected == Counterexample && !report.input_names.is_empty() {
        assert!(
            report
                .obligations
                .iter()
                .any(|o| o.counterexample_candidate.is_some()),
            "{name}"
        );
    }
    if expected == Unknown {
        assert!(!report.unknown_reasons.is_empty(), "{name}");
    }
    // Exercise resolved intrinsics, purity, contract whitelist and the same
    // frontend used by the CLI, not just the unchecked encoder.
    let checked = comparison::capture(parse(source), &options(), &mut ProofCache::default());
    if expected == Proven {
        assert!(checked.errors.is_empty(), "{name}: {:?}", checked.errors);
        assert_eq!(
            checked.verification.reports.last().unwrap().status,
            Proven,
            "{name}"
        );
    } else {
        assert!(
            !checked.errors.is_empty()
                || checked
                    .verification
                    .reports
                    .iter()
                    .any(|r| r.status != Proven),
            "{name} was accepted as proven"
        );
    }
}

#[test]
fn verified_string_comparison_cache_and_replay() {
    let source = r#"@pure @verified fn f(s:String)->String ensures result==s {return s;}"#;
    let program = parse(source);
    let mut cache = ProofCache::default();
    let first = verify_cached(&program, &options(), false, &mut cache);
    let second = verify_cached(&program, &options(), false, &mut cache);
    assert_eq!(cache.hits, 1);
    assert_eq!(
        serde_json::to_value(&first.reports).unwrap(),
        serde_json::to_value(&second.reports).unwrap()
    );
    assert_eq!(first.smt, verify(&parse(source), &options(), false).smt);
    let mut solver = solver::Solver::new(&options().solver, 5000).unwrap();
    // Replay the generated SMT with the same pinned solver.
    let replay = solver.exchange(&first.smt).unwrap();
    assert!(
        !replay.contains("unknown") && !replay.contains("error"),
        "{replay}"
    );
    let limited = verify_cached(
        &program,
        &Options {
            resource_limit: 1,
            ..options()
        },
        false,
        &mut cache,
    );
    assert_eq!(limited.reports[0].status, Status::Unknown);
    assert!(!limited.reports[0].unknown_reasons.is_empty());
    let mut cache = ProofCache::default();
    let old = comparison::capture(parse(source), &options(), &mut cache);
    let same = comparison::capture(parse(&format!("\n{source}")), &options(), &mut cache);
    let result = comparison::compare(&old, &same, &options());
    assert_eq!(
        result["changes"][0]["contract_preserved"], "PROVEN",
        "{result}"
    );
    let weak = comparison::capture(
        parse(&source.replace("result==s", "result.len()>=0")),
        &options(),
        &mut cache,
    );
    let result = comparison::compare(&old, &weak, &options());
    assert_eq!(
        result["changes"][0]["contract_preserved"], "NOT_PRESERVED",
        "{result}"
    );
}

#[test]
fn verified_string_encoding_scales_with_occurrences_and_literal_bytes() {
    for bytes in [1, 16, 256, 4096] {
        for count in [1, 8, 32] {
            let literal = "x".repeat(bytes);
            let mut source = String::from("@pure @verified fn f(s:String)->String {");
            for i in 0..count {
                source += &format!("let s{i}=\"{literal}\";");
            }
            source += "return s;}";
            let program = parse(&source);
            let Item::Function(f) = &program.items[0] else {
                panic!()
            };
            let summary =
                encode(0, &[f], &HashMap::from([("f", 0)]), &DataTypes::default()).unwrap();
            // Every literal appears once in each of the constant number of
            // shared summary definitions (value, safe, failure_safe, assume,
            // post, post_safe), never multiplied by callers or exits.
            let escaped_bytes = summary.definitions.matches("\\u{78}").count();
            assert_eq!(escaped_bytes, bytes * count * 6);
            assert!(summary.nodes <= count + 8);
            eprintln!(
                "bytes={bytes} occurrences={count} escapes={escaped_bytes} nodes={} smt_bytes={}",
                summary.nodes,
                summary.definitions.len()
            );
        }
    }
}
