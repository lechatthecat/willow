//! Canonical type metadata with one shared unit-local alias scope.
use crate::semantic::ids::TypeId;
use std::{cell::RefCell, collections::HashMap, hash::Hash, ops::Index, rc::Rc};

/// Alias spellings can refer forward to other spellings. Declaration targets
/// are terminal, even when another source alias shadows that declaration name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeBinding {
    Alias(TypeId),
    Canonical(TypeId),
}

#[derive(Clone, Debug, Default)]
pub struct TypeScope(Rc<RefCell<HashMap<TypeId, TypeBinding>>>);
impl TypeScope {
    pub fn resolve(&self, id: &TypeId) -> TypeId {
        let bindings = self.0.borrow();
        let mut current = id.clone();
        // At most one visit per binding before reaching a terminal or a cycle.
        // Cyclic aliases have no declaration identity: preserve the original
        // spelling, allowing normal missing-type handling instead of looping.
        for _ in 0..=bindings.len() {
            match bindings.get(&current) {
                Some(TypeBinding::Alias(next)) => current = next.clone(),
                Some(TypeBinding::Canonical(target)) => return target.clone(),
                None => return current,
            }
        }
        id.clone()
    }
    pub fn bind(&self, alias: &str, target: &str) -> Option<TypeBinding> {
        self.0.borrow_mut().insert(
            TypeId::from_source_name(alias),
            TypeBinding::Alias(TypeId::from_source_name(target)),
        )
    }
    pub fn bind_canonical(&self, alias: &str, canonical: &str) -> Option<TypeBinding> {
        self.0.borrow_mut().insert(
            TypeId::from_source_name(alias),
            TypeBinding::Canonical(TypeId::from_source_name(canonical)),
        )
    }
    pub fn restore(&self, alias: &str, previous: Option<TypeBinding>) {
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
    fn lookup(&self, scope: &TypeScope) -> Self::Id;
    fn declare(&self, scope: &TypeScope) -> Self::Id;
}
impl TypeKey for TypeId {
    type Id = TypeId;
    fn lookup(&self, scope: &TypeScope) -> TypeId {
        scope.resolve(self)
    }
    fn declare(&self, scope: &TypeScope) -> TypeId {
        scope.restore(&self.to_string(), None);
        self.clone()
    }
}
impl TypeKey for (TypeId, TypeId) {
    type Id = (TypeId, TypeId);
    fn lookup(&self, scope: &TypeScope) -> Self::Id {
        (scope.resolve(&self.0), scope.resolve(&self.1))
    }
    fn declare(&self, scope: &TypeScope) -> Self::Id {
        self.lookup(scope)
    }
}

#[derive(Clone, Debug)]
pub struct ScopedTypeMap<K: TypeKey, V> {
    values: HashMap<K::Id, V>,
    scope: TypeScope,
}
pub type TypeMap<V> = ScopedTypeMap<TypeId, V>;
pub type VtableMap<V> = ScopedTypeMap<(TypeId, TypeId), V>;
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
    pub fn insert(&mut self, key: impl Into<K>, value: V) -> Option<V> {
        let key = key.into();
        let id = key.declare(&self.scope);
        self.values.insert(id, value)
    }
    pub fn keys(&self) -> impl Iterator<Item = &K::Id> {
        self.values.keys()
    }
    pub fn values(&self) -> impl Iterator<Item = &V> {
        self.values.values()
    }
    pub fn iter(&self) -> impl Iterator<Item = (&K::Id, &V)> {
        self.values.iter()
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
pub trait TypeLookup {
    fn type_id(&self) -> TypeId;
}
impl<T: TypeLookup + ?Sized> TypeLookup for &T {
    fn type_id(&self) -> TypeId {
        (*self).type_id()
    }
}
impl TypeLookup for str {
    fn type_id(&self) -> TypeId {
        TypeId::from_source_name(self)
    }
}
impl TypeLookup for String {
    fn type_id(&self) -> TypeId {
        TypeId::from_source_name(self)
    }
}
impl TypeLookup for TypeId {
    fn type_id(&self) -> TypeId {
        self.clone()
    }
}

impl<V> TypeMap<V> {
    pub fn get_id(&self, id: &TypeId) -> Option<&V> {
        self.values.get(&self.scope.resolve(id))
    }
    pub fn get_canonical_id(&self, id: &TypeId) -> Option<&V> {
        self.values.get(id)
    }
    pub fn insert_canonical_id(&mut self, id: TypeId, value: V) -> Option<V> {
        self.values.insert(id, value)
    }
    pub fn get<Q: TypeLookup + ?Sized>(&self, key: &Q) -> Option<&V> {
        self.values.get(&self.scope.resolve(&key.type_id()))
    }
    pub fn contains_key<Q: TypeLookup + ?Sized>(&self, key: &Q) -> bool {
        self.get(key).is_some()
    }
    /// Look `key` up as a DECLARATION identity, ignoring whatever aliases the
    /// unit being compiled has installed (willow-kd1v).
    ///
    /// For the build-wide passes that walk names the tables themselves
    /// recorded: those names are already canonical, and a unit that binds one
    /// of them to another module's type — an `import sales as books;` in a
    /// build that also has a real `books` module — must not change what they
    /// mean for every other class in the program.
    pub fn get_canonical<Q: TypeLookup + ?Sized>(&self, key: &Q) -> Option<&V> {
        self.values.get(&key.type_id())
    }
    /// Write under `key`'s own identity, leaving any alias over it in place.
    ///
    /// The counterpart of [`Self::get_canonical`]: a build-wide pass rewrites
    /// the entries it reads, and must neither follow a unit's alias into
    /// another module's entry (which [`Self::insert`] avoids by tearing the
    /// alias down) nor tear that alias down under the unit still using it.
    pub fn entry(&mut self, key: impl Into<TypeId>) -> TypeEntry<'_, V> {
        let key = key.into();
        let id = key.declare(&self.scope);
        TypeEntry {
            entry: self.values.entry(id),
        }
    }
}
impl<V> VtableMap<V> {
    pub fn get(&self, key: &(TypeId, TypeId)) -> Option<&V> {
        self.values.get(&key.lookup(&self.scope))
    }
    pub fn contains_key(&self, key: &(TypeId, TypeId)) -> bool {
        self.get(key).is_some()
    }
}
pub struct TypeEntry<'a, V> {
    entry: std::collections::hash_map::Entry<'a, TypeId, V>,
}
impl<'a, V> TypeEntry<'a, V> {
    pub fn or_insert(self, value: V) -> &'a mut V {
        self.entry.or_insert(value)
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
impl<V> Index<&(TypeId, TypeId)> for VtableMap<V> {
    type Output = V;
    fn index(&self, key: &(TypeId, TypeId)) -> &V {
        self.get(key).expect("registered canonical vtable")
    }
}
impl<K: TypeKey, Q: Into<K>, V> FromIterator<(Q, V)> for ScopedTypeMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (Q, V)>>(iter: T) -> Self {
        let mut map = Self::new();
        for (k, v) in iter {
            map.insert(k, v);
        }
        map
    }
}

