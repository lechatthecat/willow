use super::*;
use std::{fs, path::PathBuf, process::Command};

fn captures(a: &str, b: &str) -> (Capture, Capture) {
    let scratch = tree::Scratch::new().unwrap();
    let options = Options {
        root: scratch.0.clone(),
        entry: Some("main.wi".into()),
        project: false,
        base: String::new(),
        fail_on_growth: false,
    };
    let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
    fs::write(scratch.0.join("main.wi"), a).unwrap();
    let before = analyze(&options, &scratch, &mut revision, Path::new("main.wi")).unwrap();
    fs::write(scratch.0.join("main.wi"), b).unwrap();
    let after = analyze(&options, &scratch, &mut revision, Path::new("main.wi")).unwrap();
    (before, after)
}
fn report(a: &str, b: &str) -> SemanticDiff {
    let (a, b) = captures(a, b);
    compare(&a, &b, false).unwrap()
}
#[test]
fn semantic_diff_identical_and_digest_contract() {
    let r = report("pub fn main() {}", "pub fn main() {}");
    assert_eq!(r.schema, "willow.semantic-diff.v1");
    assert_eq!(r.exit_code(), 0);
    assert!(r.changes.is_empty());
}
#[test]
fn semantic_diff_public_signature_add_remove_and_rename() {
    for (a, b, kind) in [
        ("fn main() {}", "pub fn f() {} fn main() {}", "added"),
        ("pub fn f() {} fn main() {}", "fn main() {}", "removed"),
        (
            "pub fn f() -> i64 { return 1; } fn main() {}",
            "pub fn f() -> bool { return true; } fn main() {}",
            "signature-changed",
        ),
    ] {
        let r = report(a, b);
        assert_eq!(r.exit_code(), 0, "{r:?}");
        assert!(r.changes.iter().any(|v| v["public_api"] == kind), "{r:?}");
    }
    let r = report("pub fn f() {} fn main() {}", "pub fn g() {} fn main() {}");
    assert_eq!(
        r.changes.iter().filter(|v| v["match"] == "added").count(),
        1
    );
    assert_eq!(
        r.changes.iter().filter(|v| v["match"] == "removed").count(),
        1
    );
}
#[test]
fn semantic_diff_contract_strength_and_weakness() {
    for (a, b, kind) in [
        ("", "no_wait", "added"),
        ("no_wait", "", "removed"),
        ("no_block", "no_wait", "strengthened"),
        ("no_wait", "no_block", "weakened"),
        ("no_block", "no_suspend", "changed"),
    ] {
        let src = |c: &str| {
            format!(
                "{} fn f() {{}} fn main() {{}}",
                if c.is_empty() {
                    String::new()
                } else {
                    format!("@effects({c})")
                }
            )
        };
        let r = report(&src(a), &src(b));
        assert_eq!(r.exit_code(), 0, "{r:?}");
        assert!(
            r.changes.iter().any(|v| v["contract"]["change"] == kind),
            "{r:?}"
        );
    }
}
#[test]
fn semantic_diff_existing_and_new_contract_violations() {
    let bad = "@effects(no_wait) fn f(c: BlockingCell<i64>) { c.get(); } fn main() {}";
    for a in [
        "fn main() {}",
        "fn f(c: BlockingCell<i64>) { c.get(); } fn main() {}",
        bad,
    ] {
        let r = report(a, bad);
        assert_eq!(r.exit_code(), 1, "{r:?}");
        assert!(r.summary["contract_violations"].as_u64().unwrap() > 0);
    }
    let r = report(bad, "fn main() {}");
    assert_eq!(
        r.exit_code(),
        1,
        "baseline violation must remain visible: {r:?}"
    );
}
#[test]
fn semantic_diff_helper_growth_policy_and_witness() {
    let a = "fn leaf(c: BlockingCell<i64>) {} fn middle(c: BlockingCell<i64>) { leaf(c); } fn main() {}";
    let b = a.replace(
        "fn leaf(c: BlockingCell<i64>) {}",
        "fn leaf(c: BlockingCell<i64>) { c.get(); }",
    );
    let (before, after) = captures(a, &b);
    let r = compare(&before, &after, false).unwrap();
    assert_eq!(r.exit_code(), 0, "{r:?}");
    let c = r
        .changes
        .iter()
        .find(|v| v["symbol"].as_str().unwrap().contains("::middle"))
        .unwrap();
    assert!(
        c["effects"]["added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "may-block"),
        "{r:?}"
    );
    assert!(
        c["cause"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["chain"].as_array().unwrap().len() >= 2),
        "{r:?}"
    );
    assert_eq!(compare(&before, &after, true).unwrap().exit_code(), 1);
    assert_eq!(
        serde_json::to_value(&r).unwrap(),
        serde_json::to_value(compare(&before, &after, false).unwrap()).unwrap()
    );
    assert!(r.human(true).contains("cause:"));
}
#[test]
fn semantic_diff_removal_and_uncertainty() {
    let a = "fn f(c: BlockingCell<i64>) { c.get(); } fn main() {}";
    let r = report(a, "fn f(c: BlockingCell<i64>) {} fn main() {}");
    assert!(
        r.changes
            .iter()
            .any(|v| !v["effects"]["removed"].as_array().unwrap().is_empty())
    );
    let r = report("fn main() {}", "fn main() { let n: i64 = true; }");
    assert_eq!(r.exit_code(), 3, "{r:?}");
}
#[test]
fn semantic_diff_contracts_on_members() {
    let r = report(
        "pub class A { pub fn f(self) {} } fn main() {}",
        "pub class A { @effects(no_wait) pub fn f(self) {} } fn main() {}",
    );
    assert!(
        r.changes.iter().any(|v| v["contract"]["change"] == "added"),
        "{r:?}"
    );
}
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
    String::from_utf8(output.stdout).unwrap()
}
fn repository() -> tree::Scratch {
    let dir = tree::Scratch::new().unwrap();
    git(&dir.0, &["init", "-q"]);
    git(&dir.0, &["config", "user.name", "Verification fixture"]);
    git(&dir.0, &["config", "user.email", "fixture@invalid"]);
    fs::write(dir.0.join("main.wi"), "pub fn main() {}").unwrap();
    git(&dir.0, &["add", "main.wi"]);
    git(&dir.0, &["commit", "-qm", "Initial fixture"]);
    dir
}
fn options(root: &Path) -> Options {
    Options {
        root: root.to_owned(),
        entry: Some(PathBuf::from("main.wi")),
        project: false,
        base: resolve_base(root, "HEAD").unwrap(),
        fail_on_growth: false,
    }
}
#[test]
fn semantic_diff_git_uncommitted_input_read_only_and_deterministic() {
    let dir = repository();
    let opts = options(&dir.0);
    fs::write(dir.0.join("main.wi"), "pub fn main() { println(1); }").unwrap();
    let status = git(&dir.0, &["status", "--porcelain"]);
    let r = verify(&opts).unwrap();
    assert_eq!(r.exit_code(), 0, "{r:?}");
    assert_ne!(r.base["digest"], r.target["digest"]);
    assert_eq!(git(&dir.0, &["status", "--porcelain"]), status);
    assert_eq!(
        serde_json::to_value(&r).unwrap(),
        serde_json::to_value(verify(&opts).unwrap()).unwrap()
    );
    assert!(
        !serde_json::to_string(&r)
            .unwrap()
            .contains("willow-semantic-input-")
    );
}
#[test]
fn semantic_diff_git_invalid_revision_and_deleted_entry() {
    let dir = repository();
    assert!(resolve_base(&dir.0, "--help").is_err());
    assert!(resolve_base(&dir.0, "absent-revision").is_err());
    let opts = options(&dir.0);
    fs::remove_file(dir.0.join("main.wi")).unwrap();
    assert_eq!(verify(&opts).unwrap().exit_code(), 3);
}
#[test]
fn semantic_diff_git_untracked_module_and_ignored_exclusion() {
    let dir = repository();
    fs::write(dir.0.join("extra.wi"), "pub fn f() {}").unwrap();
    fs::write(dir.0.join(".gitignore"), "ignored/\n").unwrap();
    fs::create_dir(dir.0.join("ignored")).unwrap();
    fs::write(dir.0.join("ignored/private.wi"), "invalid").unwrap();
    let tree = tree::target(&dir.0).unwrap();
    assert!(tree.contains_key("extra.wi"));
    assert!(!tree.contains_key("ignored/private.wi"));
}
#[test]
fn semantic_diff_git_rejects_source_symlink() {
    let dir = repository();
    // A Git symlink is independent of host symlink permissions.
    let blob = git(&dir.0, &["hash-object", "-w", "main.wi"]);
    git(
        &dir.0,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("120000,{},alias.wi", blob.trim()),
        ],
    );
    let tree = git(&dir.0, &["write-tree"]);
    assert!(tree::base(&dir.0, tree.trim()).is_err());
}

