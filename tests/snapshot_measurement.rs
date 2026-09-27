use serde_json::Value;
use std::{fs, path::PathBuf, process::Command};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-measurement-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("main.wi"),
            "fn value() -> i64 { return 1; } fn main() { value(); }",
        )
        .unwrap();
        Self(fs::canonicalize(root).unwrap())
    }
    fn run(&self, args: &[&str], code: i32) -> Vec<Value> {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let events: Vec<Value> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.last().unwrap()["data"]["exit_code"], code);
        events
    }
    fn result(&self, args: &[&str], code: i32) -> Value {
        self.run(args, code)
            .into_iter()
            .find(|e| e["event"] == "analysis.result")
            .unwrap()["data"]
            .take()
    }
    fn managed_snapshot(&self, source: &str, name: &str, options: &[&str]) -> Value {
        let mut args = vec![
            "snapshot",
            "save",
            source,
            "--managed-dir",
            "measure",
            "--output",
            name,
        ];
        args.extend_from_slice(options);
        let result = self.result(&args, 0);
        let snapshot: Value =
            serde_json::from_slice(&fs::read(self.0.join("measure").join(name)).unwrap()).unwrap();
        assert_eq!(snapshot["revision"], result["revision"]);
        snapshot
    }
    fn fresh_snapshot(&self, source: &str, options: &[&str]) -> Value {
        let cleared = self.result(&["snapshot", "clear", "--dir", "measure"], 0);
        assert_eq!(cleared["success"], true);
        assert_eq!(cleared["deleted_count"], cleared["planned_count"]);
        assert_eq!(
            cleared["deleted_registration_count"],
            cleared["planned_registration_count"]
        );
        self.managed_snapshot(source, "base.json", options)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn managed_full_delta_roundtrip_and_clear_preserve_project_files() {
    let f = Fixture::new();
    f.run(&["snapshot", "init", "--dir", "measure"], 0);
    f.run(
        &[
            "snapshot",
            "save",
            "main.wi",
            "--managed-dir",
            "measure",
            "--output",
            "base.json",
        ],
        0,
    );
    fs::write(
        f.0.join("main.wi"),
        "fn value() -> i64 { return 2; } fn main() { value(); }",
    )
    .unwrap();
    f.run(
        &[
            "snapshot",
            "save",
            "main.wi",
            "--managed-dir",
            "measure",
            "--output",
            "delta.json",
            "--base",
            "base.json",
        ],
        0,
    );
    let diff = f.result(
        &[
            "snapshot",
            "diff",
            "--before",
            "measure/base.json",
            "--after",
            "measure/delta.json",
        ],
        0,
    );
    assert!(!diff["difference"]["changes"].as_array().unwrap().is_empty());
    let dry = f.result(&["snapshot", "clear", "--dir", "measure", "--dry-run"], 0);
    assert_eq!(dry["deleted_count"], 0);
    assert_eq!(dry["planned_count"], 2);
    assert!(dry["planned_bytes"].as_u64().unwrap() > 0);
    assert_eq!(dry["directory_scans"], 1);
    let source = fs::read(f.0.join("main.wi")).unwrap();
    fs::write(f.0.join("measure/history.txt"), "unrelated").unwrap();
    let refused = f.result(&["snapshot", "clear", "--dir", "measure"], 1);
    assert_eq!(refused["success"], false);
    assert_eq!(refused["deleted_count"], 0);
    assert!(f.0.join("measure/base.json").exists());
    fs::remove_file(f.0.join("measure/history.txt")).unwrap();
    let cleared = f.result(&["snapshot", "clear", "--dir", "measure"], 0);
    assert_eq!(cleared["deleted_count"], 2);
    assert_eq!(cleared["deleted_bytes"], dry["planned_bytes"]);
    assert_eq!(cleared["os_page_cache"], "unchanged");
    assert_eq!(fs::read(f.0.join("main.wi")).unwrap(), source);
    assert_eq!(
        f.result(&["snapshot", "clear", "--dir", "measure"], 0)["deleted_count"],
        0
    );
    assert_eq!(
        f.result(&["snapshot", "clear", "--dir", "absent"], 0)["skipped"],
        serde_json::json!(["absent"])
    );
    assert_eq!(
        f.result(&["snapshot", "clear", "--dir", "."], 1)["success"],
        false
    );
}

#[test]
fn managed_options_reject_escapes_and_invalid_combinations() {
    let f = Fixture::new();
    for args in [
        vec!["snapshot", "clear"],
        vec!["snapshot", "clear", "--dir", "measure", "main.wi"],
        vec!["snapshot", "init", "--dir", "measure", "--dry-run"],
        vec![
            "snapshot", "save", "main.wi", "--dir", "measure", "--output", "x.json",
        ],
        vec!["snapshot", "diff", "--managed-dir", "measure"],
    ] {
        f.run(&args, 2);
    }
    f.run(&["snapshot", "init", "--dir", "measure"], 0);
    f.run(
        &[
            "snapshot",
            "save",
            "main.wi",
            "--managed-dir",
            "measure",
            "--output",
            "../outside.json",
        ],
        1,
    );
    assert!(!f.0.join("outside.json").exists());
    f.run(
        &[
            "snapshot",
            "save",
            "main.wi",
            "--managed-dir",
            "measure",
            "--output",
            "delta.json",
            "--base",
            "../outside.json",
        ],
        1,
    );
    assert!(!f.0.join("measure/delta.json").exists());
}

fn has_function(snapshot: &Value, name: &str) -> bool {
    snapshot["functions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|function| function["name"] == name)
}

#[test]
fn actual_source_transitions_restore_a_revision_and_discard_b_facts() {
    let f = Fixture::new();
    f.run(&["snapshot", "init", "--dir", "measure"], 0);
    let a = "fn only_a() -> i64 { return 11; } fn main() { only_a(); }";
    let b = "fn only_b() -> i64 { return 22; } fn main() { only_b(); }";
    fs::write(f.0.join("main.wi"), a).unwrap();
    let first_a = f.fresh_snapshot("main.wi", &[]);
    assert!(has_function(&first_a, "only_a"));
    assert!(!has_function(&first_a, "only_b"));
    // No Git operations are needed: snapshots must depend on actual source
    // content, including unsaved-to-Git edits in the same workspace.
    fs::write(f.0.join("main.wi"), b).unwrap();
    // A normal save with existing history must see current source without clear.
    let uncleared_b = f.managed_snapshot("main.wi", "without-clear.json", &[]);
    assert_ne!(first_a["revision"], uncleared_b["revision"]);
    assert!(has_function(&uncleared_b, "only_b"));
    assert!(!has_function(&uncleared_b, "only_a"));
    let fresh_b = f.fresh_snapshot("main.wi", &[]);
    assert_eq!(fresh_b, uncleared_b);
    assert_eq!(first_a["compatibility"], fresh_b["compatibility"]);
    fs::write(f.0.join("main.wi"), a).unwrap();
    let restored_a = f.fresh_snapshot("main.wi", &[]);
    assert_eq!(restored_a, first_a);
    // Recreating the baseline with unchanged contents must not change revision.
    let unchanged_a = f.fresh_snapshot("main.wi", &[]);
    assert_eq!(unchanged_a, first_a);
}

#[test]
fn actual_manifest_lock_entry_and_options_transitions_update_compatibility() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("src")).unwrap();
    let manifest = "[project]\nname='measurement'\nversion='0.1.0'\nentry='main.wi'\n[willow]\nmanifest-version=1\n";
    fs::write(f.0.join("project.toml"), manifest).unwrap();
    fs::write(
        f.0.join("alternate.wi"),
        "fn alternate() -> i64 { return 33; } fn main() { alternate(); }",
    )
    .unwrap();
    f.run(&["snapshot", "init", "--dir", "measure"], 0);
    let baseline = f.fresh_snapshot(".", &[]);
    let lock = fs::read_to_string(f.0.join("project.lock")).unwrap();
    // Valid metadata-only edits isolate configuration hashing from source facts.
    fs::write(
        f.0.join("project.toml"),
        format!("{manifest}# changed manifest bytes\n"),
    )
    .unwrap();
    let changed_manifest = f.fresh_snapshot(".", &[]);
    assert_ne!(changed_manifest["compatibility"], baseline["compatibility"]);
    assert_ne!(changed_manifest["revision"], baseline["revision"]);
    assert_eq!(changed_manifest["sources"], baseline["sources"]);
    fs::write(f.0.join("project.toml"), manifest).unwrap();
    let lock_changed = format!("{lock}\n# changed lock bytes\n");
    fs::write(f.0.join("project.lock"), &lock_changed).unwrap();
    let changed_lock = f.fresh_snapshot(".", &[]);
    assert_ne!(changed_lock["compatibility"], baseline["compatibility"]);
    assert_eq!(changed_lock["sources"], baseline["sources"]);
    assert_eq!(
        fs::read_to_string(f.0.join("project.lock")).unwrap(),
        lock_changed
    );
    fs::write(f.0.join("project.lock"), &lock).unwrap();
    assert_eq!(f.fresh_snapshot(".", &[]), baseline);
    let release = f.fresh_snapshot(".", &["--release"]);
    assert_ne!(release["compatibility"], baseline["compatibility"]);
    assert_eq!(release["sources"], baseline["sources"]);
    assert_eq!(f.fresh_snapshot(".", &[]), baseline);

    fs::write(
        f.0.join("project.toml"),
        manifest.replace("entry='main.wi'", "entry='alternate.wi'"),
    )
    .unwrap();
    let alternate = f.fresh_snapshot(".", &[]);
    assert_ne!(alternate["compatibility"], baseline["compatibility"]);
    assert!(has_function(&alternate, "alternate"));
    assert!(!has_function(&alternate, "value"));
    assert!(
        alternate["sources"]
            .as_object()
            .unwrap()
            .keys()
            .any(|path| path.ends_with("alternate.wi"))
    );
    fs::write(f.0.join("project.toml"), manifest).unwrap();
    let restored = f.fresh_snapshot(".", &[]);
    assert_eq!(restored, baseline);
}

