//! Consumer-local import routing. This does not load/parse modules or mutate the
//! package graph; the module loader owns those operations.
use std::collections::HashMap;
use std::path::PathBuf;

use crate::module::{ModuleKey, std_registry};

use super::{PackageError, PackageGraph, PackageId};

#[derive(Debug, thiserror::Error)]
pub enum PackageImportError {
    #[error(
        "dependency_alias_conflict: dependency alias {alias} conflicts with local module namespace {alias} in {root}"
    )]
    AliasConflict { alias: String, root: PathBuf },
    #[error(
        "package_module_not_found: dependency alias `{alias}` requires an internal module path"
    )]
    ModuleRequired { alias: String },
    #[error("package_module_not_found: `{path}` in {root}")]
    ModuleNotFound { path: String, root: PathBuf },
    #[error("package_module_not_found: invalid import path `{0}`")]
    InvalidPath(String),
    #[error("unknown consumer package {0:?}")]
    UnknownPackage(PackageId),
    #[error(transparent)]
    Source(#[from] PackageError),
}

#[derive(Debug, PartialEq, Eq)]
pub enum PackageImport {
    /// Builtins are validated by the standard-library resolver, never the FS.
    Std,
    Module {
        key: ModuleKey,
        file: PathBuf,
        /// Present only when the last segment names an item of the module.
        item: Option<String>,
    },
}

/// Build once per session, then reuse for every importing unit. Alias lookup
/// never scans package edges and never inherits a parent's dependencies.
pub struct PackageImports<'a> {
    graph: &'a PackageGraph,
    aliases: Vec<HashMap<&'a str, PackageId>>,
    #[cfg(test)]
    alias_lookups: std::cell::Cell<usize>,
    #[cfg(test)]
    file_probes: std::cell::Cell<usize>,
}

impl<'a> PackageImports<'a> {
    pub fn new(graph: &'a PackageGraph) -> Result<Self, PackageImportError> {
        let mut aliases = Vec::with_capacity(graph.packages.len());
        for package in &graph.packages {
            let root = package.source_root();
            let mut local = HashMap::with_capacity(package.dependencies.len());
            for dependency in &package.dependencies {
                // A dangling symlink is still a namespace claim. Do not let
                // broken local paths silently change alias precedence.
                for path in [
                    root.join(format!("{}.wi", dependency.alias)),
                    root.join(&dependency.alias),
                ] {
                    match std::fs::symlink_metadata(&path) {
                        Ok(_) => {
                            return Err(PackageImportError::AliasConflict {
                                alias: dependency.alias.clone(),
                                root: root.clone(),
                            });
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(source) => return Err(PackageError::NotFound { path, source }.into()),
                    }
                }
                local.insert(dependency.alias.as_str(), dependency.package);
            }
            aliases.push(local);
        }
        Ok(Self {
            graph,
            aliases,
            #[cfg(test)]
            alias_lookups: Default::default(),
            #[cfg(test)]
            file_probes: Default::default(),
        })
    }

    pub fn resolve(
        &self,
        consumer: PackageId,
        path: &str,
    ) -> Result<PackageImport, PackageImportError> {
        let current = self
            .graph
            .get(consumer)
            .ok_or(PackageImportError::UnknownPackage(consumer))?;
        // Source imports cannot spell filesystem components. Validate here as
        // this API can also be called without passing through the parser.
        if path.split("::").any(|segment| {
            segment.is_empty()
                || segment
                    .chars()
                    .any(|c| matches!(c, '/' | '\\' | ':' | '.' | '\0') || c.is_whitespace())
        }) {
            return Err(PackageImportError::InvalidPath(path.into()));
        }
        if std_registry::is_std_path(path) {
            return Ok(PackageImport::Std);
        }
        let (first, rest) = path.split_once("::").unwrap_or((path, ""));
        #[cfg(test)]
        self.alias_lookups.set(self.alias_lookups.get() + 1);
        let (target, logical) = match self.aliases[consumer.0 as usize].get(first) {
            Some(&id) => {
                if rest.is_empty() {
                    return Err(PackageImportError::ModuleRequired {
                        alias: first.into(),
                    });
                }
                (
                    self.graph
                        .get(id)
                        .ok_or(PackageImportError::UnknownPackage(id))?,
                    rest,
                )
            }
            None => (current, path),
        };
        if let Some(file) = self.find_file(target, logical)? {
            return Ok(PackageImport::Module {
                key: ModuleKey::new(target.id, logical),
                file,
                item: None,
            });
        }
        // Module-first precedence matches the legacy module loader. Stripping
        // the alias above prevents treating an alias itself as an item parent.
        if let Some((parent, item)) = logical.rsplit_once("::")
            && let Some(file) = self.find_file(target, parent)?
        {
            return Ok(PackageImport::Module {
                key: ModuleKey::new(target.id, parent),
                file,
                item: Some(item.into()),
            });
        }
        Err(PackageImportError::ModuleNotFound {
            path: path.into(),
            root: target.source_root(),
        })
    }

    fn find_file(
        &self,
        package: &super::ResolvedPackage,
        logical: &str,
    ) -> Result<Option<PathBuf>, PackageImportError> {
        let relative: PathBuf = logical.split("::").collect();
        for candidate in [relative.with_extension("wi"), relative.join("mod.wi")] {
            #[cfg(test)]
            self.file_probes.set(self.file_probes.get() + 1);
            match package.source_file(&candidate) {
                Ok(file) if file.is_file() => return Ok(Some(file)),
                Ok(_) => {}
                Err(PackageError::NotFound { source, .. })
                    if matches!(
                        source.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests;