#[test]
fn semantic_diff_async_creation_does_not_suspend_caller() {
    let a = "async fn pause() {} fn make() -> Task<void> { return pause(); } fn main() {}";
    let b = "async fn pause() { await sleep(1); } fn make() -> Task<void> { return pause(); } fn main() {}";
    let r = report(a, b);
    if let Some(make) = r
        .changes
        .iter()
        .find(|v| v["symbol"].as_str().unwrap().contains("::make"))
    {
        assert!(
            !make["effects"]["added"]
                .as_array()
                .unwrap()
                .contains(&json!("may-suspend")),
            "{r:?}"
        );
    }
}
#[test]
fn semantic_diff_affected_semantic_work_scales_with_edit() {
    let mut counts = Vec::new();
    for n in [8, 32, 128] {
        let scratch = tree::Scratch::new().unwrap();
        let opts = Options {
            root: scratch.0.clone(),
            entry: Some("main.wi".into()),
            project: false,
            base: String::new(),
            fail_on_growth: false,
        };
        let source = format!(
            "fn edited() -> i64 {{ return 1; }} fn main() {{ edited(); }} {}",
            (0..n)
                .map(|i| format!("fn other{i}() -> i64 {{ return {i}; }}"))
                .collect::<String>()
        );
        fs::write(scratch.0.join("main.wi"), &source).unwrap();
        let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
        let a = analyze(&opts, &scratch, &mut revision, Path::new("main.wi")).unwrap();
        fs::write(
            scratch.0.join("main.wi"),
            source.replacen("return 1;", "return 2;", 1),
        )
        .unwrap();
        let b = analyze(&opts, &scratch, &mut revision, Path::new("main.wi")).unwrap();
        let warm = compare(&a, &b, false).unwrap();
        let mut cold = crate::compiler_db::revision::AnalysisRevision::default();
        let fresh = analyze(&opts, &scratch, &mut cold, Path::new("main.wi")).unwrap();
        assert_eq!(
            serde_json::to_value(warm).unwrap(),
            serde_json::to_value(compare(&a, &fresh, false).unwrap()).unwrap()
        );
        eprintln!("semantic-work n={n} stats={}", revision.incremental_stats);
        counts.push(revision.typechecks);
    }
    assert!(counts.iter().all(|c| *c == counts[0]), "{counts:?}");
}
#[test]
fn semantic_diff_project_and_imported_helper() {
    let dir = repository();
    fs::write(
        dir.0.join("project.toml"),
        "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\nentry = \"main.wi\"\n",
    )
    .unwrap();
    fs::write(
        dir.0.join("helper.wi"),
        "pub fn leaf(c: BlockingCell<i64>) {}",
    )
    .unwrap();
    fs::write(
        dir.0.join("main.wi"),
        "import helper; fn caller(c: BlockingCell<i64>) { helper::leaf(c); } fn main() {}",
    )
    .unwrap();
    git(&dir.0, &["add", "project.toml", "helper.wi", "main.wi"]);
    git(&dir.0, &["commit", "-qm", "Project fixture"]);
    let mut opts = options(&dir.0);
    opts.project = true;
    fs::write(
        dir.0.join("helper.wi"),
        "pub fn leaf(c: BlockingCell<i64>) { c.get(); }",
    )
    .unwrap();
    let r = verify(&opts).unwrap();
    assert_eq!(r.exit_code(), 0, "{r:?}");
    assert!(
        r.changes
            .iter()
            .any(|v| v["symbol"].as_str().unwrap().contains("::caller")
                && !v["effects"]["added"].as_array().unwrap().is_empty()),
        "{r:?}"
    );
}
#[test]
fn semantic_diff_missing_source_is_incomplete() {
    let dir = repository();
    let opts = options(&dir.0);
    fs::write(dir.0.join("main.wi"), "import absent; fn main() {}").unwrap();
    let r = verify(&opts).unwrap_or_else(|e| SemanticDiff::incomplete(e.to_string()));
    assert_eq!(r.exit_code(), 3);
}
#[test]
fn semantic_diff_indirect_targets_change() {
    let a = "fn a() {} fn b() {} fn invoke(f: fn()) { f(); } fn main() { invoke(a); }";
    let b = a.replace("invoke(a)", "invoke(b)");
    let r = report(a, &b);
    assert!(
        r.changes
            .iter()
            .any(|v| v["targets"]["base"] != v["targets"]["target"]),
        "{r:?}"
    );
}

