//! Interned identities for compiler symbols.
//!
//! IDs contain an owner and an index. Each compiler session owns reclaimable
//! storage; artifacts serialize names rather than handles. Context-free adapters
//! resolve through the owning session and return shared, owned strings.

use crate::module::ModuleId;
use std::cell::{Ref, RefCell};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::Index;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

/// Persistent declaration origin; dependency aliases and session-local package
/// indices are deliberately absent. Intern once per module, then reuse its handle.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
struct SymbolModuleName {
    package: crate::package::PackageIdentity,
    path: crate::module::ModulePath,
    #[serde(skip)]
    namespace: Arc<str>,
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

/// Session-owned module identity. Artifacts store package/path, never this handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolModule(Handle);

impl SymbolModule {
    pub fn new(package: crate::package::PackageIdentity, path: crate::module::ModulePath) -> Self {
        let namespace = format!("{}::{}", package_namespace(&package), path.0);
        let name = SymbolModuleName {
            package,
            path,
            namespace: namespace.into(),
        };
        SymbolInterner::current().with_table_mut(|table| {
            #[cfg(test)]
            {
                table.requests += 1;
            }
            if let Some(id) = table.module_ids.get(&name) {
                return *id;
            }
            let name = Arc::new(name);
            let id = Self(table.handle(table.modules.len()));
            if let Some(previous) = table.module_names.get(name.namespace.as_ref()) {
                assert_eq!(
                    table.modules[previous.0.index as usize], name,
                    "package symbol hash collision"
                );
            }
            table.module_names.insert(Arc::clone(&name.namespace), id);
            table.modules.push(Arc::clone(&name));
            table.module_ids.insert(name, id);
            id
        })
    }
    fn spelling(self) -> Arc<SymbolModuleName> {
        self.0
            .with_table(|table| Arc::clone(&table.modules[self.0.index as usize]))
    }
    pub fn namespace(self) -> Arc<str> {
        self.0
            .with_table(|table| Arc::clone(&table.modules[self.0.index as usize].namespace))
    }
    fn for_namespace(namespace: &str) -> Option<Self> {
        SymbolInterner::current()
            .inner
            .table
            .borrow()
            .module_names
            .get(namespace)
            .copied()
    }
    pub fn package(self) -> crate::package::PackageIdentity {
        self.spelling().package.clone()
    }
    pub fn path(self) -> crate::module::ModulePath {
        self.spelling().path.clone()
    }
}
impl Ord for SymbolModule {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.spelling()
            .cmp(&other.spelling())
            .then_with(|| self.0.cmp(&other.0))
    }
}
impl PartialOrd for SymbolModule {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl serde::Serialize for SymbolModule {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serde::Serialize::serialize(&*self.spelling(), serializer)
    }
}
impl<'de> serde::Deserialize<'de> for SymbolModule {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = <SymbolModuleName as serde::Deserialize>::deserialize(deserializer)?;
        Ok(Self::new(name.package, name.path))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename = "TypeId")]
struct TypeName {
    #[serde(skip_serializing_if = "Option::is_none")]
    module: Option<SymbolModule>,
    namespace: Option<Arc<str>>,
    name: Arc<str>,
}
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename = "FunctionId")]
struct FunctionName {
    #[serde(skip_serializing_if = "Option::is_none")]
    module: Option<SymbolModule>,
    namespace: Option<Arc<str>>,
    owner: Option<Arc<str>>,
    name: Arc<str>,
}
#[derive(Default)]
struct SymbolTable {
    #[cfg(test)]
    requests: usize,
    owner: u32,
    modules: Vec<Arc<SymbolModuleName>>,
    module_ids: HashMap<Arc<SymbolModuleName>, SymbolModule>,
    module_names: HashMap<Arc<str>, SymbolModule>,
    strings: HashSet<Arc<str>>,
    types: Vec<TypeName>,
    type_ids: HashMap<TypeName, TypeId>,
    functions: Vec<FunctionName>,
    function_ids: HashMap<FunctionName, FunctionId>,
}
/// An index is meaningful only in its owner. Owner IDs are never reused.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Handle {
    owner: u32,
    index: u32,
}
impl fmt::Debug for Handle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.owner, self.index)
    }
}
impl Handle {
    fn owner(self) -> SymbolInterner {
        let inner = OWNERS
            .with(|owners| owners.borrow().get(&self.owner).and_then(Weak::upgrade))
            .expect("symbol owner was dropped or belongs to another thread");
        SymbolInterner { inner }
    }
    fn with_table<R>(self, f: impl FnOnce(&SymbolTable) -> R) -> R {
        f(&self.owner().inner.table.borrow())
    }
}
struct InternerOwner {
    id: u32,
    table: RefCell<SymbolTable>,
}
impl Drop for InternerOwner {
    fn drop(&mut self) {
        let _ = OWNERS.try_with(|owners| {
            owners.borrow_mut().remove(&self.id);
        });
    }
}
thread_local! {
    static OWNERS: RefCell<HashMap<u32, Weak<InternerOwner>>> = RefCell::new(HashMap::new());
    static ACTIVE: RefCell<Vec<SymbolInterner>> = const { RefCell::new(Vec::new()) };
    // Standalone unit tests exercise compiler phases without a driver. Their
    // arena is reclaimed with each test thread, never used by production code.
    #[cfg(test)]
    static TEST_SYMBOLS: SymbolInterner = SymbolInterner::new();
}
static NEXT_OWNER: AtomicU32 = AtomicU32::new(1);

