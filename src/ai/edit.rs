//! Durable structured edits. The workspace lock defines the observation boundary:
//! cooperating operations exclude one another while the lease is held. Ordinary
//! filesystem readers can observe intermediate files. After interruption, edit
//! operations refuse to proceed until explicit recovery; committed journals retain
//! the new state, otherwise recovery restores old bytes unless a conflict is found.
//! Non-cooperating writers have a check/replacement race; see STRUCTURED_EDITS.md.
use super::{Snapshot, hash};
use crate::ai::SymbolKind;
use crate::qualified_name::QualifiedName;
use crate::{
    CompilerOptions, CompilerSession,
    diagnostics::{Diagnostic, DiagnosticEmitter, FileId, SourceMap, source_map::SourceLookup},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File},
    path::{Path, PathBuf},
};

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Operation {
    Rename { function: String, name: String },
    ReplaceBody { function: String, body: String },
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub revision: String,
    pub operations: Vec<Operation>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanState {
    Prepared,
    Validated,
    Applying,
    Applied,
    Aborted,
    Unknown(String),
}
impl PlanState {
    fn as_str(&self) -> &str {
        match self {
            Self::Prepared => "prepared",
            Self::Validated => "validated",
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::Aborted => "aborted",
            Self::Unknown(s) => s,
        }
    }
}
impl Serialize for PlanState {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}
impl<'de> Deserialize<'de> for PlanState {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Ok(match String::deserialize(deserializer)?.as_str() {
            "prepared" => Self::Prepared,
            "validated" => Self::Validated,
            "applying" => Self::Applying,
            "applied" => Self::Applied,
            "aborted" => Self::Aborted,
            s => Self::Unknown(s.to_owned()),
        })
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct DiffChange {
    pub path: String,
    pub diff: String,
}
#[derive(Serialize)]
pub struct RenamePreview {
    pub status: super::query_result::Status,
    pub files_changed: usize,
    pub work: EditWork,
    pub dry_run: bool,
    pub revision: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<DiffChange>>,
}
#[derive(Serialize)]
#[serde(untagged)]
pub enum PreviewChanges {
    Full(Vec<Change>),
    Diff(Vec<DiffChange>),
}
impl PreviewChanges {
    pub fn len(&self) -> usize {
        match self {
            Self::Full(v) => v.len(),
            Self::Diff(v) => v.len(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
#[derive(Serialize)]
pub struct PreparedPreview {
    pub kind: &'static str,
    pub transaction: String,
    pub revision: String,
    pub candidate: String,
    pub state: PlanState,
    pub changes: PreviewChanges,
    pub work: EditWork,
    pub verification_manifest: Option<super::verify::EditManifest>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    path: String,
    before: String,
    after: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    offsets: Vec<(usize, usize, usize, usize)>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    version: u32,
    compiler: String,
    configuration: String,
    entry: String,
    project: bool,
    revision: String,
    inputs: BTreeMap<String, String>,
    changes: Vec<Change>,
    state: PlanState,
    candidate: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verification_baseline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    verification_modules: Option<BTreeMap<String, String>>,
    absent: Vec<String>,
    work: EditWork,
}

impl Plan {
    fn verification_manifest(&self) -> Option<super::verify::EditManifest> {
        Some(super::verify::EditManifest {
            schema_version: 1,
            baseline_revision: self.verification_baseline.clone()?,
            complete: true,
            changes: self
                .changes
                .iter()
                .map(|change| super::verify::InputChange {
                    path: change.path.replace(std::path::MAIN_SEPARATOR, "/"),
                    before: Some(hash(&change.before)),
                    after: Some(hash(&change.after)),
                })
                .collect(),
        })
    }
}

/// How `edit.preview` reports changed files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChangeFormat {
    /// Full `before`/`after` text of every changed file.
    #[default]
    Full,
    /// One unified diff (`diff -u`, three context lines) per changed file.
    Diff,
}

/// Exclusive cooperating-client lease for structured edits. Opening does not
/// recover an interrupted AI edit: call `recover` explicitly before other edit
/// operations. This is not a multi-file filesystem snapshot or mandatory lock.
pub struct Workspace {
    root: PathBuf,
    directory: PathBuf,
    _transaction: crate::workspace_transaction::WorkspaceFileTransaction,
}

#[derive(Serialize, Deserialize)]
pub(super) struct RenameHint {
    pub selector: String,
    pub path: String,
    pub start: usize,
    pub digest: String,
}
impl RenameHint {
    fn read_all(root: &Path) -> Vec<Self> {
        // Advisory metadata must never make an otherwise valid query fail.
        // Accept the original single-record format for existing workspaces.
        let Ok(file) = File::open(root.join(".willow-edits/last-local-rename.json")) else {
            return Vec::new();
        };
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Records {
            Many(Vec<RenameHint>),
            One(RenameHint),
        }
        match serde_json::from_reader(std::io::BufReader::new(file)) {
            Ok(Records::Many(hints)) => hints,
            Ok(Records::One(hint)) => vec![hint],
            Err(_) => Vec::new(),
        }
    }
    pub fn read(snapshot: &Snapshot) -> Vec<Self> {
        Self::read_all(Path::new(&snapshot.workspace))
            .into_iter()
            .filter(|hint| snapshot.sources.get(&hint.path) == Some(&hint.digest))
            .collect()
    }
}

/// Live analysis inputs, deliberately absent from persisted snapshots. Structured
/// edits need a current checked frontend, including its original configuration.
#[derive(Debug, Clone)]
pub(crate) struct AnalysisInputs {
    entry: PathBuf,
    root: PathBuf,
    project: bool,
    manifest_entry: Option<PathBuf>,
    configuration: String,
    files: BTreeMap<String, Option<String>>,
    pub(crate) diagnostics: Vec<super::verify::DiagnosticFact>,
    pub(crate) check_modules: BTreeMap<String, PathBuf>,
}
impl AnalysisInputs {
    pub(crate) fn capture(
        entry: &Path,
        project: Option<&Path>,
        options: &CompilerOptions,
    ) -> Result<Self> {
        let root = fs::canonicalize(project.unwrap_or(entry.parent().context("entry parent")?))?;
        let mut files = BTreeMap::new();
        let mut manifest_entry = None;
        for name in [
            "project.toml",
            "project.lock",
            "rust-review.toml",
            ".willow/rust/Cargo.lock",
        ] {
            files.insert(
                name.into(),
                match fs::read(root.join(name)) {
                    Ok(bytes) => {
                        if name == "project.toml" && project.is_some() {
                            let manifest: crate::project::ProjectManifest =
                                toml::from_str(std::str::from_utf8(&bytes)?)?;
                            manifest_entry = Some(manifest.entry_point(&root));
                        }
                        Some(hash(bytes))
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => return Err(error.into()),
                },
            );
        }
        Ok(Self {
            entry: entry.to_owned(),
            root,
            project: project.is_some(),
            manifest_entry,
            configuration: hash(format!("{options:?}|{}", target_lexicon::HOST)),
            files,
            diagnostics: Vec::new(),
            check_modules: BTreeMap::new(),
        })
    }
    pub(crate) fn verify_files(&self) -> Result<()> {
        for (name, expected) in &self.files {
            let actual = match fs::read(self.root.join(name)) {
                Ok(bytes) => Some(hash(bytes)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            ensure!(
                &actual == expected,
                "stale analysis configuration: {name} changed"
            );
        }
        Ok(())
    }
    pub(super) fn verify_manifest_entry(&self) -> Result<()> {
        if let Some(entry) = &self.manifest_entry {
            ensure!(
                fs::canonicalize(entry)? == self.entry,
                "analysis entry differs from current project manifest"
            );
        }
        self.verify_files()
    }
    fn verify_edit(&self, entry: &Path, root: &Path, project: bool) -> Result<()> {
        ensure!(
            self.entry == entry
                && self.project == project
                && if project {
                    self.root == root
                } else {
                    self.entry.starts_with(root)
                },
            "analysis entry/workspace/mode mismatch"
        );
        ensure!(
            self.configuration == configuration(),
            "analysis options/environment changed"
        );
        self.verify_files()
    }
}
impl Workspace {
    pub fn open(root: &Path) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let directory = root.join(".willow-edits");
        // The shared transaction owns metadata creation, symlink validation,
        // and parent-directory synchronization before publication.
        let transaction = crate::workspace_transaction::WorkspaceFileTransaction::try_open(&root)?;
        Ok(Self {
            root,
            directory,
            _transaction: transaction,
        })
    }
    /// Advisory history, never an alias used for resolution. Refresh locations
    /// only across verified edits; unrelated source changes invalidate hints.
    pub(super) fn remember_rename(
        &self,
        transaction: &str,
        selector: &str,
        location: &super::Location,
    ) -> Result<()> {
        let plan = self.load(transaction)?;
        let mut hints = RenameHint::read_all(&self.root);
        let changes: BTreeMap<_, _> = plan
            .changes
            .iter()
            .map(|change| {
                (
                    self.root.join(&change.path).to_string_lossy().into_owned(),
                    (change, hash(&change.before), hash(&change.after)),
                )
            })
            .collect();
        let (_, before, _) = changes
            .get(&location.path)
            .context("renamed source is absent from edit")?;
        hints.push(RenameHint {
            selector: selector.into(),
            path: location.path.clone(),
            start: location.start,
            digest: before.clone(),
        });
        hints.retain_mut(|hint| {
            let Some((change, before, after)) = changes.get(&hint.path) else {
                return plan.inputs.get(
                    Path::new(&hint.path)
                        .strip_prefix(&self.root)
                        .ok()
                        .and_then(Path::to_str)
                        .unwrap_or(""),
                ) == Some(&hint.digest);
            };
            if &hint.digest != before {
                return false;
            }
            let index = change
                .offsets
                .partition_point(|&(start, _, _, _)| start <= hint.start);
            if let Some(&(start, end, new_start, new_end)) =
                index.checked_sub(1).map(|i| &change.offsets[i])
            {
                if hint.start == start {
                    hint.start = new_start;
                } else if hint.start >= end {
                    hint.start = new_end + (hint.start - end);
                } else {
                    return false;
                }
            }
            hint.digest.clone_from(after);
            true
        });
        durable_write(
            &self.directory.join("last-local-rename.json"),
            &serde_json::to_vec(&hints)?,
        )
    }

    fn file(&self, relative: &str) -> Result<PathBuf> {
        checked_file(&self.root, relative)
    }
    fn plan_path(&self, id: &str) -> Result<PathBuf> {
        ensure!(
            id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid transaction id"
        );
        Ok(self.directory.join(format!("{id}.json")))
    }
}

fn checked_file(root: &Path, relative: &str) -> Result<PathBuf> {
    let path = Path::new(relative);
    ensure!(
        !path.as_os_str().is_empty()
            && path
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
        "invalid workspace-relative path"
    );
    ensure!(
        path.components().next().unwrap().as_os_str() != ".willow-edits",
        "local metadata cannot be an input"
    );
    let full = root.join(path);
    ensure!(
        fs::canonicalize(&full)? == full,
        "symlink input is not editable"
    );
    ensure!(
        fs::metadata(&full)?.is_file(),
        "input is not a regular file"
    );
    Ok(full)
}
impl Workspace {
    fn load(&self, id: &str) -> Result<Plan> {
        let plan: Plan = serde_json::from_slice(&fs::read(self.plan_path(id)?)?)?;
        ensure!(plan.version == 1, "unsupported edit plan");
        ensure!(
            plan.candidate == candidate(&plan)?,
            "candidate integrity failure"
        );
        Ok(plan)
    }
    fn store(&self, id: &str, plan: &Plan) -> Result<()> {
        durable_write(&self.plan_path(id)?, &serde_json::to_vec(plan)?)
    }
    fn current(&self, plan: &Plan) -> Result<()> {
        for (path, expected) in &plan.inputs {
            ensure!(
                hash(fs::read(self.file(path)?)?) == *expected,
                "stale transaction: {path}"
            );
        }
        for path in &plan.absent {
            ensure!(
                !self.root.join(path).exists(),
                "stale transaction: added {path}"
            );
        }
        Ok(())
    }
    /// Resolve source coordinates once against the checked base revision, then
    /// assemble each file in a single forward pass. Overlapping edits are errors.
    pub fn prepare(
        &self,
        entry: &Path,
        project: bool,
        request: Request,
        format: ChangeFormat,
        emitter: &mut dyn DiagnosticEmitter,
    ) -> Result<serde_json::Value> {
        self.ensure_recovered()?;
        let entry = fs::canonicalize(entry)?;
        let relative = entry
            .strip_prefix(&self.root)
            .context("entry outside workspace")?
            .to_str()
            .context("non UTF-8 entry")?;
        self.file(relative)?;
        let snapshot = self.analyze(&entry, project, emitter)?;
        self.prepare_analyzed(&entry, project, request, format, snapshot)
    }
    pub(super) fn prepare_analyzed(
        &self,
        entry: &Path,
        project: bool,
        request: Request,
        format: ChangeFormat,
        snapshot: Snapshot,
    ) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self.prepare_analyzed_typed(
            entry, project, request, format, snapshot,
        )?)?)
    }
    pub(super) fn prepare_analyzed_typed(
        &self,
        entry: &Path,
        project: bool,
        request: Request,
        format: ChangeFormat,
        snapshot: Snapshot,
    ) -> Result<PreparedPreview> {
        self.ensure_recovered()?;
        let entry = fs::canonicalize(entry)?;
        let context = snapshot
            .edit_context
            .as_ref()
            .context("edit requires live analysis inputs")?;
        context.verify_edit(&entry, &self.root, project)?;
        let relative = entry
            .strip_prefix(&self.root)
            .context("entry outside workspace")?
            .to_str()
            .context("non UTF-8 entry")?
            .to_owned();
        self.file(&relative)?;
        ensure!(
            snapshot.revision == request.revision,
            "base revision mismatch: source, configuration, or analysis mode differs; \
             use --project for a query of a project directory, omit --project \
             for a query of a source file; if inputs changed, query the current source again \
             and rebuild the edit request"
        );
        ensure!(!request.operations.is_empty(), "empty transaction");
        let mut sources = BTreeMap::new();
        let mut inputs = BTreeMap::new();
        for (path, expected) in &snapshot.sources {
            if verified_bundled_source(path, expected)? {
                continue;
            }
            let relative = Path::new(path)
                .strip_prefix(&self.root)
                .context("external source is not supported by isolated edits")?
                .to_str()
                .context("non UTF-8 source")?
                .to_owned();
            let source = fs::read_to_string(self.file(&relative)?)?;
            ensure!(hash(&source) == *expected, "source changed during analysis");
            inputs.insert(relative.clone(), expected.clone());
            sources.insert(relative, source);
        }
        let mut absent = Vec::new();
        for name in [
            "project.toml",
            "project.lock",
            "rust-review.toml",
            ".willow/rust/Cargo.lock",
        ] {
            if self.root.join(name).exists() {
                let bytes = fs::read(self.file(name)?)?;
                inputs.insert(name.into(), hash(&bytes));
                sources.insert(name.into(), String::from_utf8(bytes)?);
            } else {
                absent.push(name.into());
            }
        }
        context.verify_files()?;
        let (changes, work) =
            structured_changes(&snapshot, &self.root, &sources, request.operations)?;
        ensure!(!changes.is_empty(), "transaction makes no changes");
        let baseline =
            super::verify::Baseline::from_snapshot(&snapshot, context.diagnostics.clone())?;
        let mut plan = Plan {
            version: 1,
            compiler: super::storage::compiler_stamp(),
            configuration: configuration(),
            entry: relative,
            project,
            revision: snapshot.revision,
            inputs,
            changes,
            state: PlanState::Prepared,
            candidate: String::new(),
            verification_baseline: Some(baseline.revision().to_owned()),
            verification_modules: Some(
                context
                    .check_modules
                    .iter()
                    .map(|(name, path)| {
                        Ok((
                            name.clone(),
                            path.strip_prefix(&self.root)?
                                .to_str()
                                .context("non UTF-8 module path")?
                                .to_owned(),
                        ))
                    })
                    .collect::<Result<_>>()?,
            ),
            absent,
            work,
        };
        plan.candidate = candidate(&plan)?;
        let id = hash(format!(
            "{}:{:?}:{}",
            plan.candidate,
            std::time::SystemTime::now(),
            std::process::id()
        ));
        self.current(&plan)?;
        let sandbox = self.directory.join(&id);
        fs::create_dir(&sandbox)?;
        for (path, source) in sources {
            let destination = sandbox.join(&path);
            fs::create_dir_all(destination.parent().unwrap())?;
            fs::write(destination, source)?;
        }
        for change in &plan.changes {
            fs::write(sandbox.join(&change.path), &change.after)?;
        }
        baseline.save(&self.directory.join(format!("{id}.baseline.json")))?;
        self.store(&id, &plan)?;
        self.preview_typed(&id, format)
    }
    fn analyze(
        &self,
        entry: &Path,
        project: bool,
        emitter: &mut dyn DiagnosticEmitter,
    ) -> Result<Snapshot> {
        CompilerSession::new(
            entry.to_str().context("non UTF-8 entry")?,
            "",
            &CompilerOptions::debug(),
            project.then(|| self.root.clone()),
        )
        .analysis_for_edit_with_emitter(emitter)
    }
    pub fn preview(&self, id: &str, format: ChangeFormat) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self.preview_typed(id, format)?)?)
    }
    fn preview_typed(&self, id: &str, format: ChangeFormat) -> Result<PreparedPreview> {
        self.ensure_recovered()?;
        let plan = self.load(id)?;
        let verification_manifest = plan.verification_manifest();
        let changes = match format {
            ChangeFormat::Full => PreviewChanges::Full(plan.changes),
            ChangeFormat::Diff => PreviewChanges::Diff(
                plan.changes
                    .iter()
                    .map(|c| DiffChange {
                        path: c.path.clone(),
                        diff: super::diff::unified(&c.path, &c.before, &c.after),
                    })
                    .collect(),
            ),
        };
        Ok(PreparedPreview {
            kind: "edit.preview",
            transaction: id.to_owned(),
            revision: plan.revision,
            candidate: plan.candidate,
            state: plan.state,
            changes,
            work: plan.work,
            verification_manifest,
        })
    }
    fn check_candidate(&self, id: &str, plan: &Plan) -> Result<()> {
        let replacements: BTreeMap<_, _> = plan
            .changes
            .iter()
            .map(|c| (c.path.as_str(), c.after.as_str()))
            .collect();
        let sandbox = self.directory.join(id);
        ensure!(
            fs::canonicalize(&sandbox)? == sandbox,
            "candidate directory symlink"
        );
        let mut pending = vec![sandbox.clone()];
        let mut files = std::collections::BTreeSet::new();
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let kind = entry.file_type()?;
                ensure!(!kind.is_symlink(), "candidate symlink");
                if kind.is_dir() {
                    pending.push(entry.path());
                } else {
                    ensure!(kind.is_file(), "candidate special file");
                    files.insert(
                        entry
                            .path()
                            .strip_prefix(&sandbox)?
                            .to_str()
                            .context("non UTF-8 candidate")?
                            .to_owned(),
                    );
                }
            }
        }
        ensure!(
            files.len() == plan.inputs.len() && files.iter().all(|p| plan.inputs.contains_key(p)),
            "candidate file inventory changed"
        );
        for (path, expected) in &plan.inputs {
            let full = self.directory.join(id).join(path);
            ensure!(fs::canonicalize(&full)? == full, "candidate symlink");
            let actual = hash(fs::read(full)?);
            ensure!(
                actual
                    == replacements
                        .get(path.as_str())
                        .map_or_else(|| expected.clone(), hash),
                "candidate modified after preview: {path}"
            );
        }
        Ok(())
    }
    /// Read-only semantic verification of a prepared edit, including rejected
    /// candidates. Does not validate or apply the transaction.
    pub fn verify(&self, id: &str) -> Result<super::verify::Report> {
        self.verify_with_manifest(id, None, None)
    }

    /// Verify exactly the prepared changes using a fresh caller attestation.
    /// Unlisted original and candidate inputs being unchanged is the caller's
    /// guarantee. Validation and application retain exhaustive integrity checks.
    pub fn verify_manifest(
        &self,
        id: &str,
        manifest: &super::verify::EditManifest,
    ) -> Result<super::verify::Report> {
        manifest.validate()?;
        self.verify_with_manifest(id, Some(manifest), None)
    }

    /// Verify a transaction using caller-owned retained analysis.
    pub fn verify_manifest_retained(
        &self,
        id: &str,
        manifest: &super::verify::EditManifest,
        revision: &mut crate::compiler_db::revision::AnalysisRevision,
    ) -> Result<super::verify::Report> {
        manifest.validate()?;
        self.verify_with_manifest(id, Some(manifest), Some(revision))
    }

    fn current_declared(&self, manifest: &super::verify::EditManifest) -> Result<()> {
        for change in &manifest.changes {
            ensure!(
                Some(hash(fs::read(self.file(&change.path)?)?)) == change.before,
                "stale transaction: {}",
                change.path
            );
        }
        Ok(())
    }

    fn verify_with_manifest(
        &self,
        id: &str,
        supplied: Option<&super::verify::EditManifest>,
        mut revision: Option<&mut crate::compiler_db::revision::AnalysisRevision>,
    ) -> Result<super::verify::Report> {
        self.ensure_recovered()?;
        let plan = self.load(id)?;
        ensure!(
            matches!(plan.state, PlanState::Prepared | PlanState::Validated),
            "transaction is not pending"
        );
        ensure!(
            plan.compiler == super::storage::compiler_stamp(),
            "compiler changed"
        );
        ensure!(
            plan.configuration == configuration(),
            "compiler options/environment changed"
        );
        if supplied.is_none() {
            self.current(&plan)?;
            self.check_candidate(id, &plan)?;
        }
        let expected = plan
            .verification_baseline
            .as_deref()
            .context("transaction predates retained verification baselines; prepare it again")?;
        let before =
            super::verify::Baseline::load(&self.directory.join(format!("{id}.baseline.json")))?;
        ensure!(
            before.revision() == expected,
            "transaction baseline mismatch"
        );
        let prepared = plan
            .verification_manifest()
            .context("missing verification baseline")?;
        let manifest = supplied.unwrap_or(&prepared);
        if supplied.is_some() {
            ensure!(
                manifest.baseline_revision == expected,
                "transaction manifest baseline mismatch"
            );
            ensure!(
                manifest.changes.len() == prepared.changes.len(),
                "manifest must contain all prepared changes"
            );
            let changes: BTreeMap<_, _> = prepared
                .changes
                .iter()
                .map(|change| (&change.path, change))
                .collect();
            for change in &manifest.changes {
                let expected = changes
                    .get(&change.path)
                    .context("manifest contains an unprepared change")?;
                ensure!(
                    change.before == expected.before && change.after == expected.after,
                    "manifest differs from prepared change: {}",
                    change.path
                );
            }
            self.current_declared(manifest)?;
        }
        let candidate_root = self.directory.join(id);
        manifest.check(&before, &candidate_root)?;
        // Keep original physical identities for compiler cache keys. Only the
        // manifest's changed source bytes are overlaid. A supplied manifest
        // attests to unlisted inputs; the legacy path checks the full inventory.
        let modules = plan
            .verification_modules
            .as_ref()
            .context("transaction predates retained source inventory; prepare it again")?;
        let overlay = crate::compiler_db::captured::SourceOverlay::with_inventory(
            plan.changes
                .iter()
                .map(|change| (self.root.join(&change.path), change.after.clone()))
                .collect(),
            Some(crate::compiler_db::captured::OverlayInventory {
                root: self.root.clone(),
                files: plan
                    .inputs
                    .keys()
                    .map(|path| self.root.join(path))
                    .collect(),
                modules: modules
                    .iter()
                    .map(|(name, path)| (name.clone(), self.root.join(path)))
                    .collect(),
            }),
        );
        let after = super::verify::Baseline::capture_with_revision(
            &self.root.join(&plan.entry),
            plan.project.then_some(self.root.as_path()),
            revision.as_deref_mut(),
        )?;
        drop(overlay);
        if supplied.is_none() {
            self.current(&plan)?;
            self.check_candidate(id, &plan)?;
        }
        if supplied.is_some() {
            self.current_declared(manifest)?;
            manifest.check(&before, &candidate_root)?;
        }
        let started = std::time::Instant::now();
        let result = before.compare(&after);
        if let Some(revision) = revision {
            revision.incremental_stats["report_compare_us"] =
                serde_json::json!(started.elapsed().as_micros());
        }
        result
    }

    pub fn validate(
        &self,
        id: &str,
        emitter: &mut dyn DiagnosticEmitter,
    ) -> Result<serde_json::Value> {
        self.ensure_recovered()?;
        let mut plan = self.load(id)?;
        ensure!(
            matches!(plan.state, PlanState::Prepared | PlanState::Validated),
            "transaction is not pending"
        );
        self.current(&plan)?;
        ensure!(
            plan.compiler == super::storage::compiler_stamp(),
            "compiler changed"
        );
        ensure!(
            plan.configuration == configuration(),
            "compiler options/environment changed"
        );
        self.check_candidate(id, &plan)?;
        let root = self.directory.join(id);
        let checked = CompilerSession::new(
            root.join(&plan.entry).to_str().context("non UTF-8 entry")?,
            "",
            &CompilerOptions::debug(),
            plan.project.then_some(root.clone()),
        )
        .analysis_for_edit_with_emitter(&mut CandidatePaths::new(emitter, &root))?;
        for (path, digest) in &checked.sources {
            if verified_bundled_source(path, digest)? {
                continue;
            }
            let relative = Path::new(path)
                .strip_prefix(&root)
                .context("candidate imported a source outside isolation")?
                .to_str()
                .context("non UTF-8 candidate")?;
            ensure!(
                plan.inputs.contains_key(relative),
                "candidate added an unchecked input"
            );
            ensure!(
                hash(fs::read(root.join(relative))?) == *digest,
                "candidate changed during validation"
            );
        }
        self.check_candidate(id, &plan)?;
        self.current(&plan)?;
        plan.state = PlanState::Validated;
        self.store(id, &plan)?;
        Ok(
            serde_json::json!({"kind":"edit.validated", "transaction":id, "candidate":plan.candidate}),
        )
    }
    pub fn apply(
        &self,
        id: &str,
        emitter: &mut dyn DiagnosticEmitter,
    ) -> Result<serde_json::Value> {
        self.apply_inner(id, None, emitter)
    }
    fn apply_inner(
        &self,
        id: &str,
        fail_after: Option<usize>,
        emitter: &mut dyn DiagnosticEmitter,
    ) -> Result<serde_json::Value> {
        self.apply_observed(id, fail_after, None, &mut 0, emitter)
    }
    pub(super) fn apply_with_rollback(
        &self,
        id: &str,
        emitter: &mut dyn DiagnosticEmitter,
    ) -> Result<serde_json::Value> {
        self.apply_rollback_observed(id, None, emitter)
    }
    fn apply_rollback_observed(
        &self,
        id: &str,
        observe: Option<&dyn Fn(Option<usize>) -> Result<()>>,
        emitter: &mut dyn DiagnosticEmitter,
    ) -> Result<serde_json::Value> {
        let mut written = 0;
        let result = self.apply_observed(id, None, observe, &mut written, emitter);
        if result.is_err()
            && (written > 0 || self.directory.join("active").exists())
            && let Err(rollback) = self.rollback_owned(id, written)
        {
            return result.with_context(|| {
                format!("rollback failed: {rollback:#}; Recovery transaction: {id}")
            });
        }
        result
    }
    fn rollback_owned(&self, id: &str, written: usize) -> Result<()> {
        match fs::read_to_string(self.directory.join("active")) {
            Ok(active) => ensure!(active == id, "different active transaction"),
            Err(error) if written > 0 && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let mut plan = self.load(id)?;
        // A failure after the success marker was removed still needs an undo
        // journal if rollback itself is interrupted or encounters an IO error.
        plan.state = PlanState::Applying;
        self.store(id, &plan)?;
        if !self.directory.join("active").exists() {
            durable_write(&self.directory.join("active"), id.as_bytes())?;
        }
        let mut failures = Vec::new();
        for change in plan.changes.iter().take(written) {
            let reverted = (|| -> Result<()> {
                let path = self.file(&change.path)?;
                let current = fs::read_to_string(&path)?;
                // A third-party change is preserved; rollback only our exact bytes.
                if current == change.after {
                    durable_write(&path, change.before.as_bytes())?;
                }
                Ok(())
            })();
            if let Err(error) = reverted {
                failures.push(format!("{}: {error:#}", change.path));
            }
        }
        ensure!(failures.is_empty(), "{}", failures.join("; "));
        plan.state = PlanState::Aborted;
        self.store(id, &plan)?;
        if self.directory.join("active").exists() {
            self.clear_active()?;
        }
        Ok(())
    }
    fn apply_observed(
        &self,
        id: &str,
        fail_after: Option<usize>,
        observe: Option<&dyn Fn(Option<usize>) -> Result<()>>,
        written: &mut usize,
        emitter: &mut dyn DiagnosticEmitter,
    ) -> Result<serde_json::Value> {
        self.ensure_recovered()?;
        let mut plan = self.load(id)?;
        ensure!(
            plan.state == PlanState::Validated,
            "apply requires a validated, unapplied candidate"
        );
        ensure!(
            plan.compiler == super::storage::compiler_stamp(),
            "compiler changed"
        );
        ensure!(
            plan.configuration == configuration(),
            "compiler options/environment changed"
        );
        self.check_candidate(id, &plan)?;
        self.current(&plan)?;
        // Persist all undo data before the first workspace write. The applying
        // state is also the recovery marker if this process dies mid-operation.
        durable_write(&self.directory.join("active"), id.as_bytes())?;
        #[cfg(test)]
        visibility_tests::checkpoint("intent");
        plan.state = PlanState::Applying;
        self.store(id, &plan)?;
        #[cfg(test)]
        visibility_tests::checkpoint("applying");
        for (i, change) in plan.changes.iter().enumerate() {
            if let Some(hook) = observe {
                hook(Some(i))?;
            }
            if fail_after == Some(i) {
                anyhow::bail!("injected interrupted write");
            }
            let path = self.file(&change.path)?;
            ensure!(
                fs::read_to_string(&path)? == change.before,
                "concurrent edit: {}",
                change.path
            );
            durable_write_observed(&path, change.after.as_bytes(), || *written += 1)?;
        }
        #[cfg(test)]
        visibility_tests::checkpoint("written");
        let snapshot = self.analyze(&self.root.join(&plan.entry), plan.project, emitter)?;
        let replacements: BTreeMap<_, _> = plan
            .changes
            .iter()
            .map(|c| (c.path.as_str(), hash(&c.after)))
            .collect();
        for (path, expected) in &plan.inputs {
            let expected = replacements.get(path.as_str()).unwrap_or(expected);
            ensure!(
                hash(fs::read(self.file(path)?)?) == *expected,
                "post-apply source changed: {path}"
            );
        }
        for path in &plan.absent {
            ensure!(
                !self.root.join(path).exists(),
                "post-apply input added: {path}"
            );
        }
        for (path, digest) in &snapshot.sources {
            if verified_bundled_source(path, digest)? {
                continue;
            }
            let relative = Path::new(path)
                .strip_prefix(&self.root)?
                .to_str()
                .context("non UTF-8 source")?;
            let expected = replacements
                .get(relative)
                .or_else(|| plan.inputs.get(relative))
                .context("post-apply imported an unexpected source")?;
            ensure!(
                digest == expected,
                "post-apply analysis observed different input bytes"
            );
        }
        plan.state = PlanState::Applied;
        self.store(id, &plan)?;
        #[cfg(test)]
        visibility_tests::checkpoint("committed");
        self.clear_active()?;
        if let Some(hook) = observe {
            hook(None)?;
        }
        Ok(
            serde_json::json!({"kind":"edit.applied", "transaction":id, "candidate":plan.candidate, "revision":snapshot.revision}),
        )
    }
    pub fn ensure_recovered(&self) -> Result<()> {
        ensure!(
            !self.directory.join("active").exists(),
            "interrupted transaction requires edit recover"
        );
        Ok(())
    }
    fn clear_active(&self) -> Result<()> {
        fs::remove_file(self.directory.join("active"))?;
        sync_directory(&self.directory)?;
        Ok(())
    }
    pub fn recover(&self, id: &str) -> Result<serde_json::Value> {
        let mut plan = self.load(id)?;
        ensure!(
            fs::read_to_string(self.directory.join("active"))? == id,
            "different active transaction"
        );
        ensure!(
            matches!(
                plan.state,
                PlanState::Validated
                    | PlanState::Applying
                    | PlanState::Applied
                    | PlanState::Aborted
            ),
            "invalid recovery state"
        );
        if matches!(plan.state, PlanState::Applied | PlanState::Aborted) {
            self.clear_active()?;
            return Ok(
                serde_json::json!({"kind":"edit.recovered", "transaction":id, "state":plan.state}),
            );
        }
        // Preflight the entire set before touching any file. Preserve unrelated
        // bytes on conflict, including changes made after an interrupted apply.
        for change in &plan.changes {
            let current = fs::read_to_string(self.file(&change.path)?)?;
            ensure!(
                current == change.before || current == change.after,
                "recovery conflict: {} (preserved)",
                change.path
            );
        }
        for (index, change) in plan.changes.iter().enumerate() {
            let path = self.file(&change.path)?;
            let current = fs::read_to_string(&path)?;
            ensure!(
                current == change.before || current == change.after,
                "concurrent recovery conflict"
            );
            if current != change.before {
                durable_write(&path, change.before.as_bytes())?;
            }
            #[cfg(test)]
            visibility_tests::checkpoint(&format!("recovered-{index}"));
            #[cfg(not(test))]
            let _ = index;
        }
        plan.state = PlanState::Aborted;
        self.store(id, &plan)?;
        self.clear_active()?;
        Ok(serde_json::json!({"kind":"edit.recovered", "transaction":id}))
    }
}
fn configuration() -> String {
    hash(format!(
        "{:?}|{}",
        CompilerOptions::debug().resolve_environment(),
        target_lexicon::HOST
    ))
}
fn candidate(plan: &Plan) -> Result<String> {
    let base = hash(serde_json::to_vec(&(
        plan.version,
        &plan.compiler,
        &plan.configuration,
        &plan.entry,
        plan.project,
        &plan.revision,
        &plan.inputs,
        &plan.changes,
        &plan.absent,
    ))?);
    match &plan.verification_baseline {
        Some(baseline) => match &plan.verification_modules {
            Some(modules) => Ok(hash(serde_json::to_vec(&(base, baseline, modules))?)),
            None => Ok(hash(serde_json::to_vec(&(base, baseline))?)),
        },
        None => Ok(base),
    }
}

