//! R6 protocol, conservative boundary and read-only update acceptance tests.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
};
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{Direction, Limits, QuerySession, Snapshot},
    diagnostics::HumanEmitter,
};

struct Fixture {
    temp: PathBuf,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let temp = std::env::temp_dir().join(format!(
            "willow-r6-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let root = temp.join("project with spaces");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("native/src")).unwrap();
        fs::write(root.join("project.toml"), "[project]\nname='r6'\nversion='0.1.0'\n[willow]\nmanifest-version=1\n[rust-dependencies]\nregex={path='native'}\n[rust]\nbridge='bridge.rs'\n").unwrap();
        fs::write(
            root.join("native/Cargo.toml"),
            "[package]\nname='regex'\nversion='1.2.3'\nedition='2024'\n",
        )
        .unwrap();
        fs::write(
            root.join("native/src/lib.rs"),
            "pub fn is_match(v:i64)->bool {v==42}\n",
        )
        .unwrap();
        fs::write(
            root.join("bridge.rs"),
            "pub fn regex_is_match(v:i64)->bool {regex::is_match(v)}\n",
        )
        .unwrap();
        fs::write(root.join("src/main.wi"), "extern rust { fn regex_is_match(v:i64)->bool; }\nclass App { pub static fn validate(v:i64)->bool { return regex_is_match(v); } }\nfn main(){println(App::validate(42));}\n").unwrap();
        Self { temp, root }
    }
    fn run(&self, args: &[&str]) -> (bool, Value) {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.root)
            .args(args)
            .args(["--offline", "--format=json", "--cache-dir"])
            .arg(self.temp.join("cache"))
            .output()
            .unwrap();
        let value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{e}: {output:?}"));
        (output.status.success(), value)
    }
    fn snapshot(&self) -> Snapshot {
        CompilerSession::new(
            self.root.join("src/main.wi").to_str().unwrap(),
            "",
            &CompilerOptions::debug(),
            Some(self.root.clone()),
        )
        .analysis_for_edit_with_emitter(&mut HumanEmitter)
        .unwrap()
    }
    fn files(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    out.insert(path.strip_prefix(root).unwrap().into(), Vec::new());
                    walk(root, &path, out);
                } else {
                    out.insert(
                        path.strip_prefix(root).unwrap().into(),
                        fs::read(&path).unwrap(),
                    );
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.root, &self.root, &mut out);
        out
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp);
    }
}

#[test]
fn rust_bridge_json_schema_package_identity_and_unknown_selector() {
    let f = Fixture::new();
    let before = f.files();
    let (ok, v) = f.run(&["query", "rust-bridge", "regex_is_match"]);
    assert!(ok, "{v}");
    assert_eq!(v["schema"], 1);
    assert_eq!(v["status"], "ok");
    let b = &v["bridges"][0];
    assert_eq!(b["symbol"], "regex_is_match");
    assert_eq!(b["kind"], "rust-bridge");
    assert_eq!(b["crate"]["name"], "regex");
    assert_eq!(b["crate"]["version"], "1.2.3");
    assert_eq!(b["rust_adapter"], "bridge.rs");
    assert!(
        b["abi_symbol"]
            .as_str()
            .unwrap()
            .starts_with("__willow_rust_")
    );
    assert_eq!(b["signature"], json!({"inputs":["I64"],"output":"Bool"}));
    assert_eq!(b["identity"]["package"]["name"], "r6");
    assert_eq!(b["identity"]["symbol"], "regex_is_match");
    assert_eq!(f.files(), before);
    let (ok, v) = f.run(&["query", "rust-bridge", "absent"]);
    assert!(ok);
    assert_eq!(v["status"], "unknown");
    let snapshot = f.snapshot();
    let rev = snapshot.revision.clone();
    let mut session = QuerySession::new(snapshot).unwrap();
    let v = session.query(
        serde_json::from_value(
            json!({"kind":"rust-bridge","revision":rev,"symbol":"regex_is_match"}),
        )
        .unwrap(),
    );
    assert_eq!(
        v["result"]["bridges"][0]["identity"]["package"]["name"],
        "r6"
    );
    let v = session.query(
        serde_json::from_value(json!({"kind":"rust-bridge","revision":"stale","symbol":null}))
            .unwrap(),
    );
    assert_eq!(v["status"], "stale");
}

