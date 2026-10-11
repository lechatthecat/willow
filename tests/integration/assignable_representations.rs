use super::support::*;

#[test]
fn assignable_representations_example_differential_gc_and_placement() {
    let source = include_str!("../../example/assignable_representations.wi");
    let expected = "value:8\nvalue:7\n9\ntrue\n";
    // Arguments/results, stack/frame locals, fields, array elements and enum
    // payloads cross runtime FFI allocation/collection/print/await boundaries.
    for mode in ["", "alloc", "minor", "relocate", "all"] {
        let (out, ok) = compile_and_run_with_env(source, &[("WILLOW_GC_STRESS", mode)]);
        assert!(ok, "{mode}: {out}");
        assert_eq!(out, expected, "{mode}");
    }
    let (out, ok) = compile_and_run_release(source);
    assert!(ok, "release: {out}");
    assert_eq!(out, expected);
}
