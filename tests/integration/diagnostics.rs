use super::support::*;

#[test]
fn test_diagnostics_include_codes_and_help() {
    let cases: Vec<(&str, &[&str])> = vec![
        (
            r#"
fn main() {
    let x = 1;
    x = 2;
}
"#,
            &[
                "error[E0301]",
                "cannot assign to immutable variable `x`",
                "help: declare it as mutable",
            ],
        ),
        (
            r#"
fn main() {
    let x = 1
}
"#,
            &["error[E0101]", "expected `;` after statement"],
        ),
        (
            r#"
fn main() {
    let x: i64 = true;
}
"#,
            &["error[E0201]", "expected `i64`, found `bool`"],
        ),
        (
            r#"
fn main() {
    if 1 {
        println(1);
    }
}
"#,
            &[
                "error[E0203]",
                "condition must be `bool`",
                "help: use an explicit comparison",
            ],
        ),
        (
            r#"
fn main() {
    println(missing);
}
"#,
            &["error[E0350]", "cannot find variable `missing`"],
        ),
        (
            r#"
fn main() {
    let @x = 1;
}
"#,
            &["error[E0050]", "invalid character `@`"],
        ),
        (
            r#"
fn add(a: i64, b: i64) -> i64 {
    return a + b;
}

fn main() {
    println(add(1));
}
"#,
            &[
                "error[E0201]",
                "function `add` takes 2 argument(s) but 1 were supplied",
                "wrong number of arguments",
            ],
        ),
        (
            r#"
fn main() {
    missing();
}
"#,
            &[
                "error[E0350]",
                "cannot find function `missing`",
                "not found in this scope",
            ],
        ),
        (
            r#"
fn main() {
    println("hello);
}
"#,
            &[
                "error[E0051]",
                "unterminated string literal",
                "string starts here but never ends",
            ],
        ),
    ];

    for (source, expected_parts) in cases {
        let stderr = compile_error_stderr(source);
        for part in expected_parts {
            assert!(
                stderr.contains(part),
                "stderr did not contain `{part}`:\n{stderr}"
            );
        }
    }
}

#[test]
fn test_diagnostic_wrong_argument_type_details() {
    assert_compile_error_contains(
        r#"
fn takes_i64(x: i64) -> i64 {
    return x;
}

fn main() {
    println(takes_i64(true));
}
"#,
        &["error[E0201]", "expected `i64`", "found `bool`"],
    );
}

#[test]
fn test_diagnostic_return_type_mismatch_details() {
    assert_compile_error_contains(
        r#"
fn bad() -> i64 {
    return false;
}

fn main() {
    println(1);
}
"#,
        &["error[E0201]", "expected `i64`", "found `bool`"],
    );
}

#[test]
fn test_diagnostic_arithmetic_type_mismatch_details() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(1 + true);
}
"#,
        &["error[E0202]", "operator `+`", "`i64`", "`bool`"],
    );
}

#[test]
fn test_diagnostic_logical_operand_details() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(true && 1);
}
"#,
        &["error[E0202]", "logical operator requires `bool` operands"],
    );
}

#[test]
fn test_diagnostic_unary_not_requires_bool_details() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(!1);
}
"#,
        &["error[E0202]", "unary `!`", "requires `bool`"],
    );
}

#[test]
fn test_diagnostic_while_condition_details() {
    assert_compile_error_contains(
        r#"
fn main() {
    while 1 {
        println(1);
    }
}
"#,
        &[
            "error[E0203]",
            "condition must be `bool`",
            "found `i64`",
            "use an explicit comparison",
        ],
    );
}

#[test]
fn test_diagnostic_assignment_type_mismatch_details() {
    assert_compile_error_contains(
        r#"
fn main() {
    let mut x = 1;
    x = false;
}
"#,
        &["error[E0201]", "expected `i64`", "found `bool`"],
    );
}

#[test]
fn test_diagnostic_immutable_assignment_points_to_declaration() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = 1;
    x = 2;
}
"#,
        &[
            "error[E0301]",
            "cannot assign to immutable variable `x`",
            "4 |     x = 2;",
            "^ cannot assign",
            "3 |     let x = 1;",
            "declared immutable here",
            "help: declare it as mutable: `let mut x = ...`",
        ],
    );
}

