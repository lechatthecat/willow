use super::support::*;

#[test]
fn contextual_new_example_runs() {
    let (stdout, success) = compile_and_run(include_str!("../../example/contextual_new.wi"));
    assert!(success);
    assert_eq!(stdout, "3\n3\n3\n3\n");
}

#[test]
fn contextual_new_imported_class_runs() {
    let project = TestProject::new(
        "contextual_new",
        &[
            (
                "boxes.wi",
                "module boxes; pub class Box<T> { pub value: T; }",
            ),
            (
                "main.wi",
                "import boxes; fn make() -> boxes::Box<i64> { return new boxes::Box(7); } fn main() { let b: boxes::Box<i64> = new boxes::Box(5); println(b.value); println(make().value); }",
            ),
        ],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = project.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "5\n7\n");
}

#[test]
fn contextual_new_missing_context_has_help() {
    let stderr =
        compile_error_stderr("class Box<T> { pub value: T; } fn main() { let b = new Box(1); }");
    assert!(stderr.contains("error[E0201]"), "{stderr}");
    assert!(stderr.contains("new Box<T>()"), "{stderr}");
    assert!(!stderr.contains("internal compiler error"), "{stderr}");
}

#[test]
fn contextual_new_imported_nested_class_runs() {
    let project = TestProject::new(
        "contextual_new_nested",
        &[
            (
                "boxes.wi",
                "module boxes; pub class Box<T> { pub value: T; } pub class Holder<T> { pub value: T; } pub class IntHolder { pub value: Box<i64>; }",
            ),
            (
                "main.wi",
                "import boxes; fn main() { let b: boxes::Holder<boxes::Box<i64>> = new boxes::Holder(new boxes::Box(5)); let c = new boxes::IntHolder(new boxes::Box(7)); println(b.value.value); println(c.value.value); }",
            ),
        ],
    );
    let compiled = project.compile("main.wi");
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let output = project.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "5\n7\n");
}

#[test]
fn contextual_new_preserves_constructor_effects() {
    for construction in [
        "let runner: Runner<i64> = new Runner(1, ch);",
        "let runner: Holder<Runner<i64>> = new Holder(new Runner(1, ch));",
    ] {
        let source = r#"
        class Holder<T> { pub value: T; }
        class Runner<T> {
            pub value: T;
            pub init(self, value: T, ch: Channel<i64>) {
                self.value = value;
                ch.recv();
            }
        }
        async fn main() {
            let ch = Channel<i64>::new();
            let m = Mutex::new(0);
            lock m as value { CONSTRUCTION }
        }
    "#
        .replace("CONSTRUCTION", construction);
        let stderr = compile_error_stderr(&source);
        assert!(stderr.contains("error[E2604]"), "{stderr}");
        assert!(!stderr.contains("error[E0201]"), "{stderr}");
    }
}

#[test]
fn contextual_new_preserves_private_constructor() {
    let stderr = compile_error_stderr(
        r#"
        class Hidden<T> { init(self) {} }
        fn main() { let value: Hidden<i64> = new Hidden(); }
    "#,
    );
    assert!(stderr.contains("error[E0846]"), "{stderr}");
    assert!(stderr.contains("private constructor"), "{stderr}");
}
