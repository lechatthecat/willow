use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    temp: PathBuf,
    root: PathBuf,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let temp = std::env::temp_dir().join(format!(
            "willow-rust-cli-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let root = temp.join("project with spaces");
        fs::create_dir_all(&root).unwrap();
        if name == "plain" {
            fs::write(
                root.join("project.toml"),
                "[project]\nname = \"plain\"\nversion = \"0.1.0\"\n",
            )
            .unwrap();
        } else {
            let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/rust_bridge")
                .join(name);
            for file in [
                "project.toml",
                "bridge.rs",
                "native/Cargo.toml",
                "native/src/lib.rs",
            ] {
                if source.join(file).is_file() {
                    fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
                    fs::copy(source.join(file), root.join(file)).unwrap();
                }
            }
        }
        Self { temp, root }
    }
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
        command
            .current_dir(&self.root)
            .args(args)
            .arg("--cache-dir")
            .arg(self.temp.join("cache"));
        command
    }
    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp);
    }
}
fn json(output: &Output) -> Value {
    assert!(!output.stdout.contains(&0x1b), "ANSI in stdout");
    assert_eq!(output.stdout.iter().filter(|&&b| b == b'\n').count(), 1);
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("{e}: {output:?}"))
}
fn success(output: &Output) -> Value {
    assert!(output.status.success(), "{output:?}");
    let value = json(output);
    assert_eq!(value["schema"], 1);
    assert_eq!(value["ok"], true);
    value
}

#[test]
fn hyphenated_manifest_alias_survives_metadata_check_and_lock() {
    for custom_lib_name in [false, true] {
        let f = Fixture::new("path");
        for file in ["project.toml", "native/Cargo.toml"] {
            let path = f.root.join(file);
            let contents = fs::read_to_string(&path)
                .unwrap()
                .replace("willow_bridge_native", "willow-bridge-native");
            fs::write(path, contents).unwrap();
        }
        if custom_lib_name {
            let path = f.root.join("native/Cargo.toml");
            let contents = fs::read_to_string(&path).unwrap();
            fs::write(
                path,
                format!("{contents}\n[lib]\nname = \"different_crate_name\"\n"),
            )
            .unwrap();
            let bridge = f.root.join("bridge.rs");
            let contents = fs::read_to_string(&bridge)
                .unwrap()
                .replace("willow_bridge_native", "different_crate_name");
            fs::write(bridge, contents).unwrap();
        }
        for (operation, mode) in [
            ("metadata", "--offline"),
            ("check", "--frozen"),
            ("metadata", "--frozen"),
        ] {
            let value = success(&f.run(&["rust", operation, mode, "--format=json"]));
            assert_eq!(
                value["direct_dependencies"][0]["alias"],
                "willow-bridge-native"
            );
            assert_eq!(
                value["direct_dependencies"][0]["name"],
                "willow-bridge-native"
            );
            assert_eq!(value["direct_dependencies"][0]["version"], "1.2.3");
            let lock: toml::Value =
                toml::from_str(&fs::read_to_string(f.root.join("project.lock")).unwrap()).unwrap();
            assert_eq!(
                lock["rust"]["dependencies"]["willow-bridge-native"].as_str(),
                Some("1.2.3")
            );
            assert!(
                lock["rust"]["dependencies"]
                    .get("willow_bridge_native")
                    .is_none()
            );
            assert!(
                lock["rust"]["dependencies"]
                    .get("different_crate_name")
                    .is_none()
            );
        }
    }
}

#[test]
fn path_check_metadata_and_frozen_output() {
    let f = Fixture::new("path");
    for format in ["json", "ndjson"] {
        let checked = success(&f.run(&["rust", "check", "--offline", "--format", format]));
        assert_eq!(checked["kind"], "rust.check");
        assert_eq!(checked["enabled"], true);
        assert!(checked["bridge"]["artifact"].is_null());
        let manifest = PathBuf::from(checked["bridge"]["manifest"].as_str().unwrap());
        assert!(manifest.is_file());
        assert!(
            fs::read_to_string(manifest.parent().unwrap().join("src/lib.rs"))
                .unwrap()
                .contains("pub mod bridge")
        );
        let metadata = success(&f.run(&["rust", "metadata", "--frozen", "--format", format]));
        assert_eq!(metadata["kind"], "rust.metadata");
        assert_eq!(
            metadata["direct_dependencies"][0]["alias"],
            "willow_bridge_native"
        );
        assert_eq!(
            metadata["direct_dependencies"][0]["name"],
            "willow_bridge_native"
        );
        assert_eq!(metadata["direct_dependencies"][0]["version"], "1.2.3");
        assert!(
            metadata["direct_dependencies"][0]["source"]["path"]
                .as_str()
                .unwrap()
                .ends_with("Cargo.toml")
        );
        assert!(
            metadata["toolchain"]["cargo"]
                .as_str()
                .unwrap()
                .starts_with("cargo ")
        );
        assert!(
            metadata["toolchain"]["rustc"]
                .as_str()
                .unwrap()
                .contains("host: ")
        );
    }
}

