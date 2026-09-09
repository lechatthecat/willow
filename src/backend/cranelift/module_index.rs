//! Module aliases bind semantic identities; linker spelling belongs to definitions.

use std::collections::HashMap;

use crate::module::ModuleId;

#[derive(Clone, Debug)]
struct ModuleDefinition {
    canonical_path: String,
    table_name: String,
    linker_prefix: String,
}

#[derive(Default, Clone, Debug)]
pub(super) struct ModuleSymbols {
    bindings: HashMap<String, ModuleId>,
    definitions: HashMap<ModuleId, ModuleDefinition>,
    canonical: HashMap<String, ModuleId>,
}

impl ModuleSymbols {
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
        self.bindings.get(access).copied()
    }

    pub(super) fn bind(&mut self, access: String, id: ModuleId) -> Option<ModuleId> {
        assert!(
            self.definitions.contains_key(&id),
            "alias target must be registered"
        );
        self.bindings.insert(access, id)
    }

    pub(super) fn restore(&mut self, access: String, previous: Option<ModuleId>) {
        match previous {
            Some(id) => {
                self.bind(access, id);
            }
            None => {
                self.bindings.remove(&access);
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
        self.bindings.keys()
    }

    pub(super) fn contains_key(&self, access: &str) -> bool {
        self.bindings.contains_key(access)
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
