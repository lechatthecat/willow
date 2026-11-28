//! Compact compiler facts, structured control flow, and snapshot query adapter.
use super::evidence::*;
use super::operation::Operation;
use super::query_result::{EffectBounds, EffectResult, Traversal, UnknownCause};
use super::query_result::{
    QueryData, QueryResult, ReferenceCertainty, ReferenceRecord, Status, SymbolInfo, SymbolRecord,
    TypeRecord,
};
use super::*;
use crate::ai::SymbolKind;
use crate::qualified_name::QualifiedName;
use crate::semantic::builtin_types::BuiltinTypeId as Builtin;
use serde_json::{Value, json};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticFacts {
    #[serde(default)]
    pub interop: interop::InteropGraph,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rust_bridges: Vec<crate::rust_bridge_ir::RustBridgeSymbol>,
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
    #[serde(serialize_with = "super::evidence::serialize_facts")]
    pub witnesses: BTreeMap<String, Vec<EffectFact>>,
    pub compiler_effects: BTreeMap<String, u8>,
    #[serde(default)]
    pub panic_evidence: BTreeMap<String, crate::semantic::type_checker::PanicEvidence>,
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
    /// Source callee of a function-value call into a shared candidate bucket.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indirect: Option<String>,
    pub location: Location,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedCall {
    pub operation: Operation,
    pub location: Location,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expression {
    pub function: String,
    pub location: Location,
    pub ty: Option<Type>,
    pub target: Option<String>,
    pub operation: Option<Operation>,
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
    pub field_call_names: HashMap<Span, String>,
    // Builtin calls stay out of `call_sites`, but their compiler boundary
    // witnesses still need the owning call span.
    pub builtin_calls: Vec<(FunctionId, Span, FunctionId)>,
    pub witnesses: HashMap<FunctionId, Vec<EffectFact>>,
    pub compiler_effects: HashMap<FunctionId, u8>,
    pub panic_evidence: HashMap<FunctionId, crate::semantic::type_checker::PanicEvidence>,
    // Keep operation sites separate so capability replacement scans stay bounded.
    suspension_sites: HashMap<FunctionId, Vec<EffectFact>>,
    builtin_sites: HashMap<FunctionId, Vec<EffectFact>>,
}
#[path = "flow.rs"]
mod flow;
use flow::Body;
// Prefer an explicit allocation over an incidental panic path or a propagated
// capability. Equal-ranked witnesses retain stable traversal order.
fn prefer_allocation(effect: u8, candidate: &Witness, existing: &Witness) -> bool {
    effect == RuntimeEffects::MAY_ALLOCATE.bits()
        && candidate
            .operation()
            .is_some_and(CapabilityOperation::explicit_allocation)
        && !existing
            .operation()
            .is_some_and(CapabilityOperation::explicit_allocation)
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
        for fact in self
            .witnesses
            .values_mut()
            .flatten()
            .chain(self.suspension_sites.values_mut().flatten())
        {
            if let Some(operand) = fact
                .witness
                .awaited
                .as_mut()
                .and_then(|awaited| awaited.operand.as_mut())
            {
                *operand = normalize(operand);
            }
        }
    }
    pub fn builtin_effect_paths(
        &mut self,
        function: FunctionId,
        span: Span,
        method: &str,
        may_panic: bool,
    ) {
        // Zero bits keep presentation facts out of effect-state propagation.
        // RuntimeEffects retain the union, including panic-path allocation.
        let mut witness = Witness::new(WitnessKind::BuiltinEffectPaths);
        witness.cause = Some(Cause::captured(
            span,
            CapabilityOperation::Other(format!("String.{method}")),
        ));
        witness.reason = Some(if may_panic {
            "Normal byte access allocates nothing; an invalid offset allocates a panic message and payload. The conservative allocation bound is retained, including for GC rooting across recover()."
        } else { "Scalar String builtin allocates neither on its normal path nor on a panic path." }.into());
        self.builtin_sites.entry(function).or_default().push(EffectFact {
            effect: 0, witness,
            paths: Some(BuiltinPaths {
                normal: AllocationPaths { may_allocate: AllocationPath::None },
                panic: AllocationPaths { may_allocate: if may_panic { AllocationPath::May } else { AllocationPath::None } },
                scope: "builtin operation only; argument evaluation and recovered continuation excluded".into(),
                operation: None, location: None,
            }),
        });
    }
    pub fn channel_site(&mut self, function: FunctionId, span: Span, operation: &str) {
        let mut witness = Witness::new(WitnessKind::RuntimeCapability);
        witness.certainty = Some(Certainty::ConservativeBound);
        witness.cause = Some(Cause::captured(span, operation.into()));
        witness.reason = Some("May park the task when the channel cannot proceed; this is not evidence of blocking an OS thread.".into());
        self.suspension_sites
            .entry(function)
            .or_default()
            .push(EffectFact {
                effect: RuntimeEffects::MAY_SUSPEND.bits(),
                witness,
                paths: None,
            });
    }
    pub fn capability(&mut self, function: FunctionId, bits: u8, span: Span, operation: &str) {
        let operation = CapabilityOperation::from(operation);
        let witnesses = self.witnesses.entry(function).or_default();
        for bit in 0..RuntimeEffects::BIT_COUNT {
            let effect = RuntimeEffects::from_bit(bit).bits();
            if bits & effect == 0 {
                continue;
            }
            let existing = witnesses.iter().position(|w| w.effect == effect);
            if existing.is_some_and(|i| {
                let witness = &witnesses[i].witness;
                let replace = matches!(
                    witness.kind,
                    WitnessKind::Unavailable | WitnessKind::ExternalBoundary
                ) || (effect == RuntimeEffects::MAY_ALLOCATE.bits()
                    && operation.explicit_allocation()
                    && !witness
                        .operation()
                        .is_some_and(CapabilityOperation::explicit_allocation));
                !replace
            }) {
                continue;
            }
            let mut witness = Witness::new(WitnessKind::RuntimeCapability);
            witness.certainty = Some(Certainty::ConservativeBound);
            let mut cause = Cause::captured(span, operation.clone());
            if effect == RuntimeEffects::MAY_BLOCK.bits() && operation == CapabilityOperation::Await
            {
                witness.reason = Some("conservative await bound for the checked operand type: waiting may park or block depending on the selected lowering; this summary does not establish which path executes".into());
            }
            if operation == CapabilityOperation::StringLiteralAllocation {
                witness.reason = Some("The literal slot allocates and permanently roots its string on first use; subsequent uses load the cached object without allocating. This capability includes cold initialization, not an allocation on every evaluation.".into());
            }
            if operation == CapabilityOperation::ArrayLengthNullPanic {
                witness.reason = Some("Normal Array.len()/FrozenArray.len() reads the length without allocating. Only a null receiver (or a malformed negative length) reaches willow_array_len's panic path, which may allocate the panic payload; receiver non-nullness is not proven by this summary.".into());
                if effect == RuntimeEffects::MAY_ALLOCATE.bits() {
                    cause.operation = CapabilityOperation::PanicPayloadAllocation;
                }
            }
            witness.cause = Some(cause);
            let value = EffectFact {
                effect,
                witness,
                paths: None,
            };
            if let Some(i) = existing {
                witnesses[i] = value;
            } else {
                witnesses.push(value);
            }
        }
        witnesses.sort_by_key(|w| w.effect);
    }
    /// Attach the awaited operand type and explain the await's bound. `parks`
    /// is true when the async lowering is known to suspend the task instead of
    /// blocking an OS thread.
    pub fn await_type(
        &mut self,
        function: FunctionId,
        span: Span,
        operand: Option<&Type>,
        parks: bool,
    ) {
        let awaited = AwaitType {
            operand: operand.cloned(),
            parks,
        };
        // Summary witnesses retain one representative per effect. Preserve each
        // await independently, including when a lock already owns that bit.
        let mut witness = Witness::new(WitnessKind::RuntimeCapability);
        witness.certainty = Some(Certainty::ConservativeBound);
        witness.cause = Some(Cause::captured(span, CapabilityOperation::Await));
        witness.awaited = Some(awaited.clone());
        self.suspension_sites
            .entry(function)
            .or_default()
            .push(EffectFact {
                effect: RuntimeEffects::MAY_SUSPEND.bits(),
                witness,
                paths: None,
            });
        if let Some(witnesses) = self.witnesses.get_mut(&function) {
            for entry in witnesses {
                let witness = &mut entry.witness;
                if witness.cause.as_ref().is_some_and(|cause| {
                    matches!(cause.source, CauseSource::Captured { span: site }
                    if (site.file_id, site.start, site.end) == (span.file_id, span.start, span.end))
                }) {
                    witness.awaited = Some(awaited.clone());
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
pub(super) struct CallSiteBounds<'a> {
    pub dispatch: &'a HashMap<(&'a str, usize, usize), String>,
    pub indirect: &'a super::indirect::Sites,
}

/// Status of one effect witness: a witness without a source location is
/// `missing-source`; runtime capabilities are conservative bounds.
#[allow(
    clippy::too_many_lines,
    reason = "call, panic, effect and body facts share one boundary-site index and location resolver; their ordered assembly keeps witness provenance and final deduplication together"
)]
pub(super) fn finish(
    captures: &HashMap<UnitId, CapturedUnit>,
    paths: &HashMap<UnitId, String>,
    functions: &HashMap<(UnitId, FunctionId), Function>,
    resolve: &impl Fn(UnitId, FunctionId) -> (UnitId, FunctionId),
    bodyless: &impl Fn(UnitId, &FunctionId) -> bool,
    fingerprints: &HashMap<Span, String>,
    call_sites: &CallSiteBounds<'_>,
) -> Result<SemanticFacts> {
    let location = |span: Span| -> Result<Location> {
        Ok(Location {
            display: None,
            path: paths
                .get(&crate::module::ModuleId(span.file_id.0))
                .context("semantic source")?
                .clone(),
            start: span.start,
            end: span.end,
        })
    };
    // Compiler summaries include transitive boundary witnesses. Seed only at
    // the owning call; the effect graph propagates evidence to reachable callers.
    // Retain one span per caller/target pair without rescanning call sites.
    let mut boundary_sites = HashMap::new();
    for (&unit, capture) in captures {
        let calls = capture
            .semantic
            .call_sites
            .iter()
            .filter_map(|(caller, span, target, _)| {
                target.as_ref().map(|target| (caller, span, target))
            });
        let builtins = capture
            .semantic
            .builtin_calls
            .iter()
            .map(|(caller, span, target)| (caller, span, target));
        for (caller, span, target) in calls.chain(builtins) {
            boundary_sites
                .entry((resolve(unit, *caller), resolve(unit, *target)))
                .and_modify(|previous: &mut Span| {
                    let key = |s: Span| (s.file_id.0, s.start, s.end);
                    if key(*span) < key(*previous) {
                        *previous = *span;
                    }
                })
                .or_insert(*span);
        }
    }
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
            // Prefer the checked implementation (which may belong to a base
            // class). A receiver-qualified name need not have its own body.
            if let Some(target) = call_sites.indirect.get(&(unit, *caller, *span))
                && let Some(caller) = functions.get(&resolve(unit, *caller))
            {
                result
                    .effect_calls
                    .entry(caller.id.clone())
                    .or_default()
                    .push(EffectCall {
                        target: target.clone(),
                        indirect: Some(operation.clone()),
                        location: location(*span)?,
                    });
                continue;
            }
            let effect_target = target.as_ref();
            let site = location(*span)?;
            let dispatch = call_sites
                .dispatch
                .get(&(site.path.as_str(), site.start, site.end));
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
                        indirect: None,
                        target: target.id.clone(),
                        location: location(*span)?,
                    });
            }
            // Calls whose effects are already known without a body (variant
            // constructors, intrinsics) do not explain an unknown caller.
            let resolved_dispatch = dispatch.is_some();
            if let Some(dispatch) = dispatch
                && let Some(caller) = functions.get(&resolve(unit, *caller))
            {
                result
                    .effect_calls
                    .entry(caller.id.clone())
                    .or_default()
                    .push(EffectCall {
                        indirect: None,
                        target: dispatch.clone(),
                        location: site,
                    });
            }
            if resolved_dispatch
                || effect_target.is_some_and(|target| {
                    functions.contains_key(&resolve(unit, *target)) || bodyless(unit, target)
                })
            {
                continue;
            }
            if let Some(caller) = functions.get(&resolve(unit, *caller)) {
                result
                    .unresolved_calls
                    .entry(caller.id.clone())
                    .or_default()
                    .push(UnresolvedCall {
                        operation: operation.clone().into(),
                        location: location(*span)?,
                    });
            }
        }
        for (&id, evidence) in &captures[&unit].semantic.panic_evidence {
            if let Some(function) = functions.get(&resolve(unit, id)) {
                result
                    .panic_evidence
                    .insert(function.id.clone(), evidence.clone());
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
            .chain(&captures[&unit].semantic.builtin_sites)
        {
            let function = &functions[&resolve(unit, *id)].id;
            let mut local_witnesses = Vec::with_capacity(witnesses.len());
            for mut fact in witnesses.iter().cloned() {
                let w = &mut fact.witness;
                if let Some(target) = w.target_key.take() {
                    if functions
                        .get(&resolve(unit, target))
                        .is_some_and(|function| {
                            !function.synthetic && function.body_location.is_some()
                        })
                    {
                        continue;
                    }
                    let Some(site) =
                        boundary_sites.get(&(resolve(unit, *id), resolve(unit, target)))
                    else {
                        continue;
                    };
                    let mut cause = Cause::captured(*site, CapabilityOperation::ExternalCall);
                    cause.source = CauseSource::Located {
                        location: Some(location(*site)?),
                    };
                    w.cause = Some(cause);
                }
                if let Some(owner) = w.owner_key.take() {
                    w.owner = Some(
                        functions
                            .get(&resolve(unit, owner))
                            .map_or_else(|| owner.to_string(), |function| function.id.clone()),
                    );
                }
                if let Some(cause) = &mut w.cause
                    && let CauseSource::Captured { span } = cause.source
                {
                    cause.source = CauseSource::Located {
                        location: location(span).ok(),
                    };
                    if let Some(lock) = &mut cause.lock
                        && let Some(SourcePoint::Captured(span)) = lock.target
                    {
                        lock.target = location(span).ok().map(SourcePoint::Located);
                    }
                }
                local_witnesses.push(fact);
            }
            result
                .witnesses
                .entry(function.clone())
                .or_default()
                .extend(local_witnesses);
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
                let key = (*span, operation.as_ref());
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
pub(super) fn local_ranges(locations: &[Location]) -> Vec<(&str, usize, usize)> {
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
trait EvidenceSite {
    fn effect(&self) -> u8;
    fn witness(&self) -> Option<&Witness>;
}
impl EvidenceSite for EffectFact {
    fn effect(&self) -> u8 {
        self.effect
    }
    fn witness(&self) -> Option<&Witness> {
        Some(&self.witness)
    }
}
impl EvidenceSite for ExplainedEffect {
    fn effect(&self) -> u8 {
        self.effect
    }
    fn witness(&self) -> Option<&Witness> {
        self.witness.as_ref()
    }
}
fn deduplicate_evidence<T: EvidenceSite>(facts: &mut Vec<T>) {
    let mut seen = HashMap::new();
    let mut unique = Vec::with_capacity(facts.len());
    for fact in facts.drain(..) {
        let Some(location) = fact.witness().and_then(Witness::location) else {
            unique.push(fact);
            continue;
        };
        let key = (
            fact.effect(),
            location.path.clone(),
            location.start,
            location.end,
        );
        if let Some(&index) = seen.get(&key) {
            if fact
                .witness()
                .and_then(Witness::operation)
                .is_some_and(CapabilityOperation::channel)
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
    effect_states: Option<effect_states::Analysis>,
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
/// Call references through a callable binding or parameter, keyed by its
/// shareable type: a named function flows into a task-local `fn(...)` too.
fn indirect_reference_index(
    snapshot: &Snapshot,
    symbols: &HashMap<String, usize>,
) -> HashMap<Type, Vec<usize>> {
    let mut index: HashMap<Type, Vec<usize>> = HashMap::new();
    for (i, r) in snapshot.semantic.references.iter().enumerate() {
        if r.role == "call"
            && let Some(&s) = symbols.get(&r.target)
            && matches!(
                snapshot.semantic.symbols[s].kind,
                SymbolKind::Binding | SymbolKind::Parameter
            )
            && let Some(ty) = &snapshot.semantic.symbols[s].ty
        {
            index
                .entry(ty.with_callable_kind(true))
                .or_default()
                .push(i);
        }
    }
    index
}

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
        let indirect_references = indirect_reference_index(&snapshot, &symbols);
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
            effect_states: None,
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
    fn symbol_values(&self, symbols: &[&symbols::Symbol]) -> Vec<SymbolRecord> {
        symbols.iter().map(|s| self.names.symbol(s)).collect()
    }
    pub(super) fn type_query(&mut self, file: String, byte: usize) -> QueryResult {
        self.queries += 1;
        self.type_at(file, byte)
    }
    pub(super) fn symbol_query(&mut self, function: &str) -> QueryResult {
        self.queries += 1;
        self.symbol_info(function)
    }
    pub(super) fn references_query(&mut self, function: &str) -> QueryResult {
        self.queries += 1;
        self.function_references(function)
    }
    pub(super) fn effects_query(&mut self, function: &str) -> QueryResult {
        self.queries += 1;
        self.effects(function)
    }
    pub(super) fn symbol_at_query(&mut self, file: &str, byte: usize) -> QueryResult {
        self.queries += 1;
        self.symbol_at(file, byte)
    }
    fn symbol_at(&self, file: &str, byte: usize) -> QueryResult {
        let symbols = self.symbols_at(file, byte);
        if symbols.is_empty() {
            QueryResult::empty(Status::Unknown)
        } else if symbols.len() == 1 {
            QueryResult::symbol(
                Status::Ok,
                SymbolInfo::Declaration(self.names.symbol(symbols[0])),
            )
        } else if let Some(alias) = symbols.iter().find(|s| s.kind == SymbolKind::Import) {
            QueryResult {
                status: Status::Ok,
                data: QueryData::Symbol {
                    symbol: SymbolInfo::Declaration(self.names.symbol(alias)),
                    resolved_symbols: Some(self.symbol_values(&symbols)),
                },
            }
        } else {
            QueryResult {
                status: Status::Ambiguous,
                data: QueryData::Symbols {
                    symbols: self.symbol_values(&symbols),
                    total: None,
                    truncated: None,
                },
            }
        }
    }
    fn declared_type_at(&self, file: &str, byte: usize) -> QueryResult {
        let symbols = self.symbols_at(file, byte);
        let typed: Vec<_> = symbols.iter().filter(|s| s.ty.is_some()).collect();
        if let Some(first) = typed.first() {
            if typed.iter().all(|s| s.ty == first.ty) {
                // A member token's receiver binds the owner's parameters, so
                // only the declaration name itself shows the owner binder.
                let ty = first.ty.as_ref().unwrap();
                let at_declaration = first
                    .location
                    .as_ref()
                    .is_some_and(|l| l.path == file && l.start <= byte && byte < l.end);
                let display = if at_declaration || !first.details.type_params.is_empty() {
                    self.names.declaration(first, ty)
                } else {
                    let module = first.identity.as_ref().map(|i| i.module.as_str());
                    let mut display = self.names.render(ty, module);
                    if first.details.is_async && matches!(ty, Type::Fn(..)) {
                        display.insert_str(0, "async ");
                    }
                    display
                };
                let mut result = TypeRecord {
                    ty: Some(first.ty.clone()),
                    type_display: Some(display),
                    location: first.location.clone(),
                    payload: vec![],
                    payload_display: vec![],
                };
                if let [only] = typed[..] {
                    (result.payload, result.payload_display) = self.names.payload(only);
                }
                QueryResult {
                    status: Status::Ok,
                    data: QueryData::Type(result),
                }
            } else {
                QueryResult {
                    status: Status::Ambiguous,
                    data: QueryData::Symbols {
                        symbols: self.symbol_values(&symbols),
                        total: None,
                        truncated: None,
                    },
                }
            }
        } else {
            QueryResult::empty(if symbols.is_empty() {
                Status::Unknown
            } else {
                Status::Unanalyzed
            })
        }
    }
    /// Spell each acquired lock's operand type like `type` does, in the module
    /// of the locking body: one decode and render per lock witness.
    fn present_lock_types(&self, facts: &mut [ExplainedEffect]) {
        for fact in facts {
            let Some(witness) = &mut fact.witness else {
                continue;
            };
            let module = witness
                .owner
                .as_deref()
                .and_then(|owner| self.functions.get(owner))
                .and_then(|&i| self.snapshot.functions[i].identity.as_ref())
                .map(|identity| identity.module.as_str());
            if let Some(lock) = witness.cause.as_mut().and_then(|cause| cause.lock.as_mut())
                && let Some(ty) = &lock.ty
            {
                lock.type_display = Some(self.names.render(ty, module));
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
    fn typed_call_at(&self, file: &str, byte: usize, module: Option<&str>) -> Option<QueryResult> {
        let calls = self.typed_calls.get(file)?;
        let p = calls.partition_point(|&(start, _, _)| start <= byte);
        let &(_, end, i) = calls.get(p.checked_sub(1)?)?;
        if byte >= end {
            return None;
        }
        let r = &self.snapshot.semantic.references[i];
        let ty = r.ty.as_ref()?;
        let display = self.names.render(ty, module);
        Some(QueryResult {
            status: Status::Ok,
            data: QueryData::Type(TypeRecord {
                ty: Some(Some(ty.clone())),
                type_display: Some(display),
                location: Some(r.location.clone()),
                payload: vec![],
                payload_display: vec![],
            }),
        })
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
            } => self.matching_symbols(&name, &prefix, &module, &symbol_kind, limit),
            QueryRequest::SymbolAt { file, byte, .. } => {
                serde_json::to_value(self.symbol_at(&file, byte)).unwrap()
            }
            QueryRequest::TypeAt { file, byte, .. } => {
                serde_json::to_value(self.type_at(file, byte)).unwrap()
            }
            QueryRequest::SymbolInfo { function, .. } => {
                serde_json::to_value(self.symbol_info(&function)).unwrap()
            }
            QueryRequest::References { function, .. } => {
                serde_json::to_value(self.function_references(&function)).unwrap()
            }
            QueryRequest::Effects { function, .. } => {
                serde_json::to_value(self.effects(&function)).unwrap()
            }
        };
        json!({"revision":self.revision(),"result":data})
    }

    fn matching_symbols(
        &self,
        name: &Option<String>,
        prefix: &Option<String>,
        module: &Option<String>,
        symbol_kind: &Option<String>,
        limit: Option<usize>,
    ) -> Value {
        let symbol_kind = symbol_kind.as_deref().map(SymbolKind::from);
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

    /// The type answer at one source byte: the narrowest typing segment,
    /// falling back to the declared type of the symbol there.
    fn type_at(&mut self, file: String, byte: usize) -> QueryResult {
        let file = if Path::new(&file).is_absolute() {
            file
        } else {
            Path::new(&self.snapshot.workspace)
                .join(file)
                .to_string_lossy()
                .into_owned()
        };
        let Some(segments) = self.positions.get(&file) else {
            return self.declared_type_at(&file, byte);
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
            return self.declared_type_at(&file, byte);
        };
        // Resolved source write tokens are authoritative: compound lowering
        // can put several synthetic expressions on the same token span.
        // A callee token or declaration name (parameter, pattern binding,
        // let) can lie inside an expression; report its declared type
        // unless the symbol itself is untyped.
        if let Some(write) = segment.declared
            && let Some(declared) = self.declared_token_type(&file, byte, segment, write)
        {
            return declared;
        }
        let Some(index) = segment.expression else {
            return self.declared_type_at(&file, byte);
        };
        let Some(index) = index else {
            return QueryResult::empty(Status::Ambiguous);
        };
        let e = &self.snapshot.semantic.expressions[index];
        if let Some(ty) = &e.ty {
            let module = self.expression_module(index);
            let display = self.names.render(ty, module);
            QueryResult {
                status: Status::Ok,
                data: QueryData::Type(TypeRecord {
                    ty: Some(Some(ty.clone())),
                    type_display: Some(display),
                    location: Some(e.location.clone()),
                    payload: vec![],
                    payload_display: vec![],
                }),
            }
        } else {
            let declared = self.declared_type_at(&file, byte);
            if declared.status == Status::Unknown {
                QueryResult {
                    status: Status::Unanalyzed,
                    data: QueryData::Type(TypeRecord {
                        ty: Some(e.ty.clone()),
                        type_display: None,
                        location: Some(e.location.clone()),
                        payload: vec![],
                        payload_display: vec![],
                    }),
                }
            } else {
                declared
            }
        }
    }

    /// The answer for a declared token (`write` for a resolved write) when it
    /// decides the type; `None` defers to the enclosing expression.
    fn declared_token_type(
        &self,
        file: &str,
        byte: usize,
        segment: &Segment,
        write: bool,
    ) -> Option<QueryResult> {
        // The instantiated type is spelled in the calling body,
        // whose enclosing expression names its module.
        let caller = segment
            .expression
            .flatten()
            .map(|i| self.expression_module(i));
        if !write && let Some(typed) = self.typed_call_at(file, byte, caller.flatten()) {
            return Some(typed);
        }
        let declared = self.declared_type_at(file, byte);
        // A type token of `new Box<i64>(..)` keeps the
        // instantiation its expression carries; the declaration
        // only knows `Box<T>`.
        let instantiated = !write
            && segment.expression.flatten().is_some_and(|i| {
                let Some(Type::Generic(name, _)) = &self.snapshot.semantic.expressions[i].ty
                else {
                    return false;
                };
                self.symbols_at(file, byte).iter().any(|s| {
                    matches!(&s.ty, Some(Type::Named(head) | Type::Generic(head, _)) if head == name)
                })
            });
        (!instantiated && (write || matches!(declared.status, Status::Ok | Status::Ambiguous)))
            .then_some(declared)
    }

    fn symbol_info(&self, function: &str) -> QueryResult {
        match self.functions.get(function) {
            Some(&i) => {
                let f = &self.snapshot.functions[i];
                QueryResult::symbol(
                    if f.synthetic {
                        Status::Unanalyzed
                    } else if f.locations.len() > 1 {
                        Status::Ambiguous
                    } else {
                        Status::Ok
                    },
                    SymbolInfo::Function(f.into()),
                )
            }
            None => match self.symbols.get(function) {
                Some(&i) => QueryResult::symbol(
                    Status::Ok,
                    SymbolInfo::Declaration(self.names.symbol(&self.snapshot.semantic.symbols[i])),
                ),
                None => QueryResult::empty(Status::Unknown),
            },
        }
    }

    fn function_references(&self, function: &str) -> QueryResult {
        if let Some(indices) = self
            .symbol_references
            .get(function)
            .filter(|_| !self.functions.contains_key(function))
        {
            let mut references: Vec<_> = indices
                .iter()
                .map(|&i| ReferenceRecord::from(&self.snapshot.semantic.references[i]))
                .collect();
            references.sort_by(|a, b| {
                (&a.location.path, a.location.start, a.location.end).cmp(&(
                    &b.location.path,
                    b.location.start,
                    b.location.end,
                ))
            });
            return QueryResult {
                status: Status::Ok,
                data: QueryData::References {
                    references,
                    coverage: Some("compiler-resolved".into()),
                    unresolved_candidates: None,
                },
            };
        }
        let Some(&root) = self.functions.get(function) else {
            return QueryResult {
                status: if self.symbols.contains_key(function) {
                    Status::Ok
                } else {
                    Status::Unknown
                },
                data: QueryData::References {
                    references: vec![],
                    coverage: None,
                    unresolved_candidates: None,
                },
            };
        };
        let refs = merge_reference_locations(self.call_references(function, root));
        let unresolved = self.signature_matched_candidates(function);
        let mut coverage = String::from("compiler-resolved");
        if refs
            .iter()
            .any(|r| r.certainty == Some(ReferenceCertainty::PossibleDispatch))
        {
            coverage.push_str(" and possible virtual dispatch");
        }
        if !unresolved.is_empty() {
            coverage.push_str("; indirect candidates are signature-matched, not resolved");
        }
        QueryResult {
            status: if unresolved.is_empty() {
                Status::Ok
            } else {
                Status::Incomplete
            },
            data: QueryData::References {
                references: refs,
                unresolved_candidates: Some(unresolved),
                coverage: Some(coverage),
            },
        }
    }

    /// Calls of `root` and of every override reachable through dispatch
    /// callers, then every compiler-resolved token of `function`.
    fn call_references(&self, function: &str, root: usize) -> Vec<ReferenceRecord> {
        let mut seen = std::collections::HashSet::from([root]);
        let mut pending = vec![root];
        let mut refs = Vec::new();
        while let Some(i) = pending.pop() {
            if let Some(indices) = self.references.get(&self.snapshot.functions[i].id) {
                for &j in indices {
                    refs.push(ReferenceRecord::expression(
                        &self.snapshot.semantic.expressions[j],
                        &self.snapshot.functions[i],
                        i == root,
                    ));
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
        if let Some(indices) = self.symbol_references.get(function) {
            for &index in indices {
                let reference = &self.snapshot.semantic.references[index];
                let mut value = ReferenceRecord::from(reference);
                value.certainty = Some(ReferenceCertainty::Resolved);
                refs.push(value);
            }
        }
        refs
    }

    /// Indirect calls through a value of the selected callable's type.
    fn signature_matched_candidates(&self, function: &str) -> Vec<ReferenceRecord> {
        self
            .symbols
            .get(function)
            .and_then(|&i| self.snapshot.semantic.symbols[i].ty.as_ref())
            .and_then(|ty| self.indirect_references.get(&ty.with_callable_kind(true)))
            .into_iter()
            .flatten()
            .map(|&i| {
                let mut candidate = ReferenceRecord::from(&self.snapshot.semantic.references[i]);
                candidate.certainty = Some(ReferenceCertainty::SignatureMatched);
                candidate.reason = Some("Indirect call through a binding or parameter with the same function type as the selected callable; the target is not resolved.".into());
                candidate
            })
            .collect()
    }

    fn effects(&mut self, function: &str) -> QueryResult {
        let Some(&i) = self.functions.get(function) else {
            return QueryResult {
                status: if self.symbols.contains_key(function) { Status::Unanalyzed } else { Status::Unknown },
                data: QueryData::Effects(EffectResult {
                    schema: effect_states::SCHEMA, effects: effect_states::Analysis::typed_unavailable(),
                    reason: Some(if self.symbols.get(function).is_some_and(|&i| self.snapshot.semantic.symbols[i].kind == SymbolKind::Method) {
                        "This interface contract has no executable body. Query an implementing method for its effects; dispatch effects depend on the implementation."
                    } else { "No executable body is available for this symbol in the selected source graph." }.into()),
                    assumptions: None, unknown_causes: None, bounds: None, effect_evidence: None,
                    no_panic: None, witness: None, compiler_witnesses: None, meaning: None,
                }),
            };
        };
        let states = self
            .effect_states
            .get_or_insert_with(|| effect_states::Analysis::new(&self.snapshot))
            .typed_output(i);
        let assumptions = self
            .effect_states
            .as_ref()
            .unwrap()
            .typed_assumptions(i)
            .into_owned();
        let mut walk = EffectWalk::new(self, i);
        walk.run();
        let missing = walk.complete_missing_witnesses();
        let EffectWalk {
            witness,
            evidence,
            unknown_causes,
            suspension_sites,
            builtin_sites,
            edge_visits,
            ..
        } = walk;
        self.effect_edge_visits += edge_visits;
        let f = &self.snapshot.functions[i];
        let mut evidence: Vec<_> = evidence.into_values().collect();
        evidence.extend(suspension_sites);
        evidence.extend(builtin_sites);
        deduplicate_evidence(&mut evidence);
        self.present_lock_types(&mut evidence);
        self.attach_call_chains(&witness, &mut evidence);
        let compiler_witnesses = self.local_compiler_witnesses(f);
        // A checked candidate union is a complete conservative bound. Other
        // unknown facts must not be hidden by an otherwise complete graph.
        let unresolved = states.iter().any(|state| {
            state.unknown_reasons.iter().any(|reason| {
                !matches!(
                    reason.code,
                    effect_states::UnknownReasonCode::IndirectCall
                        | effect_states::UnknownReasonCode::MissingWitness
                        | effect_states::UnknownReasonCode::MissingSource
                )
            })
        });
        let missing = missing
            || states.iter().any(|state| {
                state.unknown_reasons.iter().any(|reason| {
                    matches!(
                        reason.code,
                        effect_states::UnknownReasonCode::MissingWitness
                            | effect_states::UnknownReasonCode::MissingSource
                    )
                })
            });
        QueryResult {
            status: if f.unknown || unresolved { Status::Unknown } else if missing { Status::Incomplete } else { Status::Ok },
            data: QueryData::Effects(EffectResult {
                schema: effect_states::SCHEMA, effects: states, assumptions: Some(assumptions),
                reason: f.unknown_reason().or(if unresolved { Some("Effect facts remain unknown: see the named callee, source location, and unknown reasons below.") } else { None }).or(if missing { Some("Effect evidence is incomplete: see the named capability and missing-witness or missing-source reason below.") } else { None }).map(str::to_owned),
                unknown_causes: Some(unknown_causes), bounds: Some(EffectBounds { runtime: f.runtime_effects, compiler: self.snapshot.semantic.compiler_effects.get(&f.id).copied() }),
                effect_evidence: Some(evidence), no_panic: Some(self.snapshot.semantic.panic_evidence.get(&f.id).cloned()),
                witness: Some(witness), compiler_witnesses: Some(compiler_witnesses),
                meaning: Some("compiler facts and conservative runtime capability bounds; indirect-call bounds cover checked function values in this workspace, not arbitrary values supplied by future callers; external business effects and predicate feasibility are not implied"),
            }),
        }
    }

    /// The traversal is a parent-before-child spanning tree. Share parent
    /// links, compress synthetic dispatch hops once, and materialize only the
    /// paths actually emitted as evidence.
    fn attach_call_chains(&self, witness: &[Traversal], evidence: &mut [ExplainedEffect]) {
        let mut parents = HashMap::new();
        let mut executable = HashMap::new();
        for node in witness {
            let id = node.function.as_str();
            let parent = node
                .via
                .as_deref()
                .and_then(|id| executable.get(id).copied())
                .flatten();
            if self
                .functions
                .get(id)
                .is_some_and(|&k| !self.snapshot.functions[k].synthetic)
            {
                parents.insert(id, parent);
                executable.insert(id, Some(id));
            } else {
                executable.insert(id, parent);
            }
        }
        for fact in evidence {
            let owner = fact
                .witness
                .as_ref()
                .and_then(|w| w.owner.as_deref())
                .filter(|id| parents.contains_key(id))
                .or(fact.via_function.as_deref());
            let mut chain = Vec::new();
            let mut cursor = owner;
            while let Some(id) = cursor {
                let Some(parent) = parents.get(id) else { break };
                chain.push(id.to_owned());
                cursor = *parent;
            }
            chain.reverse();
            if !chain.is_empty() {
                fact.call_chain = Some(chain);
            }
        }
    }

    /// Compiler witnesses located inside `f`'s own source ranges.
    fn local_compiler_witnesses(&self, f: &Function) -> Option<Vec<ExplainedEffect>> {
        let stored = self.snapshot.semantic.witnesses.get(&f.id)?;
        let ranges = local_ranges(&f.locations);
        let mut facts = Vec::with_capacity(stored.len());
        for fact in stored {
            let w = &fact.witness;
            let Some(loc) = w.location() else {
                continue;
            };
            let index =
                ranges.partition_point(|&(p, s, _)| (p, s) <= (loc.path.as_str(), loc.start));
            if w.owner.as_deref().is_some_and(|owner| owner != f.id)
                || !index.checked_sub(1).is_some_and(|index| {
                    let (p, _, e) = ranges[index];
                    p == loc.path && loc.start <= loc.end && loc.end <= e
                })
            {
                continue;
            }
            let mut explained = ExplainedEffect::new(fact.effect, w.clone(), String::new());
            explained.via_function = None;
            explained.paths = fact.paths.clone();
            if w.operation() == Some(&CapabilityOperation::Await) || w.awaited_type.is_some() {
                explained.task_relationship = Some(TaskRelationship {
                    kind: TaskKind::Await,
                    function: f.id.clone(),
                    location: w.location().cloned(),
                    target: None,
                    reason: None,
                });
            }
            facts.push(explained);
        }
        deduplicate_evidence(&mut facts);
        self.present_lock_types(&mut facts);
        Some(facts)
    }
}

/// One source occurrence is reported once: a resolved fact outranks possible
/// dispatch, and records merge their fields. Sorted by location.
fn merge_reference_locations(refs: Vec<ReferenceRecord>) -> Vec<ReferenceRecord> {
    let mut by_location = HashMap::new();
    let mut unique: Vec<ReferenceRecord> = Vec::with_capacity(refs.len());
    for reference in refs {
        let l = &reference.location;
        let key = (l.path.clone(), l.start, l.end);
        match by_location.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(unique.len());
                unique.push(reference);
            }
            std::collections::hash_map::Entry::Occupied(entry) => {
                unique[*entry.get()].merge(reference);
            }
        }
    }
    unique.sort_by(|a, b| {
        let a = &a.location;
        let b = &b.location;
        (&a.path, a.start, a.end).cmp(&(&b.path, b.start, b.end))
    });
    unique
}

/// A deferred edge to a function value supplied from outside the queried
/// function: target, reaching function, indirect-call origin and call site.
type ForeignEdge = (usize, usize, usize, Option<TaskRelationship>);

/// The evidence walk of one `Effects` query: breadth-first over callees of
/// `root`, each body visited once.
struct EffectWalk<'a> {
    session: &'a QuerySession,
    root: &'a Function,
    seen: std::collections::HashSet<usize>,
    queue: std::collections::VecDeque<(usize, Option<usize>)>,
    // Function values from outside `root` are caller-supplied code: keep
    // their effect bounds, but cite the indirect call site in the reaching
    // function rather than the foreign body. They are visited only after
    // every directly reachable function, so a body reachable both ways keeps
    // its own evidence. Queue provenance with the edge; do not assign it to a
    // body until that path wins against directly reachable paths.
    foreign: std::collections::VecDeque<ForeignEdge>,
    unlocated_foreign: std::collections::VecDeque<ForeignEdge>,
    origins: HashMap<usize, usize>,
    call_sites: HashMap<usize, Option<Witness>>,
    task_routes: HashMap<usize, TaskRelationship>,
    // Shared dispatch nodes carry a call site, not a task boundary. Only an
    // async executable candidate starts one.
    dispatch_sites: HashMap<usize, TaskRelationship>,
    // Prefer a synchronous path when a body is reachable both in this task
    // and in a spawned task. Each body still runs through the evidence walk
    // only once.
    tasks: std::collections::VecDeque<(usize, usize, TaskRelationship)>,
    // Some compiler graph edges bypass a shared dispatch node. Defer these
    // until its source-bearing path has been visited.
    unlocated_tasks: std::collections::VecDeque<(usize, usize)>,
    witness: Vec<Traversal>,
    evidence: BTreeMap<u8, ExplainedEffect>,
    unknown_causes: Vec<UnknownCause>,
    suspension_sites: Vec<ExplainedEffect>,
    builtin_sites: Vec<ExplainedEffect>,
    edge_visits: usize,
}
impl<'a> EffectWalk<'a> {
    fn new(session: &'a QuerySession, root: usize) -> Self {
        let f = &session.snapshot.functions[root];
        Self {
            session,
            root: f,
            seen: std::collections::HashSet::from([root]),
            queue: std::collections::VecDeque::from([(root, None)]),
            foreign: Default::default(),
            unlocated_foreign: Default::default(),
            origins: HashMap::new(),
            call_sites: HashMap::new(),
            task_routes: HashMap::new(),
            dispatch_sites: HashMap::new(),
            tasks: Default::default(),
            unlocated_tasks: Default::default(),
            witness: vec![Traversal {
                function: f.id.clone(),
                via: None,
                reason: f.unknown_reason().map(str::to_owned),
                unresolved: f.unresolved.clone(),
                runtime_effects: None,
            }],
            evidence: BTreeMap::new(),
            unknown_causes: Vec::new(),
            suspension_sites: Vec::new(),
            builtin_sites: Vec::new(),
            edge_visits: 0,
        }
    }

    fn run(&mut self) {
        while let Some((j, via)) = self.next() {
            let origin = self.origins.get(&j).copied();
            self.record_unknown_cause(j, via);
            self.record_witness_facts(j, origin);
            self.enqueue_callees(j, via, origin);
        }
    }

    /// The witness line of `k` reached from `j`.
    fn reached(&self, k: usize, j: usize) -> Traversal {
        let functions = &self.session.snapshot.functions;
        Traversal {
            function: functions[k].id.clone(),
            via: Some(functions[j].id.clone()),
            runtime_effects: Some(functions[k].runtime_effects),
            reason: functions[k].unknown_reason().map(str::to_owned),
            unresolved: functions[k].unresolved.clone(),
        }
    }

    /// Next body to visit: direct callees first, then spawned tasks, then
    /// foreign function values, then tasks without a source call site.
    fn next(&mut self) -> Option<(usize, Option<usize>)> {
        let session = self.session;
        loop {
            if let Some(next) = self.queue.pop_front() {
                return Some(next);
            } else if let Some((k, j, route)) = self.tasks.pop_front() {
                if !self.seen.insert(k) {
                    continue;
                }
                self.task_routes.insert(k, route);
                self.witness.push(self.reached(k, j));
                return Some((k, Some(j)));
            } else if let Some((k, j, origin, call)) = self
                .foreign
                .pop_front()
                .or_else(|| self.unlocated_foreign.pop_front())
            {
                if !self.seen.insert(k) {
                    continue;
                }
                self.origins.insert(k, origin);
                if let Some(route) = self.task_routes.get(&j) {
                    self.task_routes.insert(k, route.clone());
                } else if session.async_functions.contains(&k) {
                    let mut route = call.unwrap_or_else(|| TaskRelationship {
                        kind: TaskKind::Spawn,
                        function: session.snapshot.functions[j].id.clone(),
                        location: None,
                        target: None,
                        reason: Some(
                            "No source call site is available for this async graph edge.".into(),
                        ),
                    });
                    route.target = Some(session.snapshot.functions[k].id.clone());
                    self.task_routes.insert(k, route);
                } else if session.snapshot.functions[k].synthetic
                    && let Some(call) = call
                {
                    self.dispatch_sites.insert(k, call);
                }
                self.witness.push(self.reached(k, j));
                return Some((k, Some(j)));
            } else if let Some((k, j)) = self.unlocated_tasks.pop_front() {
                if !self.seen.insert(k) {
                    continue;
                }
                self.task_routes.insert(
                    k,
                    TaskRelationship {
                        kind: TaskKind::Spawn,
                        function: session.snapshot.functions[j].id.clone(),
                        target: Some(session.snapshot.functions[k].id.clone()),
                        location: None,
                        reason: Some(
                            "No source call site is available for this async graph edge.".into(),
                        ),
                    },
                );
                self.witness.push(self.reached(k, j));
                return Some((k, Some(j)));
            } else {
                return None;
            }
        }
    }

    fn record_unknown_cause(&mut self, j: usize, via: Option<usize>) {
        let session = self.session;
        let cause = &session.snapshot.functions[j];
        if cause.unknown
            && (!cause.unresolved.is_empty() || (cause.synthetic && session.callees[j].is_empty()))
        {
            let mut call_sites = session
                .snapshot
                .semantic
                .unresolved_calls
                .get(&cause.id)
                .cloned()
                .unwrap_or_default();
            call_sites.extend(
                session
                    .snapshot
                    .semantic
                    .effect_calls
                    .get(&cause.id)
                    .into_iter()
                    .flatten()
                    .filter_map(|site| {
                        let callee = site.indirect.as_ref()?;
                        if session
                            .functions
                            .get(&site.target)
                            .is_some_and(|&target| !session.snapshot.functions[target].unknown)
                        {
                            return None;
                        }
                        Some(UnresolvedCall {
                            operation: callee.clone().into(),
                            location: site.location.clone(),
                        })
                    }),
            );
            self.unknown_causes.push(UnknownCause {
                function: cause.id.clone(),
                name: cause.name.clone(),
                module: cause.module.clone(),
                locations: cause.locations.clone(),
                unresolved: cause.unresolved.clone(),
                call_sites: (!call_sites.is_empty()).then_some(call_sites),
                caller_location: if cause.locations.is_empty() {
                    via.and_then(|v| session.snapshot.functions[v].locations.first())
                        .cloned()
                } else {
                    None
                },
                reason: cause.unknown_reason().map(str::to_owned),
            });
        }
    }

    /// Folds the operation witnesses of body `j` into the evidence of the
    /// root's runtime effects, citing the indirect call of `origin` for a
    /// foreign body.
    fn record_witness_facts(&mut self, j: usize, origin: Option<usize>) {
        let session = self.session;
        let f = self.root;
        let Some(facts) = session
            .snapshot
            .semantic
            .witnesses
            .get(&session.snapshot.functions[j].id)
        else {
            return;
        };
        for fact in facts {
            let effect = fact.effect;
            if effect == 0 && fact.witness.kind == WitnessKind::BuiltinEffectPaths {
                let mut explained = ExplainedEffect::new(
                    0,
                    fact.witness.clone(),
                    session.snapshot.functions[j].id.clone(),
                );
                explained.paths = fact.paths.clone().map(|mut paths| {
                    paths.operation = fact.witness.operation().cloned();
                    paths.location = Some(fact.witness.location().cloned());
                    paths
                });
                self.builtin_sites.push(explained);
                continue;
            }
            let (witness, via_function) = match origin {
                None => (&fact.witness, j),
                Some(origin) => {
                    let site = self.call_sites.entry(origin).or_insert_with(|| {
                        session.snapshot.semantic.witnesses.get(&session.snapshot.functions[origin].id)?
                            .iter().find(|fact| fact.witness.operation() == Some(&CapabilityOperation::IndirectCall))
                            .map(|fact| {
                                let mut witness = fact.witness.clone();
                                witness.reason = Some("Bound from function values reaching this indirect call; their bodies belong to the code that supplies them.".into());
                                witness
                            })
                    });
                    match site {
                        Some(site) => (&*site, origin),
                        None => (&fact.witness, j),
                    }
                }
            };
            if f.runtime_effects & effect == 0 || witness.kind == WitnessKind::Unavailable {
                continue;
            }
            let suspension = effect == RuntimeEffects::MAY_SUSPEND.bits();
            if !suspension
                && self.evidence.get(&effect).is_some_and(|entry| {
                    entry
                        .witness
                        .as_ref()
                        .is_none_or(|existing| !prefer_allocation(effect, witness, existing))
                })
            {
                continue;
            }
            let mut explained = ExplainedEffect::new(
                effect,
                witness.clone(),
                session.snapshot.functions[via_function].id.clone(),
            );
            if let Some(route) = self.task_routes.get(&j) {
                explained.task_relationship = Some(route.clone());
            } else if witness.operation() == Some(&CapabilityOperation::Await)
                || witness.awaited_type.is_some()
            {
                explained.task_relationship = Some(TaskRelationship {
                    kind: TaskKind::Await,
                    function: session.snapshot.functions[via_function].id.clone(),
                    location: witness.location().cloned(),
                    target: None,
                    reason: None,
                });
            }
            if suspension {
                self.suspension_sites.push(explained);
            } else {
                self.evidence.insert(effect, explained);
            }
        }
    }

    /// Whether body `k` lies inside one of the root's source ranges.
    fn nested(&self, k: usize) -> bool {
        self.session.snapshot.functions[k]
            .locations
            .iter()
            .any(|inner| {
                self.root.locations.iter().any(|outer| {
                    inner.path == outer.path && outer.start <= inner.start && inner.end <= outer.end
                })
            })
    }

    /// Queues each unseen callee of `j` by its task and provenance class.
    fn enqueue_callees(&mut self, j: usize, via: Option<usize>, origin: Option<usize>) {
        let session = self.session;
        let cause = &session.snapshot.functions[j];
        let sites: HashMap<_, _> = session
            .snapshot
            .semantic
            .effect_calls
            .get(&cause.id)
            .into_iter()
            .flatten()
            .map(|site| (site.target.as_str(), site))
            .collect();
        let bucket = matches!(
            cause.synthetic_kind,
            crate::qualified_name::SyntheticKind::Indirect { .. }
        );
        for &k in &session.callees[j] {
            self.edge_visits += 1;
            let origin = origin.or(via.filter(|_| bucket && !self.nested(k)));
            if self.seen.contains(&k) {
                continue;
            }
            let target = &session.snapshot.functions[k];
            let asynchronous = session.async_functions.contains(&k);
            let call = if asynchronous || target.synthetic {
                sites
                    .get(target.id.as_str())
                    .map(|site| TaskRelationship {
                        kind: TaskKind::Spawn,
                        function: cause.id.clone(),
                        target: Some(target.id.clone()),
                        location: Some(site.location.clone()),
                        reason: None,
                    })
                    .or_else(|| self.dispatch_sites.get(&j).cloned())
            } else {
                None
            };
            if let Some(origin) = origin {
                let pending = (k, j, origin, call);
                if asynchronous && pending.3.is_none() && !self.task_routes.contains_key(&j) {
                    self.unlocated_foreign.push_back(pending);
                } else {
                    self.foreign.push_back(pending);
                }
            } else {
                if asynchronous {
                    if let Some(mut route) = self.task_routes.get(&j).cloned().or(call) {
                        if !self.task_routes.contains_key(&j) {
                            route.target = Some(target.id.clone());
                        }
                        self.tasks.push_back((k, j, route));
                    } else {
                        self.unlocated_tasks.push_back((k, j));
                    }
                    continue;
                } else if target.synthetic
                    && let Some(call) = call
                {
                    self.dispatch_sites.insert(k, call);
                }
                self.seen.insert(k);
                if let Some(route) = self.task_routes.get(&j) {
                    self.task_routes.insert(k, route.clone());
                }
                self.queue.push_back((k, Some(j)));
                self.witness.push(self.reached(k, j));
            }
        }
    }

    /// Adds a missing-witness entry for each root effect without evidence and
    /// reports whether any evidence is incomplete.
    fn complete_missing_witnesses(&mut self) -> bool {
        let f = self.root;
        let mut missing = self
            .evidence
            .values()
            .chain(&self.suspension_sites)
            .any(|e| e.status == Some(EvidenceStatus::MissingSource));
        for bit in 0..RuntimeEffects::BIT_COUNT {
            let effect = RuntimeEffects::from_bit(bit).bits();
            if f.runtime_effects & effect == 0
                || (effect == RuntimeEffects::MAY_SUSPEND.bits()
                    && !self.suspension_sites.is_empty())
            {
                continue;
            }
            self.evidence.entry(effect).or_insert_with(|| {
                missing = true;
                ExplainedEffect { effect, status: Some(EvidenceStatus::MissingWitness), witness: None,
                    via_function: None, paths: None, task_relationship: None, call_chain: None,
                    reason: Some(if f.unknown {
                        "Retained as a conservative capability of an unresolved call or dispatch candidate; no operation witness is available. This does not prove that this function suspends or blocks, including when its declaration is synchronous."
                    } else {
                        "Retained from the compiler/runtime capability summary, but no operation witness is available. This is an upper bound, not proof that this function performs the effect."
                    }.into()) }

            });
        }
        missing
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
            let kind = if matches!(operation, Operation::Timeout) { 1 }
                else if operation.is_call()
                    && e.ty.as_ref().is_some_and(|ty| matches!(ty, Type::Bool) || matches!(ty,Type::Generic(name,_) if Builtin::from_name(QualifiedName::new(name).last_segment()) == Some(Builtin::Result))) { 2 }
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
            if e.target.is_none() && functions[e.function.as_str()].unknown && operation.is_call() {
                unresolved_retry_calls = true;
            }
            // One union per caller, not per repeated virtual call site. The
            // caller graph may include other calls, so this is only a candidate.
            if matches!(operation, Operation::Method(_)) && dispatch_owners.insert(&e.function) {
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
        let mut old = HashMap::<(&str, &Operation, &str), [usize; 3]>::new();
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
        // This is the fail-closed edit-safety projection, not the public
        // effect certainty analysis. Both evidence and uncertainty prohibit
        // treating a call as pure. Public queries use effect_states::Analysis.
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
                let direct_call = op.is_direct_call() && e.target.is_some();
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
                .entry((&e.function, e.operation.as_ref().unwrap(), &e.fingerprint))
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
                && operation.is_direct_call();
            let direct_effect = matches!(operation, Operation::Io(_) | Operation::Write(_));
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
mod tests;
