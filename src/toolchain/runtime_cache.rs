//! Conservative, process-independent proof that our last runtime build is current.
//!
//! A successful Cargo invocation publishes a stamp only if inputs stayed stable
//! across the build. Source content participates as well as metadata, so equal
//! or coarse mtimes cannot hide an edit. Uncertain inputs always fall back to Cargo.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct FileStamp {
    len: u64,
    seconds: u64,
    nanos: u32,
}

impl FileStamp {
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)?;
        anyhow::ensure!(metadata.is_file(), "not a regular file: {}", path.display());
        let modified = metadata.modified()?.duration_since(UNIX_EPOCH)?;
        Ok(Self {
            len: metadata.len(),
            seconds: modified.as_secs(),
            nanos: modified.subsec_nanos(),
        })
    }
}

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct BuildStamp {
    version: u32,
    inputs: u64,
    archive: ArchiveStamp,
}

const BUILD_STAMP_VERSION: u32 = 2;

#[derive(PartialEq, Eq, Serialize, Deserialize)]
struct ArchiveStamp {
    metadata: FileStamp,
    digest: u64,
}

impl ArchiveStamp {
    fn read(path: &Path) -> Result<Self> {
        let metadata = FileStamp::read(path)?;
        anyhow::ensure!(metadata.len > 0, "empty runtime archive");
        let mut file = fs::File::open(path)?;
        let mut hash = DefaultHasher::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut bytes = 0_u64;
        loop {
            let count = match file.read(&mut buffer) {
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                result => result?,
            };
            if count == 0 {
                break;
            }
            hash.write(&buffer[..count]);
            bytes += count as u64;
        }
        anyhow::ensure!(
            bytes == metadata.len && FileStamp::read(path)? == metadata,
            "archive changed while reading"
        );
        Ok(Self {
            metadata,
            digest: hash.finish(),
        })
    }
}

fn hash_file(path: &Path, hash: &mut DefaultHasher) -> Result<()> {
    path.hash(hash);
    let before = FileStamp::read(path)?;
    before.hash(hash);
    fs::read(path)?.hash(hash);
    anyhow::ensure!(
        FileStamp::read(path)? == before,
        "input changed while reading"
    );
    Ok(())
}

