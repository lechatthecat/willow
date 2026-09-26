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
}
#[derive(Debug, Serialize)]
pub struct Impact {
    pub external_dependencies: Vec<serde_json::Value>,
    pub revision: String,
    pub nodes: Vec<ImpactNode>,
    pub truncated: bool,
    pub unknown: bool,
    pub edge_visits: usize,
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
        let path = std::fs::canonicalize(path)?.to_string_lossy().into_owned();
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
        let adjacency = match direction {
            Direction::Callers => &index.callers,
            Direction::Callees => &index.callees,
        };
        let mut distance = vec![usize::MAX; self.functions.len()];
        let mut graph_distance = vec![0; self.functions.len()];
        let mut queue = VecDeque::new();
        let mut count = 0;
        let mut truncated = false;
        for seed in seeds {
            let &i = index
                .by_id
                .get(seed.as_str())
                .context("unknown FunctionId")?;
            if distance[i] == usize::MAX {
                ensure!(count < limits.max_nodes, "seed count exceeds node limit");
                distance[i] = 0;
                count += 1;
                queue.push_back((i, 0));
            }
        }
        ensure!(!queue.is_empty(), "at least one seed is required");
        let mut edge_visits = 0;
        while let Some((i, queued_distance)) = queue.pop_front() {
            if queued_distance != distance[i] {
                continue;
            }
            for &j in &adjacency[i] {
                edge_visits += 1;
                // Synthetic dispatch unions do not introduce a display level.
                let cost = usize::from(!self.functions[j].synthetic);
                let next = distance[i] + cost;
                if next >= distance[j] {
                    continue;
                }
                if next > limits.max_depth
                    || (distance[j] == usize::MAX && count == limits.max_nodes)
                {
                    truncated = true;
                    continue;
                }
                if distance[j] == usize::MAX {
                    count += 1;
                }
                distance[j] = next;
                graph_distance[j] = graph_distance[i] + 1;
                if cost == 0 {
                    queue.push_front((j, next));
                } else {
                    queue.push_back((j, next));
                }
            }
        }
        let mut nodes = Vec::new();
        // An unresolved caller outside the discovered component could still
        // call a seed through a function value. Do not present a closed reverse
        // graph as complete without a points-to proof.
        let mut unknown = matches!(direction, Direction::Callers)
            && self.functions.iter().any(|f| !f.synthetic && f.unknown);
        for (i, f) in self.functions.iter().enumerate() {
            if distance[i] != usize::MAX {
                unknown |= f.unknown;
                if !f.synthetic {
                    nodes.push(ImpactNode {
                        identity: f.identity.clone(),
                        id: f.id.clone(),
                        level: (distance[i] + 1).min(3),
                        distance: distance[i],
                        graph_distance: graph_distance[i],
                        runtime_effects: f.runtime_effects,
                        unknown: f.unknown,
                    });
                }
            }
        }
        nodes.sort_by(|a, b| (a.distance, &a.id).cmp(&(b.distance, &b.id)));
        let external_dependencies = self.external_dependencies(&index, &distance);
        Ok(Impact {
            external_dependencies,
            revision: self.revision.clone(),
            nodes,
            truncated,
            unknown,
            edge_visits,
        })
    }
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
        ] {
            for n in [16, 64, 256, 1024, 4096] {
                let mut functions: Vec<_> = (0..n).map(node).collect();
                for (i, f) in functions.iter_mut().enumerate() {
                    match shape {
                        "fanout" | "duplicate_calls" if i > 0 => f.callees = vec!["f0".into()],
                        "cycle" => f.callees = vec![format!("f{}", (i + 1) % n)],
                        "chain" | "multiple_seeds" | "synthetic" if i > 0 => {
                            f.callees = vec![format!("f{}", i - 1)]
                        }
                        _ => {}
                    }
                    if shape == "synthetic" && i % 2 == 1 {
                        f.synthetic = true;
                    }
                }
                let snapshot = Snapshot {
                    semantic: Default::default(),
                    version: 1,
                    compiler: String::new(),
                    compatibility: String::new(),
                    workspace: String::new(),
                    revision: "r".into(),
                    sources: BTreeMap::new(),
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
                assert_eq!(
                    result.nodes.len(),
                    if shape == "synthetic" {
                        n.div_ceil(2)
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
