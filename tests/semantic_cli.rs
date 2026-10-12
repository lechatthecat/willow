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
        Self(fs::canonicalize(root).unwrap())
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
fn direct_checker_warnings_use_check_project_paths() {
    let f = Fixture::new();
    for file in ["src/main.wi", "src/order.wi"] {
        let path = f.0.join(file);
        let source = fs::read_to_string(&path).unwrap();
        let (first, rest) = source.split_once('\n').unwrap();
        fs::write(
            path,
            format!("{first}\nimport std::collections::Array;\n{rest}"),
        )
        .unwrap();
    }
    let check = f.run(&["check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let check_stderr = String::from_utf8(check.stderr).unwrap();
    let expected = &check_stderr[check_stderr.find("warning[W2003]").unwrap()..];
    assert!(expected.contains("warning[W2003]"), "{expected}");
    for file in ["main.wi", "order.wi"] {
        assert!(expected.contains(file), "{expected}");
    }
    assert!(!expected.contains(f.0.to_str().unwrap()), "{expected}");
    for args in [
        vec!["symbol", "order::submit"],
        vec!["refs", "order::submit"],
        vec!["references", "order::submit"],
        vec!["type", "order::submit"],
        vec!["effects", "order::submit"],
        vec!["impact", "order::submit"],
        vec!["rename", "main::main::item", "product", "--dry-run"],
    ] {
        for format in ["human", "json", "ndjson"] {
            let mut args = args.clone();
            args.extend(["--format", format]);
            let output = f.run(&args);
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {stderr}\n{}",
                String::from_utf8_lossy(&output.stdout)
            );
            assert_eq!(stderr, expected, "{args:?}");
        }
    }
}

#[test]
fn impact_prints_definition_location_once() {
    let f = Fixture::new();
    let value = f.json(&["impact", "order::submit"], 0);
    let location = &value["result"]["symbol"]["location"];
    let expected = format!(
        "{}:{}:{}",
        location["path"].as_str().unwrap(),
        location["line"],
        location["column"]
    );
    let output = f.run(&["impact", "order::submit"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert_eq!(
        text.lines().filter(|line| *line == expected).count(),
        1,
        "{text}"
    );
    assert!(text.contains("Symbol: order::submit (function)"), "{text}");
    assert!(text.contains("Affected functions:"), "{text}");
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
                        // Low-level storage uses the host separator; direct CLI
                        // locations use '/'. Compare the same logical path.
                        *path = path.replace('\\', "/");
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
    for path in [
        "${workspace}/src/main.wi",
        r"${workspace}\src\main.wi",
        "src/main.wi",
    ] {
        let reference = serde_json::json!([{"location":{"path":path,"start":0,"end":1}}]);
        assert_eq!(normalized(&reference)[0]["location"]["path"], "src/main.wi");
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
    assert!(
        result["result"]["coverage"]
            .as_str()
            .unwrap()
            .contains("possible virtual dispatch")
    );
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

#[test]
fn type_positions_explain_absent_types_and_keep_structured_status() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        "fn main() {\n    let value = 1;\n    println(value);\n}\n",
    )
    .unwrap();
    for selector in ["src/main.wi:1:1", "src/main.wi:2:1", "src/main.wi:2:5"] {
        let value = f.json(&["type", selector], 1);
        assert_eq!(value["status"], "unknown", "{value}");
        let out = f.run(&["type", selector]);
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(
            text.contains("No typed expression or declaration"),
            "{text}"
        );
        assert!(text.contains("Place the cursor"), "{text}");
    }
    for selector in ["src/main.wi:2:17", "src/main.wi:3:13"] {
        let value = f.json(&["type", selector], 0);
        assert_eq!(value["result"]["type_display"], "i64");
    }
    fs::write(f.0.join("src/main.wi"), "fn main() { let x = ; }").unwrap();
    let value = f.json(&["type", "src/main.wi:1:1"], 1);
    assert_eq!(value["status"], "error");
    assert!(value["message"].is_string());
}

#[test]
fn binding_declarations_report_declared_types_and_gaps_have_no_expression() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        "fn apply(f: closure(i64) -> f64, x: i64) -> f64 { return f(x); }\n\
         fn pick(o: Option<f64>) -> i64 {\n\
         \x20   return match o {\n\
         \x20       Some(h) => 1,\n\
         \x20       None => 0\n\
         \x20   };\n\
         }\n\
         fn main() {\n\
         \x20   let total = apply(|i: i64| -> f64 { return 1.5; }, 3);\n\
         \x20   let g: closure(i64) -> i64 = |n| n * 2; let twice = [g(1), 2];\n\
         \x20   for item in twice { println( item /* note */ ); }\n\
         \x20   println(pick(Some(total)) + ( 1 ));\n\
         }\n",
    )
    .unwrap();
    for (selector, expected) in [
        ("src/main.wi:9:24", "i64"),                 // annotated lambda parameter
        ("src/main.wi:10:35", "i64"),                // inferred lambda parameter
        ("src/main.wi:4:14", "f64"),                 // pattern binding in an i64 match
        ("src/main.wi:9:9", "f64"),                  // let binding
        ("src/main.wi:11:9", "i64"),                 // for binding
        ("src/main.wi:1:10", "closure(i64) -> f64"), // function parameter
        ("src/main.wi:2:9", "Option<f64>"),          // function parameter
        ("src/main.wi:1:60", "i64"),                 // parameter use
        ("src/main.wi:3:18", "Option<f64>"),         // scrutinee use
        ("src/main.wi:12:13", "fn(Option<f64>) -> i64"), // callee token
        ("src/main.wi:12:35", "i64"),                // literal inside parentheses
        ("src/main.wi:11:34", "i64"),                // for binding use
    ] {
        let value = f.json(&["type", selector], 0);
        assert_eq!(
            value["result"]["type_display"], expected,
            "{selector}: {value}"
        );
    }
    // The match expression itself still reports its own value type.
    let value = f.json(&["type", "src/main.wi:3:12"], 0);
    assert_eq!(value["result"]["type_display"], "i64", "{value}");
    // Whitespace and comments inside an enclosing expression are not typed.
    for (selector, found) in [
        ("src/main.wi:12:34", "whitespace"),
        ("src/main.wi:12:36", "whitespace"),
        ("src/main.wi:11:33", "whitespace"),
        ("src/main.wi:11:38", "whitespace"),
        ("src/main.wi:11:43", "a comment"),
        ("src/main.wi:9:31", "whitespace"),
    ] {
        let value = f.json(&["type", selector], 1);
        assert_eq!(value["status"], "unknown", "{selector}: {value}");
        assert!(value["result"].get("type_display").is_none(), "{value}");
        let text = String::from_utf8(f.run(&["type", selector]).stdout).unwrap();
        assert!(
            text.contains(&format!("found {found}")),
            "{selector}: {text}"
        );
        assert!(!text.contains("Type:"), "{selector}: {text}");
    }
    // `symbol` prints exactly one Type line, the declared type.
    for (selector, expected) in [
        ("src/main.wi:9:24", "Type: i64"),
        ("src/main.wi:4:14", "Type: f64"),
        ("main::pick", "Type: fn(Option<f64>) -> i64"),
    ] {
        let text = String::from_utf8(f.run(&["symbol", selector]).stdout).unwrap();
        let types: Vec<_> = text.lines().filter(|l| l.starts_with("Type:")).collect();
        assert_eq!(types, [expected], "{selector}: {text}");
    }
}

#[test]
fn effects_human_decodes_masks_and_retains_source_evidence() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        "fn empty() {}\nfn main() { println(1); }\n",
    )
    .unwrap();
    let empty = f.run(&["effects", "main::empty"]);
    assert!(
        String::from_utf8(empty.stdout)
            .unwrap()
            .contains("Runtime effects: none")
    );
    for extra in [vec![], vec!["--all"], vec!["--explain"]] {
        let mut args = vec!["effects", "main::main"];
        args.extend(extra);
        let out = f.run(&args);
        let text = String::from_utf8(out.stdout).unwrap();
        assert!(text.contains("may-allocate"), "{text}");
        assert!(text.contains("may-panic"), "{text}");
        assert!(text.contains("src/main.wi:2:"), "{text}");
        assert!(text.contains("Runtime evidence for main::main"), "{text}");
        assert!(text.contains("conservative"), "{text}");
    }
    let out = f.run(&["effects", "main::main", "--format=json"]);
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["result"]["runtime_effects"], 63);
    assert!(value["result"]["effect_evidence"].is_array());
    assert!(value["result"].get("compiler_witnesses").is_none());
}

#[test]
fn imported_interface_effects_use_completed_implementation_summaries() {
    use willow_compiler::semantic::effects::RuntimeEffects;
    for (body, waiting) in [
        ("", RuntimeEffects::NONE),
        (
            "let values = [3, 1, 2]; for value in values { let copy = value; }",
            RuntimeEffects::NONE,
        ),
        ("helper();", RuntimeEffects::NONE),
        ("self.channel.recv();", RuntimeEffects::MAY_SUSPEND),
        ("self.cell.get();", RuntimeEffects::MAY_BLOCK),
    ] {
        for (import, ty) in [
            ("import pairing;", "pairing::Pairing"),
            ("import pairing as model;", "model::Pairing"),
            (
                "import pairing::Lottery; import pairing::Pairing;",
                "Pairing",
            ),
            ("import pairing::Pairing as Contract;", "Contract"),
        ] {
            let f = Fixture::new();
            fs::write(f.0.join("src/pairing.wi"), format!("pub interface Pairing {{ fn pairings(self); }} pub class Lottery implements Pairing {{ pub channel: Channel<i64>; pub cell: BlockingCell<i64>; pub fn pairings(self) {{ {body} }} }} fn helper() {{ let n = 1; }}")).unwrap();
            fs::write(
                f.0.join("src/main.wi"),
                format!("{import} fn round(p: {ty}) {{ p.pairings(); }} fn main() {{}}"),
            )
            .unwrap();
            let result = f.json(&["effects", "main::round"], 0);
            let effects = result["result"]["runtime_effects"].as_u64().unwrap();
            let waits = RuntimeEffects::MAY_BLOCK
                .union(RuntimeEffects::MAY_SUSPEND)
                .bits() as u64;
            assert_eq!(
                effects & waits,
                u64::from(waiting.bits()),
                "body={body} import={import}: {result}"
            );
        }
    }
}

#[test]
fn imported_interface_lock_checks_allow_pure_and_reject_waiting_bodies() {
    for waiting in [false, true] {
        let f = Fixture::new();
        let body = if waiting {
            "self.channel.recv();"
        } else {
            "let value = 1;"
        };
        fs::write(f.0.join("src/pairing.wi"), format!("pub interface Pairing extends Send {{ fn pairings(self); }} pub class Lottery implements Pairing {{ pub channel: Channel<i64>; pub fn pairings(self) {{ {body} }} }}")).unwrap();
        fs::write(f.0.join("src/main.wi"), "import pairing; fn invoke(p: pairing::Pairing) { p.pairings(); } async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); let p = new pairing::Lottery(ch); lock m as n { invoke(p); } }").unwrap();
        let output = f.run(&["check", "."]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), !waiting, "{stderr}");
        assert_eq!(stderr.contains("E2604"), waiting, "{stderr}");
    }
}

