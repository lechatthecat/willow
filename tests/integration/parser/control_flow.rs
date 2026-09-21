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
