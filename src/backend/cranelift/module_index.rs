//! Module aliases bind semantic identities; linker spelling belongs to definitions.

use std::collections::HashMap;
use std::rc::Rc;

use crate::module::ModuleId;

#[derive(Clone, Debug)]
struct ModuleDefinition {
    canonical_path: String,
    table_name: String,
    linker_prefix: String,
}

/// Immutable snapshot of the aliases visible within one compile unit.
#[derive(Default, Clone, Debug)]
pub(super) struct ModuleResolutionContext(Rc<HashMap<String, ModuleId>>);

#[derive(Default, Clone, Debug)]
pub(super) struct ModuleSymbols {
    context: ModuleResolutionContext,
    bindings: HashMap<String, ModuleId>,
    definitions: HashMap<ModuleId, ModuleDefinition>,
    canonical: HashMap<String, ModuleId>,
}

impl ModuleSymbols {
    pub(super) fn resolution_context(&self) -> ModuleResolutionContext {
        self.context.clone()
    }

    pub(super) fn set_resolution_context(&mut self, context: ModuleResolutionContext) {
        self.context = context;
    }

    pub(super) fn register(&mut self, id: ModuleId, canonical_path: &str, access: &str) {
        let definition = self
            .definitions
            .entry(id)
            .or_insert_with(|| ModuleDefinition {
                canonical_path: canonical_path.to_string(),
                table_name: access.to_string(),
                linker_prefix: super::symbols::module_symbol_prefix(canonical_path),
            });
        assert_eq!(
            definition.canonical_path, canonical_path,
            "module ID reused for another path"
        );
        if let Some(previous) = self.canonical.insert(canonical_path.to_string(), id) {
            assert_eq!(previous, id, "canonical module registered with another ID");
        }
        self.bindings.insert(access.to_string(), id);
    }

    pub(super) fn resolve(&self, access: &str) -> Option<ModuleId> {
        self.context.0.get(access).or_else(|| self.bindings.get(access)).copied()
    }

    pub(super) fn bind(&mut self, access: String, id: ModuleId) -> Option<ModuleId> {
        assert!(
            self.definitions.contains_key(&id),
            "alias target must be registered"
        );
        let previous = self.resolve(&access);
        Rc::make_mut(&mut self.context.0).insert(access, id);
        previous
    }

    #[cfg(test)]
    pub(super) fn restore(&mut self, access: String, previous: Option<ModuleId>) {
        match previous {
            Some(id) => {
                self.bind(access, id);
            }
            None => {
                Rc::make_mut(&mut self.context.0).remove(&access);
            }
        }
    }

    pub(super) fn linker_prefix(&self, access: &str) -> Option<&String> {
        let id = self.resolve(access)?;
        self.definitions
            .get(&id)
            .map(|definition| &definition.linker_prefix)
    }

    pub(super) fn keys(&self) -> impl Iterator<Item = &String> {
        self.context.0.keys().chain(
            self.bindings.keys().filter(|access| !self.context.0.contains_key(*access)),
        )
    }

    pub(super) fn contains_key(&self, access: &str) -> bool {
        self.resolve(access).is_some()
    }

    pub(super) fn table_name(&self, canonical_path: &str) -> Option<&str> {
        let id = self.canonical.get(canonical_path)?;
        self.definitions
            .get(id)
            .map(|definition| definition.table_name.as_str())
    }

    #[cfg(test)]
    pub(super) fn insert(&mut self, access: String, prefix: String) -> Option<String> {
        let previous = self.linker_prefix(&access).cloned();
        let existing = self
            .definitions
            .iter()
            .find_map(|(id, definition)| (definition.linker_prefix == prefix).then_some(*id));
        let id = existing.unwrap_or_else(|| {
            let id = ModuleId(
                self.definitions
                    .keys()
                    .map(|id| id.0)
                    .max()
                    .map_or(0, |id| {
                        id.checked_add(1).expect("module identity exhausted")
                    }),
            );
            // Synthetic fixtures supply linker spelling directly. Their access
            // names may be rebound, so these are not resolver canonical paths.
            self.definitions.insert(
                id,
                ModuleDefinition {
                    canonical_path: access.clone(),
                    table_name: access.clone(),
                    linker_prefix: prefix,
                },
            );
            id
        });
        self.bindings.insert(access, id);
        previous
    }
}

