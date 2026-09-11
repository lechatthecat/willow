//! Interned identities for compiler symbols.
//!
//! IDs are process-local 32-bit handles. The process symbol table owns names;
//! source/artifact boundaries serialize the structured names, never handles.
//! Interned names live for the process so IDs remain valid across independent
//! compiler sessions, builtin caches, and threads. Repeated compilations of the
//! same names reuse storage; distinct names extend the process symbol table.

use crate::module::ModuleId;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Index;
use std::sync::{LazyLock, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename = "TypeId")]
struct TypeName {
    namespace: Option<&'static str>,
    name: &'static str,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename = "FunctionId")]
struct FunctionName {
    namespace: Option<&'static str>,
    owner: Option<&'static str>,
    name: &'static str,
}
#[derive(Default)]
struct SymbolTable {
    strings: HashSet<&'static str>,
    types: Vec<TypeName>,
    type_ids: HashMap<TypeName, TypeId>,
    functions: Vec<FunctionName>,
    function_ids: HashMap<FunctionName, FunctionId>,
}
static SYMBOLS: LazyLock<RwLock<SymbolTable>> =
    LazyLock::new(|| RwLock::new(SymbolTable::default()));
impl SymbolTable {
    fn find_type(&self, namespace: Option<&str>, name: &str) -> Option<TypeId> {
        let namespace = match namespace {
            Some(n) => Some(*self.strings.get(n)?),
            None => None,
        };
        self.type_ids
            .get(&TypeName {
                namespace,
                name: self.strings.get(name)?,
            })
            .copied()
    }
    fn find_function(
        &self,
        namespace: Option<&str>,
        owner: Option<&str>,
        name: &str,
    ) -> Option<FunctionId> {
        let namespace = match namespace {
            Some(n) => Some(*self.strings.get(n)?),
            None => None,
        };
        let owner = match owner {
            Some(n) => Some(*self.strings.get(n)?),
            None => None,
        };
        self.function_ids
            .get(&FunctionName {
                namespace,
                owner,
                name: self.strings.get(name)?,
            })
            .copied()
    }

    fn string(&mut self, name: &str) -> &'static str {
        if let Some(value) = self.strings.get(name) {
            return value;
        }
        let value = Box::leak(name.to_owned().into_boxed_str());
        self.strings.insert(value);
        value
    }
    fn type_id(&mut self, namespace: Option<&str>, name: &str) -> TypeId {
        let key = TypeName {
            namespace: namespace.map(|n| self.string(n)),
            name: self.string(name),
        };
        if let Some(id) = self.type_ids.get(&key) {
            return *id;
        }
        let id = TypeId(u32::try_from(self.types.len()).expect("type symbol table exhausted"));
        self.types.push(key);
        self.type_ids.insert(key, id);
        id
    }
    fn function_id(
        &mut self,
        namespace: Option<&str>,
        owner: Option<&str>,
        name: &str,
    ) -> FunctionId {
        let key = FunctionName {
            namespace: namespace.map(|n| self.string(n)),
            owner: owner.map(|n| self.string(n)),
            name: self.string(name),
        };
        if let Some(id) = self.function_ids.get(&key) {
            return *id;
        }
        let id = FunctionId(
            u32::try_from(self.functions.len()).expect("function symbol table exhausted"),
        );
        self.functions.push(key);
        self.function_ids.insert(key, id);
        id
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct TypeId(u32);
impl TypeId {
    fn intern(namespace: Option<&str>, name: &str) -> Self {
        if let Some(id) = SYMBOLS
            .read()
            .expect("symbol table poisoned")
            .find_type(namespace, name)
        {
            return id;
        }
        SYMBOLS
            .write()
            .expect("symbol table poisoned")
            .type_id(namespace, name)
    }
    fn spelling(&self) -> TypeName {
        SYMBOLS.read().expect("symbol table poisoned").types[self.0 as usize]
    }
    pub fn local(name: impl AsRef<str>) -> Self {
        Self::intern(None, name.as_ref())
    }
    pub fn from_source_name(name: &str) -> Self {
        match name.rsplit_once("::") {
            Some((namespace, name)) => Self::intern(Some(namespace), name),
            None => Self::local(name),
        }
    }
    pub fn in_namespace(self, namespace: impl AsRef<str>) -> Self {
        Self::intern(Some(namespace.as_ref()), self.name())
    }
    pub fn namespace(&self) -> Option<&str> {
        self.spelling().namespace
    }
    pub fn name(&self) -> &str {
        self.spelling().name
    }
}
impl From<String> for TypeId {
    fn from(name: String) -> Self {
        Self::from_source_name(&name)
    }
}
impl From<&str> for TypeId {
    fn from(name: &str) -> Self {
        Self::from_source_name(name)
    }
}
impl Default for TypeId {
    fn default() -> Self {
        Self::local("")
    }
}
impl fmt::Display for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spelling = self.spelling();
        if let Some(namespace) = spelling.namespace {
            write!(f, "{namespace}::")?;
        }
        f.write_str(spelling.name)
    }
}
impl fmt::Debug for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spelling = self.spelling();
        f.debug_struct("TypeId")
            .field("namespace", &spelling.namespace)
            .field("name", &spelling.name)
            .finish()
    }
}
// Ordering is lexical rather than allocation-order-dependent, preserving
// deterministic diagnostics and artifacts across concurrent compilations.
impl Ord for TypeId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.spelling().cmp(&other.spelling())
    }
}
impl PartialOrd for TypeId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl serde::Serialize for TypeId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.spelling(), serializer)
    }
}
impl<'de> serde::Deserialize<'de> for TypeId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(rename = "TypeId")]
        struct Name {
            namespace: Option<String>,
            name: String,
        }
        let spelling = <Name as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::intern(spelling.namespace.as_deref(), &spelling.name))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct FunctionId(u32);