#[test]
fn semantic_diff_ambiguous_symbols_are_not_paired() {
    let (mut a, mut b) = captures("fn main() {}", "fn main() {}");
    a.ambiguous.insert("main.wi::function::main".into());
    b.ambiguous.insert("main.wi::function::main".into());
    let r = compare(&a, &b, true).unwrap();
    assert_eq!(r.exit_code(), 3);
    assert_eq!(r.changes.len(), 1);
    assert_eq!(r.changes[0]["match"], "ambiguous");
    assert!(r.changes[0]["effects"].is_null());
}
#[test]
fn semantic_diff_generic_contract_is_reported() {
    let r = report(
        "fn identity<T>(x:T) -> T { return x; } fn main() { identity(1); }",
        "@effects(no_wait) fn identity<T>(x:T) -> T { return x; } fn main() { identity(1); }",
    );
    assert!(
        r.changes.iter().any(|v| v["contract"]["change"] == "added"),
        "{r:?}"
    );
}
#[test]
fn semantic_diff_shifted_witness_locations_are_relative() {
    let a = "fn f(c: BlockingCell<i64>) {} fn main() {}";
    let b = "\n\nfn f(c: BlockingCell<i64>) { c.get(); } fn main() {}";
    let r = report(a, b);
    let locations: Vec<_> = r
        .changes
        .iter()
        .flat_map(|v| v["cause"].as_array().unwrap())
        .filter(|v| !v["location"].is_null())
        .collect();
    assert!(!locations.is_empty());
    assert!(locations.iter().all(|v| v["location"]["path"] == "main.wi"));
    assert!(
        locations
            .iter()
            .any(|v| v["location"]["start"].as_u64().unwrap() >= 30)
    );
}
#[test]
fn semantic_diff_failure_preserves_digests() {
    let dir = repository();
    let opts = options(&dir.0);
    fs::write(dir.0.join("main.wi"), "fn main( {").unwrap();
    let r = verify(&opts).unwrap();
    assert_eq!(r.exit_code(), 3);
    assert!(r.base["digest"].is_string());
    assert!(r.target["digest"].is_string());
    assert!(
        !serde_json::to_string(&r)
            .unwrap()
            .contains("willow-semantic-input-")
    );
}
#[test]
fn semantic_diff_subdirectory_workspace() {
    let dir = repository();
    fs::create_dir(dir.0.join("app")).unwrap();
    fs::write(dir.0.join("app/main.wi"), "fn main() {}").unwrap();
    git(&dir.0, &["add", "app/main.wi"]);
    git(&dir.0, &["commit", "-qm", "Nested fixture"]);
    let root = dir.0.join("app");
    let opts = options(&root);
    fs::write(root.join("main.wi"), "fn main() { println(1); }").unwrap();
    assert_eq!(verify(&opts).unwrap().exit_code(), 0);
}
#[test]
fn semantic_diff_fanout_and_cycles_have_finite_paths() {
    let a = "fn leaf(c: BlockingCell<i64>) {} fn left(c: BlockingCell<i64>) { leaf(c); } fn right(c: BlockingCell<i64>) { leaf(c); } fn root(c: BlockingCell<i64>) { left(c); right(c); } fn main() {}";
    let r = report(
        a,
        &a.replace(
            "fn leaf(c: BlockingCell<i64>) {}",
            "fn leaf(c: BlockingCell<i64>) { c.get(); }",
        ),
    );
    let leaf = r
        .changes
        .iter()
        .find(|v| v["symbol"].as_str().unwrap().contains("::leaf"))
        .unwrap();
    assert_eq!(leaf["impacted_callers"].as_array().unwrap().len(), 3);
    let a =
        "fn a(c: BlockingCell<i64>) { b(c); } fn b(c: BlockingCell<i64>) { a(c); } fn main() {}";
    let r = report(a, &a.replace("a(c);", "c.get(); a(c);"));
    assert!(r.changes.iter().all(|v| {
        v["cause"]
            .as_array()
            .unwrap()
            .iter()
            .all(|c| c["chain"].as_array().unwrap().len() <= 2)
    }));
}

