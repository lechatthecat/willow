//! Explicit native artifacts. Cargo continues to own the Rust dependency graph.
//! Resolution produces a validated, ordered link plan; it does not schedule builds.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub mod distribution;
mod graph;
mod payload;
#[cfg(test)]
mod tests;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct NativeManifest {
    pub roots: Vec<String>,
    #[serde(default = "yes")]
    pub reproducible: bool,
    pub libraries: BTreeMap<String, Library>,
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Library {
    pub name: String,
    pub version: String,
    pub license: String,
    pub provenance: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    pub targets: BTreeMap<String, Artifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct Artifact {
    pub source: Source,
    pub digest: String,
    pub architecture: String,
    pub abi: String,
    pub abi_version: String,
    pub linkage: Linkage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader: Option<willow_abi::native::LoaderPolicy>,
    pub inputs: Vec<LinkInput>,
    #[serde(default)]
    pub header_roots: Vec<String>,
    #[serde(default)]
    pub adapter_roots: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Source {
    Local {
        path: String,
    },
    /// A pinned raw object archive, never an implicitly extracted tar/zip.
    Artifact {
        url: String,
    },
    System {
        provider: String,
        version: String,
    },
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Linkage {
    Static,
    Shared,
    System,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "kebab-case",
    deny_unknown_fields
)]
pub enum LinkInput {
    File(String),
    Search(String),
    System(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
pub struct NativeLock {
    pub schema: u32,
    pub target: String,
    pub linker: String,
    pub manifest: NativeManifest,
    pub identities: BTreeMap<String, String>,
}
#[derive(Debug, Default, Clone, Copy)]
pub struct Work {
    pub vertices: usize,
    pub edges: usize,
    pub bytes: u64,
    pub inputs: usize,
    pub payloads: usize,
}
#[derive(Debug)]
pub struct NativePlan {
    pub lock: NativeLock,
    pub args: Vec<String>,
    pub shared: Vec<distribution::SharedArtifact>,
    pub notices: Vec<String>,
    pub work: Work,
}

pub fn digest_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        h.update(&buffer[..n]);
    }
    Ok(format!("{:x}", crate::hex::Hex(&h.finalize())))
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", crate::hex::Hex(&Sha256::digest(bytes)))
}
fn relative(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty() && !path.contains(['\\', ':']) && !path.starts_with('/'),
        "native_path_escape: {path}"
    );
    ensure!(
        Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir)),
        "native_path_escape: {path}"
    );
    Ok(())
}
fn rooted(root: &Path, path: &str) -> Result<PathBuf> {
    relative(path)?;
    let root = root.canonicalize()?;
    let joined = root.join(path).canonicalize()?;
    ensure!(joined.starts_with(root), "native_path_escape: {path}");
    Ok(joined)
}

/// Hash a directory Merkle tree. Names, entry kinds and content digests are
/// framed; directory identity is independent of its absolute location.
pub fn digest_tree(root: &Path) -> Result<String> {
    hash_tree(root, &mut HashMap::new(), &mut 0)
}
fn hash_tree(root: &Path, cache: &mut HashMap<PathBuf, String>, bytes: &mut u64) -> Result<String> {
    ensure!(
        std::fs::symlink_metadata(root)?.is_dir(),
        "native_source_not_directory"
    );
    let mut stack = vec![(root.to_path_buf(), false)];
    let mut entries = HashMap::new();
    while let Some((dir, finish)) = stack.pop() {
        if cache.contains_key(&dir) {
            continue;
        }
        if finish {
            let mut h = Sha256::new();
            h.update(b"willow-native-tree-v1");
            let children: Vec<(String, bool, PathBuf)> = entries.remove(&dir).unwrap();
            for (name, directory, path) in children {
                h.update([u8::from(directory)]);
                h.update((name.len() as u64).to_le_bytes());
                h.update(name.as_bytes());
                h.update(cache[&path].as_bytes());
            }
            cache.insert(dir, format!("{:x}", crate::hex::Hex(&h.finalize())));
        } else {
            ensure!(
                std::fs::symlink_metadata(&dir)?.is_dir(),
                "native_source_not_directory"
            );
            let mut children = Vec::new();
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let ty = entry.file_type()?;
                ensure!(
                    ty.is_dir() || ty.is_file(),
                    "native_unsafe_link: {}",
                    entry.path().display()
                );
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("native_non_utf8_path"))?;
                children.push((name, ty.is_dir(), entry.path()));
            }
            children.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            stack.push((dir.clone(), true));
            for (_, directory, path) in &children {
                if *directory {
                    stack.push((path.clone(), false));
                } else if !cache.contains_key(path) {
                    let size = std::fs::metadata(path)?.len();
                    let hash = digest_file(path)?;
                    ensure!(
                        std::fs::metadata(path)?.len() == size,
                        "native_source_changed"
                    );
                    cache.insert(path.clone(), hash);
                    *bytes += size;
                }
            }
            entries.insert(dir, children);
        }
    }
    Ok(cache[root].clone())
}