impl FunctionId {
    fn intern(namespace: Option<&str>, owner: Option<&str>, name: &str) -> Self {
        if let Some(id) = SYMBOLS
            .read()
            .expect("symbol table poisoned")
            .find_function(namespace, owner, name)
        {
            return id;
        }
        SYMBOLS
            .write()
            .expect("symbol table poisoned")
            .function_id(namespace, owner, name)
    }
    fn spelling(&self) -> FunctionName {
        SYMBOLS.read().expect("symbol table poisoned").functions[self.0 as usize]
    }
    pub fn free(name: impl AsRef<str>) -> Self {
        Self::intern(None, None, name.as_ref())
    }
    pub fn free_from_source_name(name: &str) -> Self {
        match name.rsplit_once("::") {
            Some((namespace, name)) => Self::intern(Some(namespace), None, name),
            None => Self::free(name),
        }
    }
    pub fn method(owner: TypeId, name: impl AsRef<str>) -> Self {
        let owner = owner.spelling();
        Self::intern(owner.namespace, Some(owner.name), name.as_ref())
    }
    pub fn in_namespace(self, namespace: impl AsRef<str>) -> Self {
        let name = self.spelling();
        Self::intern(Some(namespace.as_ref()), name.owner, name.name)
    }
    pub fn namespace(&self) -> Option<&str> {
        self.spelling().namespace
    }
    pub fn owner(&self) -> Option<&str> {
        self.spelling().owner
    }
    pub fn name(&self) -> &str {
        self.spelling().name
    }
    pub fn unqualified_name(&self) -> &str {
        let name = self.spelling();
        if name.namespace.is_none() && name.owner.is_none() {
            name.name
        } else {
            ""
        }
    }
    pub fn owner_type(&self) -> Option<TypeId> {
        let name = self.spelling();
        name.owner
            .map(|owner| TypeId::intern(name.namespace, owner))
    }
    pub fn is_free_named(&self, expected: &str) -> bool {
        let name = self.spelling();
        name.namespace.is_none() && name.owner.is_none() && name.name == expected
    }
    pub fn is_method_of(&self, expected: &str) -> bool {
        let name = self.spelling();
        name.namespace.is_none() && name.owner == Some(expected)
    }
    pub fn remap_imported_item(&self, item: &str, local: &str) -> Option<Self> {
        if self.is_free_named(item) {
            Some(Self::free(local))
        } else if self.is_method_of(item) {
            Some(Self::method(TypeId::local(local), self.name()))
        } else {
            None
        }
    }
    pub fn resolve_self_owner(self, owner: &TypeId) -> Self {
        if matches!(self.owner(), Some("Self" | "self")) {
            Self::method(*owner, self.name())
        } else {
            self
        }
    }
}
impl From<String> for FunctionId {
    fn from(name: String) -> Self {
        Self::free_from_source_name(&name)
    }
}
impl From<&str> for FunctionId {
    fn from(name: &str) -> Self {
        Self::free_from_source_name(name)
    }
}
impl Default for FunctionId {
    fn default() -> Self {
        Self::free("")
    }
}
impl fmt::Display for FunctionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.spelling();
        if let Some(namespace) = name.namespace {
            write!(f, "{namespace}::")?;
        }
        if let Some(owner) = name.owner {
            write!(f, "{owner}::")?;
        }
        f.write_str(name.name)
    }
}
impl fmt::Debug for FunctionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.spelling();
        f.debug_struct("FunctionId")
            .field("namespace", &name.namespace)
            .field("owner", &name.owner)
            .field("name", &name.name)
            .finish()
    }
}
impl Ord for FunctionId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.spelling().cmp(&other.spelling())
    }
}
impl PartialOrd for FunctionId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl serde::Serialize for FunctionId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&self.spelling(), serializer)
    }
}
impl<'de> serde::Deserialize<'de> for FunctionId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(rename = "FunctionId")]
        struct Name {
            namespace: Option<String>,
            owner: Option<String>,
            name: String,
        }
        let name = <Name as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::intern(
            name.namespace.as_deref(),
            name.owner.as_deref(),
            &name.name,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum SymbolId {
    Type(TypeId),
    Function(FunctionId),
}

/// Canonical cross-file identity. `SymbolId` describes the declaration within
/// a module; `ModuleId` identifies the parsed source file independent of import
/// aliases.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ResolvedSymbolId {
    pub module: ModuleId,
    pub symbol: SymbolId,
}

