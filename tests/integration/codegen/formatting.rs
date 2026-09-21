use super::*;

// ── Formatted panic + generalized format (willow-csax) ──────────────────────
// 20 perspectives (plus 10 spec-parser unit tests in src/interpolate.rs):
// 1 i64 {}, 2 f64 {}, 3 bool {}, 4 String {}, 5 multiple args in order,
// 6 brace escapes at runtime, 7 f64 precision placeholders still work,
// 8 formatted panic message + location, 9 panic call stack + non-zero exit,
// 10 one-arg panic with braces stays literal (back-compat), 11 formatted
// panic in a method, 12 in an async fn, 13 nested format inside panic args,
// 14 too many args rejected, 15 too few args rejected, 16 non-literal spec
// rejected, 17 unknown placeholder rejected, 18 non-printable arg rejected,
// 19 f64 placeholder with i64 arg rejected, 20 GC stress over many pieces
// (intermediate concat results stay rooted).

#[test]
fn interp_01_i64_display() {
    let (out, ok) = compile_and_run("fn main() { println(format(\"x = {}\", 42)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "x = 42\n");
}

#[test]
fn interp_02_f64_display() {
    let (out, ok) = compile_and_run("fn main() { println(format(\"v = {}\", 1.5)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "v = 1.5\n");
}

#[test]
fn interp_03_bool_display() {
    let (out, ok) = compile_and_run("fn main() { println(format(\"b = {}\", 1 < 2)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "b = true\n");
}

#[test]
fn interp_04_string_display() {
    let (out, ok) =
        compile_and_run("fn main() { let name = \"willow\"; println(format(\"hi {}\", name)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "hi willow\n");
}

#[test]
fn interp_05_multiple_args_in_order() {
    let (out, ok) = compile_and_run("fn main() { println(format(\"{} < {} < {}\", 1, 2, 3)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "1 < 2 < 3\n");
}

#[test]
fn interp_06_brace_escapes() {
    let (out, ok) = compile_and_run("fn main() { println(format(\"{{{}}}\", 5)); }");
    assert!(ok, "{out}");
    assert_eq!(out, "{5}\n");
}

#[test]
fn interp_07_f64_precision_placeholders() {
    let (out, ok) = compile_and_run(
        "fn main() { println(format(\"{:.6f}\", 3.14159265)); println(format(\"~{:.16f}~\", 1.5)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3.141593\n~1.5000000000000000~\n");
}

#[test]
fn interp_08_formatted_panic_message_and_location() {
    let (out, ok) = compile_and_run_check_exit("fn main() { panic(\"bad value: {}\", 42); }");
    assert!(!ok);
    assert!(out.contains("runtime panic: bad value: 42 at"), "{out}");
}

#[test]
fn interp_09_panic_stack_and_exit() {
    let (out, ok) = compile_and_run_check_exit(
        "fn boom(n: i64) { panic(\"n = {}\", n); }\nfn main() { boom(7); }",
    );
    assert!(!ok);
    assert!(out.contains("n = 7"), "{out}");
    assert!(out.contains("call stack"), "{out}");
}

#[test]
fn interp_10_one_arg_panic_braces_stay_literal() {
    // Back-compat: a single-argument panic never interpolates.
    let (out, ok) = compile_and_run_check_exit("fn main() { panic(\"100% {weird}\"); }");
    assert!(!ok);
    assert!(out.contains("runtime panic: 100% {weird}"), "{out}");
}

#[test]
fn interp_11_formatted_panic_in_method() {
    let (out, ok) = compile_and_run_check_exit(
        "class C { pub fn go(self, n: i64) { panic(\"C says {}\", n); } }\nfn main() { let c = new C(); c.go(3); }",
    );
    assert!(!ok);
    assert!(out.contains("C says 3"), "{out}");
}

#[test]
fn interp_12_formatted_panic_in_async_fn() {
    let (out, ok) = compile_and_run_check_exit(
        "async fn f(n: i64) -> i64 { panic(\"async {}\", n); }\nasync fn main() { println(await f(9)); }",
    );
    assert!(!ok);
    assert!(out.contains("async 9"), "{out}");
}

#[test]
fn interp_13_nested_format_inside_panic() {
    let (out, ok) =
        compile_and_run_check_exit("fn main() { panic(\"outer {}\", format(\"inner {}\", 1)); }");
    assert!(!ok);
    assert!(out.contains("outer inner 1"), "{out}");
}

#[test]
fn interp_14_too_many_args_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { panic(\"x = {}\", 1, 2); }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E1401"), "{stderr}");
}

#[test]
fn interp_15_too_few_args_rejected() {
    let (ok, stderr) =
        compile_with_compiler_env("fn main() { let s = format(\"{} {}\", 1); }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E1401"), "{stderr}");
}

#[test]
fn interp_16_non_literal_spec_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn main() { let spec = \"x = {}\"; let s = format(spec, 1); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("string literal"), "{stderr}");
}

#[test]
fn interp_17_unknown_placeholder_rejected() {
    let (ok, stderr) = compile_with_compiler_env("fn main() { let s = format(\"{:x}\", 1); }", &[]);
    assert!(!ok);
    assert!(stderr.contains("E1401"), "{stderr}");
}

#[test]
fn interp_18_non_printable_arg_rejected() {
    let (ok, stderr) = compile_with_compiler_env(
        "import std::collections::Array;\nfn main() { let xs: Array<i64> = [1]; let s = format(\"{}\", xs); }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("cannot format"), "{stderr}");
}

#[test]
fn interp_19_f64_placeholder_i64_arg_rejected() {
    let (ok, stderr) =
        compile_with_compiler_env("fn main() { let s = format(\"{:.6f}\", 1); }", &[]);
    assert!(!ok);
    assert!(stderr.contains("expected `f64`"), "{stderr}");
}

#[test]
fn interp_20_many_pieces_under_gc_stress() {
    // Every concat allocates; under alloc-stress each one collects — the
    // rooted intermediates must survive.
    let (out, ok) = compile_and_run_gc_stress(
        "fn main() { println(format(\"{} {} {} {} {} {}\", 1, true, 2.5, \"s\", 3, false)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1 true 2.5 s 3 false\n");
}
