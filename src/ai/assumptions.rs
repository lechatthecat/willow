//! Reviewed external assumptions are dependencies, never compiler proofs.
//! Two shared witnesses per callable bound propagation independently of the
//! number of foreign declarations reachable through repeated calls or cycles.
use super::{Snapshot, graph};
use serde::Serialize;
use serde_json::Value;
#[cfg(test)]
use serde_json::json;
use std::collections::VecDeque;

/// Attribute spans can lack a source path; retain the nullable wire field.
#[derive(Debug, Clone, Serialize)]
pub struct AssumptionLocation {
    #[serde(flatten)]
    pub display: Option<Box<super::DisplayPosition>>,
    pub end: usize,
    pub path: Option<String>,
    pub start: usize,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssumedEffect {
    NoBlock,
    NoIo,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AssumptionKind {
    Assumption,
}
#[derive(Debug, Clone, Serialize)]
pub struct Assumption {
    pub effect: AssumedEffect,
    pub function: String,
    pub kind: AssumptionKind,
    pub location: Option<AssumptionLocation>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Assumptions<T = Assumption> {
    pub no_block: Option<T>,
    pub no_io: Option<T>,
    pub no_wait: Option<T>,
}
impl Assumptions<&Assumption> {
    pub fn into_owned(self) -> Assumptions {
        Assumptions {
            no_block: self.no_block.cloned(),
            no_io: self.no_io.cloned(),
            no_wait: self.no_wait.cloned(),
        }
    }
}

pub(crate) struct Analysis {
    witnesses: Vec<[Option<usize>; 2]>,
    evidence: Vec<Assumption>,
    pub edge_visits: usize,
}

impl Analysis {
    pub fn new(snapshot: &Snapshot) -> Self {
        let index = graph::Index::new(&snapshot.functions);
        let mut asynchronous = vec![false; snapshot.functions.len()];
        for function in &snapshot.semantic.async_functions {
            if let Some(&i) = index.by_id.get(function.as_str()) {
                asynchronous[i] = true;
            }
        }
        let mut result = Self {
            witnesses: vec![[None; 2]; snapshot.functions.len()],
            evidence: Vec::new(),
            edge_visits: 0,
        };
        for bridge in &snapshot.semantic.interop.bridges {
            let Some(&function) = index.by_id.get(bridge.function.as_str()) else {
                continue;
            };
            let claims = [
                bridge.symbol.assumes_no_block(),
                bridge.symbol.assumes_no_io(),
            ];
            for (effect, claimed) in claims.into_iter().enumerate() {
                if !claimed || result.witnesses[function][effect].is_some() {
                    continue;
                }
                let declaration = bridge.declaration.first();
                let location = bridge.symbol.assumption_span.map_or_else(
                    || {
                        declaration.map(|location| AssumptionLocation {
                            display: None,
                            path: Some(location.path.clone()),
                            start: location.start,
                            end: location.end,
                        })
                    },
                    |span| {
                        let path = bridge
                            .symbol
                            .native
                            .as_ref()
                            .and_then(|native| native.source_path.to_str())
                            .or_else(|| declaration.map(|location| location.path.as_str()));
                        Some(AssumptionLocation {
                            display: None,
                            path: path.map(str::to_owned),
                            start: span.start,
                            end: span.end,
                        })
                    },
                );
                let witness = result.evidence.len();
                result.evidence.push(Assumption {
                    kind: AssumptionKind::Assumption,
                    effect: [AssumedEffect::NoBlock, AssumedEffect::NoIo][effect],
                    function: bridge.function.clone(),
                    location,
                });
                result.witnesses[function][effect] = Some(witness);
            }
        }
        // Bridge attachment originates in a hash map. Seed in snapshot order
        // so equally near assumptions do not change with hash iteration order.
        let mut pending = VecDeque::new();
        for (function, witnesses) in result.witnesses.iter().enumerate() {
            for (effect, witness) in witnesses.iter().enumerate() {
                if witness.is_some() {
                    pending.push_back((function, effect));
                }
            }
        }
        while let Some((callee, effect)) = pending.pop_front() {
            // Constructing an async task does not execute its waiting body.
            // IO still follows the conservative task dependency graph.
            if effect == 0 && asynchronous[callee] {
                continue;
            }
            for &caller in &index.callers[callee] {
                result.edge_visits += 1;
                if result.witnesses[caller][effect].is_none() {
                    result.witnesses[caller][effect] = result.witnesses[callee][effect];
                    pending.push_back((caller, effect));
                }
            }
        }
        result
    }

    /// One representative dependency per asserted effect, not a success claim.
    /// Callers combine these with their checked contract/capability result.
    pub fn typed_output(&self, function: usize) -> Assumptions<&Assumption> {
        let [block, io] = self.witnesses[function];
        Assumptions {
            no_block: block.map(|id| &self.evidence[id]),
            no_io: io.map(|id| &self.evidence[id]),
            no_wait: block.map(|id| &self.evidence[id]),
        }
    }
    pub fn output(&self, function: usize) -> Value {
        serde_json::to_value(self.typed_output(function)).expect("assumptions serialize")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::{Function, SemanticFacts, interop::Bridge};
    use crate::rust_bridge_ir::{RustBridgeSymbol, Scalar};

    fn fixture(count: usize) -> Snapshot {
        let functions: Vec<Function> = (0..count)
            .map(|i| {
                serde_json::from_value(json!({
                    "id":format!("f{i}"),"module":"test.wi","name":format!("f{i}"),
                    "locations":[],"synthetic":false,"fingerprint":"","body_fingerprint":"",
                    "callees":[],"runtime_effects":0,"unknown":false,"unresolved":[],
                }))
                .unwrap()
            })
            .collect();
        Snapshot {
            edit_context: None,
            version: 1,
            compiler: String::new(),
            compatibility: String::new(),
            workspace: String::new(),
            revision: String::new(),
            sources: Default::default(),
            functions,
            semantic: SemanticFacts::default(),
        }
    }

    fn seed(snapshot: &mut Snapshot, function: usize, block: bool, io: bool, current: bool) {
        let mut symbol = RustBridgeSymbol::new(format!("f{function}"), vec![], Scalar::Void);
        if block {
            symbol.declare_nonblocking(String::new());
        }
        symbol.no_io = io;
        symbol.apply_review(current);
        snapshot.semantic.interop.bridges.push(Bridge {
            function: format!("f{function}"),
            identity: None,
            declaration: vec![crate::ai::Location {
                display: None,
                path: "native.rs".into(),
                start: function * 100,
                end: function * 100 + 50,
            }],
            declaration_text: String::new(),
            symbol,
        });
    }

    #[test]
    fn reviewed_assumptions_keep_effects_review_and_source_independent() {
        for (block, io, current) in [
            (false, false, true),
            (true, false, true),
            (false, true, true),
            (true, true, true),
            (true, true, false),
        ] {
            let mut snapshot = fixture(2);
            snapshot.functions[1].callees = vec!["f0".into()];
            seed(&mut snapshot, 0, block, io, current);
            let analysis = Analysis::new(&snapshot);
            for function in [0, 1] {
                let evidence = analysis.output(function);
                for (contract, expected) in [
                    ("no_block", block && current),
                    ("no_io", io && current),
                    ("no_wait", block && current),
                ] {
                    assert_eq!(!evidence[contract].is_null(), expected);
                    if expected {
                        assert_eq!(evidence[contract]["kind"], "assumption");
                        assert_eq!(evidence[contract]["function"], "f0");
                        assert_eq!(evidence[contract]["location"]["path"], "native.rs");
                    }
                }
            }
        }
    }

    #[test]
    fn reviewed_assumptions_use_attribute_span_and_mask_async_waiting() {
        let mut snapshot = fixture(3);
        seed(&mut snapshot, 0, true, true, true);
        snapshot.semantic.interop.bridges[0].symbol.assumption_span =
            Some(crate::diagnostics::Span::new(7, 38, 1, 8));
        snapshot.functions[1].callees = vec!["f0".into()];
        snapshot.functions[2].callees = vec!["f1".into()];
        snapshot.semantic.async_functions.insert("f1".into());
        let analysis = Analysis::new(&snapshot);
        assert_eq!(analysis.output(1)["no_block"]["location"]["start"], 7);
        assert_eq!(analysis.output(1)["no_block"]["location"]["end"], 38);
        assert!(analysis.output(2)["no_block"].is_null());
        assert!(analysis.output(2)["no_wait"].is_null());
        assert_eq!(analysis.output(2)["no_io"]["kind"], "assumption");
    }

    #[test]
    fn reviewed_assumption_choice_ignores_bridge_attachment_order() {
        let mut snapshot = fixture(3);
        seed(&mut snapshot, 0, true, true, true);
        seed(&mut snapshot, 1, true, true, true);
        snapshot.functions[2].callees = vec!["f1".into(), "f0".into()];
        let first = Analysis::new(&snapshot).output(2);
        snapshot.semantic.interop.bridges.reverse();
        assert_eq!(Analysis::new(&snapshot).output(2), first);
        assert_eq!(first["no_io"]["function"], "f0");
    }

    #[test]
    fn reviewed_assumption_propagation_visits_each_edge_twice_at_most() {
        for count in [8usize, 32, 128, 512] {
            for shape in ["chain", "fanout", "cycle", "many-seeds"] {
                let mut snapshot = fixture(count + 1);
                let seeds = if shape == "many-seeds" { count / 2 } else { 1 };
                for i in 0..seeds {
                    seed(&mut snapshot, i, true, true, true);
                }
                for i in seeds..count {
                    snapshot.functions[i].callees = match shape {
                        "fanout" => vec!["f0".into(), "f0".into()],
                        "many-seeds" if i == seeds => (0..seeds).map(|j| format!("f{j}")).collect(),
                        _ => vec![format!("f{}", i - 1)],
                    };
                }
                if shape == "cycle" {
                    snapshot.functions[0].callees = vec![format!("f{}", count - 1)];
                }
                let analysis = Analysis::new(&snapshot);
                let edges = match shape {
                    "cycle" => count,
                    "fanout" => (count - 1) * 2,
                    _ => count - 1,
                };
                assert_eq!(analysis.edge_visits, edges * 2, "{shape}, {count}");
                assert_eq!(analysis.evidence.len(), seeds * 2);
                for i in 0..count {
                    assert_eq!(analysis.output(i)["no_io"]["kind"], "assumption");
                    assert_eq!(analysis.output(i)["no_block"]["kind"], "assumption");
                }
                assert!(analysis.output(count)["no_io"].is_null());
                println!(
                    "assumptions shape={shape} nodes={count} seeds={seeds} edges={edges} visits={}",
                    analysis.edge_visits
                );
            }
        }
    }
}
