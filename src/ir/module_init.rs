//! Module initialization nodes in the resolver's dependency-first order.

use std::collections::HashMap;

use crate::module::{ModuleGraph, ModuleId};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InitUnitId {
    Module(ModuleId),
    Entry,
}

/// Execution order is fixed by module resolution, not by backend body emission.
/// Module IDs are assigned before DFS completes and must never be sorted to
/// obtain initialization order. Entry always follows its imported modules.
#[derive(Debug, Clone)]
pub struct ModuleInitPlan {
    order: Vec<InitUnitId>,
    modules: HashMap<String, ModuleId>,
}

impl Default for ModuleInitPlan {
    fn default() -> Self {
        Self {
            order: vec![InitUnitId::Entry],
            modules: HashMap::new(),
        }
    }
}

impl ModuleInitPlan {
    pub fn from_graph(graph: &ModuleGraph) -> Self {
        let mut plan = Self::default();
        for file in &graph.files {
            plan.modules.insert(file.canonical_path.clone(), file.id);
            plan.order
                .insert(plan.order.len() - 1, InitUnitId::Module(file.id));
        }
        plan
    }

    pub fn order(&self) -> &[InitUnitId] {
        &self.order
    }

    /// Standalone backend callers declare dependency modules first. The normal
    /// compiler path has already supplied every identity from ModuleGraph.
    pub fn ensure_module(&mut self, canonical_path: &str) -> InitUnitId {
        if let Some(id) = self.modules.get(canonical_path) {
            return InitUnitId::Module(*id);
        }
        let id = ModuleId(self.modules.values().map(|id| id.0).max().map_or(0, |id| {
            id.checked_add(1).expect("module identity exhausted")
        }));
        self.modules.insert(canonical_path.to_string(), id);
        let unit = InitUnitId::Module(id);
        self.order.insert(self.order.len() - 1, unit);
        unit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::ResolvedModule;
    use crate::parser::ast::Program;

    #[test]
    fn initialization_follows_dependency_order_not_numeric_ids_or_emission() {
        // Twenty perspectives: ten graph sizes, IDs ascending or descending.
        // Re-registering nodes in reverse emission order must preserve both
        // graph order and the single entry node; aliases use canonical paths.
        for count in 1..=10 {
            for descending in [false, true] {
                let mut graph = ModuleGraph::default();
                for position in 0..count {
                    let id = if descending {
                        count - position
                    } else {
                        position
                    };
                    graph.files.push(ResolvedModule {
                        id: ModuleId(id),
                        name: format!("alias_{position}"),
                        canonical_path: format!("pkg::unit_{position}"),
                        path: Default::default(),
                        source: String::new(),
                        program: Program {
                            module: None,
                            imports: vec![],
                            items: vec![],
                        },
                    });
                }
                let mut plan = ModuleInitPlan::from_graph(&graph);
                let expected: Vec<_> = graph
                    .files
                    .iter()
                    .map(|f| InitUnitId::Module(f.id))
                    .chain([InitUnitId::Entry])
                    .collect();
                for file in graph.files.iter().rev() {
                    assert_eq!(
                        plan.ensure_module(&file.canonical_path),
                        InitUnitId::Module(file.id)
                    );
                }
                assert_eq!(plan.order(), expected);
            }
        }
    }
}
