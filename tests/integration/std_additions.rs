//! `std::set`, `std::sorted_map`, `std::strings`, `std::bytes`, `std::path`
//! and `env::var`: behavior in every build mode, failures, compile-time rules,
//! the runnable examples and operation-count complexity measurements.

use super::support::*;

/// Runs `source` in debug, release and allocation-stress GC builds and
/// returns the output, which must be identical in all three.
fn run_everywhere(source: &str) -> String {
    let (debug, ok) = compile_and_run(source);
    assert!(ok, "debug run failed:\n{debug}");
    for (mode, run) in [
        (
            "release",
            compile_and_run_release as fn(&str) -> (String, bool),
        ),
        ("gc stress", compile_and_run_gc_stress),
    ] {
        let (out, ok) = run(source);
        assert!(ok, "{mode} run failed:\n{out}");
        assert_eq!(out, debug, "{mode} output differs from debug");
    }
    debug
}

fn expect_lines(out: &str, expected: &[&str]) {
    assert_eq!(out.lines().collect::<Vec<_>>(), expected, "{out}");
}

#[test]
fn hash_set_membership_algebra_and_removal() {
    let out = run_everywhere(
        r#"
import std::set::HashSet;
import std::collections::{Array, Map};

fn main() {
    let s: HashSet<i64> = new HashSet();
    println(s.is_empty());
    println(s.to_string());
    println(s.insert(3));
    println(s.insert(3));
    println(s.insert(-7));
    println(s.contains(-7));
    println(s.contains(4));
    println(s.remove(4));
    println(s.remove(3));
    println(s.len());

    // Every scalar element type, with 0.0 and -0.0 as one element.
    let f: HashSet<f64> = new HashSet();
    f.insert(0.0);
    println(f.insert(-0.0));
    f.insert(2.5);
    println(f.to_string());
    let b: HashSet<bool> = new HashSet();
    b.insert(false);
    b.insert(true);
    b.insert(false);
    println(b.to_string());
    // NaN is never an element: lookups and removals miss without panicking.
    println(f.contains(0.0 / 0.0));
    println(f.remove(0.0 / 0.0));
    println(f.len());
    let w: HashSet<String> = HashSet<String>::from_array(["日本", "", "a", "日本"]);
    println(w.len());
    println(w.contains(""));
    println(w.to_string());

    // Swap-removal against a reference Map under a deterministic workload.
    let model: Map<i64, bool> = Map::new();
    let set: HashSet<i64> = new HashSet();
    let mut seed = 12345;
    let mut mismatches = 0;
    for step in 0..3000 {
        seed = (seed * 1103515245 + 12345) % 2147483648;
        let key = seed % 97;
        if step % 3 == 0 {
            let present = model.remove(key).is_some();
            if set.remove(key) != present { mismatches += 1; }
        } else {
            let fresh = !model.contains(key);
            model.insert(key, true);
            if set.insert(key) != fresh { mismatches += 1; }
        }
        if set.len() != model.len() { mismatches += 1; }
    }
    for key in 0..97 {
        if set.contains(key) != model.contains(key) { mismatches += 1; }
    }
    let values = set.values();
    values.sort();
    if values.to_string() != model.keys().to_string() { mismatches += 1; }
    println(mismatches);

    // values() and copy() are independent snapshots.
    let snapshot = set.values();
    let copy = set.copy();
    set.clear();
    println(set.len());
    println(snapshot.len() == copy.len());
    println(copy.contains(snapshot[0]));
    set.insert(1);
    println(set.to_string());

    let a: HashSet<String> = HashSet<String>::from_array(["x", "y", "z"]);
    let c: HashSet<String> = HashSet<String>::from_array(["y", "w"]);
    println(a.union(c).to_string());
    println(a.intersection(c).to_string());
    println(a.difference(c).to_string());
    println(c.difference(a).to_string());
    println(a.intersection(c).is_subset(c));
    println(c.is_subset(a));
    let empty: HashSet<String> = new HashSet();
    println(empty.is_subset(a));
    println(a.union(empty).len());
}
"#,
    );
    expect_lines(
        &out,
        &[
            "true",
            "{}",
            "true",
            "false",
            "true",
            "true",
            "false",
            "false",
            "true",
            "1",
            "false",
            "{0, 2.5}",
            "{false, true}",
            "false",
            "false",
            "2",
            "3",
            "true",
            "{, a, 日本}",
            "0",
            "0",
            "true",
            "true",
            "{1}",
            "{w, x, y, z}",
            "{y}",
            "{x, z}",
            "{w}",
            "true",
            "false",
            "true",
            "3",
        ],
    );
}