#[test]
fn imported_interface_defaults_and_overrides_keep_the_selected_wait_effect() {
    for (default, override_body, waiting) in [
        ("let value = 1;", None, false),
        ("ch.recv();", None, true),
        ("ch.recv();", Some("let value = 1;"), false),
        ("let value = 1;", Some("ch.recv();"), true),
    ] {
        let f = Fixture::new();
        let method = override_body.map_or_else(String::new, |body| {
            format!("pub fn pairings(self, ch: Channel<i64>) {{ {body} }}")
        });
        fs::write(f.0.join("src/pairing.wi"), format!("pub interface Pairing extends Send {{ fn pairings(self, ch: Channel<i64>) {{ {default} }} }} pub class Lottery implements Pairing {{ {method} }}")).unwrap();
        fs::write(f.0.join("src/main.wi"), "import pairing; fn invoke(p: pairing::Pairing, ch: Channel<i64>) { p.pairings(ch); } async fn main() { let m = Mutex::new(0); let ch: Channel<i64> = Channel::new(); let p = new pairing::Lottery(); lock m as n { invoke(p, ch); } }").unwrap();
        let output = f.run(&["check", "."]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.success(), !waiting, "{stderr}");
        assert_eq!(stderr.contains("E2604"), waiting, "{stderr}");
    }
}

#[test]
fn condition_diagnostics_preserve_errors_and_do_not_cascade_through_cached_bodies() {
    for (expression, mismatch) in [
        ("\"abc\".missing(\"a\")", false),
        ("empty()", true),
        ("(match true { _ => 1, true => 2 })", true),
    ] {
        for statement in [
            format!("if {expression} {{}}"),
            format!("while {expression} {{}}"),
            format!("let n = {expression} ? 1 : 2;"),
            format!("let n: i64 = {expression} ? 1 : 2;"),
        ] {
            let f = Fixture::new();
            fs::write(
                f.0.join("src/main.wi"),
                format!("fn empty() {{}} fn main() {{ {statement} }}"),
            )
            .unwrap();
            let output = f.run(&["check", "."]);
            let text = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{statement}");
            assert_eq!(
                text.contains("E0203") || text.contains("E0901"),
                mismatch,
                "{statement}: {text}"
            );
            if !mismatch {
                assert!(text.contains("E0201"), "{text}");
            }
        }
    }
}

#[test]
fn contract_impact_and_effects_explain_bodyless_declarations() {
    for (label, interface, implementation, expected) in [
        (
            "contract",
            "pub interface I { fn pick(self) -> i64; }",
            "pub class A implements api::I { pub fn pick(self) -> i64 { return 1; } }",
            "implementation::A::pick",
        ),
        (
            "default",
            "pub interface I { fn pick(self) -> i64 { return 2; } }",
            "pub class A implements api::I {}",
            "api::I::pick",
        ),
    ] {
        let f = Fixture::new();
        fs::write(f.0.join("src/api.wi"), interface).unwrap();
        fs::write(
            f.0.join("src/implementation.wi"),
            format!("import api; {implementation}"),
        )
        .unwrap();
        fs::write(f.0.join("src/main.wi"), "import api; import implementation; fn call(x: api::I) -> i64 { return x.pick(); } fn main() { println(call(new implementation::A())); }").unwrap();
        f.json(&["symbol", "api::I::pick"], 0);
        f.json(&["refs", "api::I::pick"], 0);
        let result = f.json(&["impact", "api::I::pick"], 0);
        let names: Vec<_> = result["result"]["impact"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["selector"].as_str())
            .collect();
        assert!(names.contains(&expected), "{label}: {result}");
        assert!(names.contains(&"main::call"), "{label}: {result}");
        if label == "contract" {
            let result = f.json(&["effects", "api::I::pick"], 1);
            assert!(
                result["result"]["reason"]
                    .as_str()
                    .unwrap()
                    .contains("no executable body")
            );
            let output = f.run(&["effects", "api::I::pick"]);
            assert!(String::from_utf8_lossy(&output.stdout).contains("implementing method"));
        }
    }
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        "interface I { fn pick(self) -> i64; } fn main() {}",
    )
    .unwrap();
    let result = f.json(&["impact", "main::I::pick"], 1);
    assert!(
        result["result"]["reason"]
            .as_str()
            .unwrap()
            .contains("no analyzed implementation")
    );
    let result = f.json(&["impact", "main::I"], 1);
    assert!(
        result["result"]["reason"]
            .as_str()
            .unwrap()
            .contains("function or method")
    );
}

#[test]
fn project_check_checks_unimported_sources_without_importing_names() {
    for (label, path, source, success) in [
        (
            "return mismatch",
            "junk.wi",
            "fn value() -> i64 { return \"bad\"; }",
            false,
        ),
        ("reserved keyword", "junk.wi", "fn open() {}", false),
        (
            "nested module",
            "nested/junk.wi",
            "fn value() -> i64 { return \"bad\"; }",
            false,
        ),
        (
            "directory module",
            "nested/mod.wi",
            "fn value() -> i64 { return \"bad\"; }",
            false,
        ),
        (
            "valid unused",
            "junk.wi",
            "fn value() -> i64 { return 1; }",
            true,
        ),
        (
            "private name collision",
            "junk.wi",
            "fn submit() {} fn main() {}",
            true,
        ),
        (
            "transitive dependency",
            "junk.wi",
            "import order; fn value() -> i64 { return order::submit(1); }",
            true,
        ),
        ("missing import", "junk.wi", "import missing;", false),
    ] {
        let f = Fixture::new();
        let file = f.0.join("src").join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, source).unwrap();
        let output = f.run(&["check", "."]);
        assert_eq!(
            output.status.success(),
            success,
            "{label}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if !success {
            let diagnostics = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                diagnostics.contains("junk.wi") || diagnostics.contains("mod.wi"),
                "{label}: {diagnostics}"
            );
        }
        // Explicit source checks keep their reachable-source semantics.
        assert!(f.run(&["check", "src/main.wi"]).status.success(), "{label}");
    }
}

#[test]
fn local_selectors_accept_unique_names_and_disambiguate_shadowing() {
    let f = Fixture::new();
    fs::write(f.0.join("src/main.wi"), "fn serve(arg: i64) { let mut running = arg; if true { let slot = 1; println(slot); } if false { let slot = 2; println(slot); } println(running); }\nfn customer() { let r = 1; println(r); }\nfn main() { serve(1); }\n").unwrap();
    for command in ["symbol", "refs", "type"] {
        for name in ["main::serve::running", "main::serve::arg"] {
            assert_eq!(f.json(&[command, name], 0)["status"], "ok");
        }
    }
    let rename = f.json(
        &["rename", "main::serve::running", "active", "--dry-run"],
        0,
    );
    assert_eq!(rename["status"], "ok");
    let ambiguous = f.json(&["symbol", "main::serve::slot"], 1);
    assert_eq!(ambiguous["status"], "ambiguous", "{ambiguous}");
    let candidates = ambiguous["result"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    for candidate in candidates {
        f.json(&["symbol", candidate["selector"].as_str().unwrap()], 0);
    }
    let unknown = f.json(&["symbol", "main::serve::runn"], 1);
    let suggestions = unknown["result"]["suggestions"].as_array().unwrap();
    assert_eq!(suggestions.len(), 1, "{unknown}");
    assert!(suggestions[0].as_str().unwrap().ends_with("serve::running"));
}

#[test]
fn missing_position_symbols_explain_token_and_nearest_identifier() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        "fn main() {\n    let value: i64 = 1;\n    println(value);\n}\n",
    )
    .unwrap();
    for command in ["symbol", "refs", "type", "effects", "impact", "rename"] {
        for (position, found) in [
            ("src/main.wi:2:14", "token `:`"),
            ("src/main.wi:2:1", "whitespace"),
            ("src/main.wi:2:24", "end of line"),
        ] {
            let mut args = vec![command, position];
            if command == "rename" {
                args.extend(["other", "--dry-run"]);
            }
            let value = f.json(&args, 1);
            assert_eq!(value["status"], "unknown", "{value}");
            let reason = value["result"]["reason"].as_str().unwrap();
            assert!(reason.contains(found), "{value}");
            assert!(
                reason.contains(if found == "end of line" {
                    "`i64` at 2:16"
                } else {
                    "`value` at 2:9"
                }),
                "{value}"
            );
            let out = f.run(&args);
            assert!(String::from_utf8(out.stdout).unwrap().contains(found));
        }
    }
    for (position, reason) in [
        ("src/missing.wi:1:1", "source file not found"),
        ("src/main.wi:99:1", "line is out of range"),
        ("src/main.wi:2:99", "column is out of range"),
    ] {
        let value = f.json(&["symbol", position], 1);
        assert!(value.to_string().contains(reason), "{value}");
    }
}

#[test]
fn async_capture_diagnostic_explains_nested_field_without_package_hash() {
    let f = Fixture::new();
    fs::write(f.0.join("src/order.wi"), "module order;\nimport std::collections::Array;\npub class Inner { pub inboxes: Array<Channel<i64>>; }\npub class Bank { pub inner: Inner; }\npub async fn consume(bank: Bank) {}\n").unwrap();
    fs::write(
        f.0.join("src/main.wi"),
        "import order;\nfn call(bank: order::Bank) { order::consume(bank); }\nfn main() {}\n",
    )
    .unwrap();
    let out = f.run(&["check"]);
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("E2402"), "{text}");
    assert!(!text.contains("$pkg"), "{text}");
    for part in [
        "order::Bank",
        "inner:",
        "inboxes: Array<Channel<i64>>",
        "FrozenArray via .freeze()",
        "Mutex<Array<T>> when the elements are Send",
    ] {
        assert!(text.contains(part), "missing {part}: {text}");
    }
}

#[test]
fn locks_have_suspend_compiler_evidence_after_await_and_in_callers() {
    let f = Fixture::new();
    for (mode, ty) in [
        ("lock", "Mutex"),
        ("lock read", "RwLock"),
        ("lock write", "RwLock"),
    ] {
        fs::write(f.0.join("src/main.wi"), format!("async fn fees(m: {ty}<i64>) -> i64 {{\n    let mut out = 0;\n    {mode} m as value {{ out = value; }}\n    return out;\n}}\nasync fn transfer(m: {ty}<i64>) {{\n    await sleep(0);\n    {mode} m as value {{ }}\n}}\nfn relay(m: {ty}<i64>) {{ fees(m); }}\nfn main() {{}}\n")).unwrap();
        for (name, line) in [("fees", 3), ("transfer", 8), ("relay", 3)] {
            let value = f.json(&["effects", &format!("main::{name}"), "--explain"], 0);
            let evidence = value["result"]["effect_evidence"].as_array().unwrap();
            assert!(
                evidence.iter().any(|e| e["status"] == "compiler-fact"
                    && e["witness"]["cause"]["operation"] == "lock"
                    && e["witness"]["cause"]["location"]["line"] == line),
                "{mode} {name}: {value}"
            );
        }
    }
}

