//! Compact compiler facts, structured control flow, and snapshot query adapter.
use super::*;
use serde_json::{Value, json};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticFacts {
    #[serde(default)]
    pub interop: interop::InteropGraph,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rust_bridges: Vec<crate::rust_bridge::RustBridgeSymbol>,
    /// Live-only declaration relationships for atomic semantic rename.
    #[serde(skip)]
    pub(crate) rename_links: Vec<(String, String)>,
    #[serde(skip)]
    pub(crate) rename_scopes: HashMap<String, Vec<(usize, usize)>>,
    #[serde(default)]
    pub modules: Vec<ModuleEvidence>,
    pub symbols: Vec<symbols::Symbol>,
    pub references: Vec<symbols::Reference>,
    pub expressions: Vec<Expression>,
    pub flows: Vec<Flow>,
    pub witnesses: BTreeMap<String, Vec<Value>>,
    pub compiler_effects: BTreeMap<String, u8>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effect_calls: BTreeMap<String, Vec<EffectCall>>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub async_functions: BTreeSet<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unresolved_calls: BTreeMap<String, Vec<UnresolvedCall>>,
    #[serde(default, skip_serializing_if = "graph::Instances::is_empty")]
    pub instances: graph::Instances,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectCall {
    pub target: String,
    pub location: Location,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedCall {
    pub operation: String,
    pub location: Location,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expression {
    pub function: String,
    pub location: Location,
    pub ty: Option<Type>,
    pub target: Option<String>,
    pub operation: Option<String>,
    pub fingerprint: String,
    pub flow: usize,
    pub node: usize,
    pub occurrences: Vec<usize>,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flow {
    pub function: String,
    pub nodes: Vec<FlowNode>,
    pub entry: usize,
    pub complete: bool,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlowNode {
    pub location: Location,
    pub successors: Vec<usize>,
    pub loop_header: bool,
}
pub(super) struct CaptureContext<'a> {
    pub types: &'a HashMap<ExprId, Type>,
    pub calls: &'a HashMap<ExprId, Option<FunctionId>>,
    pub patterns: &'a HashMap<PatternId, Pattern>,
}
impl std::ops::Deref for CaptureContext<'_> {
    type Target = HashMap<ExprId, Type>;
    fn deref(&self) -> &Self::Target {
        self.types
    }
}
#[derive(Default)]
pub(super) struct Captured {
    pub rename_relations: super::rename::Relations,
    pub async_functions: std::collections::HashSet<FunctionId>,
    bodies: Vec<Body>,
    pub call_sites: Vec<(FunctionId, Span, Option<FunctionId>, String)>,
    pub witnesses: HashMap<FunctionId, Vec<Value>>,
    pub compiler_effects: HashMap<FunctionId, u8>,
    // Keep operation sites separate so capability replacement scans stay bounded.
    suspension_sites: HashMap<FunctionId, Vec<Value>>,
}
#[path = "flow.rs"]
mod flow;
use flow::Body;
// Prefer an explicit allocation over an incidental panic path or a propagated
// capability. Equal-ranked witnesses retain stable traversal order.
fn explicit_allocation(operation: Option<&str>) -> bool {
    matches!(
        operation,
        Some(
            "array-allocation"
                | "enum-allocation"
                | "object-allocation"
                | "object-literal-allocation"
                | "string-concatenation"
                | "lambda-environment"
        )
    )
}
fn prefer_allocation(effect: u8, candidate: &Value, existing: &Value) -> bool {
    effect == RuntimeEffects::MAY_ALLOCATE.bits()
        && explicit_allocation(candidate["cause"]["operation"].as_str())
        && !explicit_allocation(existing["cause"]["operation"].as_str())
}
impl Captured {
    pub(super) fn map_types(&mut self, normalize: impl Fn(&Type) -> Type) {
        for body in &mut self.bodies {
            for (_, ty, _, _, _) in &mut body.expressions {
                if let Some(ty) = ty {
                    *ty = normalize(ty);
                }
            }
        }
    }
    pub fn channel_site(&mut self, function: FunctionId, span: Span, operation: &str) {
        self.suspension_sites.entry(function).or_default().push(json!({
            "effect": RuntimeEffects::MAY_SUSPEND.bits(),
            "witness": {"kind":"runtime-capability", "certainty":"conservative-bound",
                "cause":{"span":span,"operation":operation},
                "reason":"May park the task when the channel cannot proceed; this is not evidence of blocking an OS thread."}
        }));
    }
    pub fn capability(&mut self, function: FunctionId, bits: u8, span: Span, operation: &str) {
        let witnesses = self.witnesses.entry(function).or_default();
        for bit in 0..RuntimeEffects::BIT_COUNT {
            let effect = RuntimeEffects::from_bit(bit).bits();
            if bits & effect == 0 {
                continue;
            }
            let existing = witnesses.iter().position(|w| w["effect"] == effect);
            if existing.is_some_and(|i| {
                let replace = matches!(
                    witnesses[i]["witness"]["kind"].as_str(),
                    Some("unavailable" | "external-boundary")
                ) || (effect == RuntimeEffects::MAY_ALLOCATE.bits()
                    && explicit_allocation(Some(operation))
                    && !explicit_allocation(
                        witnesses[i]["witness"]["cause"]["operation"].as_str(),
                    ));
                !replace
            }) {
                continue;
            }
            let mut value = json!({"effect":effect,"witness":{"kind":"runtime-capability","certainty":"conservative-bound","cause":{"span":span,"operation":operation}}});
            if effect == RuntimeEffects::MAY_BLOCK.bits() && operation == "await" {
                value["witness"]["reason"] = json!(
                    "conservative await bound for the checked operand type: waiting may park or block depending on the selected lowering; this summary does not establish which path executes"
                );
            }
            if operation == "array-length-null-panic" {
                value["witness"]["reason"] = json!(
                    "Normal Array.len()/FrozenArray.len() reads the length without allocating. Only a null receiver (or a malformed negative length) reaches willow_array_len's panic path, which may allocate the panic payload; receiver non-nullness is not proven by this summary."
                );
                if effect == RuntimeEffects::MAY_ALLOCATE.bits() {
                    value["witness"]["cause"]["operation"] = json!("panic-payload-allocation");
                }
            }
            if let Some(i) = existing {
                witnesses[i] = value;
            } else {
                witnesses.push(value);
            }
        }
        witnesses.sort_by_key(|w| w["effect"].as_u64().unwrap());
    }
    /// Attach the awaited operand type and explain the await's bound. `parks`
    /// is true when the async lowering is known to suspend the task instead of
    /// blocking an OS thread.
    pub fn await_type(
        &mut self,
        function: FunctionId,
        span: Span,
        operand: Option<&str>,
        parks: bool,
    ) {
        let operand_name = operand.unwrap_or("an unresolved operand type");
        let reason = if parks {
            format!(
                "In an async function, await of {operand_name} lowers to a scheduler suspension: the task parks until the awaited operation completes; no OS thread blocks."
            )
        } else {
            format!(
                "Conservative await bound for {operand_name}; waiting may park or block depending on the selected lowering. This summary does not establish which path executes."
            )
        };
        if let Some(witnesses) = self.witnesses.get_mut(&function) {
            let site = json!(span);
            for entry in witnesses {
                let witness = &mut entry["witness"];
                // Panic witnesses reconstruct spans without line/column caches.
                if ["file_id", "start", "end"]
                    .iter()
                    .all(|key| witness["cause"]["span"][key] == site[key])
                {
                    witness["awaited_type"] = json!(operand);
                    witness["reason"] = json!(reason);
                }
            }
        }
    }
    pub fn spans(&self) -> impl Iterator<Item = Span> + '_ {
        self.bodies
            .iter()
            .flat_map(|b| b.expressions.iter().map(|e| e.0))
    }
    pub fn body(&mut self, function: FunctionId, root: AstEvent<'_>, types: &CaptureContext<'_>) {
        self.bodies.push(Body::build(function, root, types));
    }
}
/// Status of one effect witness: a witness without a source location is
/// `missing-source`; runtime capabilities are conservative bounds.
fn evidence_status(witness: &Value) -> &'static str {
    if witness["kind"] == "unavailable" {
        "missing-witness"
    } else if witness.get("cause").is_some() && !witness["cause"]["location"].is_object() {
        "missing-source"
    } else if witness["kind"] == "runtime-capability" {
        "conservative-bound"
    } else if witness["kind"] == "external-boundary" {
        "external-boundary"
    } else {
        "compiler-fact"
    }
}
pub(super) fn finish(
    captures: &HashMap<UnitId, CapturedUnit>,
    paths: &HashMap<UnitId, String>,
    functions: &HashMap<(UnitId, FunctionId), Function>,
    resolve: &impl Fn(UnitId, FunctionId) -> (UnitId, FunctionId),
    bodyless: &impl Fn(UnitId, &FunctionId) -> bool,
    fingerprints: &HashMap<Span, String>,
) -> Result<SemanticFacts> {
    let location = |span: Span| -> Result<Location> {
        Ok(Location {
            path: paths
                .get(&crate::module::ModuleId(span.file_id.0))
                .context("semantic source")?
                .clone(),
            start: span.start,
            end: span.end,
        })
    };
    let async_functions = captures
        .iter()
        .flat_map(|(&unit, capture)| {
            capture
                .semantic
                .async_functions
                .iter()
                .filter_map(move |&id| {
                    functions
                        .get(&resolve(unit, id))
                        .map(|function| function.id.clone())
                })
        })
        .collect();
    let mut result = SemanticFacts {
        async_functions,
        ..SemanticFacts::default()
    };
    let mut units: Vec<_> = captures.keys().copied().collect();
    units.sort();
    for unit in units {
        for (caller, span, target, operation) in &captures[&unit].semantic.call_sites {
            // Virtual calls may deliberately have no single checked target.
            // Their receiver/method identity still locates the dispatch edge.
            let effect_target = captures[&unit].dispatch_sites.get(span).or(target.as_ref());
            if let Some(target) = effect_target
                && let (Some(caller), Some(target)) = (
                    functions.get(&resolve(unit, *caller)),
                    functions.get(&resolve(unit, *target)),
                )
            {
                result
                    .effect_calls
                    .entry(caller.id.clone())
                    .or_default()
                    .push(EffectCall {
                        target: target.id.clone(),
                        location: location(*span)?,
                    });
            }
            // Calls whose effects are already known without a body (variant
            // constructors, intrinsics) do not explain an unknown caller.
            if target.is_some_and(|target| {
                functions.contains_key(&resolve(unit, target)) || bodyless(unit, &target)
            }) {
                continue;
            }
            if let Some(caller) = functions.get(&resolve(unit, *caller)) {
                result
                    .unresolved_calls
                    .entry(caller.id.clone())
                    .or_default()
                    .push(UnresolvedCall {
                        operation: operation.clone(),
                        location: location(*span)?,
                    });
            }
        }
        for (&id, &effects) in &captures[&unit].semantic.compiler_effects {
            if let Some(function) = functions.get(&resolve(unit, id)) {
                *result
                    .compiler_effects
                    .entry(function.id.clone())
                    .or_default() |= effects;
            }
        }
        for (id, witnesses) in captures[&unit]
            .semantic
            .witnesses
            .iter()
            .chain(&captures[&unit].semantic.suspension_sites)
        {
            let function = &functions[&resolve(unit, *id)].id;
            let mut witnesses = witnesses.clone();
            for witness in &mut witnesses {
                if let Some(w) = witness.get_mut("witness")
                    && let Some(owner) = w.as_object_mut().and_then(|w| w.remove("owner_key"))
                {
                    let owner: FunctionId = serde_json::from_value(owner)?;
                    if let Some(function) = functions.get(&resolve(unit, owner)) {
                        w["owner"] = json!(function.id);
                    } else {
                        w["owner"] = json!(owner.to_string());
                    }
                }
                if let Some(cause) = witness.get_mut("witness").and_then(|w| w.get_mut("cause"))
                    && let Some(span) = cause.get("span")
                {
                    let span: Span = serde_json::from_value(span.clone())?;
                    cause.as_object_mut().unwrap().remove("span");
                    cause.as_object_mut().unwrap().insert(
                        "location".into(),
                        location(span)
                            .ok()
                            .map_or(Value::Null, |l| serde_json::to_value(l).unwrap()),
                    );
                    // A `lock` statement's operand span becomes a location the
                    // direct layer spells from source.
                    if let Some(target) = cause.pointer_mut("/lock/target") {
                        let span: Span = serde_json::from_value(target.take())?;
                        *target = location(span)
                            .ok()
                            .map_or(Value::Null, |l| serde_json::to_value(l).unwrap());
                    }
                }
            }
            result
                .witnesses
                .entry(function.clone())
                .or_default()
                .extend(witnesses);
        }
        for body in &captures[&unit].semantic.bodies {
            let function = functions
                .get(&resolve(unit, body.function))
                .context("semantic function")?
                .id
                .clone();
            let flow = result.flows.len();
            result.flows.push(Flow {
                function: function.clone(),
                entry: body.entry,
                complete: body.complete,
                nodes: body
                    .nodes
                    .iter()
                    .map(|(span, successors, loop_header)| {
                        Ok(FlowNode {
                            location: location(*span)?,
                            successors: successors.clone(),
                            loop_header: *loop_header,
                        })
                    })
                    .collect::<Result<_>>()?,
            });
            let mut expressions = HashMap::new();
            for (span, ty, target, operation, node) in &body.expressions {
                let key = (*span, operation.as_deref());
                if let Some(&i) = expressions.get(&key) {
                    let expression: &mut Expression = &mut result.expressions[i];
                    if expression.occurrences.last() != Some(node) {
                        expression.occurrences.push(*node);
                    }
                    continue;
                }
                expressions.insert(key, result.expressions.len());
                result.expressions.push(Expression {
                    function: function.clone(),
                    location: location(*span)?,
                    ty: ty.clone(),
                    target: target
                        .and_then(|t| functions.get(&resolve(unit, t)))
                        .map(|f| f.id.clone()),
                    operation: operation.clone(),
                    fingerprint: fingerprints
                        .get(span)
                        .context("semantic fingerprint")?
                        .clone(),
                    flow,
                    node: *node,
                    occurrences: vec![*node],
                });
            }
        }
    }
    for facts in result.witnesses.values_mut() {
        deduplicate_evidence(facts);
    }
    Ok(result)
}

/// Disjoint sorted declaration ranges support local-site membership without
/// multiplying the number of witnesses by the number of declaration ranges.
fn local_ranges(locations: &[Location]) -> Vec<(&str, usize, usize)> {
    let mut ranges: Vec<_> = locations
        .iter()
        .map(|l| (l.path.as_str(), l.start, l.end))
        .collect();
    ranges.sort_unstable();
    let mut merged: Vec<(&str, usize, usize)> = Vec::with_capacity(ranges.len());
    for (path, start, end) in ranges {
        if let Some(last) = merged.last_mut()
            && last.0 == path
            && start <= last.2
        {
            last.2 = last.2.max(end);
        } else {
            merged.push((path, start, end));
        }
    }
    merged
}

/// One source site/effect is one explanation, even when both the compiler
/// and runtime ABI observed it. Keep source-less evidence independent.
fn deduplicate_evidence(facts: &mut Vec<Value>) {
    let mut seen = HashMap::new();
    let mut unique: Vec<Value> = Vec::with_capacity(facts.len());
    for fact in facts.drain(..) {
        let location = &fact["witness"]["cause"]["location"];
        if !location.is_object() {
            unique.push(fact);
            continue;
        }
        let key = (fact["effect"].as_u64(), location.to_string());
        if let Some(&index) = seen.get(&key) {
            // Channel capability has the precise parking explanation and a
            // canonical operation name, unlike the generic compiler wait fact.
            if fact["witness"]["cause"]["operation"]
                .as_str()
                .is_some_and(|op| op.starts_with("channel-"))
            {
                unique[index] = fact;
            }
        } else {
            seen.insert(key, unique.len());
            unique.push(fact);
        }
    }
    *facts = unique;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum QueryRequest {
    RustBridge {
        revision: String,
        symbol: Option<String>,
    },
    Affected {
        revision: String,
        delta: UpdateDelta,
        #[serde(default)]
        tests: Vec<String>,
    },
    /// All declarations, optionally narrowed. Filters combine with AND;
    /// `limit` caps the returned list after filtering (`total` is uncapped).
    Symbols {
        revision: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        module: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        symbol_kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    SymbolAt {
        revision: String,
        file: String,
        byte: usize,
    },
    SymbolInfo {
        revision: String,
        function: String,
    },
    References {
        revision: String,
        function: String,
    },
    TypeAt {
        revision: String,
        file: String,
        byte: usize,
    },
    Effects {
        revision: String,
        function: String,
    },
}
/// Own one immutable revision and its indexes. Query calls do no frontend work.
pub struct QuerySession {
    packages: packages::PackageIndex,
    pub(super) names: display::TypeNames,
    pub(crate) snapshot: Snapshot,
    functions: HashMap<String, usize>,
    symbols: HashMap<String, usize>,
    symbol_references: HashMap<String, Vec<usize>>,
    symbol_positions: HashMap<String, Vec<(usize, usize, String)>>,
    references: HashMap<String, Vec<usize>>,
    positions: HashMap<String, Vec<Segment>>,
    /// Callee tokens with a receiver-instantiated member type, per file,
    /// sorted by `(start, end)`; values index `semantic.references`.
    typed_calls: HashMap<String, Vec<(usize, usize, usize)>>,
    callees: Vec<Vec<usize>>,
    async_functions: std::collections::HashSet<usize>,
    dispatch_callers: Vec<Vec<usize>>,
    indirect_references: HashMap<Type, Vec<usize>>,
    pub queries: usize,
    pub position_comparisons: usize,
    pub effect_edge_visits: usize,
}
/// A maximal source range with one constant typing answer for `TypeAt`.
struct Segment {
    start: usize,
    end: usize,
    /// Declared token priority: `Some(true)` for a resolved write
    /// (authoritative), `Some(false)` for a declaration name such as a binding
    /// or parameter, or a callee token (used when that symbol is typed).
    declared: Option<bool>,
    /// Narrowest enclosing expression; `Some(None)` when several tie.
    expression: Option<Option<usize>>,
}
const EXPRESSION: u8 = 0;
const DECLARATION: u8 = 1;
const WRITE: u8 = 2;
impl QuerySession {
    pub fn new(snapshot: Snapshot) -> Result<Self> {
        snapshot.validate()?;
        let functions: HashMap<String, usize> = snapshot
            .functions
            .iter()
            .enumerate()
            .map(|(i, f)| (f.id.clone(), i))
            .collect();
        let mut references: HashMap<String, Vec<usize>> = HashMap::new();
        // Expression, declaration, call and write spans share one sweep, so a
        // position query is a single binary search over one segment index.
        let mut events: HashMap<String, Vec<(usize, bool, u8, usize)>> = HashMap::new();
        let mut span = |l: &Location, kind: u8, i: usize| {
            if l.start < l.end {
                let file = events.entry(l.path.clone()).or_default();
                file.push((l.start, true, kind, i));
                file.push((l.end, false, kind, i));
            }
        };
        for (i, e) in snapshot.semantic.expressions.iter().enumerate() {
            if let Some(target) = &e.target {
                references.entry(target.clone()).or_default().push(i);
            }
            span(&e.location, EXPRESSION, i);
        }
        for (i, symbol) in snapshot.semantic.symbols.iter().enumerate() {
            if let Some(l) = &symbol.location {
                span(l, DECLARATION, i);
            }
        }
        for (i, r) in snapshot.semantic.references.iter().enumerate() {
            if matches!(r.role.as_str(), "write" | "write-element") {
                span(&r.location, WRITE, i);
            } else if matches!(r.role.as_str(), "call" | "variant" | "pattern" | "type") {
                // Only the callee, variant or type token gets its declared type
                // (a pattern variant has no expression of its own). Preserve
                // the call expression and fall back to it for untyped targets.
                span(&r.location, DECLARATION, i);
            }
        }
        let positions = events
            .into_iter()
            .map(|(path, mut events)| {
                events.sort_unstable();
                let mut active = std::collections::BTreeSet::new();
                let mut tokens = [0usize; 3];
                let mut segments = Vec::new();
                let mut p = 0;
                while p < events.len() {
                    let at = events[p].0;
                    while p < events.len() && events[p].0 == at {
                        let (_, add, kind, i) = events[p];
                        if kind == EXPRESSION {
                            let l = &snapshot.semantic.expressions[i].location;
                            let key = (l.end - l.start, i);
                            if add {
                                active.insert(key);
                            } else {
                                active.remove(&key);
                            }
                        } else if add {
                            tokens[kind as usize] += 1;
                        } else {
                            tokens[kind as usize] -= 1;
                        }
                        p += 1;
                    }
                    let declared = if tokens[WRITE as usize] > 0 {
                        Some(true)
                    } else {
                        (tokens[DECLARATION as usize] > 0).then_some(false)
                    };
                    if p < events.len() && (declared.is_some() || !active.is_empty()) {
                        let mut it = active.iter();
                        let expression = it.next().map(|&(width, i)| {
                            it.next().is_none_or(|&(w, _)| w != width).then_some(i)
                        });
                        segments.push(Segment {
                            start: at,
                            end: events[p].0,
                            declared,
                            expression,
                        });
                    }
                }
                (path, segments)
            })
            .collect();
        let symbols: HashMap<String, usize> = snapshot
            .semantic
            .symbols
            .iter()
            .enumerate()
            .map(|(i, s)| (s.id.clone(), i))
            .collect();
        let mut symbol_references: HashMap<String, Vec<usize>> = HashMap::new();
        let mut symbol_positions: HashMap<String, Vec<(usize, usize, String)>> = HashMap::new();
        for symbol in &snapshot.semantic.symbols {
            if let Some(l) = &symbol.location {
                symbol_positions.entry(l.path.clone()).or_default().push((
                    l.start,
                    l.end,
                    symbol.id.clone(),
                ));
            }
        }
        for (i, r) in snapshot.semantic.references.iter().enumerate() {
            symbol_references
                .entry(r.target.clone())
                .or_default()
                .push(i);
            let l = &r.location;
            symbol_positions.entry(l.path.clone()).or_default().push((
                l.start,
                l.end,
                r.target.clone(),
            ));
        }
        // Token references are authoritative. Keep expression evidence only
        // where no resolved token exists (implicit constructors, dispatch).
        for (target, expressions) in &mut references {
            if let Some(indices) = symbol_references.get_mut(target) {
                indices.sort_by_key(|&i| {
                    let l = &snapshot.semantic.references[i].location;
                    (&l.path, l.start, l.end)
                });
                expressions.retain(|&i| {
                    let expression = &snapshot.semantic.expressions[i].location;
                    let p = indices.partition_point(|&j| {
                        let l = &snapshot.semantic.references[j].location;
                        (&l.path, l.start) < (&expression.path, expression.start)
                    });
                    !indices.get(p).is_some_and(|&j| {
                        let l = &snapshot.semantic.references[j].location;
                        l.path == expression.path && l.end <= expression.end
                    })
                });
            }
        }
        for positions in symbol_positions.values_mut() {
            positions.sort();
            positions.dedup();
        }
        let mut indirect_references: HashMap<Type, Vec<usize>> = HashMap::new();
        for (i, r) in snapshot.semantic.references.iter().enumerate() {
            if r.role == "call"
                && let Some(&s) = symbols.get(&r.target)
                && matches!(
                    snapshot.semantic.symbols[s].kind.as_str(),
                    "binding" | "parameter"
                )
                && let Some(ty) = &snapshot.semantic.symbols[s].ty
            {
                indirect_references.entry(ty.clone()).or_default().push(i);
            }
        }
        let graph = graph::Index::new(&snapshot.functions);
        let dispatch_callers = graph
            .callers
            .into_iter()
            .map(|callers| {
                callers
                    .into_iter()
                    .filter(|&i| snapshot.functions[i].synthetic)
                    .collect()
            })
            .collect();
        let callees = graph.callees;
        let packages = packages::PackageIndex::new(&snapshot);
        let mut typed_calls: HashMap<String, Vec<(usize, usize, usize)>> = HashMap::new();
        for (i, r) in snapshot.semantic.references.iter().enumerate() {
            if r.role == "call" && r.ty.is_some() && r.location.start < r.location.end {
                typed_calls
                    .entry(r.location.path.clone())
                    .or_default()
                    .push((r.location.start, r.location.end, i));
            }
        }
        for calls in typed_calls.values_mut() {
            calls.sort_unstable();
        }
        let names = display::TypeNames::new(&snapshot);
        let async_functions = snapshot
            .semantic
            .async_functions
            .iter()
            .filter_map(|id| functions.get(id).copied())
            .collect();
        Ok(Self {
            packages,
            names,
            snapshot,
            functions,
            references,
            symbols,
            symbol_references,
            symbol_positions,
            positions,
            typed_calls,
            callees,
            async_functions,
            dispatch_callers,
            indirect_references,
            queries: 0,
            position_comparisons: 0,
            effect_edge_visits: 0,
        })
    }
    fn symbols_at(&self, file: &str, byte: usize) -> Vec<&symbols::Symbol> {
        let file = if Path::new(file).is_absolute() {
            file.to_owned()
        } else {
            Path::new(&self.snapshot.workspace)
                .join(file)
                .to_string_lossy()
                .into_owned()
        };
        let Some(positions) = self.symbol_positions.get(&file) else {
            return vec![];
        };
        let end = positions.partition_point(|p| p.0 <= byte);
        let Some(last) = end.checked_sub(1).map(|i| &positions[i]) else {
            return vec![];
        };
        if byte >= last.1 {
            return vec![];
        }
        let start = positions.partition_point(|p| p.0 < last.0);
        positions[start..end]
            .iter()
            .filter_map(|p| self.symbols.get(&p.2))
            .map(|&i| &self.snapshot.semantic.symbols[i])
            .collect()
    }
    fn symbol_values(&self, symbols: &[&symbols::Symbol]) -> Vec<Value> {
        symbols.iter().map(|s| self.names.symbol(s)).collect()
    }
    fn symbol_at(&self, file: &str, byte: usize) -> Value {
        let symbols = self.symbols_at(file, byte);
        if symbols.is_empty() {
            json!({"status":"unknown"})
        } else if symbols.len() == 1 {
            json!({"status":"ok","symbol":self.names.symbol(symbols[0])})
        } else if let Some(alias) = symbols.iter().find(|s| s.kind == "import") {
            json!({"status":"ok","symbol":self.names.symbol(alias),"resolved_symbols":self.symbol_values(&symbols)})
        } else {
            json!({"status":"ambiguous","symbols":self.symbol_values(&symbols)})
        }
    }
    fn declared_type_at(&self, file: &str, byte: usize) -> Value {
        let symbols = self.symbols_at(file, byte);
        let typed: Vec<_> = symbols.iter().filter(|s| s.ty.is_some()).collect();
        if let Some(first) = typed.first() {
            if typed.iter().all(|s| s.ty == first.ty) {
                let display = self.names.declaration(first, first.ty.as_ref().unwrap());
                let mut result = json!({"status":"ok","type":first.ty,"type_display":display,"location":first.location});
                if let [only] = typed[..] {
                    self.names.payload(only, &mut result);
                }
                result
            } else {
                json!({"status":"ambiguous","symbols":self.symbol_values(&symbols)})
            }
        } else {
            json!({"status":if symbols.is_empty(){"unknown"}else{"unanalyzed"}})
        }
    }
    /// Spell each acquired lock's operand type like `type` does, in the module
    /// of the locking body: one decode and render per lock witness.
    fn present_lock_types(&self, facts: &mut [Value]) {
        for fact in facts {
            let module = fact["witness"]["owner"]
                .as_str()
                .and_then(|owner| self.functions.get(owner))
                .and_then(|&i| self.snapshot.functions[i].identity.as_ref())
                .map(|identity| identity.module.as_str());
            let Some(lock) = fact.pointer_mut("/witness/cause/lock") else {
                continue;
            };
            if let Some(ty) = lock
                .get("type")
                .and_then(|ty| serde_json::from_value::<Type>(ty.clone()).ok())
            {
                lock["type_display"] = json!(self.names.render(&ty, module));
            }
        }
    }
    /// Module of the function owning expression record `index`.
    fn expression_module(&self, index: usize) -> Option<&str> {
        self.functions
            .get(&self.snapshot.semantic.expressions[index].function)
            .and_then(|&i| self.snapshot.functions[i].identity.as_ref())
            .map(|i| i.module.as_str())
    }
    /// A member callee token answers with its receiver-instantiated type
    /// (`fn() -> Option<Patient>` through `PQueue<Patient>`), not the
    /// declaration's generic signature. Callee tokens never nest, so the
    /// nearest token starting at or before `byte` is the only candidate.
    fn typed_call_at(&self, file: &str, byte: usize, module: Option<&str>) -> Option<Value> {
        let calls = self.typed_calls.get(file)?;
        let p = calls.partition_point(|&(start, _, _)| start <= byte);
        let &(_, end, i) = calls.get(p.checked_sub(1)?)?;
        if byte >= end {
            return None;
        }
        let r = &self.snapshot.semantic.references[i];
        let ty = r.ty.as_ref()?;
        let display = self.names.render(ty, module);
        Some(json!({"status":"ok","type":ty,"type_display":display,"location":r.location}))
    }
    pub fn revision(&self) -> &str {
        &self.snapshot.revision
    }
    pub fn query(&mut self, request: QueryRequest) -> Value {
        self.queries += 1;
        let revision = match &request {
            QueryRequest::RustBridge { revision, .. }
            | QueryRequest::Affected { revision, .. }
            | QueryRequest::Symbols { revision, .. }
            | QueryRequest::SymbolAt { revision, .. }
            | QueryRequest::SymbolInfo { revision, .. }
            | QueryRequest::References { revision, .. }
            | QueryRequest::TypeAt { revision, .. }
            | QueryRequest::Effects { revision, .. } => revision,
        };
        if revision != self.revision() {
            return json!({"status":"stale", "revision":self.revision()});
        }
        let data = match request {
            QueryRequest::RustBridge { symbol, .. } => {
                self.snapshot.semantic.interop.query(symbol.as_deref())
            }
            QueryRequest::Affected { delta, tests, .. } => {
                self.packages.affected(&self.snapshot, &delta, &tests)
            }
            QueryRequest::Symbols {
                name,
                prefix,
                module,
                symbol_kind,
                limit,
                ..
            } => {
                let matched: Vec<_> = self
                    .snapshot
                    .semantic
                    .symbols
                    .iter()
                    .filter(|s| name.as_ref().is_none_or(|n| s.name == *n))
                    .filter(|s| {
                        prefix
                            .as_ref()
                            .is_none_or(|p| s.name.starts_with(p.as_str()))
                    })
                    .filter(|s| symbol_kind.as_ref().is_none_or(|k| s.kind == *k))
                    .filter(|s| {
                        module
                            .as_ref()
                            .is_none_or(|m| s.identity.as_ref().is_some_and(|i| i.module == *m))
                    })
                    .collect();
                let total = matched.len();
                let shown = limit.map_or(total, |l| l.min(total));
                json!({"status":"ok","symbols":self.symbol_values(&matched[..shown]),
                    "total":total,"truncated":shown < total})
            }
            QueryRequest::SymbolAt { file, byte, .. } => self.symbol_at(&file, byte),
            QueryRequest::TypeAt { file, byte, .. } => {
                let file = if Path::new(&file).is_absolute() {
                    file
                } else {
                    Path::new(&self.snapshot.workspace)
                        .join(file)
                        .to_string_lossy()
                        .into_owned()
                };
                let Some(segments) = self.positions.get(&file) else {
                    return json!({"revision":self.revision(),"result":self.declared_type_at(&file,byte)});
                };
                let p = segments.partition_point(|s| {
                    self.position_comparisons += 1;
                    s.start <= byte
                });
                let Some(segment) = p
                    .checked_sub(1)
                    .map(|p| &segments[p])
                    .filter(|s| byte < s.end)
                else {
                    return json!({"revision":self.revision(),"result":self.declared_type_at(&file,byte)});
                };
                // Resolved source write tokens are authoritative: compound lowering
                // can put several synthetic expressions on the same token span.
                // A callee token or declaration name (parameter, pattern binding,
                // let) can lie inside an expression; report its declared type
                // unless the symbol itself is untyped.
                if let Some(write) = segment.declared {
                    // The instantiated type is spelled in the calling body,
                    // whose enclosing expression names its module.
                    let caller = segment
                        .expression
                        .flatten()
                        .map(|i| self.expression_module(i));
                    if !write && let Some(typed) = self.typed_call_at(&file, byte, caller.flatten())
                    {
                        return json!({"revision":self.revision(),"result":typed});
                    }
                    let declared = self.declared_type_at(&file, byte);
                    // A type token of `new Box<i64>(..)` keeps the
                    // instantiation its expression carries; the declaration
                    // only knows `Box<T>`.
                    let instantiated = !write
                        && segment.expression.flatten().is_some_and(|i| {
                            let Some(Type::Generic(name, _)) =
                                &self.snapshot.semantic.expressions[i].ty
                            else {
                                return false;
                            };
                            self.symbols_at(&file, byte).iter().any(|s| {
                                matches!(&s.ty, Some(Type::Named(head) | Type::Generic(head, _)) if head == name)
                            })
                        });
                    if !instantiated
                        && (write
                            || matches!(declared["status"].as_str(), Some("ok" | "ambiguous")))
                    {
                        return json!({"revision":self.revision(),"result":declared});
                    }
                }
                let Some(index) = segment.expression else {
                    return json!({"revision":self.revision(),"result":self.declared_type_at(&file,byte)});
                };
                let Some(index) = index else {
                    return json!({"revision":self.revision(),"result":{"status":"ambiguous"}});
                };
                let e = &self.snapshot.semantic.expressions[index];
                if let Some(ty) = &e.ty {
                    let module = self.expression_module(index);
                    let display = self.names.render(ty, module);
                    json!({"status":"ok","type":ty,"type_display":display,"location":e.location})
                } else {
                    let declared = self.declared_type_at(&file, byte);
                    if declared["status"] == "unknown" {
                        json!({"status":"unanalyzed","type":e.ty,"location":e.location})
                    } else {
                        declared
                    }
                }
            }
            QueryRequest::SymbolInfo { function, .. } => match self.functions.get(&function) {
                Some(&i) => {
                    let f = &self.snapshot.functions[i];
                    json!({"status":if f.synthetic {"unanalyzed"}else if f.locations.len()>1 {"ambiguous"}else{"ok"}, "symbol":f})
                }
                None => match self.symbols.get(&function) {
                    Some(&i) => {
                        json!({"status":"ok","symbol":self.names.symbol(&self.snapshot.semantic.symbols[i])})
                    }
                    None => json!({"status":"unknown"}),
                },
            },
            QueryRequest::References { function, .. } => {
                if let Some(indices) = self
                    .symbol_references
                    .get(&function)
                    .filter(|_| !self.functions.contains_key(&function))
                {
                    let mut references: Vec<_> = indices
                        .iter()
                        .map(|&i| &self.snapshot.semantic.references[i])
                        .collect();
                    references
                        .sort_by_key(|r| (&r.location.path, r.location.start, r.location.end));
                    json!({"status":"ok", "references":references, "coverage":"compiler-resolved"})
                } else if !self.functions.contains_key(&function) {
                    json!({"status":if self.symbols.contains_key(&function) {"ok"} else {"unknown"},"references":[]})
                } else {
                    let root = self.functions[&function];
                    let mut seen = std::collections::HashSet::from([root]);
                    let mut pending = vec![root];
                    let mut refs = Vec::new();
                    while let Some(i) = pending.pop() {
                        if let Some(indices) = self.references.get(&self.snapshot.functions[i].id) {
                            for &j in indices {
                                let mut reference =
                                    serde_json::to_value(&self.snapshot.semantic.expressions[j])
                                        .unwrap();
                                reference["identity"] = json!(self.snapshot.functions[i].identity);
                                reference["role"] = json!("call");
                                reference["certainty"] = json!(if i == root {
                                    "resolved"
                                } else {
                                    "possible-dispatch"
                                });
                                refs.push(reference);
                            }
                        }
                        for &caller in &self.dispatch_callers[i] {
                            if seen.insert(caller) {
                                pending.push(caller);
                            }
                        }
                    }
                    // Include every compiler-resolved token, including values,
                    // imports and constructor calls without expression records.
                    if let Some(indices) = self.symbol_references.get(&function) {
                        for &index in indices {
                            let reference = &self.snapshot.semantic.references[index];
                            let mut value = serde_json::to_value(reference).unwrap();
                            value["certainty"] = json!("resolved");
                            refs.push(value);
                        }
                    }
                    // One source occurrence is reported once: a resolved fact
                    // outranks possible dispatch, and records merge their fields.
                    let mut by_location = HashMap::new();
                    let mut unique: Vec<Value> = Vec::with_capacity(refs.len());
                    for reference in refs {
                        let key = reference["location"].to_string();
                        match by_location.get(&key) {
                            None => {
                                by_location.insert(key, unique.len());
                                unique.push(reference);
                            }
                            Some(&i) => {
                                let resolved = reference["certainty"] == "resolved"
                                    && unique[i]["certainty"] != "resolved";
                                let existing = unique[i].as_object_mut().unwrap();
                                for (field, value) in reference.as_object().unwrap() {
                                    if resolved && !value.is_null() {
                                        existing.insert(field.clone(), value.clone());
                                    } else {
                                        existing.entry(field.clone()).or_insert(value.clone());
                                    }
                                }
                            }
                        }
                    }
                    unique.sort_by(|a, b| {
                        let key = |v: &Value| {
                            (
                                v["location"]["start"].as_u64(),
                                v["location"]["end"].as_u64(),
                            )
                        };
                        a["location"]["path"]
                            .as_str()
                            .cmp(&b["location"]["path"].as_str())
                            .then_with(|| key(a).cmp(&key(b)))
                    });
                    let refs = unique;
                    let unresolved: Vec<_> = self
                        .symbols
                        .get(&function)
                        .and_then(|&i| self.snapshot.semantic.symbols[i].ty.as_ref())
                        .and_then(|ty| self.indirect_references.get(ty))
                        .into_iter()
                        .flatten()
                        .map(|&i| {
                            let mut candidate =
                                serde_json::to_value(&self.snapshot.semantic.references[i]).unwrap();
                            candidate["certainty"] = json!("signature-matched");
                            candidate["reason"] = json!("Indirect call through a binding or parameter with the same function type as the selected callable; the target is not resolved.");
                            candidate
                        })
                        .collect();
                    let mut coverage = String::from("compiler-resolved");
                    if refs.iter().any(|r| r["certainty"] == "possible-dispatch") {
                        coverage.push_str(" and possible virtual dispatch");
                    }
                    if !unresolved.is_empty() {
                        coverage
                            .push_str("; indirect candidates are signature-matched, not resolved");
                    }
                    json!({"status":if unresolved.is_empty(){"ok"}else{"incomplete"}, "references":refs,
                        "unresolved_candidates":unresolved,"coverage":coverage})
                }
            }
            QueryRequest::Effects { function, .. } => match self.functions.get(&function) {
                None => {
                    json!({"status":if self.symbols.contains_key(&function) {"unanalyzed"} else {"unknown"},
                        "reason":if self.symbols.get(&function).is_some_and(|&i| self.snapshot.semantic.symbols[i].kind == "method") {
                            "This interface contract has no executable body. Query an implementing method for its effects; dispatch effects depend on the implementation."
                        } else { "No executable body is available for this symbol in the selected source graph." }})
                }
                Some(&i) => {
                    let f = &self.snapshot.functions[i];
                    let mut seen = std::collections::HashSet::from([i]);
                    let mut queue = std::collections::VecDeque::from([(i, None)]);
                    // Function values from outside `f` are caller-supplied code:
                    // keep their effect bounds, but cite the indirect call site in
                    // the reaching function rather than the foreign body. They are
                    // visited only after every directly reachable function, so a
                    // body reachable both ways keeps its own evidence.
                    // Queue provenance with the edge; do not assign it to a body
                    // until that path wins against directly reachable paths.
                    let mut foreign =
                        std::collections::VecDeque::<(usize, usize, usize, Option<Value>)>::new();
                    let mut unlocated_foreign = std::collections::VecDeque::new();
                    let mut origins: HashMap<usize, usize> = HashMap::new();
                    let mut call_sites: HashMap<usize, Option<Value>> = HashMap::new();
                    let mut task_routes: HashMap<usize, Value> = HashMap::new();
                    // Shared dispatch nodes carry a call site, not a task
                    // boundary. Only an async executable candidate starts one.
                    let mut dispatch_sites: HashMap<usize, Value> = HashMap::new();
                    // Prefer a synchronous path when a body is reachable both
                    // in this task and in a spawned task. Each body still runs
                    // through the evidence walk only once.
                    let mut tasks = std::collections::VecDeque::<(usize, usize, Value)>::new();
                    // Some compiler graph edges bypass a shared dispatch node.
                    // Defer these until its source-bearing path has been visited.
                    let mut unlocated_tasks = std::collections::VecDeque::<(usize, usize)>::new();
                    let nested = |k: usize| {
                        self.snapshot.functions[k].locations.iter().any(|inner| {
                            f.locations.iter().any(|outer| {
                                inner.path == outer.path
                                    && outer.start <= inner.start
                                    && inner.end <= outer.end
                            })
                        })
                    };
                    let mut witness = vec![
                        json!({"function":f.id,"via":null,"reason":f.unknown_reason(),"unresolved":f.unresolved}),
                    ];
                    let mut evidence = BTreeMap::new();
                    let mut unknown_causes = Vec::new();
                    let mut suspension_sites = Vec::new();
                    loop {
                        let (j, via) = if let Some(next) = queue.pop_front() {
                            next
                        } else if let Some((k, j, route)) = tasks.pop_front() {
                            if !seen.insert(k) {
                                continue;
                            }
                            task_routes.insert(k, route);
                            witness.push(json!({"function":self.snapshot.functions[k].id,"via":self.snapshot.functions[j].id,"runtime_effects":self.snapshot.functions[k].runtime_effects,"reason":self.snapshot.functions[k].unknown_reason(),"unresolved":self.snapshot.functions[k].unresolved}));
                            (k, Some(j))
                        } else if let Some((k, j, origin, call)) = foreign
                            .pop_front()
                            .or_else(|| unlocated_foreign.pop_front())
                        {
                            if !seen.insert(k) {
                                continue;
                            }
                            origins.insert(k, origin);
                            if let Some(route) = task_routes.get(&j) {
                                task_routes.insert(k, route.clone());
                            } else if self.async_functions.contains(&k) {
                                let mut route = call.unwrap_or_else(|| json!({
                                    "kind":"spawn", "function":self.snapshot.functions[j].id,
                                    "location":null,
                                    "reason":"No source call site is available for this async graph edge."
                                }));
                                route["target"] = json!(self.snapshot.functions[k].id);
                                task_routes.insert(k, route);
                            } else if self.snapshot.functions[k].synthetic
                                && let Some(call) = call
                            {
                                dispatch_sites.insert(k, call);
                            }
                            witness.push(json!({"function":self.snapshot.functions[k].id,"via":self.snapshot.functions[j].id,"runtime_effects":self.snapshot.functions[k].runtime_effects,"reason":self.snapshot.functions[k].unknown_reason(),"unresolved":self.snapshot.functions[k].unresolved}));
                            (k, Some(j))
                        } else if let Some((k, j)) = unlocated_tasks.pop_front() {
                            if !seen.insert(k) {
                                continue;
                            }
                            task_routes.insert(k, json!({"kind":"spawn", "function":self.snapshot.functions[j].id,
                                "target":self.snapshot.functions[k].id, "location":null,
                                "reason":"No source call site is available for this async graph edge."}));
                            witness.push(json!({"function":self.snapshot.functions[k].id,"via":self.snapshot.functions[j].id,"runtime_effects":self.snapshot.functions[k].runtime_effects,"reason":self.snapshot.functions[k].unknown_reason(),"unresolved":self.snapshot.functions[k].unresolved}));
                            (k, Some(j))
                        } else {
                            break;
                        };
                        let origin = origins.get(&j).copied();
                        let cause = &self.snapshot.functions[j];
                        if cause.unknown
                            && (!cause.unresolved.is_empty()
                                || (cause.synthetic && self.callees[j].is_empty()))
                        {
                            unknown_causes.push(json!({"function":cause.id,
                                "name":cause.name, "module":cause.module,
                                "locations":cause.locations, "unresolved":cause.unresolved,
                                "call_sites":self.snapshot.semantic.unresolved_calls.get(&cause.id),
                                "caller_location":if cause.locations.is_empty() { via.and_then(|v: usize| self.snapshot.functions[v].locations.first()) } else { None },
                                "reason":cause.unknown_reason()}));
                        }
                        if let Some(facts) = self
                            .snapshot
                            .semantic
                            .witnesses
                            .get(&self.snapshot.functions[j].id)
                        {
                            for fact in facts {
                                let Some(effect) = fact["effect"].as_u64() else {
                                    continue;
                                };
                                let (fact, via_function) = match origin {
                                    None => (fact, j),
                                    Some(origin) => {
                                        let site = call_sites.entry(origin).or_insert_with(|| {
                                            self.snapshot
                                                .semantic
                                                .witnesses
                                                .get(&self.snapshot.functions[origin].id)?
                                                .iter()
                                                .find(|fact| {
                                                    fact["witness"]["cause"]["operation"]
                                                        == "indirect-call"
                                                })
                                                .map(|fact| {
                                                    let mut witness = fact["witness"].clone();
                                                    witness["reason"] = json!("Bound from function values reaching this indirect call; their bodies belong to the code that supplies them.");
                                                    json!({"witness": witness})
                                                })
                                        });
                                        // Without a recorded call site, the foreign
                                        // body remains the only available citation.
                                        match site {
                                            Some(site) => (&*site, origin),
                                            None => (fact, j),
                                        }
                                    }
                                };
                                if f.runtime_effects & effect as u8 == 0
                                    || fact["witness"]["kind"] == "unavailable"
                                {
                                    continue;
                                }
                                let channel = effect
                                    == u64::from(RuntimeEffects::MAY_SUSPEND.bits())
                                    && fact["witness"]["cause"]["operation"]
                                        .as_str()
                                        .is_some_and(|op| op.starts_with("channel-"));
                                if !channel
                                    && evidence.get(&effect).is_some_and(|entry: &Value| {
                                        !prefer_allocation(
                                            effect as u8,
                                            &fact["witness"],
                                            &entry["witness"],
                                        )
                                    })
                                {
                                    continue;
                                }
                                let mut explained = json!({"effect":effect,
                                    "status":evidence_status(&fact["witness"]),
                                    "via_function":self.snapshot.functions[via_function].id,"witness":fact["witness"]});
                                if let Some(route) = task_routes.get(&j) {
                                    explained["task_relationship"] = route.clone();
                                } else if fact["witness"]["cause"]["operation"] == "await"
                                    || fact["witness"].get("awaited_type").is_some()
                                {
                                    explained["task_relationship"] = json!({"kind":"await", "function":self.snapshot.functions[via_function].id,
                                        "location":fact["witness"]["cause"]["location"]});
                                }
                                if channel {
                                    suspension_sites.push(explained);
                                } else {
                                    evidence.insert(effect, explained);
                                }
                            }
                        }
                        let sites: HashMap<_, _> = self
                            .snapshot
                            .semantic
                            .effect_calls
                            .get(&cause.id)
                            .into_iter()
                            .flatten()
                            .map(|site| (site.target.as_str(), site))
                            .collect();
                        let bucket = cause.id.starts_with("$indirect-arity:");
                        for &k in &self.callees[j] {
                            self.effect_edge_visits += 1;
                            let origin = origin.or(via.filter(|_| bucket && !nested(k)));
                            if seen.contains(&k) {
                                continue;
                            }
                            let target = &self.snapshot.functions[k];
                            let asynchronous = self.async_functions.contains(&k);
                            let call = if asynchronous || target.synthetic {
                                sites
                                    .get(target.id.as_str())
                                    .map(|site| {
                                        json!({"kind":"spawn", "function":cause.id,
                                            "target":target.id, "location":site.location})
                                    })
                                    .or_else(|| dispatch_sites.get(&j).cloned())
                            } else {
                                None
                            };
                            if let Some(origin) = origin {
                                let pending = (k, j, origin, call);
                                if asynchronous
                                    && pending.3.is_none()
                                    && !task_routes.contains_key(&j)
                                {
                                    unlocated_foreign.push_back(pending);
                                } else {
                                    foreign.push_back(pending);
                                }
                            } else {
                                if asynchronous {
                                    if let Some(mut route) = task_routes.get(&j).cloned().or(call) {
                                        if !task_routes.contains_key(&j) {
                                            route["target"] = json!(target.id);
                                        }
                                        tasks.push_back((k, j, route));
                                    } else {
                                        unlocated_tasks.push_back((k, j));
                                    }
                                    continue;
                                } else if target.synthetic
                                    && let Some(call) = call
                                {
                                    dispatch_sites.insert(k, call);
                                }
                                seen.insert(k);
                                if let Some(route) = task_routes.get(&j) {
                                    task_routes.insert(k, route.clone());
                                }
                                queue.push_back((k, Some(j)));
                                witness.push(json!({"function":self.snapshot.functions[k].id,"via":self.snapshot.functions[j].id,"runtime_effects":self.snapshot.functions[k].runtime_effects,"reason":self.snapshot.functions[k].unknown_reason(),"unresolved":self.snapshot.functions[k].unresolved}));
                            }
                        }
                    }
                    let mut missing = evidence
                        .values()
                        .chain(&suspension_sites)
                        .any(|e| e["status"] == "missing-source");
                    for bit in 0..RuntimeEffects::BIT_COUNT {
                        let effect = RuntimeEffects::from_bit(bit).bits();
                        if f.runtime_effects & effect == 0
                            || (effect == RuntimeEffects::MAY_SUSPEND.bits()
                                && !suspension_sites.is_empty())
                        {
                            continue;
                        }
                        evidence.entry(effect as u64).or_insert_with(|| {
                            missing = true;
                            json!({"effect":effect,"status":"missing-witness", "reason":
                                if f.unknown {
                                    "Retained as a conservative capability of an unresolved call or dispatch candidate; no operation witness is available. This does not prove that this function suspends or blocks, including when its declaration is synchronous."
                                } else {
                                    "Retained from the compiler/runtime capability summary, but no operation witness is available. This is an upper bound, not proof that this function performs the effect."
                                }})
                        });
                    }
                    let mut evidence: Vec<_> = evidence.into_values().collect();
                    evidence.extend(suspension_sites);
                    deduplicate_evidence(&mut evidence);
                    self.present_lock_types(&mut evidence);
                    let mut compiler_witnesses =
                        self.snapshot.semantic.witnesses.get(&f.id).cloned();
                    if let Some(facts) = &mut compiler_witnesses {
                        let ranges = local_ranges(&f.locations);
                        facts.retain(|fact| {
                            let w = &fact["witness"];
                            let loc = &w["cause"]["location"];
                            let (Some(path), Some(start), Some(end)) = (
                                loc["path"].as_str(),
                                loc["start"].as_u64(),
                                loc["end"].as_u64(),
                            ) else {
                                return false;
                            };
                            let index =
                                ranges.partition_point(|&(p, s, _)| (p, s as u64) <= (path, start));
                            w["owner"].as_str().is_none_or(|owner| owner == f.id)
                                && index.checked_sub(1).is_some_and(|index| {
                                    let (p, _, e) = ranges[index];
                                    p == path && start <= end && end <= e as u64
                                })
                        });
                        deduplicate_evidence(facts);
                        self.present_lock_types(facts);
                        // Every evidence line carries a status, local ones too.
                        for fact in facts.iter_mut() {
                            fact["status"] = json!(evidence_status(&fact["witness"]));
                            if fact["witness"]["cause"]["operation"] == "await"
                                || fact["witness"].get("awaited_type").is_some()
                            {
                                fact["task_relationship"] = json!({"kind":"await", "function":f.id,
                                    "location":fact["witness"]["cause"]["location"]});
                            }
                        }
                    }
                    json!({"status":if f.unknown{"unknown"}else if missing{"incomplete"}else{"ok"},
                        "reason":f.unknown_reason().or(if missing { Some("Effect evidence is incomplete: see the named capability and missing-witness or missing-source reason below.") } else { None }), "unknown_causes":unknown_causes, "runtime_effects":f.runtime_effects,"compiler_effects":self.snapshot.semantic.compiler_effects.get(&f.id),"effect_evidence":evidence,
                        "witness":witness, "compiler_witnesses":compiler_witnesses,
                        "meaning":"compiler facts and conservative runtime capability bounds; indirect-call bounds cover checked function values in this workspace, not arbitrary values supplied by future callers; external business effects and predicate feasibility are not implied"})
                }
            },
        };
        json!({"revision":self.revision(),"result":data})
    }
}

// Iterative Kosaraju. One traversal per graph direction; no per-loop scans.
fn components(flow: &Flow) -> (Vec<usize>, Vec<usize>, usize) {
    let n = flow.nodes.len();
    let mut reverse = vec![vec![]; n];
    let mut visits = 0;
    for (i, node) in flow.nodes.iter().enumerate() {
        for &j in &node.successors {
            reverse[j].push(i);
            visits += 1;
        }
    }
    let mut seen = vec![false; n];
    let mut order = Vec::new();
    let mut stack = vec![(flow.entry, false)];
    while let Some((i, exit)) = stack.pop() {
        if exit {
            order.push(i);
            continue;
        }
        if seen[i] {
            continue;
        }
        seen[i] = true;
        stack.push((i, true));
        for &j in &flow.nodes[i].successors {
            stack.push((j, false));
            visits += 1;
        }
    }
    let mut component = vec![usize::MAX; n];
    let mut sizes = Vec::new();
    for &i in order.iter().rev() {
        if component[i] != usize::MAX {
            continue;
        }
        let id = sizes.len();
        let mut size = 0;
        let mut stack = vec![i];
        component[i] = id;
        while let Some(j) = stack.pop() {
            size += 1;
            for &k in &reverse[j] {
                visits += 1;
                if seen[k] && component[k] == usize::MAX {
                    component[k] = id;
                    stack.push(k);
                }
            }
        }
        sizes.push(size);
    }
    (component, sizes, visits)
}
fn cycle_paths(flow: &Flow, component: &[usize], repeating: &[bool]) -> (Value, usize) {
    let n = flow.nodes.len();
    let mut reverse = vec![vec![]; n];
    let mut roots = vec![None; n];
    let mut visits = 0;
    for (i, node) in flow.nodes.iter().enumerate() {
        for &j in &node.successors {
            reverse[j].push(i);
            visits += 1;
        }
        if repeating[i] && node.loop_header {
            roots[component[i]] = Some(i);
        }
    }
    let mut to_root = vec![None; n];
    let mut from_root = vec![None; n];
    let mut cycle_root = vec![None; n];
    for root in roots.into_iter().flatten() {
        cycle_root[root] = Some(root);
        let mut queue = std::collections::VecDeque::from([root]);
        while let Some(i) = queue.pop_front() {
            for &j in &flow.nodes[i].successors {
                visits += 1;
                if component[j] == component[root] && cycle_root[j].is_none() {
                    cycle_root[j] = Some(root);
                    from_root[j] = Some(i);
                    queue.push_back(j);
                }
            }
        }
        let mut queue = std::collections::VecDeque::from([root]);
        to_root[root] = Some(root);
        while let Some(i) = queue.pop_front() {
            for &j in &reverse[i] {
                visits += 1;
                if component[j] == component[root] && to_root[j].is_none() {
                    to_root[j] = Some(i);
                    queue.push_back(j);
                }
            }
        }
    }
    (
        json!({"root":cycle_root,"to_root":to_root,"from_root":from_root}),
        visits,
    )
}

struct RiskPaths {
    flows: Vec<Value>,
    memberships: Vec<Vec<bool>>,
    components_by_flow: Vec<Vec<usize>>,
    retry_kinds: Vec<Vec<u8>>,
    reachable: Vec<Vec<bool>>,
    call_paths: HashMap<String, Value>,
    edge_visits: usize,
    call_edge_visits: usize,
    unresolved_retry_calls: bool,
}
impl RiskPaths {
    fn new(snapshot: &Snapshot, evidence: bool) -> Self {
        let mut flows = Vec::new();
        let mut memberships = Vec::new();
        let mut components_by_flow = Vec::new();
        let mut retry_kinds = Vec::new();
        let mut reachable = Vec::new();
        let mut edge_visits = 0;
        for flow in &snapshot.semantic.flows {
            let (component, sizes, visits) = components(flow);
            edge_visits += visits;
            let repeating: Vec<_> = component
                .iter()
                .enumerate()
                .map(|(i, &c)| {
                    c != usize::MAX && (sizes[c] > 1 || flow.nodes[i].successors.contains(&i))
                })
                .collect();
            if evidence {
                let (paths, path_visits) = cycle_paths(flow, &component, &repeating);
                edge_visits += path_visits;
                // Shared graph plus SCC membership is the compact path evidence:
                // same SCC proves structural paths in both directions. Clients can
                // expand paths without quadratic copies in compiler output.
                flows.push(json!({"flow":flow,"paths":paths,"components":component.iter().map(|&c|if c==usize::MAX{None}else{Some(c)}).collect::<Vec<_>>(),"repeating":repeating}));
            }
            components_by_flow.push(component.clone());
            retry_kinds.push(vec![0u8; sizes.len()]);
            memberships.push(repeating);
            reachable.push(
                component
                    .iter()
                    .map(|&c| c != usize::MAX)
                    .collect::<Vec<_>>(),
            );
        }
        for e in &snapshot.semantic.expressions {
            let Some(operation) = &e.operation else {
                continue;
            };
            let kind = if operation == "wait:timeout" { 1 }
                else if (operation.starts_with("call:") || operation.starts_with("method:") || operation.starts_with("static:"))
                    && e.ty.as_ref().is_some_and(|ty| matches!(ty, Type::Bool) || matches!(ty,Type::Generic(name,_) if name.rsplit("::").next()==Some("Result"))) { 2 }
                else { 0 };
            for &node in &e.occurrences {
                if memberships[e.flow][node] {
                    retry_kinds[e.flow][components_by_flow[e.flow][node]] |= kind;
                }
            }
        }
        for (i, kinds) in retry_kinds.iter().enumerate().filter(|_| evidence) {
            flows[i]["retry_candidates"] = json!(kinds.iter().enumerate().filter(|(_,k)|**k!=0)
                .map(|(component,&kind)|json!({"component":component,"kind":if kind&1!=0 {"timeout-repeat"}else{"fallible-or-boolean-operation-repeat"},"certainty":"conservative-candidate"})).collect::<Vec<_>>());
        }
        let functions: HashMap<_, _> = snapshot
            .functions
            .iter()
            .map(|f| (f.id.as_str(), f))
            .collect();
        let mut call_paths = HashMap::<String, Value>::new();
        let mut queue = std::collections::VecDeque::new();
        for e in &snapshot.semantic.expressions {
            if e.operation.is_some()
                && e.occurrences.iter().any(|&node| memberships[e.flow][node])
                && snapshot.semantic.flows[e.flow].complete
                && let Some(target) = &e.target
                && !call_paths.contains_key(target)
            {
                call_paths.insert(
                    target.clone(),
                    json!({"flow":e.flow,"node":e.node,"via":null}),
                );
                queue.push_back(target.clone());
            }
        }
        let mut call_edge_visits = 0;
        let mut dispatch_owners = std::collections::HashSet::new();
        let mut unresolved_retry_calls = false;
        for e in &snapshot.semantic.expressions {
            if !e.occurrences.iter().any(|&node| memberships[e.flow][node])
                || !snapshot.semantic.flows[e.flow].complete
            {
                continue;
            }
            let Some(operation) = &e.operation else {
                continue;
            };
            if e.target.is_none()
                && functions[e.function.as_str()].unknown
                && (operation.starts_with("call:")
                    || operation.starts_with("method:")
                    || operation.starts_with("static:"))
            {
                unresolved_retry_calls = true;
            }
            // One union per caller, not per repeated virtual call site. The
            // caller graph may include other calls, so this is only a candidate.
            if operation.starts_with("method:") && dispatch_owners.insert(&e.function) {
                for target in &functions[e.function.as_str()].callees {
                    call_edge_visits += 1;
                    if !call_paths.contains_key(target) {
                        call_paths.insert(target.clone(),json!({"flow":e.flow,"node":e.node,"via":null,"certainty":"conservative-caller-dispatch"}));
                        queue.push_back(target.clone());
                    }
                }
            }
        }
        while let Some(id) = queue.pop_front() {
            if let Some(f) = functions.get(id.as_str()) {
                for target in &f.callees {
                    call_edge_visits += 1;
                    if !call_paths.contains_key(target) {
                        call_paths.insert(target.clone(), json!({"via":id}));
                        queue.push_back(target.clone());
                    }
                }
            }
        }
        Self {
            flows,
            memberships,
            components_by_flow,
            retry_kinds,
            reachable,
            call_paths,
            edge_visits,
            call_edge_visits,
            unresolved_retry_calls,
        }
    }
    fn state(&self, e: &Expression) -> (bool, bool) {
        let reachable = e
            .occurrences
            .iter()
            .any(|&node| self.reachable[e.flow][node]);
        let repeating = e
            .occurrences
            .iter()
            .any(|&node| self.memberships[e.flow][node])
            || (reachable && self.call_paths.contains_key(&e.function));
        (reachable, repeating)
    }
}
impl Snapshot {
    /// V1.2 requires no public query session. All evidence is snapshot-owned.
    pub fn risk(&self, before: &Snapshot) -> Result<Value> {
        self.validate()?;
        before.validate()?;
        ensure!(
            self.compatibility == before.compatibility && self.workspace == before.workspace,
            "incompatible risk baseline"
        );
        let baseline = RiskPaths::new(before, false);
        // Independent capacities preserve multiset matching without pairing an
        // unreachable duplicate with a reachable one based on source order.
        let mut old = HashMap::<(&str, &str, &str), [usize; 3]>::new();
        for e in &before.semantic.expressions {
            if let Some(operation) = &e.operation {
                let (reachable, repeating) = baseline.state(e);
                let counts = old
                    .entry((&e.function, operation, &e.fingerprint))
                    .or_default();
                counts[0] += 1;
                counts[1] += usize::from(reachable);
                counts[2] += usize::from(repeating);
            }
        }
        let baseline_edge_visits = baseline.edge_visits;
        let baseline_call_edge_visits = baseline.call_edge_visits;
        drop(baseline);
        let RiskPaths {
            flows,
            memberships,
            components_by_flow,
            retry_kinds,
            reachable,
            call_paths,
            edge_visits,
            call_edge_visits,
            unresolved_retry_calls,
        } = RiskPaths::new(self, true);
        let functions: HashMap<_, _> = self.functions.iter().map(|f| (f.id.as_str(), f)).collect();
        // A single backwards propagation computes absence of observable operations.
        // Do not turn a new call to an effect-free body into a new side effect.
        // Missing bodies, incomplete flow and runtime capabilities stay conservative.
        let index = graph::Index::new(&self.functions);
        let mut may_effect: Vec<_> = self
            .functions
            .iter()
            .map(|f| f.synthetic || f.unknown || f.runtime_effects != 0)
            .collect();
        let mut has_body = vec![false; self.functions.len()];
        for flow in &self.semantic.flows {
            let i = index.by_id[flow.function.as_str()];
            has_body[i] = true;
            may_effect[i] |= !flow.complete;
        }
        for (i, present) in has_body.into_iter().enumerate() {
            may_effect[i] |= !present;
        }
        for e in &self.semantic.expressions {
            if let Some(op) = &e.operation {
                let direct_call =
                    (op.starts_with("call:") || op.starts_with("static:")) && e.target.is_some();
                if !direct_call {
                    may_effect[index.by_id[e.function.as_str()]] = true;
                }
            }
        }
        let mut pending: std::collections::VecDeque<_> = may_effect
            .iter()
            .enumerate()
            .filter_map(|(i, &effect)| effect.then_some(i))
            .collect();
        let mut effect_edge_visits = 0;
        while let Some(i) = pending.pop_front() {
            for &caller in &index.callers[i] {
                effect_edge_visits += 1;
                if !may_effect[caller] {
                    may_effect[caller] = true;
                    pending.push_back(caller);
                }
            }
        }
        let mut operations = Vec::new();
        for e in &self.semantic.expressions {
            let Some(operation) = &e.operation else {
                continue;
            };
            let count = old
                .entry((&e.function, e.operation.as_deref().unwrap(), &e.fingerprint))
                .or_default();
            let new = count[0] == 0;
            count[0] = count[0].saturating_sub(1);
            let flow = &self.semantic.flows[e.flow];
            let local_repeat = e.occurrences.iter().any(|&node| memberships[e.flow][node]);
            let inherited = call_paths.contains_key(&e.function)
                && e.occurrences.iter().any(|&node| reachable[e.flow][node]);
            let repeating = local_repeat || inherited;
            let unknown = e
                .target
                .as_ref()
                .is_none_or(|t| functions.get(t.as_str()).is_none_or(|f| f.unknown));
            let reachable_here = e.occurrences.iter().any(|&node| reachable[e.flow][node]);
            let new_execution = reachable_here && count[1] == 0;
            let new_repetition = repeating && count[2] == 0;
            count[1] = count[1].saturating_sub(usize::from(reachable_here));
            count[2] = count[2].saturating_sub(usize::from(repeating));
            let new_effect = new || new_execution || new_repetition;
            let retry_kind = e
                .occurrences
                .iter()
                .filter(|&&node| memberships[e.flow][node])
                .fold(0u8, |kind, &node| {
                    kind | retry_kinds[e.flow][components_by_flow[e.flow][node]]
                });
            let pure_call = e
                .target
                .as_ref()
                .and_then(|t| index.by_id.get(t.as_str()))
                .is_some_and(|&i| !may_effect[i])
                && (operation.starts_with("call:") || operation.starts_with("static:"));
            let direct_effect = operation.starts_with("io:") || operation.starts_with("write:");
            operations.push(json!({"function":e.function,"location":e.location,"operation":operation,"new_operation":new,"new_execution":new_execution,"new_repetition":new_repetition,
                "new_side_effect":if !new_effect {"not-new"}else if pure_call || !reachable_here {"proven-absent"}else if direct_effect {"proven-operation"}else{"conservative-candidate"},
                "retry_reachability":if !flow.complete {"unanalyzed"}else if local_repeat {"structural-cycle"}else if inherited {"conservative-call-path"}else if unresolved_retry_calls {"unanalyzed"}else{"no-structural-cycle"},
                "execution_reachability":if reachable_here {"structural-path; predicate feasibility not proven"}else{"proven-unreachable"},
                "retry_detection":if retry_kind&1!=0 {"timeout-retry-candidate"}else if retry_kind&2!=0 {"fallible-or-boolean-retry-candidate"}else if inherited {"inherited-repetition-candidate"}else if local_repeat {"repetition-only"}else{"no-repetition"},
                "retry_certainty":if retry_kind!=0 || inherited {"conservative-candidate"}else{"structural-fact"},
                "retry_intent":"not established; a loop or a boolean result alone does not prove business retry intent",
                "effect_certainty":if pure_call {"proven-absent"}else if direct_effect {"proven-operation"}else if unknown{"unknown"}else{"conservative-candidate"},
                "evidence":{"flow":e.flow,"node":e.node,"nodes":e.occurrences,"target":e.target},
                "review_question":if new_effect && !pure_call && reachable_here && (repeating || !flow.complete || unresolved_retry_calls) {Some(if retry_kind != 0 || inherited {"Can this newly enabled or repeated side effect recur after failure or timeout, and is repetition safe or deduplicated?"} else {"This operation is newly enabled or can now repeat. Is repeated execution intended and safe?"})}else{None}
            }));
        }
        Ok(
            json!({"revision":self.revision,"baseline_revision":before.revision,"operations":operations,"evidence":flows,"call_paths":call_paths,"edge_visits":edge_visits,"call_edge_visits":call_edge_visits,"effect_edge_visits":effect_edge_visits,"baseline_edge_visits":baseline_edge_visits,"baseline_call_edge_visits":baseline_call_edge_visits,"unresolved_retry_calls":unresolved_retry_calls,
            "limitations":["CFG paths are structural; predicate feasibility and business retry intent are not proven","Nonconstant predicates remain structural alternatives; retry candidates do not establish business intent","Calls are conservative side-effect candidates; runtime capability bits do not prove external writes"]}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn task_effect_local_ranges_merge_overlap_and_keep_source_boundaries() {
        let locations: Vec<_> = [
            ("b", 1, 4),
            ("a", 10, 20),
            ("a", 2, 8),
            ("a", 4, 6),
            ("a", 8, 12),
            ("a", 30, 40),
        ]
        .into_iter()
        .map(|(path, start, end)| Location {
            path: path.into(),
            start,
            end,
        })
        .collect();
        assert_eq!(
            local_ranges(&locations),
            vec![("a", 2, 20), ("a", 30, 40), ("b", 1, 4)]
        );
        assert!(local_ranges(&[]).is_empty());
    }

    #[test]
    fn task_effect_dispatch_mixed_candidates_scale_without_cross_products() {
        let directory =
            std::env::temp_dir().join(format!("willow-dispatch-evidence-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("main.wi");
        for n in [8, 32, 128] {
            let mut source = String::from(
                "interface Reading<T> { fn read(self, ch: Channel<i64>) -> Task<()>; }\nasync fn done() {}\n",
            );
            for i in 0..n {
                source.push_str(&format!("class Sync{i} implements Reading<i64> {{ pub fn read(self, ch: Channel<i64>) -> Task<()> {{ ch.send(10); return done(); }} }}\nclass Async{i} implements Reading<i64> {{ pub async fn read(self, ch: Channel<i64>) {{ ch.send(20); }} }}\n"));
            }
            source.push_str("fn invoke(r: Reading<i64>, ch: Channel<i64>) -> Task<()> { return r.read(ch); }\nfn main() {}\n");
            std::fs::write(&path, &source).unwrap();
            let snapshot = crate::CompilerSession::new(
                path.to_str().unwrap(),
                "",
                &crate::CompilerOptions::debug(),
                None,
            )
            .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
            .unwrap();
            let function = snapshot
                .functions
                .iter()
                .find(|f| f.name == "invoke")
                .unwrap()
                .id
                .clone();
            let stored = snapshot
                .semantic
                .effect_calls
                .values()
                .map(Vec::len)
                .sum::<usize>();
            assert!(stored <= n + 3, "n={n} stored={stored}");
            let snapshot: Snapshot =
                serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
            let revision = snapshot.revision.clone();
            let mut session = QuerySession::new(snapshot).unwrap();
            let value = session.query(QueryRequest::Effects { revision, function });
            let sites: Vec<_> = value["result"]["effect_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|fact| fact["witness"]["cause"]["operation"] == "channel-send")
                .collect();
            assert_eq!(sites.len(), 2 * n, "{value}");
            let spawned: Vec<_> = sites
                .iter()
                .filter(|fact| fact["task_relationship"]["kind"] == "spawn")
                .collect();
            assert_eq!(spawned.len(), n, "{value}");
            let start = source.find("r.read(ch)").unwrap() + 2;
            assert!(
                spawned
                    .iter()
                    .all(|fact| fact["task_relationship"]["location"]["start"] == start),
                "{value}"
            );
            assert!(session.effect_edge_visits <= 4 * n + 4);
            println!(
                "mixed candidates={}: stored calls={stored}, emitted sends={}, spawned={}, visited edges={}",
                2 * n,
                sites.len(),
                spawned.len(),
                session.effect_edge_visits
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn task_effect_evidence_scales_with_sites_and_reachable_edges() {
        let directory =
            std::env::temp_dir().join(format!("willow-task-evidence-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("main.wi");
        for n in [8, 32, 128] {
            let mut source = format!(
                "async fn leaf(ch: Channel<i64>) {{ {} }}\n",
                "ch.send(1);".repeat(n)
            );
            for i in 0..n {
                let callee = if i == 0 {
                    "leaf".to_owned()
                } else {
                    format!("relay{}", i - 1)
                };
                source.push_str(&format!(
                    "async fn relay{i}(ch: Channel<i64>) {{ {callee}(ch); {callee}(ch); }}\n"
                ));
            }
            source.push_str("fn main() { let ch = Channel<i64>::new();");
            for i in 0..n {
                source.push_str(&format!("relay{i}(ch);"));
            }
            source.push('}');
            std::fs::write(&path, source).unwrap();
            let snapshot = crate::CompilerSession::new(
                path.to_str().unwrap(),
                "",
                &crate::CompilerOptions::debug(),
                None,
            )
            .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
            .unwrap();
            assert_eq!(
                snapshot
                    .semantic
                    .effect_calls
                    .values()
                    .map(Vec::len)
                    .sum::<usize>(),
                3 * n
            );
            // Task edges survive persisted snapshots, including generic-free IDs.
            let snapshot: Snapshot =
                serde_json::from_value(serde_json::to_value(snapshot).unwrap()).unwrap();
            let function = snapshot
                .functions
                .iter()
                .find(|f| f.name == "main")
                .unwrap()
                .id
                .clone();
            let revision = snapshot.revision.clone();
            let mut session = QuerySession::new(snapshot).unwrap();
            let result = session.query(QueryRequest::Effects { revision, function });
            let sends: Vec<_> = result["result"]["effect_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|fact| fact["witness"]["cause"]["operation"] == "channel-send")
                .collect();
            assert_eq!(sends.len(), n);
            assert!(
                sends
                    .iter()
                    .all(|fact| fact["task_relationship"]["kind"] == "spawn")
            );
            assert_eq!(session.effect_edge_visits, 2 * n);
            println!(
                "task sites={} send sites={n} emitted sends={} edge visits={}",
                3 * n,
                sends.len(),
                session.effect_edge_visits
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn channel_capture_keeps_summary_scans_bounded() {
        for n in [1, 8, 64, 512] {
            let mut captured = Captured::default();
            let function = FunctionId::free("worker");
            for _ in 0..n {
                for operation in ["channel-send", "channel-recv"] {
                    captured.channel_site(function, Span::dummy(), operation);
                    captured.capability(
                        function,
                        RuntimeEffects::ALL.bits(),
                        Span::dummy(),
                        "builtin-method-call",
                    );
                }
            }
            assert_eq!(captured.suspension_sites[&function].len(), 2 * n);
            assert_eq!(
                captured.witnesses[&function].len(),
                RuntimeEffects::BIT_COUNT as usize
            );
            println!(
                "pairs={n}, stored sites={}, summary scan bound={}",
                2 * n,
                RuntimeEffects::BIT_COUNT
            );
        }
    }

    #[test]
    fn unresolved_call_evidence_scales_with_sites_and_reachable_edges() {
        let directory =
            std::env::temp_dir().join(format!("willow-unresolved-scaling-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("main.wi");
        for n in [16, 64, 256] {
            let mut source = format!(
                "fn apply(f: fn(i64) -> i64) -> i64 {{ {} return 0; }}\n",
                "f(1);".repeat(n)
            );
            for i in 0..n {
                let callee = if i == 0 {
                    "apply".to_owned()
                } else {
                    format!("relay{}", i - 1)
                };
                source.push_str(&format!(
                    "fn relay{i}(f: fn(i64) -> i64) -> i64 {{ return {callee}(f); }}\n"
                ));
            }
            source.push_str("fn main() {}\n");
            std::fs::write(&path, source).unwrap();
            let snapshot = crate::CompilerSession::new(
                path.to_str().unwrap(),
                "",
                &crate::CompilerOptions::debug(),
                None,
            )
            .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
            .unwrap();
            assert_eq!(
                snapshot
                    .semantic
                    .unresolved_calls
                    .values()
                    .map(Vec::len)
                    .sum::<usize>(),
                n
            );
            let function = snapshot
                .functions
                .iter()
                .find(|f| f.name == format!("relay{}", n - 1))
                .unwrap()
                .id
                .clone();
            let revision = snapshot.revision.clone();
            let mut session = QuerySession::new(snapshot).unwrap();
            let value = session.query(QueryRequest::Effects { revision, function });
            // `apply` owns the unresolved sites; the arity bucket reports that no
            // function value of that arity is known.
            let causes = value["result"]["unknown_causes"].as_array().unwrap();
            assert_eq!(causes.len(), 2, "{causes:#?}");
            assert!(causes.iter().any(|cause| cause["unresolved"][0]
                == "incomplete-function-value-candidates"
                && cause["call_sites"].is_null()));
            let apply = causes
                .iter()
                .find(|cause| cause["name"] == "apply")
                .unwrap();
            assert_eq!(apply["call_sites"].as_array().unwrap().len(), n);
            // n relay/apply edges plus apply's edge into the arity bucket.
            assert_eq!(session.effect_edge_visits, n + 1);
            println!(
                "sites={n}, chain_edges={n}, stored_sites={n}, reported_sites={n}, visited_edges={}",
                session.effect_edge_visits
            );
        }
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn missing_effect_witness_is_incomplete_and_missing_identity_is_unknown() {
        let directory =
            std::env::temp_dir().join(format!("willow-effect-witness-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("main.wi");
        std::fs::write(&path, "fn main() { println(1); }").unwrap();
        let mut snapshot = crate::CompilerSession::new(
            path.to_str().unwrap(),
            "",
            &crate::CompilerOptions::debug(),
            None,
        )
        .analysis_with_emitter(&mut crate::diagnostics::HumanEmitter)
        .unwrap();
        let id = snapshot
            .functions
            .iter()
            .find(|f| f.name == "main")
            .unwrap()
            .id
            .clone();
        snapshot.semantic.witnesses.clear();
        snapshot.revision = snapshot.digest().unwrap();
        let revision = snapshot.revision.clone();
        let mut session = QuerySession::new(snapshot).unwrap();
        let result = session.query(QueryRequest::Effects {
            revision: revision.clone(),
            function: id,
        });
        assert_eq!(result["result"]["status"], "incomplete");
        assert!(
            result["result"]["effect_evidence"]
                .as_array()
                .unwrap()
                .iter()
                .all(|e| e["status"] == "missing-witness"
                    && e["reason"]
                        .as_str()
                        .is_some_and(|r| r.contains("upper bound")))
        );
        let result = session.query(QueryRequest::Effects {
            revision,
            function: "absent".into(),
        });
        assert_eq!(result["result"]["status"], "unknown");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
