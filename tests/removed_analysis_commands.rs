use serde_json::Value;
use std::{fs, process::Command};

#[test]
fn removed_commands_reject_without_touching_sources_or_saved_files() {
    let root = std::env::temp_dir().join(format!("willow-removed-cli-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    fs::write(root.join("main.wi"), "fn main() {}\n").unwrap();
    fs::write(root.join("saved.json"), "existing user artifact\n").unwrap();
    let commands: &[&[&str]] = &[
        &["snapshot", "init", "--dir", "managed"],
        &["snapshot", "clear", "--dir", "."],
        &["snapshot", "clear", "--dir", ".", "--dry-run"],
        &["snapshot", "save", "main.wi", "--output", "new.json"],
        &[
            "snapshot",
            "save",
            "main.wi",
            "--output",
            "saved.json",
            "--base",
            "saved.json",
        ],
        &[
            "snapshot",
            "diff",
            "--before",
            "saved.json",
            "--after",
            "saved.json",
        ],
        &["risk", "--before", "saved.json", "--after", "saved.json"],
        &["daemon", "main.wi"],
    ];
    for args in commands {
        for format in [None, Some("human"), Some("ndjson")] {
            let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
            command.current_dir(&root).args(*args);
            if let Some(format) = format {
                command.args(["--format", format]);
            }
            let output = command.output().unwrap();
            assert!(!output.status.success(), "{args:?} {format:?}");
            if format == Some("ndjson") {
                let events: Vec<Value> = String::from_utf8(output.stdout)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(events.len(), 2);
                assert_eq!(events[0]["event"], "request.started");
                assert_eq!(events[1]["event"], "request.finished");
                assert_eq!(events[1]["code"], "WT1001");
                assert_eq!(output.status.code(), Some(2));
                assert!(
                    events[1]["data"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("unknown command")
                );
            } else {
                assert!(String::from_utf8_lossy(&output.stderr).contains("unknown command"));
            }
            assert_eq!(
                fs::read_to_string(root.join("main.wi")).unwrap(),
                "fn main() {}\n"
            );
            assert_eq!(
                fs::read_to_string(root.join("saved.json")).unwrap(),
                "existing user artifact\n"
            );
            assert_eq!(fs::read_dir(&root).unwrap().count(), 2);
        }
    }
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn help_and_generated_instructions_only_advertise_supported_commands() {
    let output = Command::new(env!("CARGO_BIN_EXE_willow"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let help = String::from_utf8(output.stdout).unwrap();
    for name in ["snapshot", "daemon", "risk"] {
        assert!(!help.contains(&format!("willow {name}")));
    }
    for agent in ["codex", "claude"] {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .args(["agent", "instructions", agent, "--format", "json"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let result: Value = serde_json::from_slice(&output.stdout).unwrap();
        let text = result["markdown"].as_str().unwrap();
        for word in ["snapshot", "daemon"] {
            assert!(!text.contains(word));
        }
        for capability in ["snapshots", "snapshot_clear"] {
            assert!(result["capabilities"].get(capability).is_none());
        }
        for capability in [
            "direct_refs",
            "direct_symbol",
            "direct_type",
            "direct_effects",
            "direct_impact",
            "direct_rename",
            "structured_edits",
        ] {
            assert_eq!(result["capabilities"][capability], true);
        }
    }
}
