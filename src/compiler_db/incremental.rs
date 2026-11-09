//! Pure syntax queries; source capture and artifact I/O stay at the caller.
use super::retained::Retained;
use super::tracked::{
    InputNode, QueryNode, QueryProvider, QueryValue, ResultFingerprint, TrackedQueryTable,
    TrackedStats,
};
use crate::{
    diagnostics::{Diagnostic, FileId, Span},
    module::UnitId,
    parser::ast::{BodyId, Program},
    semantic::ids::FunctionId,
};
use anyhow::Result;
#[path = "persistent_admission.rs"]
pub(crate) mod admission;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
};

/// Persistent source/index inventories with cached payload accounting. Mutating
/// one key copies only that key's value; candidate clones share all map roots.
pub(crate) trait InventoryMeasure: Clone + Retained {
    fn inventory_measure(&self) -> (usize, usize);
}
macro_rules! constant_inventory_measure { ($($ty:ty),*) => { $(impl InventoryMeasure for $ty {
    fn inventory_measure(&self)->(usize,usize) {(self.heap_bytes(),1)}
})* }; }
constant_inventory_measure!(
    String,
    bool,
    u64,
    FunctionId,
    Span,
    PathBuf,
    super::tracked::Durability,
    Source,
    BodyBinding,
    CompactJson,
    im::OrdSet<FunctionId>,
    im::HashSet<BodyId>
);
impl<T: InventoryMeasure> InventoryMeasure for Vec<T> {
    fn inventory_measure(&self) -> (usize, usize) {
        self.iter().fold(
            (self.capacity() * std::mem::size_of::<T>(), 1),
            |(bytes, work), item| {
                let (b, w) = item.inventory_measure();
                (bytes + b, work + w)
            },
        )
    }
}
impl<A: InventoryMeasure, B: InventoryMeasure> InventoryMeasure for (A, B) {
    fn inventory_measure(&self) -> (usize, usize) {
        let (a, aw) = self.0.inventory_measure();
        let (b, bw) = self.1.inventory_measure();
        (a + b, aw + bw)
    }
}
impl<T: InventoryMeasure> InventoryMeasure for std::collections::BTreeSet<T> {
    fn inventory_measure(&self) -> (usize, usize) {
        self.iter().fold(
            (self.len() * (std::mem::size_of::<T>() + 64) * 2, 1),
            |(bytes, work), item| {
                let (b, w) = item.inventory_measure();
                (bytes + b, work + w)
            },
        )
    }
}
#[derive(Clone)]
struct InventoryEntry<V> {
    value: V,
    bytes: usize,
}
pub(crate) struct InventoryMap<K, V> {
    values: im::HashMap<K, std::sync::Arc<InventoryEntry<V>>>,
    bytes: usize,
    accounting: usize,
}
impl<K: Clone + Eq + std::hash::Hash, V> Clone for InventoryMap<K, V> {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            bytes: self.bytes,
            accounting: 0,
        }
    }
}
impl<K, V> Default for InventoryMap<K, V> {
    fn default() -> Self {
        Self {
            values: im::HashMap::new(),
            bytes: 0,
            accounting: 0,
        }
    }
}
impl<K: Clone + Eq + std::hash::Hash + Retained, V: InventoryMeasure> InventoryMap<K, V> {
    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        let (value_bytes, work) = value.inventory_measure();
        self.accounting += 1 + work;
        let bytes = key.heap_bytes() + value_bytes;
        let old = self
            .values
            .insert(key, std::sync::Arc::new(InventoryEntry { value, bytes }));
        self.bytes += bytes;
        if let Some(old) = &old {
            self.bytes -= old.bytes;
        }
        old.map(|old| std::sync::Arc::unwrap_or_clone(old).value)
    }
    fn replace(&mut self, key: K, value: V) {
        let (value_bytes, work) = value.inventory_measure();
        self.accounting += 1 + work;
        let bytes = key.heap_bytes() + value_bytes;
        let old = self
            .values
            .insert(key, std::sync::Arc::new(InventoryEntry { value, bytes }));
        self.bytes += bytes;
        if let Some(old) = old {
            self.bytes -= old.bytes;
        }
    }
    #[cfg(test)]
    fn len(&self) -> usize {
        self.values.len()
    }
    fn get<Q: ?Sized + Eq + std::hash::Hash>(&self, key: &Q) -> Option<&V>
    where
        K: std::borrow::Borrow<Q>,
    {
        self.values.get(key).map(|entry| &entry.value)
    }
    fn contains_key<Q: ?Sized + Eq + std::hash::Hash>(&self, key: &Q) -> bool
    where
        K: std::borrow::Borrow<Q>,
    {
        self.values.contains_key(key)
    }
    fn remove<Q: ?Sized + Eq + std::hash::Hash>(&mut self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
    {
        let old = self.values.remove(key)?;
        self.accounting += 1;
        self.bytes -= old.bytes;
        Some(std::sync::Arc::unwrap_or_clone(old).value)
    }
    fn discard<Q: ?Sized + Eq + std::hash::Hash>(&mut self, key: &Q)
    where
        K: std::borrow::Borrow<Q>,
    {
        if let Some(old) = self.values.remove(key) {
            self.accounting += 1;
            self.bytes -= old.bytes;
        }
    }
    fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.values.iter().map(|(key, entry)| (key, &entry.value))
    }
    fn keys(&self) -> impl Iterator<Item = &K> {
        self.values.keys()
    }
    fn values(&self) -> impl Iterator<Item = &V> {
        self.values.values().map(|entry| &entry.value)
    }
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    #[cfg(test)]
    fn capacity(&self) -> usize {
        self.values.len()
    }
    fn clear(&mut self) {
        let accounting = self.accounting;
        *self = Self::default();
        self.accounting = accounting;
    }
    fn update<R>(&mut self, key: K, apply: impl FnOnce(&mut V) -> R) -> R
    where
        V: Default,
    {
        if !self.values.contains_key(&key) {
            self.replace(key.clone(), V::default());
        }
        let entry = std::sync::Arc::make_mut(self.values.get_mut(&key).unwrap());
        let result = apply(&mut entry.value);
        let (value_bytes, work) = entry.value.inventory_measure();
        self.accounting += 1 + work;
        let bytes = key.heap_bytes() + value_bytes;
        self.bytes = self.bytes - entry.bytes + bytes;
        entry.bytes = bytes;
        result
    }
}
impl<K: Clone + Eq + std::hash::Hash + Retained, V: InventoryMeasure> Retained
    for InventoryMap<K, V>
{
    fn heap_bytes(&self) -> usize {
        self.bytes + self.values.len() * (std::mem::size_of::<(K, V)>() + 128) * 2
    }
}
impl Retained for im::OrdSet<FunctionId> {
    fn heap_bytes(&self) -> usize {
        self.len() * (std::mem::size_of::<FunctionId>() + 64) * 2
    }
}
impl Retained for im::HashSet<BodyId> {
    fn heap_bytes(&self) -> usize {
        self.len() * (std::mem::size_of::<BodyId>() + 64) * 2
    }
}
struct InventorySet<K> {
    values: InventoryMap<K, u64>,
}
impl<K: Clone + Eq + std::hash::Hash> Clone for InventorySet<K> {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
        }
    }
}
impl<K> Default for InventorySet<K> {
    fn default() -> Self {
        Self {
            values: InventoryMap::default(),
        }
    }
}
impl<K: Clone + Eq + std::hash::Hash + Retained> InventorySet<K> {
    fn insert(&mut self, key: K) -> bool {
        self.values.insert(key, 0).is_none()
    }
    fn iter(&self) -> impl Iterator<Item = &K> {
        self.values.keys()
    }
}
impl<K: Clone + Eq + std::hash::Hash + Retained> FromIterator<K> for InventorySet<K> {
    fn from_iter<T: IntoIterator<Item = K>>(values: T) -> Self {
        let mut result = Self::default();
        for value in values {
            result.insert(value);
        }
        result
    }
}
impl<K: Clone + Eq + std::hash::Hash + Retained> Retained for InventorySet<K> {
    fn heap_bytes(&self) -> usize {
        self.values.heap_bytes()
    }
}

#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Source {
    file: FileId,
    text: String,
}

pub(crate) type TokenInventory = Vec<(String, Span)>;
pub(crate) type TokenPairs = (TokenInventory, TokenInventory);

#[derive(Default)]
pub(crate) struct SyntaxQueries {
    table: TrackedQueryTable,
    source_roots: InventoryMap<PathBuf, super::tracked::Durability>,
    interface_dispatch: InventoryMap<UnitId, Vec<FunctionId>>,
    pub(crate) dispatch_classes: InventoryMap<UnitId, std::collections::BTreeSet<String>>,
    captured: InventoryMap<PathBuf, Source>,
    source_paths: InventoryMap<UnitId, PathBuf>,
    owner_changes: InventoryMap<PathBuf, Vec<(String, bool)>>,
    source_body_changes: InventoryMap<PathBuf, Vec<String>>,
    advanced: bool,
    bodies: InventoryMap<BodyId, BodyBinding>,
    prepared: HashMap<BodyId, PreparedBody>,
    symbols: HashMap<(UnitId, String), Rc<Value>>,
    effect_functions: InventoryMap<UnitId, im::OrdSet<FunctionId>>,
    effect_inventory_owners: usize,
    effect_dependency_reads: std::cell::Cell<usize>,
    effect_epochs: InventoryMap<UnitId, u64>,
    reference_wires: InventoryMap<super::references::SymbolUseId, CompactJson>,
    reference_symbols: InventorySet<super::references::SymbolId>,
    reference_uses: InventorySet<super::references::SymbolUseId>,
    effects: HashMap<UnitId, PreparedEffects>,
    derived: HashMap<QueryNode, DerivedValue>,
    checked_roots: std::collections::HashSet<BodyId>,
    changed_body_units: InventoryMap<UnitId, im::HashSet<BodyId>>,
    token_pairs: InventoryMap<PathBuf, TokenPairs>,
    captured_nodes: std::collections::HashSet<InputNode>,
    captured_node_bytes: usize,
    pending_dispatch: InventoryMap<UnitId, std::collections::BTreeSet<String>>,
    pending_interface: InventoryMap<UnitId, Vec<FunctionId>>,
    pending_effect: InventoryMap<UnitId, im::OrdSet<FunctionId>>,
    // Advanced once per revision whose source edits move retained coordinates.
    // A derived memo records the epoch its presentation spans belong to.
    coordinate_epoch: u64,
    coordinates_shifted: bool,
}
impl std::fmt::Debug for SyntaxQueries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntaxQueries")
            .field("revision", &self.table.revision())
            .field("stats", &self.stats())
            .finish()
    }
}
impl SyntaxQueries {
    pub(crate) fn capture_interface_dispatch(
        &mut self,
        unit: UnitId,
        inputs: &std::collections::BTreeMap<FunctionId, Vec<FunctionId>>,
    ) -> Result<()> {
        for old in self.interface_dispatch.remove(&unit).unwrap_or_default() {
            if !inputs.contains_key(&old) {
                self.capture_input(
                    InputNode::InterfaceDispatch(unit, old),
                    value(serde_json::json!([]))?,
                )?;
            }
        }
        self.interface_dispatch
            .replace(unit, inputs.keys().copied().collect());
        for (&id, targets) in inputs {
            self.capture_input(
                InputNode::InterfaceDispatch(unit, id),
                value(serde_json::to_value(targets)?)?,
            )?;
        }
        Ok(())
    }
    pub(crate) fn interface_dispatch_targets(
        &self,
        unit: UnitId,
        id: FunctionId,
    ) -> Result<Vec<FunctionId>> {
        let result = self.table.read(
            &self.provider(),
            QueryNode::InterfaceDispatchTargets(unit, id),
        )?;
        result.get::<CompactJson>().decode()
    }

    pub(crate) fn dispatch_targets(
        &self,
        unit: UnitId,
        class: crate::semantic::ids::TypeId,
        method: &str,
    ) -> Result<Vec<FunctionId>> {
        let result = self.table.read(
            &super::dispatch::DispatchProvider,
            QueryNode::DispatchTargets(unit, class, method.to_owned()),
        )?;
        result.get::<CompactJson>().decode()
    }

    pub(crate) fn capture_references(
        &mut self,
        uses: Vec<(super::references::SymbolUseId, Value)>,
        members: &std::collections::BTreeMap<
            super::references::SymbolId,
            Vec<super::references::SymbolUseId>,
        >,
    ) -> Result<()> {
        let current_uses: std::collections::BTreeSet<_> =
            uses.iter().map(|(id, _)| id.clone()).collect();
        let removed: Vec<_> = self
            .reference_uses
            .iter()
            .filter(|id| !current_uses.contains(*id))
            .cloned()
            .collect();
        for id in removed {
            self.capture_input(InputNode::ResolvedReference(id), value(Value::Null)?)?;
        }
        self.reference_uses = current_uses.into_iter().collect();
        for (id, reference) in uses {
            let mut canonical = reference.clone();
            if let Some(object) = canonical.as_object_mut() {
                object.remove("location");
            }
            self.reference_wires
                .replace(id.clone(), CompactJson::new(&reference)?);
            self.capture_input(InputNode::ResolvedReference(id.clone()), value(canonical)?)?;
            self.resolved_reference(id)?;
        }
        let removed: Vec<_> = self
            .reference_symbols
            .iter()
            .filter(|id| !members.contains_key(*id))
            .cloned()
            .collect();
        for id in removed {
            self.capture_input(
                InputNode::ReferenceMembers(id),
                QueryValue::new(
                    Vec::<super::references::SymbolUseId>::new(),
                    ResultFingerprint::bytes(b"reference-members"),
                ),
            )?;
        }
        self.reference_symbols = members.keys().cloned().collect();
        for (symbol, ids) in members {
            self.capture_input(
                InputNode::ReferenceMembers(symbol.clone()),
                QueryValue::new(ids.clone(), ResultFingerprint::bytes(b"reference-members")),
            )?;
        }
        Ok(())
    }
    pub(crate) fn resolved_reference(
        &mut self,
        id: super::references::SymbolUseId,
    ) -> Result<Value> {
        if self.reference_uses.insert(id.clone()) {
            self.capture_input(
                InputNode::ResolvedReference(id.clone()),
                value(Value::Null)?,
            )?;
        }
        let result = self
            .table
            .read(&self.provider(), QueryNode::ResolvedReference(id.clone()))?;
        match self.reference_wires.get(&id) {
            Some(wire) => wire.decode(),
            None => result.get::<CompactJson>().decode(),
        }
    }
    pub(crate) fn references(&mut self, symbol: super::references::SymbolId) -> Result<Vec<Value>> {
        if self.reference_symbols.insert(symbol.clone()) {
            self.capture_input(
                InputNode::ReferenceMembers(symbol.clone()),
                QueryValue::new(
                    Vec::<super::references::SymbolUseId>::new(),
                    ResultFingerprint::bytes(b"reference-members"),
                ),
            )?;
        }
        let result = self
            .table
            .read(&self.provider(), QueryNode::SymbolReferences(symbol))?;
        result
            .get::<Vec<(super::references::SymbolUseId, CompactJson)>>()
            .iter()
            .map(|(id, semantic)| match self.reference_wires.get(id) {
                Some(wire) => wire.decode(),
                None => semantic.decode(),
            })
            .collect()
    }

