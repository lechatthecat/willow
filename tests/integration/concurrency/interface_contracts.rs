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
