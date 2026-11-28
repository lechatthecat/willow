//! Focused body-based effects regression for closure body effects.
use willow_compiler::{
    CompilerOptions, CompilerSession,
    ai::{QueryRequest, QuerySession, Snapshot},
    diagnostics::HumanEmitter,
};

fn snapshot(source: &str, label: &str) -> Snapshot {
    let directory = std::env::temp_dir().join(format!(
        "willow-field-effects-{}-{label}",
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
fn function_field_effects_direct_generic_is_bounded() {
    let source = "class Bag<T> { pub before: fn(T, T) -> bool; pub fn direct(self, x: T, y: T) -> bool { return (self.before)(x, y); } } fn main() { let b = new Bag<i64>(|x: i64, y: i64| x < y); b.direct(1, 2); }";
    let result = effects(snapshot(source, "direct"), "Bag::direct");
    assert_eq!(result["bounds"]["runtime"], 0, "{result}");
    assert!(
        result["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["state"] == "none"),
        "{result}"
    );
}

#[test]
fn function_field_effects_exported_field_is_workspace_bounded() {
    let source = "pub class Bag { pub before: fn(i64, i64) -> bool; pub fn bound(self, x: i64, y: i64) -> bool { let f = self.before; return f(x, y); } } fn main() { let b = new Bag(|x: i64, y: i64| x < y); b.bound(1, 2); }";
    let result = effects(snapshot(source, "exported"), "Bag::bound");
    assert_eq!(result["status"], "ok", "{result}");
    assert!(
        result["effects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["unknown_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["code"] == "indirect-call")),
        "{result}"
    );
}

fn assert_bounded(result: &serde_json::Value, label: &str, blocking: bool) {
    assert_eq!(result["status"], "ok", "{label}: {result}");
    let bits = result["bounds"]["runtime"].as_u64().unwrap();
    assert_eq!(bits & 2 != 0, blocking, "{label}: {result}");
    for effect in result["effects"].as_array().unwrap() {
        for reason in effect["unknown_reasons"].as_array().unwrap() {
            assert_eq!(reason["code"], "indirect-call", "{label}: {result}");
        }
    }
}

#[test]
fn function_field_effects_storage_perspectives() {
    let cases = [
        ("literal", "", "new Bag(|a: i64, b: i64| a < b)", "", false),
        (
            "capture",
            "",
            "new Bag(|a: i64, b: i64| a + offset < b)",
            "",
            false,
        ),
        ("named", "", "new Bag(pure)", "", false),
        (
            "constructor",
            "pub init(self, f: fn(i64, i64) -> bool) { self.before = f; }",
            "new Bag(pure)",
            "",
            false,
        ),
        (
            "alias_write",
            "",
            "new Bag(pure)",
            "let alias = bag; alias.before = slow;",
            true,
        ),
        (
            "multiple_objects",
            "",
            "new Bag(pure)",
            "let other = new Bag(slow);",
            true,
        ),
        (
            "conditional_store",
            "",
            "new Bag(offset > 0 ? pure : slow)",
            "",
            true,
        ),
        ("returned", "", "new Bag(factory())", "", false),
        ("shadowed", "", "new Bag(pure)", "let before = slow;", false),
        (
            "captured_blocking",
            "",
            "new Bag(|a: i64, b: i64| cell.get())",
            "",
            true,
        ),
        (
            "unrelated_value",
            "",
            "new Bag(pure)",
            "let unrelated = slow;",
            false,
        ),
    ];
    for direct in [false, true] {
        let body = if direct {
            "return (self.before)(x, y);"
        } else {
            "let f = self.before; return f(x, y);"
        };
        for (label, constructor, allocation, extra, blocking) in cases {
            let source = format!("fn pure(a: i64, b: i64) -> bool {{ return a < b; }}
fn slow(a: i64, b: i64) -> bool {{ let c = BlockingCell::new(false); return c.get(); }}
fn factory() -> fn(i64, i64) -> bool {{ return pure; }}
class Bag {{ pub before: fn(i64, i64) -> bool; {constructor} pub fn run(self, x: i64, y: i64) -> bool {{ {body} }} }}
fn main() {{ let offset = 1; let cell = BlockingCell::new(false); let bag = {allocation}; {extra} bag.run(1, 2); }}");
            let label = format!("{label}-{direct}");
            let result = effects(snapshot(&source, &label), "Bag::run");
            assert_bounded(&result, &label, blocking);
        }
    }
}

#[test]
fn function_field_effects_generic_heap_and_sort() {
    for direct in [false, true] {
        let body = if direct {
            "return (self.before)(a, b);"
        } else {
            "let f = self.before; return f(a, b);"
        };
        let source = format!("class Heap<T> {{ pub before: fn(T, T) -> bool; pub fn push(self, a: T, b: T) -> bool {{ {body} }} }}
fn heap_sort<T>(a: T, b: T, before: fn(T, T) -> bool) -> bool {{ let heap = new Heap<T>(before); return heap.push(a, b); }}
fn main() {{ heap_sort(1, 2, |a: i64, b: i64| a < b); heap_sort(false, true, |a: bool, b: bool| !a && b); }}");
        let snapshot = snapshot(&source, &format!("generic-heap-{direct}"));
        for name in ["Heap::push", "heap_sort"] {
            assert_bounded(&effects(snapshot.clone(), name), name, false);
        }
    }
}

#[test]
fn function_field_effects_unknown_names_source_callee() {
    for (body, name) in [
        (
            "let callback = self.before; return callback(1, 2);",
            "callback",
        ),
        ("return (self.before)(1, 2);", "self.before"),
    ] {
        let source = format!(
            "class Bag {{ pub before: fn(i64, i64) -> bool; pub fn run(self) -> bool {{ {body} }} }} fn main() {{}}"
        );
        let result = effects(snapshot(&source, name), "Bag::run");
        assert_eq!(result["status"], "unknown", "{result}");
        assert_eq!(result["bounds"]["runtime"], 127, "{result}");
        for effect in result["effects"].as_array().unwrap() {
            let reason = effect["unknown_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["code"] == "indirect-call")
                .unwrap();
            assert_eq!(reason["callee"], name, "{result}");
            let start = reason["location"]["start"].as_u64().unwrap() as usize;
            assert!(
                source[start..].starts_with(if name == "callback" {
                    "callback"
                } else {
                    "before"
                }),
                "{result}"
            );
        }
    }
}

#[test]
fn function_field_effects_cross_module_escape_and_generic_methods() {
    for direct in [false, true] {
        for blocking in [false, true] {
            let directory = std::env::temp_dir().join(format!(
                "willow-field-modules-{}-{direct}-{blocking}",
                std::process::id()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let call = if direct {
                "return (self.before)(a, b);"
            } else {
                "let compare = self.before; return compare(a, b);"
            };
            let api = format!("module heap;
pub class Heap<T> {{ pub before: fn(T, T) -> bool; pub fn push(self, a: T, b: T) -> bool {{ {call} }} }}
pub fn heap_sort<T>(heap: Heap<T>, a: T, b: T) -> bool {{ return heap.push(a, b); }}");
            std::fs::write(directory.join("heap.wi"), api).unwrap();
            let callback = if blocking {
                "|a: i64, b: i64| cell.get()"
            } else {
                "|a: i64, b: i64| a < b"
            };
            std::fs::write(directory.join("factory.wi"), format!("module factory; import heap::Heap; pub fn make() -> Heap<i64> {{ let cell = BlockingCell::new(false); return new Heap<i64>({callback}); }}")).unwrap();
            let main = directory.join("main.wi");
            std::fs::write(&main, "import heap; import factory; fn main() { let h = factory::make(); heap::heap_sort(h, 1, 2); }").unwrap();
            let snapshot =
                CompilerSession::new(main.to_str().unwrap(), "", &CompilerOptions::debug(), None)
                    .analysis_with_emitter(&mut HumanEmitter)
                    .unwrap();
            for name in ["Heap::push", "heap_sort"] {
                let result = effects(snapshot.clone(), name);
                assert_bounded(&result, name, blocking);
                if blocking {
                    let reason = result["effects"][1]["unknown_reasons"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|r| r["code"] == "indirect-call")
                        .unwrap();
                    assert_eq!(
                        reason["callee"],
                        if direct { "self.before" } else { "compare" },
                        "{result}"
                    );
                    assert!(
                        reason["location"]["path"]
                            .as_str()
                            .unwrap()
                            .ends_with("heap.wi"),
                        "{result}"
                    );
                }
            }
            // Include persistence: the new source callee survives snapshot IO.
            let encoded = serde_json::to_string(&snapshot).unwrap();
            let restored: Snapshot = serde_json::from_str(&encoded).unwrap();
            assert_bounded(&effects(restored, "heap_sort"), "restored", blocking);
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}

#[test]
fn function_field_effects_expression_lambda_hazards() {
    for (body, panic, blocking) in [
        ("a < b", false, false),
        ("a / b > 0", true, false),
        ("cell.get()", false, true),
    ] {
        let source = format!(
            "class Bag {{ pub before: fn(i64, i64) -> bool; pub fn run(self, x: i64, y: i64) -> bool {{ return (self.before)(x, y); }} }} fn main() {{ let cell = BlockingCell::new(false); let bag = new Bag(|a: i64, b: i64| {body}); bag.run(1, 2); }}"
        );
        let result = effects(
            snapshot(&source, &format!("hazard-{panic}-{blocking}")),
            "Bag::run",
        );
        assert_bounded(&result, body, blocking);
        assert_eq!(
            result["bounds"]["runtime"].as_u64().unwrap() & 32 != 0,
            panic,
            "{result}"
        );
    }
}

#[test]
fn function_field_effects_repeated_sites_share_candidates() {
    for count in [8, 32, 128] {
        let source = format!(
            "fn pure(a: i64, b: i64) -> bool {{ return a < b; }} class Bag {{ pub before: fn(i64, i64) -> bool; pub fn run(self) {{ {} }} }} fn main() {{ let bag = new Bag(pure); bag.run(); }}",
            "(self.before)(1, 2);".repeat(count)
        );
        let snapshot = snapshot(&source, &format!("scale-{count}"));
        let buckets: Vec<_> = snapshot
            .functions
            .iter()
            .filter(|f| f.id.starts_with("$indirect-arity:"))
            .collect();
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].callees.len(), 1);
        let sites = snapshot
            .semantic
            .effect_calls
            .values()
            .flatten()
            .filter(|s| s.indirect.is_some())
            .count();
        assert_eq!(sites, count);
        assert_bounded(&effects(snapshot, "Bag::run"), "repeated", false);
        println!("field sites={count} buckets=1 candidate_edges=1 stored_sites={sites}");
    }
}

#[test]
fn function_field_effects_explain_names_unknowns_and_reports_bounds() {
    for direct in [false, true] {
        for known in [false, true] {
            let directory = std::env::temp_dir().join(format!(
                "willow-field-cli-{}-{direct}-{known}",
                std::process::id()
            ));
            std::fs::create_dir_all(&directory).unwrap();
            let call = if direct {
                "return (self.before)(1, 2);"
            } else {
                "let compare = self.before; return compare(1, 2);"
            };
            let main = if known {
                "let bag = new Bag(|a: i64, b: i64| { let cell = BlockingCell::new(false); return cell.get(); }); bag.run();"
            } else {
                ""
            };
            let path = directory.join("main.wi");
            std::fs::write(&path, format!("class Bag {{ pub before: fn(i64, i64) -> bool; pub fn run(self) -> bool {{ {call} }} }} fn main() {{ {main} }}")).unwrap();
            let output = std::process::Command::new(env!("CARGO_BIN_EXE_willow"))
                .args([
                    "effects",
                    "Bag::run",
                    "--source",
                    path.to_str().unwrap(),
                    "--explain",
                ])
                .output()
                .unwrap();
            let text = String::from_utf8(output.stdout).unwrap();
            assert_eq!(
                output.status.success(),
                known,
                "{text} {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                text.starts_with(if known {
                    "effects Bag::run: ok"
                } else {
                    "effects Bag::run: unknown"
                }),
                "{text}"
            );
            assert!(
                text.contains(&format!(
                    "Unknown: indirect-call `{}` at main.wi:1:",
                    if direct { "self.before" } else { "compare" }
                )),
                "{text}"
            );
            assert!(!text.contains("unresolved-callee"), "{text}");
            assert!(!text.contains("$field_callable"), "{text}");
            if known {
                assert!(text.contains("conservative-bound"), "{text}");
            }
            std::fs::remove_dir_all(directory).unwrap();
        }
    }
}

#[test]
fn function_field_effects_unknown_site_wins_over_known_bound() {
    let source = "class Bag { pub known: fn() -> bool; pub unknown: fn(i64) -> i64; pub fn run(self) -> i64 { (self.known)(); return (self.unknown)(1); } } fn seed() { let c = BlockingCell::new(false); let f = || c.get(); f(); } fn main() {}";
    let result = effects(snapshot(source, "mixed-sites"), "Bag::run");
    assert_eq!(result["status"], "unknown", "{result}");
    for effect in result["effects"].as_array().unwrap() {
        let reason = effect["unknown_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["code"] == "indirect-call")
            .unwrap();
        assert_eq!(reason["callee"], "self.unknown", "{result}");
    }
}

#[test]
fn function_field_effects_known_site_stays_bounded_beside_missing_method() {
    let source = "interface Work { fn work(self); } class Bag { pub known: fn() -> bool; } fn run(b: Bag, w: Work) { (b.known)(); w.work(); } fn main() { let b = new Bag(|| true); }";
    let result = effects(snapshot(source, "mixed-dispatch"), "run");
    assert_eq!(result["status"], "unknown", "{result}");
    let causes = result["unknown_causes"].as_array().unwrap();
    assert!(!causes.is_empty(), "{result}");
    for cause in causes {
        if let Some(sites) = cause["call_sites"].as_array() {
            assert!(
                sites.iter().all(|site| site["operation"] != "b.known"),
                "{result}"
            );
        }
    }
}
