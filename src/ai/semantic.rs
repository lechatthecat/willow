//! Compact compiler facts, structured control flow, and snapshot query adapter.
use super::*;
use serde_json::{Value, json};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticFacts {
    pub symbols: Vec<symbols::Symbol>,
    pub references: Vec<symbols::Reference>,
    pub expressions: Vec<Expression>,
    pub flows: Vec<Flow>,
    pub witnesses: BTreeMap<String, Vec<Value>>,
    pub compiler_effects: BTreeMap<String, u8>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flow {
    pub function: String,
    pub nodes: Vec<FlowNode>,
    pub entry: usize,
    pub complete: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    bodies: Vec<Body>,
    pub witnesses: HashMap<FunctionId, Vec<Value>>,
    pub compiler_effects: HashMap<FunctionId, u8>,
}
#[path = "flow.rs"]
mod flow;
use flow::Body;
impl Captured {
    pub fn capability(&mut self, function: FunctionId, bits: u8, span: Span, operation: &str) {
        let witnesses = self.witnesses.entry(function).or_default();
        for bit in 0..RuntimeEffects::BIT_COUNT {
            let effect = RuntimeEffects::from_bit(bit).bits();
            if bits & effect == 0 {
                continue;
            }
            let value = json!({"effect":effect,"witness":{"kind":"runtime-capability","certainty":"conservative-bound","cause":{"span":span,"operation":operation}}});
            if let Some(existing) = witnesses.iter_mut().find(|w| w["effect"] == effect) {
                if existing["witness"]["kind"] == "unavailable" {
                    *existing = value;
                }
            } else {
                witnesses.push(value);
            }
        }
        witnesses.sort_by_key(|w| w["effect"].as_u64().unwrap());
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
pub(super) fn finish(
    captures: &HashMap<UnitId, CapturedUnit>,
    paths: &HashMap<UnitId, String>,
    functions: &HashMap<(UnitId, FunctionId), Function>,
    resolve: &impl Fn(UnitId, FunctionId) -> (UnitId, FunctionId),
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
    let mut result = SemanticFacts::default();
    let mut units: Vec<_> = captures.keys().copied().collect();
    units.sort();
    for unit in units {
        for (&id, &effects) in &captures[&unit].semantic.compiler_effects {
            if let Some(function) = functions.get(&resolve(unit, id)) {
                *result
                    .compiler_effects
                    .entry(function.id.clone())
                    .or_default() |= effects;
            }
        }
        for (id, witnesses) in &captures[&unit].semantic.witnesses {
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
                }
            }
            result.witnesses.insert(function.clone(), witnesses);
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
    Ok(result)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum QueryRequest {
    Symbols {
        revision: String,
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
    pub(crate) snapshot: Snapshot,
    functions: HashMap<String, usize>,
    symbols: HashMap<String, usize>,
    symbol_references: HashMap<String, Vec<usize>>,
    symbol_positions: HashMap<String, Vec<(usize, usize, String)>>,
    references: HashMap<String, Vec<usize>>,
    positions: HashMap<String, Vec<(usize, usize, Option<usize>)>>,
    callees: Vec<Vec<usize>>,
    dispatch_callers: Vec<Vec<usize>>,
    indirect_references: HashMap<Type, Vec<usize>>,
    pub(crate) incomplete_references: bool,
    pub queries: usize,
    pub position_comparisons: usize,
    pub effect_edge_visits: usize,
}
impl QuerySession {
    pub fn new(snapshot: Snapshot) -> Result<Self> {
        snapshot.validate()?;
        let functions = snapshot
            .functions
            .iter()
            .enumerate()
            .map(|(i, f)| (f.id.clone(), i))
            .collect();
        let mut references: HashMap<String, Vec<usize>> = HashMap::new();
        let mut positions: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, e) in snapshot.semantic.expressions.iter().enumerate() {
            if let Some(target) = &e.target {
                references.entry(target.clone()).or_default().push(i);
            }
            positions
                .entry(e.location.path.clone())
                .or_default()
                .push(i);
        }
        let positions = positions
            .into_iter()
            .map(|(path, ids)| {
                let mut events = Vec::with_capacity(ids.len() * 2);
                for i in ids {
                    let l = &snapshot.semantic.expressions[i].location;
                    if l.start < l.end {
                        events.push((l.start, true, i));
                        events.push((l.end, false, i));
                    }
                }
                events.sort_unstable();
                let mut active = std::collections::BTreeSet::new();
                let mut segments = Vec::new();
                let mut p = 0;
                while p < events.len() {
                    let at = events[p].0;
                    while p < events.len() && events[p].0 == at {
                        let (_, add, i) = events[p];
                        let l = &snapshot.semantic.expressions[i].location;
                        let key = (l.end - l.start, i);
                        if add {
                            active.insert(key);
                        } else {
                            active.remove(&key);
                        }
                        p += 1;
                    }
                    if p < events.len() && !active.is_empty() {
                        let mut it = active.iter();
                        let &(width, i) = it.next().unwrap();
                        let unique = it.next().is_none_or(|&(w, _)| w != width);
                        segments.push((at, events[p].0, unique.then_some(i)));
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
        let incomplete_references = snapshot.functions.iter().any(|f| f.unknown)
            || snapshot.semantic.expressions.iter().any(|e| {
                e.operation
                    .as_deref()
                    .is_some_and(|op| op.starts_with("method:"))
            });
        Ok(Self {
            snapshot,
            functions,
            references,
            symbols,
            symbol_references,
            symbol_positions,
            positions,
            callees,
            dispatch_callers,
            indirect_references,
            incomplete_references,
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
    fn symbol_at(&self, file: &str, byte: usize) -> Value {
        let symbols = self.symbols_at(file, byte);
        if symbols.is_empty() {
            json!({"status":"unknown"})
        } else if symbols.len() == 1 {
            json!({"status":"ok","symbol":symbols[0]})
        } else if let Some(alias) = symbols.iter().find(|s| s.kind == "import") {
            json!({"status":"ok","symbol":alias,"resolved_symbols":symbols})
        } else {
            json!({"status":"ambiguous","symbols":symbols})
        }
    }
    fn declared_type_at(&self, file: &str, byte: usize) -> Value {
        let symbols = self.symbols_at(file, byte);
        let typed: Vec<_> = symbols.iter().filter(|s| s.ty.is_some()).collect();
        if let Some(first) = typed.first() {
            if typed.iter().all(|s| s.ty == first.ty) {
                json!({"status":"ok","type":first.ty,"location":first.location})
            } else {
                json!({"status":"ambiguous","symbols":symbols})
            }
        } else {
            json!({"status":if symbols.is_empty(){"unknown"}else{"unanalyzed"}})
        }
    }
    pub fn revision(&self) -> &str {
        &self.snapshot.revision
    }
    pub fn query(&mut self, request: QueryRequest) -> Value {
        self.queries += 1;
        let revision = match &request {
            QueryRequest::Symbols { revision }
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
            QueryRequest::Symbols { .. } => {
                json!({"status":"ok","symbols":self.snapshot.semantic.symbols})
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
                let Some(ids) = self.positions.get(&file) else {
                    return json!({"revision":self.revision(),"result":self.declared_type_at(&file,byte)});
                };
                let p = ids.partition_point(|s| {
                    self.position_comparisons += 1;
                    s.0 <= byte
                });
                let Some(&(_, end, index)) = p.checked_sub(1).and_then(|p| ids.get(p)) else {
                    return json!({"revision":self.revision(),"result":self.declared_type_at(&file,byte)});
                };
                if byte >= end {
                    return json!({"revision":self.revision(),"result":self.declared_type_at(&file,byte)});
                }
                let Some(index) = index else {
                    return json!({"revision":self.revision(),"result":{"status":"ambiguous"}});
                };
                let e = &self.snapshot.semantic.expressions[index];
                if e.ty.is_some() {
                    json!({"status":"ok","type":e.ty,"location":e.location})
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
                    Some(&i) => json!({"status":"ok","symbol":self.snapshot.semantic.symbols[i]}),
                    None => json!({"status":"unknown"}),
                },
            },
            QueryRequest::References { function, .. } => {
                if let Some(indices) = self
                    .symbol_references
                    .get(&function)
                    .filter(|_| !self.functions.contains_key(&function))
                {
                    let references: Vec<_> = indices
                        .iter()
                        .map(|&i| &self.snapshot.semantic.references[i])
                        .collect();
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
                    let unresolved: Vec<_> = self
                        .symbols
                        .get(&function)
                        .and_then(|&i| self.snapshot.semantic.symbols[i].ty.as_ref())
                        .and_then(|ty| self.indirect_references.get(ty))
                        .into_iter()
                        .flatten()
                        .map(|&i| &self.snapshot.semantic.references[i])
                        .collect();
                    json!({"status":if unresolved.is_empty(){"ok"}else{"incomplete"}, "references":refs,
                        "unresolved_candidates":unresolved,"coverage":"compiler-resolved and possible virtual dispatch; indirect candidates are signature-matched, not resolved"})
                }
            }
            QueryRequest::Effects { function, .. } => match self.functions.get(&function) {
                None => {
                    json!({"status":if self.symbols.contains_key(&function) {"unanalyzed"} else {"unknown"}})
                }
                Some(&i) => {
                    let f = &self.snapshot.functions[i];
                    let mut seen = std::collections::HashSet::from([i]);
                    let mut queue = std::collections::VecDeque::from([i]);
                    let mut witness = vec![json!({"function":f.id,"via":null})];
                    let mut evidence = BTreeMap::new();
                    while let Some(j) = queue.pop_front() {
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
                                if f.runtime_effects & effect as u8 == 0
                                    || fact["witness"]["kind"] == "unavailable"
                                {
                                    continue;
                                }
                                evidence.entry(effect).or_insert_with(||json!({"effect":effect,
                                    "status":if fact["witness"].get("cause").is_some() && !fact["witness"]["cause"]["location"].is_object() {"missing-source"}
                                        else if fact["witness"]["kind"]=="runtime-capability" {"conservative-bound"}
                                        else if fact["witness"]["kind"]=="external-boundary" {"external-boundary"}else{"compiler-fact"},
                                    "via_function":self.snapshot.functions[j].id,"witness":fact["witness"]}));
                            }
                        }
                        for &k in &self.callees[j] {
                            self.effect_edge_visits += 1;
                            if seen.insert(k) {
                                queue.push_back(k);
                                witness.push(json!({"function":self.snapshot.functions[k].id,"via":self.snapshot.functions[j].id,"runtime_effects":self.snapshot.functions[k].runtime_effects}));
                            }
                        }
                    }
                    let mut missing = evidence.values().any(|e| e["status"] == "missing-source");
                    for bit in 0..RuntimeEffects::BIT_COUNT {
                        let effect = RuntimeEffects::from_bit(bit).bits();
                        if f.runtime_effects & effect == 0 {
                            continue;
                        }
                        evidence.entry(effect as u64).or_insert_with(|| {
                            missing = true;
                            json!({"effect":effect,"status":"missing-witness"})
                        });
                    }
                    json!({"status":if f.unknown{"unknown"}else if missing{"incomplete"}else{"ok"},
                        "runtime_effects":f.runtime_effects,"compiler_effects":self.snapshot.semantic.compiler_effects.get(&f.id),"effect_evidence":evidence.into_values().collect::<Vec<_>>(),
                        "witness":witness, "compiler_witnesses":self.snapshot.semantic.witnesses.get(&f.id),
                        "meaning":"compiler facts and conservative runtime capability bounds; external business effects and predicate feasibility are not implied"})
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
                .all(|e| e["status"] == "missing-witness")
        );
        let result = session.query(QueryRequest::Effects {
            revision,
            function: "absent".into(),
        });
        assert_eq!(result["result"]["status"], "unknown");
        std::fs::remove_dir_all(directory).unwrap();
    }
}
