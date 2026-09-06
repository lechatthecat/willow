//! Canonical type metadata with one shared unit-local alias scope.
use crate::semantic::ids::TypeId;
use std::{cell::RefCell, collections::HashMap, hash::Hash, ops::Index, rc::Rc};

#[derive(Clone, Debug, Default)]
pub struct TypeScope(Rc<RefCell<HashMap<TypeId, TypeId>>>);
impl TypeScope {
    pub fn resolve(&self, id: &TypeId) -> TypeId {
        self.0
            .borrow()
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.clone())
    }
    pub fn bind(&self, alias: &str, canonical: &str) -> Option<TypeId> {
        // Targets are declaration identities, not another unit's local spelling.
        self.0.borrow_mut().insert(
            TypeId::from_source_name(alias),
            TypeId::from_source_name(canonical),
        )
    }
    pub fn restore(&self, alias: &str, previous: Option<TypeId>) {
        let id = TypeId::from_source_name(alias);
        match previous {
            Some(previous) => {
                self.0.borrow_mut().insert(id, previous);
            }
            None => {
                self.0.borrow_mut().remove(&id);
            }
        }
    }
}

pub trait TypeKey: Clone {
    type Id: Eq + Hash + Clone;
    fn canonical(id: &Self::Id) -> Self;
    fn lookup(&self, scope: &TypeScope) -> Self::Id;
    fn declare(&self, scope: &TypeScope) -> Self::Id;
}
impl TypeKey for String {
    type Id = TypeId;
    fn canonical(id: &TypeId) -> Self {
        id.to_string()
    }
    fn lookup(&self, scope: &TypeScope) -> TypeId {
        scope.resolve(&TypeId::from_source_name(self))
    }
    fn declare(&self, scope: &TypeScope) -> TypeId {
        scope.restore(self, None);
        TypeId::from_source_name(self)
    }
}
impl TypeKey for (String, String) {
    type Id = (TypeId, TypeId);
    fn canonical(id: &Self::Id) -> Self {
        (id.0.to_string(), id.1.to_string())
    }
    fn lookup(&self, scope: &TypeScope) -> Self::Id {
        (self.0.lookup(scope), self.1.lookup(scope))
    }
    fn declare(&self, scope: &TypeScope) -> Self::Id {
        self.lookup(scope)
    }
}

#[derive(Clone, Debug)]
pub struct ScopedTypeMap<K: TypeKey, V> {
    values: HashMap<K::Id, (K, V)>,
    scope: TypeScope,
}
pub type TypeMap<V> = ScopedTypeMap<String, V>;
pub type VtableMap<V> = ScopedTypeMap<(String, String), V>;
impl<K: TypeKey, V> Default for ScopedTypeMap<K, V> {
    fn default() -> Self {
        Self::new()
    }
}
impl<K: TypeKey, V> ScopedTypeMap<K, V> {
    pub fn new() -> Self {
        Self::with_scope(TypeScope::default())
    }
    pub fn with_scope(scope: TypeScope) -> Self {
        Self {
            values: HashMap::new(),
            scope,
        }
    }
    pub fn insert(&mut self, key: K, value: V) -> Option<V> {
        let id = key.declare(&self.scope);
        self.values
            .insert(id.clone(), (K::canonical(&id), value))
            .map(|(_, value)| value)
    }
    pub fn keys(&self) -> impl Iterator<Item = &K> {
        self.values.values().map(|(key, _)| key)
    }
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.values.values().map(|(_, value)| value)
    }
    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.values.values().map(|(key, value)| (key, value))
    }
    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    pub fn clear(&mut self) {
        self.values.clear();
    }
}
impl<V> TypeMap<V> {
    pub fn get(&self, key: &str) -> Option<&V> {
        self.values
            .get(&self.scope.resolve(&TypeId::from_source_name(key)))
            .map(|(_, v)| v)
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
    pub fn entry(&mut self, key: String) -> TypeEntry<'_, V> {
        let id = key.declare(&self.scope);
        TypeEntry {
            key,
            entry: self.values.entry(id),
        }
    }
}
impl<V> VtableMap<V> {
    pub fn get(&self, key: &(String, String)) -> Option<&V> {
        self.values.get(&key.lookup(&self.scope)).map(|(_, v)| v)
    }
    pub fn contains_key(&self, key: &(String, String)) -> bool {
        self.get(key).is_some()
    }
}
pub struct TypeEntry<'a, V> {
    key: String,
    entry: std::collections::hash_map::Entry<'a, TypeId, (String, V)>,
}
impl<'a, V> TypeEntry<'a, V> {
    pub fn or_insert(self, value: V) -> &'a mut V {
        &mut self.entry.or_insert((self.key, value)).1
    }
}
impl<V> Index<&str> for TypeMap<V> {
    type Output = V;
    fn index(&self, key: &str) -> &V {
        self.get(key).expect("registered canonical type")
    }
}
impl<V> Index<&String> for TypeMap<V> {
    type Output = V;
    fn index(&self, key: &String) -> &V {
        &self[key.as_str()]
    }
}
impl<V> Index<&(String, String)> for VtableMap<V> {
    type Output = V;
    fn index(&self, key: &(String, String)) -> &V {
        self.get(key).expect("registered canonical vtable")
    }
}
impl<K: TypeKey, V> FromIterator<(K, V)> for ScopedTypeMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut map = Self::new();
        for (k, v) in iter {
            map.insert(k, v);
        }
        map
    }
}

