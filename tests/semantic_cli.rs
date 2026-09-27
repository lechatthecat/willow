use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-semantic-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("project.toml"),"[willow]\nmanifest-version=1\n[project]\nname='direct'\nversion='0.1.0'\nentry='src/main.wi'\n").unwrap();
        fs::write(root.join("src/order.wi"),"module order;\npub class Order {\n    pub qty: i64;\n    pub init(self, qty: i64) { self.qty = qty; }\n    pub fn value(self) -> i64 { return self.qty; }\n}\npub fn submit(n: i64) -> i64 { return n + 1; }\n").unwrap();
        fs::write(root.join("src/main.wi"),"import order;\nfn submit(n: i64) -> i64 { return order::submit(n); }\nfn main() { let item = new order::Order(2); println(submit(item.value())); }\n").unwrap();
        Self(root)
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
        let output = self.run(&args);
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn direct_queries_resolve_fields_functions_and_locations() {
    let f = Fixture::new();
    for args in [
        vec!["refs", "order::Order::qty"],
        vec!["references", "order::Order::qty"],
        vec!["symbol", "order::Order::qty"],
        vec!["type", "src/order.wi:3:9"],
        vec!["refs", "src/order.wi:3:9"],
        vec!["effects", "order::submit"],
        vec!["impact", "order::submit"],
    ] {
        let value = f.json(&args, 0);
        assert_eq!(value["schema"], 1);
        assert_eq!(value["status"], "ok", "{value}");
    }
    let refs = f.json(&["refs", "order::Order::qty"], 0);
    assert_eq!(refs["result"]["total"], 2);
    for reference in refs["result"]["references"].as_array().unwrap() {
        assert_eq!(reference["location"]["path"], "src/order.wi");
        assert!(reference["location"]["line"].as_u64().unwrap() > 0);
    }
    let ambiguous = f.json(&["symbol", "submit"], 1);
    assert_eq!(ambiguous["status"], "ambiguous");
    for candidate in ambiguous["result"]["candidates"].as_array().unwrap() {
        f.json(&["symbol", candidate["selector"].as_str().unwrap()], 0);
    }
    f.json(
        &[
            "symbol",
            "submit",
            "--module",
            "order",
            "--kind",
            "function",
            "--package",
            "direct",
        ],
        0,
    );
    assert_eq!(f.json(&["refs", "not_existing"], 1)["status"], "unknown");
    assert!(!f.0.join(".willow-edits").exists());
    assert!(!f.0.join("snapshot.json").exists());
}

#[test]
fn direct_cli_formats_errors_discovery_and_legacy_compatibility() {
    let f = Fixture::new();
    let human = f.run(&["refs", "order::Order::qty"]);
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("total=2 shown=2 truncated=false"), "{text}");
    assert!(text.contains("Coverage:"));
    for args in [
        vec!["symbol"],
        vec!["refs", "submit", "--bad"],
        vec![
            "refs",
            "submit",
            "--source",
            "src/main.wi",
            "--project-dir",
            ".",
        ],
        vec!["refs", "submit", "--kind", "function", "--kind", "function"],
    ] {
        assert_eq!(f.json(&args, 2)["status"], "invalid-arguments");
    }
    for selector in [
        "src/order.wi:0:1",
        "src/order.wi:3:0",
        "src/order.wi:999:1",
        "src/order.wi:3:999",
        "src/order.wi:x:2",
        "missing.wi:1:1",
    ] {
        let value = f.json(&["type", selector], 1);
        assert!(
            value["message"]
                .as_str()
                .unwrap()
                .contains("invalid-location"),
            "{value}"
        );
    }
    let ndjson = f.run(&["symbol", "order::submit", "--format=ndjson"]);
    assert!(ndjson.status.success());
    assert_eq!(String::from_utf8(ndjson.stdout).unwrap().lines().count(), 1);
    let nested = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(f.0.join("src"))
        .args(["symbol", "order::submit", "--format=json"])
        .output()
        .unwrap();
    assert!(
        nested.status.success(),
        "{}",
        String::from_utf8_lossy(&nested.stdout)
    );
    let check = f.run(&["check", ".", "--format=ndjson"]);
    assert!(check.status.success());
    assert!(
        String::from_utf8(check.stdout)
            .unwrap()
            .contains("request.finished")
    );
}