#[test]
fn mismatch_diagnostics_keep_dependency_type_names_distinct() {
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("ledger/src")).unwrap();
    fs::write(
        f.0.join("ledger/project.toml"),
        "[willow]\nmanifest-version=1\n[project]\nname='ledger'\nversion='1.0.0'\n",
    )
    .unwrap();
    fs::write(
        f.0.join("ledger/src/order.wi"),
        "module order; pub class Bank {} pub fn make() -> Bank { return new Bank(); }\n",
    )
    .unwrap();
    let manifest = fs::read_to_string(f.0.join("project.toml")).unwrap();
    fs::write(
        f.0.join("project.toml"),
        format!("{manifest}\n[dependencies]\nexternal={{path='ledger'}}\n"),
    )
    .unwrap();
    fs::write(
        f.0.join("src/order.wi"),
        "module order; pub class Bank {} pub fn make() -> Bank { return new Bank(); }\n",
    )
    .unwrap();
    fs::write(f.0.join("src/main.wi"), "import order; import external::order as other;\nfn takes(bank: order::Bank) {}\nfn wrong() { takes(other::make()); }\nfn main() {}\n").unwrap();
    let out = f.run(&["check"]);
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!text.contains("$pkg"), "{text}");
    assert!(text.contains("order::Bank"), "{text}");
    assert!(text.contains("ledger::order::Bank"), "{text}");
}

#[test]
fn await_block_bounds_explain_lowering_without_blaming_locks() {
    use willow_compiler::semantic::effects::RuntimeEffects;
    for (mode, ty) in [
        ("lock", "Mutex"),
        ("lock read", "RwLock"),
        ("lock write", "RwLock"),
    ] {
        for (setup, wait) in [
            ("", "await sleep(0);"),
            ("", "await sleep(1);"),
            ("", "await yield();"),
            ("let timer = sleep(0);", "await timer;"),
        ] {
            let f = Fixture::new();
            fs::write(f.0.join("src/main.wi"), format!("async fn fees(m: {ty}<i64>) {{ {mode} m as value {{ }} }}\nasync fn transfer(m: {ty}<i64>) {{\n{setup}\n{wait}\n{mode} m as value {{ }}\n}}\nfn relay(m: {ty}<i64>) {{ transfer(m); }}\nfn main() {{}}\n")).unwrap();
            for name in ["transfer", "relay"] {
                let selector = format!("main::{name}");
                let result = f.json(&["effects", &selector, "--explain"], 0);
                let evidence = result["result"]["effect_evidence"].as_array().unwrap();
                if setup.is_empty() {
                    assert!(
                        evidence
                            .iter()
                            .all(|e| e["effect"] != RuntimeEffects::MAY_BLOCK.bits()),
                        "{result}"
                    );
                    assert_eq!(
                        result["result"]["runtime_effects"].as_u64().unwrap()
                            & u64::from(
                                RuntimeEffects::MAY_BLOCK.bits()
                                    | RuntimeEffects::NO_PREEMPT_REGION.bits()
                            ),
                        0,
                        "{result}"
                    );
                    continue;
                }
                let block = evidence
                    .iter()
                    .find(|e| e["effect"] == RuntimeEffects::MAY_BLOCK.bits())
                    .unwrap();
                assert_eq!(block["status"], "conservative-bound", "{result}");
                assert_eq!(block["witness"]["cause"]["operation"], "await", "{result}");
                assert_eq!(block["witness"]["cause"]["location"]["line"], 4, "{result}");
                let reason = block["witness"]["reason"].as_str().unwrap();
                assert!(
                    reason.contains("willow_future_await_void")
                        && reason.contains("does not identify the selected lowering"),
                    "{result}"
                );
                let output = f.run(&["effects", &selector]);
                assert!(output.status.success());
                let text = String::from_utf8(output.stdout).unwrap();
                assert!(
                    text.contains("operation=await") && text.contains(reason),
                    "{text}"
                );
                assert!(!text.contains("typed-expression-lowering"), "{text}");
            }
            let fees = f.json(&["effects", "main::fees"], 0);
            assert_eq!(
                fees["result"]["runtime_effects"].as_u64().unwrap()
                    & u64::from(RuntimeEffects::MAY_BLOCK.bits()),
                0,
                "{fees}"
            );
        }
    }
}

#[test]
fn buildgraph_typo_suggestions_stay_in_scope() {
    let f = Fixture::new();
    fs::write(f.0.join("src/main.wi"), "class Bank { pub fn transfer(self) {} } fn serve() { let running = true; let slot = 1; println(slot); } fn unrelated() { let r = 1; } fn main() {} ").unwrap();
    for (typo, wanted) in [
        ("main::Bank::tranfer", "transfer"),
        ("main::serve::runing", "running"),
        ("main::serve::slto", "slot"),
    ] {
        for command in ["refs", "symbol", "type"] {
            let value = f.json(&[command, typo], 1);
            let suggestions = value["result"]["suggestions"].as_array().unwrap();
            assert!(
                suggestions
                    .iter()
                    .any(|s| s.as_str().unwrap().contains(wanted)),
                "{value}"
            );
            assert!(
                suggestions
                    .iter()
                    .all(|s| !s.as_str().unwrap().contains("unrelated")),
                "{value}"
            );
        }
    }
}

#[test]
fn buildgraph_element_mutation_references() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        "import std::collections::Array;
class Data { pub values: Array<i64>; }
fn f() {
let xs = [1, 2];
let i = 0;
xs[i] = xs[i] + 1;
let d = new Data(xs);
d.values[i] = d.values[i] + 1;
}
fn main() {}
",
    )
    .unwrap();
    for (selector, line) in [("main::f::xs", 6), ("main::Data::values", 8)] {
        let value = f.json(&["refs", selector], 0);
        let refs = value["result"]["references"].as_array().unwrap();
        assert!(
            refs.iter()
                .any(|r| r["location"]["line"] == line && r["role"] == "write-element"),
            "{value}"
        );
        assert!(
            refs.iter()
                .any(|r| r["location"]["line"] == line && r["role"] != "write-element"),
            "{value}"
        );
    }
    let indices = f.json(&["refs", "main::f::i"], 0);
    assert!(
        indices["result"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["role"] == "read"),
        "{indices}"
    );
}

#[test]
fn buildgraph_rename_counts_all_edits() {
    for (source, selector, declarations, references) in [
        (
            "class A { pub fn unused(self) {} } fn main() {}",
            "main::A::unused",
            1,
            0,
        ),
        (
            "interface P { fn choose(self) -> i64; } class A implements P { pub fn choose(self) -> i64 { return 1; } } class B implements P { pub fn choose(self) -> i64 { return 2; } } class C implements P { pub fn choose(self) -> i64 { return 3; } } fn f(p: P) { println(p.choose()); } fn main() {}",
            "main::P::choose",
            4,
            1,
        ),
    ] {
        let f = Fixture::new();
        fs::write(f.0.join("src/main.wi"), source).unwrap();
        let value = f.json(&["rename", selector, "pick"], 0);
        assert_eq!(
            value["result"]["edits"],
            declarations + references,
            "{value}"
        );
        assert_eq!(
            value["result"]["declarations_updated"], declarations,
            "{value}"
        );
        assert_eq!(value["result"]["references_updated"], references, "{value}");
        let after = fs::read_to_string(f.0.join("src/main.wi")).unwrap();
        assert_eq!(
            after.matches("pick").count(),
            (declarations + references) as usize
        );
    }
    let f = Fixture::new();
    let output = f.run(&["rename", "order::Order::value", "get"]);
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("2 edits (1 declaration, 1 reference)")
    );
}

#[test]
fn buildgraph_loop_evidence_has_origin_and_explanation() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        "fn work(n: i64) {
let mut i = 0;
while i < n { i = i + 1; }
}
fn relay() { work(2); }
fn main() {}
",
    )
    .unwrap();
    for name in ["main::work", "main::relay"] {
        let value = f.json(&["effects", name, "--explain"], 0);
        let output = f.run(&["effects", name]);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains("operation=synchronous-loop") && text.contains("src/main.wi:3:1"),
            "{value}\n{text}"
        );
        assert!(text.contains("runtime/platform gating"), "{text}");
        assert!(!text.contains("typed-expression-lowering"), "{text}");
    }
}

#[test]
fn diagnostic_selector_regressions() {
    let f = Fixture::new();
    fs::write(f.0.join("src/main.wi"), "enum Trap { Underflow, Overflow } fn apply() { let modified = true; println(modified); } fn main() {} ").unwrap();
    for (selector, wanted) in [
        ("Trap::Underflw", "Underflow"),
        ("main::apply::changed", "modified"),
    ] {
        for command in ["refs", "symbol", "type", "effects", "impact"] {
            let value = f.json(&[command, selector], 1);
            assert!(
                value["result"]["suggestions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s.as_str().unwrap().contains(wanted)),
                "{value}"
            );
        }
    }
}

#[test]
fn diagnostic_missing_import_hints_and_recovery() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/vm.wi"),
        "module vm; pub enum Trap { Underflow } pub fn trap() -> Trap { return Trap::Underflow; }",
    )
    .unwrap();
    fs::write(
        f.0.join("src/main.wi"),
        "import vm; fn run() -> Result<i64, Trap> { return Err(vm::trap()); } fn main() {} ",
    )
    .unwrap();
    let output = f.run(&["check", "."]);
    assert!(!output.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("import vm::Trap;"), "{text}");
    assert!(!text.contains("expected `Result"), "{text}");
}

#[test]
fn diagnostic_import_hints_cover_public_type_kinds() {
    let f = Fixture::new();
    fs::write(f.0.join("src/vm.wi"), "module vm; pub class Machine {} pub interface Device { fn run(self); } enum Secret { Hidden }").unwrap();
    fs::write(
        f.0.join("src/main.wi"),
        "import vm; fn run(a: Machine, b: Device, c: Secret) {} fn main() {} ",
    )
    .unwrap();
    let output = f.run(&["check", "."]);
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(text.contains("import vm::Machine;"), "{text}");
    assert!(text.contains("import vm::Device;"), "{text}");
    assert!(!text.contains("import vm::Secret;"), "{text}");
}

#[test]
fn buildgraph_storage_reference_roles() {
    for (statement, selector, expected) in [
        ("println(d.n);", "main::Data::n", vec!["read"]),
        ("d.n = 2;", "main::Data::n", vec!["write"]),
        ("d.n += 2;", "main::Data::n", vec!["read", "write"]),
        ("d.n -= 2;", "main::Data::n", vec!["read", "write"]),
        ("d.n *= 2;", "main::Data::n", vec!["read", "write"]),
        ("d.n /= 2;", "main::Data::n", vec!["read", "write"]),
        ("d.n %= 2;", "main::Data::n", vec!["read", "write"]),
        ("xs[i] = 2;", "main::f::xs", vec!["write-element"]),
        ("xs[i] += 2;", "main::f::xs", vec!["read", "write-element"]),
        ("xs[i] -= 2;", "main::f::xs", vec!["read", "write-element"]),
        ("xs[i] *= 2;", "main::f::xs", vec!["read", "write-element"]),
        ("xs[i] /= 2;", "main::f::xs", vec!["read", "write-element"]),
        ("xs[i] %= 2;", "main::f::xs", vec!["read", "write-element"]),
        (
            "d.values[i] = 2;",
            "main::Data::values",
            vec!["write-element"],
        ),
        (
            "d.values[i] += 2;",
            "main::Data::values",
            vec!["read", "write-element"],
        ),
        (
            "d.values[i] -= 2;",
            "main::Data::values",
            vec!["read", "write-element"],
        ),
        ("d.values[i] += 2;", "main::f::d", vec!["read"]),
        ("d.values[i] += 2;", "main::f::i", vec!["read"]),
        (
            "xs[i] += xs[i];",
            "main::f::xs",
            vec!["read", "read", "write-element"],
        ),
        (
            "grid[i][i] += 2;",
            "main::f::grid",
            vec!["read", "write-element"],
        ),
        ("grid[i][i] += 2;", "main::f::i", vec!["read", "read"]),
        ("d.n = d.n + 1;", "main::Data::n", vec!["read", "write"]),
    ] {
        let f = Fixture::new();
        fs::write(f.0.join("src/main.wi"), format!(
            "import std::collections::Array;\nclass Data {{ pub n: i64; pub values: Array<i64>; }}\nfn f() {{\nlet xs = [8]; let grid = [[8]]; let i = 0; let d = new Data(8, xs);\n{statement}\n}}\nfn main() {{ f(); }}\n"
        )).unwrap();
        let value = f.json(&["refs", selector], 0);
        let mut roles: Vec<_> = value["result"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["location"]["line"] == 5)
            .map(|r| r["role"].as_str().unwrap())
            .collect();
        roles.sort_unstable();
        assert_eq!(roles, expected, "{statement}: {value}");
    }
}

