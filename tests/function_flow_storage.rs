//! Checked storage boundaries: external dynamic calls and inherited layouts.
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{QueryRequest, QuerySession},
    diagnostics::{Diagnostic, DiagnosticEmitter, ErrorCode, Severity, source_map::SourceLookup},
};

#[derive(Default)]
struct Capture(Vec<Diagnostic>);
impl DiagnosticEmitter for Capture {
    fn emit(&mut self, diagnostic: &Diagnostic, _: &dyn SourceLookup) -> std::io::Result<()> {
        self.0.push(diagnostic.clone());
        Ok(())
    }
}

/// Check both effects and lock diagnostics from the same independently supplied
/// expectation. Each waiting diagnostic must carry a source-bearing witness.
fn verify(label: &str, library: &str, declarations: &str, body: &str, waits: bool, unknown: bool) {
    let directory = std::env::temp_dir().join(format!(
        "willow-flow-storage-{}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    if !library.is_empty() {
        std::fs::write(directory.join("lib.wi"), library).unwrap();
    }
    let path = directory.join("main.wi");
    let prefix = format!(
        "import std::collections::Array; {} fn safe(x:i64)->i64 {{ return x; }} fn slow(x:i64)->i64 {{ let c=Channel<i64>::new(); return c.recv(); }} {declarations} fn run()->i64 {{ {body} }}",
        if library.is_empty() {
            ""
        } else {
            "import lib;"
        }
    );
    std::fs::write(&path, format!("{prefix} fn main() {{ run(); }}")).unwrap();
    let mut diagnostics = Capture::default();
    let snapshot =
        CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
            .analysis_with_emitter(&mut diagnostics)
            .unwrap_or_else(|e| {
                panic!(
                    "{label}: {e}
{:#?}",
                    diagnostics.0
                )
            });
    let function = snapshot
        .functions
        .iter()
        .find(|f| f.name == "run")
        .unwrap()
        .id
        .clone();
    let revision = snapshot.revision.clone();
    let result = QuerySession::new(snapshot)
        .unwrap()
        .query(QueryRequest::Effects { revision, function })["result"]
        .take();
    assert_eq!(
        result["bounds"]["runtime"].as_u64().unwrap() & 6 != 0,
        waits,
        "{label}: {result}"
    );
    if waits {
        assert!(
            result["effects"]
                .as_array()
                .unwrap()
                .iter()
                .any(|effect| !effect["witness"].is_null()),
            "{label}: {result}"
        );
    }
    if unknown {
        assert!(
            result.to_string().contains("function-flow-unknown-storage"),
            "{label}: {result}"
        );
    }
    std::fs::write(
        &path,
        format!("{prefix} async fn main() {{ let m=Mutex::new(0); lock m as v {{ run(); }} }}"),
    )
    .unwrap();
    let mut diagnostics = Capture::default();
    let checked = CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
        .analysis_with_emitter(&mut diagnostics);
    std::fs::remove_dir_all(directory).unwrap();
    let errors: Vec<_> = diagnostics
        .0
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .collect();
    if waits {
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
    } else {
        assert!(checked.is_ok() && errors.is_empty(), "{label}: {errors:#?}");
    }
}

#[test]
fn function_flow_external_storage_calls() {
    for target in ["safe", "slow"] {
        let library = format!(
            "import std::collections::Array; fn safe(x:i64)->i64 {{ return x; }} fn slow(x:i64)->i64 {{ let c=Channel<i64>::new(); return c.recv(); }} pub fn replace(a:Array<fn(i64)->i64>) {{ a.push({target}); }} pub fn operator()->fn(Array<fn(i64)->i64>) {{ return replace; }} pub class Holder {{ pub f:fn(i64)->i64; }} pub fn replace_holder(h:Holder) {{ h.f={target}; }} pub fn make()->Holder {{ return new Holder({target}); }} pub fn make_array()->Array<fn(i64)->i64> {{ return [{target}]; }} pub fn use_holder(callback:fn(Holder)->i64)->i64 {{ return callback(new Holder({target})); }}"
        );
        for (label, declarations, body) in [
            (
                "direct",
                "",
                "let a=[safe]; lib::replace(a); let f=a[1]; return f(1);",
            ),
            (
                "dynamic",
                "",
                "let a=[safe]; let op=lib::replace; op(a); let f=a[1]; return f(1);",
            ),
            (
                "forwarded",
                "fn apply(op:fn(Array<fn(i64)->i64>),a:Array<fn(i64)->i64>) { op(a); }",
                "let a=[safe]; apply(lib::replace,a); let f=a[1]; return f(1);",
            ),
            (
                "returned",
                "fn operator()->fn(Array<fn(i64)->i64>) { return lib::replace; }",
                "let a=[safe]; let op=operator(); op(a); let f=a[1]; return f(1);",
            ),
            (
                "alias",
                "",
                "let a=[safe]; let b=a; let op=lib::replace; op(b); let f=a[1]; return f(1);",
            ),
            (
                "stored-callee",
                "",
                "let a=[safe]; let ops=[lib::replace]; let op=ops[0]; op(a); let f=a[1]; return f(1);",
            ),
            (
                "unknown-callee",
                "",
                "let a=[safe]; let op=lib::operator(); op(a); let f=a[1]; return f(1);",
            ),
            (
                "object-argument",
                "class Local { pub f:fn(i64)->i64; }",
                "let anchor=new Local(safe); let h=new lib::Holder(safe); let op=lib::replace_holder; op(h); let f=h.f; return f(1);",
            ),
            (
                "object-result",
                "class Local { pub f:fn(i64)->i64; }",
                "let anchor=new Local(safe); let make=lib::make; let h=make(); let f=h.f; return f(1);",
            ),
            (
                "array-result",
                "",
                "let anchor=[safe]; let make=lib::make_array; let a=make(); let f=a[0]; return f(1);",
            ),
            (
                "callback-object",
                "class Local { pub f:fn(i64)->i64; } fn inspect(h:lib::Holder)->i64 { let f=h.f; return f(1); }",
                "let anchor=new Local(safe); let op=lib::use_holder; return op(inspect);",
            ),
        ] {
            verify(
                &format!("{label}-{target}"),
                &library,
                declarations,
                body,
                target == "slow",
                true,
            );
        }
    }
}

#[test]
fn function_flow_local_storage_call_keeps_precision() {
    verify(
        "local-storage",
        "",
        "fn replace(a:Array<fn(i64)->i64>) { a.push(safe); }",
        "let unused=slow; let a=[safe]; let op=replace; op(a); let f=a[1]; return f(1);",
        false,
        false,
    );
}

#[test]
fn function_flow_memberwise_inherited_and_declared_fields() {
    let declarations = "open class Base { pub f:fn(i64)->i64; pub n:i64; } open class Middle extends Base {} class Child extends Middle { pub g:fn(i64)->i64; pub flag:bool; }";
    for (label, first, second, field, waits) in [
        ("all-safe", "safe", "safe", "f", false),
        ("base-waits", "slow", "safe", "f", true),
        ("own-waits", "safe", "slow", "g", true),
        ("base-isolated", "safe", "slow", "f", false),
        ("own-isolated", "slow", "safe", "g", false),
    ] {
        let body = format!(
            "let unused=slow; let base=new Base(safe,1); let child=new Child({first},2,{second},true); let f=child.{field}; return f(1);"
        );
        verify(label, "", declarations, &body, waits, false);
    }
    for target in ["safe", "slow"] {
        verify(
            &format!("empty-derived-{target}"),
            "",
            "open class Base { pub f:fn(i64)->i64; } class Child extends Base {}",
            &format!(
                "let unused=slow; let base=new Base(safe); let child=new Child({target}); let f=child.f; return f(1);"
            ),
            target == "slow",
            false,
        );
        verify(
            &format!("super-memberwise-{target}"),
            "",
            "open class Base { pub f:fn(i64)->i64; } open class Middle extends Base { pub n:i64; } class Child extends Middle { pub init(self,f:fn(i64)->i64) { super.init(f,2); } }",
            &format!(
                "let unused=slow; let base=new Base(safe); let child=new Child({target}); let f=child.f; return f(1);"
            ),
            target == "slow",
            false,
        );
    }
}

#[test]
fn function_flow_memberwise_imported_base() {
    for target in ["safe", "slow"] {
        verify(
            &format!("imported-base-{target}"),
            "pub open class Base { pub f:fn(i64)->i64; pub n:i64; }",
            "class Child extends lib::Base { pub flag:bool; }",
            &format!(
                "let unused=slow; let base=new lib::Base(safe,1); let child=new Child({target},2,true); let f=child.f; return f(1);"
            ),
            target == "slow",
            false,
        );
    }
}
