use super::*;
use std::{fs, path::PathBuf};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow-semantic-verify-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    fn capture(&self, source: &str) -> Baseline {
        fs::write(self.0.join("main.wi"), source).unwrap();
        Baseline::capture(&self.0.join("main.wi"), None).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn basic() -> &'static str {
    "pub fn value() -> i64 { return 1; } pub fn main() { let n = value(); }"
}

#[test]
fn semantic_verify_identical_public_api() {
    let f = Fixture::new();
    let a = f.capture(basic());
    let b = f.capture(basic());
    let r = a.compare(&b).unwrap();
    assert!(r.complete);
    assert_eq!(r.public_api.status, "unchanged");
    assert!(r.public_api.unchanged_count >= 2);
    assert!(r.signatures.is_empty());
    assert!(r.effects.changed.is_empty());
    assert_eq!(r.work.compared_symbols, 0);
    assert_eq!(r.work.index_nodes, 1);
}
#[test]
fn semantic_verify_body_change_and_callers() {
    let f = Fixture::new();
    let a = f.capture(basic());
    let b = f.capture(&basic().replace("return 1", "return 2"));
    let r = a.compare(&b).unwrap();
    assert_eq!(r.public_api.status, "unchanged");
    assert!(
        r.affected_callers.iter().any(|s| s.contains("main")),
        "{r:?}"
    );
}
#[test]
fn semantic_verify_signature_values() {
    let f = Fixture::new();
    let a = f.capture(basic());
    let b = f.capture("pub fn value() -> bool { return true; } pub fn main() { let n = value(); }");
    let r = a.compare(&b).unwrap();
    assert!(r.complete);
    assert_eq!(r.signatures.len(), 1, "{r:?}");
    assert_eq!(
        r.signatures.changed[0].before.as_ref().unwrap()["type"],
        "fn() -> i64"
    );
    assert_eq!(
        r.signatures.changed[0].after.as_ref().unwrap()["type"],
        "fn() -> bool"
    );
}
#[test]
fn semantic_verify_added_removed_function() {
    let f = Fixture::new();
    let a = f.capture("fn main() {}");
    let b = f.capture("fn extra() -> i64 { return 1; } fn main() {}");
    let r = a.compare(&b).unwrap();
    assert!(
        r.signatures
            .iter()
            .any(|c| c.before.is_none() && c.after.is_some())
    );
    let r = b.compare(&a).unwrap();
    assert!(
        r.signatures
            .iter()
            .any(|c| c.before.is_some() && c.after.is_none())
    );
}
#[test]
fn semantic_verify_nonawaitable_diagnostic_and_recovery() {
    let f = Fixture::new();
    let a = f.capture("fn main() {}");
    let b = f.capture("async fn main() { let n = await 1; }");
    let r = a.compare(&b).unwrap();
    assert!(r.complete, "{r:?}");
    assert!(!r.effects.newly_non_awaitable.is_empty(), "{r:?}");
    assert!(r.diagnostics.added.iter().any(|d| d.code == "E0803"));
    let r = b.compare(&a).unwrap();
    assert!(r.diagnostics.removed.iter().any(|d| d.code == "E0803"));
}
#[test]
fn semantic_verify_parser_error_is_not_unchanged() {
    let f = Fixture::new();
    let a = f.capture(basic());
    let b = f.capture("fn main( {");
    let r = a.compare(&b).unwrap();
    assert!(!r.complete);
    assert_eq!(r.public_api.status, "unknown");
    assert!(!r.diagnostics.added.is_empty());
}
#[test]
fn semantic_verify_baseline_roundtrip_and_no_overwrite() {
    let f = Fixture::new();
    let a = f.capture(basic());
    let path = f.0.join("baseline.json");
    a.save(&path).unwrap();
    assert!(a.save(&path).is_err());
    let b = Baseline::load(&path).unwrap();
    assert!(a.compare(&b).unwrap().signatures.is_empty());
}
#[test]
fn semantic_verify_deterministic_json() {
    let f = Fixture::new();
    let a = f.capture(basic());
    let b = f.capture(&basic().replace("return 1", "return 3"));
    let c = f.capture(&basic().replace("return 1", "return 3"));
    assert_eq!(
        serde_json::to_string(&a.compare(&b).unwrap()).unwrap(),
        serde_json::to_string(&a.compare(&c).unwrap()).unwrap()
    );
}
#[test]
fn semantic_verify_relative_diagnostic_locations() {
    let f = Fixture::new();
    let a = f.capture("fn main() { let n = await 1; }");
    assert!(
        a.diagnostics
            .iter()
            .flat_map(|d| &d.locations)
            .all(|l| !Path::new(&l.path).is_absolute())
    );
}
#[test]
fn semantic_verify_readonly_capture() {
    let f = Fixture::new();
    f.capture(basic());
    let before = fs::read(f.0.join("main.wi")).unwrap();
    Baseline::capture(&f.0.join("main.wi"), None).unwrap();
    assert_eq!(fs::read(f.0.join("main.wi")).unwrap(), before);
    assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
}
#[test]
fn semantic_verify_missing_input_is_infrastructure_error() {
    let f = Fixture::new();
    assert!(Baseline::capture(&f.0.join("missing.wi"), None).is_err());
}
#[test]
fn semantic_verify_corrupt_baseline_rejected() {
    let f = Fixture::new();
    let path = f.0.join("baseline.json");
    fs::write(&path, "{}").unwrap();
    assert!(Baseline::load(&path).is_err());
}
#[test]
fn semantic_verify_compiler_mismatch_rejected() {
    let f = Fixture::new();
    let a = f.capture(basic());
    let mut b = a.clone();
    b.compiler.push('x');
    assert!(a.compare(&b).is_err());
}
#[test]
fn semantic_verify_configuration_mismatch_rejected() {
    let f = Fixture::new();
    let a = f.capture(basic());
    let mut b = a.clone();
    b.compatibility.push('x');
    assert!(a.compare(&b).is_err());
}
fn synthetic(n: usize) -> Baseline {
    let facts = (0..n)
        .map(|i| {
            (
                format!("f{i}"),
                Arc::new(Fact {
                    signature: Some(json!({"type":"fn() -> i64","exported":true})),
                    effects: Some(EffectState { may: 0, unknown: 0 }),
                    body: "one".into(),
                    callees: vec![],
                }),
            )
        })
        .collect();
    let mut b = Baseline {
        schema_version: 1,
        compiler: "test".into(),
        compatibility: "test".into(),
        revision: "test".into(),
        complete: true,
        diagnostics: vec![],
        facts,
        inputs: im::OrdMap::new(),
        callers: im::OrdMap::new(),
        index: None,
        diagnostic_index: None,
        diagnostic_lookup: BTreeMap::new(),
        public_count: 0,
    };
    b.reindex().unwrap();
    b
}
#[test]
fn semantic_verify_sparse_effect_index_matches_full_reindex() {
    for count in [8, 32, 128] {
        let original = synthetic(count);
        let original_wire = serde_json::to_value(&original).unwrap();
        let mut sparse = original.clone();
        let identity = hash("f0");
        let root = original.index.as_ref().unwrap();
        let changed_digit = identity.as_bytes()[root.prefix.len()] as char;
        let (&sibling_digit, sibling) = root
            .children
            .iter()
            .find(|(digit, _)| **digit != changed_digit)
            .unwrap();

        sparse
            .facts
            .get_mut("f0")
            .map(Arc::make_mut)
            .unwrap()
            .effects = Some(EffectState {
            may: crate::semantic::effects::RuntimeEffects::MAY_BLOCK.bits(),
            unknown: 0,
        });
        let visits = sparse.refresh_effect_fact_index("f0").unwrap();
        assert!(visits <= 65);
        assert!(Arc::ptr_eq(
            sibling,
            &sparse.index.as_ref().unwrap().children[&sibling_digit]
        ));
        let mut cold = sparse.clone();
        cold.reindex().unwrap();
        assert_eq!(
            serde_json::to_value(&sparse.index).unwrap(),
            serde_json::to_value(&cold.index).unwrap()
        );
        assert_eq!(
            serde_json::to_value(&sparse).unwrap(),
            serde_json::to_value(&cold).unwrap()
        );
        assert_eq!(original.facts["f0"].effects.as_ref().unwrap().may, 0);
        assert_eq!(serde_json::to_value(&original).unwrap(), original_wire);

        // The index operation owns only the leaf. Visibility/caller bookkeeping
        // is deliberately untouched here so this tests the deletion projection.
        sparse.facts.remove("f0");
        assert!(sparse.refresh_effect_fact_index("f0").unwrap() <= 65);
        assert!(Arc::ptr_eq(
            sibling,
            &sparse.index.as_ref().unwrap().children[&sibling_digit]
        ));
        cold = sparse.clone();
        cold.reindex().unwrap();
        assert_eq!(
            serde_json::to_value(&sparse.index).unwrap(),
            serde_json::to_value(&cold.index).unwrap()
        );
        let sparse_wire = serde_json::to_value(&sparse).unwrap();
        assert_eq!(sparse_wire, serde_json::to_value(&cold).unwrap());
        assert_eq!(
            sparse_wire.as_object().unwrap().keys().collect::<Vec<_>>(),
            original_wire
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>()
        );
        let mut restored: Baseline = serde_json::from_value(sparse_wire.clone()).unwrap();
        assert!(restored.index.is_none());
        restored.reindex().unwrap();
        assert_eq!(
            serde_json::to_value(&restored.index).unwrap(),
            serde_json::to_value(&sparse.index).unwrap()
        );
        assert_eq!(serde_json::to_value(&restored).unwrap(), sparse_wire);
    }
}