use crate::workspace_transaction::{durable_write, durable_write_observed, sync_directory};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct EditWork {
    pub(super) tokens_indexed: usize,
    pub(super) expressions_indexed: usize,
    #[serde(default)]
    pub(super) value_facts_indexed: usize,
    #[serde(default)]
    pub(super) value_probes: usize,
    pub(super) references_visited: usize,
    pub(super) patches: usize,
    #[serde(default)]
    pub(super) declarations: usize,
    pub(super) output_bytes: usize,
}
#[derive(Clone)]
pub(super) struct Patch {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) text: String,
}
/// Reports candidate diagnostics against workspace-relative source paths, so
/// agents never open or edit the isolated `.willow-edits/<tx>/` copy.
struct CandidatePaths<'a> {
    inner: &'a mut dyn DiagnosticEmitter,
    prefix: String,
    maps: std::collections::HashMap<FileId, SourceMap>,
}
impl<'a> CandidatePaths<'a> {
    fn new(inner: &'a mut dyn DiagnosticEmitter, sandbox: &Path) -> Self {
        Self {
            inner,
            prefix: format!("{}{}", sandbox.display(), std::path::MAIN_SEPARATOR),
            maps: Default::default(),
        }
    }
    fn relative(&self, text: &str) -> String {
        text.replace(&self.prefix, "")
    }
}
struct Remapped<'a> {
    maps: &'a std::collections::HashMap<FileId, SourceMap>,
    fallback: &'a dyn SourceLookup,
}
impl SourceLookup for Remapped<'_> {
    fn get(&self, id: FileId) -> Option<&SourceMap> {
        self.maps.get(&id).or_else(|| self.fallback.get(id))
    }
}
impl DiagnosticEmitter for CandidatePaths<'_> {
    fn emit(&mut self, diagnostic: &Diagnostic, sources: &dyn SourceLookup) -> std::io::Result<()> {
        let ids = diagnostic
            .labels
            .iter()
            .map(|l| l.span.file_id)
            .chain(diagnostic.fix_suggestions.iter().map(|f| f.span.file_id));
        for id in ids {
            if !self.maps.contains_key(&id)
                && let Some(map) = sources.get(id)
                && map.path.starts_with(&self.prefix)
            {
                let mut map = map.clone();
                map.path = self.relative(&map.path);
                self.maps.insert(id, map);
            }
        }
        let mut diagnostic = diagnostic.clone();
        diagnostic.message = self.relative(&diagnostic.message);
        for text in diagnostic
            .notes
            .iter_mut()
            .chain(diagnostic.helps.iter_mut())
            .chain(diagnostic.labels.iter_mut().map(|l| &mut l.message))
        {
            *text = self.relative(text);
        }
        let lookup = Remapped {
            maps: &self.maps,
            fallback: sources,
        };
        self.inner.emit(&diagnostic, &lookup)
    }
}

