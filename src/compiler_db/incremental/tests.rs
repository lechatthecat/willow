use super::*;

#[test]
fn source_owner_classification_preserves_wire_keys_and_borrowed_lookup() {
    for (key, function) in [
        ("Function:f", Some("f")),
        ("Function:名前", Some("名前")),
        ("Function:f/methods:m", Some("f/methods:m")),
        ("Class:C/methods:m", None),
        ("Interface:I/methods:f", None),
        ("FunctionLike:f", None),
        ("", None),
    ] {
        let wire = serde_json::to_string(key).unwrap();
        let owner: SourceOwner = serde_json::from_str(&wire).unwrap();
        assert_eq!(owner.function_name(), function);
        assert_eq!(owner.wire_key(), key);
        assert_eq!(serde_json::to_string(&owner).unwrap(), wire);
        let map = HashMap::from([(owner.clone(), 42)]);
        assert_eq!(map.get(key), Some(&42));
        let encoded = serde_json::to_value(&map).unwrap();
        assert_eq!(encoded, serde_json::json!({key: 42}));
        let restored: HashMap<SourceOwner, i32> = serde_json::from_value(encoded).unwrap();
        assert_eq!(restored.get(key), Some(&42));
        assert_eq!(restored.keys().next().unwrap().function_name(), function);
        if let Some(name) = function {
            assert_eq!(owner, SourceOwner::function(name));
        }
    }
    let mut index = std::collections::BTreeMap::new();
    index_declarations(
        &serde_json::json!([
            {"Function": {"name": "f", "body": null}},
            {"Class": {"name": "C", "methods": [{"name": "m", "body": null}]}}
        ]),
        "",
        None,
        &mut index,
    );
    let function = index.get_key_value("Function:f").unwrap().0;
    assert_eq!(function.function_name(), Some("f"));
    let method = index.get_key_value("Class:C/methods:m").unwrap().0;
    assert_eq!(method.function_name(), None);
}

#[test]
fn derived_typed_wire_ownership_schema_and_green_hits() {
    for size in [0, 1, 8, 64, 256] {
        for shape in ["empty", "flat", "nested", "strings"] {
            let mut queries = SyntaxQueries::default();
            let node = QueryNode::LirUnit(UnitId::ENTRY);
            let payload = match shape {
                "empty" => serde_json::json!([]),
                "flat" => serde_json::json!((0..size).collect::<Vec<_>>()),
                "nested" => {
                    serde_json::json!((0..size).map(|n| vec![n, n]).collect::<Vec<_>>())
                }
                _ => serde_json::json!(vec!["repeated"; size]),
            };
            queries
                .publish_derived(node.clone(), payload.clone(), vec![])
                .unwrap();
            let stats = queries.stats();
            let mut decoded: Value = queries
                .validate_derived_typed(node.clone(), vec![])
                .unwrap()
                .unwrap();
            assert_eq!(decoded, payload);
            if let Some(array) = decoded.as_array_mut() {
                array.push(Value::Null);
            }
            assert_eq!(
                queries.validate_derived(node.clone(), vec![]).unwrap(),
                Some(payload.clone())
            );
            assert_eq!(queries.stats().recomputed, stats.recomputed);
            let memo = queries.table.peek(&node).unwrap();
            let old = memo.get::<DerivedValue>();
            let canonical = old.canonical.clone();
            for wire in [old.wire[1..].to_vec(), {
                let mut wire = old.wire.as_ref().clone();
                wire[0] = 0;
                wire
            }] {
                queries
                    .table
                    .refresh_value(
                        &node,
                        QueryValue::new(
                            DerivedValue {
                                wire: wire.into(),
                                canonical: canonical.clone(),
                                epoch: 0,
                            },
                            ResultFingerprint::bytes(&canonical),
                        ),
                    )
                    .unwrap();
                assert!(
                    queries
                        .validate_derived(node.clone(), vec![])
                        .unwrap()
                        .is_none()
                );
                queries
                    .publish_derived(node.clone(), payload.clone(), vec![])
                    .unwrap();
                assert_eq!(
                    queries.validate_derived(node.clone(), vec![]).unwrap(),
                    Some(payload.clone())
                );
            }
            queries.finish().unwrap();
            assert_eq!(
                queries.validate_derived(node, vec![]).unwrap(),
                Some(payload)
            );
        }
    }
}

