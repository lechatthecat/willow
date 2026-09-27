use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "willow_init_{}_{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn init_new_current_and_named_projects_check() {
    for args in [vec!["init", "basic"], vec!["init", ".", "--name", "basic"]] {
        let f = Fixture::new();
        let output = f.run(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let root = if args[1] == "." {
            f.0.clone()
        } else {
            f.0.join("basic")
        };
        assert_eq!(
            fs::read_to_string(root.join("project.toml")).unwrap(),
            include_str!("fixtures/init/basic/project.toml")
        );
        assert_eq!(
            fs::read_to_string(root.join("src/main.wi")).unwrap(),
            include_str!("fixtures/init/basic/src/main.wi")
        );
        assert_eq!(
            fs::read_to_string(root.join(".gitignore")).unwrap(),
            include_str!("fixtures/init/basic/.gitignore")
        );
        assert!(!root.join("project.lock").exists());
        let output = f.run(&[
            "check",
            root.to_str().unwrap(),
            "--format",
            "ndjson",
            "--protocol-version",
            "1",
        ]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
#[test]
fn invalid_reserved_and_existing_manifest_are_untouched() {
    for name in [
        "bad name", "my-app", "1bad", "std", "std.foo", "std::foo", "",
    ] {
        let f = Fixture::new();
        let result = f.run(&["init", "new", "--name", name]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("--name"));
        assert!(!f.0.join("new").exists());
    }
    let f = Fixture::new();
    fs::write(f.0.join("project.toml"), "original").unwrap();
    assert!(!f.run(&["init", "."]).status.success());
    assert_eq!(
        fs::read_to_string(f.0.join("project.toml")).unwrap(),
        "original"
    );
}
#[test]
fn preserves_main_and_rolls_back_partial_failure() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("src")).unwrap();
    fs::write(f.0.join("src/main.wi"), "original").unwrap();
    fs::write(f.0.join(".gitignore"), "user rules\n").unwrap();
    assert!(f.run(&["init", "."]).status.success());
    assert_eq!(
        fs::read_to_string(f.0.join("src/main.wi")).unwrap(),
        "original"
    );
    assert_eq!(
        fs::read_to_string(f.0.join(".gitignore")).unwrap(),
        "user rules\n"
    );
    let f = Fixture::new();
    fs::write(f.0.join("src"), "keep").unwrap();
    assert!(!f.run(&["init", "."]).status.success());
    assert!(!f.0.join("project.toml").exists());
    assert_eq!(fs::read_to_string(f.0.join("src")).unwrap(), "keep");
    let f = Fixture::new();
    {
        let mut scaffold = willow_compiler::project::init::Scaffold::create(
            &f.0.join("new/nested"),
            Some("basic"),
        )
        .unwrap();
        assert!(
            scaffold
                .write_new(&f.0.join("new/nested/src"), b"fail")
                .is_err()
        );
    }
    assert!(!f.0.join("new").exists());
}

#[test]
fn init_generates_canonical_agent_instructions_and_preserves_existing_files() {
    let f = Fixture::new();
    let out = f.run(&["init", "."]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    for (agent, file) in [("codex", "AGENTS.md"), ("claude", "CLAUDE.md")] {
        let instructions = f.run(&["agent", "instructions", agent]);
        assert!(instructions.status.success());
        assert_eq!(fs::read(f.0.join(file)).unwrap(), instructions.stdout);
    }
    for file in ["AGENTS.md", "CLAUDE.md"] {
        let text = fs::read_to_string(f.0.join(file)).unwrap();
        for guidance in [
            "--project-dir DIR",
            "Human-readable output is the default",
            "not UTF-8 byte offsets",
            "Start with the smallest query",
            "do not run every query as a checklist",
            "Never bypass stale checks",
            "Do not manually search for or extract FunctionId, SymbolId, revision IDs",
            "do not manually run prepare/validate/apply",
            "does not require Git, commits or branches",
        ] {
            assert!(text.contains(guidance), "{file}: missing {guidance}");
        }
        assert!(text.find("- Rename:").unwrap() < text.find("- Effects:").unwrap());
        assert!(!text.contains("Before a measurement session"));
        assert!(!text.contains("willow snapshot clear"));
    }
    assert!(!String::from_utf8_lossy(&out.stdout).contains("[Y/n]"));
    assert!(!String::from_utf8_lossy(&out.stdout).contains("[y/N]"));

    let f = Fixture::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        fs::write(f.0.join(name), "User text\r\n").unwrap();
    }
    assert!(f.run(&["init", "."]).status.success());
    for name in ["AGENTS.md", "CLAUDE.md"] {
        assert_eq!(fs::read(f.0.join(name)).unwrap(), b"User text\r\n");
    }

    for args in [
        ["init", ".", "--ai", "all"].as_slice(),
        &["init", ".", "--ai=codex"],
        &["init", ".", "--yes"],
    ] {
        let f = Fixture::new();
        let out = f.run(args);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("unknown init argument"));
        assert!(!f.0.join("project.toml").exists());
        assert!(!f.0.join("AGENTS.md").exists());
        assert!(!f.0.join("CLAUDE.md").exists());
    }
}

