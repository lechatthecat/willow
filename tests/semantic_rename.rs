use serde_json::Value;
use std::{fs, path::PathBuf, process::Command};

struct Fixture(PathBuf);
impl Fixture {
    fn new(source: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "willow-direct-rename-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("main.wi"), source).unwrap();
        Self(fs::canonicalize(root).unwrap())
    }
    fn write(&self, path: &str, text: &str) {
        fs::write(self.0.join(path), text).unwrap();
    }
    fn run(&self, args: &[&str], code: i32) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .args(["--source", "main.wi", "--format=json"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn direct_rename_previews_without_metadata_and_applies_cross_file_imports() {
    for import in [
        "import helper;",
        "import helper::value;",
        "import helper::value as alias;",
    ] {
        let call = if import.contains("alias") {
            "alias()"
        } else if import.contains("::value") {
            "value()"
        } else {
            "helper::value()"
        };
        let f = Fixture::new(&format!("{import} fn main() {{ println({call}); }}"));
        f.write("helper.wi", "pub fn value() -> i64 { return 2; }");
        let before = fs::read(f.0.join("main.wi")).unwrap();
        let preview = f.run(&["rename", "helper::value", "answer", "--dry-run"], 0);
        assert!(preview["result"]["changes"].as_array().unwrap().len() == 2);
        assert_eq!(fs::read(f.0.join("main.wi")).unwrap(), before);
        assert!(!f.0.join(".willow-edits").exists());
        let applied = f.run(&["rename", "helper::value", "answer"], 0);
        assert_eq!(applied["result"]["files_changed"], 2);
        assert_eq!(applied["result"]["validation"], "passed");
        assert!(applied["result"].get("transaction").is_none());
        assert!(
            fs::read_to_string(f.0.join("helper.wi"))
                .unwrap()
                .contains("fn answer")
        );
        f.run(&["refs", "helper::answer"], 0);
        let main = fs::read_to_string(f.0.join("main.wi")).unwrap();
        assert!(main.contains(if import.contains("alias") {
            "alias()"
        } else {
            "answer()"
        }));
    }
}

#[test]
fn rename_rejections_preserve_sources_and_report_collision_location() {
    for (selector, name) in [
        ("missing", "new_name"),
        ("value", "answer"),
        ("value", "if"),
        ("value", "value"),
    ] {
        let source = "fn value() -> i64 { return 1; } fn answer() {} fn main() { value(); }";
        let f = Fixture::new(source);
        let result = f.run(&["rename", selector, name], 1);
        assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
        if name == "answer" {
            assert_eq!(result["location"]["path"], "main.wi");
            assert_eq!(result["location"]["line"], 1);
            assert!(result["location"]["column"].as_u64().unwrap() > 1);
        }
    }
    let f = Fixture::new("import helper; fn value() {} fn main() { helper::value(); value(); }");
    f.write("helper.wi", "pub fn value() {}");
    assert_eq!(
        f.run(&["rename", "value", "answer"], 1)["status"],
        "ambiguous"
    );
    assert!(!f.0.join(".willow-edits").exists());
}

#[test]
fn unsupported_binding_and_validation_failure_preserve_source() {
    for (source, selector, name) in [
        (
            "fn main() { let local = 1; println(local); }",
            "local",
            "other",
        ),
        (
            "fn value() -> i64 { return 1; } fn main() { let answer = 3; println(value()); }",
            "value",
            "answer",
        ),
    ] {
        let f = Fixture::new(source);
        let result = f.run(&["rename", selector, name], 1);
        assert_eq!(result["status"], "error");
        assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
        assert!(!f.0.join(".willow-edits/active").exists());
    }
}

#[test]
fn source_race_after_resolution_is_rejected_before_application() {
    use willow_compiler::{
        CompilerOptions, CompilerSession,
        ai::direct::{DirectSession, SelectorFilter},
        diagnostics::HumanEmitter,
    };
    let f = Fixture::new("fn value() {} fn main() { value(); }");
    let entry = f.0.join("main.wi");
    let snapshot =
        CompilerSession::new(entry.to_str().unwrap(), "", &CompilerOptions::debug(), None)
            .analysis_for_edit_with_emitter(&mut HumanEmitter)
            .unwrap();
    let session = DirectSession::new(snapshot).unwrap();
    let edited = "fn value() {} fn main() { value(); value(); }";
    f.write("main.wi", edited);
    assert!(
        session
            .rename(
                &entry,
                false,
                "value",
                &SelectorFilter::default(),
                "answer",
                false
            )
            .is_err()
    );
    assert_eq!(fs::read_to_string(entry).unwrap(), edited);
}

#[test]
fn configuration_changes_after_analysis_reject_preview_and_apply() {
    use willow_compiler::{
        CompilerOptions, CompilerSession,
        ai::direct::{DirectSession, SelectorFilter},
        diagnostics::HumanEmitter,
    };
    for file in ["project.toml", "project.lock"] {
        for dry_run in [false, true] {
            let source = "fn value() {} fn main() { value(); }";
            let f = Fixture::new(source);
            let entry = f.0.join("main.wi");
            let snapshot =
                CompilerSession::new(entry.to_str().unwrap(), "", &CompilerOptions::debug(), None)
                    .analysis_for_edit_with_emitter(&mut HumanEmitter)
                    .unwrap();
            f.write(file, "changed configuration");
            let error = DirectSession::new(snapshot)
                .unwrap()
                .rename(
                    &entry,
                    false,
                    "value",
                    &SelectorFilter::default(),
                    "answer",
                    dry_run,
                )
                .unwrap_err();
            assert!(
                error.to_string().contains("stale analysis configuration"),
                "{error:#}"
            );
            assert_eq!(fs::read_to_string(entry).unwrap(), source);
        }
    }
}

#[test]
fn method_dispatch_family_rename_updates_both_implementations() {
    let f = Fixture::new(
        "open class Base { pub open fn value(self) -> i64 { return 1; } } class Child extends Base { pub override fn value(self) -> i64 { return 2; } } fn call(x: Base) -> i64 { return x.value(); } fn main() { println(call(new Child())); }",
    );
    let result = f.run(&["rename", "main::Base::value", "answer"], 0);
    assert_eq!(result["result"]["files_changed"], 1);
    assert_eq!(result["result"]["references_updated"], 1);
    let text = fs::read_to_string(f.0.join("main.wi")).unwrap();
    assert_eq!(text.matches("fn answer").count(), 2);
    assert!(text.contains("x.answer()"));
}

#[test]
fn project_rename_dry_run_and_failure_never_publish_a_lock() {
    for name in ["answer", "main"] {
        let source = "fn value() {} fn main() { value(); }";
        let f = Fixture::new(source);
        fs::create_dir(f.0.join("src")).unwrap();
        f.write("project.toml", "[willow]\nmanifest-version=1\n[project]\nname='demo'\nversion='0.1.0'\nentry='main.wi'\n");
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&f.0)
            .args(["rename", "main::value", name, "--dry-run"])
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            name == "answer",
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!f.0.join("project.lock").exists());
        assert!(!f.0.join(".willow-edits").exists());
        assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
    }
}

#[test]
fn direct_rename_rejects_entry_that_disagrees_with_current_manifest() {
    use willow_compiler::{
        CompilerOptions, CompilerSession,
        ai::direct::{DirectSession, SelectorFilter},
        diagnostics::HumanEmitter,
    };
    let f = Fixture::new("fn value() {} fn main() { value(); }");
    f.write("alternate.wi", "fn main() {}");
    f.write(
        "project.toml",
        "[project]\nname='demo'\nversion='0.1.0'\nentry='alternate.wi'\n",
    );
    let entry = f.0.join("main.wi");
    let snapshot = CompilerSession::new(
        entry.to_str().unwrap(),
        "",
        &CompilerOptions::debug(),
        Some(f.0.clone()),
    )
    .analysis_for_edit_with_emitter(&mut HumanEmitter)
    .unwrap();
    let error = DirectSession::new(snapshot)
        .unwrap()
        .rename(
            &entry,
            true,
            "value",
            &SelectorFilter::default(),
            "answer",
            true,
        )
        .unwrap_err();
    assert!(error.to_string().contains("entry differs"), "{error:#}");
    assert!(!f.0.join("project.lock").exists());
    // The ordinary analysis API still honors an explicitly selected alternate
    // source; manifest-entry consistency is required by direct project selection.
    CompilerSession::new(
        entry.to_str().unwrap(),
        "",
        &CompilerOptions::debug(),
        Some(f.0.clone()),
    )
    .analysis_with_emitter(&mut HumanEmitter)
    .unwrap();
}

#[test]
fn unsupported_symbol_kinds_and_interface_coverage_have_actionable_reasons() {
    let source = "interface Dispatcher { fn assign(self) -> i64; }\nclass Nearest implements Dispatcher { pub fn assign(self) -> i64 { return 1; } }\nclass Passenger { pub board: i64; pub init(self) { self.board = 0; } }\nenum Dir { Idle, Up }\nfn main() {}\n";
    for dry in [false, true] {
        for (selector, reason) in [
            ("main::Dispatcher::assign", "unsupported rename target"),
            ("main::Passenger::board", "unsupported rename target"),
            ("main::Dir::Idle", "unsupported rename target"),
            (
                "main::Nearest::assign",
                "interface contract declarations are not covered",
            ),
        ] {
            let f = Fixture::new(source);
            let mut args = vec!["rename", selector, "renamed"];
            if dry {
                args.push("--dry-run");
            }
            let result = f.run(&args, 1);
            assert!(
                result["message"].as_str().unwrap().contains(reason),
                "{result}"
            );
            assert_eq!(result["location"]["path"], "main.wi");
            assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
            assert!(!f.0.join(".willow-edits/active").exists());
        }
    }
}