#[test]
fn semantic_verify_radix_scaling_counts() {
    for n in [16, 64, 256, 1024, 4096] {
        let a = synthetic(n);
        let mut b = a.clone();
        b.facts.get_mut("f0").map(Arc::make_mut).unwrap().body = "two".into();
        b.reindex().unwrap();
        let r = a.compare(&b).unwrap();
        println!(
            "semantic_diff unrelated={n} nodes={} compared={} edges={}",
            r.work.index_nodes, r.work.compared_symbols, r.work.caller_edges
        );
        assert_eq!(r.work.compared_symbols, 1);
        assert!(r.work.index_nodes <= 16 * 64 + 1);
        assert_eq!(r.public_api.unchanged_count, n);
    }
}
#[test]
fn semantic_verify_radix_insert_delete_shapes() {
    let a = synthetic(128);
    for i in 0..128 {
        let mut b = a.clone();
        b.facts.remove(&format!("f{i}"));
        b.reindex().unwrap();
        for r in [a.compare(&b).unwrap(), b.compare(&a).unwrap()] {
            assert_eq!(r.signatures.len(), 1, "{i}");
            assert_eq!(r.work.compared_symbols, 1);
        }
    }
}
#[test]
fn semantic_verify_radix_empty_singleton() {
    let a = synthetic(0);
    let b = synthetic(1);
    assert!(a.compare(&a).unwrap().signatures.is_empty());
    assert_eq!(a.compare(&b).unwrap().signatures.len(), 1);
    assert_eq!(b.compare(&a).unwrap().signatures.len(), 1);
}
#[test]
fn semantic_verify_radix_matches_full_oracle() {
    let a = synthetic(200);
    for offset in 0..16 {
        let mut b = a.clone();
        for i in (offset..200).step_by(17) {
            b.facts
                .get_mut(&format!("f{i}"))
                .map(Arc::make_mut)
                .unwrap()
                .signature = Some(json!({"type":"fn() -> bool","exported":true}));
        }
        b.reindex().unwrap();
        let expected: Vec<_> = a
            .facts
            .iter()
            .filter(|(k, v)| b.facts.get(*k) != Some(*v))
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(
            a.compare(&b)
                .unwrap()
                .signatures
                .iter()
                .map(|c| c.symbol.clone())
                .collect::<Vec<_>>(),
            expected
        );
    }
}
#[test]
fn semantic_verify_effect_unknown_transition() {
    let a = synthetic(2);
    let mut b = a.clone();
    b.facts.get_mut("f0").map(Arc::make_mut).unwrap().effects =
        Some(EffectState { may: 1, unknown: 2 });
    b.reindex().unwrap();
    let r = a.compare(&b).unwrap();
    assert_eq!(r.effects.changed.len(), 1);
    assert_eq!(r.effects.changed[0].after.as_ref().unwrap().unknown, 2);
}
#[test]
fn semantic_verify_diagnostic_sort_and_e2604() {
    let a = synthetic(1);
    let mut b = a.clone();
    b.diagnostics.push(DiagnosticFact {
        code: "E2604".into(),
        severity: "error".into(),
        message: "lock".into(),
        locations: vec![],
    });
    b.reindex().unwrap();
    let r = a.compare(&b).unwrap();
    assert_eq!(r.effects.new_e2604.len(), 1);
    assert_eq!(b.compare(&a).unwrap().diagnostics.removed.len(), 1);
}
#[test]
fn semantic_verify_deep_chain_and_cycle_callers() {
    let mut a = synthetic(1024);
    for i in 0..1024 {
        let callee = format!("f{}", (i + 1) % 1024);
        a.facts
            .get_mut(&format!("f{i}"))
            .map(Arc::make_mut)
            .unwrap()
            .callees
            .push(callee.clone());
        a.callers.entry(callee).or_default().insert(format!("f{i}"));
    }
    a.reindex().unwrap();
    let mut b = a.clone();
    b.facts.get_mut("f0").map(Arc::make_mut).unwrap().body = "two".into();
    b.reindex().unwrap();
    let r = a.compare(&b).unwrap();
    assert_eq!(r.affected_callers.len(), 1023);
    assert_eq!(r.work.caller_edges, 2048);
}
#[test]
fn semantic_verify_fanout_and_shared_caller() {
    let mut a = synthetic(512);
    for i in 1..512 {
        a.facts
            .get_mut(&format!("f{i}"))
            .map(Arc::make_mut)
            .unwrap()
            .callees
            .push("f0".into());
        a.callers
            .entry("f0".into())
            .or_default()
            .insert(format!("f{i}"));
    }
    a.reindex().unwrap();
    let mut b = a.clone();
    b.facts.get_mut("f0").map(Arc::make_mut).unwrap().body = "two".into();
    b.reindex().unwrap();
    let r = a.compare(&b).unwrap();
    assert_eq!(r.affected_callers.len(), 511);
    assert_eq!(r.work.caller_edges, 1022);
}

#[test]
fn semantic_verify_lock_exposure_and_normal_checker_rejects() {
    let f = Fixture::new();
    let a = f.capture("async fn main() { let m = Mutex::new(0); lock m as value {} }");
    let b =
        f.capture("async fn main() { let m = Mutex::new(0); lock m as value { await sleep(1); } }");
    let r = a.compare(&b).unwrap();
    assert!(r.complete, "{r:?}");
    assert!(!r.effects.new_e2604.is_empty(), "{r:?}");
    let entry = f.0.join("main.wi");
    assert!(
        crate::CompilerSession::new(
            entry.to_str().unwrap(),
            "",
            &crate::CompilerOptions::debug(),
            None
        )
        .analysis_with_emitter(&mut Diagnostics::default())
        .is_err()
    );
}
#[test]
fn semantic_verify_type_members_and_generic_signature() {
    let f = Fixture::new();
    let a = f.capture("class Box<T> { value: T; } fn main() {}");
    let b = f.capture("class Box<T> { value: i64; } fn main() {}");
    let r = a.compare(&b).unwrap();
    assert!(r.complete, "{r:?}");
    assert!(
        r.signatures.iter().any(|c| c.symbol.contains("field")),
        "{r:?}"
    );
}
#[test]
fn semantic_verify_transaction_dryrun_and_rejection() {
    let f = Fixture::new();
    let original = format!(
        "{} fn warning(n: i64) -> i64 {{ return match n {{ _ => 1, x if x > 0 => 2 }}; }}",
        basic()
    );
    let base = f.capture(&original);
    assert!(base.diagnostics.iter().any(|d| d.code == "W1201"));
    let entry = f.0.join("main.wi");
    let source = fs::read(&entry).unwrap();
    let snapshot = crate::CompilerSession::new(
        entry.to_str().unwrap(),
        "",
        &crate::CompilerOptions::debug(),
        None,
    )
    .analysis_for_edit_with_emitter(&mut Diagnostics::default())
    .unwrap();
    let function = snapshot
        .functions
        .iter()
        .find(|x| x.name == "value")
        .unwrap()
        .id
        .clone();
    let workspace = edit::Workspace::open(&f.0).unwrap();
    let preview = workspace
        .prepare(
            &entry,
            false,
            edit::Request {
                revision: snapshot.revision,
                operations: vec![edit::Operation::ReplaceBody {
                    function,
                    body: "{ return 7; }".into(),
                }],
            },
            edit::ChangeFormat::Diff,
            &mut Diagnostics::default(),
        )
        .unwrap();
    let id = preview["transaction"].as_str().unwrap();
    let report = workspace.verify(id).unwrap();
    let manifest: EditManifest =
        serde_json::from_value(preview["verification_manifest"].clone()).unwrap();
    let attested = workspace.verify_manifest(id, &manifest).unwrap();
    let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
    Baseline::capture_retained(&entry, None, &mut retained).unwrap();
    for _ in 0..3 {
        let warm = workspace
            .verify_manifest_retained(id, &manifest, &mut retained)
            .unwrap();
        assert_eq!(
            serde_json::to_value(&report).unwrap(),
            serde_json::to_value(warm).unwrap()
        );
    }
    assert_eq!(retained.incremental_stats["concurrency_checks"], 0);
    assert_eq!(retained.incremental_stats["signature_checks"], 0);
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::to_value(attested).unwrap()
    );
    for case in 0..11 {
        let mut invalid = manifest.clone();
        match case {
            0 => invalid.complete = false,
            1 => invalid.schema_version = 2,
            2 => invalid.baseline_revision = "0".repeat(64),
            3 => invalid.changes.clear(),
            4 => invalid.changes.push(invalid.changes[0].clone()),
            5 => invalid.changes[0].path = "../main.wi".into(),
            6 => invalid.changes[0].path = "other.wi".into(),
            7 => invalid.changes[0].before = Some("0".repeat(64)),
            8 => invalid.changes[0].after = Some("0".repeat(64)),
            9 => invalid.changes[0].before = None,
            10 => invalid.changes[0].after = None,
            _ => unreachable!(),
        }
        assert!(
            workspace.verify_manifest(id, &invalid).is_err(),
            "case {case}"
        );
    }
    let candidate = f.0.join(".willow-edits").join(id).join("main.wi");
    let candidate_bytes = fs::read(&candidate).unwrap();
    fs::write(&candidate, "fn main() {}").unwrap();
    assert!(workspace.verify_manifest(id, &manifest).is_err());
    fs::write(&candidate, &candidate_bytes).unwrap();
    fs::write(&entry, "fn main() {}").unwrap();
    assert!(workspace.verify_manifest(id, &manifest).is_err());
    fs::write(&entry, &source).unwrap();
    assert!(report.complete, "{report:?}");
    assert!(report.diagnostics.added.is_empty(), "{report:?}");
    assert!(report.diagnostics.removed.is_empty(), "{report:?}");
    assert_eq!(report.public_api.status, "unchanged");
    assert_eq!(source, fs::read(&entry).unwrap());
    assert_eq!(
        workspace.preview(id, edit::ChangeFormat::Diff).unwrap()["state"],
        "prepared"
    );
    assert_eq!(
        serde_json::to_value(&report).unwrap(),
        serde_json::to_value(workspace.verify(id).unwrap()).unwrap()
    );
    let retained =
        f.0.join(".willow-edits")
            .join(format!("{id}.baseline.json"));
    let bytes = fs::read(&retained).unwrap();
    fs::write(&retained, "corrupt retained evidence").unwrap();
    assert!(workspace.verify(id).is_err());
    fs::write(&retained, bytes).unwrap();
    workspace.verify(id).unwrap();
    fs::write(&entry, "fn main() {}").unwrap();
    assert!(workspace.verify(id).is_err());
}

