//! Owner-only memberwise construction across module and specialization boundaries.
use super::support::*;

const CLASSES: &str = include_str!("../../example/memberwise_owner/secrets.wi");
const MAIN: &str = include_str!("../../example/memberwise_owner/main.wi");

#[test]
fn memberwise_owner_project_example() {
    let (out, ok) = compile_file_and_run("example/memberwise_owner/main.wi");
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\nkept\n");
}

#[test]
fn memberwise_owner_single_file() {
    let source = format!(
        "{}\n{}",
        CLASSES.replace("module secrets;", ""),
        MAIN.replace("import secrets::Plain;", "")
            .replace("import secrets::Gen;", "")
    );
    let (out, ok) = compile_and_run(&source);
    assert!(ok, "{out}");
    assert_eq!(out, "1\n2\nkept\n");
}

#[test]
fn memberwise_owner_module_alias() {
    let main = "import secrets as vault; fn main() { println(vault::Plain::make(3).copy().get()); println(vault::Gen<i64>::make(4).copy().get()); }";
    let (out, ok) = compile_temp_project_with_env_and_run(
        &[("secrets.wi", CLASSES), ("main.wi", main)],
        "main.wi",
        &[],
    );
    assert!(ok, "{out}");
    assert_eq!(out, "3\n4\n");
}

#[test]
fn memberwise_owner_external_module_rejected() {
    for (imports, expr) in [
        ("import secrets::Plain;", "new Plain(1)"),
        ("import secrets::Gen;", "new Gen<i64>(1)"),
        ("import secrets as vault;", "new vault::Plain(1)"),
        ("import secrets as vault;", "new vault::Gen<i64>(1)"),
    ] {
        let main = format!("{imports} fn main() {{ let x = {expr}; }}");
        let project = TestProject::new(
            "memberwise_owner_external",
            &[("secrets.wi", CLASSES), ("main.wi", &main)],
        );
        let output = project.check("main.wi");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{expr}");
        assert!(stderr.contains("E0501"), "{stderr}");
        assert!(
            stderr.contains("memberwise constructor initializes a private field"),
            "{stderr}"
        );
    }
}

#[test]
fn memberwise_owner_same_short_name_is_not_owner() {
    let main = "import secrets; class Plain { pub static fn bad() { let x = new secrets::Plain(1); } } fn main() {}";
    let project = TestProject::new(
        "memberwise_owner_names",
        &[("secrets.wi", CLASSES), ("main.wi", main)],
    );
    let output = project.check("main.wi");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("E0501"), "{stderr}");
}

#[test]
fn memberwise_owner_manifest_project() {
    let project = TestProject::new(
        "memberwise_owner_package",
        &[
            (
                "project.toml",
                "[project]\nname='memberwise-owner'\nversion='0.1.0'\n[willow]\nmanifest-version=1\n",
            ),
            ("src/secrets.wi", CLASSES),
            ("src/main.wi", MAIN),
        ],
    );
    for command in ["check", "build"] {
        let output = project.package_command(command);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let output = project.run();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "1\n2\nkept\n");
}

#[test]
fn class_private_scope_example_runs() {
    let (out, ok) = compile_file_and_run("example/class_private_scope.wi");
    assert!(ok, "class-private public API example failed: {out}");
    assert_eq!(out, "true\n");
}