#[test]
fn effect_publication_delta_retains_unread_inputs_and_scales_by_changed_owner() {
    use super::super::effects::{CapturedEffect, EffectCapabilities, EffectEvidence};
    use crate::semantic::effects::{EffectSummary, RuntimeEffects};
    use std::sync::Arc;
    let capture = |io| CapturedEffect {
        capabilities: EffectCapabilities {
            runtime: RuntimeEffects::NONE,
            may_io: io,
        },
        evidence: EffectEvidence {
            summary: Arc::new(EffectSummary::default()),
        },
        helper: None,
        loop_bodies: Vec::new(),
    };
    for size in [8, 32, 128] {
        let mut first = SyntaxQueries::default().candidate().unwrap();
        let ids: Vec<_> = (0..size)
            .map(|i| FunctionId::free(format!("item_{i}")))
            .collect();
        first
            .publish_effect_delta(
                UnitId::ENTRY,
                vec![],
                vec![],
                ids.iter().map(|&id| (id, Some(capture(false)))).collect(),
                vec![],
            )
            .unwrap();
        first.finish().unwrap();
        let before = first
            .table
            .peek(&QueryNode::EffectInventory(UnitId::ENTRY))
            .unwrap();
        let untouched = before.get::<EffectInventoryValue>().functions[&ids[1]].clone();
        let mut next = first.candidate().unwrap();
        next.changed_body_units
            .update(UnitId::ENTRY, |bodies| bodies.insert(BodyId::fresh()));
        assert!(!next.validate_effects(UnitId::ENTRY, vec![]).unwrap());
        next.publish_effect_delta(
            UnitId::ENTRY,
            vec![],
            vec![],
            [(ids[0], Some(capture(true)))].into(),
            vec![],
        )
        .unwrap();
        assert_eq!(next.effect_inventory_owners(), 1);
        assert_eq!(next.effect_dependency_reads(), 1);
        next.finish().unwrap();
        assert!(
            !next
                .effect_capabilities(UnitId::ENTRY, ids[size - 1])
                .unwrap()
                .may_io
        );
        assert!(
            next.effect_capabilities(UnitId::ENTRY, ids[0])
                .unwrap()
                .may_io
        );
        let after = next
            .table
            .peek(&QueryNode::EffectInventory(UnitId::ENTRY))
            .unwrap();
        assert!(Arc::ptr_eq(
            &untouched,
            &after.get::<EffectInventoryValue>().functions[&ids[1]]
        ));
        assert_eq!(before.get::<EffectInventoryValue>().functions.len(), size);
    }
}

#[test]
fn effect_inventory_byte_accounting_tracks_stored_vector_capacity() {
    use super::super::effects::{CapturedEffect, EffectCapabilities, EffectEvidence};
    use crate::semantic::effects::{EffectSummary, RuntimeEffects};
    use std::sync::Arc;
    let id = FunctionId::free("value");
    let body = BodyId::fresh();
    let mut accepted = SyntaxQueries::default();
    for step in 0..9 {
        let mut next = accepted.candidate().unwrap();
        next.changed_body_units
            .update(UnitId::ENTRY, |bodies| bodies.insert(body));
        let mut loops = Vec::with_capacity(32);
        loops.push(body);
        let effect = (step < 8).then(|| CapturedEffect {
            capabilities: EffectCapabilities {
                runtime: RuntimeEffects::NONE,
                may_io: step % 2 == 0,
            },
            evidence: EffectEvidence {
                summary: Arc::new(EffectSummary::default()),
            },
            helper: None,
            loop_bodies: loops,
        });
        next.publish_effect_delta(UnitId::ENTRY, vec![], vec![], [(id, effect)].into(), vec![])
            .unwrap();
        let value = next
            .table
            .peek(&QueryNode::EffectInventory(UnitId::ENTRY))
            .unwrap();
        let inventory = value.get::<EffectInventoryValue>();
        assert_eq!(
            inventory.bytes,
            inventory
                .functions
                .values()
                .map(|v| v.as_ref().retained_bytes())
                .sum::<usize>()
        );
        if step == 8 {
            assert_eq!(inventory.bytes, 0);
        }
        next.finish().unwrap();
        accepted = next;
    }
}

