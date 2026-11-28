use super::super::tests::wire;
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
        wire(&initial.summary)["approval"]["contract_status"],
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
    assert_eq!(wire(&second.changes[0])["contract_preserved"], "PROVEN");
    assert_eq!(
        wire(&second.summary)["approval"]["approval_status"],
        "approval_required"
    );
    assert_eq!(
        wire(&second.summary)["approval"]["specification_provenance"],
        "UNVERIFIED"
    );
    let again = verify(&o, &p).unwrap();
    assert_eq!(
        serde_json::to_value(second).unwrap(),
        serde_json::to_value(again).unwrap()
    );
    assert!(super::super::verify(&o).unwrap().schema.ends_with("v3"));
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
    assert!(approval(&policy, &workspace.0, &binding).unwrap());
    let untrusted = workspace.0.join("untrusted-verifier");
    fs::write(&untrusted, "fixture").unwrap();
    let untrusted_policy = Policy {
        approval_evidence: Some(evidence.clone()),
        approval_verifier: Some(untrusted),
        ..Default::default()
    };
    assert!(approval(&untrusted_policy, &workspace.0, &binding).is_err());
    for field in binding.as_object().unwrap().keys() {
        let mut replay = binding.clone();
        replay[field] = json!("stale");
        let response =
            json!({"schema":"willow.contract-approval.v1","approved":true,"binding":replay});
        set_script(&format!("printf '%s' '{}'", response));
        assert!(
            !approval(&policy, &workspace.0, &binding).unwrap(),
            "{field}"
        );
    }
    for response in [
        json!({"schema":"willow.contract-approval.v1","approved":false,"binding":binding}),
        json!({"schema":"wrong","approved":true,"binding":binding}),
        json!({"schema":"willow.contract-approval.v1","approved":true,"binding":binding,"extra":true}),
    ] {
        set_script(&format!("printf '%s' '{}'", response));
        assert!(!approval(&policy, &workspace.0, &binding).unwrap());
    }
    set_script("exit 1");
    assert!(approval(&policy, &workspace.0, &binding).is_err());
    set_script("printf '{}'");
    assert!(!approval(&policy, &workspace.0, &binding).unwrap());
    set_script("printf 'invalid'");
    assert!(approval(&policy, &workspace.0, &binding).is_err());
}

// Re-execute a single test so changing CI never mutates the parallel test
// process's environment. Exercise the real approval entry point in each child.
#[test]
#[cfg(unix)]
fn contract_policy_ci_environment_is_not_trust_signal() {
    for ci in [None, Some("false"), Some("true"), Some("arbitrary")] {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "ai::verify::semantic_diff::contracts::tests::contract_policy_external_verifier_binding_and_fail_closed",
        ]);
        command.env_remove("CI");
        if let Some(value) = ci {
            command.env("CI", value);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "CI={ci:?}: {output:?}");
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }
}

#[test]
fn contract_policy_missing_verifier_or_evidence_cannot_approve() {
    let workspace = tree::Scratch::new().unwrap();
    for policy in [
        Policy::default(),
        Policy {
            approval_verifier: Some("missing".into()),
            ..Default::default()
        },
        Policy {
            approval_evidence: Some("missing".into()),
            ..Default::default()
        },
    ] {
        assert!(!approval(&policy, &workspace.0, &json!({})).unwrap());
    }
}

#[test]
#[cfg(unix)]
fn contract_policy_verifier_input_and_output_limits() {
    let workspace = tree::Scratch::new().unwrap();
    let external = tree::Scratch::new().unwrap();
    let evidence = external.0.join("verifier.sh");
    let policy = Policy {
        approval_verifier: Some("/bin/sh".into()),
        approval_evidence: Some(evidence.clone()),
        ..Default::default()
    };
    let response =
        json!({"schema":"willow.contract-approval.v1","approved":true,"binding":{}}).to_string();
    // Exactly the limit remains valid; one byte over must fail closed. Output
    // padding also exceeds pipe capacity, exercising concurrent bounded reads.
    for length in [65536, 65537] {
        let script = format!(
            "printf '%s' '{response}'; printf '%{}s' ''\n",
            length - response.len()
        );
        fs::write(&evidence, script).unwrap();
        let result = approval(&policy, &workspace.0, &json!({}));
        if length == 65536 {
            assert!(result.unwrap());
        } else {
            assert_eq!(
                result.unwrap_err().to_string(),
                "approval verifier output exceeds limit"
            );
        }
        let mut script = format!("printf '%s' '{response}'\n#");
        script.extend(std::iter::repeat_n('x', length - script.len()));
        fs::write(&evidence, script).unwrap();
        let result = approval(&policy, &workspace.0, &json!({}));
        if length == 65536 {
            assert!(result.unwrap());
        } else {
            assert_eq!(
                result.unwrap_err().to_string(),
                "approval evidence exceeds limit"
            );
        }
    }
}

