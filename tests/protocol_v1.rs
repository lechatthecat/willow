use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new(source: &str) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-impact-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("main.wi"), source).unwrap();
        Self(path)
    }
    fn write(&self, name: &str, source: &str) {
        fs::write(self.0.join(name), source).unwrap();
    }
    fn run(&self, args: &[&str], exit: i32) -> Vec<Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap();
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            output.status.code(),
            Some(exit),
            "{args:?}\n{text}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let events: Vec<Value> = text
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(events.first().unwrap()["event"], "request.started");
        assert_eq!(events.last().unwrap()["event"], "request.finished");
        for (i, event) in events.iter().enumerate() {
            assert_eq!(event["seq"], i);
            assert_eq!(event["stream_id"], events[0]["stream_id"]);
        }
        events
    }
    fn result(&self, args: &[&str]) -> Value {
        self.run(args, 0)
            .into_iter()
            .find(|v| v["event"] == "analysis.result")
            .unwrap()["data"]
            .clone()
    }
    fn analyze(&self) -> willow_compiler::ai::Snapshot {
        self.analyze_mode(false)
    }
    fn analyze_mode(&self, project: bool) -> willow_compiler::ai::Snapshot {
        willow_compiler::CompilerSession::new(
            self.0.join("main.wi").to_str().unwrap(),
            "",
            &willow_compiler::CompilerOptions::debug(),
            project.then(|| self.0.clone()),
        )
        .analysis_with_emitter(&mut willow_compiler::diagnostics::HumanEmitter)
        .unwrap()
    }
    fn facts(&self) -> Value {
        let mut value = serde_json::to_value(self.analyze()).unwrap();
        self.write("revision-query.json", "[]");
        value["revision"] = self.result(&["query", "main.wi", "--requests", "revision-query.json"])
            ["revision"]
            .clone();
        value
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn named<'a>(s: &'a Value, name: &str) -> &'a Value {
    s["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == name)
        .unwrap_or_else(|| panic!("missing {name}: {s}"))
}

#[test]
fn position_callers_callees_levels_recursion_and_limits() {
    let source = "fn leaf() {} fn middle() { leaf(); leaf(); } fn main() { middle(); }";
    let f = Fixture::new(source);
    let result = f.result(&["impact", "main.wi", "--file", "main.wi", "--byte", "4"]);
    let nodes = result["impact"]["nodes"].as_array().unwrap();
    for (name, level) in [("leaf", 1), ("middle", 2), ("main", 3)] {
        let id = &named(&result, name)["id"];
        assert_eq!(
            nodes.iter().find(|n| &n["id"] == id).unwrap()["level"],
            level
        );
    }
    assert_eq!(result["impact"]["edge_visits"], 2);
    assert_eq!(result["impact"]["unknown"], false);
    assert_eq!(result["impact"]["truncated"], false);
    let limited = f.result(&[
        "impact",
        "main.wi",
        "--file",
        "main.wi",
        "--byte",
        "4",
        "--max-depth",
        "0",
    ]);
    assert_eq!(limited["impact"]["truncated"], true);
    assert_eq!(limited["impact"]["nodes"].as_array().unwrap().len(), 1);
    let byte = source.find("main").unwrap().to_string();
    let forward = f.result(&[
        "impact",
        "main.wi",
        "--file",
        "main.wi",
        "--byte",
        &byte,
        "--direction",
        "callees",
    ]);
    assert_eq!(forward["impact"]["nodes"].as_array().unwrap().len(), 3);
    f.run(
        &["impact", "main.wi", "--file", "main.wi", "--byte", "9999"],
        1,
    );
    f.write(
        "main.wi",
        "fn a() { b(); } fn b() { a(); } fn main() { a(); }",
    );
    let cycle = f.result(&["impact", "main.wi", "--file", "main.wi", "--byte", "4"]);
    assert_eq!(cycle["impact"]["nodes"].as_array().unwrap().len(), 3);
}