fn hash_optional_file(path: &Path, hash: &mut DefaultHasher) -> Result<()> {
    path.hash(hash);
    match fs::symlink_metadata(path) {
        Ok(_) => hash_file(path, hash),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            false.hash(hash);
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

fn reject_custom_config(path: &Path) -> Result<()> {
    match fs::read_to_string(path) {
        Ok(contents) => {
            let config: toml::Table = toml::from_str(&contents)?;
            anyhow::ensure!(
                config.is_empty(),
                "custom Cargo configuration requires Cargo"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_manifest(path: &Path, crates: &Path) -> Result<()> {
    let manifest: toml::Table = toml::from_str(&fs::read_to_string(path)?)?;
    anyhow::ensure!(
        !manifest.contains_key("patch") && !manifest.contains_key("replace"),
        "dependency overrides require Cargo"
    );
    fn visit(value: &toml::Value, base: &Path, crates: &Path) -> Result<()> {
        match value {
            toml::Value::Table(table) => {
                if let Some(path) = table.get("path").and_then(toml::Value::as_str) {
                    // All local dependency and source paths must live in the
                    // scanned tree. Unrecognized path-bearing tables fail closed.
                    anyhow::ensure!(
                        base.join(path).canonicalize()?.starts_with(crates),
                        "external local input requires Cargo"
                    );
                }
                for value in table.values() {
                    visit(value, base, crates)?;
                }
            }
            toml::Value::Array(values) => {
                for value in values {
                    visit(value, base, crates)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    // The workspace compiler's own bin/lib paths are not runtime dependencies.
    for key in [
        "dependencies",
        "dev-dependencies",
        "build-dependencies",
        "target",
        "workspace",
    ] {
        if let Some(value) = manifest.get(key) {
            visit(value, path.parent().unwrap(), crates)?;
        }
    }
    anyhow::ensure!(
        !path.parent().unwrap().join("build.rs").exists(),
        "local build scripts require Cargo"
    );
    anyhow::ensure!(
        manifest
            .get("package")
            .and_then(|p| p.get("build"))
            .is_none_or(|value| value.as_bool() == Some(false)),
        "local build scripts require Cargo"
    );
    if path.parent().unwrap().canonicalize()?.starts_with(crates) {
        // Unlike the compiler workspace manifest, dependency source targets
        // (including [lib] and [[bin]] paths) must also remain in the scan.
        visit(
            &toml::Value::Table(manifest),
            path.parent().unwrap(),
            crates,
        )?;
    }
    Ok(())
}

fn validate_local_manifests(path: &Path, crates: &Path) -> Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            validate_local_manifests(&entry.path(), crates)?;
        } else if entry.file_name() == "Cargo.toml" {
            validate_manifest(&entry.path(), crates)?;
        } else if entry
            .path()
            .extension()
            .is_some_and(|extension| extension == "rs")
        {
            validate_source_directives(&entry.path(), crates)?;
        }
    }
    Ok(())
}

fn validate_source_directives(path: &Path, crates: &Path) -> Result<()> {
    // Do not pretend the fingerprint is a Rust macro/dependency resolver.
    // Unsupported source inclusion uses Cargo. False positives in comments
    // merely lose the optimization; ordinary local #[path] modules are tracked.
    let source = fs::read_to_string(path)?;
    let custom_inclusion = source.split("include").skip(1).any(|tail| {
        ["", "_str", "_bytes"].iter().any(|suffix| {
            tail.strip_prefix(suffix)
                .is_some_and(|tail| tail.trim_start().starts_with('!'))
        })
    }) || (source.contains("cfg_attr") && source.contains("path"));
    anyhow::ensure!(!custom_inclusion, "custom source inclusion requires Cargo");
    for tail in source.split('#').skip(1) {
        let Some(tail) = tail.trim_start().strip_prefix('[') else {
            continue;
        };
        let Some(tail) = tail.trim_start().strip_prefix("path") else {
            continue;
        };
        let Some(tail) = tail.trim_start().strip_prefix('=') else {
            continue;
        };
        let tail = tail
            .trim_start()
            .strip_prefix('"')
            .ok_or_else(|| anyhow::anyhow!("custom module path"))?;
        let (relative, _) = tail
            .split_once('"')
            .ok_or_else(|| anyhow::anyhow!("custom module path"))?;
        anyhow::ensure!(
            !relative.contains('\\'),
            "escaped module path requires Cargo"
        );
        anyhow::ensure!(
            path.parent()
                .unwrap()
                .join(relative)
                .canonicalize()?
                .starts_with(crates),
            "external module requires Cargo"
        );
    }
    Ok(())
}

fn hash_tree(path: &Path, hash: &mut DefaultHasher) -> Result<()> {
    let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let kind = entry.file_type()?;
        if kind.is_dir() {
            hash_tree(&entry.path(), hash)?;
        } else if kind.is_file() {
            // Include non-Rust inputs too, for future include_* and build scripts.
            hash_file(&entry.path(), hash)?;
        } else {
            anyhow::bail!("uncertain runtime input {}", entry.path().display());
        }
    }
    Ok(())
}

pub(super) fn fingerprint(manifest: &Path, archive: &Path) -> Result<u64> {
    let mut hash = DefaultHasher::new();
    std::env::current_dir()?.hash(&mut hash);
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        let custom_build = matches!(
            key.as_ref(),
            "RUSTC"
                | "RUSTC_WRAPPER"
                | "RUSTC_WORKSPACE_WRAPPER"
                | "RUSTFLAGS"
                | "CARGO_ENCODED_RUSTFLAGS"
        ) || key.starts_with("CARGO_BUILD_")
            || (key.starts_with("CARGO_TARGET_") && key != "CARGO_TARGET_DIR")
            || key.starts_with("CARGO_PROFILE_")
            || key.starts_with("CARGO_SOURCE_")
            || key.starts_with("CARGO_REGISTRIES_");
        anyhow::ensure!(!custom_build, "custom build environment requires Cargo");
    }
    let crates = manifest.join("crates").canonicalize()?;
    validate_manifest(&manifest.join("Cargo.toml"), &crates)?;
    validate_local_manifests(&crates, &crates)?;
    archive.hash(&mut hash); // Includes profile and selected target directory.
    hash_file(&manifest.join("Cargo.toml"), &mut hash)?;
    hash_file(&manifest.join("Cargo.lock"), &mut hash)?;
    // Track all local workspace dependencies, including future runtime inputs.
    hash_tree(&manifest.join("crates"), &mut hash)?;
    for ancestor in manifest.ancestors() {
        for name in [
            ".cargo/config",
            ".cargo/config.toml",
            "rust-toolchain",
            "rust-toolchain.toml",
        ] {
            if name.starts_with(".cargo/") {
                reject_custom_config(&ancestor.join(name))?;
            }
            hash_optional_file(&ancestor.join(name), &mut hash)?;
        }
    }
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|home| PathBuf::from(home).join(".cargo"))
        });
    if let Some(home) = cargo_home {
        let home = if home.is_absolute() {
            home
        } else {
            manifest.join(home)
        };
        for name in ["config", "config.toml"] {
            reject_custom_config(&home.join(name))?;
            hash_optional_file(&home.join(name), &mut hash)?;
        }
    } else {
        anyhow::bail!("cannot determine Cargo configuration directory");
    }
    // Hash, never persist, environment values: Cargo/build scripts may consult
    // arbitrary variables, and credentials must not appear in the stamp.
    let mut environment: Vec<_> = std::env::vars_os()
        .filter(|(key, _)| key != "WILLOW_FORCE_RUNTIME_BUILD")
        .collect();
    environment.sort();
    environment.hash(&mut hash);
    // Rustup may change the selected compiler without changing any source.
    // This is cheaper than Cargo's dependency/fingerprint probe on warm builds.
    let rustc = Command::new("rustc")
        .arg("-vV")
        .current_dir(manifest)
        .output()?;
    anyhow::ensure!(rustc.status.success(), "cannot identify runtime toolchain");
    rustc.stdout.hash(&mut hash);
    let compiler = std::env::current_exe()?;
    compiler.hash(&mut hash);
    FileStamp::read(&compiler)?.hash(&mut hash);
    Ok(hash.finish())
}

pub(super) fn build_if_stale(
    archive: &Path,
    force: bool,
    mut inputs: impl FnMut() -> Result<u64>,
    build: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let stamp_path = archive.with_extension("willow-fresh.json");
    let before = inputs().ok();
    if !force
        && let Some(inputs) = before
        && let Ok(contents) = fs::read(&stamp_path)
        && let Ok(stamp) = serde_json::from_slice::<BuildStamp>(&contents)
        && stamp.version == BUILD_STAMP_VERSION
        && stamp.inputs == inputs
        // Avoid streaming an archive when the cheaper input proof is stale.
        && ArchiveStamp::read(archive).ok() == Some(stamp.archive)
    {
        return Ok(());
    }
    // Invalidate BEFORE Cargo can replace the archive, including on failure.
    match fs::remove_file(&stamp_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("cannot invalidate runtime build stamp"),
    }
    build()?;
    if let Some(before) = before
        && inputs().ok() == Some(before)
        && let Ok(archive) = ArchiveStamp::read(archive)
    {
        let stamp = BuildStamp {
            version: BUILD_STAMP_VERSION,
            inputs: before,
            archive,
        };
        // The caller holds the runtime lock. A partial or failed stamp write
        // simply fails JSON validation and forces a build next time.
        let _ = fs::write(stamp_path, serde_json::to_vec(&stamp)?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "willow-runtime-cache-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("source.rs"), "original").unwrap();
            Self(root)
        }
        fn archive(&self) -> PathBuf {
            self.0.join("runtime.a")
        }
        fn inputs(&self) -> Result<u64> {
            let mut hash = DefaultHasher::new();
            hash_file(&self.0.join("source.rs"), &mut hash)?;
            Ok(hash.finish())
        }
        fn build(&self, count: &Cell<usize>, force: bool) -> Result<()> {
            build_if_stale(
                &self.archive(),
                force,
                || self.inputs(),
                || {
                    count.set(count.get() + 1);
                    fs::write(self.archive(), "archive")?;
                    Ok(())
                },
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn consecutive_builds_skip_only_after_successful_publication() {
        let fixture = Fixture::new();
        let count = Cell::new(0);
        fixture.build(&count, false).unwrap();
        fixture.build(&count, false).unwrap();
        assert_eq!(count.get(), 1);
        fixture.build(&count, true).unwrap();
        fixture.build(&count, false).unwrap();
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn equal_mtime_and_size_cannot_hide_changed_source_content() {
        let fixture = Fixture::new();
        let count = Cell::new(0);
        fixture.build(&count, false).unwrap();
        let path = fixture.0.join("source.rs");
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "modified").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        fixture.build(&count, false).unwrap();
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn equal_mtime_and_size_cannot_hide_changed_archive_content() {
        let fixture = Fixture::new();
        let count = Cell::new(0);
        fixture.build(&count, false).unwrap();
        let archive = fixture.archive();
        let before = FileStamp::read(&archive).unwrap();
        let modified = fs::metadata(&archive).unwrap().modified().unwrap();
        fs::write(&archive, "partial").unwrap();
        fs::File::options()
            .write(true)
            .open(&archive)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(FileStamp::read(&archive).unwrap(), before);
        fixture.build(&count, false).unwrap();
        assert_eq!(
            count.get(),
            2,
            "replaced bytes must invalidate the archive proof"
        );
        assert_eq!(fs::read(&archive).unwrap(), b"archive");
        fixture.build(&count, false).unwrap();
        assert_eq!(count.get(), 2, "rebuilt archive should be fresh");
    }

    #[test]
    fn touching_source_without_changing_content_rebuilds() {
        let fixture = Fixture::new();
        let count = Cell::new(0);
        fixture.build(&count, false).unwrap();
        let source = fixture.0.join("source.rs");
        let modified = fs::metadata(&source).unwrap().modified().unwrap();
        fs::File::options()
            .write(true)
            .open(source)
            .unwrap()
            .set_times(
                fs::FileTimes::new().set_modified(modified + std::time::Duration::from_secs(2)),
            )
            .unwrap();
        fixture.build(&count, false).unwrap();
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn missing_replaced_or_corrupt_artifact_and_stamp_rebuild() {
        let fixture = Fixture::new();
        let count = Cell::new(0);
        fixture.build(&count, false).unwrap();
        fs::remove_file(fixture.archive()).unwrap();
        fixture.build(&count, false).unwrap();
        fs::write(fixture.archive(), "partial").unwrap();
        fixture.build(&count, false).unwrap();
        fs::write(fixture.archive().with_extension("willow-fresh.json"), "{").unwrap();
        fixture.build(&count, false).unwrap();
        assert_eq!(count.get(), 4);
    }

    #[test]
    fn directory_archive_is_never_fresh() {
        let fixture = Fixture::new();
        let count = Cell::new(0);
        fixture.build(&count, false).unwrap();
        fs::remove_file(fixture.archive()).unwrap();
        fs::create_dir(fixture.archive()).unwrap();
        assert!(fixture.build(&count, false).is_err());
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn failure_invalidates_previous_proof_even_when_inputs_are_restored() {
        let fixture = Fixture::new();
        let count = Cell::new(0);
        fixture.build(&count, false).unwrap();
        let result = build_if_stale(
            &fixture.archive(),
            true,
            || fixture.inputs(),
            || anyhow::bail!("failed build"),
        );
        assert!(result.is_err());
        fixture.build(&count, false).unwrap();
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn uncertain_or_changing_inputs_do_not_publish_proof() {
        let fixture = Fixture::new();
        let count = Cell::new(0);
        for _ in 0..2 {
            build_if_stale(
                &fixture.archive(),
                false,
                || anyhow::bail!("unreadable"),
                || {
                    count.set(count.get() + 1);
                    fs::write(fixture.archive(), "archive")?;
                    Ok(())
                },
            )
            .unwrap();
        }
        assert_eq!(count.get(), 2);
        build_if_stale(
            &fixture.archive(),
            false,
            || fixture.inputs(),
            || {
                fs::write(fixture.0.join("source.rs"), "changed during Cargo")?;
                Ok(())
            },
        )
        .unwrap();
        assert!(
            !fixture
                .archive()
                .with_extension("willow-fresh.json")
                .exists()
        );
    }

    #[test]
    fn nested_new_files_and_manifest_changes_change_tree_identity() {
        let fixture = Fixture::new();
        let digest = || {
            let mut hash = DefaultHasher::new();
            hash_tree(&fixture.0, &mut hash).unwrap();
            hash.finish()
        };
        let original = digest();
        fs::create_dir_all(fixture.0.join("nested/deep")).unwrap();
        fs::write(fixture.0.join("nested/deep/new.rs"), "new source").unwrap();
        let added = digest();
        assert_ne!(original, added);
        fs::write(fixture.0.join("Cargo.toml"), "manifest").unwrap();
        let manifest = digest();
        assert_ne!(added, manifest);
        fs::write(fixture.0.join("Cargo.lock"), "lockfile").unwrap();
        assert_ne!(manifest, digest());
    }

    #[test]
    fn custom_configuration_and_external_inputs_require_cargo() {
        let fixture = Fixture::new();
        let config = fixture.0.join("config.toml");
        assert!(reject_custom_config(&config).is_ok());
        fs::write(&config, "# no custom configuration\n").unwrap();
        assert!(reject_custom_config(&config).is_ok());
        fs::write(&config, "[build]\nrustc-wrapper = '/tmp/wrapper'\n").unwrap();
        assert!(reject_custom_config(&config).is_err());
        let manifest = fixture.0.join("Cargo.toml");
        let crates = fixture.0.join("crates");
        fs::create_dir_all(crates.join("local")).unwrap();
        let crates = crates.canonicalize().unwrap();
        fs::write(&manifest, "[dependencies.local]\npath = 'crates/local'\n").unwrap();
        assert!(validate_manifest(&manifest, &crates).is_ok());
        fs::write(&manifest, "[dependencies.external]\npath = '..'\n").unwrap();
        assert!(validate_manifest(&manifest, &crates).is_err());
        fs::write(&manifest, "[patch.crates-io.libc]\npath = 'crates/local'\n").unwrap();
        assert!(validate_manifest(&manifest, &crates).is_err());
        fs::write(&manifest, "[package]\nbuild = 'generate.rs'\n").unwrap();
        assert!(validate_manifest(&manifest, &crates).is_err());
        let nested = crates.join("local/nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(
            nested.join("Cargo.toml"),
            "[lib]\npath = '../../../source.rs'\n",
        )
        .unwrap();
        assert!(validate_local_manifests(&crates, &crates).is_err());
        fs::write(
            nested.join("Cargo.toml"),
            "[package]\nbuild = 'generate.rs'\n",
        )
        .unwrap();
        assert!(validate_local_manifests(&crates, &crates).is_err());
    }

    #[test]
    fn profile_archives_have_independent_proof() {
        let fixture = Fixture::new();
        let count = Cell::new(0);
        for profile in ["debug", "release", "debug", "release"] {
            let directory = fixture.0.join(profile);
            fs::create_dir_all(&directory).unwrap();
            let archive = directory.join("runtime.a");
            build_if_stale(
                &archive,
                false,
                || fixture.inputs(),
                || {
                    count.set(count.get() + 1);
                    fs::write(&archive, profile)?;
                    Ok(())
                },
            )
            .unwrap();
        }
        assert_eq!(count.get(), 2);
    }

    #[test]
    fn external_module_and_macro_inputs_require_cargo() {
        let fixture = Fixture::new();
        let crates = fixture.0.join("crates");
        fs::create_dir_all(&crates).unwrap();
        let crates = crates.canonicalize().unwrap();
        let source = crates.join("lib.rs");
        fs::write(crates.join("local.rs"), "").unwrap();
        fs::write(&source, "# [ path = \"local.rs\" ] mod local;").unwrap();
        assert!(validate_source_directives(&source, &crates).is_ok());
        for contents in [
            "#[path = \"../source.rs\"] mod external;",
            "include ! (\"../source.rs\");",
            "include_str ! (\"../source.rs\");",
            "include_bytes! (\"../source.rs\");",
            "#[cfg_attr(test, path = \"../source.rs\")] mod external;",
        ] {
            fs::write(&source, contents).unwrap();
            assert!(
                validate_source_directives(&source, &crates).is_err(),
                "{contents}"
            );
        }
    }
}