#[test]
fn sorted_map_order_queries_and_model_check() {
    let out = run_everywhere(
        r#"
import std::sorted_map::SortedMap;
import std::collections::{Array, Map};

fn key(value: Option<i64>) -> String {
    match value { Some(v) => { return v.to_string(); } None => { return "none"; } }
}
fn text(value: Option<String>) -> String {
    match value { Some(v) => { return v; } None => { return "none"; } }
}

fn main() {
    let m: SortedMap<i64, String> = new SortedMap();
    println(m.is_empty());
    println(key(m.first_key()));
    println(key(m.floor_key(5)));
    println(m.keys().len());
    for k in [50, 20, 80, 10, 30, 70, 90, 60] { m.insert(k, "v" + k.to_string()); }
    m.insert(30, "thirty");
    println(m.len());
    println(m.keys().to_string());
    println(text(m.get(30)));
    println(text(m.get(31)));
    println(m.contains(90));
    println(key(m.first_key()) + " " + key(m.last_key()));
    println(key(m.floor_key(55)) + " " + key(m.ceiling_key(55)));
    println(key(m.floor_key(50)) + " " + key(m.ceiling_key(50)));
    println(key(m.floor_key(5)) + " " + key(m.ceiling_key(95)));
    println(m.range_keys(20, 70).to_string());
    println(m.range_keys(21, 22).to_string());
    println(m.range_keys(70, 20).to_string());
    println(m.range_keys(-100, 100).len());
    // Root, internal two-child node and leaf removals.
    println(text(m.remove(50)));
    println(text(m.remove(20)));
    println(text(m.remove(90)));
    println(text(m.remove(90)));
    println(m.keys().to_string());
    println(m.values().to_string());
    let entries = m.entries();
    let (k0, v0) = entries[0];
    println(k0.to_string() + " " + v0);

    // Negative and fractional f64 keys, String keys in byte order.
    let f: SortedMap<f64, bool> = new SortedMap();
    for k in [2.5, -1.0, 0.0, -0.5, 1e10] { f.insert(k, k > 0.0); }
    println(f.keys().to_string());
    // NaN never matches a stored key: lookups miss, removal leaves the map
    // unchanged, floor/ceiling are None and a NaN range bound is empty.
    let nan = 0.0 / 0.0;
    println(f.get(nan).is_none());
    println(f.contains(nan));
    println(f.remove(nan).is_none());
    println(f.len());
    println(f.keys().to_string());
    println(f.floor_key(nan).is_none() && f.ceiling_key(nan).is_none());
    println(f.range_keys(nan, 100.0).len() + f.range_keys(-100.0, nan).len());
    println(f.range_keys(-100.0, 100.0).len());
    let s: SortedMap<String, i64> = new SortedMap();
    for k in ["b", "a", "B", "", "ab", "日本"] { s.insert(k, k.len()); }
    println(s.keys().to_string());

    // A deterministic insert/remove/lookup workload against Map.
    let model: Map<i64, i64> = Map::new();
    let tree: SortedMap<i64, i64> = new SortedMap();
    let mut seed = 7;
    let mut mismatches = 0;
    for step in 0..4000 {
        seed = (seed * 1103515245 + 12345) % 2147483648;
        let k = seed % 500 - 250;
        if step % 4 == 0 {
            let expected = model.remove(k);
            let actual = tree.remove(k);
            if expected.is_some() != actual.is_some() { mismatches += 1; }
        } else {
            model.insert(k, step);
            tree.insert(k, step);
        }
        if tree.len() != model.len() { mismatches += 1; }
    }
    if tree.keys().to_string() != model.keys().to_string() { mismatches += 1; }
    if tree.values().to_string() != model.values().to_string() { mismatches += 1; }
    for k in -260..260 {
        match tree.get(k) {
            Some(v) => { if model.get(k).unwrap() != v { mismatches += 1; } }
            None => { if model.contains(k) { mismatches += 1; } }
        }
    }
    println(mismatches);
    tree.clear();
    println(tree.len());
    tree.insert(1, 1);
    println(tree.keys().to_string());
}
"#,
    );
    expect_lines(
        &out,
        &[
            "true",
            "none",
            "none",
            "0",
            "8",
            "[10, 20, 30, 50, 60, 70, 80, 90]",
            "thirty",
            "none",
            "true",
            "10 90",
            "50 60",
            "50 50",
            "none none",
            "[20, 30, 50, 60]",
            "[]",
            "[]",
            "8",
            "v50",
            "v20",
            "v90",
            "none",
            "[10, 30, 60, 70, 80]",
            "[\"v10\", \"thirty\", \"v60\", \"v70\", \"v80\"]",
            "10 v10",
            "[-1, -0.5, 0, 2.5, 10000000000]",
            "true",
            "false",
            "true",
            "5",
            "[-1, -0.5, 0, 2.5, 10000000000]",
            "true",
            "0",
            "4",
            "[\"\", \"B\", \"a\", \"ab\", \"b\", \"日本\"]",
            "0",
            "0",
            "[1]",
        ],
    );
}

