//! Exercise production initialization, which unit-test symbol contexts can mask.
use std::{fs, path::Path, process::Command};
fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
#[test]
fn contract_comparison_cli_initializes_checked_context_and_preserves_versions() {
    let root = std::env::temp_dir().join(format!("willow-contract-cli-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.name", "Fixture"]);
    git(&root, &["config", "user.email", "fixture@example.invalid"]);
    let source =
        "@pure @verified fn f(x:i64)->i64 requires x>=0 ensures result>=0 {return x;} fn main() {}";
    fs::write(root.join("main.wi"), source).unwrap();
    git(&root, &["add", "main.wi"]);
    git(&root, &["commit", "-qm", "Initial fixture"]);
    let commit = git(&root, &["rev-parse", "HEAD"]);
    let run = |extra: &[&str], expected: i32| {
        let o = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["verify-edit", "--root"])
            .arg(&root)
            .args(["--base", &commit, "--format", "json"])
            .args(extra)
            .output()
            .unwrap();
        assert_eq!(
            o.status.code(),
            Some(expected),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&o.stdout).unwrap()
    };
    assert_eq!(run(&[], 0)["schema"], "willow.semantic-diff.v1");
    let r = run(&["--schema", "v2", "--protected-base", &commit], 0);
    assert_eq!(r["schema"], "willow.semantic-diff.v2");
    assert_eq!(r["changes"][0]["implementation_verified"], true);
    assert_eq!(r["changes"][0]["contract_preserved"], "PROVEN");
    assert_eq!(run(&["--schema", "v2"], 3)["status"], "incomplete");
    fs::write(
        root.join("main.wi"),
        source.replace("requires x>=0", "requires x>=10"),
    )
    .unwrap();
    assert_eq!(
        run(&["--schema", "v2", "--protected-base", &commit], 1)["changes"][0]["contract_preserved"],
        "NOT_PRESERVED"
    );
}

#[test]
fn contract_comparison_cli_protected_manifest_and_semantic_status() {
    let root =
        std::env::temp_dir().join(format!("willow-contract-policy-cli-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    struct Cleanup(std::path::PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    git(&root, &["init", "-q"]);
    git(&root, &["config", "user.name", "Fixture"]);
    git(&root, &["config", "user.email", "fixture@example.invalid"]);
    let source =
        "const LIMIT:i64=1; @pure fn f()->i64 ensures (result>=LIMIT) {return LIMIT;} fn main() {}";
    fs::write(root.join("main.wi"), source).unwrap();
    git(&root, &["add", "main.wi"]);
    git(&root, &["commit", "-qm", "Initial fixture"]);
    let run = |commit: &str, extra: &[&str], expected: i32| {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["verify-edit", "--root"])
            .arg(&root)
            .args([
                "--base",
                commit,
                "--protected-base",
                commit,
                "--schema",
                "v2",
                "--format",
                "json",
            ])
            .args(extra)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let initial = git(&root, &["rev-parse", "HEAD"]);
    run(&initial, &["--write-contracts-lock"], 0);
    let lock = fs::read(root.join("contracts.lock")).unwrap();
    git(&root, &["add", "contracts.lock"]);
    git(&root, &["commit", "-qm", "Protect contract manifest"]);
    let protected = git(&root, &["rev-parse", "HEAD"]);
    let unchanged = run(&protected, &[], 0);
    assert_eq!(unchanged["summary"]["lock_manifest"]["change"], "unchanged");
    fs::remove_file(root.join("contracts.lock")).unwrap();
    let removed = run(&protected, &[], 1);
    assert_eq!(
        removed["summary"]["approval"]["approval_status"],
        "approval_required"
    );
    assert_eq!(
        removed["summary"]["approval"]["contract_status"],
        "contract_digest_unchanged"
    );
    assert_eq!(
        removed["summary"]["approval"]["semantic_dependencies_changed"],
        false
    );
    assert_eq!(removed["summary"]["lock_manifest"]["change"], "removed");
    assert_eq!(
        removed["summary"]["approval"]["binding"]["protected_lock_present"],
        true
    );
    assert_eq!(
        removed["summary"]["approval"]["binding"]["target_lock_present"],
        false
    );
    fs::write(root.join("contracts.lock"), &lock).unwrap();
    assert_eq!(
        run(&protected, &[], 0)["summary"]["approval"]["approval_status"],
        "not_required"
    );
    for (edited, dependency_changed) in [
        (source.replace("LIMIT:i64=1", "LIMIT:i64=2"), true),
        (source.replace("result>=LIMIT", "result==LIMIT"), false),
    ] {
        fs::write(root.join("main.wi"), edited).unwrap();
        fs::write(root.join("contracts.lock"), &lock).unwrap();
        // Repairing local consistency must not establish policy approval.
        run(&protected, &["--write-contracts-lock"], 3);
        let report = run(&protected, &[], 1);
        assert_eq!(
            report["summary"]["approval"]["contract_status"],
            "contract_changed"
        );
        assert_eq!(
            report["summary"]["approval"]["semantic_dependencies_changed"],
            dependency_changed
        );
        assert_eq!(
            report["summary"]["approval"]["approval_status"],
            "approval_required"
        );
        assert_eq!(report["summary"]["lock_manifest"]["change"], "unchanged");
        assert_eq!(report["changes"][0]["contract_preserved"], "PROVEN");
    }
}