/// Resolve without publishing locks. Callers publish only after all graphs pass.
pub fn resolve(
    root: &Path,
    manifest: &NativeManifest,
    target: &str,
    offline: bool,
) -> Result<NativePlan> {
    let triple: target_lexicon::Triple = target
        .parse()
        .map_err(|e| anyhow::anyhow!("native_target_invalid: {e}"))?;
    let arch = triple.architecture.to_string();
    ensure!(
        matches!(arch.as_str(), "x86_64" | "aarch64"),
        "native_target_unsupported: {target}"
    );
    let (abi, linker) = if target.ends_with("windows-msvc") {
        ("c-msvc", "msvc")
    } else if target.ends_with("apple-darwin") {
        ("c-darwin", "darwin")
    } else if target.ends_with("linux-gnu") {
        ("c-sysv", "gnu")
    } else {
        bail!("native_target_unsupported: {target}");
    };
    let mut work = Work::default();
    let graph = graph::Graph::new(manifest, &mut work)?;
    let components = graph.components(&mut work);
    for component in &components {
        let cyclic = component.len() > 1 || graph.edges[component[0]].contains(&component[0]);
        if cyclic {
            ensure!(
                linker == "gnu"
                    && component.iter().all(|&i| manifest.libraries[graph.names[i]]
                        .targets
                        .get(target)
                        .is_some_and(|a| a.linkage == Linkage::Static)),
                "UnsupportedLinkCycle: {} ({target})",
                graph.path(component[0])
            );
        }
    }
    let mut identities = BTreeMap::new();
    let mut inputs = Vec::with_capacity(graph.names.len());
    let mut shared_inputs = vec![None; graph.names.len()];
    let mut distribution_names = std::collections::HashSet::new();
    let mut notices = Vec::new();
    let mut symbols: HashMap<String, (usize, payload::Definition)> = HashMap::new();
    let mut versions: HashMap<&str, (usize, &str)> = HashMap::new();
    let mut checked_files = HashMap::new();
    let mut source_digests = HashMap::new();
    let mut fetched_artifacts = HashMap::new();
    let mut shared_exports = HashMap::new();
    for (i, name) in graph.names.iter().enumerate() {
        let lib = &manifest.libraries[*name];
        ensure!(
            !lib.name.is_empty()
                && !lib.version.is_empty()
                && !lib.license.is_empty()
                && !lib.provenance.is_empty(),
            "native_identity_incomplete: {name}"
        );
        if let Some((other, version)) = versions.insert(&lib.name, (i, &lib.version)) {
            ensure!(
                version == lib.version,
                "native_version_conflict: {} and {}",
                graph.path(other),
                graph.path(i)
            );
        }
        let a = lib
            .targets
            .get(target)
            .with_context(|| format!("native_target_missing: {}: {target}", graph.path(i)))?;
        ensure!(
            a.architecture == arch && a.abi == abi && a.abi_version == "1",
            "native_abi_mismatch: {}",
            graph.path(i)
        );
        if a.linkage == Linkage::Shared {
            let loader = a.loader.as_ref().context("native_loader_policy_required")?;
            ensure!(
                !manifest.reproducible,
                "native_non_hermetic: platform loader dependency search requires reproducible = false"
            );
            notices.push(format!(
                "native_non_hermetic: {name}: loader {} {}",
                loader.platform_provider, loader.platform_version
            ));
            ensure!(
                loader.valid() && willow_abi::native::identifier(name),
                "native_loader_policy_invalid"
            );
            ensure!(
                distribution_names.insert(loader.distribution_name.to_ascii_lowercase()),
                "native_distribution_collision"
            );
            ensure!(
                a.inputs.len() == 1 && matches!(a.inputs[0], LinkInput::File(_)),
                "native_shared_requires_one_file"
            );
        } else {
            ensure!(a.loader.is_none(), "native_loader_policy_on_non_shared");
        }
        ensure!(
            a.digest.len() == 64
                && a.digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "native_digest_invalid: {name}"
        );
        let source_root = match &a.source {
            Source::Local { path } => {
                let dir = rooted(root, path)?;
                let actual = hash_tree(&dir, &mut source_digests, &mut work.bytes)?;
                ensure!(
                    actual == a.digest,
                    "native_digest_mismatch: {}",
                    graph.path(i)
                );
                Some(dir)
            }
            Source::Artifact { url } => {
                ensure!(
                    url.starts_with("https://"),
                    "native_unpinned_source: HTTPS required"
                );
                ensure!(
                    a.header_roots.is_empty() && a.adapter_roots.is_empty(),
                    "native_raw_artifact_roots"
                );
                if !fetched_artifacts.contains_key(&a.digest) {
                    let file = payload::fetch(root, url, &a.digest, offline)?;
                    work.bytes += std::fs::metadata(&file)?.len();
                    source_digests.insert(file.clone(), a.digest.clone());
                    fetched_artifacts.insert(a.digest.clone(), file);
                }
                let file = &fetched_artifacts[&a.digest];
                // The single downloaded file is addressed as `artifact`.
                Some(file.parent().unwrap().to_path_buf())
            }
            Source::System { provider, version } => {
                ensure!(!manifest.reproducible, "native_non_hermetic: {name}");
                ensure!(
                    !provider.is_empty()
                        && !version.is_empty()
                        && a.linkage == Linkage::System
                        && a.header_roots.is_empty()
                        && a.adapter_roots.is_empty(),
                    "native_system_identity_invalid"
                );
                ensure!(
                    a.digest == digest(&serde_json::to_vec(&(provider, version, target))?),
                    "native_digest_mismatch: system {name}"
                );
                notices.push(format!(
                    "native_non_hermetic: {name}: {provider} {version} ({target})"
                ));
                None
            }
        };
        if let Some(dir) = &source_root {
            ensure!(
                a.linkage != Linkage::System,
                "native_linkage_mismatch: {name}"
            );
            for path in a.header_roots.iter().chain(&a.adapter_roots) {
                ensure!(
                    rooted(dir, path)?.is_dir(),
                    "native_root_not_directory: {path}"
                );
            }
        }
        let mut args = Vec::new();
        let mut has_payload = false;
        ensure!(!a.inputs.is_empty(), "native_link_inputs_empty: {name}");
        for input in &a.inputs {
            work.inputs += 1;
            match input {
                LinkInput::File(path) => {
                    has_payload = true;
                    let dir = source_root
                        .as_ref()
                        .context("native_system_file_forbidden")?;
                    if matches!(a.source, Source::Artifact { .. }) {
                        ensure!(path == "artifact", "native_raw_artifact_path");
                    }
                    let path = rooted(dir, path)?;
                    if a.linkage == Linkage::Shared {
                        let loader = a.loader.as_ref().unwrap();
                        if !shared_exports.contains_key(&path) {
                            let mut exports = std::collections::HashSet::new();
                            payload::validate(&path, &arch, abi, a.linkage, |symbol, _| {
                                exports.insert(
                                    if abi == "c-darwin" {
                                        symbol.strip_prefix('_').unwrap_or(symbol)
                                    } else {
                                        symbol
                                    }
                                    .to_owned(),
                                );
                                Ok(())
                            })?;
                            shared_exports.insert(path.clone(), exports);
                            work.payloads += 1;
                        }
                        let exports = &shared_exports[&path];
                        ensure!(
                            exports.contains(&loader.abi_symbol)
                                && loader.symbols.iter().all(|s| exports.contains(s)),
                            "native_shared_export_missing: {name}"
                        );
                        let hash = match source_digests.get(&path) {
                            Some(hash) => hash.clone(),
                            None => digest_file(&path)?,
                        };
                        shared_inputs[i] = Some(distribution::SharedArtifact {
                            source: path,
                            library: willow_abi::native::Library {
                                name: (*name).into(),
                                digest: hash,
                                policy: loader.clone(),
                            },
                        });
                        continue;
                    }
                    if let Some((previous, linkage)) = checked_files.get(&path) {
                        ensure!(*linkage == a.linkage, "native_linkage_mismatch: {name}");
                        // Identical payload referenced by two identities must not
                        // silently acquire incompatible ABI metadata.
                        ensure!(
                            *previous == i,
                            "native_duplicate_artifact_identity: {} and {}",
                            graph.path(*previous),
                            graph.path(i)
                        );
                    } else {
                        work.payloads += 1;
                        payload::validate(&path, &arch, abi, a.linkage, |symbol, definition| {
                            if let Some((other, previous)) = symbols.get(symbol) {
                                ensure!(
                                    previous.compatible(&definition),
                                    "native_symbol_conflict: {symbol}: {} and {}",
                                    graph.path(*other),
                                    graph.path(i)
                                );
                            } else {
                                symbols.insert(symbol.to_owned(), (i, definition));
                            }
                            Ok(())
                        })?;
                        checked_files.insert(path.clone(), (i, a.linkage));
                    }
                    args.push(path.to_str().context("native_non_utf8_path")?.to_owned());
                }
                LinkInput::Search(path) => {
                    let dir = source_root
                        .as_ref()
                        .context("native_system_search_forbidden")?;
                    let path = rooted(dir, path)?;
                    ensure!(path.is_dir(), "native_search_not_directory");
                    args.push(format!(
                        "{}{}",
                        if linker == "msvc" { "/LIBPATH:" } else { "-L" },
                        path.to_str().context("native_non_utf8_path")?
                    ));
                }
                LinkInput::System(name) => {
                    ensure!(
                        matches!(a.source, Source::System { .. })
                            && !name.is_empty()
                            && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                        "native_ambient_lookup_forbidden"
                    );
                    args.push(if linker == "msvc" {
                        format!("{name}.lib")
                    } else {
                        format!("-l{name}")
                    });
                }
            }
        }
        ensure!(
            a.linkage == Linkage::System || has_payload,
            "native_artifact_missing: {name}"
        );
        identities.insert(
            (*name).to_owned(),
            digest(&serde_json::to_vec(&(lib, target))?),
        );
        inputs.push(args);
    }
    // Shared dependencies load before consumers; static link arguments retain
    // the opposite (consumer-first) ordering needed by archive linkers.
    let mut shared = Vec::new();
    for component in components.iter().rev() {
        for &i in component {
            if let Some(artifact) = shared_inputs[i].take() {
                shared.push(artifact);
            }
        }
    }
    let mut args = Vec::new();
    for component in components {
        let cyclic = component.len() > 1 || graph.edges[component[0]].contains(&component[0]);
        if cyclic {
            ensure!(
                linker == "gnu"
                    && component
                        .iter()
                        .all(
                            |&i| manifest.libraries[graph.names[i]].targets[target].linkage
                                == Linkage::Static
                        ),
                "UnsupportedLinkCycle: {} ({target})",
                graph.path(component[0])
            );
            args.push("-Wl,--start-group".into());
        }
        for i in component {
            args.append(&mut inputs[i]);
        }
        if cyclic {
            args.push("-Wl,--end-group".into());
        }
    }
    Ok(NativePlan {
        lock: NativeLock {
            schema: 1,
            target: target.into(),
            linker: linker.into(),
            manifest: manifest.clone(),
            identities,
        },
        args,
        shared,
        notices,
        work,
    })
}