impl<K: TypeKey, V, const N: usize> From<[(K, V); N]> for ScopedTypeMap<K, V> {
    fn from(items: [(K, V); N]) -> Self {
        items.into_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_pair() -> (TypeScope, TypeMap<i64>) {
        let scope = TypeScope::default();
        let mut map = TypeMap::with_scope(scope.clone());
        map.insert("pal::Color".to_string(), 7);
        (scope, map)
    }

    #[test]
    fn ti_01_unbound_id_resolves_to_itself() {
        let scope = TypeScope::default();
        let id = TypeId::from_source_name("pal::Color");
        assert_eq!(scope.resolve(&id), id);
    }

    #[test]
    fn ti_02_alias_reads_the_canonical_entry() {
        let (scope, map) = map_pair();
        assert_eq!(map.get("Color"), None);
        scope.bind("Color", "pal::Color");
        assert_eq!(map.get("Color"), Some(&7));
    }

    #[test]
    fn ti_03_alias_does_not_duplicate_metadata() {
        let (scope, map) = map_pair();
        scope.bind("Color", "pal::Color");
        // One entry, spelled by its identity: the alias is a lookup rule, not a
        // second copy of the enum's metadata.
        assert_eq!(map.len(), 1);
        assert_eq!(
            map.keys().cloned().collect::<Vec<_>>(),
            vec!["pal::Color".to_string()]
        );
    }

    #[test]
    fn ti_04_restore_none_drops_the_binding() {
        let (scope, map) = map_pair();
        let previous = scope.bind("Color", "pal::Color");
        assert_eq!(previous, None);
        scope.restore("Color", previous);
        assert_eq!(map.get("Color"), None);
    }

    #[test]
    fn ti_05_restore_reinstates_an_outer_binding() {
        let scope = TypeScope::default();
        let mut map = TypeMap::with_scope(scope.clone());
        map.insert("pal::Color".to_string(), 7);
        map.insert("ui::Color".to_string(), 9);
        scope.bind("Color", "pal::Color");
        // A nested unit rebinds the same spelling, then hands it back.
        let saved = scope.bind("Color", "ui::Color");
        assert_eq!(map.get("Color"), Some(&9));
        scope.restore("Color", saved);
        assert_eq!(map.get("Color"), Some(&7));
    }

    #[test]
    fn ti_06_declaring_a_name_clears_its_alias() {
        let (scope, mut map) = map_pair();
        scope.bind("Color", "pal::Color");
        // A unit that declares its own `Color` shadows the alias it inherited.
        map.insert("Color".to_string(), 42);
        assert_eq!(map.get("Color"), Some(&42));
        assert_eq!(map.get("pal::Color"), Some(&7));
    }

    #[test]
    fn ti_07_insert_returns_the_replaced_value() {
        let mut map = TypeMap::new();
        assert_eq!(map.insert("pal::Color".to_string(), 7), None);
        assert_eq!(map.insert("pal::Color".to_string(), 8), Some(7));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn ti_08_contains_key_follows_the_alias() {
        let (scope, map) = map_pair();
        assert!(!map.contains_key("Color"));
        scope.bind("Color", "pal::Color");
        assert!(map.contains_key("Color"));
        assert!(map.contains_key("pal::Color"));
    }

    #[test]
    fn ti_09_index_follows_the_alias() {
        let (scope, map) = map_pair();
        scope.bind("Color", "pal::Color");
        assert_eq!(map["Color"], 7);
        assert_eq!(map[&"pal::Color".to_string()], 7);
    }

    #[test]
    #[should_panic(expected = "registered canonical type")]
    fn ti_10_index_of_an_unregistered_type_panics() {
        let map: TypeMap<i64> = TypeMap::new();
        let _ = map["Color"];
    }

    #[test]
    fn ti_11_maps_sharing_a_scope_see_one_binding() {
        let scope = TypeScope::default();
        let mut enums = TypeMap::with_scope(scope.clone());
        let mut layouts = TypeMap::with_scope(scope.clone());
        enums.insert("pal::Color".to_string(), 7);
        layouts.insert("pal::Color".to_string(), 3);
        scope.bind("Color", "pal::Color");
        // One bind, every table that shares the scope: no per-table alias pass.
        assert_eq!(enums.get("Color"), Some(&7));
        assert_eq!(layouts.get("Color"), Some(&3));
    }

    #[test]
    fn ti_12_entry_inserts_under_the_identity() {
        let (scope, mut map) = map_pair();
        scope.bind("Color", "pal::Color");
        // `entry` declares, so it addresses the written name itself.
        *map.entry("Color".to_string()).or_insert(1) += 1;
        assert_eq!(map.get("Color"), Some(&2));
        assert_eq!(map.get("pal::Color"), Some(&7));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn ti_13_vtable_key_resolves_both_halves() {
        let scope = TypeScope::default();
        let mut vtables = VtableMap::with_scope(scope.clone());
        vtables.insert(("pal::Color".to_string(), "shape::Draw".to_string()), 5);
        assert_eq!(
            vtables.get(&("Color".to_string(), "Draw".to_string())),
            None
        );
        scope.bind("Color", "pal::Color");
        scope.bind("Draw", "shape::Draw");
        assert_eq!(
            vtables.get(&("Color".to_string(), "Draw".to_string())),
            Some(&5)
        );
        assert!(vtables.contains_key(&("Color".to_string(), "shape::Draw".to_string())));
        assert_eq!(vtables[&("Color".to_string(), "Draw".to_string())], 5);
    }

    #[test]
    fn ti_14_vtable_insert_stores_the_written_pair_once() {
        let scope = TypeScope::default();
        let mut vtables = VtableMap::with_scope(scope.clone());
        scope.bind("Color", "pal::Color");
        // An alias in force when the vtable is registered still lands on the
        // identity, so a later unit without that alias finds it.
        vtables.insert(("Color".to_string(), "shape::Draw".to_string()), 5);
        scope.restore("Color", None);
        assert_eq!(
            vtables.get(&("pal::Color".to_string(), "shape::Draw".to_string())),
            Some(&5)
        );
        assert_eq!(vtables.len(), 1);
    }

    #[test]
    fn ti_15_clear_empties_the_table_but_not_the_scope() {
        let (scope, mut map) = map_pair();
        scope.bind("Color", "pal::Color");
        map.clear();
        assert!(map.is_empty());
        assert_eq!(map.get("Color"), None);
        map.insert("pal::Color".to_string(), 11);
        assert_eq!(map.get("Color"), Some(&11));
    }

    #[test]
    fn ti_16_from_iter_and_array_build_a_private_scope() {
        let map: TypeMap<i64> = TypeMap::from([("pal::Color".to_string(), 7)]);
        let collected: TypeMap<i64> = vec![("pal::Color".to_string(), 7)].into_iter().collect();
        assert_eq!(map.get("pal::Color"), collected.get("pal::Color"));
        assert_eq!(map.values().copied().collect::<Vec<_>>(), vec![7]);
        assert_eq!(
            map.iter().map(|(k, v)| (k.clone(), *v)).collect::<Vec<_>>(),
            vec![("pal::Color".to_string(), 7)]
        );
    }

    #[test]
    fn ti_17_a_bare_identity_is_reachable_without_a_scope() {
        let mut map = TypeMap::new();
        map.insert("Color".to_string(), 7);
        assert_eq!(map.get("Color"), Some(&7));
        assert_eq!(map.get("pal::Color"), None);
    }

    #[test]
    fn ti_18_binding_chains_are_not_followed_transitively() {
        let scope = TypeScope::default();
        let mut map = TypeMap::with_scope(scope.clone());
        map.insert("pal::Color".to_string(), 7);
        scope.bind("Rank", "Level");
        scope.bind("Level", "pal::Color");
        // `bind` targets are declaration identities, so one hop is the contract.
        assert_eq!(map.get("Level"), Some(&7));
        assert_eq!(map.get("Rank"), None);
    }
}