#[test]
#[cfg(unix)]
fn contract_policy_growing_bindings_invoke_verifier_once() {
    let workspace = tree::Scratch::new().unwrap();
    let external = tree::Scratch::new().unwrap();
    let evidence = external.0.join("verifier.sh");
    fs::write(
        &evidence,
        r#"printf x >> "$0.count"
printf '{"schema":"willow.contract-approval.v1","approved":true,"binding":%s}' "$1"
"#,
    )
    .unwrap();
    let policy = Policy {
        approval_verifier: Some("/bin/sh".into()),
        approval_evidence: Some(evidence),
        ..Default::default()
    };
    for (index, size) in [1, 32, 256].into_iter().enumerate() {
        let binding = json!({"changes": vec!["change"; size]});
        assert!(approval(&policy, &workspace.0, &binding).unwrap());
        assert_eq!(
            fs::read(external.0.join("verifier.sh.count"))
                .unwrap()
                .len(),
            index + 1
        );
        println!("binding_items={size} verifier_invocations=1");
    }
}

#[test]
fn contract_policy_wait_child() {
    match std::env::var("WILLOW_APPROVAL_WAIT_TEST").as_deref() {
        Ok("success") => (),
        Ok("reject") => std::process::exit(7),
        Ok("block") => {
            // Parent keeps the pipe open without writing. No timing assumption:
            // the verifier wait must kill this process at its real deadline.
            use std::io::Read;
            std::io::stdin().read_exact(&mut [0]).unwrap();
            panic!("blocked verifier unexpectedly received input");
        }
        _ => (),
    }
}

#[test]
fn contract_policy_blocking_wait_exit_and_ten_second_timeout() {
    for mode in ["success", "reject", "block"] {
        let child = shared_child::SharedChild::spawn(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "ai::verify::semantic_diff::contracts::tests::contract_policy_wait_child",
                ])
                .env("WILLOW_APPROVAL_WAIT_TEST", mode)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()),
        )
        .unwrap();
        let started = std::time::Instant::now();
        let result = wait_for_approval(&child);
        match mode {
            "success" => assert!(result.unwrap().success()),
            "reject" => assert_eq!(result.unwrap().code(), Some(7)),
            _ => {
                assert_eq!(
                    result.unwrap_err().to_string(),
                    "approval verifier timed out"
                );
                assert!(started.elapsed() >= std::time::Duration::from_secs(10));
                assert!(
                    !child
                        .try_wait()
                        .unwrap()
                        .expect("verifier must be reaped")
                        .success()
                );
            }
        }
    }
}

#[test]
#[cfg(unix)]
fn contract_policy_closed_stdout_still_obeys_verifier_timeout() {
    let workspace = tree::Scratch::new().unwrap();
    let external = tree::Scratch::new().unwrap();
    let evidence = external.0.join("verifier.sh");
    // The timer is the behavior under test. exec keeps the timeout target and
    // sleeper the same process; closing stdout must not count as process exit.
    fs::write(&evidence, "exec 1>&-; exec sleep 60\n").unwrap();
    let policy = Policy {
        approval_verifier: Some("/bin/sh".into()),
        approval_evidence: Some(evidence),
        ..Default::default()
    };
    let started = std::time::Instant::now();
    let error = approval(&policy, &workspace.0, &json!({})).unwrap_err();
    assert_eq!(error.to_string(), "approval verifier timed out");
    assert!(started.elapsed() >= std::time::Duration::from_secs(10));
}

