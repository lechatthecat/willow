//! Source-order dependency index shared by compiler queries.
use super::query::QueryTable;
#[cfg(test)]
use crate::DEPENDENCY_WORK;
use crate::module;
use crate::module::artifacts::UnitArtifacts;

/// A borrowed consumer view: aliases never duplicate the target's module map.
pub(crate) struct ModulePaths<'a> {
    dependencies: &'a ModuleDependencies,
    consumer: crate::package::PackageId,
}

impl ModulePaths<'_> {
    pub(crate) fn get(&self, path: &str) -> Option<&usize> {
        if module::std_registry::is_std_path(path) {
            return None;
        }
        let (first, rest) = path.split_once("::").unwrap_or((path, ""));
        let (package, path) = match self
            .dependencies
            .aliases
            .get(&self.consumer)
            .and_then(|aliases| aliases.get(first))
        {
            Some(package) => (*package, rest),
            None => (self.consumer, path),
        };
        self.dependencies.by_package.get(&package)?.get(path)
    }
}

pub(crate) struct ModuleDependencies {
    by_package: std::collections::HashMap<
        crate::package::PackageId,
        std::collections::HashMap<String, usize>,
    >,
    aliases: std::collections::HashMap<
        crate::package::PackageId,
        std::collections::HashMap<String, crate::package::PackageId>,
    >,
    root_package: crate::package::PackageId,
    units: Vec<module::UnitId>,
    package_by_unit: std::collections::HashMap<module::UnitId, crate::package::PackageId>,
    pub(crate) edges: Vec<Vec<usize>>,
    by_unit: std::collections::HashMap<module::UnitId, usize>,
    closure_queries: QueryTable<module::UnitId, usize>,
}