#[test]
fn modules_same_names_item_and_module_aliases() {
    let f = Fixture::new(
        "import a::work as task; import b as other; fn main() { task(); other::work(); }",
    );
    f.write("a.wi", "pub fn work() { leaf(); } fn leaf() {}");
    f.write("b.wi", "pub fn work() {}");
    let result = f.result(&["impact", "main.wi", "--file", "a.wi", "--byte", "30"]);
    let names: Vec<_> = result["functions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 3, "{result}");
    assert!(names.contains(&"main"));
    assert!(names.contains(&"work"));
    assert!(names.contains(&"leaf"));
    let snapshot = f.facts();
    let work: Vec<_> = snapshot["functions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["name"] == "work")
        .collect();
    assert_eq!(work.len(), 2);
    assert_ne!(work[0]["id"], work[1]["id"]);
    assert_eq!(
        named(&snapshot, "main")["callees"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn in_memory_comparison_without_git_empty_diff_effects_deleted_edges_and_rename() {
    let f = Fixture::new("fn leaf() {} fn main() { leaf(); }");
    let before_facts = f.facts();
    let before = f.analyze();
    f.write(
        "main.wi",
        "// comment\nfn leaf() { }\nfn main() { leaf(); }\n",
    );
    let whitespace = f.analyze();
    assert!(
        difference(&before, &whitespace)["changes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.write("main.wi", "fn leaf() { println(1); } fn main() { leaf(); }");
    let effects = f.analyze();
    let diff = difference(&before, &effects);
    assert!(
        diff["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["added_effects"].as_u64().unwrap() > 0)
    );
    let reverse = difference(&effects, &before);
    assert!(
        reverse["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["removed_effects"].as_u64().unwrap() > 0)
    );
    f.write("main.wi", "fn main() {}");
    let deleted = f.analyze();
    let diff = difference(&before, &deleted);
    assert!(
        diff["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "deleted")
    );
    assert!(
        diff["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| !c["removed_callees"].as_array().unwrap().is_empty())
    );
    assert_eq!(diff["before_impact"]["nodes"].as_array().unwrap().len(), 2);
    f.write("main.wi", "fn renamed() {} fn main() { renamed(); }");
    let rename = f.analyze();
    assert!(
        difference(&before, &rename)["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["kind"] == "renamed")
    );
    f.run(
        &[
            "impact",
            "main.wi",
            "--function",
            named(&before_facts, "leaf")["id"].as_str().unwrap(),
            "--revision",
            before_facts["revision"].as_str().unwrap(),
        ],
        1,
    );
    assert!(!f.0.join(".git").exists());
}

#[test]
fn unknown_indirect_effects_and_virtual_interface_dispatch() {
    let f = Fixture::new("fn indirect(f: fn() -> i64) -> i64 { return f(); } fn main() {}");
    let snapshot = f.facts();
    assert_eq!(named(&snapshot, "indirect")["unknown"], true);
    assert_eq!(named(&snapshot, "indirect")["runtime_effects"], 63);
    f.write("main.wi", "interface I { fn run(self) -> i64; } fn caller(x: I) -> i64 { return x.run(); } fn main() {}");
    let unknown = f.facts();
    assert_eq!(named(&unknown, "caller")["unknown"], true);
    assert_eq!(named(&unknown, "caller")["runtime_effects"], 63);
    let source = "interface I { fn run(self) -> i64; } class A implements I { pub fn run(self) -> i64 { return 1; } } fn call(x: I) -> i64 { return x.run(); } fn main() {}";
    f.write("main.wi", source);
    let snapshot = f.facts();
    let method = snapshot["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"].as_str().unwrap().contains("A") && !v["synthetic"].as_bool().unwrap())
        .unwrap();
    let result = f.result(&[
        "impact",
        "main.wi",
        "--function",
        method["id"].as_str().unwrap(),
        "--revision",
        snapshot["revision"].as_str().unwrap(),
    ]);
    let call = named(&result, "call");
    let node = result["impact"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["id"] == call["id"])
        .unwrap();
    assert_eq!(node["level"], 2, "{result}");
    assert!(node["graph_distance"].as_u64().unwrap() >= 1);
    f.write("main.wi","open class Base { pub open fn run(self) -> i64 { return 1; } } class Child extends Base { pub override fn run(self) -> i64 { return 2; } } fn call(x: Base) -> i64 { return x.run(); } fn main() {}");
    let snapshot = f.facts();
    let callees = named(&snapshot, "call")["callees"].as_array().unwrap();
    let concrete = snapshot["functions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| !f["synthetic"].as_bool().unwrap() && callees.contains(&f["id"]))
        .count();
    assert_eq!(concrete, 2, "{snapshot}");
}

#[test]
fn project_configuration_and_dependency_scope_are_checked() {
    let f = Fixture::new("import helper; fn main() { helper::work(); }");
    f.write("helper.wi", "pub fn work() {}");
    f.write(
        "project.toml",
        "[project]\nname = \"impact_test\"\nversion = \"0.1.0\"\nentry = \"main.wi\"\n",
    );
    let before = f.analyze_mode(true);
    // Files outside the resolved source closure do not change this revision.
    f.write("unrelated.wi", "fn unused() { println(1); }");
    let unrelated = f.analyze_mode(true);
    let difference = difference(&before, &unrelated);
    assert_eq!(difference["before_revision"], difference["after_revision"]);
    assert!(difference["changes"].as_array().unwrap().is_empty());
    f.write(
        "project.toml",
        "[project]\nname = \"impact_test\"\nversion = \"0.2.0\"\nentry = \"main.wi\"\n",
    );
    assert!(
        before
            .compare(&f.analyze_mode(true), Default::default())
            .is_err()
    );
}

#[test]
fn invalid_arguments_diagnostics_positions_and_live_references() {
    let f = Fixture::new("fn a() {} fn b() {} fn main() { a(); b(); }");
    let snapshot = f.facts();
    let a = named(&snapshot, "a")["id"].as_str().unwrap();
    let b = named(&snapshot, "b")["id"].as_str().unwrap();
    let revision = snapshot["revision"].as_str().unwrap();
    let result = f.result(&[
        "impact",
        "main.wi",
        "--function",
        a,
        "--function",
        b,
        "--revision",
        revision,
    ]);
    assert_eq!(result["impact"]["nodes"].as_array().unwrap().len(), 3);
    for args in [
        vec!["impact", "main.wi"],
        vec!["impact", "main.wi", "--file", "main.wi"],
        vec!["impact", "main.wi", "--function", a],
        vec![
            "impact",
            "main.wi",
            "--file",
            "main.wi",
            "--byte",
            "0",
            "--max-nodes",
            "0",
        ],
    ] {
        f.run(&args, 2);
    }
    f.run(
        &[
            "impact",
            "main.wi",
            "--file",
            "main.wi",
            "--byte",
            "0",
            "--protocol-version",
            "99",
        ],
        2,
    );
    let human = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(&f.0)
        .args([
            "impact", "main.wi", "--file", "main.wi", "--byte", "3", "--format", "human",
        ])
        .output()
        .unwrap();
    assert!(human.status.success());
    let result: Value = serde_json::from_slice(&human.stdout).unwrap();
    assert_eq!(result["kind"], "impact");
    f.write("main.wi", "fn main() { missing(); }");
    let events = f.run(
        &["impact", "main.wi", "--file", "main.wi", "--byte", "0"],
        1,
    );
    assert!(events.iter().any(|e| e["event"] == "diagnostic"));
    assert_eq!(events.last().unwrap()["code"], "WT2001");
}

#[test]
fn copied_defaults_report_ambiguity_instead_of_selecting_an_implementation() {
    let source = "interface I { fn run(self) -> i64 { return 1; } } class A implements I {} class B implements I {} fn main() {}";
    let f = Fixture::new(source);
    let byte = source.find("return").unwrap().to_string();
    let events = f.run(
        &["impact", "main.wi", "--file", "main.wi", "--byte", &byte],
        1,
    );
    assert!(
        events.last().unwrap()["data"]["message"]
            .as_str()
            .unwrap()
            .contains("source position resolves to")
    );
}

#[test]
fn interface_dispatch_crosses_modules_and_repeated_import_spellings() {
    let f = Fixture::new(
        "import api; import impls as concrete; import impls::A as Alias; fn main() { let x: api::I = new Alias(); api::call(x); }",
    );
    f.write(
        "api.wi",
        "pub interface I { fn run(self) -> i64; } pub fn call(x: I) -> i64 { return x.run(); }",
    );
    f.write(
        "impls.wi",
        "import api; pub class A implements api::I { pub fn run(self) -> i64 { return 7; } }",
    );
    let snapshot = f.facts();
    let method = snapshot["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "A::run")
        .unwrap();
    let result = f.result(&[
        "impact",
        "main.wi",
        "--function",
        method["id"].as_str().unwrap(),
        "--revision",
        snapshot["revision"].as_str().unwrap(),
    ]);
    let call = named(&result, "call");
    let nodes = result["impact"]["nodes"].as_array().unwrap();
    assert_eq!(
        nodes.iter().find(|v| v["id"] == call["id"]).unwrap()["level"],
        2,
        "{result}"
    );
    named(&result, "main");
}

#[test]
fn virtual_dispatch_in_dependency_reaches_subclass_defined_by_consumer() {
    let f = Fixture::new(
        "import base; class Child extends base::Base { pub override fn run(self) -> i64 { return 2; } } fn main() { base::call(new Child()); }",
    );
    f.write("base.wi","pub open class Base { pub open fn run(self) -> i64 { return 1; } } pub fn call(x: Base) -> i64 { return x.run(); }");
    let snapshot = f.facts();
    let child = named(&snapshot, "Child::run");
    let result = f.result(&[
        "impact",
        "main.wi",
        "--function",
        child["id"].as_str().unwrap(),
        "--revision",
        snapshot["revision"].as_str().unwrap(),
    ]);
    let call = named(&result, "call");
    assert_eq!(
        result["impact"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == call["id"])
            .unwrap()["level"],
        2
    );
    named(&result, "main");
}

#[test]
fn nested_expression_and_block_lambdas_own_their_source_positions() {
    for source in [
        "fn main() { let f = |x: i64| x + 1; println(f(1)); }",
        "fn main() { let f = |x: i64| { return x + 1; }; println(f(1)); }",
    ] {
        let f = Fixture::new(source);
        let byte = source.find("x + 1").unwrap().to_string();
        let result = f.result(&["impact", "main.wi", "--file", "main.wi", "--byte", &byte]);
        let functions = result["functions"].as_array().unwrap();
        assert_eq!(
            result["impact"]["unknown"], true,
            "indirect callers must make reverse coverage incomplete"
        );
        assert!(
            functions
                .iter()
                .any(|f| f["name"].as_str().unwrap().contains("lambda")),
            "{result}"
        );
        assert!(!functions.iter().any(|f| f["name"] == "main"), "{result}");
        let first = f.analyze();
        let second = f.analyze();
        let diff = difference(&first, &second);
        assert_eq!(diff["before_revision"], diff["after_revision"]);
        assert!(diff["changes"].as_array().unwrap().is_empty());
    }
}

fn difference(
    before: &willow_compiler::ai::Snapshot,
    after: &willow_compiler::ai::Snapshot,
) -> Value {
    serde_json::to_value(before.compare(after, Default::default()).unwrap()).unwrap()
}
