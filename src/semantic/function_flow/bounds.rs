use crate::semantic::{
    call_graph::{CallGraph, CallSites},
    ids::FunctionId,
    indirect_lock::IndirectInputs,
};
use std::collections::{HashMap, HashSet};

/// Shared type buckets keep the fallback graph linear in values + call sites.
/// A wildcard key is shared once, never expanded once per typed call.
pub(crate) fn attach(
    inputs: &IndirectInputs,
    graph: &mut CallGraph,
) -> HashMap<FunctionId, crate::diagnostics::Span> {
    let mut unknown = HashMap::new();
    let mut typed: HashMap<(u64, Option<&str>), HashSet<FunctionId>> = HashMap::new();
    let mut opaque = HashMap::new();
    for value in &inputs.values {
        let key = (value.bit, value.key.as_deref());
        if let Some(target) = value.target {
            typed.entry(key).or_default().insert(target);
        } else {
            opaque.entry(key).or_insert(value.span);
        }
    }
    let mut built = HashSet::new();
    for call in &inputs.calls {
        if call.targets.is_some() {
            continue;
        }
        let bound = call.bound();
        graph.merge(
            call.caller,
            CallSites {
                targets: [bound].into(),
                call_spans: [(bound, call.span)].into(),
                ..Default::default()
            },
        );
        if !built.insert(bound) {
            continue;
        }
        let mut targets = typed
            .get(&(call.bit, call.key.as_deref()))
            .cloned()
            .unwrap_or_default();
        if call.key.is_some() {
            let wildcard = FunctionId::synthetic_free(
                format!("$value-wild:{}", call.bit.trailing_zeros()),
                crate::qualified_name::SyntheticKind::Indirect {
                    slot: call.bit.trailing_zeros() as u8,
                    wildcard: true,
                },
            );
            if built.insert(wildcard) {
                if let Some(&span) = opaque.get(&(call.bit, None)) {
                    unknown.insert(wildcard, span);
                }
                graph.merge(
                    wildcard,
                    CallSites {
                        targets: typed
                            .get(&(call.bit, None))
                            .into_iter()
                            .flatten()
                            .copied()
                            .collect(),
                        has_unknown: opaque.contains_key(&(call.bit, None)),
                        ..Default::default()
                    },
                );
            }
            targets.insert(wildcard);
        } else {
            // At most 64 wildcard arity requests; each value is visited once
            // for its own arity, independently of the number of call sites.
            for ((bit, _), values) in &typed {
                if *bit == call.bit {
                    targets.extend(values);
                }
            }
        }
        let origin = if call.key.is_some() {
            opaque.get(&(call.bit, call.key.as_deref())).copied()
        } else {
            opaque
                .iter()
                .find_map(|((bit, _), &span)| (*bit == call.bit).then_some(span))
        };
        if let Some(span) = origin {
            unknown.insert(bound, span);
        }
        graph.merge(
            bound,
            CallSites {
                targets: targets.into_iter().collect(),
                has_unknown: origin.is_some(),
                ..Default::default()
            },
        );
    }
    unknown
}
