use super::*;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub enum Direction {
    Callers,
    Callees,
}
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_nodes: usize,
    pub max_depth: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_nodes: 10_000,
            max_depth: 100,
        }
    }
}
#[derive(Debug, Serialize)]
pub struct ImpactNode {
    pub identity: Option<SymbolIdentity>,
    pub id: String,
    pub level: usize,
    pub distance: usize,
    pub graph_distance: usize,
    pub runtime_effects: u8,
    pub unknown: bool,
    pub reason: Option<&'static str>,
}
#[derive(Debug, Serialize)]
pub struct Impact {
    pub rust_dependencies: Vec<interop::RustCrateIdentity>,
    pub dependency_boundary: Vec<serde_json::Value>,
    pub external_dependencies: Vec<serde_json::Value>,
    pub revision: String,
    pub nodes: Vec<ImpactNode>,
    pub truncated: bool,
    pub unknown: bool,
    pub reason: Option<&'static str>,
    pub edge_visits: usize,
}

/// The call graph before generic instantiations were merged into their
/// source functions. Caller impact walks it, so a function value made for one
/// instantiation does not reach the callers of another (callers of
/// `Heap<B>::push` never run `Heap<A>`'s comparator). Empty when nothing was
/// merged, where the source graph is already exact.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Instances {
    /// Snapshot function index of each node; `None` for a union of every
    /// instance of one function, standing in for a source-level edge that no
    /// instance edge carries (references in never-instantiated templates).
    pub owners: Vec<Option<u32>>,
    pub callees: Vec<Vec<u32>>,
}
impl Instances {
    pub fn is_empty(&self) -> bool {
        self.owners.is_empty()
    }

    /// `ids`/`callees` describe the graph before `aggregate`, `merged` maps
    /// each id to its merged id. Linear in nodes + edges: each source edge
    /// missing from the instance graph adds one union per target function and
    /// one edge per instance of the caller (a never-instantiated template has
    /// a single one).
    pub(super) fn new(
        ids: &[String],
        callees: &[Vec<String>],
        merged: &HashMap<String, String>,
        functions: &[Function],
    ) -> Self {
        if ids.len() == functions.len() {
            return Self::default();
        }
        let function_index: HashMap<&str, u32> = functions
            .iter()
            .enumerate()
            .map(|(i, f)| (f.id.as_str(), i as u32))
            .collect();
        let node_index: HashMap<&str, u32> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i as u32))
            .collect();
        let mut owners: Vec<Option<u32>> = ids
            .iter()
            .map(|id| {
                let id = merged.get(id).unwrap_or(id);
                function_index.get(id.as_str()).copied()
            })
            .collect();
        let mut instances = vec![vec![]; functions.len()];
        for (node, owner) in owners.iter().enumerate() {
            if let Some(owner) = owner {
                instances[*owner as usize].push(node as u32);
            }
        }
        let mut covered = std::collections::HashSet::new();
        let mut edges: Vec<Vec<u32>> = callees
            .iter()
            .enumerate()
            .map(|(caller, targets)| {
                targets
                    .iter()
                    .filter_map(|target| node_index.get(target.as_str()).copied())
                    .inspect(|&target| {
                        if let (Some(a), Some(b)) = (owners[caller], owners[target as usize]) {
                            covered.insert((a, b));
                        }
                    })
                    .collect()
            })
            .collect();
        // A function added after aggregation keeps its source edges below.
        for (function, nodes) in instances.iter_mut().enumerate() {
            if nodes.is_empty() {
                nodes.push(owners.len() as u32);
                owners.push(Some(function as u32));
                edges.push(vec![]);
            }
        }
        let mut unions = HashMap::new();
        for (caller, f) in functions.iter().enumerate() {
            for target in &f.callees {
                let Some(&target) = function_index.get(target.as_str()) else {
                    continue;
                };
                if !covered.insert((caller as u32, target)) {
                    continue;
                }
                let union = *unions.entry(target).or_insert_with(|| {
                    owners.push(None);
                    edges.push(instances[target as usize].clone());
                    (owners.len() - 1) as u32
                });
                for &node in &instances[caller] {
                    edges[node as usize].push(union);
                }
            }
        }
        Self {
            owners,
            callees: edges,
        }
    }
}

/// Whether `f` is a never-instantiated template: it never runs, so its
/// incomplete summary cannot hide a caller.
fn dead_template(f: &Function) -> bool {
    !f.unresolved.is_empty() && f.unresolved.iter().all(|r| r == generics::UNINSTANTIATED)
}