#[test]
fn finish_prunes_inventory_for_units_absent_from_candidate() {
    let mut queries = SyntaxQueries::default();
    queries
        .dispatch_classes
        .insert(UnitId::ENTRY, ["Removed".into()].into());
    queries.interface_dispatch.insert(UnitId::ENTRY, vec![]);
    queries
        .effect_functions
        .insert(UnitId::ENTRY, Default::default());
    let mut candidate = queries.candidate().unwrap();
    candidate.finish().unwrap();
    assert!(candidate.dispatch_classes.is_empty());
    assert!(candidate.interface_dispatch.is_empty());
    assert!(candidate.effect_functions.is_empty());
    assert_eq!(candidate.dispatch_classes.capacity(), 0);
    assert_eq!(candidate.interface_dispatch.capacity(), 0);
    assert_eq!(candidate.effect_functions.capacity(), 0);
    // Finishing an unaccepted candidate must not mutate accepted inventory.
    assert_eq!(queries.dispatch_classes.len(), 1);
    assert_eq!(queries.interface_dispatch.len(), 1);
    assert_eq!(queries.effect_functions.len(), 1);
}
#[test]
fn finish_releases_preparation_and_preserves_late_query_reads() {
    let path = Path::new("main.wi");
    let mut queries = SyntaxQueries::default().candidate().unwrap();
    queries.parse(path, FileId::ENTRY, "fn main() {}").unwrap();
    let body = BodyId::fresh();
    queries.register_body(UnitId::ENTRY, body, path, "Function:main");
    queries
        .publish_body(
            UnitId::ENTRY,
            body,
            serde_json::json!({"typed": true}),
            vec![],
            vec![],
        )
        .unwrap();
    let node = QueryNode::NormalizedBody(body);
    let wire = serde_json::json!({"body": [], "metadata": "retained"});
    let dependencies = vec![QueryNode::TypedBody(body)];
    queries
        .publish_derived(node.clone(), wire.clone(), dependencies.clone())
        .unwrap();
    let before = queries.retained_query_bytes();
    queries.finish().unwrap();
    assert!(
        queries.prepared.is_empty()
            && queries.derived.is_empty()
            && queries.effects.is_empty()
            && queries.symbols.is_empty()
    );
    assert!(queries.retained_query_bytes() < before);
    let stats = queries.stats();
    assert_eq!(
        queries
            .validate_derived(node.clone(), dependencies)
            .unwrap(),
        Some(wire)
    );
    assert_eq!(queries.stats().recomputed, stats.recomputed);
    let late = QueryNode::LirBody(body);
    queries
        .publish_derived(
            late.clone(),
            serde_json::json!(["late"]),
            vec![node.clone()],
        )
        .unwrap();
    assert_eq!(
        queries.validate_derived(late, vec![node]).unwrap(),
        Some(serde_json::json!(["late"]))
    );
}
#[test]
fn compact_parse_preserves_artifact_deserialization_depth() {
    let mut accepted = 0;
    for depth in 1..80 {
        let source = format!(
            "fn f(x: {}i64{}) {{}}",
            "Array<".repeat(depth),
            ">".repeat(depth)
        );
        let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
        let (program, diagnostics) = crate::parser::Parser::new(tokens).parse();
        assert!(diagnostics.is_empty());
        let wire = serde_json::to_vec(&program).unwrap();
        if serde_json::from_slice::<Program>(&wire).is_err() {
            continue;
        }
        accepted += 1;
        let mut queries = SyntaxQueries::default().candidate().unwrap();
        queries
            .parse(Path::new("nested.wi"), FileId::ENTRY, &source)
            .unwrap_or_else(|error| panic!("depth={depth}: {error}"));
    }
    assert!(accepted > 10);
}
#[test]
fn compact_json_roundtrips() {
    let cases = [
        Value::Null,
        serde_json::json!(true),
        serde_json::json!(1),
        serde_json::json!(1.5),
        serde_json::json!("quote\"\\\n日本語"),
        serde_json::json!([1, false, null, {"nested": ["x", "y"]}]),
        serde_json::json!({"b": 2, "a": 1}),
    ];
    for a in &cases {
        let encoded = CompactJson::new(a).unwrap();
        assert_eq!(encoded.decode::<Value>().unwrap(), *a);
    }
}
#[test]
fn unchecked_template_source_body_journal_and_current_span() {
    let path = Path::new("template.wi");
    let initial = "fn identity<T>(x:T)->T { return x; } fn main() {}";
    let edited = "// shifted\nfn identity<T>(x:T)->T { println(1); return x; } fn main() {}";
    let mut accepted = SyntaxQueries::default().candidate().unwrap();
    accepted.parse(path, FileId::ENTRY, initial).unwrap();
    let mut next = accepted.candidate().unwrap();
    next.parse(path, FileId::ENTRY, edited).unwrap();
    assert_eq!(
        next.source_body_changes(UnitId::ENTRY),
        vec![SourceOwner::function("identity")]
    );
    assert!(next.changed_bodies_in(UnitId::ENTRY).is_empty());
    assert_eq!(
        next.source_owner_span(UnitId::ENTRY, &SourceOwner::function("identity"))
            .unwrap()
            .start,
        edited.find("fn identity").unwrap()
    );
    next.parse(path, FileId::ENTRY, edited).unwrap();
    assert_eq!(
        next.source_body_changes(UnitId::ENTRY),
        vec![SourceOwner::function("identity")]
    );
    let mut moved = next.candidate().unwrap();
    let moved_source = format!("\n{edited}");
    moved.parse(path, FileId::ENTRY, &moved_source).unwrap();
    assert!(moved.source_body_changes(UnitId::ENTRY).is_empty());
    assert_eq!(
        moved
            .source_owner_span(UnitId::ENTRY, &SourceOwner::function("identity"))
            .unwrap()
            .start,
        moved_source.find("fn identity").unwrap()
    );
    let mut removed = moved.candidate().unwrap();
    removed.parse(path, FileId::ENTRY, "fn main() {}").unwrap();
    assert_eq!(
        removed.source_body_changes(UnitId::ENTRY),
        vec![SourceOwner::function("identity")]
    );
    assert!(
        removed
            .source_owner_span(UnitId::ENTRY, &SourceOwner::function("identity"))
            .is_none()
    );
}