/// One unit's local spellings resolve to canonical function identities.
/// Clones retain immutable alias snapshots while sharing build-wide declarations.
#[derive(Debug, Clone, Default)]
pub struct FunctionScope {
    aliases: std::rc::Rc<HashMap<FunctionId, FunctionId>>,
    /// Backend spellings are labels for IDs, never input to an ID parser.
    declarations: std::rc::Rc<std::cell::RefCell<HashMap<String, FunctionId>>>,
}

impl FunctionScope {
    /// Associate an emitted/lookup spelling with its declaration identity.
    pub fn declare(&self, spelling: &str, id: FunctionId) {
        let mut declarations = self.declarations.borrow_mut();
        declarations.insert(id.to_string(), id);
        declarations.insert(spelling.to_owned(), id);
    }
    pub fn lookup_id(&self, spelling: &str) -> FunctionId {
        self.resolve(&FunctionId::free(spelling))
    }
    fn declaration_id(&self, spelling: &str) -> FunctionId {
        self.declarations
            .borrow()
            .get(spelling)
            .copied()
            .unwrap_or_else(|| FunctionId::free(spelling))
    }
    pub fn resolve(&self, id: &FunctionId) -> FunctionId {
        self.aliases.get(id).copied().unwrap_or_else(|| {
            self.declarations
                .borrow()
                .get(&id.to_string())
                .copied()
                .unwrap_or(*id)
        })
    }

    pub fn bind(&mut self, alias: FunctionId, canonical: FunctionId) -> Option<FunctionId> {
        let canonical = self.resolve(&canonical);
        std::rc::Rc::make_mut(&mut self.aliases).insert(alias, canonical)
    }

    pub fn restore(&mut self, alias: FunctionId, previous: Option<FunctionId>) {
        let aliases = std::rc::Rc::make_mut(&mut self.aliases);
        match previous {
            Some(id) => {
                aliases.insert(alias, id);
            }
            None => {
                aliases.remove(&alias);
            }
        }
    }
}

/// A function-keyed compiler index with string adapters only at AST/linker
/// boundaries. The stored key is always a [`FunctionId`], preventing it from
/// being accidentally queried with a type or module ID.
#[derive(Debug, Clone)]
pub struct FunctionMap<V> {
    values: HashMap<FunctionId, V>,
    scope: FunctionScope,
}

impl<V> Default for FunctionMap<V> {
    fn default() -> Self {
        Self::with_scope(FunctionScope::default())
    }
}

impl<V> FunctionMap<V> {
    pub fn with_scope(scope: FunctionScope) -> Self {
        Self {
            values: HashMap::new(),
            scope,
        }
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
        self.scope.restore(id.clone(), None);
        self.values.insert(id, value)
    }

    pub fn get(&self, name: &str) -> Option<&V> {
        self.values.get(&self.scope.lookup_id(name))
    }

    pub fn get_id(&self, id: &FunctionId) -> Option<&V> {
        self.values.get(&self.scope.resolve(id))
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    pub fn ids(&self) -> impl Iterator<Item = &FunctionId> {
        self.values.keys()
    }

    pub fn insert_id(&mut self, id: FunctionId, value: V) -> Option<V> {
        self.values.insert(id, value)
    }

    pub fn remove_id(&mut self, id: &FunctionId) -> Option<V> {
        self.values.remove(id)
    }
}

impl<V> Index<&str> for FunctionMap<V> {
    type Output = V;

