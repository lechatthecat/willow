//! Versioned semantic edit evidence. Baseline capture is separate from comparison:
//! the radix index skips equal subtrees, and caller traversal visits each reached
//! edge once. Loading/capturing a baseline still costs its serialized input size.
use super::*;
use crate::diagnostics::{Diagnostic, DiagnosticEmitter, source_map::SourceLookup};
use serde_json::{Value, json};
use std::collections::VecDeque;

const BASELINE_SIZE_LIMIT: usize = 64 * 1024 * 1024;

struct LimitedWriter<W> {
    inner: W,
    remaining: usize,
}
impl<W: std::io::Write> std::io::Write for LimitedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other(
                "verification baseline exceeds size limit",
            ));
        }
        let written = self.inner.write(bytes)?;
        self.remaining -= written;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The caller attests that this list contains every changed compiler input.
/// Paths are normalized workspace-relative paths; hashes are SHA-256 of bytes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditManifest {
    pub schema_version: u32,
    pub baseline_revision: String,
    pub complete: bool,
    pub changes: Vec<InputChange>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputChange {
    pub path: String,
    pub before: Option<String>,
    pub after: Option<String>,
}
impl EditManifest {
    pub fn load(path: &Path) -> Result<Self> {
        use std::io::Read;
        let mut reader = std::fs::File::open(path)?.take(BASELINE_SIZE_LIMIT as u64 + 1);
        let manifest: Self = serde_json::from_reader(std::io::BufReader::new(&mut reader))?;
        ensure!(reader.limit() > 0, "edit manifest exceeds size limit");
        manifest.validate()?;
        Ok(manifest)
    }
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.complete,
            "complete version-1 edit manifest required"
        );
        let digest = |s: &str| {
            s.len() == 64
                && s.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        };
        ensure!(
            digest(&self.baseline_revision),
            "invalid manifest baseline revision"
        );
        let mut paths = BTreeSet::new();
        for change in &self.changes {
            let path = Path::new(&change.path);
            ensure!(
                !change.path.is_empty()
                    && !change.path.contains('\\')
                    && change
                        .path
                        .split('/')
                        .all(|part| !part.is_empty() && part != "." && part != "..")
                    && path
                        .components()
                        .all(|part| matches!(part, std::path::Component::Normal(_))),
                "invalid manifest path"
            );
            ensure!(paths.insert(&change.path), "duplicate manifest input");
            ensure!(change.before != change.after, "manifest input is unchanged");
            ensure!(
                change.before.iter().chain(&change.after).all(|s| digest(s)),
                "invalid manifest input digest"
            );
        }
        Ok(())
    }
    pub(super) fn check(&self, baseline: &Baseline, root: &Path) -> Result<()> {
        self.validate()?;
        ensure!(
            self.baseline_revision == baseline.revision,
            "manifest baseline revision mismatch"
        );
        for change in &self.changes {
            ensure!(
                baseline.inputs.get(&change.path) == change.before.as_ref(),
                "manifest before digest mismatch: {}",
                change.path
            );
            let path = root.join(&change.path);
            let after = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => {
                    ensure!(
                        metadata.is_file() && std::fs::canonicalize(&path)? == path,
                        "manifest input is not a regular canonical file"
                    );
                    Some(hash(std::fs::read(&path)?))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            ensure!(
                after == change.after,
                "manifest after digest mismatch: {}",
                change.path
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiagnosticFact {
    pub code: String,
    pub severity: String,
    pub message: String,
    pub locations: Vec<Site>,
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Site {
    pub path: String,
    pub owner: Option<String>,
    pub anchor: String,
    pub start: usize,
    pub end: usize,
}
#[derive(Default)]
pub(crate) struct Diagnostics {
    pub facts: Vec<DiagnosticFact>,
}
impl DiagnosticEmitter for Diagnostics {
    fn emit(&mut self, d: &Diagnostic, sources: &dyn SourceLookup) -> std::io::Result<()> {
        self.facts.push(DiagnosticFact {
            code: d.code.as_str().into(),
            severity: d.severity.as_str().into(),
            message: d.message.clone(),
            locations: d
                .labels
                .iter()
                .filter_map(|label| {
                    let map = sources.get(label.span.file_id)?;
                    Some(Site {
                        path: map.path.clone(),
                        owner: None,
                        anchor: hash(
                            map.source
                                .get(label.span.start..label.span.end)
                                .unwrap_or(""),
                        ),
                        start: label.span.start,
                        end: label.span.end,
                    })
                })
                .collect(),
        });
        Ok(())
    }
}
pub(crate) struct RecordingDiagnostics<'a> {
    pub downstream: &'a mut dyn DiagnosticEmitter,
    pub captured: Diagnostics,
}
impl DiagnosticEmitter for RecordingDiagnostics<'_> {
    fn emit(&mut self, diagnostic: &Diagnostic, sources: &dyn SourceLookup) -> std::io::Result<()> {
        self.captured.emit(diagnostic, sources)?;
        self.downstream.emit(diagnostic, sources)
    }
}
fn relative(path: &str, root: &str) -> String {
    Path::new(path)
        .strip_prefix(root)
        .ok()
        .and_then(Path::to_str)
        .unwrap_or(path)
        .replace(std::path::MAIN_SEPARATOR, "/")
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectState {
    /// Evidence-backed capabilities, including propagated known callees.
    pub may: u8,
    /// Capabilities retained solely because a reachable target is unknown.
    pub unknown: u8,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fact {
    signature: Option<Value>,
    effects: Option<EffectState>,
    body: String,
    callees: Vec<String>,
}
/// A compressed SHA-256 radix tree: at most two nodes per stored fact when
/// branches have two or more children; depth is bounded by 64 hex digits.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    prefix: String,
    digest: String,
    key: Option<String>,
    children: BTreeMap<char, Node>,
}
impl Node {
    fn build(entries: &[(String, String, String)]) -> Option<Self> {
        let first = entries.first()?;
        let last = entries.last().unwrap();
        let common = first
            .0
            .bytes()
            .zip(last.0.bytes())
            .take_while(|(a, b)| a == b)
            .count();
        if entries.len() == 1 {
            return Some(Self {
                prefix: first.0.clone(),
                digest: first.2.clone(),
                key: Some(first.1.clone()),
                children: BTreeMap::new(),
            });
        }
        // Collision is not accepted as an identity equivalence.
        assert!(common < 64, "semantic identity hash collision");
        let mut children = BTreeMap::new();
        let mut start = 0;
        while start < entries.len() {
            let digit = entries[start].0.as_bytes()[common];
            let end = start + entries[start..].partition_point(|e| e.0.as_bytes()[common] == digit);
            children.insert(digit as char, Self::build(&entries[start..end]).unwrap());
            start = end;
        }
        let digest = hash_serialized(
            &children
                .iter()
                .map(|(k, n)| (k, &n.digest))
                .collect::<Vec<_>>(),
        )
        .expect("radix digest");
        Some(Self {
            prefix: first.0[..common].into(),
            digest,
            key: None,
            children,
        })
    }
    fn collect(&self, keys: &mut BTreeSet<String>, work: &mut Work) {
        work.index_nodes += 1;
        if let Some(key) = &self.key {
            keys.insert(key.clone());
        }
        for child in self.children.values() {
            child.collect(keys, work);
        }
    }
    fn difference(
        a: Option<&Self>,
        b: Option<&Self>,
        keys: &mut BTreeSet<String>,
        work: &mut Work,
    ) {
        work.index_nodes += 1;
        let (a, b) = match (a, b) {
            (Some(a), Some(b)) => (a, b),
            (Some(n), None) | (None, Some(n)) => {
                n.collect(keys, work);
                return;
            }
            _ => return,
        };
        if a.prefix == b.prefix && a.digest == b.digest {
            return;
        }
        if a.prefix == b.prefix && a.key.is_none() && b.key.is_none() {
            for digit in a
                .children
                .keys()
                .chain(b.children.keys())
                .copied()
                .collect::<BTreeSet<_>>()
            {
                Self::difference(a.children.get(&digit), b.children.get(&digit), keys, work);
            }
        } else if b.prefix.starts_with(&a.prefix) && a.key.is_none() {
            let digit = b.prefix.as_bytes()[a.prefix.len()] as char;
            for (k, child) in &a.children {
                Self::difference(Some(child), (*k == digit).then_some(b), keys, work);
            }
            if !a.children.contains_key(&digit) {
                b.collect(keys, work);
            }
        } else if a.prefix.starts_with(&b.prefix) && b.key.is_none() {
            Self::difference(Some(b), Some(a), keys, work);
        } else {
            a.collect(keys, work);
            b.collect(keys, work);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Baseline {
    schema_version: u32,
    compiler: String,
    compatibility: String,
    revision: String,
    complete: bool,
    diagnostics: Vec<DiagnosticFact>,
    facts: BTreeMap<String, Fact>,
    inputs: BTreeMap<String, String>,
    #[serde(skip)]
    callers: BTreeMap<String, BTreeSet<String>>,
    #[serde(skip)]
    index: Option<Node>,
    #[serde(skip)]
    diagnostic_index: Option<Node>,
    #[serde(skip)]
    diagnostic_lookup: BTreeMap<String, usize>,
    #[serde(skip)]
    public_count: usize,
}
#[derive(Debug, Default, Serialize)]
pub struct Work {
    pub index_nodes: usize,
    pub compared_symbols: usize,
    pub caller_edges: usize,
}
#[derive(Debug, Serialize)]
pub struct Change<T> {
    pub symbol: String,
    pub before: Option<T>,
    pub after: Option<T>,
}
#[derive(Debug, Serialize)]
pub struct Delta<T> {
    pub added: Vec<T>,
    pub removed: Vec<T>,
}
#[derive(Debug, Serialize)]
pub struct Effects {
    pub changed: Vec<Change<EffectState>>,
    pub new_e2604: Vec<DiagnosticFact>,
    pub newly_non_awaitable: Vec<DiagnosticFact>,
}
#[derive(Debug, Serialize)]
pub struct PublicApi {
    pub status: String,
    pub unchanged_count: usize,
    pub changed_symbols: Vec<String>,
}
#[derive(Debug, Default, Serialize)]
pub struct Signatures {
    pub added: Vec<Change<Value>>,
    pub removed: Vec<Change<Value>>,
    pub changed: Vec<Change<Value>>,
}
impl Signatures {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
    pub fn len(&self) -> usize {
        self.added.len() + self.removed.len() + self.changed.len()
    }
    pub fn iter(&self) -> impl Iterator<Item = &Change<Value>> {
        self.added.iter().chain(&self.removed).chain(&self.changed)
    }
}
#[derive(Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub kind: &'static str,
    pub before_revision: String,
    pub after_revision: String,
    pub complete: bool,
    pub signatures: Signatures,
    pub effects: Effects,
    pub affected_callers: Vec<String>,
    pub diagnostics: Delta<DiagnosticFact>,
    pub public_api: PublicApi,
    pub work: Work,
}
fn public_signature(value: Option<&Value>) -> bool {
    value.is_some_and(|v| v["exported"] == true)
}
fn configuration_inputs(root: &Path, inputs: &mut BTreeMap<String, String>) -> Result<()> {
    for name in ["project.toml", "project.lock"] {
        match std::fs::read(root.join(name)) {
            Ok(bytes) => {
                inputs.insert(name.into(), hash(bytes));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}
impl Baseline {
    pub fn verify_manifest(
        &self,
        entry: &Path,
        project: Option<&Path>,
        manifest: &EditManifest,
    ) -> Result<Report> {
        let entry = std::fs::canonicalize(entry)?;
        let root =
            std::fs::canonicalize(project.unwrap_or(entry.parent().context("entry parent")?))?;
        manifest.check(self, &root)?;
        let after = Self::capture(&entry, project)?;
        manifest.check(self, &root)?;
        self.compare(&after)
    }
    pub fn revision(&self) -> &str {
        &self.revision
    }
    pub fn is_complete(&self) -> bool {
        self.complete
    }
    pub fn diagnostics(&self) -> &[DiagnosticFact] {
        &self.diagnostics
    }
    pub(crate) fn capture_overlay(
        entry: &Path,
        project: Option<&Path>,
        sources: BTreeMap<std::path::PathBuf, String>,
        inventory: Option<crate::compiler_db::captured::OverlayInventory>,
    ) -> Result<Self> {
        let _overlay =
            crate::compiler_db::captured::SourceOverlay::with_inventory(sources, inventory);
        Self::capture(entry, project)
    }
    pub fn capture(entry: &Path, project: Option<&Path>) -> Result<Self> {
        let entry = std::fs::canonicalize(entry)?;
        let root =
            std::fs::canonicalize(project.unwrap_or(entry.parent().context("entry parent")?))?;
        let mut diagnostics = Diagnostics::default();
        let checked = crate::CompilerSession::new(
            entry.to_str().context("non UTF-8 entry")?,
            "",
            &crate::CompilerOptions::debug(),
            project.map(Path::to_owned),
        )
        .analysis_for_verification(&mut diagnostics);
        match checked {
            Ok(snapshot) => Self::from_snapshot(&snapshot, diagnostics.facts),
            Err(error) => {
                // Infrastructure failures are not semantic edit results.
                ensure!(
                    diagnostics.facts.iter().any(|d| d.severity == "error"),
                    "{error:#}"
                );
                let mut result = Self {
                    schema_version: 1,
                    compiler: storage::compiler_stamp(),
                    compatibility: String::new(),
                    revision: hash(
                        crate::compiler_db::captured::FileInput::read(entry.clone())?.source,
                    ),
                    complete: false,
                    diagnostics: diagnostics.facts,
                    facts: BTreeMap::new(),
                    inputs: BTreeMap::new(),
                    callers: BTreeMap::new(),
                    index: None,
                    diagnostic_index: None,
                    diagnostic_lookup: BTreeMap::new(),
                    public_count: 0,
                };
                result.inputs.insert(
                    relative(
                        entry.to_str().context("non UTF-8 entry")?,
                        root.to_str().context("non UTF-8 root")?,
                    ),
                    hash(crate::compiler_db::captured::FileInput::read(entry.clone())?.source),
                );
                configuration_inputs(&root, &mut result.inputs)?;
                result.normalize_diagnostics(root.to_str().context("non UTF-8 workspace")?);
                result.reindex()?;
                result.revision = result.digest()?;
                Ok(result)
            }
        }
    }
    fn normalize_diagnostics(&mut self, root: &str) {
        for d in &mut self.diagnostics {
            for location in &mut d.locations {
                location.path = relative(&location.path, root);
            }
        }
        self.diagnostics.sort();
        self.diagnostics.dedup();
    }
    pub(crate) fn from_snapshot(
        snapshot: &Snapshot,
        diagnostics: Vec<DiagnosticFact>,
    ) -> Result<Self> {
        let names = display::TypeNames::new(snapshot);
        let mut facts = BTreeMap::new();
        let mut ids = HashMap::new();
        let mut ordinals = BTreeMap::new();
        let mut declarations: Vec<_> = snapshot
            .semantic
            .symbols
            .iter()
            .filter(|s| {
                matches!(
                    s.kind.as_str(),
                    "function"
                        | "method"
                        | "constructor"
                        | "class"
                        | "interface"
                        | "enum"
                        | "variant"
                        | "field"
                        | "static-field"
                        | "const"
                )
            })
            .collect();
        declarations.sort_by_key(|s| s.location.as_ref().map(|l| (&l.path, l.start, l.end)));
        for s in declarations {
            let Some(location) = &s.location else {
                continue;
            };
            if super::bundled_source(&location.path).is_some() {
                continue;
            }
            let name = s.source_name.as_deref().unwrap_or(&s.name);
            let stem = format!(
                "{}::{}::{name}",
                relative(&location.path, &snapshot.workspace),
                s.kind
            );
            let ordinal = ordinals.entry(stem.clone()).or_insert(0usize);
            let key = format!("{stem}#{ordinal}");
            *ordinal += 1;
            ids.insert(s.id.clone(), key.clone());
            let module = s.identity.as_ref().map(|i| i.module.as_str());
            let types = |ts: &[Type]| {
                ts.iter()
                    .map(|t| names.render(t, module))
                    .collect::<Vec<_>>()
            };
            let signature = json!({"kind":s.kind,"type":s.ty.as_ref().map(|ty| names.render(ty,module)),
                "exported":s.details.public_api,"public":s.details.is_public,"protected":s.details.is_protected,
                "static":s.details.is_static,"mutable":s.details.is_mut,"open":s.details.is_open,"override":s.details.is_override,
                "parameters":types(&s.details.parameters),"parameter_modes":s.details.parameter_modes,
                "constraint_parameter":s.details.constraint_parameter,"constraint_alternatives":types(&s.details.constraint_alternatives),
                "constant_value":s.details.constant_value,"async":s.details.is_async,"const":s.details.is_const,"type_parameters":s.details.type_params,
                "extends":types(&s.details.extends),
                "implements":types(&s.details.implements),"payload":types(&s.details.payload)});
            facts.insert(
                key,
                Fact {
                    signature: Some(signature),
                    effects: None,
                    body: String::new(),
                    callees: vec![],
                },
            );
        }
        for f in &snapshot.functions {
            ids.entry(f.id.clone()).or_insert_with(|| {
                format!(
                    "{}::function::{}",
                    relative(&f.module, &snapshot.workspace),
                    f.name
                )
            });
        }
        let mut callers: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        let mut known = BTreeMap::new();
        let mut queue = VecDeque::new();
        for f in &snapshot.functions {
            let key = ids[&f.id].clone();
            let mut bits = if f.unknown { 0 } else { f.runtime_effects };
            if f.unknown {
                for witness in snapshot.semantic.witnesses.get(&f.id).into_iter().flatten() {
                    if matches!(
                        witness["witness"]["kind"].as_str(),
                        Some("runtime-capability" | "panic" | "nonpreemptible" | "wait")
                    ) && witness["witness"]["cause"]["is_conservative"] != true
                    {
                        bits |= witness["effect"].as_u64().unwrap_or(0) as u8;
                    }
                }
            }
            known.insert(key.clone(), bits);
            if bits != 0 {
                queue.push_back(key.clone());
            }
            let mut callees: Vec<_> = f.callees.iter().map(|id| ids[id].clone()).collect();
            callees.sort();
            callees.dedup();
            for callee in &callees {
                callers
                    .entry(callee.clone())
                    .or_default()
                    .insert(key.clone());
            }
            let fact = facts.entry(key).or_insert_with(|| Fact {
                signature: None,
                effects: None,
                body: String::new(),
                callees: vec![],
            });
            fact.body = f.body_fingerprint.clone();
            fact.callees = callees;
            fact.effects = Some(EffectState {
                may: bits,
                unknown: if f.unknown {
                    RuntimeEffects::ALL.bits()
                } else {
                    0
                },
            });
        }
        // Attribute resolved type/member references to their innermost callable
        // with one ordered sweep per source, rather than scanning all functions
        // for each reference. These edges affect edit impact, not effect flow.
        let mut owners: BTreeMap<&str, Vec<(usize, usize, &str)>> = BTreeMap::new();
        for f in &snapshot.functions {
            for location in &f.locations {
                owners.entry(&location.path).or_default().push((
                    location.start,
                    location.end,
                    &ids[&f.id],
                ));
            }
        }
        let mut references: BTreeMap<&str, Vec<_>> = BTreeMap::new();
        for reference in &snapshot.semantic.references {
            if let Some(target) = ids.get(&reference.target) {
                references
                    .entry(&reference.location.path)
                    .or_default()
                    .push((&reference.location, target));
            }
        }
        let mut reference_edges = Vec::new();
        for (path, refs) in &mut references {
            let Some(ranges) = owners.get_mut(path) else {
                continue;
            };
            ranges.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
            refs.sort_by_key(|(location, _)| location.start);
            let mut next = 0;
            let mut stack: Vec<(usize, usize, &str)> = Vec::new();
            for (location, target) in refs {
                while next < ranges.len() && ranges[next].0 <= location.start {
                    let range = ranges[next];
                    next += 1;
                    while stack.last().is_some_and(|r| r.1 <= range.0) {
                        stack.pop();
                    }
                    stack.push(range);
                }
                while stack.last().is_some_and(|r| r.1 < location.end) {
                    stack.pop();
                }
                if let Some((_, _, owner)) = stack.last()
                    && *owner != target.as_str()
                {
                    reference_edges.push(((*owner).to_owned(), (*target).clone()));
                }
            }
        }
        // Each capability can enter a node once, bounding propagation by 8E.
        while let Some(key) = queue.pop_front() {
            let bits = known[&key];
            for caller in callers.get(&key).into_iter().flatten() {
                let old = known.get_mut(caller).expect("caller fact");
                if bits & !*old != 0 {
                    *old |= bits;
                    queue.push_back(caller.clone());
                }
            }
        }
        for (key, bits) in known {
            let effects = facts.get_mut(&key).unwrap().effects.as_mut().unwrap();
            effects.may = bits;
            effects.unknown &= !bits;
        }
        for (owner, target) in reference_edges {
            callers
                .entry(target.clone())
                .or_default()
                .insert(owner.clone());
            facts
                .get_mut(&owner)
                .expect("reference owner")
                .callees
                .push(target);
        }
        for fact in facts.values_mut() {
            fact.callees.sort();
            fact.callees.dedup();
        }
        let mut diagnostics = diagnostics;
        let mut diagnostic_sites: BTreeMap<&str, Vec<_>> = BTreeMap::new();
        for diagnostic in &mut diagnostics {
            for site in &mut diagnostic.locations {
                diagnostic_sites.entry(&site.path).or_default().push((
                    &mut site.owner,
                    site.start,
                    site.end,
                ));
            }
        }
        for (path, sites) in &mut diagnostic_sites {
            let Some(ranges) = owners.get_mut(path) else {
                continue;
            };
            ranges.sort_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
            sites.sort_by_key(|(_, start, _)| *start);
            let mut next = 0;
            let mut stack: Vec<(usize, usize, &str)> = Vec::new();
            for (owner, start, end) in sites {
                while next < ranges.len() && ranges[next].0 <= *start {
                    let range = ranges[next];
                    next += 1;
                    while stack.last().is_some_and(|r| r.1 <= range.0) {
                        stack.pop();
                    }
                    stack.push(range);
                }
                while stack.last().is_some_and(|r| r.1 < *end) {
                    stack.pop();
                }
                if let Some((_, _, key)) = stack.last() {
                    **owner = Some((*key).to_owned());
                }
            }
        }
        let mut result = Self {
            schema_version: 1,
            compiler: snapshot.compiler.clone(),
            compatibility: snapshot.compatibility.clone(),
            revision: snapshot.revision.clone(),
            complete: true,
            diagnostics,
            facts,
            inputs: snapshot
                .sources
                .iter()
                .filter_map(|(path, digest)| {
                    Path::new(path)
                        .strip_prefix(&snapshot.workspace)
                        .ok()
                        .and_then(Path::to_str)
                        .map(|path| (path.replace(std::path::MAIN_SEPARATOR, "/"), digest.clone()))
                })
                .collect(),
            callers,
            index: None,
            diagnostic_index: None,
            diagnostic_lookup: BTreeMap::new(),
            public_count: 0,
        };
        configuration_inputs(Path::new(&snapshot.workspace), &mut result.inputs)?;
        result.normalize_diagnostics(&snapshot.workspace);
        result.reindex()?;
        result.revision = result.digest()?;
        Ok(result)
    }
    fn digest(&self) -> Result<String> {
        hash_serialized(&(
            self.schema_version,
            &self.compiler,
            self.complete,
            &self.facts,
            &self.diagnostics,
            &self.inputs,
        ))
    }
    fn reindex(&mut self) -> Result<()> {
        let mut entries = self
            .facts
            .iter()
            .map(|(k, v)| Ok((hash(k), k.clone(), hash_serialized(&(k, v))?)))
            .collect::<Result<Vec<_>>>()?;
        entries.sort();
        self.index = Node::build(&entries);
        self.public_count = self
            .facts
            .values()
            .filter(|f| public_signature(f.signature.as_ref()))
            .count();
        self.diagnostic_lookup.clear();
        let mut diagnostics = Vec::new();
        let mut occurrences = BTreeMap::<String, usize>::new();
        for (i, diagnostic) in self.diagnostics.iter().enumerate() {
            let sites: Vec<_> = diagnostic
                .locations
                .iter()
                .map(|l| (&l.path, &l.owner, &l.anchor))
                .collect();
            let identity = hash_serialized(&(
                &diagnostic.code,
                &diagnostic.severity,
                &diagnostic.message,
                sites,
            ))?;
            let ordinal = occurrences.entry(identity.clone()).or_default();
            let digest = hash_serialized(&(identity, *ordinal))?;
            *ordinal += 1;
            self.diagnostic_lookup.insert(digest.clone(), i);
            diagnostics.push((digest.clone(), digest.clone(), digest));
        }
        diagnostics.sort();
        diagnostics.dedup();
        self.diagnostic_index = Node::build(&diagnostics);
        Ok(())
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        self.save_with(path, BASELINE_SIZE_LIMIT, std::io::BufWriter::new)
    }
    fn save_with<W: std::io::Write>(
        &self,
        path: &Path,
        limit: usize,
        writer: impl FnOnce(std::fs::File) -> W,
    ) -> Result<()> {
        // Only remove files successfully created by this call, never existing output.
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        let result = (|| -> Result<()> {
            use std::io::Write;
            let mut writer = LimitedWriter {
                inner: writer(file),
                remaining: limit,
            };
            serde_json::to_writer(&mut writer, self)?;
            writer.flush()?;
            Ok(())
        })();
        // The writer (including buffered data and its file handle) is dropped before
        // removal, including on Windows. Preserve cleanup errors rather than hiding them.
        if let Err(error) = result {
            std::fs::remove_file(path).map_err(|cleanup| {
                anyhow::anyhow!("{error}; baseline cleanup failed: {cleanup}")
            })?;
            return Err(error);
        }
        Ok(())
    }
    pub fn load(path: &Path) -> Result<Self> {
        Self::load_with_limit(path, BASELINE_SIZE_LIMIT)
    }
    fn load_with_limit(path: &Path, limit: usize) -> Result<Self> {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        ensure!(
            file.metadata()?.len() <= limit as u64,
            "verification baseline exceeds size limit"
        );
        // Bound reads too: metadata alone cannot protect against concurrent growth.
        // Decode directly to avoid retaining a second baseline-sized byte buffer.
        let mut reader = file.take(limit as u64 + 1);
        let mut result: Self = serde_json::from_reader(std::io::BufReader::new(&mut reader))?;
        ensure!(
            reader.limit() > 0,
            "verification baseline exceeds size limit"
        );
        ensure!(
            result.schema_version == 1 && result.compiler == storage::compiler_stamp(),
            "incompatible verification baseline"
        );
        ensure!(
            result.revision == result.digest()?,
            "corrupt verification baseline digest"
        );
        // Derived indexes are rebuilt once at the untrusted storage boundary.
        result.callers.clear();
        for (key, fact) in &result.facts {
            for callee in &fact.callees {
                ensure!(
                    result.facts.contains_key(callee),
                    "dangling baseline callee"
                );
                result
                    .callers
                    .entry(callee.clone())
                    .or_default()
                    .insert(key.clone());
            }
        }
        result.diagnostics.sort();
        result.diagnostics.dedup();
        result.reindex()?;
        Ok(result)
    }
    pub fn compare(&self, after: &Self) -> Result<Report> {
        ensure!(
            self.compiler == after.compiler,
            "incompatible verification compiler"
        );
        ensure!(
            !self.complete || !after.complete || self.compatibility == after.compatibility,
            "incompatible verification configuration"
        );
        let mut work = Work::default();
        let mut changed = BTreeSet::new();
        if self.complete && after.complete {
            Node::difference(
                self.index.as_ref(),
                after.index.as_ref(),
                &mut changed,
                &mut work,
            );
        }
        let mut signatures = Signatures::default();
        let mut effect_changes = vec![];
        let mut seeds = BTreeSet::new();
        let mut changed_public_before = 0;
        for key in &changed {
            work.compared_symbols += 1;
            let a = self.facts.get(key);
            let b = after.facts.get(key);
            let sa = a.and_then(|f| f.signature.clone());
            let sb = b.and_then(|f| f.signature.clone());
            if sa != sb {
                changed_public_before += usize::from(public_signature(sa.as_ref()));
                let group = if sa.is_none() {
                    &mut signatures.added
                } else if sb.is_none() {
                    &mut signatures.removed
                } else {
                    &mut signatures.changed
                };
                group.push(Change {
                    symbol: key.clone(),
                    before: sa,
                    after: sb,
                });
            }
            let ea = a.and_then(|f| f.effects.clone());
            let eb = b.and_then(|f| f.effects.clone());
            if ea != eb {
                effect_changes.push(Change {
                    symbol: key.clone(),
                    before: ea,
                    after: eb,
                });
            }
            if a.map(|f| (&f.signature, &f.body, &f.callees))
                != b.map(|f| (&f.signature, &f.body, &f.callees))
            {
                seeds.insert(key.clone());
            }
        }
        let mut reached = seeds.clone();
        let mut queue: VecDeque<_> = seeds.iter().cloned().collect();
        while let Some(key) = queue.pop_front() {
            for index in [&self.callers, &after.callers] {
                for caller in index.get(&key).into_iter().flatten() {
                    work.caller_edges += 1;
                    if reached.insert(caller.clone()) {
                        queue.push_back(caller.clone());
                    }
                }
            }
        }
        let mut changed_diagnostics = BTreeSet::new();
        if self.diagnostic_index.is_some() || after.diagnostic_index.is_some() {
            Node::difference(
                self.diagnostic_index.as_ref(),
                after.diagnostic_index.as_ref(),
                &mut changed_diagnostics,
                &mut work,
            );
        }
        let mut added = Vec::new();
        let mut removed = Vec::new();
        for key in changed_diagnostics {
            if let Some(&i) = self.diagnostic_lookup.get(&key) {
                removed.push(self.diagnostics[i].clone());
            }
            if let Some(&i) = after.diagnostic_lookup.get(&key) {
                added.push(after.diagnostics[i].clone());
            }
        }
        added.sort();
        removed.sort();
        let complete = self.complete && after.complete;
        let changed_public: BTreeSet<_> = signatures
            .iter()
            .filter(|c| public_signature(c.before.as_ref()) || public_signature(c.after.as_ref()))
            .map(|c| c.symbol.clone())
            .collect();
        Ok(Report {
            schema_version: 1,
            kind: "semantic-difference",
            before_revision: self.revision.clone(),
            after_revision: after.revision.clone(),
            complete,
            public_api: PublicApi {
                status: if !complete {
                    "unknown"
                } else if changed_public.is_empty() {
                    "unchanged"
                } else {
                    "changed"
                }
                .into(),
                unchanged_count: if complete {
                    self.public_count - changed_public_before
                } else {
                    0
                },
                changed_symbols: changed_public.into_iter().collect(),
            },
            signatures,
            effects: Effects {
                changed: effect_changes,
                new_e2604: added
                    .iter()
                    .filter(|d| d.code == "E2604")
                    .cloned()
                    .collect(),
                newly_non_awaitable: added
                    .iter()
                    .filter(|d| {
                        d.code == "E0803"
                            && (d.message.starts_with("cannot await value")
                                || d.message
                                    == "synchronous filesystem operations cannot be awaited")
                    })
                    .cloned()
                    .collect(),
            },
            affected_callers: reached.difference(&seeds).cloned().collect(),
            diagnostics: Delta { added, removed },
            work,
        })
    }
}

#[cfg(test)]
#[path = "verify_tests.rs"]
mod tests;