impl ModuleDependencies {
    /// Source spans retain the consumer unit even after bodies are offloaded.
    /// Entry is absent from the imported modules and uses the session root.
    pub(crate) fn paths_for(&self, program: &crate::parser::ast::Program) -> ModulePaths<'_> {
        self.paths_for_unit(
            program
                .imports
                .first()
                .map_or(module::UnitId::ENTRY, |import| {
                    module::ModuleId(import.span.file_id.0)
                }),
        )
    }

    pub(crate) fn paths_for_unit(&self, unit: module::UnitId) -> ModulePaths<'_> {
        ModulePaths {
            dependencies: self,
            consumer: self
                .package_by_unit
                .get(&unit)
                .copied()
                .unwrap_or(self.root_package),
        }
    }

    pub(crate) fn unit_for_path(
        &self,
        consumer: module::UnitId,
        path: &str,
    ) -> Option<module::UnitId> {
        Some(self.units[*self.paths_for_unit(consumer).get(path)?])
    }

    pub(crate) fn index_for_module(
        &self,
        package: crate::package::PackageId,
        path: &str,
    ) -> Option<usize> {
        self.by_package.get(&package)?.get(path).copied()
    }

    pub(crate) fn index_for_unit(&self, unit: module::UnitId) -> Option<usize> {
        self.by_unit.get(&unit).copied()
    }

    pub(crate) fn direct(&self, unit: module::UnitId) -> Option<&[usize]> {
        self.by_unit
            .get(&unit)
            .map(|&index| self.edges[index].as_slice())
    }

    pub(crate) fn unit_closure(
        &self,
        unit: module::UnitId,
        artifacts: &UnitArtifacts,
    ) -> anyhow::Result<Vec<usize>> {
        let artifact = self.closure_queries.query(unit, || {
            let roots = self
                .direct(unit)
                .ok_or_else(|| anyhow::anyhow!("unknown dependency unit: {unit:?}"))?;
            artifacts.write(&self.reachable(roots.iter().copied()))
        })?;
        artifacts.read(*artifact)
    }

    #[cfg(test)]
    pub(crate) fn new(modules: &[module::ResolvedModule]) -> Self {
        Self::with_packages(modules, None)
    }

    pub(crate) fn with_packages(
        modules: &[module::ResolvedModule],
        packages: Option<&crate::package::PackageGraph>,
    ) -> Self {
        let mut by_package: std::collections::HashMap<_, std::collections::HashMap<_, _>> =
            Default::default();
        for (index, module) in modules.iter().enumerate() {
            by_package
                .entry(module.package)
                .or_default()
                .insert(module.canonical_path.clone(), index);
        }
        let mut result = Self {
            by_package,
            aliases: packages
                .into_iter()
                .flat_map(|graph| &graph.packages)
                .map(|package| {
                    (
                        package.id,
                        package
                            .dependencies
                            .iter()
                            .map(|dep| (dep.alias.clone(), dep.package))
                            .collect(),
                    )
                })
                .collect(),
            units: modules.iter().map(|module| module.id).collect(),
            root_package: packages.map_or(crate::package::PackageId(0), |graph| graph.root),
            package_by_unit: modules.iter().map(|m| (m.id, m.package)).collect(),
            edges: Vec::new(),
            by_unit: modules.iter().enumerate().map(|(i, m)| (m.id, i)).collect(),
            closure_queries: QueryTable::named("module_dependency_closure"),
        };
        result.edges = modules
            .iter()
            .map(|module| {
                let by_path = ModulePaths {
                    dependencies: &result,
                    consumer: module.package,
                };
                let mut seen = std::collections::HashSet::new();
                module
                    .program
                    .imports
                    .iter()
                    .filter_map(|import| {
                        #[cfg(test)]
                        DEPENDENCY_WORK.with(|work| {
                            let (lookups, visits) = work.get();
                            work.set((lookups + 1, visits));
                        });
                        let id = by_path.get(&import.path).copied().or_else(|| {
                            let (path, _) = import.path.rsplit_once("::")?;
                            #[cfg(test)]
                            DEPENDENCY_WORK.with(|work| {
                                let (lookups, visits) = work.get();
                                work.set((lookups + 1, visits));
                            });
                            by_path.get(path).copied()
                        })?;
                        seen.insert(id).then_some(id)
                    })
                    .collect()
            })
            .collect();
        result
    }

    /// Sparse closure: unrelated units do not allocate entries or get scanned.
    /// Sorted indices retain the resolver's declaration/dependency order.
    pub(crate) fn reachable(&self, roots: impl Iterator<Item = usize>) -> Vec<usize> {
        let mut seen = std::collections::HashSet::new();
        let mut pending = Vec::new();
        for id in roots {
            if seen.insert(id) {
                pending.push(id);
            }
        }
        while let Some(id) = pending.pop() {
            for &dependency in &self.edges[id] {
                #[cfg(test)]
                DEPENDENCY_WORK.with(|work| {
                    let (lookups, visits) = work.get();
                    work.set((lookups, visits + 1));
                });
                if seen.insert(dependency) {
                    pending.push(dependency);
                }
            }
        }
        let mut result: Vec<_> = seen.into_iter().collect();
        result.sort_unstable();
        result
    }

    #[cfg(test)]
    pub(crate) fn closure(&self, roots: impl Iterator<Item = usize>) -> Vec<bool> {
        let mut mask = vec![false; self.edges.len()];
        for id in self.reachable(roots) {
            mask[id] = true;
        }
        mask
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packages(edges: &[Vec<(&str, u32)>]) -> crate::package::PackageGraph {
        use crate::package::*;
        PackageGraph {
            root: PackageId(0),
            packages: edges
                .iter()
                .enumerate()
                .map(|(i, edges)| {
                    let root = std::path::PathBuf::from(format!("p{i}"));
                    ResolvedPackage {
                        checksum: None,
                        id: PackageId(i as u32),
                        identity: PackageIdentity {
                            name: format!("p{i}"),
                            version: "1.0.0".into(),
                            source: PackageSourceIdentity::Path { path: root.clone() },
                            revision: None,
                        },
                        root,
                        dependencies: edges
                            .iter()
                            .map(|(alias, p)| ResolvedDependency {
                                selector: None,
                                alias: (*alias).into(),
                                package: PackageId(*p),
                            })
                            .collect(),
                    }
                })
                .collect(),
            stats: Default::default(),
        }
    }

    fn add(graph: &mut module::ModuleGraph, package: u32, path: &str, source: &str) {
        let key = module::ModuleKey::new(crate::package::PackageId(package), path);
        let id = graph.reserve_key(key.clone());
        let tokens = crate::lexer::Lexer::with_file_id(source, id.file_id())
            .tokenize()
            .unwrap();
        let (program, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty(), "{errors:?}");
        graph.add_package_file(path.into(), key, path.into(), String::new(), program);
    }

    #[test]
    fn consumer_aliases_preserve_identity_visibility_and_module_precedence() {
        let packages = packages(&[
            vec![("a", 1), ("same", 1), ("b", 2)],
            vec![("b", 3)],
            vec![],
            vec![],
        ]);
        let mut graph = module::ModuleGraph::default();
        add(&mut graph, 0, "util", "");
        add(&mut graph, 1, "util", "import b::util; import b::util::f;");
        add(&mut graph, 2, "util", "");
        add(&mut graph, 3, "util", "");
        add(&mut graph, 1, "util::f", "");
        add(
            &mut graph,
            0,
            "consumer",
            "import a::util; import same::util; import a::util::f; import b::util::f;",
        );
        let index = ModuleDependencies::with_packages(&graph.files, Some(&packages));
        let root = index.paths_for(&graph.files[5].program);
        for (path, expected) in [
            ("util", 0),
            ("a::util", 1),
            ("same::util", 1),
            ("b::util", 2),
            ("a::util::f", 4),
        ] {
            assert_eq!(root.get(path), Some(&expected), "{path}");
        }
        for path in [
            "a",
            "b",
            "p3::util",
            "missing::util",
            "std::io",
            "b::util::f",
        ] {
            assert_eq!(root.get(path), None, "{path}");
        }
        let dependency = index.paths_for(&graph.files[1].program);
        assert_eq!(dependency.get("b::util"), Some(&3));
        assert_eq!(dependency.get("util"), Some(&1));
        assert_eq!(dependency.get("a::util"), None);
        assert_eq!(dependency.get("same::util"), None);
        assert_eq!(index.edges[1], [3]);
        assert_eq!(index.edges[5], [1, 4, 2]);
        assert_eq!(index.reachable([5].into_iter()), [1, 2, 3, 4, 5]);
        // Entry spans do not occur among imported units. Respect the graph root.
        let mut packages = packages;
        packages.root = crate::package::PackageId(1);
        let index = ModuleDependencies::with_packages(&graph.files, Some(&packages));
        let tokens = crate::lexer::Lexer::new("import b::util;")
            .tokenize()
            .unwrap();
        let (entry, errors) = crate::parser::Parser::new(tokens).parse();
        assert!(errors.is_empty());
        assert_eq!(index.paths_for(&entry).get("b::util"), Some(&3));
    }

    #[test]
    fn package_dependency_index_storage_and_work_are_linear() {
        for size in [16, 64, 256, 1024] {
            for chain in [false, true] {
                let edges: Vec<_> = (0..size)
                    .map(|i| {
                        if i == 0 {
                            vec![]
                        } else {
                            let target = if chain { i - 1 } else { 0 };
                            vec![("dep", target), ("again", target)]
                        }
                    })
                    .collect();
                let packages = packages(&edges);
                let mut graph = module::ModuleGraph::default();
                for i in 0..size {
                    add(
                        &mut graph,
                        i,
                        "util",
                        if i == 0 {
                            ""
                        } else {
                            "import dep::util; import again::util; import dep::util::f; import again::util::f;"
                        },
                    );
                }
                DEPENDENCY_WORK.with(|work| work.set((0, 0)));
                let index = ModuleDependencies::with_packages(&graph.files, Some(&packages));
                assert_eq!(
                    index.by_package.values().map(|m| m.len()).sum::<usize>(),
                    size as usize
                );
                assert_eq!(
                    index.aliases.values().map(|m| m.len()).sum::<usize>(),
                    2 * (size - 1) as usize
                );
                assert_eq!(
                    index.edges.iter().map(|e| e.len()).sum::<usize>(),
                    (size - 1) as usize
                );
                assert_eq!(
                    DEPENDENCY_WORK.with(|work| work.get()),
                    (6 * (size - 1) as usize, 0)
                );
                for i in 1..size as usize {
                    assert_eq!(index.edges[i], [if chain { i - 1 } else { 0 }]);
                }
                eprintln!(
                    "package-index chain={chain} packages={size} modules={size} aliases={} edges={} lookups={}",
                    2 * (size - 1),
                    size - 1,
                    6 * (size - 1)
                );
            }
        }
    }

    #[test]
    fn many_aliases_share_target_module_storage() {
        for modules in [16, 64, 256, 1024] {
            for aliases in [1, 8, modules] {
                let mut packages = packages(&[vec![], vec![], vec![]]);
                packages.packages[0].dependencies = (0..aliases)
                    .map(|i| crate::package::ResolvedDependency {
                        selector: None,
                        alias: format!("alias{i}"),
                        package: crate::package::PackageId(1),
                    })
                    .collect();
                let mut graph = module::ModuleGraph::default();
                for i in 0..modules {
                    add(&mut graph, 1, &format!("m{i}"), "");
                }
                let index = ModuleDependencies::with_packages(&graph.files, Some(&packages));
                assert_eq!(
                    index.by_package.values().map(|m| m.len()).sum::<usize>(),
                    modules
                );
                assert_eq!(
                    index.aliases.values().map(|m| m.len()).sum::<usize>(),
                    aliases
                );
                assert_eq!(index.by_package.len(), 1); // unused packages load no modules
                let paths = ModulePaths {
                    dependencies: &index,
                    consumer: packages.root,
                };
                for i in 0..aliases {
                    for _ in 0..8 {
                        assert_eq!(
                            paths.get(&format!("alias{i}::m{}", modules - 1)),
                            Some(&(modules - 1))
                        );
                    }
                }
                assert_eq!(
                    index.by_package.values().map(|m| m.len()).sum::<usize>(),
                    modules
                );
                eprintln!(
                    "shared-target modules={modules} aliases={aliases} stored_paths={modules} requests={}",
                    8 * aliases
                );
            }
        }
    }

    #[test]
    fn dependency_edges_and_import_lookups_are_package_local() {
        let mut graph = module::ModuleGraph::default();
        for package in [crate::package::PackageId(0), crate::package::PackageId(1)] {
            for (path, source) in [
                ("util", "pub fn format() {}"),
                ("consumer", "import util as u; import util::format as f;"),
            ] {
                let key = module::ModuleKey::new(package, path);
                let id = graph.reserve_key(key.clone());
                let tokens = crate::lexer::Lexer::with_file_id(source, id.file_id())
                    .tokenize()
                    .unwrap();
                let (program, errors) = crate::parser::Parser::new(tokens).parse();
                assert!(errors.is_empty());
                graph.add_package_file(path.into(), key, path.into(), String::new(), program);
            }
        }
        let index = ModuleDependencies::new(&graph.files);
        assert_eq!(index.edges, [vec![], vec![0], vec![], vec![2]]);
        for (consumer, expected) in [(1, 0), (3, 2)] {
            assert_eq!(
                index.paths_for(&graph.files[consumer].program).get("util"),
                Some(&expected)
            );
            assert_eq!(
                index.direct(graph.files[consumer].id),
                Some([expected].as_slice())
            );
        }
    }

    #[test]
    fn sparse_dependency_artifacts_scale_with_reachable_output() {
        for size in [16, 64, 256, 1024] {
            for chain in [false, true] {
                let index = ModuleDependencies {
                    by_package: Default::default(),
                    aliases: Default::default(),
                    root_package: crate::package::PackageId(0),
                    units: (0..size).map(|i| module::ModuleId(i as u32)).collect(),
                    package_by_unit: Default::default(),
                    edges: (0..size)
                        .map(|i| {
                            if i == 0 {
                                vec![]
                            } else {
                                vec![if chain { i - 1 } else { 0 }]
                            }
                        })
                        .collect(),
                    by_unit: (0..size).map(|i| (module::ModuleId(i as u32), i)).collect(),
                    closure_queries: QueryTable::named("module_dependency_closure"),
                };
                let artifacts = UnitArtifacts::new().unwrap();
                let mut entries = 0;
                for i in 0..size {
                    let unit = module::ModuleId(i as u32);
                    let result = index.unit_closure(unit, &artifacts).unwrap();
                    let count = if chain { i } else { usize::from(i > 0) };
                    assert_eq!(result.len(), count);
                    assert!(result.windows(2).all(|pair| pair[0] < pair[1]));
                    entries += result.len();
                    for _ in 0..3 {
                        assert_eq!(index.unit_closure(unit, &artifacts).unwrap(), result);
                    }
                }
                assert_eq!(
                    entries,
                    if chain {
                        size * (size - 1) / 2
                    } else {
                        size - 1
                    }
                );
                assert_eq!(index.closure_queries.stats().computations, size);
                assert_eq!(index.closure_queries.stats().hits, size * 3);
                eprintln!(
                    "dependency shape={} units={size} stored_entries={entries} computations={size}",
                    if chain { "chain" } else { "fanout" }
                );
            }
        }
    }
}
