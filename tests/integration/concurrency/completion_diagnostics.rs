use super::*;

// ── Migration diagnostics for the removed Task completion methods ───────────
// `join()` and `try_join()` no longer exist on Task. Both report migration text
// instead of a generic "unknown method" / "bad select case". Unrelated user
// classes remain free to use either method name.

#[test]
fn tmig_01_join_on_a_task_reports_e0812() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn w() -> i64 { return 1; }\nfn main() { let v = w().join(); }",
        &[],
    );
    assert!(!ok);
    for expected in ["error[E0812]", "has been removed", "await task"] {
        assert!(stderr.contains(expected), "missing {expected}: {stderr}");
    }
}

#[test]
fn tmig_02_join_in_a_select_case_reports_e0812() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn w() -> i64 { return 1; }\nasync fn main() { let h = w(); select { let v = h.join() => { println(v); } } }",
        &[],
    );
    assert!(!ok);
    for expected in ["error[E0812]", "let v = await t"] {
        assert!(stderr.contains(expected), "missing {expected}: {stderr}");
    }
}

#[test]
fn tmig_03_try_join_on_a_task_reports_e0813() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn w() -> i64 { return 1; }\nfn main() { let value = w().try_join(); }",
        &[],
    );
    assert!(!ok);
    for expected in ["error[E0813]", "has been removed", "await task"] {
        assert!(stderr.contains(expected), "missing {expected}: {stderr}");
    }
    assert!(stderr.contains("await task.result()"), "{stderr}");
}

#[test]
fn tmig_04_try_join_on_a_join_handle_reports_e0813() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn inspect(h: JoinHandle<i64>) { let value = h.try_join(); }\nfn main() {}",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("error[E0813]"), "{stderr}");
    assert!(stderr.contains("has been removed"), "{stderr}");
}

#[test]
fn tmig_05_try_join_in_a_select_case_reports_e0813() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn w() -> i64 { return 1; }\nasync fn main() { let h = w(); select { let v = h.try_join() => { println(1); } } }",
        &[],
    );
    assert!(!ok);
    for expected in ["error[E0813]", "has been removed", "await t.result()"] {
        assert!(stderr.contains(expected), "missing {expected}: {stderr}");
    }
}

#[test]
fn tmig_06_bare_try_join_select_case_reports_e0813() {
    let (ok, stderr) = compile_with_compiler_env(
        "async fn w() -> i64 { return 1; }\nasync fn main() { let h = w(); select { h.try_join() => { println(1); } } }",
        &[],
    );
    assert!(!ok);
    assert!(stderr.contains("error[E0813]"), "{stderr}");
}

#[test]
fn tmig_07_user_class_may_define_try_join() {
    let (out, ok) = compile_and_run(
        "class Probe { pub fn try_join(self) -> i64 { return 7; } }\nfn main() { println(new Probe().try_join()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "7\n");
}