/// Rejected edit tied to one source occurrence. `path` is workspace-relative;
/// offsets are zero-based end-exclusive bytes; line/column are one-based.
#[derive(Debug, Clone, Serialize)]
pub struct RejectionLocation {
    pub path: String,
    pub start: usize,
    pub end: usize,
    pub line: usize,
    pub column: usize,
}
#[derive(Debug, Clone, Serialize)]
pub struct Rejection {
    pub message: String,
    pub location: RejectionLocation,
}
impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let l = &self.location;
        write!(f, "{} at {}:{}:{}", self.message, l.path, l.line, l.column)
    }
}
impl std::error::Error for Rejection {}

/// Preview semantic changes without creating transaction metadata or candidates.
pub(super) fn preview_analyzed(
    snapshot: &Snapshot,
    request: Request,
    diff: bool,
) -> Result<RenamePreview> {
    ensure!(request.revision == snapshot.revision, "stale revision");
    let root = Path::new(&snapshot.workspace);
    let context = snapshot
        .edit_context
        .as_ref()
        .context("edit requires live analysis inputs")?;
    context.verify_edit(&context.entry, root, context.project)?;
    ensure!(
        !root.join(".willow-edits/active").exists(),
        "interrupted transaction requires edit recover"
    );
    let mut sources = BTreeMap::new();
    for (path, expected) in &snapshot.sources {
        if verified_bundled_source(path, expected)? {
            continue;
        }
        let relative = Path::new(path)
            .strip_prefix(root)
            .context("external source is not editable")?
            .to_str()
            .context("non UTF-8 path")?;
        let source = fs::read_to_string(checked_file(root, relative)?)?;
        ensure!(hash(&source) == *expected, "source changed during analysis");
        sources.insert(relative.to_owned(), source);
    }
    let (changes, work) = structured_changes(snapshot, root, &sources, request.operations)?;
    for (path, expected) in &snapshot.sources {
        if verified_bundled_source(path, expected)? {
            continue;
        }
        ensure!(
            hash(fs::read(path)?) == *expected,
            "source changed during preview"
        );
    }
    context.verify_files()?;
    Ok(RenamePreview {
        status: super::query_result::Status::Ok,
        files_changed: changes.len(),
        work,
        dry_run: true,
        revision: snapshot.revision.clone(),
        changes: diff.then(|| {
            changes
                .iter()
                .map(|c| DiffChange {
                    path: c.path.clone(),
                    diff: super::diff::unified(&c.path, &c.before, &c.after),
                })
                .collect()
        }),
    })
}

