//! Source-order dependency index shared by compiler queries.
use super::query::QueryTable;
#[cfg(test)]
use crate::DEPENDENCY_WORK;
use crate::module;
use crate::module::artifacts::UnitArtifacts;

pub(crate) struct ModuleDependencies {
    pub(crate) by_path: std::collections::HashMap<String, usize>,
    pub(crate) edges: Vec<Vec<usize>>,
    by_unit: std::collections::HashMap<module::UnitId, usize>,
    closure_queries: QueryTable<module::UnitId, usize>,
}

impl ModuleDependencies {
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

    pub(crate) fn new(modules: &[module::ResolvedModule]) -> Self {
        let by_path: std::collections::HashMap<_, _> = modules
            .iter()
            .enumerate()
            .map(|(id, module)| (module.canonical_path.clone(), id))
            .collect();
        let edges = modules
            .iter()
            .map(|module| {
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
        Self {
            by_path,
            edges,
            by_unit: modules.iter().enumerate().map(|(i, m)| (m.id, i)).collect(),
            closure_queries: QueryTable::named("module_dependency_closure"),
        }
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
    #[test]
    fn sparse_dependency_artifacts_scale_with_reachable_output() {
        for size in [16, 64, 256, 1024] {
            for chain in [false, true] {
                let index = ModuleDependencies {
                    by_path: Default::default(),
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
