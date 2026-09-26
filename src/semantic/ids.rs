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

/// Persistent declaration origin; dependency aliases and session-local package
/// indices are deliberately absent. Intern once per module, then reuse its handle.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
struct SymbolModuleName {
    package: crate::package::PackageIdentity,
    path: crate::module::ModulePath,
    #[serde(skip)]
    namespace: String,
}

/// `$pkg<hash>` prefix of every type name declared by `package`. Stable
/// FNV-1a-128 over the canonical identity (not a security hash); a collision is
/// detected by [`SymbolModule::new`] rather than silently merging declarations.
pub fn package_namespace(package: &crate::package::PackageIdentity) -> String {
    let mut hash = 0x6c62272e07bb014262b821756295c58du128;
    for byte in serde_json::to_vec(package).expect("package identity serializes") {
        hash = (hash ^ u128::from(byte)).wrapping_mul(0x1000000000000000000013b);
    }
    format!("$pkg{hash:032x}")
}

/// Process-local module handle used by compact symbol IDs. Artifacts serialize
/// the underlying package identity and logical path, never the numeric handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolModule(u32);

impl SymbolModule {
    pub fn new(package: crate::package::PackageIdentity, path: crate::module::ModulePath) -> Self {
        let namespace = format!("{}::{}", package_namespace(&package), path.0);
        let name = SymbolModuleName {
            package,
            path,
            namespace,
        };
        if let Some(id) = SYMBOLS
            .read()
            .expect("symbol table poisoned")
            .module_ids
            .get(&name)
        {
            return *id;
        }
        let mut table = SYMBOLS.write().expect("symbol table poisoned");
        if let Some(id) = table.module_ids.get(&name) {
            return *id;
        }
        let id = Self(u32::try_from(table.modules.len()).expect("symbol module table exhausted"));
        let stored = Box::leak(Box::new(name));
        if let Some(previous) = table.module_names.insert(&stored.namespace, id) {
            assert_eq!(
                table.modules[previous.0 as usize], stored,
                "package symbol hash collision"
            );
        }
        table.modules.push(stored);
        table.module_ids.insert(stored, id);
        id
    }
    fn spelling(self) -> &'static SymbolModuleName {
        SYMBOLS.read().expect("symbol table poisoned").modules[self.0 as usize]
    }
    /// Unspellable compiler adapter used by existing string-based type tables.
    pub fn namespace(self) -> &'static str {
        &self.spelling().namespace
    }
    fn for_namespace(namespace: &str) -> Option<Self> {
        SYMBOLS
            .read()
            .expect("symbol table poisoned")
            .module_names
            .get(namespace)
            .copied()
    }
    pub fn package(self) -> &'static crate::package::PackageIdentity {
        &self.spelling().package
    }
    pub fn path(self) -> &'static crate::module::ModulePath {
        &self.spelling().path
    }
}
impl Ord for SymbolModule {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.spelling().cmp(other.spelling())
    }
}
impl PartialOrd for SymbolModule {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl serde::Serialize for SymbolModule {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(self.spelling(), serializer)
    }
}
impl<'de> serde::Deserialize<'de> for SymbolModule {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = <SymbolModuleName as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::new(name.package, name.path))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename = "TypeId")]
struct TypeName {
    #[serde(skip_serializing_if = "Option::is_none")]
    module: Option<SymbolModule>,
    namespace: Option<&'static str>,
    name: &'static str,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename = "FunctionId")]
struct FunctionName {
    #[serde(skip_serializing_if = "Option::is_none")]
    module: Option<SymbolModule>,
    namespace: Option<&'static str>,
    owner: Option<&'static str>,
    name: &'static str,
}
#[derive(Default)]
struct SymbolTable {
    modules: Vec<&'static SymbolModuleName>,
    module_ids: HashMap<&'static SymbolModuleName, SymbolModule>,
    module_names: HashMap<&'static str, SymbolModule>,
    strings: HashSet<&'static str>,
    types: Vec<TypeName>,
    type_ids: HashMap<TypeName, TypeId>,
    functions: Vec<FunctionName>,
    function_ids: HashMap<FunctionName, FunctionId>,
}
static SYMBOLS: LazyLock<RwLock<SymbolTable>> =
    LazyLock::new(|| RwLock::new(SymbolTable::default()));
impl SymbolTable {
    fn find_type(
        &self,
        module: Option<SymbolModule>,
        namespace: Option<&str>,
        name: &str,
    ) -> Option<TypeId> {
        let namespace = match namespace {
            Some(n) => Some(*self.strings.get(n)?),
            None => None,
        };
        self.type_ids
            .get(&TypeName {
                module,
                namespace,
                name: self.strings.get(name)?,
            })
            .copied()
    }
    fn find_function(
        &self,
        module: Option<SymbolModule>,
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
                module,
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
    fn type_id(
        &mut self,
        module: Option<SymbolModule>,
        namespace: Option<&str>,
        name: &str,
    ) -> TypeId {
        let key = TypeName {
            module,
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
        module: Option<SymbolModule>,
        namespace: Option<&str>,
        owner: Option<&str>,
        name: &str,
    ) -> FunctionId {
        let key = FunctionName {
            module,
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
        match namespace.and_then(SymbolModule::for_namespace) {
            Some(module) => Self::intern_in(Some(module), None, name),
            None => Self::intern_in(None, namespace, name),
        }
    }
    fn intern_in(module: Option<SymbolModule>, namespace: Option<&str>, name: &str) -> Self {
        if let Some(id) = SYMBOLS
            .read()
            .expect("symbol table poisoned")
            .find_type(module, namespace, name)
        {
            return id;
        }
        SYMBOLS
            .write()
            .expect("symbol table poisoned")
            .type_id(module, namespace, name)
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
    /// Legacy spelling qualification. Resolved declarations retain their identity;
    /// consumer aliases belong in the scope, not in the canonical symbol.
    pub fn in_namespace(self, namespace: impl AsRef<str>) -> Self {
        if self.module().is_some() {
            return self;
        }
        Self::intern(Some(namespace.as_ref()), self.name())
    }
    /// Attach the declaring module, discarding any consumer-local namespace.
    pub fn in_module(self, module: SymbolModule) -> Self {
        Self::intern_in(Some(module), None, self.name())
    }
    pub fn module(&self) -> Option<SymbolModule> {
        self.spelling().module
    }
    pub fn namespace(&self) -> Option<&str> {
        let name = self.spelling();
        name.module.map(SymbolModule::namespace).or(name.namespace)
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
        if let Some(namespace) = self.namespace() {
            write!(f, "{namespace}::")?;
        }
        f.write_str(spelling.name)
    }
}
impl fmt::Debug for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spelling = self.spelling();
        let mut debug = f.debug_struct("TypeId");
        if let Some(module) = spelling.module {
            debug.field("module", module.spelling());
        }
        debug
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
            module: Option<SymbolModule>,
            namespace: Option<String>,
            name: String,
        }
        let spelling = <Name as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::intern_in(
            spelling.module,
            spelling.namespace.as_deref(),
            &spelling.name,
        ))
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct FunctionId(u32);
impl FunctionId {
    fn intern(namespace: Option<&str>, owner: Option<&str>, name: &str) -> Self {
        match namespace.and_then(SymbolModule::for_namespace) {
            Some(module) => Self::intern_in(Some(module), None, owner, name),
            None => Self::intern_in(None, namespace, owner, name),
        }
    }
    fn intern_in(
        module: Option<SymbolModule>,
        namespace: Option<&str>,
        owner: Option<&str>,
        name: &str,
    ) -> Self {
        if let Some(id) = SYMBOLS
            .read()
            .expect("symbol table poisoned")
            .find_function(module, namespace, owner, name)
        {
            return id;
        }
        SYMBOLS
            .write()
            .expect("symbol table poisoned")
            .function_id(module, namespace, owner, name)
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
        Self::intern_in(
            owner.module,
            owner.namespace,
            Some(owner.name),
            name.as_ref(),
        )
    }
    /// The callable identity of a lambda body, shared by the checker's
    /// resolved call graph and the backend's effect inventory. Body identities
    /// are process-unique and the spelling is not a legal identifier, so it
    /// cannot collide with a source function.
    pub fn lambda(body: crate::parser::ast::BodyId) -> Self {
        Self::free(format!("<lambda {body}>"))
    }
    /// Legacy spelling qualification. Resolved declarations retain their identity;
    /// consumer aliases belong in the scope, not in the canonical symbol.
    pub fn in_namespace(self, namespace: impl AsRef<str>) -> Self {
        if self.module().is_some() {
            return self;
        }
        let name = self.spelling();
        Self::intern(Some(namespace.as_ref()), name.owner, name.name)
    }
    /// Attach the declaring module, discarding any consumer-local namespace.
    pub fn in_module(self, module: SymbolModule) -> Self {
        let name = self.spelling();
        Self::intern_in(Some(module), None, name.owner, name.name)
    }
    pub fn module(&self) -> Option<SymbolModule> {
        self.spelling().module
    }
    pub fn namespace(&self) -> Option<&str> {
        let name = self.spelling();
        name.module.map(SymbolModule::namespace).or(name.namespace)
    }
    pub fn owner(&self) -> Option<&str> {
        self.spelling().owner
    }
    pub fn name(&self) -> &str {
        self.spelling().name
    }
    pub fn unqualified_name(&self) -> &str {
        let name = self.spelling();
        if name.module.is_none() && name.namespace.is_none() && name.owner.is_none() {
            name.name
        } else {
            ""
        }
    }
    pub fn owner_type(&self) -> Option<TypeId> {
        let name = self.spelling();
        name.owner
            .map(|owner| TypeId::intern_in(name.module, name.namespace, owner))
    }
    pub fn is_free_named(&self, expected: &str) -> bool {
        let name = self.spelling();
        name.module.is_none()
            && name.namespace.is_none()
            && name.owner.is_none()
            && name.name == expected
    }
    pub fn is_method_of(&self, expected: &str) -> bool {
        let name = self.spelling();
        name.module.is_none() && name.namespace.is_none() && name.owner == Some(expected)
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
        if let Some(namespace) = self.namespace() {
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
        let mut debug = f.debug_struct("FunctionId");
        if let Some(module) = name.module {
            debug.field("module", module.spelling());
        }
        debug
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
            module: Option<SymbolModule>,
            namespace: Option<String>,
            owner: Option<String>,
            name: String,
        }
        let name = <Name as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::intern_in(
            name.module,
            name.namespace.as_deref(),
            name.owner.as_deref(),
            &name.name,
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
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

/// Persistent identity carries source identity rather than a session-local
/// package number or the consumer's dependency alias.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ResolvedSymbolRef {
    pub package: crate::package::PackageIdentity,
    pub module: crate::module::ModulePath,
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
        if id.module().is_none() {
            declarations.insert(id.to_string(), id);
        }
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
            if id.module().is_some() {
                return *id;
            }
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

/// Equal maps hold equal entries, aliases and backend spellings, so either
/// one reproduces the other's serialized form exactly.
impl<V: PartialEq> PartialEq for FunctionMap<V> {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
            && *self.scope.aliases == *other.scope.aliases
            && *self.scope.declarations.borrow() == *other.scope.declarations.borrow()
    }
}

impl<V: Clone> FunctionMap<V> {
    /// A copy that shares no spelling registry with `self`, as a decoded copy
    /// would not: later `declare` calls on either side stay private to it.
    pub fn detached_clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            scope: FunctionScope {
                aliases: std::rc::Rc::clone(&self.scope.aliases),
                declarations: std::rc::Rc::new(std::cell::RefCell::new(
                    self.scope.declarations.borrow().clone(),
                )),
            },
        }
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
        self.scope.restore(id, None);
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
    fn persistent_symbol_reference_distinguishes_packages_and_roundtrips() {
        use crate::package::{PackageIdentity, PackageSourceIdentity};
        let a = ResolvedSymbolRef {
            package: PackageIdentity {
                name: "utility".into(),
                version: "1.0.0".into(),
                source: PackageSourceIdentity::Git {
                    url: "https://example.test/a".into(),
                },
                revision: Some("abc".into()),
            },
            module: crate::module::ModulePath("util::format".into()),
            symbol: SymbolId::Function(FunctionId::free("format")),
        };
        let mut b = a.clone();
        b.package.source = PackageSourceIdentity::Git {
            url: "https://example.test/b".into(),
        };
        assert_ne!(a, b);
        for symbol in [a.symbol.clone(), SymbolId::Type(TypeId::local("Formatter"))] {
            let original = ResolvedSymbolRef {
                symbol,
                ..a.clone()
            };
            let json = serde_json::to_string(&original).unwrap();
            assert_eq!(
                serde_json::from_str::<ResolvedSymbolRef>(&json).unwrap(),
                original
            );
            assert!(!json.contains("alias"));
        }
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
        let old = scope.bind(alias, FunctionId::free_from_source_name("module.target"));
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
                scope.declare("arbitrary.$linker.label", id);
                signatures.insert("arbitrary.$linker.label", 42);
                effects.insert("arbitrary.$linker.label", true);
                assert_eq!(signatures.ids().next(), Some(&id));
                assert_eq!(signatures.get_id(&id), Some(&42));
                assert_eq!(effects.get_id(&id), Some(&true));
                assert_eq!(scope.lookup_id("arbitrary.$linker.label"), id);
                let alias = FunctionId::free("local_alias");
                let previous = scope.bind(alias, id);
                signatures.set_scope(scope.clone());
                effects.set_scope(scope.clone());
                assert_eq!(signatures.get_id(&alias), Some(&42));
                assert_eq!(effects.get_id(&alias), Some(&true));
                scope.restore(alias, previous);
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

// Session artifacts preserve resolution scope as well as declaration identity.
impl<V: serde::Serialize> serde::Serialize for FunctionMap<V> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let values: Vec<_> = self.values.iter().collect();
        let aliases: Vec<_> = self.scope.aliases.iter().collect();
        serde::Serialize::serialize(
            &(values, aliases, &*self.scope.declarations.borrow()),
            serializer,
        )
    }
}
impl<'de, V: serde::Deserialize<'de>> serde::Deserialize<'de> for FunctionMap<V> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        type Stored<V> = (
            Vec<(FunctionId, V)>,
            Vec<(FunctionId, FunctionId)>,
            HashMap<String, FunctionId>,
        );
        let (values, aliases, declarations) = Stored::<V>::deserialize(deserializer)?;
        Ok(Self {
            values: values.into_iter().collect(),
            scope: FunctionScope {
                aliases: std::rc::Rc::new(aliases.into_iter().collect()),
                declarations: std::rc::Rc::new(std::cell::RefCell::new(declarations)),
            },
        })
    }
}

#[cfg(test)]
mod package_symbol_tests {
    use super::*;

