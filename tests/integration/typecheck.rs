use super::support::*;

#[test]
fn match_01_bool_true_false_arms() {
    let src = r#"
fn describe(b: bool) -> String {
    return match b {
        true => "yes",
        false => "no",
    };
}
fn main() {
    println(describe(true));
    println(describe(false));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "yes\nno");
}

#[test]
fn match_02_i64_with_wildcard() {
    let src = r#"
fn classify(n: i64) -> String {
    return match n {
        0 => "zero",
        1 => "one",
        _ => "other",
    };
}
fn main() {
    println(classify(0));
    println(classify(1));
    println(classify(99));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "zero\none\nother");
}

#[test]
fn match_03_fieldless_enum() {
    let src = r#"
enum Color {
    Red,
    Green,
    Blue,
}
fn name(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Green => "green",
        Color::Blue => "blue",
    };
}
fn main() {
    println(name(Color::Red));
    println(name(Color::Green));
    println(name(Color::Blue));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "red\ngreen\nblue");
}

#[test]
fn match_04_wildcard_arm() {
    let src = r#"
fn sign(n: i64) -> String {
    return match n {
        0 => "zero",
        _ => "nonzero",
    };
}
fn main() {
    println(sign(0));
    println(sign(5));
    println(sign(-3));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "zero\nnonzero\nnonzero");
}

#[test]
fn match_05_binding_pattern() {
    let src = r#"
fn double_or_zero(n: i64) -> i64 {
    return match n {
        0 => 0,
        v => v + v,
    };
}
fn main() {
    println(double_or_zero(0));
    println(double_or_zero(5));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "0\n10");
}

#[test]
fn match_06_as_expression_assigned_to_variable() {
    let src = r#"
enum Dir {
    Up,
    Down,
}
fn main() {
    let d = Dir::Up;
    let label = match d {
        Dir::Up => "up",
        Dir::Down => "down",
    };
    println(label);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "up");
}

#[test]
fn match_07_negative_integer_pattern() {
    let src = r#"
fn describe(n: i64) -> String {
    return match n {
        -1 => "minus one",
        0 => "zero",
        _ => "other",
    };
}
fn main() {
    println(describe(-1));
    println(describe(0));
    println(describe(3));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "minus one\nzero\nother");
}

#[test]
fn match_08_enum_passed_as_function_argument() {
    let src = r#"
enum Season {
    Spring,
    Summer,
    Autumn,
    Winter,
}
fn season_msg(s: Season) -> String {
    return match s {
        Season::Spring => "bloom",
        Season::Summer => "hot",
        Season::Autumn => "fall",
        Season::Winter => "cold",
    };
}
fn main() {
    println(season_msg(Season::Winter));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "cold");
}

#[test]
fn match_09_match_example_file_compiles_and_outputs_green() {
    let (out, ok) = compile_file_and_run("example/match_color.wi");
    assert!(ok, "match_color.wi failed to compile");
    assert_eq!(out.trim(), "green");
}

