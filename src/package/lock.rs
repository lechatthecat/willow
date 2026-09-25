//! Root-owned lock policy. Path sources remain live; Git sources use exact commits.
#[cfg(test)]
use super::resolve_path_packages;
use super::{PackageGraph, PackageSourceIdentity, PathSource};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Lock {
    #[serde(rename = "lock-version")]
    version: u32,
    root: Root,
    #[serde(rename = "package")]
    packages: Vec<Package>,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Root {
    package: String,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Package {
    id: String,
    name: String,
    version: String,
    source: Source,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checksum: Option<String>,
    dependencies: Vec<Dependency>,
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Source {
    Path { path: String },
    Git { url: String },
    GitSubdirectory { url: String, path: String },
}
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Dependency {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    selector: Option<String>,
    alias: String,
    package: String,
}

// Both arguments are canonical absolute paths. Reject different filesystem
// prefixes instead of leaking an absolute machine-specific path into the lock.
fn relative_path(base: &Path, target: &Path) -> Result<String> {
    let base: Vec<_> = base.components().collect();
    let target: Vec<_> = target.components().collect();
    anyhow::ensure!(
        base.first() == target.first(),
        "lockfile_path_unrepresentable"
    );
    let shared = base.iter().zip(&target).take_while(|(a, b)| a == b).count();
    let mut parts = vec![".."; base.len() - shared];
    for component in &target[shared..] {
        parts.push(
            component
                .as_os_str()
                .to_str()
                .context("lockfile_path_not_utf8")?,
        );
    }
    Ok(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}

impl Lock {
    fn canonical_text(mut self) -> Result<String> {
        self.packages.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        for p in &mut self.packages {
            p.dependencies
                .sort_unstable_by(|a, b| a.alias.cmp(&b.alias));
        }
        Ok(toml::to_string_pretty(&self)?)
    }

    // Index borrowed keys once: a valid lock needs no sorting or nested scans.
    fn matches(&self, expected: &Self) -> bool {
        use std::collections::HashMap;
        if self.version != expected.version
            || self.root != expected.root
            || self.packages.len() != expected.packages.len()
        {
            return false;
        }
        let packages: HashMap<_, _> = self.packages.iter().map(|p| (&p.id, p)).collect();
        if packages.len() != self.packages.len() {
            return false;
        }
        expected.packages.iter().all(|p| {
            #[cfg(test)]
            VALIDATION_VISITS.with(|counts| {
                let (v, e) = counts.get();
                counts.set((v + 1, e));
            });
            let Some(actual) = packages.get(&p.id) else {
                return false;
            };
            if actual.name != p.name
                || actual.version != p.version
                || actual.source != p.source
                || actual.revision != p.revision
                || actual.checksum != p.checksum
                || actual.dependencies.len() != p.dependencies.len()
            {
                return false;
            }
            let dependencies: HashMap<_, _> = actual
                .dependencies
                .iter()
                .map(|d| (&d.alias, (&d.package, &d.selector)))
                .collect();
            dependencies.len() == actual.dependencies.len()
                && p.dependencies.iter().all(|d| {
                    #[cfg(test)]
                    VALIDATION_VISITS.with(|counts| {
                        let (v, e) = counts.get();
                        counts.set((v, e + 1));
                    });
                    dependencies.get(&d.alias) == Some(&(&d.package, &d.selector))
                })
        })
    }

    fn pins(&self) -> std::collections::HashMap<String, super::solve::GitPin> {
        let mut pins: std::collections::HashMap<_, _> = self
            .packages
            .iter()
            .filter_map(|p| {
                let Source::Git { url } = &p.source else {
                    return None;
                };
                Some((
                    url.clone(),
                    super::solve::GitPin {
                        identity: super::PackageIdentity {
                            name: p.name.clone(),
                            version: p.version.clone(),
                            source: PackageSourceIdentity::Git { url: url.clone() },
                            revision: p.revision.clone(),
                        },
                        selectors: Default::default(),
                    },
                ))
            })
            .collect();
        let urls: std::collections::HashMap<_, _> = self
            .packages
            .iter()
            .filter_map(|p| {
                let Source::Git { url } = &p.source else {
                    return None;
                };
                Some((p.id.as_str(), url))
            })
            .collect();
        for dependency in self.packages.iter().flat_map(|p| &p.dependencies) {
            if let Some(url) = urls.get(dependency.package.as_str())
                && let Some(selector) = &dependency.selector
            {
                pins.get_mut(*url)
                    .unwrap()
                    .selectors
                    .insert(selector.clone());
            }
        }
        pins
    }

    fn from_graph(graph: &PackageGraph) -> Result<Self> {
        let root = &graph.get(graph.root).context("invalid root package")?.root;
        let ids: Vec<_> = graph
            .packages
            .iter()
            .map(|p| match &p.identity.source {
                PackageSourceIdentity::Path { path } => {
                    Ok(format!("path:{}", relative_path(root, path)?))
                }
                PackageSourceIdentity::Git { url } => Ok(format!("git:{url}")),
                PackageSourceIdentity::GitSubdirectory { url, path } => {
                    Ok(format!("git-path:{}", serde_json::to_string(&(url, path))?))
                }
            })
            .collect::<Result<_>>()?;
        let mut packages = Vec::with_capacity(ids.len());
        for p in &graph.packages {
            let dependencies: Vec<_> = p
                .dependencies
                .iter()
                .map(|d| Dependency {
                    selector: d.selector.clone(),
                    alias: d.alias.clone(),
                    package: ids[d.package.0 as usize].clone(),
                })
                .collect();
            let source = match &p.identity.source {
                PackageSourceIdentity::Path { .. } => Source::Path {
                    path: ids[p.id.0 as usize][5..].into(),
                },
                PackageSourceIdentity::Git { url } => Source::Git { url: url.clone() },
                PackageSourceIdentity::GitSubdirectory { url, path } => Source::GitSubdirectory {
                    url: url.clone(),
                    path: path.clone(),
                },
            };
            packages.push(Package {
                id: ids[p.id.0 as usize].clone(),
                name: p.identity.name.clone(),
                version: p.identity.version.clone(),
                source,
                revision: p.identity.revision.clone(),
                checksum: p.checksum.clone(),
                dependencies,
            });
        }
        Ok(Self {
            version: 1,
            root: Root {
                package: ids[graph.root.0 as usize].clone(),
            },
            packages,
        })
    }
}

/// Validate all live path manifests once. The resulting graph is reused by the
/// compiler, so lock validation does not cause a second dependency traversal.
/// Only the selected root lock is read. A matching lock is never rewritten.
pub fn resolve_locked_path_packages(root: &Path, locked: bool) -> Result<PackageGraph> {
    resolve_source(PathSource::open(root, false)?, locked, false)
}

/// Resolve and cache dependencies without invoking the compiler or runtime builder.
pub fn fetch_packages(root: &Path, locked: bool, offline: bool) -> Result<PackageGraph> {
    resolve_source(PathSource::open(root, false)?, locked, offline)
}

pub(crate) fn resolve_project_packages(
    root: &Path,
    locked: bool,
    offline: bool,
) -> Result<Option<PackageGraph>> {
    let source = PathSource::open(root, false)?;
    let package_mode = source.manifest.willow.is_some() || !source.manifest.dependencies.is_empty();
    let graph = resolve_source(source, locked, offline)?;
    Ok(package_mode.then_some(graph))
}

pub(super) fn update(root: &Path) -> Result<PackageGraph> {
    let graph = super::resolve_packages(root)?;
    let path = graph.get(graph.root).unwrap().root.join("project.lock");
    atomic_write(
        &path,
        Lock::from_graph(&graph)?.canonical_text()?.as_bytes(),
        || Ok(()),
    )?;
    Ok(graph)
}

fn resolve_source(source: PathSource, locked: bool, offline: bool) -> Result<PackageGraph> {
    let path = source.root.join("project.lock");
    let previous = match std::fs::read_to_string(&path) {
        Ok(text) => Some(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).context("cannot read project.lock"),
    };
    anyhow::ensure!(
        !locked || previous.is_some(),
        "lockfile_missing: {}",
        path.display()
    );
    let current = previous
        .as_deref()
        .and_then(|text| toml::from_str::<Lock>(text).ok());
    let mut pins = std::collections::HashMap::new();
    let mut checksums = std::collections::HashMap::new();
    if let Some(lock) = &current
        && lock.version == 1
    {
        pins = lock.pins();
        for package in &lock.packages {
            if let Source::Git { url } = &package.source
                && let Some(checksum) = &package.checksum
            {
                checksums.insert(url.clone(), checksum.clone());
            }
        }
    }
    let graph = match super::solve::resolve_source(source, pins, checksums, offline, !locked) {
        Ok(graph) => graph,
        Err(
            error @ (super::PackageError::CacheMissingOffline(_)
            | super::PackageError::CacheChecksumMismatch(_)),
        ) => return Err(error.into()),
        Err(error) if locked => {
            anyhow::bail!("lockfile_stale: cannot validate locked graph: {error}")
        }
        Err(error) => return Err(error.into()),
    };
    let expected = Lock::from_graph(&graph)?;
    if !current
        .as_ref()
        .is_some_and(|current| current.matches(&expected))
    {
        anyhow::ensure!(!locked, "lockfile_stale: {}", path.display());
        let text = expected.canonical_text()?;
        atomic_write(&path, text.as_bytes(), || Ok(()))?;
    }
    Ok(graph)
}

struct Temporary(PathBuf);
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

// A complete validated temporary is persisted before replacement. Manifest is
// never mutated here: an interrupted manifest edit leaves a detectable stale
// lock that ordinary build can regenerate and --locked will reject.
fn atomic_write(
    path: &Path,
    bytes: &[u8],
    before_rename: impl FnOnce() -> Result<()>,
) -> Result<()> {
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (guard, mut file) = loop {
        let seq = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = path.with_file_name(format!(".project.lock.{}.{seq}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
        {
            Ok(file) => break (Temporary(temp), file),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    };
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    let text = std::fs::read_to_string(&guard.0)?;
    let lock: Lock = toml::from_str(&text)?;
    anyhow::ensure!(lock.version == 1, "unsupported_lock_version");
    before_rename()?;
    std::fs::rename(&guard.0, path)?;
    Ok(())
}

#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! { static VALIDATION_VISITS: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) }; }

/// Read pins without materializing sources or modifying the lock.
pub(super) fn command_pins(
    root: &Path,
    update: Option<Option<&str>>,
    refresh_versions: bool,
) -> Result<(
    std::collections::HashMap<String, super::solve::GitPin>,
    std::collections::HashMap<String, String>,
)> {
    let text = match std::fs::read_to_string(root.join("project.lock")) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(e) => return Err(e.into()),
    };
    let lock: Lock =
        toml::from_str(&text).context("invalid project.lock; run fetch to repair it")?;
    anyhow::ensure!(lock.version == 1, "unsupported_lock_version");
    // Adding a constraint re-solves version sources within their requirements.
    // Moving references remain locked: only an explicit update follows them.
    let fixed: std::collections::HashSet<_> = if refresh_versions {
        lock.packages
            .iter()
            .flat_map(|p| &p.dependencies)
            .filter(|d| {
                d.selector
                    .as_ref()
                    .is_some_and(|s| !s.starts_with("version:"))
            })
            .map(|d| d.package.as_str())
            .collect()
    } else {
        Default::default()
    };
    let mut unlocked = std::collections::HashSet::new();
    if let Some(alias) = update {
        if let Some(alias) = alias {
            let by_id: std::collections::HashMap<_, _> =
                lock.packages.iter().map(|p| (p.id.as_str(), p)).collect();
            let mut pending: Vec<_> = by_id
                .get(lock.root.package.as_str())
                .into_iter()
                .flat_map(|p| &p.dependencies)
                .filter(|d| d.alias == alias)
                .map(|d| d.package.as_str())
                .collect();
            while let Some(id) = pending.pop() {
                #[cfg(test)]
                COMMAND_UNLOCK_VISITS.with(|n| n.set(n.get() + 1));
                if unlocked.insert(id)
                    && let Some(package) = by_id.get(id)
                {
                    pending.extend(package.dependencies.iter().map(|d| d.package.as_str()));
                }
            }
        } else {
            unlocked.extend(lock.packages.iter().map(|p| p.id.as_str()));
        }
    }
    let mut pins = lock.pins();
    let mut checksums = std::collections::HashMap::new();
    for package in &lock.packages {
        if unlocked.contains(package.id.as_str())
            || (refresh_versions && !fixed.contains(package.id.as_str()))
        {
            if let Source::Git { url } = &package.source {
                pins.remove(url);
            }
            continue;
        }

        if let Source::Git { url } = &package.source
            && let Some(checksum) = &package.checksum
        {
            checksums.insert(url.clone(), checksum.clone());
        }
    }
    Ok((pins, checksums))
}

pub(super) fn command_lock(graph: &PackageGraph) -> Result<String> {
    Lock::from_graph(graph)?.canonical_text()
}

pub(super) fn write_command_lock(path: &Path, text: &str) -> Result<()> {
    atomic_write(path, text.as_bytes(), || Ok(()))
}

#[cfg(test)]
thread_local! { static COMMAND_UNLOCK_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
