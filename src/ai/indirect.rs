//! Conservative closed-workspace function-value effects. Reuse the checker's
//! address-taken model: a call reaches the values of its arity whose type key
//! matches (see [`crate::semantic::indirect_lock::IndirectCall::key`]). Shared
//! buckets avoid caller × candidate edges.
use super::*;
use crate::semantic::indirect_lock::IndirectCall;
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct Inputs {
    pub values: Vec<(Option<FunctionId>, u64, Option<String>)>,
    pub calls: Vec<IndirectCall>,
}

type Key = (UnitId, FunctionId);
type Calls = HashMap<Key, BTreeSet<String>>;

/// Candidate values of one key, or of no key (`None`).
#[derive(Default)]
struct Candidates {
    targets: HashSet<String>,
    opaque: bool,
}

/// Buckets per arity: a typed call reaches its key's values plus the values
/// without a key (shared through one wildcard bucket); an untyped call
/// reaches every value of the arity. Nodes and edges are linear in values +
/// calls + distinct keys.
pub(super) fn infer(
    captures: &HashMap<UnitId, CapturedUnit>,
    nodes: &HashMap<Key, Function>,
    resolve: &impl Fn(UnitId, FunctionId) -> Key,
) -> (Vec<Function>, Calls) {
    let mut typed: [HashMap<&str, Candidates>; 64] = std::array::from_fn(|_| HashMap::new());
    let mut wild: [Candidates; 64] = std::array::from_fn(|_| Candidates::default());
    let mut calls: Calls = HashMap::new();
    let mut untyped_calls = [false; 64];
    let mut typed_calls: [BTreeSet<&str>; 64] = std::array::from_fn(|_| BTreeSet::new());
    for (&unit, capture) in captures {
        for (target, bit, key) in &capture.indirect.values {
            let slot = bit.trailing_zeros() as usize;
            let candidates = match key {
                Some(key) => typed[slot].entry(key).or_default(),
                None => &mut wild[slot],
            };
            if let Some(node) = target.and_then(|id| nodes.get(&resolve(unit, id))) {
                candidates.targets.insert(node.id.clone());
            } else {
                candidates.opaque = true;
            }
        }
        for call in &capture.indirect.calls {
            let slot = call.bit.trailing_zeros() as usize;
            let bucket = match &call.key {
                Some(key) => {
                    typed_calls[slot].insert(key);
                    typed_bucket_id(slot, key)
                }
                None => {
                    untyped_calls[slot] = true;
                    bucket_id(slot)
                }
            };
            calls
                .entry(resolve(unit, call.caller))
                .or_default()
                .insert(bucket);
        }
    }
    let mut functions = Vec::new();
    for slot in 0..64 {
        let has_wild = !wild[slot].targets.is_empty() || wild[slot].opaque;
        if untyped_calls[slot] {
            let all = typed[slot].values().chain([&wild[slot]]);
            let opaque = all.clone().any(|c| c.opaque);
            let callees = all.flat_map(|c| c.targets.iter().cloned()).collect();
            functions.push(bucket(bucket_id(slot), slot, None, callees, opaque));
        }
        if typed_calls[slot].is_empty() {
            continue;
        }
        if has_wild {
            let callees = wild[slot].targets.iter().cloned().collect();
            functions.push(bucket(
                wild_bucket_id(slot),
                slot,
                Some("*"),
                callees,
                wild[slot].opaque,
            ));
        }
        for &key in &typed_calls[slot] {
            let own = typed[slot].remove(key).unwrap_or_default();
            let mut callees: Vec<_> = own.targets.into_iter().collect();
            if has_wild {
                callees.push(wild_bucket_id(slot));
            }
            functions.push(bucket(
                typed_bucket_id(slot, key),
                slot,
                Some(key),
                callees,
                own.opaque,
            ));
        }
    }
    (functions, calls)
}

fn bucket(
    id: String,
    slot: usize,
    key: Option<&str>,
    mut callees: Vec<String>,
    opaque: bool,
) -> Function {
    let unknown = opaque || callees.is_empty();
    callees.sort_unstable();
    Function {
        identity: None,
        body_id: None,
        body_location: None,
        rename_calls: vec![],
        rename_imports: vec![],
        id,
        module: String::new(),
        name: format!(
            "<function-value candidates: arity {slot}{}{}>",
            if slot == 63 { "+" } else { "" },
            key.map_or_else(String::new, |key| format!(", type {key}")),
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
}

fn bucket_id(slot: usize) -> String {
    format!("$indirect-arity:{slot}")
}

fn wild_bucket_id(slot: usize) -> String {
    format!("$indirect-arity:{slot}:*")
}

fn typed_bucket_id(slot: usize, key: &str) -> String {
    format!("$indirect-arity:{slot}:{}", hash(key))
}