#[test]
fn match_10_bool_exhaustiveness_error_missing_false() {
    let src = r#"
fn f(b: bool) -> String {
    return match b {
        true => "yes",
    };
}
fn main() { println(f(true)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected exhaustiveness error for missing false arm"
    );
}

#[test]
fn match_11_enum_exhaustiveness_error_missing_variant() {
    let src = r#"
enum Color { Red, Green, Blue, }
fn name(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Green => "green",
    };
}
fn main() { println(name(Color::Red)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected exhaustiveness error for missing Blue variant"
    );
}

#[test]
fn match_12_incompatible_arm_types_error() {
    let src = r#"
fn f(b: bool) -> i64 {
    return match b {
        true => 1,
        false => "nope",
    };
}
fn main() { println(f(true)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected incompatible arm types error"
    );
}

#[test]
fn match_13_unknown_enum_variant_in_pattern_error() {
    let src = r#"
enum Color { Red, Green, }
fn f(c: Color) -> String {
    return match c {
        Color::Red => "red",
        Color::Purple => "purple",
    };
}
fn main() { println(f(Color::Red)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected unknown variant error for Color::Purple"
    );
}

#[test]
fn match_14_i64_non_exhaustive_error_missing_wildcard() {
    let src = r#"
fn f(n: i64) -> String {
    return match n {
        0 => "zero",
        1 => "one",
    };
}
fn main() { println(f(0)); }
"#;
    assert!(
        expect_compile_error(src),
        "expected non-exhaustive error for i64 without wildcard"
    );
}

#[test]
fn match_15_enum_variant_in_let_binding() {
    let src = r#"
enum State {
    Active,
    Inactive,
}
fn main() {
    let s = State::Active;
    let r = match s {
        State::Active => "on",
        State::Inactive => "off",
    };
    println(r);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "on");
}

#[test]
fn match_16_match_in_return_with_enum_multiple_values() {
    let src = r#"
enum Priority {
    Low,
    Medium,
    High,
}
fn score(p: Priority) -> i64 {
    return match p {
        Priority::Low => 1,
        Priority::Medium => 5,
        Priority::High => 10,
    };
}
fn main() {
    println(score(Priority::Low));
    println(score(Priority::Medium));
    println(score(Priority::High));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1\n5\n10");
}

#[test]
fn match_17_bool_match_both_arms_covered() {
    let src = r#"
fn to_int(b: bool) -> i64 {
    return match b {
        true => 1,
        false => 0,
    };
}
fn main() {
    println(to_int(true));
    println(to_int(false));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "1\n0");
}

#[test]
fn match_18_enum_with_single_variant() {
    let src = r#"
enum Unit { Only, }
fn describe(u: Unit) -> String {
    return match u {
        Unit::Only => "just one",
    };
}
fn main() {
    println(describe(Unit::Only));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "just one");
}

#[test]
fn match_19_wildcard_after_some_integer_patterns() {
    let src = r#"
fn greet(n: i64) -> String {
    return match n {
        1 => "one",
        2 => "two",
        _ => "many",
    };
}
fn main() {
    println(greet(1));
    println(greet(2));
    println(greet(100));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "one\ntwo\nmany");
}

#[test]
fn match_20_enum_variant_as_function_result() {
    let src = r#"
enum Toggle {
    On,
    Off,
}
fn flip(t: Toggle) -> Toggle {
    return match t {
        Toggle::On => Toggle::Off,
        Toggle::Off => Toggle::On,
    };
}
fn describe(t: Toggle) -> String {
    return match t {
        Toggle::On => "on",
        Toggle::Off => "off",
    };
}
fn main() {
    let t = Toggle::On;
    let t2 = flip(t);
    println(describe(t2));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "compilation failed");
    assert_eq!(out.trim(), "off");
}

#[test]
fn test_leibniz_pi_release_completes_within_150ms() {
    use std::time::Instant;

    let id = unique_test_id();
    let bin_path = temp_path(format!("willow_leibniz_perf_{}", id));

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let status = Command::new(compiler)
        .args([
            "build",
            "example/leibniz_pi.wi",
            "--release",
            "-o",
            &bin_path,
        ])
        .stderr(Stdio::null())
        .status()
        .expect("failed to run compiler");
    assert!(status.success(), "leibniz_pi.wi failed to compile");

    let mut best_ms = u128::MAX;
    for _ in 0..3 {
        let start = Instant::now();
        let out = Command::new(&bin_path)
            .output()
            .expect("failed to run binary");
        let elapsed_ms = start.elapsed().as_millis();

        assert!(out.status.success(), "binary exited with error");
        assert_eq!(
            out.stdout.trim_ascii(),
            b"3.141592663589326",
            "output mismatch"
        );
        best_ms = best_ms.min(elapsed_ms);
    }

    remove_output_artifacts(&bin_path);

    let max_ms = if cfg!(windows) { 1000 } else { 250 };
    assert!(
        best_ms < max_ms,
        "leibniz_pi release build took {best_ms}ms at best — expected < {max_ms}ms (performance regression?)"
    );
}

// ── Option<T> and Result<T,E> ─────────────────────────────────────────────────

#[test]
fn test_option_some_and_none_i64() {
    let src = r#"
fn safe_div(a: i64, b: i64) -> Option<i64> {
    if b == 0 {
        return Option::None;
    }
    return Option::Some(a / b);
}

fn main() {
    let r1 = match safe_div(10, 2) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    let r2 = match safe_div(7, 0) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option<i64> should compile and run");
    assert_eq!(out, "5\n-1\n");
}

#[test]
fn test_option_none_in_function_return() {
    let src = r#"
fn first_positive(a: i64, b: i64) -> Option<i64> {
    if a > 0 {
        return Option::Some(a);
    }
    if b > 0 {
        return Option::Some(b);
    }
    return Option::None;
}

fn main() {
    let r1 = match first_positive(-1, 5) {
        Option::Some(v) => v,
        Option::None => 0,
    };
    let r2 = match first_positive(-3, -7) {
        Option::Some(v) => v,
        Option::None => 0,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option::None in function return should compile");
    assert_eq!(out, "5\n0\n");
}

#[test]
fn test_option_map_via_match() {
    let src = r#"
fn double_opt(opt: Option<i64>) -> Option<i64> {
    return match opt {
        Option::Some(v) => Option::Some(v * 2),
        Option::None => Option::None,
    };
}

fn main() {
    let r1 = match double_opt(Option::Some(21)) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    let r2 = match double_opt(Option::None) {
        Option::Some(v) => v,
        Option::None => -1,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option::map-like function should compile");
    assert_eq!(out, "42\n-1\n");
}

#[test]
fn test_result_ok_and_err_i64_string() {
    let src = r#"
fn parse_positive(n: i64) -> Result<i64, String> {
    if n <= 0 {
        return Result::Err("non-positive");
    }
    return Result::Ok(n * 10);
}

fn main() {
    let v1 = match parse_positive(5) {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    };
    let v2 = match parse_positive(-3) {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    };
    println(v1);
    println(v2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Result<i64,String> should compile and run");
    assert_eq!(out, "50\n-1\n");
}

#[test]
fn test_result_err_message_extracted() {
    let src = r#"
fn parse_even(n: i64) -> Result<i64, String> {
    if n % 2 != 0 {
        return Result::Err("not even");
    }
    return Result::Ok(n / 2);
}

fn main() {
    let msg = match parse_even(7) {
        Result::Ok(_) => "ok",
        Result::Err(e) => e,
    };
    println(msg);
    let val = match parse_even(8) {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    };
    println(val);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Result Err payload extraction should compile");
    assert_eq!(out, "not even\n4\n");
}

#[test]
fn test_option_f64_payload() {
    let src = r#"
fn safe_sqrt(x: f64) -> Option<f64> {
    if x < 0.0 {
        return Option::None;
    }
    return Option::Some(pow(x, 0.5));
}

fn main() {
    let r1 = match safe_sqrt(9.0) {
        Option::Some(v) => v,
        Option::None => -1.0,
    };
    let r2 = match safe_sqrt(-4.0) {
        Option::Some(v) => v,
        Option::None => -1.0,
    };
    println(r1);
    println(r2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option<f64> payload should compile and run");
    assert_eq!(out, "3\n-1\n");
}

// ── ? operator ────────────────────────────────────────────────────────────────

#[test]
fn test_try_propagate_extracts_ok_payload() {
    let src = r#"
fn safe_div(a: i64, b: i64) -> Result<i64, String> {
    if b == 0 { return Result::Err("zero"); }
    return Result::Ok(a / b);
}

fn halve(n: i64) -> Result<i64, String> {
    return Result::Ok(safe_div(n, 2)?);
}

fn main() {
    let r = halve(10);
    let v = match r {
        Result::Ok(x) => x,
        Result::Err(_) => -1,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "? operator should compile and run");
    assert_eq!(out, "5\n");
}

#[test]
fn test_try_propagate_returns_err_early() {
    let src = r#"
fn fail() -> Result<i64, String> {
    return Result::Err("oops");
}

fn caller() -> Result<i64, String> {
    let v = fail()?;
    return Result::Ok(v + 1);
}

fn main() {
    let r = caller();
    let msg = match r {
        Result::Ok(_) => "ok",
        Result::Err(e) => e,
    };
    println(msg);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "? early return should compile and run");
    assert_eq!(out, "oops\n");
}

#[test]
fn test_try_propagate_chains_multiple_calls() {
    let src = r#"
fn parse(s: String) -> Result<i64, String> {
    if s == "10" { return Result::Ok(10); }
    if s == "20" { return Result::Ok(20); }
    return Result::Err("bad input");
}

fn sum_two(a: String, b: String) -> Result<i64, String> {
    let x = parse(a)?;
    let y = parse(b)?;
    return Result::Ok(x + y);
}

fn main() {
    let r1 = sum_two("10", "20");
    let v1 = match r1 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v1);
    let r2 = sum_two("10", "bad");
    let v2 = match r2 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "chained ? should compile and run");
    assert_eq!(out, "30\n-1\n");
}

#[test]
fn test_option_try_propagate_extracts_some_payload() {
    let src = r#"
fn maybe(n: i64) -> Option<i64> {
    if n > 0 { return Option::Some(n); }
    return Option::None;
}

fn doubled(n: i64) -> Option<i64> {
    let v = maybe(n)?;
    return Option::Some(v * 2);
}

fn main() {
    let a = doubled(21);
    let av = match a { Option::Some(v) => v, Option::None => -1, };
    println(av);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option ? should extract Some payload");
    assert_eq!(out, "42\n");
}

#[test]
fn test_option_try_propagate_returns_none_early() {
    let src = r#"
fn maybe(n: i64) -> Option<i64> {
    if n > 0 { return Option::Some(n); }
    return Option::None;
}

fn doubled(n: i64) -> Option<i64> {
    let v = maybe(n)?;
    return Option::Some(v * 2);
}

fn main() {
    let a = doubled(-1);
    let av = match a { Option::Some(v) => v, Option::None => -1, };
    println(av);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option ? should propagate None");
    assert_eq!(out, "-1\n");
}

#[test]
fn test_option_try_propagate_preserves_f64_payload_type() {
    let src = r#"
fn maybe(flag: bool) -> Option<f64> {
    if flag { return Option::Some(2.5); }
    return Option::None;
}

fn add(flag: bool) -> Option<f64> {
    let v = maybe(flag)?;
    return Option::Some(v + 0.5);
}

fn main() {
    let a = add(true);
    let av = match a { Option::Some(v) => v, Option::None => -1.0, };
    println(av);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option ? should preserve f64 payloads");
    assert_eq!(out, "3\n");
}

#[test]
fn test_try_propagate_on_non_result_reports_e1806() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: i64 = 42;
    let y = x?;
    println(y);
}
"#,
        &[
            "error[E1806]",
            "requires `Result<T,E>` or `Option<T>`",
            "found `i64`",
        ],
    );
}

#[test]
fn test_try_propagate_in_non_result_function_reports_e1807() {
    assert_compile_error_contains(
        r#"
fn get() -> Result<i64, String> {
    return Result::Ok(1);
}

fn main() {
    let v = get()?;
    println(v);
}
"#,
        &[
            "error[E1807]",
            "can only be used inside a function returning `Result",
        ],
    );
}

#[test]
fn test_ternary_and_try_propagate_coexist() {
    let src = r#"
fn ok_or(n: i64) -> Result<i64, String> {
    if n > 0 { return Result::Ok(n); }
    return Result::Err("non-positive");
}

fn scaled(n: i64) -> Result<i64, String> {
    let v = ok_or(n)?;
    let factor = v > 5 ? 10 : 1;
    return Result::Ok(v * factor);
}

fn main() {
    let r1 = scaled(7);
    let v1 = match r1 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v1);
    let r2 = scaled(3);
    let v2 = match r2 { Result::Ok(v) => v, Result::Err(_) => -1, };
    println(v2);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "? and ternary ? should coexist");
    assert_eq!(out, "70\n3\n");
}

// ── Option / Result GC tracing ────────────────────────────────────────────────

#[test]
fn test_option_some_class_payload_survives_gc_collect() {
    let src = r#"
class Node {
    pub value: i64;
    pub fn get(self) -> i64 { return self.value; }
}

fn make_some(v: i64) -> Option<Node> {
    let n = new Node(v);
    return Option::Some(n);
}

fn main() {
    let opt = make_some(42);
    gc_collect();
    let v = match opt {
        Option::Some(n) => n.get(),
        Option::None => -1,
    };
    println(v);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option<Node> should compile and run");
    assert_eq!(out, "42\ntrue\n", "Node payload must survive gc_collect");
}

#[test]
fn test_option_none_traces_nothing() {
    let src = r#"
class Node { pub value: i64; }

fn empty() -> Option<Node> {
    return Option::None;
}

fn main() {
    let opt = empty();
    gc_collect();
    let v = match opt {
        Option::Some(n) => n.value,
        Option::None => 0,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option::None should compile and run");
    assert_eq!(out, "0\n");
}

#[test]
fn test_result_ok_class_payload_survives_gc_collect() {
    let src = r#"
class Node { pub value: i64; }

fn make_ok(v: i64) -> Result<Node, String> {
    let n = new Node(v);
    return Result::Ok(n);
}

fn main() {
    let r = make_ok(99);
    gc_collect();
    let v = match r {
        Result::Ok(n) => n.value,
        Result::Err(_) => -1,
    };
    println(v);
    println(gc_allocated_bytes() > 0);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Result<Node,String> should compile and run");
    assert_eq!(
        out, "99\ntrue\n",
        "Node payload in Ok must survive gc_collect"
    );
}

#[test]
fn test_option_some_unrooted_option_collected_after_use() {
    let src = r#"
class Node { pub value: i64; }

fn alloc_and_use() -> i64 {
    let n = new Node(7);
    let opt = Option::Some(n);
    let v = match opt {
        Option::Some(nd) => nd.value,
        Option::None => -1,
    };
    return v;
}

fn main() {
    let v = alloc_and_use();
    println(v);
    gc_collect();
    println(gc_allocated_bytes());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "Option wrapping class should compile and run");
    assert_eq!(
        out, "7\n0\n",
        "Option and Node should be collected after use"
    );
}

// ── Option / Result exhaustiveness ────────────────────────────────────────────

#[test]
fn test_option_match_missing_none_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let opt: Option<i64> = Option::Some(1);
    let v = match opt {
        Option::Some(x) => x,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Option::None` not covered"],
    );
}

#[test]
fn test_option_match_missing_some_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let opt: Option<i64> = Option::None;
    let v = match opt {
        Option::None => 0,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Option::Some` not covered"],
    );
}

#[test]
fn test_option_match_wildcard_arm_is_exhaustive() {
    let src = r#"
fn main() {
    let opt: Option<i64> = Option::Some(42);
    let v = match opt {
        Option::Some(x) => x,
        _ => 0,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "wildcard arm should satisfy exhaustiveness");
    assert_eq!(out, "42\n");
}

#[test]
fn test_result_match_missing_err_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let r: Result<i64, String> = Result::Ok(1);
    let v = match r {
        Result::Ok(x) => x,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Result::Err` not covered"],
    );
}

#[test]
fn test_result_match_missing_ok_reports_e1202() {
    assert_compile_error_contains(
        r#"
fn main() {
    let r: Result<i64, String> = Result::Err("bad");
    let v = match r {
        Result::Err(e) => 0,
    };
    println(v);
}
"#,
        &["error[E1202]", "variant `Result::Ok` not covered"],
    );
}

#[test]
fn test_result_match_wildcard_arm_is_exhaustive() {
    let src = r#"
fn parse(n: i64) -> Result<i64, String> {
    if n < 0 { return Result::Err("negative"); }
    return Result::Ok(n);
}

fn main() {
    let v = match parse(5) {
        Result::Ok(x) => x,
        _ => -1,
    };
    println(v);
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "wildcard arm satisfies exhaustiveness for Result");
    assert_eq!(out, "5\n");
}

#[test]
fn test_option_unknown_variant_reports_e1801() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Option::Maybe(1);
}
"#,
        &["error[E1801]", "unknown variant `Maybe` in `Option`"],
    );
}

#[test]
fn test_result_unknown_variant_reports_e1801() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Result::Value(1);
}
"#,
        &["error[E1801]", "unknown variant `Value` in `Result`"],
    );
}

// ── E180x type-inference and `?` diagnostics (willow-aff.3) ────────────────
// Acceptance criteria from requirements/requirements_option_result.md:
//   E1801 — cannot infer `T` for `Option::None`
//   E1803 — cannot infer `E` for `Result::Ok` / cannot infer `T` for `Result::Err`
//   E1805 — `?` error type mismatch
//   E1806 — `?` applied to a non-Result/non-Option value
//   E1807 — `?` in a function that does not return the matching wrapper
// (Non-exhaustive match for Option/Result is reported generically as E1202;
//  see the test_*_match_missing_* tests above.)

// Perspective 1: bare `Option::None` without annotation cannot infer `T`.
#[test]
fn test_e1801_bare_none_cannot_infer_t() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Option::None;
    println(1);
}
"#,
        &[
            "error[E1801]",
            "cannot infer type parameter `T` for `Option::None`",
            "type annotation required",
        ],
    );
}

// Perspective 2: the inference error also fires for `let mut`.
#[test]
fn test_e1801_bare_none_let_mut_cannot_infer_t() {
    assert_compile_error_contains(
        r#"
fn main() {
    let mut x = Option::None;
    println(1);
}
"#,
        &["error[E1801]", "cannot infer type parameter `T`"],
    );
}

// Perspective 3: bare `Result::Ok(v)` cannot infer the error type `E`.
#[test]
fn test_e1803_bare_ok_cannot_infer_error_type() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Result::Ok(10);
    println(1);
}
"#,
        &[
            "error[E1803]",
            "cannot infer error type `E` for `Result::Ok`",
        ],
    );
}

// Perspective 4: bare `Result::Err(e)` cannot infer the success type `T`.
#[test]
fn test_e1803_bare_err_cannot_infer_success_type() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = Result::Err("boom");
    println(1);
}
"#,
        &[
            "error[E1803]",
            "cannot infer success type `T` for `Result::Err`",
        ],
    );
}

// Perspective 5: annotation resolves `Option::None` — no diagnostic.
#[test]
fn test_e1801_annotation_resolves_none() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.is_none());
}
"#,
    );
    assert!(ok, "annotated None must compile");
    assert_eq!(out, "true\n");
}

// Perspective 6: annotation resolves `Result::Ok` — no diagnostic, runs.
#[test]
fn test_e1803_annotation_resolves_ok() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(10);
    println(x.unwrap());
}
"#,
    );
    assert!(ok, "annotated Ok must compile");
    assert_eq!(out, "10\n");
}

// Perspective 7: annotation resolves `Result::Err` — no diagnostic.
#[test]
fn test_e1803_annotation_resolves_err() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Err("nope");
    println(x.is_err());
}
"#,
    );
    assert!(ok, "annotated Err must compile");
    assert_eq!(out, "true\n");
}

// Perspective 8: `Option::Some(v)` infers `T` from the payload — no diagnostic.
#[test]
fn test_e1801_some_infers_t_no_annotation() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(7);
    println(x.unwrap());
}
"#,
    );
    assert!(ok, "Some(7) must infer T=i64");
    assert_eq!(out, "7\n");
}

// Perspective 9: a `Void` placeholder reaching a binding through a method
// chain is benign and must NOT trigger E1803 (guards against over-reporting).
#[test]
fn test_e1803_not_reported_through_method_chain() {
    let (out, ok) = compile_and_run(
        r#"
fn add_five(v: i64) -> Result<i64, String> {
    return Result::Ok(v + 5);
}

fn main() {
    let chained = Result::Ok(10).and_then(add_five);
    println(chained.unwrap());
}
"#,
    );
    assert!(ok, "method-chain result must not trigger E1803");
    assert_eq!(out, "15\n");
}

// Perspective 10: `Option::None` as a direct return is resolved by the return
// type — no diagnostic.
#[test]
fn test_e1801_none_as_return_is_resolved() {
    let (out, ok) = compile_and_run(
        r#"
fn empty() -> Option<i64> {
    return Option::None;
}

fn main() {
    println(empty().is_none());
}
"#,
    );
    assert!(ok, "None as return must compile");
    assert_eq!(out, "true\n");
}

// Perspective 11: `?` propagating a mismatched error type reports E1805.
#[test]
fn test_e1805_question_error_type_mismatch() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn consumer() -> Result<i64, i64> {
    let v = source()?;
    return Result::Ok(v);
}

fn main() {}
"#,
        &[
            "error[E1805]",
            "error type mismatch",
            "but `?` propagates `String`",
        ],
    );
}

// Perspective 12: `?` with matching error types compiles and runs end-to-end.
#[test]
fn test_e1805_matching_error_types_ok() {
    let (out, ok) = compile_and_run(
        r#"
fn source(n: i64) -> Result<i64, String> {
    if n < 0 { return Result::Err("neg"); }
    return Result::Ok(n);
}

fn consumer(n: i64) -> Result<i64, String> {
    let v = source(n)?;
    return Result::Ok(v * 2);
}

fn main() {
    let r = consumer(21);
    println(r.unwrap());
}
"#,
    );
    assert!(ok, "matching error types must compile");
    assert_eq!(out, "42\n");
}

// Perspective 13: `?` on a `bool` reports E1806.
#[test]
fn test_e1806_question_on_bool() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    let b = true;
    let x = b?;
    return Result::Ok(1);
}

fn main() {}
"#,
        &[
            "error[E1806]",
            "requires `Result<T,E>` or `Option<T>`",
            "found `bool`",
        ],
    );
}

// Perspective 14: `?` on an `Option` inside a Result-returning function is
// rejected because no Option-to-Result conversion is defined.
#[test]
fn test_e1807_question_on_option_in_result_function() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    let o: Option<i64> = Option::Some(1);
    let x = o?;
    return Result::Ok(x);
}

fn main() {}
"#,
        &[
            "error[E1807]",
            "`?` on `Option<T>` can only be used inside a function returning `Option<U>`",
            "found `Result<i64, String>`",
        ],
    );
}

// Perspective 15: `?` on a `String` reports E1806.
#[test]
fn test_e1806_question_on_string() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    let s = "hello";
    let x = s?;
    return Result::Ok(1);
}

fn main() {}
"#,
        &[
            "error[E1806]",
            "requires `Result<T,E>` or `Option<T>`",
            "found `String`",
        ],
    );
}

// Perspective 16: `?` inside a `void` function reports E1807.
#[test]
fn test_e1807_question_in_void_function() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn main() {
    let v = source()?;
    println(v);
}
"#,
        &[
            "error[E1807]",
            "can only be used inside a function returning `Result",
            "found `void`",
        ],
    );
}

// Perspective 17: `?` inside an `Option`-returning function reports E1807.
#[test]
fn test_e1807_question_in_option_function() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn wrapped() -> Option<i64> {
    let v = source()?;
    return Option::Some(v);
}

fn main() {}
"#,
        &["error[E1807]", "found `Option<i64>`"],
    );
}

// Perspective 18: `?` inside an `i64`-returning function reports E1807.
#[test]
fn test_e1807_question_in_i64_function() {
    assert_compile_error_contains(
        r#"
fn source() -> Result<i64, String> {
    return Result::Ok(1);
}

fn doubled() -> i64 {
    let v = source()?;
    return v * 2;
}

fn main() {}
"#,
        &["error[E1807]", "found `i64`"],
    );
}

// Perspective 19: too many arguments to a variant constructor is source-aware
// (E0201 reports the expected and actual argument counts).
#[test]
fn test_variant_constructor_too_many_args_e0201() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: Option<i64> = Option::Some(1, 2);
    println(1);
}
"#,
        &[
            "error[E0201]",
            "`Option::Some` expects 1 argument(s), got 2",
        ],
    );
}

// Perspective 20: a payload type mismatch in a variant constructor is
// source-aware (reports the concrete instantiations).
#[test]
fn test_variant_constructor_payload_type_mismatch_e0201() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: Option<i64> = Option::Some(true);
    println(1);
}
"#,
        &[
            "error[E0201]",
            "expected `Option<i64>`",
            "found `Option<bool>`",
        ],
    );
}

// Perspective 21: a missing payload on a variant constructor is source-aware.
#[test]
fn test_variant_constructor_missing_payload_e0201() {
    assert_compile_error_contains(
        r#"
fn f() -> Result<i64, String> {
    return Result::Ok();
}

fn main() {}
"#,
        &["error[E0201]", "`Result::Ok` expects 1 argument(s), got 0"],
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Unqualified enum-variant CONSTRUCTION (`Ok(42)` vs `Result::Ok(42)`) resolved
// by expected type, slice 1: payload variants in let-annotation and call-arg
// positions (willow-60o.1). Fieldless variants, return position, and patterns
// are follow-up slices.
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn test_unqualified_variant_let_annotation_constructs() {
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Idle(i64),
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
        Status::Idle(n) => n + 1000,
    };
}

fn main() {
    let a: Status = Active(42);
    let b: Status = Idle(7);
    println(code(a));
    println(code(b));
}
"#,
    );
    assert!(
        ok,
        "unqualified variant construction (let) should compile and run"
    );
    assert_eq!(out, "42\n1007\n");
}

#[test]
fn test_unqualified_variant_call_argument_constructs() {
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Idle(i64),
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
        Status::Idle(n) => n + 1000,
    };
}

fn main() {
    println(code(Active(42)));
    println(code(Idle(7)));
}
"#,
    );
    assert!(
        ok,
        "unqualified variant construction (arg) should compile and run"
    );
    assert_eq!(out, "42\n1007\n");
}

#[test]
fn test_unqualified_variant_matches_qualified_construction() {
    // The unqualified form must build the same value as the qualified form.
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
    };
}

fn main() {
    let a: Status = Active(5);
    let b: Status = Status::Active(5);
    println(code(a) + code(b));
}
"#,
    );
    assert!(ok, "unqualified and qualified construction should agree");
    assert_eq!(out, "10\n");
}

#[test]
fn test_unqualified_variant_wrong_payload_type_reports_error() {
    assert_compile_error_contains(
        r#"
enum Status {
    Active(i64),
}

fn main() {
    let a: Status = Active(true);
}
"#,
        &["error[E0201]", "mismatched types"],
    );
}

#[test]
fn test_unqualified_variant_wrong_arity_reports_error() {
    assert_compile_error_contains(
        r#"
enum Status {
    Active(i64),
}

fn main() {
    let a: Status = Active(1, 2);
}
"#,
        &["error[E0201]", "takes 1 argument(s), got 2"],
    );
}

#[test]
fn test_unqualified_variant_requires_expected_enum_context() {
    // Type-directed: without an expected enum type, `Active(42)` is just an
    // unknown function call — confirms resolution is not global.
    assert_compile_error_contains(
        r#"
enum Status {
    Active(i64),
}

fn main() {
    let a = Active(42);
}
"#,
        &["error[E0350]"],
    );
}

#[test]
fn test_non_variant_call_in_enum_context_still_calls_function() {
    // A real function call in an expected-enum position must NOT be hijacked as
    // a variant (its name is not a variant of the enum).
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
}