    pub(crate) fn read_visible_scope(&self, unit: UnitId, path: &Path) -> Result<QueryValue> {
        self.table.read(
            &self.provider(),
            QueryNode::VisibleScope(unit, path.to_owned()),
        )
    }
    pub(crate) fn visible_scope(
        &mut self,
        unit: UnitId,
        path: &Path,
        declarations: Value,
    ) -> Result<Value> {
        self.capture_input(InputNode::VisibleDeclarations(unit), value(declarations)?)?;
        self.table
            .read(
                &self.provider(),
                QueryNode::VisibleScope(unit, path.to_owned()),
            )?
            .get::<CompactJson>()
            .decode()
    }
    pub(crate) fn candidate(&self) -> Result<Self> {
        let mut candidate = Self {
            table: self.table.candidate()?,
            source_roots: self.source_roots.clone(),
            dispatch_classes: self.dispatch_classes.clone(),
            interface_dispatch: self.interface_dispatch.clone(),
            captured: InventoryMap::default(),
            source_paths: InventoryMap::default(),
            owner_changes: InventoryMap::default(),
            source_body_changes: InventoryMap::default(),
            advanced: false,
            bodies: InventoryMap::default(),
            prepared: HashMap::new(),
            symbols: HashMap::new(),
            effect_functions: self.effect_functions.clone(),
            effect_inventory_owners: 0,
            effect_dependency_reads: Default::default(),
            effect_epochs: self.effect_epochs.clone(),
            reference_wires: InventoryMap::default(),
            reference_symbols: self.reference_symbols.clone(),
            reference_uses: self.reference_uses.clone(),
            effects: HashMap::new(),
            derived: HashMap::new(),
            checked_roots: Default::default(),
            changed_body_units: Default::default(),
            token_pairs: InventoryMap::default(),
            captured_nodes: Default::default(),
            captured_node_bytes: 0,
            pending_dispatch: self.dispatch_classes.clone(),
            pending_interface: self.interface_dispatch.clone(),
            pending_effect: self.effect_functions.clone(),
            coordinate_epoch: self.coordinate_epoch,
            coordinates_shifted: false,
        };
        for (node, payload) in [
            (
                InputNode::CompilerStamp,
                serde_json::json!(env!("CARGO_PKG_VERSION")),
            ),
            (
                InputNode::StdlibStamp,
                serde_json::json!(env!("CARGO_PKG_VERSION")),
            ),
            (
                InputNode::RuntimeAbiRevision,
                serde_json::json!(willow_abi::OBJECT_LAYOUT_REVISION),
            ),
        ] {
            candidate.capture_input(node, value(payload)?)?;
        }
        Ok(candidate)
    }
    pub(crate) fn configure_sources(&mut self, graph: Option<&crate::package::PackageGraph>) {
        use super::tracked::Durability;
        use crate::package::PackageSourceIdentity;
        self.source_roots.clear();
        for package in graph.into_iter().flat_map(|graph| &graph.packages) {
            let immutable = matches!(
                package.identity.source,
                PackageSourceIdentity::Git { .. } | PackageSourceIdentity::GitSubdirectory { .. }
            ) && package.identity.revision.is_some()
                && package.checksum.is_some();
            self.source_roots.replace(
                package.root.clone(),
                if immutable {
                    Durability::High
                } else {
                    Durability::Low
                },
            );
        }
    }
    fn source_durability(&self, path: &Path) -> super::tracked::Durability {
        path.ancestors()
            .find_map(|root| self.source_roots.get(root).copied())
            .unwrap_or_default()
    }
    pub(crate) fn capture_input(&mut self, node: InputNode, payload: QueryValue) -> Result<()> {
        if !self.captured_nodes.insert(node.clone()) {
            anyhow::ensure!(
                self.table
                    .matches_inputs(&[(node.clone(), payload.clone())]),
                "input changed after capture in one candidate: {node:?}"
            );
            return Ok(());
        }
        self.captured_node_bytes += node.heap_bytes();
        match &node {
            InputNode::DispatchDeclaration(unit, _) => {
                self.pending_dispatch.discard(unit);
            }
            InputNode::InterfaceDispatch(unit, _) => {
                self.pending_interface.discard(unit);
            }
            InputNode::EffectRoots(unit) => {
                self.pending_effect.discard(unit);
            }
            _ => {}
        }
        if !self.advanced
            && !self
                .table
                .matches_inputs(&[(node.clone(), payload.clone())])
        {
            self.table.advance_candidate()?;
            self.advanced = true;
        }
        self.table.capture_candidate(node, payload);
        Ok(())
    }
    pub(crate) fn read_with(
        &self,
        provider: &impl QueryProvider,
        node: QueryNode,
    ) -> Result<QueryValue> {
        self.table.read(provider, node)
    }
    fn provider(&self) -> SyntaxProvider<'_> {
        SyntaxProvider {
            source_paths: &self.source_paths,
            interface_dispatch: &self.interface_dispatch,
            bodies: &self.bodies,
            prepared: &self.prepared,
            effects: &self.effects,
            effect_dependency_reads: &self.effect_dependency_reads,
            derived: &self.derived,
        }
    }
    pub(crate) fn capture_semantic(
        &mut self,
        unit: UnitId,
        reads: Vec<(String, Rc<Value>)>,
    ) -> Result<()> {
        for (key, result) in reads {
            if let Some(previous) = self.symbols.get(&(unit, key.clone())) {
                anyhow::ensure!(
                    Rc::ptr_eq(previous, &result) || previous == &result,
                    "semantic input changed during candidate: {key}"
                );
                continue;
            }
            let node = InputNode::SemanticSymbol(unit, key.clone());
            let payload = value((*result).clone())?;
            if !self.advanced
                && !self
                    .table
                    .matches_inputs(&[(node.clone(), payload.clone())])
            {
                self.table.advance_candidate()?;
                self.advanced = true;
            }
            self.table.capture_candidate(node, payload);
            self.symbols.insert((unit, key), result);
        }
        Ok(())
    }
    pub(crate) fn register_body(&mut self, unit: UnitId, body: BodyId, path: &Path, owner: &str) {
        self.bodies.replace(
            body,
            BodyBinding {
                unit,
                path: path.to_owned(),
                owner: owner.to_owned(),
            },
        );
    }
    pub(crate) fn validate_body(
        &mut self,
        unit: UnitId,
        body: BodyId,
        path: &Path,
        owner: &str,
        reads: Vec<(String, Rc<Value>)>,
    ) -> Result<bool> {
        self.capture_semantic(unit, reads)?;
        self.register_body(unit, body, path, owner);
        self.checked_roots.insert(body);
        match self
            .table
            .read(&self.provider(), QueryNode::TypedBody(body))
        {
            Ok(_) => Ok(true),
            Err(error)
                if error
                    .downcast_ref::<super::tracked::DeferredQuery>()
                    .is_some() =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
    /// Typed owners whose semantic result changed in this candidate. Source
    /// removals must be supplied separately by the source inventory owner.
    pub(crate) fn changed_bodies(&self) -> std::collections::HashSet<BodyId> {
        self.table
            .changed_queries()
            .iter()
            .filter_map(|node| match node {
                QueryNode::TypedBody(body) => Some(*body),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn changed_bodies_in(&self, unit: UnitId) -> std::collections::HashSet<BodyId> {
        self.changed_body_units
            .get(&unit)
            .map(|bodies| bodies.iter().copied().collect())
            .unwrap_or_default()
    }
    pub(crate) fn declarations_changed(&self, unit: UnitId) -> bool {
        self.source_paths.get(&unit).is_some_and(|path| {
            self.table
                .changed_queries()
                .contains(&QueryNode::SyntaxDeclarations(path.clone()))
        })
    }
    /// Source body deltas include unchecked templates, which have no TypedBody memo.
    pub(crate) fn source_body_changes(&self, unit: UnitId) -> Vec<String> {
        self.source_paths
            .get(&unit)
            .and_then(|path| self.source_body_changes.get(path))
            .cloned()
            .unwrap_or_default()
    }
    pub(crate) fn source_owner_span(&self, unit: UnitId, owner: &str) -> Option<Span> {
        let path = self.source_paths.get(&unit)?;
        let parsed = self.table.peek(&QueryNode::Parse(path.clone()))?;
        parsed.get::<ParsedValue>().owner_spans.get(owner).copied()
    }
    pub(crate) fn effect_source_changes(&self, unit: UnitId) -> Option<Vec<(String, bool)>> {
        let path = self.source_paths.get(&unit)?;
        if self
            .table
            .changed_queries()
            .contains(&QueryNode::SyntaxImports(path.clone()))
        {
            return None;
        }
        Some(self.owner_changes.get(path).cloned().unwrap_or_default())
    }
    pub(crate) fn body_source_owner(&self, body: BodyId) -> Option<String> {
        Some(self.bodies.get(&body)?.owner.clone())
    }

    pub(crate) fn has_typed_body(&self, body: BodyId) -> bool {
        self.table.peek(&QueryNode::TypedBody(body)).is_some()
    }
    pub(crate) fn publish_body(
        &mut self,
        unit: UnitId,
        body: BodyId,
        result: Value,
        reads: Vec<(String, Rc<Value>)>,
        children: Vec<BodyId>,
    ) -> Result<()> {
        let keys = reads.iter().map(|(key, _)| key.clone()).collect();
        self.capture_semantic(unit, reads)?;
        self.prepared.insert(
            body,
            PreparedBody {
                result: CompactJson::new(&result)?,
                keys,
                children,
            },
        );
        self.table
            .read(&self.provider(), QueryNode::TypedBody(body))?;
        if self
            .table
            .changed_queries()
            .contains(&QueryNode::TypedBody(body))
        {
            self.changed_body_units
                .update(unit, |bodies| bodies.insert(body));
        }
        Ok(())
    }
    #[cfg(test)]
    pub(crate) fn validate_derived(
        &mut self,
        node: QueryNode,
        dependencies: Vec<QueryNode>,
    ) -> Result<Option<Value>> {
        self.validate_derived_typed(node, dependencies)
    }

    /// Decode directly into the caller's owned type. The graph retains only
    /// stable wire bytes; callers may mutate results without changing a memo.
    pub(crate) fn validate_derived_typed<V: serde::de::DeserializeOwned>(
        &mut self,
        node: QueryNode,
        dependencies: Vec<QueryNode>,
    ) -> Result<Option<V>> {
        self.capture_input(
            InputNode::DerivedDependencies(node.clone()),
            QueryValue::new(
                dependencies,
                ResultFingerprint::bytes(b"derived-dependencies-v1"),
            ),
        )?;
        match self.table.read(&self.provider(), node) {
            Ok(result) => {
                let wire = &result.get::<DerivedValue>().wire;
                match wire.split_first() {
                    Some((&DERIVED_WIRE_VERSION, payload)) => {
                        Ok(Some(serde_json::from_slice(payload)?))
                    }
                    _ => Ok(None), // An older wire schema must be recomputed.
                }
            }
            Err(error)
                if error
                    .downcast_ref::<super::tracked::DeferredQuery>()
                    .is_some() =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
    /// Record that this revision's correspondence moves retained coordinates.
    pub(crate) fn shift_coordinates(&mut self) {
        if !self.coordinates_shifted {
            self.coordinates_shifted = true;
            self.coordinate_epoch += 1;
        }
    }
    /// How a validated derived memo's presentation spans relate to the
    /// current source. Memos are relocated only when read, so one skipped in
    /// a shifted revision is more than one correspondence behind.
    pub(crate) fn derived_coordinates(&self, node: &QueryNode) -> DerivedCoordinates {
        let epoch = self
            .table
            .peek(node)
            .map_or(0, |memo| memo.get::<DerivedValue>().epoch);
        if epoch == self.coordinate_epoch {
            DerivedCoordinates::Current
        } else if self.coordinates_shifted && epoch + 1 == self.coordinate_epoch {
            DerivedCoordinates::Previous
        } else {
            DerivedCoordinates::Stale
        }
    }
    pub(crate) fn refresh_derived(&self, node: QueryNode, wire: Value) -> Result<()> {
        let canonical = serde_json::to_vec(&super::syntax::without_spans(&wire))?;
        let fingerprint = ResultFingerprint::bytes(&canonical);
        let mut encoded = vec![DERIVED_WIRE_VERSION];
        serde_json::to_writer(&mut encoded, &wire)?;
        let wire = encoded;
        if self.table.peek(&node).is_some_and(|old| {
            let old = old.get::<DerivedValue>();
            old.epoch == self.coordinate_epoch && old.wire.as_ref() == wire.as_slice()
        }) {
            return Ok(());
        }
        self.table.refresh_value(
            &node,
            QueryValue::new(
                DerivedValue {
                    wire: wire.into(),
                    canonical: canonical.into(),
                    epoch: self.coordinate_epoch,
                },
                fingerprint,
            ),
        )
    }
    pub(crate) fn publish_derived(
        &mut self,
        node: QueryNode,
        wire: Value,
        dependencies: Vec<QueryNode>,
    ) -> Result<()> {
        self.capture_input(
            InputNode::DerivedDependencies(node.clone()),
            QueryValue::new(
                dependencies,
                ResultFingerprint::bytes(b"derived-dependencies-v1"),
            ),
        )?;
        let canonical = serde_json::to_vec(&super::syntax::without_spans(&wire))?;
        let mut encoded = vec![DERIVED_WIRE_VERSION];
        serde_json::to_writer(&mut encoded, &wire)?;
        let wire = encoded;
        let fingerprint = ResultFingerprint::bytes(&canonical);
        let value = DerivedValue {
            wire: wire.into(),
            canonical: canonical.into(),
            epoch: self.coordinate_epoch,
        };
        self.derived.insert(node.clone(), value.clone());
        self.table.read(&self.provider(), node.clone())?;
        // Also replaces an old-schema green memo without another JSON pass.
        self.table
            .refresh_value(&node, QueryValue::new(value, fingerprint))
    }
    fn capture_effect_epoch(&mut self, unit: UnitId) -> Result<()> {
        let node = InputNode::EffectRoots(unit);
        if self.captured_nodes.contains(&node) {
            return Ok(());
        }
        let changed = self
            .changed_body_units
            .get(&unit)
            .is_some_and(|ids| !ids.is_empty());
        let value = self.effect_epochs.update(unit, |epoch| {
            if changed {
                *epoch = epoch.checked_add(1).expect("effect input epoch exhausted");
            }
            *epoch
        });
        self.capture_input(
            node,
            QueryValue::new(value, ResultFingerprint::bytes(b"effect-unit-epoch-v1")),
        )
    }
    pub(crate) fn validate_effects(&mut self, unit: UnitId, _roots: Vec<BodyId>) -> Result<bool> {
        self.capture_effect_epoch(unit)?;
        match self
            .table
            .read(&self.provider(), QueryNode::EffectInventory(unit))
        {
            Ok(_) => Ok(true),
            Err(error)
                if error
                    .downcast_ref::<super::tracked::DeferredQuery>()
                    .is_some() =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
    #[cfg(test)]
    pub(crate) fn publish_effects(
        &mut self,
        unit: UnitId,
        roots: Vec<BodyId>,
        imported: Vec<(UnitId, FunctionId)>,
        inventory: HashMap<FunctionId, super::effects::CapturedEffect>,
        mut source_units: Vec<UnitId>,
        _metadata: Value,
    ) -> Result<()> {
        source_units.extend(imported.iter().map(|(unit, _)| *unit));
        source_units.sort();
        source_units.dedup();
        let mut updates: HashMap<_, _> = inventory
            .into_iter()
            .map(|(id, effect)| (id, Some(effect)))
            .collect();
        if let Some(old) = self.effect_functions.get(&unit) {
            for &id in old {
                updates.entry(id).or_insert(None);
            }
        }
        self.publish_effect_delta(unit, roots, imported, updates, source_units)
    }
    pub(crate) fn effect_dependency_reads(&self) -> usize {
        self.effect_dependency_reads.get()
    }
    pub(crate) fn effect_inventory_owners(&self) -> usize {
        self.effect_inventory_owners
    }
    pub(crate) fn has_effect_inventory(&self, unit: UnitId) -> bool {
        self.table
            .peek(&QueryNode::EffectInventory(unit))
            .is_some_and(|value| value.downcast::<EffectInventoryValue>().is_some())
    }
    pub(crate) fn effect_inventory_ids(&self, unit: UnitId) -> Vec<FunctionId> {
        self.effect_functions
            .get(&unit)
            .map(|ids| ids.iter().copied().collect())
            .unwrap_or_default()
    }
    pub(crate) fn publish_effect_delta(
        &mut self,
        unit: UnitId,
        _roots: Vec<BodyId>,
        _imported: Vec<(UnitId, FunctionId)>,
        updates: HashMap<FunctionId, Option<super::effects::CapturedEffect>>,
        source_units: Vec<UnitId>,
    ) -> Result<()> {
        self.capture_effect_epoch(unit)?;
        let mut inventory = self
            .table
            .peek(&QueryNode::EffectInventory(unit))
            .and_then(|value| value.downcast::<EffectInventoryValue>().cloned())
            .unwrap_or_default();
        let mut changed = false;
        let mut added = Vec::new();
        for (function, effect) in updates {
            self.effect_inventory_owners += 1;
            let previous = inventory.functions.get(&function);
            if previous.map(|value| value.as_ref()) == effect.as_ref() {
                continue;
            }
            changed = true;
            if let Some(previous) = previous {
                inventory.bytes -= previous.as_ref().retained_bytes();
            }
            match &effect {
                Some(effect) => {
                    let stored = std::sync::Arc::new(effect.clone());
                    inventory.bytes += stored.as_ref().retained_bytes();
                    if inventory.functions.insert(function, stored).is_none() {
                        added.push(function);
                    }
                    self.effect_functions
                        .update(unit, |functions| functions.insert(function));
                }
                None => {
                    inventory.functions.remove(&function);
                    self.effect_functions
                        .update(unit, |functions| functions.remove(&function));
                }
            }
            self.capture_input(
                InputNode::ComputedEffect(unit, function),
                QueryValue::new(effect, ResultFingerprint::bytes(b"computed-effect")),
            )?;
        }
        if changed {
            inventory.stamp = inventory
                .stamp
                .checked_add(1)
                .expect("effect stamp exhausted");
        }
        self.effects.insert(
            unit,
            PreparedEffects {
                inventory,
                source_units,
            },
        );
        self.table
            .read(&self.provider(), QueryNode::EffectInventory(unit))?;
        // Root capability memos keep their captured input alive during graph
        // compaction. Existing owners retain these nodes without being reread.
        for function in added {
            self.table.read(
                &self.provider(),
                QueryNode::EffectCapabilities(unit, function),
            )?;
        }
        Ok(())
    }
    pub(crate) fn effect_capabilities(
        &self,
        unit: UnitId,
        id: FunctionId,
    ) -> Result<super::effects::EffectCapabilities> {
        Ok(*self
            .table
            .read(&self.provider(), QueryNode::EffectCapabilities(unit, id))?
            .get::<super::effects::EffectCapabilities>())
    }
    pub(crate) fn effect_evidence(
        &self,
        unit: UnitId,
        id: FunctionId,
    ) -> Result<super::effects::EffectEvidence> {
        Ok(self
            .table
            .read(&self.provider(), QueryNode::EffectEvidence(unit, id))?
            .get::<super::effects::EffectEvidence>()
            .clone())
    }
    pub(crate) fn finish(&mut self) -> Result<(usize, usize)> {
        // Candidate metadata starts from the accepted inventories so capture
        // can publish removals. Units not captured in this revision must not
        // keep deleted classes/functions rooted during graph compaction.
        for unit in self.pending_dispatch.keys() {
            self.dispatch_classes.discard(unit);
        }
        for unit in self.pending_interface.keys() {
            self.interface_dispatch.discard(unit);
        }
        for unit in self.pending_effect.keys() {
            self.effect_functions.discard(unit);
            self.effect_epochs.discard(unit);
        }
        self.pending_dispatch.clear();
        self.pending_interface.clear();
        self.pending_effect.clear();
        let (nodes, edges) = if self.table.needs_compaction() {
            let mut index_work =
                self.dispatch_classes.values.len() + self.interface_dispatch.values.len();
            let captured = &self.captured_nodes;
            let bodies = &self.bodies;
            let classes: std::collections::HashSet<_> = self
                .dispatch_classes
                .values()
                .flatten()
                .inspect(|_| index_work += 1)
                .map(|name| crate::semantic::ids::TypeId::from_source_name(name))
                .collect();
            let interfaces: std::collections::HashSet<_> = self
                .interface_dispatch
                .values()
                .flatten()
                .inspect(|_| index_work += 1)
                .filter_map(FunctionId::owner_type)
                .collect();
            let effects = &self.effect_functions;
            let size = self.table.compact(|node| match node {
                QueryNode::EffectCapabilities(unit, id) => {
                    effects.get(unit).is_some_and(|ids| ids.contains(id))
                }
                QueryNode::ClassLayout(id) | QueryNode::GcLayout(id) => {
                    classes.contains(id) || captured.contains(&InputNode::LayoutDeclaration(*id))
                }
                QueryNode::InterfaceLayout(id) => {
                    interfaces.contains(id)
                        || captured.contains(&InputNode::InterfaceDeclaration(*id))
                }
                QueryNode::TypedBody(id)
                | QueryNode::NormalizedBody(id)
                | QueryNode::DefiniteAssignment(id)
                | QueryNode::AsyncBorrowReport(id)
                | QueryNode::ResolvedReferences(id)
                | QueryNode::DirectEffects(id)
                | QueryNode::LirBody(id)
                | QueryNode::AsyncFrameLayout(id) => bodies.contains_key(id),
                _ => false,
            });
            self.table.account_reclamation_indexes(index_work);
            size
        } else {
            self.table.graph_size()
        };
        anyhow::ensure!(nodes <= 1_000_000, "revision query node limit exceeded");
        anyhow::ensure!(
            edges <= 8_000_000,
            "revision dependency edge limit exceeded"
        );
        // Providers only need these payloads while publishing a memo. Ready
        // memos own the accepted values; a new candidate prepares fresh inputs.
        // Drop the backing tables too, rather than retaining their capacity.
        self.prepared = HashMap::new();
        self.derived = HashMap::new();
        self.effects = HashMap::new();
        self.symbols = HashMap::new();
        Ok((nodes, edges))
    }
    pub(crate) fn signature_bytes(&self, path: &Path, owner: &str) -> Result<Vec<u8>> {
        Ok(self
            .table
            .read(
                &self.provider(),
                QueryNode::SyntaxSignature(path.to_owned(), owner.to_owned()),
            )?
            .get::<CompactJson>()
            .bytes
            .to_vec())
    }

    /// Owner is a declaration path, e.g. `Function:f` or `Class:C/methods:f`.
    #[cfg(test)]
    pub(crate) fn signature(&self, path: &Path, owner: &str) -> Result<Value> {
        self.table
            .read(
                &self.provider(),
                QueryNode::SyntaxSignature(path.to_owned(), owner.to_owned()),
            )?
            .get::<CompactJson>()
            .decode()
    }
    #[cfg(test)]
    pub(crate) fn body_syntax(&self, path: &Path, owner: &str) -> Result<Value> {
        self.table
            .read(
                &self.provider(),
                QueryNode::BodySyntax(path.to_owned(), owner.to_owned()),
            )?
            .get::<CompactJson>()
            .decode()
    }
    #[cfg(test)]
    pub(crate) fn recomputations(&self, node: &QueryNode) -> usize {
        self.table.recomputations(node)
    }
    #[cfg(test)]
    pub(crate) fn query_dependencies(&self, node: &QueryNode) -> Vec<QueryNode> {
        self.table.query_dependencies(node)
    }
    pub(crate) fn retained_query_bytes(&self) -> usize {
        #[cfg(test)]
        if *crate::diagnostic_settings::RETAIN_PROFILE {
            eprintln!(
                "retained_payloads={:?} auxiliary_prepared={} auxiliary_derived={} auxiliary_effects={} auxiliary_symbols={} table={}",
                self.table.retention_profile(),
                self.prepared.heap_bytes(),
                self.derived.heap_bytes(),
                self.effects.heap_bytes(),
                self.symbols.heap_bytes(),
                self.table.retained_bytes()
            );
        }
        self.table.retained_bytes()
            + self.source_roots.heap_bytes()
            + self.owner_changes.heap_bytes()
            + self.source_body_changes.heap_bytes()
            + self.effect_epochs.heap_bytes()
            + self.interface_dispatch.heap_bytes()
            + self.dispatch_classes.heap_bytes()
            + self.captured.heap_bytes()
            + self.source_paths.heap_bytes()
            + self.bodies.heap_bytes()
            + self.prepared.heap_bytes()
            + self.symbols.heap_bytes()
            + self.effect_functions.heap_bytes()
            + self.reference_wires.heap_bytes()
            + self.reference_symbols.heap_bytes()
            + self.reference_uses.heap_bytes()
            + self.effects.heap_bytes()
            + self.derived.heap_bytes()
            + self.checked_roots.capacity() * std::mem::size_of::<BodyId>() * 2
            + self.changed_body_units.heap_bytes()
            + self.token_pairs.heap_bytes()
            + self.captured_node_bytes
            + self.captured_nodes.capacity() * std::mem::size_of::<InputNode>() * 2
    }
    pub(crate) fn verification_dump(&self) -> String {
        self.table.verification_dump()
    }
    pub(crate) fn stats(&self) -> TrackedStats {
        let mut stats = self.table.stats();
        stats.auxiliary_accounting_entries = self.source_roots.accounting
            + self.interface_dispatch.accounting
            + self.dispatch_classes.accounting
            + self.captured.accounting
            + self.source_paths.accounting
            + self.owner_changes.accounting
            + self.source_body_changes.accounting
            + self.bodies.accounting
            + self.effect_functions.accounting
            + self.effect_epochs.accounting
            + self.reference_wires.accounting
            + self.reference_symbols.values.accounting
            + self.reference_uses.values.accounting
            + self.changed_body_units.accounting
            + self.token_pairs.accounting
            + self.pending_dispatch.accounting
            + self.pending_interface.accounting
            + self.pending_effect.accounting
            + self.captured_nodes.len();
        stats
    }
    pub(crate) fn take_token_pairs(&mut self, path: &Path) -> Option<TokenPairs> {
        self.token_pairs.remove(path)
    }
    pub(crate) fn parse(
        &mut self,
        path: &Path,
        file: FileId,
        source: &str,
    ) -> Result<(Program, Vec<Diagnostic>, bool)> {
        let previous = self.table.peek(&QueryNode::Parse(path.to_owned()));
        let captured = Source {
            file,
            text: source.to_owned(),
        };
        if let Some(previous) = self.captured.get(path) {
            anyhow::ensure!(
                previous == &captured,
                "source changed during candidate evaluation: {}",
                path.display()
            );
        } else {
            let node = InputNode::Source(path.to_owned());
            let value = QueryValue::new(
                captured.clone(),
                ResultFingerprint::bytes(source.as_bytes()),
            );
            if !self.advanced && !self.table.matches_inputs(&[(node.clone(), value.clone())]) {
                self.table.advance_candidate()?;
                self.advanced = true;
            }
            self.table
                .capture_candidate_with_durability(node, value, self.source_durability(path));
            self.source_paths
                .replace(crate::module::ModuleId(file.0), path.to_owned());
            self.captured.replace(path.to_owned(), captured);
        }
        let result = self
            .table
            .read(&self.provider(), QueryNode::Parse(path.to_owned()))?;
        self.table.read(
            &self.provider(),
            QueryNode::SyntaxDeclarations(path.to_owned()),
        )?;
        self.table
            .read(&self.provider(), QueryNode::SyntaxImports(path.to_owned()))?;
        let parsed = result.get::<ParsedValue>();
        let mut owner_changes = Vec::new();
        let mut source_body_changes = Vec::new();
        if let Some(previous) = &previous {
            let old = &previous.get::<ParsedValue>().owners;
            for (owner, (signature, body)) in &parsed.owners {
                if old.get(owner).is_none_or(|(_, before)| before != body) {
                    source_body_changes.push(owner.clone());
                }
                if old.get(owner).is_none_or(|(before, _)| before != signature) {
                    owner_changes.push((owner.clone(), true));
                }
            }
            for owner in old.keys() {
                if !parsed.owners.contains_key(owner) {
                    owner_changes.push((owner.clone(), false));
                    source_body_changes.push(owner.clone());
                }
            }
        }
        if previous.is_none() {
            source_body_changes.extend(parsed.owners.keys().cloned());
        }
        if !self.source_body_changes.contains_key(path) {
            self.source_body_changes
                .replace(path.to_owned(), source_body_changes);
        }
        self.owner_changes.replace(path.to_owned(), owner_changes);
        // Parsing is the capture boundary. Publish owner projections as
        // independent inputs: a changed file must not recompute every body's
        // otherwise-equal syntax/signature query through one Parse dependency.
        for (owner, (signature, body)) in &parsed.owners {
            self.capture_input(
                InputNode::SyntaxSignature(path.to_owned(), owner.clone()),
                signature.clone().into_query(),
            )?;
            self.capture_input(
                InputNode::BodySyntax(path.to_owned(), owner.clone()),
                body.clone().into_query(),
            )?;
        }
        if let Some(previous) = &previous {
            for owner in previous.get::<ParsedValue>().owners.keys() {
                if !parsed.owners.contains_key(owner) {
                    self.capture_input(
                        InputNode::SyntaxSignature(path.to_owned(), owner.clone()),
                        value(Value::Null)?,
                    )?;
                    self.capture_input(
                        InputNode::BodySyntax(path.to_owned(), owner.clone()),
                        value(Value::Null)?,
                    )?;
                }
            }
        }
        if let Some(previous) = previous {
            let old: TokenInventory =
                serde_json::from_slice(&previous.get::<ParsedValue>().tokens)?;
            let new: TokenInventory = serde_json::from_slice(&parsed.tokens)?;
            self.token_pairs.replace(path.to_owned(), (old, new));
        }
        Ok(serde_json::from_slice(&parsed.wire)?)
    }
}

// A compiler-version-bound derived wire; bump when its payload contract changes.
// Semantic static targets are now total for checked static expressions;
// constructor summaries append base/self-use flags to the field bits.
const DERIVED_WIRE_VERSION: u8 = 3;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct DerivedValue {
    // Preparation and the memo share immutable buffers; publication is O(1)
    // beyond the single encoding/canonicalization pass.
    #[serde(with = "json_buffer")]
    wire: Rc<Vec<u8>>,
    #[serde(with = "json_buffer")]
    canonical: Rc<Vec<u8>>,
    #[serde(default)]
    epoch: u64,
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DerivedCoordinates {
    Current,
    /// The current correspondence maps these spans.
    Previous,
    /// Recompute: no single correspondence maps these spans.
    Stale,
}
impl PartialEq for DerivedValue {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}
impl Eq for DerivedValue {}

#[derive(serde::Serialize, serde::Deserialize)]
struct ParsedValue {
    #[serde(with = "json_buffer")]
    wire: Vec<u8>,
    #[serde(with = "json_buffer")]
    canonical: Vec<u8>,
    #[serde(with = "json_buffer")]
    tokens: Vec<u8>,
    declarations: CompactJson,
    imports: CompactJson,
    // Precomputed per-owner projections keep lookups independent of file size.
    owners: HashMap<String, (CompactJson, CompactJson)>,
    #[serde(default)]
    owner_spans: HashMap<String, Span>,
}
impl PartialEq for ParsedValue {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}
impl Eq for ParsedValue {}

#[derive(Clone)]
struct BodyBinding {
    unit: UnitId,
    path: PathBuf,
    owner: String,
}
struct PreparedBody {
    result: CompactJson,
    keys: Vec<String>,
    children: Vec<BodyId>,
}
// Every lambda binds to its enclosing declaration's BodySyntax. Copying that
// entire syntax into every typed result costs O(body count * declaration size).
// The table-local exact change stamp preserves syntax invalidation in O(1)
// space per body, without relying on a hash for equality or retaining old trees.
#[derive(PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct TypedBodyValue {
    syntax_node: QueryNode,
    syntax_revision: super::tracked::Revision,
    result: CompactJson,
}
impl Retained for TypedBodyValue {
    fn heap_bytes(&self) -> usize {
        self.syntax_node.heap_bytes() + self.result.heap_bytes()
    }
}

struct PreparedEffects {
    inventory: EffectInventoryValue,
    source_units: Vec<UnitId>,
}
#[derive(Clone, Default)]
struct EffectInventoryValue {
    functions: im::OrdMap<FunctionId, std::sync::Arc<super::effects::CapturedEffect>>,
    // Advanced only after exact per-owner comparisons find a changed value.
    stamp: u64,
    bytes: usize,
}
impl PartialEq for EffectInventoryValue {
    fn eq(&self, other: &Self) -> bool {
        self.stamp == other.stamp
    }
}
impl Eq for EffectInventoryValue {}
struct SyntaxProvider<'a> {
    source_paths: &'a InventoryMap<UnitId, PathBuf>,
    interface_dispatch: &'a InventoryMap<UnitId, Vec<FunctionId>>,
    bodies: &'a InventoryMap<BodyId, BodyBinding>,
    prepared: &'a HashMap<BodyId, PreparedBody>,
    effects: &'a HashMap<UnitId, PreparedEffects>,
    effect_dependency_reads: &'a std::cell::Cell<usize>,
    derived: &'a HashMap<QueryNode, DerivedValue>,
}
/// Immutable canonical JSON without the per-field allocation overhead of Value.
/// Encoding happens once; equality and fingerprints compare the same bytes.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct CompactJson {
    #[serde(with = "json_buffer")]
    bytes: std::sync::Arc<[u8]>,
}

// These buffers are produced by serde_json, hence UTF-8. A JSON string keeps
// their exact bytes (and equality/fingerprint semantics) without expanding each
// byte into a numeric JSON value during persistence.
mod json_buffer {
    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        let text = std::str::from_utf8(bytes).map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(text)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>, T: From<Vec<u8>>>(
        deserializer: D,
    ) -> Result<T, D::Error> {
        Ok(String::deserialize(deserializer)?.into_bytes().into())
    }
}
impl CompactJson {
    fn new(value: &Value) -> Result<Self> {
        Ok(Self::from_bytes(serde_json::to_vec(value)?))
    }
    fn from_bytes(bytes: Vec<u8>) -> Self {
        Self {
            bytes: bytes.into(),
        }
    }
    fn decode<T: serde::de::DeserializeOwned>(&self) -> Result<T> {
        Ok(serde_json::from_slice(&self.bytes)?)
    }
    fn into_query(self) -> QueryValue {
        let fingerprint = ResultFingerprint::bytes(&self.bytes);
        QueryValue::new(self, fingerprint)
    }
}
impl Retained for CompactJson {
    fn heap_bytes(&self) -> usize {
        2 * std::mem::size_of::<usize>() + self.bytes.len()
    }
}
pub(crate) fn value(value: Value) -> Result<QueryValue> {
    Ok(CompactJson::new(&value)?.into_query())
}
/// Exclude executable trees before semantic canonicalization, avoiding scans of
/// bodies in every declaration projection. Initializers remain declarations:
/// inferred field types and constant values can affect declaration semantics.
fn declarations(mut syntax: Value) -> Value {
    fn strip(value: &mut Value) {
        match value {
            Value::Object(values) => {
                values.remove("body");
                values.remove("default_body");
                for value in values.values_mut() {
                    strip(value);
                }
            }
            Value::Array(values) => {
                for value in values {
                    strip(value);
                }
            }
            _ => {}
        }
    }
    strip(&mut syntax);
    super::syntax::semantic(&syntax)
}
// One inventory per parse; projection lookups never scan the file again.
fn index_declarations(items: &Value, prefix: &str, index: &mut serde_json::Map<String, Value>) {
    let Some(items) = items.as_array() else {
        return;
    };
    for (position, item) in items.iter().enumerate() {
        let Some(object) = item.as_object() else {
            continue;
        };
        let (kind, declaration) = if object.len() == 1 {
            let (kind, value) = object.iter().next().unwrap();
            (kind.as_str(), value)
        } else {
            ("", item)
        };
        let fallback = position.to_string();
        let name = declaration
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&fallback);
        let owner = format!("{prefix}{kind}:{name}");
        let body = declaration
            .get("body")
            .or_else(|| declaration.get("default_body"))
            .or_else(|| declaration.get("initializer"))
            .cloned()
            .unwrap_or(Value::Null);
        index.insert(owner.clone(), serde_json::json!({"signature": declarations(declaration.clone()), "body": super::syntax::semantic(&body), "span": declaration.get("span")}));
        for field in ["methods", "static_methods", "constructors", "fields"] {
            index_declarations(&declaration[field], &format!("{owner}/{field}"), index);
        }
    }
}

impl QueryProvider for SyntaxProvider<'_> {
    fn contains_body(&self, body: BodyId) -> bool {
        self.bodies.contains_key(&body)
    }
    fn compute(&self, table: &TrackedQueryTable, node: &QueryNode) -> Result<QueryValue> {
        match node {
            QueryNode::CompilerEnvironment => {
                let compiler = table.input(&InputNode::CompilerStamp)?;
                let stdlib = table.input(&InputNode::StdlibStamp)?;
                let runtime = table.input(&InputNode::RuntimeAbiRevision)?;
                value(serde_json::json!([
                    compiler.get::<CompactJson>().decode::<Value>()?,
                    stdlib.get::<CompactJson>().decode::<Value>()?,
                    runtime.get::<CompactJson>().decode::<Value>()?
                ]))
            }
            QueryNode::Parse(path) => {
                table.read(self, QueryNode::CompilerEnvironment)?;
                let input = table.input(&InputNode::Source(path.clone()))?;
                let source = input.get::<Source>();
                let mut inventory = Vec::new();
                let (program, diagnostics, lexer_failed) =
                    match crate::lexer::Lexer::with_file_id(&source.text, source.file).tokenize() {
                        Ok(tokens) => {
                            inventory = tokens
                                .iter()
                                .map(|token| {
                                    (
                                        source
                                            .text
                                            .get(token.span.start..token.span.end)
                                            .unwrap_or("")
                                            .to_owned(),
                                        token.span,
                                    )
                                })
                                .collect();
                            let (program, diagnostics) = crate::parser::Parser::new(tokens).parse();
                            (program, diagnostics, false)
                        }
                        Err(diagnostics) => (
                            Program {
                                instantiation_types: Vec::new(),
                                type_uses: vec![],
                                module: None,
                                imports: vec![],
                                items: vec![],
                            },
                            diagnostics.into_iter().collect::<Vec<_>>(),
                            true,
                        ),
                    };
                let parsed = serde_json::to_value((program, diagnostics, lexer_failed))?;
                let mut index = serde_json::Map::new();
                index_declarations(&parsed[0]["items"], "", &mut index);
                let owner_spans = index
                    .iter()
                    .filter_map(|(owner, projection)| {
                        serde_json::from_value::<Span>(projection["span"].clone())
                            .ok()
                            .map(|span| (owner.clone(), span))
                    })
                    .collect();
                let owners = index
                    .into_iter()
                    .map(|(owner, projection)| {
                        Ok((
                            owner,
                            (
                                CompactJson::new(&projection["signature"])?,
                                CompactJson::new(&projection["body"])?,
                            ),
                        ))
                    })
                    .collect::<Result<_>>()?;
                // The owner index is derived solely from the program, so it
                // need not be duplicated in the semantic comparison payload.
                let canonical =
                    serde_json::to_vec(&(super::syntax::without_ids(&parsed), &inventory))?;
                let fingerprint = ResultFingerprint::bytes(&canonical);
                Ok(QueryValue::new(
                    ParsedValue {
                        wire: serde_json::to_vec(&parsed)?,
                        canonical,
                        tokens: serde_json::to_vec(&inventory)?,
                        declarations: CompactJson::new(&declarations(parsed[0]["items"].clone()))?,
                        imports: CompactJson::new(&super::syntax::semantic(&parsed[0]["imports"]))?,
                        owners,
                        owner_spans,
                    },
                    fingerprint,
                ))
            }
            QueryNode::SyntaxDeclarations(path) | QueryNode::SyntaxImports(path) => {
                let parsed = table.read(self, QueryNode::Parse(path.clone()))?;
                let parsed = parsed.get::<ParsedValue>();
                if matches!(node, QueryNode::SyntaxImports(_)) {
                    Ok(parsed.imports.clone().into_query())
                } else {
                    Ok(parsed.declarations.clone().into_query())
                }
            }
            QueryNode::SyntaxSignature(path, owner) | QueryNode::BodySyntax(path, owner) => {
                let input = if matches!(node, QueryNode::BodySyntax(_, _)) {
                    InputNode::BodySyntax(path.clone(), owner.clone())
                } else {
                    InputNode::SyntaxSignature(path.clone(), owner.clone())
                };
                if table.contains_input(&input) {
                    return table.input(&input);
                }
                // An owner never observed in this file still has to track its
                // possible future addition. A captured removal uses Null above.
                let parsed = table.read(self, QueryNode::Parse(path.clone()))?;
                match parsed.get::<ParsedValue>().owners.get(owner) {
                    Some((signature, body)) => {
                        Ok((if matches!(node, QueryNode::BodySyntax(_, _)) {
                            body
                        } else {
                            signature
                        })
                        .clone()
                        .into_query())
                    }
                    None => value(Value::Null),
                }
            }
            QueryNode::NormalizedBody(_)
            | QueryNode::DirectEffects(_)
            | QueryNode::DefiniteAssignment(_)
            | QueryNode::AsyncBorrowReport(_)
            | QueryNode::ResolvedReferences(_)
            | QueryNode::LirBody(_)
            | QueryNode::LirUnit(_)
            | QueryNode::AsyncFrameLayout(_) => {
                let Some(prepared) = self.derived.get(node) else {
                    return Err(super::tracked::DeferredQuery(node.clone()).into());
                };
                let dependencies = table.input(&InputNode::DerivedDependencies(node.clone()))?;
                for dependency in dependencies.get::<Vec<QueryNode>>() {
                    table.read(self, dependency.clone())?;
                }
                let fingerprint = ResultFingerprint::bytes(&prepared.canonical);
                Ok(QueryValue::new(prepared.clone(), fingerprint))
            }
            QueryNode::EffectInventory(unit) => {
                let Some(prepared) = self.effects.get(unit) else {
                    return Err(super::tracked::DeferredQuery(node.clone()).into());
                };
                self.effect_dependency_reads.set(
                    self.effect_dependency_reads.get()
                        + 1
                        + usize::from(self.source_paths.contains_key(unit))
                        + self.interface_dispatch.get(unit).map_or(0, Vec::len)
                        + prepared.source_units.len(),
                );
                table.input(&InputNode::EffectRoots(*unit))?;
                // Source capture establishes the current unit inventory. Body
                // journals separately force completion after type-only changes;
                // no per-function dependency inventory is rebuilt here.
                if let Some(path) = self.source_paths.get(unit) {
                    table.read(self, QueryNode::Parse(path.clone()))?;
                }
                for &id in self.interface_dispatch.get(unit).into_iter().flatten() {
                    table.read(self, QueryNode::InterfaceDispatchTargets(*unit, id))?;
                }
                for unit in &prepared.source_units {
                    table.read(self, QueryNode::EffectInventory(*unit))?;
                }
                Ok(QueryValue::new(
                    prepared.inventory.clone(),
                    ResultFingerprint::bytes(b"effect-inventory-v1"),
                ))
            }
            QueryNode::EffectCapabilities(unit, function)
            | QueryNode::EffectEvidence(unit, function) => {
                let captured = table.input(&InputNode::ComputedEffect(*unit, *function))?;
                let effect = captured
                    .get::<Option<super::effects::CapturedEffect>>()
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("unknown effect function {function:?}"))?;
                if matches!(node, QueryNode::EffectCapabilities(_, _)) {
                    Ok(QueryValue::new(
                        effect.capabilities,
                        ResultFingerprint::bytes(b"effect-capability-v1"),
                    ))
                } else {
                    Ok(QueryValue::new(
                        effect.evidence.clone(),
                        ResultFingerprint::bytes(b"effect-evidence-v1"),
                    ))
                }
            }
            QueryNode::ResolvedReference(id) => {
                table.input(&InputNode::ResolvedReference(id.clone()))
            }
            QueryNode::SymbolReferences(symbol) => {
                let members = table.input(&InputNode::ReferenceMembers(symbol.clone()))?;
                let mut references = Vec::new();
                for id in members.get::<Vec<super::references::SymbolUseId>>() {
                    references.push((
                        id.clone(),
                        table
                            .read(self, QueryNode::ResolvedReference(id.clone()))?
                            .get::<CompactJson>()
                            .clone(),
                    ));
                }
                Ok(QueryValue::new(
                    references,
                    ResultFingerprint::bytes(b"symbol-references"),
                ))
            }
            QueryNode::InterfaceDispatchTargets(unit, id) => {
                table.input(&InputNode::InterfaceDispatch(*unit, *id))
            }
            QueryNode::DispatchTargets(..) => {
                super::dispatch::DispatchProvider.compute(table, node)
            }
            QueryNode::SemanticSignature(unit, key) => {
                if let Ok(crate::semantic::symbols::SymbolRead::Dispatch(class, method)) =
                    serde_json::from_str(key)
                {
                    return table.read(self, QueryNode::DispatchTargets(*unit, class, method));
                }
                table.input(&InputNode::SemanticSymbol(*unit, key.clone()))
            }
            QueryNode::VisibleScope(unit, path) => {
                table.read(self, QueryNode::SyntaxDeclarations(path.clone()))?;
                table.read(self, QueryNode::SyntaxImports(path.clone()))?;
                table.input(&InputNode::VisibleDeclarations(*unit))
            }
            QueryNode::TypedBody(body) => {
                let binding = self
                    .bodies
                    .get(body)
                    .ok_or_else(|| anyhow::anyhow!("unregistered body {body:?}"))?;
                let Some(prepared) = self.prepared.get(body) else {
                    return Err(super::tracked::DeferredQuery(node.clone()).into());
                };
                let syntax_node =
                    QueryNode::BodySyntax(binding.path.clone(), binding.owner.clone());
                let (_, syntax_revision) = table.read_stamped(self, syntax_node.clone())?;
                table.read(
                    self,
                    QueryNode::SyntaxSignature(binding.path.clone(), binding.owner.clone()),
                )?;
                for key in &prepared.keys {
                    table.read(
                        self,
                        QueryNode::SemanticSignature(binding.unit, key.clone()),
                    )?;
                }
                for child in &prepared.children {
                    table.read(self, QueryNode::TypedBody(*child))?;
                }
                Ok(QueryValue::new(
                    TypedBodyValue {
                        syntax_node,
                        syntax_revision,
                        result: prepared.result.clone(),
                    },
                    ResultFingerprint::bytes(&prepared.result.bytes),
                ))
            }
            _ => anyhow::bail!("unsupported syntax query {node:?}"),
        }
    }
}

impl Retained for Source {
    fn heap_bytes(&self) -> usize {
        self.text.heap_bytes()
    }
}
impl Retained for BodyBinding {
    fn heap_bytes(&self) -> usize {
        self.path.heap_bytes() + self.owner.heap_bytes()
    }
}
impl Retained for PreparedBody {
    fn heap_bytes(&self) -> usize {
        self.result.heap_bytes() + self.keys.heap_bytes() + self.children.heap_bytes()
    }
}
impl Retained for PreparedEffects {
    fn heap_bytes(&self) -> usize {
        self.inventory.heap_bytes() + self.source_units.heap_bytes()
    }
}
impl Retained for ParsedValue {
    fn heap_bytes(&self) -> usize {
        self.wire.capacity()
            + self.canonical.capacity()
            + self.tokens.capacity()
            + self.declarations.heap_bytes()
            + self.imports.heap_bytes()
            + self.owners.heap_bytes()
            + self.owner_spans.heap_bytes()
    }
}
impl Retained for DerivedValue {
    fn heap_bytes(&self) -> usize {
        self.wire.capacity() + self.canonical.capacity()
    }
}
impl Retained for EffectInventoryValue {
    fn heap_bytes(&self) -> usize {
        self.bytes + self.functions.len() * 64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn derived_typed_wire_ownership_schema_and_green_hits() {
        for size in [0, 1, 8, 64, 256] {
            for shape in ["empty", "flat", "nested", "strings"] {
                let mut queries = SyntaxQueries::default();
                let node = QueryNode::LirUnit(UnitId::ENTRY);
                let payload = match shape {
                    "empty" => serde_json::json!([]),
                    "flat" => serde_json::json!((0..size).collect::<Vec<_>>()),
                    "nested" => {
                        serde_json::json!((0..size).map(|n| vec![n, n]).collect::<Vec<_>>())
                    }
                    _ => serde_json::json!(vec!["repeated"; size]),
                };
                queries
                    .publish_derived(node.clone(), payload.clone(), vec![])
                    .unwrap();
                let stats = queries.stats();
                let mut decoded: Value = queries
                    .validate_derived_typed(node.clone(), vec![])
                    .unwrap()
                    .unwrap();
                assert_eq!(decoded, payload);
                if let Some(array) = decoded.as_array_mut() {
                    array.push(Value::Null);
                }
                assert_eq!(
                    queries.validate_derived(node.clone(), vec![]).unwrap(),
                    Some(payload.clone())
                );
                assert_eq!(queries.stats().recomputed, stats.recomputed);
                let memo = queries.table.peek(&node).unwrap();
                let old = memo.get::<DerivedValue>();
                let canonical = old.canonical.clone();
                for wire in [old.wire[1..].to_vec(), {
                    let mut wire = old.wire.as_ref().clone();
                    wire[0] = 0;
                    wire
                }] {
                    queries
                        .table
                        .refresh_value(
                            &node,
                            QueryValue::new(
                                DerivedValue {
                                    wire: wire.into(),
                                    canonical: canonical.clone(),
                                    epoch: 0,
                                },
                                ResultFingerprint::bytes(&canonical),
                            ),
                        )
                        .unwrap();
                    assert!(
                        queries
                            .validate_derived(node.clone(), vec![])
                            .unwrap()
                            .is_none()
                    );
                    queries
                        .publish_derived(node.clone(), payload.clone(), vec![])
                        .unwrap();
                    assert_eq!(
                        queries.validate_derived(node.clone(), vec![]).unwrap(),
                        Some(payload.clone())
                    );
                }
                queries.finish().unwrap();
                assert_eq!(
                    queries.validate_derived(node, vec![]).unwrap(),
                    Some(payload)
                );
            }
        }
    }

    #[test]
    fn effect_publication_delta_retains_unread_inputs_and_scales_by_changed_owner() {
        use super::super::effects::{CapturedEffect, EffectCapabilities, EffectEvidence};
        use crate::semantic::effects::{EffectSummary, RuntimeEffects};
        use std::sync::Arc;
        let capture = |io| CapturedEffect {
            capabilities: EffectCapabilities {
                runtime: RuntimeEffects::NONE,
                may_io: io,
            },
            evidence: EffectEvidence {
                summary: Arc::new(EffectSummary::default()),
            },
            helper: None,
            loop_bodies: Vec::new(),
        };
        for size in [8, 32, 128] {
            let mut first = SyntaxQueries::default().candidate().unwrap();
            let ids: Vec<_> = (0..size)
                .map(|i| FunctionId::free(format!("item_{i}")))
                .collect();
            first
                .publish_effect_delta(
                    UnitId::ENTRY,
                    vec![],
                    vec![],
                    ids.iter().map(|&id| (id, Some(capture(false)))).collect(),
                    vec![],
                )
                .unwrap();
            first.finish().unwrap();
            let before = first
                .table
                .peek(&QueryNode::EffectInventory(UnitId::ENTRY))
                .unwrap();
            let untouched = before.get::<EffectInventoryValue>().functions[&ids[1]].clone();
            let mut next = first.candidate().unwrap();
            next.changed_body_units
                .update(UnitId::ENTRY, |bodies| bodies.insert(BodyId::fresh()));
            assert!(!next.validate_effects(UnitId::ENTRY, vec![]).unwrap());
            next.publish_effect_delta(
                UnitId::ENTRY,
                vec![],
                vec![],
                [(ids[0], Some(capture(true)))].into(),
                vec![],
            )
            .unwrap();
            assert_eq!(next.effect_inventory_owners(), 1);
            assert_eq!(next.effect_dependency_reads(), 1);
            next.finish().unwrap();
            assert!(
                !next
                    .effect_capabilities(UnitId::ENTRY, ids[size - 1])
                    .unwrap()
                    .may_io
            );
            assert!(
                next.effect_capabilities(UnitId::ENTRY, ids[0])
                    .unwrap()
                    .may_io
            );
            let after = next
                .table
                .peek(&QueryNode::EffectInventory(UnitId::ENTRY))
                .unwrap();
            assert!(Arc::ptr_eq(
                &untouched,
                &after.get::<EffectInventoryValue>().functions[&ids[1]]
            ));
            assert_eq!(before.get::<EffectInventoryValue>().functions.len(), size);
        }
    }

    #[test]
    fn effect_inventory_byte_accounting_tracks_stored_vector_capacity() {
        use super::super::effects::{CapturedEffect, EffectCapabilities, EffectEvidence};
        use crate::semantic::effects::{EffectSummary, RuntimeEffects};
        use std::sync::Arc;
        let id = FunctionId::free("value");
        let body = BodyId::fresh();
        let mut accepted = SyntaxQueries::default();
        for step in 0..9 {
            let mut next = accepted.candidate().unwrap();
            next.changed_body_units
                .update(UnitId::ENTRY, |bodies| bodies.insert(body));
            let mut loops = Vec::with_capacity(32);
            loops.push(body);
            let effect = (step < 8).then(|| CapturedEffect {
                capabilities: EffectCapabilities {
                    runtime: RuntimeEffects::NONE,
                    may_io: step % 2 == 0,
                },
                evidence: EffectEvidence {
                    summary: Arc::new(EffectSummary::default()),
                },
                helper: None,
                loop_bodies: loops,
            });
            next.publish_effect_delta(UnitId::ENTRY, vec![], vec![], [(id, effect)].into(), vec![])
                .unwrap();
            let value = next
                .table
                .peek(&QueryNode::EffectInventory(UnitId::ENTRY))
                .unwrap();
            let inventory = value.get::<EffectInventoryValue>();
            assert_eq!(
                inventory.bytes,
                inventory
                    .functions
                    .values()
                    .map(|v| v.as_ref().retained_bytes())
                    .sum::<usize>()
            );
            if step == 8 {
                assert_eq!(inventory.bytes, 0);
            }
            next.finish().unwrap();
            accepted = next;
        }
    }

    #[test]
    fn finish_prunes_inventory_for_units_absent_from_candidate() {
        let mut queries = SyntaxQueries::default();
        queries
            .dispatch_classes
            .insert(UnitId::ENTRY, ["Removed".into()].into());
        queries.interface_dispatch.insert(UnitId::ENTRY, vec![]);
        queries
            .effect_functions
            .insert(UnitId::ENTRY, Default::default());
        let mut candidate = queries.candidate().unwrap();
        candidate.finish().unwrap();
        assert!(candidate.dispatch_classes.is_empty());
        assert!(candidate.interface_dispatch.is_empty());
        assert!(candidate.effect_functions.is_empty());
        assert_eq!(candidate.dispatch_classes.capacity(), 0);
        assert_eq!(candidate.interface_dispatch.capacity(), 0);
        assert_eq!(candidate.effect_functions.capacity(), 0);
        // Finishing an unaccepted candidate must not mutate accepted inventory.
        assert_eq!(queries.dispatch_classes.len(), 1);
        assert_eq!(queries.interface_dispatch.len(), 1);
        assert_eq!(queries.effect_functions.len(), 1);
    }
    #[test]
    fn finish_releases_preparation_and_preserves_late_query_reads() {
        let path = Path::new("main.wi");
        let mut queries = SyntaxQueries::default().candidate().unwrap();
        queries.parse(path, FileId::ENTRY, "fn main() {}").unwrap();
        let body = BodyId::fresh();
        queries.register_body(UnitId::ENTRY, body, path, "Function:main");
        queries
            .publish_body(
                UnitId::ENTRY,
                body,
                serde_json::json!({"typed": true}),
                vec![],
                vec![],
            )
            .unwrap();
        let node = QueryNode::NormalizedBody(body);
        let wire = serde_json::json!({"body": [], "metadata": "retained"});
        let dependencies = vec![QueryNode::TypedBody(body)];
        queries
            .publish_derived(node.clone(), wire.clone(), dependencies.clone())
            .unwrap();
        let before = queries.retained_query_bytes();
        queries.finish().unwrap();
        assert!(
            queries.prepared.is_empty()
                && queries.derived.is_empty()
                && queries.effects.is_empty()
                && queries.symbols.is_empty()
        );
        assert!(queries.retained_query_bytes() < before);
        let stats = queries.stats();
        assert_eq!(
            queries
                .validate_derived(node.clone(), dependencies)
                .unwrap(),
            Some(wire)
        );
        assert_eq!(queries.stats().recomputed, stats.recomputed);
        let late = QueryNode::LirBody(body);
        queries
            .publish_derived(
                late.clone(),
                serde_json::json!(["late"]),
                vec![node.clone()],
            )
            .unwrap();
        assert_eq!(
            queries.validate_derived(late, vec![node]).unwrap(),
            Some(serde_json::json!(["late"]))
        );
    }
    #[test]
    fn compact_parse_preserves_artifact_deserialization_depth() {
        let mut accepted = 0;
        for depth in 1..80 {
            let source = format!(
                "fn f(x: {}i64{}) {{}}",
                "Array<".repeat(depth),
                ">".repeat(depth)
            );
            let tokens = crate::lexer::Lexer::new(&source).tokenize().unwrap();
            let (program, diagnostics) = crate::parser::Parser::new(tokens).parse();
            assert!(diagnostics.is_empty());
            let wire = serde_json::to_vec(&program).unwrap();
            if serde_json::from_slice::<Program>(&wire).is_err() {
                continue;
            }
            accepted += 1;
            let mut queries = SyntaxQueries::default().candidate().unwrap();
            queries
                .parse(Path::new("nested.wi"), FileId::ENTRY, &source)
                .unwrap_or_else(|error| panic!("depth={depth}: {error}"));
        }
        assert!(accepted > 10);
    }
    #[test]
    fn compact_json_roundtrips() {
        let cases = [
            Value::Null,
            serde_json::json!(true),
            serde_json::json!(1),
            serde_json::json!(1.5),
            serde_json::json!("quote\"\\\n日本語"),
            serde_json::json!([1, false, null, {"nested": ["x", "y"]}]),
            serde_json::json!({"b": 2, "a": 1}),
        ];
        for a in &cases {
            let encoded = CompactJson::new(a).unwrap();
            assert_eq!(encoded.decode::<Value>().unwrap(), *a);
        }
    }
    #[test]
    fn unchecked_template_source_body_journal_and_current_span() {
        let path = Path::new("template.wi");
        let initial = "fn identity<T>(x:T)->T { return x; } fn main() {}";
        let edited = "// shifted\nfn identity<T>(x:T)->T { println(1); return x; } fn main() {}";
        let mut accepted = SyntaxQueries::default().candidate().unwrap();
        accepted.parse(path, FileId::ENTRY, initial).unwrap();
        let mut next = accepted.candidate().unwrap();
        next.parse(path, FileId::ENTRY, edited).unwrap();
        assert_eq!(
            next.source_body_changes(UnitId::ENTRY),
            vec!["Function:identity"]
        );
        assert!(next.changed_bodies_in(UnitId::ENTRY).is_empty());
        assert_eq!(
            next.source_owner_span(UnitId::ENTRY, "Function:identity")
                .unwrap()
                .start,
            edited.find("fn identity").unwrap()
        );
        next.parse(path, FileId::ENTRY, edited).unwrap();
        assert_eq!(
            next.source_body_changes(UnitId::ENTRY),
            vec!["Function:identity"]
        );
        let mut moved = next.candidate().unwrap();
        let moved_source = format!("\n{edited}");
        moved.parse(path, FileId::ENTRY, &moved_source).unwrap();
        assert!(moved.source_body_changes(UnitId::ENTRY).is_empty());
        assert_eq!(
            moved
                .source_owner_span(UnitId::ENTRY, "Function:identity")
                .unwrap()
                .start,
            moved_source.find("fn identity").unwrap()
        );
        let mut removed = moved.candidate().unwrap();
        removed.parse(path, FileId::ENTRY, "fn main() {}").unwrap();
        assert_eq!(
            removed.source_body_changes(UnitId::ENTRY),
            vec!["Function:identity"]
        );
        assert!(
            removed
                .source_owner_span(UnitId::ENTRY, "Function:identity")
                .is_none()
        );
    }

    #[test]
    fn owner_projection_recomputations_ignore_unrelated_functions() {
        for count in [8, 32, 128] {
            let path = Path::new("owners.wi");
            let unrelated: String = (0..count)
                .map(|i| format!("fn function_{i}() -> i64 {{ return {i}; }}\n"))
                .collect();
            let source = format!("fn edited() -> i64 {{ return 1; }} {unrelated}");
            let mut first = SyntaxQueries::default().candidate().unwrap();
            first.parse(path, FileId::ENTRY, &source).unwrap();
            let owners: Vec<_> = std::iter::once("Function:edited".to_owned())
                .chain((0..count).map(|i| format!("Function:function_{i}")))
                .collect();
            for owner in &owners {
                first.body_syntax(path, owner).unwrap();
                first.signature(path, owner).unwrap();
            }
            first.finish().unwrap();
            let mut next = first.candidate().unwrap();
            next.parse(
                path,
                FileId::ENTRY,
                &source.replacen("return 1;", "return 20;", 1),
            )
            .unwrap();
            for owner in &owners {
                next.body_syntax(path, owner).unwrap();
                next.signature(path, owner).unwrap();
            }
            assert_eq!(next.stats().recomputed, 4, "unrelated={count}");
            assert_eq!(
                next.recomputations(&QueryNode::BodySyntax(
                    path.into(),
                    "Function:edited".into()
                )),
                1
            );
            for owner in &owners[1..] {
                assert_eq!(
                    next.recomputations(&QueryNode::BodySyntax(path.into(), owner.clone())),
                    0
                );
                assert_eq!(
                    next.recomputations(&QueryNode::SyntaxSignature(path.into(), owner.clone())),
                    0
                );
            }
        }
    }

    #[test]
    fn owner_projection_missing_add_remove_and_disk_roundtrip() {
        let path = Path::new("missing-owner.wi");
        let owner = "Function:later";
        let mut current = SyntaxQueries::default().candidate().unwrap();
        for source in [
            "fn main() {}",
            "fn later() -> i64 { return 1; } fn main() {}",
            "\nfn later() -> i64 { return 1; } fn main() {}",
            "fn main() {}",
            "fn main() {}",
            "fn later() -> bool { return true; } fn main() {}",
        ] {
            let mut next = current.candidate().unwrap();
            next.parse(path, FileId::ENTRY, source).unwrap();
            let body = next.body_syntax(path, owner).unwrap();
            let signature = next.signature(path, owner).unwrap();
            assert_eq!(body.is_null(), !source.contains("fn later"));
            assert_eq!(signature.is_null(), !source.contains("fn later"));
            next.finish().unwrap();
            let wire = serde_json::to_vec(&next.disk_snapshot().unwrap()).unwrap();
            current = SyntaxQueries::from_disk(serde_json::from_slice(&wire).unwrap()).unwrap();
        }
    }

    #[test]
    fn compact_parse_storage_and_projection_work_scale_with_owners() {
        let mut previous = None;
        for count in [16, 64, 256] {
            let mut queries = SyntaxQueries::default().candidate().unwrap();
            let source: String = (0..count)
                .map(|i| format!("fn function_{i}() -> i64 {{ return {i}; }}\n"))
                .collect();
            let path = Path::new("compact.wi");
            queries.parse(path, FileId::ENTRY, &source).unwrap();
            let result = queries.table.peek(&QueryNode::Parse(path.into())).unwrap();
            let parsed = result.get::<ParsedValue>();
            assert_eq!(parsed.owners.len(), count);
            let bytes = parsed.retained_bytes();
            // Bounded serialized storage per generated owner, including tokens,
            // diagnostics and the separately indexed body/signature projections.
            assert!(bytes < 16_384 * count);
            if let Some(old) = previous {
                assert!(bytes < old * 5);
            }
            previous = Some(bytes);
            let before = queries.stats();
            for i in 0..count {
                queries
                    .body_syntax(path, &format!("Function:function_{i}"))
                    .unwrap();
                queries
                    .signature(path, &format!("Function:function_{i}"))
                    .unwrap();
            }
            assert_eq!(queries.stats().recomputed - before.recomputed, 2 * count);
            assert_eq!(
                queries.stats().dependency_reads - before.dependency_reads,
                2 * count
            );
            let projection = queries
                .table
                .peek(&QueryNode::BodySyntax(
                    path.into(),
                    "Function:function_0".into(),
                ))
                .unwrap();
            assert!(
                std::sync::Arc::ptr_eq(
                    &parsed.owners["Function:function_0"].1.bytes,
                    &projection.get::<CompactJson>().bytes,
                ),
                "publishing a projection must share its encoded payload"
            );
            println!(
                "compact_parse owners={count} retained_bytes={bytes} projection_reads={}",
                2 * count
            );
        }
    }
    #[test]
    fn retained_accounting_includes_auxiliary_payload_capacity() {
        let mut queries = SyntaxQueries::default();
        let empty = queries.retained_query_bytes();
        let mut source = String::with_capacity(65_536);
        source.push_str("fn main() {}");
        queries.captured.insert(
            PathBuf::from("main.wi"),
            Source {
                file: FileId::ENTRY,
                text: source,
            },
        );
        assert!(queries.retained_query_bytes() >= empty + 65_536);
        let captured = queries.retained_query_bytes();
        queries.prepared.insert(
            BodyId::fresh(),
            PreparedBody {
                result: CompactJson::new(&Value::String("x".repeat(32_768))).unwrap(),
                keys: Vec::with_capacity(128),
                children: Vec::new(),
            },
        );
        assert!(
            queries.retained_query_bytes()
                >= captured + 32_768 + 128 * std::mem::size_of::<String>()
        );
    }
    #[test]
    fn immutable_package_durability_requires_git_revision_and_checksum() {
        use super::super::tracked::Durability;
        use crate::package::{
            PackageGraph, PackageId, PackageIdentity, PackageSourceIdentity, ResolvedPackage,
        };
        for git in [false, true] {
            for revision in [false, true] {
                for checksum in [false, true] {
                    let root = PathBuf::from("dependencies/pkg");
                    let package = ResolvedPackage {
                        id: PackageId(0),
                        root: root.clone(),
                        dependencies: vec![],
                        checksum: checksum.then(|| "checksum".into()),
                        identity: PackageIdentity {
                            name: "pkg".into(),
                            version: "1.0.0".into(),
                            source: if git {
                                PackageSourceIdentity::Git {
                                    url: "https://example.invalid/pkg".into(),
                                }
                            } else {
                                PackageSourceIdentity::Path { path: root.clone() }
                            },
                            revision: revision.then(|| "revision".into()),
                        },
                    };
                    let graph = PackageGraph {
                        native: None,
                        root: PackageId(0),
                        packages: vec![package],
                        stats: Default::default(),
                    };
                    let mut queries = SyntaxQueries::default();
                    queries.configure_sources(Some(&graph));
                    assert_eq!(
                        queries.source_durability(&root.join("module.wi")),
                        if git && revision && checksum {
                            Durability::High
                        } else {
                            Durability::Low
                        }
                    );
                    assert_eq!(
                        queries.source_durability(Path::new("main.wi")),
                        Durability::Low
                    );
                }
            }
        }
    }
    #[test]
    fn compiler_environment_is_high_and_skips_edges_after_source_edit() {
        let mut queries = SyntaxQueries::default().candidate().unwrap();
        queries
            .parse(
                Path::new("main.wi"),
                FileId::ENTRY,
                "fn main() { println(1); }",
            )
            .unwrap();
        let mut next = queries.candidate().unwrap();
        next.parse(
            Path::new("main.wi"),
            FileId::ENTRY,
            "fn main() { println(2); }",
        )
        .unwrap();
        assert!(next.stats().durability_shortcuts > 0);
        assert_eq!(next.recomputations(&QueryNode::CompilerEnvironment), 0);
    }
    #[test]
    fn review_reference_locations_refresh_without_semantic_recomputation() {
        use super::super::references::{SymbolId, SymbolUseId};
        let symbol = SymbolId("f".into());
        let id = SymbolUseId {
            owner: SymbolId("g".into()),
            ordinal: 0,
        };
        let members = std::iter::once((symbol.clone(), vec![id.clone()])).collect();
        let reference = |start| serde_json::json!({"target":"f", "role":"call", "location":{"path":"entry.wi", "start":start, "end":start+1}});
        let mut first = SyntaxQueries::default().candidate().unwrap();
        first
            .capture_references(vec![(id.clone(), reference(1))], &members)
            .unwrap();
        first.references(symbol.clone()).unwrap();
        let mut shifted = first.candidate().unwrap();
        shifted
            .capture_references(vec![(id.clone(), reference(100))], &members)
            .unwrap();
        assert_eq!(shifted.resolved_reference(id).unwrap(), reference(100));
        assert_eq!(shifted.references(symbol).unwrap(), vec![reference(100)]);
        assert_eq!(shifted.stats().recomputed, 0);
        assert!(
            shifted
                .references(SymbolId("unused".into()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn review_visible_scope_tracks_imports_and_resolved_declarations() {
        let path = Path::new("scope.wi");
        let mut first = SyntaxQueries::default().candidate().unwrap();
        first
            .parse(path, FileId::ENTRY, "fn f() -> i64 { return 1; }")
            .unwrap();
        let scope = serde_json::json!({"f":"i64"});
        first
            .visible_scope(UnitId::ENTRY, path, scope.clone())
            .unwrap();
        let mut body = first.candidate().unwrap();
        body.parse(path, FileId::ENTRY, "fn f() -> i64 { return 2; }")
            .unwrap();
        let before = body.stats();
        assert_eq!(
            body.visible_scope(UnitId::ENTRY, path, scope.clone())
                .unwrap(),
            scope
        );
        assert_eq!(body.stats().recomputed, before.recomputed);
        let mut imported = body.candidate().unwrap();
        imported
            .parse(
                path,
                FileId::ENTRY,
                "import value; fn f() -> i64 { return 2; }",
            )
            .unwrap();
        let before = imported.stats();
        imported
            .visible_scope(UnitId::ENTRY, path, scope.clone())
            .unwrap();
        assert_eq!(imported.stats().recomputed, before.recomputed + 1);
        let mut signature = imported.candidate().unwrap();
        signature
            .parse(
                path,
                FileId::ENTRY,
                "import value; fn f() -> i64 { return 2; }",
            )
            .unwrap();
        let changed = serde_json::json!({"f":"i64", "value::get":"String"});
        assert_eq!(
            signature
                .visible_scope(UnitId::ENTRY, path, changed.clone())
                .unwrap(),
            changed
        );
        assert_ne!(
            signature
                .read_visible_scope(UnitId::ENTRY, path)
                .unwrap()
                .get::<CompactJson>()
                .decode::<Value>()
                .unwrap(),
            scope
        );
    }

    #[test]
    fn review_reference_queries_retarget_remove_and_isolate_symbols() {
        use super::super::references::{SymbolId, SymbolUseId};
        for count in [8, 32, 128] {
            let symbol = |i| SymbolId(format!("function:{i}"));
            let use_id = |i| SymbolUseId {
                owner: SymbolId(format!("body:{i}")),
                ordinal: 0,
            };
            let members: std::collections::BTreeMap<_, _> =
                (0..count).map(|i| (symbol(i), vec![use_id(i)])).collect();
            let uses: Vec<_> = (0..count)
                .map(|i| (use_id(i), serde_json::json!({"target": i})))
                .collect();
            let mut first = SyntaxQueries::default().candidate().unwrap();
            first.capture_references(uses.clone(), &members).unwrap();
            for (i, (_, reference)) in uses.iter().enumerate() {
                assert_eq!(
                    first.references(symbol(i)).unwrap(),
                    vec![reference.clone()]
                );
            }
            let mut next = first.candidate().unwrap();
            let mut changed = uses.clone();
            changed[0].1 = serde_json::json!({"target":1});
            let mut changed_members = members.clone();
            changed_members.remove(&symbol(0));
            changed_members.get_mut(&symbol(1)).unwrap().push(use_id(0));
            next.capture_references(changed, &changed_members).unwrap();
            assert!(next.references(symbol(0)).unwrap().is_empty());
            assert_eq!(next.references(symbol(1)).unwrap().len(), 2);
            let before = next.stats();
            for (i, (_, reference)) in uses.iter().enumerate().skip(2) {
                assert_eq!(next.references(symbol(i)).unwrap(), vec![reference.clone()]);
            }
            assert_eq!(next.stats().recomputed, before.recomputed);
            let mut removed = next.candidate().unwrap();
            removed
                .capture_references(vec![], &Default::default())
                .unwrap();
            assert!(removed.references(symbol(1)).unwrap().is_empty());
            assert!(removed.resolved_reference(use_id(0)).unwrap().is_null());
            println!("review_references symbols={count} unrelated_recomputations=0");
        }
    }

    #[test]
    fn repeated_semantic_consumers_share_one_captured_signature() {
        for size in [8, 32, 128] {
            let signature = Rc::new(serde_json::json!((0..size).collect::<Vec<_>>()));
            let mut queries = SyntaxQueries::default().candidate().unwrap();
            for _ in 0..size {
                queries
                    .capture_semantic(
                        UnitId::ENTRY,
                        vec![("shared".into(), Rc::clone(&signature))],
                    )
                    .unwrap();
                assert!(Rc::ptr_eq(
                    &queries.symbols[&(UnitId::ENTRY, "shared".into())],
                    &signature,
                ));
                assert_eq!(Rc::strong_count(&signature), 2);
            }
            assert_eq!(queries.symbols.len(), 1);
            // Sharing is an optimization, never a substitute for validating
            // a separately produced value under the same input key.
            assert!(
                queries
                    .capture_semantic(UnitId::ENTRY, vec![("shared".into(), Rc::new(Value::Null))],)
                    .is_err()
            );
        }
    }

    #[test]
    fn body_edit_backdates_declarations_and_imports() {
        let mut accepted = SyntaxQueries::default().candidate().unwrap();
        let path = Path::new("entry.wi");
        accepted
            .parse(path, FileId::ENTRY, "fn f() -> i64 { return 1; }")
            .unwrap();
        let mut next = accepted.candidate().unwrap();
        next.parse(path, FileId::ENTRY, "fn f() -> i64 { return 2; }")
            .unwrap();
        assert_eq!(next.stats().recomputed, 3);
        assert_eq!(next.stats().changed, 1);
        assert_eq!(next.stats().green_after_recompute, 2);
        assert_eq!(next.stats().dependency_edges_visited, 4);
    }
    #[test]
    fn per_owner_projections_have_linear_edge_visits() {
        for count in [8, 32, 128] {
            let path = Path::new("entry.wi");
            let source = (0..count)
                .map(|i| format!("fn function_{i}() -> i64 {{ return {i}; }}\n"))
                .collect::<String>();
            let mut accepted = SyntaxQueries::default().candidate().unwrap();
            accepted.parse(path, FileId::ENTRY, &source).unwrap();
            for i in 0..count {
                accepted
                    .signature(path, &format!("Function:function_{i}"))
                    .unwrap();
                accepted
                    .body_syntax(path, &format!("Function:function_{i}"))
                    .unwrap();
            }
            let changed = source.replacen("return 0", "return 999", 1);
            let mut next = accepted.candidate().unwrap();
            next.parse(path, FileId::ENTRY, &changed).unwrap();
            for i in 0..count {
                assert_eq!(
                    accepted
                        .signature(path, &format!("Function:function_{i}"))
                        .unwrap(),
                    next.signature(path, &format!("Function:function_{i}"))
                        .unwrap()
                );
                let before = accepted
                    .body_syntax(path, &format!("Function:function_{i}"))
                    .unwrap();
                let after = next
                    .body_syntax(path, &format!("Function:function_{i}"))
                    .unwrap();
                assert_eq!(before == after, i != 0);
            }
            assert_eq!(next.stats().dependency_edges_visited, 4 + 2 * count);
            assert_eq!(next.stats().changed, 2); // parse and edited body projection
            assert_eq!(next.stats().green_after_recompute, 2);
            assert_eq!(next.stats().recomputed, 4);
        }
    }
    #[test]
    fn signature_and_coordinates_are_not_backdated_incorrectly() {
        let path = Path::new("entry.wi");
        let mut first = SyntaxQueries::default().candidate().unwrap();
        first
            .parse(path, FileId::ENTRY, "fn f() -> i64 { return 1; }")
            .unwrap();
        let signature = first.signature(path, "Function:f").unwrap();
        let mut next = first.candidate().unwrap();
        let (program, _, _) = next
            .parse(path, FileId::ENTRY, "\nfn f() -> String { return \"a\"; }")
            .unwrap();
        assert_ne!(signature, next.signature(path, "Function:f").unwrap());
        let crate::parser::ast::Item::Function(function) = &program.items[0] else {
            panic!()
        };
        assert_eq!(function.span.line, 2);
        assert_eq!(next.stats().changed, 3); // parse, declarations, signature
    }

    #[test]
    fn typed_syntax_stamp_is_scoped_to_its_query() {
        let value = |owner: &str| TypedBodyValue {
            syntax_node: QueryNode::BodySyntax("entry.wi".into(), owner.into()),
            syntax_revision: super::super::tracked::Revision(1),
            result: CompactJson::new(&Value::Null).unwrap(),
        };
        assert!(value("Function:f") == value("Function:f"));
        assert!(value("Function:f") != value("Function:g"));
    }

    #[test]
    fn typed_syntax_stamp_preserves_changes_with_equal_typed_results() {
        let path = Path::new("entry.wi");
        let body = BodyId::fresh();
        let mut accepted = SyntaxQueries::default();
        let mut stamps = Vec::new();
        for (source, reuse) in [
            ("fn f() -> i64 { return 1; }", false),
            ("fn f() -> i64 { return 1; }", true),
            ("\nfn f() -> i64 { return 1; }", true),
            ("fn f() -> i64 { return 2; }", false),
            ("fn f() -> i64 { return 1; }", false),
        ] {
            let mut next = accepted.candidate().unwrap();
            next.parse(path, FileId::ENTRY, source).unwrap();
            assert_eq!(
                next.validate_body(UnitId::ENTRY, body, path, "Function:f", vec![])
                    .unwrap(),
                reuse
            );
            if !reuse {
                next.publish_body(UnitId::ENTRY, body, Value::Null, vec![], vec![])
                    .unwrap();
            }
            let value = next
                .table
                .read(&next.provider(), QueryNode::TypedBody(body))
                .unwrap();
            stamps.push(value.get::<TypedBodyValue>().syntax_revision);
            accepted = next;
        }
        assert_eq!(stamps[0], stamps[1]);
        assert_eq!(stamps[1], stamps[2]);
        assert_ne!(stamps[2], stamps[3]);
        assert_ne!(stamps[3], stamps[4]);
    }

    #[test]
    fn typed_demand_runs_only_after_real_dependency_change() {
        let path = Path::new("entry.wi");
        let mut first = SyntaxQueries::default().candidate().unwrap();
        let source = "fn f() -> i64 { return 1; } fn g() -> i64 { return f(); }";
        first.parse(path, FileId::ENTRY, source).unwrap();
        let f = BodyId::fresh();
        let g = BodyId::fresh();
        let symbol = || vec![("function:f".to_owned(), Rc::new(serde_json::json!("i64")))];
        assert!(
            !first
                .validate_body(UnitId::ENTRY, f, path, "Function:f", vec![])
                .unwrap()
        );
        first
            .publish_body(UnitId::ENTRY, f, Value::Null, vec![], vec![])
            .unwrap();
        assert!(
            !first
                .validate_body(UnitId::ENTRY, g, path, "Function:g", symbol())
                .unwrap()
        );
        first
            .publish_body(UnitId::ENTRY, g, Value::Null, symbol(), vec![])
            .unwrap();
        let mut next = first.candidate().unwrap();
        next.parse(
            path,
            FileId::ENTRY,
            &source.replacen("return 1", "return 2", 1),
        )
        .unwrap();
        assert!(
            !next
                .validate_body(UnitId::ENTRY, f, path, "Function:f", vec![])
                .unwrap()
        );
        next.publish_body(UnitId::ENTRY, f, Value::Null, vec![], vec![])
            .unwrap();
        assert!(
            next.validate_body(UnitId::ENTRY, g, path, "Function:g", symbol())
                .unwrap()
        );
        let mut signature_change = next.candidate().unwrap();
        signature_change
            .parse(
                path,
                FileId::ENTRY,
                &source.replace("fn f() -> i64", "fn f() -> String"),
            )
            .unwrap();
        assert!(
            !signature_change
                .validate_body(
                    UnitId::ENTRY,
                    g,
                    path,
                    "Function:g",
                    vec![("function:f".into(), Rc::new(serde_json::json!("String")))]
                )
                .unwrap()
        );
        // A deferred read is not cached as a diagnostic; retry after supplying
        // checked output succeeds and keeps the original dependency graph.
        signature_change
            .publish_body(
                UnitId::ENTRY,
                g,
                Value::Bool(true),
                vec![("function:f".into(), Rc::new(serde_json::json!("String")))],
                vec![],
            )
            .unwrap();
        assert!(
            signature_change
                .validate_body(UnitId::ENTRY, g, path, "Function:g", vec![])
                .unwrap()
        );
    }

    #[test]
    fn measured_symbol_edges_avoid_aggregate_typecheck_fanout() {
        for count in [8, 32, 128] {
            for aggregate in [false, true] {
                let path = Path::new("entry.wi");
                let source = (0..count)
                    // `fN` names would collide with builtin types such as `f64`.
                    .map(|i| format!("fn fun{i}() -> i64 {{ return {i}; }}\n"))
                    .collect::<String>();
                let mut first = SyntaxQueries::default().candidate().unwrap();
                first.parse(path, FileId::ENTRY, &source).unwrap();
                let bodies = (0..count).map(|_| BodyId::fresh()).collect::<Vec<_>>();
                let key = |i| {
                    if aggregate {
                        "exports".to_owned()
                    } else {
                        format!("symbol:{i}")
                    }
                };
                for (i, &body) in bodies.iter().enumerate() {
                    let reads = vec![(key(i), Rc::new(Value::Bool(false)))];
                    assert!(
                        !first
                            .validate_body(
                                UnitId::ENTRY,
                                body,
                                path,
                                &format!("Function:fun{i}"),
                                reads.clone()
                            )
                            .unwrap()
                    );
                    first
                        .publish_body(UnitId::ENTRY, body, Value::Null, reads, vec![])
                        .unwrap();
                }
                let mut next = first.candidate().unwrap();
                next.parse(path, FileId::ENTRY, &source).unwrap();
                let reads = if aggregate {
                    vec![(key(0), Rc::new(Value::Bool(true)))]
                } else {
                    (0..count)
                        .map(|i| (key(i), Rc::new(Value::Bool(i == 0))))
                        .collect()
                };
                next.capture_semantic(UnitId::ENTRY, reads).unwrap();
                let mut demands = 0;
                for (i, &body) in bodies.iter().enumerate() {
                    demands += usize::from(
                        !next
                            .validate_body(
                                UnitId::ENTRY,
                                body,
                                path,
                                &format!("Function:fun{i}"),
                                vec![],
                            )
                            .unwrap(),
                    );
                }
                assert_eq!(demands, if aggregate { count } else { 1 });
                // Per-owner syntax inputs validate without revisiting Parse.
                let expected_visits = if aggregate { 5 * count + 1 } else { 6 * count };
                assert_eq!(next.stats().dependency_edges_visited, expected_visits);
                println!(
                    "signature_strategy={} bodies={count} edge_visits={expected_visits} demanded_typechecks={demands}",
                    if aggregate { "aggregate" } else { "per_symbol" }
                );
            }
        }
    }

    #[test]
    fn candidate_rollback_and_unchanged_parse() {
        let mut accepted = SyntaxQueries::default().candidate().unwrap();
        let path = Path::new("entry.wi");
        let source = "fn f() -> i64 { return 1; }";
        accepted.parse(path, FileId::ENTRY, source).unwrap();
        let mut rejected = accepted.candidate().unwrap();
        rejected.parse(path, FileId::ENTRY, "fn bad(").unwrap();
        drop(rejected);
        let mut next = accepted.candidate().unwrap();
        next.parse(path, FileId::ENTRY, source).unwrap();
        assert_eq!(next.stats().recomputed, 0);
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct SyntaxSnapshot {
    table: super::tracked::DiskTable,
    dispatch_classes: Vec<(UnitId, std::collections::BTreeSet<String>)>,
    #[serde(default)]
    coordinate_epoch: u64,
}
impl SyntaxQueries {
    pub(crate) fn disk_snapshot(&self) -> Result<SyntaxSnapshot> {
        Ok(SyntaxSnapshot {
            table: self.table.disk_snapshot_filtered(|node| match node {
                QueryNode::Parse(path) => self.captured.contains_key(path),
                QueryNode::TypedBody(body)
                | QueryNode::NormalizedBody(body)
                | QueryNode::DirectEffects(body)
                | QueryNode::DefiniteAssignment(body)
                | QueryNode::AsyncBorrowReport(body)
                | QueryNode::ResolvedReferences(body) => self.bodies.contains_key(body),
                _ => true,
            })?,
            dispatch_classes: self
                .dispatch_classes
                .iter()
                .map(|(unit, classes)| (*unit, classes.clone()))
                .collect(),
            coordinate_epoch: self.coordinate_epoch,
        })
    }
    pub(crate) fn from_disk(snapshot: SyntaxSnapshot) -> Result<Self> {
        let mut dispatch_classes = InventoryMap::default();
        for (unit, classes) in snapshot.dispatch_classes {
            anyhow::ensure!(
                dispatch_classes.insert(unit, classes).is_none(),
                "duplicate dispatch inventory"
            );
        }
        Ok(Self {
            table: TrackedQueryTable::from_disk(snapshot.table)?,
            dispatch_classes,
            coordinate_epoch: snapshot.coordinate_epoch,
            ..Self::default()
        })
    }
}

/// An explicit, closed codec keeps unrecognized Any payloads out of disk state.
#[derive(serde::Serialize, serde::Deserialize)]
pub(crate) struct DiskValue {
    kind: DiskKind,
    // Decode directly into the known type; never allocate a second generic
    // JSON tree for the entire query closure.
    payload: Box<serde_json::value::RawValue>,
}
#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
enum DiskKind {
    Source,
    Parsed,
    Compact,
    Typed,
    Dispatch,
    Children,
    Derived,
    Dependencies,
}
impl DiskValue {
    fn query_kind(node: &QueryNode) -> Result<DiskKind> {
        Ok(match node {
            QueryNode::Parse(_) => DiskKind::Parsed,
            QueryNode::TypedBody(_) => DiskKind::Typed,
            QueryNode::NormalizedBody(_)
            | QueryNode::DirectEffects(_)
            | QueryNode::DefiniteAssignment(_)
            | QueryNode::AsyncBorrowReport(_)
            | QueryNode::ResolvedReferences(_) => DiskKind::Derived,
            QueryNode::CompilerEnvironment
            | QueryNode::SyntaxDeclarations(_)
            | QueryNode::SyntaxImports(_)
            | QueryNode::SyntaxSignature(..)
            | QueryNode::BodySyntax(..)
            | QueryNode::SemanticSignature(..)
            | QueryNode::DispatchTargets(..) => DiskKind::Compact,
            _ => anyhow::bail!("unsupported disk query"),
        })
    }
    fn input_kind(node: &InputNode) -> Result<DiskKind> {
        Ok(match node {
            InputNode::Source(_) => DiskKind::Source,
            InputNode::DerivedDependencies(_) => DiskKind::Dependencies,
            InputNode::SyntaxSignature(..)
            | InputNode::BodySyntax(..)
            | InputNode::SemanticSymbol(..)
            | InputNode::CompilerStamp
            | InputNode::StdlibStamp
            | InputNode::RuntimeAbiRevision => DiskKind::Compact,
            InputNode::DispatchDeclaration(..) => DiskKind::Dispatch,
            InputNode::DispatchChildren(..) => DiskKind::Children,
            _ => anyhow::bail!("unsupported disk input"),
        })
    }
    fn encode(kind: DiskKind, value: &QueryValue) -> Result<Self> {
        fn encode_as<T: serde::Serialize + 'static>(
            value: &QueryValue,
        ) -> Result<Box<serde_json::value::RawValue>> {
            let value = value
                .downcast::<T>()
                .ok_or_else(|| anyhow::anyhow!("disk payload type mismatch"))?;
            Ok(serde_json::value::to_raw_value(value)?)
        }
        let payload = match kind {
            DiskKind::Source => encode_as::<Source>(value)?,
            DiskKind::Parsed => encode_as::<ParsedValue>(value)?,
            DiskKind::Compact => encode_as::<CompactJson>(value)?,
            DiskKind::Typed => encode_as::<TypedBodyValue>(value)?,
            DiskKind::Dispatch => encode_as::<super::dispatch::DispatchDeclaration>(value)?,
            DiskKind::Children => encode_as::<Vec<String>>(value)?,
            DiskKind::Derived => encode_as::<DerivedValue>(value)?,
            DiskKind::Dependencies => encode_as::<Vec<QueryNode>>(value)?,
        };
        Ok(Self { kind, payload })
    }
    pub(crate) fn query(node: &QueryNode, value: &QueryValue) -> Result<Self> {
        Self::encode(Self::query_kind(node)?, value)
    }
    pub(crate) fn input(node: &InputNode, value: &QueryValue) -> Result<Self> {
        Self::encode(Self::input_kind(node)?, value)
    }
    pub(crate) fn decode_query(
        self,
        node: &QueryNode,
        fingerprint: ResultFingerprint,
    ) -> Result<QueryValue> {
        anyhow::ensure!(
            self.kind == Self::query_kind(node)?,
            "wrong disk query payload"
        );
        self.decode(fingerprint)
    }
    pub(crate) fn decode_input(
        self,
        node: &InputNode,
        fingerprint: ResultFingerprint,
    ) -> Result<QueryValue> {
        anyhow::ensure!(
            self.kind == Self::input_kind(node)?,
            "wrong disk input payload"
        );
        self.decode(fingerprint)
    }
    fn decode(self, fingerprint: ResultFingerprint) -> Result<QueryValue> {
        fn decode_as<T: serde::de::DeserializeOwned + Eq + Retained + 'static>(
            payload: &str,
            fingerprint: ResultFingerprint,
        ) -> Result<QueryValue> {
            Ok(QueryValue::new(
                serde_json::from_str::<T>(payload)?,
                fingerprint,
            ))
        }
        match self.kind {
            DiskKind::Source => decode_as::<Source>(self.payload.get(), fingerprint),
            DiskKind::Parsed => decode_as::<ParsedValue>(self.payload.get(), fingerprint),
            DiskKind::Compact => decode_as::<CompactJson>(self.payload.get(), fingerprint),
            DiskKind::Typed => decode_as::<TypedBodyValue>(self.payload.get(), fingerprint),
            DiskKind::Dispatch => {
                decode_as::<super::dispatch::DispatchDeclaration>(self.payload.get(), fingerprint)
            }
            DiskKind::Children => decode_as::<Vec<String>>(self.payload.get(), fingerprint),
            DiskKind::Derived => decode_as::<DerivedValue>(self.payload.get(), fingerprint),
            DiskKind::Dependencies => decode_as::<Vec<QueryNode>>(self.payload.get(), fingerprint),
        }
    }
}

#[cfg(test)]
mod disk_snapshot_tests {
    use super::*;

    #[test]
    fn disk_json_buffers_preserve_exact_bytes_without_numeric_arrays() {
        for text in [
            "null",
            "\"日本語 🦀\"",
            " {\"escaped\":\"\\n\\\"\\\\\",\"x\":[1,2]} ",
        ] {
            let original = CompactJson::from_bytes(text.as_bytes().to_vec());
            let encoded = serde_json::to_vec(&original).unwrap();
            let shape: Value = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(shape["bytes"].as_str(), Some(text));
            let decoded: CompactJson = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(decoded.bytes, original.bytes);
            assert_eq!(
                ResultFingerprint::bytes(&decoded.bytes),
                ResultFingerprint::bytes(&original.bytes)
            );
        }
        assert!(serde_json::from_str::<CompactJson>(r#"{"bytes":[0,255]}"#).is_err());
    }

    fn fixture() -> (SyntaxQueries, BodyId) {
        let mut queries = SyntaxQueries::default().candidate().unwrap();
        let path = Path::new("disk-entry.wi");
        queries
            .parse(path, FileId::ENTRY, "fn f() -> i64 { return 1; }")
            .unwrap();
        let body = BodyId::fresh();
        queries.register_body(UnitId::ENTRY, body, path, "Function:f");
        queries
            .publish_body(UnitId::ENTRY, body, Value::Null, vec![], vec![])
            .unwrap();
        (queries, body)
    }

    #[test]
    fn disk_snapshot_typed_closure_reuses_and_invalidates() {
        let (queries, body) = fixture();
        let bytes = serde_json::to_vec(&queries.disk_snapshot().unwrap()).unwrap();
        for (source, expected) in [
            ("fn f() -> i64 { return 1; }", true),
            ("fn f() -> i64 { return 2; }", false),
        ] {
            let snapshot = serde_json::from_slice(&bytes).unwrap();
            let restored = SyntaxQueries::from_disk(snapshot).unwrap();
            let mut next = restored.candidate().unwrap();
            let path = Path::new("disk-entry.wi");
            next.parse(path, FileId::ENTRY, source).unwrap();
            assert_eq!(
                next.validate_body(UnitId::ENTRY, body, path, "Function:f", vec![])
                    .unwrap(),
                expected
            );
        }
    }

    #[test]
    fn disk_snapshot_retains_body_analysis_and_invalidates_changed_dependencies() {
        let (mut queries, body) = fixture();
        let nodes = [
            QueryNode::NormalizedBody(body),
            QueryNode::DirectEffects(body),
            QueryNode::DefiniteAssignment(body),
            QueryNode::AsyncBorrowReport(body),
            QueryNode::ResolvedReferences(body),
        ];
        let dependencies = vec![QueryNode::BodySyntax(
            PathBuf::from("disk-entry.wi"),
            "Function:f".into(),
        )];
        for node in &nodes {
            queries
                .publish_derived(node.clone(), serde_json::json!([42]), dependencies.clone())
                .unwrap();
        }
        let bytes = serde_json::to_vec(&queries.disk_snapshot().unwrap()).unwrap();
        for (source, retained) in [
            ("fn f() -> i64 { return 1; }", true),
            ("fn f() -> i64 { return 2; }", false),
            ("fn other() -> i64 { return 1; }", false),
        ] {
            let mut next = SyntaxQueries::from_disk(serde_json::from_slice(&bytes).unwrap())
                .unwrap()
                .candidate()
                .unwrap();
            next.parse(Path::new("disk-entry.wi"), FileId::ENTRY, source)
                .unwrap();
            for node in &nodes {
                let result = next
                    .validate_derived_typed::<Vec<i32>>(node.clone(), dependencies.clone())
                    .unwrap();
                assert_eq!(result, retained.then_some(vec![42]), "{node:?}: {source}");
            }
        }
    }

    #[test]
    fn disk_snapshot_rejects_missing_cycle_timestamp_and_wrong_payload() {
        let (queries, _) = fixture();
        let pristine = serde_json::to_value(queries.disk_snapshot().unwrap()).unwrap();
        let mut missing = pristine.clone();
        missing["table"]["inputs"] = serde_json::json!([]);
        let mut timestamp = pristine.clone();
        timestamp["table"]["revision"] = serde_json::json!(0);
        let mut wrong = pristine.clone();
        wrong["table"]["memos"][0]["value"]["kind"] = serde_json::json!("Children");
        let mut cycle = pristine.clone();
        let node = cycle["table"]["memos"][0]["node"].clone();
        let revision = cycle["table"]["memos"][0]["changed_at"].clone();
        cycle["table"]["memos"][0]["dependencies"] = serde_json::json!([
            {"node":{"Query":node}, "observed_changed_at": revision}
        ]);
        for invalid in [missing, timestamp, wrong, cycle] {
            let snapshot = serde_json::from_value(invalid).unwrap();
            assert!(SyntaxQueries::from_disk(snapshot).is_err());
        }
    }
}