#[cfg(test)]
impl<const N: usize> From<[(String, String); N]> for ModuleSymbols {
    fn from(bindings: [(String, String); N]) -> Self {
        bindings.into_iter().collect()
    }
}

#[cfg(test)]
impl FromIterator<(String, String)> for ModuleSymbols {
    fn from_iter<T: IntoIterator<Item = (String, String)>>(iter: T) -> Self {
        let mut symbols = Self::default();
        for (access, prefix) in iter {
            symbols.insert(access, prefix);
        }
        symbols
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restoring_unit_context_preserves_new_global_registrations() {
        let mut symbols = ModuleSymbols::default();
        symbols.register(ModuleId(0), "pkg::base", "base");
        let outer = symbols.resolution_context();
        symbols.bind("local".into(), ModuleId(0));
        symbols.register(ModuleId(1), "pkg::new", "new");
        symbols.set_resolution_context(outer);
        assert_eq!(symbols.resolve("local"), None);
        assert_eq!(symbols.resolve("new"), Some(ModuleId(1)));
        assert_eq!(symbols.table_name("pkg::new"), Some("new"));
    }

    #[test]
    fn snapshots_and_clones_keep_aliases_isolated() {
        let mut symbols = ModuleSymbols::default();
        symbols.register(ModuleId(0), "pkg::a", "a");
        symbols.register(ModuleId(1), "pkg::b", "b");
        symbols.bind("local".into(), ModuleId(0));
        let snapshot = symbols.resolution_context();
        let mut other = symbols.clone();
        symbols.bind("local".into(), ModuleId(1));
        other.restore("local".into(), None);
        assert_eq!(symbols.resolve("local"), Some(ModuleId(1)));
        assert_eq!(other.resolve("local"), None);
        other.set_resolution_context(snapshot);
        assert_eq!(other.resolve("local"), Some(ModuleId(0)));
    }

    #[test]
    fn overlay_shadows_global_names_without_duplicate_keys() {
        let mut symbols = ModuleSymbols::default();
        symbols.register(ModuleId(0), "pkg::a", "a");
        symbols.register(ModuleId(1), "pkg::b", "b");
        assert_eq!(symbols.bind("a".into(), ModuleId(1)), Some(ModuleId(0)));
        assert_eq!(symbols.resolve("a"), Some(ModuleId(1)));
        assert_eq!(symbols.keys().filter(|name| name.as_str() == "a").count(), 1);
        symbols.restore("a".into(), None);
        assert_eq!(symbols.resolve("a"), Some(ModuleId(0)));
        assert!(symbols.contains_key("a"));
        assert_eq!(symbols.keys().count(), 2);
    }

    #[test]
    fn module_aliases_preserve_identity_and_original_table_names() {
        // Twenty perspectives: ten module counts, fresh or shadowed alias.
        // Similar canonical paths exercise prefixes without using linker strings
        // as identity. Re-registration must retain the first table spelling.
        for count in 1..=10 {
            for shadowed in [false, true] {
                let mut symbols = ModuleSymbols::default();
                for index in 0..count {
                    let path = format!("pkg::unit_{index}::nested");
                    symbols.register(ModuleId(index), &path, &format!("original_{index}"));
                }
                let alias = "temporary".to_string();
                if shadowed {
                    symbols.bind(alias.clone(), ModuleId(0));
                }
                let original = symbols.resolve(&alias);
                for index in 0..count {
                    let path = format!("pkg::unit_{index}::nested");
                    let previous = symbols.bind(alias.clone(), ModuleId(index));
                    assert_eq!(previous, original);
                    assert_eq!(symbols.resolve(&alias), Some(ModuleId(index)));
                    assert_eq!(
                        symbols.linker_prefix(&alias),
                        Some(&super::super::symbols::module_symbol_prefix(&path))
                    );
                    symbols.register(ModuleId(index), &path, &format!("second_{index}"));
                    assert_eq!(
                        symbols.table_name(&path),
                        Some(format!("original_{index}").as_str())
                    );
                    symbols.restore(alias.clone(), previous);
                    assert_eq!(symbols.resolve(&alias), original);
                    assert_eq!(symbols.contains_key(&alias), shadowed);
                }
                assert_eq!(symbols.table_name("missing"), None);
                assert_eq!(symbols.linker_prefix("missing"), None);
            }
        }
    }
}