#[test]
fn test_diagnostic_parameter_assignment_has_parameter_code() {
    assert_compile_error_contains(
        r#"
fn reset(x: i64) {
    x = 0;
}

fn main() {
    reset(1);
}
"#,
        &[
            "error[E0302]",
            "cannot assign to immutable parameter `x`",
            "introduce a mutable local variable",
        ],
    );
}

#[test]
fn test_diagnostic_duplicate_variable_points_to_redefinition() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = 1;
    let x = 2;
    println(x);
}
"#,
        &[
            "error[E0351]",
            "variable `x` is already defined",
            "previous definition",
        ],
    );
}

#[test]
fn test_diagnostic_local_scope_does_not_leak() {
    assert_compile_error_contains(
        r#"
fn main() {
    if true {
        let inside = 1;
    }

    println(inside);
}
"#,
        &["error[E0350]", "cannot find variable `inside`"],
    );
}

#[test]
fn test_diagnostic_invalid_character_is_source_aware() {
    let stderr = compile_error_stderr(
        r#"
fn main() {
    let @x = 1;
}
"#,
    );
    let temp_diag_prefix = format!(" --> {}", temp_path("willow_diag_"));
    for part in [
        "error[E0050]",
        "invalid character `@`",
        temp_diag_prefix.as_str(),
        ":3:9",
        "3 |     let @x = 1;",
        "        ^ invalid character",
    ] {
        assert!(
            stderr.contains(part),
            "stderr did not contain `{part}`:\n{stderr}"
        );
    }
}

#[test]
fn test_diagnostic_single_ampersand_outside_call_arg_is_invalid() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(true & false);
}
"#,
        &[
            "error[E0102]",
            "`&` is only valid before a call argument",
            ":3:18",
            "3 |     println(true & false);",
            "                 ^ `&` is only valid before a call argument",
        ],
    );
}

#[test]
fn test_reference_escape_return_expression_is_rejected() {
    assert_compile_error_contains(
        r#"
fn leak() -> i64 {
    let value = 1;
    return &value;
}

fn main() {
    println(1);
}
"#,
        &["error[E0102]", "`&` is only valid before a call argument"],
    );
}

#[test]
fn test_diagnostic_single_pipe_in_call_is_parse_error() {
    // `|` is now the lambda-param delimiter, so `true | false` inside a call
    // is a parse error (unexpected token while expecting `)` or `,`).
    assert!(expect_compile_error(
        r#"
fn main() {
    println(true | false);
}
"#
    ));
}

#[test]
fn test_diagnostic_unterminated_string_is_source_aware() {
    assert_compile_error_contains(
        r#"
fn main() {
    println("hello);
}
"#,
        &[
            "error[E0051]",
            "unterminated string literal",
            ":3:13",
            "3 |     println(\"hello);",
            "string starts here but never ends",
        ],
    );
}

#[test]
fn test_diagnostic_missing_return_value_from_non_void_function() {
    assert_compile_error_contains(
        r#"
fn bad() -> i64 {
    return;
}

fn main() {
    println(1);
}
"#,
        &["error[E0201]", "expected `i64`", "found `void`"],
    );
}

#[test]
fn test_diagnostic_return_value_from_void_function() {
    assert_compile_error_contains(
        r#"
fn bad() {
    return 1;
}

fn main() {
    bad();
}
"#,
        &["error[E0201]", "expected `void`", "found `i64`"],
    );
}

#[test]
fn test_error_immutable_assign() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = 1;
    x = 2;
}
"#
    ));
}

#[test]
fn test_error_type_mismatch() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x: i64 = true;
}
"#
    ));
}

#[test]
fn test_error_condition_not_bool() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = 5;
    if x { println(x); }
}
"#
    ));
}

#[test]
fn test_error_undefined_variable() {
    assert!(expect_compile_error(
        r#"
fn main() {
    println(undefined_var);
}
"#
    ));
}

#[test]
fn test_error_invalid_char() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let @x = 1;
}
"#
    ));
}

