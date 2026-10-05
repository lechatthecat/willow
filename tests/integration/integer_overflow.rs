//! Integer overflow policy (willow-jz15.14, docs/decisions/0012).
//!
//! Debug builds check `+ - * **` and prefix `-` on `i64` and raise a
//! recoverable panic; release builds wrap. `wrapping_*` / `checked_*` behave
//! the same in both modes.
//!
//! Perspectives:
//!   1  `+` overflow panics in debug with the operator and its location
//!   2  `-` overflow panics in debug
//!   3  `*` overflow (positive) panics in debug
//!   4  `i64::MIN * -1` panics in debug
//!   5  negation of `i64::MIN` panics in debug
//!   6  `**` overflow (dynamic exponent) panics in debug
//!   7  `**` overflow (literal exponent) panics in debug
//!   8  constant-foldable overflow is not folded away: debug panics
//!   9  compound assignment `+= -= *=` is checked
//!  10  release builds wrap every one of the above
//!  11  overflow panics are recoverable, and `defer` runs
//!  12  boundary values that fit never panic (incl. `(-2)**63`, `3**39`)
//!  13  loops that reach `i64::MAX` exactly do not panic
//!  14  `wrapping_*` wrap identically in debug and release
//!  15  `checked_*` return `None`/`Some` identically in debug and release
//!  16  methods on literal and chained receivers
//!  17  an uncaught overflow exits non-zero and reports `file:line:col`
//!  18  the faulting leaf keeps its call-stack frame (no debug inlining)
//!  19  an unused overflowing binding still panics in debug only
//!  20  overflow inside an `async fn` faults its task
//!  21  overflow inside a class method
//!  22  `f64` arithmetic is unaffected
//!  23  checker rejects wrong arity / argument types / misuse of the result
//!  24  debug objects reference the overflow raiser; release and
//!      explicit-method-only programs do not
//!  25  division overflow (`MIN / -1`) still panics in release

use std::fs;
use std::process::Command;

use crate::support::*;

/// Build `source` in debug or release mode and run it.
/// Returns `(stdout, stderr, exited successfully)`.
fn run_mode(source: &str, release: bool) -> (String, String, bool) {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_overflow_{id}.wi"));
    let bin_path = temp_path(format!("willow_overflow_{id}"));
    fs::write(&src_path, source).unwrap();
    let mut args = vec!["build", &src_path, "-o", &bin_path];
    if release {
        args.push("--release");
    }
    let built = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(&args)
        .output()
        .expect("failed to run compiler");
    let _ = fs::remove_file(&src_path);
    assert!(
        built.status.success(),
        "compile failed: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    let out = Command::new(&bin_path)
        .output()
        .expect("failed to run binary");
    remove_output_artifacts(&bin_path);
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.success(),
    )
}

const MAX: &str = "9223372036854775807";
const MIN: &str = "-9223372036854775808";

/// `(guarded statement, debug output, release output)`. Each statement runs in
/// its own scope with a `recover()` defer, so one program covers every case.
const CASES: &[(&str, &str, &str)] = &[
    // 1
    ("println(add(max, 1));", "integer overflow: `+`", MIN),
    ("println(add(min, 0 - 1));", "integer overflow: `+`", MAX),
    // 2
    ("println(sub(min, 1));", "integer overflow: `-`", MAX),
    ("println(sub(max, 0 - 1));", "integer overflow: `-`", MIN),
    ("println(sub(0, min));", "integer overflow: `-`", MIN),
    // 3
    ("println(mul(max, 2));", "integer overflow: `*`", "-2"),
    (
        "println(mul(4294967296, 4294967296));",
        "integer overflow: `*`",
        "0",
    ),
    // 4
    ("println(mul(min, 0 - 1));", "integer overflow: `*`", MIN),
    // 5
    (
        "println(neg(min));",
        "integer overflow: negation of `i64::MIN`",
        MIN,
    ),
    (
        "println(-min);",
        "integer overflow: negation of `i64::MIN`",
        MIN,
    ),
    // 6
    ("println(pow(2, 63));", "integer overflow: `**`", MIN),
    (
        "println(pow(3, 40));",
        "integer overflow: `**`",
        "-6289078614652622815",
    ),
    ("println(pow(2, 64));", "integer overflow: `**`", "0"),
    // 7
    ("println(2 ** 63);", "integer overflow: `**`", MIN),
    ("println(max ** 2);", "integer overflow: `**`", "1"),
    // 8
    (
        "println(9223372036854775807 + 1);",
        "integer overflow: `+`",
        MIN,
    ),
    ("println(max + 1);", "integer overflow: `+`", MIN),
    (
        "println(-(0 - 9223372036854775807 - 1));",
        "integer overflow: negation of `i64::MIN`",
        MIN,
    ),
    // 9
    (
        "let mut x = max; x += 1; println(x);",
        "integer overflow: `+`",
        MIN,
    ),
    (
        "let mut x = min; x -= 1; println(x);",
        "integer overflow: `-`",
        MAX,
    ),
    (
        "let mut x = max; x *= 3; println(x);",
        "integer overflow: `*`",
        "9223372036854775805",
    ),
];

