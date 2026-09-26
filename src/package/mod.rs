//! Session-local package resolution. Module loading and build scheduling remain
//! separate from this graph; persistent consumers use identities, never IDs.

mod cache;
mod git;
mod graph;
mod solve;
pub use git::{GitBackend, GitSource, SystemGit};
pub use solve::{resolve_packages, update_packages};
mod imports;
mod lock;
pub(crate) use lock::resolve_project_packages;
pub use lock::{fetch_packages, resolve_locked_path_packages};
mod resolver;
mod source;

pub use graph::{
    PackageGraph, PackageId, PackageIdentity, PackageSourceIdentity, ResolvedDependency,
    ResolvedPackage,
};
pub use imports::{PackageImport, PackageImportError, PackageImports};
pub use resolver::{ResolutionStats, resolve_path_packages};
pub use source::{PackageError, PackageSource, PathSource, ResolutionRequirement};

#[cfg(test)]
mod tests;

#[cfg(test)]
mod git_tests;

mod verify;
pub use verify::{Verification, verify_package};

mod commands;
pub use commands::{
    MutationReport, PackageMutation, display_dependencies, inspect_packages, mutate_packages,
    mutate_packages_report,
};

mod output;
pub use output::{Metadata, MetadataDependency, MetadataPackage, PackageDelta};
mod errors;
pub use errors::{CommandError, package_error_json};
