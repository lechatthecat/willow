use super::*;
use std::{fs, path::PathBuf, process::Command};

pub(super) fn captures(a: &str, b: &str) -> (Capture, Capture) {
    captures_mode(a, b, true)
}
fn captures_mode(a: &str, b: &str, retained: bool) -> (Capture, Capture) {
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
    if !retained {
        revision = crate::compiler_db::revision::AnalysisRevision::default();
    }
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
    assert_eq!(r.changes.len(), 1, "{r:?}");
    assert_eq!(r.changes[0]["match"], "renamed");
    assert_eq!(r.changes[0]["public_api"], "renamed");
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

// Keep a full enumeration and independently cold target as the oracle. This
// checks both compiler revision reuse and the report's sparse candidate index.
fn incremental_oracle(sources: &[&str]) {
    let scratch = tree::Scratch::new().unwrap();
    let opts = Options {
        root: scratch.0.clone(),
        entry: Some("main.wi".into()),
        project: false,
        base: String::new(),
        fail_on_growth: false,
    };
    let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
    let mut previous: Option<Capture> = None;
    for source in sources {
        fs::write(scratch.0.join("main.wi"), source).unwrap();
        let warm = analyze(&opts, &scratch, &mut revision, Path::new("main.wi")).unwrap();
        let cold = analyze(
            &opts,
            &scratch,
            &mut crate::compiler_db::revision::AnalysisRevision::default(),
            Path::new("main.wi"),
        )
        .unwrap();
        if let Some(base) = &previous {
            let keys: BTreeSet<String> = base
                .baseline
                .facts
                .keys()
                .chain(cold.baseline.facts.keys())
                .cloned()
                .collect();
            for policy in [false, true] {
                let expected =
                    serde_json::to_value(compare_keys(base, &cold, policy, keys.clone()).unwrap())
                        .unwrap();
                let actual = serde_json::to_value(compare(base, &warm, policy).unwrap()).unwrap();
                assert_eq!(actual, expected, "{source}");
                assert_eq!(
                    actual,
                    serde_json::to_value(compare(base, &warm, policy).unwrap()).unwrap()
                );
            }
        }
        previous = Some(cold);
    }
}

#[test]
fn semantic_diff_incremental_cold_oracle_edit_shapes() {
    // Addition, deletion, rename, signature, visibility, contracts, existing
    // violations, growth, shrink, offsets, async masks, dispatch, cycles,
    // closures, generic bodies, branches, exits, call targets and repeated edits.
    incremental_oracle(&[
        "fn main() {}",
        "pub fn f() {} fn main() {}",
        "pub fn g() {} fn main() {}",
        "pub fn g() -> i64 { return 1; } fn main() {}",
        "fn g() -> i64 { return 1; } fn main() {}",
        "fn g() -> i64 { return 2; } fn main() {}",
        "fn main() {}",
    ]);
    incremental_oracle(&[
        "fn leaf(c: BlockingCell<i64>) {} fn caller(c: BlockingCell<i64>) { leaf(c); } fn main() {}",
        "fn leaf(c: BlockingCell<i64>) { c.get(); } fn caller(c: BlockingCell<i64>) { leaf(c); } fn main() {}",
        "\n\nfn leaf(c: BlockingCell<i64>) { c.get(); } fn caller(c: BlockingCell<i64>) { leaf(c); } fn main() {}",
        "@effects(no_wait) fn leaf(c: BlockingCell<i64>) { c.get(); } fn caller(c: BlockingCell<i64>) { leaf(c); } fn main() {}",
        "@effects(no_wait) fn leaf(c: BlockingCell<i64>) { c.get(); } fn caller(c: BlockingCell<i64>) { leaf(c); } fn main() {}",
        "@effects(no_block) fn leaf(c: BlockingCell<i64>) { c.get(); } fn caller(c: BlockingCell<i64>) { leaf(c); } fn main() {}",
        "@effects(no_suspend) fn leaf(c: BlockingCell<i64>) { c.get(); } fn caller(c: BlockingCell<i64>) { leaf(c); } fn main() {}",
        "async fn leaf(c: BlockingCell<i64>) { c.get(); } fn caller(c: BlockingCell<i64>) { leaf(c); } fn main() {}",
        "async fn leaf(c: BlockingCell<i64>) {} fn caller(c: BlockingCell<i64>) { leaf(c); } fn main() {}",
    ]);
    for (before, after) in [
        (
            "fn a() { b(); } fn b() { a(); } fn main() {}",
            "fn a() { b(); } fn b() { println(1); a(); } fn main() {}",
        ),
        (
            "fn f() { let g = |x: i64| x; g(1); } fn main() {}",
            "fn f() { let g = |x: i64| { println(x); return x; }; g(1); } fn main() {}",
        ),
        (
            "fn f<T>(x: T) -> T { return x; } fn main() { f(1); }",
            "fn f<T>(x: T) -> T { println(1); return x; } fn main() { f(1); }",
        ),
        (
            "fn a() {} fn b() { println(1); } fn f() { a(); } fn main() {}",
            "fn a() {} fn b() { println(1); } fn f() { b(); } fn main() {}",
        ),
        (
            "fn f(x: bool, c: BlockingCell<i64>) { if x { return; } } fn main() {}",
            "fn f(x: bool, c: BlockingCell<i64>) { if x { c.get(); return; } c.get(); } fn main() {}",
        ),
        (
            "interface I { fn f(self); } class C implements I { pub fn f(self) {} } fn call(x: I) { x.f(); } fn main() {}",
            "interface I { fn f(self); } class C implements I { pub fn f(self) { println(1); } } fn call(x: I) { x.f(); } fn main() {}",
        ),
    ] {
        incremental_oracle(&[before, after, before]);
    }
}

#[test]
fn semantic_diff_incremental_query_scaling() {
    let mut independent_work = None;
    for shape in ["independent", "chain", "fanout", "exits"] {
        for size in [8, 32, 128] {
            let scratch = tree::Scratch::new().unwrap();
            let opts = Options {
                root: scratch.0.clone(),
                entry: Some("main.wi".into()),
                project: false,
                base: String::new(),
                fail_on_growth: false,
            };
            let mut source = "fn leaf(c: BlockingCell<i64>) {}\n".to_owned();
            for i in 0..size {
                let body = match shape {
                    "chain" if i > 0 => format!("node_{}(c);", i - 1),
                    "chain" | "fanout" => "leaf(c);".into(),
                    _ => String::new(),
                };
                source.push_str(&format!("fn node_{i}(c: BlockingCell<i64>) {{ {body} }}\n"));
            }
            source.push_str("fn main() {}\n");
            let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
            fs::write(scratch.0.join("main.wi"), &source).unwrap();
            let base = analyze(&opts, &scratch, &mut revision, Path::new("main.wi")).unwrap();
            let replacement = if shape == "exits" {
                let exits = "if c.get() == 0 { return; } ".repeat(size);
                format!("fn leaf(c: BlockingCell<i64>) {{ {exits} }}")
            } else {
                "fn leaf(c: BlockingCell<i64>) { c.get(); }".into()
            };
            fs::write(
                scratch.0.join("main.wi"),
                source.replacen("fn leaf(c: BlockingCell<i64>) {}", &replacement, 1),
            )
            .unwrap();
            crate::semantic::effects::REACHABLE_EDGE_VISITS.with(|v| v.set(0));
            let target = analyze(&opts, &scratch, &mut revision, Path::new("main.wi")).unwrap();
            let solver_edge_visits =
                crate::semantic::effects::REACHABLE_EDGE_VISITS.with(|v| v.get());
            assert_eq!(revision.changed_bodies.len(), 1);
            assert_eq!(revision.incremental_stats["changed_typed_bodies"], 1);
            let cold = analyze(
                &opts,
                &scratch,
                &mut crate::compiler_db::revision::AnalysisRevision::default(),
                Path::new("main.wi"),
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(compare(&base, &target, false).unwrap()).unwrap(),
                serde_json::to_value(compare(&base, &cold, false).unwrap()).unwrap(),
                "cold scaling oracle {shape}/{size}"
            );
            let mut work = Work::default();
            let keys = comparison_keys(&base, &target, &mut work);
            let affected = if matches!(shape, "chain" | "fanout") {
                size + 1
            } else {
                1
            };
            assert_eq!(keys.len(), affected);
            assert_eq!(revision.incremental_stats["effects_scanned"], 1);
            assert_eq!(revision.typechecks, 1);
            // Include every tracked query recomputation, not only body checks.
            // Extra exits add two expression-level queries each. This bound is
            // not a total-analysis bound: equation comparisons and snapshot
            // reconstruction are measured separately below.
            // Complete signature/builtin references and the retained effect inventory
            // add a fixed set of projection queries. Unrelated-size equality
            // below is the strict scaling assertion.
            let query_budget = if shape == "exits" { 14 + 2 * size } else { 16 };
            assert!(
                revision.incremental_stats["queries_recomputed"]
                    .as_u64()
                    .unwrap()
                    <= query_budget as u64,
                "{shape}/{size}: {}",
                revision.incremental_stats
            );
            let equations = revision.incremental_stats["effects_recomputed"]
                .as_u64()
                .unwrap();
            assert!(
                equations <= 2 * affected as u64 + 2,
                "{shape} {size}: {equations}"
            );
            for (counter, budget) in [
                ("snapshot_functions_rebuilt", affected),
                ("snapshot_symbols_rebuilt", 5 * affected),
                ("report_facts_rebuilt", affected),
                ("report_witness_owners_visited", 6 * affected),
                ("report_witness_edges_visited", 12 * affected),
                ("report_witness_queue_operations", 4 * affected),
                ("report_dependency_edges_visited", 10 * affected),
            ] {
                assert!(
                    revision.incremental_stats[counter].as_u64().unwrap() <= budget as u64,
                    "{shape}/{size} {counter}: {}",
                    revision.incremental_stats
                );
            }
            if shape == "independent" {
                let counts: Vec<_> = [
                    "candidate_entries_cloned",
                    "memo_entries_written",
                    "accounting_entries_visited",
                    "accounting_edges_visited",
                    "queries_recomputed",
                    "effect_equations_compared",
                    "effect_inventory_owners",
                    "effect_dependency_reads",
                    "snapshot_owner_checks",
                    "snapshot_dispatch_visits",
                    "snapshot_projection_items_cloned",
                    "snapshot_boundary_functions",
                    "snapshot_functions_rebuilt",
                    "snapshot_symbols_rebuilt",
                    "report_facts_rebuilt",
                    "report_witness_owners_visited",
                    "report_witness_edges_visited",
                    "report_witness_queue_operations",
                    "report_dependency_edges_visited",
                ]
                .map(|key| revision.incremental_stats[key].as_u64().unwrap())
                .into_iter()
                .chain(std::iter::once(solver_edge_visits as u64))
                .collect();
                if let Some(previous) = &independent_work {
                    assert_eq!(
                        previous, &counts,
                        "unrelated input increased work at {size}"
                    );
                } else {
                    independent_work = Some(counts);
                }
            }
            assert!(
                solver_edge_visits <= 40 * (affected + 1),
                "{shape}/{size} solver edges {solver_edge_visits}"
            );
            assert!(work.index_nodes <= 1 + 1024 * affected);
            let all = base
                .baseline
                .facts
                .keys()
                .chain(target.baseline.facts.keys())
                .cloned()
                .collect();
            assert_eq!(
                serde_json::to_value(compare(&base, &target, false).unwrap()).unwrap(),
                serde_json::to_value(compare_keys(&base, &target, false, all).unwrap()).unwrap()
            );
            println!(
                "incremental shape={shape} size={size} scans=1 equations={equations} compared={} index_nodes={} typechecks={} queries_recomputed={} equation_comparisons={} snapshot_functions={} snapshot_symbols={} snapshot_owner_checks={} coordinate_owners={} changed_typed_bodies={} report_facts={} report_witness_owners={} report_witness_edges={} report_witness_queue={} report_dependency_edges={} effect_inventory_owners={} coordinate_leaves={} effect_dependency_reads={} dispatch_visits={} dispatch_nodes={} projection_items={} boundary_functions={} solver_edges={solver_edge_visits} candidate_clones={} accounting_entries={} accounting_edges={} gc_nodes={} gc_edges={} gc_credit={} memo_writes={} validation_metadata={} auxiliary_accounting={}",
                work.compared_symbols,
                work.index_nodes,
                revision.typechecks,
                revision.incremental_stats["queries_recomputed"],
                revision.incremental_stats["effect_equations_compared"],
                revision.incremental_stats["snapshot_functions_rebuilt"],
                revision.incremental_stats["snapshot_symbols_rebuilt"],
                revision.incremental_stats["snapshot_owner_checks"],
                revision.incremental_stats["snapshot_coordinate_owners"],
                revision.incremental_stats["changed_typed_bodies"],
                revision.incremental_stats["report_facts_rebuilt"],
                revision.incremental_stats["report_witness_owners_visited"],
                revision.incremental_stats["report_witness_edges_visited"],
                revision.incremental_stats["report_witness_queue_operations"],
                revision.incremental_stats["report_dependency_edges_visited"],
                revision.incremental_stats["effect_inventory_owners"],
                revision.incremental_stats["report_coordinate_leaves_visited"],
                revision.incremental_stats["effect_dependency_reads"],
                revision.incremental_stats["snapshot_dispatch_visits"],
                revision.incremental_stats["snapshot_dispatch_nodes"],
                revision.incremental_stats["snapshot_projection_items_cloned"],
                revision.incremental_stats["snapshot_boundary_functions"],
                revision.incremental_stats["candidate_entries_cloned"],
                revision.incremental_stats["accounting_entries_visited"],
                revision.incremental_stats["accounting_edges_visited"],
                revision.incremental_stats["reclamation_nodes_visited"],
                revision.incremental_stats["reclamation_edges_visited"],
                revision.incremental_stats["reclamation_credit_added"],
                revision.incremental_stats["memo_entries_written"],
                revision.incremental_stats["validation_metadata_entries"],
                revision.incremental_stats["auxiliary_accounting_entries"]
            );
        }
    }
}

#[test]
fn semantic_diff_incremental_dependency_edits_match_cold() {
    let scratch = tree::Scratch::new().unwrap();
    let opts = Options {
        root: scratch.0.clone(),
        entry: Some("main.wi".into()),
        project: false,
        base: String::new(),
        fail_on_growth: false,
    };
    fs::write(scratch.0.join("main.wi"), "import dep; @effects(no_wait) fn call(c: BlockingCell<i64>) { dep::leaf(c); } fn main() {}").unwrap();
    let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
    let mut previous: Option<Capture> = None;
    for source in [
        "pub fn leaf(c: BlockingCell<i64>) {}",
        "pub fn leaf(c: BlockingCell<i64>) { c.get(); }",
        "\n\npub fn leaf(c: BlockingCell<i64>) { c.get(); }",
        "pub fn leaf(c: BlockingCell<i64>) {}",
    ] {
        fs::write(scratch.0.join("dep.wi"), source).unwrap();
        let warm = analyze(&opts, &scratch, &mut revision, Path::new("main.wi")).unwrap();
        let cold = analyze(
            &opts,
            &scratch,
            &mut crate::compiler_db::revision::AnalysisRevision::default(),
            Path::new("main.wi"),
        )
        .unwrap();
        if let Some(base) = &previous {
            let keys = base
                .baseline
                .facts
                .keys()
                .chain(cold.baseline.facts.keys())
                .cloned()
                .collect();
            assert_eq!(
                serde_json::to_value(compare(base, &warm, false).unwrap()).unwrap(),
                serde_json::to_value(compare_keys(base, &cold, false, keys).unwrap()).unwrap()
            );
        }
        previous = Some(cold);
    }
}

#[test]
fn semantic_diff_retained_duplicate_diagnostics_keep_distinct_owners() {
    let warnings = "fn warning_a(n:i64)->i64 { return match n { _ => 1, x if x > 0 => 2 }; }\nfn warning_b(n:i64)->i64 { return match n { _ => 1, x if x > 0 => 2 }; }\nfn main() {}";
    let before = format!("fn leaf() {{}}\n{warnings}");
    let after = format!("fn leaf() {{ println(42); }}\n{warnings}");
    let moved = format!("\n\n{after}");
    incremental_oracle(&[&before, &after, &moved]);
}

#[test]
fn semantic_diff_report_failure_invalidates_retained_snapshot() {
    let scratch = tree::Scratch::new().unwrap();
    let opts = Options {
        root: scratch.0.clone(),
        entry: Some("main.wi".into()),
        project: false,
        base: String::new(),
        fail_on_growth: false,
    };
    let entry = Path::new("main.wi");
    let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
    fs::write(scratch.0.join(entry), "fn leaf() {} fn main() {}").unwrap();
    let base = analyze(&opts, &scratch, &mut revision, entry).unwrap();
    // Simulate rejection/loss of the report after accepting a compiler state.
    revision.verification_capture = None;
    fs::write(
        scratch.0.join(entry),
        "fn leaf() { println(42); } fn main() {}",
    )
    .unwrap();
    assert!(analyze(&opts, &scratch, &mut revision, entry).is_err());
    let recovered = analyze(&opts, &scratch, &mut revision, entry).unwrap();
    let cold = analyze(
        &opts,
        &scratch,
        &mut crate::compiler_db::revision::AnalysisRevision::default(),
        entry,
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(compare(&base, &recovered, false).unwrap()).unwrap(),
        serde_json::to_value(compare(&base, &cold, false).unwrap()).unwrap()
    );
}

#[test]
fn semantic_diff_retained_free_declaration_deltas_scale() {
    for size in [8, 32, 128] {
        let scratch = tree::Scratch::new().unwrap();
        let opts = Options {
            root: scratch.0.clone(),
            entry: Some("main.wi".into()),
            project: false,
            base: String::new(),
            fail_on_growth: false,
        };
        let mut source = String::new();
        for i in 0..size {
            source.push_str(&format!("fn unrelated_{i}() {{}}\n"));
        }
        source.push_str("fn main() {}\n");
        let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
        let entry = Path::new("main.wi");
        fs::write(scratch.0.join(entry), &source).unwrap();
        let mut before = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        for prefix in [
            "pub fn added() { println(42); }\n",
            "pub fn added(n:i64) { println(n); }\n",
            "",
        ] {
            fs::write(scratch.0.join(entry), format!("{prefix}{source}")).unwrap();
            let after = analyze(&opts, &scratch, &mut revision, entry).unwrap();
            let cold = analyze(
                &opts,
                &scratch,
                &mut crate::compiler_db::revision::AnalysisRevision::default(),
                entry,
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(compare(&before, &after, false).unwrap()).unwrap(),
                serde_json::to_value(compare(&before, &cold, false).unwrap()).unwrap(),
                "size={size} edit={prefix}"
            );
            assert!(
                revision.incremental_stats["snapshot_functions_rebuilt"]
                    .as_u64()
                    .unwrap()
                    <= 2,
                "{}",
                revision.incremental_stats
            );
            assert!(
                revision.incremental_stats["snapshot_symbols_rebuilt"]
                    .as_u64()
                    .unwrap()
                    <= 4,
                "{}",
                revision.incremental_stats
            );
            assert!(
                revision.incremental_stats["report_facts_rebuilt"]
                    .as_u64()
                    .unwrap()
                    <= 2,
                "{}",
                revision.incremental_stats
            );
            before = after;
        }
    }
}

#[test]
fn semantic_diff_retained_nominal_coordinates_match_cold() {
    let suffix = "class C { pub fn value(self) -> i64 { return 1; } }\nfn use_it(c:C)->i64 { return c.value(); }\nfn main() {}";
    let before = format!("fn leaf() {{}}\n{suffix}");
    let after = format!("fn leaf() {{ println(42); }}\n{suffix}");
    let moved = format!("\n\n{after}");
    incremental_oracle(&[&before, &after, &moved, &before]);
}

#[test]
fn semantic_diff_retained_duplicate_declarations_match_cold() {
    incremental_oracle(&[
        "fn same() {} fn main() {}",
        "fn same() {} fn same() { println(42); } fn main() {}",
        "fn same() {} fn same() {} fn main() {}",
        "fn same() {} fn main() {}",
    ]);
}

#[test]
fn semantic_diff_retained_witness_coordinates_do_not_accumulate() {
    let scratch = tree::Scratch::new().unwrap();
    let opts = Options {
        root: scratch.0.clone(),
        entry: Some("main.wi".into()),
        project: false,
        base: String::new(),
        fail_on_growth: false,
    };
    let entry = Path::new("main.wi");
    let source =
        "fn leaf(c:BlockingCell<i64>) { c.get(); } fn caller(c:BlockingCell<i64>) {} fn main() {}";
    let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
    fs::write(scratch.0.join(entry), source).unwrap();
    let base = analyze(&opts, &scratch, &mut revision, entry).unwrap();
    let leaves = base.causes.leaves.len();
    let locations = base.causes.locations.len();
    for edit in 1..=128 {
        fs::write(
            scratch.0.join(entry),
            format!("{}{source}", "\n".repeat(edit % 2)),
        )
        .unwrap();
        let current = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        assert_eq!(current.causes.leaves.len(), leaves);
        assert_eq!(current.causes.locations.len(), locations);
        if [8, 32, 128].contains(&edit) {
            let changed = source.replace(
                "fn caller(c:BlockingCell<i64>) {}",
                "fn caller(c:BlockingCell<i64>) { leaf(c); }",
            );
            fs::write(scratch.0.join(entry), &changed).unwrap();
            let target = analyze(&opts, &scratch, &mut revision, entry).unwrap();
            let cold = analyze(
                &opts,
                &scratch,
                &mut crate::compiler_db::revision::AnalysisRevision::default(),
                entry,
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(compare(&base, &target, false).unwrap()).unwrap(),
                serde_json::to_value(compare(&base, &cold, false).unwrap()).unwrap()
            );
            fs::write(scratch.0.join(entry), source).unwrap();
            analyze(&opts, &scratch, &mut revision, entry).unwrap();
        }
    }
}

#[test]
fn semantic_diff_retained_unrelated_generic_does_not_rebuild() {
    let mut measured = None;
    for size in [8, 32, 128] {
        let scratch = tree::Scratch::new().unwrap();
        let opts = Options {
            root: scratch.0.clone(),
            entry: Some("main.wi".into()),
            project: false,
            base: String::new(),
            fail_on_growth: false,
        };
        let entry = Path::new("main.wi");
        let mut source = String::from(
            "fn leaf(c:BlockingCell<i64>) {}\nfn identity<T>(x:T)->T { return x; }\nfn main() { identity(1); }\n",
        );
        for i in 0..size {
            source.push_str(&format!("fn unrelated_{i}() {{}}\n"));
        }
        let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
        fs::write(scratch.0.join(entry), &source).unwrap();
        let before = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        fs::write(
            scratch.0.join(entry),
            source.replace(
                "fn leaf(c:BlockingCell<i64>) {}",
                "fn leaf(c:BlockingCell<i64>) { c.get(); }",
            ),
        )
        .unwrap();
        let after = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        let counts = [
            "effect_equations_compared",
            "snapshot_functions_rebuilt",
            "snapshot_symbols_rebuilt",
            "report_facts_rebuilt",
        ]
        .map(|key| revision.incremental_stats[key].as_u64().unwrap());
        if let Some(old) = measured {
            assert_eq!(
                old, counts,
                "unrelated generics caused full rebuild at {size}"
            );
        }
        measured = Some(counts);
        assert_eq!(counts[1], 1);
        let cold = analyze(
            &opts,
            &scratch,
            &mut crate::compiler_db::revision::AnalysisRevision::default(),
            entry,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(compare(&before, &after, false).unwrap()).unwrap(),
            serde_json::to_value(compare(&before, &cold, false).unwrap()).unwrap()
        );
    }
}

#[test]
fn semantic_diff_retained_unrelated_modules_scale() {
    let mut measured = None;
    for size in [8, 32, 128] {
        let scratch = tree::Scratch::new().unwrap();
        let opts = Options {
            root: scratch.0.clone(),
            entry: Some("main.wi".into()),
            project: false,
            base: String::new(),
            fail_on_growth: false,
        };
        let entry = Path::new("main.wi");
        let mut source = String::from("import dep;\n");
        for i in 0..size {
            source.push_str(&format!("import unrelated_{i};\n"));
            fs::write(
                scratch.0.join(format!("unrelated_{i}.wi")),
                "pub fn idle() {}",
            )
            .unwrap();
        }
        source.push_str("fn main() {}\n");
        fs::write(scratch.0.join(entry), source).unwrap();
        fs::write(
            scratch.0.join("dep.wi"),
            "pub fn leaf(c:BlockingCell<i64>) {}",
        )
        .unwrap();
        let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
        let before = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        fs::write(
            scratch.0.join("dep.wi"),
            "pub fn leaf(c:BlockingCell<i64>) { c.get(); }",
        )
        .unwrap();
        let after = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        let counts = [
            "queries_recomputed",
            "effect_equations_compared",
            "effect_inventory_owners",
            "snapshot_owner_checks",
            "snapshot_functions_rebuilt",
            "snapshot_symbols_rebuilt",
            "report_facts_rebuilt",
        ]
        .map(|key| revision.incremental_stats[key].as_u64().unwrap());
        if let Some(old) = measured {
            assert_eq!(
                old, counts,
                "unrelated modules increased semantic work at {size}"
            );
        }
        measured = Some(counts);
        let cold = analyze(
            &opts,
            &scratch,
            &mut crate::compiler_db::revision::AnalysisRevision::default(),
            entry,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(compare(&before, &after, false).unwrap()).unwrap(),
            serde_json::to_value(compare(&before, &cold, false).unwrap()).unwrap()
        );
    }
}

#[test]
fn semantic_diff_retained_method_and_effectful_neighbors_scale() {
    for method in [false, true] {
        let mut measured = None;
        for size in [8, 32, 128] {
            let scratch = tree::Scratch::new().unwrap();
            let opts = Options {
                root: scratch.0.clone(),
                entry: Some("main.wi".into()),
                project: false,
                base: String::new(),
                fail_on_growth: false,
            };
            let entry = Path::new("main.wi");
            let signature = if method {
                "class C { fn leaf(self,c:BlockingCell<i64>)"
            } else {
                "fn leaf(c:BlockingCell<i64>)"
            };
            let suffix = if method { " }\n" } else { "\n" };
            // Equal byte lengths exclude coordinate repair from semantic work.
            let before_body = "{          }";
            let after_body = "{ c.get(); }";
            assert_eq!(before_body.len(), after_body.len());
            let mut source = format!("{signature} {before_body}{suffix}");
            for i in 0..size {
                source.push_str(&format!("fn unrelated_{i}() {{ println(1); }}\n"));
            }
            source.push_str("fn main() {}\n");
            fs::write(scratch.0.join(entry), &source).unwrap();
            let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
            let before = analyze(&opts, &scratch, &mut revision, entry).unwrap();
            fs::write(
                scratch.0.join(entry),
                source.replacen(before_body, after_body, 1),
            )
            .unwrap();
            let after = analyze(&opts, &scratch, &mut revision, entry).unwrap();
            let counts = [
                "queries_recomputed",
                "effect_equations_compared",
                "effect_inventory_owners",
                "snapshot_owner_checks",
                "snapshot_functions_rebuilt",
                "snapshot_symbols_rebuilt",
                "report_facts_rebuilt",
            ]
            .map(|key| revision.incremental_stats[key].as_u64().unwrap());
            if let Some(old) = measured {
                assert_eq!(
                    old, counts,
                    "method={method} unrelated effectful functions increased work at {size}"
                );
            }
            measured = Some(counts);
            assert_eq!(
                counts[4], 1,
                "method={method}: {}",
                revision.incremental_stats
            );
            let cold = analyze(
                &opts,
                &scratch,
                &mut crate::compiler_db::revision::AnalysisRevision::default(),
                entry,
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(compare(&before, &after, false).unwrap()).unwrap(),
                serde_json::to_value(compare(&before, &cold, false).unwrap()).unwrap()
            );
        }
    }
}

#[test]
fn semantic_diff_retained_lambda_edits_scale() {
    retained_specialized_edit_scaling("lambda");
}
#[test]
fn semantic_diff_retained_generic_edits_scale() {
    retained_specialized_edit_scaling("generic");
}
#[test]
fn semantic_diff_retained_virtual_edits_scale() {
    retained_specialized_edit_scaling("virtual");
}
#[test]
fn semantic_diff_retained_constructor_edits_scale() {
    retained_specialized_edit_scaling("constructor");
}
#[test]
fn semantic_diff_retained_default_edits_scale() {
    retained_specialized_edit_scaling("default");
}
#[test]
fn semantic_diff_retained_generic_class_edits_scale() {
    retained_specialized_edit_scaling("generic_class");
}
#[test]
fn semantic_diff_retained_constructor_group_edits_scale() {
    retained_specialized_edit_scaling("constructor_group");
}
fn retained_specialized_edit_scaling(selected: &str) {
    for (shape, before_root, after_root, main) in [
        (
            "lambda",
            "fn leaf() { let g = |x:i64| x; g(1); }",
            "fn leaf() { let g = |x:i64| { println(x); return x; }; g(1); }",
            "fn main() {}",
        ),
        (
            "generic",
            "fn leaf<T>(x:T)->T { return x; }",
            "fn leaf<T>(x:T)->T { println(1); return x; }",
            "fn main() { leaf(1); leaf(true); }",
        ),
        (
            "virtual",
            "interface I { fn leaf(self); } class C implements I { pub fn leaf(self) {} } fn call(x:I) { x.leaf(); }",
            "interface I { fn leaf(self); } class C implements I { pub fn leaf(self) { println(1); } } fn call(x:I) { x.leaf(); }",
            "fn main() {}",
        ),
        (
            "constructor",
            "class C { pub init(self) {} }",
            "class C { pub init(self) { println(1); } }",
            "fn main() { let c = new C(); }",
        ),
        (
            "default",
            "interface I { fn leaf(self) {} } class C implements I {}",
            "interface I { fn leaf(self) { println(1); } } class C implements I {}",
            "fn main() {}",
        ),
        (
            "generic_class",
            "class C<T> { pub fn leaf(self,x:T)->T { return x; } }",
            "class C<T> { pub fn leaf(self,x:T)->T { println(1); return x; } }",
            "fn main() { let c = new C<i64>(); c.leaf(1); }",
        ),
        (
            "constructor_group",
            "class C { pub init(self) {} pub init(self,x:i64) {} }",
            "class C { pub init(self) {} pub init(self,x:i64) { println(x); } }",
            "fn main() { let c = new C(); }",
        ),
    ] {
        if shape != selected {
            continue;
        }
        let mut measured = None;
        for size in [8, 32, 128] {
            let scratch = tree::Scratch::new().unwrap();
            let opts = Options {
                root: scratch.0.clone(),
                entry: Some("main.wi".into()),
                project: false,
                base: String::new(),
                fail_on_growth: false,
            };
            let entry = Path::new("main.wi");
            let mut suffix = format!("\n{main}\n");
            for i in 0..size {
                if shape == "virtual" {
                    suffix.push_str(&format!("class Unrelated_{i} {{ fn idle(self) {{}} }}\n"));
                } else {
                    suffix.push_str(&format!("fn unrelated_{i}() {{}}\n"));
                }
            }
            fs::write(scratch.0.join(entry), format!("{before_root}{suffix}")).unwrap();
            let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
            let before = analyze(&opts, &scratch, &mut revision, entry).unwrap();
            fs::write(scratch.0.join(entry), format!("{after_root}{suffix}")).unwrap();
            let after = analyze(&opts, &scratch, &mut revision, entry).unwrap();
            let counts = [
                "effect_equations_compared",
                "effect_inventory_owners",
                "snapshot_functions_rebuilt",
                "snapshot_symbols_rebuilt",
                "report_facts_rebuilt",
                "snapshot_dispatch_visits",
            ]
            .map(|key| revision.incremental_stats[key].as_u64().unwrap());
            println!(
                "specialized shape={shape} size={size} equation_comparisons={} effect_inventory_owners={} snapshot_functions={} snapshot_symbols={} report_facts={} queries_recomputed={} report_witness_owners={} report_witness_edges={} report_witness_queue={} dispatch_visits={}",
                counts[0],
                counts[1],
                counts[2],
                counts[3],
                counts[4],
                revision.incremental_stats["queries_recomputed"],
                revision.incremental_stats["report_witness_owners_visited"],
                revision.incremental_stats["report_witness_edges_visited"],
                revision.incremental_stats["report_witness_queue_operations"],
                revision.incremental_stats["snapshot_dispatch_visits"]
            );
            if let Some(old) = measured {
                assert_eq!(
                    old, counts,
                    "{shape} unrelated functions increased work at {size}"
                );
            }
            measured = Some(counts);
            assert!(counts[2] <= 4, "{shape}: {}", revision.incremental_stats);
            let cold = analyze(
                &opts,
                &scratch,
                &mut crate::compiler_db::revision::AnalysisRevision::default(),
                entry,
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(compare(&before, &after, false).unwrap()).unwrap(),
                serde_json::to_value(compare(&before, &cold, false).unwrap()).unwrap(),
                "{shape}/{size}"
            );
            fs::write(scratch.0.join(entry), format!("{before_root}{suffix}")).unwrap();
            let reverted = analyze(&opts, &scratch, &mut revision, entry).unwrap();
            assert_eq!(
                serde_json::to_value(compare(&after, &reverted, false).unwrap()).unwrap(),
                serde_json::to_value(compare(&after, &before, false).unwrap()).unwrap(),
                "reverted {shape}/{size}"
            );
        }
    }
}

#[test]
fn semantic_diff_retained_imported_default_edits_scale() {
    retained_imported_edit_scaling("imported_default");
}
#[test]
fn semantic_diff_retained_imported_generic_edits_scale() {
    retained_imported_edit_scaling("imported_generic");
}
fn retained_imported_edit_scaling(shape: &str) {
    let mut measured = None;
    for size in [8, 32, 128] {
        let scratch = tree::Scratch::new().unwrap();
        let opts = Options {
            root: scratch.0.clone(),
            entry: Some("main.wi".into()),
            project: false,
            base: String::new(),
            fail_on_growth: false,
        };
        let entry = Path::new("main.wi");
        let mut source = String::from(if shape == "imported_default" {
            "import dep; class C implements dep::I {} fn main() {}\n"
        } else {
            "import dep; fn main() { dep::leaf(1); }\n"
        });
        for i in 0..size {
            source.push_str(&format!("fn unrelated_{i}() {{}}\n"));
        }
        fs::write(scratch.0.join(entry), source).unwrap();
        fs::write(
            scratch.0.join("dep.wi"),
            if shape == "imported_default" {
                "pub interface I { fn leaf(self) {} }"
            } else {
                "pub fn leaf<T>(x:T)->T { return x; }"
            },
        )
        .unwrap();
        let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
        let before = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        fs::write(
            scratch.0.join("dep.wi"),
            if shape == "imported_default" {
                "pub interface I { fn leaf(self) { println(1); } }"
            } else {
                "pub fn leaf<T>(x:T)->T { println(1); return x; }"
            },
        )
        .unwrap();
        let after = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        let counts = [
            "effect_equations_compared",
            "effect_inventory_owners",
            "snapshot_functions_rebuilt",
            "snapshot_symbols_rebuilt",
            "report_facts_rebuilt",
            "snapshot_dispatch_visits",
        ]
        .map(|key| revision.incremental_stats[key].as_u64().unwrap());
        println!(
            "specialized shape={shape} size={size} equation_comparisons={} effect_inventory_owners={} snapshot_functions={} snapshot_symbols={} report_facts={} queries_recomputed={} report_witness_owners={} report_witness_edges={} report_witness_queue={} dispatch_visits={}",
            counts[0],
            counts[1],
            counts[2],
            counts[3],
            counts[4],
            revision.incremental_stats["queries_recomputed"],
            revision.incremental_stats["report_witness_owners_visited"],
            revision.incremental_stats["report_witness_edges_visited"],
            revision.incremental_stats["report_witness_queue_operations"],
            revision.incremental_stats["snapshot_dispatch_visits"]
        );
        if let Some(old) = measured {
            assert_eq!(old, counts, "{shape} at {size}");
        }
        measured = Some(counts);
        assert!(counts[2] <= 4, "{}", revision.incremental_stats);
        let cold = analyze(
            &opts,
            &scratch,
            &mut crate::compiler_db::revision::AnalysisRevision::default(),
            entry,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(compare(&before, &after, false).unwrap()).unwrap(),
            serde_json::to_value(compare(&before, &cold, false).unwrap()).unwrap()
        );
    }
}

#[test]
fn semantic_diff_retained_generic_instantiation_lifecycle() {
    incremental_oracle(&[
        "fn identity<T>(x:T)->T { println(1); return x; } fn main() {}",
        "fn identity<T>(x:T)->T { println(1); return x; } fn main() { identity(1); }",
        "fn identity<T>(x:T)->T { println(1); return x; } fn main() { identity(1); identity(true); }",
        "fn identity<T>(x:T)->T { println(1); return x; } fn main() { identity(true); }",
        "fn identity<T>(x:T)->T { println(1); return x; } fn main() {}",
        "fn identity<T>(x:T)->T { return x; } fn main() {}",
    ]);
    let scratch = tree::Scratch::new().unwrap();
    let opts = Options {
        root: scratch.0.clone(),
        entry: Some("main.wi".into()),
        project: false,
        base: String::new(),
        fail_on_growth: false,
    };
    let entry = Path::new("main.wi");
    fs::write(
        scratch.0.join("dep.wi"),
        "pub fn identity<T>(x:T)->T { println(1); return x; }",
    )
    .unwrap();
    let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
    let mut previous = None;
    for body in [
        "",
        "dep::identity(1);",
        "dep::identity(1); dep::identity(true);",
        "dep::identity(true);",
        "",
    ] {
        fs::write(
            scratch.0.join(entry),
            format!("import dep; fn main() {{ {body} }}"),
        )
        .unwrap();
        let warm = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        let cold = analyze(
            &opts,
            &scratch,
            &mut crate::compiler_db::revision::AnalysisRevision::default(),
            entry,
        )
        .unwrap();
        if let Some(base) = &previous {
            assert_eq!(
                serde_json::to_value(compare(base, &warm, false).unwrap()).unwrap(),
                serde_json::to_value(compare(base, &cold, false).unwrap()).unwrap()
            );
        }
        previous = Some(cold);
    }
}

#[test]
fn semantic_diff_retained_call_target_lifecycle() {
    let prefix = "fn a() { println(1); } fn b() {} ";
    let before = format!("{prefix} fn call() {{ let f=a; f(); }} fn main() {{}}");
    let after = format!("{prefix} fn call() {{ let f=b; f(); }} fn main() {{}}");
    let removed = format!("{prefix} fn call() {{}} fn main() {{}}");
    incremental_oracle(&[&before, &after, &removed, &before]);
    let classes = "open class Base { pub open fn run(self) {} } class C extends Base { pub override fn run(self) { println(1); } } ";
    let before = format!("{classes} fn call(x:Base) {{}} fn main() {{}}");
    let after = format!("{classes} fn call(x:Base) {{ x.run(); }} fn main() {{}}");
    incremental_oracle(&[&before, &after, &before, &after]);
}

#[test]
fn semantic_diff_retained_new_callees_preserve_direct_witnesses() {
    for _ in 0..3 {
        incremental_oracle(&[
            "fn old() {} fn caller() { old(); } fn main() {}",
            "fn old() {} fn caller() { added(); } fn added() { println(1); } fn main() {}",
            "fn old() {} fn caller() { old(); } fn main() {}",
        ]);
    }
}

#[test]
fn semantic_diff_retained_generic_unused_imports_scale() {
    let mut measured = None;
    for size in [8, 32, 128] {
        let scratch = tree::Scratch::new().unwrap();
        let opts = Options {
            root: scratch.0.clone(),
            entry: Some("main.wi".into()),
            project: false,
            base: String::new(),
            fail_on_growth: false,
        };
        let entry = Path::new("main.wi");
        fs::write(
            scratch.0.join(entry),
            "import dep; import types; fn main() { dep::leaf(1, new types::C()); }",
        )
        .unwrap();
        fs::write(scratch.0.join("types.wi"), "pub class C {}").unwrap();
        let mut imports = String::from("import types;\n");
        for i in 0..size {
            imports.push_str(&format!("import unused_{i};\n"));
            fs::write(scratch.0.join(format!("unused_{i}.wi")), "pub fn idle() {}").unwrap();
        }
        fs::write(
            scratch.0.join("dep.wi"),
            format!("{imports}pub fn leaf<T>(x:T,c:types::C)->T {{ return x; }}"),
        )
        .unwrap();
        let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
        let before = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        fs::write(
            scratch.0.join("dep.wi"),
            format!("{imports}pub fn leaf<T>(x:T,c:types::C)->T {{ println(1); return x; }}"),
        )
        .unwrap();
        let after = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        let counts = [
            "effect_equations_compared",
            "snapshot_functions_rebuilt",
            "snapshot_symbols_rebuilt",
            "report_facts_rebuilt",
        ]
        .map(|key| revision.incremental_stats[key].as_u64().unwrap());
        if let Some(old) = measured {
            assert_eq!(old, counts, "unused imports at {size}");
        }
        measured = Some(counts);
        let cold = analyze(
            &opts,
            &scratch,
            &mut crate::compiler_db::revision::AnalysisRevision::default(),
            entry,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(compare(&before, &after, false).unwrap()).unwrap(),
            serde_json::to_value(compare(&before, &cold, false).unwrap()).unwrap()
        );
    }
}

#[test]
fn semantic_diff_retained_large_unchanged_boundary_scale() {
    let mut measured = None;
    for size in [8, 32, 128] {
        let scratch = tree::Scratch::new().unwrap();
        let opts = Options {
            root: scratch.0.clone(),
            entry: Some("main.wi".into()),
            project: false,
            base: String::new(),
            fail_on_growth: false,
        };
        let entry = Path::new("main.wi");
        let mut source = String::from("fn leaf(c:BlockingCell<i64>) {          }\nfn large() {\n");
        for i in 0..size {
            source.push_str(&format!("let local_{i} = {i};\n"));
        }
        source.push_str("}\nfn caller(c:BlockingCell<i64>) { large(); leaf(c); } fn main() {}\n");
        fs::write(scratch.0.join(entry), &source).unwrap();
        let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
        let before = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        fs::write(
            scratch.0.join(entry),
            source.replacen("{          }", "{ c.get(); }", 1),
        )
        .unwrap();
        let after = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        let counts = [
            "effect_equations_compared",
            "snapshot_functions_rebuilt",
            "snapshot_symbols_rebuilt",
            "report_facts_rebuilt",
            "snapshot_projection_items_cloned",
            "snapshot_boundary_functions",
        ]
        .map(|key| revision.incremental_stats[key].as_u64().unwrap());
        if let Some(old) = measured {
            assert_eq!(old, counts, "unchanged callee body at {size}");
        }
        measured = Some(counts);
        let cold = analyze(
            &opts,
            &scratch,
            &mut crate::compiler_db::revision::AnalysisRevision::default(),
            entry,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(compare(&before, &after, false).unwrap()).unwrap(),
            serde_json::to_value(compare(&before, &cold, false).unwrap()).unwrap()
        );
    }
}

#[test]
fn semantic_diff_competing_witness_chain_scales() {
    for size in [8usize, 32, 128] {
        let scratch = tree::Scratch::new().unwrap();
        let opts = Options {
            root: scratch.0.clone(),
            entry: Some("main.wi".into()),
            project: false,
            base: String::new(),
            fail_on_growth: false,
        };
        let entry = Path::new("main.wi");
        let mut source = String::from("fn noop() {} fn main() {}\n");
        for i in 0..size {
            source.push_str(&format!(
                "fn witness_{:04}(c:BlockingCell<i64>) {{ c.get(); }}\n",
                size - i
            ));
            let next = if i + 1 < size {
                format!("caller_{:04}(c);", i + 1)
            } else {
                String::new()
            };
            source.push_str(&format!(
                "fn caller_{i:04}(c:BlockingCell<i64>) {{ witness_{:04}(c); {next} }}\n",
                size - i
            ));
        }
        fs::write(scratch.0.join(entry), &source).unwrap();
        let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
        let base = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        let changed = source.replace("witness_0001(c);  }", "witness_0001(c); noop(); }");
        assert_ne!(changed, source);
        for (phase, text) in [("add", &changed), ("remove", &source)] {
            fs::write(scratch.0.join(entry), text).unwrap();
            crate::semantic::effects::DELTA_QUEUE_POPS.with(|v| v.set(0));
            crate::semantic::effects::REACHABLE_EDGE_VISITS.with(|v| v.set(0));
            let target = analyze(&opts, &scratch, &mut revision, entry).unwrap();
            let pops = crate::semantic::effects::DELTA_QUEUE_POPS.with(|v| v.get());
            let edges = crate::semantic::effects::REACHABLE_EDGE_VISITS.with(|v| v.get());
            assert!(
                pops <= 48 * (size + 2),
                "size={size} phase={phase} pops={pops}"
            );
            assert!(
                edges <= 96 * (size + 2),
                "size={size} phase={phase} edges={edges}"
            );
            let cold = analyze(
                &opts,
                &scratch,
                &mut crate::compiler_db::revision::AnalysisRevision::default(),
                entry,
            )
            .unwrap();
            assert_eq!(
                serde_json::to_value(compare(&base, &target, false).unwrap()).unwrap(),
                serde_json::to_value(compare(&base, &cold, false).unwrap()).unwrap()
            );
            println!(
                "competing-production size={size} phase={phase} queue_pops={pops} edges={edges}"
            );
        }
    }
}

#[test]
fn semantic_diff_candidate_reclamation_is_amortized() {
    let mut independent_accounting = None;
    for size in [8usize, 32, 128] {
        let scratch = tree::Scratch::new().unwrap();
        let opts = Options {
            root: scratch.0.clone(),
            entry: Some("main.wi".into()),
            project: false,
            base: String::new(),
            fail_on_growth: false,
        };
        let entry = Path::new("main.wi");
        let mut source = String::from("fn leaf(c:BlockingCell<i64>) {}\nfn main() {}\n");
        for i in 0..size {
            source.push_str(&format!("fn unrelated_{i}() {{}}\n"));
        }
        let edited = source.replacen(
            "fn leaf(c:BlockingCell<i64>) {}",
            "fn leaf(c:BlockingCell<i64>) { c.get(); }",
            1,
        );
        fs::write(scratch.0.join(entry), &source).unwrap();
        let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
        let base = analyze(&opts, &scratch, &mut revision, entry).unwrap();
        let mut work = 0u64;
        let mut credit = 0u64;
        let mut collections = 0;
        for step in 0..128 {
            let text = if step % 2 == 0 { &edited } else { &source };
            fs::write(scratch.0.join(entry), text).unwrap();
            let target = analyze(&opts, &scratch, &mut revision, entry).unwrap();
            let stats = &revision.incremental_stats;
            assert_eq!(stats["candidate_entries_cloned"], 0);
            let visited = stats["reclamation_nodes_visited"].as_u64().unwrap()
                + stats["reclamation_edges_visited"].as_u64().unwrap();
            work += visited;
            credit += stats["reclamation_credit_added"].as_u64().unwrap();
            collections += usize::from(visited > 0);
            assert!(
                work <= 4 * credit,
                "size={size} step={step} gc_work={work} mutation_credit={credit}"
            );
            if step == 0 {
                let accounting = (
                    stats["accounting_entries_visited"].as_u64().unwrap(),
                    stats["accounting_edges_visited"].as_u64().unwrap(),
                );
                if let Some(previous) = independent_accounting {
                    assert_eq!(accounting, previous, "size={size}");
                }
                independent_accounting = Some(accounting);
            }
            if step == 127 {
                let cold = analyze(
                    &opts,
                    &scratch,
                    &mut crate::compiler_db::revision::AnalysisRevision::default(),
                    entry,
                )
                .unwrap();
                assert_eq!(
                    serde_json::to_value(compare(&base, &target, false).unwrap()).unwrap(),
                    serde_json::to_value(compare(&base, &cold, false).unwrap()).unwrap()
                );
            }
        }
        assert!(
            collections > 0,
            "fixture must exercise actual reclamation: size={size}"
        );
        for _ in 0..8 {
            analyze(&opts, &scratch, &mut revision, entry).unwrap();
            for key in [
                "candidate_entries_cloned",
                "reclamation_nodes_visited",
                "reclamation_edges_visited",
                "reclamation_credit_added",
            ] {
                assert_eq!(
                    revision.incremental_stats[key], 0,
                    "unchanged size={size} {key}"
                );
            }
        }
        println!(
            "candidate-amortized size={size} revisions=128 gc_work={work} mutation_credit={credit} collections={collections}"
        );
    }
}

#[test]
fn semantic_diff_rename_five_kinds() {
    let source = include_str!("../../example/rename_semantic_verification.wi");
    let edits = [
        ("paid_total", "settled_total"),
        ("Borrowed", "Lent"),
        ("digest", "fingerprint"),
        ("member", "require_member"),
        ("tag", "reason_tag"),
    ];
    for individual in 0..=edits.len() {
        let mut target = source.to_owned();
        for (i, (old, new)) in edits.iter().enumerate() {
            if individual == edits.len() || individual == i {
                target = target.replace(old, new);
            }
        }
        let r = report(source, &target);
        assert_eq!(r.exit_code(), 0, "{individual}: {r:?}");
        assert_eq!(r.changes.len(), [1, 1, 2, 1, 0, 5][individual], "{r:?}");
        assert!(
            r.changes.iter().all(|c| c["match"] == "renamed"),
            "{individual}: {r:?}"
        );
        assert!(r.changes.iter().all(|c| c["effects"]["added"] == json!([]) && c["effects"]["removed"] == json!([])), "{r:?}");
        let text = r.human(true);
        assert!(
            !text.contains("$dispatch$")
                && !text.contains("@")
                && !text.contains("function-value candidates"),
            "{text}"
        );
    }
}

#[test]
fn semantic_diff_rename_boundaries() {
    // Each row is a separate acceptance perspective, not a whole-suite run.
    for (label, source, target, renamed, added) in [
        (
            "private function",
            "fn f() {} fn main() { f(); }",
            "fn longer() {} fn main() { longer(); }",
            1,
            false,
        ),
        (
            "recursive",
            "fn f(n:i64)->i64 { if n == 0 { return 0; } return f(n-1); } fn main() {}",
            "fn g(n:i64)->i64 { if n == 0 { return 0; } return g(n-1); } fn main() {}",
            1,
            false,
        ),
        (
            "literal change",
            "fn f()->i64 { return 1; } fn main() {}",
            "fn g()->i64 { return 2; } fn main() {}",
            0,
            true,
        ),
        (
            "operator change",
            "fn f(n:i64)->i64 { return n+1; } fn main() {}",
            "fn g(n:i64)->i64 { return n-1; } fn main() {}",
            0,
            true,
        ),
        (
            "signature change",
            "fn f(n:i64) {} fn main() {}",
            "fn g(n:bool) {} fn main() {}",
            0,
            true,
        ),
        (
            "visibility change",
            "fn f() {} fn main() {}",
            "pub fn g() {} fn main() {}",
            0,
            true,
        ),
        (
            "ambiguous candidates",
            "fn a() {} fn b() {} fn main() {}",
            "fn c() {} fn d() {} fn main() {}",
            0,
            true,
        ),
        (
            "different owners",
            "class A { fn f(self) {} } class B {} fn main() {}",
            "class A {} class B { fn g(self) {} } fn main() {}",
            0,
            true,
        ),
        (
            "lambda displacement",
            "fn f() {} fn main() { let f = |x:i64| x+1; println(f(2)); }",
            "// moved\nfn f() {} fn main() { let f = |x:i64| x+1; println(f(2)); }",
            0,
            false,
        ),
        (
            "local rename",
            "fn main() { let x = 1; println(x); }",
            "fn main() { let longer = 1; println(longer); }",
            0,
            false,
        ),
        (
            "parameter rename",
            "fn main() { let f = |x:i64| x+1; println(f(2)); }",
            "fn main() { let f = |longer:i64| longer+1; println(f(2)); }",
            0,
            false,
        ),
        (
            "field type change",
            "class A { pub x:i64; } fn main() {}",
            "class A { pub y:bool; } fn main() {}",
            0,
            true,
        ),
        (
            "variant payload change",
            "enum E { A(i64) } fn main() {}",
            "enum E { B(bool) } fn main() {}",
            0,
            true,
        ),
        (
            "unchanged sibling",
            "fn keep() {} fn a() {} fn main() {}",
            "fn keep() {} fn b() {} fn main() {}",
            1,
            false,
        ),
    ] {
        let r = report(source, target);
        assert_eq!(r.exit_code(), 0, "{label}: {r:?}");
        assert_eq!(
            r.changes.iter().filter(|c| c["match"] == "renamed").count(),
            renamed,
            "{label}: {r:?}"
        );
        assert_eq!(
            r.changes.iter().any(|c| c["match"] == "added"),
            added,
            "{label}: {r:?}"
        );
        if !added {
            assert_eq!(r.changes.len(), renamed, "{label}: {r:?}");
        }
    }
}

#[test]
fn semantic_diff_rename_preserves_effect_and_contract_evidence() {
    let a = "fn f(c:BlockingCell<i64>) { c.get(); } fn main() {}";
    let b = "@effects(no_wait) fn g(c:BlockingCell<i64>) { c.get(); } fn main() {}";
    let r = report(a, b);
    assert_eq!(r.exit_code(), 1, "{r:?}");
    let change = r.changes.iter().find(|c| c["match"] == "renamed").unwrap();
    assert_eq!(change["contract"]["change"], "added");
    assert!(
        !change["contract"]["target_violations"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        change["cause"]
            .as_array()
            .unwrap()
            .iter()
            .any(|cause| cause["side"] == "target" && cause["chain"][0] == change["renamed_to"])
    );
}

#[test]
fn semantic_diff_rename_repeated_lambdas_keep_occurrence_order() {
    let source = "fn main() { let a = |x:i64| x+1; let b = |x:i64| x+1; println(a(1)+b(2)); }";
    let r = report(source, &format!("// shift all identical bodies\n{source}"));
    assert_eq!(r.exit_code(), 0, "{r:?}");
    assert!(r.changes.is_empty(), "{r:?}");
}

#[test]
fn semantic_diff_rename_does_not_hide_effect_growth_or_retargeting() {
    let prefix = "fn quiet(c:BlockingCell<i64>) {} fn loud(c:BlockingCell<i64>) { c.get(); } ";
    let a = format!("{prefix} fn f(c:BlockingCell<i64>) {{ quiet(c); }} fn main() {{}}");
    let b = format!("{prefix} fn g(c:BlockingCell<i64>) {{ loud(c); }} fn main() {{}}");
    let (before, after) = captures(&a, &b);
    let r = compare(&before, &after, true).unwrap();
    assert_eq!(r.exit_code(), 1, "{r:?}");
    let change = r.changes.iter().find(|c| c["match"] == "renamed").unwrap();
    assert!(
        change["effects"]["added"]
            .as_array()
            .unwrap()
            .contains(&json!("may-block"))
    );
    assert_eq!(change["targets"]["change"], "changed");
    assert!(
        change["targets"]["base"]
            .as_array()
            .unwrap()
            .contains(&json!("main.wi::function::quiet#0"))
    );
    assert!(
        change["targets"]["target"]
            .as_array()
            .unwrap()
            .contains(&json!("main.wi::function::loud#0"))
    );
}

#[test]
fn semantic_diff_rename_cold_and_retained_agree() {
    let source = include_str!("../../example/rename_semantic_verification.wi");
    let target = source
        .replace("paid_total", "settled_total")
        .replace("Borrowed", "Lent")
        .replace("digest", "fingerprint")
        .replace("member", "require_member")
        .replace("tag", "reason_tag");
    let scratch = tree::Scratch::new().unwrap();
    let opts = Options {
        root: scratch.0.clone(),
        entry: Some("main.wi".into()),
        project: false,
        base: String::new(),
        fail_on_growth: false,
    };
    let mut revision = crate::compiler_db::revision::AnalysisRevision::default();
    fs::write(scratch.0.join("main.wi"), source).unwrap();
    let before = analyze(&opts, &scratch, &mut revision, Path::new("main.wi")).unwrap();
    fs::write(scratch.0.join("main.wi"), &target).unwrap();
    let retained = analyze(&opts, &scratch, &mut revision, Path::new("main.wi")).unwrap();
    let cold = analyze(
        &opts,
        &scratch,
        &mut crate::compiler_db::revision::AnalysisRevision::default(),
        Path::new("main.wi"),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(compare(&before, &retained, false).unwrap()).unwrap(),
        serde_json::to_value(compare(&before, &cold, false).unwrap()).unwrap()
    );
}

#[test]
fn semantic_diff_rename_lambda_offset_collision() {
    let source = "fn main() { let a = |x:i64| x+1; let b = |x:i64| x+2; println(a(1)+b(2)); }";
    let prefix = "//123456789012345678\n";
    assert_eq!(prefix.len(), 21);
    for (before, after) in [
        (source.to_owned(), format!("{prefix}{source}")),
        (format!("{prefix}{source}"), source.to_owned()),
    ] {
        for retained in [false, true] {
            let (before, after) = captures_mode(&before, &after, retained);
            let r = compare(&before, &after, false).unwrap();
            assert_eq!(r.exit_code(), 0, "{r:?}");
            assert!(r.changes.is_empty(), "{r:?}");
        }
    }
}

#[test]
fn semantic_diff_rename_lambda_owner_correspondence() {
    for source in [
        "fn f()->i64 { let h = |x:i64| x+1; return h(1); } fn main() { println(f()); }",
        "class C { fn f(self)->i64 { let h = |x:i64| x+1; return h(1); } pub fn call(self)->i64 { return self.f(); } } fn main() { let c = new C(); println(c.call()); }",
    ] {
        let target = source.replace("f(", "g(");
        for retained in [false, true] {
            let (before, after) = captures_mode(source, &target, retained);
            let r = compare(&before, &after, false).unwrap();
            assert_eq!(r.exit_code(), 0, "{r:?}");
            assert_eq!(r.changes.len(), 1, "{r:?}");
            assert_eq!(r.changes[0]["match"], "renamed");
            assert_eq!(r.changes[0]["effects"]["added"], json!([]));
            assert_eq!(r.changes[0]["effects"]["removed"], json!([]));
            assert_eq!(r.changes[0]["targets"]["change"], "unchanged");
        }
    }
}

#[test]
fn semantic_diff_rename_lambda_shared_equal_keys() {
    // The colliding middle key has equal facts and is absent from the diff
    // index. Matching must still include every occurrence of this owner.
    let source = "fn main() { let a = |x:i64| x+1; let b = |x:i64| x+1; println(a(1)+b(2)); }";
    let target = format!("//123456789012345678\n{source}");
    let (before, after) = captures_mode(source, &target, false);
    let keys = comparison_keys(&before, &after, &mut Work::default());
    let shared: Vec<_> = before
        .baseline
        .facts
        .keys()
        .filter(|k| matching::is_lambda(k) && after.baseline.facts.contains_key(*k))
        .collect();
    assert_eq!(shared.len(), 1);
    assert!(!keys.contains(shared[0]));
    let r = compare(&before, &after, false).unwrap();
    assert_eq!(r.exit_code(), 0, "{r:?}");
    assert!(r.changes.is_empty(), "{r:?}");
}

#[test]
fn semantic_diff_rename_lambda_collision_preserves_effect_growth() {
    let source = "fn quiet(c:BlockingCell<i64>)->i64 { return 0; } fn noisy(c:BlockingCell<i64>)->i64 { return c.get(); } fn main() { let a = |x:BlockingCell<i64>| quiet(x); let b = |x:BlockingCell<i64>| quiet(x); }";
    let positions: Vec<_> = source.match_indices("|x:").map(|(at, _)| at).collect();
    let shift = positions[1] - positions[0];
    let target = format!(
        "//{}\n{}",
        "s".repeat(shift - 3),
        source.replacen("| quiet(x)", "| noisy(x)", 1)
    );
    let (before, after) = captures(source, &target);
    let r = compare(&before, &after, true).unwrap();
    assert_eq!(r.exit_code(), 1, "{r:?}");
    assert!(
        r.changes
            .iter()
            .any(|c| c["symbol"].as_str().is_some_and(matching::is_lambda)
                && c["effects"]["added"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("may-block"))),
        "{r:?}"
    );
}
