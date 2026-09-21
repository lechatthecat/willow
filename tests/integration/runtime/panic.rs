use super::*;

// ── panic() builtin (regression: codegen no longer crashes; willow-4j6) ──────

#[test]
fn panic_01_compiles_runs_and_exits_nonzero() {
    let (out, ok) = compile_and_run_check_exit(r#"fn main() { panic("boom"); }"#);
    assert!(!ok, "panic must exit non-zero");
    assert!(
        out.contains("boom"),
        "panic should print its message: {out}"
    );
}

#[test]
fn panic_02_in_nested_function() {
    let (out, ok) = compile_and_run_check_exit(
        r#"
fn deeper() { panic("deep failure"); }
fn helper() { deeper(); }
fn main() { helper(); }
"#,
    );
    assert!(!ok, "panic in a nested call must exit non-zero");
    assert!(
        out.contains("deep failure"),
        "panic message must appear: {out}"
    );
}

#[test]
fn panic_03_debug_build_reports_source_location() {
    // Debug builds include the panic call-site location (willow-4j6).
    let (out, ok) = compile_and_run_check_exit("fn main() {\n    panic(\"located\");\n}\n");
    assert!(!ok, "panic must exit non-zero");
    assert!(out.contains("located"), "message present: {out}");
    assert!(
        out.contains(".wi:2:"),
        "debug panic should report source line: {out}"
    );
}
