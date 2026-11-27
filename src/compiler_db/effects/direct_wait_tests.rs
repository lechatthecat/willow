use super::*;

#[test]
fn direct_wait_evidence_is_bounded_and_merges_both_bits() {
    for count in [32, 128, 512, 2048] {
        for reversed in [false, true] {
            let mut effects = DirectLockEffects::default();
            for index in 0..count {
                let kind = if (index % 2 == 0) != reversed {
                    LockEffectKind::Block
                } else {
                    LockEffectKind::Suspend
                };
                let mut fragment = DirectLockEffects::default();
                fragment.insert(LockEffectCause {
                    is_conservative: false,
                    span: Span::new(index, index + 1, 1, index + 1),
                    operation: "wait".into(),
                    kind,
                    lock: None,
                });
                effects.merge(fragment);
            }
            assert_eq!(effects.causes.len(), 2);
            assert_eq!(effects.causes[0].span.start, 0);
            assert_eq!(effects.causes[1].span.start, 1);
            let encoded = serde_json::to_vec(&effects).unwrap();
            let restored: DirectLockEffects = serde_json::from_slice(&encoded).unwrap();
            let bits = restored
                .causes
                .iter()
                .fold(RuntimeEffects::NONE, |bits, cause| {
                    bits.union(cause.kind.effects())
                });
            assert_eq!(bits, LOCK_EFFECT_WAIT);
            println!(
                "direct-wait-evidence operations={count} reversed={reversed} retained={}",
                restored.causes.len()
            );
        }
    }
}
