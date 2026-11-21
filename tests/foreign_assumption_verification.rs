//! Foreign assumption evidence survives captured Git inputs and edit comparison.
use serde_json::Value;
use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicUsize, Ordering},
};

const NATIVE: &str = "#[willow::assume(no_block, no_io)]\npub fn answer() -> i64 { 42 }\n";
const CALLING: &str = "import native;\n@effects(no_io, no_wait) fn guard() -> i64 { return native::answer(); }\nfn main() { println(guard()); }\n";
const CONSTANT: &str = "import native;\n@effects(no_io, no_wait) fn guard() -> i64 { return 42; }\nfn main() { println(guard()); }\n";

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-foreign-verification-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("src")).unwrap();
        let fixture = Self(fs::canonicalize(root).unwrap());
        fixture.write(
            "project.toml",
            "[project]\nname = 'foreign_verification'\nversion = '0.1.0'\n",
        );
        fixture.write(".gitignore", ".willow/\n.willow-edits/\n");
        fixture.write("src/native.rs", NATIVE);
        fixture.write(
            "src/main.wi",
            "import native; fn main() { println(native::answer()); }\n",
        );
        let output = fixture.success(&[
            "query",
            "rust-bridge",
            "--project-dir",
            ".",
            "--offline",
            "--format",
            "json",
        ]);
        let query: Value = serde_json::from_slice(&output.stdout).unwrap();
        let fingerprint = query["review_fingerprint"].as_str().unwrap();
        fixture.write(
            "rust-review.toml",
            &format!("fingerprint = '{fingerprint}'\n"),
        );
        fixture.write("src/main.wi", CALLING);
        fixture.git(&["init", "-q"]);
        fixture.git(&["config", "user.name", "Fixture"]);
        fixture.git(&["config", "user.email", "fixture@example.invalid"]);
        fixture.commit(&[
            ".gitignore",
            "project.toml",
            "rust-review.toml",
            "src/main.wi",
            "src/native.rs",
        ]);
        fixture
    }

    fn write(&self, path: &str, text: &str) {
        fs::write(self.0.join(path), text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }

    fn success(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn commit(&self, paths: &[&str]) {
        let mut args = vec!["add", "--"];
        args.extend(paths);
        self.git(&args);
        let names = self.git(&["diff", "--cached", "--name-only"]);
        let mut expected = paths.to_vec();
        expected.sort_unstable();
        assert_eq!(names.lines().collect::<Vec<_>>(), expected);
        let diff = self.git(&["diff", "--cached"]);
        assert!(!diff.contains("requirements/"));
        assert_eq!(diff.matches("diff --git ").count(), paths.len());
        self.git(&["diff", "--cached", "--check"]);
        self.git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-qm",
            "Fixture state",
        ]);
    }

    fn verify(&self, success: bool) -> Value {
        let output = self.run(&[
            "verify-edit",
            "--root",
            ".",
            "--base",
            "HEAD",
            "--schema",
            "v3",
            "--format",
            "json",
        ]);
        let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "{error}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(output.status.success(), success, "{report}");
        assert_eq!(report["schema"], "willow.semantic-diff.v3");
        report
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn assumptions<'a>(report: &'a Value, side: &str) -> &'a Value {
    let functions = report["summary"]["assumptions"][side]
        .as_object()
        .unwrap_or_else(|| panic!("missing assumption inventory: {report}"));
    functions
        .iter()
        .find(|(name, _)| name.ends_with("::guard#0"))
        .map_or(&Value::Null, |(_, value)| value)
}

fn assert_assumed(evidence: &Value) {
    for contract in ["no_io", "no_wait"] {
        assert_eq!(evidence[contract]["kind"], "assumption", "{evidence}");
        assert_eq!(evidence[contract]["location"]["path"], "src/native.rs");
        let start = evidence[contract]["location"]["start"].as_u64().unwrap() as usize;
        let end = evidence[contract]["location"]["end"].as_u64().unwrap() as usize;
        assert_eq!(&NATIVE[start..end], "#[willow::assume(no_block, no_io)]");
        assert!(!evidence[contract]["function"].is_null());
    }
}

fn assert_contracts_unproven(report: &Value) {
    assert_eq!(report["status"], "incomplete", "{report}");
    let diagnostics = report["summary"]["diagnostics"]["target"]
        .as_array()
        .unwrap();
    for contract in ["no_io", "no_wait"] {
        assert!(
            diagnostics.iter().any(|diagnostic| {
                diagnostic["code"] == "E2612"
                    && diagnostic["message"]
                        .as_str()
                        .is_some_and(|message| message.contains(&format!("`{contract}`")))
            }),
            "missing fail-closed {contract} diagnostic: {report}"
        );
    }
    let guard = report["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| {
            change["symbol"]
                .as_str()
                .is_some_and(|name| name.ends_with("::guard#0"))
        })
        .unwrap();
    for effect in ["may-io", "may-block"] {
        assert!(
            guard["effects"]["target"]
                .as_array()
                .unwrap()
                .contains(&Value::String(effect.into())),
            "missing external capability {effect}: {report}"
        );
        let cause = guard["cause"]
            .as_array()
            .unwrap()
            .iter()
            .find(|cause| cause["side"] == "target" && cause["effect"][0] == effect)
            .unwrap();
        assert_eq!(cause["certainty"], "conservative-bound", "{report}");
    }
}

