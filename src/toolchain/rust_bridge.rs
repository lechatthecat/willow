//! Cargo owns dependency resolution and freshness; this layer owns the hidden
//! bridge project and its toolchain/ABI cache identity, never compiler IR.
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::BuildMode;
use crate::project::{ProjectManifest, RustDependencySource, RustGitSelector};

pub mod commands;

const BRIDGE_NAME: &str = "willow_generated_bridge";

#[derive(Debug, Clone)]
pub struct RustToolchain {
    pub cargo: PathBuf,
    pub rustc: PathBuf,
    pub cargo_version: String,
    pub rustc_version: String,
    pub host: String,
}

impl RustToolchain {
    pub fn detect(
        cargo: impl Into<PathBuf>,
        rustc: impl Into<PathBuf>,
        root: &Path,
    ) -> Result<Self> {
        let cargo = cargo.into();
        let rustc = rustc.into();
        let version = |program: &Path, argument: &str, code: &str| -> Result<String> {
            let output = Command::new(program)
                .arg(argument)
                .current_dir(root)
                .output()
                .with_context(|| format!("{code}: cannot execute {}", program.display()))?;
            if !output.status.success() {
                bail!("{code}: {}", String::from_utf8_lossy(&output.stderr));
            }
            Ok(String::from_utf8(output.stdout)?.trim().to_owned())
        };
        let cargo_version = version(&cargo, "--version", "cargo_missing")?;
        let rustc_version = version(&rustc, "-vV", "rust_toolchain_missing")?;
        let host = rustc_version
            .lines()
            .find_map(|line| line.strip_prefix("host: "))
            .context("rust_toolchain_missing: rustc -vV did not report a host")?
            .to_owned();
        Ok(Self {
            cargo,
            rustc,
            cargo_version,
            rustc_version,
            host,
        })
    }
}

/// Every field is length-delimited before hashing (no ambiguous concatenation).
#[derive(Debug, Clone, Serialize)]
pub struct BridgeCacheKey<'a> {
    pub target: &'a str,
    pub profile: &'a str,
    pub rustc_version: &'a str,
    pub cargo_version: &'a str,
    pub lock_hash: &'a str,
    pub bridge_source_hash: &'a str,
    pub wrapper_schema: &'a str,
    pub abi_revision: &'a str,
    pub manifest_hash: &'a str,
}

impl BridgeCacheKey<'_> {
    pub fn digest(&self) -> String {
        hash(&serde_json::to_vec(self).expect("string-only cache key"))
    }
}

#[derive(Debug, Clone)]
pub struct BridgeOptions {
    /// Resolve only in the external cache, without project lock publication.
    pub read_only: bool,
    pub symbols: Vec<crate::rust_bridge::RustBridgeSymbol>,
    /// Parent of the per-project hidden project; defaults to ~/.willow/build.
    pub cache_root: PathBuf,
    pub mode: BuildMode,
    /// None means rustc's host. Future target-aware callers supply a triple.
    pub target: Option<String>,
    pub offline: bool,
    pub locked: bool,
    pub cargo: PathBuf,
    pub rustc: PathBuf,
    pub wrapper_schema: String,
    pub abi_revision: String,
}