/// Owns all names for a compilation (or retained incremental analysis).
/// This owner is thread-confined, like CompilerDb. Independent threads use
/// independent owners; no interning or resolution acquires a global lock.
#[derive(Clone)]
pub struct SymbolInterner {
    inner: Rc<InternerOwner>,
}
impl fmt::Debug for SymbolInterner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SymbolInterner")
            .field("owner", &self.inner.id)
            .finish()
    }
}
impl Default for SymbolInterner {
    fn default() -> Self {
        Self::new()
    }
}
impl SymbolInterner {
    pub fn new() -> Self {
        let id = NEXT_OWNER
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .expect("symbol owner IDs exhausted");
        let inner = Rc::new(InternerOwner {
            id,
            table: RefCell::new(SymbolTable {
                owner: id,
                ..Default::default()
            }),
        });
        OWNERS.with(|owners| {
            owners.borrow_mut().insert(id, Rc::downgrade(&inner));
        });
        Self { inner }
    }
    /// Select the destination of constructor and serde adapters for this scope.
    /// The guard restores the enclosing owner even when unwinding.
    pub fn enter(&self) -> SymbolInternerGuard {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            let depth = active.len();
            active.push(self.clone());
            SymbolInternerGuard {
                depth,
                _thread: std::marker::PhantomData,
            }
        })
    }
    pub(crate) fn current() -> Self {
        let current = ACTIVE.with(|active| active.borrow().last().cloned());
        if let Some(current) = current {
            return current;
        }
        #[cfg(test)]
        {
            TEST_SYMBOLS.with(Clone::clone)
        }
        #[cfg(not(test))]
        {
            panic!("symbol construction requires SymbolInterner::enter or CompilerSession")
        }
    }
    fn with_table_mut<R>(&self, f: impl FnOnce(&mut SymbolTable) -> R) -> R {
        f(&mut self.inner.table.borrow_mut())
    }
    /// Intern a source type spelling explicitly in this owner.
    pub fn intern_type(&self, name: &str) -> TypeId {
        let _scope = self.enter();
        TypeId::from_source_name(name)
    }
    /// Intern a source free-function spelling explicitly in this owner.
    pub fn intern_function(&self, name: &str) -> FunctionId {
        let _scope = self.enter();
        FunctionId::free_from_source_name(name)
    }
    /// Borrow a type name without cloning or allocating. Reject foreign IDs.
    pub fn type_name(&self, id: TypeId) -> Ref<'_, str> {
        assert_eq!(id.0.owner, self.inner.id, "foreign symbol owner");
        Ref::map(self.inner.table.borrow(), |table| {
            table.types[id.0.index as usize].name.as_ref()
        })
    }
    /// Borrow a function name without cloning or allocating. Reject foreign IDs.
    pub fn function_name(&self, id: FunctionId) -> Ref<'_, str> {
        assert_eq!(id.0.owner, self.inner.id, "foreign symbol owner");
        Ref::map(self.inner.table.borrow(), |table| {
            table.functions[id.0.index as usize].name.as_ref()
        })
    }
}
#[must_use]
pub struct SymbolInternerGuard {
    depth: usize,
    _thread: std::marker::PhantomData<Rc<()>>,
}
impl Drop for SymbolInternerGuard {
    fn drop(&mut self) {
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            assert_eq!(
                active.len(),
                self.depth + 1,
                "symbol scopes must be dropped in reverse order"
            );
            active.pop();
        });
    }
}
impl SymbolTable {
    fn handle(&self, index: usize) -> Handle {
        Handle {
            owner: self.owner,
            index: u32::try_from(index).expect("symbol table exhausted"),
        }
    }
    fn find_type(
        &self,
        module: Option<SymbolModule>,
        namespace: Option<&str>,
        name: &str,
    ) -> Option<TypeId> {
        let namespace = match namespace {
            Some(n) => Some(Arc::clone(self.strings.get(n)?)),
            None => None,
        };
        self.type_ids
            .get(&TypeName {
                module,
                namespace,
                name: Arc::clone(self.strings.get(name)?),
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
            Some(n) => Some(Arc::clone(self.strings.get(n)?)),
            None => None,
        };
        let owner = match owner {
            Some(n) => Some(Arc::clone(self.strings.get(n)?)),
            None => None,
        };
        self.function_ids
            .get(&FunctionName {
                module,
                namespace,
                owner,
                name: Arc::clone(self.strings.get(name)?),
            })
            .copied()
    }

    fn string(&mut self, name: &str) -> Arc<str> {
        if let Some(value) = self.strings.get(name) {
            return Arc::clone(value);
        }
        let value: Arc<str> = Arc::from(name);
        self.strings.insert(Arc::clone(&value));
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
        let id = TypeId(self.handle(self.types.len()));
        self.types.push(key.clone());
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
        let id = FunctionId(self.handle(self.functions.len()));
        self.functions.push(key.clone());
        self.function_ids.insert(key, id);
        id
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct TypeId(Handle);
impl TypeId {
    fn intern(namespace: Option<&str>, name: &str) -> Self {
        match namespace.and_then(SymbolModule::for_namespace) {
            Some(module) => Self::intern_in(Some(module), None, name),
            None => Self::intern_in(None, namespace, name),
        }
    }
    fn intern_in(module: Option<SymbolModule>, namespace: Option<&str>, name: &str) -> Self {
        SymbolInterner::current().with_table_mut(|table| {
            #[cfg(test)]
            {
                table.requests += 1;
            }
            if let Some(module) = module {
                assert_eq!(module.0.owner, table.owner, "foreign module owner");
            }
            table
                .find_type(module, namespace, name)
                .unwrap_or_else(|| table.type_id(module, namespace, name))
        })
    }
    fn spelling(&self) -> TypeName {
        self.0
            .with_table(|table| table.types[self.0.index as usize].clone())
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
        let _symbols = self.0.owner().enter();
        if self.module().is_some() {
            return self;
        }
        Self::intern(Some(namespace.as_ref()), &self.name())
    }
    /// Attach the declaring module, discarding any consumer-local namespace.
    pub fn in_module(self, module: SymbolModule) -> Self {
        let _symbols = self.0.owner().enter();
        Self::intern_in(Some(module), None, &self.name())
    }
    pub fn module(&self) -> Option<SymbolModule> {
        self.spelling().module
    }
    pub fn namespace(&self) -> Option<Arc<str>> {
        let name = self.spelling();
        name.module.map(SymbolModule::namespace).or(name.namespace)
    }
    pub fn name(&self) -> Arc<str> {
        self.0
            .with_table(|table| Arc::clone(&table.types[self.0.index as usize].name))
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
        f.write_str(&spelling.name)
    }
}
impl fmt::Debug for TypeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spelling = self.spelling();
        let mut debug = f.debug_struct("TypeId");
        if let Some(module) = spelling.module {
            debug.field("module", &*module.spelling());
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
        self.spelling()
            .cmp(&other.spelling())
            .then_with(|| self.0.cmp(&other.0))
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
pub struct FunctionId(Handle);
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
        SymbolInterner::current().with_table_mut(|table| {
            #[cfg(test)]
            {
                table.requests += 1;
            }
            if let Some(module) = module {
                assert_eq!(module.0.owner, table.owner, "foreign module owner");
            }
            table
                .find_function(module, namespace, owner, name)
                .unwrap_or_else(|| table.function_id(module, namespace, owner, name))
        })
    }
    fn spelling(&self) -> FunctionName {
        self.0
            .with_table(|table| table.functions[self.0.index as usize].clone())
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
        let _symbols = owner.0.owner().enter();
        let owner = owner.spelling();
        Self::intern_in(
            owner.module,
            owner.namespace.as_deref(),
            Some(&owner.name),
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
        let _symbols = self.0.owner().enter();
        if self.module().is_some() {
            return self;
        }
        let name = self.spelling();
        Self::intern(Some(namespace.as_ref()), name.owner.as_deref(), &name.name)
    }
    /// Attach the declaring module, discarding any consumer-local namespace.
    pub fn in_module(self, module: SymbolModule) -> Self {
        let _symbols = self.0.owner().enter();
        let name = self.spelling();
        Self::intern_in(Some(module), None, name.owner.as_deref(), &name.name)
    }
    pub fn module(&self) -> Option<SymbolModule> {
        self.spelling().module
    }
    pub fn namespace(&self) -> Option<Arc<str>> {
        let name = self.spelling();
        name.module.map(SymbolModule::namespace).or(name.namespace)
    }
    pub fn owner(&self) -> Option<Arc<str>> {
        self.0
            .with_table(|table| table.functions[self.0.index as usize].owner.clone())
    }
    pub fn name(&self) -> Arc<str> {
        self.0
            .with_table(|table| Arc::clone(&table.functions[self.0.index as usize].name))
    }
    pub fn unqualified_name(&self) -> Option<Arc<str>> {
        self.0.with_table(|table| {
            let name = &table.functions[self.0.index as usize];
            (name.module.is_none() && name.namespace.is_none() && name.owner.is_none())
                .then(|| Arc::clone(&name.name))
        })
    }
    pub fn owner_type(&self) -> Option<TypeId> {
        let _symbols = self.0.owner().enter();
        let name = self.spelling();
        name.owner
            .map(|owner| TypeId::intern_in(name.module, name.namespace.as_deref(), &owner))
    }
    pub fn is_free_named(&self, expected: &str) -> bool {
        let name = self.spelling();
        name.module.is_none()
            && name.namespace.is_none()
            && name.owner.is_none()
            && name.name.as_ref() == expected
    }
    pub fn is_method_of(&self, expected: &str) -> bool {
        let name = self.spelling();
        name.module.is_none() && name.namespace.is_none() && name.owner.as_deref() == Some(expected)
    }
    pub fn remap_imported_item(&self, item: &str, local: &str) -> Option<Self> {
        let _symbols = self.0.owner().enter();
        if self.is_free_named(item) {
            Some(Self::free(local))
        } else if self.is_method_of(item) {
            Some(Self::method(TypeId::local(local), self.name()))
        } else {
            None
        }
    }
    pub fn resolve_self_owner(self, owner: &TypeId) -> Self {
        if matches!(self.owner().as_deref(), Some("Self" | "self")) {
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
        f.write_str(&name.name)
    }
}
impl fmt::Debug for FunctionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self.spelling();
        let mut debug = f.debug_struct("FunctionId");
        if let Some(module) = name.module {
            debug.field("module", &*module.spelling());
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
        self.spelling()
            .cmp(&other.spelling())
            .then_with(|| self.0.cmp(&other.0))
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

type CodegenUnitAliases = std::rc::Rc<std::cell::RefCell<HashMap<FunctionId, Option<FunctionId>>>>;

/// One unit's local spellings resolve to canonical function identities.
/// Clones retain immutable alias snapshots while sharing build-wide declarations.
#[derive(Debug, Clone, Default)]
pub struct FunctionScope {
    aliases: std::rc::Rc<HashMap<FunctionId, FunctionId>>,
    /// Backend unit views share a private overlay; ordinary clones remain snapshots.
    unit_aliases: Option<CodegenUnitAliases>,
    frozen_declarations: bool,
    /// Backend spellings are labels for IDs, never input to an ID parser.
    declarations: std::rc::Rc<std::cell::RefCell<HashMap<String, FunctionId>>>,
}

impl FunctionScope {
    pub(crate) fn fork_codegen_unit(&self, frozen_declarations: bool) -> Self {
        assert!(self.unit_aliases.is_none());
        Self {
            aliases: std::rc::Rc::clone(&self.aliases),
            declarations: std::rc::Rc::clone(&self.declarations),
            unit_aliases: Some(Default::default()),
            frozen_declarations,
        }
    }
    fn alias(&self, id: &FunctionId) -> Option<FunctionId> {
        if let Some(unit) = &self.unit_aliases
            && let Some(value) = unit.borrow().get(id)
        {
            return *value;
        }
        self.aliases.get(id).copied()
    }

    fn snapshot_aliases(&self) -> std::rc::Rc<HashMap<FunctionId, FunctionId>> {
        let mut aliases = std::rc::Rc::clone(&self.aliases);
        if let Some(unit) = &self.unit_aliases {
            let unit = unit.borrow();
            if !unit.is_empty() {
                let aliases = std::rc::Rc::make_mut(&mut aliases);
                for (key, value) in unit.iter() {
                    match value {
                        Some(value) => {
                            aliases.insert(*key, *value);
                        }
                        None => {
                            aliases.remove(key);
                        }
                    }
                }
            }
        }
        aliases
    }

    /// Associate an emitted/lookup spelling with its declaration identity.
    pub fn declare(&self, spelling: &str, id: FunctionId) {
        assert!(
            !self.frozen_declarations,
            "attempt to change frozen function declarations"
        );
        let mut declarations = self.declarations.borrow_mut();
        if id.module().is_none() {
            declarations.insert(id.to_string(), id);
        }
        declarations.insert(spelling.to_owned(), id);
    }
    pub fn lookup_id(&self, spelling: &str) -> FunctionId {
        self.resolve(&FunctionId::free(spelling))
    }
    pub(crate) fn declaration_id(&self, spelling: &str) -> FunctionId {
        self.declarations
            .borrow()
            .get(spelling)
            .copied()
            .unwrap_or_else(|| FunctionId::free(spelling))
    }
    pub fn resolve(&self, id: &FunctionId) -> FunctionId {
        self.alias(id).unwrap_or_else(|| {
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
        if let Some(unit) = &self.unit_aliases {
            let previous = self.alias(&alias);
            unit.borrow_mut().insert(alias, Some(canonical));
            previous
        } else {
            std::rc::Rc::make_mut(&mut self.aliases).insert(alias, canonical)
        }
    }

    pub fn restore(&mut self, alias: FunctionId, previous: Option<FunctionId>) {
        if let Some(unit) = &self.unit_aliases {
            let mut unit = unit.borrow_mut();
            if previous.is_some() || self.aliases.contains_key(&alias) {
                unit.insert(alias, previous);
            } else {
                unit.remove(&alias);
            }
            return;
        }
        if previous.is_none() && !self.aliases.contains_key(&alias) {
            return;
        }
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
            && *self.scope.snapshot_aliases() == *other.scope.snapshot_aliases()
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
                unit_aliases: None,
                frozen_declarations: false,
                aliases: self.scope.snapshot_aliases(),
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
        assert_eq!(id.namespace().as_deref(), Some("net"));
        assert_eq!(id.owner().as_deref(), Some("Client"));
        assert_eq!(id.name().as_ref(), "connect");
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
                assert_eq!(id.namespace().as_deref(), namespace);
                assert_eq!(id.owner().is_some(), matches!(kind, 1 | 2));
            }
        }
    }
}

#[cfg(test)]
mod intern_tests {
    use super::*;

    #[test]
    fn handles_are_eight_bytes_and_preserve_structural_identity() {
        assert_eq!(std::mem::size_of::<TypeId>(), 8);
        assert_eq!(std::mem::size_of::<FunctionId>(), 8);
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
    fn concurrent_sessions_have_independent_handles() {
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
        assert!(values.windows(2).all(|pair| pair[0] != pair[1]));
    }
}

// Session artifacts preserve resolution scope as well as declaration identity.
impl<V: serde::Serialize> serde::Serialize for FunctionMap<V> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut values: Vec<_> = self.values.iter().collect();
        values.sort_unstable_by_key(|(id, _)| **id);
        let alias_snapshot = self.scope.snapshot_aliases();
        let mut aliases: Vec<_> = alias_snapshot.iter().collect();
        aliases.sort_unstable_by_key(|(id, _)| **id);
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
                unit_aliases: None,
                frozen_declarations: false,
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
        assert_eq!(std::mem::size_of::<TypeId>(), 8);
        assert_eq!(std::mem::size_of::<FunctionId>(), 8);
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

#[cfg(test)]
#[path = "ids_session_tests.rs"]
mod session_tests;

#[cfg(test)]
mod codegen_scope_tests {
    use super::*;
    #[test]
    fn unit_overlay_roundtrip_and_detached_clone_keep_bindings() {
        let mut table = FunctionMap::default();
        table.insert("A", 1);
        table.insert("B", 2);
        let mut base = table.scope().clone();
        base.bind(FunctionId::free("local"), FunctionId::free("A"));
        let mut unit = base.fork_codegen_unit(true);
        unit.bind(FunctionId::free("local"), FunctionId::free("B"));
        table.set_scope(unit.clone());
        let detached = table.detached_clone();
        let decoded: FunctionMap<i32> =
            serde_json::from_str(&serde_json::to_string(&table).unwrap()).unwrap();
        assert_eq!(table, detached);
        assert_eq!(table, decoded);
        unit.restore(FunctionId::free("local"), None);
        assert_eq!(table.get("local"), None);
        assert_eq!(detached.get("local"), Some(&2));
        assert_eq!(decoded.get("local"), Some(&2));
        assert_eq!(base.lookup_id("local"), FunctionId::free("A"));
    }
}
