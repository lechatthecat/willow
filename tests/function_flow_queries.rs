use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{QueryRequest, QuerySession},
    diagnostics::HumanEmitter,
};

fn effects(source: &str, label: &str, name: &str) -> serde_json::Value {
    let directory = std::env::temp_dir().join(format!(
        "willow-function-flow-{}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    std::fs::write(&path, source).unwrap();
    let snapshot =
        CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
            .analysis_with_emitter(&mut HumanEmitter)
            .unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    let function = snapshot
        .functions
        .iter()
        .find(|f| f.name == name)
        .unwrap()
        .id
        .clone();
    let revision = snapshot.revision.clone();
    QuerySession::new(snapshot)
        .unwrap()
        .query(QueryRequest::Effects { revision, function })["result"]
        .take()
}
#[test]
fn function_flow_query_uses_reaching_targets() {
    let source = "fn safe(x:i64)->i64 { return x; } fn slow(x:i64)->i64 { let c=Channel<i64>::new(); return c.recv(); } fn apply(f:fn(i64)->i64)->i64 { return f(1); } fn main() { let unused=slow; apply(safe); }";
    let result = effects(source, "precise", "apply");
    assert_eq!(
        result["bounds"]["runtime"].as_u64().unwrap() & 6,
        0,
        "{result}"
    );
    for effect in result["effects"].as_array().unwrap() {
        assert!(
            effect["unknown_reasons"].as_array().unwrap().is_empty(),
            "{result}"
        );
    }
}
#[test]
fn function_flow_query_preserves_waiting_witness() {
    let source = "fn slow(x:i64)->i64 { let c=Channel<i64>::new(); return c.recv(); } fn apply(f:fn(i64)->i64)->i64 { return f(1); } fn main() { apply(slow); }";
    let result = effects(source, "wait", "apply");
    assert_ne!(
        result["bounds"]["runtime"].as_u64().unwrap() & 6,
        0,
        "{result}"
    );
}
#[test]
fn function_flow_query_explains_external_fallback() {
    let source = "fn safe(x:i64)->i64 { return x; } pub fn apply(f:fn(i64)->i64)->i64 { return f(1); } fn main() { apply(safe); }";
    let result = effects(source, "external", "apply");
    assert!(
        result.to_string().contains("function-flow-external-escape"),
        "{result}"
    );
}
#[test]
fn function_flow_query_explains_candidate_limit() {
    let mut source = String::new();
    for i in 0..40 {
        source.push_str(&format!("fn f{i}(x:i64)->i64 {{ return x; }}\n"));
    }
    source.push_str("fn main() { let mut f=f0;");
    for i in 1..40 {
        source.push_str(&format!("f=f{i};"));
    }
    source.push_str("f(0); }");
    let result = effects(&source, "limit", "main");
    assert!(
        result.to_string().contains("function-flow-candidate-limit"),
        "{result}"
    );
}
#[test]
fn function_flow_query_explains_depth_limit() {
    let mut source = "fn safe(x:i64)->i64 { return x; } fn main() { let cb0=safe;".to_owned();
    for i in 1..100 {
        source.push_str(&format!("let cb{i}=cb{};", i - 1));
    }
    source.push_str("cb99(1); }");
    let result = effects(&source, "depth", "main");
    assert!(
        result["effects"]
            .to_string()
            .contains("function-flow-depth-limit"),
        "{result}"
    );
}
#[test]
fn function_flow_query_explains_storage_fallback() {
    let source = "pub class Holder { pub f:fn(i64)->i64; } fn safe(x:i64)->i64 { return x; } fn main() { let h=new Holder(safe); let f=h.f; f(1); }";
    let result = effects(source, "storage", "main");
    assert!(
        result["effects"]
            .to_string()
            .contains("function-flow-unknown-storage"),
        "{result}"
    );
}

#[test]
fn function_flow_query_storage_write_paths() {
    let prelude = "fn safe(x:i64)->i64 { return x; } fn slow(x:i64)->i64 { let c=Channel<i64>::new(); return c.recv(); }";
    for (label, helper, body) in STORAGE_CASES {
        for target in ["safe", "slow"] {
            let source = format!("{prelude} {helper} fn run()->i64 {{ let unused=slow; {body} return f(1); }} fn main() {{ run(); }}").replace("TARGET",target);
            let result = effects(&source, &format!("{label}-{target}"), "run");
            assert_eq!(
                result["bounds"]["runtime"].as_u64().unwrap() & 6 != 0,
                target == "slow",
                "{label}-{target}: {result}"
            );
            if target == "slow" {
                assert!(
                    result["effects"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|effect| !effect["witness"].is_null()),
                    "{label}: {result}"
                );
            }
        }
    }
}

#[test]
fn function_flow_query_unmodelled_collection_receiver_explained() {
    let source = "fn safe(x:i64)->i64 { return x; } fn slow(x:i64)->i64 { let c=Channel<i64>::new(); return c.recv(); } fn run()->i64 { let unused=slow; let a=[safe]; let b=a.slice(0,1); let f=b[0]; return f(1); } fn main() { run(); }";
    let result = effects(source, "collection-fallback", "run");
    assert!(
        result.to_string().contains("function-flow-unknown-storage"),
        "{result}"
    );
    assert_ne!(
        result["bounds"]["runtime"].as_u64().unwrap() & 6,
        0,
        "{result}"
    );
}

const STORAGE_CASES: &[(&str, &str, &str)] = &[
    (
        "array-reference",
        "fn replace(f:&mut fn(i64)->i64) { f=TARGET; }",
        "let a=[safe]; replace(&a[0]); let f=a[0];",
    ),
    (
        "super-constructor",
        "open class Base { pub f:fn(i64)->i64; pub init(self,f:fn(i64)->i64) { self.f=f; } } class Child extends Base { pub init(self,f:fn(i64)->i64) { super.init(f); } }",
        "let a=new Base(safe); let b=new Child(TARGET); let f=b.f;",
    ),
    (
        "push-return",
        "fn make()->fn(i64)->i64 { let a=[safe]; a.push(TARGET); return a[1]; }",
        "let f=make();",
    ),
    (
        "push-alias",
        "",
        "let a=[safe]; let b=a; b.push(TARGET); let f=a[1];",
    ),
    (
        "insert",
        "",
        "let a=[safe]; a.insert(0,TARGET); let f=a[0];",
    ),
    ("fill", "", "let a=[safe]; a.fill(TARGET); let f=a[0];"),
    ("pop", "", "let a=[safe]; a.push(TARGET); let f=a.pop();"),
    (
        "remove",
        "",
        "let a=[safe]; a.push(TARGET); let f=a.remove(1);",
    ),
];

#[test]
fn function_flow_storage_writes_lock_rejections_and_safe_controls() {
    use willow_compiler::diagnostics::{
        Diagnostic, DiagnosticEmitter, ErrorCode, Severity, source_map::SourceLookup,
    };
    #[derive(Default)]
    struct Capture(Vec<Diagnostic>);
    impl DiagnosticEmitter for Capture {
        fn emit(&mut self, diagnostic: &Diagnostic, _: &dyn SourceLookup) -> std::io::Result<()> {
            self.0.push(diagnostic.clone());
            Ok(())
        }
    }
    for (label, helper, body) in STORAGE_CASES {
        for target in ["safe", "slow"] {
            let source = format!("fn safe(x:i64)->i64 {{ return x; }} fn slow(x:i64)->i64 {{ let c=Channel<i64>::new(); return c.recv(); }} {helper} fn run()->i64 {{ let unused=slow; {body} return f(1); }} async fn main() {{ let m=Mutex::new(0); lock m as v {{ run(); }} }}").replace("TARGET",target);
            let directory = std::env::temp_dir().join(format!(
                "willow-flow-lock-{}-{label}-{target}",
                std::process::id()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let path = directory.join("main.wi");
            std::fs::write(&path, &source).unwrap();
            let mut diagnostics = Capture::default();
            let result =
                CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
                    .analysis_with_emitter(&mut diagnostics);
            std::fs::remove_dir_all(directory).unwrap();
            let errors: Vec<_> = diagnostics
                .0
                .iter()
                .filter(|d| d.severity == Severity::Error)
                .collect();
            if target == "safe" {
                assert!(result.is_ok() && errors.is_empty(), "{label}: {errors:#?}");
            } else {
                assert!(
                    errors
                        .iter()
                        .any(|d| d.code == ErrorCode::E2604 && d.labels.len() >= 2),
                    "{label}: {errors:#?}"
                );
                assert!(
                    errors.iter().all(|d| d.code == ErrorCode::E2604),
                    "{label}: {errors:#?}"
                );
            }
        }
    }
}

#[test]
fn function_flow_query_collection_storage_is_not_invocation() {
    let source = "fn safe(x:i64)->i64 { return x; } fn slow(x:i64)->i64 { let c=Channel<i64>::new(); return c.recv(); } fn run()->i64 { let a=[safe]; a.push(slow); return 1; } async fn main() { let m=Mutex::new(0); lock m as v { run(); } }";
    let result = effects(source, "store-only", "run");
    assert_eq!(
        result["bounds"]["runtime"].as_u64().unwrap() & 6,
        0,
        "{result}"
    );
}
