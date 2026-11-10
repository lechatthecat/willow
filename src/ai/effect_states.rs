//! Witness-backed effects, independent of conservative safety capability bits.
use super::*;
use serde_json::{Value, json};
use std::collections::VecDeque;

pub const SCHEMA: &str = "willow.effects.v2";
const COUNT: usize = RuntimeEffects::BIT_COUNT as usize;
const MISSING_WITNESS: usize = 5;
const REASONS: [&str; 7] = [
    "indirect-call",
    "unresolved-callee",
    "incomplete-flow",
    "missing-body",
    "external-interop",
    "missing-witness",
    "missing-source",
];

#[derive(Clone, Default)]
struct Fact {
    witness: Option<usize>,
    reasons: [Option<usize>; 7],
}
impl Fact {
    fn get(&self, slot: usize) -> Option<usize> {
        if slot == 0 {
            self.witness
        } else {
            self.reasons[slot - 1]
        }
    }
    fn get_mut(&mut self, slot: usize) -> &mut Option<usize> {
        if slot == 0 {
            &mut self.witness
        } else {
            &mut self.reasons[slot - 1]
        }
    }
}
/// Fixed-size summaries reference shared evidence instead of copying transitive
/// cause sets. At most COUNT * (1 + REASONS.len()) facts arrive at each node.
pub(super) struct Analysis {
    facts: Vec<[Fact; COUNT]>,
    evidence: Vec<Value>,
    pub edge_visits: usize,
}
impl Analysis {
    pub fn new(snapshot: &Snapshot) -> Self {
        let index = graph::Index::new(&snapshot.functions);
        let mut result = Self {
            facts: (0..snapshot.functions.len())
                .map(|_| std::array::from_fn(|_| Fact::default()))
                .collect(),
            evidence: Vec::new(),
            edge_visits: 0,
        };
        let mut declared_no_io = vec![false; snapshot.functions.len()];
        for bridge in &snapshot.semantic.interop.bridges {
            if let Some(&i) = index.by_id.get(bridge.function.as_str()) {
                declared_no_io[i] = bridge.symbol.no_io;
                let unknown = if bridge.symbol.no_io {
                    RuntimeEffects::ALL.difference(RuntimeEffects::MAY_IO)
                } else {
                    RuntimeEffects::ALL
                };
                result.reason(i, unknown.bits(), 4, None);
            }
        }
        let mut bodies = vec![false; snapshot.functions.len()];
        for flow in &snapshot.semantic.flows {
            if let Some(&i) = index.by_id.get(flow.function.as_str()) {
                bodies[i] = true;
                if !flow.complete {
                    result.reason(i, RuntimeEffects::ALL.bits(), 2, None);
                }
            }
        }
        for (i, f) in snapshot.functions.iter().enumerate() {
            if !bodies[i] && !f.synthetic {
                let unknown = if declared_no_io[i] {
                    RuntimeEffects::ALL.difference(RuntimeEffects::MAY_IO)
                } else {
                    RuntimeEffects::ALL
                };
                result.reason(i, unknown.bits(), 3, f.locations.first());
            }
            if f.id.starts_with("$indirect-arity:") {
                result.reason(i, f.runtime_effects, 0, None);
            }
            if let Some(sites) = snapshot.semantic.unresolved_calls.get(&f.id) {
                for site in sites {
                    result.reason(
                        i,
                        RuntimeEffects::ALL.bits(),
                        if site.operation == "indirect-call" {
                            0
                        } else {
                            1
                        },
                        Some(&site.location),
                    );
                }
            }
            if !f.unresolved.is_empty() || (f.synthetic && f.unknown && index.callees[i].is_empty())
            {
                result.reason(i, RuntimeEffects::ALL.bits(), 1, f.locations.first());
            }
            if f.callees
                .iter()
                .any(|id| !index.by_id.contains_key(id.as_str()))
            {
                result.reason(i, RuntimeEffects::ALL.bits(), 1, f.locations.first());
            }
            let ranges = super::semantic::local_ranges(&f.locations);
            if let Some(facts) = snapshot.semantic.witnesses.get(&f.id) {
                for fact in facts {
                    let Some(bits) = fact["effect"].as_u64().and_then(|b| u8::try_from(b).ok())
                    else {
                        continue;
                    };
                    let w = &fact["witness"];
                    let location =
                        serde_json::from_value::<Location>(w["cause"]["location"].clone()).ok();
                    let reason = if w["kind"] == "external-boundary" {
                        Some(4)
                    } else if w["cause"]["operation"] == "indirect-call" {
                        Some(0)
                    } else if w["kind"] == "unavailable" {
                        Some(5)
                    } else if location.is_none() {
                        Some(6)
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        result.reason(i, bits, reason, location.as_ref());
                        continue;
                    }
                    // Compiler summaries may cite another body's operation.
                    // Only local facts seed evidence; graph edges propagate it.
                    if w["owner"].as_str().is_some_and(|owner| owner != f.id) {
                        continue;
                    }
                    let location = location.unwrap();
                    let range = ranges.partition_point(|&(path, start, _)| {
                        (path, start) <= (location.path.as_str(), location.start)
                    });
                    if !range.checked_sub(1).is_some_and(|j| {
                        ranges[j].0 == location.path
                            && location.start <= location.end
                            && location.end <= ranges[j].2
                    }) {
                        continue;
                    }
                    if !(0..COUNT)
                        .any(|bit| bits & (1 << bit) != 0 && result.facts[i][bit].witness.is_none())
                    {
                        continue;
                    }
                    let id = result.evidence.len();
                    result.evidence.push(json!({"function":f.id, "location":location, "operation":w["cause"]["operation"]}));
                    for bit in 0..COUNT {
                        if bits & (1 << bit) != 0 && result.facts[i][bit].witness.is_none() {
                            result.facts[i][bit].witness = Some(id);
                        }
                    }
                }
            }
            let child_bits = index.callees[i]
                .iter()
                .fold(0, |bits, &j| bits | snapshot.functions[j].runtime_effects);
            for bit in 0..COUNT {
                let mask = 1 << bit;
                if f.runtime_effects & mask != 0
                    && child_bits & mask == 0
                    && result.facts[i][bit].witness.is_none()
                    && result.facts[i][bit].reasons.iter().all(Option::is_none)
                {
                    result.reason(i, mask, MISSING_WITNESS, f.locations.first());
                }
            }
        }
        let mut queue = VecDeque::new();
        for (i, facts) in result.facts.iter().enumerate() {
            for (bit, fact) in facts.iter().enumerate() {
                for slot in 0..=REASONS.len() {
                    if fact.get(slot).is_some() {
                        queue.push_back((i, bit, slot));
                    }
                }
            }
        }
        result.propagate(snapshot, &index, queue);
        let mut missing = VecDeque::new();
        // A capability-only cycle can have no local source at all. Retain its
        // bound as uncertainty; never infer absence from missing evidence.
        for (i, f) in snapshot.functions.iter().enumerate() {
            for bit in 0..COUNT {
                if f.unknown && result.facts[i][bit].reasons.iter().all(Option::is_none) {
                    result.reason(i, 1 << bit, 1, None);
                    missing.push_back((i, bit, 2));
                }
                if f.runtime_effects & (1 << bit) != 0
                    && result.facts[i][bit].witness.is_none()
                    && result.facts[i][bit].reasons.iter().all(Option::is_none)
                {
                    result.reason(i, 1 << bit, MISSING_WITNESS, None);
                    missing.push_back((i, bit, MISSING_WITNESS + 1));
                }
            }
        }
        // Resume the same monotone propagation for previously unexplained
        // bounds, including a cycle beside an independently witnessed path.
        result.propagate(snapshot, &index, missing);
        result
    }
    fn propagate(
        &mut self,
        snapshot: &Snapshot,
        index: &graph::Index<'_>,
        mut queue: VecDeque<(usize, usize, usize)>,
    ) {
        // Each (node, effect, evidence/reason category) is inserted once.
        // Every corresponding edge is visited once, including resumed seeds.
        while let Some((i, bit, slot)) = queue.pop_front() {
            let source = self.facts[i][bit].get(slot);
            for &caller in &index.callers[i] {
                self.edge_visits += 1;
                if slot == 0
                    && snapshot.functions[caller]
                        .id
                        .starts_with("$indirect-arity:")
                {
                    continue;
                }
                let target = self.facts[caller][bit].get_mut(slot);
                if target.is_none() {
                    *target = source;
                    queue.push_back((caller, bit, slot));
                }
            }
        }
    }
    fn reason(&mut self, function: usize, bits: u8, reason: usize, location: Option<&Location>) {
        if !(0..COUNT).any(|bit| {
            bits & (1 << bit) != 0 && self.facts[function][bit].reasons[reason].is_none()
        }) {
            return;
        }
        let id = self.evidence.len();
        self.evidence
            .push(json!({"code":REASONS[reason], "location":location}));
        for bit in 0..COUNT {
            if bits & (1 << bit) != 0 && self.facts[function][bit].reasons[reason].is_none() {
                self.facts[function][bit].reasons[reason] = Some(id);
            }
        }
    }
    pub fn output(&self, function: usize) -> Value {
        Value::Array(self.facts[function].iter().enumerate().map(|(bit, fact)| {
            let reasons: Vec<_> = fact.reasons.iter().flatten().map(|&i| &self.evidence[i]).collect();
            json!({"effect":1u8 << bit, "state":if fact.witness.is_some() {"may"} else if !reasons.is_empty() {"unknown"} else {"none"},
                "witness":fact.witness.map(|i| &self.evidence[i]), "unknown_reasons":reasons})
        }).collect())
    }
    pub fn unavailable() -> Value {
        Value::Array(
            (0..COUNT)
                .map(|bit| {
                    json!({"effect":1u8 << bit, "state":"unknown", "witness":null,
            "unknown_reasons":[{"code":"missing-body", "location":null}]})
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(n: usize) -> Snapshot {
        let mut semantic = SemanticFacts::default();
        let functions = (0..n)
            .map(|i| {
                let id = format!("f{i}");
                let location = Location {
                    path: "test.wi".into(),
                    start: i * 10,
                    end: i * 10 + 10,
                };
                semantic.flows.push(semantic::Flow {
                    function: id.clone(),
                    nodes: vec![],
                    entry: 0,
                    complete: true,
                });
                Function {
                    identity: None,
                    body_id: None,
                    body_location: None,
                    rename_calls: vec![],
                    rename_imports: vec![],
                    id,
                    module: "test".into(),
                    name: format!("f{i}"),
                    locations: vec![location],
                    synthetic: false,
                    fingerprint: String::new(),
                    body_fingerprint: String::new(),
                    callees: vec![],
                    runtime_effects: 0,
                    unknown: false,
                    unresolved: vec![],
                }
            })
            .collect();
        Snapshot {
            edit_context: None,
            version: 1,
            compiler: String::new(),
            compatibility: String::new(),
            workspace: String::new(),
            revision: String::new(),
            sources: BTreeMap::new(),
            functions,
            semantic,
        }
    }
    fn witness(s: &mut Snapshot, i: usize, bit: u8, kind: &str, operation: &str) {
        s.functions[i].runtime_effects |= bit;
        s.semantic.witnesses.entry(format!("f{i}")).or_default().push(json!({"effect":bit,"witness":{
            "kind":kind,"cause":{"operation":operation,"location":{"path":"test.wi","start":i*10+1,"end":i*10+2}}
        }}));
    }
    fn link(s: &mut Snapshot, caller: usize, callee: usize) {
        let id = s.functions[callee].id.clone();
        s.functions[caller].callees.push(id);
    }
    fn output(s: &Snapshot, i: usize) -> Value {
        Analysis::new(s).output(i)
    }
    fn reason(value: &Value, code: &str) -> bool {
        value[0]["unknown_reasons"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["code"] == code)
    }
    #[test]
    fn pure_is_none() {
        assert_eq!(output(&fixture(1), 0)[0]["state"], "none");
    }
    #[test]
    fn local_witness_is_may() {
        let mut s = fixture(1);
        witness(&mut s, 0, 1, "runtime-capability", "array-allocation");
        assert_eq!(output(&s, 0)[0]["state"], "may");
    }
    #[test]
    fn witness_has_location() {
        let mut s = fixture(1);
        witness(&mut s, 0, 1, "panic", "panic");
        assert_eq!(output(&s, 0)[0]["witness"]["location"]["start"], 1);
    }
    #[test]
    fn each_bit_is_independent() {
        let mut s = fixture(1);
        witness(&mut s, 0, 2, "runtime-capability", "sleep");
        let v = output(&s, 0);
        assert_eq!(v[0]["state"], "none");
        assert_eq!(v[1]["state"], "may");
    }
    #[test]
    fn missing_body_is_unknown() {
        let mut s = fixture(1);
        s.semantic.flows.clear();
        assert!(reason(&output(&s, 0), "missing-body"));
    }
    #[test]
    fn incomplete_flow_is_unknown() {
        let mut s = fixture(1);
        s.semantic.flows[0].complete = false;
        assert!(reason(&output(&s, 0), "incomplete-flow"));
    }
    #[test]
    fn unresolved_callee_is_unknown() {
        let mut s = fixture(1);
        s.functions[0].callees.push("absent".into());
        assert!(reason(&output(&s, 0), "unresolved-callee"));
    }
    #[test]
    fn explicit_unresolved_is_unknown() {
        let mut s = fixture(1);
        s.functions[0].unresolved.push("callee".into());
        assert!(reason(&output(&s, 0), "unresolved-callee"));
    }
    #[test]
    fn unresolved_reason_prefers_call_site_over_declaration() {
        let mut s = fixture(2);
        link(&mut s, 1, 0);
        s.functions[0].unresolved.push("callee".into());
        s.semantic.unresolved_calls.insert(
            "f0".into(),
            vec![semantic::UnresolvedCall {
                operation: "callee".into(),
                location: Location {
                    path: "test.wi".into(),
                    start: 3,
                    end: 7,
                },
            }],
        );
        for i in [0, 1] {
            let value = output(&s, i);
            for effect in value.as_array().unwrap() {
                assert_eq!(effect["state"], "unknown");
                assert_eq!(effect["unknown_reasons"][0]["location"]["start"], 3);
                assert_eq!(effect["unknown_reasons"][0]["location"]["end"], 7);
            }
        }
    }
    #[test]
    fn indirect_witness_is_unknown() {
        let mut s = fixture(1);
        witness(&mut s, 0, 1, "runtime-capability", "indirect-call");
        assert_eq!(output(&s, 0)[0]["state"], "unknown");
        assert!(reason(&output(&s, 0), "indirect-call"));
    }
    #[test]
    fn external_boundary_is_unknown() {
        let mut s = fixture(1);
        witness(&mut s, 0, 1, "external-boundary", "ffi");
        assert!(reason(&output(&s, 0), "external-interop"));
    }
    #[test]
    fn unavailable_witness_is_unknown() {
        let mut s = fixture(1);
        witness(&mut s, 0, 1, "unavailable", "opaque");
        assert!(reason(&output(&s, 0), "missing-witness"));
    }
    #[test]
    fn missing_source_is_unknown() {
        let mut s = fixture(1);
        witness(&mut s, 0, 1, "panic", "panic");
        s.semantic.witnesses.get_mut("f0").unwrap()[0]["witness"]["cause"]["location"] =
            Value::Null;
        assert!(reason(&output(&s, 0), "missing-source"));
    }
    #[test]
    fn transitive_may_keeps_origin() {
        let mut s = fixture(2);
        link(&mut s, 0, 1);
        witness(&mut s, 1, 1, "panic", "panic");
        assert_eq!(output(&s, 0)[0]["witness"]["function"], "f1");
    }
    #[test]
    fn transitive_unknown_never_becomes_may() {
        let mut s = fixture(2);
        link(&mut s, 0, 1);
        s.semantic.flows[1].complete = false;
        assert_eq!(output(&s, 0)[0]["state"], "unknown");
    }
    #[test]
    fn mixed_paths_keep_reasons() {
        let mut s = fixture(3);
        link(&mut s, 0, 1);
        link(&mut s, 0, 2);
        witness(&mut s, 1, 1, "panic", "panic");
        s.semantic.flows[2].complete = false;
        let v = output(&s, 0);
        assert_eq!(v[0]["state"], "may");
        assert!(reason(&v, "incomplete-flow"));
    }
    #[test]
    fn local_mixed_keeps_reasons() {
        let mut s = fixture(1);
        witness(&mut s, 0, 1, "panic", "panic");
        s.semantic.flows[0].complete = false;
        let v = output(&s, 0);
        assert_eq!(v[0]["state"], "may");
        assert!(reason(&v, "incomplete-flow"));
    }
    #[test]
    fn distinct_reason_categories_survive() {
        let mut s = fixture(2);
        link(&mut s, 0, 1);
        s.semantic.flows[1].complete = false;
        s.functions[1].unresolved.push("callee".into());
        let v = output(&s, 0);
        assert!(reason(&v, "incomplete-flow"));
        assert!(reason(&v, "unresolved-callee"));
    }
    #[test]
    fn unknown_cycle_terminates() {
        let mut s = fixture(2);
        link(&mut s, 0, 1);
        link(&mut s, 1, 0);
        s.semantic.flows[1].complete = false;
        assert_eq!(output(&s, 0)[0]["state"], "unknown");
    }
    #[test]
    fn pure_cycle_is_none() {
        let mut s = fixture(2);
        link(&mut s, 0, 1);
        link(&mut s, 1, 0);
        assert_eq!(output(&s, 0)[0]["state"], "none");
    }
    #[test]
    fn bound_only_cycle_is_unknown() {
        let mut s = fixture(2);
        link(&mut s, 0, 1);
        link(&mut s, 1, 0);
        for f in &mut s.functions {
            f.runtime_effects = 1;
        }
        assert_eq!(output(&s, 0)[0]["state"], "unknown");
    }
    #[test]
    fn duplicate_edges_do_not_duplicate_reasons() {
        let mut s = fixture(2);
        link(&mut s, 0, 1);
        link(&mut s, 0, 1);
        s.semantic.flows[1].complete = false;
        assert_eq!(
            output(&s, 0)[0]["unknown_reasons"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn disconnected_unknown_does_not_taint_pure() {
        let mut s = fixture(2);
        s.semantic.flows[1].complete = false;
        assert_eq!(output(&s, 0)[0]["state"], "none");
    }
    #[test]
    fn foreign_owner_is_not_local_evidence() {
        let mut s = fixture(1);
        witness(&mut s, 0, 1, "panic", "panic");
        s.semantic.witnesses.get_mut("f0").unwrap()[0]["witness"]["owner"] = json!("foreign");
        assert_eq!(output(&s, 0)[0]["state"], "unknown");
    }
    #[test]
    fn out_of_range_is_not_local_evidence() {
        let mut s = fixture(1);
        witness(&mut s, 0, 1, "panic", "panic");
        s.functions[0].locations.clear();
        assert_eq!(output(&s, 0)[0]["state"], "unknown");
    }
    #[test]
    fn safety_bits_are_unchanged() {
        let mut s = fixture(1);
        s.functions[0].runtime_effects = 63;
        s.functions[0].unknown = true;
        let before = s.functions.clone();
        let _ = output(&s, 0);
        assert_eq!(s.functions, before);
    }
    #[test]
    fn indirect_bucket_does_not_promote_candidate_witness() {
        let mut s = fixture(2);
        s.functions[0].id = "$indirect-arity:1".into();
        s.functions[0].synthetic = true;
        s.functions[0].runtime_effects = 1;
        link(&mut s, 0, 1);
        witness(&mut s, 1, 1, "panic", "panic");
        assert_eq!(output(&s, 0)[0]["state"], "unknown");
    }
    #[test]
    fn unavailable_symbol_has_all_unknown() {
        assert!(
            Analysis::unavailable()
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["state"] == "unknown")
        );
    }
    #[test]
    fn bound_cycle_beside_witness_retains_uncertainty() {
        let mut s = fixture(3);
        link(&mut s, 0, 1);
        link(&mut s, 1, 2);
        link(&mut s, 2, 1);
        for f in &mut s.functions {
            f.runtime_effects = 1;
        }
        witness(&mut s, 0, 1, "panic", "panic");
        let v = output(&s, 0);
        assert_eq!(v[0]["state"], "may");
        assert!(reason(&v, "missing-witness"));
    }
    #[test]
    fn unexplained_unknown_flag_remains_fail_closed() {
        let mut s = fixture(1);
        s.functions[0].unknown = true;
        witness(&mut s, 0, 1, "panic", "panic");
        let v = output(&s, 0);
        assert_eq!(v[0]["state"], "may");
        assert!(reason(&v, "unresolved-callee"));
        assert_eq!(v[1]["state"], "unknown");
    }
    #[test]
    fn deterministic_graph_counts() {
        for shape in ["chain", "fanout", "cycle"] {
            for n in [16, 64, 256, 1024] {
                let mut s = fixture(n);
                for i in 1..n {
                    link(&mut s, if shape == "fanout" { 0 } else { i - 1 }, i);
                }
                if shape == "cycle" {
                    link(&mut s, n - 1, 0);
                }
                for i in if shape == "fanout" { 1..n } else { n - 1..n } {
                    witness(&mut s, i, 1, "panic", "panic");
                    s.semantic.flows[i].complete = false;
                }
                let a = Analysis::new(&s);
                let e = if shape == "cycle" { n } else { n - 1 };
                assert!(
                    a.edge_visits <= (COUNT + 1) * e,
                    "{shape} {n}: {}",
                    a.edge_visits
                );
                assert_eq!(a.output(0)[0]["state"], "may");
                assert!(reason(&a.output(0), "incomplete-flow"));
                println!(
                    "three-state shape={shape} vertices={n} edges={e} visits={} summary_slots={}",
                    a.edge_visits,
                    n * COUNT * (REASONS.len() + 1)
                );
            }
        }
    }
}
