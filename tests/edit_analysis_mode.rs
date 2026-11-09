use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-edit-analysis-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/main.wi"),
            "fn value() -> i64 { return 1; } fn main() { println(value()); }",
        )
        .unwrap();
        Self(root)
    }
    fn result(&self, args: &[&str]) -> Value {
        let out = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .find(|v| v["event"] == "analysis.result")
            .unwrap()["data"]
            .clone()
    }
    fn symbols(&self, source: &str) -> Value {
        fs::write(
            self.0.join("symbols-query.json"),
            r#"[{"kind":"symbols","symbol_kind":"function"}]"#,
        )
        .unwrap();
        let result = self.result(&["query", source, "--requests", "symbols-query.json"]);
        json!({"revision": result["revision"], "functions": result["results"][0]["result"]["symbols"]})
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn prepare_documents_and_preserves_query_analysis_mode() {
    let f = Fixture::new();
    fs::write(f.0.join("project.toml"), "[willow]\nmanifest-version = 1\n[project]\nname = \"app\"\nversion = \"0.1.0\"\nentry = \"src/main.wi\"\n[dependencies]\n").unwrap();
    let help = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["edit", "--help"])
        .output()
        .unwrap();
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("[--project]"));
    for project in [false, true] {
        let snapshot = if project {
            f.symbols(".")
        } else {
            f.symbols("src/main.wi")
        };
        let function = snapshot["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "value")
            .unwrap()["id"]
            .clone();
        fs::write(
            f.0.join("request.json"),
            serde_json::to_vec(&json!({
                "revision": snapshot["revision"],
                "operations": [{"kind":"rename", "function":function, "name":"answer"}]
            }))
            .unwrap(),
        )
        .unwrap();
        let before = fs::read(f.0.join("src/main.wi")).unwrap();
        let mut args = vec![
            "edit",
            "prepare",
            "--entry",
            "src/main.wi",
            "--requests",
            "request.json",
        ];
        if !project {
            args.push("--project");
        }
        let rejected = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&f.0)
            .args(&args)
            .output()
            .unwrap();
        assert!(!rejected.status.success());
        let output = String::from_utf8_lossy(&rejected.stdout);
        assert!(output.contains("base revision mismatch"), "{output}");
        assert!(output.contains("use --project"), "{output}");
        assert!(output.contains("omit --project"), "{output}");
        assert!(
            output.contains("query the current source again"),
            "{output}"
        );
        assert_eq!(fs::read(f.0.join("src/main.wi")).unwrap(), before);
        if !project {
            args.pop();
        } else {
            args.push("--project");
        }
        assert_eq!(f.result(&args)["state"], "prepared");
        assert_eq!(fs::read(f.0.join("src/main.wi")).unwrap(), before);
    }
}

#[test]
fn verification_disk_signature_cache_both_workflows_match_clean() {
    verification_disk_cache_both_workflows(&[
        ("class Box {}", "Box", 0),
        ("class Box<T> { value: T; }", "Box<i64>", 0),
        ("class Box<T> { value: T; }", "Box<i64>", 1),
        ("class Box<T> { value: T; }", "Box<i64>", -1),
        ("class Box<T> { value: T; }", "Box<i64>", 2),
        ("class Box<T> { value: T; }", "Box<i64>", -2),
        ("class Box<T> { value: T; }", "declarations", 0),
        ("enum Box<T> { Value(T) }", "Box<i64>", 0),
    ]);
}

#[test]
fn verification_disk_borrow_cache_both_workflows_match_clean() {
    verification_disk_cache_both_workflows(&[
        ("class Box<T> { value: T; }", "borrow", 2),
        ("class Box<T> { value: T; }", "borrow", -2),
    ]);
}

