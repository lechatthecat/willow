use super::*;

// ── String content equality ───────────────────────────────────
// == / != on String compare CONTENT via willow_string_eq, never pointers.
// 20 perspectives: 1 concat==literal (the original bug), 2 != inverse,
// 3 different strings unequal, 4 empty==empty, 5 empty vs non-empty,
// 6 literal==literal, 7 same variable both sides, 8 case sensitivity,
// 9 multibyte UTF-8, 10 prefix is not equal, 11 Option::None predicate,
// 12 Option::Some predicate pair, 13 comparison drives if, 14 comparison
// drives while exit, 15 == inside lambda, 16 interpolated/format result ==
// literal, 17 GC stress (rhs allocation during comparison, lhs rooted),
// 18 chained comparisons via bools, 19 array element == literal (break
// scenario from string-content comparison), 20 long strings differing at the last byte.

#[test]
fn streq_01_concat_vs_literal() {
    let (out, ok) = compile_and_run("fn main() { let a = \"c\" + \"!\"; println(a == \"c!\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_02_ne_inverse() {
    let (out, ok) = compile_and_run(
        "fn main() { let a = \"c\" + \"!\"; println(a != \"c!\"); println(a != \"c?\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\ntrue\n");
}

#[test]
fn streq_03_different_unequal() {
    let (out, ok) = compile_and_run("fn main() { println(\"x\" == \"y\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn streq_04_empty_eq_empty() {
    let (out, ok) = compile_and_run("fn main() { let e = \"\"; println(e == \"\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_05_empty_vs_nonempty() {
    let (out, ok) = compile_and_run("fn main() { println(\"\" == \"a\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn streq_06_literal_literal() {
    let (out, ok) = compile_and_run("fn main() { println(\"abc\" == \"abc\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_07_same_variable() {
    let (out, ok) = compile_and_run("fn main() { let s = \"q\" + \"r\"; println(s == s); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_08_case_sensitive() {
    let (out, ok) = compile_and_run("fn main() { println(\"Abc\" == \"abc\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn streq_09_multibyte_utf8() {
    let (out, ok) = compile_and_run(
        "fn main() { let s = \"日\" + \"本\"; println(s == \"日本\"); println(s == \"日体\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn streq_10_prefix_not_equal() {
    let (out, ok) = compile_and_run("fn main() { let s = \"ab\" + \"\"; println(s == \"abc\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "false\n");
}

#[test]
fn streq_11_option_none_predicate() {
    let (out, ok) =
        compile_and_run("fn main() { let s: Option<String> = None; println(s.is_none()); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_12_option_some_predicates() {
    let (out, ok) = compile_and_run(
        "fn main() { let t: Option<String> = Some(\"hi\"); println(t.is_none()); println(t.is_some()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "false\ntrue\n");
}

#[test]
fn streq_13_drives_if() {
    let (out, ok) = compile_and_run(
        "fn main() { let w = \"wil\" + \"low\"; if w == \"willow\" { println(1); } else { println(0); } }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "1\n");
}

#[test]
fn streq_14_drives_while_exit() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut s = \"\"; let mut n = 0; while s != \"aaa\" { s = s + \"a\"; n = n + 1; } println(n); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n");
}

#[test]
fn streq_15_inside_lambda() {
    let (out, ok) = compile_and_run(
        "fn main() { let f = |s: String| s == \"ok\"; println(f(\"o\" + \"k\")); println(f(\"no\")); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\n");
}

#[test]
fn streq_16_format_result() {
    let (out, ok) =
        compile_and_run("fn main() { let s = format(\"n = {}\", 5); println(s == \"n = 5\"); }");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_17_gc_stress_rhs_allocates() {
    // rhs concat allocates during the comparison; lhs must stay rooted.
    let (out, ok) = compile_and_run_gc_stress(
        "fn main() { let a = \"x\" + \"y\"; println(a == (\"x\" + \"y\")); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_18_chained_bools() {
    let (out, ok) = compile_and_run(
        "fn main() { let a = \"p\" + \"q\"; let b = a == \"pq\" && \"r\" == \"r\"; println(b); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn streq_19_array_element_break_scenario() {
    // The string-content comparison discovery case: break on a content match now fires.
    let (out, ok) = compile_and_run(
        "import std::collections::Array;\nfn main() { let xs: Array<String> = [\"a\", \"b\", \"c\", \"d\"]; for s in xs { let t = s + \"!\"; if t == \"c!\" { break; } println(t); } println(\"done\"); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "a!\nb!\ndone\n");
}

#[test]
fn streq_20_long_last_byte_differs() {
    let (out, ok) = compile_and_run(
        "fn main() { let mut a = \"\"; let mut b = \"\"; let mut i = 0; while i < 200 { a = a + \"z\"; b = b + \"z\"; i = i + 1; } println(a == b); println((a + \"1\") == (b + \"2\")); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\nfalse\n");
}

// Review fix for brk_18's loop shape: in an async while, `continue` must go
// through the safepoint back edge (dedicated cont block), not straight to
// the header — a continue BEFORE any await in the body must still let the
// scheduler run (no busy-loop past suspension points).

#[test]
fn brk_21_async_while_continue_before_await() {
    let (out, ok) = compile_and_run(
        "async fn side() -> i64 { await sleep(5); return 7; }\nasync fn main() { let t = side(); let mut i = 0; let mut s = 0; while i < 500 { i = i + 1; if i % 2 == 0 { s = s + 1; continue; } await sleep(0); } println(s); println(await t); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "250\n7\n");
}

#[test]
fn brk_22_async_while_continue_only_still_terminates() {
    // EVERY iteration continues before the await: the loop must still
    // terminate and the safepoint edge must not corrupt the frame state.
    let (out, ok) = compile_and_run(
        "async fn main() { let mut i = 0; while i < 100 { i = i + 1; if true { continue; } await sleep(1); } println(i); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "100\n");
}

// Ordering perspectives: all four operators in both directions and equality;
// empty/prefix, case, UTF-8 (two/three/four bytes), no normalization, NUL,
// concat, format, variables, parameters/returns, lambda, branch, loop,
// collections, short circuit, GC-stress RHS allocation and long common prefix.
#[test]
fn string_ordering_matrix() {
    let pairs = [
        ("", ""),
        ("", "a"),
        ("a", ""),
        ("a", "a"),
        ("a", "b"),
        ("b", "a"),
        ("a", "aa"),
        ("aa", "a"),
        ("Z", "a"),
        ("é", "ê"),
        ("z", "é"),
        ("水", "火"),
        ("😀", "😁"),
        ("é", "é"),
        ("a\0b", "a\0c"),
    ];
    let mut source = String::from("fn main() {");
    let mut expected = String::new();
    for (a, b) in pairs {
        for (op, result) in [("<", a < b), ("<=", a <= b), (">", a > b), (">=", a >= b)] {
            source.push_str(&format!("println({a:?} {op} {b:?});"));
            expected.push_str(&format!("{result}\n"));
        }
    }
    source.push('}');
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    assert_eq!(out, expected);
}

#[test]
fn string_ordering_contexts() {
    let (out, ok) = compile_and_run(
        r#"
import std::collections::Array;
fn less(a: String, b: String) -> bool { return a < b; }
fn main() {
    let a = "a" + "b";
    println(a <= "ab");
    println(format("{}", a) >= "ab");
    println(less(a, "ac"));
    let f = |s: String| s > "aa";
    println(f(a));
    if a < "ac" { println(true); }
    let mut s = "";
    while s < "aaa" { s = s + "a"; }
    println(s == "aaa");
    let xs: Array<String> = ["ab", "ac"];
    for x in xs { println(x >= a); }
    println(a < "ac" && a > "aa");
    let mut long = "";
    let mut i = 0;
    while i < 256 { long = long + "x"; i = i + 1; }
    println((long + "a") < (long + "b"));
}
"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n".repeat(10));
}

#[test]
fn string_ordering_gc_stress() {
    let (out, ok) = compile_and_run_gc_stress(
        r#"fn main() { let a = "x" + "a"; println(a < ("x" + "b")); println(("x" + "a") >= a); }"#,
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\ntrue\n");
}

#[test]
fn string_ordering_rejects_mixed_and_arithmetic_operands() {
    for expression in [r#""a" < 1"#, r#"1 >= "a""#, r#""a" - "b""#, "true < false"] {
        let source = format!("fn main() {{ println({expression}); }}");
        let project = TestProject::new("string_ordering_invalid", &[("main.wi", &source)]);
        let output = project.check("main.wi");
        assert!(!output.status.success(), "{expression}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("E0202"));
    }
}

#[test]
fn string_ordering_generated_calls_scale_linearly() {
    for count in [1, 4, 16] {
        let mut source = String::new();
        for i in 0..count {
            source.push_str(&format!(
                "fn compare_{i}(a: String, b: String) -> bool {{ return a < b; }}"
            ));
        }
        source.push_str("fn main() {");
        for i in 0..count {
            source.push_str(&format!("println(compare_{i}(\"a\", \"b\"));"));
        }
        source.push('}');
        let calls = compile_and_collect_relocation_targets_all(&source, &[]);
        let comparisons = calls
            .iter()
            .filter(|name| *name == "willow_string_cmp")
            .count();
        assert_eq!(comparisons, count);
        println!("comparisons={count} emitted_runtime_calls={comparisons}");
    }
}

#[test]
fn string_ordering_example_runs() {
    let (out, ok) = compile_file_and_run("example/string_compare.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "true\n".repeat(8));
}