    fn origin(package: usize, path: &str) -> SymbolModule {
        SymbolModule::new(
            crate::package::PackageIdentity {
                name: "utility".into(),
                version: "1.0.0".into(),
                source: crate::package::PackageSourceIdentity::Git {
                    url: format!("https://example.test/package-{package}"),
                },
                revision: Some("abc".into()),
            },
            crate::module::ModulePath(path.into()),
        )
    }

    #[test]
    fn package_symbols_preserve_origin_through_methods_and_artifacts() {
        let a = origin(0, "util::format");
        let b = origin(1, "util::format");
        let first = TypeId::local("Formatter")
            .in_namespace("first_alias")
            .in_module(a);
        let alias = TypeId::local("Formatter")
            .in_namespace("other_alias")
            .in_module(a);
        let other = TypeId::local("Formatter").in_module(b);
        assert_eq!(first, alias);
        assert_ne!(first, other);
        assert_ne!(
            first,
            TypeId::local("Formatter").in_module(origin(0, "other"))
        );
        let method = FunctionId::method(first, "format");
        assert_eq!(method.owner_type(), Some(first));
        assert_ne!(method, FunctionId::method(other, "format"));
        assert_eq!(method.in_namespace("visible"), method);
        assert_eq!(first.in_namespace("visible"), first);
        assert_eq!(
            FunctionId::method(TypeId::local("Self"), "format").resolve_self_owner(&first),
            method
        );
        assert!(
            !FunctionId::free("format")
                .in_module(a)
                .is_free_named("format")
        );
        assert!(!method.is_method_of("Formatter"));
        for id in [
            SymbolId::Type(first),
            SymbolId::Function(method),
            SymbolId::Function(FunctionId::free("format").in_module(a)),
        ] {
            let json = serde_json::to_string(&id).unwrap();
            assert_eq!(serde_json::from_str::<SymbolId>(&json).unwrap(), id);
            assert!(!json.contains("alias"));
            assert!(json.contains("package-0"));
        }
        assert_eq!(std::mem::size_of::<TypeId>(), 4);
        assert_eq!(std::mem::size_of::<FunctionId>(), 4);
    }

