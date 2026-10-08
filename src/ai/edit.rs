//! Durable structured edits. The workspace lock defines the observation boundary:
//! cooperating readers/writers see all old or all new files. Ordinary filesystem
//! readers can observe intermediate files; an interrupted apply is rolled back
//! before the next edit operation. Recovery never overwrites a third-party edit.
use super::{Snapshot, hash};
use crate::{
    CompilerOptions, CompilerSession,
    diagnostics::{Diagnostic, DiagnosticEmitter, FileId, SourceMap, source_map::SourceLookup},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
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
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    path: String,
    before: String,
    after: String,
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
    state: String,
    candidate: String,
    absent: Vec<String>,
    work: EditWork,
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

pub struct Workspace {
    root: PathBuf,
    directory: PathBuf,
    _lock: File,
}

#[derive(Serialize, Deserialize)]
pub(super) struct LocalRenameHint {
    pub selector: String,
    pub path: String,
    pub start: usize,
    pub digest: String,
}
impl LocalRenameHint {
    pub fn read(snapshot: &Snapshot) -> Option<Self> {
        // Advisory metadata must never make an otherwise valid query fail.
        use std::io::Read;
        let file =
            File::open(Path::new(&snapshot.workspace).join(".willow-edits/last-local-rename.json"))
                .ok()?;
        let mut bytes = Vec::new();
        file.take(65537).read_to_end(&mut bytes).ok()?;
        if bytes.len() > 65536 {
            return None;
        }
        let hint: Self = serde_json::from_slice(&bytes).ok()?;
        (snapshot.sources.get(&hint.path) == Some(&hint.digest)).then_some(hint)
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
        for name in ["project.toml", "project.lock"] {
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
        fs::create_dir_all(&directory)?;
        ensure!(
            !fs::symlink_metadata(&directory)?.file_type().is_symlink(),
            "edit directory is a symlink"
        );
        let lock_path = directory.join("lock");
        if lock_path.exists() {
            ensure!(
                !fs::symlink_metadata(&lock_path)?.file_type().is_symlink(),
                "edit lock is a symlink"
            );
        }
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)?;
        lock.try_lock()
            .context("workspace edit operation already running")?;
        Ok(Self {
            root,
            directory,
            _lock: lock,
        })
    }
    /// A bounded advisory record, never an alias used for resolution. Its source
    /// digest prevents an interrupted/failed edit or later source edit from
    /// redirecting suggestions to a different declaration.
    pub(super) fn remember_local_rename(
        &self,
        transaction: &str,
        selector: &str,
        location: &super::Location,
    ) -> Result<()> {
        let plan = self.load(transaction)?;
        let relative = Path::new(&location.path).strip_prefix(&self.root)?;
        let change = plan
            .changes
            .iter()
            .find(|change| Path::new(&change.path) == relative)
            .context("renamed local source is absent from edit")?;
        let hint = LocalRenameHint {
            selector: selector.into(),
            path: location.path.clone(),
            start: location.start,
            digest: hash(&change.after),
        };
        durable_write(
            &self.directory.join("last-local-rename.json"),
            &serde_json::to_vec(&hint)?,
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
        for name in ["project.toml", "project.lock"] {
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
        let mut plan = Plan {
            version: 1,
            compiler: super::storage::compiler_stamp(),
            configuration: configuration(),
            entry: relative,
            project,
            revision: snapshot.revision,
            inputs,
            changes,
            state: "prepared".into(),
            candidate: String::new(),
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
        self.store(&id, &plan)?;
        self.preview(&id, format)
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
        self.ensure_recovered()?;
        let plan = self.load(id)?;
        let changes = match format {
            ChangeFormat::Full => serde_json::to_value(&plan.changes)?,
            ChangeFormat::Diff => plan
                .changes
                .iter()
                .map(|c| {
                    serde_json::json!({"path":c.path,
                        "diff":super::diff::unified(&c.path, &c.before, &c.after)})
                })
                .collect(),
        };
        Ok(
            serde_json::json!({"kind":"edit.preview", "transaction":id, "revision":plan.revision,
            "candidate":plan.candidate, "state":plan.state, "changes":changes, "work":plan.work}),
        )
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
    pub fn validate(
        &self,
        id: &str,
        emitter: &mut dyn DiagnosticEmitter,
    ) -> Result<serde_json::Value> {
        self.ensure_recovered()?;
        let mut plan = self.load(id)?;
        ensure!(
            matches!(plan.state.as_str(), "prepared" | "validated"),
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
        plan.state = "validated".into();
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
        plan.state = "applying".into();
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
        plan.state = "aborted".into();
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
            plan.state == "validated",
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
        plan.state = "applying".into();
        self.store(id, &plan)?;
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
        plan.state = "applied".into();
        self.store(id, &plan)?;
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
                plan.state.as_str(),
                "validated" | "applying" | "applied" | "aborted"
            ),
            "invalid recovery state"
        );
        if matches!(plan.state.as_str(), "applied" | "aborted") {
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
        for change in &plan.changes {
            let path = self.file(&change.path)?;
            let current = fs::read_to_string(&path)?;
            ensure!(
                current == change.before || current == change.after,
                "concurrent recovery conflict"
            );
            if current != change.before {
                durable_write(&path, change.before.as_bytes())?;
            }
        }
        plan.state = "aborted".into();
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
    Ok(hash(serde_json::to_vec(&(
        plan.version,
        &plan.compiler,
        &plan.configuration,
        &plan.entry,
        plan.project,
        &plan.revision,
        &plan.inputs,
        &plan.changes,
        &plan.absent,
    ))?))
}
// Unix supports syncing directory entries after rename/unlink. Windows cannot
// open a directory through File::open; file contents are still synced before
// replacement there. Process-crash recovery is supported on both platforms;
// power-loss durability of directory entries is not promised on Windows.
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn durable_write(path: &Path, bytes: &[u8]) -> Result<()> {
    durable_write_observed(path, bytes, || {})
}
fn durable_write_observed(path: &Path, bytes: &[u8], published: impl FnOnce()) -> Result<()> {
    let parent = path.parent().context("missing parent")?;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = parent.join(format!(
        ".willow-write-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        if path.exists() {
            file.set_permissions(fs::metadata(path)?.permissions())?;
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        published();
        sync_directory(parent)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[derive(Default, Serialize, Deserialize)]
pub(super) struct EditWork {
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
pub(super) fn preview_analyzed(snapshot: &Snapshot, request: Request) -> Result<serde_json::Value> {
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
        ensure!(
            hash(fs::read(path)?) == *expected,
            "source changed during preview"
        );
    }
    context.verify_files()?;
    let changes:Vec<_>=changes.iter().map(|c|serde_json::json!({"path":c.path,"diff":super::diff::unified(&c.path,&c.before,&c.after)})).collect();
    Ok(
        serde_json::json!({"status":"ok","changes":changes,"work":work,"dry_run":true,"revision":snapshot.revision}),
    )
}

/// Member-call tokens the type checker proved to be builtin methods
/// (`items.len()`, `counter.add(1)`): never source rename targets.
fn builtin_member_points<'a>(
    snapshot: &'a Snapshot,
    root: &Path,
) -> std::collections::HashSet<(&'a Path, usize)> {
    snapshot
        .semantic
        .references
        .iter()
        .filter(|r| r.target.starts_with("builtin:builtin-method:"))
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
    use crate::lexer::{Lexer, token::TokenKind};
    let mut work = EditWork::default();
    let mut patches: BTreeMap<String, Vec<Patch>> = BTreeMap::new();
    let functions: BTreeMap<_, _> = snapshot
        .functions
        .iter()
        .map(|f| (f.id.as_str(), f))
        .collect();
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
    let mut identifiers: BTreeMap<&str, Vec<(&str, &crate::diagnostics::Span)>> = BTreeMap::new();
    let mut call_names = BTreeMap::new();
    let mut qualifiers = std::collections::HashSet::new();
    let mut alias_tokens = std::collections::HashSet::new();
    for (path, ts) in &tokens {
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
        if f.synthetic && f.name.starts_with("$dispatch$") {
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
                symbol.kind.as_str(),
                "method" | "field" | "static-field" | "variant" | "binding" | "parameter"
            )
    });
    let local_ids: std::collections::HashSet<_> = snapshot
        .semantic
        .symbols
        .iter()
        .filter(|s| matches!(s.kind.as_str(), "binding" | "parameter"))
        .map(|s| s.id.as_str())
        .collect();
    if requested_renames.iter().any(|id| local_ids.contains(id)) {
        let mut destinations = std::collections::HashSet::new();
        for operation in &operations {
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
        if symbol.kind == "import"
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
    let mut semantic_renames = has_member_rename
        .then(|| super::rename::Index::new(snapshot, root, &identifiers, &qualifiers));
    let mut renamed = std::collections::HashSet::new();
    // Built lazily, once per request, only when a same-spelled token is not a
    // proven declaration/reference of the renamed function.
    let mut builtin_members = None;
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
            index.plan(id, name, root, &mut patches, &mut work)?;
            continue;
        }
        let function = functions.get(id.as_str()).context("unknown function")?;
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
            }
            Operation::Rename { name, .. } => {
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
                if let Some(occurrences) = identifiers.get(name.as_str()) {
                    // Cite the conflicting declaration rather than an import or a
                    // use of it: source declarations rank before import aliases,
                    // then the first occurrence. Error path only: one scan of the
                    // symbols spelled `name` plus that spelling's occurrences.
                    let declarations: std::collections::HashMap<_, _> = snapshot
                        .semantic
                        .symbols
                        .iter()
                        .filter(|s| s.name.rsplit("::").next() == Some(name.as_str()))
                        .filter_map(|s| {
                            let l = s.location.as_ref()?;
                            let path = Path::new(&l.path).strip_prefix(root).ok()?;
                            Some(((path, l.start), usize::from(s.kind == "import")))
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
                let mut allowed: BTreeMap<String, std::collections::BTreeSet<usize>> =
                    BTreeMap::new();
                for id in &family {
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
                        if let Some(&(name, start)) =
                            call_names.get(&(p, expression.location.start))
                            && name == old
                        {
                            allowed.entry(p.into()).or_default().insert(start);
                        }
                    }
                }
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
                            _ => {
                                "same-name identifier outside the proven declaration/reference set"
                            }
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
            }
        }
    }
    let mut changes = Vec::new();
    for (path, mut edits) in patches {
        edits.sort_by_key(|e| (e.start, e.end));
        let before = &sources[&path];
        let mut after = String::new();
        let mut cursor = 0;
        work.patches += edits.len();
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
            after.push_str(&edit.text);
            cursor = edit.end;
        }
        after.push_str(&before[cursor..]);
        work.output_bytes += after.len();
        if after != *before {
            changes.push(Change {
                path,
                before: before.clone(),
                after,
            });
        }
    }
    Ok((changes, work))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn direct_apply_rolls_back_own_writes_on_late_failure_or_race() {
        for race in [0, 1, 2, 3] {
            let f = Fixture::new();
            fs::write(f.0.join("helper.wi"), "pub fn other() -> i64 { return 2; }").unwrap();
            fs::write(f.0.join("main.wi"),"import helper; fn value() -> i64 { return 1; } fn main() { println(value() + helper::other()); }").unwrap();
            let workspace = f.workspace();
            let mut emitter = crate::diagnostics::HumanEmitter;
            let snapshot = workspace
                .analyze(&f.0.join("main.wi"), false, &mut emitter)
                .unwrap();
            let operations = snapshot
                .functions
                .iter()
                .filter(|f| matches!(f.name.as_str(), "value" | "other"))
                .map(|f| Operation::ReplaceBody {
                    function: f.id.clone(),
                    body: "{ return 9; }".into(),
                })
                .collect();
            let preview = workspace
                .prepare_analyzed(
                    &f.0.join("main.wi"),
                    false,
                    Request {
                        revision: snapshot.revision.clone(),
                        operations,
                    },
                    ChangeFormat::Diff,
                    snapshot,
                )
                .unwrap();
            let id = preview["transaction"].as_str().unwrap();
            workspace.validate(id, &mut emitter).unwrap();
            let plan = workspace.load(id).unwrap();
            assert_eq!(plan.changes.len(), 2);
            let second = workspace.root.join(&plan.changes[1].path);
            let third_party = if race == 2 {
                plan.changes[1].after.clone()
            } else {
                format!("{} // concurrent user edit", plan.changes[1].before)
            };
            let hook = |event: Option<usize>| -> Result<()> {
                if event.is_none() && race == 3 {
                    anyhow::bail!("injected failure after active marker removal");
                }
                if matches!(event, Some(1)) && race != 3 {
                    if race != 0 {
                        fs::write(&second, &third_party)?;
                    } else {
                        anyhow::bail!("injected write failure");
                    }
                }
                Ok(())
            };
            assert!(
                workspace
                    .apply_rollback_observed(id, Some(&hook), &mut emitter)
                    .is_err()
            );
            assert_eq!(
                fs::read_to_string(workspace.root.join(&plan.changes[0].path)).unwrap(),
                plan.changes[0].before
            );
            assert_eq!(
                fs::read_to_string(second).unwrap(),
                if race == 1 || race == 2 {
                    third_party
                } else {
                    plan.changes[1].before.clone()
                }
            );
            assert!(!workspace.directory.join("active").exists());
            assert_eq!(workspace.load(id).unwrap().state, "aborted");
        }
    }
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "willow-edit-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::write(
                path.join("main.wi"),
                "fn value() -> i64 { return 1; } fn main() { println(value()); }",
            )
            .unwrap();
            Self(path)
        }
        fn workspace(&self) -> Workspace {
            Workspace::open(&self.0).unwrap()
        }
        fn prepare(&self, workspace: &Workspace, body: &str) -> String {
            let mut emitter = crate::diagnostics::HumanEmitter;
            let snapshot = workspace
                .analyze(&self.0.join("main.wi"), false, &mut emitter)
                .unwrap();
            let function = snapshot
                .functions
                .iter()
                .find(|f| f.name == "value")
                .unwrap()
                .id
                .clone();
            let result = workspace
                .prepare(
                    &self.0.join("main.wi"),
                    false,
                    Request {
                        revision: snapshot.revision,
                        operations: vec![Operation::ReplaceBody {
                            function,
                            body: body.into(),
                        }],
                    },
                    ChangeFormat::Full,
                    &mut emitter,
                )
                .unwrap();
            result["transaction"].as_str().unwrap().into()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn durable_write_creates_replaces_and_cleans_up_failed_publication() {
        let fixture = Fixture::new();
        let path = fixture.0.join("journal");
        durable_write(&path, b"prepared").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"prepared");
        durable_write(&path, b"applied").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"applied");
        let directory = fixture.0.join("directory");
        fs::create_dir(&directory).unwrap();
        assert!(durable_write(&directory, b"invalid").is_err());
        assert!(directory.is_dir());
        assert!(fs::read_dir(&fixture.0).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".willow-write-")
        }));
    }
    #[test]
    fn isolated_preview_validate_apply_and_replay_rejection() {
        let f = Fixture::new();
        let w = f.workspace();
        let before = fs::read(f.0.join("main.wi")).unwrap();
        let id = f.prepare(&w, "{ return 2; }");
        assert_eq!(fs::read(f.0.join("main.wi")).unwrap(), before);
        assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
        w.validate(&id, &mut crate::diagnostics::HumanEmitter)
            .unwrap();
        w.apply(&id, &mut crate::diagnostics::HumanEmitter).unwrap();
        assert!(
            fs::read_to_string(f.0.join("main.wi"))
                .unwrap()
                .contains("return 2")
        );
        assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
    }
    #[test]
    fn preview_formats_report_full_texts_or_unified_diffs() {
        let f = Fixture::new();
        let w = f.workspace();
        let id = f.prepare(&w, "{ return 2; }");
        let full = w.preview(&id, ChangeFormat::Full).unwrap();
        let diff = w.preview(&id, ChangeFormat::Diff).unwrap();
        assert_eq!(full["transaction"], diff["transaction"]);
        let full = &full["changes"][0];
        let change = &diff["changes"][0];
        assert_eq!(change["path"], full["path"]);
        assert!(change.get("before").is_none() && change.get("after").is_none());
        let expected = super::super::diff::unified(
            full["path"].as_str().unwrap(),
            full["before"].as_str().unwrap(),
            full["after"].as_str().unwrap(),
        );
        assert_eq!(change["diff"], expected);
        assert!(expected.starts_with("--- a/main.wi\n+++ b/main.wi\n@@ -1,1 +1,1 @@\n-"));
        assert!(expected.contains("+fn value() -> i64 { return 2; }"));
        // Diff previews do not change the transaction; it still validates.
        w.validate(&id, &mut crate::diagnostics::HumanEmitter)
            .unwrap();
    }
    #[derive(Default)]
    struct Collect(Vec<(String, String, usize)>);
    impl DiagnosticEmitter for Collect {
        fn emit(&mut self, d: &Diagnostic, sources: &dyn SourceLookup) -> std::io::Result<()> {
            for label in &d.labels {
                let path = sources.get(label.span.file_id).unwrap().path.clone();
                self.0.push((path, d.message.clone(), label.span.line));
            }
            Ok(())
        }
    }
    #[test]
    fn batch_local_rename_rejects_new_destination_capture() {
        let f = Fixture::new();
        let source = "fn main() { let a = 1; let b = 2; println(a + b); }";
        fs::write(f.0.join("main.wi"), source).unwrap();
        let w = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w
            .analyze(&f.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let operations = snapshot
            .semantic
            .symbols
            .iter()
            .filter(|s| s.kind == "binding")
            .map(|s| Operation::Rename {
                function: s.id.clone(),
                name: "fresh".into(),
            })
            .collect::<Vec<_>>();
        assert_eq!(operations.len(), 2);
        let error = w
            .prepare(
                &f.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision.clone(),
                    operations,
                },
                ChangeFormat::Full,
                &mut emitter,
            )
            .unwrap_err();
        assert!(format!("{error:#}").contains("distinct destination names"));
        assert_eq!(fs::read_to_string(f.0.join("main.wi")).unwrap(), source);
    }

    #[test]
    fn validation_diagnostics_use_workspace_relative_paths() {
        let f = Fixture::new();
        fs::create_dir(f.0.join("lib")).unwrap();
        fs::write(
            f.0.join("main.wi"),
            "import lib::helper;\nfn value() -> i64 { return 1; }\nfn main() { println(value() + helper::get()); }",
        )
        .unwrap();
        fs::write(
            f.0.join("lib/helper.wi"),
            "pub fn get() -> i64 {\n  return 2;\n}",
        )
        .unwrap();
        let w = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w
            .analyze(&f.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let id = |name: &str| {
            snapshot
                .functions
                .iter()
                .find(|g| g.name == name)
                .unwrap()
                .id
                .clone()
        };
        let result = w
            .prepare(
                &f.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision.clone(),
                    operations: vec![
                        Operation::ReplaceBody {
                            function: id("value"),
                            body: "{ return false; }".into(),
                        },
                        Operation::ReplaceBody {
                            function: id("get"),
                            body: "{\n  return \"two\";\n}".into(),
                        },
                    ],
                },
                ChangeFormat::Full,
                &mut emitter,
            )
            .unwrap();
        let tx = result["transaction"].as_str().unwrap();
        let mut collect = Collect::default();
        assert!(w.validate(tx, &mut collect).is_err());
        let paths: std::collections::BTreeSet<_> =
            collect.0.iter().map(|(p, _, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "lib/helper.wi"
                    .replace('/', std::path::MAIN_SEPARATOR_STR)
                    .as_str(),
                "main.wi"
            ]
            .into_iter()
            .collect()
        );
        assert!(
            collect
                .0
                .iter()
                .all(|(p, m, line)| !p.contains(".willow-edits")
                    && !m.contains(".willow-edits")
                    && *line > 0)
        );
    }
    #[test]
    fn validation_failure_and_stale_source_preserve_existing_changes() {
        let f = Fixture::new();
        let w = f.workspace();
        let id = f.prepare(&w, "{ return false; }");
        assert!(
            w.validate(&id, &mut crate::diagnostics::HumanEmitter)
                .is_err()
        );
        assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
        let id = f.prepare(&w, "{ return 3; }");
        w.validate(&id, &mut crate::diagnostics::HumanEmitter)
            .unwrap();
        fs::write(f.0.join("main.wi"), "fn main() {} // user edit").unwrap();
        assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            "fn main() {} // user edit"
        );
    }
    #[test]
    fn candidate_tampering_and_added_files_are_rejected() {
        let f = Fixture::new();
        let w = f.workspace();
        let id = f.prepare(&w, "{ return 2; }");
        w.validate(&id, &mut crate::diagnostics::HumanEmitter)
            .unwrap();
        fs::write(w.directory.join(&id).join("extra.wi"), "fn extra() {}").unwrap();
        assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
        fs::remove_file(w.directory.join(&id).join("extra.wi")).unwrap();
        fs::write(w.directory.join(&id).join("main.wi"), "fn main() {}").unwrap();
        assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
    }
    #[test]
    fn restart_rolls_back_all_files_and_preserves_unrelated_files() {
        let f = Fixture::new();
        let w = f.workspace();
        let id = f.prepare(&w, "{ return 2; }");
        // Exercise the same journal with two files, independently of semantic
        // edit selection. All rollback bytes predate the first workspace write.
        let mut plan = w.load(&id).unwrap();
        fs::write(f.0.join("second.wi"), "before").unwrap();
        fs::write(w.directory.join(&id).join("second.wi"), "after").unwrap();
        plan.inputs.insert("second.wi".into(), hash("before"));
        plan.changes.push(Change {
            path: "second.wi".into(),
            before: "before".into(),
            after: "after".into(),
        });
        plan.state = "validated".into();
        plan.candidate = candidate(&plan).unwrap();
        w.store(&id, &plan).unwrap();
        fs::write(f.0.join("unrelated.txt"), "keep me").unwrap();
        drop(w);
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "ai::edit::tests::crash_worker", "--nocapture"])
            .env("WILLOW_EDIT_TEST_CRASH_ROOT", &f.0)
            .env("WILLOW_EDIT_TEST_CRASH_ID", &id)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(79));
        let w = f.workspace();
        assert!(w.preview(&id, ChangeFormat::Full).is_err());
        w.recover(&id).unwrap();
        for c in plan.changes {
            assert_eq!(fs::read_to_string(f.0.join(c.path)).unwrap(), c.before);
        }
        assert_eq!(
            fs::read_to_string(f.0.join("unrelated.txt")).unwrap(),
            "keep me"
        );
        assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
    }
    #[test]
    fn recovery_conflicts_never_overwrite_user_bytes() {
        let f = Fixture::new();
        let w = f.workspace();
        let id = f.prepare(&w, "{ return 2; }");
        w.validate(&id, &mut crate::diagnostics::HumanEmitter)
            .unwrap();
        assert!(
            w.apply_inner(&id, Some(0), &mut crate::diagnostics::HumanEmitter)
                .is_err()
        );
        fs::write(f.0.join("main.wi"), "user change").unwrap();
        assert!(w.recover(&id).is_err());
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            "user change"
        );
    }
    #[test]
    fn rename_resolved_calls_and_reject_ambiguous_tokens() {
        let f = Fixture::new();
        let w = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w
            .analyze(&f.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "value")
            .unwrap()
            .id
            .clone();
        let result = w
            .prepare(
                &f.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision,
                    operations: vec![Operation::Rename {
                        function,
                        name: "answer".into(),
                    }],
                },
                ChangeFormat::Full,
                &mut emitter,
            )
            .unwrap();
        let id = result["transaction"].as_str().unwrap();
        w.validate(id, &mut emitter).unwrap();
        w.apply(id, &mut emitter).unwrap();
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            "fn answer() -> i64 { return 1; } fn main() { println(answer()); }"
        );
    }
    #[test]
    fn dispatch_family_rename_preserves_overrides() {
        let f = Fixture::new();
        fs::write(f.0.join("main.wi"), "open class Base { pub open fn run(self) -> i64 { return 1; } } class Child extends Base { pub override fn run(self) -> i64 { return 2; } } fn call(x: Base) -> i64 { return x.run(); } fn main() {}").unwrap();
        let w = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w
            .analyze(&f.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "Base::run")
            .unwrap()
            .id
            .clone();
        let result = w
            .prepare(
                &f.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision,
                    operations: vec![Operation::Rename {
                        function,
                        name: "execute".into(),
                    }],
                },
                ChangeFormat::Full,
                &mut emitter,
            )
            .unwrap();
        let id = result["transaction"].as_str().unwrap();
        w.validate(id, &mut emitter).unwrap();
        w.apply(id, &mut emitter).unwrap();
        let source = fs::read_to_string(f.0.join("main.wi")).unwrap();
        assert_eq!(source.matches("execute").count(), 3);
        assert!(!source.contains("run("));
    }
    #[test]
    fn rename_across_modules_and_replace_body_in_one_candidate() {
        let f = Fixture::new();
        fs::write(
            f.0.join("main.wi"),
            "import helper; fn main() { println(helper::value()); }",
        )
        .unwrap();
        fs::write(f.0.join("helper.wi"), "pub fn value() -> i64 { return 1; }").unwrap();
        let w = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w
            .analyze(&f.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "value")
            .unwrap()
            .id
            .clone();
        let result = w
            .prepare(
                &f.0.join("main.wi"),
                false,
                Request {
                    revision: snapshot.revision,
                    operations: vec![
                        Operation::Rename {
                            function: function.clone(),
                            name: "answer".into(),
                        },
                        Operation::ReplaceBody {
                            function,
                            body: "{ return 42; }".into(),
                        },
                    ],
                },
                ChangeFormat::Full,
                &mut emitter,
            )
            .unwrap();
        assert_eq!(result["changes"].as_array().unwrap().len(), 2);
        let id = result["transaction"].as_str().unwrap();
        w.validate(id, &mut emitter).unwrap();
        w.apply(id, &mut emitter).unwrap();
        assert!(
            fs::read_to_string(f.0.join("main.wi"))
                .unwrap()
                .contains("helper::answer()")
        );
        assert!(
            fs::read_to_string(f.0.join("helper.wi"))
                .unwrap()
                .contains("answer() -> i64 { return 42; }")
        );
    }

    /// Rename helper::value to `answer` in a two-or-more file fixture and apply.
    fn rename_helper_value(files: &[(&str, &str)]) -> Result<Fixture> {
        let f = Fixture::new();
        for (path, source) in files {
            fs::write(f.0.join(path), source).unwrap();
        }
        let w = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w.analyze(&f.0.join("main.wi"), false, &mut emitter)?;
        let function = snapshot
            .functions
            .iter()
            .find(|g| g.name == "value" && g.module.ends_with("helper.wi"))
            .unwrap()
            .id
            .clone();
        let result = w.prepare(
            &f.0.join("main.wi"),
            false,
            Request {
                revision: snapshot.revision,
                operations: vec![Operation::Rename {
                    function,
                    name: "answer".into(),
                }],
            },
            ChangeFormat::Full,
            &mut emitter,
        )?;
        let id = result["transaction"].as_str().unwrap().to_owned();
        w.validate(&id, &mut emitter)?;
        w.apply(&id, &mut emitter)?;
        drop(w);
        Ok(f)
    }
    const HELPER: (&str, &str) = (
        "helper.wi",
        "pub fn value() -> i64 { return 1; } pub fn other() -> i64 { return 2; }",
    );

    #[test]
    fn rename_rewrites_grouped_item_import_and_bare_calls() {
        let f = rename_helper_value(&[
            HELPER,
            (
                "main.wi",
                "import helper::{other, value};\nfn main() { println(value() + other()); }",
            ),
        ])
        .unwrap();
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            "import helper::{other, answer};\nfn main() { println(answer() + other()); }"
        );
        assert!(
            fs::read_to_string(f.0.join("helper.wi"))
                .unwrap()
                .starts_with("pub fn answer()")
        );
    }

    #[test]
    fn rename_rewrites_single_item_import() {
        let f = rename_helper_value(&[
            HELPER,
            (
                "main.wi",
                "import helper::value;\nfn main() { println(value()); }",
            ),
        ])
        .unwrap();
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            "import helper::answer;\nfn main() { println(answer()); }"
        );
    }

    #[test]
    fn rename_keeps_import_alias_and_alias_uses() {
        let f = rename_helper_value(&[
            HELPER,
            (
                "main.wi",
                "import helper::{value as v};\nimport helper::value as w;\nfn main() { println(v() + w()); }",
            ),
        ])
        .unwrap();
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            "import helper::{answer as v};\nimport helper::answer as w;\nfn main() { println(v() + w()); }"
        );
    }

    #[test]
    fn rename_follows_item_imports_in_every_consumer_module() {
        let f = rename_helper_value(&[
            HELPER,
            (
                "mid.wi",
                "import helper::{value};\npub fn twice() -> i64 { return value() * 2; }",
            ),
            (
                "main.wi",
                "import helper;\nimport mid;\nfn main() { println(helper::value() + mid::twice()); }",
            ),
        ])
        .unwrap();
        assert_eq!(
            fs::read_to_string(f.0.join("mid.wi")).unwrap(),
            "import helper::{answer};\npub fn twice() -> i64 { return answer() * 2; }"
        );
        assert!(
            fs::read_to_string(f.0.join("main.wi"))
                .unwrap()
                .contains("helper::answer()")
        );
    }

    #[test]
    fn rename_rejects_same_spelled_import_of_other_module_with_location() {
        let main = "import helper;\nimport other::{value};\nfn main() { println(helper::value() + value()); }";
        let error = rename_helper_value(&[
            HELPER,
            ("other.wi", "pub fn value() -> i64 { return 3; }"),
            ("main.wi", main),
        ])
        .err()
        .unwrap();
        let rejection = error.downcast_ref::<Rejection>().unwrap();
        let l = &rejection.location;
        assert_eq!(l.path, "main.wi");
        assert_eq!((l.line, l.column), (2, 16));
        assert_eq!(&main[l.start..l.end], "value");
        assert!(error.to_string().ends_with("at main.wi:2:16"));
    }

    #[test]
    fn rename_destination_collision_reports_declaration() {
        for count in [1, 16, 64] {
            let helper = format!(
                "// answer is only a comment\npub fn answer() -> i64 {{ return 2; }}\npub fn value() -> i64 {{ {} return 1; }}",
                "answer();".repeat(count)
            );
            let error = rename_helper_value(&[
                ("helper.wi", &helper),
                (
                    "main.wi",
                    "import helper;\nfn main() { let answer = helper::value(); println(answer); }",
                ),
            ])
            .err()
            .unwrap();
            let rejection = error.downcast_ref::<Rejection>().unwrap();
            assert_eq!(
                rejection.message,
                "rename destination conflicts with an existing declaration"
            );
            let location = &rejection.location;
            let start = helper.find("fn answer").unwrap() + 3;
            assert_eq!(
                serde_json::to_value(location).unwrap(),
                serde_json::json!({
                    "path": "helper.wi", "start": start, "end": start + 6,
                    "line": 2, "column": 8
                })
            );
            assert_eq!(&helper[location.start..location.end], "answer");
            assert!(error.to_string().ends_with("at helper.wi:2:8"));
        }
    }

    #[test]
    fn rename_destination_collision_prefers_declaration_over_earlier_import() {
        let lib = "pub fn answer() -> i64 { return 2; }";
        let error = rename_helper_value(&[
            HELPER,
            ("lib.wi", lib),
            (
                "app.wi",
                "import lib::{answer};\npub fn run() -> i64 { return answer(); }",
            ),
            (
                "main.wi",
                "import app;\nimport helper;\nfn main() { println(app::run() + helper::value()); }",
            ),
        ])
        .err()
        .unwrap();
        let rejection = error.downcast_ref::<Rejection>().unwrap();
        assert_eq!(
            rejection.message,
            "rename destination conflicts with an existing declaration"
        );
        let l = &rejection.location;
        assert_eq!((l.path.as_str(), l.line, l.column), ("lib.wi", 1, 8));
        assert_eq!(&lib[l.start..l.end], "answer");
    }

    #[test]
    fn rename_supports_function_values() {
        for main in [
            "import helper::{value}; fn main() { let f = value; println(f()); }",
            "import helper::value as v; fn main() { let f = v; println(f()); }",
            "import helper::{value as v}; fn main() { let f = v; println(f()); }",
        ] {
            let f = rename_helper_value(&[HELPER, ("main.wi", main)]).unwrap();
            assert_eq!(
                fs::read_to_string(f.0.join("main.wi")).unwrap(),
                main.replace("value", "answer")
            );
        }
    }

    #[test]
    fn rename_preserves_explicit_alias_equal_to_original_name() {
        for helper in [HELPER.1, "pub const value: i64 = 7;"] {
            for import in [
                "import helper::value as value;",
                "import helper::{value as value};",
            ] {
                let main = format!("{import} fn main() {{ let f = value; }}");
                let f = rename_helper_value(&[("helper.wi", helper), ("main.wi", &main)]).unwrap();
                assert_eq!(
                    fs::read_to_string(f.0.join("main.wi")).unwrap(),
                    main.replacen("value", "answer", 1)
                );
            }
        }
    }

    #[test]
    fn rename_repeated_module_and_item_names() {
        for (import, usage, expected_import, expected_usage) in [
            (
                "import value::value;",
                "value",
                "import value::answer;",
                "answer",
            ),
            (
                "import value::{value};",
                "value",
                "import value::{answer};",
                "answer",
            ),
            (
                "import value::value as v;",
                "v",
                "import value::answer as v;",
                "v",
            ),
            (
                "import value::{value as v};",
                "v",
                "import value::{answer as v};",
                "v",
            ),
            (
                "import value::value as value;",
                "value",
                "import value::answer as value;",
                "value",
            ),
            (
                "import value::{value as value};",
                "value",
                "import value::{answer as value};",
                "value",
            ),
        ] {
            let f = Fixture::new();
            fs::write(f.0.join("value.wi"), "pub const value: i64 = 7;").unwrap();
            fs::write(
                f.0.join("main.wi"),
                format!("{import} fn main() {{ println({usage}); }}"),
            )
            .unwrap();
            let w = f.workspace();
            let mut emitter = crate::diagnostics::HumanEmitter;
            let snapshot = w
                .analyze(&f.0.join("main.wi"), false, &mut emitter)
                .unwrap();
            let id = snapshot
                .functions
                .iter()
                .find(|f| f.name == "value")
                .unwrap()
                .id
                .clone();
            let result = w
                .prepare(
                    &f.0.join("main.wi"),
                    false,
                    Request {
                        revision: snapshot.revision,
                        operations: vec![Operation::Rename {
                            function: id,
                            name: "answer".into(),
                        }],
                    },
                    ChangeFormat::Full,
                    &mut emitter,
                )
                .unwrap();
            let transaction = result["transaction"].as_str().unwrap();
            w.validate(transaction, &mut emitter).unwrap();
            w.apply(transaction, &mut emitter).unwrap();
            assert_eq!(
                fs::read_to_string(f.0.join("value.wi")).unwrap(),
                "pub const answer: i64 = 7;"
            );
            assert_eq!(
                fs::read_to_string(f.0.join("main.wi")).unwrap(),
                format!("{expected_import} fn main() {{ println({expected_usage}); }}")
            );
        }
    }

    #[test]
    fn rename_constant_across_consumers() {
        let f = rename_helper_value(&[
            ("helper.wi", "pub const value: i64 = 7;"),
            (
                "mid.wi",
                "import helper::{value}; pub fn read() -> i64 { return value; }",
            ),
            (
                "main.wi",
                "import helper; import mid; fn main() { println(helper::value + mid::read()); }",
            ),
        ])
        .unwrap();
        assert_eq!(
            fs::read_to_string(f.0.join("helper.wi")).unwrap(),
            "pub const answer: i64 = 7;"
        );
        assert_eq!(
            fs::read_to_string(f.0.join("mid.wi")).unwrap(),
            "import helper::{answer}; pub fn read() -> i64 { return answer; }"
        );
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            "import helper; import mid; fn main() { println(helper::answer + mid::read()); }"
        );
    }

    #[test]
    fn rename_constant_spellings_and_local_shadowing() {
        let helper = "pub const value: i64 = 7; pub fn read() -> i64 { return value; }";
        for main in [
            "import helper; fn main() { println(helper::value); }",
            "import helper as h; fn main() { println(h::value); }",
            "import helper::value; fn main() { println(value); }",
            "import helper::{value}; fn main() { println(value); }",
            "import helper::value as v; fn main() { println(v); }",
            "import helper::{value as v}; fn main() { println(v); }",
        ] {
            let f = rename_helper_value(&[("helper.wi", helper), ("main.wi", main)]).unwrap();
            assert_eq!(
                fs::read_to_string(f.0.join("main.wi")).unwrap(),
                main.replace("value", "answer")
            );
            assert_eq!(
                fs::read_to_string(f.0.join("helper.wi")).unwrap(),
                helper.replace("value", "answer")
            );
        }
        let main = "import helper::value; fn main() { println(value); if true { let value = 9; println(value); } println(value); } fn shadow(value: i64) -> i64 { return value; }";
        let f = rename_helper_value(&[("helper.wi", helper), ("main.wi", main)]).unwrap();
        assert_eq!(
            fs::read_to_string(f.0.join("main.wi")).unwrap(),
            "import helper::answer; fn main() { println(answer); if true { let value = 9; println(value); } println(answer); } fn shadow(value: i64) -> i64 { return value; }"
        );
    }

    #[test]
    fn edit_scaling_indexes_tokens_once_and_visits_only_target_references() {
        for n in [16, 64, 256] {
            let f = Fixture::new();
            fs::write(
                f.0.join("main.wi"),
                format!(
                    "fn value() -> i64 {{ return 1; }} fn main() {{ {} }}",
                    "println(value());".repeat(n)
                ),
            )
            .unwrap();
            let w = f.workspace();
            let mut emitter = crate::diagnostics::HumanEmitter;
            let snapshot = w
                .analyze(&f.0.join("main.wi"), false, &mut emitter)
                .unwrap();
            let function = snapshot
                .functions
                .iter()
                .find(|f| f.name == "value")
                .unwrap()
                .id
                .clone();
            let result = w
                .prepare(
                    &f.0.join("main.wi"),
                    false,
                    Request {
                        revision: snapshot.revision,
                        operations: vec![Operation::Rename {
                            function,
                            name: "answer".into(),
                        }],
                    },
                    ChangeFormat::Full,
                    &mut emitter,
                )
                .unwrap();
            assert_eq!(result["work"]["patches"], n + 1);
            assert_eq!(result["work"]["references_visited"], n);
            assert_eq!(result["work"]["tokens_indexed"], 7 * n + 18);
            println!("edit calls={n} work={}", result["work"]);
        }
    }
    #[test]
    fn rename_value_scaling_counts_exact_facts_and_probes() {
        for n in [16, 64, 256] {
            let f = Fixture::new();
            let source = format!(
                "const value: i64 = 7; fn main() {{ {} }}",
                "println(value); if true { let value = 9; println(value); }".repeat(n)
            );
            fs::write(f.0.join("main.wi"), &source).unwrap();
            let w = f.workspace();
            let mut emitter = crate::diagnostics::HumanEmitter;
            let snapshot = w
                .analyze(&f.0.join("main.wi"), false, &mut emitter)
                .unwrap();
            let id = snapshot
                .functions
                .iter()
                .find(|f| f.name == "value")
                .unwrap()
                .id
                .clone();
            let sources = BTreeMap::from([("main.wi".into(), source.clone())]);
            let (changes, work) = structured_changes(
                &snapshot,
                &f.0,
                &sources,
                vec![Operation::Rename {
                    function: id,
                    name: "answer".into(),
                }],
            )
            .unwrap();
            assert_eq!(
                work.value_facts_indexed,
                snapshot.semantic.symbols.len() + snapshot.semantic.references.len()
            );
            assert_eq!(work.value_probes, 3 * n + 1);
            assert_eq!(work.patches, n + 1);
            assert_eq!(changes.len(), 1);
            println!(
                "value occurrences={n} work={}",
                serde_json::to_string(&work).unwrap()
            );
        }
    }

    #[test]
    fn rejects_shadowing_and_declarations_outside_replacement() {
        let f = Fixture::new();
        let w = f.workspace();
        let mut emitter = crate::diagnostics::HumanEmitter;
        let snapshot = w
            .analyze(&f.0.join("main.wi"), false, &mut emitter)
            .unwrap();
        let function = snapshot
            .functions
            .iter()
            .find(|f| f.name == "value")
            .unwrap()
            .id
            .clone();
        for operation in [
            Operation::Rename {
                function: function.clone(),
                name: "main".into(),
            },
            Operation::ReplaceBody {
                function,
                body: "{} fn injected() {}".into(),
            },
        ] {
            assert!(
                w.prepare(
                    &f.0.join("main.wi"),
                    false,
                    Request {
                        revision: snapshot.revision.clone(),
                        operations: vec![operation]
                    },
                    ChangeFormat::Full,
                    &mut emitter
                )
                .is_err()
            );
        }
    }

    #[test]
    fn crash_worker() {
        let Ok(root) = std::env::var("WILLOW_EDIT_TEST_CRASH_ROOT") else {
            return;
        };
        let id = std::env::var("WILLOW_EDIT_TEST_CRASH_ID").unwrap();
        let w = Workspace::open(Path::new(&root)).unwrap();
        assert!(
            w.apply_inner(&id, Some(1), &mut crate::diagnostics::HumanEmitter)
                .is_err()
        );
        // Deliberately skip Rust destructors: the OS releases the file lock.
        std::process::exit(79);
    }
    #[test]
    fn workspace_lock_excludes_other_edit_observers() {
        let f = Fixture::new();
        let first = f.workspace();
        assert!(Workspace::open(&f.0).is_err());
        drop(first);
        assert!(Workspace::open(&f.0).is_ok());
    }
    #[test]
    fn changed_compiler_configuration_rejects_validation_and_apply() {
        let f = Fixture::new();
        let w = f.workspace();
        let id = f.prepare(&w, "{ return 2; }");
        let mut plan = w.load(&id).unwrap();
        plan.configuration = "changed".into();
        plan.candidate = candidate(&plan).unwrap();
        w.store(&id, &plan).unwrap();
        assert!(
            w.validate(&id, &mut crate::diagnostics::HumanEmitter)
                .is_err()
        );
        plan.state = "validated".into();
        w.store(&id, &plan).unwrap();
        assert!(w.apply(&id, &mut crate::diagnostics::HumanEmitter).is_err());
        assert!(
            fs::read_to_string(f.0.join("main.wi"))
                .unwrap()
                .contains("return 1;")
        );
    }
}
