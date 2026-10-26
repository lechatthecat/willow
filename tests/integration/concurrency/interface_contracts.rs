use super::*;

#[test]
fn test_interface_marker_contract_rejected_before_interface_erasure() {
    for fields in ["pub offending: Plain;", "pub offending: Array<Plain>;"] {
        let source = format!(
            "import std::collections::Array; interface Plain {{}} interface Holder extends Send {{}} class Bag implements Holder {{ {fields} }} fn main() {{}}"
        );
        let (ok, stderr) = compile_with_compiler_env(&source, &[("WILLOW_WORKERS", "1")]);
        assert!(!ok, "{stderr}");
        assert!(stderr.contains("error[E2406]"), "{stderr}");
        assert!(stderr.contains("offending"), "{stderr}");
    }
}

#[test]
fn test_interface_marker_contract_reports_cross_module_writes_with_structural_failure() {
    let stderr = compile_temp_project_error_stderr(
        &[
            (
                "players.wi",
                "import std::collections::Array; pub interface Player extends Sync {} pub class Blend implements Player { pub data: Array<i64>; } pub class RandomPlayer implements Player { pub seed: i64; }",
            ),
            (
                "main.wi",
                "import players::RandomPlayer; fn id<T>(x: T) -> T { return x; } fn main() { let p = id(new RandomPlayer(0)); p.seed = 1; }",
            ),
        ],
        "main.wi",
    );
    assert_eq!(stderr.matches("error[E2406]").count(), 2, "{stderr}");
    assert!(stderr.contains("class `Blend`"), "{stderr}");
    assert!(stderr.contains("class `RandomPlayer`"), "{stderr}");
}

// 2 compiler paths x 2 writer locations x 5 write forms = 20 perspectives.
#[test]
fn test_interface_marker_contract_diagnostic_batch_matrix() {
    let players = "import std::collections::Array; pub interface Player extends Sync {} pub class Blend implements Player { pub data: Array<i64>; } pub class RandomPlayer implements Player { pub seed: i64; }";
    for generic in [false, true] {
        for separate_writer in [false, true] {
            for write in [
                "p.seed = 1;",
                "p.seed += 1;",
                "let alias = p; alias.seed = 1;",
                "let f = || { p.seed = 1; }; f();",
                "if true { p.seed = 1; }",
            ] {
                let template = if generic {
                    "fn id<T>(x: T) -> T { return x; }"
                } else {
                    ""
                };
                let writer = format!(
                    "import players::RandomPlayer; pub fn mutate(p: RandomPlayer) {{ {write} }}"
                );
                let main = if separate_writer {
                    format!("import writer; {template} fn main() {{}}")
                } else {
                    format!("{writer} {template} fn main() {{}}")
                };
                let project = TestProject::new(
                    "marker_batch",
                    &[
                        ("players.wi", players),
                        ("writer.wi", &writer),
                        ("main.wi", &main),
                    ],
                );
                let output = project.check("main.wi");
                let stderr = String::from_utf8_lossy(&output.stderr);
                assert!(
                    !output.status.success(),
                    "{generic}/{separate_writer}/{write}"
                );
                assert_eq!(
                    stderr.matches("error[E2406]").count(),
                    2,
                    "{generic}/{separate_writer}/{write}: {stderr}"
                );
                assert!(stderr.contains("class `Blend`"), "{stderr}");
                assert!(stderr.contains("class `RandomPlayer`"), "{stderr}");
                assert!(
                    stderr.contains("field `seed: i64` is reassigned"),
                    "{stderr}"
                );
                assert!(!stderr.contains("error[E2402]"), "{stderr}");
            }
        }
    }
}

#[test]
fn test_interface_marker_contract_entry_uses_shared_write_summary() {
    let project = TestProject::new(
        "marker_entry",
        &[
            ("writer.wi", "pub fn mutate<T>(p: T) { p.seed = 1; }"),
            (
                "main.wi",
                "import std::collections::Array; import writer; interface Player extends Sync {} class Blend implements Player { pub data: Array<i64>; } class RandomPlayer implements Player { pub seed: i64; } fn main() { writer::mutate(new RandomPlayer(0)); }",
            ),
        ],
    );
    let output = project.check("main.wi");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert_eq!(stderr.matches("error[E2406]").count(), 2, "{stderr}");
    assert!(stderr.contains("class `RandomPlayer`"), "{stderr}");
}

#[test]
fn test_interface_marker_contract_safe_example_runs() {
    let (out, ok) = compile_and_run(include_str!("../../../example/interface_sync_contracts.wi"));
    assert!(ok, "{out}");
    assert_eq!(out, "3\n7\n");
}