#[test]
fn contract_policy_project_scope_cannot_silently_omit_sources() {
    let (dir, mut o, mut p) = fixture();
    fs::create_dir(dir.0.join("src")).unwrap();
    fs::rename(dir.0.join("main.wi"), dir.0.join("src/main.wi")).unwrap();
    fs::write(
        dir.0.join("project.toml"),
        "[project]\nname=\"comparison\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    git(&dir.0, &["add", "main.wi", "src/main.wi", "project.toml"]);
    git(&dir.0, &["commit", "-qm", "Project scope fixture"]);
    o.base = resolve_base(&dir.0, "HEAD").unwrap();
    p.protected_base = Some(git(&dir.0, &["rev-parse", "HEAD"]));
    o.project = true;
    let same = verify(&o, &p).unwrap();
    assert_eq!(same.exit_code(), 0, "{same:?}");
    fs::write(
        dir.0.join("src/extra.wi"),
        "@pure fn other()->i64 {return 1;}",
    )
    .unwrap();
    let changed = verify(&o, &p).unwrap();
    assert_eq!(changed.exit_code(), 1, "{changed:?}");
    assert!(
        changed
            .changes
            .iter()
            .any(|c| c.symbol.ends_with("extra::other") && c.base.is_none()),
        "{changed:?}"
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
        wire(&legacy.summary)["no_panic"]
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
        wire(&report.summary)["diagnostics"]
            .to_string()
            .contains("E2610"),
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
                wire(&report.summary)["no_panic"][side]
                    .as_object()
                    .unwrap()
                    .len(),
                1
            );
        }
        println!(
            "proof-output unrelated={n} evidence_entries=2 bytes={}",
            wire(&report.summary)["no_panic"].to_string().len()
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
        wire(&report.summary)["diagnostics"]["target"]
            .to_string()
            .contains("E2610"),
        "{report:?}"
    );
    assert!(
        wire(&report.summary)["no_panic"]["target"]
            .as_object()
            .unwrap()
            .values()
            .any(|e| e["status"] == "UNPROVEN"),
        "{report:?}"
    );
}

#[test]
fn modular_contract_verify_edit_dependency_digests_and_cache() {
    let mut tree = tree::Tree::new();
    let module =
        "@pure @verified pub fn id(x:i64)->i64 requires x>=0 ensures result==x {return x;}";
    tree.insert("math.wi".into(), module.as_bytes().to_vec());
    tree.insert("main.wi".into(),b"import math; @pure @verified fn caller(x:i64)->i64 requires x>=1 ensures result==x {return math::id(x);} @pure @verified fn unrelated(x:i64)->i64 ensures result==x {return x;} fn main() {}".to_vec());
    let mut cache = ProofCache::default();
    let a = capture(&tree, Path::new("main.wi"), &mut cache).unwrap();
    assert!(a.errors.is_empty(), "{:?}", a.errors);
    assert!(
        a.verification
            .reports
            .iter()
            .all(|r| r.status == verification::Status::Proven),
        "{:?}",
        a.verification.reports
    );
    assert_eq!(cache.misses, 3);
    tree.insert(
        "math.wi".into(),
        module.replace("x>=0", "x>=1").into_bytes(),
    );
    let b = capture(&tree, Path::new("main.wi"), &mut cache).unwrap();
    assert_eq!(cache.hits, 1);
    assert_eq!(cache.misses, 5);
    let report = comparison::compare_typed(
        &a,
        &b,
        &verification::Options {
            artifacts: None,
            ..Default::default()
        },
    );
    let caller = report
        .changes
        .iter()
        .find(|c| c.symbol == "caller")
        .unwrap();
    assert!(caller.proof_dependencies_changed, "{report:?}");
    assert_eq!(
        caller.proof_dependencies.base.as_ref().unwrap().functions,
        ["$local::math::id"]
    );
    assert!(caller.implementation_verified, "{report:?}");
    assert_eq!(
        caller.contract_preserved,
        comparison::Preservation::Proven,
        "{report:?}"
    );
}

#[test]
fn modular_contract_verify_edit_reports_and_unknown_exit() {
    let (dir, mut o, mut p) = fixture();
    fs::write(
        dir.0.join("math.wi"),
        "@pure @verified pub fn id(x:i64)->i64 requires x>=0 ensures result==x {return x;}",
    )
    .unwrap();
    fs::write(dir.0.join("main.wi"),"import math; @pure @verified fn caller(x:i64)->i64 requires x>=1 ensures result==x {return math::id(x);} fn main() {}").unwrap();
    git(&dir.0, &["add", "main.wi", "math.wi"]);
    git(&dir.0, &["commit", "-qm", "Modular contracts"]);
    o.base = resolve_base(&dir.0, "HEAD").unwrap();
    p.protected_base = Some(git(&dir.0, &["rev-parse", "HEAD"]));
    let same = verify(&o, &p).unwrap();
    assert_eq!(same.exit_code(), 0, "{same:?}");
    fs::write(
        dir.0.join("math.wi"),
        "@pure @verified pub fn id(x:i64)->i64 requires x>=1 ensures result==x {return x;}",
    )
    .unwrap();
    let changed = verify(&o, &p).unwrap();
    assert_eq!(changed.exit_code(), 1, "{changed:?}");
    assert!(
        changed
            .changes
            .iter()
            .any(|c| c.symbol == "caller" && c.proof_dependencies_changed),
        "{changed:?}"
    );
    fs::write(
        dir.0.join("math.wi"),
        "@pure @verified pub fn id(x:i64)->i64 {return id(x);}",
    )
    .unwrap();
    let unknown = verify(&o, &p).unwrap();
    assert_eq!(unknown.exit_code(), 3, "{unknown:?}");
}

