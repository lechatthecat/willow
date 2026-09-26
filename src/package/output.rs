//! Versioned package output. Numeric session-local IDs never cross this boundary.
use super::{PackageGraph, PackageIdentity};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Serialize)]
pub struct Metadata<'a> {
    pub schema: u32,
    pub ok: bool,
    pub kind: &'static str,
    pub root_package: &'a PackageIdentity,
    pub packages: Vec<MetadataPackage<'a>>,
}
#[derive(Debug, Serialize)]
pub struct MetadataPackage<'a> {
    pub id: &'a PackageIdentity,
    pub path: &'a std::path::Path,
    pub direct: bool,
    pub aliases: Vec<&'a str>,
    pub dependencies: Vec<MetadataDependency<'a>>,
}
#[derive(Debug, Serialize)]
pub struct MetadataDependency<'a> {
    pub alias: &'a str,
    pub package: &'a PackageIdentity,
}
impl PackageGraph {
    pub fn metadata(&self) -> Metadata<'_> {
        self.metadata_selected(None)
    }

    fn metadata_selected(&self, relevant: Option<&[bool]>) -> Metadata<'_> {
        let included = |id: super::PackageId| relevant.is_none_or(|mask| mask[id.0 as usize]);
        let mut aliases = vec![Vec::new(); self.packages.len()];
        for edge in &self.packages[self.root.0 as usize].dependencies {
            aliases[edge.package.0 as usize].push(edge.alias.as_str());
        }
        let mut packages: Vec<_> = self
            .packages
            .iter()
            .filter(|p| included(p.id))
            .map(|p| {
                #[cfg(test)]
                PACKAGE_VISITS.with(|n| n.set(n.get() + 1));
                let mut aliases = std::mem::take(&mut aliases[p.id.0 as usize]);
                aliases.sort_unstable();
                let mut dependencies: Vec<_> = p
                    .dependencies
                    .iter()
                    .filter(|d| included(d.package))
                    .map(|d| {
                        #[cfg(test)]
                        EDGE_VISITS.with(|n| n.set(n.get() + 1));
                        MetadataDependency {
                            alias: &d.alias,
                            package: &self.packages[d.package.0 as usize].identity,
                        }
                    })
                    .collect();
                dependencies.sort_unstable_by_key(|d| d.alias);
                MetadataPackage {
                    id: &p.identity,
                    path: &p.root,
                    direct: !aliases.is_empty(),
                    aliases,
                    dependencies,
                }
            })
            .collect();
        packages.sort_unstable_by_key(|p| p.id);
        Metadata {
            schema: 1,
            ok: true,
            kind: "package.metadata",
            root_package: &self.packages[self.root.0 as usize].identity,
            packages,
        }
    }

    /// Compact reverse-reachable subgraph represents every explanation without
    /// enumerating exponentially many root-to-target paths in a shared DAG.
    pub fn dependency_metadata(&self, query: Option<&str>) -> anyhow::Result<Metadata<'_>> {
        let mut metadata = if let Some(query) = query {
            let alias = self.packages[self.root.0 as usize]
                .dependencies
                .iter()
                .find(|d| d.alias == query);
            let mut relevant = vec![false; self.packages.len()];
            let mut pending = Vec::new();
            for p in &self.packages {
                if alias.map_or(p.identity.name == query, |d| d.package == p.id) {
                    relevant[p.id.0 as usize] = true;
                    pending.push(p.id);
                }
            }
            anyhow::ensure!(!pending.is_empty(), "dependency `{query}` not found");
            let mut incoming = vec![Vec::new(); self.packages.len()];
            for p in &self.packages {
                for d in &p.dependencies {
                    #[cfg(test)]
                    EDGE_VISITS.with(|n| n.set(n.get() + 1));
                    incoming[d.package.0 as usize].push(p.id);
                }
            }
            while let Some(id) = pending.pop() {
                for parent in &incoming[id.0 as usize] {
                    #[cfg(test)]
                    EDGE_VISITS.with(|n| n.set(n.get() + 1));
                    if !relevant[parent.0 as usize] {
                        relevant[parent.0 as usize] = true;
                        pending.push(*parent);
                    }
                }
            }
            self.metadata_selected(Some(&relevant))
        } else {
            self.metadata()
        };
        metadata.kind = if query.is_some() {
            "package.deps.why"
        } else {
            "package.deps.tree"
        };
        Ok(metadata)
    }
}

#[derive(Debug, Serialize)]
pub struct PackageDelta {
    pub before: Option<PackageIdentity>,
    pub after: Option<PackageIdentity>,
}

