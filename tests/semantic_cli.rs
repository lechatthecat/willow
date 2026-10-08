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
        for import in [
            "import pairing;",
            "import pairing::Lottery; import pairing::Pairing;",
        ] {
            let f = Fixture::new();
            fs::write(f.0.join("src/pairing.wi"), format!("pub interface Pairing {{ fn pairings(self); }} pub class Lottery implements Pairing {{ pub channel: Channel<i64>; pub cell: BlockingCell<i64>; pub fn pairings(self) {{ {body} }} }} fn helper() {{ let n = 1; }}")).unwrap();
            let ty = if import == "import pairing;" {
                "pairing::Pairing"
            } else {
                "Pairing"
            };
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
    assert!(suggestions[0].as_str().unwrap().contains("serve::running@"));
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
