use super::*;

// ── `void` is a writable type (foundation for willow-exg) ──────────────────

// An explicit `-> void` return annotation is accepted and behaves like an
// omitted return type.
#[test]
fn test_explicit_void_return_type() {
    let (out, ok) = compile_and_run(
        r#"
fn greet() -> void { println(1); }
fn main() { greet(); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

// `void` is usable as a generic type argument in an annotation (e.g. a future
// Result<void, E>); the annotation parses and type-checks.
#[test]
fn test_void_as_generic_type_arg_annotation() {
    let (out, ok) = compile_and_run(
        r#"
fn use_r(r: Result<void, String>) -> i64 { return 0; }
fn main() { println(2); }
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\n");
}

// ----------------------------------------------------------------------------
// Range<i64> as a first-class value (willow: range-value feature).
// 20 perspectives on materializing, reading, passing, returning, and iterating
// a `Range<i64>` held as a value rather than only as an inline `for` iterable.
// ----------------------------------------------------------------------------

// P1: `let r = a..b` materializes a value; P2: `.start`; P3: `.end`.
#[test]
fn range_value_p01_let_and_fields() {
    let (out, ok) =
        compile_and_run("fn main() { let r = 4..9; println(r.start); println(r.end); }");
    assert!(ok, "{out}");
    assert_eq!(out, "4\n9\n");
}

// P4: a function may return `Range<i64>`; P5: and accept it as a parameter.
#[test]
fn range_value_p02_return_and_param() {
    let (out, ok) = compile_and_run(
        r#"
fn make() -> Range<i64> { return 3..8; }
fn width(r: Range<i64>) -> i64 { return r.end - r.start; }
fn main() {
    let r = make();
    println(r.start);
    println(width(r));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n5\n");
}

// P6: `for x in <range variable>` iterates the stored bounds.
#[test]
fn range_value_p03_for_over_variable() {
    let (out, ok) = compile_and_run("fn main() { let r = 1..4; for x in r { println(x); } }");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// P7: bounds may be arbitrary i64 expressions (not just literals).
#[test]
fn range_value_p04_expression_bounds() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a = 2;
    let b = a + 3;
    let r = (a - 1)..(b * 2);
    println(r.start);
    println(r.end);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n10\n");
}

// P8: an empty range (start == end) yields no iterations; fields still correct.
#[test]
fn range_value_p05_empty_range() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 5..5;
    let mut n = 0;
    for _ in r { n = n + 1; }
    println(n);
    println(r.end - r.start);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n0\n");
}

// P9: a reversed range (start > end) yields no iterations.
#[test]
fn range_value_p06_reversed_range_no_iterations() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 7..3;
    let mut n = 0;
    for _ in r { n = n + 1; }
    println(n);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

// P10: negative bounds; P11: summing a range variable.
#[test]
fn range_value_p07_negative_bounds_sum() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = -2..3;
    let mut total = 0;
    for x in r { total = total + x; }
    println(total);
    println(r.start);
}
"#,
    );
    assert!(ok, "{out}");
    // -2 + -1 + 0 + 1 + 2 = 0
    assert_eq!(out, "0\n-2\n");
}

// P12: multiple range values coexist independently.
#[test]
fn range_value_p08_multiple_ranges() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a = 0..2;
    let b = 10..13;
    println(a.end);
    println(b.start);
    for x in a { println(x); }
    for y in b { println(y); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n10\n0\n1\n10\n11\n12\n");
}

// P13: range value survives GC stress (heap object is rooted).
#[test]
fn range_value_p09_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
fn main() {
    let r = 2..6;
    let s = "keepalive";
    let mut total = 0;
    for x in r { total = total + x; }
    println(s);
    println(total);
    println(r.start);
}
"#,
    );
    assert!(ok, "{out}");
    // 2+3+4+5 = 14
    assert_eq!(out, "keepalive\n14\n2\n");
}

// P14: iterate directly over a range returned by a call.
#[test]
fn range_value_p10_for_over_call_result() {
    let (out, ok) = compile_and_run(
        r#"
fn upto(n: i64) -> Range<i64> { return 0..n; }
fn main() { for x in upto(3) { println(x); } }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n1\n2\n");
}

// P15: a `mut` range may be reassigned to another range value.
#[test]
fn range_value_p11_mut_reassign() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut r = 0..1;
    r = 5..8;
    println(r.start);
    println(r.end);
    for x in r { println(x); }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5\n8\n5\n6\n7\n");
}

// P16: range fields participate in conditions/arithmetic.
#[test]
fn range_value_p12_field_in_condition() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 4..10;
    if r.end > r.start {
        println(r.end - r.start);
    } else {
        println(0);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// P17: a range literal `for` loop still works (no regression).
#[test]
fn range_value_p13_literal_for_loop_regression() {
    let (out, ok) =
        compile_and_run("fn main() { let mut t = 0; for x in 1..5 { t = t + x; } println(t); }");
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

// P18: range value lives in an async frame across an await; fields read after.
#[test]
fn range_value_p14_async_frame_across_await() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn compute() -> i64 {
    let r = 3..7;
    await sleep(1);
    return r.start + r.end;
}
async fn main() {
    let v = await compute();
    println(v);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n");
}

// P19: cooperative `for` over a range variable with an await in the body.
#[test]
fn range_value_p15_cooperative_for_over_variable() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn run() -> i64 {
    let r = 1..4;
    let mut total = 0;
    for x in r {
        await sleep(1);
        total = total + x;
    }
    return total;
}
async fn main() {
    let t = await run();
    println(t);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// P20: range bounds must be `i64` (float bound is a diagnostic).
#[test]
fn range_value_p16_non_i64_bound_is_error() {
    assert_compile_error_contains(
        "fn main() { let r = 0.0..5; println(r.start); }",
        &["error[E0201]", "range bounds must be `i64`"],
    );
}

// P21: accessing an unknown range field is a diagnostic.
#[test]
fn range_value_p17_unknown_field_is_error() {
    assert_compile_error_contains(
        "fn main() { let r = 0..5; println(r.middle); }",
        &["error[E0201]", "has no field `middle`"],
    );
}
