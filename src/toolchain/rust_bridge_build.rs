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
pub mod review;

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
    pub symbols: Vec<crate::rust_bridge_ir::RustBridgeSymbol>,
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
            wrapper_schema: crate::rust_bridge_ir::WRAPPER_SCHEMA.into(),
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
    pub review_fingerprint: String,
    pub legacy_review_fingerprint: String,
    pub target: String,
    pub staticlib: Option<PathBuf>,
    pub messages: CargoMessages,
    /// Cargo metadata includes exact package versions and resolved graph.
    pub metadata: Value,
    pub direct_dependencies: Value,
    // Hold until caller finishes consuming/linking the artifact. Another Willow
    // process must not replace the stable Cargo staticlib alias during linking.
    _lease: File,
    _project_lease: Option<crate::workspace_transaction::WorkspaceFileTransaction>,
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
    let transaction = crate::workspace_transaction::WorkspaceFileTransaction::open(root)?;
    transaction.ensure_no_ai_edit()?;
    let project = ProjectManifest::load(&root.join("project.toml"))?;
    anyhow::ensure!(
        crate::package::lock::rust_input_hash(
            &project,
            &options.wrapper_schema,
            &options.abi_revision
        )? == resolution.bridge_input_hash,
        "project changed before Rust publication; retry the command"
    );
    transaction.publish(resolution_changes(
        root, directory, options, resolution, project,
    )?)
}

fn resolution_changes(
    root: &Path,
    directory: &Path,
    options: &BridgeOptions,
    resolution: &crate::package::lock::RustLock,
    project: ProjectManifest,
) -> Result<Vec<crate::workspace_transaction::FileChange>> {
    use crate::workspace_transaction::{FileChange, read_optional};
    let bytes = fs::read(directory.join("Cargo.lock"))?;
    let _: toml::Table = toml::from_str(std::str::from_utf8(&bytes)?)?;
    anyhow::ensure!(
        hash(&bytes) == resolution.cargo_lock_hash,
        "Cargo candidate lock changed"
    );
    let summary =
        (resolution.modules || !resolution.dependencies.is_empty()).then(|| resolution.clone());
    let lock = crate::package::lock::candidate_rust_lock(root, project, summary, options.offline)?;
    Ok(vec![
        FileChange::new(
            "project.lock",
            read_optional(&root.join("project.lock"))?,
            Some(lock.into_bytes()),
        ),
        FileChange::new(
            ".willow/rust/Cargo.lock",
            read_optional(&root.join(".willow/rust/Cargo.lock"))?,
            Some(bytes),
        ),
    ])
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

fn prepare_bridge_lock(
    root: &Path,
    directory: &Path,
    options: &BridgeOptions,
    action: BridgeAction<'_>,
    input_hash: &str,
) -> Result<Option<crate::package::lock::RustLock>> {
    let persisted = root.join(".willow/rust/Cargo.lock");
    let previous = crate::package::lock::read_rust_lock(root)
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
    Ok(previous)
}

struct BridgeCargo<'a> {
    root: &'a Path,
    toolchain: &'a RustToolchain,
    directory: &'a Path,
    manifest_path: &'a Path,
    lib_path: &'a Path,
    target: &'a str,
    profile: &'a str,
}