#[test]
fn foreign_assumption_verify_edit_keeps_unchanged_and_edited_evidence() {
    let fixture = Fixture::new();
    let unchanged = fixture.verify(true);
    assert_eq!(unchanged["status"], "complete");
    assert_assumed(assumptions(&unchanged, "base"));
    assert_assumed(assumptions(&unchanged, "target"));
    assert_eq!(
        assumptions(&unchanged, "base"),
        assumptions(&unchanged, "target")
    );

    fixture.write("src/main.wi", CONSTANT);
    let removed = fixture.verify(true);
    assert_assumed(assumptions(&removed, "base"));
    assert!(assumptions(&removed, "target").is_null());
    fixture.commit(&["src/main.wi"]);
    let cold_constant = fixture.verify(true);
    assert!(assumptions(&cold_constant, "base").is_null());
    assert!(assumptions(&cold_constant, "target").is_null());

    fixture.write("src/main.wi", CALLING);
    let restored = fixture.verify(true);
    assert!(assumptions(&restored, "base").is_null());
    assert_assumed(assumptions(&restored, "target"));
    assert_eq!(
        assumptions(&restored, "target"),
        assumptions(&unchanged, "target")
    );

    let old_schema = fixture.run(&[
        "verify-edit",
        "--base",
        "HEAD",
        "--schema",
        "v1",
        "--format",
        "json",
    ]);
    assert_eq!(old_schema.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&old_schema.stderr).contains("schema"));
}

#[test]
fn foreign_assumption_verify_edit_expires_source_and_missing_review() {
    let fixture = Fixture::new();
    fixture.write("src/native.rs", &NATIVE.replace("42", "43"));
    let stale = fixture.verify(false);
    assert_assumed(assumptions(&stale, "base"));
    assert!(assumptions(&stale, "target").is_null());
    assert_contracts_unproven(&stale);

    fixture.write("src/native.rs", NATIVE);
    fs::remove_file(fixture.0.join("rust-review.toml")).unwrap();
    let missing = fixture.verify(false);
    assert_assumed(assumptions(&missing, "base"));
    assert!(assumptions(&missing, "target").is_null());
    assert_contracts_unproven(&missing);
}

#[test]
fn foreign_assumption_verify_edit_captures_path_dependency_data() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("dependency/src")).unwrap();
    fixture.write(
        "dependency/Cargo.toml",
        "[package]\nname='asset_probe'\nversion='0.1.0'\nedition='2024'\n[workspace]\n",
    );
    fixture.write(
        "dependency/src/lib.rs",
        "pub fn answer() -> i64 { include_bytes!(\"../data.bin\")[0] as i64 }\n",
    );
    fixture.write("dependency/data.bin", "*");
    fixture.write(
        "project.toml",
        "[project]\nname='foreign_verification'\nversion='0.1.0'\n[rust-dependencies]\nasset_probe={path='dependency'}\n",
    );
    fixture.write(
        "src/native.rs",
        "#[willow::assume(no_block, no_io)]\npub fn answer() -> i64 { asset_probe::answer() }\n",
    );
    let output = fixture.success(&[
        "query",
        "rust-bridge",
        "--project-dir",
        ".",
        "--offline",
        "--format",
        "json",
    ]);
    let query: Value = serde_json::from_slice(&output.stdout).unwrap();
    let fingerprint = query["review_fingerprint"].as_str().unwrap();
    fixture.write(
        "rust-review.toml",
        &format!("fingerprint = '{fingerprint}'\n"),
    );
    fixture.commit(&[
        "project.toml",
        "rust-review.toml",
        "src/native.rs",
        "dependency/Cargo.toml",
        "dependency/src/lib.rs",
        "dependency/data.bin",
    ]);
    let reviewed = fixture.verify(true);
    assert_assumed(assumptions(&reviewed, "base"));
    assert_assumed(assumptions(&reviewed, "target"));

    fixture.write("dependency/data.bin", "+");
    let changed = fixture.verify(false);
    assert_assumed(assumptions(&changed, "base"));
    assert!(assumptions(&changed, "target").is_null());
    assert_contracts_unproven(&changed);
}
