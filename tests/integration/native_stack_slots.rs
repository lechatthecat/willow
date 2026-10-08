//! willow-jz15.50: shared native slots must preserve GC and control-flow behavior.
use super::support::{compile_and_run_release, compile_and_run_with_env};

#[test]
fn native_stack_slots_example_and_relocation() {
    let source = include_str!("../../example/native_stack_slots.wi");
    for mode in ["", "minor", "alloc", "relocate"] {
        let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", mode)]);
        assert!(ok, "{mode}: {out}");
        assert_eq!(out, "6\n15\n820\n", "{mode}");
    }
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "{out}");
    assert_eq!(out, "6\n15\n820\n");
}

#[test]
fn native_stack_slots_sequential_calls_keep_nested_operands() {
    let source = r#"
fn join(a: String, b: String) -> String { gc_minor_collect(); return a + b; }
fn main() {
    println(join(join("a", "b"), join("c", "d")));
    println(join("e", join("f", "g")));
    println(join(join("h", "i"), "j"));
    println(join("k", "l"));
}"#;
    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", "relocate")]);
    assert!(ok, "{out}");
    assert_eq!(out, "abcd\nefg\nhij\nkl\n");
}

#[test]
fn native_stack_slots_defer_capture_survives_reused_temporaries() {
    let source = r#"
fn main() {
    let outer = "outer" + "!";
    defer println(outer);
    if true {
        let inner = "inner" + "!";
        defer println(inner);
        println("first" + "!");
        gc_minor_collect();
        println("second" + "!");
    }
    println("last" + "!");
}"#;
    let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", "relocate")]);
    assert!(ok, "{out}");
    assert_eq!(out, "first!\nsecond!\ninner!\nlast!\nouter!\n");
}
