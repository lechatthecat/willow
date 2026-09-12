use super::support::compile_and_run;

#[test]
fn lir_constant_folding_preserves_evaluation_and_recovery() {
    let source = r#"
fn base() -> i64 { print("base "); return 2; }
fn divide() {
    defer { match recover() { Some(_) => println("recovered"), None => {} } }
    println(10 / (2 - 2));
}
fn main() {
    println(base() ** (1 + 2));
    println(base() ** (3 - 3));
    println(base() * (3 - 3));
    println(9223372036854775807 + 1);
    println((-7) / (1 + 2));
    println((-7) % (1 + 2));
    println(false && (base() == 2));
    println(true || (base() == 2));
    divide();
}
"#;
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "base 8\nbase 1\nbase 0\n-9223372036854775808\n-2\n-1\nfalse\ntrue\nrecovered\n"
    );
}

#[test]
fn lir_dead_results_preserve_faults_and_call_effects() {
    let (out, ok) = compile_and_run(
        r#"
fn effect() -> i64 { println("called"); return 2; }
fn discarded(x: i64, zero: i64) {
    defer { match recover() { Some(_) => println("recovered"), None => {} } }
    (x * 2) + 1;
    effect() * 0;
    (x + 1) / zero;
    println("unreachable");
}
fn main() { discarded(5, 0); println("done"); }
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "called\nrecovered\ndone\n");
}

#[test]
fn scalar_loop_unrolling_preserves_partial_groups_and_float_order() {
    let (out, ok) = compile_and_run(
        r#"
fn digits(n: i64) -> i64 {
    let mut i = 0;
    let mut value = 0;
    while i < n { value = value * 10 + i; i = i + 1; }
    return value;
}
fn floating(n: i64) -> f64 {
    let mut i = 0;
    let mut sum = 0.0;
    let mut sign = 1.0;
    let mut denominator = 1.0;
    while i < n {
        sum = sum + sign / denominator;
        sign = -sign;
        denominator = denominator * 2.0;
        i = i + 1;
    }
    return sum;
}
fn ordered(n: i64) -> f64 {
    let mut i = 0;
    let mut sum = 0.0;
    while i < n {
        sum = sum + 10000000000000000.0;
        sum = sum - 10000000000000000.0;
        sum = sum + 1.0;
        i = i + 1;
    }
    return sum;
}
fn main() {
    println(digits(0)); println(digits(1)); println(digits(3));
    println(digits(4)); println(digits(5)); println(digits(9));
    println(floating(0)); println(floating(5)); println(floating(8));
    println(ordered(9));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(
        out,
        "0\n0\n12\n123\n1234\n12345678\n0.0\n0.6875\n0.6640625\n1\n"
    );
}

#[test]
fn tail_recursion_handles_deep_calls_and_parallel_arguments() {
    let (out, ok) = compile_and_run(
        r#"
fn sum(n: i64, acc: i64) -> i64 {
    if n == 0 { return acc; }
    return sum(n - 1, acc + n);
}
fn swap(n: i64, a: i64, b: i64) -> i64 {
    if n == 0 { return a * 10 + b; }
    return swap(n - 1, b, a);
}
fn fact(n: i64, acc: i64) -> i64 {
    if n == 0 { return acc; }
    return fact(n - 1, acc * n);
}
fn fib(n: i64) -> i64 {
    if n < 2 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn branches(n: i64, acc: f64, flip: bool) -> f64 {
    let value = acc + 1.0;
    if n == 0 { return value; }
    if flip { return branches(n - 1, value, false); }
    return branches(n - 1, value + 1.0, true);
}
fn cleanup(n: i64) -> i64 {
    defer { println(n); }
    if n == 0 { return n; }
    return cleanup(n - 1);
}
fn main() {
    println(sum(1000000, 0));
    println(swap(1000001, 2, 7));
    println(fact(10, 1));
    println(fib(10));
    println(branches(1000000, 0.0, true));
    println(cleanup(2));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "500000500000\n72\n3628800\n55\n1500001\n0\n1\n2\n0\n");
}

#[test]
fn recursive_inlining_preserves_arguments_base_cases_and_float_order() {
    let (out, ok) = compile_and_run(
        r#"
fn fib(n: i64) -> i64 { if n < 2 { return n; } return fib(n-1) + fib(n-2); }
fn swapped(n: i64, a: i64, b: i64) -> i64 {
    if n == 0 { return a - b; }
    return swapped(n-1, b, a) + a;
}
fn ordered(n: i64) -> f64 {
    if n == 0 { return 1.0; }
    return ((ordered(n-1) + 10000000000000000.0) - 10000000000000000.0) + 1.0;
}
fn multiple(n: i64, flag: bool) -> i64 {
    if n < 2 { return n; }
    if flag { return multiple(n-1, false) + 2; }
    return multiple(n-2, true) - 3;
}
fn main() {
    println(fib(-2)); println(fib(0)); println(fib(1)); println(fib(2));
    println(fib(3)); println(fib(4)); println(fib(5)); println(fib(20));
    println(swapped(7, 2, 9)); println(swapped(8, 2, 9));
    println(ordered(10)); println(multiple(10, true));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "-2\n0\n1\n1\n2\n3\n5\n6765\n42\n37\n1\n-2\n");
}
