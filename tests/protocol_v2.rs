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
            "willow-query-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("main.wi"), source).unwrap();
        // Query paths must match the canonical paths recorded in snapshots.
        Self(fs::canonicalize(path).unwrap())
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
    fn save(&self, name: &str) -> Value {
        self.result(&["snapshot", "save", "main.wi", "--output", name]);
        // Decode like consumers do: workspace placeholders expand to paths.
        let mut value = serde_json::from_slice(&fs::read(self.0.join(name)).unwrap()).unwrap();
        willow_compiler::ai::expand_snapshot_paths(&mut value).unwrap();
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

fn risk(f: &Fixture, after: &str) -> Value {
    f.save("before.json");
    f.write("main.wi", after);
    f.save("after.json");
    f.result(&["risk", "--before", "before.json", "--after", "after.json"])["risk"].clone()
}
#[test]
fn retry_cfg_excludes_success_break_return_dead_code_and_single_execution_iterable() {
    let f = Fixture::new("fn main() {}");
    let r = risk(
        &f,
        "fn main() { let mut n = 0; while n < 3 { n = n + 1; if n == 2 { println(1); break; } println(2); } println(3); while false { println(4); } }",
    );
    let ops = r["operations"].as_array().unwrap();
    let states: Vec<_> = ops
        .iter()
        .filter(|v| v["operation"] == "io:print")
        .map(|v| {
            (
                v["location"]["start"].as_u64().unwrap(),
                v["retry_reachability"].as_str().unwrap(),
            )
        })
        .collect();
    let mut states = states;
    states.sort();
    assert_eq!(
        states.iter().map(|s| s.1).collect::<Vec<_>>(),
        vec![
            "no-structural-cycle",
            "structural-cycle",
            "no-structural-cycle",
            "no-structural-cycle"
        ]
    );
    assert_eq!(
        ops.iter()
            .filter(|v| v["operation"] == "io:print" && v["review_question"].is_string())
            .count(),
        1
    );
    assert!(ops.iter().all(|v| v["new_operation"] == true));
}
#[test]
fn nested_retry_and_cross_function_new_effect_have_compact_evidence() {
    let f = Fixture::new(
        "fn helper() {} fn main() { let mut n = 0; while n < 2 { n = n + 1; helper(); } }",
    );
    let r = risk(
        &f,
        "fn helper() { println(99); } fn main() { let mut n = 0; while n < 2 { n = n + 1; helper(); while n < 1 { println(7); break; } } }",
    );
    let prints: Vec<_> = r["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["operation"] == "io:print")
        .collect();
    assert_eq!(prints.len(), 2);
    assert!(
        prints
            .iter()
            .any(|v| v["retry_reachability"] == "conservative-call-path")
    );
    assert!(
        prints
            .iter()
            .any(|v| v["retry_reachability"] == "structural-cycle")
    );
    assert!(prints.iter().all(|v| v["review_question"].is_string()));
    let helper = r["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["operation"] == "call:helper")
        .unwrap();
    assert_eq!(helper["new_operation"], false);
}
#[test]
fn deferred_operations_and_indirect_calls_keep_structural_evidence() {
    let f = Fixture::new("fn main() {}");
    let r = risk(
        &f,
        "fn main() { let f = |x: i64| x + 1; let mut n = 0; while n < 2 { n = f(n); defer println(n); } }",
    );
    let call = r["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["operation"] == "call:f")
        .unwrap();
    assert_eq!(call["effect_certainty"], "unknown");
    assert_eq!(call["retry_reachability"], "structural-cycle");
}
#[test]
fn queries_share_revision_and_match_compiler_facts() {
    let source = "fn leaf() -> i64 { return 42; } fn main() { println(leaf()); }";
    let f = Fixture::new(source);
    let snapshot = f.save("snapshot.json");
    let leaf = named(&snapshot, "leaf");
    let requests = serde_json::json!([
        {"kind":"symbol-info","function":leaf["id"]},
        {"kind":"references","function":leaf["id"]},
        {"kind":"type-at","file":f.0.join("main.wi"),"byte":source.find("42").unwrap()},
        {"kind":"effects","function":named(&snapshot,"main")["id"]},
        {"kind":"symbol-info","function":leaf["id"],"revision":"old"},
        {"kind":"symbol-info","function":"absent"}
    ]);
    f.write("requests.json", &requests.to_string());
    let r = f.result(&["query", "main.wi", "--requests", "requests.json"]);
    assert_eq!(r["revision"], snapshot["revision"]);
    let results = &r["results"];
    assert_eq!(results[0]["result"]["symbol"], *leaf);
    assert_eq!(
        results[1]["result"]["references"].as_array().unwrap().len(),
        1
    );
    assert_eq!(results[2]["result"]["status"], "ok");
    assert_eq!(
        results[2]["result"]["type"],
        serde_json::to_value(willow_compiler::parser::ast::Type::<String>::I64).unwrap()
    );
    assert_eq!(
        results[3]["result"]["runtime_effects"],
        named(&snapshot, "main")["runtime_effects"]
    );
    assert!(results[3]["result"]["witness"].as_array().unwrap().len() >= 2);
    assert_eq!(results[4]["status"], "stale");
    assert_eq!(results[5]["result"]["status"], "unknown");
}

#[test]
fn timeout_branch_retries_but_success_branch_exits() {
    let f = Fixture::new("async fn main() {}");
    let r = risk(
        &f,
        r#"async fn main() {
        let channel = Channel<i64>::new();
        while true {
            select {
                let value = channel.recv() => { println(value); break; }
                sleep(1) => { println("timeout"); continue; }
            }
        }
    }"#,
    );
    let operations = r["operations"].as_array().unwrap();
    let timeout = operations
        .iter()
        .find(|v| v["operation"] == "wait:timeout")
        .unwrap();
    assert_eq!(timeout["retry_reachability"], "structural-cycle");
    let recv = operations
        .iter()
        .find(|v| v["operation"] == "channel:recv")
        .unwrap();
    assert_eq!(recv["retry_reachability"], "no-structural-cycle");
    let flow = timeout["evidence"]["flow"].as_u64().unwrap() as usize;
    let node = timeout["evidence"]["node"].as_u64().unwrap() as usize;
    assert!(r["evidence"][flow]["paths"]["root"][node].is_number());
}
#[test]
fn ambiguous_and_unanalyzed_type_positions_are_explicit() {
    let source = "interface I { fn run(self) -> i64 { return 1; } } class A implements I {} class B implements I {} fn main() {}";
    let f = Fixture::new(source);
    f.write(
        "requests.json",
        &serde_json::json!([
            {"kind":"type-at","file":f.0.join("main.wi"),"byte":source.find("1;").unwrap()},
            {"kind":"type-at","file":f.0.join("main.wi"),"byte":source.len()+1}
        ])
        .to_string(),
    );
    let r = f.result(&["query", "main.wi", "--requests", "requests.json"]);
    assert_eq!(r["results"][0]["result"]["status"], "ambiguous");
    assert_eq!(r["results"][1]["result"]["status"], "unknown");
}

#[test]
fn references_use_checked_bindings_and_imported_identities() {
    let f = Fixture::new(
        "import dep; fn leaf() -> i64 { return 1; } fn main() { let leaf = |x: i64| x; println(leaf(7)); println(dep::value()); }",
    );
    f.write("dep.wi", "pub fn value() -> i64 { return 42; }");
    let snapshot = f.save("snapshot.json");
    let local = named(&snapshot, "leaf");
    let imported = named(&snapshot, "value");
    f.write(
        "requests.json",
        &serde_json::json!([
            {"kind":"references","function":local["id"]},
            {"kind":"references","function":imported["id"]},
            {"kind":"type-at","file":"dep.wi","byte":29}
        ])
        .to_string(),
    );
    let r = f.result(&["query", "main.wi", "--requests", "requests.json"]);
    assert_eq!(
        r["results"][0]["result"]["references"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(r["results"][0]["result"]["status"], "ok");
    assert_eq!(
        r["results"][1]["result"]["references"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}
#[test]
fn writes_returns_and_for_iterable_preserve_control_flow_boundaries() {
    let f = Fixture::new("fn main() {}");
    let r = risk(
        &f,
        "import std::collections::Array; fn values() -> Array<i64> { return [1, 2]; } fn main() { let mut a = [0]; for n in values() { a[0] = n; if n == 1 { println(n); return; } } }",
    );
    let ops = r["operations"].as_array().unwrap();
    assert_eq!(
        ops.iter()
            .find(|v| v["operation"] == "call:values")
            .unwrap()["retry_reachability"],
        "no-structural-cycle"
    );
    assert_eq!(
        ops.iter()
            .find(|v| v["operation"] == "write:index")
            .unwrap()["retry_reachability"],
        "structural-cycle"
    );
    assert_eq!(
        ops.iter().find(|v| v["operation"] == "io:print").unwrap()["retry_reachability"],
        "no-structural-cycle"
    );
}

#[test]
fn short_circuit_and_panicking_paths_prune_impossible_repetition() {
    let f = Fixture::new("fn main() {}");
    let r = risk(
        &f,
        "fn effect() -> bool { println(1); return true; } fn main() { while true { if false && effect() { break; } panic(\"stop\"); } }",
    );
    let ops = r["operations"].as_array().unwrap();
    let call = ops
        .iter()
        .find(|v| v["operation"] == "call:effect")
        .unwrap();
    assert_eq!(call["retry_reachability"], "no-structural-cycle");
}

#[test]
fn retry_virtual_dispatch_includes_override_candidates() {
    let f = Fixture::new("fn main() {}");
    let r = risk(
        &f,
        "open class Base { pub open fn run(self) -> i64 { return 1; } } class Child extends Base { pub override fn run(self) -> i64 { println(7); return 2; } } fn call(x: Base) { let mut n = 0; while n < 2 { n = n + 1; x.run(); } } fn main() { call(new Child()); }",
    );
    let print = r["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["operation"] == "io:print")
        .unwrap();
    assert_eq!(print["retry_reachability"], "conservative-call-path");
    assert!(print["review_question"].is_string());
}

#[test]
fn lock_body_preserves_success_exit_and_repeating_path() {
    let f = Fixture::new("async fn main() {}");
    let r = risk(
        &f,
        r#"async fn main() {
        let m = Mutex::new(0);
        while true {
            lock m as value {
                if value == 1 { println(10); break; }
                println(20);
            }
        }
    }"#,
    );
    let mut prints: Vec<_> = r["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["operation"] == "io:print")
        .collect();
    prints.sort_by_key(|v| v["location"]["start"].as_u64().unwrap());
    assert_eq!(prints.len(), 2);
    assert_eq!(prints[0]["retry_reachability"], "no-structural-cycle");
    assert_eq!(prints[1]["retry_reachability"], "structural-cycle");
}

#[test]
fn new_pure_calls_are_not_new_side_effects() {
    let f = Fixture::new("fn pure() -> i64 { return 1; } fn main() {}");
    let r = risk(
        &f,
        "fn pure() -> i64 { return 1; } fn main() { for i in 0..3 { pure(); println(i); } }",
    );
    let ops = r["operations"].as_array().unwrap();
    let pure = ops.iter().find(|v| v["operation"] == "call:pure").unwrap();
    assert_eq!(pure["new_operation"], true);
    assert_eq!(pure["new_side_effect"], "proven-absent");
    assert!(pure["review_question"].is_null());
    let print = ops.iter().find(|v| v["operation"] == "io:print").unwrap();
    assert_eq!(print["new_side_effect"], "proven-operation");
}

#[test]
fn side_effect_absence_propagates_but_unknown_and_effectful_calls_stay_candidates() {
    let f = Fixture::new("fn main() {}");
    let r = risk(
        &f,
        "fn leaf() -> i64 { return 1; } fn pure() -> i64 { return leaf(); } fn effect() { println(1); } fn main() { while true { pure(); effect(); let f = |x: i64| x; f(1); } }",
    );
    let ops = r["operations"].as_array().unwrap();
    for name in ["call:leaf", "call:pure"] {
        let op = ops.iter().find(|v| v["operation"] == name).unwrap();
        assert_eq!(op["new_side_effect"], "proven-absent");
    }
    for name in ["call:effect", "call:f"] {
        let op = ops.iter().find(|v| v["operation"] == name).unwrap();
        assert_eq!(op["new_side_effect"], "conservative-candidate");
        assert!(op["review_question"].is_string());
    }
}

#[test]
fn reference_parameter_writes_do_not_prove_side_effect_absence() {
    let f = Fixture::new("fn main() {}");
    let r = risk(
        &f,
        "fn write(x: &mut i64) { x = 2; } fn main() { let mut x = 0; while true { write(&x); } }",
    );
    let op = r["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["operation"] == "call:write")
        .unwrap();
    assert_eq!(op["new_side_effect"], "conservative-candidate");
    assert!(op["review_question"].is_string());
}

#[test]
fn declarations_and_references_use_checker_binding_identity() {
    let source = "fn take(p: &mut i64) { p = p + 1; } fn main() { let mut x = 1; take(&x); if true { let x = 2; println(x); } println(x); }";
    let f = Fixture::new(source);
    let s = f.save("symbols.json");
    let symbols = s["semantic"]["symbols"].as_array().unwrap();
    let mut xs: Vec<_> = symbols.iter().filter(|s| s["name"] == "x").collect();
    xs.sort_by_key(|s| s["location"]["start"].as_u64().unwrap());
    assert_eq!(xs.len(), 2, "{symbols:?}");
    let parameter = symbols.iter().find(|s| s["name"] == "p").unwrap();
    assert_eq!(parameter["kind"], "parameter");
    f.write(
        "requests.json",
        &serde_json::json!([
            {"kind":"symbols"},
            {"kind":"symbol-info","function":xs[0]["id"]},
            {"kind":"references","function":xs[0]["id"]},
            {"kind":"references","function":xs[1]["id"]},
            {"kind":"references","function":parameter["id"]},
            {"kind":"symbol-at","file":"main.wi","byte":source.rfind("x)").unwrap()}
        ])
        .to_string(),
    );
    let r = f.result(&["query", "main.wi", "--requests", "requests.json"]);
    let r = &r["results"];
    assert_eq!(r[1]["result"]["symbol"]["id"], xs[0]["id"]);
    assert_eq!(
        r[2]["result"]["references"].as_array().unwrap().len(),
        2,
        "{r}"
    );
    assert_eq!(
        r[3]["result"]["references"].as_array().unwrap().len(),
        1,
        "{r}"
    );
    assert_eq!(
        r[4]["result"]["references"].as_array().unwrap().len(),
        2,
        "{r}"
    );
    assert_eq!(r[5]["result"]["symbol"]["id"], xs[0]["id"]);
}

#[test]
fn field_type_variant_and_import_references_have_source_identity() {
    let f = Fixture::new(
        r#"import model::Box as B;
        fn use_box(p: B) -> i64 { return p.value; }
        fn main() { let b = new B(1); println(use_box(b)); }
    "#,
    );
    f.write(
        "model.wi",
        r#"pub class Box { pub value: i64; }
        pub enum Choice<T> { Empty, Value(T) }
        fn sample(x: Choice<i64>) -> i64 {
            let y: Choice<i64> = Choice::Value(1);
            return match y { Choice::Empty => 0, Choice::Value(v) => v };
        }
    "#,
    );
    let s = f.save("symbols.json");
    let symbols = s["semantic"]["symbols"].as_array().unwrap();
    let refs = s["semantic"]["references"].as_array().unwrap();
    for (name, kind) in [
        ("B", "import"),
        ("value", "field"),
        ("Box", "class"),
        ("Choice", "enum"),
        ("T", "type-parameter"),
        ("Value", "variant"),
        ("Empty", "variant"),
    ] {
        let symbol = symbols
            .iter()
            .find(|s| s["name"] == name && s["kind"] == kind)
            .unwrap_or_else(|| panic!("missing {name} {kind}: {symbols:?}"));
        assert!(symbol["location"].is_object(), "{symbol}");
        assert!(
            refs.iter().any(|r| r["target"] == symbol["id"]),
            "no references to {symbol}: {refs:?}"
        );
    }
}

#[test]
fn dynamic_references_are_target_specific_and_keep_possible_dispatch() {
    let f = Fixture::new(
        "open class Base { pub open fn run(self) -> i64 { return 0; } } class Child extends Base { pub override fn run(self) -> i64 { return 1; } } fn leaf() -> bool { return true; } fn invoke(x: Base) { x.run(); } fn main() { let indirect = |v: i64| v; indirect(1); invoke(new Child()); leaf(); }",
    );
    let s = f.save("symbols.json");
    let child = named(&s, "Child::run");
    let leaf = named(&s, "leaf");
    f.write(
        "requests.json",
        &serde_json::json!([
            {"kind":"references","function":child["id"]},
            {"kind":"references","function":leaf["id"]}
        ])
        .to_string(),
    );
    let r = f.result(&["query", "main.wi", "--requests", "requests.json"]);
    assert_eq!(r["results"][0]["result"]["status"], "ok");
    assert!(
        r["results"][0]["result"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["certainty"] == "possible-dispatch")
    );
    assert_eq!(r["results"][1]["result"]["status"], "ok");
    assert_eq!(
        r["results"][1]["result"]["references"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn defer_cleanup_runs_at_scope_exit_with_registration_operands_separate() {
    for (source, expected) in [
        (
            "fn main() { while true { defer println(1); break; } }",
            "no-structural-cycle",
        ),
        (
            "fn main() { let mut n=0; while n<3 { defer println(n); n=n+1; } }",
            "structural-cycle",
        ),
        (
            "fn main() { defer println(1); let mut n=0; while n<3 { n=n+1; } }",
            "no-structural-cycle",
        ),
        (
            "fn main() { while true { defer { defer println(1); } break; } }",
            "no-structural-cycle",
        ),
    ] {
        let f = Fixture::new("fn main() {}");
        let r = risk(&f, source);
        let prints: Vec<_> = r["operations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|v| v["operation"] == "io:print")
            .collect();
        assert_eq!(prints.len(), 1, "{r}");
        assert_eq!(prints[0]["retry_reachability"], expected, "{source}");
        assert!(
            r["evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["flow"]["complete"] == true)
        );
    }
}

#[test]
fn match_and_expression_exits_preserve_success_and_retry_paths() {
    let f = Fixture::new("fn main() {}");
    let r = risk(
        &f,
        "fn consume(x: i64) {} fn main() { let mut n=0; while n<3 { n=n+1; consume(match n { 1 => { println(1); return; } _ => 2 }); println(2); } }",
    );
    let mut prints: Vec<_> = r["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["operation"] == "io:print")
        .collect();
    prints.sort_by_key(|p| p["location"]["start"].as_u64().unwrap());
    assert_eq!(prints[0]["retry_reachability"], "no-structural-cycle");
    assert_eq!(prints[1]["retry_reachability"], "structural-cycle");
}

#[test]
fn effects_have_per_bit_roots_and_distinguish_unknown_evidence() {
    let f = Fixture::new(
        "fn danger(x: i64) -> i64 { return 1/x; } fn relay(x:i64)->i64 { return danger(x); } fn opaque(f: fn(i64)->i64)->i64 { return f(1); } fn main() { println(relay(1)); }",
    );
    let s = f.save("effects.json");
    f.write(
        "requests.json",
        &serde_json::json!([
            {"kind":"effects","function":named(&s,"relay")["id"]},
            {"kind":"effects","function":named(&s,"main")["id"]},
            {"kind":"effects","function":named(&s,"opaque")["id"]}
        ])
        .to_string(),
    );
    let result = f.result(&["query", "main.wi", "--requests", "requests.json"]);
    let values = &result["results"];
    assert_eq!(values[0]["result"]["status"], "ok");
    assert!(
        values[0]["result"]["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["status"] == "compiler-fact"
                && e["witness"]["kind"] == "panic"
                && e["witness"]["cause"]["location"].is_object())
    );
    for i in [0, 1] {
        let result = &values[i]["result"];
        let bits = result["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .fold(0, |bits, e| bits | e["effect"].as_u64().unwrap());
        assert_eq!(bits, result["runtime_effects"].as_u64().unwrap());
    }
    assert_eq!(values[2]["result"]["status"], "unknown");
    assert!(
        values[2]["result"]["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["status"] == "missing-witness")
    );
}

#[test]
fn risk_distinguishes_loop_timeout_retry_and_unreachable_effects() {
    let f = Fixture::new("fn main() {}");
    let r = risk(
        &f,
        "fn main() { let mut n=0; while n<3 { n=n+1; println(1); } if false { println(2); } }",
    );
    let mut prints: Vec<_> = r["operations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["operation"] == "io:print")
        .collect();
    prints.sort_by_key(|p| p["location"]["start"].as_u64().unwrap());
    assert_eq!(prints[0]["retry_detection"], "repetition-only");
    assert_eq!(prints[1]["execution_reachability"], "proven-unreachable");
    assert_eq!(prints[1]["new_side_effect"], "proven-absent");
    let f = Fixture::new("async fn main() {}");
    let r = risk(
        &f,
        "async fn main() { while true { select { sleep(1) => { println(1); continue; } } } }",
    );
    assert!(
        r["operations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["operation"] == "io:print"
                && o["retry_detection"] == "timeout-retry-candidate")
    );
}

#[test]
fn callable_references_include_function_values_and_import_targets() {
    let f = Fixture::new(
        "import dep::leaf as imported; fn main() { let callback = imported; println(callback(1)); }",
    );
    f.write("dep.wi", "pub fn leaf(x: i64) -> i64 { return x; }");
    let snapshot = f.save("snapshot.json");
    f.write(
        "requests.json",
        &serde_json::json!([
            {"kind":"references", "function":named(&snapshot,"leaf")["id"]}
        ])
        .to_string(),
    );
    let result = f.result(&["query", "main.wi", "--requests", "requests.json"]);
    let refs = result["results"][0]["result"]["references"]
        .as_array()
        .unwrap();
    for role in ["value", "import-target"] {
        assert!(
            refs.iter().any(|r| r["role"] == role),
            "missing {role}: {result}"
        );
    }
}

#[test]
fn nested_select_and_defer_operands_preserve_evaluation_order() {
    let f = Fixture::new("async fn main() {}");
    let r = risk(
        &f,
        r#"fn argument() -> i64 { println(7); return 1; }
        async fn main() { while true {
            defer println(argument());
            select { sleep(1) => { select { sleep(2) => { println(9); break; } } } }
        } }"#,
    );
    assert!(
        r["evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["flow"]["complete"] == true)
    );
    assert!(
        r["operations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|o| o["retry_reachability"] == "no-structural-cycle")
    );
    let f = Fixture::new("fn main() {}");
    let r = risk(
        &f,
        "fn argument() -> i64 { return 1; } fn main() { while true { defer println(argument()); continue; } }",
    );
    for operation in ["call:argument", "io:print"] {
        assert!(
            r["operations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|o| o["operation"] == operation
                    && o["retry_reachability"] == "structural-cycle")
        );
    }
}

#[test]
fn binding_forms_and_inherited_field_writes_keep_exact_reference_tokens() {
    let source = r#"open class Base { pub value: i64; }
        class Child extends Base {}
        fn use_bindings() {
            let child = new Child(1);
            child.value = child.value + 1;
            let callback = |argument: i64| argument + 1;
            for index in 0..2 { println(callback(index)); }
        }
        async fn main() {
            let mutex = Mutex::new(0);
            lock mutex as mut held { held = held + 1; }
            let channel = Channel<i64>::new();
            select { let received = channel.recv() => { println(received); } sleep(1) => {} }
        }"#;
    let f = Fixture::new(source);
    let s = f.save("symbols.json");
    let symbols = s["semantic"]["symbols"].as_array().unwrap();
    let refs = s["semantic"]["references"].as_array().unwrap();
    for (name, count) in [
        ("value", 2),
        ("argument", 1),
        ("index", 1),
        ("held", 2),
        ("received", 1),
    ] {
        let symbol = symbols
            .iter()
            .find(|s| s["name"] == name)
            .unwrap_or_else(|| panic!("missing {name}"));
        let references: Vec<_> = refs
            .iter()
            .filter(|r| r["target"] == symbol["id"])
            .collect();
        assert_eq!(references.len(), count, "{name}: {references:?}");
        for reference in references {
            let location = &reference["location"];
            assert_eq!(
                &source[location["start"].as_u64().unwrap() as usize
                    ..location["end"].as_u64().unwrap() as usize],
                name
            );
        }
    }
}

#[test]
fn module_and_static_assignment_references_keep_declared_targets() {
    let source =
        "import dep as model; fn main() { model::Counter::count = model::Counter::count + 1; }";
    let f = Fixture::new(source);
    f.write(
        "dep.wi",
        "module dep; pub class Counter { pub static mut count: i64 = 0; }",
    );
    let snapshot = f.save("snapshot.json");
    let symbols = snapshot["semantic"]["symbols"].as_array().unwrap();
    let references = snapshot["semantic"]["references"].as_array().unwrap();
    for (name, kind, count) in [
        ("dep", "module", 1),
        ("count", "static-field", 2),
        ("Counter", "class", 2),
    ] {
        let symbol = symbols
            .iter()
            .find(|s| s["name"] == name && s["kind"] == kind)
            .unwrap();
        assert_eq!(
            references
                .iter()
                .filter(|r| r["target"] == symbol["id"])
                .count(),
            count,
            "{name}: {references:?}"
        );
    }
}

#[test]
fn declared_types_use_normalized_import_identity() {
    let f =
        Fixture::new("import dep::Color as Shade; class Box { pub value: Shade; } fn main() {}");
    f.write("dep.wi", "module dep; pub enum Color { Red }");
    let snapshot = f.save("snapshot.json");
    let field = snapshot["semantic"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["kind"] == "field")
        .unwrap();
    assert_eq!(
        field["ty"],
        serde_json::to_value(willow_compiler::parser::ast::Type::<String>::Named(
            "dep::Color".into()
        ))
        .unwrap()
    );
}

#[test]
fn existing_operations_compare_reachability_and_repetition() {
    for (before, after, changed) in [
        (
            "while false { println(1); }",
            "while true { println(1); }",
            true,
        ),
        ("println(1);", "while true { println(1); }", true),
        (
            "while true { println(1); }",
            "while true { println(1); }",
            false,
        ),
        (
            "while true { println(1); }",
            "while false { println(1); }",
            false,
        ),
        ("while false { leaf(); }", "while true { leaf(); }", true),
        ("leaf();", "while true { leaf(); }", true),
        (
            "while false { println(1); } println(1);",
            "println(1); while false { println(1); }",
            false,
        ),
    ] {
        let f = Fixture::new(&format!(
            "fn leaf() {{ println(1); }} fn main() {{ {before} }}"
        ));
        let r = risk(
            &f,
            &format!("fn leaf() {{ println(1); }} fn main() {{ {after} }}"),
        );
        let ops = r["operations"].as_array().unwrap();
        let op = ops
            .iter()
            .find(|op| {
                op["operation"]
                    == if after.contains("leaf") {
                        "call:leaf"
                    } else {
                        "io:print"
                    }
                    && op["location"]["start"].as_u64().unwrap()
                        >= "fn leaf() { println(1); } fn main() { ".len() as u64
            })
            .unwrap();
        assert_eq!(op["new_operation"], false, "{before} -> {after}: {r}");
        assert_eq!(
            op["review_question"].is_string(),
            changed,
            "{before} -> {after}: {r}"
        );
        if changed {
            assert_ne!(op["new_side_effect"], "not-new");
        }
        if after.contains("leaf") && changed {
            let leaf = ops.iter().find(|op| op["operation"] == "io:print").unwrap();
            assert!(leaf["review_question"].is_string(), "{r}");
        }
    }
}

#[test]
fn assignment_lhs_type_falls_back_to_resolved_symbol() {
    let source = "fn main() { let mut x = 0; x = x + 1; }";
    let f = Fixture::new(source);
    let byte = source.find("x = x").unwrap();
    f.write(
        "requests.json",
        &serde_json::json!([
            {"kind":"symbol-at","file":"main.wi","byte":byte},
            {"kind":"type-at","file":"main.wi","byte":byte}
        ])
        .to_string(),
    );
    let r = f.result(&["query", "main.wi", "--requests", "requests.json"]);
    assert_eq!(r["results"][1]["result"]["status"], "ok", "{r}");
    assert_eq!(
        r["results"][1]["result"]["type"],
        serde_json::json!(["I64"]),
        "{r}"
    );
}

#[test]
fn same_named_enum_variant_uses_its_own_token() {
    for source in [
        "enum Foo { Foo, Other } fn main() { let x = Foo::Foo; }",
        "enum Foo { Foo(i64), Other } fn main() { let Foo = 1; let x = Foo::Foo(Foo); }",
        "enum Foo<T> { Foo(T), Other } fn main() { let Foo = 1; let x = Foo<i64>::Foo(Foo); }",
        "enum Foo { Foo, Other } fn main() { let x = Foo::Foo; match x { Foo::Foo => {}, Foo::Other => {} }; }",
        "enum Foo { Foo(i64), Other } fn main() { let x = Foo::Foo(1); match x { Foo(v) => {}, Other => {} }; }",
    ] {
        let f = Fixture::new(source);
        let s = f.save("snapshot.json");
        let variant = s["semantic"]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["kind"] == "variant" && v["name"] == "Foo")
            .unwrap();
        let refs: Vec<_> = s["semantic"]["references"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["target"] == variant["id"])
            .collect();
        let expected: Vec<_> = source
            .match_indices("::Foo")
            .map(|(i, _)| i + 2)
            .chain(source.find("Foo(v)"))
            .collect();
        let actual: Vec<_> = refs
            .iter()
            .map(|r| r["location"]["start"].as_u64().unwrap() as usize)
            .collect();
        assert_eq!(actual, expected, "{source}");
    }
}
#[test]
fn method_references_are_unique_and_prelude_contracts_are_not_functions() {
    let source = "open class Level { pub price: i64; pub open fn front(self) -> i64 { return self.price; } }\n\
                  class Deep extends Level { pub override fn front(self) -> i64 { return 0; } }\n\
                  fn show(x: i64) -> String { return \"v=${x}\"; }\n\
                  fn main() { let l = new Level(3); println(l.front()); println(show(l.front())); }";
    let f = Fixture::new(source);
    let snapshot = f.save("snapshot.json");
    let names: Vec<_> = snapshot["functions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(!names.iter().any(|n| n.starts_with("Into::")), "{names:?}");
    let front = named(&snapshot, "Level::front");
    let requests = serde_json::json!([{"kind":"references","function":front["id"]}]);
    f.write("requests.json", &requests.to_string());
    let r = f.result(&["query", "main.wi", "--requests", "requests.json"]);
    let refs = r["results"][0]["result"]["references"].as_array().unwrap();
    let mut starts: Vec<_> = refs
        .iter()
        .map(|r| r["location"]["start"].as_u64().unwrap() as usize)
        .collect();
    starts.sort();
    let calls: Vec<_> = source.match_indices("front()").map(|(i, _)| i).collect();
    assert_eq!(starts, calls, "{refs:?}");
    assert!(
        refs.iter().all(|r| r["certainty"] == "resolved"),
        "{refs:?}"
    );
}

#[test]
fn symbol_filters_readable_types_and_compact_snapshot_paths() {
    let f = Fixture::new("");
    fs::remove_file(f.0.join("main.wi")).unwrap();
    f.write(
        "project.toml",
        "[willow]\nmanifest-version = 1\n\n[project]\nname = \"app\"\nversion = \"0.1.0\"\nentry = \"src/main.wi\"\n\n[dependencies]\n",
    );
    fs::create_dir(f.0.join("src")).unwrap();
    f.write(
        "src/shapes.wi",
        "import std::collections::Array;\npub class Box { pub side: i64; }\npub fn wrap(n: i64) -> Box { return new Box(n); }\npub fn sides(xs: Array<Box>, f: fn(Box, i64) -> i64) -> i64 { return f(xs[0], 1); }\n",
    );
    let main = "import shapes;\nimport shapes::Box;\nfn area(b: Box, k: i64) -> i64 { return b.side * k; }\nfn main() { let b = shapes::wrap(2); println(area(b, 3)); }\n";
    f.write("src/main.wi", main);
    let names = |result: &Value| -> Vec<String> {
        result["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_owned())
            .collect()
    };
    let requests = serde_json::json!([
        {"kind":"symbols","symbol_kind":"function"},
        {"kind":"symbols","module":"shapes","prefix":"s","limit":1},
        {"kind":"symbols","name":"wrap"},
        {"kind":"symbols","name":"absent"},
        {"kind":"symbols","module":"main","symbol_kind":"class"},
        {"kind":"symbols","limit":0},
        {"kind":"symbols"},
        {"kind":"type-at","file":f.0.join("src/main.wi"),"byte":main.find("wrap(2)").unwrap()}
    ]);
    f.write("requests.json", &requests.to_string());
    let r = f.result(&["query", ".", "--requests", "requests.json"]);
    let results: Vec<&Value> = r["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| &v["result"])
        .collect();
    // Kind filter keeps only functions; every function carries a readable type
    // whose names are canonical module paths, local and item-imported alike.
    let mut functions = names(results[0]);
    functions.sort();
    assert_eq!(functions, ["area", "main", "sides", "wrap"]);
    assert_eq!(results[0]["total"], 4);
    assert_eq!(results[0]["truncated"], false);
    let display = |name: &str| {
        results[0]["symbols"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == name)
            .unwrap()["type_display"]
            .clone()
    };
    assert_eq!(display("area"), "fn(shapes::Box, i64) -> i64");
    assert_eq!(display("wrap"), "fn(i64) -> shapes::Box");
    assert_eq!(
        display("sides"),
        "fn(Array<shapes::Box>, fn(shapes::Box, i64) -> i64) -> i64"
    );
    assert_eq!(display("main"), "fn() -> void");
    // The exact compiler tree stays beside the readable spelling.
    assert!(results[0]["symbols"][0]["ty"].is_array());
    // Filters combine; limit reports the cut.
    assert_eq!(names(results[1]).len(), 1);
    assert_eq!(results[1]["total"], 2);
    assert_eq!(results[1]["truncated"], true);
    assert_eq!(names(results[2]), ["wrap"]);
    assert_eq!(results[2]["symbols"][0]["identity"]["module"], "shapes");
    assert_eq!(names(results[3]).len(), 0);
    assert_eq!(results[3]["total"], 0);
    assert_eq!(names(results[4]).len(), 0);
    assert_eq!(results[5]["truncated"], true);
    assert_eq!(names(results[5]).len(), 0);
    // No filter returns everything, including imports and the class.
    let all = names(results[6]);
    assert_eq!(results[6]["total"], all.len());
    assert!(all.contains(&"Box".to_owned()));
    assert_eq!(results[7]["status"], "ok");
    assert_eq!(results[7]["type_display"], "shapes::Box");

    // Saved snapshots write the workspace once; ids read from the file still
    // resolve in queries against the live project.
    f.result(&["snapshot", "save", ".", "--output", "snapshot.json"]);
    let text = fs::read_to_string(f.0.join("snapshot.json")).unwrap();
    let snapshot: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(snapshot["path_encoding"], "workspace-placeholder-v1");
    assert_eq!(snapshot["workspace"], f.0.to_str().unwrap());
    assert!(text.contains("${workspace}/src/shapes.wi"));
    let wrap = named(&snapshot, "wrap");
    let symbol = snapshot["semantic"]["symbols"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "Box" && s["kind"] == "class")
        .unwrap();
    let requests = serde_json::json!([
        {"kind":"symbol-info","function":wrap["id"]},
        {"kind":"symbol-info","function":symbol["id"]}
    ]);
    f.write("requests.json", &requests.to_string());
    let r = f.result(&["query", ".", "--requests", "requests.json"]);
    assert_eq!(r["revision"], snapshot["revision"]);
    assert_eq!(r["results"][0]["result"]["symbol"]["name"], "wrap");
    assert_eq!(r["results"][1]["result"]["status"], "ok");
    assert_eq!(
        r["results"][1]["result"]["symbol"]["type_display"],
        "shapes::Box"
    );
    // The compacted file round-trips through snapshot diff.
    f.result(&[
        "snapshot",
        "diff",
        "--before",
        "snapshot.json",
        "--after",
        "snapshot.json",
    ]);
}