#[test]
fn semantic_verify_private_signature_preserves_public_api() {
    let f = Fixture::new();
    let a = f.capture("fn helper() -> i64 { return 1; } pub fn main() {}");
    let b = f.capture("fn helper() -> bool { return true; } pub fn main() {}");
    let r = a.compare(&b).unwrap();
    assert_eq!(r.signatures.changed.len(), 1);
    assert_eq!(r.public_api.status, "unchanged");
    assert_eq!(r.public_api.unchanged_count, 1);
}
#[test]
fn semantic_verify_visibility_is_a_signature_change() {
    let f = Fixture::new();
    let a = f.capture("pub fn helper() {} fn main() {}");
    let b = f.capture("fn helper() {} fn main() {}");
    let r = a.compare(&b).unwrap();
    assert_eq!(r.public_api.status, "changed");
    assert_eq!(r.public_api.changed_symbols.len(), 1);
}
#[test]
fn semantic_verify_field_change_follows_resolved_users() {
    let f = Fixture::new();
    let source = "pub class Data { pub value: i64; } fn consume(d: Data) -> i64 { return d.value; } fn main() {}";
    let a = f.capture(source);
    let b = f.capture(&source.replacen("value: i64", "value: bool", 1));
    let r = a.compare(&b).unwrap();
    assert!(r.complete, "{r:?}");
    assert_eq!(r.public_api.status, "changed");
    assert!(
        r.affected_callers.iter().any(|s| s.contains("consume")),
        "{r:?}"
    );
}
#[test]
fn semantic_verify_unknown_is_not_evidence_backed_may() {
    let f = Fixture::new();
    let a = f.capture("fn apply(f: fn(i64) -> i64) -> i64 { return 0; } fn main() {}");
    let b = f.capture("fn apply(f: fn(i64) -> i64) -> i64 { return f(1); } fn main() {}");
    let r = a.compare(&b).unwrap();
    assert!(r.complete, "{r:?}");
    assert!(r.effects.changed.iter().any(|c| c.symbol.contains("apply") && c.after.as_ref().is_some_and(|e| e.unknown != 0)),"{r:?}");
}
#[test]
fn semantic_verify_relocated_diagnostic_is_not_new() {
    let f = Fixture::new();
    let source = "async fn main() { let n = await 1; }";
    let a = f.capture(source);
    let b = f.capture(&format!("// shifted source\n{source}"));
    let r = a.compare(&b).unwrap();
    assert!(r.complete);
    assert!(r.effects.newly_non_awaitable.is_empty(), "{r:?}");
    assert!(r.diagnostics.added.is_empty());
    assert!(r.diagnostics.removed.is_empty());
}
#[test]
fn semantic_verify_duplicate_diagnostic_count_is_retained() {
    let f = Fixture::new();
    let a = f.capture("async fn main() { let a = await 1; }");
    let b = f.capture("async fn main() { let a = await 1; let b = await 1; }");
    let r = a.compare(&b).unwrap();
    assert_eq!(r.effects.newly_non_awaitable.len(), 1, "{r:?}");
}

#[test]
fn semantic_verify_invalid_candidate_never_validates_or_applies() {
    let f = Fixture::new();
    f.capture("async fn main() {}");
    let entry = f.0.join("main.wi");
    let before = fs::read(&entry).unwrap();
    let snapshot = crate::CompilerSession::new(
        entry.to_str().unwrap(),
        "",
        &crate::CompilerOptions::debug(),
        None,
    )
    .analysis_for_edit_with_emitter(&mut Diagnostics::default())
    .unwrap();
    let function = snapshot
        .functions
        .iter()
        .find(|x| x.name == "main")
        .unwrap()
        .id
        .clone();
    let workspace = edit::Workspace::open(&f.0).unwrap();
    let preview = workspace
        .prepare(
            &entry,
            false,
            edit::Request {
                revision: snapshot.revision,
                operations: vec![edit::Operation::ReplaceBody {
                    function,
                    body: "{ let value = await 1; }".into(),
                }],
            },
            edit::ChangeFormat::Diff,
            &mut Diagnostics::default(),
        )
        .unwrap();
    let id = preview["transaction"].as_str().unwrap();
    let report = workspace.verify(id).unwrap();
    assert!(report.complete);
    assert_eq!(report.effects.newly_non_awaitable.len(), 1);
    assert!(workspace.validate(id, &mut Diagnostics::default()).is_err());
    assert!(workspace.apply(id, &mut Diagnostics::default()).is_err());
    assert_eq!(fs::read(&entry).unwrap(), before);
    assert_eq!(
        workspace.preview(id, edit::ChangeFormat::Diff).unwrap()["state"],
        "prepared"
    );
}
#[test]
fn semantic_verify_public_constant_value_change() {
    let f = Fixture::new();
    let a = f.capture("pub const LIMIT: i64 = 1; fn main() {}");
    let b = f.capture("pub const LIMIT: i64 = 2; fn main() {}");
    let r = a.compare(&b).unwrap();
    assert_eq!(r.public_api.status, "changed");
    assert_eq!(r.signatures.changed.len(), 1);
}
#[test]
fn semantic_verify_baseline_digest_rejects_modified_facts() {
    let f = Fixture::new();
    let a = f.capture(basic());
    let path = f.0.join("baseline.json");
    let mut value = serde_json::to_value(a).unwrap();
    value["complete"] = json!(false);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        Baseline::load(&path)
            .unwrap_err()
            .to_string()
            .contains("digest")
    );
}
#[test]
fn semantic_verify_public_interface_method_signature() {
    let f = Fixture::new();
    let a = f.capture("pub interface I { fn value(self) -> i64; } fn main() {}");
    let b = f.capture("pub interface I { fn value(self) -> bool; } fn main() {}");
    let r = a.compare(&b).unwrap();
    assert!(r.complete, "{r:?}");
    assert_eq!(r.public_api.status, "changed", "{r:?}");
}

#[test]
fn semantic_verify_path_separator_preserves_literal_unix_backslash() {
    let expected = if cfg!(windows) { "a/b.wi" } else { r"a\b.wi" };
    assert_eq!(relative(r"a\b.wi", "unrelated-root"), expected);
}

#[test]
fn semantic_verify_baseline_size_boundary_and_retry() {
    let f = Fixture::new();
    let baseline = f.capture(basic());
    let size = serde_json::to_vec(&baseline).unwrap().len();
    let path = f.0.join("bounded.json");
    assert!(
        baseline
            .save_with(&path, size - 1, std::io::BufWriter::new)
            .is_err()
    );
    assert!(!path.exists());
    baseline
        .save_with(&path, size, std::io::BufWriter::new)
        .unwrap();
    Baseline::load_with_limit(&path, size).unwrap();
    assert!(Baseline::load_with_limit(&path, size - 1).is_err());
    let original = fs::read(&path).unwrap();
    assert!(
        baseline
            .save_with(&path, size - 1, std::io::BufWriter::new)
            .is_err()
    );
    assert_eq!(original, fs::read(&path).unwrap());
}

#[test]
fn semantic_verify_baseline_write_and_flush_failure_cleanup() {
    struct FailingWriter {
        file: fs::File,
        flush_failure: bool,
        remaining: usize,
    }
    impl std::io::Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.remaining == 0 {
                return Err(std::io::Error::other("injected write failure"));
            }
            let n = self.file.write(&bytes[..bytes.len().min(self.remaining)])?;
            self.remaining -= n;
            Ok(n)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            if self.flush_failure {
                return Err(std::io::Error::other("injected flush failure"));
            }
            self.file.flush()
        }
    }
    let f = Fixture::new();
    let baseline = f.capture(basic());
    for flush_failure in [false, true] {
        let path = f.0.join(format!("failed-{flush_failure}.json"));
        let error = baseline
            .save_with(&path, BASELINE_SIZE_LIMIT, |file| FailingWriter {
                file,
                flush_failure,
                remaining: if flush_failure { usize::MAX } else { 17 },
            })
            .unwrap_err();
        assert!(error.to_string().contains(if flush_failure {
            "flush failure"
        } else {
            "write failure"
        }));
        assert!(!path.exists());
        baseline.save(&path).unwrap();
        Baseline::load(&path).unwrap();
    }
}

#[test]
fn semantic_verify_baseline_rejects_oversized_sparse_file() {
    let f = Fixture::new();
    let path = f.0.join("oversized.json");
    let file = fs::File::create(&path).unwrap();
    file.set_len(BASELINE_SIZE_LIMIT as u64 + 1).unwrap();
    drop(file);
    assert!(
        Baseline::load(&path)
            .unwrap_err()
            .to_string()
            .contains("size limit")
    );
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        BASELINE_SIZE_LIMIT as u64 + 1
    );
}

fn manifest(before: &Baseline, path: &str, old: Option<&str>, new: Option<&str>) -> EditManifest {
    EditManifest {
        schema_version: 1,
        baseline_revision: before.revision().into(),
        complete: true,
        changes: vec![InputChange {
            path: path.into(),
            before: old.map(hash),
            after: new.map(hash),
        }],
    }
}
#[test]
fn semantic_verify_manifest_tree_edit_and_stale_hashes() {
    let f = Fixture::new();
    let baseline = f.capture(basic());
    let changed = basic().replace("return 1", "return 2");
    fs::write(f.0.join("main.wi"), &changed).unwrap();
    let edits = manifest(&baseline, "main.wi", Some(basic()), Some(&changed));
    let report = baseline
        .verify_manifest(&f.0.join("main.wi"), None, &edits)
        .unwrap();
    assert!(report.complete);
    assert_eq!(report.work.compared_symbols, 1);
    let mut stale = edits.clone();
    stale.baseline_revision = hash("another revision");
    assert!(stale.check(&baseline, &f.0).is_err());
    stale = edits.clone();
    stale.changes[0].before = Some(hash("wrong before"));
    assert!(stale.check(&baseline, &f.0).is_err());
    stale = edits;
    stale.changes[0].after = Some(hash("wrong after"));
    assert!(stale.check(&baseline, &f.0).is_err());
}
#[test]
fn semantic_verify_manifest_rejects_invalid_contracts() {
    let f = Fixture::new();
    let baseline = f.capture(basic());
    let valid = manifest(&baseline, "main.wi", Some(basic()), Some("fn main() {}"));
    for path in [
        "",
        "../main.wi",
        "/main.wi",
        "a/../main.wi",
        "./main.wi",
        "a//main.wi",
        "a/",
        "a\\main.wi",
    ] {
        let mut invalid = valid.clone();
        invalid.changes[0].path = path.into();
        assert!(invalid.validate().is_err(), "{path}");
    }
    let mut invalid = valid.clone();
    invalid.complete = false;
    assert!(invalid.validate().is_err());
    invalid = valid.clone();
    invalid.schema_version = 2;
    assert!(invalid.validate().is_err());
    invalid = valid.clone();
    invalid.changes.push(invalid.changes[0].clone());
    assert!(invalid.validate().is_err());
    invalid = valid.clone();
    invalid.changes[0].after = invalid.changes[0].before.clone();
    assert!(invalid.validate().is_err());
    invalid = valid;
    invalid.changes[0].before = Some("not a digest".into());
    assert!(invalid.validate().is_err());
}
#[test]
fn semantic_verify_manifest_added_removed_inputs_and_json() {
    let f = Fixture::new();
    let baseline = f.capture(basic());
    fs::write(f.0.join("extra.wi"), "fn extra() {}").unwrap();
    let added = manifest(&baseline, "extra.wi", None, Some("fn extra() {}"));
    added.check(&baseline, &f.0).unwrap();
    let path = f.0.join("manifest.json");
    fs::write(&path, serde_json::to_vec(&added).unwrap()).unwrap();
    EditManifest::load(&path)
        .unwrap()
        .check(&baseline, &f.0)
        .unwrap();
    let mut value = serde_json::to_value(&added).unwrap();
    value["extra"] = json!(true);
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(EditManifest::load(&path).is_err());
    fs::remove_file(f.0.join("main.wi")).unwrap();
    manifest(&baseline, "main.wi", Some(basic()), None)
        .check(&baseline, &f.0)
        .unwrap();
}
#[test]
fn semantic_verify_manifest_syntax_failure_can_recover() {
    let f = Fixture::new();
    let invalid = "fn main( {";
    let baseline = f.capture(invalid);
    fs::write(f.0.join("main.wi"), basic()).unwrap();
    let edits = manifest(&baseline, "main.wi", Some(invalid), Some(basic()));
    baseline
        .verify_manifest(&f.0.join("main.wi"), None, &edits)
        .unwrap();
}