#[test]
fn buildgraph_self_and_static_field_roles() {
    let f = Fixture::new();
    fs::write(f.0.join("src/main.wi"), "class Counter {\n pub n: i64;\n pub static mut total: i64 = 0;\n pub init(self) { self.n = 1; }\n pub fn bump(self) {\n self.n += 2;\n Counter::total = self.n;\n println(Counter::total);\n }\n}\nfn main() { let c = new Counter(); c.bump(); }\n").unwrap();
    for (selector, line, expected) in [
        ("main::Counter::n", 4, vec!["write"]),
        ("main::Counter::n", 6, vec!["read", "write"]),
        ("main::Counter::n", 7, vec!["read"]),
        ("main::Counter::total", 7, vec!["write"]),
        ("main::Counter::total", 8, vec!["read"]),
    ] {
        let value = f.json(&["refs", selector], 0);
        let mut roles: Vec<_> = value["result"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["location"]["line"] == line)
            .map(|r| r["role"].as_str().unwrap())
            .collect();
        roles.sort_unstable();
        assert_eq!(roles, expected, "{value}");
    }
}

#[test]
fn buildgraph_compound_roles_do_not_duplicate_rename_edits() {
    let f = Fixture::new();
    fs::write(f.0.join("src/main.wi"), "class Counter { pub n: i64; pub init(self) { self.n = 0; } pub fn bump(self) { self.n += 1; } } fn main() { let c = new Counter(); c.bump(); println(c.n); let xs = [1]; xs[0] += 2; println(xs[0]); }").unwrap();
    for (selector, destination, edits) in [
        ("main::Counter::n", "count", 4),
        ("main::main::xs", "values", 3),
    ] {
        let value = f.json(&["rename", selector, destination], 0);
        assert_eq!(value["result"]["edits"], edits, "{value}");
    }
}

#[test]
fn assignment_target_types() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        "class Counter {
 pub n: i64;
 pub init(self) { self.n = 8; }
 pub fn bump(self) {
 self.n += 2;
 self.n = 3;
 }
}
fn main() {
 let mut n = 8; let xs = [8]; let i = 0; let c = new Counter();
 n += 2;
 n = 3;
 xs[i] += 2;
 xs[i] = 3;
 c.n += 2;
 c.n = 3;
}
",
    )
    .unwrap();
    for (line, column) in [
        (11, 2),
        (12, 2),
        (5, 7),
        (6, 7),
        (13, 4),
        (14, 4),
        (15, 4),
        (16, 4),
    ] {
        let selector = format!("src/main.wi:{line}:{column}");
        let value = f.json(&["type", &selector], 0);
        assert_eq!(
            value["result"]["type_display"], "i64",
            "{selector}: {value}"
        );
    }
}

#[test]
fn unknown_effects_and_impact_explain_conservative_capabilities() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        include_str!("../example/effect_explanations.wi"),
    )
    .unwrap();
    for name in ["apply", "relay", "worker"] {
        let selector = format!("main::{name}");
        let value = f.json(&["effects", &selector, "--explain"], 1);
        let result = &value["result"];
        assert_eq!(result["status"], "unknown", "{value}");
        assert!(
            result["reason"].as_str().unwrap().contains("conservative"),
            "{value}"
        );
        let missing: Vec<_> = result["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["status"] == "missing-witness")
            .collect();
        assert!(!missing.is_empty(), "{value}");
        for fact in missing {
            let reason = fact["reason"].as_str().unwrap();
            assert!(
                reason.contains("unresolved") && reason.contains("synchronous"),
                "{value}"
            );
        }
        assert!(
            result["witness"]
                .as_array()
                .unwrap()
                .iter()
                .any(
                    |w| w["unresolved"].as_array().is_some_and(|u| !u.is_empty())
                        && w["reason"].as_str().is_some()
                ),
            "{value}"
        );
        let output = f.run(&["effects", &selector]);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains(result["reason"].as_str().unwrap()), "{text}");
        assert!(
            text.contains("This does not prove that this function suspends or blocks"),
            "{text}"
        );
    }
    for name in ["apply", "increment"] {
        let selector = format!("main::{name}");
        let value = f.json(&["impact", &selector], 0);
        assert_eq!(value["result"]["impact"]["unknown"], true, "{value}");
        let reason = value["result"]["impact"]["reason"].as_str().unwrap();
        assert!(reason.contains("points-to proof"), "{value}");
        let output = f.run(&["impact", &selector]);
        assert!(String::from_utf8(output.stdout).unwrap().contains(reason));
        for node in value["result"]["impact"]["nodes"].as_array().unwrap() {
            if node["unknown"] == true {
                assert!(node["reason"].is_string(), "{node}");
            }
        }
    }
    for command in ["effects", "impact"] {
        let value = f.json(&[command, "main::absent"], 1);
        assert!(value["result"]["reason"].is_string(), "{value}");
    }
}

#[test]
fn length_queries_attribute_only_panic_allocation_and_resolved_dispatch_stays_known() {
    use willow_compiler::semantic::effects::RuntimeEffects as E;
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        include_str!("../example/effect_explanations.wi"),
    )
    .unwrap();
    for name in [
        "array_len",
        "frozen_len",
        "map_len",
        "frozen_map_len",
        "string_len",
        "increment",
        "dispatch",
    ] {
        let value = f.json(&["effects", &format!("main::{name}"), "--explain"], 0);
        let result = &value["result"];
        assert_eq!(result["status"], "ok", "{name}: {value}");
        assert!(result["reason"].is_null(), "{value}");
        if ["array_len", "frozen_len"].contains(&name) {
            let allocation = result["effect_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["effect"] == E::MAY_ALLOCATE.bits())
                .unwrap();
            assert_eq!(
                allocation["witness"]["cause"]["operation"], "panic-payload-allocation",
                "{value}"
            );
            let reason = allocation["witness"]["reason"].as_str().unwrap();
            assert!(
                reason.contains("without allocating") && reason.contains("null receiver"),
                "{value}"
            );
            let output = f.run(&["effects", &format!("main::{name}")]);
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(
                text.contains(reason) && text.contains("panic-payload-allocation"),
                "{text}"
            );
        } else {
            assert_eq!(
                result["runtime_effects"].as_u64().unwrap() & u64::from(E::MAY_ALLOCATE.bits()),
                0,
                "{value}"
            );
        }
    }
}

#[test]
fn constants_report_their_value_type_and_references() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/limits.wi"),
        "module limits;\npub const MAX: i64 = 3;\npub const SPARE: bool = true;\n",
    )
    .unwrap();
    fs::write(
        f.0.join("src/main.wi"),
        "import limits;\nimport limits::MAX;\nfn main() { println(MAX + limits::MAX); }\n",
    )
    .unwrap();
    let symbol = f.json(&["symbol", "limits::MAX"], 0);
    assert_eq!(symbol["result"]["type_display"], "i64", "{symbol}");
    let refs = f.json(&["refs", "limits::MAX"], 0);
    assert_eq!(refs["result"]["total"], 3, "{refs}");
    let renamed = f.run(&["rename", "limits::SPARE", "UNUSED"]);
    assert!(
        renamed.status.success(),
        "{}",
        String::from_utf8_lossy(&renamed.stderr)
    );
    let source = fs::read_to_string(f.0.join("src/limits.wi")).unwrap();
    assert!(
        source.contains("pub const UNUSED: bool = true;"),
        "{source}"
    );
}

#[test]
fn callee_type_cli_reports_callable_types() {
    let f = Fixture::new();
    let source = include_str!("../example/callee_type_queries.wi");
    fs::write(f.0.join("src/main.wi"), source).unwrap();
    for (needle, expected) in [
        ("callback(value)", "closure(i64) -> f64"),
        ("function(value)", "fn(i64) -> i64"),
        ("increment(3)", "fn(i64) -> i64"),
        ("local(4)", "fn(i64) -> i64"),
        ("captured(5)", "closure(i64) -> i64"),
    ] {
        let byte = source.find(needle).unwrap();
        let line = source[..byte].bytes().filter(|&b| b == b'\n').count() + 1;
        let column = byte - source[..byte].rfind('\n').unwrap();
        let selector = format!("src/main.wi:{line}:{column}");
        let value = f.json(&["type", &selector], 0);
        assert_eq!(
            value["result"]["type_display"], expected,
            "{selector}: {value}"
        );
        let output = f.run(&["type", &selector]);
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains(&format!("Type: {expected}")), "{text}");
    }
}

#[test]
fn lambda_human_anchors_and_legacy_selectors_round_trip() {
    let f = Fixture::new();
    let source = "fn leaf(n: i64) -> i64 { println(n); return n; }\nfn main() {\n    let f = |n: i64| leaf(n);\n    println(f(1));\n}\n";
    fs::write(f.0.join("src/main.wi"), source).unwrap();
    let impact = f.json(&["impact", "main::leaf"], 0);
    let node = impact["result"]["impact"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["selector"].as_str().unwrap().contains("<lambda "))
        .unwrap();
    let legacy = node["selector"].as_str().unwrap();
    assert!(
        legacy.contains(&format!("@{}:", source.find("|n:").unwrap())),
        "{legacy}"
    );
    let unqualified = legacy.strip_prefix("main::").unwrap();
    let short = f.json(
        &[
            "effects",
            unqualified,
            "--kind",
            "function",
            "--module",
            "main",
        ],
        0,
    );
    assert_eq!(short["result"]["selected"]["id"], node["id"]);
    for command in ["effects", "impact"] {
        let old = f.json(&[command, legacy], 0);
        let anchored = f.json(&[command, "src/main.wi:3:13"], 0);
        let selected = if command == "impact" {
            "symbol"
        } else {
            "selected"
        };
        assert_eq!(old["result"][selected]["id"], node["id"]);
        assert_eq!(anchored["result"][selected]["id"], node["id"]);
        let output = f.run(&[command, legacy]);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("Symbol: src/main.wi:3:13"), "{text}");
        if command == "effects" {
            assert!(text.contains("via=src/main.wi:3:13"), "{text}");
        }
    }
    for absolute in [false, true] {
        let mut args = vec!["impact", "main::leaf"];
        if absolute {
            args.push("--absolute-paths");
        }
        let output = f.run(&args);
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8(output.stdout).unwrap();
        let anchor = if absolute {
            format!("{}:3:13", f.0.join("src/main.wi").display())
        } else {
            "src/main.wi:3:13".into()
        };
        assert!(text.lines().any(|line| line.trim() == anchor), "{text}");
        assert!(!text.contains("<lambda "), "{text}");
        let resolved = f.json(&["effects", &anchor], 0);
        assert_eq!(resolved["result"]["selected"]["id"], node["id"]);
    }
}