fn make(n: i64) -> Status {
    return Status::Active(n);
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
    };
}

fn main() {
    let a: Status = make(9);
    println(code(a));
}
"#,
    );
    assert!(
        ok,
        "non-variant function call in enum context should still call the function"
    );
    assert_eq!(out, "9\n");
}

#[test]
fn test_unqualified_fieldless_variant_constructs() {
    // A fieldless variant (`Closed`) is a bare identifier, resolved in both
    // let-annotation and argument positions.
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
        Status::Closed => -1,
    };
}

fn main() {
    let a: Status = Active(42);
    let c: Status = Closed;
    println(code(a));
    println(code(c));
    println(code(Closed));
}
"#,
    );
    assert!(
        ok,
        "unqualified fieldless variant construction should compile and run"
    );
    assert_eq!(out, "42\n-1\n-1\n");
}

#[test]
fn test_unqualified_fieldless_variant_requires_expected_enum_context() {
    // Without an expected enum type, a bare `Closed` is an undefined name.
    assert_compile_error_contains(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn main() {
    let x = Closed;
}
"#,
        &["error[E0350]"],
    );
}

#[test]
fn test_local_variable_shadows_fieldless_variant_name() {
    // A local variable named like a fieldless variant takes precedence over the
    // variant when used as a value.
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn main() {
    let Closed = 7;
    println(Closed);
}
"#,
    );
    assert!(
        ok,
        "a local variable should shadow a fieldless variant name"
    );
    assert_eq!(out, "7\n");
}

#[test]
fn test_unqualified_variant_in_return_position_constructs() {
    // `return Active(n)` / `return Closed` resolve against the function's
    // return type.
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn make(n: i64) -> Status {
    if n < 0 {
        return Closed;
    }
    return Active(n);
}

fn code(s: Status) -> i64 {
    return match s {
        Status::Active(n) => n,
        Status::Closed => -1,
    };
}

fn main() {
    println(code(make(42)));
    println(code(make(-5)));
}
"#,
    );
    assert!(
        ok,
        "unqualified variant in return position should compile and run"
    );
    assert_eq!(out, "42\n-1\n");
}

#[test]
fn test_unqualified_generic_variant_result_and_option_construct() {
    // The headline case: `Ok`/`Err`/`Some`/`None` resolved against a generic
    // `Result<T, E>` / `Option<T>` expected type.
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let r: Result<i64, String> = Ok(42);
    let e: Result<i64, String> = Err("bad");
    let o: Option<i64> = Some(7);
    let n: Option<i64> = None;
    println(match r {
        Result::Ok(v) => v,
        Result::Err(_) => -1,
    });
    println(match e {
        Result::Ok(v) => v,
        Result::Err(_) => -2,
    });
    println(match o {
        Option::Some(v) => v,
        Option::None => -3,
    });
    println(match n {
        Option::Some(v) => v,
        Option::None => -4,
    });
}
"#,
    );
    assert!(
        ok,
        "unqualified generic variant construction should compile and run"
    );
    assert_eq!(out, "42\n-2\n7\n-4\n");
}

#[test]
fn test_unqualified_generic_variant_in_argument_and_return() {
    // Call-argument and return positions for a generic enum.
    let (out, ok) = compile_and_run(
        r#"
fn wrap(n: i64) -> Result<i64, String> {
    if n < 0 {
        return Err("negative");
    }
    return Ok(n);
}

fn unwrap_or(r: Result<i64, String>, d: i64) -> i64 {
    return match r {
        Result::Ok(v) => v,
        Result::Err(_) => d,
    };
}

fn main() {
    println(unwrap_or(wrap(10), 0));
    println(unwrap_or(wrap(-1), 99));
    println(unwrap_or(Ok(5), 0));
}
"#,
    );
    assert!(ok, "generic variant in arg/return should compile and run");
    assert_eq!(out, "10\n99\n5\n");
}

#[test]
fn test_unqualified_generic_variant_wrong_payload_type_reports_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let r: Result<i64, String> = Ok(true);
}
"#,
        &["error[E0201]", "mismatched types"],
    );
}

// ── Unqualified enum-variant PATTERNS in `match` (willow-60o.1) ──────────────

#[test]
fn test_unqualified_pattern_non_generic_payload_and_fieldless() {
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn code(s: Status) -> i64 {
    return match s {
        Active(n) => n,
        Closed => -1,
    };
}

fn main() {
    println(code(Active(42)));
    println(code(Closed));
}
"#,
    );
    assert!(
        ok,
        "unqualified non-generic variant patterns should compile and run"
    );
    assert_eq!(out, "42\n-1\n");
}

#[test]
fn test_unqualified_pattern_generic_result_and_option() {
    let (out, ok) = compile_and_run(
        r#"
fn unwrap_or(r: Result<i64, String>, d: i64) -> i64 {
    return match r {
        Ok(v) => v,
        Err(_) => d,
    };
}

fn first(o: Option<i64>) -> i64 {
    return match o {
        Some(v) => v,
        None => -1,
    };
}

fn main() {
    println(unwrap_or(Ok(42), 0));
    println(unwrap_or(Err("x"), 99));
    println(first(Some(7)));
    println(first(None));
}
"#,
    );
    assert!(
        ok,
        "unqualified generic variant patterns should compile and run"
    );
    assert_eq!(out, "42\n99\n7\n-1\n");
}

#[test]
fn test_unqualified_and_qualified_patterns_mix_in_one_match() {
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Idle(i64),
    Closed,
}

fn code(s: Status) -> i64 {
    return match s {
        Active(n) => n,            // unqualified
        Status::Idle(n) => n + 1000, // qualified
        Closed => -1,              // unqualified fieldless
    };
}

fn main() {
    println(code(Active(5)));
    println(code(Idle(5)));
    println(code(Closed));
}
"#,
    );
    assert!(ok, "mixing qualified and unqualified patterns should work");
    assert_eq!(out, "5\n1005\n-1\n");
}

#[test]
fn test_catch_all_binding_not_confused_with_variant() {
    // A binding whose name is not a variant of the scrutinee enum is still a
    // catch-all binding, not a variant pattern.
    let (out, ok) = compile_and_run(
        r#"
enum Status {
    Active(i64),
    Closed,
}

fn code(s: Status) -> i64 {
    return match s {
        Active(n) => n,
        other => -1,
    };
}

fn main() {
    println(code(Active(42)));
    println(code(Closed));
}
"#,
    );
    assert!(ok, "a non-variant binding name should remain a catch-all");
    assert_eq!(out, "42\n-1\n");
}

// Perspective 22: the full happy path — `?` extracts the Ok payload, chains,
// and propagates an early Err — compiles and runs.
#[test]
fn test_question_operator_happy_path_end_to_end() {
    let (out, ok) = compile_and_run(
        r#"
fn checked(n: i64) -> Result<i64, String> {
    if n < 0 { return Result::Err("negative"); }
    return Result::Ok(n);
}

fn pipeline(n: i64) -> Result<i64, String> {
    let a = checked(n)?;
    let b = checked(a - 5)?;
    return Result::Ok(b);
}

fn main() {
    let good = pipeline(10);
    println(match good { Result::Ok(v) => v, Result::Err(_) => -1, });
    let bad = pipeline(2);
    println(match bad { Result::Ok(v) => v, Result::Err(_) => -1, });
}
"#,
    );
    assert!(ok, "? happy path must compile and run");
    assert_eq!(out, "5\n-1\n");
}

// ── Option helper method tests ─────────────────────────────────────────────

#[test]
fn test_option_is_some_and_is_none() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a = Option::Some(42);
    let b: Option<i64> = Option::None;
    println(a.is_some());
    println(a.is_none());
    println(b.is_some());
    println(b.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\nfalse\ntrue\n");
}

#[test]
fn test_option_unwrap_some_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(99);
    println(x.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

#[test]
fn test_option_unwrap_none_panics() {
    let src = r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.unwrap());
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "unwrap on None should panic (non-zero exit)");
    assert!(
        out.contains("None") || out.is_empty(),
        "panic message should mention None"
    );
}

#[test]
fn test_option_expect_some_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(7);
    println(x.expect("should have value"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_option_expect_none_panics_with_message() {
    let src = r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.expect("custom message"));
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "expect on None should panic");
    assert!(
        out.contains("custom message"),
        "panic should include custom message"
    );
}