pub(super) struct Index<'a> {
    pub by_id: HashMap<&'a str, usize>,
    pub callers: Vec<Vec<usize>>,
    pub callees: Vec<Vec<usize>>,
}
impl<'a> Index<'a> {
    pub fn new(functions: &'a [Function]) -> Self {
        let by_id: HashMap<_, _> = functions
            .iter()
            .enumerate()
            .map(|(i, f)| (f.id.as_str(), i))
            .collect();
        let mut callers = vec![vec![]; functions.len()];
        let mut callees = vec![vec![]; functions.len()];
        for (i, f) in functions.iter().enumerate() {
            for target in &f.callees {
                if let Some(&j) = by_id.get(target.as_str()) {
                    callers[j].push(i);
                    callees[i].push(j);
                }
            }
        }
        Self {
            by_id,
            callers,
            callees,
        }
    }
}

pub(super) fn propagate(functions: &mut [Function]) -> usize {
    let mut edge_visits = 0;
    let callers = Index::new(functions).callers;
    let mut queue: VecDeque<_> = (0..functions.len()).collect();
    let mut queued = vec![true; functions.len()];
    while let Some(i) = queue.pop_front() {
        queued[i] = false;
        for &j in &callers[i] {
            edge_visits += 1;
            let bits = functions[j].runtime_effects | functions[i].runtime_effects;
            let unknown = functions[j].unknown | functions[i].unknown;
            if bits != functions[j].runtime_effects || unknown != functions[j].unknown {
                functions[j].runtime_effects = bits;
                functions[j].unknown = unknown;
                if !queued[j] {
                    queued[j] = true;
                    queue.push_back(j);
                }
            }
        }
    }
    edge_visits
}

impl Snapshot {
    /// Byte offsets are zero-based; an end boundary never belongs to the body.
    /// Nested callables choose the most specific range; equal ranges are an
    /// ambiguity (e.g. injected defaults), never silently selected.
    pub fn resolve_position(
        &self,
        path: &Path,
        byte: usize,
        revision: Option<&str>,
    ) -> Result<&Function> {
        self.check_revision(revision)?;
        let path = source_path(path)?.to_string_lossy().into_owned();
        let mut candidates = BTreeMap::new();
        let mut width = usize::MAX;
        for function in &self.functions {
            for span in &function.locations {
                if span.path == path && span.start <= byte && byte < span.end {
                    let size = span.end - span.start;
                    if size < width {
                        candidates.clear();
                        width = size;
                    }
                    if size == width {
                        candidates.insert(&function.id, function);
                    }
                }
            }
        }
        ensure!(
            candidates.len() == 1,
            "source position resolves to {} functions",
            candidates.len()
        );
        Ok(*candidates.values().next().unwrap())
    }
    pub fn check_revision(&self, revision: Option<&str>) -> Result<()> {
        ensure!(
            revision.is_none_or(|r| r == self.revision),
            "stale workspace revision"
        );
        Ok(())
    }
    pub fn impact(
        &self,
        seeds: &[String],
        direction: Direction,
        limits: Limits,
        revision: Option<&str>,
    ) -> Result<Impact> {
        self.check_revision(revision)?;
        ensure!(limits.max_nodes > 0, "max_nodes must be positive");
        let index = Index::new(&self.functions);
        let instances = &self.semantic.instances;
        let mut seed_ids = Vec::new();
        for seed in seeds {
            seed_ids.push(
                *index
                    .by_id
                    .get(seed.as_str())
                    .context("unknown FunctionId")?,
            );
        }
        let walk = if matches!(direction, Direction::Callers) && !instances.is_empty() {
            let mut callers = vec![vec![]; instances.owners.len()];
            for (node, targets) in instances.callees.iter().enumerate() {
                for &target in targets {
                    callers[target as usize].push(node);
                }
            }
            let mut by_function = vec![vec![]; self.functions.len()];
            for (node, owner) in instances.owners.iter().enumerate() {
                if let Some(owner) = owner {
                    by_function[*owner as usize].push(node);
                }
            }
            self.walk(
                &seed_ids,
                &callers,
                |f| by_function[f].iter().copied(),
                |node| instances.owners[node].map(|f| f as usize),
                limits,
            )?
        } else {
            let adjacency = match direction {
                Direction::Callers => &index.callers,
                Direction::Callees => &index.callees,
            };
            self.walk(&seed_ids, adjacency, std::iter::once, Some, limits)?
        };
        let Walk {
            distance,
            graph_distance,
            truncated,
            edge_visits,
        } = walk;
        let mut nodes = Vec::new();
        // An unresolved caller outside the discovered component could still
        // call a seed through a function value. Do not present a closed reverse
        // graph as complete without a points-to proof.
        let callers = matches!(direction, Direction::Callers);
        let mut unknown = callers
            && self
                .functions
                .iter()
                .any(|f| !f.synthetic && f.unknown && !dead_template(f));
        for (i, f) in self.functions.iter().enumerate() {
            if distance[i] != usize::MAX {
                unknown |= f.unknown && !(callers && dead_template(f));
                if !f.synthetic {
                    nodes.push(ImpactNode {
                        identity: f.identity.clone(),
                        id: f.id.clone(),
                        level: (distance[i] + 1).min(3),
                        distance: distance[i],
                        graph_distance: graph_distance[i],
                        runtime_effects: f.runtime_effects,
                        unknown: f.unknown,
                        reason: f.unknown_reason(),
                    });
                }
            }
        }
        nodes.sort_by(|a, b| (a.distance, &a.id).cmp(&(b.distance, &b.id)));
        let external_dependencies = self.external_dependencies(&index, &distance);
        let dependency_boundary = self
            .semantic
            .interop
            .boundaries(nodes.iter().map(|n| &n.id));
        let rust_dependencies = if dependency_boundary.is_empty() {
            vec![]
        } else {
            self.semantic.interop.crates.clone()
        };
        Ok(Impact {
            rust_dependencies,
            dependency_boundary,
            external_dependencies,
            revision: self.revision.clone(),
            nodes,
            truncated,
            unknown,
            reason: unknown.then_some(if matches!(direction, Direction::Callers) {
                "Indirect or unresolved calls in the source graph may reach the selected functions; caller coverage is conservative without a points-to proof."
            } else {
                "A reachable callee or dynamic dispatch candidate has an incomplete effect summary; callee coverage and runtime capabilities are conservative."
            }),
            edge_visits,
        })
    }

