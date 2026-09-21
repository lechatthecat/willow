use super::*;

// ── Array for-loop inline element access (willow-pcoy) ──────────────────────
// The loop header now loads len from the handle and the body loads the
// element through a re-read buffer pointer (no willow_array_len/get calls).
// 20 perspectives: 1 i64 sum, 2 String elements (GC-managed), 3 f64
// elements, 4 bool elements, 5 empty array body never runs, 6 single
// element, 7 push DURING iteration is observed (len re-read), 8 pop DURING
// iteration shrinks the walk, 9 growth reallocation mid-iteration (buffer
// pointer re-read), 10 nested loops over the same array, 11 `_` binding
// (no element read), 12 loop variable is a copy (mutating array after read
// does not change it), 13 class elements, 14 large array, 15 two sequential
// loops same array, 16 for inside async fn, 17 GC stress with string
// elements, 18 GC stress with growth mid-iteration, 19 element order
// preserved, 20 loop over freshly returned array expression.

#[test]
fn afor_01_i64_sum() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3, 4]; let mut s = 0; for x in xs { s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

#[test]
fn afor_02_string_elements() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"a\", \"b\"]; for x in xs { println(x); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a\nb\n");
}

#[test]
fn afor_03_f64_elements() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<f64> = [0.5, 1.25]; let mut s = 0.0; for x in xs { s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1.75\n");
}

#[test]
fn afor_04_bool_elements() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<bool> = [true, false, true]; let mut n = 0; for b in xs { if b { n = n + 1; } } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

#[test]
fn afor_05_empty_never_runs() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = []; for x in xs { println(x); } println(9); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn afor_06_single_element() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [7]; for x in xs { println(x); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}

#[test]
fn afor_07_push_during_iteration_observed() {
    // len is re-read each entry: pushing while below 3 extends the walk.
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; let mut n = 0; for x in xs { n = n + 1; if n < 3 { xs.push(n * 10); } } println(n); println(xs.len()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n3\n");
}

#[test]
fn afor_08_pop_during_iteration_shrinks() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3, 4, 5, 6]; let mut n = 0; for x in xs { n = n + 1; xs.pop(); } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn afor_09_growth_realloc_mid_iteration() {
    // Start at cap 1; pushes force buffer reallocation while iterating —
    // subsequent element reads must go through the NEW buffer.
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [10]; let mut i = 0; for x in xs { if i < 7 { xs.push(x + 1); } i = i + 1; } println(xs.len()); let mut s = 0; for x in xs { s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "8\n108\n");
}

#[test]
fn afor_10_nested_same_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2]; let mut s = 0; for a in xs { for b in xs { s = s + a * b; } } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

#[test]
fn afor_11_underscore_binding() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3]; let mut n = 0; for _ in xs { n = n + 1; } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn afor_12_loop_var_is_copy() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [5, 6]; for x in xs { xs[0] = 99; println(x); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n6\n");
}

#[test]
fn afor_13_class_elements() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nclass P { pub v: i64; }\nfn main() { let xs: Array<P> = [new P(1), new P(2)]; let mut s = 0; for p in xs { s = s + p.v; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn afor_14_large_array() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = []; let mut i = 0; while i < 10000 { xs.push(i); i = i + 1; } let mut s = 0; for x in xs { s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "49995000\n");
}

#[test]
fn afor_15_two_sequential_loops() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1, 2, 3]; let mut a = 0; for x in xs { a = a + x; } let mut b = 0; for x in xs { b = b + x * 2; } println(a + b); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

#[test]
fn afor_16_inside_async_fn() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nasync fn work() -> i64 { let xs: Array<i64> = [1, 2, 3]; let mut s = 0; for x in xs { s = s + x; } return s; }\nasync fn main() { println(await work()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

#[test]
fn afor_17_gc_stress_strings() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"x\", \"y\", \"z\"]; let mut out = \"\"; for s in xs { out = out + s; } println(out); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "xyz\n");
}

#[test]
fn afor_18_gc_stress_growth() {
    let (out, ok) = compile_and_run_gc_stress(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; let mut i = 0; for x in xs { if i < 20 { xs.push(x + 1); } i = i + 1; } println(xs.len()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "21\n");
}

#[test]
fn afor_19_order_preserved() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [3, 1, 2]; for x in xs { println(x); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n1\n2\n");
}

#[test]
fn afor_20_fresh_array_expression() {
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn make() -> Array<i64> { let xs: Array<i64> = [4, 5]; return xs; }\nfn main() { let mut s = 0; for x in make() { s = s + x; } println(s); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}
