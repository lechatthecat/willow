use super::*;
use std::{fs, process::Command};
fn git(root: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap().trim().into()
}
fn fixture() -> (tree::Scratch, Options, Policy) {
    let dir = tree::Scratch::new().unwrap();
    git(&dir.0, &["init", "-q"]);
    git(&dir.0, &["config", "user.name", "Fixture"]);
    git(&dir.0, &["config", "user.email", "fixture@example.invalid"]);
    fs::write(
        dir.0.join("main.wi"),
        "@pure fn f(x:i64)->i64 requires x>=0 ensures result>=0 {return x;} fn main() {}",
    )
    .unwrap();
    git(&dir.0, &["add", "main.wi"]);
    git(&dir.0, &["commit", "-qm", "Initial fixture"]);
    let commit = git(&dir.0, &["rev-parse", "HEAD"]);
    let options = Options {
        root: dir.0.clone(),
        entry: None,
        base: resolve_base(&dir.0, "HEAD").unwrap(),
        project: false,
        fail_on_growth: false,
    };
    let policy = Policy {
        protected_base: Some(commit),
        ..Default::default()
    };
    (dir, options, policy)
}
#[test]
fn contract_policy_lock_and_source_changed_together_need_approval() {
    let (dir, o, mut p) = fixture();
    let initial = verify(&o, &p).unwrap();
    assert_eq!(initial.schema, "willow.semantic-diff.v4");
    assert_eq!(initial.exit_code(), 0, "{initial:?}");
    assert_eq!(
        initial.summary["approval"]["contract_status"],
        "contract_digest_unchanged"
    );
    fs::write(
        dir.0.join("main.wi"),
        "@pure fn f(x:i64)->i64 requires x>=0 ensures result==x {return x;} fn main() {}",
    )
    .unwrap();
    p.write_lock = true;
    let first = verify(&o, &p).unwrap();
    assert_eq!(first.exit_code(), 1, "{first:?}");
    p.write_lock = false;
    let second = verify(&o, &p).unwrap();
    assert_eq!(second.exit_code(), 1, "{second:?}");
    assert_eq!(second.changes[0]["contract_preserved"], "PROVEN");
    assert_eq!(
        second.summary["approval"]["approval_status"],
        "approval_required"
    );
    assert_eq!(
        second.summary["approval"]["specification_provenance"],
        "UNVERIFIED"
    );
    let again = verify(&o, &p).unwrap();
    assert_eq!(
        serde_json::to_value(second).unwrap(),
        serde_json::to_value(again).unwrap()
    );
    assert!(super::super::verify(&o).unwrap().schema.ends_with("v1"));
}
#[test]
fn contract_policy_missing_baseline_mutable_revision_and_corrupt_lock() {
    let (dir, o, p) = fixture();
    assert_eq!(verify(&o, &Policy::default()).unwrap().exit_code(), 3);
    assert!(
        verify(
            &o,
            &Policy {
                protected_base: Some("HEAD".into()),
                ..Default::default()
            }
        )
        .is_err()
    );
    fs::write(dir.0.join("contracts.lock"), "{}").unwrap();
    assert!(verify(&o, &p).is_err());
}
#[test]
fn contract_policy_legacy_does_not_capture_new_lock() {
    let (dir, o, _) = fixture();
    let before = super::super::verify(&o).unwrap();
    fs::write(dir.0.join("contracts.lock"), "untrusted content").unwrap();
    let after = super::super::verify(&o).unwrap();
    assert_eq!(
        serde_json::to_value(before).unwrap(),
        serde_json::to_value(after).unwrap()
    );
}
#[test]
#[cfg(unix)]
fn contract_policy_external_verifier_binding_and_fail_closed() {
    let workspace = tree::Scratch::new().unwrap();
    let trusted = tree::Scratch::new().unwrap();
    let evidence = trusted.0.join("verifier-fixture.sh");
    fs::write(&evidence, "{}").unwrap();
    let verifier = PathBuf::from("/bin/sh");
    let binding = json!({"protected_commit":"base","base_tree":"before","target_tree":"after","protected_contracts":"old","target_contracts":"new","normalization":"v1"});
    let policy = Policy {
        approval_evidence: Some(evidence.clone()),
        approval_verifier: Some(verifier.clone()),
        ..Default::default()
    };
    let set_script = |body: &str| {
        fs::write(&evidence, format!("{body}\n")).unwrap();
    };
    set_script(
        "printf '{\"schema\":\"willow.contract-approval.v1\",\"approved\":true,\"binding\":%s}' \"$1\"",
    );
    assert!(approval_in_context(&policy, &workspace.0, &binding, true).unwrap());
    assert!(approval_in_context(&policy, &workspace.0, &binding, false).is_err());
    let untrusted = workspace.0.join("untrusted-verifier");
    fs::write(&untrusted, "fixture").unwrap();
    let untrusted_policy = Policy {
        approval_evidence: Some(evidence.clone()),
        approval_verifier: Some(untrusted),
        ..Default::default()
    };
    assert!(approval_in_context(&untrusted_policy, &workspace.0, &binding, true).is_err());
    let mut replay = binding.clone();
    replay["target_tree"] = json!("stale");
    let response = json!({"schema":"willow.contract-approval.v1","approved":true,"binding":replay});
    set_script(&format!("printf '%s' '{}'", response));
    assert!(!approval_in_context(&policy, &workspace.0, &binding, true).unwrap());
    set_script("exit 1");
    assert!(approval_in_context(&policy, &workspace.0, &binding, true).is_err());
    set_script("printf '{}'");
    assert!(!approval_in_context(&policy, &workspace.0, &binding, true).unwrap());
    set_script("printf 'invalid'");
    assert!(approval_in_context(&policy, &workspace.0, &binding, true).is_err());
}