#[test]
fn string_builder_appends_caches_and_counts_bytes() {
    let out = run_everywhere(
        r#"
import std::strings::StringBuilder;

fn main() {
    let b = new StringBuilder();
    println(b.is_empty());
    println(b.to_string() == "");
    b.append("");
    println(b.is_empty());
    b.append("héllo");
    b.append_line(",");
    b.append_i64(-9223372036854775807 - 1);
    b.append_f64(-0.5);
    b.append_bool(false);
    b.append_scalar(0x1F600);
    b.append_scalar(0x41);
    println(b.len());
    let first = b.to_string();
    print(first);
    println("");
    println(first.len() == b.len());
    // A second call with no appends returns the same text.
    println(b.to_string() == first);
    b.append("!");
    println(b.to_string() == first + "!");
    b.clear();
    println(b.len());
    b.append("again");
    println(b.to_string());
    let w = StringBuilder::with_text("seed");
    w.append("ed");
    println(w.to_string());
    // Many small pieces, with interleaved snapshots.
    let big = new StringBuilder();
    for i in 0..20000 {
        big.append("x");
        if i % 5000 == 0 { big.to_string(); }
    }
    let joined = big.to_string();
    println(joined.len());
    println(joined == "x".repeat(20000));
}
"#,
    );
    expect_lines(
        &out,
        &[
            "true",
            "true",
            "true",
            "42",
            "héllo,",
            "-9223372036854775808-0.5false😀A",
            "true",
            "true",
            "true",
            "0",
            "again",
            "seeded",
            "20000",
            "true",
        ],
    );
}