#[test]
fn app_validate_impact_stops_at_regex_boundary() {
    let f = Fixture::new();
    let (ok, v) = f.run(&["rust", "metadata"]);
    assert!(ok, "{v}");
    let snapshot = f.snapshot();
    let app = snapshot
        .functions
        .iter()
        .find(|f| f.name == "App::validate")
        .unwrap();
    let impact = snapshot
        .impact(
            std::slice::from_ref(&app.id),
            Direction::Callees,
            Limits::default(),
            Some(&snapshot.revision),
        )
        .unwrap();
    assert_eq!(impact.nodes.len(), 2);
    assert_eq!(impact.dependency_boundary.len(), 1);
    let b = &impact.dependency_boundary[0];
    assert_eq!(b["kind"], "rust");
    assert_eq!(b["crate"]["name"], "regex");
    assert_eq!(b["crate"]["version"], "1.2.3");
    assert_eq!(b["bridge"], "regex_is_match");
    assert!(impact.nodes.iter().any(|n| n.id == b["function"]));
}

#[test]
fn update_dry_run_preserves_all_project_files_fresh_and_locked() {
    for locked in [false, true] {
        let f = Fixture::new();
        if locked {
            let (ok, v) = f.run(&["rust", "metadata"]);
            assert!(ok, "{v}");
            let p = f.root.join("native/Cargo.toml");
            fs::write(
                &p,
                fs::read_to_string(&p).unwrap().replace("1.2.3", "1.2.4"),
            )
            .unwrap();
        }
        let before = f.files();
        for selector in [Some("regex"), None] {
            let mut args = vec!["rust", "update", "--dry-run"];
            if let Some(s) = selector {
                args.push(s);
            }
            let (ok, v) = f.run(&args);
            assert!(ok, "{v}");
            assert_eq!(v["dry_run"], true);
            assert_eq!(v["from"][0]["version"], "1.2.3");
            assert_eq!(
                v["to"][0]["version"],
                if locked { "1.2.4" } else { "1.2.3" }
            );
            let names: Vec<_> = v["affected_willow_callers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["name"].as_str().unwrap())
                .collect();
            assert_eq!(names, vec!["App::validate", "main"]);
            assert_eq!(v["impact"]["edge_visits"], 2);
            assert_eq!(f.files(), before);
        }
        let (ok, _) = f.run(&["rust", "update", "missing", "--dry-run"]);
        assert!(!ok);
        assert_eq!(f.files(), before);
    }
}

#[test]
fn bridge_compile_failure_bundles_cargo_declarations_and_callers() {
    let f = Fixture::new();
    fs::write(
        f.root.join("bridge.rs"),
        "pub fn regex_is_match(v:i64)->i64 {v}\n",
    )
    .unwrap();
    let (ok, v) = f.run(&["rust", "check"]);
    assert!(!ok, "{v}");
    let text = v.to_string();
    assert!(text.contains("rust_bridge_signature_mismatch"), "{v}");
    assert!(text.contains("E0308"), "{v}");
    assert!(text.contains("bridge_declarations"), "{v}");
    assert!(text.contains("regex_is_match"), "{v}");
    assert!(text.contains("App::validate"), "{v}");
}

#[test]
fn namespaced_and_imported_bridges_keep_distinct_function_ids() {
    let f = Fixture::new();
    fs::write(f.root.join("src/main.wi"),"import first as a; import second as b; fn main(){println(a::answer()); println(b::answer());}").unwrap();
    for name in ["first", "second"] {
        fs::write(f.root.join(format!("src/{name}.wi")),"extern rust native { fn regex_is_match(v:i64)->bool; } pub fn answer()->bool {return native::regex_is_match(42);}").unwrap();
    }
    let (ok, v) = f.run(&["query", "rust-bridge"]);
    assert!(ok, "{v}");
    let bridges = v["bridges"].as_array().unwrap();
    assert_eq!(bridges.len(), 2);
    assert_ne!(bridges[0]["function"], bridges[1]["function"]);
    assert_ne!(
        bridges[0]["identity"]["module"],
        bridges[1]["identity"]["module"]
    );
    assert_ne!(bridges[0]["abi_symbol"], bridges[1]["abi_symbol"]);
    for b in bridges {
        assert!(
            b["declaration_text"]
                .as_str()
                .unwrap()
                .contains("fn regex_is_match")
        );
        let (ok, v) = f.run(&["query", "rust-bridge", b["function"].as_str().unwrap()]);
        assert!(ok, "{v}");
        assert_eq!(v["bridges"].as_array().unwrap().len(), 1);
    }
    let (ok, v) = f.run(&["rust", "update", "--dry-run"]);
    assert!(ok, "{v}");
    assert_eq!(v["affected_willow_callers"].as_array().unwrap().len(), 3);
    assert_eq!(v["impact"]["edge_visits"], 4);
}

#[test]
fn multiple_crates_are_conservative_and_unused_bridges_have_no_callers() {
    let f = Fixture::new();
    fs::create_dir_all(f.root.join("other/src")).unwrap();
    fs::write(
        f.root.join("other/Cargo.toml"),
        "[package]\nname='other'\nversion='2.0.0'\nedition='2024'\n",
    )
    .unwrap();
    fs::write(f.root.join("other/src/lib.rs"), "").unwrap();
    let manifest = f.root.join("project.toml");
    fs::write(
        &manifest,
        fs::read_to_string(&manifest).unwrap().replace(
            "[rust-dependencies]",
            "[rust-dependencies]\nother={path='other'}",
        ),
    )
    .unwrap();
    fs::write(
        f.root.join("src/main.wi"),
        "extern rust { fn regex_is_match(v:i64)->bool; } fn main() {println(42);}",
    )
    .unwrap();
    let (ok, v) = f.run(&["query", "rust-bridge"]);
    assert!(ok, "{v}");
    assert!(v["bridges"][0]["crate"].is_null());
    assert_eq!(v["crate_candidates"].as_array().unwrap().len(), 2);
    let (ok, v) = f.run(&["rust", "update", "other", "--dry-run"]);
    assert!(ok, "{v}");
    assert_eq!(v["affected_willow_callers"], json!([]));
    assert_eq!(v["impact"]["edge_visits"], 0);
    fs::write(f.root.join("src/main.wi"), "fn main() {println(42);}").unwrap();
    let (ok, v) = f.run(&["query", "rust-bridge"]);
    assert!(ok, "{v}");
    assert_eq!(v["bridges"], json!([]));
}

#[test]
fn invalid_preview_options_and_cargo_failure_preserve_files() {
    let f = Fixture::new();
    let before = f.files();
    for args in [
        vec!["rust", "update", "--dry-run", "--dry-run"],
        vec!["rust", "update", "--dry-run", "--locked"],
        vec!["rust", "update", "--dry-run", "--frozen"],
        vec!["rust", "remove", "regex", "--dry-run"],
        vec!["rust", "check", "--dry-run"],
        vec!["query", "rust-bridge", "--dry-run"],
    ] {
        let (ok, v) = f.run(&args);
        assert!(!ok, "{args:?}: {v}");
        assert_eq!(f.files(), before);
    }
    let (ok, v) = f.run(&["rust", "update", "regex", "--dry-run", "--breaking"]);
    assert!(ok, "{v}");
    assert_eq!(f.files(), before);
    fs::write(f.root.join("native/Cargo.toml"), "invalid TOML [").unwrap();
    let before = f.files();
    let (ok, v) = f.run(&["rust", "update", "--dry-run"]);
    assert!(!ok, "{v}");
    assert_eq!(f.files(), before);
}

#[test]
fn batch_query_does_not_label_stale_or_missing_lock_versions_as_resolved() {
    for change in ["manifest", "cargo-lock", "missing-cargo-lock"] {
        let f = Fixture::new();
        let (ok, v) = f.run(&["rust", "metadata"]);
        assert!(ok, "{v}");
        match change {
            "manifest" => {
                let p = f.root.join("project.toml");
                fs::write(
                    &p,
                    fs::read_to_string(&p)
                        .unwrap()
                        .replace("path='native'", "path='./native'"),
                )
                .unwrap();
            }
            "cargo-lock" => {
                let p = f.root.join(".willow/rust/Cargo.lock");
                fs::write(
                    &p,
                    format!("{}\n# changed\n", fs::read_to_string(&p).unwrap()),
                )
                .unwrap();
            }
            _ => fs::remove_file(f.root.join(".willow/rust/Cargo.lock")).unwrap(),
        }
        let snapshot = f.snapshot();
        snapshot.validate().unwrap();
        assert!(
            snapshot.semantic.interop.crates[0].version.is_none(),
            "{change}"
        );
    }
}
