use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use willow_compiler::BuildMode;
use willow_compiler::project::ProjectManifest;
use willow_compiler::toolchain::rust_bridge::*;

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
struct Fixture {
    temp: PathBuf,
    root: PathBuf,
    options: BridgeOptions,
}
impl Fixture {
    fn new(name: &str) -> Self {
        let temp = std::env::temp_dir().join(format!(
            "willow-rust-bridge-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let root = temp.join("project with spaces");
        fs::create_dir_all(&root).unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/rust_bridge")
            .join(name);
        for file in [
            "project.toml",
            "bridge.rs",
            "native/Cargo.toml",
            "native/src/lib.rs",
        ] {
            if source.join(file).exists() {
                fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
                fs::copy(source.join(file), root.join(file)).unwrap();
            }
        }
        let mut options = BridgeOptions::new(BuildMode::Debug).unwrap();
        options.cache_root = temp.join("cache");
        options.offline = true;
        Self {
            temp,
            root,
            options,
        }
    }
    fn manifest(&self) -> ProjectManifest {
        ProjectManifest::load(&self.root.join("project.toml")).unwrap()
    }
    fn build(&self, check: bool) -> BridgeBuild {
        build_bridge(&self.manifest(), &self.root, &self.options, check)
            .unwrap()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.temp);
    }
}
fn fresh(build: &BridgeBuild) -> bool {
    build
        .messages
        .artifacts
        .iter()
        .find(|a| a.target["name"] == "willow_generated_bridge")
        .unwrap()
        .fresh
}

#[test]
fn path_fixture_check_build_versions_and_warm_cache() {
    let fixture = Fixture::new("path");
    let checked = fixture.build(true);
    assert!(checked.staticlib.is_none());
    drop(checked);
    let built = fixture.build(false);
    assert!(!fresh(&built));
    assert!(built.staticlib.as_ref().unwrap().is_file());
    assert!(
        built.metadata["packages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "willow_bridge_native" && p["version"] == "1.2.3")
    );
    assert!(
        !built
            .messages
            .native_static_libs
            .as_ref()
            .unwrap()
            .is_empty()
    );
    assert!(!built.directory.starts_with(&fixture.root));
    let key = built.cache_key.clone();
    let generated = fs::read_to_string(built.directory.join("Cargo.toml")).unwrap();
    assert!(generated.contains("2024") && generated.contains("staticlib"));
    built.ensure_target(&built.target).unwrap();
    assert!(
        built
            .ensure_target("different-target")
            .unwrap_err()
            .to_string()
            .contains("rust_target_mismatch")
    );
    drop(built);
    let warm = fixture.build(false);
    assert!(fresh(&warm));
    assert_eq!(key, warm.cache_key);
    assert!(!fixture.root.join("Cargo.lock").exists());
    assert!(!fixture.root.join("target").exists());
}

#[test]
fn every_cache_dimension_changes_identity() {
    let base = BridgeCacheKey {
        target: "target",
        profile: "dev",
        rustc_version: "rustc",
        cargo_version: "cargo",
        lock_hash: "lock",
        bridge_source_hash: "source",
        wrapper_schema: "schema",
        abi_revision: "abi",
        manifest_hash: "manifest",
    };
    let original = base.digest();
    for field in 0..9 {
        let mut changed = base.clone();
        *match field {
            0 => &mut changed.target,
            1 => &mut changed.profile,
            2 => &mut changed.rustc_version,
            3 => &mut changed.cargo_version,
            4 => &mut changed.lock_hash,
            5 => &mut changed.bridge_source_hash,
            6 => &mut changed.wrapper_schema,
            7 => &mut changed.abi_revision,
            _ => &mut changed.manifest_hash,
        } = "changed";
        assert_ne!(original, changed.digest(), "dimension {field}");
    }
    assert_eq!(original, base.digest());
}

#[test]
fn source_schema_abi_manifest_lock_and_profile_invalidate_compilation() {
    let mut fixture = Fixture::new("path");
    let first = fixture.build(false);
    let mut key = first.cache_key.clone();
    drop(first);
    for change in 0..6 {
        match change {
            0 => {
                fs::write(
                    fixture.root.join("bridge.rs"),
                    "pub fn changed() -> i32 { willow_bridge_native::answer() }\n",
                )
                .unwrap();
            }
            1 => fixture.options.wrapper_schema = "2".into(),
            2 => fixture.options.abi_revision = "2".into(),
            3 => {
                let manifest = fixture.root.join("project.toml");
                let text = fs::read_to_string(&manifest)
                    .unwrap()
                    .replace("default-features = false", "default-features = true");
                fs::write(manifest, text).unwrap();
            }
            4 => {
                let path = fixture.root.join("native/Cargo.toml");
                let text = fs::read_to_string(&path).unwrap().replace("1.2.3", "1.2.4");
                fs::write(path, text).unwrap();
            }
            _ => fixture.options.mode = BuildMode::Release,
        }
        let built = fixture.build(false);
        assert_ne!(key, built.cache_key, "change {change}");
        assert!(!fresh(&built), "change {change}");
        key = built.cache_key.clone();
        if change == 5 {
            assert_eq!(
                built.messages.artifacts.last().unwrap().profile["opt_level"],
                "3"
            );
        }
        drop(built);
        assert!(fresh(&fixture.build(false)), "warm change {change}");
    }
}

#[test]
fn cargo_tracks_transitive_path_sources_even_with_same_willow_key() {
    let fixture = Fixture::new("path");
    let first = fixture.build(false);
    let key = first.cache_key.clone();
    drop(first);
    fs::write(
        fixture.root.join("native/src/lib.rs"),
        "pub fn answer() -> i32 { 43 }\n",
    )
    .unwrap();
    let changed = fixture.build(false);
    assert_eq!(key, changed.cache_key);
    assert!(!fresh(&changed));
}

#[test]
fn plain_project_does_not_require_rust_tools() {
    let mut fixture = Fixture::new("path");
    let mut manifest = fixture.manifest();
    manifest.rust_dependencies.clear();
    fixture.options.cargo = fixture.temp.join("no-cargo");
    fixture.options.rustc = fixture.temp.join("no-rustc");
    assert!(
        build_bridge(&manifest, &fixture.root, &fixture.options, false)
            .unwrap()
            .is_none()
    );
}

#[test]
fn absent_cargo_is_cargo_missing() {
    let mut fixture = Fixture::new("path");
    fixture.options.cargo = fixture.temp.join("no-cargo");
    assert!(
        build_bridge(&fixture.manifest(), &fixture.root, &fixture.options, false)
            .unwrap_err()
            .to_string()
            .contains("cargo_missing")
    );
}

#[test]
fn absent_rustc_is_rust_toolchain_missing() {
    let mut fixture = Fixture::new("path");
    fixture.options.rustc = fixture.temp.join("no-rustc");
    assert!(
        build_bridge(&fixture.manifest(), &fixture.root, &fixture.options, false)
            .unwrap_err()
            .to_string()
            .contains("rust_toolchain_missing")
    );
}

#[test]
fn invalid_target_is_rejected() {
    let mut fixture = Fixture::new("path");
    fixture.options.target = Some("invalid-target".into());
    assert!(
        build_bridge(&fixture.manifest(), &fixture.root, &fixture.options, false)
            .unwrap_err()
            .to_string()
            .contains("rust_target_mismatch")
    );
}

#[test]
fn compile_failure_retains_structured_diagnostic() {
    let fixture = Fixture::new("path");
    fs::write(
        fixture.root.join("bridge.rs"),
        "pub fn bad() { missing_function(); }\n",
    )
    .unwrap();
    let error = build_bridge(&fixture.manifest(), &fixture.root, &fixture.options, false)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("rust_bridge_build_failed") && error.contains("E0425"),
        "{error}"
    );
}

#[test]
fn malformed_json_is_rejected_and_unknown_reasons_are_ignored() {
    assert!(parse_cargo_messages(b"human text\n", Path::new("Cargo.toml")).is_err());
    let messages = parse_cargo_messages(
        b"{\"reason\":\"future\"}\n{\"reason\":\"build-finished\",\"success\":true}\n",
        Path::new("Cargo.toml"),
    )
    .unwrap();
    assert!(messages.success);
}

#[test]
fn native_metadata_keeps_order_and_duplicates_on_four_targets() {
    for libs in [
        "-lgcc_s -lutil -lrt -lpthread -lm -ldl -lc",
        "-lSystem -liconv -lSystem",
        "-lSystem -liconv",
        "kernel32.lib ws2_32.lib kernel32.lib",
    ] {
        let line = serde_json::json!({"reason":"compiler-message", "manifest_path":"Cargo.toml", "target":{"name":"willow_generated_bridge"}, "message":{"message":format!("native-static-libs: {libs}")}});
        let messages =
            parse_cargo_messages(&serde_json::to_vec(&line).unwrap(), Path::new("Cargo.toml"))
                .unwrap();
        assert_eq!(messages.native_static_libs.as_deref(), Some(libs));
        assert_eq!(messages.diagnostics.len(), 1);
        let wrong = parse_cargo_messages(
            &serde_json::to_vec(&line).unwrap(),
            Path::new("other/Cargo.toml"),
        )
        .unwrap();
        assert!(wrong.native_static_libs.is_none());
    }
}

#[test]
#[ignore = "requires crates.io regex cache; run explicitly with Cargo network/cache available"]
fn regex_registry_fixture_check_build_and_resolved_version() {
    let mut fixture = Fixture::new("regex");
    fixture.options.offline = std::env::var_os("WILLOW_TEST_ONLINE").is_none();
    drop(fixture.build(true));
    let built = fixture.build(false);
    assert!(built.staticlib.unwrap().is_file());
    let packages = built.metadata["packages"].as_array().unwrap();
    let regex = packages.iter().find(|p| p["name"] == "regex").unwrap();
    assert!(regex["version"].as_str().unwrap().starts_with("1."));
}

#[test]
fn missing_bridge_configuration_is_reported() {
    let fixture = Fixture::new("path");
    let mut manifest = fixture.manifest();
    manifest.rust = None;
    let error = build_bridge(&manifest, &fixture.root, &fixture.options, false).unwrap_err();
    assert!(error.to_string().contains("rust_bridge_missing"));
}

#[test]
fn cache_in_source_tree_is_rejected_before_creation() {
    let mut fixture = Fixture::new("path");
    fixture.options.cache_root = fixture.root.join("unwanted-cache");
    let error =
        build_bridge(&fixture.manifest(), &fixture.root, &fixture.options, false).unwrap_err();
    assert!(error.to_string().contains("outside the project"));
    assert!(!fixture.options.cache_root.exists());
}

#[test]
fn missing_archive_rebuilds_instead_of_reusing_a_stale_path() {
    let fixture = Fixture::new("path");
    let first = fixture.build(false);
    let archive = first.staticlib.clone().unwrap();
    drop(first);
    fs::remove_file(&archive).unwrap();
    let rebuilt = fixture.build(false);
    assert!(rebuilt.staticlib.as_ref().unwrap().is_file());
}

#[test]
fn cargo_tracks_bridge_child_modules_and_reuses_checks() {
    let fixture = Fixture::new("path");
    fs::write(
        fixture.root.join("bridge.rs"),
        "mod helper; pub fn answer() -> i32 { helper::answer() }\n",
    )
    .unwrap();
    fs::write(
        fixture.root.join("helper.rs"),
        "pub fn answer() -> i32 { 42 }\n",
    )
    .unwrap();
    drop(fixture.build(true));
    assert!(fresh(&fixture.build(true)));
    let initial = fixture.build(false);
    let key = initial.cache_key.clone();
    drop(initial);
    fs::write(
        fixture.root.join("helper.rs"),
        "pub fn answer() -> i32 { 43 }\n",
    )
    .unwrap();
    let changed = fixture.build(false);
    assert_eq!(key, changed.cache_key);
    assert!(!fresh(&changed));
}

#[test]
fn json_artifact_counts_scale_with_messages_without_duplication() {
    for count in [16, 64, 256, 1024] {
        let artifact = serde_json::json!({"reason":"compiler-artifact", "package_id":"fixture@1.0.0", "target":{"name":"fixture"}, "profile":{"opt_level":"0"}, "filenames":["fixture.a"], "fresh":true});
        let mut bytes = serde_json::to_vec(&artifact).unwrap();
        bytes.push(b'\n');
        let input = bytes.repeat(count);
        let parsed = parse_cargo_messages(&input, Path::new("Cargo.toml")).unwrap();
        assert_eq!(parsed.artifacts.len(), count);
        assert_eq!(
            parsed
                .artifacts
                .iter()
                .map(|a| a.filenames.len())
                .sum::<usize>(),
            count
        );
        assert!(!parsed.success); // Missing build-finished must not look successful.
        eprintln!(
            "cargo_json bytes={} records={} artifacts={}",
            input.len(),
            count,
            parsed.artifacts.len()
        );
    }
}

#[cfg(unix)]
#[test]
fn reported_rustc_and_cargo_versions_invalidate_compilation() {
    use std::os::unix::fs::PermissionsExt;
    let mut fixture = Fixture::new("path");
    let first = fixture.build(false);
    let mut key = first.cache_key.clone();
    drop(first);
    for tool in ["cargo", "rustc"] {
        let wrapper = fixture.temp.join(format!("{tool}-wrapper"));
        let script = if tool == "cargo" {
            "#!/bin/sh\nif [ \"$1\" = --version ]; then printf 'cargo cache-test-version\\n'; else exec cargo \"$@\"; fi\n"
        } else {
            "#!/bin/sh\nif [ \"$1\" = -vV ]; then rustc -vV; printf '\\nwillow-test-version: 2\\n'; else exec rustc \"$@\"; fi\n"
        };
        fs::write(&wrapper, script).unwrap();
        fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
        if tool == "cargo" {
            fixture.options.cargo = wrapper;
        } else {
            fixture.options.rustc = wrapper;
        }
        let built = fixture.build(false);
        assert_ne!(key, built.cache_key);
        assert!(!fresh(&built));
        key = built.cache_key.clone();
        drop(built);
        assert!(fresh(&fixture.build(false)));
    }
}

#[test]
#[ignore = "requires four installed Rust standard-library targets; no foreign execution"]
fn four_target_artifacts_and_target_key_invalidation() {
    let mut fixture = Fixture::new("path");
    let mut keys = std::collections::BTreeSet::new();
    for target in [
        "x86_64-unknown-linux-gnu",
        "x86_64-pc-windows-msvc",
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
    ] {
        fixture.options.target = Some(target.into());
        let built = fixture.build(false);
        assert!(keys.insert(built.cache_key.clone()));
        assert!(!fresh(&built));
        built.ensure_target(target).unwrap();
        eprintln!(
            "target={target} native-static-libs={}",
            built.messages.native_static_libs.as_ref().unwrap()
        );
        drop(built);
        assert!(fresh(&fixture.build(false)));
    }
}

#[test]
fn persistent_locks_warm_frozen_and_regeneration() {
    let mut f = Fixture::new("path");
    let first = f.build(true);
    let directory = first.directory.clone();
    drop(first);
    let cargo = f.root.join(".willow/rust/Cargo.lock");
    let project = f.root.join("project.lock");
    let bytes = fs::read(&cargo).unwrap();
    let project_bytes = fs::read(&project).unwrap();
    let lock: toml::Value = toml::from_str(std::str::from_utf8(&project_bytes).unwrap()).unwrap();
    assert_eq!(
        lock["rust"]["dependencies"]["willow_bridge_native"].as_str(),
        Some("1.2.3")
    );
    let modified = fs::metadata(&cargo).unwrap().modified().unwrap();
    let project_modified = fs::metadata(&project).unwrap().modified().unwrap();
    f.options.locked = true; // offline already enabled: frozen
    assert!(fresh(&f.build(true)));
    assert_eq!(bytes, fs::read(&cargo).unwrap());
    assert_eq!(project_bytes, fs::read(&project).unwrap());
    assert_eq!(modified, fs::metadata(&cargo).unwrap().modified().unwrap());
    assert_eq!(
        project_modified,
        fs::metadata(&project).unwrap().modified().unwrap()
    );
    // A discarded build cache is restored from the authoritative project lock.
    fs::remove_file(directory.join("Cargo.lock")).unwrap();
    drop(f.build(true));
    assert_eq!(bytes, fs::read(directory.join("Cargo.lock")).unwrap());
    // Removing the authoritative copy is forbidden under --locked.
    fs::remove_file(&cargo).unwrap();
    let error = build_bridge(&f.manifest(), &f.root, &f.options, true).unwrap_err();
    assert!(error.to_string().contains("rust_lockfile_stale"));
    assert!(!cargo.exists());
    f.options.locked = false;
    drop(f.build(true));
    assert_eq!(bytes, fs::read(&cargo).unwrap());
    assert_eq!(project_bytes, fs::read(&project).unwrap());
    // Network-free path resolution is reproducible even with both copies removed.
    fs::remove_file(&cargo).unwrap();
    fs::remove_file(directory.join("Cargo.lock")).unwrap();
    drop(f.build(true));
    assert_eq!(bytes, fs::read(&cargo).unwrap());
}

#[test]
fn locked_rejects_changed_direct_spec_and_live_path_version_without_writes() {
    for change in ["features", "version", "summary", "cargo", "missing-rust"] {
        let mut f = Fixture::new("path");
        drop(f.build(true));
        let cargo = f.root.join(".willow/rust/Cargo.lock");
        let project = f.root.join("project.lock");
        match change {
            "features" => {
                let path = f.root.join("project.toml");
                let text = fs::read_to_string(&path)
                    .unwrap()
                    .replace("default-features = false", "default-features = true");
                fs::write(path, text).unwrap();
            }
            "version" => {
                let path = f.root.join("native/Cargo.toml");
                let text = fs::read_to_string(&path).unwrap().replace("1.2.3", "1.2.4");
                fs::write(path, text).unwrap();
            }
            "summary" => {
                let text = fs::read_to_string(&project)
                    .unwrap()
                    .replace("1.2.3", "9.9.9");
                fs::write(&project, text).unwrap();
            }
            "cargo" => {
                fs::write(&cargo, "corrupt").unwrap();
            }
            _ => {
                let mut lock: toml::Table =
                    toml::from_str(&fs::read_to_string(&project).unwrap()).unwrap();
                lock.remove("rust");
                fs::write(&project, toml::to_string(&lock).unwrap()).unwrap();
            }
        }
        let a = fs::read(&cargo).unwrap();
        let b = fs::read(&project).unwrap();
        f.options.locked = true;
        let error = build_bridge(&f.manifest(), &f.root, &f.options, true).unwrap_err();
        assert!(
            error.to_string().contains("rust_lockfile_stale"),
            "{change}: {error:#}"
        );
        assert_eq!(a, fs::read(&cargo).unwrap());
        assert_eq!(b, fs::read(&project).unwrap());
    }
}

#[test]
fn offline_missing_registry_dependency_fails_without_publishing_rust_lock() {
    let f = Fixture::new("path");
    let mut manifest = f.manifest();
    manifest.rust_dependencies.insert(
        "willow_nonexistent_offline_fixture_987654321".into(),
        willow_compiler::project::RustDependencySpec {
            version: Some("=0.0.987654321".into()),
            ..Default::default()
        },
    );
    let error = build_bridge(&manifest, &f.root, &f.options, true).unwrap_err();
    assert!(format!("{error:#}").contains("offline"));
    assert!(!f.root.join(".willow/rust/Cargo.lock").exists());
}

#[test]
fn willow_lock_refresh_preserves_rust_summary() {
    let f = Fixture::new("path");
    drop(f.build(true));
    let project = f.root.join("project.lock");
    let before: toml::Value = toml::from_str(&fs::read_to_string(&project).unwrap()).unwrap();
    let manifest = f.root.join("project.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap()
        .replace("version = \"0.1.0\"", "version = \"0.2.0\"");
    fs::write(&manifest, text).unwrap();
    willow_compiler::package::fetch_packages(&f.root, false, true).unwrap();
    let after: toml::Value = toml::from_str(&fs::read_to_string(&project).unwrap()).unwrap();
    assert_eq!(before["rust"], after["rust"]);
}

#[test]
fn cli_build_run_frozen_and_stale_rust_graph() {
    let f = Fixture::new("path");
    fs::create_dir_all(f.root.join("src")).unwrap();
    fs::write(f.root.join("src/main.wi"), "fn main() { println(42); }\n").unwrap();
    let home = f.temp.join("home");
    fs::create_dir_all(&home).unwrap();
    let original_home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .unwrap();
    let command = |args: &[&str]| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_willow"));
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
        command.output().unwrap()
    };
    let first = command(&["build", "--offline", "-o", "app"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let cargo = fs::read(f.root.join(".willow/rust/Cargo.lock")).unwrap();
    let project = fs::read(f.root.join("project.lock")).unwrap();
    let frozen = command(&["run", "--frozen"]);
    assert!(
        frozen.status.success(),
        "{}",
        String::from_utf8_lossy(&frozen.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&frozen.stdout), "42\n");
    assert_eq!(
        cargo,
        fs::read(f.root.join(".willow/rust/Cargo.lock")).unwrap()
    );
    assert_eq!(project, fs::read(f.root.join("project.lock")).unwrap());
    let manifest_path = f.root.join("project.toml");
    let original = fs::read_to_string(&manifest_path).unwrap();
    fs::write(&manifest_path, original.replace("0.1.0", "0.2.0")).unwrap();
    let stale_willow = command(&["build", "--frozen", "-o", "app"]);
    assert!(!stale_willow.status.success());
    assert!(String::from_utf8_lossy(&stale_willow.stderr).contains("lockfile_stale"));
    assert_eq!(
        cargo,
        fs::read(f.root.join(".willow/rust/Cargo.lock")).unwrap()
    );
    assert_eq!(project, fs::read(f.root.join("project.lock")).unwrap());
    fs::write(&manifest_path, &original).unwrap();
    let path = f.root.join("project.toml");
    let text = fs::read_to_string(&path)
        .unwrap()
        .replace("default-features = false", "default-features = true");
    fs::write(&path, text).unwrap();
    let stale = command(&["build", "--locked", "-o", "app"]);
    assert!(!stale.status.success());
    assert!(String::from_utf8_lossy(&stale.stderr).contains("rust_lockfile_stale"));
    assert_eq!(
        cargo,
        fs::read(f.root.join(".willow/rust/Cargo.lock")).unwrap()
    );
    assert_eq!(project, fs::read(f.root.join("project.lock")).unwrap());
}
