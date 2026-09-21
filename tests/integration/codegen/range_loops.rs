use super::*;

// ── For loops over i64 ranges (willow-range-for) ────────────────────────────
// 22 explicit perspectives: half-open behavior, empty ranges, bound typing,
// evaluation order, scoping, array interop, and cooperative async.

// Perspective 1: `start..end` is half-open.
#[test]
fn test_range_for_loop_perspective_01_half_open_prints_start_to_end_minus_one() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    for n in 1..4 {
        println(n);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n");
}

// Perspective 2: `1..101` covers 1 through 100.
#[test]
fn test_range_for_loop_perspective_02_one_to_one_hundred_sum() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for n in 1..101 {
        total = total + n;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "5050\n");
}

// Perspective 3: equal start/end runs zero iterations.
#[test]
fn test_range_for_loop_perspective_03_equal_bounds_are_empty() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut count = 0;
    for _ in 5..5 {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

// Perspective 4: descending ranges run zero iterations.
#[test]
fn test_range_for_loop_perspective_04_descending_range_is_empty() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut count = 0;
    for _ in 5..2 {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "0\n");
}

// Perspective 5: negative starts work with the same +1 step.
#[test]
fn test_range_for_loop_perspective_05_negative_start() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    for n in -2..2 {
        println(n);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-2\n-1\n0\n1\n");
}

// Perspective 6: variable bounds are accepted.
#[test]
fn test_range_for_loop_perspective_06_variable_bounds() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let start = 2;
    let end = 5;
    let mut total = 0;
    for n in start..end {
        total = total + n;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

// Perspective 7: arithmetic bound expressions are accepted.
#[test]
fn test_range_for_loop_perspective_07_arithmetic_bounds() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for n in (1 + 1)..(3 + 2) {
        total = total + n;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "9\n");
}

// Perspective 8: bound expressions are evaluated once, left to right.
#[test]
fn test_range_for_loop_perspective_08_bounds_evaluated_once_left_to_right() {
    let (out, ok) = compile_and_run(
        r#"
fn start() -> i64 {
    println(10);
    return 1;
}

fn stop() -> i64 {
    println(20);
    return 3;
}

fn main() {
    for n in start()..stop() {
        println(n);
    }
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "10\n20\n1\n2\n");
}

// Perspective 9: nested range loops compose.
#[test]
fn test_range_for_loop_perspective_09_nested_ranges() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for a in 1..3 {
        for b in 1..3 {
            total = total + a * 10 + b;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "66\n");
}

// Perspective 10: range loops can live inside while loops.
#[test]
fn test_range_for_loop_perspective_10_range_inside_while() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut round = 0;
    let mut total = 0;
    while round < 2 {
        for n in 1..3 {
            total = total + n;
        }
        round = round + 1;
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// Perspective 11: while loops can live inside range loop bodies.
#[test]
fn test_range_for_loop_perspective_11_while_inside_range() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut total = 0;
    for limit in 1..4 {
        let mut i = 0;
        while i < limit {
            total = total + 1;
            i = i + 1;
        }
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n");
}

// Perspective 12: `_` discards range elements but preserves iteration count.
#[test]
fn test_range_for_loop_perspective_12_underscore_discards_range_item() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut count = 0;
    for _ in 3..7 {
        count = count + 1;
    }
    println(count);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "4\n");
}

// Perspective 13: the loop variable shadows only inside the range loop.
#[test]
fn test_range_for_loop_perspective_13_shadowing_restores_outer() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let n = 99;
    for n in 1..3 {
        println(n);
    }
    println(n);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n99\n");
}

// Perspective 14: returning from inside a range loop terminates the function.
#[test]
fn test_range_for_loop_perspective_14_return_inside_range_loop() {
    let (out, ok) = compile_and_run(
        r#"
fn first() -> i64 {
    for n in 2..5 {
        return n;
    }
    return 0;
}

fn main() {
    println(first());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "2\n");
}

// Perspective 15: range loops interoperate with Array indexing.
#[test]
fn test_range_for_loop_perspective_15_range_indexes_array() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;

fn main() {
    let xs: Array<i64> = [5, 6, 7];
    let mut total = 0;
    for i in 0..xs.len() {
        total = total + xs[i];
    }
    println(total);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "18\n");
}

// Perspective 16: the end bound is snapshotted before the loop starts.
#[test]
fn test_range_for_loop_perspective_16_end_bound_snapshot() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let mut end = 4;
    let mut total = 0;
    for n in 1..end {
        total = total + n;
        if n == 1 {
            end = 2;
        }
    }
    println(total);
    println(end);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "6\n2\n");
}

// Perspective 17: range loop variables are immutable.
#[test]
fn test_range_for_loop_perspective_17_loop_var_assignment_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in 1..3 {
        n = 9;
    }
}
"#,
        &["error[E0301]", "cannot assign to immutable variable `n`"],
    );
}

// Perspective 18: range loop variables do not leak out of the body.
#[test]
fn test_range_for_loop_perspective_18_loop_var_is_scoped_to_body() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in 1..3 {
        println(n);
    }
    println(n);
}
"#,
        &["error[E0350]", "cannot find variable `n`"],
    );
}

// Perspective 19: the start bound must be i64.
#[test]
fn test_range_for_loop_perspective_19_start_bound_must_be_i64() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in true..3 {
        println(n);
    }
}
"#,
        &["error[E0201]", "range bounds must be `i64`"],
    );
}

// Perspective 20: the end bound must be i64.
#[test]
fn test_range_for_loop_perspective_20_end_bound_must_be_i64() {
    assert_compile_error_contains(
        r#"
fn main() {
    for n in 1..3.5 {
        println(n);
    }
}
"#,
        &["error[E0201]", "range bounds must be `i64`"],
    );
}

// Perspective 21: a range outside a `for` loop is now a first-class value.
#[test]
fn test_range_for_loop_perspective_21_range_value_outside_for_is_allowed() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r = 1..3;
    println(r.start);
    println(r.end);
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n3\n");
}

// Perspective 22: await works inside range loops in async main and leaf fns.
#[test]
fn test_range_for_loop_perspective_22_async_await_in_range_main_and_leaf() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"
async fn sum() -> i64 {
    let mut total = 0;
    for n in 1..5 {
        await sleep(1);
        total = total + n;
    }
    return total;
}

async fn main() {
    for n in 1..4 {
        await sleep(1);
        println(n);
    }
    println(await sum());
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\n3\n10\n");
}
