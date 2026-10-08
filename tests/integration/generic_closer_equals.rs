use super::support::*;

#[test]
fn generic_closer_equals_example_runs() {
    let source = include_str!("../../example/generic_closer_equals.wi");
    let (out, ok) = compile_and_run(source);
    assert!(ok, "{out}");
    assert_eq!(out, "41\n42\n7\n8\n3\ntrue\n8\n");
}

#[test]
fn generic_closer_equals_matches_spaced_program() {
    let source = include_str!("../../example/generic_closer_equals.wi");
    // Expand only the type/initializer boundaries, leaving comparisons and
    // shift assignments identical in the two compiled programs.
    let spaced = source
        .replace(">= Some", "> = Some")
        .replace(">= [", "> = [");
    let (out, ok) = compile_and_run(&spaced);
    assert!(ok, "{out}");
    assert_eq!(out, "41\n42\n7\n8\n3\ntrue\n8\n");
}
