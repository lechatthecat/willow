//! Pure syntax queries; source capture and artifact I/O stay at the caller.
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
    interface_dispatch: HashMap<UnitId, Vec<FunctionId>>,
    pub(crate) dispatch_classes: HashMap<UnitId, std::collections::BTreeSet<String>>,
    captured: HashMap<PathBuf, Source>,
    advanced: bool,
    bodies: HashMap<BodyId, BodyBinding>,
    prepared: HashMap<BodyId, PreparedBody>,
    symbols: HashMap<(UnitId, String), Rc<Value>>,
    effect_functions: HashMap<UnitId, Vec<FunctionId>>,
    reference_wires: HashMap<super::references::SymbolUseId, Value>,
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
        Ok(serde_json::from_value(result.get::<Value>().clone())?)
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
        Ok(serde_json::from_value(result.get::<Value>().clone())?)
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
            self.reference_wires.insert(id.clone(), reference);
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
        Ok(self
            .reference_wires
            .get(&id)
            .unwrap_or_else(|| result.get::<Value>())
            .clone())
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
        Ok(result
            .get::<Vec<(super::references::SymbolUseId, Value)>>()
            .iter()
            .map(|(id, semantic)| self.reference_wires.get(id).unwrap_or(semantic).clone())
            .collect())
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
        Ok(self
            .table
            .read(
                &self.provider(),
                QueryNode::VisibleScope(unit, path.to_owned()),
            )?
            .get::<Value>()
            .clone())
    }
    pub(crate) fn candidate(&self) -> Result<Self> {
        Ok(Self {
            table: self.table.candidate()?,
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
        })
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
                result,
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
            Ok(result) => Ok(Some(result.get::<DerivedValue>().wire.clone())),
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
        let canonical = super::syntax::without_spans(&wire);
        let fingerprint = ResultFingerprint::bytes(&serde_json::to_vec(&canonical)?);
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
        let canonical = super::syntax::without_spans(&wire);
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
                metadata,
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
    pub(crate) fn finish(&mut self) -> Result<()> {
        self.table
            .retain_sources(&self.captured.keys().cloned().collect());
        Ok(())
    }
    /// Owner is a declaration path, e.g. `Function:f` or `Class:C/methods:f`.
    #[cfg(test)]
    pub(crate) fn signature(&self, path: &Path, owner: &str) -> Result<Value> {
        Ok(self
            .table
            .read(
                &self.provider(),
                QueryNode::SyntaxSignature(path.to_owned(), owner.to_owned()),
            )?
            .get::<Value>()
            .clone())
    }
    #[cfg(test)]
    pub(crate) fn body_syntax(&self, path: &Path, owner: &str) -> Result<Value> {
        Ok(self
            .table
            .read(
                &self.provider(),
                QueryNode::BodySyntax(path.to_owned(), owner.to_owned()),
            )?
            .get::<Value>()
            .clone())
    }
    #[cfg(test)]
    pub(crate) fn recomputations(&self, node: &QueryNode) -> usize {
        self.table.recomputations(node)
    }
    #[cfg(test)]
    pub(crate) fn query_dependencies(&self, node: &QueryNode) -> Vec<QueryNode> {
        self.table.query_dependencies(node)
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
            self.table.capture_candidate(node, value);
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
        let values = result.get::<ParsedValue>().wire.as_array().unwrap();
        if let Some(previous) = previous {
            let old: TokenInventory =
                serde_json::from_value(previous.get::<ParsedValue>().wire[4].clone())?;
            let new: TokenInventory = serde_json::from_value(values[4].clone())?;
            self.token_pairs.insert(path.to_owned(), (old, new));
        }
        Ok((
            serde_json::from_value(values[0].clone())?,
            serde_json::from_value(values[1].clone())?,
            values[2].as_bool().unwrap(),
        ))
    }
}

#[derive(Clone)]
struct DerivedValue {
    wire: Value,
    canonical: Value,
}
impl PartialEq for DerivedValue {
    fn eq(&self, other: &Self) -> bool {
        self.canonical == other.canonical
    }
}
impl Eq for DerivedValue {}

struct ParsedValue {
    wire: Value,
    canonical: Value,
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
    result: Value,
    keys: Vec<String>,
    children: Vec<BodyId>,
}
struct PreparedEffects {
    roots: Vec<BodyId>,
    imported: Vec<(UnitId, FunctionId)>,
    inventory: HashMap<FunctionId, super::effects::CapturedEffect>,
    source_units: Vec<UnitId>,
    metadata: Value,
}
#[derive(Clone, PartialEq, Eq)]
struct EffectInventoryValue {
    functions: HashMap<FunctionId, super::effects::CapturedEffect>,
    metadata: Value,
}
struct SyntaxProvider<'a> {
    interface_dispatch: &'a HashMap<UnitId, Vec<FunctionId>>,
    bodies: &'a HashMap<BodyId, BodyBinding>,
    prepared: &'a HashMap<BodyId, PreparedBody>,
    effects: &'a HashMap<UnitId, PreparedEffects>,
    derived: &'a HashMap<QueryNode, DerivedValue>,
    checked_roots: &'a std::collections::HashSet<BodyId>,
}
fn value(value: Value) -> Result<QueryValue> {
    let fingerprint = ResultFingerprint::bytes(&serde_json::to_vec(&value)?);
    Ok(QueryValue::new(value, fingerprint))
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
            QueryNode::Parse(path) => {
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
                let mut parsed = serde_json::to_value((program, diagnostics, lexer_failed))?;
                let mut index = serde_json::Map::new();
                index_declarations(&parsed[0]["items"], "", &mut index);
                parsed.as_array_mut().unwrap().push(Value::Object(index));
                parsed
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::to_value(inventory)?);
                let canonical = super::syntax::without_ids(&parsed);
                let fingerprint = ResultFingerprint::bytes(&serde_json::to_vec(&canonical)?);
                Ok(QueryValue::new(
                    ParsedValue {
                        wire: parsed,
                        canonical,
                    },
                    fingerprint,
                ))
            }
            QueryNode::SyntaxDeclarations(path) | QueryNode::SyntaxImports(path) => {
                let parsed = table.read(self, QueryNode::Parse(path.clone()))?;
                let program = &parsed.get::<ParsedValue>().wire[0];
                if matches!(node, QueryNode::SyntaxImports(_)) {
                    value(super::syntax::semantic(&program["imports"]))
                } else {
                    value(declarations(program["items"].clone()))
                }
            }
            QueryNode::SyntaxSignature(path, owner) | QueryNode::BodySyntax(path, owner) => {
                let parsed = table.read(self, QueryNode::Parse(path.clone()))?;
                let entry = &parsed.get::<ParsedValue>().wire[3][owner];
                let field = if matches!(node, QueryNode::BodySyntax(_, _)) {
                    "body"
                } else {
                    "signature"
                };
                value(entry[field].clone())
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
                let fingerprint =
                    ResultFingerprint::bytes(&serde_json::to_vec(&prepared.canonical)?);
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
                            .get::<Value>()
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
                value(serde_json::json!([syntax.get::<Value>(), prepared.result]))
            }
            _ => anyhow::bail!("unsupported syntax query {node:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
                .get::<Value>(),
            &scope
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
        assert_eq!(next.stats().dependency_edges_visited, 3);
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
            assert_eq!(next.stats().dependency_edges_visited, 3 + 2 * count);
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
                    5 * count + 2
                } else {
                    6 * count + 1
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