#[test]
fn modular_contract_pure_helper_changes_are_compared_with_isolated_definitions() {
    let mut tree = tree::Tree::new();
    tree.insert(
        "math.wi".into(),
        b"@pure pub fn positive(x:i64)->bool {return x>=0;}".to_vec(),
    );
    tree.insert("main.wi".into(),b"import math; @pure @verified fn caller(x:i64)->i64 requires math::positive(x) ensures math::positive(result) {return x;} fn main() {}".to_vec());
    let mut cache = ProofCache::default();
    let a = capture(&tree, Path::new("main.wi"), &mut cache).unwrap();
    let same = capture(&tree, Path::new("main.wi"), &mut cache).unwrap();
    let options = verification::Options {
        artifacts: None,
        ..Default::default()
    };
    let unchanged = comparison::compare_typed(&a, &same, &options);
    assert!(unchanged.uncertainty.is_empty(), "{unchanged:?}");
    assert!(
        unchanged
            .changes
            .iter()
            .all(|c| c.contract_preserved == comparison::Preservation::Proven),
        "{unchanged:?}"
    );
    tree.insert(
        "math.wi".into(),
        b"@pure pub fn positive(x:i64)->bool {return x>0;}".to_vec(),
    );
    let b = capture(&tree, Path::new("main.wi"), &mut cache).unwrap();
    let changed = comparison::compare_typed(&a, &b, &options);
    let caller = changed
        .changes
        .iter()
        .find(|c| c.symbol == "caller")
        .unwrap();
    assert!(caller.proof_dependencies_changed, "{changed:?}");
    assert_eq!(
        caller.contract_preserved,
        comparison::Preservation::NotPreserved,
        "{changed:?}"
    );
    assert!(caller.implementation_verified, "{changed:?}");
}

#[test]
fn modular_contract_constants_keep_module_identity_and_dependencies() {
    let mut tree = tree::Tree::new();
    let left = "const LIMIT:i64=100; @pure @verified pub fn id(x:i64)->i64 requires x<LIMIT ensures result==x {return x;}";
    tree.insert("left.wi".into(), left.as_bytes().to_vec());
    tree.insert("right.wi".into(), left.replace("100", "200").into_bytes());
    tree.insert("main.wi".into(), b"import left; import right; @pure @verified fn caller(x:i64)->i64 requires x<90 ensures result==x {return left::id(right::id(x));} fn main() {}".to_vec());
    let mut cache = ProofCache::default();
    let a = capture(&tree, Path::new("main.wi"), &mut cache).unwrap();
    let options = verification::Options {
        artifacts: None,
        ..Default::default()
    };
    let same = comparison::compare_typed(&a, &a, &options);
    assert!(same.uncertainty.is_empty(), "{same:?}");
    assert!(
        same.changes
            .iter()
            .all(|c| c.contract_preserved == comparison::Preservation::Proven),
        "{same:?}"
    );
    tree.insert("left.wi".into(), left.replace("100", "99").into_bytes());
    let b = capture(&tree, Path::new("main.wi"), &mut cache).unwrap();
    let changed = comparison::compare_typed(&a, &b, &options);
    let left = changed
        .changes
        .iter()
        .find(|c| c.symbol == "$local::left::id")
        .unwrap();
    assert!(left.proof_dependencies_changed, "{changed:?}");
    assert_eq!(
        left.contract_preserved,
        comparison::Preservation::NotPreserved,
        "{changed:?}"
    );
    let right = changed
        .changes
        .iter()
        .find(|c| c.symbol == "$local::right::id")
        .unwrap();
    assert!(!right.proof_dependencies_changed, "{changed:?}");
    assert_eq!(
        right.contract_preserved,
        comparison::Preservation::Proven,
        "{changed:?}"
    );
}