#[test]
fn test_error_missing_semicolon() {
    assert!(expect_compile_error(
        r#"
fn main() {
    let x = 1
}
"#
    ));
}

#[test]
fn test_error_wrong_argument_count() {
    assert!(expect_compile_error(
        r#"
fn add(a: i64, b: i64) -> i64 {
    return a + b;
}

fn main() {
    println(add(1));
}
"#
    ));
}

#[test]
fn test_error_wrong_argument_type() {
    assert!(expect_compile_error(
        r#"
fn takes_i64(x: i64) -> i64 {
    return x;
}

fn main() {
    println(takes_i64(true));
}
"#
    ));
}

#[test]
fn test_error_unknown_function() {
    assert!(expect_compile_error(
        r#"
fn main() {
    missing();
}
"#
    ));
}

#[test]
fn test_error_return_type_mismatch() {
    assert!(expect_compile_error(
        r#"
fn bad() -> i64 {
    return false;
}

fn main() {
    println(1);
}
"#
    ));
}

#[test]
fn test_error_arithmetic_type_mismatch() {
    assert!(expect_compile_error(
        r#"
fn main() {
    println(1 + true);
}
"#
    ));
}

#[test]
fn test_error_logical_operand_not_bool() {
    assert!(expect_compile_error(
        r#"
fn main() {
    println(true && 1);
}
"#
    ));
}

#[test]
fn test_error_unary_not_requires_bool() {
    assert!(expect_compile_error(
        r#"
fn main() {
    println(!1);
}
"#
    ));
}

#[test]
fn test_error_while_condition_not_bool() {
    assert!(expect_compile_error(
        r#"
fn main() {
    while 1 {
        println(1);
    }
}
"#
    ));
}

#[test]
fn test_error_assign_to_parameter() {
    assert!(expect_compile_error(
        r#"
fn reset(x: i64) {
    x = 0;
}

fn main() {
    reset(1);
}
"#
    ));
}

#[test]
fn test_error_unterminated_string() {
    assert!(expect_compile_error(
        r#"
fn main() {
    println("hello);
}
"#
    ));
}

// ── Function values and lambdas ───────────────────────────────────────────────

#[test]
fn test_named_function_passed_as_argument() {
    let (out, ok) = compile_and_run(
        r#"
fn double(x: i64) -> i64 {
    return x * 2;
}
fn apply(x: i64, f: fn(i64) -> i64) -> i64 {
    return f(x);
}
fn main() {
    println(apply(10, double));
}
"#,
    );
    assert!(ok, "compilation failed");
    assert_eq!(out, "20\n");
}

#[test]
fn test_non_capturing_lambda_as_argument() {
    let (out, ok) = compile_and_run(
        r#"
fn apply(x: i64, f: fn(i64) -> i64) -> i64 {
    return f(x);
}
fn main() {
    println(apply(10, |x: i64| x * 2));
}
"#,
    );
    assert!(ok, "compilation failed");
    assert_eq!(out, "20\n");
}

#[test]
fn test_lambda_stored_in_variable_and_called() {
    let (out, ok) = compile_and_run(
        r#"
fn main() {
    let f: fn(i64) -> i64 = |x: i64| x + 5;
    println(f(10));
    println(f(20));
}
"#,
    );
    assert!(ok, "compilation failed");
    assert_eq!(out, "15\n25\n");
}

#[test]
fn test_zero_param_lambda() {
    let (out, ok) = compile_and_run(
        r#"
fn call(f: fn() -> i64) -> i64 {
    return f();
}
fn main() {
    println(call(|| 42));
}
"#,
    );
    assert!(ok, "compilation failed");
    assert_eq!(out, "42\n");
}

#[test]
fn test_lambda_with_block_body() {
    let (out, ok) = compile_and_run(
        r#"
fn apply(x: i64, f: fn(i64) -> i64) -> i64 {
    return f(x);
}
fn main() {
    let result = apply(5, |x: i64| {
        let y = x * x;
        return y + 1;
    });
    println(result);
}
"#,
    );
    assert!(ok, "compilation failed");
    assert_eq!(out, "26\n");
}

// ── Parser error focused tests ───────────────────────────────────────────────

#[test]
fn test_diagnostic_missing_closing_brace() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = 1;
"#,
        &["error[E0103]", "expected `}` to close block"],
    );
}

