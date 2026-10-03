//! Function metadata views with unit-owned resolution and separate emission symbols.
use super::borrowed::Storage;
use crate::semantic::ids::{FunctionId, FunctionScope};
use std::{collections::HashMap, ops::Index};

#[derive(Debug)]
pub struct FunctionMap<'a, V> {
    values: Storage<'a, HashMap<FunctionId, V>>,
    scope: FunctionScope,
    generated: Option<&'a mut HashMap<FunctionId, V>>,
}

impl<V> Default for FunctionMap<'_, V> {
    fn default() -> Self {
        Self::with_scope(FunctionScope::default())
    }
}

impl<V> FunctionMap<'_, V> {
    pub fn with_scope(scope: FunctionScope) -> Self {
        Self {
            values: Storage::default(),
            generated: None,
            scope,
        }
    }

    pub(super) fn borrowed(&self) -> FunctionMap<'_, V> {
        FunctionMap {
            values: Storage::Shared(&self.values),
            scope: self.scope.clone(),
            generated: None,
        }
    }
    pub(super) fn borrowed_mut(&mut self) -> FunctionMap<'_, V> {
        FunctionMap {
            values: Storage::Mutable(&mut self.values),
            scope: self.scope.clone(),
            generated: None,
        }
    }
    pub(super) fn with_generated<'b>(
        &'b self,
        generated: &'b mut HashMap<FunctionId, V>,
    ) -> FunctionMap<'b, V> {
        FunctionMap {
            values: Storage::Shared(&self.values),
            scope: self.scope.clone(),
            generated: Some(generated),
        }
    }
    #[cfg(test)]
    pub(super) fn storage_identity(&self) -> usize {
        std::ptr::from_ref(&*self.values) as usize
    }
    pub fn scope(&self) -> &FunctionScope {
        &self.scope
    }

    /// Install the resolution snapshot for the unit being compiled.
    pub fn set_scope(&mut self, scope: FunctionScope) {
        self.scope = scope;
    }

    /// Register a declaration; an own declaration shadows a same-named import.
    pub fn insert(&mut self, name: impl AsRef<str>, value: V) -> Option<V> {
        let id = self.scope.declaration_id(name.as_ref());
        self.scope.restore(id, None);
        self.generated
            .as_deref_mut()
            .unwrap_or_else(|| &mut self.values)
            .insert(id, value)
    }

    pub fn get(&self, name: &str) -> Option<&V> {
        self.get_canonical(&self.scope.lookup_id(name))
    }

    pub fn get_id(&self, id: &FunctionId) -> Option<&V> {
        self.get_canonical(&self.scope.resolve(id))
    }

    fn get_canonical(&self, id: &FunctionId) -> Option<&V> {
        self.generated
            .as_deref()
            .and_then(|map| map.get(id))
            .or_else(|| self.values.get(id))
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }
}

impl<V> Index<&str> for FunctionMap<'_, V> {
    type Output = V;

    fn index(&self, name: &str) -> &Self::Output {
        self.get(name)
            .expect("function is registered in the current scope")
    }
}

impl<V> Index<&String> for FunctionMap<'_, V> {
    type Output = V;

    fn index(&self, name: &String) -> &Self::Output {
        self.index(name.as_str())
    }
}

impl<K: AsRef<str>, V> FromIterator<(K, V)> for FunctionMap<'_, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut map = Self::default();
        for (name, value) in iter {
            map.insert(name, value);
        }
        map
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_alias_targets_are_resolved_exactly_once() {
        let mut base = FunctionMap::default();
        base.insert("A", 1);
        base.insert("B", 2);
        let mut view = base.borrowed();
        let mut scope = view.scope().fork_codegen_unit(true);
        scope.bind(FunctionId::free("local"), FunctionId::free("A"));
        scope.bind(FunctionId::free("A"), FunctionId::free("B"));
        view.set_scope(scope);
        assert_eq!(view.get("local"), Some(&1));
        assert_eq!(view.get_id(&FunctionId::free("local")), Some(&1));
        assert_eq!(view.get("A"), Some(&2));
        assert_eq!(base.get("A"), Some(&1));
    }
}
