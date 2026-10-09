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
        if self.temp.join("cargo-home").exists() {
            command.env("WILLOW_CARGO_HOME", self.temp.join("cargo-home"));
        }
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

fn empty_bridge(f: &Fixture) {
    fs::write(f.root.join("project.toml"), "# keep header\n[project]\nname = \"mutation\" # keep name\nversion = \"0.1.0\"\n[rust]\nbridge = \"bridge.rs\" # keep bridge\n").unwrap();
    fs::write(f.root.join("bridge.rs"), "pub fn answer() -> i32 { 42 }\n").unwrap();
}
fn native_command(f: &Fixture, args: &[&str]) -> Command {
    let original_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap();
    let home = f.temp.join("home");
    fs::create_dir_all(&home).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_willow"));
    command
        .current_dir(&f.root)
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env(
            "RUSTUP_HOME",
            std::env::var_os("RUSTUP_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&original_home).join(".rustup")),
        )
        .env(
            "CARGO_HOME",
            std::env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(&original_home).join(".cargo")),
        );
    if f.temp.join("cargo-home").exists() {
        command.env("WILLOW_CARGO_HOME", f.temp.join("cargo-home"));
    }
    command
}
fn build_mutation(f: &Fixture) {
    fs::create_dir_all(f.root.join("src")).unwrap();
    fs::write(f.root.join("src/main.wi"), "fn main() { println(42); }\n").unwrap();
    let mut command = native_command(f, &["build", "--offline", "-o"]);
    command.arg(f.temp.join("app"));
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = Command::new(f.temp.join(if cfg!(windows) { "app.exe" } else { "app" }))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"42\n");
}
fn assert_removed(f: &Fixture, name: &str, added: &Value) {
    let mut willow_lock: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("project.lock")).unwrap()).unwrap();
    willow_lock.as_table_mut().unwrap().remove("rust");
    let removed = success(&f.run(&["rust", "remove", name, "--offline", "--format=json"]));
    assert_eq!(removed["direct_dependencies"], serde_json::json!([]));
    assert_eq!(
        removed["diagnostics"][0]["kind"],
        "rust_bridge_unused_declaration_candidate"
    );
    assert_eq!(removed["diagnostics"][0]["dependency"], name);
    let text = fs::read_to_string(f.root.join("project.toml")).unwrap();
    for comment in ["# keep header", "# keep name", "# keep bridge"] {
        assert!(text.contains(comment));
    }
    let manifest: toml::Value = toml::from_str(&text).unwrap();
    assert!(manifest["rust-dependencies"].as_table().unwrap().is_empty());
    let generated = PathBuf::from(added["bridge"]["manifest"].as_str().unwrap());
    let generated: toml::Value = toml::from_str(&fs::read_to_string(generated).unwrap()).unwrap();
    assert!(generated["dependencies"].as_table().unwrap().is_empty());
    let cargo: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join(".willow/rust/Cargo.lock")).unwrap())
            .unwrap();
    assert_eq!(cargo["package"].as_array().unwrap().len(), 1);
    let lock: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("project.lock")).unwrap()).unwrap();
    let paths = ["project.lock", ".willow/rust/Cargo.lock"];
    let saved: Vec<_> = paths
        .iter()
        .map(|p| fs::read(f.root.join(p)).unwrap())
        .collect();
    // Final removal must immediately support reproducible native builds/runs;
    // no intervening unlocked command may repair the project lock.
    let output = native_command(f, &["build", "--locked", "--offline", "-o"])
        .arg(f.temp.join("after-removal"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = native_command(f, &["run", "--frozen"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"42\n");
    for (path, bytes) in paths.iter().zip(saved) {
        assert_eq!(fs::read(f.root.join(path)).unwrap(), bytes);
    }
    assert!(lock.get("rust").is_none());
    assert_eq!(lock, willow_lock);
}
#[test]
fn path_add_build_tree_remove_preserves_comments_and_locks() {
    let f = Fixture::new("path");
    empty_bridge(&f);
    let added = success(&f.run(&[
        "rust",
        "add",
        "willow_bridge_native",
        "--path",
        "native",
        "--offline",
        "--format=ndjson",
    ]));
    assert_eq!(added["direct_dependencies"][0]["version"], "1.2.3");
    build_mutation(&f);
    let tree = success(&f.run(&["rust", "tree", "--frozen", "--format=json"]));
    assert_eq!(tree["tree"]["packages"].as_array().unwrap().len(), 2);
    assert_eq!(tree["tree"]["nodes"].as_array().unwrap().len(), 2);
    let human = f.run(&["rust", "tree", "--frozen"]);
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).unwrap();
    assert!(text.contains("willow_bridge_native 1.2.3") && text.contains(" -> "));
    assert_removed(&f, "willow_bridge_native", &added);
}
#[test]
fn git_rev_and_tag_add_build_remove() {
    for selector in ["--rev", "--tag"] {
        let f = Fixture::new("path");
        empty_bridge(&f);
        fs::create_dir_all(f.temp.join("cargo-home")).unwrap();
        let native = f.root.join("native");
        for args in [
            vec!["init", "--quiet"],
            vec!["add", "Cargo.toml", "src/lib.rs"],
            vec![
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
            vec!["tag", "v1"],
        ] {
            assert!(
                Command::new("git")
                    .current_dir(&native)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        }
        let revision = Command::new("git")
            .current_dir(&native)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        let rev = String::from_utf8(revision.stdout).unwrap();
        let url = format!("file://{}", native.to_string_lossy().replace('\\', "/"));
        let added = success(&f.run(&[
            "rust",
            "add",
            "willow_bridge_native",
            "--git",
            &url,
            selector,
            if selector == "--rev" {
                rev.trim()
            } else {
                "v1"
            },
            "--format=json",
        ]));
        assert!(
            added["direct_dependencies"][0]["source"]
                .as_str()
                .unwrap()
                .starts_with("git+")
        );
        build_mutation(&f);
        assert_removed(&f, "willow_bridge_native", &added);
    }
}
fn registry_version(f: &Fixture, version: &str) {
    registry_package(f, "fixture_dep", version);
}
fn registry_package(f: &Fixture, name: &str, version: &str) {
    let directory = f.temp.join("vendor").join(format!("{name}-{version}"));
    fs::create_dir_all(directory.join("src")).unwrap();
    fs::write(directory.join("Cargo.toml"), format!("[package]\nname = \"{name}\"\nversion = \"{version}\"\nedition = \"2021\"\n[features]\nextra = []\n")).unwrap();
    fs::write(
        directory.join("src/lib.rs"),
        "pub fn value() -> u32 { 42 }\n",
    )
    .unwrap();
    fs::write(
        directory.join(".cargo-checksum.json"),
        "{\"files\":{},\"package\":null}",
    )
    .unwrap();
}
fn registry(f: &Fixture) {
    fs::create_dir_all(f.root.join(".cargo")).unwrap();
    let vendor = f.temp.join("vendor").to_str().unwrap().replace('\\', "/");
    fs::write(f.root.join(".cargo/config.toml"), format!("[source.crates-io]\nreplace-with = \"fixture\"\n[source.fixture]\ndirectory = {vendor:?}\n")).unwrap();
    registry_version(f, "1.0.0");
}
#[test]
fn registry_add_build_update_breaking_remove() {
    let f = Fixture::new("plain");
    empty_bridge(&f);
    registry(&f);
    let added = success(&f.run(&["rust", "add", "fixture_dep@1", "--offline", "--format=json"]));
    assert_eq!(added["direct_dependencies"][0]["version"], "1.0.0");
    build_mutation(&f);
    registry_version(&f, "1.2.0");
    registry_version(&f, "2.0.0");
    let path = f.root.join("project.toml");
    let text = fs::read_to_string(&path).unwrap().replace(
        "version = \"1\"",
        "version = \"1\", features = [\"extra\"], default-features = false",
    );
    fs::write(&path, &text).unwrap();
    let compatible = success(&f.run(&[
        "rust",
        "update",
        "fixture_dep",
        "--offline",
        "--format=json",
    ]));
    assert_eq!(compatible["direct_dependencies"][0]["version"], "1.2.0");
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
    let breaking = success(&f.run(&[
        "rust",
        "update",
        "--breaking",
        "--offline",
        "--format=ndjson",
    ]));
    assert_eq!(breaking["direct_dependencies"][0]["version"], "2.0.0");
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("features = [\"extra\"]") && text.contains("default-features = false"));
    let manifest: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(
        manifest["rust-dependencies"]["fixture_dep"]["version"].as_str(),
        Some("2.0.0")
    );
    success(&f.run(&["rust", "check", "--frozen", "--format=json"]));
    assert_removed(&f, "fixture_dep", &added);
}
#[test]
fn malformed_edits_and_failed_resolution_do_not_change_user_files() {
    let f = Fixture::new("path");
    success(&f.run(&["rust", "metadata", "--offline", "--format=json"]));
    let paths = ["project.toml", "project.lock", ".willow/rust/Cargo.lock"];
    let before: Vec<_> = paths
        .iter()
        .map(|p| fs::read(f.root.join(p)).unwrap())
        .collect();
    for args in [
        vec!["add"],
        vec!["remove"],
        vec!["remove", "absent"],
        vec!["add", "bad@1", "--version", "2"],
        vec!["add", "bad", "--path", "absent"],
        vec!["add", "bad", "--version", "1", "--path", "native"],
        vec![
            "add",
            "bad",
            "--git",
            "file:///absent",
            "--rev",
            "a",
            "--tag",
            "b",
        ],
        vec!["add", "bad", "--rev", "a", "--version", "1"],
        vec!["add", "willow_bridge_native", "--path", "native"],
        vec!["add", "willow_missing_test_dependency_98123@=99.0.0"],
        vec!["update", "absent"],
        vec!["update", "--locked"],
        vec!["remove", "willow_bridge_native", "--frozen"],
        vec!["remove", "willow_bridge_native", "--breaking"],
    ] {
        let mut command = vec!["rust"];
        command.extend(args);
        command.extend(["--offline", "--format=json"]);
        let output = f.run(&command);
        assert!(!output.status.success(), "{command:?}");
        assert_eq!(json(&output)["ok"], false);
        for (path, bytes) in paths.iter().zip(&before) {
            assert_eq!(
                fs::read(f.root.join(path)).unwrap(),
                *bytes,
                "{command:?}: {path}"
            );
        }
    }
    // Failed resolution may leave disposable cache files; frozen inspection
    // regenerates them from the unchanged manifest and authoritative locks.
    success(&f.run(&["rust", "metadata", "--frozen", "--format=json"]));
}
#[test]
fn trust_notice_is_explicit_on_every_doctor_and_metadata_inspection() {
    for name in ["plain", "path"] {
        let f = Fixture::new(name);
        for operation in ["doctor", "metadata"] {
            for format in ["json", "ndjson", "human"] {
                let mut args = if operation == "doctor" {
                    vec!["doctor"]
                } else {
                    vec!["rust", "metadata"]
                };
                args.extend(["--offline", "--format", format]);
                for _ in 0..2 {
                    let output = f.run(&args);
                    if format == "human" {
                        let text = String::from_utf8(output.stdout).unwrap();
                        assert_eq!(
                            text.contains("build.rs scripts and procedural macros"),
                            name == "path"
                        );
                    } else {
                        assert!(output.stderr.is_empty());
                        let value = json(&output);
                        assert_eq!(
                            value["rust_dependency_build_execution_notice"]["build_time_code_execution_possible"]
                                == true,
                            name == "path"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn named_breaking_update_and_partial_remove_preserve_other_dependencies() {
    let f = Fixture::new("plain");
    empty_bridge(&f);
    registry(&f);
    registry_package(&f, "other_dep", "1.0.0");
    for name in ["fixture_dep", "other_dep"] {
        success(&f.run(&[
            "rust",
            "add",
            name,
            "--version=1",
            "--offline",
            "--format=json",
        ]));
    }
    registry_version(&f, "2.0.0");
    registry_package(&f, "other_dep", "1.1.0");
    registry_package(&f, "other_dep", "2.0.0");
    success(&f.run(&[
        "rust",
        "update",
        "fixture_dep",
        "--breaking",
        "--offline",
        "--format=json",
    ]));
    let manifest: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join("project.toml")).unwrap()).unwrap();
    assert_eq!(
        manifest["rust-dependencies"]["fixture_dep"]["version"].as_str(),
        Some("2.0.0")
    );
    assert_eq!(
        manifest["rust-dependencies"]["other_dep"]["version"].as_str(),
        Some("1")
    );
    let output = f.run(&["rust", "remove", "fixture_dep", "--offline"]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("Review bridge declarations")
    );
    let tree = success(&f.run(&["rust", "tree", "--frozen", "--format=ndjson"]));
    assert_eq!(tree["direct_dependencies"].as_array().unwrap().len(), 1);
    assert_eq!(tree["direct_dependencies"][0]["alias"], "other_dep");
    assert_eq!(tree["direct_dependencies"][0]["version"], "1.0.0");
}

fn registry_dependency(
    f: &Fixture,
    name: &str,
    version: &str,
    dependency: &str,
    requirement: &str,
) {
    let path = f
        .temp
        .join("vendor")
        .join(format!("{name}-{version}"))
        .join("Cargo.toml");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        path,
        format!("{text}\n[dependencies]\n{dependency} = {requirement:?}\n"),
    )
    .unwrap();
}
fn locked_versions(f: &Fixture, name: &str) -> Vec<String> {
    let lock: toml::Value =
        toml::from_str(&fs::read_to_string(f.root.join(".willow/rust/Cargo.lock")).unwrap())
            .unwrap();
    lock["package"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|p| p["name"].as_str() == Some(name))
        .map(|p| p["version"].as_str().unwrap().to_owned())
        .collect()
}
fn assert_frozen_locks(f: &Fixture) {
    let paths = ["project.lock", ".willow/rust/Cargo.lock"];
    let before: Vec<_> = paths
        .iter()
        .map(|p| fs::read(f.root.join(p)).unwrap())
        .collect();
    success(&f.run(&["rust", "metadata", "--frozen", "--format=json"]));
    for (path, bytes) in paths.iter().zip(before) {
        assert_eq!(fs::read(f.root.join(path)).unwrap(), bytes);
    }
}
#[test]
fn named_updates_select_direct_package_with_multiple_versions() {
    for (transitive, requirement, latest) in [("0.5.0", "0.5", "2.0.0"), ("2.0.0", "2", "3.0.0")] {
        for breaking in [false, true] {
            let f = Fixture::new("plain");
            empty_bridge(&f);
            registry(&f);
            registry_version(&f, transitive);
            registry_package(&f, "holder", "1.0.0");
            registry_dependency(&f, "holder", "1.0.0", "fixture_dep", requirement);
            for name in ["fixture_dep@1", "holder@1"] {
                success(&f.run(&["rust", "add", name, "--offline", "--format=json"]));
            }
            let mut initial = vec![transitive.to_owned(), "1.0.0".to_owned()];
            initial.sort();
            assert_eq!(locked_versions(&f, "fixture_dep"), initial);
            registry_version(&f, "1.1.0");
            registry_version(&f, latest);
            let mut args = vec![
                "rust",
                "update",
                "fixture_dep",
                "--offline",
                "--format=json",
            ];
            if breaking {
                args.push("--breaking");
            }
            let result = success(&f.run(&args));
            let direct = result["direct_dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["alias"] == "fixture_dep")
                .unwrap();
            let expected = if breaking { latest } else { "1.1.0" };
            assert_eq!(direct["version"], expected);
            let mut versions = vec![transitive.to_owned(), expected.to_owned()];
            versions.sort();
            assert_eq!(locked_versions(&f, "fixture_dep"), versions);
            assert_frozen_locks(&f);
        }
    }
}
#[test]
fn breaking_update_retains_compatible_transitive_update() {
    let f = Fixture::new("plain");
    empty_bridge(&f);
    registry(&f);
    registry_package(&f, "leaf", "1.0.0");
    registry_dependency(&f, "fixture_dep", "1.0.0", "leaf", "1");
    success(&f.run(&["rust", "add", "fixture_dep@1", "--offline", "--format=json"]));
    assert_eq!(locked_versions(&f, "leaf"), ["1.0.0"]);
    registry_package(&f, "leaf", "1.1.0");
    let result = success(&f.run(&["rust", "update", "--breaking", "--offline", "--format=json"]));
    assert_eq!(result["direct_dependencies"][0]["version"], "1.0.0");
    assert_eq!(locked_versions(&f, "leaf"), ["1.1.0"]);
    assert_frozen_locks(&f);
}
#[test]
fn breaking_update_retains_moving_git_revision() {
    let f = Fixture::new("path");
    empty_bridge(&f);
    registry(&f);
    fs::create_dir_all(f.temp.join("cargo-home")).unwrap();
    let native = f.root.join("native");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(&native)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "--quiet"]);
    git(&["add", "Cargo.toml", "src/lib.rs"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "initial",
    ]);
    let old_revision = git(&["rev-parse", "HEAD"]);
    let url = format!("file://{}", native.to_string_lossy().replace('\\', "/"));
    success(&f.run(&[
        "rust",
        "add",
        "willow_bridge_native",
        "--git",
        &url,
        "--format=json",
    ]));
    success(&f.run(&["rust", "add", "fixture_dep@1", "--offline", "--format=json"]));
    fs::write(
        native.join("src/lib.rs"),
        "pub fn changed() -> i32 { 43 }\n",
    )
    .unwrap();
    git(&["add", "src/lib.rs"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "update",
    ]);
    let revision = git(&["rev-parse", "HEAD"]);
    assert_ne!(revision, old_revision);
    let result = success(&f.run(&["rust", "update", "--breaking", "--format=json"]));
    let direct = result["direct_dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["alias"] == "willow_bridge_native")
        .unwrap();
    assert!(
        direct["source"].as_str().unwrap().ends_with(&revision),
        "{direct}"
    );
    let lock = fs::read_to_string(f.root.join(".willow/rust/Cargo.lock")).unwrap();
    assert!(lock.contains(&revision) && !lock.contains(&old_revision));
    assert_frozen_locks(&f);
}
