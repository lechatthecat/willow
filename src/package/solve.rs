//! Incremental depth-first search with an undo trail. Decisions restore only
//! the changed suffix; manifests and Git objects are shared across backtracking.
use super::{
    PackageError, PackageGraph, PackageId, PackageIdentity, PackageSourceIdentity, PathSource,
    ResolutionRequirement, ResolutionStats, ResolvedDependency, ResolvedPackage,
    git::{GitSource, SystemGit},
    source::canonical_root,
};
use crate::project::{DependencySource, GitSelector};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
};

#[cfg(test)]
thread_local! {
    pub(super) static FAILURE_STATS: std::cell::RefCell<ResolutionStats> = std::cell::RefCell::new(ResolutionStats::default());
}

#[derive(Clone)]
pub(super) struct GitPin {
    pub identity: PackageIdentity,
    pub selectors: HashSet<String>,
}

type Key = PackageSourceIdentity;
#[derive(Clone)]
struct Task {
    parent: PackageId,
    alias: String,
    source: DependencySource,
}
struct Checkpoint {
    packages: usize,
    edges: usize,
    tasks: usize,
    cursor: usize,
}
struct Decision {
    checkpoint: Checkpoint,
    task: Task,
    url: String,
    // None defers remote alternatives until a pinned assignment conflicts.
    candidates: Option<super::git::GitCandidates>,
    causes: HashSet<Key>,
}
struct GitOrigin {
    url: String,
    revision: String,
    root: PathBuf,
}
struct Solver {
    graph: PackageGraph,
    known: HashMap<Key, PackageId>,
    paths: HashMap<PathBuf, Rc<PathSource>>,
    git: HashMap<String, GitSource>,
    pins: HashMap<String, GitPin>,
    checksums: HashMap<String, String>,
    offline: bool,
    read_only: bool,
    repair_pins: bool,
    tasks: Vec<Task>,
    cursor: usize,
    edges: Vec<PackageId>,
    decisions: Vec<Decision>,
    parents: Vec<Option<PackageId>>,
    origins: Vec<Option<Rc<GitOrigin>>>,
    incoming: HashMap<String, Vec<usize>>,
    causes: HashSet<Key>,
}

/// Resolve afresh, choosing the highest matching versions. Does not write a lock.
pub fn resolve_packages(root: &Path) -> Result<PackageGraph, PackageError> {
    resolve_source(
        PathSource::open(root, false)?,
        HashMap::new(),
        HashMap::new(),
        false,
        false,
    )
}
/// Explicit update entry point. CLI presentation is owned by the package CLI.
pub fn update_packages(root: &Path) -> anyhow::Result<PackageGraph> {
    super::lock::update(root)
}
pub(super) fn resolve_source(
    root: PathSource,
    pins: HashMap<String, GitPin>,
    checksums: HashMap<String, String>,
    offline: bool,
    repair_pins: bool,
) -> Result<PackageGraph, PackageError> {
    resolve_prepared(
        root,
        pins,
        checksums,
        offline,
        false,
        HashMap::new(),
        repair_pins,
    )
}