fn overflow_matrix_source() -> String {
    let mut source = String::from(
        "fn add(a: i64, b: i64) -> i64 { return a + b; }\n\
         fn sub(a: i64, b: i64) -> i64 { return a - b; }\n\
         fn mul(a: i64, b: i64) -> i64 { return a * b; }\n\
         fn neg(a: i64) -> i64 { return -a; }\n\
         fn pow(a: i64, b: i64) -> i64 { return a ** b; }\n\
         fn main() {\n\
             let max = 9223372036854775807;\n\
             let min = 0 - max - 1;\n",
    );
    for (statement, _, _) in CASES {
        source.push_str(
            "    if true {\n        \
             defer match recover() { Some(info) => println(info.message), None => {} }\n        ",
        );
        source.push_str(statement);
        source.push_str("\n    }\n");
    }
    source.push_str("    println(\"end\");\n}\n");
    source
}

fn expected(release: bool) -> String {
    let mut out = String::new();
    for (_, debug, wrapped) in CASES {
        out.push_str(if release { wrapped } else { debug });
        out.push('\n');
    }
    out.push_str("end\n");
    out
}

#[test]
fn integer_overflow_01_to_09_debug_panics_recoverably() {
    // Perspectives 1-9 and 11: every case is caught by its own `recover()`.
    let (out, err, ok) = run_mode(&overflow_matrix_source(), false);
    assert!(ok, "{out}\n{err}");
    assert_eq!(out, expected(false));
}

#[test]
fn integer_overflow_10_release_wraps() {
    let (out, err, ok) = run_mode(&overflow_matrix_source(), true);
    assert!(ok, "{out}\n{err}");
    assert_eq!(out, expected(true));
}

#[test]
fn integer_overflow_11_defer_runs_before_recovery() {
    let source = "fn add(a: i64, b: i64) -> i64 {\n\
                      defer println(\"cleanup\");\n\
                      return a + b;\n\
                  }\n\
                  fn main() {\n\
                      if true {\n\
                          defer match recover() { Some(info) => println(\"recovered: \" + info.message), None => {} }\n\
                          println(add(9223372036854775807, 1));\n\
                      }\n\
                      println(\"after\");\n\
                  }\n";
    let (out, err, ok) = run_mode(source, false);
    assert!(ok, "{out}\n{err}");
    assert_eq!(out, "cleanup\nrecovered: integer overflow: `+`\nafter\n");
}

#[test]
fn integer_overflow_12_13_values_that_fit_never_panic() {
    let source = "fn add(a: i64, b: i64) -> i64 { return a + b; }\n\
                  fn sub(a: i64, b: i64) -> i64 { return a - b; }\n\
                  fn mul(a: i64, b: i64) -> i64 { return a * b; }\n\
                  fn neg(a: i64) -> i64 { return -a; }\n\
                  fn pow(a: i64, b: i64) -> i64 { return a ** b; }\n\
                  fn main() {\n\
                      let max = 9223372036854775807;\n\
                      let min = 0 - max - 1;\n\
                      println(add(max - 1, 1));\n\
                      println(sub(min + 1, 1));\n\
                      println(add(min, max));\n\
                      println(sub(0 - 1, max));\n\
                      println(mul(min, 1));\n\
                      println(mul(0, min));\n\
                      println(mul(3037000499, 3037000499));\n\
                      println(mul(0 - 4294967296, 2147483648));\n\
                      println(neg(max));\n\
                      println(pow(0 - 2, 63));\n\
                      println(pow(2, 62));\n\
                      println(pow(3, 39));\n\
                      println(pow(0 - 3, 39));\n\
                      println((0 - 2) ** 63);\n\
                      println(pow(1, max));\n\
                      println(pow(0 - 1, max));\n\
                      let mut n = 0;\n\
                      for i in (max - 3)..max { n = n + 1; }\n\
                      let mut j = max - 5;\n\
                      while j < max { j += 1; n = n + 1; }\n\
                      println(n);\n\
                      println(j);\n\
                  }\n";
    let expected = "9223372036854775807\n-9223372036854775808\n-1\n-9223372036854775808\n\
                    -9223372036854775808\n0\n9223372030926249001\n-9223372036854775808\n\
                    -9223372036854775807\n-9223372036854775808\n4611686018427387904\n\
                    4052555153018976267\n-4052555153018976267\n-9223372036854775808\n1\n-1\n\
                    8\n9223372036854775807\n";
    for release in [false, true] {
        let (out, err, ok) = run_mode(source, release);
        assert!(ok, "release={release}: {out}\n{err}");
        assert_eq!(out, expected, "release={release}");
    }
}