#[test]
fn semantic_diff_digest_check_detects_changes_additions_and_removals() {
    let dir = repository();
    let digest = hash_serialized(&tree::target(&dir.0).unwrap()).unwrap();
    for case in ["change", "add", "remove"] {
        fs::write(dir.0.join("main.wi"), "pub fn main() {}").unwrap();
        let _ = fs::remove_file(dir.0.join("extra.wi"));
        match case {
            "change" => fs::write(dir.0.join("main.wi"), "fn main() {}").unwrap(),
            "add" => fs::write(dir.0.join("extra.wi"), "pub fn extra() {}").unwrap(),
            _ => fs::remove_file(dir.0.join("main.wi")).unwrap(),
        }
        let mut report = SemanticDiff::incomplete(String::new());
        report.status = "complete";
        check_target(&mut report, &dir.0, &digest).unwrap();
        assert_eq!(report.exit_code(), 3, "{case}");
        assert!(
            report.summary["uncertainty"]
                .as_array()
                .unwrap()
                .contains(&json!("snapshot changed during verification"))
        );
    }
}
#[test]
fn semantic_diff_witness_storage_is_linear_on_deep_chains() {
    for n in [8, 32, 128] {
        let mut source = "fn leaf(c: BlockingCell<i64>) { c.get(); } fn main() {}".to_owned();
        for i in 0..n {
            let callee = if i == 0 {
                "leaf".to_owned()
            } else {
                format!("node{}", i - 1)
            };
            source.push_str(&format!(
                "fn node{i}(c: BlockingCell<i64>) {{ {callee}(c); }}"
            ));
        }
        let (capture, _) = captures(&source, &source);
        let edges = capture
            .causes
            .next
            .keys()
            .filter(|(s, _)| s.starts_with("main.wi"))
            .count();
        assert!(edges <= 6 * n, "{n}: {edges}");
        let chain = capture.causes.render(
            &format!("main.wi::function::node{}#0", n - 1),
            RuntimeEffects::MAY_BLOCK.bits(),
        );
        assert_eq!(chain["chain"].as_array().unwrap().len(), n + 1);
        eprintln!("witness-storage n={n} next_hops={edges}");
    }
}

