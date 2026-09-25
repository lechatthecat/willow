use std::path::{Path, PathBuf};

use crate::project::ProjectManifest;

use super::{PackageIdentity, PackageSourceIdentity};

#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("cache_missing_offline: {0}")]
    CacheMissingOffline(String),
    #[error("cache_checksum_mismatch: {0}")]
    CacheChecksumMismatch(String),
    #[error("package_cache: {0}")]
    Cache(#[from] std::io::Error),
    #[error("source_unreachable: {url}: {message}")]
    SourceUnreachable { url: String, message: String },
    #[error("git_revision_not_found: {url}: {revision}")]
    GitRevisionNotFound { url: String, revision: String },
    #[error("version_not_found: {url}: {requirement}")]
    VersionNotFound { url: String, requirement: String },
    #[error("version_conflict: {url}: {requirements:?}")]
    VersionConflict {
        url: String,
        requirements: Vec<ResolutionRequirement>,
    },
    #[error("tag_manifest_version_mismatch: {tag}: manifest version {version}")]
    TagManifestVersionMismatch { tag: String, version: String },
    #[error("git_materialization_failed: {0}")]
    GitMaterialization(String),
    #[error("package_not_found: {path}: {source}")]
    NotFound {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("package_not_directory: {0}")]
    NotDirectory(PathBuf),
    #[error("package_manifest_missing: {0}")]
    ManifestMissing(PathBuf),
    #[error("package_manifest_invalid: {path}: {source}")]
    ManifestInvalid {
        path: PathBuf,
        source: anyhow::Error,
    },
    #[error("not a Willow package: {0}; dependency requires [willow] manifest-version = 1")]
    NotWillowPackage(PathBuf),
    #[error("package_source_missing: expected directory {0}")]
    SourceMissing(PathBuf),
    #[error("package_path_escape: {path} escapes {root}")]
    PathEscape { root: PathBuf, path: PathBuf },
    #[error("package_dependency_cycle: {0:?}")]
    Cycle(Vec<PathBuf>),
    #[error("package_source_unsupported: Git dependency `{alias}` in {root}")]
    UnsupportedSource { root: PathBuf, alias: String },
    #[error("package_version_unavailable: {0}")]
    VersionUnavailable(String),
    #[error("package_revision_mismatch: {0}")]
    RevisionMismatch(PathBuf),
    #[error("package_count_overflow")]
    TooManyPackages,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResolutionRequirement {
    pub required_by: String,
    pub requirement: String,
}

/// A source exposes candidate versions, selects a package identity, then
/// materializes its directory. Git/cache implementations can implement the same
/// interface without changing graph identities or manifest syntax.
pub trait PackageSource {
    type Selector;

    fn versions(&self) -> Result<Vec<semver::Version>, PackageError>;
    fn resolve(&self, selector: &Self::Selector) -> Result<PackageIdentity, PackageError>;
    fn materialize(&self, revision: &PackageIdentity) -> Result<PathBuf, PackageError>;
}

#[derive(Debug)]
pub struct PathSource {
    pub(crate) root: PathBuf,
    pub(crate) manifest: ProjectManifest,
    version: semver::Version,
}

pub(crate) fn canonical_root(path: &Path) -> Result<PathBuf, PackageError> {
    let root = std::fs::canonicalize(path).map_err(|source| PackageError::NotFound {
        path: path.into(),
        source,
    })?;
    if !root.is_dir() {
        return Err(PackageError::NotDirectory(root));
    }
    Ok(root)
}

pub(crate) fn contained_path(root: &Path, path: &Path) -> Result<PathBuf, PackageError> {
    // Reject lexical escapes even when the target does not exist yet.
    let relative = path
        .strip_prefix(root)
        .map_err(|_| PackageError::PathEscape {
            root: root.into(),
            path: path.into(),
        })?;
    let mut depth = 0usize;
    for component in relative.components() {
        match component {
            std::path::Component::Normal(_) => depth += 1,
            std::path::Component::ParentDir if depth != 0 => depth -= 1,
            std::path::Component::CurDir => {}
            _ => {
                return Err(PackageError::PathEscape {
                    root: root.into(),
                    path: path.into(),
                });
            }
        }
    }
    let resolved = std::fs::canonicalize(path).map_err(|source| PackageError::NotFound {
        path: path.into(),
        source,
    })?;
    if !resolved.starts_with(root) {
        return Err(PackageError::PathEscape {
            root: root.into(),
            path: resolved,
        });
    }
    Ok(resolved)
}

impl PathSource {
    /// The entry package may use a legacy manifest. Dependencies are strict.
    pub fn open(path: &Path, dependency: bool) -> Result<Self, PackageError> {
        Self::open_canonical(canonical_root(path)?, dependency)
    }

    pub(crate) fn open_canonical(root: PathBuf, dependency: bool) -> Result<Self, PackageError> {
        let manifest_path = root.join("project.toml");
        if !manifest_path.is_file() {
            return Err(PackageError::ManifestMissing(manifest_path));
        }
        let manifest_path = contained_path(&root, &manifest_path)?;
        let manifest = ProjectManifest::load(&manifest_path).map_err(|source| {
            PackageError::ManifestInvalid {
                path: manifest_path,
                source,
            }
        })?;
        Self::validate_layout(&root, &manifest, dependency)?;
        // ProjectManifest has already validated this version.
        let version = semver::Version::parse(&manifest.project.version)
            .map_err(|error| PackageError::VersionUnavailable(error.to_string()))?;
        Ok(Self {
            root,
            manifest,
            version,
        })
    }

    fn validate_layout(
        root: &Path,
        manifest: &ProjectManifest,
        dependency: bool,
    ) -> Result<(), PackageError> {
        if dependency && manifest.willow.is_none() {
            return Err(PackageError::NotWillowPackage(root.to_path_buf()));
        }
        if dependency || manifest.willow.is_some() || !manifest.dependencies.is_empty() {
            let source_root =
                contained_path(root, &root.join("src")).map_err(|error| match error {
                    PackageError::NotFound { .. } => PackageError::SourceMissing(root.join("src")),
                    other => other,
                })?;
            if !source_root.is_dir() {
                return Err(PackageError::SourceMissing(source_root));
            }
            if let Some(entry) = &manifest.project.entry {
                contained_path(root, &root.join(entry))?;
            }
        }
        Ok(())
    }

    pub(crate) fn replace_manifest(
        &mut self,
        manifest: ProjectManifest,
    ) -> Result<(), PackageError> {
        Self::validate_layout(&self.root, &manifest, false)?;
        self.manifest = manifest;
        Ok(())
    }

    pub(crate) fn identity(&self) -> PackageIdentity {
        PackageIdentity {
            name: self.manifest.project.name.clone(),
            version: self.manifest.project.version.clone(),
            source: PackageSourceIdentity::Path {
                path: self.root.clone(),
            },
            revision: None,
        }
    }
}

impl PackageSource for PathSource {
    type Selector = semver::VersionReq;

    fn versions(&self) -> Result<Vec<semver::Version>, PackageError> {
        Ok(vec![self.version.clone()])
    }

    fn resolve(&self, selector: &Self::Selector) -> Result<PackageIdentity, PackageError> {
        if !selector.matches(&self.version) {
            return Err(PackageError::VersionUnavailable(selector.to_string()));
        }
        Ok(self.identity())
    }

    fn materialize(&self, revision: &PackageIdentity) -> Result<PathBuf, PackageError> {
        if *revision != self.identity() {
            return Err(PackageError::RevisionMismatch(self.root.clone()));
        }
        Ok(self.root.clone())
    }
}
