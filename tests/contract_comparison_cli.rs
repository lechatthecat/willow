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
    assert_eq!(run(&[], 0)["schema"], "willow.semantic-diff.v3");
    let r = run(&["--schema", "v4", "--protected-base", &commit], 0);
    assert_eq!(r["schema"], "willow.semantic-diff.v4");
    assert_eq!(r["changes"][0]["implementation_verified"], true);
    assert_eq!(r["changes"][0]["contract_preserved"], "PROVEN");
    assert_eq!(run(&["--schema", "v4"], 3)["status"], "incomplete");
    fs::write(
        root.join("main.wi"),
        source.replace("requires x>=0", "requires x>=10"),
    )
    .unwrap();
    assert_eq!(
        run(&["--schema", "v4", "--protected-base", &commit], 1)["changes"][0]["contract_preserved"],
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
                "v4",
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

#[test]
fn contract_repair_cli_exposes_evidence_and_rechecks_fixed_source() {
    let root = std::env::temp_dir().join(format!("willow-contract-repair-{}", std::process::id()));
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
    let source = include_str!("../example/contract_repair.wi");
    fs::write(root.join("main.wi"), source).unwrap();
    git(&root, &["add", "main.wi"]);
    git(&root, &["commit", "-qm", "Initial fixture"]);
    let commit = git(&root, &["rev-parse", "HEAD"]);
    let check = |expected: bool| {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["check"])
            .arg(root.join("main.wi"))
            .env("WILLOW_VERIFICATION_DIR", root.join("artifacts"))
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            expected,
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let run = |expected: i32, missing_solver: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
        command
            .args(["verify-edit", "--root"])
            .arg(&root)
            .args([
                "--base",
                &commit,
                "--schema",
                "v4",
                "--protected-base",
                &commit,
                "--format",
                "json",
            ])
            .env("WILLOW_VERIFICATION_DIR", root.join("artifacts"));
        if missing_solver {
            command.env("WILLOW_Z3", root.join("missing-solver"));
        }
        let output = command.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    check(true);
    let initial = run(0, false);
    let base = &initial["summary"]["verification"]["target"][0];
    assert_eq!(base["status"], "PROVEN");
    assert_eq!(initial["summary"]["verification"]["source_path"], "main.wi");
    fs::write(
        root.join("main.wi"),
        source.replace("if x < 0 { return 0; }", ""),
    )
    .unwrap();
    check(false);
    let broken = run(1, false);
    let evidence = &broken["summary"]["verification"]["target"][0];
    assert_eq!(evidence["schema"], "willow.verification.v1");
    assert_eq!(evidence["status"], "COUNTEREXAMPLE");
    assert_eq!(evidence["declaration_id"], base["declaration_id"]);
    assert_eq!(evidence["contract_digest"], base["contract_digest"]);
    assert_eq!(evidence["input_symbols"]["a0_0"], "x");
    let obligation = evidence["obligations"].as_array().unwrap().last().unwrap();
    assert_eq!(obligation["kind"], "ensures truth");
    assert!(
        obligation["counterexample_candidate"]
            .as_str()
            .unwrap()
            .contains("a0_0")
    );
    assert_eq!(
        evidence["specification_provenance"]["approval_status"],
        "UNVERIFIED"
    );
    for field in [
        "location",
        "analyzer_version",
        "solver_version",
        "verified_scope",
        "dependencies",
        "contract",
    ] {
        assert!(!evidence[field].is_null(), "{field}");
    }
    let unknown = run(3, true);
    let evidence = &unknown["summary"]["verification"]["target"][0];
    assert_eq!(evidence["status"], "UNKNOWN");
    assert!(!evidence["unknown_reasons"].as_array().unwrap().is_empty());
    assert_eq!(evidence["declaration_id"], base["declaration_id"]);
    fs::write(root.join("main.wi"), source).unwrap();
    check(true);
    assert_eq!(run(0, false)["changes"][0]["implementation_verified"], true);
    // A replacement contract can prove the faulty body but still needs independent approval.
    fs::write(
        root.join("main.wi"),
        source
            .replace("if x < 0 { return 0; }", "")
            .replace("result >= 0", "result == x"),
    )
    .unwrap();
    check(true);
    let changed = run(1, false);
    assert_eq!(
        changed["summary"]["approval"]["approval_status"],
        "approval_required"
    );
    assert_eq!(
        changed["summary"]["approval"]["specification_provenance"],
        "UNVERIFIED"
    );
    assert_eq!(changed["changes"][0]["contract_preserved"], "NOT_PRESERVED");
    assert_eq!(
        changed["changes"][0]["input_symbols"]["cmp0_return"],
        "result"
    );
    assert!(
        changed["changes"][0]["obligations"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["counterexample_candidate"].is_string())
    );
}

#[test]
fn contract_comparison_cli_immutable_data_and_quantified_lock() {
    let root = std::env::temp_dir().join(format!(
        "willow-immutable-contract-cli-{}",
        std::process::id()
    ));
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
    let source = "class Limit {pub value:i64;} enum Value { Present(i64), Empty } @pure @verified fn first(a:FrozenArray<i64>, limit:Limit)->i64 requires a.len()>0 requires forall(0,a.len(),|i:i64| a[i]>=limit.value) ensures result>=limit.value {return a[0];} @pure @verified fn decode(v:Value)->i64 ensures result==match v {Value::Present(x)=>x,Value::Empty=>0} {return match v {Value::Present(x)=>x,Value::Empty=>0};} fn main() {}";
    fs::write(root.join("main.wi"), source).unwrap();
    git(&root, &["add", "main.wi"]);
    git(
        &root,
        &["commit", "-qm", "Initial immutable contract fixture"],
    );
    let initial = git(&root, &["rev-parse", "HEAD"]);
    let run = |base: &str, write: bool, expected: i32| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_willow"));
        cmd.args(["verify-edit", "--root"]).arg(&root).args([
            "--base",
            base,
            "--protected-base",
            base,
            "--schema",
            "v4",
            "--format",
            "json",
        ]);
        if write {
            cmd.arg("--write-contracts-lock");
        }
        let output = cmd.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(expected),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let generated = run(&initial, true, 0);
    assert!(
        generated["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["contract_preserved"] == "PROVEN"),
        "{generated}"
    );
    let lock = fs::read(root.join("contracts.lock")).unwrap();
    git(&root, &["add", "contracts.lock"]);
    git(&root, &["commit", "-qm", "Protect immutable contracts"]);
    let protected = git(&root, &["rev-parse", "HEAD"]);
    let unchanged = run(&protected, false, 0);
    assert_eq!(unchanged["summary"]["lock_manifest"]["change"], "unchanged");
    fs::write(
        root.join("main.wi"),
        source.replace("ensures result>=limit.value", "ensures true"),
    )
    .unwrap();
    let changed = run(&protected, false, 3);
    assert!(
        changed["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["contract_preserved"] == "NOT_PRESERVED"),
        "{changed}"
    );
    fs::write(
        root.join("main.wi"),
        source.replace("pub value:i64;", "pub value:i64; pub label:bool;"),
    )
    .unwrap();
    let changed = run(&protected, false, 3);
    assert_eq!(
        changed["summary"]["approval"]["semantic_dependencies_changed"], true,
        "{changed}"
    );
    assert_eq!(fs::read(root.join("contracts.lock")).unwrap(), lock);
}

#[test]
fn io_contract_verify_edit_reports_new_external_effect() {
    let root = std::env::temp_dir().join(format!("willow-io-diff-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
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
    fs::write(root.join("main.wi"), "fn leaf() {} fn main() { leaf(); }").unwrap();
    git(&root, &["add", "main.wi"]);
    git(&root, &["commit", "-qm", "Initial fixture"]);
    let base = git(&root, &["rev-parse", "HEAD"]);
    fs::write(
        root.join("main.wi"),
        "fn leaf() { println(1); } fn main() { leaf(); }",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .args(["verify-edit", "--root"])
        .arg(&root)
        .args(["--base", &base, "--format", "json"])
        .output()
        .unwrap();
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        result["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| change["effects"]["added"]
                .as_array()
                .is_some_and(|effects| effects.iter().any(|effect| effect == "may-io"))),
        "{result}"
    );
}

#[test]
fn verify_edit_panic_allocation_contract_changes() {
    let root = std::env::temp_dir().join(format!("willow-effect-changes-{}", std::process::id()));
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
    for (before, after, change) in [
        ("", "no_panic", "added"),
        ("no_panic", "", "removed"),
        ("", "no_alloc", "added"),
        ("no_alloc", "", "removed"),
        ("no_panic", "no_panic,no_alloc", "strengthened"),
        ("no_alloc", "no_panic,no_alloc", "strengthened"),
        ("no_panic,no_alloc", "no_panic", "weakened"),
        ("no_panic,no_alloc", "no_alloc", "weakened"),
    ] {
        let source = |c: &str| {
            format!(
                "{} fn f() {{}} fn main() {{}}",
                if c.is_empty() {
                    String::new()
                } else {
                    format!("@effects({c})")
                }
            )
        };
        fs::write(root.join("main.wi"), source(before)).unwrap();
        git(&root, &["add", "main.wi"]);
        let staged_files = git(&root, &["diff", "--cached", "--name-only"]);
        if !staged_files.is_empty() {
            assert_eq!(staged_files, "main.wi");
            let staged = git(&root, &["diff", "--cached"]);
            assert!(staged.contains("fn f()"));
            git(&root, &["commit", "-qm", "Contract baseline"]);
        }
        fs::write(root.join("main.wi"), source(after)).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["verify-edit", "--root"])
            .arg(&root)
            .args(["--base", "HEAD", "--format", "json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            result["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c["contract"]["change"] == change),
            "{before} -> {after}: {result}"
        );
    }
}

#[test]
fn modular_verification_cli_dependency_changes_and_unknown_exit() {
    let root = std::env::temp_dir().join(format!(
        "willow-modular-contract-cli-{}",
        std::process::id()
    ));
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
    for file in ["main.wi", "bounds.wi", "arithmetic.wi"] {
        fs::copy(
            Path::new("example/modular_verification").join(file),
            root.join(file),
        )
        .unwrap();
    }
    git(&root, &["add", "main.wi", "bounds.wi", "arithmetic.wi"]);
    git(&root, &["commit", "-qm", "Modular example"]);
    let base = git(&root, &["rev-parse", "HEAD"]);
    let run = |expected| {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["verify-edit", "--root"])
            .arg(&root)
            .args([
                "--base",
                &base,
                "--protected-base",
                &base,
                "--schema",
                "v4",
                "--format",
                "json",
            ])
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
    let same = run(0);
    assert!(
        same["changes"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["verification"]["target"] == "PROVEN")
            .count()
            >= 2
    );
    let bounds = fs::read_to_string(root.join("bounds.wi")).unwrap();
    fs::write(root.join("bounds.wi"), bounds.replace("x >= 0", "x > 0")).unwrap();
    let changed = run(1);
    assert!(
        changed["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["symbol"] == "twice" && r["proof_dependencies_changed"] == true)
    );
    fs::write(
        root.join("bounds.wi"),
        "@pure pub fn small(x:i64)->bool {return small(x);}",
    )
    .unwrap();
    let unknown = run(3);
    assert_eq!(unknown["status"], "incomplete");
    assert!(
        unknown["summary"]["verification"]["target"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["status"] == "UNKNOWN")
    );
}

#[test]
fn modular_verification_cli_unchanged_opaque_helpers_preserve_contracts() {
    let root =
        std::env::temp_dir().join(format!("willow-opaque-helper-cli-{}", std::process::id()));
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
    let module = "@pure fn a(x:i64)->bool {return x>=0;} @pure fn b(x:i64)->bool {return x<=10;} @pure @verified pub fn helper(x:i64)->bool {return a(x) && b(x);} @pure @verified pub fn positive(x:i64)->bool {return x>=0;} @pure @verified pub fn bounded(x:i64)->i64 ensures result>=0 {return 1;}";
    fs::write(root.join("math.wi"), module).unwrap();
    fs::write(root.join("main.wi"), "import math; @pure @verified fn caller(x:i64)->i64 requires math::positive(x) ensures result==x {return x;} @pure @verified fn bounded(x:i64)->i64 requires math::bounded(x)>0 ensures result==x {return x;} @pure @verified fn reordered(x:i64)->i64 requires math::helper(x) ensures result==x {return x;} fn main() {}").unwrap();
    git(&root, &["add", "main.wi", "math.wi"]);
    git(&root, &["commit", "-qm", "Opaque helper contracts"]);
    let base = git(&root, &["rev-parse", "HEAD"]);
    let run = |expected| {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["verify-edit", "--root"])
            .arg(&root)
            .args([
                "--base",
                &base,
                "--protected-base",
                &base,
                "--schema",
                "v4",
                "--format",
                "json",
            ])
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
    let same = run(0);
    for caller in ["caller", "bounded", "reordered"] {
        let change = same["changes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["symbol"] == caller)
            .unwrap();
        assert_eq!(change["contract_preserved"], "PROVEN");
        assert_eq!(change["implementation_verified"], true);
        assert_eq!(change["proof_dependencies_changed"], false);
    }
    let swapped = module.replace(
        "@pure fn a(x:i64)->bool {return x>=0;} @pure fn b(x:i64)->bool {return x<=10;}",
        "@pure fn b(x:i64)->bool {return x<=10;} @pure fn a(x:i64)->bool {return x>=0;}",
    );
    fs::write(root.join("math.wi"), swapped).unwrap();
    let reordered = run(0);
    for change in reordered["changes"].as_array().unwrap() {
        assert_eq!(change["contract_preserved"], "PROVEN", "{change}");
        assert_eq!(change["proof_dependencies_changed"], false, "{change}");
    }
    assert_eq!(
        reordered["summary"]["proof_cache"]["misses"],
        same["summary"]["proof_cache"]["misses"]
    );
    assert_eq!(
        reordered["summary"]["proof_cache"]["hits"],
        same["summary"]["proof_cache"]["hits"]
    );
    // Same contracts cannot justify coupling different implementations.
    fs::write(root.join("math.wi"), module.replace("x>=0", "x>0")).unwrap();
    let changed = run(1);
    let caller = changed["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["symbol"] == "caller")
        .unwrap();
    assert_eq!(caller["contract_preserved"], "NOT_PRESERVED");
    assert_eq!(caller["proof_dependencies_changed"], true);
}
