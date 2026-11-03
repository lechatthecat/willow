use super::*;

pub(super) struct Graph<'a> {
    pub names: Vec<&'a str>,
    pub edges: Vec<Vec<usize>>,
    reverse: Vec<Vec<usize>>,
    parents: Vec<Option<usize>>,
    roots: Vec<usize>,
}
impl<'a> Graph<'a> {
    pub fn new(manifest: &'a NativeManifest, work: &mut Work) -> Result<Self> {
        let names: Vec<_> = manifest.libraries.keys().map(String::as_str).collect();
        let index: HashMap<_, _> = names.iter().enumerate().map(|(i, n)| (*n, i)).collect();
        let mut edges = vec![Vec::new(); names.len()];
        let mut reverse = edges.clone();
        for (i, name) in names.iter().enumerate() {
            work.vertices += 1;
            for dependency in &manifest.libraries[*name].dependencies {
                let &j = index.get(dependency.as_str()).with_context(|| {
                    format!("native_dependency_missing: {name} -> {dependency}")
                })?;
                edges[i].push(j);
                reverse[j].push(i);
                work.edges += 1;
            }
        }
        let roots = manifest
            .roots
            .iter()
            .map(|name| {
                index
                    .get(name.as_str())
                    .copied()
                    .with_context(|| format!("native_root_missing: {name}"))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut parents = vec![None; names.len()];
        let mut visited = vec![false; names.len()];
        let mut queue = std::collections::VecDeque::new();
        for &i in &roots {
            if !visited[i] {
                visited[i] = true;
                queue.push_back(i);
            }
        }
        while let Some(i) = queue.pop_front() {
            work.vertices += 1;
            for &j in &edges[i] {
                work.edges += 1;
                if !visited[j] {
                    visited[j] = true;
                    parents[j] = Some(i);
                    queue.push_back(j);
                }
            }
        }
        ensure!(visited.iter().all(|v| *v), "native_unreachable_library");
        Ok(Self {
            names,
            edges,
            reverse,
            parents,
            roots,
        })
    }
    pub fn path(&self, mut i: usize) -> String {
        let mut parts = vec![self.names[i]];
        while let Some(parent) = self.parents[i] {
            parts.push(self.names[parent]);
            i = parent;
        }
        parts.reverse();
        parts.join(" -> ")
    }
    /// Iterative Kosaraju, including deep graphs without native-stack recursion.
    /// Reverse root traversal makes independent roots retain declaration order.
    pub fn components(&self, work: &mut Work) -> Vec<Vec<usize>> {
        let mut seen = vec![false; self.names.len()];
        let mut finish = Vec::with_capacity(seen.len());
        let mut stack = Vec::new();
        for &root in self.roots.iter().rev() {
            if seen[root] {
                continue;
            }
            seen[root] = true;
            work.vertices += 1;
            stack.push((root, self.edges[root].len()));
            while let Some((i, cursor)) = stack.last_mut() {
                if *cursor == 0 {
                    finish.push(*i);
                    stack.pop();
                    continue;
                }
                *cursor -= 1;
                let next = self.edges[*i][*cursor];
                work.edges += 1;
                if !seen[next] {
                    seen[next] = true;
                    work.vertices += 1;
                    stack.push((next, self.edges[next].len()));
                }
            }
        }
        seen.fill(false);
        let mut components = Vec::new();
        for i in finish.into_iter().rev() {
            if seen[i] {
                continue;
            }
            let mut component = Vec::new();
            let mut pending = vec![i];
            seen[i] = true;
            while let Some(j) = pending.pop() {
                work.vertices += 1;
                component.push(j);
                for &k in &self.reverse[j] {
                    work.edges += 1;
                    if !seen[k] {
                        seen[k] = true;
                        pending.push(k);
                    }
                }
            }
            components.push(component);
        }
        components
    }
}
