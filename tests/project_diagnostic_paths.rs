use std::{fs, path::Path, process::Command};

#[test]
fn project_diagnostic_paths() {
    let root = std::env::temp_dir().join(format!("willow-diagnostic-paths-{}", std::process::id()));
    fs::create_dir_all(root.join("src/nested")).unwrap();
    fs::write(
        root.join("project.toml"),
        "[project]\nname = \"diagnostic_paths\"\nversion = \"0.1.0\"\nentry = \"src/main.wi\"\n",
    )
    .unwrap();
    for (name, entry, module, expected) in [
        (
            "entry-type",
            "fn main() { let n: i64 = true; }",
            "",
            "src/main.wi",
        ),
        ("entry-parse", "fn main() { let = ; }", "", "src/main.wi"),
        (
            "import-type",
            "import helper; fn main() { helper::value(); }",
            "pub fn value() { let n: i64 = true; }",
            "src/helper.wi",
        ),
        (
            "import-parse",
            "import helper; fn main() {}",
            "pub fn value() { let = ; }",
            "src/helper.wi",
        ),
        (
            "discovered-type",
            "fn main() {}",
            "pub fn value() { let n: i64 = true; }",
            "src/helper.wi",
        ),
    ] {
        fs::write(root.join("src/main.wi"), entry).unwrap();
        fs::write(root.join("src/helper.wi"), module).unwrap();
        for nested in [false, true] {
            for format in ["human", "ndjson"] {
                let cwd = if nested {
                    root.join("src/nested")
                } else {
                    root.clone()
                };
                let output = Command::new(env!("CARGO_BIN_EXE_willow"))
                    .current_dir(cwd)
                    .args(["check", ".", "--format", format])
                    .output()
                    .unwrap();
                assert!(!output.status.success(), "{name} {format}");
                let text = String::from_utf8(if format == "human" {
                    output.stderr
                } else {
                    output.stdout
                })
                .unwrap();
                let expected: std::path::PathBuf = expected.split('/').collect();
                if format == "human" {
                    assert!(
                        text.contains(&format!("--> {}:", expected.display())),
                        "{name}: {text}"
                    );
                    assert!(!text.contains(root.to_str().unwrap()), "{text}");
                } else {
                    let labels: Vec<serde_json::Value> = text
                        .lines()
                        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                        .filter(|event| event["event"] == "diagnostic")
                        .flat_map(|event| event["data"]["labels"].as_array().unwrap().clone())
                        .collect();
                    assert!(
                        labels
                            .iter()
                            .any(|label| Path::new(label["path"].as_str().unwrap()) == expected),
                        "{name}: {text}"
                    );
                    assert!(
                        labels
                            .iter()
                            .all(|label| !Path::new(label["path"].as_str().unwrap()).is_absolute()),
                        "{text}"
                    );
                }
            }
        }
    }
    // Build uses the same display boundary, including absolute project arguments.
    fs::write(root.join("src/main.wi"), "fn main() { let n: i64 = true; }").unwrap();
    for format in ["human", "ndjson"] {
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .arg("build")
            .arg(&root)
            .args(["--format", format])
            .output()
            .unwrap();
        assert!(!output.status.success());
        if format == "human" {
            let text = String::from_utf8(output.stderr).unwrap();
            assert!(
                text.contains(&format!(
                    "--> {}:",
                    Path::new("src").join("main.wi").display()
                )),
                "{text}"
            );
        } else {
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(
                text.lines()
                    .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                    .any(|event| {
                        event["event"] == "diagnostic"
                            && event["data"]["labels"]
                                .as_array()
                                .unwrap()
                                .iter()
                                .any(|label| {
                                    label["path"].as_str().is_some_and(|path| {
                                        Path::new(path) == Path::new("src").join("main.wi")
                                    })
                                })
                    }),
                "{text}"
            );
        }
    }
    // Single-file mode preserves the caller's path, even inside a project.
    for absolute in [false, true] {
        let source = if absolute {
            root.join("src/main.wi")
        } else {
            "src/main.wi".into()
        };
        fs::write(root.join("src/main.wi"), "fn main() { let n: i64 = true; }").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_willow"))
            .current_dir(&root)
            .arg("check")
            .arg(&source)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains(&format!("--> {}:", source.display()))
        );
    }
    fs::remove_dir_all(root).unwrap();
}
