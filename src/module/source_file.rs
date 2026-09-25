use std::path::PathBuf;

use crate::parser::ast::Program;

use super::module_graph::ModuleId;

/// Parsed source file cached in a [`super::module_graph::ModuleGraph`].
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub id: ModuleId,
    pub package: crate::package::PackageId,
    pub symbol_module: Option<crate::semantic::ids::SymbolModule>,
    /// Name used to access the module from the entry file (possibly an alias).
    pub name: String,
    /// Package-local `::`-separated path (legacy adapter).
    pub canonical_path: String,
    pub path: PathBuf,
    pub source: String,
    pub program: Program,
}

impl SourceFile {
    pub fn identity_path(&self) -> &str {
        self.symbol_module
            .map_or(&self.canonical_path, |origin| origin.namespace())
    }

    pub fn registration_name(&self) -> &str {
        self.symbol_module
            .map_or(&self.name, |origin| origin.namespace())
    }

    pub fn module_key(&self) -> super::ModuleKey {
        super::ModuleKey::new(self.package, self.canonical_path.clone())
    }

    pub fn module_path(&self) -> &str {
        &self.canonical_path
    }

    /// First importing unit's access spelling, never an identity component.
    pub fn access_name(&self) -> &str {
        &self.name
    }
}
