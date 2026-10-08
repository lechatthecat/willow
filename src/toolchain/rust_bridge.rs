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
    /// Parent of the per-project hidden project; defaults to ~/.willow/build.
    pub cache_root: PathBuf,
    pub mode: BuildMode,
    /// None means rustc's host. Future target-aware callers supply a triple.
    pub target: Option<String>,
    pub offline: bool,
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
            cache_root: PathBuf::from(home).join(".willow/build"),
            mode,
            target: None,
            offline: false,
            cargo: "cargo".into(),
            rustc: "rustc".into(),
            wrapper_schema: "1".into(),
            abi_revision: "1".into(),
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
    pub directory: PathBuf,
    pub cache_key: String,
    pub target: String,
    pub staticlib: Option<PathBuf>,
    pub messages: CargoMessages,
    /// Cargo metadata includes exact package versions and resolved graph.
    pub metadata: Value,
    // Hold until caller finishes consuming/linking the artifact. Another Willow
    // process must not replace the stable Cargo staticlib alias during linking.
    _lease: File,
}

impl BridgeBuild {
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
    if project.rust_dependencies.is_empty() {
        return Ok(None);
    }
    let root = fs::canonicalize(root)?;
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
    let directory = fs::canonicalize(directory)?;
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
    fs::create_dir_all(directory.join("src"))?;
    let manifest = generated_manifest(project, &root)?;
    let manifest_path = directory.join("Cargo.toml");
    let lib_path = directory.join("src/lib.rs");
    write_changed(&manifest_path, manifest.as_bytes())?;
    let source = format!(
        "#[path = {:?}]\npub mod bridge;\n",
        bridge.to_str().context("non-UTF8 bridge path")?
    );
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
    let mut metadata_command = command();
    metadata_command
        .args(["metadata", "--format-version=1", "--manifest-path"])
        .arg(&manifest_path)
        .arg("--filter-platform")
        .arg(target);
    if options.offline {
        metadata_command.arg("--offline");
    }
    let metadata: Value = serde_json::from_slice(&run(&mut metadata_command)?.stdout)?;
    let lock_hash = hash_file(&directory.join("Cargo.lock"))?;
    let source_hash = hash_file(&bridge)?;
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
    if !check {
        cargo.args(["--", "--print=native-static-libs"]);
    }
    let output = cargo
        .output()
        .context("cargo_missing: cannot execute Cargo")?;
    let messages = parse_cargo_messages(&output.stdout, &manifest_path)?;
    if !output.status.success() || !messages.success {
        bail!(
            "rust_bridge_build_failed: {}\n{}",
            serde_json::to_string(&messages.diagnostics)?,
            String::from_utf8_lossy(&output.stderr)
        );
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
        _lease: lease,
    }))
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
                let path = fs::canonicalize(root.join(path))?;
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
    let mut resolved = fs::canonicalize(existing)?;
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
