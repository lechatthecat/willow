use super::*;

// ── FrozenArray<T> (willow-dgwo.7) ───────────────────────────────────────────
// Perspectives: 1 freeze+len; 2 indexing read; 3 independent copy (original
// mutation does not leak); 4 push rejected; 5 pop rejected; 6 index-assign
// rejected; 7 unknown method rejected; 8 freeze takes no args; 9 FrozenArray<i64>
// is Sync (passable to async under the check); 10 FrozenArray<Array<i64>> is not
// Sync (rejected); 11 FrozenArray<String> ok; 12 low worker values still check.
#[test]
fn test_frozen_array_freeze_len_index_and_independent_copy() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
fn main() {
    let xs: Array<i64> = [10, 20, 30];
    let fa = xs.freeze();
    println(fa.len());   // 3
    println(fa[0]);      // 10
    println(fa[2]);      // 30
    xs.push(40);
    println(fa.len());   // 3 (independent of the original)
    println(xs.len());   // 4
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n10\n30\n3\n4\n");
}

#[test]
fn test_frozen_array_push_rejected() {
    assert_compile_error_contains(
        "import std::collections::Array;\nfn main() { let f = [1, 2].freeze(); f.push(3); }\n",
        &["error[E0201]", "immutable"],
    );
}

#[test]
fn test_frozen_array_pop_rejected() {
    assert_compile_error_contains(
        "import std::collections::Array;\nfn main() { let f = [1, 2].freeze(); f.pop(); }\n",
        &["error[E0201]", "immutable"],
    );
}

#[test]
fn test_frozen_array_index_assign_rejected() {
    assert_compile_error_contains(
        "import std::collections::Array;\nfn main() { let f = [1, 2].freeze(); f[0] = 9; }\n",
        &["error[E0201]"],
    );
}

#[test]
fn test_frozen_array_unknown_method_rejected() {
    assert_compile_error_contains(
        "import std::collections::Array;\nfn main() { let f = [1, 2].freeze(); f.frob(); }\n",
        &["error[E0201]", "no method `frob`"],
    );
}

#[test]
fn test_frozen_array_freeze_takes_no_args() {
    assert_compile_error_contains(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; let f = xs.freeze(7); }\n",
        &["error[E0201]"],
    );
}

#[test]
fn test_frozen_array_is_sync_passable_to_async() {
    // FrozenArray<i64> is Sync, so it is accepted by the data-race check.
    let (ok, stderr) = compile_with_data_race_check(
        r#"
import std::collections::Array;
async fn t(fa: FrozenArray<i64>) -> i64 { await sleep(1); return fa.len(); }
async fn main() { let fa = [1, 2, 3].freeze(); println(await t(fa)); }
"#,
    );
    assert!(ok, "FrozenArray<i64> should be Sync: {stderr}");
}

#[test]
fn test_frozen_array_string_is_sync() {
    let (ok, _) = compile_with_data_race_check(
        r#"
import std::collections::Array;
async fn t(fa: FrozenArray<String>) -> i64 { await sleep(1); return fa.len(); }
async fn main() { let fa: Array<String> = ["a", "b"]; println(await t(fa.freeze())); }
"#,
    );
    assert!(ok);
}

#[test]
fn test_frozen_array_of_array_not_sync_rejected() {
    // FrozenArray<Array<i64>> follows its element: inner Array is not Sync.
    let (ok, _) = compile_with_data_race_check(
        r#"
import std::collections::Array;
async fn t(fa: FrozenArray<Array<i64>>) -> i64 { await sleep(1); return fa.len(); }
async fn main() {
    let inner: Array<i64> = [1];
    let outer: Array<Array<i64>> = [inner];
    println(await t(outer.freeze()));
}
"#,
    );
    assert!(!ok, "FrozenArray<Array<i64>> is not Sync");
}

// ── FrozenMap<K,V> (willow-dgwo.10) ──────────────────────────────────────────
// Perspectives: 1 freeze+len; 2 contains; 3 get->Option<V>; 4 independent copy;
// 5 insert rejected; 6 remove rejected; 7 unknown method rejected; 8 freeze no
// args; 9 FrozenMap<String,i64> is Sync (passable to async under the check).
#[test]
fn test_frozen_map_reads_and_independent_copy() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Map;
fn main() {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    m.insert("b", 2);
    let fm = m.freeze();
    println(fm.len());           // 2
    println(fm.contains("a"));   // true
    println(fm.contains("z"));   // false
    println(match fm.get("b") { Option::Some(v) => v, Option::None => -1 });  // 2
    m.insert("c", 3);
    println(m.len());            // 3
    println(fm.len());           // 2 (independent)
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\ntrue\nfalse\n2\n3\n2\n");
}

#[test]
fn test_frozen_map_insert_rejected() {
    assert_compile_error_contains(
        "import std::collections::Map;\nfn main() { let f = Map<String, i64>::new().freeze(); f.insert(\"x\", 1); }\n",
        &["error[E0201]", "immutable"],
    );
}

#[test]
fn test_frozen_map_unknown_method_rejected() {
    assert_compile_error_contains(
        "import std::collections::Map;\nfn main() { let f = Map<String, i64>::new().freeze(); f.frob(); }\n",
        &["error[E0201]", "no method `frob`"],
    );
}

#[test]
fn test_frozen_map_freeze_takes_no_args() {
    assert_compile_error_contains(
        "import std::collections::Map;\nfn main() { let m: Map<String, i64> = Map::new(); let f = m.freeze(7); }\n",
        &["error[E0201]"],
    );
}

#[test]
fn test_frozen_map_is_sync_passable_to_async() {
    let (ok, stderr) = compile_with_data_race_check(
        r#"
import std::collections::Map;
async fn t(fm: FrozenMap<String, i64>) -> i64 { await sleep(1); return fm.len(); }
async fn main() {
    let m: Map<String, i64> = Map::new();
    m.insert("a", 1);
    println(await t(m.freeze()));
}
"#,
    );
    assert!(ok, "FrozenMap<String,i64> should be Sync: {stderr}");
}