#[test]
fn metadata_resolves_without_compiling_invalid_bridge() {
    let f = Fixture::new("path");
    fs::write(f.root.join("bridge.rs"), "invalid Rust syntax!").unwrap();
    let metadata = success(&f.run(&["rust", "metadata", "--offline", "--format=json"]));
    let manifest = PathBuf::from(metadata["bridge"]["manifest"].as_str().unwrap());
    assert!(!manifest.parent().unwrap().join("target").exists());
}

#[test]
fn check_does_not_invoke_native_linker() {
    let f = Fixture::new("path");
    let linker = format!(
        "CARGO_TARGET_{}_LINKER",
        target_lexicon::Triple::host()
            .to_string()
            .replace('-', "_")
            .to_uppercase()
    );
    let output = f
        .command(&["rust", "check", "--offline", "--format=json"])
        .env(linker, f.temp.join("nonexistent-linker"))
        .output()
        .unwrap();
    success(&output);
}

#[test]
fn locked_resolution_and_missing_bridge_fail_before_cargo_check() {
    let f = Fixture::new("path");
    let output = f.run(&["rust", "check", "--frozen", "--format=json"]);
    assert!(!output.status.success());
    assert!(
        json(&output)["error"]["message"]
            .as_str()
            .unwrap()
            .contains("rust_lockfile_stale")
    );
    fs::remove_file(f.root.join("bridge.rs")).unwrap();
    let output = f.run(&["rust", "check", "--format=json"]);
    assert!(!output.status.success());
    assert_ne!(json(&output)["error"]["kind"], "rust_bridge_compile_error");
}

