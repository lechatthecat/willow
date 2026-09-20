use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::diagnostics::FileId;

use super::source_file::SourceFile;

/// Stable module identity within one compilation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct ModuleId(pub u32);

impl ModuleId {
    pub fn file_id(self) -> FileId {
        FileId(self.0 + 1)
    }
}

#[derive(Debug, Default)]
struct Dependencies {
    ordered: Vec<String>,
    membership: HashSet<String>,
}

/// Entry-rooted dependency graph and parsed source cache.
#[derive(Debug, Default)]
pub struct ModuleGraph {
    pub root: PathBuf,
    /// Dependency-first order, suitable for type registration and codegen.
    pub files: Vec<SourceFile>,
    pub(crate) artifacts: Option<super::artifacts::UnitArtifacts>,
    by_canonical_path: HashMap<String, ModuleId>,
    resolved: HashSet<String>,
    next_module_id: u32,
    dependencies: HashMap<String, Dependencies>,
    visiting: Vec<String>,
    visiting_positions: HashMap<String, usize>,
    #[cfg(test)]
    membership_probes: usize,
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
            .map(|dependencies| dependencies.ordered.as_slice())
            .unwrap_or(&[])
    }

    pub fn mark_import_seen(&mut self, path: &str) -> bool {
        self.seen_imports.insert(path.to_string())
    }

    pub fn begin_visit(&mut self, canonical_path: &str) -> Result<(), Vec<String>> {
        #[cfg(test)]
        {
            self.membership_probes += 1;
        }
        if let Some(&start) = self.visiting_positions.get(canonical_path) {
            let mut cycle = self.visiting[start..].to_vec();
            cycle.push(canonical_path.to_string());
            return Err(cycle);
        }
        self.visiting_positions
            .insert(canonical_path.to_string(), self.visiting.len());
        self.visiting.push(canonical_path.to_string());
        Ok(())
    }

    pub fn end_visit(&mut self, canonical_path: &str) {
        debug_assert_eq!(
            self.visiting.last().map(String::as_str),
            Some(canonical_path)
        );
        if let Some(path) = self.visiting.pop() {
            self.visiting_positions.remove(&path);
        }
    }

    pub fn add_dependency(&mut self, module: &str, dependency: &str) {
        #[cfg(test)]
        {
            self.membership_probes += 1;
        }
        let dependencies = self.dependencies.entry(module.to_string()).or_default();
        if !dependencies.membership.contains(dependency) {
            dependencies.membership.insert(dependency.to_string());
            dependencies.ordered.push(dependency.to_string());
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

    #[test]
    fn visit_index_preserves_nested_cycles_and_revisit_after_unwind() {
        let mut graph = ModuleGraph::default();
        for name in ["root", "a", "b"] {
            graph.begin_visit(name).unwrap();
        }
        assert_eq!(
            graph.begin_visit("a"),
            Err(vec!["a".into(), "b".into(), "a".into()])
        );
        assert_eq!(graph.visiting.len(), 3);
        for name in ["b", "a"] {
            graph.end_visit(name);
        }
        graph.begin_visit("a").unwrap();
        assert_eq!(graph.begin_visit("a"), Err(vec!["a".into(), "a".into()]));
        graph.end_visit("a");
        graph.end_visit("root");
        assert!(graph.visiting_positions.is_empty());
    }

    #[test]
    fn graph_membership_work_scales_with_visits_and_edges() {
        for size in [16, 64, 256, 1024] {
            let names: Vec<_> = (0..size).map(|i| format!("module_{i}")).collect();
            let mut graph = ModuleGraph::default();
            for name in &names {
                graph.begin_visit(name).unwrap();
            }
            assert_eq!(graph.membership_probes, size);
            assert_eq!(graph.visiting_positions.len(), size);
            for name in names.iter().rev() {
                graph.end_visit(name);
            }
            assert!(graph.visiting_positions.is_empty());

            // Wide fan-out, repeated edges, and diamond-shaped shared dependencies.
            for name in &names {
                graph.add_dependency("root", name);
                graph.add_dependency("root", name);
                graph.add_dependency(name, "shared");
                graph.add_dependency(name, "shared");
            }
            assert_eq!(graph.membership_probes, 5 * size);
            assert_eq!(graph.dependencies("root"), names);
            for name in &names {
                assert_eq!(graph.dependencies(name), ["shared"]);
            }
            let stored_edges: usize = graph
                .dependencies
                .values()
                .map(|dependencies| dependencies.membership.len())
                .sum();
            assert_eq!(stored_edges, 2 * size);
            eprintln!(
                "modules={size} membership_probes={} stored_edges={stored_edges}",
                graph.membership_probes
            );
        }
    }
}