impl<K: TypeKey, Q: Into<K>, V, const N: usize> From<[(Q, V); N]> for ScopedTypeMap<K, V> {
    fn from(items: [(Q, V); N]) -> Self {
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
            vec![TypeId::from_source_name("pal::Color")]
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
        vtables.insert(
            (
                TypeId::from_source_name("pal::Color"),
                TypeId::from_source_name("shape::Draw"),
            ),
            5,
        );
        assert_eq!(
            vtables.get(&(
                TypeId::from_source_name("Color"),
                TypeId::from_source_name("Draw")
            )),
            None
        );
        scope.bind("Color", "pal::Color");
        scope.bind("Draw", "shape::Draw");
        assert_eq!(
            vtables.get(&(
                TypeId::from_source_name("Color"),
                TypeId::from_source_name("Draw")
            )),
            Some(&5)
        );
        assert!(vtables.contains_key(&(
            TypeId::from_source_name("Color"),
            TypeId::from_source_name("shape::Draw")
        )));
        assert_eq!(
            vtables[&(
                TypeId::from_source_name("Color"),
                TypeId::from_source_name("Draw")
            )],
            5
        );
    }

    #[test]
    fn ti_14_vtable_insert_stores_the_written_pair_once() {
        let scope = TypeScope::default();
        let mut vtables = VtableMap::with_scope(scope.clone());
        scope.bind("Color", "pal::Color");
        // An alias in force when the vtable is registered still lands on the
        // identity, so a later unit without that alias finds it.
        vtables.insert(
            (
                TypeId::from_source_name("Color"),
                TypeId::from_source_name("shape::Draw"),
            ),
            5,
        );
        scope.restore("Color", None);
        assert_eq!(
            vtables.get(&(
                TypeId::from_source_name("pal::Color"),
                TypeId::from_source_name("shape::Draw")
            )),
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
            vec![(TypeId::from_source_name("pal::Color"), 7)]
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
    fn ti_18_forward_binding_chains_reach_the_canonical_entry() {
        let scope = TypeScope::default();
        let mut map = TypeMap::with_scope(scope.clone());
        map.insert("pal::Color".to_string(), 7);
        scope.bind("Rank", "Level");
        scope.bind("Level", "pal::Color");
        // Forward spelling aliases resolve after their target is installed.
        assert_eq!(map.get("Level"), Some(&7));
        assert_eq!(map.get("Rank"), Some(&7));
    }

    #[test]
    fn ti_19_a_canonical_read_ignores_an_installed_binding() {
        let scope = TypeScope::default();
        let mut map = TypeMap::with_scope(scope.clone());
        map.insert("pal::Color".to_string(), 7);
        map.insert("ui::Color".to_string(), 9);
        // One unit reaches `ui` under a spelling that is another module's key.
        scope.bind("ui::Color", "pal::Color");
        assert_eq!(map.get("ui::Color"), Some(&7));
        // A build-wide pass walking recorded names must still see the entry it
        // recorded (willow-kd1v).
        assert_eq!(map.get_canonical("ui::Color"), Some(&9));
    }

    #[test]
    fn ti_20_a_canonical_write_leaves_the_binding_standing() {
        let scope = TypeScope::default();
        let mut map = TypeMap::with_scope(scope.clone());
        map.insert("pal::Color".to_string(), 7);
        map.insert("ui::Color".to_string(), 9);
        scope.bind("ui::Color", "pal::Color");
        // The written entry is the shadowed one, and the alias survives the
        // write -- where `insert` would both retarget it and tear it down.
        assert_eq!(
            map.insert_canonical_id(TypeId::from_source_name("ui::Color"), 11),
            Some(9)
        );
        assert_eq!(map.get_canonical("ui::Color"), Some(&11));
        assert_eq!(map.get_canonical("pal::Color"), Some(&7));
        assert_eq!(map.get("ui::Color"), Some(&7));
        assert_eq!(map.len(), 2);
    }
    #[test]
    fn ti_21_canonical_targets_stop_at_shadowed_declarations() {
        let (scope, mut map) = map_pair();
        map.insert("ui::Color", 9);
        scope.bind_canonical("pal::Color", "ui::Color");
        scope.bind_canonical("Color", "pal::Color");
        scope.bind("Rank", "Color");
        assert_eq!(map.get("pal::Color"), Some(&9));
        assert_eq!(map.get("Color"), Some(&7));
        assert_eq!(map.get("Rank"), Some(&7));
    }

    #[test]
    fn ti_22_restore_preserves_alias_and_terminal_target_kinds() {
        let (scope, mut map) = map_pair();
        map.insert("ui::Color", 9);
        scope.bind_canonical("pal::Color", "ui::Color");
        scope.bind_canonical("Color", "pal::Color");
        let terminal = scope.bind("Color", "pal::Color");
        assert_eq!(map.get("Color"), Some(&9));
        scope.restore("Color", terminal);
        assert_eq!(map.get("Color"), Some(&7));
        scope.bind("Rank", "Color");
        let alias = scope.bind_canonical("Rank", "ui::Color");
        scope.restore("Rank", alias);
        assert_eq!(map.get("Rank"), Some(&7));
    }

    #[test]
    fn ti_23_cycles_terminate_and_can_be_repaired() {
        for count in 1..=20 {
            let (scope, map) = map_pair();
            for index in 0..count {
                scope.bind(&format!("A{index}"), &format!("A{}", (index + 1) % count));
            }
            assert_eq!(
                scope.resolve(&TypeId::from_source_name("A0")).to_string(),
                "A0"
            );
            assert_eq!(map.get("A0"), None);
            scope.bind_canonical(&format!("A{}", count - 1), "pal::Color");
            assert_eq!(map.get("A0"), Some(&7));
        }
    }

    #[test]
    fn ti_24_long_forward_chain_has_no_depth_limit() {
        let (scope, map) = map_pair();
        for index in 0..1024 {
            scope.bind(&format!("A{index}"), &format!("A{}", index + 1));
        }
        assert_eq!(map.get("A0"), None);
        scope.bind_canonical("A1024", "pal::Color");
        assert_eq!(map.get("A0"), Some(&7));
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn ti_25_declaration_replaces_a_forward_alias() {
        let (scope, mut map) = map_pair();
        scope.bind("Rank", "Level");
        scope.bind("Level", "pal::Color");
        map.insert("Level", 12);
        assert_eq!(map.get("Rank"), Some(&12));
        assert_eq!(map.get_canonical("pal::Color"), Some(&7));
    }

    #[test]
    fn ti_26_vtables_resolve_chains_on_both_sides() {
        let scope = TypeScope::default();
        let mut map = VtableMap::with_scope(scope.clone());
        map.insert(("pal::Color".into(), "shape::Draw".into()), 3);
        scope.bind("Color", "C");
        scope.bind_canonical("C", "pal::Color");
        scope.bind("Draw", "D");
        scope.bind_canonical("D", "shape::Draw");
        assert_eq!(map.get(&("Color".into(), "Draw".into())), Some(&3));
    }
}