#[test]
fn successful_check_keeps_warning_diagnostics_inside_json() {
    let f = Fixture::new("path");
    fs::write(
        f.root.join("bridge.rs"),
        "pub fn valid() { let unused = 1; }\n",
    )
    .unwrap();
    let output = f.run(&["rust", "check", "--offline", "--format=json"]);
    let value = success(&output);
    assert!(output.stderr.is_empty());
    assert!(
        value["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["message"]["level"] == "warning")
    );
}

#[test]
fn compile_error_has_lossless_structured_diagnostics_and_source_position() {
    let f = Fixture::new("path");
    fs::write(
        f.root.join("bridge.rs"),
        "pub fn bad() { missing_function(); }\n",
    )
    .unwrap();
    for format in ["json", "ndjson"] {
        let output = f.run(&["rust", "check", "--offline", "--format", format]);
        assert!(!output.status.success());
        assert!(output.stderr.is_empty(), "{output:?}");
        let error = json(&output);
        assert_eq!(error["kind"], "rust.error");
        assert_eq!(error["error"]["kind"], "rust_bridge_compile_error");
        let diagnostic = error["error"]["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["message"]["code"]["code"] == "E0425")
            .unwrap();
        assert_eq!(diagnostic["reason"], "compiler-message");
        assert!(
            diagnostic["message"]["rendered"]
                .as_str()
                .unwrap()
                .contains("missing_function")
        );
        let span = &diagnostic["message"]["spans"][0];
        assert!(span["file_name"].as_str().unwrap().ends_with("bridge.rs"));
        assert_eq!(span["line_start"], 1);
        assert!(span["column_start"].as_u64().unwrap() > 0);
        assert!(error["error"]["cargo_stderr"].is_string());
    }
    let human = f.run(&["rust", "check", "--offline"]);
    assert!(!human.status.success());
    let stderr = String::from_utf8(human.stderr).unwrap();
    assert!(stderr.contains("bridge.rs:1:") && stderr.contains("E0425"));
}

#[test]
fn plain_project_never_requires_or_executes_rust_for_check_metadata() {
    let f = Fixture::new("plain");
    for operation in ["check", "metadata"] {
        let output = f
            .command(&["rust", operation, "--format=json"])
            .env("PATH", "")
            .output()
            .unwrap();
        let value = success(&output);
        assert_eq!(value["enabled"], false);
        assert_eq!(value["direct_dependencies"], serde_json::json!([]));
        assert!(value["toolchain"]["cargo"].is_null());
        assert!(!f.temp.join("cache").exists());
        assert!(!f.root.join("project.lock").exists());
    }
}

#[test]
fn doctor_reports_separate_capabilities_and_optional_rust() {
    for name in ["plain", "path"] {
        let f = Fixture::new(name);
        let value = json(&f.run(&["doctor", "--format=json"]));
        assert_eq!(value["kind"], "doctor");
        assert_eq!(value["rust_interop"]["enabled"], name != "plain");
        for tool in ["cargo", "rustc"] {
            assert_eq!(value[tool]["required"], name != "plain");
            assert_eq!(value[tool]["available"], true);
        }
        for tool in ["runtime", "native_linker"] {
            assert_eq!(value[tool]["required"], true);
            assert!(value[tool]["available"].is_boolean());
        }
        assert!(!f.temp.join("cache").exists());
        assert!(!f.root.join("project.lock").exists());
        let human = f.run(&["doctor"]);
        let text = String::from_utf8(human.stdout).unwrap();
        assert!(text.contains("Rust interop enabled:") && text.contains("native_linker:"));
        assert!(text.contains(if name == "plain" {
            "cargo: available (optional)"
        } else {
            "cargo: available (required)"
        }));
    }
}

#[test]
fn doctor_reports_missing_tools_independently() {
    let f = Fixture::new("path");
    let output = f
        .command(&["doctor", "--format=ndjson"])
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let value = json(&output);
    for tool in ["cargo", "rustc"] {
        assert_eq!(value[tool]["available"], false);
        assert_eq!(value[tool]["required"], true);
    }
}

#[test]
fn malformed_command_and_manifest_stay_json() {
    let f = Fixture::new("plain");
    for args in [
        vec!["rust"],
        vec!["rust", "unknown"],
        vec!["rust", "check", "--unknown"],
        vec!["rust", "check", "--format=json"],
        vec!["rust", "check", ".", "."],
        vec!["rust", "metadata", "--offline=yes"],
        vec!["doctor", "--project-dir"],
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
        let output = command
            .current_dir(&f.root)
            .args(&args)
            .arg("--format=json")
            .output()
            .unwrap();
        assert!(!output.status.success(), "{args:?}");
        assert_eq!(json(&output)["ok"], false);
    }
    fs::write(f.root.join("project.toml"), "invalid = [").unwrap();
    let output = f.run(&["rust", "check", "--format=json"]);
    assert!(!output.status.success());
    assert_eq!(json(&output)["error"]["kind"], "manifest_invalid");
    assert!(!f.temp.join("cache").exists());
}

#[test]
fn discovers_parent_project_and_human_metadata() {
    let f = Fixture::new("path");
    fs::create_dir(f.root.join("child")).unwrap();
    let output = f
        .command(&["rust", "metadata", "--offline", "--format=json"])
        .current_dir(f.root.join("child"))
        .output()
        .unwrap();
    success(&output);
    let human = f.run(&["rust", "metadata", "--frozen"]);
    assert!(human.status.success());
    assert!(
        String::from_utf8(human.stdout)
            .unwrap()
            .contains("willow_bridge_native 1.2.3")
    );
}

#[test]
#[ignore = "requires the crates.io regex cache; run explicitly for acceptance"]
fn regex_check_and_resolved_metadata() {
    let f = Fixture::new("regex");
    success(&f.run(&["rust", "check", "--offline", "--format=json"]));
    let metadata = success(&f.run(&["rust", "metadata", "--frozen", "--format=json"]));
    let regex = metadata["direct_dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["alias"] == "regex")
        .unwrap();
    assert_eq!(regex["name"], "regex");
    assert!(regex["version"].as_str().unwrap().starts_with("1."));
    assert!(regex["source"].as_str().unwrap().starts_with("registry+"));
}