#[test]
fn lambda_anchors_cover_imports_nested_siblings_and_unicode_crlf() {
    use willow_compiler::ai::direct::SourceIndex;
    for (body, newline) in [
        ("let f = |n: i64| leaf(n); return f(n);", "\n"),
        (
            "let a = |n: i64| { return leaf(n); }; let b = |n: i64| leaf(n); return a(n) + b(n);",
            "\n",
        ),
        (
            "let a = |n: i64| { let b = |v: i64| leaf(v); return b(n); }; return a(n);",
            "\n",
        ),
        (
            "let label = \"日本😀\"; let f = |n: i64| leaf(n); return f(n);",
            "\r\n",
        ),
    ] {
        let f = Fixture::new();
        let source = format!(
            "module order;{newline}pub fn leaf(n: i64) -> i64 {{ println(n); return n; }}{newline}pub fn run(n: i64) -> i64 {{ {body} }}{newline}"
        );
        fs::write(f.0.join("src/order.wi"), &source).unwrap();
        fs::write(
            f.0.join("src/main.wi"),
            "import order; fn main() { println(order::run(1)); }\n",
        )
        .unwrap();
        let impact = f.json(&["impact", "order::leaf"], 0);
        let nodes = impact["result"]["impact"]["nodes"].as_array().unwrap();
        let lambdas: Vec<_> = nodes
            .iter()
            .filter(|n| n["selector"].as_str().unwrap().contains("<lambda "))
            .collect();
        assert!(!lambdas.is_empty(), "{impact}");
        let human = f.run(&["impact", "order::leaf"]);
        assert!(human.status.success(), "{human:?}");
        let text = String::from_utf8(human.stdout).unwrap();
        let index = SourceIndex::new(source);
        for node in lambdas {
            let legacy = node["selector"].as_str().unwrap();
            let offset: usize = legacy
                .rsplit_once('@')
                .unwrap()
                .1
                .split(':')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            let (line, column) = index.position(offset).unwrap();
            let anchor = format!("src/order.wi:{line}:{column}");
            assert!(text.lines().any(|line| line.trim() == anchor), "{text}");
            for selector in [legacy, anchor.as_str()] {
                let result = f.json(&["effects", selector], 0);
                assert_eq!(result["result"]["selected"]["id"], node["id"]);
            }
            let filtered = f.json(&["effects", &anchor, "--kind", "method"], 1);
            assert_eq!(filtered["status"], "unknown");
        }
        assert!(!text.contains("<lambda "), "{text}");
    }
}

#[test]
fn stale_local_rename_prefers_same_declaration_and_rejects_collision() {
    let f = Fixture::new();
    let path = f.0.join("src/main.wi");
    let source = "fn main() {\n    let pred = 1;\n    let rejected = 2;\n    let expr = 3;\n    println(pred + rejected + expr);\n}\n";
    fs::write(&path, source).unwrap();
    let offset_selector = format!("main::main::pred@{}", source.find("pred").unwrap());
    f.json(&["rename", "main::main::pred", "matcher", "--dry-run"], 0);
    assert!(!f.0.join(".willow-edits/last-local-rename.json").exists());
    f.json(&["rename", "main::main::pred", "matcher"], 0);
    let after = fs::read_to_string(&path).unwrap();
    for selector in ["main::main::pred", "main::pred", "pred", &offset_selector] {
        let result = f.json(&["rename", selector, "rejected"], 1);
        assert_eq!(result["status"], "unknown", "{result}");
        assert_eq!(
            result["result"]["suggestions"][0], "main::main::matcher",
            "{result}"
        );
        let suggestions = result["result"]["suggestions"].as_array().unwrap();
        assert!(
            suggestions
                .iter()
                .all(|s| !s.as_str().unwrap().contains('@')),
            "{result}"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), after);
    }
    let collision = f.run(&["rename", "main::main::matcher", "rejected"]);
    assert!(!collision.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&collision.stdout),
        String::from_utf8_lossy(&collision.stderr)
    );
    // Windows diagnostics use backslashes in relative paths.
    let normalized = text.replace('\\', "/");
    assert!(normalized.contains("src/main.wi:3:9"), "{text}");
    assert_eq!(fs::read_to_string(&path).unwrap(), after);
    let filtered = f.json(&["symbol", "main::main::pred", "--kind", "function"], 1);
    assert!(
        !filtered["result"]["suggestions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == "main::main::matcher")
    );
    fs::write(&path, after.replace("matcher", "different")).unwrap();
    let stale = f.json(&["symbol", "main::main::pred"], 1);
    assert!(
        stale["result"]["suggestions"]
            .as_array()
            .unwrap()
            .is_empty(),
        "{stale}"
    );
    fs::write(f.0.join(".willow-edits/last-local-rename.json"), "invalid").unwrap();
    f.json(&["symbol", "main::main::different"], 0);
}

#[test]
fn stale_parameter_suggestion_beats_similar_spelling_and_respects_scope() {
    let f = Fixture::new();
    fs::write(f.0.join("src/main.wi"), "fn serve(pred: i64) { let preds = 2; println(pred + preds); } fn other() { let pred = 3; println(pred); } fn main() { serve(1); }\n").unwrap();
    f.json(&["rename", "main::serve::pred", "matcher"], 0);
    let result = f.json(&["symbol", "main::serve::pred"], 1);
    let suggestions = result["result"]["suggestions"].as_array().unwrap();
    assert_eq!(suggestions[0], "main::serve::matcher", "{result}");
    assert_eq!(suggestions[1], "main::serve::preds", "{result}");
    assert_eq!(suggestions.len(), 2, "{result}");
    f.json(&["symbol", "main::other::pred"], 0);
}

#[test]
fn effect_precision_arithmetic_select_concat_and_allocation() {
    use willow_compiler::semantic::effects::RuntimeEffects as E;
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        include_str!("../example/effect_precision.wi"),
    )
    .unwrap();
    for name in [
        "float_div",
        "float_zero",
        "int_div",
        "int_rem",
        "int_one",
        "rem_one",
    ] {
        let value = f.json(&["effects", &format!("main::{name}")], 0);
        assert_eq!(
            value["result"]["runtime_effects"].as_u64().unwrap() & u64::from(E::MAY_PANIC.bits()),
            0,
            "{name}: {value}"
        );
    }
    for name in [
        "int_variable",
        "rem_variable",
        "int_zero",
        "rem_zero",
        "int_negative_one",
    ] {
        let value = f.json(&["effects", &format!("main::{name}")], 0);
        assert_ne!(
            value["result"]["runtime_effects"].as_u64().unwrap() & u64::from(E::MAY_PANIC.bits()),
            0,
            "{name}: {value}"
        );
    }
    for name in ["concat", "concat_relay", "async_select", "Selector::wait"] {
        let value = f.json(&["effects", &format!("main::{name}")], 0);
        let bits = value["result"]["runtime_effects"].as_u64().unwrap();
        assert_eq!(bits & u64::from(E::MAY_BLOCK.bits()), 0, "{name}: {value}");
        assert_eq!(
            bits & u64::from(E::NO_PREEMPT_REGION.bits()),
            0,
            "{name}: {value}"
        );
        if name.contains("concat") {
            assert_ne!(bits & u64::from(E::MAY_ALLOCATE.bits()), 0, "{value}");
            assert_ne!(bits & u64::from(E::MAY_PANIC.bits()), 0, "{value}");
        } else {
            assert_ne!(bits & u64::from(E::MAY_SUSPEND.bits()), 0, "{value}");
        }
    }
    for name in ["sync_select", "select_with_io"] {
        let value = f.json(&["effects", &format!("main::{name}")], 0);
        assert_ne!(
            value["result"]["runtime_effects"].as_u64().unwrap() & u64::from(E::MAY_BLOCK.bits()),
            0,
            "{name}: {value}"
        );
    }
    for (name, operation) in [
        ("len_only", "panic-payload-allocation"),
        ("allocate_after_len", "array-allocation"),
        ("allocate_before_len", "array-allocation"),
        ("allocate_in_callee", "array-allocation"),
    ] {
        let value = f.json(&["effects", &format!("main::{name}"), "--explain"], 0);
        let allocation = value["result"]["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["effect"] == E::MAY_ALLOCATE.bits())
            .unwrap();
        assert_eq!(
            allocation["witness"]["cause"]["operation"], operation,
            "{name}: {value}"
        );
    }
}

#[test]
fn effect_precision_select_callable_contexts() {
    use willow_compiler::semantic::effects::RuntimeEffects as E;
    use willow_compiler::{CompilerOptions, CompilerSession, diagnostics::HumanEmitter};
    let f = Fixture::new();
    let path = f.0.join("src/main.wi");
    fs::write(
        &path,
        r#"
async fn enclosing() { || { select { sleep(0) => {} } }; }
async fn recv_case(ch: Channel<i64>) { select { let v = ch.recv() => {} } }
async fn send_case(ch: Channel<i64>) { select { ch.send(1) => {} } }
async fn default_case() { select { default => {} } }
class Selector {
    pub fn wait(self) { select { sleep(0) => {} } }
    pub async fn cooperative(self) { select { sleep(0) => {} } }
}
interface Waiting { fn wait(self) { select { sleep(0) => {} } } }
fn main() {}
"#,
    )
    .unwrap();
    let snapshot =
        CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
            .analysis_with_emitter(&mut HumanEmitter)
            .unwrap();
    for name in [
        "enclosing",
        "recv_case",
        "send_case",
        "default_case",
        "Selector::cooperative",
    ] {
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        assert_eq!(
            function.runtime_effects & E::MAY_BLOCK.bits(),
            0,
            "{name}: {function:?}"
        );
        assert_eq!(
            function.runtime_effects & E::NO_PREEMPT_REGION.bits(),
            0,
            "{name}: {function:?}"
        );
    }
    let sync: Vec<_> = snapshot
        .functions
        .iter()
        .filter(|f| {
            f.name.contains("<lambda")
                || f.name == "Selector::wait"
                || f.name.contains("$default$wait")
        })
        .collect();
    assert_eq!(
        sync.len(),
        3,
        "{:?}",
        snapshot
            .functions
            .iter()
            .map(|f| &f.name)
            .collect::<Vec<_>>()
    );
    for function in sync {
        assert_ne!(
            function.runtime_effects & E::MAY_BLOCK.bits(),
            0,
            "{function:?}"
        );
    }
}

#[test]
fn references_sort_source_positions_and_show_roles() {
    let f = Fixture::new();
    fs::write(f.0.join("src/main.wi"), "import order::{submit};\nfn z() { submit(1); submit(2); }\nfn a() { submit(3); }\nfn main() { z(); a(); }\n").unwrap();
    let result = f.json(&["refs", "order::submit", "--all"], 0);
    let refs = result["result"]["references"].as_array().unwrap();
    assert_eq!(refs.len(), 4, "{result}");
    assert_eq!(refs[0]["role"], "import-target");
    assert!(refs[1..].iter().all(|r| r["role"] == "call"));
    let positions: Vec<_> = refs
        .iter()
        .map(|r| {
            (
                r["location"]["path"].as_str().unwrap(),
                r["location"]["line"].as_u64().unwrap(),
                r["location"]["column"].as_u64().unwrap(),
            )
        })
        .collect();
    assert!(positions.windows(2).all(|w| w[0] < w[1]), "{result}");
    assert_eq!(result["result"]["coverage"], "compiler-resolved");
    let out = f.run(&["refs", "order::submit"]);
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        text.lines()
            .filter(|l| l.ends_with(" import-target"))
            .count(),
        1,
        "{text}"
    );
    assert_eq!(
        text.lines().filter(|l| l.ends_with(" call")).count(),
        3,
        "{text}"
    );
    assert_eq!(
        f.json(&["refs", "order::Order::value"], 0)["result"]["coverage"],
        "compiler-resolved"
    );
}