/// Member-call tokens the type checker proved to be builtin methods
/// (`items.len()`, `counter.add(1)`): never source rename targets.
fn builtin_member_points<'a>(
    snapshot: &'a Snapshot,
    root: &Path,
) -> std::collections::HashSet<(&'a Path, usize)> {
    // Methods the bundled standard source declares are compiler-owned like
    // intrinsic builtins: a call to one is never a workspace rename target.
    let compiler_owned: std::collections::HashSet<_> = snapshot
        .semantic
        .symbols
        .iter()
        .filter(|s| {
            (s.is_builtin() && s.kind == super::SymbolKind::BuiltinMethod)
                || s.location
                    .as_ref()
                    .is_some_and(|l| super::bundled_source(&l.path).is_some())
        })
        .map(|s| s.id.as_str())
        .collect();
    snapshot
        .semantic
        .references
        .iter()
        .filter(|r| compiler_owned.contains(r.target.as_str()))
        .filter_map(|r| {
            let path = Path::new(&r.location.path).strip_prefix(root).ok()?;
            Some((path, r.location.start))
        })
        .collect()
}

fn structured_changes(
    snapshot: &Snapshot,
    root: &Path,
    sources: &BTreeMap<String, String>,
    operations: Vec<Operation>,
) -> Result<(Vec<Change>, EditWork)> {
    use crate::lexer::Lexer;
    let mut state = FunctionEdits::default();
    let mut tokens = BTreeMap::new();
    for (path, source) in sources {
        if path.ends_with(".wi") {
            tokens.insert(
                path.clone(),
                Lexer::new(source)
                    .tokenize()
                    .map_err(|_| anyhow::anyhow!("source lexing failed"))?,
            );
        }
    }
    let index = StructuredIndex::new(snapshot, root, &tokens, &operations, &mut state.work)?;
    let mut semantic_renames = index
        .has_member_rename
        .then(|| super::rename::Index::new(snapshot, root, &index.identifiers, &index.qualifiers));
    for operation in operations {
        let id = match &operation {
            Operation::Rename { function, .. } | Operation::ReplaceBody { function, .. } => {
                function
            }
        };
        if let Operation::Rename { name, .. } = &operation
            && let Some(index) = &mut semantic_renames
            && index.supports(id)
        {
            index.plan(id, name, root, &mut state.patches, &mut state.work)?;
            continue;
        }
        let function = index
            .functions
            .get(id.as_str())
            .context("unknown function")?;
        ensure!(
            !function.synthetic && function.locations.len() == 1,
            "function has no unique source declaration"
        );
        let location = &function.locations[0];
        let path = Path::new(&location.path)
            .strip_prefix(root)?
            .to_str()
            .context("non UTF-8 path")?;
        let file_tokens = &tokens[path];
        let start = file_tokens.partition_point(|t| t.span.start < location.start);
        let end = file_tokens.partition_point(|t| t.span.end <= location.end);
        let declaration = &file_tokens[start..end];
        match operation {
            Operation::ReplaceBody { body, .. } => {
                plan_body_replacement(function, path, body, &mut state.patches)?;
            }
            Operation::Rename { name, .. } => {
                index.plan_function_rename(function, declaration, name, &mut state)?;
            }
        }
    }
    let changes = apply_structured_patches(sources, state.patches, &mut state.work)?;
    Ok((changes, state.work))
}

