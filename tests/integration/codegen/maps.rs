use super::*;

// ── Map<K,V> type (willow-5t6) ─────────────────────────────────────────────
// GC-managed hash map: Map::new(), .insert(k,v), .get(k) -> Option<V>,
// .contains(k) -> bool, .len() -> i64. Keys: String (by content) or i64.

// Perspective 1: insert/get/len with String keys.
#[test]
fn test_map_string_key_insert_get_len() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut ages: Map<String, i64> = Map::new();
    ages.insert("Alice", 30);
    ages.insert("Bob", 25);
    println(ages.len());
    println(match ages.get("Alice") { Option::Some(a) => a, Option::None => -1, });
    println(match ages.get("Bob") { Option::Some(a) => a, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n30\n25\n");
}

// Perspective 2: a missing key returns None.
#[test]
fn test_map_get_missing_returns_none() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    println(match m.get("zzz") { Option::Some(v) => v, Option::None => -99, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "-99\n");
}

// Perspective 3: insert overwrites an existing key.
#[test]
fn test_map_insert_overwrites() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("k", 1);
    m.insert("k", 2);
    println(m.len());
    println(match m.get("k") { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

// Perspective 4: contains reports presence/absence.
#[test]
fn test_map_contains() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("x", 1);
    println(m.contains("x"));
    println(m.contains("y"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

// Perspective 5: i64 keys.
#[test]
fn test_map_i64_keys() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<i64, i64> = Map::new();
    m.insert(10, 100);
    m.insert(20, 200);
    println(match m.get(20) { Option::Some(v) => v, Option::None => -1, });
    println(match m.get(30) { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "200\n-1\n");
}

// Perspective 6: String values (GC references).
#[test]
fn test_map_string_values() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<i64, String> = Map::new();
    m.insert(1, "one");
    m.insert(2, "two");
    println(match m.get(2) { Option::Some(s) => s, Option::None => "none", });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "two\n");
}

// Perspective 7: empty map has length 0.
#[test]
fn test_map_empty_len_zero() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// Perspective 8: a map passed as a function parameter.
#[test]
fn test_map_as_parameter() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn get_or(m: Map<String, i64>, k: String, d: i64) -> i64 {
    return match m.get(k) { Option::Some(v) => v, Option::None => d, };
}
fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("a", 7);
    println(get_or(m, "a", -1));
    println(get_or(m, "b", -1));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n-1\n");
}

// Perspective 9: a map returned from a function.
#[test]
fn test_map_returned_from_function() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn build() -> Map<String, i64> {
    let mut m: Map<String, i64> = Map::new();
    m.insert("v", 99);
    return m;
}
fn main() {
    let m = build();
    println(match m.get("v") { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// Perspective 10: String keys compare by content, not identity.
#[test]
fn test_map_string_keys_by_content() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn key() -> String { return "dynamic"; }
fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("dynamic", 5);
    // A value produced separately but equal in content must hit.
    println(match m.get(key()) { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// Perspective 11: len grows with distinct keys.
#[test]
fn test_map_len_distinct_keys() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<i64, i64> = Map::new();
    m.insert(1, 1);
    m.insert(2, 2);
    m.insert(3, 3);
    println(m.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

// Perspective 12: a get result bound to a variable, then matched.
#[test]
fn test_map_get_result_in_variable() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("k", 42);
    let r = m.get("k");
    println(match r { Option::Some(v) => v, Option::None => -1, });
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 13: reference values survive a GC collection while the map lives.
#[test]
fn test_map_string_values_survive_gc() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<i64, String> = Map::new();
    m.insert(1, "alpha");
    m.insert(2, "beta");
    gc_collect();
    println(match m.get(1) { Option::Some(s) => s, Option::None => "gone", });
    println(match m.get(2) { Option::Some(s) => s, Option::None => "gone", });
}
"#,
    );
    assert!(ok, "map string values must survive GC");
    assert_eq!(out, "alpha\nbeta\n");
}

// Perspective 14: a get value used in arithmetic.
#[test]
fn test_map_value_in_arithmetic() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("n", 21);
    let v = match m.get("n") { Option::Some(x) => x, Option::None => 0, };
    println(v * 2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// Perspective 15: a wrong key type is a compile error.
#[test]
fn test_map_wrong_key_type_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert(1, 2);
}
"#,
        &["error[E0201]", "map key type mismatch"],
    );
}

// Perspective 16: a wrong value type is a compile error.
#[test]
fn test_map_wrong_value_type_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let mut m: Map<String, i64> = Map::new();
    m.insert("a", true);
}
"#,
        &["error[E0201]", "map value type mismatch"],
    );
}

// Perspective 17: an unknown method is a compile error.
#[test]
fn test_map_unknown_method_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    m.clear();
}
"#,
        &["error[E0201]", "no method `clear` on `Map<"],
    );
}

// Perspective 18: get with the wrong argument count is a compile error.
#[test]
fn test_map_get_wrong_arity_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new();
    let r = m.get();
}
"#,
        &["error[E0201]", "`Map::get` expects 1 argument"],
    );
}

// Perspective 19: Map::new with arguments is a compile error.
#[test]
fn test_map_new_with_args_is_error() {
    assert_compile_error_contains(
        r#"
import std::collections::Map;

fn main() {
    let m: Map<String, i64> = Map::new(5);
}
"#,
        &["error[E0201]", "`Map::new` takes no arguments"],
    );
}

// Perspective 20: two independent maps do not share state.
#[test]
fn test_map_independent_instances() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;

fn main() {
    let mut a: Map<String, i64> = Map::new();
    let mut b: Map<String, i64> = Map::new();
    a.insert("k", 1);
    b.insert("k", 2);
    println(match a.get("k") { Option::Some(v) => v, Option::None => -1, });
    println(match b.get("k") { Option::Some(v) => v, Option::None => -1, });
    println(b.contains("k"));
    println(a.len());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\ntrue\n1\n");
}
