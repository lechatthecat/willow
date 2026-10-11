//! Shared workspace lease, durable replacement, and undo journal.
//! The lease excludes cooperating processes; raw filesystem readers can see
//! intermediate files. An interrupted journal always rolls back before use.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Arc, Weak},
};

thread_local! {
    // Nested compiler/package/bridge calls on the same thread reuse the lease.
    // Other threads and processes still acquire the OS lock independently.
    static LEASES: RefCell<HashMap<PathBuf, Weak<File>>> = RefCell::new(HashMap::new());
}

#[derive(Debug)]
pub(crate) struct WorkspaceFileTransaction {
    root: PathBuf,
    directory: PathBuf,
    _lease: Arc<File>,
}

impl Drop for WorkspaceFileTransaction {
    fn drop(&mut self) {
        if Arc::strong_count(&self._lease) == 1 {
            LEASES.with(|leases| {
                leases.borrow_mut().remove(&self.root);
            });
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FileChange {
    path: String,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}
impl FileChange {
    pub(crate) fn new(path: &str, before: Option<Vec<u8>>, after: Option<Vec<u8>>) -> Self {
        Self {
            path: path.into(),
            before,
            after,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    revision: String,
    changes: Vec<FileChange>,
}

impl WorkspaceFileTransaction {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        Self::acquire(root, false)
    }
    pub(crate) fn try_open(root: &Path) -> Result<Self> {
        Self::acquire(root, true)
    }
    /// Read-only commands do not create metadata. Existing metadata participates
    /// in recovery and exclusion, including --locked/--offline commands.
    pub(crate) fn existing(root: &Path) -> Result<Option<Self>> {
        if root.join(".willow-edits").exists() {
            Ok(Some(Self::open(root)?))
        } else {
            Ok(None)
        }
    }
    fn acquire(root: &Path, nonblocking: bool) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let directory = root.join(".willow-edits");
        if let Some(lease) =
            LEASES.with(|leases| leases.borrow().get(&root).and_then(Weak::upgrade))
        {
            ensure!(!nonblocking, "workspace edit operation already running");
            return Ok(Self {
                root,
                directory,
                _lease: lease,
            });
        }
        // Never wait for a second workspace while owning another: opposing
        // path-dependency traversals would otherwise form an OS-lock cycle.
        let nested_other = LEASES.with(|leases| !leases.borrow().is_empty());
        fs::create_dir_all(&directory)?;
        ensure!(
            !fs::symlink_metadata(&directory)?.file_type().is_symlink(),
            "workspace transaction directory is a symlink"
        );
        // Persist the metadata directory's entry even if another opener created
        // it, or a prior initialization stopped before syncing its parent.
        // Existence alone is not evidence that the directory entry is durable.
        sync_directory(&root)?;
        let lease = LEASES.with(|leases| -> Result<Arc<File>> {
            let mut leases = leases.borrow_mut();
            let path = directory.join("lock");
            reject_symlink(&path)?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(path)?;
            if nonblocking || nested_other {
                file.try_lock()
                    .context("workspace edit operation already running")?;
            } else {
                file.lock().context("cannot lock workspace transaction")?;
            }
            let lease = Arc::new(file);
            leases.insert(root.clone(), Arc::downgrade(&lease));
            Ok(lease)
        })?;
        let transaction = Self {
            root,
            directory,
            _lease: lease,
        };
        transaction.recover()?;
        Ok(transaction)
    }
    pub(crate) fn ensure_no_ai_edit(&self) -> Result<()> {
        ensure!(
            !self.directory.join("active").exists(),
            "interrupted transaction requires edit recover"
        );
        Ok(())
    }
    fn journal_path(&self) -> PathBuf {
        self.directory.join("workspace-journal.json")
    }
    fn file(&self, relative: &str) -> Result<PathBuf> {
        let path = Path::new(relative);
        ensure!(
            !path.as_os_str().is_empty()
                && path.components().all(|c| matches!(c, Component::Normal(_))),
            "invalid transaction path"
        );
        let mut absolute = self.root.clone();
        for component in path.components() {
            absolute.push(component);
            reject_symlink(&absolute)?;
        }
        ensure!(
            !path.starts_with(".willow-edits"),
            "transaction cannot edit its metadata"
        );
        Ok(absolute)
    }
    fn validate(&self, journal: &Journal) -> Result<()> {
        ensure!(
            journal.version == 1,
            "unsupported workspace journal version"
        );
        let mut paths = HashSet::with_capacity(journal.changes.len());
        for change in &journal.changes {
            ensure!(
                paths.insert(self.file(&change.path)?),
                "duplicate transaction path"
            );
        }
        Ok(())
    }
    fn prepare(&self, changes: Vec<FileChange>) -> Result<Journal> {
        self.ensure_no_ai_edit()?;
        ensure!(!self.journal_path().exists(), "workspace recovery required");
        let revision = crate::ai::hash(serde_json::to_vec(&changes)?);
        let journal = Journal {
            version: 1,
            revision,
            changes,
        };
        self.validate(&journal)?;
        for change in &journal.changes {
            ensure!(
                read_optional(&self.file(&change.path)?)? == change.before,
                "project changed while resolving; retry the command: {}",
                change.path
            );
        }
        Ok(journal)
    }
    fn persist_intent(&self, journal: &Journal) -> Result<()> {
        // Intent and complete undo bytes reach storage before the first write.
        durable_write(&self.journal_path(), &serde_json::to_vec(journal)?)
    }
    #[cfg(test)]
    fn begin(&self, changes: Vec<FileChange>) -> Result<Journal> {
        let journal = self.prepare(changes)?;
        self.persist_intent(&journal)?;
        Ok(journal)
    }
    pub(crate) fn publish(&self, changes: Vec<FileChange>) -> Result<()> {
        let journal = self.prepare(changes)?;
        if journal
            .changes
            .iter()
            .all(|change| change.before == change.after)
        {
            return Ok(());
        }
        self.persist_intent(&journal)?;
        let result = (|| {
            #[cfg(test)]
            tests::crash_after(0);
            for (index, change) in journal.changes.iter().enumerate() {
                let path = self.file(&change.path)?;
                ensure!(
                    read_optional(&path)? == change.before,
                    "concurrent workspace edit: {}",
                    change.path
                );
                if change.before != change.after {
                    replace_optional(&path, change.after.as_deref())?;
                }
                #[cfg(test)]
                tests::crash_after(index + 1);
                #[cfg(not(test))]
                let _ = index;
            }
            self.clear()
        })();
        if let Err(error) = result {
            self.recover()
                .context("workspace publication failed and rollback failed")?;
            return Err(error);
        }
        Ok(())
    }
    fn clear(&self) -> Result<()> {
        fs::remove_file(self.journal_path())?;
        sync_directory(&self.directory)
    }
    fn recover(&self) -> Result<()> {
        let path = self.journal_path();
        reject_symlink(&path)?;
        let Some(bytes) = read_optional(&path)? else {
            return Ok(());
        };
        let journal: Journal =
            serde_json::from_slice(&bytes).context("invalid workspace undo journal")?;
        self.validate(&journal)?;
        ensure!(
            journal.revision == crate::ai::hash(serde_json::to_vec(&journal.changes)?),
            "workspace journal revision mismatch"
        );
        // Preflight every file before rollback: preserve third-party edits.
        for change in &journal.changes {
            let current = read_optional(&self.file(&change.path)?)?;
            ensure!(
                current == change.before || current == change.after,
                "workspace recovery conflict: {} (preserved)",
                change.path
            );
        }
        for (index, change) in journal.changes.iter().enumerate() {
            let path = self.file(&change.path)?;
            let current = read_optional(&path)?;
            ensure!(
                current == change.before || current == change.after,
                "concurrent workspace recovery conflict: {} (preserved)",
                change.path
            );
            if current != change.before {
                replace_optional(&path, change.before.as_deref())?;
            }
            #[cfg(test)]
            tests::recovery_crash_after(index + 1);
            #[cfg(not(test))]
            let _ = index;
        }
        self.clear()
    }
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => ensure!(
            !metadata.file_type().is_symlink(),
            "transaction path is a symlink: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(error) => return Err(error.into()),
    }
    Ok(())
}
pub(crate) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>> {
    #[cfg(test)]
    tests::READS.with(|n| n.set(n.get() + 1));
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}
fn replace_optional(path: &Path, bytes: Option<&[u8]>) -> Result<()> {
    if let Some(bytes) = bytes {
        durable_write(path, bytes)
    } else {
        fs::remove_file(path)?;
        sync_directory(path.parent().context("missing parent")?)
    }
}
pub(crate) fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    #[cfg(all(test, unix))]
    tests::record_durability(|| tests::DurabilityEvent::DirectorySynced(path.to_owned()));
    Ok(())
}
pub(crate) fn durable_write(path: &Path, bytes: &[u8]) -> Result<()> {
    durable_write_observed(path, bytes, || {})
}
pub(crate) fn durable_write_observed(
    path: &Path,
    bytes: &[u8],
    published: impl FnOnce(),
) -> Result<()> {
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
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(all(test, unix))]
        tests::record_durability(|| tests::DurabilityEvent::Published(path.to_owned()));
        published();
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests;

#[cfg(all(test, unix))]
pub(crate) use tests::{DurabilityEvent, DurabilityTrace};
