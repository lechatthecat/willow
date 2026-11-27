use super::*;
#[test]
fn task_effect_local_ranges_merge_overlap_and_keep_source_boundaries() {
    let locations: Vec<_> = [
        ("b", 1, 4),
        ("a", 10, 20),
        ("a", 2, 8),
        ("a", 4, 6),
        ("a", 8, 12),
        ("a", 30, 40),
    ]
    .into_iter()
    .map(|(path, start, end)| Location {
        display: None,
        path: path.into(),
        start,
        end,
    })
    .collect();
    assert_eq!(
        local_ranges(&locations),
        vec![("a", 2, 20), ("a", 30, 40), ("b", 1, 4)]
    );
    assert!(local_ranges(&[]).is_empty());
}

#[test]
fn inherited_effect_chain_scales_with_dispatch_edges() {
    let directory =
        std::env::temp_dir().join(format!("willow-inherited-evidence-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    for shape in ["chain", "fanout"] {
        for n in [8, 32, 128] {
            let mut source = String::from(
                "fn allocate() -> i64 { let xs = [1, 2]; return 2; } open class Base { pub fn charge(self) -> i64 { self.cost(); self.cost(); return self.cost(); } pub open fn cost(self) -> i64 { return 0; } }\n",
            );
            for i in 0..n {
                let base = if shape == "chain" && i != 0 {
                    format!("C{}", i - 1)
                } else {
                    "Base".to_owned()
                };
                let method = if shape == "fanout" || i == n - 1 {
                    "pub override fn cost(self) -> i64 { allocate(); allocate(); return allocate(); }"
                } else {
                    ""
                };
                source.push_str(&format!("open class C{i} extends {base} {{ {method} }}\n"));
            }
            source.push_str("fn main() {}\n");
            std::fs::write(&path, source).unwrap();
            let snapshot = crate::CompilerSession::new(
                path.to_str().unwrap(),
                "",
                &crate::CompilerOptions::debug(),
                None,
            )
            .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
            .unwrap();
            let function = snapshot
                .functions
                .iter()
                .find(|f| f.name == "Base::charge")
                .unwrap()
                .id
                .clone();
            let edges: usize = snapshot.functions.iter().map(|f| f.callees.len()).sum();
            let stored: usize = snapshot.semantic.effect_calls.values().map(Vec::len).sum();
            let revision = snapshot.revision.clone();
            let mut session = QuerySession::new(snapshot).unwrap();
            let value = session.query(QueryRequest::Effects { revision, function });
            let allocation = value["result"]["effect_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["effect"] == 1)
                .unwrap();
            assert_eq!(
                allocation["call_chain"].as_array().unwrap().len(),
                3,
                "{value}"
            );
            assert!(edges <= 4 * n + 10, "edges={edges}");
            assert!(stored <= 3 * n + 10, "stored={stored}");
            assert!(session.effect_edge_visits <= edges);
            println!(
                "inherited shape={shape} n={n} edges={edges} stored={stored} visits={} emitted_chain=3",
                session.effect_edge_visits
            );
        }
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn task_effect_dispatch_mixed_candidates_scale_without_cross_products() {
    let directory =
        std::env::temp_dir().join(format!("willow-dispatch-evidence-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    for n in [8, 32, 128] {
        let mut source = String::from(
            "interface Reading<T> { fn read(self, ch: Channel<i64>) -> Task<()>; }\nasync fn done() {}\n",
        );
        for i in 0..n {
            source.push_str(&format!("class Sync{i} implements Reading<i64> {{ pub fn read(self, ch: Channel<i64>) -> Task<()> {{ ch.send(10); return done(); }} }}\nclass Async{i} implements Reading<i64> {{ pub async fn read(self, ch: Channel<i64>) {{ ch.send(20); }} }}\n"));
        }
        source.push_str("fn invoke(r: Reading<i64>, ch: Channel<i64>) -> Task<()> { return r.read(ch); }\nfn main() {}\n");
        std::fs::write(&path, &source).unwrap();
        let snapshot = crate::CompilerSession::new(
            path.to_str().unwrap(),
            "",
            &crate::CompilerOptions::debug(),
            None,
        )
        .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
        .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "invoke")
            .unwrap()
            .id
            .clone();
        let stored = snapshot
            .semantic
            .effect_calls
            .values()
            .map(Vec::len)
            .sum::<usize>();
        assert!(stored <= n + 3, "n={n} stored={stored}");
        let snapshot: Snapshot =
            serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        let value = session.query(QueryRequest::Effects { revision, function });
        let sites: Vec<_> = value["result"]["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|fact| fact["witness"]["cause"]["operation"] == "channel-send")
            .collect();
        assert_eq!(sites.len(), 2 * n, "{value}");
        let spawned: Vec<_> = sites
            .iter()
            .filter(|fact| fact["task_relationship"]["kind"] == "spawn")
            .collect();
        assert_eq!(spawned.len(), n, "{value}");
        let start = source.find("r.read(ch)").unwrap() + 2;
        assert!(
            spawned
                .iter()
                .all(|fact| fact["task_relationship"]["location"]["start"] == start),
            "{value}"
        );
        assert!(session.effect_edge_visits <= 4 * n + 4);
        println!(
            "mixed candidates={}: stored calls={stored}, emitted sends={}, spawned={}, visited edges={}",
            2 * n,
            sites.len(),
            spawned.len(),
            session.effect_edge_visits
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn task_effect_evidence_scales_with_sites_and_reachable_edges() {
    let directory =
        std::env::temp_dir().join(format!("willow-task-evidence-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    for n in [8, 32, 128] {
        let mut source = format!(
            "async fn leaf(ch: Channel<i64>) {{ {} }}\n",
            "ch.send(1);".repeat(n)
        );
        for i in 0..n {
            let callee = if i == 0 {
                "leaf".to_owned()
            } else {
                format!("relay{}", i - 1)
            };
            source.push_str(&format!(
                "async fn relay{i}(ch: Channel<i64>) {{ {callee}(ch); {callee}(ch); }}\n"
            ));
        }
        source.push_str("fn main() { let ch = Channel<i64>::new();");
        for i in 0..n {
            source.push_str(&format!("relay{i}(ch);"));
        }
        source.push('}');
        std::fs::write(&path, source).unwrap();
        let snapshot = crate::CompilerSession::new(
            path.to_str().unwrap(),
            "",
            &crate::CompilerOptions::debug(),
            None,
        )
        .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
        .unwrap();
        assert_eq!(
            snapshot
                .semantic
                .effect_calls
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            3 * n
        );
        // Task edges survive persisted snapshots, including generic-free IDs.
        let snapshot: Snapshot =
            serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "main")
            .unwrap()
            .id
            .clone();
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        let result = session.query(QueryRequest::Effects { revision, function });
        let sends: Vec<_> = result["result"]["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|fact| fact["witness"]["cause"]["operation"] == "channel-send")
            .collect();
        assert_eq!(sends.len(), n);
        assert!(
            sends
                .iter()
                .all(|fact| fact["task_relationship"]["kind"] == "spawn")
        );
        assert_eq!(session.effect_edge_visits, 2 * n);
        println!(
            "task sites={} send sites={n} emitted sends={} edge visits={}",
            3 * n,
            sends.len(),
            session.effect_edge_visits
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn await_capture_keeps_summary_scans_bounded() {
    for n in [1, 8, 64, 512] {
        let mut captured = Captured::default();
        let function = FunctionId::free("worker");
        for _ in 0..n {
            captured.capability(function, RuntimeEffects::ALL.bits(), Span::dummy(), "await");
            captured.await_type(function, Span::dummy(), Some("Future<void>"), true);
        }
        assert_eq!(captured.suspension_sites[&function].len(), n);
        assert_eq!(
            captured.witnesses[&function].len(),
            RuntimeEffects::BIT_COUNT as usize
        );
        println!(
            "awaits={n}, stored sites={n}, summary scan bound={}",
            RuntimeEffects::BIT_COUNT
        );
    }
}

#[test]
fn channel_capture_keeps_summary_scans_bounded() {
    for n in [1, 8, 64, 512] {
        let mut captured = Captured::default();
        let function = FunctionId::free("worker");
        for _ in 0..n {
            for operation in ["channel-send", "channel-recv"] {
                captured.channel_site(function, Span::dummy(), operation);
                captured.capability(
                    function,
                    RuntimeEffects::ALL.bits(),
                    Span::dummy(),
                    "builtin-method-call",
                );
            }
        }
        assert_eq!(captured.suspension_sites[&function].len(), 2 * n);
        assert_eq!(
            captured.witnesses[&function].len(),
            RuntimeEffects::BIT_COUNT as usize
        );
        println!(
            "pairs={n}, stored sites={}, summary scan bound={}",
            2 * n,
            RuntimeEffects::BIT_COUNT
        );
    }
}

#[test]
fn boundary_call_provenance_scales_with_disconnected_callers_and_wrappers() {
    let directory =
        std::env::temp_dir().join(format!("willow-boundary-provenance-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    for (resolved, n) in [false, true]
        .into_iter()
        .flat_map(|resolved| [16, 64, 256].map(|n| (resolved, n)))
    {
        let mut source = String::from("interface W { fn weight(self) -> i64; }\n");
        if resolved {
            source.push_str("class C implements W { pub fn weight(self) -> i64 { return 1; } }\n");
        }
        let mut origins = Vec::new();
        for i in 0..n {
            source.push_str(&format!("fn caller{i}(w: W) -> i64 {{ return w."));
            origins.push(source.len());
            source.push_str("weight(); }\n");
            source.push_str(&format!(
                "fn wrapper{i}(w: W) -> i64 {{ return caller{i}(w); }}\n"
            ));
            let callee = if i == 0 {
                "wrapper0".to_owned()
            } else {
                format!("chain{}", i - 1)
            };
            source.push_str(&format!(
                "fn chain{i}(w: W) -> i64 {{ return {callee}(w); }}\n"
            ));
        }
        source.push_str("fn main() {}\n");
        std::fs::write(&path, source).unwrap();
        let snapshot = crate::CompilerSession::new(
            path.to_str().unwrap(),
            "",
            &crate::CompilerOptions::debug(),
            None,
        )
        .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
        .unwrap();
        let seeds = snapshot
            .semantic
            .witnesses
            .values()
            .flatten()
            .filter(|w| w.witness.kind == WitnessKind::ExternalBoundary)
            .count();
        assert_eq!(
            seeds, 0,
            "unit-local external summaries must not become workspace seeds"
        );
        let analysis = super::super::effect_states::Analysis::new(&snapshot);
        let by_name: HashMap<_, _> = snapshot
            .functions
            .iter()
            .enumerate()
            .map(|(i, f)| (f.name.as_str(), i))
            .collect();
        for (i, &origin) in origins.iter().enumerate() {
            for (name, expected) in [
                (format!("caller{i}"), origin),
                (format!("wrapper{i}"), origin),
                (format!("chain{i}"), origins[0]),
            ] {
                let output = analysis.output(by_name[name.as_str()]);
                let reasons = output[5]["unknown_reasons"].as_array().unwrap();
                if resolved {
                    assert_eq!(output[5]["state"], "none", "{name}: {output}");
                    assert!(reasons.is_empty(), "{name}: {output}");
                    continue;
                }
                assert_eq!(reasons.len(), 1, "{name}: {output}");
                assert_eq!(reasons[0]["callee"], "weight", "{name}: {output}");
                assert_eq!(
                    reasons[0]["location"]["start"], expected,
                    "{name}: {output}"
                );
                assert_eq!(
                    reasons[0]["location"]["end"],
                    expected + 6,
                    "{name}: {output}"
                );
            }
        }
        // One unresolved reason per effect traverses each wrapper/chain
        // edge. A closed pure union propagates no evidence at all.
        assert_eq!(analysis.edge_visits, if resolved { 0 } else { 14 * n });
        println!(
            "boundary resolved={resolved} callers={n} seeds={seeds} propagated_edge_visits={}",
            analysis.edge_visits
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn dispatch_effects_workspace_union_scales_with_sites_and_implementations() {
    let directory =
        std::env::temp_dir().join(format!("willow-workspace-union-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    for n in [8, 32, 128] {
        let api = format!(
            "module api; pub interface Worker {{ fn work(self) -> i64; }} pub fn invoke(w: Worker) -> i64 {{ {} return 0; }}",
            "w.work();".repeat(n)
        );
        std::fs::write(directory.join("api.wi"), api).unwrap();
        let mut source = String::from("import api;\n");
        for i in 0..n {
            source.push_str(&format!(
                "class C{i} implements api::Worker {{ pub fn work(self) -> i64 {{ return 0; }} }}\n"
            ));
        }
        source.push_str("fn main() {}\n");
        std::fs::write(&path, source).unwrap();
        let snapshot = crate::CompilerSession::new(
            path.to_str().unwrap(),
            "",
            &crate::CompilerOptions::debug(),
            None,
        )
        .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
        .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "invoke")
            .unwrap()
            .id
            .clone();
        let edges: usize = snapshot.functions.iter().map(|f| f.callees.len()).sum();
        let sites: usize = snapshot.semantic.effect_calls.values().map(Vec::len).sum();
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        let value = session.query(QueryRequest::Effects { revision, function });
        assert!(
            value["result"]["effects"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["state"] == "none"),
            "{value}"
        );
        assert_eq!(edges, n + 1);
        assert_eq!(sites, n);
        assert_eq!(session.effect_edge_visits, n + 1);
        println!(
            "workspace dispatch n={n} edges={edges} sites={sites} visits={}",
            session.effect_edge_visits
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn unresolved_call_evidence_scales_with_sites_and_reachable_edges() {
    let directory =
        std::env::temp_dir().join(format!("willow-unresolved-scaling-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    for n in [16, 64, 256] {
        let mut source = format!(
            "fn apply(f: fn(i64) -> i64) -> i64 {{ {} return 0; }}\n",
            "f(1);".repeat(n)
        );
        for i in 0..n {
            let callee = if i == 0 {
                "apply".to_owned()
            } else {
                format!("relay{}", i - 1)
            };
            source.push_str(&format!(
                "fn relay{i}(f: fn(i64) -> i64) -> i64 {{ return {callee}(f); }}\n"
            ));
        }
        source.push_str("fn main() {}\n");
        std::fs::write(&path, source).unwrap();
        let snapshot = crate::CompilerSession::new(
            path.to_str().unwrap(),
            "",
            &crate::CompilerOptions::debug(),
            None,
        )
        .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
        .unwrap();
        assert_eq!(
            snapshot
                .semantic
                .unresolved_calls
                .values()
                .map(Vec::len)
                .sum::<usize>(),
            n
        );
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == format!("relay{}", n - 1))
            .unwrap()
            .id
            .clone();
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        let value = session.query(QueryRequest::Effects { revision, function });
        // `apply` owns the unresolved sites; the arity bucket reports that no
        // function value of that arity is known.
        let causes = value["result"]["unknown_causes"].as_array().unwrap();
        assert_eq!(causes.len(), 2, "{causes:#?}");
        assert!(causes.iter().any(|cause| cause["unresolved"][0]
            == "incomplete-function-value-candidates"
            && cause["call_sites"].is_null()));
        let apply = causes
            .iter()
            .find(|cause| cause["name"] == "apply")
            .unwrap();
        assert_eq!(apply["call_sites"].as_array().unwrap().len(), n);
        // n relay/apply edges plus apply's edge into the arity bucket.
        assert_eq!(session.effect_edge_visits, n + 1);
        println!(
            "sites={n}, chain_edges={n}, stored_sites={n}, reported_sites={n}, visited_edges={}",
            session.effect_edge_visits
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn missing_effect_witness_is_incomplete_and_missing_identity_is_unknown() {
    let directory =
        std::env::temp_dir().join(format!("willow-effect-witness-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    std::fs::write(&path, "fn main() { println(1); }").unwrap();
    let mut snapshot = crate::CompilerSession::new(
        path.to_str().unwrap(),
        "",
        &crate::CompilerOptions::debug(),
        None,
    )
    .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
    .unwrap();
    let id = snapshot
        .functions
        .iter()
        .find(|f| f.name == "main")
        .unwrap()
        .id
        .clone();
    snapshot.semantic.witnesses.clear();
    snapshot.revision = snapshot.digest().unwrap();
    let revision = snapshot.revision.clone();
    let mut session = QuerySession::new(snapshot).unwrap();
    let result = session.query(QueryRequest::Effects {
        revision: revision.clone(),
        function: id,
    });
    assert_eq!(result["result"]["status"], "incomplete");
    assert!(
        result["result"]["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .all(|e| e["status"] == "missing-witness"
                && e["reason"]
                    .as_str()
                    .is_some_and(|r| r.contains("upper bound")))
    );
    let result = session.query(QueryRequest::Effects {
        revision,
        function: "absent".into(),
    });
    assert_eq!(result["result"]["status"], "unknown");
    std::fs::remove_dir_all(directory).unwrap();
}
#[test]
fn builtin_path_display_preserves_bits_and_scales_after_roundtrip() {
    let directory =
        std::env::temp_dir().join(format!("willow-string-paths-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("main.wi");
    let abi =
        crate::semantic::intrinsics::builtin_target_effects(&Intrinsic::StringByteAt.function_id())
            .unwrap();
    assert!(abi.contains(RuntimeEffects::MAY_ALLOCATE));
    assert!(abi.contains(RuntimeEffects::MAY_PANIC));
    for n in [8, 32, 128] {
        let source = format!(
            "fn scan(s: String) {{ for i in 0..s.len() {{ {} }} }}\nfn relay(s: String) {{ scan(s); scan(s); }}\nfn main() {{}}",
            "s.byte_at(i);".repeat(n)
        );
        std::fs::write(&path, source).unwrap();
        let snapshot = crate::CompilerSession::new(
            path.to_str().unwrap(),
            "",
            &crate::CompilerOptions::debug(),
            None,
        )
        .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
        .unwrap();
        let mut baseline = snapshot.clone();
        // Presentation facts must have no influence on ANY internal bit or
        // effect-state propagation. Compare with those facts removed.
        for facts in baseline.semantic.witnesses.values_mut() {
            facts.retain(|f| f.paths.is_none());
        }
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "relay")
            .unwrap()
            .id
            .clone();
        let revision = snapshot.revision.clone();
        let snapshot: Snapshot =
            serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
        let mut session = QuerySession::new(snapshot).unwrap();
        baseline.revision = baseline.digest().unwrap();
        let baseline_revision = baseline.revision.clone();
        let mut baseline = QuerySession::new(baseline).unwrap();
        let value = session.query(QueryRequest::Effects {
            revision,
            function: function.clone(),
        });
        let previous = baseline.query(QueryRequest::Effects {
            revision: baseline_revision,
            function,
        });
        assert_eq!(value["result"]["bounds"], previous["result"]["bounds"]);
        assert_eq!(value["result"]["effects"], previous["result"]["effects"]);
        assert_eq!(
            value["result"]["bounds"]["runtime"].as_u64().unwrap() & 1,
            1
        );
        let paths: Vec<_> = value["result"]["effect_evidence"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e.get("paths"))
            .collect();
        assert_eq!(paths.len(), n + 1);
        assert_eq!(session.effect_edge_visits, 1);
        assert!(
            paths
                .iter()
                .filter(|p| p["operation"] == "String.byte_at")
                .all(|p| p["normal"]["may_allocate"] == "none"
                    && p["panic"]["may_allocate"] == "may")
        );
        println!(
            "reads={n}, emitted path facts={}, graph edge visits={}",
            paths.len(),
            session.effect_edge_visits
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}
