//! Pure syntax queries; source capture and artifact I/O stay at the caller.
use super::retained::Retained;
use super::tracked::{
    InputNode, QueryNode, QueryOutcome, QueryProvider, QueryValue, ResultFingerprint,
    TrackedQueryTable, TrackedStats, query_ready,
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
    SourceOwner,
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

/// Classification accompanies the existing cache key; consumers never decode
/// declaration kinds from that key. The serialized identity remains unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceOwnerKind {
    Function,
    Other,
}
#[derive(Debug, Clone)]
pub(crate) struct SourceOwner {
    wire: String,
    kind: SourceOwnerKind,
}
impl SourceOwner {
    pub(crate) fn function(name: &str) -> Self {
        Self {
            wire: format!("Function:{name}"),
            kind: SourceOwnerKind::Function,
        }
    }
    pub(crate) fn function_name(&self) -> Option<&str> {
        match self.kind {
            SourceOwnerKind::Function => Some(&self.wire["Function:".len()..]),
            SourceOwnerKind::Other => None,
        }
    }
    pub(crate) fn wire_key(&self) -> &str {
        &self.wire
    }
}
impl PartialEq for SourceOwner {
    fn eq(&self, other: &Self) -> bool {
        self.wire == other.wire
    }
}
impl Eq for SourceOwner {}
impl Ord for SourceOwner {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.wire.cmp(&other.wire)
    }
}
impl PartialOrd for SourceOwner {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl std::hash::Hash for SourceOwner {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.wire.hash(state);
    }
}
impl std::borrow::Borrow<str> for SourceOwner {
    fn borrow(&self) -> &str {
        self.wire_key()
    }
}
impl serde::Serialize for SourceOwner {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.wire_key())
    }
}
impl<'de> serde::Deserialize<'de> for SourceOwner {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = <String as serde::Deserialize>::deserialize(deserializer)?;
        // Persisted caches use the original textual declaration keys. Decode
        // their tag once at this boundary, just as AST capture supplies it below.
        let kind = if wire.starts_with("Function:") {
            SourceOwnerKind::Function
        } else {
            SourceOwnerKind::Other
        };
        Ok(Self { wire, kind })
    }
}
impl Retained for SourceOwner {
    fn heap_bytes(&self) -> usize {
        self.wire.heap_bytes()
    }
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
    owner_changes: InventoryMap<PathBuf, Vec<(SourceOwner, bool)>>,
    source_body_changes: InventoryMap<PathBuf, Vec<SourceOwner>>,
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
        Ok(matches!(
            self.table
                .evaluate(&self.provider(), QueryNode::TypedBody(body))?,
            QueryOutcome::Ready(_)
        ))
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
    pub(crate) fn source_body_changes(&self, unit: UnitId) -> Vec<SourceOwner> {
        self.source_paths
            .get(&unit)
            .and_then(|path| self.source_body_changes.get(path))
            .cloned()
            .unwrap_or_default()
    }
    pub(crate) fn source_owner_span(&self, unit: UnitId, owner: &SourceOwner) -> Option<Span> {
        let path = self.source_paths.get(&unit)?;
        let parsed = self.table.peek(&QueryNode::Parse(path.clone()))?;
        parsed.get::<ParsedValue>().owner_spans.get(owner).copied()
    }
    pub(crate) fn effect_source_changes(&self, unit: UnitId) -> Option<Vec<(SourceOwner, bool)>> {
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
        match self.table.evaluate(&self.provider(), node)? {
            QueryOutcome::Ready(result) => {
                let wire = &result.get::<DerivedValue>().wire;
                match wire.split_first() {
                    Some((&DERIVED_WIRE_VERSION, payload)) => {
                        Ok(Some(serde_json::from_slice(payload)?))
                    }
                    _ => Ok(None), // An older wire schema must be recomputed.
                }
            }
            QueryOutcome::Deferred(_) => Ok(None),
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
        Ok(matches!(
            self.table
                .evaluate(&self.provider(), QueryNode::EffectInventory(unit))?,
            QueryOutcome::Ready(_)
        ))
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
            let size = self.table.compact_rooted(
                |node| match node {
                    QueryNode::EffectCapabilities(unit, id) => {
                        effects.get(unit).is_some_and(|ids| ids.contains(id))
                    }
                    QueryNode::ClassLayout(id) | QueryNode::GcLayout(id) => {
                        classes.contains(id)
                            || captured.contains(&InputNode::LayoutDeclaration(*id))
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
                },
                |node| captured.contains(node),
            );
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
                InputNode::SyntaxSignature(path.to_owned(), owner.wire_key().to_owned()),
                signature.clone().into_query(),
            )?;
            self.capture_input(
                InputNode::BodySyntax(path.to_owned(), owner.wire_key().to_owned()),
                body.clone().into_query(),
            )?;
        }
        if let Some(previous) = &previous {
            for owner in previous.get::<ParsedValue>().owners.keys() {
                if !parsed.owners.contains_key(owner) {
                    self.capture_input(
                        InputNode::SyntaxSignature(path.to_owned(), owner.wire_key().to_owned()),
                        value(Value::Null)?,
                    )?;
                    self.capture_input(
                        InputNode::BodySyntax(path.to_owned(), owner.wire_key().to_owned()),
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
    owners: HashMap<SourceOwner, (CompactJson, CompactJson)>,
    #[serde(default)]
    owner_spans: HashMap<SourceOwner, Span>,
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
fn index_declarations(
    items: &Value,
    prefix: &str,
    root_kind: Option<SourceOwnerKind>,
    index: &mut std::collections::BTreeMap<SourceOwner, Value>,
) {
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
        let owner_kind = root_kind.unwrap_or(if kind == "Function" {
            SourceOwnerKind::Function
        } else {
            SourceOwnerKind::Other
        });
        let owner = SourceOwner {
            wire: format!("{prefix}{kind}:{name}"),
            kind: owner_kind,
        };
        let body = declaration
            .get("body")
            .or_else(|| declaration.get("default_body"))
            .or_else(|| declaration.get("initializer"))
            .cloned()
            .unwrap_or(Value::Null);
        index.insert(owner.clone(), serde_json::json!({"signature": declarations(declaration.clone()), "body": super::syntax::semantic(&body), "span": declaration.get("span")}));
        for field in ["methods", "static_methods", "constructors", "fields"] {
            index_declarations(
                &declaration[field],
                &format!("{}/{field}", owner.wire_key()),
                Some(owner_kind),
                index,
            );
        }
    }
}

impl QueryProvider for SyntaxProvider<'_> {
    fn contains_body(&self, body: BodyId) -> bool {
        self.bodies.contains_key(&body)
    }
    #[allow(
        clippy::too_many_lines,
        reason = "the QueryNode dispatch keeps each query's dependency reads beside its result and Deferred exits, making cache invalidation dependencies auditable"
    )]
    fn compute(
        &self,
        table: &TrackedQueryTable,
        node: &QueryNode,
    ) -> Result<QueryOutcome<QueryValue>> {
        let value = match node {
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
                query_ready!(table.evaluate(self, QueryNode::CompilerEnvironment));
                let input = table.input(&InputNode::Source(path.clone()))?;
                let source = input.get::<Source>();
                let mut inventory = Vec::new();
                let (program, diagnostics, lexer_failed) = if path
                    .extension()
                    .is_some_and(|e| e == "rs")
                {
                    let (program, diagnostics) =
                        crate::rust_module::parse(path, &source.text, source.file);
                    (program, diagnostics, false)
                } else {
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
                    }
                };
                let parsed = serde_json::to_value((program, diagnostics, lexer_failed))?;
                let mut index = std::collections::BTreeMap::new();
                index_declarations(&parsed[0]["items"], "", None, &mut index);
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
                let parsed = query_ready!(table.evaluate(self, QueryNode::Parse(path.clone())));
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
                    return table.input(&input).map(QueryOutcome::Ready);
                }
                // An owner never observed in this file still has to track its
                // possible future addition. A captured removal uses Null above.
                let parsed = query_ready!(table.evaluate(self, QueryNode::Parse(path.clone())));
                match parsed.get::<ParsedValue>().owners.get(owner.as_str()) {
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
                    return Ok(QueryOutcome::Deferred(Box::new(node.clone())));
                };
                let dependencies = table.input(&InputNode::DerivedDependencies(node.clone()))?;
                for dependency in dependencies.get::<Vec<QueryNode>>() {
                    query_ready!(table.evaluate(self, dependency.clone()));
                }
                let fingerprint = ResultFingerprint::bytes(&prepared.canonical);
                Ok(QueryValue::new(prepared.clone(), fingerprint))
            }
            QueryNode::EffectInventory(unit) => {
                let Some(prepared) = self.effects.get(unit) else {
                    return Ok(QueryOutcome::Deferred(Box::new(node.clone())));
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
                    query_ready!(table.evaluate(self, QueryNode::Parse(path.clone())));
                }
                for &id in self.interface_dispatch.get(unit).into_iter().flatten() {
                    query_ready!(
                        table.evaluate(self, QueryNode::InterfaceDispatchTargets(*unit, id))
                    );
                }
                for unit in &prepared.source_units {
                    query_ready!(table.evaluate(self, QueryNode::EffectInventory(*unit)));
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
                        query_ready!(
                            table.evaluate(self, QueryNode::ResolvedReference(id.clone()))
                        )
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
                return super::dispatch::DispatchProvider.compute(table, node);
            }
            QueryNode::SemanticSignature(unit, key) => {
                if let Ok(crate::semantic::symbols::SymbolRead::Dispatch(class, method)) =
                    serde_json::from_str(key)
                {
                    return table.evaluate(self, QueryNode::DispatchTargets(*unit, class, method));
                }
                table.input(&InputNode::SemanticSymbol(*unit, key.clone()))
            }
            QueryNode::VisibleScope(unit, path) => {
                query_ready!(table.evaluate(self, QueryNode::SyntaxDeclarations(path.clone())));
                query_ready!(table.evaluate(self, QueryNode::SyntaxImports(path.clone())));
                table.input(&InputNode::VisibleDeclarations(*unit))
            }
            QueryNode::TypedBody(body) => {
                let binding = self
                    .bodies
                    .get(body)
                    .ok_or_else(|| anyhow::anyhow!("unregistered body {body:?}"))?;
                let Some(prepared) = self.prepared.get(body) else {
                    return Ok(QueryOutcome::Deferred(Box::new(node.clone())));
                };
                let syntax_node =
                    QueryNode::BodySyntax(binding.path.clone(), binding.owner.clone());
                let (_, syntax_revision) =
                    query_ready!(table.read_stamped(self, syntax_node.clone()));
                query_ready!(table.evaluate(
                    self,
                    QueryNode::SyntaxSignature(binding.path.clone(), binding.owner.clone()),
                ));
                for key in &prepared.keys {
                    query_ready!(table.evaluate(
                        self,
                        QueryNode::SemanticSignature(binding.unit, key.clone()),
                    ));
                }
                for child in &prepared.children {
                    query_ready!(table.evaluate(self, QueryNode::TypedBody(*child)));
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
        }?;
        Ok(QueryOutcome::Ready(value))
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
mod tests;

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
mod disk_snapshot_tests;
