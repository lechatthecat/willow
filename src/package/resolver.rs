use std::{
    collections::{HashMap, btree_map},
    path::Path,
};

use crate::project::DependencySource;

use super::{
    PackageError, PackageGraph, PackageId, PathSource, ResolvedDependency, ResolvedPackage,
};

/// Deterministic work counts, independent of filesystem timing.
#[derive(Debug, Default, Clone, Copy)]
pub struct ResolutionStats {
    pub manifests_loaded: usize,
    pub dependencies_visited: usize,
    pub paths_canonicalized: usize,
    pub candidate_attempts: usize,
    pub backtracks: usize,
    pub constraint_checks: usize,
    pub git_sources_fetched: usize,
    pub version_candidates_checked: usize,
}

struct Frame {
    id: PackageId,
    dependencies: btree_map::IntoIter<String, DependencySource>,
}

fn insert(source: PathSource, graph: &mut PackageGraph) -> Result<Frame, PackageError> {
    let id =
        PackageId(u32::try_from(graph.packages.len()).map_err(|_| PackageError::TooManyPackages)?);
    graph.packages.push(ResolvedPackage {
        checksum: None,
        id,
        identity: source.identity(),
        root: source.root,
        dependencies: Vec::with_capacity(source.manifest.dependencies.len()),
    });
    graph.stats.manifests_loaded += 1;
    Ok(Frame {
        id,
        dependencies: source.manifest.dependencies.into_iter(),
    })
}

/// Resolve each canonical directory once, retaining aliases on graph edges.
/// An explicit DFS stack avoids host-stack overflow on long dependency chains.
pub fn resolve_path_packages(root: &Path) -> Result<PackageGraph, PackageError> {
    resolve_path_source(PathSource::open(root, false)?)
}

pub(super) fn resolve_path_source(root: PathSource) -> Result<PackageGraph, PackageError> {
    let mut graph = PackageGraph {
        root: PackageId(0),
        packages: Vec::new(),
        stats: ResolutionStats::default(),
    };
    graph.stats.paths_canonicalized = 1;
    let mut known = HashMap::new();
    known.insert(root.root.clone(), PackageId(0));
    let mut stack = vec![insert(root, &mut graph)?];
    // One state per node; indices into the active DFS stack support O(1)
    // cycle detection and only copy the path when reporting an actual cycle.
    let mut active = vec![Some(0usize)];
    while let Some(frame) = stack.last_mut() {
        let parent = frame.id;
        let Some((alias, source)) = frame.dependencies.next() else {
            active[parent.0 as usize] = None;
            stack.pop();
            continue;
        };
        graph.stats.dependencies_visited += 1;
        let parent_root = &graph.packages[parent.0 as usize].root;
        let DependencySource::Path { path } = source else {
            return Err(PackageError::UnsupportedSource {
                root: parent_root.clone(),
                alias,
            });
        };
        graph.stats.paths_canonicalized += 1;
        let root = super::source::canonical_root(&parent_root.join(path))?;
        let (id, next) = if let Some(&id) = known.get(&root) {
            if let Some(start) = active[id.0 as usize] {
                let mut cycle: Vec<_> = stack[start..]
                    .iter()
                    .map(|frame| graph.packages[frame.id.0 as usize].root.clone())
                    .collect();
                cycle.push(root);
                return Err(PackageError::Cycle(cycle));
            }
            (id, None)
        } else {
            let frame = insert(PathSource::open_canonical(root.clone(), true)?, &mut graph)?;
            known.insert(root, frame.id);
            active.push(Some(stack.len()));
            (frame.id, Some(frame))
        };
        graph.packages[parent.0 as usize]
            .dependencies
            .push(ResolvedDependency {
                alias,
                package: id,
                selector: None,
            });
        if let Some(next) = next {
            stack.push(next);
        }
    }
    Ok(graph)
}