    /// 0-1 BFS over `adjacency` from every node of each seed function.
    /// Nodes belong to at most one function (`owner`); a function's distance
    /// is that of its nearest node, and `max_nodes` counts functions. Each
    /// reachable edge is relaxed at most twice (cost 0 or 1).
    fn walk<N: Iterator<Item = usize>>(
        &self,
        seeds: &[usize],
        adjacency: &[Vec<usize>],
        nodes_of: impl Fn(usize) -> N,
        owner: impl Fn(usize) -> Option<usize>,
        limits: Limits,
    ) -> Result<Walk> {
        let mut node_distance = vec![usize::MAX; adjacency.len()];
        let mut node_graph_distance = vec![0; adjacency.len()];
        let mut distance = vec![usize::MAX; self.functions.len()];
        let mut graph_distance = vec![0; self.functions.len()];
        let mut queue = VecDeque::new();
        let mut count = 0;
        let mut truncated = false;
        for &seed in seeds {
            if distance[seed] == usize::MAX {
                ensure!(count < limits.max_nodes, "seed count exceeds node limit");
                distance[seed] = 0;
                count += 1;
                for node in nodes_of(seed) {
                    node_distance[node] = 0;
                    queue.push_back((node, 0));
                }
            }
        }
        ensure!(!seeds.is_empty(), "at least one seed is required");
        let mut edge_visits = 0;
        while let Some((i, queued_distance)) = queue.pop_front() {
            if queued_distance != node_distance[i] {
                continue;
            }
            for &j in &adjacency[i] {
                edge_visits += 1;
                let function = owner(j);
                // Synthetic dispatch unions do not introduce a display level.
                let cost = function.map_or(0, |f| usize::from(!self.functions[f].synthetic));
                let next = node_distance[i] + cost;
                if next >= node_distance[j] {
                    continue;
                }
                let new = function.is_some_and(|f| distance[f] == usize::MAX);
                if next > limits.max_depth || (new && count == limits.max_nodes) {
                    truncated = true;
                    continue;
                }
                count += usize::from(new);
                node_distance[j] = next;
                // An instance union is not a call: it adds no graph hop.
                node_graph_distance[j] = node_graph_distance[i] + usize::from(function.is_some());
                if let Some(f) = function
                    && next < distance[f]
                {
                    distance[f] = next;
                    graph_distance[f] = node_graph_distance[j];
                }
                if cost == 0 {
                    queue.push_front((j, next));
                } else {
                    queue.push_back((j, next));
                }
            }
        }
        Ok(Walk {
            distance,
            graph_distance,
            truncated,
            edge_visits,
        })
    }
}

