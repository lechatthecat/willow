use super::*;

#[test]
fn divguard_01_div_zero_message_lir() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(f(1, 0)); }",
    );
    assert!(out.contains("runtime panic: division by zero at"), "{out}");
}

#[test]
fn divguard_02_div_zero_message_reaches_stderr() {
    let source = "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(f(1, 0)); }";
    let (out, ok) = compile_with_env_and_run(source, &[]);
    assert!(!ok, "expected panic");
    let _ = out; // stdout empty; the message goes to stderr (checked via exit path below)
    let (all, ok2) = compile_and_run_check_exit(source);
    assert!(!ok2);
    assert!(all.contains("division by zero"), "{all}");
}

#[test]
fn divguard_03_rem_zero_message() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a % b; }\nfn main() { println(f(1, 0)); }",
    );
    assert!(out.contains("runtime panic: remainder by zero at"), "{out}");
}

#[test]
fn divguard_04_min_div_neg1_overflow() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { let a = -9223372036854775807 - 1; println(f(a, -1)); }",
    );
    assert!(out.contains("integer overflow: `i64::MIN / -1`"), "{out}");
}

#[test]
fn divguard_05_min_rem_neg1_overflow() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a % b; }\nfn main() { let a = -9223372036854775807 - 1; println(f(a, -1)); }",
    );
    assert!(out.contains("integer overflow: `i64::MIN % -1`"), "{out}");
}

#[test]
fn divguard_06_nonzero_exit() {
    let (_, ok) = compile_and_run_check_exit(
        "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(f(1, 0)); }",
    );
    assert!(!ok);
}

#[test]
fn divguard_07_call_stack_frame() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(f(1, 0)); }",
    );
    assert!(out.contains("call stack"), "{out}");
}

#[test]
fn divguard_08_normal_div_unaffected_lir() {
    let (out, ok) = compile_and_run("fn main() { println(10 / 3); println(10 % 3); }");
    assert!(ok);
    assert_eq!(out, "3\n1\n");
}

#[test]
fn divguard_09_normal_div_unaffected_ast() {
    let (out, ok) =
        compile_with_env_and_run("fn main() { println(10 / 3); println(10 % 3); }", &[]);
    assert!(ok);
    assert_eq!(out, "3\n1\n");
}

#[test]
fn divguard_10_runtime_divisor() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut d = 5; let mut t = 0; while d > 0 { t = t + 100 / d; d = d - 1; } println(t); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "228\n"); // 20+25+33+50+100
}

#[test]
fn divguard_11_zero_inside_loop() {
    let out = div_panic_output(
        "fn main() { let mut d = 2; while d >= 0 { println(10 / d); d = d - 1; } }",
    );
    assert!(out.contains("division by zero"), "{out}");
    assert!(
        out.contains("5\n10\n"),
        "loop iterations before the panic: {out}"
    );
}

#[test]
fn divguard_12_guard_in_class_method() {
    let out = div_panic_output(
        "class C { pub fn ratio(self, a: i64, b: i64) -> i64 { return a / b; } }\nfn main() { let c = new C(); println(c.ratio(1, 0)); }",
    );
    assert!(out.contains("division by zero"), "{out}");
}

#[test]
fn divguard_13_guard_in_async_fn() {
    let out = div_panic_output(
        "async fn f(a: i64, b: i64) -> i64 { return a / b; }\nasync fn main() { println(await f(1, 0)); }",
    );
    assert!(out.contains("division by zero"), "{out}");
}

#[test]
fn divguard_14_f64_div_zero_not_trapped() {
    let (out, ok) = compile_and_run("fn main() { let x = 1.0 / 0.0; println(x > 100.0); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn divguard_15_constant_operands_guarded() {
    let out = div_panic_output("fn main() { let z = 0; println(1 / z); }");
    assert!(out.contains("division by zero"), "{out}");
}

#[test]
fn divguard_16_computed_zero_divisor() {
    let out = div_panic_output(
        "fn f(b: i64) -> i64 { return 10 / (b - b); }\nfn main() { println(f(3)); }",
    );
    assert!(out.contains("division by zero"), "{out}");
}

#[test]
fn divguard_17_rem_in_lir_loop() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut t = 0; for i in 1..5 { t = t + 10 % i; } println(t); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n"); // 0+0+1+2
}

#[test]
fn divguard_18_message_names_source_file() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64) -> i64 { return a / b; }\nfn main() { println(f(1, 0)); }",
    );
    assert!(
        out.contains(".wi:"),
        "location with file name expected: {out}"
    );
}

#[test]
fn divguard_19_zero_mid_chain() {
    let out = div_panic_output(
        "fn f(a: i64, b: i64, c: i64) -> i64 { return a / b / c; }\nfn main() { println(f(100, 0, 5)); }",
    );
    assert!(out.contains("division by zero"), "{out}");
}

#[test]
fn divguard_20_negative_dividend_unaffected() {
    let (out, ok) = compile_and_run("fn main() { println(-7 / 2); println(-7 % 2); }");
    assert!(ok, "{out}");
    assert_eq!(out, "-3\n-1\n");
}
