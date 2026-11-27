use super::*;

#[test]
fn local_method_identity_index_scales_with_declarations_and_edges() {
    for count in [16, 64, 256, 1024] {
        let mut index = CallableIndex::default();
        for i in 0..count {
            let local = FunctionId::method(TypeId::local(format!("C{i}")), "run");
            let canonical =
                FunctionId::method(TypeId::from_source_name(&format!("provider::C{i}")), "run");
            index.local_methods.insert(canonical, local);
        }
        assert_eq!(index.local_methods.len(), count);
        let mut lookups = 0;
        // Reverse declaration order, with repeated lookups of the same bodies. No
        // hierarchy walk or allocation occurs inside local_target.
        for _ in 0..8 {
            for i in (0..count).rev() {
                let local = FunctionId::method(TypeId::local(format!("C{i}")), "run");
                let canonical =
                    FunctionId::method(TypeId::from_source_name(&format!("provider::C{i}")), "run");
                assert_eq!(index.local_target(canonical), local);
                let foreign =
                    FunctionId::method(TypeId::from_source_name(&format!("other::C{i}")), "run");
                assert_eq!(index.local_target(foreign), foreign);
                lookups += 2;
            }
        }
        assert_eq!(lookups, 16 * count);
        println!(
            "declarations={count} index_entries={} lookup_invocations={lookups}",
            index.local_methods.len()
        );
    }
}
