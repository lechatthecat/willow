use super::*;

// ── Collection debug display: .toString() (willow-vwn6) ─────────────────────
// 20 perspectives: 1 i64 array, 2 String array (quoted), 3 f64 array
// (shortest repr), 4 bool array, 5 empty array, 6 single element, 7 f64
// specials inside (Infinity/NaN), 8 negative numbers, 9 map sorted by key,
// 10 empty map, 11 i64-keyed map, 12 map String values quoted, 13 format {}
// interop, 14 println of the result, 15 class-element array rejected,
// 16 nested array rejected, 17 map with class values rejected, 18 toString
// with arguments rejected, 19 result usable in concatenation, 20 GC stress.

#[test]
fn tostr_01_i64_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[1, 2, 3]\n");
}

#[test]
fn tostr_02_string_array_quoted() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"a\", \"b c\"]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[\"a\", \"b c\"]\n");
}

#[test]
fn tostr_03_f64_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<f64> = [0.5, 2.25]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[0.5, 2.25]\n");
}

#[test]
fn tostr_04_bool_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<bool> = [true, false]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[true, false]\n");
}

#[test]
fn tostr_05_empty_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = []; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[]\n");
}

#[test]
fn tostr_06_single_element() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [7]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[7]\n");
}

#[test]
fn tostr_07_f64_specials_inside() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let inf = 1.0 / 0.0; let nan = 0.0 / 0.0; let xs: Array<f64> = [inf, 0.0 - inf, nan, 0.0]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[Infinity, -Infinity, NaN, 0.0]\n");
}

#[test]
fn tostr_08_negative_numbers() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [0 - 5, 0, 5]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[-5, 0, 5]\n");
}

#[test]
fn tostr_09_map_sorted_by_key() {
    let (out, ok) = compile_and_run(
        "import std::collections::Map;\nfn main() { let m: Map<String, i64> = Map::new(); m.insert(\"b\", 2); m.insert(\"a\", 1); m.insert(\"c\", 3); println(m.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "{a: 1, b: 2, c: 3}\n");
}

#[test]
fn tostr_10_empty_map() {
    let (out, ok) = compile_and_run(
        "import std::collections::Map;\nfn main() { let m: Map<String, i64> = Map::new(); println(m.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "{}\n");
}

#[test]
fn tostr_11_i64_keyed_map() {
    let (out, ok) = compile_and_run(
        "import std::collections::Map;\nfn main() { let m: Map<i64, String> = Map::new(); m.insert(2, \"b\"); m.insert(1, \"a\"); println(m.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "{1: \"a\", 2: \"b\"}\n");
}

#[test]
fn tostr_12_map_string_values_quoted() {
    let (out, ok) = compile_and_run(
        "import std::collections::Map;\nfn main() { let m: Map<String, String> = Map::new(); m.insert(\"k\", \"v w\"); println(m.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "{k: \"v w\"}\n");
}

#[test]
fn tostr_13_format_interop() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2]; println(format(\"xs = {}\", xs.toString())); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "xs = [1, 2]\n");
}

#[test]
fn tostr_14_println_direct() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { println([10, 20].toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[10, 20]\n");
}

#[test]
fn tostr_15_class_element_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::collections::Array;\nclass P { pub x: i64; }\nfn main() { let ps: Array<P> = [new P(1)]; println(ps.toString()); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E1402"), "{stderr}");
}

#[test]
fn tostr_16_nested_array_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::collections::Array;\nfn main() { let xs: Array<Array<i64>> = [[1]]; println(xs.toString()); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E1402"), "{stderr}");
}

#[test]
fn tostr_17_map_class_values_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::collections::Map;\nclass P { pub x: i64; }\nfn main() { let m: Map<String, P> = Map::new(); println(m.toString()); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("E1402"), "{stderr}");
}

#[test]
fn tostr_18_arguments_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; println(xs.toString(2)); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("no arguments"), "{stderr}");
}

#[test]
fn tostr_19_concatenation() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; println(\"xs: \" + xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "xs: [1]\n");
}

#[test]
fn tostr_20_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"a\", \"b\", \"c\"]; println(xs.toString()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "[\"a\", \"b\", \"c\"]\n");
}