#[test]
fn semantic_diff_public_visibility_and_equivalent_contracts() {
    for (a, b, status) in [
        ("pub fn f() {}", "fn f() {}", "removed"),
        ("fn f() {}", "pub fn f() {}", "added"),
    ] {
        let r = report(
            &format!("{a} fn main() {{}}"),
            &format!("{b} fn main() {{}}"),
        );
        assert!(r.changes.iter().any(|v| v["public_api"] == status), "{r:?}");
    }
    let r = report(
        "@effects(no_wait) fn main() {}",
        "@effects(no_block, no_suspend) fn main() {}",
    );
    assert!(
        r.changes
            .iter()
            .any(|v| v["contract"]["change"] == "equivalent"),
        "{r:?}"
    );
}

#[test]
fn semantic_diff_shared_caller_work_on_dense_graphs() {
    let (mut a, mut b) = captures("fn main() {}", "fn main() {}");
    for n in [16usize, 64, 128] {
        a.baseline.callers.clear();
        b.baseline.callers.clear();
        for i in 0..n {
            a.baseline.callers.insert(
                format!("node{i:03}"),
                ((i + 1)..n).map(|j| format!("node{j:03}")).collect(),
            );
        }
        let mut changes: Vec<_> = (0..n)
            .map(|i| json!({"symbol":format!("node{i:03}"),"match":"exact","impacted_callers":[]}))
            .collect();
        let unions = impact::assign(&mut changes, &a.baseline, &b.baseline).unwrap();
        assert_eq!(unions, n * (n - 1) / 2 * n.div_ceil(64));
        for (i, c) in changes.iter().enumerate() {
            assert_eq!(c["impacted_callers"].as_array().unwrap().len(), n - i - 1);
        }
        eprintln!("caller-work n={n} word_unions={unions}");
    }
}