#[test]
fn owner_projection_recomputations_ignore_unrelated_functions() {
    for count in [8, 32, 128] {
        let path = Path::new("owners.wi");
        let unrelated: String = (0..count)
            .map(|i| format!("fn function_{i}() -> i64 {{ return {i}; }}\n"))
            .collect();
        let source = format!("fn edited() -> i64 {{ return 1; }} {unrelated}");
        let mut first = SyntaxQueries::default().candidate().unwrap();
        first.parse(path, FileId::ENTRY, &source).unwrap();
        let owners: Vec<_> = std::iter::once("Function:edited".to_owned())
            .chain((0..count).map(|i| format!("Function:function_{i}")))
            .collect();
        for owner in &owners {
            first.body_syntax(path, owner).unwrap();
            first.signature(path, owner).unwrap();
        }
        first.finish().unwrap();
        let mut next = first.candidate().unwrap();
        next.parse(
            path,
            FileId::ENTRY,
            &source.replacen("return 1;", "return 20;", 1),
        )
        .unwrap();
        for owner in &owners {
            next.body_syntax(path, owner).unwrap();
            next.signature(path, owner).unwrap();
        }
        assert_eq!(next.stats().recomputed, 4, "unrelated={count}");
        assert_eq!(
            next.recomputations(&QueryNode::BodySyntax(
                path.into(),
                "Function:edited".into()
            )),
            1
        );
        for owner in &owners[1..] {
            assert_eq!(
                next.recomputations(&QueryNode::BodySyntax(path.into(), owner.clone())),
                0
            );
            assert_eq!(
                next.recomputations(&QueryNode::SyntaxSignature(path.into(), owner.clone())),
                0
            );
        }
    }
}

#[test]
fn owner_projection_missing_add_remove_and_disk_roundtrip() {
    let path = Path::new("missing-owner.wi");
    let owner = "Function:later";
    let mut current = SyntaxQueries::default().candidate().unwrap();
    for source in [
        "fn main() {}",
        "fn later() -> i64 { return 1; } fn main() {}",
        "\nfn later() -> i64 { return 1; } fn main() {}",
        "fn main() {}",
        "fn main() {}",
        "fn later() -> bool { return true; } fn main() {}",
    ] {
        let mut next = current.candidate().unwrap();
        next.parse(path, FileId::ENTRY, source).unwrap();
        let body = next.body_syntax(path, owner).unwrap();
        let signature = next.signature(path, owner).unwrap();
        assert_eq!(body.is_null(), !source.contains("fn later"));
        assert_eq!(signature.is_null(), !source.contains("fn later"));
        next.finish().unwrap();
        let wire = serde_json::to_vec(&next.disk_snapshot().unwrap()).unwrap();
        current = SyntaxQueries::from_disk(serde_json::from_slice(&wire).unwrap()).unwrap();
    }
}

