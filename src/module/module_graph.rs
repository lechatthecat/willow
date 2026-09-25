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
    pub const ENTRY: Self = Self(0);
    pub fn file_id(self) -> FileId {
        FileId(self.0)
    }
    pub fn role(self) -> UnitRole {
        if self == Self::ENTRY {
            UnitRole::Entry
        } else {
            UnitRole::Imported
        }
    }
}

pub type UnitId = ModuleId;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnitRole {
    Entry,
    Imported,
}

/// A logical path inside a package; consumer aliases are not part of it.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ModulePath(pub String);

/// Session identity. PackageId is deliberately never serialized.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModuleKey {
    pub package: crate::package::PackageId,
    pub path: ModulePath,
}

impl ModuleKey {
    pub fn new(package: crate::package::PackageId, path: impl Into<String>) -> Self {
        Self {
            package,
            path: ModulePath(path.into()),
        }
    }
}

#[derive(Debug, Default)]
struct Dependencies {
    ordered: Vec<ModuleKey>,
    membership: HashSet<ModuleKey>,
}

/// Entry-rooted dependency graph and parsed source cache.
#[derive(Debug, Default)]
pub struct ModuleGraph {
    pub root: PathBuf,
    #[cfg(test)]
    pub(crate) source_loads: usize,
    #[cfg(test)]
    pub(crate) import_routes: usize,
    pub package_graph: Option<std::sync::Arc<crate::package::PackageGraph>>,
    pub entry_path: Option<PathBuf>,
    /// Explicit project mode; a legacy project may have no package graph.
    pub(crate) project_mode: bool,
    /// Dependency-first order, suitable for type registration and codegen.
    pub files: Vec<SourceFile>,
    pub(crate) artifacts: Option<super::artifacts::UnitArtifacts>,
    by_key: HashMap<ModuleKey, ModuleId>,
    resolved: HashSet<ModuleKey>,
    next_module_id: u32,
    dependencies: HashMap<ModuleKey, Dependencies>,
    visiting: Vec<ModuleKey>,
    visiting_positions: HashMap<ModuleKey, usize>,
    #[cfg(test)]
    membership_probes: usize,
    seen_imports: HashSet<ModuleKey>,
    #[cfg(test)]
    key_reservations: usize,
}

