//! Conservative closed-workspace function-value effects. Reuse the checker's
//! address-taken/arity model; a shared bucket avoids caller × candidate edges.
use super::*;
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct Inputs {
    pub values: Vec<(Option<FunctionId>, u64)>,
    pub calls: Vec<(FunctionId, u64)>,
}

type Key = (UnitId, FunctionId);
type Calls = HashMap<Key, BTreeSet<String>>;

pub(super) fn infer(
    captures: &HashMap<UnitId, CapturedUnit>,
    nodes: &HashMap<Key, Function>,
    resolve: &impl Fn(UnitId, FunctionId) -> Key,
) -> (Vec<Function>, Calls) {
    let mut targets: [HashSet<String>; 64] = std::array::from_fn(|_| HashSet::new());
    let mut opaque = [false; 64];
    let mut calls: Calls = HashMap::new();
    let mut used = [false; 64];
    for (&unit, capture) in captures {
        for &(target, bit) in &capture.indirect.values {
            let slot = bit.trailing_zeros() as usize;
            if let Some(node) = target.and_then(|id| nodes.get(&resolve(unit, id))) {
                targets[slot].insert(node.id.clone());
            } else {
                opaque[slot] = true;
            }
        }
        for &(caller, bit) in &capture.indirect.calls {
            let slot = bit.trailing_zeros() as usize;
            used[slot] = true;
            calls
                .entry(resolve(unit, caller))
                .or_default()
                .insert(bucket_id(slot));
        }
    }
    let functions = (0..64)
        .filter(|&slot| used[slot])
        .map(|slot| {
            let unknown = opaque[slot] || targets[slot].is_empty();
            let mut callees: Vec<_> = std::mem::take(&mut targets[slot]).into_iter().collect();
            callees.sort_unstable();
            Function {
                identity: None,
                body_id: None,
                body_location: None,
                rename_calls: vec![],
                rename_imports: vec![],
                id: bucket_id(slot),
                module: String::new(),
                name: format!(
                    "<function-value candidates: arity {slot}{}>",
                    if slot == 63 { "+" } else { "" }
                ),
                locations: vec![],
                synthetic: true,
                fingerprint: String::new(),
                body_fingerprint: String::new(),
                callees,
                runtime_effects: if unknown {
                    RuntimeEffects::ALL.bits()
                } else {
                    0
                },
                unknown,
                unresolved: if unknown {
                    vec!["incomplete-function-value-candidates".into()]
                } else {
                    vec![]
                },
            }
        })
        .collect();
    (functions, calls)
}

fn bucket_id(slot: usize) -> String {
    format!("$indirect-arity:{slot}")
}