#[test]
fn method_reference_order_scales_across_files_and_callers() {
    for n in [16, 64, 256] {
        let f = Fixture::new();
        let mut source = String::from("import order;\n");
        for i in (0..n).rev() {
            source.push_str(&format!(
                "fn caller{i}(x: order::Order) {{ x.value(); x.value(); }}\n"
            ));
        }
        source.push_str("fn main() {}\n");
        fs::write(f.0.join("src/main.wi"), source).unwrap();
        let mut order = fs::read_to_string(f.0.join("src/order.wi")).unwrap();
        order.push_str("fn local(x: Order) { x.value(); }\n");
        fs::write(f.0.join("src/order.wi"), order).unwrap();
        let result = f.json(&["refs", "order::Order::value", "--all"], 0);
        let refs = result["result"]["references"].as_array().unwrap();
        assert_eq!(refs.len(), 2 * n + 1, "{result}");
        let keys: Vec<_> = refs
            .iter()
            .map(|r| {
                (
                    r["location"]["path"].as_str().unwrap(),
                    r["location"]["line"].as_u64().unwrap(),
                    r["location"]["column"].as_u64().unwrap(),
                )
            })
            .collect();
        assert!(keys.windows(2).all(|w| w[0] < w[1]), "{result}");
        assert_eq!(result["result"]["coverage"], "compiler-resolved");
        println!(
            "callers={n} references={} adjacent_checks={}",
            refs.len(),
            keys.len() - 1
        );
    }
}

#[test]
fn select_bindings_support_direct_queries_and_rename() {
    for (operation, ty, setup) in [
        ("ch.recv()", "i64", ""),
        ("ch.recv_opt()", "Option<i64>", ""),
        ("await task", "i64", "let task = answer();"),
    ] {
        for rename_selector in 0..3 {
            let f = Fixture::new();
            let source = format!(
                "module engine;\nasync fn answer() -> i64 {{ return 7; }}\npub async fn search() {{\n    let ch = Channel<i64>::new();\n    {setup}\n    select {{\n        let resp = {operation} => {{ let first = resp; let second = resp; }}\n        default => {{}}\n    }}\n}}\n"
            );
            fs::write(f.0.join("src/engine.wi"), &source).unwrap();
            fs::write(
                f.0.join("src/main.wi"),
                "import engine;\nasync fn main() { await engine::search(); }\n",
            )
            .unwrap();
            let line = source.lines().nth(6).unwrap();
            let usage = format!("src/engine.wi:7:{}", line.find("= resp;").unwrap() + 3);
            let selectors = ["engine::search::resp", "src/engine.wi:7:13", &usage];
            for selector in selectors
                .into_iter()
                .take(if rename_selector == 0 { 3 } else { 0 })
            {
                let symbol = f.json(&["symbol", selector], 0);
                assert_eq!(
                    symbol["result"]["selected"]["selector"]
                        .as_str()
                        .unwrap()
                        .split('@')
                        .next()
                        .unwrap(),
                    "engine::search::resp",
                    "{symbol}"
                );
                let result = f.json(&["type", selector], 0);
                assert_eq!(result["result"]["type_display"], ty, "{result}");
                let refs = f.json(&["refs", selector], 0);
                assert_eq!(refs["result"]["total"], 2, "{refs}");
            }
            let preview = f.json(
                &[
                    "rename",
                    selectors[rename_selector],
                    "response",
                    "--dry-run",
                ],
                0,
            );
            assert_eq!(preview["result"]["edits"], 3, "{preview}");
            assert_eq!(
                fs::read_to_string(f.0.join("src/engine.wi")).unwrap(),
                source
            );
            let applied = f.json(&["rename", selectors[rename_selector], "response"], 0);
            assert_eq!(applied["result"]["validation"], "passed");
            assert_eq!(
                fs::read_to_string(f.0.join("src/engine.wi")).unwrap(),
                source.replace("resp", "response")
            );
        }
    }
}

#[test]
fn select_bindings_keep_sibling_outer_and_nested_scopes_separate() {
    let f = Fixture::new();
    let source = "fn main() {\n let ch = Channel<i64>::new();\n let resp = 1;\n select {\n  let resp = ch.recv() => { println(resp); select { let nested = ch.recv() => { println(nested); } default => {} } }\n  let resp = ch.recv() => { println(resp); }\n  let _ = ch.recv() => {}\n  ch.recv() => {}\n  ch.send(resp) => {}\n  default => {}\n }\n println(resp);\n}\n";
    fs::write(f.0.join("src/main.wi"), source).unwrap();
    let ambiguous = f.json(&["symbol", "main::main::resp"], 1);
    assert_eq!(ambiguous["status"], "ambiguous");
    assert_eq!(
        ambiguous["result"]["candidates"].as_array().unwrap().len(),
        3
    );
    for (line, count) in [(3, 2), (5, 1), (6, 1)] {
        let selector = format!("src/main.wi:{line}:{}", if line == 3 { 6 } else { 7 });
        let refs = f.json(&["refs", &selector], 0);
        assert_eq!(refs["result"]["total"], count, "{refs}");
    }
    let nested = f.json(&["refs", "main::main::nested"], 0);
    assert_eq!(nested["result"]["total"], 1);
    let renamed = f.json(&["rename", "src/main.wi:5:7", "response"], 0);
    assert_eq!(renamed["result"]["edits"], 2);
    assert_eq!(
        fs::read_to_string(f.0.join("src/main.wi")).unwrap(),
        source.replacen(
            "let resp = ch.recv() => { println(resp);",
            "let response = ch.recv() => { println(response);",
            1
        )
    );
}

