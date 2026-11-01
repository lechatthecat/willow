//! Shared caller reachability for every changed symbol.
use super::*;

pub(super) fn assign(changes: &mut [Value], before: &Baseline, after: &Baseline) -> Result<usize> {
    let seeds: Vec<_> = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c["match"] != "ambiguous")
        .filter_map(|(i, c)| c["symbol"].as_str().map(|s| (i, s.to_owned())))
        .collect();
    if seeds.is_empty() {
        return Ok(0);
    }
    let mut reached: BTreeSet<String> = seeds.iter().map(|(_, s)| s.clone()).collect();
    let mut pending: VecDeque<_> = reached.iter().cloned().collect();
    let mut graph = BTreeMap::new();
    while let Some(node) = pending.pop_front() {
        let edges: BTreeSet<_> = before
            .callers
            .get(&node)
            .into_iter()
            .flatten()
            .chain(after.callers.get(&node).into_iter().flatten())
            .cloned()
            .collect();
        for caller in &edges {
            if reached.insert(caller.clone()) {
                pending.push_back(caller.clone());
            }
        }
        graph.insert(node, edges);
    }
    let names: Vec<_> = reached.into_iter().collect();
    let ids: BTreeMap<_, _> = names
        .iter()
        .enumerate()
        .map(|(i, s)| (s.as_str(), i))
        .collect();
    let edges: Vec<Vec<usize>> = names
        .iter()
        .map(|s| graph[s].iter().map(|c| ids[c.as_str()]).collect())
        .collect();
    let components = crate::semantic::effects::strongly_connected_components(&edges);
    let count = components.iter().max().map_or(0, |c| c + 1);
    let mut dag = vec![BTreeSet::new(); count];
    let mut indegree = vec![0; count];
    for (node, edges) in edges.iter().enumerate() {
        let from = components[node];
        for &target in edges {
            let to = components[target];
            if from != to && dag[from].insert(to) {
                indegree[to] += 1;
            }
        }
    }
    let words = seeds.len().div_ceil(64);
    ensure!(
        count
            .checked_mul(words)
            .is_some_and(|n| n <= 8 * 1024 * 1024),
        "caller evidence exceeds 64 MiB"
    );
    let mut bits = vec![vec![0u64; words]; count];
    for (bit, (_, seed)) in seeds.iter().enumerate() {
        bits[components[ids[seed.as_str()]]][bit / 64] |= 1 << (bit % 64);
    }
    let mut ready: VecDeque<_> = indegree
        .iter()
        .enumerate()
        .filter(|(_, n)| **n == 0)
        .map(|(i, _)| i)
        .collect();
    let mut word_unions = 0;
    while let Some(from) = ready.pop_front() {
        let source = bits[from].clone();
        for &to in &dag[from] {
            for (target, source) in bits[to].iter_mut().zip(&source) {
                *target |= source;
                word_unions += 1;
            }
            indegree[to] -= 1;
            if indegree[to] == 0 {
                ready.push_back(to);
            }
        }
    }
    let mut emitted = 0usize;
    for (node, name) in names.iter().enumerate() {
        for (word, &value) in bits[components[node]].iter().enumerate() {
            let mut value = value;
            while value != 0 {
                let bit = word * 64 + value.trailing_zeros() as usize;
                let (change, seed) = &seeds[bit];
                if seed != name {
                    emitted += 1;
                    ensure!(emitted <= 1_000_000, "caller evidence exceeds output limit");
                    changes[*change]["impacted_callers"]
                        .as_array_mut()
                        .context("caller evidence array")?
                        .push(json!(name));
                }
                value &= value - 1;
            }
        }
    }
    Ok(word_unions)
}
