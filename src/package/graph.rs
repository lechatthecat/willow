use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::PackageError;

/// Compact index valid only within its owning graph. Deliberately not serializable.
///
/// ```compile_fail
/// use willow_compiler::package::PackageGraph;
/// fn export(graph: &PackageGraph) {
///     serde_json::to_string(&graph.root).unwrap();
/// }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackageId(pub(crate) u32);

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PackageSourceIdentity {
    Path {
        path: PathBuf,
    },
    Git {
        url: String,
    },
    /// A path dependency contained in an immutable Git snapshot.
    GitSubdirectory {
        url: String,
        path: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PackageIdentity {
    pub name: String,
    pub version: String,
    pub source: PackageSourceIdentity,
    pub revision: Option<String>,
}

#[derive(Debug)]
pub struct ResolvedDependency {
    /// Normalized Git selector retained for lock staleness checks.
    pub selector: Option<String>,
    pub alias: String,
    pub package: PackageId,
}

#[derive(Debug)]
pub struct ResolvedPackage {
    /// SHA256 of the complete materialized Git tree; local paths stay live.
    pub checksum: Option<String>,
    pub id: PackageId,
    pub identity: PackageIdentity,
    pub root: PathBuf,
    pub dependencies: Vec<ResolvedDependency>,
}

impl ResolvedPackage {
    /// Logical source root is always `src/`, independent of the entry setting.
    pub fn source_root(&self) -> PathBuf {
        self.root.join("src")
    }

    /// Resolve a source-root-relative file and check the physical target before
    /// handing it to the compiler. Callers must not bypass this for symlinks.
    pub fn source_file(&self, relative: &Path) -> Result<PathBuf, PackageError> {
        super::source::contained_path(&self.root, &self.source_root().join(relative))
    }
}

#[derive(Debug)]
pub struct PackageGraph {
    pub root: PackageId,
    pub packages: Vec<ResolvedPackage>,
    pub stats: super::ResolutionStats,
}

impl PackageGraph {
    pub fn get(&self, id: PackageId) -> Option<&ResolvedPackage> {
        self.packages.get(id.0 as usize)
    }
}
