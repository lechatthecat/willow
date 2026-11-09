//! Separate-process proof that semantic CLI queries consume, but never publish,
//! the same persistent body records as check/build.
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
    time::SystemTime,
};

struct Fixture(PathBuf);
impl Fixture {
    fn new(source: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-readonly-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        let fixture = Self(fs::canonicalize(root).unwrap());
        fixture.write(source);
        fixture
    }
    fn write(&self, source: &str) {
        fs::write(self.0.join("main.wi"), source).unwrap();
    }
    fn run(&self, args: &[&str], disabled: bool) -> Output {
        Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .env("WILLOW_QUERY_CACHE", if disabled { "0" } else { "1" })
            .env("WILLOW_QUERY_CACHE_STATS", "1")
            .env("WILLOW_QUERY_CACHE_DIR", self.0.join("cache"))
            .output()
            .unwrap()
    }
    fn warm(&self) {
        let output = self.run(&["check", "main.wi"], false);
        assert!(output.status.success(), "{}", stderr(&output));
    }
    fn cache(&self) -> PathBuf {
        fs::read_dir(self.0.join("cache"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }
    fn parity(&self, args: &[&str], expected: Option<(bool, u64)>) -> Value {
        let before = tree(&self.0);
        let warm = self.run(args, false);
        let cold = self.run(args, true);
        assert_eq!(warm.status.code(), cold.status.code(), "{args:?}");
        assert_eq!(output_value(&warm), output_value(&cold), "{args:?}");
        let diagnostics = |output: &Output| {
            stderr(output)
                .lines()
                .filter(|line| !line.starts_with("willow-query-cache "))
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert_eq!(diagnostics(&warm), diagnostics(&cold), "{args:?}");
        assert_eq!(tree(&self.0), before, "query wrote files: {args:?}");
        if let Some((loaded, count)) = expected {
            // An incomplete/ambiguous semantic answer can have a nonzero CLI
            // status after successful analysis; its status and output match above.
            let stats = stats(&warm);
            assert_eq!(stats["loaded"], loaded, "{args:?}: {stats}");
            assert_eq!(stats["typechecks"], count, "{args:?}: {stats}");
        }
        output_value(&warm)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}
fn stats(output: &Output) -> Value {
    stderr(output)
        .lines()
        .filter_map(|line| line.strip_prefix("willow-query-cache "))
        .find_map(|line| serde_json::from_str(line).ok())
        .expect("cache statistics")
}
fn output_value(output: &Output) -> Value {
    let text = String::from_utf8_lossy(&output.stdout);
    if text.is_empty() {
        return Value::Null;
    }
    let values: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    if let Some(result) = values
        .iter()
        .find(|value| value["event"] == "analysis.result")
    {
        result["data"].clone()
    } else {
        Value::Array(values)
    }
}
// Compare bytes and modification times, including all paths (not just .cache).
fn tree(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, SystemTime)> {
    let mut files = BTreeMap::new();
    let mut todo = vec![root.to_owned()];
    while let Some(dir) = todo.pop() {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                todo.push(path.clone());
                files.insert(path, (vec![], SystemTime::UNIX_EPOCH));
            } else {
                files.insert(
                    path.clone(),
                    (
                        fs::read(&path).unwrap(),
                        fs::metadata(path).unwrap().modified().unwrap(),
                    ),
                );
            }
        }
    }
    files
}
const SOURCE: &str = "fn first() -> i64 { return 1; } fn second() -> i64 { return first(); } fn main() { println(second()); }";

#[test]
fn first_check_after_shrink_or_replacement_does_not_restore_old_payloads() {
    for count in [16, 64, 256] {
        let mut source = String::from(SOURCE);
        for i in 0..count {
            source.push_str(&format!(" fn extra{i}() -> i64 {{ return 1; }}"));
        }
        let fixture = Fixture::new(&source);
        for edited in [
            SOURCE.to_owned(),
            source.replace("return 1;", "return 999;"),
        ] {
            fixture.write(&source);
            fixture.warm();
            let old_bytes = fs::metadata(fixture.cache()).unwrap().len();
            fixture.write(&edited);
            let first = fixture.run(&["check", "main.wi"], false);
            let clean = fixture.run(&["check", "main.wi"], true);
            assert!(first.status.success(), "{}", stderr(&first));
            assert_eq!(first.status.code(), clean.status.code());
            assert_eq!(first.stdout, clean.stdout);
            let diagnostics = stderr(&first)
                .lines()
                .filter(|line| !line.starts_with("willow-query-cache "))
                .collect::<Vec<_>>()
                .join("\n");
            assert_eq!(diagnostics, stderr(&clean).trim_end());
            let observed = stats(&first);
            assert_eq!(observed["loaded"], false);
            assert_eq!(observed["reused_bodies"], 0);
            assert_eq!(observed["restore_work"]["payload_loads"], 0);
            assert_eq!(observed["restore_work"]["payload_bytes"], 0);
            assert!(
                observed["restore_work"]["admission_bytes"]
                    .as_u64()
                    .unwrap()
                    < old_bytes / 2
            );
            println!(
                "first_check old_bodies={} restore_work={}",
                count + 3,
                observed["restore_work"]
            );
        }
    }
}

#[test]
fn persistent_cache_requires_explicit_opt_in_and_preserves_diagnostics() {
    let fixture = Fixture::new(SOURCE);
    // Even an existing warm cache and a configured cache directory/statistics
    // must not opt users into persistence.
    fixture.warm();
    for source in [
        SOURCE.to_owned(),
        format!("// comment\n{SOURCE}"),
        SOURCE.replace("return 1;", "return 9;"),
        SOURCE.replace("return 1;", "return missing;"),
        SOURCE.replace("return 1;", "return true;"),
        SOURCE.replace("return 1;", "return @;"),
        format!("import std::collections::Array;\n{SOURCE}"),
    ] {
        fixture.write(&source);
        let before = tree(&fixture.0);
        let baseline = fixture.run(&["check", "main.wi", "--warnings"], true);
        for setting in [None, Some(""), Some("0"), Some("true"), Some("2")] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
            command
                .current_dir(&fixture.0)
                .args(["check", "main.wi", "--warnings"])
                .env_remove("WILLOW_QUERY_CACHE")
                .env("WILLOW_QUERY_CACHE_DIR", fixture.0.join("cache"))
                .env("WILLOW_QUERY_CACHE_STATS", "1");
            if let Some(value) = setting {
                command.env("WILLOW_QUERY_CACHE", value);
            }
            let output = command.output().unwrap();
            assert_eq!(output.status.code(), baseline.status.code(), "{setting:?}");
            assert_eq!(output.stdout, baseline.stdout, "{setting:?}");
            assert_eq!(output.stderr, baseline.stderr, "{setting:?}");
            assert_eq!(tree(&fixture.0), before, "{setting:?}");
        }
    }
    fs::remove_dir_all(fixture.0.join("cache")).unwrap();
    fixture.write(SOURCE);
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(&fixture.0)
        .args(["check", "main.wi"])
        .env_remove("WILLOW_QUERY_CACHE")
        .env("WILLOW_QUERY_CACHE_DIR", fixture.0.join("cache"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!fixture.0.join("cache").exists());
}

#[test]
fn readonly_queries_reuse_check_bodies_and_match_cold_in_separate_processes() {
    let fixture = Fixture::new(SOURCE);
    fixture.warm();
    for (source, count) in [
        (SOURCE.to_owned(), 0),
        (SOURCE.replace("return 1;", "return 9;"), 1),
        (SOURCE.replace("return 1;", "println(1); return 1;"), 1),
    ] {
        fixture.write(&source);
        for kind in ["refs", "effects", "symbol", "type", "impact"] {
            let result = fixture.parity(
                &[kind, "first", "--source", "main.wi", "--format", "json"],
                Some((true, count)),
            );
            if kind == "refs" {
                assert_eq!(result[0]["result"]["total"], 1);
            }
        }
        // Batch covers Symbols, SymbolAt and TypeAt, with repeated requests.
        let requests = json!([
            {"kind":"symbols"},
            {"kind":"symbol-at", "file":"main.wi", "byte":source.find("first();").unwrap()},
            {"kind":"type-at", "file":"main.wi", "byte":source.find("first();").unwrap()},
            {"kind":"symbols", "prefix":"fir"},
            {"kind":"symbols"}
        ]);
        fs::write(fixture.0.join("requests.json"), requests.to_string()).unwrap();
        fixture.parity(
            &["query", "main.wi", "--requests", "requests.json"],
            Some((true, count)),
        );
    }
}

#[test]
fn readonly_query_cache_misses_corruption_and_incompatibility_never_write() {
    let fixture = Fixture::new(SOURCE);
    let args = ["refs", "first", "--source", "main.wi", "--format", "json"];
    fixture.parity(&args, Some((false, 3)));
    assert!(!fixture.0.join("cache").exists());
    fixture.warm();
    let path = fixture.cache();
    let valid = fs::read(&path).unwrap();
    for bytes in [
        vec![],
        b"invalid cache".to_vec(),
        valid[..valid.len() - 1].to_vec(),
    ] {
        fs::write(&path, bytes).unwrap();
        fixture.parity(&args, Some((false, 3)));
    }
    // A valid envelope with a mismatched compiler key must also fall back.
    let mut snapshot: Value = serde_json::from_slice(&valid[65..]).unwrap();
    snapshot["key"]["compiler"] = json!("incompatible");
    let payload = serde_json::to_vec(&snapshot).unwrap();
    use sha2::{Digest, Sha256};
    let checksum: String = Sha256::digest(&payload)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut bytes = format!("{checksum}\n").into_bytes();
    bytes.extend(payload);
    fs::write(&path, bytes).unwrap();
    fixture.parity(&args, Some((false, 3)));
    fs::write(&path, valid).unwrap();
    // A changed signature invalidates its users; an invalid body keeps diagnostics.
    for source in [
        SOURCE
            .replace("first() -> i64", "first() -> bool")
            .replace("return 1;", "return true;"),
        SOURCE.replace("return 1;", "return missing;"),
        SOURCE.replace("return 1;", "return ; ; @"),
    ] {
        fixture.write(&source);
        fixture.parity(&args, None);
    }
}

#[test]
fn readonly_query_cache_generic_lambda_and_relocated_facts_match_cold() {
    let source = "fn id<T>(x: T) -> T { return x; } fn first() -> i64 { return id(1); } fn second() -> i64 { let f = || first(); return f(); } fn main() { println(second()); }";
    let fixture = Fixture::new(source);
    fixture.warm();
    for (index, source) in [
        source.to_owned(),
        format!("\n\n{source}"),
        source.replace("id(1)", "id(9)"),
    ]
    .into_iter()
    .enumerate()
    {
        fixture.write(&source);
        for kind in ["refs", "effects", "symbol", "type"] {
            fixture.parity(
                &[kind, "first", "--source", "main.wi", "--format", "json"],
                (index == 0).then_some((true, 0)),
            );
        }
    }
}

#[test]
fn readonly_query_cache_module_scaling_and_signature_invalidation() {
    for modules in [1, 4, 16] {
        let fixture = Fixture::new("");
        let mut source = String::new();
        for i in 0..modules {
            source.push_str(&format!("import lib{i};\n"));
            fs::write(
                fixture.0.join(format!("lib{i}.wi")),
                format!("pub fn value() -> i64 {{ return {i}; }}"),
            )
            .unwrap();
        }
        source.push_str(
            "fn first() -> i64 { return lib0::value(); } fn main() { println(first()); }",
        );
        fixture.write(&source);
        fixture.warm();
        let args = ["refs", "first", "--source", "main.wi", "--format", "json"];
        fixture.parity(&args, Some((true, 0)));
        fs::write(
            fixture.0.join("lib0.wi"),
            "pub fn value() -> i64 { return 99; }",
        )
        .unwrap();
        fixture.parity(&args, Some((true, 1)));
        let observed = fixture.run(&args, false);
        println!(
            "readonly_modules modules={modules} stats={}",
            stats(&observed)
        );
        fs::write(
            fixture.0.join("lib0.wi"),
            "pub fn value() -> bool { return true; }",
        )
        .unwrap();
        fixture.parity(&args, None);
    }
}

#[test]
fn readonly_query_cache_project_discovery_and_package_inputs_match_cold() {
    let fixture = Fixture::new(SOURCE);
    fs::create_dir(fixture.0.join("src")).unwrap();
    fs::rename(fixture.0.join("main.wi"), fixture.0.join("src/main.wi")).unwrap();
    fs::write(
        fixture.0.join("project.toml"),
        "[project]\nname=\"cache-query\"\nversion=\"1.0.0\"\n[willow]\nmanifest-version=1\n",
    )
    .unwrap();
    let checked = fixture.run(&["check", "."], false);
    assert!(checked.status.success(), "{}", stderr(&checked));
    let args = ["refs", "first", "--project-dir", ".", "--format", "json"];
    fixture.parity(&args, Some((true, 0)));
    fs::write(
        fixture.0.join("src/main.wi"),
        SOURCE.replace("return 1;", "return 9;"),
    )
    .unwrap();
    fixture.parity(&args, Some((true, 1)));
    // Discovery includes modules absent from the cached graph.
    fs::write(
        fixture.0.join("src/extra.wi"),
        "pub fn extra() -> i64 { return 7; }",
    )
    .unwrap();
    fixture.parity(&args, None);
    fs::remove_file(fixture.0.join("src/extra.wi")).unwrap();
    fixture.parity(&args, Some((true, 1)));
}

#[test]
fn readonly_query_cache_member_variant_and_binding_facts_match_cold() {
    let source = include_str!("../example/rename_semantic_verification.wi");
    let fixture = Fixture::new(source);
    fixture.warm();
    for selector in [
        "Library",
        "paid_total",
        "digest",
        "Event",
        "Borrowed",
        "submit",
    ] {
        for kind in ["refs", "symbol", "type"] {
            fixture.parity(
                &[kind, selector, "--source", "main.wi", "--format", "json"],
                Some((true, 0)),
            );
        }
    }
    fs::write(
        fixture.0.join("requests.json"),
        json!([
            {"kind":"symbols"},
            {"kind":"symbol-at", "file":"main.wi", "byte":source.find("let tag").unwrap() + 4},
            {"kind":"type-at", "file":"main.wi", "byte":source.find("let tag").unwrap() + 4}
        ])
        .to_string(),
    )
    .unwrap();
    fixture.parity(
        &["query", "main.wi", "--requests", "requests.json"],
        Some((true, 0)),
    );
}

#[test]
fn readonly_query_cache_warning_diagnostics_match_cold_for_each_query_kind() {
    let fixture = Fixture::new(&format!("import std::collections::Array;\n{SOURCE}"));
    fixture.warm();
    for kind in ["refs", "effects", "symbol", "type", "impact"] {
        let result = fixture.parity(
            &[
                kind,
                "first",
                "--source",
                "main.wi",
                "--format",
                "json",
                "--warnings",
            ],
            Some((true, 0)),
        );
        assert_eq!(result[0]["diagnostics"]["warnings"], 1);
    }
    fs::write(
        fixture.0.join("requests.json"),
        json!([{"kind":"symbols"}]).to_string(),
    )
    .unwrap();
    fixture.parity(
        &["query", "main.wi", "--requests", "requests.json"],
        Some((true, 0)),
    );
}

#[test]
fn readonly_query_cache_isolated_candidate_rejects_foreign_entry() {
    let original = Fixture::new(SOURCE);
    original.warm();
    let candidate = Fixture::new(&SOURCE.replace("return 1;", "return 9;"));
    // Put the foreign entry snapshot at the candidates expected cache path.
    // Its envelope is intact; input identity, not corruption, rejects it.
    candidate.warm();
    fs::copy(original.cache(), candidate.cache()).unwrap();
    candidate.parity(
        &["refs", "first", "--source", "main.wi", "--format", "json"],
        Some((false, 3)),
    );
    assert_eq!(
        fs::read_to_string(original.0.join("main.wi")).unwrap(),
        SOURCE
    );
}

// Qualified constructors in an imported editor module must not make its effect
// inventory depend on itself, including with inherited interface implementations.
#[test]
fn editor_effect_inventory_cache_check_run_and_queries_match_disabled() {
    let fixture = Fixture::new(include_str!("../example/effect_inventory_editor/m2.wi"));
    for (name, source) in [
        (
            "edit.wi",
            include_str!("../example/effect_inventory_editor/edit.wi"),
        ),
        (
            "buffer.wi",
            include_str!("../example/effect_inventory_editor/buffer.wi"),
        ),
    ] {
        fs::write(fixture.0.join(name), source).unwrap();
    }
    let diagnostics = |output: &Output| {
        stderr(output)
            .lines()
            .filter(|line| !line.starts_with("willow-query-cache "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    for command in ["check", "run"] {
        let cache = fixture.0.join("cache");
        if cache.exists() {
            fs::remove_dir_all(cache).unwrap();
        }
        let disabled = fixture.run(&[command, "main.wi"], true);
        assert!(
            disabled.status.success(),
            "{command}: {}",
            stderr(&disabled)
        );
        for warm in [false, true] {
            let cached = fixture.run(&[command, "main.wi"], false);
            assert!(
                cached.status.success(),
                "{command}, warm={warm}: {}",
                stderr(&cached)
            );
            assert_eq!(cached.stdout, disabled.stdout, "{command}, warm={warm}");
            if command == "check" {
                assert_eq!(diagnostics(&cached), diagnostics(&disabled));
            }
            assert_eq!(stats(&cached)["loaded"], warm);
            if command == "run" {
                assert_eq!(String::from_utf8_lossy(&cached.stdout).trim(), "ok");
            }
        }
    }
    for kind in ["symbol", "type", "refs", "effects", "impact"] {
        fs::remove_dir_all(fixture.0.join("cache")).unwrap();
        let args = [kind, "main", "--source", "main.wi", "--format", "json"];
        fixture.parity(&args, Some((false, 27)));
        fixture.warm();
        fixture.parity(&args, Some((true, 0)));
    }
}