#[test]
fn test_diagnostic_missing_closing_paren() {
    assert_compile_error_contains(
        r#"
fn main() {
    println(1;
}
"#,
        &["error[E0104]", "expected `)` to close parenthesis"],
    );
}

#[test]
fn test_diagnostic_invalid_function_name() {
    assert_compile_error_contains(
        r#"
fn () {
}
fn main() {}
"#,
        &["error[E0102]", "expected identifier"],
    );
}

#[test]
fn test_diagnostic_invalid_type_annotation() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x: = 1;
}
"#,
        &["error[E0107]"],
    );
}

#[test]
fn test_diagnostic_missing_semicolon_has_code() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = 1
}
"#,
        &["error[E0101]", "expected `;` after statement"],
    );
}

// ── Multiple independent diagnostics ─────────────────────────────────────────

#[test]
fn test_multiple_type_errors_in_different_functions_all_reported() {
    // Two independent arithmetic type mismatches in separate functions.
    // Both should be reported in a single compile run.
    let stderr = compile_error_stderr(
        r#"
fn bad_add(a: i64, b: bool) -> i64 {
    return a + b;
}

fn bad_mul(x: bool, y: i64) -> i64 {
    return x * y;
}

fn main() {
    bad_add(1, true);
    bad_mul(false, 2);
}
"#,
    );
    let error_count = stderr.matches("error[").count();
    assert!(
        error_count >= 2,
        "expected at least 2 independent errors, got {error_count}:\n{stderr}"
    );
}

