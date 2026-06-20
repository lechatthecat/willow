use super::support::*;

#[test]
fn lexer_diag_integer_overflow_e0052() {
    assert_compile_error_contains(
        r#"
fn main() {
    let x = 99999999999999999999;
    println(x);
}
"#,
        &["error[E0052]", "out of range for `i64`"],
    );
}

// End-to-end: an unterminated block comment surfaces as E0053.
#[test]
fn lexer_diag_unterminated_block_comment_e0053() {
    assert_compile_error_contains(
        r#"
fn main() {
    /* this comment never closes
    println(1);
}
"#,
        &["error[E0053]", "unterminated block comment"],
    );
}

// End-to-end: a valid (nested) block comment compiles and runs.
#[test]
fn lexer_diag_block_comment_compiles_and_runs() {
    let (out, ok) = compile_and_run(
        r#"
/* header /* nested */ comment */
fn main() {
    let a = 10; /* inline */ let b = 20;
    println(a + b);
}
"#,
    );
    assert!(ok, "block comments should compile");
    assert_eq!(out, "30\n");
}