pub(super) fn delta(
    before: Vec<PackageIdentity>,
    graph: &PackageGraph,
) -> (Vec<PackageIdentity>, Vec<PackageDelta>) {
    let mut old: HashMap<_, _> = before.into_iter().map(|p| (p.source.clone(), p)).collect();
    let mut added = Vec::new();
    let mut changes = Vec::new();
    for p in &graph.packages {
        if p.id == graph.root {
            continue;
        }
        #[cfg(test)]
        DELTA_PROBES.with(|n| n.set(n.get() + 1));
        let previous = old.remove(&p.identity.source);
        if previous.as_ref() != Some(&p.identity) {
            if previous.is_none() {
                added.push(p.identity.clone());
            }
            changes.push(PackageDelta {
                before: previous,
                after: Some(p.identity.clone()),
            });
        }
    }
    changes.extend(old.into_values().map(|p| PackageDelta {
        before: Some(p),
        after: None,
    }));
    changes.sort_unstable_by(|a, b| {
        a.after
            .as_ref()
            .or(a.before.as_ref())
            .cmp(&b.after.as_ref().or(b.before.as_ref()))
    });
    added.sort_unstable();
    (added, changes)
}

#[cfg(test)]
thread_local! {
    static PACKAGE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static EDGE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static DELTA_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{PackageId, PackageSourceIdentity, ResolvedDependency, ResolvedPackage};
    fn graph(n: usize, fanout: bool) -> PackageGraph {
        PackageGraph {
            root: PackageId(0),
            stats: Default::default(),
            packages: (0..n)
                .map(|i| {
                    let targets: Vec<_> = if i + 1 == n {
                        vec![]
                    } else if fanout && i == 0 {
                        (1..n).collect()
                    } else {
                        vec![i + 1]
                    };
                    ResolvedPackage {
                        id: PackageId(i as u32),
                        checksum: None,
                        root: format!("/p{i}").into(),
                        identity: PackageIdentity {
                            name: format!("p{i}"),
                            version: "1.0.0".into(),
                            revision: None,
                            source: PackageSourceIdentity::Path {
                                path: format!("/p{i}").into(),
                            },
                        },
                        dependencies: targets
                            .into_iter()
                            .map(|j| ResolvedDependency {
                                alias: format!("a{j}"),
                                selector: None,
                                package: PackageId(j as u32),
                            })
                            .collect(),
                    }
                })
                .collect(),
        }
    }
    #[test]
    fn metadata_and_why_output_scale_with_graph_not_number_of_paths() {
        for n in [32, 128, 512, 2048] {
            for shape in ["chain", "fanout", "layered"] {
                let mut g = graph(n, shape == "fanout");
                if shape == "layered" {
                    for i in 0..n - 2 {
                        g.packages[i].dependencies.push(ResolvedDependency {
                            alias: "skip".into(),
                            selector: None,
                            package: PackageId((i + 2) as u32),
                        });
                    }
                }
                let expected_edges = if shape == "chain" { n - 1 } else { 2 * n - 3 };
                PACKAGE_VISITS.with(|n| n.set(0));
                EDGE_VISITS.with(|n| n.set(0));
                DELTA_PROBES.with(|n| n.set(0));
                let m = g.dependency_metadata(Some(&format!("p{}", n - 1))).unwrap();
                assert_eq!(m.packages.len(), n);
                let edges: usize = m.packages.iter().map(|p| p.dependencies.len()).sum();
                assert_eq!(edges, expected_edges);
                let value = serde_json::to_value(&m).unwrap();
                assert!(value["root_package"].is_object());
                for p in value["packages"].as_array().unwrap() {
                    assert!(p["id"].is_object());
                    for d in p["dependencies"].as_array().unwrap() {
                        assert!(d["package"].is_object());
                    }
                }
                let previous = g
                    .packages
                    .iter()
                    .skip(1)
                    .map(|p| p.identity.clone())
                    .collect();
                let (added, changed) = delta(previous, &g);
                assert!(added.is_empty() && changed.is_empty());
                let package_visits = PACKAGE_VISITS.with(|n| n.get());
                let edge_visits = EDGE_VISITS.with(|n| n.get());
                let delta_probes = DELTA_PROBES.with(|n| n.get());
                assert_eq!(package_visits, n);
                assert_eq!(edge_visits, 3 * expected_edges);
                assert_eq!(delta_probes, n - 1);
                eprintln!(
                    "package_output n={n} shape={shape} packages={} edges={edges} package_visits={package_visits} projection_reverse_edge_visits={edge_visits} delta_probes={delta_probes}",
                    m.packages.len()
                );
            }
        }
    }
    #[test]
    fn sorting_and_why_alias_precedence_are_deterministic() {
        let mut g = graph(5, true);
        let before = serde_json::to_value(g.metadata()).unwrap();
        g.packages[0].dependencies.reverse();
        assert_eq!(before, serde_json::to_value(g.metadata()).unwrap());
        let m = g.dependency_metadata(Some("a1")).unwrap();
        assert_eq!(m.packages.len(), 2);
        assert_eq!(
            m.packages
                .iter()
                .map(|p| p.dependencies.len())
                .sum::<usize>(),
            1
        );
        assert!(g.dependency_metadata(Some("missing")).is_err());
    }
}