#[test]
fn test_option_unwrap_or_some_returns_payload() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(5);
    println(x.unwrap_or(0));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn test_option_unwrap_or_none_returns_default() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Option<i64> = Option::None;
    println(x.unwrap_or(42));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_option_map_some_transforms_value() {
    let (out, ok) = compile_and_run(
        r#"
fn double(x: i64) -> i64 {
    return x * 2;
}
fn main() {
    let x = Option::Some(10);
    let y = x.map(double);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "20\n");
}

#[test]
fn test_option_map_none_stays_none() {
    let (out, ok) = compile_and_run(
        r#"
fn double(x: i64) -> i64 {
    return x * 2;
}
fn main() {
    let x: Option<i64> = Option::None;
    let y = x.map(double);
    println(y.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_option_map_with_lambda() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = Option::Some(3);
    let y = x.map(|v: i64| v * v);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

#[test]
fn test_option_and_then_some_calls_f() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_double(x: i64) -> Option<i64> {
    if x > 100 {
        return Option::None;
    }
    return Option::Some(x * 2);
}
fn main() {
    let a = Option::Some(5).and_then(safe_double);
    let b = Option::Some(200).and_then(safe_double);
    println(a.unwrap());
    println(b.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\ntrue\n");
}

#[test]
fn test_option_and_then_none_stays_none() {
    let (out, ok) = compile_and_run(
        r#"
fn safe_double(x: i64) -> Option<i64> {
    return Option::Some(x * 2);
}
fn main() {
    let x: Option<i64> = Option::None;
    let y = x.and_then(safe_double);
    println(y.is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_option_or_else_some_returns_self() {
    let (out, ok) = compile_and_run(
        r#"
fn fallback() -> Option<i64> {
    return Option::Some(99);
}
fn main() {
    let x = Option::Some(1);
    let y = x.or_else(fallback);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

#[test]
fn test_option_or_else_none_calls_f() {
    let (out, ok) = compile_and_run(
        r#"
fn fallback() -> Option<i64> {
    return Option::Some(99);
}
fn main() {
    let x: Option<i64> = Option::None;
    let y = x.or_else(fallback);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// ── Result helper method tests ─────────────────────────────────────────────

#[test]
fn test_result_is_ok_and_is_err() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let a: Result<i64, String> = Result::Ok(1);
    let b: Result<i64, String> = Result::Err("oops");
    println(a.is_ok());
    println(a.is_err());
    println(b.is_ok());
    println(b.is_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\nfalse\ntrue\n");
}

#[test]
fn test_result_unwrap_ok_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(55);
    println(x.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

#[test]
fn test_result_unwrap_err_panics() {
    let src = r#"
fn main() {
    let x: Result<i64, String> = Result::Err("fail");
    println(x.unwrap());
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "unwrap on Err should panic");
    assert!(out.contains("Err") || out.is_empty());
}

#[test]
fn test_result_expect_ok_returns_value() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(7);
    println(x.expect("should be ok"));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_result_expect_err_panics_with_message() {
    let src = r#"
fn main() {
    let x: Result<i64, String> = Result::Err("bad");
    println(x.expect("my error message"));
}
"#;
    let (out, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "expect on Err should panic");
    assert!(
        out.contains("my error message"),
        "panic should include custom message"
    );
}

#[test]
fn test_result_unwrap_or_ok_returns_payload() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(10);
    println(x.unwrap_or(0));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

#[test]
fn test_result_unwrap_or_err_returns_default() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Err("fail");
    println(x.unwrap_or(42));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_result_unwrap_err_extracts_error() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Err("my error");
    println(x.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "my error\n");
}

#[test]
fn test_result_unwrap_err_on_ok_panics() {
    let src = r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(1);
    println(x.unwrap_err());
}
"#;
    let (_, ok) = compile_and_run_check_exit(src);
    assert!(!ok, "unwrap_err on Ok should panic");
}

#[test]
fn test_result_map_ok_transforms_value() {
    let (out, ok) = compile_and_run(
        r#"
fn triple(x: i64) -> i64 {
    return x * 3;
}
fn main() {
    let x: Result<i64, String> = Result::Ok(4);
    let y = x.map(triple);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_result_map_err_unchanged() {
    let (out, ok) = compile_and_run(
        r#"
fn triple(x: i64) -> i64 {
    return x * 3;
}
fn main() {
    let x: Result<i64, String> = Result::Err("oops");
    let y = x.map(triple);
    println(y.is_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

#[test]
fn test_result_map_with_lambda() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(5);
    let y = x.map(|v: i64| v + 10);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n");
}

#[test]
fn test_result_map_err_transforms_error() {
    let (out, ok) = compile_and_run(
        r#"
fn add_prefix(s: String) -> String {
    return "error: " + s;
}
fn main() {
    let x: Result<i64, String> = Result::Err("bad input");
    let y = x.map_err(add_prefix);
    println(y.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "error: bad input\n");
}

#[test]
fn test_result_map_err_ok_unchanged() {
    let (out, ok) = compile_and_run(
        r#"
fn add_prefix(s: String) -> String {
    return "error: " + s;
}
fn main() {
    let x: Result<i64, String> = Result::Ok(42);
    let y = x.map_err(add_prefix);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_result_and_then_ok_chains() {
    let (out, ok) = compile_and_run(
        r#"
fn parse_positive(n: i64) -> Result<i64, String> {
    if n > 0 {
        return Result::Ok(n);
    }
    return Result::Err("not positive");
}
fn main() {
    let a = Result::Ok(5).and_then(parse_positive);
    let b = Result::Ok(-3).and_then(parse_positive);
    println(a.unwrap());
    println(b.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\nnot positive\n");
}

#[test]
fn test_result_and_then_err_stays_err() {
    let (out, ok) = compile_and_run(
        r#"
fn parse_positive(n: i64) -> Result<i64, String> {
    return Result::Ok(n * 2);
}
fn main() {
    let x: Result<i64, String> = Result::Err("initial error");
    let y = x.and_then(parse_positive);
    println(y.unwrap_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "initial error\n");
}

#[test]
fn test_result_or_else_ok_returns_self() {
    let (out, ok) = compile_and_run(
        r#"
fn recover(s: String) -> Result<i64, String> {
    return Result::Ok(0);
}
fn main() {
    let x: Result<i64, String> = Result::Ok(7);
    let y = x.or_else(recover);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_result_or_else_err_calls_f() {
    let (out, ok) = compile_and_run(
        r#"
fn recover(s: String) -> Result<i64, String> {
    return Result::Ok(99);
}
fn main() {
    let x: Result<i64, String> = Result::Err("fail");
    let y = x.or_else(recover);
    println(y.unwrap());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// ── Type error tests for Option/Result method helpers ─────────────────────

#[test]
fn test_option_is_some_with_args_reports_error() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = Option::Some(1);
    let _ = x.is_some(42);
}
"#
    ));
}

#[test]
fn test_option_unwrap_or_type_mismatch_reports_error() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = Option::Some(1);
    let _ = x.unwrap_or(true);
}
"#
    ));
}

#[test]
fn test_result_is_ok_with_args_reports_error() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x: Result<i64, String> = Result::Ok(1);
    let _ = x.is_ok(42);
}
"#
    ));
}

#[test]
fn test_result_map_wrong_fn_type_reports_error() {
    assert!(expect_compile_error(
        r#"
fn wrong(s: String) -> i64 {
    return 0;
}
fn main() {
    let x: Result<i64, String> = Result::Ok(1);
    let _ = x.map(wrong);
}
"#
    ));
}

// ── prot (protected) access modifier tests ────────────────────────────────

// 1. prot field accessible within own class method
#[test]
fn test_prot_field_accessible_in_own_class() {
    let (out, ok) = compile_and_run(
        r#"
class Bag {
    pub init(self, items: i64) {
        self.items = items;
    }
    prot items: i64;
    pub fn count(self) -> i64 { return self.items; }
}
fn main() {
    let b = new Bag(7);
    println(b.count());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 2. prot method callable within own class
#[test]
fn test_prot_method_callable_in_own_class() {
    let (out, ok) = compile_and_run(
        r#"
class Calc {
    pub init(self, val: i64) {
        self.val = val;
    }
    val: i64;
    prot fn triple(self) -> i64 { return self.val * 3; }
    pub fn result(self) -> i64 { return self.triple(); }
}
fn main() {
    let c = new Calc(4);
    println(c.result());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

// 3. prot field accessible in direct subclass method
#[test]
fn test_prot_field_accessible_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, score: i64) {
        self.score = score;
    }
    prot score: i64;
    pub fn score(self) -> i64 { return self.score; }
}
pub class Child extends Base {
    pub init(self, score: i64) {
        super.init(score);
    }
    pub fn bonus(self) -> i64 { return self.score + 10; }
}
fn main() {
    let c = new Child(5);
    println(c.score());
    println(c.bonus());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n15\n");
}

// 4. prot method callable in direct subclass
#[test]
fn test_prot_method_callable_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Engine {
    pub init(self, power: i64) {
        self.power = power;
    }
    power: i64;
    prot fn raw_power(self) -> i64 { return self.power; }
    pub fn get_power(self) -> i64 { return self.power; }
}
pub class Turbo extends Engine {
    pub init(self, power: i64) {
        super.init(power);
    }
    pub fn boosted(self) -> i64 { return self.raw_power() * 2; }
}
fn main() {
    let t = new Turbo(50);
    println(t.get_power());
    println(t.boosted());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "50\n100\n");
}

// 5. prot field accessible two levels down the hierarchy
#[test]
fn test_prot_field_accessible_two_levels_deep() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub init(self, n: i64) {
        self.n = n;
    }
    prot n: i64;
    pub fn n(self) -> i64 { return self.n; }
}
pub open class B extends A {
    pub init(self, n: i64) {
        super.init(n);
    }
    pub fn double(self) -> i64 { return self.n * 2; }
}
pub class C extends B {
    pub init(self, n: i64) {
        super.init(n);
    }
    pub fn triple(self) -> i64 { return self.n * 3; }
}
fn main() {
    let c = new C(4);
    println(c.n());
    println(c.double());
    println(c.triple());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n8\n12\n");
}

// 6. prot field rejected from unrelated external function
#[test]
fn test_prot_field_rejected_from_external_function() {
    assert_compile_error_contains(
        r#"
pub open class Vault {
    prot secret: i64;
}
fn steal(v: Vault) -> i64 { return v.secret; }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "field `secret` of class `Vault` is protected",
        ],
    );
}

// 7. prot method rejected from unrelated external function
#[test]
fn test_prot_method_rejected_from_external_function() {
    assert_compile_error_contains(
        r#"
pub open class Vault {
    x: i64;
    prot fn secret(self) -> i64 { return self.x; }
}
fn steal(v: Vault) -> i64 { return v.secret(); }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "method `secret` of class `Vault` is protected",
        ],
    );
}

// 8. prot field rejected via direct access from main
#[test]
fn test_prot_field_rejected_from_main() {
    assert!(expect_compile_error(
        r#"
class Box {
    prot val: i64;
}
fn main() {
    let b = new Box(1);
    println(b.val);
}
"#
    ));
}

// 9. pub still allows access from anywhere
#[test]
fn test_pub_overrides_prot_restriction() {
    let (out, ok) = compile_and_run(
        r#"
class Mix {
    pub init(self, x: i64, y: i64) {
        self.x = x;
        self.y = y;
    }
    pub x: i64;
    prot y: i64;
}
fn read_x(m: Mix) -> i64 { return m.x; }
fn main() {
    let m = new Mix(10, 20);
    println(read_x(m));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// 10. private is stricter than prot (subclass cannot see private)
#[test]
fn test_private_stricter_than_prot() {
    assert_compile_error_contains(
        r#"
pub open class Base {
    secret: i64;
}
pub class Child extends Base {
    pub fn leak(self) -> i64 { return self.secret; }
}
fn main() { println(1); }
"#,
        &["error[E0501]", "field `secret` of class `Base` is private"],
    );
}

// 11. prot method diagnostic points to declaration site
#[test]
fn test_prot_method_diagnostic_points_to_declaration() {
    assert_compile_error_contains(
        r#"
pub open class Service {
    x: i64;
    prot fn internal(self) -> i64 { return self.x; }
}
fn call(s: Service) -> i64 { return s.internal(); }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "method `internal` of class `Service` is protected",
            "method defined here",
        ],
    );
}

// 12. prot field diagnostic includes help text
#[test]
fn test_prot_field_diagnostic_help_text() {
    assert_compile_error_contains(
        r#"
pub open class Secure {
    prot token: i64;
}
fn grab(s: Secure) -> i64 { return s.token; }
fn main() { println(1); }
"#,
        &[
            "error[E0503]",
            "field `token` of class `Secure` is protected",
            "prot members are accessible only within",
        ],
    );
}

// 13. override method can access prot field from base
#[test]
fn test_prot_override_method_accesses_base_field() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub init(self, energy: i64) {
        self.energy = energy;
    }
    prot energy: i64;
    pub open fn cost(self) -> i64 { return self.energy; }
}
pub class Dog extends Animal {
    pub init(self, energy: i64) {
        super.init(energy);
    }
    pub override fn cost(self) -> i64 { return self.energy * 2; }
}
fn main() {
    let d = new Dog(5);
    println(d.cost());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// 14. prot + override: override of a prot method callable directly on the subclass type
#[test]
fn test_prot_method_override_in_subclass() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Shape {
    pub init(self, sides: i64) {
        self.sides = sides;
    }
    prot sides: i64;
    prot open fn side_count(self) -> i64 { return self.sides; }
    pub fn info(self) -> i64 { return self.side_count(); }
}
pub class Triangle extends Shape {
    pub init(self, sides: i64) {
        super.init(sides);
    }
    pub override fn side_count(self) -> i64 { return self.sides * 3; }
}
fn main() {
    let s = new Shape(4);
    let t = new Triangle(2);
    println(s.info());
    println(t.side_count());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n6\n");
}

// 15. prot field not visible as pub field from outside
#[test]
fn test_prot_field_not_publicly_visible() {
    assert!(expect_compile_error(
        r#"
class Hidden {
    prot value: i64;
}
fn main() {
    let h = new Hidden(1);
    println(h.value);
}
"#
    ));
}

// 16. error code is E0503, not E0501 or E0502
#[test]
fn test_prot_uses_error_code_e0503() {
    assert_compile_error_contains(
        r#"
class C { prot x: i64; }
fn main() {
    let c = new C(1);
    println(c.x);
}
"#,
        &["error[E0503]"],
    );
}

// 17. prot keyword parses on fields without other modifiers
#[test]
fn test_prot_parses_on_field() {
    let (out, ok) = compile_and_run(
        r#"
class Wrapper {
    pub init(self, inner: i64) {
        self.inner = inner;
    }
    prot inner: i64;
    pub fn get(self) -> i64 { return self.inner; }
}
fn main() {
    let w = new Wrapper(42);
    println(w.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 18. prot keyword parses on methods without other modifiers
#[test]
fn test_prot_parses_on_method() {
    let (out, ok) = compile_and_run(
        r#"
class Worker {
    pub init(self, load: i64) {
        self.load = load;
    }
    load: i64;
    prot fn internal_load(self) -> i64 { return self.load; }
    pub fn public_load(self) -> i64 { return self.internal_load(); }
}
fn main() {
    let w = new Worker(9);
    println(w.public_load());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

// 19. prot + open: protected open method overrideable in subclass, called on concrete type
#[test]
fn test_prot_open_method_can_be_overridden() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub init(self, speed: i64) {
        self.speed = speed;
    }
    prot speed: i64;
    pub fn get_speed(self) -> i64 { return self.speed; }
    prot open fn describe(self) -> i64 { return self.speed; }
    pub fn show(self) -> i64 { return self.describe(); }
}
pub class Car extends Vehicle {
    pub init(self, speed: i64) {
        super.init(speed);
    }
    pub override fn describe(self) -> i64 { return self.speed + 10; }
}
fn main() {
    let v = new Vehicle(30);
    let c = new Car(50);
    println(v.show());
    println(c.describe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n60\n");
}

// 20. prot field accessible within class and subclass, rejected elsewhere
#[test]
fn test_prot_complete_access_rules() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Counter {
    pub init(self, count: i64) {
        self.count = count;
    }
    prot count: i64;
    pub fn get(self) -> i64 { return self.count; }
    prot fn increment(self) -> i64 { return self.count + 1; }
}
pub class BoundedCounter extends Counter {
    pub init(self, count: i64) {
        super.init(count);
    }
    pub fn safe_inc(self, max: i64) -> i64 {
        let next = self.increment();
        if next > max {
            return self.count;
        }
        return next;
    }
}
fn main() {
    let c = new BoundedCounter(8);
    println(c.get());
    println(c.safe_inc(10));
    println(c.safe_inc(7));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n9\n8\n");
}

// ── Class / Object tests ───────────────────────────────────────────────────

// 1. Single field, single method
#[test]
fn test_class_single_field_and_getter() {
    let (out, ok) = compile_and_run(
        r#"
class Num {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let x = new Num(7);
    println(x.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 2. Multiple fields accessed via methods
#[test]
fn test_class_multiple_fields() {
    let (out, ok) = compile_and_run(
        r#"
class Rect {
    pub init(self, w: i64, h: i64) {
        self.w = w;
        self.h = h;
    }
    w: i64;
    h: i64;
    pub fn width(self) -> i64  { return self.w; }
    pub fn height(self) -> i64 { return self.h; }
    pub fn area(self) -> i64   { return self.w * self.h; }
}
fn main() {
    let r = new Rect(6, 4);
    println(r.width());
    println(r.height());
    println(r.area());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n4\n24\n");
}

// 3. Method that takes extra argument
#[test]
fn test_class_method_with_extra_arg() {
    let (out, ok) = compile_and_run(
        r#"
class Adder {
    pub init(self, base: i64) {
        self.base = base;
    }
    base: i64;
    pub fn add(self, n: i64) -> i64 { return self.base + n; }
}
fn main() {
    let a = new Adder(10);
    println(a.add(5));
    println(a.add(90));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n100\n");
}

// 4. Method that calls another method on self
#[test]
fn test_class_method_calls_sibling_method() {
    let (out, ok) = compile_and_run(
        r#"
class Circle {
    pub init(self, r: i64) {
        self.r = r;
    }
    r: i64;
    pub fn radius(self) -> i64     { return self.r; }
    pub fn diameter(self) -> i64   { return self.r * 2; }
}
fn main() {
    let c = new Circle(5);
    println(c.diameter());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// 5. Object passed to a free function
#[test]
fn test_class_object_passed_to_free_function() {
    let (out, ok) = compile_and_run(
        r#"
class Val {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn double(x: Val) -> i64 { return x.get() * 2; }
fn main() {
    let x = new Val(21);
    println(double(x));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 6. Object returned from free function
#[test]
fn test_class_object_returned_from_function() {
    let (out, ok) = compile_and_run(
        r#"
class Point {
    pub init(self, x: i64, y: i64) {
        self.x = x;
        self.y = y;
    }
    x: i64;
    y: i64;
    pub fn x(self) -> i64 { return self.x; }
    pub fn y(self) -> i64 { return self.y; }
}
fn make(x: i64, y: i64) -> Point { return new Point(x, y); }
fn main() {
    let p = make(3, 4);
    println(p.x());
    println(p.y());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n4\n");
}

// 7. Bool field
#[test]
fn test_class_bool_field() {
    let (out, ok) = compile_and_run(
        r#"
class Flag {
    pub init(self, on: bool) {
        self.on = on;
    }
    on: bool;
    pub fn is_on(self) -> bool { return self.on; }
}
fn main() {
    let f = new Flag(true);
    println(f.is_on());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 8. String field
#[test]
fn test_class_string_field() {
    let (out, ok) = compile_and_run(
        r#"
class Msg {
    pub init(self, text: String) {
        self.text = text;
    }
    text: String;
    pub fn get(self) -> String { return self.text; }
}
fn main() {
    let m = new Msg("hello");
    println(m.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// 9. f64 field
#[test]
fn test_class_f64_field() {
    let (out, ok) = compile_and_run(
        r#"
class Temp {
    pub init(self, celsius: f64) {
        self.celsius = celsius;
    }
    celsius: f64;
    pub fn get(self) -> f64 { return self.celsius; }
}
fn main() {
    let t = new Temp(36.6);
    println(t.get());
}
"#,
    );
    assert!(ok);
    assert!(out.starts_with("36.6"));
}

// 10. Nested class fields
#[test]
fn test_class_nested_field() {
    let (out, ok) = compile_and_run(
        r#"
class Inner {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
class Outer {
    pub init(self, inner: Inner) {
        self.inner = inner;
    }
    pub inner: Inner;
    pub fn inner(self) -> Inner { return self.inner; }
}
fn main() {
    let o = new Outer(new Inner(99));
    println(o.inner().get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// 11. Multiple objects of same class
#[test]
fn test_class_multiple_instances() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let a = new Box(1);
    let b = new Box(2);
    let c = new Box(3);
    let va = a.get();
    let vb = b.get();
    let vc = c.get();
    println(va + vb + vc);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n");
}

// 12. Free-function constructor (factory pattern)
#[test]
fn test_class_static_constructor_method() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn get(self) -> i64 { return self.n; }
}
fn make_counter(start: i64) -> Counter { return new Counter(start); }
fn main() {
    let c = make_counter(42);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 13. Method returning bool comparison
#[test]
fn test_class_method_returns_bool() {
    let (out, ok) = compile_and_run(
        r#"
class Score {
    pub init(self, points: i64) {
        self.points = points;
    }
    points: i64;
    pub fn passing(self) -> bool { return self.points >= 60; }
}
fn main() {
    let s1 = new Score(80);
    let s2 = new Score(40);
    println(s1.passing());
    println(s2.passing());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

// 14. Method with conditional logic
#[test]
fn test_class_method_with_if() {
    let (out, ok) = compile_and_run(
        r#"
class Abs {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn abs(self) -> i64 {
        if self.v < 0 {
            return self.v * -1;
        }
        return self.v;
    }
}
fn main() {
    let a = new Abs(-5);
    let b = new Abs(3);
    println(a.abs());
    println(b.abs());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n3\n");
}

// 15. Method with loop
#[test]
fn test_class_method_with_loop() {
    let (out, ok) = compile_and_run(
        r#"
class Pow {
    pub init(self, base: i64) {
        self.base = base;
    }
    base: i64;
    pub fn pow(self, exp: i64) -> i64 {
        let mut result = 1;
        let mut i = 0;
        while i < exp {
            result = result * self.base;
            i = i + 1;
        }
        return result;
    }
}
fn main() {
    let p = new Pow(2);
    println(p.pow(8));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "256\n");
}

// 16. Public field direct access
#[test]
fn test_class_public_field_direct_access() {
    let (out, ok) = compile_and_run(
        r#"
class Point {
    pub x: i64;
    pub y: i64;
}
fn main() {
    let p = new Point(10, 20);
    println(p.x);
    println(p.y);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n");
}

// 17. Private field rejected outside class
#[test]
fn test_class_private_field_rejected_outside() {
    assert!(expect_compile_error(
        r#"
class Pair {
    a: i64;
    b: i64;
}
fn main() {
    let p = new Pair(1, 2);
    println(p.a);
}
"#
    ));
}

// 18. Private method rejected outside class
#[test]
fn test_class_private_method_rejected_outside() {
    assert!(expect_compile_error(
        r#"
class Pair {
    a: i64;
    fn sum(self) -> i64 { return self.a; }
}
fn main() {
    let p = new Pair(1);
    println(p.sum());
}
"#
    ));
}

// 19. Simple inheritance: child can be assigned to base variable (type check)
#[test]
fn test_class_inheritance_inherits_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub open fn value(self) -> i64 { return 42; }
}
pub class Child extends Base {
    pub override fn value(self) -> i64 { return 42; }
}
fn main() {
    let c = new Child();
    println(c.value());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 20. Override: derived class method called directly on derived type
#[test]
fn test_class_override_changes_value() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub open fn id(self) -> i64 { return 1; }
}
pub class Derived extends Base {
    pub override fn id(self) -> i64 { return 2; }
}
fn main() {
    let b = new Base();
    let d = new Derived();
    println(b.id());
    println(d.id());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

// 21. Two levels of inheritance, each called directly
#[test]
fn test_class_two_level_inheritance() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub open fn tag(self) -> i64 { return 1; }
}
pub open class B extends A {
    pub open override fn tag(self) -> i64 { return 2; }
}
pub class C extends B {
    pub override fn tag(self) -> i64 { return 3; }
}
fn main() {
    let a = new A();
    let b = new B();
    let c = new C();
    println(a.tag());
    println(b.tag());
    println(c.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n3\n");
}

// 22. Child inherits field from base, override method accesses inherited field
#[test]
fn test_class_child_with_field_and_inherited_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Named {
    pub name: String;
    pub open fn greet(self) -> String { return self.name; }
}
pub class Employee extends Named {
    pub dept: String;
    pub override fn greet(self) -> String { return self.name; }
    pub fn dept(self) -> String { return self.dept; }
}
fn main() {
    let e = new Employee("Alice", "Eng");
    println(e.greet());
    println(e.dept());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Alice\nEng\n");
}

// 23. Override reads inherited public i64 field
#[test]
fn test_class_override_reads_base_field() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub age: i64;
    pub open fn describe(self) -> i64 { return self.age; }
}
pub class Cat extends Animal {
    pub override fn describe(self) -> i64 { return self.age * 2; }
}
fn main() {
    let base = new Animal(5);
    let cat = new Cat(3);
    println(base.describe());
    println(cat.describe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n6\n");
}

// 24. Extending a non-open class reports error
#[test]
fn test_class_extend_non_open_is_error() {
    assert!(expect_compile_error(
        r#"
class Closed {}
class Child extends Closed {}
fn main() { println(1); }
"#
    ));
}

// 25. Override without keyword reports error
#[test]
fn test_class_override_without_keyword_is_error() {
    assert!(expect_compile_error(
        r#"
pub open class Base {
    pub open fn foo(self) -> i64 { return 1; }
}
pub class Child extends Base {
    pub fn foo(self) -> i64 { return 2; }
}
fn main() { println(1); }
"#
    ));
}

// 26. Override non-open method is error
#[test]
fn test_class_override_non_open_method_is_error() {
    assert!(expect_compile_error(
        r#"
pub open class Base {
    pub fn foo(self) -> i64 { return 1; }
}
pub class Child extends Base {
    pub override fn foo(self) -> i64 { return 2; }
}
fn main() { println(1); }
"#
    ));
}

// 27. Object stored in local variable, method called later
#[test]
fn test_class_stored_then_method_called() {
    let (out, ok) = compile_and_run(
        r#"
class Token {
    pub init(self, id: i64) {
        self.id = id;
    }
    id: i64;
    pub fn id(self) -> i64 { return self.id; }
}
fn main() {
    let t = new Token(77);
    let val = t.id();
    println(val);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "77\n");
}

// 28. Multiple method calls on same object
#[test]
fn test_class_multiple_method_calls_on_same_obj() {
    let (out, ok) = compile_and_run(
        r#"
class Stats {
    pub init(self, total: i64, count: i64) {
        self.total = total;
        self.count = count;
    }
    total: i64;
    count: i64;
    pub fn total(self) -> i64 { return self.total; }
    pub fn count(self) -> i64 { return self.count; }
    pub fn avg(self) -> i64   { return self.total / self.count; }
}
fn main() {
    let s = new Stats(90, 3);
    println(s.total());
    println(s.count());
    println(s.avg());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "90\n3\n30\n");
}

// 29. Method returns another class instance
#[test]
fn test_class_method_returns_class_instance() {
    let (out, ok) = compile_and_run(
        r#"
class Inner {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
class Outer {
    pub fn make_inner(self, v: i64) -> Inner { return new Inner(v); }
}
fn main() {
    let o = new Outer();
    let i = o.make_inner(55);
    println(i.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

// 30. Class with Option field
#[test]
fn test_class_with_option_field() {
    let (out, ok) = compile_and_run(
        r#"
class MaybeNum {
    pub init(self, val: Option<i64>) {
        self.val = val;
    }
    val: Option<i64>;
    pub fn get_or(self, def: i64) -> i64 { return self.val.unwrap_or(def); }
    pub fn has_value(self) -> bool { return self.val.is_some(); }
}
fn main() {
    let a = new MaybeNum(Option::Some(10));
    let b = new MaybeNum(Option::None);
    println(a.get_or(0));
    println(b.get_or(99));
    println(a.has_value());
    println(b.has_value());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n99\ntrue\nfalse\n");
}

// 31. Class with Result field
#[test]
fn test_class_with_result_field() {
    let (out, ok) = compile_and_run(
        r#"
class Op {
    pub init(self, result: Result<i64, String>) {
        self.result = result;
    }
    result: Result<i64, String>;
    pub fn ok_or(self, def: i64) -> i64 { return self.result.unwrap_or(def); }
    pub fn succeeded(self) -> bool { return self.result.is_ok(); }
}
fn main() {
    let a = new Op(Result::Ok(7));
    let b = new Op(Result::Err("fail"));
    println(a.ok_or(0));
    println(b.ok_or(0));
    println(a.succeeded());
    println(b.succeeded());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n0\ntrue\nfalse\n");
}

// 32. Class method returns Option
#[test]
fn test_class_method_returns_option() {
    let (out, ok) = compile_and_run(
        r#"
class Lookup {
    pub init(self, key: i64, value: i64) {
        self.key = key;
        self.value = value;
    }
    key: i64;
    value: i64;
    pub fn find(self, k: i64) -> Option<i64> {
        if self.key == k {
            return Option::Some(self.value);
        }
        return Option::None;
    }
}
fn main() {
    let l = new Lookup(5, 100);
    println(l.find(5).unwrap());
    println(l.find(9).is_none());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\ntrue\n");
}

// 33. Class method returns Result
#[test]
fn test_class_method_returns_result() {
    let (out, ok) = compile_and_run(
        r#"
class Divider {
    pub init(self, denom: i64) {
        self.denom = denom;
    }
    denom: i64;
    pub fn divide(self, n: i64) -> Result<i64, String> {
        if self.denom == 0 {
            return Result::Err("division by zero");
        }
        return Result::Ok(n / self.denom);
    }
}
fn main() {
    let d = new Divider(4);
    let z = new Divider(0);
    println(d.divide(20).unwrap());
    println(z.divide(1).is_err());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\ntrue\n");
}

// 34. Array-free accumulator via two class instances
#[test]
fn test_class_two_counters_independent() {
    let (out, ok) = compile_and_run(
        r#"
class Acc {
    pub init(self, start: i64) {
        self.start = start;
    }
    start: i64;
    pub fn sum_to(self, n: i64) -> i64 {
        let mut s = self.start;
        let mut i = 0;
        while i < n {
            s = s + i;
            i = i + 1;
        }
        return s;
    }
}
fn main() {
    let a = new Acc(0);
    let b = new Acc(100);
    println(a.sum_to(5));
    println(b.sum_to(5));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n110\n");
}

// 35. GC: class allocated inside function, returned as primitive
#[test]
fn test_class_gc_inner_alloc_returns_primitive() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn compute() -> i64 {
    let t = new Tmp(42);
    return t.get();
}
fn main() {
    gc_collect();
    let x = compute();
    gc_collect();
    println(x);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 36. GC: live object survives collect
#[test]
fn test_class_gc_live_object_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let b = new Box(123);
    gc_collect();
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "123\n");
}

// 37. GC: two objects, one goes out of scope
#[test]
fn test_class_gc_one_survives_one_collected() {
    let (out, ok) = compile_and_run(
        r#"
class Obj {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn make_and_drop() {
    let tmp = new Obj(999);
}
fn main() {
    let live = new Obj(7);
    make_and_drop();
    gc_collect();
    println(live.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 38. Nullable class: nil assignment
#[test]
fn test_class_nullable_accepts_nil() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn main() {
    let n: Node? = nil;
    println(n == nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 39. Nullable: after nil-check, use object
#[test]
fn test_class_nullable_nil_guard_then_use() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    }
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
}
fn maybe_get(n: Node?) -> i64 {
    if n == nil {
        return -1;
    }
    return n.get();
}
fn main() {
    let a: Node? = new Node(5);
    let b: Node? = nil;
    println(maybe_get(a));
    println(maybe_get(b));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n-1\n");
}

// 40. Class as function argument and return type
#[test]
fn test_class_as_fn_arg_and_return() {
    let (out, ok) = compile_and_run(
        r#"
class Vec2 {
    pub x: i64;
    pub y: i64;
    pub fn x(self) -> i64 { return self.x; }
    pub fn y(self) -> i64 { return self.y; }
}
fn add(a: Vec2, b: Vec2) -> Vec2 {
    return new Vec2(a.x() + b.x(), a.y() + b.y());
}
fn main() {
    let u = new Vec2(1, 2);
    let v = new Vec2(3, 4);
    let w = add(u, v);
    println(w.x());
    println(w.y());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "4\n6\n");
}

// 41. Class with two String fields, each returned independently
#[test]
fn test_class_method_string_concat() {
    let (out, ok) = compile_and_run(
        r#"
class Person {
    pub init(self, first: String, last: String) {
        self.first = first;
        self.last = last;
    }
    first: String;
    last: String;
    pub fn first(self) -> String { return self.first; }
    pub fn last(self) -> String  { return self.last;  }
}
fn main() {
    let p = new Person("Jane", "Doe");
    println(p.first());
    println(p.last());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "Jane\nDoe\n");
}

// 42. Class method used in boolean expression
#[test]
fn test_class_method_in_boolean_expr() {
    let (out, ok) = compile_and_run(
        r#"
class Range {
    pub init(self, lo: i64, hi: i64) {
        self.lo = lo;
        self.hi = hi;
    }
    lo: i64;
    hi: i64;
    pub fn contains(self, v: i64) -> bool { return v >= self.lo && v <= self.hi; }
}
fn main() {
    let r = new Range(10, 20);
    println(r.contains(15));
    println(r.contains(25));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\nfalse\n");
}

// 43. Class method used in while condition
#[test]
fn test_class_method_in_while_condition() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, limit: i64) {
        self.limit = limit;
    }
    limit: i64;
    pub fn below(self, n: i64) -> bool { return n < self.limit; }
}
fn main() {
    let c = new Counter(3);
    let mut i = 0;
    while c.below(i) {
        println(i);
        i = i + 1;
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n1\n2\n");
}

// 44. Two different class types in same function
#[test]
fn test_class_two_different_classes() {
    let (out, ok) = compile_and_run(
        r#"
class Width  {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class Height {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn area(w: Width, h: Height) -> i64 { return w.get() * h.get(); }
fn main() {
    let w = new Width(7);
    let h = new Height(3);
    println(area(w, h));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "21\n");
}

// 45. Method returning f64
#[test]
fn test_class_method_returning_f64() {
    let (out, ok) = compile_and_run(
        r#"
class Circle {
    pub init(self, radius: f64) {
        self.radius = radius;
    }
    radius: f64;
    pub fn area(self) -> f64 { return 3.14159 * self.radius * self.radius; }
}
fn main() {
    let c = new Circle(2.0);
    let a = c.area();
    println(a > 12.0);
    println(a < 13.0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// 46. Child accesses inherited public field via speed() method and own override
#[test]
fn test_class_inherited_base_field_via_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub speed: i64;
    pub fn speed(self) -> i64 { return self.speed; }
    pub open fn describe(self) -> i64 { return self.speed; }
}
pub class Car extends Vehicle {
    pub override fn describe(self) -> i64 { return self.speed * 2; }
}
fn main() {
    let v = new Vehicle(30);
    let car = new Car(60);
    println(v.speed());
    println(car.speed());
    println(v.describe());
    println(car.describe());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n60\n30\n120\n");
}

// 47. Object used in match expression (via method)
#[test]
fn test_class_method_result_used_in_match() {
    let (out, ok) = compile_and_run(
        r#"
class Tag {
    pub init(self, kind: i64) {
        self.kind = kind;
    }
    kind: i64;
    pub fn kind(self) -> i64 { return self.kind; }
}
fn describe(t: Tag) -> String {
    let k = t.kind();
    if k == 1 { return "one"; }
    if k == 2 { return "two"; }
    return "other";
}
fn main() {
    println(describe(new Tag(1)));
    println(describe(new Tag(2)));
    println(describe(new Tag(9)));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "one\ntwo\nother\n");
}

// 48. Object created in if-else branch
#[test]
fn test_class_created_in_if_else() {
    let (out, ok) = compile_and_run(
        r#"
class Signed {
    pub init(self, v: i64, neg: bool) {
        self.v = v;
        self.neg = neg;
    }
    v: i64;
    neg: bool;
    pub fn value(self) -> i64 { return self.v; }
    pub fn is_neg(self) -> bool { return self.neg; }
}
fn make(n: i64) -> Signed {
    if n < 0 {
        return new Signed(n * -1, true);
    }
    return new Signed(n, false);
}
fn main() {
    let a = make(-7);
    let b = make(3);
    println(a.value());
    println(a.is_neg());
    println(b.value());
    println(b.is_neg());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\ntrue\n3\nfalse\n");
}

// 49. Class missing field in literal is an error
#[test]
fn test_class_literal_missing_field_is_error() {
    assert!(expect_compile_error(
        r#"
class Point { x: i64; y: i64; }
fn main() {
    let p = new Point(1);
    println(1);
}
"#
    ));
}

// 50. Class literal with extra field is an error
#[test]
fn test_class_literal_extra_field_is_error() {
    assert!(expect_compile_error(
        r#"
class Point { x: i64; }
fn main() {
    let p = new Point(1, 2);
    println(1);
}
"#
    ));
}

// ── Subtype: Dog passed to fn(Animal) ─────────────────────────────────────

// 1. void return: child passes to parent-typed parameter
#[test]
fn test_subtype_child_passes_to_parent_param() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
fn feed(a: Animal) { println(1); }
fn main() {
    let d = new Dog();
    feed(d);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n");
}

// 2. parent method (same name) callable on concrete type — each dispatch to own impl
#[test]
fn test_subtype_parent_method_callable_on_child() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub open fn kind(self) -> i64 { return 0; }
}
pub class Dog extends Animal {
    pub override fn kind(self) -> i64 { return 1; }
}
fn main() {
    let a = new Animal();
    let d = new Dog();
    println(a.kind());
    println(d.kind());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n1\n");
}

// 3. function returns child as parent type
#[test]
fn test_subtype_function_returns_child_as_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub fn tag(self) -> i64 { return 42; }
}
pub class Cat extends Animal {}
fn make() -> Animal { return new Cat(); }
fn main() {
    let a = make();
    println(a.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// 4. child stored in parent-typed variable — compiles, parent method used
#[test]
fn test_subtype_stored_in_parent_typed_var() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub open fn wheels(self) -> i64 { return 4; }
}
pub class Bike extends Vehicle {
    pub override fn wheels(self) -> i64 { return 2; }
}
fn main() {
    let v: Vehicle = new Bike();
    let b = new Bike();
    println(v.wheels());
    println(b.wheels());
}
"#,
    );
    assert!(ok);
    // Dynamic dispatch: v holds a Bike at runtime, so Bike__wheels (2) is called.
    // b is a Bike, so Bike__wheels (2) is called.
    assert_eq!(out, "2\n2\n");
}

// 5. two different subtypes each compile correctly as parent-typed argument
#[test]
fn test_subtype_two_children_same_function() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Shape {
    pub open fn name(self) -> i64 { return 0; }
}
pub class Square extends Shape {
    pub override fn name(self) -> i64 { return 4; }
}
pub class Triangle extends Shape {
    pub override fn name(self) -> i64 { return 3; }
}
fn accept_shape(s: Shape) { println(1); }
fn main() {
    let sq = new Square();
    let tr = new Triangle();
    accept_shape(sq);
    accept_shape(tr);
    println(sq.name());
    println(tr.name());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n4\n3\n");
}

// 6. child passed through two function calls
#[test]
fn test_subtype_child_passed_through_two_calls() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub fn val(self) -> i64 { return 7; }
}
pub class Child extends Base {}
fn wrap(b: Base) -> i64 { return b.val(); }
fn outer(b: Base) -> i64 { return wrap(b); }
fn main() {
    println(outer(new Child()));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// 7. three-level hierarchy: grandchild compiles as grandparent arg; each type calls own method
#[test]
fn test_subtype_grandchild_to_grandparent_fn() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub open fn tag(self) -> i64 { return 1; }
}
pub open class B extends A {
    pub open override fn tag(self) -> i64 { return 2; }
}
pub class C extends B {
    pub override fn tag(self) -> i64 { return 3; }
}
fn accept_a(a: A) { println(1); }
fn main() {
    let c = new C();
    accept_a(c);
    println(new A().tag());
    println(new B().tag());
    println(c.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n2\n3\n");
}

// 8. child with own field passes to parent-typed function; child's own method works
#[test]
fn test_subtype_child_with_extra_field_passes() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Node {
    pub open fn kind(self) -> i64 { return 0; }
}
pub class Leaf extends Node {
    pub extra: i64;
    pub override fn kind(self) -> i64 { return 1; }
}
fn accept_node(n: Node) { println(1); }
fn main() {
    let leaf = new Leaf(99);
    accept_node(leaf);
    println(leaf.kind());
    println(leaf.extra);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n1\n99\n");
}

// 9. base type rejected when child type expected (negative test)
#[test]
fn test_subtype_base_rejected_as_child() {
    assert!(expect_compile_error(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
fn use_dog(d: Dog) { println(1); }
fn main() {
    let a = new Animal();
    use_dog(a);
}
"#
    ));
}

// 10. sibling type rejected (not a subtype)
#[test]
fn test_subtype_sibling_rejected() {
    assert!(expect_compile_error(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
pub class Cat extends Animal {}
fn use_dog(d: Dog) { println(1); }
fn main() {
    let c = new Cat();
    use_dog(c);
}
"#
    ));
}

// ── Subtype: Dog passed to fn(Animal?) ────────────────────────────────────

// 1. child passes to nullable parent param
#[test]
fn test_nullable_subtype_child_to_nullable_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
pub class Dog extends Animal {}
fn maybe_feed(a: Animal?) { println(a == nil); }
fn main() {
    let d = new Dog();
    maybe_feed(d);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\n");
}

// 2. nil also passes to nullable parent param
#[test]
fn test_nullable_nil_passes_to_nullable_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
fn maybe_feed(a: Animal?) { println(a == nil); }
fn main() {
    maybe_feed(nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 3. nil check inside nullable function
#[test]
fn test_nullable_subtype_nil_guard_in_function() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {
    pub fn kind(self) -> i64 { return 1; }
}
pub class Dog extends Animal {}
fn describe(a: Animal?) -> i64 {
    if a == nil { return -1; }
    return a.kind();
}
fn main() {
    let d = new Dog();
    println(describe(d));
    println(describe(nil));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n-1\n");
}

// 4. child stored in nullable parent variable
#[test]
fn test_nullable_child_stored_in_nullable_parent_var() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
pub class Cat extends Animal {}
fn main() {
    let a: Animal? = new Cat();
    println(a == nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\n");
}

// 5. nil stored in nullable parent variable
#[test]
fn test_nullable_nil_stored_in_nullable_parent_var() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Animal {}
fn main() {
    let a: Animal? = nil;
    println(a == nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// 6. function returning nullable parent from child
#[test]
fn test_nullable_function_returns_child_as_nullable_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Vehicle {
    pub fn tag(self) -> i64 { return 99; }
}
pub class Car extends Vehicle {}
fn maybe_car(use_it: bool) -> Vehicle? {
    if use_it { return new Car(); }
    return nil;
}
fn main() {
    let v = maybe_car(true);
    println(v == nil);
    let n = maybe_car(false);
    println(n == nil);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "false\ntrue\n");
}

// 7. child through nullable then nil-guarded method call
#[test]
fn test_nullable_child_through_nullable_then_method() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Node {
    pub fn value(self) -> i64 { return 42; }
}
pub class Leaf extends Node {}
fn get_value(n: Node?) -> i64 {
    if n == nil { return 0; }
    return n.value();
}
fn main() {
    let leaf = new Leaf();
    println(get_value(leaf));
    println(get_value(nil));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n0\n");
}

// 8. two different subtypes compile as nullable parent; nil check works
#[test]
fn test_nullable_two_children_to_nullable_parent() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Fruit {
    pub open fn name(self) -> i64 { return 0; }
}
pub class Apple extends Fruit {
    pub override fn name(self) -> i64 { return 1; }
}
pub class Orange extends Fruit {
    pub override fn name(self) -> i64 { return 2; }
}
fn not_nil(f: Fruit?) -> bool {
    return f != nil;
}
fn main() {
    let a = new Apple();
    let o = new Orange();
    println(not_nil(a));
    println(not_nil(o));
    println(not_nil(nil));
    println(a.name());
    println(o.name());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\nfalse\n1\n2\n");
}

// 9. child with field passed to nullable parent, field inaccessible through nullable
#[test]
fn test_nullable_child_field_inaccessible_through_nullable_base() {
    assert!(expect_compile_error(
        r#"
pub open class Animal {}
pub class Dog extends Animal { pub breed: i64; }
fn main() {
    let d: Animal? = new Dog(1);
    println(d.breed);
}
"#
    ));
}

// 10. nullable of child type assigned to nullable of parent type
#[test]
fn test_nullable_child_nullable_to_parent_nullable() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {}
pub class Sub extends Base {}
fn use_base(b: Base?) -> i64 {
    if b == nil { return 0; }
    return 1;
}
fn main() {
    let s: Sub? = new Sub();
    println(use_base(s));
    let n: Sub? = nil;
    println(use_base(n));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\n");
}

// ── GC tests ──────────────────────────────────────────────────────────────

// GC-01: single object freed after scope exit
#[test]
fn test_gc_01_single_object_freed_after_scope() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let b = new Box(1);
    return b.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-02: two objects freed after scope exit
#[test]
fn test_gc_02_two_objects_freed_after_scope() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let a = new Box(1);
    let b = new Box(2);
    return a.get() + b.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-03: live object NOT freed
#[test]
fn test_gc_03_live_object_not_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = new Box(42);
    gc_collect();
    println(b.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\ntrue\n");
}

// GC-04: gc_allocated_bytes increases with each allocation
#[test]
fn test_gc_04_allocated_bytes_grows_per_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; }
fn main() {
    let before = gc_allocated_bytes();
    let _a = new Box(1);
    let mid = gc_allocated_bytes();
    let _b = new Box(2);
    let after = gc_allocated_bytes();
    println(mid > before);
    println(after > mid);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// GC-05: explicit gc_collect returns zero after all freed
#[test]
fn test_gc_05_explicit_collect_returns_zero() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn alloc_and_drop() -> i64 {
    let t = new Tmp(99);
    return t.get();
}
fn main() {
    let r1 = alloc_and_drop();
    let r2 = alloc_and_drop();
    println(r1 + r2);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "198\n0\n");
}

// GC-06: object allocated in loop, freed after loop
#[test]
fn test_gc_06_objects_in_loop_freed_after() {
    let (out, ok) = compile_and_run(
        r#"
class Item {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn process(n: i64) -> i64 {
    let mut i = 0;
    let mut sum = 0;
    while i < n {
        let item = new Item(i);
        sum = sum + item.get();
        i = i + 1;
    }
    return sum;
}
fn main() {
    let result = process(5);
    gc_collect();
    println(result);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n0\n");
}

// GC-07: nested function allocation, inner freed
#[test]
fn test_gc_07_nested_function_alloc_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn inner() -> i64 {
    let n = new Node(5);
    return n.get();
}
fn outer() -> i64 { return inner() + inner(); }
fn main() {
    let r = outer();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n0\n");
}

// GC-08: object field holding i64 doesn't prevent GC
#[test]
fn test_gc_08_i64_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Point {
    pub init(self, x: i64, y: i64) {
        self.x = x;
        self.y = y;
    } x: i64; y: i64; pub fn sum(self) -> i64 { return self.x + self.y; } }
fn make_sum() -> i64 {
    let p = new Point(3, 4);
    return p.sum();
}
fn main() {
    let _ = make_sum();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-09: bool field object freed
#[test]
fn test_gc_09_bool_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Flag {
    pub init(self, on: bool) {
        self.on = on;
    } on: bool; pub fn get(self) -> bool { return self.on; } }
fn check() -> bool {
    let f = new Flag(true);
    return f.get();
}
fn main() {
    let _ = check();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-10: multiple collect cycles — already-freed objects stay at zero
#[test]
fn test_gc_10_multiple_collect_cycles() {
    let (out, ok) = compile_and_run(
        r#"
class Obj {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_obj() -> i64 { let o = new Obj(1); return o.get(); }
fn main() {
    let _ = drop_obj();
    gc_collect();
    let after1 = gc_allocated_bytes();
    gc_collect();
    let after2 = gc_allocated_bytes();
    println(after1);
    println(after2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n0\n");
}

// GC-11: object reachable through local variable survives
#[test]
fn test_gc_11_local_var_keeps_alive() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let n = new Node(7);
    gc_collect();
    gc_collect();
    println(n.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// GC-12: two live objects both survive collect
#[test]
fn test_gc_12_two_live_objects_both_survive() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class B {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let a = new A(10);
    let b = new B(20);
    gc_collect();
    println(a.get());
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n");
}

// GC-13: live and dead objects — only dead freed
#[test]
fn test_gc_13_live_and_dead_objects_mixed() {
    let (out, ok) = compile_and_run(
        r#"
class Live {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class Dead {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_dead() -> i64 { let d = new Dead(0); return d.get(); }
fn main() {
    let live = new Live(5);
    let _ = drop_dead();
    gc_collect();
    println(live.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\ntrue\n");
}

// GC-14: object passed to function and returned as i64, original freed
#[test]
fn test_gc_14_passed_to_fn_extract_primitive_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Wrap {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn extract(w: Wrap) -> i64 { return w.get(); }
fn main() {
    let val = extract(new Wrap(99));
    gc_collect();
    println(val);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n0\n");
}

// GC-15: object allocated before and after collect
#[test]
fn test_gc_15_alloc_collect_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_one() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let _ = drop_one();
    gc_collect();
    let b2 = new Box(2);
    println(b2.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "2\ntrue\n");
}

// GC-16: object with string field — string GC-managed too
#[test]
fn test_gc_16_string_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Msg {
    pub init(self, text: String) {
        self.text = text;
    } text: String; pub fn get(self) -> String { return self.text; } }
fn drop_msg() -> String {
    let m = new Msg("hello");
    return m.get();
}
fn main() {
    let s = drop_msg();
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello\n");
}

// GC-17: nullable field pointing to live object keeps it alive
#[test]
fn test_gc_17_nullable_field_keeps_child_alive() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub next: Node?; }
fn main() {
    let tail = new Node(2, nil);
    let head = new Node(1, tail);
    gc_collect();
    println(head.v);
    let n = head.next;
    if n != nil { println(n.v); }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n");
}

// GC-18: nil nullable field — object still freed when out of scope
#[test]
fn test_gc_18_nil_nullable_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64, next: Node?) {
        self.v = v;
        self.next = next;
    } v: i64; next: Node?; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let n = new Node(3, nil);
    return n.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-19: chain of nullable nodes — all freed together
#[test]
fn test_gc_19_chain_of_nodes_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub next: Node?; }
fn make_chain() -> i64 {
    let c = new Node(3, nil);
    let b = new Node(2, c);
    let a = new Node(1, b);
    return a.v;
}
fn main() {
    let _ = make_chain();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-20: chain of nullable nodes — head kept, rest freed (not possible to free partial chain while head live)
#[test]
fn test_gc_20_live_chain_all_survive() {
    let (out, ok) = compile_and_run(
        r#"
class Node { pub v: i64; pub next: Node?; }
fn main() {
    let c = new Node(3, nil);
    let b = new Node(2, c);
    let a = new Node(1, b);
    gc_collect();
    println(a.v);
    let n1 = a.next;
    if n1 != nil {
        println(n1.v);
        let n2 = n1.next;
        if n2 != nil { println(n2.v); }
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n3\n");
}

// GC-21: inherited class object freed
#[test]
fn test_gc_21_inherited_class_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn drop_child() -> i64 { let c = new Child(5); return c.get(); }
fn main() {
    let _ = drop_child();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-22: inherited class object live, survives
#[test]
fn test_gc_22_inherited_class_live_survives() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn main() {
    let c = new Child(11);
    gc_collect();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "11\n");
}

// GC-23: object with prot field freed
#[test]
fn test_gc_23_prot_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Secret {
    pub init(self, key: i64) {
        self.key = key;
    } prot key: i64; pub fn get(self) -> i64 { return self.key; } }
fn drop_it() -> i64 { let s = new Secret(7); return s.get(); }
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-24: object allocated inside if-branch, freed after branch
#[test]
fn test_gc_24_object_in_if_branch_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn conditional(flag: bool) -> i64 {
    if flag {
        let t = new Tmp(3);
        return t.get();
    }
    return 0;
}
fn main() {
    let _ = conditional(true);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-25: object alive across if-branch
#[test]
fn test_gc_25_object_alive_across_if() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = new Box(9);
    if b.get() > 0 {
        gc_collect();
    }
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n");
}

// GC-26: object allocated inside while loop, freed each iteration
#[test]
fn test_gc_26_loop_object_freed_each_iteration() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let mut i = 0;
    while i < 3 {
        let t = new Tmp(i);
        let _ = t.get();
        i = i + 1;
    }
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-27: collect before allocation — zero
#[test]
fn test_gc_27_collect_before_any_alloc() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-28: allocated_bytes zero at start of program
#[test]
fn test_gc_28_bytes_zero_at_start() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-29: object size proportional to field count
#[test]
fn test_gc_29_larger_object_uses_more_bytes() {
    let (out, ok) = compile_and_run(
        r#"
class Small {
    pub init(self, a: i64) {
        self.a = a;
    } a: i64; }
class Large {
    pub init(self, a: i64, b: i64, c: i64, d: i64) {
        self.a = a;
        self.b = b;
        self.c = c;
        self.d = d;
    } a: i64; b: i64; c: i64; d: i64; }
fn main() {
    let before = gc_allocated_bytes();
    let _s = new Small(1);
    let after_small = gc_allocated_bytes();
    let _l = new Large(1, 2, 3, 4);
    let after_large = gc_allocated_bytes();
    println(after_small > before);
    println(after_large > after_small);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// GC-30: GC-managed object returned from function, caller holds it
#[test]
fn test_gc_30_object_returned_and_held_by_caller() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make(v: i64) -> Node { return new Node(v); }
fn main() {
    let n = make(55);
    gc_collect();
    println(n.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "55\n");
}

// GC-31: object passed to function, function holds local copy
#[test]
fn test_gc_31_object_alive_while_in_called_function() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn use_box(b: Box) -> i64 {
    gc_collect();
    return b.get();
}
fn main() {
    let b = new Box(7);
    println(use_box(b));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

// GC-32: two separate collect calls
#[test]
fn test_gc_32_two_separate_collects() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_one() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let r1 = drop_one();
    gc_collect();
    let b = new Box(2);
    let r2 = b.get();
    gc_collect();
    println(r1 + r2);
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\ntrue\n");
}

// GC-33: Option<T> with class payload — freed when out of scope
#[test]
fn test_gc_33_option_class_payload_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_opt() -> i64 {
    let opt = Option::Some(new Node(42));
    return match opt {
        Option::Some(n) => n.get(),
        Option::None => 0,
    };
}
fn main() {
    let _ = make_opt();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-34: Option::Some class payload survives when held
#[test]
fn test_gc_34_option_class_payload_survives_when_held() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let opt = Option::Some(new Node(13));
    gc_collect();
    let v = match opt {
        Option::Some(n) => n.get(),
        Option::None => 0,
    };
    println(v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "13\n");
}

// GC-35: Result::Ok with class payload freed when out of scope
#[test]
fn test_gc_35_result_ok_payload_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_res() -> i64 {
    let r: Result<Node, String> = Result::Ok(new Node(7));
    return match r {
        Result::Ok(n) => n.get(),
        Result::Err(_) => 0,
    };
}
fn main() {
    let _ = make_res();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-36: Result::Ok class payload survives when held
#[test]
fn test_gc_36_result_ok_payload_survives_when_held() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let r: Result<Node, String> = Result::Ok(new Node(17));
    gc_collect();
    let v = match r {
        Result::Ok(n) => n.get(),
        Result::Err(_) => 0,
    };
    println(v);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "17\n");
}

// GC-37: gc_collect does not corrupt live i64 variables
#[test]
fn test_gc_37_collect_does_not_corrupt_i64_vars() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let x = 12345;
    gc_collect();
    println(x);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12345\n");
}

// GC-38: gc_collect does not corrupt live bool variables
#[test]
fn test_gc_38_collect_does_not_corrupt_bool_vars() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let b = true;
    gc_collect();
    println(b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// GC-39: gc_collect does not corrupt live string variables
#[test]
fn test_gc_39_collect_does_not_corrupt_string_vars() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let s = "hello gc";
    gc_collect();
    println(s);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "hello gc\n");
}

// GC-40: object with multiple i64 fields freed correctly
#[test]
fn test_gc_40_multi_i64_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Quad {
    pub init(self, a: i64, b: i64, c: i64, d: i64) {
        self.a = a;
        self.b = b;
        self.c = c;
        self.d = d;
    } a: i64; b: i64; c: i64; d: i64;
    pub fn sum(self) -> i64 { return self.a + self.b + self.c + self.d; }
}
fn make() -> i64 {
    let q = new Quad(1, 2, 3, 4);
    return q.sum();
}
fn main() {
    let r = make();
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n0\n");
}

// GC-41: object allocated in deeply nested function freed
#[test]
fn test_gc_41_deep_nested_alloc_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn f3() -> i64 { let n = new Node(3); return n.get(); }
fn f2() -> i64 { return f3() + f3(); }
fn f1() -> i64 { return f2() + f2(); }
fn main() {
    let _ = f1();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-42: recursive function allocating objects — all freed after recursion
#[test]
fn test_gc_42_recursive_alloc_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn sum(n: i64) -> i64 {
    if n <= 0 { return 0; }
    let node = new Node(n);
    return node.get() + sum(n - 1);
}
fn main() {
    let _ = sum(5);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-43: live object in recursive function survives
#[test]
fn test_gc_43_live_object_in_recursive_fn_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn fib(n: i64) -> i64 {
    if n <= 1 { return n; }
    return fib(n - 1) + fib(n - 2);
}
fn main() {
    let b = new Box(fib(5));
    gc_collect();
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

// GC-44: object stored in multiple variables (aliases) — freed when all out of scope
#[test]
fn test_gc_44_alias_both_out_of_scope_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_two() -> i64 {
    let a = new Node(1);
    let b = a;
    return b.get();
}
fn main() {
    let _ = make_two();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-45: object returned from if-else — retained by caller
#[test]
fn test_gc_45_conditional_returned_object_retained() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class B {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_a(flag: bool) -> i64 {
    if flag {
        let a = new A(10);
        return a.get();
    }
    let b = new B(20);
    return b.get();
}
fn main() {
    let ra = make_a(true);
    let rb = make_a(false);
    gc_collect();
    println(ra);
    println(rb);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n20\n0\n");
}

// GC-46: enum payload (non-class) — no GC impact expected
#[test]
fn test_gc_46_i64_enum_payload_no_gc_impact() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let before = gc_allocated_bytes();
    let opt = Option::Some(42);
    let after = gc_allocated_bytes();
    println(after > before);
    let _ = match opt { Option::Some(v) => v, Option::None => 0 };
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n");
}

// GC-47: gc_collect called with no allocations is safe
#[test]
fn test_gc_47_collect_with_no_allocs_safe() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    gc_collect();
    gc_collect();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-48: large number of objects freed in one collect
#[test]
fn test_gc_48_many_objects_freed_together() {
    let (out, ok) = compile_and_run(
        r#"
class Obj {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make_many() -> i64 {
    let mut sum = 0;
    let mut i = 0;
    while i < 20 {
        let o = new Obj(i);
        sum = sum + o.get();
        i = i + 1;
    }
    return sum;
}
fn main() {
    let _ = make_many();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-49: objects allocated in separate scopes both freed
#[test]
fn test_gc_49_two_scopes_both_freed() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class B {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn scope1() -> i64 { let a = new A(1); return a.get(); }
fn scope2() -> i64 { let b = new B(2); return b.get(); }
fn main() {
    let r1 = scope1();
    let r2 = scope2();
    println(r1 + r2);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n0\n");
}

// GC-50: gc_allocated_bytes is monotonically increasing without collect
#[test]
fn test_gc_50_bytes_monotonically_increasing() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; }
fn main() {
    let b0 = gc_allocated_bytes();
    let _a = new Box(1);
    let b1 = gc_allocated_bytes();
    let _b = new Box(2);
    let b2 = gc_allocated_bytes();
    let _c = new Box(3);
    let b3 = gc_allocated_bytes();
    println(b1 >= b0);
    println(b2 >= b1);
    println(b3 >= b2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\ntrue\n");
}

// GC-51: object with only public fields freed
#[test]
fn test_gc_51_all_public_fields_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Point { pub x: i64; pub y: i64; }
fn drop_it() -> i64 {
    let p = new Point(3, 4);
    return p.x + p.y;
}
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-52: inherited object freed (child)
#[test]
fn test_gc_52_child_class_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64, extra: i64) {
        super.init(v);
        self.extra = extra;
    } extra: i64; pub fn extra(self) -> i64 { return self.extra; } }
fn drop_child() -> i64 {
    let c = new Child(1, 2);
    return c.get() + c.extra();
}
fn main() {
    let r = drop_child();
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n0\n");
}

// GC-53: inherited object survives when live
#[test]
fn test_gc_53_child_class_survives_when_live() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64, extra: i64) {
        super.init(v);
        self.extra = extra;
    } extra: i64; }
fn main() {
    let c = new Child(10, 5);
    gc_collect();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// GC-54: three-level hierarchy object freed
#[test]
fn test_gc_54_three_level_hierarchy_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub open class B extends A {
    pub init(self, v: i64) {
        super.init(v);
    }}
pub class C extends B {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn drop_c() -> i64 { let c = new C(7); return c.get(); }
fn main() {
    let _ = drop_c();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-55: three-level hierarchy object survives when live
#[test]
fn test_gc_55_three_level_hierarchy_survives() {
    let (out, ok) = compile_and_run(
        r#"
pub open class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub open class B extends A {
    pub init(self, v: i64) {
        super.init(v);
    }}
pub class C extends B {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn main() {
    let c = new C(22);
    gc_collect();
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "22\n");
}

// GC-56: object holding child type — all freed
#[test]
fn test_gc_56_object_holding_child_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn process() -> i64 {
    let c = new Child(3);
    let b: Base = c;
    return b.get();
}
fn main() {
    let _ = process();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-57: object method returns new object (both freed after scope)
#[test]
fn test_gc_57_method_returning_new_object_both_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Outer {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn val(self) -> i64 { return self.v; } }
class Inner {
    pub init(self, w: i64) {
        self.w = w;
    } w: i64; pub fn val(self) -> i64 { return self.w; } }
fn compute() -> i64 {
    let o = new Outer(5);
    let i = new Inner(o.val() * 2);
    return i.val();
}
fn main() {
    let _ = compute();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-58: gc_allocated_bytes after one collect then one alloc equals one object
#[test]
fn test_gc_58_bytes_after_collect_then_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class B {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_a() -> i64 { let a = new A(1); return a.get(); }
fn main() {
    let r = drop_a();
    println(r);
    gc_collect();
    let zero = gc_allocated_bytes();
    let b = new B(2);
    let one = gc_allocated_bytes();
    println(zero);
    println(one > 0);
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\ntrue\n2\n");
}

// GC-59: object with f64 field freed
#[test]
fn test_gc_59_f64_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Flt {
    pub init(self, v: f64) {
        self.v = v;
    } v: f64; pub fn get(self) -> f64 { return self.v; } }
fn drop_it() -> f64 { let f = new Flt(1.5); return f.get(); }
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-60: object with f64 field survives collect
#[test]
fn test_gc_60_f64_field_object_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Flt {
    pub init(self, v: f64) {
        self.v = v;
    } v: f64; pub fn get(self) -> f64 { return self.v; } }
fn main() {
    let f = new Flt(2.5);
    gc_collect();
    let r = f.get();
    println(r > 2.0);
    println(r < 3.0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\ntrue\n");
}

// GC-61: nullable object freed when nil-guarded scope exits
#[test]
fn test_gc_61_nullable_freed_when_scope_exits() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn extract(n: Node?) -> i64 {
    if n == nil { return 0; }
    return n.get();
}
fn make_and_extract() -> i64 {
    let n: Node? = new Node(5);
    return extract(n);
}
fn main() {
    let r = make_and_extract();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n0\n");
}

// GC-62: nullable nil field — no allocation
#[test]
fn test_gc_62_nullable_nil_no_extra_alloc() {
    let (out, ok) = compile_and_run(
        r#"
class Node { v: i64; }
fn main() {
    let before = gc_allocated_bytes();
    let n: Node? = nil;
    let after = gc_allocated_bytes();
    println(n == nil);
    println(before);
    println(after);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n0\n0\n");
}

// GC-63: two nullable nodes, one nil — only non-nil freed
#[test]
fn test_gc_63_nullable_one_nil_one_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn make() -> i64 {
    let a: Node? = new Node(1);
    let b: Node? = nil;
    if a != nil { return a.get(); }
    return 0;
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-64: object reachable from function parameter — not freed during call
#[test]
fn test_gc_64_object_not_freed_while_in_param() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn consume(b: Box) -> i64 {
    gc_collect();
    return b.get();
}
fn main() {
    println(consume(new Box(77)));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "77\n");
}

// GC-65: object in Option::Some survives gc when option is live
#[test]
fn test_gc_65_option_some_live_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let opt = Option::Some(new Node(8));
    gc_collect();
    let v = opt.unwrap();
    println(v.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "8\n");
}

// GC-66: gc_collect after empty loop still zero
#[test]
fn test_gc_66_collect_after_zero_iter_loop() {
    let (out, ok) = compile_and_run(
        r#"
class Obj {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; }
fn main() {
    let mut i = 0;
    while i < 0 {
        let _ = new Obj(i);
        i = i + 1;
    }
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-67: object freed after being passed by value and returned as i64
#[test]
fn test_gc_67_pass_by_value_extract_i64_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Wrap {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn extract(w: Wrap) -> i64 { return w.get(); }
fn main() {
    let r = extract(new Wrap(100));
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "100\n0\n");
}

// GC-68: class with inherited prot field freed
#[test]
fn test_gc_68_inherited_prot_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } prot v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    } pub fn doubled(self) -> i64 { return self.v * 2; } }
fn drop_child() -> i64 {
    let c = new Child(4);
    return c.doubled();
}
fn main() {
    let _ = drop_child();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-69: object alive through method chain
#[test]
fn test_gc_69_object_alive_through_method_chain() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } pub fn doubled(self) -> i64 { return self.v * 2; } }
fn main() {
    let n = new Node(5);
    gc_collect();
    let a = n.get();
    let b = n.doubled();
    println(a);
    println(b);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n10\n");
}

// GC-70: allocate, collect, verify zero, allocate again, verify positive
#[test]
fn test_gc_70_alloc_collect_zero_alloc_positive() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_a() -> i64 { let a = new A(1); return a.get(); }
fn main() {
    let r = drop_a();
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
    let b = new A(2);
    println(gc_allocated_bytes() > 0);
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\ntrue\n2\n");
}

// GC-71: deeply nested object reference — all alive while root is live
#[test]
fn test_gc_71_nested_nullable_chain_all_alive() {
    let (out, ok) = compile_and_run(
        r#"
class N { pub v: i64; pub n: N?; }
fn main() {
    let d = new N(3, nil);
    let c = new N(2, d);
    let b = new N(1, c);
    gc_collect();
    println(b.v);
    let bc = b.n;
    if bc != nil {
        println(bc.v);
        let bcc = bc.n;
        if bcc != nil { println(bcc.v); }
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n2\n3\n");
}

// GC-72: object used in conditional — survives both branches
#[test]
fn test_gc_72_object_survives_across_condition() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = new Box(3);
    let flag = b.get() > 2;
    gc_collect();
    if flag {
        println(b.get());
    }
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "3\n");
}

// GC-73: gc does not affect i64 arithmetic result
#[test]
fn test_gc_73_gc_does_not_affect_arithmetic() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let t = new Tmp(10);
    let tv = t.get();
    let x = tv * 3;
    gc_collect();
    println(x);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "30\n");
}

// GC-74: object allocated after collect has fresh identity
#[test]
fn test_gc_74_post_collect_object_fresh() {
    let (out, ok) = compile_and_run(
        r#"
class V {
    pub init(self, val: i64) {
        self.val = val;
    } val: i64; pub fn get(self) -> i64 { return self.val; } }
fn drop_v() -> i64 { let v = new V(1); return v.get(); }
fn main() {
    let _ = drop_v();
    gc_collect();
    let v2 = new V(99);
    println(v2.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "99\n");
}

// GC-75: multiple classes, mixed live and dead
#[test]
fn test_gc_75_mixed_live_dead_multiple_classes() {
    let (out, ok) = compile_and_run(
        r#"
class A {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class B {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class C {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_bc() -> i64 {
    let b = new B(2);
    let c = new C(3);
    return b.get() + c.get();
}
fn main() {
    let a = new A(1);
    let _ = drop_bc();
    gc_collect();
    println(a.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\ntrue\n");
}

// GC-76: object with bool field freed
#[test]
fn test_gc_76_bool_field_object_freed_correctly() {
    let (out, ok) = compile_and_run(
        r#"
class Toggle {
    pub init(self, flag: bool) {
        self.flag = flag;
    } flag: bool; pub fn get(self) -> bool { return self.flag; } }
fn drop_toggle() -> bool {
    let t = new Toggle(false);
    return t.get();
}
fn main() {
    let _ = drop_toggle();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-77: object survives across multiple function calls
#[test]
fn test_gc_77_object_survives_multiple_fn_calls() {
    let (out, ok) = compile_and_run(
        r#"
class Acc {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn use_acc(a: Acc) -> i64 { return a.get(); }
fn main() {
    let a = new Acc(5);
    let r1 = use_acc(a);
    gc_collect();
    let r2 = use_acc(a);
    gc_collect();
    println(r1 + r2);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "10\n");
}

// GC-78: interleaved alloc/collect/alloc/collect stays consistent
#[test]
fn test_gc_78_interleaved_alloc_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_box(v: i64) -> i64 { let b = new Box(v); return b.get(); }
fn main() {
    let r1 = drop_box(1);
    gc_collect();
    let r2 = drop_box(2);
    gc_collect();
    let r3 = drop_box(3);
    gc_collect();
    println(r1 + r2 + r3);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n");
}

// GC-79: object created inside match arm freed
#[test]
fn test_gc_79_object_in_match_arm_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn wrap(v: i64) -> i64 {
    let t = new Tmp(v);
    return t.get();
}
fn main() {
    let r = match Option::Some(9) {
        Option::Some(v) => wrap(v),
        Option::None => 0,
    };
    println(r);
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "9\n0\n");
}

// GC-80: subtype object used as base type — GC still works
#[test]
fn test_gc_80_subtype_as_base_gc_works() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn process(b: Base) -> i64 { return b.get(); }
fn make() -> i64 {
    let c = new Child(6);
    return process(c);
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-81: object method call does not prevent GC after scope
#[test]
fn test_gc_81_method_call_then_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, n: i64) {
        self.n = n;
    } n: i64; pub fn next(self) -> i64 { return self.n + 1; } }
fn run() -> i64 {
    let c = new Counter(0);
    return c.next() + c.next();
}
fn main() {
    let _ = run();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-82: object with optional class field — field freed with owner
#[test]
fn test_gc_82_optional_class_field_freed_with_owner() {
    let (out, ok) = compile_and_run(
        r#"
class Inner {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
class Outer {
    pub init(self, child: Inner?) {
        self.child = child;
    } pub child: Inner?; }
fn make() -> i64 {
    let i = new Inner(3);
    let o = new Outer(i);
    let c = o.child;
    if c != nil { return c.get(); }
    return 0;
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-83: object with nil optional field — freed without following nil
#[test]
fn test_gc_83_nil_optional_field_freed_safely() {
    let (out, ok) = compile_and_run(
        r#"
class Outer {
    pub init(self, child: Inner?, v: i64) {
        self.child = child;
        self.v = v;
    } child: Inner?; v: i64; pub fn get(self) -> i64 { return self.v; } }
class Inner {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; }
fn make() -> i64 {
    let o = new Outer(nil, 7);
    return o.get();
}
fn main() {
    let _ = make();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-84: gc does not corrupt i64 return value from function
#[test]
fn test_gc_84_gc_does_not_corrupt_return_value() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn compute() -> i64 {
    let b = new Box(42);
    let result = b.get();
    gc_collect();
    return result;
}
fn main() {
    println(compute());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// GC-85: function allocating then calling gc_collect internally
#[test]
fn test_gc_85_gc_inside_allocating_function() {
    let (out, ok) = compile_and_run(
        r#"
class Node {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn alloc_and_get() -> i64 {
    let n = new Node(3);
    return n.get();
}
fn main() {
    let r1 = alloc_and_get();
    let r2 = alloc_and_get();
    gc_collect();
    println(r1 + r2);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n");
}

// GC-86: zero-field class object allocated and freed
#[test]
fn test_gc_86_zero_field_class_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Empty {}
fn drop_it() -> i64 {
    let _e = new Empty();
    return 1;
}
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-87: zero-field class object alive survives collect
#[test]
fn test_gc_87_zero_field_class_survives() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Empty { pub fn tag(self) -> i64 { return 0; } }
fn main() {
    let e = new Empty();
    gc_collect();
    println(gc_allocated_bytes() > 0);
    println(e.tag());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "true\n0\n");
}

// GC-88: child zero-field class extends parent with field — freed
#[test]
fn test_gc_88_child_inherits_field_both_freed() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Empty extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn drop_it() -> i64 { let e = new Empty(9); return e.get(); }
fn main() {
    let _ = drop_it();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-89: object surviving ternary expression
#[test]
fn test_gc_89_object_survives_ternary() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let b = new Box(7);
    let x = b.get() > 5 ? 1 : 0;
    gc_collect();
    println(x);
    println(b.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n7\n");
}

// GC-90: object with four fields — all freed when dead
#[test]
fn test_gc_90_four_field_object_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Quad {
    pub init(self, a: i64, b: i64, c: i64, d: i64) {
        self.a = a;
        self.b = b;
        self.c = c;
        self.d = d;
    } a: i64; b: i64; c: i64; d: i64;
    pub fn sum(self) -> i64 { return self.a + self.b + self.c + self.d; }
}
fn drop_quad() -> i64 {
    let q = new Quad(1, 2, 3, 4);
    return q.sum();
}
fn main() {
    let _ = drop_quad();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-91: object as function return — freed when caller doesn't store it
#[test]
fn test_gc_91_returned_object_not_stored_freed() {
    let (out, ok) = compile_and_run(
        r#"
class V {
    pub init(self, val: i64) {
        self.val = val;
    } val: i64; pub fn get(self) -> i64 { return self.val; } }
fn make_v() -> V { return new V(5); }
fn main() {
    let _ = make_v().get();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-92: object stored temporarily in variable then dropped
#[test]
fn test_gc_92_temp_stored_then_dropped() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn use_temp() -> i64 {
    let tmp = new Box(3);
    let tv = tmp.get();
    let result = tv * 2;
    return result;
}
fn main() {
    let r = use_temp();
    gc_collect();
    println(r);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "6\n0\n");
}

// GC-93: child and parent object both allocated, child freed first
#[test]
fn test_gc_93_parent_child_child_freed_parent_live() {
    let (out, ok) = compile_and_run(
        r#"
pub open class Base {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
pub class Child extends Base {
    pub init(self, v: i64) {
        super.init(v);
    }}
fn drop_child() -> i64 { let c = new Child(2); return c.get(); }
fn main() {
    let parent = new Base(1);
    let _ = drop_child();
    gc_collect();
    println(parent.get());
    println(gc_allocated_bytes() > 0);
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\ntrue\n");
}

// GC-94: allocate inside while body, free each iteration via scope
#[test]
fn test_gc_94_while_body_alloc_freed_each_iter() {
    let (out, ok) = compile_and_run(
        r#"
class Tmp {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn main() {
    let mut i = 0;
    let mut acc = 0;
    while i < 4 {
        let t = new Tmp(i * i);
        acc = acc + t.get();
        i = i + 1;
    }
    gc_collect();
    println(acc);
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "14\n0\n");
}

// GC-95: object with both pub and prot fields freed
#[test]
fn test_gc_95_mixed_visibility_fields_freed() {
    let (out, ok) = compile_and_run(
        r#"
class Mixed {
    pub init(self, a: i64, b: i64) {
        self.a = a;
        self.b = b;
    } pub a: i64; prot b: i64; pub fn sum(self) -> i64 { return self.a + self.b; } }
fn drop_mixed() -> i64 { let m = new Mixed(3, 4); return m.sum(); }
fn main() {
    let _ = drop_mixed();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-96: object alive when used as argument to function that gc_collects
#[test]
fn test_gc_96_alive_during_fn_that_collects() {
    let (out, ok) = compile_and_run(
        r#"
class Key {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn expensive(k: Key) -> i64 {
    gc_collect();
    return k.get();
}
fn main() {
    let k = new Key(13);
    println(expensive(k));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "13\n");
}

// GC-97: collect between two allocs — second survives
#[test]
fn test_gc_97_collect_between_allocs_second_survives() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_one() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let _ = drop_one();
    gc_collect();
    let b2 = new Box(50);
    println(b2.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "50\n");
}

// GC-98: gc_collect idempotent — calling twice is safe
#[test]
fn test_gc_98_double_collect_idempotent() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_box() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let _ = drop_box();
    gc_collect();
    gc_collect();
    println(gc_allocated_bytes());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "0\n");
}

// GC-99: live object across gc_collect in a loop
#[test]
fn test_gc_99_live_across_collect_in_loop() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, n: i64) {
        self.n = n;
    } n: i64; pub fn get(self) -> i64 { return self.n; } }
fn main() {
    let c = new Counter(42);
    let mut i = 0;
    while i < 3 {
        gc_collect();
        i = i + 1;
    }
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

// GC-100: gc_allocated_bytes tracks only live bytes after collect
#[test]
fn test_gc_100_bytes_tracks_only_live_after_collect() {
    let (out, ok) = compile_and_run(
        r#"
class Box {
    pub init(self, v: i64) {
        self.v = v;
    } v: i64; pub fn get(self) -> i64 { return self.v; } }
fn drop_box() -> i64 { let b = new Box(1); return b.get(); }
fn main() {
    let r = drop_box();
    println(r);
    gc_collect();
    let zero = gc_allocated_bytes();
    let live = new Box(2);
    let nonzero = gc_allocated_bytes();
    gc_collect();
    let still_nonzero = gc_allocated_bytes();
    println(zero);
    println(nonzero > 0);
    println(still_nonzero > 0);
    println(live.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "1\n0\ntrue\ntrue\n2\n");
}

// ── self receiver semantics ─────────────────────────────────────────────────

#[test]
fn test_self_field_read_and_assignment() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn inc(self) { self.n = self.n + 1; }
    pub fn add(self, n: i64) { self.n = self.n + n; }
    pub fn get(self) -> i64 { return self.n; }
}
fn main() {
    let c = new Counter(0);
    c.inc();
    c.inc();
    c.add(5);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "7\n");
}

#[test]
fn test_self_method_call_inside_method() {
    let (out, ok) = compile_and_run(
        r#"
class Wrap {
    pub init(self, n: i64) {
        self.n = n;
    }
    n: i64;
    pub fn double(self) -> i64 { return self.n * 2; }
    pub fn compute(self) -> i64 { return self.double() + 1; }
}
fn main() {
    let w = new Wrap(5);
    println(w.compute());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "11\n");
}

#[test]
fn test_self_accesses_and_assigns_inherited_field() {
    let (out, ok) = compile_and_run(
        r#"
open class Base {
    pub score: i64;
}
class Child extends Base {
    pub fn boost(self, n: i64) { self.score = self.score + n; }
    pub fn get(self) -> i64 { return self.score; }
}
fn main() {
    let c = new Child(10);
    c.boost(5);
    println(c.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n");
}

#[test]
fn test_self_field_assign_inside_control_flow() {
    let (out, ok) = compile_and_run(
        r#"
class Acc {
    pub init(self, total: i64) {
        self.total = total;
    }
    total: i64;
    pub fn accumulate(self, n: i64) {
        let mut i = 0;
        while i < n {
            if i >= 0 {
                self.total = self.total + 1;
            }
            i = i + 1;
        }
    }
    pub fn get(self) -> i64 { return self.total; }
}
fn main() {
    let a = new Acc(0);
    a.accumulate(5);
    println(a.get());
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "5\n");
}

#[test]
fn test_static_and_instance_methods_coexist() {
    let (out, ok) = compile_and_run(
        r#"
class Adder {
    pub init(self, base: i64) {
        self.base = base;
    }
    base: i64;
    pub fn add_base(self, n: i64) -> i64 { return self.base + n; }
    pub static fn pure(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let a = new Adder(10);
    println(a.add_base(5));
    println(Adder::pure(2, 3));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "15\n5\n");
}

#[test]
fn test_self_upper_static_call_inside_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Counter {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub static fn make(value: i64) -> Counter { return new Counter(value); }
    pub fn clone_plus(self, n: i64) -> i64 {
        let next = Self::make(self.value + n);
        return next.value;
    }
}
fn main() {
    let c = new Counter(8);
    println(c.clone_plus(4));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "12\n");
}

#[test]
fn test_self_lower_static_call_inside_instance_method() {
    let (out, ok) = compile_and_run(
        r#"
class Math {
    pub init(self, value: i64) {
        self.value = value;
    }
    value: i64;
    pub static fn pure(a: i64, b: i64) -> i64 { return a + b; }
    pub fn add_to_value(self, n: i64) -> i64 {
        return self::pure(self.value, n);
    }
}
fn main() {
    let m = new Math(20);
    println(m.add_to_value(22));
}
"#,
    );
    assert!(ok);
    assert_eq!(out, "42\n");
}

#[test]
fn test_instance_method_called_with_static_syntax_is_error() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn get(self) -> i64 { return self.v; }
    pub fn bad(self) -> i64 { return Self::get(); }
}
fn main() {}
"#,
        &[
            "instance method called with `::`",
            "write `self.get` instead",
        ],
    );
}

#[test]
fn test_static_method_called_with_dot_is_error() {
    assert_compile_error_contains(
        r#"
class Math {
    pub static fn add(a: i64, b: i64) -> i64 { return a + b; }
}
fn main() {
    let m = new Math();
    println(m.add(1, 2));
}
"#,
        &["static method called with `.`", "write `Math::add` instead"],
    );
}

#[test]
fn test_self_static_call_outside_class_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(Self::make());
}
"#,
        &["`Self` can only be used inside a class method"],
    );
}

#[test]
fn test_legacy_this_receiver_is_error() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn get(self) -> i64 { return this.v; }
}
fn main() {}
"#,
        &["receiver alias `this` is not supported", "use `self`"],
    );
}

#[test]
fn test_legacy_this_identifier_declaration_is_error() {
    assert_compile_error_contains(
        r#"
fn main() {
    let this = 1;
}
"#,
        &["identifier `this` is reserved", "use `self`"],
    );
}

#[test]
fn test_self_in_static_method_is_error() {
    assert_compile_error_contains(
        r#"
class Math {
    pub static fn bad() -> i64 {
        return self.value;
    }
}
fn main() {}
"#,
        &["`self` is not available in static method"],
    );
}

#[test]
fn test_assign_to_self_is_error() {
    assert_compile_error_contains(
        r#"
class Box {
    v: i64;
    pub fn bad(self) {
        self = new Box(1);
    }
}
fn main() {}
"#,
        &["cannot assign to `self`"],
    );
}