#[test]
fn instructions_json_and_sync_upgrade_preserve_user_bytes() {
    let f = Fixture::new();
    let out = f.run(&["agent", "instructions", "codex", "--format", "json"]);
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["agent"], "codex");
    assert_eq!(value["instruction_schema"], 3);
    let caps = value["capabilities"].as_object().unwrap();
    assert_eq!(caps.len(), 15);
    assert!(caps.values().all(|v| v == &serde_json::json!(true)));
    let current = value["markdown"].as_str().unwrap();
    let old = include_str!("fixtures/agent/v0/AGENTS.md");
    let original = format!("User prefix\r\n{old}User suffix: 日本語\r\n");
    fs::write(f.0.join("AGENTS.md"), &original).unwrap();
    fs::write(f.0.join("CLAUDE.md"), "no managed block").unwrap();
    assert!(f.run(&["agent", "sync"]).status.success());
    assert_eq!(fs::read_to_string(f.0.join("AGENTS.md")).unwrap(), original);
    let out = f.run(&["agent", "sync", "--yes"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 -> 3"));
    assert_eq!(
        fs::read_to_string(f.0.join("AGENTS.md")).unwrap(),
        format!("User prefix\r\n{current}User suffix: 日本語\r\n")
    );
    assert_eq!(
        fs::read_to_string(f.0.join("CLAUDE.md")).unwrap(),
        "no managed block"
    );
    let out = f.run(&["agent", "sync", "--yes"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("already current"));
}

#[test]
fn malformed_managed_blocks_are_rejected_without_writes() {
    let f = Fixture::new();
    for text in [
        "<!-- BEGIN WILLOW MANAGED -->",
        "<!-- BEGIN WILLOW MANAGED -->trailing\n<!-- END WILLOW MANAGED -->",
        "<!-- BEGIN WILLOW MANAGED -->\nprefix<!-- END WILLOW MANAGED -->",
        "<!-- END WILLOW MANAGED -->",
        "<!-- END WILLOW MANAGED -->\n<!-- BEGIN WILLOW MANAGED -->",
        "<!-- BEGIN WILLOW MANAGED -->\n<!-- BEGIN WILLOW MANAGED -->\n<!-- END WILLOW MANAGED -->",
    ] {
        fs::write(f.0.join("AGENTS.md"), text).unwrap();
        assert!(!f.run(&["agent", "sync", "--yes"]).status.success());
        assert_eq!(fs::read_to_string(f.0.join("AGENTS.md")).unwrap(), text);
    }
}

#[test]
fn parent_components_resolve_for_project_files() {
    for path in ["demo/child/..", "new/../demo", "new/a/../../demo"] {
        let f = Fixture::new();
        if path.starts_with("demo/") {
            fs::create_dir_all(f.0.join("demo/child")).unwrap();
        }
        let out = f.run(&["init", path]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let root = f.0.join("demo");
        assert!(
            fs::read_to_string(root.join("project.toml"))
                .unwrap()
                .contains("name = \"demo\"")
        );
        assert!(root.join("AGENTS.md").is_file());
        assert!(root.join("CLAUDE.md").is_file());
        assert!(!f.0.join("new").exists());
    }
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("demo/child")).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_willow"))
        .current_dir(f.0.join("demo/child"))
        .args(["init", ".."])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        fs::read_to_string(f.0.join("demo/project.toml"))
            .unwrap()
            .contains("name = \"demo\"")
    );
}

#[cfg(unix)]
#[test]
fn parent_components_follow_existing_symlinks() {
    let f = Fixture::new();
    fs::create_dir_all(f.0.join("actual/child")).unwrap();
    std::os::unix::fs::symlink("actual/child", f.0.join("link")).unwrap();
    let out = f.run(&["init", "link/../new/../demo"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(f.0.join("actual/demo/project.toml").is_file());
    assert!(f.0.join("actual/demo/AGENTS.md").is_file());
    assert!(!f.0.join("demo").exists());
}

#[test]
fn new_file_publication_preserves_existing_entries_and_cleans_temporaries() {
    use willow_compiler::project::init::Scaffold;
    let f = Fixture::new();
    let mut scaffold = Scaffold::default();
    let path = f.0.join("AGENTS.md");
    fs::write(&path, "original").unwrap();
    assert!(scaffold.write_new(&path, b"replacement").is_err());
    drop(scaffold);
    assert_eq!(fs::read(&path).unwrap(), b"original");
    assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    fs::remove_file(&path).unwrap();
    for size in [0, 1024, 1024 * 1024] {
        let data = vec![42; size];
        let mut scaffold = Scaffold::default();
        scaffold.write_new(&path, &data).unwrap();
        assert_eq!(fs::read(&path).unwrap(), data);
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
        drop(scaffold);
        assert!(!path.exists());
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("missing", &path).unwrap();
        let mut scaffold = Scaffold::default();
        assert!(scaffold.write_new(&path, b"replacement").is_err());
        drop(scaffold);
        assert_eq!(fs::read_link(&path).unwrap(), PathBuf::from("missing"));
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 1);
    }
}

#[test]
fn concurrent_readers_only_observe_complete_new_files() {
    use std::sync::{Arc, Barrier};
    use willow_compiler::project::init::Scaffold;
    let f = Fixture::new();
    let bytes = vec![42; 8 * 1024 * 1024];
    for index in 0..8 {
        let path = f.0.join(format!("published-{index}"));
        let barrier = Arc::new(Barrier::new(2));
        std::thread::scope(|scope| {
            let reader_path = &path;
            let reader_bytes = &bytes;
            let ready = Arc::clone(&barrier);
            let reader = scope.spawn(move || {
                ready.wait();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                loop {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "publication timed out"
                    );
                    match fs::read(reader_path) {
                        Ok(contents) => {
                            assert_eq!(contents.len(), reader_bytes.len());
                            assert_eq!(contents, *reader_bytes);
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            std::thread::yield_now();
                        }
                        Err(error) => panic!("{error}"),
                    }
                }
            });
            let mut scaffold = Scaffold::default();
            barrier.wait();
            scaffold.write_new(&path, &bytes).unwrap();
            reader.join().unwrap();
            drop(scaffold);
        });
    }
    assert_eq!(fs::read_dir(&f.0).unwrap().count(), 0);
}

#[test]
fn generated_direct_workflow_executes_without_snapshot_requests() {
    let f = Fixture::new();
    assert!(f.run(&["init", "."]).status.success());
    fs::write(
        f.0.join("src/main.wi"),
        "fn leaf() {} fn main() { leaf(); }",
    )
    .unwrap();
    let markdown = fs::read_to_string(f.0.join("CLAUDE.md")).unwrap();
    for text in [
        "willow refs",
        "willow type",
        "willow rename",
        "willow effects",
        "willow impact",
    ] {
        assert!(markdown.contains(text));
    }
    for text in ["Before semantic", "queries.json", "REVISION", "FUNCTION_ID"] {
        assert!(!markdown.contains(text));
    }
    for args in [
        vec!["refs", "main::leaf"],
        vec!["type", "src/main.wi:1:4"],
        vec!["effects", "main::leaf"],
        vec!["impact", "main::leaf"],
        vec!["rename", "main::leaf", "smaller", "--dry-run"],
        vec!["rename", "main::leaf", "smaller"],
        vec!["check", "."],
    ] {
        let output = f.run(&args);
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(!f.0.join("snapshot.json").exists());
    assert!(!f.0.join("queries.json").exists());
    assert!(
        fs::read_to_string(f.0.join("src/main.wi"))
            .unwrap()
            .contains("smaller();")
    );
}

#[test]
fn sync_validates_both_files_before_writing_and_leaves_missing_files_absent() {
    let f = Fixture::new();
    assert!(f.run(&["agent", "sync", "--yes"]).status.success());
    assert!(!f.0.join("AGENTS.md").exists());
    assert!(!f.0.join("CLAUDE.md").exists());
    let old = include_str!("fixtures/agent/v0/CLAUDE.md");
    fs::write(f.0.join("CLAUDE.md"), old).unwrap();
    fs::write(f.0.join("AGENTS.md"), "<!-- BEGIN WILLOW MANAGED -->").unwrap();
    assert!(!f.run(&["agent", "sync", "--yes"]).status.success());
    assert_eq!(fs::read_to_string(f.0.join("CLAUDE.md")).unwrap(), old);
}
#[test]
fn hyphenated_directory_requires_valid_explicit_name() {
    let f = Fixture::new();
    let result = f.run(&["init", "my-app"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("--name"));
    assert!(!f.0.join("my-app").exists());
    let result = f.run(&["init", "my-app", "--name", "my_app"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        fs::read_to_string(f.0.join("my-app/project.toml"))
            .unwrap()
            .contains(r#"name = "my_app""#)
    );
}

#[test]
fn init_preserves_existing_agent_bytes_and_creates_only_missing_files() {
    for size in [0, 1024, 1024 * 1024] {
        for existing in ["AGENTS.md", "CLAUDE.md"] {
            let f = Fixture::new();
            let bytes = vec![0xff; size];
            fs::write(f.0.join(existing), &bytes).unwrap();
            assert!(f.run(&["init", "."]).status.success());
            assert_eq!(fs::read(f.0.join(existing)).unwrap(), bytes);
            let other = if existing == "AGENTS.md" {
                "CLAUDE.md"
            } else {
                "AGENTS.md"
            };
            assert!(
                fs::read_to_string(f.0.join(other))
                    .unwrap()
                    .contains("<!-- BEGIN WILLOW MANAGED -->")
            );
            assert_eq!(fs::read_dir(&f.0).unwrap().count(), 5);
        }
    }
}

#[test]
fn init_preserves_agent_directories() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("AGENTS.md")).unwrap();
    fs::write(f.0.join("AGENTS.md/keep"), "user").unwrap();
    assert!(f.run(&["init", "."]).status.success());
    assert_eq!(fs::read(f.0.join("AGENTS.md/keep")).unwrap(), b"user");
    assert!(f.0.join("CLAUDE.md").is_file());
}

#[cfg(unix)]
#[test]
fn init_preserves_agent_symlinks_without_following_them() {
    let f = Fixture::new();
    fs::write(f.0.join("target"), "user").unwrap();
    std::os::unix::fs::symlink("target", f.0.join("AGENTS.md")).unwrap();
    std::os::unix::fs::symlink("missing", f.0.join("CLAUDE.md")).unwrap();
    assert!(f.run(&["init", "."]).status.success());
    assert_eq!(
        fs::read_link(f.0.join("AGENTS.md")).unwrap(),
        PathBuf::from("target")
    );
    assert_eq!(
        fs::read_link(f.0.join("CLAUDE.md")).unwrap(),
        PathBuf::from("missing")
    );
    assert_eq!(fs::read(f.0.join("target")).unwrap(), b"user");
    assert!(!f.0.join("missing").exists());
}