#[test]
fn semantic_verify_overlay_import_and_restore() {
    let f = Fixture::new();
    let original = "pub fn value() -> i64 { return 1; }";
    fs::write(f.0.join("lib.wi"), original).unwrap();
    let baseline = f.capture("import lib; fn main() { let n = lib::value(); }");
    let entry = fs::canonicalize(f.0.join("main.wi")).unwrap();
    let library = fs::canonicalize(f.0.join("lib.wi")).unwrap();
    let overlay = BTreeMap::from([(
        library.clone(),
        "pub fn value() -> bool { return true; }".into(),
    )]);
    let candidate = Baseline::capture_overlay(&entry, None, overlay, None).unwrap();
    let report = baseline.compare(&candidate).unwrap();
    assert!(report.complete, "{report:?}");
    assert!(
        report
            .signatures
            .changed
            .iter()
            .any(|c| c.symbol.contains("value"))
    );
    assert_eq!(fs::read_to_string(&library).unwrap(), original);
    fs::write(&library, "pub fn value() -> bool { return true; }").unwrap();
    let physical_candidate = Baseline::capture(&entry, None).unwrap();
    assert_eq!(candidate.revision(), physical_candidate.revision());
    fs::write(&library, original).unwrap();
    let restored = Baseline::capture(&entry, None).unwrap();
    assert_eq!(baseline.revision(), restored.revision());
    let invalid = Baseline::capture_overlay(
        &entry,
        None,
        BTreeMap::from([(entry.clone(), "fn main( {".into())]),
        None,
    )
    .unwrap();
    assert!(!invalid.is_complete());
    assert_eq!(invalid.inputs["main.wi"], hash("fn main( {"));
    assert_eq!(
        baseline.revision(),
        Baseline::capture(&entry, None).unwrap().revision()
    );
}

#[test]
fn semantic_verify_overlay_nested_and_thread_isolation() {
    use crate::compiler_db::captured::{FileInput, SourceOverlay};
    let f = Fixture::new();
    fs::write(f.0.join("main.wi"), "disk").unwrap();
    let path = fs::canonicalize(f.0.join("main.wi")).unwrap();
    let outer = SourceOverlay::enter(BTreeMap::from([(path.clone(), "outer".into())]));
    assert_eq!(FileInput::read(path.clone()).unwrap().source, "outer");
    {
        let _inner = SourceOverlay::enter(BTreeMap::from([(path.clone(), "inner".into())]));
        assert_eq!(FileInput::read(path.clone()).unwrap().source, "inner");
        let other = path.clone();
        assert_eq!(
            std::thread::spawn(move || FileInput::read(other).unwrap().source)
                .join()
                .unwrap(),
            "disk"
        );
    }
    assert_eq!(FileInput::read(path.clone()).unwrap().source, "outer");
    drop(outer);
    assert_eq!(FileInput::read(path).unwrap().source, "disk");
}

#[test]
fn semantic_verify_transaction_keeps_prepared_project_inventory() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("src")).unwrap();
    fs::write(
        f.0.join("project.toml"),
        "[project]\nname='demo'\nversion='1.0.0'\n",
    )
    .unwrap();
    let entry = f.0.join("src/main.wi");
    fs::write(&entry, basic()).unwrap();
    let snapshot = crate::CompilerSession::new(
        entry.to_str().unwrap(),
        "",
        &crate::CompilerOptions::debug(),
        Some(f.0.clone()),
    )
    .analysis_for_edit_with_emitter(&mut Diagnostics::default())
    .unwrap();
    let function = snapshot
        .functions
        .iter()
        .find(|function| function.name == "value")
        .unwrap()
        .id
        .clone();
    let workspace = edit::Workspace::open(&f.0).unwrap();
    let prepared = workspace
        .prepare(
            &entry,
            true,
            edit::Request {
                revision: snapshot.revision,
                operations: vec![edit::Operation::ReplaceBody {
                    function,
                    body: "{ return 7; }".into(),
                }],
            },
            edit::ChangeFormat::Diff,
            &mut Diagnostics::default(),
        )
        .unwrap();
    let id = prepared["transaction"].as_str().unwrap();
    let before = serde_json::to_value(workspace.verify(id).unwrap()).unwrap();
    let manifest: EditManifest =
        serde_json::from_value(prepared["verification_manifest"].clone()).unwrap();
    assert_eq!(
        before,
        serde_json::to_value(workspace.verify_manifest(id, &manifest).unwrap()).unwrap()
    );
    // Unlisted candidate additions are covered by caller attestation. The
    // legacy integrity path still rejects them, and verification preserves them.
    let unlisted = f.0.join(".willow-edits").join(id).join("src/unlisted.wi");
    fs::write(&unlisted, "fn extra() {}").unwrap();
    assert!(workspace.verify(id).is_err());
    assert_eq!(
        before,
        serde_json::to_value(workspace.verify_manifest(id, &manifest).unwrap()).unwrap()
    );
    assert_eq!(fs::read_to_string(&unlisted).unwrap(), "fn extra() {}");
    fs::remove_file(unlisted).unwrap();
    let extra = f.0.join("src/extra.wi");
    fs::write(&extra, "pub fn extra() -> i64 { return 3; }").unwrap();
    let after = serde_json::to_value(workspace.verify(id).unwrap()).unwrap();
    assert_eq!(before, after);
    assert_eq!(
        fs::read_to_string(extra).unwrap(),
        "pub fn extra() -> i64 { return 3; }"
    );
}

#[test]
fn semantic_verify_overlay_cannot_import_unlisted_original_source() {
    let f = Fixture::new();
    f.capture("fn main() {}");
    let root = fs::canonicalize(&f.0).unwrap();
    let entry = root.join("main.wi");
    fs::write(root.join("extra.wi"), "pub fn extra() {}").unwrap();
    let candidate = Baseline::capture_overlay(
        &entry,
        None,
        BTreeMap::from([(
            entry.clone(),
            "import extra; fn main() { extra::extra(); }".into(),
        )]),
        Some(crate::compiler_db::captured::OverlayInventory {
            root,
            files: std::collections::HashSet::from([entry.clone()]),
            modules: BTreeMap::new(),
        }),
    )
    .unwrap();
    assert!(candidate.diagnostics.iter().any(|d| d.severity == "error"));
}

#[test]
fn semantic_verify_overlay_preserves_module_precedence() {
    for project in [false, true] {
        let f = Fixture::new();
        let source_root = if project {
            fs::write(
                f.0.join("project.toml"),
                "[project]\nname='demo'\nversion='1.0.0'\n",
            )
            .unwrap();
            f.0.join("src")
        } else {
            f.0.clone()
        };
        fs::create_dir_all(source_root.join("lib")).unwrap();
        fs::write(
            source_root.join("lib/mod.wi"),
            "pub fn value() -> i64 { return 1; }",
        )
        .unwrap();
        let entry = source_root.join("main.wi");
        fs::write(&entry, "import lib; fn main() { let n = lib::value(); }").unwrap();
        let snapshot = crate::CompilerSession::new(
            entry.to_str().unwrap(),
            "",
            &crate::CompilerOptions::debug(),
            project.then(|| f.0.clone()),
        )
        .analysis_for_edit_with_emitter(&mut Diagnostics::default())
        .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|function| function.name == "main")
            .unwrap()
            .id
            .clone();
        let workspace = edit::Workspace::open(&f.0).unwrap();
        let prepared = workspace
            .prepare(
                &entry,
                project,
                edit::Request {
                    revision: snapshot.revision,
                    operations: vec![edit::Operation::ReplaceBody {
                        function,
                        body: "{ let n = lib::value(); let x = 2; }".into(),
                    }],
                },
                edit::ChangeFormat::Diff,
                &mut Diagnostics::default(),
            )
            .unwrap();
        let id = prepared["transaction"].as_str().unwrap();
        let before = serde_json::to_value(workspace.verify(id).unwrap()).unwrap();
        fs::write(source_root.join("lib.wi"), "invalid !!!").unwrap();
        let after = serde_json::to_value(workspace.verify(id).unwrap()).unwrap();
        assert_eq!(before, after, "project={project}");
    }
}

#[test]
fn semantic_verify_retained_matches_clean_across_edits_and_topology() {
    for count in [8, 32, 128] {
        let f = Fixture::new();
        let unrelated = (0..count)
            .map(|i| format!("fn unrelated{i}() -> i64 {{ return {i}; }}\n"))
            .collect::<String>();
        let source = format!(
            "pub fn value() -> i64 {{ return 1; }} fn main() {{ println(value()); }}\n{unrelated}"
        );
        let entry = f.0.join("main.wi");
        fs::write(&entry, &source).unwrap();
        let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
        let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        for (case, candidate) in [
            ("body", source.replacen("return 1;", "return 2;", 1)),
            (
                "signature",
                source.replacen("-> i64 { return 1; }", "-> bool { return true; }", 1),
            ),
            (
                "effect",
                source.replacen("return 1;", "println(1); return 1;", 1),
            ),
            ("add_file", format!("import extra;\n{source}")),
            ("remove_file", source.clone()),
            (
                "remove_declaration",
                source
                    .replace("pub fn value() -> i64 { return 1; }", "")
                    .replace("println(value());", ""),
            ),
        ] {
            let extra = f.0.join("extra.wi");
            if case == "add_file" {
                fs::write(&extra, "pub fn extra() -> i64 { return 42; }").unwrap();
            }
            if case == "remove_file" {
                fs::remove_file(&extra).unwrap();
            }
            fs::write(&entry, &candidate).unwrap();
            let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            let clean = Baseline::capture(&entry, None).unwrap();
            assert_eq!(
                serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
                "{case} {count}"
            );
            let repeat = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            assert_eq!(
                serde_json::to_value(baseline.compare(&repeat).unwrap()).unwrap(),
                serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap()
            );
            assert_eq!(
                retained.incremental_stats["concurrency_checks"], 0,
                "{case}"
            );
            assert_eq!(retained.incremental_stats["signature_checks"], 0, "{case}");
            assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
        }
    }
}

#[test]
fn semantic_verify_retained_concurrency_diagnostic_relocation_matches_clean() {
    let f = Fixture::new();
    let entry = f.0.join("main.wi");
    let source = "async fn borrow(x: &mut fn(i64) -> i64) {} fn main() {}";
    let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
    fs::write(&entry, source).unwrap();
    let before = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
    assert!(!before.diagnostics().is_empty());
    for prefix in ["\n", "\n\n\n", ""] {
        fs::write(&entry, format!("{prefix}{source}")).unwrap();
        let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        let clean = Baseline::capture(&entry, None).unwrap();
        assert_eq!(
            serde_json::to_value(before.compare(&warm).unwrap()).unwrap(),
            serde_json::to_value(before.compare(&clean).unwrap()).unwrap()
        );
        assert_eq!(retained.incremental_stats["concurrency_checks"], 0);
        assert_eq!(retained.incremental_stats["signature_checks"], 0);
    }
}

