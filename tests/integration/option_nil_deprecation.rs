use super::support::*;

// Final source-level `nil` removal — willow-glaj.10.

fn removal_error(source: &str) -> String {
    let (ok, stderr) = compile_with_compiler_env(source, &[]);
    assert!(!ok, "source `nil` must be rejected: {stderr}");
    assert!(stderr.contains("error[E0201]"), "{stderr}");
    assert!(stderr.contains("`nil` has been removed"), "{stderr}");
    assert!(
        stderr.contains("help: replace `nil` with `None`"),
        "{stderr}"
    );
    stderr
}

fn removal_count(stderr: &str) -> usize {
    stderr.matches("`nil` has been removed").count()
}

#[test]
fn nil_removed_01_annotated_let() {
    let stderr = removal_error("fn main() { let value: Option<i64> = nil; }");
    assert_eq!(removal_count(&stderr), 1, "{stderr}");
}

#[test]
fn nil_removed_02_argument_context() {
    removal_error("fn take(v: Option<i64>) {} fn main() { take(nil); }");
}

#[test]
fn nil_removed_03_return_context() {
    removal_error("fn missing() -> Option<i64> { return nil; } fn main() {}");
}

#[test]
fn nil_removed_04_reassignment() {
    removal_error("fn main() { let mut value: Option<i64> = Some(1); value = nil; }");
}

#[test]
fn nil_removed_05_constructor_field_context() {
    removal_error(
        "class Holder { pub value: Option<String>; } fn main() { let h = new Holder(nil); }",
    );
}

#[test]
fn nil_removed_06_field_assignment() {
    removal_error(
        "class Holder { pub value: Option<String>; } fn main() { let h = new Holder(Some(\"x\")); h.value = nil; }",
    );
}

#[test]
fn nil_removed_07_array_element_context() {
    removal_error(
        "import std::collections::Array; fn main() { let xs: Array<Option<i64>> = [nil]; }",
    );
}

#[test]
fn nil_removed_08_map_value_context() {
    removal_error(
        "import std::collections::Map; fn main() { let mut m: Map<i64, Option<String>> = Map::new(); m.insert(1, nil); }",
    );
}

#[test]
fn nil_removed_09_channel_value_context() {
    removal_error("fn main() { let ch: Channel<Option<i64>> = Channel::new(); ch.send(nil); }");
}

#[test]
fn nil_removed_10_ternary_branch_context() {
    removal_error("fn main() { let value: Option<i64> = true ? Some(1) : nil; }");
}

#[test]
fn nil_removed_11_nested_scope() {
    removal_error("fn main() { if true { let value: Option<i64> = nil; } }");
}

#[test]
fn nil_removed_12_async_return() {
    removal_error(
        "async fn missing() -> Option<i64> { await sleep(1); return nil; } async fn main() {}",
    );
}

#[test]
fn nil_removed_13_each_source_occurrence_errors_once() {
    let stderr =
        removal_error("fn main() { let a: Option<i64> = nil; let b: Option<String> = nil; }");
    assert_eq!(removal_count(&stderr), 2, "{stderr}");
}

#[test]
fn nil_removed_14_module_body_uses_module_source_map() {
    let project = TestProject::new(
        "nil_removal_module",
        &[
            (
                "maybe.wi",
                "module maybe; pub fn missing() -> Option<i64> { return nil; }",
            ),
            ("main.wi", "import maybe; fn main() {}"),
        ],
    );
    let output = project.compile("main.wi");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("`nil` has been removed"), "{stderr}");
    assert!(stderr.contains("maybe.wi"), "{stderr}");
}

#[test]
fn nil_removed_15_bare_context() {
    let stderr = removal_error("fn main() { let value = nil; }");
    assert_eq!(removal_count(&stderr), 1, "{stderr}");
}

#[test]
fn nil_removed_16_non_option_context() {
    let stderr = removal_error("fn main() { let value: i64 = nil; }");
    assert_eq!(removal_count(&stderr), 1, "{stderr}");
}

#[test]
fn nil_removed_17_message_and_help_are_stable() {
    let stderr = removal_error("fn main() { let value: Option<i64> = nil; }");
    assert!(stderr.contains("removed absence literal"), "{stderr}");
}

#[test]
fn nil_removed_18_primary_label_points_at_token() {
    let stderr = removal_error("fn main() {\n    let value: Option<i64> = nil;\n}");
    assert!(
        stderr.contains("2 |     let value: Option<i64> = nil;"),
        "{stderr}"
    );
    assert!(stderr.contains("^^^ removed absence literal"), "{stderr}");
}

#[test]
fn nil_removed_19_qualified_none_still_compiles() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn main() { let value: Option<i64> = Option::None; println(value.is_none()); }",
        &[],
    );
    assert!(ok, "{stderr}");
    assert!(!stderr.contains("`nil` has been removed"), "{stderr}");
}

#[test]
fn nil_removed_20_bare_none_still_compiles() {
    let (ok, stderr) = compile_with_compiler_env(
        "fn main() { let value: Option<i64> = None; println(value.is_none()); }",
        &[],
    );
    assert!(ok, "{stderr}");
    assert!(!stderr.contains("`nil` has been removed"), "{stderr}");
}

#[test]
fn nil_removed_21_none_runtime_semantics_for_gc_reference() {
    let (out, ok) = compile_and_run(
        "fn main() { let value: Option<String> = None; println(value.is_none()); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "true\n");
}

#[test]
fn nil_removed_22_none_runtime_semantics_for_boxed_scalar() {
    let (out, ok) = compile_and_run(
        "fn main() { let value: Option<i64> = None; println(value.unwrap_or(42)); }",
    );
    assert!(ok, "{out}");
    assert_eq!(out, "42\n");
}
