use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new(project: bool) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-overview-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(path.join("src/sub")).unwrap();
        if project {
            fs::write(path.join("project.toml"), "[willow]\nmanifest-version=1\n[project]\nname='overview'\nversion='0.1.0'\nentry='src/main.wi'\n").unwrap();
        }
        fs::write(path.join("src/main.wi"), "fn zebra(n: i64) -> i64 { let local = n; return local; }\nclass Boxed { pub qty: i64; pub init(self, qty: i64) { self.qty = qty; } pub fn value(self) -> i64 { return self.qty; } }\nfn alpha() {}\nfn main() {}\n").unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
    fn json(&self, args: &[&str], code: i32) -> Value {
        let mut args = args.to_vec();
        args.extend(["--format", "json"]);
        let out = self.run(&args);
        assert_eq!(
            out.status.code(),
            Some(code),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
    fn source(&self, name: &str, source: &str) {
        fs::write(self.0.join(name), source).unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn file_structure_source_order_types_and_exclusions() {
    let f = Fixture::new(false);
    let v = f.json(&["overview", "src/main.wi"], 0);
    let s = v["files"][0]["symbols"].as_array().unwrap();
    assert_eq!(
        s.iter()
            .map(|s| s["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["zebra", "Boxed", "alpha", "main"]
    );
    assert_eq!(s[0]["type_display"], "fn(i64) -> i64");
    let m = s[1]["members"].as_array().unwrap();
    assert_eq!(
        m.iter()
            .map(|s| s["kind"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["field", "constructor", "method"]
    );
    assert_eq!(m[0]["type_display"], "i64");
    assert_eq!(m[2]["type_display"], "fn() -> i64");
    assert!(!v.to_string().contains("local"));
    assert!(!v.to_string().contains("\"parameter\""));
    assert_eq!(v["counts"]["members"], 3);
}
#[test]
fn depth_zero_and_member_filter() {
    let f = Fixture::new(false);
    let v = f.json(&["overview", "src/main.wi", "--depth", "0"], 0);
    assert_eq!(v["files"][0]["symbols"][1]["member_count"], 3);
    assert!(v["files"][0]["symbols"][1].get("members").is_none());
    assert_eq!(v["counts"]["hidden_members"], 3);
    let v = f.json(&["overview", "src/main.wi", "--kind", "field"], 0);
    assert_eq!(v["files"][0]["symbols"].as_array().unwrap().len(), 1);
    assert_eq!(v["files"][0]["symbols"][0]["members"][0]["name"], "qty");
}
#[test]
fn project_includes_unimported_sources_and_narrows_directory() {
    let f = Fixture::new(true);
    f.source("src/sub/unused.wi", "pub fn hidden() -> i64 { return 1; }");
    let v = f.json(&["overview"], 0);
    assert_eq!(v["requested_depth"], 0);
    assert_eq!(v["counts"]["files"], 2);
    assert!(v.to_string().contains("hidden"));
    let v = f.json(&["overview", "src/sub"], 0);
    assert_eq!(v["counts"]["files"], 1);
    assert_eq!(v["files"][0]["path"], "src/sub/unused.wi");
    f.source("src/main.wi", "fn main() { println(hidden()); }");
    let v = f.json(&["overview"], 1);
    assert_eq!(v["status"], "incomplete");
    assert!(v["root_errors"].as_u64().unwrap() > 0);
    assert!(v["hint"].as_str().unwrap().contains("willow check"));
}
#[test]
fn project_selectors_round_trip_and_imports_are_hidden() {
    let f = Fixture::new(true);
    f.source("src/sub/unused.wi", "pub fn alpha() {} ");
    let v = f.json(&["overview", "src/main.wi", "--all"], 0);
    let symbols = v["files"][0]["symbols"].as_array().unwrap();
    for symbol in symbols.iter().chain(
        symbols
            .iter()
            .flat_map(|s| s["members"].as_array().into_iter().flatten()),
    ) {
        let selector = symbol["selector"].as_str().unwrap();
        assert_eq!(
            f.json(&["symbol", selector], 0)["status"],
            "ok",
            "{selector}"
        );
        assert_eq!(f.json(&["refs", selector], 0)["status"], "ok", "{selector}");
    }
    let selector = symbols[0]["selector"].as_str().unwrap();
    assert_eq!(
        f.json(&["rename", selector, "renamed", "--dry-run"], 0)["status"],
        "ok"
    );
    f.source(
        "src/main.wi",
        "import sub::unused; fn main() { unused::alpha(); }",
    );
    let v = f.json(&["overview", "src/main.wi"], 0);
    assert_eq!(v["counts"]["top_level_symbols"], 1);
}
#[test]
fn formats_budgets_paths_and_argument_errors() {
    let f = Fixture::new(false);
    for format in ["human", "json", "ndjson"] {
        let out = f.run(&["overview", "src/main.wi", "--format", format]);
        assert!(out.status.success());
        if format != "human" {
            let _: Value = serde_json::from_slice(&out.stdout).unwrap();
        } else {
            let s = String::from_utf8(out.stdout).unwrap();
            assert!(!s.contains("return "));
            assert!(!s.contains("location"));
        }
    }
    let full = f.run(&["overview", "src/main.wi", "--all", "--format=json"]);
    let shallow = f.run(&[
        "overview",
        "src/main.wi",
        "--depth=0",
        "--all",
        "--format=json",
    ]);
    let budget = ((full.stdout.len() + shallow.stdout.len()) / 2).to_string();
    let v = f.json(&["overview", "src/main.wi", "--max-chars", &budget], 0);
    assert_eq!(v["effective_depth"], 0);
    assert_eq!(v["fallback"], "depth0");
    let v = f.json(&["overview", "src/main.wi", "--max-chars=1"], 0);
    assert!(v["effective_depth"].is_null());
    assert_eq!(v["truncated"], true);
    assert!(v["hint"].as_str().unwrap().starts_with("Narrow the path"));
    let v = f.json(&["overview", "src/main.wi", "--max-chars=1", "--all"], 0);
    assert_eq!(v["truncated"], false);
    let v = f.json(&["overview", "src/main.wi", "--absolute-paths"], 0);
    assert!(std::path::Path::new(v["files"][0]["path"].as_str().unwrap()).is_absolute());
    for flag in [
        "--depth=2",
        "--max-chars=0",
        "--kind=nope",
        "--format=nope",
        "--unknown",
    ] {
        assert_eq!(
            f.run(&["overview", "src/main.wi", flag]).status.code(),
            Some(2)
        );
    }
    assert_eq!(f.json(&["overview", "missing.wi"], 1)["status"], "error");
    assert!(
        f.json(&["overview", "."], 1)["message"]
            .as_str()
            .unwrap()
            .contains("directory overview requires a Willow project")
    );
}
#[test]
fn interfaces_enum_inheritance_static_members_and_duplicate_owners() {
    let f = Fixture::new(false);
    f.source("src/main.wi", "interface I { fn required(self) -> i64; fn defaulted(self) -> i64 { return 2; } }\nopen class A implements I { pub static total: i64 = 0; pub fn required(self) -> i64 { return 1; } }\nclass B extends A { pub fn own(self) {} }\nenum Choice { First, Second }\nclass Other { pub fn required(self) -> i64 { return 3; } }\nfn main() {}\n");
    let v = f.json(&["overview", "src/main.wi"], 0);
    let s = v["files"][0]["symbols"].as_array().unwrap();
    assert_eq!(s[0]["members"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!(s[1]["members"].as_array().unwrap().len(), 2, "{v}");
    assert_eq!(s[2]["members"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(s[3]["members"].as_array().unwrap().len(), 2, "{v}");
    assert_ne!(
        s[1]["members"][1]["selector"],
        s[4]["members"][0]["selector"]
    );
    assert_eq!(s[1]["members"][0]["kind"], "static-field");
}
#[test]
fn unicode_path_and_names_are_not_byte_budgeted() {
    let f = Fixture::new(false);
    f.source(
        "src/日本語.wi",
        "fn calculate(value: i64) -> i64 { return value; } fn main() {} ",
    );
    let v = f.json(&["overview", "src/日本語.wi"], 0);
    assert_eq!(v["files"][0]["symbols"][0]["name"], "calculate");
    let out = f.run(&["overview", "src/日本語.wi", "--all"]);
    let text = String::from_utf8(out.stdout).unwrap();
    let budget = (text.chars().count() + 8).to_string();
    let out = f.run(&["overview", "src/日本語.wi", "--max-chars", &budget]);
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("truncated=false"), "{text}");
}
#[test]
fn help_and_agent_overview_workflow() {
    let f = Fixture::new(true);
    assert!(
        String::from_utf8(f.run(&["--help"]).stdout)
            .unwrap()
            .contains("willow overview [PATH]")
    );
    for agent in ["codex", "claude"] {
        let v = f.json(&["agent", "instructions", agent], 0);
        assert_eq!(v["capabilities"]["direct_overview"], true);
        assert_eq!(v["instruction_schema"], 4);
        let text = v["markdown"].as_str().unwrap();
        assert!(text.contains("narrow the path before"));
        assert!(text.contains("target symbol is already known"));
    }
    for name in ["AGENTS.md", "CLAUDE.md"] {
        f.source(name, "user before\n<!-- BEGIN WILLOW MANAGED -->\n<!-- willow-instruction-schema: 3 -->\nold\n<!-- END WILLOW MANAGED -->\nuser after\n");
    }
    assert!(f.run(&["agent", "sync", "--yes"]).status.success());
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let s = fs::read_to_string(f.0.join(name)).unwrap();
        assert!(s.starts_with("user before\n"));
        assert!(s.ends_with("user after\n"));
        assert!(s.contains("willow overview ."));
        assert!(s.contains("schema: 4"));
    }
}
#[test]
fn constructor_overloads_have_distinct_reusable_selectors() {
    let f = Fixture::new(false);
    f.source(
        "src/main.wi",
        "class A { pub init(self) {} pub init(self, n: i64) {} } fn main() {} ",
    );
    let v = f.json(&["overview", "src/main.wi"], 0);
    let members = v["files"][0]["symbols"][0]["members"].as_array().unwrap();
    assert_eq!(members.len(), 2, "{v}");
    assert_ne!(members[0]["selector"], members[1]["selector"]);
    for m in members {
        let selector = m["selector"].as_str().unwrap();
        assert_eq!(
            f.json(&["symbol", selector, "--source", "src/main.wi"], 0)["status"],
            "ok"
        );
        for command in ["effects", "impact"] {
            assert_eq!(
                f.json(&[command, selector, "--source", "src/main.wi"], 0)["status"],
                "ok",
                "{command}: {selector}"
            );
        }
    }
}

#[test]
fn explicit_file_outside_analyzed_source_graph_reports_error() {
    let f = Fixture::new(true);
    f.source("extra.wi", "pub fn extra() {} ");
    let result = f.json(&["overview", "extra.wi"], 1);
    assert_eq!(result["status"], "error");
    assert!(
        result["message"]
            .as_str()
            .unwrap()
            .contains("outside analyzed source graph")
    );
}

#[cfg(unix)]
#[test]
fn directory_symlink_alias_is_deduplicated_for_overview() {
    let f = Fixture::new(true);
    f.source("src/sub/real.wi", "pub fn real() {} ");
    std::os::unix::fs::symlink(f.0.join("src/sub"), f.0.join("src/alias")).unwrap();
    let result = f.json(&["overview"], 0);
    let paths: Vec<_> = result["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&"src/sub/real.wi"), "{result}");
    assert!(
        !paths.iter().any(|p| p.starts_with("src/alias/")),
        "{result}"
    );
}
#[test]
fn canonical_types_for_imported_and_generic_declarations() {
    let f = Fixture::new(true);
    f.source("src/model.wi", "pub class Thing {} ");
    f.source("src/main.wi", "import std::collections::Array; import std::collections::Map; import model; fn types(a: Option<i64>, b: Result<i64, String>, c: Array<i64>, d: Map<String, i64>, e: Channel<i64>, f: model::Thing) {} fn main() {} ");
    let v = f.json(&["overview", "src/main.wi"], 0);
    let ty = v["files"][0]["symbols"][0]["type_display"]
        .as_str()
        .unwrap();
    for expected in [
        "Option<i64>",
        "Result<i64, String>",
        "Array<i64>",
        "Map<String, i64>",
        "Channel<i64>",
        "model::Thing",
    ] {
        assert!(ty.contains(expected), "{ty}");
    }
    assert!(!v.to_string().contains("$pkg"));
}
#[test]
fn unimported_nested_module_selectors_support_direct_queries_and_rename() {
    let f = Fixture::new(true);
    f.source(
        "src/sub/unused.wi",
        "pub fn hidden() -> i64 { return 1; } pub fn caller() -> i64 { return hidden(); }",
    );
    let v = f.json(&["overview", "src/sub"], 0);
    let selector = v["files"][0]["symbols"][0]["selector"].as_str().unwrap();
    for command in ["symbol", "refs", "type", "effects", "impact"] {
        assert_eq!(f.json(&[command, selector], 0)["status"], "ok");
    }
    assert_eq!(
        f.json(&["rename", selector, "renamed", "--dry-run"], 0)["status"],
        "ok"
    );
    assert!(
        fs::read_to_string(f.0.join("src/sub/unused.wi"))
            .unwrap()
            .contains("hidden")
    );
}
#[test]
fn imported_container_members_are_source_owned_and_paths_are_canonical() {
    let f = Fixture::new(true);
    f.source("src/model.wi", "pub class Thing { pub value: i64; pub init(self, value: i64) { self.value = value; } pub fn get(self) -> i64 { return self.value; } }");
    f.source(
        "src/main.wi",
        "import model; fn main() { let x = new model::Thing(1); println(x.get()); }",
    );
    let v = f.json(&["overview", "src/sub/../model.wi"], 0);
    assert_eq!(v["files"][0]["path"], "src/model.wi");
    assert_eq!(v["files"][0]["module"], "model");
    assert_eq!(
        v["files"][0]["symbols"][0]["members"]
            .as_array()
            .unwrap()
            .len(),
        3,
        "{v}"
    );
    assert_eq!(
        v["files"][0]["symbols"][0]["members"][2]["selector"],
        "model::Thing::get"
    );
}