impl ModuleGraph {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            ..Self::default()
        }
    }

    // Compatibility adapters for the existing single-package resolver.
    fn local_key(path: &str) -> ModuleKey {
        ModuleKey::new(crate::package::PackageId(0), path)
    }

    pub fn contains(&self, path: &str) -> bool {
        self.contains_key(&Self::local_key(path))
    }

    pub fn contains_key(&self, key: &ModuleKey) -> bool {
        self.resolved.contains(key)
    }

    pub fn module_id(&self, path: &str) -> Option<ModuleId> {
        self.module_id_for(&Self::local_key(path))
    }

    pub fn module_id_for(&self, key: &ModuleKey) -> Option<ModuleId> {
        self.by_key.get(key).copied()
    }

    pub fn file(&self, id: ModuleId) -> Option<&SourceFile> {
        self.files.iter().find(|file| file.id == id)
    }

    pub fn reserve_module_id(&mut self, path: &str) -> ModuleId {
        self.reserve_key(Self::local_key(path))
    }

    pub fn reserve_key(&mut self, key: ModuleKey) -> ModuleId {
        #[cfg(test)]
        {
            self.key_reservations += 1;
        }
        use std::collections::hash_map::Entry;
        match self.by_key.entry(key) {
            Entry::Occupied(entry) => *entry.get(),
            Entry::Vacant(entry) => {
                self.next_module_id = self
                    .next_module_id
                    .checked_add(1)
                    .expect("module identity exhausted");
                *entry.insert(ModuleId(self.next_module_id))
            }
        }
    }

    pub fn dependencies(&self, path: &str) -> Vec<&str> {
        self.dependencies_for(&Self::local_key(path))
            .iter()
            .map(|key| key.path.0.as_str())
            .collect()
    }

    pub fn dependencies_for(&self, key: &ModuleKey) -> &[ModuleKey] {
        self.dependencies
            .get(key)
            .map(|dependencies| dependencies.ordered.as_slice())
            .unwrap_or(&[])
    }

    pub fn mark_import_seen(&mut self, path: &str) -> bool {
        self.mark_key_seen(Self::local_key(path))
    }

    pub fn mark_key_seen(&mut self, key: ModuleKey) -> bool {
        self.seen_imports.insert(key)
    }

    pub fn begin_visit(&mut self, path: &str) -> Result<(), Vec<String>> {
        self.begin_key_visit(&Self::local_key(path))
            .map_err(|cycle| cycle.into_iter().map(|key| key.path.0).collect())
    }

    pub fn begin_key_visit(&mut self, key: &ModuleKey) -> Result<(), Vec<ModuleKey>> {
        #[cfg(test)]
        {
            self.membership_probes += 1;
        }
        if let Some(&start) = self.visiting_positions.get(key) {
            let mut cycle = self.visiting[start..].to_vec();
            cycle.push(key.clone());
            return Err(cycle);
        }
        self.visiting_positions
            .insert(key.clone(), self.visiting.len());
        self.visiting.push(key.clone());
        Ok(())
    }

    pub fn end_visit(&mut self, path: &str) {
        self.end_key_visit(&Self::local_key(path));
    }

    pub fn end_key_visit(&mut self, key: &ModuleKey) {
        debug_assert_eq!(self.visiting.last(), Some(key));
        if let Some(key) = self.visiting.pop() {
            self.visiting_positions.remove(&key);
        }
    }

    pub fn add_dependency(&mut self, module: &str, dependency: &str) {
        self.add_key_dependency(&Self::local_key(module), &Self::local_key(dependency));
    }

    pub fn add_key_dependency(&mut self, module: &ModuleKey, dependency: &ModuleKey) {
        #[cfg(test)]
        {
            self.membership_probes += 1;
        }
        let dependencies = self.dependencies.entry(module.clone()).or_default();
        if dependencies.membership.insert(dependency.clone()) {
            dependencies.ordered.push(dependency.clone());
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
        self.add_package_file(
            name,
            Self::local_key(&canonical_path),
            path,
            source,
            program,
        )
    }

    pub fn add_package_file(
        &mut self,
        name: String,
        key: ModuleKey,
        path: PathBuf,
        source: String,
        program: crate::parser::ast::Program,
    ) -> ModuleId {
        if self.contains_key(&key) {
            return self.module_id_for(&key).expect("resolved module id");
        }
        let id = self.reserve_key(key.clone());
        self.resolved.insert(key.clone());
        self.files.push(SourceFile {
            id,
            package: key.package,
            symbol_module: self.package_graph.as_ref().map(|graph| {
                crate::semantic::ids::SymbolModule::new(
                    graph
                        .get(key.package)
                        .expect("resolved package")
                        .identity
                        .clone(),
                    key.path.clone(),
                )
            }),
            name,
            canonical_path: key.path.0,
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

    fn empty_program() -> crate::parser::ast::Program {
        crate::parser::ast::Program {
            module: None,
            imports: vec![],
            items: vec![],
        }
    }

    #[test]
    fn package_keys_isolate_same_paths_and_deduplicate_aliases() {
        let mut graph = ModuleGraph::default();
        let a = ModuleKey::new(crate::package::PackageId(0), "util::format");
        let b = ModuleKey::new(crate::package::PackageId(1), "util::format");
        let reserved = graph.reserve_key(a.clone());
        let first = graph.add_package_file(
            "first_alias".into(),
            a.clone(),
            "a.wi".into(),
            String::new(),
            empty_program(),
        );
        let second = graph.add_package_file(
            "second_alias".into(),
            a.clone(),
            "a.wi".into(),
            String::new(),
            empty_program(),
        );
        let other = graph.add_package_file(
            "first_alias".into(),
            b.clone(),
            "b.wi".into(),
            String::new(),
            empty_program(),
        );
        assert_eq!(reserved, first);
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(graph.files.len(), 2);
        assert_eq!(graph.file(first).unwrap().module_key(), a);
        assert_eq!(graph.file(other).unwrap().module_key(), b);
        assert_eq!(graph.module_id("util::format"), Some(first));
        assert_eq!(graph.file(first).unwrap().access_name(), "first_alias");
        assert!(graph.mark_key_seen(a.clone()));
        assert!(graph.mark_key_seen(b.clone()));
        assert!(!graph.mark_key_seen(a.clone()));
        graph.begin_key_visit(&a).unwrap();
        graph.begin_key_visit(&b).unwrap();
        assert_eq!(
            graph.begin_key_visit(&a),
            Err(vec![a.clone(), b.clone(), a.clone()])
        );
        graph.end_key_visit(&b);
        graph.end_key_visit(&a);
        graph.add_key_dependency(&a, &b);
        graph.add_key_dependency(&a, &b);
        graph.add_key_dependency(&b, &a);
        assert_eq!(graph.dependencies_for(&a), std::slice::from_ref(&b));
        assert_eq!(graph.dependencies_for(&b), &[a]);
    }

    #[test]
    fn package_key_work_scales_with_nodes_and_edges_without_package_scans() {
        for size in [16, 64, 256, 1024] {
            for package_count in [1, 8, size] {
                let mut graph = ModuleGraph::default();
                let keys: Vec<_> = (0..size)
                    .map(|i| {
                        ModuleKey::new(
                            crate::package::PackageId((i % package_count) as u32),
                            format!("module_{}", i / package_count),
                        )
                    })
                    .collect();
                for key in &keys {
                    let id = graph.reserve_key(key.clone());
                    assert_eq!(graph.reserve_key(key.clone()), id);
                    graph.begin_key_visit(key).unwrap();
                    graph.add_key_dependency(&keys[0], key);
                    graph.add_key_dependency(&keys[0], key);
                }
                for key in keys.iter().rev() {
                    graph.end_key_visit(key);
                }
                assert_eq!(graph.key_reservations, 2 * size);
                assert_eq!(graph.by_key.len(), size);
                assert_eq!(graph.next_module_id as usize, size);
                assert_eq!(graph.dependencies_for(&keys[0]).len(), size);
                assert_eq!(graph.membership_probes, 3 * size);
                eprintln!(
                    "modules={size} packages={package_count} reservations={} membership_probes={}",
                    2 * size,
                    graph.membership_probes
                );
            }
        }
    }

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
