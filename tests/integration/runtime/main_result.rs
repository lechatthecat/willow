use super::*;

// ── fn main() -> Result<void, E> (willow-exg) ────────────────────────────────

#[test]
fn main_result_01_err_prints_and_exits_nonzero() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, String> {
    return Result::Err("boom");
}
"#,
    );
    assert!(!ok, "Err main must exit non-zero");
    assert!(
        out.contains("boom"),
        "Err report must include the message: {out}"
    );
}

#[test]
fn main_result_02_ok_exits_zero() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, String> {
    println(7);
    return Result::Ok();
}
"#,
    );
    assert!(ok, "Ok main must exit 0: {out}");
    assert_eq!(out, "7\n");
}

#[test]
fn main_result_03_implicit_end_is_success() {
    // Falling off the end of a Result<void,E> main is success (exit 0).
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, String> {
    println(99);
}
"#,
    );
    assert!(ok, "implicit-end main must exit 0: {out}");
    assert_eq!(out, "99\n");
}

#[test]
fn main_result_04_question_mark_propagates_err() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn risky(ok: bool) -> Result<i64, String> {
    if ok { return Result::Ok(7); }
    return Result::Err("propagated");
}
fn main() -> Result<void, String> {
    let x = risky(false)?;
    println(x);
    return Result::Ok();
}
"#,
    );
    assert!(!ok, "? propagating Err must exit non-zero");
    assert!(
        out.contains("propagated"),
        "should report the propagated error: {out}"
    );
}

#[test]
fn main_result_05_question_mark_success_path() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn risky(ok: bool) -> Result<i64, String> {
    if ok { return Result::Ok(7); }
    return Result::Err("nope");
}
fn main() -> Result<void, String> {
    let x = risky(true)?;
    println(x);
    return Result::Ok();
}
"#,
    );
    assert!(ok, "? success path must exit 0: {out}");
    assert_eq!(out, "7\n");
}

#[test]
fn main_result_06_non_string_error_exits_nonzero() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn main() -> Result<void, i64> {
    return Result::Err(42);
}
"#,
    );
    assert!(!ok, "non-String Err main must still exit non-zero: {out}");
}