#[test]
fn byte_buffer_integers_copies_and_strict_utf8() {
    let out = run_everywhere(
        r#"
import std::bytes::{ByteBuffer, Utf8Error};
import std::collections::Array;

fn decode(bytes: Array<u8>) -> String {
    match ByteBuffer::from_array(bytes).decode_utf8() {
        Ok(text) => { return "ok:" + text; }
        Err(error) => { return "err@" + error.position.to_string(); }
    }
}

fn main() {
    let b = ByteBuffer::from_string("hé");
    println(b.to_string());
    println(b.is_empty());
    for value in [0, 1, -1, 255, 256, -9223372036854775807 - 1, 9223372036854775807] {
        let e = new ByteBuffer();
        e.write_i64_le(value);
        e.write_i64_be(value);
        if e.len() != 16 || e.read_i64_le(0) != value || e.read_i64_be(8) != value {
            println("roundtrip failed");
        }
    }
    let le = new ByteBuffer();
    le.write_i64_le(258);
    println(le.to_string());
    let be = new ByteBuffer();
    be.write_i64_be(258);
    println(be.to_string());

    // slice() and to_array() are independent copies; set() edits in place.
    let part = b.slice(1, 3);
    let raw = b.to_array();
    raw[0] = 0;
    part.set(0, 0x41);
    println(b.get(0));
    println(b.get(1));
    println(part.to_string());
    b.push(255);
    b.extend([0, 7]);
    println(b.len());
    println(b.get(b.len() - 1));
    b.clear();
    println(b.len());
    println(ByteBuffer::from_array([]).decode_utf8().unwrap() == "");

    println(decode([0x41, 0x7F]));
    println(decode([0xC2, 0x80, 0xDF, 0xBF]) == "ok:" + text::from_scalar(0x80) + text::from_scalar(0x7FF));
    println(decode([0xE0, 0xA0, 0x80, 0xEF, 0xBF, 0xBF]) == "ok:" + text::from_scalar(0x800) + text::from_scalar(0xFFFF));
    println(decode([0xF0, 0x90, 0x80, 0x80, 0xF4, 0x8F, 0xBF, 0xBF]) == "ok:" + text::from_scalar(0x10000) + text::from_scalar(0x10FFFF));
    println(decode([0xED, 0x9F, 0xBF]) == "ok:" + text::from_scalar(0xD7FF));
    // Overlong, surrogate, beyond U+10FFFF, invalid lead, lone continuation,
    // truncated and bad-continuation sequences.
    println(decode([0x61, 0xC0, 0x80]));
    println(decode([0xC1, 0xBF]));
    println(decode([0xE0, 0x9F, 0xBF]));
    println(decode([0x61, 0x62, 0xED, 0xA0, 0x80]));
    println(decode([0xF0, 0x8F, 0xBF, 0xBF]));
    println(decode([0xF4, 0x90, 0x80, 0x80]));
    println(decode([0xF5, 0x80, 0x80, 0x80]));
    println(decode([0x80]));
    println(decode([0x61, 0xE2, 0x82]));
    println(decode([0xE2, 0x28, 0xA1]));
    println(decode([0xF0, 0x9F, 0x98, 0x41]));
    let roundtrip = ByteBuffer::from_string("日本😀é");
    println(roundtrip.decode_utf8().unwrap());
}
"#,
    );
    expect_lines(
        &out,
        &[
            "[104, 195, 169]",
            "false",
            "[2, 1, 0, 0, 0, 0, 0, 0]",
            "[0, 0, 0, 0, 0, 0, 1, 2]",
            "104",
            "195",
            "[65, 169]",
            "6",
            "7",
            "0",
            "true",
            "ok:A\u{7f}",
            "true",
            "true",
            "true",
            "true",
            "err@1",
            "err@0",
            "err@0",
            "err@2",
            "err@0",
            "err@0",
            "err@0",
            "err@0",
            "err@1",
            "err@0",
            "err@0",
            "日本😀é",
        ],
    );
}

#[test]
fn invalid_bytes_keys_and_scalars_panic_with_messages() {
    let cases = [
        (
            "import std::bytes::ByteBuffer; fn main() { let b = new ByteBuffer(); b.write_i64_le(1); b.read_i64_le(1); }",
            "byte buffer read out of bounds",
        ),
        (
            "import std::bytes::ByteBuffer; fn main() { new ByteBuffer().read_i64_be(-1); }",
            "byte buffer read out of bounds",
        ),
        (
            "import std::sorted_map::SortedMap; fn main() { let m: SortedMap<f64, i64> = new SortedMap(); m.insert(0.0 / 0.0, 1); }",
            "SortedMap key must not be NaN",
        ),
        (
            "import std::set::HashSet; fn main() { let s: HashSet<f64> = new HashSet(); s.insert(0.0 / 0.0); }",
            "HashSet element must not be NaN",
        ),
        (
            "import std::set::HashSet; fn main() { HashSet<f64>::from_array([1.0, 0.0 / 0.0]); }",
            "HashSet element must not be NaN",
        ),
        (
            "import std::strings::StringBuilder; fn main() { new StringBuilder().append_scalar(0xD800); }",
            "",
        ),
        (
            "import std::bytes::ByteBuffer; fn main() { ByteBuffer::from_string(\"a\").get(1); }",
            "",
        ),
    ];
    for (source, message) in cases {
        let (out, ok) = compile_and_run_check_exit(source);
        assert!(!ok, "{source} must fail:\n{out}");
        assert!(out.contains(message), "{source}:\n{out}");
    }
}

#[test]
fn byte_buffer_large_offsets_report_bounds_in_both_build_modes() {
    for method in ["read_i64_le", "read_i64_be"] {
        for offset in ["9223372036854775807", "9223372036854775800"] {
            let source = format!(
                "import std::bytes::ByteBuffer; fn main() {{ defer {{ if let Some(info) = recover() {{ println(info.message); }} }} let b = new ByteBuffer(); b.write_i64_le(1); b.{method}({offset}); }}"
            );
            for run in [
                compile_and_run as fn(&str) -> (String, bool),
                compile_and_run_release,
            ] {
                let (out, ok) = run(&source);
                assert!(ok, "{method}({offset}) must recover: {out}");
                assert_eq!(
                    out.trim(),
                    "byte buffer read out of bounds",
                    "{method}({offset}): {out}"
                );
            }
        }
    }
}