struct Walk {
    distance: Vec<usize>,
    graph_distance: Vec<usize>,
    truncated: bool,
    edge_visits: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(i: usize) -> Function {
        Function {
            identity: None,
            body_id: None,
            body_location: None,
            rename_calls: vec![],
            rename_imports: vec![],
            id: format!("f{i}"),
            module: "m".into(),
            name: format!("f{i}"),
            locations: vec![],
            synthetic: false,
            fingerprint: String::new(),
            body_fingerprint: String::new(),
            callees: vec![],
            runtime_effects: 0,
            unknown: false,
            unresolved: vec![],
        }
    }
    #[test]
    fn increasing_graphs_visit_each_reachable_edge_once() {
        for shape in [
            "chain",
            "fanout",
            "cycle",
            "multiple_seeds",
            "duplicate_calls",
            "synthetic",
            "missing_source",
        ] {
            for n in [16, 64, 256, 1024, 4096] {
                let mut functions: Vec<_> = (0..n).map(node).collect();
                for (i, f) in functions.iter_mut().enumerate() {
                    match shape {
                        "fanout" | "duplicate_calls" if i > 0 => f.callees = vec!["f0".into()],
                        "cycle" => f.callees = vec![format!("f{}", (i + 1) % n)],
                        "chain" | "multiple_seeds" | "synthetic" | "missing_source" if i > 0 => {
                            f.callees = vec![format!("f{}", i - 1)]
                        }
                        _ => {}
                    }
                    if shape == "synthetic" && i % 2 == 1 {
                        f.synthetic = true;
                    }
                }
                functions[0].unknown = true;
                functions[0]
                    .unresolved
                    .push("indirect-or-unresolved".into());
                if shape == "missing_source" {
                    functions[0].synthetic = true;
                    functions[0].unresolved.clear();
                    functions[1].locations.push(Location {
                        path: "m".into(),
                        start: 1,
                        end: 2,
                    });
                }
                propagate(&mut functions);
                let snapshot = Snapshot {
                    edit_context: None,
                    semantic: Default::default(),
                    version: 1,
                    compiler: String::new(),
                    compatibility: String::new(),
                    workspace: String::new(),
                    revision: "r".into(),
                    sources: BTreeMap::from([("m".into(), "source".into())]),
                    functions,
                };
                let seeds = if shape == "multiple_seeds" {
                    (0..n).map(|i| format!("f{i}")).collect()
                } else {
                    vec!["f0".into()]
                };
                let result = snapshot
                    .impact(
                        &seeds,
                        Direction::Callers,
                        Limits {
                            max_nodes: n,
                            max_depth: n,
                        },
                        None,
                    )
                    .unwrap();
                let expected = if shape == "cycle" { n } else { n - 1 };
                assert_eq!(result.edge_visits, expected, "{shape} {n}");
                assert!(!result.truncated);
                assert!(result.unknown && result.reason.is_some());
                assert!(result.nodes.iter().all(|f| f.unknown && f.reason.is_some()));
                let mut snapshot = snapshot;
                snapshot.compiler = storage::compiler_stamp();
                snapshot.revision = snapshot.digest().unwrap();
                let revision = snapshot.revision.clone();
                let mut session = QuerySession::new(snapshot).unwrap();
                // Traverse in the other direction too. Reasons share the
                // existing effects BFS, including cycles and synthetic nodes.
                let root = if shape == "fanout" || shape == "duplicate_calls" {
                    1
                } else {
                    n - 1
                };
                for _ in 0..3 {
                    let before = session.effect_edge_visits;
                    let value = session.query(QueryRequest::Effects {
                        revision: revision.clone(),
                        function: format!("f{root}"),
                    });
                    assert_eq!(
                        value["result"]["unknown_causes"].as_array().unwrap().len(),
                        1,
                        "{shape} {n}: {value}"
                    );
                    assert_eq!(value["result"]["unknown_causes"][0]["function"], "f0");
                    if shape == "missing_source" {
                        assert_eq!(
                            value["result"]["unknown_causes"][0]["caller_location"]["start"],
                            1
                        );
                        assert_eq!(
                            value["result"]["unknown_causes"][0]["locations"],
                            serde_json::json!([])
                        );
                    }
                    assert_eq!(value["result"]["status"], "unknown");
                    assert!(value["result"]["reason"].is_string());
                    assert_eq!(
                        session.effect_edge_visits - before,
                        if shape == "fanout" || shape == "duplicate_calls" {
                            1
                        } else {
                            expected
                        }
                    );
                }
                assert_eq!(
                    result.nodes.len(),
                    if shape == "synthetic" {
                        n.div_ceil(2)
                    } else if shape == "missing_source" {
                        n - 1
                    } else {
                        n
                    }
                );
                println!(
                    "impact shape={shape} nodes={n} seeds={} edge_visits={} emitted={}",
                    seeds.len(),
                    result.edge_visits,
                    result.nodes.len()
                );
            }
        }
    }
    #[test]
    fn allocation_witness_selection_visits_each_edge_once() {
        use serde_json::json;
        for shape in ["chain", "fanout", "cycle", "duplicate_calls"] {
            for n in [16, 64, 256, 1024] {
                let mut functions: Vec<_> = (0..n).map(node).collect();
                let mut semantic = SemanticFacts::default();
                for (i, f) in functions.iter_mut().enumerate() {
                    f.runtime_effects = RuntimeEffects::MAY_ALLOCATE.bits();
                    f.callees = match shape {
                        "fanout" if i == 0 => (1..n).map(|j| format!("f{j}")).collect(),
                        "fanout" => vec![],
                        "cycle" => vec![format!("f{}", (i + 1) % n)],
                        "duplicate_calls" if i + 1 < n => vec![format!("f{}", i + 1); 8],
                        _ if i + 1 < n => vec![format!("f{}", i + 1)],
                        _ => vec![],
                    };
                    semantic.witnesses.insert(f.id.clone(), vec![json!({
                        "effect": RuntimeEffects::MAY_ALLOCATE.bits(),
                        "witness": {"kind":"runtime-capability", "cause": {
                            "operation": if i + 1 == n {"array-allocation"} else {"panic-payload-allocation"},
                            "location": {"path":"fixture.wi", "start":i, "end":i+1}
                        }}
                    })]);
                }
                let mut snapshot = Snapshot {
                    edit_context: None,
                    semantic,
                    version: 1,
                    compiler: storage::compiler_stamp(),
                    compatibility: String::new(),
                    workspace: String::new(),
                    revision: String::new(),
                    sources: BTreeMap::new(),
                    functions,
                };
                snapshot.revision = snapshot.digest().unwrap();
                let revision = snapshot.revision.clone();
                let mut session = QuerySession::new(snapshot).unwrap();
                // Every supplied edge is visited once, even if a handcrafted
                // snapshot repeats a callee (normal capture deduplicates it).
                let edges = match shape {
                    "cycle" => n,
                    "duplicate_calls" => 8 * (n - 1),
                    _ => n - 1,
                };
                for _ in 0..3 {
                    let before = session.effect_edge_visits;
                    let value = session.query(QueryRequest::Effects {
                        revision: revision.clone(),
                        function: "f0".into(),
                    });
                    assert_eq!(
                        value["result"]["effect_evidence"][0]["witness"]["cause"]["operation"],
                        "array-allocation",
                        "{shape}/{n}: {value}"
                    );
                    assert_eq!(session.effect_edge_visits - before, edges, "{shape}/{n}");
                }
                println!(
                    "allocation-witness shape={shape} nodes={n} edges_per_query={edges} queries=3"
                );
            }
        }
    }

    #[test]
    fn effects_propagate_through_deep_cycles_and_unknowns() {
        for n in [16, 64, 256, 1024, 4096, 8192] {
            let mut functions: Vec<_> = (0..n).map(node).collect();
            for (i, f) in functions.iter_mut().enumerate() {
                f.callees = vec![format!("f{}", (i + 1) % n)];
            }
            functions[n / 2].runtime_effects = RuntimeEffects::ALL.bits();
            functions[n / 2].unknown = true;
            let visits = propagate(&mut functions);
            assert!(
                functions
                    .iter()
                    .all(|f| f.runtime_effects == RuntimeEffects::ALL.bits() && f.unknown)
            );
            assert!(visits <= 2 * n);
            println!("effect-cycle nodes={n} edge_visits={visits}");
        }
    }
}
