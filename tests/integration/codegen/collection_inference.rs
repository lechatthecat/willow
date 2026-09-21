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
            .filter(|name| *name == "willow_array_new")
            .count();
        assert_eq!(allocations, sites * 2);
        println!("sites={sites} literal_allocations={allocations}");
    }
}