#[test]
fn path_operations_follow_host_rules_and_env_var_reads_environment() {
    let source = r#"
fn show(value: Option<String>) -> String {
    match value { Some(v) => { return "[" + v + "]"; } None => { return "none"; } }
}
fn main() {
    for input in ["a/b/c.tar.gz", "c.txt", "", "/", "a/..", ".bashrc", "dir/", "日本/語.txt", "a\\b.txt", "C:\\x\\y.z"] {
        println(show(path::parent(input)) + " " + show(path::file_name(input)) + " "
            + show(path::extension(input)) + " " + show(path::file_stem(input)) + " "
            + path::is_absolute(input).to_string());
    }
    println(path::join("dir", "file"));
    println(path::join("", "file"));
    println(path::join("dir", "/abs"));
    let path = "shadowed local";
    println(path);
    println(show(env::var("WILLOW_STD_ADDITIONS_VAR")));
    println(show(env::var("WILLOW_STD_ADDITIONS_UNSET")));
    println(show(env::var("")));
    println(show(env::var("A=B")));
}
"#;
    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_STD_ADDITIONS_VAR", "värde=1")]);
    assert!(ok, "{out}");
    // The expectations come from Rust's std::path on the same host.
    use std::path::Path;
    let show = |value: Option<&str>| value.map_or("none".to_string(), |v| format!("[{v}]"));
    let mut expected = Vec::new();
    for input in [
        "a/b/c.tar.gz",
        "c.txt",
        "",
        "/",
        "a/..",
        ".bashrc",
        "dir/",
        "日本/語.txt",
        "a\\b.txt",
        "C:\\x\\y.z",
    ] {
        let path = Path::new(input);
        expected.push(format!(
            "{} {} {} {} {}",
            show(path.parent().and_then(Path::to_str)),
            show(path.file_name().and_then(|s| s.to_str())),
            show(path.extension().and_then(|s| s.to_str())),
            show(path.file_stem().and_then(|s| s.to_str())),
            path.is_absolute()
        ));
    }
    for (base, child) in [("dir", "file"), ("", "file"), ("dir", "/abs")] {
        expected.push(Path::new(base).join(child).to_str().unwrap().to_string());
    }
    expected.extend(["shadowed local", "[värde=1]", "none", "none", "none"].map(str::to_string));
    assert_eq!(out.lines().collect::<Vec<_>>(), expected);
}