impl BridgeCargo<'_> {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.toolchain.cargo);
        cmd.current_dir(self.root)
            .env("RUSTC", &self.toolchain.rustc);
        if let Some(home) = std::env::var_os("WILLOW_CARGO_HOME") {
            cmd.env("CARGO_HOME", home);
        }
        cmd
    }
    fn metadata(&self, options: &BridgeOptions, action: BridgeAction<'_>) -> Result<Value> {
        if let BridgeAction::Edit {
            update: Some(package),
            ..
        } = action
        {
            let mut update = self.command();
            update
                .args(["update", "--manifest-path"])
                .arg(self.manifest_path);
            if let Some(package) = package {
                update.args(["--package", package]);
            }
            if options.offline {
                update.arg("--offline");
            }
            run(&mut update)?;
        }
        let mut metadata_command = self.command();
        metadata_command
            .args(["metadata", "--format-version=1", "--manifest-path"])
            .arg(self.manifest_path)
            .arg("--filter-platform")
            .arg(self.target);
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
        Ok(serde_json::from_slice(&metadata_output.stdout)?)
    }
    fn compile(
        &self,
        options: &BridgeOptions,
        check: bool,
    ) -> Result<(CargoMessages, Option<PathBuf>)> {
        let mut cargo = self.command();
        cargo
            .arg(if check { "check" } else { "rustc" })
            .args(["--lib", "--message-format=json", "--manifest-path"])
            .arg(self.manifest_path)
            .arg("--target-dir")
            .arg(self.directory.join("target"))
            .arg("--target")
            .arg(self.target)
            .arg("--profile")
            .arg(self.profile);
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
        let messages = parse_cargo_messages(&output.stdout, self.manifest_path)?;
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
                                            path == self.lib_path || path == Path::new("src/lib.rs")
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
                if adapter_error(&["E0308", "E0277", "E0283"]) {
                    "Rust bridge compilation failed: byte adapters use Array<u8> for &[u8] / Vec<u8>; Array<i64> now maps to Vec<i64>. Migrate byte declarations to Array<u8> and use checked_to_u8() for integer values"
                } else {
                    "Rust bridge compilation failed"
                },
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
                            == Some(self.lib_path)
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
        Ok((messages, staticlib))
    }
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
    let has_native_modules = options.symbols.iter().any(|symbol| symbol.native.is_some());
    if project.rust_dependencies.is_empty()
        && !has_native_modules
        && !matches!(action, BridgeAction::Edit { .. })
    {
        return Ok(None);
    }
    if !options.symbols.is_empty()
        && options.abi_revision != willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION.to_string()
    {
        bail!("rust_bridge_abi_mismatch: incompatible adapter revision");
    }
    let root = canonicalize(root)?;
    let project_lease = if options.read_only {
        crate::workspace_transaction::WorkspaceFileTransaction::existing(&root)?
    } else {
        Some(crate::workspace_transaction::WorkspaceFileTransaction::open(&root)?)
    };
    if let Some(lease) = &project_lease {
        lease.ensure_no_ai_edit()?;
    }
    let bridge = project
        .rust
        .as_ref()
        .map(|rust| rust.resolve_bridge(&root))
        .transpose()?;
    anyhow::ensure!(
        bridge.is_some() || has_native_modules,
        "rust_bridge_missing: [rust] bridge is required"
    );
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
    if !options.read_only {
        fs::create_dir_all(&rust_directory)?;
        crate::workspace_transaction::sync_directory(&root.join(".willow"))?;
        crate::workspace_transaction::sync_directory(&root)?;
    }
    fs::create_dir_all(directory.join("src"))?;
    let manifest = prepare_manifest(
        project,
        &root,
        &directory,
        has_native_modules && review::required(&root, &options.symbols),
    )?;
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
    let previous = prepare_bridge_lock(&root, &directory, options, action, &input_hash)?;
    write_changed(&manifest_path, manifest.as_bytes())?;
    let (source, native_hash) =
        generated_bridge_source(&root, &directory, bridge.as_deref(), &options.symbols)?;
    // Cargo metadata needs a lib target, but never rewrite a previous fingerprint
    // before determining the new key: that would invalidate every warm build.
    if !lib_path.exists() {
        write_changed(&lib_path, source.as_bytes())?;
    }
    let cargo = BridgeCargo {
        root: &root,
        toolchain: &toolchain,
        directory: &directory,
        manifest_path: &manifest_path,
        lib_path: &lib_path,
        target,
        profile,
    };
    let metadata = cargo.metadata(options, action)?;
    let lock_hash = hash_file(&directory.join("Cargo.lock"))?;
    let direct_dependencies = direct_dependency_metadata(
        &metadata,
        project.rust_dependencies.keys().map(String::as_str),
    )?;
    let rust_lock = crate::package::lock::RustLock {
        modules: has_native_modules || previous.as_ref().is_some_and(|lock| lock.modules),
        cargo_lock_hash: lock_hash.clone(),
        bridge_input_hash: input_hash,
        dependencies: direct_versions(&direct_dependencies)?,
    };
    if options.locked
        && !previous.as_ref().is_some_and(|lock| {
            lock.cargo_lock_hash == rust_lock.cargo_lock_hash
                && lock.bridge_input_hash == rust_lock.bridge_input_hash
                && lock.dependencies == rust_lock.dependencies
        })
    {
        bail!("rust_lockfile_stale: Cargo resolution or direct version summary changed");
    }
    let (bridge_hash, review, legacy_review) = bridge_reviews(
        &root,
        options,
        action,
        bridge.as_deref(),
        native_hash,
        &rust_lock,
    )?;
    let source_hash = hash(&serde_json::to_vec(&(&bridge_hash, &options.symbols))?);
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
            review_fingerprint: review,
            legacy_review_fingerprint: legacy_review,
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
    let (messages, staticlib) = cargo.compile(options, action == BridgeAction::Check)?;
    Ok(Some(BridgeBuild {
        directory,
        cache_key,
        review_fingerprint: review,
        legacy_review_fingerprint: legacy_review,
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

fn prepare_manifest(
    project: &ProjectManifest,
    root: &Path,
    directory: &Path,
    inject_assume: bool,
) -> Result<String> {
    let mut manifest = generated_manifest(project, root)?;
    if inject_assume {
        anyhow::ensure!(
            !project.rust_dependencies.contains_key("willow"),
            "Rust dependency alias `willow` is reserved for the assertion macro"
        );
        let macro_dir = directory.join("willow-assume");
        fs::create_dir_all(macro_dir.join("src"))?;
        write_changed(
            &macro_dir.join("Cargo.toml"),
            include_bytes!("../../crates/willow_assume/Cargo.toml"),
        )?;
        write_changed(
            &macro_dir.join("src/lib.rs"),
            include_bytes!("../../crates/willow_assume/src/lib.rs"),
        )?;
        manifest.push_str(
            "\n[dependencies.willow]\npackage = \"willow_assume\"\npath = \"willow-assume\"\n",
        );
    }
    Ok(manifest)
}

fn bridge_reviews(
    root: &Path,
    options: &BridgeOptions,
    action: BridgeAction<'_>,
    bridge: Option<&Path>,
    native_hash: Option<String>,
    rust_lock: &crate::package::lock::RustLock,
) -> Result<(String, String, String)> {
    let has_native_modules = options.symbols.iter().any(|symbol| symbol.native.is_some());
    // Compute once for all declarations. Source and manifest changes require
    // review even when Cargo's resolved package versions have not changed.
    // Line endings carry no Rust meaning and differ between checkouts (CRLF on
    // Windows), so a reviewed fingerprint stays portable across platforms.
    let native_review = has_native_modules && review::required(root, &options.symbols);
    let legacy_hash = bridge
        .map(|path| {
            if native_review {
                review::reviewed_file_hash(path)
            } else {
                hash_source_file(path)
            }
        })
        .transpose()?;
    // Preserve legacy review semantics while native reviews bind every module.
    let bridge_hash = legacy_hash
        .clone()
        .or(native_hash)
        .context("rust_bridge_missing: bridge source is required")?;
    let legacy_review = hash(&serde_json::to_vec(&(
        &rust_lock.cargo_lock_hash,
        &bridge_hash,
        &rust_lock.bridge_input_hash,
    ))?);
    let review = if native_review {
        review::fingerprint(
            root,
            &options.symbols,
            &rust_lock.cargo_lock_hash,
            &rust_lock.bridge_input_hash,
            legacy_hash.as_deref(),
        )?
    } else {
        legacy_review.clone()
    };
    if !matches!(action, BridgeAction::Metadata | BridgeAction::Edit { .. }) {
        review::validate(root, &options.symbols, &review)?;
        for symbol in &options.symbols {
            if symbol.native.is_none()
                && let Some(declared) = &symbol.nonblocking
                && declared.fingerprint != legacy_review
            {
                bail!(
                    "rust_nonblocking_review_required: {}: review current bridge and dependencies, then use nonblocking(\"{}\")",
                    symbol.willow_function,
                    legacy_review
                );
            }
        }
    }
    Ok((bridge_hash, review, legacy_review))
}

fn generated_bridge_source(
    root: &Path,
    directory: &Path,
    legacy: Option<&Path>,
    symbols: &[crate::rust_bridge_ir::RustBridgeSymbol],
) -> Result<(String, Option<String>)> {
    let (mut source, native_hash) = module_sources(root, legacy, symbols)?;
    if !symbols.is_empty() {
        let abi = directory.join("src/willow_bridge_abi.rs");
        write_changed(&abi, include_bytes!("../../crates/willow_abi/src/ffi.rs"))?;
        source.push_str(&crate::rust_bridge_ir::wrappers(
            symbols,
            &abi,
            willow_abi::ffi::WILLOW_RUST_BRIDGE_ABI_REVISION,
        ));
    }
    Ok((source, native_hash))
}

fn review_module_path(root: &Path, path: &Path) -> String {
    fn portable(path: &Path) -> String {
        let path = path.to_string_lossy().replace('\\', "/");
        if let Some(path) = path.strip_prefix("//?/UNC/") {
            format!("//{path}")
        } else {
            path.strip_prefix("//?/").unwrap_or(&path).to_owned()
        }
    }
    let root = portable(root);
    let path = portable(path);
    path.strip_prefix(&format!("{}/", root.trim_end_matches('/')))
        .unwrap_or(&path)
        .to_owned()
}

/// Fingerprint captured modules without rereading them. Rustc receives original
/// source paths so relative modules, includes and diagnostics retain Rust semantics.
/// Adapters share the existing bridge namespace and therefore the existing ABI.
fn module_sources(
    root: &Path,
    legacy: Option<&Path>,
    symbols: &[crate::rust_bridge_ir::RustBridgeSymbol],
) -> Result<(String, Option<String>)> {
    let mut module_index = std::collections::HashMap::new();
    let mut modules = Vec::new();
    let mut adapters = String::new();
    for native in symbols.iter().filter_map(|symbol| symbol.native.as_ref()) {
        let index = *module_index
            .entry(native.module.as_str())
            .or_insert_with(|| {
                let index = modules.len();
                modules.push((native.module.as_str(), native.source_path.as_path(), None));
                index
            });
        let source = &mut modules[index].2;
        if let Some(captured) = &native.source {
            anyhow::ensure!(source.is_none(), "duplicate captured Rust module source");
            *source = Some(captured.as_str());
        }
        adapters.push_str(&native.adapter);
        adapters.push('\n');
    }
    if modules.is_empty() {
        let legacy = legacy.context("rust_bridge_missing: bridge source is required")?;
        return Ok((
            format!(
                "#[path = {:?}]\npub mod bridge;\n",
                legacy.to_str().context("non-UTF8 bridge path")?
            ),
            None,
        ));
    }
    let mut source = String::new();
    let mut module_hashes = Vec::with_capacity(modules.len());
    for (module, path, captured) in modules {
        let captured = captured.context("Rust module source was not captured")?;
        source.push_str(&format!(
            "#[path = {:?}] pub mod {module};\n",
            path.to_str().context("non-UTF8 Rust module path")?
        ));
        module_hashes.push((
            review_module_path(root, path),
            hash_source_bytes(captured.as_bytes()),
        ));
    }
    if let Some(legacy) = legacy {
        source.push_str(&format!(
            "#[path = {:?}]\npub mod legacy_bridge;\n",
            legacy.to_str().context("non-UTF8 bridge path")?
        ));
    }
    source.push_str("pub mod bridge {\n#[allow(unused_imports, reason = \"Native type adapters share the bridge namespace\")] use crate::bridge;\n");
    if legacy.is_some() {
        source.push_str("pub use crate::legacy_bridge::*;\n");
    }
    source.push_str(&adapters);
    source.push_str("}\n");
    // Generated names contain absolute-path hashes solely for ABI disambiguation.
    // A source review uses project-relative paths and a native adapter schema so
    // moving the checkout does not invalidate a mixed legacy nonblocking review.
    let fingerprint = hash(&serde_json::to_vec(&("native-modules-v1", module_hashes))?);
    Ok((source, Some(fingerprint)))
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
    format!("{:x}", crate::hex::Hex(&Sha256::digest(bytes)))
}

/// `fs::canonicalize` without the Windows `\\?\` verbatim prefix. MSVC's
/// `cl.exe` cannot open a verbatim archive path (LNK1104), and Cargo echoes
/// the `--target-dir` spelling into its artifact paths.
pub(super) fn canonicalize(path: &Path) -> std::io::Result<PathBuf> {
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

fn hash_source_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(hash_source_bytes(&bytes))
}

fn hash_source_bytes(bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    let mut rest = bytes;
    // Feed each CRLF as LF without allocating a normalized copy.
    while let Some(at) = rest.windows(2).position(|pair| pair == b"\r\n") {
        digest.update(&rest[..at]);
        digest.update(b"\n");
        rest = &rest[at + 2..];
    }
    digest.update(rest);
    format!("{:x}", crate::hex::Hex(&digest.finalize()))
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
    Ok(format!("{:x}", crate::hex::Hex(&digest.finalize())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::RustDependencySpec;

    fn native_symbol(
        module: &str,
        source: Option<&str>,
        adapter: &str,
    ) -> crate::rust_bridge_ir::RustBridgeSymbol {
        let mut symbol = crate::rust_bridge_ir::RustBridgeSymbol::new(
            format!("{module}::value"),
            vec![],
            crate::rust_bridge_ir::Scalar::I64,
        );
        symbol.native = Some(crate::rust_module::NativeBridge {
            definitions: vec![],
            module: module.into(),
            source_path: PathBuf::from("/source-must-not-be-read.rs"),
            source: source.map(str::to_owned),
            adapter: adapter.into(),
            target: format!("{module}_value"),
        });
        symbol
    }

    #[test]
    fn native_module_sources_use_captures_and_share_one_crate() {
        let directory =
            std::env::temp_dir().join(format!("willow-native-source-{}", std::process::id()));
        fs::create_dir_all(directory.join("src")).unwrap();
        let mut symbols = vec![
            native_symbol(
                "first",
                Some("pub fn value() -> i64 { 1 }\n"),
                "pub use crate::first::value as first_value;",
            ),
            native_symbol(
                "first",
                None,
                "pub use crate::first::value as another_value;",
            ),
            native_symbol(
                "second",
                Some("pub fn value() -> i64 { 2 }\n"),
                "pub use crate::second::value as second_value;",
            ),
        ];
        let legacy = directory.join("absent-legacy.rs");
        let (source, fingerprint) = module_sources(&directory, Some(&legacy), &symbols).unwrap();
        assert_eq!(source.matches("pub mod first;").count(), 1);
        assert_eq!(source.matches("pub mod second;").count(), 1);
        assert!(source.contains("pub use crate::legacy_bridge::*;"));
        assert!(source.contains("another_value;"));
        assert!(source.contains("/source-must-not-be-read.rs"));
        assert!(!directory.join("src/first.rs").exists());
        symbols[0].native.as_mut().unwrap().source = Some("pub fn value() -> i64 { 1 }\r\n".into());
        assert_eq!(
            module_sources(&directory, Some(&legacy), &symbols)
                .unwrap()
                .1,
            fingerprint
        );
        symbols[0].native.as_mut().unwrap().source = Some("pub fn value() -> i64 { 3 }\n".into());
        assert_ne!(
            module_sources(&directory, Some(&legacy), &symbols)
                .unwrap()
                .1,
            fingerprint
        );
        symbols[0].native.as_mut().unwrap().source = None;
        assert!(module_sources(&directory, None, &symbols).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn native_review_paths_normalize_windows_prefixes_without_io() {
        assert_eq!(
            review_module_path(
                Path::new("C:/work"),
                Path::new(r"\\?\C:\work\src\native.rs")
            ),
            "src/native.rs"
        );
        assert_eq!(
            review_module_path(
                Path::new(r"\\server\share\work"),
                Path::new(r"\\?\UNC\server\share\work\src\native.rs")
            ),
            "src/native.rs"
        );
        assert_eq!(
            review_module_path(Path::new("/work"), Path::new("/work/src/native.rs")),
            "src/native.rs"
        );
        assert_eq!(
            review_module_path(Path::new("/work"), Path::new("/work-other/src/native.rs")),
            "/work-other/src/native.rs"
        );
    }

    #[test]
    fn native_review_fingerprint_ignores_checkout_location() {
        let first_root = Path::new("/checkout-a");
        let second_root = Path::new("/checkout-b");
        let mut first = native_symbol(
            "native_first",
            Some("pub fn value() -> i64 { 42 }"),
            "pub use crate::native_first::value as native_first_value;",
        );
        first.native.as_mut().unwrap().source_path = first_root.join("src/native.rs");
        let mut second = first.clone();
        let native = second.native.as_mut().unwrap();
        native.module = "native_second".into();
        native.source_path = second_root.join("src/native.rs");
        native.adapter = native.adapter.replace("native_first", "native_second");
        let first_hash = module_sources(first_root, None, &[first]).unwrap().1;
        assert_eq!(
            first_hash,
            module_sources(second_root, None, &[second.clone()])
                .unwrap()
                .1
        );
        second.native.as_mut().unwrap().source = Some("pub fn value() -> i64 { 43 }".into());
        assert_ne!(
            first_hash,
            module_sources(second_root, None, &[second]).unwrap().1
        );
    }

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
