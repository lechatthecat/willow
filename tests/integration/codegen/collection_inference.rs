use super::*;

#[test]
fn map_inference_from_insert() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;
fn main() {
    let m = Map::new();
    println(m.len());
    m.insert("k", 3);
    println(m.get("k").unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n3\n");
}

#[test]
fn map_inference_preserves_types_and_shadowing() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;
fn main() {
    let m = Map::new();
    if true {
        let m = Map::new();
        m.insert(7, "inner");
        println(m.get(7).unwrap());
    }
    m.insert("outer", 42);
    println(m.get("outer").unwrap());
    let n = Map::new();
    n.insert(1, "value");
    println(n.get(1).unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "inner\n42\nvalue\n");
}

#[test]
fn map_inference_rejects_conflicting_insertions() {
    for insertion in ["m.insert(1, 2);", "m.insert(\"b\", \"bad\");"] {
        let source = format!(
            r#"import std::collections::Map;
fn main() {{ let m = Map::new(); m.insert("a", 1); {insertion} }}"#
        );
        assert_compile_error_contains(&source, &["error[E0201]", "map"]);
    }
}

#[test]
fn map_inference_requires_evidence_for_empty_map() {
    assert_compile_error_contains(
        "import std::collections::Map; fn main() { let m = Map::new(); println(m.len()); }",
        &["error[E0201]", "cannot infer map key and value types"],
    );
}

#[test]
fn map_inference_works_in_async_body() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;
async fn number() -> i64 { return 3; }
async fn main() {
    let m = Map::new();
    m.insert("k", await number());
    println(m.get("k").unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

#[test]
fn map_inference_reports_early_content_use() {
    assert_compile_error_contains(
        r#"import std::collections::Map;
fn main() { let m = Map::new(); m.get("k"); m.insert("k", 3); }"#,
        &["error[E0201]", "before key and value types are known"],
    );
}

#[test]
fn map_inference_insertion_in_nested_scope() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;
fn main() {
    let m = Map::new();
    if true { m.insert("k", 3); }
    println(m.get("k").unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

// willow-ssl7.36: empty literal arguments must retain their parameter type.
#[test]
fn contextual_empty_array_arguments() {
    let source = r#"
import std::collections::Array;
fn work(xs: Array<i64>, n: i64) -> i64 { return xs.len() + n; }
fn strings(xs: Array<String>) -> i64 { return xs.len(); }
fn make() -> Array<i64> { return []; }
fn main() {
    println(work([], 4096));
    let xs: Array<i64> = [];
    println(work(xs, 7));
    println(strings([]));
    println(work(true ? [] : [], 8));
    println(work(make(), 9));
}
"#;
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "contextual empty arrays must compile and run: {out}");
        assert_eq!(out, "4096\n7\n0\n8\n9\n");
    }
}

#[test]
fn contextual_empty_array_rejects_non_array_parameter() {
    assert_compile_error_contains(
        "fn work(x: i64) {} fn main() { work([]); }",
        &["error[E0201]"],
    );
}

#[test]
fn contextual_empty_array_allocation_sites_scale_linearly() {
    // The bundled `Array` methods own a fixed number of literal sites.
    let mut library_sites = None;
    for sites in [1, 8, 32, 128] {
        let mut source = String::from(
            "import std::collections::Array; \
             fn work(xs: Array<i64>) -> i64 { return xs.len(); } \
             fn batch(flag: bool) {",
        );
        for _ in 0..sites {
            source.push_str("println(work(flag ? [] : []));");
        }
        source.push_str("} fn main() { batch(true); }");
        let names = compile_and_collect_relocation_targets_all(&source, &[]);
        let allocations = names
            .iter()
            .filter(|name| *name == "willow_array_new_literal")
            .count();
        let library = *library_sites.get_or_insert(allocations - sites * 2);
        assert_eq!(allocations, sites * 2 + library);
        println!("sites={sites} literal_allocations={allocations}");
    }
}

#[test]
fn freeze_inference_native() {
    let source = include_str!("../../../example/freeze_inference.wi");
    for source in [
        source.to_string(),
        source.replace("fn main()", "async fn main()"),
    ] {
        for run in [compile_and_run, compile_and_run_release] {
            let (out, ok) = run(&source);
            assert!(ok, "{out}");
            assert_eq!(out, "0\n0\n0\n0\n1\n7\n42\n0\n0\n");
        }
    }
}

#[test]
fn freeze_inference_allocation_sites_scale_linearly() {
    let mut shared_array_sites = None;
    for n in [1, 8, 32, 128] {
        let mut source = String::from(
            "import std::collections::Array; import std::collections::Map; fn a(xs: FrozenArray<i64>) { println(xs.len()); } fn m(xs: FrozenMap<String, i64>) { println(xs.len()); } fn main() {",
        );
        source.push_str(&"a(Array::new().freeze()); m(Map::new().freeze());".repeat(n));
        source.push('}');
        let names = compile_and_collect_relocation_targets_all(&source, &[]);
        // Allocation/copy live in shared source specializations. Callsites grow
        // linearly, but neither source body is duplicated per consumer call.
        let shared = ["willow_array_new_literal", "willow_array_copy"]
            .map(|symbol| names.iter().filter(|name| name.as_str() == symbol).count());
        assert!(shared.iter().all(|count| *count > 0));
        assert_eq!(*shared_array_sites.get_or_insert(shared), shared);
        for method in [".new", ".freeze"] {
            let count = names
                .iter()
                .filter(|name| name.contains("Array") && name.ends_with(method))
                .count();
            assert_eq!(count, n, "Array{method}/{n}");
        }
        for symbol in ["willow_map_new", "willow_map_copy"] {
            let count = names
                .iter()
                .filter(|name| {
                    *name == symbol
                        || (symbol == "willow_map_new" && *name == "willow_map_new_local")
                })
                .count();
            assert_eq!(count, n, "{symbol}/{n}");
        }
        println!(
            "freeze sites={n} shared_array_allocations={} shared_array_copies={} source_new_calls={n} source_freeze_calls={n}",
            shared[0], shared[1]
        );
    }
}

#[test]
fn empty_array_binding_task_class_string_annotations() {
    let source = r#"
import std::collections::Array;
class Item { pub value: i64; }
async fn worker() -> i64 { return 7; }
async fn main() {
    let mut tasks: Array<Task<i64>> = [];
    tasks.push(worker());
    for task in tasks { println(await task); }
    let mut items: Array<Item> = [];
    items.push(new Item(9));
    for item in items { println(item.value); }
    let mut strings: Array<String> = [];
    strings.push("hello");
    for s in strings { println(s); }
}
"#;
    for run in [compile_and_run, compile_and_run_release] {
        let (out, ok) = run(source);
        assert!(ok, "{out}");
        assert_eq!(out, "7\n9\nhello\n");
    }
}

// willow-8hq4.33: the maximum inline buffer and the first old-generation
// buffer must preserve contents/roots in both optimization modes.
#[test]
fn array_literal_tlab_size_boundary_preserves_elements() {
    for length in [0, 1, 3, 509, 510, 1024] {
        let elements = std::iter::repeat_n("17", length)
            .collect::<Vec<_>>()
            .join(",");
        let source = format!(
            "import std::collections::Array; fn make() -> Array<i64> {{ return [{elements}]; }} \
             fn main() {{ let a=make(); gc_minor_collect(); println(a.len()); {} }}",
            if length == 0 {
                String::new()
            } else {
                format!("println(a[{}]);", length - 1)
            }
        );
        for run in [compile_and_run, compile_and_run_release] {
            let (out, ok) = run(&source);
            assert!(ok, "length={length}: {out}");
            let expected = if length == 0 {
                "0\n".to_string()
            } else {
                format!("{length}\n17\n")
            };
            assert_eq!(out, expected);
        }
    }
}