#[test]
fn compact_parse_storage_and_projection_work_scale_with_owners() {
    let mut previous = None;
    for count in [16, 64, 256] {
        let mut queries = SyntaxQueries::default().candidate().unwrap();
        let source: String = (0..count)
            .map(|i| format!("fn function_{i}() -> i64 {{ return {i}; }}\n"))
            .collect();
        let path = Path::new("compact.wi");
        queries.parse(path, FileId::ENTRY, &source).unwrap();
        let result = queries.table.peek(&QueryNode::Parse(path.into())).unwrap();
        let parsed = result.get::<ParsedValue>();
        assert_eq!(parsed.owners.len(), count);
        let bytes = parsed.retained_bytes();
        // Bounded serialized storage per generated owner, including tokens,
        // diagnostics and the separately indexed body/signature projections.
        assert!(bytes < 16_384 * count);
        if let Some(old) = previous {
            assert!(bytes < old * 5);
        }
        previous = Some(bytes);
        let before = queries.stats();
        for i in 0..count {
            queries
                .body_syntax(path, &format!("Function:function_{i}"))
                .unwrap();
            queries
                .signature(path, &format!("Function:function_{i}"))
                .unwrap();
        }
        assert_eq!(queries.stats().recomputed - before.recomputed, 2 * count);
        assert_eq!(
            queries.stats().dependency_reads - before.dependency_reads,
            2 * count
        );
        let projection = queries
            .table
            .peek(&QueryNode::BodySyntax(
                path.into(),
                "Function:function_0".into(),
            ))
            .unwrap();
        assert!(
            std::sync::Arc::ptr_eq(
                &parsed.owners["Function:function_0"].1.bytes,
                &projection.get::<CompactJson>().bytes,
            ),
            "publishing a projection must share its encoded payload"
        );
        println!(
            "compact_parse owners={count} retained_bytes={bytes} projection_reads={}",
            2 * count
        );
    }
}
#[test]
fn retained_accounting_includes_auxiliary_payload_capacity() {
    let mut queries = SyntaxQueries::default();
    let empty = queries.retained_query_bytes();
    let mut source = String::with_capacity(65_536);
    source.push_str("fn main() {}");
    queries.captured.insert(
        PathBuf::from("main.wi"),
        Source {
            file: FileId::ENTRY,
            text: source,
        },
    );
    assert!(queries.retained_query_bytes() >= empty + 65_536);
    let captured = queries.retained_query_bytes();
    queries.prepared.insert(
        BodyId::fresh(),
        PreparedBody {
            result: CompactJson::new(&Value::String("x".repeat(32_768))).unwrap(),
            keys: Vec::with_capacity(128),
            children: Vec::new(),
        },
    );
    assert!(
        queries.retained_query_bytes() >= captured + 32_768 + 128 * std::mem::size_of::<String>()
    );
}
#[test]
fn immutable_package_durability_requires_git_revision_and_checksum() {
    use super::super::tracked::Durability;
    use crate::package::{
        PackageGraph, PackageId, PackageIdentity, PackageSourceIdentity, ResolvedPackage,
    };
    for git in [false, true] {
        for revision in [false, true] {
            for checksum in [false, true] {
                let root = PathBuf::from("dependencies/pkg");
                let package = ResolvedPackage {
                    id: PackageId(0),
                    root: root.clone(),
                    dependencies: vec![],
                    checksum: checksum.then(|| "checksum".into()),
                    identity: PackageIdentity {
                        name: "pkg".into(),
                        version: "1.0.0".into(),
                        source: if git {
                            PackageSourceIdentity::Git {
                                url: "https://example.invalid/pkg".into(),
                            }
                        } else {
                            PackageSourceIdentity::Path { path: root.clone() }
                        },
                        revision: revision.then(|| "revision".into()),
                    },
                };
                let graph = PackageGraph {
                    native: None,
                    root: PackageId(0),
                    packages: vec![package],
                    stats: Default::default(),
                };
                let mut queries = SyntaxQueries::default();
                queries.configure_sources(Some(&graph));
                assert_eq!(
                    queries.source_durability(&root.join("module.wi")),
                    if git && revision && checksum {
                        Durability::High
                    } else {
                        Durability::Low
                    }
                );
                assert_eq!(
                    queries.source_durability(Path::new("main.wi")),
                    Durability::Low
                );
            }
        }
    }
}
#[test]
fn compiler_environment_is_high_and_skips_edges_after_source_edit() {
    let mut queries = SyntaxQueries::default().candidate().unwrap();
    queries
        .parse(
            Path::new("main.wi"),
            FileId::ENTRY,
            "fn main() { println(1); }",
        )
        .unwrap();
    let mut next = queries.candidate().unwrap();
    next.parse(
        Path::new("main.wi"),
        FileId::ENTRY,
        "fn main() { println(2); }",
    )
    .unwrap();
    assert!(next.stats().durability_shortcuts > 0);
    assert_eq!(next.recomputations(&QueryNode::CompilerEnvironment), 0);
}
#[test]
fn review_reference_locations_refresh_without_semantic_recomputation() {
    use super::super::references::{SymbolId, SymbolUseId};
    let symbol = SymbolId("f".into());
    let id = SymbolUseId {
        owner: SymbolId("g".into()),
        ordinal: 0,
    };
    let members = std::iter::once((symbol.clone(), vec![id.clone()])).collect();
    let reference = |start| serde_json::json!({"target":"f", "role":"call", "location":{"path":"entry.wi", "start":start, "end":start+1}});
    let mut first = SyntaxQueries::default().candidate().unwrap();
    first
        .capture_references(vec![(id.clone(), reference(1))], &members)
        .unwrap();
    first.references(symbol.clone()).unwrap();
    let mut shifted = first.candidate().unwrap();
    shifted
        .capture_references(vec![(id.clone(), reference(100))], &members)
        .unwrap();
    assert_eq!(shifted.resolved_reference(id).unwrap(), reference(100));
    assert_eq!(shifted.references(symbol).unwrap(), vec![reference(100)]);
    assert_eq!(shifted.stats().recomputed, 0);
    assert!(
        shifted
            .references(SymbolId("unused".into()))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn review_visible_scope_tracks_imports_and_resolved_declarations() {
    let path = Path::new("scope.wi");
    let mut first = SyntaxQueries::default().candidate().unwrap();
    first
        .parse(path, FileId::ENTRY, "fn f() -> i64 { return 1; }")
        .unwrap();
    let scope = serde_json::json!({"f":"i64"});
    first
        .visible_scope(UnitId::ENTRY, path, scope.clone())
        .unwrap();
    let mut body = first.candidate().unwrap();
    body.parse(path, FileId::ENTRY, "fn f() -> i64 { return 2; }")
        .unwrap();
    let before = body.stats();
    assert_eq!(
        body.visible_scope(UnitId::ENTRY, path, scope.clone())
            .unwrap(),
        scope
    );
    assert_eq!(body.stats().recomputed, before.recomputed);
    let mut imported = body.candidate().unwrap();
    imported
        .parse(
            path,
            FileId::ENTRY,
            "import value; fn f() -> i64 { return 2; }",
        )
        .unwrap();
    let before = imported.stats();
    imported
        .visible_scope(UnitId::ENTRY, path, scope.clone())
        .unwrap();
    assert_eq!(imported.stats().recomputed, before.recomputed + 1);
    let mut signature = imported.candidate().unwrap();
    signature
        .parse(
            path,
            FileId::ENTRY,
            "import value; fn f() -> i64 { return 2; }",
        )
        .unwrap();
    let changed = serde_json::json!({"f":"i64", "value::get":"String"});
    assert_eq!(
        signature
            .visible_scope(UnitId::ENTRY, path, changed.clone())
            .unwrap(),
        changed
    );
    assert_ne!(
        signature
            .read_visible_scope(UnitId::ENTRY, path)
            .unwrap()
            .get::<CompactJson>()
            .decode::<Value>()
            .unwrap(),
        scope
    );
}

#[test]
fn review_reference_queries_retarget_remove_and_isolate_symbols() {
    use super::super::references::{SymbolId, SymbolUseId};
    for count in [8, 32, 128] {
        let symbol = |i| SymbolId(format!("function:{i}"));
        let use_id = |i| SymbolUseId {
            owner: SymbolId(format!("body:{i}")),
            ordinal: 0,
        };
        let members: std::collections::BTreeMap<_, _> =
            (0..count).map(|i| (symbol(i), vec![use_id(i)])).collect();
        let uses: Vec<_> = (0..count)
            .map(|i| (use_id(i), serde_json::json!({"target": i})))
            .collect();
        let mut first = SyntaxQueries::default().candidate().unwrap();
        first.capture_references(uses.clone(), &members).unwrap();
        for (i, (_, reference)) in uses.iter().enumerate() {
            assert_eq!(
                first.references(symbol(i)).unwrap(),
                vec![reference.clone()]
            );
        }
        let mut next = first.candidate().unwrap();
        let mut changed = uses.clone();
        changed[0].1 = serde_json::json!({"target":1});
        let mut changed_members = members.clone();
        changed_members.remove(&symbol(0));
        changed_members.get_mut(&symbol(1)).unwrap().push(use_id(0));
        next.capture_references(changed, &changed_members).unwrap();
        assert!(next.references(symbol(0)).unwrap().is_empty());
        assert_eq!(next.references(symbol(1)).unwrap().len(), 2);
        let before = next.stats();
        for (i, (_, reference)) in uses.iter().enumerate().skip(2) {
            assert_eq!(next.references(symbol(i)).unwrap(), vec![reference.clone()]);
        }
        assert_eq!(next.stats().recomputed, before.recomputed);
        let mut removed = next.candidate().unwrap();
        removed
            .capture_references(vec![], &Default::default())
            .unwrap();
        assert!(removed.references(symbol(1)).unwrap().is_empty());
        assert!(removed.resolved_reference(use_id(0)).unwrap().is_null());
        println!("review_references symbols={count} unrelated_recomputations=0");
    }
}

#[test]
fn repeated_semantic_consumers_share_one_captured_signature() {
    for size in [8, 32, 128] {
        let signature = Rc::new(serde_json::json!((0..size).collect::<Vec<_>>()));
        let mut queries = SyntaxQueries::default().candidate().unwrap();
        for _ in 0..size {
            queries
                .capture_semantic(
                    UnitId::ENTRY,
                    vec![("shared".into(), Rc::clone(&signature))],
                )
                .unwrap();
            assert!(Rc::ptr_eq(
                &queries.symbols[&(UnitId::ENTRY, "shared".into())],
                &signature,
            ));
            assert_eq!(Rc::strong_count(&signature), 2);
        }
        assert_eq!(queries.symbols.len(), 1);
        // Sharing is an optimization, never a substitute for validating
        // a separately produced value under the same input key.
        assert!(
            queries
                .capture_semantic(UnitId::ENTRY, vec![("shared".into(), Rc::new(Value::Null))],)
                .is_err()
        );
    }
}

#[test]
fn body_edit_backdates_declarations_and_imports() {
    let mut accepted = SyntaxQueries::default().candidate().unwrap();
    let path = Path::new("entry.wi");
    accepted
        .parse(path, FileId::ENTRY, "fn f() -> i64 { return 1; }")
        .unwrap();
    let mut next = accepted.candidate().unwrap();
    next.parse(path, FileId::ENTRY, "fn f() -> i64 { return 2; }")
        .unwrap();
    assert_eq!(next.stats().recomputed, 3);
    assert_eq!(next.stats().changed, 1);
    assert_eq!(next.stats().green_after_recompute, 2);
    assert_eq!(next.stats().dependency_edges_visited, 4);
}
#[test]
fn per_owner_projections_have_linear_edge_visits() {
    for count in [8, 32, 128] {
        let path = Path::new("entry.wi");
        let source = (0..count)
            .map(|i| format!("fn function_{i}() -> i64 {{ return {i}; }}\n"))
            .collect::<String>();
        let mut accepted = SyntaxQueries::default().candidate().unwrap();
        accepted.parse(path, FileId::ENTRY, &source).unwrap();
        for i in 0..count {
            accepted
                .signature(path, &format!("Function:function_{i}"))
                .unwrap();
            accepted
                .body_syntax(path, &format!("Function:function_{i}"))
                .unwrap();
        }
        let changed = source.replacen("return 0", "return 999", 1);
        let mut next = accepted.candidate().unwrap();
        next.parse(path, FileId::ENTRY, &changed).unwrap();
        for i in 0..count {
            assert_eq!(
                accepted
                    .signature(path, &format!("Function:function_{i}"))
                    .unwrap(),
                next.signature(path, &format!("Function:function_{i}"))
                    .unwrap()
            );
            let before = accepted
                .body_syntax(path, &format!("Function:function_{i}"))
                .unwrap();
            let after = next
                .body_syntax(path, &format!("Function:function_{i}"))
                .unwrap();
            assert_eq!(before == after, i != 0);
        }
        assert_eq!(next.stats().dependency_edges_visited, 4 + 2 * count);
        assert_eq!(next.stats().changed, 2); // parse and edited body projection
        assert_eq!(next.stats().green_after_recompute, 2);
        assert_eq!(next.stats().recomputed, 4);
    }
}
#[test]
fn signature_and_coordinates_are_not_backdated_incorrectly() {
    let path = Path::new("entry.wi");
    let mut first = SyntaxQueries::default().candidate().unwrap();
    first
        .parse(path, FileId::ENTRY, "fn f() -> i64 { return 1; }")
        .unwrap();
    let signature = first.signature(path, "Function:f").unwrap();
    let mut next = first.candidate().unwrap();
    let (program, _, _) = next
        .parse(path, FileId::ENTRY, "\nfn f() -> String { return \"a\"; }")
        .unwrap();
    assert_ne!(signature, next.signature(path, "Function:f").unwrap());
    let crate::parser::ast::Item::Function(function) = &program.items[0] else {
        panic!()
    };
    assert_eq!(function.span.line, 2);
    assert_eq!(next.stats().changed, 3); // parse, declarations, signature
}

#[test]
fn typed_syntax_stamp_is_scoped_to_its_query() {
    let value = |owner: &str| TypedBodyValue {
        syntax_node: QueryNode::BodySyntax("entry.wi".into(), owner.into()),
        syntax_revision: super::super::tracked::Revision(1),
        result: CompactJson::new(&Value::Null).unwrap(),
    };
    assert!(value("Function:f") == value("Function:f"));
    assert!(value("Function:f") != value("Function:g"));
}

#[test]
fn typed_syntax_stamp_preserves_changes_with_equal_typed_results() {
    let path = Path::new("entry.wi");
    let body = BodyId::fresh();
    let mut accepted = SyntaxQueries::default();
    let mut stamps = Vec::new();
    for (source, reuse) in [
        ("fn f() -> i64 { return 1; }", false),
        ("fn f() -> i64 { return 1; }", true),
        ("\nfn f() -> i64 { return 1; }", true),
        ("fn f() -> i64 { return 2; }", false),
        ("fn f() -> i64 { return 1; }", false),
    ] {
        let mut next = accepted.candidate().unwrap();
        next.parse(path, FileId::ENTRY, source).unwrap();
        assert_eq!(
            next.validate_body(UnitId::ENTRY, body, path, "Function:f", vec![])
                .unwrap(),
            reuse
        );
        if !reuse {
            next.publish_body(UnitId::ENTRY, body, Value::Null, vec![], vec![])
                .unwrap();
        }
        let value = next
            .table
            .read(&next.provider(), QueryNode::TypedBody(body))
            .unwrap();
        stamps.push(value.get::<TypedBodyValue>().syntax_revision);
        accepted = next;
    }
    assert_eq!(stamps[0], stamps[1]);
    assert_eq!(stamps[1], stamps[2]);
    assert_ne!(stamps[2], stamps[3]);
    assert_ne!(stamps[3], stamps[4]);
}

#[test]
fn typed_demand_runs_only_after_real_dependency_change() {
    let path = Path::new("entry.wi");
    let mut first = SyntaxQueries::default().candidate().unwrap();
    let source = "fn f() -> i64 { return 1; } fn g() -> i64 { return f(); }";
    first.parse(path, FileId::ENTRY, source).unwrap();
    let f = BodyId::fresh();
    let g = BodyId::fresh();
    let symbol = || vec![("function:f".to_owned(), Rc::new(serde_json::json!("i64")))];
    assert!(
        !first
            .validate_body(UnitId::ENTRY, f, path, "Function:f", vec![])
            .unwrap()
    );
    first
        .publish_body(UnitId::ENTRY, f, Value::Null, vec![], vec![])
        .unwrap();
    assert!(
        !first
            .validate_body(UnitId::ENTRY, g, path, "Function:g", symbol())
            .unwrap()
    );
    first
        .publish_body(UnitId::ENTRY, g, Value::Null, symbol(), vec![])
        .unwrap();
    let mut next = first.candidate().unwrap();
    next.parse(
        path,
        FileId::ENTRY,
        &source.replacen("return 1", "return 2", 1),
    )
    .unwrap();
    assert!(
        !next
            .validate_body(UnitId::ENTRY, f, path, "Function:f", vec![])
            .unwrap()
    );
    next.publish_body(UnitId::ENTRY, f, Value::Null, vec![], vec![])
        .unwrap();
    assert!(
        next.validate_body(UnitId::ENTRY, g, path, "Function:g", symbol())
            .unwrap()
    );
    let mut signature_change = next.candidate().unwrap();
    signature_change
        .parse(
            path,
            FileId::ENTRY,
            &source.replace("fn f() -> i64", "fn f() -> String"),
        )
        .unwrap();
    assert!(
        !signature_change
            .validate_body(
                UnitId::ENTRY,
                g,
                path,
                "Function:g",
                vec![("function:f".into(), Rc::new(serde_json::json!("String")))]
            )
            .unwrap()
    );
    // A deferred read is not cached as a diagnostic; retry after supplying
    // checked output succeeds and keeps the original dependency graph.
    signature_change
        .publish_body(
            UnitId::ENTRY,
            g,
            Value::Bool(true),
            vec![("function:f".into(), Rc::new(serde_json::json!("String")))],
            vec![],
        )
        .unwrap();
    assert!(
        signature_change
            .validate_body(UnitId::ENTRY, g, path, "Function:g", vec![])
            .unwrap()
    );
}

#[test]
fn measured_symbol_edges_avoid_aggregate_typecheck_fanout() {
    for count in [8, 32, 128] {
        for aggregate in [false, true] {
            let path = Path::new("entry.wi");
            let source = (0..count)
                // `fN` names would collide with builtin types such as `f64`.
                .map(|i| format!("fn fun{i}() -> i64 {{ return {i}; }}\n"))
                .collect::<String>();
            let mut first = SyntaxQueries::default().candidate().unwrap();
            first.parse(path, FileId::ENTRY, &source).unwrap();
            let bodies = (0..count).map(|_| BodyId::fresh()).collect::<Vec<_>>();
            let key = |i| {
                if aggregate {
                    "exports".to_owned()
                } else {
                    format!("symbol:{i}")
                }
            };
            for (i, &body) in bodies.iter().enumerate() {
                let reads = vec![(key(i), Rc::new(Value::Bool(false)))];
                assert!(
                    !first
                        .validate_body(
                            UnitId::ENTRY,
                            body,
                            path,
                            &format!("Function:fun{i}"),
                            reads.clone()
                        )
                        .unwrap()
                );
                first
                    .publish_body(UnitId::ENTRY, body, Value::Null, reads, vec![])
                    .unwrap();
            }
            let mut next = first.candidate().unwrap();
            next.parse(path, FileId::ENTRY, &source).unwrap();
            let reads = if aggregate {
                vec![(key(0), Rc::new(Value::Bool(true)))]
            } else {
                (0..count)
                    .map(|i| (key(i), Rc::new(Value::Bool(i == 0))))
                    .collect()
            };
            next.capture_semantic(UnitId::ENTRY, reads).unwrap();
            let mut demands = 0;
            for (i, &body) in bodies.iter().enumerate() {
                demands += usize::from(
                    !next
                        .validate_body(
                            UnitId::ENTRY,
                            body,
                            path,
                            &format!("Function:fun{i}"),
                            vec![],
                        )
                        .unwrap(),
                );
            }
            assert_eq!(demands, if aggregate { count } else { 1 });
            // Per-owner syntax inputs validate without revisiting Parse.
            let expected_visits = if aggregate { 5 * count + 1 } else { 6 * count };
            assert_eq!(next.stats().dependency_edges_visited, expected_visits);
            println!(
                "signature_strategy={} bodies={count} edge_visits={expected_visits} demanded_typechecks={demands}",
                if aggregate { "aggregate" } else { "per_symbol" }
            );
        }
    }
}

#[test]
fn candidate_rollback_and_unchanged_parse() {
    let mut accepted = SyntaxQueries::default().candidate().unwrap();
    let path = Path::new("entry.wi");
    let source = "fn f() -> i64 { return 1; }";
    accepted.parse(path, FileId::ENTRY, source).unwrap();
    let mut rejected = accepted.candidate().unwrap();
    rejected.parse(path, FileId::ENTRY, "fn bad(").unwrap();
    drop(rejected);
    let mut next = accepted.candidate().unwrap();
    next.parse(path, FileId::ENTRY, source).unwrap();
    assert_eq!(next.stats().recomputed, 0);
}
