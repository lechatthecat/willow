use super::*;
use crate::package::PackageId;

fn label(graph: &PackageGraph, id: PackageId) -> String {
    let p = &graph.packages[id.0 as usize];
    format!("{} v{}", p.identity.name, p.identity.version)
}

/// Stream the tree or all root-to-target paths. Shared tree nodes expand once;
/// why uses a reverse reachability index before enumerating relevant paths.
pub fn display_dependencies(
    graph: &PackageGraph,
    why: Option<&str>,
    out: &mut impl Write,
) -> Result<()> {
    display(graph, why, out).map(|_| ())
}

fn display(graph: &PackageGraph, why: Option<&str>, out: &mut impl Write) -> Result<usize> {
    let n = graph.packages.len();
    let mut visits = 0;
    let mut targets = vec![false; n];
    let mut relevant = vec![true; n];
    if let Some(query) = why {
        let direct = graph.packages[graph.root.0 as usize]
            .dependencies
            .iter()
            .find(|edge| edge.alias == query);
        if let Some(edge) = direct {
            targets[edge.package.0 as usize] = true;
        } else {
            for p in &graph.packages {
                targets[p.id.0 as usize] = p.identity.name == query;
            }
        }
        ensure!(targets.iter().any(|v| *v), "dependency `{query}` not found");
        let mut incoming = vec![Vec::new(); n];
        for p in &graph.packages {
            for edge in &p.dependencies {
                visits += 1;
                incoming[edge.package.0 as usize].push(p.id);
            }
        }
        relevant.clone_from(&targets);
        let mut pending: Vec<_> = graph
            .packages
            .iter()
            .filter(|p| targets[p.id.0 as usize])
            .map(|p| p.id)
            .collect();
        while let Some(id) = pending.pop() {
            for parent in &incoming[id.0 as usize] {
                visits += 1;
                if !relevant[parent.0 as usize] {
                    relevant[parent.0 as usize] = true;
                    pending.push(*parent);
                }
            }
        }
    }
    // Filter once; a relevant node with many irrelevant children may occur in
    // exponentially many paths. Filtering during enumeration would multiply scans.
    let edges: Vec<Vec<_>> = graph
        .packages
        .iter()
        .map(|p| {
            p.dependencies
                .iter()
                .filter(|edge| {
                    visits += 1;
                    relevant[edge.package.0 as usize]
                })
                .collect()
        })
        .collect();
    let mut expanded = vec![false; n];
    let mut stack = vec![(graph.root, 0usize)];
    let mut path = Vec::new();
    expanded[graph.root.0 as usize] = true;
    if why.is_none() || targets[graph.root.0 as usize] {
        writeln!(out, "{}", label(graph, graph.root))?;
    }
    while let Some((id, next)) = stack.last_mut() {
        let Some(edge) = edges[id.0 as usize].get(*next) else {
            stack.pop();
            path.pop();
            continue;
        };
        *next += 1;
        visits += 1;
        let target = edge.package;
        path.push(*edge);
        if why.is_some() {
            if targets[target.0 as usize] {
                write!(out, "{}", label(graph, graph.root))?;
                for edge in &path {
                    write!(out, " -> {}: {}", edge.alias, label(graph, edge.package))?;
                }
                writeln!(out)?;
            }
        } else {
            writeln!(
                out,
                "{}{}: {}{}",
                "  ".repeat(path.len()),
                edge.alias,
                label(graph, target),
                if expanded[target.0 as usize] {
                    " (*)"
                } else {
                    ""
                }
            )?;
        }
        if why.is_some() || !expanded[target.0 as usize] {
            expanded[target.0 as usize] = true;
            stack.push((target, 0));
        } else {
            path.pop();
        }
    }
    Ok(visits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{
        PackageIdentity, PackageSourceIdentity, ResolvedDependency, ResolvedPackage,
    };
    fn graph(n: usize) -> PackageGraph {
        PackageGraph {
            root: PackageId(0),
            stats: Default::default(),
            packages: (0..n)
                .map(|i| ResolvedPackage {
                    id: PackageId(i as u32),
                    checksum: None,
                    root: Default::default(),
                    identity: PackageIdentity {
                        name: format!("p{i}"),
                        version: "1.0.0".into(),
                        revision: None,
                        source: PackageSourceIdentity::Path {
                            path: Default::default(),
                        },
                    },
                    dependencies: vec![],
                })
                .collect(),
        }
    }
    fn edge(g: &mut PackageGraph, from: usize, to: usize) {
        g.packages[from].dependencies.push(ResolvedDependency {
            alias: format!("a{to}"),
            package: PackageId(to as u32),
            selector: None,
        });
    }
    #[test]
    fn why_enumerates_diamond_and_tree_expands_shared_nodes_once() {
        let mut g = graph(4);
        for (a, b) in [(0, 1), (0, 2), (1, 3), (2, 3)] {
            edge(&mut g, a, b);
        }
        let mut output = Vec::new();
        display_dependencies(&g, Some("p3"), &mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "p0 v1.0.0 -> a1: p1 v1.0.0 -> a3: p3 v1.0.0\np0 v1.0.0 -> a2: p2 v1.0.0 -> a3: p3 v1.0.0\n"
        );
        let mut output = Vec::new();
        display_dependencies(&g, None, &mut output).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .ends_with("    a3: p3 v1.0.0 (*)\n")
        );
    }
    #[test]
    fn deterministic_visits_scale_with_edges_and_relevant_paths() {
        for n in [32, 128, 512, 2048] {
            let mut chain = graph(n);
            for i in 1..n {
                edge(&mut chain, i - 1, i);
            }
            let count =
                display(&chain, Some(&format!("p{}", n - 1)), &mut std::io::sink()).unwrap();
            assert_eq!(count, 4 * (n - 1));
            // Irrelevant fan-out is scanned once, not for every incoming path.
            let mut fan = graph(2 * n + 3);
            for i in 1..=n {
                edge(&mut fan, 0, i);
                edge(&mut fan, i, n + 1);
            }
            for i in n + 2..2 * n + 3 {
                edge(&mut fan, n + 1, i);
            }
            let count =
                display(&fan, Some(&format!("p{}", 2 * n + 2)), &mut std::io::sink()).unwrap();
            assert_eq!(count, 11 * n + 3);
            eprintln!("n={n} chain_visits={} fan_visits={count}", 4 * (n - 1));
        }
    }
}