#[test]
fn semantic_verify_retained_named_and_generic_signatures_both_workflows() {
    for count in [8, 32, 128] {
        for (declaration, ty) in [
            ("class Box {}", "Box"),
            ("class Box<T> { value: T; }", "Box<i64>"),
            ("enum Box<T> { Value(T) }", "Box<i64>"),
            ("enum Box<T> { Value(T) }", "fn(Box<i64>) -> Box<i64>"),
        ] {
            let f = Fixture::new();
            let entry = f.0.join("main.wi");
            let unrelated = (0..count)
                .map(|i| format!("fn unrelated{i}(v: {ty}) -> {ty} {{ return v; }}\n"))
                .collect::<String>();
            let source = format!("{} {declaration}\n{unrelated}", basic());
            fs::write(&entry, &source).unwrap();
            let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
            let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            let snapshot = crate::CompilerSession::new(
                entry.to_str().unwrap(),
                "",
                &crate::CompilerOptions::debug(),
                None,
            )
            .analysis_for_edit_with_emitter(&mut Diagnostics::default())
            .unwrap();
            let function = snapshot
                .functions
                .iter()
                .find(|f| f.name == "value")
                .unwrap()
                .id
                .clone();
            let workspace = edit::Workspace::open(&f.0).unwrap();
            let preview = workspace
                .prepare(
                    &entry,
                    false,
                    edit::Request {
                        revision: snapshot.revision,
                        operations: vec![edit::Operation::ReplaceBody {
                            function,
                            body: "{ return 7; }".into(),
                        }],
                    },
                    edit::ChangeFormat::Diff,
                    &mut Diagnostics::default(),
                )
                .unwrap();
            let id = preview["transaction"].as_str().unwrap();
            let manifest =
                serde_json::from_value(preview["verification_manifest"].clone()).unwrap();
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let clean = workspace
                .verify_manifest_retained(id, &manifest, &mut clean_state)
                .unwrap();
            for _ in 0..3 {
                let warm = workspace
                    .verify_manifest_retained(id, &manifest, &mut retained)
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(warm).unwrap(),
                    serde_json::to_value(&clean).unwrap()
                );
                assert_eq!(
                    retained.incremental_stats["signature_checks"], 0,
                    "transaction {ty} {count}"
                );
                assert_eq!(fs::read_to_string(&entry).unwrap(), source);
            }
            // A signature edit invalidates one signature, then moving every
            // declaration must remap the cached results without rechecking them.
            for (expected, changed) in [
                (0, source.replacen("return 1;", "return 2;", 1)),
                (
                    1,
                    source.replacen("-> i64 { return 1; }", "-> bool { return true; }", 1),
                ),
                (1, format!("\n\n{source}")),
                (0, format!("\n\n\n{source}")),
            ] {
                fs::write(&entry, &changed).unwrap();
                let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
                let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
                let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
                assert_eq!(
                    serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                    serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
                    "tree {ty} {count}"
                );
                assert_eq!(
                    retained.incremental_stats["signature_checks"], expected,
                    "tree {ty} {count}"
                );
                assert_eq!(fs::read_to_string(&entry).unwrap(), changed);
            }
        }
    }
}