#[test]
fn semantic_diff_failed_target_preserves_baseline_contract_errors() {
    let dir = repository();
    fs::write(
        dir.0.join("main.wi"),
        "@effects(no_wait) fn f(c: BlockingCell<i64>) { c.get(); } fn main() {}",
    )
    .unwrap();
    git(&dir.0, &["add", "main.wi"]);
    git(&dir.0, &["commit", "-qm", "Contract fixture"]);
    let opts = options(&dir.0);
    fs::write(dir.0.join("main.wi"), "fn main( {").unwrap();
    let r = verify(&opts).unwrap();
    assert_eq!(r.exit_code(), 3);
    assert!(r.summary["contract_violations"].as_u64().unwrap() > 0);
    assert!(
        r.summary["diagnostics"]["base"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "E2610")
    );
}

#[test]
fn semantic_diff_virtual_dispatch_target_set_changes() {
    let a = "open class Base { pub open fn run(self) {} } class A extends Base { pub override fn run(self) {} } fn caller(b: Base) { b.run(); } fn main() { let b: Base = new A(); caller(b); }";
    let b = format!("{a} class B extends Base {{ pub override fn run(self) {{ println(1); }} }}");
    let r = report(a, &b);
    assert!(
        r.changes
            .iter()
            .any(|v| v["match"] == "exact" && v["targets"]["base"] != v["targets"]["target"]),
        "{r:?}"
    );
}