fn verification_disk_cache_both_workflows(cases: &[(&str, &str, i32)]) {
    use sha2::{Digest, Sha256};
    use std::collections::BTreeMap;
    fn inventory(root: &std::path::Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut result = BTreeMap::new();
        for entry in fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                result.extend(inventory(&path));
            } else {
                result.insert(path.clone(), fs::read(path).unwrap());
            }
        }
        result
    }
    for n in [8, 32, 128] {
        for &(decl, ty, import_change) in cases {
            let f = Fixture::new();
            let entry = f.0.join("main.wi");
            let mut source = format!(
                "pub fn value() -> i64 {{ return 1; }} fn main() {{ println(value()); }} {decl}\n"
            ) + &(0..n)
                .map(|i| if ty == "declarations" {
                    format!("class C{i} {{ value: i64; }} enum E{i} {{ Value(i64) }} interface I{i} {{ fn get(self) -> i64; }}\n")
                } else if ty == "borrow" {
                    format!("fn unrelated{i}() -> i64 {{ return {i}; }}\n")
                } else { format!("fn unrelated{i}(v: {ty}) -> {ty} {{ return v; }}\n") })
                .collect::<String>();
            if ty == "borrow" {
                source += "async fn borrow(x: &i64) -> i64 { return x; } async fn consumer() { let x = 1; let t = borrow(&x); await t; }";
            }
            if import_change == 2 || import_change == -2 {
                let library = (0..n)
                    .map(|i| format!("pub fn stable{i}() -> i64 {{ return {i}; }}\n"))
                    .collect::<String>();
                fs::write(f.0.join("stable.wi"), library).unwrap();
                source = format!("import stable;\n{source}");
            }
            fs::write(f.0.join("extra.wi"), "").unwrap();
            if import_change == -2 {
                source = format!("import extra;\n{source}");
            }
            if import_change == -1 {
                source = format!("import std::time;\n{source}");
            }
            fs::write(&entry, &source).unwrap();
            let run = |args: &[&str], clean: bool| {
                let out = Command::new(env!("CARGO_BIN_EXE_willow"))
                    .current_dir(&f.0)
                    .args(args)
                    .args(["--format", "human"])
                    .env("WILLOW_QUERY_CACHE_DIR", f.0.join("cache"))
                    .env("WILLOW_QUERY_CACHE_STATS", "1")
                    .env("WILLOW_QUERY_CACHE", if clean { "0" } else { "1" })
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "{}",
                    String::from_utf8_lossy(&out.stderr)
                );
                let stats: Vec<Value> = String::from_utf8_lossy(&out.stderr)
                    .lines()
                    .filter_map(|line| line.strip_prefix("willow-query-cache "))
                    .map(|json| serde_json::from_str(json).unwrap())
                    .collect();
                (String::from_utf8(out.stdout).unwrap(), stats)
            };
            let json_run = |args: &[&str], clean| {
                let (output, stats) = run(args, clean);
                (serde_json::from_str::<Value>(&output).unwrap(), stats)
            };
            let (_, cold_stats) = run(&["check", "main.wi"], false);
            if ty == "borrow" {
                assert!(cold_stats[0]["async_borrow_checks"].as_u64().unwrap() >= n);
            }
            if import_change == 1 {
                source = format!("import std::time;\n{source}");
            }
            if import_change == -1 {
                source = source.replacen("import std::time;\n", "", 1);
            }
            if import_change == 2 {
                source = format!("import extra;\n{source}");
            }
            if import_change == -2 {
                source = source.replacen("import extra;\n", "", 1);
                fs::remove_file(f.0.join("extra.wi")).unwrap();
            }
            fs::write(&entry, &source).unwrap();
            let (baseline, _) = json_run(
                &[
                    "edit",
                    "baseline",
                    "--entry",
                    "main.wi",
                    "--output",
                    "baseline.json",
                ],
                false,
            );
            let offset = source.find("return 1").unwrap().to_string();
            let (impact, _) = json_run(
                &[
                    "impact",
                    "main.wi",
                    "--file",
                    entry.to_str().unwrap(),
                    "--byte",
                    &offset,
                    "--direction",
                    "callees",
                ],
                false,
            );
            let function = impact["functions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["name"] == "value")
                .unwrap()["id"]
                .clone();
            fs::write(f.0.join("request.json"), serde_json::to_vec(&json!({
                "revision": impact["impact"]["revision"], "operations": [{"kind":"replace-body", "function":function, "body":"{ return 2; }"}]
            })).unwrap()).unwrap();
            let (prepared, _) = json_run(
                &[
                    "edit",
                    "prepare",
                    "--entry",
                    "main.wi",
                    "--requests",
                    "request.json",
                ],
                false,
            );
            fs::write(
                f.0.join("manifest.json"),
                serde_json::to_vec(&prepared["verification_manifest"]).unwrap(),
            )
            .unwrap();
            let transaction = prepared["transaction"].as_str().unwrap();
            for transaction_workflow in [true, false] {
                let args = if transaction_workflow {
                    vec![
                        "edit",
                        "verify",
                        "--transaction",
                        transaction,
                        "--manifest",
                        "manifest.json",
                    ]
                } else {
                    let changed = source.replacen("return 1;", "return 2;", 1);
                    fs::write(&entry, &changed).unwrap();
                    let digest = |text: &str| {
                        Sha256::digest(text.as_bytes())
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>()
                    };
                    fs::write(f.0.join("manifest.json"), serde_json::to_vec(&json!({
                        "schema_version":1, "baseline_revision":baseline["revision"], "complete":true,
                        "changes":[{"path":"main.wi", "before":digest(&source), "after":digest(&changed)}]
                    })).unwrap()).unwrap();
                    vec![
                        "edit",
                        "verify",
                        "--entry",
                        "main.wi",
                        "--baseline",
                        "baseline.json",
                        "--manifest",
                        "manifest.json",
                    ]
                };
                let saved = inventory(&f.0);
                let (clean, _) = json_run(&args, true);
                for _ in 0..2 {
                    let (warm, stats) = json_run(&args, false);
                    assert_eq!(warm, clean, "{n} {ty} transaction={transaction_workflow}");
                    assert_eq!(stats.len(), 1);
                    assert_eq!(stats[0]["loaded"], true);
                    assert_eq!(stats[0]["signature_checks"], 0, "{stats:?}");
                    assert_eq!(stats[0]["typechecks"], 1, "{stats:?}");
                    if ty == "borrow" {
                        assert_eq!(
                            stats[0]["async_borrow_checks"], 1,
                            "n={n} import={import_change} transaction={transaction_workflow}: {stats:?}"
                        );
                    }
                    assert_eq!(
                        stats[0]["effects_recomputed"], 0,
                        "n={n} ty={ty} import={import_change} transaction={transaction_workflow} {stats:?}"
                    );
                    if decl.starts_with("class Box<T>") {
                        assert_eq!(stats[0]["precheck_bodies"], 1, "{stats:?}");
                        assert_eq!(stats[0]["precheck_signatures"], 0, "{stats:?}");
                        assert_eq!(stats[0]["precheck_templates"], 0, "{stats:?}");
                        assert_eq!(stats[0]["precheck_instances"], 0, "{stats:?}");
                        assert_eq!(stats[0]["precheck_declarations"], 0, "{stats:?}");
                        assert_eq!(stats[0]["declaration_validations"], 0, "{stats:?}");
                    }
                    assert_eq!(inventory(&f.0), saved);
                }
            }
        }
    }
}