#[test]
fn test_multiple_parse_errors_all_reported() {
    // Two functions with missing closing braces.
    // The parser recovers after each item and should surface both errors.
    let stderr = compile_error_stderr(
        r#"
fn a() -> i64 {
    return @

fn b() -> i64 {
    return #
}

fn main() {}
"#,
    );
    let error_count = stderr.matches("error[").count();
    assert!(
        error_count >= 2,
        "expected at least 2 parse errors, got {error_count}:\n{stderr}"
    );
}

#[test]
fn test_parse_errors_and_type_errors_both_reported() {
    // One function has a parse error (missing RHS); another has a type error.
    // The pipeline should continue past the parse error and report both.
    let stderr = compile_error_stderr(
        r#"
fn bad_parse() -> i64 {
    return 1 +;
}

fn bad_type(x: bool) -> i64 {
    return x + 1;
}

fn main() {}
"#,
    );
    let error_count = stderr.matches("error[").count();
    assert!(
        error_count >= 2,
        "expected parse error and type error both reported, got {error_count}:\n{stderr}"
    );
}

// ── Fix suggestions ───────────────────────────────────────────────────────────

#[test]
fn test_fix_suggestion_immutable_shows_mut_insertion() {
    // E0301 must show a code fix block with `mut ` inserted after `let `.
    assert_compile_error_contains(
        r#"
fn main() {
    let count = 0;
    count = count + 1;
}
"#,
        &[
            "error[E0301]",
            "cannot assign to immutable variable `count`",
            "help: declare it as mutable",
            // fix block: modified source line and `+` markers
            "let mut count = 0;",
            "++++",
        ],
    );
}

#[test]
fn test_fix_suggestion_secondary_label_shown_before_fix() {
    // The secondary "declared immutable here" label must appear in the error
    // body, and the fix block must appear after the help line.
    let stderr = compile_error_stderr(
        r#"
fn main() {
    let x = 1;
    x = 2;
}
"#,
    );
    let label_pos = stderr
        .find("declared immutable here")
        .expect("missing secondary label");
    let fix_pos = stderr.find("let mut x = 1;").expect("missing fix line");
    assert!(
        label_pos < fix_pos,
        "secondary label should appear before the fix block:\n{stderr}"
    );
}

// ── Nil dereference runtime check ────────────────────────────────────────────

/// Debug builds emit a nil pointer check before every field access and method
/// call.  Correct programs must not trigger the check.
#[test]
fn test_nil_deref_check_does_not_fire_for_valid_field_access() {
    let src = r#"
class Box {
    pub value: i64;
}

fn read(b: Box) -> i64 {
    return b.value;
}

fn main() {
    let b = new Box(42);
    println(read(b));
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "field access in debug mode should compile and run");
    assert_eq!(out, "42\n");
}

/// Debug builds also guard method calls.  Valid calls must complete normally.
#[test]
fn test_nil_deref_check_does_not_fire_for_valid_method_call() {
    let src = r#"
class Counter {
    pub count: i64;

    pub fn get(self) -> i64 {
        return self.count;
    }
}

fn main() {
    let c = new Counter(7);
    println(c.get());
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "method call in debug mode should compile and run");
    assert_eq!(out, "7\n");
}

/// Nil-narrowing: after `!= nil` guard the field access must succeed without
/// triggering the nil check.
#[test]
fn test_nil_deref_check_does_not_fire_after_nil_narrowing() {
    let src = r#"
class Node {
    pub value: i64;
    pub next: Node?;
}

fn sum_chain(n: Node?) -> i64 {
    if n == nil {
        return 0;
    }
    let nxt = n.next;
    if nxt != nil {
        return n.value + nxt.value;
    }
    return n.value;
}

fn main() {
    let b = new Node(20, nil);
    let a = new Node(10, b);
    println(sum_chain(a));   // 30
    println(sum_chain(b));   // 20
    let c: Node? = nil;
    println(sum_chain(c));   // 0
}
"#;
    let (out, ok) = compile_and_run(src);
    assert!(ok, "nil narrowing should not trigger nil deref check");
    assert_eq!(out, "30\n20\n0\n");
}

/// Release builds should not emit nil checks.
/// Verify by running the same program under --release and confirming it still
/// works (no false check) and that it does not print a nil-deref message.
#[test]
fn test_nil_deref_check_absent_in_release_build() {
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_nil_rel_{}.wi", id));
    let bin_path = temp_path(format!("willow_nil_rel_{}", id));

    let source = r#"
class Box { pub value: i64; }
fn read(b: Box) -> i64 { return b.value; }
fn main() { println(read(new Box(99))); }
"#;
    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path, "--release"])
        .output()
        .expect("failed to run compiler");

    assert!(output.status.success(), "release build should succeed");

    let run_output = std::process::Command::new(&bin_path)
        .output()
        .expect("failed to run binary");

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    let stdout = String::from_utf8_lossy(&run_output.stdout);
    let stderr = String::from_utf8_lossy(&run_output.stderr);

    assert_eq!(
        stdout.trim(),
        "99",
        "release binary should print correct output"
    );
    assert!(
        !stderr.contains("nil dereference"),
        "release binary should not print nil dereference message; stderr: {stderr}"
    );
}

/// The nil deref diagnostic string must be present in the C runtime (which is
/// always linked in).  This is the message that would be shown at runtime when
/// the check fires.
#[test]
fn test_nil_deref_runtime_message_is_embedded_in_binary() {
    let source = r#"
class Box { pub value: i64; }
fn main() { println(new Box(1).value); }
"#;
    let id = unique_test_id();
    let src_path = temp_path(format!("willow_nil_msg_{}.wi", id));
    let bin_path = temp_path(format!("willow_nil_msg_{}", id));

    std::fs::write(&src_path, source).unwrap();

    let compiler = env!("CARGO_BIN_EXE_willowc");
    let output = std::process::Command::new(compiler)
        .args(["build", &src_path, "-o", &bin_path])
        .output()
        .expect("failed to compile");
    assert!(output.status.success(), "should compile");

    let binary = std::fs::read(&bin_path).expect("binary should exist");
    let content = String::from_utf8_lossy(&binary);

    let _ = std::fs::remove_file(&src_path);
    let _ = std::fs::remove_file(&bin_path);
    let _ = std::fs::remove_file(format!("{bin_path}.wsmap"));

    assert!(
        content.contains("nil dereference"),
        "binary should contain nil dereference diagnostic message"
    );
}

// ── Match expression tests ─────────────────────────────────────────────────────