#[test]
fn integer_overflow_14_16_explicit_methods_ignore_build_mode() {
    let source = "fn show(value: Option<i64>) -> String {\n\
                      match value {\n\
                          Some(n) => { return \"Some(\" + n.toString() + \")\"; }\n\
                          None => { return \"None\"; }\n\
                      }\n\
                  }\n\
                  fn main() {\n\
                      let max = 9223372036854775807;\n\
                      let min = 0 - max - 1;\n\
                      println(max.wrapping_add(1));\n\
                      println(min.wrapping_add(0 - 1));\n\
                      println(min.wrapping_sub(1));\n\
                      println(max.wrapping_mul(2));\n\
                      println(min.wrapping_mul(0 - 1));\n\
                      println(min.wrapping_neg());\n\
                      println(5.wrapping_neg());\n\
                      println(40.wrapping_add(2));\n\
                      println(show(max.checked_add(1)));\n\
                      println(show(max.checked_add(0 - 1)));\n\
                      println(show(min.checked_sub(1)));\n\
                      println(show(min.checked_sub(0 - 1)));\n\
                      println(show(min.checked_mul(0 - 1)));\n\
                      println(show(3037000499.checked_mul(3037000499)));\n\
                      println(show(min.checked_neg()));\n\
                      println(show(max.checked_neg()));\n\
                      println(show(7.checked_mul(6)));\n\
                      println(max.wrapping_add(1).wrapping_sub(1));\n\
                      println(show((max - 1).wrapping_add(2).checked_sub(1)));\n\
                  }\n";
    let expected = "-9223372036854775808\n9223372036854775807\n9223372036854775807\n-2\n\
                    -9223372036854775808\n-9223372036854775808\n-5\n42\n\
                    None\nSome(9223372036854775806)\nNone\nSome(-9223372036854775807)\nNone\n\
                    Some(9223372030926249001)\nNone\nSome(-9223372036854775807)\nSome(42)\n\
                    9223372036854775807\nNone\n";
    for release in [false, true] {
        let (out, err, ok) = run_mode(source, release);
        assert!(ok, "release={release}: {out}\n{err}");
        assert_eq!(out, expected, "release={release}");
    }
}

#[test]
fn integer_overflow_17_18_uncaught_reports_location_and_frame() {
    let source = "fn d(a: i64, b: i64) -> i64 { return a * b; }\n\
                  fn mid(a: i64) -> i64 { return d(a, a) + 1; }\n\
                  fn main() {\n\
                      println(\"start\");\n\
                      println(mid(4294967296));\n\
                  }\n";
    let (out, err, ok) = run_mode(source, false);
    assert!(!ok, "debug overflow must exit non-zero: {out}");
    assert_eq!(out, "start\n");
    assert!(
        err.contains("runtime panic: integer overflow: `*` at "),
        "{err}"
    );
    assert!(err.contains(".wi:1:40"), "location of `a * b`: {err}");
    assert!(err.contains("0: d at "), "leaf frame must survive: {err}");
    assert!(err.contains("1: mid at "), "{err}");

    let (out, err, ok) = run_mode(source, true);
    assert!(ok, "release wraps: {out}\n{err}");
    assert_eq!(out, "start\n1\n");
}

#[test]
fn integer_overflow_19_unused_binding_is_still_checked_in_debug() {
    let source = "fn main() {\n\
                      let max = 9223372036854775807;\n\
                      if true {\n\
                          defer match recover() { Some(info) => println(info.message), None => {} }\n\
                          let unused = max + 1;\n\
                          println(\"not reached in debug\");\n\
                      }\n\
                      println(\"end\");\n\
                  }\n";
    let (out, err, ok) = run_mode(source, false);
    assert!(ok, "{out}\n{err}");
    assert_eq!(out, "integer overflow: `+`\nend\n");
    let (out, err, ok) = run_mode(source, true);
    assert!(ok, "{out}\n{err}");
    assert_eq!(out, "not reached in debug\nend\n");
}

