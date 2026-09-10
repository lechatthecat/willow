//! Typed identities for compiler symbols.
//!
//! Source names remain strings at the parser boundary, but compiler indexes use
//! these structured IDs so a module, type, and function cannot be mixed up and
//! `module::Type::method` is never interpreted by ad-hoc string slicing.

use std::collections::HashMap;
use std::fmt;
use std::ops::Index;

use crate::module::ModuleId;

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct TypeId {
    namespace: Option<Box<str>>,
    name: Box<str>,
}

impl TypeId {
    pub fn local(name: impl AsRef<str>) -> Self {
        Self {
            namespace: None,
            name: name.as_ref().into(),
        }
    }

    pub fn from_source_name(name: &str) -> Self {
        match name.rsplit_once("::") {
            Some((namespace, name)) => Self {
                namespace: Some(namespace.into()),
                name: name.into(),
            },
            None => Self::local(name),
        }
    }

    pub fn in_namespace(mut self, namespace: impl AsRef<str>) -> Self {
        self.namespace = Some(namespace.as_ref().into());
        self
    }

    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    pub fn name(&self) -> &str {
        &self.name
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
        if let Some(namespace) = &self.namespace {
            write!(f, "{namespace}::")?;
        }
        f.write_str(&self.name)
    }
}

#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct FunctionId {
    namespace: Option<Box<str>>,
    owner: Option<Box<str>>,
    name: Box<str>,
}

impl FunctionId {
    pub fn free(name: impl AsRef<str>) -> Self {
        Self {
            namespace: None,
            owner: None,
            name: name.as_ref().into(),
        }
    }

    /// Build a free-function ID from a parser call name. The last path segment
    /// is the function and preceding segments form its module namespace.
    pub fn free_from_source_name(name: &str) -> Self {
        match name.rsplit_once("::") {
            Some((namespace, name)) => Self::free(name).in_namespace(namespace),
            None => Self::free(name),
        }
    }

    pub fn method(owner: TypeId, name: impl AsRef<str>) -> Self {
        Self {
            namespace: owner.namespace,
            owner: Some(owner.name),
            name: name.as_ref().into(),
        }
    }

    pub fn in_namespace(mut self, namespace: impl AsRef<str>) -> Self {
        self.namespace = Some(namespace.as_ref().into());
        self
    }

    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    pub fn owner(&self) -> Option<&str> {
        self.owner.as_deref()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn unqualified_name(&self) -> &str {
        if self.namespace.is_none() && self.owner.is_none() {
            self.name()
        } else {
            ""
        }
    }

    pub fn owner_type(&self) -> Option<TypeId> {
        self.owner.as_ref().map(|name| TypeId {
            namespace: self.namespace.clone(),
            name: name.clone(),
        })
    }

    pub fn is_free_named(&self, name: &str) -> bool {
        self.namespace.is_none() && self.owner.is_none() && self.name() == name
    }

    pub fn is_method_of(&self, owner: &str) -> bool {
        self.namespace.is_none() && self.owner() == Some(owner)
    }

    pub fn remap_imported_item(&self, item: &str, local: &str) -> Option<Self> {
        if self.is_free_named(item) {
            Some(Self::free(local))
        } else if self.is_method_of(item) {
            Some(Self::method(TypeId::local(local), self.name.as_ref()))
        } else {
            None
        }
    }

    pub fn resolve_self_owner(mut self, owner: &TypeId) -> Self {
        if self.owner() == Some("Self") || self.owner() == Some("self") {
            self.namespace = owner.namespace.clone();
            self.owner = Some(owner.name.clone());
        }
        self
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
        if let Some(namespace) = &self.namespace {
            write!(f, "{namespace}::")?;
        }
        if let Some(owner) = &self.owner {
            write!(f, "{owner}::")?;
        }
        f.write_str(&self.name)
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
/// All signature/ABI tables share this scope instead of copying metadata.
#[derive(Debug, Clone, Default)]
pub struct FunctionScope(std::rc::Rc<std::cell::RefCell<FunctionBindings>>);

#[derive(Debug, Default)]
struct FunctionBindings {
    aliases: HashMap<FunctionId, FunctionId>,
    /// Backend spellings are labels for IDs, never input to an ID parser.
    declarations: HashMap<String, FunctionId>,
}

impl FunctionScope {
    /// Associate an emitted/lookup spelling with its declaration identity.
    pub fn declare(&self, spelling: &str, id: FunctionId) {
        let mut bindings = self.0.borrow_mut();
        bindings.declarations.insert(id.to_string(), id.clone());
        bindings.declarations.insert(spelling.to_owned(), id);
    }
    pub fn lookup_id(&self, spelling: &str) -> FunctionId {
        self.resolve(&FunctionId::free(spelling))
    }
    fn declaration_id(&self, spelling: &str) -> FunctionId {
        self.0
            .borrow()
            .declarations
            .get(spelling)
            .cloned()
            .unwrap_or_else(|| FunctionId::free(spelling))
    }
    pub fn resolve(&self, id: &FunctionId) -> FunctionId {
        let bindings = self.0.borrow();
        bindings.aliases.get(id).cloned().unwrap_or_else(|| {
            bindings
                .declarations
                .get(&id.to_string())
                .cloned()
                .unwrap_or_else(|| id.clone())
        })
    }

    pub fn bind(&self, alias: FunctionId, canonical: FunctionId) -> Option<FunctionId> {
        let canonical = self.resolve(&canonical);
        self.0.borrow_mut().aliases.insert(alias, canonical)
    }

    pub fn restore(&self, alias: FunctionId, previous: Option<FunctionId>) {
        match previous {
            Some(id) => {
                self.0.borrow_mut().aliases.insert(alias, id);
            }
            None => {
                self.0.borrow_mut().aliases.remove(&alias);
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
    fn aliases_share_identity_and_restore_shadowed_declarations() {
        let scope = FunctionScope::default();
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
        assert_eq!(signatures.get("local"), Some(&"String"));
        assert_eq!(effects.get("local"), Some(&true));
        signatures.insert("module.target", "bool");
        assert_eq!(signatures.get("local"), Some(&"bool"));
        scope.restore(alias, old);
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
                let scope = FunctionScope::default();
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
                assert_eq!(signatures.get_id(&alias), Some(&42));
                assert_eq!(effects.get_id(&alias), Some(&true));
                scope.restore(alias.clone(), previous);
                assert_eq!(signatures.get_id(&alias), None);
                assert_eq!(id.namespace(), namespace);
                assert_eq!(id.owner().is_some(), matches!(kind, 1 | 2));
            }
        }
    }
}