#[test]
fn contract_policy_project_scope_cannot_silently_omit_sources() {
    let (dir, mut o, p) = fixture();
    fs::write(dir.0.join("extra.wi"), "@pure fn other()->i64 {return 1;}").unwrap();
    o.project = true;
    assert!(
        verify(&o, &p)
            .unwrap_err()
            .to_string()
            .contains("multi-source")
    );
}

#[test]
fn contract_policy_ignored_lock_is_checked() {
    let (dir, o, p) = fixture();
    fs::write(dir.0.join(".gitignore"), "contracts.lock\n").unwrap();
    fs::write(dir.0.join("contracts.lock"), "{}").unwrap();
    assert!(verify(&o, &p).is_err());
}
#[test]
#[cfg(unix)]
fn contract_policy_lock_symlink_cannot_overwrite_external_file() {
    let (dir, o, mut p) = fixture();
    let outside = tree::Scratch::new().unwrap();
    let keep = outside.0.join("keep");
    fs::write(&keep, "keep").unwrap();
    fs::write(dir.0.join(".gitignore"), "contracts.lock\n").unwrap();
    std::os::unix::fs::symlink(&keep, dir.0.join("contracts.lock")).unwrap();
    p.write_lock = true;
    assert!(verify(&o, &p).is_err());
    assert_eq!(fs::read_to_string(keep).unwrap(), "keep");
}

#[test]
fn proof_no_panic_verify_edit_and_lock_evidence() {
    let (dir, o, mut policy) = fixture();
    let source = "@pure @verified @effects(no_panic) fn f(x:i64)->i64 requires x>=0 && x<100 {return x+1;} @effects(no_panic) fn plain(x:i64)->i64 {return x;} fn main() {}";
    fs::write(dir.0.join("main.wi"), source).unwrap();
    git(&dir.0, &["add", "main.wi"]);
    git(&dir.0, &["commit", "-qm", "Proven arithmetic"]);
    let mut o = o;
    o.base = resolve_base(&dir.0, "HEAD").unwrap();
    policy.protected_base = Some(git(&dir.0, &["rev-parse", "HEAD"]));
    fs::write(
        dir.0.join("main.wi"),
        source
            .replace("return x+1;", "return 1+x;")
            .replace("{return x;}", "{return 0;}"),
    )
    .unwrap();
    let legacy = super::super::verify(&o).unwrap();
    assert_eq!(legacy.exit_code(), 0, "{legacy:?}");
    assert!(
        legacy.summary["no_panic"]
            .to_string()
            .contains("solver-proof"),
        "{legacy:?}"
    );
    assert!(legacy.human(false).contains("solver-proof"));
    policy.write_lock = true;
    let report = verify(&o, &policy).unwrap();
    assert_eq!(report.exit_code(), 0, "{report:?}");
    let lock: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.0.join("contracts.lock")).unwrap()).unwrap();
    assert_eq!(lock["functions"]["f"]["no_panic"]["basis"], "solver-proof");
    assert_eq!(
        lock["functions"]["plain"]["no_panic"]["basis"],
        "effect-analysis"
    );
    fs::write(dir.0.join("main.wi"), source.replace("x<100", "x>=0")).unwrap();
    let report = super::super::verify(&o).unwrap();
    assert_ne!(report.exit_code(), 0, "{report:?}");
    assert!(
        report.summary["diagnostics"].to_string().contains("E2610"),
        "{report:?}"
    );
}

#[test]
fn proof_no_panic_verify_edit_dependency_and_output_bounds() {
    for n in [0, 32, 128] {
        let (dir, mut options, _) = fixture();
        let mut source = "@effects(no_panic) fn f(x:i64)->i64 {return x;} fn main() {}".to_owned();
        for i in 0..n {
            source += &format!("fn unrelated{i}(x:i64)->i64 {{return x;}}");
        }
        fs::write(dir.0.join("main.wi"), &source).unwrap();
        git(&dir.0, &["add", "main.wi"]);
        git(&dir.0, &["commit", "-qm", "Evidence inventory"]);
        options.base = resolve_base(&dir.0, "HEAD").unwrap();
        fs::write(
            dir.0.join("main.wi"),
            source.replacen("return x;", "return 0;", 1),
        )
        .unwrap();
        let report = super::super::verify(&options).unwrap();
        assert_eq!(report.exit_code(), 0, "{report:?}");
        for side in ["base", "target"] {
            assert_eq!(
                report.summary["no_panic"][side].as_object().unwrap().len(),
                1
            );
        }
        println!(
            "proof-output unrelated={n} evidence_entries=2 bytes={}",
            report.summary["no_panic"].to_string().len()
        );
    }
    let (dir, mut options, _) = fixture();
    let source = "@pure @verified fn helper(x:i64)->i64 requires x>=0 && x<100 {return x+1;} @pure @verified @effects(no_panic) fn f(x:i64)->i64 requires x>=0 && x<100 {return helper(x);} fn main() {}";
    fs::write(dir.0.join("main.wi"), source).unwrap();
    git(&dir.0, &["add", "main.wi"]);
    git(&dir.0, &["commit", "-qm", "Proven caller"]);
    options.base = resolve_base(&dir.0, "HEAD").unwrap();
    fs::write(dir.0.join("main.wi"), source.replacen("x<100", "x<50", 1)).unwrap();
    let report = super::super::verify(&options).unwrap();
    assert_ne!(report.exit_code(), 0, "{report:?}");
    assert!(
        report.summary["diagnostics"]["target"]
            .to_string()
            .contains("E2610"),
        "{report:?}"
    );
    assert!(
        report.summary["no_panic"]["target"]
            .as_object()
            .unwrap()
            .values()
            .any(|e| e["status"] == "UNPROVEN"),
        "{report:?}"
    );
}
