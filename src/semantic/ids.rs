//! Typed identities for compiler symbols.
//!
//! Source names remain strings at the parser boundary, but compiler indexes use
//! these structured IDs so a module, type, and function cannot be mixed up and
//! `module::Type::method` is never interpreted by ad-hoc string slicing.

use std::collections::HashMap;
use std::fmt;
use std::ops::Index;

use crate::module::ModuleId;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeId {
    namespace: Option<Box<str>>,
    name: Box<str>,
}

impl TypeId {
    pub fn local(name: impl Into<Box<str>>) -> Self {
        Self {
            namespace: None,
            name: name.into(),
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

    pub fn in_namespace(mut self, namespace: impl Into<Box<str>>) -> Self {
        self.namespace = Some(namespace.into());
        self
    }

    pub fn namespace(&self) -> Option<&str> {
        self.namespace.as_deref()
    }

    pub fn name(&self) -> &str {
        &self.name
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

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FunctionId {
    namespace: Option<Box<str>>,
    owner: Option<Box<str>>,
    name: Box<str>,
}

impl FunctionId {
    pub fn free(name: impl Into<Box<str>>) -> Self {
        Self {
            namespace: None,
            owner: None,
            name: name.into(),
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

    pub fn method(owner: TypeId, name: impl Into<Box<str>>) -> Self {
        Self {
            namespace: owner.namespace,
            owner: Some(owner.name),
            name: name.into(),
        }
    }

    pub fn in_namespace(mut self, namespace: impl Into<Box<str>>) -> Self {
        self.namespace = Some(namespace.into());
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

/// A function-keyed compiler index with string adapters only at AST/linker
/// boundaries. The stored key is always a [`FunctionId`], preventing it from
/// being accidentally queried with a type or module ID.
#[derive(Debug, Clone)]
pub struct FunctionMap<V>(HashMap<FunctionId, V>);

impl<V> Default for FunctionMap<V> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<V> FunctionMap<V> {
    pub fn insert(&mut self, name: impl AsRef<str>, value: V) -> Option<V> {
        self.0
            .insert(FunctionId::free_from_source_name(name.as_ref()), value)
    }

    pub fn get(&self, name: &str) -> Option<&V> {
        self.0.get(&FunctionId::free_from_source_name(name))
    }

    pub fn contains_key(&self, name: &str) -> bool {
        self.0
            .contains_key(&FunctionId::free_from_source_name(name))
    }

    pub fn ids(&self) -> impl Iterator<Item = &FunctionId> {
        self.0.keys()
    }

    pub fn insert_id(&mut self, id: FunctionId, value: V) -> Option<V> {
        self.0.insert(id, value)
    }

    pub fn remove_id(&mut self, id: &FunctionId) -> Option<V> {
        self.0.remove(id)
    }
}

impl<V> Index<&str> for FunctionMap<V> {
    type Output = V;

    fn index(&self, name: &str) -> &Self::Output {
        &self.0[&FunctionId::free_from_source_name(name)]
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