    fn index(&self, name: &str) -> &Self::Output {
        self.get(name)
            .expect("function is registered in the current scope")
    }
}

impl<V> Index<&String> for FunctionMap<V> {
    type Output = V;

    fn index(&self, name: &String) -> &Self::Output {
        self.index(name.as_str())
    }
}

impl<K: AsRef<str>, V> FromIterator<(K, V)> for FunctionMap<V> {
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
    fn method_identity_keeps_namespace_owner_and_name_separate() {
        let id = FunctionId::method(TypeId::from_source_name("net::Client"), "connect");
        assert_eq!(id.namespace(), Some("net"));
        assert_eq!(id.owner(), Some("Client"));
        assert_eq!(id.name(), "connect");
        assert_eq!(id.to_string(), "net::Client::connect");
    }

    #[test]
    fn imported_class_alias_remaps_only_the_owner() {
        let original = FunctionId::method(TypeId::local("Worker"), "heavy");
        let imported = original.remap_imported_item("Worker", "W").unwrap();
        assert_eq!(imported, FunctionId::method(TypeId::local("W"), "heavy"));
    }

    #[test]
    fn resolved_identity_uses_stable_module_id_not_alias_text() {
        let symbol = SymbolId::Function(FunctionId::free("run"));
        let a = ResolvedSymbolId {
            module: ModuleId(7),
            symbol: symbol.clone(),
        };
        let b = ResolvedSymbolId {
            module: ModuleId(7),
            symbol,
        };
        assert_eq!(a, b);
    }
}

#[cfg(test)]
mod function_scope_tests {
    use super::*;

    #[test]
    fn unit_alias_snapshots_are_isolated_but_declarations_are_shared() {
        let global = FunctionScope::default();
        let mut first = global.clone();
        let mut second = global.clone();
        let alias = FunctionId::free("run");
        let a = FunctionId::free("run").in_namespace("a");
        let b = FunctionId::free("run").in_namespace("b");
        first.bind(alias, a);
        let snapshot = first.clone();
        second.bind(alias, b);
        first.restore(alias, None);
        assert_eq!(global.resolve(&alias), alias);
        assert_eq!(first.resolve(&alias), alias);
        assert_eq!(snapshot.resolve(&alias), a);
        assert_eq!(second.resolve(&alias), b);
        global.declare("shared_linker_label", a);
        assert_eq!(snapshot.lookup_id("shared_linker_label"), a);
        assert_eq!(second.lookup_id("shared_linker_label"), a);
    }

    #[test]
    fn declaration_shadowing_changes_only_the_receiving_map() {
        let mut scope = FunctionScope::default();
        let target = FunctionId::free("target");
        scope.bind(FunctionId::free("local"), target);
        let mut first = FunctionMap::with_scope(scope.clone());
        let mut second = FunctionMap::with_scope(scope.clone());
        first.insert_id(target, 1);
        second.insert_id(target, 2);
        first.insert("local", 3);
        assert_eq!(first.get("local"), Some(&3));
        assert_eq!(second.get("local"), Some(&2));
        assert_eq!(scope.lookup_id("local"), target);
    }