#[test]
fn indirect_reference_candidates_explain_each_site_in_json_and_text() {
    let f = Fixture::new();
    fs::write(f.0.join("src/main.wi"), "fn submit(n: i64) -> i64 { return n + 1; }\nfn apply(f: fn(i64) -> i64) -> i64 { return f(1) + f(2); }\nfn main() { let local: fn(i64) -> i64 = submit; println(local(3)); println(apply(submit)); }\n").unwrap();
    let value = f.json(&["refs", "main::submit"], 1);
    assert_eq!(value["result"]["status"], "incomplete");
    let candidates = value["result"]["unresolved_candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 3, "{value}");
    let output = f.run(&["refs", "main::submit"]);
    assert_eq!(output.status.code(), Some(1));
    let text = String::from_utf8(output.stdout).unwrap();
    for candidate in candidates {
        assert_eq!(candidate["certainty"], "signature-matched");
        let reason = candidate["reason"].as_str().unwrap();
        assert!(reason.contains("same function type"), "{reason}");
        assert!(reason.contains("not resolved"), "{reason}");
        let loc = &candidate["location"];
        assert_eq!(loc["path"], "src/main.wi");
        let site = format!("src/main.wi:{}:{}", loc["line"], loc["column"]);
        assert!(
            text.contains(&format!("{site} indirect candidate: {reason}")),
            "{text}"
        );
        let source = fs::read_to_string(f.0.join("src/main.wi")).unwrap();
        let token =
            &source[loc["start"].as_u64().unwrap() as usize..loc["end"].as_u64().unwrap() as usize];
        assert!(matches!(token, "f" | "local"), "{token}");
    }
    let absolute = f.json(
        &["references", "main::submit", "--absolute-paths", "--all"],
        1,
    );
    for candidate in absolute["result"]["unresolved_candidates"]
        .as_array()
        .unwrap()
    {
        assert!(
            std::path::Path::new(candidate["location"]["path"].as_str().unwrap()).is_absolute()
        );
        assert!(candidate["reason"].is_string());
    }
    let other = f.json(&["refs", "main::main"], 0);
    assert_eq!(other["result"]["status"], "ok");
    assert!(
        other["result"]["unresolved_candidates"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn indirect_reference_candidate_counts_scale_without_truncation() {
    let f = Fixture::new();
    for n in [1, 8, 64] {
        let calls = "println(f(1));".repeat(n);
        fs::write(f.0.join("src/main.wi"), format!("fn submit(n: i64) -> i64 {{ return n + 1; }}\nfn apply(f: fn(i64) -> i64) {{ {calls} }}\nfn main() {{ apply(submit); }}\n")).unwrap();
        let value = f.json(&["refs", "main::submit"], 1);
        let candidates = value["result"]["unresolved_candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), n);
        let sites: std::collections::HashSet<_> = candidates
            .iter()
            .map(|c| c["location"].to_string())
            .collect();
        assert_eq!(sites.len(), n);
        let output = f.run(&["refs", "main::submit"]);
        assert_eq!(output.status.code(), Some(1));
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(
            text.lines()
                .filter(|line| line.contains("indirect candidate:"))
                .count(),
            n,
            "{text}"
        );
        println!(
            "indirect calls={n} JSON candidates={} text candidates={n}",
            candidates.len()
        );
    }
}

#[test]
fn indirect_reference_candidates_include_imported_static_methods() {
    let f = Fixture::new();
    fs::write(f.0.join("src/order.wi"), "module order;\npub class Plain { pub static fn normalize(t: String) -> String { return t; } }\npub class Stem { pub static fn normalize(t: String) -> String { return t; } }\n").unwrap();
    fs::write(f.0.join("src/main.wi"), "import order;\nfn map_terms(f: fn(String) -> String, t: String) -> String { return f(t); }\nfn main() { println(order::Plain::normalize(\"hi\")); }\n").unwrap();
    for selector in ["order::Plain::normalize", "order::Stem::normalize"] {
        let value = f.json(&["refs", selector], 1);
        let candidates = value["result"]["unresolved_candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 1, "{value}");
        assert_eq!(candidates[0]["location"]["path"], "src/main.wi");
        assert_eq!(candidates[0]["location"]["line"], 2);
        assert_eq!(candidates[0]["location"]["column"], 69);
        assert_eq!(candidates[0]["certainty"], "signature-matched");
        assert!(
            candidates[0]["reason"]
                .as_str()
                .unwrap()
                .contains("same function type")
        );
    }
}

#[test]
fn effect_witness_precision_literals_scheduler_and_unknown_origins() {
    use willow_compiler::semantic::effects::RuntimeEffects as E;
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        include_str!("../example/effect_witness_precision.wi"),
    )
    .unwrap();
    for name in ["literal", "literal_relay"] {
        let value = f.json(&["effects", &format!("main::{name}"), "--explain"], 1);
        assert_ne!(
            value["result"]["runtime_effects"].as_u64().unwrap()
                & u64::from(E::MAY_ALLOCATE.bits()),
            0,
            "{value}"
        );
        let allocation = value["result"]["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["effect"] == E::MAY_ALLOCATE.bits())
            .unwrap();
        assert_eq!(allocation["status"], "missing-witness", "{value}");
        assert!(allocation.get("witness").is_none(), "{value}");
    }
    for name in [
        "panic_message",
        "allocate_after_literal",
        "allocate_before_literal",
        "allocate_relay",
        "concat",
    ] {
        let value = f.json(
            &["effects", &format!("main::{name}"), "--explain"],
            if name == "panic_message" { 1 } else { 0 },
        );
        assert!(!value.to_string().contains("string-allocation"), "{value}");
        if name.starts_with("allocate") || name == "concat" {
            let allocation = value["result"]["effect_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["effect"] == E::MAY_ALLOCATE.bits())
                .unwrap();
            assert_eq!(
                allocation["witness"]["cause"]["operation"],
                if name == "concat" {
                    "string-concatenation"
                } else {
                    "object-allocation"
                },
                "{value}"
            );
            assert!(
                allocation["witness"]["cause"]["location"]["line"].is_number(),
                "{value}"
            );
        }
    }
    for name in [
        "timer_zero",
        "timer_positive",
        "timer_negative",
        "scheduler_yield",
        "timer_relay",
    ] {
        let value = f.json(&["effects", &format!("main::{name}"), "--explain"], 0);
        let bits = value["result"]["runtime_effects"].as_u64().unwrap();
        assert_eq!(
            bits & u64::from(E::MAY_BLOCK.bits() | E::NO_PREEMPT_REGION.bits()),
            0,
            "{value}"
        );
        assert_ne!(bits & u64::from(E::MAY_SUSPEND.bits()), 0, "{value}");
    }
    for name in ["apply", "relay", "dispatch"] {
        let value = f.json(&["effects", &format!("main::{name}"), "--explain"], 1);
        let causes = value["result"]["unknown_causes"].as_array().unwrap();
        let cause = causes.iter().find(|c| c["name"] == "apply").unwrap();
        assert!(
            !cause["unresolved"].as_array().unwrap().is_empty(),
            "{value}"
        );
        assert_eq!(cause["locations"][0]["line"], 16, "{value}");
        let output = f.run(&["effects", &format!("main::{name}")]);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains("Incomplete effect summary: main::apply"),
            "{text}"
        );
        assert!(text.contains("src/main.wi:16:"), "{text}");
    }
}

#[test]
fn effect_witness_precision_shadowed_scheduler_names_stay_conservative() {
    use willow_compiler::semantic::effects::RuntimeEffects as E;
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        "async fn sleep(n: i64) {}\nasync fn custom() { await sleep(1); }\nasync fn main() {}\n",
    )
    .unwrap();
    let value = f.json(&["effects", "main::custom", "--explain"], 0);
    let block = value["result"]["effect_evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["effect"] == E::MAY_BLOCK.bits())
        .unwrap();
    assert_eq!(block["witness"]["cause"]["operation"], "await", "{value}");
}

#[test]
fn generic_source_queries() {
    let f = Fixture::new();
    fs::write(f.0.join("src/coll.wi"), "module coll;\npub fn fold<T>(value: T) -> T { return value; }\npub fn unused<T>(value: T) -> T { return value; }\npub class Ring<T> {\n    pub value: T;\n    pub fn push(self, value: T) -> T { return value; }\n}\n").unwrap();
    fs::write(f.0.join("src/main.wi"), "import coll::fold;\nimport coll::Ring;\nfn main() { println(fold(1)); println(fold(2.0)); let r = new Ring<i64>(3); println(r.push(4)); }\n").unwrap();
    let check = f.run(&["check"]);
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let overview = f.json(&["overview", "src/coll.wi", "--depth", "1"], 0);
    let text = overview.to_string();
    assert!(text.contains("fold"), "{text}");
    assert!(text.contains("unused"), "{text}");
    assert!(text.contains("Ring"), "{text}");
    let symbols = overview["files"][0]["symbols"].as_array().unwrap();
    for name in ["fold", "unused", "Ring"] {
        let symbol = symbols.iter().find(|s| s["name"] == name).unwrap();
        assert_eq!(symbol["type_params"], serde_json::json!(["T"]), "{symbol}");
    }
    assert!(!text.contains("$mono$"));
    assert!(!text.contains("$pkg"));
    let callee = f.json(&["type", "src/main.wi:3:21"], 0);
    assert_eq!(callee["result"]["type_display"], "fn(T) -> T", "{callee}");
    let unused = f.json(&["symbol", "coll::unused"], 0);
    assert_eq!(
        unused["result"]["selected"]["selector"], "coll::unused",
        "{unused}"
    );
    let ring = f.json(&["symbol", "coll::Ring"], 0);
    assert_eq!(
        ring["result"]["selected"]["selector"], "coll::Ring",
        "{ring}"
    );
    let binding = f.json(&["type", "main::main::r"], 0);
    assert_eq!(
        binding["result"]["type_display"], "coll::Ring<i64>",
        "{binding}"
    );
    let effects = f.json(&["effects", "main::main"], 0);
    assert_eq!(effects["result"]["status"], "ok", "{effects}");
    for selector in [
        "coll::fold",
        "coll::Ring::push",
        "src/coll.wi:2:8",
        "src/coll.wi:6:12",
    ] {
        for command in ["symbol", "refs", "type", "impact", "effects"] {
            let value = f.json(&[command, selector], 0);
            let text = value.to_string();
            if command == "type" {
                assert!(
                    value["result"]["type_display"]
                        .as_str()
                        .is_some_and(|ty| ty.starts_with("fn(")),
                    "callee/declaration must report callable type: {text}"
                );
            }
            if command == "refs" && selector == "coll::fold" {
                assert_eq!(
                    value["result"]["references"].as_array().unwrap().len(),
                    3,
                    "{value}"
                );
            }
            if command == "effects" {
                assert_eq!(value["result"]["status"], "ok", "{value}");
                assert_eq!(
                    value["result"]["unknown_causes"],
                    serde_json::json!([]),
                    "{value}"
                );
            }
            assert!(!text.contains("$mono$"), "{command} {selector}: {text}");
            assert!(!text.contains("$pkg"), "{command} {selector}: {text}");
        }
    }
    let refs = f.json(&["refs", "coll::fold"], 0);
    assert!(refs.to_string().contains("main.wi"), "{refs}");
    let preview = f.json(&["rename", "coll::fold", "reduce", "--dry-run"], 0);
    assert!(preview.to_string().contains("reduce"), "{preview}");
    assert_eq!(preview["result"]["edits"], 4, "{preview}");
    let method = f.json(&["rename", "coll::Ring::push", "put", "--dry-run"], 0);
    assert_eq!(method["result"]["edits"], 2, "{method}");
    for format in ["human", "ndjson"] {
        for args in [
            vec!["overview", "src/coll.wi", "--depth", "1"],
            vec!["effects", "main::main"],
            vec!["symbol", "coll::fol"],
        ] {
            let mut args = args;
            args.extend(["--format", format]);
            let out = f.run(&args);
            let text = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(!text.contains("$mono$"), "{text}");
            assert!(!text.contains("$pkg"), "{text}");
        }
    }
    f.json(&["rename", "coll::fold", "reduce"], 0);
    assert!(
        fs::read_to_string(f.0.join("src/coll.wi"))
            .unwrap()
            .contains("fn reduce<T>")
    );
    let main = fs::read_to_string(f.0.join("src/main.wi")).unwrap();
    assert!(main.contains("import coll::reduce;"));
    assert_eq!(main.matches("reduce(").count(), 2);
    f.json(&["rename", "coll::Ring::push", "put"], 0);
    assert!(
        fs::read_to_string(f.0.join("src/coll.wi"))
            .unwrap()
            .contains("fn put(")
    );
    assert!(
        fs::read_to_string(f.0.join("src/main.wi"))
            .unwrap()
            .contains("r.put(")
    );
    fs::write(f.0.join("src/other.wi"), "module other;\nimport coll::reduce as preserve;\nimport coll::Ring as Bucket;\npub fn apply() -> i64 { let bucket = new Bucket<i64>(2); return preserve(bucket.put(9)); }\n").unwrap();
    let main = fs::read_to_string(f.0.join("src/main.wi")).unwrap();
    fs::write(f.0.join("src/main.wi"), format!("import other;\n{main}")).unwrap();
    let source = fs::read_to_string(f.0.join("src/other.wi")).unwrap();
    for (token, command, expected) in [
        ("preserve(bucket", "type", "fn(T) -> T"),
        ("Bucket<i64>", "symbol", "coll::Ring"),
    ] {
        let start = source.find(token).unwrap();
        let prefix = &source[..start];
        let line = prefix.bytes().filter(|b| *b == b'\n').count() + 1;
        let column = prefix.rsplit('\n').next().unwrap().chars().count() + 1;
        let selector = format!("src/other.wi:{line}:{column}");
        let result = f.json(&[command, &selector], 0);
        if command == "type" {
            assert_eq!(result["result"]["type_display"], expected, "{result}");
        } else {
            assert_eq!(
                result["result"]["selected"]["selector"], expected,
                "{result}"
            );
        }
    }
    let refs = f.json(&["refs", "coll::reduce"], 0);
    assert_eq!(
        refs["result"]["references"].as_array().unwrap().len(),
        5,
        "{refs}"
    );
    let preview = f.json(&["rename", "coll::reduce", "combine", "--dry-run"], 0);
    assert_eq!(preview["result"]["edits"], 5, "{preview}");
    f.json(&["rename", "coll::reduce", "combine"], 0);
    let other = fs::read_to_string(f.0.join("src/other.wi")).unwrap();
    assert!(
        other.contains("import coll::combine as preserve;"),
        "{other}"
    );
    assert!(other.contains("return preserve(bucket.put(9))"), "{other}");
}

#[test]
fn uninstantiated_generic_body_refs_and_rename() {
    for (cross_module, instantiated_target) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let f = Fixture::new();
        let (selector, caller, file, original) = if cross_module {
            fs::write(
                f.0.join("src/coll.wi"),
                "module coll;\npub fn fold<T>(x: T) -> T { return x; }\n",
            )
            .unwrap();
            let source = "module unused;\nimport coll::fold as imported;\npub fn wrapper<T>(x: T) -> T { return imported(x); }\n";
            fs::write(f.0.join("src/unused.wi"), source).unwrap();
            fs::write(
                f.0.join("src/main.wi"),
                if instantiated_target {
                    "import unused; import coll; fn main() { println(coll::fold(1)); }\n"
                } else {
                    "import unused; fn main() {}\n"
                },
            )
            .unwrap();
            (
                "coll::fold",
                "unused::wrapper",
                "src/unused.wi",
                source.to_owned(),
            )
        } else {
            let source = format!(
                "fn fold<T>(x: T) -> T {{ return x; }}\nfn wrapper<T>(x: T) -> T {{ return fold(x); }}\nfn main() {{ {} }}\n",
                if instantiated_target {
                    "println(fold(1));"
                } else {
                    ""
                }
            );
            fs::write(f.0.join("src/main.wi"), &source).unwrap();
            ("main::fold", "main::wrapper", "src/main.wi", source)
        };
        let refs = f.json(&["refs", selector], 0);
        let references = refs["result"]["references"].as_array().unwrap();
        assert_eq!(
            references.len(),
            if cross_module { 2 } else { 1 } + usize::from(instantiated_target),
            "{refs}"
        );
        let call = references
            .iter()
            .find(|r| r["location"]["path"] == file && r["role"] == "call")
            .unwrap();
        let position = format!(
            "{file}:{}:{}",
            call["location"]["line"], call["location"]["column"]
        );
        let symbol = f.json(&["symbol", &position], 0);
        assert_eq!(
            symbol["result"]["selected"]["selector"], selector,
            "{symbol}"
        );
        let typed = f.json(&["type", &position], 0);
        assert_eq!(typed["result"]["type_display"], "fn(T) -> T", "{typed}");
        let impact = f.json(&["impact", selector], 0);
        assert!(impact.to_string().contains(caller), "{impact}");
        if !cross_module {
            fs::write(
                f.0.join(file),
                format!(
                    "{original}fn shadow<T>(fold: fn(T) -> T, x: T) -> T {{ return fold(x); }}\n"
                ),
            )
            .unwrap();
        }
        let preview = f.json(&["rename", selector, "reduce", "--dry-run"], 0);
        assert_eq!(
            preview["result"]["edits"],
            2 + usize::from(instantiated_target),
            "{preview}"
        );
        f.json(&["rename", selector, "reduce"], 0);
        let source = fs::read_to_string(f.0.join(file)).unwrap();
        assert!(
            source.contains(if cross_module {
                "return imported(x);"
            } else {
                "return reduce(x);"
            }),
            "{source}"
        );
        if cross_module {
            assert!(
                source.contains("import coll::reduce as imported;"),
                "{source}"
            );
            assert!(
                fs::read_to_string(f.0.join("src/coll.wi"))
                    .unwrap()
                    .contains("fn reduce<T>")
            );
        } else {
            assert!(source.contains("fn reduce<T>"));
            assert!(
                source.contains("fn shadow<T>(fold: fn(T) -> T, x: T) -> T { return fold(x); }"),
                "{source}"
            );
        }
        assert_eq!(
            original.matches("fn wrapper<T>").count(),
            source.matches("fn wrapper<T>").count()
        );
        for text in [
            refs.to_string(),
            symbol.to_string(),
            typed.to_string(),
            preview.to_string(),
        ] {
            assert!(!text.contains("$mono$"), "{text}");
            assert!(!text.contains("$pkg"), "{text}");
        }
    }
}

#[test]
fn analysis_readonly_routes_leave_lock_and_metadata_untouched() {
    for project in [true, false] {
        for existing in [false, true] {
            for route in ["api", "query", "impact", "overview", "symbol"] {
                let f = Fixture::new();
                fs::write(f.0.join("src/main.wi"), "fn main() { println(1); }\n").unwrap();
                fs::write(f.0.join("requests.json"), "[]").unwrap();
                let workspace = if project {
                    f.0.clone()
                } else {
                    f.0.join("src")
                };
                let metadata = workspace.join(".willow-edits");
                if existing {
                    fs::create_dir(&metadata).unwrap();
                    fs::write(metadata.join("keep"), "unchanged").unwrap();
                }
                let entry = f.0.join("src/main.wi");
                let target = if project { "." } else { "src/main.wi" };
                if route == "api" {
                    willow_compiler::CompilerSession::new(
                        entry.to_str().unwrap(),
                        "",
                        &willow_compiler::CompilerOptions::debug(),
                        project.then(|| f.0.clone()),
                    )
                    .analysis_with_emitter(&mut willow_compiler::diagnostics::HumanEmitter)
                    .unwrap();
                } else {
                    let args = match route {
                        "query" => vec!["query", target, "--requests", "requests.json"],
                        "impact" => vec!["impact", target, "--file", "src/main.wi", "--byte", "3"],
                        "overview" => vec!["overview", target, "--format=json"],
                        "symbol" if project => vec![
                            "symbol",
                            "main::main",
                            "--project-dir",
                            ".",
                            "--format=json",
                        ],
                        "symbol" => vec![
                            "symbol",
                            "main::main",
                            "--source",
                            "src/main.wi",
                            "--format=json",
                        ],
                        _ => unreachable!(),
                    };
                    let output = f.run(&args);
                    assert!(
                        output.status.success(),
                        "route={route} project={project}: {} {}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                assert_eq!(
                    metadata.exists(),
                    existing,
                    "route={route} project={project} existing={existing}"
                );
                assert!(
                    !f.0.join("project.lock").exists(),
                    "route={route} project={project}"
                );
                if existing {
                    assert_eq!(
                        fs::read_to_string(metadata.join("keep")).unwrap(),
                        "unchanged"
                    );
                }
            }
        }
    }
}

#[test]
fn generic_import_source_provenance_matches_project_and_standalone() {
    #[derive(Default)]
    struct Warnings(Vec<willow_compiler::diagnostics::Diagnostic>);
    impl willow_compiler::diagnostics::DiagnosticEmitter for Warnings {
        fn emit(
            &mut self,
            diagnostic: &willow_compiler::diagnostics::Diagnostic,
            _: &dyn willow_compiler::diagnostics::source_map::SourceLookup,
        ) -> std::io::Result<()> {
            self.0.push(diagnostic.clone());
            Ok(())
        }
    }
    for project in [false, true] {
        for (import, callee) in [
            ("import coll::fold;", "fold"),
            ("import coll::fold as keep;", "keep"),
            ("import coll::{fold};", "fold"),
            ("import coll;", "coll::fold"),
            ("import coll as model;", "model::fold"),
        ] {
            for explicit in [false, true] {
                let f = Fixture::new();
                fs::write(
                    f.0.join("src/coll.wi"),
                    "pub fn fold<T>(value: T) -> T { return value; }\n",
                )
                .unwrap();
                let args = if explicit { "<i64>" } else { "" };
                let source = format!("{import}\nfn main() {{ println({callee}{args}(7)); }}\n");
                fs::write(f.0.join("src/main.wi"), &source).unwrap();
                let entry = f.0.join("src/main.wi");
                let mut diagnostics = Warnings::default();
                let snapshot = willow_compiler::CompilerSession::new(
                    entry.to_str().unwrap(),
                    "",
                    &willow_compiler::CompilerOptions::debug(),
                    project.then(|| f.0.clone()),
                )
                .analysis_with_emitter(&mut diagnostics)
                .unwrap();
                if !callee.contains("::") {
                    assert!(
                        !snapshot
                            .semantic
                            .symbols
                            .iter()
                            .any(|s| s.kind == "builtin-type" && s.name == "coll"),
                        "phantom namespace for {import} project={project}"
                    );
                }
                assert!(
                    !diagnostics
                        .0
                        .iter()
                        .any(|d| d.code == willow_compiler::diagnostics::ErrorCode::W2003),
                    "{import} project={project}: {:?}",
                    diagnostics.0
                );
                let function = snapshot
                    .semantic
                    .symbols
                    .iter()
                    .find(|s| s.name == "fold" && s.kind == "function")
                    .unwrap();
                let calls: Vec<_> = snapshot
                    .semantic
                    .references
                    .iter()
                    .filter(|r| r.target == function.id && r.role == "call")
                    .collect();
                assert_eq!(calls.len(), 1, "{import} project={project}");
                let call = &calls[0].location;
                assert_eq!(
                    &source[call.start..call.end],
                    callee.rsplit("::").next().unwrap()
                );
            }
        }
    }

    // The provenance filter must not hide a genuinely unused source import.
    let f = Fixture::new();
    fs::write(
        f.0.join("src/coll.wi"),
        "pub fn fold<T>(value: T) -> T { return value; }\n",
    )
    .unwrap();
    fs::write(
        f.0.join("src/main.wi"),
        "import coll::fold as keep; fn main() {}\n",
    )
    .unwrap();
    let entry = f.0.join("src/main.wi");
    let mut diagnostics = Warnings::default();
    willow_compiler::CompilerSession::new(
        entry.to_str().unwrap(),
        "",
        &willow_compiler::CompilerOptions::debug(),
        None,
    )
    .analysis_with_emitter(&mut diagnostics)
    .unwrap();
    let unused: Vec<_> = diagnostics
        .0
        .iter()
        .filter(|d| d.code == willow_compiler::diagnostics::ErrorCode::W2003)
        .collect();
    assert_eq!(unused.len(), 1, "{unused:?}");
    assert_eq!(unused[0].message, "unused import `keep`");
}

#[test]
fn builtin_effect_summaries_are_complete_and_located() {
    use willow_compiler::semantic::effects::RuntimeEffects as E;
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        r#"
import std::collections::Map;
fn res(r: Result<i64, String>) -> i64 { return r.unwrap(); }
fn opt(m: Map<String, i64>) -> i64 { return m.get("a").unwrap(); }
fn some(o: Option<i64>) -> bool { return o.is_some(); }
fn ok(r: Result<i64, String>) -> bool { return r.is_ok(); }
fn parse_only(s: String) -> bool { return f64::parse(s).is_ok(); }
fn conv(s: String) -> f64 { return match f64::parse(s) { Ok(v) => v, Err(_) => 0.0 }; }
fn main() {}
"#,
    )
    .unwrap();
    for name in ["res", "opt", "some", "ok", "parse_only", "conv"] {
        let v = f.json(&["effects", &format!("main::{name}"), "--explain"], 0);
        assert_eq!(v["result"]["status"], "ok", "{v}");
        let bits = v["result"]["runtime_effects"].as_u64().unwrap();
        assert_eq!(
            bits & !u64::from(E::MAY_PANIC.union(E::MAY_ALLOCATE).bits()),
            0,
            "{v}"
        );
        for evidence in v["result"]["effect_evidence"].as_array().unwrap() {
            assert!(
                evidence["witness"]["cause"]["location"]["line"].is_number(),
                "{v}"
            );
        }
    }
}

#[test]
fn effect_await_reasons_use_checked_operand_types() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        r#"
async fn value() -> i64 { return 1; }
async fn task_wait(t: Task<i64>) -> i64 { return await t; }
async fn future_wait() { let f = sleep(1); await f; }
async fn main() {}
"#,
    )
    .unwrap();
    for (name, ty) in [("task_wait", "Task<i64>"), ("future_wait", "Future<void>")] {
        let v = f.json(&["effects", &format!("main::{name}"), "--explain"], 0);
        let evidence = v["result"]["effect_evidence"].as_array().unwrap();
        let waits: Vec<_> = evidence
            .iter()
            .filter(|e| e["witness"]["cause"]["operation"] == "await")
            .collect();
        assert!(!waits.is_empty(), "{v}");
        for e in waits {
            assert_eq!(e["witness"]["awaited_type"], ty, "{v}");
            assert!(e["witness"]["reason"].as_str().unwrap().contains(ty), "{v}");
            assert!(!e.to_string().contains("willow_future_await_void"), "{v}");
        }
    }
}

#[test]
fn effect_unresolved_calls_name_the_exact_site() {
    let f = Fixture::new();
    fs::write(
        f.0.join("src/main.wi"),
        "fn apply(f: fn(i64) -> i64) -> i64 { return f(1); }\nfn main() {}\n",
    )
    .unwrap();
    let v = f.json(&["effects", "main::apply", "--explain"], 1);
    let sites = v["result"]["unknown_causes"][0]["call_sites"]
        .as_array()
        .unwrap();
    assert_eq!(sites.len(), 1, "{v}");
    assert_eq!(sites[0]["operation"], "f", "{v}");
    assert_eq!(sites[0]["location"]["column"], 45, "{v}");
    let out = f.run(&["effects", "main::apply"]);
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("Unresolved call `f` at src/main.wi:1:45")
    );
}
