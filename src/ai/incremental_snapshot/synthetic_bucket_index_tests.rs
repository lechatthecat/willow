use super::*;
use crate::qualified_name::SyntheticKind;

#[test]
fn synthetic_bucket_edge_classification_scales_with_edges() {
    for buckets in [1, 16, 256] {
        let mut state = VerificationSnapshotState::default();
        for i in 0..buckets {
            state.record_indirect_kind(
                &format!("renamed{i}"),
                SyntheticKind::Indirect {
                    slot: 1,
                    wildcard: false,
                },
            );
            state.record_indirect_kind(&format!("$indirect-arity:{i}"), SyntheticKind::Source);
        }
        for fanout in [1, 16, 256] {
            INDIRECT_KIND_PROBES.set(0);
            for _ in 0..fanout {
                for i in 0..buckets {
                    assert!(state.is_indirect_bucket(&format!("renamed{i}")));
                    assert!(!state.is_indirect_bucket(&format!("$indirect-arity:{i}")));
                }
            }
            assert_eq!(INDIRECT_KIND_PROBES.get(), 2 * buckets * fanout);
        }
        assert_eq!(state.indirect_kinds.len(), buckets);
        state.remove_records(&["renamed0".to_owned()]);
        assert!(!state.is_indirect_bucket("renamed0"));
        state.record_indirect_kind("renamed1", SyntheticKind::Source);
        assert!(!state.is_indirect_bucket("renamed1"));
    }
}
