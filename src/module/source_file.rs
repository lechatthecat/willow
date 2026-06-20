use std::path::PathBuf;

use crate::parser::ast::Program;

use super::module_graph::ModuleId;

/// Parsed source file cached in a [`super::module_graph::ModuleGraph`].
#[derive(Debug, Clone)]
pub struct SourceFile {
    pub id: ModuleId,
    /// Name used to access the module from the entry file (possibly an alias).
    pub name: String,
    /// Canonical `::`-separated module identity.
    pub canonical_path: String,
    pub path: PathBuf,
    pub source: String,
    pub program: Program,
}
