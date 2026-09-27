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
use serde_json::Value;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    rc::Rc,
};

#[derive(Clone, PartialEq, Eq)]
struct Source {
    file: FileId,
    text: String,
}

pub(crate) type TokenInventory = Vec<(String, Span)>;
pub(crate) type TokenPairs = (TokenInventory, TokenInventory);

#[derive(Default)]
pub(crate) struct SyntaxQueries {
    table: TrackedQueryTable,
    source_roots: HashMap<PathBuf, super::tracked::Durability>,
    interface_dispatch: HashMap<UnitId, Vec<FunctionId>>,
    pub(crate) dispatch_classes: HashMap<UnitId, std::collections::BTreeSet<String>>,
    captured: HashMap<PathBuf, Source>,
    advanced: bool,
    bodies: HashMap<BodyId, BodyBinding>,
    prepared: HashMap<BodyId, PreparedBody>,
    symbols: HashMap<(UnitId, String), Rc<Value>>,
    effect_functions: HashMap<UnitId, Vec<FunctionId>>,
    reference_wires: HashMap<super::references::SymbolUseId, CompactJson>,
    reference_symbols: std::collections::BTreeSet<super::references::SymbolId>,
    reference_uses: std::collections::BTreeSet<super::references::SymbolUseId>,
    effects: HashMap<UnitId, PreparedEffects>,
    derived: HashMap<QueryNode, DerivedValue>,
    checked_roots: std::collections::HashSet<BodyId>,
    token_pairs: HashMap<PathBuf, TokenPairs>,
    captured_nodes: std::collections::HashSet<InputNode>,
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
            .insert(unit, inputs.keys().copied().collect());
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
            .difference(&current_uses)
            .cloned()
            .collect();
        for id in removed {
            self.capture_input(InputNode::ResolvedReference(id), value(Value::Null)?)?;
        }
        self.reference_uses = current_uses;
        for (id, reference) in uses {
            let mut canonical = reference.clone();
            if let Some(object) = canonical.as_object_mut() {
                object.remove("location");
            }
            self.reference_wires
                .insert(id.clone(), CompactJson::new(&reference)?);
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
            captured: HashMap::new(),
            advanced: false,
            bodies: HashMap::new(),
            prepared: HashMap::new(),
            symbols: HashMap::new(),
            effect_functions: self.effect_functions.clone(),
            reference_wires: HashMap::new(),
            reference_symbols: self.reference_symbols.clone(),
            reference_uses: self.reference_uses.clone(),
            effects: HashMap::new(),
            derived: HashMap::new(),
            checked_roots: Default::default(),
            token_pairs: HashMap::new(),
            captured_nodes: Default::default(),
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
            self.source_roots.insert(
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
            interface_dispatch: &self.interface_dispatch,
            bodies: &self.bodies,
            prepared: &self.prepared,
            effects: &self.effects,
            derived: &self.derived,
            checked_roots: &self.checked_roots,
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
        self.bodies.insert(
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
        Ok(())
    }
    pub(crate) fn validate_derived(
        &mut self,
        node: QueryNode,
        dependencies: Vec<QueryNode>,
    ) -> Result<Option<Value>> {
        self.capture_input(
            InputNode::DerivedDependencies(node.clone()),
            QueryValue::new(
                dependencies,
                ResultFingerprint::bytes(b"derived-dependencies-v1"),
            ),
        )?;
        match self.table.read(&self.provider(), node) {
            Ok(result) => Ok(Some(serde_json::from_slice(
                &result.get::<DerivedValue>().wire,
            )?)),
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
    pub(crate) fn refresh_derived(&self, node: QueryNode, wire: Value) -> Result<()> {
        let canonical = serde_json::to_vec(&super::syntax::without_spans(&wire))?;
        let fingerprint = ResultFingerprint::bytes(&canonical);
        let wire = serde_json::to_vec(&wire)?;
        self.table.refresh_value(
            &node,
            QueryValue::new(DerivedValue { wire, canonical }, fingerprint),
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
        let wire = serde_json::to_vec(&wire)?;
        self.derived
            .insert(node.clone(), DerivedValue { wire, canonical });
        self.table.read(&self.provider(), node)?;
        Ok(())
    }
    pub(crate) fn validate_effects(&mut self, unit: UnitId, roots: Vec<BodyId>) -> Result<bool> {
        self.capture_input(
            InputNode::EffectRoots(unit),
            QueryValue::new(
                roots
                    .iter()
                    .map(|body| (*body, self.checked_roots.contains(body)))
                    .collect::<Vec<_>>(),
                ResultFingerprint::bytes(b"effect-roots-v1"),
            ),
        )?;
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
    pub(crate) fn publish_effects(
        &mut self,
        unit: UnitId,
        roots: Vec<BodyId>,
        imported: Vec<(UnitId, FunctionId)>,
        inventory: HashMap<FunctionId, super::effects::CapturedEffect>,
        source_units: Vec<UnitId>,
        metadata: Value,
    ) -> Result<()> {
        self.capture_input(
            InputNode::EffectRoots(unit),
            QueryValue::new(
                roots
                    .iter()
                    .map(|body| (*body, self.checked_roots.contains(body)))
                    .collect::<Vec<_>>(),
                ResultFingerprint::bytes(b"effect-roots-v1"),
            ),
        )?;
        for function in self.effect_functions.remove(&unit).unwrap_or_default() {
            if !inventory.contains_key(&function) {
                self.capture_input(
                    InputNode::ComputedEffect(unit, function),
                    QueryValue::new(
                        None::<super::effects::CapturedEffect>,
                        ResultFingerprint::bytes(b"computed-effect"),
                    ),
                )?;
            }
        }
        self.effect_functions
            .insert(unit, inventory.keys().copied().collect());
        for (&function, effect) in &inventory {
            self.capture_input(
                InputNode::ComputedEffect(unit, function),
                QueryValue::new(
                    Some(effect.clone()),
                    ResultFingerprint::bytes(b"computed-effect"),
                ),
            )?;
        }
        self.effects.insert(
            unit,
            PreparedEffects {
                roots,
                imported,
                inventory,
                source_units,
                metadata: CompactJson::new(&metadata)?,
            },
        );
        self.table
            .read(&self.provider(), QueryNode::EffectInventory(unit))?;
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
        let mut dispatch_units = std::collections::HashSet::new();
        let mut interface_units = std::collections::HashSet::new();
        let mut effect_units = std::collections::HashSet::new();
        for input in &self.captured_nodes {
            match input {
                InputNode::DispatchDeclaration(unit, _) => {
                    dispatch_units.insert(*unit);
                }
                InputNode::InterfaceDispatch(unit, _) => {
                    interface_units.insert(*unit);
                }
                InputNode::EffectRoots(unit) => {
                    effect_units.insert(*unit);
                }
                _ => {}
            }
        }
        let old_lengths = (
            self.dispatch_classes.len(),
            self.interface_dispatch.len(),
            self.effect_functions.len(),
        );
        self.dispatch_classes
            .retain(|unit, _| dispatch_units.contains(unit));
        self.interface_dispatch
            .retain(|unit, _| interface_units.contains(unit));
        self.effect_functions
            .retain(|unit, _| effect_units.contains(unit));
        if self.dispatch_classes.len() != old_lengths.0 {
            self.dispatch_classes.shrink_to_fit();
        }
        if self.interface_dispatch.len() != old_lengths.1 {
            self.interface_dispatch.shrink_to_fit();
        }
        if self.effect_functions.len() != old_lengths.2 {
            self.effect_functions.shrink_to_fit();
        }
        let captured = &self.captured_nodes;
        let bodies = &self.bodies;
        let classes: std::collections::HashSet<_> = self
            .dispatch_classes
            .values()
            .flatten()
            .map(|name| crate::semantic::ids::TypeId::from_source_name(name))
            .collect();
        let interfaces: std::collections::HashSet<_> = self
            .interface_dispatch
            .values()
            .flatten()
            .filter_map(FunctionId::owner_type)
            .collect();
        let (nodes, edges) = self.table.compact(|node| match node {
            QueryNode::ClassLayout(id) | QueryNode::GcLayout(id) => {
                classes.contains(id) || captured.contains(&InputNode::LayoutDeclaration(*id))
            }
            QueryNode::InterfaceLayout(id) => {
                interfaces.contains(id) || captured.contains(&InputNode::InterfaceDeclaration(*id))
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
        if std::env::var_os("WILLOW_RETAIN_PROFILE").is_some_and(|v| v == "1") {
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
            + self.interface_dispatch.heap_bytes()
            + self.dispatch_classes.heap_bytes()
            + self.captured.heap_bytes()
            + self.bodies.heap_bytes()
            + self.prepared.heap_bytes()
            + self.symbols.heap_bytes()
            + self.effect_functions.heap_bytes()
            + self.reference_wires.heap_bytes()
            + self.reference_symbols.heap_bytes()
            + self.reference_uses.heap_bytes()
            + self.effects.heap_bytes()
            + self.derived.heap_bytes()
            + self.checked_roots.heap_bytes()
            + self.token_pairs.heap_bytes()
            + self.captured_nodes.heap_bytes()
    }
    pub(crate) fn verification_dump(&self) -> String {
        self.table.verification_dump()
    }
    pub(crate) fn stats(&self) -> TrackedStats {
        self.table.stats()
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
            self.captured.insert(path.to_owned(), captured);
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
        if let Some(previous) = previous {
            let old: TokenInventory =
                serde_json::from_slice(&previous.get::<ParsedValue>().tokens)?;
            let new: TokenInventory = serde_json::from_slice(&parsed.tokens)?;
            self.token_pairs.insert(path.to_owned(), (old, new));
        }
        Ok(serde_json::from_slice(&parsed.wire)?)
    }
}

#[derive(Clone)]
struct DerivedValue {
    wire: Vec<u8>,
    canonical: Vec<u8>,
}
impl PartialEq for DerivedValue {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}
impl Eq for DerivedValue {}

struct ParsedValue {
    wire: Vec<u8>,
    canonical: Vec<u8>,
    tokens: Vec<u8>,
    declarations: CompactJson,
    imports: CompactJson,
    // Precomputed per-owner projections keep lookups independent of file size.
    owners: HashMap<String, (CompactJson, CompactJson)>,
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
struct PreparedEffects {
    roots: Vec<BodyId>,
    imported: Vec<(UnitId, FunctionId)>,
    inventory: HashMap<FunctionId, super::effects::CapturedEffect>,
    source_units: Vec<UnitId>,
    metadata: CompactJson,
}
#[derive(Clone, PartialEq, Eq)]
struct EffectInventoryValue {
    functions: HashMap<FunctionId, super::effects::CapturedEffect>,
    metadata: CompactJson,
}
struct SyntaxProvider<'a> {
    interface_dispatch: &'a HashMap<UnitId, Vec<FunctionId>>,
    bodies: &'a HashMap<BodyId, BodyBinding>,
    prepared: &'a HashMap<BodyId, PreparedBody>,
    effects: &'a HashMap<UnitId, PreparedEffects>,
    derived: &'a HashMap<QueryNode, DerivedValue>,
    checked_roots: &'a std::collections::HashSet<BodyId>,
}
/// Immutable canonical JSON without the per-field allocation overhead of Value.
/// Encoding happens once; equality and fingerprints compare the same bytes.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CompactJson {
    bytes: std::sync::Arc<[u8]>,
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
    fn pair(a: &Self, b: &Self) -> Self {
        let mut bytes = Vec::with_capacity(a.bytes.len() + b.bytes.len() + 3);
        bytes.push(b'[');
        bytes.extend_from_slice(&a.bytes);
        bytes.push(b',');
        bytes.extend_from_slice(&b.bytes);
        bytes.push(b']');
        Self::from_bytes(bytes)
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
        index.insert(owner.clone(), serde_json::json!({"signature": declarations(declaration.clone()), "body": super::syntax::semantic(&body)}));
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
                table.input(&InputNode::EffectRoots(*unit))?;
                for &id in self.interface_dispatch.get(unit).into_iter().flatten() {
                    table.read(self, QueryNode::InterfaceDispatchTargets(*unit, id))?;
                }
                let mut paths = std::collections::HashSet::new();
                for body in &prepared.roots {
                    if self.checked_roots.contains(body) {
                        table.read(self, QueryNode::TypedBody(*body))?;
                    } else {
                        let binding = self.bodies.get(body).ok_or_else(|| {
                            anyhow::anyhow!("missing effect source binding {body:?}")
                        })?;
                        table.read(
                            self,
                            QueryNode::BodySyntax(binding.path.clone(), binding.owner.clone()),
                        )?;
                        table.read(
                            self,
                            QueryNode::SyntaxSignature(binding.path.clone(), binding.owner.clone()),
                        )?;
                    }
                    if let Some(binding) = self.bodies.get(body)
                        && paths.insert(binding.path.clone())
                    {
                        table.read(self, QueryNode::Parse(binding.path.clone()))?;
                    }
                }
                for (unit, function) in &prepared.imported {
                    table.read(self, QueryNode::EffectCapabilities(*unit, *function))?;
                }
                for unit in &prepared.source_units {
                    table.read(self, QueryNode::EffectInventory(*unit))?;
                }
                Ok(QueryValue::new(
                    EffectInventoryValue {
                        functions: prepared.inventory.clone(),
                        metadata: prepared.metadata.clone(),
                    },
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
                let syntax = table.read(
                    self,
                    QueryNode::BodySyntax(binding.path.clone(), binding.owner.clone()),
                )?;
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
                Ok(CompactJson::pair(syntax.get::<CompactJson>(), &prepared.result).into_query())
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
        self.roots.heap_bytes()
            + self.imported.heap_bytes()
            + self.inventory.heap_bytes()
            + self.source_units.heap_bytes()
            + self.metadata.heap_bytes()
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
    }
}
impl Retained for DerivedValue {
    fn heap_bytes(&self) -> usize {
        self.wire.capacity() + self.canonical.capacity()
    }
}
impl Retained for EffectInventoryValue {
    fn heap_bytes(&self) -> usize {
        self.functions.heap_bytes() + self.metadata.heap_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finish_prunes_inventory_for_units_absent_from_candidate() {
        let mut queries = SyntaxQueries::default();
        queries
            .dispatch_classes
            .insert(UnitId::ENTRY, ["Removed".into()].into());
        queries.interface_dispatch.insert(UnitId::ENTRY, vec![]);
        queries.effect_functions.insert(UnitId::ENTRY, vec![]);
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
    fn compact_json_roundtrips_and_pairs_without_reparsing() {
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
            for b in &cases {
                let pair = CompactJson::pair(&encoded, &CompactJson::new(b).unwrap());
                assert_eq!(pair.decode::<Value>().unwrap(), serde_json::json!([a, b]));
                assert_eq!(
                    pair.bytes.len(),
                    encoded.bytes.len() + serde_json::to_vec(b).unwrap().len() + 3
                );
            }
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
                .map(|i| format!("fn f{i}() -> i64 {{ return {i}; }}\n"))
                .collect::<String>();
            let mut accepted = SyntaxQueries::default().candidate().unwrap();
            accepted.parse(path, FileId::ENTRY, &source).unwrap();
            for i in 0..count {
                accepted.signature(path, &format!("Function:f{i}")).unwrap();
                accepted
                    .body_syntax(path, &format!("Function:f{i}"))
                    .unwrap();
            }
            let changed = source.replacen("return 0", "return 999", 1);
            let mut next = accepted.candidate().unwrap();
            next.parse(path, FileId::ENTRY, &changed).unwrap();
            for i in 0..count {
                assert_eq!(
                    accepted.signature(path, &format!("Function:f{i}")).unwrap(),
                    next.signature(path, &format!("Function:f{i}")).unwrap()
                );
                let before = accepted
                    .body_syntax(path, &format!("Function:f{i}"))
                    .unwrap();
                let after = next.body_syntax(path, &format!("Function:f{i}")).unwrap();
                assert_eq!(before == after, i != 0);
            }
            assert_eq!(next.stats().dependency_edges_visited, 4 + 2 * count);
            assert_eq!(next.stats().changed, 2); // parse and edited body projection
            assert_eq!(next.stats().green_after_recompute, 1 + 2 * count);
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
                    .map(|i| format!("fn f{i}() -> i64 {{ return {i}; }}\n"))
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
                                &format!("Function:f{i}"),
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
                                &format!("Function:f{i}"),
                                vec![],
                            )
                            .unwrap(),
                    );
                }
                assert_eq!(demands, if aggregate { count } else { 1 });
                let expected_visits = if aggregate {
                    5 * count + 3
                } else {
                    6 * count + 2
                };
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
