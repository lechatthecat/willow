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
    for args in [
        vec!["init", "basic", "--ai", "none"],
        vec!["init", ".", "--name", "basic", "--ai", "none"],
    ] {
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
    for name in ["bad name", "1bad", "std", "std.foo", "std::foo", ""] {
        let f = Fixture::new();
        let result = f.run(&["init", "new", "--name", name, "--ai", "none"]);
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
    assert!(f.run(&["init", "."]).status.success());
    assert_eq!(
        fs::read_to_string(f.0.join("src/main.wi")).unwrap(),
        "original"
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
fn ai_selection_is_explicit_and_non_tty_auto_is_silent() {
    for (selection, codex, claude) in [
        ("none", false, false),
        ("auto", false, false),
        ("codex", true, false),
        ("claude", false, true),
        ("codex,claude", true, true),
        ("all", true, true),
    ] {
        let f = Fixture::new();
        let out = f.run(&["init", ".", "--ai", selection]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(f.0.join("AGENTS.md").exists(), codex);
        assert_eq!(f.0.join("CLAUDE.md").exists(), claude);
        assert!(!String::from_utf8_lossy(&out.stdout).contains("[Y/n]"));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("[y/N]"));
    }
}

#[test]
fn existing_ai_files_preserved_and_ai_failure_rolls_back_project() {
    let f = Fixture::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        fs::write(f.0.join(name), "User text\r\n").unwrap();
    }
    assert!(
        f.run(&["init", ".", "--ai", "all", "--yes"])
            .status
            .success()
    );
    for name in ["AGENTS.md", "CLAUDE.md"] {
        assert_eq!(fs::read(f.0.join(name)).unwrap(), b"User text\r\n");
    }
    let f = Fixture::new();
    fs::create_dir(f.0.join("AGENTS.md")).unwrap();
    assert!(!f.run(&["init", ".", "--ai", "all"]).status.success());
    assert!(!f.0.join("project.toml").exists());
    assert!(!f.0.join("src").exists());
    assert!(!f.0.join("CLAUDE.md").exists());
    assert!(f.0.join("AGENTS.md").is_dir());
}

#[test]
fn instructions_json_and_sync_upgrade_preserve_user_bytes() {
    let f = Fixture::new();
    let out = f.run(&["agent", "instructions", "codex", "--format", "json"]);
    assert!(out.status.success());
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["agent"], "codex");
    assert_eq!(value["instruction_schema"], 2);
    let caps = value["capabilities"].as_object().unwrap();
    assert_eq!(caps.len(), 8);
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
    assert!(String::from_utf8_lossy(&out.stdout).contains("1 -> 2"));
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
fn parent_components_resolve_for_project_and_ai_files() {
    for path in ["demo/child/..", "new/../demo", "new/a/../../demo"] {
        let f = Fixture::new();
        if path.starts_with("demo/") {
            fs::create_dir_all(f.0.join("demo/child")).unwrap();
        }
        let out = f.run(&["init", path, "--ai", "all"]);
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
        .args(["init", "..", "--ai", "none"])
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
    let out = f.run(&["init", "link/../new/../demo", "--ai", "all"]);
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
fn generated_snapshot_query_and_callers_workflow_executes_and_detects_staleness() {
    let f = Fixture::new();
    assert!(f.run(&["init", ".", "--ai", "all"]).status.success());
    fs::write(
        f.0.join("src/main.wi"),
        "fn leaf() {} fn main() { leaf(); }",
    )
    .unwrap();
    let markdown = fs::read_to_string(f.0.join("AGENTS.md")).unwrap();
    let display = f.run(&["agent", "instructions", "codex"]);
    assert_eq!(display.stdout, markdown.as_bytes());

    let run = |command: &str| {
        let args: Vec<_> = command.split_whitespace().skip(1).collect();
        let out = f.run(&args);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stdout)
        );
        let events: Vec<serde_json::Value> = String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        for (seq, event) in events.iter().enumerate() {
            assert_eq!(event["seq"], seq);
            assert_eq!(event["schema_version"], 1);
            assert_eq!(event["stream_id"], events[0]["stream_id"]);
        }
        assert_eq!(events[0]["event"], "request.started");
        let terminal = events.last().unwrap();
        assert_eq!(terminal["event"], "request.finished");
        assert_eq!(terminal["data"]["status"], "ok");
        assert_eq!(terminal["data"]["exit_code"], 0);
        events
            .iter()
            .find(|event| event["event"] == "analysis.result")
            .map(|event| event["data"].clone())
            .unwrap_or(serde_json::Value::Null)
    };
    let commands: Vec<_> = markdown
        .split('`')
        .filter(|s| s.starts_with("willow "))
        .collect();
    let snapshot_command = *commands
        .iter()
        .find(|c| c.starts_with("willow snapshot "))
        .unwrap();
    let query_command = *commands
        .iter()
        .find(|c| c.starts_with("willow query "))
        .unwrap();
    let callers_command = *commands
        .iter()
        .find(|c| c.starts_with("willow impact "))
        .unwrap();
    let check_command = *commands
        .iter()
        .find(|c| c.starts_with("willow check "))
        .unwrap();
    let saved = run(snapshot_command);
    let snapshot: serde_json::Value =
        serde_json::from_slice(&fs::read(f.0.join("snapshot.json")).unwrap()).unwrap();
    assert_eq!(saved["revision"], snapshot["revision"]);
    let revision = saved["revision"].as_str().unwrap();
    let functions = snapshot["functions"].as_array().unwrap();
    let leaf = functions.iter().find(|v| v["name"] == "leaf").unwrap();
    let main = functions.iter().find(|v| v["name"] == "main").unwrap();
    let id = leaf["id"].as_str().unwrap();
    let mut examples = Vec::new();
    for block in markdown.split("```json\n").skip(1) {
        let example = block
            .split("```")
            .next()
            .unwrap()
            .replace("REVISION", revision)
            .replace("SYMBOL_ID", id);
        fs::write(f.0.join("queries.json"), &example).unwrap();
        let result = run(query_command);
        assert_eq!(result["revision"], revision);
        assert_eq!(result["results"][0]["result"]["status"], "ok");
        examples.push(example);
    }
    assert_eq!(examples.len(), 2);
    let impact = run(&callers_command
        .replace("FUNCTION_ID", id)
        .replace("REVISION", revision));
    assert!(
        impact["impact"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["id"] == main["id"])
    );

    fs::write(
        f.0.join("src/main.wi"),
        "fn leaf() {} fn main() { leaf(); leaf(); }",
    )
    .unwrap();
    let stale = run(query_command);
    assert_eq!(stale["results"][0]["status"], "stale");
    run(check_command);
    let fresh = run(&snapshot_command.replace("snapshot.json", "fresh-snapshot.json"));
    assert_ne!(fresh["revision"], revision);
    fs::write(
        f.0.join("queries.json"),
        examples[0].replace(revision, fresh["revision"].as_str().unwrap()),
    )
    .unwrap();
    assert_eq!(run(query_command)["results"][0]["result"]["status"], "ok");
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