#[test]
fn std_additions_compile_time_rules() {
    let rejected = [
        (
            "import std::set::HashSet; import std::collections::Array; fn main() { let s: HashSet<Array<i64>> = new HashSet(); s.insert([1]); }",
            "no method `insert`",
        ),
        (
            "import std::sorted_map::SortedMap; fn main() { let m: SortedMap<bool, i64> = new SortedMap(); m.insert(true, 1); }",
            "no method `insert`",
        ),
        (
            "import std::bytes::ByteBuffer; fn main() { let b = new ByteBuffer(); println(b.data.len()); }",
            "is private",
        ),
        (
            "import std::sorted_map::SortedMap; fn main() { let m: SortedMap<i64, i64> = new SortedMap(); println(m.root); }",
            "is private",
        ),
        (
            "fn main() { println(path::join(1, 2)); }",
            "expected `String`, found `i64`",
        ),
        (
            "fn main() { let v: String = env::var(\"X\"); }",
            "found `Option<String>`",
        ),
        ("import std::set::Nope; fn main() {}", "E2006"),
        ("import std::sett; fn main() {}", "set"),
        (
            "@effects(no_io) fn f() -> bool { return env::var(\"X\").is_some(); } fn main() { f(); }",
            "E2610",
        ),
    ];
    for (source, expected) in rejected {
        assert_compile_error_contains(source, &[expected]);
    }
    let (out, ok) = compile_and_run(
        r#"
import std::path as p;
import std::set;
import std::sorted_map as tree;
@effects(no_io) fn name(input: String) -> String {
    match p::file_name(input) { Some(v) => { return v; } None => { return ""; } }
}
@effects(no_io) fn count() -> i64 {
    let s: set::HashSet<i64> = new set::HashSet();
    s.insert(1);
    let m: tree::SortedMap<i64, i64> = new tree::SortedMap();
    m.insert(1, 2);
    return s.len() + m.len();
}
fn main() { println(name(p::join("a", "b.txt"))); println(count()); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "b.txt\n2\n");
}

#[test]
fn std_additions_runnable_examples() {
    for (path, expected) in [
        (
            "example/hash_set.wi",
            "6\n2\ntrue\ntrue\nfalse\n{1, 2, 3, 4, 6, 8}\n{2, 4}\n{1, 3}\ntrue\n",
        ),
        (
            "example/sorted_map.wi",
            "alice=95\nbob=85\ncarol=72\n30\n40\n[20, 30, 40]\nremoved event@10\n20\n4\n",
        ),
        (
            "example/string_builder.wi",
            "row 1: 10 ok=true\nrow 2: 20, 21 ok=false\nrow 3: 30, 31, 32 ok=true\n0.25 \u{2713} done\n81\n",
        ),
        (
            "example/byte_buffer.wi",
            "15\n2026\nh\u{e9}llo!\ninvalid UTF-8 at 2\n[254, 255, 255, 255, 255, 255, 255, 255]\n-2\n",
        ),
        (
            "example/path_ops.wi",
            "release.tar.gz\ngz\nrelease.tar\nassets/images\n(none)\n(none)\nfalse\nnotes.txt\n",
        ),
        (
            "example/env_var.wi",
            "unset, using default\ntrue\ntrue\ntrue\n",
        ),
    ] {
        let (out, ok) = compile_file_and_run(path);
        assert!(ok, "{path}: {out}");
        assert_eq!(out, expected, "{path}");
    }
}

/// Instrument the real `std/sorted_map.wi` with a node-visit counter and a
/// height probe. Over sorted, reverse-sorted and scrambled insertion orders,
/// the visits per insert/lookup/remove stay within c·log2(n) and the AVL
/// height within 1.45·log2(n + 2), at every doubling of n.
#[test]
fn sorted_map_operation_counts_are_logarithmic() {
    let mut source = include_str!("../../std/sorted_map.wi")
        .replace("    freed: i64;", "    freed: i64;\n    pub steps: i64;")
        .replace(
            "        self.freed = -1;\n    }",
            "        self.freed = -1;\n        self.steps = 0;\n    }\n    pub fn tree_height() -> i64 { return self.depth(self.root); }",
        )
        .replace(
            "        while node >= 0 {\n            let current = self.node_keys[node];",
            "        while node >= 0 {\n            self.steps = self.steps + 1;\n            let current = self.node_keys[node];",
        )
        .replace(
            "            while node != last {",
            "            while node != last {\n                self.steps = self.steps + 1;",
        )
        .replace(
            "while self.left[successor] >= 0 { successor = self.left[successor]; }",
            "while self.left[successor] >= 0 { self.steps = self.steps + 1; successor = self.left[successor]; }",
        );
    // Count every recursive insert/remove step, whatever key bound the
    // declarations spell.
    for header in ["    fn insert_at(", "    fn remove_at("] {
        let start = source.find(header).expect(header);
        let open = start + source[start..].find("{\n").expect("body") + 1;
        source.insert_str(open, "\n        self.steps = self.steps + 1;");
    }
    assert_eq!(source.matches("self.steps = self.steps + 1").count(), 5);
    source.push_str(
        r#"
fn log2(n: i64) -> i64 {
    let mut bits = 0;
    let mut rest = n;
    while rest > 1 { rest = rest / 2; bits += 1; }
    return bits;
}
fn main() {
    for n in [256, 1024, 4096, 16384] {
        for shape in 0..3 {
            let m: SortedMap<i64, i64> = new SortedMap();
            let mut worst = 0;
            for i in 0..n {
                let mut k = i;
                if shape == 1 { k = n - i; }
                if shape == 2 { k = (i * 7919) % n; }
                let before = m.steps;
                m.insert(k, i);
                if m.steps - before > worst { worst = m.steps - before; }
            }
            let height = m.tree_height();
            let inserts = m.steps;
            m.steps = 0;
            for i in 0..n { m.get(i); m.get(n + i + 1); }
            let lookups = m.steps;
            m.steps = 0;
            for i in 0..n {
                if i % 2 == 0 {
                    let before = m.steps;
                    m.remove((i * 7919) % n);
                    if m.steps - before > worst { worst = m.steps - before; }
                }
            }
            let removes = m.steps;
            let bound = 3 * log2(n) + 6;
            if worst > bound { panic("superlogarithmic operation"); }
            if height * 100 > 145 * log2(n + 2) + 100 { panic("unbalanced"); }
            println(n.to_string() + " " + shape.to_string() + " " + height.to_string() + " "
                + worst.to_string() + " " + (inserts / n).to_string() + " "
                + (lookups / (2 * n)).to_string() + " " + (removes / (n / 2)).to_string());
        }
    }
}
"#,
    );
    let (out, ok) = compile_and_run_release(&source);
    assert!(ok, "{out}");
    let rows: Vec<Vec<i64>> = out
        .lines()
        .map(|line| line.split(' ').map(|v| v.parse().unwrap()).collect())
        .collect();
    assert_eq!(rows.len(), 12);
    for row in &rows {
        let log2 = 63 - row[0].leading_zeros() as i64;
        // Average visits per operation track log2(n), not n. A removal
        // walks three root-to-leaf paths at most: to the key, to its
        // successor, and to the parent of the slot compaction moves.
        assert!(row[4] <= 2 * log2 + 4, "{row:?}");
        assert!(row[5] <= 2 * log2 + 4, "{row:?}");
        assert!(row[6] <= 3 * log2 + 6, "{row:?}");
    }
    eprintln!("SortedMap n shape height worst avg_insert avg_lookup avg_remove: {rows:?}");
}

/// Constant work per operation in the Willow layer of HashSet, StringBuilder
/// and ByteBuffer, measured by instrumenting their real sources. Combined
/// with the runtime measurements (hash probes per map lookup, element copies
/// per array push), each operation is expected O(1) / amortized O(1).
#[test]
fn set_and_builder_operation_counts_are_constant() {
    let set = include_str!("../../std/set.wi")
        .replace(
            "    slots: Map<T, i64>;",
            "    slots: Map<T, i64>;\n    pub steps: i64;",
        )
        .replace(
            "        self.slots = Map::new();\n    }\n\n    pub static fn",
            "        self.slots = Map::new();\n        self.steps = 0;\n    }\n\n    pub static fn",
        )
        // One step per map operation the set performs.
        .replace(
            "if self.slots.contains(value) { return false; }",
            "self.steps = self.steps + 1; if self.slots.contains(value) { return false; }",
        )
        .replace(
            "        self.slots.insert(value, self.elements.len());",
            "        self.steps = self.steps + 1; self.slots.insert(value, self.elements.len());",
        )
        .replace(
            "        return self.slots.contains(value);",
            "        self.steps = self.steps + 1; return self.slots.contains(value);",
        )
        .replace(
            "        if let Some(slot) = self.slots.remove(value) {",
            "        self.steps = self.steps + 1; if let Some(slot) = self.slots.remove(value) {",
        )
        .replace(
            "                self.slots.insert(last, slot);",
            "                self.steps = self.steps + 1; self.slots.insert(last, slot);",
        );
    assert_eq!(set.matches("self.steps = self.steps + 1").count(), 5);
    let builder = include_str!("../../std/strings.wi")
        .replace("import std::collections::Array;\nimport std::text;\n", "")
        .replace("    byte_len: i64;", "    byte_len: i64;\n    pub pieces_joined: i64;\n    pub appends: i64;\n    pub bytes_copied: i64;")
        .replace(
            "        self.byte_len = 0;\n    }\n\n    pub static fn",
            "        self.byte_len = 0;\n        self.pieces_joined = 0;\n        self.appends = 0;\n        self.bytes_copied = 0;\n    }\n\n    pub static fn",
        )
        .replace(
            "        self.pieces.push(text);",
            "        self.appends = self.appends + 1;\n        self.pieces.push(text);",
        )
        .replace(
            "            let joined = self.pieces.join(\"\");",
            "            self.pieces_joined = self.pieces_joined + self.pieces.len();\n            self.bytes_copied = self.bytes_copied + self.byte_len;\n            let joined = self.pieces.join(\"\");",
        );
    let mut source = String::from("import std::collections::{Array, Map};\nimport std::text;\n");
    source.push_str(&set.replace("import std::collections::{Array, Map};\n", ""));
    source.push_str(&builder);
    source.push_str(
        r#"
fn main() {
    for n in [1000, 4000, 16000] {
        let s: HashSet<i64> = new HashSet();
        for i in 0..n { s.insert(i * 4294967296); }
        let inserts = s.steps;
        s.steps = 0;
        for i in 0..n { s.contains(i * 4294967296); s.contains(-i - 1); }
        let lookups = s.steps;
        s.steps = 0;
        // Remove from the front: every removal moves the last element.
        for i in 0..n { s.remove(i * 4294967296); }
        let removes = s.steps;
        if s.len() != 0 { panic("not empty"); }

        let b = new StringBuilder();
        for i in 0..n { b.append("ab"); if i % 100 == 0 { b.to_string(); } }
        b.to_string();
        b.to_string();
        // One build: a single to_string copies every byte exactly once, and
        // a repeated call with no append copies nothing.
        let once = new StringBuilder();
        for i in 0..n { once.append("ab"); }
        once.to_string();
        let first = once.bytes_copied;
        once.to_string();
        println(n.to_string() + " " + inserts.to_string() + " " + lookups.to_string() + " "
            + removes.to_string() + " " + b.appends.to_string() + " " + b.pieces_joined.to_string()
            + " " + b.len().to_string() + " " + b.bytes_copied.to_string() + " "
            + first.to_string() + " " + once.bytes_copied.to_string());
    }
}
"#,
    );
    let (out, ok) = compile_and_run_release(&source);
    assert!(ok, "{out}");
    let rows: Vec<Vec<i64>> = out
        .lines()
        .map(|line| line.split(' ').map(|v| v.parse().unwrap()).collect())
        .collect();
    assert_eq!(rows.len(), 3);
    for row in &rows {
        let n = row[0];
        // insert: one membership test and one map insert per element.
        assert_eq!(row[1], 2 * n, "{row:?}");
        // contains: exactly one map lookup each, hits and misses alike.
        assert_eq!(row[2], 2 * n, "{row:?}");
        // remove: one map remove plus at most one slot update.
        assert!(row[3] <= 2 * n, "{row:?}");
        // append: one piece push; every piece is joined at most once more
        // than the number of snapshots that collapse it.
        assert_eq!(row[4], n, "{row:?}");
        let snapshots = n / 100 + 2;
        assert!(row[5] <= n + snapshots, "{row:?}");
        assert_eq!(row[6], 2 * n, "{row:?}");
        // Interleaved snapshots copy the whole text so far each time (the
        // documented O(total bytes) per to_string after an append): the
        // snapshot after append i (i = 100, 200, ...) copies 2(i + 1) bytes,
        // the final call 2n.
        let interleaved: i64 = (100..n).step_by(100).map(|i| 2 * (i + 1)).sum::<i64>() + 2 * n;
        assert_eq!(row[7], interleaved, "{row:?}");
        assert_eq!(row[8], 2 * n, "{row:?}");
        assert_eq!(row[9], 2 * n, "{row:?}");
    }
    eprintln!(
        "HashSet/StringBuilder n inserts lookups removes appends pieces_joined bytes interleaved_copied once_copied once_after_repeat: {rows:?}"
    );
}

#[test]
fn byte_api_migration_diagnostics_and_literal_ranges() {
    for body in [
        "new ByteBuffer().push(256);",
        "new ByteBuffer().push(-1);",
        "net::write_bytes_async(s, [-1]);",
        "net::write_bytes_async(s, [256]);",
        "ByteBuffer::from_array([1, 999]);",
        "ByteBuffer::from_string(\"a\").set(0, 300);",
        "let a: Array<i64> = [0, 255]; ByteBuffer::from_array(a);",
        "let a: Array<i64> = [0]; new ByteBuffer().extend(a);",
        "let a: Array<i64> = [0]; accept(a);",
        "let a: Array<i64> = [0]; write(a);",
        "let a: Array<i64> = [0]; net::write_bytes_async(s, a);",
    ] {
        let source = format!(
            "import std::bytes::ByteBuffer; import std::net; import std::collections::Array;
             fn accept(a: Array<u8>) {{}}
             fn write(a: Array<u8>) {{}}
             fn probe(s: TcpStream) {{ {body} }} fn main() {{}}"
        );
        let path = temp_path(format!("byte_api_diagnostic_{}.wi", unique_test_id()));
        fs::write(&path, source).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["check", &path])
            .output()
            .unwrap();
        let _ = fs::remove_file(path);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{body}");
        if body.contains("Array<i64>") {
            assert!(
                stderr.contains("Array<u8>") && stderr.contains("checked_to_u8"),
                "{stderr}"
            );
        } else {
            assert!(
                stderr.contains("range") || stderr.contains("0..=255"),
                "{stderr}"
            );
        }
    }
}
