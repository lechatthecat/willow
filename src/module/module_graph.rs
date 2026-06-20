use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::diagnostics::FileId;

use super::source_file::SourceFile;

/// Stable module identity within one compilation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleId(pub u32);

impl ModuleId {
    pub fn file_id(self) -> FileId {
        FileId(self.0 + 1)
    }
}

/// Entry-rooted dependency graph and parsed source cache.
#[derive(Debug, Default)]
pub struct ModuleGraph {
    pub root: PathBuf,
    /// Dependency-first order, suitable for type registration and codegen.
    pub files: Vec<SourceFile>,
    by_canonical_path: HashMap<String, ModuleId>,
    resolved: HashSet<String>,
    next_module_id: u32,
    dependencies: HashMap<String, Vec<String>>,
    visiting: Vec<String>,
    seen_imports: HashSet<String>,
}

impl ModuleGraph {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            ..Self::default()
        }
    }

    pub fn contains(&self, canonical_path: &str) -> bool {
        self.resolved.contains(canonical_path)
    }

    pub fn module_id(&self, canonical_path: &str) -> Option<ModuleId> {
        self.by_canonical_path.get(canonical_path).copied()
    }

    pub fn file(&self, id: ModuleId) -> Option<&SourceFile> {
        self.files.iter().find(|file| file.id == id)
    }

    pub fn reserve_module_id(&mut self, canonical_path: &str) -> ModuleId {
        if let Some(id) = self.module_id(canonical_path) {
            return id;
        }
        let id = ModuleId(self.next_module_id);
        self.next_module_id += 1;
        self.by_canonical_path
            .insert(canonical_path.to_string(), id);
        id
    }

    pub fn dependencies(&self, canonical_path: &str) -> &[String] {
        self.dependencies
            .get(canonical_path)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn mark_import_seen(&mut self, path: &str) -> bool {
        self.seen_imports.insert(path.to_string())
    }

    pub fn begin_visit(&mut self, canonical_path: &str) -> Result<(), Vec<String>> {
        if let Some(start) = self
            .visiting
            .iter()
            .position(|visiting| visiting == canonical_path)
        {
            let mut cycle = self.visiting[start..].to_vec();
            cycle.push(canonical_path.to_string());
            return Err(cycle);
        }
        self.visiting.push(canonical_path.to_string());
        Ok(())
    }

    pub fn end_visit(&mut self, canonical_path: &str) {
        debug_assert_eq!(
            self.visiting.last().map(String::as_str),
            Some(canonical_path)
        );
        self.visiting.pop();
    }

    pub fn add_dependency(&mut self, module: &str, dependency: &str) {
        let dependencies = self.dependencies.entry(module.to_string()).or_default();
        if !dependencies.iter().any(|existing| existing == dependency) {
            dependencies.push(dependency.to_string());
        }
    }

    pub fn add_file(
        &mut self,
        name: String,
        canonical_path: String,
        path: PathBuf,
        source: String,
        program: crate::parser::ast::Program,
    ) -> ModuleId {
        if self.contains(&canonical_path) {
            let id = self.module_id(&canonical_path).expect("resolved module id");
            return id;
        }
        let id = self.reserve_module_id(&canonical_path);
        self.resolved.insert(canonical_path.clone());
        self.files.push(SourceFile {
            id,
            name,
            canonical_path,
            path,
            source,
            program,
        });
        id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_path_is_reported_from_first_repeated_module() {
        let mut graph = ModuleGraph::new(PathBuf::from("project"));
        graph.begin_visit("a").unwrap();
        graph.begin_visit("b").unwrap();
        assert_eq!(
            graph.begin_visit("a"),
            Err(vec!["a".into(), "b".into(), "a".into()])
        );
    }

    #[test]
    fn dependencies_are_deduplicated_in_source_order() {
        let mut graph = ModuleGraph::default();
        graph.add_dependency("a", "b");
        graph.add_dependency("a", "b");
        graph.add_dependency("a", "c");
        assert_eq!(graph.dependencies("a"), ["b", "c"]);
    }
}