#[test]
fn semantic_diff_review_manifest_deletion_preserves_baseline_scope() {
    let dir = repository();
    fs::write(
        dir.0.join("project.toml"),
        "[project]\nname = \"fixture\"\nversion = \"0.1.0\"\nentry = \"main.wi\"\n",
    )
    .unwrap();
    fs::write(
        dir.0.join("unimported.wi"),
        "@effects(no_wait) pub fn bad(c: BlockingCell<i64>) { c.get(); }",
    )
    .unwrap();
    git(&dir.0, &["add", "project.toml", "unimported.wi"]);
    git(&dir.0, &["commit", "-qm", "Project discovery fixture"]);
    let opts = options(&dir.0); // CLI without --project after manifest deletion.
    fs::remove_file(dir.0.join("project.toml")).unwrap();
    let r = verify(&opts).unwrap();
    assert_eq!(r.exit_code(), 3, "{r:?}");
    assert!(
        r.summary["contract_violations"].as_u64().unwrap() > 0,
        "{r:?}"
    );
    assert!(
        r.summary["diagnostics"]["base"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "E2610"
                && d["locations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|l| l["path"] == "unimported.wi")),
        "{r:?}"
    );
    assert!(
        r.summary["uncertainty"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.contains("scope"))),
        "{r:?}"
    );
}

#[test]
fn semantic_diff_review_nested_entry_uses_captured_root() {
    let dir = repository();
    fs::create_dir(dir.0.join("src")).unwrap();
    fs::write(
        dir.0.join("src/main.wi"),
        "import helper; fn caller(c: BlockingCell<i64>) { helper::leaf(c); } fn main() {}",
    )
    .unwrap();
    fs::write(
        dir.0.join("src/helper.wi"),
        "@effects(no_wait) pub fn leaf(c: BlockingCell<i64>) {}",
    )
    .unwrap();
    git(&dir.0, &["add", "src/main.wi", "src/helper.wi"]);
    git(&dir.0, &["commit", "-qm", "Nested entry fixture"]);
    let mut opts = options(&dir.0);
    opts.entry = Some("src/main.wi".into());
    fs::write(
        dir.0.join("src/helper.wi"),
        "@effects(no_wait) pub fn leaf(c: BlockingCell<i64>) { c.get(); }",
    )
    .unwrap();
    let r = verify(&opts).unwrap();
    assert_eq!(r.exit_code(), 1, "{r:?}");
    let leaf = r
        .changes
        .iter()
        .find(|v| v["symbol"] == "src/helper.wi::function::leaf#0")
        .expect("root-relative helper identity");
    assert!(
        leaf["impacted_callers"]
            .as_array()
            .unwrap()
            .contains(&json!("src/main.wi::function::caller#0"))
    );
    let caller = r
        .changes
        .iter()
        .find(|v| v["symbol"] == "src/main.wi::function::caller#0")
        .unwrap();
    assert!(
        caller["cause"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cause| cause["location"]["path"] == "src/helper.wi"
                && cause["chain"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("src/helper.wi::function::leaf#0"))),
        "{r:?}"
    );
    assert!(
        r.summary["diagnostics"]["target"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "E2610"
                && d["locations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|l| l["path"].as_str().is_some_and(|s| s.starts_with("src/")))),
        "{r:?}"
    );
    assert!(
        !serde_json::to_string(&r)
            .unwrap()
            .contains("willow-semantic-input-")
    );
}

#[test]
fn semantic_diff_review_manifest_entry_change_preserves_original_scope() {
    let dir = repository();
    for folder in ["old", "new"] {
        fs::create_dir(dir.0.join(folder)).unwrap();
        fs::write(dir.0.join(folder).join("main.wi"), "fn main() {}").unwrap();
    }
    fs::write(
        dir.0.join("old/bad.wi"),
        "@effects(no_wait) pub fn bad(c: BlockingCell<i64>) { c.get(); }",
    )
    .unwrap();
    let manifest = |entry: &str| {
        format!("[project]\nname = \"fixture\"\nversion = \"0.1.0\"\nentry = \"{entry}\"\n")
    };
    fs::write(dir.0.join("project.toml"), manifest("old/main.wi")).unwrap();
    git(
        &dir.0,
        &[
            "add",
            "old/main.wi",
            "old/bad.wi",
            "new/main.wi",
            "project.toml",
        ],
    );
    git(&dir.0, &["commit", "-qm", "Entry scope fixture"]);
    let mut opts = options(&dir.0);
    opts.entry = None;
    fs::write(dir.0.join("project.toml"), manifest("new/main.wi")).unwrap();
    let r = verify(&opts).unwrap();
    assert_eq!(r.exit_code(), 3, "{r:?}");
    assert!(
        r.summary["diagnostics"]["base"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "E2610"
                && d["locations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|l| l["path"] == "old/bad.wi")),
        "{r:?}"
    );
    assert!(
        r.summary["uncertainty"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v.as_str().unwrap().contains("scope")),
        "{r:?}"
    );
    for (entry, violation) in [("old/main.wi", true), ("new/main.wi", false)] {
        opts.entry = Some(entry.into());
        let r = verify(&opts).unwrap();
        assert_eq!(
            r.summary["diagnostics"]["base"]
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == "E2610"),
            violation,
            "{r:?}"
        );
        // An explicit override keeps the same entry in both trees. Existing
        // configuration compatibility rules may still reject the manifest edit.
        assert!(
            r.summary["uncertainty"]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v.as_str().unwrap().contains("configuration")),
            "{r:?}"
        );
    }
}

#[test]
fn semantic_diff_review_entry_defaults_and_validation() {
    let dir = repository();
    let mut opts = options(&dir.0);
    opts.entry = None;
    let mut inputs = tree::Tree::new();
    assert_eq!(resolve_entry(&opts, &inputs).unwrap(), Path::new("main.wi"));
    inputs.insert(
        "project.toml".into(),
        b"[project]\nname = \"fixture\"\nversion = \"0.1.0\"\n".to_vec(),
    );
    assert_eq!(
        resolve_entry(&opts, &inputs).unwrap(),
        Path::new("src/main.wi")
    );
    for entry in ["old/main.wi", "new/main.wi"] {
        inputs.insert(
            "project.toml".into(),
            format!("[project]\nname = \"fixture\"\nversion = \"0.1.0\"\nentry = \"{entry}\"\n")
                .into_bytes(),
        );
        assert_eq!(resolve_entry(&opts, &inputs).unwrap(), Path::new(entry));
    }
    opts.entry = Some("explicit/main.wi".into());
    assert_eq!(
        resolve_entry(&opts, &inputs).unwrap(),
        Path::new("explicit/main.wi")
    );
    opts.entry = None;
    for entry in ["", "../outside.wi", "/absolute.wi"] {
        inputs.insert(
            "project.toml".into(),
            format!("[project]\nname = \"fixture\"\nversion = \"0.1.0\"\nentry = \"{entry}\"\n")
                .into_bytes(),
        );
        assert!(resolve_entry(&opts, &inputs).is_err(), "{entry}");
    }
    inputs.insert("project.toml".into(), b"invalid [".to_vec());
    assert!(resolve_entry(&opts, &inputs).is_err());
}