#[test]
fn references_are_bounded_and_agree_with_low_level_queries() {
    let f = Fixture::new();
    let calls = "order::submit(1);".repeat(500);
    fs::write(
        f.0.join("src/main.wi"),
        format!("import order; fn main() {{ {calls} }}"),
    )
    .unwrap();
    let short = f.json(&["refs", "order::submit"], 0);
    assert_eq!(short["result"]["total"], 500);
    assert_eq!(short["result"]["shown"], 50);
    assert_eq!(short["result"]["truncated"], true);
    let all = f.json(&["refs", "order::submit", "--all"], 0);
    assert_eq!(all["result"]["shown"], 500);
    let id = all["result"]["selected"]["id"].as_str().unwrap();
    fs::write(
        f.0.join("requests.json"),
        serde_json::json!([{"kind":"references","function":id}]).to_string(),
    )
    .unwrap();
    let low = f.run(&["query", ".", "--requests", "requests.json"]);
    assert!(
        low.status.success(),
        "{}",
        String::from_utf8_lossy(&low.stderr)
    );
    let events: Vec<Value> = String::from_utf8(low.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let low = &events
        .iter()
        .find(|event| event["event"] == "analysis.result")
        .expect("query emits analysis.result")["data"]["results"][0]["result"];
    fn normalized(references: &Value) -> Vec<Value> {
        fn paths(value: &mut Value) {
            match value {
                Value::Object(fields) => {
                    if let Some(Value::String(path)) = fields.get_mut("path") {
                        if path == "${workspace}" {
                            *path = ".".into();
                        } else if let Some(relative) = path.strip_prefix("${workspace}/") {
                            *path = relative.to_owned();
                        }
                    }
                    for value in fields.values_mut() {
                        paths(value);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        paths(value);
                    }
                }
                _ => {}
            }
        }
        let mut references: Vec<_> = references
            .as_array()
            .expect("reference array")
            .iter()
            .map(|reference| {
                serde_json::json!({
                    "location": {
                        "path": reference["location"]["path"],
                        "start": reference["location"]["start"],
                        "end": reference["location"]["end"],
                    },
                    "identity": reference["identity"],
                    "certainty": reference["certainty"],
                    "role": reference["role"],
                    "target": reference["target"],
                })
            })
            .collect();
        for reference in &mut references {
            paths(reference);
        }
        references.sort_by_key(Value::to_string);
        references
    }
    assert_eq!(low["status"], all["result"]["status"]);
    assert_eq!(low["coverage"], all["result"]["coverage"]);
    let low = normalized(&low["references"]);
    let direct = normalized(&all["result"]["references"]);
    assert_eq!(low.len(), direct.len());
    for (index, (low, direct)) in low.iter().zip(&direct).enumerate() {
        assert_eq!(low, direct, "reference {index}");
    }
}

#[test]
fn standalone_and_project_selectors_distinguish_same_named_owners() {
    let f = Fixture::new();
    let mut order = fs::read_to_string(f.0.join("src/order.wi")).unwrap();
    order.push_str("pub class Other { pub qty: i64; pub init(self, qty: i64) { self.qty = qty; } pub fn value(self) -> i64 { return self.qty; } }\n");
    fs::write(f.0.join("src/order.wi"), order).unwrap();
    for source in [false, true] {
        let mut selected = Vec::new();
        for owner in ["Order", "Other"] {
            let selector = format!("order::{owner}::qty");
            let mut args = vec!["refs", &selector, "--module", "order", "--kind", "field"];
            if source {
                args.extend(["--source", "src/main.wi"]);
            }
            let value = f.json(&args, 0);
            assert_eq!(value["result"]["total"], 2, "{value}");
            selected.push(value["result"]["selected"]["id"].clone());
            let method = format!("order::{owner}::value");
            let mut args = vec!["symbol", &method, "--module", "order"];
            if source {
                args.extend(["--source", "src/main.wi"]);
            }
            f.json(&args, 0);
        }
        assert_ne!(selected[0], selected[1]);
    }
}

#[test]
fn position_selectors_respect_filters() {
    let f = Fixture::new();
    for command in ["symbol", "refs", "type"] {
        for (option, value) in [
            ("--kind", "function"),
            ("--module", "absent"),
            ("--package", "absent"),
        ] {
            let result = f.json(&[command, "src/order.wi:3:9", option, value], 1);
            assert_eq!(result["status"], "unknown", "{result}");
        }
    }
}

#[test]
fn human_output_exposes_symbol_location_ids_and_impact_names() {
    let f = Fixture::new();
    for command in ["symbol", "refs", "effects"] {
        let output = f.run(&[command, "order::submit", "--show-id"]);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("src/order.wi:7:"), "{text}");
        assert!(
            text.lines()
                .any(|line| line.starts_with("ID: ") && line != "ID: ?"),
            "{text}"
        );
    }
    let output = f.run(&["impact", "order::submit"]);
    assert!(output.status.success(), "{output:?}");
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("main::submit"), "{text}");
    assert!(text.contains("main::main"), "{text}");
    assert!(text.contains("truncated=false"), "{text}");
}

#[test]
fn references_distinguish_possible_virtual_dispatch() {
    let f = Fixture::new();
    fs::write(f.0.join("src/main.wi"), "open class Base { pub open fn hook(self) -> i64 { return 1; } }\nclass Child extends Base { pub override fn hook(self) -> i64 { return 2; } }\nfn via_base(x: Base) -> i64 { return x.hook(); }\nfn via_child(x: Child) -> i64 { return x.hook(); }\nfn main() {}\n").unwrap();
    let result = f.json(&["refs", "main::Child::hook"], 0);
    let references = result["result"]["references"].as_array().unwrap();
    assert!(
        references
            .iter()
            .any(|reference| reference["certainty"] == "possible-dispatch"
                && reference["location"]["line"] == 3),
        "{result}"
    );
    assert!(
        references
            .iter()
            .any(|reference| reference["certainty"] == "resolved"
                && reference["location"]["line"] == 4),
        "{result}"
    );
}
