use super::super::support::*;

// ── Control flow ─────────────────────────────────────────────────────────────

#[test]
fn test_if_else() {
    let src = r#"
fn main() {
    let x = 5;
    if x > 3 {
        println(1);
    } else {
        println(0);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

#[test]
fn test_if_without_else() {
    let src = r#"
fn main() {
    let mut value = 1;

    if true {
        value = value + 41;
    }

    println(value);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_while_loop() {
    let src = r#"
fn main() {
    let mut i = 0;
    while i < 5 {
        println(i);
        i = i + 1;
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "0\n1\n2\n3\n4\n");
}

#[test]
fn test_while_zero_iterations() {
    let src = r#"
fn main() {
    let mut count = 0;

    while false {
        count = count + 1;
    }

    println(count);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "0");
}

#[test]
fn test_nested_if_inside_while() {
    let src = r#"
fn main() {
    let mut i = 0;
    let mut total = 0;

    while i < 6 {
        if i % 2 == 0 {
            total = total + i;
        } else {
            total = total + 1;
        }
        i = i + 1;
    }

    println(total);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "9");
}

#[test]
fn test_while_factorial_accumulator() {
    let src = r#"
fn main() {
    let mut n = 1;
    let mut acc = 1;

    while n <= 6 {
        acc = acc * n;
        n = n + 1;
    }

    println(acc);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "720");
}

#[test]
fn test_bool_condition_from_expression() {
    let src = r#"
fn main() {
    let a = 10;
    let b = 20;

    if (a < b && b == 20) || false {
        println(1);
    } else {
        println(0);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

// ── Functions ────────────────────────────────────────────────────────────────

#[test]
fn test_function_call() {
    let src = r#"
fn add(a: i64, b: i64) -> i64 {
    return a + b;
}
fn main() {
    println(add(10, 32));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_nested_calls_and_bool_return() {
    let src = r#"
fn midpoint(a: f64, b: f64) -> f64 {
    return (a + b) / 2.0;
}

fn above_midpoint(a: f64, b: f64, limit: f64) -> bool {
    return midpoint(a, b) > limit;
}

fn main() {
    println(midpoint(3.0, 5.0));
    println(above_midpoint(3.0, 5.0, 3.5));
    println(above_midpoint(3.0, 5.0, 4.0));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "4\ntrue\nfalse\n");
}

#[test]
fn test_recursive_fib() {
    let src = r#"
fn fib(n: i64) -> i64 {
    if n <= 1 {
        return n;
    }
    return fib(n - 1) + fib(n - 2);
}
fn main() {
    println(fib(10));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "55");
}

#[test]
fn test_recursive_factorial_function() {
    let src = r#"
fn factorial(n: i64) -> i64 {
    if n <= 1 {
        return 1;
    }

    return n * factorial(n - 1);
}

fn main() {
    println(factorial(6));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "720");
}

#[test]
fn test_mutual_recursion() {
    let src = r#"
fn is_even(n: i64) -> bool {
    if n == 0 {
        return true;
    }

    return is_odd(n - 1);
}

fn is_odd(n: i64) -> bool {
    if n == 0 {
        return false;
    }

    return is_even(n - 1);
}

fn main() {
    println(is_even(8));
    println(is_odd(8));
    println(is_odd(9));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "true\nfalse\ntrue\n");
}

#[test]
fn test_pub_function() {
    let src = r#"
pub fn double(x: i64) -> i64 {
    return x * 2;
}
fn main() {
    println(double(21));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_forward_function_call() {
    let src = r#"
fn main() {
    println(triple(14));
}

fn triple(x: i64) -> i64 {
    return x * 3;
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "42");
}

#[test]
fn test_return_from_both_if_branches() {
    let src = r#"
fn sign(n: i64) -> i64 {
    if n < 0 {
        return -1;
    } else {
        return 1;
    }
}

fn main() {
    println(sign(-8));
    println(sign(8));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "-1\n1\n");
}

#[test]
fn test_function_returning_bool_used_as_if_condition() {
    let src = r#"
fn in_range(value: i64, min: i64, max: i64) -> bool {
    return value >= min && value <= max;
}

fn main() {
    if in_range(7, 1, 10) {
        println(1);
    } else {
        println(0);
    }
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1");
}

#[test]
fn test_void_function_without_explicit_return() {
    let src = r#"
fn emit_twice(value: i64) {
    println(value);
    println(value);
}

fn main() {
    emit_twice(9);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "9\n9\n");
}

#[test]
fn test_void_function_and_early_return() {
    let src = r#"
fn emit(flag: bool) {
    if flag {
        println(1);
        return;
    }

    println(0);
}

fn main() {
    emit(true);
    emit(false);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out, "1\n0\n");
}

// ── `else if` chains (willow-hg6e) ───────────────────────────────────────────

fn run_ok(src: &str) -> String {
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed: {out}");
    out
}

// Perspective 16: the first true rung runs and later conditions are never
// evaluated.
#[test]
fn else_if_16_first_true_rung_wins_and_short_circuits() {
    let src = r#"
fn probe(label: i64, result: bool) -> bool {
    println(label);
    return result;
}

fn main() {
    if probe(1, false) {
        println(10);
    } else if probe(2, true) {
        println(20);
    } else if probe(3, true) {
        println(30);
    } else {
        println(40);
    }
}
"#;
    assert_eq!(run_ok(src), "1\n2\n20\n");
}

// Perspective 17: when no rung matches, the final else runs.
#[test]
fn else_if_17_falls_through_to_final_else() {
    let src = r#"
fn classify(n: i64) -> i64 {
    if n < 0 {
        return -1;
    } else if n == 0 {
        return 0;
    } else if n < 10 {
        return 1;
    } else {
        return 2;
    }
}

fn main() {
    println(classify(-4));
    println(classify(0));
    println(classify(7));
    println(classify(99));
}
"#;
    assert_eq!(run_ok(src), "-1\n0\n1\n2\n");
}

// Perspective 18: without a final else, no rung runs and control continues.
#[test]
fn else_if_18_no_final_else_runs_nothing() {
    let src = r#"
fn main() {
    let n = 5;
    if n == 1 {
        println(1);
    } else if n == 2 {
        println(2);
    }
    println(99);
}
"#;
    assert_eq!(run_ok(src), "99\n");
}

// Perspective 19: missing-return analysis accepts a ladder whose every rung
// and final else return (the function body ends at the ladder).
#[test]
fn else_if_19_all_rungs_returning_satisfies_missing_return() {
    let src = r#"
fn grade(n: i64) -> String {
    if n >= 90 {
        return "A";
    } else if n >= 80 {
        return "B";
    } else {
        return "C";
    }
}

fn main() {
    println(grade(95));
    println(grade(85));
    println(grade(5));
}
"#;
    assert_eq!(run_ok(src), "A\nB\nC\n");
}

// Perspective 20: a ladder with no final else can fall off the end, so E0205
// still fires.
#[test]
fn else_if_20_ladder_without_final_else_reports_missing_return() {
    let stderr = compile_error_stderr(
        r#"
fn grade(n: i64) -> i64 {
    if n >= 90 {
        return 1;
    } else if n >= 80 {
        return 2;
    }
}

fn main() {
    println(grade(1));
}
"#,
    );
    assert!(stderr.contains("E0205"), "{stderr}");
}

// Perspective 21: a binding introduced in one rung is scoped to that rung;
// sibling rungs may reuse the name with another type.
#[test]
fn else_if_21_rung_bindings_are_rung_scoped() {
    let src = r#"
fn main() {
    let n = 2;
    if n == 1 {
        let v = 10;
        println(v);
    } else if n == 2 {
        let v = "two";
        println(v);
    } else {
        let v = true;
        println(v);
    }
}
"#;
    assert_eq!(run_ok(src), "two\n");
    let stderr = compile_error_stderr(
        r#"
fn main() {
    if true {
        let a = 1;
    } else if false {
        println(a);
    }
}
"#,
    );
    assert!(stderr.contains("cannot find variable `a`"), "{stderr}");
}

// Perspective 22: assignments in a rung are visible after the ladder.
#[test]
fn else_if_22_mutation_in_rung_is_visible_after() {
    let src = r#"
fn main() {
    let mut total = 0;
    let mut i = 0;
    while i < 6 {
        if i % 3 == 0 {
            total = total + 100;
        } else if i % 3 == 1 {
            total = total + 10;
        } else {
            total = total + 1;
        }
        i = i + 1;
    }
    println(total);
}
"#;
    assert_eq!(run_ok(src), "222\n");
}

// Perspective 23: `break` and `continue` inside rungs target the enclosing loop.
#[test]
fn else_if_23_break_and_continue_in_rungs() {
    let src = r#"
fn main() {
    let mut i = 0;
    while true {
        i = i + 1;
        if i == 2 {
            continue;
        } else if i == 5 {
            break;
        } else {
            println(i);
        }
    }
    println(i);
}
"#;
    assert_eq!(run_ok(src), "1\n3\n4\n5\n");
}

// Perspective 24: `defer` inside rungs behaves exactly like the hand-written
// nested form.
#[test]
fn else_if_24_defer_matches_hand_written_nesting() {
    let sugar = r#"
fn f(n: i64) {
    defer println("fn");
    if n == 0 {
        defer println("zero");
        println("a");
    } else if n == 1 {
        defer println("one");
        println("b");
    } else {
        defer println("other");
        println("c");
    }
    println("end");
}

fn main() {
    f(0);
    f(1);
    f(2);
}
"#;
    let nested = r#"
fn f(n: i64) {
    defer println("fn");
    if n == 0 {
        defer println("zero");
        println("a");
    } else {
        if n == 1 {
            defer println("one");
            println("b");
        } else {
            defer println("other");
            println("c");
        }
    }
    println("end");
}

fn main() {
    f(0);
    f(1);
    f(2);
}
"#;
    let out = run_ok(sugar);
    assert_eq!(out, run_ok(nested));
    assert!(out.contains("b\n"), "{out}");
}

// Perspective 25: a non-bool condition in a later rung is a type error.
#[test]
fn else_if_25_non_bool_rung_condition_is_a_type_error() {
    let stderr = compile_error_stderr(
        r#"
fn main() {
    if false {
        println(1);
    } else if 42 {
        println(2);
    }
}
"#,
    );
    assert!(stderr.contains("bool"), "{stderr}");
}

// Perspective 26: `await` works inside a later rung of an async function.
#[test]
fn else_if_26_await_inside_rung() {
    let src = r#"
async fn value(n: i64) -> i64 {
    await sleep(1);
    return n;
}

async fn main() {
    let n = 3;
    if n == 1 {
        println(1);
    } else if n == 3 {
        let v = await value(30);
        println(v);
    } else {
        println(0);
    }
}
"#;
    assert_eq!(run_ok(src), "30\n");
}

// Perspective 27: rung conditions may be compound expressions.
#[test]
fn else_if_27_compound_conditions() {
    let src = r#"
fn pick(a: i64, b: i64) -> i64 {
    if a > 0 && b > 0 {
        return 1;
    } else if a > 0 || b > 0 {
        return 2;
    } else if !(a == b) {
        return 3;
    } else {
        return 4;
    }
}

fn main() {
    println(pick(1, 1));
    println(pick(1, -1));
    println(pick(-1, -2));
    println(pick(-1, -1));
}
"#;
    assert_eq!(run_ok(src), "1\n2\n3\n4\n");
}

// Perspective 28: heap values produced in rungs survive a collection.
#[test]
fn else_if_28_heap_values_across_rungs_survive_gc() {
    let src = r#"
fn label(n: i64) -> String {
    let mut out = "none";
    if n == 1 {
        out = "one";
    } else if n == 2 {
        gc_collect();
        out = "two";
    }
    gc_collect();
    return out;
}

fn main() {
    println(label(1));
    println(label(2));
    println(label(3));
}
"#;
    assert_eq!(run_ok(src), "one\ntwo\nnone\n");
}

// Perspective 29: release builds agree with debug builds.
#[test]
fn else_if_29_release_build_matches() {
    let src = r#"
fn classify(n: i64) -> i64 {
    if n < 0 {
        return -1;
    } else if n == 0 {
        return 0;
    } else {
        return 1;
    }
}

fn main() {
    println(classify(-3));
    println(classify(0));
    println(classify(3));
}
"#;
    let (out, ok) = compile_and_run_release(src);
    assert!(ok, "compilation failed: {out}");
    assert_eq!(out, "-1\n0\n1\n");
}
