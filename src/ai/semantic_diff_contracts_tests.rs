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
