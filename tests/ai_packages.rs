use serde_json::{Value, json};
use std::{fs, path::PathBuf};
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{Direction, Limits, QueryRequest, QuerySession, Snapshot},
    diagnostics::HumanEmitter,
    package::{PackageMutation, mutate_packages_report},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-ai-packages-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        Self(fs::canonicalize(root).unwrap())
    }
    fn write(&self, path: &str, source: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, source).unwrap();
    }
    fn manifest(&self, alias: &str, version: &str) {
        self.write("app/project.toml", &format!("[project]\nname='app'\nversion='1.0.0'\n[willow]\nmanifest-version=1\n[dependencies]\n{alias}={{path='../dep'}}\n"));
        self.write(
            "dep/project.toml",
            &format!(
                "[project]\nname='library'\nversion='{version}'\n[willow]\nmanifest-version=1\n"
            ),
        );
    }
    fn snapshot(&self, alias: &str) -> Snapshot {
        self.write("app/src/main.wi", &format!("import {alias}::util as tools; fn test_value() -> i64 {{ return tools::value(); }} fn main() {{ test_value(); }}"));
        self.write(
            "dep/src/util.wi",
            "module util; pub fn value() -> i64 { return 42; }",
        );
        let root = self.0.join("app");
        let entry = root.join("src/main.wi");
        CompilerSession::new(
            entry.to_str().unwrap(),
            "",
            &CompilerOptions::debug(),
            Some(root),
        )
        .analysis_with_emitter(&mut HumanEmitter)
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn package_queries_preserve_identity_across_aliases_and_follow_dry_run_delta() {
    let fixture = Fixture::new();
    fixture.manifest("dep", "1.0.0");
    let snapshot = fixture.snapshot("dep");
    snapshot.validate().unwrap();
    let target = snapshot
        .functions
        .iter()
        .find(|f| f.name == "value")
        .unwrap();
    let identity = target.identity.clone().unwrap();
    assert_eq!(identity.package.name, "library");
    assert_eq!(identity.package.version, "1.0.0");
    assert_eq!(identity.package.revision, None);
    assert_eq!(identity.module, "util");
    assert_eq!(identity.symbol, "value");
    let target_id = target.id.clone();
    let test_id = snapshot
        .functions
        .iter()
        .find(|f| f.name == "test_value")
        .unwrap()
        .id
        .clone();
    let impact = snapshot
        .impact(
            std::slice::from_ref(&target_id),
            Direction::Callers,
            Limits::default(),
            None,
        )
        .unwrap();
    assert_eq!(impact.nodes.len(), 3);
    assert!(impact.nodes.iter().all(|n| n.identity.is_some()));
    assert_eq!(impact.external_dependencies.len(), 1);
    assert_eq!(impact.external_dependencies[0]["relation"], "callee");
    let revision = snapshot.revision.clone();
    let mut session = QuerySession::new(snapshot.clone()).unwrap();
    let result = session.query(QueryRequest::SymbolInfo {
        revision: revision.clone(),
        function: target_id.clone(),
    });
    assert_eq!(result["result"]["symbol"]["identity"], json!(identity));
    let refs = session.query(QueryRequest::References {
        revision: revision.clone(),
        function: target_id,
    });
    assert!(
        refs["result"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["identity"] == json!(identity))
    );
    // Change only the consumer alias; source identity and logical module stay fixed.
    fixture.manifest("renamed", "1.0.0");
    let renamed = fixture.snapshot("renamed");
    assert_eq!(
        renamed
            .functions
            .iter()
            .find(|f| f.name == "value")
            .unwrap()
            .identity,
        Some(identity)
    );
    fixture.write(
        "app/requests.json",
        &json!([
            {"kind":"symbols"},
            {"kind":"affected","delta":{"schema":1,"ok":true,"kind":"package.mutation"}}
        ])
        .to_string(),
    );
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(fixture.0.join("app"))
        .args(["query", "--requests", "requests.json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let event = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|v| v["event"] == "analysis.result")
        .unwrap();
    assert!(
        event["data"]["results"][0]["result"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["identity"]["package"]["name"] == "library")
    );
    assert_eq!(event["data"]["results"][1]["result"]["status"], "ok");
    fixture.manifest("renamed", "1.1.0");
    let root = fixture.0.join("app");
    let lock_before = fs::read(root.join("project.lock")).unwrap();
    let report = mutate_packages_report(
        &root,
        PackageMutation::Update {
            alias: None,
            breaking: false,
        },
        true,
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(fs::read(root.join("project.lock")).unwrap(), lock_before);
    assert!(!report.applied);
    let delta = serde_json::to_value(report).unwrap();
    let request: QueryRequest = serde_json::from_value(
        json!({"kind":"affected","revision":revision,"delta":delta,"tests":[test_id]}),
    )
    .unwrap();
    let affected = session.query(request);
    let result = &affected["result"];
    assert_eq!(result["status"], "ok", "{affected}");
    assert_eq!(result["modules"].as_array().unwrap().len(), 2);
    assert!(
        result["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == "test_value")
    );
    assert_eq!(result["tests"].as_array().unwrap().len(), 1);
    assert_eq!(result["edge_visits"], 1);
    let stale: QueryRequest =
        serde_json::from_value(json!({"kind":"affected","revision":"stale","delta":delta}))
            .unwrap();
    assert_eq!(session.query(stale)["status"], "stale");
    let roundtrip: Snapshot =
        serde_json::from_value(serde_json::to_value(&renamed).unwrap()).unwrap();
    roundtrip.validate().unwrap();
    let empty_delta: Value = json!({"schema":1,"ok":true,"kind":"package.mutation","changes":[]});
    let no_change = renamed.affected(&serde_json::from_value(empty_delta).unwrap(), &[]);
    assert!(no_change["symbols"].as_array().unwrap().is_empty());
}

#[test]
fn legacy_and_invalid_delta_coverage_is_explicit() {
    let fixture = Fixture::new();
    fixture.write("main.wi", "fn main() {}");
    let entry = fixture.0.join("main.wi");
    let snapshot =
        CompilerSession::new(entry.to_str().unwrap(), "", &CompilerOptions::debug(), None)
            .analysis_with_emitter(&mut HumanEmitter)
            .unwrap();
    assert!(snapshot.functions.iter().all(|f| f.identity.is_none()));
    let delta =
        serde_json::from_value(json!({"schema":1,"ok":true,"kind":"package.mutation"})).unwrap();
    assert_eq!(snapshot.affected(&delta, &[])["status"], "incomplete");
    let invalid =
        serde_json::from_value(json!({"schema":2,"ok":true,"kind":"package.mutation"})).unwrap();
    assert_eq!(snapshot.affected(&invalid, &[])["status"], "invalid-delta");
    fixture.manifest("dep", "1.0.0");
    let snapshot = fixture.snapshot("dep");
    let result = snapshot.affected(&delta, &["absent-test".into()]);
    assert_eq!(result["status"], "incomplete");
    assert_eq!(result["unknown_tests"], json!(["absent-test"]));
}

#[test]
fn scoped_symbol_identities_distinguish_owners_and_shadowed_bindings() {
    let fixture = Fixture::new();
    let source = r#"module util;
        pub open class A { pub value: i64; pub fn get(self, x: i64) -> i64 { return self.value + x; } }
        pub class Child extends A {}
        pub class B { pub value: i64; pub fn get(self, x: i64) -> i64 { return self.value + x; } }
        pub fn inherited(child: Child) -> i64 { return child.value; }
        pub fn first(x: i64) -> i64 { return x; }
        pub fn second(x: i64) -> i64 { return x; }
        pub fn scopes(x: i64) -> i64 {
            let y = x;
            if true { let y = 2; println(y); }
            if true { let y = 3; println(y); }
            let f = |x: i64| x;
            return f(x) + y;
        }
    "#;
    let mut previous = None;
    for alias in ["dep", "renamed_dependency"] {
        fixture.manifest(alias, "1.0.0");
        fixture.write("dep/src/util.wi", source);
        fixture.write(
            "app/src/main.wi",
            &format!("import {alias}::util as tools; fn main() {{ tools::first(1); }}"),
        );
        let root = fixture.0.join("app");
        let entry = root.join("src/main.wi");
        let snapshot = CompilerSession::new(
            entry.to_str().unwrap(),
            "",
            &CompilerOptions::debug(),
            Some(root),
        )
        .analysis_with_emitter(&mut HumanEmitter)
        .unwrap();
        snapshot.validate().unwrap();
        let mut session = QuerySession::new(snapshot.clone()).unwrap();
        let declarations: Vec<_> = snapshot
            .semantic
            .symbols
            .iter()
            .filter(|s| {
                s.identity
                    .as_ref()
                    .is_some_and(|i| i.package.name == "library")
            })
            .collect();
        let mut unique = std::collections::HashMap::new();
        for symbol in &declarations {
            let identity = symbol.identity.as_ref().unwrap();
            assert!(
                unique.insert(identity.symbol.clone(), &symbol.id).is_none(),
                "duplicate identity: {identity:?}"
            );
        }
        for (name, kind, count) in [
            ("x", "parameter", 6),
            ("value", "field", 2),
            ("y", "binding", 3),
        ] {
            let symbols: Vec<_> = declarations
                .iter()
                .filter(|s| s.name == name && s.kind == kind)
                .collect();
            assert_eq!(symbols.len(), count, "{name}: {symbols:?}");
            for symbol in symbols {
                let refs: Vec<_> = snapshot
                    .semantic
                    .references
                    .iter()
                    .filter(|r| r.target == symbol.id)
                    .collect();
                assert!(!refs.is_empty(), "missing references to {symbol:?}");
                assert!(refs.iter().all(|r| r.identity == symbol.identity));
                let result = session.query(QueryRequest::References {
                    revision: snapshot.revision.clone(),
                    function: symbol.id.clone(),
                });
                let query_refs = result["result"]["references"].as_array().unwrap();
                assert_eq!(query_refs.len(), refs.len());
                assert!(
                    query_refs
                        .iter()
                        .all(|r| r["identity"] == json!(symbol.identity))
                );
            }
        }
        for name in ["A::field:value", "B::field:value"] {
            assert!(unique.contains_key(name), "missing {name}: {unique:?}");
        }
        assert!(unique.keys().any(|s| s.starts_with("first::parameter:x@")));
        assert!(unique.keys().any(|s| s.starts_with("second::parameter:x@")));
        assert!(unique.keys().any(|s| s.starts_with("A::get::parameter:x@")));
        assert!(unique.keys().any(|s| s.starts_with("B::get::parameter:x@")));
        let identities: std::collections::BTreeMap<_, _> = declarations
            .iter()
            .map(|s| (s.id.clone(), serde_json::to_value(&s.identity).unwrap()))
            .collect();
        if let Some(previous) = &previous {
            assert_eq!(&identities, previous);
        }
        previous = Some(identities);
        let roundtrip: Snapshot =
            serde_json::from_value(serde_json::to_value(&snapshot).unwrap()).unwrap();
        roundtrip.validate().unwrap();
    }
}