pub(super) fn resolve_prepared(
    root: PathSource,
    pins: HashMap<String, GitPin>,
    checksums: HashMap<String, String>,
    offline: bool,
    read_only: bool,
    git: HashMap<String, GitSource>,
    repair_pins: bool,
) -> Result<PackageGraph, PackageError> {
    let mut solver = Solver {
        graph: PackageGraph {
            root: PackageId(0),
            packages: Vec::new(),
            stats: ResolutionStats {
                manifests_loaded: 1,
                paths_canonicalized: 1,
                ..Default::default()
            },
        },
        known: HashMap::new(),
        paths: HashMap::new(),
        git,
        pins,
        checksums,
        offline,
        read_only,
        repair_pins,
        tasks: Vec::new(),
        cursor: 0,
        edges: Vec::new(),
        decisions: Vec::new(),
        parents: Vec::new(),
        origins: Vec::new(),
        incoming: HashMap::new(),
        causes: HashSet::new(),
    };
    let identity = root.identity();
    let root = Rc::new(root);
    solver.paths.insert(root.root.clone(), root.clone());
    solver.insert(identity, root, None, None)?;
    solver.run()
}
impl Solver {
    fn insert(
        &mut self,
        identity: PackageIdentity,
        source: Rc<PathSource>,
        parent: Option<PackageId>,
        origin: Option<Rc<GitOrigin>>,
    ) -> Result<PackageId, PackageError> {
        let id = PackageId(
            u32::try_from(self.graph.packages.len()).map_err(|_| PackageError::TooManyPackages)?,
        );
        self.parents.push(parent);
        self.origins.push(origin);
        self.known.insert(identity.source.clone(), id);
        let checksum = match &identity.source {
            PackageSourceIdentity::Git { url } => self.git[url]
                .checksum(identity.revision.as_deref().expect("resolved Git revision"))?,
            _ => None,
        };
        self.graph.packages.push(ResolvedPackage {
            checksum,
            id,
            identity,
            root: source.root.clone(),
            dependencies: Vec::new(),
        });
        // Append once per assignment; no prefix graph scans or manifest clones.
        for (alias, dependency) in &source.manifest.dependencies {
            if let DependencySource::Git { url, .. } = dependency {
                self.incoming
                    .entry(url.as_str().into())
                    .or_default()
                    .push(self.tasks.len());
            }
            self.tasks.push(Task {
                parent: id,
                alias: alias.clone(),
                source: dependency.clone(),
            });
        }
        Ok(id)
    }
    fn edge(&mut self, task: &Task, id: PackageId) {
        self.graph.packages[task.parent.0 as usize]
            .dependencies
            .push(ResolvedDependency {
                alias: task.alias.clone(),
                package: id,
                selector: match &task.source {
                    DependencySource::Git { selector, .. } => Some(selector_text(selector)),
                    _ => None,
                },
            });
        self.edges.push(task.parent);
    }
    fn checkpoint(&self) -> Checkpoint {
        Checkpoint {
            packages: self.graph.packages.len(),
            edges: self.edges.len(),
            tasks: self.tasks.len(),
            cursor: self.cursor,
        }
    }
    fn restore(&mut self, checkpoint: &Checkpoint) {
        while self.edges.len() > checkpoint.edges {
            let parent = self.edges.pop().unwrap();
            self.graph.packages[parent.0 as usize].dependencies.pop();
        }
        while self.graph.packages.len() > checkpoint.packages {
            let package = self.graph.packages.pop().unwrap();
            self.known.remove(&package.identity.source);
            self.parents.pop();
            self.origins.pop();
        }
        while self.tasks.len() > checkpoint.tasks {
            let task = self.tasks.pop().unwrap();
            if let DependencySource::Git { url, .. } = task.source {
                let indices = self.incoming.get_mut(url.as_str()).unwrap();
                indices.pop();
                if indices.is_empty() {
                    self.incoming.remove(url.as_str());
                }
            }
        }
        self.cursor = checkpoint.cursor;
    }
    fn ensure_git(&mut self, url: &crate::project::CanonicalGitUrl) -> Result<(), PackageError> {
        if self.git.contains_key(url.as_str()) {
            return Ok(());
        }
        let pinned = self.pins.contains_key(url.as_str());
        let source = GitSource::cached_mode(
            url.clone(),
            SystemGit,
            self.offline,
            pinned,
            self.checksums.get(url.as_str()).cloned(),
            self.read_only,
        )?;
        self.git.insert(url.as_str().into(), source);
        Ok(())
    }
    fn assign(
        &mut self,
        task: &Task,
        url: &str,
        reference: &str,
        tag: Option<&str>,
    ) -> Result<(), PackageError> {
        self.graph.stats.candidate_attempts += 1;
        let git = &self.git[url];
        let revision = git.revision(reference)?;
        let identity = git.at(&revision, tag)?;
        let source = git.source(&revision);
        if !self.paths.contains_key(&source.root) {
            self.graph.stats.manifests_loaded += 1;
            self.paths.insert(source.root.clone(), source.clone());
        }
        let origin = Rc::new(GitOrigin {
            url: url.into(),
            revision,
            root: source.root.clone(),
        });
        let id = self.insert(identity, source, Some(task.parent), Some(origin))?;
        self.edge(task, id);
        Ok(())
    }
    fn conflict(&mut self, url: &str) -> PackageError {
        let mut parents = Vec::new();
        let requirements = self
            .incoming
            .get(url)
            .into_iter()
            .flatten()
            .copied()
            .take_while(|i| *i < self.cursor)
            .map(|i| &self.tasks[i])
            .filter_map(|task| {
                let DependencySource::Git {
                    url: other,
                    selector,
                } = &task.source
                else {
                    return None;
                };
                (url == other.as_str()).then(|| {
                    parents.push(task.parent);
                    ResolutionRequirement {
                        required_by: self.graph.packages[task.parent.0 as usize]
                            .identity
                            .name
                            .clone(),
                        requirement: selector_text(selector),
                    }
                })
            })
            .collect();
        self.causes.insert(Key::Git { url: url.into() });
        if let Some(&id) = self.known.get(&Key::Git { url: url.into() }) {
            parents.push(id);
        }
        self.causes_for(parents);
        PackageError::VersionConflict {
            url: url.into(),
            requirements,
        }
    }
    fn causes_for(&mut self, nodes: impl IntoIterator<Item = PackageId>) {
        // Each ancestor is visited at most once for this failure.
        let mut seen = HashSet::new();
        for mut id in nodes {
            loop {
                if !seen.insert(id) {
                    break;
                }
                self.causes
                    .insert(self.graph.packages[id.0 as usize].identity.source.clone());
                let Some(parent) = self.parents[id.0 as usize] else {
                    break;
                };
                id = parent;
            }
        }
    }
    fn step(&mut self, task: Task) -> Result<(), PackageError> {
        self.graph.stats.dependencies_visited += 1;
        match &task.source {
            DependencySource::Path { path } => {
                self.graph.stats.paths_canonicalized += 1;
                let parent = &self.graph.packages[task.parent.0 as usize];
                let root = canonical_root(&parent.root.join(path))?;
                let origin = self.origins[task.parent.0 as usize].clone();
                let key = if let Some(origin) = &origin {
                    let relative =
                        root.strip_prefix(&origin.root)
                            .map_err(|_| PackageError::PathEscape {
                                root: origin.root.clone(),
                                path: root.clone(),
                            })?;
                    if relative.as_os_str().is_empty() {
                        Key::Git {
                            url: origin.url.clone(),
                        }
                    } else {
                        let path = relative
                            .components()
                            .map(|c| {
                                c.as_os_str().to_str().ok_or_else(|| {
                                    PackageError::GitMaterialization("non-UTF8 Git path".into())
                                })
                            })
                            .collect::<Result<Vec<_>, _>>()?
                            .join("/");
                        Key::GitSubdirectory {
                            url: origin.url.clone(),
                            path,
                        }
                    }
                } else {
                    Key::Path { path: root.clone() }
                };
                let id = if let Some(&id) = self.known.get(&key) {
                    id
                } else {
                    let source = if let Some(source) = self.paths.get(&root) {
                        source.clone()
                    } else {
                        let source = Rc::new(PathSource::open_canonical(root.clone(), true)?);
                        self.paths.insert(root, source.clone());
                        self.graph.stats.manifests_loaded += 1;
                        source
                    };
                    let mut identity = source.identity();
                    identity.source = key;
                    identity.revision = origin.as_ref().map(|origin| origin.revision.clone());
                    self.insert(identity, source, Some(task.parent), origin)?
                };
                self.edge(&task, id);
            }
            DependencySource::Git { url, selector } => {
                self.ensure_git(url)?;
                self.graph.stats.constraint_checks += 1;
                let key = Key::Git {
                    url: url.as_str().into(),
                };
                if let Some(&id) = self.known.get(&key) {
                    let identity = &self.graph.packages[id.0 as usize].identity;
                    if !self.matches(url.as_str(), selector, identity)? {
                        return Err(self.conflict(url.as_str()));
                    }
                    self.edge(&task, id);
                } else if let Some(pin) =
                    self.pins.get(url.as_str()).map(|pin| pin.identity.clone())
                {
                    if self.repair_pins {
                        // Only a conflict involving this source activates its
                        // fresh alternatives. Keep the prefix and cached analyses.
                        self.decisions.push(Decision {
                            checkpoint: self.checkpoint(),
                            task: task.clone(),
                            url: url.as_str().into(),
                            candidates: None,
                            causes: HashSet::new(),
                        });
                    }
                    if !self.matches(url.as_str(), selector, &pin)? {
                        return Err(self.conflict(url.as_str()));
                    }
                    let revision = pin
                        .revision
                        .as_deref()
                        .ok_or_else(|| self.conflict(url.as_str()))?;
                    self.assign(&task, url.as_str(), revision, None)?;
                    let actual = &self.graph.packages.last().unwrap().identity;
                    if *actual != pin {
                        return Err(self.conflict(url.as_str()));
                    }
                } else {
                    let mut candidates = self.git[url.as_str()].candidates(selector)?;
                    let (reference, tag) = candidates.next().unwrap();
                    let checkpoint = self.checkpoint();
                    let result = self.assign(&task, url.as_str(), &reference, tag.as_deref());
                    if candidates.len() != 0 {
                        let url = url.as_str().to_owned();
                        self.decisions.push(Decision {
                            checkpoint,
                            task,
                            url,
                            candidates: Some(candidates),
                            causes: HashSet::new(),
                        });
                    }
                    result?;
                }
            }
        }
        Ok(())
    }
    fn matches(
        &self,
        url: &str,
        selector: &GitSelector,
        identity: &PackageIdentity,
    ) -> Result<bool, PackageError> {
        let revision = identity.revision.as_deref().unwrap_or("");
        if self.pins.get(url).is_some_and(|pin| {
            pin.identity == *identity && pin.selectors.contains(&selector_text(selector))
        }) && !matches!(selector, GitSelector::Version(_))
        {
            return Ok(match selector {
                GitSelector::Revision(rev) => {
                    !rev.is_empty()
                        && rev.bytes().all(|c| c.is_ascii_hexdigit())
                        && revision.starts_with(&rev.to_ascii_lowercase())
                }
                _ => true,
            });
        }
        let needs_refs = match selector {
            GitSelector::Branch(_) | GitSelector::Tag(_) => true,
            GitSelector::Revision(rev) => !matches!(rev.len(), 40 | 64),
            GitSelector::Version(_) => false,
        };
        if self.pins.contains_key(url) && needs_refs {
            self.git[url].refresh_refs()?;
        }
        Ok(match selector {
            GitSelector::Version(req) => req.matches(
                &semver::Version::parse(&identity.version)
                    .map_err(|e| PackageError::VersionUnavailable(e.to_string()))?,
            ),
            GitSelector::Revision(rev) => {
                !rev.is_empty()
                    && rev.bytes().all(|c| c.is_ascii_hexdigit())
                    && self.git[url].revision(rev)? == revision
            }
            GitSelector::Branch(branch) => {
                self.git[url].revision(&format!("refs/heads/{branch}"))? == revision
            }
            GitSelector::Tag(tag) => {
                let same = self.git[url].revision(&format!("refs/tags/{tag}"))? == revision;
                if same {
                    self.git[url].at(revision, Some(tag))?;
                }
                same
            }
        })
    }
    fn finish_stats(&mut self) {
        self.graph.stats.git_sources_fetched =
            self.git.values().map(|source| source.fetches.get()).sum();
        self.graph.stats.version_candidates_checked = self
            .git
            .values()
            .map(|source| source.candidate_checks.get())
            .sum();
    }
    fn fail(mut self, error: PackageError) -> Result<PackageGraph, PackageError> {
        self.finish_stats();
        #[cfg(test)]
        FAILURE_STATS.with(|stats| *stats.borrow_mut() = self.graph.stats);
        Err(error)
    }
    fn run(mut self) -> Result<PackageGraph, PackageError> {
        loop {
            let result = if self.cursor < self.tasks.len() {
                let task = self.tasks[self.cursor].clone();
                self.cursor += 1;
                let parent = task.parent;
                let key = match &task.source {
                    DependencySource::Git { url, .. } => Some(Key::Git {
                        url: url.as_str().into(),
                    }),
                    _ => None,
                };
                let result = self.step(task);
                if result.is_err() && self.causes.is_empty() {
                    if let Some(key) = key {
                        self.causes.insert(key);
                    }
                    self.causes_for([parent]);
                }
                result
            } else {
                match cycles(&self.graph) {
                    Ok(()) => {
                        self.finish_stats();
                        return Ok(self.graph);
                    }
                    Err(e) => {
                        self.causes_for(
                            (0..self.graph.packages.len())
                                .map(|i| PackageId(i as u32))
                                .collect::<Vec<_>>(),
                        );
                        Err(e)
                    }
                }
            };
            if let Err(mut error) = result {
                loop {
                    if !matches!(
                        error,
                        PackageError::VersionConflict { .. }
                            | PackageError::VersionNotFound { .. }
                            | PackageError::GitRevisionNotFound { .. }
                            | PackageError::Cycle(_)
                    ) {
                        return self.fail(error);
                    }
                    let Some(mut decision) = self.decisions.pop() else {
                        return self.fail(error);
                    };
                    let key = Key::Git {
                        url: decision.url.clone(),
                    };
                    self.restore(&decision.checkpoint);
                    if !self.causes.contains(&key) {
                        continue;
                    }
                    decision.causes.extend(self.causes.drain());
                    if decision.candidates.is_none() {
                        self.pins.remove(&decision.url);
                        self.checksums.remove(&decision.url);
                        let DependencySource::Git { url, selector } = &decision.task.source else {
                            unreachable!()
                        };
                        let candidates = self
                            .git
                            .get_mut(url.as_str())
                            .unwrap()
                            .unlock()
                            .and_then(|()| self.git[&decision.url].candidates(selector));
                        match candidates {
                            Ok(candidates) => decision.candidates = Some(candidates),
                            Err(next) => {
                                self.causes = decision.causes;
                                self.causes.remove(&key);
                                self.causes_for([decision.task.parent]);
                                error = next;
                                continue;
                            }
                        }
                    }
                    let Some((reference, tag)) = decision.candidates.as_mut().unwrap().next()
                    else {
                        self.causes = decision.causes;
                        self.causes.remove(&key);
                        self.causes_for([decision.task.parent]);
                        continue;
                    };
                    self.graph.stats.backtracks += 1;
                    let result =
                        self.assign(&decision.task, &decision.url, &reference, tag.as_deref());
                    if result.is_err() && self.causes.is_empty() {
                        self.causes.insert(key);
                        self.causes_for([decision.task.parent]);
                    }
                    // Retain even an exhausted decision to propagate the union
                    // of causes from all failed alternatives to earlier choices.
                    self.decisions.push(decision);
                    match result {
                        Ok(()) => break,
                        Err(next) => error = next,
                    }
                }
            }
        }
    }
}
pub(super) fn selector_text(selector: &GitSelector) -> String {
    match selector {
        GitSelector::Version(req) => format!("version:{req}"),
        GitSelector::Revision(rev) => format!("rev:{rev}"),
        GitSelector::Branch(branch) => format!("branch:{branch}"),
        GitSelector::Tag(tag) => format!("tag:{tag}"),
    }
}
fn cycles(graph: &PackageGraph) -> Result<(), PackageError> {
    let mut state = vec![0u8; graph.packages.len()];
    let mut stack = vec![(graph.root, 0)];
    state[graph.root.0 as usize] = 1;
    while let Some((id, edge)) = stack.last_mut() {
        let package = &graph.packages[id.0 as usize];
        if *edge == package.dependencies.len() {
            state[id.0 as usize] = 2;
            stack.pop();
            continue;
        }
        let next = package.dependencies[*edge].package;
        *edge += 1;
        match state[next.0 as usize] {
            0 => {
                state[next.0 as usize] = 1;
                stack.push((next, 0));
            }
            1 => {
                let mut paths: Vec<_> = stack
                    .iter()
                    .map(|(id, _)| graph.packages[id.0 as usize].root.clone())
                    .collect();
                paths.push(graph.packages[next.0 as usize].root.clone());
                return Err(PackageError::Cycle(paths));
            }
            _ => {}
        }
    }
    Ok(())
}
