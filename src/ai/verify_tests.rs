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
                Fact {
                    signature: Some(json!({"type":"fn() -> i64","exported":true})),
                    effects: Some(EffectState { may: 0, unknown: 0 }),
                    body: "one".into(),
                    callees: vec![],
                },
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
        inputs: BTreeMap::new(),
        callers: BTreeMap::new(),
        index: None,
        diagnostic_index: None,
        diagnostic_lookup: BTreeMap::new(),
        public_count: 0,
    };
    b.reindex().unwrap();
    b
}
#[test]
fn semantic_verify_radix_scaling_counts() {
    for n in [16, 64, 256, 1024, 4096] {
        let a = synthetic(n);
        let mut b = a.clone();
        b.facts.get_mut("f0").unwrap().body = "two".into();
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
            b.facts.get_mut(&format!("f{i}")).unwrap().signature =
                Some(json!({"type":"fn() -> bool","exported":true}));
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
    b.facts.get_mut("f0").unwrap().effects = Some(EffectState { may: 1, unknown: 2 });
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
            .unwrap()
            .callees
            .push(callee.clone());
        a.callers.entry(callee).or_default().insert(format!("f{i}"));
    }
    a.reindex().unwrap();
    let mut b = a.clone();
    b.facts.get_mut("f0").unwrap().body = "two".into();
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
    b.facts.get_mut("f0").unwrap().body = "two".into();
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
