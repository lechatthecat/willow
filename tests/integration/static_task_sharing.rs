use super::support::*;

#[test]
fn static_task_sharing_synchronized_example_runs() {
    let source = include_str!("../../example/static_task_sharing.wi");
    let project = TestProject::new("static_task_sharing", &[("main.wi", source)]);
    let build = project.compile_with_env("main.wi", &[("WILLOW_WORKERS", "1")]);
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    for workers in ["1", "4"] {
        let run = project.run_with_env(&[("WILLOW_WORKERS", workers)]);
        assert!(
            run.status.success(),
            "{}",
            String::from_utf8_lossy(&run.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&run.stdout), "2\n2\n2\n");
    }
}

#[test]
fn static_task_sharing_rejection_does_not_depend_on_compiler_workers() {
    let source = "import std::collections::Array; class G { pub static data: Array<i64> = [1]; } fn helper() { G::data.push(2); } async fn main() { helper(); }";
    for workers in ["1", "4", "invalid"] {
        let (ok, diagnostics) = compile_with_compiler_env(source, &[("WILLOW_WORKERS", workers)]);
        assert!(!ok, "{diagnostics}");
        assert!(diagnostics.contains("cannot share static"), "{diagnostics}");
        assert!(
            diagnostics.contains("non-Sync static declared here"),
            "{diagnostics}"
        );
    }
}