    #[test]
    fn same_spelling_functions_do_not_collapse_in_scopes_or_maps() {
        let a = FunctionId::free("format").in_module(origin(0, "util"));
        let b = FunctionId::free("format").in_module(origin(1, "util"));
        let mut scope = FunctionScope::default();
        scope.declare("link_a", a);
        scope.declare("link_b", b);
        // A source spelling with the same display must not override canonical IDs.
        scope.declare("format", FunctionId::free("shadow"));
        scope.bind(FunctionId::free("alias_a"), a);
        scope.bind(FunctionId::free("alias_b"), b);
        let mut map = FunctionMap::with_scope(scope);
        map.insert("link_a", 1);
        map.insert("link_b", 2);
        for restored in [
            map.detached_clone(),
            serde_json::from_str::<FunctionMap<i32>>(&serde_json::to_string(&map).unwrap())
                .unwrap(),
        ] {
            assert_eq!(restored.get_id(&a), Some(&1));
            assert_eq!(restored.get_id(&b), Some(&2));
            assert_eq!(restored.get("alias_a"), Some(&1));
            assert_eq!(restored.get("alias_b"), Some(&2));
            assert_eq!(restored.ids().count(), 2);
        }
    }

    #[test]
    fn repeated_symbols_share_module_handles_across_package_counts() {
        for size in [16, 64, 256, 1024] {
            for packages in [1, 8, size] {
                let mut table = SymbolTable::default();
                let mut modules = HashSet::new();
                let mut symbols = HashSet::new();
                let mut requests = 0;
                for i in 0..size {
                    let module = origin(i % packages, &format!("scaling::m{i}"));
                    modules.insert(module);
                    for _ in 0..8 {
                        let ty = TypeId::local("Formatter").in_module(module);
                        symbols.insert(FunctionId::method(ty, "format"));
                        table.type_id(Some(module), None, "Formatter");
                        table.function_id(Some(module), None, Some("Formatter"), "format");
                        requests += 1;
                    }
                }
                assert_eq!(modules.len(), size);
                assert_eq!(symbols.len(), size);
                assert_eq!(requests, size * 8);
                assert_eq!(table.types.len(), size);
                assert_eq!(table.functions.len(), size);
                assert_eq!(table.strings.len(), 2);
                eprintln!(
                    "symbol modules={size} packages={packages} requests={requests} unique={}",
                    symbols.len()
                );
            }
        }
    }
}