    #[test]
    fn aliases_share_identity_and_restore_shadowed_declarations() {
        let mut scope = FunctionScope::default();
        let mut signatures = FunctionMap::with_scope(scope.clone());
        let mut effects = FunctionMap::with_scope(scope.clone());
        signatures.insert("module.target", "String");
        effects.insert("module.target", true);
        signatures.insert("local", "i64");
        let alias = FunctionId::free_from_source_name("local");
        let old = scope.bind(
            alias.clone(),
            FunctionId::free_from_source_name("module.target"),
        );
        signatures.set_scope(scope.clone());
        effects.set_scope(scope.clone());
        assert_eq!(signatures.get("local"), Some(&"String"));
        assert_eq!(effects.get("local"), Some(&true));
        signatures.insert("module.target", "bool");
        assert_eq!(signatures.get("local"), Some(&"bool"));
        scope.restore(alias, old);
        signatures.set_scope(scope.clone());
        effects.set_scope(scope.clone());
        assert_eq!(signatures.get("local"), Some(&"i64"));
        assert_eq!(effects.get("local"), None);
    }
}

/// Resolved types keep nominal identities separate from source spellings.
pub type SemanticType = crate::parser::ast::Type<TypeId>;

impl From<crate::parser::ast::Type> for SemanticType {
    fn from(ty: crate::parser::ast::Type) -> Self {
        ty.map_names(|name| TypeId::from_source_name(name))
    }
}
impl From<&crate::parser::ast::Type> for SemanticType {
    fn from(ty: &crate::parser::ast::Type) -> Self {
        ty.map_names(|name| TypeId::from_source_name(name))
    }
}

impl SemanticType {
    pub fn to_source(&self) -> crate::parser::ast::Type {
        self.map_names(ToString::to_string)
    }
}

#[cfg(test)]
mod backend_identity_tests {
    use super::*;
    #[test]
    fn twenty_linker_labels_preserve_structured_declaration_identity() {
        // 5 namespaces x 4 declaration kinds: free function, instance/static
        // method, constructor and compiler-generated closure entry.
        for namespace in [
            None,
            Some("one"),
            Some("one::two"),
            Some("under__score"),
            Some("日本"),
        ] {
            for kind in 0..4 {
                let owner = namespace.map_or_else(
                    || TypeId::local("Owner"),
                    |n| TypeId::local("Owner").in_namespace(n),
                );
                let id = match kind {
                    0 => namespace.map_or_else(
                        || FunctionId::free("run"),
                        |n| FunctionId::free("run").in_namespace(n),
                    ),
                    1 => FunctionId::method(owner, "run"),
                    2 => FunctionId::method(owner, "init"),
                    _ => namespace.map_or_else(
                        || FunctionId::free("$lambda.7"),
                        |n| FunctionId::free("$lambda.7").in_namespace(n),
                    ),
                };
                let mut scope = FunctionScope::default();
                let mut signatures = FunctionMap::with_scope(scope.clone());
                let mut effects = FunctionMap::with_scope(scope.clone());
                // A deliberately unrelated linker spelling cannot recover
                // namespace/owner/name by parsing or component splitting.
                scope.declare("arbitrary.$linker.label", id.clone());
                signatures.insert("arbitrary.$linker.label", 42);
                effects.insert("arbitrary.$linker.label", true);
                assert_eq!(signatures.ids().next(), Some(&id));
                assert_eq!(signatures.get_id(&id), Some(&42));
                assert_eq!(effects.get_id(&id), Some(&true));
                assert_eq!(scope.lookup_id("arbitrary.$linker.label"), id);
                let alias = FunctionId::free("local_alias");
                let previous = scope.bind(alias.clone(), id.clone());
                signatures.set_scope(scope.clone());
                effects.set_scope(scope.clone());
                assert_eq!(signatures.get_id(&alias), Some(&42));
                assert_eq!(effects.get_id(&alias), Some(&true));
                scope.restore(alias.clone(), previous);
                signatures.set_scope(scope.clone());
                effects.set_scope(scope.clone());
                assert_eq!(signatures.get_id(&alias), None);
                assert_eq!(id.namespace(), namespace);
                assert_eq!(id.owner().is_some(), matches!(kind, 1 | 2));
            }
        }
    }
}

#[cfg(test)]
mod intern_tests {
    use super::*;

    #[test]
    fn handles_are_four_bytes_and_preserve_structural_identity() {
        assert_eq!(std::mem::size_of::<TypeId>(), 4);
        assert_eq!(std::mem::size_of::<FunctionId>(), 4);
        let ty = TypeId::from_source_name("ns::Owner");
        assert_eq!(ty, TypeId::local("Owner").in_namespace("ns"));
        let method = FunctionId::method(ty, "call");
        assert_eq!(method.owner_type(), Some(ty));
        assert_ne!(method, FunctionId::free_from_source_name("ns::Owner::call"));
        assert_eq!(method.to_string(), "ns::Owner::call");
    }

    #[test]
    fn artifact_roundtrip_uses_names_not_allocation_order() {
        let source = r#"{"namespace":"日本::nested","owner":"Owner","name":"run"}"#;
        let id: FunctionId = serde_json::from_str(source).unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), source);
        assert_eq!(
            id,
            FunctionId::method(TypeId::local("Owner").in_namespace("日本::nested"), "run")
        );
        let source = r#"{"namespace":null,"name":"Item"}"#;
        let id: TypeId = serde_json::from_str(source).unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), source);
        assert!(TypeId::local("a") < TypeId::local("z"));
    }

    #[test]
    fn concurrent_sessions_reuse_the_same_handles() {
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    (0..500)
                        .map(|i| {
                            FunctionId::method(TypeId::local(format!("intern_test_{i}")), "run")
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let values: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert!(values.windows(2).all(|pair| pair[0] == pair[1]));
    }
}