#[test]
fn integer_overflow_20_async_task_faults() {
    let source = "async fn bump(a: i64) -> i64 {\n\
                      await sleep(1);\n\
                      return a + 1;\n\
                  }\n\
                  async fn main() {\n\
                      let task = bump(9223372036854775807);\n\
                      println(await task);\n\
                  }\n";
    let (out, err, ok) = run_mode(source, false);
    assert!(!ok, "{out}");
    assert!(err.contains("integer overflow: `+` at "), "{err}");
    assert!(err.contains("async bump"), "{err}");
    let (out, err, ok) = run_mode(source, true);
    assert!(ok, "{out}\n{err}");
    assert_eq!(out, "-9223372036854775808\n");
}

#[test]
fn integer_overflow_21_22_methods_and_floats() {
    let source = "class Counter {\n\
                      pub n: i64;\n\
                      pub fn twice(self) -> i64 { return self.n * 2; }\n\
                  }\n\
                  fn main() {\n\
                      println(new Counter(21).twice());\n\
                      let mut big = 1.0;\n\
                      for i in 0..400 { big = big * 10.0; }\n\
                      println(big > 1.0);\n\
                      println(0.0 - big < 0.0);\n\
                      if true {\n\
                          defer match recover() { Some(info) => println(info.message), None => {} }\n\
                          println(new Counter(9223372036854775807).twice());\n\
                      }\n\
                  }\n";
    let (out, err, ok) = run_mode(source, false);
    assert!(ok, "{out}\n{err}");
    assert_eq!(out, "42\ntrue\ntrue\ninteger overflow: `*`\n");
    let (out, err, ok) = run_mode(source, true);
    assert!(ok, "{out}\n{err}");
    assert_eq!(out, "42\ntrue\ntrue\n-2\n");
}

#[test]
fn integer_overflow_23_checker_rejects_misuse() {
    assert_compile_error_contains(
        "fn main() { let a = 1; println(a.checked_add()); }",
        &["checked_add"],
    );
    assert_compile_error_contains(
        "fn main() { let a = 1; println(a.wrapping_add(1.5)); }",
        &["f64"],
    );
    assert_compile_error_contains(
        "fn main() { let a = 1; println(a.wrapping_neg(1)); }",
        &["wrapping_neg"],
    );
    assert_compile_error_contains(
        "fn main() { let a = 1; let b: i64 = a.checked_mul(2); println(b); }",
        &["Option"],
    );
    assert_compile_error_contains(
        "fn main() { let f = 1.0; println(f.wrapping_add(1.0)); }",
        &["wrapping_add"],
    );
}

#[test]
fn integer_overflow_24_raiser_is_debug_only() {
    const CHECKED: &str = "fn add(a: i64, b: i64) -> i64 { return a + b; }\n\
                           fn main() { println(add(1, 2)); }\n";
    let debug = compile_and_collect_relocation_targets_mode(CHECKED, &[], false);
    assert!(
        debug.iter().any(|name| name == "willow_int_overflow_panic"),
        "{debug:?}"
    );
    let release = compile_and_collect_relocation_targets_mode(CHECKED, &[], true);
    assert!(
        !release
            .iter()
            .any(|name| name == "willow_int_overflow_panic"),
        "{release:?}"
    );

    // Generated code is linear in the number of checked sites: one raiser
    // call per site in debug, none in release.
    for sites in [4, 32] {
        let mut source = String::new();
        for i in 0..sites {
            source.push_str(&format!("fn f{i}(a: i64) -> i64 {{ return a + {i}; }}\n"));
        }
        source.push_str("fn main() {\n");
        for i in 0..sites {
            source.push_str(&format!("    println(f{i}(1));\n"));
        }
        source.push_str("}\n");
        let count = |release| {
            compile_and_collect_relocation_targets_mode(&source, &[], release)
                .iter()
                .filter(|name| *name == "willow_int_overflow_panic")
                .count()
        };
        assert_eq!(count(false), sites, "debug sites={sites}");
        assert_eq!(count(true), 0, "release sites={sites}");
    }

    const EXPLICIT: &str = "fn f(a: i64, b: i64) -> i64 { return a.wrapping_mul(b); }\n\
                            fn main() { println(f(3, 4)); println(1.checked_add(2).is_some()); }\n";
    let explicit = compile_and_collect_relocation_targets_mode(EXPLICIT, &[], false);
    assert!(
        !explicit
            .iter()
            .any(|name| name == "willow_int_overflow_panic"),
        "explicit methods never panic: {explicit:?}"
    );
}

#[test]
fn integer_overflow_25_division_overflow_still_panics_in_release() {
    let source = "fn div(a: i64, b: i64) -> i64 { return a / b; }\n\
                  fn main() { let min = 0 - 9223372036854775807 - 1; println(div(min, 0 - 1)); }\n";
    let (out, err, ok) = run_mode(source, true);
    assert!(!ok, "{out}");
    assert!(err.contains("integer overflow"), "{err}");
}