impl BridgeOptions {
    pub fn new(mode: BuildMode) -> Result<Self> {
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .context("cannot locate home directory for Rust bridge cache")?;
        Ok(Self {
            read_only: false,
            symbols: Vec::new(),
            cache_root: PathBuf::from(home).join(".willow/build"),
            mode,
            target: None,
            offline: false,
            locked: false,
            cargo: "cargo".into(),
            rustc: "rustc".into(),
            wrapper_schema: crate::rust_bridge::WRAPPER_SCHEMA.into(),
            abi_revision: willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION.to_string(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CargoArtifact {
    pub package_id: String,
    pub target: Value,
    pub profile: Value,
    pub filenames: Vec<PathBuf>,
    pub fresh: bool,
}

#[derive(Debug, Default)]
pub struct CargoMessages {
    pub artifacts: Vec<CargoArtifact>,
    pub diagnostics: Vec<Value>,
    /// Unsplit rustc linker argument text: order, quoting and duplicates matter.
    pub native_static_libs: Option<String>,
    pub success: bool,
}

/// Parse only the JSON message protocol, never Cargo's human stderr rendering.
/// Unknown message reasons are deliberately forward-compatible.
pub fn parse_cargo_messages(bytes: &[u8], bridge_manifest: &Path) -> Result<CargoMessages> {
    let mut result = CargoMessages::default();
    for line in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let message: Value = serde_json::from_slice(line).context("invalid Cargo JSON message")?;
        match message["reason"].as_str() {
            Some("compiler-artifact") => result.artifacts.push(serde_json::from_value(message)?),
            Some("compiler-message") => {
                if message["manifest_path"].as_str().map(Path::new) == Some(bridge_manifest)
                    && message["target"]["name"] == BRIDGE_NAME
                    && let Some(libs) = message["message"]["message"]
                        .as_str()
                        .and_then(|text| text.strip_prefix("native-static-libs: "))
                {
                    result.native_static_libs = Some(libs.to_owned());
                }
                result.diagnostics.push(message);
            }
            Some("build-finished") => {
                result.success = message["success"].as_bool().unwrap_or(false)
            }
            _ => {}
        }
    }
    Ok(result)
}

#[derive(Debug)]
pub struct BridgeBuild {
    pub toolchain: RustToolchain,
    pub directory: PathBuf,
    pub cache_key: String,
    pub target: String,
    pub staticlib: Option<PathBuf>,
    pub messages: CargoMessages,
    /// Cargo metadata includes exact package versions and resolved graph.
    pub metadata: Value,
    pub direct_dependencies: Value,
    // Hold until caller finishes consuming/linking the artifact. Another Willow
    // process must not replace the stable Cargo staticlib alias during linking.
    _lease: File,
    _project_lease: Option<File>,
    resolution: crate::package::lock::RustLock,
}

impl BridgeBuild {
    fn persist_resolution(&self, root: &Path, options: &BridgeOptions) -> Result<()> {
        persist_resolution(root, &self.directory, options, &self.resolution)
    }

    pub fn ensure_target(&self, linker_target: &str) -> Result<()> {
        if self.target != linker_target {
            bail!(
                "rust_target_mismatch: bridge {} != linker {linker_target}",
                self.target
            );
        }
        Ok(())
    }
}

fn persist_resolution(
    root: &Path,
    directory: &Path,
    options: &BridgeOptions,
    resolution: &crate::package::lock::RustLock,
) -> Result<()> {
    if !root.join("project.lock").exists() {
        crate::package::fetch_packages(root, options.locked, options.offline)?;
    }
    let bytes = fs::read(directory.join("Cargo.lock"))?;
    let persisted = root.join(".willow/rust/Cargo.lock");
    if fs::read(&persisted).ok().as_deref() != Some(bytes.as_slice()) {
        crate::package::lock::atomic_write_validated(&persisted, &bytes, || {
            let _: toml::Table = toml::from_str(std::str::from_utf8(&bytes)?)?;
            Ok(())
        })?;
    }
    // An empty generated bridge still has a Cargo.lock, but Willow's locked
    // project validation requires no Rust summary when no Rust dependencies remain.
    let summary = (!resolution.dependencies.is_empty()).then(|| resolution.clone());
    crate::package::lock::write_rust_lock(root, summary)
}

/// R1 toolchain API. `check` selects Cargo check instead of staticlib production.
/// Plain Willow projects return before invoking either Rust executable.
/// Cargo is still consulted on a cache hit: its dep-info/build-script tracking is
/// the authority for transitive files and environment inputs. `fresh` proves no
/// compilation occurred; skipping Cargo would silently miss these inputs.
pub fn build_bridge(
    project: &ProjectManifest,
    root: &Path,
    options: &BridgeOptions,
    check: bool,
) -> Result<Option<BridgeBuild>> {
    prepare_bridge(
        project,
        root,
        options,
        if check {
            BridgeAction::Check
        } else {
            BridgeAction::Build
        },
    )
}

/// Resolve dependencies and generate the wrapper without compiling it.
pub fn inspect_bridge(
    project: &ProjectManifest,
    root: &Path,
    options: &BridgeOptions,
) -> Result<Option<BridgeBuild>> {
    prepare_bridge(project, root, options, BridgeAction::Metadata)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditLock<'a> {
    Persisted,
    Candidate(&'a [u8]),
    /// Resolve preferred versions without the old graph's reuse preferences.
    Fresh,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BridgeAction<'a> {
    Metadata,
    Check,
    Build,
    Edit {
        /// Cargo package ID, already projected from the original direct graph.
        update: Option<Option<&'a str>>,
        /// Owned by the edit caller across prepare passes; never published early.
        lock: EditLock<'a>,
    },
}

fn prepare_bridge(
    project: &ProjectManifest,
    root: &Path,
    options: &BridgeOptions,
    action: BridgeAction<'_>,
) -> Result<Option<BridgeBuild>> {
    anyhow::ensure!(
        !options.read_only || matches!(action, BridgeAction::Metadata | BridgeAction::Edit { .. }),
        "read-only Rust resolution cannot compile adapters"
    );
    if project.rust_dependencies.is_empty() && !matches!(action, BridgeAction::Edit { .. }) {
        return Ok(None);
    }
    if !options.symbols.is_empty()
        && options.abi_revision != willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION.to_string()
    {
        bail!("rust_bridge_abi_mismatch: incompatible adapter revision");
    }
    let root = canonicalize(root)?;
    let bridge = project
        .rust
        .as_ref()
        .context("rust_bridge_missing: [rust] bridge is required")?
        .resolve_bridge(&root)?;
    let toolchain = RustToolchain::detect(&options.cargo, &options.rustc, &root)?;
    let target = options.target.as_deref().unwrap_or(&toolchain.host);
    // Parse before Cargo/linking; Cargo validates installation of the target.
    target
        .parse::<target_lexicon::Triple>()
        .map_err(|e| anyhow::anyhow!("rust_target_mismatch: {e}"))?;
    let profile = if options.mode == BuildMode::Release {
        "release"
    } else {
        "dev"
    };
    let cache_root = canonical_output_path(&options.cache_root)?;
    if cache_root.starts_with(&root) {
        bail!("Rust bridge cache must be outside the project source tree");
    }
    let directory = cache_root
        .join(hash(root.as_os_str().as_encoded_bytes()))
        .join("rust-bridge");
    fs::create_dir_all(&directory)?;
    let directory = canonicalize(&directory)?;
    if directory.starts_with(&root) {
        bail!("Rust bridge cache must be outside the project source tree");
    }
    let lease = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(".willow.lock"))?;
    lease.lock().context("cannot lock Rust bridge cache")?;
    let rust_directory = root.join(".willow/rust");
    let project_lease = if options.read_only {
        None
    } else {
        fs::create_dir_all(&rust_directory)?;
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(rust_directory.join(".lock"))?;
        lease
            .lock()
            .context("cannot lock project Rust resolution")?;
        Some(lease)
    };
    fs::create_dir_all(directory.join("src"))?;
    let manifest = generated_manifest(project, &root)?;
    let manifest_path = directory.join("Cargo.toml");
    let lib_path = directory.join("src/lib.rs");
    write_changed(
        &directory.join("willow-symbols.json"),
        &serde_json::to_vec_pretty(&options.symbols)?,
    )?;
    let input_hash = crate::package::lock::rust_input_hash(
        project,
        &options.wrapper_schema,
        &options.abi_revision,
    )?;
    let persisted = root.join(".willow/rust/Cargo.lock");
    let previous = crate::package::lock::read_rust_lock(&root)
        .context("rust_lockfile_stale: invalid project.lock")?;
    if options.locked {
        let valid = previous.as_ref().is_some_and(|lock| {
            lock.bridge_input_hash == input_hash
                && hash_file(&persisted).ok().as_ref() == Some(&lock.cargo_lock_hash)
        });
        if !valid {
            bail!("rust_lockfile_stale: missing or inconsistent Rust lock fingerprints");
        }
    }
    let cached_lock = directory.join("Cargo.lock");
    if let BridgeAction::Edit {
        lock: EditLock::Candidate(bytes),
        ..
    } = action
    {
        write_changed(&cached_lock, bytes)?;
    } else if matches!(
        action,
        BridgeAction::Edit {
            lock: EditLock::Fresh,
            ..
        }
    ) {
        if cached_lock.exists() {
            fs::remove_file(&cached_lock)?;
        }
    } else {
        match fs::read(&persisted) {
            Ok(bytes) => write_changed(&cached_lock, &bytes)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if cached_lock.exists() {
                    fs::remove_file(&cached_lock)?;
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    write_changed(&manifest_path, manifest.as_bytes())?;
    let mut source = format!(
        "#[path = {:?}]\npub mod bridge;\n",
        bridge.to_str().context("non-UTF8 bridge path")?
    );
    if !options.symbols.is_empty() {
        write_changed(
            &directory.join("src/willow_bridge_abi.rs"),
            include_bytes!("../../crates/willow_abi/src/ffi.rs"),
        )?;
        source.push_str(&crate::rust_bridge::wrappers(
            &options.symbols,
            &directory.join("src/willow_bridge_abi.rs"),
            willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION,
        ));
    }
    // Cargo metadata needs a lib target, but never rewrite a previous fingerprint
    // before determining the new key: that would invalidate every warm build.
    if !lib_path.exists() {
        write_changed(&lib_path, source.as_bytes())?;
    }
    let command = || {
        let mut cmd = Command::new(&toolchain.cargo);
        cmd.current_dir(&root).env("RUSTC", &toolchain.rustc);
        if let Some(home) = std::env::var_os("WILLOW_CARGO_HOME") {
            cmd.env("CARGO_HOME", home);
        }
        cmd
    };
    if let BridgeAction::Edit {
        update: Some(package),
        ..
    } = action
    {
        let mut update = command();
        update
            .args(["update", "--manifest-path"])
            .arg(&manifest_path);
        if let Some(package) = package {
            update.args(["--package", package]);
        }
        if options.offline {
            update.arg("--offline");
        }
        run(&mut update)?;
    }
    let mut metadata_command = command();
    metadata_command
        .args(["metadata", "--format-version=1", "--manifest-path"])
        .arg(&manifest_path)
        .arg("--filter-platform")
        .arg(target);
    if options.offline {
        metadata_command.arg("--offline");
    }
    if options.locked {
        metadata_command.arg("--locked");
    }
    let metadata_output = run(&mut metadata_command).with_context(|| {
        if options.locked {
            "rust_lockfile_stale: Cargo rejected locked resolution"
        } else {
            "Rust dependency resolution failed"
        }
    })?;
    let metadata: Value = serde_json::from_slice(&metadata_output.stdout)?;
    let lock_hash = hash_file(&directory.join("Cargo.lock"))?;
    let direct_dependencies = direct_dependency_metadata(
        &metadata,
        project.rust_dependencies.keys().map(String::as_str),
    )?;
    let rust_lock = crate::package::lock::RustLock {
        cargo_lock_hash: lock_hash.clone(),
        bridge_input_hash: input_hash,
        dependencies: direct_versions(&direct_dependencies)?,
    };
    if options.locked && previous.as_ref() != Some(&rust_lock) {
        bail!("rust_lockfile_stale: Cargo resolution or direct version summary changed");
    }
    let source_hash = hash(&serde_json::to_vec(&(
        hash_file(&bridge)?,
        &options.symbols,
    ))?);
    let manifest_hash = hash(manifest.as_bytes());
    let cache_key = BridgeCacheKey {
        target,
        profile,
        rustc_version: &toolchain.rustc_version,
        cargo_version: &toolchain.cargo_version,
        lock_hash: &lock_hash,
        bridge_source_hash: &source_hash,
        wrapper_schema: &options.wrapper_schema,
        abi_revision: &options.abi_revision,
        manifest_hash: &manifest_hash,
    }
    .digest();
    write_changed(
        &lib_path,
        format!("// Willow bridge key: {cache_key}\n{source}").as_bytes(),
    )?;
    if matches!(action, BridgeAction::Metadata | BridgeAction::Edit { .. }) {
        let build = BridgeBuild {
            directory,
            cache_key,
            target: target.to_owned(),
            staticlib: None,
            messages: CargoMessages::default(),
            metadata,
            direct_dependencies,
            toolchain,
            _lease: lease,
            _project_lease: project_lease,
            resolution: rust_lock,
        };
        if action == BridgeAction::Metadata && !options.locked && !options.read_only {
            build.persist_resolution(&root, options)?;
        }
        return Ok(Some(build));
    }
    if !options.locked && !options.read_only {
        persist_resolution(&root, &directory, options, &rust_lock)?;
    }
    let check = action == BridgeAction::Check;
    let mut cargo = command();
    cargo
        .arg(if check { "check" } else { "rustc" })
        .args(["--lib", "--message-format=json", "--manifest-path"])
        .arg(&manifest_path)
        .arg("--target-dir")
        .arg(directory.join("target"))
        .arg("--target")
        .arg(target)
        .arg("--profile")
        .arg(profile);
    if options.offline {
        cargo.arg("--offline");
    }
    cargo.arg("--locked");
    if !check {
        cargo.args(["--", "--print=native-static-libs"]);
    }
    let output = cargo
        .output()
        .context("cargo_missing: cannot execute Cargo")?;
    let messages = parse_cargo_messages(&output.stdout, &manifest_path)?;
    if !output.status.success() || !messages.success {
        let adapter_error = |codes: &[&str]| {
            !options.symbols.is_empty()
                && messages.diagnostics.iter().any(|diagnostic| {
                    let message = &diagnostic["message"];
                    message["code"]["code"]
                        .as_str()
                        .is_some_and(|code| codes.contains(&code))
                        && message["spans"].as_array().is_some_and(|spans| {
                            spans.iter().any(|span| {
                                span["is_primary"] == true
                                    && span["file_name"].as_str().is_some_and(|name| {
                                        let path = Path::new(name);
                                        path == lib_path || path == Path::new("src/lib.rs")
                                    })
                            })
                        })
                })
        };
        let mut error = crate::package::CommandError::new(
            if adapter_error(&["E0425", "E0603"]) {
                "rust_bridge_symbol_missing"
            } else if adapter_error(&["E0308", "E0277", "E0283"]) {
                "rust_bridge_signature_mismatch"
            } else {
                "rust_bridge_compile_error"
            },
            "Rust bridge compilation failed",
        );
        error.fields.insert(
            "diagnostics".into(),
            serde_json::to_value(&messages.diagnostics)?,
        );
        error.fields.insert(
            "cargo_stderr".into(),
            String::from_utf8_lossy(&output.stderr).into_owned().into(),
        );
        return Err(error.into());
    }
    let staticlib = if check {
        None
    } else {
        let artifact = messages
            .artifacts
            .iter()
            .find(|artifact| {
                artifact.target["name"] == BRIDGE_NAME
                    && artifact.target["src_path"].as_str().map(Path::new)
                        == Some(lib_path.as_path())
                    && artifact.target["crate_types"]
                        .as_array()
                        .is_some_and(|types| types.iter().any(|ty| ty == "staticlib"))
            })
            .context("Cargo did not report the bridge staticlib artifact")?;
        let path = artifact
            .filenames
            .iter()
            .find(|path| {
                matches!(
                    path.extension().and_then(|ext| ext.to_str()),
                    Some("a" | "lib")
                )
            })
            .context("Cargo staticlib artifact has no archive")?;
        if !path.is_file() {
            bail!("Cargo staticlib does not exist: {}", path.display());
        }
        if messages.native_static_libs.is_none() {
            bail!("Cargo did not report native-static-libs");
        }
        Some(path.clone())
    };
    Ok(Some(BridgeBuild {
        directory,
        cache_key,
        target: target.to_owned(),
        staticlib,
        messages,
        metadata,
        direct_dependencies,
        toolchain,
        _lease: lease,
        _project_lease: project_lease,
        resolution: rust_lock,
    }))
}

#[cfg(test)]
thread_local! { static SUMMARY_VISITS: std::cell::Cell<[usize; 4]> = const { std::cell::Cell::new([0; 4]) }; }
#[cfg(test)]
fn summary_visit(index: usize) {
    SUMMARY_VISITS.with(|counts| {
        let mut values = counts.get();
        values[index] += 1;
        counts.set(values);
    });
}

// Reuse the already projected edges for the persisted version summary.
fn direct_versions(dependencies: &Value) -> Result<std::collections::BTreeMap<String, String>> {
    dependencies
        .as_array()
        .context("Cargo direct dependencies missing")?
        .iter()
        .map(|d| {
            Ok((
                d["alias"]
                    .as_str()
                    .context("Cargo dependency name missing")?
                    .to_owned(),
                d["version"]
                    .as_str()
                    .context("Cargo dependency version missing")?
                    .to_owned(),
            ))
        })
        .collect()
}

/// Project manifest aliases via indexed direct packages (expected O(P + N + D)).
/// Cargo owns resolved versions and registry/git source identities.
pub fn direct_dependency_metadata<'a>(
    metadata: &Value,
    aliases: impl IntoIterator<Item = &'a str>,
) -> Result<Value> {
    let packages: std::collections::HashMap<_, _> = metadata["packages"]
        .as_array()
        .context("Cargo packages missing")?
        .iter()
        .map(|p| {
            #[cfg(test)]
            summary_visit(0);
            Ok((p["id"].as_str().context("Cargo package id missing")?, p))
        })
        .collect::<Result<_>>()?;
    let root = metadata["resolve"]["root"]
        .as_str()
        .context("Cargo root missing")?;
    let node = metadata["resolve"]["nodes"]
        .as_array()
        .context("Cargo nodes missing")?
        .iter()
        .find(|n| {
            #[cfg(test)]
            summary_visit(1);
            n["id"].as_str() == Some(root)
        })
        .context("Cargo root node missing")?;
    // Generated manifests currently have no `package` rename: each original
    // alias is the exact Cargo package name, not the normalized/custom lib name
    // in resolve.nodes[].deps[].name. Index only resolved direct packages so a
    // transitive package with the same name cannot replace the direct version.
    let direct: std::collections::HashMap<_, _> = node["deps"]
        .as_array()
        .context("Cargo direct dependencies missing")?
        .iter()
        .map(|d| {
            #[cfg(test)]
            summary_visit(2);
            let package = packages
                .get(d["pkg"].as_str().context("Cargo dependency id missing")?)
                .context("Cargo dependency package missing")?;
            Ok((
                package["name"]
                    .as_str()
                    .context("Cargo package name missing")?,
                *package,
            ))
        })
        .collect::<Result<_>>()?;
    let dependencies = aliases
        .into_iter()
        .map(|alias| {
            #[cfg(test)]
            summary_visit(3);
            let package = direct
                .get(alias)
                .context("Cargo manifest dependency missing from resolution")?;
            Ok(
                serde_json::json!({"alias":alias, "id":package["id"], "name":package["name"],
            "version":package["version"], "source": if package["source"].is_null() {
                serde_json::json!({"path":package["manifest_path"]})
            } else { package["source"].clone() }}),
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Value::Array(dependencies))
}

fn generated_manifest(project: &ProjectManifest, root: &Path) -> Result<String> {
    let mut dependencies = toml::Table::new();
    for (alias, spec) in &project.rust_dependencies {
        let dependency = spec.normalize(alias)?;
        let mut fields = toml::Table::new();
        match dependency.source {
            RustDependencySource::Registry { version } => {
                fields.insert("version".into(), version.into());
            }
            RustDependencySource::Path { path } => {
                let path = canonicalize(&root.join(path))?;
                fields.insert(
                    "path".into(),
                    path.to_str().context("non-UTF8 dependency path")?.into(),
                );
            }
            RustDependencySource::Git { url, selector } => {
                fields.insert("git".into(), url.as_str().into());
                match selector {
                    RustGitSelector::Default => {}
                    RustGitSelector::Revision(rev) => {
                        fields.insert("rev".into(), rev.into());
                    }
                    RustGitSelector::Tag(tag) => {
                        fields.insert("tag".into(), tag.into());
                    }
                }
            }
        }
        fields.insert(
            "features".into(),
            toml::Value::Array(dependency.features.into_iter().map(Into::into).collect()),
        );
        fields.insert(
            "default-features".into(),
            dependency.default_features.into(),
        );
        dependencies.insert(alias.clone(), toml::Value::Table(fields));
    }
    let mut manifest: toml::Table = toml::from_str(&format!(
        "[package]\nname = {BRIDGE_NAME:?}\nversion = \"0.0.0\"\nedition = \"2024\"\n[lib]\ncrate-type = [\"staticlib\"]\n[workspace]\n"
    ))?;
    manifest.insert("dependencies".into(), toml::Value::Table(dependencies));
    Ok(toml::to_string(&manifest)?)
}

fn run(command: &mut Command) -> Result<Output> {
    let output = command
        .output()
        .context("cargo_missing: cannot execute Cargo")?;
    if !output.status.success() {
        // Retain stderr verbatim for users; it is never scraped for metadata.
        bail!("cargo_failed: {}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(output)
}

fn write_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    if fs::read(path).ok().as_deref() != Some(bytes) {
        fs::write(path, bytes)?;
    }
    Ok(())
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// `fs::canonicalize` without the Windows `\\?\` verbatim prefix. MSVC's
/// `cl.exe` cannot open a verbatim archive path (LNK1104), and Cargo echoes
/// the `--target-dir` spelling into its artifact paths.
fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
    let path = fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        use std::path::{Component, Prefix};
        let mut components = path.components();
        let plain = match components.next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::VerbatimDisk(drive) => Some(format!("{}:", drive as char)),
                Prefix::VerbatimUNC(server, share) => Some(format!(
                    r"\\{}\{}",
                    server.to_string_lossy(),
                    share.to_string_lossy()
                )),
                _ => None,
            },
            _ => None,
        };
        if let Some(prefix) = plain {
            let mut plain = PathBuf::from(prefix + r"\");
            plain.extend(components.filter(|c| !matches!(c, Component::RootDir)));
            return Ok(plain);
        }
    }
    Ok(path)
}

// Resolve symlinks in the existing prefix before creating any output. This also
// rejects a cache configured inside the source tree without polluting that tree.
fn canonical_output_path(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut existing = absolute.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        suffix.push(existing.file_name().context("invalid cache path")?);
        existing = existing
            .parent()
            .context("cache path has no existing ancestor")?;
    }
    let mut resolved = canonicalize(existing)?;
    for component in suffix.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

fn hash_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; 8192];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::RustDependencySpec;

    #[test]
    fn direct_projection_preserves_aliases_and_sources_without_transitives() {
        let metadata = serde_json::json!({
            "packages":[
                {"id":"unused", "name":"actual-name", "version":"9.0.0"},
                {"id":"git", "name":"actual-name", "version":"2.3.4", "source":"git+https://example.invalid/repo#abc"},
                {"id":"registry", "name":"regex", "version":"1.12.0", "source":"registry+https://example.invalid/index"}
            ],
            "resolve":{"root":"root", "nodes":[{"id":"unused","deps":[]},
                {"id":"root","deps":[{"name":"renamed", "pkg":"git"},{"name":"regex", "pkg":"registry"}]}]}
        });
        let dependencies = direct_dependency_metadata(&metadata, ["actual-name", "regex"]).unwrap();
        assert_eq!(dependencies.as_array().unwrap().len(), 2);
        assert_eq!(dependencies[0]["alias"], "actual-name");
        assert_eq!(dependencies[0]["name"], "actual-name");
        assert_eq!(dependencies[0]["id"], "git");
        assert_eq!(
            dependencies[0]["source"],
            "git+https://example.invalid/repo#abc"
        );
        assert_eq!(dependencies[1]["version"], "1.12.0");
        assert_eq!(
            direct_versions(&dependencies).unwrap()["actual-name"],
            "2.3.4"
        );
        assert!(direct_dependency_metadata(&metadata, ["missing-alias"]).is_err());
    }

    #[test]
    fn manifest_cardinality_scales_without_duplicate_dependencies() {
        for count in [16, 64, 256, 1024] {
            let mut project: ProjectManifest =
                toml::from_str("[project]\nname = \"scaling\"\nversion = \"0.1.0\"\n").unwrap();
            for index in 0..count {
                project.rust_dependencies.insert(
                    format!("dependency_{index:04}"),
                    RustDependencySpec {
                        version: Some("1".into()),
                        ..Default::default()
                    },
                );
            }
            let manifest = generated_manifest(&project, Path::new(".")).unwrap();
            let parsed: toml::Table = toml::from_str(&manifest).unwrap();
            assert_eq!(parsed["dependencies"].as_table().unwrap().len(), count);
            eprintln!(
                "bridge_manifest dependencies={count} bytes={}",
                manifest.len()
            );
        }
    }

    #[test]
    fn summary_indexes_packages_once_for_wide_graphs_and_root_last() {
        for count in [16, 64, 256, 1024] {
            let packages: Vec<_> = (0..count)
                .map(|i| serde_json::json!({"id":format!("id{i}"), "name":format!("dep-{i}"), "version":"1.2.3"}))
                .collect();
            let mut nodes: Vec<_> = (0..count)
                .map(|i| serde_json::json!({"id":format!("id{i}"), "deps":[]}))
                .collect();
            let deps: Vec<_> = (0..count)
                .map(|i| serde_json::json!({"name":format!("dep{i}"), "pkg":format!("id{i}")}))
                .collect();
            nodes.push(serde_json::json!({"id":"root", "deps":deps}));
            let metadata =
                serde_json::json!({"packages":packages, "resolve":{"root":"root", "nodes":nodes}});
            SUMMARY_VISITS.with(|v| v.set([0; 4]));
            let aliases: Vec<_> = (0..count).map(|i| format!("dep-{i}")).collect();
            let projection =
                direct_dependency_metadata(&metadata, aliases.iter().map(String::as_str)).unwrap();
            let summary = direct_versions(&projection).unwrap();
            let visits = SUMMARY_VISITS.with(|v| v.get());
            assert_eq!(summary.len(), count);
            assert_eq!(visits, [count, count + 1, count, count]);
            eprintln!("rust_summary dependencies={count} visits={visits:?}");
        }
    }

    #[test]
    fn canonical_paths_drop_the_verbatim_prefix_and_stay_absolute() {
        let directory = std::env::temp_dir();
        let path = canonicalize(&directory).unwrap();
        assert!(path.is_absolute(), "{}", path.display());
        assert!(
            !path.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            path.display()
        );
        assert_eq!(
            fs::canonicalize(&path).unwrap(),
            fs::canonicalize(&directory).unwrap()
        );
        let output = canonical_output_path(&directory.join("missing").join("cache")).unwrap();
        assert!(output.starts_with(&path), "{}", output.display());
    }

    #[test]
    fn streaming_hash_matches_bytes_across_buffer_boundaries() {
        let path = std::env::temp_dir().join(format!("willow-bridge-hash-{}", std::process::id()));
        for size in [0, 1, 8191, 8192, 8193, 32768] {
            let bytes = vec![42; size];
            fs::write(&path, &bytes).unwrap();
            assert_eq!(hash_file(&path).unwrap(), hash(&bytes));
        }
        fs::remove_file(path).unwrap();
    }
}
