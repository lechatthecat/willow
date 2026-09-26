//! Package projections of existing compiler evidence. No dependency resolution
//! or build scheduling takes place in semantic queries.
use super::*;
use crate::package::PackageIdentity;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SymbolIdentity {
    pub package: PackageIdentity,
    pub module: String,
    pub symbol: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleIdentity {
    pub package: PackageIdentity,
    pub module: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModuleEvidence {
    pub path: String,
    pub identity: ModuleIdentity,
    /// Snapshot module indices, never package IDs.
    pub dependencies: Vec<usize>,
}

/// Accepts the JSON produced by package add/update --format json, including
/// dry-run reports. Unknown report metadata is intentionally ignored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateDelta {
    pub schema: u32,
    pub ok: bool,
    pub kind: String,
    #[serde(default)]
    pub packages_added: Vec<PackageIdentity>,
    #[serde(default)]
    pub changes: Vec<DirectChange>,
    #[serde(default)]
    pub transitive_changes: Vec<Change>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectChange {
    pub resolved_before: Option<PackageIdentity>,
    pub resolved_after: Option<PackageIdentity>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Change {
    pub before: Option<PackageIdentity>,
    pub after: Option<PackageIdentity>,
}

pub(super) fn attach(
    frontend: &crate::Frontend,
    paths: &HashMap<UnitId, String>,
    captured: &HashMap<UnitId, CapturedUnit>,
    functions: &mut [Function],
    semantic: &mut SemanticFacts,
) -> Result<()> {
    let Some(graph) = frontend.db.package_graph() else {
        return Ok(());
    };
    let root = graph.get(graph.root).context("root package")?;
    let entry = Path::new(&paths[&UnitId::ENTRY]);
    let module = frontend
        .program
        .module
        .as_ref()
        .map(|m| m.path.clone())
        .unwrap_or_else(|| {
            entry
                .strip_prefix(root.source_root())
                .unwrap_or(entry)
                .with_extension("")
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("::")
        });
    let mut units = HashMap::from([(UnitId::ENTRY, 0)]);
    semantic.modules.push(ModuleEvidence {
        path: paths[&UnitId::ENTRY].clone(),
        identity: ModuleIdentity {
            package: root.identity.clone(),
            module,
        },
        dependencies: vec![],
    });
    for file in &frontend.module_graph.files {
        let Some(origin) = file.symbol_module else {
            continue;
        };
        units.insert(file.id, semantic.modules.len());
        semantic.modules.push(ModuleEvidence {
            path: paths[&file.id].clone(),
            identity: ModuleIdentity {
                package: origin.package().clone(),
                module: origin.path().0.clone(),
            },
            dependencies: vec![],
        });
    }
    // Resolve import spellings with the same consumer-aware index as checking.
    for (unit, program) in std::iter::once((UnitId::ENTRY, &frontend.program)).chain(
        frontend
            .module_graph
            .files
            .iter()
            .map(|f| (f.id, &f.program)),
    ) {
        let Some(&index) = units.get(&unit) else {
            continue;
        };
        let mut seen = HashSet::new();
        for import in &program.imports {
            let target = frontend
                .db
                .dependencies()
                .unit_for_path(unit, &import.path)
                .or_else(|| {
                    import
                        .path
                        .rsplit_once("::")
                        .and_then(|(m, _)| frontend.db.dependencies().unit_for_path(unit, m))
                });
            if let Some(target) = target.and_then(|u| units.get(&u)).copied()
                && seen.insert(target)
            {
                semantic.modules[index].dependencies.push(target);
            }
        }
    }
    let by_path: HashMap<_, _> = semantic
        .modules
        .iter()
        .map(|m| (m.path.as_str(), &m.identity))
        .collect();
    let identity = |path: &str, symbol: &str| {
        by_path.get(path).map(|m| SymbolIdentity {
            package: m.package.clone(),
            module: m.module.clone(),
            symbol: symbol.to_owned(),
        })
    };
    for function in functions.iter_mut() {
        function.identity = identity(&function.module, &function.name);
    }
    let by_id: HashMap<_, _> = functions
        .iter()
        .map(|f| (f.id.as_str(), &f.identity))
        .collect();
    let owners = OwnerIndex::new(captured.values().flat_map(|c| c.symbol_owners.iter()));
    let units_by_path: HashMap<_, _> = paths.iter().map(|(u, p)| (p.as_str(), *u)).collect();
    for symbol in &mut semantic.symbols {
        symbol.identity = by_id
            .get(symbol.id.as_str())
            .and_then(|i| (*i).clone())
            .or_else(|| {
                symbol.location.as_ref().and_then(|l| {
                    let owner = units_by_path
                        .get(l.path.as_str())
                        .and_then(|u| owners.at(u.file_id(), l.start));
                    let name = match symbol.kind.as_str() {
                        // A source anchor distinguishes nested/sibling scopes and
                        // repeated bindings without encoding a whole scope chain.
                        "parameter" | "binding" => format!(
                            "{}::{}:{}@{}",
                            owner.unwrap_or("<module>"),
                            symbol.kind,
                            symbol.name,
                            l.start
                        ),
                        "field" | "static-field" | "variant" | "type-parameter" => format!(
                            "{}::{}:{}",
                            owner.unwrap_or("<module>"),
                            symbol.kind,
                            symbol.name
                        ),
                        "method" | "constructor" => owner.unwrap_or(&symbol.name).to_owned(),
                        _ => symbol.name.clone(),
                    };
                    identity(&l.path, &name)
                })
            });
    }
    let by_id: HashMap<_, _> = semantic
        .symbols
        .iter()
        .map(|s| (s.id.as_str(), &s.identity))
        .collect();
    for reference in &mut semantic.references {
        reference.identity = by_id
            .get(reference.target.as_str())
            .and_then(|i| (*i).clone());
    }
    Ok(())
}

/// Disjoint source segments make innermost-owner lookup logarithmic even when
/// scopes are deeply nested. Names are stored once, not copied into each segment.
struct OwnerIndex<'a> {
    names: Vec<&'a str>,
    segments: HashMap<crate::diagnostics::FileId, Vec<(usize, usize, usize)>>,
    #[cfg(test)]
    event_updates: usize,
    #[cfg(test)]
    comparisons: std::cell::Cell<usize>,
}
impl<'a> OwnerIndex<'a> {
    fn new(owners: impl Iterator<Item = &'a (Span, String)>) -> Self {
        let mut index = Self {
            names: Vec::new(),
            segments: HashMap::new(),
            #[cfg(test)]
            event_updates: 0,
            #[cfg(test)]
            comparisons: std::cell::Cell::new(0),
        };
        let mut events: HashMap<_, Vec<_>> = HashMap::new();
        for (span, name) in owners {
            if span.start >= span.end {
                continue;
            }
            let id = index.names.len();
            index.names.push(name.as_str());
            let file = events.entry(span.file_id).or_default();
            let key = (span.end - span.start, name.as_str(), id);
            file.push((span.start, true, key));
            file.push((span.end, false, key));
        }
        for (file, mut events) in events {
            events.sort_unstable();
            let mut active = BTreeSet::new();
            let mut segments = Vec::new();
            let mut p = 0;
            while p < events.len() {
                let at = events[p].0;
                while p < events.len() && events[p].0 == at {
                    let (_, add, key) = events[p];
                    if add {
                        active.insert(key);
                    } else {
                        active.remove(&key);
                    }
                    p += 1;
                    #[cfg(test)]
                    {
                        index.event_updates += 1;
                    }
                }
                if p < events.len()
                    && let Some(&(_, _, id)) = active.first()
                {
                    segments.push((at, events[p].0, id));
                }
            }
            index.segments.insert(file, segments);
        }
        index
    }

    fn at(&self, file: crate::diagnostics::FileId, position: usize) -> Option<&str> {
        let segments = self.segments.get(&file)?;
        let p = segments.partition_point(|&(start, _, _)| {
            #[cfg(test)]
            self.comparisons.set(self.comparisons.get() + 1);
            start <= position
        });
        let &(_, end, owner) = segments.get(p.checked_sub(1)?)?;
        (position < end).then_some(self.names[owner])
    }
}

impl Snapshot {
    pub(super) fn external_dependencies(
        &self,
        index: &graph::Index<'_>,
        distances: &[usize],
    ) -> Vec<Value> {
        let mut edges = Vec::new();
        for (i, f) in self.functions.iter().enumerate() {
            if distances[i] == usize::MAX {
                continue;
            }
            for &j in &index.callees[i] {
                let target = &self.functions[j];
                if let (Some(from), Some(to)) = (&f.identity, &target.identity)
                    && from.package != to.package
                {
                    edges.push(json!({"from":from,"target":to,"relation":"callee"}));
                }
            }
        }
        edges
    }

    pub fn affected(&self, delta: &UpdateDelta, tests: &[String]) -> Value {
        PackageIndex::new(self).affected(self, delta, tests)
    }
}

/// Built once per QuerySession; queries visit only changed-package modules and
/// their reverse closure, rather than scanning the snapshot on every request.
pub(super) struct PackageIndex {
    by_package: HashMap<PackageIdentity, Vec<usize>>,
    reverse: Vec<Vec<usize>>,
    symbols: Vec<Vec<usize>>,
    functions: HashMap<String, (usize, usize)>,
}
impl PackageIndex {
    pub fn new(snapshot: &Snapshot) -> Self {
        let modules = &snapshot.semantic.modules;
        let mut by_package: HashMap<_, Vec<_>> = HashMap::new();
        let mut reverse = vec![vec![]; modules.len()];
        let mut symbols = vec![vec![]; modules.len()];
        let by_path: HashMap<_, _> = modules
            .iter()
            .enumerate()
            .map(|(i, m)| (m.path.as_str(), i))
            .collect();
        for (i, module) in modules.iter().enumerate() {
            by_package
                .entry(module.identity.package.clone())
                .or_default()
                .push(i);
            for &dependency in &module.dependencies {
                if let Some(callers) = reverse.get_mut(dependency) {
                    callers.push(i);
                }
            }
        }
        for (i, symbol) in snapshot.semantic.symbols.iter().enumerate() {
            if let Some(module) = symbol
                .location
                .as_ref()
                .and_then(|l| by_path.get(l.path.as_str()))
            {
                symbols[*module].push(i);
            }
        }
        let functions = snapshot
            .functions
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                by_path
                    .get(f.module.as_str())
                    .map(|&m| (f.id.clone(), (m, i)))
            })
            .collect();
        Self {
            by_package,
            reverse,
            symbols,
            functions,
        }
    }

    pub fn affected(&self, snapshot: &Snapshot, delta: &UpdateDelta, tests: &[String]) -> Value {
        if delta.schema != 1 || !delta.ok || delta.kind != "package.mutation" {
            return json!({"status":"invalid-delta"});
        }
        let mut changed: HashSet<&PackageIdentity> = delta.packages_added.iter().collect();
        for (before, after) in delta
            .changes
            .iter()
            .map(|c| (&c.resolved_before, &c.resolved_after))
            .chain(
                delta
                    .transitive_changes
                    .iter()
                    .map(|c| (&c.before, &c.after)),
            )
        {
            if before != after {
                changed.extend(before.iter());
                changed.extend(after.iter());
            }
        }
        let mut affected = HashSet::new();
        let mut pending = VecDeque::new();
        let mut unmatched = BTreeSet::new();
        for package in changed {
            if let Some(modules) = self.by_package.get(package) {
                for &i in modules {
                    if affected.insert(i) {
                        pending.push_back(i);
                    }
                }
            } else {
                unmatched.insert(package);
            }
        }
        let mut visits = 0;
        while let Some(i) = pending.pop_front() {
            for &caller in &self.reverse[i] {
                visits += 1;
                if affected.insert(caller) {
                    pending.push_back(caller);
                }
            }
        }
        let mut ordered: Vec<_> = affected.iter().copied().collect();
        ordered.sort_unstable();
        let symbols: Vec<_> = ordered
            .iter()
            .flat_map(|&i| {
                self.symbols[i]
                    .iter()
                    .map(|&s| &snapshot.semantic.symbols[s])
            })
            .collect();
        let mut affected_tests = Vec::new();
        let mut unknown_tests = Vec::new();
        let mut seen_tests = HashSet::new();
        for test in tests {
            if !seen_tests.insert(test) {
                continue;
            }
            match self.functions.get(test) {
                Some(&(module, function)) if affected.contains(&module) => {
                    affected_tests.push(&snapshot.functions[function])
                }
                Some(_) => {}
                None => unknown_tests.push(test),
            }
        }
        json!({"status":if snapshot.semantic.modules.is_empty() || !unknown_tests.is_empty() {"incomplete"}else{"ok"},
            "coverage":"conservative loaded-module import closure; supply test FunctionIds from this snapshot; unloaded modules/tests are not analyzed",
            "modules":ordered.iter().map(|&i| &snapshot.semantic.modules[i].identity).collect::<Vec<_>>(),
            "symbols":symbols,"tests":affected_tests,"unknown_tests":unknown_tests,
            "unmatched_packages":unmatched,"module_visits":affected.len(),"edge_visits":visits})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_index_work_is_bounded_for_nested_and_sibling_spans() {
        use crate::diagnostics::FileId;
        for n in [16usize, 64, 256, 1024] {
            for nested in [false, true] {
                let owners: Vec<_> = (0..n)
                    .map(|i| {
                        let (start, end) = if nested {
                            (i, 2 * n - i)
                        } else {
                            (2 * i, 2 * i + 1)
                        };
                        (
                            Span::in_file(FileId(1), start, end, 0, 0),
                            format!("owner{i}"),
                        )
                    })
                    .collect();
                let index = OwnerIndex::new(owners.iter().rev());
                assert_eq!(index.event_updates, 2 * n);
                assert!(index.segments.values().map(Vec::len).sum::<usize>() <= 2 * n);
                assert!(index.at(FileId(0), 0).is_none());
                for _ in 0..8 {
                    for (i, (span, name)) in owners.iter().enumerate() {
                        assert_eq!(index.at(FileId(1), span.start), Some(name.as_str()));
                        if !nested {
                            assert!(index.at(FileId(1), span.end).is_none());
                        } else {
                            assert_eq!(index.at(FileId(1), 2 * n - i - 1), Some(name.as_str()));
                        }
                    }
                    assert!(index.at(FileId(1), 2 * n).is_none());
                }
                let queries = 8 * (2 * n + 1);
                let bound = queries * ((2 * n).ilog2() as usize + 2);
                assert!(index.comparisons.get() <= bound);
                eprintln!(
                    "owner-index nested={nested} owners={n} events={} segments={} queries={queries} comparisons={} bound={bound}",
                    index.event_updates,
                    index.segments[&FileId(1)].len(),
                    index.comparisons.get()
                );
            }
        }
    }

    #[test]
    fn affected_counts_scale_with_reachable_modules_and_edges() {
        for n in [16, 64, 256, 1024] {
            for shape in ["chain", "fan-out", "diamond"] {
                let package = PackageIdentity {
                    name: "dep".into(),
                    version: "1.0.0".into(),
                    source: crate::package::PackageSourceIdentity::Path {
                        path: "/dep".into(),
                    },
                    revision: None,
                };
                let mut local = package.clone();
                local.name = "app".into();
                let modules: Vec<_> = (0..2 * n)
                    .map(|i| ModuleEvidence {
                        path: format!("/m{i}.wi"),
                        identity: ModuleIdentity {
                            package: if i == 0 {
                                package.clone()
                            } else {
                                local.clone()
                            },
                            module: format!("m{i}"),
                        },
                        dependencies: if i == 0 || i >= n {
                            vec![]
                        } else {
                            match shape {
                                "chain" => vec![i - 1],
                                "diamond" if i > 1 => vec![0, i - 1],
                                _ => vec![0],
                            }
                        },
                    })
                    .collect();
                let edges: usize = modules.iter().map(|m| m.dependencies.len()).sum();
                let snapshot = Snapshot {
                    version: 1,
                    compiler: storage::compiler_stamp(),
                    compatibility: "test".into(),
                    workspace: "/".into(),
                    revision: String::new(),
                    sources: BTreeMap::new(),
                    functions: vec![],
                    semantic: SemanticFacts {
                        modules,
                        ..Default::default()
                    },
                };
                let index = PackageIndex::new(&snapshot);
                let delta = UpdateDelta {
                    schema: 1,
                    ok: true,
                    kind: "package.mutation".into(),
                    packages_added: vec![package.clone(); n],
                    changes: vec![],
                    transitive_changes: vec![],
                };
                for _ in 0..8 {
                    let result = index.affected(&snapshot, &delta, &[]);
                    assert_eq!(result["module_visits"], n);
                    assert_eq!(result["edge_visits"], edges);
                }
                eprintln!(
                    "shape={shape} loaded={} affected={n} edge_visits={edges} queries=8",
                    2 * n
                );
                let mut other_source = package.clone();
                other_source.source = crate::package::PackageSourceIdentity::Path {
                    path: "/other-dep".into(),
                };
                let foreign = UpdateDelta {
                    packages_added: vec![other_source],
                    ..delta.clone()
                };
                assert_eq!(index.affected(&snapshot, &foreign, &[])["module_visits"], 0);
                let mut next = package.clone();
                next.revision = Some("planned-revision".into());
                let transitive = UpdateDelta {
                    packages_added: vec![],
                    transitive_changes: vec![Change {
                        before: Some(package.clone()),
                        after: Some(next),
                    }],
                    ..delta.clone()
                };
                assert_eq!(
                    index.affected(&snapshot, &transitive, &[])["module_visits"],
                    n
                );
                let unchanged = UpdateDelta {
                    packages_added: vec![],
                    changes: vec![DirectChange {
                        resolved_before: Some(package.clone()),
                        resolved_after: Some(package),
                    }],
                    ..delta
                };
                assert_eq!(
                    index.affected(&snapshot, &unchanged, &[])["module_visits"],
                    0
                );
            }
        }
    }
}
