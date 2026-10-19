//! Focused body-based effects regression for willow-25da.29.44.
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{QueryRequest, QuerySession, Snapshot},
    diagnostics::HumanEmitter,
};

fn snapshot(source: &str, label: &str) -> Snapshot {
    let directory = std::env::temp_dir().join(format!(
        "willow-closure-effects-{}-{label}",
        std::process::id()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    std::fs::write(&path, source).unwrap();
    let result = CompilerSession::new(path.to_str().unwrap(), "", &CompilerOptions::debug(), None)
        .analysis_with_emitter(&mut HumanEmitter)
        .unwrap();
    std::fs::remove_dir_all(directory).unwrap();
    result
}

fn effects(snapshot: Snapshot, name: &str) -> serde_json::Value {
    let function = snapshot
        .functions
        .iter()
        .find(|f| f.name == name)
        .unwrap()
        .id
        .clone();
    let revision = snapshot.revision.clone();
    QuerySession::new(snapshot)
        .unwrap()
        .query(QueryRequest::Effects { revision, function })["result"]
        .take()
}

#[test]
fn closure_effects_comparator_and_route_are_known() {
    let source = "fn before(f: closure(i64, i64) -> bool) -> bool { return f(1, 2); }\nclass Dijkstra { pub fn route(self) -> bool { let offset = 1; return before(|a: i64, b: i64| a + offset < b); } }\nfn main() { let d = new Dijkstra(); println(d.route()); }";
    let snapshot = snapshot(source, "comparator");
    for name in ["before", "Dijkstra::route"] {
        let result = effects(snapshot.clone(), name);
        assert_eq!(result["status"], "ok", "{name}: {result}");
        assert_eq!(
            result["runtime_effects"].as_u64().unwrap() & 6,
            0,
            "{result}"
        );
    }
}

#[test]
fn closure_effects_blocking_body_is_preserved() {
    let result = effects(
        snapshot(include_str!("../example/closure_effects.wi"), "blocking"),
        "apply",
    );
    assert_eq!(result["status"], "ok", "{result}");
    assert_ne!(
        result["runtime_effects"].as_u64().unwrap() & 2,
        0,
        "{result}"
    );
}

#[test]
fn closure_effects_body_inference_perspectives() {
    // Each case checks actual compiler capture, graph inference and evidence.
    let cases = [
        ("noncapturing", "let f = |x: i64| x + 1; f(2);", "", false),
        (
            "capturing",
            "let k = 2; let f = |x: i64| x + k; f(2);",
            "",
            false,
        ),
        (
            "named",
            "let f = inc; f(2);",
            "fn inc(x: i64) -> i64 { return x + 1; }",
            false,
        ),
        (
            "alias",
            "let k = 2; let f = |x: i64| x + k; let g = f; g(2);",
            "",
            false,
        ),
        (
            "parameter",
            "apply(|x: i64| x + 1);",
            "fn apply(f: fn(i64) -> i64) { f(1); }",
            false,
        ),
        (
            "closure_parameter",
            "let k = 2; apply(|x: i64| x + k);",
            "fn apply(f: closure(i64) -> i64) { f(1); }",
            false,
        ),
        (
            "nested",
            "let k = 2; let f = |x: i64| { let g = |y: i64| y + k; return g(x); }; f(1);",
            "",
            false,
        ),
        (
            "block_body",
            "let k = 2; let f = |x: i64| { return x + k; }; f(1);",
            "",
            false,
        ),
        (
            "generic",
            "apply(1, |x: i64| x + 1);",
            "fn apply<T>(x: T, f: closure(T) -> T) -> T { return f(x); }",
            false,
        ),
        (
            "return_value",
            "let f = factory(); f(1);",
            "fn factory() -> closure(i64) -> i64 { let k = 2; return |x: i64| x + k; }",
            false,
        ),
        (
            "field",
            "let k = 2; let h = new Holder(|x: i64| x + k); h.run();",
            "class Holder { pub f: closure(i64) -> i64; pub init(self, f: closure(i64) -> i64) { self.f = f; } pub fn run(self) -> i64 { let callback = self.f; return callback(1); } }",
            false,
        ),
        (
            "conditional",
            "let k = 2; let a = |x: i64| x + k; let b = |x: i64| x - k; let f = true ? a : b; f(1);",
            "",
            false,
        ),
        (
            "repeated",
            "let f = |x: i64| x + 1; f(1); f(2); f(3);",
            "",
            false,
        ),
        ("zero_arity", "let k = 2; let f = || k; f();", "", false),
        (
            "two_arities",
            "let f = |x: i64| x; let g = |x: i64, y: i64| x < y; f(1); g(1, 2);",
            "",
            false,
        ),
        (
            "blocking",
            "let cell = BlockingCell::new(false); let f = || cell.get(); f();",
            "",
            true,
        ),
        (
            "blocking_helper",
            "let cell = BlockingCell::new(false); let f = || read(cell); f();",
            "fn read(c: BlockingCell<bool>) -> bool { return c.get(); }",
            true,
        ),
        (
            "blocking_parameter",
            "let cell = BlockingCell::new(false); apply(|| cell.get());",
            "fn apply(f: closure() -> bool) -> bool { return f(); }",
            true,
        ),
        (
            "reassignment",
            "let cell = BlockingCell::new(false); let mut f: closure() -> bool = || false; f = || cell.get(); f();",
            "",
            true,
        ),
        (
            "mixed_candidates",
            "let cell = BlockingCell::new(false); let f = || cell.get(); let g = || true; g(); f();",
            "",
            true,
        ),
    ];
    for (label, body, declarations, blocking) in cases {
        let source = format!("{declarations}\nfn main() {{ {body} }}");
        let result = effects(snapshot(&source, label), "main");
        assert_eq!(result["status"], "ok", "{label}: {result}");
        let bits = result["runtime_effects"].as_u64().unwrap();
        assert_eq!(bits & 4, 0, "{label}: {result}");
        assert_eq!(bits & 2 != 0, blocking, "{label}: {result}");
    }
}

#[test]
fn closure_effects_missing_candidates_remain_unknown() {
    let result = effects(
        snapshot(
            "fn apply(f: closure() -> bool) -> bool { return f(); } fn main() {}",
            "missing",
        ),
        "apply",
    );
    assert_eq!(result["status"], "unknown", "{result}");
    assert_eq!(result["runtime_effects"], 63);
}

#[test]
fn closure_effects_imported_heap_and_dijkstra_route() {
    let directory =
        std::env::temp_dir().join(format!("willow-closure-route-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let main = directory.join("main.wi");
    std::fs::write(
        &main,
        "import net::Dijkstra; fn main() { let d = new Dijkstra(); println(d.route()); }",
    )
    .unwrap();
    std::fs::write(directory.join("heap.wi"), "module heap; pub class Heap<T> { pub compare: closure(T, T) -> bool; pub init(self, compare: closure(T, T) -> bool) { self.compare = compare; } pub fn before(self, a: T, b: T) -> bool { let f = self.compare; return f(a, b); } }").unwrap();
    std::fs::write(directory.join("net.wi"), "module net; import heap::Heap; pub class Dijkstra { pub fn route(self) -> bool { let offset = 1; let h = new Heap<i64>(|a: i64, b: i64| a + offset < b); return h.before(1, 3); } }").unwrap();
    let snapshot =
        CompilerSession::new(main.to_str().unwrap(), "", &CompilerOptions::debug(), None)
            .analysis_with_emitter(&mut HumanEmitter)
            .unwrap();
    for name in ["Heap::before", "Dijkstra::route"] {
        let result = effects(snapshot.clone(), name);
        assert_eq!(result["status"], "ok", "{name}: {result}");
        assert_eq!(
            result["runtime_effects"].as_u64().unwrap() & 6,
            0,
            "{name}: {result}"
        );
    }
    let mut direct = willow_compiler::ai::direct::DirectSession::new(snapshot).unwrap();
    for selector in ["heap::Heap::before", "net::Dijkstra::route"] {
        let result = direct
            .query("effects", selector, &Default::default(), true)
            .unwrap();
        assert_eq!(result["status"], "ok", "{selector}: {result}");
        assert_eq!(
            result["runtime_effects"].as_u64().unwrap() & 6,
            0,
            "{result}"
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn closure_effects_shared_candidates_scale_linearly() {
    for n in [8, 32, 128] {
        let mut source = String::new();
        for i in 0..n {
            source.push_str(&format!("fn value_{i}(x: i64) -> i64 {{ return x; }}\nfn caller_{i}(f: fn(i64) -> i64) -> i64 {{ return f(1); }}\n"));
        }
        source.push_str("fn main() {");
        for i in 0..n {
            source.push_str(&format!("caller_{i}(value_{i});"));
        }
        source.push('}');
        let snapshot = snapshot(&source, &format!("scale-{n}"));
        let buckets: Vec<_> = snapshot
            .functions
            .iter()
            .filter(|f| f.id.starts_with("$indirect-arity:"))
            .collect();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].callees.len(), n);
        let uses = snapshot
            .functions
            .iter()
            .flat_map(|f| &f.callees)
            .filter(|id| *id == &buckets[0].id)
            .count();
        assert_eq!(uses, n);
        let result = effects(snapshot, "main");
        assert_eq!(result["status"], "ok", "{result}");
        println!("indirect scale n={n} shared_buckets=1 candidate_edges={n} caller_edges={uses}");
    }
}

#[test]
fn closure_effects_chains_cycles_and_serialization() {
    for n in [8, 32, 128] {
        let mut source = "fn value(x: i64) -> i64 { return x; }\nfn invoke(f: fn(i64) -> i64, n: i64) -> i64 { if n > 0 { return invoke(f, n - 1); } return f(n); }\n".to_owned();
        for i in 0..n {
            let call = if i == 0 {
                "invoke(value, 2)".into()
            } else {
                format!("chain_{}()", i - 1)
            };
            source.push_str(&format!("fn chain_{i}() -> i64 {{ return {call}; }}\n"));
        }
        source.push_str(&format!("fn main() {{ chain_{}(); }}", n - 1));
        let snapshot = snapshot(&source, &format!("chain-{n}"));
        let restored =
            serde_json::from_slice::<Snapshot>(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let first = effects(snapshot, "main");
        assert_eq!(first["status"], "ok", "{first}");
        assert_eq!(first["runtime_effects"].as_u64().unwrap() & 6, 0, "{first}");
        assert_eq!(first, effects(restored, "main"));
        println!("indirect chain depth={n} recursive_body=1 serialization=stable");
    }
}

#[test]
fn closure_effects_distinct_arities_do_not_share_blocking_bounds() {
    let source = "fn compare(f: closure(i64, i64) -> bool) -> bool { return f(1, 2); } fn main() { let c = BlockingCell::new(false); let read = || c.get(); let k = 1; compare(|a: i64, b: i64| a + k < b); read(); }";
    let snapshot = snapshot(source, "separate");
    let compare = effects(snapshot.clone(), "compare");
    assert_eq!(compare["status"], "ok", "{compare}");
    assert_eq!(
        compare["runtime_effects"].as_u64().unwrap() & 6,
        0,
        "{compare}"
    );
    let main = effects(snapshot, "main");
    assert_eq!(main["status"], "ok", "{main}");
    assert_ne!(main["runtime_effects"].as_u64().unwrap() & 2, 0, "{main}");
}

#[test]
fn closure_effects_opaque_value_does_not_disappear_beside_known_candidate() {
    // Addressable builtin has no captured Willow body: retain unknown even
    // when a checked lambda shares its arity.
    let source = "fn apply(f: fn(f64, f64) -> f64) -> f64 { return f(1.0, 2.0); } fn main() { apply(pow); apply(|a: f64, b: f64| a + b); }";
    let result = effects(snapshot(source, "opaque"), "apply");
    assert_eq!(result["status"], "unknown", "{result}");
    assert_eq!(result["runtime_effects"], 63);
}

#[test]
fn closure_effects_repeated_values_do_not_duplicate_edges() {
    for n in [16, 64, 256] {
        let source = format!(
            "fn value(x: i64) -> i64 {{ return x; }} fn apply(f: fn(i64) -> i64) -> i64 {{ return f(1); }} fn main() {{ {} }}",
            "apply(value);".repeat(n)
        );
        let snapshot = snapshot(&source, &format!("repeat-{n}"));
        let bucket = snapshot
            .functions
            .iter()
            .find(|f| f.id.starts_with("$indirect-arity:1:"))
            .unwrap();
        assert_eq!(bucket.callees.len(), 1);
        let uses = snapshot
            .functions
            .iter()
            .flat_map(|f| &f.callees)
            .filter(|id| *id == &bucket.id)
            .count();
        assert_eq!(uses, 1);
        assert_eq!(effects(snapshot, "main")["status"], "ok");
        println!("indirect repeated_values={n} candidate_edges=1 caller_edges=1");
    }
}

#[test]
fn closure_effects_known_method_and_indirect_call_compose() {
    let source = "class Heap { pub fn count(self) -> i64 { return 2; } pub fn before(self, compare: closure(i64, i64) -> bool) -> bool { return compare(1, self.count()); } } fn main() { let heap = new Heap(); let k = 0; heap.before(|a: i64, b: i64| a + k < b); }";
    let result = effects(snapshot(source, "method-and-indirect"), "Heap::before");
    assert_eq!(result["status"], "ok", "{result}");
    assert_eq!(
        result["runtime_effects"].as_u64().unwrap() & 6,
        0,
        "{result}"
    );
}

#[test]
fn closure_effects_unresolved_virtual_call_is_not_hidden_by_known_lambda() {
    let source = "interface Run { fn run(self) -> i64; } fn apply(f: closure() -> i64, r: Run) -> i64 { return f() + r.run(); } fn main() { let k = 1; let f = || k; f(); }";
    let result = effects(snapshot(source, "unresolved-virtual"), "apply");
    assert_eq!(result["status"], "unknown", "{result}");
    assert_eq!(result["runtime_effects"], 63);
}

fn evidence_operation(result: &serde_json::Value, effect: u64) -> String {
    result["effect_evidence"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["effect"] == effect)
        .unwrap_or_else(|| panic!("{result}"))["witness"]["cause"]["operation"]
        .as_str()
        .unwrap_or_else(|| panic!("{result}"))
        .to_owned()
}

#[test]
fn closure_effects_cite_indirect_site_not_caller_lambda_body() {
    // willow-25da.29.53: caller-supplied lambda bodies keep their bounds but
    // are cited at the reaching indirect call; the lambda owner cites the body.
    let source = "fn apply<T>(x: T, f: closure(T) -> T) -> T { return f(x); }\nfn relay(f: closure(i64) -> i64) -> i64 { return apply(1, f); }\nfn main() { let c = BlockingCell::new(false); relay(|x: i64| { c.get(); return x; }); }";
    let snapshot = snapshot(source, "caller-lambda");
    for name in ["apply", "relay"] {
        let result = effects(snapshot.clone(), name);
        assert_ne!(
            result["runtime_effects"].as_u64().unwrap() & 2,
            0,
            "{result}"
        );
        assert_eq!(evidence_operation(&result, 2), "indirect-call", "{name}");
        let blocking = result["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["effect"] == 2)
            .unwrap();
        assert_eq!(blocking["status"], "conservative-bound", "{result}");
        assert_eq!(
            blocking["witness"]["cause"]["location"]["start"], 52,
            "{result}"
        );
    }
    let main = effects(snapshot, "main");
    assert_eq!(evidence_operation(&main, 2), "BlockingCell.get", "{main}");
}

#[test]
fn closure_effects_direct_callee_keeps_evidence_beside_foreign_lambda() {
    let source = "fn read(c: BlockingCell<bool>) -> bool { return c.get(); }\nfn apply(f: closure() -> bool, c: BlockingCell<bool>) -> bool { f(); return read(c); }\nfn own<T>(x: T, f: closure(T) -> T) -> T { let g = |y: T| f(y); return g(x); }\nfn main() { let cell = BlockingCell::new(false); apply(|| read(cell), cell); own(1, |x: i64| { cell.get(); return x; }); }";
    let snapshot = snapshot(source, "direct-and-foreign");
    let apply = effects(snapshot.clone(), "apply");
    assert_eq!(evidence_operation(&apply, 2), "BlockingCell.get", "{apply}");
    // A lambda nested in the queried generic body is its own code; the value
    // it forwards to is still caller-supplied.
    let own = effects(snapshot, "own");
    assert_ne!(own["runtime_effects"].as_u64().unwrap() & 2, 0, "{own}");
    assert_eq!(evidence_operation(&own, 2), "indirect-call", "{own}");
}

#[test]
fn closure_effects_foreign_candidates_visit_each_edge_once() {
    // Each foreign candidate is queued once per incoming edge and expanded
    // once; the indirect call site is looked up once per reaching function.
    for n in [8, 32, 128] {
        let mut source = "fn apply<T>(x: T, f: closure(T) -> T) -> T { return f(x); }\n".to_owned();
        for i in 0..n {
            source.push_str(&format!(
                "fn caller_{i}() -> String {{ return apply(\"{i}\", |x: String| x + \"!\"); }}\n"
            ));
        }
        source.push_str("fn main() {");
        for i in 0..n {
            source.push_str(&format!("caller_{i}();"));
        }
        source.push('}');
        let snapshot = snapshot(&source, &format!("foreign-scale-{n}"));
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "apply")
            .unwrap()
            .id
            .clone();
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        let result = session.query(QueryRequest::Effects { revision, function })["result"].take();
        assert_eq!(evidence_operation(&result, 1), "indirect-call", "{result}");
        let visits = session.effect_edge_visits;
        assert!(visits <= 2 * n + 2, "n={n} visits={visits}");
        println!("foreign candidates n={n} effect_edge_visits={visits}");
    }
}