#[test]
fn independent_workspace_baselines_remain_separate_and_reject_cross_workspace_delta() {
    let first = Fixture::new();
    let second = Fixture::new();
    for fixture in [&first, &second] {
        fixture.run(&["snapshot", "init", "--dir", "measure"], 0);
    }
    let a = first.fresh_snapshot("main.wi", &[]);
    let b = second.fresh_snapshot("main.wi", &[]);
    assert_ne!(a["workspace"], b["workspace"]);
    assert_ne!(a["compatibility"], b["compatibility"]);
    assert_ne!(a["revision"], b["revision"]);
    let external_base = first.0.join("measure/base.json");
    second.run(
        &[
            "snapshot",
            "save",
            "main.wi",
            "--output",
            "invalid.json",
            "--base",
            external_base.to_str().unwrap(),
        ],
        1,
    );
    assert!(!second.0.join("invalid.json").exists());
    first.result(&["snapshot", "clear", "--dir", "measure"], 0);
    assert!(second.0.join("measure/base.json").exists());
    assert_eq!(second.fresh_snapshot("main.wi", &[]), b);
}

#[test]
fn human_clear_refusal_returns_nonzero_and_preserves_files() {
    let f = Fixture::new();
    f.run(&["snapshot", "init", "--dir", "measure"], 0);
    f.managed_snapshot("main.wi", "base.json", &[]);
    fs::write(f.0.join("measure/history.txt"), "preserve").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(&f.0)
        .args(["snapshot", "clear", "--dir", "measure", "--format", "human"])
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["success"], false);
    assert_eq!(result["deleted_count"], 0);
    assert!(f.0.join("measure/base.json").exists());
    assert_eq!(
        fs::read_to_string(f.0.join("measure/history.txt")).unwrap(),
        "preserve"
    );
}