#[test]
fn semantic_verify_signature_resolution_dependencies_and_collection_replay() {
    for (declarations, annotation) in [
        ("class Box {}", "Box"),
        ("enum Box<T> { Value(T) }", "Box<i64>"),
        ("", "std::collections::Map<i64, i64>"),
        ("", "std::collections::Array<i64>"),
    ] {
        let fixture = Fixture::new();
        let entry = fixture.0.join("main.wi");
        let first = format!("fn first(v: {annotation}) -> {annotation} {{ return v; }}\n");
        let second = first.replace("first", "second");
        let tail = "fn uses_array(v: Array<i64>) -> Array<i64> { return v; } fn main() {}";
        let source = format!("{declarations}\n{first}{second}{tail}");
        let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
        fs::write(&entry, &source).unwrap();
        let before = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        for candidate in [
            source.replace(&first, ""),
            format!("{declarations}\n{second}{first}{tail}"),
            source
                .replace("class Box {}", "enum Box { Value }")
                .replace(
                    "enum Box<T> { Value(T) }",
                    "enum Box<T> { Value(T), Empty }",
                ),
            source.clone(),
        ] {
            fs::write(&entry, &candidate).unwrap();
            let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
            assert_eq!(
                serde_json::to_value(before.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(before.compare(&clean).unwrap()).unwrap(),
                "{annotation} {candidate}"
            );
        }
    }
}

#[test]
fn semantic_verify_signature_import_invalidation_is_affected_only() {
    for count in [8, 32, 128] {
        let f = Fixture::new();
        let entry = f.0.join("main.wi");
        let lib = f.0.join("lib.wi");
        let unrelated = (0..count)
            .map(|i| format!("fn unrelated{i}(v: Stable) -> Stable {{ return v; }}\n"))
            .collect::<String>();
        let source = format!(
            "import lib::Box as Alias; class Stable {{}} fn dependent(v: Alias<i64>) -> Alias<i64> {{ return v; }} fn main() {{}}\n{unrelated}"
        );
        let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
        fs::write(&entry, &source).unwrap();
        fs::write(&lib, "pub enum Box<T> { Value(T) }").unwrap();
        let before = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        for library in [
            "pub enum Box<T> { Value(T), Empty }",
            "pub enum Box<T> { Value(T) } pub enum Unused { Empty }",
            "pub enum Box<T> { Value(T) } pub enum Unused { Empty, Other }",
        ] {
            fs::write(&lib, library).unwrap();
            let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            let expected = usize::from(!library.contains("Other"));
            assert_eq!(
                retained.incremental_stats["signature_checks"], expected,
                "{count} {library}"
            );
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
            assert_eq!(
                serde_json::to_value(before.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(before.compare(&clean).unwrap()).unwrap()
            );
            assert_eq!(fs::read_to_string(&lib).unwrap(), library);
            assert_eq!(fs::read_to_string(&entry).unwrap(), source);
        }
    }
}

#[test]
fn semantic_verify_signature_mixed_collection_spelling_uses_entry_state() {
    for params in [
        "a: std::collections::Array<i64>, b: Array<i64>",
        "a: Array<i64>, b: std::collections::Array<i64>",
        "a: Array<i64>, b: Array<i64>",
    ] {
        let f = Fixture::new();
        let entry = f.0.join("main.wi");
        let source = format!("fn mixed({params}) {{}} fn main() {{}}");
        fs::write(&entry, &source).unwrap();
        let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
        let before = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        for offset in 1..=3 {
            fs::write(&entry, format!("{}{source}", "\n".repeat(offset))).unwrap();
            let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            assert_eq!(
                retained.incremental_stats["signature_checks"], 0,
                "{params}"
            );
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
            assert_eq!(
                serde_json::to_value(before.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(before.compare(&clean).unwrap()).unwrap()
            );
        }
    }
}

#[test]
fn semantic_verify_precheck_skips_unrelated_functions() {
    for count in [8, 32, 128] {
        for imported in [false, true] {
            let f = Fixture::new();
            let entry = f.0.join("main.wi");
            let edited = if imported {
                f.0.join("lib.wi")
            } else {
                entry.clone()
            };
            let source = format!("class Box<T> {{ value: T; }} {}", basic())
                + &(0..count)
                    .map(|i| format!("fn unrelated{i}() -> i64 {{ return {i}; }}\n"))
                    .collect::<String>();
            let entry_source = if imported {
                "import lib; fn main() { let n = lib::value(); }".to_owned()
            } else {
                source.clone()
            };
            fs::write(&entry, &entry_source).unwrap();
            fs::write(&edited, &source).unwrap();
            let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
            let before = Baseline::capture_retained(&entry, None, &mut revision).unwrap();
            let snapshot = crate::CompilerSession::new(
                entry.to_str().unwrap(),
                "",
                &crate::CompilerOptions::debug(),
                None,
            )
            .analysis_for_edit_with_emitter(&mut Diagnostics::default())
            .unwrap();
            let function = snapshot
                .functions
                .iter()
                .find(|f| f.name == "value")
                .unwrap()
                .id
                .clone();
            let workspace = edit::Workspace::open(&f.0).unwrap();
            let preview = workspace
                .prepare(
                    &entry,
                    false,
                    edit::Request {
                        revision: snapshot.revision,
                        operations: vec![edit::Operation::ReplaceBody {
                            function,
                            body: "{ return 7; }".into(),
                        }],
                    },
                    edit::ChangeFormat::Diff,
                    &mut Diagnostics::default(),
                )
                .unwrap();
            let id = preview["transaction"].as_str().unwrap();
            let manifest =
                serde_json::from_value(preview["verification_manifest"].clone()).unwrap();
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let clean = workspace
                .verify_manifest_retained(id, &manifest, &mut clean_state)
                .unwrap();
            for repeat in 0..3 {
                let warm = workspace
                    .verify_manifest_retained(id, &manifest, &mut revision)
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(warm).unwrap(),
                    serde_json::to_value(&clean).unwrap(),
                    "transaction imported={imported} count={count} repeat={repeat}"
                );
                // Repeated verification reuses the retained candidate state.
                assert_eq!(
                    revision.incremental_stats["precheck_bodies"],
                    usize::from(repeat == 0),
                    "transaction imported={imported} count={count} repeat={repeat}: {}",
                    revision.incremental_stats
                );
                assert_eq!(revision.incremental_stats["precheck_signatures"], 0);
                assert_eq!(revision.incremental_stats["precheck_templates"], 0);
                assert_eq!(fs::read_to_string(&entry).unwrap(), entry_source);
                assert_eq!(fs::read_to_string(&edited).unwrap(), source);
            }
            let changed = source.replacen("return 1;", "return 2;", 1);
            fs::write(&edited, &changed).unwrap();
            for repeat in 0..3 {
                let warm = Baseline::capture_retained(&entry, None, &mut revision).unwrap();
                assert_eq!(
                    revision.incremental_stats["precheck_bodies"],
                    usize::from(repeat == 0),
                    "tree imported={imported} count={count} repeat={repeat}: {}",
                    revision.incremental_stats
                );
                assert_eq!(revision.incremental_stats["precheck_signatures"], 0);
                assert_eq!(revision.incremental_stats["precheck_templates"], 0);
                let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
                let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
                assert_eq!(
                    serde_json::to_value(before.compare(&warm).unwrap()).unwrap(),
                    serde_json::to_value(before.compare(&clean).unwrap()).unwrap()
                );
                assert_eq!(fs::read_to_string(&edited).unwrap(), changed);
                if imported {
                    assert_eq!(fs::read_to_string(&entry).unwrap(), entry_source);
                }
            }
        }
    }
}

#[test]
fn semantic_verify_precheck_generic_dependencies_match_clean() {
    let f = Fixture::new();
    let entry = f.0.join("main.wi");
    let template = "fn identity<T>(x: T) -> T { return x; }";
    let first = "fn first() -> i64 { return identity(1); }";
    let second = "fn second() -> i64 { return identity(2); }";
    let stable = "fn stable() -> i64 { return 3; } fn main() {}";
    let source = format!("{template} {first} {second} {stable}");
    fs::write(&entry, &source).unwrap();
    let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
    let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
    for (case, candidate) in [
        ("body", source.replacen("return 3;", "return 4;", 1)),
        (
            "remove_first_requester",
            format!("{template} {second} {stable}"),
        ),
        (
            "template_body",
            format!(
                "{} {second} {stable}",
                template.replace("return x;", "let copy = x; return copy;")
            ),
        ),
        (
            "negative_lookup",
            format!(
                "{} {second} {stable}",
                template.replace("return x;", "missing(); return x;")
            ),
        ),
        (
            "add_dependency",
            format!(
                "{} {second} {stable} fn missing() {{}}",
                template.replace("return x;", "missing(); return x;")
            ),
        ),
        (
            "dependency_effect",
            format!(
                "{} {second} {stable} fn missing() {{ println(1); }}",
                template.replace("return x;", "missing(); return x;")
            ),
        ),
        (
            "remove_dependency",
            format!(
                "{} {second} {stable}",
                template.replace("return x;", "missing(); return x;")
            ),
        ),
        ("remove_template", format!("{second} {stable}")),
        ("restore", source.clone()),
    ] {
        fs::write(&entry, &candidate).unwrap();
        let warm = Baseline::capture_retained(&entry, None, &mut retained)
            .unwrap_or_else(|error| panic!("{case}: {error:#}"));
        let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
        let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
        assert_eq!(
            serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
            serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
            "{case}"
        );
        if case == "body" {
            assert_eq!(
                retained.incremental_stats["precheck_bodies"], 1,
                "{}",
                retained.incremental_stats
            );
            assert_eq!(retained.incremental_stats["precheck_signatures"], 0);
            assert_eq!(retained.incremental_stats["precheck_templates"], 0);
            assert_eq!(retained.incremental_stats["precheck_instances"], 0);
        }
        assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
    }
}

#[test]
fn semantic_verify_precheck_imported_template_changes_match_clean() {
    let f = Fixture::new();
    let entry = f.0.join("main.wi");
    let library = f.0.join("lib.wi");
    let entry_source = "import lib; fn one() -> i64 { return lib::identity(1); } fn main() {}";
    let source = "pub fn identity<T>(x: T) -> T { return x; } fn unrelated() -> i64 { return 7; }";
    fs::write(&entry, entry_source).unwrap();
    fs::write(&library, source).unwrap();
    let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
    let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
    for (case, candidate) in [
        ("relocation", format!("\n\n{source}")),
        (
            "template_body",
            source.replace("return x;", "let copy = x; return copy;"),
        ),
        (
            "negative_lookup",
            source.replace("return x;", "missing(); return x;"),
        ),
        (
            "add_dependency",
            format!(
                "{} fn missing() {{}}",
                source.replace("return x;", "missing(); return x;")
            ),
        ),
        (
            "change_dependency",
            format!(
                "{} fn missing() {{ println(1); }}",
                source.replace("return x;", "missing(); return x;")
            ),
        ),
        (
            "remove_template",
            "fn unrelated() -> i64 { return 7; }".to_owned(),
        ),
        ("restore", source.to_owned()),
        ("relocate_restored", format!("\n\n\n{source}")),
    ] {
        fs::write(&library, &candidate).unwrap();
        let warm = Baseline::capture_retained(&entry, None, &mut retained)
            .unwrap_or_else(|error| panic!("{case}: {error:#}"));
        let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
        let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
        assert_eq!(
            serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
            serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
            "{case}"
        );
        if case == "relocation" || case == "relocate_restored" {
            for counter in [
                "precheck_bodies",
                "precheck_signatures",
                "precheck_templates",
                "precheck_instances",
            ] {
                assert_eq!(
                    retained.incremental_stats[counter], 0,
                    "{case}: {counter}: {}",
                    retained.incremental_stats
                );
            }
        }
        assert_eq!(fs::read_to_string(&library).unwrap(), candidate);
        assert_eq!(fs::read_to_string(&entry).unwrap(), entry_source);
    }
}

#[test]
fn semantic_verify_precheck_unrelated_import_changes_skip_semantics() {
    verify_unrelated_import_changes_skip_semantics(false);
}

#[test]
fn semantic_verify_precheck_declaration_scaling_both_workflows() {
    for count in [8, 32, 128] {
        let f = Fixture::new();
        let entry = f.0.join("main.wi");
        let source = format!("class Box<T> {{ value: T; }} {}\n", basic())
            + &(0..count).map(|i| format!("class C{i} {{ value: i64; }} enum E{i} {{ Value(i64) }} interface I{i} {{ fn get(self) -> i64; }}\n")).collect::<String>();
        fs::write(&entry, &source).unwrap();
        let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
        let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        let snapshot = crate::CompilerSession::new(
            entry.to_str().unwrap(),
            "",
            &crate::CompilerOptions::debug(),
            None,
        )
        .analysis_for_edit_with_emitter(&mut Diagnostics::default())
        .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "value")
            .unwrap()
            .id
            .clone();
        let workspace = edit::Workspace::open(&f.0).unwrap();
        let preview = workspace
            .prepare(
                &entry,
                false,
                edit::Request {
                    revision: snapshot.revision,
                    operations: vec![edit::Operation::ReplaceBody {
                        function,
                        body: "{ return 7; }".into(),
                    }],
                },
                edit::ChangeFormat::Diff,
                &mut Diagnostics::default(),
            )
            .unwrap();
        let id = preview["transaction"].as_str().unwrap();
        let manifest = serde_json::from_value(preview["verification_manifest"].clone()).unwrap();
        let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
        let clean = workspace
            .verify_manifest_retained(id, &manifest, &mut clean_state)
            .unwrap();
        for repeat in 0..2 {
            let warm = workspace
                .verify_manifest_retained(id, &manifest, &mut retained)
                .unwrap();
            assert_eq!(
                serde_json::to_value(warm).unwrap(),
                serde_json::to_value(&clean).unwrap()
            );
            assert_eq!(
                retained.incremental_stats["precheck_declarations"], 0,
                "transaction count={count} repeat={repeat}: {}",
                retained.incremental_stats
            );
            assert_eq!(
                retained.incremental_stats["declaration_validations"], 0,
                "count={count} repeat={repeat}: {}",
                retained.incremental_stats
            );
            assert_eq!(
                retained.incremental_stats["precheck_bodies"],
                usize::from(repeat == 0)
            );
            assert_eq!(fs::read_to_string(&entry).unwrap(), source);
        }
        let changed = source.replacen("return 1;", "return 2;", 1);
        fs::write(&entry, &changed).unwrap();
        for repeat in 0..2 {
            let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
            assert_eq!(
                serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap()
            );
            assert_eq!(
                retained.incremental_stats["precheck_declarations"], 0,
                "tree count={count} repeat={repeat}: {}",
                retained.incremental_stats
            );
            assert_eq!(
                retained.incremental_stats["declaration_validations"], 0,
                "count={count} repeat={repeat}: {}",
                retained.incremental_stats
            );
            assert_eq!(
                retained.incremental_stats["precheck_bodies"],
                usize::from(repeat == 0)
            );
            assert_eq!(fs::read_to_string(&entry).unwrap(), changed);
        }
        let mut candidate = changed;
        for (case, before, after) in [
            (
                "class",
                "class C0 { value: i64; }",
                "class C0 { value: bool; }",
            ),
            ("enum", "enum E0 { Value(i64) }", "enum E0 { Value(bool) }"),
            (
                "interface",
                "interface I0 { fn get(self) -> i64; }",
                "interface I0 { fn get(self) -> bool; }",
            ),
            (
                "diagnostic",
                "interface I0 { fn get(self) -> bool; }",
                "interface I0 { fn get(self) -> bool; fn get(self) -> i64; }",
            ),
            ("diagnostic_replay", "", "\n"),
            (
                "repair",
                "interface I0 { fn get(self) -> bool; fn get(self) -> i64; }",
                "interface I0 { fn get(self) -> bool; }",
            ),
        ] {
            candidate = candidate.replacen(before, after, 1);
            fs::write(&entry, &candidate).unwrap();
            let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
            assert_eq!(
                serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
                "{case} count={count}"
            );
            assert_eq!(
                retained.incremental_stats["precheck_declarations"],
                if case == "diagnostic_replay" { 0 } else { 2 },
                "{case} count={count}: {}",
                retained.incremental_stats
            );
            if case.starts_with("diagnostic") {
                assert!(warm.diagnostics.iter().any(|d| d.severity == "error"));
            }
            assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
        }
    }
}

#[test]
fn semantic_verify_precheck_import_resolution_matches_clean() {
    #[derive(Default)]
    struct RawDiagnostics {
        raw: Vec<serde_json::Value>,
        facts: Diagnostics,
    }
    impl crate::diagnostics::DiagnosticEmitter for RawDiagnostics {
        fn emit(
            &mut self,
            diagnostic: &crate::diagnostics::Diagnostic,
            sources: &dyn crate::diagnostics::source_map::SourceLookup,
        ) -> std::io::Result<()> {
            self.raw.push(serde_json::to_value(diagnostic).unwrap());
            self.facts.emit(diagnostic, sources)
        }
    }
    fn capture(
        entry: &std::path::Path,
        retained: &mut crate::compiler_db::revision::AnalysisRevision,
    ) -> (Baseline, Vec<serde_json::Value>) {
        let mut diagnostics = RawDiagnostics::default();
        let snapshot = retained
            .analyze_verification(
                crate::CompilerSession::new(
                    entry.to_str().unwrap(),
                    "",
                    &crate::CompilerOptions::debug(),
                    None,
                ),
                &mut diagnostics,
            )
            .unwrap();
        (
            Baseline::from_snapshot(&snapshot, diagnostics.facts.facts).unwrap(),
            diagnostics.raw,
        )
    }
    let unrelated = (0..16)
        .map(|i| format!("fn unrelated{i}() -> i64 {{ return {i}; }}\n"))
        .collect::<String>();
    for (shape, annotation, initial, changes) in [
        (
            "alias",
            "Alias",
            "",
            vec![
                ("add", "import types::Payload as Alias;", Some((1, 0))),
                ("retarget", "import types::Choice as Alias;", Some((1, 1))),
                ("remove", "", Some((1, 1))),
                ("restore", "import types::Payload as Alias;", Some((1, 0))),
            ],
        ),
        (
            "collections",
            "Items<i64>",
            "import std::collections::Array as Items;",
            vec![
                (
                    "unrelated",
                    "import std::collections::Array as Items; import std::time;",
                    Some((0, 0)),
                ),
                (
                    "retarget",
                    "import std::collections::Map as Items;",
                    Some((1, 1)),
                ),
                (
                    "restore",
                    "import std::collections::Array as Items;",
                    Some((1, 1)),
                ),
            ],
        ),
        (
            "hint",
            "Paylod",
            "import types;",
            vec![
                ("nearby", "import types::Payload;", None),
                ("different", "import types::Choice;", None),
                ("restore", "import types::Payload;", None),
            ],
        ),
    ] {
        let f = Fixture::new();
        let entry = f.0.join("main.wi");
        let library = f.0.join("types.wi");
        let library_source = "pub class Payload { pub value: i64; } pub enum Choice { Value(i64) }";
        fs::write(&library, library_source).unwrap();
        let body = format!(
            "class Gate<T> {{ value: T; }} fn dependent(v: {annotation}) -> {annotation} {{ return v; }} fn main() {{}}\n{unrelated}"
        );
        fs::write(&entry, format!("{initial}\n{body}")).unwrap();
        let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
        let (baseline, _) = capture(&entry, &mut retained);
        for (case, imports, expected) in changes {
            let candidate = format!("{imports}\n{body}");
            fs::write(&entry, &candidate).unwrap();
            let (warm, warm_diagnostics) = capture(&entry, &mut retained);
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let (clean, clean_diagnostics) = capture(&entry, &mut clean_state);
            assert_eq!(
                serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
                "{shape} {case}"
            );
            assert_eq!(
                warm_diagnostics, clean_diagnostics,
                "full diagnostics {shape} {case}"
            );
            for counter in ["precheck_bodies", "precheck_signatures"] {
                let count = retained.incremental_stats[counter].as_u64().unwrap();
                if let Some((bodies, signatures)) = expected {
                    let expected = if counter == "precheck_bodies" {
                        bodies
                    } else {
                        signatures
                    };
                    assert_eq!(
                        count, expected,
                        "{shape} {case} {counter}: {}",
                        retained.incremental_stats
                    );
                } else {
                    // Only the invalid dependent signature/body can observe a
                    // change in available type-name suggestions.
                    assert!(
                        count <= 1,
                        "{shape} {case} {counter}: {}",
                        retained.incremental_stats
                    );
                }
            }
            assert_eq!(
                retained.incremental_stats["precheck_templates"], 0,
                "{shape} {case}"
            );
            assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
            assert_eq!(fs::read_to_string(&library).unwrap(), library_source);
        }
    }
}

#[test]
fn semantic_verify_precheck_inheritance_validation_skips_unrelated() {
    for count in [8, 32, 128] {
        for chain in [false, true] {
            let f = Fixture::new();
            let entry = f.0.join("main.wi");
            let mut source = format!(
                "class Gate<T> {{ value: T; }} {} open class C0 {{}}\n",
                basic()
            );
            for i in 1..=count {
                let parent = if chain { i - 1 } else { 0 };
                source.push_str(&format!("open class C{i} extends C{parent} {{}}\n"));
            }
            fs::write(&entry, &source).unwrap();
            let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
            let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            let candidate = source.replacen("return 1;", "return 2;", 1);
            fs::write(&entry, &candidate).unwrap();
            let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
            assert_eq!(
                serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap()
            );
            for counter in [
                "declaration_validations",
                "precheck_declarations",
                "precheck_signatures",
                "precheck_templates",
            ] {
                assert_eq!(
                    retained.incremental_stats[counter], 0,
                    "chain={chain} count={count} {counter}: {}",
                    retained.incremental_stats
                );
            }
            assert_eq!(retained.incremental_stats["precheck_bodies"], 1);
            assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
            // The ancestor becomes sealed, invalidating the children that read
            // its inheritance contract; diagnostic replay must match clean.
            let invalid = candidate.replacen("open class C0", "class C0", 1);
            for candidate in [invalid.clone(), format!("\n{invalid}"), source.clone()] {
                fs::write(&entry, &candidate).unwrap();
                let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
                let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
                let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
                assert_eq!(
                    serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                    serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
                    "chain={chain} count={count}"
                );
                assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
            }
        }
    }
}

#[test]
fn semantic_verify_precheck_class_state_matches_clean() {
    for (shape, declaration, needle, change) in [
        (
            "marker",
            "interface Send {} interface Sync extends Send {} interface Contract extends Sync {} class Shared implements Contract { pub value: i64; }",
            "pub value: i64;",
            "pub mut value: i64;",
        ),
        (
            "default",
            "interface Named { fn get(self) -> i64 { return 3; } } class Shared implements Named {}",
            "return 3;",
            "return true;",
        ),
        (
            "static",
            "class Shared { pub static value: i64 = 3; }",
            "= 3;",
            "= true;",
        ),
    ] {
        let f = Fixture::new();
        let entry = f.0.join("main.wi");
        let source = format!("class Gate<T> {{ value: T; }} {} {declaration}", basic());
        fs::write(&entry, &source).unwrap();
        let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
        let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        assert!(
            !baseline.diagnostics.iter().any(|d| d.severity == "error"),
            "{shape}: {:?}",
            baseline.diagnostics
        );
        for (case, candidate) in [
            ("unrelated", source.replacen("return 1;", "return 2;", 1)),
            ("invalid", source.replacen(needle, change, 1)),
            (
                "relocate",
                format!("\n{}", source.replacen(needle, change, 1)),
            ),
            ("repair", source.clone()),
        ] {
            fs::write(&entry, &candidate).unwrap();
            let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
            assert_eq!(
                serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
                "{shape} {case}"
            );
            if case == "unrelated" {
                assert_eq!(
                    retained.incremental_stats["declaration_validations"], 0,
                    "{shape}: {}",
                    retained.incremental_stats
                );
                assert_eq!(
                    retained.incremental_stats["precheck_bodies"], 1,
                    "{shape}: {}",
                    retained.incremental_stats
                );
            } else if case != "repair" {
                assert!(
                    warm.diagnostics.iter().any(|d| d.severity == "error"),
                    "{shape} {case}"
                );
            }
            assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
        }
    }
}

#[test]
fn semantic_verify_precheck_collision_diagnostics_match_clean() {
    #[derive(Default)]
    struct CompleteDiagnostics {
        raw: Vec<serde_json::Value>,
        facts: Diagnostics,
    }
    impl crate::diagnostics::DiagnosticEmitter for CompleteDiagnostics {
        fn emit(
            &mut self,
            diagnostic: &crate::diagnostics::Diagnostic,
            sources: &dyn crate::diagnostics::source_map::SourceLookup,
        ) -> std::io::Result<()> {
            self.raw.push(serde_json::to_value(diagnostic).unwrap());
            self.facts.emit(diagnostic, sources)
        }
    }
    fn capture(
        entry: &std::path::Path,
        state: &mut crate::compiler_db::revision::AnalysisRevision,
    ) -> (Baseline, Vec<serde_json::Value>) {
        let mut diagnostics = CompleteDiagnostics::default();
        let snapshot = state
            .analyze_verification(
                crate::CompilerSession::new(
                    entry.to_str().unwrap(),
                    "",
                    &crate::CompilerOptions::debug(),
                    None,
                ),
                &mut diagnostics,
            )
            .unwrap();
        (
            Baseline::from_snapshot(&snapshot, diagnostics.facts.facts).unwrap(),
            diagnostics.raw,
        )
    }
    let f = Fixture::new();
    let entry = f.0.join("main.wi");
    let provider = f.0.join("provider.wi");
    fs::write(&provider, "pub fn other() -> i64 { return 4; }").unwrap();
    fs::write(f.0.join("value.wi"), "").unwrap();
    fs::write(f.0.join("extra.wi"), "").unwrap();
    let base = "class Gate<T> { value: T; } fn value() -> i64 { return 1; } fn main() {}";
    let duplicate = "fn pick<T>(x: T) -> T { return x; } fn pick<T>(x: T) -> T { return x; }";
    fs::write(&entry, base).unwrap();
    let mut state = crate::compiler_db::revision::AnalysisRevision::default();
    let (baseline, _) = capture(&entry, &mut state);
    for (case, candidate, invalid) in [
        ("module_add", format!("import value; {base}"), true),
        ("module_remove", base.to_owned(), false),
        (
            "alias_add",
            format!("import provider as value; import extra; {base}"),
            true,
        ),
        (
            "alias_reorder",
            format!("import extra; import provider as value; {base}"),
            true,
        ),
        (
            "alias_relocate",
            format!("\n\nimport extra; import provider as value; {base}"),
            true,
        ),
        (
            "item_alias",
            format!("import provider::other as value; {base}"),
            true,
        ),
        ("alias_remove", base.to_owned(), false),
        (
            "constant_first",
            format!("const value: i64 = 9; {base}"),
            true,
        ),
        (
            "constant_last",
            format!("{base} const value: i64 = 9;"),
            true,
        ),
        (
            "constant_relocate",
            format!("\n{base} const value: i64 = 9;"),
            true,
        ),
        ("constant_remove", base.to_owned(), false),
        ("duplicate_generic", format!("{base} {duplicate}"), true),
        ("duplicate_relocate", format!("\n{base} {duplicate}"), true),
        ("duplicate_remove", base.to_owned(), false),
    ] {
        fs::write(&entry, &candidate).unwrap();
        for repeat in 0..2 {
            let (warm, warm_diagnostics) = capture(&entry, &mut state);
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let (clean, clean_diagnostics) = capture(&entry, &mut clean_state);
            assert_eq!(
                serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
                "{case} repeat={repeat}"
            );
            assert_eq!(
                warm_diagnostics, clean_diagnostics,
                "full diagnostics {case} repeat={repeat}"
            );
            assert_eq!(
                warm.diagnostics.iter().any(|d| d.severity == "error"),
                invalid,
                "{case}"
            );
            if repeat == 1 {
                assert_eq!(
                    state.incremental_stats["declaration_validations"], 0,
                    "stable {case}: {}",
                    state.incremental_stats
                );
            }
            assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
            assert_eq!(
                fs::read_to_string(&provider).unwrap(),
                "pub fn other() -> i64 { return 4; }"
            );
        }
    }
}

#[test]
fn semantic_verify_import_topology_retains_unrelated_modules() {
    for count in [8, 32, 128] {
        let f = Fixture::new();
        let entry = f.0.join("main.wi");
        let library = f.0.join("stable.wi");
        let extra = f.0.join("extra.wi");
        let unchanged = (0..count)
            .map(|i| format!("pub fn unrelated{i}() -> i64 {{ return {i}; }}\n"))
            .collect::<String>();
        fs::write(&library, &unchanged).unwrap();
        let source = format!("import stable;\n{}", basic());
        fs::write(&entry, &source).unwrap();
        let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
        let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        for (case, candidate) in [
            ("add", format!("import extra;\n{source}")),
            (
                "reorder",
                format!("import stable; import extra;\n{}", basic()),
            ),
            ("remove", source.clone()),
        ] {
            if case == "add" {
                fs::write(&extra, "pub fn added() -> i64 { return 42; }").unwrap();
            }
            if case == "remove" {
                fs::remove_file(&extra).unwrap();
            }
            fs::write(&entry, &candidate).unwrap();
            let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
            let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
            assert_eq!(
                serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
                "{case} {count}"
            );
            assert_eq!(
                retained.typechecks,
                usize::from(case == "add"),
                "{case} {count}: {}",
                retained.incremental_stats
            );
            assert_eq!(
                retained.incremental_stats["effects_recomputed"],
                // New module: its function plus inherited Into, in two effect lattices.
                if case == "add" { 4 } else { 0 },
                "{case} {count}: {}",
                retained.incremental_stats
            );
            assert_eq!(
                retained.incremental_stats["effects_scanned"],
                usize::from(case == "add"),
                "{case} {count}: {}",
                retained.incremental_stats
            );
            assert_eq!(
                retained.incremental_stats["precheck_bodies"],
                usize::from(case == "add"),
                "{case} {count}: {}",
                retained.incremental_stats
            );
            assert_eq!(fs::read_to_string(&library).unwrap(), unchanged);
            assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
        }
    }
}

#[test]
fn semantic_verify_import_topology_invalidates_dependents_and_relocated_evidence() {
    let f = Fixture::new();
    let entry = f.0.join("main.wi");
    let library = f.0.join("library.wi");
    let extra = f.0.join("extra.wi");
    fs::write(&library, "pub fn value(x: i64) -> i64 { return x + 1; }").unwrap();
    let body =
        "fn dependent() -> i64 { return library::value(1); } fn main() { println(dependent()); }";
    let source = format!("import library; {body}");
    fs::write(&entry, &source).unwrap();
    let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
    let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
    for (case, imports, lib) in [
        (
            "insert_before",
            "import extra; import library;",
            Some("pub fn value(x: i64) -> i64 { return x + 1; }"),
        ),
        (
            "changed_effect",
            "import extra; import library;",
            Some("pub fn value(x: i64) -> i64 { println(x); return x; }"),
        ),
        (
            "changed_signature",
            "import extra; import library;",
            Some("pub fn value(x: bool) -> bool { return x; }"),
        ),
        ("remove", "import extra;", None),
        (
            "restore",
            "import library; import extra;",
            Some("pub fn value(x: i64) -> i64 { return x + 2; }"),
        ),
        (
            "relocate",
            "\n\nimport library; import extra;",
            Some("\n\npub fn value(x: i64) -> i64 { return x + 2; }"),
        ),
    ] {
        fs::write(&extra, "pub fn spare() {}").unwrap();
        match lib {
            Some(text) => fs::write(&library, text).unwrap(),
            None => fs::remove_file(&library).unwrap(),
        }
        let candidate = format!("{imports} {body}");
        fs::write(&entry, &candidate).unwrap();
        let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
        let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
        assert_eq!(
            serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
            serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
            "{case}"
        );
        if case == "insert_before" {
            assert_eq!(retained.typechecks, 1);
        }
        if case == "relocate" {
            assert_eq!(retained.typechecks, 0);
            assert_eq!(
                retained.incremental_stats["effects_recomputed"], 0,
                "{}",
                retained.incremental_stats
            );
        }
        assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
    }
}

#[test]
fn semantic_verify_async_borrow_unrelated_import_changes_skip_checks() {
    verify_unrelated_import_changes_skip_semantics(true);
}

fn verify_unrelated_import_changes_skip_semantics(with_borrows: bool) {
    for count in [8, 32, 128] {
        for transaction in [false, true] {
            let f = Fixture::new();
            let entry = f.0.join("main.wi");
            let extra = f.0.join("extra.wi");
            fs::write(&extra, "").unwrap();
            let source = format!(
                "class Box<T> {{ value: T; }} {} {}",
                basic(),
                if with_borrows {
                    "async fn borrow(x: &i64) -> i64 { return x; } async fn consumer() { let x = 1; let t = borrow(&x); await t; }"
                } else {
                    ""
                }
            ) + &(0..count)
                .map(|i| format!("fn unrelated{i}() -> i64 {{ return {i}; }}\n"))
                .collect::<String>();
            fs::write(&entry, &source).unwrap();
            let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
            let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
            if with_borrows {
                assert!(
                    retained.incremental_stats["async_borrow_checks"]
                        .as_u64()
                        .unwrap()
                        >= count
                );
            }
            for (case, candidate) in [
                ("add", format!("import extra;\n{source}")),
                ("remove", source.clone()),
            ] {
                fs::write(&entry, &candidate).unwrap();
                if transaction {
                    // A retained compiler revision may predate the current
                    // prepared transaction. Import edits must not invalidate
                    // bodies that do not read the changed import bindings.
                    let snapshot = crate::CompilerSession::new(
                        entry.to_str().unwrap(),
                        "",
                        &crate::CompilerOptions::debug(),
                        None,
                    )
                    .analysis_for_edit_with_emitter(&mut Diagnostics::default())
                    .unwrap();
                    let function = snapshot
                        .functions
                        .iter()
                        .find(|f| f.name == "value")
                        .unwrap()
                        .id
                        .clone();
                    let workspace = edit::Workspace::open(&f.0).unwrap();
                    let preview = workspace
                        .prepare(
                            &entry,
                            false,
                            edit::Request {
                                revision: snapshot.revision,
                                operations: vec![edit::Operation::ReplaceBody {
                                    function,
                                    body: "{ return 7; }".into(),
                                }],
                            },
                            edit::ChangeFormat::Diff,
                            &mut Diagnostics::default(),
                        )
                        .unwrap();
                    let id = preview["transaction"].as_str().unwrap();
                    let manifest =
                        serde_json::from_value(preview["verification_manifest"].clone()).unwrap();
                    let warm = workspace
                        .verify_manifest_retained(id, &manifest, &mut retained)
                        .unwrap();
                    let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
                    let clean = workspace
                        .verify_manifest_retained(id, &manifest, &mut clean_state)
                        .unwrap();
                    assert_eq!(
                        serde_json::to_value(warm).unwrap(),
                        serde_json::to_value(clean).unwrap(),
                        "{case} transaction count={count}"
                    );
                } else {
                    let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
                    let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
                    let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
                    assert_eq!(
                        serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
                        serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
                        "{case} tree count={count}"
                    );
                }
                assert_eq!(
                    retained.incremental_stats["precheck_bodies"],
                    usize::from(transaction && case == "add"),
                    "{case} transaction={transaction} count={count}: {}",
                    retained.incremental_stats
                );
                for counter in [
                    "precheck_signatures",
                    "precheck_templates",
                    "precheck_declarations",
                    "declaration_validations",
                ] {
                    assert_eq!(
                        retained.incremental_stats[counter], 0,
                        "{case} transaction={transaction} count={count} {counter}: {}",
                        retained.incremental_stats
                    );
                }
                assert_eq!(
                    retained.typechecks,
                    usize::from(transaction && case == "add"),
                    "{case} count={count}"
                );
                assert_eq!(
                    retained.incremental_stats["effects_recomputed"], 0,
                    "{case} count={count}: {}",
                    retained.incremental_stats
                );
                assert_eq!(
                    retained.incremental_stats["async_borrow_checks"],
                    usize::from(with_borrows && transaction && case == "add"),
                    "{case} transaction={transaction} count={count}: {}",
                    retained.incremental_stats
                );
                assert_eq!(fs::read_to_string(&entry).unwrap(), candidate);
                assert_eq!(fs::read_to_string(&extra).unwrap(), "");
            }
        }
    }
}

#[test]
fn semantic_verify_async_borrow_actual_dependencies_match_clean() {
    let f = Fixture::new();
    let entry = f.0.join("main.wi");
    let provider = f.0.join("provider.wi");
    let asynchronous = "pub async fn borrow(x: &i64) -> i64 { return x; }";
    let synchronous = "async fn other() -> i64 { return 1; } pub fn borrow(x: &i64) -> Task<i64> { return other(); }";
    let source = "import provider; async fn local(x: &i64) -> i64 { return x; } async fn active() { let x = 1; await local(&x); } async fn dependent() { let x = 1; let t = provider::borrow(&x); } fn main() {}";
    fs::write(&entry, source).unwrap();
    fs::write(&provider, asynchronous).unwrap();
    let mut retained = crate::compiler_db::revision::AnalysisRevision::default();
    let baseline = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
    assert!(baseline.diagnostics.iter().any(|d| d.code == "E1708"));
    for (case, library, candidate, escaping, checks) in [
        ("sync", synchronous, source.to_owned(), false, 1),
        ("async", asynchronous, source.to_owned(), true, 1),
        (
            "await",
            asynchronous,
            source.replace(
                "let t = provider::borrow(&x);",
                "let t = provider::borrow(&x); await t;",
            ),
            false,
            1,
        ),
        (
            "nested",
            asynchronous,
            source.replace(
                "let t = provider::borrow(&x);",
                "let f = || { let t = provider::borrow(&x); };",
            ),
            true,
            1,
        ),
        ("restore", asynchronous, source.to_owned(), true, 1),
        ("relocate", asynchronous, format!("\n\n{source}"), true, 0),
    ] {
        fs::write(&provider, library).unwrap();
        fs::write(&entry, &candidate).unwrap();
        let warm = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        let mut clean_state = crate::compiler_db::revision::AnalysisRevision::default();
        let clean = Baseline::capture_retained(&entry, None, &mut clean_state).unwrap();
        assert_eq!(
            serde_json::to_value(baseline.compare(&warm).unwrap()).unwrap(),
            serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
            "{case}"
        );
        assert_eq!(
            warm.diagnostics.iter().any(|d| d.code == "E1708"),
            escaping,
            "{case}"
        );
        assert_eq!(
            retained.incremental_stats["async_borrow_checks"], checks,
            "{case}: {}",
            retained.incremental_stats
        );
        let repeat = Baseline::capture_retained(&entry, None, &mut retained).unwrap();
        assert_eq!(
            serde_json::to_value(baseline.compare(&repeat).unwrap()).unwrap(),
            serde_json::to_value(baseline.compare(&clean).unwrap()).unwrap(),
            "repeat {case}"
        );
        assert_eq!(
            retained.incremental_stats["async_borrow_checks"], 0,
            "repeat {case}"
        );
    }
}