#[derive(Default)]
struct FunctionEdits<'a> {
    patches: BTreeMap<String, Vec<Patch>>,
    work: EditWork,
    renamed: std::collections::HashSet<String>,
    // Built lazily, once per request, only for unproven same-spelled tokens.
    builtin_members: Option<std::collections::HashSet<(&'a Path, usize)>>,
}

struct TokenIndex<'a> {
    identifiers: BTreeMap<&'a str, Vec<(&'a str, &'a crate::diagnostics::Span)>>,
    call_names: BTreeMap<(&'a str, usize), (&'a str, usize)>,
    qualifiers: std::collections::HashSet<(&'a str, usize, usize)>,
    alias_tokens: std::collections::HashSet<(&'a Path, usize, usize)>,
}

impl<'a> TokenIndex<'a> {
    fn new(
        tokens: &'a BTreeMap<String, Vec<crate::lexer::token::Token>>,
        work: &mut EditWork,
    ) -> Self {
        use crate::lexer::token::TokenKind;
        let mut identifiers: BTreeMap<&str, Vec<(&str, &crate::diagnostics::Span)>> =
            BTreeMap::new();
        let mut call_names = BTreeMap::new();
        let mut qualifiers = std::collections::HashSet::new();
        let mut alias_tokens = std::collections::HashSet::new();
        for (path, ts) in tokens {
            for (i, token) in ts.iter().enumerate() {
                work.tokens_indexed += 1;
                if let TokenKind::Ident(name) = &token.kind {
                    if i > 0 && ts[i - 1].kind == TokenKind::As {
                        alias_tokens.insert((Path::new(path), token.span.start, token.span.end));
                    }
                    if ts
                        .get(i + 1)
                        .is_some_and(|t| t.kind == TokenKind::ColonColon)
                    {
                        qualifiers.insert((path.as_str(), token.span.start, token.span.end));
                    }
                    identifiers
                        .entry(name)
                        .or_default()
                        .push((path, &token.span));
                    if ts.get(i + 1).is_some_and(|t| t.kind == TokenKind::LParen) {
                        let mut first = i;
                        while first >= 2
                            && ts[first - 1].kind == TokenKind::ColonColon
                            && matches!(ts[first - 2].kind, TokenKind::Ident(_))
                        {
                            first -= 2;
                        }
                        call_names.insert(
                            (path.as_str(), ts[first].span.start),
                            (name.as_str(), token.span.start),
                        );
                        call_names.insert(
                            (path.as_str(), token.span.start),
                            (name.as_str(), token.span.start),
                        );
                    }
                }
            }
        }
        Self {
            identifiers,
            call_names,
            qualifiers,
            alias_tokens,
        }
    }
}

struct StructuredIndex<'a> {
    snapshot: &'a Snapshot,
    root: &'a Path,
    tokens: &'a BTreeMap<String, Vec<crate::lexer::token::Token>>,
    functions: BTreeMap<&'a str, &'a super::Function>,
    identifiers: BTreeMap<&'a str, Vec<(&'a str, &'a crate::diagnostics::Span)>>,
    call_names: BTreeMap<(&'a str, usize), (&'a str, usize)>,
    qualifiers: std::collections::HashSet<(&'a str, usize, usize)>,
    references: BTreeMap<&'a str, Vec<&'a super::semantic::Expression>>,
    dispatch: BTreeMap<&'a str, Vec<&'a str>>,
    local_ids: std::collections::HashSet<&'a str>,
    value_targets:
        std::collections::HashMap<(&'a Path, usize, usize), std::collections::HashSet<&'a str>>,
    alias_points: std::collections::HashSet<(&'a Path, usize, usize)>,
    has_member_rename: bool,
}

impl<'a> StructuredIndex<'a> {
    fn new(
        snapshot: &'a Snapshot,
        root: &'a Path,
        tokens: &'a BTreeMap<String, Vec<crate::lexer::token::Token>>,
        operations: &[Operation],
        work: &mut EditWork,
    ) -> Result<Self> {
        let functions: BTreeMap<_, _> = snapshot
            .functions
            .iter()
            .map(|f| (f.id.as_str(), f))
            .collect();
        let TokenIndex {
            identifiers,
            call_names,
            qualifiers,
            alias_tokens,
        } = TokenIndex::new(tokens, work);
        let mut references: BTreeMap<&str, Vec<_>> = BTreeMap::new();
        for expression in &snapshot.semantic.expressions {
            work.expressions_indexed += 1;
            if let Some(target) = &expression.target {
                references.entry(target).or_default().push(expression);
            }
        }
        // Synthetic dispatch unions connect implementations sharing a dispatch
        // contract. Follow only those edges, never ordinary caller/callee edges.
        let mut dispatch: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for f in &snapshot.functions {
            if f.synthetic_kind == crate::qualified_name::SyntheticKind::Dispatch {
                for callee in &f.callees {
                    dispatch.entry(&f.id).or_default().push(callee);
                    dispatch.entry(callee).or_default().push(&f.id);
                }
            }
        }
        let requested_renames: std::collections::HashSet<_> = operations
            .iter()
            .filter_map(|op| {
                if let Operation::Rename { function, .. } = op {
                    Some(function.as_str())
                } else {
                    None
                }
            })
            .collect();
        let has_member_rename = snapshot.semantic.symbols.iter().any(|symbol| {
            requested_renames.contains(symbol.id.as_str())
                && matches!(
                    symbol.kind,
                    SymbolKind::Method
                        | SymbolKind::Field
                        | SymbolKind::StaticField
                        | SymbolKind::Variant
                        | SymbolKind::Binding
                        | SymbolKind::Parameter
                )
        });
        let local_ids: std::collections::HashSet<_> = snapshot
            .semantic
            .symbols
            .iter()
            .filter(|s| matches!(s.kind, SymbolKind::Binding | SymbolKind::Parameter))
            .map(|s| s.id.as_str())
            .collect();
        if requested_renames.iter().any(|id| local_ids.contains(id)) {
            let mut destinations = std::collections::HashSet::new();
            for operation in operations {
                if let Operation::Rename { name, .. } = operation {
                    ensure!(
                        destinations.insert(name),
                        "batch rename with local bindings requires distinct destination names to prevent capture"
                    );
                }
            }
        }
        // Exact checker-owned identifier ranges, indexed once for all operations.
        // Unlike expression ranges these cover values as well as calls.
        let mut value_targets: std::collections::HashMap<_, std::collections::HashSet<&str>> =
            std::collections::HashMap::new();
        let mut explicit_aliases = std::collections::HashSet::new();
        let mut alias_points = std::collections::HashSet::new();
        for symbol in &snapshot.semantic.symbols {
            if symbol.kind == SymbolKind::Import
                && let Some(l) = &symbol.location
                && let Ok(path) = Path::new(&l.path).strip_prefix(root)
                && alias_tokens.contains(&(path, l.start, l.end))
            {
                explicit_aliases.insert(symbol.id.as_str());
                alias_points.insert((path, l.start, l.end));
            }
            work.value_facts_indexed += 1;
            if local_ids.contains(symbol.id.as_str())
                && let Some(l) = &symbol.location
                && let Ok(path) = Path::new(&l.path).strip_prefix(root)
            {
                value_targets
                    .entry((path, l.start, l.end))
                    .or_default()
                    .insert(symbol.id.as_str());
            }
        }
        for reference in &snapshot.semantic.references {
            work.value_facts_indexed += 1;
            let l = &reference.location;
            let Ok(path) = Path::new(&l.path).strip_prefix(root) else {
                continue;
            };
            // Import provenance accompanies the actual resolved value target. An
            // explicit alias owns its own spelling and must survive target renames.
            if reference.role == "import" {
                if explicit_aliases.contains(reference.target.as_str()) {
                    alias_points.insert((path, l.start, l.end));
                }
                continue;
            }
            // Import-target facts select a spelling within the whole import and
            // may point at a repeated module name. The dedicated rename_imports
            // path below proves the final item token, excluding qualifiers/aliases.
            if reference.role == "import-target" {
                continue;
            }
            value_targets
                .entry((path, l.start, l.end))
                .or_default()
                .insert(reference.target.as_str());
        }
        Ok(Self {
            snapshot,
            root,
            tokens,
            functions,
            identifiers,
            call_names,
            qualifiers,
            references,
            dispatch,
            local_ids,
            value_targets,
            alias_points,
            has_member_rename,
        })
    }

    fn check_destination(&self, name: &str) -> Result<()> {
        let Self {
            snapshot,
            root,
            identifiers,
            ..
        } = self;
        if let Some(occurrences) = identifiers.get(name) {
            // Cite the conflicting declaration rather than an import or a
            // use of it: source declarations rank before import aliases,
            // then the first occurrence. Error path only: one scan of the
            // symbols spelled `name` plus that spelling's occurrences.
            let declarations: std::collections::HashMap<_, _> = snapshot
                .semantic
                .symbols
                .iter()
                .filter(|s| QualifiedName::new(&s.name).last_segment() == name)
                .filter_map(|s| {
                    let l = s.location.as_ref()?;
                    let path = Path::new(&l.path).strip_prefix(root).ok()?;
                    Some(((path, l.start), usize::from(s.kind == SymbolKind::Import)))
                })
                .collect();
            let declaration = occurrences
                .iter()
                .filter_map(|occurrence @ (path, span)| {
                    Some((
                        declarations.get(&(Path::new(path), span.start))?,
                        occurrence,
                    ))
                })
                .min_by_key(|(rank, _)| **rank);
            let (message, &(path, span)) = match declaration {
                Some((_, found)) => (
                    "rename destination conflicts with an existing declaration",
                    found,
                ),
                None => (
                    "rename destination already occurs in workspace",
                    &occurrences[0],
                ),
            };
            return Err(Rejection {
                message: message.into(),
                location: RejectionLocation {
                    path: path.into(),
                    start: span.start,
                    end: span.end,
                    line: span.line,
                    column: span.col,
                },
            }
            .into());
        }
        Ok(())
    }

    fn allowed_family_occurrences(
        &self,
        family: &std::collections::BTreeSet<&str>,
        old: &str,
        work: &mut EditWork,
    ) -> Result<BTreeMap<String, std::collections::BTreeSet<usize>>> {
        use crate::lexer::token::TokenKind;
        let Self {
            functions,
            root,
            tokens,
            call_names,
            references,
            ..
        } = self;
        let mut allowed: BTreeMap<String, std::collections::BTreeSet<usize>> = BTreeMap::new();
        for id in family {
            let member = functions[id];
            for location in &member.rename_calls {
                let p = Path::new(&location.path)
                    .strip_prefix(root)?
                    .to_str()
                    .context("non UTF-8 path")?;
                if let Some(&(name, start)) = call_names.get(&(p, location.start))
                    && name == old
                {
                    allowed.entry(p.into()).or_default().insert(start);
                }
            }
            if !member.synthetic {
                for location in &member.locations {
                    let p = Path::new(&location.path)
                        .strip_prefix(root)?
                        .to_str()
                        .context("non UTF-8 path")?;
                    let ts = &tokens[p];
                    let a = ts.partition_point(|t| t.span.start < location.start);
                    let b = ts.partition_point(|t| t.span.end <= location.end);
                    let marker = ts[a..b]
                        .iter()
                        .position(|t| matches!(t.kind, TokenKind::Fn | TokenKind::Const))
                        .context("unsupported rename declaration")?
                        + a;
                    let t = ts
                        .get(marker + 1)
                        .context("missing declaration identifier")?;
                    ensure!(
                        matches!(&t.kind, TokenKind::Ident(s) if s == old),
                        "inconsistent dispatch declaration"
                    );
                    if allowed.entry(p.into()).or_default().insert(t.span.start) {
                        work.declarations += 1;
                    }
                }
            }
            // Item imports (`import m::{f}` / `import m::f as g`) name the
            // target by its declared spelling: the final path segment.
            for location in &member.rename_imports {
                let p = Path::new(&location.path)
                    .strip_prefix(root)?
                    .to_str()
                    .context("non UTF-8 path")?;
                let ts = &tokens[p];
                let a = ts.partition_point(|t| t.span.start < location.start);
                let b = ts.partition_point(|t| t.span.end <= location.end);
                for i in a..b {
                    if matches!(&ts[i].kind, TokenKind::Ident(s) if s == old)
                        && ts
                            .get(i + 1)
                            .is_none_or(|t| t.kind != TokenKind::ColonColon)
                        && (i == 0 || ts[i - 1].kind != TokenKind::As)
                    {
                        allowed
                            .entry(p.into())
                            .or_default()
                            .insert(ts[i].span.start);
                    }
                }
            }
            for expression in references.get(id).into_iter().flatten() {
                work.references_visited += 1;
                let p = Path::new(&expression.location.path)
                    .strip_prefix(root)?
                    .to_str()
                    .context("non UTF-8 path")?;
                if let Some(&(name, start)) = call_names.get(&(p, expression.location.start))
                    && name == old
                {
                    allowed.entry(p.into()).or_default().insert(start);
                }
            }
        }
        Ok(allowed)
    }

    fn plan_function_rename(
        &self,
        function: &super::Function,
        declaration: &[crate::lexer::token::Token],
        name: String,
        state: &mut FunctionEdits<'a>,
    ) -> Result<()> {
        use crate::lexer::token::TokenKind;
        let Self {
            snapshot,
            root,
            tokens,
            identifiers,
            dispatch,
            qualifiers,
            alias_points,
            value_targets,
            local_ids,
            ..
        } = self;
        let FunctionEdits {
            patches,
            work,
            renamed,
            builtin_members,
        } = state;
        ensure!(
            !renamed.contains(&function.id),
            "duplicate rename of dispatch family"
        );
        super::rename::validate_name(&name)?;
        let marker = declaration
            .iter()
            .position(|t| matches!(t.kind, TokenKind::Fn | TokenKind::Const))
            .context("unsupported declaration")?;
        let old = match &declaration
            .get(marker + 1)
            .context("missing declaration name")?
            .kind
        {
            TokenKind::Ident(s) => s,
            _ => anyhow::bail!("unsupported declaration name"),
        };
        ensure!(name != *old, "rename has no effect");
        self.check_destination(&name)?;
        // Conservative closed-world rename: every same-spelled token
        // must be proven to belong to the target or a local binding.
        // Unknown or conflicting semantic targets still fail closed.
        let mut family = std::collections::BTreeSet::new();
        let mut pending = vec![function.id.as_str()];
        while let Some(id) = pending.pop() {
            if family.insert(id)
                && let Some(edges) = dispatch.get(id)
            {
                pending.extend(edges);
            }
        }
        renamed.extend(family.iter().map(|id| (*id).to_owned()));
        let allowed = self.allowed_family_occurrences(&family, old, work)?;
        for &(p, span) in identifiers.get(old.as_str()).into_iter().flatten() {
            work.value_probes += 1;
            // Functions and constants cannot own a namespace. A
            // same-spelled path qualifier is not a use of this target.
            if qualifiers.contains(&(p, span.start, span.end)) {
                continue;
            }
            let point = (Path::new(p), span.start, span.end);
            if alias_points.contains(&point) {
                continue;
            }
            let mut proven_value = false;
            if let Some(targets) = value_targets.get(&point) {
                if targets.iter().all(|target| local_ids.contains(target)) {
                    continue;
                }
                if targets.iter().all(|target| family.contains(target)) {
                    proven_value = true;
                } else if targets.iter().any(|target| family.contains(target)) {
                    anyhow::bail!(
                        "rename occurrence has conflicting semantic targets at {p}:{}:{}",
                        span.line,
                        span.col
                    );
                }
            }
            if !proven_value && !allowed.get(p).is_some_and(|a| a.contains(&span.start)) {
                if builtin_members
                    .get_or_insert_with(|| builtin_member_points(snapshot, root))
                    .contains(&(Path::new(p), span.start))
                {
                    continue;
                }
                let ts = &tokens[p];
                let index = ts.partition_point(|t| t.span.start < span.start);
                let reason = match index
                    .checked_sub(1)
                    .and_then(|i| ts.get(i))
                    .map(|t| &t.kind)
                {
                    Some(TokenKind::Fn) => {
                        "function/method declaration outside the supported dispatch family; interface contract declarations are not covered"
                    }
                    Some(TokenKind::Dot) => {
                        "member reference without a proven target in the supported dispatch family"
                    }
                    Some(TokenKind::ColonColon) => {
                        "qualified reference without a proven target in the supported dispatch family"
                    }
                    _ => "same-name identifier outside the proven declaration/reference set",
                };
                return Err(Rejection {
                    message: format!(
                        "rename coverage incomplete or ambiguous: `{old}` is a {reason}. Inspect this occurrence with `willow symbol {p}:{}:{}` and `willow refs`; no files changed",
                        span.line, span.col
                    ),
                    location: RejectionLocation {
                        path: p.into(),
                        start: span.start,
                        end: span.end,
                        line: span.line,
                        column: span.col,
                    },
                }
                .into());
            }
            patches.entry(p.into()).or_default().push(Patch {
                start: span.start,
                end: span.end,
                text: name.clone(),
            });
        }
        Ok(())
    }
}

fn plan_body_replacement(
    function: &super::Function,
    path: &str,
    body: String,
    patches: &mut BTreeMap<String, Vec<Patch>>,
) -> Result<()> {
    use crate::lexer::{Lexer, token::TokenKind};
    let location = &function.locations[0];
    let ts = Lexer::new(&body)
        .tokenize()
        .map_err(|_| anyhow::anyhow!("invalid replacement body"))?;
    ensure!(
        ts.first().is_some_and(|t| t.kind == TokenKind::LBrace),
        "replacement must be one block"
    );
    let mut depth = 0usize;
    for (i, t) in ts.iter().enumerate().take(ts.len() - 1) {
        match t.kind {
            TokenKind::LBrace => depth += 1,
            TokenKind::RBrace => {
                depth = depth.checked_sub(1).context("unbalanced replacement")?;
                ensure!(
                    depth != 0 || i == ts.len() - 2,
                    "replacement contains trailing declarations"
                );
            }
            _ => {}
        }
    }
    ensure!(depth == 0, "unbalanced replacement");
    let body_location = function
        .body_location
        .as_ref()
        .context("missing compiler body range")?;
    ensure!(
        body_location.path == location.path,
        "body is in a different source"
    );
    patches.entry(path.into()).or_default().push(Patch {
        start: body_location.start,
        end: body_location.end,
        text: body,
    });
    Ok(())
}

fn apply_structured_patches(
    sources: &BTreeMap<String, String>,
    patches: BTreeMap<String, Vec<Patch>>,
    work: &mut EditWork,
) -> Result<Vec<Change>> {
    let mut changes = Vec::new();
    for (path, mut edits) in patches {
        edits.sort_by_key(|e| (e.start, e.end));
        let before = &sources[&path];
        let mut after = String::new();
        let mut cursor = 0;
        work.patches += edits.len();
        let mut offsets = Vec::with_capacity(edits.len());
        for edit in edits {
            ensure!(
                edit.start >= cursor && edit.end <= before.len(),
                "overlapping edits"
            );
            after.push_str(
                before
                    .get(cursor..edit.start)
                    .context("invalid edit boundary")?,
            );
            let new_start = after.len();
            after.push_str(&edit.text);
            offsets.push((edit.start, edit.end, new_start, after.len()));
            cursor = edit.end;
        }
        after.push_str(&before[cursor..]);
        work.output_bytes += after.len();
        if after != *before {
            changes.push(Change {
                path,
                before: before.clone(),
                after,
                offsets,
            });
        }
    }
    Ok(changes)
}

fn verified_bundled_source(path: &str, expected: &str) -> Result<bool> {
    let Some(source) = super::bundled_source(path) else {
        return Ok(false);
    };
    ensure!(
        hash(source) == expected,
        "bundled source changed during analysis"
    );
    Ok(true)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "edit_visibility_tests.rs"]
mod visibility_tests;
